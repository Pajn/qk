//! Stamps: what of a file's metadata changes whenever what it holds can have
//! changed, so that a file or directory whose stamp is unchanged need not be
//! read again. A stamp stands in for what it holds only once it has settled:
//! see [`crate::settle`].

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Metadata that changes whenever a file's content can have changed: size,
/// modification time and, on Unix, change time, inode and mode. The stamp
/// [`look`] takes of a symlink has its target's stamp after its own.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp(pub Vec<i64>);

/// How many values one file's stamp has.
#[cfg(unix)]
const WIDTH: usize = 6;
#[cfg(not(unix))]
const WIDTH: usize = 2;

impl Stamp {
    pub fn new(metadata: &std::fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(-1, |duration| duration.as_nanos() as i64);
        #[allow(unused_mut)]
        let mut values = vec![metadata.len() as i64, modified];
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            values.extend([
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.ino() as i64,
                i64::from(metadata.mode()),
            ]);
        }
        Self(values)
    }

    /// Whether a stamp [`look`] took, with the stamp of a symlink's target
    /// after its own, was older than `window` at `looked`: see
    /// [`Self::settled`].
    pub fn look_settled(&self, window: Duration, looked: SystemTime) -> bool {
        self.0
            .as_chunks::<WIDTH>()
            .0
            .iter()
            .all(|values| times_settled(values, window, looked))
    }

    /// Whether every time in this stamp is more than `window`, as
    /// [`crate::settle::window`] gives it for the file's filesystem, before
    /// `looked`, a time taken before the file was read: a write within the
    /// window of the last could leave the stamp as it was, so a file that
    /// changed more recently is always read again. The change time counts
    /// too, as tools set modification times back but nothing sets change
    /// times back.
    pub fn settled(&self, window: Duration, looked: SystemTime) -> bool {
        times_settled(&self.0, window, looked)
    }
}

/// [`Stamp::settled`] of the values of one file's stamp.
fn times_settled(values: &[i64], window: Duration, looked: SystemTime) -> bool {
    let modified = SystemTime::UNIX_EPOCH + Duration::from_nanos(values[1].max(0) as u64);
    if values[1] < 0 || !crate::settle::older(modified, window, looked) {
        return false;
    }
    #[cfg(unix)]
    {
        let changed = SystemTime::UNIX_EPOCH
            + Duration::new(
                values[2].max(0) as u64,
                values[3].clamp(0, 999_999_999) as u32,
            );
        values[2] >= 0 && crate::settle::older(changed, window, looked)
    }
    #[cfg(not(unix))]
    true
}

/// The stamp of `path`, a directory or file, `None` when there is nothing
/// there; for a symlink, its own with that of the target it leads to now
/// after it, so retargeting any link of the chain or editing the target in
/// place changes it. With it, the window of [`crate::settle::window`] that
/// covers each file it stamps.
pub fn look(path: &Path) -> Result<Option<(Stamp, Duration)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut stamp = Stamp::new(&metadata);
            let mut window = crate::settle::window(path, &metadata);
            if metadata.is_symlink() {
                match std::fs::metadata(path) {
                    Ok(target) => {
                        stamp.0.extend(Stamp::new(&target).0);
                        // Asked of the target's own filesystem, which holds a
                        // regular file or directory and is not followed again.
                        window = window.max(match std::fs::canonicalize(path) {
                            Ok(resolved) => crate::settle::window(&resolved, &target),
                            Err(_) => crate::settle::FALLBACK,
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => stamp.0[1] = -1,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some((stamp, window)))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}
