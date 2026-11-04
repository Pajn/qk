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
//! mode it is refused, except reading and writing `.git`, which a task may
//! need although it is not keyed. A dotenv file is read only as its inputs
//! or its ignored reads name it: one qk loads for the task matters through
//! the variables it sets, which `env` inputs key, and one it does not load
//! is an input like any other file.
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
//! A task the cache records for `--record` is audited from that recording
//! rather than recorded twice.
//!
//! The cache stays on: a task restored from it did not run, so it is neither
//! audited nor held to its plan, and is listed as restored.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Task, TaskGraph};

use crate::installs::Installs;

mod audit;
mod landlock;
mod policy;
mod seatbelt;

use audit::TaskAudit;
pub use audit::{Findings, is_dotenv};
use policy::Policy;
pub use policy::in_git;
use seatbelt::{Fit, quote, seatbelt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Allow what a task does beyond its declarations, and report it.
    Audit,
    /// Refuse it.
    Enforce,
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
    /// Its dependencies' outputs, and directories its ignored reads name
    /// whole.
    read_trees: BTreeSet<PathBuf>,
    /// The files and directories its ignored reads name, which held no link
    /// as the run started: Landlock, which follows links, allows each only
    /// while that still holds, so a link an earlier task put in its place
    /// leads nowhere new.
    unlinked: BTreeSet<PathBuf>,
    /// Its outputs, warm paths and scratch directories.
    write_trees: BTreeSet<PathBuf>,
    /// Its scratch directories, which are directories whatever their names:
    /// Landlock, which holds only paths that exist, needs them made first.
    scratch: BTreeSet<PathBuf>,
    /// Audit: the pnpm store directories whose manifests' reads are
    /// reported, as the shorter list Seatbelt takes.
    packages: Option<Packages>,
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
    policies: BTreeMap<String, Policy>,
    profiles: BTreeMap<String, PathBuf>,
    /// The installed packages, for telling which package a reported read
    /// belongs to.
    installs: Option<Installs>,
    /// What each task's reports are judged against.
    audits: BTreeMap<String, TaskAudit>,
    /// Tasks that run unsandboxed, and why.
    unsandboxed: Mutex<BTreeMap<String, String>>,
    /// Enforce: tasks that cannot be sandboxed, and why. They fail when they
    /// would run, rather than run unconfined; a cache hit runs nothing.
    refused: Mutex<BTreeMap<String, String>>,
    /// Audit: tasks sandboxed without reporting installed packages, and why.
    packages_unchecked: BTreeMap<String, String>,
    watcher: Option<Watcher>,
    directory: PathBuf,
    /// Linux enforce: what the policies' patterns name in the workspace,
    /// since Landlock takes paths rather than patterns: those there as the
    /// run starts, and the `node_modules` tasks over overlays, or unsandboxed,
    /// make as they end, for the tasks starting after them. A task held by
    /// Landlock
    /// alone cannot make a `node_modules` itself, and a command over an
    /// overlay cannot read the files it wrote in one it made, only what
    /// links there lead to.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    expansion: Mutex<landlock::Expansion>,
    /// Linux enforce: tasks running unsandboxed, whose `node_modules` the
    /// workspace is scanned for again as they end.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    running_bare: Mutex<BTreeSet<String>>,
    /// Linux audit: the recorder, and each task's recording with the
    /// directory its commands start in.
    recording: Option<qk_executor::Recording>,
    traces: Mutex<BTreeMap<String, (qk_executor::Recording, PathBuf)>>,
    /// Trace directories made so far, which names the next: tasks restored
    /// from the cache leave `traces`, so its length could repeat a name.
    trace_count: std::sync::atomic::AtomicUsize,
    /// The workspace root as given, which recorded paths may name rather
    /// than `root`, its links resolved.
    given_root: PathBuf,
    /// Tasks restored from the cache, which did nothing to check.
    restored: Mutex<BTreeSet<String>>,
    /// Tasks that ran no command, such as `nx:noop`: held to their plans,
    /// with nothing recorded to hold them to.
    idle: Mutex<BTreeSet<String>>,
    /// Linux enforce: whether tasks run over overlays, why they are held by
    /// Landlock instead where they cannot, each task's overlay until it
    /// settles, and what each wrote outside its outputs, which was not kept.
    overlay: bool,
    fallback: Option<String>,
    overlays: Mutex<BTreeMap<String, std::sync::Arc<qk_executor::Overlay>>>,
    discarded: Mutex<BTreeMap<String, BTreeSet<String>>>,
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
    /// Enforce: tasks that could not be sandboxed, and so failed, and why.
    pub refused: BTreeMap<String, String>,
    /// Tasks sandboxed without reporting installed packages, and why.
    pub packages_unchecked: BTreeMap<String, String>,
    /// Audit: the tasks held to their plans, whether or not they went
    /// beyond them.
    pub audited: BTreeSet<String>,
    /// Tasks restored from the cache, which did not run to be checked.
    pub restored: BTreeSet<String>,
    /// Linux enforce: why tasks were held by Landlock rather than run over
    /// overlays, where they were.
    pub fallback: Option<String>,
}

/// Tag in each rule's message: the task a report belongs to.
const TAG: &str = "qk-task:";
const SENTINEL: &str = "qk-sandbox-sentinel";
/// How long a probe's report may take before the probe is made again, and
/// before the system log is taken not to carry the sandbox's reports.
const PROBE_RETRY: Duration = Duration::from_millis(500);
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

impl Sandbox {
    /// Works out what each task may do, and with the dotenv files `prepared`
    /// says each task's environment was loaded from, what its reports are
    /// judged against; on macOS writes its profile and starts reading the
    /// log.
    pub fn start(
        workspace: &Workspace,
        graph: &TaskGraph,
        prepared: &BTreeMap<String, qk_executor::PreparedTask>,
        mode: Mode,
    ) -> Result<Self> {
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
        let installs = Installs::load(&root);
        let projects = qk_graph::ProjectGraph::build(workspace)?;
        let mut audits: BTreeMap<String, TaskAudit> = depended(workspace, graph, &projects)
            .into_iter()
            .map(|(id, depended)| (id, TaskAudit::new(&root, depended)))
            .collect();
        let mut policies = BTreeMap::new();
        // The files each list of ignored reads matches, walked once however
        // many tasks share it.
        let mut walked: BTreeMap<(Vec<String>, String), Ignored> = BTreeMap::new();
        let mut unsandboxed = BTreeMap::new();
        let mut refused = BTreeMap::new();
        // What keeps a task from its sandbox: audit runs it unsandboxed and
        // lists it, enforce fails it.
        let unconfinable = |unsandboxed: &mut BTreeMap<String, String>,
                            refused: &mut BTreeMap<String, String>,
                            id: &str,
                            reason: String| {
            match mode {
                Mode::Enforce => refused.insert(id.to_owned(), reason),
                Mode::Audit => unsandboxed.insert(id.to_owned(), reason),
            };
        };
        for (id, task) in &graph.tasks {
            let audit = audits.get_mut(id).expect("every task has an audit");
            match qk_cache::scratch::Settings::of(task.definition.extra.get("qk:sandbox")) {
                Ok(settings) if settings.sandboxed => {}
                Ok(_) => {
                    unsandboxed.insert(id.clone(), "its target sets \"qk:sandbox\": false".into());
                    continue;
                }
                Err(error) => {
                    unconfinable(&mut unsandboxed, &mut refused, id, format!("{error:#}"));
                    continue;
                }
            }
            // Reads that change nothing it does, recorded or not.
            let mut ignored_paths = Ignored::default();
            if let Ok(written) = qk_cache::ultracache::ignored_reads(task)
                && let Ok(reads) = qk_cache::ultracache::IgnoredReads::new(
                    &written,
                    workspace,
                    Some(&projects),
                    &task.project,
                )
                && !reads.is_empty()
            {
                // What `^` entries match depends on the task's project; what
                // the others match does not.
                let project = if written.iter().any(|glob| glob.starts_with('^')) {
                    task.project.clone()
                } else {
                    String::new()
                };
                ignored_paths = walked
                    .entry((written, project))
                    .or_insert_with(|| matching_paths(&root, &reads))
                    .clone();
                audit.ignoring(reads);
            }
            match &resolution.tasks[id] {
                Ok(resolved) => {
                    let files = resolved.file_inputs().map(str::to_owned).collect();
                    let mut plan = plan(workspace, graph, task, &root, &tree, &files);
                    // What its ignored reads name, which it may read under
                    // enforce too. Landlock and Seatbelt hold paths, so of a
                    // file a glob matches, only one there as the run starts;
                    // in a directory allowed whole, any file, made before
                    // the run or during it.
                    let ignored_paths = std::mem::take(&mut ignored_paths);
                    plan.unlinked.extend(ignored_paths.files.iter().cloned());
                    plan.unlinked.extend(ignored_paths.trees.iter().cloned());
                    plan.read_files.extend(ignored_paths.files);
                    plan.read_trees.extend(ignored_paths.trees);
                    if let Some(installs) = &installs {
                        // `None` when the whole lockfile is keyed.
                        let mut keyed = installs.covered(resolved);
                        // What it reaches through the nested workspaces its
                        // ignored reads cover is permitted as they are.
                        if let Some(reads) = audit.ignored() {
                            // Keyed whole, the lockfile names no importers of
                            // its own; its project's is.
                            let importers =
                                resolved.inferred.lockfile.clone().unwrap_or_else(|| {
                                    [workspace.projects[&task.project].root.clone()].into()
                                });
                            let (snapshots, manifests) =
                                installs.ignored(&importers, keyed.as_ref(), |manifest| {
                                    reads.is_match(manifest)
                                });
                            if let Some(keyed) = &mut keyed {
                                keyed.extend(snapshots);
                            }
                            plan.read_files.extend(
                                manifests
                                    .iter()
                                    .map(|manifest| root.join(manifest))
                                    .filter(|path| path.is_file()),
                            );
                            audit.ignoring_manifests(manifests);
                        }
                        // Enforce leaves `node_modules` open.
                        if let Some(keyed) = keyed
                            && mode == Mode::Audit
                        {
                            let (keyed_directories, unkeyed) = installs.directories(&keyed);
                            plan.packages = Some(Packages::shorter(keyed_directories, unkeyed));
                            audit.covering(keyed, resolved.declared.external_declared);
                        }
                    }
                    audit.writing(plan.write_trees.iter().cloned());
                    audit.loading(
                        prepared
                            .get(id)
                            .into_iter()
                            .flat_map(|task| &task.dotenv)
                            .filter_map(|path| workspace_relative(&root, &workspace.root, path)),
                    );
                    policies.insert(id.clone(), Policy::new(mode, &root, &state, plan));
                }
                Err(reason) => {
                    unconfinable(&mut unsandboxed, &mut refused, id, reason.clone());
                }
            }
        }
        let mut sandbox = Self {
            mode,
            root,
            policies,
            profiles: BTreeMap::new(),
            installs,
            audits,
            unsandboxed: Mutex::new(unsandboxed),
            refused: Mutex::new(refused),
            packages_unchecked: BTreeMap::new(),
            watcher: None,
            directory,
            expansion: Mutex::new(landlock::Expansion::default()),
            running_bare: Mutex::new(BTreeSet::new()),
            recording: None,
            traces: Mutex::new(BTreeMap::new()),
            trace_count: std::sync::atomic::AtomicUsize::new(0),
            given_root: workspace.root.clone(),
            restored: Mutex::new(BTreeSet::new()),
            idle: Mutex::new(BTreeSet::new()),
            overlay: false,
            fallback: None,
            overlays: Mutex::new(BTreeMap::new()),
            discarded: Mutex::new(BTreeMap::new()),
            rulesets: Mutex::new(Vec::new()),
            created: Mutex::new(Vec::new()),
        };
        // Linux enforce runs each task over an overlay where a command can
        // have one, and holds it by Landlock alone where not, saying why.
        #[cfg(target_os = "linux")]
        if mode == Mode::Enforce
            && std::env::var_os("QK_SANDBOX_OVERLAY").is_none_or(|value| value != "0")
        {
            let probe = sandbox.directory.join("overlay-probe");
            match qk_executor::overlay::probe(&probe) {
                Ok(()) => sandbox.overlay = true,
                Err(error) => sandbox.fallback = Some(format!("{error:#}")),
            }
        }
        if cfg!(target_os = "macos") {
            for (index, id) in sandbox.policies.keys().enumerate() {
                let path = sandbox.directory.join(format!("{index}.sb"));
                // A profile Seatbelt cannot take: audit runs the task
                // unsandboxed, with the reason, and enforce fails it.
                match seatbelt(&sandbox.policies[id], id, &path)? {
                    Fit::Whole => {
                        sandbox.profiles.insert(id.clone(), path);
                    }
                    Fit::WithoutPackages(reason) => {
                        sandbox.profiles.insert(id.clone(), path);
                        sandbox.packages_unchecked.insert(id.clone(), reason);
                    }
                    Fit::Rejected(reason) if mode == Mode::Enforce => {
                        sandbox.refused.lock().unwrap().insert(id.clone(), reason);
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
            let recording = qk_executor::Recording::probe(&sandbox.directory)
                .context("--sandbox on Linux records what each task does, with eBPF or strace")?;
            sandbox.recording = Some(recording.resolving());
        } else {
            sandbox.expansion = Mutex::new(landlock::Expansion::scan(&sandbox.root));
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
    /// unsandboxed, with the reason recorded. Enforced, a task that cannot be
    /// held to it is refused instead.
    pub fn confine(&self, id: &str) -> Option<qk_executor::Confinement> {
        if let Some(reason) = self.refused.lock().unwrap().get(id) {
            return Some(qk_executor::Confinement::Refused(reason.clone()));
        }
        if !self.policies.contains_key(id) || self.recording.is_some() {
            if cfg!(target_os = "linux") && self.mode == Mode::Enforce {
                self.running_bare.lock().unwrap().insert(id.to_owned());
            }
            return None;
        }
        if self.overlay {
            // As for Seatbelt: the overlay's lower layer is the workspace, so
            // a missing output's directories made in it are there to build in.
            self.make_outputs(id);
            return match self.overlaid(id) {
                Ok(confinement) => Some(confinement),
                Err(error) => {
                    let reason = format!("{error:#}");
                    self.refused
                        .lock()
                        .unwrap()
                        .insert(id.to_owned(), reason.clone());
                    Some(qk_executor::Confinement::Refused(reason))
                }
            };
        }
        if cfg!(target_os = "macos") {
            self.make_outputs(id);
            return self
                .profiles
                .get(id)
                .map(|profile| qk_executor::Confinement::Seatbelt(profile.clone()));
        }
        match self.landlock(id) {
            Ok(confinement) => Some(confinement),
            Err(error) if self.mode == Mode::Enforce => {
                let reason = format!("{error:#}");
                self.refused
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), reason.clone());
                Some(qk_executor::Confinement::Refused(reason))
            }
            Err(error) => {
                self.unsandboxed
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), format!("{error:#}"));
                None
            }
        }
    }

    /// Linux enforce over an overlay: where the task's layers go, what
    /// passes through to the workspace, `.git`, qk's state where it lies in
    /// the workspace, every `node_modules` and the task's scratch
    /// directories, and the Landlock rules reads keep to.
    #[cfg(target_os = "linux")]
    fn overlaid(&self, id: &str) -> Result<qk_executor::Confinement> {
        let policy = &self.policies[id];
        let number = self
            .trace_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for directory in policy.scratch() {
            std::fs::create_dir_all(directory)?;
        }
        let (rules, passthrough, keep) =
            landlock::overlay(policy, &self.expansion.lock().unwrap())?;
        // In this run's own directory: another run in the workspace numbers
        // its tasks' overlays from 0 too. One a run of the same process ID
        // left behind is cleared, never mounted again.
        let directory = self.directory.join(format!("overlay-{number}"));
        qk_executor::overlay::clear(&directory);
        let overlay = std::sync::Arc::new(qk_executor::Overlay {
            workspace: self.root.clone(),
            directory,
            passthrough,
            rules,
            keep,
            settled: std::sync::OnceLock::new(),
        });
        self.overlays
            .lock()
            .unwrap()
            .insert(id.to_owned(), overlay.clone());
        Ok(qk_executor::Confinement::Overlay(overlay))
    }

    #[cfg(not(target_os = "linux"))]
    fn overlaid(&self, _: &str) -> Result<qk_executor::Confinement> {
        bail!("overlays are Linux's")
    }

    /// As a task ends, over an overlay: applies its commands' writes, if the
    /// executor has not as they ended, removes its layers, and keeps what
    /// lay outside its outputs, warm paths and scratch directories as
    /// findings. Under Linux enforce, unsandboxed: adds the `node_modules`
    /// it made to those later tasks are granted, rescanning the workspace
    /// and adding to what is known rather than replacing it, which another
    /// task settling meanwhile may have added to.
    pub fn settle(&self, id: &str) {
        if self.running_bare.lock().unwrap().remove(id) {
            let found = landlock::Expansion::scan(&self.root);
            self.expansion
                .lock()
                .unwrap()
                .add_node_modules(&found.node_modules);
        }
        let Some(overlay) = self.overlays.lock().unwrap().remove(id) else {
            return;
        };
        let settled = overlay.settle();
        let discarded = settled.discarded.clone();
        self.expansion
            .lock()
            .unwrap()
            .add_node_modules(&settled.made);
        #[cfg(target_os = "linux")]
        qk_executor::overlay::clear(&overlay.directory);
        #[cfg(not(target_os = "linux"))]
        let _ = std::fs::remove_dir_all(&overlay.directory);
        if !discarded.is_empty() {
            self.discarded
                .lock()
                .unwrap()
                .entry(id.to_owned())
                .or_default()
                .extend(discarded);
        }
    }

    /// Linux audit: the recording a task runs under, as it starts in `cwd`.
    /// One `existing` already, as the cache records it for `--record`, is
    /// audited instead, and `None` returned.
    pub fn recording(
        &self,
        id: &str,
        cwd: &Path,
        existing: Option<&qk_executor::Recording>,
    ) -> Option<qk_executor::Recording> {
        let recording = self.recording.as_ref()?;
        if !self.policies.contains_key(id) {
            return None;
        }
        self.make_outputs(id);
        let mut traces = self.traces.lock().unwrap();
        if let Some(existing) = existing {
            traces.insert(id.to_owned(), (existing.clone(), cwd.to_owned()));
            return None;
        }
        let number = self
            .trace_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = self.directory.join(format!("trace-{number}"));
        if let Err(error) = std::fs::create_dir_all(&directory) {
            drop(traces);
            self.unconfined(id, &format!("it could not be recorded: {error}"));
            return None;
        }
        let recording = recording.in_directory(directory);
        traces.insert(id.to_owned(), (recording.clone(), cwd.to_owned()));
        Some(recording)
    }

    /// Records that a task runs no command: it did nothing beyond its
    /// declarations, and has no recording to read.
    pub fn ran_nothing(&self, id: &str) {
        if self.policies.contains_key(id) {
            self.idle.lock().unwrap().insert(id.to_owned());
        }
    }

    /// Records that a task was restored from the cache: it did not run, so
    /// there is nothing to check, and its empty recording is not read.
    pub fn restored(&self, id: &str) {
        self.traces.lock().unwrap().remove(id);
        self.restored.lock().unwrap().insert(id.to_owned());
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
            // A finding names each path once, whichever process read it.
            distinct: true,
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
            let policy = &self.policies[&id];
            reports.extend(
                accesses
                    .into_iter()
                    .filter_map(|access| self.report(&id, policy, access)),
            );
            audited.insert(id);
        }
        (reports, audited)
    }

    /// The report Seatbelt's audit profile would make of `access`, if any: a
    /// successful read of a file or write in the workspace that `policy`
    /// reports.
    fn report(
        &self,
        id: &str,
        policy: &Policy,
        access: qk_input_analysis::Access,
    ) -> Option<Report> {
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
        let (kind, is_dir) = if write {
            (policy::Access::Write, false)
        } else {
            (policy::Access::Read, path.is_dir())
        };
        if policy.judge(&path, kind, is_dir) != policy::Verdict::Report {
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

    /// Creates the directories a task's outputs lie in that do not exist
    /// yet, to be removed again if the task leaves them empty, as it starts
    /// under the sandbox in either mode. A tool that makes a directory by
    /// renaming one it built beside it, as cargo makes `target` for an output
    /// in `target/release`, then finds it there rather than writing outside
    /// its outputs. An output itself is not made, as a path names a file or a
    /// directory alike. Failing to create one leaves the task to.
    fn make_outputs(&self, id: &str) {
        let Some(policy) = self.policies.get(id) else {
            return;
        };
        for tree in policy.outputs() {
            let Some(parent) = tree.parent() else {
                continue;
            };
            let missing: Vec<PathBuf> = parent
                .ancestors()
                .take_while(|directory| !directory.exists())
                .map(Path::to_owned)
                .collect();
            if !missing.is_empty() && std::fs::create_dir_all(parent).is_ok() {
                self.created.lock().unwrap().extend(missing);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn landlock(&self, id: &str) -> Result<qk_executor::Confinement> {
        use std::os::fd::AsRawFd;
        let policy = &self.policies[id];
        // Landlock holds paths that exist: an output without an extension is
        // taken for a directory and made too.
        self.make_outputs(id);
        for tree in policy.outputs() {
            if !tree.exists() && tree.extension().is_none() {
                std::fs::create_dir_all(tree)?;
                self.created.lock().unwrap().push(tree.to_owned());
            }
        }
        // A scratch directory is one whatever its name, and is made before
        // the task runs anyway.
        for directory in policy.scratch() {
            std::fs::create_dir_all(directory)?;
        }
        let rules = landlock::ruleset(policy, &self.expansion.lock().unwrap())?;
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
        // Every task has ended, so what they left empty goes, the deepest
        // first, whether or not the reports can be read.
        let mut created = std::mem::take(&mut *self.created.lock().unwrap());
        created.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
        created.dedup();
        for directory in created {
            let _ = std::fs::remove_dir(directory);
        }
        let (reports, audited) = match self.watcher.take() {
            Some(watcher) => {
                let (mut reports, over) = watcher.finish(&self.directory)?;
                // Its reports are incomplete, so it is listed as unaudited
                // rather than with some of what it did.
                for (task, count) in &over {
                    self.unconfined(
                        task,
                        &format!(
                            "it was not audited, as it made {count} sandbox reports, more than the {TASK_REPORTS} kept of one task"
                        ),
                    );
                }
                reports.retain(|report| !over.contains_key(&report.task));
                let audited = self
                    .policies
                    .keys()
                    .filter(|id| !over.contains_key(*id))
                    .cloned()
                    .collect();
                (reports, audited)
            }
            None if self.mode == Mode::Audit => self.recorded(),
            None => (Vec::new(), BTreeSet::new()),
        };
        let _ = std::fs::remove_dir_all(&self.directory);
        let mut by_task: BTreeMap<String, (Vec<Report>, BTreeSet<String>)> = BTreeMap::new();
        for report in reports {
            by_task
                .entry(report.task.clone())
                .or_default()
                .0
                .push(report);
        }
        // Over an overlay, what a task wrote outside its outputs was not
        // kept: a finding as audit would report it.
        for (id, paths) in std::mem::take(&mut *self.discarded.lock().unwrap()) {
            by_task.entry(id).or_default().1.extend(paths);
        }
        // A report naming a task this run does not know is judged against
        // nothing but the workspace.
        let unknown = TaskAudit::new(&self.root, Vec::new());
        let mut findings: BTreeMap<String, Findings> = by_task
            .into_iter()
            .map(|(id, (reports, discarded))| {
                let audit = self.audits.get(&id).unwrap_or(&unknown);
                let found = audit.classify(self.installs.as_ref(), reports, discarded);
                (id, found)
            })
            .collect();
        let unsandboxed = std::mem::take(&mut *self.unsandboxed.lock().unwrap());
        // A task run again unsandboxed was audited only until it failed, so it
        // is listed as unsandboxed rather than with part of its findings.
        findings.retain(|id, findings| !findings.is_empty() && !unsandboxed.contains_key(id));
        let restored = std::mem::take(&mut *self.restored.lock().unwrap());
        let idle = std::mem::take(&mut *self.idle.lock().unwrap());
        let audited = audited
            .into_iter()
            .chain(idle.into_iter().filter(|_| self.mode == Mode::Audit))
            .filter(|id| !unsandboxed.contains_key(id) && !restored.contains(id))
            .collect();
        Ok(Finished {
            findings,
            unsandboxed,
            refused: std::mem::take(&mut *self.refused.lock().unwrap()),
            packages_unchecked: std::mem::take(&mut self.packages_unchecked),
            audited,
            restored,
            fallback: self.fallback.take(),
        })
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.child.kill();
        }
        // An overlay's work directory is one only its owner may enter.
        #[cfg(target_os = "linux")]
        qk_executor::overlay::clear(&self.directory);
        #[cfg(not(target_os = "linux"))]
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// `path` as reports name it, `/`-separated beneath the workspace, whose
/// root `root` is resolved and `given` as given; `None` outside it.
fn workspace_relative(root: &Path, given: &Path, path: &Path) -> Option<String> {
    let relative = path
        .strip_prefix(root)
        .or_else(|_| path.strip_prefix(given))
        .ok()?;
    relative
        .components()
        .filter(|part| *part != std::path::Component::CurDir)
        .map(|part| match part {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
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

/// For each task, every project's root, the deepest first so a path is
/// found in the innermost project that holds it, named where the task's
/// project depends on it, directly or not. A path in a project nested in a
/// dependency that is not one itself, the task's own among them, is no
/// dependency's. The workspace's root project is left out: every path is
/// beneath it.
fn depended(
    workspace: &Workspace,
    graph: &TaskGraph,
    projects: &qk_graph::ProjectGraph,
) -> BTreeMap<String, Vec<(Option<String>, String)>> {
    let mut by_project: BTreeMap<&str, Vec<(Option<String>, String)>> = BTreeMap::new();
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
            let mut roots: Vec<(Option<String>, String)> = workspace
                .projects
                .iter()
                .filter_map(|(name, config)| {
                    // A root is kept as written, so `./libs/lib/` is
                    // `libs/lib`, as reads are named.
                    let root = config
                        .root
                        .split('/')
                        .filter(|part| !part.is_empty() && *part != ".")
                        .collect::<Vec<_>>()
                        .join("/");
                    let depended = seen.contains(name.as_str()).then(|| name.clone());
                    (!root.is_empty()).then_some((depended, root))
                })
                .collect();
            roots.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
            roots
        });
        found.insert(id.clone(), roots.clone());
    }
    found
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
    // Its scratch directories, which it may read and write, and which keep
    // nothing between runs.
    let mut scratch = BTreeSet::new();
    for directory in qk_cache::scratch::directories(workspace, task).unwrap_or_default() {
        if let Ok(relative) = directory.strip_prefix(&workspace.root) {
            write_trees.insert(root.join(relative));
            scratch.insert(root.join(relative));
        }
    }
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
        scratch,
        unlinked: BTreeSet::new(),
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

/// What a task's ignored reads name on disk as the run starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Ignored {
    /// Files a glob matches.
    files: Vec<PathBuf>,
    /// Directories a glob ending in `/**` names whole: everything below one
    /// is matched, so it is named once rather than file by file, which keeps
    /// a profile within what Seatbelt compiles however much they hold.
    trees: Vec<PathBuf>,
}

/// The paths below `root` that `reads` matches as the run starts, whether
/// Git ignores them or not: each glob is walked from the directory its
/// leading literal parts name, a `^` entry from each root of a project it can
/// match in. `.git`, `.qk` and `node_modules` are not walked, and links are
/// left out.
fn matching_paths(root: &Path, reads: &qk_cache::ultracache::IgnoredReads) -> Ignored {
    let (plain, dependency) = reads.walks();
    // The globs ending in `/**`, which match a directory whole when they
    // match a name below it: matched whole, the directory is a directory
    // segment, which an omitted group widens as it would not as the last.
    // One that excludes, from a negated group, can leave out a path deeper
    // down, so it is matched file by file.
    let whole = |written: &[String]| -> Vec<qk_cache::Pattern> {
        written
            .iter()
            .filter(|glob| !glob.starts_with('!') && glob.ends_with("/**"))
            .filter_map(|glob| qk_cache::Pattern::new(glob, false).ok())
            .filter(|glob| !glob.excludes())
            .collect()
    };
    let (whole, whole_within) = (whole(&plain), whole(&dependency));
    // A `^` entry matches all a directory holds only where no other project
    // has its root inside.
    let covers = |relative: &Path| -> bool {
        let probe = relative.join("qk-tree-probe");
        whole.iter().any(|glob| glob.is_match(&probe))
            || (whole_within.iter().any(|glob| glob.is_match(&probe))
                && reads.is_match(&probe)
                && relative
                    .to_str()
                    .is_some_and(|text| !reads.project_within(&text.replace('\\', "/"))))
    };
    let written: Vec<&String> = plain.iter().chain(&dependency).collect();
    // The directories the walk skips, or cannot list, below each directory
    // scanned for them, each scanned once however deep the walk goes.
    let mut scanned: Vec<(PathBuf, HashSet<PathBuf>)> = Vec::new();
    let mut files = BTreeSet::new();
    let mut trees = BTreeSet::new();
    let mut walked = BTreeSet::new();
    for glob in written {
        let base: PathBuf = glob
            .split('/')
            .take_while(|part| !part.contains(['*', '?', '[', ']', '{', '}', '(', ')', '!', '\\']))
            .collect();
        // A base that leaves the workspace names nothing in it.
        if !base.components().all(|part| {
            matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            continue;
        }
        // A link anywhere in the base would lead the walk elsewhere.
        let mut through = root.to_owned();
        let linked = base.components().any(|part| {
            through.push(part);
            std::fs::symlink_metadata(&through).is_ok_and(|metadata| metadata.is_symlink())
        });
        if linked {
            continue;
        }
        let mut pending = vec![root.join(&base)];
        // A path named whole, which a task it depends on may make as the run
        // goes: allowed as it starts, when its rules are opened, if it is
        // there then.
        if base.as_os_str() == glob.as_str() && !root.join(&base).exists() && reads.is_match(&base)
        {
            files.insert(root.join(&base));
        }
        while let Some(path) = pending.pop() {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // A link's target is not what the glob names, and allowing the
            // link would allow its target.
            if metadata.file_type().is_symlink() {
                continue;
            }
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if !metadata.is_dir() {
                if reads.is_match(relative) {
                    files.insert(path);
                }
                continue;
            }
            if skipped(&path) || !walked.insert(path.clone()) {
                continue;
            }
            // Named whole, it would allow what the walk leaves out below it.
            if !relative.as_os_str().is_empty()
                && covers(relative)
                && !holds_skipped(&mut scanned, &path)
            {
                trees.insert(path);
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }
    Ignored {
        files: files
            .into_iter()
            .filter(|file| {
                !file
                    .ancestors()
                    .skip(1)
                    .any(|ancestor| trees.contains(ancestor))
            })
            .collect(),
        trees: trees.into_iter().collect(),
    }
}

/// A directory the walk for ignored reads never enters.
fn skipped(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| matches!(name.to_str(), Some(".git" | ".qk" | "node_modules")))
}

/// Whether `directory` holds, at any depth, one the walk never enters. A
/// directory that cannot be listed whole counts as holding one. A scan
/// keeps, in `scanned`, every directory below where it started that holds
/// one, so a directory below one scanned is answered without listing it
/// again.
fn holds_skipped(scanned: &mut Vec<(PathBuf, HashSet<PathBuf>)>, directory: &Path) -> bool {
    if let Some((_, holding)) = scanned.iter().find(|(top, _)| directory.starts_with(top)) {
        return holding.contains(directory);
    }
    let mut holding = HashSet::new();
    // `found` holds one, and so does each directory between it and the top.
    let mut hold = |found: &Path| {
        for ancestor in found.ancestors() {
            if !ancestor.starts_with(directory) || !holding.insert(ancestor.to_owned()) {
                break;
            }
        }
    };
    let mut pending = vec![directory.to_owned()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            hold(&current);
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                hold(&current);
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                hold(&current);
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let path = entry.path();
            if skipped(&path) {
                hold(&current);
            } else {
                pending.push(path);
            }
        }
    }
    let holds = holding.contains(directory);
    scanned.push((directory.to_owned(), holding));
    holds
}

/// A report from the kernel's log.
pub(crate) struct Report {
    pub task: String,
    pub process: u32,
    pub operation: String,
    pub path: String,
}

impl Report {
    /// Whether it reports a write, rather than a read.
    pub(crate) fn writes(&self) -> bool {
        self.operation.starts_with("file-write")
    }
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
    /// How many reports each task made, kept or not.
    counts: Arc<Mutex<HashMap<String, usize>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

/// The reports kept of one task: beyond them, a task that makes many, as a
/// package manager installing thousands of files outside its outputs, is not
/// audited, rather than crowding out every other task's reports.
pub(crate) const TASK_REPORTS: usize = 100_000;

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
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let (ready, started) = std::sync::mpsc::channel();
        let reader = {
            let reports = reports.clone();
            let sentinel = sentinel.clone();
            let marker = marker.clone();
            let counts = counts.clone();
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
                        let mut counts = counts.lock().unwrap();
                        let count = counts.entry(report.task.clone()).or_insert(0);
                        *count += 1;
                        if *count <= TASK_REPORTS {
                            reports.lock().unwrap().push(report);
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
            counts,
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

    /// The reports kept, and each task that made more than could be kept,
    /// with how many it made.
    pub(crate) fn finish(
        mut self,
        directory: &Path,
    ) -> Result<(Vec<Report>, BTreeMap<String, usize>)> {
        let probed = self.probe(directory, "finish");
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        probed?;
        let over = self
            .counts
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, count)| **count > TASK_REPORTS)
            .map(|(task, count)| (task.clone(), *count))
            .collect();
        Ok((std::mem::take(&mut *self.reports.lock().unwrap()), over))
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
mod matching_tests {
    use super::*;

    #[test]
    fn finds_what_globs_match_below_their_literal_parts() {
        let root = tempfile::tempdir().unwrap();
        for file in [
            "ignored/data.json",
            "ignored/deep/more.json",
            "ignored/node_modules/skip.json",
            "notes.txt",
            "other/data.json",
        ] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                root.path().join("other/data.json"),
                root.path().join("ignored/link.json"),
            )
            .unwrap();
            std::os::unix::fs::symlink(root.path().join("other"), root.path().join("alias"))
                .unwrap();
        }
        let written = [
            "ignored/**/*.json".to_owned(),
            "notes.txt".to_owned(),
            "alias/data.json".to_owned(),
            "../*".to_owned(),
        ];
        let reads = qk_cache::ultracache::IgnoredReads::of_globs(&written).unwrap();
        let found = matching_paths(root.path(), &reads);
        assert!(found.trees.is_empty(), "{found:?}");
        let relative: Vec<_> = found
            .files
            .iter()
            .map(|path| path.strip_prefix(root.path()).unwrap().to_owned())
            .collect();
        assert_eq!(
            relative,
            [
                Path::new("ignored/data.json"),
                Path::new("ignored/deep/more.json"),
                Path::new("notes.txt"),
            ]
        );
    }

    /// A glob ending in `/**` names each directory it matches once, however
    /// much it holds, rather than every file in it.
    #[test]
    fn names_a_directory_a_glob_matches_whole_once() {
        let root = tempfile::tempdir().unwrap();
        for file in [
            "a/.cache/x/1.js",
            "a/.cache/x/2.js",
            "b/c/.cache/3.js",
            "b/keep.json",
            "b/c/other.js",
            "d/axyz/4.js",
            "e/5.js",
        ] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        // An omitted group widens the directory segment, as in Nx.
        let written = [
            "**/.cache/**".to_owned(),
            "b/*.json".to_owned(),
            "d/a?(b)/**".to_owned(),
        ];
        let reads = qk_cache::ultracache::IgnoredReads::of_globs(&written).unwrap();
        let found = matching_paths(root.path(), &reads);
        let relative = |paths: &[PathBuf]| -> Vec<PathBuf> {
            paths
                .iter()
                .map(|path| path.strip_prefix(root.path()).unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            relative(&found.trees),
            [
                PathBuf::from("a/.cache"),
                PathBuf::from("b/c/.cache"),
                PathBuf::from("d/axyz")
            ]
        );
        assert_eq!(relative(&found.files), [PathBuf::from("b/keep.json")]);
    }

    /// A directory holding one the walk never enters, as a nested `.git`, is
    /// not named whole; the directories beside it still are.
    #[test]
    fn names_no_directory_whole_that_holds_a_skipped_one() {
        let root = tempfile::tempdir().unwrap();
        for file in [
            "app/src/main.js",
            "app/.git/HEAD",
            "app/node_modules/dep/index.js",
            "app/keep.json",
            "app/lib/deep/node_modules/dep/index.js",
            "app/lib/deep/other/x.js",
            "app/lib/state/.qk/x",
            "app/lib/plain/y.js",
        ] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let written = ["app/**".to_owned()];
        let reads = qk_cache::ultracache::IgnoredReads::of_globs(&written).unwrap();
        let found = matching_paths(root.path(), &reads);
        assert_eq!(
            found.trees,
            [
                root.path().join("app/lib/deep/other"),
                root.path().join("app/lib/plain"),
                root.path().join("app/src"),
            ]
        );
        assert_eq!(found.files, [root.path().join("app/keep.json")]);
    }

    /// A directory that cannot be listed may hold one the walk never
    /// enters, so its parent is not named whole.
    #[cfg(unix)]
    #[test]
    fn names_no_directory_whole_that_holds_one_it_cannot_list() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for file in ["app/locked/x.js", "app/open/y.js"] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let locked = root.path().join("app/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Where permissions do not hold, as for root, there is nothing to test.
        let held = std::fs::read_dir(&locked).is_err();
        let written = ["app/**".to_owned()];
        let reads = qk_cache::ultracache::IgnoredReads::of_globs(&written).unwrap();
        let found = matching_paths(root.path(), &reads);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if held {
            assert_eq!(found.trees, [root.path().join("app/open")]);
        }
    }

    /// A glob with a negated group names no directory whole: what it leaves
    /// out can lie anywhere below one it matches.
    #[test]
    fn names_no_directory_whole_for_a_glob_that_excludes() {
        let root = tempfile::tempdir().unwrap();
        for file in ["public/shown.js", "public/private/hidden.js"] {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let written = ["**/!(private)/**".to_owned()];
        let reads = qk_cache::ultracache::IgnoredReads::of_globs(&written).unwrap();
        let found = matching_paths(root.path(), &reads);
        assert!(found.trees.is_empty(), "{found:?}");
        assert_eq!(found.files, [root.path().join("public/shown.js")]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loaded file's path, absolute or joined to the root as given, is
    /// named as reports name it; one outside the workspace is not.
    #[test]
    fn names_loaded_files_as_reports_do() {
        let (root, given) = (Path::new("/real/repo"), Path::new("/link/repo"));
        for (path, named) in [
            ("/real/repo/apps/app/.env", Some("apps/app/.env")),
            (
                "/link/repo/./config/variables.txt",
                Some("config/variables.txt"),
            ),
            ("/elsewhere/.env", None),
            ("/real/repo/../other/.env", None),
        ] {
            assert_eq!(
                workspace_relative(root, given, Path::new(path)).as_deref(),
                named,
                "{path}"
            );
        }
    }

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
}
