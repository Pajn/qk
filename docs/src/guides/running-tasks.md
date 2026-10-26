# Running tasks

Inspect the normalized project before running it:

```sh
qk show project web
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
- `--nx-bail` (or `NX_BAIL=true`, which `--nx-bail=false` overrides): after a
  task fails, nothing else starts; what is running finishes.
- `--exclude-task-dependencies`: only the requested tasks run.
- `--nx-ignore-cycles` (or `NX_IGNORE_CYCLES=true`): a task dependency cycle
  is broken, as Nx does, by dropping the dependency that closes it, with a
  warning; otherwise it is an error.
- `--graph=<file>` or `--graph=stdout` (`--graph` alone prints too, where Nx
  opens its viewer): writes the project graph and the task graph in the
  shape Nx's `--graph` writes, without running anything. Nx's `taskPlans`,
  its hashing plan, is left out.
- `--verbose` sets `NX_VERBOSE_LOGGING=true` for tasks; without it they see
  `NX_VERBOSE_LOGGING=false`, unless it is already `true`.
- `--output-style` and `--dry-run`, which prints qk's own task graph.
- Any other option, as in Nx, is forwarded to the requested tasks; see
  [Forwarding arguments](#forwarding-arguments).
- Nx's own options (`--runner`, `--batch`, `--skip-sync`, `--cloud`,
  `--dte`, `--agents`, `--tui`, `--tui-auto-exit`) are accepted and have no
  effect.

`--output-style` takes Nx's names, and qk's own `quiet`:

- `dynamic`, `tui` and `dynamic-legacy` show a live panel on stderr: how many
  tasks are done, cached, running, queued and failed, and each running task
  with its time. Successful tasks' output stays hidden; a failed task's
  output is printed above the panel. For `run`, the panel counts the
  requested task's dependencies, prints warnings as they come, and once they
  are done gives way to one line saying how they went and the task's own
  output, streamed under `> nx run <task>`; continuous dependencies stream
  behind their project prefix. Without a terminal, under
  `NX_TASKS_RUNNER_DYNAMIC_OUTPUT=false` or with verbose logging they fall
  back to `static`, as in Nx.
- `quiet` prints nothing while tasks run except a failed task's output,
  under `✖ nx run <task> failed`, with escape sequences removed when the
  output is not a terminal and `FORCE_COLOR` is not set. It is meant for
  agents and scripts.
- `stream` prefixes each non-empty line with the task's project, in Nx's
  colour for it; `stream-without-prefixes` passes output through untouched.
- `static` holds each task's output until it ends and prints it under
  `> nx run <task>`, as Nx does, with the target in quotes when its name has
  a colon and the task's arguments after it, marked `[local cache]` for a
  hit, and in GitHub Actions folds each task into a log group unless
  `NX_SKIP_LOG_GROUPING=true`. As in Nx, the run opens with a banner naming
  what it runs and ends with one saying whether it succeeded, listing failed
  tasks and those not run because of them. For `run`, `static` follows
  `nx run`: the requested task streams under its header as it runs, and its
  dependencies are held, with only the header shown for a cache hit.
- `static-failures-only` is `static`, except that a task that did not fail
  is one line, `✔  nx run <task>`, and the run ends saying how many tasks'
  output was left out. For `run`, the tasks of the requested task's project
  still print in full. With `--verbose`, every task prints in full, as in Nx.
- `summary` prints no task output. The run ends with how many tasks
  succeeded, came from the cache, failed and were skipped, then each failed
  task with the file holding its output, kept in the worktree's state
  directory until `qk reset`, and how to see the logs inline.

Without the option, `NX_DEFAULT_OUTPUT_STYLE` applies when it names a style;
otherwise every command uses `static` in CI; every command uses the live
panel on a terminal; where the panel is turned off, `run` passes output
through, and `run-many` and `affected` use `static` on a terminal and
`quiet` otherwise. These
defaults are qk's; Nx's is `static-failures-only` where it does not use its
terminal UI. `stream` and `stream-without-prefixes` print `qk:` status lines
on stderr; `static` shows each task's status in its header instead, and the
panel and `quiet` collect warnings for the summary. Continuous tasks stream
under the static styles, since their output would otherwise never appear. As in Nx, when no static style was named, as with
the CI default, a long-running task streams too: one that runs with
`--watch`, or whose target is `serve`, `dev`, `start` or ends in `-watch`
or `:watch`. `NX_STREAM_OUTPUT=true` streams every task and
`NX_STREAM_OUTPUT=false` none but continuous ones. As in Nx, output streamed
under a style other than `stream` carries the project prefix only with
`NX_PREFIX_OUTPUT=true`. Colour follows picocolors: off
with `NO_COLOR` or `FORCE_COLOR=0`, on with `FORCE_COLOR`, in CI or on a
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

## Terminals and input

As in Nx, when qk's stdout is a terminal, on Unix, a command whose output qk
captures runs under a pseudo-terminal, so tools that check for a terminal
draw as they would on one. It is the command's controlling terminal, as large
as qk's, and both its stdout and stderr, so the output is recorded and
replayed as one stream on stdout. Commands keep pipes for a target with
`usePty: false`, for a command entry with a `prefix`, for several commands
run side by side, and for every `nx:run-commands` target under
`NX_NATIVE_COMMAND_RUNNER=false`.
Output qk passes straight through already reaches the terminal.

Only the task a run is for reads qk's stdin, when it has a single command
whose output is shown as it runs; other tasks read nothing. Run directly, it
has the terminal itself. Under a pseudo-terminal, it reads input that is not
a terminal, such as a pipe, directly; from a terminal, qk copies its input in
and puts the terminal in raw input mode while the task runs, so each key arrives
as typed and Ctrl-C interrupts the task; `tty: false` keeps the terminal's own
line editing. What the task echoes of the input is part of its output.

## Waiting for memory

Tasks that each fit in memory can exhaust it together, and a machine out of
memory freezes rather than failing one task. So a task starts only once the
machine has the memory it is expected to use free. Free memory is the
machine's, so other runs, worktrees and programs count. What is free, less a
tenth of the machine's memory kept for everything else and what the run's
running tasks are still expected to grow by, must cover what the task is
expected to use. A task that waits leaves its slot to the next task that
fits, and starts regardless once nothing else of the run is running, so a
run cannot stall. The summary names the tasks that waited, and does not
count their wait for memory as a wait for a `--parallel` slot.

A task is expected to use the most memory it used in its recent runs that
executed it, as [run history](history.md) records. A task without such runs
is expected to use the median of the run's other tasks of its target, so a
type-check of a new project is expected to use what the others do. A task of
a target none of which has been measured starts without waiting.

## Dependencies and configurations

The planner expands `dependsOn` before execution: local targets, `^target`
on project dependencies, `project:target`, and objects with `target`,
`projects`, `dependencies`, `params` and `options`. Target names may contain
colons: `a:b` is project `a`'s target `b` when `a` is a project with that
target, and otherwise the task's own target `a:b`. A target may be a glob,
such as `lint-*` or `^build-*`: as in Nx, it stands for every target name in
the workspace it matches. `options: "forward"` passes the target's options,
its configuration's included, to the dependency as its own options, which
they override; as in Nx that includes a run-commands target's own `command`,
so it suits targets of the same executor. `params: "forward"` passes the
run's own arguments, however the dependency is reached. `projects` takes
[project selectors](affected.md#project-selectors). As in Nx 24, `projects: "self"` and
`projects: "dependencies"` fail: leave `projects` out for the task's own
project, or set `dependencies: true`. As in Nx, a project dependency without the
target is looked through: the task's `dependsOn` applies again from that
project, so `^tsc` reaches the nearest dependencies that have `tsc`. Other
missing dependency targets are skipped; unknown explicit projects and task cycles fail.
Shared tasks run once. When different requested configurations reach the same
task, its dependencies include the selections from every request.
Requested configurations propagate to dependencies that define the same name;
otherwise the dependency's default configuration applies. The requested name
continues through that dependency to its dependencies. Without a requested
configuration, each task chooses its own default. `run-many -c` treats the
targets it selects the same way. As in Nx, a shared task reached with
different forwarded arguments or options keeps those it was first reached
with.

## Executors

See [Executors](../reference/executors.md) for supported executors and options.

## Environment

See [Environment variables](../reference/environment.md) for precedence, dotenv
loading and task variables.

## Forwarding arguments

Forward task arguments after `--`, or, as in Nx, give options qk does not
know directly:

```sh
qk run app:build -- --mode=production 'two words'
qk run app:package -c release -- --arch=arm64
qk test app --coverage --max-workers 2
```

The options qk and Nx know, such as `--configuration` or `--nx-bail`, are
read as theirs; everything else, and any positional after the task, reaches
the requested tasks in the order given, before what follows `--`.

A run-commands command, as Nx's, puts the task's arguments in one of three
ways:

- `{args.name}` stands for one value: a `--name=value` or `--name value`
  argument, a flag without a value as `true`, `--no-name` as `false`, a
  repeated flag as its values joined with commas, a name the `args` option
  sets (also in camel case), or another option of the target. The value goes
  in as it is, unquoted, for the shell to read, so quote the placeholder
  where the command needs one word: `echo "{args.message}"`. A missing value
  interpolates as nothing. As in Nx 24, `??` lists fallbacks, and the first
  that is set and not empty applies: another `args.name`, `env.NAME` from the
  task's environment, or a final literal, as in
  `{args.port ?? env.PORT ?? 4200}`. `0` and `false` are values.
- `{args}` stands for everything: the options run-commands does not know, as
  `--name=value`, the arguments, and then the `args` option.
- Without either, the same are appended, unless `forwardAllArgs` is `false`.

A command cannot use both `{args}` and `{args.name}`. Forwarded options and
arguments the shell would split or read are put in double quotes, as Nx
puts them, so `$NAME` in one still expands. Arguments that set a
run-commands option, such as `--cwd=dir`, are not forwarded.
Path tokens such as `{projectRoot}` in options resolve as the workspace
loads; see [`options`](../reference/targets.md#options). Dependencies
receive no arguments unless their dependency object sets
`params: "forward"`.

## Failures and cancellation

All selected tasks are prepared before any command starts, so unsupported
executors and options fail before dependencies run. A failed task skips its
dependents while independent tasks continue. As in Nx, the CLI exits with 1
when any task failed or was skipped, whatever the task's own exit code.
Ctrl-C and, on Unix, SIGTERM and SIGHUP cancel the run, which exits with 128
plus the signal's number, as Nx does: 130, 143 or 129. Cancelled commands
receive SIGTERM on Unix and are killed with their process group if they
have not exited within `NX_PROCESS_KILL_GRACE_PERIOD` milliseconds, five
seconds by default; on Windows they are killed directly. Parallel commands
within a failed task are terminated immediately, and as in Nx the task then
fails with exit code 1; commands run in sequence stop at the first failure.
Under `--nx-bail`, a run that left tasks unstarted exits with 1, where Nx
exits with 130 as if interrupted. Commands must not detach themselves into
separate sessions or launch external services; those processes are outside
the managed group.

As Nx does, a run refuses to run a task that one of the runs it was started
from is running with the same arguments, such as a task whose command runs
`qk run` for itself, and names the chain of tasks.

## Continuous tasks and readiness

Targets with `continuous: true`, such as dev servers and watchers, run until
they exit or are stopped. A task that depends on a continuous task starts as
soon as that task has started, not when it finishes, unless its `dependsOn`
entry sets `waitFor: "ready"`. Continuous tasks do not count towards
`--parallel`. A requested continuous task runs until it exits or the run is
cancelled. A continuous task that only other tasks depend on is stopped, like
a cancelled command, once all of them have finished; that stop counts as
success. A continuous task that exits by itself reports its exit status as
usual. Continuous tasks are never cached. As in Nx, a cacheable task depending
on one is keyed with the continuous task's inputs as its own, unless every
`dependsOn` entry naming it sets `inputs: false`.

With `readyWhen`, a string or an array of strings, a task is ready once every
string has appeared in its output, on stdout or stderr. Its dependents start
then, while its commands keep running, and qk stops it, as a success, once
nothing still to run needs it. A requested task with `readyWhen` is done once
ready, as in Nx, unless it is also continuous. Such a task holds a
`--parallel` slot until it is ready, and a continuous one uses `readyWhen` to
hold its dependents back until then. A task whose commands exit before it is
ready ends with their status. With `commands`, `readyWhen` requires
`parallel`. These tasks are not cached, since their commands outlive any
result.

A continuous target's own [`readyWhen`](../reference/targets.md#readywhen),
an object, is a check for the tasks that depend on it with
`waitFor: "ready"`, as in Nx 24: they start once a url answers, a port
accepts a connection, a command succeeds, text appears in the output or the
task signals that it is ready. A task that waited gets the address of the
one server it waited for that reported one, in `NX_READY_URL`, and its own
check, resolved once those servers are ready, can read it too.
Waiting holds no `--parallel` slot. qk checks only while a task in the run
waits. A check that times out fails the task, and the tasks waiting for it
are skipped. A task that exits before its check passes keeps its own status,
and the tasks waiting for it fail, naming it, as in Nx. An invalid check
fails the task only when something waits for it; otherwise qk warns and runs
it unchecked. A `port` of `0` or `auto` is one the server picks, so there is
nothing to check and the task is ready once started. A request to this
machine skips proxies and accepts a self-signed certificate; other hosts use
the proxy environment and verify certificates.

A target with `parallelism: false` runs alone, as in Nx: it waits until no
other task runs, continuous ones included, and nothing starts while it runs.
Like Nx, qk rejects a graph where such a task depends on a continuous task,
or where a continuous task other tasks depend on has it.

`--dry-run` only plans and prints the task graph: it does not load dotenv,
validate executor support, execute runtime inputs, or run commands.
