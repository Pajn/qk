//! Standalone project watching: native file events, debounced batches and
//! commands in cancellable process groups. No Nx daemon is involved.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use qk_config::Workspace;
use qk_executor::{Display, Outcome, PreparedTask};
use qk_graph::{ProjectGraph, select_projects};
use qk_taskgraph::Task;

#[derive(Args)]
pub struct Options {
    #[arg(short = 'p', long, value_delimiter = ',', num_args = 1.., required_unless_present = "all", conflicts_with = "all")]
    projects: Vec<String>,
    #[arg(long)]
    all: bool,
    #[arg(short = 'd', long, alias = "includeDependencies")]
    include_dependencies: bool,
    #[arg(short = 'i', long, alias = "initialRun")]
    initial_run: bool,
    #[arg(long)]
    verbose: bool,
    /// Shell command after --. It runs from the invocation directory.
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

struct View {
    workspace: Workspace,
    selected: BTreeSet<String>,
    files: BTreeSet<String>,
    outputs: Vec<qk_cache::Outputs>,
    cache: PathBuf,
    ignore: qk_cache::SourceIgnore,
}

impl View {
    /// Reload discovery and selection so --all also sees newly created projects.
    fn load(root: &Path, options: &Options) -> Result<Self> {
        let workspace = Workspace::load(root)?;
        let mut selected = select_projects(&workspace.projects, &options.projects, &[])?;
        if options.include_dependencies {
            let graph = ProjectGraph::build(&workspace)?;
            let mut pending = selected.clone();
            while let Some(project) = pending.pop_first() {
                for edge in &graph.dependencies[&project] {
                    if selected.insert(edge.target.clone()) {
                        pending.insert(edge.target.clone());
                    }
                }
            }
        }
        let mut outputs = Vec::new();
        for (project, definition) in &workspace.projects {
            for (target, definition) in &definition.targets {
                let task = Task {
                    id: format!("{project}:{target}"),
                    project: project.clone(),
                    target: target.clone(),
                    configuration: None,
                    args: Vec::new(),
                    definition: definition.clone(),
                    dependencies: BTreeSet::new(),
                };
                let mut variants = vec![task.clone()];
                for configuration in definition.configurations.values() {
                    let mut configured = task.clone();
                    configured.definition.options.extend(configuration.clone());
                    if let Some(value) = configuration.get("outputs") {
                        configured.definition.outputs = serde_json::from_value(value.clone()).ok();
                    }
                    variants.push(configured);
                }
                for variant in variants {
                    if variant.definition.outputs.is_some()
                        && let Ok(output) = qk_cache::Outputs::new(&workspace, &variant)
                    {
                        outputs.push(output);
                    }
                    if let Ok(Some(warm)) = qk_cache::warm::config(&workspace, &variant)
                        && let Ok(output) = qk_cache::Outputs::from_paths(&warm.paths)
                    {
                        outputs.push(output);
                    }
                }
            }
        }
        let ignore = qk_cache::SourceIgnore::new(&workspace.root)?;
        let files = qk_cache::source_files(&workspace.root)?;
        let cache = qk_cache::cache_location(&workspace);
        Ok(Self {
            workspace,
            selected,
            files,
            outputs,
            cache,
            ignore,
        })
    }

    /// Filter runner state and generated artifacts before discovery or callbacks.
    fn ignored(&self, path: &str) -> bool {
        self.ignore.matches(path)
            || path
                .split('/')
                .any(|part| matches!(part, ".git" | ".qk" | "node_modules"))
            || self.workspace.root.join(path).starts_with(&self.cache)
            || self.outputs.iter().any(|outputs| outputs.matches(path))
    }

    /// A file belongs to the project whose root contains it most specifically.
    fn owner(&self, path: &str) -> Option<String> {
        self.workspace
            .projects
            .iter()
            .filter(|(_, project)| {
                project.root == "." || Path::new(path).starts_with(&project.root)
            })
            .max_by_key(|(_, project)| {
                if project.root == "." {
                    0
                } else {
                    project.root.len()
                }
            })
            .filter(|(name, _)| self.selected.contains(*name))
            .map(|(name, _)| name.clone())
    }
}

/// Watch native file changes, retaining queued events while a callback runs.
pub fn run(workspace: &Workspace, options: Options) -> Result<i32> {
    if options.command.is_empty() {
        bail!("watch needs a command after --");
    }
    let root = workspace.root.canonicalize()?;
    let cwd = std::env::current_dir()?;
    let command = options.command.join(" ");
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .context("cannot register cancellation handler")?;
    let (sender, receiver) = mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(sender).context("cannot start filesystem watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .context("cannot watch workspace")?;
    let mut view = View::load(&root, &options)?;
    if options.initial_run {
        // Nx --all initially runs once without a project name; named selections
        // initially run in the explicitly requested projects, not their closure.
        let projects = if options.all {
            BTreeSet::new()
        } else {
            select_projects(&view.workspace.projects, &options.projects, &[])?
        };
        callbacks(
            &command,
            &cwd,
            &projects,
            &BTreeSet::new(),
            &options,
            &cancelled,
        )?;
    }
    eprintln!("qk: watching {} projects", view.selected.len());
    let mut pending_paths = BTreeSet::new();
    while !cancelled.load(Ordering::SeqCst) {
        let first = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => watch_event(event),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
        };
        let mut paths = std::mem::take(&mut pending_paths);
        add_event(first, &root, &view, &mut paths);
        if paths.is_empty() {
            continue;
        }
        let mut deadline = Instant::now() + Duration::from_millis(150);
        // Bound a batch even if files keep arriving. Events after the deadline
        // stay queued and are processed after the current commands finish.
        let limit = Instant::now() + Duration::from_secs(1);
        while !cancelled.load(Ordering::SeqCst) && Instant::now() < deadline.min(limit) {
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => {
                    let before = paths.len();
                    add_event(watch_event(event), &root, &view, &mut paths);
                    if paths.len() != before {
                        deadline = Instant::now() + Duration::from_millis(150);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
            }
        }
        let next = match View::load(&root, &options) {
            Ok(next) => next,
            Err(error) => {
                eprintln!("qk: watch configuration not reloaded: {error:#}");
                // Retry this batch on the next event, such as a completed save.
                pending_paths = paths;
                continue;
            }
        };
        let candidates: BTreeSet<_> = view.files.union(&next.files).cloned().collect();
        let files: BTreeSet<_> = candidates
            .into_iter()
            .filter(|file| {
                paths.iter().any(|path| {
                    path.is_empty() || file == path || Path::new(file).starts_with(path)
                }) && !view.ignored(file)
                    && !next.ignored(file)
                    && (view.owner(file).is_some() || next.owner(file).is_some())
            })
            .collect();
        let projects: BTreeSet<_> = files
            .iter()
            .filter_map(|file| next.owner(file).or_else(|| view.owner(file)))
            .collect();
        view = next;
        if !projects.is_empty() {
            callbacks(&command, &cwd, &projects, &files, &options, &cancelled)?;
        }
    }
    Ok(130)
}

/// Recover backend errors by rescanning rather than ending the watch session.
fn watch_event(result: notify::Result<Event>) -> Event {
    result.unwrap_or_else(|error| {
        eprintln!("qk: watch event error: {error}");
        Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
    })
}

/// Collect write/create/delete/rename events; access events cannot trigger loops.
fn add_event(event: Event, root: &Path, view: &View, paths: &mut BTreeSet<String>) {
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    if event.need_rescan() {
        paths.insert(String::new());
    }
    for path in event.paths {
        if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            // Policy changes must reload even when the old policy ignores itself.
            let policy = matches!(relative.as_str(), ".gitignore" | ".nxignore");
            if !relative.is_empty() && (policy || !view.ignored(&relative)) {
                paths.insert(relative);
            }
        }
    }
}

/// Run a batch concurrently by changed project, or once when no project variable
/// occurs in the command. Failed callbacks are reported and watching continues.
fn callbacks(
    command: &str,
    cwd: &Path,
    projects: &BTreeSet<String>,
    files: &BTreeSet<String>,
    options: &Options,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let names = if command.contains("NX_PROJECT_NAME") && !projects.is_empty() {
        projects.iter().cloned().collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };
    let files = files.iter().cloned().collect::<Vec<_>>().join(" ");
    std::thread::scope(|scope| -> Result<()> {
        let mut jobs = Vec::new();
        for name in names {
            let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
            env.insert("NX_PROJECT_NAME".into(), name.clone().into());
            env.insert("NX_FILE_CHANGES".into(), files.clone().into());
            let prepared = PreparedTask {
                id: format!("watch:{name}"),
                commands: vec![command.to_owned()],
                cwd: cwd.to_owned(),
                env,
                execution: BTreeMap::new(),
                parallel: false,
                ready_when: Vec::new(),
                ready: Default::default(),
                decorations: Vec::new(),
                interactive: false,
                sandbox: None,
                display: Display::Stream,
            };
            if options.verbose {
                eprintln!("qk: watch {name}: {command} ({files})");
            }
            jobs.push(scope.spawn(move || qk_executor::execute(&prepared, cancelled)));
        }
        for job in jobs {
            let outcome = job
                .join()
                .map_err(|_| anyhow::anyhow!("watch callback panicked"))??;
            if outcome != Outcome::Success && outcome != Outcome::Cancelled {
                eprintln!("qk: watch command failed: {outcome:?}");
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-ignored policy files remain observable, so removing exclusions works.
    #[test]
    fn self_ignored_policies_can_reload_source_selection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("app")).unwrap();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::write(root.join("app/project.json"), r#"{"name":"app"}"#).unwrap();
        std::fs::write(root.join("app/ignored.txt"), "source").unwrap();
        std::fs::write(root.join(".gitignore"), ".gitignore\n").unwrap();
        std::fs::write(root.join(".nxignore"), ".nxignore\napp/ignored.txt\n").unwrap();
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let view = View::load(root, &options).unwrap();
        assert!(!view.files.contains("app/ignored.txt"));
        let mut paths = BTreeSet::new();
        for name in [".gitignore", ".nxignore"] {
            assert!(view.ignored(name));
            add_event(
                Event::new(EventKind::Any).add_path(root.join(name)),
                root,
                &view,
                &mut paths,
            );
            assert!(paths.contains(name));
        }
        std::fs::write(root.join(".nxignore"), ".nxignore\n").unwrap();
        let reloaded = View::load(root, &options).unwrap();
        assert!(reloaded.files.contains("app/ignored.txt"));
        paths.clear();
        add_event(
            Event::new(EventKind::Any).add_path(root.join("app/ignored.txt")),
            root,
            &reloaded,
            &mut paths,
        );
        assert!(paths.contains("app/ignored.txt"));
        assert_eq!(reloaded.owner("app/ignored.txt").as_deref(), Some("app"));
    }

    /// A backend failure queues a full rescan and subsequent events remain usable.
    #[test]
    fn backend_error_requests_rescan() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nx.json"), "{}").unwrap();
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let view = View::load(temp.path(), &options).unwrap();
        let mut paths = BTreeSet::new();
        add_event(
            watch_event(Err(notify::Error::generic("backend overflow"))),
            temp.path(),
            &view,
            &mut paths,
        );
        add_event(
            watch_event(Ok(
                Event::new(EventKind::Any).add_path(temp.path().join("source.txt"))
            )),
            temp.path(),
            &view,
            &mut paths,
        );
        assert_eq!(paths, BTreeSet::from([String::new(), "source.txt".into()]));
    }
}
