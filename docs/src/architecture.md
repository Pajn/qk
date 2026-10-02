# Architecture

qk separates workspace discovery from execution. Inspecting projects and
exporting graphs reads configuration without running executor commands.

## From a request to a run

1. `qk-config` discovers the workspace and normalizes layered definitions.
2. `qk-graph` builds project dependency edges and selects projects.
3. `qk-affected` narrows selection when the request specifies changes.
4. `qk-taskgraph` expands target dependencies into a task graph, resolves
   configurations, and checks cycles and continuous-task relationships.
5. `qk-executor` prepares commands and child environments before execution.
6. `qk-runner` schedules ready tasks with bounded concurrency and core shares,
   propagates failures and handles cancellation.
7. `qk-cache` computes keys, restores results and warm state, and saves
   successful outputs. `qk-lockfile` supplies pnpm installation fingerprints.
8. `qk-history` records runs and key changes. `qk-cli` presents results and
   optional JSON reports.

`inputs analyze` attaches a per-task recorder at command launch. The runner
collects macOS Seatbelt reports or Linux strace output and classifies observations
using the cache's input resolver. `qk-input-analysis` owns the report model and
observation unions. Analysis never substitutes observations into cache keys.

`--dry-run` ends after planning. It does not load dotenv, validate executor
support or execute runtime inputs, so a valid dry run does not guarantee a
successful execution preflight.

## Project graph and task graph

The project graph describes relationships between workspace projects,
including package and implicit dependencies. Project cycles are allowed;
reverse traversal terminates when a project has already been visited.

The task graph describes a particular request after `dependsOn` expansion.
It may run one shared task needed by several projects. Task cycles normally
fail; `--nx-ignore-cycles` drops the edge closing the cycle with a warning.

`qk graph --file -` exports project relationships; `qk run web:build --graph`
exports the project and task graph without running commands.

## Storage and worktrees

By default, linked Git worktrees share a result cache and history database
under the Git common directory. File digests, restore records and active
scratch state live under each worktree's own Git directory. Branch names and
checkout locations are not part of result keys.

See [Local cache](guides/cache.md) for key construction,
[Warm state](guides/warm-state.md) for scratch state, and
[Run history](guides/history.md) for recorded causes.

## Design proposals

The [implementation plan](design.md) records design decisions and development
milestones. Use the guides and reference for current
behavior. [Development](development.md) describes crate responsibilities,
parity tests, releases and benchmarks.
