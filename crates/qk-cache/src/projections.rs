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

/// The artifacts kept under `key`; none when there are none or they cannot be
/// read.
pub fn load(cache: &Path, key: &str) -> Option<BTreeMap<String, String>> {
    let stored: Stored = serde_json::from_slice(&std::fs::read(location(cache, key)).ok()?).ok()?;
    (stored.version == 1 && stored.key == key).then_some(stored.artifacts)
}

pub fn keep(cache: &Path, key: &str, artifacts: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(cache.join("projections"))?;
    std::fs::create_dir_all(cache.join("tmp"))?;
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
