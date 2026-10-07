//! `--sandbox`: each task runs under the system's sandbox, which holds it to
//! what it declares inside the workspace. Everything outside the workspace
//! stays open.
//!
//! Inside it, a task may read its cache key's files, `node_modules`, the
//! lockfile, the outputs of its own and its dependencies' tasks and its
//! warm paths, and list any directory; it may write its outputs and warm
//! paths. In audit mode anything else is allowed and reported; in enforce
//! mode it is refused, except reading and writing `.git` and reading dotenv
//! files, which a task may need although they are not keyed.
//!
//! On macOS that is Seatbelt, through `sandbox-exec`, with a profile per
//! task. Each report names the task through the rule's message, so the
//! kernel's log, read while the run lasts, says which task did what. On
//! Linux it is Landlock, which can only refuse, so only enforce mode is
//! there; a task's rules are built as it starts, since Landlock holds paths
//! that exist and its dependencies' outputs appear during the run.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Task, TaskGraph};
use serde::Serialize;

#[cfg(target_os = "linux")]
mod landlock;
#[cfg(not(target_os = "linux"))]
mod landlock {
    pub fn abi() -> anyhow::Result<u32> {
        anyhow::bail!("Landlock is Linux's")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Allow what a task does beyond its declarations, and report it.
    Audit,
    /// Refuse it.
    Enforce,
}

/// What one task did beyond what it declares.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Findings {
    /// Workspace files it read that are not in its cache key.
    pub undeclared_reads: BTreeSet<String>,
    /// `.git` and dotenv files it read: allowed, but not in the key either.
    pub unkeyed_reads: BTreeSet<String>,
    /// Paths it wrote in the workspace outside its outputs.
    pub stray_writes: BTreeSet<String>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.undeclared_reads.is_empty()
            && self.unkeyed_reads.is_empty()
            && self.stray_writes.is_empty()
    }
}

#[cfg(target_os = "linux")]
type Ruleset = std::os::fd::OwnedFd;
#[cfg(not(target_os = "linux"))]
type Ruleset = ();

/// What one task may do in the workspace, as absolute paths.
#[derive(Clone, Debug, Default)]
struct Plan {
    /// Directories whose every tracked file is an input, read whole.
    read_directories: Vec<PathBuf>,
    /// Input files one by one, the lockfile, and symlinked inputs' targets.
    read_files: Vec<PathBuf>,
    /// Its dependencies' outputs.
    read_trees: BTreeSet<PathBuf>,
    /// Its outputs and warm paths.
    write_trees: BTreeSet<PathBuf>,
}

/// A run's sandbox: what each task may do, its profiles or rules, and on
/// macOS the log being read.
pub struct Sandbox {
    pub mode: Mode,
    root: PathBuf,
    state: PathBuf,
    plans: BTreeMap<String, Plan>,
    profiles: BTreeMap<String, PathBuf>,
    /// Tasks that run unsandboxed, and why.
    unsandboxed: Mutex<BTreeMap<String, String>>,
    watcher: Option<Watcher>,
    directory: PathBuf,
    /// Linux: every `node_modules` directory and dotenv file of the
    /// workspace, since Landlock takes paths rather than patterns.
    shared: Vec<PathBuf>,
    dotenv: Vec<PathBuf>,
    /// Linux: rule sets handed to tasks, open until the run ends, and the
    /// output directories created so their rules had a path to hold.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    rulesets: Mutex<Vec<Ruleset>>,
    created: Mutex<Vec<PathBuf>>,
}

/// Tag in each rule's message: the task a report belongs to.
const TAG: &str = "qk-task:";
const SENTINEL: &str = "qk-sandbox-sentinel";

impl Sandbox {
    /// Works out what each task may do; on macOS writes its profile and
    /// starts reading the log.
    pub fn start(workspace: &Workspace, graph: &TaskGraph, mode: Mode) -> Result<Self> {
        if cfg!(target_os = "linux") {
            if mode == Mode::Audit {
                bail!("on Linux the sandbox can only refuse, not report; use --sandbox=enforce");
            }
            landlock::abi().context("--sandbox needs Landlock, in Linux 5.13 and later")?;
        } else if !cfg!(target_os = "macos") {
            bail!("--sandbox needs macOS or Linux");
        }
        let root = workspace
            .root
            .canonicalize()
            .context("cannot resolve the workspace root")?;
        let state = qk_cache::worktree_state(&workspace.root);
        let directory = state.join("sandbox").join(std::process::id().to_string());
        std::fs::create_dir_all(&directory)?;
        let resolution = qk_cache::resolve_each(workspace, graph)?;
        let tree = Tree::new(&resolution.candidates);
        let mut plans = BTreeMap::new();
        let mut unsandboxed = BTreeMap::new();
        for (id, task) in &graph.tasks {
            match &resolution.tasks[id] {
                Ok(resolved) => {
                    let mut files = resolved.files.clone();
                    files.extend(
                        resolved
                            .values
                            .keys()
                            .filter_map(|key| key.strip_prefix("json:").map(str::to_owned)),
                    );
                    plans.insert(
                        id.clone(),
                        plan(workspace, graph, task, &root, &tree, &files),
                    );
                }
                Err(reason) => {
                    unsandboxed.insert(id.clone(), reason.clone());
                }
            }
        }
        let mut sandbox = Self {
            mode,
            root,
            state,
            plans,
            profiles: BTreeMap::new(),
            unsandboxed: Mutex::new(unsandboxed),
            watcher: None,
            directory,
            shared: Vec::new(),
            dotenv: Vec::new(),
            rulesets: Mutex::new(Vec::new()),
            created: Mutex::new(Vec::new()),
        };
        if cfg!(target_os = "macos") {
            for (index, id) in sandbox.plans.keys().enumerate() {
                let profile = profile(&sandbox.plans[id], id, &sandbox.root, &sandbox.state, mode)?;
                let path = sandbox.directory.join(format!("{index}.sb"));
                std::fs::write(&path, profile)?;
                // A profile Seatbelt cannot take would fail the task; it runs
                // unsandboxed instead, with the reason.
                match compiles(&path) {
                    Ok(()) => {
                        sandbox.profiles.insert(id.clone(), path);
                    }
                    Err(reason) => {
                        sandbox
                            .unsandboxed
                            .lock()
                            .unwrap()
                            .insert(id.clone(), reason);
                    }
                }
            }
            // Reports only name what enforce mode refuses, so it runs without
            // them where the log keeps them out; audit has nothing without.
            match Watcher::start(&sandbox.directory) {
                Ok(watcher) => sandbox.watcher = Some(watcher),
                Err(error) if mode == Mode::Enforce => {
                    qk_executor::status!("qk: refused paths will not be listed: {error:#}")
                }
                Err(error) => return Err(error),
            }
        } else {
            let (shared, dotenv) = scan(&sandbox.root);
            sandbox.shared = shared;
            sandbox.dotenv = dotenv;
        }
        Ok(sandbox)
    }

    /// How a task is held to its plan, as it starts; `None` runs it
    /// unsandboxed, with the reason recorded.
    pub fn confine(&self, id: &str) -> Option<qk_executor::Confinement> {
        if !self.plans.contains_key(id) {
            return None;
        }
        if cfg!(target_os = "macos") {
            return self
                .profiles
                .get(id)
                .map(|profile| qk_executor::Confinement::Seatbelt(profile.clone()));
        }
        match self.landlock(id) {
            Ok(confinement) => Some(confinement),
            Err(error) => {
                self.unsandboxed
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), format!("{error:#}"));
                None
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn landlock(&self, id: &str) -> Result<qk_executor::Confinement> {
        use std::os::fd::AsRawFd;
        let plan = &self.plans[id];
        // Landlock holds paths that exist: an output directory that does not
        // yet is created, and removed again if the task leaves it empty.
        for tree in &plan.write_trees {
            if !tree.exists() && tree.extension().is_none() {
                std::fs::create_dir_all(tree)?;
                self.created.lock().unwrap().push(tree.clone());
            }
        }
        let rules = landlock::rules(&landlock::Paths {
            root: &self.root,
            read_directories: &plan.read_directories,
            read_files: plan.read_files.iter().chain(&self.dotenv),
            read_trees: &plan.read_trees,
            write_trees: plan
                .write_trees
                .iter()
                .chain(&self.shared)
                .chain([&self.root.join(".git"), &self.state]),
        })?;
        let fd = rules.as_raw_fd();
        self.rulesets.lock().unwrap().push(rules);
        Ok(qk_executor::Confinement::Landlock(fd))
    }

    #[cfg(not(target_os = "linux"))]
    fn landlock(&self, _: &str) -> Result<qk_executor::Confinement> {
        bail!("Landlock is Linux's")
    }

    /// Stops reading the log, once every report so far has arrived, and
    /// returns each task's findings, and the tasks that ran unsandboxed.
    pub fn finish(mut self) -> Result<(BTreeMap<String, Findings>, BTreeMap<String, String>)> {
        let reports = match self.watcher.take() {
            Some(watcher) => {
                let (reports, dropped) = watcher.finish(&self.directory)?;
                if dropped > 0 {
                    bail!("sandbox log exceeded 100000 reports; {dropped} reports omitted");
                }
                reports
            }
            None => Vec::new(),
        };
        let _ = std::fs::remove_dir_all(&self.directory);
        for directory in std::mem::take(&mut *self.created.lock().unwrap()) {
            // Only if the task left it empty.
            let _ = std::fs::remove_dir(directory);
        }
        let mut findings: BTreeMap<String, Findings> = BTreeMap::new();
        for report in reports {
            let Ok(relative) = Path::new(&report.path).strip_prefix(&self.root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let entry = findings.entry(report.task).or_default();
            if report.operation.starts_with("file-write") {
                entry.stray_writes.insert(relative);
            } else if unkeyed(&relative) {
                entry.unkeyed_reads.insert(relative);
            } else {
                entry.undeclared_reads.insert(relative);
            }
        }
        findings.retain(|_, findings| !findings.is_empty());
        let unsandboxed = std::mem::take(&mut *self.unsandboxed.lock().unwrap());
        Ok((findings, unsandboxed))
    }
}

/// Every `node_modules` directory and dotenv file in the workspace, without
/// looking inside `node_modules` or `.git`.
fn scan(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut shared = Vec::new();
    let mut dotenv = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                match name.as_ref() {
                    "node_modules" => shared.push(entry.path()),
                    ".git" => {}
                    _ => pending.push(entry.path()),
                }
            } else if unkeyed(&name) {
                dotenv.push(entry.path());
            }
        }
    }
    (shared, dotenv)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.child.kill();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// `.git` and dotenv files, which tasks read outside their keys by design.
fn unkeyed(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path == ".git"
        || path.starts_with(".git/")
        || name == ".env"
        || name.starts_with(".env.")
        || (name.starts_with('.') && name.ends_with(".env"))
}

// Rules name `file-read-data` rather than `file-read*`: Seatbelt lets a rule
// for a specific operation win over one for a wildcard, whatever their order,
// so a wildcard allow would not override the reporting rule.

/// A string in a profile.
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A regular expression matching `text` literally.
fn literal_regex(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if "\\.+*?()|[]{}^$".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Every task `task` depends on, directly or not.
fn dependencies<'a>(graph: &'a TaskGraph, task: &'a Task) -> Vec<&'a Task> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<&String> = task.dependencies.iter().collect();
    let mut found = Vec::new();
    while let Some(id) = pending.pop() {
        if seen.insert(id) {
            let dependency = &graph.tasks[id];
            pending.extend(dependency.dependencies.iter());
            found.push(dependency);
        }
    }
    found
}

/// Whether Seatbelt accepts a profile, or its complaint.
fn compiles(profile: &Path) -> std::result::Result<(), String> {
    let output = Command::new("sandbox-exec")
        .arg("-f")
        .arg(profile)
        .arg("/usr/bin/true")
        .output()
        .map_err(|error| format!("cannot run sandbox-exec: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("sandbox-exec rejected the profile")
            .trim_start_matches("sandbox-exec: ")
            .to_owned())
    }
}

/// The workspace's files as directories, for allowing a directory whole
/// when all of its files are inputs.
struct Tree {
    /// Files under each directory, the root being `""`.
    counts: BTreeMap<String, usize>,
}

impl Tree {
    fn new(files: &BTreeSet<String>) -> Self {
        let mut counts = BTreeMap::new();
        for file in files {
            for directory in parents(file) {
                *counts.entry(directory.to_owned()).or_insert(0) += 1;
            }
        }
        Self { counts }
    }

    /// The inputs as the fewest paths: each outermost directory whose every
    /// file is an input, and the other inputs one by one.
    fn cover<'a>(&self, inputs: &'a BTreeSet<String>) -> (Vec<String>, Vec<&'a str>) {
        let mut covered: BTreeMap<&str, usize> = BTreeMap::new();
        for input in inputs {
            for directory in parents(input) {
                *covered.entry(directory).or_insert(0) += 1;
            }
        }
        let whole: BTreeSet<&str> = covered
            .iter()
            .filter(|(directory, count)| {
                !directory.is_empty() && self.counts.get(**directory) == Some(count)
            })
            .map(|(directory, _)| *directory)
            .collect();
        // Only the outermost of those.
        let outermost: Vec<String> = whole
            .iter()
            .filter(|directory| !parents(directory).any(|parent| whole.contains(parent)))
            .map(|directory| (*directory).to_owned())
            .collect();
        let singles = inputs
            .iter()
            .map(String::as_str)
            .filter(|input| !parents(input).any(|directory| whole.contains(directory)))
            .collect();
        (outermost, singles)
    }
}

/// The directories above a path, innermost first, ending with the root `""`.
fn parents(path: &str) -> impl Iterator<Item = &str> {
    let mut current = Some(path);
    std::iter::from_fn(move || {
        let path = current?;
        let parent = match path.rsplit_once('/') {
            Some((parent, _)) => parent,
            None if path.is_empty() => return None,
            None => "",
        };
        current = Some(parent);
        Some(parent)
    })
}

/// What a task may do: its inputs, collapsed into whole directories where
/// every tracked file is one, its dependencies' outputs, and its own outputs
/// and warm paths.
fn plan(
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    root: &Path,
    tree: &Tree,
    files: &BTreeSet<String>,
) -> Plan {
    let path = |relative: &str| -> PathBuf {
        if relative == "." || relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(relative)
        }
    };
    let (whole, singles) = tree.cover(files);
    let mut read_files: Vec<PathBuf> = singles.iter().map(|file| path(file)).collect();
    // Package managers and Node read these in every task. What they install is
    // keyed through the lockfile rather than as files.
    for file in ["pnpm-lock.yaml", "pnpm-workspace.yaml", "package.json"] {
        read_files.push(path(file));
    }
    let mut read_trees = BTreeSet::new();
    for file in files {
        for target in linked_targets(root, &root.join(file)) {
            if target.is_dir() {
                read_trees.insert(target);
            } else {
                read_files.push(target);
            }
        }
    }
    let anchors = |task: &Task| -> Vec<PathBuf> {
        qk_cache::Outputs::new(workspace, task)
            .map(|outputs| outputs.anchors().map(path).collect())
            .unwrap_or_default()
    };
    for dependency in dependencies(graph, task) {
        read_trees.extend(anchors(dependency));
    }
    let mut write_trees: BTreeSet<PathBuf> = anchors(task).into_iter().collect();
    if let Ok(Some(warm)) = qk_cache::warm::config(workspace, task)
        && let Ok(paths) = qk_cache::Outputs::from_paths(&warm.kept_paths())
    {
        write_trees.extend(paths.anchors().map(path));
    }
    Plan {
        read_directories: whole.iter().map(|directory| path(directory)).collect(),
        read_files,
        read_trees,
        write_trees,
    }
}

/// Linked directory inputs key the whole target tree, including nested links.
pub(crate) fn linked_targets(root: &Path, link: &Path) -> BTreeSet<PathBuf> {
    if !std::fs::symlink_metadata(link).is_ok_and(|metadata| metadata.is_symlink()) {
        return BTreeSet::new();
    }
    let mut pending = vec![link.to_path_buf()];
    let mut visited = BTreeSet::new();
    while let Some(link) = pending.pop() {
        let Ok(target) = link.canonicalize() else {
            continue;
        };
        if !target.starts_with(root) || !visited.insert(target.clone()) {
            continue;
        }
        if target.is_dir() {
            pending.extend(
                walkdir::WalkDir::new(&target)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_symlink())
                    .map(|entry| entry.into_path()),
            );
        }
    }
    visited
}

/// The Seatbelt profile for a task's plan.
fn profile(plan: &Plan, id: &str, root: &Path, state: &Path, mode: Mode) -> Result<String> {
    let text = |path: &Path| -> Result<String> {
        path.to_str()
            .map(quote)
            .context("sandboxed paths must be UTF-8")
    };
    let root_text = root.to_str().context("the workspace path must be UTF-8")?;
    let tag = quote(&format!("{TAG}{id}"));
    let mut rules = vec!["(version 1)".to_owned(), "(allow default)".to_owned()];
    match mode {
        Mode::Audit => {
            rules.push(format!(
                "(allow file-read-data (subpath {}) (with report) (with message {tag}))",
                quote(root_text)
            ));
            rules.push(format!(
                "(allow file-write* (subpath {}) (with report) (with message {tag}))",
                quote(root_text)
            ));
        }
        Mode::Enforce => {
            rules.push(format!(
                "(deny file-read-data (subpath {}) (with message {tag}))",
                quote(root_text)
            ));
            rules.push(format!(
                "(deny file-write* (subpath {}) (with message {tag}))",
                quote(root_text)
            ));
            // Not keyed, but a task may need them.
            rules.push(format!(
                "(allow file-read-data file-write* (subpath {}))",
                text(&root.join(".git"))?
            ));
            rules.push(format!(
                "(allow file-read-data (regex #\"^{}/(.*/)?\\.(env|env\\.[^/]*|[^/]*\\.env)$\"))",
                literal_regex(root_text)
            ));
        }
    }
    // Directories may be listed; what a listing finds is not read.
    rules.push("(allow file-read-data (vnode-type DIRECTORY))".to_owned());
    rules.push(format!(
        "(allow file-read-data file-write* (regex #\"^{}/(.*/)?node_modules/\"))",
        literal_regex(root_text)
    ));
    // qk's own state for the worktree, where warm directories live.
    rules.push(format!(
        "(allow file-read-data file-write* (subpath {}))",
        text(state)?
    ));
    for directory in &plan.read_directories {
        rules.push(format!(
            "(allow file-read-data (subpath {}))",
            text(directory)?
        ));
    }
    // Seatbelt limits each rule's data to 64 KiB, so the paths are spread
    // over rules of a few hundred each.
    for chunk in plan.read_files.chunks(200) {
        rules.push("(allow file-read-data".to_owned());
        for file in chunk {
            rules.push(format!("  (literal {})", text(file)?));
        }
        rules.push(")".to_owned());
    }
    for tree in &plan.read_trees {
        rules.push(format!("(allow file-read-data (subpath {}))", text(tree)?));
    }
    for tree in &plan.write_trees {
        rules.push(format!(
            "(allow file-read-data file-write* (subpath {}))",
            text(tree)?
        ));
    }
    Ok(rules.join("\n") + "\n")
}

/// A report from the kernel's log.
pub(crate) struct Report {
    pub task: String,
    pub process: u32,
    pub operation: String,
    pub path: String,
}

/// `log stream`, reading the sandbox's reports while the run lasts.
pub(crate) struct Watcher {
    child: Child,
    reports: Arc<Mutex<Vec<Report>>>,
    sentinel: Arc<Mutex<bool>>,
    marker: String,
    dropped: Arc<std::sync::atomic::AtomicUsize>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub(crate) fn start(directory: &Path) -> Result<Self> {
        let mut child = Command::new("log")
            .args([
                "stream",
                "--level",
                "debug",
                "--style",
                "ndjson",
                "--predicate",
                "sender == \"Sandbox\"",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot read the system log for the sandbox's reports")?;
        let stdout = child.stdout.take().context("log stream has no output")?;
        let reports = Arc::new(Mutex::new(Vec::new()));
        let sentinel = Arc::new(Mutex::new(false));
        let marker = format!("{SENTINEL}:{}", directory.canonicalize()?.display());
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ready, started) = std::sync::mpsc::channel();
        let reader = {
            let reports = reports.clone();
            let sentinel = sentinel.clone();
            let marker = marker.clone();
            let dropped = dropped.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let _ = ready.send(());
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    let Some(message) = event["eventMessage"].as_str() else {
                        continue;
                    };
                    if message.contains(&marker) {
                        *sentinel.lock().unwrap() = true;
                        continue;
                    }
                    if let Some(report) = parse(message) {
                        let mut reports = reports.lock().unwrap();
                        if reports.len() < 100_000 {
                            reports.push(report);
                        } else {
                            dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            })
        };
        let watcher = Self {
            child,
            reports,
            sentinel,
            marker,
            dropped,
            reader: Some(reader),
        };
        // The stream's first line says it has started filtering.
        if started.recv_timeout(Duration::from_secs(10)).is_err() {
            bail!("the system log did not start streaming the sandbox's reports");
        }
        watcher.probe(directory, "start")?;
        Ok(watcher)
    }

    /// Makes the sandbox report a read of a marker file, so the reader knows
    /// every report before it has arrived.
    fn probe(&self, directory: &Path, name: &str) -> Result<()> {
        let marker = directory.join(format!("{name}.marker"));
        std::fs::write(&marker, "")?;
        let marker = marker.canonicalize()?;
        let profile = directory.join(format!("{name}.sb"));
        std::fs::write(
            &profile,
            format!(
                "(version 1)\n(allow default)\n(allow file-read-data (literal {}) (with report) (with message {}))\n",
                quote(marker.to_str().context("marker path must be UTF-8")?),
                quote(&self.marker)
            ),
        )?;
        *self.sentinel.lock().unwrap() = false;
        Command::new("sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .arg("/bin/cat")
            .arg(&marker)
            .stdout(Stdio::null())
            .status()
            .context("cannot run sandbox-exec")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*self.sentinel.lock().unwrap() {
            if Instant::now() > deadline {
                bail!("the sandbox's reports did not reach the system log");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    pub(crate) fn finish(mut self, directory: &Path) -> Result<(Vec<Report>, usize)> {
        let probed = self.probe(directory, "finish");
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        probed?;
        Ok((
            std::mem::take(&mut *self.reports.lock().unwrap()),
            self.dropped.load(std::sync::atomic::Ordering::Relaxed),
        ))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// `Sandbox: cat(51583) allow file-read-data /path\nqk-task:web:build`, or
/// with `deny(1)`.
fn parse(message: &str) -> Option<Report> {
    let (line, task) = message.split_once(&format!("\n{TAG}"))?;
    // The system log may prefix a coalesced event with a duplicate count.
    let rest = line.split_once("Sandbox: ")?.1;
    let (process, rest) = rest.split_once(") ")?;
    let process = process.rsplit_once('(')?.1.parse().ok()?;
    let (_, rest) = rest.split_once(' ')?;
    let (operation, path) = rest.split_once(' ')?;
    Some(Report {
        task: task.trim().to_owned(),
        process,
        operation: operation.to_owned(),
        path: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_reports() {
        let report =
            parse("Sandbox: cat(51583) allow file-read-data /repo/src/a b.ts\nqk-task:web:build")
                .unwrap();
        assert_eq!(
            (
                report.task.as_str(),
                report.operation.as_str(),
                report.path.as_str()
            ),
            ("web:build", "file-read-data", "/repo/src/a b.ts")
        );
        let denied =
            parse("Sandbox: node(12) deny(1) file-write-create /repo/stray\nqk-task:app:x")
                .unwrap();
        assert_eq!(denied.operation, "file-write-create");
        assert!(parse("Sandbox: tccd(80131) deny(1) system-info vfs.disk-space").is_none());
    }

    #[test]
    fn covers_whole_directories_with_one_rule() {
        assert_eq!(parents("a/b/c.ts").collect::<Vec<_>>(), ["a/b", "a", ""]);
        assert_eq!(parents("c.ts").collect::<Vec<_>>(), [""]);
        let files: BTreeSet<String> = [
            "app/src/a.ts",
            "app/src/deep/b.ts",
            "app/src/c.test.ts",
            "app/lib/d.ts",
            "app/lib/e.ts",
            "README.md",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let tree = Tree::new(&files);
        let inputs: BTreeSet<String> = [
            "app/src/a.ts",
            "app/src/deep/b.ts",
            "app/lib/d.ts",
            "app/lib/e.ts",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let (whole, singles) = tree.cover(&inputs);
        assert_eq!(whole, ["app/lib", "app/src/deep"]);
        assert_eq!(singles, ["app/src/a.ts"]);
    }

    #[cfg(unix)]
    #[test]
    fn linked_targets_follow_nested_links_without_cycles_or_external_paths() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for directory in ["shared", "nested"] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("file.txt"), "input").unwrap();
        for (target, alias) in [
            (PathBuf::from("shared"), "selected"),
            (PathBuf::from("../file.txt"), "shared/file.txt"),
            (PathBuf::from("../nested"), "shared/nested"),
            (PathBuf::from("../shared"), "shared/cycle"),
            (external.path().to_path_buf(), "shared/external"),
        ] {
            std::os::unix::fs::symlink(target, root.join(alias)).unwrap();
        }
        assert_eq!(
            linked_targets(&root, &root.join("selected")),
            [
                root.join("shared"),
                root.join("nested"),
                root.join("file.txt")
            ]
            .into_iter()
            .collect()
        );
        assert!(linked_targets(&root, &root.join("file.txt")).is_empty());
    }

    #[test]
    fn tells_unkeyed_files() {
        for path in [
            ".git",
            ".git/HEAD",
            ".env",
            "apps/web/.env.local",
            "apps/web/.build.env",
        ] {
            assert!(unkeyed(path), "{path}");
        }
        assert!(!unkeyed("src/env.ts") && !unkeyed(".github/workflows/ci.yml"));
    }
}
