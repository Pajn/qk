//! Inventories and their source listings saved for the runs that follow.
//!
//! Taking a [`Membership`] inventory reads every directory in its scope and
//! asks Git for its listing, which a cached run otherwise only waits for. A
//! settled inventory is saved in the worktree's state as the run keeps it; the
//! next run with the same [`Identity`] loads it, and takes its listing when
//! [`restore`] finds that it still holds, which costs looking at stamps alone.
//!
//! # When a saved listing is still true
//!
//! The listing depends on two kinds of things:
//!
//! - What is on disk: the directories in scope and their names, the ignore
//!   files, and the files Git reads to decide what is a source (its index,
//!   exclude and config files). The inventory stamps each of them, and
//!   [`Membership::state`] compares those stamps with what is there now,
//!   reading again the names of any directory, and the rules of any ignore
//!   file, whose stamp moved or is too recent to trust.
//! - What the process that took it was asked and ran with, which no stamp
//!   shows: what [`Identity`] holds. An inventory saved for another identity
//!   is never loaded.
//!
//! Git's policy files are found by asking Git, and which files those are
//! depends on more than the stamps of those it found: on conditional
//! includes, on the configuration Git was built to read, and on its
//! environment. So they are found again on every load, while the stamps are
//! looked at, and a saved listing is only taken when Git names the same set.
//!
//! # The settle window between runs
//!
//! A stamp taken within its filesystem's window (see [`crate::settle`]) can
//! hide a later change. The inventory keeps the time it began stamping, and
//! keeps it when saved, so a later run judges each stamp against the time it
//! was taken, as the run that took it would:
//!
//! - Only a settled inventory is saved: one whose listing policies were older
//!   than their window when it began stamping and whose stamped directories
//!   hold every listed file. A policy that changed just before or while it
//!   was saved leaves it unsaved.
//! - A directory, or ignore file, whose stamp was recent when it was taken
//!   has its names, or rules, read again and compared by every run that
//!   loads it, whatever its stamp says then: a change made just before,
//!   while or just after the saving run is seen there, as in that run.
//! - A loaded inventory is only taken when every other policy has also
//!   settled by the time it is looked at again, as within a run.
//!
//! Anything that does not read, or that is in doubt, lists again as a run
//! without a saved inventory does, and saves what it finds.

use super::*;

/// What every saved file starts with, then its format.
const MAGIC: &[u8; 8] = b"qk-list\0";
/// Bumped whenever what is saved, or what it means, changes.
const FORMAT: u32 = 1;
/// Saved inventories kept per worktree, the most recently saved: one for each
/// of the few kinds of run a worktree alternates between.
const KEPT: usize = 4;
/// Paths of the directory holding the saved inventories begin with this,
/// apart from files being written.
const PREFIX: &str = "listing-";

/// Where a worktree's saved inventories are.
pub(super) fn directory(root: &Path) -> PathBuf {
    paths::worktree_state(root).join("listings")
}

/// Everything a saved inventory and its listing depend on that its stamps do
/// not show, as a digest: what the process that took them was asked to
/// list, with what, and how.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Identity(blake3::Hash);

impl Identity {
    /// The identity of an inventory of `root`, of `scope` alone if any,
    /// leaving out `outputs` and `cache` and adding `extra`, as
    /// [`Membership::new`] and [`Membership::list`] take them.
    pub(super) fn new(
        root: &Path,
        scope: Option<&Scope>,
        outputs: &[Outputs],
        cache: Option<&Path>,
        extra: &BTreeSet<String>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        // Each field is preceded by its length, so no two lists of fields
        // hash alike.
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        // The format, and the qk that wrote it, whose listing may differ.
        field(MAGIC);
        field(&FORMAT.to_le_bytes());
        field(env!("CARGO_PKG_VERSION").as_bytes());
        // The workspace: several in one Git worktree share its state.
        field(root.as_os_str().as_encoded_bytes());
        // What is listed: a narrowed inventory lists its scope alone and
        // reads only the names leading to it.
        match scope {
            None => field(b"whole"),
            Some(scope) => {
                field(b"within");
                for path in &scope.paths {
                    field(path.as_bytes());
                }
            }
        }
        // What is left out of the listing and of the names captured: the
        // tasks' outputs, from their anchors and complete roots, and the cache.
        field(b"outputs");
        for outputs in outputs {
            let (globs, negated, explicit) = outputs.describe();
            field(&(globs.len() as u64).to_le_bytes());
            for glob in globs {
                field(glob.as_bytes());
            }
            field(&(negated.len() as u64).to_le_bytes());
            for glob in negated {
                field(glob.as_bytes());
            }
            field(&[u8::from(explicit)]);
        }
        field(b"cache");
        field(cache.map_or(&[][..], |cache| cache.as_os_str().as_encoded_bytes()));
        // Paths added to the listing whether or not they are on disk.
        field(b"extra");
        for path in extra {
            field(path.as_bytes());
        }
        // How it is listed: a root `.nxignore` makes the listing walk the
        // workspace rather than ask Git. Its appearance also changes the
        // root's names, which the inventory compares.
        field(&[u8::from(root.join(".nxignore").is_file())]);
        // Git's environment, which decides what repository, index and
        // configuration it reads; and where it looks for the user's
        // configuration. Some of these the listing's Git processes never
        // see, which only costs a listing when they change.
        let mut environment: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| {
                name.to_str().is_some_and(|name| {
                    name.starts_with("GIT_") || matches!(name, "HOME" | "XDG_CONFIG_HOME")
                })
            })
            .collect();
        environment.sort();
        field(b"environment");
        for (name, value) in &environment {
            field(name.as_encoded_bytes());
            field(value.as_encoded_bytes());
        }
        Self(hasher.finalize())
    }

    /// The file an inventory of this identity is saved in.
    pub(super) fn file(&self, root: &Path) -> PathBuf {
        directory(root).join(format!("{PREFIX}{}", &self.0.to_hex()[..16]))
    }
}

/// The inventory and listing saved for `identity`, if one reads and still
/// holds, with `source_ignore` read now as its policy and `template`'s
/// settings, which are those `identity` was made of. `None` whenever in
/// doubt: then the caller lists again.
pub(super) fn restore(
    root: &Path,
    identity: &Identity,
    template: Membership,
    source_ignore: SourceIgnore,
) -> Option<Membership> {
    // Taken before any stamp is looked at again, so a policy is only taken
    // as settled when it was by then.
    let looked = SystemTime::now();
    let file = identity.file(root);
    if !file.is_file() {
        return None;
    }
    // Asking Git takes longer than reading the file and looking at every
    // stamp, so it starts first. Which files Git names needs no order with
    // the stamps: a policy file changed before or after Git is asked shows
    // in its stamp, and one Git names now but did not then in the set.
    let (policies, membership) = std::thread::scope(|scope| {
        let policies = scope.spawn(|| git_membership_policies(root));
        let membership = (|| {
            let bytes = std::fs::read(&file).ok()?;
            let mut membership = decode(&bytes, identity, template)?;
            // Read before the stamps are compared: the comparison then
            // vouches for the rules read, as listing after stamping does.
            membership.source_ignore = Arc::new(source_ignore);
            // `settled` is that of the run that saved it, which is only
            // saved settled; the state says whether every policy has
            // settled since.
            matches!(membership.state(None, looked), Ok((true, true))).then_some(membership)
        })();
        (
            policies.join().expect("git policy thread panicked"),
            membership,
        )
    });
    let mut membership = membership?;
    if policies != membership.git_policies {
        return None;
    }
    membership.settled = true;
    membership.restored = true;
    Some(membership)
}

/// Saves `membership`, a settled inventory, for later runs under `identity`,
/// replacing any saved under it before, and keeps only the most recent few.
/// Two runs may save at once: each writes a file of its own and moves it
/// into place, so a run loading it finds one or the other whole.
pub(super) fn save(root: &Path, identity: &Identity, membership: &Membership) -> Result<()> {
    // Paths that are not UTF-8 are not saved: such an inventory is taken
    // again by each run, as without saving.
    let Some(bytes) = encode(identity, membership) else {
        return Ok(());
    };
    let path = identity.file(root);
    let directory = path.parent().context("saved listings have a directory")?;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&bytes)?;
    file.persist(&path)?;
    prune(directory);
    Ok(())
}

/// Removes all but the [`KEPT`] most recently saved inventories, and files
/// left by a save that was stopped while writing.
fn prune(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    let mut saved = Vec::new();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if entry.file_name().to_string_lossy().starts_with(PREFIX) {
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
    for (_, path) in saved.iter().rev().skip(KEPT) {
        let _ = std::fs::remove_file(path);
    }
}

/// The flags each saved stamp carries, saying what its path is in the
/// inventory.
const STAMPED: u8 = 1;
const IS_DIRECTORY: u8 = 2;
const IS_POLICY: u8 = 4;
const IS_GIT_POLICY: u8 = 8;
const HAS_ENTRIES: u8 = 16;
const HAS_CONTENT: u8 = 32;

/// The saved form of `membership`: the format and identity, the time it
/// began stamping, its listing, and each stamped path with its stamp, what
/// it is in the inventory, and the names or rules captured for it; then a
/// digest of all of it. Strings are kept as the length of what they share
/// with the one before and the rest, as sorted paths share most of
/// themselves. `None` when a path is not UTF-8.
pub(super) fn encode(identity: &Identity, membership: &Membership) -> Option<Vec<u8>> {
    let mut writer = Writer::default();
    writer.bytes.extend_from_slice(MAGIC);
    writer.bytes.extend_from_slice(&FORMAT.to_le_bytes());
    writer.bytes.extend_from_slice(identity.0.as_bytes());
    let stamped = membership
        .stamped
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    writer.number(stamped.as_secs());
    writer.number(u64::from(stamped.subsec_nanos()));
    writer.number(membership.files.len() as u64);
    let mut previous = "";
    for path in membership.files.iter() {
        writer.string(previous, path);
        previous = path;
    }
    writer.number(membership.stamps.len() as u64);
    let mut previous = String::new();
    for (path, stamp) in &membership.stamps {
        let path_string = path.to_str()?;
        writer.string(&previous, path_string);
        previous = path_string.to_owned();
        let entries = membership.directory_entries.get(path);
        let content = membership.ignore_contents.get(path);
        let flags = [
            (stamp.is_some(), STAMPED),
            (membership.directories.contains(path), IS_DIRECTORY),
            (membership.policies.contains(path), IS_POLICY),
            (membership.git_policies.contains(path), IS_GIT_POLICY),
            (entries.is_some(), HAS_ENTRIES),
            (content.is_some(), HAS_CONTENT),
        ]
        .iter()
        .filter(|(set, _)| *set)
        .fold(0, |flags, (_, flag)| flags | flag);
        writer.bytes.push(flags);
        if let Some(stamp) = stamp {
            writer.number(stamp.0.len() as u64);
            for value in &stamp.0 {
                writer.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        if let Some(entries) = entries {
            writer.number(entries.len() as u64);
            let mut previous = "";
            for (name, kind) in entries {
                let name = name.to_str()?;
                writer.string(previous, name);
                writer.bytes.push(*kind);
                previous = name;
            }
        }
        if let Some(content) = content {
            writer.string("", content);
        }
    }
    // Every directory, policy and Git policy has a stamp, so the flags
    // above hold them all; one without would not be saved whole.
    let flagged =
        |set: &BTreeSet<PathBuf>| set.iter().all(|path| membership.stamps.contains_key(path));
    if !flagged(&membership.directories)
        || !flagged(&membership.policies)
        || !flagged(&membership.git_policies)
        || !membership
            .directory_entries
            .keys()
            .chain(membership.ignore_contents.keys())
            .all(|path| membership.stamps.contains_key(path))
    {
        return None;
    }
    let digest = blake3::hash(&writer.bytes);
    writer.bytes.extend_from_slice(digest.as_bytes());
    Some(writer.bytes)
}

/// The inventory [`encode`] saved in `bytes`, with `template`'s settings,
/// when `bytes` are whole and were saved for `identity`.
pub(super) fn decode(
    bytes: &[u8],
    identity: &Identity,
    template: Membership,
) -> Option<Membership> {
    let (body, digest) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    if blake3::hash(body).as_bytes() != digest {
        return None;
    }
    let mut reader = Reader { bytes: body };
    if reader.take(MAGIC.len())? != MAGIC
        || reader.take(4)? != FORMAT.to_le_bytes()
        || reader.take(32)? != identity.0.as_bytes()
    {
        return None;
    }
    let mut membership = template;
    let seconds = reader.number()?;
    let nanos = u32::try_from(reader.number()?).ok()?;
    membership.stamped = SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))?;
    let count = reader.count()?;
    let mut files = Vec::with_capacity(count);
    let mut previous = String::new();
    for _ in 0..count {
        let path = reader.string(&previous)?;
        previous.clone_from(&path);
        files.push(path);
    }
    membership.files = Arc::new(files.into_iter().collect());
    let count = reader.count()?;
    let mut stamps = Vec::with_capacity(count);
    let mut directories = Vec::new();
    let mut policies = Vec::new();
    let mut git_policies = Vec::new();
    let mut directory_entries = Vec::new();
    let mut ignore_contents = Vec::new();
    let mut previous = String::new();
    for _ in 0..count {
        let path_string = reader.string(&previous)?;
        let path = PathBuf::from(&path_string);
        previous = path_string;
        let flags = reader.take(1)?[0];
        let stamp = if flags & STAMPED != 0 {
            let length = reader.count()?;
            let values = reader.take(length.checked_mul(8)?)?;
            Some(Stamp(
                values
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|value| i64::from_le_bytes(*value))
                    .collect(),
            ))
        } else {
            None
        };
        if flags & IS_DIRECTORY != 0 {
            directories.push(path.clone());
        }
        if flags & IS_POLICY != 0 {
            policies.push(path.clone());
        }
        if flags & IS_GIT_POLICY != 0 {
            git_policies.push(path.clone());
        }
        if flags & HAS_ENTRIES != 0 {
            let count = reader.count()?;
            let mut entries = Vec::with_capacity(count);
            let mut previous = String::new();
            for _ in 0..count {
                let name = reader.string(&previous)?;
                let kind = reader.take(1)?[0];
                entries.push((OsString::from(&name), kind));
                previous = name;
            }
            directory_entries.push((path.clone(), entries.into_iter().collect()));
        }
        if flags & HAS_CONTENT != 0 {
            ignore_contents.push((path.clone(), reader.string("")?));
        }
        stamps.push((path, stamp));
    }
    if !reader.bytes.is_empty() {
        return None;
    }
    membership.stamps = stamps.into_iter().collect();
    membership.directories = directories.into_iter().collect();
    membership.policies = policies.into_iter().collect();
    membership.git_policies = git_policies.into_iter().collect();
    membership.directory_entries = directory_entries.into_iter().collect();
    membership.ignore_contents = ignore_contents.into_iter().collect();
    Some(membership)
}

/// Writes the parts of [`encode`]'s form.
#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// `value` in seven-bit groups, least significant first, each but the
    /// last with its top bit set.
    fn number(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.bytes.push(value as u8);
    }

    /// `string`, as the length of what it shares with `previous` and the rest.
    fn string(&mut self, previous: &str, string: &str) {
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
struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let (taken, rest) = self.bytes.split_at_checked(count)?;
        self.bytes = rest;
        Some(taken)
    }

    fn number(&mut self) -> Option<u64> {
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
    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.number()?)
            .ok()
            .filter(|count| *count <= self.bytes.len())
    }

    fn string(&mut self, previous: &str) -> Option<String> {
        let shared = usize::try_from(self.number()?).ok()?;
        let rest = self.count()?;
        let mut string = previous.as_bytes().get(..shared)?.to_vec();
        string.extend_from_slice(self.take(rest)?);
        String::from_utf8(string).ok()
    }
}
