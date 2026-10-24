//! `qk:threads`: how many threads a task that fans out across cores may use.
//!
//! Each task that runs holds cores from the run's budget: one, or for a
//! target with `qk:threads` its share, fixed when it starts, since a tool
//! cannot give up workers it has started. The share is what is free, divided
//! among the threaded tasks ready to start, after a core for each task slot
//! other pending tasks could take, or for a task expected from earlier runs to
//! take some time, the part of the cores its work is of the work that can run
//! beside it, and kept within `min` and `max`. It reaches the task,
//! times `scale`, as `QK_THREADS` and through the `env` variables naming
//! `{threads}`.
//!
//! - `env`: variables for the task, which may name `{threads}`.
//! - `min`: the fewest threads the task starts with; it waits for them.
//!   Defaults to 1.
//! - `max`: the most it is given. Defaults to the whole budget.
//! - `scale`: how many threads the task is told to start for each core it
//!   holds, for a tool whose threads spend much of their time waiting.
//!   Defaults to 1. The task holds its share of the budget all the same; only
//!   the count it is told grows, rounded up.

use std::collections::BTreeMap;
use std::ffi::OsString;

use anyhow::{Context, Result, bail};
use qk_taskgraph::Task;
use serde_json::Value;

/// The fields a `qk:threads` object can set. Others are ignored, as Nx ignores
/// keys it does not know; `qk doctor` reports them.
pub const FIELDS: &[&str] = &["env", "min", "max", "scale"];

#[derive(Clone, Debug, PartialEq)]
pub struct Threads {
    pub env: BTreeMap<String, String>,
    pub min: usize,
    pub max: Option<usize>,
    pub scale: f64,
}

impl Threads {
    pub fn config(task: &Task) -> Result<Option<Self>> {
        let Some(value) = task.definition.extra.get("qk:threads") else {
            return Ok(None);
        };
        let object = match value {
            Value::Bool(true) => return Ok(Some(Self::default())),
            Value::Object(object) => object,
            _ => bail!("qk:threads must be true or an object"),
        };
        let count = |name: &str| -> Result<Option<usize>> {
            object
                .get(name)
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|count| *count > 0)
                        .map(|count| count as usize)
                        .with_context(|| format!("qk:threads.{name} must be a positive integer"))
                })
                .transpose()
        };
        let min = count("min")?.unwrap_or(1);
        let max = count("max")?;
        if max.is_some_and(|max| max < min) {
            bail!("qk:threads.max is below qk:threads.min");
        }
        let scale = match object.get("scale") {
            None => 1.0,
            Some(value) => value
                .as_f64()
                .filter(|scale| scale.is_finite() && *scale > 0.0 && *scale <= MOST_SCALE)
                .with_context(|| {
                    format!("qk:threads.scale must be a number above 0 and at most {MOST_SCALE}")
                })?,
        };
        let env = match object.get("env") {
            None => BTreeMap::new(),
            Some(Value::Object(env)) => env
                .iter()
                .map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.clone(), value.to_owned()))
                        .with_context(|| format!("qk:threads.env.{name} must be a string"))
                })
                .collect::<Result<_>>()?,
            Some(_) => bail!("qk:threads.env must be an object"),
        };
        Ok(Some(Self {
            env,
            min,
            max,
            scale,
        }))
    }

    /// The even split of `free` cores among `ready` threaded tasks starting
    /// together, after `reserved` cores kept for other tasks.
    pub fn even(free: usize, ready: usize, reserved: usize) -> usize {
        free.saturating_sub(reserved) / ready.max(1)
    }

    /// This task's share given the `even` split, or `None` while fewer than
    /// `min` of the cores are `free`. With nothing else running it starts
    /// regardless, so a `min` above the budget cannot stall the run.
    pub fn share(&self, budget: usize, even: usize, free: usize, idle: bool) -> Option<usize> {
        let share = even
            .max(self.min)
            .min(self.max.unwrap_or(budget))
            .min(budget)
            .max(1);
        if share <= free {
            Some(share)
        } else if idle {
            Some(free.max(1))
        } else {
            None
        }
    }

    /// The threads a task holding `cores` is told to start: its share,
    /// scaled and rounded up, and at least one.
    pub fn threads(&self, cores: usize) -> usize {
        ((cores as f64 * self.scale).ceil() as usize).max(1)
    }

    /// The variables that tell the task its share.
    pub fn environment(&self, threads: usize) -> BTreeMap<OsString, OsString> {
        let count = threads.to_string();
        let mut env: BTreeMap<OsString, OsString> = self
            .env
            .iter()
            .map(|(name, value)| (name.into(), value.replace("{threads}", &count).into()))
            .collect();
        env.insert("QK_THREADS".into(), count.into());
        env
    }
}

impl Default for Threads {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            min: 1,
            max: None,
            scale: 1.0,
        }
    }
}

/// The largest `scale`: past it a typo, such as 20 for 2.0, would start a
/// worker pool many times the machine.
const MOST_SCALE: f64 = 8.0;

#[cfg(test)]
mod tests {
    use super::Threads;

    #[test]
    fn divides_what_is_free_among_the_ready() {
        let threads = Threads::default();
        let share = |free, ready, reserved| {
            threads.share(8, Threads::even(free, ready, reserved), free, true)
        };
        assert_eq!(share(8, 1, 0), Some(8));
        assert_eq!(share(8, 2, 0), Some(4));
        assert_eq!(share(8, 3, 0), Some(2));
        // A core for each other task starting now.
        assert_eq!(share(8, 2, 2), Some(3));
        // Later starters get what is left.
        assert_eq!(threads.share(8, Threads::even(2, 1, 0), 2, false), Some(2));
        assert_eq!(threads.share(8, 0, 0, false), None);
    }

    #[test]
    fn scales_the_threads_it_tells_not_the_cores_it_holds() {
        let config = |value: serde_json::Value| {
            let mut definition: qk_config::Target =
                serde_json::from_value(serde_json::json!({})).unwrap();
            definition.extra.insert("qk:threads".into(), value);
            Threads::config(&qk_taskgraph::Task {
                id: "app:test".into(),
                project: "app".into(),
                target: "test".into(),
                configuration: None,
                args: Vec::new(),
                definition,
                dependencies: Default::default(),
                edges: Default::default(),
            })
        };
        let scaled = config(serde_json::json!({"scale": 1.5})).unwrap().unwrap();
        assert_eq!(scaled.share(8, 3, 8, false), Some(3));
        assert_eq!(scaled.threads(3), 5);
        assert_eq!(scaled.threads(0), 1);
        assert_eq!(
            scaled.environment(scaled.threads(2))[std::ffi::OsStr::new("QK_THREADS")],
            std::ffi::OsString::from("3")
        );
        assert_eq!(Threads::default().threads(4), 4);
        for invalid in [0.0, -1.0, 9.0] {
            assert!(config(serde_json::json!({"scale": invalid})).is_err());
        }
        assert!(config(serde_json::json!({"scale": "2"})).is_err());
    }

    #[test]
    fn keeps_within_min_and_max() {
        let bounded = Threads {
            min: 3,
            max: Some(4),
            ..Threads::default()
        };
        assert_eq!(bounded.share(16, 16, 16, true), Some(4));
        assert_eq!(bounded.share(16, 2, 4, false), Some(3));
        assert_eq!(bounded.share(16, 2, 2, false), None);
        // Alone, it starts with what there is.
        assert_eq!(bounded.share(2, 2, 2, true), Some(2));
    }
}
