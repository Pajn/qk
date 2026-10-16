//! What the remote cache needs from the storage behind it. [`super::Remote`]
//! decides what to fetch, upload and list; a [`Store`] moves the bytes.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;

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

pub(super) trait Store: Send + Sync {
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

    /// Records ahead of an upload that the store is about to hold `key`, so
    /// readers find it even if this run never lists it, returning what to
    /// [`list`](Store::list) as replaced once it does.
    fn announce(&self, key: &str) -> Result<Option<String>>;

    /// Lists `keys` as held by the store now, given what `known` read of its
    /// index, and retires the `announcements` the listing replaces.
    fn list(
        &self,
        keys: BTreeSet<String>,
        known: Option<&index::Known>,
        announcements: BTreeSet<String>,
    ) -> Result<()>;
}
