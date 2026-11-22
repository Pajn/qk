use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use foldhash::{HashMap, HashSet};
use qk_config::Workspace;
use qk_executor::{Capture, Display, Outcome, PreparedTask, execute_captured, read_capture};
use qk_graph::ProjectGraph;
use qk_lockfile::Lockfile;
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

use crate::ignored::Group;
use crate::inputs::{
    Declared, Inferred, JsonFields, Resolved, TsConfig, manifest_for_dependents,
    workspace_without_resolution,
};
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

    /// Whether a membership stamp, with the stamp of a symlink's target after
    /// its own, was older than `window` at `looked`: see [`Self::settled`].
    fn membership_settled(&self, window: Duration, looked: SystemTime) -> bool {
        #[cfg(unix)]
        let width = 6;
        #[cfg(not(unix))]
        let width = 2;
        self.0
            .chunks_exact(width)
            .all(|values| times_settled(values, window, looked))
    }

    /// Whether every time in this stamp is more than `window`, as
    /// [`crate::settle::window`] gives it for the file's filesystem, before
    /// `looked`, a time taken before the file was read: a write within the
    /// window of the last could leave the stamp as it was, so a file that
    /// changed more recently is always read again. The change time counts
    /// too, as tools set modification times back but nothing sets change
    /// times back.
    fn settled(&self, window: Duration, looked: SystemTime) -> bool {
        times_settled(&self.0, window, looked)
    }
}

/// [`Stamp::settled`] of the values of one file's stamp.
fn times_settled(values: &[i64], window: Duration, looked: SystemTime) -> bool {
    let modified = SystemTime::UNIX_EPOCH + Duration::from_nanos(values[1].max(0) as u64);
    if values[1] < 0 || !crate::settle::older(modified, window, looked) {
        return false;
    }
    #[cfg(unix)]
    {
        let changed = SystemTime::UNIX_EPOCH
            + Duration::new(
                values[2].max(0) as u64,
                values[3].clamp(0, 999_999_999) as u32,
            );
        values[2] >= 0 && crate::settle::older(changed, window, looked)
    }
    #[cfg(not(unix))]
    true
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
    root: PathBuf,
    /// The initial candidate list, see [`Self::files`]: listed with the
    /// snapshot unless every task's file inputs name paths, and otherwise on
    /// first use.
    files: std::sync::OnceLock<std::result::Result<BTreeSet<String>, String>>,
    /// The inventory and listing a snapshot for keying takes while its first
    /// tasks are keyed, see [`Self::settle`]. A task whose file inputs name
    /// paths needs neither, so it is keyed, and runs, without waiting.
    listing: Mutex<Option<std::thread::JoinHandle<Result<Membership>>>>,
    /// The ignore policy read with that listing, which then replaces
    /// `built_ignore`.
    listed_ignore: std::sync::OnceLock<Arc<SourceIgnore>>,
    extra_candidates: BTreeSet<String>,
    membership: Mutex<Option<Arc<Membership>>>,
    /// What a snapshot for keying inventories and lists when every task's
    /// file inputs lie beneath a few paths: see [`Self::listing`]. `None`
    /// when it takes the whole workspace.
    scope: Option<Arc<Scope>>,
    membership_prefixes: Mutex<HashMap<String, Arc<Vec<PathBuf>>>>,
    /// Each task's [`FilePlan`], by its id and declared inputs.
    plans: Mutex<HashMap<(String, String), Arc<FilePlan>>>,
    source_exclusions: Vec<Outputs>,
    /// Every task's scratch directories, among `source_exclusions`, which no
    /// input reads, `includeIgnored` ones included.
    scratch: Vec<Outputs>,
    cache_relative: Option<PathBuf>,
    projects: ProjectGraph,
    /// Each project by its root, to find the project owning a file.
    roots: BTreeMap<String, String>,
    generated_tasks: Vec<(Task, Outputs)>,
    task_dependencies: BTreeMap<String, BTreeSet<String>>,
    /// Each task's servers, by project and declared inputs: see [`servers`].
    servers: BTreeMap<String, Vec<(String, Vec<Value>)>>,
    /// The ignore policy read as the snapshot was built: see
    /// [`Self::source_ignore`].
    built_ignore: Arc<SourceIgnore>,
    canonical_root: PathBuf,
    workspace_prefix: Option<PathBuf>,
    patterns: Mutex<HashMap<(String, bool, bool), Arc<Pattern>>>,
    digests: Mutex<HashMap<String, (Stamp, String)>>,
    /// Files whose digests this run used, which are saved beside those of
    /// `files`, the source listing, which leaves out many of them: what
    /// `includeIgnored` filesets read, other tasks' declared outputs, and
    /// the targets of links.
    read_files: Mutex<HashSet<String>>,
    /// Whether `digests` gained entries worth saving.
    digests_changed: std::sync::atomic::AtomicBool,
    /// Directories already checked not to be symlinks.
    directories: Mutex<HashSet<PathBuf>>,
    /// Runtime input results by command and environment, computed once per run like Nx.
    runtime: Mutex<HashMap<RuntimeKey, Arc<Mutex<Option<Value>>>>>,
    /// The parsed pnpm lockfile, replaced whenever its content changes.
    lockfile: Mutex<Option<Arc<Installed>>>,
    /// Each dependency task's output files, listed when a task first asks or
    /// handed over by its restore, and dropped once a task that may have
    /// written where they lie is done.
    dependency_outputs: Mutex<Listings>,
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
    ignore_contents: BTreeMap<PathBuf, String>,
    settled: bool,
    /// When the refresh that produced this inventory began listing.
    listed: Option<Instant>,
    /// When this inventory began taking its stamps, before any of them: what
    /// it captured holds only if its stamps had settled by then.
    stamped: SystemTime,
    /// The paths this inventory and its listing cover, when not the whole
    /// workspace: see [`Scope`].
    scope: Option<Arc<Scope>>,
}

/// Workspace-relative paths a narrowed inventory and listing cover: what is
/// at each of them or beneath it. The inventory reads every directory
/// beneath them, and of each directory leading to them only the names that
/// lead on and its ignore files, which together decide what is a source
/// there. No path lies beneath another.
#[derive(PartialEq, Eq)]
struct Scope {
    paths: BTreeSet<String>,
    /// The same paths, for comparing paths by component.
    relative: BTreeSet<PathBuf>,
}

/// Where a directory lies with respect to a [`Scope`].
#[derive(PartialEq, Eq)]
enum Reach {
    /// At one of its paths or beneath it.
    Within,
    /// Above one of its paths.
    Toward,
    Outside,
}

impl Scope {
    /// The scope of `paths`, without those beneath another of them.
    fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut relative: BTreeSet<PathBuf> = BTreeSet::new();
        // In component order a path comes before those beneath it, so the
        // last one kept is the only one that can hold the next.
        for path in paths
            .into_iter()
            .map(PathBuf::from)
            .collect::<BTreeSet<_>>()
        {
            if relative.last().is_none_or(|last| !path.starts_with(last)) {
                relative.insert(path);
            }
        }
        Self {
            paths: relative
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            relative,
        }
    }

    /// Where the workspace-relative `path` lies: see [`Reach`].
    fn reach(&self, path: &Path) -> Reach {
        if path
            .ancestors()
            .any(|ancestor| self.relative.contains(ancestor))
        {
            return Reach::Within;
        }
        // Paths beneath `path` follow it in component order.
        match self.relative.range(path.to_owned()..).next() {
            Some(below) if below.starts_with(path) => Reach::Toward,
            _ => Reach::Outside,
        }
    }

    /// Whether the workspace-relative `path` is at one of its paths or
    /// beneath one.
    fn holds(&self, path: &str) -> bool {
        self.reach(Path::new(path)) == Reach::Within
    }
}

/// A candidate listing and the ignore policy captured with it. Keeping both
/// alive prevents a concurrent refresh from changing dependency-output selection.
#[derive(Clone)]
struct Candidates {
    files: Arc<BTreeSet<String>>,
    source_ignore: Arc<SourceIgnore>,
}

/// What a task's file inputs need of the workspace's files, read from its
/// declared inputs without any listing.
struct FilePlan {
    /// The paths its filesets name, when each of them names one path rather
    /// than matching, and it has no `json` glob: then every candidate it can
    /// select is at one of these paths or beneath it, and listing those alone
    /// selects what listing the workspace would.
    paths: Option<BTreeSet<String>>,
    /// The fixed directories or paths its filesets that select files start
    /// from, when each has one and it has no `json` glob: then every file it
    /// selects is at one of these or beneath it, as for `paths`, though
    /// matching rather than named. An exclusion only takes away what one
    /// that selects files would, so it needs none.
    prefixes: Option<BTreeSet<String>>,
    /// Its `dependentTasksOutputFiles` globs, each with whether it is
    /// transitive.
    dependency_outputs: Vec<(String, bool)>,
}

/// The type [`Membership::entries`] records for a directory; a file is 0 and
/// a symlink 2.
const DIRECTORY: u8 = 1;
/// Directories read at once while the inventory walks the workspace.
const WALKERS: usize = 8;
/// Threads listing source files when ignore files are read on the way. Past a
/// few, more threads add system time faster than they save time.
const LISTERS: usize = 4;
/// Paths a narrowed listing covers at most. Git matches every file it lists
/// against each path asked for: in a large monorepo, asking for 256 took
/// about as long as listing everything, and fewer cost less in proportion.
/// Below this, a narrowed listing costs at most about what the whole one does.
const SCOPE_PATHS: usize = 200;

/// What a run needs listed of the workspace's files: see [`Snapshot::listing`].
enum Listing {
    Nothing,
    Within(Scope),
    Whole,
}

/// Whether `name` is an ignore file, whose rules change which files are sources.
fn is_policy(name: &std::ffi::OsStr) -> bool {
    matches!(name.to_str(), Some(".gitignore" | ".nxignore"))
}

impl Membership {
    /// Inventory source directories and policy stamps before a new listing,
    /// of the whole workspace or of `scope` alone.
    fn new(
        root: &Path,
        files: &BTreeSet<String>,
        source_ignore: &Arc<SourceIgnore>,
        outputs: &[Outputs],
        cache: Option<&Path>,
        git_policies: Option<&BTreeSet<PathBuf>>,
        scope: Option<&Arc<Scope>>,
    ) -> Result<Self> {
        let mut output_anchors: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for (index, outputs) in outputs.iter().enumerate() {
            for anchor in outputs.anchors() {
                output_anchors
                    .entry(PathBuf::from(anchor))
                    .or_default()
                    .push(index);
            }
        }
        let stamped = SystemTime::now();
        let mut membership = Self {
            files: Arc::new(BTreeSet::new()),
            source_ignore: source_ignore.clone(),
            stamps: BTreeMap::new(),
            directories: BTreeSet::new(),
            directory_entries: BTreeMap::new(),
            root: root.to_owned(),
            outputs: outputs.to_vec(),
            output_anchors,
            excluded_roots: outputs
                .iter()
                .flat_map(Outputs::complete_roots)
                .map(PathBuf::from)
                .collect(),
            cache: cache.map(PathBuf::from),
            policies: BTreeSet::new(),
            git_policies: BTreeSet::new(),
            ignore_contents: BTreeMap::new(),
            settled: false,
            listed: None,
            stamped,
            scope: scope.cloned(),
        };
        // Finding Git's own policy files takes several Git processes, which
        // run while the directories are read. They are stamped as soon as
        // they are found: a config rewritten during the walk could name an
        // excludes file the discovered set lacks, and a stamp taken after
        // that rewrite would not show it.
        let (git_policies, git_stamps) = std::thread::scope(|scope| {
            let discovery = scope.spawn(|| {
                let policies = git_policies
                    .cloned()
                    .unwrap_or_else(|| git_membership_policies(root));
                let stamps = policies
                    .iter()
                    .map(|policy| Ok((policy.clone(), membership_stamp(policy)?)))
                    .collect::<Result<BTreeMap<_, _>>>()?;
                anyhow::Ok((policies, stamps))
            });
            membership.walk()?;
            discovery.join().expect("git policy thread panicked")
        })?;
        // Git lists tracked files even beneath ignored directories. Watching
        // only the ignore walker would miss deletion/rename of those files.
        let mut beneath = Vec::new();
        for path in files {
            for parent in Path::new(path).ancestors().skip(1) {
                let directory = root.join(parent);
                // A watched directory's parent is watched too, so the
                // nearest one watched ends the search.
                if !membership.directories.insert(directory.clone()) {
                    break;
                }
                membership
                    .stamps
                    .insert(directory.clone(), membership_stamp(&directory)?);
                for name in [".gitignore", ".nxignore"] {
                    let path = directory.join(name);
                    // Creation of a new ignore file changes its directory stamp.
                    // Existing rules can be edited without changing the directory.
                    if membership_stamp(&path)?.is_some() {
                        membership.policies.insert(path);
                    }
                }
                beneath.push(directory);
            }
        }
        for directory in root.ancestors() {
            membership.policies.insert(directory.join(".gitignore"));
            // Also detect a newly initialized repository or changed worktree pointer.
            membership.policies.insert(directory.join(".git"));
        }
        membership.policies.extend(git_policies.iter().cloned());
        membership.git_policies = git_policies;
        for policy in &membership.policies {
            let stamp = match git_stamps.get(policy) {
                Some(stamp) => stamp.clone(),
                None => membership_stamp(policy)?,
            };
            membership.stamps.insert(policy.clone(), stamp);
        }
        // Capture contents before listing, so a later policy change cannot be
        // mistaken for the rules that produced that listing.
        for policy in &membership.policies {
            if policy.file_name().is_some_and(is_policy)
                && let Some(content) = ignore_content(policy, &membership.stamps[policy])?
            {
                membership.ignore_contents.insert(policy.clone(), content);
            }
        }
        // The walk did not read these: their names are captured as any
        // later check reads them, now that every policy is known.
        for directory in beneath {
            if let Some(entries) = membership.entries(&directory)? {
                membership.directory_entries.insert(directory, entries);
            }
        }
        Ok(membership)
    }

    /// Stamp each directory from the root, then capture its names from the
    /// same read that finds its subdirectories and ignore files. A change made
    /// after the read changes the stamp, which later checks compare.
    fn walk(&mut self) -> Result<()> {
        // The directories waiting to be read, and how many are being read,
        // whose subdirectories may still join them.
        let queue = Mutex::new((vec![self.root.clone()], 0_usize));
        let changed = Condvar::new();
        let failed = AtomicBool::new(false);
        let membership = &*self;
        let read = |directory: &Path| -> Result<_> {
            let stamp = membership_stamp(directory)?;
            Ok((stamp, membership.listed(directory)?))
        };
        let walked: Vec<Result<Vec<_>>> = std::thread::scope(|scope| {
            let walkers: Vec<_> = (0..WALKERS)
                .map(|_| {
                    scope.spawn(|| {
                        let mut walked = Vec::new();
                        loop {
                            let directory = {
                                let mut queue = queue.lock().unwrap();
                                loop {
                                    if failed.load(Ordering::Relaxed) {
                                        return Ok(walked);
                                    }
                                    if let Some(directory) = queue.0.pop() {
                                        queue.1 += 1;
                                        break directory;
                                    }
                                    if queue.1 == 0 {
                                        return Ok(walked);
                                    }
                                    queue = changed.wait(queue).unwrap();
                                }
                            };
                            let result = read(&directory);
                            let mut queue = queue.lock().unwrap();
                            queue.1 -= 1;
                            changed.notify_all();
                            let (stamp, names) = match result {
                                Ok(read) => read,
                                Err(error) => {
                                    failed.store(true, Ordering::Relaxed);
                                    return Err(error);
                                }
                            };
                            for (name, kind) in names.iter().flatten() {
                                if *kind == DIRECTORY {
                                    queue.0.push(directory.join(name));
                                }
                            }
                            drop(queue);
                            walked.push((directory, stamp, names));
                        }
                    })
                })
                .collect();
            walkers
                .into_iter()
                .map(|walker| walker.join().expect("directory walker panicked"))
                .collect()
        });
        for walked in walked {
            for (directory, stamp, names) in walked? {
                self.stamps.insert(directory.clone(), stamp);
                if let Some(names) = names {
                    for (name, _) in &names {
                        // Creation of a new ignore file changes its directory stamp.
                        // Existing rules can be edited without changing the directory.
                        if is_policy(name) {
                            self.policies.insert(directory.join(name));
                        }
                    }
                    self.directory_entries.insert(directory.clone(), names);
                }
                self.directories.insert(directory);
            }
        }
        Ok(())
    }

    /// Names and types that can affect source membership in this directory.
    /// Output files may be skipped, but partially excluded output directories
    /// must remain visible because they can contain source files or ignore rules.
    fn entries(&self, directory: &Path) -> Result<Option<BTreeSet<(OsString, u8)>>> {
        let Some(names) = self.listed(directory)? else {
            return Ok(None);
        };
        for (name, kind) in &names {
            let path = directory.join(name);
            // A policy created between policy discovery and name capture must
            // get its own stamp before its in-place edits can permit reuse.
            if is_policy(name) && !self.policies.contains(&path) {
                return Ok(None);
            }
            // An empty directory created after the inventory walk would not
            // appear among source files. Never bless its parent signature until
            // that directory is itself watched by the inventory.
            if *kind == DIRECTORY && !self.directories.contains(&path) {
                return Ok(None);
            }
        }
        Ok(Some(names))
    }

    /// The names [`Self::entries`] keeps, before it checks that the inventory
    /// already watches each directory and policy among them. Of a directory
    /// leading to a narrowed inventory's scope, those are the names that lead
    /// on and its ignore files: what else it holds lists nothing there.
    fn listed(&self, directory: &Path) -> Result<Option<BTreeSet<(OsString, u8)>>> {
        let relative_directory = directory.strip_prefix(&self.root).unwrap_or(directory);
        let toward = self
            .scope
            .as_ref()
            .filter(|scope| scope.reach(relative_directory) != Reach::Within);
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
            if let Some(scope) = toward
                && !is_policy(&name)
                && scope.reach(relative) == Reach::Outside
            {
                continue;
            }
            let kind = entry.file_type()?;
            if !kind.is_dir()
                && !is_policy(&name)
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
                    DIRECTORY
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
    /// Directories and ignore files whose stamps were recent when this
    /// inventory stamped them require fresh entry names and matching contents:
    /// what it captured may predate a change their stamps do not show. Other
    /// listing policies must have settled by `looked`, a time taken before
    /// they are stamped again, for the result to say they have.
    fn state(&self, prefixes: Option<&[PathBuf]>, looked: SystemTime) -> Result<(bool, bool)> {
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
        // Each path with its stamp and, for a directory, the names captured
        // for it, if any.
        type Checked<'a> = (
            &'a PathBuf,
            &'a Option<Stamp>,
            Option<Option<&'a BTreeSet<(OsString, u8)>>>,
        );
        let paths: Box<dyn Iterator<Item = Checked>> = match selected {
            Some(selected) => Box::new(
                selected
                    .into_iter()
                    .map(|path| {
                        (
                            path,
                            &self.stamps[path],
                            Some(self.directory_entries.get(path)),
                        )
                    })
                    .chain(
                        self.policies
                            .iter()
                            .map(|path| (path, &self.stamps[path], None)),
                    ),
            ),
            None => {
                // Every directory has a stamp, and every captured listing
                // belongs to a directory: walking the three in their shared
                // order finds each path's own without searching for it.
                let mut directories = self.directories.iter().peekable();
                let mut entries = self.directory_entries.iter().peekable();
                Box::new(self.stamps.iter().map(move |(path, before)| {
                    let directory = directories
                        .next_if(|directory| *directory == path)
                        .map(|_| {
                            entries
                                .next_if(|(directory, _)| *directory == path)
                                .map(|(_, names)| names)
                        });
                    (path, before, directory)
                }))
            }
        };
        for (path, before, directory) in paths {
            let (after, window) = match membership_look(path)? {
                Some((stamp, window)) => (Some(stamp), window),
                None => (None, crate::settle::FALLBACK),
            };
            let recent = |looked| {
                after
                    .as_ref()
                    .is_some_and(|stamp| !stamp.membership_settled(window, looked))
            };
            if let Some(captured) = directory {
                if after.is_some() && captured.is_none() {
                    return Ok((false, false));
                }
                // Output publication changes its parent's timestamp too. Check
                // source-relevant names rather than relisting the whole workspace.
                // Recent directories are always read, even with an equal stamp:
                // coarse timestamps can conceal an addition or removal.
                if *before != after || recent(self.stamped) {
                    let entries = self.entries(path)?;
                    if entries.as_ref() != captured || membership_stamp(path)? != after {
                        return Ok((false, false));
                    }
                }
            } else {
                if (*before != after || recent(self.stamped))
                    && let Some(content) = self.ignore_contents.get(path)
                {
                    if ignore_content(path, &after)?.as_ref() != Some(content) {
                        return Ok((false, false));
                    }
                    continue;
                }
                if *before != after {
                    return Ok((false, false));
                }
                settled &= !recent(looked);
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

    /// List source files once this inventory has stamped directories and
    /// policies, so a change made while listing shows in a later comparison.
    /// `rebuilt` says whether the stamps were taken for this listing.
    fn list(&mut self, extra: &BTreeSet<String>, rebuilt: bool) -> Result<()> {
        let mut files = match &self.scope {
            None => source_files(&self.root)?,
            // Where Git cannot list the scope's paths apart, the workspace
            // listing would walk or ask Git of the whole: what that lists
            // in the scope is what it holds.
            Some(scope) => match source_files_at(&self.root, &scope.paths)? {
                Some(files) => files,
                None => source_files(&self.root)?
                    .into_iter()
                    .filter(|path| scope.holds(path))
                    .collect(),
            },
        };
        exclude_artifacts(&mut files, &self.outputs, self.cache.as_deref());
        let ignore = SourceIgnore::new(&self.root)?;
        files.extend(
            extra
                .iter()
                .filter(|path| {
                    !ignore.matches(path)
                        && self.scope.as_ref().is_none_or(|scope| scope.holds(path))
                })
                .cloned(),
        );
        self.source_ignore = Arc::new(ignore);
        self.files = Arc::new(files);
        // An unstable inventory or recent policy never enables the fast path.
        // Keep candidates so a rebuild also watches tracked, ignored parents.
        self.settled = rebuilt && self.covers(&self.files) && self.state(None, self.stamped)?.1;
        Ok(())
    }

    /// Whether every directory above each of `files` has a stamp. Files share
    /// parents, so each directory is looked up once.
    fn covers(&self, files: &BTreeSet<String>) -> bool {
        let mut watched: HashSet<&Path> = HashSet::default();
        files.iter().all(|path| {
            // A directory seen before was checked with all its ancestors, or
            // the search would have ended there.
            Path::new(path)
                .ancestors()
                .skip(1)
                .take_while(|parent| watched.insert(parent))
                .all(|parent| self.stamps.contains_key(&self.root.join(parent)))
        })
    }
}

/// Find Git index, exclusion and config files that can change source visibility,
/// including absent policy files whose later creation must invalidate reuse.
fn git_membership_policies(root: &Path) -> BTreeSet<PathBuf> {
    // No question needs another's answer, so the three Git processes run at
    // once: each costs about as long as reading a few hundred directories.
    let (paths, origins, excludes) = std::thread::scope(|scope| {
        let paths = scope.spawn(|| {
            paths::git(
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
            )
        });
        let origins =
            scope.spawn(|| paths::git(root, &["config", "--null", "--show-origin", "--list"]));
        let excludes = paths::git(
            root,
            &["config", "--null", "--path", "--get", "core.excludesfile"],
        );
        (
            paths.join().expect("git policy thread panicked"),
            origins.join().expect("git policy thread panicked"),
            excludes,
        )
    });
    let mut policies = BTreeSet::new();
    if let Ok(output) = paths
        && output.status.success()
        && let Ok(paths) = String::from_utf8(output.stdout)
    {
        policies.extend(paths.lines().map(PathBuf::from));
    }
    // Config can include other files. Track every origin used by Git, as
    // well as currently missing standard config and excludes files.
    if let Ok(output) = origins
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
    if let Ok(output) = excludes
        && output.status.success()
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

/// Read a regular ignore policy only when the captured metadata remains stable.
/// A digest read after a listing must never replace the pre-listing digest.
fn ignore_content(path: &Path, expected: &Option<Stamp>) -> Result<Option<String>> {
    if expected.is_none() || membership_stamp(path)? != *expected {
        return Ok(None);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok((membership_stamp(path)? == *expected).then(|| blake3::hash(&content).to_hex().to_string()))
}

/// Stamp a directory or listing policy, including a symlink's resolved target.
fn membership_stamp(path: &Path) -> Result<Option<Stamp>> {
    Ok(membership_look(path)?.map(|(stamp, _)| stamp))
}

/// [`membership_stamp`], with the window of [`crate::settle::window`] that
/// covers each file it stamps.
fn membership_look(path: &Path) -> Result<Option<(Stamp, Duration)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut stamp = Stamp::new(&metadata);
            let mut window = crate::settle::window(path, &metadata);
            if metadata.is_symlink() {
                // Follow the current chain on every check. In-place target
                // edits or retargeted intermediate links can change policy.
                match std::fs::metadata(path) {
                    Ok(target) => {
                        stamp.0.extend(Stamp::new(&target).0);
                        // Asked of the target's own filesystem, which holds a
                        // regular file or directory and is not followed again.
                        window = window.max(match std::fs::canonicalize(path) {
                            Ok(resolved) => crate::settle::window(&resolved, &target),
                            Err(_) => crate::settle::FALLBACK,
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => stamp.0[1] = -1,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some((stamp, window)))
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

/// What a task that is done may have written in the workspace, which decides
/// the listings of dependency outputs that can no longer be trusted.
#[derive(Clone)]
pub(crate) enum Written {
    /// Nothing a listing holds: it ran no command and has no scratch or warm
    /// paths, or it was restored with its outputs left as they were.
    Nothing,
    /// Nothing but these paths and what lies below them.
    Within(Vec<String>),
    /// Nothing but paths its declared outputs match, and the paths beside
    /// them with what lies below: it was restored, or ran a command, which is
    /// trusted to write only its declared outputs, after its scratch
    /// directories were emptied and its warm state restored.
    Declared(Box<Outputs>, Vec<String>),
    /// Anything: it ran a command that declares no outputs.
    Anything,
}

impl Written {
    /// Whether what was written may touch a listing anchored at `anchors`.
    fn touches(&self, anchors: &[String]) -> bool {
        match self {
            Written::Nothing => false,
            Written::Within(written) => written
                .iter()
                .any(|a| anchors.iter().any(|b| overlaps_path(a, b))),
            Written::Declared(outputs, beside) => anchors.iter().any(|anchor| {
                outputs.reaches(anchor) || beside.iter().any(|path| overlaps_path(path, anchor))
            }),
            Written::Anything => true,
        }
    }
}

impl std::fmt::Debug for Written {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Written::Nothing => f.write_str("Nothing"),
            Written::Within(paths) => f.debug_tuple("Within").field(paths).finish(),
            Written::Declared(outputs, beside) => f
                .debug_tuple("Declared")
                .field(&outputs.globs())
                .field(beside)
                .finish(),
            Written::Anything => f.write_str("Anything"),
        }
    }
}

/// Whether one of two workspace-relative paths is the other or lies below it.
fn overlaps_path(a: &str, b: &str) -> bool {
    a == b
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
}

/// The dependency-output listings a snapshot keeps, and what each task done
/// so far may have written, in the order they were done.
#[derive(Default)]
struct Listings {
    by_task: HashMap<String, Arc<DependencyOutputs>>,
    /// Kept for the run, so a listing taken while a task was being done is
    /// checked against what that task wrote: see [`Listings::still_valid`].
    writes: Vec<Written>,
}

impl Listings {
    /// Whether a listing anchored at `anchors`, which began once the first
    /// `since` writes were done, misses nothing a later one wrote.
    fn still_valid(&self, since: usize, anchors: &[String]) -> bool {
        self.writes[since.min(self.writes.len())..]
            .iter()
            .all(|written| !written.touches(anchors))
    }
}

/// The output files a task's restore left in the worktree, handed to the
/// snapshot so that its dependents need not list them again.
pub(crate) struct RestoredOutputs {
    /// How many writes the snapshot had seen before the restore began: see
    /// [`Snapshot::writes_seen`].
    pub since: usize,
    /// Its output paths that are not directories, symlinks among them.
    pub files: Vec<String>,
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

/// Every task's declared outputs, warm paths and scratch directories, which no
/// source listing includes, and the scratch directories among them.
fn source_exclusions(workspace: &Workspace, graph: &TaskGraph) -> (Vec<Outputs>, Vec<Outputs>) {
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
    // Scratch directories, which each run empties: what is in them is no
    // input, before or after the task runs.
    let mut scratch_paths = Vec::new();
    for task in graph.tasks.values() {
        let scratch: Vec<String> = crate::scratch::directories(workspace, task)
            .unwrap_or_default()
            .iter()
            .filter_map(|directory| {
                let relative = directory.strip_prefix(&workspace.root).ok()?.to_str()?;
                let named = crate::glob::escape(&relative.replace('\\', "/"));
                Some([named.clone(), format!("{named}/**")])
            })
            .flatten()
            .collect();
        if !scratch.is_empty()
            && let Ok(paths) = Outputs::from_paths(&scratch)
        {
            source_exclusions.push(paths.clone());
            scratch_paths.push(paths);
        }
    }
    (source_exclusions, scratch_paths)
}

impl Snapshot {
    /// A snapshot that lists source files on their own, for inspecting
    /// inputs. Its first refresh, if any, lists them again with a directory
    /// inventory to compare later refreshes against.
    pub fn new(workspace: &Workspace, graph: &TaskGraph, cache_path: &Path) -> Result<Self> {
        Self::build(workspace, graph, cache_path, false)
    }

    /// A snapshot for keying tasks. When a task needs the workspace's files
    /// listed, the listing is taken after the directory inventory a refresh
    /// compares against, so the first task to key reuses it instead of
    /// listing the workspace again.
    pub fn watched(workspace: &Workspace, graph: &TaskGraph, cache_path: &Path) -> Result<Self> {
        Self::build(workspace, graph, cache_path, true)
    }

    fn build(
        workspace: &Workspace,
        graph: &TaskGraph,
        cache_path: &Path,
        watched: bool,
    ) -> Result<Self> {
        let canonical_root = workspace.root.canonicalize()?;
        let cache_relative = cache_path
            .strip_prefix(&workspace.root)
            .ok()
            .map(PathBuf::from);
        let (source_exclusions, scratch_paths) = source_exclusions(workspace, graph);
        let mut snapshot = Self {
            root: workspace.root.clone(),
            files: std::sync::OnceLock::new(),
            listing: Mutex::default(),
            listed_ignore: std::sync::OnceLock::new(),
            extra_candidates: BTreeSet::new(),
            membership: Mutex::default(),
            scope: None,
            membership_prefixes: Mutex::default(),
            plans: Mutex::default(),
            source_exclusions,
            scratch: scratch_paths,
            cache_relative,
            projects: ProjectGraph::build(workspace)?,
            roots: workspace
                .projects
                .values()
                .map(|project| (project.root.clone(), project.name.clone()))
                .collect(),
            built_ignore: Arc::new(SourceIgnore::new(&workspace.root)?),
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
            servers: graph
                .tasks
                .keys()
                .map(|id| (id.clone(), servers(graph, id)))
                .collect(),
            canonical_root: canonical_root.clone(),
            workspace_prefix: None,
            patterns: Mutex::default(),
            digests: Mutex::default(),
            digests_changed: Default::default(),
            read_files: Mutex::default(),
            directories: Mutex::default(),
            runtime: Mutex::default(),
            lockfile: Mutex::default(),
            dependency_outputs: Mutex::default(),
        };
        // Every early task needs these, so loading saved digests and parsing
        // the lockfile overlap listing files.
        let (lockfile, (digests, workspace_prefix)) = std::thread::scope(|scope| {
            let lockfile = scope.spawn(|| parse_lockfile(&workspace.root.join("pnpm-lock.yaml")));
            let digests = scope.spawn(|| {
                let digests = load_digests(&workspace.root);
                let workspace_prefix = paths::git_path(&workspace.root, "--show-toplevel")
                    .and_then(|root| root.canonicalize().ok())
                    .and_then(|root| canonical_root.strip_prefix(root).ok().map(PathBuf::from));
                (digests, workspace_prefix)
            });
            // A task whose file inputs name paths lists only those, so a run
            // of such tasks alone never lists the workspace. Without an
            // inventory, each task's refresh takes one, and reports why it
            // cannot, as for a plain listing.
            match snapshot.listing(workspace, graph) {
                Listing::Nothing => {}
                Listing::Within(scope) if watched => {
                    snapshot.scope = Some(Arc::new(scope));
                    snapshot.watch();
                }
                _ if watched => snapshot.watch(),
                // Inspection resolves every task from the whole listing.
                _ => {
                    snapshot.files()?;
                }
            }
            anyhow::Ok((
                lockfile.join().expect("lockfile thread panicked"),
                digests.join().expect("digest thread panicked"),
            ))
        })?;
        snapshot.lockfile = Mutex::new(lockfile);
        snapshot.digests = Mutex::new(digests);
        snapshot.workspace_prefix = workspace_prefix;
        Ok(snapshot)
    }

    /// Starts listing the workspace's files after taking the directory
    /// inventory a refresh compares against, as a refresh does, to keep both
    /// for the first task that keys. Tasks that need neither are keyed
    /// meanwhile; [`Self::settle`] waits for them.
    fn watch(&mut self) {
        // Taken before the inventory begins, so a refresh judges the listing
        // no more current than it is.
        let listed = Instant::now();
        let root = self.root.clone();
        let ignore = self.built_ignore.clone();
        let exclusions = self.source_exclusions.clone();
        let cache = self.cache_relative.clone();
        let extra = self.extra_candidates.clone();
        let scope = self.scope.clone();
        let listing = std::thread::spawn(move || {
            let _listing = crate::profile::span("snapshot", "inventory_list");
            // No earlier listing names tracked files beneath directories the
            // walk skips, so such a file leaves the inventory unsettled and
            // the first refresh watches its parents.
            let mut membership = Membership::new(
                &root,
                &BTreeSet::new(),
                &ignore,
                &exclusions,
                cache.as_deref(),
                None,
                scope.as_ref(),
            )?;
            membership.list(&extra, true)?;
            membership.listed = Some(listed);
            Ok(membership)
        });
        *self.listing.get_mut().unwrap() = Some(listing);
    }

    /// Waits for the listing [`Self::watch`] started, if it has not been
    /// waited for, and keeps it and its inventory. Without them, the
    /// listing is taken on first use and each refresh takes an inventory,
    /// reporting why it cannot.
    fn settle(&self) {
        let mut listing = self.listing.lock().unwrap();
        let Some(pending) = listing.take() else {
            return;
        };
        let _waiting = crate::profile::span("snapshot", "inventory_wait");
        let Ok(membership) = pending.join().expect("listing thread panicked") else {
            return;
        };
        let _ = self.listed_ignore.set(membership.source_ignore.clone());
        // A narrowed listing is no initial candidate list, which is the
        // whole workspace's: that is listed on first use, if any.
        if membership.scope.is_none() {
            let _ = self.files.set(Ok((*membership.files).clone()));
        }
        *self.membership.lock().unwrap() = Some(Arc::new(membership));
    }

    /// The ignore policy that decides which files are sources: the one read
    /// with the snapshot's listing, once it is taken.
    fn source_ignore(&self) -> &Arc<SourceIgnore> {
        self.settle();
        self.listed_ignore.get().unwrap_or(&self.built_ignore)
    }

    /// What a run of `graph`'s tasks needs listed of the workspace's files:
    /// nothing when each task's filesets name paths, which are listed for it
    /// alone; the whole workspace for a task with a fileset that has no
    /// fixed directory, or one a recording may key, whose declared selection
    /// is compared with what it read; and otherwise the directories the
    /// tasks' filesets start from, together, unless there are so many of
    /// them that asking Git for each would cost what listing everything does.
    fn listing(&self, workspace: &Workspace, graph: &TaskGraph) -> Listing {
        let mut prefixes = BTreeSet::new();
        for task in graph.tasks.values() {
            // One in mode "off" is neither recorded nor keyed by a recording;
            // one whose settings cannot be read is taken as recorded.
            if !matches!(crate::ultracache::Settings::of(task), Ok(None)) {
                return Listing::Whole;
            }
            match self.file_plan(workspace, task) {
                Ok(plan) if plan.paths.is_some() => {}
                Ok(plan) => match &plan.prefixes {
                    Some(paths) => prefixes.extend(paths.iter().cloned()),
                    None => return Listing::Whole,
                },
                Err(_) => return Listing::Whole,
            }
        }
        if prefixes.is_empty() {
            return Listing::Nothing;
        }
        let scope = Scope::new(prefixes);
        if scope.paths.len() > SCOPE_PATHS {
            return Listing::Whole;
        }
        Listing::Within(scope)
    }

    /// The initial candidate list: the workspace's source files, without
    /// declared artifacts and cache storage, and with added candidates.
    pub(crate) fn files(&self) -> Result<&BTreeSet<String>> {
        self.settle();
        self.files
            .get_or_init(|| {
                let mut files = source_files(&self.root).map_err(|error| format!("{error:#}"))?;
                exclude_artifacts(
                    &mut files,
                    &self.source_exclusions,
                    self.cache_relative.as_deref(),
                );
                files.extend(
                    self.extra_candidates
                        .iter()
                        .filter(|path| !self.source_ignore().matches(path))
                        .cloned(),
                );
                Ok(files)
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// What `task`'s file inputs need of the workspace's files: see
    /// [`FilePlan`]. Env and runtime inputs are not evaluated.
    fn file_plan(&self, workspace: &Workspace, task: &Task) -> Result<Arc<FilePlan>> {
        // By declared inputs too: a task's key without some of its inputs
        // resolves under the same id.
        let key = (
            task.id.clone(),
            serde_json::to_string(&task.definition.inputs)?,
        );
        if let Some(plan) = self.plans.lock().unwrap().get(&key) {
            return Ok(plan.clone());
        }
        let empty = BTreeSet::new();
        let cancelled = AtomicBool::new(false);
        let mut resolver = Resolver::new(self, workspace, None, &cancelled, &empty);
        resolver.task_inputs(task)?;
        // A fileset of the workspace root, or a path that is no plain
        // relative one, is left to the listing.
        let plain = |pattern: &FilePattern| {
            !pattern.prefix.is_empty()
                && pattern
                    .prefix
                    .split('/')
                    .all(|segment| !matches!(segment, "" | "." | ".."))
        };
        let patterns = || resolver.scopes.values().flat_map(|scope| &scope.patterns);
        let listable = resolver.json_globs.is_empty();
        let paths = listable
            .then(|| {
                patterns()
                    .map(|pattern| {
                        (pattern.rest.is_none() && plain(pattern)).then(|| pattern.prefix.clone())
                    })
                    .collect::<Option<BTreeSet<_>>>()
            })
            .flatten();
        let prefixes = listable
            .then(|| {
                patterns()
                    .filter(|pattern| !pattern.excluded)
                    .map(|pattern| plain(pattern).then(|| pattern.prefix.clone()))
                    .collect::<Option<BTreeSet<_>>>()
            })
            .flatten();
        let dependency_outputs = resolver
            .values
            .iter()
            .filter_map(|(name, transitive)| {
                name.strip_prefix("dependentTasksOutputFiles:")
                    .map(|pattern| (pattern.to_owned(), transitive == &Value::Bool(true)))
            })
            .collect();
        let plan = Arc::new(FilePlan {
            paths,
            prefixes,
            dependency_outputs,
        });
        self.plans.lock().unwrap().insert(key, plan.clone());
        Ok(plan)
    }

    /// The candidates [`Self::current_files_for`] would list at `paths` and
    /// beneath them, with their policy, listing nothing else. `None` when
    /// they cannot be listed apart from the rest.
    fn candidates_at(&self, paths: &BTreeSet<String>) -> Result<Option<Candidates>> {
        let mut files = if paths.is_empty() {
            BTreeSet::new()
        } else {
            match source_files_at(&self.root, paths)? {
                Some(files) => files,
                None => return Ok(None),
            }
        };
        exclude_artifacts(
            &mut files,
            &self.source_exclusions,
            self.cache_relative.as_deref(),
        );
        let ignore = SourceIgnore::new(&self.root)?;
        files.extend(
            self.extra_candidates
                .iter()
                .filter(|path| within(paths, path) && !ignore.matches(path))
                .cloned(),
        );
        Ok(Some(Candidates {
            files: Arc::new(files),
            source_ignore: Arc::new(ignore),
        }))
    }

    /// The candidates `task`'s filesets select from. Filesets that each name
    /// a path select only what is at that path or beneath it, so listing
    /// those paths alone selects what listing the workspace would. Filesets
    /// that match beneath fixed directories select only there too, so the
    /// snapshot's narrowed listing serves them when it covers those.
    fn task_candidates(&self, workspace: &Workspace, task: &Task) -> Result<Candidates> {
        let plan = self.file_plan(workspace, task).ok();
        if let Some(paths) = plan.as_ref().and_then(|plan| plan.paths.as_ref()) {
            let _listing = crate::profile::span(&task.id, "inventory_paths");
            if let Some(candidates) = self.candidates_at(paths)? {
                return Ok(candidates);
            }
        }
        let scope = self.scope.as_ref().filter(|scope| {
            plan.as_ref()
                .and_then(|plan| plan.prefixes.as_ref())
                .is_some_and(|prefixes| prefixes.iter().all(|prefix| scope.holds(prefix)))
        });
        let prefixes = self.membership_prefixes(workspace, task)?;
        self.current_files_for(&workspace.root, Some(&prefixes), scope, &task.id)
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
                .map(|pattern| &pattern.prefix)
                .chain(resolver.json_globs.iter().map(|glob| &glob.prefix))
                .map(|prefix| workspace.root.join(prefix))
                .collect(),
        );
        prefixes.insert(task.id.clone(), Arc::clone(&paths));
        Ok(paths)
    }

    /// The directories a task's `includeIgnored` filesets read from, as
    /// workspace-relative paths: each positive glob's leading literal
    /// segments.
    pub(crate) fn include_ignored_roots(
        &self,
        workspace: &Workspace,
        task: &Task,
    ) -> Result<Vec<String>> {
        let empty = BTreeSet::new();
        let cancelled = AtomicBool::new(false);
        let mut resolver = Resolver::new(self, workspace, None, &cancelled, &empty);
        resolver.task_inputs(task)?;
        Ok(resolver
            .ignored
            .values()
            .flatten()
            .filter(|glob| !glob.starts_with('!'))
            .map(|glob| crate::ignored::partition(glob.trim_end_matches('/')).0)
            .collect())
    }

    /// Reuse membership while source directories and listing policy are unchanged.
    /// Content changes are checked separately by the file digest cache.
    #[cfg(test)]
    fn current_files(&self, root: &Path) -> Result<Arc<BTreeSet<String>>> {
        Ok(self.current_files_for(root, None, None, "test")?.files)
    }

    /// Refresh candidates as needed, returning their policy in the same snapshot.
    /// With `scope`, which holds every path the caller selects from, the
    /// candidates may be those of the scope alone; without it, they are the
    /// whole workspace's.
    fn current_files_for(
        &self,
        root: &Path,
        prefixes: Option<&[PathBuf]>,
        scope: Option<&Arc<Scope>>,
        task: &str,
    ) -> Result<Candidates> {
        let requested = Instant::now();
        self.settle();
        // Whether an inventory, and the listing taken with it, covers what
        // the caller selects from: a narrowed one never serves a caller
        // reaching beyond its scope, which needs another taken.
        let serves = |entry: &Membership| match (&entry.scope, scope) {
            (None, _) => true,
            (Some(covered), Some(scope)) => Arc::ptr_eq(covered, scope),
            (Some(_), None) => false,
        };
        loop {
            let previous = self.membership.lock().unwrap().clone();
            // Immutable entries let independent fingerprints validate their
            // directory stamps concurrently. Only a refresh needs serialization.
            let (unchanged, settled) = match &previous {
                Some(entry) if serves(entry) => entry.state(prefixes, SystemTime::now())?,
                _ => (false, false),
            };
            if settled && previous.as_ref().unwrap().settled {
                return Ok(previous.as_ref().unwrap().candidates());
            }
            // A refresh holds this lock while it lists the workspace, so time
            // spent here is time waiting for another task's listing.
            let waiting = crate::profile::span(task, "inventory_wait");
            let mut cached = self.membership.lock().unwrap();
            drop(waiting);
            // A listing that began after this call reflects every change made
            // before it, so it is as current as one made now. Tasks queued
            // behind a refresh share the next one instead of each listing the
            // workspace in turn.
            if let Some(entry) = &*cached
                && entry.listed.is_some_and(|listed| listed >= requested)
                && serves(entry)
            {
                return Ok(entry.candidates());
            }
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
            let _listing = crate::profile::span(task, "inventory_list");
            let listed = Instant::now();
            // Recent listing policies may conceal edits on coarse filesystems.
            // Re-list sources while they settle, then rebuild the directory inventory
            // once before enabling reuse (including any newly created empty dirs).
            // Recent directories are checked by freshly reading their child names.
            let rebuilt = !unchanged || settled;
            let empty = BTreeSet::new();
            let mut membership = if !rebuilt {
                Arc::unwrap_or_clone(cached.take().unwrap())
            } else {
                // Capture directories before listing files, then compare afterward.
                // An addition during listing must not hide behind a newer stamp.
                // A narrowed one is taken for a caller it may serve, without
                // the whole listing, which an inventory of the same scope
                // lacking leaves it unsettled until the next.
                Membership::new(
                    root,
                    match cached.as_ref() {
                        Some(entry) if entry.scope.as_ref() == scope => &entry.files,
                        _ if scope.is_some() => &empty,
                        _ => self.files()?,
                    },
                    self.source_ignore(),
                    &self.source_exclusions,
                    self.cache_relative.as_deref(),
                    cached
                        .as_ref()
                        .filter(|entry| unchanged && entry.settled)
                        .map(|entry| &entry.git_policies),
                    scope,
                )?
            };
            membership.list(&self.extra_candidates, rebuilt)?;
            let candidates = membership.candidates();
            membership.listed = Some(listed);
            *cached = Some(Arc::new(membership));
            return Ok(candidates);
        }
    }

    /// Saves the digests of files that still exist, for the next run: those
    /// the run listed or read. A run that listed no files keeps every
    /// digest, until one that lists them.
    pub fn save_digests(&self, root: &Path) {
        if !self
            .digests_changed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        self.settle();
        let digests = self.digests.lock().unwrap();
        let read = self.read_files.lock().unwrap();
        let files = match self.files.get() {
            Some(Ok(files)) => Some(files),
            _ => None,
        };
        let entries: BTreeMap<&String, (&Stamp, &String)> = digests
            .iter()
            .filter(|(path, _)| {
                files.is_none_or(|files| files.contains(*path)) || read.contains(*path)
            })
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
        paths::safe_parents_once(root, path, "input", &self.directories)
    }

    fn installed(&self, root: &Path, task: &str) -> Result<Option<Arc<Installed>>> {
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
        let parsing = crate::profile::span(task, "lockfile_parse");
        let parsed = parse_lockfile(&absolute);
        drop(parsing);
        let Some(installed) = parsed else {
            return Ok(None);
        };
        *current = Some(installed.clone());
        Ok(Some(installed))
    }

    /// Adds paths to the candidate files, such as files deleted since a base
    /// revision.
    pub fn with_candidates(mut self, paths: &[String]) -> Self {
        self.settle();
        let ignore = self.source_ignore().clone();
        // Files listed later take them as they are listed.
        if let Some(Ok(files)) = self.files.get_mut() {
            files.extend(paths.iter().filter(|path| !ignore.matches(path)).cloned());
        }
        self.extra_candidates.extend(paths.iter().cloned());
        // A listing taken with its inventory adds them as a refresh would.
        if let Some(membership) = self.membership.get_mut().unwrap() {
            let membership = Arc::make_mut(membership);
            let ignore = membership.source_ignore.clone();
            let scope = membership.scope.clone();
            Arc::make_mut(&mut membership.files).extend(
                paths
                    .iter()
                    .filter(|path| {
                        !ignore.matches(path)
                            && scope.as_ref().is_none_or(|scope| scope.holds(path))
                    })
                    .cloned(),
            );
            membership.settled &= membership.covers(&membership.files);
        }
        self
    }

    fn pattern(&self, pattern: &str, negated: bool) -> Result<Arc<Pattern>> {
        self.compiled(pattern, negated, false)
    }

    /// A fileset's pattern, see [`Pattern::fileset`].
    fn fileset_pattern(&self, pattern: &str, negated: bool) -> Result<Arc<Pattern>> {
        self.compiled(pattern, negated, true)
    }

    fn compiled(&self, pattern: &str, negated: bool, fileset: bool) -> Result<Arc<Pattern>> {
        let key = (pattern.to_owned(), negated, fileset);
        if let Some(pattern) = self.patterns.lock().unwrap().get(&key) {
            return Ok(pattern.clone());
        }
        let compiled = Arc::new(if fileset {
            Pattern::fileset(pattern, negated)?
        } else {
            Pattern::new(pattern, negated)?
        });
        self.patterns.lock().unwrap().insert(key, compiled.clone());
        Ok(compiled)
    }

    fn digest(&self, path: &str, absolute: &Path, metadata: &std::fs::Metadata) -> Result<String> {
        self.read_files.lock().unwrap().insert(path.to_owned());
        let stamp = Stamp::new(metadata);
        if let Some((known, digest)) = self.digests.lock().unwrap().get(path)
            && *known == stamp
        {
            return Ok(digest.clone());
        }
        // Before reading: a write the window lets keep the stamp, made while
        // the file is read or after, would leave a digest of other contents.
        let reading = SystemTime::now();
        let digest = digest_file(absolute)?;
        // Re-check after reading so a concurrent write is never recorded as settled.
        let after = Stamp::new(&std::fs::symlink_metadata(absolute)?);
        if after == stamp && stamp.settled(crate::settle::window(absolute, metadata), reading) {
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
        let since = {
            let listed = self.dependency_outputs.lock().unwrap();
            if let Some(outputs) = listed.by_task.get(&task.id) {
                return Ok(outputs.clone());
            }
            listed.writes.len()
        };
        let outputs = Arc::new(self.fresh_dependency_outputs(workspace, task)?);
        let mut listed = self.dependency_outputs.lock().unwrap();
        // A task done while this one listed may have written what it read.
        if !listed.still_valid(since, &outputs.anchors) {
            return Ok(outputs);
        }
        Ok(listed
            .by_task
            .entry(task.id.clone())
            .or_insert(outputs)
            .clone())
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

    /// How many tasks that may have written something are done, for
    /// [`RestoredOutputs::since`].
    pub(crate) fn writes_seen(&self) -> usize {
        self.dependency_outputs.lock().unwrap().writes.len()
    }

    /// Once `task` is done: drops the listings what it `written` may have
    /// touched, its own among them, and keeps the listing its restore made,
    /// unless a task done since that restore began may have touched it.
    pub(crate) fn outputs_written(
        &self,
        workspace: &Workspace,
        task: &Task,
        written: Written,
        restored: Option<RestoredOutputs>,
    ) {
        let restored = restored.and_then(|restored| {
            let anchors: Vec<String> = Outputs::new(workspace, task)
                .ok()?
                .anchors()
                .map(str::to_owned)
                .collect();
            Some((restored.since, anchors, restored.files))
        });
        let mut listed = self.dependency_outputs.lock().unwrap();
        let restored = restored
            .filter(|(since, anchors, _)| listed.still_valid(*since, anchors))
            .map(|(_, anchors, files)| DependencyOutputs {
                anchors,
                files,
                values: Mutex::default(),
            });
        if !matches!(written, Written::Nothing) {
            listed
                .by_task
                .retain(|_, outputs| !written.touches(&outputs.anchors));
            listed.writes.push(written);
        }
        if let Some(restored) = restored {
            listed.by_task.insert(task.id.clone(), Arc::new(restored));
        }
    }

    /// A file an `includeIgnored` fileset read, by the content it points to
    /// wherever that is, as Nx reads it; `None` when it is gone.
    fn ignored_value(&self, root: &Path, path: &str) -> Result<Option<Value>> {
        let absolute = root.join(path);
        let metadata = match std::fs::metadata(&absolute) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mode = crate::store::mode(&metadata) & 0o111 != 0;
        Ok(Some(
            json!({"content":self.digest(path, &absolute, &metadata)?, "mode":mode}),
        ))
    }

    /// A file a recording read, keyed as an input file is.
    pub(crate) fn recorded_value(&self, root: &Path, path: &str) -> Result<Value> {
        self.file_value(root, path)
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

/// Whether `path` is one of `paths` or beneath one, as [`under`] selects.
fn within(paths: &BTreeSet<String>, path: &str) -> bool {
    paths.iter().any(|prefix| {
        path.strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
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
    // Nx negations can reinclude Git-ignored files, so this policy needs
    // the walker regardless of what Git lists. Avoid launching Git here.
    let git = if root.join(".nxignore").is_file() {
        None
    } else {
        git_sources(root, &["."])
    };
    let candidates = match git {
        Some(listed) => listed?,
        None => walked_sources(root, |_| true)?,
    };
    visible_sources(root, candidates)
}

/// What [`source_files`] lists at the workspace-relative `paths` and beneath
/// them, reading only the directories that lead to them and those beneath
/// them. `None` when Git cannot list them, where `source_files` would walk.
fn source_files_at(root: &Path, paths: &BTreeSet<String>) -> Result<Option<BTreeSet<String>>> {
    let candidates = if root.join(".nxignore").is_file() {
        // The walker decides on each entry from the ignore files of its own
        // directory and those above it, so skipping every other directory
        // leaves what it decides here as it is.
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        walked_sources(root, move |relative| {
            paths
                .iter()
                .any(|path| path.starts_with(relative) || relative.starts_with(path))
        })?
    } else {
        let specs: Vec<String> = paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect();
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        match git_sources(root, &specs) {
            Some(listed) => listed?,
            None => return Ok(None),
        }
    };
    let candidates = candidates
        .into_iter()
        .filter(|path| within(paths, path))
        .collect();
    Ok(Some(visible_sources(root, candidates)?))
}

/// The files Git lists for `pathspecs`, tracked or not ignored, without
/// those deleted from the working tree. `None` when Git cannot list them.
fn git_sources(root: &Path, pathspecs: &[&str]) -> Option<Result<BTreeSet<String>>> {
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
    let (listed, deleted) = std::thread::scope(|scope| {
        let deleted = scope.spawn(|| {
            let mut args = vec!["ls-files", "--deleted", "-z", "--"];
            args.extend(pathspecs);
            paths::git(root, &args)
        });
        let mut args = vec![
            "ls-files",
            "-v",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ];
        args.extend(pathspecs);
        let listed = paths::git(root, &args);
        (
            lines(listed),
            lines(deleted.join().expect("git listing thread panicked")),
        )
    });
    let (listed, deleted) = (listed?, deleted?);
    Some((|| {
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
        Ok(files)
    })())
}

/// The files the ignore walker finds, entering only the directories whose
/// workspace-relative path `enter` accepts.
fn walked_sources(
    root: &Path,
    enter: impl Fn(&Path) -> bool + Send + Sync + 'static,
) -> Result<BTreeSet<String>> {
    // Directories are read on several threads; each keeps the files it finds
    // and adds them to the shared listing when it finishes.
    let found = Mutex::new((Vec::new(), None));
    let walked = root.to_owned();
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .ignore(false)
        .add_custom_ignore_filename(".nxignore")
        .follow_links(false)
        .filter_entry(move |entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".qk" | "node_modules")
            ) && enter(entry.path().strip_prefix(&walked).unwrap_or(entry.path()))
        })
        .threads(LISTERS)
        .build_parallel()
        .visit(&mut WalkedFiles {
            root,
            found: &found,
            files: Vec::new(),
        });
    let (files, error) = found.into_inner().unwrap();
    if let Some(error) = error {
        return Err(error);
    }
    Ok(files.into_iter().collect())
}

/// `candidates` without root ignore rules' files and runner state.
fn visible_sources(root: &Path, candidates: BTreeSet<String>) -> Result<BTreeSet<String>> {
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

/// The files one thread of [`source_files`]' walk finds, added to `found` when
/// the thread finishes, with the first error any thread meets.
struct WalkedFiles<'a> {
    root: &'a Path,
    found: &'a Mutex<(Vec<String>, Option<anyhow::Error>)>,
    files: Vec<String>,
}

impl<'a> ignore::ParallelVisitorBuilder<'a> for WalkedFiles<'a> {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 'a> {
        Box::new(WalkedFiles {
            root: self.root,
            found: self.found,
            files: Vec::new(),
        })
    }
}

impl ignore::ParallelVisitor for WalkedFiles<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let path = entry.map_err(anyhow::Error::from).and_then(|entry| {
            Ok(if entry.file_type().is_some_and(|kind| !kind.is_dir()) {
                Some(paths::relative(self.root, entry.path())?)
            } else {
                None
            })
        });
        match path {
            Ok(path) => {
                self.files.extend(path);
                ignore::WalkState::Continue
            }
            Err(error) => {
                self.found.lock().unwrap().1.get_or_insert(error);
                ignore::WalkState::Quit
            }
        }
    }
}

impl Drop for WalkedFiles<'_> {
    fn drop(&mut self) {
        // A panicking thread poisons the lock; the walk fails regardless.
        if let Ok(mut found) = self.found.lock() {
            found.0.append(&mut self.files);
        }
    }
}

/// The text of `path` as a `json` input keys it: only `fields` when given,
/// without `exclude`. A `.yaml` or `.yml` file is read as YAML, and any other
/// as JSON or JSONC. `None` when it cannot be read so.
pub(crate) fn json_projection(path: &str, text: &str, fields: &JsonFields) -> Option<Value> {
    let mut value: Value = if path.ends_with(".yaml") || path.ends_with(".yml") {
        serde_yaml_ng::from_str(text).ok()?
    } else {
        jsonc_parser::parse_to_serde_value(text, &Default::default()).ok()?
    };
    if let Some(fields) = &fields.fields {
        let mut kept = json!({});
        for field in fields {
            if let Some(found) = json_path(&value, field) {
                set_json_path(&mut kept, field, found.clone());
            }
        }
        value = kept;
    }
    for field in &fields.exclude {
        remove_json_path(&mut value, field);
    }
    Some(value)
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
    /// The glob after `prefix`, the literal directory it starts with; `None`
    /// when the pattern names one path.
    rest: Option<String>,
    excluded: bool,
    /// The project a project fileset selects the files of.
    owner: Option<String>,
}

/// The patterns of a task's declared filesets, by the scope they select in:
/// kept to tell whether a file where there is none would be selected, as
/// [`Coverage`] asks.
#[derive(Default)]
pub struct Selection {
    scopes: Vec<Vec<FilePattern>>,
}

impl Selection {
    /// Whether a file at `path` would be selected, gitignore aside: an
    /// included pattern of a scope matches it, and none of that scope's
    /// exclusions does.
    fn selects(&self, snapshot: &Snapshot, path: &str) -> bool {
        let applies = |pattern: &FilePattern| {
            pattern.matcher.is_match(path)
                && (pattern.owner.is_none() || snapshot.owner(path) == pattern.owner.as_deref())
        };
        self.scopes.iter().any(|patterns| {
            patterns
                .iter()
                .any(|pattern| !pattern.excluded && applies(pattern))
                && !patterns
                    .iter()
                    .any(|pattern| pattern.excluded && applies(pattern))
        })
    }

    /// Whether every entry `directory` could hold, at any depth, would be
    /// selected, gitignore aside: an included pattern of a scope takes all of
    /// what is under a directory at or above it, and none of that scope's
    /// exclusions reaches into it. `""` is the workspace root.
    fn selects_every_entry(&self, snapshot: &Snapshot, directory: &str) -> bool {
        let within = |prefix: &str| {
            prefix.is_empty()
                || directory == prefix
                || directory
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let reaches = |prefix: &str| {
            within(prefix)
                || directory.is_empty()
                || prefix
                    .strip_prefix(directory)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let owner = snapshot.owner(if directory.is_empty() { "." } else { directory });
        self.scopes.iter().any(|patterns| {
            patterns.iter().any(|pattern| {
                !pattern.excluded
                    && within(&pattern.prefix)
                    && matches!(pattern.rest.as_deref(), Some("**" | "**/*"))
                    && (pattern.owner.is_none() || owner == pattern.owner.as_deref())
            }) && !patterns
                .iter()
                .any(|pattern| pattern.excluded && reaches(&pattern.prefix))
        })
    }
}

/// What a task's declared key holds of the workspace's files, to tell
/// whether it keys everything a recording says the task read, looked for
/// and listed: when it does, a result under the declared key is as sound as
/// qk's keys are without recordings. A file qk keys by part of its content,
/// such as a dependency's `package.json` or a `json` input, counts as keyed,
/// as it does for every task's key.
pub(crate) struct Coverage {
    resolved: Resolved,
    /// Each `dependentTasksOutputFiles` glob, with the declared outputs of
    /// the tasks it reads.
    dependency_outputs: Vec<(Arc<Pattern>, Vec<Outputs>)>,
}

/// What `task`'s declared key covers: see [`Coverage`].
pub(crate) fn coverage(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    prepared: &PreparedTask,
    cancelled: &AtomicBool,
) -> Result<Coverage> {
    let resolved = resolve(snapshot, workspace, task, Some(prepared), cancelled)?;
    let selections: Vec<(String, bool)> = resolved
        .dependency_output_inputs()
        .map(|(pattern, transitive)| (pattern.to_owned(), transitive))
        .collect();
    let mut dependency_outputs = Vec::new();
    for (pattern, transitive) in selections {
        let matcher = snapshot.pattern(&pattern, false)?;
        let selected = graph.output_dependencies(task, transitive);
        let mut outputs = Vec::new();
        for id in selected {
            let declared = Outputs::new(workspace, dependency_task(graph, &id)?)?;
            if declared.declared() {
                outputs.push(declared);
            }
        }
        dependency_outputs.push((matcher, outputs));
    }
    Ok(Coverage {
        resolved,
        dependency_outputs,
    })
}

impl Coverage {
    /// Whether the declared key holds the content of the file at `path`, or
    /// what of it reaches tasks, by its declared inputs or by what qk adds.
    fn keyed(&self, path: &str) -> bool {
        let inferred = &self.resolved.inferred;
        self.resolved.is_whole_file(path)
            || self.resolved.is_json_input(path)
            || inferred.manifests.contains_key(path)
            || inferred.tsconfig.as_deref() == Some(path)
            || inferred.settings.contains(path)
            || inferred.project.contains(path)
            || (inferred.workspace_file && path == "pnpm-workspace.yaml")
            || (inferred.lockfile.is_some() && path == "pnpm-lock.yaml")
            || self.dependency_outputs.iter().any(|(matcher, outputs)| {
                matcher.is_match(path) && outputs.iter().any(|outputs| outputs.matches(path))
            })
    }

    /// Whether the declared key covers every path of `set`: each file read
    /// or found is keyed, or a directory found holds a keyed file, so that
    /// its going changes the key; a file created where one was looked for,
    /// or where one read is gone since, would be selected, and is neither
    /// gitignored nor a declared output;
    /// and every entry a listed directory could gain or lose that is not
    /// gitignored would be selected, the directory being no declared output.
    /// A declared output never enters the declared files, unless a
    /// `dependentTasksOutputFiles` input keys it.
    /// `output` says whether a path is a declared output of any target.
    pub(crate) fn covers(
        &self,
        snapshot: &Snapshot,
        root: &Path,
        set: &crate::recorded::PathSet,
        output: &dyn Fn(&str) -> bool,
    ) -> bool {
        self.uncovered(snapshot, root, set, output).next().is_none()
    }

    /// The paths of `set` the declared key does not cover, as
    /// [`Self::covers`] judges them, each with what the task did to it:
    /// `read`, `found`, `looked for` or `listed`.
    pub(crate) fn uncovered<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        root: &'a Path,
        set: &'a crate::recorded::PathSet,
        output: &'a dyn Fn(&str) -> bool,
    ) -> impl Iterator<Item = (&'a str, &'static str)> + 'a {
        let selection = &self.resolved.declared.selection;
        fn directory(path: &str) -> &str {
            if path == "." { "" } else { path }
        }
        let holds_keyed = move |path: &str| {
            let path = directory(path);
            if path.is_empty() {
                return self.resolved.whole_files().next().is_some();
            }
            under(&self.resolved.declared.files, path).next().is_some()
                || under(&self.resolved.inferred.files, path).next().is_some()
        };
        // Whether a file created at `path` would enter the declared key.
        let ignore = snapshot.source_ignore();
        let selected = move |path: &str| {
            !ignore.matches(path) && !output(path) && selection.selects(snapshot, path)
        };
        // A file the task read that is gone since is keyed as one it looked
        // for and did not find: its coming back changes the key. A path it
        // found may have been a directory, which the declared key leaves out
        // while empty, so its going is not judged so.
        let gone = move |path: &str| {
            root.join(path).symlink_metadata().is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                )
            })
        };
        let reads = set
            .reads
            .iter()
            .filter(move |path| !(self.keyed(path) || (gone(path) && selected(path))))
            .map(|path| (path.as_str(), "read"));
        let present = set
            .present
            .iter()
            .filter(move |path| {
                !(self.keyed(path) || (root.join(path).is_dir() && holds_keyed(path)))
            })
            .map(|path| (path.as_str(), "found"));
        let absent = set
            .absent
            .iter()
            .filter(move |path| !(self.keyed(path) || selected(path)))
            .map(|path| (path.as_str(), "looked for"));
        let listings = set
            .listings
            .iter()
            .filter(move |path| {
                let path = directory(path);
                !((path.is_empty() || !output(path))
                    && selection.selects_every_entry(snapshot, path))
            })
            .map(|path| (path.as_str(), "listed"));
        reads.chain(present).chain(absent).chain(listings)
    }
}

/// A `json` glob, kept to select dependency outputs once they are written.
struct JsonGlob {
    matcher: Arc<Pattern>,
    prefix: String,
    /// The project whose files it selects, or `None` for the workspace's.
    owner: Option<String>,
    fields: JsonFields,
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
    json_fields: BTreeMap<String, Vec<JsonFields>>,
    json_globs: Vec<JsonGlob>,
    named_stack: Vec<(String, String)>,
    /// Dependency named inputs already expanded for `^` inputs.
    expanded: BTreeSet<(String, String)>,
    external: BTreeSet<String>,
    /// `includeIgnored` globs, resolved, by the project that reads them and
    /// whether they came with `dependencies: true`: each is one group, whose
    /// negations filter all of its positives.
    ignored: BTreeMap<(String, bool), Vec<String>>,
    external_declared: bool,
    /// Every project whose inputs were read, as Nx hashes each one's
    /// configuration.
    projects: BTreeSet<String>,
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
            json_fields: BTreeMap::new(),
            json_globs: Vec::new(),
            named_stack: Vec::new(),
            expanded: BTreeSet::new(),
            external: BTreeSet::new(),
            ignored: BTreeMap::new(),
            external_declared: false,
            projects: BTreeSet::new(),
            prepared,
            cancelled,
        }
    }

    fn task_inputs(&mut self, task: &Task) -> Result<()> {
        self.projects.insert(task.project.clone());
        for input in declared_inputs(task) {
            self.input(&task.project, &input)?;
        }
        // As in Nx, a continuous task serving this one counts its own inputs
        // here.
        let snapshot = self.snapshot;
        for (project, inputs) in snapshot.servers.get(&task.id).into_iter().flatten() {
            for input in inputs {
                self.input(project, input)
                    .with_context(|| format!("inputs of {project}, which serves this task"))?;
            }
        }
        Ok(())
    }

    fn input(&mut self, project: &str, input: &Value) -> Result<()> {
        self.projects.insert(project.to_owned());
        // Nx 24's `always` keeps an input in a hash taken from an Ultracache
        // recording. qk hashes no recordings, so the input reads as without it.
        if let Value::Object(object) = input
            && let Some(always) = object.get("always")
        {
            // As in Nx, `always` keeps an input on purpose, so a project
            // pattern it selects nothing with is a mistake.
            if always.as_bool().context("input always must be boolean")?
                && let Some(Value::String(name)) = object.get("input")
                && let Some(projects) = object.get("projects")
            {
                let patterns: Vec<String> = match projects {
                    Value::String(pattern) => vec![pattern.clone()],
                    other => serde_json::from_value(other.clone())
                        .context("input projects must be a string or an array")?,
                };
                for pattern in patterns.iter().filter(|pattern| {
                    !pattern.starts_with('!')
                        && !matches!(pattern.as_str(), "self" | "dependencies")
                }) {
                    if qk_graph::select_projects(
                        &self.workspace.projects,
                        std::slice::from_ref(pattern),
                        &[],
                    )?
                    .is_empty()
                    {
                        bail!(
                            "the input {{\"input\": \"{name}\", \"projects\"}} with \"always\": true selects \"{pattern}\", which matches no project"
                        );
                    }
                }
            }
            let mut object = object.clone();
            object.remove("always");
            return self.input(project, &Value::Object(object));
        }
        match input {
            // As in Nx, `^{projectRoot}/…` and `^{workspaceRoot}/…` are a
            // fileset of every project the project depends on, directly or
            // not, and `^name` is `name` of each of them.
            Value::String(name)
                if name.starts_with("^{projectRoot}") || name.starts_with("^{workspaceRoot}") =>
            {
                self.dependency_fileset(project, &name[1..])?;
            }
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
                if let Some(listed) = listed {
                    let patterns: Vec<String> = match listed {
                        Value::String(pattern) => vec![pattern.clone()],
                        other => serde_json::from_value(other.clone())
                            .context("input projects must be a string or an array")?,
                    };
                    // Nx's native hasher reads these patterns, not the
                    // `findMatchingProjects` the rest of Nx uses.
                    for selected in qk_config::find_matching_projects(
                        &patterns,
                        &self.workspace.projects,
                        qk_config::Reader::Native,
                    ) {
                        // As in Nx 24, `dependencies: true` beside `projects`
                        // is each selected project's `^name`, not its own.
                        if dependencies {
                            self.dependencies_named(&selected, name)?;
                        } else if self.expanded.insert((selected.clone(), name.to_owned())) {
                            self.named(&selected, name)?;
                        }
                    }
                } else if dependencies {
                    self.dependencies_named(project, name)?;
                } else {
                    self.named(project, name)?;
                }
            }
            Value::Object(object) if object.contains_key("fileset") => {
                if let Some(key) = object.keys().find(|key| {
                    !matches!(key.as_str(), "fileset" | "dependencies" | "includeIgnored")
                }) {
                    bail!("unsupported fileset input field {key:?}");
                }
                let flag = |name: &str| {
                    object
                        .get(name)
                        .map(|value| {
                            value
                                .as_bool()
                                .with_context(|| format!("{name} must be boolean"))
                        })
                        .transpose()
                        .map(Option::unwrap_or_default)
                };
                let (dependencies, include_ignored) =
                    (flag("dependencies")?, flag("includeIgnored")?);
                let pattern = object["fileset"]
                    .as_str()
                    .context("fileset input must be a glob")?
                    .to_owned();
                validate_fileset(&pattern)?;
                match (dependencies, include_ignored) {
                    (false, false) => self.fileset(project, &pattern)?,
                    (true, false) => self.dependency_fileset(project, &pattern)?,
                    (false, true) => self.ignored_fileset(project, &pattern, false),
                    (true, true) => {
                        if !self.named_stack.is_empty() {
                            bail!(
                                "named inputs can only refer to named inputs of their own project"
                            );
                        }
                        for dependency in self.dependency_closure(project) {
                            self.ignored_fileset(&dependency, &pattern, true);
                        }
                    }
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
            // As in Nx, a string names a named input when there is one, and is
            // a fileset otherwise.
            Value::String(name) if self.has_named(project, name) => self.named(project, name)?,
            Value::String(pattern) => {
                validate_fileset(pattern)?;
                self.fileset(project, pattern)?
            }
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
                // Its output is hashed, so it must not depend on a terminal.
                prepared.pseudo_terminal = false;
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
                // As in Nx, a workspace project is no external dependency.
                if let Some(name) = names
                    .iter()
                    .find(|name| self.workspace.projects.contains_key(*name))
                {
                    if self.snapshot.projects.dependencies[project]
                        .iter()
                        .any(|edge| edge.target == *name)
                    {
                        bail!(
                            "the externalDependency {name:?} of {project} is not an external node and is already a dependency; remove it from the externalDependencies inputs"
                        );
                    }
                    bail!(
                        "the externalDependency {name:?} of {project} is not an external node; if it is a dependency, add it to the project's implicitDependencies"
                    );
                }
                self.external.extend(names);
                self.external_declared = true;
            }
            _ => bail!("unsupported task input declaration"),
        }
        Ok(())
    }

    fn fileset(&mut self, project: &str, pattern: &str) -> Result<()> {
        self.projects.insert(project.to_owned());
        let (exclude, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern));
        // Nx hashes workspace files and each project's files in separate
        // scopes, a project's being only the files it owns: those of a project
        // nested in its root are that project's. A fileset is the project's
        // when it starts from `{projectRoot}/`, and the workspace's otherwise,
        // where only one starting from `{workspaceRoot}/` selects anything.
        let (scope, owner) = if pattern.starts_with("{projectRoot}/") {
            (
                FileScope::Project(project.to_owned()),
                Some(project.to_owned()),
            )
        } else if pattern.starts_with("{workspaceRoot}/") {
            (FileScope::Workspace, None)
        } else {
            return Ok(());
        };
        let pattern = paths::expand_glob(self.workspace, project, pattern)?;
        let matcher = self.snapshot.fileset_pattern(&pattern, exclude)?;
        let (prefix, rest) = crate::ignored::partition(pattern.trim_end_matches('/'));
        let rest = rest.map(str::to_owned);
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
            rest,
            excluded: exclude,
            owner,
        });
        Ok(())
    }

    /// An `includeIgnored` glob of `project`, with its tokens resolved as
    /// Nx's `resolve_files_glob` resolves them, to read from disk later.
    fn ignored_fileset(&mut self, project: &str, pattern: &str, dependencies: bool) {
        // The root and name go in escaped, as for every fileset; Nx inserts
        // them as they are.
        let root = &self.workspace.projects[project].root;
        let resolved = if root == "." {
            pattern.replace("{projectRoot}/", "")
        } else {
            pattern.replace("{projectRoot}", &crate::glob::escape(root))
        }
        .replace("{projectName}", &crate::glob::escape(project));
        let resolved = match resolved.strip_prefix("!{workspaceRoot}/") {
            Some(rest) => format!("!{rest}"),
            None => resolved
                .strip_prefix("{workspaceRoot}/")
                .map(str::to_owned)
                .unwrap_or(resolved),
        };
        self.ignored
            .entry((project.to_owned(), dependencies))
            .or_default()
            .push(resolved);
    }

    /// A fileset of every project `project` depends on, directly or not.
    fn dependency_fileset(&mut self, project: &str, pattern: &str) -> Result<()> {
        if !self.named_stack.is_empty() {
            bail!("named inputs can only refer to named inputs of their own project");
        }
        for dependency in self.dependency_closure(project) {
            self.fileset(&dependency, pattern)?;
        }
        Ok(())
    }

    /// Whether `name` names one of `project`'s named inputs.
    fn has_named(&self, project: &str, name: &str) -> bool {
        name == "default"
            || self.workspace.projects[project]
                .named_inputs
                .contains_key(name)
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

    /// A JSON input: the fields of a JSON file, rather than all of it, or of
    /// each file a glob selects. `fields` keeps only the named dotted paths;
    /// `excludeFields` removes them.
    fn json(&mut self, project: &str, object: &serde_json::Map<String, Value>) -> Result<()> {
        for key in object.keys() {
            if !matches!(key.as_str(), "json" | "fields" | "excludeFields") {
                bail!("unsupported json input field {key:?}");
            }
        }
        let declared = object["json"]
            .as_str()
            .context("json input must name a file")?;
        validate_fileset(declared)?;
        let path = paths::expand_glob(self.workspace, project, declared)?;
        let list = |name: &str| -> Result<Option<Vec<String>>> {
            object
                .get(name)
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .with_context(|| format!("json input {name} must be an array of strings"))
                })
                .transpose()
        };
        let fields = JsonFields {
            fields: list("fields")?,
            exclude: list("excludeFields")?.unwrap_or_default(),
        };
        if crate::glob::names_a_path(&path) {
            let path = crate::glob::literal_path(&path);
            paths::validate_path(&path)?;
            return self.json_file(path, &fields);
        }
        // As in Nx, a glob selects the files a fileset from the same root
        // would: the project's own, or the workspace's.
        let owner = if declared.starts_with("{projectRoot}") {
            Some(project)
        } else if declared.starts_with("{workspaceRoot}/") {
            None
        } else {
            return Ok(());
        };
        let matcher = self.snapshot.pattern(&path, false)?;
        let snapshot = self.snapshot;
        let matched: Vec<String> = self
            .candidates
            .iter()
            .filter(|file| matcher.is_match(file))
            .filter(|file| owner.is_none() || snapshot.owner(file) == owner)
            .cloned()
            .collect();
        for file in matched {
            self.json_file(file, &fields)?;
        }
        self.json_globs.push(JsonGlob {
            matcher,
            prefix: crate::ignored::partition(&path).0,
            owner: owner.map(str::to_owned),
            fields,
        });
        Ok(())
    }

    /// One file a `json` input selects, keyed by its fields. Several inputs
    /// may select different fields of one file: each counts, as in Nx since
    /// 24.0.0-rc.0, where one used to replace the other.
    fn json_file(&mut self, path: String, fields: &JsonFields) -> Result<()> {
        let text = match std::fs::read_to_string(self.workspace.root.join(&path)) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut selections = self.json_fields.get(&path).cloned().unwrap_or_default();
        if !selections.contains(fields) {
            selections.push(fields.clone());
            // In one order, whichever input came first.
            selections.sort();
        }
        let mut values = Vec::new();
        for selection in &selections {
            values.push(match &text {
                Some(text) => match json_projection(&path, text, selection) {
                    Some(value) => value,
                    // As in Nx, a file it cannot read is keyed by its content,
                    // without the field filters. Nx reads no YAML, so it keys
                    // a YAML file whole where qk keys its fields.
                    None => {
                        self.selected.insert(path);
                        return Ok(());
                    }
                },
                None => Value::Null,
            });
        }
        // One selection keys its fields as before; several, each in turn.
        let value = if values.len() == 1 {
            values.remove(0)
        } else {
            Value::Array(values)
        };
        self.values.insert(format!("json:{path}"), value);
        self.json_fields.insert(path, selections);
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

/// As Nx's `validate_file_set`: a fileset starts from `{projectRoot}` or
/// `{workspaceRoot}`, after its `!`.
fn validate_fileset(fileset: &str) -> Result<()> {
    let body = fileset.strip_prefix('!').unwrap_or(fileset);
    if !body.starts_with("{projectRoot}") && !body.starts_with("{workspaceRoot}") {
        bail!(
            "{fileset:?} is an invalid fileset: filesets have to start with either {{workspaceRoot}} or {{projectRoot}}, and a named input must be defined"
        );
    }
    Ok(())
}

/// A task's declared inputs, or Nx's default when it declares none.
fn declared_inputs(task: &Task) -> Vec<Value> {
    task.definition
        .inputs
        .clone()
        .unwrap_or_else(|| vec![json!("default"), json!("^default")])
}

/// The continuous tasks serving a task, whose inputs count as its own, as Nx
/// hashes them: its continuous dependencies, theirs in turn, and so on, each
/// by its project and declared inputs. A dependency declared with
/// `inputs: false` is not followed, though a server it leads to still counts
/// when another way reaches it.
fn servers(graph: &TaskGraph, id: &str) -> Vec<(String, Vec<Value>)> {
    let mut servers = Vec::new();
    let mut visited = BTreeSet::from([id]);
    let mut pending = std::collections::VecDeque::from([id]);
    while let Some(current) = pending.pop_front() {
        let task = &graph.tasks[current];
        for dependency in &task.dependencies {
            let Some(server) = graph.tasks.get(dependency) else {
                continue;
            };
            if server.definition.continuous != Some(true)
                || !task.edge(dependency).inputs
                || !visited.insert(dependency)
            {
                continue;
            }
            servers.push((server.project.clone(), declared_inputs(server)));
            pending.push_back(dependency);
        }
    }
    servers
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
        snapshot.files()?,
        snapshot.source_ignore(),
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
    // Dependency outputs may also be declared sources, for filesets and `json`
    // globs alike. Downstream and unrelated
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
    let mut producers = BTreeSet::new();
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
            .map(|pattern| &pattern.prefix)
            .chain(resolver.json_globs.iter().map(|glob| &glob.prefix))
            .any(|prefix| {
                declared.anchors().any(|anchor| {
                    overlaps(prefix, anchor)
                        && (!source_ignore.matches_directory(anchor)
                            || !workspace.root.join(anchor).is_dir())
                })
            });
        if !relevant {
            continue;
        }
        producers.insert(producer.id.clone());
        let outputs = snapshot.dependency_outputs(workspace, producer)?;
        let mut json = Vec::new();
        for path in &outputs.files {
            if source_ignore.matches(path)
                || own_outputs.as_ref().is_some_and(|own| own.matches(path))
            {
                continue;
            }
            json.extend(
                resolver
                    .json_globs
                    .iter()
                    .filter(|glob| {
                        glob.matcher.is_match(path)
                            && (glob.owner.is_none()
                                || snapshot.owner(path) == glob.owner.as_deref())
                    })
                    .map(|glob| (path.clone(), glob.fields.clone())),
            );
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
        for (path, fields) in json {
            resolver.json_file(path, &fields)?;
        }
    }
    // A changed file affected selection adds counts whatever produces it, as
    // in Nx, where a declared output is a file like any other.
    for selection in resolver.scopes.values() {
        resolver.selected.extend(
            selection
                .included
                .difference(&selection.excluded)
                .filter(|path| {
                    snapshot.extra_candidates.contains(*path)
                        || own_outputs.as_ref().is_none_or(|own| !own.matches(path))
                })
                .cloned(),
        );
    }
    // `includeIgnored` groups read the disk, gitignored files included, and
    // a changed path they name counts whether or not it is there. As in Nx,
    // one reading another task's declared outputs makes that task a producer.
    let mut ignored = BTreeSet::new();
    for ((project, _), globs) in &resolver.ignored {
        let group = Group::new(
            project,
            globs,
            workspace
                .projects
                .values()
                .map(|project| project.root.as_str()),
        )?;
        let found = group.expand(&workspace.root)?;
        ignored.extend(
            found
                .into_iter()
                .chain(
                    snapshot
                        .extra_candidates
                        .iter()
                        .filter(|path| group.matches(path))
                        .cloned(),
                )
                // A task's own outputs are no input of it, as for any fileset,
                // and no task's scratch is any task's.
                .filter(|path| own_outputs.as_ref().is_none_or(|own| !own.matches(path)))
                .filter(|path| !snapshot.scratch.iter().any(|scratch| scratch.matches(path))),
        );
        for (producer, declared) in &snapshot.generated_tasks {
            if producer.id != task.id
                && upstream.contains(&producer.id)
                && declared.declared()
                && reads_outputs(&group, declared)
            {
                producers.insert(producer.id.clone());
            }
        }
    }
    resolver.selected.extend(ignored.iter().cloned());
    let mut declared = Declared {
        files: resolver.selected,
        ignored,
        values: resolver.values,
        json_fields: resolver.json_fields,
        external: resolver.external,
        external_declared: resolver.external_declared,
        projects: resolver.projects,
        producers,
        selection: Selection {
            scopes: resolver
                .scopes
                .into_values()
                .map(|selection| selection.patterns)
                .collect(),
        },
    };
    // `dependentTasksOutputFiles` reads its direct dependencies' declared
    // outputs, or with `transitive` those of every task it depends on.
    let reads: Vec<bool> = declared
        .values
        .iter()
        .filter(|(name, _)| name.starts_with("dependentTasksOutputFiles:"))
        .map(|(_, transitive)| transitive == &Value::Bool(true))
        .collect();
    if !reads.is_empty() {
        let transitive = reads.contains(&true);
        for (producer, outputs) in &snapshot.generated_tasks {
            let read = if transitive {
                upstream.contains(&producer.id)
            } else {
                task.dependencies.contains(&producer.id)
            };
            if read && outputs.declared() {
                declared.producers.insert(producer.id.clone());
            }
        }
    }
    let mut inferred = infer(snapshot, workspace, task)?;
    // As in Nx, a task naming its external dependencies hashes those in place
    // of every installed package, so the root importer's installs count only
    // for the root project's own tasks.
    if declared.external_declared
        && let Some(importers) = &mut inferred.lockfile
    {
        let root = workspace.projects[&task.project].root == ".";
        if !root {
            importers.remove(".");
        }
    }
    Ok(join(declared, inferred))
}

/// Whether an `includeIgnored` group may read files `outputs` declares, as
/// Nx's `GlobShape::may_read` judges it from the globs alone: a positive's
/// directory and an output's meet, the one holding the other, unless the
/// output lies in a project the positive does not reach, and their
/// extensions, where both have one, agree.
fn reads_outputs(group: &Group, outputs: &Outputs) -> bool {
    let holds = |outer: &str, inner: &str| {
        outer.is_empty()
            || inner == outer
            || inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    group.reads().any(|(root, extension)| {
        outputs.globs().iter().any(|output| {
            let (prefix, _) = crate::ignored::partition(output);
            (holds(root, &prefix) || holds(&prefix, root))
                && !group
                    .skipped_below(root)
                    .iter()
                    .any(|project| holds(project, &prefix))
                && match (extension, crate::ignored::extension(output)) {
                    (Some(read), Some(written)) => read == written,
                    _ => true,
                }
        })
    })
}

/// What qk adds to `task`'s key on its own, whatever its inputs declare.
fn infer(snapshot: &Snapshot, workspace: &Workspace, task: &Task) -> Result<Inferred> {
    let readable = snapshot
        .installed(&workspace.root, &task.id)?
        .is_some_and(|installed| installed.lockfile.is_some());
    let exists = |path: &str| {
        workspace.root.join(path).is_file() || snapshot.extra_candidates.contains(path)
    };
    let mut inferred = Inferred {
        lockfile_readable: readable,
        ..Inferred::default()
    };
    // Workspace resolution and configuration. Dotenv files are not keyed, as
    // in Nx: they hold per-machine values and credentials, and a task's `env`
    // inputs key the variables it declares. The root tsconfig, as Nx hashes it
    // into every task. The root package.json is not, as in Nx: what the root
    // importer installs counts for every task through the lockfile, and the
    // rest of it is the root project's manifest.
    inferred.tsconfig = crate::inputs::ROOT_TSCONFIG_FILES
        .into_iter()
        .find(|name| exists(name))
        .map(str::to_owned);
    // nx.json counts through what it changes: through the task's resolved
    // configuration, and by the settings that reach tasks otherwise. A tool
    // reading it itself declares it as an input, which keys the file whole.
    for path in ["nx.json", qk_config::LOCAL_WORKSPACE] {
        if exists(path) {
            inferred.settings.insert(path.into());
        }
    }
    if let Some(extended) = &workspace.extended {
        inferred.settings.insert(extended.clone());
    }
    for path in [
        ".gitignore",
        ".nxignore",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
    ] {
        if exists(path) {
            inferred.files.insert(path.into());
        }
    }
    // A pnpm lockfile qk can read is keyed by what the task's projects
    // install, and the workspace file without the resolution keys whose effect
    // lands in it.
    if exists("pnpm-workspace.yaml") {
        if readable {
            inferred.workspace_file = true;
        } else {
            inferred.files.insert("pnpm-workspace.yaml".into());
        }
    }
    if exists("pnpm-lock.yaml") && !readable {
        inferred.files.insert("pnpm-lock.yaml".into());
    }
    // The task's own package.json counts whole, its package scripts and
    // dependency declarations with it. A dependency's package.json counts by what it
    // decides for dependents, such as where their imports of it resolve; its
    // installs count through the lockfile.
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
            // Its project.json and project.local.json are not added: what
            // they decide for the task counts through its resolved
            // configuration, and an input selecting them keys them as it
            // says.
            if own && name != "package.json" {
                inferred.project.insert(path);
                continue;
            }
            if own {
                inferred.files.insert(path);
                continue;
            }
            let text = std::fs::read_to_string(workspace.root.join(&path))
                .with_context(|| format!("cannot read {path}"))?;
            match manifest_for_dependents(&text, readable) {
                Some(value) => {
                    inferred.manifests.insert(path, value);
                }
                None => {
                    inferred.files.insert(path);
                }
            }
        }
    }
    if readable {
        // The root importer's packages resolve from every package in the
        // workspace, so they count for every task.
        inferred.lockfile = Some(
            std::iter::once(".".to_owned())
                .chain(
                    packages
                        .iter()
                        .map(|project| workspace.projects[project].root.clone()),
                )
                .collect(),
        );
    }
    Ok(inferred)
}

/// `declared` and `inferred` together. A file both select is left to the
/// declared inputs, which count all of it.
fn join(declared: Declared, mut inferred: Inferred) -> Resolved {
    let mut covered = BTreeSet::new();
    inferred.files.retain(|path| {
        let keep = !declared.files.contains(path);
        if !keep {
            covered.insert(path.clone());
        }
        keep
    });
    inferred.manifests.retain(|path, _| {
        let keep = !declared.files.contains(path);
        if !keep {
            covered.insert(path.clone());
        }
        keep
    });
    if inferred.lockfile.is_some() && declared.files.contains("pnpm-lock.yaml") {
        inferred.lockfile = None;
        covered.insert("pnpm-lock.yaml".into());
    }
    if inferred.workspace_file && declared.files.contains("pnpm-workspace.yaml") {
        inferred.workspace_file = false;
        covered.insert("pnpm-workspace.yaml".into());
    }
    if let Some(tsconfig) = inferred
        .tsconfig
        .take_if(|path| declared.files.contains(path))
    {
        covered.insert(tsconfig);
    }
    inferred.covered = covered;
    Resolved { declared, inferred }
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
    let candidates = snapshot.task_candidates(workspace, task)?;
    inputs_from(
        snapshot,
        workspace,
        task,
        prepared,
        dependencies,
        cancelled,
        &candidates,
    )
}

/// [`inputs`], selecting files from `candidates`.
fn inputs_from(
    snapshot: &Snapshot,
    workspace: &Workspace,
    task: &Task,
    prepared: &PreparedTask,
    dependencies: &BTreeMap<String, String>,
    cancelled: &AtomicBool,
    candidates: &Candidates,
) -> Result<Value> {
    let Resolved { declared, inferred } = resolve_candidates(
        snapshot,
        workspace,
        task,
        Some(prepared),
        cancelled,
        &candidates.files,
        &candidates.source_ignore,
    )?;
    let file_values = |paths: &BTreeSet<String>| -> Result<BTreeMap<String, Value>> {
        let paths: Vec<&String> = paths.iter().collect();
        Ok(in_parallel(&paths, FILES_PER_THREAD, |path| {
            let value = snapshot
                .file_value(&workspace.root, path)
                .with_context(|| format!("cannot read input {path}"))?;
            Ok(((*path).clone(), value))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>())
    };
    let mut files = file_values(&(&declared.files - &declared.ignored))?;
    for path in &declared.ignored {
        if let Some(value) = snapshot
            .ignored_value(&workspace.root, path)
            .with_context(|| format!("cannot read input {path}"))?
        {
            files.insert(path.clone(), value);
        }
    }
    let mut values = declared.values;
    // Left out without a declared environment, so other keys stay as they
    // were.
    if let Some(environment) = &prepared.environment {
        values.insert("environment".into(), environment.keyed());
    }
    let mut configuration = json!({
        "files": file_values(&inferred.files)?,
        "settings": workspace.config.keyed_settings(),
        "project": task_configuration(workspace, task),
    });
    let installed = snapshot.installed(&workspace.root, &task.id)?;
    if let (Some(importers), Some(installed)) = (&inferred.lockfile, &installed)
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
        configuration["lockfile"] = json!({
            "global": installed.digest("global", || BTreeSet::from([lockfile.global().to_string()])),
            "importers": importers,
        });
        // A lockfile keyed whole already covers the packages it installs.
        if !declared.external.is_empty() {
            let external: BTreeMap<_, _> = declared
                .external
                .iter()
                .map(|name| {
                    let digest =
                        installed.digest(&format!("package:{name}"), || lockfile.package(name));
                    // As in Nx, a package nothing installs is an error.
                    if digest == blake3::hash(b"").to_hex().as_str() {
                        bail!(
                            "the externalDependency {name:?} of {} could not be found",
                            task.id
                        );
                    }
                    Ok((name.clone(), digest))
                })
                .collect::<Result<_>>()?;
            values.insert("externalDependencies".into(), json!(external));
        }
    }
    if inferred.workspace_file {
        configuration["workspaceFile"] = match workspace_without_resolution(&workspace.root) {
            Some(value) => value,
            None => snapshot.file_value(&workspace.root, "pnpm-workspace.yaml")?,
        };
    }
    if !inferred.manifests.is_empty() {
        configuration["manifests"] = json!(inferred.manifests);
    }
    if let Some(path) = &inferred.tsconfig {
        let parsed = std::fs::read_to_string(workspace.root.join(path))
            .ok()
            .and_then(|text| TsConfig::parse(&text));
        configuration["tsconfig"] = match parsed {
            Some(tsconfig) if crate::inputs::selectively_hash_tsconfig(workspace) => json!({
                "rest": tsconfig.rest,
                "paths": tsconfig.paths_of(&declared.projects, |target| snapshot.owner(target)),
            }),
            Some(tsconfig) => json!({ "rest": tsconfig.rest }),
            None => snapshot.file_value(&workspace.root, path)?,
        };
    }
    let definition = keyed_definition(task);
    let hash = json!({
        "schema":"qk-local-v4", "qk":env!("CARGO_PKG_VERSION"),
        "platform":[std::env::consts::OS, std::env::consts::ARCH], "workspace":snapshot.workspace_prefix,
        "id":task.id, "args":task.args, "definition":definition, "packageManager":workspace.package_manager,
        "files":files, "values":values, "inferred":configuration, "dependencies":dependencies,
    });
    Ok(hash)
}

/// Files a thread reads the values of at least, so a short list is not
/// split.
const FILES_PER_THREAD: usize = 256;

/// `f` of each of `items`, in their order, on up to eight threads, each
/// taking `per_thread` of them at least. Reading files is mostly system
/// calls, which wait apart from each other. The error is the first in
/// `items`' order, as one thread taking them in turn would return.
pub(crate) fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    per_thread: usize,
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8)
        .min(items.len() / per_thread);
    if threads <= 1 {
        return items.iter().map(&f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    let f = &f;
    let chunks: Vec<Result<Vec<R>>> = std::thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(chunk)
            .map(|items| scope.spawn(move || items.iter().map(f).collect()))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("input thread panicked"))
            .collect()
    });
    let mut results = Vec::with_capacity(items.len());
    for chunk in chunks {
        results.extend(chunk?);
    }
    Ok(results)
}

/// A task's definition as its key holds it. Warm state configuration never
/// changes a result, and as in Nx a target's metadata, such as the run
/// command shown for a script, is description only.
pub fn keyed_definition(task: &Task) -> qk_config::Target {
    let mut definition = task.definition.clone();
    definition.extra.remove("qk:warm");
    definition.extra.remove("qk:threads");
    definition.extra.remove("metadata");
    // What a recording keys is the recording's own concern.
    definition.extra.remove("qk:ultracache");
    definition
}

/// What a task's key holds of its project's configuration beside its
/// definition and resolved inputs: the project's keyed settings, such as its
/// root, the paths its outputs resolve to, which `{project.*}` tokens can
/// read from anywhere in the configuration, and all of the configuration
/// when a forwarded argument holds such a token, as the task reads it then.
pub fn task_configuration(workspace: &Workspace, task: &Task) -> Value {
    let project = &workspace.projects[&task.project];
    let mut value = json!({ "settings": project.keyed_settings() });
    if let Ok((outputs, _)) = paths::resolved_outputs(workspace, task) {
        value["outputs"] = json!(outputs);
    }
    if task.args.iter().any(|arg| arg.contains("{project")) {
        value["configuration"] = serde_json::to_value(project).unwrap_or_default();
    }
    value
}

/// The key for a task's inputs. Objects are keyed with their fields sorted,
/// so the order they were written in, which configuration keeps, does not
/// change the key.
pub fn key(inputs: &Value) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&sorted(inputs))?)
        .to_hex()
        .to_string())
}

/// `value` with every object's fields in key order.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut fields: Vec<_> = object.iter().collect();
            fields.sort_unstable_by_key(|(name, _)| *name);
            Value::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (name.clone(), sorted(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
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

/// The key of a task keyed by its recordings, before its path set: `inputs`,
/// the task's key, without its declared file inputs, which a path set
/// replaces, but with its `"always": true` inputs, which count whatever it
/// read, and with what `ignoreReads` leaves out of its path sets and
/// `produced`, what decides which paths in `node_modules` they keep.
#[allow(clippy::too_many_arguments)]
pub(crate) fn weak_key(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    prepared: &PreparedTask,
    inputs: &Value,
    ignore_reads: &[String],
    ignored_within: &[(&str, bool)],
    produced: Option<Value>,
    cancelled: &AtomicBool,
) -> Result<String> {
    let mut weak = inputs.clone();
    let object = weak
        .as_object_mut()
        .context("a key's inputs are an object")?;
    object.remove("files");
    object.remove("dependencies");
    if let Some(Value::Object(values)) = object.get_mut("values") {
        values.retain(|name, _| {
            !name.starts_with("json:") && !name.starts_with("dependentTasksOutputFiles:")
        });
    }
    let mut always = task.clone();
    always.definition.inputs = Some(
        declared_inputs(task)
            .into_iter()
            .filter(|input| input.get("always") == Some(&Value::Bool(true)))
            .collect(),
    );
    let dependencies = dependency_keys(snapshot, workspace, graph, &always)?;
    let always = self::inputs(
        snapshot,
        workspace,
        &always,
        prepared,
        &dependencies,
        cancelled,
    )?;
    object.insert(
        "always".into(),
        json!({"files": always["files"], "values": always["values"], "dependencies": always["dependencies"]}),
    );
    // v2: path sets keep what upstream tasks write into `node_modules`. One
    // recorded before left it out, and could key a task whose generated
    // input had since changed.
    object.insert("schema".into(), json!("qk-weak-v2"));
    object.insert("ignoreReads".into(), json!(ignore_reads));
    // What its `^` entries match moves with the project graph, the projects
    // depended on and those nested in them that are not, so a path set
    // recorded under one never keys a task under another. Left out without
    // them, so other weak keys stay as they were.
    if !ignored_within.is_empty() {
        object.insert("ignoredWithin".into(), json!(ignored_within));
    }
    // Which paths in `node_modules` its path sets keep moves with what the
    // tasks it depends on declare as outputs, and its `includeIgnored`
    // inputs: a path set recorded under others could leave out a file
    // generated there, and so key nothing when it changes.
    if let Some(produced) = produced {
        object.insert("produced".into(), produced);
    }
    key(&weak)
}

/// The task `id` of `graph`, run or left out by `exclude_task_dependencies`.
fn dependency_task<'a>(graph: &'a TaskGraph, id: &str) -> Result<&'a qk_taskgraph::Task> {
    graph
        .keyed_task(id)
        .with_context(|| format!("task {id} is not in the task graph"))
}

/// The dependency outputs a task's `dependentTasksOutputFiles` inputs read,
/// as Nx hashes them: the files each glob selects of the declared outputs of
/// its direct dependencies, or with `transitive` of every task it depends on.
/// Nothing else about a dependency keys a task.
pub(crate) fn dependency_keys(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
) -> Result<BTreeMap<String, String>> {
    dependency_keys_impl(snapshot, workspace, graph, task, false)
}

/// Read dependency artifacts again before accepting an execution as unchanged.
pub(crate) fn recheck_dependency_keys(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
) -> Result<BTreeMap<String, String>> {
    dependency_keys_impl(snapshot, workspace, graph, task, true)
}

fn dependency_keys_impl(
    snapshot: &Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    fresh: bool,
) -> Result<BTreeMap<String, String>> {
    // The globs alone, which need no listing.
    let plan = snapshot.file_plan(workspace, task)?;
    // Each glob with the dependencies it reads, which are listed together
    // beforehand, each on a thread of its own. What fails is reported as
    // reading them in turn would report it.
    let mut reads = Vec::new();
    for (pattern, transitive) in &plan.dependency_outputs {
        reads.push((pattern, graph.output_dependencies(task, *transitive)));
    }
    let ids: BTreeSet<&String> = reads.iter().flat_map(|(_, ids)| ids).collect();
    let ids: Vec<&String> = ids.into_iter().collect();
    let listed = in_parallel(&ids, 1, |id| {
        let listed = (|| {
            let dependency = dependency_task(graph, id)?;
            // As in Nx, a dependency without declared outputs offers none.
            if !Outputs::new(workspace, dependency)?.declared() {
                return Ok(None);
            }
            Ok(Some(if fresh {
                Arc::new(snapshot.fresh_dependency_outputs(workspace, dependency)?)
            } else {
                snapshot.dependency_outputs(workspace, dependency)?
            }))
        })();
        Ok(listed.transpose().map(|listed| (*id, listed)))
    })?;
    let listed: HashMap<&String, Result<Arc<DependencyOutputs>>> =
        listed.into_iter().flatten().collect();
    let mut keys = BTreeMap::new();
    for (pattern, selected) in &reads {
        let matcher = snapshot.pattern(pattern, false)?;
        for id in selected {
            let Some(outputs) = listed.get(id) else {
                continue;
            };
            let outputs = match outputs {
                Ok(outputs) => outputs,
                Err(error) => bail!("{error:#}"),
            };
            let matched: Vec<&String> = outputs
                .files
                .iter()
                .filter(|path| matcher.is_match(path))
                .collect();
            let files: BTreeMap<_, _> = in_parallel(&matched, FILES_PER_THREAD, |path| {
                let known = outputs.values.lock().unwrap().get(*path).cloned();
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
                                .insert((*path).clone(), value.clone());
                        }
                        value
                    }
                };
                Ok(((*path).clone(), value))
            })?
            .into_iter()
            .collect();
            keys.insert(format!("{id}:{pattern}"), key(&json!(files))?);
        }
    }
    Ok(keys)
}

/// What one task's `dependentTasksOutputFiles` glob selects of the outputs it
/// reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyGlob {
    pub task: String,
    pub pattern: String,
    pub selects: Selects,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selects {
    /// None of the tasks it reads declares outputs, so nothing ever.
    NoDeclaredOutputs,
    /// Some of their outputs are not built, so it cannot be told.
    NotBuilt,
    /// None of the files their outputs hold now, this many.
    NoneOf(usize),
    /// Some of the files their outputs hold now.
    Some,
}

/// What each `dependentTasksOutputFiles` glob of `graph`'s tasks selects of
/// its dependencies' outputs as they are on disk, without running anything.
pub fn dependency_globs(workspace: &Workspace, graph: &TaskGraph) -> Result<Vec<DependencyGlob>> {
    let snapshot = Snapshot::new(workspace, graph, &paths::cache_location(workspace))?;
    // Every resolution chooses from the listing.
    snapshot.files()?;
    let cancelled = AtomicBool::new(false);
    let mut globs = Vec::new();
    for task in graph.tasks.values() {
        // A task whose inputs cannot be resolved fails as it runs instead.
        let Ok(resolved) = resolve(&snapshot, workspace, task, None, &cancelled) else {
            continue;
        };
        for (pattern, transitive) in resolved.dependency_output_inputs() {
            let selected = graph.output_dependencies(task, transitive);
            let matcher = snapshot.pattern(pattern, false)?;
            // A dependency not built yet may still produce what it matches.
            let (mut declared, mut files, mut matched, mut unbuilt) = (false, 0, false, false);
            for id in &selected {
                let dependency = dependency_task(graph, id)?;
                if !Outputs::new(workspace, dependency)?.declared() {
                    continue;
                }
                declared = true;
                let outputs = snapshot.dependency_outputs(workspace, dependency)?;
                unbuilt |= outputs.files.is_empty();
                files += outputs.files.len();
                matched |= outputs.files.iter().any(|path| matcher.is_match(path));
            }
            globs.push(DependencyGlob {
                task: task.id.clone(),
                pattern: pattern.to_owned(),
                selects: match (declared, matched, unbuilt) {
                    (false, ..) => Selects::NoDeclaredOutputs,
                    (_, true, _) => Selects::Some,
                    (_, _, true) => Selects::NotBuilt,
                    _ => Selects::NoneOf(files),
                },
            });
        }
    }
    Ok(globs)
}

#[cfg(test)]
mod tests {

    /// Every file keyed by its whole content, declared or inferred.
    fn whole(resolved: &Resolved) -> BTreeSet<String> {
        resolved.whole_files().map(str::to_owned).collect()
    }
    use super::*;
    use qk_taskgraph::Request;

    /// A snapshot that lists files on its own, or with the directory
    /// inventory a refresh compares against: refreshes behave alike.
    fn snapshot(workspace: &Workspace, graph: &TaskGraph, watched: bool) -> Snapshot {
        let cache = workspace.root.join("cache");
        if watched {
            Snapshot::watched(workspace, graph, &cache).unwrap()
        } else {
            Snapshot::new(workspace, graph, &cache).unwrap()
        }
    }

    fn fixture(outputs: Option<Value>) -> (tempfile::TempDir, Workspace, TaskGraph) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        let mut post = json!({"command": "true"});
        if let Some(outputs) = outputs {
            post["outputs"] = outputs;
        }
        std::fs::write(root.path().join("project.json"), json!({"name":"app", "targets": {
            "gen": {"command":"true", "outputs":["{projectRoot}/generated"]},
            "post": post,
            "build": {"command":"true", "dependsOn":["gen", "post"], "inputs":[{"dependentTasksOutputFiles":"**/*"}]}
        }}).to_string()).unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(root.path().join("generated/value"), "one").unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        (root, workspace, graph)
    }

    /// A `json` glob selects a dependency's declared outputs, keyed by their
    /// fields, and makes that dependency a producer, as a fileset does.

    #[test]
    fn keys_ignore_the_order_object_fields_were_written_in() {
        let written = json!({"options": {"cwd": "app", "command": "x"}, "args": ["a", "b"]});
        let reordered = json!({"args": ["a", "b"], "options": {"command": "x", "cwd": "app"}});
        assert_eq!(key(&written).unwrap(), key(&reordered).unwrap());
        let swapped = json!({"options": {"cwd": "app", "command": "x"}, "args": ["b", "a"]});
        assert_ne!(key(&written).unwrap(), key(&swapped).unwrap());
    }
    #[test]
    fn json_globs_select_dependency_outputs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        for (path, content) in [
            (
                "apps/app/project.json",
                json!({"name": "app", "implicitDependencies": ["lib"], "targets": {
                    "build": {"command": "true", "dependsOn": ["^build"], "inputs": [
                        {"json": "{workspaceRoot}/libs/lib/out/*.json", "fields": ["version"]}
                    ]}
                }}),
            ),
            (
                "libs/lib/project.json",
                json!({"name": "lib", "targets": {
                    "build": {"command": "true", "outputs": ["{projectRoot}/out"]}
                }}),
            ),
            (
                "libs/lib/out/meta.json",
                json!({"version": 2, "built": "now"}),
            ),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content.to_string()).unwrap();
        }
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let resolved = resolve(
            &snapshot,
            &workspace,
            &graph.tasks["app:build"],
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            resolved.declared.values.get("json:libs/lib/out/meta.json"),
            Some(&json!({"version": 2}))
        );
        assert_eq!(
            resolved.declared.producers,
            BTreeSet::from(["lib:build".to_owned()])
        );
    }

    /// The declared key covers a recording when every file it read is keyed,
    /// every file it looked for would be selected and not gitignored, and
    /// every entry a directory it listed could hold would be selected.
    #[test]
    fn declared_inputs_cover_what_they_would_select_and_not_what_they_leave_out() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "app", "targets": {
                "build": {"command": "true", "inputs": [
                    "{projectRoot}/src/**/*", "!{projectRoot}/src/skip/**/*"
                ]},
                "gen": {"command": "true", "outputs": ["{projectRoot}/src/gen"]}
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join(".gitignore"), "src/secret.ts\n").unwrap();
        for path in [
            "src/a.ts",
            "src/lib/b.ts",
            "src/skip/c.ts",
            "src/gen/d.ts",
            "README.md",
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let task = &graph.tasks["app:build"];
        let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let coverage = coverage(
            &snapshot,
            &workspace,
            &graph,
            task,
            &prepared,
            &AtomicBool::new(false),
        )
        .unwrap();
        let set = |part: &str, path: &str| {
            let mut set = crate::recorded::PathSet::default();
            match part {
                "read" => &mut set.reads,
                "present" => &mut set.present,
                "absent" => &mut set.absent,
                _ => &mut set.listings,
            }
            .insert(path.to_owned());
            set
        };
        for (part, path, covered) in [
            ("read", "src/a.ts", true),
            ("read", "README.md", false),
            ("read", "src/skip/c.ts", false),
            ("present", "src/lib", true),
            ("absent", "src/new.ts", true),
            // Excluded, or gitignored: creating it would not change the key.
            ("absent", "src/skip/new.ts", false),
            ("absent", "src/secret.ts", false),
            ("absent", ".env.local", false),
            // Another target's output, which no declared key holds.
            ("absent", "src/gen/e.ts", false),
            ("listing", "src/gen", false),
            ("listing", "src/lib", true),
            // An exclusion reaches into it, or nothing selects all of it.
            ("listing", "src", false),
            ("listing", ".", false),
        ] {
            let generated = |path: &str| path == "src/gen" || path.starts_with("src/gen/");
            assert_eq!(
                coverage.covers(&snapshot, root.path(), &set(part, path), &generated),
                covered,
                "{part} {path}"
            );
        }
    }

    #[test]
    fn execution_fingerprints_refresh_sources_and_preserve_artifact_exclusions() {
        let (root, mut workspace, _) = fixture(Some(json!(["{projectRoot}/dist"])));
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
        for (git, watched) in [(false, false), (true, false), (false, true), (true, true)] {
            let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
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
            let snapshot = snapshot(&workspace, &graph, watched);
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
    fn a_watched_snapshot_lists_what_a_refresh_lists_and_its_first_refresh_reuses_it() {
        for (git, policy) in [
            (false, ".gitignore"),
            (true, ".gitignore"),
            (true, ".nxignore"),
        ] {
            let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
            for directory in ["src/nested", "src/empty", "dist", "ignored"] {
                std::fs::create_dir_all(root.path().join(directory)).unwrap();
            }
            for file in ["src/nested/kept", "dist/artifact", "ignored/hidden"] {
                std::fs::write(root.path().join(file), "a").unwrap();
            }
            std::fs::write(root.path().join(policy), "ignored/\n").unwrap();
            if git {
                for args in [&["init", "-q"][..], &["add", "-A"]] {
                    assert!(paths::git(root.path(), args).unwrap().status.success());
                }
            }
            // Recent policies keep any listing from being reused.
            std::thread::sleep(Duration::from_millis(2100));
            let plain = snapshot(&workspace, &graph, false);
            let expected = plain.current_files(root.path()).unwrap();
            let watched = snapshot(&workspace, &graph, true);
            assert_eq!(watched.files().unwrap(), plain.files().unwrap());
            assert_eq!(watched.files().unwrap(), &*expected);
            assert!(watched.files().unwrap().contains("src/nested/kept"));
            assert!(!watched.files().unwrap().contains("dist/artifact"));
            assert!(!watched.files().unwrap().contains("ignored/hidden"));
            let seeded = watched
                .membership
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .files
                .clone();
            assert!(Arc::ptr_eq(
                &seeded,
                &watched.current_files(root.path()).unwrap()
            ));
            std::fs::write(root.path().join("src/empty/added"), "b").unwrap();
            assert!(
                watched
                    .current_files(root.path())
                    .unwrap()
                    .contains("src/empty/added")
            );
        }
    }

    #[test]
    fn changes_between_stamping_and_listing_leave_the_listing_unsettled() {
        let (root, _, _) = fixture(None);
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/original"), "one").unwrap();
        let ignore = Arc::new(SourceIgnore::new(root.path()).unwrap());
        let mut membership = Membership::new(
            root.path(),
            &BTreeSet::new(),
            &ignore,
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        // Model a change made after the walk but before the listing: the
        // listing shows it, and the stamps taken earlier do not.
        std::fs::rename(
            root.path().join("src/original"),
            root.path().join("src/renamed"),
        )
        .unwrap();
        membership.list(&BTreeSet::new(), true).unwrap();
        assert!(membership.files.contains("src/renamed"));
        assert!(!membership.files.contains("src/original"));
        assert!(!membership.settled);
        assert!(!membership.state(None, SystemTime::now()).unwrap().0);
    }

    #[test]
    fn candidates_beneath_unwatched_directories_leave_the_listing_unsettled() {
        let (root, workspace, graph) = fixture(None);
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let ignore = Arc::new(SourceIgnore::new(root.path()).unwrap());
        let membership = Membership::new(
            root.path(),
            &BTreeSet::new(),
            &ignore,
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        let files = |paths: &[&str]| paths.iter().map(|path| (*path).to_owned()).collect();
        assert!(membership.covers(&files(&["top", "src/a", "src/nested/b", "src/nested/c"])));
        assert!(!membership.covers(&files(&["src/nested/b", "src/missing/c"])));
        assert!(!membership.covers(&files(&["missing/deeper/c", "src/a"])));
        // A deleted path a caller adds is kept, though no directory holds it:
        // the listing is not reused until a refresh watches where it was.
        let snapshot = snapshot(&workspace, &graph, true).with_candidates(&["gone/file".into()]);
        assert!(snapshot.files().unwrap().contains("gone/file"));
        let seeded = snapshot.membership.lock().unwrap().clone().unwrap();
        assert!(seeded.files.contains("gone/file"));
        assert!(!seeded.settled);
        let refreshed = snapshot.current_files(root.path()).unwrap();
        assert!(!Arc::ptr_eq(&seeded.files, &refreshed));
        assert!(refreshed.contains("gone/file"));
        let membership = snapshot.membership.lock().unwrap().clone().unwrap();
        assert!(membership.stamps.contains_key(&root.path().join("gone")));
    }

    #[test]
    fn identical_ignore_rewrites_reuse_candidates_and_changed_rules_refresh() {
        for watched in [false, true] {
            for name in [".gitignore", ".nxignore"] {
                let (root, workspace, graph) = fixture(None);
                let policy = root.path().join(name);
                std::fs::write(root.path().join("alpha"), "a").unwrap();
                std::fs::write(root.path().join("bravo"), "b").unwrap();
                std::fs::write(&policy, "alpha\n").unwrap();
                let snapshot = snapshot(&workspace, &graph, watched);
                let original = snapshot.current_files(root.path()).unwrap();
                assert!(!original.contains("alpha"));
                assert!(original.contains("bravo"));
                for _ in 0..3 {
                    std::fs::write(&policy, "alpha\n").unwrap();
                    assert!(Arc::ptr_eq(
                        &original,
                        &snapshot.current_files(root.path()).unwrap()
                    ));
                }
                // Same-length content can change while coarse timestamps appear
                // identical. Recent policies must still compare their contents.
                std::fs::write(&policy, "bravo\n").unwrap();
                {
                    let mut cached = snapshot.membership.lock().unwrap();
                    Arc::make_mut(cached.as_mut().unwrap())
                        .stamps
                        .insert(policy.clone(), membership_stamp(&policy).unwrap());
                }
                let changed = snapshot.current_files(root.path()).unwrap();
                assert!(!Arc::ptr_eq(&original, &changed));
                assert!(changed.contains("alpha"));
                assert!(!changed.contains("bravo"));
                std::fs::remove_file(&policy).unwrap();
                assert!(
                    snapshot
                        .current_files(root.path())
                        .unwrap()
                        .contains("bravo")
                );
            }
        }
    }

    #[test]
    fn ignore_contents_changed_after_inventory_cannot_bless_a_listing() {
        let (root, workspace, graph) = fixture(None);
        let policy = root.path().join(".nxignore");
        std::fs::write(&policy, "alpha\n").unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(root.path()).unwrap();
        let membership = snapshot.membership.lock().unwrap().clone().unwrap();
        std::fs::write(&policy, "bravo\n").unwrap();
        assert!(!membership.state(None, SystemTime::now()).unwrap().0);
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
        assert!(!membership.state(None, SystemTime::now()).unwrap().0);
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
        assert!(!membership.state(None, SystemTime::now()).unwrap().0);
    }

    #[test]
    fn output_publication_reuses_candidates_but_source_names_still_refresh() {
        for watched in [false, true] {
            for partial in [false, true] {
                let outputs = if partial {
                    json!(["{projectRoot}/dist", "!{projectRoot}/dist/keep"])
                } else {
                    json!(["{projectRoot}/dist"])
                };
                let (root, workspace, graph) = fixture(Some(outputs));
                std::fs::create_dir_all(root.path().join("src")).unwrap();
                std::fs::write(root.path().join("src/original"), "one").unwrap();
                if partial {
                    std::fs::create_dir_all(root.path().join("dist/keep")).unwrap();
                }
                let snapshot = snapshot(&workspace, &graph, watched);
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
    }

    #[test]
    fn concurrent_fingerprints_refresh_without_replacing_newer_membership() {
        for watched in [false, true] {
            let (root, workspace, graph) = fixture(None);
            std::fs::create_dir_all(root.path().join("src")).unwrap();
            let snapshot = snapshot(&workspace, &graph, watched);
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
        let first = snapshot
            .current_files_for(root.path(), None, None, "test")
            .unwrap();
        policy("beta");
        let second = snapshot
            .current_files_for(root.path(), None, None, "test")
            .unwrap();
        policy("gamma");
        let third = snapshot
            .current_files_for(root.path(), None, None, "test")
            .unwrap();
        // Deterministically pause two fingerprints after candidate acquisition,
        // then let another fingerprint refresh policy twice before they resolve.
        let cancelled = AtomicBool::new(false);
        let expected = |name: &str| {
            BTreeSet::from([
                ".nxignore".into(),
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
            assert_eq!(whole(&resolved), expected(name));
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
        assert_eq!(whole(&inspected), expected("alpha"));
    }

    #[test]
    fn scoped_membership_defers_unrelated_changes_until_a_matching_task_asks() {
        for watched in [false, true] {
            let (root, mut workspace, _) = fixture(Some(json!(["{projectRoot}/dist"])));
            let targets = &mut workspace.projects.get_mut("app").unwrap().targets;
            targets.get_mut("post").unwrap().inputs =
                Some(vec![json!("{workspaceRoot}/left/**/*.txt")]);
            targets.get_mut("gen").unwrap().inputs =
                Some(vec![json!("{workspaceRoot}/right/**/*.txt")]);
            targets.get_mut("build").unwrap().inputs =
                Some(vec![json!("{workspaceRoot}/**/*.txt")]);
            for directory in ["left/empty", "right/empty"] {
                std::fs::create_dir_all(root.path().join(directory)).unwrap();
            }
            let graph =
                TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
            let snapshot = snapshot(&workspace, &graph, watched);
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
                .current_files_for(&workspace.root, Some(&left), None, "test")
                .unwrap();
            std::fs::write(root.path().join("right/empty/new.txt"), "right").unwrap();
            let unrelated = snapshot
                .current_files_for(&workspace.root, Some(&left), None, "test")
                .unwrap();
            assert!(Arc::ptr_eq(&before.files, &unrelated.files));
            assert!(!unrelated.files.contains("right/empty/new.txt"));
            assert!(
                snapshot
                    .current_files_for(&workspace.root, Some(&right), None, "test")
                    .unwrap()
                    .files
                    .contains("right/empty/new.txt")
            );
            std::fs::create_dir_all(root.path().join("left/empty/nested")).unwrap();
            std::fs::write(root.path().join("left/empty/nested/new.txt"), "left").unwrap();
            assert!(
                snapshot
                    .current_files_for(&workspace.root, Some(&left), None, "test")
                    .unwrap()
                    .files
                    .contains("left/empty/nested/new.txt")
            );
            std::fs::write(root.path().join("right/empty/another.txt"), "right").unwrap();
            assert!(
                snapshot
                    .current_files_for(&workspace.root, Some(&all), None, "test")
                    .unwrap()
                    .files
                    .contains("right/empty/another.txt")
            );
        }
    }

    #[test]
    fn membership_watches_the_directories_json_globs_select_from() {
        for watched in [false, true] {
            let (root, mut workspace, _) = fixture(Some(json!(["{projectRoot}/dist"])));
            let targets = &mut workspace.projects.get_mut("app").unwrap().targets;
            targets.get_mut("build").unwrap().inputs = Some(vec![json!(
                {"json": "{workspaceRoot}/config/**/*.json", "fields": ["version"]}
            )]);
            std::fs::create_dir_all(root.path().join("config/empty")).unwrap();
            let graph =
                TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
            let snapshot = snapshot(&workspace, &graph, watched);
            snapshot.current_files(&workspace.root).unwrap();
            std::thread::sleep(Duration::from_millis(2100));
            let prefixes = snapshot
                .membership_prefixes(&workspace, &graph.tasks["app:build"])
                .unwrap();
            snapshot
                .current_files_for(&workspace.root, Some(&prefixes), None, "test")
                .unwrap();
            std::fs::write(
                root.path().join("config/empty/new.json"),
                r#"{"version": 1}"#,
            )
            .unwrap();
            assert!(
                snapshot
                    .current_files_for(&workspace.root, Some(&prefixes), None, "test")
                    .unwrap()
                    .files
                    .contains("config/empty/new.json")
            );
        }
    }

    #[test]
    fn membership_watches_git_ignored_tracked_parents_and_index_policy() {
        for watched in [false, true] {
            let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
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
            let snapshot = snapshot(&workspace, &graph, watched);
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
    }

    #[test]
    fn membership_watches_ignore_files_found_deep_in_the_walk() {
        for watched in [false, true] {
            let (root, workspace, graph) = fixture(None);
            let nested = root.path().join("packages/app/src/deep");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("kept"), "a").unwrap();
            std::fs::write(nested.join("hidden"), "b").unwrap();
            let policy = root.path().join("packages/app/.gitignore");
            std::fs::write(&policy, "\n").unwrap();
            let snapshot = snapshot(&workspace, &graph, watched);
            let files = || snapshot.current_files(root.path()).unwrap();
            files();
            // Waiting lets the listing settle, so it is reused, as in
            // `membership_reuses_settled_candidates_and_detects_directory_changes`.
            std::thread::sleep(Duration::from_millis(2100));
            let before = files();
            assert!(Arc::ptr_eq(&before, &files()));
            assert!(before.contains("packages/app/src/deep/hidden"));
            // Edited in place, so no directory's names change: only the policy
            // the walk found can refuse the settled listing.
            std::fs::write(&policy, "hidden\n").unwrap();
            let after = files();
            assert!(!after.contains("packages/app/src/deep/hidden"));
            assert!(after.contains("packages/app/src/deep/kept"));
        }
    }

    #[test]
    fn a_new_git_config_include_invalidates_settled_listing_policy() {
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
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
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
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
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
        let policy = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(policy.path(), "hidden\n").unwrap();
        std::os::unix::fs::symlink(policy.path(), root.path().join(".gitignore")).unwrap();
        std::fs::write(root.path().join("hidden"), "source").unwrap();
        let written = Instant::now();
        let window = crate::settle::window(
            policy.path(),
            &std::fs::symlink_metadata(policy.path()).unwrap(),
        );
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        assert!(
            !snapshot
                .current_files(root.path())
                .unwrap()
                .contains("hidden")
        );
        // Listed within the policy's window of its write, as a slow machine
        // can miss on the shortest windows, the listing is not reused.
        let unsettled = !snapshot
            .membership
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .settled;
        assert!(unsettled || written.elapsed() >= window);
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

    /// An input linking to a directory keys every file beneath it, the
    /// Git-ignored ones included, such as a dotenv file there; one linking to
    /// the workspace root cannot be keyed.
    #[cfg(unix)]
    #[test]
    fn a_linked_directory_input_keys_ignored_files_beneath_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "app", "targets": {
                "read": {"command": "true", "inputs": ["{projectRoot}/linked"]},
                "whole": {"command": "true", "inputs": ["{projectRoot}/everything"]}
            }})
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join(".gitignore"), ".env\n").unwrap();
        std::fs::create_dir(root.path().join("shared")).unwrap();
        std::fs::write(root.path().join("shared/a.ts"), "a").unwrap();
        std::fs::write(root.path().join("shared/.env"), "X=1\n").unwrap();
        std::os::unix::fs::symlink("shared", root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(".", root.path().join("everything")).unwrap();
        assert!(
            paths::git(root.path(), &["init", "--quiet"])
                .unwrap()
                .status
                .success()
        );
        let workspace = Workspace::load(root.path()).unwrap();
        let requests = ["app:read", "app:whole"].map(|request| Request::parse(request).unwrap());
        let graph = TaskGraph::build(&workspace, &requests).unwrap();
        let key = |id: &str| {
            let task = &graph.tasks[id];
            let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
            let snapshot =
                Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
            inputs(
                &snapshot,
                &workspace,
                task,
                &prepared,
                &BTreeMap::new(),
                &AtomicBool::new(false),
            )
        };
        let before = key("app:read").unwrap();
        std::fs::write(root.path().join("shared/.env"), "X=2\n").unwrap();
        assert_ne!(before, key("app:read").unwrap());
        let error = format!("{:#}", key("app:whole").unwrap_err());
        assert!(error.contains("resolves to the workspace root"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn recent_change_time_prevents_reuse_despite_old_mtime() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        File::open(root.path())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let metadata = std::fs::metadata(root.path()).unwrap();
        assert!(metadata.ctime() > 1);
        let changed = SystemTime::UNIX_EPOCH
            + Duration::new(metadata.ctime() as u64, metadata.ctime_nsec() as u32);
        let stamp = Stamp::new(&metadata);
        let window = crate::settle::window(root.path(), &metadata);
        for looked in [changed, changed + window] {
            assert!(!stamp.settled(window, looked));
            assert!(!stamp.membership_settled(window, looked));
        }
        let later = changed + window + Duration::from_millis(1);
        assert!(stamp.settled(window, later));
        assert!(stamp.membership_settled(window, later));
    }

    /// A file's stamp is trusted once both its times are older than its
    /// filesystem's window, and not within it, whatever that window is.
    #[test]
    fn a_stamp_settles_after_its_filesystems_window() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, "same").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        #[allow(unused_mut)]
        let mut newest = metadata.modified().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            newest = newest.max(
                SystemTime::UNIX_EPOCH
                    + Duration::new(metadata.ctime() as u64, metadata.ctime_nsec() as u32),
            );
        }
        let stamp = Stamp::new(&metadata);
        for window in [
            crate::settle::window(&path, &metadata),
            crate::settle::FALLBACK,
            Duration::from_millis(10),
        ] {
            assert!(!stamp.settled(window, newest));
            assert!(!stamp.settled(window, newest + window / 2));
            assert!(stamp.settled(window, newest + window + Duration::from_millis(1)));
        }
    }

    /// A file rewritten with as many bytes inside its filesystem's window of
    /// the last write is read again: its digest was not kept.
    #[test]
    fn a_same_size_rewrite_within_the_window_is_read_again() {
        let (root, workspace, graph) =
            literal_fixture(false, true, json!(["{workspaceRoot}/config.yaml"]));
        let path = root.path().join("config.yaml");
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        std::fs::write(&path, "first").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let window = crate::settle::window(&path, &metadata);
        let before = Instant::now();
        let first = snapshot.digest("config.yaml", &path, &metadata).unwrap();
        assert_eq!(first, digest_file(&path).unwrap());
        // Only meaningful where both looks fell inside the window, which a
        // slow machine can miss on the shortest windows.
        let kept = snapshot.digests.lock().unwrap().contains_key("config.yaml");
        if before.elapsed() < window {
            assert!(!kept);
        }
        std::fs::write(&path, "again").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let second = snapshot.digest("config.yaml", &path, &metadata).unwrap();
        if before.elapsed() < window {
            assert_eq!(second, digest_file(&path).unwrap());
        }
        // Past the window, a file's digest is kept, and taken for its stamp.
        settle(&path);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        snapshot.digest("config.yaml", &path, &metadata).unwrap();
        assert!(snapshot.digests.lock().unwrap().contains_key("config.yaml"));
    }

    /// A listing taken once every directory and policy is older than its
    /// filesystem's window is reused, and one taken within it is not.
    #[test]
    fn a_listing_is_reused_once_its_filesystems_window_has_passed() {
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
        std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        snapshot.current_files(root.path()).unwrap();
        settle(&root.path().join(".gitignore"));
        settle(root.path());
        // The first look past the window stamps the inventory again.
        let settled = snapshot.current_files(root.path()).unwrap();
        assert!(
            snapshot
                .membership
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .settled
        );
        assert!(Arc::ptr_eq(
            &settled,
            &snapshot.current_files(root.path()).unwrap()
        ));
        // A policy rewritten with as many bytes is read again.
        std::fs::write(root.path().join(".gitignore"), "src/xxxx\n").unwrap();
        let changed = snapshot.current_files(root.path()).unwrap();
        assert!(!Arc::ptr_eq(&settled, &changed));
    }

    /// Waits until a file just written is older than its filesystem's window.
    fn settle(path: &Path) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        std::thread::sleep(crate::settle::window(path, &metadata) + Duration::from_millis(20));
    }

    #[test]
    fn inspection_keeps_deleted_candidates() {
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/dist"])));
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
        assert!(resolved.declared.files.contains("src/deleted"));
    }

    #[test]
    fn undeclared_writes_invalidate_dependency_values() {
        for outputs in [None, Some(json!(["{projectRoot}/generated"]))] {
            let (root, workspace, graph) = fixture(outputs.clone());
            let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
            let keys = || {
                dependency_keys(&snapshot, &workspace, &graph, &graph.tasks["app:build"]).unwrap()
            };
            let before = keys();
            std::fs::write(root.path().join("generated/value"), "different").unwrap();
            // As `post` would have written, having run its command.
            let written = match &outputs {
                Some(_) => Written::Within(vec!["generated".into()]),
                None => Written::Anything,
            };
            snapshot.outputs_written(&workspace, &graph.tasks["app:post"], written, None);
            assert_ne!(before, keys());
        }
    }

    /// A listing is kept until a task that may have written where it lies is
    /// done: one that wrote nothing, or wrote elsewhere, leaves it.
    #[test]
    fn listings_are_dropped_only_by_writes_that_touch_them() {
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/elsewhere"])));
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        let keys =
            || dependency_keys(&snapshot, &workspace, &graph, &graph.tasks["app:build"]).unwrap();
        let before = keys();
        // A file the listing does not hold, which only a fresh listing sees.
        std::fs::write(root.path().join("generated/added"), "added").unwrap();
        let post = &graph.tasks["app:post"];
        let beside = |patterns: &[&str], beside: &[&str]| {
            let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
            let beside = beside.iter().map(|path| (*path).to_owned()).collect();
            Written::Declared(Box::new(Outputs::from_paths(&patterns).unwrap()), beside)
        };
        let declared = |patterns: &[&str]| beside(patterns, &[]);
        for written in [
            Written::Nothing,
            Written::Within(vec!["elsewhere".into()]),
            Written::Within(vec!["generate".into()]),
            declared(&["elsewhere/*/generated", "generate/*"]),
            beside(&["elsewhere/*"], &["generate", "warm"]),
        ] {
            snapshot.outputs_written(&workspace, post, written, None);
            assert_eq!(before, keys());
        }
        for written in [
            Written::Within(vec!["generated/added".into()]),
            Written::Within(vec![".".into(), "generated".into()]),
            declared(&["generated/*.txt"]),
            // Warm state restored where the outputs do not reach.
            beside(&["elsewhere/*"], &["generated"]),
            beside(&["elsewhere/*"], &["warm", "generated/added"]),
            Written::Anything,
        ] {
            snapshot.outputs_written(&workspace, post, Written::Nothing, None);
            assert_eq!(before, keys());
            snapshot.outputs_written(&workspace, post, written.clone(), None);
            let after = keys();
            assert_ne!(before, after, "{written:?}");
            std::fs::remove_file(root.path().join("generated/added")).unwrap();
            snapshot.outputs_written(&workspace, post, Written::Anything, None);
            assert_eq!(before, keys());
            std::fs::write(root.path().join("generated/added"), "added").unwrap();
        }
    }

    /// What a restore listed stands for a dependency's outputs, unless a task
    /// done since the restore began may have written among them.
    #[test]
    fn restored_listings_stand_unless_written_since() {
        let (root, workspace, graph) = fixture(Some(json!(["{projectRoot}/elsewhere"])));
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        let keys =
            || dependency_keys(&snapshot, &workspace, &graph, &graph.tasks["app:build"]).unwrap();
        let fresh = || {
            recheck_dependency_keys(&snapshot, &workspace, &graph, &graph.tasks["app:build"])
                .unwrap()
        };
        let generate = &graph.tasks["app:gen"];
        let post = &graph.tasks["app:post"];
        std::fs::write(root.path().join("generated/second"), "two").unwrap();
        // A restore that listed one of the two files, which only shows when
        // its listing is the one used.
        let restored = |since| RestoredOutputs {
            since,
            files: vec!["generated/value".into()],
        };
        let since = snapshot.writes_seen();
        snapshot.outputs_written(
            &workspace,
            post,
            Written::Within(vec!["elsewhere".into()]),
            None,
        );
        snapshot.outputs_written(
            &workspace,
            generate,
            Written::Within(vec!["generated".into()]),
            Some(restored(since)),
        );
        let handed = keys();
        assert_ne!(handed, fresh());
        // Dropped by a write among its files, and listed again.
        snapshot.outputs_written(
            &workspace,
            post,
            Written::Within(vec!["generated".into()]),
            None,
        );
        assert_eq!(keys(), fresh());
        // Not kept when such a write came while it was restoring.
        let since = snapshot.writes_seen();
        snapshot.outputs_written(
            &workspace,
            post,
            Written::Within(vec!["generated".into()]),
            None,
        );
        snapshot.outputs_written(
            &workspace,
            generate,
            Written::Nothing,
            Some(restored(since)),
        );
        assert_eq!(keys(), fresh());
        // Kept when the restore left its outputs as they were and nothing
        // was written since.
        let since = snapshot.writes_seen();
        snapshot.outputs_written(
            &workspace,
            generate,
            Written::Nothing,
            Some(restored(since)),
        );
        assert_eq!(keys(), handed);
    }

    #[cfg(unix)]
    #[test]
    fn dependency_symlinks_recheck_targets_outside_output_roots() {
        let (root, mut workspace, _) = fixture(Some(json!(["{projectRoot}/generated"])));
        workspace
            .projects
            .get_mut("app")
            .unwrap()
            .targets
            .get_mut("gen")
            .unwrap()
            .outputs = Some(vec!["{projectRoot}/links".into()]);
        std::fs::create_dir(root.path().join("links")).unwrap();
        std::os::unix::fs::symlink("../generated/value", root.path().join("links/value")).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        let keys =
            || dependency_keys(&snapshot, &workspace, &graph, &graph.tasks["app:build"]).unwrap();
        let before = keys();
        std::fs::write(root.path().join("generated/value"), "different").unwrap();
        snapshot.outputs_written(
            &workspace,
            &graph.tasks["app:post"],
            Written::Within(vec!["generated".into()]),
            None,
        );
        assert_ne!(before["app:gen:**/*"], keys()["app:gen:**/*"]);
    }
    /// The digest of a git-ignored file an `includeIgnored` fileset reads is
    /// saved with the others, so a warm run does not read it again.
    #[test]
    fn digests_of_ignored_files_read_by_include_ignored_are_saved() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "app", "targets": {"build": {"command": "true", "inputs": [
                {"fileset": "{projectRoot}/generated/**/*", "includeIgnored": true}
            ]}}})
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(root.path().join("generated/schema.json"), "{}").unwrap();
        // Digests are only saved for files old enough to have settled.
        settle(&root.path().join("generated/schema.json"));
        let git = |args: &[&str]| assert!(paths::git(root.path(), args).unwrap().status.success());
        git(&["init", "--quiet"]);
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        assert!(!snapshot.files().unwrap().contains("generated/schema.json"));
        let prepared =
            qk_executor::prepare(&workspace, &graph.tasks["app:build"], &BTreeMap::new()).unwrap();
        inputs(
            &snapshot,
            &workspace,
            &graph.tasks["app:build"],
            &prepared,
            &BTreeMap::new(),
            &AtomicBool::new(false),
        )
        .unwrap();
        snapshot.save_digests(root.path());
        assert!(load_digests(root.path()).contains_key("generated/schema.json"));
    }

    /// A run that lists the workspace saves the digests of the files it read
    /// beside the listing, such as another task's outputs, gitignored, that a
    /// task keys, and drops those of files it neither listed nor read.
    #[test]
    fn digests_of_other_tasks_outputs_a_task_keys_are_saved() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "app", "targets": {
                "gen": {"command": "true", "outputs": ["{projectRoot}/generated"]},
                "build": {"command": "true", "dependsOn": ["gen"], "inputs": [
                    "{projectRoot}/src/**/*", {"dependentTasksOutputFiles": "**/*.ts"}
                ]}
            }})
            .to_string(),
        )
        .unwrap();
        // Digests are only saved for files old enough to have settled.
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        for path in ["src/a.ts", "generated/names.ts"] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        assert!(
            paths::git(root.path(), &["init", "--quiet"])
                .unwrap()
                .status
                .success()
        );
        let path = digests_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            json!({"version": 1, "entries": {"generated/gone.ts": [[1, 2], "digest"]}}).to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
        assert!(!snapshot.files().unwrap().contains("generated/names.ts"));
        let task = &graph.tasks["app:build"];
        let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
        let dependencies = dependency_keys(&snapshot, &workspace, &graph, task).unwrap();
        inputs(
            &snapshot,
            &workspace,
            task,
            &prepared,
            &dependencies,
            &AtomicBool::new(false),
        )
        .unwrap();
        snapshot.save_digests(root.path());
        let saved = load_digests(root.path());
        assert!(saved.contains_key("src/a.ts"));
        assert!(saved.contains_key("generated/names.ts"));
        assert!(!saved.contains_key("generated/gone.ts"));
    }

    /// A workspace whose `app:build` reads `inputs`, beside `app:gen`, which
    /// declares outputs, warm paths and scratch, among paths each listing
    /// treats in its own way. With `git`, a repository tracks some of them;
    /// with `nxignore`, the workspace has a root `.nxignore`, so the listing
    /// walks rather than asking Git.
    fn literal_fixture(
        git: bool,
        nxignore: bool,
        inputs: Value,
    ) -> (tempfile::TempDir, Workspace, TaskGraph) {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, content: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("nx.json", "{}");
        write(
            "project.json",
            &json!({"name": "app", "targets": {
                "gen": {"command": "true", "outputs": ["{projectRoot}/dist"],
                    "inputs": ["{workspaceRoot}/config.yaml"],
                    "qk:warm": {"paths": ["warm"]},
                    "qk:sandbox": {"scratch": ["{projectRoot}/scratch"]}},
                "build": {"command": "true", "inputs": inputs}
            }})
            .to_string(),
        );
        write(".gitignore", "ignored.yaml\n*.log\nskip/\n");
        if nxignore {
            write(".nxignore", "nx-ignored.yaml\n");
        }
        for path in [
            "config.yaml",
            "ignored.yaml",
            "nx-ignored.yaml",
            "sub/nested-ignored.yaml",
            "sub/tracked-nested.yaml",
            "sub/nx-nested.yaml",
            "sub/kept.yaml",
            "node_modules/pkg/package.json",
            "dir/a.txt",
            "dir/x.log",
            "dir/deep/b.txt",
            "dir/skip/c.txt",
            "dist/out.txt",
            "warm/state.bin",
            "scratch/tmp.txt",
            "cache/entry",
            ".qk/state",
            "deleted.yaml",
            "skipped.yaml",
        ] {
            write(path, path);
        }
        write(
            "sub/.gitignore",
            "nested-ignored.yaml\ntracked-nested.yaml\n",
        );
        write("sub/.nxignore", "nx-nested.yaml\n");
        #[cfg(unix)]
        for (target, link) in [
            ("config.yaml", "link-file"),
            ("dir", "link-dir"),
            ("missing", "dangling"),
        ] {
            std::os::unix::fs::symlink(target, root.path().join(link)).unwrap();
        }
        if git {
            let git = |args: &[&str]| {
                assert!(paths::git(root.path(), args).unwrap().status.success());
            };
            git(&["init", "--quiet"]);
            git(&["add", "."]);
            git(&["add", "--force", "sub/tracked-nested.yaml"]);
            git(&["update-index", "--skip-worktree", "skipped.yaml"]);
            std::fs::remove_file(root.path().join("skipped.yaml")).unwrap();
        }
        std::fs::remove_file(root.path().join("deleted.yaml")).unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(
            &workspace,
            &[
                Request::parse("app:build").unwrap(),
                Request::parse("app:gen").unwrap(),
            ],
        )
        .unwrap();
        (root, workspace, graph)
    }

    /// Paths a literal input can name, each of which some listing treats in
    /// its own way.
    const LITERAL_PATHS: &[&str] = &[
        "config.yaml",
        "absent.yaml",
        "ignored.yaml",
        "nx-ignored.yaml",
        "sub/nested-ignored.yaml",
        "sub/tracked-nested.yaml",
        "sub/nx-nested.yaml",
        "sub",
        "node_modules/pkg/package.json",
        "node_modules",
        "dir",
        "dir/deep",
        "dir/x.log",
        "link-file",
        "link-dir",
        "link-dir/a.txt",
        "dangling",
        "dist",
        "dist/out.txt",
        "warm/state.bin",
        "scratch/tmp.txt",
        "cache/entry",
        ".qk/state",
        ".git/HEAD",
        "deleted.yaml",
        "skipped.yaml",
        "config.yaml/below",
    ];

    /// Listing the paths literal inputs name lists there what listing the
    /// whole workspace does: the same files, whether absent, ignored by Git
    /// or Nx at any depth, tracked though ignored, in `node_modules`, a
    /// directory, a link or beneath one, or a declared output, warm path,
    /// scratch or the cache.
    #[test]
    fn literal_paths_list_what_the_workspace_listing_holds_there() {
        for (git, nxignore) in [(true, false), (true, true), (false, true), (false, false)] {
            let (root, workspace, graph) = literal_fixture(git, nxignore, json!([]));
            let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache"))
                .unwrap()
                .with_candidates(&["gone/file".into(), "dir/removed.txt".into()]);
            assert!(!snapshot.scratch.is_empty());
            let all = snapshot.current_files(root.path()).unwrap();
            let sets = LITERAL_PATHS
                .iter()
                .map(|path| BTreeSet::from([path.to_string()]))
                .chain([
                    LITERAL_PATHS.iter().map(|path| path.to_string()).collect(),
                    BTreeSet::from(["gone".to_owned(), "dir".to_owned()]),
                ]);
            for paths in sets {
                let listed = snapshot.candidates_at(&paths).unwrap();
                // Without Git, the workspace listing walks, where one of
                // some paths would ask Git, so it is left to the listing.
                if !git && !nxignore {
                    assert!(listed.is_none());
                    continue;
                }
                let expected: BTreeSet<String> = all
                    .iter()
                    .filter(|path| within(&paths, path))
                    .cloned()
                    .collect();
                assert_eq!(
                    *listed.unwrap().files,
                    expected,
                    "{paths:?} with git {git}, nxignore {nxignore}"
                );
            }
            if !git && !nxignore {
                continue;
            }
            // Each case is one the listings tell apart.
            for path in [
                "config.yaml",
                "sub/kept.yaml",
                "dir/a.txt",
                "dir/deep/b.txt",
            ] {
                assert!(all.contains(path), "{path}");
            }
            for path in [
                "ignored.yaml",
                "sub/nested-ignored.yaml",
                "node_modules/pkg/package.json",
                "dir/x.log",
                "dist/out.txt",
                "warm/state.bin",
                "scratch/tmp.txt",
                "cache/entry",
                ".qk/state",
                "deleted.yaml",
                "link-dir/a.txt",
            ] {
                assert!(!all.contains(path), "{path}");
            }
            assert!(all.contains("gone/file") && all.contains("dir/removed.txt"));
            assert_eq!(all.contains("nx-ignored.yaml"), !nxignore);
            assert_eq!(all.contains("sub/nx-nested.yaml"), !nxignore);
            assert_eq!(all.contains("sub/tracked-nested.yaml"), !nxignore);
            assert_eq!(all.contains("skipped.yaml"), !git);
            #[cfg(unix)]
            for path in ["link-file", "link-dir", "dangling"] {
                assert!(all.contains(path), "{path}");
            }
        }
    }

    /// Work split across threads comes back in order, and fails as taking
    /// it in turn would: with the first error in that order.
    #[test]
    fn parallel_work_keeps_its_order_and_its_first_error() {
        let items: Vec<usize> = (0..5000).collect();
        assert_eq!(in_parallel(&items, 16, |item| Ok(item * 2)).unwrap(), {
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        });
        let failed = in_parallel(&items, 16, |item| {
            if *item % 1000 == 999 {
                bail!("{item}");
            }
            Ok(*item)
        });
        assert_eq!(failed.unwrap_err().to_string(), "999");
    }

    /// A snapshot for keying lists the workspace beside the tasks it keys:
    /// one whose filesets name paths is keyed without waiting for the
    /// listing, as it would be after it, and one that matches files takes it.
    #[test]
    fn tasks_naming_paths_are_keyed_while_the_workspace_is_listed() {
        let (_root, workspace, graph) = literal_fixture(
            true,
            false,
            json!(["{workspaceRoot}/config.yaml", "{workspaceRoot}/dir"]),
        );
        let mut graph = graph;
        let mut lint = graph.tasks["app:build"].clone();
        lint.id = "app:lint".into();
        lint.definition.inputs = Some(vec![json!("{projectRoot}/**/*.yaml")]);
        graph.tasks.insert(lint.id.clone(), lint);
        let cache = workspace.root.join("cache");
        let key = |snapshot: &Snapshot, id: &str| {
            let task = &graph.tasks[id];
            let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
            let dependencies = dependency_keys(snapshot, &workspace, &graph, task).unwrap();
            let inputs = super::inputs(
                snapshot,
                &workspace,
                task,
                &prepared,
                &dependencies,
                &AtomicBool::new(false),
            )
            .unwrap();
            key(&inputs).unwrap()
        };
        let watched = Snapshot::watched(&workspace, &graph, &cache).unwrap();
        let named = key(&watched, "app:build");
        assert!(watched.listing.lock().unwrap().is_some());
        let matched = key(&watched, "app:lint");
        assert!(watched.listing.lock().unwrap().is_none());
        assert!(watched.membership.lock().unwrap().is_some());
        let listed = Snapshot::new(&workspace, &graph, &cache).unwrap();
        listed.files().unwrap();
        assert_eq!(named, key(&listed, "app:build"));
        assert_eq!(matched, key(&listed, "app:lint"));
    }

    /// A task whose filesets each name a path keys as it would from the
    /// workspace's listing, and a run of such tasks alone lists nothing.
    #[test]
    fn literal_inputs_key_as_the_workspace_listing_does() {
        let mut inputs: Vec<Value> = LITERAL_PATHS
            .iter()
            .filter(|path| !matches!(**path, "config.yaml/below" | "link-dir"))
            .map(|path| json!(format!("{{workspaceRoot}}/{path}")))
            .collect();
        inputs.extend([
            json!("{projectRoot}/sub/kept.yaml"),
            json!("!{workspaceRoot}/dir/deep"),
            json!({"json": "{workspaceRoot}/config.yaml", "fields": ["a"]}),
            json!({"fileset": "{projectRoot}/dist", "includeIgnored": true}),
        ]);
        for (git, nxignore) in [(true, false), (true, true), (false, true)] {
            let (root, workspace, graph) = literal_fixture(git, nxignore, json!(inputs));
            let snapshot =
                Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
            assert!(snapshot.files.get().is_none());
            let task = &graph.tasks["app:build"];
            let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
            let cancelled = AtomicBool::new(false);
            let dependencies = dependency_keys(&snapshot, &workspace, &graph, task).unwrap();
            let literal = super::inputs(
                &snapshot,
                &workspace,
                task,
                &prepared,
                &dependencies,
                &cancelled,
            )
            .unwrap();
            assert!(snapshot.files.get().is_none());
            assert!(literal["files"].get("config.yaml").is_some());
            assert!(literal["files"].get("dir/a.txt").is_some());
            assert!(literal["files"].get("dir/deep/b.txt").is_none());
            let listed = inputs_from(
                &snapshot,
                &workspace,
                task,
                &prepared,
                &dependencies,
                &cancelled,
                &snapshot
                    .current_files_for(root.path(), None, None, "test")
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(literal, listed, "git {git}, nxignore {nxignore}");
        }
    }

    /// One task whose inputs match files, or which recordings may key, makes
    /// the snapshot list the workspace's files as it is taken, with their
    /// directory inventory when the snapshot is for keying.
    #[test]
    fn snapshots_list_files_when_a_task_matches_them() {
        for (target, matching) in [
            (
                json!({"command": "true", "inputs": ["{workspaceRoot}/config.yaml"]}),
                false,
            ),
            (
                json!({"command": "true", "inputs": ["{workspaceRoot}/*.yaml"]}),
                true,
            ),
            (json!({"command": "true"}), true),
            (
                json!({"command": "true", "inputs": [{"json": "{workspaceRoot}/*.json"}]}),
                true,
            ),
            (
                json!({"command": "true", "inputs": ["{workspaceRoot}/"]}),
                true,
            ),
            (
                json!({"command": "true", "inputs": ["{workspaceRoot}/config.yaml"],
                    "qk:ultracache": {}}),
                true,
            ),
            // Not recorded, so keyed by its declared inputs alone.
            (
                json!({"command": "true", "inputs": ["{workspaceRoot}/config.yaml"],
                    "qk:ultracache": {"mode": "off"}}),
                false,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("nx.json"), "{}").unwrap();
            std::fs::write(root.path().join("config.yaml"), "a: 1").unwrap();
            std::fs::write(
                root.path().join("project.json"),
                json!({"name": "app", "targets": {"build": target}}).to_string(),
            )
            .unwrap();
            let workspace = Workspace::load(root.path()).unwrap();
            let graph =
                TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
            let snapshot = Snapshot::new(&workspace, &graph, &root.path().join("cache")).unwrap();
            assert_eq!(snapshot.files.get().is_some(), matching, "{target}");
            assert!(snapshot.membership.lock().unwrap().is_none(), "{target}");
            // For keying, the listing comes with the inventory its first
            // refresh reuses, and a snapshot that lists nothing takes neither.
            let watched =
                Snapshot::watched(&workspace, &graph, &root.path().join("cache")).unwrap();
            assert_eq!(
                watched.listing.lock().unwrap().is_some(),
                matching,
                "{target}"
            );
            watched.settle();
            assert_eq!(watched.files.get().is_some(), matching, "{target}");
            let inventory = watched.membership.lock().unwrap().clone();
            assert_eq!(inventory.is_some(), matching, "{target}");
            if let Some(inventory) = inventory {
                assert_eq!(watched.files().unwrap(), &*inventory.files, "{target}");
                assert_eq!(
                    watched.files().unwrap(),
                    snapshot.files().unwrap(),
                    "{target}"
                );
            }
        }
    }

    /// A task's plan follows its declared inputs, as a key leaving some out
    /// resolves the same task with others.
    #[test]
    fn file_plans_follow_declared_inputs() {
        let (_root, workspace, graph) =
            literal_fixture(false, true, json!(["{workspaceRoot}/config.yaml"]));
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let mut task = graph.tasks["app:build"].clone();
        let plan = snapshot.file_plan(&workspace, &task).unwrap();
        assert_eq!(plan.paths, Some(BTreeSet::from(["config.yaml".to_owned()])));
        task.definition.inputs = Some(vec![json!("{workspaceRoot}/dir/**")]);
        assert_eq!(snapshot.file_plan(&workspace, &task).unwrap().paths, None);
    }

    /// Globs beneath fixed directories, as filesets of the tasks a run keys
    /// alone, are keyed from a listing and an inventory of those directories
    /// together, and key as they would from the workspace's listing: with
    /// exclusions, with or without a directory of their own, ignore files at
    /// any depth, an ignored directory beneath one, another task's declared
    /// outputs that its dependent selects and others do not, a directory
    /// that is not there, one that is a link, and `node_modules`.
    #[test]
    fn glob_inputs_beneath_directories_key_as_the_workspace_listing_does() {
        let inputs = json!([
            "{workspaceRoot}/dir/**/*",
            "!{workspaceRoot}/dir/deep/**",
            "{projectRoot}/sub/**/*.yaml",
            "!{projectRoot}/**/kept.yaml",
            "{workspaceRoot}/dist/**/*",
            "{workspaceRoot}/missing/**/*",
            "{workspaceRoot}/link-dir/**/*",
            "{workspaceRoot}/link-file/**",
            "{workspaceRoot}/node_modules/**/*",
            "{workspaceRoot}/config.yaml",
            {"fileset": "{projectRoot}/warm/**/*", "includeIgnored": true},
        ]);
        for (git, nxignore) in [(true, false), (true, true), (false, true), (false, false)] {
            for upstream in [false, true] {
                let (_root, workspace, mut graph) = literal_fixture(git, nxignore, inputs.clone());
                if upstream {
                    let build = graph.tasks.get_mut("app:build").unwrap();
                    build.dependencies.insert("app:gen".into());
                }
                let cache = workspace.root.join("cache");
                let task = &graph.tasks["app:build"];
                let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
                let inputs = |snapshot: &Snapshot| {
                    super::inputs(
                        snapshot,
                        &workspace,
                        task,
                        &prepared,
                        &BTreeMap::new(),
                        &AtomicBool::new(false),
                    )
                    .unwrap()
                };
                let label = format!("git {git}, nxignore {nxignore}, upstream {upstream}");
                let watched = Snapshot::watched(&workspace, &graph, &cache).unwrap();
                let scope = watched.scope.clone().expect(&label);
                assert_eq!(
                    scope.paths,
                    [
                        "config.yaml",
                        "dir",
                        "dist",
                        "link-dir",
                        "link-file",
                        "missing",
                        "node_modules",
                        "sub"
                    ]
                    .map(str::to_owned)
                    .into(),
                    "{label}"
                );
                let narrowed = inputs(&watched);
                // The narrowed listing is no initial candidate list.
                assert!(watched.files.get().is_none(), "{label}");
                let membership = watched.membership.lock().unwrap().clone().unwrap();
                assert!(membership.scope.is_some(), "{label}");
                assert!(
                    !membership
                        .directories
                        .contains(&workspace.root.join("warm"))
                );
                let listed = Snapshot::new(&workspace, &graph, &cache).unwrap();
                listed.files().unwrap();
                assert_eq!(narrowed, inputs(&listed), "{label}");
                let files = &narrowed["files"];
                assert!(files.get("dir/a.txt").is_some(), "{label}");
                assert!(files.get("sub/kept.yaml").is_none(), "{label}");
                for path in ["dir/deep/b.txt", "dir/skip/c.txt", "dir/x.log"] {
                    assert!(files.get(path).is_none(), "{path}, {label}");
                }
                assert_eq!(files.get("dist/out.txt").is_some(), upstream, "{label}");
            }
        }
    }

    /// Paths with every directory beneath them, listed with an inventory of
    /// those alone, list what the workspace's listing holds there.
    #[test]
    fn narrowed_listings_hold_what_the_workspace_listing_holds_there() {
        for (git, nxignore) in [(true, false), (true, true), (false, true), (false, false)] {
            let (root, workspace, graph) = literal_fixture(git, nxignore, json!([]));
            let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache"))
                .unwrap()
                .with_candidates(&["gone/file".into(), "dir/removed.txt".into()]);
            let all = snapshot.current_files(root.path()).unwrap();
            let sets = LITERAL_PATHS
                .iter()
                .map(|path| BTreeSet::from([path.to_string()]))
                .chain([
                    LITERAL_PATHS.iter().map(|path| path.to_string()).collect(),
                    BTreeSet::from(["gone".to_owned(), "dir".to_owned(), "missing".to_owned()]),
                    BTreeSet::from(["dir/deep".to_owned(), "sub".to_owned()]),
                ]);
            for paths in sets {
                let scope = Arc::new(Scope::new(paths.clone()));
                let mut membership = Membership::new(
                    root.path(),
                    &BTreeSet::new(),
                    snapshot.source_ignore(),
                    &snapshot.source_exclusions,
                    snapshot.cache_relative.as_deref(),
                    None,
                    Some(&scope),
                )
                .unwrap();
                membership.list(&snapshot.extra_candidates, true).unwrap();
                let expected: BTreeSet<String> = all
                    .iter()
                    .filter(|path| scope.holds(path))
                    .cloned()
                    .collect();
                let label = format!("{paths:?} with git {git}, nxignore {nxignore}");
                assert_eq!(*membership.files, expected, "{label}");
                // Nothing outside the scope is read, but the directories
                // leading to it.
                for directory in &membership.directories {
                    let relative = directory.strip_prefix(root.path()).unwrap();
                    assert!(
                        scope.reach(relative) != Reach::Outside,
                        "{relative:?}, {label}"
                    );
                }
            }
        }
    }

    /// A narrowed inventory lets a listing be reused while nothing it covers
    /// changed, whatever changes elsewhere, and a change to what it covers,
    /// to an ignore file above it, to Git's index or to a directory leading
    /// to it lists again what the workspace's listing would hold there. A
    /// caller reaching beyond it gets the whole workspace's listing.
    #[test]
    fn a_narrowed_inventory_guards_what_it_covers() {
        let inputs = json!([
            "{workspaceRoot}/dir/deep/**/*",
            "{workspaceRoot}/sub/**/*",
            "{workspaceRoot}/missing/inner/**/*",
        ]);
        let mut fixtures = Vec::new();
        for (git, nxignore) in [(true, false), (true, true), (false, true), (false, false)] {
            let (root, workspace, graph) = literal_fixture(git, nxignore, inputs.clone());
            let snapshot = snapshot(&workspace, &graph, true);
            fixtures.push((root, workspace, graph, snapshot, git, nxignore));
        }
        // Recent policies keep any listing from being reused.
        std::thread::sleep(Duration::from_millis(2100));
        for (root, workspace, graph, snapshot, git, nxignore) in fixtures {
            let task = &graph.tasks["app:build"];
            let scope = snapshot.scope.clone().unwrap();
            let label = format!("git {git}, nxignore {nxignore}");
            // The task's candidates, checked against a fresh listing of the
            // whole workspace there.
            let candidates = |step: &str| {
                let files = snapshot.task_candidates(&workspace, task).unwrap().files;
                let fresh = Snapshot::new(&workspace, &graph, &workspace.root.join("cache"))
                    .unwrap()
                    .current_files(root.path())
                    .unwrap();
                let held = |files: &BTreeSet<String>| -> BTreeSet<String> {
                    files
                        .iter()
                        .filter(|path| scope.holds(path))
                        .cloned()
                        .collect()
                };
                assert_eq!(held(&files), held(&fresh), "{step}, {label}");
                files
            };
            let write = |path: &str| {
                let path = root.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "new").unwrap();
            };
            candidates("first");
            let settled = candidates("settled");
            assert!(Arc::ptr_eq(&settled, &candidates("reused")), "{label}");
            for path in [
                "outside/new.txt",
                "top.txt",
                "dir/beside.txt",
                "dist/new.txt",
            ] {
                write(path);
            }
            std::fs::create_dir(root.path().join("dir/empty")).unwrap();
            assert!(Arc::ptr_eq(&settled, &candidates("outside")), "{label}");
            write("dir/deep/new.txt");
            assert!(candidates("within").contains("dir/deep/new.txt"));
            // Git lists a tracked file whatever ignores it, so an untracked
            // one shows the rules.
            std::fs::write(root.path().join("dir/.gitignore"), "deep/new.txt\n").unwrap();
            assert!(!candidates("new ignore file above").contains("dir/deep/new.txt"));
            std::fs::write(root.path().join("dir/.gitignore"), "deep/old.txt\n").unwrap();
            assert!(candidates("edited ignore file above").contains("dir/deep/new.txt"));
            write("missing/inner/x.txt");
            assert!(candidates("created beneath").contains("missing/inner/x.txt"));
            if git {
                let added = paths::git(root.path(), &["add", "--force", "sub/nested-ignored.yaml"]);
                assert!(added.unwrap().status.success());
                assert_eq!(
                    candidates("index").contains("sub/nested-ignored.yaml"),
                    !nxignore,
                    "{label}"
                );
            }
            #[cfg(unix)]
            {
                std::fs::rename(root.path().join("dir/deep"), root.path().join("dir/copy"))
                    .unwrap();
                std::os::unix::fs::symlink("copy", root.path().join("dir/deep")).unwrap();
                assert!(!candidates("linked").contains("dir/deep/new.txt"));
            }
            // Beyond the scope, the whole workspace's listing.
            let whole = snapshot.current_files(root.path()).unwrap();
            let fresh = Snapshot::new(&workspace, &graph, &workspace.root.join("cache"))
                .unwrap()
                .current_files(root.path())
                .unwrap();
            assert_eq!(whole, fresh, "{label}");
            assert!(whole.contains("top.txt"), "{label}");
            candidates("after the whole listing");
        }
    }

    /// A run lists the directories its tasks' globs start from together, the
    /// whole workspace when one of them has none, and nothing when each
    /// names paths.
    #[test]
    fn a_run_lists_the_directories_its_globs_start_from() {
        let (_root, workspace, mut graph) = literal_fixture(true, false, json!([]));
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let build = graph.tasks.remove("app:build").unwrap();
        graph.tasks.clear();
        let mut listing = |inputs: &[Value]| {
            graph.tasks.clear();
            for (index, inputs) in inputs.iter().enumerate() {
                let mut task = build.clone();
                task.id = format!("app:task{index}");
                task.definition.inputs = Some(inputs.as_array().unwrap().clone());
                graph.tasks.insert(task.id.clone(), task);
            }
            match snapshot.listing(&workspace, &graph) {
                Listing::Nothing => None,
                Listing::Within(scope) => Some(Some(scope.paths)),
                Listing::Whole => Some(None),
            }
        };
        let paths =
            |paths: &[&str]| Some(Some(paths.iter().map(|path| path.to_string()).collect()));
        assert_eq!(listing(&[json!(["{workspaceRoot}/a"])]), None);
        assert_eq!(
            listing(&[
                json!(["{workspaceRoot}/a/**/*", "!{workspaceRoot}/**/*.md"]),
                json!(["{workspaceRoot}/a/b/*.ts", "{workspaceRoot}/c"]),
                json!(["{workspaceRoot}/ab/**", "{workspaceRoot}/config.yaml"]),
            ]),
            paths(&["a", "ab", "c", "config.yaml"])
        );
        for whole in [
            json!(["{workspaceRoot}/**/*.ts"]),
            json!(["{workspaceRoot}/*/src/**"]),
            json!([{"json": "{workspaceRoot}/a/*.json"}]),
            json!([]),
        ] {
            let mut inputs = vec![json!(["{workspaceRoot}/a/**"]), whole.clone()];
            if whole == json!([]) {
                // The default inputs of a project at the workspace root.
                inputs[1] = json!(["default"]);
            }
            assert_eq!(listing(&inputs), Some(None), "{whole}");
        }
        let many: Vec<Value> = (0..=SCOPE_PATHS)
            .map(|index| json!([format!("{{workspaceRoot}}/d{index}/**")]))
            .collect();
        assert_eq!(listing(&many), Some(None));
        // A task recordings may key compares its declared selection with
        // what it read, from the whole listing; one not recorded needs none.
        for (settings, whole) in [
            (json!({}), true),
            (json!({"mode": "record"}), true),
            (json!({"mode": "unknown"}), true),
            (json!({"mode": "off"}), false),
        ] {
            let mut recorded = build.clone();
            recorded.definition.inputs = Some(vec![json!("{workspaceRoot}/a/**")]);
            recorded
                .definition
                .extra
                .insert("qk:ultracache".into(), settings.clone());
            graph.tasks.clear();
            graph.tasks.insert(recorded.id.clone(), recorded);
            assert_eq!(
                matches!(snapshot.listing(&workspace, &graph), Listing::Whole),
                whole,
                "{settings}"
            );
        }
    }

    /// A run that lists no files keeps the digests saved before, beside its
    /// own, until a run that lists them leaves out those of files gone.
    #[test]
    fn unlisted_runs_keep_saved_digests() {
        let (root, workspace, graph) =
            literal_fixture(false, true, json!(["{workspaceRoot}/config.yaml"]));
        settle(&root.path().join("config.yaml"));
        let path = digests_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            json!({"version": 1, "entries": {"elsewhere.txt": [[1, 2], "digest"]}}).to_string(),
        )
        .unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &workspace.root.join("cache")).unwrap();
        let task = &graph.tasks["app:build"];
        let prepared = qk_executor::prepare(&workspace, task, &BTreeMap::new()).unwrap();
        inputs(
            &snapshot,
            &workspace,
            task,
            &prepared,
            &BTreeMap::new(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(snapshot.files.get().is_none());
        snapshot.save_digests(root.path());
        let saved = load_digests(root.path());
        assert!(saved.contains_key("config.yaml") && saved.contains_key("elsewhere.txt"));
        snapshot.files().unwrap();
        snapshot.digests_changed.store(true, Ordering::Relaxed);
        snapshot.save_digests(root.path());
        let saved = load_digests(root.path());
        assert!(saved.contains_key("config.yaml") && !saved.contains_key("elsewhere.txt"));
    }
}
