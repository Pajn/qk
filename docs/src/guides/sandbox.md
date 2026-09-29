# Checking inputs and outputs in a sandbox

`--sandbox` runs each task under the system's sandbox to check that it
declares what it reads and writes in the workspace: Seatbelt, through
`sandbox-exec`, on macOS, and Landlock on Linux. qk is not a hermetic build system: outside the workspace
everything stays open, including system files, the home directory,
temporary files and tool caches.

Inside the workspace, a task may:

- read the files in its cache key, any `node_modules`, the lockfile, its own
  outputs, its dependencies' outputs and its warm paths;
- list any directory;
- write its outputs and warm paths.

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

For each task it lists the files read outside its inputs, the `.git` and
dotenv files it read, which no cache key covers, and what it wrote outside
its outputs. Paths read by many tasks, at least a quarter of those with
findings, are listed once at the top, under `read by many tasks, likely by
the tools their commands start through`: they are typically a package
manager reading every workspace `package.json`, or a version manager reading
`.tool-versions`, as a command starts.

## Enforce

`--sandbox=enforce` refuses those reads and writes instead, so a task that
depends on what it does not declare fails, and only its declared outputs can
change in the workspace. Reading and writing `.git` and reading dotenv files
stay allowed.

## Notes

- A sandboxed run skips the cache, since a task has to run to be watched.
- `--sandbox-report <path>` writes every finding as JSON.
- qk reads the sandbox's reports from the system log while the run lasts;
  each report is tagged with its task.
- A task whose inputs cannot be resolved, or whose profile the sandbox
  rejects, runs unsandboxed and is listed with the reason.
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
