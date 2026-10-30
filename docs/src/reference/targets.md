# Target configuration

Targets appear in a project's `targets` object or workspace `targetDefaults`.
The example uses explicit command execution and output declarations:

```jsonc
{
  "targets": {
    "build": {
      "executor": "nx:run-commands",
      "options": { "command": "pnpm exec tsc --build", "cwd": "{projectRoot}" },
      "configurations": { "release": { "args": "--verbose" } },
      "dependsOn": ["^build"],
      "inputs": ["default", "^default"],
      "outputs": ["{projectRoot}/dist"],
      "cache": true
    }
  }
}
```

## Merging

A target can be defined in several places: target defaults, package.json
and project.json (see [Configuration precedence](projects.md#configuration-precedence)).
As in Nx 24, each definition merges over the one beneath it field by field:
a field it sets replaces the one beneath, except that `options` merge by
option name and `configurations` by configuration name and then by option
name. Arrays and nested option values are replaced.

A definition that runs the target differently replaces the one beneath it
whole, every field included: it names another executor, or for
`nx:run-commands` another command, or for `nx:run-script` another script.
A definition without an executor, command or script merges over any.

The spread token `"..."` merges instead of replacing. In an array, such as
`inputs`, `outputs` or `dependsOn`, the item `"..."` stands for the items
beneath it, in its place: `"inputs": ["mine", "..."]`. In an object, the
key `"...": true` stands for the fields beneath it: a field written after
it wins over the one beneath, and one written before it yields to it. It
works on the target itself, on `options` and each option's value, on
`configurations`, on each configuration and each of its options' values.
Beneath that, a spread is an ordinary value. With nothing beneath it, a
spread adds nothing. An object with `"..."` cannot also have a key that is
a whole number, such as `"1"`: Nx cannot tell where it was written.

A target key that is a glob, such as `"e2e-ci--*"`, merges onto every target
defined beneath it whose name it matches, and is a target of its own when it
matches none. Keys are globs and match as `targetDefaults` keys do (see
[Workspace](workspace.md#targetdefaults)): `*` does not cross a `/`.

## `executor`

A string naming `nx:run-commands`, `nx:run-script`, or `nx:noop`.
Unsupported names remain inspectable but fail execution preflight.
See [Executors](executors.md) for their options. A target with neither an
executor nor a command runs `nx:noop` when it has `dependsOn`, and is left
out otherwise, as in Nx.

## `command`

A shorthand command normalized into `nx:run-commands` options. Prefer an
explicit executor with `options.commands` for multiple commands. A target
cannot have both `command` and `executor`.

## `options`

An object, empty by default, of executor-specific options. Selected
configuration values merge over these by key, followed by task argument
overrides. Nested objects, including `env`, are replaced rather than recursively
merged. A configuration with `env: {}` removes the base option's overrides;
process and dotenv values still apply.

As in Nx, the tokens `{projectRoot}` and `{projectName}` are replaced in
every string of the options and configurations, nested ones included, as the
workspace loads, and `qk show project` shows the result. `{workspaceRoot}`
may only start a string, and is removed with the slash after it, leaving a
path relative to the workspace root: `"{workspaceRoot}/dist"` becomes
`"dist"`. Anywhere else it is an error.

## `configurations`

An object mapping configuration names to option objects, empty by default.
Select with `-c release` or `project:target:release`. `--prod` selects
`production`. Requested configuration names propagate through dependencies.
A dependency without that configuration uses its own default, while its
dependencies still receive the requested name. A configuration's keys are
all options: `outputs` in a configuration is an option named `outputs`, not
the target's outputs, as in Nx.

## `defaultConfiguration`

An optional string selecting a configuration when none is requested.
Without a requested configuration, each task chooses its own default;
a parent's default does not propagate to its dependencies.

## `dependsOn`

An array of dependency declarations. Without it, a target has no declared
task dependencies. Shared dependencies run once.

| Declaration | Meaning |
| --- | --- |
| `"build"` | Target in the same project |
| `"^build"` | Target on project dependencies, looking through projects without it |
| `"core:build"` | Target in an explicit project |
| `"lint-*"` or `"^build-*"` | Matching target names |
| `{ "target": "build", "projects": ["tag:library"] }` | Target on selected projects |
| `{ "target": "build", "dependencies": true }` | Target on project dependencies |

Target names may contain colons. As in Nx, `"a:b"` names target `b` of
project `a` when `a` is a project with that target, and otherwise target
`a:b` of the same project; `"^a:b"` and `{ "target": "a:b" }` name target
`a:b`.

Object declarations also accept `params: "forward"` and `options: "forward"`;
any other value, or none, does not forward. `params` passes the run's own
arguments; `options` passes the target's options, its configuration's
included, as the dependency's options, overriding its own, lists and
objects included. With both `projects` and `dependencies: true`, `projects`
applies. Other fields are ignored, as in Nx. As in Nx 24, the Nx 15
spellings `projects: "self"` and `projects: "dependencies"` fail.

An object declaration naming a continuous target also accepts `inputs` and
`waitFor`. `inputs: false` keeps the continuous task's inputs from counting
for the dependent; see [Affected selection](../guides/affected.md#selecting-affected-tasks-by-inputs).
`waitFor` is `"started"`, the default, or `"ready"`, which starts the
dependent once the target's [`readyWhen`](#readywhen) check passes. When several
declarations name the same task, `"ready"` wins, and its inputs count unless
every declaration sets `inputs: false`. Both have no effect on a dependency
that is not continuous.

Other missing dependency targets are skipped. Unknown explicit projects
and task cycles fail unless `--nx-ignore-cycles` is used. A shared task
reached with different forwarded arguments or options keeps those it was
first reached with, as in Nx.
See [Running tasks](../guides/running-tasks.md#dependencies-and-configurations).

## `inputs`

An array of [input declarations](inputs-outputs.md). Without it, qk uses
`default` and `^default`. Inputs determine both cache keys and task-level
affected selection.

## `outputs`

An array of output paths or globs with fixed directory prefixes. On a cache
hit these are restored and recorded logs replayed. Explicit outputs also let
dependent keys reflect output content instead of the dependency's entire
key. See [Inputs and outputs](inputs-outputs.md#outputs).

## `cache`

A boolean; absent or `false` means the target does not opt in to result
caching. `true` enables caching when supported by the task's inputs and
outputs. Continuous and readiness tasks run uncached. As in Nx, a target
with both `cache` and `continuous` set to `true` is an error.
See [Local cache](../guides/cache.md).

## `continuous`

A boolean, default `false`. Dependents may start when this task starts,
or, with `waitFor: "ready"`, when its [`readyWhen`](#readywhen) check passes. Continuous tasks do not count towards
`--parallel`; they run until exit or cancellation. A continuous task used
only as a dependency is stopped successfully once its consumers finish.
See [Continuous tasks](../guides/running-tasks.md#continuous-tasks-and-readiness).

## `readyWhen`

How a continuous target tells that it is ready, for tasks depending on it
with `waitFor: "ready"`, as in Nx 24. It is an object with exactly one of:

| Key | Ready when |
| --- | --- |
| `url` | An HTTP or HTTPS `GET` gets a status from 200 to 403. Redirects are not followed, and a 404 at the root path is tried again at `/index.html`. |
| `port` | A TCP connection succeeds, to `127.0.0.1` or `::1` unless `host` names the address. |
| `command` | The command, run through the shell at the workspace root with the environment the target's commands run with, exits with 0. It reads the options they run with, as JSON, from `NX_TASK_OPTIONS`. |
| `logMatches` | Every one of a string or an array of strings has appeared in the output, terminal control sequences left out. |
| `signal` | `true`: the task reports that it is ready, as `@nx/devkit`'s `signalTaskReady` does, through the socket qk names in `NX_READY_SIGNAL_SOCKET`. |

`timeout`, 600000 (ten minutes) by default, is how many milliseconds the check may take, and
`interval`, 250 by default, how long a `url`, `port` or `command` check waits
between tries. Strings may use `{workspaceRoot}`, `{projectRoot}`,
`{projectName}`, `{options.name}`, `{env.NAME}` and `??` fallbacks, read from
the options, arguments and environment the task runs with. `null` turns off
a check set by a target default. See
[Continuous tasks](../guides/running-tasks.md#continuous-tasks-and-readiness).

A `signal` takes no `interval`. The address a signalling task reports is
passed on in `NX_READY_URL` to a task that waited for it, when it is the only
server that task waited for to report one. A task that sets
`NX_USE_V8_SERIALIZER=true` signals in Node's own serialization, which qk
cannot read.

As in Nx 24.0.1, a task resolves its own `readyWhen` once the servers it
waits for are ready, so `{env.NX_READY_URL}` and a `command` check's
environment have the address. Only a signal check reports one: behind a
server with another check, `{env.NX_READY_URL}` has no value. Behind a
signalling server, a `port` read from `NX_READY_URL` gets the address rather
than a `??` fallback, so it is not an integer, and the check is invalid.

This is unrelated to the run-commands `readyWhen` option, which marks the
task itself as ready.

## `parallelism`

A boolean, default `true`. `false` runs the task alone, waiting for all
other tasks, including continuous ones. No other task starts while it runs.
Graphs requiring it to overlap a continuous dependency are rejected.

## `env`

A target-level object of environment variables, for every executor. This is
a qk extension; Nx ignores it. Executor `options.env` and selected
configuration `env` can override it. See
[Environment precedence](environment.md#environment-precedence).

## `qk:env`

The environment the task inherits: an array of entries, each the name of an
environment in nx.json's [`qk:environments`](workspace.md#qkenvironments-and-qkdefaultenv),
whose entries it stands for, or an object with one of these fields:

| Field | Type | What it does |
| --- | --- | --- |
| `inherit` | array of variable names | Passes the variables through and keys their values |
| `pass` | array of variable names | Passes the variables through without keying their values, as for credentials and proxies |
| `set` | object of string values | Sets the variables, keyed as written |
| `bin` | array of command names, or of objects with `name` and `key` | Puts the commands, as found on the `PATH` qk inherited, on the task's `PATH`, keyed by the files they resolve to, or with `"key": "content"` by their content |
| `dirs` | array of paths, with `{projectRoot}` and `{workspaceRoot}` | Adds the directories to the task's `PATH`, after the commands, keyed as written; relative to the workspace root |
| `from` | object with `command` and, optionally, `inputs`, an array of paths | Sets the variables the command prints as JSON, as `nix print-dev-env --json`, `direnv export json` or `mise env --json` print them; see [Declared environments](environment.md#declared-environments) |

A name can contain `*`, which matches any run of characters, as `LC_*`. A
variable both inherited and passed fails the task, as does a command `bin`
names that is not on `PATH`. An environment with `bin` or `dirs` entries
builds `PATH`, so it cannot also inherit, pass or set it. `false` inherits every
variable, as without the field, which overrides nx.json's `qk:defaultEnv`.
See [Declared environments](environment.md#declared-environments).

## `qk:threads`

`true`, or an object containing:

| Field | Type | Default |
| --- | --- | --- |
| `env` | object of string values | Empty; `{threads}` expands to the threads the task is told to start: its share of cores, times `scale`, rounded up |
| `min` | positive integer | `1` |
| `max` | positive integer, at least `min` | Run's core budget |
| `scale` | number above 0, at most 8 | `1`; threads the task is told to start per core it holds, rounded up |

Opted-in tasks receive `QK_THREADS`; `true` sets only that variable.
The share is fixed when the task starts. `min` and `max` bound the cores a
task holds; `scale` only changes how many threads it is told to start on them.
Without this key a task holds one core. The setting and allocated count do not affect cache keys.
Continuous tasks do not take a core share; this setting does not allocate
threads or set these environment variables for them.
`false` is not a supported value; omit the key to disable it.
Other fields are ignored, as Nx ignores keys it does not know, so a
configuration can use fields a newer qk added; `qk doctor` reports them.
See [Sharing cores](../guides/threads.md) for allocation examples.

## `qk:sandbox`

`false` runs the task unsandboxed under `--sandbox`, listed with the reason,
for a task that applies a sandbox of its own, which cannot run inside qk's,
such as an Xcode build running Swift macros. `true`, the default, sandboxes
it. An object sandboxes it too, and names its scratch directories:

| Field | Type | Default |
| --- | --- | --- |
| `scratch` | array of paths, with `{projectRoot}` and `{workspaceRoot}` | Empty; directories emptied before each run of the task, with or without `--sandbox`, which the task may read and write and which nothing keeps |

A scratch directory lies in the workspace, outside `.git`, `.qk` and
`node_modules` in any case, holds no project and neither qk's cache nor its
state, and neither is nor lies under a symbolic link; a task naming another fails, and so does a run in which two
tasks share one or have one inside the other's.
`qk doctor` reports one, any other value, and fields it does not know. See
[Scratch directories and ignored reads](../guides/sandbox.md#scratch-directories-and-ignored-reads).

## `qk:ultracache`

An object: the task is keyed by what it read when a run with `--record` last
recorded it, rather than by its declared file inputs, once it has been
recorded. See [Keying tasks by what they read](../guides/ultracache.md).

| Field | Type | Default |
| --- | --- | --- |
| `mode` | `"on"`, `"record"` or `"off"` | `"on"`; `"record"` records the task and reports what its recordings would have done, but keys it by its declared inputs; `"off"` keys it by its declared inputs, as without the field |
| `ignoreReads` | array of workspace globs | Empty; files left out of recordings, whose changes cannot change the result; the sandbox's audit leaves them out too, and enforce lets the task read them. A glob starting with `^` is matched from the root of each project the task's project depends on, directly or not |

Nx's `"warn"` and `"error"` modes are not supported yet. Other fields are
ignored, as in `qk:threads`, and `qk doctor` reports them. The field does not
change the task's key; `ignoreReads` changes what its recordings key.

## `qk:warm`

An object describing reusable scratch state, or an array of them restored and
saved separately. In an array, at most one entry omits `group`, no group is
named twice and no variable is set by two entries. See
[Several entries](../guides/warm-state.md#several-entries). Each object has:

| Field | Type | Default |
| --- | --- | --- |
| `outputs` | boolean | `false`; restore previous outputs before execution |
| `paths` | array of workspace paths and globs | Empty; `!` excludes; `{warm}` is not allowed here |
| `env` | object of strings | Empty; `{warm}`, `{workspaceRoot}`, `{projectRoot}` expand to absolute directories |
| `maxSize` | bytes or size string | No per-group limit |
| `remote` | boolean | `true`; permit remote exchange when `portable` is `true` or `"path"` |
| `portable` | `true`, `false` or `"path"` | `false`; `true` permits another worktree's save and remote exchange; `"path"` permits remote exchange, restoring a save only when made in a checkout at the same path |
| `mtimes` | `"epoch"` or `"preserve"` | `"epoch"`; `"preserve"` keeps a worktree's own save's modification times |
| `key` | array of workspace paths and `{"env": name}` objects | Empty; a save is restored only where the key matches |
| `restoreKeys` | count | None; how many leading `key` parts a save must match when none matches whole |
| `group` | string | None; share `{warm}` and its saves with every target naming the group; an entry with a group cannot set `outputs`, `paths` or `survive` |
| `survive` | array of targets, `target` or `project:target` | Empty; keep `paths` across these dependencies |
| `save` | `"wait"` or `"background"` | `"wait"`; `"background"` saves after the task has reported |
| `restore` | `"whole"` or `"each-path"` | `"whole"`; `"each-path"` restores each missing path from `paths` and leaves those on disk; see [Restoring paths one by one](../guides/warm-state.md#restoring-paths-one-by-one) |
| `prefetch` | boolean | `false`; fetch the remote save as the run starts, while the task's dependencies run; see [Fetching remote state ahead](../guides/warm-state.md#fetching-remote-state-ahead) |

Other fields are ignored, as in `qk:threads`, and `qk doctor` reports them.
Put it beside `inputs` and `outputs`, rather than in executor `options`.
Environment values retain their other text unchanged; their directory tokens
are absolute even when the task runs from a project subdirectory.
Warm state is restored on misses and uncached runs, saved after success,
and never changes a key or a hit. `--skip-cache` disables it.
See [Warm state](../guides/warm-state.md) for tool configurations, portability checks
and lifecycle details.
