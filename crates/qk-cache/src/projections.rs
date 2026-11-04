//! Change projection manifests, kept under a key of everything that decides
//! them, so that projecting the same revision again reads the manifest
//! instead of checking out the revision and running the adapter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    key: String,
    artifacts: BTreeMap<String, String>,
}

/// The key of a manifest decided by `parts`.
pub fn key(parts: &impl Serialize) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(parts)?)
        .to_hex()
        .to_string())
}

fn location(cache: &Path, key: &str) -> PathBuf {
    cache.join("projections").join(format!("{key}.json"))
}

/// The artifacts kept under `key`, marked as used so eviction keeps them; none
/// when there are none or they cannot be read.
pub fn load(cache: &Path, key: &str) -> Option<BTreeMap<String, String>> {
    let path = location(cache, key);
    let stored: Stored = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    if stored.version != 1 || stored.key != key {
        return None;
    }
    crate::evict::touch(&path);
    Some(stored.artifacts)
}

pub fn keep(cache: &Path, key: &str, artifacts: &BTreeMap<String, String>) -> Result<()> {
    // The whole layout, so a cache that only ever kept manifests is pruned too
    crate::Cache::new(cache.to_path_buf()).initialize()?;
    std::fs::create_dir_all(cache.join("projections"))?;
    crate::record::publish(
        cache,
        &location(cache, key),
        &Stored {
            version: 1,
            key: key.to_owned(),
            artifacts: artifacts.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn age(cache: &Path, key: &str) -> SystemTime {
        let file = std::fs::File::options()
            .write(true)
            .open(location(cache, key))
            .unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        SystemTime::UNIX_EPOCH
    }

    fn used(cache: &Path, key: &str) -> SystemTime {
        std::fs::metadata(location(cache, key))
            .unwrap()
            .modified()
            .unwrap()
    }

    #[test]
    fn a_manifest_read_again_counts_as_used() {
        let cache = tempfile::tempdir().unwrap();
        let artifacts = BTreeMap::from([("a.js".to_owned(), "sha256:1".to_owned())]);
        keep(cache.path(), "k", &artifacts).unwrap();
        let aged = age(cache.path(), "k");

        assert_eq!(load(cache.path(), "k"), Some(artifacts));
        assert!(used(cache.path(), "k") > aged);
    }

    #[test]
    fn a_cache_of_manifests_alone_ages_them_out() {
        let cache = tempfile::tempdir().unwrap();
        keep(cache.path(), "old", &BTreeMap::new()).unwrap();
        age(cache.path(), "old");
        keep(cache.path(), "new", &BTreeMap::new()).unwrap();

        crate::prune(cache.path(), u64::MAX).unwrap();
        assert!(!location(cache.path(), "old").exists());
        assert!(location(cache.path(), "new").exists());
    }
}
