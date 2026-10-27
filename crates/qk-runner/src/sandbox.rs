//! `--sandbox`: each task runs under the system's sandbox, which holds it to
//! what it declares inside the workspace. Everything outside the workspace
//! stays open.
//!
//! Inside it, a task may read the files its cache key holds whole or by the
//! fields its `json` inputs select, `node_modules`, the outputs of its own
//! and its dependencies' tasks and its warm paths, and list any directory;
//! it may write its outputs and warm paths. A file its key holds by what it
//! changes or resolves to, such as nx.json, project.json or the pnpm
//! lockfile, is read only as declared, since what the task reads of it
//! could change without its key. In audit mode anything else is allowed and reported; in enforce
//! mode it is refused, except reading and writing `.git` and reading dotenv
//! files, which a task may need although they are not keyed.
//!
//! Audit also reports reading an installed package the key leaves out, as a
//! task naming its `externalDependencies` but running a tool only the root
//! installs does. Enforce allows it: Landlock grants a directory with all
//! beneath it, so a store directory could not be kept from a task while
//! `node_modules` stays open for what tools create there as they run, such
//! as `node_modules/.cache`, and enforce would refuse on macOS what it lets
//! through on Linux.
//!
//! On macOS that is Seatbelt, through `sandbox-exec`, with a profile per
//! task. Each report names the task through the rule's message, so the
//! kernel's log, read while the run lasts, says which task did what. On
//! Linux enforce mode is Landlock, which can only refuse; a task's rules are
//! built as it starts, since Landlock holds paths that exist and its
//! dependencies' outputs appear during the run. Audit mode records each task
//! instead, with the recorder `--record` uses, and holds what it did against
//! the plan a profile would have allowed without a report.
//!
//! With `--record`, audit mode holds the recordings the cache makes against
//! those plans instead, and the cache stays on: only the tasks that run and
//! are recorded are audited, at no cost beyond reading their traces again.

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

use crate::installs::Installs;

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
    /// Workspace files it read that are not in its cache key, but for those
    /// grouped below.
    pub undeclared_reads: BTreeSet<String>,
    /// Workspace packages' `package.json` read outside its key, as package
    /// managers and bundlers do to resolve workspace packages.
    pub manifest_reads: BTreeSet<String>,
    /// Version and package managers' configuration read outside its key.
    pub toolchain_reads: BTreeSet<String>,
    /// TypeScript configuration read outside its key: likely inputs.
    pub tsconfig_reads: BTreeSet<String>,
    /// Files read outside its key in projects its project depends on, by
    /// project: what running a dependency's code or following imports into
    /// it reads. `^` inputs key them.
    pub dependency_reads: BTreeMap<String, BTreeSet<String>>,
    /// `.git` and dotenv files it read: allowed, but not in the key either.
    pub unkeyed_reads: BTreeSet<String>,
    /// Paths it wrote in the workspace outside its outputs.
    pub stray_writes: BTreeSet<String>,
    /// Installed packages it read that its key leaves out, by name.
    pub unkeyed_packages: BTreeSet<String>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.undeclared_reads.is_empty()
            && self.manifest_reads.is_empty()
            && self.toolchain_reads.is_empty()
            && self.tsconfig_reads.is_empty()
            && self.dependency_reads.is_empty()
            && self.unkeyed_reads.is_empty()
            && self.stray_writes.is_empty()
            && self.unkeyed_packages.is_empty()
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
    /// Audit: the pnpm store directories whose manifests' reads are
    /// reported, as the shorter list Seatbelt takes.
    packages: Option<Packages>,
}

impl Plan {
    /// Whether `path` may be read, or with `write` written, without a report:
    /// what the audit profile allows outright.
    fn allows(&self, path: &Path, write: bool, state: &Path) -> bool {
        let under = |tree: &PathBuf| path.starts_with(tree);
        path.starts_with(state)
            || self.write_trees.iter().any(under)
            || (!write
                && (self.read_directories.iter().any(under)
                    || self.read_trees.iter().any(under)
                    || self.read_files.iter().any(|file| file == path)))
    }
}

/// The store directories whose manifests' reads are reported.
#[derive(Clone, Debug)]
enum Packages {
    /// Those listed, holding only installations the key leaves out.
    Unkeyed(Vec<PathBuf>),
    /// Every one but those listed, which hold an installation it counts.
    AllBut(Vec<PathBuf>),
}

impl Packages {
    /// The store's directories as the shorter list, from those holding an
    /// installation the key counts and those holding only others.
    fn shorter(keyed: Vec<PathBuf>, unkeyed: Vec<PathBuf>) -> Self {
        if keyed.len() < unkeyed.len() {
            Self::AllBut(keyed)
        } else {
            Self::Unkeyed(unkeyed)
        }
    }
}

/// A run's sandbox: what each task may do, its profiles or rules, and on
/// macOS the log being read.
pub struct Sandbox {
    pub mode: Mode,
    root: PathBuf,
    state: PathBuf,
    plans: BTreeMap<String, Plan>,
    profiles: BTreeMap<String, PathBuf>,
    /// Audit: the installed packages, and the installations each task's key
    /// covers, for telling which package a reported read belongs to.
    installs: Option<Installs>,
    covered: BTreeMap<String, BTreeSet<String>>,
    /// The projects each task's project depends on, directly or not, by name
    /// with their roots, innermost first.
    depended: BTreeMap<String, Vec<(String, String)>>,
    /// Tasks that run unsandboxed, and why.
    unsandboxed: Mutex<BTreeMap<String, String>>,
    /// Audit: tasks sandboxed without reporting installed packages, and why.
    packages_unchecked: BTreeMap<String, String>,
    watcher: Option<Watcher>,
    directory: PathBuf,
    /// Linux: every `node_modules` directory and dotenv file of the
    /// workspace, since Landlock takes paths rather than patterns.
    shared: Vec<PathBuf>,
    dotenv: Vec<PathBuf>,
    /// Linux audit: the recorder, and each task's recording with the
    /// directory its commands start in.
    recording: Option<qk_executor::Recording>,
    traces: Mutex<BTreeMap<String, (qk_executor::Recording, PathBuf)>>,
    /// The workspace root as given, which recorded paths may name rather
    /// than `root`, its links resolved.
    given_root: PathBuf,
    /// Linux audit under `--record`: tasks are audited from the cache's
    /// recordings, handed over as they finish, rather than recorded here.
    from_recordings: bool,
    /// Linux: rule sets handed to tasks, open until the run ends, and the
    /// output directories created so their rules had a path to hold.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    rulesets: Mutex<Vec<Ruleset>>,
    created: Mutex<Vec<PathBuf>>,
}

/// What a sandboxed run found.
pub struct Finished {
    pub findings: BTreeMap<String, Findings>,
    /// Tasks that ran unsandboxed, and why.
    pub unsandboxed: BTreeMap<String, String>,
    /// Tasks sandboxed without reporting installed packages, and why.
    pub packages_unchecked: BTreeMap<String, String>,
    /// Audit: the tasks held to their plans, whether or not they went
    /// beyond them.
    pub audited: BTreeSet<String>,
}

/// Tag in each rule's message: the task a report belongs to.
const TAG: &str = "qk-task:";
const SENTINEL: &str = "qk-sandbox-sentinel";
/// How long a probe's report may take before the probe is made again, and
/// before the system log is taken not to carry the sandbox's reports.
const PROBE_RETRY: Duration = Duration::from_millis(500);
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

impl Sandbox {
    /// Works out what each task may do; on macOS writes its profile and
    /// starts reading the log. With `from_recordings`, an audit on Linux
    /// records nothing itself and audits what [`Self::audit_recorded`] hands
    /// it.
    pub fn start(
        workspace: &Workspace,
        graph: &TaskGraph,
        mode: Mode,
        from_recordings: bool,
    ) -> Result<Self> {
        if from_recordings && !(cfg!(target_os = "linux") && mode == Mode::Audit) {
            bail!("only --sandbox=audit on Linux audits what --record records");
        }
        if cfg!(target_os = "linux") {
            if mode == Mode::Enforce {
                landlock::abi()
                    .context("--sandbox=enforce needs Landlock, in Linux 5.13 and later")?;
            }
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
        let tree = Tree::new(&resolution.candidates).protecting(protected(workspace));
        let installs = (mode == Mode::Audit)
            .then(|| Installs::load(&root))
            .flatten();
        let depended = depended(workspace, graph)?;
        let mut plans = BTreeMap::new();
        let mut covered = BTreeMap::new();
        let mut unsandboxed = BTreeMap::new();
        for (id, task) in &graph.tasks {
            if task.definition.extra.get("qk:sandbox") == Some(&serde_json::Value::Bool(false)) {
                unsandboxed.insert(id.clone(), "its target sets \"qk:sandbox\": false".into());
                continue;
            }
            match &resolution.tasks[id] {
                Ok(resolved) => {
                    let files = resolved.file_inputs().map(str::to_owned).collect();
                    let mut plan = plan(workspace, graph, task, &root, &tree, &files);
                    if let Some(installs) = &installs
                        && let Some(keyed) = installs.covered(resolved)
                    {
                        let (keyed_directories, unkeyed) = installs.directories(&keyed);
                        plan.packages = Some(Packages::shorter(keyed_directories, unkeyed));
                        covered.insert(id.clone(), keyed);
                    }
                    plans.insert(id.clone(), plan);
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
            installs,
            covered,
            depended,
            unsandboxed: Mutex::new(unsandboxed),
            packages_unchecked: BTreeMap::new(),
            watcher: None,
            directory,
            shared: Vec::new(),
            dotenv: Vec::new(),
            recording: None,
            traces: Mutex::new(BTreeMap::new()),
            given_root: workspace.root.clone(),
            from_recordings,
            rulesets: Mutex::new(Vec::new()),
            created: Mutex::new(Vec::new()),
        };
        if cfg!(target_os = "macos") {
            for (index, id) in sandbox.plans.keys().enumerate() {
                let path = sandbox.directory.join(format!("{index}.sb"));
                let plan = &sandbox.plans[id];
                // A profile Seatbelt cannot take would fail the task; it runs
                // unsandboxed instead, with the reason.
                match seatbelt(plan, id, &sandbox.root, &sandbox.state, mode, &path)? {
                    Fit::Whole => {
                        sandbox.profiles.insert(id.clone(), path);
                    }
                    Fit::WithoutPackages(reason) => {
                        sandbox.profiles.insert(id.clone(), path);
                        sandbox.packages_unchecked.insert(id.clone(), reason);
                    }
                    Fit::Rejected(reason) => {
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
        } else if mode == Mode::Audit {
            if !from_recordings {
                let recording = qk_executor::Recording::probe(&sandbox.directory).context(
                    "--sandbox on Linux records what each task does, with eBPF or strace",
                )?;
                sandbox.recording = Some(recording.resolving());
            }
        } else {
            let (shared, dotenv) = scan(&sandbox.root);
            sandbox.shared = shared;
            sandbox.dotenv = dotenv;
        }
        Ok(sandbox)
    }

    /// Records that a task ran unsandboxed after all, and why.
    pub(crate) fn unconfined(&self, id: &str, reason: &str) {
        self.unsandboxed
            .lock()
            .unwrap()
            .insert(id.to_owned(), reason.to_owned());
    }

    /// How a task is held to its plan, as it starts; `None` runs it
    /// unsandboxed, with the reason recorded.
    pub fn confine(&self, id: &str) -> Option<qk_executor::Confinement> {
        if !self.plans.contains_key(id) || self.recording.is_some() || self.from_recordings {
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

    /// Linux audit: the recording a task runs under, as it starts in `cwd`.
    pub fn recording(&self, id: &str, cwd: &Path) -> Option<qk_executor::Recording> {
        let recording = self.recording.as_ref()?;
        if !self.plans.contains_key(id) {
            return None;
        }
        let mut traces = self.traces.lock().unwrap();
        let directory = self.directory.join(format!("trace-{}", traces.len()));
        if let Err(error) = std::fs::create_dir_all(&directory) {
            drop(traces);
            self.unconfined(id, &format!("it could not be recorded: {error}"));
            return None;
        }
        let recording = recording.in_directory(directory);
        traces.insert(id.to_owned(), (recording.clone(), cwd.to_owned()));
        Some(recording)
    }

    /// Linux audit under `--record`: audits `recording`, a task's recording
    /// made for the cache, as it ran in `cwd`. Only a task that ran is handed
    /// over; one restored from the cache did nothing to audit.
    pub fn audit_recorded(&self, id: &str, recording: &qk_executor::Recording, cwd: &Path) {
        if self.from_recordings && self.plans.contains_key(id) {
            self.traces
                .lock()
                .unwrap()
                .insert(id.to_owned(), (recording.clone(), cwd.to_owned()));
        }
    }

    /// Linux audit: what each recorded task did that its plan does not
    /// allow, as the reports Seatbelt's profile would have made. A task whose
    /// recording is incomplete is not audited, and listed with why.
    fn recorded(&self) -> (Vec<Report>, BTreeSet<String>) {
        let traces = std::mem::take(&mut *self.traces.lock().unwrap());
        let mut reports = Vec::new();
        let mut audited = BTreeSet::new();
        // Only what a report can name is kept and counted: in the workspace,
        // and in `node_modules` only packages' manifests. A build reads far
        // more outside it, which within the budget `inputs analyze` keeps
        // would leave a task the cache records unaudited.
        let keep = |path: &Path| {
            [&self.root, &self.given_root].iter().any(|root| {
                path.strip_prefix(root).is_ok_and(|relative| {
                    !relative
                        .components()
                        .any(|component| component.as_os_str() == "node_modules")
                        || path.file_name() == Some(std::ffi::OsStr::new("package.json"))
                })
            })
        };
        let limits = qk_input_analysis::strace::Limits {
            records: 1_000_000,
            bytes: 256 * 1024 * 1024,
            accesses: 500_000,
            keep: Some(&keep),
            resolved: true,
        };
        for (id, (recording, cwd)) in traces {
            let collected = recording.settle().and_then(|()| {
                qk_input_analysis::strace::collect_within(&recording.directory, &cwd, &limits)
            });
            let accesses = match collected {
                Ok((accesses, issues)) => match issues.first() {
                    None => accesses,
                    Some(issue) => {
                        self.unconfined(
                            &id,
                            &format!("it was not audited, as its recording is incomplete: {issue}"),
                        );
                        continue;
                    }
                },
                Err(error) => {
                    self.unconfined(
                        &id,
                        &format!("it was not audited, as its recording is incomplete: {error:#}"),
                    );
                    continue;
                }
            };
            let plan = &self.plans[&id];
            reports.extend(
                accesses
                    .into_iter()
                    .filter_map(|access| self.report(&id, plan, access)),
            );
            audited.insert(id);
        }
        (reports, audited)
    }

    /// The report Seatbelt's audit profile would make of `access`, if any:
    /// a read of a file or a write in the workspace that `plan` does not
    /// allow, and in `node_modules` only a read of a package's manifest, by
    /// which a package the key leaves out is found.
    fn report(&self, id: &str, plan: &Plan, access: qk_input_analysis::Access) -> Option<Report> {
        use qk_input_analysis::Operation;
        let write = access.operation.writes();
        if write {
            if !matches!(access.result.as_str(), "success" | "unknown") {
                return None;
            }
        } else if !matches!(
            access.operation,
            Operation::OpenRead | Operation::ReadData | Operation::Execute
        ) || access.result != "success"
        {
            return None;
        }
        let path = PathBuf::from(access.resolved_path.as_deref().unwrap_or(&access.path));
        let path = if path.starts_with(&self.root) {
            path
        } else {
            self.root.join(path.strip_prefix(&self.given_root).ok()?)
        };
        // Directories may be listed; what a listing finds is not read.
        if !write && path.is_dir() {
            return None;
        }
        let relative = path.strip_prefix(&self.root).ok()?;
        if relative
            .components()
            .any(|component| component.as_os_str() == "node_modules")
        {
            if write
                || plan.packages.is_none()
                || path.file_name() != Some(std::ffi::OsStr::new("package.json"))
            {
                return None;
            }
        } else if plan.allows(&path, write, &self.state) {
            return None;
        }
        Some(Report {
            task: id.to_owned(),
            process: access.process,
            operation: if write {
                "file-write-data"
            } else {
                "file-read-data"
            }
            .into(),
            path: path.to_string_lossy().into_owned(),
        })
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
    /// returns each task's findings, the tasks that ran unsandboxed and those
    /// whose installed packages went unchecked.
    pub fn finish(mut self) -> Result<Finished> {
        let (reports, audited) = match self.watcher.take() {
            Some(watcher) => {
                let (reports, dropped) = watcher.finish(&self.directory)?;
                if dropped > 0 {
                    bail!("sandbox log exceeded 100000 reports; {dropped} reports omitted");
                }
                (reports, self.plans.keys().cloned().collect())
            }
            None if self.mode == Mode::Audit => self.recorded(),
            None => (Vec::new(), BTreeSet::new()),
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
            let package = (!report.operation.starts_with("file-write"))
                .then(|| {
                    let installs = self.installs.as_ref()?;
                    installs.unkeyed(self.covered.get(&report.task)?, Path::new(&report.path))
                })
                .flatten();
            // Reads in `node_modules` are reported only to find packages: one
            // the key covers, or a manifest belonging to no package, is not
            // a finding.
            if package.is_none() && relative.split('/').any(|part| part == "node_modules") {
                continue;
            }
            let dependency = self
                .depended
                .get(&report.task)
                .into_iter()
                .flatten()
                .find(|(_, root)| relative.starts_with(&format!("{root}/")))
                .map(|(project, _)| project.clone());
            let entry = findings.entry(report.task).or_default();
            if let Some(package) = package {
                entry.unkeyed_packages.insert(package);
            } else if report.operation.starts_with("file-write") {
                entry.stray_writes.insert(relative);
            } else if unkeyed(&relative) {
                entry.unkeyed_reads.insert(relative);
            } else if let Some(project) = dependency
                && qk_input_analysis::ReadKind::of(&relative)
                    == qk_input_analysis::ReadKind::Content
            {
                entry
                    .dependency_reads
                    .entry(project)
                    .or_default()
                    .insert(relative);
            } else {
                match qk_input_analysis::ReadKind::of(&relative) {
                    qk_input_analysis::ReadKind::Manifest => &mut entry.manifest_reads,
                    qk_input_analysis::ReadKind::Toolchain => &mut entry.toolchain_reads,
                    qk_input_analysis::ReadKind::TsConfig => &mut entry.tsconfig_reads,
                    qk_input_analysis::ReadKind::Content => &mut entry.undeclared_reads,
                }
                .insert(relative);
            }
        }
        // A file a task writes outside its outputs and reads, as a version
        // stamp it keeps, is one finding: declaring it an output covers both.
        for findings in findings.values_mut() {
            for path in &findings.stray_writes {
                findings.undeclared_reads.remove(path);
                findings.manifest_reads.remove(path);
                findings.toolchain_reads.remove(path);
                findings.tsconfig_reads.remove(path);
                for paths in findings.dependency_reads.values_mut() {
                    paths.remove(path);
                }
            }
            findings
                .dependency_reads
                .retain(|_, paths| !paths.is_empty());
        }
        let unsandboxed = std::mem::take(&mut *self.unsandboxed.lock().unwrap());
        // A task run again unsandboxed was audited only until it failed, so it
        // is listed as unsandboxed rather than with part of its findings.
        findings.retain(|id, findings| !findings.is_empty() && !unsandboxed.contains_key(id));
        let audited = audited
            .into_iter()
            .filter(|id| !unsandboxed.contains_key(id))
            .collect();
        Ok(Finished {
            findings,
            unsandboxed,
            packages_unchecked: std::mem::take(&mut self.packages_unchecked),
            audited,
        })
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

/// How much of a task's plan its profile holds it to.
#[derive(Debug, PartialEq)]
enum Fit {
    Whole,
    /// All but its installed packages, with why.
    WithoutPackages(String),
    /// Nothing: Seatbelt rejects its profile, with why.
    Rejected(String),
}

/// Writes a task's profile to `path`. Seatbelt caps the paths a profile with
/// reports may name, and a large store can take them all: a task whose
/// profile it rejects is still held to its inputs and outputs, without
/// reporting packages.
fn seatbelt(
    plan: &Plan,
    id: &str,
    root: &Path,
    state: &Path,
    mode: Mode,
    path: &Path,
) -> Result<Fit> {
    std::fs::write(path, profile(plan, id, root, state, mode)?)?;
    let Err(reason) = compiles(path) else {
        return Ok(Fit::Whole);
    };
    if plan.packages.is_none() {
        return Ok(Fit::Rejected(reason));
    }
    let without = Plan {
        packages: None,
        ..plan.clone()
    };
    std::fs::write(path, profile(&without, id, root, state, mode)?)?;
    Ok(match compiles(path) {
        Ok(()) => Fit::WithoutPackages(reason),
        Err(reason) => Fit::Rejected(reason),
    })
}

/// For each task, the projects its project depends on, directly or not, with
/// their roots, the deepest root first so a nested project is found before
/// the one around it. The workspace's root project is left out: every path
/// is beneath it.
fn depended(
    workspace: &Workspace,
    graph: &TaskGraph,
) -> Result<BTreeMap<String, Vec<(String, String)>>> {
    let projects = qk_graph::ProjectGraph::build(workspace)?;
    let mut by_project: BTreeMap<&str, Vec<(String, String)>> = BTreeMap::new();
    let mut found = BTreeMap::new();
    for (id, task) in &graph.tasks {
        let roots = by_project.entry(task.project.as_str()).or_insert_with(|| {
            let mut seen = BTreeSet::new();
            let mut pending = vec![task.project.as_str()];
            while let Some(project) = pending.pop() {
                for dependency in projects.dependencies.get(project).into_iter().flatten() {
                    if dependency.target != task.project && seen.insert(dependency.target.as_str())
                    {
                        pending.push(&dependency.target);
                    }
                }
            }
            let mut roots: Vec<(String, String)> = seen
                .into_iter()
                .filter_map(|name| {
                    // A root is kept as written, so `./libs/lib/` is
                    // `libs/lib`, as reads are named.
                    let root = workspace
                        .projects
                        .get(name)?
                        .root
                        .split('/')
                        .filter(|part| !part.is_empty() && *part != ".")
                        .collect::<Vec<_>>()
                        .join("/");
                    (!root.is_empty()).then(|| (name.to_owned(), root))
                })
                .collect();
            roots.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
            roots
        });
        found.insert(id.clone(), roots.clone());
    }
    Ok(found)
}

/// Whether `task` failed because a command in it applied a sandbox of its
/// own inside the task's, which Seatbelt refuses once the outer one restricts
/// anything, as every task's profile does.
pub(crate) fn nested(task: &qk_executor::PreparedTask, result: &qk_cache::TaskResult) -> bool {
    const REFUSED: &[u8] = b"sandbox_apply: Operation not permitted";
    if !matches!(task.sandbox, Some(qk_executor::Confinement::Seatbelt(_)))
        || !matches!(result.outcome, qk_executor::Outcome::Failed(_))
    {
        return false;
    }
    let Some(execution) = &result.execution else {
        return false;
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let read = qk_executor::read_capture(execution.log.data.as_slice(), |is_stderr, bytes| {
        if is_stderr { &mut stderr } else { &mut stdout }.extend_from_slice(bytes);
        Ok(())
    });
    read.is_ok()
        && [stdout, stderr].iter().any(|stream| {
            stream
                .windows(REFUSED.len())
                .any(|window| window == REFUSED)
        })
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
    /// Configuration files a task reads only as declared, which the files
    /// counted can leave out, as Git leaves out an ignored
    /// `project.local.json`.
    protected: BTreeSet<String>,
}

impl Tree {
    fn new(files: &BTreeSet<String>) -> Self {
        let mut counts = BTreeMap::new();
        for file in files {
            for directory in parents(file) {
                *counts.entry(directory.to_owned()).or_insert(0) += 1;
            }
        }
        Self {
            counts,
            protected: BTreeSet::new(),
        }
    }

    /// With `protected`, the configuration files no directory holding one is
    /// allowed whole for, unless the task's inputs hold it: Landlock grants a
    /// directory with everything beneath it, so it cannot leave one out.
    fn protecting(mut self, protected: BTreeSet<String>) -> Self {
        self.protected = protected;
        self
    }

    /// The inputs as the fewest paths: each outermost directory whose every
    /// file is an input, and holding no protected file that is not, and the
    /// other inputs one by one.
    fn cover<'a>(&self, inputs: &'a BTreeSet<String>) -> (Vec<String>, Vec<&'a str>) {
        let mut covered: BTreeMap<&str, usize> = BTreeMap::new();
        for input in inputs {
            for directory in parents(input) {
                *covered.entry(directory).or_insert(0) += 1;
            }
        }
        let held: BTreeSet<&str> = self
            .protected
            .iter()
            .filter(|path| !inputs.contains(*path))
            .flat_map(|path| parents(path))
            .collect();
        let whole: BTreeSet<&str> = covered
            .iter()
            .filter(|(directory, count)| {
                !directory.is_empty()
                    && self.counts.get(**directory) == Some(count)
                    && !held.contains(**directory)
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

/// The configuration files of `workspace` a task reads only as declared,
/// workspace-relative, those that exist: nx.json, the file it extends and
/// nx.local.json, each project's `project.json` and `project.local.json`,
/// the pnpm lockfile, `pnpm-workspace.yaml` and the root `package.json`.
fn protected(workspace: &Workspace) -> BTreeSet<String> {
    // A project's manifests where its root is and where they were read,
    // which a `root` they give can move apart. A configured root keeps its
    // spelling, such as `./libs/lib/`, where candidates are normalized.
    let directories = workspace.projects.values().flat_map(|project| {
        [
            Some(project.root.as_str()),
            workspace
                .configuration_directories
                .get(&project.name)
                .map(String::as_str),
        ]
    });
    let manifests = directories.flatten().flat_map(|directory| {
        let directory = directory
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/");
        ["project.json", qk_config::LOCAL_OVERRIDES].map(move |name| match directory.as_str() {
            "" => name.to_owned(),
            directory => format!("{directory}/{name}"),
        })
    });
    [
        "nx.json",
        qk_config::LOCAL_WORKSPACE,
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "package.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(workspace.extended.clone())
    .chain(manifests)
    .filter(|path| workspace.root.join(path).is_file())
    .collect()
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
    if let Ok(entries) = qk_cache::warm::config(workspace, task)
        && let Ok(paths) = qk_cache::Outputs::from_paths(&qk_cache::warm::kept_paths(&entries))
    {
        write_trees.extend(paths.anchors().map(path));
    }
    Plan {
        read_directories: whole.iter().map(|directory| path(directory)).collect(),
        read_files,
        read_trees,
        write_trees,
        packages: None,
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
    rules.push(format!(
        "(allow file-read-data file-write* (regex #\"^{}/(.*/)?node_modules/\"))",
        literal_regex(root_text)
    ));
    // A package the key leaves out is reported by its manifests, which Node
    // and bundlers read for every package they load: a report for each of
    // the thousands of files a tool reads would crowd out the run's other
    // reports, which the log drops when many arrive at once. Seatbelt matches
    // the path a link resolves to, so reads through an importer's
    // `node_modules/<name>` land here too.
    //
    // Seatbelt caps the paths a profile with reports may name at 64 KiB
    // together, whatever rules they are spread over, and a store may hold
    // thousands of directories: the shorter of the two lists is named, those
    // to report or, after a rule reporting every manifest in the store, those
    // not to, since a later rule wins.
    let manifests = |directories: &[PathBuf], report: &str| -> Result<Vec<String>> {
        let mut rules = Vec::new();
        for chunk in directories.chunks(200) {
            rules.push(
                "(allow file-read-data (require-all (regex #\"/package\\.json$\") (require-any"
                    .to_owned(),
            );
            for directory in chunk {
                rules.push(format!("  (subpath {})", text(directory)?));
            }
            rules.push(format!(")){report})"));
        }
        Ok(rules)
    };
    let report = format!(" (with report) (with message {tag})");
    match &plan.packages {
        None => {}
        Some(Packages::Unkeyed(directories)) => rules.extend(manifests(directories, &report)?),
        Some(Packages::AllBut(directories)) => {
            rules.push(format!(
                "(allow file-read-data (regex #\"^{}/node_modules/\\.pnpm/[^/]+/(.*/)?package\\.json$\"){report})",
                literal_regex(root_text)
            ));
            rules.extend(manifests(directories, "")?);
        }
    }
    // Directories may be listed; what a listing finds is not read.
    rules.push("(allow file-read-data (vnode-type DIRECTORY))".to_owned());
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
    // The paths are spread over rules of a few hundred each; together they
    // count toward Seatbelt's cap, as the packages' do.
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
///
/// The system log drops some of the sandbox's reports when many arrive at
/// once, or as other readers start, for every reader alike and without
/// saying so. A probe is therefore repeated until one of its reports
/// arrives, rather than waited for once.
pub(crate) struct Watcher {
    child: Child,
    reports: Arc<Mutex<Vec<Report>>>,
    /// The latest probe whose report has arrived, counted from 1.
    sentinel: Arc<Mutex<u64>>,
    /// Probes made so far.
    probes: std::sync::atomic::AtomicU64,
    marker: String,
    dropped: Arc<std::sync::atomic::AtomicUsize>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub(crate) fn start(directory: &Path) -> Result<Self> {
        let mut child = Command::new("log")
            // Reports are logged at the default level and refusals as errors;
            // the debug level would add every profile Seatbelt applies,
            // which only crowds the log the reports pass through.
            .args([
                "stream",
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
        let sentinel = Arc::new(Mutex::new(0));
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
                    if let Some((_, probe)) = message.split_once(&marker) {
                        if let Some(probe) = probe
                            .strip_prefix(':')
                            .and_then(|probe| probe.trim().parse::<u64>().ok())
                        {
                            let mut sentinel = sentinel.lock().unwrap();
                            *sentinel = (*sentinel).max(probe);
                        }
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
            probes: std::sync::atomic::AtomicU64::new(0),
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
    /// every report before it has arrived. The system log delivers reports
    /// in the order they were made, to within far less than a probe takes,
    /// so any one of the attempts arriving says as much; an attempt the log
    /// dropped is made again until the deadline.
    fn probe(&self, directory: &Path, name: &str) -> Result<()> {
        let marker = directory.join(format!("{name}.marker"));
        std::fs::write(&marker, "")?;
        let marker = marker.canonicalize()?;
        let marker_text = quote(marker.to_str().context("marker path must be UTF-8")?);
        let profile = directory.join(format!("{name}.sb"));
        let deadline = Instant::now() + PROBE_DEADLINE;
        let mut first = None;
        loop {
            let attempt = self
                .probes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let first = *first.get_or_insert(attempt);
            std::fs::write(
                &profile,
                format!(
                    "(version 1)\n(allow default)\n(allow file-read-data (literal {marker_text}) (with report) (with message {}))\n",
                    quote(&format!("{}:{attempt}", self.marker))
                ),
            )?;
            let output = Command::new("sandbox-exec")
                .arg("-f")
                .arg(&profile)
                .arg("/bin/cat")
                .arg(&marker)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .context("cannot run sandbox-exec")?;
            if !output.status.success() {
                bail!(
                    "sandbox-exec could not run the probe for the sandbox's reports: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let retry = (Instant::now() + PROBE_RETRY).min(deadline);
            while Instant::now() < retry {
                if *self.sentinel.lock().unwrap() >= first {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if Instant::now() >= deadline {
                bail!("the sandbox's reports did not reach the system log");
            }
        }
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
    fn protects_the_manifests_of_a_root_spelled_with_dots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("config/lib")).unwrap();
        std::fs::create_dir_all(root.join("libs/lib")).unwrap();
        std::fs::write(
            root.join("config/lib/project.json"),
            r#"{"name":"lib","root":"./libs/lib/"}"#,
        )
        .unwrap();
        std::fs::write(root.join("libs/lib/project.local.json"), "{}").unwrap();
        let workspace = qk_config::Workspace::load(root).unwrap();
        let protected = protected(&workspace);
        assert!(
            protected.contains("libs/lib/project.local.json"),
            "{protected:?}"
        );
        // And where it was read, which its `root` moved away from.
        assert!(
            protected.contains("config/lib/project.json"),
            "{protected:?}"
        );
    }

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

        // A configuration file the files leave out, such as an ignored
        // project.local.json, keeps its directories from being allowed
        // whole, unless the inputs hold it.
        let tree =
            Tree::new(&files).protecting(BTreeSet::from(["app/lib/project.local.json".into()]));
        let (whole, singles) = tree.cover(&inputs);
        assert_eq!(whole, ["app/src/deep"]);
        assert_eq!(singles, ["app/lib/d.ts", "app/lib/e.ts", "app/src/a.ts"]);
        let mut declared = inputs.clone();
        declared.insert("app/lib/project.local.json".into());
        assert!(!tree.cover(&declared).1.contains(&"app/lib/d.ts"));
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

    /// Seatbelt rejects a profile naming thousands of store directories with
    /// reports; the shorter list fits, and a store too large for either still
    /// holds the task to its inputs and outputs.
    #[cfg(target_os = "macos")]
    #[test]
    fn fits_a_large_store_into_a_profile() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = |range: std::ops::Range<usize>| -> Vec<PathBuf> {
            range
                .map(|index| {
                    root.join(format!(
                        "node_modules/.pnpm/@scope+package-{index}@1.0.{index}_peer@2.0.0"
                    ))
                })
                .collect()
        };
        let plan = |packages| Plan {
            read_files: vec![root.join("src/index.ts")],
            packages: Some(packages),
            ..Plan::default()
        };
        let fit = |plan: &Plan| {
            seatbelt(
                plan,
                "app:lint",
                &root,
                &root.join(".qk"),
                Mode::Audit,
                &root.join("p.sb"),
            )
            .unwrap()
        };
        let (keyed, unkeyed) = (store(0..100), store(100..2600));
        assert_eq!(
            fit(&plan(Packages::shorter(keyed, unkeyed.clone()))),
            Fit::Whole
        );
        assert!(matches!(
            fit(&plan(Packages::Unkeyed(unkeyed))),
            Fit::WithoutPackages(reason) if reason.contains("exceeds maximum")
        ));
        assert!(matches!(
            Packages::shorter(store(0..2), store(2..3)),
            Packages::Unkeyed(directories) if directories.len() == 1
        ));
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
