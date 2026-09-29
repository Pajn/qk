use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use qk_config::{Workspace, find_workspace};
use qk_graph::{GraphReport, ProjectGraph, select_projects};

#[derive(Parser)]
#[command(
    name = "qk",
    version,
    about = "Quick workspace task tooling",
    long_about = "qk (quick): a standalone Rust task runner, under development.\nCurrently supports workspace inspection; execution and caching are not implemented yet."
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
    if let Err(error) = run(Cli::parse()) {
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

fn run(cli: Cli) -> Result<()> {
    let root = match cli.workspace {
        Some(path) => path,
        None => find_workspace(&std::env::current_dir().context("cannot read current directory")?)?,
    };
    let workspace = Workspace::load(&root)?;
    match cli.command {
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
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    io::stdout().lock().write_all(&bytes)?;
    Ok(())
}
