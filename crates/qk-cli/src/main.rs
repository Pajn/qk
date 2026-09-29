use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use qk_config::{Workspace, find_workspace};
use qk_graph::{GraphReport, ProjectGraph, select_projects};
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
    long_about = "qk (quick): a standalone Rust task runner.\nExecute finite tasks with dependency ordering and a local cache shared by Git worktrees."
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
        task: String,
        #[command(flatten)]
        options: RunOptions,
    },
    /// Execute targets on selected projects and their dependencies.
    RunMany {
        #[arg(short = 't', long, required = true, value_delimiter = ',')]
        targets: Vec<String>,
        #[arg(short = 'p', long, value_delimiter = ',')]
        projects: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        exclude: Vec<String>,
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
    /// List project names, sorted alphabetically.
    Projects {
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
    /// Show a project's normalized configuration.
    Project {
        name: String,
        /// Emit JSON (currently the default and only format).
        #[arg(long)]
        json: bool,
    },
}

fn main() {
    match run(Cli::parse()) {
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
            let mut requests = Vec::new();
            for project in selected {
                for target in &targets {
                    if workspace.projects[&project].targets.contains_key(target) {
                        requests.push(Request {
                            project: project.clone(),
                            target: target.clone(),
                            configuration: options.configuration.clone(),
                            args: options.args.clone(),
                        });
                    }
                }
            }
            return execute_tasks(&workspace, requests, &options);
        }
        Command::Show {
            command:
                ShowCommand::Projects {
                    projects,
                    exclude,
                    json,
                },
        } => {
            let names = select_projects(&workspace.projects, &projects, &exclude)?;
            if json {
                print_json(&names)?;
            } else {
                let mut stdout = io::stdout().lock();
                for name in names {
                    writeln!(stdout, "{name}")?;
                }
            }
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
