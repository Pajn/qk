//! Looking a task with `qk:ultracache` up by its recordings, and the verdicts
//! that lookup gives: what is asked of the store, and in what order, apart
//! from how a result is restored or only checked for.

use crate::recorded::PathSet;
use crate::ultracache::Mode;
use crate::{Recorded, Shadow};

/// What a run does with a task's recordings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Mode `"on"`: restores a result one of them keys.
    Restore,
    /// Mode `"record"`: looks for one without restoring it, to say what
    /// they would have done.
    Shadow,
    /// A cold run: looks nothing up, and replaces the results they key.
    Replace,
}

pub(crate) fn lookup(mode: Mode, cold: bool) -> Lookup {
    match mode {
        _ if cold => Lookup::Replace,
        Mode::On => Lookup::Restore,
        Mode::Record => Lookup::Shadow,
    }
}

/// Whether a lookup that found nothing here asks the store for the path sets
/// and results it holds. Without path sets here, a result here under the
/// declared key, as `declared_here` says, is the task's, and asking the store
/// for path sets would only delay it, run after run, and have them shadow it.
/// A shadow lookup restores nothing, so it always asks.
pub(crate) fn asks_store(
    lookup: Lookup,
    sets: &[PathSet],
    declared_here: impl FnOnce() -> bool,
) -> bool {
    match lookup {
        Lookup::Restore => !sets.is_empty() || !declared_here(),
        Lookup::Shadow => true,
        Lookup::Replace => false,
    }
}

/// Whether a task is looked up by its declared key: without recordings, in
/// mode `"record"`, or without path sets, or where its declared inputs, as
/// `covers` says, cover what each of its path sets read: otherwise its
/// declared inputs are what a recording found wanting.
pub(crate) fn by_declared(
    mode: Option<Mode>,
    sets: &[PathSet],
    covers: impl FnOnce(&[PathSet]) -> bool,
) -> bool {
    match mode {
        None | Some(Mode::Record) => true,
        Some(Mode::On) => sets.is_empty() || covers(sets),
    }
}

/// Where the results a lookup finds are, and what finding one does.
pub(crate) trait Results {
    /// Whether the local cache gives a result under `key`.
    fn here(&self, key: &str) -> bool;
    /// Whether the remote store does.
    fn there(&self, key: &str) -> bool;
}

/// What a lookup by a task's recordings found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// A result under this key in the local cache.
    Local(String),
    /// A result under this key in the remote store.
    Remote(String),
    Nothing,
}

/// A lookup by a task's recordings that found nothing here: the keys of the
/// path sets it tried, in order, for the store.
#[derive(Debug)]
pub(crate) struct Search {
    tried: Vec<String>,
}

impl Search {
    /// Looks each path set's key up here, first set first; a set `key`
    /// cannot key is passed over.
    pub(crate) fn here(
        sets: &[PathSet],
        key: impl Fn(&PathSet) -> Option<String>,
        results: &impl Results,
    ) -> Result<Found, Self> {
        let mut tried = Vec::new();
        for set in sets {
            if let Some(strong) = key(set) {
                if results.here(&strong) {
                    return Ok(Found::Local(strong));
                }
                tried.push(strong);
            }
        }
        Err(Self { tried })
    }

    /// Goes on with the path sets the store held that were new here,
    /// `added`: each looked up here, and added to `sets`, until one has a
    /// result; then every key tried in the store, in order.
    pub(crate) fn there(
        mut self,
        added: Vec<PathSet>,
        sets: &mut Vec<PathSet>,
        key: impl Fn(&PathSet) -> Option<String>,
        results: &impl Results,
    ) -> Found {
        for set in added {
            if let Some(strong) = key(&set) {
                if results.here(&strong) {
                    sets.push(set);
                    return Found::Local(strong);
                }
                self.tried.push(strong);
            }
            sets.push(set);
        }
        self.tried
            .into_iter()
            .find(|strong| results.there(strong))
            .map_or(Found::Nothing, Found::Remote)
    }
}

/// What became of a recording of a task that ran.
#[derive(Debug)]
pub(crate) enum Ran {
    /// Nothing recorded it to key its result.
    Unkeyed,
    /// Its recording keys nothing, for this reason.
    Refused(String),
    /// Its recording was whole, but the run's result is not kept.
    Unkept,
    /// Its result was saved under its recording's key, too.
    Saved(String),
    /// Saving it under its recording's key failed, for this reason.
    Unsaved(String),
}

/// Where a task's result came from.
#[derive(Debug)]
pub(crate) enum Taken {
    /// The cache, under one of its path sets' keys.
    Recording(String),
    /// The cache, under its declared key.
    Declared(String),
    /// It ran.
    Ran(Ran),
}

/// How a task in `mode` was keyed by its recordings, with path sets or
/// without once looked up: `None` in mode `"record"`, and without
/// recordings, unless it ran and was recorded.
pub(crate) fn recorded(mode: Option<Mode>, had_sets: bool, taken: Taken) -> Option<Recorded> {
    let on = mode == Some(Mode::On);
    match taken {
        Taken::Recording(key) => Some(Recorded::Hit { key }),
        Taken::Declared(key) => on.then_some(match had_sets {
            true => Recorded::Declared { key },
            false => Recorded::Unrecorded,
        }),
        Taken::Ran(Ran::Saved(key)) => Some(Recorded::Saved { key }),
        Taken::Ran(Ran::Refused(reason) | Ran::Unsaved(reason)) => {
            Some(Recorded::Failed { reason })
        }
        Taken::Ran(Ran::Unkeyed | Ran::Unkept) => on.then_some(match had_sets {
            true => Recorded::Missed,
            false => Recorded::Unrecorded,
        }),
    }
}

/// What a task's recordings would have done, by what their lookup found.
pub(crate) fn shadow(found: Found, sets: &[PathSet]) -> Shadow {
    match found {
        Found::Local(key) | Found::Remote(key) => Shadow::WouldHit { key, check: None },
        Found::Nothing => unfound(sets),
    }
}

/// What a task's recordings would have done when none of their keys has a
/// result.
pub(crate) fn unfound(sets: &[PathSet]) -> Shadow {
    if sets.is_empty() {
        Shadow::Unrecorded
    } else {
        Shadow::WouldMiss {
            changed: vec![],
            uncovered: vec![],
            declared: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn set(name: &str) -> PathSet {
        PathSet {
            reads: [name.to_owned()].into(),
            ..Default::default()
        }
    }

    /// A set's key is its read's name; one named `unkeyed` has none.
    fn key(set: &PathSet) -> Option<String> {
        set.reads
            .first()
            .filter(|name| *name != "unkeyed")
            .map(|name| format!("key-{name}"))
    }

    #[derive(Default)]
    struct Fake {
        here: HashSet<String>,
        there: HashSet<String>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(here: &[&str], there: &[&str]) -> Self {
            Self {
                here: here.iter().map(|key| key.to_string()).collect(),
                there: there.iter().map(|key| key.to_string()).collect(),
                calls: Default::default(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Results for Fake {
        fn here(&self, key: &str) -> bool {
            self.calls.borrow_mut().push(format!("here {key}"));
            self.here.contains(key)
        }

        fn there(&self, key: &str) -> bool {
            self.calls.borrow_mut().push(format!("there {key}"));
            self.there.contains(key)
        }
    }

    #[test]
    fn a_lookup_follows_the_mode_unless_the_run_is_cold() {
        assert_eq!(lookup(Mode::On, false), Lookup::Restore);
        assert_eq!(lookup(Mode::Record, false), Lookup::Shadow);
        assert_eq!(lookup(Mode::On, true), Lookup::Replace);
        assert_eq!(lookup(Mode::Record, true), Lookup::Replace);
    }

    #[test]
    fn only_a_restore_without_path_sets_skips_the_store_for_a_declared_result() {
        let sets = [set("a")];
        assert!(!asks_store(Lookup::Restore, &[], || true));
        assert!(asks_store(Lookup::Restore, &[], || false));
        assert!(asks_store(Lookup::Restore, &sets, || panic!("asked")));
        assert!(asks_store(Lookup::Shadow, &[], || panic!("asked")));
        assert!(!asks_store(Lookup::Replace, &sets, || panic!("asked")));
    }

    #[test]
    fn coverage_is_judged_only_for_path_sets_in_mode_on() {
        let sets = [set("a")];
        let never = |_: &[PathSet]| -> bool { panic!("judged") };
        assert!(by_declared(None, &sets, never));
        assert!(by_declared(Some(Mode::Record), &sets, never));
        assert!(by_declared(Some(Mode::On), &[], never));
        assert!(by_declared(Some(Mode::On), &sets, |_| true));
        assert!(!by_declared(Some(Mode::On), &sets, |_| false));
    }

    #[test]
    fn the_first_path_set_with_a_result_here_wins() {
        let results = Fake::new(&["key-b", "key-c"], &[]);
        let sets = [set("a"), set("b"), set("c")];
        let found = Search::here(&sets, key, &results).unwrap();
        assert_eq!(found, Found::Local("key-b".into()));
        assert_eq!(results.calls(), ["here key-a", "here key-b"]);
    }

    #[test]
    fn a_path_set_that_cannot_key_is_passed_over() {
        let results = Fake::new(&[], &["key-a"]);
        let sets = [set("unkeyed"), set("a")];
        let search = Search::here(&sets, key, &results).unwrap_err();
        assert_eq!(search.tried, ["key-a"]);
        let mut sets = sets.to_vec();
        let found = search.there(vec![set("unkeyed")], &mut sets, key, &results);
        assert_eq!(found, Found::Remote("key-a".into()));
        assert_eq!(sets.len(), 3);
        assert_eq!(results.calls(), ["here key-a", "there key-a"]);
    }

    #[test]
    fn the_stores_path_sets_are_tried_here_before_any_key_in_the_store() {
        let results = Fake::new(&["key-c"], &["key-a"]);
        let mut sets = vec![set("a")];
        let search = Search::here(&sets, key, &results).unwrap_err();
        let found = search.there(vec![set("b"), set("c"), set("d")], &mut sets, key, &results);
        assert_eq!(found, Found::Local("key-c".into()));
        assert_eq!(results.calls(), ["here key-a", "here key-b", "here key-c"]);
        // Added up to the one that had a result, which is added too.
        assert_eq!(sets, [set("a"), set("b"), set("c")]);
    }

    #[test]
    fn the_store_is_asked_for_every_key_in_order() {
        let results = Fake::new(&[], &["key-b", "key-a"]);
        let mut sets = vec![set("a")];
        let search = Search::here(&sets, key, &results).unwrap_err();
        let found = search.there(vec![set("b")], &mut sets, key, &results);
        assert_eq!(found, Found::Remote("key-a".into()));
        assert_eq!(results.calls(), ["here key-a", "here key-b", "there key-a"]);
        assert_eq!(sets, [set("a"), set("b")]);
        let results = Fake::new(&[], &[]);
        let mut sets = vec![set("a")];
        let search = Search::here(&sets, key, &results).unwrap_err();
        let found = search.there(vec![set("b")], &mut sets, key, &results);
        assert_eq!(found, Found::Nothing);
        assert_eq!(
            results.calls(),
            ["here key-a", "here key-b", "there key-a", "there key-b"]
        );
    }

    #[test]
    fn verdicts() {
        let failed = |reason: &str| {
            Some(Recorded::Failed {
                reason: reason.into(),
            })
        };
        let on = Some(Mode::On);
        let record = Some(Mode::Record);
        assert_eq!(
            recorded(on, true, Taken::Recording("r".into())),
            Some(Recorded::Hit { key: "r".into() })
        );
        assert_eq!(
            recorded(on, true, Taken::Declared("d".into())),
            Some(Recorded::Declared { key: "d".into() })
        );
        assert_eq!(
            recorded(on, false, Taken::Declared("d".into())),
            Some(Recorded::Unrecorded)
        );
        assert_eq!(recorded(record, true, Taken::Declared("d".into())), None);
        assert_eq!(recorded(None, false, Taken::Declared("d".into())), None);
        for ran in [Ran::Unkeyed, Ran::Unkept] {
            assert_eq!(
                recorded(on, false, Taken::Ran(ran)),
                Some(Recorded::Unrecorded)
            );
        }
        for ran in [Ran::Unkeyed, Ran::Unkept] {
            assert_eq!(recorded(on, true, Taken::Ran(ran)), Some(Recorded::Missed));
        }
        for ran in [Ran::Unkeyed, Ran::Unkept] {
            assert_eq!(recorded(record, true, Taken::Ran(ran)), None);
        }
        // A failed recording is said even where the task's path sets missed.
        for mode in [on, record] {
            assert_eq!(
                recorded(mode, true, Taken::Ran(Ran::Refused("partial".into()))),
                failed("partial")
            );
            assert_eq!(
                recorded(mode, true, Taken::Ran(Ran::Unsaved("full".into()))),
                failed("full")
            );
            assert_eq!(
                recorded(mode, true, Taken::Ran(Ran::Saved("s".into()))),
                Some(Recorded::Saved { key: "s".into() })
            );
        }
    }

    #[test]
    fn without_a_result_a_task_with_path_sets_would_miss() {
        assert_eq!(
            shadow(Found::Remote("r".into()), &[]),
            Shadow::WouldHit {
                key: "r".into(),
                check: None
            }
        );
        assert_eq!(shadow(Found::Nothing, &[]), Shadow::Unrecorded);
        assert_eq!(unfound(&[]), Shadow::Unrecorded);
        assert!(matches!(
            unfound(&[set("a")]),
            Shadow::WouldMiss {
                declared: false,
                ..
            }
        ));
    }
}
