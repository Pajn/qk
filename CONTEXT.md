# qk

qk runs workspace tasks with dependency ordering and reuses verified results
and optional warm state across runs.

## Language

**Result record**:
A saved successful task result, identified by its cache key, that describes
the task's outputs and recorded log.

**Warm-state record**:
A saved collection of a task's or named group's scratch state, associated
with the worktree that produced it. Warm state helps a tool run faster but
does not establish a successful task result or a cache hit.

**Blob**:
Saved file or log content identified by its content hash. Several result
records or warm-state records can cite the same blob.
