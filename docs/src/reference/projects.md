# Projects and discovery

qk searches ancestors for `nx.json`, `pnpm-workspace.yaml`, or a root
`package.json` with `workspaces`. A standalone `project.json`, or package
with an `nx` object, also works when no parent workspace marker exists.
`--workspace <path>` selects a root explicitly.

## Workspace membership

qk discovers `project.json` files, including nested projects, and package
manifests selected by pnpm workspace globs or `package.json` workspaces.
pnpm patterns take precedence; exclusion globs win. An explicit
`project.json` remains a project outside package globs.
The root package is a project when it has an `nx` object or adjacent
`project.json`.

Discovery respects workspace ignore files, skips symlink directories, and
excludes `node_modules`, `.git`, `.nx`, `.qk`, `.pnpm-store`, and the root
Cargo `target` directory. Parent and global Git ignore rules do not affect
it. Other hidden project directories remain discoverable.

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

A nonempty string. An explicit name wins over the package name; otherwise
qk uses the relative directory with `/` replaced by `-`. For the workspace
root it uses the directory's name. Duplicate names fail with both roots in
the error. Use the normalized name in CLI selectors.

## `root`

The workspace-relative configuration directory, normalized with forward
slashes (`.` for the workspace root). An explicit root must agree with that
directory; an empty string is also accepted for the workspace root.

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
