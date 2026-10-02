# CLI reference


| Command | Result |
| --- | --- |
| `qk inputs analyze <task> [--report <path>] [--previous <path>] [--suggestions <path>]` | Execute tasks without caching, record file accesses and review input coverage; macOS or Linux with strace. See [Recording task inputs](../guides/input-analysis.md) |
| `qk show projects [--json]` | Project names in Nx graph order, one per line or as a compact JSON array |
| `qk show projects -p 'web,tag:library' --exclude 'experimental-*'` | Union of matching names, globs and tags, minus exclusions |
| `qk show projects --with-target e2e --type app [--sep ,]` | Projects with one of the targets, of a type (`app`, `lib` or `e2e`, as Nx types them), joined by a separator |
| `qk show project <name> [--json]` | Normalized project configuration as JSON |
| `qk graph --file <path>` | Workspace project graph in a `{ "graph": { "nodes": ..., "dependencies": ... } }` envelope |
| `qk graph` or `qk graph --file -` | The same graph on stdout |
| `qk graph --external` | The graph with the packages the pnpm lockfile installs as `externalNodes` |
| `qk graph --focus web --exclude 'tag:e2e'` | Only `web`, what it depends on and what depends on it, minus exclusions |
| `qk run <project>:<target>[:<configuration>]` | Execute a task and its dependencies |
| `qk <target> [project]`, `qk <project>:<target>` | Nx-style shorthand for `qk run` |
| `qk run-many -t build,test -p 'web,core' --parallel 4` | Execute matching targets with bounded task concurrency |
| `qk exec -p web -- <command>` | Run a command in projects, or as a package script's target, as `nx exec` does |
| `qk run web:build --dry-run` | Prepare every planned task without running it, and print the task graph as JSON |
| `qk show projects --affected [--base <rev>] [--head <rev>]` | Projects affected by the changes, in graph order |
| `qk affected -t build,test [--base <rev>] [--head <rev>]` | Execute targets on the affected projects |
| `qk show affected [project] [--json]` | Why projects are affected, or why one project is |
| `qk affected -t build --granularity task` | Execute only the tasks whose inputs changed |
| `qk show tasks -t build,test --affected [--json]` | The affected tasks and why |
| `qk show runs`, `qk show run [id]` | Recent runs; one run's tasks, cache results and critical path |
| `qk show flaky [task]` | Mixed successful/failed executions for identical declared inputs |
| `qk show log <run> <task>` | Replay retained stdout/stderr from an actual cacheable execution |
| `qk show task <project:target>` | A task's recent runs and why its cache key changed |
| `qk cache path` | Print the local cache directory without creating it |
| `qk cache prune [--max-size 1GB]` | Evict least recently used entries until the cache fits |
| `qk warm suggest <project:target>` | Run a task in the sandbox and list directories it wrote outside its outputs, as candidate warm paths (macOS) |
| `qk reset [--only-cache \| --only-workspace-data]` | Remove the cache and this worktree's state, as `nx reset` does |

As in Nx, `qk build web` and `qk web:build` mean `qk run web:build`, and take the
same options. Without a project, `qk build` and `qk run build` use the project
whose root most specifically contains the current directory, as Nx does:
in the root project, `NX_DEFAULT_PROJECT` picks another, and where no
project contains the directory, `NX_DEFAULT_PROJECT`, then nx.json
`cli.defaultProjectName`, then `defaultProject`. As in Nx, `--base`, `--head`,
`--files`, `--uncommitted` and `--untracked` imply `--affected` for
`show projects`. qk's own
subcommands, and Nx commands qk does not implement such as `format` and
`release`, are never read as targets; use `qk run` for a target
with one of those names.

As in `nx graph`, `--focus` keeps the project and every project connected to
it through dependencies in either direction, and `--exclude` takes names, globs
and tags. The kept projects' edges are unchanged, so they can name projects
left out. `--print` and `--file stdout` write to stdout. qk has no browser
view, so `nx graph`'s `--affected`, `--view` and server options do not apply;
for a task graph, use `qk run-many -t <targets> --graph`.

`--workspace <path>` works before or after the subcommand. Graph output paths
are relative to the invocation directory; their parent directories must
already exist. JSON output has no progress messages mixed into stdout.
Errors go to stderr and return a nonzero exit code. Unknown commands and
flags fail explicitly.


## `exec`

`qk exec -- <command>` runs a command as `nx exec` does. Each argument is
wrapped in double quotes and the result runs in the shell tasks use, so
`$NAME` expands; `NX_PROJECT_NAME` and `NX_PROJECT_ROOT_PATH` name the project.

- Inside a task, where `NX_TASK_TARGET_PROJECT` is set, the command runs in
  the current directory for that project.
- From a package script, where the package manager sets
  `npm_lifecycle_event`, the current directory's project runs the script's
  target instead, with the run options and any arguments beyond the
  script's own. That task runs the script again, which then takes the first
  case, so a script such as `"build": "qk exec -- tsc"` is cached.
- Otherwise it runs in each project `-p` and `--exclude` select, all by
  default, and in every project they depend on, dependencies first. The
  first failure stops it. `--exclude-task-dependencies` runs only the
  selected projects.

A failing command exits with `1`, as in Nx. Only the package-script case
runs a task, so the others refuse `--sandbox`, `--dry-run` and `--graph`
rather than run the command without them.

## Shared run options

These options apply to `run`, `run-many` and `affected`:

| Option | Behavior / default |
| --- | --- |
| `-c`, `--configuration <name>` | Select named options; otherwise `defaultConfiguration` |
| `--prod` | Select `production` |
| `--parallel [value]` | Number, percentage, or `false`; `NX_PARALLEL`, then workspace `parallel`, then `3` |
| `--cores <count>` | Positive core budget; `QK_CORES`, then `qk:cores`, then available cores |
| `--skip-cache` | Bypass all result caching and warm state |
| `--skip-remote-cache` | Keep local caching only |
| `--nx-bail` | Stop starting tasks after the first failure |
| `--nx-ignore-cycles` | Drop cycle-closing dependency edges with a warning |
| `--exclude-task-dependencies` | Run only requested tasks |
| `--graph[=<file>]` | Write Nx-shaped project/task graph without execution; default stdout |
| `--dry-run` | Print qk's task graph; no dotenv, runtime inputs or executor preflight |
| `--output-style <style>` | Select output behavior; see [Running tasks](../guides/running-tasks.md) |
| `--verbose` | Set `NX_VERBOSE_LOGGING=true` for tasks |
| `--sandbox[=audit\|enforce]` | Report (`audit`, macOS only) or refuse (`enforce`, macOS and Linux) what tasks read and write in the workspace beyond their declarations; skips the cache. See [Checking inputs in a sandbox](../guides/sandbox.md) |
| `--sandbox-report <path>` | Write every sandbox finding as JSON |
| `--report <path>` | Save JSON run report; defaults to `NX_RUN_REPORT` when set |
| `-- <args>` | Forward arguments to requested tasks |

`--parallel` alone uses `NX_PARALLEL`, otherwise `3`. Percentages are
rounded down with a minimum of one task. `--skip-nx-cache` and
`--disable-nx-cache` alias `--skip-cache`; `--disable-remote-cache` aliases
`--skip-remote-cache`. The corresponding environment flags are documented in
[Environment variables](environment.md).

## Change selection

`affected`, `show affected`, `show tasks`, and `show projects` accept:

| Option | Behavior |
| --- | --- |
| `--base <revision>` | Base: `NX_BASE`, then `defaultBase`, then `main` |
| `--head <revision>` | Head: `NX_HEAD`, otherwise working tree |
| `--files <paths>` | Replace Git comparison with named workspace-relative paths |
| `--stdin` | Read changed workspace-relative paths from stdin, one per line; an empty list means no changes |
| `--uncommitted` | Only uncommitted changes |
| `--untracked` | Only untracked files |

`show projects` change options imply `--affected`. `--stdin` preserves spaces
in paths and conflicts with `--files`, `--uncommitted` and `--untracked`. For `show tasks`, use
`--affected` to filter the planned tasks. `affected --granularity task`
selects by task inputs; `project` is the default.
See [Affected selection](../guides/affected.md) for merge-base behavior.

## Selection and inspection

`run-many`, `affected` and `show tasks` accept `-t/--targets`,
`-p/--projects` and `--exclude`; lists accept commas, spaces or repeated flags.
`show projects` accepts `-p/--projects`, `--exclude`, `-t/--with-target`,
`--type app|lib|e2e`, and `--sep`. `--sep` conflicts with `--json`.
Quote name/tag globs so the shell does not expand them.

`show runs --limit` defaults to `20`, and `show task <id> --limit` to `10`.
`show run` without an ID uses the most recent run. All support `--json`.
`show flaky [task] --limit` defaults to 20 task/key groups and supports `--json`.
`show log <run> <task>` replays output to its original stdout/stderr streams.
See [Run history](../guides/history.md) for retention limits and interpretation.

## Cache administration

`cache path` prints the location without creating it. `cache prune` uses
`--max-size`, else `NX_MAX_CACHE_SIZE`, else `maxCacheSize`, else the default
disk fraction. See [Storage limits](../guides/remote-cache.md).

`reset`, or its alias `clear-cache`, removes the local cache and this
worktree's state: file digests, output records and warm directories. The
cache is shared by the repository's worktrees, so resetting clears it for
all of them. Run history and the remote cache are kept. `--only-cache` and
`--only-workspace-data` (or `--onlyCache` and `--onlyWorkspaceData`) limit it
to one of the two; `--only-daemon` and `--only-cloud` are accepted without
effect.

## Accepted Nx options

`--runner`, `--batch`, `--skip-sync`, `--cloud`, `--no-cloud`, `--dte`,
`--no-dte`, `--agents`, `--tui`, `--no-tui`, and `--tui-auto-exit` are accepted without
effect. They do not enable Nx plugins, distributed execution or Nx Cloud.

## Target inspection

`qk show target <project:target[:configuration]> --json` shows the resolved
executor, options, caching and concurrency settings, and task dependencies.
`-c <configuration>` selects a configuration. A target name without a project
uses the current project, as `run` does.

`qk show target inputs <target> --json` lists files and declared environment,
runtime and dependency-output inputs. `qk show target outputs <target> --json`
lists resolved output patterns, existing output paths and unresolved templates.
Without `--json`, inputs and outputs print one entry per line. Inspection does
not load dotenv files, execute runtime inputs or require a supported executor.
The listed inputs include qk's mandatory workspace and package configuration
files; these can differ from Nx's hash plan.

Both input and output commands accept `--check <values...>`. File and directory
queries use workspace-relative paths. Inputs also accept declared environment
variable names and runtime commands. The command exits with 1 if any query does
not match. `--json` returns each query and its membership result.

## Project watch

`qk watch -p web --includeDependencies -- <shell command>` watches the selected
projects and, optionally, their project dependencies. `--all` watches all projects,
including projects created after watch starts. `--initialRun` runs once before
waiting for edits. The kebab-case aliases also work.

Callbacks run from the invocation directory with `NX_PROJECT_NAME` and
`NX_FILE_CHANGES` (workspace-relative paths joined with spaces). If the command
mentions `NX_PROJECT_NAME`, a batch runs concurrently once per changed project;
otherwise it runs once for the batch with an empty project name. Initial `--all`
also runs once with an empty project name, matching Nx. Quote the shell command
so variables expand in the callback rather than in the invoking shell.

Native file events are debounced and changes arriving during a callback are
queued for the next batch. A directory event or backend rescan can include other
eligible source files in the reported batch. Callback failures are reported;
watch continues.
Edits to sources known to be regular files reuse the current project view. Configuration
and ignore-policy changes, file creation/deletion, directories, symlinks and
backend rescans reload discovery and selection before the next callback.
Workspaces using package presets or presets outside the workspace retain a reload
on every source batch, because preset resolution has inputs outside the source set.
Ctrl+C cancels callbacks and their process groups and exits with 130. Ignored
source files, node_modules, Git/qk state, configured caches, explicit target
outputs and warm paths do not trigger callbacks. Global files outside any
selected project do not trigger callbacks. Watch uses no daemon or Nx plugins.

## Compatibility diagnostics

`qk doctor` reports unsupported executors, configured Nx plugins and sync
generators without executing commands, loading dotenv files or running plugins.
It also lists Nx CLI options accepted without effect. `--json` returns a
versioned report with stable finding codes, locations and error/warning counts.
Unsupported executors cause exit code 1; `--strict` also fails on warnings.

This checks normalized explicit target definitions. A clean report does not
establish that commands can execute or that plugin-inferred targets exist.
