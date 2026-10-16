//! Which entries the remote store holds, so that a lookup it would answer with
//! a miss is not made.
//!
//! Each run that writes to the store adds a listing under `index/`: a line of
//! `<key> <milliseconds>` for every entry the run found there or put there,
//! with when it did. A reader lists `index/` and fetches the listings it has
//! not seen, keeping what it learns in the local cache. Once there are many
//! listings, a writer merges them into one and deletes the ones it merged, so
//! a reader's listing request stays small.
//!
//! A listing also records its run: a line `run <id> <milliseconds>` followed
//! by the first 16 hex digits of each key the run used, in the order it used
//! them. Merging keeps the most recent runs, so a reader knows which entries
//! were used together and can fetch them before they are asked for.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;

/// How long a key stays listed after the last run that found its entry.
pub(super) const KEPT_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How many listings a writer leaves before it merges them.
pub(super) const MERGE_AFTER: usize = 16;
/// How many runs the index remembers, the most recent first.
const HISTORY_RUNS: usize = 32;
/// How long the index remembers a run.
const HISTORY_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// The most keys every remembered run names together, and one run alone.
const HISTORY_MEMBERS: usize = 100_000;
const RUN_MEMBERS: usize = 20_000;
/// How many hex digits of a key a run names it by.
pub(super) const MEMBER_DIGITS: usize = 16;

/// What a reader knows of the store's entries.
#[derive(Default)]
pub(super) struct Known {
    /// The listings this is built from.
    pub listings: BTreeSet<String>,
    /// Each key, with when a run last found its entry in the store.
    pub keys: HashMap<String, u64>,
    /// Recent runs, by their id.
    pub runs: BTreeMap<String, Run>,
}

/// A run that wrote to the store: when, and the keys it used, in order, by
/// their first [`MEMBER_DIGITS`] hex digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Run {
    pub time: u64,
    pub members: Vec<String>,
}

impl Known {
    /// Adds a listing's keys, keeping the later time of a key listed twice,
    /// and its runs.
    pub fn add(&mut self, listing: &str) {
        for (key, time) in parse(listing) {
            let known = self.keys.entry(key).or_insert(time);
            *known = (*known).max(time);
        }
        self.runs.extend(parse_runs(listing));
    }

    /// Drops the keys no run has found within [`KEPT_FOR`] of `now`, and the
    /// runs beyond the history's bounds.
    pub fn expire(&mut self, now: u64) {
        let oldest = now.saturating_sub(KEPT_FOR.as_millis() as u64);
        self.keys.retain(|_, time| *time >= oldest);
        let oldest = now.saturating_sub(HISTORY_FOR.as_millis() as u64);
        let mut runs: Vec<(String, Run)> = std::mem::take(&mut self.runs)
            .into_iter()
            .filter(|(_, run)| run.time >= oldest)
            .collect();
        runs.sort_by(|(a, left), (b, right)| right.time.cmp(&left.time).then(b.cmp(a)));
        let mut members = 0;
        for (id, run) in runs.into_iter().take(HISTORY_RUNS) {
            members += run.members.len();
            if members > HISTORY_MEMBERS {
                break;
            }
            self.runs.insert(id, run);
        }
    }

    /// The local copy at `path`, empty when there is none or it cannot be read.
    pub fn load(path: &Path) -> Self {
        let mut known = Self::default();
        let text = std::fs::File::open(path)
            .ok()
            .and_then(|file| super::read_bounded(file, super::LARGEST_INDEX, None).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let Some(text) = text else {
            return known;
        };
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("listing ") {
                known.listings.insert(name.to_owned());
            }
        }
        known.add(&text);
        known
    }

    /// Replaces the local copy at `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let directory = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory)?;
        let mut text = String::new();
        for name in &self.listings {
            text.push_str(&format!("listing {name}\n"));
        }
        text.push_str(&format(
            self.keys.iter().map(|(key, time)| (key.as_str(), *time)),
        ));
        text.push_str(&format_runs(&self.runs));
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.persist(path)?;
        Ok(())
    }
}

/// A listing's keys and times. Lines that are not a key and a time are left
/// out, so a reader never takes a malformed line for an entry.
pub(super) fn parse(listing: &str) -> impl Iterator<Item = (String, u64)> + '_ {
    listing.lines().filter_map(|line| {
        let (key, time) = line.split_once(' ')?;
        let time = time.parse().ok()?;
        crate::record::valid_hash(key).then(|| (key.to_owned(), time))
    })
}

/// A listing's runs. A line that is not a run is left out, and so is any
/// member that is not a key's first [`MEMBER_DIGITS`] hex digits.
pub(super) fn parse_runs(listing: &str) -> impl Iterator<Item = (String, Run)> + '_ {
    listing.lines().filter_map(|line| {
        let mut words = line.strip_prefix("run ")?.split(' ');
        let id = words.next().filter(|id| !id.is_empty())?.to_owned();
        let time = words.next()?.parse().ok()?;
        let members = words
            .filter(|member| {
                member.len() == MEMBER_DIGITS && member.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .take(RUN_MEMBERS)
            .map(str::to_owned)
            .collect();
        Some((id, Run { time, members }))
    })
}

/// The lines of `runs`, one each.
pub(super) fn format_runs(runs: &BTreeMap<String, Run>) -> String {
    let mut text = String::new();
    for (id, run) in runs {
        text.push_str(&format!("run {id} {}", run.time));
        for member in &run.members {
            text.push(' ');
            text.push_str(member);
        }
        text.push('\n');
    }
    text
}

/// The name a run gives `key`.
pub(super) fn member(key: &str) -> &str {
    &key[..MEMBER_DIGITS.min(key.len())]
}

/// The listing of `keys`, one line each.
pub(super) fn format<'a>(keys: impl Iterator<Item = (&'a str, u64)>) -> String {
    let mut lines: Vec<String> = keys.map(|(key, time)| format!("{key} {time}\n")).collect();
    lines.sort();
    lines.concat()
}

/// A new listing's name: names sort by when they were written, and the rest
/// keeps two runs writing at once apart.
pub(super) fn name(now: u64) -> String {
    static NAMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = blake3::hash(
        format!(
            "{}\0{:?}\0{}",
            std::process::id(),
            SystemTime::now(),
            NAMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
        .as_bytes(),
    );
    format!("{now:013}-{}", &unique.to_hex()[..16])
}

/// Milliseconds since the Unix epoch.
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    #[test]
    fn listings_merge_keeping_the_latest_time_and_skip_malformed_lines() {
        let mut known = Known::default();
        known.add(&format(
            [(key('a').as_str(), 5), (key('b').as_str(), 7)].into_iter(),
        ));
        known.add(&format!(
            "{} 9\nnot a line\n{} soon\nshort 3\n",
            key('a'),
            key('c')
        ));
        assert_eq!(known.keys.len(), 2);
        assert_eq!(known.keys[&key('a')], 9);
        assert_eq!(known.keys[&key('b')], 7);
    }

    #[test]
    fn keys_not_found_lately_expire() {
        let mut known = Known::default();
        let now = KEPT_FOR.as_millis() as u64 * 2;
        known.add(&format(
            [(key('a').as_str(), now - 1), (key('b').as_str(), 1)].into_iter(),
        ));
        known.expire(now);
        assert_eq!(known.keys.keys().collect::<Vec<_>>(), [&key('a')]);
    }

    #[test]
    fn the_local_copy_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote/store");
        let mut known = Known::default();
        known.listings.insert("0000000000001-abc".into());
        known.add(&format([(key('a').as_str(), 3)].into_iter()));
        known.save(&path).unwrap();
        let loaded = Known::load(&path);
        assert_eq!(loaded.listings, known.listings);
        assert_eq!(loaded.keys, known.keys);
        assert!(
            Known::load(&directory.path().join("missing"))
                .keys
                .is_empty()
        );
    }

    #[test]
    fn runs_round_trip_and_keep_their_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store");
        let mut known = Known::default();
        let members = [key('b'), key('a')].map(|key| member(&key).to_owned());
        let listing = format!(
            "{}{}",
            format([(key('a').as_str(), 3), (key('b').as_str(), 3)].into_iter()),
            format_runs(&BTreeMap::from([(
                "0000000000003-x".to_owned(),
                Run {
                    time: 3,
                    members: members.to_vec(),
                }
            )]))
        );
        known.add(&listing);
        assert_eq!(known.runs["0000000000003-x"].members, members);
        known.save(&path).unwrap();
        assert_eq!(Known::load(&path).runs, known.runs);
    }

    #[test]
    fn malformed_runs_and_members_are_left_out() {
        let mut known = Known::default();
        known.add(&format!(
            "run\nrun  5\nrun a soon\nrun b 5 {} zz {}\n",
            "1".repeat(16),
            "2".repeat(64)
        ));
        assert_eq!(
            known.runs,
            BTreeMap::from([(
                "b".to_owned(),
                Run {
                    time: 5,
                    members: vec!["1".repeat(16)],
                }
            )])
        );
    }

    #[test]
    fn the_history_keeps_recent_runs_within_its_bounds() {
        let now = HISTORY_FOR.as_millis() as u64 * 2;
        let mut known = Known::default();
        for index in 0..HISTORY_RUNS as u64 + 5 {
            known.runs.insert(
                format!("{index:013}"),
                Run {
                    time: now - index,
                    members: vec!["0".repeat(16)],
                },
            );
        }
        known.runs.insert(
            "old".into(),
            Run {
                time: 1,
                members: Vec::new(),
            },
        );
        known.expire(now);
        assert_eq!(known.runs.len(), HISTORY_RUNS);
        assert!(!known.runs.contains_key("old"));
        assert!(known.runs.contains_key(&format!("{:013}", 0)));
        assert!(!known.runs.contains_key(&format!("{:013}", HISTORY_RUNS)));

        let mut known = Known::default();
        for index in 0..3 {
            known.runs.insert(
                format!("{index}"),
                Run {
                    time: now - index,
                    members: vec!["0".repeat(16); HISTORY_MEMBERS / 2],
                },
            );
        }
        known.expire(now);
        assert_eq!(known.runs.len(), 2);
    }

    #[test]
    fn names_sort_by_time() {
        assert!(name(999) < name(1000));
        assert_ne!(name(5), name(5));
    }
}
