//! Fetching the entries a run is likely to ask for next.
//!
//! A cached chain of tasks fetches one entry per level, and each fetch waits
//! for the level below, because a task's key is not known until its
//! dependencies are restored. The index's runs say which entries were used
//! together. Once a task's entry is a remote hit, the other entries of the
//! recent runs that used it are fetched in the background, nearest to it in
//! those runs first, so a dependent usually finds its entry already local.
//! Prefetching stops when the run's keys stop matching the runs it follows,
//! or a budget is spent. A prefetched entry counts as used, and is listed,
//! only once a task claims it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{Remote, index};

/// Entries fetched at once ahead of the tasks that need them.
const PREFETCHERS: usize = 4;
/// The most runs followed from one hit.
const FOLLOW_RUNS: usize = 3;
/// The most a run prefetches: bytes received, and entries.
const BUDGET_BYTES: u64 = 1 << 30;
const BUDGET_ENTRIES: usize = 5_000;
/// Keys a run resolves before it judges whether its keys match the runs it
/// follows, and the share that must.
const JUDGE_AFTER: usize = 8;
const MATCHING: f64 = 0.5;

#[derive(Default)]
pub(super) struct Prefetch {
    state: Mutex<State>,
    /// Set once prefetching stops for the run.
    pub(super) stopped: AtomicBool,
    /// Bytes received by prefetches.
    pub(super) received: AtomicU64,
}

#[derive(Default)]
struct State {
    queue: VecDeque<String>,
    /// Every key ever queued, whether or not it was fetched.
    queued: HashSet<String>,
    /// Entries fetched ahead that no task has claimed.
    fetched: HashSet<String>,
    /// How many prefetched entries tasks claimed.
    claimed: usize,
    /// Keys the run resolved, and how many of them were queued.
    resolved: usize,
    matched: usize,
    /// Runs already followed.
    followed: HashSet<String>,
    workers: usize,
    /// Full keys by the digits runs name them by, `None` where two share them.
    members: Option<HashMap<String, Option<String>>>,
}

impl Prefetch {
    /// Queues the entries recent runs used beside `key`, and starts workers
    /// to fetch them into the local store at `root`.
    pub(super) fn follow(&self, remote: &Arc<Remote>, root: &Path, key: &str) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Some(known) = remote.known() else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        let member = index::member(key);
        let mut runs: Vec<(&String, &index::Run)> = known
            .runs
            .iter()
            .filter(|(id, run)| {
                !state.followed.contains(*id) && run.members.iter().any(|m| m == member)
            })
            .collect();
        runs.sort_by(|(a, left), (b, right)| right.time.cmp(&left.time).then(b.cmp(a)));
        let members = state.members.get_or_insert_with(|| {
            let mut members: HashMap<String, Option<String>> = HashMap::new();
            for key in known.keys.keys() {
                members
                    .entry(index::member(key).to_owned())
                    .and_modify(|full| *full = None)
                    .or_insert_with(|| Some(key.clone()));
            }
            members
        });
        let mut found = Vec::new();
        for (_, run) in runs.iter().take(FOLLOW_RUNS) {
            let at = run.members.iter().position(|m| m == member).unwrap_or(0);
            // After the hit first, nearest first, then what came before it.
            let after = run.members[at + 1..].iter();
            let before = run.members[..at].iter().rev();
            for name in after.chain(before) {
                if let Some(Some(full)) = members.get(name) {
                    found.push(full.clone());
                }
            }
        }
        for (id, _) in runs.iter().take(FOLLOW_RUNS) {
            state.followed.insert((*id).clone());
        }
        for key in found {
            let local = root.join("entries").join(format!("{key}.json")).is_file();
            if !local && state.queued.insert(key.clone()) {
                state.queue.push_back(key);
            }
        }
        while state.workers < PREFETCHERS && !state.queue.is_empty() {
            state.workers += 1;
            let remote = remote.clone();
            let root = root.to_owned();
            std::thread::spawn(move || work(&remote, &root));
        }
    }

    /// Notes that the run resolved `key`, and stops prefetching once the run
    /// has gone its own way.
    pub(super) fn resolved(&self, key: &str) {
        let mut state = self.state.lock().unwrap();
        if state.queued.is_empty() {
            return;
        }
        state.resolved += 1;
        if state.queued.contains(key) {
            state.matched += 1;
        }
        if state.resolved >= JUDGE_AFTER
            && (state.matched as f64) < state.resolved as f64 * MATCHING
        {
            drop(state);
            self.stop();
        }
    }

    /// Whether `key` was fetched ahead and is still unclaimed.
    pub(super) fn holds(&self, key: &str) -> bool {
        self.state.lock().unwrap().fetched.contains(key)
    }

    /// Takes `key`'s prefetched entry for a task, returning whether there was one.
    pub(super) fn claim(&self, key: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let claimed = state.fetched.remove(key);
        state.claimed += usize::from(claimed);
        claimed
    }

    /// Stops prefetching for the run. Fetches in progress finish on their own.
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.state.lock().unwrap().queue.clear();
    }

    /// What prefetching did: entries fetched, entries claimed.
    pub(super) fn summary(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (state.fetched.len() + state.claimed, state.claimed)
    }

    /// The next key to fetch, or `None` when the worker asking should end.
    fn next(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let key = if self.stopped.load(Ordering::SeqCst) {
            None
        } else {
            state.queue.pop_front()
        };
        if key.is_none() {
            state.workers -= 1;
        }
        key
    }
}

fn work(remote: &Arc<Remote>, root: &Path) {
    let prefetch = &remote.prefetch;
    while let Some(key) = prefetch.next() {
        if root.join("entries").join(format!("{key}.json")).is_file() {
            continue;
        }
        if remote.fetch_ahead(root, &key) {
            let mut state = prefetch.state.lock().unwrap();
            // A task waiting on this fetch may have taken it already.
            if !remote.confirmed.lock().unwrap().contains(&key) {
                state.fetched.insert(key);
            }
            let over = prefetch.received.load(Ordering::Relaxed) > BUDGET_BYTES
                || state.fetched.len() + state.claimed >= BUDGET_ENTRIES;
            drop(state);
            if over {
                prefetch.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(keys: &[&str]) -> Prefetch {
        let prefetch = Prefetch::default();
        prefetch
            .state
            .lock()
            .unwrap()
            .queued
            .extend(keys.iter().map(|key| (*key).to_owned()));
        prefetch
    }

    #[test]
    fn prefetching_stops_once_the_run_goes_its_own_way() {
        let prefetch = queued(&["a", "b"]);
        for key in ["a", "x", "y", "z", "b", "v", "w"] {
            prefetch.resolved(key);
        }
        assert!(!prefetch.stopped.load(Ordering::SeqCst));
        prefetch.resolved("u");
        assert!(prefetch.stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_run_that_follows_keeps_prefetching() {
        let keys: Vec<String> = (0..10).map(|index| index.to_string()).collect();
        let names: Vec<&str> = keys.iter().map(String::as_str).collect();
        let prefetch = queued(&names);
        for key in &names {
            prefetch.resolved(key);
        }
        assert!(!prefetch.stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_prefetched_entry_is_claimed_once() {
        let prefetch = Prefetch::default();
        prefetch.state.lock().unwrap().fetched.insert("a".into());
        assert!(prefetch.holds("a"));
        assert!(prefetch.claim("a"));
        assert!(!prefetch.claim("a"));
        assert_eq!(prefetch.summary(), (1, 1));
    }
}
