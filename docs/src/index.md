# qk

**qk**, short for **quick**, is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration and executes tasks with
dependency ordering, affected selection and a cache shared by Git worktrees.

Start with [Getting started](getting-started.md) to inspect and run the
self-contained example workspace.

This book documents the current implementation. The repository's
`docs/task-runner-design.md` describes the longer-term plan; planned behavior
should not be treated as supported functionality.
