# Projects and discovery

qk finds the workspace root as Nx does: `NX_WORKSPACE_ROOT_PATH` when set,
else the nearest ancestor with an `nx.json` or an `nx` or `nx.bat` wrapper
script, else the topmost ancestor with Nx installed in `node_modules`. For a
workspace without Nx, it takes the nearest ancestor with
`pnpm-workspace.yaml` or a `package.json` with `workspaces`; a standalone
`project.json`, or package with an `nx` object, also works when no parent
workspace marker exists. `--workspace <path>` selects a root explicitly.

## Workspace membership

qk discovers `project.json` files, including nested projects, and package
manifests the package manager's workspaces take in, as Nx's package.json
plugin does. The workspace patterns are those of the root `package.json`
`workspaces` (an array, or an object's `packages`), `pnpm-workspace.yaml`
`packages` and `lerna.json` `packages` together; a `lerna.json` without
`packages` means `packages/*`. A pattern names directories, or package.json
files when it ends in `/package.json`. Patterns are minimatch globs without
the `dot` option, so `*` and `**` pass over directories starting with `.`.
`!` patterns exclude, and with exclusions alone every package.json not
excluded is taken in. An unreadable `pnpm-workspace.yaml` or `lerna.json` warns and
adds none.

A `package.json` beside a `project.json` is part of that project whether the
workspaces take it in or not. The root package is a project when it has an
`nx` object or an adjacent `project.json`. With
`NX_INFER_ALL_PACKAGE_JSONS=true`, a workspace without a root `package.json`
takes in every package.json, as in Nx.

Discovery reads `.gitignore` and `.nxignore` files with Git's rules, in the
workspace and, when the workspace is inside a Git repository, the
repository's `.gitignore` files above it, as Nx does. It does not follow
symbolic links, and leaves out `node_modules`, `.git`, `.nx/cache`,
`.nx/workspace-data`, `.yarn/cache` and qk's `.qk` wherever they are. Other
hidden directories remain discoverable. Unlike Nx, qk does not read the
machine's global Git excludes, or `.gitignore` files above a workspace that is
not in a Git repository.

## Configuration precedence

Package scripts become `nx:run-script` targets, except a script that an
adjacent `project.json` runs another way: with a `command`, another
executor, or run-script running another script. Package `nx` configuration
is merged over these; a target there with an `executor` or `command`
replaces the script's. Adjacent `project.json` merges over that, then
`project.local.json`. Workspace `targetDefaults` lie beneath them all.
Targets merge as described in [Merging](targets.md#merging). As in Nx, tags
merge as an ordered union, `implicitDependencies` concatenate, named inputs
merge by name, `generators` merge by generator and then option, and
`metadata` merges by field, arrays concatenating one level down; other
project fields are replaced.

A package's `metadata` is what Nx reads from it, replacing any in its `nx`
object: `targetGroups` listing its script targets under `NPM Scripts`, its
`description`, and under `js` its `name`, `version`, `exports` and `main` and
whether the package manager's workspaces take it in. Each script target's
`metadata` holds the script and the package manager's command for it. Neither
is part of a cache key.

```jsonc
{
  "name": "web",
  "projectType": "application",
  "sourceRoot": "apps/web/src",
  "tags": ["scope:web"],
  "implicitDependencies": ["codegen"],
  "targets": {
    "build": {
      "command": "echo build web",
      "outputs": ["{projectRoot}/dist"]
    }
  }
}
```

## `name`

A string. An explicit name wins over the package name; otherwise, as in Nx,
a project is named after its directory, in lower case: `libs/data/Access`
is `access`. For the workspace root qk uses the workspace directory's name.
An unnamed root `package.json` cannot be a project, as in Nx: name it, or
name the project in its `nx` object. Duplicate names fail with both roots in
the error.

## `root`

The workspace-relative project directory, with forward slashes (`.` for the
workspace root). It defaults to the configuration's directory; as in Nx, a
`root` given in project.json or the package's `nx` object is used as written,
an empty string meaning the workspace root. It must stay inside the
workspace, and two projects cannot share a root. As in Nx, the project's
package dependencies, scripts and lockfile entry then come from the
`package.json` at that root, not the one beside the configuration.

## `sourceRoot`

An optional string retained in the normalized project configuration.
Set it to the source directory to describe the project.

## `projectType`

An optional type string. `qk show projects --type` accepts `app`, `lib` and
`e2e` and applies Nx's project typing rules. Package entry points and
`workspaceLayout` also contribute to inferred types.

## `tags`

An array of strings, empty by default. Select them with `tag:<glob>`.
Packages also gain `npm:private` or `npm:public` and `npm:<keyword>` tags.

## `implicitDependencies`

An array of [project selectors](../guides/affected.md#project-selectors),
empty by default. What the selectors match, less the project itself, gains an
implicit graph edge; a `!` selector removes every edge to the projects it
matches, package dependencies included. As in Nx, a selector matching no
project is an error, and the graph's node lists the expanded names, each
exclusion as `!name`. Package dependency sections also create graph edges when
the range identifies a workspace package; see [Nx compatibility](../compatibility.md).

## `namedInputs`

An object of named input arrays, overriding workspace definitions by name.
See [Inputs and outputs](inputs-outputs.md).

## `targets`

An object mapping target names to [target definitions](targets.md).
Inspect the merged result with `qk show project web --json`.
Unknown executor names are preserved during inspection; execution rejects
unsupported executors before commands start.

## `includedScripts`

An array under the package's `nx` object restricting which package scripts
become targets. By default all scripts do. An empty array disables script
target creation. Listed names initially create targets even if the script
is absent, allowing explicit definitions to replace them; missing scripts
fail in execution preflight, not inspection.

## `qk:reachability`

A qk extension: the `anchors` and `sources` an affected profile with
`reachability` uses to leave the project out when its anchors do not import
what changed. See [Import reachability](../guides/affected-reachability.md).

## `project.local.json`

A machine-specific overlay beside `project.json` or `package.json`. It can
change options or add targets, but cannot set `name` or `root`. Add it to
your workspace's `.gitignore`. Nx does not read it.

```jsonc
{
  "targets": {
    "serve": { "options": { "port": 4300 } }
  }
}
```

qk reports each overlay used. It is included in cache keys, so overridden
tasks do not reuse the checked-in definition's cache entries.
