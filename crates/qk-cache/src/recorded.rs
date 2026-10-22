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

use std::collections::BTreeSet;
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
    /// or renamed, which moves the directory's own modification time; what
    /// is written inside an entry does not change it, though it moves the
    /// entry's change time. Where the task wrote into the directory, its time
    /// moves with the task's own writes, so there an entry other than the
    /// task's writes and `outputs` changed it: a file whose change time
    /// moved, by being added, moved in or rewritten, or a subdirectory
    /// created after `started`, by its birth time, which a file system
    /// without one cannot tell. A removal there, or a subdirectory moved in,
    /// cannot be told from the task's own writes.
    pub fn changed_since(
        &self,
        root: &Path,
        started: SystemTime,
        outputs: &[&Outputs],
    ) -> Option<&str> {
        let file = self.reads.iter().chain(&self.present).find(|path| {
            std::fs::symlink_metadata(root.join(path))
                .is_ok_and(|metadata| !metadata.is_dir() && changed(&metadata) >= started)
        });
        file.or_else(|| {
            self.listings.iter().find(|directory| {
                let absolute = if *directory == "." {
                    root.to_path_buf()
                } else {
                    root.join(directory)
                };
                let Ok(entries) = std::fs::read_dir(&absolute) else {
                    return false;
                };
                // Each entry by its path, and whether the task wrote it.
                let entries: Vec<(std::fs::DirEntry, bool)> = entries
                    .flatten()
                    .map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let path = if *directory == "." {
                            name
                        } else {
                            format!("{directory}/{name}")
                        };
                        let own = self.written.contains(&path)
                            || outputs.iter().any(|outputs| outputs.matches(&path));
                        (entry, own)
                    })
                    .collect();
                let own = self.written.iter().any(|path| parent(path) == *directory)
                    || entries.iter().any(|(_, own)| *own);
                if !own {
                    return std::fs::symlink_metadata(&absolute)
                        .and_then(|metadata| metadata.modified())
                        .is_ok_and(|modified| modified >= started);
                }
                entries.iter().filter(|(_, own)| !own).any(|(entry, _)| {
                    std::fs::symlink_metadata(entry.path()).is_ok_and(|metadata| {
                        // A file's change time moves when it is moved in,
                        // and with a rewrite, which counts too. A
                        // subdirectory's moves with what is written inside
                        // it, so it counts only once it was created.
                        if metadata.is_dir() {
                            metadata.created().is_ok_and(|born| born >= started)
                        } else {
                            changed(&metadata) >= started
                        }
                    })
                })
            })
        })
        .map(String::as_str)
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
        outputs: &[&Outputs],
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
            listings.insert(directory.clone(), self.listing(root, directory, outputs)?);
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
    fn listing(&self, root: &Path, directory: &str, outputs: &[&Outputs]) -> Result<Value> {
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
            if self.written.contains(&path) || outputs.iter().any(|outputs| outputs.matches(&path))
            {
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

/// What is at `path`, without following a link.
fn kind(path: &Path) -> &'static str {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => "symlink",
        Ok(metadata) if metadata.is_dir() => "directory",
        Ok(metadata) if metadata.is_file() => "file",
        Ok(_) => "other",
        Err(_) => "absent",
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
    outputs: &[&Outputs],
) -> Result<String> {
    key(&json!({
        "schema": "qk-recorded-v1",
        "weak": weak,
        "set": set.digest()?,
        "values": set.values(snapshot, root, outputs)?,
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
        let strong = || strong_key("weak", &set, &snapshot, &root, &[&outputs]).unwrap();
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
            set.changed_since(temp.path(), started, &[])
                .map(str::to_owned)
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
        assert_eq!(root.changed_since(removed.path(), started, &[]), None);
        std::fs::remove_file(removed.path().join("gone.txt")).unwrap();
        assert_eq!(root.changed_since(removed.path(), started, &[]), Some("."));
    }
}
