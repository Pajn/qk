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
  and including transitive installation changes Nx can miss.
- The `pnpm-workspace.yaml` qk adds to every key leaves out its resolution
  keys, which reach tasks through the lockfile. A task whose inputs select the
  file keys it whole, as in Nx.
- Cache keys always include the task's project manifests, and each dependency
  project's `package.json` without its version, scripts and other metadata,
  even when the task's inputs leave them out. Nx keys these files only through
  declared inputs. qk keeps them so that a task's scripts, and where a
  dependency's imports resolve, cannot change without a cache miss. See
  [What changes the key](guides/cache.md#what-changes-the-key).
- A tracked upstream can adjust the affected base to avoid counting changes
  already landed upstream.
- `qk affected` selects projects unless given `--granularity task`. Nx 24
  selects tasks unless nx.json sets `affected.legacy: true`.
- Inputs marked `"always": true` read as without it: the flag only matters to
  a hash taken from an Nx Cloud Ultracache recording, which qk does not read.
- The run-commands `readyWhen` option accepts readiness text from either
  stdout or stderr.
- A target's `readyWhen` is checked only while a task in the run waits for
  it, and its verdict is not shared with other qk processes. The `local-ip`
  hosts resolve to the machine's outward address of their family, close to
  but not exactly the dev servers' choice.
- `exec --exclude-task-dependencies` runs the command in the selected
  projects; Nx's leaves every project out and runs nothing.
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
  them for every project, tagged or not. A pattern with an unknown label, such
  as `scope:shared`, is matched whole; Nx drops the label and would select a
  project named `shared`. A plain name matches as a word by its characters;
  Nx puts it unescaped into a regular expression, so `.` there matches any
  character.
- A `targetDefaults` `filter.projects` given as a string is one pattern; Nx
  spreads it into one pattern per character.
- Globs in project selectors, workspace package patterns, `targetDefaults`
  keys and project target keys do not support minimatch's extended globs
  such as `@(a|b)`: their characters match themselves.
- For affected-project selection, bare parenthesized extension groups such
  as `**/*.(svg|png)` select projects in qk that Nx 23.1.0 does not.
  See [Inputs and outputs](reference/inputs-outputs.md) for compatible syntax.

See [Affected selection](guides/affected.md), [Inputs and outputs](reference/inputs-outputs.md)
and [Running tasks](guides/running-tasks.md) for the detailed rules.

## Limits

This is a subset of the design's compatibility surface. There is no Nx
parity claim yet. In particular:

- The graph includes **workspace projects only**, as `nx graph --file`
  does. `--external` adds a node per installation in the pnpm lockfile,
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
