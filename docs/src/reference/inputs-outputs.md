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

- filesets, as strings or `{"fileset": "…"}`, with `!` exclusions, and
  `{"fileset": "…", "includeIgnored": true}`, which reads files on disk that
  ignore rules leave out;
- named inputs, also as `{"input": "name"}`;
- `^name` and `{"input": "name", "dependencies": true}` (or `"projects":
  "dependencies"`), which as in Nx cover every project the project depends
  on, directly or not, and `^{projectRoot}/…`, `^{workspaceRoot}/…` or
  `{"fileset": "…", "dependencies": true}`, a fileset of each of them;
- `{"input": "name", "projects": […]}`, the named input of the projects the
  names, globs or `tag:` patterns select, and with `"dependencies": true`
  their `^name` instead, as in Nx 24: every project they depend on, but not
  themselves;
- `env`, `runtime`, `dependentTasksOutputFiles` with `transitive`, and
  `externalDependencies`, which adds every installation of the named
  packages in the pnpm lockfile, whichever importer installs them;
- `{"workingDirectory": "relative" | "absolute"}`, the directory qk was run
  from;
- `{"json": "path", "fields": […], "excludeFields": […]}`, only the selected
  dotted fields of a JSON or YAML file, or of each file a glob selects.

As in Nx, a string names a named input when one of that name exists, and is
a fileset otherwise. A fileset, like a `json` path, starts from
`{projectRoot}` or `{workspaceRoot}`. `{projectRoot}` and `{projectName}`
are replaced anywhere in it; `{workspaceRoot}` only at the start, and
anywhere else it is glob text, as in Nx. A fileset starting from
`{projectRoot}/` selects the project's files, and one from
`{workspaceRoot}/` the workspace's. Write globs with `/` on every platform:
`\` escapes the character after it, so `{projectRoot}/app/\\(marketing\\)/**/*`
in JSON selects the `app/(marketing)` directory. A path with no glob
pattern names that file, or that directory and everything under it, so
`{projectRoot}/src`, `{projectRoot}/src/` and `{projectRoot}/src/**/*`
select the same files.

As in Nx, a named input cannot use `dependencies` or `projects`, an input
with `"always": true` and a `projects` pattern that matches no project is an
error, and so is an `externalDependencies` entry naming a workspace project
or, with a pnpm lockfile qk can read, a package nothing installs. With a
package manager other than pnpm, lockfiles are always keyed by content.
The [runtime input environment](../guides/cache.md) determines which tasks can
share a runtime evaluation.
`dependentTasksOutputFiles` selects matching files from completed dependency
outputs rather than their full output fingerprints. `.` and `..` segments in
paths are resolved within the workspace. A symlinked input is keyed by its
target text and the content it resolves to, including the files below a
linked directory; links that resolve outside the workspace are not supported.

As in Nx, a project's filesets select only the files the project owns, each
file belonging to the project whose root most specifically contains it. A
project nested in another's root owns its files, so they reach neither the
outer project's `{projectRoot}/**/*` nor, for a root project at `.`, its
`default`; select files outside a project's own with a `{workspaceRoot}`
fileset.

File exclusions apply to every inclusion in the same input scope, regardless of
declaration order. Workspace filesets and each project's filesets have separate
scopes: an exclusion in `^production` cannot remove a file selected explicitly
by a `{workspaceRoot}` fileset. Generated files selected by source filesets are
keyed after dependencies complete, independently of `dependentTasksOutputFiles`;
a task's own declared outputs stay out of its source inputs.

Extended globs (`?(…)`, `*(…)`, `+(…)`, `@(…)`, `!(…)`, `(a|b)` and `{,…}`) are expanded
into plain globs. Some expansions follow Nx 23's approximations: `+(a|b)`
matches one occurrence, and an omitted group in a directory segment widens
that segment to `*`.

Bare parenthesized alternation differs for affected-project selection. qk
selects projects with `**/*.(svg|png)` inputs when SVG or PNG files change,
while Nx 23.1.0's affected-project matcher treats that group as literal text.
Nx's native cache hasher does expand the group. Use `**/*.{svg,png}` for
consistent hashing and affected selection in both tools.


### `includeIgnored`

A fileset only sees files that `.gitignore` and `.nxignore` do not exclude.
With `"includeIgnored": true` it reads the disk instead, whether Git tracks a
file or not, as in Nx 24:

```jsonc
{
  "inputs": [
    { "fileset": "{projectRoot}/generated/**/*.json", "includeIgnored": true },
    { "fileset": "!{projectRoot}/generated/**/*.local.json", "includeIgnored": true },
    { "fileset": "{workspaceRoot}/openapi.generated.json", "includeIgnored": true }
  ]
}
```

Each glob starts from `{projectRoot}` or `{workspaceRoot}`; one that climbs
out with `..`, has a `.` segment or names an absolute path is an error. A
glob is read from its leading directory: with no pattern after it, it names
that file, or that directory and everything under it, and a path that names
a missing file changes the key when the file appears. A glob read from
inside a project matches only that project's files, not those of a project
nested below it, and one read from outside every project matches all it
names. A walk does not enter `node_modules`, `.git`, `.nx/cache`,
`.nx/workspace-data`, `.yarn/cache` or `.qk` unless it starts inside one,
skips the timestamp files Vite and Vitest write beside their config, and
does not enter a linked directory it meets. A linked file, or a directory
named exactly, is read wherever it points.

A `!` negation with `includeIgnored` filters every `includeIgnored` glob
resolved with it: the project's own, or, with `"dependencies": true`, the
globs carrying that flag, applied together to each project the project
depends on, directly or not. A negation with nothing to filter is an error.

Such a fileset reading another task's declared outputs makes that task's
changes carry to this one in affected selection, as `dependentTasksOutputFiles`
does. Affected selection matches a changed path against the globs, so a
deleted file counts. Unlike Nx, the task's own declared outputs stay out of
these inputs, as they do for every fileset.

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
`transitive: true` also includes outputs of their dependencies. As in Nx, a
dependency without declared outputs offers none, and other artifacts do not
change this input's key. Multiple patterns are combined. This and filesets
selecting a dependency's outputs are the only ways a dependency task keys a
task, and the only ways a change to it makes the task affected.

### `workingDirectory`

`{"workingDirectory": "relative"}` or `"absolute"` includes the directory
qk was invoked from in the key.

### `json`

`{"json": "path", "fields": ["compilerOptions"], "excludeFields": ["scripts"]}`
selects dotted fields of a JSON or JSONC file. A path with no glob pattern is
read even when source ignore rules exclude it, which Nx does not do. A glob
selects, as in Nx, among the files a fileset from the same root would: the
project's own, or the workspace's from `{workspaceRoot}/`. A file named `.yaml` or `.yml` is read as YAML, which Nx
does not do: Nx keys such a file by its whole content, so a key qk leaves
unchanged can still change Nx's. As in Nx, a file qk cannot read is keyed by
its whole content, without the field filters.

Affected task selection compares the selected fields at both revisions, so a
change to fields the input leaves out does not affect the task. A change it
cannot compare counts, such as an added or deleted file, or one it cannot read
at either revision. Project selection, as in Nx, does not read `json`
inputs: a changed file touches the project that contains it, if any.

## `outputs`

Outputs resolve as in Nx 24. `{projectRoot}` and `{projectName}` name the
project, `{options.name}` reads the target's options, with the task's
`--name=value` arguments applied, and the legacy `{project.<path>}` reads
the project's configuration, such as `{project.sourceRoot}`. A placeholder
can fall back, left to right, to others and finally to a literal:
`{options.outputPath ?? options.outDir ?? {projectRoot}/dist}`. A value is
written as JavaScript writes it, so `false` is `false`, `0` is `0` and an
array is its items joined with `,`; only a missing option, `null` and `""`
have no value. Option values have `{projectRoot}`, `{projectName}` and a
leading `{workspaceRoot}` resolved first, as Nx does when it builds the
project graph. Braces that do not start with a token are glob text, so
`{projectRoot}/dist/**/*.{js,map}` is one output, and so is `{env.NAME}`:
environment variables are no placeholder in outputs. As in inputs, `\`
escapes a glob character, so `{projectRoot}/app/\\(group\\)` in JSON names
the `app/(group)` directory. The project's root and name go in as the
paths they are, so `{projectRoot}/dist` of a project at `apps/(group)` names
`apps/(group)/dist`, which Nx does not do; an option's value goes in as
written, a pattern if it holds one.

An output whose `{options.…}` placeholder, or a `{projectRoot}` or
`{workspaceRoot}` left as text, has no value is left out. `{workspaceRoot}`
may only start an output, and an output from it must name a directory
before any pattern: `{workspaceRoot}/dist/**`, not `{workspaceRoot}/**`. An
absolute output, written so or from an option, is read relative to the
workspace, and one outside the workspace is an error. `!` negates an output:
what it matches is neither cached nor removed when an entry is restored. A
target without `outputs` takes `options.outputPath`, and a `build` or
`prepare` target `dist/<root>`, `<root>/dist`, `<root>/build` and
`<root>/public`. Those defaults are cached and restored, but because they are
guesses they are not declared outputs: they stay source files that other
tasks' filesets can select, and `dependentTasksOutputFiles` selects none of
them.

A negated group such as `src/**/!(*.test).ts` or `dist/!(cache)/**` expands
as Nx expands it, into a glob with the group widened and one naming the
group's items that excludes, so both tools select the same files, including
where Nx's expansion is surprising: `*.!(ts)` keeps only files ending in `.`,
as in Nx. Nx applies such exclusions to every pattern of a project at once;
qk applies each to the pattern it comes from, which can only add inputs.

A task with any other input declaration, or an output without a fixed
directory prefix, runs uncached and reports why.


Use explicit output declarations for build artifacts. A cache hit removes
existing matching outputs before restoring the saved result; negated outputs
are preserved. An output pattern without a fixed directory prefix disables
caching, with a reported reason.

Root `.gitignore` and `.nxignore` rules exclude files from source input discovery,
including tracked files. Negations use Git ignore semantics. Watch uses the same
rules, and project affected selection also applies the root ignore files. qk still hashes
mandatory workspace/project configuration independently of source filesets,
including the root `.gitignore` and `.nxignore` files themselves.
Task-level affected selection follows these cache inputs, including explicitly
declared JSON inputs and mandatory configuration hidden by source ignore rules.
