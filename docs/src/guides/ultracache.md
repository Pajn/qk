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
- A path it **looked up** is keyed by whether it exists, and as what. A file
  created where a resolver once found nothing changes the key; so does
  deleting one it found.

Left out are paths outside the workspace, `node_modules`, which the lockfile
keys, `.git`, qk's own state, the task's outputs and warm paths, files it wrote
before reading them, and what its `ignoreReads` names. A directory's listing
leaves out the task's outputs and what it wrote there, which are there after
it runs and need not be before.

## Keying

A task with `qk:ultracache` has a weak key: its key without its declared file
inputs. It keeps the command, arguments, environment and runtime inputs,
`externalDependencies`, the lockfile and the files qk keys for every task, such
as `nx.json` and its project's manifests, and its inputs marked
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
recording yet. `--report` writes each task's as `recorded`.

## What a recording cannot see

- Reads by processes outside the task, such as a build daemon started before
  it or shared between tasks: keep such a target `off`.
- Environment variables, which are no file reads: declare them as inputs.
- A compiled program in the workspace that the task runs directly. A script it
  runs is read by its interpreter, and so recorded.
- Files opened before the task started, and reads through `io_uring`.
- A recording a task's trace was too large for, or that strace could not
  decode wholly, is not kept, and the summary says so.

Recordings are kept in the local cache for now: a machine uses the recordings
it made itself.

## Running the tests

`tools/ultracache/fixture` is a workspace whose build reads, lists and looks
for files, and `crates/qk-cli/tests/ultracache.rs` follows it through edits. The
tests need Linux and `strace`; `tools/ultracache/podman.sh` runs them, or any
other `cargo test` arguments, in a Linux container from another system.
