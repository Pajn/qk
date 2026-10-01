# Run history and reports

```sh
qk show runs
qk show run --json
qk show task web:build
qk run web:build --report run.json
```

Every run is recorded in a SQLite database beside the cache
(`<git common dir>/qk/history.db`, or `.qk/history.db` outside Git), so linked
worktrees share it. Each task's record holds its status, cache result
(`local-hit`, `remote-hit`, `miss` or `uncached`), key, timing and cause: what
differs from the previous key recorded for the task, grouped as `files` (with
the paths added, removed and changed), `env`, `runtime`, `dependencies` (with
the dependency tasks), `lockfile` (with the importers and packages),
`inputs`, `definition`, `args` and `tooling`, or `first`, `unchanged` and
`unknown` when the previous inputs are no longer kept. For targets with warm
state it also holds where that state came from (`local`, `worktree <root>`
or `remote <branch>`), how many files and bytes were restored, which groups
were on disk already, and how long saving it took, or that it was saved in
the background; the run summary names the tasks that started from warm
state, and `qk show task` compares how long the task took from warm state
with how long it took without, over its successful runs that executed. The newest 200 runs are
kept. The schema is versioned in `schema_version`.

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
        dependency ui:build
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
execution log. Uncached tasks, tasks run with `--skip-cache`, and tasks bypassed
before execution do not retain logs. Failed executions never publish reusable
cache entries.

`show flaky [task]` lists task keys with both successful and failed executions
in retained history. `--limit` defaults to 20 groups; `--json` includes the exact
run IDs, outcomes and whether each log is still available. It excludes cache
hits, cancelled executions and executions whose declared inputs changed while
running. These are **mixed outcomes for identical declared inputs**, which can
also indicate undeclared inputs, network dependencies or changing external
state. To collect another actual execution after a cached success, `qk reset
--only-cache` clears result entries while keeping history.

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
