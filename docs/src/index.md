# qk

**qk**, short for **quick**, is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration and executes tasks with
dependency ordering, affected selection and a cache shared by Git worktrees.

Start with [Getting started](getting-started.md) to inspect and run the
self-contained example workspace.

This book documents the current implementation. The repository's
`docs/task-runner-design.md` describes the longer-term plan; planned behavior
should not be treated as supported functionality.

Read [Running tasks](guides/running-tasks.md) for execution and dependency
ordering, [Affected selection](guides/affected.md) for change detection, and
[Local cache](guides/cache.md) for inputs and output restoration.

qk also implements [S3-compatible remote caching](guides/remote-cache.md),
[warm state](guides/warm-state.md) and [run history](guides/history.md).

Use [Workspace configuration](reference/workspace.md),
[Target configuration](reference/targets.md), and the [CLI reference](reference/cli.md)
for options and defaults. [Nx compatibility](compatibility.md) explains the supported subset.
