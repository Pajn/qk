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
    regular_files: BTreeSet<String>,
    outputs: Vec<qk_cache::Outputs>,
    cache: PathBuf,
    ignore: qk_cache::SourceIgnore,
}

impl View {
    /// Configuration and policy files can change discovery, ownership or filters.
    fn control_file(&self, path: &str) -> bool {
        matches!(
            Path::new(path).file_name().and_then(|name| name.to_str()),
            Some(
                "nx.json"
                    | "nx.local.json"
                    | "project.json"
                    | "project.local.json"
                    | "package.json"
                    | "pnpm-workspace.yaml"
                    | "pnpm-lock.yaml"
                    | ".gitignore"
                    | ".nxignore"
                    | ".ignore"
            )
        ) || self.preset_file(path)
    }

    /// Observe a preset's logical symlink path as well as its canonical target.
    fn preset_file(&self, path: &str) -> bool {
        self.workspace.extended.as_deref() == Some(path)
            || self.workspace.extended_logical.as_deref() == Some(path)
    }

    /// Existing regular source edits keep ownership and discovery unchanged.
    /// New/deleted files, directories, symlinks and rescan requests still reload.
    fn reusable_for(&self, paths: &BTreeSet<String>) -> bool {
        // Package/external preset resolution has inputs outside the observed
        // source set. Keep reloading those workspaces on each source batch.
        if self
            .workspace
            .extended_specifier
            .as_deref()
            .is_some_and(|specifier| {
                self.workspace.extended.is_none()
                    || self.workspace.extended_logical.is_none()
                    || !(specifier.starts_with("./")
                        || specifier.starts_with("../")
                        || Path::new(specifier).is_absolute())
            })
        {
            return false;
        }
        paths.iter().all(|path| {
            self.regular_files.contains(path)
                && !self.control_file(path)
                && std::fs::symlink_metadata(self.workspace.root.join(path))
                    .is_ok_and(|metadata| metadata.file_type().is_file())
        })
    }

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
        let regular_files = files
            .iter()
            .filter(|path| {
                std::fs::symlink_metadata(workspace.root.join(path))
                    .is_ok_and(|metadata| metadata.file_type().is_file())
            })
            .cloned()
            .collect();
        let cache = qk_cache::cache_location(&workspace);
        Ok(Self {
            workspace,
            selected,
            files,
            regular_files,
            outputs,
            cache,
            ignore,
        })
    }

    /// Filter runner state and generated artifacts before discovery or callbacks.
    fn ignored(&self, path: &str) -> bool {
        self.ignore.matches(path) || self.reserved(path)
    }

    /// Generated outputs and dependency/runner state cannot become source events.
    fn reserved(&self, path: &str) -> bool {
        path.split('/')
            .any(|part| matches!(part, ".git" | ".qk" | "node_modules"))
            || self.generated(path)
    }

    /// Explicit outputs and cache directories remain excluded even for controls.
    fn generated(&self, path: &str) -> bool {
        self.workspace.root.join(path).starts_with(&self.cache)
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
    let mut pending_reload = false;
    while !cancelled.load(Ordering::SeqCst) {
        let first = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => watch_event(event),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
        };
        let mut paths = std::mem::take(&mut pending_paths);
        let mut reload = std::mem::take(&mut pending_reload);
        reload |= add_event(first, &root, &view, &mut paths);
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
                    reload |= add_event(watch_event(event), &root, &view, &mut paths);
                    if paths.len() != before {
                        deadline = Instant::now() + Duration::from_millis(150);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
            }
        }
        if !reload && view.reusable_for(&paths) {
            let files = changed_files(&view, &view, &paths);
            let projects = files.iter().filter_map(|file| view.owner(file)).collect();
            if !files.is_empty() {
                callbacks(&command, &cwd, &projects, &files, &options, &cancelled)?;
            }
            continue;
        }
        let next = match View::load(&root, &options) {
            Ok(next) => next,
            Err(error) => {
                eprintln!("qk: watch configuration not reloaded: {error:#}");
                // Retry this batch on the next event, such as a completed save.
                pending_paths = paths;
                pending_reload = reload;
                continue;
            }
        };
        let files = changed_files(&view, &next, &paths);
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

/// Select changed sources using current policy, preserving deleted-file ownership.
fn changed_files(previous: &View, next: &View, paths: &BTreeSet<String>) -> BTreeSet<String> {
    previous
        .files
        .union(&next.files)
        .filter(|file| {
            paths
                .iter()
                .any(|path| path.is_empty() || *file == path || Path::new(file).starts_with(path))
                && !next.ignored(file)
                && (next.files.contains(*file)
                    || std::fs::symlink_metadata(next.workspace.root.join(file))
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound))
                && (previous.owner(file).is_some() || next.owner(file).is_some())
        })
        .cloned()
        .collect()
}

/// Recover backend errors by rescanning rather than ending the watch session.
fn watch_event(result: notify::Result<Event>) -> Event {
    result.unwrap_or_else(|error| {
        eprintln!("qk: watch event error: {error}");
        Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
    })
}

/// Collect write/create/delete/rename events; access events cannot trigger loops.
fn add_event(event: Event, root: &Path, view: &View, paths: &mut BTreeSet<String>) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let structural = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    let mut reload = event.need_rescan();
    if event.need_rescan() {
        paths.insert(String::new());
    }
    for path in event.paths {
        if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            // Policy changes must reload even when the old policy ignores itself.
            let policy = matches!(
                Path::new(&relative)
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some(".gitignore" | ".nxignore" | ".ignore")
            );
            let observable = !view.reserved(&relative)
                || (view.preset_file(&relative)
                    && !view.generated(&relative)
                    && !relative
                        .split('/')
                        .any(|part| matches!(part, ".git" | ".qk")));
            if policy && observable {
                // Source events in this batch may have been hidden by the old policy.
                paths.insert(String::new());
            }
            if !relative.is_empty()
                && observable
                && (view.control_file(&relative) || !view.ignored(&relative))
            {
                paths.insert(relative);
                reload |= structural;
            }
        }
    }
    reload
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

    /// Only existing source edits reuse discovery; controls and structure reload.
    #[test]
    fn source_edits_reuse_view_without_hiding_control_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("app/dist")).unwrap();
        std::fs::write(root.join("nx.json"), r#"{"extends":"./preset.json"}"#).unwrap();
        std::fs::write(root.join("preset.json"), "{}").unwrap();
        std::fs::write(root.join(".gitignore"), "*.local.json\n").unwrap();
        std::fs::write(
            root.join("app/project.json"),
            r#"{"name":"app","targets":{"build":{"command":"echo build","outputs":["{projectRoot}/dist","{projectRoot}/.ignore"]}}}"#,
        ).unwrap();
        std::fs::write(root.join("app/source.txt"), "one").unwrap();
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let view = View::load(root, &options).unwrap();
        let batch = |path: &str| BTreeSet::from([path.to_owned()]);
        std::fs::write(root.join("app/source.txt"), "two").unwrap();
        assert!(view.reusable_for(&batch("app/source.txt")));
        for path in [
            "",
            "app",
            "app/new.txt",
            "app/project.json",
            "preset.json",
            ".gitignore",
        ] {
            assert!(!view.reusable_for(&batch(path)), "{path}");
        }
        std::fs::remove_file(root.join("app/source.txt")).unwrap();
        assert!(!view.reusable_for(&batch("app/source.txt")));
        // Structural events still reload when the final path is a known file.
        std::fs::write(root.join("app/source.txt"), "replacement").unwrap();
        let mut structural_paths = BTreeSet::new();
        assert!(add_event(
            Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(root.join("app/source.txt")),
            root,
            &view,
            &mut structural_paths,
        ));

        let mut paths = BTreeSet::new();
        assert!(view.ignored("nx.local.json"));
        add_event(
            Event::new(EventKind::Any).add_path(root.join("nx.local.json")),
            root,
            &view,
            &mut paths,
        );
        assert!(paths.contains("nx.local.json"));
        assert!(!view.reusable_for(&paths));
        paths.clear();
        // A generated package manifest must not bypass output exclusions.
        add_event(
            Event::new(EventKind::Any).add_path(root.join("app/dist/package.json")),
            root,
            &view,
            &mut paths,
        );
        assert!(paths.is_empty());
        add_event(
            Event::new(EventKind::Any).add_path(root.join("node_modules/pkg/package.json")),
            root,
            &view,
            &mut paths,
        );
        assert!(paths.is_empty());
        // A generated policy at the project root cannot queue an endless rescan.
        assert!(!add_event(
            Event::new(EventKind::Any).add_path(root.join("app/.ignore")),
            root,
            &view,
            &mut paths
        ));
        assert!(paths.is_empty());
    }

    /// A nested exclusion is not a deletion; actual deleted sources stay reported.
    #[test]
    fn nested_policy_rescans_exclude_existing_files_but_keep_deletions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("app")).unwrap();
        for (path, text) in [
            ("nx.json", "{}"),
            (".nxignore", ""),
            ("app/project.json", r#"{"name":"app"}"#),
            ("app/source.txt", "one"),
            ("app/deleted.txt", "one"),
        ] {
            std::fs::write(root.join(path), text).unwrap();
        }
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let before = View::load(root, &options).unwrap();
        std::fs::write(root.join("app/.nxignore"), "source.txt\n").unwrap();
        std::fs::remove_file(root.join("app/deleted.txt")).unwrap();
        let after = View::load(root, &options).unwrap();
        let mut paths = BTreeSet::new();
        add_event(
            Event::new(EventKind::Any).add_path(root.join("app/.nxignore")),
            root,
            &before,
            &mut paths,
        );
        let files = changed_files(&before, &after, &paths);
        assert!(!files.contains("app/source.txt"));
        assert!(files.contains("app/deleted.txt"));
    }

    /// Replacing a logical preset symlink reloads even when the backend says Any.
    #[cfg(unix)]
    #[test]
    fn replacing_a_preset_symlink_cannot_reuse_the_old_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("nx.json"), r#"{"extends":"./preset.json"}"#).unwrap();
        std::fs::write(root.join("base.json"), r#"{"marker":"old"}"#).unwrap();
        std::fs::write(root.join(".gitignore"), "preset.json\n").unwrap();
        std::os::unix::fs::symlink("base.json", root.join("preset.json")).unwrap();
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let before = View::load(root, &options).unwrap();
        assert!(before.ignored("preset.json"));
        std::fs::remove_file(root.join("preset.json")).unwrap();
        std::fs::write(root.join("preset.json"), r#"{"marker":"new"}"#).unwrap();
        assert!(!before.reusable_for(&BTreeSet::from(["preset.json".into()])));
        let mut paths = BTreeSet::new();
        add_event(
            Event::new(EventKind::Any).add_path(root.join("preset.json")),
            root,
            &before,
            &mut paths,
        );
        assert!(paths.contains("preset.json"));
        let after = View::load(root, &options).unwrap();
        assert_eq!(after.workspace.config.extra["marker"], "new");
        assert!(after.control_file("preset.json"));
    }

    /// Local merging cannot hide the specifier used for package resolution.
    #[test]
    fn local_extends_override_cannot_enable_reuse_for_a_package_preset() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("node_modules/plain")).unwrap();
        std::fs::create_dir(root.join("app")).unwrap();
        for (path, text) in [
            ("nx.json", r#"{"extends":"plain"}"#),
            ("nx.local.json", r#"{"extends":"./unused.json"}"#),
            ("node_modules/plain/package.json", r#"{"main":"old.json"}"#),
            ("node_modules/plain/old.json", r#"{"marker":"old"}"#),
            ("node_modules/plain/new.json", r#"{"marker":"new"}"#),
            ("app/project.json", r#"{"name":"app"}"#),
            ("app/source.txt", "one"),
        ] {
            std::fs::write(root.join(path), text).unwrap();
        }
        let options = Options {
            projects: Vec::new(),
            all: true,
            include_dependencies: false,
            initial_run: false,
            verbose: false,
            command: vec!["echo changed".into()],
        };
        let before = View::load(root, &options).unwrap();
        assert_eq!(before.workspace.config.extra["extends"], "./unused.json");
        assert_eq!(
            before.workspace.extended_specifier.as_deref(),
            Some("plain")
        );
        assert!(!before.reusable_for(&BTreeSet::from(["app/source.txt".into()])));
        std::fs::write(
            root.join("node_modules/plain/package.json"),
            r#"{"main":"new.json"}"#,
        )
        .unwrap();
        let after = View::load(root, &options).unwrap();
        assert_eq!(after.workspace.config.extra["marker"], "new");
    }

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
        assert!(paths.contains(""));
        assert!(changed_files(&view, &reloaded, &paths).contains("app/ignored.txt"));
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
