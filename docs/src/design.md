# qk: design and implementation plan

**qk** (short for **quick**) is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration without requiring an Nx
process. Its design targets reproducible execution, precise change selection,
and efficient caching across machines and Git worktrees.

This document records design intent and development milestones. The guides
and configuration reference describe implemented behavior; a proposal here
is not a claim that the corresponding feature is available.

## Goals

- **Speed:** keep workspace inspection and cached runs inexpensive, and
  measure cold and warm paths using reproducible synthetic benchmarks.
- **Affected precision:** attribute file and package installation changes to
  the projects and tasks that consume them.
- **Caching:** maintain a content-addressed local store with optional
  S3-compatible remote storage, output verification and bounded disk usage.
- **Observability:** expose run results, timings and cache-key explanations
  through documented CLI output and versioned JSON reports.
- **Compatibility:** support a clearly bounded subset of Nx configuration,
  with fixture coverage and explicit documentation of differences.

## Scope

The configuration model covers `nx.json`, `project.json`, package `nx`
objects and scripts, pnpm workspace membership and pnpm v9 lockfiles.
Task definitions include named inputs, outputs, configurations, dependency
expansion, caching and continuous execution.

Supported executor families are shell commands, package scripts and no-op
aggregation. Commands use the platform shell and receive an explicit
working directory, environment and forwarded arguments.

Nx plugins, inferred targets, generators, Nx Cloud and release automation
are outside the initial scope. Unknown metadata may be retained for
inspection without implying execution support. Any scope expansion needs
its own design, fixtures and compatibility documentation.

## Architecture

A Cargo workspace produces one `qk` binary. Libraries separate concerns:

| Concern | Responsibility |
| --- | --- |
| Configuration | Discover projects, parse files and normalize layered definitions |
| Project graph | Resolve package and implicit dependencies and project selectors |
| Lockfile | Resolve importer installations and package fingerprints |
| Affected selection | Attribute revision changes to projects or declared task inputs |
| Task graph | Expand dependencies, resolve configurations and validate execution relationships |
| Executor | Prepare commands, environments, process groups and output capture |
| Runner | Schedule tasks, manage concurrency and propagate failure or cancellation |
| Cache | Compute keys, verify and restore outputs, manage local and remote storage |
| History | Record runs, task results, key changes and critical paths |
| CLI | Map command syntax to the library interfaces and render results |

Workspace inspection must not execute task commands. Planning and execution
preflight are distinct stages: a valid task graph does not establish that
all executors, tools or runtime inputs can run successfully.

## Design decisions

### Explicit dependency graphs

Project edges come from package manifests and `implicitDependencies`.
Task edges come from `dependsOn` expansion. Shared tasks execute once;
missing targets, cycles and incompatible continuous-task relationships have
explicit, testable behavior. Source import analysis is a separate possible
extension, rather than an assumed graph input.

### Inputs connect caching and affected selection

Resolved file inputs, environment declarations, runtime values, external
packages and dependency fingerprints determine a task's cache key.
Task-level affected selection reuses the input model against a change set.
Environment and runtime inputs require conservative treatment because a
base revision's process environment cannot be reconstructed from Git.

Lockfile changes are attributed by what each importer installs, including
transitive dependencies, peer resolutions, patches and integrity data.
Resolution-only pnpm workspace changes reach tasks through the lockfile;
workspace membership and other configuration changes are handled separately.

### Versioned, verified cache entries

Cache keys and storage layouts are versioned. Keys include the tooling and
platform information needed to avoid incompatible reuse. Paths and
serialized definitions are normalized deterministically.

Explicit output declarations allow a dependency's output content to define
its fingerprint even when its own inputs change. Undeclared effects must
not be assumed equivalent merely because selected output files match.

Per-file blobs allow deduplication across entries. Manifests identify output
paths, content and file metadata. Restores verify content, remove stale
declared outputs and preserve exclusions. Restored files must not share
mutable storage with cached blobs.

Default result storage is shared by linked Git worktrees; file digests and
restore bookkeeping remain specific to each worktree. Cache location
overrides and eviction limits are configurable.

### Optional remote storage

An S3-compatible store uses its own namespace and verified manifests.
Reads fetch missing data; uploads publish output data before the manifest.
Read-only operation, missing credentials and network failures have defined
fallback behavior. Remote failures should be reported without turning a
successful task into a failed run.

### Warm state is separate from results

Incremental tools may reuse scratch state through a target's `qk:warm`
configuration. Warm state is not a valid task result and never establishes
a cache hit. The tool must validate its own state before reuse.

Restore warm state before execution, save it after success, prevent
conflicting scratch paths, and include it in storage accounting. Record
its provenance and cost so users can assess its value.

### Public run records

Persist run and task records with a versioned schema. JSON reports expose
status, timing, cache results, key-change reasons and the critical path.
Consumers should use this documented interface rather than private
implementation details.

## Development milestones

1. **Fixtures and baselines:** create self-contained workspaces and capture
   project graphs, task graphs, affected sets, execution results and timings
   from a pinned Nx release. Record accepted differences with reasons.
2. **Read side:** implement discovery, normalization, project selection,
   lockfile parsing and graph export. Validate against the fixtures.
3. **Affected selection:** cover file ownership, dependency propagation,
   workspace configuration changes and installation-aware lockfile changes.
4. **Execution:** implement dependency ordering, configurations, command
   preparation, bounded scheduling, failure propagation and cancellation.
   Validate continuous tasks and readiness using controlled fixture commands.
5. **Local caching:** verify deterministic keys, output restoration, log
   replay, corruption handling and concurrent access across worktrees.
6. **Remote storage and observability:** validate S3 modes, eviction, run
   history, key-change explanations and JSON reports.
7. **Distribution and documentation:** publish platform binaries, prepare
   package-manager installation, and maintain searchable guides and references.
8. **Refinement:** measure task-level affected selection, incremental warm
   state and concurrency controls using repeatable workloads. Expand support
   only when requirements and tests justify it.

Each milestone has observable acceptance criteria and runs in this project's
CI. Compatibility checks remain useful after a feature ships.

## Test strategy

- **Unit tests:** configuration merging, selection, dependency expansion,
  input resolution, lockfile fingerprints, interpolation and size accounting.
- **Synthetic fixtures:** nested projects, package scripts, named inputs,
  configurations, continuous tasks, output exclusions and package changes.
- **Nx parity:** compare both tools on identical fixtures using pinned
  dependencies. Differences require a documented explanation.
- **Execution and cache behavior:** exit codes, skipped dependents, process
  cancellation, environment precedence, output replay, restore verification,
  concurrent writers and remote failure modes.
- **Cross-platform CI:** Linux, macOS and Windows, including shell quoting,
  path handling and executable-file behavior.
- **Performance:** record cold inspection, unchanged affected selection and
  fully cached runs on generated workspaces; investigate measured regressions.

## Risks and open questions

- Shell syntax and argument quoting differ across platforms; document the
  supported forms and use environment variables where forwarding is limited.
- Native outputs require platform-aware cache keys; a shared store must not
  imply cross-platform artifact compatibility.
- Cached file metadata can become stale; content verification and conservative
  fallback behavior must protect correctness.
- Greater affected precision can change task selection; explain differences
  and keep regression cases demonstrating why the result is correct.
- Warm state can be stale or incomplete; tools must validate it independently
  of result caching.
- Keep bootstrapping independent of qk's own task graph: Cargo builds the
  runner, and installation must work before workspace tasks can execute.

## Bounded project watching

Project watching uses native filesystem events and the explicit project graph.
It selects projects and optional dependency closures, refreshes discovery when
files change, debounces event bursts, and queues edits while callbacks execute.
Callbacks use the invocation directory and Nx's project/file-change environment
variables. Cancellation uses the executor's process-group cleanup. It does not
run plugins, sync generators or a daemon. Declared outputs, warm paths and runner
state are excluded to prevent feedback loops. Tests cover dependency selection,
new project discovery, queued changes and cancellation.

## Warm state for native builds

**Status: implemented.** [Warm state](guides/warm-state.md) and the
[`qk:warm` reference](reference/targets.md#qkwarm) describe the behavior;
this section records why it takes this shape.

Native build systems keep two kinds of state. Some is bound to the checkout's
absolute path and trusts file timestamps, as Gradle's execution history,
CMake's build directories and Ninja do. Some is content-addressed and
path-independent, as ccache is when its base directory is the workspace root.
The first kind makes rebuilds fast inside the worktree that produced it and is
worth little elsewhere, where CMake refuses a cache created in another
directory. The second kind carries across worktrees and machines. The
portability and timestamp controls distinguish these two kinds of state.

A React Native Android debug build, whose prebuild step recreates the native
project on every run, measured once on one machine:

| Situation | Build |
| --- | --- |
| Same worktree, build state deleted by the prebuild | 35–40 s |
| Same worktree, build state moved aside and back | 7–10 s |
| Same worktree, build state restored with Unix-epoch timestamps | 24 s |
| New worktree, cold | 182 s |
| New worktree, Gradle state copied from another worktree | 167 s |
| New worktree, CMake state copied from another worktree | Fails |
| New worktree, ccache filled by another worktree | 98 s |

### Keeping state across a destructive dependency

```jsonc
"android": { "qk:warm": { "paths": ["…"], "survive": ["prebuild-android"] } }
```

`survive` names dependency targets that delete the warm paths. qk moves the
paths aside before such a dependency runs and back after it succeeds, so the
state keeps its files and timestamps without being stored or copied. If the
dependency fails, the paths return unchanged. A path the dependency itself
recreates is replaced by the kept one, so `survive` suits generated trees
that hold build state, not a dependency's own outputs.

### Timestamps

`mtimes: "preserve"` restores files with the modification times they were
saved with, instead of the Unix epoch. Epoch times make `tsc --build` check
sources against restored state; they also make Ninja rebuild every restored
object and Gradle rehash every restored file. Preserved times are only
meaningful against the worktree that saved them, so they apply to a
worktree's own save and fall back to the epoch for any other.

### Preferring the worktree's own save

A restore chooses the worktree's own most recent save before any other
worktree's. Before this, the most recent save of the task won, whichever
worktree made it.

### Groups that do not relocate

`portable: false` restores a group only from the worktree that saved it. State
that records absolute paths, such as CMake build directories, then never
reaches a checkout it would break or merely occupy. Portable groups can
restore from other worktrees and the remote store.

### Named groups shared between targets

```jsonc
"qk:warm": {
  "group": "ccache",
  "env": { "CCACHE_DIR": "{warm}", "CCACHE_BASEDIR": "{workspaceRoot}" }
}
```

A named group is one piece of warm state that several targets use, as one
ccache serves every native build. Its targets share the live directory,
which the tool itself keeps consistent, so only restoring and saving it are
serialized, and each save holds everything the directory held. A group has
no `outputs` or `paths`, which belong to one task. Combined with remote warm
state, a new worktree or machine starts from the default branch's ccache.

### Choosing a save by key

```jsonc
"qk:warm": {
  "key": ["{workspaceRoot}/pnpm-lock.yaml", { "env": "ANDROID_NDK_VERSION" }],
  "restoreKeys": 1
}
```

A group keeps several saves, each labeled with a hash of the declared `key`
inputs. A restore takes the save with an exact key, else one matching the
first `restoreKeys` entries, else none. Among equal candidates, the save
made at the commit nearest the checkout's `HEAD` in Git history wins over the
newest, so a worktree cut from `main` starts from `main`'s state. The key only
chooses a save; it never makes warm state a result.

### Excluding and globbing paths

A path starting with `!` omits files no later build reads, such as packaged
artifacts that are rebuilt on every run, as it does for outputs. Glob
patterns in `paths` name directories whose location depends on package
versions, as pnpm places a package's native build under a versioned
directory. Excluded files stay out of task inputs, like the rest of a warm
path.

### Saving in the background

`save: "background"` saves after the task has reported success, so the next
task starts without waiting. Saves and restores of a group are serialized,
and the run waits for background saves before it finishes its remote
uploads, which a save may add to.

### Suggesting warm paths

`qk warm suggest <task>` runs the task in the sandbox and lists what it wrote
outside its declared outputs as candidate warm paths, each under its topmost
directory that holds no source file. Native builds write to places that are
hard to guess, such as build directories that a build script moves to the
workspace root. It needs macOS, where the sandbox can report rather than
refuse.

### Reporting warm state's effect

Run records hold where warm state came from, what was restored, which
groups were on disk already and how long saving took. `qk show task`
compares the task's duration from warm state with its duration without,
which shows whether its warm state is worth its size.
