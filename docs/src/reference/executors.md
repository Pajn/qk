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
an array of strings or command objects; `command` wins when both are set. An
empty `commands` succeeds without running a process. Each object can have
`command`, `forwardAllArgs`, `description`, `prefix`, `prefixColor`, `color`,
and `bgColor`. Decorations apply when commands run in parallel.

### `parallel`

A boolean, default `true`, controlling concurrency **within the task**.
`false` runs commands sequentially and stops at the first failure. It is
separate from the run's `--parallel` task limit. With `command`, as in Nx,
it is ignored: the command runs in parallel exactly when `readyWhen` is set.

### `cwd`

A directory relative to the workspace root, which is the default, or an
absolute one. Like every option, it may use `{projectRoot}` and
`{projectName}`, and start with `{workspaceRoot}`, which resolve as the
workspace loads; see [`options`](targets.md#options).

### `env`, `envFile`, and `color`

`env` is an object of environment variables. `envFile` is a dotenv path
relative to the workspace root, loading only variables not already set, or
set but empty, as in Nx 24.
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
`{args.name}`; lists, objects and nulls are not forwarded, and `{args.name}`
writes a list's items joined with commas. Task arguments override values
of the same name. An argument naming an executor option sets that option,
for example `-- --args='--watch'`, `--cwd=dir`, `--no-parallel`, or
`--env.NAME=value`. As in Nx, the tokens `{projectRoot}`, `{projectName}`,
`{project.<field>}` and a leading `{workspaceRoot}` resolve in such a value
for the task's project.

`usePty: false` runs the target's commands with pipes rather than a
pseudo-terminal, and `tty: false` leaves the terminal out of raw mode while
the task a run is for reads it. `streamOutput` and `verbose` are accepted
without effect. See [Terminals and input](../guides/running-tasks.md#terminals-and-input).

## `nx:run-script`

`options.script` names a package script. As in Nx, it is the only option
read: qk runs the script in the project directory, whatever `cwd` says, with
the task's environment, without `options.env` or `envFile`, using the
detected package manager:

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
The task's arguments are each quoted for the shell; Nx joins them as they
are.

## `nx:noop`

Succeeds after its dependencies without launching a process. No executor
options are needed.
