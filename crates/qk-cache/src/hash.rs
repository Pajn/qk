use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use foldhash::{HashMap, HashMapExt, HashSet};
use qk_config::Workspace;
use qk_executor::{Capture, Display, Outcome, PreparedTask, execute_captured, read_capture};
use qk_graph::ProjectGraph;
use qk_lockfile::Lockfile;
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

use crate::glob::is_literal;
use crate::inputs::{Resolved, manifest_for_dependents, workspace_without_resolution};
use crate::paths::{self, Outputs, Pattern};

pub fn digest_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut reader = File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Metadata that changes whenever a file's content can have changed: size,
/// modification time and, on Unix, change time, inode and mode.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Stamp(Vec<i64>);

impl Stamp {
    fn new(metadata: &std::fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(-1, |duration| duration.as_nanos() as i64);
        #[allow(unused_mut)]
        let mut values = vec![metadata.len() as i64, modified];
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            values.extend([
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.ino() as i64,
                i64::from(metadata.mode()),
            ]);
        }
        Self(values)
    }

    /// Membership stamps also use change time: restoring a directory's mtime
    /// must not allow changes inside a coarse timestamp window to go unnoticed.
    fn membership_settled(&self) -> bool {
        #[cfg(unix)]
        let width = 6;
        #[cfg(not(unix))]
        let width = 2;
        self.0.chunks_exact(width).all(|values| {
            let modified = SystemTime::UNIX_EPOCH + Duration::from_nanos(values[1].max(0) as u64);
            let settled = |time| {
                SystemTime::now()
                    .duration_since(time)
                    .is_ok_and(|age| age > Duration::from_secs(2))
            };
            if values[1] < 0 || !settled(modified) {
                return false;
            }
            #[cfg(unix)]
            {
                let changed = SystemTime::UNIX_EPOCH
                    + Duration::new(values[2].max(0) as u64, values[3].max(0) as u32);
                values[2] >= 0 && settled(changed)
            }
            #[cfg(not(unix))]
            true
        })
    }

    /// A file written within the timestamp resolution window could change again
    /// without changing its stamp, so recent files are always re-read.
    fn settled(&self) -> bool {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_nanos(self.0[1].max(0) as u64);
        self.0[1] >= 0
            && SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > Duration::from_secs(2))
    }
}

/// Digests persisted between runs in the worktree's state, keyed by path and
/// stamp, so a warm run reads only the files that changed. Stamps include the
/// inode, so they belong to one worktree.
fn digests_path(root: &Path) -> PathBuf {
    paths::worktree_state(root).join("digests.json")
}

#[derive(PartialEq, Eq, Hash)]
struct RuntimeKey {
    command: String,
    environment: BTreeMap<OsString, OsString>,
}

/// Workspace state shared by every fingerprint in one run. Inspection uses the
/// initial candidate list; execution fingerprints refresh membership so files
/// added or removed during the run cannot escape input verification.
pub struct Snapshot {
    pub(crate) files: BTreeSet<String>,
    extra_candidates: BTreeSet<String>,
    membership: Mutex<Option<Arc<Membership>>>,
    membership_prefixes: Mutex<HashMap<String, Arc<Vec<PathBuf>>>>,
    source_exclusions: Vec<Outputs>,
    cache_relative: Option<PathBuf>,
    projects: ProjectGraph,
    /// Each project by its root, to find the project owning a file.
    roots: BTreeMap<String, String>,
    generated_tasks: Vec<(Task, Outputs)>,
    task_dependencies: BTreeMap<String, BTreeSet<String>>,
    source_ignore: Arc<SourceIgnore>,
    canonical_root: PathBuf,
    workspace_prefix: Option<PathBuf>,
    patterns: Mutex<HashMap<(String, bool), Arc<Pattern>>>,
    digests: Mutex<HashMap<String, (Stamp, String)>>,
    /// Whether `digests` gained entries worth saving.
    digests_changed: std::sync::atomic::AtomicBool,
    /// Directories already checked not to be symlinks.
    directories: Mutex<HashSet<PathBuf>>,
    /// Runtime input results by command and environment, computed once per run like Nx.
    runtime: Mutex<HashMap<RuntimeKey, Arc<Mutex<Option<Value>>>>>,
    /// The parsed pnpm lockfile, replaced whenever its content changes.
    lockfile: Mutex<Option<Arc<Installed>>>,
    /// Each dependency task's output files, listed when a task first asks and
    /// dropped once a task whose outputs overlap them is done.
    dependency_outputs: Mutex<HashMap<String, Arc<DependencyOutputs>>>,
    /// Counts drops, so a listing taken while a task finished is not kept.
    outputs_written: std::sync::atomic::AtomicU64,
}

/// Membership depends on directory entries and Git/ignore listing policy, not
/// on every source file's contents. Empty directories must also be watched.
#[derive(Clone)]
struct Membership {
    files: Arc<BTreeSet<String>>,
    source_ignore: Arc<SourceIgnore>,
    stamps: BTreeMap<PathBuf, Option<Stamp>>,
    directories: BTreeSet<PathBuf>,
    directory_entries: BTreeMap<PathBuf, BTreeSet<(OsString, u8)>>,
    root: PathBuf,
    outputs: Vec<Outputs>,
    output_anchors: BTreeMap<PathBuf, Vec<usize>>,
    excluded_roots: BTreeSet<PathBuf>,
    cache: Option<PathBuf>,
    policies: BTreeSet<PathBuf>,
    git_policies: BTreeSet<PathBuf>,
    settled: bool,
}

/// A candidate listing and the ignore policy captured with it. Keeping both
/// alive prevents a concurrent refresh from changing dependency-output selection.
#[derive(Clone)]
struct Candidates {
    files: Arc<BTreeSet<String>>,
    source_ignore: Arc<SourceIgnore>,
}

impl Membership {
    /// Inventory source directories and policy stamps before a new listing.
    fn new(
        root: &Path,
        files: &BTreeSet<String>,
        source_ignore: &Arc<SourceIgnore>,
        outputs: &[Outputs],
        cache: Option<&Path>,
        git_policies: Option<&BTreeSet<PathBuf>>,
    ) -> Result<Self> {
        let excluded: BTreeSet<_> = outputs
            .iter()
            .flat_map(Outputs::complete_roots)
            .map(PathBuf::from)
            .collect();
        let excluded_roots = excluded.clone();
        let walk_root = root.to_owned();
        let walk_cache = cache.map(PathBuf::from);
        let mut directories = BTreeSet::new();
        let mut stamps = BTreeMap::new();
        for entry in ignore::WalkBuilder::new(root)
            .hidden(false)
            .parents(false)
            .git_global(false)
            .require_git(false)
            .ignore(false)
            .git_ignore(false)
            .git_exclude(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                let relative = entry
                    .path()
                    .strip_prefix(&walk_root)
                    .unwrap_or(entry.path());
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".qk" | "node_modules")
                ) && walk_cache
                    .as_ref()
                    .is_none_or(|cache| !relative.starts_with(cache))
                    && !relative.ancestors().any(|path| excluded.contains(path))
            })
            .build()
        {
            let entry = entry?;
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                let directory = entry.path().to_owned();
                stamps.insert(directory.clone(), membership_stamp(&directory)?);
                directories.insert(directory);
            }
        }
        // Git lists tracked files even beneath ignored directories. Watching
        // only the ignore walker would miss deletion/rename of those files.
        for path in files {
            for parent in Path::new(path).ancestors().skip(1) {
                let directory = root.join(parent);
                if directories.insert(directory.clone()) {
                    stamps.insert(directory.clone(), membership_stamp(&directory)?);
                }
            }
        }
        let mut policies = BTreeSet::new();
        for directory in &directories {
            for name in [".gitignore", ".nxignore"] {
                let path = directory.join(name);
                // Creation of a new ignore file changes its directory stamp.
                // Existing rules can be edited without changing the directory.
                if membership_stamp(&path)?.is_some() {
                    policies.insert(path);
                }
            }
        }
        for directory in root.ancestors() {
            policies.insert(directory.join(".gitignore"));
            // Also detect a newly initialized repository or changed worktree pointer.
            policies.insert(directory.join(".git"));
        }
        let git_policies = git_policies
            .cloned()
            .unwrap_or_else(|| git_membership_policies(root));
        policies.extend(git_policies.iter().cloned());
        for policy in &policies {
            stamps.insert(policy.clone(), membership_stamp(policy)?);
        }
        let mut output_anchors: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for (index, outputs) in outputs.iter().enumerate() {
            for anchor in outputs.anchors() {
                output_anchors
                    .entry(PathBuf::from(anchor))
                    .or_default()
                    .push(index);
            }
        }
        let mut membership = Self {
            files: Arc::new(BTreeSet::new()),
            source_ignore: source_ignore.clone(),
            stamps,
            directories,
            directory_entries: BTreeMap::new(),
            root: root.to_owned(),
            outputs: outputs.to_vec(),
            output_anchors,
            excluded_roots,
            cache: cache.map(PathBuf::from),
            policies,
            git_policies,
            settled: false,
        };
        for directory in &membership.directories {
            if let Some(entries) = membership.entries(directory)? {
                membership
                    .directory_entries
                    .insert(directory.clone(), entries);
            }
        }
        Ok(membership)
    }

    /// Names and types that can affect source membership in this directory.
    /// Output files may be skipped, but partially excluded output directories
    /// must remain visible because they can contain source files or ignore rules.
    fn entries(&self, directory: &Path) -> Result<Option<BTreeSet<(OsString, u8)>>> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let relative_directory = directory.strip_prefix(&self.root).unwrap_or(directory);
        let mut outputs = BTreeSet::new();
        for ancestor in relative_directory.ancestors() {
            if let Some(indices) = self.output_anchors.get(ancestor) {
                outputs.extend(indices.iter().copied());
            }
        }
        for (_, indices) in self
            .output_anchors
            .range(relative_directory.to_owned()..)
            .take_while(|(anchor, _)| anchor.starts_with(relative_directory))
        {
            outputs.extend(indices.iter().copied());
        }
        let mut names = BTreeSet::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let path = entry.path();
            let relative = path.strip_prefix(&self.root).unwrap_or(&path);
            if matches!(name.to_str(), Some(".git" | ".qk" | "node_modules"))
                || self
                    .cache
                    .as_ref()
                    .is_some_and(|cache| relative.starts_with(cache))
                || relative
                    .ancestors()
                    .any(|root| self.excluded_roots.contains(root))
            {
                continue;
            }
            // A policy created between policy discovery and name capture must
            // get its own stamp before its in-place edits can permit reuse.
            if matches!(name.to_str(), Some(".gitignore" | ".nxignore"))
                && !self.policies.contains(&path)
            {
                return Ok(None);
            }
            let kind = entry.file_type()?;
            // An empty directory created after the inventory walk would not
            // appear among source files. Never bless its parent signature until
            // that directory is itself watched by the inventory.
            if kind.is_dir() && !self.directories.contains(&path) {
                return Ok(None);
            }
            if !kind.is_dir()
                && !matches!(name.to_str(), Some(".gitignore" | ".nxignore"))
                && relative.to_str().is_some_and(|path| {
                    outputs
                        .iter()
                        .any(|index| self.outputs[*index].matches(path))
                })
            {
                continue;
            }
            names.insert((
                name,
                if kind.is_dir() {
                    1
                } else if kind.is_symlink() {
                    2
                } else {
                    0
                },
            ));
        }
        Ok(Some(names))
    }

    /// Check source-relevant directory membership and every listing policy.
    /// Recent directory stamps require fresh entry names; policies must settle.
    fn state(&self, prefixes: Option<&[PathBuf]>) -> Result<(bool, bool)> {
        let mut settled = true;
        let selected = prefixes.map(|prefixes| {
            let mut selected = BTreeSet::new();
            for prefix in prefixes {
                for ancestor in prefix.ancestors() {
                    if let Some(directory) = self.directories.get(ancestor) {
                        selected.insert(directory);
                    }
                }
                selected.extend(
                    self.directories
                        .range(prefix.clone()..)
                        .take_while(|directory| directory.starts_with(prefix)),
                );
            }
            selected
        });
        let paths: Box<dyn Iterator<Item = (&PathBuf, bool)>> = match selected {
            Some(selected) => Box::new(
                selected
                    .into_iter()
                    .map(|path| (path, true))
                    .chain(self.policies.iter().map(|path| (path, false))),
            ),
            None => Box::new(
                self.stamps
                    .keys()
                    .map(|path| (path, self.directories.contains(path))),
            ),
        };
        for (path, directory) in paths {
            let before = &self.stamps[path];
            let after = membership_stamp(path)?;
            if directory {
                if after.is_some() && !self.directory_entries.contains_key(path) {
                    return Ok((false, false));
                }
                // Output publication changes its parent's timestamp too. Check
                // source-relevant names rather than relisting the whole workspace.
                // Recent directories are always read, even with an equal stamp:
                // coarse timestamps can conceal an addition or removal.
                if *before != after
                    || after
                        .as_ref()
                        .is_some_and(|stamp| !stamp.membership_settled())
                {
                    let entries = self.entries(path)?;
                    if entries.as_ref() != self.directory_entries.get(path)
                        || membership_stamp(path)? != after
                    {
                        return Ok((false, false));
                    }
                }
            } else {
                if *before != after {
                    return Ok((false, false));
                }
                settled &= after.as_ref().is_none_or(Stamp::membership_settled);
            }
        }
        Ok((true, settled))
    }

    /// Retain a matched listing and policy for use after releasing the lock.
    fn candidates(&self) -> Candidates {
        Candidates {
            files: self.files.clone(),
            source_ignore: self.source_ignore.clone(),
        }
    }
}

/// Find Git index, exclusion and config files that can change source visibility,
/// including absent policy files whose later creation must invalidate reuse.
fn git_membership_policies(root: &Path) -> BTreeSet<PathBuf> {
    let mut policies = BTreeSet::new();
    if let Ok(output) = paths::git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
            "--git-path",
            "info/exclude",
            "--git-path",
            "config",
            "--git-path",
            "config.worktree",
        ],
    ) && output.status.success()
        && let Ok(paths) = String::from_utf8(output.stdout)
    {
        policies.extend(paths.lines().map(PathBuf::from));
    }
    // Config can include other files. Track every origin used by Git, as
    // well as currently missing standard config and excludes files.
    if let Ok(output) = paths::git(root, &["config", "--null", "--show-origin", "--list"])
        && output.status.success()
    {
        // NUL mode emits raw origins, without Git's C-style path quoting,
        // and separates each origin from its key/newline/value record. Values
        // may themselves contain newlines (including include/excludes paths).
        let output = String::from_utf8_lossy(&output.stdout);
        let mut fields = output.split_terminator('\0');
        while let (Some(origin), Some(entry)) = (fields.next(), fields.next()) {
            if let Some(path) = origin.strip_prefix("file:") {
                let origin = root.join(path);
                if let Some((name, value)) = entry.split_once('\n')
                    && name.starts_with("include")
                    && name.ends_with(".path")
                {
                    let included = if let Some(relative) = value.strip_prefix("~/") {
                        std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .unwrap_or_default()
                            .join(relative)
                    } else {
                        origin.parent().unwrap_or(root).join(value)
                    };
                    // Git omits missing includes from the origin listing, but
                    // creating one later can change core.excludesfile.
                    policies.insert(included);
                }
                policies.insert(origin);
            }
        }
    }
    if let Ok(output) = paths::git(
        root,
        &["config", "--null", "--path", "--get", "core.excludesfile"],
    ) && output.status.success()
        && let Ok(path) = String::from_utf8(output.stdout)
    {
        policies.insert(root.join(path.strip_suffix('\0').unwrap_or(&path)));
    }
    for variable in ["GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        if let Some(path) = std::env::var_os(variable) {
            policies.insert(PathBuf::from(path));
        }
    }
    policies.insert(PathBuf::from("/etc/gitconfig"));
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        policies.insert(home.join(".gitconfig"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        policies.insert(config.join("git/config"));
        policies.insert(config.join("git/ignore"));
    }
    policies
}

/// Stamp a directory or listing policy, including a symlink's resolved target.
fn membership_stamp(path: &Path) -> Result<Option<Stamp>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut stamp = Stamp::new(&metadata);
            if metadata.is_symlink() {
                // Follow the current chain on every check. In-place target
                // edits or retargeted intermediate links can change policy.
                match std::fs::metadata(path) {
                    Ok(target) => stamp.0.extend(Stamp::new(&target).0),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => stamp.0[1] = -1,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some(stamp))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// One dependency task's output files, and the value of each one a pattern
/// has selected so far.
struct DependencyOutputs {
    anchors: Vec<String>,
    files: Vec<String>,
    values: Mutex<HashMap<String, Value>>,
}

/// One revision of `pnpm-lock.yaml`, with digests of what it installs computed
/// once and shared by every task that asks.
struct Installed {
    content: String,
    /// `None` when the file cannot be read as a pnpm v9 lockfile.
    lockfile: Option<Lockfile>,
    digests: Mutex<HashMap<String, String>>,
}

impl Installed {
    fn digest(&self, key: &str, fingerprints: impl FnOnce() -> BTreeSet<String>) -> String {
        if let Some(digest) = self.digests.lock().unwrap().get(key) {
            return digest.clone();
        }
        let mut hasher = blake3::Hasher::new();
        for fingerprint in fingerprints() {
            hasher.update(fingerprint.as_bytes());
            hasher.update(b"\n");
        }
        let digest = hasher.finalize().to_hex().to_string();
        self.digests
            .lock()
            .unwrap()
            .insert(key.to_owned(), digest.clone());
        digest
    }
}

impl Snapshot {
    pub fn new(workspace: &Workspace, graph: &TaskGraph, cache_path: &Path) -> Result<Self> {
        // Every early task needs these, so loading saved digests and parsing
        // the lockfile overlap listing files.
        let canonical_root = workspace.root.canonicalize()?;
        let (files, lockfile, (digests, workspace_prefix)) = std::thread::scope(|scope| {
            let lockfile = scope.spawn(|| parse_lockfile(&workspace.root.join("pnpm-lock.yaml")));
            let digests = scope.spawn(|| {
                let digests = load_digests(&workspace.root);
                let workspace_prefix = paths::git_path(&workspace.root, "--show-toplevel")
                    .and_then(|root| root.canonicalize().ok())
                    .and_then(|root| canonical_root.strip_prefix(root).ok().map(PathBuf::from));
                (digests, workspace_prefix)
            });
            let files = source_files(&workspace.root);
            (
                files,
                lockfile.join().expect("lockfile thread panicked"),
                digests.join().expect("digest thread panicked"),
            )
        });
        let mut files = files?;
        let cache_relative = cache_path
            .strip_prefix(&workspace.root)
            .ok()
            .map(PathBuf::from);
        // Keep the patterns, not only the artifacts present at startup: new
        // outputs and warm scratch files must be excluded on every refresh.
        let mut source_exclusions: Vec<_> = graph
            .tasks
            .values()
            .filter_map(|task| Outputs::new(workspace, task).ok())
            .filter(Outputs::is_explicit)
            .collect();
        for task in graph.tasks.values() {
            let Ok(entries) = crate::warm::config(workspace, task) else {
                continue;
            };
            if let Ok(paths) = Outputs::from_paths(&crate::warm::kept_paths(&entries)) {
                source_exclusions.push(paths);
            }
        }
        exclude_artifacts(&mut files, &source_exclusions, cache_relative.as_deref());
        Ok(Self {
            files,
            extra_candidates: BTreeSet::new(),
            membership: Mutex::default(),
            membership_prefixes: Mutex::default(),
            source_exclusions,
            cache_relative,
            projects: ProjectGraph::build(workspace)?,
            roots: workspace
                .projects
                .values()
                .map(|project| (project.root.clone(), project.name.clone()))
                .collect(),
            source_ignore: Arc::new(SourceIgnore::new(&workspace.root)?),
            generated_tasks: graph
                .tasks
                .values()
                .filter_map(|task| {
                    let outputs = Outputs::new(workspace, task).ok()?;
                    outputs.is_explicit().then(|| (task.clone(), outputs))
                })
                .collect(),
            task_dependencies: graph
                .tasks
                .iter()
                .map(|(id, task)| (id.clone(), task.dependencies.clone()))
                .collect(),
            canonical_root,
            workspace_prefix,
            patterns: Mutex::default(),
            digests: Mutex::new(digests),
            digests_changed: Default::default(),
            directories: Mutex::default(),
            runtime: Mutex::default(),
            lockfile: Mutex::new(lockfile),
            dependency_outputs: Mutex::default(),
            outputs_written: Default::default(),
        })
    }

    /// Restrict directory checks to the positive fileset prefixes a task reads.
    fn membership_prefixes(&self, workspace: &Workspace, task: &Task) -> Result<Arc<Vec<PathBuf>>> {
        let mut prefixes = self.membership_prefixes.lock().unwrap();
        if let Some(prefixes) = prefixes.get(&task.id) {
            return Ok(prefixes.clone());
        }
        let empty = BTreeSet::new();
        let cancelled = AtomicBool::new(false);
        let mut resolver = Resolver::new(self, workspace, None, &cancelled, &empty);
        resolver.task_inputs(task)?;
        let paths = Arc::new(
            resolver
                .scopes
                .values()
                .flat_map(|scope| &scope.patterns)
                .filter(|pattern| !pattern.excluded)
                .map(|pattern| workspace.root.join(&pattern.prefix))
                .collect(),
        );
        prefixes.insert(task.id.clone(), Arc::clone(&paths));
        Ok(paths)
    }

    /// Reuse membership while source directories and listing policy are unchanged.
    /// Content changes are checked separately by the file digest cache.
    #[cfg(test)]
    fn current_files(&self, root: &Path) -> Result<Arc<BTreeSet<String>>> {
        Ok(self.current_files_for(root, None)?.files)
    }

    /// Refresh candidates as needed, returning their policy in the same snapshot.
    fn current_files_for(&self, root: &Path, prefixes: Option<&[PathBuf]>) -> Result<Candidates> {
        loop {
            let previous = self.membership.lock().unwrap().clone();
            // Immutable entries let independent fingerprints validate their
            // directory stamps concurrently. Only a refresh needs serialization.
            let (unchanged, settled) = match &previous {
                Some(entry) => entry.state(prefixes)?,
                None => (false, false),
            };
            if settled && previous.as_ref().unwrap().settled {
                return Ok(previous.as_ref().unwrap().candidates());
            }
            let mut cached = self.membership.lock().unwrap();
            let current = match (&previous, &*cached) {
                (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
                (None, None) => true,
                _ => false,
            };
            if !current {
                // Another fingerprint refreshed while this one validated.
                // Recheck that newer inventory before making any replacement.
                continue;
            }
            drop(previous);
            // Recent listing policies may conceal edits on coarse filesystems.
            // Re-list sources while they settle, then rebuild the directory inventory
            // once before enabling reuse (including any newly created empty dirs).
            // Recent directories are checked by freshly reading their child names.
            let rebuilt = !unchanged || settled;
            let mut membership = if !rebuilt {
                Arc::unwrap_or_clone(cached.take().unwrap())
            } else {
                // Capture directories before listing files, then compare afterward.
                // An addition during listing must not hide behind a newer stamp.
                Membership::new(
                    root,
                    cached.as_ref().map_or(&self.files, |entry| &entry.files),
                    &self.source_ignore,
                    &self.source_exclusions,
                    self.cache_relative.as_deref(),
                    cached
                        .as_ref()
                        .filter(|entry| unchanged && entry.settled)
                        .map(|entry| &entry.git_policies),
                )?
            };
            let mut files = source_files(root)?;
            exclude_artifacts(
                &mut files,
                &self.source_exclusions,
                self.cache_relative.as_deref(),
            );
            let ignore = SourceIgnore::new(root)?;
            files.extend(
                self.extra_candidates
                    .iter()
                    .filter(|path| !ignore.matches(path))
                    .cloned(),
            );
            membership.source_ignore = Arc::new(ignore);
            membership.files = Arc::new(files);
            let candidates = membership.candidates();
            // An unstable inventory or recent policy never enables the fast path.
            // Keep candidates so a rebuild also watches tracked, ignored parents.
            let covered = candidates.files.iter().all(|path| {
                Path::new(path)
                    .ancestors()
                    .skip(1)
                    .all(|parent| membership.stamps.contains_key(&root.join(parent)))
            });
            membership.settled = rebuilt && covered && membership.state(None)?.1;
            *cached = Some(Arc::new(membership));
            return Ok(candidates);
        }
    }

    /// Saves the digests of files that still exist, for the next run.
    pub fn save_digests(&self, root: &Path) {
        if !self
            .digests_changed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let digests = self.digests.lock().unwrap();
        let entries: BTreeMap<&String, (&Stamp, &String)> = digests
            .iter()
            .filter(|(path, _)| self.files.contains(*path))
            .map(|(path, (stamp, digest))| (path, (stamp, digest)))
            .collect();
        let path = digests_path(root);
        let saved = (|| -> Result<()> {
            let directory = path.parent().context("digests have a directory")?;
            std::fs::create_dir_all(directory)?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            {
                let mut writer = std::io::BufWriter::new(file.as_file_mut());
                serde_json::to_writer(&mut writer, &json!({"version": 1, "entries": entries}))?;
                writer.flush()?;
            }
            file.persist(&path)?;
            Ok(())
        })();
        if let Err(error) = saved {
            qk_executor::status!("qk: could not save file digests: {error:#}");
        }
    }

    /// `paths::safe_parents`, checking each directory once per run.
    fn safe_parents(&self, root: &Path, path: &str) -> Result<()> {
        paths::validate_path(path)?;
        let mut current = root.to_owned();
        for part in Path::new(path)
            .parent()
            .into_iter()
            .flat_map(Path::components)
        {
            current.push(part);
            if self.directories.lock().unwrap().contains(&current) {
                continue;
            }
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("input parent is a symlink: {}", current.display())
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!("input parent is not a directory: {}", current.display())
                }
                Ok(_) => {
                    self.directories.lock().unwrap().insert(current.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn installed(&self, root: &Path) -> Result<Option<Arc<Installed>>> {
        let absolute = root.join("pnpm-lock.yaml");
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        let content = self.digest("pnpm-lock.yaml", &absolute, &metadata)?;
        let mut current = self.lockfile.lock().unwrap();
        if let Some(installed) = &*current
            && installed.content == content
        {
            return Ok(Some(installed.clone()));
        }
        let Some(installed) = parse_lockfile(&absolute) else {
            return Ok(None);
        };
        *current = Some(installed.clone());
        Ok(Some(installed))
    }

    /// Adds paths to the candidate files, such as files deleted since a base
    /// revision.
    pub fn with_candidates(mut self, paths: &[String]) -> Self {
        self.files.extend(
            paths
                .iter()
                .filter(|path| !self.source_ignore.matches(path))
                .cloned(),
        );
        self.extra_candidates.extend(paths.iter().cloned());
        self
    }

    fn pattern(&self, pattern: &str, negated: bool) -> Result<Arc<Pattern>> {
        let key = (pattern.to_owned(), negated);
        if let Some(pattern) = self.patterns.lock().unwrap().get(&key) {
            return Ok(pattern.clone());
        }
        let compiled = Arc::new(Pattern::new(pattern, negated)?);
        self.patterns.lock().unwrap().insert(key, compiled.clone());
        Ok(compiled)
    }

    fn digest(&self, path: &str, absolute: &Path, metadata: &std::fs::Metadata) -> Result<String> {
        let stamp = Stamp::new(metadata);
        if let Some((known, digest)) = self.digests.lock().unwrap().get(path)
            && *known == stamp
        {
            return Ok(digest.clone());
        }
        let digest = digest_file(absolute)?;
        // Re-check after reading so a concurrent write is never recorded as settled.
        let after = Stamp::new(&std::fs::symlink_metadata(absolute)?);
        if after == stamp && stamp.settled() {
            self.digests
                .lock()
                .unwrap()
                .insert(path.to_owned(), (stamp, digest.clone()));
            self.digests_changed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(digest)
    }

    /// `task`'s output files that are not directories, as listed since the
    /// last task that could have written them was done.
    fn dependency_outputs(
        &self,
        workspace: &Workspace,
        task: &Task,
    ) -> Result<Arc<DependencyOutputs>> {
        if let Some(outputs) = self.dependency_outputs.lock().unwrap().get(&task.id) {
            return Ok(outputs.clone());
        }
        let written = self
            .outputs_written
            .load(std::sync::atomic::Ordering::SeqCst);
        let outputs = Arc::new(self.fresh_dependency_outputs(workspace, task)?);
        let mut listed = self.dependency_outputs.lock().unwrap();
        if self
            .outputs_written
            .load(std::sync::atomic::Ordering::SeqCst)
            != written
        {
            return Ok(outputs);
        }
        Ok(listed.entry(task.id.clone()).or_insert(outputs).clone())
    }

    /// Re-list artifacts for verification, without reusing earlier content values.
    fn fresh_dependency_outputs(
        &self,
        workspace: &Workspace,
        task: &Task,
    ) -> Result<DependencyOutputs> {
        let declared = Outputs::new(workspace, task)?;
        let mut files = Vec::new();
        for entry in declared.entries(&workspace.root)? {
            let (path, metadata) = entry?;
            if !metadata.is_dir() {
                files.push(path);
            }
        }
        Ok(DependencyOutputs {
            anchors: declared.anchors().map(str::to_owned).collect(),
            files,
            values: Mutex::default(),
        })
    }

    /// Drops the listings `task` may have changed, once it is done. Without
    /// its outputs, every listing goes.
    pub(crate) fn outputs_written(&self, workspace: &Workspace, task: &Task) {
        let anchors: Option<Vec<String>> = Outputs::new(workspace, task)
            .ok()
            .filter(Outputs::is_explicit)
            .map(|outputs| outputs.anchors().map(str::to_owned).collect());
        let overlap = |a: &str, b: &str| {
            a == b
                || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
                || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        };
        let mut listed = self.dependency_outputs.lock().unwrap();
        self.outputs_written
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        listed.retain(|_, outputs| {
            anchors.as_ref().is_some_and(|written| {
                !written
                    .iter()
                    .any(|a| outputs.anchors.iter().any(|b| overlap(a, b)))
            })
        });
    }

    fn file_value(&self, root: &Path, path: &str) -> Result<Value> {
        self.file_value_with_links(root, path, &mut BTreeSet::new())
    }

    fn file_value_with_links(
        &self,
        root: &Path,
        path: &str,
        visiting: &mut BTreeSet<PathBuf>,
    ) -> Result<Value> {
        self.safe_parents(root, path)?;
        let absolute = root.join(path);
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)?;
            // A dangling link is keyed as such, so the key changes once it resolves.
            let Ok(resolved) = absolute.canonicalize() else {
                return Ok(json!({"link":target, "dangling":true}));
            };
            let Ok(relative) = resolved.strip_prefix(&self.canonical_root) else {
                bail!("input symlink resolves outside the workspace: {path}");
            };
            if resolved.is_file() {
                let metadata = std::fs::metadata(&resolved)?;
                let key = paths::relative(Path::new(""), relative)?;
                return Ok(
                    json!({"link":target, "content":self.digest(&key, &resolved, &metadata)?,
                    "mode":crate::store::mode(&metadata) & 0o111 != 0}),
                );
            }
            let relative = paths::relative(Path::new(""), relative)?;
            if relative.is_empty() {
                bail!("input symlink resolves to the workspace root: {path}");
            }
            if !visiting.insert(resolved.clone()) {
                bail!("directory input symlink cycle: {path}");
            }
            let mut contents = BTreeMap::new();
            // Selecting the link explicitly reads its target, even when source
            // discovery ignores that target or another link is nested inside it.
            for entry in walkdir::WalkDir::new(&resolved).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_dir() {
                    continue;
                }
                let file = paths::relative(&self.canonical_root, entry.path())?;
                let value = self.file_value_with_links(&self.canonical_root, &file, visiting)?;
                contents.insert(
                    file.strip_prefix(&relative).unwrap_or(&file).to_owned(),
                    value,
                );
            }
            visiting.remove(&resolved);
            return Ok(json!({"link":target, "directory":contents}));
        }
        if !metadata.is_file() {
            bail!("input is not a regular file: {path}");
        }
        // Only the executable bit, which is all git records: other permission
        // bits follow the checkout's umask and would keep machines apart.
        let mode = crate::store::mode(&metadata) & 0o111 != 0;
        Ok(json!({"content":self.digest(path, &absolute, &metadata)?, "mode":mode}))
    }
}

/// Reads and parses a pnpm lockfile, `None` when there is none. One qk cannot
/// read is reported, and kept as unreadable so tasks key its whole content.
fn parse_lockfile(path: &Path) -> Option<Arc<Installed>> {
    let text = std::fs::read_to_string(path).ok()?;
    let lockfile = match Lockfile::parse(&text) {
        Ok(lockfile) => Some(lockfile),
        Err(error) => {
            qk_executor::status!("qk: pnpm-lock.yaml: keying tasks by the whole file ({error:#})");
            None
        }
    };
    Some(Arc::new(Installed {
        content: blake3::hash(text.as_bytes()).to_hex().to_string(),
        lockfile,
        digests: Mutex::default(),
    }))
}

fn load_digests(root: &Path) -> HashMap<String, (Stamp, String)> {
    #[derive(serde::Deserialize)]
    struct Saved {
        version: u32,
        entries: HashMap<String, (Stamp, String)>,
    }
    std::fs::read(digests_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        .filter(|saved| saved.version == 1)
        .map(|saved| saved.entries)
        .unwrap_or_default()
}

/// Apply the same source policy to startup and refreshed file membership.
/// Remove declared artifacts and cache storage from a refreshed source listing.
fn exclude_artifacts(files: &mut BTreeSet<String>, outputs: &[Outputs], cache: Option<&Path>) {
    let mut generated = BTreeSet::new();
    for outputs in outputs {
        for anchor in outputs.anchors() {
            generated.extend(
                under(files, anchor)
                    .filter(|path| outputs.matches(path))
                    .cloned(),
            );
        }
    }
    files.retain(|path| {
        !generated.contains(path) && cache.is_none_or(|cache| !Path::new(path).starts_with(cache))
    });
}

/// Files equal to `prefix` or below it, using the sorted order of the set.
impl Snapshot {
    /// The project whose root most specifically contains `path`, as Nx assigns
    /// files to projects.
    fn owner(&self, path: &str) -> Option<&str> {
        let mut path = path;
        loop {
            if let Some(project) = self.roots.get(path) {
                return Some(project);
            }
            match path.rfind('/') {
                Some(slash) => path = &path[..slash],
                None if path != "." => path = ".",
                None => return None,
            }
        }
    }
}

fn under<'a>(files: &'a BTreeSet<String>, prefix: &'a str) -> impl Iterator<Item = &'a String> {
    files
        .range::<str, _>((
            std::ops::Bound::Included(prefix),
            std::ops::Bound::Unbounded,
        ))
        .take_while(move |path| path.starts_with(prefix))
        .filter(move |path| path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

/// Compiled root ignore rules used by input discovery and project watching.
pub struct SourceIgnore(ignore::gitignore::Gitignore);

impl SourceIgnore {
    /// Root ignore rules shared by caching and watch, including tracked files.
    pub fn new(root: &Path) -> Result<Self> {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        for name in [".gitignore", ".nxignore"] {
            let path = root.join(name);
            if path.is_file()
                && let Some(error) = builder.add(path)
            {
                return Err(error.into());
            }
        }
        Ok(Self(builder.build()?))
    }

    fn matches_directory(&self, path: &str) -> bool {
        self.matches(path) || self.0.matched(path, true).is_ignore()
    }

    /// Whether a workspace-relative file or its parent is ignored.
    pub fn matches(&self, path: &str) -> bool {
        // A file whitelist cannot reinclude it beneath an excluded directory.
        Path::new(path)
            .ancestors()
            .skip(1)
            .take_while(|parent| !parent.as_os_str().is_empty())
            .any(|parent| self.0.matched(parent, true).is_ignore())
            || self.0.matched(path, false).is_ignore()
    }
}

/// Source files visible to Nx, with runner state and root ignore rules removed.
pub fn source_files(root: &Path) -> Result<BTreeSet<String>> {
    let lines =
        |output: std::io::Result<std::process::Output>| -> Option<Result<BTreeSet<String>>> {
            let output = output.ok().filter(|output| output.status.success())?;
            Some(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| {
                        String::from_utf8(path.to_vec()).context("input paths must be UTF-8")
                    })
                    .collect(),
            )
        };
    // Tracked files deleted from the working tree are listed as cached; git
    // names them in a second listing, which is cheaper than checking each file.
    // Skip-worktree entries are listed whether or not they are on disk and never
    // as deleted, so those few are checked; `-v` tags them `S`.
    let (listed, deleted) = if root.join(".nxignore").is_file() {
        // Nx negations can reinclude Git-ignored files, so this policy needs
        // the walker regardless of what Git lists. Avoid launching Git here.
        (None, None)
    } else {
        std::thread::scope(|scope| {
            let deleted =
                scope.spawn(|| paths::git(root, &["ls-files", "--deleted", "-z", "--", "."]));
            let listed = paths::git(
                root,
                &[
                    "ls-files",
                    "-v",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                ],
            );
            (
                lines(listed),
                lines(deleted.join().expect("git listing thread panicked")),
            )
        })
    };
    let candidates = match (listed, deleted) {
        (Some(listed), Some(deleted)) => {
            let deleted = deleted?;
            let mut files = BTreeSet::new();
            for entry in listed? {
                let Some((tag, path)) = entry.split_once(' ') else {
                    bail!("unexpected git ls-files entry {entry:?}");
                };
                let absent =
                    matches!(tag, "S" | "s") && std::fs::symlink_metadata(root.join(path)).is_err();
                if !absent && !deleted.contains(path) {
                    files.insert(path.to_owned());
                }
            }
            files
        }
        _ => {
            let mut files = BTreeSet::new();
            for entry in ignore::WalkBuilder::new(root)
                .hidden(false)
                .parents(false)
                .git_global(false)
                .require_git(false)
                .ignore(false)
                .add_custom_ignore_filename(".nxignore")
                .follow_links(false)
                .filter_entry(|entry| {
                    !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | ".qk" | "node_modules")
                    )
                })
                .build()
            {
                let entry = entry?;
                if entry.file_type().is_some_and(|kind| !kind.is_dir()) {
                    files.insert(paths::relative(root, entry.path())?);
                }
            }
            files
        }
    };
    let ignore = SourceIgnore::new(root)?;
    Ok(candidates
        .into_iter()
        .filter(|path| {
            !ignore.matches(path)
                && !path
                    .split('/')
                    .any(|part| matches!(part, ".git" | ".qk" | "node_modules"))
        })
        .collect())
}

fn json_path<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.')
        .try_fold(value, |value, part| value.get(part))
}

fn set_json_path(target: &mut Value, path: &str, value: Value) {
    let mut parts = path.split('.').peekable();
    let mut current = target;
    while let Some(part) = parts.next() {
        let Value::Object(object) = current else {
            return;
        };
        if parts.peek().is_none() {
            object.insert(part.to_owned(), value);
            return;
        }
        current = object.entry(part).or_insert_with(|| json!({}));
    }
}

fn remove_json_path(target: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((parent, last)) => (Some(parent), last),
        None => (None, path),
    };
    let parent = match parent {
        Some(parent) => parent
            .split('.')
            .try_fold(&mut *target, |value, part| value.get_mut(part)),
        None => Some(target),
    };
    if let Some(Value::Object(object)) = parent {
        object.remove(last);
    }
}

struct FilePattern {
    matcher: Arc<Pattern>,
    prefix: String,
    excluded: bool,
    /// The project a project fileset selects the files of.
    owner: Option<String>,
}

#[derive(Default)]
struct FileSelection {
    patterns: Vec<FilePattern>,
    included: BTreeSet<String>,
    excluded: BTreeSet<String>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum FileScope {
    Workspace,
    Project(String),
}

struct Resolver<'a> {
    workspace: &'a Workspace,
    snapshot: &'a Snapshot,
    candidates: &'a BTreeSet<String>,
    selected: BTreeSet<String>,
    scopes: BTreeMap<FileScope, FileSelection>,
    values: BTreeMap<String, Value>,
    named_stack: Vec<(String, String)>,
    /// Dependency named inputs already expanded for `^` inputs.
    expanded: BTreeSet<(String, String)>,
    external: BTreeSet<String>,
    /// `None` resolves which inputs a task has without evaluating env or
    /// runtime inputs.
    prepared: Option<&'a PreparedTask>,
    cancelled: &'a AtomicBool,
}

impl<'a> Resolver<'a> {
    fn new(
        snapshot: &'a Snapshot,
        workspace: &'a Workspace,
        prepared: Option<&'a PreparedTask>,
        cancelled: &'a AtomicBool,
        candidates: &'a BTreeSet<String>,
    ) -> Self {
        Self {
            workspace,
            snapshot,
            candidates,
            selected: BTreeSet::new(),
            scopes: BTreeMap::new(),
            values: BTreeMap::new(),
            named_stack: Vec::new(),
            expanded: BTreeSet::new(),
            external: BTreeSet::new(),
            prepared,
            cancelled,
        }
    }

    fn task_inputs(&mut self, task: &Task) -> Result<()> {
        let default = vec![json!("default"), json!("^default")];
        for input in task.definition.inputs.as_ref().unwrap_or(&default) {
            self.input(&task.project, input)?;
        }
        Ok(())
    }

    fn input(&mut self, project: &str, input: &Value) -> Result<()> {
        match input {
            // As in Nx, `^name` is `name` of every project the project depends
            // on, directly or not.
            Value::String(name) if name.starts_with('^') => {
                self.dependencies_named(project, &name[1..])?;
            }
            // The object spellings of named inputs, as Nx reads them.
            Value::Object(object)
                if object.contains_key("input") && !object.contains_key("fileset") =>
            {
                let name = object["input"]
                    .as_str()
                    .context("input must name a named input")?;
                let projects = object.get("projects");
                let dependencies = object.get("dependencies") == Some(&json!(true))
                    || projects == Some(&json!("dependencies"));
                let listed = projects.filter(|projects| {
                    **projects != json!("self") && **projects != json!("dependencies")
                });
                if (dependencies || listed.is_some()) && !self.named_stack.is_empty() {
                    bail!("named inputs can only refer to named inputs of their own project");
                }
                if dependencies {
                    self.dependencies_named(project, name)?;
                } else if let Some(listed) = listed {
                    let patterns: Vec<String> = match listed {
                        Value::String(pattern) => vec![pattern.clone()],
                        other => serde_json::from_value(other.clone())
                            .context("input projects must be a string or an array")?,
                    };
                    for selected in
                        qk_graph::select_projects(&self.workspace.projects, &patterns, &[])?
                    {
                        if self.expanded.insert((selected.clone(), name.to_owned())) {
                            self.named(&selected, name)?;
                        }
                    }
                } else {
                    self.named(project, name)?;
                }
            }
            Value::Object(object)
                if object.contains_key("fileset")
                    && object.get("dependencies") == Some(&json!(true)) =>
            {
                if !self.named_stack.is_empty() {
                    bail!("named inputs can only refer to named inputs of their own project");
                }
                let pattern = object["fileset"]
                    .as_str()
                    .context("fileset input must be a glob")?
                    .to_owned();
                for dependency in self.dependency_closure(project) {
                    self.fileset(&dependency, &pattern)?;
                }
            }
            Value::Object(object)
                if object.len() == 1 && object.contains_key("workingDirectory") =>
            {
                let mode = object["workingDirectory"].as_str().unwrap_or_default();
                let directory = std::env::current_dir()?;
                let value = match mode {
                    "absolute" => directory.to_string_lossy().into_owned(),
                    "relative" => {
                        let root = self.workspace.root.canonicalize()?;
                        let directory = directory.canonicalize()?;
                        paths::relative(&root, &directory)
                            .unwrap_or_else(|_| directory.to_string_lossy().into_owned())
                    }
                    _ => bail!("workingDirectory must be \"relative\" or \"absolute\""),
                };
                self.values
                    .insert(format!("workingDirectory:{mode}"), json!(value));
            }
            Value::Object(object) if object.contains_key("json") => {
                self.json(project, object)?;
            }
            Value::String(name)
                if !name.contains("{projectRoot}") && !name.contains("{workspaceRoot}") =>
            {
                self.named(project, name)?
            }
            Value::Object(object) if object.len() == 1 && object.contains_key("fileset") => {
                let pattern = object["fileset"]
                    .as_str()
                    .context("fileset input must be a glob")?;
                self.fileset(project, pattern)?;
            }
            Value::String(pattern) => self.fileset(project, pattern)?,
            Value::Object(object) if object.len() == 1 && object.contains_key("env") => {
                let name = object["env"]
                    .as_str()
                    .context("env input must name a variable")?;
                let Some(prepared) = self.prepared else {
                    self.values.insert(format!("env:{name}"), Value::Null);
                    return Ok(());
                };
                let value = prepared
                    .env
                    .get(std::ffi::OsStr::new(name))
                    .map(|value| {
                        value
                            .to_str()
                            .map(str::to_owned)
                            .context("declared env input must be UTF-8")
                    })
                    .transpose()?;
                self.values.insert(format!("env:{name}"), json!(value));
            }
            Value::Object(object) if object.len() == 1 && object.contains_key("runtime") => {
                let command = object["runtime"]
                    .as_str()
                    .context("runtime input must be a command")?;
                let Some(prepared) = self.prepared else {
                    self.values
                        .insert(format!("runtime:{command}"), Value::Null);
                    return Ok(());
                };
                let memo = RuntimeKey {
                    command: command.into(),
                    environment: prepared.runtime_env.clone(),
                };
                let slot = self
                    .snapshot
                    .runtime
                    .lock()
                    .unwrap()
                    .entry(memo)
                    .or_default()
                    .clone();
                let mut saved = slot.lock().unwrap();
                if let Some(value) = saved.as_ref() {
                    self.values
                        .insert(format!("runtime:{command}"), value.clone());
                    return Ok(());
                }
                let _profile = crate::profile::span(command, "runtime");
                let mut prepared = prepared.clone();
                prepared.env = prepared.runtime_env.clone();
                prepared.execution.clear();
                prepared.node_path = None;
                prepared.commands = vec![command.into()];
                prepared.cwd = self.workspace.root.clone();
                prepared.parallel = false;
                let log = tempfile::NamedTempFile::new()?;
                let capture = Capture::new(Some(log.as_file().try_clone()?), Display::Hidden);
                if execute_captured(&prepared, self.cancelled, Some(&capture))? != Outcome::Success
                {
                    bail!("runtime input did not succeed");
                }
                let mut stdout = blake3::Hasher::new();
                let mut stderr = blake3::Hasher::new();
                read_capture(File::open(log.path())?, |error, bytes| {
                    if error {
                        stderr.update(bytes);
                    } else {
                        stdout.update(bytes);
                    }
                    Ok(())
                })?;
                let value = json!([
                    stdout.finalize().to_hex().to_string(),
                    stderr.finalize().to_hex().to_string()
                ]);
                *saved = Some(value.clone());
                self.values.insert(format!("runtime:{command}"), value);
            }
            Value::Object(object) if object.contains_key("dependentTasksOutputFiles") => {
                if object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "dependentTasksOutputFiles" | "transitive"))
                {
                    bail!("unsupported output input field");
                }
                let pattern = object["dependentTasksOutputFiles"]
                    .as_str()
                    .context("dependentTasksOutputFiles must be a glob")?;
                self.snapshot.pattern(pattern, false)?;
                let transitive = object
                    .get("transitive")
                    .map(|value| value.as_bool().context("transitive must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                // Resolved before execution too, so inspection can display the
                // selection without reading generated files. dependency_keys
                // hashes these files after the dependencies finish.
                let value = self
                    .values
                    .entry(format!("dependentTasksOutputFiles:{pattern}"))
                    .or_insert(json!(false));
                if transitive {
                    *value = json!(true);
                }
            }
            Value::Object(object)
                if object.len() == 1 && object.contains_key("externalDependencies") =>
            {
                let names: Vec<String> =
                    serde_json::from_value(object["externalDependencies"].clone())?;
                self.external.extend(names);
            }
            _ => bail!("unsupported task input declaration"),
        }
        Ok(())
    }

    fn fileset(&mut self, project: &str, pattern: &str) -> Result<()> {
        let (exclude, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern));
        // Nx hashes workspace files and each project's files in separate
        // scopes, a project's being only the files it owns: those of a project
        // nested in its root are that project's.
        let (scope, owner) = if pattern.contains("{workspaceRoot}") {
            (FileScope::Workspace, None)
        } else {
            (
                FileScope::Project(project.to_owned()),
                Some(project.to_owned()),
            )
        };
        let pattern = paths::expand(self.workspace, project, pattern)?;
        let matcher = self.snapshot.pattern(&pattern, exclude)?;
        let prefix = pattern
            .split('/')
            .take_while(|part| is_literal(part))
            .collect::<Vec<_>>()
            .join("/");
        let candidates: Box<dyn Iterator<Item = &String>> = if prefix.is_empty() {
            Box::new(self.candidates.iter())
        } else {
            Box::new(under(self.candidates, &prefix))
        };
        let snapshot = self.snapshot;
        let selection = self.scopes.entry(scope).or_default();
        let selected = if exclude {
            &mut selection.excluded
        } else {
            &mut selection.included
        };
        selected.extend(
            candidates
                .filter(|file| matcher.is_match(file))
                .filter(|file| owner.is_none() || snapshot.owner(file) == owner.as_deref())
                .cloned(),
        );
        selection.patterns.push(FilePattern {
            matcher,
            prefix,
            excluded: exclude,
            owner,
        });
        Ok(())
    }

    /// Every project `project` depends on, directly or not.
    fn dependency_closure(&self, project: &str) -> BTreeSet<String> {
        let mut closure = BTreeSet::new();
        let mut pending = vec![project.to_owned()];
        while let Some(current) = pending.pop() {
            for edge in &self.snapshot.projects.dependencies[&current] {
                if edge.target != project && closure.insert(edge.target.clone()) {
                    pending.push(edge.target.clone());
                }
            }
        }
        closure
    }

    /// As in Nx, `^name` is `name` of every project the project depends on.
    fn dependencies_named(&mut self, project: &str, name: &str) -> Result<()> {
        for dependency in self.dependency_closure(project) {
            if self.expanded.insert((dependency.clone(), name.to_owned())) {
                self.named(&dependency, name)?;
            }
        }
        Ok(())
    }

    /// A JSON input: the fields of one JSON file, rather than all of it.
    /// `fields` keeps only the named dotted paths; `excludeFields` removes
    /// them.
    fn json(&mut self, project: &str, object: &serde_json::Map<String, Value>) -> Result<()> {
        for key in object.keys() {
            if !matches!(key.as_str(), "json" | "fields" | "excludeFields") {
                bail!("unsupported json input field {key:?}");
            }
        }
        let path = object["json"]
            .as_str()
            .context("json input must name a file")?;
        let path = paths::expand(self.workspace, project, path)?;
        paths::validate_path(&path)?;
        let list = |name: &str| -> Result<Option<Vec<String>>> {
            object
                .get(name)
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .with_context(|| format!("json input {name} must be an array of strings"))
                })
                .transpose()
        };
        let fields = list("fields")?;
        let exclude = list("excludeFields")?.unwrap_or_default();
        let value = match std::fs::read_to_string(self.workspace.root.join(&path)) {
            Ok(text) => {
                let mut value: Value =
                    jsonc_parser::parse_to_serde_value(&text, &Default::default())
                        .with_context(|| format!("cannot parse json input {path}"))?;
                if let Some(fields) = &fields {
                    let mut kept = json!({});
                    for field in fields {
                        if let Some(found) = json_path(&value, field) {
                            set_json_path(&mut kept, field, found.clone());
                        }
                    }
                    value = kept;
                }
                for field in &exclude {
                    remove_json_path(&mut value, field);
                }
                value
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error.into()),
        };
        self.values.insert(format!("json:{path}"), value);
        Ok(())
    }

    fn named(&mut self, project: &str, name: &str) -> Result<()> {
        let identity = (project.to_owned(), name.to_owned());
        if self.named_stack.contains(&identity) {
            bail!("named input cycle at {project}:{name}");
        }
        self.named_stack.push(identity);
        let inputs = self.workspace.projects[project]
            .named_inputs
            .get(name)
            .cloned()
            .or_else(|| (name == "default").then(|| vec![json!("{projectRoot}/**/*")]))
            .with_context(|| format!("unknown named input {name:?}"))?;
        for input in &inputs {
            self.input(project, input)?;
        }
        self.named_stack.pop();
        Ok(())
    }
}

pub fn resolve(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: Option<&PreparedTask>,
    cancelled: &AtomicBool,
) -> Result<Resolved> {
    resolve_candidates(
        snapshot,
        workspace,
        task,
        prepared,
        cancelled,
        &snapshot.files,
        &snapshot.source_ignore,
    )
}

fn resolve_candidates(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: Option<&PreparedTask>,
    cancelled: &AtomicBool,
    candidates: &BTreeSet<String>,
    source_ignore: &SourceIgnore,
) -> Result<Resolved> {
    let mut resolver = Resolver::new(snapshot, workspace, prepared, cancelled, candidates);
    resolver.task_inputs(task)?;
    // Dependency outputs may also be declared sources. Downstream and unrelated
    // outputs remain artifacts, so producing them cannot invalidate an upstream task.
    let mut upstream = BTreeSet::new();
    let mut pending: Vec<_> = task.dependencies.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        if upstream.insert(id.clone())
            && let Some(dependencies) = snapshot.task_dependencies.get(&id)
        {
            pending.extend(dependencies.iter().cloned());
        }
    }
    let own_outputs = Outputs::new(workspace, task)
        .ok()
        .filter(Outputs::is_explicit);
    for (producer, declared) in &snapshot.generated_tasks {
        if producer.id == task.id || !upstream.contains(&producer.id) {
            continue;
        }
        let overlaps = |a: &str, b: &str| {
            a.is_empty()
                || a == b
                || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
                || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        };
        let relevant = resolver
            .scopes
            .values()
            .flat_map(|scope| &scope.patterns)
            .filter(|pattern| !pattern.excluded)
            .any(|pattern| {
                declared.anchors().any(|anchor| {
                    overlaps(&pattern.prefix, anchor)
                        && (!source_ignore.matches_directory(anchor)
                            || !workspace.root.join(anchor).is_dir())
                })
            });
        if !relevant {
            continue;
        }
        let outputs = snapshot.dependency_outputs(workspace, producer)?;
        for path in &outputs.files {
            if source_ignore.matches(path)
                || own_outputs.as_ref().is_some_and(|own| own.matches(path))
            {
                continue;
            }
            for selection in resolver.scopes.values_mut() {
                for pattern in &selection.patterns {
                    if pattern.matcher.is_match(path)
                        && (pattern.owner.is_none()
                            || snapshot.owner(path) == pattern.owner.as_deref())
                    {
                        if pattern.excluded {
                            selection.excluded.insert(path.clone());
                        } else {
                            selection.included.insert(path.clone());
                        }
                    }
                }
            }
        }
    }
    for selection in resolver.scopes.values() {
        resolver.selected.extend(
            selection
                .included
                .difference(&selection.excluded)
                .filter(|path| own_outputs.as_ref().is_none_or(|own| !own.matches(path)))
                .cloned(),
        );
    }
    let readable = snapshot
        .installed(&workspace.root)?
        .is_some_and(|installed| installed.lockfile.is_some());
    // Always include workspace resolution and configuration. Dotenv files are
    // not keyed, as in Nx: they hold per-machine values and credentials, and a
    // task's `env` inputs key the variables it declares.
    // A pnpm lockfile qk can read is keyed by what the task's projects install instead.
    // The root tsconfig, as Nx hashes it into every task. The root package.json
    // is not, as in Nx: what the root importer installs counts for every task
    // through the lockfile, and the rest of it is the root project's manifest.
    let tsconfig = ["tsconfig.base.json", "tsconfig.json"]
        .into_iter()
        .find(|name| {
            workspace.root.join(name).is_file() || snapshot.extra_candidates.contains(*name)
        });
    let mut mandatory = BTreeSet::new();
    for path in tsconfig.into_iter().chain([
        "nx.json",
        ".gitignore",
        ".nxignore",
        qk_config::LOCAL_WORKSPACE,
        "pnpm-workspace.yaml",
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
    ]) {
        if (workspace.root.join(path).is_file() || snapshot.extra_candidates.contains(path))
            && !(path == "pnpm-lock.yaml" && readable)
        {
            resolver.selected.insert(path.into());
            mandatory.insert(path.into());
        }
    }
    if let Some(extended) = &workspace.extended {
        resolver.selected.insert(extended.clone());
        mandatory.insert(extended.clone());
    }
    // The task's own manifests remain inputs even when filesets exclude them,
    // its package scripts and dependency declarations with them. A dependency's
    // package.json counts by what it decides for dependents, such as where
    // their imports of it resolve; its installs count through the lockfile.
    let mut packages = BTreeSet::from([task.project.clone()]);
    let mut pending = packages.clone();
    while let Some(project) = pending.pop_first() {
        for edge in &snapshot.projects.dependencies[&project] {
            if packages.insert(edge.target.clone()) {
                pending.insert(edge.target.clone());
            }
        }
    }
    for project in &packages {
        let own = *project == task.project;
        for name in ["project.json", qk_config::LOCAL_OVERRIDES, "package.json"] {
            if !own && name != "package.json" {
                continue;
            }
            let path = Path::new(&workspace.projects[project].root).join(name);
            let path = path.strip_prefix(".").unwrap_or(&path).to_owned();
            if !workspace.root.join(&path).is_file() {
                continue;
            }
            let path = path
                .to_str()
                .context("project path must be UTF-8")?
                .replace('\\', "/");
            mandatory.insert(path.clone());
            // A manifest the task's inputs name counts whole, as they say.
            if own || resolver.selected.contains(&path) {
                resolver.selected.insert(path);
                continue;
            }
            let text = std::fs::read_to_string(workspace.root.join(&path))
                .with_context(|| format!("cannot read {path}"))?;
            match manifest_for_dependents(&text, readable) {
                Some(value) => {
                    resolver.values.insert(format!("manifest:{path}"), value);
                }
                None => {
                    resolver.selected.insert(path);
                }
            }
        }
    }
    // However it was selected, the workspace file's resolution keys reach the
    // task through the lockfile when qk can read it.
    let workspace_file = readable && resolver.selected.remove("pnpm-workspace.yaml");
    if workspace_file {
        mandatory.remove("pnpm-workspace.yaml");
    }
    let lockfile = readable.then(|| {
        // The root importer's packages resolve from every package in the
        // workspace, so they count for every task.
        let importers = std::iter::once(".".to_owned())
            .chain(
                packages
                    .iter()
                    .map(|project| workspace.projects[project].root.clone()),
            )
            .collect();
        (importers, resolver.external.clone())
    });
    Ok(Resolved {
        files: resolver.selected,
        mandatory,
        values: resolver.values,
        lockfile,
        workspace_file,
        external: resolver.external,
    })
}

/// What a task's key is computed from, as JSON: kept by the history so a
/// changed key can be explained.
pub fn inputs(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<Value> {
    let prefixes = snapshot.membership_prefixes(workspace, task)?;
    let candidates = snapshot.current_files_for(&workspace.root, Some(&prefixes))?;
    let Resolved {
        files,
        mut values,
        lockfile,
        workspace_file,
        ..
    } = resolve_candidates(
        snapshot,
        workspace,
        task,
        Some(prepared),
        cancelled,
        &candidates.files,
        &candidates.source_ignore,
    )?;
    let installed = snapshot.installed(&workspace.root)?;
    if let (Some((importers, external)), Some(installed)) = (lockfile, &installed)
        && let Some(lockfile) = &installed.lockfile
    {
        let importers: BTreeMap<_, _> = importers
            .iter()
            .filter(|importer| lockfile.has_importer(importer))
            .map(|importer| {
                let digest = installed.digest(&format!("importer:{importer}"), || {
                    lockfile.installed(importer).unwrap_or_default()
                });
                (importer.clone(), digest)
            })
            .collect();
        let external: BTreeMap<_, _> = external
            .iter()
            .map(|name| {
                let digest =
                    installed.digest(&format!("package:{name}"), || lockfile.package(name));
                (name.clone(), digest)
            })
            .collect();
        values.insert(
            "lockfile".into(),
            json!({
                "global": installed.digest("global", || BTreeSet::from([lockfile.global().to_string()])),
                "importers": importers,
                "external": external,
            }),
        );
    }
    let mut files = files
        .iter()
        .map(|path| {
            let value = snapshot
                .file_value(&workspace.root, path)
                .with_context(|| format!("cannot read input {path}"))?;
            Ok((path.clone(), value))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if workspace_file {
        match workspace_without_resolution(&workspace.root) {
            Some(value) => {
                values.insert("pnpm-workspace".into(), value);
            }
            None => {
                files.insert(
                    "pnpm-workspace.yaml".into(),
                    snapshot.file_value(&workspace.root, "pnpm-workspace.yaml")?,
                );
            }
        }
    }
    // Warm state configuration never changes a result.
    let mut definition = task.definition.clone();
    definition.extra.remove("qk:warm");
    definition.extra.remove("qk:threads");
    let hash = json!({
        "schema":"qk-local-v2", "qk":env!("CARGO_PKG_VERSION"),
        "platform":[std::env::consts::OS, std::env::consts::ARCH], "workspace":snapshot.workspace_prefix,
        "id":task.id, "args":task.args, "definition":definition, "packageManager":workspace.package_manager,
        "files":files, "values":values, "dependencies":dependencies,
    });
    Ok(hash)
}

/// The key for a task's inputs.
pub fn key(inputs: &Value) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(inputs)?)
        .to_hex()
        .to_string())
}

pub fn fingerprint(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<String> {
    key(&inputs(
        snapshot,
        workspace,
        task,
        prepared,
        dependencies,
        cancelled,
    )?)
}

/// Use selected dependency artifacts when output inputs are declared; otherwise
/// preserve the normal dependency fingerprints. All dependencies must have
/// succeeded and be fingerprintable before this selection is applied.
pub(crate) fn dependency_keys(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<BTreeMap<String, String>> {
    dependency_keys_impl(
        snapshot,
        workspace,
        graph,
        task,
        dependencies,
        cancelled,
        false,
    )
}

/// Read dependency artifacts again before accepting an execution as unchanged.
pub(crate) fn recheck_dependency_keys(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
) -> Result<BTreeMap<String, String>> {
    dependency_keys_impl(
        snapshot,
        workspace,
        graph,
        task,
        dependencies,
        cancelled,
        true,
    )
}

fn dependency_keys_impl(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
    fresh: bool,
) -> Result<BTreeMap<String, String>> {
    let resolved = resolve(snapshot, workspace, task, None, cancelled)?;
    let selections: Vec<_> = resolved.dependency_output_inputs().collect();
    if selections.is_empty() {
        if fresh {
            return dependencies
                .iter()
                .map(|(id, key)| {
                    let outputs = Outputs::new(workspace, &graph.tasks[id])?;
                    let key = if outputs.declared() {
                        crate::output_fingerprint(&workspace.root, &outputs, "")?
                    } else {
                        key.clone()
                    };
                    Ok((id.clone(), key))
                })
                .collect();
        }
        return Ok(dependencies.clone());
    }
    let mut keys = BTreeMap::new();
    let mut fresh_outputs = HashMap::new();
    for (pattern, transitive) in selections {
        let matcher = snapshot.pattern(pattern, false)?;
        let mut selected = task.dependencies.clone();
        if transitive {
            let mut pending = selected.clone();
            while let Some(id) = pending.pop_first() {
                for dependency in &graph.tasks[&id].dependencies {
                    if selected.insert(dependency.clone()) {
                        pending.insert(dependency.clone());
                    }
                }
            }
        }
        for id in selected {
            let outputs = if fresh {
                if !fresh_outputs.contains_key(&id) {
                    fresh_outputs.insert(
                        id.clone(),
                        Arc::new(snapshot.fresh_dependency_outputs(workspace, &graph.tasks[&id])?),
                    );
                }
                fresh_outputs[&id].clone()
            } else {
                snapshot.dependency_outputs(workspace, &graph.tasks[&id])?
            };
            let mut files = BTreeMap::new();
            for path in outputs.files.iter().filter(|path| matcher.is_match(path)) {
                let known = outputs.values.lock().unwrap().get(path).cloned();
                let value = match known {
                    Some(value) => value,
                    None => {
                        let value = snapshot.file_value(&workspace.root, path)?;
                        // Links depend on paths outside the declared output roots.
                        if value.get("link").is_none() {
                            outputs
                                .values
                                .lock()
                                .unwrap()
                                .insert(path.clone(), value.clone());
                        }
                        value
                    }
                };
                files.insert(path.clone(), value);
            }
            keys.insert(format!("{id}:{pattern}"), key(&json!(files))?);
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qk_taskgraph::Request;

    fn fixture(outputs: Option<Value>) -> (tempfile::TempDir, Workspace, TaskGraph) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        let mut post = json!({"command": "true"});
        if let Some(outputs) = outputs {
            post["outputs"] = outputs;
        }
        std::fs::write(root.path().join("project.json"), json!({"name":"app", "targets": {
            "gen": {"command":"true", "outputs":["generated"]},
            "post": post,
            "build": {"command":"true", "dependsOn":["gen", "post"], "inputs":[{"dependentTasksOutputFiles":"**/*"}]}
        }}).to_string()).unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(root.path().join("generated/value"), "one").unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        (root, workspace, graph)
    }

    #[test]
    fn execution_fingerprints_refresh_sources_and_preserve_artifact_exclusions() {
        let (root, mut workspace, _) = fixture(Some(json!(["dist"])));
        let definition = workspace
            .projects
            .get_mut("app")
            .unwrap()
            .targets
            .get_mut("post")
            .unwrap();
        definition
            .extra
            .insert("qk:warm".into(), json!({"paths": ["scratch"]}));
        std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/original"), "one").unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let task = &graph.tasks["app:post"];
        let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let inputs = || {
            super::inputs(
                &snapshot,
                &workspace,
                task,
                &prepared,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let before = inputs();
        // Artifacts created after startup must not change the execution key.
        for directory in ["generated", "dist", "scratch", "cache", "ignored"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
            std::fs::write(root.path().join(directory).join("new"), "artifact").unwrap();
        }
        assert_eq!(before, inputs());
        // This covers both post-execution verification and a later task's first
        // fingerprint against a snapshot created before this source existed.
        std::fs::write(root.path().join("src/added"), "two").unwrap();
        let after = inputs();
        assert!(after["files"].get("src/added").is_some());
        assert_ne!(key(&before).unwrap(), key(&after).unwrap());
        std::fs::remove_file(root.path().join("src/added")).unwrap();
        assert_eq!(before, inputs());
        std::fs::remove_file(root.path().join("src/original")).unwrap();
        assert!(inputs()["files"].get("src/original").is_none());
    }

    #[test]
    fn membership_reuses_settled_candidates_and_detects_directory_changes() {
        let mut fixtures = Vec::new();
        for git in [false, true] {
            let (root, workspace, graph) = fixture(Some(json!(["dist"])));
            for directory in ["src/empty", "dist", "cache", "ignored"] {
                std::fs::create_dir_all(root.path().join(directory)).unwrap();
            }
            std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
            std::fs::write(root.path().join("src/original"), "one").unwrap();
            if git {
                assert!(
                    paths::git(root.path(), &["init", "-q"])
                        .unwrap()
                        .status
                        .success()
                );
                assert!(
                    paths::git(root.path(), &["add", "."])
                        .unwrap()
                        .status
                        .success()
                );
            }
            let snapshot =
                Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
            snapshot.current_files(root.path()).unwrap();
            fixtures.push((root, snapshot));
        }
        // Recent directory metadata deliberately uses the conservative listing
        // path. Waiting lets this test exercise the actual reuse path too.
        std::thread::sleep(Duration::from_millis(2100));
        for (root, snapshot) in fixtures {
            let files = || snapshot.current_files(root.path()).unwrap();
            let before = files();
            assert!(Arc::ptr_eq(&before, &files()));
            for directory in ["generated", "dist", "cache"] {
                std::fs::write(root.path().join(directory).join("new"), "artifact").unwrap();
            }
            assert!(Arc::ptr_eq(&before, &files()));
            std::fs::write(root.path().join("src/empty/added"), "two").unwrap();
            assert!(files().contains("src/empty/added"));
            std::fs::create_dir_all(root.path().join("src/new/nested")).unwrap();
            std::fs::write(root.path().join("src/new/nested/added"), "three").unwrap();
            assert!(files().contains("src/new/nested/added"));
            std::fs::rename(
                root.path().join("src/original"),
                root.path().join("src/renamed"),
            )
            .unwrap();
            let renamed = files();
            assert!(!renamed.contains("src/original"));
            assert!(renamed.contains("src/renamed"));
            std::fs::remove_file(root.path().join("src/renamed")).unwrap();
            assert!(!files().contains("src/renamed"));
            // Policy files must be checked even when directory entries do not
            // change (editing an existing ignore file in place).
            std::fs::write(root.path().join(".gitignore"), "ignored/\nsrc/empty/\n").unwrap();
            assert!(!files().contains("src/empty/added"));
            std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
            assert!(files().contains("src/empty/added"));
        }
    }

    #[test]
    fn a_directory_missed_by_the_inventory_cannot_enable_reuse() {
        let (root, workspace, graph) = fixture(None);
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(root.path()).unwrap();
        std::fs::create_dir(root.path().join("new-empty")).unwrap();
        let mut cached = snapshot.membership.lock().unwrap();
        let membership = Arc::make_mut(cached.as_mut().unwrap());
        // Model creation between the directory walk and child-name capture:
        // the new directory is in its parent's names but not in the inventory.
        membership
            .directory_entries
            .get_mut(root.path())
            .unwrap()
            .insert((OsString::from("new-empty"), 1));
        assert!(membership.entries(root.path()).unwrap().is_none());
        assert!(!membership.state(None).unwrap().0);
    }

    #[test]
    fn a_policy_missed_by_discovery_cannot_enable_reuse() {
        let (root, workspace, graph) = fixture(None);
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(root.path()).unwrap();
        std::fs::write(root.path().join(".nxignore"), "hidden\n").unwrap();
        let mut cached = snapshot.membership.lock().unwrap();
        let membership = Arc::make_mut(cached.as_mut().unwrap());
        // Model a new policy appearing after discovery but before capturing
        // its parent's entries; its content would otherwise have no stamp.
        membership
            .directory_entries
            .get_mut(root.path())
            .unwrap()
            .insert((OsString::from(".nxignore"), 0));
        assert!(membership.entries(root.path()).unwrap().is_none());
        assert!(!membership.state(None).unwrap().0);
    }

    #[test]
    fn output_publication_reuses_candidates_but_source_names_still_refresh() {
        for partial in [false, true] {
            let outputs = if partial {
                json!(["dist", "!dist/keep"])
            } else {
                json!(["dist"])
            };
            let (root, workspace, graph) = fixture(Some(outputs));
            std::fs::create_dir_all(root.path().join("src")).unwrap();
            std::fs::write(root.path().join("src/original"), "one").unwrap();
            if partial {
                std::fs::create_dir_all(root.path().join("dist/keep")).unwrap();
            }
            let snapshot =
                Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
            snapshot.current_files(root.path()).unwrap();
            // Policies retain their conservative timestamp window. Directory
            // names are freshly verified throughout the window instead.
            std::thread::sleep(Duration::from_millis(2100));
            let before = snapshot.current_files(root.path()).unwrap();
            std::fs::create_dir_all(root.path().join("dist")).unwrap();
            std::fs::write(root.path().join("dist/restored"), "artifact").unwrap();
            let restored = snapshot.current_files(root.path()).unwrap();
            assert!(Arc::ptr_eq(&before, &restored));
            std::fs::remove_file(root.path().join("dist/restored")).unwrap();
            assert!(Arc::ptr_eq(
                &before,
                &snapshot.current_files(root.path()).unwrap()
            ));
            let source = if partial { "dist/keep/new" } else { "src/new" };
            std::fs::write(root.path().join(source), "source").unwrap();
            let after = snapshot.current_files(root.path()).unwrap();
            assert!(after.contains(source));
            assert!(!Arc::ptr_eq(&before, &after));
            std::fs::remove_file(root.path().join(source)).unwrap();
            assert!(
                !snapshot
                    .current_files(root.path())
                    .unwrap()
                    .contains(source)
            );
        }
    }

    #[test]
    fn concurrent_fingerprints_refresh_without_replacing_newer_membership() {
        let (root, workspace, graph) = fixture(None);
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let original = snapshot.current_files(root.path()).unwrap();
        let barrier = std::sync::Barrier::new(9);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let mut rounds = Vec::new();
                        for _ in 0..6 {
                            barrier.wait();
                            rounds.push(snapshot.current_files(root.path()));
                            barrier.wait();
                        }
                        rounds
                    })
                })
                .collect();
            for round in 0..6 {
                std::fs::write(root.path().join(format!("src/new{round}")), "source").unwrap();
                barrier.wait();
                barrier.wait();
            }
            // Assertions run after synchronization, so a failed membership
            // check cannot strand other workers at a barrier.
            for worker in workers {
                for (round, files) in worker.join().unwrap().into_iter().enumerate() {
                    let files = files.unwrap();
                    for added in 0..=round {
                        assert!(files.contains(&format!("src/new{added}")));
                    }
                }
            }
        });
        assert!(!original.contains("src/new0"));
        assert!(
            snapshot
                .current_files(root.path())
                .unwrap()
                .contains("src/new5")
        );
    }

    #[test]
    fn retained_candidates_keep_their_dependency_output_ignore_policy() {
        let (root, mut workspace, _) = fixture(None);
        workspace
            .projects
            .get_mut("app")
            .unwrap()
            .targets
            .get_mut("build")
            .unwrap()
            .inputs = Some(vec![json!("{workspaceRoot}/**/*.txt")]);
        std::fs::create_dir(root.path().join("src")).unwrap();
        for name in ["alpha", "beta", "gamma"] {
            for directory in ["src", "generated"] {
                std::fs::write(root.path().join(format!("{directory}/{name}.txt")), name).unwrap();
            }
        }
        let policy = |keep: &str| {
            let ignored: String = ["alpha", "beta", "gamma"]
                .into_iter()
                .filter(|name| *name != keep)
                .flat_map(|name| {
                    [
                        format!("src/{name}.txt\n"),
                        format!("generated/{name}.txt\n"),
                    ]
                })
                .collect();
            std::fs::write(root.path().join(".nxignore"), ignored).unwrap();
        };
        policy("alpha");
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        let first = snapshot.current_files_for(root.path(), None).unwrap();
        policy("beta");
        let second = snapshot.current_files_for(root.path(), None).unwrap();
        policy("gamma");
        let third = snapshot.current_files_for(root.path(), None).unwrap();
        // Deterministically pause two fingerprints after candidate acquisition,
        // then let another fingerprint refresh policy twice before they resolve.
        let cancelled = AtomicBool::new(false);
        let expected = |name: &str| {
            BTreeSet::from([
                ".nxignore".into(),
                "nx.json".into(),
                "project.json".into(),
                format!("src/{name}.txt"),
                format!("generated/{name}.txt"),
            ])
        };
        for (candidates, name) in [(first, "alpha"), (second, "beta"), (third, "gamma")] {
            let resolved = resolve_candidates(
                &snapshot,
                &workspace,
                &graph.tasks["app:build"],
                None,
                &cancelled,
                &candidates.files,
                &candidates.source_ignore,
            )
            .unwrap();
            assert_eq!(resolved.files, expected(name));
        }
        // Inspection still uses its original candidate list and original policy.
        let inspected = resolve(
            &snapshot,
            &workspace,
            &graph.tasks["app:build"],
            None,
            &cancelled,
        )
        .unwrap();
        assert_eq!(inspected.files, expected("alpha"));
    }

    #[test]
    fn scoped_membership_defers_unrelated_changes_until_a_matching_task_asks() {
        let (root, mut workspace, _) = fixture(Some(json!(["dist"])));
        let targets = &mut workspace.projects.get_mut("app").unwrap().targets;
        targets.get_mut("post").unwrap().inputs =
            Some(vec![json!("{workspaceRoot}/left/**/*.txt")]);
        targets.get_mut("gen").unwrap().inputs =
            Some(vec![json!("{workspaceRoot}/right/**/*.txt")]);
        targets.get_mut("build").unwrap().inputs = Some(vec![json!("{workspaceRoot}/**/*.txt")]);
        for directory in ["left/empty", "right/empty"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(&workspace.root).unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let left = snapshot
            .membership_prefixes(&workspace, &graph.tasks["app:post"])
            .unwrap();
        let right = snapshot
            .membership_prefixes(&workspace, &graph.tasks["app:gen"])
            .unwrap();
        let all = snapshot
            .membership_prefixes(&workspace, &graph.tasks["app:build"])
            .unwrap();
        let before = snapshot
            .current_files_for(&workspace.root, Some(&left))
            .unwrap();
        std::fs::write(root.path().join("right/empty/new.txt"), "right").unwrap();
        let unrelated = snapshot
            .current_files_for(&workspace.root, Some(&left))
            .unwrap();
        assert!(Arc::ptr_eq(&before.files, &unrelated.files));
        assert!(!unrelated.files.contains("right/empty/new.txt"));
        assert!(
            snapshot
                .current_files_for(&workspace.root, Some(&right))
                .unwrap()
                .files
                .contains("right/empty/new.txt")
        );
        std::fs::create_dir_all(root.path().join("left/empty/nested")).unwrap();
        std::fs::write(root.path().join("left/empty/nested/new.txt"), "left").unwrap();
        assert!(
            snapshot
                .current_files_for(&workspace.root, Some(&left))
                .unwrap()
                .files
                .contains("left/empty/nested/new.txt")
        );
        std::fs::write(root.path().join("right/empty/another.txt"), "right").unwrap();
        assert!(
            snapshot
                .current_files_for(&workspace.root, Some(&all))
                .unwrap()
                .files
                .contains("right/empty/another.txt")
        );
    }

    #[test]
    fn membership_watches_git_ignored_tracked_parents_and_index_policy() {
        let (root, workspace, graph) = fixture(Some(json!(["dist"])));
        std::fs::create_dir_all(root.path().join("tracked/nested")).unwrap();
        std::fs::write(root.path().join("tracked/.gitignore"), "nested/\n").unwrap();
        std::fs::write(root.path().join("tracked/nested/source"), "one").unwrap();
        assert!(
            paths::git(root.path(), &["init", "-q"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            paths::git(root.path(), &["add", "-f", "tracked/nested/source"])
                .unwrap()
                .status
                .success()
        );
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let files = || snapshot.current_files(root.path()).unwrap();
        assert!(files().contains("tracked/nested/source"));
        std::fs::remove_file(root.path().join("tracked/nested/source")).unwrap();
        assert!(!files().contains("tracked/nested/source"));
        std::fs::write(root.path().join("tracked/nested/another"), "two").unwrap();
        assert!(!files().contains("tracked/nested/another"));
        assert!(
            paths::git(root.path(), &["add", "-f", "tracked/nested/another"])
                .unwrap()
                .status
                .success()
        );
        assert!(files().contains("tracked/nested/another"));
        std::fs::write(root.path().join("untracked"), "three").unwrap();
        assert!(files().contains("untracked"));
        std::fs::write(root.path().join(".git/info/exclude"), "untracked\n").unwrap();
        assert!(!files().contains("untracked"));
    }

    #[test]
    fn a_new_git_config_include_invalidates_settled_listing_policy() {
        let (root, workspace, graph) = fixture(Some(json!(["dist"])));
        let policy = tempfile::tempdir().unwrap();
        let included = policy.path().join("included.config");
        let excludes = policy.path().join("excludes");
        std::fs::write(&excludes, "hidden\n").unwrap();
        std::fs::write(root.path().join("hidden"), "source").unwrap();
        assert!(
            paths::git(root.path(), &["init", "-q"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            paths::git(
                root.path(),
                &["config", "include.path", included.to_str().unwrap()]
            )
            .unwrap()
            .status
            .success()
        );
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(&workspace.root).unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let before = snapshot.current_files(&workspace.root).unwrap();
        assert!(before.contains("hidden"));
        assert!(Arc::ptr_eq(
            &before,
            &snapshot.current_files(&workspace.root).unwrap()
        ));
        assert!(
            paths::git(
                root.path(),
                &[
                    "config",
                    "--file",
                    included.to_str().unwrap(),
                    "core.excludesfile",
                    excludes.to_str().unwrap(),
                ]
            )
            .unwrap()
            .status
            .success()
        );
        assert!(
            !snapshot
                .current_files(&workspace.root)
                .unwrap()
                .contains("hidden")
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_policy_paths_preserve_quoted_origins_and_multiline_values() {
        let (root, workspace, graph) = fixture(Some(json!(["dist"])));
        let policy = tempfile::tempdir().unwrap();
        // Git's text output C-quotes these origins and octal-escapes UTF-8.
        let included = policy.path().join("included\\é\t\n.config");
        let excludes = policy.path().join("excludes\\é\t\n");
        let replacement = policy.path().join("replacement\n");
        std::fs::write(&excludes, "hidden\n").unwrap();
        std::fs::write(&replacement, "visible\n").unwrap();
        for file in ["hidden", "visible"] {
            std::fs::write(root.path().join(file), "source").unwrap();
        }
        let git = |args: &[&str]| {
            let output = paths::git(root.path(), args).unwrap();
            assert!(output.status.success(), "{output:?}");
            output
        };
        git(&["init", "-q"]);
        git(&["config", "include.path", included.to_str().unwrap()]);
        git(&[
            "config",
            "--file",
            included.to_str().unwrap(),
            "core.excludesfile",
            excludes.to_str().unwrap(),
        ]);
        let text = git(&["config", "--show-origin", "--list"]);
        let text = String::from_utf8_lossy(&text.stdout);
        let quoted_origin = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("file:\"")
                    .and_then(|_| line.split_once('\t'))
            })
            .unwrap()
            .0
            .strip_prefix("file:")
            .unwrap();
        let policies = git_membership_policies(root.path());
        assert!(!policies.contains(&root.path().join(quoted_origin)));
        assert!(policies.contains(&included));
        assert!(policies.contains(&excludes));
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let initial = snapshot.current_files(&workspace.root).unwrap();
        assert!(!initial.contains("hidden"));
        assert!(initial.contains("visible"));
        std::thread::sleep(Duration::from_millis(2100));
        let settled = snapshot.current_files(&workspace.root).unwrap();
        assert!(Arc::ptr_eq(
            &settled,
            &snapshot.current_files(&workspace.root).unwrap()
        ));
        git(&[
            "config",
            "--file",
            included.to_str().unwrap(),
            "core.excludesfile",
            replacement.to_str().unwrap(),
        ]);
        let changed = snapshot.current_files(&workspace.root).unwrap();
        assert!(changed.contains("hidden"));
        assert!(!changed.contains("visible"));
        assert!(git_membership_policies(root.path()).contains(&replacement));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ignore_policy_never_reuses_stale_membership() {
        let (root, workspace, graph) = fixture(Some(json!(["dist"])));
        let policy = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(policy.path(), "hidden\n").unwrap();
        std::os::unix::fs::symlink(policy.path(), root.path().join(".gitignore")).unwrap();
        std::fs::write(root.path().join("hidden"), "source").unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        assert!(
            !snapshot
                .current_files(root.path())
                .unwrap()
                .contains("hidden")
        );
        assert!(
            !snapshot
                .membership
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .settled
        );
        std::thread::sleep(Duration::from_millis(2100));
        let before = snapshot.current_files(root.path()).unwrap();
        assert!(Arc::ptr_eq(
            &before,
            &snapshot.current_files(root.path()).unwrap()
        ));
        std::fs::write(policy.path(), "").unwrap();
        assert!(
            snapshot
                .current_files(root.path())
                .unwrap()
                .contains("hidden")
        );
    }

    #[cfg(unix)]
    #[test]
    fn recent_change_time_prevents_membership_reuse_despite_old_mtime() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        File::open(root.path())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let metadata = std::fs::metadata(root.path()).unwrap();
        assert!(metadata.ctime() > 1);
        let stamp = Stamp::new(&metadata);
        assert!(stamp.settled());
        assert!(!stamp.membership_settled());
    }

    #[test]
    fn inspection_keeps_deleted_candidates() {
        let (root, workspace, graph) = fixture(Some(json!(["dist"])));
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache"))
            .unwrap()
            .with_candidates(&["src/deleted".into()]);
        let resolved = resolve(
            &snapshot,
            &workspace,
            &graph.tasks["app:post"],
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(resolved.files.contains("src/deleted"));
    }

    #[test]
    fn undeclared_writes_invalidate_dependency_values() {
        for outputs in [None, Some(json!(["generated"]))] {
            let (root, workspace, graph) = fixture(outputs);
            let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
            let keys = || {
                dependency_keys(
                    &snapshot,
                    &workspace,
                    &graph,
                    &graph.tasks["app:build"],
                    &BTreeMap::new(),
                    &AtomicBool::new(false),
                )
                .unwrap()
            };
            let before = keys();
            std::fs::write(root.path().join("generated/value"), "different").unwrap();
            snapshot.outputs_written(&workspace, &graph.tasks["app:post"]);
            assert_ne!(before, keys());
        }
    }

    #[cfg(unix)]
    #[test]
    fn dependency_symlinks_recheck_targets_outside_output_roots() {
        let (root, mut workspace, _) = fixture(Some(json!(["generated"])));
        workspace
            .projects
            .get_mut("app")
            .unwrap()
            .targets
            .get_mut("gen")
            .unwrap()
            .outputs = Some(vec!["links".into()]);
        std::fs::create_dir(root.path().join("links")).unwrap();
        std::os::unix::fs::symlink("../generated/value", root.path().join("links/value")).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        let keys = || {
            dependency_keys(
                &snapshot,
                &workspace,
                &graph,
                &graph.tasks["app:build"],
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let before = keys();
        std::fs::write(root.path().join("generated/value"), "different").unwrap();
        snapshot.outputs_written(&workspace, &graph.tasks["app:post"]);
        assert_ne!(before["app:gen:**/*"], keys()["app:gen:**/*"]);
    }
}
