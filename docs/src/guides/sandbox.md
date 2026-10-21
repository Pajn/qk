# Checking inputs and outputs in a sandbox

`--sandbox` runs each task under the system's sandbox to check that it
declares what it reads and writes in the workspace: Seatbelt, through
`sandbox-exec`, on macOS, and Landlock on Linux. qk is not a hermetic build system: outside the workspace
everything stays open, including system files, the home directory,
temporary files and tool caches.

Inside the workspace, a task may:

- read the files in its cache key, any `node_modules`, the lockfile, its own
  outputs, its dependencies' outputs and its warm paths, though audit reports
  reading an installed package its key leaves out;
- list any directory;
- write its outputs and warm paths.

Structured JSON inputs permit reading their declared JSON file, including
its symlink target. Declared directory symlinks permit reading the whole
linked tree and its nested link targets, matching the content in the cache
key. These read permissions do not allow writing input files.

A directory whose every tracked file is an input is allowed whole, so
untracked files in it can be read too.

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
    1 read, not in any key:
      .env
    1 written outside its outputs:
      stray.txt
```

For each task it lists the files read outside its inputs, the TypeScript
configuration (`tsconfig*.json`) it read outside them, which is likely an
input, the `.git` and dotenv files it read, which no cache key covers, and
what it wrote outside its outputs. A file it writes there and reads, such as
a version stamp it keeps, is listed once, as written: declaring it an output
covers both. Reads of workspace packages'
`package.json`, which package managers and bundlers make to resolve them,
and of version and package managers' configuration, such as
`.tool-versions`, are counted once at the top rather than listed for each
task; `--sandbox-report` lists them as `manifestReads` and `toolchainReads`.
Other paths read by many tasks, at least a quarter of those with findings,
are listed once at the top, under `read by many tasks, likely by the tools
their commands start through`.

Files read in a project the task's project depends on, directly or not, are
grouped by that project, as `dependencyReads` in `--sandbox-report`, with how
many each in the summary. They are what running a dependency's code, or a
tool following imports into it, reads. Inputs prefixed with `^`, such as
`^production`, key them. Whether one changes the task's result depends on the
tool: a test running a dependency's code depends on it, while a lint rule
that only follows imports to look for a cycle through the linted file cannot
find one through a project that cannot import back.

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
Sandbox audit: 1 of 3 tasks went beyond what they declare
  app:lint
    1 installed package read, not in its key; name it in its externalDependencies:
      fallow
```

Name the package in the task's `externalDependencies` input, beside any it
names already; its installations and everything they reach then count. A
package read by many tasks, as for paths, is listed once at the top with how
many tasks read it.

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
stay allowed.

Reading an installed package the key leaves out stays allowed too, and is
not listed: Landlock grants a directory with everything beneath it, so on
Linux a store directory could not be refused while `node_modules` stays open
for what tools create there as they run, and enforce would refuse on macOS
what it lets through on Linux. Audit on macOS, or
[input analysis](input-analysis.md) on either system, finds such reads.

## Notes

- A sandboxed run skips the cache, since a task has to run to be watched.
- `--sandbox-report <path>` writes every finding as JSON.
- qk reads the sandbox's reports from the system log while the run lasts;
  each report is tagged with its task. The log can drop reports when many
  arrive at once, so audit lists what was reported rather than proving
  nothing else happened.
- A task whose inputs cannot be resolved, or whose profile the sandbox
  rejects, runs unsandboxed and is listed with the reason.
- Seatbelt caps the paths a profile with reports may name at 64 KiB
  together. A task's profile names the shorter list of pnpm's store
  directories, those to report or those not to, which fits stores of a few
  thousand directories. A task whose profile still does not fit is retried
  without reporting installed packages: if Seatbelt takes that profile, the
  task runs sandboxed and is listed under `packagesUnchecked` in
  `--sandbox-report`; if not, it runs unsandboxed and is listed with the
  reason, as above. Only tasks that started are listed.
- Seatbelt refuses to apply a sandbox inside one that restricts anything, as
  every task's profile does. A task that applies its own, as Xcode does to
  run Swift macros, fails with `sandbox_apply: Operation not permitted`; qk
  runs it again unsandboxed and lists it with the reason, without the
  findings of its first attempt, which ended early.
- On Windows, qk has no sandbox.

## Linux

Landlock, in Linux 5.13 and later, is available to any process, without
root and inside containers. It can refuse but not report, so Linux has
enforce mode only: `--sandbox` alone is an error there, and a refused access
fails the task with `Permission denied` rather than being listed.

Landlock holds paths that exist, so each task's rules are built as it
starts, after its dependencies have written their outputs. An output
directory that does not exist yet is created then, and removed again if the
task leaves it empty. A task may write beneath its outputs, but not remove
or rename an output directory itself, which would need rights over its
parent: a tool should clear an output directory rather than delete and
recreate it. Outside the workspace, Landlock allows what exists when the
task starts; an entry created later directly in a directory above the
workspace, such as a new file in the home directory when the workspace is
below it, is refused.
