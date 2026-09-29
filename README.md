# qk

**qk**, short for **quick**, is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration and executes tasks with
dependency ordering. Affected selection, caching and run history are planned.

The current implementation supports **workspace inspection and finite task
execution**. Lockfile analysis and caching are not implemented yet. The
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
| `qk show projects [--json]` | Sorted project names, one per line or as a JSON array |
| `qk show projects -p 'web,tag:library' --exclude 'experimental-*'` | Union of matching names, globs and tags, minus exclusions |
| `qk show project <name> [--json]` | Normalized project configuration as JSON |
| `qk graph --file <path>` | Workspace project graph in a `{ "graph": { "nodes": ..., "dependencies": ... } }` envelope |
| `qk graph` or `qk graph --file -` | The same graph on stdout |
| `qk run <project>:<target>[:<configuration>]` | Execute a task and its dependencies |
| `qk run-many -t build,test -p 'web,core' --parallel 4` | Execute matching targets with bounded task concurrency |
| `qk run web:build --dry-run` | Print the planned task graph as JSON without execution |

`--workspace <path>` works before or after the subcommand. Graph output paths
are relative to the invocation directory; their parent directories must
already exist. JSON output has no progress messages mixed into stdout.
Errors go to stderr and return a nonzero exit code. Unknown commands and
flags, including `affected` and `--affected`, fail explicitly.

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
`NX_PARALLEL`), `--output-style stream` and `--dry-run`.

The planner expands `dependsOn` before execution: local targets, `^target`
on project dependencies, `project:target`, and objects with `target`,
`projects`, `dependencies` and `params`. Project selectors accept names,
globs, tags, `self` and `!self`. Missing dependency targets are skipped;
unknown explicit projects and task cycles fail. Shared tasks run once.
Configurations propagate to dependencies that define the same name;
otherwise the dependency's default configuration applies. A shared task
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
to `PATH`. Tasks currently have closed stdin; interactive and continuous
tasks are not supported. stdout and stderr stream directly, with no task
prefix added to child output. qk writes its own status messages to stderr.

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
executors, options and continuous tasks fail before dependencies run. A
failed task skips its dependents while independent tasks continue. The CLI
returns the first observed failing task's exit code. Ctrl-C and, on Unix,
SIGTERM cancel the run, terminate managed process groups and return `130`.
Parallel commands within a failed task are also terminated. Commands must
not detach themselves into separate sessions or launch external services;
those processes are outside the managed group.

**Execution is uncached**, even for targets with `cache: true`. `--dry-run`
only plans and prints the task graph: it does not load dotenv, validate
executor support, execute runtime inputs, or run commands.

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
  Executor defaults take precedence. Options merge by key; each named
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

- The graph includes **workspace projects only**. External dependencies,
  lockfile versions, workspace dependency aliases and version-range resolution
  await the lockfile layer. Name matching is currently conservative.
- Target-default glob keys and filtered defaults are not implemented;
  filtered default arrays are rejected. Nx plugins and inferred targets are
  outside the design's scope.
- Input expressions and runtime inputs are preserved for inspection; hashing
  and affected semantics are not resolved yet.
- `affected`, continuous and interactive tasks, output capture/replay,
  static/dynamic output styles, cache storage, remote storage, history,
  release commands and npm binary distribution remain future work.

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

The Cargo workspace contains six crates:

| Crate | Responsibility |
| --- | --- |
| `qk-config` | Workspace discovery, JSONC/YAML parsing, typed project configuration and normalization |
| `qk-graph` | Workspace edges, project selection, reverse reachability and graph export |
| `qk-taskgraph` | Dependency expansion, configuration selection and task DAG validation |
| `qk-executor` | Command preparation, environment, argument interpolation and process groups |
| `qk-runner` | Bounded task scheduling, dependency failure propagation and cancellation |
| `qk-cli` | Argument parsing and output; produces the `qk` binary |

Fixture and CLI tests use self-contained workspaces. GitHub Actions is
configured for Linux, macOS and Windows. The lockfile is checked in for
reproducible dependency resolution.

Next: expand execution parity and continuous-task lifecycle handling, then add
pnpm v9 lockfile parsing and external graph nodes before hashing and caching.
