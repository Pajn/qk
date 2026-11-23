//! Stamps: what of a file's metadata changes whenever what it holds can have
//! changed, so that a file or directory whose stamp is unchanged need not be
//! read again. A stamp stands in for what it holds only once it has settled:
//! see [`crate::settle`].

use std::path::{Path, PathBuf};
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

    /// Whether [`look`] took this stamp of a symlink, so that it holds the
    /// stamps of what the link leads through and to after the link's own.
    pub fn through_link(&self) -> bool {
        self.0.len() > WIDTH
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
/// there; for a symlink, its own with, after it, that of each link met on
/// the way to what it leads to and of what is reached, so retargeting any
/// link on that way, or editing the target in place, changes it. With it,
/// the window of [`crate::settle::window`] that covers each file it stamps.
pub fn look(path: &Path) -> Result<Option<(Stamp, Duration)>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    let mut stamp = Stamp::new(&metadata);
    let mut window = crate::settle::window(path, &metadata);
    if metadata.is_symlink() && !follow(path, &mut stamp, &mut window)? {
        // It leads nowhere, or through more links than are followed.
        stamp.0[1] = -1;
    }
    Ok(Some((stamp, window)))
}

/// Whether `error` says that nothing is at a path.
fn absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Follows the link at `link` to what it leads to, a name at a time, adding
/// to `stamp` the stamp of each link met on the way and of what is reached,
/// and widening `window` to cover them. Whether anything was reached.
///
/// A link is met where the path ends and also among the directories it
/// leads through: `rules -> shared/rules` reads another file once `shared`,
/// itself a link, is pointed elsewhere, though `rules` and both files are
/// left as they were. So the path is not handed to the system whole, which
/// would follow such a link unseen.
fn follow(link: &Path, stamp: &mut Stamp, window: &mut Duration) -> Result<bool> {
    // The directory reached so far, through no link left to follow, and the
    // names still to take from it, the next one last.
    let mut reached = link.parent().unwrap_or(Path::new("")).to_owned();
    let mut ahead: Vec<std::ffi::OsString> = Vec::new();
    // Puts what a link names in place of the link.
    fn splice(target: &Path, reached: &mut PathBuf, ahead: &mut Vec<std::ffi::OsString>) {
        if target.is_absolute() {
            reached.clear();
        }
        let mut names = Vec::new();
        for component in target.components() {
            match component {
                std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                    reached.push(component.as_os_str());
                }
                name => names.push(name.as_os_str().to_owned()),
            }
        }
        ahead.extend(names.into_iter().rev());
    }
    let mut note = |path: &Path, metadata: &std::fs::Metadata| {
        stamp.0.extend(Stamp::new(metadata).0);
        *window = (*window).max(crate::settle::window(path, metadata));
    };
    let Ok(target) = std::fs::read_link(link) else {
        return Ok(false);
    };
    splice(&target, &mut reached, &mut ahead);
    let mut followed = 1;
    while let Some(name) = ahead.pop() {
        if name == "." {
            continue;
        }
        if name == ".." {
            reached.pop();
            continue;
        }
        let candidate = reached.join(&name);
        let metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if absent(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.is_symlink() {
            followed += 1;
            // A chain too long to follow leads nowhere, as a loop does.
            if followed > LINKS {
                return Ok(false);
            }
            note(&candidate, &metadata);
            let Ok(target) = std::fs::read_link(&candidate) else {
                return Ok(false);
            };
            splice(&target, &mut reached, &mut ahead);
        } else if ahead.is_empty() {
            note(&candidate, &metadata);
            return Ok(true);
        } else {
            reached = candidate;
        }
    }
    // The link named a directory by `.` and `..` alone.
    match std::fs::symlink_metadata(&reached) {
        Ok(metadata) => {
            note(&reached, &metadata);
            Ok(true)
        }
        Err(error) if absent(&error) => Ok(false),
        Err(error) => Err(error.into()),
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
        assert!(before.through_link());
        assert!(!look(&at("one")).unwrap().unwrap().0.through_link());
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

    /// A link among the directories a link leads through is stamped too:
    /// pointed elsewhere and back again, it is another link, though the
    /// first link and the file reached are as they were.
    #[test]
    fn a_look_stamps_the_links_among_the_directories_it_leads_through() {
        let directory = tempfile::tempdir().unwrap();
        let at = |name: &str| directory.path().join(name);
        for name in ["old", "new", "old/sub"] {
            std::fs::create_dir(at(name)).unwrap();
        }
        std::fs::write(at("old/rules"), "same").unwrap();
        std::fs::write(at("new/rules"), "same").unwrap();
        symlink("old", at("via")).unwrap();
        symlink("via/rules", at("first")).unwrap();
        let (before, _) = look(&at("first")).unwrap().unwrap();
        // The link, the linked directory and the file.
        assert_eq!(before.0.len(), 3 * WIDTH);

        let point = |to: &str| {
            std::fs::remove_file(at("via")).unwrap();
            symlink(to, at("via")).unwrap();
        };
        point("new");
        let (elsewhere, _) = look(&at("first")).unwrap().unwrap();
        assert_ne!(elsewhere, before);
        point("old");
        let (back, _) = look(&at("first")).unwrap().unwrap();
        assert_eq!(back.0[..WIDTH], before.0[..WIDTH]);
        assert_eq!(back.0[2 * WIDTH..], before.0[2 * WIDTH..]);
        assert_ne!(back, before);

        // The same link moved aside and put back, with another in its place
        // meanwhile, keeps everything but its change time, which a stamp
        // holds on Unix.
        std::fs::rename(at("via"), at("aside")).unwrap();
        symlink("new", at("via")).unwrap();
        std::fs::remove_file(at("via")).unwrap();
        std::fs::rename(at("aside"), at("via")).unwrap();
        let (returned, _) = look(&at("first")).unwrap().unwrap();
        assert_ne!(returned, back);
        let back = returned;

        // `..` is taken from where the links before it led, and an absolute
        // target from the top.
        symlink("via/sub/../rules", at("around")).unwrap();
        let (around, _) = look(&at("around")).unwrap().unwrap();
        assert_eq!(around.0[WIDTH..], back.0[WIDTH..]);
        // By its real path, as the directory's own may lead through links.
        let real = directory.path().canonicalize().unwrap();
        symlink(real.join("old/rules"), at("absolute")).unwrap();
        let (absolute, _) = look(&at("absolute")).unwrap().unwrap();
        assert_eq!(absolute.0.len(), 2 * WIDTH);
        assert_eq!(absolute.0[WIDTH..], back.0[2 * WIDTH..]);
        // A link naming its own directory reaches that directory.
        symlink(".", at("here")).unwrap();
        let (here, _) = look(&at("here")).unwrap().unwrap();
        let (plain, _) = look(directory.path()).unwrap().unwrap();
        assert_eq!(here.0[WIDTH..], plain.0);
    }
}
