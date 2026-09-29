# Checking inputs and outputs in a sandbox

On macOS, `--sandbox` runs each task under the system sandbox (Seatbelt,
through `sandbox-exec`) to check that it declares what it reads and writes
in the workspace. qk is not a hermetic build system: outside the workspace
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
- Only macOS has a sandbox for qk so far.
