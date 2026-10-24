# Nx compatibility

qk reads a bounded subset of Nx 24 configuration. It runs without invoking
Nx or loading Nx plugins. The implementation, fixtures and parity tests
establish supported behavior; preserving unknown metadata during inspection
does not mean qk executes it.

## Supported executors

`nx:run-commands`, `nx:run-script` and `nx:noop` are executable.
Other executor names remain inspectable but fail execution preflight.

## Project dependencies

Package dependencies create workspace graph edges when their ranges are
`workspace:…`, `*`, a `file:` path to the package, or a semver range the
package's version satisfies (including prereleases by npm's rules).
Ranges such as `catalog:`, npm aliases, or ranges outside the workspace
version do not create an edge. Names are looked up by dependency key, so
`"ui": "workspace:@scope/ui@*"` does not link to `@scope/ui`.
When a name occurs in multiple sections, precedence is `dependencies`,
`devDependencies`, `peerDependencies`, then `optionalDependencies`. When two
packages share a name, the name means the one whose package.json path sorts
last, as in Nx. `pluginsConfig["@nx/js"].analyzePackageJson` turns these
edges off.
`implicitDependencies` adds selected project edges.

## Deliberate differences

- A pnpm lockfile change under `projectsAffectedByDependencyUpdates: "auto"`
  follows installed sets, avoiding projects whose installations are unchanged
  and including transitive installation changes Nx can miss. Task selection
  judges such a change by the same installed sets.
- `projectsAffectedByDependencyUpdates` as a list of selectors touches its
  projects only when a lockfile changed; Nx's project selection touches them on
  every change, which reads as a bug, since its task selection checks.
- nx.json counts through what it changes. Nx hashes it whole into every task,
  so any change to it, even a comment, misses every task's cache and affects
  every project and task. qk keys each task by its resolved configuration,
  which `targetDefaults` and `namedInputs` reach it through, and by the
  remaining settings that can change a task's work. A change affects every
  project and task only where it changes those settings; one to
  `targetDefaults` or `namedInputs` affects the projects whose configuration
  it changes and the tasks whose key it changes; and one to comments,
  formatting or settings such as `defaultBase` affects nothing. A tool that
  reads nx.json declares it as an input. See
  [What changes the key](guides/cache.md#what-changes-the-key).
- A change to the file nx.json `extends` counts as a change to nx.json does;
  Nx does not look at it.
- Where no input selects a project's `project.json` or `project.local.json`,
  they count through what they change. Nx hashes the whole configuration of
  every project whose inputs a task reads, so editing one target misses and
  affects every task of the project and of its dependents reading it through
  `^` inputs. qk keys each task by its resolved configuration and what
  remains of its own project's, such as its root and output paths, so the
  edit misses and affects the tasks of that target. An input selecting the
  file, as `{projectRoot}/**/*` does, keys it as in Nx; a workspace opts in
  by leaving the files out of `default`. See
  [What changes the key](guides/cache.md#what-changes-the-key).
- Task selection adds to what Nx selects every task whose qk key changed, by
  the parts of the key Nx does not hash (see below). Nx selects some tasks
  whose qk key is unchanged, such as for a changed tracked output or a
  lockfile change under `"all"`; qk selects them too, and they are cache hits.
- Cache keys leave every planned task's declared outputs out of source
  filesets, unless a task reads them from a dependency. Nx hashes a tracked
  output like any file, so a task regenerating one changes its own key.
- The root tsconfig is keyed without the version of the installed
  `typescript` package Nx hashes beside it; qk keys what the task's importers
  install instead.
- The `pnpm-workspace.yaml` qk adds to every key leaves out its resolution
  keys, which reach tasks through the lockfile. A task whose inputs select the
  file keys it whole, as in Nx.
- Cache keys always include the task's project's `package.json`, what its
  `project.json` decides for the task, and each dependency project's
  `package.json` without its version, scripts and other metadata, even when
  the task's inputs leave them out. Nx keys these files only through
  declared inputs. qk keeps them so that a task's scripts, and where a
  dependency's imports resolve, cannot change without a cache miss. See
  [What changes the key](guides/cache.md#what-changes-the-key).
- A tracked upstream can adjust the affected base to avoid counting changes
  already landed upstream.
- Deleting a project's manifests affects the projects that depended on it,
  read from its manifests at the base revision, where Nx affects every
  project. See [How changes reach projects](guides/affected.md#how-changes-reach-projects).
- `qk affected` takes `--granularity task|project`, which comes before
  `NX_LEGACY_AFFECTED` and nx.json `affected.legacy`. Nx has no such option.
  See [Tasks or projects](guides/affected.md#tasks-or-projects).
- Nx's Ultracache keys tasks by recordings Nx Cloud makes, which qk cannot
  read. qk's `qk:ultracache` records tasks itself, with `--record` on Linux,
  and keys them by the files they read, the directories they listed and the
  paths they looked up, rather than by globs. Nx's target `ultracache` field
  is read and merged, but has no effect. Inputs marked `"always": true` count
  for a task keyed by its recordings, and read as without the flag otherwise.
  See [Keying tasks by what they read](guides/ultracache.md).
- A target's `readyWhen` is checked only while a task in the run waits for
  it, and its verdict is not shared with other qk processes. The `local-ip`
  hosts resolve to the machine's outward address of their family, close to
  but not exactly the dev servers' choice.
- `exec --exclude-task-dependencies` runs the command in the selected
  projects; Nx's leaves every project out and runs nothing.
- From a package script, `exec` runs the script's target with the run
  options it was given, and passes the arguments beyond the script's own to
  the task. Nx runs `nx run` for it without the options, and reads those
  arguments as its own options.
- When a command that runs tasks is given both options it does not know and
  arguments after `--`, qk forwards both, the unknown options first. Nx
  forwards only what follows `--` and drops the others.
- `qk run <name> --target <target>` runs the target on project `<name>`;
  Nx ignores the positional and uses the current directory's project.
- Under `--nx-bail`, a run that failed and left tasks unstarted exits with 1.
  Nx counts it as interrupted and exits with 130.
- qk's default output styles are its own: `static` in CI, the live panel on
  a terminal and `quiet` otherwise for several tasks, and raw output for one.
  Nx defaults to `static-failures-only` where it does not use its terminal UI,
  and to `summary` when driven by an AI agent.
- `--output-style=summary` writes the output of failed tasks only, to the
  worktree's state directory; Nx writes every task's output to its cache
  directory.
- `project.local.json`, `nx.local.json`, `qk:threads`, `qk:warm`,
  `qk:affectedProfiles`, `qk:reachability` and a target-level `env` are qk
  extensions. Nx does not apply them.
- A target default stays beneath a target the project's files replace along
  the way, for instance when `includedScripts` makes a script target that
  project.json gives a command. Nx stamps the default with the executor the
  files end with, so the script target in between replaces it and the
  default is lost, unlike when the script is left out.
- Two run-commands targets whose `command` is an array merge when the arrays
  are equal. Nx compares arrays by identity, so they never do.
- run-script quotes each of the task's arguments for the shell. Nx joins them
  as they are, so an argument with a space becomes several.
- Options a dependency forwards with `options: "forward"` are taken as they
  are. Nx interpolates them again, which only normalizes as a path the text
  of a value holding braces, turning `http://x/{args.y}` into
  `http:/x/{args.y}`.
- Tokens in a task's arguments resolve with `/` separators on every
  platform; Nx joins them with `\` on Windows.
- Input negated-group exclusions apply to the pattern they came from in qk;
  Nx applies them across a project's patterns. qk can include extra inputs.
- A `json` input naming one file reads it even when source ignore rules
  exclude it; Nx reads only the files a fileset could select, so qk can key
  a file Nx leaves out.
- An `includeIgnored` fileset leaves out the task's own declared outputs, as
  every qk fileset does, so a task that writes into what it reads can still
  be cached. Nx keys them.
- In filesets, `json` inputs, `includeIgnored` filesets and outputs,
  `{projectRoot}` and `{projectName}` go in with their glob characters
  escaped, so a project at `apps/(marketing)` reads and caches that
  directory. So do the project's values in an output's `{project.…}` and the
  root in the outputs a target without `outputs` takes. Nx inserts them as
  they are, which turns such a root into a pattern that misses the project's
  files.
- `pnpm-workspace.yaml` and `lerna.json` name workspace packages in a
  workspace without a root `package.json` too, and package.json dependencies
  make edges there; Nx then reads no workspace patterns and analyses no
  package.json.
- With `analyzeLockfile: false`, a lockfile change under
  `projectsAffectedByDependencyUpdates: "auto"` affects every project even
  when no package changed, where Nx affects none; cache keys and
  `externalDependencies` inputs still read the lockfile.
- Project discovery ignores the machine's global Git excludes, and the
  `.gitignore` files above a workspace that is not in a Git repository;
  Nx reads both, so what they hide depends on the machine.
- An unnamed project at the workspace root is named after the workspace
  directory; Nx names it `.`. Two configurations giving the same `root` are
  an error, where Nx merges them into one project, and a `root` outside the
  workspace is refused.
- Project selectors read `tag:*` and `directory:*` as globs, where Nx takes
  them for every project, tagged or not.
- An atomized target, one naming a `metadata.nonAtomizedTarget`, reads its
  own `.env.<target>` files; Nx reads those of the first atomized target in
  its group for every target of the group.
- With `NX_LOAD_DOT_ENV_FILES=false`, a task keeps a shell variable whose
  value a root `.env` file repeats. Nx removes it, unloading files it never
  loaded.
- A `targetDefaults` `filter.projects` given as a string is one pattern; Nx
  spreads it into one pattern per character.
- Globs in project selectors, workspace package patterns, `targetDefaults`
  keys, project target keys and `dependsOn` targets do not support
  minimatch's extended globs such as `@(a|b)`: their characters match
  themselves.
- For affected-project selection, bare parenthesized extension groups such
  as `**/*.(svg|png)` select projects in qk that Nx 23.1.0 does not.
  See [Inputs and outputs](reference/inputs-outputs.md) for compatible syntax.

See [Affected selection](guides/affected.md), [Inputs and outputs](reference/inputs-outputs.md)
and [Running tasks](guides/running-tasks.md) for the detailed rules.

## Limits

This is a subset of the design's compatibility surface. There is no Nx
parity claim yet. In particular:

- The graph includes **workspace projects only**, as `nx graph --file`
  does, and is written as JSON only: qk has no HTML graph viewer, so
  `graph --file <name>.html` fails. `--external` adds a node per installation in the pnpm lockfile,
  `npm:<name>@<version>` with peers and patch in the version, with edges from
  projects to what they install directly and between installations.
- Nx plugins and inferred targets are outside the design's scope.
- qk has its own live output panel, which takes no input, and does not
  implement Nx release commands. Nx adds an `nx-release-publish` target to
  each public package when `@nx/js` is installed; qk does not, as its
  executor is a release executor qk cannot run.
- An `externalDependencies` entry naming a package nothing installs is an
  error only with a pnpm lockfile qk can read; with any other lockfile qk
  cannot tell, and keys the lockfile whole.
- Pseudo-terminals are Unix only; on Windows commands see pipes, as they do
  in Nx unless `NX_WINDOWS_PTY_SUPPORT` is set. A pseudo-terminal passes `\n`
  through, where Nx's turns it into `\r\n`.

The compatibility baseline is documented in Nx's
[project configuration](https://nx.dev/docs/reference/project-configuration)
and [workspace configuration](https://nx.dev/docs/reference/nx-json)
references. The bounded subset above and fixture tests define qk's current
behavior; new Nx features are not automatically supported.
