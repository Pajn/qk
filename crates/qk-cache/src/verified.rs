//! The blobs whose contents have been hashed since the machine started.
//!
//! A restore copies a blob, cloning it where the filesystem can, and hashes
//! the copy to catch a blob the disk lost: a crash can keep a blob's name and
//! lose its contents, since qk leaves flushing to the system. Within one boot
//! a blob that hashed right once stays right, so each is hashed once per boot
//! rather than on every restore. The list is kept under the cache, named by
//! the boot, and a new boot starts an empty one.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) struct Verified {
    held: Mutex<HashSet<String>>,
    /// Appended to as blobs are verified; none where the boot cannot be told,
    /// and every restore then hashes what it copies.
    file: Option<Mutex<File>>,
}

impl Verified {
    pub(crate) fn load(cache: &Path) -> Self {
        let none = || Self {
            held: Mutex::new(HashSet::new()),
            file: None,
        };
        let Some(boot) = boot() else {
            return none();
        };
        let directory = cache.join("verified");
        if fs::create_dir_all(&directory).is_err() {
            return none();
        }
        let path: PathBuf = directory.join(&boot);
        // The lists of earlier boots vouch for nothing now.
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            if entry.file_name() != boot.as_str() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let held = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            // A line a crash cut short is no hash.
            .filter(|line| line.len() == 64 && line.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_owned)
            .collect();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new);
        Self {
            held: Mutex::new(held),
            file,
        }
    }

    /// Whether this boot can be told, and blobs verified in it remembered.
    pub(crate) fn available(&self) -> bool {
        self.file.is_some()
    }

    pub(crate) fn contains(&self, blob: &str) -> bool {
        self.available() && self.held.lock().unwrap().contains(blob)
    }

    /// Notes a blob whose contents matched its hash. Each line is one append,
    /// so runs noting blobs at once do not interleave them.
    pub(crate) fn record(&self, blob: &str) {
        let Some(file) = &self.file else { return };
        if self.held.lock().unwrap().insert(blob.to_owned()) {
            let _ = file
                .lock()
                .unwrap()
                .write_all(format!("{blob}\n").as_bytes());
        }
    }
}

/// What tells this boot from others, as a file name.
fn boot() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let id = id.trim();
        (!id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        .then(|| id.to_owned())
    }
    #[cfg(target_os = "macos")]
    {
        // When the system started, to the microsecond.
        let mut time = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of::<libc::timeval>();
        let found = unsafe {
            libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                (&mut time as *mut libc::timeval).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (found == 0 && time.tv_sec > 0).then(|| format!("{}.{:06}", time.tv_sec, time.tv_usec))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verified_blob_is_remembered_by_later_runs_of_the_same_boot() {
        let cache = tempfile::tempdir().unwrap();
        let blob = "a".repeat(64);
        let first = Verified::load(cache.path());
        if boot().is_none() {
            // Nothing is remembered where the boot cannot be told.
            first.record(&blob);
            assert!(!first.contains(&blob));
            return;
        }
        assert!(!first.contains(&blob));
        first.record(&blob);
        assert!(first.contains(&blob));
        drop(first);
        assert!(Verified::load(cache.path()).contains(&blob));
    }

    #[test]
    fn the_lists_of_other_boots_are_dropped_and_cut_lines_ignored() {
        let cache = tempfile::tempdir().unwrap();
        let Some(boot) = boot() else { return };
        let directory = cache.path().join("verified");
        fs::create_dir_all(&directory).unwrap();
        let blob = "b".repeat(64);
        fs::write(directory.join("an-earlier-boot"), format!("{blob}\n")).unwrap();
        fs::write(
            directory.join(&boot),
            format!("{}\n{}", "c".repeat(64), &blob[..20]),
        )
        .unwrap();
        let verified = Verified::load(cache.path());
        assert!(!directory.join("an-earlier-boot").exists());
        assert!(verified.contains(&"c".repeat(64)));
        assert!(!verified.contains(&blob));
    }
}
