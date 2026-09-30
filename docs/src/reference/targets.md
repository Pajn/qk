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

## `executor`

A string naming `nx:run-commands`, `nx:run-script`, or `nx:noop`.
Unsupported names remain inspectable but fail execution preflight.
See [Executors](executors.md) for their options.

## `command`

A shorthand command normalized into `nx:run-commands` options. Prefer an
explicit executor with `options.commands` for multiple commands.

## `options`

An object, empty by default, of executor-specific options. Selected
configuration values merge over these by key, followed by task argument
overrides. Nested objects are replaced rather than recursively merged.

## `configurations`

An object mapping configuration names to option objects, empty by default.
Select with `-c release` or `project:target:release`. `--prod` selects
`production`. Configuration names propagate to dependencies that define
that name; otherwise each dependency uses its default configuration.

## `defaultConfiguration`

An optional string selecting a configuration when none is requested.

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

Object declarations also accept `params: "forward"` and `options: "forward"`;
the default is not to forward. `params` passes task arguments; `options`
passes scalar options as `--name=value` and object fields such as
`--env.NAME=value`. Lists cannot be forwarded this way. Project selectors
also accept `self` and `!self`.

Other missing dependency targets are skipped. Unknown explicit projects
and task cycles fail unless `--nx-ignore-cycles` is used. A shared task
reached with different forwarded arguments is rejected as ambiguous.
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
caching. `true` enables caching when supported by the task's inputs,
outputs and dependency chain. Continuous and readiness tasks run uncached.
See [Local cache](../guides/cache.md).

## `continuous`

A boolean, default `false`. Dependents may start when this task starts,
or when `readyWhen` is satisfied. Continuous tasks do not count towards
`--parallel`; they run until exit or cancellation. A continuous task used
only as a dependency is stopped successfully once its consumers finish.
See [Continuous tasks](../guides/running-tasks.md#continuous-tasks-and-readiness).

## `parallelism`

A boolean, default `true`. `false` runs the task alone, waiting for all
other tasks, including continuous ones. No other task starts while it runs.
Graphs requiring it to overlap a continuous dependency are rejected.

## `env`

A target-level object of environment variables. Executor `options.env`
and selected configuration `env` can override it. See
[Environment precedence](environment.md#environment-precedence).

## `qk:threads`

`true`, or an object containing:

| Field | Type | Default |
| --- | --- | --- |
| `env` | object of string values | Empty; `{threads}` expands to the allocated share |
| `min` | positive integer | `1` |
| `max` | positive integer, at least `min` | Run's core budget |

Opted-in tasks receive `QK_THREADS`; `true` sets only that variable.
The share is fixed when the task starts. Without this key a task holds one
core. The setting and allocated count do not affect cache keys.
`false` is not a supported value; omit the key to disable it.
See [Sharing cores](../guides/threads.md) for allocation examples.

## `qk:warm`

An object describing reusable scratch state:

| Field | Type | Default |
| --- | --- | --- |
| `outputs` | boolean | `false`; restore previous outputs before execution |
| `paths` | array of workspace paths and globs | Empty; `!` excludes; `{warm}` is not allowed here |
| `env` | object of strings | Empty; supports `{warm}`, `{workspaceRoot}`, `{projectRoot}` |
| `maxSize` | bytes or size string | No per-group limit |
| `remote` | boolean | `true`; permit sharing warm state remotely |
| `portable` | boolean | `true`; restore another worktree's or the remote's save |
| `mtimes` | `"epoch"` or `"preserve"` | `"epoch"`; `"preserve"` keeps a worktree's own save's modification times |
| `key` | array of workspace paths and `{"env": name}` objects | Empty; a save is restored only where the key matches |
| `restoreKeys` | count | None; how many leading `key` parts a save must match when none matches whole |
| `group` | string | None; share `{warm}` and its saves with every target naming the group |

Put it beside `inputs` and `outputs`, rather than in executor `options`.
Warm state is restored on misses and uncached runs, saved after success,
and never changes a key or a hit. `--skip-cache` disables it.
See [Warm state](../guides/warm-state.md) for examples and lifecycle details.
