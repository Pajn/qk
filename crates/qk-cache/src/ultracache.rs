//! `qk:ultracache`: key a task by what it read when it last ran, rather
//! than by its declared file inputs.
//!
//! A run with `--record` traces each opted-in task it executes and keeps a
//! path set: the files the task read, the directories it listed and the
//! paths it looked up. A later run keys the task by how those paths are now,
//! so a change to a file the task never read is still a hit, and a new file
//! in a directory it listed is a miss. See [`crate::recorded`].
//!
//! - `mode`: `"on"`, the default once the field is set, `"record"` or
//!   `"off"`. With `"record"`, a task is recorded and its recordings kept and
//!   shared as with `"on"`, but it is keyed by its declared inputs, and the
//!   run reports what its recordings would have done. Nx's `"warn"` and
//!   `"error"` are not supported yet.
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
    pub mode: Mode,
    pub ignore_reads: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Keyed by its recordings once it has any.
    #[default]
    On,
    /// Recorded, but keyed by its declared inputs.
    Record,
}

impl Settings {
    /// The target's settings when it is recorded, `None` when it is not:
    /// without the field, or with `mode` `"off"`.
    pub fn of(task: &Task) -> Result<Option<Self>> {
        let Some(value) = task.definition.extra.get("qk:ultracache") else {
            return Ok(None);
        };
        let Value::Object(object) = value else {
            bail!("qk:ultracache must be an object");
        };
        let mode = match object.get("mode").map(Value::as_str) {
            None | Some(Some("on")) => Mode::On,
            Some(Some("record")) => Mode::Record,
            Some(Some("off")) => return Ok(None),
            Some(Some(mode @ ("warn" | "error"))) => {
                bail!(
                    "qk:ultracache.mode {mode:?} is not supported yet; use \"on\", \"record\" or \"off\""
                )
            }
            Some(_) => bail!("qk:ultracache.mode must be \"on\", \"record\" or \"off\""),
        };
        let ignore_reads: Vec<String> = match object.get("ignoreReads") {
            None => Vec::new(),
            Some(value) => serde_json::from_value(value.clone())
                .context("qk:ultracache.ignoreReads must be an array of globs")?,
        };
        // Checked before the task runs, rather than after it was traced.
        for glob in &ignore_reads {
            crate::glob::Pattern::new(glob, false).with_context(|| {
                format!("qk:ultracache.ignoreReads has an invalid glob {glob:?}")
            })?;
        }
        Ok(Some(Self { mode, ignore_reads }))
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
        assert_eq!(on.mode, Mode::On);
        let record = Settings::of(&task(serde_json::json!({"mode": "record"})))
            .unwrap()
            .unwrap();
        assert_eq!(record.mode, Mode::Record);
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
            serde_json::json!({"ignoreReads": ["src/[*.log"]}),
        ] {
            assert!(Settings::of(&task(invalid.clone())).is_err(), "{invalid}");
        }
    }
}
