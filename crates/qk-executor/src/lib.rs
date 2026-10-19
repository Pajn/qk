//! Prepare finite tasks, then execute shell commands in managed process groups.

mod capture;
pub mod dotenv;
mod env_flags;
mod interpolate;
mod process;
#[cfg(unix)]
mod pty;
pub mod readiness;
pub mod report;
pub mod tokens;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;
use serde_json::{Map, Value};

pub use capture::{
    Capture, Decoration, Display, MAX_LOG_BYTES, OutputStyle, RecordedLog, Shown, collapsed,
    printable, read_capture, replay,
};
pub use env_flags::{force_colour, is_ci, is_ci_with};
pub use process::{Outcome, execute, execute_captured, shell};

#[derive(Debug)]
struct NodeRuntime {
    root: PathBuf,
    env: BTreeMap<OsString, OsString>,
    paths: Vec<PathBuf>,
    directory: std::sync::OnceLock<Option<PathBuf>>,
}

/// The runner's Node selection, resolved only when a command starts.
#[derive(Clone, Debug)]
pub struct NodePath {
    runtime: std::sync::Arc<NodeRuntime>,
    local_entries: usize,
}

impl NodePath {
    fn new(root: &Path, env: &BTreeMap<OsString, OsString>, local_entries: usize) -> Self {
        type Key = (PathBuf, BTreeMap<OsString, OsString>);
        static RUNTIMES: std::sync::OnceLock<
            std::sync::Mutex<BTreeMap<Key, std::sync::Arc<NodeRuntime>>>,
        > = std::sync::OnceLock::new();
        let mut runtimes = RUNTIMES.get_or_init(Default::default).lock().unwrap();
        let runtime = runtimes
            .entry((root.to_owned(), env.clone()))
            .or_insert_with(|| {
                let paths = env
                    .get(&path_key(env))
                    .map(|path| std::env::split_paths(path).collect())
                    .unwrap_or_default();
                std::sync::Arc::new(NodeRuntime {
                    root: root.to_owned(),
                    env: env.clone(),
                    paths,
                    directory: Default::default(),
                })
            })
            .clone();
        Self {
            runtime,
            local_entries,
        }
    }

    fn path(&self, env: &BTreeMap<OsString, OsString>) -> Result<Option<(OsString, OsString)>> {
        // Select before task overrides, like process.execPath in Nx. Cache hits
        // never call this, and concurrent commands share the same proxy probe.
        let Some(directory) = self.runtime.directory.get_or_init(|| {
            node_directory(&self.runtime.root, &self.runtime.paths, &self.runtime.env)
        }) else {
            return Ok(None);
        };
        let key = path_key(env);
        let mut paths: Vec<_> = env
            .get(&key)
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default();
        paths.insert(self.local_entries.min(paths.len()), directory.clone());
        Ok(Some((
            key,
            std::env::join_paths(paths).context("cannot construct task PATH")?,
        )))
    }
}

fn path_key(env: &BTreeMap<OsString, OsString>) -> OsString {
    if cfg!(windows) {
        env.keys()
            .find(|key| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .cloned()
            .unwrap_or_else(|| "PATH".into())
    } else {
        "PATH".into()
    }
}

fn environment_key(env: &BTreeMap<OsString, OsString>, name: OsString) -> OsString {
    if cfg!(windows) && name.to_string_lossy().eq_ignore_ascii_case("PATH") {
        path_key(env)
    } else {
        name
    }
}

fn node_directory(
    cwd: &Path,
    paths: &[PathBuf],
    env: &BTreeMap<OsString, OsString>,
) -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    for directory in paths {
        let candidate = cwd.join(directory).join(name);
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        // The first executable PATH match is authoritative, even if discovery fails.
        let actual = candidate.canonicalize().ok()?;
        if actual.file_name()? == name {
            use std::io::Read;
            let mut header = [0; 2];
            let script = std::fs::File::open(&actual)
                .and_then(|mut file| file.read_exact(&mut header))
                .is_ok()
                && header == *b"#!";
            if !script {
                return actual.parent().map(Path::to_owned);
            }
        }
        return probe_node(&candidate, cwd, env);
    }
    None
}

fn probe_node(candidate: &Path, cwd: &Path, env: &BTreeMap<OsString, OsString>) -> Option<PathBuf> {
    use std::io::{Read, Seek};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut output = tempfile::tempfile().ok()?;
    let mut child = Command::new(candidate)
        .args(["-p", "process.execPath"])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdout(Stdio::from(output.try_clone().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut bytes = Vec::new();
                output.rewind().ok()?;
                output.take(4096).read_to_end(&mut bytes).ok()?;
                let path = PathBuf::from(std::str::from_utf8(&bytes).ok()?.trim());
                return (path.is_absolute() && path.is_file())
                    .then(|| path.parent().unwrap().to_owned());
            }
            Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Reports a `qk:` warning to the installed sink; by default it is written to
/// stderr in one write, so task processes sharing stderr cannot split it.
#[macro_export]
macro_rules! status {
    ($($arg:tt)*) => {{
        $crate::report::warning(&::std::format!($($arg)*));
    }};
}

/// How a sandboxed task's commands are held to what it declares.
#[derive(Clone, Debug)]
pub enum Confinement {
    /// macOS: a Seatbelt profile, through `sandbox-exec`.
    Seatbelt(PathBuf),
    /// Linux: a Landlock rule set's descriptor, which each command restricts
    /// itself to before it starts. It stays open while the run lasts.
    Landlock(i32),
}

/// An external syscall recorder wrapping each command, with separate trace files.
#[derive(Clone, Debug)]
pub struct Recording {
    pub program: PathBuf,
    pub directory: PathBuf,
}

impl Recording {
    /// The same invocation is used by preflight and actual task commands.
    pub fn command(&self, index: usize, text: &str) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        // No read/write buffers or exec argument/environment arrays.
        command
            .args([
                "-ff",
                "-ttt",
                "-yy",
                "-qq",
                "-s",
                "4096",
                "-e",
                "trace=%file,%process,getdents64,fchdir,close,dup,dup2,dup3,getcwd",
                "-e",
                "raw=execve,execveat",
                "-o",
            ])
            .arg(self.directory.join(format!("command-{index}")))
            .args(["--", "/bin/sh", "-c", text]);
        command
    }
}

#[derive(Clone, Debug)]
pub struct PreparedTask {
    pub id: String,
    pub commands: Vec<String>,
    pub parallel: bool,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    /// Dotenv, explicit settings and local binary lookup without task metadata.
    pub runtime_env: BTreeMap<OsString, OsString>,
    pub node_path: Option<NodePath>,
    /// Variables set for the process after `env` that are not part of what
    /// the task is: nothing reads them to key it, such as a thread count.
    pub execution: BTreeMap<OsString, OsString>,
    /// Nx's `readyWhen`: text whose appearance in the output, every piece of
    /// it, makes the task ready while its commands keep running.
    pub ready_when: Vec<String>,
    /// Set once all of `ready_when` has appeared.
    pub ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A continuous target's own `readyWhen`, which tasks depending on it
    /// with `waitFor: "ready"` wait for, or why it is invalid.
    pub ready_check: Option<std::result::Result<readiness::ReadyCheck, String>>,
    /// What `ready_check` concluded.
    pub verdict: std::sync::Arc<readiness::Verdict>,
    /// The process ids of its commands while they run, from which the runner
    /// measures the memory the task uses.
    pub processes: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<u32>>>,
    /// Each command's prefix and colours, by position; empty without any.
    pub decorations: Vec<Decoration>,
    /// Whether the task reads qk's stdin, and has the terminal while it runs.
    /// The runner sets it for the one task a run is for, when that task has a
    /// single command whose output goes straight to the terminal.
    pub interactive: bool,
    /// Whether captured commands may run under a pseudo-terminal, which they
    /// do when qk's output is a terminal: Nx's `usePty`. Commands whose
    /// output is hashed set it to false, so keys never depend on a terminal.
    pub pseudo_terminal: bool,
    /// Nx's `tty`: whether qk's terminal is in raw input mode while the
    /// interactive task reads it through a pseudo-terminal.
    pub tty: bool,
    /// What the commands run under, for `--sandbox`.
    pub sandbox: Option<Confinement>,
    pub recording: Option<Recording>,
    /// How the task's output is shown; the runner sets it per output style.
    pub display: Display,
}

/// The dotenv files Nx loads for a task, most specific first: in the
/// project root, then the workspace root, for the target and configuration
/// (`.env.<target>.<configuration>`, `.env.<configuration>`, `.env.<target>`),
/// then `.env.local`, `.local.env` and `.env`, each also in its `.local` and
/// `.<name>.env` spellings. An atomized target also reads the files of the
/// target it was split from, after its own of the same kind.
fn dotenv_files(
    project_root: &str,
    target: &str,
    configuration: Option<&str>,
    non_atomized: Option<&str>,
) -> Vec<String> {
    let mut identifiers = Vec::new();
    if let Some(configuration) = configuration {
        identifiers.push(format!("{target}.{configuration}"));
        if let Some(non_atomized) = non_atomized {
            identifiers.push(format!("{non_atomized}.{configuration}"));
        }
        identifiers.push(configuration.to_owned());
    }
    identifiers.push(target.to_owned());
    if let Some(non_atomized) = non_atomized {
        identifiers.push(non_atomized.to_owned());
    }
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
            // Colons in target names cannot name regular dotenv files on Windows.
            #[cfg(windows)]
            if identifier.contains(':') {
                continue;
            }
            for file in variants(identifier, root) {
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
    }
    files
}

/// The target whose dotenv files a task reads, and the target it was
/// atomized from, as Nx's getOwnerTargetForTask finds them: through the
/// project's `metadata.targetGroups`, a target of the task's group that
/// names a `metadata.nonAtomizedTarget`. Unlike Nx, a task that names one
/// itself is its own owner, so each atomized target reads its own files
/// rather than the first of its group's.
fn owner_target<'a>(
    project: &'a qk_config::Project,
    target: &'a str,
) -> (&'a str, Option<&'a str>) {
    let non_atomized = |definition: &'a qk_config::Target| {
        definition
            .extra
            .get("metadata")
            .and_then(|metadata| metadata.get("nonAtomizedTarget"))
            .and_then(Value::as_str)
    };
    if let Some((name, definition)) = project.targets.get_key_value(target)
        && let Some(non_atomized) = non_atomized(definition)
    {
        return (name, Some(non_atomized));
    }
    let groups = project
        .extra
        .get("metadata")
        .and_then(|metadata| metadata.get("targetGroups"))
        .and_then(Value::as_object);
    for group in groups.into_iter().flat_map(|groups| groups.values()) {
        let members: Vec<&str> = group
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !members.contains(&target) {
            continue;
        }
        for member in members {
            if let Some((name, definition)) = project.targets.get_key_value(member)
                && let Some(non_atomized) = non_atomized(definition)
            {
                return (name, Some(non_atomized));
            }
        }
    }
    (target, None)
}

/// The variable naming the tasks a task runs inside, one invocation a line.
pub const INVOCATIONS: &str = "QK_TASK_INVOCATIONS";

/// A task as an invocation: its id and arguments, which a task running inside
/// it repeats only by recursing, as Nx's task invocation tracker tells it.
pub fn invocation(task: &Task) -> String {
    let mut text = task.id.clone();
    for arg in &task.args {
        text.push('\u{1f}');
        text.push_str(arg);
    }
    text.replace('\n', " ")
}

/// The tasks this process runs inside, from [`INVOCATIONS`].
pub fn enclosing_invocations() -> Vec<String> {
    std::env::var(INVOCATIONS)
        .map(|chain| chain.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The root dotenv files Nx loads into its own process at startup, in the
/// order they take precedence.
pub const ROOT_DOTENV_FILES: [&str; 3] = [".local.env", ".env.local", ".env"];

/// A task's environment as Nx builds it: the process environment, without
/// what the root dotenv files set in it at startup, then the task's dotenv
/// files, loaded and expanded together, then the variables Nx sets for
/// every task. A target's `env` applies on top.
fn task_environment(
    workspace: &Workspace,
    task: &Task,
    base_env: &BTreeMap<OsString, OsString>,
) -> Result<(BTreeMap<OsString, OsString>, BTreeMap<OsString, OsString>)> {
    let mut env = base_env.clone();
    let project = &workspace.projects[&task.project];
    if loads_dotenv(base_env) {
        // As Nx's unloadDotEnvFiles: a variable whose value is a root file's
        // goes, so the task's own files decide it. Nx unloads them even when
        // it did not load them; qk keeps what the shell set then.
        for file in [".env", ".local.env", ".env.local"] {
            for (name, value) in dotenv::read_alone(&workspace.root.join(file))? {
                if env.get(&name) == Some(&value) {
                    env.remove(&name);
                }
            }
        }
        let (target, non_atomized) = owner_target(project, &task.target);
        let files: Vec<PathBuf> = dotenv_files(
            &project.root,
            target,
            task.configuration.as_deref(),
            non_atomized,
        )
        .into_iter()
        .map(|file| workspace.root.join(file))
        .collect();
        dotenv::load(&files, &mut env)?;
    }
    let runtime_env = env.clone();
    // Nx's own variables from an enclosing run, which describe that run.
    for name in [
        "NX_SET_CLI",
        "NX_ORIGINAL_FORCE_COLOR",
        "NX_TERMINAL_OUTPUT_PATH",
        "NX_STREAM_OUTPUT",
        "NX_PREFIX_OUTPUT",
    ] {
        env.remove(std::ffi::OsStr::new(name));
    }
    let mut set = |name: &str, value: OsString| {
        env.insert(name.into(), value);
    };
    set("NX_WORKSPACE_ROOT", workspace.root.clone().into_os_string());
    set("NX_TASK_TARGET_PROJECT", task.project.clone().into());
    set("NX_TASK_TARGET_TARGET", task.target.clone().into());
    set("LERNA_PACKAGE_NAME", task.project.clone().into());
    set("NX_TUI", "false".into());
    // The qk and Nx processes this task runs inside, outermost first.
    let ancestors = match base_env.get(std::ffi::OsStr::new("NX_INVOCATION_ANCESTOR_PIDS")) {
        Some(inherited) if !inherited.is_empty() => {
            format!("{},{}", inherited.to_string_lossy(), std::process::id())
        }
        _ => std::process::id().to_string(),
    };
    set("NX_INVOCATION_ANCESTOR_PIDS", ancestors.into());
    // The tasks this one runs inside, outermost first, so a run it starts can
    // tell when it would run one of them again.
    let mut chain = base_env
        .get(std::ffi::OsStr::new(INVOCATIONS))
        .map(|chain| chain.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !chain.is_empty() {
        chain.push('\n');
    }
    chain.push_str(&invocation(task));
    set(INVOCATIONS, chain.into());
    if task.target == "test" {
        set("NX_TERMINAL_CAPTURE_STDERR", "true".into());
    }
    match &task.configuration {
        Some(configuration) => set("NX_TASK_TARGET_CONFIGURATION", configuration.clone().into()),
        None => {
            env.remove(std::ffi::OsStr::new("NX_TASK_TARGET_CONFIGURATION"));
        }
    }
    Ok((env, runtime_env))
}

/// Whether dotenv files load: as in Nx, `NX_LOAD_DOT_ENV_FILES=false` turns
/// them all off.
fn loads_dotenv(env: &BTreeMap<OsString, OsString>) -> bool {
    env.get(std::ffi::OsStr::new("NX_LOAD_DOT_ENV_FILES"))
        .is_none_or(|value| value != "false")
}

/// The process environment with the root dotenv files loaded into it, as
/// Nx loads them into its own process at startup, unless
/// `NX_LOAD_DOT_ENV_FILES=false`. Variables already set win.
pub fn environment(workspace: &Path) -> Result<BTreeMap<OsString, OsString>> {
    let mut values: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    if loads_dotenv(&values) {
        let files: Vec<PathBuf> = ROOT_DOTENV_FILES
            .iter()
            .map(|file| workspace.join(file))
            .collect();
        dotenv::load(&files, &mut values)?;
    }
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
    let project = &workspace.projects[&task.project];
    let run_commands = executor == "nx:run-commands";
    if !matches!(executor, "nx:run-commands" | "nx:run-script" | "nx:noop") {
        bail!("unsupported executor {executor:?}");
    }
    // As in Nx, run-script reads only `script`, and noop nothing.
    let options = if run_commands {
        run_commands_overrides(&definition.options, &task.args, project)?
    } else {
        definition.options.clone()
    };
    let options = &options;
    let empty = Map::new();
    let mut interpolation = if run_commands {
        interpolate::Interpolation::new(&task.args, options)?
    } else {
        interpolate::Interpolation::new(&[], &empty)?
    };
    let (mut env, mut runtime_env) = task_environment(workspace, task, base_env)?;
    // As in Nx 24, `envFile` sets what the task's environment, dotenv files
    // included, leaves unset or empty, beneath `env`; and the task's
    // environment says whether it loads.
    if let Some(file) = options.get("envFile").filter(|_| run_commands) {
        let file = file.as_str().context("envFile must be a string")?;
        let path = workspace.root.join(file);
        if loads_dotenv(&runtime_env) {
            if !path.is_file() {
                bail!("envFile {file} does not exist");
            }
            dotenv::load(std::slice::from_ref(&path), &mut runtime_env)?;
            dotenv::load(std::slice::from_ref(&path), &mut env)?;
        }
    }
    // A target-level `env` is a qk extension, for every executor.
    let option_env = options.get("env").filter(|_| run_commands);
    for values in [definition.extra.get("env"), option_env]
        .into_iter()
        .flatten()
    {
        for (name, value) in values.as_object().context("env must be an object")? {
            if name.is_empty() || name.contains(['=', '\0']) {
                bail!("invalid environment variable name");
            }
            let value: OsString = value
                .as_str()
                .context("environment values must be strings")?
                .into();
            let name = environment_key(&env, name.into());
            runtime_env.insert(name.clone(), value.clone());
            env.insert(name, value);
        }
    }
    interpolation.set_env(&env);
    let mut execution = BTreeMap::new();
    execution.insert(
        "FORCE_COLOR".into(),
        env.get(std::ffi::OsStr::new("FORCE_COLOR"))
            .cloned()
            .unwrap_or_else(|| "true".into()),
    );
    if run_commands && boolean(options.get("color"), false, "color")? {
        execution.insert("FORCE_COLOR".into(), "true".into());
    }
    // run-commands runs where `cwd` says, the workspace root by default, and
    // run-script in the project root.
    let cwd = match options.get("cwd").filter(|_| run_commands) {
        Some(cwd) => cwd.as_str().context("cwd must be a string")?,
        None if run_commands => ".",
        None => &project.root,
    };
    let cwd = workspace.root.join(cwd);
    let mut resolved = PathBuf::new();
    for part in cwd.components() {
        match part {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            _ => resolved.push(part.as_os_str()),
        }
    }
    // Expose local binaries even for plain run-commands targets.
    let mut paths = Vec::new();
    for directory in resolved.ancestors() {
        paths.push(directory.join("node_modules/.bin"));
    }
    let node_path = NodePath::new(&workspace.root, base_env, paths.len());
    let path_key = path_key(&env);
    if let Some(path) = env.get(&path_key) {
        paths.extend(std::env::split_paths(path));
    }
    let path = std::env::join_paths(paths).context("cannot construct task PATH")?;
    env.insert(path_key.clone(), path.clone());
    runtime_env.insert(path_key, path);
    let ready_when: Vec<String> = match options.get("readyWhen").filter(|_| run_commands) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![text.clone()],
        Some(value) => serde_json::from_value(value.clone())
            .context("readyWhen must be a string or an array of strings")?,
    };
    // As in Nx, a single `command` runs in parallel exactly when it has
    // `readyWhen`, whatever `parallel` says.
    let parallel = if run_commands && options.get("command").is_some_and(merge_truthy) {
        !ready_when.is_empty()
    } else if run_commands {
        boolean(options.get("parallel"), true, "parallel")?
    } else {
        true
    };
    let (mut pseudo_terminal, tty) = if run_commands {
        (
            boolean(options.get("usePty"), true, "usePty")?
                && base_env
                    .get(std::ffi::OsStr::new("NX_NATIVE_COMMAND_RUNNER"))
                    .is_none_or(|value| value != "false"),
            boolean(options.get("tty"), true, "tty")?,
        )
    } else {
        (true, true)
    };
    let mut decorations = Vec::new();
    if !ready_when.is_empty() && !parallel {
        bail!("readyWhen can only be used when parallel is true");
    }
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
            // Nx joins the arguments as they are; qk quotes each, so that
            // one with a space or a shell character stays one argument.
            let args = interpolate::quoted_arguments(&task.args)?;
            let script = interpolate::quote(script)?;
            // Nx's `run` for each: npm and bun need `--` before arguments,
            // and yarn runs a script by name.
            let mut command = match manager {
                "yarn" => format!("yarn {script}"),
                manager => format!("{manager} run {script}"),
            };
            if !args.is_empty() {
                if matches!(manager, "npm" | "bun") {
                    command.push_str(" --");
                }
                command.push(' ');
                command.push_str(&args);
            }
            commands.push(command);
        }
        "nx:run-commands" => {
            let forward = boolean(options.get("forwardAllArgs"), true, "forwardAllArgs")?;
            // As in Nx, `command` wins over `commands`.
            if let Some(command) = options
                .get("command")
                .filter(|command| merge_truthy(command))
            {
                // Nx also takes an array, joined with spaces.
                let command = match command {
                    Value::String(command) => command.clone(),
                    Value::Array(words) => words
                        .iter()
                        .map(|word| word.as_str().context("command words must be strings"))
                        .collect::<Result<Vec<_>>>()?
                        .join(" "),
                    _ => bail!("command must be a string or an array of strings"),
                };
                commands.push(interpolation.command(&command, forward)?);
            } else if let Some(values) = options.get("commands") {
                for value in values.as_array().context("commands must be an array")? {
                    let (command, forward) = if let Some(command) = value.as_str() {
                        (command, forward)
                    } else {
                        let value = value
                            .as_object()
                            .context("commands entries must be strings or objects")?;
                        for key in value.keys() {
                            if !matches!(
                                key.as_str(),
                                "command"
                                    | "forwardAllArgs"
                                    | "prefix"
                                    | "prefixColor"
                                    | "color"
                                    | "bgColor"
                                    | "description"
                            ) {
                                bail!("unsupported command entry field {key:?}");
                            }
                        }
                        let field =
                            |name: &str, background: Option<bool>| -> Result<Option<String>> {
                                let Some(value) = value.get(name) else {
                                    return Ok(None);
                                };
                                let text = value.as_str().with_context(|| {
                                    format!("command entry {name} must be a string")
                                })?;
                                if let Some(background) = background
                                    && !Decoration::valid(text, background)
                                {
                                    bail!("command entry {name} {text:?} is not a colour");
                                }
                                Ok(Some(text.to_owned()))
                            };
                        let decoration = Decoration {
                            prefix: field("prefix", None)?,
                            prefix_color: field("prefixColor", Some(false))?,
                            color: field("color", Some(false))?,
                            bg_color: field("bgColor", Some(true))?,
                        };
                        if !decoration.is_empty() && !parallel {
                            bail!(
                                "prefix, prefixColor, color and bgColor can only be set when parallel is true"
                            );
                        }
                        decorations.resize(commands.len(), Decoration::default());
                        decorations.push(decoration);
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
            // An empty `commands` succeeds without running anything, as in Nx.
            if commands.iter().any(|command| command.trim().is_empty()) {
                bail!("run-commands requires nonempty commands");
            }
        }
        _ => bail!("unsupported executor {executor:?}"),
    }
    // As in Nx, commands run side by side share qk's output through pipes.
    pseudo_terminal &= commands.len() == 1 || !parallel;
    let ready_check = ready_check(workspace, task, options, &env)?;
    Ok(PreparedTask {
        id: task.id.clone(),
        commands,
        parallel,
        cwd,
        env,
        runtime_env,
        node_path: Some(node_path),
        execution,
        ready_when,
        ready: Default::default(),
        ready_check,
        verdict: Default::default(),
        processes: Default::default(),
        decorations,
        interactive: false,
        pseudo_terminal,
        tty,
        sandbox: None,
        recording: None,
        display: Display::default(),
    })
}

/// A continuous target's `readyWhen`, resolved against the options and the
/// environment the task runs with, as Nx 24 resolves it. `null` turns off one
/// a target default sets. A target that is not continuous has none.
fn ready_check(
    workspace: &Workspace,
    task: &Task,
    options: &Map<String, Value>,
    env: &BTreeMap<OsString, OsString>,
) -> Result<Option<std::result::Result<readiness::ReadyCheck, String>>> {
    let Some(ready_when) = task
        .definition
        .extra
        .get("readyWhen")
        .filter(|value| !value.is_null() && task.definition.continuous == Some(true))
    else {
        return Ok(None);
    };
    let mut options = options.clone();
    let project = &workspace.projects[&task.project];
    for (key, value) in interpolate::yargs(&task.args, false, false) {
        if key == "_" {
            continue;
        }
        // As in Nx, an argument's tokens resolve for the task's project.
        let value = match value {
            Value::String(text) => Value::String(qk_config::interpolate_override(&text, project)?),
            value => value,
        };
        if key.contains('-') {
            options.insert(interpolate::camel_case(&key), value.clone());
        }
        options.insert(key, value);
    }
    let env: BTreeMap<String, String> = env
        .iter()
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value.to_str()?.to_owned())))
        .collect();
    let scope = readiness::Scope {
        workspace_root: workspace
            .root
            .to_str()
            .context("workspace path must be UTF-8")?,
        project_root: &workspace.projects[&task.project].root,
        project_name: &task.project,
        options: &options,
        env: &env,
    };
    Ok(readiness::resolve(&task.id, ready_when, &scope)
        .map_err(|error| format!("{error:#}"))
        .transpose())
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
/// option applied. As in Nx, `--cwd=dir`, `--no-parallel` or
/// `--env.NAME=value` set the option rather than reaching the command.
fn run_commands_overrides(
    options: &serde_json::Map<String, Value>,
    args: &[String],
    project: &qk_config::Project,
) -> Result<serde_json::Map<String, Value>> {
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
    let mut ready = Vec::new();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--") else {
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
            // As in Nx, an argument's tokens resolve for the task's project.
            Value::String(qk_config::interpolate_override(&value, project)?)
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
    Ok(options)
}

/// JavaScript truthiness, which decides what Nx reads as set.
fn merge_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
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
    workspace.package_manager_name()
}

#[cfg(all(test, windows))]
mod dotenv_tests {
    #[test]
    fn colon_targets_keep_configuration_and_generic_dotenv_files() {
        let files = super::dotenv_files("app", "test:unit:watch", Some("ci"), None);
        assert!(files.iter().all(|file| !file.contains(':')));
        assert!(files.contains(&"app/.env.ci".to_owned()));
        assert!(files.contains(&"app/.env".to_owned()));
        assert!(files.contains(&".env.ci".to_owned()));
        assert!(files.contains(&".env".to_owned()));
    }
}
