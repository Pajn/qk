//! Stamps: what of a file's metadata changes whenever what it holds can have
//! changed, so that a file or directory whose stamp is unchanged need not be
//! read again. A stamp stands in for what it holds only once it has settled:
//! see [`crate::settle`].

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Metadata that changes whenever a file's content can have changed: size,
/// modification time and, on Unix, change time, inode and mode. The stamp
/// [`look`] takes of a symlink has, after its own, the stamp of each link it
/// leads through and of its target.
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

    /// Whether a stamp [`look`] took, with the stamps of what a symlink
    /// leads through and to after its own, was older than `window` at
    /// `looked`: see [`Self::settled`].
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

/// How many links [`look`] follows from one to the next before it takes the
/// chain for a loop, as the system does.
const LINKS: usize = 40;

/// The stamp of `path`, a directory or file, `None` when there is nothing
/// there; for a symlink, its own with, after it, that of each link it leads
/// through and of the target they lead to now, so retargeting any link of
/// the chain or editing the target in place changes it. With it, the window
/// of [`crate::settle::window`] that covers each file it stamps.
pub fn look(path: &Path) -> Result<Option<(Stamp, Duration)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut stamp = Stamp::new(&metadata);
            let mut window = crate::settle::window(path, &metadata);
            // Each link is stamped as it is followed: one in the middle of a
            // chain can be pointed elsewhere while the first link and the
            // file reached both keep their stamps.
            let mut link = path.to_owned();
            let mut metadata = metadata;
            let mut followed = 0;
            while metadata.is_symlink() {
                followed += 1;
                let next = match std::fs::read_link(&link) {
                    Ok(target) => link.parent().unwrap_or(Path::new("")).join(target),
                    Err(_) => {
                        stamp.0[1] = -1;
                        break;
                    }
                };
                match std::fs::symlink_metadata(&next) {
                    Ok(target) if followed < LINKS => {
                        stamp.0.extend(Stamp::new(&target).0);
                        window = window.max(crate::settle::window(&next, &target));
                        link = next;
                        metadata = target;
                    }
                    // A chain too long to follow leads nowhere.
                    Ok(_) => {
                        stamp.0[1] = -1;
                        break;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        stamp.0[1] = -1;
                        break;
                    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A link in the middle of a chain pointed elsewhere changes the stamp
    /// of the chain's first link, though that link and both files are left
    /// as they were.
    #[test]
    fn a_look_stamps_every_link_it_follows() {
        let directory = tempfile::tempdir().unwrap();
        let at = |name: &str| directory.path().join(name);
        std::fs::write(at("one"), "same").unwrap();
        std::fs::write(at("two"), "same").unwrap();
        symlink("one", at("middle")).unwrap();
        symlink("middle", at("first")).unwrap();
        let (before, _) = look(&at("first")).unwrap().unwrap();
        // The link's own stamp, the middle link's and the file's.
        assert_eq!(before.0.len(), 3 * WIDTH);
        assert_eq!(before, look(&at("first")).unwrap().unwrap().0);

        std::fs::remove_file(at("middle")).unwrap();
        symlink("two", at("middle")).unwrap();
        let (after, _) = look(&at("first")).unwrap().unwrap();
        assert_eq!(after.0[..WIDTH], before.0[..WIDTH]);
        assert_ne!(after, before);

        // A chain that leads nowhere, or back to itself, is stamped as such.
        std::fs::remove_file(at("two")).unwrap();
        assert_eq!(look(&at("first")).unwrap().unwrap().0.0[1], -1);
        symlink("loop", at("loop")).unwrap();
        assert_eq!(look(&at("loop")).unwrap().unwrap().0.0[1], -1);
        assert!(look(&at("absent")).unwrap().is_none());
    }
}
