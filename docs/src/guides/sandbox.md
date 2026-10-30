# Checking inputs and outputs in a sandbox

`--sandbox` checks that each task declares what it reads and writes in the
workspace. On macOS it runs the task under Seatbelt, through `sandbox-exec`.
On Linux, audit records the task, as [Ultracache](ultracache.md#recorders)
does, and enforce runs it under Landlock. qk is not a hermetic build system: outside the workspace
everything stays open, including system files, the home directory,
temporary files and tool caches.

Inside the workspace, a task may:

- read the files its cache key holds whole, such as its declared inputs, its
  project's `package.json` and the root `.gitignore`, any `node_modules`,
  its own outputs, its dependencies' outputs and its warm paths, though
  audit reports reading an installed package its key leaves out;
- list any directory;
- write its outputs and warm paths.

A file the key holds only by what it changes or resolves to is not one of
them, unless the task's inputs select it: `nx.json`, the file it extends
and `nx.local.json`, the task's project's `project.json` and
`project.local.json`, a pnpm lockfile qk reads, `pnpm-workspace.yaml`, and
the root `package.json` outside the root project. A tool that reads one,
such as a package manager running a script or a nested `qk`, could act on
what the key leaves out, so the task declares the file: as a file input,
which keys it whole, or as a `json` input selecting the fields the tool
reads. `default`'s `{projectRoot}/**/*` selects the project's own
`project.json` and `project.local.json`, and a root project's `nx.json`.

Structured JSON inputs permit reading their declared JSON file, including
its symlink target. Declared directory symlinks permit reading the whole
linked tree and its nested link targets, matching the content in the cache
key. These read permissions do not allow writing input files.

A directory whose every tracked file is an input is allowed whole, so
untracked files in it can be read too, unless it holds one of the
configuration files above that the task does not declare, such as an
ignored `project.local.json`: Landlock cannot allow a directory less a file
in it, so its inputs are then allowed one by one.

## Audit

`--sandbox` (or `--sandbox=audit`) lets a task do everything, and reports
what went beyond its declarations:

With a `build` target that declares `src` as input and `dist` as output,
but also reads `other/notes.txt` and `.env` and writes `stray.txt`:

```text
$ qk run-many -t build,clean --sandbox
…
Sandbox audit: 1 of 2 tasks went beyond what they declare
  app:build
    1 read outside its inputs:
      other/notes.txt
    1 written outside its outputs:
      stray.txt
  read, which the sandbox allows, but not in their keys: .env (1 task)
```

For each task it lists the files read outside its inputs, the TypeScript
configuration (`tsconfig*.json`) it read outside them, which is likely an
input, and what it wrote outside its outputs. The count of tasks that went
beyond what they declare leaves out what enforce allows: the `.git` and
dotenv files tasks read, what they wrote in `.git`, and the installed
packages their keys leave out, listed below. The `.git` and dotenv files are
not listed for each task either: the summary ends with each, once, and how
many tasks read it, then how many wrote in `.git`, and `--sandbox-report`
lists them for each task, as `unkeyedReads` and among `strayWrites`. A task
that reads one its inputs do not declare can be restored from the cache
unchanged when it differs, as one stamping the commit it was built from. A file it writes there and reads, such as
a version stamp it keeps, is listed once, as written: declaring it an output
covers both. Creating the directory that holds a declared output is not
listed: a tool writing `dist/app.js` creates `dist`. Another file written
there, such as `dist/other.txt`, still is. The directories an output lies in
that do not exist yet are created as the task starts, in audit as in
enforce, and removed again if the task leaves them empty: a tool that makes
one by renaming a directory it built beside it, as cargo makes `target` for
an output in `target/release`, then finds it there rather than writing
outside its outputs. The output itself is not created, as its path may name
a file, except by enforce on Linux, which takes a missing output without an
extension for a directory (see [Linux](#linux)). Reads of workspace packages'
`package.json`, which package managers and bundlers make to resolve them,
and of version and package managers' configuration, such as
`.tool-versions`, are counted once at the top rather than listed for each
task; `--sandbox-report` lists them as `manifestReads` and `toolchainReads`.
Other paths read by many tasks, at least three and at least a quarter of
those that went beyond what they declare, are listed once at the top, under
`read by many tasks, likely by the tools their commands start through`.

Files read in a project the task's project depends on, directly or not, are
grouped by that project, as `dependencyReads` in `--sandbox-report`, with how
many each in the summary. They are what running a dependency's code, or a
tool following imports into it, reads. Inputs prefixed with `^`, such as
`^production`, key them. Whether one changes the task's result depends on the
tool: a test running a dependency's code depends on it, while a lint rule
that only follows imports to look for a cycle through the linted file cannot
find one through a project that cannot import back.

A task whose dependencies' files cannot change its result names them in
`qk:ultracache.ignoreReads` (see
[ignored reads](#scratch-directories-and-ignored-reads)) with a leading `^`,
the glob after it matched from the root of each project its project depends
on, directly or not: `^**` matches every file in them, `^src/**` their
sources. A file in a project nested in a dependency is that project's, and
matches only if the task depends on it too.

### Installed packages outside the key

With a pnpm v9 lockfile qk reads, a task's key counts the packages its
importers install, and those its `externalDependencies` name. A task that
names its external dependencies, even none, leaves the root importer out (see
[the cache](cache.md)), so a tool only the root `package.json` installs, such
as a linter, is in its key only if the task names it. Audit reports reading
an installed package the key leaves out, by name:

```text
$ qk run-many -t lint --sandbox
…
Sandbox audit: 3 tasks kept to what they declare
  app:lint
    1 installed package read, which the sandbox allows, but not in its key; name it in its externalDependencies:
      fallow
```

Enforce allows the read, so it does not count the task as going beyond what
it declares, but no key holds the package: name it in the task's
`externalDependencies` input, beside any it names already; its installations
and everything they reach then count. A
package read by many tasks, as for paths, is listed once at the top with how
many tasks read it.

A task without `externalDependencies` keys every package its importers
install and everything those depend on. A package it still reads outside
that set is no dependency of any package its importers install: some package
imports it without declaring it, and finds it through pnpm's hoisting, which links every package
installed anywhere in the workspace where any of them can resolve it. What the
task reads then changes with what another project installs. Audit lists such
a package apart, as `hoistedPackages` in `--sandbox-report`:

```text
  app:build
    1 installed package read that nothing its importers install depends on, which the sandbox allows, found through pnpm's hoisting; declare it as a dependency of the package importing it:
      phantom
```

Naming it in `externalDependencies` would key it, but in place of every other
installed package. Declaring it as a dependency of the package importing it,
with pnpm's `packageExtensions`, keys it with the rest. Narrowing pnpm's
`hoist-pattern` instead makes the import fail rather than find it, which shows
where it is missing but keys nothing.

Audit finds a package by a read of its `package.json`, which Node and
bundlers make for every package they load, rather than by each of the
thousands of files a tool may read: reports of those would crowd the system
log, which drops reports when many arrive at once. A tool reading a package without its
`package.json` goes unreported; [input analysis](input-analysis.md), which
records every read, finds it.

A read is placed by the directory of pnpm's store it lies in,
`node_modules/.pnpm/<directory>`, which pnpm names after the installation's
snapshot key in the lockfile; a read through an importer's
`node_modules/<name>` link counts as a read of the directory it points to.
What lies elsewhere in `node_modules`, such as `.bin`, `.modules.yaml` or a
tool's `.cache`, and a store directory no snapshot in the lockfile names,
such as one left from an earlier install, belong to no package and are never
reported. Without a pnpm lockfile qk reads, with one a task's inputs select,
or with pnpm's `node-linker=hoisted`, which has no such store, nothing is
reported.

## Enforce

`--sandbox=enforce` refuses those reads and writes instead, so a task that
depends on what it does not declare fails, and only its declared outputs can
change in the workspace. Reading and writing `.git` and reading dotenv files
stay allowed, and so do reading and writing qk's own state for the
worktree, where warm state lives.

Reading an installed package the key leaves out stays allowed too, and is
not listed: Landlock grants a directory with everything beneath it, so on
Linux a store directory could not be refused while `node_modules` stays open
for what tools create there as they run, and enforce would refuse on macOS
what it lets through on Linux. Audit, or
[input analysis](input-analysis.md), finds such reads.

## Scratch directories and ignored reads

A tool may keep a cache in the project that nothing declares, as a linter's
results by file. Declared as [warm state](warm-state.md), qk keeps and restores
it, which is safe only for a tool that checks each entry against what it
read. For any other, name the directory as scratch on its target:

```json
"lint": { "qk:sandbox": { "scratch": ["{projectRoot}/.tool-cache"] } }
```

qk empties a scratch directory before the task runs, with or without
`--sandbox`, so nothing an earlier run left there can change a result,
whatever the tool does with it. The cache neither keeps nor restores it, a
recording for [Ultracache](ultracache.md) leaves it out, and the sandbox lets
the task read and write it, in audit and enforce alike. A scratch directory
lies in the workspace, outside `.git`, `.qk` and `node_modules` however their
names are cased, holds no project and neither qk's cache nor its state, and
neither is nor lies under a symbolic link, which emptying it could follow out
of the workspace; `qk doctor` reports one that breaks these rules. The
links are checked again as each directory is emptied, in case an earlier
task made one. Two tasks of a run cannot share a scratch directory, nor
have one inside the other's, since each empties it before it runs: such a
run fails before any task starts.

A file a task reads that cannot change its result, as a tool reading the
tsconfig only to print a hint, is named in its `qk:ultracache.ignoreReads`,
whether or not the task is recorded. That it cannot is a claim qk cannot
check, which a scratch directory does not need. The globs that leave it out
of recordings also leave it out of the audit's findings, and enforce lets
the task read the files they match, Git-ignored ones included.

Landlock and Seatbelt hold paths, so enforce allows the files that exist as
the run starts, found by walking each glob from the directory its leading
literal parts name. A glob ending in `/**`, as `**/.cache/**`, allows each
directory it names whole, so a profile stays small however many files the
directories hold, and a file made in one during the run may be read too.
Any other glob, or one with a negated group such as `!(private)`, which can
leave out a path deeper down, allows the files it matches one by one. The
walk never enters `.git`, `.qk` or `node_modules`, and a directory holding
one of them is not allowed whole: the directories beside it are. A symbolic
link is left out, since allowing it would allow whatever it leads to. Seatbelt
matches the path a link resolves to, so a link put in place of one of these
during the run reaches only what is allowed anyway. Landlock follows links
as it allows a path, so on Linux each is allowed only while no link stands
anywhere on the way to it from the workspace root: one put there by an
earlier task of the run gets no rule.

A glob that covers a nested workspace, a directory with a `pnpm-lock.yaml`
importer of its own such as a project's end-to-end tests, covers what it
installs too. Reads of those packages reach their pnpm store directory, not
a path under the ignored directory, so the audit leaves out a read of a
package that the nested workspace installs, directly or not, unless the
task's key covers it. The same goes for the workspace projects it links to,
and what they install, unless the task's key counts them: enforce lets the
task read their manifests, and the audit leaves those reads out. The root
importer, and an importer the task's key counts, is never covered this way.

## Notes

- A sandboxed run keeps the cache. A task restored from it did not run, so
  it is not checked; the summary counts it apart, and `--sandbox-report`
  lists none of its findings. `--skip-cache` runs every task to check it.
  Enforce caches what a task produced held to its declarations, which a run
  without the sandbox then restores, as it restores any result of the same
  key.
- `--sandbox-report <path>` writes every finding as JSON.
- On macOS, qk reads the sandbox's reports from the system log while the
  run lasts; each report is tagged with its task. The log can drop reports
  when many arrive at once, so audit lists what was reported rather than
  proving nothing else happened. A task that makes more than 100000 reports,
  as a package manager installing thousands of files outside its outputs, is
  listed as not audited, with how many it made, and the other tasks keep
  theirs. On Linux, audit's reports come from each
  task's recording, and a recording that lost events or could not be decoded
  leaves its task listed as not audited.
- A task the sandbox cannot be built for, as one whose inputs cannot be
  resolved, one whose profile Seatbelt rejects on macOS, or one whose rules
  Landlock cannot take on Linux, runs unsandboxed under audit and is listed
  under `unsandboxed` with the reason. Enforce does not run it unconfined: it
  fails, is listed under `refused` in `--sandbox-report`, and says to set
  `"qk:sandbox": false` on its target to run it unsandboxed. A task taken from
  the cache runs nothing, so it is not refused.
- On macOS, Seatbelt caps the paths a profile with reports may name at 64 KiB
  together. A task's profile names the shorter list of pnpm's store
  directories, those to report or those not to, which fits stores of a few
  thousand directories. A task whose profile still does not fit is retried
  without reporting installed packages: if Seatbelt takes that profile, the
  task runs sandboxed and is listed under `packagesUnchecked` in
  `--sandbox-report`; if not, it is handled as above. Only audit profiles
  carry reports, so the cap is an audit's: an enforce profile can name far
  more. Only tasks that started are listed.
- On macOS, Seatbelt refuses to apply a sandbox inside one that restricts anything, as
  every task's profile does. A task that applies its own, as Xcode does to
  run Swift macros, fails with `sandbox_apply: Operation not permitted`.
  Audit runs it again unsandboxed and lists it with the reason, without the
  findings of its first attempt, which ended early; enforce leaves it failed.
  `"qk:sandbox": false` on its target runs it unsandboxed from the start, in
  either mode. For Swift, `-disable-sandbox` in its Swift flags instead
  leaves out only the compiler's own sandbox, so the task can be checked.
- On Windows, qk has no sandbox.

## Linux

Audit records each task with the recorder [Ultracache](ultracache.md#recorders)
uses, eBPF where it loads and strace otherwise, as `QK_RECORDER` picks, and
reports what the recording shows the task did beyond what it declares. A
read is a file the task opened for reading, and a write one it opened for
writing, created, renamed or removed. A task whose recording is incomplete
is listed as unaudited, with why, rather than with part of its findings.

Enforce uses Landlock, in Linux 5.13 and later, which is available to any
process, without root and inside containers. It can refuse but not report:
a refused access fails the task with `Permission denied` rather than being
listed.

### Auditing recorded runs

With `--record`, `--sandbox` audits a task the cache records from that
recording rather than recording it twice. `QK_RECORD=audit` does the same
as `--record --sandbox`, for CI, which sets it once for every run:

```text
$ QK_RECORD=audit qk run-many -t build
…
Sandbox audit: 1 of 4 tasks went beyond what they declare
  app:build
    1 read outside its inputs:
      other/notes.txt
  12 tasks restored from the cache, so not audited
```

Where no recorder works, such a run goes on unrecorded and unaudited, and
says so, when `QK_RECORDER` is unset or `auto`. A `QK_RECORDER` naming a
recorder that cannot record here, or naming none, fails the run.

A task without [`qk:ultracache`](ultracache.md) is recorded for the audit
alone, as without `--record`. Only `--record` saves what a recording
read for the cache: without it, an audit's recordings save nothing, though
recordings saved before still key results. An audit costs reading each recorded task's
trace a second time, as the audit reads installed packages' manifests that a
path set leaves out. `--report` gives
each task audited its findings as `audit`, by the kinds `--sandbox-report`
names, those with nothing left out: `{}` for a task that kept to its
declarations. A task not audited has no `audit`, and the run history keeps
it with the rest of the task.

Enforce cannot be combined with `--record` yet.

Landlock holds paths that exist, so each task's rules are built as it
starts, after its dependencies have written their outputs. An output that
does not exist yet and has no extension is taken for a directory and created
then too, and removed again if the task leaves it empty. A task may write beneath its outputs, but not remove
or rename an output directory itself, which would need rights over its
parent: a tool should clear an output directory rather than delete and
recreate it. Outside the workspace, Landlock allows what exists when the
task starts; an entry created later directly in a directory above the
workspace, such as a new file in the home directory when the workspace is
below it, is refused.
