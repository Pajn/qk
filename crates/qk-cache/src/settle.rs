//! How long after a time on a file another write could still leave that same
//! time, for the filesystem the file is on.
//!
//! A file's stamp (size, times, and on Unix inode and mode) stands in for its
//! contents, so a file whose stamp is unchanged is not read again. That holds
//! only once a later write is certain to carry a later time: two writes close
//! enough together can be given the same time, either because the filesystem
//! stores times coarsely or because the clock it reads them from advances in
//! steps. A stamp is trusted only when its times are older than this window,
//! measured on this machine's clock from before the file was looked at.
//!
//! The window is short only on filesystems known to store fine times taken
//! from this machine's clock:
//!
//! - Linux ext4, XFS, Btrfs and tmpfs store nanoseconds (each sets the
//!   superblock's `s_time_gran` to 1; ext4 only with inodes large enough for
//!   the extra time fields, and otherwise to whole seconds, which the zero
//!   fraction rule below catches). The kernel stamps a write with
//!   `current_time()`, which before multigrain timestamps (6.13) reads the
//!   coarse clock, `ktime_get_coarse_real_ts64()`, and since then still does
//!   unless the file's times were read since its last change (see the
//!   kernel's `Documentation/filesystems/multigrain-ts.rst`). The coarse clock
//!   advances once per tick, at most 10 ms at the lowest `CONFIG_HZ` of 100,
//!   and since 5.12 any ticking CPU takes over updating it once it stalls for
//!   a few ticks (`MAX_STALLED_JIFFIES` in `kernel/time/tick-sched.c`). The
//!   window, 100 ms, covers ten ticks at 100 Hz.
//! - macOS APFS stores nanoseconds (Apple's "Apple File System Reference",
//!   `j_inode_val_t`), and the time a write carries is read from the
//!   precise clock as it happens: in back-to-back writes and `fstat` calls
//!   on one file, no two writes shared a time, and none carried a time
//!   before the clock read just ahead of it. The window, 10 ms, is that
//!   clock's step many times over.
//!
//! Everything else keeps two seconds, which covers FAT's two-second times:
//! network filesystems, whose times come from another machine's clock;
//! FUSE, 9p, virtiofs and overlay filesystems, whose real resolution is that
//! of whatever lies beneath; HFS+ and every other filesystem not listed;
//! Windows and other platforms; and any file whose filesystem cannot be
//! told. Too long a window only costs reading a file again, too short a one
//! trusts a stale digest, so every doubt resolves to two seconds.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// The window wherever the filesystem is not positively known.
pub(crate) const FALLBACK: Duration = Duration::from_secs(2);

/// The window for the file at `path`, whose metadata, taken without following
/// a final symlink, is `metadata`.
///
/// A time with no fraction of a second may come from a filesystem storing
/// whole seconds even where its kind can store finer ones, so such a file
/// keeps the fallback whatever its filesystem.
pub(crate) fn window(path: &Path, metadata: &std::fs::Metadata) -> Duration {
    if whole_seconds(metadata) {
        return FALLBACK;
    }
    platform::window(path, metadata)
}

/// Whether `time`, read from a file, is more than `window` before `looked`,
/// the time on this machine's clock before the file was looked at. A time
/// after `looked`, as another machine's clock can give, never is.
pub(crate) fn older(time: SystemTime, window: Duration, looked: SystemTime) -> bool {
    looked.duration_since(time).is_ok_and(|age| age > window)
}

fn whole_seconds(metadata: &std::fs::Metadata) -> bool {
    let fraction = |time: std::io::Result<SystemTime>| {
        time.ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|since| since.subsec_nanos())
    };
    #[allow(unused_mut)]
    let mut whole = fraction(metadata.modified()) == Some(0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        whole |= metadata.ctime_nsec() == 0;
    }
    whole
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::{LazyLock, RwLock};
    use std::time::Duration;

    use super::FALLBACK;

    /// The window found for each filesystem, by device: many thousands of
    /// files are checked in a run, on few filesystems.
    static WINDOWS: LazyLock<RwLock<HashMap<u64, Duration>>> = LazyLock::new(Default::default);

    thread_local! {
        /// The last device a thread asked about and its window: a thread
        /// mostly looks at files of one filesystem, and so does not contend
        /// with the others for `WINDOWS`.
        static LAST: std::cell::Cell<Option<(u64, Duration)>> =
            const { std::cell::Cell::new(None) };
    }

    pub(super) fn window(path: &Path, metadata: &std::fs::Metadata) -> Duration {
        let device = metadata.dev();
        if let Some((last, window)) = LAST.get()
            && last == device
        {
            return window;
        }
        let known = WINDOWS.read().unwrap().get(&device).copied();
        // Not remembered when the filesystem cannot be told, so one file that
        // cannot be opened does not decide for the rest.
        let window = match known {
            Some(window) => window,
            None => match probe(path, metadata, device) {
                Some(window) => *WINDOWS.write().unwrap().entry(device).or_insert(window),
                None => return FALLBACK,
            },
        };
        LAST.set(Some((device, window)));
        window
    }

    /// The window of the filesystem holding `path`, asked through a handle
    /// checked to be on `device`: anything but a regular file or directory is
    /// asked through its parent, which holds its entry, so that a symlink is
    /// not followed and a pipe is not opened.
    fn probe(path: &Path, metadata: &std::fs::Metadata, device: u64) -> Option<Duration> {
        use std::os::unix::fs::OpenOptionsExt;
        let target = if metadata.is_file() || metadata.is_dir() {
            path
        } else {
            path.parent()?
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(target)
            .ok()?;
        if file.metadata().ok()?.dev() != device {
            return None;
        }
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        use std::os::fd::AsRawFd;
        // SAFETY: `stat` is written whole by a successful call, which is the
        // only case it is read in.
        if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: as above.
        Some(classify(&unsafe { stat.assume_init() }))
    }

    #[cfg(target_os = "linux")]
    fn classify(stat: &libc::statfs) -> Duration {
        // `f_type`'s width differs by architecture; every magic fits 32 bits.
        named_window(stat.f_type as u32)
    }

    /// The window of a Linux filesystem by its magic number, from
    /// `include/uapi/linux/magic.h`.
    #[cfg(target_os = "linux")]
    pub(super) fn named_window(magic: u32) -> Duration {
        const EXT4: u32 = 0xEF53;
        const XFS: u32 = 0x5846_5342;
        const BTRFS: u32 = 0x9123_683E;
        const TMPFS: u32 = 0x0102_1994;
        match magic {
            EXT4 | XFS | BTRFS | TMPFS => Duration::from_millis(100),
            _ => FALLBACK,
        }
    }

    #[cfg(target_os = "macos")]
    fn classify(stat: &libc::statfs) -> Duration {
        let name: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        named_window(&name, stat.f_flags & libc::MNT_LOCAL as u32 != 0)
    }

    /// The window of a macOS filesystem by its type name, and whether it is
    /// mounted from this machine.
    #[cfg(target_os = "macos")]
    pub(super) fn named_window(name: &[u8], local: bool) -> Duration {
        if local && name == b"apfs" {
            Duration::from_millis(10)
        } else {
            FALLBACK
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::path::Path;
    use std::time::Duration;

    /// Windows and the rest keep the fallback: NTFS stores 100 ns, but the
    /// times of writes through a handle may be set late, and nothing here
    /// tells NTFS from FAT or a network share.
    pub(super) fn window(_: &Path, _: &std::fs::Metadata) -> Duration {
        super::FALLBACK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn only_known_local_filesystems_are_short() {
        #[cfg(target_os = "linux")]
        {
            for magic in [0xEF53, 0x5846_5342, 0x9123_683E, 0x0102_1994] {
                assert_eq!(platform::named_window(magic), Duration::from_millis(100));
            }
            // NFS, SMB, FUSE, 9p, virtiofs, overlay, FAT, an unknown magic.
            for magic in [
                0x6969,
                0xFE53_4D42,
                0x6573_5546,
                0x0102_1997,
                0x6A65_6A63,
                0x794C_7630,
                0x4D44,
                0x1234_5678,
            ] {
                assert_eq!(platform::named_window(magic), FALLBACK);
            }
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                platform::named_window(b"apfs", true),
                Duration::from_millis(10)
            );
            assert_eq!(platform::named_window(b"apfs", false), FALLBACK);
            for name in [
                &b"hfs"[..],
                b"msdos",
                b"exfat",
                b"smbfs",
                b"nfs",
                b"macfuse",
            ] {
                assert_eq!(platform::named_window(name, true), FALLBACK);
            }
        }
    }

    #[test]
    fn whole_second_times_keep_the_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "a").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(window(&path, &metadata), FALLBACK);
    }

    #[test]
    fn a_window_is_the_same_for_every_file_of_a_filesystem() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::write(&first, "a").unwrap();
        std::fs::write(&second, "b").unwrap();
        let window_of = |path: &Path| window(path, &std::fs::symlink_metadata(path).unwrap());
        assert_eq!(window_of(&first), window_of(&second));
        assert!(window_of(&first) <= FALLBACK);
        // A symlink is asked about through its directory, never followed.
        #[cfg(unix)]
        {
            let link = directory.path().join("link");
            std::os::unix::fs::symlink("missing", &link).unwrap();
            assert!(window_of(&link) <= FALLBACK);
        }
    }

    #[test]
    fn a_time_must_be_older_than_the_window() {
        let looked = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let window = Duration::from_millis(10);
        assert!(older(looked - Duration::from_millis(11), window, looked));
        assert!(!older(looked - window, window, looked));
        assert!(!older(looked - Duration::from_millis(1), window, looked));
        // From a clock ahead of this one.
        assert!(!older(looked + Duration::from_secs(1), window, looked));
    }
}
