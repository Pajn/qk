//! Which directories hold a project.json or a package.json, remembered from
//! one load for the next.
//!
//! Finding them walks every directory of the workspace, which costs opening
//! and listing each one however many threads share the work. A load that
//! walks keeps what the walk found, the [`Manifests`], in a record; a later
//! load takes the manifests from the record instead of walking when it can
//! show that a walk would find the same, which costs looking at stamps
//! alone. Whether the workspaces take in a directory's package.json is not
//! remembered: it is decided again on every load, from the patterns read
//! then.
//!
//! # What decides what the walk finds
//!
//! Read from the walker's configuration in
//! [`crate::discovery::project_directories`] and the `ignore` crate it runs
//! on, the walk's result depends on these alone:
//!
//! - The workspace's root, canonical: in the record's identity, so a record
//!   is only taken for the root it was saved for, wherever
//!   `NX_WORKSPACE_ROOT_PATH` or the working directory led.
//! - The names and kinds of the entries of each directory the walk reads,
//!   which say which subdirectories it reads in turn and where a
//!   project.json or package.json is. A directory's stamp changes with its
//!   entries: adding, removing or renaming anything in it, a subdirectory
//!   replaced by a link or a file by a directory, a new `.git` or ignore
//!   file. A link is never followed, so what it leads to does not matter.
//!   Every directory the walk reads is stamped, the root among them.
//! - In each directory the walk reads, its `.gitignore` and `.nxignore`: the
//!   walker reads each by its path, following links, whether or not it is
//!   itself ignored. Each that is there is stamped, with the target of a
//!   link; one being added or removed shows in its directory's stamp.
//! - In each directory the walk reads that holds a `.git` entry,
//!   `.git/info/exclude`, which the walker reads as that path and nothing
//!   else: with `require_git(false)` it never follows a `.git` file to a
//!   linked worktree's directories, so beneath a `.git` file there is
//!   nothing to read. Beneath a `.git` directory, or a link, `.git/info` is
//!   stamped as well as `exclude`, as adding or removing `exclude` changes
//!   `info` and nothing the walk reads; one without `info` is not
//!   remembered.
//! - Above the root, the `.gitignore` files from its parent up to the
//!   repository's top, which the walk adds as rules of its own, and which
//!   directory that top is: the nearest from the root up holding a `.git`.
//!   Whether each directory from the root up holds a `.git` is compared,
//!   following links as that search does, and each `.gitignore` is stamped.
//! - Above the root too, the walker reads every ancestor's `.gitignore`,
//!   `.nxignore` and `.git/info/exclude`: as `parents(false)` is set, their
//!   rules hide nothing, but a pattern that does not parse in one fails the
//!   walk, and so the load. Each is stamped, there or not.
//! - Nothing else: `.ignore` files are not read (`ignore(false)`), the
//!   global excludes file and the Git configuration naming it are not
//!   (`git_global(false)`), and with `require_git(false)` a `.git` neither
//!   starts nor stops any rules, so whether a directory is a repository
//!   matters only for the files above. Hidden entries are walked like any
//!   other, links are not followed, and the names left out wherever they are
//!   (`node_modules`, `.git`, `.qk`, `.nx/cache`, `.nx/workspace-data` and
//!   `.yarn/cache`) are fixed in qk. None of it reads an environment
//!   variable; `NX_INFER_ALL_PACKAGE_JSONS` only changes what the workspaces
//!   take in, which is decided again on every load.
//! - The qk that wrote the record, whose walk may differ: its version and
//!   [`FORMAT`], bumped whenever the walk or the record changes, are in the
//!   identity.
//!
//! # When the stamps are taken, and the settle window
//!
//! A stamp stands in for what it covers only once it has settled (see
//! [`qk_state::settle`]): a change within its filesystem's window of the
//! last could leave it as it was. The time the walk begins is taken before
//! anything is looked at, and a stamp settled by then shows that nothing it
//! covers changed after: a later change carries a later time. So:
//!
//! - What lies outside the directories the walk reads, the files above the
//!   root and where a `.git` is, is looked at before the walk begins, and a
//!   walk is only remembered when those stamps had settled by then. Nothing
//!   stamped holds them, so only a look from before the walk shows that the
//!   walk read them as they are.
//! - What lies within them is stamped after the walk, by the next load: each
//!   is there or not as its directory's entries say, and a directory whose
//!   stamp had settled by the time the walk began has not changed since. A
//!   walk is cheaper to remember that way, as the next load would look at
//!   each of them anyway. That load takes the walk's result only when every
//!   one of those stamps had settled by the time the walk began, and saves
//!   the stamps with the result for the loads after it.
//! - A record with stamps is taken while every stamp is still what it was:
//!   each had settled when the walk began, so an unchanged one is trusted.
//!
//! # When the record does not hold
//!
//! Anything that differs, has not settled, does not read, or is in doubt
//! walks the whole workspace again, as a load without a record does, and
//! remembers that walk. Reading again only the directories whose stamp
//! moved was left out: a walk below one of them needs the ignore rules of
//! every directory above it, as the walker builds them, which only a walk
//! from the root gives exactly.
//!
//! # Where the record is and who uses it
//!
//! The caller names the directory, which belongs to the worktree: see
//! [`crate::Workspace::load_remembering`]. Only a load that reads from disk
//! takes or saves a record; one reading another revision's files through
//! [`crate::Workspace::load_with`] walks, as a record of what is on disk
//! says nothing of a revision.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use qk_state::Stamp;
use qk_state::saved::{Reader, Writer};

use crate::discovery::{Manifests, parent_gitignores};

/// What every record starts with, then its format.
const MAGIC: &[u8; 8] = b"qk-proj\0";
/// Bumped whenever what is recorded, what it means, or what decides the
/// walk's result changes.
const FORMAT: u32 = 1;
/// Records kept per directory, the most recently saved: one for each of the
/// few workspaces a worktree holds.
const KEPT: usize = 4;
/// Names of records begin with this, apart from files being written.
const PREFIX: &str = "projects-";
/// Paths one thread looks at at least, so a short list is not split: a look
/// costs a few microseconds, starting a thread tens of them.
const PATHS_PER_THREAD: usize = 128;
/// The most threads looking at paths at once.
const LOOKERS: usize = 8;

/// What a recorded path was, as it is compared with what is there now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Seen {
    /// Its stamp, with a link's target after its own, or that nothing is
    /// there: see [`qk_state::look`].
    Stamp(Option<Stamp>),
    /// Only whether something is there, following links, and if so whether
    /// it is a directory, as [`presence`] gives it: a `.git` directory's own
    /// stamp moves with nearly every Git command, and only where one is
    /// matters.
    Presence(u8),
}

/// Paths with what each was.
pub(crate) type Looked = Vec<(PathBuf, Seen)>;

/// The walk's result with what it depends on, as saved by one load for the
/// next.
pub(crate) struct Record {
    manifests: Manifests,
    /// When the walk began, before anything was looked at.
    began: SystemTime,
    /// What lies outside the directories the walk read, as it was before
    /// the walk began.
    outside: Looked,
    within: Within,
}

/// What lies within the directories the walk read.
enum Within {
    /// The directories, not yet stamped.
    Read(Vec<PathBuf>),
    /// Each directory and file the walk read there, with its stamp.
    Stamped(Looked),
}

/// What [`Record::holds`] found.
pub(crate) enum Verdict {
    /// A walk would find what the record holds.
    Holds,
    /// So too, and these are the stamps to keep with it: see
    /// [`Record::stamped`].
    Stamped(Looked),
    /// A walk may find something else.
    Changed,
}

/// The digest of everything a record depends on that its stamps do not
/// show: see the module's documentation.
fn identity(root: &Path) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    // Each field is preceded by its length, so no two lists of fields hash
    // alike.
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(MAGIC);
    field(&FORMAT.to_le_bytes());
    field(env!("CARGO_PKG_VERSION").as_bytes());
    field(root.as_os_str().as_encoded_bytes());
    hasher.finalize()
}

/// The file a record of the workspace at `root` is kept in, in `directory`.
fn file(directory: &Path, root: &Path) -> PathBuf {
    let identity = identity(root);
    directory.join(format!("{PREFIX}{}", &identity.to_hex()[..16]))
}

/// The record kept in `directory` for the workspace at the canonical
/// `root`, when one is there and reads whole. Whether it still holds is
/// [`Record::holds`].
pub(crate) fn recall(directory: &Path, root: &Path) -> Option<Record> {
    let bytes = std::fs::read(file(directory, root)).ok()?;
    decode(&bytes, &identity(root))
}

/// Saves `record` of the workspace at the canonical `root` in `directory`.
/// Nothing is saved when a path is not UTF-8: the next load walks again.
pub(crate) fn save(directory: &Path, root: &Path, record: &Record) -> Result<()> {
    let Some(bytes) = encode(record, &identity(root)) else {
        return Ok(());
    };
    qk_state::saved::save(&file(directory, root), &bytes, PREFIX, KEPT)
}

/// What a walk about to begin is remembered with: see [`before_walk`].
pub(crate) struct Before {
    /// When the walk began, before anything was looked at.
    began: SystemTime,
    /// What lies outside the directories the walk reads, as it was then.
    outside: Looked,
    /// The stamp, as it was then, of each directory above the root whose
    /// entries decide what the walk finds: those up to the repository's
    /// top, or every one where no directory from the root up holds a
    /// `.git`.
    above: Vec<(PathBuf, Stamp)>,
}

/// What a walk of the workspace at the canonical `root` about to begin
/// needs remembered before it begins: the time, what lies outside the
/// directories it reads, and the stamps of the directories above the root.
/// `None` when one of those had not settled, as then the walk cannot be
/// remembered.
///
/// A file that is not there has no stamp to show that it appeared. A
/// `.gitignore` or a `.git` put above the root while the walk runs changes
/// what the walk finds, and taken away afterwards it leaves everything
/// looked at here as it was. It does change the stamp of the directory it
/// was put in, so the directories where one would count are stamped here
/// and again once the walk is done: see [`walked`]. Those are the ones from
/// the root's parent up to the repository's top, whose `.gitignore` files
/// the walk adds and where a nearer `.git` would make another top; above
/// the top nothing counts, and without a top a `.git` anywhere above would
/// make one. A workspace that is the repository's top has none. The stamps
/// are not kept afterwards, as whatever else comes and goes in those
/// directories between loads says nothing about the walk.
pub(crate) fn before_walk(root: &Path) -> Option<Before> {
    let began = SystemTime::now();
    let mut outside = Vec::new();
    let mut above = Vec::new();
    // Whether a `.git` was found from the root up to the last directory.
    let mut topped = false;
    for ancestor in root.ancestors() {
        let git = ancestor.join(".git");
        let presence = presence(&git);
        outside.push((git, Seen::Presence(presence)));
        let beneath_top = !topped;
        topped |= presence != 0;
        if ancestor == root {
            continue;
        }
        if beneath_top {
            let look = qk_state::look(ancestor).ok()?;
            if !settled(&look, began) {
                return None;
            }
            above.push((ancestor.to_owned(), look?.0));
        }
        for name in [".gitignore", ".nxignore", ".git/info/exclude"] {
            let path = ancestor.join(name);
            let look = qk_state::look(&path).ok()?;
            if !settled(&look, began) {
                return None;
            }
            outside.push((path, Seen::Stamp(look.map(|(stamp, _)| stamp))));
        }
    }
    // The parent `.gitignore` files the walk adds are among them.
    debug_assert!(
        parent_gitignores(root)
            .iter()
            .all(|path| outside.iter().any(|(looked, _)| looked == path))
    );
    Some(Before {
        began,
        outside,
        above,
    })
}

/// The record of a walk that began as `before` found things, read the
/// directories `read` and found `manifests`. `None` when a directory above
/// the root no longer has the stamp it had: something came or went there
/// while the walk ran, which the walk may have read and the next load may
/// not find.
pub(crate) fn walked(before: Before, read: Vec<PathBuf>, manifests: Manifests) -> Option<Record> {
    let unchanged = before.above.iter().all(|(directory, stamp)| {
        qk_state::look(directory)
            .ok()
            .flatten()
            .is_some_and(|(now, _)| now == *stamp)
    });
    unchanged.then_some(Record {
        manifests,
        began: before.began,
        outside: before.outside,
        within: Within::Read(read),
    })
}

/// Whether `look` shows nothing there, or a stamp that had settled by
/// `began`.
fn settled(look: &Option<(Stamp, Duration)>, began: SystemTime) -> bool {
    look.as_ref()
        .is_none_or(|(stamp, window)| stamp.look_settled(*window, began))
}

/// What is at `path`, following links, for [`Seen::Presence`]: 0 when
/// nothing is, 1 for a directory, 2 for anything else.
fn presence(path: &Path) -> u8 {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => 1,
        Ok(_) => 2,
        Err(_) => 0,
    }
}

/// `each` of `items` on a few threads, until one of them answers `false`;
/// then the rest are passed over. Whether every one answered `true`.
fn all_on_threads<T: Sync>(items: &[T], each: impl Fn(&T) -> bool + Sync) -> bool {
    let failed = AtomicBool::new(false);
    let threads = (items.len() / PATHS_PER_THREAD).clamp(1, LOOKERS);
    let chunk = items.len().div_ceil(threads).max(1);
    let run = |items: &[T]| {
        for item in items {
            if failed.load(Ordering::Relaxed) {
                return;
            }
            if !each(item) {
                failed.store(true, Ordering::Relaxed);
                return;
            }
        }
    };
    std::thread::scope(|scope| {
        let mut chunks = items.chunks(chunk);
        let first = chunks.next().unwrap_or_default();
        for items in chunks {
            let run = &run;
            scope.spawn(move || run(items));
        }
        run(first);
    });
    !failed.load(Ordering::Relaxed)
}

impl Record {
    pub(crate) fn manifests(&self) -> &Manifests {
        &self.manifests
    }

    /// Whether a walk now would find what the record holds. What lies
    /// outside the directories read must be as it was before the walk; what
    /// lies within them must have its stamps unchanged, or, when they were
    /// not taken yet, take stamps that had all settled when the walk began.
    /// A path that cannot be looked at is taken as changed: the walk then
    /// meets what is wrong with it.
    pub(crate) fn holds(&self) -> Verdict {
        let same = |(path, before): &(PathBuf, Seen)| {
            let now = match before {
                Seen::Stamp(_) => match qk_state::look(path) {
                    Ok(look) => Seen::Stamp(look.map(|(stamp, _)| stamp)),
                    Err(_) => return false,
                },
                Seen::Presence(_) => Seen::Presence(presence(path)),
            };
            now == *before
        };
        if !self.outside.iter().all(same) {
            return Verdict::Changed;
        }
        match &self.within {
            Within::Stamped(stamps) => {
                if all_on_threads(stamps, same) {
                    Verdict::Holds
                } else {
                    Verdict::Changed
                }
            }
            Within::Read(read) => match stamps(read, self.began) {
                Some(stamps) => Verdict::Stamped(stamps),
                None => Verdict::Changed,
            },
        }
    }

    /// This record with `stamps`, which [`Record::holds`] took, in place of
    /// the directories read.
    pub(crate) fn stamped(self, stamps: Looked) -> Self {
        Self {
            within: Within::Stamped(stamps),
            ..self
        }
    }
}

/// The stamp of each directory in `read` and of each file the walker reads
/// in it, or `None` when one of them had not settled by `began`, the time
/// the walk that read them began: then something may have changed since.
/// See the module's documentation for why each is there.
fn stamps(read: &[PathBuf], began: SystemTime) -> Option<Looked> {
    let found = std::sync::Mutex::new(Vec::with_capacity(read.len()));
    let directory = |directory: &PathBuf| -> Option<Looked> {
        let mut seen = Vec::new();
        // A path that must be there, or may be.
        let mut stamp = |path: PathBuf, required: bool| -> Option<()> {
            let look = qk_state::look(&path).ok()?;
            if (required && look.is_none()) || !settled(&look, began) {
                return None;
            }
            seen.push((path, Seen::Stamp(look.map(|(stamp, _)| stamp))));
            Some(())
        };
        stamp(directory.clone(), true)?;
        for name in [".gitignore", ".nxignore"] {
            let path = directory.join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                stamp(path, true)?;
            }
        }
        match std::fs::symlink_metadata(directory.join(".git")) {
            // Nothing lies beneath a file.
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                stamp(directory.join(".git/info"), true)?;
                stamp(directory.join(".git/info/exclude"), false)?;
            }
            Err(_) => {}
        }
        Some(seen)
    };
    let all = all_on_threads(read, |path| match directory(path) {
        Some(seen) => {
            found.lock().unwrap().extend(seen);
            true
        }
        None => false,
    });
    if !all {
        return None;
    }
    let mut found = found.into_inner().unwrap();
    found.sort_by(|(a, _), (b, _)| a.cmp(b));
    Some(found)
}

/// How [`encode`] marks each kind of [`Seen`], and what follows the
/// manifests.
const ABSENT: u8 = 0;
const STAMPED: u8 = 1;
const PRESENCE: u8 = 2;
const READ: u8 = 3;
const WITHIN_STAMPED: u8 = 4;

/// The flags each recorded manifest directory carries.
const PROJECT_JSON: u8 = 1;
const PACKAGE_JSON: u8 = 2;

/// Writes `paths`, each with what it was.
fn write_looked(writer: &mut Writer, looked: &Looked) -> Option<()> {
    writer.number(looked.len() as u64);
    let mut previous = "";
    for (path, seen) in looked {
        let path = path.to_str()?;
        writer.string(previous, path);
        previous = path;
        match seen {
            Seen::Stamp(None) => writer.bytes.push(ABSENT),
            Seen::Stamp(Some(stamp)) => {
                writer.bytes.push(STAMPED);
                writer.number(stamp.0.len() as u64);
                for value in &stamp.0 {
                    writer.bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            Seen::Presence(presence) => writer.bytes.extend([PRESENCE, *presence]),
        }
    }
    Some(())
}

/// Reads what [`write_looked`] wrote.
fn read_looked(reader: &mut Reader) -> Option<Looked> {
    let count = reader.count()?;
    let mut looked = Vec::with_capacity(count);
    let mut previous = String::new();
    for _ in 0..count {
        let path = reader.string(&previous)?;
        let seen = match reader.take(1)?[0] {
            ABSENT => Seen::Stamp(None),
            STAMPED => {
                let length = reader.count()?;
                let values = reader.take(length.checked_mul(8)?)?;
                Seen::Stamp(Some(Stamp(
                    values
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|value| i64::from_le_bytes(*value))
                        .collect(),
                )))
            }
            PRESENCE => Seen::Presence(reader.take(1)?[0]),
            _ => return None,
        };
        looked.push((PathBuf::from(&path), seen));
        previous = path;
    }
    Some(looked)
}

/// The saved form of `record`: the format and identity, when its walk
/// began, each manifest directory with what it holds, what lay outside the
/// directories read, and those directories or their stamps; sealed with a
/// digest. `None` when a path is not UTF-8.
fn encode(record: &Record, identity: &blake3::Hash) -> Option<Vec<u8>> {
    let mut writer = Writer::default();
    writer.bytes.extend_from_slice(MAGIC);
    writer.bytes.extend_from_slice(&FORMAT.to_le_bytes());
    writer.bytes.extend_from_slice(identity.as_bytes());
    let began = record.began.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    writer.number(began.as_secs());
    writer.number(u64::from(began.subsec_nanos()));
    writer.number(record.manifests.len() as u64);
    let mut previous = "";
    for (directory, (project_json, package_json)) in &record.manifests {
        let directory = directory.to_str()?;
        writer.string(previous, directory);
        previous = directory;
        let mut flags = 0;
        if *project_json {
            flags |= PROJECT_JSON;
        }
        if *package_json {
            flags |= PACKAGE_JSON;
        }
        writer.bytes.push(flags);
    }
    write_looked(&mut writer, &record.outside)?;
    match &record.within {
        Within::Read(read) => {
            writer.bytes.push(READ);
            writer.number(read.len() as u64);
            let mut previous = "";
            for directory in read {
                let directory = directory.to_str()?;
                writer.string(previous, directory);
                previous = directory;
            }
        }
        Within::Stamped(stamps) => {
            writer.bytes.push(WITHIN_STAMPED);
            write_looked(&mut writer, stamps)?;
        }
    }
    Some(qk_state::saved::seal(writer.bytes))
}

/// The record [`encode`] saved in `bytes`, when they are whole and were
/// saved for `identity`.
fn decode(bytes: &[u8], identity: &blake3::Hash) -> Option<Record> {
    let body = qk_state::saved::unseal(bytes)?;
    let mut reader = Reader { bytes: body };
    if reader.take(MAGIC.len())? != MAGIC
        || reader.take(4)? != FORMAT.to_le_bytes()
        || reader.take(32)? != identity.as_bytes()
    {
        return None;
    }
    let seconds = reader.number()?;
    let nanos = u32::try_from(reader.number()?).ok()?;
    let began = SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))?;
    let count = reader.count()?;
    let mut manifests = Vec::with_capacity(count);
    let mut previous = String::new();
    for _ in 0..count {
        let directory = reader.string(&previous)?;
        let flags = reader.take(1)?[0];
        manifests.push((
            PathBuf::from(&directory),
            (flags & PROJECT_JSON != 0, flags & PACKAGE_JSON != 0),
        ));
        previous = directory;
    }
    let outside = read_looked(&mut reader)?;
    let within = match reader.take(1)?[0] {
        READ => {
            let count = reader.count()?;
            let mut read = Vec::with_capacity(count);
            let mut previous = String::new();
            for _ in 0..count {
                let directory = reader.string(&previous)?;
                read.push(PathBuf::from(&directory));
                previous = directory;
            }
            Within::Read(read)
        }
        WITHIN_STAMPED => Within::Stamped(read_looked(&mut reader)?),
        _ => return None,
    };
    if !reader.bytes.is_empty() {
        return None;
    }
    Some(Record {
        manifests: manifests.into_iter().collect(),
        began,
        outside,
        within,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_back_only_for_its_identity() {
        let root = Path::new("/workspace");
        let identity = identity(root);
        let outside = vec![
            (PathBuf::from("/.git"), Seen::Presence(0)),
            (PathBuf::from("/.gitignore"), Seen::Stamp(None)),
            (
                PathBuf::from("/.nxignore"),
                Seen::Stamp(Some(Stamp(vec![1, 2, -3, i64::MAX]))),
            ),
            (PathBuf::from("/workspace/.git"), Seen::Presence(1)),
        ];
        let manifests: Manifests = [
            (PathBuf::from("/workspace"), (false, true)),
            (PathBuf::from("/workspace/packages/a"), (true, false)),
            (PathBuf::from("/workspace/packages/b"), (true, true)),
        ]
        .into_iter()
        .collect();
        let began = SystemTime::UNIX_EPOCH + Duration::new(1_000_000, 123);
        let read = vec![
            PathBuf::from("/workspace"),
            PathBuf::from("/workspace/packages"),
        ];
        let before = |outside: &Looked| Before {
            began,
            outside: outside.clone(),
            above: Vec::new(),
        };
        let record = walked(before(&outside), read.clone(), manifests.clone()).unwrap();
        let stamps = vec![(
            PathBuf::from("/workspace"),
            Seen::Stamp(Some(Stamp(vec![4, 5]))),
        )];
        {
            let record = record.stamped(stamps.clone());
            let bytes = encode(&record, &identity).unwrap();
            let read = decode(&bytes, &identity).unwrap();
            assert_eq!(read.manifests, manifests);
            assert_eq!(read.began, began);
            assert_eq!(read.outside, outside);
            assert!(matches!(&read.within, Within::Stamped(read) if *read == stamps));
            assert!(decode(&bytes, &super::identity(Path::new("/other"))).is_none());
            let mut damaged = bytes.clone();
            damaged[20] ^= 1;
            assert!(decode(&damaged, &identity).is_none());
        }
        let record = walked(before(&outside), read.clone(), manifests).unwrap();
        let bytes = encode(&record, &identity).unwrap();
        let decoded = decode(&bytes, &identity).unwrap();
        assert!(matches!(&decoded.within, Within::Read(directories) if *directories == read));
    }

    /// A workspace nested in a repository, and a directory for its records
    /// beside them: `outer` holds the repository's `.git` and a
    /// `.gitignore`, `outer/workspace` the workspace.
    struct Fixture {
        _temp: tempfile::TempDir,
        outer: PathBuf,
        root: PathBuf,
        records: PathBuf,
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A loaded workspace as a whole, or the error loading it.
    fn shown(loaded: Result<crate::Workspace>) -> String {
        match loaded {
            Ok(workspace) => format!("{workspace:?}"),
            Err(error) => format!("error: {error:#}"),
        }
    }

    /// How far a record has come: saved by the walk, or stamped by the load
    /// after it.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Stage {
        Walked,
        Stamped,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = crate::simplified(temp.path().canonicalize().unwrap());
            let outer = base.join("outer");
            let root = outer.join("workspace");
            std::fs::create_dir_all(outer.join(".git")).unwrap();
            write(&outer.join(".gitignore"), "unrelated/\n");
            write(&root.join("nx.json"), "{}");
            write(&root.join("package.json"), r#"{"name":"root"}"#);
            write(&root.join("pnpm-workspace.yaml"), "packages:\n  - libs/*\n");
            write(&root.join(".gitignore"), "dist/\n");
            write(&root.join(".nxignore"), "tmp/\n");
            write(&root.join("libs/.gitignore"), "unrelated/\n");
            write(&root.join("libs/a/package.json"), r#"{"name":"a"}"#);
            write(&root.join("libs/b/project.json"), r#"{"name":"b"}"#);
            write(&root.join("apps/c/project.json"), r#"{"name":"c"}"#);
            std::fs::create_dir_all(root.join("apps/d")).unwrap();
            write(&root.join(".tools/e/project.json"), r#"{"name":"e"}"#);
            write(&root.join("node_modules/f/project.json"), r#"{"name":"f"}"#);
            Self {
                _temp: temp,
                outer,
                root,
                records: base.join("records"),
            }
        }

        /// Loads remembering, and waits for any record to be saved.
        fn load(&self) -> Result<crate::Workspace> {
            let loaded = crate::Workspace::load_remembering(&self.root, &self.records);
            crate::finish_remembering();
            loaded
        }

        /// Loads walking, as without a record.
        fn walk(&self) -> Result<crate::Workspace> {
            crate::Workspace::load(&self.root)
        }

        /// The stage of the record kept, if any, and whether it still holds.
        fn record(&self) -> Option<(Stage, bool)> {
            let record = recall(&self.records, &self.root)?;
            let stage = match record.within {
                Within::Read(_) => Stage::Walked,
                Within::Stamped(_) => Stage::Stamped,
            };
            Some((stage, !matches!(record.holds(), Verdict::Changed)))
        }

        /// Waits until everything written so far has settled.
        fn settle(&self) {
            let window = [&self.root, &self.outer]
                .iter()
                .map(|path| {
                    qk_state::settle::window(path, &std::fs::symlink_metadata(path).unwrap())
                })
                .max()
                .unwrap();
            std::thread::sleep(window + Duration::from_millis(20));
        }

        /// Waits until everything written so far has settled, then loads
        /// until the record reaches `stage`, and checks that it holds.
        fn remember(&self, stage: Stage) {
            self.settle();
            self.load().unwrap();
            assert_eq!(self.record(), Some((Stage::Walked, true)));
            if stage == Stage::Stamped {
                self.load().unwrap();
                assert_eq!(self.record(), Some((Stage::Stamped, true)));
            }
        }
    }

    /// A walk is not remembered when something came or went, while it ran,
    /// in a directory above the root whose entries decide what it finds:
    /// an ignore file there for the walk alone leaves nothing else to show
    /// it. Only the directories up to the repository's top are looked at.
    #[test]
    fn a_walk_is_not_remembered_when_a_directory_above_it_changed_meanwhile() {
        let fixture = Fixture::new();
        fixture.settle();
        let before = before_walk(&fixture.root).unwrap();
        let above: Vec<&Path> = before
            .above
            .iter()
            .map(|(path, _)| path.as_path())
            .collect();
        assert_eq!(above, [fixture.outer.as_path()]);
        assert!(walked(before, Vec::new(), Manifests::new()).is_some());

        let before = before_walk(&fixture.root).unwrap();
        let came = fixture.outer.join(".nxignore");
        write(&came, "apps/\n");
        std::fs::remove_file(&came).unwrap();
        assert!(walked(before, Vec::new(), Manifests::new()).is_none());
        // Changed too recently to tell what a walk beginning now reads.
        assert!(before_walk(&fixture.root).is_none());
        fixture.settle();
        assert!(before_walk(&fixture.root).is_some());

        // A workspace that is the repository's top depends on nothing
        // above it.
        std::fs::create_dir(fixture.root.join(".git")).unwrap();
        fixture.settle();
        let before = before_walk(&fixture.root).unwrap();
        assert!(before.above.is_empty());
        write(&came, "apps/\n");
        assert!(walked(before, Vec::new(), Manifests::new()).is_some());
    }

    type Change = fn(&Fixture);

    /// Each change alters what a walk finds; after it, a record saved
    /// before, stamped or not, no longer holds, and a load gives what a walk
    /// does.
    #[test]
    fn a_change_to_anything_the_walk_depends_on_makes_the_next_load_walk() {
        fn nothing(_: &Fixture) {}
        // Unix adds the changes that need links.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut changes: Vec<(&str, Change, Change)> = vec![
            ("a project.json added", nothing, |f| {
                write(&f.root.join("apps/d/project.json"), r#"{"name":"d"}"#)
            }),
            ("a package.json removed", nothing, |f| {
                std::fs::remove_file(f.root.join("libs/a/package.json")).unwrap()
            }),
            ("a project.json renamed", nothing, |f| {
                std::fs::rename(
                    f.root.join("libs/b/project.json"),
                    f.root.join("libs/b/project.json.old"),
                )
                .unwrap()
            }),
            ("a directory added", nothing, |f| {
                write(&f.root.join("apps/g/project.json"), r#"{"name":"g"}"#)
            }),
            ("a hidden directory added", nothing, |f| {
                write(&f.root.join(".tools/h/project.json"), r#"{"name":"h"}"#)
            }),
            ("a directory removed", nothing, |f| {
                std::fs::remove_dir_all(f.root.join("apps/c")).unwrap()
            }),
            ("a .gitignore edited", nothing, |f| {
                write(&f.root.join(".gitignore"), "dist/\nc/\n")
            }),
            ("an .nxignore edited", nothing, |f| {
                write(&f.root.join(".nxignore"), "tmp/\nb/\n")
            }),
            ("a nested .gitignore edited", nothing, |f| {
                write(&f.root.join("libs/.gitignore"), "b/\n")
            }),
            ("an ignore file added", nothing, |f| {
                write(&f.root.join("apps/.nxignore"), "c/\n")
            }),
            ("a repository with an exclude file added", nothing, |f| {
                write(&f.root.join("apps/c/.git/info/exclude"), "project.json\n")
            }),
            (
                "an exclude file added beneath a .git",
                |f| std::fs::create_dir_all(f.root.join("libs/b/.git/info")).unwrap(),
                |f| write(&f.root.join("libs/b/.git/info/exclude"), "project.json\n"),
            ),
            (
                "an exclude file removed",
                |f| write(&f.root.join("libs/b/.git/info/exclude"), "project.json\n"),
                |f| std::fs::remove_file(f.root.join("libs/b/.git/info/exclude")).unwrap(),
            ),
            (
                "the root's exclude file edited",
                |f| write(&f.root.join(".git/info/exclude"), "\n"),
                |f| write(&f.root.join(".git/info/exclude"), "c/\n"),
            ),
            (
                "the repository's .gitignore above the root edited",
                nothing,
                |f| write(&f.outer.join(".gitignore"), "c/\n"),
            ),
            (
                "the repository's .gitignore above the root removed",
                |f| write(&f.outer.join(".gitignore"), "c/\n"),
                |f| std::fs::remove_file(f.outer.join(".gitignore")).unwrap(),
            ),
            (
                "a .git added at the root, leaving the one above",
                |f| write(&f.outer.join(".gitignore"), "c/\n"),
                |f| std::fs::create_dir(f.root.join(".git")).unwrap(),
            ),
            (
                "the repository's .git removed",
                |f| write(&f.outer.join(".gitignore"), "c/\n"),
                |f| std::fs::remove_dir_all(f.outer.join(".git")).unwrap(),
            ),
            (
                "an ignore file above the root that does not parse",
                nothing,
                |f| write(&f.outer.join(".nxignore"), "a{b\n"),
            ),
        ];
        #[cfg(unix)]
        changes.extend([
            (
                "a directory replaced by a link",
                nothing as Change,
                (|f| {
                    std::fs::rename(f.root.join("apps/c"), f.outer.join("c")).unwrap();
                    std::os::unix::fs::symlink(f.outer.join("c"), f.root.join("apps/c")).unwrap();
                }) as Change,
            ),
            (
                "a linked ignore file's target edited",
                |f| {
                    write(&f.outer.join("rules"), "dist/\n");
                    std::fs::remove_file(f.root.join(".gitignore")).unwrap();
                    std::os::unix::fs::symlink(f.outer.join("rules"), f.root.join(".gitignore"))
                        .unwrap();
                },
                |f| write(&f.outer.join("rules"), "dist/\nc/\n"),
            ),
        ]);
        for stage in [Stage::Walked, Stage::Stamped] {
            for (name, setup, change) in &changes {
                let fixture = Fixture::new();
                setup(&fixture);
                fixture.remember(stage);
                let before = shown(fixture.walk());
                change(&fixture);
                assert_eq!(
                    fixture.record(),
                    Some((stage, false)),
                    "{name}: the record still holds"
                );
                let walked = shown(fixture.walk());
                assert_ne!(walked, before, "{name}: a walk finds the same");
                assert_eq!(shown(fixture.load()), walked, "{name}, {stage:?}");
            }
        }
    }

    /// What the walk does not read can change without the record failing to
    /// hold, and what is decided again on every load follows the change.
    #[test]
    fn a_record_holds_while_nothing_the_walk_depends_on_changes() {
        let changes: [Change; 4] = [
            // A project's own configuration is read on every load.
            |f| write(&f.root.join("libs/b/project.json"), r#"{"name":"b2"}"#),
            // So are the workspaces' patterns.
            |f| {
                write(
                    &f.root.join("pnpm-workspace.yaml"),
                    "packages:\n  - libs/*\n  - '!libs/a'\n",
                )
            },
            // The walk does not go into node_modules, nor into .git, whose
            // presence alone above the workspace matters.
            |f| write(&f.root.join("node_modules/f/project.json"), "{}"),
            |f| write(&f.outer.join(".git/index"), "index"),
        ];
        for stage in [Stage::Walked, Stage::Stamped] {
            let fixture = Fixture::new();
            fixture.remember(stage);
            assert_eq!(shown(fixture.load()), shown(fixture.walk()));
            for change in changes {
                change(&fixture);
                assert_eq!(fixture.record(), Some((Stage::Stamped, true)));
                assert_eq!(shown(fixture.load()), shown(fixture.walk()));
            }
            let loaded = fixture.load().unwrap();
            assert!(loaded.projects.contains_key("b2"));
            assert!(!loaded.projects.contains_key("a"));
        }
    }

    /// A stamp taken within its filesystem's window can hide a later change,
    /// so a walk is only taken again once what it read had settled when it
    /// began, and is not remembered at all unless what lies outside what it
    /// read had.
    #[cfg(unix)]
    #[test]
    fn a_walk_is_only_taken_again_once_what_it_read_had_settled() {
        let later = SystemTime::now() + Duration::from_secs(3600);
        let fixture = Fixture::new();
        fixture.settle();
        std::fs::File::open(fixture.root.join("libs"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(shown(fixture.load()), shown(fixture.walk()));
        assert_eq!(fixture.record(), Some((Stage::Walked, false)));
        assert_eq!(shown(fixture.load()), shown(fixture.walk()));
        assert_eq!(fixture.record(), Some((Stage::Walked, false)));

        let fixture = Fixture::new();
        fixture.settle();
        std::fs::File::options()
            .write(true)
            .open(fixture.outer.join(".gitignore"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(shown(fixture.load()), shown(fixture.walk()));
        assert_eq!(fixture.record(), None);
    }

    #[test]
    fn each_workspace_has_a_record_of_its_own() {
        let fixture = Fixture::new();
        let other = fixture.outer.join("other");
        write(&other.join("nx.json"), "{}");
        write(&other.join("app/project.json"), r#"{"name":"other"}"#);
        fixture.remember(Stage::Walked);
        assert!(recall(&fixture.records, &other).is_none());
        let loaded = crate::Workspace::load_remembering(&other, &fixture.records).unwrap();
        crate::finish_remembering();
        assert_eq!(loaded.projects.keys().collect::<Vec<_>>(), vec!["other"]);
        assert!(recall(&fixture.records, &other).is_some());
        assert_eq!(fixture.record(), Some((Stage::Walked, true)));
    }

    #[test]
    fn a_damaged_record_is_walked_past_and_saved_again() {
        let fixture = Fixture::new();
        fixture.remember(Stage::Stamped);
        let file = file(&fixture.records, &fixture.root);
        let mut bytes = std::fs::read(&file).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(fixture.record(), None);
        assert_eq!(shown(fixture.load()), shown(fixture.walk()));
        assert_eq!(fixture.record(), Some((Stage::Walked, true)));
    }
}
