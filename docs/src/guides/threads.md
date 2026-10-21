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
one core. Continuous tasks are outside the core budget: `qk:threads` does
not allocate a share or set these environment variables for them.

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

## Threads that wait

A share counts cores, one thread to each. Test runners' workers spend much of
their time waiting, on the file system, a database or timers, so one worker
per core leaves the cores idle. `scale` tells the task to start more threads
than the cores it holds:

```jsonc
"test": {
  "qk:threads": {
    "scale": 2,
    "env": { "VITEST_MAX_WORKERS": "{threads}" }
  }
}
```

The task holds its share of the budget as without `scale`, and is told
`ceil(share × scale)` threads: a task holding 3 cores starts 6 workers, and one
running alone on 16 starts 32. The threads every task is told stay within
`scale` times the budget, so a test's wall-clock budgets, such as a `waitFor`
or a test timeout, are no less trustworthy than the scale makes them, however
many tasks run at once. Keep `scale` to targets whose threads wait; for a
compiler, whose threads keep their cores busy, it only adds contention.
Vitest reads `VITEST_MAX_WORKERS` itself, and a variable a script sets for
Nx keeps applying there, since qk sets nothing under Nx.

## How busy a task kept the cores

While a task runs, qk samples the CPU time of its processes, as it samples
their memory. The summary follows each threaded task's threads with the
cores it kept busy on average, `app:test 6 on 3 (2.4 busy)`, and
`qk show task` says it for every task that ran for two seconds or more, with
the cores it held. A task busy on fewer cores than it held is waiting, and
can take a higher `scale`; one busy on more is starting threads it was not
told about, such as a runtime's own, and holds less than it uses. A process
that starts and exits between two samples, a quarter of a second apart, is
not counted, so the figure is a lower bound, close for a task whose work runs
in processes that last.

