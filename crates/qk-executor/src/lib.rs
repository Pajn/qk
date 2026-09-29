//! Prepare finite tasks, then execute shell commands in managed process groups.

mod capture;
mod interpolate;
mod process;
pub mod report;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;
use serde_json::Value;

pub use capture::{Capture, Display, OutputStyle, Shown, read_capture, replay};
pub use process::{Outcome, execute, execute_captured};

/// Reports a `qk:` warning to the installed sink; by default it is written to
/// stderr in one write, so task processes sharing stderr cannot split it.
#[macro_export]
macro_rules! status {
    ($($arg:tt)*) => {{
        $crate::report::warning(&::std::format!($($arg)*));
    }};
}

#[derive(Clone, Debug)]
pub struct PreparedTask {
    pub id: String,
    pub commands: Vec<String>,
    pub parallel: bool,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    /// Variables set for the process after `env` that are not part of what
    /// the task is: nothing reads them to key it, such as a thread count.
    pub execution: BTreeMap<OsString, OsString>,
    /// How the task's output is shown; the runner sets it per output style.
    pub display: Display,
}

/// The dotenv files Nx loads for a task, most specific first: in the
/// project root, then the workspace root, for the target and configuration
/// (`.env.<target>.<configuration>`, `.env.<configuration>`, `.env.<target>`),
/// then `.env.local`, `.local.env` and `.env`, each also in its `.local` and
/// `.<name>.env` spellings.
fn dotenv_files(project_root: &str, target: &str, configuration: Option<&str>) -> Vec<String> {
    let mut identifiers = Vec::new();
    if let Some(configuration) = configuration {
        identifiers.push(format!("{target}.{configuration}"));
        identifiers.push(configuration.to_owned());
    }
    identifiers.push(target.to_owned());
    identifiers.push(String::new());
    let variants = |identifier: &str, root: &str| -> Vec<String> {
        let prefix = if root.is_empty() || root == "." {
            String::new()
        } else {
            format!("{root}/")
        };
        if identifier.is_empty() {
            [".env.local", ".local.env", ".env"]
                .map(|name| format!("{prefix}{name}"))
                .to_vec()
        } else {
            [
                format!(".env.{identifier}.local"),
                format!(".env.{identifier}"),
                format!(".{identifier}.local.env"),
                format!(".{identifier}.env"),
            ]
            .map(|name| format!("{prefix}{name}"))
            .to_vec()
        }
    };
    let mut files = Vec::new();
    for root in [project_root, ""] {
        for identifier in &identifiers {
            for file in variants(identifier, root) {
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
    }
    files
}

/// A task's environment as Nx builds it: the process environment, then each
/// of the task's dotenv files without overriding what is already set, then
/// the variables Nx sets for every task. A target's `env` applies on top.
fn task_environment(
    workspace: &Workspace,
    task: &Task,
    base_env: &BTreeMap<OsString, OsString>,
) -> Result<BTreeMap<OsString, OsString>> {
    let mut env = base_env.clone();
    let root = &workspace.projects[&task.project].root;
    for file in dotenv_files(root, &task.target, task.configuration.as_deref()) {
        for (name, value) in read_dotenv(&workspace.root.join(file))? {
            env.entry(name).or_insert(value);
        }
    }
    let force_color = env
        .get(std::ffi::OsStr::new("FORCE_COLOR"))
        .cloned()
        .unwrap_or_else(|| "true".into());
    let mut set = |name: &str, value: OsString| {
        env.insert(name.into(), value);
    };
    set("FORCE_COLOR", force_color);
    set("NX_WORKSPACE_ROOT", workspace.root.clone().into_os_string());
    set("NX_TASK_TARGET_PROJECT", task.project.clone().into());
    set("NX_TASK_TARGET_TARGET", task.target.clone().into());
    set("LERNA_PACKAGE_NAME", task.project.clone().into());
    set("NX_TUI", "false".into());
    match &task.configuration {
        Some(configuration) => set("NX_TASK_TARGET_CONFIGURATION", configuration.clone().into()),
        None => {
            env.remove(std::ffi::OsStr::new("NX_TASK_TARGET_CONFIGURATION"));
        }
    }
    Ok(env)
}

/// A dotenv file's entries, or none when it does not exist.
fn read_dotenv(path: &Path) -> Result<Vec<(OsString, OsString)>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    dotenvy::from_read_iter(file)
        .enumerate()
        .map(|(index, entry)| {
            // Dotenv parser errors can contain secret values; report only location.
            let (name, value) = entry.map_err(|_| {
                anyhow::anyhow!("invalid dotenv entry {} in {}", index + 1, path.display())
            })?;
            Ok((name.into(), value.into()))
        })
        .collect()
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
    let executor = definition
        .executor
        .as_deref()
        .context("target has no executor or command")?;
    let (options, args) = if executor == "nx:run-commands" {
        run_commands_overrides(&definition.options, &task.args)
    } else {
        (definition.options.clone(), task.args.clone())
    };
    let options = &options;
    // Options Nx's run-commands knows; any other scalar option is forwarded to
    // the command as `--name=value`, and object values are ignored, as in Nx.
    const RUN_COMMANDS: &[&str] = &[
        "command",
        "commands",
        "color",
        "no-color",
        "parallel",
        "no-parallel",
        "readyWhen",
        "cwd",
        "args",
        "envFile",
        "__unparsed__",
        "env",
        "usePty",
        "streamOutput",
        "verbose",
        "forwardAllArgs",
        "tty",
    ];
    let mut forwarded = BTreeMap::new();
    if executor == "nx:run-commands" {
        for (key, value) in options {
            if RUN_COMMANDS.contains(&key.as_str()) {
                continue;
            }
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => continue,
            };
            forwarded.insert(key.clone(), value);
        }
    }
    let extra = match options.get("args") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(words)) => Some(
            words
                .iter()
                .map(|word| word.as_str().context("args entries must be strings"))
                .collect::<Result<Vec<_>>>()?
                .join(" "),
        ),
        Some(_) => bail!("args must be a string or an array of strings"),
    };
    let interpolation =
        interpolate::Interpolation::new(workspace, task, args, &forwarded, extra.as_deref())?;
    let mut env = task_environment(workspace, task, base_env)?;
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
            "nx:run-commands" => {
                matches!(
                    key.as_str(),
                    "command" | "commands" | "cwd" | "env" | "parallel" | "forwardAllArgs" | "args"
                ) || !RUN_COMMANDS.contains(&key.as_str())
            }
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
        execution: BTreeMap::new(),
        display: Display::default(),
    })
}

/// The run-commands options Nx reads, which a task's arguments may set too.
const OPTION_FLAGS: &[&str] = &[
    "command",
    "commands",
    "color",
    "parallel",
    "readyWhen",
    "cwd",
    "args",
    "envFile",
    "__unparsed__",
    "env",
    "usePty",
    "streamOutput",
    "verbose",
    "forwardAllArgs",
    "tty",
];

/// The target's options with the task's arguments that name a run-commands
/// option applied, and the arguments left for the command. As in Nx,
/// `--cwd=dir`, `--no-parallel` or `--env.NAME=value` set the option instead
/// of reaching the command.
fn run_commands_overrides(
    options: &serde_json::Map<String, Value>,
    args: &[String],
) -> (serde_json::Map<String, Value>, Vec<String>) {
    const BOOLEAN: &[&str] = &[
        "color",
        "parallel",
        "usePty",
        "streamOutput",
        "verbose",
        "forwardAllArgs",
        "tty",
    ];
    let mut options = options.clone();
    let mut rest = Vec::new();
    let mut ready = Vec::new();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            rest.push(arg.clone());
            continue;
        };
        let (key, value) = match flag.split_once('=') {
            Some((key, value)) => (key, Some(value.to_owned())),
            None => (flag, None),
        };
        let (key, negated) = match key.strip_prefix("no-") {
            Some(key) if OPTION_FLAGS.contains(&key) => (key, true),
            _ => (key, false),
        };
        let (base, field) = match key.split_once('.') {
            Some((base, field)) => (base, Some(field)),
            None => (key, None),
        };
        if !OPTION_FLAGS.contains(&base) {
            rest.push(arg.clone());
            continue;
        }
        let boolean = BOOLEAN.contains(&base);
        let value = match value {
            Some(value) => value,
            None if negated => "false".into(),
            None if !boolean && args.peek().is_some_and(|next| !next.starts_with('-')) => {
                args.next().unwrap().clone()
            }
            None => "true".into(),
        };
        let value = if boolean {
            Value::Bool(value != "false")
        } else {
            Value::String(value)
        };
        match (base, field) {
            ("env", Some(name)) => {
                let env = options
                    .entry("env")
                    .or_insert_with(|| Value::Object(Default::default()));
                if let Value::Object(env) = env {
                    env.insert(name.to_owned(), value);
                }
            }
            ("readyWhen", None) => ready.push(value),
            // Nx sets these itself, and a list cannot come from a flag.
            ("__unparsed__" | "commands", _) | (_, Some(_)) => {}
            (base, None) => {
                options.insert(base.to_owned(), value);
            }
        }
    }
    if !ready.is_empty() {
        let ready = if ready.len() == 1 {
            ready.pop().unwrap()
        } else {
            Value::Array(ready)
        };
        options.insert("readyWhen".into(), ready);
    }
    (options, rest)
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
