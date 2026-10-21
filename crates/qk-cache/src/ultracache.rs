//! `qk:ultracache`: key a task by what it read when it last ran, rather
//! than by its declared file inputs.
//!
//! A run with `--record` traces each opted-in task it executes and keeps a
//! path set: the files the task read, the directories it listed and the
//! paths it looked up. A later run keys the task by how those paths are now,
//! so a change to a file the task never read is still a hit, and a new file
//! in a directory it listed is a miss. See [`crate::recorded`].
//!
//! - `mode`: `"on"`, the default once the field is set, or `"off"`. Nx's
//!   `"warn"` and `"error"` are not supported yet.
//! - `ignoreReads`: workspace globs left out of the path set, for files whose
//!   changes cannot change the result.

use anyhow::{Context, Result, bail};
use qk_taskgraph::Task;
use serde_json::Value;

/// The fields a `qk:ultracache` object can set. Others are ignored, as Nx
/// ignores keys it does not know; `qk doctor` reports them.
pub const FIELDS: &[&str] = &["mode", "ignoreReads"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub ignore_reads: Vec<String>,
}

impl Settings {
    /// The target's settings when it is keyed by its recordings, `None` when
    /// it is not: without the field, or with `mode` `"off"`.
    pub fn of(task: &Task) -> Result<Option<Self>> {
        let Some(value) = task.definition.extra.get("qk:ultracache") else {
            return Ok(None);
        };
        let Value::Object(object) = value else {
            bail!("qk:ultracache must be an object");
        };
        match object.get("mode").map(Value::as_str) {
            None | Some(Some("on")) => {}
            Some(Some("off")) => return Ok(None),
            Some(Some(mode @ ("warn" | "error"))) => {
                bail!("qk:ultracache.mode {mode:?} is not supported yet; use \"on\" or \"off\"")
            }
            Some(_) => bail!("qk:ultracache.mode must be \"on\" or \"off\""),
        }
        let ignore_reads = match object.get("ignoreReads") {
            None => Vec::new(),
            Some(value) => serde_json::from_value(value.clone())
                .context("qk:ultracache.ignoreReads must be an array of globs")?,
        };
        Ok(Some(Self { ignore_reads }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(value: Value) -> Task {
        let mut definition: qk_config::Target =
            serde_json::from_value(serde_json::json!({})).unwrap();
        definition.extra.insert("qk:ultracache".into(), value);
        Task {
            id: "app:build".into(),
            project: "app".into(),
            target: "build".into(),
            configuration: None,
            args: Vec::new(),
            definition,
            dependencies: Default::default(),
            edges: Default::default(),
        }
    }

    #[test]
    fn reads_the_mode_and_ignored_reads() {
        let on = Settings::of(&task(serde_json::json!({"ignoreReads": ["tmp/**"]})))
            .unwrap()
            .unwrap();
        assert_eq!(on.ignore_reads, ["tmp/**"]);
        assert!(
            Settings::of(&task(serde_json::json!({"mode": "off"})))
                .unwrap()
                .is_none()
        );
        for invalid in [
            serde_json::json!(true),
            serde_json::json!({"mode": "warn"}),
            serde_json::json!({"mode": 1}),
            serde_json::json!({"ignoreReads": "tmp/**"}),
        ] {
            assert!(Settings::of(&task(invalid.clone())).is_err(), "{invalid}");
        }
    }
}
