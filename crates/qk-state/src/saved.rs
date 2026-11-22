//! Files a run saves for the runs that follow: a compact form for writing
//! them, a digest sealing each so that one damaged or cut short is never
//! read, and saving that moves a whole file into place and keeps only the
//! most recent few.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// `body` followed by its digest, for [`unseal`].
pub fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let digest = blake3::hash(&body);
    body.extend_from_slice(digest.as_bytes());
    body
}

/// What [`seal`] sealed in `bytes`, when they are whole.
pub fn unseal(bytes: &[u8]) -> Option<&[u8]> {
    let (body, digest) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    (blake3::hash(body).as_bytes() == digest).then_some(body)
}

/// Saves `bytes` as `path`, replacing what was there, and keeps in its
/// directory only the `kept` most recently saved files whose names start
/// with `prefix`, which `path`'s must. Two runs may save at once: each writes
/// a file of its own and moves it into place, so a run reading it finds one
/// or the other whole.
pub fn save(path: &Path, bytes: &[u8], prefix: &str, kept: usize) -> Result<()> {
    let directory = path.parent().context("a saved file has a directory")?;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.persist(path)?;
    prune(directory, prefix, kept);
    Ok(())
}

/// Removes all but the `kept` most recently saved files starting with
/// `prefix`, and files left by a save that was stopped while writing.
fn prune(directory: &Path, prefix: &str, kept: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    let mut saved = Vec::new();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            saved.push((modified, entry.path()));
        } else if now
            .duration_since(modified)
            .is_ok_and(|age| age > Duration::from_secs(3600))
        {
            // Another run's file being written is never this old.
            let _ = std::fs::remove_file(entry.path());
        }
    }
    saved.sort();
    for (_, path) in saved.iter().rev().skip(kept) {
        let _ = std::fs::remove_file(path);
    }
}

/// Writes the parts of a saved file.
#[derive(Default)]
pub struct Writer {
    pub bytes: Vec<u8>,
}

impl Writer {
    /// `value` in seven-bit groups, least significant first, each but the
    /// last with its top bit set.
    pub fn number(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.bytes.push(value as u8);
    }

    /// `string`, as the length of what it shares with `previous` and the
    /// rest: sorted paths share most of themselves with the one before.
    pub fn string(&mut self, previous: &str, string: &str) {
        let shared = previous
            .bytes()
            .zip(string.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        self.number(shared as u64);
        self.number((string.len() - shared) as u64);
        self.bytes.extend_from_slice(&string.as_bytes()[shared..]);
    }
}

/// Reads what [`Writer`] wrote, `None` past its end or where it does not
/// read.
pub struct Reader<'a> {
    pub bytes: &'a [u8],
}

impl Reader<'_> {
    pub fn take(&mut self, count: usize) -> Option<&[u8]> {
        let (taken, rest) = self.bytes.split_at_checked(count)?;
        self.bytes = rest;
        Some(taken)
    }

    pub fn number(&mut self) -> Option<u64> {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7f).checked_shl(shift)?;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// A count of things that follow, each at least a byte long: never more
    /// than the bytes left, so a damaged count cannot ask for much memory.
    pub fn count(&mut self) -> Option<usize> {
        usize::try_from(self.number()?)
            .ok()
            .filter(|count| *count <= self.bytes.len())
    }

    pub fn string(&mut self, previous: &str) -> Option<String> {
        let shared = usize::try_from(self.number()?).ok()?;
        let rest = self.count()?;
        let mut string = previous.as_bytes().get(..shared)?.to_vec();
        string.extend_from_slice(self.take(rest)?);
        String::from_utf8(string).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_reads_back() {
        let mut writer = Writer::default();
        writer.number(0);
        writer.number(300);
        writer.number(u64::MAX);
        writer.string("", "packages/a");
        writer.string("packages/a", "packages/b/c");
        let sealed = seal(writer.bytes);
        let body = unseal(&sealed).unwrap();
        let mut reader = Reader { bytes: body };
        assert_eq!(reader.number(), Some(0));
        assert_eq!(reader.number(), Some(300));
        assert_eq!(reader.number(), Some(u64::MAX));
        assert_eq!(reader.string("").as_deref(), Some("packages/a"));
        assert_eq!(reader.string("packages/a").as_deref(), Some("packages/b/c"));
        assert!(reader.bytes.is_empty());
        assert_eq!(reader.number(), None);
    }

    #[test]
    fn a_damaged_or_short_file_is_not_read() {
        let sealed = seal(b"body".to_vec());
        let mut damaged = sealed.clone();
        damaged[0] ^= 1;
        assert!(unseal(&damaged).is_none());
        assert!(unseal(&sealed[..sealed.len() - 1]).is_none());
        assert!(unseal(&[]).is_none());
    }

    #[test]
    fn only_the_most_recent_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..4 {
            let path = directory.path().join(format!("kept-{index}"));
            save(&path, b"saved", "kept-", 2).unwrap();
            // Distinct modification times order them.
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + index))
                .unwrap();
        }
        save(&directory.path().join("kept-4"), b"saved", "kept-", 2).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["kept-3", "kept-4"]);
    }
}
