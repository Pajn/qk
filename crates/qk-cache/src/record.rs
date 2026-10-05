//! Stored result and warm-state records: formats, identity checks, blob references
//! and atomic publication. Checkout-dependent safety and blob contents are
//! verified by restoration, not by record parsing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::path::{Component, Path};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{combine_outputs, directory_output, file_output, link_output};

#[derive(Serialize, Deserialize)]
pub(super) struct ResultRecord {
    version: u32,
    pub(super) key: String,
    pub(super) log: String,
    pub(super) artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub(crate) enum Artifact {
    File { blob: String, mode: u32 },
    Directory { mode: u32 },
    Symlink { target: String, directory: bool },
}

impl ResultRecord {
    /// The output fingerprint of the stored artifacts, without reading any file.
    pub(super) fn output_fingerprint(&self, declared: bool) -> Result<String> {
        let files = self
            .artifacts
            .iter()
            .map(|(path, artifact)| {
                let value = match artifact {
                    Artifact::File { blob, mode } => file_output(blob, *mode),
                    Artifact::Directory { .. } => directory_output(),
                    Artifact::Symlink { target, .. } => link_output(target),
                };
                (path.clone(), value)
            })
            .collect();
        combine_outputs(&self.key, &files, declared)
    }
}

/// One group's saved files, relative to its base directory.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct WarmGroup {
    pub(super) artifacts: BTreeMap<String, Artifact>,
    /// Metadata of each file when saved, so an unchanged file is not read
    /// again on the next save.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) stamps: BTreeMap<String, Vec<i64>>,
    /// Each file's modification time when saved, in nanoseconds since the
    /// Unix epoch, for restores that keep it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) mtimes: BTreeMap<String, i64>,
}

/// One save of a task's warm state.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct WarmRecord {
    version: u32,
    pub(super) task: String,
    /// The root of the worktree that saved it.
    pub(super) worktree: String,
    /// The commit that worktree had checked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) commit: Option<String>,
    /// When it was saved, in milliseconds since the Unix epoch.
    pub(super) saved: u64,
    /// A digest of each part of the target's warm key when it was saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) key: Vec<String>,
    pub(super) groups: BTreeMap<String, WarmGroup>,
}

const WARM_VERSION: u32 = 2;

impl ResultRecord {
    pub(super) fn new(key: String, log: String, artifacts: BTreeMap<String, Artifact>) -> Self {
        Self {
            version: 1,
            key,
            log,
            artifacts,
        }
    }

    /// Validate the result header at the same stage as local restoration.
    pub(super) fn read(bytes: &[u8], key: &str) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes)?;
        if record.version != 1 || record.key != key || !valid_hash(&record.log) {
            bail!("invalid cache manifest");
        }
        Ok(record)
    }
}

impl WarmRecord {
    pub(super) fn new(
        task: String,
        worktree: String,
        commit: Option<String>,
        saved: u64,
        key: Vec<String>,
    ) -> Self {
        Self {
            version: WARM_VERSION,
            task,
            worktree,
            commit,
            saved,
            key,
            groups: BTreeMap::new(),
        }
    }

    /// Warm records have their own version and task identity, not result keys.
    pub(super) fn read(bytes: &[u8], task: &str) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes)?;
        if record.version != WARM_VERSION || record.task != task {
            bail!("invalid warm record");
        }
        Ok(record)
    }
}

/// The transport and eviction view of a stored record. It deliberately accepts
/// partial or unknown records: eviction must retain every cited blob, and remote
/// transport leaves typed validation to the result or warm-state reader.
/// Blob ordering is part of the remote pack format.
pub(crate) struct StoredRecord(Value);

impl StoredRecord {
    pub(crate) fn read(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes).map(Self)
    }

    /// Remote result fetching historically checks only the requested key before
    /// receiving blobs. Do not move full result validation into this stage.
    pub(crate) fn check_key(&self, key: &str) -> Result<()> {
        if self.0.get("key").and_then(Value::as_str) != Some(key) {
            bail!("remote manifest is for another key");
        }
        Ok(())
    }

    /// Sorted, deduplicated blob names, including result logs and warm groups.
    pub(crate) fn blobs(&self) -> BTreeSet<String> {
        let mut blobs = BTreeSet::new();
        if let Some(log) = self.0.get("log").and_then(Value::as_str) {
            blobs.insert(log.to_owned());
        }
        let groups = self
            .0
            .get("groups")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|groups| groups.values())
            .filter_map(|group| group.get("artifacts").and_then(Value::as_object));
        for artifact in self
            .0
            .get("artifacts")
            .and_then(Value::as_object)
            .into_iter()
            .chain(groups)
            .flat_map(|artifacts| artifacts.values())
        {
            if let Some(blob) = artifact.get("blob").and_then(Value::as_str) {
                blobs.insert(blob.to_owned());
            }
        }
        blobs
    }
}

/// Publish only after serialization and flushing complete. The caller owns
/// directory creation and the decision that all required blobs are available.
pub(crate) fn publish(root: &Path, destination: &Path, record: &impl Serialize) -> Result<()> {
    publish_with(root, destination, |file| {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, record)?;
        writer.flush()?;
        Ok(())
    })
}

/// Preserve the original remote bytes rather than reserializing their JSON.
pub(crate) fn publish_bytes(root: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    publish_with(root, destination, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

fn publish_with(
    root: &Path,
    destination: &Path,
    write: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
    write(file.as_file_mut())?;
    file.persist(destination)?;
    Ok(())
}

pub(crate) fn validate_link(path: &str, target: &str) -> Result<()> {
    if target.contains(['\\', ':', '\0']) || Path::new(target).is_absolute() {
        bail!("cache symlinks must be relative");
    }
    let mut depth = Path::new(path)
        .parent()
        .map(|path| path.components().count())
        .unwrap_or_default();
    for part in Path::new(target).components() {
        match part {
            Component::Normal(part) if part != ".git" && part != ".qk" => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => bail!("cache symlink escapes the workspace or references a reserved directory"),
        }
    }
    Ok(())
}

pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_reader_preserves_format_and_header_acceptance() {
        let key = "a".repeat(64);
        let log = "b".repeat(64);
        let mut value = json!({
            "version": 1, "key": key, "log": log,
            "artifacts": {
                "out": {"type": "Directory", "mode": 493},
                "out/file": {"type": "File", "blob": "c".repeat(64), "mode": 420},
                "out/link": {"type": "Symlink", "target": "file", "directory": false}
            },
            "futureMetadata": true
        });
        let read = |value: &Value| ResultRecord::read(&serde_json::to_vec(value).unwrap(), &key);
        let record = read(&value).unwrap();
        let mut expected = value.clone();
        expected.as_object_mut().unwrap().remove("futureMetadata");
        assert_eq!(serde_json::to_value(&record).unwrap(), expected);
        for (field, invalid) in [
            ("version", json!(2)),
            ("key", json!("another key")),
            ("log", json!("not a hash")),
        ] {
            let mut invalid_record = value.clone();
            invalid_record[field] = invalid;
            assert_eq!(
                read(&invalid_record).err().unwrap().to_string(),
                "invalid cache manifest"
            );
        }
        value["artifacts"]["out/file"]["type"] = json!("Unknown");
        assert!(read(&value).is_err());
    }

    #[test]
    fn warm_reader_preserves_optional_metadata_and_separate_identity() {
        let mut value = json!({
            "version": 2, "task": "shared-ccache", "worktree": "/checkout",
            "saved": 123, "groups": {"directory": {"artifacts": {
                "file": {"type": "File", "blob": "a".repeat(64), "mode": 420}
            }}}
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let record = WarmRecord::read(&bytes, "shared-ccache").unwrap();
        assert_eq!(serde_json::to_value(&record).unwrap(), value);
        assert!(WarmRecord::read(&bytes, "another-task").is_err());
        assert!(ResultRecord::read(&bytes, "shared-ccache").is_err());
        value["commit"] = json!("commit");
        value["key"] = json!(["toolchain"]);
        value["groups"]["directory"]["stamps"] = json!({"file": [1, 2, 3]});
        value["groups"]["directory"]["mtimes"] = json!({"file": 456});
        let record =
            WarmRecord::read(&serde_json::to_vec(&value).unwrap(), "shared-ccache").unwrap();
        assert_eq!(serde_json::to_value(&record).unwrap(), value);
        value["version"] = json!(1);
        assert!(WarmRecord::read(&serde_json::to_vec(&value).unwrap(), "shared-ccache").is_err());
    }

    #[test]
    fn transport_and_eviction_preserve_sorted_references_in_partial_records() {
        // Eviction must not lose references just because a future or malformed
        // record would be rejected by the typed restore reader.
        let value = json!({
            "version": 99, "key": "requested-key", "log": "z",
            "artifacts": {
                "first": {"blob": "b"}, "duplicate": {"blob": "b"},
                "unknownType": {"type": "Future", "blob": "a"},
                "notString": {"blob": 12}, "notObject": false
            },
            "groups": {
                "directory": {"artifacts": {"file": {"blob": "c"}, "shared": {"blob": "b"}}},
                "malformed": {"artifacts": []}
            }
        });
        let record = StoredRecord::read(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            record.blobs().into_iter().collect::<Vec<_>>(),
            ["a", "b", "c", "z"]
        );
        record.check_key("requested-key").unwrap();
        assert_eq!(
            record.check_key("other").unwrap_err().to_string(),
            "remote manifest is for another key"
        );
        for value in [
            json!(null),
            json!([]),
            json!({"log": 1, "artifacts": [], "groups": false}),
        ] {
            let record = StoredRecord::read(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(record.blobs().is_empty());
            assert!(record.check_key("requested-key").is_err());
        }
        assert!(StoredRecord::read(b"not JSON").is_err());
    }

    #[test]
    fn failed_serialization_preserves_published_record_and_cleans_staging() {
        struct Fails;
        impl Serialize for Fails {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("partial", "written before failure")?;
                Err(serde::ser::Error::custom("serialization failed"))
            }
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        let destination = root.path().join("record.json");
        let original = b"{ \"key\": \"original\" }\n";
        publish_bytes(root.path(), &destination, original).unwrap();
        assert!(publish(root.path(), &destination, &Fails).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
            0
        );
        let value = json!({"key": "replacement"});
        publish(root.path(), &destination, &value).unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            serde_json::to_vec(&value).unwrap()
        );
    }
}
