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
//! `ignoreReads` are left out. What lies in `node_modules` without being
//! installed there stays: see [`Produced`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use qk_input_analysis::{Access, Operation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::hash::{Snapshot, key};
use crate::paths::Outputs;

/// How many path sets are kept for one weak key, the newest first: a task
/// that reads differently from run to run, such as by platform or by what it
/// finds, keeps a set for each way.
const KEPT: usize = 8;

/// The version of the record path sets are kept and shared in. Version 2
/// lists paths by directory, and leaves out the existence of the paths a
/// task found missing while they still were. Version 1 is read too.
const VERSION: u32 = 2;

#[derive(Clone, Debug, Default, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathSet {
    /// Files whose content the task read.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        with = "by_directory"
    )]
    pub reads: BTreeSet<String>,
    /// Directories it listed; `.` is the workspace root.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        with = "by_directory"
    )]
    pub listings: BTreeSet<String>,
    /// Paths it found, without reading them. A file among them is keyed by
    /// its content all the same: a tool that compares timestamps, as
    /// `tsc --build` does, decides by a file it never reads.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        with = "by_directory"
    )]
    pub present: BTreeSet<String>,
    /// Paths it looked for and did not find.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        with = "by_directory"
    )]
    pub absent: BTreeSet<String>,
    /// Paths it wrote in the directories it listed, left out of their
    /// listings: they are there after it runs, and need not be before.
    #[serde(
        default,
        skip_serializing_if = "BTreeSet::is_empty",
        with = "by_directory"
    )]
    pub written: BTreeSet<String>,
    /// How its paths were when it was recorded, to tell which changed once
    /// its key no longer matches. Not part of the set, nor of its key: sets
    /// recorded before it was kept have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<Values>,
}

/// Two path sets are the same set when they hold the same paths, however
/// those were when each was recorded.
impl PartialEq for PathSet {
    fn eq(&self, other: &Self) -> bool {
        (
            &self.reads,
            &self.listings,
            &self.present,
            &self.absent,
            &self.written,
        ) == (
            &other.reads,
            &other.listings,
            &other.present,
            &other.absent,
            &other.written,
        )
    }
}

/// A short digest of how each of a path set's paths was: each read file's
/// content, each listed directory's entries and each looked-up path's kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Values {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reads: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub listings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub existence: BTreeMap<String, String>,
}

impl Values {
    /// The digests of `values`, as [`PathSet::values`] gives them.
    fn of(values: &Value) -> Result<Self> {
        let digests = |part: &str| -> Result<BTreeMap<String, String>> {
            values[part]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(path, value)| Ok((path.clone(), key(value)?[..16].to_owned())))
                .collect()
        };
        Ok(Self {
            reads: digests("reads")?,
            listings: digests("listings")?,
            existence: digests("existence")?,
        })
    }
}

/// What a path set leaves out.
pub struct Exclusions<'a> {
    /// The workspace root, as given and with its links resolved.
    pub roots: [&'a Path; 2],
    /// The task's outputs and warm paths.
    pub outputs: &'a [&'a Outputs],
    /// `ignoreReads`.
    pub ignore: crate::ultracache::IgnoredReads,
    /// qk's own directories inside the workspace, relative to it.
    pub state: Vec<String>,
    /// What lies in `node_modules` without being installed there.
    pub produced: &'a Produced,
}

/// Paths in `node_modules` that the lockfile does not key, which a path set
/// keeps: what the tasks a task depends on declare as their outputs, such as
/// a client generated into `node_modules/.prisma`, and what lies below a
/// directory in `node_modules` that an upstream output or an
/// `includeIgnored` fileset reads from, with the directories between such a
/// directory and the `node_modules` holding it, where a task looks for what
/// was produced. Everything else there is installed.
#[derive(Default)]
pub struct Produced {
    /// The declared outputs of the tasks the task depends on.
    upstream: Vec<Outputs>,
    /// Directories naming a `node_modules` directory, workspace-relative.
    roots: Vec<String>,
    /// The outputs of the tasks it depends on as they declare them, sorted.
    pub declared: Vec<Vec<String>>,
    /// The bounds of `upstream`: an output is or lies below one of them.
    bounds: BTreeSet<String>,
}

impl Produced {
    /// The tasks' upstream outputs and `includeIgnored` roots: of the roots,
    /// only those in `node_modules` count, as a project root would hold
    /// installed packages too.
    pub fn new(upstream: Vec<Outputs>, include_ignored: Vec<String>) -> Self {
        let mut roots: Vec<String> = upstream
            .iter()
            .flat_map(|outputs| outputs.anchors().map(str::to_owned))
            .chain(include_ignored)
            .filter(|root| in_node_modules(root))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        let bounds = upstream
            .iter()
            .flat_map(|outputs| outputs.bounds().map(str::to_owned))
            .collect();
        Self {
            upstream,
            roots,
            declared: Vec::new(),
            bounds,
        }
    }

    /// With the upstream outputs as declared, which [`Self::weak`] keys.
    pub fn declared(mut self, mut declared: Vec<Vec<String>>) -> Self {
        declared.sort_unstable();
        self.declared = declared;
        self
    }

    /// Whether `path`, workspace-relative, is produced rather than installed,
    /// or a directory in `node_modules` holding what is.
    ///
    /// Asked of every access in `node_modules` a trace holds, so the output
    /// patterns are only matched within one of their bounds.
    pub fn holds(&self, path: &str) -> bool {
        self.roots.iter().any(|root| {
            path == root || under(path, root) || (under(root, path) && inside_node_modules(path))
        }) || self.within_bounds(path) && self.upstream.iter().any(|outputs| outputs.matches(path))
    }

    /// Whether `path` is one of the upstream outputs' bounds or lies below
    /// one.
    fn within_bounds(&self, path: &str) -> bool {
        self.bounds.contains("")
            || std::iter::successors(Some(path), |path| {
                path.rsplit_once('/').map(|(parent, _)| parent)
            })
            .any(|ancestor| self.bounds.contains(ancestor))
    }

    /// Whether a listing of `path`, a directory in `node_modules`, is kept,
    /// and whether, as an entry of one, it is listed: what holds what is
    /// produced or may come to, as `node_modules` holds `.gen` for an
    /// output in `node_modules/.gen/app`, or as a directory below the anchor
    /// of an upstream output's glob holds what it may match. An installed
    /// package elsewhere is neither listed nor its listing kept.
    pub fn lists(&self, path: &str) -> bool {
        self.holds(path)
            || self.roots.iter().any(|root| under(root, path))
            || self
                .upstream
                .iter()
                .flat_map(Outputs::anchors)
                .any(|anchor| under(path, anchor))
    }

    /// Whether an access to `path`, in `node_modules`, is read from a trace
    /// at all, and so counts toward its limits: what [`Self::holds`], and
    /// where only a listing is kept, as [`Self::lists`] says, only listings
    /// and writes. An installed package read below where an upstream glob
    /// starts is dropped from a path set anyway.
    pub fn traced(&self, path: &str, operation: &Operation) -> bool {
        let listed = matches!(
            operation,
            Operation::ListDirectory | Operation::ReadDirectory
        ) || operation.writes();
        self.holds(path) || listed && self.lists(path)
    }

    /// What decides which paths in `node_modules` a recording keeps, for
    /// the weak key: a path set recorded under other declarations could
    /// leave out what this task's would key. `None` where nothing does.
    pub fn weak(&self) -> Option<Value> {
        (!self.roots.is_empty() || !self.declared.is_empty())
            .then(|| json!({"roots": self.roots, "upstream": self.declared}))
    }
}

/// Whether `path` has a `node_modules` component.
pub fn in_node_modules(path: &str) -> bool {
    path.split('/').any(|part| part == "node_modules")
}

/// Whether `path` lies inside a `node_modules` directory, rather than only
/// naming one.
fn inside_node_modules(path: &str) -> bool {
    path.rsplit_once('/')
        .is_some_and(|(directory, _)| in_node_modules(directory))
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
        self.excluded_unless(path, |path| self.produced.holds(path))
    }

    /// Whether a listing of `path` is left out: as [`Self::excluded`], but
    /// a directory in `node_modules` that [`Produced::lists`] is kept.
    fn excluded_listing(&self, path: &str) -> bool {
        self.excluded_unless(path, |path| self.produced.lists(path))
    }

    fn excluded_unless(&self, path: &str, produced: impl Fn(&str) -> bool) -> bool {
        path.split('/').next() == Some(".git")
            || in_node_modules(path) && !produced(path)
            || self.state.iter().any(|state| {
                path == state
                    || path
                        .strip_prefix(state.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            || self.outputs.iter().any(|outputs| outputs.matches(path))
            || self.ignore.is_match(path)
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
            let listing = matches!(
                access.operation,
                Operation::ListDirectory | Operation::ReadDirectory
            );
            // What it wrote itself is no input.
            if written.contains(&path)
                || if listing {
                    exclusions.excluded_listing(&path)
                } else {
                    exclusions.excluded(&path)
                }
            {
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
        // has nothing in it to key. One only holding the task's outputs
        // keys what else it holds.
        if directory != "." && unlisted.matches(directory) && !unlisted.own_folder(directory) {
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

    /// The set's paths, as its key holds them: each in full, as version 1
    /// kept them, so that a set keys the same whichever version it came in.
    pub fn digest(&self) -> Result<String> {
        fn none(paths: &&BTreeSet<String>) -> bool {
            paths.is_empty()
        }
        #[derive(Serialize)]
        struct Paths<'a> {
            #[serde(skip_serializing_if = "none")]
            reads: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "none")]
            listings: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "none")]
            present: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "none")]
            absent: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "none")]
            written: &'a BTreeSet<String>,
        }
        key(&serde_json::to_value(Paths {
            reads: &self.reads,
            listings: &self.listings,
            present: &self.present,
            absent: &self.absent,
            written: &self.written,
        })?)
    }

    /// The paths that are not as they were when it was recorded, each with
    /// what the task did to it: `read`, `listed`, `found` or `looked for`.
    /// None when it kept no record of them.
    pub(crate) fn changed(
        &self,
        snapshot: &Snapshot,
        root: &Path,
        unlisted: &Unlisted,
    ) -> Result<Vec<(&str, &'static str)>> {
        let Some(recorded) = &self.recorded else {
            return Ok(Vec::new());
        };
        let now = Values::of(&self.values(snapshot, root, unlisted)?)?;
        let differs = |then: &BTreeMap<String, String>,
                       now: &BTreeMap<String, String>,
                       path: &str| { then.get(path) != now.get(path) };
        let mut changed = Vec::new();
        for path in &self.reads {
            if differs(&recorded.reads, &now.reads, path) {
                changed.push((path.as_str(), "read"));
            }
        }
        for path in &self.listings {
            if differs(&recorded.listings, &now.listings, path) {
                changed.push((path.as_str(), "listed"));
            }
        }
        for path in &self.present {
            if differs(&recorded.existence, &now.existence, path) {
                changed.push((path.as_str(), "found"));
            }
        }
        // A path found missing keeps no record of that while it was.
        let missing = missing()?;
        for path in &self.absent {
            let then = recorded.existence.get(path).unwrap_or(&missing);
            if Some(then) != now.existence.get(path) {
                changed.push((path.as_str(), "looked for"));
            }
        }
        Ok(changed)
    }

    /// How the set's paths are now: each read file's content, each listed
    /// directory's entries and each looked-up path's kind. `outputs` are the
    /// task's own, left out of listings.
    /// Why a file it names cannot be keyed by its content, as one read
    /// through a linked directory, or a link leading out of the workspace,
    /// cannot: [`Self::values`] would key it the same whatever it held.
    pub(crate) fn unkeyable(&self, snapshot: &Snapshot, root: &Path) -> Option<String> {
        self.reads
            .iter()
            .chain(&self.present)
            .chain(&self.absent)
            .find_map(|path| match kind(&root.join(path)) {
                "file" | "symlink" => snapshot
                    .recorded_value(root, path)
                    .err()
                    .map(|error| format!("{path} cannot be keyed by its content ({error:#})")),
                _ => None,
            })
    }

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
        // A directory that only holds the task's outputs is there once they
        // are, or once a sandbox made it for them: made or not, it keys the
        // same, and lists only what else it holds.
        let absent = json!("absent");
        let mut listings = Map::new();
        for directory in &self.listings {
            let listing = match self.listing(root, directory, unlisted)? {
                listing if listing == absent && unlisted.own_folder(directory) => json!({}),
                listing => listing,
            };
            listings.insert(directory.clone(), listing);
        }
        let existence: Map<String, Value> = self
            .present
            .iter()
            .chain(&self.absent)
            .map(|path| {
                let value = match value(path) {
                    value
                        if unlisted.own_folder(path)
                            && (value == absent || value == json!("directory")) =>
                    {
                        json!("folder")
                    }
                    value => value,
                };
                (path.clone(), value)
            })
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

/// The digest [`Values`] holds for a path that is missing.
fn missing() -> Result<String> {
    Ok(key(&json!("absent"))?[..16].to_owned())
}

/// The directory a workspace-relative path is in, `.` for the root.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(parent, _)| parent)
}

/// Whether `path` lies below `directory`, both workspace-relative.
fn under(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
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
    /// Each project's root, and each directory holding one.
    projects: BTreeSet<String>,
}

impl WorkspaceOutputs {
    /// Whether `path`, workspace-relative, is a declared output of any
    /// target.
    pub(crate) fn matches(&self, path: &str) -> bool {
        self.outputs
            .iter()
            .any(|(_, outputs)| outputs.matches(path))
    }
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
        let projects = projects.into_iter().map(str::to_owned).collect();
        Self {
            outputs,
            by_anchor,
            folders,
            by_folder,
            projects,
        }
    }

    /// The directories that hold nothing a task keys but `own`, its outputs:
    /// each directory between the workspace root and one of their anchors,
    /// such as `dist` and `dist/apps` for `dist/apps/web`, but no project's
    /// root, nor a directory holding one, nor one holding or held by an
    /// output of `upstream`, the targets it depends on.
    fn own_folders(&self, own: &[&Outputs], upstream: &BTreeSet<TargetOf>) -> BTreeSet<String> {
        let inputs: Vec<&str> = self
            .outputs
            .iter()
            .filter(|(target, _)| upstream.contains(target))
            .flat_map(|(_, outputs)| outputs.anchors())
            .collect();
        let related = |a: &str, b: &str| a == b || under(a, b) || under(b, a);
        own.iter()
            .flat_map(|outputs| outputs.anchors())
            .flat_map(|anchor| anchor.match_indices('/').map(move |(at, _)| &anchor[..at]))
            .filter(|folder| !self.projects.contains(*folder))
            .filter(|folder| !inputs.iter().any(|input| related(input, folder)))
            .map(str::to_owned)
            .collect()
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

/// The outputs a recording's listings leave out: those of every target in
/// the workspace, the task's own included, except the targets the task
/// depends on, whose outputs are its inputs.
pub(crate) struct Others<'a> {
    pub outputs: &'a WorkspaceOutputs,
    /// The targets the task depends on.
    pub upstream: BTreeSet<TargetOf>,
    /// The directories that only hold the task's own outputs, which come
    /// and go with them as its project's folders do: see
    /// [`WorkspaceOutputs::own_folders`].
    pub own_folders: BTreeSet<String>,
}

impl<'a> Others<'a> {
    /// `own` holds the task's outputs and warm paths.
    pub(crate) fn new(
        outputs: &'a WorkspaceOutputs,
        upstream: BTreeSet<TargetOf>,
        own: &[&Outputs],
    ) -> Self {
        let own_folders = outputs.own_folders(own, &upstream);
        Self {
            outputs,
            upstream,
            own_folders,
        }
    }

    /// Whether `path` is one of the task's own folders.
    pub(crate) fn own_folder(&self, path: &str) -> bool {
        self.own_folders.contains(path)
    }
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
/// not those of the targets it depends on, what its `ignoreReads` names, and
/// in `node_modules`, what is installed rather than produced.
/// Only listings leave them out: a file among them that the task read is
/// still keyed by its content.
pub(crate) struct Unlisted<'a> {
    pub own: Vec<&'a Outputs>,
    pub others: Option<Others<'a>>,
    pub ignore: &'a crate::ultracache::IgnoredReads,
    /// What is produced in `node_modules`, which alone is listed there.
    pub produced: Option<&'a Produced>,
}

impl Unlisted<'_> {
    fn matches(&self, path: &str) -> bool {
        self.own(path)
            || self
                .others
                .as_ref()
                .is_some_and(|others| others.matches(path))
            || self.ignore.is_match(path)
            || inside_node_modules(path)
                && self.produced.is_some_and(|produced| !produced.lists(path))
    }

    /// Whether `path` is the task's own output, or a directory that only
    /// holds its outputs.
    fn own(&self, path: &str) -> bool {
        self.own.iter().any(|outputs| outputs.matches(path)) || self.own_folder(path)
    }

    /// Whether `path` is a directory that only holds the task's outputs.
    fn own_folder(&self, path: &str) -> bool {
        self.others
            .as_ref()
            .is_some_and(|others| others.own_folder(path))
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
            // Its own folders, whose other entries its listings keep.
            directories.extend(others.own_folders.iter().cloned());
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
        .filter(|stored| matches!(stored.version, 1 | VERSION) && stored.weak == weak)
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

/// `record`, a record of path sets, in at most `largest` bytes: with its
/// oldest sets left out until it fits, or `None` when its newest set alone
/// does not.
pub(crate) fn within(record: &[u8], largest: usize) -> Result<Option<Vec<u8>>> {
    if record.len() <= largest {
        return Ok(Some(record.to_vec()));
    }
    let mut stored: Stored = serde_json::from_slice(record)?;
    while stored.sets.len() > 1 {
        stored.sets.pop();
        let shorter = serde_json::to_vec(&stored)?;
        if shorter.len() <= largest {
            return Ok(Some(shorter));
        }
    }
    Ok(None)
}

/// Adds the path sets in a record the remote store held for `weak` behind
/// those kept locally, returning the ones that are new here. A record for
/// another weak key or another version adds nothing, and neither does a set
/// naming a path outside the workspace.
pub(crate) fn merge(cache: &Path, weak: &str, record: &[u8]) -> Result<Vec<PathSet>> {
    let Some(fetched) = serde_json::from_slice::<Stored>(record)
        .ok()
        .filter(|stored| matches!(stored.version, 1 | VERSION) && stored.weak == weak)
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
            version: VERSION,
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
    keyed(weak, set, &set.values(snapshot, root, unlisted)?)
}

fn keyed(weak: &str, set: &PathSet, values: &Value) -> Result<String> {
    key(&json!({
        "schema": "qk-recorded-v1",
        "weak": weak,
        "set": set.digest()?,
        "values": values,
    }))
}

/// `set`'s key under `weak`, from how its paths are now, and `set` with
/// how they are kept for telling later which changed.
pub(crate) fn with_values(
    weak: &str,
    mut set: PathSet,
    snapshot: &Snapshot,
    root: &Path,
    unlisted: &Unlisted,
) -> Result<(String, PathSet)> {
    let values = set.values(snapshot, root, unlisted)?;
    let strong = keyed(weak, &set, &values)?;
    let mut recorded = Values::of(&values)?;
    // Most of what a task looks up it finds missing, such as each extension
    // a module resolver tries: those that still are are left to be assumed.
    let missing = missing()?;
    recorded
        .existence
        .retain(|path, digest| !set.absent.contains(path) || *digest != missing);
    set.recorded = Some(recorded);
    Ok((strong, set))
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

/// A record's lists of paths, by the directory each path is in, so that a
/// directory is written once rather than for every path in it: a path is its
/// directory, up to and with its last `/`, followed by its name. A list of
/// whole paths, as version 1 kept them, is read too.
mod by_directory {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;

    use serde::de::{MapAccess, SeqAccess, Visitor};
    use serde::{Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        paths: &BTreeSet<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for path in paths {
            let name = path.rfind('/').map_or(0, |slash| slash + 1);
            grouped
                .entry(&path[..name])
                .or_default()
                .push(&path[name..]);
        }
        grouped.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeSet<String>, D::Error> {
        deserializer.deserialize_any(Paths)
    }

    struct Paths;

    impl<'de> Visitor<'de> for Paths {
        type Value = BTreeSet<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("paths by directory, or a list of paths")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut paths = BTreeSet::new();
            while let Some((directory, names)) = map.next_entry::<String, Vec<String>>()? {
                paths.extend(names.into_iter().map(|name| format!("{directory}{name}")));
            }
            Ok(paths)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut paths = BTreeSet::new();
            while let Some(path) = seq.next_element::<String>()? {
                paths.insert(path);
            }
            Ok(paths)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ultracache::IgnoredReads;

    static IGNORE_NOTHING: IgnoredReads = IgnoredReads::none();

    /// Nothing left out of listings.
    const NOTHING: Unlisted<'static> = Unlisted {
        own: Vec::new(),
        others: None,
        ignore: &IGNORE_NOTHING,
        produced: None,
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
            ignore: IgnoredReads::of_globs(&["src/*.log".to_owned()]).unwrap(),
            state: vec![".qk".into()],
            produced: &Produced::default(),
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
                recorded: None,
            }
        );
    }

    #[test]
    fn holds_what_any_upstream_output_matches() {
        let upstream = || {
            vec![
                Outputs::from_paths(&["node_modules/.gen/app".into()]).unwrap(),
                Outputs::from_paths(&["libs/*/node_modules/.out/**/*.js".into()]).unwrap(),
                Outputs::from_paths(&[
                    "libs/types/**/*.d.ts".into(),
                    "!libs/types/skip.d.ts".into(),
                ])
                .unwrap(),
                // Matched as `packages/scope/node_modules/pkg/{foo,bar}.js`,
                // outside its anchor, `packages/@scope/node_modules/pkg`.
                Outputs::from_paths(&["packages/@scope/node_modules/pkg/(foo|bar).js".into()])
                    .unwrap(),
                // Matched below `vendor`, as `\/` is a literal `/`.
                Outputs::from_paths(&[r"vendor/lib\/node_modules/pkg/*.js".into()]).unwrap(),
            ]
        };
        let produced = Produced::new(upstream(), Vec::new());
        // A bound of the workspace root would let every path through.
        assert!(!produced.bounds.contains(""));
        let outputs = upstream();
        for path in [
            "node_modules/.gen/app",
            "node_modules/.gen/app/index.js",
            "node_modules/.gen",
            "node_modules/.gen/other",
            "node_modules/react/index.js",
            "libs/a/node_modules/.out/x/y.js",
            "libs/a/node_modules/.out/x/y.ts",
            "libs/a/node_modules/react/index.js",
            "libs/types/a/b.d.ts",
            "libs/types/skip.d.ts",
            "libs/typesx/a.d.ts",
            "packages/scope/node_modules/pkg/foo.js",
            "packages/@scope/node_modules/pkg/foo.js",
            "vendor/lib/node_modules/pkg/a.js",
            "vendor/other/a.js",
            ".",
        ] {
            let matching_every_output = produced.roots.iter().any(|root| {
                path == root
                    || under(path, root)
                    || (under(root, path) && inside_node_modules(path))
            }) || outputs.iter().any(|outputs| outputs.matches(path));
            assert_eq!(produced.holds(path), matching_every_output, "{path}");
        }
        assert!(produced.holds("packages/scope/node_modules/pkg/foo.js"));
        assert!(produced.holds("vendor/lib/node_modules/pkg/a.js"));
        // An escaped `/` in the first segment bounds an output by the root.
        let rooted = Produced::new(
            vec![Outputs::from_paths(&[r"vendor\/node_modules/pkg/*.js".into()]).unwrap()],
            Vec::new(),
        );
        assert!(rooted.bounds.contains(""));
        assert!(rooted.holds("vendor/node_modules/pkg/a.js"));
    }

    /// What upstream tasks write into `node_modules`, and what an
    /// `includeIgnored` fileset reads there, is no installed package: the
    /// lockfile does not key it, so the path set does.
    #[test]
    fn keeps_what_is_produced_in_node_modules() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = |relative: &str| root.join(relative).to_str().unwrap().to_owned();
        let produced = Produced::new(
            vec![
                Outputs::from_paths(&["node_modules/.gen/app".into()]).unwrap(),
                Outputs::from_paths(&["dist/lib".into()]).unwrap(),
                // A glob from a project root names no directory in
                // node_modules: only what it matches is produced.
                Outputs::from_paths(&["libs/types/**/*.d.ts".into()]).unwrap(),
            ],
            vec![
                "libs/e2e/node_modules/.cache/fixtures".into(),
                // Reading a project's own directory does not make its
                // installed packages produced.
                "libs/e2e".into(),
            ],
        );
        assert_eq!(
            produced.roots,
            [
                "libs/e2e/node_modules/.cache/fixtures",
                "node_modules/.gen/app"
            ]
        );
        let exclusions = Exclusions {
            roots: [&root, &root],
            outputs: &[],
            ignore: IgnoredReads::none(),
            state: Vec::new(),
            produced: &produced,
        };
        let read = |relative: &str| access(&path(relative), Operation::OpenRead, "success");
        let accesses = [
            read("node_modules/.gen/app/index.js"),
            access(
                &path("node_modules/.gen/app"),
                Operation::ListDirectory,
                "success",
            ),
            read("node_modules/.gen/other/index.js"),
            read("node_modules/foo/index.js"),
            read("dist/lib/node_modules/dep/index.js"),
            read("libs/types/node_modules/@types/x/index.d.ts"),
            read("libs/types/node_modules/x/index.js"),
            read("libs/e2e/node_modules/.cache/fixtures/a.json"),
            read("libs/e2e/node_modules/foo/index.js"),
            // Where it looks for what was produced, but not among what is
            // installed.
            access(
                &path("node_modules/.gen"),
                Operation::ListDirectory,
                "success",
            ),
            access(
                &path("libs/e2e/node_modules/.cache"),
                Operation::ListDirectory,
                "success",
            ),
            access(&path("node_modules"), Operation::ListDirectory, "success"),
            // Where an upstream glob may come to match.
            access(
                &path("libs/types/node_modules/@types"),
                Operation::ListDirectory,
                "success",
            ),
            access(
                &path("node_modules/foo"),
                Operation::ListDirectory,
                "success",
            ),
            access(
                &path("node_modules/.gen/app/index.d.ts"),
                Operation::Metadata,
                "success",
            ),
            access(
                &path("node_modules/.gen/app/extra.js"),
                Operation::Metadata,
                "missing",
            ),
            access(
                &path("node_modules/foo/package.json"),
                Operation::Metadata,
                "success",
            ),
            access(
                &path("node_modules/foo/extra.js"),
                Operation::Metadata,
                "missing",
            ),
        ];
        let set = PathSet::from_accesses(&accesses, &exclusions, &root);
        assert_eq!(
            set.reads,
            [
                "dist/lib/node_modules/dep/index.js",
                "libs/e2e/node_modules/.cache/fixtures/a.json",
                "libs/types/node_modules/@types/x/index.d.ts",
                "node_modules/.gen/app/index.js",
            ]
            .map(String::from)
            .into()
        );
        assert_eq!(
            set.listings,
            [
                "libs/e2e/node_modules/.cache",
                "libs/types/node_modules/@types",
                "node_modules",
                "node_modules/.gen",
                "node_modules/.gen/app",
            ]
            .map(String::from)
            .into()
        );
        assert_eq!(
            set.present,
            ["node_modules/.gen/app/index.d.ts".to_owned()].into()
        );
        assert_eq!(
            set.absent,
            ["node_modules/.gen/app/extra.js".to_owned()].into()
        );
        // And what it looked up there keys it.
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
        let cache = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, cache.path()).unwrap();
        std::fs::create_dir_all(root.join("node_modules/.gen/app")).unwrap();
        std::fs::write(root.join("node_modules/.gen/app/index.d.ts"), "").unwrap();
        let unlisted = Unlisted {
            produced: Some(&produced),
            ..NOTHING
        };
        let key = || strong_key("weak", &set, &snapshot, &root, &unlisted).unwrap();
        let generated = key();
        // A package installed beside what was produced is the lockfile's.
        std::fs::create_dir_all(root.join("node_modules/bar")).unwrap();
        assert_eq!(key(), generated);
        // What was produced coming and going is not.
        std::fs::rename(root.join("node_modules/.gen"), root.join(".gen")).unwrap();
        assert_ne!(key(), generated);
        std::fs::rename(root.join(".gen"), root.join("node_modules/.gen")).unwrap();
        assert_eq!(key(), generated);
        // The listing of node_modules alone keys what was produced in it.
        let listing = PathSet {
            listings: ["node_modules".to_owned()].into(),
            ..PathSet::default()
        };
        let listed = || strong_key("weak", &listing, &snapshot, &root, &unlisted).unwrap();
        let before = listed();
        std::fs::create_dir_all(root.join("node_modules/baz")).unwrap();
        assert_eq!(listed(), before);
        std::fs::rename(root.join("node_modules/.gen"), root.join(".gen")).unwrap();
        assert_ne!(listed(), before);
        std::fs::rename(root.join(".gen"), root.join("node_modules/.gen")).unwrap();
        assert_eq!(listed(), before);
        // What it found and what it found missing there each key it alone.
        let keyed = |set: &PathSet| strong_key("weak", set, &snapshot, &root, &unlisted).unwrap();
        let found = PathSet {
            present: set.present.clone(),
            ..PathSet::default()
        };
        let before = keyed(&found);
        std::fs::remove_file(root.join("node_modules/.gen/app/index.d.ts")).unwrap();
        assert_ne!(keyed(&found), before);
        std::fs::write(root.join("node_modules/.gen/app/index.d.ts"), "").unwrap();
        assert_eq!(keyed(&found), before);
        let missing = PathSet {
            absent: set.absent.clone(),
            ..PathSet::default()
        };
        let before = keyed(&missing);
        std::fs::write(root.join("node_modules/.gen/app/extra.js"), "").unwrap();
        assert_ne!(keyed(&missing), before);
        std::fs::remove_file(root.join("node_modules/.gen/app/extra.js")).unwrap();
        assert_eq!(keyed(&missing), before);
        std::fs::remove_file(root.join("node_modules/.gen/app/index.d.ts")).unwrap();
        assert_ne!(key(), generated);
        std::fs::write(root.join("node_modules/.gen/app/index.d.ts"), "").unwrap();
        assert_eq!(key(), generated);
        std::fs::write(root.join("node_modules/.gen/app/extra.js"), "").unwrap();
        assert_ne!(key(), generated);
    }

    /// Below where an upstream glob starts, only listings and writes are read
    /// from a trace, so an installed package's reads there never count
    /// toward its limits.
    #[test]
    fn a_trace_keeps_in_node_modules_only_what_a_path_set_may() {
        let produced = Produced::new(
            vec![
                Outputs::from_paths(&["node_modules/.gen/app".into()]).unwrap(),
                Outputs::from_paths(&["libs/types/**/*.d.ts".into()]).unwrap(),
            ],
            Vec::new(),
        );
        let traced = |path: &str, operation: Operation| produced.traced(path, &operation);
        // What is produced, however it is accessed.
        assert!(traced(
            "node_modules/.gen/app/index.js",
            Operation::OpenRead
        ));
        assert!(traced(
            "libs/types/node_modules/@types/x/index.d.ts",
            Operation::OpenRead
        ));
        // Where only listings are kept, listings and writes alone.
        assert!(traced("node_modules", Operation::ListDirectory));
        assert!(traced(
            "libs/types/node_modules/@types",
            Operation::ListDirectory
        ));
        assert!(traced(
            "libs/types/node_modules/x/cache.json",
            Operation::Write
        ));
        assert!(!traced(
            "libs/types/node_modules/x/index.js",
            Operation::OpenRead
        ));
        assert!(!traced("libs/types/node_modules/x", Operation::Metadata));
        // An installed package elsewhere, not at all.
        assert!(!traced("node_modules/foo/index.js", Operation::OpenRead));
        assert!(!traced("node_modules/foo", Operation::ListDirectory));
    }

    /// What decides which paths in node_modules a recording keeps is keyed
    /// however the dependencies come, and not at all where nothing does.
    #[test]
    fn what_keeps_paths_in_node_modules_is_keyed_once() {
        assert_eq!(Produced::default().weak(), None);
        let produced = |declared: &[&[&str]]| {
            Produced::default()
                .declared(
                    declared
                        .iter()
                        .map(|outputs| outputs.iter().map(|output| (*output).to_owned()).collect())
                        .collect(),
                )
                .weak()
        };
        let both = produced(&[&["dist/lib"], &["node_modules/.gen/app"]]);
        assert!(both.is_some());
        assert_eq!(produced(&[&["node_modules/.gen/app"], &["dist/lib"]]), both);
        assert_ne!(produced(&[&["dist/lib"]]), both);
    }

    /// A file read through a linked directory cannot be keyed by its
    /// content, so a recording naming one keys nothing.
    #[cfg(unix)]
    #[test]
    fn a_file_read_through_a_linked_directory_is_unkeyable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.join("project.json"),
            r#"{"name":"app","targets":{"build":{"command":"true"}}}"#,
        )
        .unwrap();
        std::fs::create_dir(root.join("package")).unwrap();
        std::fs::write(root.join("package/index.js"), "").unwrap();
        std::os::unix::fs::symlink("package", root.join("linked")).unwrap();
        let workspace = qk_config::Workspace::load(&root).unwrap();
        let graph = qk_taskgraph::TaskGraph::build(
            &workspace,
            &[qk_taskgraph::Request::parse("app:build").unwrap()],
        )
        .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, cache.path()).unwrap();
        let reading = |path: &str| PathSet {
            reads: [path.to_owned()].into(),
            ..PathSet::default()
        };
        assert_eq!(
            reading("package/index.js").unkeyable(&snapshot, &root),
            None
        );
        let linked = reading("linked/index.js").unkeyable(&snapshot, &root);
        assert!(
            linked
                .as_deref()
                .is_some_and(|reason| reason.starts_with("linked/index.js cannot be keyed")),
            "{linked:?}"
        );
        // A path it found absent is keyed as such.
        assert_eq!(
            reading("linked/missing.js").unkeyable(&snapshot, &root),
            None
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
            recorded: None,
        };
        let outputs = Outputs::from_paths(&["icons/out".into()]).unwrap();
        let unlisted = Unlisted {
            own: vec![&outputs],
            others: None,
            ignore: &IGNORE_NOTHING,
            produced: None,
        };
        let strong = || strong_key("weak", &set, &snapshot, &root, &unlisted).unwrap();
        let first = strong();
        // Kept with how its paths were, the set has the same key, and names
        // what changes.
        let (kept_key, kept) =
            with_values("weak", set.clone(), &snapshot, &root, &unlisted).unwrap();
        assert_eq!(kept_key, first);
        assert_eq!(kept, set);
        assert_eq!(kept.digest().unwrap(), set.digest().unwrap());
        // A path still missing is assumed so rather than kept.
        let existence = &kept.recorded.as_ref().unwrap().existence;
        assert!(existence.contains_key("checked.txt"));
        assert!(!existence.contains_key("optional.txt"));
        assert!(
            kept.changed(&snapshot, &root, &unlisted)
                .unwrap()
                .is_empty()
        );
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        std::fs::write(root.join("optional.txt"), "").unwrap();
        assert_eq!(
            kept.changed(&snapshot, &root, &unlisted).unwrap(),
            [("a.txt", "read"), ("optional.txt", "looked for")]
        );
        assert!(set.changed(&snapshot, &root, &unlisted).unwrap().is_empty());
        std::fs::write(root.join("a.txt"), "one").unwrap();
        std::fs::remove_file(root.join("optional.txt")).unwrap();
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
    fn keeps_paths_by_directory_and_reads_whole_paths_too() {
        let set = PathSet {
            reads: ["a.txt", "src/a.ts", "src/b.ts", "src/lib/c.ts"]
                .map(String::from)
                .into(),
            present: [".", "/abs", "../escape", "dir/"].map(String::from).into(),
            absent: ["src/a.js", "src/a.mjs"].map(String::from).into(),
            ..PathSet::default()
        };
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            json["reads"],
            json!({"": ["a.txt"], "src/": ["a.ts", "b.ts"], "src/lib/": ["c.ts"]})
        );
        assert_eq!(json["absent"], json!({"src/": ["a.js", "a.mjs"]}));
        // Every path comes back as it was, those outside the workspace too,
        // so that a fetched set naming one is still turned away.
        let read: PathSet = serde_json::from_value(json).unwrap();
        assert_eq!(read, set);
        assert!(!read.is_relative());
        let whole: PathSet = serde_json::from_value(json!({
            "reads": ["a.txt", "src/a.ts", "src/b.ts", "src/lib/c.ts"],
            "present": [".", "/abs", "../escape", "dir/"],
            "absent": ["src/a.js", "src/a.mjs"],
        }))
        .unwrap();
        assert_eq!(whole, set);
    }

    #[test]
    fn a_set_keys_as_its_whole_paths() {
        let set = PathSet {
            reads: ["src/a.ts".into()].into(),
            absent: ["src/a.js".into()].into(),
            ..PathSet::default()
        };
        assert_eq!(
            set.digest().unwrap(),
            key(&json!({"reads": ["src/a.ts"], "absent": ["src/a.js"]})).unwrap()
        );
    }

    #[test]
    fn reads_the_records_of_version_1() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("tmp")).unwrap();
        let record = serde_json::to_vec(&json!({
            "version": 1,
            "weak": "weak",
            "sets": [{"reads": ["src/a.ts"], "absent": ["src/a.js"]}],
        }))
        .unwrap();
        let set = PathSet {
            reads: ["src/a.ts".into()].into(),
            absent: ["src/a.js".into()].into(),
            ..PathSet::default()
        };
        assert_eq!(
            merge(temp.path(), "weak", &record).unwrap(),
            std::slice::from_ref(&set)
        );
        assert_eq!(load(temp.path(), "weak"), [set]);
        let kept: Value =
            serde_json::from_slice(&super::record(temp.path(), "weak").unwrap()).unwrap();
        assert_eq!(kept["version"], VERSION);
    }

    #[test]
    fn a_record_too_large_leaves_out_its_oldest_sets() {
        let set = |read: &str| PathSet {
            reads: [read.to_owned()].into(),
            ..PathSet::default()
        };
        let record = |sets: Vec<PathSet>| {
            serde_json::to_vec(&Stored {
                version: 1,
                weak: "weak".into(),
                sets,
            })
            .unwrap()
        };
        let newest = "n".repeat(100);
        let full = record(vec![
            set(&newest),
            set(&"o".repeat(100)),
            set(&"p".repeat(100)),
        ]);
        assert_eq!(within(&full, full.len()).unwrap(), Some(full.clone()));
        let one = record(vec![set(&newest)]);
        assert_eq!(within(&full, one.len() + 50).unwrap(), Some(one.clone()));
        assert_eq!(within(&full, one.len() - 1).unwrap(), None);
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

    /// The directories that only hold the task's own outputs are there once
    /// they are, or when a sandbox made them before it ran, and not before:
    /// a lookup on a fresh checkout keys them as a run after the task does.
    #[test]
    fn directories_only_holding_its_own_outputs_key_the_same_made_or_not() {
        let roots = [
            ("web".to_owned(), "apps/web".to_owned()),
            ("ui".to_owned(), "libs/ui".to_owned()),
        ]
        .into();
        let declared = |upstream: Option<&str>| {
            let mut outputs = vec![(
                ("web".into(), "build".into(), None),
                Outputs::from_patterns(&["dist/apps/web"]),
            )];
            outputs.extend(upstream.map(|anchor| {
                (
                    ("ui".into(), "build".into(), None),
                    Outputs::from_patterns(&[anchor]),
                )
            }));
            WorkspaceOutputs::new(outputs, &roots)
        };
        let build = Outputs::from_patterns(&["dist/apps/web"]);
        let coverage = Outputs::from_patterns(&["apps/web/coverage/unit"]);
        let upstream: BTreeSet<TargetOf> = [("ui".into(), "build".into(), None)].into();
        let folders = |outputs: &WorkspaceOutputs| {
            outputs
                .own_folders(&[&build, &coverage], &upstream)
                .into_iter()
                .collect::<Vec<_>>()
        };
        // Outside its project's root and inside it, but never the root, nor
        // the directory holding it.
        assert_eq!(
            folders(&declared(None)),
            ["apps/web/coverage", "dist", "dist/apps"]
        );
        // A directory also holding an output it depends on, or held by one,
        // holds its input.
        assert_eq!(
            folders(&declared(Some("dist/libs/ui"))),
            ["apps/web/coverage", "dist/apps"]
        );
        assert_eq!(folders(&declared(Some("dist"))), ["apps/web/coverage"]);
        let outputs = declared(None);

        let temp = tempfile::tempdir().unwrap();
        let root = &temp.path().canonicalize().unwrap();
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
        // Its cache apart, as the root is listed.
        let cache = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::new(&workspace, &graph, cache.path()).unwrap();
        let own = [&build, &coverage];
        let unlisted = Unlisted {
            own: own.to_vec(),
            others: Some(Others::new(&outputs, BTreeSet::new(), &own)),
            ignore: &IGNORE_NOTHING,
            produced: None,
        };
        let set = PathSet {
            listings: [".", "dist", "apps/web/coverage"].map(String::from).into(),
            present: ["dist/apps".to_owned()].into(),
            ..PathSet::default()
        };
        let key = || strong_key("weak", &set, &snapshot, root, &unlisted).unwrap();
        // Made before it ran, as the sandbox makes them, or not.
        std::fs::create_dir_all(root.join("dist/apps")).unwrap();
        std::fs::create_dir_all(root.join("apps/web/coverage")).unwrap();
        let made = key();
        std::fs::remove_dir_all(root.join("dist")).unwrap();
        std::fs::remove_dir(root.join("apps/web/coverage")).unwrap();
        assert_eq!(key(), made);
        // What else such a directory holds is still listed.
        std::fs::create_dir(root.join("apps/web/coverage")).unwrap();
        assert_eq!(key(), made);
        std::fs::write(root.join("apps/web/coverage/README.md"), "").unwrap();
        assert_ne!(key(), made);
        std::fs::remove_file(root.join("apps/web/coverage/README.md")).unwrap();
        // And another directory appearing beside them changes it.
        std::fs::create_dir(root.join("build")).unwrap();
        assert_ne!(key(), made);
        // A source added beside its outputs while it ran is told, as in any
        // directory it listed.
        std::fs::create_dir_all(root.join("dist/apps")).unwrap();
        let directories = unlisted.directories(".");
        assert!(directories.contains("apps/web/coverage"));
        let taken = Before::take(root, directories, &unlisted);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let started = now(root);
        std::fs::write(root.join("apps/web/coverage/added.md"), "").unwrap();
        assert_eq!(
            set.changed_since(root, started, &unlisted, &taken)
                .map(|changed| changed.path),
            Some("apps/web/coverage")
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
        let ignore = IgnoredReads::of_globs(&["**/.fallow".to_owned()]).unwrap();
        let unlisted = Unlisted {
            own: Vec::new(),
            others: Some(Others::new(
                &outputs,
                // It depends on codegen, whose output is its input.
                [
                    ("app".into(), "build".into(), None),
                    ("app".into(), "codegen".into(), None),
                ]
                .into(),
                &[],
            )),
            ignore: &ignore,
            produced: None,
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
