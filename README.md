# qk

**qk**, short for **quick**, is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration and executes tasks with
dependency ordering, a local cache and Nx's affected selection. Remote
caching and run history are planned.

The current implementation supports **workspace inspection, finite task
execution and a local task cache** shared by the worktrees of a Git
repository, keyed by the packages each task's projects install according to
the pnpm lockfile. The
[design](docs/task-runner-design.md) describes the longer-term plan, not the
current feature set.

## Try it

Install a current stable Rust toolchain, then run from this repository:

```sh
cargo run -p qk-cli -- --workspace examples/basic show projects
cargo run -p qk-cli -- --workspace examples/basic show project web --json
cargo run -p qk-cli -- --workspace examples/basic show projects --projects 'tag:scope:*' --json
cargo run -p qk-cli -- --workspace examples/basic graph --file -
cargo run -p qk-cli -- --workspace examples/basic run codegen:smoke
```

Inspection and the `codegen:smoke` task are self-contained; no Node
installation, package installation, Nx process is required. Other example targets demonstrate package scripts
and require pnpm when executed.

To install the binary locally:

```sh
cargo install --path crates/qk-cli --locked
qk --help
qk --workspace /path/to/your/workspace show projects --json
```

Inside a workspace, omit `--workspace`. qk searches ancestors for `nx.json`,
`pnpm-workspace.yaml` or a root `package.json` with `workspaces`. A standalone
`project.json`, or a package with an `nx` object, is also supported when no
parent workspace marker exists.

## Current commands

| Command | Result |
| --- | --- |
| `qk show projects [--json]` | Project names in Nx graph order, one per line or as a compact JSON array |
| `qk show projects -p 'web,tag:library' --exclude 'experimental-*'` | Union of matching names, globs and tags, minus exclusions |
| `qk show project <name> [--json]` | Normalized project configuration as JSON |
| `qk graph --file <path>` | Workspace project graph in a `{ "graph": { "nodes": ..., "dependencies": ... } }` envelope |
| `qk graph` or `qk graph --file -` | The same graph on stdout |
| `qk run <project>:<target>[:<configuration>]` | Execute a task and its dependencies |
| `qk <target> [project]`, `qk <project>:<target>` | Nx-style shorthand for `qk run` |
| `qk run-many -t build,test -p 'web,core' --parallel 4` | Execute matching targets with bounded task concurrency |
| `qk run web:build --dry-run` | Print the planned task graph as JSON without execution |
| `qk show projects --affected [--base <rev>] [--head <rev>]` | Projects affected by the changes, in graph order |
| `qk affected -t build,test [--base <rev>] [--head <rev>]` | Execute targets on the affected projects |
| `qk show affected [project] [--json]` | Why projects are affected, or why one project is |
| `qk affected -t build --granularity task` | Execute only the tasks whose inputs changed |
| `qk show tasks -t build,test --affected [--json]` | The affected tasks and why |
| `qk show runs`, `qk show run [id]` | Recent runs; one run's tasks, cache results and critical path |
| `qk show task <project:target>` | A task's recent runs and why its cache key changed |
| `qk cache path` | Print the local cache directory without creating it |
| `qk cache prune [--max-size 1GB]` | Evict least recently used entries until the cache fits |

As in Nx, `qk build web` and `qk web:build` mean `qk run web:build`, and take the
same options. Without a project, `qk build` and `qk run build` use the project
whose root most specifically contains the current directory. qk's own
subcommands, and Nx commands qk does not implement such as `format` and
`release`, are never read as targets; use `qk run` for a target
with one of those names.

`--workspace <path>` works before or after the subcommand. Graph output paths
are relative to the invocation directory; their parent directories must
already exist. JSON output has no progress messages mixed into stdout.
Errors go to stderr and return a nonzero exit code. Unknown commands and
flags fail explicitly.

### Affected

As in Nx 23, changed files are those between the merge base of `--base` and
`--head`, or with no head, between the merge base and the working tree,
including uncommitted and untracked files. The base defaults to `NX_BASE`,
then nx.json `defaultBase`, then `main`; the head to `NX_HEAD`. `--files`,
`--uncommitted` and `--untracked` replace the comparison. Files matching the
root `.gitignore` or `.nxignore` are left out.

A changed file touches the project whose root most specifically contains it.
`nx.json` touches every project; a file named by a `{workspaceRoot}` input
touches the projects declaring it; a deleted `project.json` or
`package.json` touches every project. Dependencies changed in the root
`package.json` touch the projects installing the package, and path mappings
changed in the root tsconfig touch the projects they point into. Every
project depending on a touched project is affected too.

Two rules deliberately differ from Nx:

- A `pnpm-workspace.yaml` change confined to resolution keys (`catalog`,
  `catalogs`, `overrides`, `patchedDependencies` and the like) reaches
  projects only through the lockfile.
- Under `projectsAffectedByDependencyUpdates: "auto"`, a `pnpm-lock.yaml`
  change touches the projects whose importer installs something different,
  by the same installed sets the cache keys use. This leaves out projects
  that install exactly what they did before, which Nx reports, and catches
  transitive changes such as a dependency's dependency moving version,
  which Nx can miss. Other modes behave as in Nx.

`show projects --affected` prints projects in graph order; Nx prints its
traversal order, so compare the two as sets.

`qk show affected` takes the same change options and explains the result.
Without a project it lists every affected project with the first reason it
is touched, or the dependency it is affected through. With a project it
prints a shortest dependency path to a touched project, the project's other
affected dependencies, and every reason the touched project is touched. For
a lockfile reason it lists what the importer installs differently, by
package: versions that moved first, then packages added or removed, then
those that kept their version but resolved different peers or patch, then
those whose dependencies changed as a consequence.

```text
$ qk show affected web --base HEAD^ --head HEAD
2 changed files between 3f1c0a9e2b7d and HEAD.
web is affected because it depends on ui:
  web -> ui (static)
ui is touched:
  what packages/ui installs changed in pnpm-lock.yaml (3 packages)
      react (direct): 19.0.0 -> 19.1.0
      react-dom (direct): 19.0.0: peers or patch changed
      scheduler: dependencies or integrity changed
```

With `--granularity task`, `qk affected` selects tasks instead of projects: a
task is affected when a changed file is one of its resolved inputs, when what
its lockfile importers or `externalDependencies` install changed, when
`pnpm-workspace.yaml` changed outside its resolution keys, when a project
manifest was deleted, or when it depends on an affected task. These are the
same inputs its cache key reads, so an unaffected task would be a cache hit,
except that env and runtime inputs count as unchanged, since the base
revision's environment is unknowable. A test-only change then reaches the
test tasks and not the builds whose `production` inputs exclude tests.
`project` stays the default, as in Nx. `qk show tasks -t <targets> --affected`
lists the affected tasks with their first reason, or the whole analysis with
`--json`.

`--json` gives the whole analysis, or for one project its path with the
typed reasons, including the snapshot keys a lockfile change added,
removed or changed.

Project selectors support `*`, `?`, character classes and `tag:<glob>`.
Repeat `--projects` or use commas to combine selectors. Prefix a selector
with `!` to exclude it; exclusions win regardless of order. With only
exclusions, selection starts with all projects. A selector matching nothing
returns an empty list; `show project` requires an existing exact name.
Quote globs so the shell does not expand them. Commas delimit CLI selectors,
so brace globs containing commas are not supported on the command line.

## Task execution

`run` requires an existing target; `run-many` skips projects without the
requested targets and errors when nothing matches. Both accept
`-c/--configuration`, `--parallel` (default `3`, also settable through
`NX_PARALLEL`), `--output-style` and `--dry-run`.

`--output-style` takes Nx's names. `stream` prefixes each non-empty line with
the task's project, in Nx's colour for it; `stream-without-prefixes` passes
output through untouched; `static` holds each task's output until it ends and
prints it under `> qk run <task>`, marked `[local cache]` for a hit, and in
GitHub Actions folds each task into a log group unless
`NX_SKIP_LOG_GROUPING=true`. `tui`, `dynamic` and `dynamic-legacy` render as
`static`, since qk has no interactive terminal output. Without the option,
`NX_DEFAULT_OUTPUT_STYLE` applies; otherwise `run` passes output through, and
`run-many` and `affected` use `static` in CI or when stdout is not a terminal
and `stream` on one. Continuous tasks always stream with prefixes, since their
output would otherwise never appear. Colour follows picocolors: off with
`NO_COLOR`, on with `FORCE_COLOR`, in CI or on a terminal.

The planner expands `dependsOn` before execution: local targets, `^target`
on project dependencies, `project:target`, and objects with `target`,
`projects`, `dependencies` and `params`. Project selectors accept names,
globs, tags, `self` and `!self`. As in Nx, a project dependency without the
target is looked through: the task's `dependsOn` applies again from that
project, so `^tsc` reaches the nearest dependencies that have `tsc`. Other
missing dependency targets are skipped; unknown explicit projects and task cycles fail. Shared tasks run once.
Configurations propagate to dependencies that define the same name;
otherwise the dependency's default configuration applies. `run-many -c`
treats the targets it selects the same way. A shared task
reached with different forwarded arguments is rejected as ambiguous.

Supported executors:

- `nx:run-commands`: `command`, or a `commands` array of strings or objects
  with `command` and `forwardAllArgs`. Multiple commands run concurrently by
  default; `options.parallel: false` runs them in sequence. Supports `cwd`,
  `env` and `forwardAllArgs`. The default cwd is the workspace root.
- `nx:run-script`: invokes `npm run` or `pnpm run` in the project directory,
  preserving the package manager's script behavior. The manager comes from
  root `packageManager`, then pnpm workspace/lockfile markers, otherwise npm.
  Other declared managers are rejected. Missing scripts fail during execution
  preflight, after explicit project target overrides have been applied.
- `nx:noop`: completes successfully after its dependencies, without a process.

Commands use `/bin/sh -c` on Unix and `cmd.exe /D /S /C` on Windows. Local
`node_modules/.bin` directories from cwd up to the workspace root are added
to `PATH`. Tasks currently have closed stdin; interactive tasks are not
supported. stdout and stderr stream directly, with no task prefix added to
child output. qk writes its own status messages to stderr.

Environment precedence, highest first: configuration `env`, `options.env`,
target-level `env`, inherited process environment, root `.env.local`, root
`.env`. Dotenv is loaded into child environments without mutating qk's process
environment. Dotenv interpolation uses dotenvy's per-file semantics; it does
not provide cross-file interpolation against the merged child environment.

Forward task arguments after `--`:

```sh
qk run app:build -- --mode=production 'two words'
qk run app:package -c release -- --arch=arm64
```

Commands support `{projectRoot}`, `{workspaceRoot}`, `{projectName}`,
`{args}` (all forwarded arguments), and `{args.name}` (a `--name=value` or
`--name value` argument; a flag without a value becomes `true`). Place argument
tokens outside quotes: qk quotes their values as shell data. Missing named
arguments and quoted argument placeholders are errors. On Windows, forwarded
values containing quotes, `%`, `!`, `^` or newlines are currently rejected;
use target environment variables for those values. Workspace/project path
tokens are substituted as written, so quote path tokens where your command
requires it.

Without argument tokens, arguments are appended by default; set
`forwardAllArgs: false` to disable that. With argument tokens, only the
explicit substitutions are used. Dependencies receive no arguments unless
their dependency object sets `params: "forward"`.

All selected tasks are prepared before any command starts, so unsupported
executors and options fail before dependencies run. A failed task skips its
dependents while independent tasks continue. The CLI returns the first
observed failing task's exit code. Ctrl-C and, on Unix, SIGTERM cancel the
run and return `130`. Cancelled commands receive SIGTERM on Unix and are
killed with their process group if they have not exited within five seconds;
on Windows they are killed directly. Parallel commands within a failed task
are terminated immediately. Commands must
not detach themselves into separate sessions or launch external services;
those processes are outside the managed group.

Targets with `continuous: true`, such as dev servers and watchers, run until
they exit or are stopped. A task that depends on a continuous task starts as
soon as that task has started, not when it finishes, so a client must wait for
the server's readiness itself. Continuous tasks do not count towards
`--parallel`. A requested continuous task runs until it exits or the run is
cancelled. A continuous task that only other tasks depend on is stopped, like
a cancelled command, once all of them have finished; that stop counts as
success. A continuous task that exits by itself reports its exit status as
usual. Continuous tasks are never cached, and cacheable tasks depending on one
run uncached.

`--dry-run` only plans and prints the task graph: it does not load dotenv,
validate executor support, execute runtime inputs, or run commands.

## Local cache

Targets with `cache: true` are cached locally. Inside a Git repository the
cache lives in `<git common dir>/qk/cache/v1`, so every linked worktree of the
repository shares one cache. Outside Git it lives in `.qk/cache/v1` at the
workspace root. `qk cache path` prints the location. `--skip-cache` (also
`--skip-nx-cache` and `--skipNxCache`) bypasses all cache reads and writes.

A task's key covers its ID, forwarded arguments, resolved target definition,
declared inputs, the fingerprints of its dependency tasks (including their
outputs), the package manager, qk's version and the platform. Files are keyed
by workspace-relative path, content and mode. Branch names and checkout
locations are not part of the key, so identical sources in two worktrees
share entries. Root workspace files (the root `tsconfig.base.json` or
`tsconfig.json`, `nx.json`, `package.json`, `pnpm-workspace.yaml`,
lockfiles, `.env`, `.env.local`) and the manifests of
the task's project and its transitive project dependencies are always
included.

A pnpm v9 `pnpm-lock.yaml` is keyed by what it installs rather than by its
content: for the root importer and the importers of the task's project and
its transitive project dependencies, every package installation they reach,
by snapshot key (version, peers and patch), integrity and dependencies. The
root importer counts for every task because its packages resolve from every
package. A lockfile change therefore invalidates only the tasks whose
projects install something that changed, plus every task when the lockfile
version, pnpm's `settings` or the lock of pnpm itself changes. A lockfile qk
cannot read is keyed by its whole content, and qk says so.

With such a lockfile, `pnpm-workspace.yaml` is keyed without the keys that
configure resolution (`catalog`, `catalogs`, `overrides`,
`patchedDependencies`, `peerDependencyRules` and the like), whether it is
always included or named by an input: their effect lands in the lockfile, so
a catalog bump invalidates only the tasks whose installs change. This is the
same rule affected selection applies.

Candidate input files are tracked and untracked-but-not-ignored files, minus
any task's declared outputs. The list is taken once per run, as Nx does, so a
file that a task creates without declaring it as an output is seen from the
next run. File contents are re-read whenever their metadata changes. Without
`inputs`, a task uses `default` and `^default`; `default` is
`{projectRoot}/**/*` unless a named input overrides it.

Supported input declarations are globs with `!` exclusions, `fileset`, named
inputs and `^named` inputs, which as in Nx cover every project the project
depends on, directly or not, `env`, `runtime`, `dependentTasksOutputFiles`
with `transitive`, and `externalDependencies`, which adds every installation
of the named packages in the pnpm lockfile, whichever importer installs them.
With another package manager, lockfiles are always keyed by content. `runtime` commands run once per run for each environment.
`dependentTasksOutputFiles` needs no file access: every dependency's
fingerprint already covers its declared outputs. `.` and `..` segments in
paths are resolved within the workspace. A symlinked input is keyed by its
target text and the content it resolves to, including the files below a
linked directory; links that resolve outside the workspace are not supported.

Extended globs (`?(…)`, `*(…)`, `+(…)`, `@(…)`, `(a|b)` and `{,…}`) are expanded
exactly as Nx 23 expands them, including its approximations: `+(a|b)` matches
one occurrence, and an omitted group in a directory segment widens that
segment to `*`. This keeps input sets written for Nx selecting the same files.

A task with any other input declaration, negation inside a glob such as
`!(a|b)`, `{options.*}` or `{args.*}` paths, negated outputs, or an output
without a fixed directory prefix runs uncached and reports why. Its dependents
then run uncached too, naming the task and reason they depend on.

On a miss, the task runs with stdout and stderr streamed through a pipe while
they are recorded, so child processes do not see a terminal. The entry is
saved only when the task succeeds and its inputs are unchanged afterwards.
On a hit, qk removes existing files matching the declared outputs, copies the
cached outputs into place and replays the recorded stdout and stderr. Restores
are staged in `.qk/` at the workspace root. Saving and restoring copy files,
which clones them on copy-on-write filesystems such as APFS when the cache and
the checkout share a volume. Files are never hardlinked, so editing a restored
file never changes the cache. Every file is verified
against its content hash; an unreadable or corrupt entry is a miss.
A per-key lock makes concurrent runs of the same task, including runs in
different worktrees, wait for each other and reuse the result.

### Run history

Every run is recorded in a SQLite database beside the cache
(`<git common dir>/qk/history.db`, or `.qk/history.db` outside Git), so linked
worktrees share it. Each task's record holds its status, cache result
(`local-hit`, `remote-hit`, `miss` or `uncached`), key, timing and cause: what
differs from the previous key recorded for the task, grouped as `files` (with
the paths added, removed and changed), `env`, `runtime`, `dependencies` (with
the dependency tasks), `lockfile` (with the importers and packages),
`inputs`, `definition`, `args` and `tooling`, or `first`, `unchanged` and
`unknown` when the previous inputs are no longer kept. The newest 200 runs are
kept. The schema is versioned in `schema_version`.

`--report <path>` (or `NX_RUN_REPORT`) on `run`, `run-many` and `affected`
writes the run as JSON: the command, commit, exit code, each task with its
record, and the critical path, the dependency chain with the longest total
duration. `qk show run --json` prints the same for a recorded run.

```text
$ qk show task web:build
web:build, most recent first:
  run 1790687500772-75771  2 min ago  success  miss  4.1s
      key changed since run 1790687500464-74260: dependencies, files
        changed packages/ui/src/index.ts
        dependency ui:build
```

### Remote cache

nx.json's `s3` key, as `@nx/s3-cache` reads it, adds a remote store on
S3-compatible storage: `bucket`, `region`, `endpoint`, `forcePathStyle`,
`cacheKeyPrefix`, `accessKeyId` and `secretAccessKey`. Credentials otherwise
come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
`AWS_SESSION_TOKEN`, including from the workspace's `.env` files. `localMode`
applies outside CI and `ciMode` when `CI` is set; each is `read-write` (the
default), `read` (also spelled `read-only`) or `no-cache`, and
`NX_POWERPACK_CACHE_MODE` overrides both. `encryptionKey` and SSO profiles are
not supported; a store that cannot be used is reported and left out, and the
local cache carries on.

Entries are stored under `<cacheKeyPrefix>qk/v1/` in the local layout, so a
bucket shared with Nx never mixes the two. A local miss fetches the manifest
and only the outputs the local cache lacks, verifying each against its hash,
and shows `[remote cache]`; any failure is a miss. After a task is saved it is
uploaded in the background, outputs first and the manifest last, and a run
waits for its uploads before it ends. Uploads report their failures without
failing the run.

The cache stays under a size limit: `NX_MAX_CACHE_SIZE`, else nx.json
`maxCacheSize`, else a tenth of the disk holding it, as in Nx. Sizes are a
number of bytes with an optional `KB`, `MB` or `GB`, in powers of 1024; `0`
means unlimited. After each run, and on `qk cache prune [--max-size <size>]`,
qk evicts the least recently used entries until the cache fits. A hit counts
as a use. Outputs shared between entries are stored once and deleted only
with the last entry citing them. Stored outputs no entry cites, scratch files
and locks of evicted entries are removed once they are an hour old, since a
concurrent run may still be writing them. A run under the limit only sums the
cache's size; the full pass runs at most hourly unless the cache is over its
limit. `NX_CACHE_DIRECTORY` is not read.

## Configuration and graph support

- Reads JSON with comments, trailing commas and `"//"` comment keys.
- Discovers `project.json` files, including nested projects, and package
  manifests selected by pnpm workspace globs or `package.json` workspaces.
  pnpm patterns take precedence when both are present. Exclusion globs win.
  An explicit `project.json` remains a project even outside package globs.
- Includes a root package as a project when it has an `nx` object or an
  adjacent `project.json`. Package scripts become `nx:run-script` targets;
  `includedScripts` supplies that set, including an empty array to disable it.
  Listed names need not exist in `scripts`: they initially declare script
  targets, which explicit targets in package `nx` configuration or
  `project.json` can override. Script existence is not validated during inspection.
- Merges package `nx` configuration with adjacent `project.json`, with the
  latter taking precedence. Explicit project names win over package names;
  projects without either use their relative directory with `/` replaced by
  `-`. Duplicate names fail with both project roots in the error.
- Applies exact executor-keyed or target-name-keyed `targetDefaults`.
  Executor defaults take precedence. As in Nx, a default naming a different
  executor from the target's own is not applied. Options merge by key; each named
  configuration merges by key; nested values and arrays are replaced.
  Project-level named inputs override workspace definitions by name.
- Normalizes a target's `command` shorthand into `nx:run-commands` options.
  Changing an executor drops options and configurations from the previous
  executor. Inspection preserves other executor names and unknown metadata.
- Builds workspace edges from the four dependency sections in package
  manifests, matching declared dependency names to workspace package names.
  Adds `implicitDependencies` selected by names, globs or tags; negations
  remove matching edges. Unknown exact implicit dependencies are errors.
- Emits sorted project nodes and deduplicated dependency edges, with portable
  workspace-relative paths. Cycles are allowed in the project graph; reverse
  dependency traversal terminates even when cycles exist.

Discovery respects workspace ignore files, skips symlink directories and
excludes `node_modules`, `.git`, `.nx`, `.qk`, `.pnpm-store`, and the root
Cargo `target` directory. Parent and global Git ignore rules do not affect
discovery. Hidden project directories otherwise remain discoverable.

This is a subset of the design's compatibility surface. There is no Nx
parity claim yet. In particular:

- The graph includes **workspace projects only**, as `nx graph --file`
  does. The pnpm lockfile is read for cache keys, not for graph nodes.
  Workspace dependency aliases and version-range resolution are not
  implemented; name matching is conservative.
- Target-default glob keys and filtered defaults are not implemented;
  filtered default arrays are rejected. Nx plugins and inferred targets are
  outside the design's scope.
- Interactive tasks, interactive output styles, release commands and npm
  binary distribution remain future work.

The compatibility baseline is documented in Nx's
[project configuration](https://nx.dev/docs/reference/project-configuration)
and [workspace configuration](https://nx.dev/docs/reference/nx-json)
references. The bounded subset above and fixture tests define qk's current
behavior; new Nx features are not automatically supported.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

The Cargo workspace contains eight crates:

| Crate | Responsibility |
| --- | --- |
| `qk-config` | Workspace discovery, JSONC/YAML parsing, typed project configuration and normalization |
| `qk-graph` | Workspace edges, project selection, reverse reachability and graph export |
| `qk-taskgraph` | Dependency expansion, configuration selection and task DAG validation |
| `qk-executor` | Command preparation, environment, argument interpolation, process groups and output capture |
| `qk-runner` | Bounded task scheduling, dependency failure propagation and cancellation |
| `qk-lockfile` | pnpm v9 lockfile parsing and what each importer installs |
| `qk-cache` | Input hashing, cache entry storage, output restoration and log replay |
| `qk-cli` | Argument parsing and output; produces the `qk` binary |

Fixture and CLI tests use self-contained workspaces.

### Parity with Nx

`tools/parity/parity.mjs` measures qk against Nx. By default it uses the
synthetic workspace in `tools/parity/fixture` and the goldens committed in
`tools/parity/goldens`, captured with the Nx release pinned in
`tools/parity/nx`. It needs Node, and pnpm for the cases that run package
scripts:

```sh
node tools/parity/parity.mjs compare --qk target/release/qk
```

Each run builds a scratch git repository from the fixture. The cases in
`tools/parity/cases.json` compare task graphs, `show projects --affected`
for a change applied on a branch (the files in `tools/parity/changes/<case>`,
plus any deletions the case lists), and runs, which execute tasks without
cache and compare whether the run failed and which tasks ran. The comparison
requires byte-identical `show projects --json`, a `graph --file` equal after
normalisation, and task graphs with the same tasks, dependencies, and cache
and continuous flags. The differences it normalises away are listed at the
top of the script. An affected or run case that differs fails unless it
records the exact difference it accepts and why.

After changing the fixture or the pinned Nx, recapture and commit the
goldens:

```sh
(cd tools/parity/nx && pnpm install)
node tools/parity/parity.mjs capture
```

`capture --check` captures into a scratch directory and fails when the
committed goldens differ. CI runs both `compare` and `capture --check`.

The harness can also point at any workspace with its own Nx installation
and git history, reading cases from `<goldens>/parity.json`, with affected
cases given as `base` and `head` revisions:

```sh
node tools/parity/parity.mjs capture --workspace <dir> --goldens <dir>
node tools/parity/parity.mjs compare --workspace <dir> --goldens <dir> --qk target/release/qk
```

For an external workspace, capture also records Nx's median wall times, and
compare prints them beside qk's.

GitHub Actions is configured for Linux, macOS and Windows. The lockfile is
checked in for reproducible dependency resolution.

Next: external graph nodes, benchmarks and binary distribution.
