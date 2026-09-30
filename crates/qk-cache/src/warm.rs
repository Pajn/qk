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
//! from a save: this worktree's own if it has one, else another worktree's,
//! the most recent first. It is saved after successful runs, one record per
//! worktree.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Task, TaskGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::{self, Outputs};
use crate::store::{Artifact, mode, set_mode, symlink, validate_link};
use crate::{Cache, hash::digest_file};

/// When restored files say they were written. Warm state comes from another
/// checkout, so it must look older than every file in this one: tools that
/// trust timestamps, as `tsc --build` does, then check the sources against it
/// instead of taking a restored build for an up-to-date one.
pub const RESTORED_AT: std::time::SystemTime = std::time::UNIX_EPOCH;

/// How many saves are kept for a task, across worktrees and keys.
const KEPT: usize = 8;

/// A target's `qk:warm`.
#[derive(Clone, Debug, Default)]
pub struct Warm {
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
}

/// One part of a warm key.
#[derive(Clone, Debug)]
pub enum KeyPart {
    /// A workspace-relative file, by its content.
    File(String),
    /// An environment variable, by its value.
    Env(String),
}

/// The target's `qk:warm`, or `None` without one.
pub fn config(workspace: &Workspace, task: &Task) -> Result<Option<Warm>> {
    let Some(value) = task.definition.extra.get("qk:warm") else {
        return Ok(None);
    };
    let object = value.as_object().context("qk:warm must be an object")?;
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
    let directory = directory(workspace, task);
    let directory_text = directory.to_str().context("warm directory must be UTF-8")?;
    let expand = |text: &str| -> Result<String> {
        let text = paths::expand(workspace, &task.project, text)?;
        Ok(text.replace("{warm}", directory_text))
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
        let path = paths::expand(workspace, &task.project, path)?
            .trim_end_matches('/')
            .to_owned();
        paths::validate_path(&path)?;
        paths.push(path);
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
    Ok(Some(Warm {
        outputs: flag("outputs", false)?,
        paths,
        env,
        directory: uses_directory,
        max_size,
        remote: flag("remote", true)?,
        preserve_mtimes,
        portable: flag("portable", true)?,
        key,
        restore_keys,
    }))
}

/// The directory `{warm}` names for a task, in the worktree's state.
pub fn directory(workspace: &Workspace, task: &Task) -> PathBuf {
    paths::worktree_state(&workspace.root)
        .join("warm")
        .join(&blake3::hash(task.id.as_bytes()).to_hex()[..32])
}

/// Two tasks in a run cannot keep the same scratch path, since both would
/// write it.
pub fn check_overlaps(workspace: &Workspace, graph: &TaskGraph) -> Result<()> {
    let mut claimed: Vec<(String, &str)> = Vec::new();
    for (id, task) in &graph.tasks {
        let Some(warm) = config(workspace, task)? else {
            continue;
        };
        for path in warm.paths {
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

/// One group's saved files, relative to its base directory.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Group {
    artifacts: BTreeMap<String, Artifact>,
    /// Metadata of each file when saved, so an unchanged file is not read
    /// again on the next save.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    stamps: BTreeMap<String, Vec<i64>>,
    /// Each file's modification time when saved, in nanoseconds since the
    /// Unix epoch, for restores that keep it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mtimes: BTreeMap<String, i64>,
}

impl Group {
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

/// One save of a task's warm state.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Record {
    version: u32,
    task: String,
    /// The root of the worktree that saved it.
    worktree: String,
    /// The commit that worktree had checked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// When it was saved, in milliseconds since the Unix epoch.
    saved: u64,
    /// A digest of each part of the target's warm key when it was saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    key: Vec<String>,
    groups: BTreeMap<String, Group>,
}

const RECORD_VERSION: u32 = 2;

/// What this worktree last restored, per group: each file's metadata once
/// restored and the artifact it came from, so a save need not read it again.
#[derive(Default, Serialize, Deserialize)]
struct RestoredFiles {
    groups: BTreeMap<String, BTreeMap<String, (Vec<i64>, Artifact)>>,
}

/// What a restore brought back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
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
    /// What was restored before the run, if anything.
    pub restored: Option<Restored>,
    /// How long saving it after the run took, when it was saved.
    pub save_ms: Option<u64>,
}

/// A warm group: its name, the directory its paths are relative to, and
/// which paths below that directory belong to it.
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

    fn matches(&self, path: &str) -> bool {
        self.outputs
            .as_ref()
            .is_none_or(|outputs| outputs.matches(path))
    }
}

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
            base: directory(workspace, task),
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

fn current_commit(workspace: &Workspace) -> Option<String> {
    let output = paths::git(&workspace.root, &["rev-parse", "HEAD"]).ok()?;
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !commit.is_empty()).then_some(commit)
}

/// A digest of each part of the warm key, as this checkout has them.
fn key_digests(workspace: &Workspace, warm: &Warm) -> Result<Vec<String>> {
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
                KeyPart::Env(name) => match std::env::var(name) {
                    Ok(value) => format!("env:{}", hash32(&value)),
                    Err(_) => "env:unset".to_owned(),
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

fn hash32(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..32].to_owned()
}

fn nanos(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Where a worktree notes what it last restored for a task.
fn restored_files_path(workspace: &Workspace, task: &Task) -> PathBuf {
    paths::worktree_state(&workspace.root)
        .join("warm-restored")
        .join(format!("{}.json", hash32(&task.id)))
}

fn read_restored_files(workspace: &Workspace, task: &Task) -> RestoredFiles {
    fs::read(restored_files_path(workspace, task))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

impl Cache {
    /// The file-name prefix every save of the task shares.
    fn warm_prefix(&self, task: &Task) -> String {
        hash32(&format!(
            "{}\0{}\0{}",
            task.id,
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    }

    fn warm_record(&self, task: &Task, record: &Record) -> PathBuf {
        self.root.join("warm").join(format!(
            "{}-{}.json",
            self.warm_prefix(task),
            hash32(&format!("{}\0{}", record.worktree, record.key.join("\0")))
        ))
    }

    /// Every save of the task, newest first.
    fn warm_records(&self, task: &Task) -> Vec<(PathBuf, Record)> {
        let prefix = format!("{}-", self.warm_prefix(task));
        let mut records: Vec<(PathBuf, Record)> = fs::read_dir(self.root.join("warm"))
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
                    serde_json::from_slice::<Record>(&fs::read(entry.path()).ok()?).ok()?;
                (record.version == RECORD_VERSION && record.task == task.id)
                    .then(|| (entry.path(), record))
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
        task: &Task,
        warm: &Warm,
        current: &[String],
    ) -> Option<(Record, String)> {
        let own = worktree(workspace);
        let mut candidates: Vec<(u8, bool, Record)> = self
            .warm_records(task)
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

    fn store_warm(&self, task: &Task, record: &Record) -> Result<()> {
        let path = self.warm_record(task, record);
        let directory = path.parent().context("warm records have a directory")?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(self.root.join("tmp"))?;
        serde_json::to_writer(std::io::BufWriter::new(file.as_file_mut()), record)?;
        file.persist(path)?;
        Ok(())
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
    ) -> Result<Restored> {
        let mut restored = Restored::default();
        let current = key_digests(workspace, warm)?;
        let (record, source) = match self.choose_warm(workspace, task, warm, &current) {
            Some(chosen) => chosen,
            None => match self.remote_warm(workspace, task, warm, &current) {
                Some((record, branch)) => (record, format!("remote {branch}")),
                None => return Ok(restored),
            },
        };
        let keep_mtimes = warm.preserve_mtimes && source == "local";
        restored.source = source;
        let mut noted = read_restored_files(workspace, task);
        for location in locations(workspace, task, warm)? {
            let Some(group) = record.groups.get(location.name) else {
                continue;
            };
            // Local state wins: it is the newest for this checkout.
            if !location.paths()?.is_empty() {
                continue;
            }
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
            let path = restored_files_path(workspace, task);
            fs::create_dir_all(path.parent().context("restore notes have a directory")?)?;
            fs::write(path, serde_json::to_vec(&noted)?)?;
        }
        Ok(restored)
    }

    /// The task's warm state from the remote store: the current branch's, else
    /// the default branch's. It is kept locally from then on.
    fn remote_warm(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
        current: &[String],
    ) -> Option<(Record, String)> {
        let remote = self
            .remote
            .as_ref()
            .filter(|_| warm.remote && warm.portable)?;
        for branch in branches(workspace) {
            match remote.fetch_warm(&self.root, &task.id, &branch) {
                Ok(Some(bytes)) => {
                    let record =
                        serde_json::from_slice::<Record>(&bytes)
                            .ok()
                            .filter(|record| {
                                record.version == RECORD_VERSION && record.task == task.id
                            })?;
                    // A branch's save that does not suit this checkout gives
                    // way to the next branch's.
                    if key_match(warm, current, &record.key).is_none() {
                        continue;
                    }
                    if self.store_warm(task, &record).is_err() {
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
                    return None;
                }
            }
        }
        None
    }

    /// Saves the task's warm groups after a successful run, as this worktree's
    /// save. A file whose metadata matches this worktree's last save or
    /// restore keeps its blob unread.
    pub(crate) fn save_warm(&self, workspace: &Workspace, task: &Task, warm: &Warm) -> Result<()> {
        let own = worktree(workspace);
        let key = key_digests(workspace, warm)?;
        let records = self.warm_records(task);
        // This worktree's save under the same key, else under any.
        let previous = records
            .iter()
            .filter(|(_, record)| record.worktree == own)
            .min_by_key(|(_, record)| record.key != key)
            .map(|(_, record)| record);
        let noted = read_restored_files(workspace, task);
        let mut record = Record {
            version: RECORD_VERSION,
            task: task.id.clone(),
            worktree: own.clone(),
            commit: current_commit(workspace),
            saved: now_ms(),
            key,
            groups: BTreeMap::new(),
        };
        for location in locations(workspace, task, warm)? {
            if !location.base.exists() {
                continue;
            }
            let known = previous.and_then(|previous| previous.groups.get(location.name));
            let restored_files = noted.groups.get(location.name);
            let mut group = Group::default();
            for path in location.paths()? {
                paths::safe_parents(&location.base, &path)?;
                let absolute = location.base.join(&path);
                let metadata = fs::symlink_metadata(&absolute)?;
                let artifact = if metadata.file_type().is_symlink() {
                    let target = fs::read_link(&absolute)?
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
                    group.stamps.insert(path.clone(), now);
                    if let Some(modified) = metadata.modified().ok().and_then(nanos) {
                        group.mtimes.insert(path.clone(), modified);
                    }
                    match reused {
                        Some(artifact) => artifact.clone(),
                        None => Artifact::File {
                            blob: self.put_blob(&absolute)?,
                            mode: mode(&metadata),
                        },
                    }
                } else if metadata.is_dir() {
                    Artifact::Directory {
                        mode: mode(&metadata),
                    }
                } else {
                    continue;
                };
                group.artifacts.insert(path, artifact);
            }
            if let Some(limit) = warm.max_size
                && group.size(self) > limit
            {
                qk_executor::status!(
                    "qk: {}: warm {} is over its maxSize and was not saved",
                    task.id,
                    location.name
                );
                continue;
            }
            record.groups.insert(location.name.to_owned(), group);
        }
        self.store_warm(task, &record)?;
        let _ = fs::remove_file(restored_files_path(workspace, task));
        // The oldest saves beyond the kept number go; the blobs they alone
        // cite are evicted with the cache.
        let saved = self.warm_record(task, &record);
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
            && let Some(branch) = current_branch(workspace)
        {
            for group in record.groups.values_mut() {
                group.stamps.clear();
            }
            remote.upload_warm(&self.root, &task.id, &branch, serde_json::to_vec(&record)?);
        }
        Ok(())
    }
}
