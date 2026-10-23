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
naming `{threads}`, times its `scale` if it has one (see below); `"qk:threads": true` sets only `QK_THREADS`. A tool
without such a variable reads it in its configuration, as Vitest's
`maxWorkers: Number(process.env.QK_THREADS) || undefined` does. Under Nx
neither is set, so the tool keeps its default there. Every other task holds
one core. Continuous tasks are outside the core budget: `qk:threads` does
not allocate a share or set these environment variables for them.

A share is fixed when the task starts, since a tool cannot give up workers
it has started. Threaded tasks without an expectation, from earlier runs,
split the free cores evenly, after a core for each task slot other pending
tasks could take, so two get half each and three a third, and one starting
alone leaves room for what follows.

A task's expectation is its time in its recent runs, those that executed it,
on the cores it held then: from local history, which lives beside the cache,
or for a task local history lacks, from the [remote store](remote-cache.md),
which shares what other machines ran. A task with an expectation is given the part of the cores that its work is of the work that can
run beside it: its own, and the largest of what the other task slots can hold
of the tasks still to finish, leaving out those that depend on it, which wait
for it to end. A running task counts what it has still to do, its expected
core-milliseconds less what it has done on the cores it holds. A short suite
thus takes a few cores, finishes, and leaves the rest free for a long one
still waiting for its dependencies, rather than holding an even split of them
when the long one starts; and a long task is given most of the cores, so that
it does not keep running once the rest is done. Either share leaves a core
for each other task slot, including tasks without history, and the minimum of
other ready threaded tasks when the budget can accommodate them.

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
running alone on 16 starts 32. Rounding up adds less than one thread per
task, so the threads every task is told stay within `scale` times the
budget plus one for each task running: eight tasks holding a core each with
`scale: 1.1` are told 2 threads each. A test's wall-clock budgets, such as a
`waitFor` or a test timeout, are no less trustworthy than that bound makes
them, however many tasks run at once. Keep `scale` to targets whose threads wait; for a
compiler, whose threads keep their cores busy, it only adds contention.
Vitest reads `VITEST_MAX_WORKERS` itself, and a variable a script sets for
Nx keeps applying there, since qk sets nothing under Nx.

## How busy a task kept the cores

While a task runs, qk samples the CPU time of its processes, as it samples
their memory. The summary follows each threaded task's threads with the
cores it kept busy on average, `app:test 6 on 3 (2.4 busy)`, and
`qk show task` says it, with the cores it held, for a task that ran for two
seconds or more and whose processes were sampled using CPU time: one whose
processes all started and exited between samples has no figure. A task busy on fewer cores than it held is waiting, and
can take a higher `scale`; one busy on more is starting threads it was not
told about, such as a runtime's own, and holds less than it uses. A process
that starts and exits between two samples, a quarter of a second apart, is
not counted, so the figure is a lower bound, close for a task whose work runs
in processes that last.

