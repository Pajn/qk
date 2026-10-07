# Environment variables

## Environment precedence

Environment precedence, highest first: `FORCE_COLOR` from `color: true`,
configuration `env`, `options.env`, target-level `env`, the variables Nx sets
for every task, the inherited process environment, `envFile`, then the
task's dotenv files in Nx's order: in the
project root and then the workspace root, `.env.<target>.<configuration>`,
`.env.<configuration>`, `.env.<target>`, then `.env.local`, `.local.env` and
`.env`, each also as `.env.<name>.local`, `.<name>.local.env` and
`.<name>.env`. A file never overrides a variable an earlier source set. The
task variables are `NX_TASK_TARGET_PROJECT`, `NX_TASK_TARGET_TARGET`,
`NX_TASK_TARGET_CONFIGURATION`, `NX_WORKSPACE_ROOT`, `LERNA_PACKAGE_NAME`,
`NX_TUI=false` and `FORCE_COLOR`, which is `true` unless already set, and
`NX_TASK_HASH`, the task's cache key, whenever the run computes one: for
every task that goes through the cache, cacheable or not, but not under
`--skip-cache` or in a run without a cacheable target.
Dotenv is loaded into child environments without mutating qk's process
environment; qk's own environment, where remote cache credentials arrive,
includes the root `.env.local` and `.env`. `NX_LOAD_DOT_ENV_FILES=false`
turns every dotenv file off, `envFile` included, as in Nx. Dotenv interpolation uses
dotenvy's per-file semantics; it does not provide cross-file interpolation
against the merged child environment.


## Run controls

| Variable | Behavior |
| --- | --- |
| `NX_DEFAULT_PROJECT` | Fallback project for shorthand commands |
| `NX_BASE`, `NX_HEAD` | Affected comparison revisions; CLI overrides them |
| `NX_PARALLEL` | Task concurrency, overridden by `--parallel` |
| `QK_CORES` | Core budget, overridden by `--cores` |
| `NX_BAIL=true` | Stop starting tasks after a failure |
| `NX_IGNORE_CYCLES=true` | Break dependency cycles with a warning |
| `NX_DEFAULT_OUTPUT_STYLE` | Default output style, overridden by `--output-style` |
| `NX_RUN_REPORT` | JSON report path, overridden by `--report` |
| `NX_VERBOSE_LOGGING` | Task verbosity; `--verbose` sets it to `true` |
| `NX_SKIP_LOG_GROUPING=true` | Disable static output groups in GitHub Actions |
| `NO_COLOR`, `FORCE_COLOR` | Output color controls |
| `CI` | CI output and remote cache mode selection |

## Cache controls

| Variable | Behavior |
| --- | --- |
| `NX_SKIP_NX_CACHE=true` | Bypass cache reads, writes and warm state |
| `NX_SKIP_REMOTE_CACHE=true`, `NX_DISABLE_REMOTE_CACHE=true` | Local cache only |
| `NX_CACHE_DIRECTORY` | Override `nx.json`'s `cacheDirectory` |
| `NX_MAX_CACHE_SIZE` | Override `nx.json`'s `maxCacheSize` |
| `NX_POWERPACK_CACHE_MODE` | Override S3 `localMode`/`ciMode` |
| `QK_REMOTE_UPLOAD_MODE` | Override S3 `uploadMode`: `wait` or `background` |
| `QK_PROFILE_CACHE=1` | Enable [cache phase timings](cli.md#profiling-cache-operations), as `--profile` does |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` | Remote credentials |
| `GITHUB_HEAD_REF`, `GITHUB_REF_NAME` | CI branch for remote warm state |

See [Workspace configuration](workspace.md) for defaults and
[Remote caching](../guides/remote-cache.md) for failure behavior.

## Variables supplied to tasks

`NX_TASK_TARGET_PROJECT`, `NX_TASK_TARGET_TARGET`,
`NX_TASK_TARGET_CONFIGURATION`, `NX_WORKSPACE_ROOT`, `LERNA_PACKAGE_NAME`,
`NX_TUI=false`, and `FORCE_COLOR` describe the running task.
`NX_TASK_HASH` is available when a cache key was computed.
Targets using `qk:threads` additionally receive `QK_THREADS` and their
configured thread variables. Targets using `qk:warm` receive their expanded
warm environment when warm state is enabled. A task an affected profile with
reachability narrowed to some of its cases receives `QK_AFFECTED_CASES`, the
absolute path of a file naming those cases one per line, each relative to the
workspace root; see
[Import reachability](../guides/affected-reachability.md#running-only-the-selected-cases).
