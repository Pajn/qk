mod explain;

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
    after_help = "Shorthand, as in Nx: `qk <target> [project]` and `qk <project>:<target>` run like `qk run`.\nWithout a project, the project containing the current directory is used."
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
        #[command(flatten)]
        options: RunOptions,
    },
    /// Inspect workspace projects and normalized configuration.
    Show {
        #[command(subcommand)]
        command: ShowCommand,
    },
    /// Export the workspace project graph as JSON (external nodes not yet included).
    Graph {
        /// Output file; use - for stdout. Paths are relative to the current directory.
        #[arg(long, value_name = "PATH", default_value = "-")]
        file: PathBuf,
    },
}

#[derive(Args)]
struct RunOptions {
    /// Bypass all local cache reads and writes.
    #[arg(long, aliases = ["skip-nx-cache", "skipNxCache"])]
    skip_cache: bool,
    #[arg(short = 'c', long)]
    configuration: Option<String>,
    /// Maximum number of tasks executing concurrently.
    #[arg(long, env = "NX_PARALLEL", default_value = "3")]
    parallel: std::num::NonZeroUsize,
    /// Print the task graph as JSON without executing commands or loading dotenv.
    #[arg(long)]
    dry_run: bool,
    /// Only raw streaming output is currently supported.
    #[arg(long, value_enum, default_value = "stream")]
    output_style: OutputStyle,
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
    fn affected(&self, workspace: &Workspace) -> Result<std::collections::BTreeSet<String>> {
        Ok(self.analyse(workspace)?.projects.into_keys().collect())
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

#[derive(Clone, ValueEnum)]
enum OutputStyle {
    Stream,
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Print the shared cache directory without creating it.
    Path,
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
        /// Emit a JSON array instead of one name per line.
        #[arg(long)]
        json: bool,
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
fn current_project(workspace: &Workspace) -> Result<String> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()?;
    let root = workspace.root.canonicalize()?;
    let relative = cwd.strip_prefix(&root).map_err(|_| {
        anyhow::anyhow!("the current directory is outside the workspace; name a project")
    })?;
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
        .map(|(name, _)| name.clone())
        .context("no project contains the current directory; name one as `qk <target> <project>`")
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
                qk_cache::cache_directory(&workspace.root).display()
            )?;
        }
        Command::Run { task, options } => {
            let task = if task.contains(':') {
                task
            } else {
                format!("{}:{task}", current_project(&workspace)?)
            };
            let mut request = Request::parse(&task)?;
            if let Some(configuration) = &options.configuration {
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
            return execute_tasks(&workspace, vec![request], &options);
        }
        Command::RunMany {
            targets,
            projects,
            exclude,
            options,
        } => {
            let selected = select_projects(&workspace.projects, &projects, &exclude)?;
            let requests = requests(&workspace, selected, &targets, &options);
            return execute_tasks(&workspace, requests, &options);
        }
        Command::Affected {
            targets,
            projects,
            exclude,
            changes,
            options,
        } => {
            let affected = changes.affected(&workspace)?;
            let selected = select_projects(&workspace.projects, &projects, &exclude)?
                .into_iter()
                .filter(|project| affected.contains(project));
            let requests = requests(&workspace, selected, &targets, &options);
            if requests.is_empty() {
                eprintln!("qk: no affected tasks");
                return Ok(0);
            }
            return execute_tasks(&workspace, requests, &options);
        }
        Command::Show {
            command:
                ShowCommand::Projects {
                    affected,
                    changes,
                    projects,
                    exclude,
                    json,
                },
        } => {
            let mut selected = select_projects(&workspace.projects, &projects, &exclude)?;
            if affected {
                let affected = changes.affected(&workspace)?;
                selected.retain(|project| affected.contains(project));
            }
            let names = graph_order(&workspace.projects, selected);
            if json {
                // Compact, like `nx show projects --json`.
                let mut stdout = io::stdout().lock();
                serde_json::to_writer(&mut stdout, &names)?;
                writeln!(stdout)?;
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
            command: ShowCommand::Project { name, .. },
        } => {
            let Some(project) = workspace.projects.get(&name) else {
                bail!("unknown project {name:?}");
            };
            print_json(project)?;
        }
        Command::Graph { file } => {
            let graph = ProjectGraph::build(&workspace)?;
            let mut bytes = serde_json::to_vec_pretty(&GraphReport { graph: &graph })?;
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
) -> Result<i32> {
    let graph = TaskGraph::build(workspace, &requests)?;
    if options.dry_run {
        print_json(&graph)?;
        return Ok(0);
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })
    .context("cannot register cancellation handler")?;
    let result = qk_runner::run(
        workspace,
        &graph,
        options.parallel.get(),
        options.skip_cache,
        cancelled,
    )?;
    Ok(result.exit_code)
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
    options: &RunOptions,
) -> Vec<Request> {
    let mut requests = Vec::new();
    for project in projects {
        for target in targets {
            if let Some(definition) = workspace.projects[&project].targets.get(target) {
                // Like Nx, a target without the configuration runs its default.
                let configuration = options
                    .configuration
                    .clone()
                    .filter(|name| definition.configurations.contains_key(name));
                requests.push(Request {
                    project: project.clone(),
                    target: target.clone(),
                    configuration,
                    args: options.args.clone(),
                });
            }
        }
    }
    requests
}
