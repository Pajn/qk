# Keying tasks by what they read

```json
{
  "targetDefaults": {
    "build": { "cache": true, "qk:ultracache": {} }
  }
}
```

```sh
qk run-many -t build --record     # on Linux, in CI
qk run-many -t build              # anywhere, later
```

A target's declared inputs are usually wider than what its task reads, so an
edit the task never reads still misses the cache, or narrower, so a hit can be
stale. With `qk:ultracache`, a task is keyed by what it actually read the last
time it was recorded, as Nx keys tasks from Nx Cloud's Ultracache recordings.
qk records tasks itself, and keeps the recordings in its cache.

## Recording

A run with `--record`, or with `QK_RECORD=true`, records each task with
`qk:ultracache` that it executes. Recording uses `strace`, so it needs Linux
with `strace` on `PATH`; it filters the traced calls in the kernel with
seccomp where strace can, which strace 5.3 and later do. On other systems
`--record` records nothing and says so. A task taken from the cache is not
recorded, since nothing runs.

A recording keeps a path set: what the task did to each path in the
workspace.

- A file it **read** is keyed by its content.
- A directory it **listed** is keyed by its entries' names and kinds, so a new
  file there, such as another `.svg` in a directory a build bundles whole,
  changes the key even though no recording read it.
- A path it **looked up** is keyed by whether it exists, and as what, and a
  file it found by its content: a tool that compares timestamps, as
  `tsc --build` does, decides by files it never reads. A file created where a
  resolver once found nothing changes the key; so does deleting or changing
  one it found.

Left out are paths outside the workspace, `node_modules`, which the lockfile
keys, `.git`, qk's own state, the task's outputs and warm paths, files it wrote
before reading them, and what its `ignoreReads` names. A directory's listing
leaves out the task's outputs and what it wrote there, which are there after
it runs and need not be before, and the declared outputs of every other
target in the workspace, which other tasks write whether or not this one
reads them, with the directories between an output and its project's root,
which hold it: `.cache` for an output in `.cache/build`. The outputs of the
targets it depends on are its inputs, and stay in its listings. A directory
listings leave out has nothing in them to key.

## Keying

A task with `qk:ultracache` has a weak key: its key without its declared file
inputs. It keeps the command, arguments, environment and runtime inputs,
`externalDependencies`, the lockfile and what qk keys for every task, such
as the nx.json settings and its project's configuration and `package.json`,
and its inputs marked
`"always": true`, which count whether it read them or not. Up to eight path
sets are kept for a weak key, the newest first. A task that reads differently
from run to run keeps a set for each way.

For each path set, qk keys the paths as they are now, and restores a result
stored under that key. When none has one, the task runs: a task with path sets
is keyed by them alone, never by its declared inputs, which are what a
recording found wanting. Recorded, its new path set and result are kept; not
recorded, its result is kept under its declared key only.

A task without path sets is keyed by its declared inputs, as without
`qk:ultracache`, until a run records it. Its results are also kept under its
declared key when recorded, so runs without its recordings find them.

The run summary names the tasks taken from the cache by what they read, those
recorded, those that missed what they read when recorded, and those with no
recording yet. `--report` writes which of these each task's recordings did as
`recorded`, and the [run history](history.md) keeps it, so `qk show run --json`
prints it too.

## What a recording cannot see

- Reads by processes outside the task, such as a build daemon started before
  it or shared between tasks: keep such a target `off`.
- Environment variables, which are no file reads: declare them as inputs.
- A compiled program in the workspace that the task runs directly. A script it
  runs is read by its interpreter, and so recorded.
- Files opened before the task started, and reads through `io_uring`.
- A recording a task's trace was too large for, or that strace could not
  decode wholly, is not kept, and the summary says so. Neither is one that
  names no path in the workspace, which would match any workspace, nor one
  whose task changed a file it read or found while it ran: keyed by the file
  as the task left it, it would be keyed by the task's own write. Nor is one
  where an entry in a directory it listed was added, renamed or removed while
  it ran, other than what its listings leave out, and the summary names the
  entry that changed where it can be told; what another task
  writes inside one of its entries, or a file it rewrites in place, leaves
  the listing as it was. Where the task wrote into the directory, any file
  there that changed counts, rewritten or not, and a subdirectory counts if
  it was created while the task ran, which a file system that keeps no
  creation time cannot tell. An entry another process removed there, or a
  subdirectory it moved in, cannot be told from the task's own writes, and
  goes unnoticed.

## Sharing recordings

With a [remote cache](remote-cache.md), recordings are shared. A recorded run
uploads a task's path sets for its weak key, as `pathsets/<weak>`, and its
result under the key of what it read, beside its result under its declared
key. A task whose local path sets all miss fetches the path sets the store
holds for its weak key, adds the new ones to its own, and fetches a result for
any of their keys the store's index lists. A machine that never records, such
as one on macOS, is keyed by the recordings CI made.

A local hit makes no request. A local miss makes one for the path sets, and
fetches only results the store holds. The newest upload of a weak key's path
sets replaces the last, after merging what the store held when the task was
looked up; two machines recording one weak key at once can lose one's path
set, which costs a miss, never a wrong result.

## Trying it out

```json
"qk:ultracache": { "mode": "record" }
```

With `"record"`, a task is keyed by its declared inputs, as without the field,
while a run with `--record` records it and keeps and shares its recordings as
with `"on"`. Every run also looks the task up by its recordings without
restoring anything, and says what they would have done: `--report` writes it
as `shadow`, `wouldHit` with the key that had a result, `wouldMiss`, or
`unrecorded` without path sets, and the run history keeps it, so runs can be
counted afterwards: a task its declared inputs missed with `wouldHit` is a run
its recordings would have saved. The run summary names the tasks where the two
disagree:

- a task its declared inputs missed where its recordings had a result: a
  change it never read, which `"on"` would have taken from the cache;
- a task its declared inputs took from the cache where its recordings had
  none: a change to what it read that its declared inputs leave out, a hit
  that may be stale, or a recorded result never shared.

Looking a task up by its recordings costs the request for its path sets
whenever it has none of its own, even when its declared inputs hit the local
cache: that is where a stale hit would show. Switching to `"on"` keys it by them.

## Running the tests

`tools/ultracache/fixture` is a workspace whose build reads, lists and looks
for files, and `crates/qk-cli/tests/ultracache.rs` follows it through edits. The
tests need Linux and `strace`; `tools/ultracache/podman.sh` runs them, or any
other `cargo test` arguments, in a Linux container from another system.
