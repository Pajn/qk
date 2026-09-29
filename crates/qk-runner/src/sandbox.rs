//! `--sandbox`: each task runs under macOS's sandbox (Seatbelt, through
//! `sandbox-exec`), which holds it to what it declares inside the workspace.
//! Everything outside the workspace stays open.
//!
//! Inside it, a task may read its cache key's files, `node_modules`, the
//! lockfile, the outputs of its own and its dependencies' tasks and its
//! warm paths, and list any directory; it may write its outputs and warm
//! paths. In audit mode anything else is allowed and reported; in enforce
//! mode it is refused, except reading and writing `.git` and reading dotenv
//! files, which a task may need although they are not keyed.
//!
//! Each report names the task through the rule's message, so the kernel's
//! log, read while the run lasts, says which task did what.

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

/// A run's sandbox: a profile per task, and the log being read.
pub struct Sandbox {
    pub mode: Mode,
    root: PathBuf,
    profiles: BTreeMap<String, PathBuf>,
    /// Tasks that run unsandboxed, and why.
    pub unsandboxed: BTreeMap<String, String>,
    watcher: Option<Watcher>,
    directory: PathBuf,
}

/// Tag in each rule's message: the task a report belongs to.
const TAG: &str = "qk-task:";
const SENTINEL: &str = "qk-sandbox-sentinel";

impl Sandbox {
    /// Writes a profile for each task and starts reading the log.
    pub fn start(workspace: &Workspace, graph: &TaskGraph, mode: Mode) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            bail!("--sandbox needs macOS's sandbox-exec");
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
        let mut profiles = BTreeMap::new();
        let mut unsandboxed = BTreeMap::new();
        for (index, (id, task)) in graph.tasks.iter().enumerate() {
            let files = match &resolution.tasks[id] {
                Ok(resolved) => &resolved.files,
                Err(reason) => {
                    unsandboxed.insert(id.clone(), reason.clone());
                    continue;
                }
            };
            let profile = profile(workspace, graph, task, &root, &state, &tree, files, mode)?;
            let path = directory.join(format!("{index}.sb"));
            std::fs::write(&path, profile)?;
            // A profile Seatbelt cannot take would fail the task; it runs
            // unsandboxed instead, with the reason.
            match compiles(&path) {
                Ok(()) => {
                    profiles.insert(id.clone(), path);
                }
                Err(reason) => {
                    unsandboxed.insert(id.clone(), reason);
                }
            }
        }
        let watcher = Watcher::start(&directory)?;
        Ok(Self {
            mode,
            root,
            profiles,
            unsandboxed,
            watcher: Some(watcher),
            directory,
        })
    }

    /// The profile a task runs under, if it is sandboxed.
    pub fn profile(&self, id: &str) -> Option<&Path> {
        self.profiles.get(id).map(PathBuf::as_path)
    }

    /// Stops reading the log, once every report so far has arrived, and
    /// returns each task's findings.
    pub fn finish(mut self) -> Result<BTreeMap<String, Findings>> {
        let reports = match self.watcher.take() {
            Some(watcher) => watcher.finish(&self.directory)?,
            None => Vec::new(),
        };
        let _ = std::fs::remove_dir_all(&self.directory);
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
        Ok(findings)
    }
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

#[allow(clippy::too_many_arguments)]
fn profile(
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    root: &Path,
    state: &Path,
    tree: &Tree,
    files: &BTreeSet<String>,
    mode: Mode,
) -> Result<String> {
    let root_text = root.to_str().context("the workspace path must be UTF-8")?;
    let path = |relative: &str| -> String {
        if relative == "." || relative.is_empty() {
            root_text.to_owned()
        } else {
            format!("{root_text}/{relative}")
        }
    };
    let tag = quote(&format!("{TAG}{}", task.id));
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
                quote(&path(".git"))
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
        quote(
            state
                .to_str()
                .context("the git directory path must be UTF-8")?
        )
    ));
    let (whole, singles) = tree.cover(files);
    for directory in &whole {
        rules.push(format!(
            "(allow file-read-data (subpath {}))",
            quote(&path(directory))
        ));
    }
    let mut readable: Vec<String> = singles.iter().map(|file| path(file)).collect();
    // Keyed through what the lockfile installs rather than as files.
    for file in ["pnpm-lock.yaml", "pnpm-workspace.yaml", "package.json"] {
        readable.push(path(file));
    }
    // A symlinked input is read where it points.
    for file in files {
        let absolute = root.join(file);
        if std::fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_symlink())
            && let Ok(target) = absolute.canonicalize()
        {
            readable.push(target.to_string_lossy().into_owned());
        }
    }
    // Seatbelt limits each rule's data to 64 KiB, so the paths are spread
    // over rules of a few hundred each.
    for chunk in readable.chunks(200) {
        rules.push("(allow file-read-data".to_owned());
        for file in chunk {
            rules.push(format!("  (literal {})", quote(file)));
        }
        rules.push(")".to_owned());
    }
    let anchors = |task: &Task| -> Vec<String> {
        qk_cache::Outputs::new(workspace, task)
            .map(|outputs| outputs.anchors().map(path).collect())
            .unwrap_or_default()
    };
    let mut read_trees: BTreeSet<String> = BTreeSet::new();
    for dependency in dependencies(graph, task) {
        read_trees.extend(anchors(dependency));
    }
    let mut write_trees: BTreeSet<String> = anchors(task).into_iter().collect();
    if let Ok(Some(warm)) = qk_cache::warm::config(workspace, task)
        && let Ok(paths) = qk_cache::Outputs::from_paths(&warm.paths)
    {
        write_trees.extend(paths.anchors().map(path));
    }
    for tree in &read_trees {
        rules.push(format!("(allow file-read-data (subpath {}))", quote(tree)));
    }
    for tree in &write_trees {
        rules.push(format!(
            "(allow file-read-data file-write* (subpath {}))",
            quote(tree)
        ));
    }
    Ok(rules.join("\n") + "\n")
}

/// A report from the kernel's log.
struct Report {
    task: String,
    operation: String,
    path: String,
}

/// `log stream`, reading the sandbox's reports while the run lasts.
struct Watcher {
    child: Child,
    reports: Arc<Mutex<Vec<Report>>>,
    sentinel: Arc<Mutex<bool>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn start(directory: &Path) -> Result<Self> {
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
        let (ready, started) = std::sync::mpsc::channel();
        let reader = {
            let reports = reports.clone();
            let sentinel = sentinel.clone();
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
                    if message.contains(SENTINEL) {
                        *sentinel.lock().unwrap() = true;
                        continue;
                    }
                    if let Some(report) = parse(message) {
                        reports.lock().unwrap().push(report);
                    }
                }
            })
        };
        let watcher = Self {
            child,
            reports,
            sentinel,
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
                quote(SENTINEL)
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

    fn finish(mut self, directory: &Path) -> Result<Vec<Report>> {
        let probed = self.probe(directory, "finish");
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        probed?;
        Ok(std::mem::take(&mut *self.reports.lock().unwrap()))
    }
}

/// `Sandbox: cat(51583) allow file-read-data /path\nqk-task:web:build`, or
/// with `deny(1)`.
fn parse(message: &str) -> Option<Report> {
    let (line, task) = message.split_once(&format!("\n{TAG}"))?;
    let rest = line.strip_prefix("Sandbox: ")?;
    let (_, rest) = rest.split_once(") ")?;
    let (_, rest) = rest.split_once(' ')?;
    let (operation, path) = rest.split_once(' ')?;
    Some(Report {
        task: task.trim().to_owned(),
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
