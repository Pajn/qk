# Sharing cores between tasks

`--parallel` bounds how many tasks run; tools such as Vitest and Jest then
start a worker per core each, so tasks running side by side oversubscribe
the machine. qk also keeps a core budget, `--cores` (or `QK_CORES`, nx.json
`qk:cores`, else the cores available), and a target with `qk:threads` is
given a share of it:

```jsonc
"build": {
  "qk:threads": {
    "env": { "CARGO_BUILD_JOBS": "{threads}", "RAYON_NUM_THREADS": "{threads}" },
    "min": 2,
    "max": 8
  }
},
"test": { "qk:threads": true }
```

The task gets its share as `QK_THREADS` and through each `env` variable
naming `{threads}`; `"qk:threads": true` sets only `QK_THREADS`. A tool
without such a variable reads it in its configuration, as Vitest's
`maxWorkers: Number(process.env.QK_THREADS) || undefined` does. Under Nx
neither is set, so the tool keeps its default there. Every other task holds
one core.

A share is fixed when the task starts, since a tool cannot give up workers
it has started. Threaded tasks starting together split the free cores
evenly, after a core for each task slot other pending tasks could take, so
two get half each and three a third, and one starting alone leaves room for
what follows. A task expected to do a larger part of the work left in the
run, from its recent runs and the threads it had in them, gets that part of
the cores instead, so that it does not keep running once the rest is done.
This larger share leaves capacity for other task slots, including tasks without
history, and respects the minimum of other ready threaded tasks when the budget
can accommodate them. Remaining work subtracts the work already done with each
running task's current thread allocation from its historical core-milliseconds.
Cores come back as tasks finish and go to the next to start. A
share stays within `min` (default 1) and `max` (default the budget); a task
waits until `min` cores are free, unless nothing else runs. The thread count
is not part of the cache key, and neither is `qk:threads`. The summary lists
what each threaded task was given, and `qk show task` records it.
