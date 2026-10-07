# Running tasks

Inspect the normalized project before running it:

```sh
qk show project web --json
qk run web:build --dry-run
qk run web:build
qk run-many -t build,test -p 'web,tag:library' --parallel 4
```

The dry run exposes dependency expansion without starting a process.
Use `qk --help` or `qk <command> --help` for command details.

## Selecting tasks and run options

`run` requires an existing target; `run-many` skips projects without the
requested targets and errors when nothing matches. `run`, `run-many` and
`affected` take Nx's run options:

- `-c/--configuration`, and `--prod` for `-c production`.
- `--parallel`: a number, a percentage of the cores such as `50%`, or `false`
  for one task at a time. Without it, `NX_PARALLEL`, then nx.json
  `parallel`, then 3; `--parallel` alone means `NX_PARALLEL`, else 3, as in
  Nx.
- `--skip-nx-cache` (`--skip-cache`, `--disable-nx-cache`, or
  `NX_SKIP_NX_CACHE=true`), and `--skip-remote-cache`
  (`--disable-remote-cache`, `NX_SKIP_REMOTE_CACHE=true` or
  `NX_DISABLE_REMOTE_CACHE=true`) to keep to the local cache. Tasks see
  `NX_SKIP_NX_CACHE=true` when the cache is skipped.
- `--nx-bail` (or `NX_BAIL=true`): after a task fails, nothing else starts;
  what is running finishes.
- `--exclude-task-dependencies`: only the requested tasks run.
- `--nx-ignore-cycles` (or `NX_IGNORE_CYCLES=true`): a task dependency cycle
  is broken, as Nx does, by dropping the dependency that closes it, with a
  warning; otherwise it is an error.
- `--graph=<file>` or `--graph=stdout` (`--graph` alone prints too, where Nx
  opens its viewer): writes the project graph and the task graph in the
  shape Nx's `--graph` writes, without running anything. Nx's `taskPlans`,
  its hashing plan, is left out.
- `--verbose` sets `NX_VERBOSE_LOGGING=true` for tasks.
- `--output-style` and `--dry-run`, which prints qk's own task graph.
- Nx's own options (`--runner`, `--batch`, `--skip-sync`, `--cloud`,
  `--dte`, `--agents`, `--tui`, `--tui-auto-exit`) are accepted and have no
  effect.

`--output-style` takes Nx's names, and qk's own `quiet`:

- `dynamic`, `tui` and `dynamic-legacy` show a live panel on stderr: how many
  tasks are done, cached, running, queued and failed, and each running task
  with its time. Successful tasks' output stays hidden; a failed task's
  output is printed above the panel. Without a terminal they fall back to
  `static`, as in Nx.
- `quiet` prints nothing while tasks run except a failed task's output,
  under `✖ qk run <task> failed`, with escape sequences removed when the
  output is not a terminal and `FORCE_COLOR` is not set. It is meant for
  agents and scripts.
- `stream` prefixes each non-empty line with the task's project, in Nx's
  colour for it; `stream-without-prefixes` passes output through untouched.
- `static` holds each task's output until it ends and prints it under
  `> qk run <task>`, marked `[local cache]` for a hit, and in GitHub Actions
  folds each task into a log group unless `NX_SKIP_LOG_GROUPING=true`. As
  in Nx, the run opens with a banner naming what it runs and ends with one
  saying whether it succeeded, listing failed tasks and those not run
  because of them. For `run`, `static` follows `nx run`: the requested task
  streams under its header as it runs, and its dependencies are held, with
  only the header shown for a cache hit.

Without the option, `NX_DEFAULT_OUTPUT_STYLE` applies; otherwise every
command uses `static` in CI; `run` passes output through, and `run-many` and
`affected` use the live panel on a terminal and `quiet` otherwise. `stream`
and `stream-without-prefixes` print `qk:` status lines on stderr; `static`
shows each task's status in its header instead, and the panel and `quiet`
collect warnings for the summary. Continuous tasks stream with prefixes under `static` and
`stream`, since their output would otherwise never appear. Colour follows
picocolors: off with `NO_COLOR`, on with `FORCE_COLOR`, in CI or on a
terminal.

Runs of several tasks, and any run under `static`, the panel or `quiet`,
end with a summary on stderr: how many tasks succeeded and came from cache, or which
failed and which were skipped because of them; the critical path with its
three longest tasks, in the order they ran; the most memory the tasks used
together and the tasks that used the most; and whether `--parallel` held the
run back. For that, qk records how long each task waited for a free slot
after its dependencies finished, and samples the machine's CPU use through
the run. When tasks on the critical path waited while the machine had
capacity to spare, it says how much sooner a higher `--parallel` could
finish; when the machine was busy while they waited, it says more
parallelism would not help.

Of the tasks ready to start, those with the longest expected path to the end
of the run start first: how long the task took in its recent runs that
executed it, rather than restoring it from the cache, and the longest chain
of tasks depending on it. A task without such runs is expected to take no
time, so a workspace's first run starts tasks in the order of their ids.

## Waiting for memory

Tasks that each fit in memory can exhaust it together, and a machine out of
memory freezes rather than failing one task. So a task starts only once the
machine has the memory it is expected to use free. Free memory is the
machine's, so other runs, worktrees and programs count. What is free, less a
tenth of the machine's memory kept for everything else and what the run's
running tasks are still expected to grow by, must cover what the task is
expected to use. A task that waits leaves its slot to the next task that
fits, and starts regardless once nothing else of the run is running, so a
run cannot stall. The summary names the tasks that waited.

A task is expected to use the most memory it used in its recent runs that
executed it, as [run history](history.md) records. A task without such runs
is expected to use the median of the run's other tasks of its target, so a
type-check of a new project is expected to use what the others do. A task of
a target none of which has been measured starts without waiting.

## Dependencies and configurations

The planner expands `dependsOn` before execution: local targets, `^target`
on project dependencies, `project:target`, and objects with `target`,
`projects`, `dependencies`, `params` and `options`. A target may be a glob,
such as `lint-*` or `^build-*`: as in Nx, it stands for every target name in
the workspace it matches. `options: "forward"` passes the task's options,
its configuration's included, to the dependency as overrides, as `--name=value`
arguments (`--env.NAME=value` for an object's fields; lists cannot be passed
this way), before any arguments `params: "forward"` passes. As in Nx that
includes a run-commands target's own `command`, so it suits targets of the
same executor. Project selectors accept names,
globs, tags, `self` and `!self`. As in Nx, a project dependency without the
target is looked through: the task's `dependsOn` applies again from that
project, so `^tsc` reaches the nearest dependencies that have `tsc`. Other
missing dependency targets are skipped; unknown explicit projects and task cycles fail.
Shared tasks run once. When different requested configurations reach the same
task, its dependencies include the selections from every request.
Requested configurations propagate to dependencies that define the same name;
otherwise the dependency's default configuration applies. The requested name
continues through that dependency to its dependencies. Without a requested
configuration, each task chooses its own default. `run-many -c` treats the
targets it selects the same way. A shared task
reached with different forwarded arguments is rejected as ambiguous.

## Executors

See [Executors](../reference/executors.md) for supported executors and options.

## Environment

See [Environment variables](../reference/environment.md) for precedence, dotenv
loading and task variables.

## Forwarding arguments

Forward task arguments after `--`:

```sh
qk run app:build -- --mode=production 'two words'
qk run app:package -c release -- --arch=arm64
```

Commands support `{projectRoot}`, `{workspaceRoot}`, `{projectName}`,
`{args}` (all forwarded arguments, with the `args` option last, as in Nx),
and `{args.name}` (a `--name=value` or `--name value` argument; a flag
without a value becomes `true`, and `--no-name` sets `name` to `false`). A
command cannot use both `{args}` and `{args.name}`. Place argument
tokens outside quotes: qk quotes their values as shell data. A missing named
argument interpolates as nothing, as in Nx; quoted argument placeholders are
errors. On Windows, forwarded
values containing quotes, `%`, `!`, `^` or newlines are currently rejected;
use target environment variables for those values. Workspace/project path
tokens are substituted as written, so quote path tokens where your command
requires it.

Without argument tokens, arguments are appended by default; set
`forwardAllArgs: false` to disable that. With argument tokens, only the
explicit substitutions are used. Dependencies receive no arguments unless
their dependency object sets `params: "forward"`.

## Failures and cancellation

All selected tasks are prepared before any command starts, so unsupported
executors and options fail before dependencies run. A failed task skips its
dependents while independent tasks continue. The CLI returns the first
observed failing task's exit code. Ctrl-C and, on Unix, SIGTERM cancel the
run and return `130`. Cancelled commands receive SIGTERM on Unix and are
killed with their process group if they have not exited within five seconds;
on Windows they are killed directly. Parallel commands within a failed task
are terminated immediately, and as in Nx the task then fails with exit code
1; commands run in sequence stop at the first failure and keep its code. Commands must
not detach themselves into separate sessions or launch external services;
those processes are outside the managed group.

## Continuous tasks and readiness

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

With `readyWhen`, a string or an array of strings, a task is ready once every
string has appeared in its output, on stdout or stderr. Its dependents start
then, while its commands keep running, and qk stops it, as a success, once
nothing still to run needs it. A requested task with `readyWhen` is done once
ready, as in Nx, unless it is also continuous. Such a task holds a
`--parallel` slot until it is ready, and a continuous one uses `readyWhen` to
hold its dependents back until then. A task whose commands exit before it is
ready ends with their status. With `commands`, `readyWhen` requires
`parallel`. These tasks are not cached, since their commands outlive any
result. Unlike Nx, which counts text on stderr as a failure, either stream
makes a task ready.

A target with `parallelism: false` runs alone, as in Nx: it waits until no
other task runs, continuous ones included, and nothing starts while it runs.
Like Nx, qk rejects a graph where such a task depends on a continuous task,
or where a continuous task other tasks depend on has it.

`--dry-run` only plans and prints the task graph: it does not load dotenv,
validate executor support, execute runtime inputs, or run commands.
