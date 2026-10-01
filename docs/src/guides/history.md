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
