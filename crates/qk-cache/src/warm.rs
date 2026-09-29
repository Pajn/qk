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
//! from the most recent save for the task. It is saved after successful runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

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
            "outputs" | "paths" | "env" | "maxSize" | "remote"
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
    Ok(Some(Warm {
        outputs: flag("outputs", false)?,
        paths,
        env,
        directory: uses_directory,
        max_size,
        remote: flag("remote", true)?,
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
    /// Metadata of each file when saved or restored, so an unchanged file is
    /// not read again on the next save.
    stamps: BTreeMap<String, Vec<i64>>,
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

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Record {
    version: u32,
    task: String,
    groups: BTreeMap<String, Group>,
}

/// What a restore brought back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    /// `local`, or `remote <branch>`.
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

impl Cache {
    fn warm_record(&self, task: &Task) -> PathBuf {
        let identity = format!(
            "{}\0{}\0{}",
            task.id,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        self.root.join("warm").join(format!(
            "{}.json",
            &blake3::hash(identity.as_bytes()).to_hex()[..32]
        ))
    }

    pub(crate) fn load_warm(&self, task: &Task) -> Option<Record> {
        let bytes = fs::read(self.warm_record(task)).ok()?;
        serde_json::from_slice::<Record>(&bytes)
            .ok()
            .filter(|record| record.version == 1 && record.task == task.id)
    }

    fn store_warm(&self, task: &Task, record: &Record) -> Result<()> {
        let path = self.warm_record(task);
        let directory = path.parent().context("warm records have a directory")?;
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(self.root.join("tmp"))?;
        serde_json::to_writer(std::io::BufWriter::new(file.as_file_mut()), record)?;
        file.persist(path)?;
        Ok(())
    }

    /// Restores each warm group not already on disk from the task's last save,
    /// and records what the restore left, so the next save can tell which
    /// files are unchanged. Every file is dated [`RESTORED_AT`].
    pub(crate) fn restore_warm(
        &self,
        workspace: &Workspace,
        task: &Task,
        warm: &Warm,
    ) -> Result<Restored> {
        let mut restored = Restored::default();
        let (mut record, source) = match self.load_warm(task) {
            Some(record) => (record, "local".to_owned()),
            None => match self.remote_warm(workspace, task, warm) {
                Some((record, branch)) => (record, format!("remote {branch}")),
                None => return Ok(restored),
            },
        };
        restored.source = source;
        for location in locations(workspace, task, warm)? {
            let Some(group) = record.groups.get_mut(location.name) else {
                continue;
            };
            // Local state wins: it is the newest for this checkout.
            if !location.paths()?.is_empty() {
                continue;
            }
            fs::create_dir_all(&location.base)?;
            let mut stamps = BTreeMap::new();
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
                        fs::File::options()
                            .write(true)
                            .open(&destination)?
                            .set_modified(RESTORED_AT)?;
                        set_mode(&destination, *mode)?;
                        restored.files += 1;
                        restored.bytes += fs::metadata(&destination)?.len();
                        stamps.insert(path.clone(), stamp(&fs::symlink_metadata(&destination)?));
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
            group.stamps = stamps;
            restored.groups.push(location.name.to_owned());
        }
        if !restored.groups.is_empty() {
            self.store_warm(task, &record)?;
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
    ) -> Option<(Record, String)> {
        let remote = self.remote.as_ref().filter(|_| warm.remote)?;
        for branch in branches(workspace) {
            match remote.fetch_warm(&self.root, &task.id, &branch) {
                Ok(Some(bytes)) => {
                    let record = serde_json::from_slice::<Record>(&bytes)
                        .ok()
                        .filter(|record| record.version == 1 && record.task == task.id)?;
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

    /// Saves the task's warm groups after a successful run. A file whose
    /// metadata matches the last save or restore keeps its blob unread.
    pub(crate) fn save_warm(&self, workspace: &Workspace, task: &Task, warm: &Warm) -> Result<()> {
        let previous = self.load_warm(task).unwrap_or_default();
        let mut record = Record {
            version: 1,
            task: task.id.clone(),
            groups: BTreeMap::new(),
        };
        for location in locations(workspace, task, warm)? {
            if !location.base.exists() {
                continue;
            }
            let known = previous.groups.get(location.name);
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
                    let reused = known.and_then(|known| {
                        (known.stamps.get(&path) == Some(&now))
                            .then(|| known.artifacts.get(&path))
                            .flatten()
                            .filter(|artifact| match artifact {
                                Artifact::File { blob, .. } => {
                                    self.root.join("blobs").join(blob).is_file()
                                }
                                _ => false,
                            })
                            .cloned()
                    });
                    group.stamps.insert(path.clone(), now);
                    match reused {
                        Some(artifact) => artifact,
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
        // Stamps describe this machine's files; the remote record goes without.
        if warm.remote
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
