mod explain;
mod history;
mod ui;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use qk_config::{Workspace, find_workspace};
use qk_graph::{GraphReport, ProjectGraph, graph_order, select_projects};
use qk_taskgraph::{Request, TaskGraph};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Parser)]
#[command(
    name = "qk",
    version,
    about = "Quick workspace task tooling",
    long_about = "qk (quick): a standalone Rust task runner.\nExecute tasks with dependency ordering and a local cache shared by Git worktrees.",
    after_help = "Shorthand, as in Nx: `qk <target> [project]` and `qk <project>:<target>` run like `qk run`.\nWithout a project, the project containing the current directory is used, else NX_DEFAULT_PROJECT."
)]
struct Cli {
    /// Workspace root; defaults to discovery from the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect the local repository cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Execute a task and its dependencies. Forward arguments after --.
    Run {
        /// project:target[:configuration], or a target of the current directory's project.
        task: String,
        #[command(flatten)]
        options: RunOptions,
    },
    /// Execute targets on selected projects and their dependencies.
    RunMany {
        // Like Nx, lists take commas, spaces or repeated flags.
        #[arg(short = 't', long, required = true, value_delimiter = ',', num_args = 1..)]
        targets: Vec<String>,
        #[arg(short = 'p', long, value_delimiter = ',', num_args = 1..)]
        projects: Vec<String>,
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        exclude: Vec<String>,
        #[command(flatten)]
        options: RunOptions,
    },
    /// Execute targets on the projects affected by changes, and their dependencies.
    Affected {
        #[arg(short = 't', long, required = true, value_delimiter = ',', num_args = 1..)]
        targets: Vec<String>,
        #[arg(short = 'p', long, value_delimiter = ',', num_args = 1..)]
        projects: Vec<String>,
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        exclude: Vec<String>,
        #[command(flatten)]
        changes: ChangeOptions,
        /// `project` selects the targets of affected projects, as Nx does;
        /// `task` selects the tasks whose inputs changed.
        #[arg(long, value_enum, default_value = "project")]
        granularity: Granularity,
        #[command(flatten)]
        options: RunOptions,
    },
    /// Inspect workspace projects and normalized configuration.
    Show {
        #[command(subcommand)]
        command: ShowCommand,
    },
    /// Export the workspace project graph as JSON.
    Graph {
        /// Output file; use - for stdout. Paths are relative to the current directory.
        #[arg(long, value_name = "PATH", default_value = "-")]
        file: PathBuf,
        /// Add the packages the pnpm lockfile installs as `externalNodes`, with
        /// edges from projects and between packages. `nx graph --file` leaves
        /// them out.
        #[arg(long)]
        external: bool,
    },
}

#[derive(Args)]
struct RunOptions {
    /// Bypass all cache reads and writes; also NX_SKIP_NX_CACHE=true.
    #[arg(long, aliases = ["skip-nx-cache", "skipNxCache", "disable-nx-cache", "disableNxCache"])]
    skip_cache: bool,
    /// Use the local cache only; also NX_SKIP_REMOTE_CACHE=true.
    #[arg(long, aliases = ["skipRemoteCache", "disable-remote-cache", "disableRemoteCache"])]
    skip_remote_cache: bool,
    #[arg(short = 'c', long)]
    configuration: Option<String>,
    /// Use the production configuration, as `-c production`.
    #[arg(long)]
    prod: bool,
    /// Maximum number of tasks executing concurrently: a number, a percentage
    /// of the cores, or `false` for one at a time. Defaults to NX_PARALLEL, then
    /// nx.json `parallel`, then 3; `--parallel` alone means NX_PARALLEL, else 3.
    #[arg(long, env = "NX_PARALLEL", num_args = 0..=1, default_missing_value = "true")]
    parallel: Option<String>,
    #[arg(long, alias = "maxParallel", hide = true)]
    max_parallel: Option<String>,
    /// Stop starting tasks after the first failure; also NX_BAIL=true.
    #[arg(long, alias = "nxBail")]
    nx_bail: bool,
    /// Break task dependency cycles instead of failing; also NX_IGNORE_CYCLES=true.
    #[arg(long, alias = "nxIgnoreCycles")]
    nx_ignore_cycles: bool,
    /// Run only the requested tasks, not the tasks they depend on.
    #[arg(long, alias = "excludeTaskDependencies")]
    exclude_task_dependencies: bool,
    /// Write the task graph as Nx does, to a file or to stdout (`--graph` or
    /// `--graph=stdout`), without running anything.
    #[arg(long, num_args = 0..=1, default_missing_value = "stdout", value_name = "FILE")]
    graph: Option<String>,
    /// Set NX_VERBOSE_LOGGING=true for tasks.
    #[arg(long)]
    verbose: bool,
    /// Nx options qk has no use for, accepted so Nx commands run unchanged.
    #[command(flatten)]
    ignored: IgnoredOptions,
    /// Cores the run's tasks share; defaults to nx.json `qk:cores`, then the
    /// cores available to qk.
    #[arg(long, env = "QK_CORES")]
    cores: Option<std::num::NonZeroUsize>,
    /// Print the task graph as JSON without executing commands or loading dotenv.
    #[arg(long)]
    dry_run: bool,
    /// How task output is shown, as in Nx. Defaults to NX_DEFAULT_OUTPUT_STYLE,
    /// else raw output for `run`, and for several tasks `static` in CI or when
    /// not on a terminal, `stream` otherwise. The interactive styles render as
    /// `static`.
    #[arg(long, value_enum)]
    output_style: Option<OutputStyle>,
    /// Write a JSON report of the run to this file; also NX_RUN_REPORT.
    #[arg(long, env = "NX_RUN_REPORT", value_name = "PATH")]
    report: Option<PathBuf>,
    /// Arguments forwarded to requested tasks; dependencies require params: forward.
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

/// What counts as changed, as in Nx. Without `--head`, the working tree is
/// compared, including uncommitted and untracked files.
#[derive(Args)]
struct ChangeOptions {
    /// Base revision; defaults to NX_BASE, then nx.json `defaultBase`, then `main`.
    #[arg(long)]
    base: Option<String>,
    /// Head revision; defaults to NX_HEAD.
    #[arg(long)]
    head: Option<String>,
    /// Treat exactly these workspace-relative files as changed.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    files: Vec<String>,
    /// Only uncommitted changes.
    #[arg(long)]
    uncommitted: bool,
    /// Only untracked files.
    #[arg(long)]
    untracked: bool,
}

impl ChangeOptions {
    /// Whether any change option was given.
    fn given(&self) -> bool {
        self.base.is_some()
            || self.head.is_some()
            || !self.files.is_empty()
            || self.uncommitted
            || self.untracked
    }

    fn options(&self) -> qk_affected::Options {
        qk_affected::Options {
            base: self.base.clone(),
            head: self.head.clone(),
            files: self.files.clone(),
            uncommitted: self.uncommitted,
            untracked: self.untracked,
        }
    }

    fn affected(&self, workspace: &Workspace) -> Result<std::collections::BTreeSet<String>> {
        let analysis = self.analyse(workspace)?;
        warn_landed(analysis.range.as_ref());
        Ok(analysis.projects.into_keys().collect())
    }

    fn analyse(&self, workspace: &Workspace) -> Result<qk_affected::Analysis> {
        let graph = ProjectGraph::build(workspace)?;
        qk_affected::analyse(
            workspace,
            &graph,
            &qk_affected::Options {
                base: self.base.clone(),
                head: self.head.clone(),
                files: self.files.clone(),
                uncommitted: self.uncommitted,
                untracked: self.untracked,
            },
        )
    }
}

/// Nx's own run options: its task runner, batching, sync generators, Nx
/// Cloud and its terminal UI.
#[derive(Args)]
struct IgnoredOptions {
    #[arg(long, hide = true)]
    runner: Option<String>,
    #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "true")]
    batch: Option<String>,
    #[arg(long, alias = "skipSync", hide = true)]
    skip_sync: bool,
    #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "true")]
    cloud: Option<String>,
    #[arg(long, hide = true)]
    no_cloud: bool,
    #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "true")]
    dte: Option<String>,
    #[arg(long, hide = true)]
    no_dte: bool,
    #[arg(long, aliases = ["useAgents", "use-agents"], hide = true, num_args = 0..=1, default_missing_value = "true")]
    agents: Option<String>,
    #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "true")]
    tui: Option<String>,
    #[arg(long, hide = true)]
    no_tui: bool,
    #[arg(long, alias = "tuiAutoExit", hide = true)]
    tui_auto_exit: Option<String>,
}

impl RunOptions {
    /// The configuration, with `--prod` meaning `production`.
    fn configuration(&self) -> Option<String> {
        self.configuration
            .clone()
            .or_else(|| self.prod.then(|| "production".to_owned()))
    }

    fn skips_cache(&self) -> bool {
        self.skip_cache || env_flag("NX_SKIP_NX_CACHE")
    }

    /// Tasks per run, as Nx reads `--parallel`.
    fn parallel(&self, workspace: &Workspace) -> Result<usize> {
        let count = |value: &str| -> Result<usize> {
            let value = value.trim();
            let parsed = match value.strip_suffix('%') {
                Some(percent) => {
                    let cores =
                        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
                    let percent: f64 = percent
                        .parse()
                        .with_context(|| format!("invalid --parallel {value:?}"))?;
                    (cores as f64 * percent / 100.0).floor() as usize
                }
                None => value
                    .parse()
                    .with_context(|| format!("invalid --parallel {value:?}"))?,
            };
            Ok(parsed.max(1))
        };
        match self.parallel.as_deref() {
            None => Ok(workspace
                .config
                .extra
                .get("parallel")
                .and_then(serde_json::Value::as_u64)
                .filter(|parallel| *parallel > 0)
                .map_or(3, |parallel| parallel as usize)),
            Some("false") => Ok(1),
            Some("true" | "") => {
                let fallback = self
                    .max_parallel
                    .clone()
                    .or_else(|| std::env::var("NX_PARALLEL").ok())
                    .filter(|value| !matches!(value.as_str(), "true" | "false" | ""))
                    .unwrap_or_else(|| "3".into());
                count(&fallback)
            }
            Some(value) => count(value),
        }
    }

    /// Sets what Nx sets in its own environment for these options, so tasks
    /// and the remote cache see it. Called before any thread starts.
    fn export(&self) {
        let set = |name: &str| {
            // Only this thread exists yet.
            unsafe { std::env::set_var(name, "true") }
        };
        if self.verbose {
            set("NX_VERBOSE_LOGGING");
        }
        if self.skips_cache() {
            set("NX_SKIP_NX_CACHE");
        }
        if self.skip_remote_cache {
            set("NX_SKIP_REMOTE_CACHE");
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value == "true")
}

/// Nx's project types, as `show projects --type` takes them.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProjectKind {
    App,
    Lib,
    E2e,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Granularity {
    Project,
    Task,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputStyle {
    Tui,
    Dynamic,
    DynamicLegacy,
    Static,
    Stream,
    StreamWithoutPrefixes,
    /// qk's own: only failed tasks' output, then a summary.
    Quiet,
}

/// How a run is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rendered {
    /// Task output as the executor style has it, with `qk:` status lines.
    Lines(qk_executor::OutputStyle),
    Quiet,
    /// The live panel; needs stderr to be a terminal.
    Dynamic,
}

impl OutputStyle {
    /// The style qk renders; the interactive ones need stderr to be a
    /// terminal, and are static otherwise, as in Nx.
    fn rendered(self) -> Rendered {
        use std::io::IsTerminal;
        match self {
            Self::Stream => Rendered::Lines(qk_executor::OutputStyle::Stream),
            Self::StreamWithoutPrefixes => {
                Rendered::Lines(qk_executor::OutputStyle::StreamWithoutPrefixes)
            }
            Self::Static => Rendered::Lines(qk_executor::OutputStyle::Static),
            Self::Quiet => Rendered::Quiet,
            Self::Tui | Self::Dynamic | Self::DynamicLegacy => {
                if io::stderr().is_terminal() {
                    Rendered::Dynamic
                } else {
                    Rendered::Lines(qk_executor::OutputStyle::Static)
                }
            }
        }
    }
}

/// The output style when none is given: `NX_DEFAULT_OUTPUT_STYLE`, else raw for
/// a single `run`, else Nx's choice for several tasks without its terminal UI.
/// The output style when none is given: `NX_DEFAULT_OUTPUT_STYLE`, else raw
/// for a single `run`; for several tasks `static` in CI, for full logs, the
/// live panel on a terminal, and `quiet` otherwise, for agents and scripts.
fn default_output_style(single: bool) -> Rendered {
    use std::io::IsTerminal;
    if let Ok(name) = std::env::var("NX_DEFAULT_OUTPUT_STYLE")
        && let Ok(style) = OutputStyle::from_str(&name, false)
    {
        return style.rendered();
    }
    if single {
        Rendered::Lines(qk_executor::OutputStyle::StreamWithoutPrefixes)
    } else if std::env::var_os("CI").is_some_and(|value| value != "false") {
        Rendered::Lines(qk_executor::OutputStyle::Static)
    } else if io::stderr().is_terminal() {
        Rendered::Dynamic
    } else {
        Rendered::Quiet
    }
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Print the shared cache directory without creating it.
    Path,
    /// Evict least recently used entries until the cache fits its size limit.
    Prune {
        /// Size limit for this prune, as NX_MAX_CACHE_SIZE takes it; 0 removes
        /// everything. Defaults to the configured limit.
        #[arg(long)]
        max_size: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShowCommand {
    /// List project names in Nx's graph order.
    Projects {
        /// Only projects affected by changes, as `qk affected` selects them.
        #[arg(long)]
        affected: bool,
        #[command(flatten)]
        changes: ChangeOptions,
        /// Select names, globs or tag:<tag>; comma-separated or repeated.
        #[arg(long, short = 'p', value_delimiter = ',', action = clap::ArgAction::Append)]
        projects: Vec<String>,
        /// Exclude matching names, globs or tags.
        #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append)]
        exclude: Vec<String>,
        /// Only projects with one of these targets.
        #[arg(long, short = 't', alias = "withTarget", value_delimiter = ',', action = clap::ArgAction::Append)]
        with_target: Vec<String>,
        /// Only projects of this type.
        #[arg(long = "type", value_enum)]
        kind: Option<ProjectKind>,
        /// Emit a JSON array instead of one name per line.
        #[arg(long, conflicts_with = "sep")]
        json: bool,
        /// Separate names with this instead of a line each.
        #[arg(long)]
        sep: Option<String>,
    },
    /// Explain which projects are affected and why; with a project, the path
    /// from it to the change that affects it.
    Affected {
        project: Option<String>,
        #[command(flatten)]
        changes: ChangeOptions,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// List the tasks the targets plan, or with `--affected` the affected
    /// ones and why.
    Tasks {
        #[arg(short = 't', long, required = true, value_delimiter = ',', num_args = 1..)]
        targets: Vec<String>,
        #[arg(short = 'p', long, value_delimiter = ',', num_args = 1..)]
        projects: Vec<String>,
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        exclude: Vec<String>,
        /// Only tasks whose inputs changed, or that depend on one.
        #[arg(long)]
        affected: bool,
        #[command(flatten)]
        changes: ChangeOptions,
        #[arg(short = 'c', long)]
        configuration: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List recent runs, newest first.
    Runs {
        #[arg(long, default_value = "20")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a recorded run, the latest by default, as `--report` writes it.
    Run {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show a task's recent runs, and why its cache key changed in each.
    Task {
        /// A task id, `project:target[:configuration]`.
        id: String,
        #[arg(long, default_value = "10")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a project's normalized configuration.
    Project {
        name: String,
        /// Emit JSON (currently the default and only format).
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    match run(Cli::parse_from(shorthand(std::env::args_os().collect()))) {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(error) => {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return;
            }
            eprintln!("qk: {error:#}");
            std::process::exit(1);
        }
    }
}

/// Nx's built-in commands that qk does not implement. Nx never reads these as
/// targets, so neither does the shorthand; they fail as unknown subcommands.
const NX_COMMANDS: &[&str] = &[
    "add",
    "connect",
    "daemon",
    "exec",
    "format",
    "format:check",
    "format:write",
    "g",
    "generate",
    "import",
    "init",
    "list",
    "mcp",
    "migrate",
    "release",
    "repair",
    "report",
    "reset",
    "sync",
    "sync:check",
    "view-logs",
    "watch",
];

/// Rewrites Nx's shorthand into `run`: `<target> <project>` and `<project>:<target>`
/// become `run <project>:<target>`, and a bare `<target>` becomes `run <target>`.
/// Anything that starts with one of qk's own subcommands is left alone.
fn shorthand(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    use clap::CommandFactory;
    let command = Cli::command();
    let mut index = 1;
    // Skip global options that precede the command slot.
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        match arg {
            "--workspace" => index += 2,
            _ if arg.starts_with("--workspace=") => index += 1,
            _ => break,
        }
    }
    let Some(first) = args
        .get(index)
        .and_then(|arg| arg.to_str())
        .map(str::to_owned)
    else {
        return args;
    };
    let known = first == "help"
        || NX_COMMANDS.contains(&first.as_str())
        || command.get_subcommands().any(|subcommand| {
            subcommand.get_name() == first
                || subcommand.get_all_aliases().any(|alias| alias == first)
        });
    if first.starts_with('-') || known {
        return args;
    }
    let project = args
        .get(index + 1)
        .and_then(|arg| arg.to_str())
        .filter(|arg| !first.contains(':') && !arg.starts_with('-'))
        .map(str::to_owned);
    match project {
        Some(project) => {
            args.splice(
                index..index + 2,
                ["run".into(), format!("{project}:{first}").into()],
            );
        }
        None => args.insert(index, "run".into()),
    }
    args
}

/// The project whose root most specifically contains the current directory.
/// The project a target without one runs on, as Nx picks it: the project
/// containing the current directory, unless that is the root project and
/// NX_DEFAULT_PROJECT names another; without one, NX_DEFAULT_PROJECT, then
/// nx.json `cli.defaultProjectName`, then `defaultProject`.
fn current_project(workspace: &Workspace) -> Result<String> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()?;
    let root = workspace.root.canonicalize()?;
    let containing = cwd.strip_prefix(&root).ok().and_then(|relative| {
        workspace
            .projects
            .iter()
            .filter(|(_, project)| project.root == "." || relative.starts_with(&project.root))
            .max_by_key(|(_, project)| {
                if project.root == "." {
                    0
                } else {
                    project.root.len()
                }
            })
            .map(|(name, project)| (name.clone(), project.root == "."))
    });
    let fallback = std::env::var("NX_DEFAULT_PROJECT")
        .ok()
        .filter(|name| !name.is_empty());
    match containing {
        Some((name, false)) => Ok(name),
        Some((name, true)) => Ok(fallback.unwrap_or(name)),
        None => fallback
            .or_else(|| {
                let config = &workspace.config.extra;
                config
                    .get("cli")
                    .and_then(|cli| cli.get("defaultProjectName"))
                    .or_else(|| config.get("defaultProject"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .context(
                "no project contains the current directory; name one as `qk <target> <project>`",
            ),
    }
}

fn run(cli: Cli) -> Result<i32> {
    let root = match cli.workspace {
        Some(path) => path,
        None => find_workspace(&std::env::current_dir().context("cannot read current directory")?)?,
    };
    let workspace = Workspace::load(&root)?;
    match cli.command {
        Command::Cache {
            command: CacheCommand::Path,
        } => {
            writeln!(
                io::stdout().lock(),
                "{}",
                qk_cache::cache_location(&workspace).display()
            )?;
        }
        Command::Cache {
            command: CacheCommand::Prune { max_size },
        } => {
            let cache = qk_cache::cache_location(&workspace);
            let limit = match max_size {
                Some(text) => Some(qk_cache::parse_size(&text)?),
                None => qk_cache::max_size(workspace.config.extra.get("maxCacheSize"), &cache)?,
            };
            let pruned = qk_cache::prune(&cache, limit.unwrap_or(u64::MAX))?;
            writeln!(
                io::stdout().lock(),
                "Evicted {} entries and {} blobs, freeing {} bytes; the cache holds {} bytes.",
                pruned.entries,
                pruned.blobs,
                pruned.freed,
                pruned.size
            )?;
        }
        Command::Run { task, options } => {
            let task = if task.contains(':') {
                task
            } else {
                format!("{}:{task}", current_project(&workspace)?)
            };
            let mut request = Request::parse(&task)?;
            // As in Nx, `project:a:b` is the target `a:b` when the project has
            // one, and the target `a` in configuration `b` otherwise.
            if let Some(configuration) = &request.configuration {
                let combined = format!("{}:{configuration}", request.target);
                if workspace
                    .projects
                    .get(&request.project)
                    .is_some_and(|project| project.targets.contains_key(&combined))
                {
                    request.target = combined;
                    request.configuration = None;
                }
            }
            if let Some(configuration) = &options.configuration() {
                if request
                    .configuration
                    .as_ref()
                    .is_some_and(|existing| existing != configuration)
                {
                    bail!("configuration in task identifier conflicts with --configuration");
                }
                request.configuration = Some(configuration.clone());
            }
            request.args = options.args.clone();
            return execute_tasks(&workspace, vec![request], &options, true);
        }
        Command::RunMany {
            targets,
            projects,
            exclude,
            options,
        } => {
            let selected = select_projects(&workspace.projects, &projects, &exclude)?;
            let requests = requests(
                &workspace,
                selected,
                &targets,
                options.configuration().as_ref(),
                &options.args,
            );
            return execute_tasks(&workspace, requests, &options, false);
        }
        Command::Affected {
            targets,
            projects,
            exclude,
            changes,
            granularity: Granularity::Task,
            options,
        } => {
            let selected = select_projects(&workspace.projects, &projects, &exclude)?;
            let graph = TaskGraph::build(
                &workspace,
                &requests(
                    &workspace,
                    selected,
                    &targets,
                    options.configuration().as_ref(),
                    &options.args,
                ),
            )?;
            let analysis = qk_affected::affected_tasks(&workspace, &graph, &changes.options())?;
            warn_landed(analysis.range.as_ref());
            let requests: Vec<Request> = graph
                .roots
                .iter()
                .filter(|id| analysis.tasks.contains_key(*id))
                .map(|id| {
                    let task = &graph.tasks[id];
                    Request {
                        project: task.project.clone(),
                        target: task.target.clone(),
                        configuration: task.configuration.clone(),
                        args: task.args.clone(),
                    }
                })
                .collect();
            if requests.is_empty() {
                eprintln!("qk: no affected tasks");
                return Ok(0);
            }
            return execute_tasks(&workspace, requests, &options, false);
        }
        Command::Affected {
            targets,
            projects,
            exclude,
            changes,
            options,
            ..
        } => {
            let affected = changes.affected(&workspace)?;
            let selected = select_projects(&workspace.projects, &projects, &exclude)?
                .into_iter()
                .filter(|project| affected.contains(project));
            let requests = requests(
                &workspace,
                selected,
                &targets,
                options.configuration().as_ref(),
                &options.args,
            );
            if requests.is_empty() {
                eprintln!("qk: no affected tasks");
                return Ok(0);
            }
            return execute_tasks(&workspace, requests, &options, false);
        }
        Command::Show {
            command:
                ShowCommand::Projects {
                    affected,
                    changes,
                    projects,
                    exclude,
                    with_target,
                    kind,
                    json,
                    sep,
                },
        } => {
            let mut selected = select_projects(&workspace.projects, &projects, &exclude)?;
            if let Some(kind) = kind {
                let graph = ProjectGraph::build(&workspace)?;
                let wanted = match kind {
                    ProjectKind::App => "app",
                    ProjectKind::Lib => "lib",
                    ProjectKind::E2e => "e2e",
                };
                selected.retain(|project| graph.nodes[project].kind == wanted);
            }
            if !with_target.is_empty() {
                selected.retain(|project| {
                    with_target
                        .iter()
                        .any(|target| workspace.projects[project].targets.contains_key(target))
                });
            }
            // As in Nx, naming what changed implies --affected.
            if affected || changes.given() {
                let affected = changes.affected(&workspace)?;
                selected.retain(|project| affected.contains(project));
            }
            let names = graph_order(&workspace.projects, selected);
            if json {
                // Compact, like `nx show projects --json`.
                let mut stdout = io::stdout().lock();
                serde_json::to_writer(&mut stdout, &names)?;
                writeln!(stdout)?;
            } else if let Some(sep) = sep {
                writeln!(io::stdout().lock(), "{}", names.join(&sep))?;
            } else {
                let mut stdout = io::stdout().lock();
                for name in names {
                    writeln!(stdout, "{name}")?;
                }
            }
        }
        Command::Show {
            command:
                ShowCommand::Affected {
                    project,
                    changes,
                    json,
                },
        } => {
            let analysis = changes.analyse(&workspace)?;
            explain::explain(
                &workspace,
                &analysis,
                project.as_deref(),
                json,
                &mut io::stdout().lock(),
            )?;
        }
        Command::Show {
            command:
                ShowCommand::Tasks {
                    targets,
                    projects,
                    exclude,
                    affected,
                    changes,
                    configuration,
                    json,
                },
        } => {
            let selected = select_projects(&workspace.projects, &projects, &exclude)?;
            let graph = TaskGraph::build(
                &workspace,
                &requests(&workspace, selected, &targets, configuration.as_ref(), &[]),
            )?;
            explain::tasks(
                &workspace,
                &graph,
                affected
                    .then(|| qk_affected::affected_tasks(&workspace, &graph, &changes.options()))
                    .transpose()?
                    .as_ref(),
                json,
                &mut io::stdout().lock(),
            )?;
        }
        Command::Show {
            command: ShowCommand::Runs { limit, json },
        } => history::show_runs(&workspace, limit, json, &mut io::stdout().lock())?,
        Command::Show {
            command: ShowCommand::Run { id, json },
        } => history::show_run(&workspace, id.as_deref(), json, &mut io::stdout().lock())?,
        Command::Show {
            command: ShowCommand::Task { id, limit, json },
        } => history::show_task(&workspace, &id, limit, json, &mut io::stdout().lock())?,
        Command::Show {
            command: ShowCommand::Project { name, .. },
        } => {
            let Some(project) = workspace.projects.get(&name) else {
                bail!("unknown project {name:?}");
            };
            print_json(project)?;
        }
        Command::Graph { file, external } => {
            let graph = ProjectGraph::build(&workspace)?;
            let mut report = serde_json::to_value(GraphReport { graph: &graph })?;
            if external {
                external_nodes(&workspace, &mut report["graph"])?;
            }
            let mut bytes = serde_json::to_vec_pretty(&report)?;
            bytes.push(b'\n');
            if file.as_os_str() == "-" {
                io::stdout().lock().write_all(&bytes)?;
            } else {
                std::fs::write(&file, bytes)
                    .with_context(|| format!("cannot write graph to {}", file.display()))?;
            }
        }
    }
    Ok(0)
}

fn execute_tasks(
    workspace: &Workspace,
    requests: Vec<Request>,
    options: &RunOptions,
    single: bool,
) -> Result<i32> {
    options.export();
    let graph = TaskGraph::build_with(
        workspace,
        &requests,
        qk_taskgraph::BuildOptions {
            exclude_task_dependencies: options.exclude_task_dependencies,
            ignore_cycles: options.nx_ignore_cycles || env_flag("NX_IGNORE_CYCLES"),
        },
    )?;
    for cycle in &graph.cycles {
        qk_executor::status!(
            "qk: the task graph has a cycle, broken as Nx does: {}",
            cycle.join(" -> ")
        );
    }
    if let Some(target) = options.graph.as_deref().filter(|target| *target != "false") {
        let mut bytes = serde_json::to_vec_pretty(&nx_task_graph(workspace, &graph)?)?;
        bytes.push(b'\n');
        if target == "stdout" || target == "true" {
            io::stdout().lock().write_all(&bytes)?;
        } else {
            std::fs::write(target, bytes)
                .with_context(|| format!("cannot write the task graph to {target}"))?;
        }
        return Ok(0);
    }
    if options.dry_run {
        // Preparing has no side effects, and catches what would stop a real run.
        let environment: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        for (id, task) in &graph.tasks {
            qk_executor::prepare(workspace, task, &environment)
                .with_context(|| format!("cannot execute {id}"))?;
        }
        print_json(&graph)?;
        return Ok(0);
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })
    .context("cannot register cancellation handler")?;
    let skip_cache = options.skips_cache();
    let rendered = options
        .output_style
        .map_or_else(|| default_output_style(single), OutputStyle::rendered);
    let sink = match rendered {
        Rendered::Lines(_) => ui::SinkKind::Lines,
        Rendered::Quiet => {
            let quiet = Arc::new(ui::Quiet::default());
            qk_executor::report::install(Box::new(quiet.clone()));
            ui::SinkKind::Quiet(quiet)
        }
        Rendered::Dynamic => {
            let dynamic = ui::Dynamic::start();
            qk_executor::report::install(Box::new(dynamic.clone()));
            ui::SinkKind::Dynamic(dynamic)
        }
    };
    // Only the overrides of projects with a task in this run.
    let projects: std::collections::BTreeSet<&str> = graph
        .tasks
        .values()
        .map(|task| task.project.as_str())
        .collect();
    for path in &workspace.local_overrides {
        let root = path
            .rsplit_once('/')
            .map_or(".", |(directory, _)| directory);
        if path == qk_config::LOCAL_WORKSPACE
            || projects
                .iter()
                .any(|project| workspace.projects[*project].root == root)
        {
            qk_executor::status!("qk: using local overrides from {path}");
        }
    }
    let style = match rendered {
        Rendered::Lines(style) => style,
        Rendered::Quiet | Rendered::Dynamic => qk_executor::OutputStyle::Quiet,
    };
    let parallel = options.parallel(workspace)?;
    let cores = options.cores.map_or_else(
        || {
            workspace
                .config
                .extra
                .get("qk:cores")
                .and_then(serde_json::Value::as_u64)
                .filter(|cores| *cores > 0)
                .map_or_else(
                    || std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
                    |cores| cores as usize,
                )
        },
        std::num::NonZeroUsize::get,
    );
    let started = std::time::SystemTime::now();
    let result = qk_runner::run(
        workspace,
        &graph,
        &qk_runner::Settings {
            parallel,
            cores,
            skip_cache,
            bail: options.nx_bail || env_flag("NX_BAIL"),
            style,
        },
        cancelled,
    );
    if let ui::SinkKind::Dynamic(dynamic) = &sink {
        dynamic.finish();
    }
    let result = result?;
    let report = history::record(
        workspace,
        &graph,
        &result,
        started,
        options.report.as_deref(),
    );
    if !skip_cache {
        let cache = qk_cache::cache_location(workspace);
        let pruned = qk_cache::max_size(workspace.config.extra.get("maxCacheSize"), &cache)
            .and_then(|limit| match limit {
                Some(limit) => qk_cache::prune(&cache, limit).map(Some),
                None => Ok(None),
            });
        match pruned {
            Ok(Some(pruned)) if pruned.entries > 0 => qk_executor::status!(
                "qk: evicted {} cache entries to stay under the cache size limit",
                pruned.entries
            ),
            Ok(_) => {}
            Err(error) => qk_executor::status!("qk: could not prune the cache: {error:#}"),
        }
    }
    if !single || !matches!(sink, ui::SinkKind::Lines) {
        let summary = ui::summary(
            &result,
            &report,
            parallel,
            cores,
            &ui::warnings(&sink),
            ui::Paint::stderr(),
        );
        qk_executor::report::write_all(true, format!("\n{summary}").as_bytes());
    }
    Ok(result.exit_code)
}

fn warn_landed(range: Option<&qk_affected::Range>) {
    if let Some(warning) = range.and_then(explain::landed_warning) {
        eprintln!("qk: warning: {warning}");
    }
}

/// The task graph as Nx's `--graph` writes it: the project graph, then each
/// task with its target, root, overrides, outputs and flags, its dependencies
/// with continuous ones apart, and as roots the tasks without dependencies.
/// Nx's `taskPlans`, its hashing plan, has no counterpart in qk.
fn nx_task_graph(workspace: &Workspace, graph: &TaskGraph) -> Result<serde_json::Value> {
    use serde_json::json;
    let projects = ProjectGraph::build(workspace)?;
    let continuous = |id: &str| graph.tasks[id].definition.continuous == Some(true);
    let mut tasks = serde_json::Map::new();
    let mut dependencies = serde_json::Map::new();
    let mut continuous_dependencies = serde_json::Map::new();
    let mut roots = Vec::new();
    for (id, task) in &graph.tasks {
        let (outputs, _) = qk_cache::resolved_outputs(workspace, task).unwrap_or_default();
        // As in Nx, a task without a configuration has no such key.
        let mut target = json!({"project": task.project, "target": task.target});
        if let Some(configuration) = &task.configuration {
            target["configuration"] = json!(configuration);
        }
        tasks.insert(
            id.clone(),
            json!({
                "id": id,
                "target": target,
                "projectRoot": workspace.projects[&task.project].root,
                "overrides": {"__overrides_unparsed__": task.args},
                "outputs": outputs,
                "cache": task.definition.cache == Some(true),
                "parallelism": task.definition.parallelism != Some(false),
                "continuous": continuous(id),
            }),
        );
        let (serving, finite): (Vec<&String>, Vec<&String>) = task
            .dependencies
            .iter()
            .partition(|dependency| continuous(dependency));
        if task.dependencies.is_empty() {
            roots.push(id.clone());
        }
        dependencies.insert(id.clone(), json!(finite));
        continuous_dependencies.insert(id.clone(), json!(serving));
    }
    Ok(json!({
        "graph": serde_json::to_value(GraphReport { graph: &projects })?["graph"],
        "tasks": {
            "roots": roots,
            "tasks": tasks,
            "dependencies": dependencies,
            "continuousDependencies": continuous_dependencies,
        },
    }))
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    io::stdout().lock().write_all(&bytes)?;
    Ok(())
}

/// The requested targets of the selected projects that define them.
fn requests(
    workspace: &Workspace,
    projects: impl IntoIterator<Item = String>,
    targets: &[String],
    configuration: Option<&String>,
    args: &[String],
) -> Vec<Request> {
    let mut requests = Vec::new();
    for project in projects {
        for target in targets {
            if let Some(definition) = workspace.projects[&project].targets.get(target) {
                // Like Nx, a target without the configuration runs its default.
                let configuration = configuration
                    .cloned()
                    .filter(|name| definition.configurations.contains_key(name));
                requests.push(Request {
                    project: project.clone(),
                    target: target.clone(),
                    configuration,
                    args: args.to_vec(),
                });
            }
        }
    }
    requests
}

/// Adds `externalNodes` from the pnpm lockfile to a graph report: one node per
/// installation, `npm:<name>@<version>`, edges from each project to what its
/// importer installs directly, and edges between installations.
fn external_nodes(workspace: &Workspace, graph: &mut serde_json::Value) -> Result<()> {
    use serde_json::json;
    let text = std::fs::read_to_string(workspace.root.join("pnpm-lock.yaml"))
        .context("--external needs a pnpm-lock.yaml")?;
    let lockfile = qk_lockfile::Lockfile::parse(&text)?;
    let node = |key: &str| format!("npm:{key}");
    let mut nodes = serde_json::Map::new();
    for installed in lockfile.installations() {
        nodes.insert(
            node(installed.key),
            json!({"type": "npm", "name": node(installed.key), "data": {
                "packageName": installed.name, "version": installed.version, "hash": installed.integrity,
            }}),
        );
        graph["dependencies"][node(installed.key)] = installed
            .dependencies
            .iter()
            .map(|dependency| json!({"source": node(installed.key), "target": node(dependency), "type": "static"}))
            .collect();
    }
    for project in workspace.projects.values() {
        let Some(direct) = lockfile.direct_snapshots(&project.root) else {
            continue;
        };
        let edges = graph["dependencies"][&project.name]
            .as_array_mut()
            .context("project dependencies must be an array")?;
        for key in direct {
            edges.push(json!({"source": project.name, "target": node(&key), "type": "static"}));
        }
    }
    graph["externalNodes"] = serde_json::Value::Object(nodes);
    Ok(())
}
