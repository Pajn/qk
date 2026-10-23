//! Path sets: what a task did to the workspace when it last ran, kept so a
//! later run can key it by how those paths are now. See
//! [`crate::ultracache`] for the target field that opts a task in.
//!
//! A path set holds the files the task read, the directories it listed and
//! the paths it looked up, found or missing. Its key, the strong key, hashes
//! each file's content, each directory's listing and each path's existence,
//! on top of the weak key: the task's key without its declared file inputs.
//! A new file in a listed directory changes the listing; a file created
//! where the task found nothing changes its existence.
//!
//! Paths outside the workspace, `node_modules`, `.git`, qk's own state, the
//! task's outputs and warm paths, files it wrote before reading them and its
//! `ignoreReads` are left out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use qk_input_analysis::{Access, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::glob::Pattern;
use crate::hash::{Snapshot, key};
use crate::paths::Outputs;

/// How many path sets are kept for one weak key, the newest first: a task
/// that reads differently from run to run, such as by platform or by what it
/// finds, keeps a set for each way.
const KEPT: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathSet {
    /// Files whose content the task read.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub reads: BTreeSet<String>,
    /// Directories it listed; `.` is the workspace root.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub listings: BTreeSet<String>,
    /// Paths it found, without reading them. A file among them is keyed by
    /// its content all the same: a tool that compares timestamps, as
    /// `tsc --build` does, decides by a file it never reads.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub present: BTreeSet<String>,
    /// Paths it looked for and did not find.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub absent: BTreeSet<String>,
    /// Paths it wrote in the directories it listed, left out of their
    /// listings: they are there after it runs, and need not be before.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub written: BTreeSet<String>,
}

/// What a path set leaves out.
pub struct Exclusions<'a> {
    /// The workspace root, as given and with its links resolved.
    pub roots: [&'a Path; 2],
    /// The task's outputs and warm paths.
    pub outputs: &'a [&'a Outputs],
    /// `ignoreReads`.
    pub ignore: Vec<Pattern>,
    /// qk's own directories inside the workspace, relative to it.
    pub state: Vec<String>,
}

impl Exclusions<'_> {
    /// `path`, absolute, relative to the workspace: `None` outside it.
    fn relative(&self, path: &str) -> Option<String> {
        let path = qk_input_analysis::strace::normalize(Path::new(path));
        let relative = self
            .roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())?;
        let relative = relative.to_str()?.replace('\\', "/");
        Some(if relative.is_empty() {
            ".".into()
        } else {
            relative
        })
    }

    fn excluded(&self, path: &str) -> bool {
        let mut parts = path.split('/');
        parts.clone().next() == Some(".git")
            || parts.any(|part| part == "node_modules")
            || self.state.iter().any(|state| {
                path == state
                    || path
                        .strip_prefix(state.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            || self.outputs.iter().any(|outputs| outputs.matches(path))
            || self.ignore.iter().any(|pattern| pattern.is_match(path))
    }
}

impl PathSet {
    /// The path set of a task's recorded accesses, in the order it made
    /// them. `root` is the workspace root, to tell a directory opened for
    /// listing from a file opened for reading.
    pub fn from_accesses(accesses: &[Access], exclusions: &Exclusions, root: &Path) -> Self {
        let mut set = Self::default();
        let mut written = BTreeSet::new();
        for access in accesses {
            let Some(path) = exclusions.relative(&access.path) else {
                continue;
            };
            if access.operation.writes() {
                written.insert(path);
                continue;
            }
            // What it wrote itself is no input.
            if written.contains(&path) || exclusions.excluded(&path) {
                continue;
            }
            let found = matches!(access.result.as_str(), "success" | "unknown");
            let missing = matches!(access.result.as_str(), "missing" | "ENOTDIR");
            match access.operation {
                Operation::OpenRead | Operation::ReadData | Operation::Execute if found => {
                    // A directory is opened to be listed, which its listing
                    // records.
                    if !root.join(&path).is_dir() {
                        set.reads.insert(path);
                    }
                }
                Operation::ListDirectory | Operation::ReadDirectory if found => {
                    set.listings.insert(path);
                }
                Operation::Metadata | Operation::ReadLink if found => {
                    set.present.insert(path);
                }
                _ if missing => {
                    set.absent.insert(path);
                }
                _ => {}
            }
        }
        set.present.retain(|path| !set.reads.contains(path));
        set.absent
            .retain(|path| !set.reads.contains(path) && !set.present.contains(path));
        set.written = written
            .into_iter()
            .filter(|path| set.listings.contains(parent(path)))
            .collect();
        set
    }

    /// Whether every path it names lies within the workspace: relative, and
    /// never through `..`.
    pub fn is_relative(&self) -> bool {
        [
            &self.reads,
            &self.listings,
            &self.present,
            &self.absent,
            &self.written,
        ]
        .into_iter()
        .flatten()
        .all(|path| {
            !path.is_empty()
                && Path::new(path).components().all(|component| {
                    matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
        })
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.listings.is_empty()
            && self.present.is_empty()
            && self.absent.is_empty()
    }

    /// A path the set keys that changed after `started`, when the task ran:
    /// keyed by how it is now, a recording would key the task by what was
    /// left there rather than by what the task saw.
    ///
    /// A file read or found changed if it was written. A listing, which keys
    /// its entries' names and kinds, changed if an entry was added, removed
    /// or renamed. In a directory `before` holds, that is told exactly, by
    /// comparing its entries with those before the task ran, leaving out
    /// what the task wrote and what listings leave out. Elsewhere it moves
    /// the directory's own modification time; what is written inside an
    /// entry does not change it, though it moves the entry's change time.
    /// Where the task wrote into the directory, or its own outputs are
    /// there, its time moves with the task's own writes, so there an entry
    /// other than those changed it: a file whose change time moved, by being
    /// added, moved in or rewritten, or a subdirectory created after
    /// `started`, by its birth time, which a file system without one cannot
    /// tell. A removal there, or a subdirectory moved in, cannot be told
    /// from the task's own writes.
    pub(crate) fn changed_since(
        &self,
        root: &Path,
        started: SystemTime,
        unlisted: &Unlisted,
        before: &Before,
    ) -> Option<Changed<'_>> {
        let file = self.reads.iter().chain(&self.present).find(|path| {
            std::fs::symlink_metadata(root.join(path))
                .is_ok_and(|metadata| !metadata.is_dir() && changed(&metadata) >= started)
        });
        if let Some(path) = file {
            return Some(Changed { path, how: None });
        }
        self.listings.iter().find_map(|directory| {
            self.listing_changed(root, directory, started, unlisted, before)
                .map(|how| Changed {
                    path: directory,
                    how,
                })
        })
    }

    /// Whether `directory`'s listing changed while the task ran, and if so,
    /// what changed in it, where that can be told.
    fn listing_changed(
        &self,
        root: &Path,
        directory: &str,
        started: SystemTime,
        unlisted: &Unlisted,
        before: &Before,
    ) -> Option<Option<String>> {
        // A directory listings leave out, such as another target's output,
        // has nothing in it to key.
        if directory != "." && unlisted.matches(directory) {
            return None;
        }
        let absolute = if directory == "." {
            root.to_path_buf()
        } else {
            root.join(directory)
        };
        let skipped = |path: &str| self.written.contains(path) || unlisted.matches(path);
        if let Some(earlier) = before.0.get(directory) {
            let earlier: BTreeSet<_> = earlier
                .iter()
                .filter(|(name, _)| !self.written.contains(&child(directory, name)))
                .cloned()
                .collect();
            let Some(now) = entries(root, directory, skipped) else {
                return Some(Some("it could no longer be read".to_owned()));
            };
            if let Some((name, _)) = now.difference(&earlier).next() {
                return Some(Some(format!("{name} added")));
            }
            if let Some((name, _)) = earlier.difference(&now).next() {
                return Some(Some(format!("{name} removed")));
            }
            return None;
        }
        let Ok(entries) = std::fs::read_dir(&absolute) else {
            return Some(Some("it was removed or could no longer be read".to_owned()));
        };
        // Each entry, whether listings leave it out, and whether the task
        // wrote it or it is among the task's own outputs.
        let entries: Vec<(std::fs::DirEntry, bool, bool)> = entries
            .flatten()
            .map(|entry| {
                let path = child(directory, &entry.file_name().to_string_lossy());
                let own = self.written.contains(&path) || unlisted.own(&path);
                (entry, skipped(&path), own)
            })
            .collect();
        // A file's change time moves when it is moved in, and with a rewrite,
        // which counts too. A subdirectory's moves with what is written
        // inside it, so it counts only once it was created.
        let new = |entry: &std::fs::DirEntry| {
            std::fs::symlink_metadata(entry.path()).is_ok_and(|metadata| {
                if metadata.is_dir() {
                    metadata.created().is_ok_and(|born| born >= started)
                } else {
                    changed(&metadata) >= started
                }
            })
        };
        let unskipped = || entries.iter().filter(|(_, skipped, _)| !skipped);
        let name = |entry: &std::fs::DirEntry| entry.file_name().to_string_lossy().into_owned();
        let own = self.written.iter().any(|path| parent(path) == directory)
            || entries.iter().any(|(_, _, own)| *own);
        if !own {
            let moved = std::fs::symlink_metadata(&absolute)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified >= started);
            return moved.then(|| {
                Some(unskipped().find(|(entry, _, _)| new(entry)).map_or_else(
                    || "an entry removed or renamed".to_owned(),
                    |(entry, _, _)| format!("{} added or changed", name(entry)),
                ))
            });
        }
        unskipped()
            .find(|(entry, _, _)| new(entry))
            .map(|(entry, _, _)| Some(format!("{} added or changed", name(entry))))
    }

    pub fn digest(&self) -> Result<String> {
        key(&serde_json::to_value(self)?)
    }

    /// How the set's paths are now: each read file's content, each listed
    /// directory's entries and each looked-up path's kind. `outputs` are the
    /// task's own, left out of listings.
    pub(crate) fn values(
        &self,
        snapshot: &Snapshot,
        root: &Path,
        unlisted: &Unlisted,
    ) -> Result<Value> {
        // A file by its content, anything else by what it is.
        let value = |path: &str| match kind(&root.join(path)) {
            "file" | "symlink" => snapshot
                .recorded_value(root, path)
                .unwrap_or_else(|_| json!({"unreadable": true})),
            other => json!(other),
        };
        let reads: Map<String, Value> = self
            .reads
            .iter()
            .map(|path| (path.clone(), value(path)))
            .collect();
        let mut listings = Map::new();
        for directory in &self.listings {
            listings.insert(directory.clone(), self.listing(root, directory, unlisted)?);
        }
        let existence: Map<String, Value> = self
            .present
            .iter()
            .chain(&self.absent)
            .map(|path| (path.clone(), value(path)))
            .collect();
        Ok(json!({"reads": reads, "listings": listings, "existence": existence}))
    }

    /// A directory's entries by name and kind, without what the task writes.
    fn listing(&self, root: &Path, directory: &str, unlisted: &Unlisted) -> Result<Value> {
        let absolute = if directory == "." {
            root.to_path_buf()
        } else {
            root.join(directory)
        };
        let entries = match std::fs::read_dir(&absolute) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!("absent"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                return Ok(json!(kind(&absolute)));
            }
            Err(error) => return Err(error.into()),
        };
        let mut listed = Map::new();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = if directory == "." {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            if self.written.contains(&path) || unlisted.matches(&path) {
                continue;
            }
            listed.insert(name, json!(kind(&entry.path())));
        }
        Ok(Value::Object(listed))
    }
}

/// The directory a workspace-relative path is in, `.` for the root.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(parent, _)| parent)
}

/// The path of `name` in `directory`, `.` being the workspace root.
fn child(directory: &str, name: &str) -> String {
    if directory == "." {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

/// The names and kinds of `directory`'s entries but those `skipped`; none
/// when it, or any of its entries or their kinds, cannot be read.
fn entries(
    root: &Path,
    directory: &str,
    skipped: impl Fn(&str) -> bool,
) -> Option<BTreeSet<(String, &'static str)>> {
    let absolute = if directory == "." {
        root.to_path_buf()
    } else {
        root.join(directory)
    };
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(&absolute).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !skipped(&child(directory, &name)) {
            found.insert((name, file_kind(entry.file_type().ok()?)));
        }
    }
    Some(found)
}

/// What is at `path`, without following a link.
fn kind(path: &Path) -> &'static str {
    std::fs::symlink_metadata(path).map_or("absent", |metadata| file_kind(metadata.file_type()))
}

fn file_kind(file_type: std::fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

/// A project's root as output paths name it: without a leading `./` or a
/// trailing `/`, and empty for the workspace's root, which `""` and `"."`
/// both name.
fn relative_root(root: &str) -> &str {
    let mut root = root.trim_end_matches('/');
    while let Some(rest) = root.strip_prefix("./") {
        root = rest;
    }
    if root == "." { "" } else { root }
}

/// A path whose recording changed while the task ran, and what changed in
/// it, where that can be told.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Changed<'a> {
    pub path: &'a str,
    pub how: Option<String>,
}

impl std::fmt::Display for Changed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.how {
            Some(how) => write!(f, "{} changed while it ran ({how})", self.path),
            None => write!(f, "{} changed while it ran", self.path),
        }
    }
}

/// The declared outputs of every target in the workspace, and of each of
/// their configurations, which can move an output with its options, by the
/// workspace's configuration rather than what a run schedules. Indexed by
/// their anchors, so a path is matched only against the outputs that can
/// hold it.
#[derive(Default)]
pub(crate) struct WorkspaceOutputs {
    outputs: Vec<(TargetOf, Outputs)>,
    by_anchor: BTreeMap<String, Vec<usize>>,
    /// The directories between each output's project root and its anchor,
    /// such as `.cache` for `.cache/build`, which exist to hold it.
    folders: Vec<Vec<String>>,
    by_folder: BTreeMap<String, Vec<usize>>,
}

impl WorkspaceOutputs {
    /// `roots` holds each project's root.
    pub(crate) fn new(outputs: Vec<(TargetOf, Outputs)>, roots: &BTreeMap<String, String>) -> Self {
        let mut by_anchor: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_folder: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut folders = Vec::new();
        // No project's root, nor a directory holding one, is a folder an
        // output owns.
        let projects: BTreeSet<&str> = roots
            .values()
            .map(|root| relative_root(root))
            .filter(|root| !root.is_empty())
            .flat_map(|root| {
                std::iter::once(root).chain(root.match_indices('/').map(|(at, _)| &root[..at]))
            })
            .collect();
        for (index, ((project, _, _), declared)) in outputs.iter().enumerate() {
            let root = roots.get(project).map_or("", |root| relative_root(root));
            let mut held = Vec::new();
            for anchor in declared.anchors() {
                by_anchor.entry(anchor.to_owned()).or_default().push(index);
                let rest = if root.is_empty() {
                    Some(anchor)
                } else {
                    anchor
                        .strip_prefix(root)
                        .and_then(|rest| rest.strip_prefix('/'))
                };
                let Some(rest) = rest else {
                    continue;
                };
                // Each directory below the root that holds the anchor.
                for (at, _) in rest.match_indices('/') {
                    let folder = if root.is_empty() {
                        rest[..at].to_owned()
                    } else {
                        format!("{root}/{}", &rest[..at])
                    };
                    if projects.contains(folder.as_str()) {
                        continue;
                    }
                    by_folder.entry(folder.clone()).or_default().push(index);
                    held.push(folder);
                }
            }
            folders.push(held);
        }
        Self {
            outputs,
            by_anchor,
            folders,
            by_folder,
        }
    }

    /// The outputs whose anchor is `path` or a directory holding it.
    fn holding<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a (TargetOf, Outputs)> {
        std::iter::once(path)
            .chain(path.match_indices('/').map(|(at, _)| &path[..at]))
            .filter_map(|anchor| self.by_anchor.get(anchor))
            .flatten()
            .map(|&index| &self.outputs[index])
    }
}

/// A target by project, target and configuration, whose options can move
/// its outputs.
pub(crate) type TargetOf = (String, String, Option<String>);

/// Other targets' outputs a recording's listings leave out: every target's
/// in the workspace but the task's own and those it depends on, whose
/// outputs are its inputs.
pub(crate) struct Others<'a> {
    pub outputs: &'a WorkspaceOutputs,
    /// The task's target and its upstream.
    pub upstream: BTreeSet<TargetOf>,
}

impl Others<'_> {
    /// Whether `path` is another target's output, and none of the
    /// upstream's: an output both declare is the task's input.
    ///
    /// A directory that only holds another target's outputs, between its
    /// project's root and the outputs, counts as one, as `.cache` does for
    /// `.cache/build`: it comes and goes with them.
    pub(crate) fn matches(&self, path: &str) -> bool {
        let mut other = false;
        let holding = self
            .outputs
            .holding(path)
            .filter(|(_, outputs)| outputs.matches(path))
            .map(|(target, _)| target);
        let folders = self
            .outputs
            .by_folder
            .get(path)
            .into_iter()
            .flatten()
            .map(|&index| &self.outputs.outputs[index].0);
        for target in holding.chain(folders) {
            if self.upstream.contains(target) {
                return false;
            }
            other = true;
        }
        other
    }
}

/// What a recording's listings leave out of a directory's entries: what is
/// there after a task runs whether or not it read anything, rather than what
/// it read. The task's own outputs, the outputs of other targets, such as a
/// build's output beside a lint's sources or another project's coverage, but
/// not those of the targets it depends on, and what its `ignoreReads` names.
/// Only listings leave them out: a file among them that the task read is
/// still keyed by its content.
pub(crate) struct Unlisted<'a> {
    pub own: Vec<&'a Outputs>,
    pub others: Option<Others<'a>>,
    pub ignore: &'a [Pattern],
}

impl Unlisted<'_> {
    fn matches(&self, path: &str) -> bool {
        self.own(path)
            || self
                .others
                .as_ref()
                .is_some_and(|others| others.matches(path))
            || self.ignore.iter().any(|pattern| pattern.is_match(path))
    }

    fn own(&self, path: &str) -> bool {
        self.own.iter().any(|outputs| outputs.matches(path))
    }

    /// Where other targets' outputs and ignored paths come and go while the
    /// task runs: the directories holding those outputs, up to their
    /// project's root, and their anchors, where an output a glob declares
    /// comes and goes, and the root of the task's project, where a tool's
    /// `ignoreReads` state usually sits.
    pub(crate) fn directories(&self, project_root: &str) -> BTreeSet<String> {
        let project_root = match relative_root(project_root) {
            "" => ".",
            root => root,
        };
        let mut directories = BTreeSet::from([project_root.to_owned()]);
        if let Some(others) = &self.others {
            let all = &others.outputs;
            for (index, (target, outputs)) in all.outputs.iter().enumerate() {
                if others.upstream.contains(target) {
                    continue;
                }
                for anchor in outputs.anchors() {
                    directories.insert(anchor.to_owned());
                    directories.insert(parent(anchor).to_owned());
                }
                // Each folder, and the directory holding it, which can be
                // another project's root.
                for folder in &all.folders[index] {
                    directories.insert(folder.clone());
                    directories.insert(parent(folder).to_owned());
                }
            }
        }
        directories
    }
}

/// The entries of directories before a task ran, but those its listings
/// leave out, so a listing whose directory gained or lost something else as
/// it ran is told apart from one where only those came and went.
#[derive(Default)]
pub(crate) struct Before(BTreeMap<String, BTreeSet<(String, &'static str)>>);

impl Before {
    pub(crate) fn take(root: &Path, directories: BTreeSet<String>, unlisted: &Unlisted) -> Self {
        Self(
            directories
                .into_iter()
                .filter_map(|directory| {
                    let entries = entries(root, &directory, |path| unlisted.matches(path))?;
                    Some((directory, entries))
                })
                .collect(),
        )
    }
}

/// The path sets kept for weak keys, in `pathsets/` beside the entries.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    weak: String,
    sets: Vec<PathSet>,
}

fn location(cache: &Path, weak: &str) -> PathBuf {
    cache.join("pathsets").join(format!("{weak}.json"))
}

/// The path sets kept for `weak`, newest first; none when there are none or
/// they cannot be read.
pub(crate) fn load(cache: &Path, weak: &str) -> Vec<PathSet> {
    std::fs::read(location(cache, weak))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
        .filter(|stored| stored.version == 1 && stored.weak == weak)
        .map(|stored| stored.sets)
        .unwrap_or_default()
}

/// Marks `weak`'s path sets as used, so eviction keeps them as it keeps the
/// entries they lead to.
pub(crate) fn touch(cache: &Path, weak: &str) {
    if let Ok(file) = std::fs::File::options()
        .append(true)
        .open(location(cache, weak))
    {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Keeps `set` as the newest for `weak`, and at most [`KEPT`] in all.
pub(crate) fn save(cache: &Path, weak: &str, set: PathSet) -> Result<()> {
    let mut sets = load(cache, weak);
    sets.retain(|kept| kept != &set);
    sets.insert(0, set);
    keep(cache, weak, sets)
}

/// The record of `weak`'s path sets as the local cache keeps them, for the
/// remote store.
pub(crate) fn record(cache: &Path, weak: &str) -> Option<Vec<u8>> {
    std::fs::read(location(cache, weak)).ok()
}

/// Adds the path sets in a record the remote store held for `weak` behind
/// those kept locally, returning the ones that are new here. A record for
/// another weak key or another version adds nothing, and neither does a set
/// naming a path outside the workspace.
pub(crate) fn merge(cache: &Path, weak: &str, record: &[u8]) -> Result<Vec<PathSet>> {
    let Some(fetched) = serde_json::from_slice::<Stored>(record)
        .ok()
        .filter(|stored| stored.version == 1 && stored.weak == weak)
    else {
        return Ok(Vec::new());
    };
    let mut sets = load(cache, weak);
    let mut added = Vec::new();
    for set in fetched.sets {
        if sets.len() + added.len() < KEPT
            && set.is_relative()
            && !set.is_empty()
            && !sets.contains(&set)
            && !added.contains(&set)
        {
            added.push(set);
        }
    }
    if !added.is_empty() {
        sets.extend(added.iter().cloned());
        keep(cache, weak, sets)?;
    }
    Ok(added)
}

fn keep(cache: &Path, weak: &str, mut sets: Vec<PathSet>) -> Result<()> {
    sets.truncate(KEPT);
    std::fs::create_dir_all(cache.join("pathsets"))?;
    crate::record::publish(
        cache,
        &location(cache, weak),
        &Stored {
            version: 1,
            weak: weak.to_owned(),
            sets,
        },
    )
}

/// The key of a result for `set` under `weak`, from how its paths are now.
pub(crate) fn strong_key(
    weak: &str,
    set: &PathSet,
    snapshot: &Snapshot,
    root: &Path,
    unlisted: &Unlisted,
) -> Result<String> {
    key(&json!({
        "schema": "qk-recorded-v1",
        "weak": weak,
        "set": set.digest()?,
        "values": set.values(snapshot, root, unlisted)?,
    }))
}

/// Now, as the file system in `directory` stamps a file it writes. The kernel
/// stamps files from a clock coarser than the system's, so a file written
/// just after a [`SystemTime::now`] can bear an earlier time; this one is
/// never later than the stamp of a file written after it.
pub(crate) fn now(directory: &Path) -> SystemTime {
    tempfile::NamedTempFile::new_in(directory)
        .and_then(|file| file.as_file().metadata())
        .map(|metadata| changed(&metadata))
        .unwrap_or_else(|_| SystemTime::now())
}

/// When a path's content or metadata last changed: its ctime where there is
/// one, which a tool resetting the modification time does not hide.
fn changed(metadata: &std::fs::Metadata) -> SystemTime {
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ctime = u64::try_from(metadata.ctime()).ok().map(|seconds| {
            SystemTime::UNIX_EPOCH
                + std::time::Duration::new(
                    seconds,
                    u32::try_from(metadata.ctime_nsec()).unwrap_or(0),
                )
        });
        if let Some(ctime) = ctime {
            return modified.max(ctime);
        }
    }
    modified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing left out of listings.
    const NOTHING: Unlisted<'static> = Unlisted {
        own: Vec::new(),
        others: None,
        ignore: &[],
    };
    use qk_input_analysis::Category;

    fn access(path: &str, operation: Operation, result: &str) -> Access {
        Access {
            process: 1,
            path: path.into(),
            resolved_path: None,
            keyed_path: None,
            package: None,
            operation,
            result: result.into(),
            category: Category::Uncovered,
        }
    }

    #[test]
    fn keeps_what_the_task_read_listed_and_looked_up() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for directory in ["src", "icons", "dist"] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        let path = |relative: &str| root.join(relative).to_str().unwrap().to_owned();
        let outputs = Outputs::from_paths(&["dist".into()]).unwrap();
        let exclusions = Exclusions {
            roots: [&root, &root],
            outputs: &[&outputs],
            ignore: vec![Pattern::new("src/*.log", false).unwrap()],
            state: vec![".qk".into()],
        };
        let accesses = [
            access(&path("src/a.ts"), Operation::OpenRead, "success"),
            access(&path("src"), Operation::OpenRead, "success"),
            access(&path("icons"), Operation::ListDirectory, "success"),
            access(&path("config.local"), Operation::Metadata, "missing"),
            access(&path("src/b.ts"), Operation::Metadata, "success"),
            // A file it writes and then reads is its own.
            access(&path("icons/sprite.svg"), Operation::Write, "success"),
            access(&path("icons/sprite.svg"), Operation::OpenRead, "success"),
            // Left out wholesale.
            access("/usr/lib/libc.so", Operation::OpenRead, "success"),
            access(
                &path("node_modules/x/index.js"),
                Operation::OpenRead,
                "success",
            ),
            access(&path(".git/HEAD"), Operation::OpenRead, "success"),
            access(&path(".qk/state.json"), Operation::OpenRead, "success"),
            access(&path("dist/out.js"), Operation::OpenRead, "success"),
            access(&path("src/debug.log"), Operation::OpenRead, "success"),
            // A file read is not also only present.
            access(&path("src/a.ts"), Operation::Metadata, "success"),
        ];
        let set = PathSet::from_accesses(&accesses, &exclusions, &root);
        assert_eq!(
            set,
            PathSet {
                reads: ["src/a.ts".into()].into(),
                listings: ["icons".into()].into(),
                present: ["src/b.ts".into()].into(),
                absent: ["config.local".into()].into(),
                written: ["icons/sprite.svg".into()].into(),
            }
        );
    }

    #[test]
    fn values_change_with_contents_listings_and_existence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("icons")).unwrap();
        std::fs::write(root.join("a.txt"), "one").unwrap();
        std::fs::write(root.join("icons/a.svg"), "").unwrap();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.join("project.json"),
            r#"{"name":"app","targets":{"build":{"command":"true"}}}"#,
        )
        .unwrap();
        let workspace = qk_config::Workspace::load(&root).unwrap();
        let graph = qk_taskgraph::TaskGraph::build(
            &workspace,
            &[qk_taskgraph::Request::parse("app:build").unwrap()],
        )
        .unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, &root.join("cache")).unwrap();
        std::fs::write(root.join("checked.txt"), "one").unwrap();
        let set = PathSet {
            reads: ["a.txt".into()].into(),
            listings: ["icons".into()].into(),
            present: ["checked.txt".into()].into(),
            absent: ["optional.txt".into()].into(),
            written: ["icons/sprite.svg".into()].into(),
        };
        let outputs = Outputs::from_paths(&["icons/out".into()]).unwrap();
        let unlisted = Unlisted {
            own: vec![&outputs],
            others: None,
            ignore: &[],
        };
        let strong = || strong_key("weak", &set, &snapshot, &root, &unlisted).unwrap();
        let first = strong();
        // What the task writes in a listed directory, or its outputs there,
        // and files it never read change nothing.
        std::fs::write(root.join("icons/sprite.svg"), "").unwrap();
        std::fs::create_dir(root.join("icons/out")).unwrap();
        std::fs::write(root.join("b.txt"), "unread").unwrap();
        assert_eq!(strong(), first);
        for change in [
            |root: &Path| std::fs::write(root.join("icons/b.svg"), "").unwrap(),
            |root: &Path| std::fs::write(root.join("optional.txt"), "").unwrap(),
            |root: &Path| std::fs::write(root.join("a.txt"), "two").unwrap(),
            // A file it only looked at, as by its timestamp.
            |root: &Path| std::fs::write(root.join("checked.txt"), "two").unwrap(),
        ] {
            let before = strong();
            change(&root);
            assert_ne!(strong(), before);
        }
    }

    #[test]
    fn keeps_the_newest_sets_first() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("tmp")).unwrap();
        let set = |read: &str| PathSet {
            reads: [read.to_owned()].into(),
            ..PathSet::default()
        };
        for index in 0..10 {
            save(temp.path(), "weak", set(&format!("{index}"))).unwrap();
        }
        save(temp.path(), "weak", set("5")).unwrap();
        let kept: Vec<String> = load(temp.path(), "weak")
            .into_iter()
            .map(|set| set.reads.into_iter().next().unwrap())
            .collect();
        assert_eq!(kept, ["5", "9", "8", "7", "6", "4", "3", "2"]);
        assert!(load(temp.path(), "other").is_empty());
    }

    #[test]
    fn merges_the_sets_another_machine_recorded_behind_its_own() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("tmp")).unwrap();
        let set = |read: &str| PathSet {
            reads: [read.to_owned()].into(),
            ..PathSet::default()
        };
        save(temp.path(), "weak", set("a")).unwrap();
        let record = |weak: &str, sets: Vec<PathSet>| {
            serde_json::to_vec(&Stored {
                version: 1,
                weak: weak.into(),
                sets,
            })
            .unwrap()
        };
        let fetched = record(
            "weak",
            vec![set("b"), set("a"), set("../escape"), set("/abs")],
        );
        assert_eq!(merge(temp.path(), "weak", &fetched).unwrap(), [set("b")]);
        assert_eq!(load(temp.path(), "weak"), [set("a"), set("b")]);
        assert!(merge(temp.path(), "weak", &fetched).unwrap().is_empty());
        assert!(
            merge(temp.path(), "weak", &record("other", vec![set("c")]))
                .unwrap()
                .is_empty()
        );
        let many = (0..10).map(|index| set(&format!("{index}"))).collect();
        assert_eq!(
            merge(temp.path(), "weak", &record("weak", many))
                .unwrap()
                .len(),
            6
        );
        assert_eq!(load(temp.path(), "weak").len(), KEPT);
    }

    #[test]
    fn finds_what_changed_while_the_task_ran() {
        let temp = tempfile::tempdir().unwrap();
        for path in ["read.txt", "found.txt", "listed.txt", "before.txt"] {
            std::fs::write(temp.path().join(path), "before").unwrap();
        }
        for directory in ["listed", "written"] {
            std::fs::create_dir(temp.path().join(directory)).unwrap();
            std::fs::write(temp.path().join(directory).join("kept.txt"), "").unwrap();
        }
        std::fs::create_dir(temp.path().join("listed/sub")).unwrap();
        let set = PathSet {
            reads: ["read.txt".to_owned(), "gone.txt".to_owned()].into(),
            present: ["found.txt".to_owned(), ".".to_owned()].into(),
            listings: ["listed".to_owned(), "written".to_owned()].into(),
            written: ["written/new.txt".to_owned()].into(),
            ..PathSet::default()
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        let started = now(temp.path());
        let changed = |set: &PathSet| {
            set.changed_since(temp.path(), started, &NOTHING, &Before::default())
                .map(|changed| changed.path.to_owned())
        };
        assert_eq!(changed(&set), None);
        // What the task wrote, and content in a listed directory, keys nothing.
        std::fs::write(temp.path().join("written/new.txt"), "").unwrap();
        std::fs::write(temp.path().join("listed/kept.txt"), "after").unwrap();
        std::fs::write(temp.path().join("listed.txt"), "after").unwrap();
        // Content in a listed directory is not part of the listing.
        let unlisted = PathSet {
            listings: ["written".to_owned()].into(),
            ..set.clone()
        };
        assert_eq!(changed(&unlisted), None);
        std::fs::write(temp.path().join("found.txt"), "after").unwrap();
        assert_eq!(changed(&set), Some("found.txt".into()));
        let listings = PathSet {
            reads: [].into(),
            present: [].into(),
            ..set.clone()
        };
        // Another task writing inside an entry, or rewriting one in place,
        // leaves the listing as it was.
        std::fs::write(temp.path().join("listed/sub/made.txt"), "").unwrap();
        assert_eq!(changed(&listings), None);
        // An entry another process adds where the task wrote too.
        std::fs::write(temp.path().join("written/other.txt"), "").unwrap();
        if std::fs::metadata(temp.path().join("written/other.txt"))
            .and_then(|metadata| metadata.created())
            .is_ok()
        {
            assert_eq!(changed(&listings), Some("written".into()));
        }
        std::fs::remove_file(temp.path().join("written/other.txt")).unwrap();
        // A file that was there before, unchanged, moved in where the task
        // wrote too: a move updates its change time, which Unix keeps.
        #[cfg(unix)]
        {
            std::fs::rename(
                temp.path().join("before.txt"),
                temp.path().join("written/moved.txt"),
            )
            .unwrap();
            assert_eq!(changed(&listings), Some("written".into()));
            std::fs::rename(
                temp.path().join("written/moved.txt"),
                temp.path().join("before.txt"),
            )
            .unwrap();
        }
        // An entry added where the task wrote nothing.
        std::fs::write(temp.path().join("listed/new.txt"), "").unwrap();
        assert_eq!(changed(&listings), Some("listed".into()));
        // A removal, where the task wrote nothing.
        let removed = tempfile::tempdir().unwrap();
        std::fs::write(removed.path().join("gone.txt"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Taken elsewhere, as qk takes it in its cache: taking it changes the
        // directory it is taken in.
        let started = now(temp.path());
        let root = PathSet {
            listings: [".".to_owned()].into(),
            ..PathSet::default()
        };
        assert_eq!(
            root.changed_since(removed.path(), started, &NOTHING, &Before::default())
                .map(|changed| changed.path),
            None
        );
        std::fs::remove_file(removed.path().join("gone.txt")).unwrap();
        assert_eq!(
            root.changed_since(removed.path(), started, &NOTHING, &Before::default())
                .map(|changed| changed.path),
            Some(".")
        );
    }

    #[test]
    fn listings_leave_out_other_targets_outputs_and_ignored_reads() {
        let temp = tempfile::tempdir().unwrap();
        let root = &temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(root.join("app/src/main.ts"), "").unwrap();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.join("project.json"),
            r#"{"name":"app","targets":{"build":{"command":"true"}}}"#,
        )
        .unwrap();
        let workspace = qk_config::Workspace::load(root).unwrap();
        let graph = qk_taskgraph::TaskGraph::build(
            &workspace,
            &[qk_taskgraph::Request::parse("app:build").unwrap()],
        )
        .unwrap();
        let set = PathSet {
            listings: ["app".to_owned()].into(),
            ..PathSet::default()
        };
        std::fs::write(root.join("app/kept.txt"), "").unwrap();
        std::fs::create_dir(root.join("app/out")).unwrap();
        let roots = [("app".to_owned(), "app".to_owned())].into();
        let outputs = WorkspaceOutputs::new(
            vec![
                (
                    ("app".into(), "tsc".into(), None),
                    Outputs::from_patterns(&["app/.tsc"]),
                ),
                (
                    ("app".into(), "codegen".into(), None),
                    Outputs::from_patterns(&["app/generated"]),
                ),
                // An output a glob declares, inside a directory the task lists.
                (
                    ("app".into(), "bundle".into(), None),
                    Outputs::from_patterns(&["app/out/*.js"]),
                ),
                // Another target declaring the dependency's output too.
                (
                    ("app".into(), "package".into(), None),
                    Outputs::from_patterns(&["app/generated"]),
                ),
                // An output inside a directory that only exists to hold it.
                (
                    ("app".into(), "deploy".into(), None),
                    Outputs::from_patterns(&["app/.cache/deploy"]),
                ),
            ],
            &roots,
        );
        let ignore = [Pattern::new("**/.fallow", false).unwrap()];
        let unlisted = Unlisted {
            own: Vec::new(),
            others: Some(Others {
                outputs: &outputs,
                // It depends on codegen, whose output is its input.
                upstream: [
                    ("app".into(), "build".into(), None),
                    ("app".into(), "codegen".into(), None),
                ]
                .into(),
            }),
            ignore: &ignore,
        };
        assert_eq!(
            unlisted.directories("app"),
            [
                "app".to_owned(),
                "app/.cache".to_owned(),
                "app/.cache/deploy".to_owned(),
                "app/.tsc".to_owned(),
                "app/generated".to_owned(),
                "app/out".to_owned()
            ]
            .into()
        );
        let snapshot = Snapshot::new(&workspace, &graph, &root.join("cache")).unwrap();
        let key = || strong_key("weak", &set, &snapshot, root, &unlisted).unwrap();
        let before = key();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let taken = Before::take(root, unlisted.directories("app"), &unlisted);
        let started = now(root);
        let changed = |taken: &Before| {
            set.changed_since(root, started, &unlisted, taken)
                .map(|changed| changed.path)
        };
        // Another target's output, the directory holding another's, and an
        // ignored path appear as it runs.
        std::fs::create_dir(root.join("app/.tsc")).unwrap();
        std::fs::create_dir_all(root.join("app/.cache/deploy")).unwrap();
        std::fs::create_dir(root.join("app/.fallow")).unwrap();
        assert_eq!(key(), before);
        assert_eq!(changed(&taken), None);
        // Another target's output listed itself has nothing to key, though
        // nothing listed it before.
        std::fs::write(root.join("app/.tsc/index.js"), "").unwrap();
        let output = PathSet {
            listings: ["app/.tsc".to_owned()].into(),
            ..PathSet::default()
        };
        assert_eq!(
            output.changed_since(root, started, &unlisted, &Before::default()),
            None
        );
        // The project's root is no folder holding its outputs.
        assert!(!unlisted.matches("app"));
        // A listed directory removed as it ran, with no listing from before.
        std::fs::create_dir(root.join("app/gone")).unwrap();
        let gone = PathSet {
            listings: ["app/gone".to_owned()].into(),
            ..PathSet::default()
        };
        std::fs::remove_dir(root.join("app/gone")).unwrap();
        assert_eq!(
            gone.changed_since(root, started, &unlisted, &Before::default())
                .map(|changed| changed.path),
            Some("app/gone")
        );
        // An output appearing in a listed directory a glob declares it in.
        std::fs::write(root.join("app/out/new.js"), "").unwrap();
        let out = PathSet {
            listings: ["app/out".to_owned()].into(),
            ..PathSet::default()
        };
        assert_eq!(out.changed_since(root, started, &unlisted, &taken), None);
        assert_eq!(
            out.changed_since(root, started, &unlisted, &Before::default())
                .map(|changed| changed.path),
            Some("app/out")
        );
        // A dependency's output is the task's input, and stays listed.
        assert!(!unlisted.matches("app/generated"));
        assert!(unlisted.matches("app/.tsc/index.js"));
        // Without the entries from before, the directory's time is all there
        // is, and it moved.
        assert_eq!(changed(&Before::default()), Some("app"));
        // A removal is still a change, though left-out entries are there.
        std::fs::remove_file(root.join("app/kept.txt")).unwrap();
        assert_ne!(key(), before);
        assert_eq!(
            set.changed_since(root, started, &unlisted, &taken)
                .and_then(|changed| changed.how),
            Some("kept.txt removed".to_owned())
        );
        std::fs::write(root.join("app/kept.txt"), "").unwrap();
        assert_eq!(changed(&taken), None);
        // So is anything else added, which the reason names.
        std::fs::write(root.join("app/README.md"), "").unwrap();
        assert_ne!(key(), before);
        assert_eq!(
            set.changed_since(root, started, &unlisted, &taken)
                .map(|changed| changed.to_string()),
            Some("app changed while it ran (README.md added)".to_owned())
        );
    }
}
