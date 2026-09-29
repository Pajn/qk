//! Prepare finite tasks, then execute shell commands in managed process groups.

mod capture;
mod interpolate;
mod process;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;
use serde_json::Value;

pub use capture::{Capture, read_capture, replay};
pub use process::{Outcome, execute, execute_captured};

#[derive(Clone, Debug)]
pub struct PreparedTask {
    pub id: String,
    pub commands: Vec<String>,
    pub parallel: bool,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

/// Read dotenv files into a child environment without changing process globals.
/// Existing process variables win over .env.local, which wins over .env.
pub fn environment(workspace: &Path) -> Result<BTreeMap<OsString, OsString>> {
    let mut values = BTreeMap::new();
    for name in [".env", ".env.local"] {
        let path = workspace.join(name);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        for (index, entry) in dotenvy::from_read_iter(file).enumerate() {
            // Dotenv parser errors can contain secret values; report only location.
            let (name, value) = entry.map_err(|_| {
                anyhow::anyhow!("invalid dotenv entry {} in {}", index + 1, path.display())
            })?;
            values.insert(name.into(), value.into());
        }
    }
    values.extend(std::env::vars_os());
    Ok(values)
}

pub fn prepare(
    workspace: &Workspace,
    task: &Task,
    base_env: &BTreeMap<OsString, OsString>,
) -> Result<PreparedTask> {
    let definition = &task.definition;
    let options = &definition.options;
    let executor = definition
        .executor
        .as_deref()
        .context("target has no executor or command")?;
    let interpolation = interpolate::Interpolation::new(workspace, task);
    let mut env = base_env.clone();
    for values in [definition.extra.get("env"), options.get("env")]
        .into_iter()
        .flatten()
    {
        for (name, value) in values.as_object().context("env must be an object")? {
            if name.is_empty() || name.contains(['=', '\0']) {
                bail!("invalid environment variable name");
            }
            let value = value
                .as_str()
                .context("environment values must be strings")?;
            env.insert(name.into(), interpolation.text(value)?.into());
        }
    }
    for key in options.keys() {
        let allowed = match executor {
            "nx:run-commands" => matches!(
                key.as_str(),
                "command" | "commands" | "cwd" | "env" | "parallel" | "forwardAllArgs"
            ),
            "nx:run-script" => matches!(key.as_str(), "script" | "env" | "cwd"),
            "nx:noop" => true,
            _ => bail!("unsupported executor {executor:?}"),
        };
        if !allowed {
            bail!("unsupported {executor} option {key:?}");
        }
    }
    let default_cwd = if executor == "nx:run-script" {
        &workspace.projects[&task.project].root
    } else {
        "."
    };
    let cwd = options
        .get("cwd")
        .map(|value| value.as_str().context("cwd must be a string"))
        .transpose()?
        .unwrap_or(default_cwd);
    let cwd = workspace.root.join(interpolation.text(cwd)?);
    if !cwd.is_dir() {
        bail!("working directory does not exist: {}", cwd.display());
    }
    // Expose local binaries even for plain run-commands targets.
    let mut paths = Vec::new();
    for directory in cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(&workspace.root))
    {
        paths.push(directory.join("node_modules/.bin"));
    }
    let path_key = if cfg!(windows) {
        env.keys()
            .find(|key| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .cloned()
            .unwrap_or_else(|| "PATH".into())
    } else {
        "PATH".into()
    };
    if let Some(path) = env.get(&path_key) {
        paths.extend(std::env::split_paths(path));
    }
    env.insert(
        path_key,
        std::env::join_paths(paths).context("cannot construct task PATH")?,
    );
    let parallel = boolean(options.get("parallel"), true, "parallel")?;
    let mut commands = Vec::new();
    match executor {
        "nx:noop" => {}
        "nx:run-script" => {
            let script = options
                .get("script")
                .and_then(Value::as_str)
                .context("run-script requires options.script")?;
            let package = workspace
                .packages
                .get(&task.project)
                .context("run-script requires a package.json")?;
            if !package.scripts.contains_key(script) {
                bail!("package.json has no script {script:?}");
            }
            let manager = package_manager(workspace)?;
            let args = interpolation.arguments()?;
            let separator = if manager == "npm" && !args.is_empty() {
                " --"
            } else {
                ""
            };
            commands.push(format!(
                "{manager} run {}{separator}{}",
                interpolate::quote(script)?,
                if args.is_empty() {
                    String::new()
                } else {
                    format!(" {args}")
                }
            ));
        }
        "nx:run-commands" => {
            if options.contains_key("command") && options.contains_key("commands") {
                bail!("specify command or commands, not both");
            }
            let forward = boolean(options.get("forwardAllArgs"), true, "forwardAllArgs")?;
            if let Some(command) = options.get("command") {
                commands.push(interpolation.command(
                    command.as_str().context("command must be a string")?,
                    forward,
                )?);
            } else if let Some(values) = options.get("commands") {
                for value in values.as_array().context("commands must be an array")? {
                    let (command, forward) = if let Some(command) = value.as_str() {
                        (command, forward)
                    } else {
                        let value = value
                            .as_object()
                            .context("commands entries must be strings or objects")?;
                        for key in value.keys() {
                            if !matches!(key.as_str(), "command" | "forwardAllArgs") {
                                bail!("unsupported command entry field {key:?}");
                            }
                        }
                        (
                            value
                                .get("command")
                                .and_then(Value::as_str)
                                .context("command entry requires a command string")?,
                            boolean(value.get("forwardAllArgs"), forward, "forwardAllArgs")?,
                        )
                    };
                    commands.push(interpolation.command(command, forward)?);
                }
            } else {
                bail!("run-commands requires command or commands");
            }
            if commands.is_empty() || commands.iter().any(|command| command.trim().is_empty()) {
                bail!("run-commands requires nonempty commands");
            }
        }
        _ => bail!("unsupported executor {executor:?}"),
    }
    Ok(PreparedTask {
        id: task.id.clone(),
        commands,
        parallel,
        cwd,
        env,
    })
}

fn boolean(value: Option<&Value>, default: bool, name: &str) -> Result<bool> {
    value
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("{name} must be boolean"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn package_manager(workspace: &Workspace) -> Result<&'static str> {
    let declared = workspace
        .package_manager
        .as_deref()
        .map(|value| value.split('@').next().unwrap_or(value));
    match declared {
        Some("pnpm") => Ok("pnpm"),
        Some("npm") => Ok("npm"),
        Some(other) => bail!(
            "package manager {other:?} is not supported yet; run-script supports npm and pnpm"
        ),
        None if workspace.root.join("pnpm-workspace.yaml").is_file()
            || workspace.root.join("pnpm-lock.yaml").is_file() =>
        {
            Ok("pnpm")
        }
        None => Ok("npm"),
    }
}
