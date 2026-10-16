//! What the remote cache needs from the storage behind it. [`super::Remote`]
//! decides what to fetch, upload and list; a [`Store`] moves the bytes.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Result;

use super::index;

/// A pack's body as the store sends it.
pub(super) type Body = Box<dyn Read + Send>;

/// An object the remote cache reads or writes whole.
#[derive(Clone, Copy)]
pub(super) enum Object<'a> {
    /// The result entry for a cache key.
    Entry(&'a str),
    /// A task's warm record, saved for a branch.
    Warm { identity: &'a str, branch: &'a str },
    /// A blob stored apart from the packs that cite it, named by its hash.
    Blob(&'a str),
}

/// How a store wants a record and the blobs it carries sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Framing {
    /// One pack, read back as it was written.
    Pack,
    /// An upload body a store server composes packs from.
    Upload,
}

/// The two names a warm record is stored under: its task's identity on this
/// platform, and its branch, each as 32 hex digits.
pub(super) fn warm_names(identity: &str, branch: &str) -> (String, String) {
    let hash = |text: &str| blake3::hash(text.as_bytes()).to_hex()[..32].to_owned();
    let identity = format!(
        "{identity}\0{}\0{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    (hash(&identity), hash(branch))
}

/// A commit the store refused because it lacks these blobs stored apart.
#[derive(Debug)]
pub(super) struct MissingBlobs(pub Vec<String>);

impl std::fmt::Display for MissingBlobs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the store lacks {} blobs", self.0.len())
    }
}

impl std::error::Error for MissingBlobs {}

pub(super) trait Store: Send + Sync {
    /// How [`put`](Store::put) wants records sent.
    fn framing(&self) -> Framing {
        Framing::Pack
    }

    /// Starts whatever the store needs from the local cache at `root`.
    fn begin(&self, _root: &Path) {}

    /// The blob `hash`, stored apart from the record `owner` cites it in.
    fn get_blob(&self, hash: &str, _owner: Object, task: &str) -> Result<Option<Body>> {
        self.get(Object::Blob(hash), task)
    }

    /// Names the store, so that each keeps its own local copy of its index.
    fn location(&self) -> String;

    /// Brings `known` up to date with the index the store holds now.
    fn sync_index(&self, known: &mut index::Known) -> Result<()>;

    /// The pack at `object`, or `None` when there is none. `task` names the
    /// task it is for in profile timings.
    fn get(&self, object: Object, task: &str) -> Result<Option<Body>>;

    /// Whether the store holds `object`.
    fn has(&self, object: Object) -> Result<bool>;

    /// Writes the pack in `file` as `object`.
    fn put(&self, object: Object, file: File) -> Result<()>;

    /// Records ahead of an upload that the store is about to hold `key`, whose
    /// entry stores the blobs `apart` on their own, so readers find it even
    /// if this run never lists it, returning what to [`list`](Store::list) as
    /// replaced once it does.
    fn announce(&self, key: &str, apart: &[String]) -> Result<Option<String>>;

    /// Lists `keys` as held by the store now, with the blobs `apart` names
    /// for them and the run that used `members` in that order, given what
    /// `known` read of its index, and retires the `announcements` the listing
    /// replaces.
    fn list(
        &self,
        keys: BTreeSet<String>,
        apart: HashMap<String, Vec<String>>,
        members: Vec<String>,
        known: Option<&index::Known>,
        announcements: BTreeSet<String>,
    ) -> Result<()>;
}
