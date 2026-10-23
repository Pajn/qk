# Run history and reports

```sh
qk show runs
qk show run --json
qk show task web:build
qk run web:build --report run.json
```

Every run is recorded in a SQLite database beside the cache
(`<git common dir>/qk/history.db`, or `.qk/history.db` outside Git), so linked
worktrees share it. Each run records the worktree it ran in: `qk show runs`,
`qk show run` without an id, and `qk show flaky` read the current worktree's
runs, and `--all-worktrees` reads every worktree's, naming each run's. Runs
recorded before qk kept the worktree appear only with `--all-worktrees`.
Each task's record holds its status, cache result
(`local-hit`, `remote-hit`, `miss` or `uncached`), key, timing and cause: what
differs from the previous key recorded for the task, grouped as `files` (with
the paths added, removed and changed), `configuration` (the files qk keys for
every task, such as `.gitignore` and the task's own `package.json`, listed
with `files`, the nx.json settings every key holds, by key, what the key
holds of the task's project's configuration, such as its root and output
paths, and dependency
manifests and `pnpm-workspace.yaml` as dependents read them), `env`, `runtime`, `dependencies` (the dependency outputs
`dependentTasksOutputFiles` reads, by task and pattern),
`lockfile` (with the importers and packages), `inputs`, `definition` (with each field that differs and both of its
values), `args` and `tooling`, or `first`, `unchanged` and
`unknown` when the previous inputs are no longer kept. For targets with warm
state it also holds, for each entry that restored any, its group when it
has one, where that state came from (`local`, `worktree <root>` or
`remote <branch>`) and how many files and bytes were restored; and, once for
the task, which groups were on disk already and how long saving took, or that
saving happened in the background. The run summary names the tasks that
started from warm state, and `qk show task` compares how long the task took from warm state
with how long it took without, over its successful runs that executed. A task
that executed also records the most memory it used: that of its commands and
every process under them, summed, sampled every 250 ms while it runs, so a
task too brief to be sampled records none. On macOS a process's memory is
its physical footprint, the figure Activity Monitor shows, and a task's peak
is at least the most its largest process used in its life, which the system
keeps, so a spike between samples is not missed; elsewhere it is its
resident memory. The scheduler [waits for memory](running-tasks.md#waiting-for-memory)
from these peaks. The run
summary gives the most the run's tasks used together and the three tasks
that used the most, and `qk show task` what the task used in each run. The
newest 200 runs are kept. The schema is versioned in `schema_version`.

`qk show hash <task> --against <run-id>` compares the key a task would have
now with its key in a recorded run, without running it; see
[Key inspection](../reference/cli.md#key-inspection).

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
        dependency ui:build:**/*.d.ts
      used 2.1 GB of memory at most
```

## Execution logs and mixed outcomes

```sh
qk show flaky
qk show flaky web:test --json
qk show log <run-id> web:test
```

Successful and failed cacheable executions retain stdout and stderr separately,
with their observed chunk order preserved. `show log` replays each chunk to its
original stream. Cache hits replay a previous success and do not create another
execution log. Otherwise cacheable tasks run with `--skip-nx-cache` (or its
aliases or `NX_SKIP_NX_CACHE=true`) retain their keys and execution logs while
always executing, without restoring or publishing results or warm state.
Finite tasks that run without the cache, under `--sandbox` or
`inputs analyze`, or with the cases `qk affected` narrowed them to, retain
their execution logs too, cacheable or not, within the same budget. Their
inputs are not rechecked, so `show flaky` does not count them. Otherwise,
uncacheable tasks and tasks whose inputs cannot be keyed do not retain
execution logs. Failed executions never publish reusable
cache entries.

`show flaky [task]` lists task keys with both successful and failed executions
in retained history. `--limit` defaults to 20 groups; `--json` includes the exact
run IDs, outcomes and whether each log is still available. It excludes cache
hits, cancelled executions and executions whose declared inputs changed while
running. These are **mixed outcomes for identical declared inputs**, which can
also indicate undeclared inputs, network dependencies or changing external
state. To collect more actual executions while keeping existing results, run
the task repeatedly with `--skip-nx-cache`, then use `qk show flaky` and
`qk show log` to inspect the outcomes. `qk reset --only-cache` also clears
result entries while keeping history.

Each log retains at most its first 4 MiB of framed output and reports truncation
when replayed. History keeps at most 64 MiB of log payload across linked
worktrees, evicting older payloads while keeping their outcome observations.
The normal 200-run retention also removes those runs' logs and observations.
These limits apply to retained payload, not SQLite metadata, free pages or
result-cache logs. Logs stay local in `history.db`; remote result caching is
unchanged. Logs can contain anything the command prints, including credentials.

Schema version 4 adds execution observations and logs. Older histories upgrade
in place; existing runs remain readable but have no execution logs and are not
included in mixed-outcome detection.

Schema version 5 adds the memory each task used. Older histories upgrade in
place, and runs recorded before have none.

Schema version 6 adds the worktree each run ran in. Older histories upgrade in
place, and runs recorded before have none. Linked worktrees share the history,
so once one worktree's qk upgrades it, a qk that reads only an earlier version
cannot read or record runs in any of them.
