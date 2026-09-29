# Local task cache

```sh
qk cache path
qk run web:build
qk run web:build
qk run web:build --skip-cache
qk cache prune --max-size 1GB
```

With a cacheable target and unchanged inputs, the second run restores outputs
and replays logs. [Run history](history.md) explains why a key changed.

Targets with `cache: true` are cached locally. Inside a Git repository the
cache lives by default in `<git common dir>/qk/cache/v1`, so every linked worktree of the
repository shares one cache. What belongs to one worktree (file digests,
records of the outputs it holds, restores in progress) lives in its own git
directory, `$(git rev-parse --git-dir)/qk`, so nothing of qk's appears in the
working tree. Outside Git both live in `.qk` at the workspace root, the cache
in `.qk/cache/v1`. `qk cache path` prints the cache's location. `--skip-cache` (also
`--skip-nx-cache` and `--skipNxCache`) bypasses all cache reads and writes.

## What changes the key

A task's key covers its ID, forwarded arguments, resolved target definition,
declared inputs, the fingerprints of its dependency tasks, the package
manager, qk's version and the platform. A dependency that declares outputs
is fingerprinted by the content of those outputs alone, so a run that
reproduces them leaves its dependents cached however its own inputs changed;
one without declared outputs is fingerprinted by its key. Files are keyed
by workspace-relative path, content and mode. Branch names and checkout
locations are not part of the key, so identical sources in two worktrees
share entries. Root workspace files (the root `tsconfig.base.json` or
`tsconfig.json`, `nx.json`, `package.json`, `pnpm-workspace.yaml`,
lockfiles) and the manifests of
the task's project and its transitive project dependencies are always
included. Dotenv files are not, as in Nx: they hold per-machine values and
credentials, so keying them would keep machines from sharing entries; `env`
inputs key the variables a task declares.

A pnpm v9 `pnpm-lock.yaml` is keyed by what it installs rather than by its
content: for the root importer and the importers of the task's project and
its transitive project dependencies, every package installation they reach,
by snapshot key (version, peers and patch), integrity and dependencies. The
root importer counts for every task because its packages resolve from every
package. A lockfile change therefore invalidates only the tasks whose
projects install something that changed, plus every task when the lockfile
version, pnpm's `settings` or the lock of pnpm itself changes. A lockfile qk
cannot read is keyed by its whole content, and qk says so.

With such a lockfile, `pnpm-workspace.yaml` is keyed without the keys that
configure resolution (`catalog`, `catalogs`, `overrides`,
`patchedDependencies`, `peerDependencyRules` and the like), whether it is
always included or named by an input: their effect lands in the lockfile, so
a catalog bump invalidates only the tasks whose installs change. This is the
same rule affected selection applies.

## Inputs

Candidate input files are tracked and untracked-but-not-ignored files, minus
any task's declared outputs. The list is taken once per run, as Nx does, so a
file that a task creates without declaring it as an output is seen from the
next run. File contents are re-read whenever their metadata changes; their
digests persist between runs in the worktree's state, keyed by
path, size, modification time and, on Unix, change time, inode and mode, so a
warm run reads only files whose metadata changed. Without
`inputs`, a task uses `default` and `^default`; `default` is
`{projectRoot}/**/*` unless a named input overrides it.

Every input declaration of Nx 23 is supported:

- globs with `!` exclusions, and `fileset`;
- named inputs, also as `{"input": "name"}`;
- `^name` and `{"input": "name", "dependencies": true}` (or `"projects":
  "dependencies"`), which as in Nx cover every project the project depends
  on, directly or not, and `{"fileset": "…", "dependencies": true}`, a
  fileset of each of them;
- `{"input": "name", "projects": […]}`, the named input of the projects the
  names, globs or `tag:` patterns select;
- `env`, `runtime`, `dependentTasksOutputFiles` with `transitive`, and
  `externalDependencies`, which adds every installation of the named
  packages in the pnpm lockfile, whichever importer installs them;
- `{"workingDirectory": "relative" | "absolute"}`, the directory qk was run
  from;
- `{"json": "path", "fields": […], "excludeFields": […]}`, only the selected
  dotted fields of a JSON file. Affected selection counts any change to the
  file.

As in Nx, a named input cannot use `dependencies` or `projects`. With a
package manager other than pnpm, lockfiles are always keyed by content.
`runtime` commands run once per run for each environment.
`dependentTasksOutputFiles` needs no file access: every dependency's
fingerprint already covers its declared outputs. `.` and `..` segments in
paths are resolved within the workspace. A symlinked input is keyed by its
target text and the content it resolves to, including the files below a
linked directory; links that resolve outside the workspace are not supported.

Extended globs (`?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)`, `(a|b)` and `{,…}`) are expanded
exactly as Nx 23 expands them, including its approximations: `+(a|b)` matches
one occurrence, and an omitted group in a directory segment widens that
segment to `*`. This keeps input sets written for Nx selecting the same files.

## Outputs

Outputs resolve as in Nx. `{options.name}` reads the target's options, with
the task's `--name=value` arguments applied, and `{projectName}`,
`{project.name}` and `{project.root}` work too; an output naming anything
without a value is left out. `!` negates an output: what it matches is
neither cached nor removed when an entry is restored. A target without
`outputs` takes `options.outputPath`, and a `build` or `prepare` target
`dist/<root>`, `<root>/dist`, `<root>/build` and `<root>/public`. Those
defaults are cached and restored, but because they are guesses they stay
inputs of other tasks, and dependents are keyed by the task's key rather
than by those files alone.

A negated group such as `src/**/!(*.test).ts` or `dist/!(cache)/**` expands
as Nx expands it, into a glob with the group widened and one naming the
group's items that excludes, so both tools select the same files, including
where Nx's expansion is surprising: `*.!(ts)` keeps only files ending in `.`,
as in Nx. Nx applies such exclusions to every pattern of a project at once;
qk applies each to the pattern it comes from, which can only add inputs.

A task with any other input declaration, or an output without a fixed
directory prefix, runs uncached and reports why. Its dependents then run uncached too, naming the task and reason
they depend on.

## Saving and restoring results

On a miss, the task runs with stdout and stderr streamed through a pipe while
they are recorded, so child processes do not see a terminal. The entry is
saved only when the task succeeds and its inputs are unchanged afterwards.
On a hit, qk removes existing files matching the declared outputs, copies the
cached outputs into place and replays the recorded stdout and stderr. As in
Nx, outputs a worktree already holds for the key are left as they are: after
each restore or save qk records, in the worktree's state, every output
path with its size, times, inode and mode, and a hit that finds exactly those
still in place only replays the log, marked
`[existing outputs match the cache, left as is]`. Restores
are staged in the worktree's state. Saving and restoring copy files,
which clones them on copy-on-write filesystems such as APFS when the cache and
the checkout share a volume. Files are never hardlinked, so editing a restored
file never changes the cache. Every file is verified
against its content hash; an unreadable or corrupt entry is a miss.
A per-key lock makes concurrent runs of the same task, including runs in
different worktrees, wait for each other and reuse the result.

See [remote caching](remote-cache.md) for size limits and cache directory overrides.
