# qk

qk runs workspace tasks with dependency ordering and reuses verified results
and optional warm state across runs.

## Language

**Declared input**:
A file, environment value, runtime value or dependency artifact selected by
a task's input rules, including configuration that qk always includes.

**Installation input**:
The package installation of a consumed workspace importer, or a named
external package's installations across importers. It includes transitive
dependencies, peer resolutions, patches and integrity data.

**Affected task**:
A task selected because revision changes touch its inputs, or because it
reads the declared outputs of an affected task. Depending on a task without
reading its outputs carries nothing. A continuous task's inputs count as those
of the tasks depending on it. Selection can be conservative and does not
guarantee that the task's cache key changes.

**Change projection**:
An opt-in comparison of declared source changes through consumer-specific
artifact fingerprints at two revisions. It refines affected project selection
without changing declared task inputs or cache keys.

**Import reachability**:
An opt-in refinement of affected selection that leaves out a task when none of
its anchors, the files it depends on through their imports, imports a changed
source, and a project when every task of it a change affects is left out. A
target opts in through a named config. It does not change declared task inputs
or cache keys.

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

**Remote store**:
Shared storage of result records, warm-state records, what tasks are expected
to take and blobs, through which one machine reuses what another saved.

**Run listing**:
The cache keys one run found in or added to the remote store. It tells readers
what the store holds, and which keys were used together.

**Store server**:
A service that answers qk's remote store requests itself, in front of the
storage that holds the records and blobs, rather than the storage serving them
directly.
