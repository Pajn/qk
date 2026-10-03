# Inputs and outputs

Inputs describe what changes a task's result. Outputs describe files to
save and restore. [Local caching](../guides/cache.md) explains the complete
key, including the root workspace files and manifests always included.

## `inputs`

Without an explicit array, a task uses `default` and `^default`.
Unless overridden by a named input, `default` means `{projectRoot}/**/*`.

```jsonc
{
  "inputs": [
    "production",
    "^production",
    { "env": "NODE_ENV" },
    { "runtime": "node --version" }
  ],
  "outputs": ["{projectRoot}/dist"]
}
```

Supported declarations:


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
`dependentTasksOutputFiles` selects matching files from completed dependency
outputs rather than their full output fingerprints. `.` and `..` segments in
paths are resolved within the workspace. A symlinked input is keyed by its
target text and the content it resolves to, including the files below a
linked directory; links that resolve outside the workspace are not supported.

File exclusions apply to every inclusion in the same input scope, regardless of
declaration order. Workspace filesets and each project's filesets have separate
scopes: an exclusion in `^production` cannot remove a file selected explicitly
by a `{workspaceRoot}` fileset. Generated files selected by source filesets are
keyed after dependencies complete, independently of `dependentTasksOutputFiles`;
a task's own declared outputs stay out of its source inputs.

Extended globs (`?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)`, `(a|b)` and `{,…}`) are expanded
exactly as Nx 23 expands them, including its approximations: `+(a|b)` matches
one occurrence, and an omitted group in a directory segment widens that
segment to `*`. This keeps input sets written for Nx selecting the same files.


### `env` and `runtime`

`{"env": "NAME"}` keys the declared variable's value. Dotenv file content
is not automatically keyed. `{"runtime": "command"}` keys command output;
commands run once per run for each environment.
Task-level affected selection treats both as unchanged because it cannot
recover the base revision's environment.

### `externalDependencies`

`{"externalDependencies": ["typescript"]}` adds every installation of the
named packages in the pnpm lockfile, whichever importer installs them.
The task's root/project/transitive importer installations are already keyed.

### `dependentTasksOutputFiles`

`{"dependentTasksOutputFiles": "**/*.d.ts", "transitive": true}` is
selects matching files from declared outputs of direct task dependencies.
`transitive: true` also includes outputs of their dependencies. Other artifacts
do not change this input's fingerprint. Multiple patterns are combined. Without
an output input declaration, qk retains its normal dependency fingerprints.
Dependencies that cannot be fingerprinted still make the consumer run uncached.

### `workingDirectory`

`{"workingDirectory": "relative"}` or `"absolute"` includes the directory
qk was invoked from in the key.

### `json`

`{"json": "path", "fields": ["compilerOptions"], "excludeFields": ["scripts"]}`
selects dotted fields of a JSON file. Affected selection conservatively
counts any change to the file.

## `outputs`

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


Use explicit output declarations for build artifacts. A cache hit removes
existing matching outputs before restoring the saved result; negated outputs
are preserved. An output pattern without a fixed directory prefix disables
caching, with a reported reason.

Root `.gitignore` and `.nxignore` rules exclude files from source input discovery,
including tracked files. Negations use Git ignore semantics. Watch uses the same
rules, and affected selection also applies the root ignore files. qk still hashes
mandatory workspace/project configuration independently of source filesets,
including the root `.gitignore` and `.nxignore` files themselves.
