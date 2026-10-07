//! Warm state: scratch state a task runs faster with, kept between runs of
//! the same task and restored before it runs, but never part of a result.
//! A target opts in with `qk:warm`:
//!
//! - `outputs: true`: the task's previous outputs, so an incremental tool
//!   such as `tsc --build` starts from its last build;
//! - `paths`: scratch directories in the workspace;
//! - `env`: variables for the task, which may name `{warm}`, a directory qk
//!   keeps for the task outside the working tree.
//!
//! A group already present on disk is left alone. Otherwise it is restored
//! from this worktree's own save. `portable: true` also permits other
//! worktrees' and remote saves. It is saved after successful runs, one record
//! per worktree.
//!
//! `qk:warm` may also be an array of such objects, each restored and saved on
//! its own, so state that relocates can be portable beside state that cannot.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Task, TaskGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::{self, Outputs};
use crate::record::{self, Artifact, WarmGroup, WarmRecord, validate_link};
use crate::store::{mode, set_mode, symlink};
use crate::{Cache, hash::digest_file};

/// When restored files say they were written. Warm state comes from another
/// checkout, so it must look older than every file in this one: tools that
/// trust timestamps, as `tsc --build` does, then check the sources against it
/// instead of taking a restored build for an up-to-date one.
pub const RESTORED_AT: std::time::SystemTime = std::time::UNIX_EPOCH;

/// What a group's identity starts with, before its name.
const GROUP: &str = "group ";

/// How many saves are kept for a task, across worktrees and keys.
const KEPT: usize = 8;

/// A target's `qk:warm`.
#[derive(Clone, Debug, Default)]
pub struct Warm {
    /// What its saves belong to: the task, or the group it shares them with.
    pub identity: String,
    pub outputs: bool,
    /// Workspace-relative scratch paths.
    pub paths: Vec<String>,
    /// Variables for the task, with tokens expanded.
    pub env: BTreeMap<String, String>,
    /// Whether any variable names `{warm}`, which makes it a group.
    pub directory: bool,
    pub max_size: Option<u64>,
    pub remote: bool,
    /// Whether a worktree's own save comes back with the modification times it
    /// was saved with, rather than [`RESTORED_AT`].
    pub preserve_mtimes: bool,
    /// Whether another worktree's save, or the remote's, may be restored.
    pub portable: bool,
    /// What decides whether a save suits this checkout, in order.
    pub key: Vec<KeyPart>,
    /// How many leading parts of the key a save must match when none matches
    /// it whole, or `None` to restore only exact matches.
    pub restore_keys: Option<usize>,
    /// Dependencies that delete the paths, which are moved aside while one
    /// runs: `target` in the task's project, or `project:target`.
    pub survive: Vec<String>,
    /// Whether saving happens after the task has reported, in the
    /// background, rather than before.
    pub background: bool,
}

/// One part of a warm key.
#[derive(Clone, Debug)]
pub enum KeyPart {
    /// A workspace-relative file, by its content.
    File(String),
    /// An environment variable, by its value.
    Env(String),
}

/// The target's `qk:warm` entries, none without one.
///
/// Entries of an array keep their saves apart, so at most one of them may
/// belong to the task itself, and the rest name groups. No two may set the
/// same variable.
pub fn config(workspace: &Workspace, task: &Task) -> Result<Vec<Warm>> {
    const SHAPE: &str = "qk:warm must be an object or an array of objects";
    let objects = match task.definition.extra.get("qk:warm") {
        None => return Ok(Vec::new()),
        Some(Value::Object(object)) => vec![object],
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| entry.as_object().context(SHAPE))
            .collect::<Result<_>>()?,
        Some(_) => bail!(SHAPE),
    };
    let mut entries: Vec<Warm> = Vec::new();
    for object in objects {
        let warm = entry(workspace, task, object)?;
        if entries.iter().any(|other| other.identity == warm.identity) {
            bail!(match warm.group() {
                Some(group) => format!("qk:warm names group {group:?} twice"),
                None => "only one qk:warm entry may go without a group".to_owned(),
            });
        }
        for name in warm.env.keys() {
            if entries.iter().any(|other| other.env.contains_key(name)) {
                bail!("qk:warm sets {name} in more than one entry");
            }
        }
        entries.push(warm);
    }
    Ok(entries)
}

/// One `qk:warm` object.
fn entry(
    workspace: &Workspace,
    task: &Task,
    object: &serde_json::Map<String, Value>,
) -> Result<Warm> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "outputs"
                | "paths"
                | "env"
                | "maxSize"
                | "remote"
                | "mtimes"
                | "portable"
                | "key"
                | "restoreKeys"
                | "group"
                | "survive"
                | "save"
        ) {
            bail!("unknown qk:warm field {key:?}");
        }
    }
    let flag = |name: &str, default: bool| -> Result<bool> {
        object.get(name).map_or(Ok(default), |value| {
            value
                .as_bool()
                .with_context(|| format!("qk:warm.{name} must be true or false"))
        })
    };
    let group = object
        .get("group")
        .map(|value| {
            value
                .as_str()
                .filter(|name| !name.is_empty())
                .context("qk:warm.group must be a name")
        })
        .transpose()?;
    if group.is_some() && (object.contains_key("outputs") || object.contains_key("paths")) {
        bail!("a qk:warm.group shares {{warm}} alone; outputs and paths belong to one task");
    }
    let identity = group.map_or_else(|| task.id.clone(), |group| format!("{GROUP}{group}"));
    const SURVIVE_SHAPE: &str = "qk:warm.survive must be an array of target names";
    let survive = object
        .get("survive")
        .map(|value| {
            value
                .as_array()
                .context(SURVIVE_SHAPE)?
                .iter()
                .map(|target| {
                    target
                        .as_str()
                        .filter(|target| !target.is_empty())
                        .map(str::to_owned)
                        .context(SURVIVE_SHAPE)
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let directory = directory(workspace, &identity);
    let directory_text = directory.to_str().context("warm directory must be UTF-8")?;
    let workspace_text = workspace
        .root
        .to_str()
        .context("workspace root must be UTF-8")?;
    let project_root = &workspace.projects[&task.project].root;
    let project_directory = if project_root == "." {
        workspace.root.clone()
    } else {
        workspace.root.join(project_root)
    };
    let project_text = project_directory
        .to_str()
        .context("project root must be UTF-8")?;
    let expand = |text: &str| -> Result<String> {
        if text.contains("{options.") || text.contains("{args.") {
            bail!("dynamic warm environment values are not supported yet");
        }
        Ok(text
            .replace("{workspaceRoot}", workspace_text)
            .replace("{projectRoot}", project_text)
            .replace("{warm}", directory_text))
    };
    let mut paths = Vec::new();
    for path in object
        .get("paths")
        .map(|value| {
            value
                .as_array()
                .context("qk:warm.paths must be an array of paths")
        })
        .transpose()?
        .into_iter()
        .flatten()
    {
        let path = path
            .as_str()
            .context("qk:warm.paths must be an array of paths")?;
        if path.contains("{warm}") {
            bail!("qk:warm.paths are workspace paths; {{warm}} is kept already");
        }
        let (excluded, path) = match path.strip_prefix('!') {
            Some(path) => (true, path),
            None => (false, path),
        };
        let path = paths::expand(workspace, &task.project, path)?
            .trim_end_matches('/')
            .to_owned();
        paths::validate_path(&path)?;
        paths.push(if excluded { format!("!{path}") } else { path });
    }
    let mut env = BTreeMap::new();
    let mut uses_directory = false;
    if let Some(values) = object.get("env") {
        for (name, value) in values
            .as_object()
            .context("qk:warm.env must be an object")?
        {
            let value = value
                .as_str()
                .context("qk:warm.env values must be strings")?;
            uses_directory |= value.contains("{warm}");
            env.insert(name.clone(), expand(value)?);
        }
    }
    let max_size = match object.get("maxSize") {
        None => None,
        Some(Value::String(text)) => Some(crate::parse_size(text)?),
        Some(Value::Number(number)) => number.as_u64(),
        Some(_) => bail!("qk:warm.maxSize must be a size such as \"2GB\""),
    };
    let preserve_mtimes = match object.get("mtimes").map(|value| value.as_str()) {
        None | Some(Some("epoch")) => false,
        Some(Some("preserve")) => true,
        Some(_) => bail!("qk:warm.mtimes must be \"epoch\" or \"preserve\""),
    };
    const KEY_SHAPE: &str = "qk:warm.key must be an array of paths and {\"env\": name} objects";
    let mut key = Vec::new();
    for part in object
        .get("key")
        .map(|value| value.as_array().context(KEY_SHAPE))
        .transpose()?
        .into_iter()
        .flatten()
    {
        key.push(match part {
            Value::String(path) => {
                let path = paths::expand(workspace, &task.project, path)?;
                paths::validate_path(&path)?;
                KeyPart::File(path)
            }
            Value::Object(object) if object.len() == 1 => KeyPart::Env(
                object
                    .get("env")
                    .and_then(Value::as_str)
                    .context(KEY_SHAPE)?
                    .to_owned(),
            ),
            _ => bail!(KEY_SHAPE),
        });
    }
    let restore_keys = object
        .get("restoreKeys")
        .map(|value| {
            value
                .as_u64()
                .and_then(|count| usize::try_from(count).ok())
                .filter(|count| *count <= key.len())
                .context("qk:warm.restoreKeys must be a count no larger than qk:warm.key")
        })
        .transpose()?;
    let background = match object.get("save").map(Value::as_str) {
        None | Some(Some("wait")) => false,
        Some(Some("background")) => true,
        Some(_) => bail!("qk:warm.save must be \"wait\" or \"background\""),
    };
    Ok(Warm {
        identity,
        outputs: flag("outputs", false)?,
        paths,
        env,
        directory: uses_directory,
        max_size,
        remote: flag("remote", true)?,
        preserve_mtimes,
        portable: flag("portable", false)?,
        key,
        restore_keys,
        survive,
        background,
    })
}

/// The directory `{warm}` names for a task or group, in the worktree's state.
fn directory(workspace: &Workspace, identity: &str) -> PathBuf {
    paths::worktree_state(&workspace.root)
        .join("warm")
        .join(&blake3::hash(identity.as_bytes()).to_hex()[..32])
}

impl Warm {
    /// The group the entry shares its saves with, if it names one.
    pub fn group(&self) -> Option<&str> {
        self.identity.strip_prefix(GROUP)
    }

    /// The scratch paths without their exclusions: what the task keeps,
    /// whether or not it is saved, and so never an input.
    pub fn kept_paths(&self) -> Vec<String> {
        self.paths
            .iter()
            .filter(|path| !path.starts_with('!'))
            .cloned()
            .collect()
    }
}

/// The scratch paths every entry keeps.
pub fn kept_paths(entries: &[Warm]) -> Vec<String> {
    entries.iter().flat_map(Warm::kept_paths).collect()
}

/// Paths moved aside while a dependency that deletes them runs, and moved
/// back, over whatever it left there, when this is dropped.
#[derive(Default)]
pub struct Kept {
    /// The stashes this dependency holds a share of.
    roots: Vec<PathBuf>,
}

impl Drop for Kept {
    fn drop(&mut self) {
        for root in self.roots.iter().rev() {
            release(root);
        }
    }
}

/// One task's paths, moved into its directory in the worktree's state.
#[derive(Default, Serialize, Deserialize)]
struct Stash {
    #[serde(skip)]
    root: PathBuf,
    #[serde(skip)]
    workspace: PathBuf,
    /// Each moved path, relative to the workspace, by the order it was moved.
    moved: Vec<String>,
}

/// A stash in use, and how many of the running dependencies share it. The
/// lock keeps another qk process from taking it for abandoned.
struct Active {
    users: usize,
    stash: Stash,
    _lock: fs::File,
}

/// The stashes this process holds, by root. Dependencies that run at once
/// share one: the first moves the paths, the last puts them back.
static ACTIVE: std::sync::Mutex<BTreeMap<PathBuf, Active>> = std::sync::Mutex::new(BTreeMap::new());

impl Stash {
    /// The ownership record for the numbered paths in a stash.
    fn manifest(root: &std::path::Path) -> PathBuf {
        root.join("moved.json")
    }

    /// Publishes ownership before moving a path, without truncating the last
    /// valid record. Sync the file before the atomic replacement.
    fn save_manifest(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.as_file().sync_all()?;
        file.persist(Self::manifest(&self.root))?;
        Ok(())
    }

    /// Moves the paths back, replacing what is there now, and removes the
    /// stash once every path is back. A path whose parents are not plain
    /// directories inside the workspace, as when a dependency put a link or a
    /// file in their place, is not touched and stays in the stash for the
    /// next run. Whether every path went back.
    fn put_back(&self) -> bool {
        let mut complete = true;
        for (index, path) in self.moved.iter().enumerate().rev() {
            let kept = self.root.join(index.to_string());
            match fs::symlink_metadata(&kept) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    complete = false;
                    qk_executor::status!(
                        "qk: could not inspect saved {path}, kept for the next run ({error})"
                    );
                    continue;
                }
            }
            let original = self.workspace.join(path);
            let result = paths::safe_parents(&self.workspace, path).and_then(|()| {
                remove(&original)?;
                if let Some(parent) = original.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&kept, &original)?;
                Ok(())
            });
            if let Err(error) = result {
                complete = false;
                qk_executor::status!(
                    "qk: could not put {path} back, kept for the next run ({error:#})"
                );
            }
        }
        if complete {
            let _ = fs::remove_dir_all(&self.root);
        }
        complete
    }
}

/// Removes a path itself, including a symlink, without following it.
fn remove(path: &std::path::Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// Gives up one dependency's share of a stash, putting the paths back when
/// it was the last.
fn release(root: &std::path::Path) {
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = active.get_mut(root) else {
        return;
    };
    entry.users -= 1;
    if entry.users > 0 {
        return;
    }
    let entry = active.remove(root).expect("present above");
    // Another dependency taking the stash again waits on the lock, not on
    // every other task's stash.
    drop(active);
    entry.stash.put_back();
}

/// Whether `entry`, from `owner`'s `survive`, names `task`.
fn names(owner: &Task, entry: &str, task: &Task) -> bool {
    match entry.split_once(':') {
        Some((project, target)) => task.project == project && task.target == target,
        None => task.project == owner.project && task.target == entry,
    }
}

/// Whether `from` depends on `on`, directly or through other tasks.
fn depends_on(graph: &TaskGraph, from: &str, on: &str) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    let mut next = vec![from];
    while let Some(id) = next.pop() {
        for dependency in graph
            .tasks
            .get(id)
            .into_iter()
            .flat_map(|task| &task.dependencies)
        {
            if dependency == on {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                next.push(dependency);
            }
        }
    }
    false
}

/// Moves aside the warm paths of every task in the graph that depends on
/// `task` and names it in `survive`, so that `task` cannot delete them.
pub fn keep_across(workspace: &Workspace, graph: &TaskGraph, task: &Task) -> Kept {
    let mut kept = Kept::default();
    for owner in graph.tasks.values() {
        let Ok(entries) = config(workspace, owner) else {
            continue;
        };
        // Only the task's own entry has paths to keep.
        let Some(warm) = entries.into_iter().find(|warm| warm.group().is_none()) else {
            continue;
        };
        if !warm.survive.iter().any(|entry| names(owner, entry, task))
            || !depends_on(graph, &owner.id, &task.id)
        {
            continue;
        }
        match acquire(workspace, owner, &warm) {
            Ok(root) => kept.roots.push(root),
            Err(error) => qk_executor::status!(
                "qk: {}: warm paths not kept across {} ({error:#})",
                owner.id,
                task.id
            ),
        }
    }
    kept
}

/// Takes a share of `owner`'s stash, moving its paths aside if no running
/// dependency has yet.
fn acquire(workspace: &Workspace, owner: &Task, warm: &Warm) -> Result<PathBuf> {
    let state = paths::worktree_state(&workspace.root).join("warm-kept");
    let root = state.join(hash32(&owner.id));
    fs::create_dir_all(&state)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join(format!("{}.lock", hash32(&owner.id))))?;
    // Only the nonblocking attempt holds ACTIVE. Waiting for another process
    // must leave this process's other stashes free to acquire and release.
    let mut active = loop {
        let mut active = ACTIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = active.get_mut(&root) {
            entry.users += 1;
            return Ok(root);
        }
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break active,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                drop(active);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    };
    // Under the lock, a stash on disk is one no run holds: one an earlier run
    // could not put back, or left when it was stopped. It goes back first.
    // What cannot go back yet, as when its parent is not a directory, stays
    // in the stash this dependency holds, beside the paths that did go back,
    // and is put back after the dependency, which may well recreate the
    // parent.
    let left = match fs::read(Stash::manifest(&root)) {
        Ok(bytes) => {
            let mut left: Stash = serde_json::from_slice(&bytes)
                .context("invalid warm stash manifest; saved paths retained for recovery")?;
            left.root = root.clone();
            left.workspace = workspace.root.clone();
            Some(left)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if root.is_dir() && fs::read_dir(&root)?.next().is_some() {
                bail!("missing warm stash manifest; saved paths retained for recovery");
            }
            None
        }
        Err(error) => return Err(error).context("cannot read warm stash; saved paths retained"),
    };
    let retained = left.filter(|left| !left.put_back());
    let stash = stash(workspace, &root, warm, retained)?;
    active.insert(
        root.clone(),
        Active {
            users: 1,
            stash,
            _lock: lock,
        },
    );
    Ok(root)
}

/// Moves the warm paths into a stash at `root`: a new one, or `retained`,
/// what an earlier run could not put back, which keeps its entries. A path
/// that is one of those, or inside or around one, stays where it is.
fn stash(
    workspace: &Workspace,
    root: &std::path::Path,
    warm: &Warm,
    retained: Option<Stash>,
) -> Result<Stash> {
    let mut stash = match retained {
        Some(retained) => retained,
        None => {
            let _ = fs::remove_dir_all(root);
            Stash {
                root: root.to_owned(),
                workspace: workspace.root.clone(),
                moved: Vec::new(),
            }
        }
    };
    let held: Vec<String> = stash
        .moved
        .iter()
        .enumerate()
        .filter_map(
            |(index, path)| match fs::symlink_metadata(root.join(index.to_string())) {
                Ok(_) => Some(Ok(path.clone())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => Some(Err(error).context("cannot inspect retained warm path")),
            },
        )
        .collect::<Result<_>>()?;
    let overlaps = |path: &str| {
        held.iter().any(|held| {
            held == path
                || path.starts_with(&format!("{held}/"))
                || held.starts_with(&format!("{path}/"))
        })
    };
    // Each path on its own: one whose parents are not usable now, as one
    // retained above may have, has nothing live there to move.
    let mut kept = std::collections::BTreeSet::new();
    for path in warm.kept_paths() {
        if let Ok(found) = Outputs::from_paths(std::slice::from_ref(&path))
            .and_then(|outputs| outputs.paths(&workspace.root))
        {
            kept.extend(found);
        }
    }
    // The topmost paths only: moving one moves what is below it.
    let topmost: Vec<String> = kept
        .iter()
        .filter(|path| {
            !std::path::Path::new(path.as_str())
                .ancestors()
                .skip(1)
                .any(|ancestor| {
                    ancestor
                        .to_str()
                        .is_some_and(|ancestor| kept.contains(ancestor))
                })
        })
        .filter(|path| !overlaps(path))
        .cloned()
        .collect();
    if topmost.is_empty() {
        return Ok(stash);
    }
    fs::create_dir_all(root)?;
    for path in topmost {
        let moved = paths::safe_parents(&workspace.root, &path).and_then(|()| {
            stash.moved.push(path.clone());
            stash.save_manifest()?;
            // An entry put back earlier left its slot empty, and a retained
            // one's slot is taken; a new one goes at the end either way.
            fs::rename(
                workspace.root.join(&path),
                root.join((stash.moved.len() - 1).to_string()),
            )
            .inspect_err(|_| {
                stash.moved.pop();
            })?;
            Ok(())
        });
        if let Err(error) = moved {
            // What was moved before goes back, rather than waiting on a run
            // that holds no share of it.
            stash.save_manifest()?;
            stash.put_back();
            return Err(error);
        }
    }
    Ok(stash)
}

/// Two tasks in a run cannot keep the same scratch path, since both would
/// write it.
pub fn check_overlaps(workspace: &Workspace, graph: &TaskGraph) -> Result<()> {
    let mut claimed: Vec<(String, &str)> = Vec::new();
    for (id, task) in &graph.tasks {
        for path in kept_paths(&config(workspace, task)?) {
            if let Some((_, other)) = claimed.iter().find(|(known, _)| {
                known == &path
                    || known.starts_with(&format!("{path}/"))
                    || path.starts_with(&format!("{known}/"))
            }) {
                bail!("{id} and {other} both keep {path} as warm state");
            }
            claimed.push((path, id));
        }
    }
    Ok(())
}

impl WarmGroup {
    /// Total saved file bytes, excluding directories and links.
    fn size(&self, cache: &Cache) -> u64 {
        self.artifacts
            .values()
            .filter_map(|artifact| match artifact {
                Artifact::File { blob, .. } => {
                    fs::metadata(cache.root.join("blobs").join(blob)).ok()
                }
                _ => None,
            })
            .map(|metadata| metadata.len())
            .sum()
    }
}

/// What this worktree last restored, per group: each file's metadata once
/// restored and the artifact it came from, so a save need not read it again.
#[derive(Default, Serialize, Deserialize)]
struct RestoredFiles {
    groups: BTreeMap<String, BTreeMap<String, (Vec<i64>, Artifact)>>,
}

/// What a restore brought back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    /// The group the entry shares its saves with, absent for the task's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `local` for this worktree's own save, `worktree <root>` for another
    /// worktree's, or `remote <branch>`.
    pub source: String,
    pub groups: Vec<String>,
    pub files: usize,
    pub bytes: u64,
}

/// What warm state did for one run of a task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmReport {
    /// What each entry restored before the run, for those that restored any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restored: Vec<Restored>,
    /// The groups already on disk before the run, which were left as they
    /// were: `outputs`, `paths` and `directory` for the task's own entry, and
    /// a named group's name for its directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<String>,
    /// How long saving the entries after the run took, when any was saved
    /// before the task reported.
    pub save_ms: Option<u64>,
    /// Whether any entry was saved in the background, after the task reported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

/// A warm group: its name, the directory its paths are relative to, and
/// which paths below that directory belong to it.
#[derive(Clone)]
struct Location {
    name: &'static str,
    base: PathBuf,
    /// `None` for everything below `base`.
    outputs: Option<Outputs>,
}

impl Location {
    /// The group's paths on disk, relative to `base`.
    fn paths(&self) -> Result<std::collections::BTreeSet<String>> {
        if !self.base.exists() {
            return Ok(Default::default());
        }
        match &self.outputs {
            Some(outputs) => outputs.paths(&self.base),
            None => {
                let mut paths = std::collections::BTreeSet::new();
                for entry in walkdir::WalkDir::new(&self.base)
                    .min_depth(1)
                    .follow_links(false)
                {
                    let entry = entry?;
                    paths.insert(paths::relative(&self.base, entry.path())?);
                }
                Ok(paths)
            }
        }
    }

    /// Whether a relative path belongs to this configured group.
    fn matches(&self, path: &str) -> bool {
        self.outputs
            .as_ref()
            .is_none_or(|outputs| outputs.matches(path))
    }
}

/// Resolves output, scratch and external environment groups for this task.
fn locations(workspace: &Workspace, task: &Task, warm: &Warm) -> Result<Vec<Location>> {
    let mut locations = Vec::new();
    if warm.outputs {
        locations.push(Location {
            name: "outputs",
            base: workspace.root.clone(),
            outputs: Some(Outputs::new(workspace, task)?),
        });
    }
    if !warm.paths.is_empty() {
        locations.push(Location {
            name: "paths",
            base: workspace.root.clone(),
            outputs: Some(Outputs::from_paths(&warm.paths)?),
        });
    }
    if warm.directory {
        locations.push(Location {
            name: "directory",
            base: directory(workspace, &warm.identity),
            outputs: None,
        });
    }
    Ok(locations)
}

/// The branch being built: from CI when it checks out a detached head, else
/// from git. `None` when there is no branch to name.
fn current_branch(workspace: &Workspace) -> Option<String> {
    for name in ["GITHUB_HEAD_REF", "GITHUB_REF_NAME"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    let output = paths::git(&workspace.root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// The branches to take warm state from, in order: the current one, then the
/// workspace's default branch.
fn branches(workspace: &Workspace) -> Vec<String> {
    let default = workspace
        .config
        .default_base
        .clone()
        .unwrap_or_else(|| "main".into());
    let default = default
        .strip_prefix("origin/")
        .unwrap_or(&default)
        .to_owned();
    let mut branches: Vec<String> = current_branch(workspace).into_iter().collect();
    if !branches.contains(&default) {
        branches.push(default);
    }
    branches
}

/// Metadata used to detect files unchanged since their save or restore.
fn stamp(metadata: &fs::Metadata) -> Vec<i64> {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(-1, |duration| duration.as_nanos() as i64);
    #[allow(unused_mut)]
    let mut stamp = vec![metadata.len() as i64, modified, i64::from(mode(metadata))];
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        stamp.extend([
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.ino() as i64,
        ]);
    }
    stamp
}

/// This worktree's root, as records name it.
fn worktree(workspace: &Workspace) -> String {
    fs::canonicalize(&workspace.root)
        .unwrap_or_else(|_| workspace.root.clone())
        .to_string_lossy()
        .into_owned()
}

/// The checkout commit, absent outside Git workspaces.
fn current_commit(workspace: &Workspace) -> Option<String> {
    let output = paths::git(&workspace.root, &["rev-parse", "HEAD"]).ok()?;
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !commit.is_empty()).then_some(commit)
}

/// A digest of each part of the warm key, as this checkout and the task's
/// environment have them.
fn key_digests(
    workspace: &Workspace,
    warm: &Warm,
    prepared: &qk_executor::PreparedTask,
) -> Result<Vec<String>> {
    warm.key
        .iter()
        .map(|part| {
            Ok(match part {
                KeyPart::File(path) => {
                    paths::safe_parents(&workspace.root, path)?;
                    let absolute = workspace.root.join(path);
                    if absolute.is_file() {
                        format!("file:{}", digest_file(&absolute)?)
                    } else {
                        "file:absent".to_owned()
                    }
                }
                // As the task's process gets them: its execution variables
                // over its environment, and nothing else.
                KeyPart::Env(name) => match prepared
                    .execution
                    .get(std::ffi::OsStr::new(name))
                    .or_else(|| prepared.env.get(std::ffi::OsStr::new(name)))
                {
                    Some(value) => format!(
                        "env:{}",
                        blake3::hash(value.as_encoded_bytes()).to_hex()[..32].to_owned()
                    ),
                    None => "env:unset".to_owned(),
                },
            })
        })
        .collect()
}

/// How well a save's key suits this checkout: `Some(0)` when it matches
/// whole, `Some(1)` when it matches only in the leading parts `restoreKeys`
/// allows, `None` when it does not suit.
fn key_match(warm: &Warm, current: &[String], saved: &[String]) -> Option<u8> {
    if saved == current {
        return Some(0);
    }
    let count = warm.restore_keys?;
    (saved.len() == current.len() && saved[..count] == current[..count]).then_some(1)
}

/// How many commits `commit` is behind the checkout's `HEAD`, or `None` when
/// it is not an ancestor of it.
fn behind(workspace: &Workspace, commit: &str) -> Option<u64> {
    let ancestor = paths::git(
        &workspace.root,
        &["merge-base", "--is-ancestor", commit, "HEAD"],
    )
    .ok()?;
    if !ancestor.status.success() {
        return None;
    }
    let output = paths::git(
        &workspace.root,
        &["rev-list", "--count", &format!("{commit}..HEAD")],
    )
    .ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// A compact stable filename component for task and group identities.
fn hash32(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..32].to_owned()
}

/// A representable timestamp for restoring saved modification times.
fn nanos(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

/// The save time used to order records across worktrees.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Where a worktree notes what it last restored for a task or group.
fn restored_files_path(workspace: &Workspace, warm: &Warm) -> PathBuf {
    paths::worktree_state(&workspace.root)
        .join("warm-restored")
        .join(format!("{}.json", hash32(&warm.identity)))
}

/// Reads optional restore metadata; a missing note forces fresh file reads.
fn read_restored_files(workspace: &Workspace, warm: &Warm) -> RestoredFiles {
    fs::read(restored_files_path(workspace, warm))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

impl Cache {
    /// The file-name prefix every save of a task or group shares.
    fn warm_prefix(&self, identity: &str) -> String {
        hash32(&format!(
            "{identity}\0{}\0{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    }

    /// Serializes restoring and saving a task's or group's state, which the
    /// tasks of a group may do at once.
    fn lock_warm(&self, warm: &Warm) -> Result<fs::File> {
        let directory = self.root.join("locks");
        fs::create_dir_all(&directory)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("warm-{}", self.warm_prefix(&warm.identity))))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn warm_record(&self, record: &WarmRecord) -> PathBuf {
        self.root.join("warm").join(format!(
            "{}-{}.json",
            self.warm_prefix(&record.task),
            hash32(&format!("{}\0{}", record.worktree, record.key.join("\0")))
        ))
    }

    /// Every save of the task or group, newest first.
    fn warm_records(&self, warm: &Warm) -> Vec<(PathBuf, WarmRecord)> {
        let prefix = format!("{}-", self.warm_prefix(&warm.identity));
        let mut records: Vec<(PathBuf, WarmRecord)> = fs::read_dir(self.root.join("warm"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
            })
            .filter_map(|entry| {
                let record =
                    WarmRecord::read(&fs::read(entry.path()).ok()?, &warm.identity).ok()?;
                Some((entry.path(), record))
            })
            .collect();
        records.sort_by_key(|(_, record)| std::cmp::Reverse(record.saved));
        records
    }

    /// The save to restore from, and how to name where it came from. A save
    /// whose key matches whole comes before one that matches in part; then
    /// this worktree's own before another's, which only a portable target
    /// restores; then the one made at the commit nearest behind `HEAD`; then
    /// the newest.
    fn choose_warm(
        &self,
        workspace: &Workspace,
        warm: &Warm,
        current: &[String],
    ) -> Option<(WarmRecord, String)> {
        let own = worktree(workspace);
        let mut candidates: Vec<(u8, bool, WarmRecord)> = self
            .warm_records(warm)
            .into_iter()
            .filter_map(|(_, record)| {
                let quality = key_match(warm, current, &record.key)?;
                let other = record.worktree != own;
                (!other || warm.portable).then_some((quality, other, record))
            })
            .collect();
        // Newest first within each rank, as warm_records returned them.
        candidates.sort_by_key(|(quality, other, _)| (*quality, *other));
        let rank = candidates
            .first()
            .map(|(quality, other, _)| (*quality, *other))?;
        let tied = candidates
            .iter()
            .take_while(|(quality, other, _)| (*quality, *other) == rank)
            .count();
        let index = if tied > 1 {
            (0..tied)
                .min_by_key(|&index| {
                    let behind = candidates[index]
                        .2
                        .commit
                        .as_deref()
                        .and_then(|commit| behind(workspace, commit));
                    (behind.is_none(), behind, index)
                })
                .unwrap_or(0)
        } else {
            0
        };
        let (_, other, record) = candidates.swap_remove(index);
        let source = if other {
            format!("worktree {}", record.worktree)
        } else {
            "local".to_owned()
        };
        Some((record, source))
    }

    fn store_warm(&self, record: &WarmRecord) -> Result<()> {
        let path = self.warm_record(record);
        let directory = path.parent().context("warm records have a directory")?;
        fs::create_dir_all(directory)?;
        record::publish(&self.root, &path, record)
    }

    /// Restores each warm group not already on disk from the save
    /// [`Cache::choose_warm`] picks, or the remote's, and notes what it
    /// restored so the next save can tell which files are unchanged. Files are
    /// dated [`RESTORED_AT`], unless the target keeps modification times and
    /// the save is this worktree's own.
    pub(crate) fn restore_warm(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        prepared: &qk_executor::PreparedTask,
    ) -> Result<(Restored, Vec<String>)> {
        let mut restored = Restored {
            group: warm.group().map(str::to_owned),
            ..Restored::default()
        };
        let _lock = self.lock_warm(warm)?;
        // Local state wins: it is the newest for this checkout.
        let mut present = Vec::new();
        let mut missing = Vec::new();
        for location in locations(workspace, task, warm)? {
            if location.paths()?.is_empty() {
                missing.push(location);
            } else {
                present.push(warm.group().map_or(location.name, |group| group).to_owned());
            }
        }
        if missing.is_empty() {
            return Ok((restored, present));
        }
        let current = key_digests(workspace, warm, prepared)?;
        let (record, source) = match self.choose_warm(workspace, warm, &current) {
            Some(chosen) => chosen,
            None => match self.remote_warm(workspace, task, warm, &current) {
                Some((record, branch)) => (record, format!("remote {branch}")),
                None => return Ok((restored, present)),
            },
        };
        let keep_mtimes = warm.preserve_mtimes && source == "local";
        restored.source = source;
        let mut noted = read_restored_files(workspace, warm);
        for location in missing {
            let Some(group) = record.groups.get(location.name) else {
                continue;
            };
            fs::create_dir_all(&location.base)?;
            let mut files = BTreeMap::new();
            for (path, artifact) in &group.artifacts {
                paths::safe_parents(&location.base, path)?;
                if !location.matches(path) {
                    bail!("warm state for {} holds {path}, outside its paths", task.id);
                }
                let destination = location.base.join(path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                match artifact {
                    Artifact::File { blob, mode } => {
                        let source = self.root.join("blobs").join(blob);
                        if digest_file(&source)? != *blob {
                            bail!("warm state for {} has a corrupt blob", task.id);
                        }
                        fs::copy(&source, &destination)?;
                        let modified = keep_mtimes
                            .then(|| group.mtimes.get(path))
                            .flatten()
                            .and_then(|nanos| u64::try_from(*nanos).ok())
                            .map_or(RESTORED_AT, |nanos| {
                                UNIX_EPOCH + Duration::from_nanos(nanos)
                            });
                        fs::File::options()
                            .write(true)
                            .open(&destination)?
                            .set_modified(modified)?;
                        set_mode(&destination, *mode)?;
                        restored.files += 1;
                        restored.bytes += fs::metadata(&destination)?.len();
                        files.insert(
                            path.clone(),
                            (
                                stamp(&fs::symlink_metadata(&destination)?),
                                artifact.clone(),
                            ),
                        );
                    }
                    Artifact::Directory { mode } => {
                        fs::create_dir_all(&destination)?;
                        set_mode(&destination, *mode)?;
                    }
                    Artifact::Symlink { target, directory } => {
                        validate_link(path, target)?;
                        symlink(target, &destination, *directory)?;
                    }
                }
            }
            noted.groups.insert(location.name.to_owned(), files);
            restored.groups.push(location.name.to_owned());
        }
        if !restored.groups.is_empty() {
            let path = restored_files_path(workspace, warm);
            fs::create_dir_all(path.parent().context("restore notes have a directory")?)?;
            fs::write(path, serde_json::to_vec(&noted)?)?;
        }
        Ok((restored, present))
    }

    /// The task's warm state from the remote store: the current branch's, else
    /// the default branch's. It is kept locally from then on.
    fn remote_warm(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        current: &[String],
    ) -> Option<(WarmRecord, String)> {
        let remote = self
            .remote
            .as_ref()
            .filter(|_| warm.remote && warm.portable)?;
        for branch in branches(workspace) {
            match remote.fetch_warm(&self.root, &warm.identity, &branch) {
                Ok(Some(bytes)) => {
                    let Some(record) = WarmRecord::read(&bytes, &warm.identity).ok() else {
                        continue;
                    };
                    // A branch's save that does not suit this checkout gives
                    // way to the next branch's.
                    if key_match(warm, current, &record.key).is_none() {
                        continue;
                    }
                    if self.store_warm(&record).is_err() {
                        return None;
                    }
                    return Some((record, branch));
                }
                Ok(None) => {}
                Err(error) => {
                    qk_executor::status!(
                        "qk: {}: remote warm state unavailable ({error:#})",
                        task.id
                    );
                    // An invalid record or unavailable blob on this branch
                    // does not rule out a usable default-branch save.
                    continue;
                }
            }
        }
        None
    }

    /// Saves the task's warm groups after a successful run, as this worktree's
    /// save.
    pub(crate) fn save_warm(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        prepared: &qk_executor::PreparedTask,
    ) -> Result<()> {
        self.save_prepared(&Save::prepare(workspace, task, warm, prepared)?)
    }

    /// Saves the task's warm groups on a thread of its own, which
    /// [`Cache::finish`] waits for.
    pub(crate) fn save_warm_in_background(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        prepared: &qk_executor::PreparedTask,
    ) -> Result<()> {
        let save = Save::prepare(workspace, task, warm, prepared)?;
        let cache = Cache {
            remote: self.remote.clone(),
            ..Cache::new(self.root.clone())
        };
        let handle = std::thread::spawn(move || {
            if let Err(error) = cache.save_prepared(&save) {
                qk_executor::status!("qk: {}: warm state not saved ({error:#})", save.task);
            }
        });
        self.saves.lock().unwrap().push(handle);
        Ok(())
    }

    /// Saves what [`Save::prepare`] gathered. A file whose metadata matches
    /// this worktree's last save or restore keeps its blob unread; one that
    /// is gone by the time it is read, as another task may remove it, is left
    /// out.
    fn save_prepared(&self, save: &Save) -> Result<()> {
        let warm = &save.warm;
        let _lock = self.lock_warm(warm)?;
        let records = self.warm_records(warm);
        // This worktree's save under the same key, else under any.
        let previous = records
            .iter()
            .filter(|(_, record)| record.worktree == save.worktree)
            .min_by_key(|(_, record)| record.key != save.key)
            .map(|(_, record)| record);
        let noted: RestoredFiles = fs::read(&save.notes)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut record = WarmRecord::new(
            warm.identity.clone(),
            save.worktree.clone(),
            save.commit.clone(),
            now_ms(),
            save.key.clone(),
        );
        let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
        for location in &save.locations {
            if !location.base.exists() {
                continue;
            }
            let known = previous.and_then(|previous| previous.groups.get(location.name));
            let restored_files = noted.groups.get(location.name);
            let mut group = WarmGroup::default();
            let mut pending = Vec::new();
            for path in location.paths()? {
                paths::safe_parents(&location.base, &path)?;
                let absolute = location.base.join(&path);
                let metadata = match fs::symlink_metadata(&absolute) {
                    Err(error) if gone(&error) => continue,
                    result => result?,
                };
                let artifact = if metadata.file_type().is_symlink() {
                    let target = match fs::read_link(&absolute) {
                        Err(error) if gone(&error) => continue,
                        result => result?,
                    };
                    let target = target
                        .to_str()
                        .context("symlink target must be UTF-8")?
                        .to_owned();
                    validate_link(&path, &target)?;
                    Artifact::Symlink {
                        target,
                        directory: absolute.is_dir(),
                    }
                } else if metadata.is_file() {
                    let now = stamp(&metadata);
                    let saved = known.and_then(|known| {
                        (known.stamps.get(&path) == Some(&now))
                            .then(|| known.artifacts.get(&path))
                            .flatten()
                    });
                    let restored = restored_files
                        .and_then(|files| files.get(&path))
                        .filter(|(stamp, _)| *stamp == now)
                        .map(|(_, artifact)| artifact);
                    let reused = saved.or(restored).filter(|artifact| match artifact {
                        Artifact::File { blob, .. } => self.root.join("blobs").join(blob).is_file(),
                        _ => false,
                    });
                    let Some(artifact) = reused.cloned() else {
                        pending.push((path, absolute, metadata));
                        continue;
                    };
                    group.stamps.insert(path.clone(), now);
                    if let Some(modified) = metadata.modified().ok().and_then(nanos) {
                        group.mtimes.insert(path.clone(), modified);
                    }
                    artifact
                } else if metadata.is_dir() {
                    Artifact::Directory {
                        mode: mode(&metadata),
                    }
                } else {
                    continue;
                };
                group.artifacts.insert(path, artifact);
            }
            let sources: Vec<_> = pending
                .iter()
                .map(|(_, absolute, _)| absolute.clone())
                .collect();
            for ((path, _, metadata), result) in pending.into_iter().zip(self.put_blobs(&sources)) {
                let blob = match result {
                    Ok(blob) => blob,
                    Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(gone) => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                group.stamps.insert(path.clone(), stamp(&metadata));
                if let Some(modified) = metadata.modified().ok().and_then(nanos) {
                    group.mtimes.insert(path.clone(), modified);
                }
                group.artifacts.insert(
                    path,
                    Artifact::File {
                        blob,
                        mode: mode(&metadata),
                    },
                );
            }
            if let Some(limit) = warm.max_size
                && group.size(self) > limit
            {
                qk_executor::status!(
                    "qk: {}: warm {} is over its maxSize and was not saved",
                    save.task,
                    location.name
                );
                continue;
            }
            record.groups.insert(location.name.to_owned(), group);
        }
        self.store_warm(&record)?;
        let _ = fs::remove_file(&save.notes);
        // The oldest saves beyond the kept number go; the blobs they alone
        // cite are evicted with the cache.
        let saved = self.warm_record(&record);
        for (path, _) in records
            .into_iter()
            .filter(|(path, _)| *path != saved)
            .skip(KEPT - 1)
        {
            let _ = fs::remove_file(path);
        }
        // Stamps describe this machine's files; the remote record goes without.
        if warm.remote
            && warm.portable
            && let Some(remote) = &self.remote
            && let Some(branch) = &save.branch
        {
            for group in record.groups.values_mut() {
                group.stamps.clear();
            }
            remote.upload_warm(
                &self.root,
                &warm.identity,
                branch,
                serde_json::to_vec(&record)?,
            );
        }
        Ok(())
    }
}

/// What a save needs from the workspace, gathered where the task ran so that
/// the save can run elsewhere.
struct Save {
    task: String,
    warm: Warm,
    locations: Vec<Location>,
    worktree: String,
    commit: Option<String>,
    branch: Option<String>,
    key: Vec<String>,
    notes: PathBuf,
}

impl Save {
    /// Captures the effective key and locations before a background save.
    fn prepare(
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        prepared: &qk_executor::PreparedTask,
    ) -> Result<Self> {
        Ok(Self {
            task: task.id.clone(),
            warm: warm.clone(),
            locations: locations(workspace, task, warm)?,
            worktree: worktree(workspace),
            commit: current_commit(workspace),
            branch: current_branch(workspace),
            key: key_digests(workspace, warm, prepared)?,
            notes: restored_files_path(workspace, warm),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qk_taskgraph::Request;
    use serde_json::json;

    /// Two independent owners let a blocked file lock contend with a release.
    fn fixture() -> (tempfile::TempDir, Workspace, TaskGraph) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("nx.json"), "{}").unwrap();
        fs::write(
            temp.path().join("project.json"),
            json!({
                "name": "app", "targets": {
                    "one": {"command": "true", "qk:warm": {"paths": ["scratch-one"]}},
                    "two": {"command": "true", "qk:warm": {"paths": ["scratch-two"]}}
                }
            })
            .to_string(),
        )
        .unwrap();
        for path in ["scratch-one", "scratch-two"] {
            fs::create_dir(temp.path().join(path)).unwrap();
            fs::write(temp.path().join(path).join("state"), "saved").unwrap();
        }
        let workspace = Workspace::load(temp.path()).unwrap();
        let graph = TaskGraph::build(
            &workspace,
            &[
                Request::parse("app:one").unwrap(),
                Request::parse("app:two").unwrap(),
            ],
        )
        .unwrap();
        (temp, workspace, graph)
    }

    /// The `qk:warm` entries of an `app:build` whose target declares `warm`.
    fn entries(warm: Value) -> Result<Vec<Warm>> {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("nx.json"), "{}").unwrap();
        fs::write(
            temp.path().join("project.json"),
            json!({"name": "app", "targets": {"build": {"command": "true", "qk:warm": warm}}})
                .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(temp.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        config(&workspace, &graph.tasks["app:build"])
    }

    #[test]
    fn an_array_holds_the_task_entry_beside_groups() {
        let entries = entries(json!([
            {"group": "metro", "portable": true, "env": {"METRO_CACHE_ROOT": "{warm}/metro"}},
            {"paths": ["build"], "mtimes": "preserve"}
        ]))
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].group(), Some("metro"));
        assert!(entries[0].portable && entries[0].directory);
        assert_eq!(entries[1].group(), None);
        assert!(!entries[1].portable && entries[1].preserve_mtimes);
        assert_eq!(kept_paths(&entries), ["build"]);
    }

    #[test]
    fn entries_cannot_share_saves_or_variables() {
        let error = |warm| format!("{:#}", entries(warm).unwrap_err());
        assert_eq!(
            error(json!([{"paths": ["one"]}, {"paths": ["two"]}])),
            "only one qk:warm entry may go without a group"
        );
        assert_eq!(
            error(json!([{"group": "metro"}, {"group": "metro"}])),
            "qk:warm names group \"metro\" twice"
        );
        assert_eq!(
            error(json!([
                {"group": "one", "env": {"CACHE": "{warm}/one"}},
                {"group": "two", "env": {"CACHE": "{warm}/two"}}
            ])),
            "qk:warm sets CACHE in more than one entry"
        );
        assert_eq!(
            error(json!(["build"])),
            "qk:warm must be an object or an array of objects"
        );
    }

    /// An external owner lock must not prevent another stash's final release.
    #[test]
    fn waiting_for_an_owner_does_not_block_other_releases() {
        let (_temp, workspace, graph) = fixture();
        let one = &graph.tasks["app:one"];
        let two = &graph.tasks["app:two"];
        let first = acquire(&workspace, one, &config(&workspace, one).unwrap()[0]).unwrap();
        let state = paths::worktree_state(&workspace.root).join("warm-kept");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join(format!("{}.lock", hash32(&two.id))))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let (released, received) = std::sync::mpsc::channel();
        let completed = std::thread::scope(|scope| {
            scope.spawn(|| {
                let second =
                    acquire(&workspace, two, &config(&workspace, two).unwrap()[0]).unwrap();
                release(&second);
            });
            std::thread::sleep(Duration::from_millis(100));
            scope.spawn(|| {
                release(&first);
                released.send(()).unwrap();
            });
            let completed = received.recv_timeout(Duration::from_secs(2)).is_ok();
            fs2::FileExt::unlock(&lock).unwrap();
            completed
        });
        assert!(
            completed,
            "a blocked owner lock prevented an unrelated release"
        );
        assert_eq!(
            fs::read_to_string(workspace.root.join("scratch-one/state")).unwrap(),
            "saved"
        );
    }

    /// Damaged or absent ownership metadata never makes saved contents disposable.
    #[test]
    fn unreadable_ownership_preserves_saved_paths() {
        let (_temp, workspace, graph) = fixture();
        let owner = &graph.tasks["app:one"];
        let warm = config(&workspace, owner).unwrap().remove(0);
        let root = acquire(&workspace, owner, &warm).unwrap();
        let entry = ACTIVE.lock().unwrap().remove(&root).unwrap();
        drop(entry); // Simulate a stopped process, without returning its paths.
        fs::write(Stash::manifest(&root), "{").unwrap();
        assert!(acquire(&workspace, owner, &warm).is_err());
        assert_eq!(fs::read_to_string(root.join("0/state")).unwrap(), "saved");
        fs::remove_file(Stash::manifest(&root)).unwrap();
        assert!(acquire(&workspace, owner, &warm).is_err());
        assert_eq!(fs::read_to_string(root.join("0/state")).unwrap(), "saved");
        fs::create_dir(Stash::manifest(&root)).unwrap();
        assert!(acquire(&workspace, owner, &warm).is_err());
        assert_eq!(fs::read_to_string(root.join("0/state")).unwrap(), "saved");
    }

    /// Inspection errors are incomplete recovery, not evidence of an empty slot.
    #[cfg(unix)]
    #[test]
    fn inspection_errors_keep_recovery_incomplete() {
        let (_temp, workspace, graph) = fixture();
        let owner = &graph.tasks["app:one"];
        let warm = config(&workspace, owner).unwrap().remove(0);
        let root = acquire(&workspace, owner, &warm).unwrap();
        let entry = ACTIVE.lock().unwrap().remove(&root).unwrap();
        let saved = root.with_extension("saved");
        fs::rename(&root, &saved).unwrap();
        fs::write(&root, "blocked").unwrap(); // Numbered slot inspection returns ENOTDIR.
        assert!(!entry.stash.put_back());
        let retained = Stash {
            root: root.clone(),
            workspace: workspace.root.clone(),
            moved: entry.stash.moved.clone(),
        };
        assert!(stash(&workspace, &root, &warm, Some(retained)).is_err());
        assert_eq!(fs::read_to_string(saved.join("0/state")).unwrap(), "saved");
        assert_eq!(fs::read_to_string(&root).unwrap(), "blocked");
    }
}
