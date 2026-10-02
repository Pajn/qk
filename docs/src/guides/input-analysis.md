# Recording task inputs

```sh
qk inputs analyze web:build --report inputs.json --suggestions inputs-review.json
qk inputs analyze web:build --report next.json --previous inputs.json
qk inputs analyze web:build -c production --report production.json -- --verbose
```

`inputs analyze` executes a finite task and its task dependencies, records
file accesses by their commands and descendants, and compares those accesses
with qk's effective file-input coverage. The terminal summary goes to stderr;
task output keeps its usual streams. `--report` writes a versioned input-analysis
JSON report instead of the normal run report. Without it, analysis prints only
the summary. Paths to reports are relative to the invocation directory.

Analysis bypasses result-cache reads and writes, remote caching, and warm-state
restoration/publication for every task in the graph. Existing output and warm
files remain on disk: this is not a clean build. Use a clean checkout when you
want to observe a tool's cold behavior. Normal run history still records the run.

## Reading the report

Each task retains its arguments, configuration, outcome, backend and coverage
limitations. Events contain a process ID, operation, path, result and category.
Workspace paths are relative; recorded external paths are absolute. Accesses
retain descriptor targets as `resolvedPath` when available. Declared
symlinks captured before execution can identify a covering input as `keyedPath`.
Target definitions are represented by a digest, not their environment values. Reports
contain filenames and forwarded arguments, so review them before sharing.

Categories distinguish keyed source files, mandatory workspace/package
configuration, structured JSON inputs, dependency outputs, own outputs, warm
state, installed packages, resolution metadata, Git state, discovery and external
accesses. Installed-package classification does not prove that the package is
covered by the task's lockfile fingerprint. Discovery includes directory accesses
and missing-path checks; these deserve review independently of content inputs.

`uncoveredAccesses` lists workspace paths accessed outside those categories.
`observedInputs` lists keyed file inputs accessed in this execution. Successful
recordings contribute to `successfulObservations`; `unobservedInputs` lists
current declared files absent from that union. Mandatory configuration files
are excluded from narrowing suggestions because qk keys them regardless of the
target's filesets.

`candidateFilesets` contains exact observed file patterns. Suggestions replace
only broad `{projectRoot}/**/*` and `{projectRoot}/**` filesets. Existing specific
filesets and named references that do not contain broad project filesets stay
intact, even when some of their files were not observed. A task already using
specific inputs receives a note that no replacement is suggested.

For broad project filesets, `globSuggestions` groups active top-level source trees
into recursive patterns such as `{projectRoot}/src/**/*.{ts,tsx}`. The extension
set includes all currently declared file types in that tree, retaining files that
a partial trace missed. Successful directory enumeration retains whole trees,
such as `{projectRoot}/public/**/*`, so new assets and nested directories stay
covered. Directory evidence is unioned across compatible successful recordings.
Root files remain literal or are grouped by extension. Each glob reports its
observed count, additional declared files retained, and the reason for the scope.
These are review suggestions; discovery at the project root, empty directories,
absent-path probes and new file types outside enumerated trees need further review.

`configurationFragment` is a review-only `project.json` fragment replacing the
target's `inputs`. Only named inputs containing broad project filesets are
expanded. Specific file patterns, other named references, exclusions, environment,
runtime, structured JSON, working-directory, package and dependency declarations
remain in their original order. Mandatory configuration files stay keyed by qk.
No fragment is produced without usable successful observations or when the
existing declarations need no replacement. Earlier version 1 reports remain
compatible; their successful directory events can still contribute evidence.

`accessReview` separates uncovered potential content reads, workspace package
manifest reads, toolchain configuration reads, and metadata-only checks. These
groups help distinguish likely source dependencies from package-manager scans;
none proves an access is harmless. Raw events and `uncoveredAccesses` remain
available in full. The terminal shows content reads first and limits examples
from the other groups.

The terminal prints the fragment. `--suggestions inputs-review.json` exports
fragments grouped by task ID, with a `reviews` entry for every task, including
no-change notes and uncovered-access groups; this wrapper is not itself a
project configuration. Copy a reviewed fragment into the corresponding project's
configuration. Suggestions apply to the whole target, even when the recording
used `-c production`: validate every configuration before accepting them. Expanding
broad named inputs also means future edits to those expanded definitions will no
longer flow through the copied declaration. Specific named references stay live. Review uncovered accesses and directory
or missing-file discovery separately; they are not automatically added. Evaluated
environment values are excluded, but input declarations can contain runtime
commands and reports retain arguments and paths, so review exports before sharing.
No configuration or cache key is changed automatically.

**Not observed does not mean unnecessary.** A tool can use other inputs with
different arguments, environment values, configurations, file existence or warm
state. Recording coverage is partial on both platforms. Validate proposed
changes across representative runs and clean builds.

## Combining recordings

Repeat `--previous <path>` to union observations from several reports. Only
compatible task definitions, effective input configuration, arguments and
platforms are merged. Run IDs prevent counting an observation twice. Failed,
cancelled, skipped or interrupted recordings do not add evidence for narrowing;
their access events remain useful for diagnosing missing declarations. A union
can itself be saved and passed to a later run.

## Backends and limits

On **macOS**, qk uses Seatbelt through `sandbox-exec` and reads its reports from
the system log. It probes the log before starting tasks. Workspace file-access
authorization checks are observed, including subprocesses; external files and
syscall results are not. Failed lookups may never produce an event. The log can
coalesce or lose events, and directory reads do not establish an exact listing.
Directory type is inspected after execution, so renamed or removed directories
can be ambiguous. A machine that does not expose these logs refuses analysis
before task execution.
Symlink accesses can be reported by target path rather than link path.

On **Linux**, install `strace` and make it available on `PATH`. qk probes child
tracing before starting tasks. The backend records path operations and directory
enumeration, follows descendants, tracks working-directory changes, and records
syscall results such as `ENOENT`. Readable opens are potential content dependencies;
write-capable opens do not prove a write, and the recorder does not inspect read
buffers. Symlink targets, inherited descriptors,
`io_uring`, external services and processes outside the task tree are not fully
reconstructed. Undecodable or truncated records are reported as collection issues.

Neither backend traces qk's own command preparation, dotenv loading or runtime
input evaluation. Raw Linux trace files are temporary and removed after collection.
Read/write buffers and exec argument/environment arrays are excluded from tracing.

`--dry-run`, `--graph`, `--sandbox`, continuous tasks and tasks with `readyWhen`
are not supported by analysis. Normal task failures preserve their exit code and
still write a report. Cancellation preserves qk's exit code 130. Collection
issues appear in the report and prevent that execution contributing narrowing
evidence. Windows has no recorder backend.
