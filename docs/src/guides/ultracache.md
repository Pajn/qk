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
`qk:ultracache` that it executes. Recording needs Linux, with eBPF or strace
as [Recorders](#recorders) describes. On other systems `--record` records
nothing and says so. A task taken from the cache is not
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
before reading them, its
[scratch directories](sandbox.md#scratch-directories-and-ignored-reads), and
what its `ignoreReads` names, which the sandbox's audit leaves out too. A
directory's listing leaves out the task's outputs and what it wrote there, which are there after
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
stored under that key. When none has one, a task whose declared inputs cover
every one of its path sets is looked up by its declared key, as without
`qk:ultracache`; any other runs. Recorded, its new path set and result are
kept; not recorded, its result is kept under its declared key only, which the
next run finds when its declared inputs cover its path sets.

The declared inputs cover a path set when the declared key changes with
everything the task could notice through it:

- each file it read or found is keyed, by the declared inputs or by what qk
  keys for every task, or a directory it found holds a keyed file;
- a file created where it looked for one and found none would be selected,
  and is neither gitignored nor a declared output, so that, say, a missing
  `.env.local` keeps it on its recordings;
- every entry a directory it listed could gain or lose would be selected,
  gitignored entries aside: a fileset takes all of what is under the
  directory, as `{projectRoot}/**/*` takes a project's directories, no
  exclusion of that fileset reaches into it, and the directory is no declared
  output.

A file qk keys by part of its content, such as a dependency's `package.json`
or a `json` input, counts as keyed, as it does for every task's key: a result
taken by the declared key is as sound as one without `qk:ultracache`.

Otherwise its declared inputs are what a recording found wanting, and the task
is keyed by its path sets alone. Coverage is judged when the task is looked
up, by the declared inputs it is looked up with, so that a declaration changed
since a path set was recorded, such as a narrowed named input, is what counts.

A task without path sets is keyed by its declared inputs, as without
`qk:ultracache`, until a run records it. Its results are also kept under its
declared key when recorded, so runs without its recordings find them.

The run summary names the tasks taken from the cache by what they read, those
taken by their declared key in place of their recordings, those recorded,
those that missed what they read when recorded, and those with no recording
yet. `--report` writes which of these each task's recordings did as
`recorded`, and the [run history](history.md) keeps it, so `qk show run --json`
prints it too.

## Recorders

qk records a task on Linux in one of two ways, and a recording is kept or
refused alike whichever made it.

- **eBPF**, by default. A program in the kernel notes the file calls of the
  task's processes and passes them to qk, so a recorded task runs at close to
  its usual speed. Loading the program takes root, so qk starts a helper,
  `qk __trace-helper`: itself when it runs as root, otherwise through
  `sudo -n`, which never asks for a password. It needs Linux 5.8 or later
  built with BTF, as distributions build it, on x86-64 or arm64; in a
  container, it also needs the `CAP_BPF` and `CAP_PERFMON` capabilities.
- **strace**, on `PATH`, where eBPF cannot load. strace stops the task at
  each call it records, so a task that opens many files takes noticeably
  longer. It filters the calls it records in the kernel with seccomp where it
  can, which strace 5.3 and later do, so that other calls run at full speed.

`QK_RECORDER` picks one: `auto`, the default, records with eBPF where it
loads and with strace otherwise; `ebpf` or `strace` records only with that
one, and a run that cannot record with it fails.

## What a recording cannot see

- Reads by processes outside the task, such as a build daemon started before
  it or shared between tasks: keep such a target `off`.
- Environment variables, which are no file reads: declare them as inputs.
- A compiled program in the workspace that the task runs directly. A script it
  runs is read by its interpreter, and so recorded.
- Files opened before the task started, and reads through `io_uring`.
- A recording a task's trace was too large for, that could not be decoded
  wholly, or that lost calls the recorder had no room for, is not kept, and the summary says so. Neither is one that
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

Two more things tell whether `"on"` would be right:

- **Whether a recorded result is the one a run gives.** A task that runs
  where its recordings had a result is compared with that result: `shadow`
  carries `check`, with `matched` true when the task succeeded with the same
  output files, and otherwise the `differences`, such as `failed (exit 1)` or
  `dist/main.js (differs)`. Its terminal output is not compared, as timings
  and the like change from run to run. The run summary names each task that
  gave another result: a recording that left out something the task read, or
  a task that does not give the same result twice.
- **What changed that the declared inputs did not see.** Where the declared
  inputs took a task from the cache and its recordings had no result,
  `shadow` carries `changed`: the paths of its newest path set that are not
  as they were when it was recorded, with what the task did to each, such as
  `package.json (read)`, and `declared` where the declared inputs cover the
  path. A change they do not cover either matters, so their hits may be
  stale, or does not, and `ignoreReads` keeps the recording from keying it.
  For a path set recorded by a qk that did not keep how its paths were,
  `uncovered` names instead what it holds that the declared inputs do not
  cover, one of which changed.

The recordings also show what a task read beyond its declared inputs at
all: `QK_RECORD=audit` holds each task that runs against its declarations,
as `--sandbox` does, from the recording it already made. See [Auditing
recorded runs](sandbox.md#auditing-recorded-runs).

Looking a task up by its recordings costs the request for its path sets
whenever it has none of its own, even when its declared inputs hit the local
cache: that is where a stale hit would show. Switching to `"on"` keys it by them.

## Running the tests

`tools/ultracache/fixture` is a workspace whose build reads, lists and looks
for files, and `crates/qk-cli/tests/ultracache.rs` follows it through edits. The
tests need Linux and `strace`; `tools/ultracache/podman.sh` runs them, or any
other `cargo test` arguments, in a Linux container from another system. With
`QK_RECORDER=ebpf` it runs them with the eBPF recorder, in a container with the
capabilities it needs.

The kernel program is `crates/qk-trace/bpf/trace.bpf.c`, and the objects built
from it are committed beside it, so that building qk needs no BPF toolchain.
After changing it, rebuild them with `tools/trace/build.sh`, which needs
Podman; CI checks that they match.
