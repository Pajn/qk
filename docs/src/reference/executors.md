# Executors

qk executes commands through `/bin/sh -c` on Unix and `cmd.exe /D /S /C`
on Windows. Task `PATH` starts with `node_modules/.bin` directories from cwd
through every ancestor to the filesystem root, then the selected Node runtime's
directory, then the task's environment `PATH`. Node is selected from qk's inherited
`PATH` in the workspace root before task environment or cwd overrides, as in Nx.
Version-manager proxies are probed when a command first starts, with a two-second limit.
Cache hits do not probe Node. A failed probe of the first executable Node match
does not promote a later installation from the inherited `PATH`; executable
lookup continues through the task's `PATH` without adding a runtime directory.
Unsupported executors fail before tasks start.

## `nx:run-commands`

```jsonc
{
  "executor": "nx:run-commands",
  "options": {
    "commands": ["echo first", "echo second"],
    "parallel": false,
    "cwd": "{projectRoot}"
  }
}
```

### `command` and `commands`

Use `command` for a string (or an array joined with spaces) or `commands` for
an array of strings or command objects. An empty `commands` succeeds without
running a process. Each object can have `command`, `forwardAllArgs`,
`description`, `prefix`, `prefixColor`, `color`, and `bgColor`. Decorations
apply when commands run in parallel.

### `parallel`

A boolean, default `true`, controlling concurrency **within the task**.
`false` runs commands sequentially and stops at the first failure. It is
separate from the run's `--parallel` task limit.

### `cwd`

A directory relative to the workspace root, which is the default.
Supports workspace/project path tokens such as `{projectRoot}`.

### `env`, `envFile`, and `color`

`env` is an object of environment variables. `envFile` is a dotenv path
relative to the workspace root, loading only variables not already set.
`color: true` sets `FORCE_COLOR=true`. `NX_LOAD_DOT_ENV_FILES=false` disables
all dotenv loading, including `envFile`. See [Environment](environment.md).

### `args` and `forwardAllArgs`

`args` supplies extra arguments as shell text or an array joined with spaces.
They are forwarded after other options and before the task's own arguments.
`forwardAllArgs` defaults to `true`. Argument placeholders select which
values to interpolate. See [Forwarding arguments](../guides/running-tasks.md#forwarding-arguments).

### `readyWhen`

A string or array of strings. The task is ready once all strings appear on
stdout or stderr. With `commands`, it requires `parallel: true`.
It keeps running while dependents need it, and is not cached.
See [Readiness](../guides/running-tasks.md#continuous-tasks-and-readiness).

### Other options and task overrides

Other scalar options are forwarded as `--name=value` and exposed as
`{args.name}`; object values are ignored. Task arguments override values
of the same name. An argument naming an executor option sets that option,
for example `-- --args='--watch'`, `--cwd=dir`, `--no-parallel`, or
`--env.NAME=value`.

`tty`, `usePty`, `streamOutput`, and `verbose` are accepted without effect.
qk does not create a pseudo-terminal. A single requested task with a single
command and direct output can inherit stdin and the terminal foreground;
captured commands see pipes. See [Running tasks](../guides/running-tasks.md).

## `nx:run-script`

`options.script` names a package script. qk runs it in the project directory
using the detected package manager:

| Manager | Invocation |
| --- | --- |
| npm | `npm run <script> -- <args>` |
| pnpm | `pnpm run <script> <args>` |
| yarn | `yarn <script> <args>` |
| bun | `bun run <script> -- <args>` |

Detection order: `nx.json`'s `cli.packageManager`, then the lockfile
(`bun.lockb` or `bun.lock`, `yarn.lock`, `pnpm-lock.yaml`, `package-lock.json`),
then root `packageManager` or `pnpm-workspace.yaml`, then the invoking
manager's `npm_config_user_agent`, else npm. Nx does not use the root
`packageManager`/workspace-file fallback.
Missing scripts fail in execution preflight after explicit overrides apply.

## `nx:noop`

Succeeds after its dependencies without launching a process. No executor
options are needed.
