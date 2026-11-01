# Environment variables

## Environment precedence

Environment precedence, highest first: `FORCE_COLOR` from `color: true`,
configuration `env`, `options.env`, the variables Nx sets
for every task, the inherited process environment, the task's dotenv files
in Nx's order: in the
project root and then the workspace root, `.env.<target>.<configuration>`,
`.env.<configuration>`, `.env.<target>`, then `.env.local`, `.local.env` and
`.env`, each also as `.env.<name>.local`, `.<name>.local.env` and
`.<name>.env`, then `envFile`. `color`, `env` and `envFile` are
run-commands options; run-script reads none of them. A target that a plugin
atomized, named in its project's `metadata.targetGroups` with a
`metadata.nonAtomizedTarget`, also reads the files of the target it was split
from, after its own. Each atomized target reads its own files, where Nx gives
every target of the group the files of its first atomized one. The
task variables are `NX_TASK_TARGET_PROJECT`, `NX_TASK_TARGET_TARGET`,
`NX_TASK_TARGET_CONFIGURATION`, `NX_WORKSPACE_ROOT`, `LERNA_PACKAGE_NAME`,
`NX_TUI=false`, `NX_INVOCATION_ANCESTOR_PIDS`, `NX_TERMINAL_CAPTURE_STDERR=true`
for a `test` target, `FORCE_COLOR`, which is `true` unless already set, and
`NX_TASK_HASH`, the task's cache key, which every task is given, as in Nx,
whether or not the run uses the cache, except under `--sandbox` and input
analysis.

## Declared environments

A target with [`qk:env`](targets.md#qkenv), or with nx.json's
[`qk:defaultEnv`](workspace.md#qkenvironments-and-qkdefaultenv) when it
names none, inherits only the variables its environment lets through. Its
environment applies with or without `--sandbox`. What the precedence above
calls the inherited process environment holds then only the variables the
environment inherits or passes, and the ones it sets. The rest of the
task's environment is built as before: its dotenv files, `envFile`, its
`env` settings and the variables Nx sets apply on top, since its
configuration declares them.

`PATH` is either a variable like any other, which the environment passes or
inherits, or built from its `bin` and `dirs` entries. A built `PATH` holds a
directory linking each command `bin` names, found on the `PATH` qk inherited,
and nothing else from that directory, then the directories `dirs` names. The
directory of links is in the user's cache directory, under
`~/Library/Caches/qk/bin` on macOS and `$XDG_CACHE_HOME/qk/bin` or
`~/.cache/qk/bin` elsewhere, named by the commands it links, so tasks
linking the same commands share one. On Windows, where making a link takes a
privilege a user may not hold, `PATH` holds the directory of each command
instead. Either way, qk adds the workspace's `node_modules/.bin` directories
and Node's directory to it, as for every task.

A `from` entry runs its command in the workspace root, with every variable
qk inherited, and sets the variables it prints as JSON: either Nix's
`{"variables": {NAME: {"type": …, "value": …}}}`, of which the exported
variables are taken, less those `nix develop` also leaves as they are, such
as `HOME` and `TMPDIR`, or a flat `{NAME: value}`, in which `null` leaves a
variable unset. What it sets takes the place of an inherited variable, and
the environment's `set` entries take the place of what it sets. Its `PATH`
ends a `PATH` built from `bin` and `dirs` entries, or is the task's `PATH`
without them; the environment cannot also inherit or pass `PATH` then. The
command runs once per run for every task sharing it. With `inputs`, files
relative to the workspace root, such as `flake.nix` and `flake.lock`, its
output is kept in the user's cache directory, readable by its owner alone,
until they change, and the task's key holds the command and their content
rather than the output, which can hold paths that differ from machine to
machine. Without them, the key
holds a digest of each value it printed, which changes with the value without
keeping it, as it can be a credential. A command that fails, or prints something else, fails
the task.

The task's key holds its environment's entries, with named environments
replaced by theirs, so a change to an environment changes the key of every
task running in it, the values of the variables it inherits, and the file
each command `bin` names resolves to, whatever links lead to it. A command
found somewhere else changes the key; the rest of the inherited `PATH`, and
where the links are, do not. A command written as
`{"name": "clang", "key": "content"}` is keyed by its content instead: the
same command installed elsewhere keys alike, and one replaced in place, as
some system toolchains update, changes the key. Its digest is kept in the
user's cache directory until the file's size or modification time changes.

A version manager's shim, as mise and asdf install, resolves to the version
manager itself, whichever version it starts, so neither its path nor its
content changes with that version. Name such a tool's directory with `dirs`,
or set the version manager's own environment with a `from` entry, such as
`mise env --json`, rather than naming its shim in `bin`. The values
of the variables it passes are not keyed. Without a declared environment,
keys are as before.

A process reads its environment from its own memory, so the sandbox's audit
cannot see which variables a task reads: a variable a task needs and its
environment leaves out shows as the task failing. When a task with a declared
environment fails, qk names the inherited variables its environment left out
that its dotenv files and settings do not set again, by name only, as values
can be secrets.

## Dotenv files

Dotenv files are read as Nx reads them, with npm's `dotenv` and
`dotenv-expand`. A task's files are read together: the first to set a
variable wins, a variable the environment already holds keeps its value
unless it is empty, and then every value is expanded once, across all the
files: `$NAME`, `${NAME}`, `${NAME:-default}`, `${NAME-default}`,
`${NAME:+alternate}` and `${NAME+alternate}` read the environment, then the
values the files set before. A reference preceded by `\` is left as `$`, and
a value that refers back to itself expands to its default. Values in single
quotes expand too, as dotenv-expand does not know about quotes. Lines are
read as dotenv reads them: `export` is allowed, `KEY: value` works, quoted
values may span lines, `\n` and `\r` are line breaks inside double quotes,
an unquoted value ends at `#`, and a line dotenv cannot read is skipped
rather than failing the run. `envFile` is read the same way, and so fills a
variable set but empty, as in Nx 24.

As Nx does at startup, qk loads the workspace's root `.local.env`,
`.env.local` and `.env`, in that order of precedence, into its own
environment before it reads its command line, so the run controls below can
be set there; remote cache credentials arrive the same way. For each task,
a variable whose value is one of those files' is then removed again, so the
task's own files decide it, as in Nx. `NX_LOAD_DOT_ENV_FILES=false` turns
every dotenv file off, `envFile` included, as in Nx; qk then removes nothing,
where Nx still removes a shell variable whose value a root file repeats. For `envFile`, the
task's environment decides, so a task's dotenv file can turn it off.

A task does not see `NX_SET_CLI`, `NX_ORIGINAL_FORCE_COLOR`,
`NX_TERMINAL_OUTPUT_PATH`, `NX_STREAM_OUTPUT` or `NX_PREFIX_OUTPUT` from an
enclosing run, as in Nx. It sees `QK_TASK_INVOCATIONS`, the tasks it runs
inside, which a run it starts reads to refuse running one of them again.
dotenv-expand's use of JavaScript's `String.replace`, which reads `$&` and
similar patterns in a value it substitutes, is not reproduced.

## Run controls

| Variable | Behavior |
| --- | --- |
| `NX_DEFAULT_PROJECT` | Fallback project for shorthand commands |
| `NX_WORKSPACE_ROOT_PATH` | Workspace root, unless `--workspace` names one |
| `NX_INFER_ALL_PACKAGE_JSONS=true` | Every package.json is a project, in a workspace without a root package.json |
| `NX_BASE`, `NX_HEAD` | Affected comparison revisions; CLI overrides them |
| `NX_LEGACY_AFFECTED` | `true` makes affected selection select projects, `false` tasks; overrides nx.json `affected.legacy`, and `--granularity` overrides it |
| `NX_PARALLEL` | Task concurrency, overridden by `--parallel` |
| `QK_CORES` | Core budget, overridden by `--cores` |
| `QK_COLD=true` | Run each task from scratch and save what it gives, as `--cold` does; ignored by a run that skips the cache |
| `QK_RECORD=true` | Record tasks with `qk:ultracache` that execute, as `--record` does |
| `QK_RECORD=audit` | Record them and audit what they did beyond their declarations, as `--record --sandbox` does |
| `QK_SANDBOX_OVERLAY` | `0` holds Linux [enforce](../guides/sandbox.md#linux) by Landlock alone, rather than running each task over an overlay that keeps what it writes outside its outputs from the workspace |
| `QK_RECORDER` | How Linux [records](../guides/ultracache.md#recorders) tasks: `auto` (the default) uses eBPF where it loads and strace otherwise; `ebpf` or `strace` uses only that one |
| `NX_BAIL=true` | Stop starting tasks after a failure, unless `--nx-bail=false` |
| `NX_IGNORE_CYCLES=true` | Break dependency cycles with a warning |
| `NX_DEFAULT_OUTPUT_STYLE` | Default output style, overridden by `--output-style`; an unknown name is ignored, as in Nx |
| `NX_TASKS_RUNNER_DYNAMIC_OUTPUT=false` | No live panel; `static` instead |
| `NX_STREAM_OUTPUT` | `true` streams every task's output rather than holding it; `false` streams none but continuous tasks |
| `NX_PREFIX_OUTPUT=true` | Under the static styles, streamed output carries the project prefix |
| `NX_PROCESS_KILL_GRACE_PERIOD` | Milliseconds a cancelled or stopped command has to exit after SIGTERM before it is killed; 5000 by default |
| `NX_LOAD_DOT_ENV_FILES=false` | Load no dotenv file |
| `NX_RUN_REPORT` | JSON report path, overridden by `--report` |
| `NX_VERBOSE_LOGGING` | Task verbosity; tasks see `true` or `false`, and `--verbose` or `--verbose=false` sets it; `true` also turns the live panel off |
| `NX_SKIP_LOG_GROUPING=true` | Disable static output groups in GitHub Actions |
| `NO_COLOR`, `FORCE_COLOR` | Output color controls; `FORCE_COLOR=0` or `false` keeps qk's own output plain, and tasks receive it as set |
| `NX_NATIVE_COMMAND_RUNNER=false` | Run `nx:run-commands` targets with pipes rather than a pseudo-terminal |
| `CI` | CI output and remote cache mode selection; as Nx's `isCI`, any value but `false` (`0` included), or a CI provider's variable such as `GITHUB_ACTIONS=true`, `GITLAB_CI`, `BUILDKITE=true` or `JENKINS_URL` |

## Cache controls

| Variable | Behavior |
| --- | --- |
| `NX_SKIP_NX_CACHE=true`, `NX_DISABLE_NX_CACHE=true` | Bypass cache reads, writes and warm state |
| `NX_SKIP_REMOTE_CACHE=true`, `NX_DISABLE_REMOTE_CACHE=true` | Local cache only |
| `NX_CACHE_DIRECTORY` | Override `nx.json`'s `cacheDirectory` |
| `NX_MAX_CACHE_SIZE` | Override `nx.json`'s `maxCacheSize` |
| `NX_POWERPACK_CACHE_MODE` | Override the remote store's `localMode`/`ciMode` |
| `QK_REMOTE_UPLOAD_MODE` | Override the remote store's `uploadMode`: `wait` or `background` |
| `QK_REMOTE_BLOB_THRESHOLD` | The compressed size from which an output is stored apart, such as `256KB`, or `off`; 1 MiB by default |
| `QK_REMOTE_PREFETCH=off` | Do not fetch entries ahead of the tasks that need them |
| `QK_REMOTE_EXPECTATIONS` | What a run does with the [task expectations](../guides/remote-cache.md) the remote store holds: `read-write` (the default, as the store's mode allows), `read` or `off` |
| `QK_STORE_SERVER_TOKEN` | The [store server](../guides/remote-cache.md#store-server)'s token |
| `QK_STORE_SERVER_URL` | Override `qk:storeServer.url` |
| `QK_PROFILE_CACHE=1` | Enable [cache phase timings](cli.md#profiling-cache-operations), as `--profile` does |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` | Remote credentials |
| `GITHUB_HEAD_REF`, `GITHUB_REF_NAME` | CI branch for remote warm state |

See [Workspace configuration](workspace.md) for defaults and
[Remote caching](../guides/remote-cache.md) for failure behavior.

## Variables supplied to tasks

`NX_TASK_TARGET_PROJECT`, `NX_TASK_TARGET_TARGET`,
`NX_TASK_TARGET_CONFIGURATION`, `NX_WORKSPACE_ROOT`, `LERNA_PACKAGE_NAME`,
`NX_TUI=false`, and `FORCE_COLOR` describe the running task.
`NX_TASK_HASH` holds its cache key. `NX_VERBOSE_LOGGING` is `true` or
`false`, `NX_INVOCATION_ANCESTOR_PIDS` lists the qk and Nx processes it runs
inside, outermost first, and a `test` target sees
`NX_TERMINAL_CAPTURE_STDERR=true`, as in Nx.
Targets using `qk:threads` additionally receive `QK_THREADS` and their
configured thread variables. Targets using `qk:warm` receive their expanded
warm environment when warm state is enabled. A task an affected profile with
reachability narrowed to some of its cases receives `QK_AFFECTED_CASES`, the
absolute path of a file naming those cases one per line, each relative to the
workspace root; see
[Import reachability](../guides/affected-reachability.md#running-only-the-selected-cases).
