# Workspace configuration

qk reads `nx.json` as JSON with comments, trailing commas and `"//"` comment
keys. Unknown metadata is retained, but that does not imply qk executes it.
Use [Nx compatibility](../compatibility.md) to check the supported surface.

```jsonc
{
  "defaultBase": "main",
  "parallel": 3,
  "qk:cores": 8,
  "namedInputs": {
    "default": ["{projectRoot}/**/*"],
    "production": ["default", "!{projectRoot}/**/*.test.ts"]
  },
  "targetDefaults": {
    "build": {
      "cache": true,
      "inputs": ["production", "^production"],
      "dependsOn": ["^build"]
    }
  }
}
```

## `extends`

A file whose settings nx.json starts from, resolved as Nx resolves it with
Node's `require.resolve` from the workspace root: a path beginning `./` or
`../`, or a package subpath such as `nx/presets/npm.json`, found in
`node_modules` through the package's `exports`, whose conditions apply in the
order they are declared. A bare package name without `exports` resolves to
its `main`, else `index`. As in Nx, each setting
nx.json declares replaces the extended file's whole, and the extended file's
own `extends` is not followed. `nx.local.json` merges over the result. A
change to the extended file counts as a change to nx.json does: through what
it changes, by the merged settings (see
[What changes the key](../guides/cache.md#what-changes-the-key)).

## `namedInputs`

An object mapping names to arrays of [input declarations](inputs-outputs.md).
Defaults to an empty object. Without an explicit definition, `default` means
`{projectRoot}/**/*`. Projects override workspace definitions by name.
A named input cannot itself declare `dependencies` or `projects`.

## `targetDefaults`

An object of target defaults; defaults to empty. Matching keys are tried in
this order: executor, target name, then matching target-name globs, longest
first. A key is a glob when it holds one of `*`, `|`, `{`, `}`, `(`, `)` or
`[`, and it matches as minimatch matches: `*` does not cross a `/`. The first
key with a matching entry wins; keys do not all accumulate. As in Nx 24, the
selected defaults lie beneath the project's files, which merge over them as
described in [Merging](targets.md#merging), spreads included. Beneath a
default there is only the earlier matching entries of its key, so a spread in
the first adds nothing.

Each value may be a target object or an array of target objects. Array entries
may have `filter.projects`, an array of
[project selectors](../guides/affected.md#project-selectors), and
`filter.executor`, and other filter keys are ignored. `filter.plugin` names
the plugin in nx.json `plugins` that wrote the target; targets from
project.json and package.json have none, so an entry with `filter.plugin`
never applies. Matching entries merge in array order, later ones winning.
An entry naming an executor different from the target's own is left out,
although its key still wins.

As in Nx, a default takes the executor of the target it applies to, and a
run-commands target's command, so that the target merges over it, unless
it names its own command; a default naming another command or script is
replaced by the target whole. A default whose target the files replace
along the way still lies beneath the result; see
[Nx compatibility](../compatibility.md#deliberate-differences).

## `defaultBase`

A revision string for affected selection and remote warm state's default
branch. The fallback is `main`. Affected selection resolves `--base`, then
`NX_BASE`, then this setting. See [Affected selection](../guides/affected.md).

## `affected`

`affected.legacy: true` makes affected selection select projects rather
than tasks, as in Nx 24. `NX_LEGACY_AFFECTED` and `--granularity` override
it. See [Tasks or projects](../guides/affected.md#tasks-or-projects).

## `qk:affectedProfiles`

Named, opt-in refinements of affected project selection. Each profile
contains a `projections` array of adapters with `name`, `command`, `sources`,
`outputs` and optional `timeoutSeconds`, `reachability`, or both.
`reachability` is `true`, or `{"default": "<name>"}` naming the
`qk:reachability` config for targets that declare none.
Select a profile with `--affected-profile <name>`.
See [Project change projections](../guides/affected-projections.md) for the
protocol, configuration example, tool setup and conservative fallback rules,
and [Import reachability](../guides/affected-reachability.md) for
reachability.

## `qk:reachability`

Named import reachability configs, each an object with `anchors`, `cases`
and `sources` arrays of globs. A target's `qk:reachability` names one, is
`false`, or declares its own settings; without one, it takes its profile's
default. See [Import reachability](../guides/affected-reachability.md).

## `parallel`

A positive integer giving the default number of concurrently running tasks.
Defaults to `3`; `--parallel` and `NX_PARALLEL` take precedence. This is
separate from `options.parallel`, which runs commands within one task.

## `qk:cores`

A positive integer giving the core budget for a run. Defaults to the cores
available to qk. `--cores`, then `QK_CORES`, take precedence. Targets opt in to
sharing this budget with [`qk:threads`](targets.md#qkthreads).

## `cli.packageManager`

Selects the package manager for `nx:run-script`. Lockfile detection is used
when absent; see [Executors](executors.md#nxrun-script) for the full order.

## `cli.defaultProjectName` and `defaultProject`

Fallback project names for a target invoked without a project. The project
whose root most specifically contains the current directory normally wins.
In the root project, `NX_DEFAULT_PROJECT` can pick another. Where no project
contains the directory, qk tries `NX_DEFAULT_PROJECT`, then
`cli.defaultProjectName`, then `defaultProject`.

## `workspaceLayout`

An object with `appsDir` and `libsDir` string prefixes used to infer package
project types. Matching is by plain string prefix. Explicit `project.json`
configuration can override the inferred type.

## `pluginsConfig["@nx/js"]`

The settings Nx's JavaScript support reads, which qk reads too:

- `analyzePackageJson`: anything but `true` stops package.json dependencies
  making graph edges.
- `analyzeLockfile`: `false` leaves the installed packages out of the graph,
  so `graph --external` adds none and a lockfile change under
  `projectsAffectedByDependencyUpdates: "auto"` affects every project, where
  Nx affects none (see [Compatibility](../compatibility.md)). It defaults to
  on, unless a `lerna.json` stands without an `nx.json`.
  Cache keys and `externalDependencies` inputs read the lockfile either way.
- `projectsAffectedByDependencyUpdates`: see
  [Affected selection](../guides/affected.md).

## `cacheDirectory`

A path relative to the workspace root, or an absolute path, overriding the
default cache location. `NX_CACHE_DIRECTORY` takes precedence; without either,
`tasksRunnerOptions.default.options.cacheDirectory` is read, as in Nx. qk stores its
entries in `qk/v1` beneath this directory so it can coexist with Nx.
History and worktree state keep their default locations. A relative override
gives each worktree its own cache.

Without an override, the cache lives in `<git common dir>/qk/cache/v1`, or
`.qk/cache/v1` outside Git. Use `qk cache path` to inspect it.

## `tasksRunnerOptions`

Nx's task runners. qk runs tasks itself, so it reads only what Nx's default
runner reads from here: `default.options.cacheDirectory`, under
[`cacheDirectory`](#cachedirectory). As in Nx, `--runner` must name a runner
configured here, or `default`. A runner's `parallel` option counts in neither
Nx nor qk: Nx's own `parallel` setting replaces it.

## `maxCacheSize`

A byte count or size such as `"2GB"`. Units `KB`, `MB` and `GB` use powers of
1024; `0` means unlimited. `NX_MAX_CACHE_SIZE` takes precedence. The default
is a tenth of the disk holding the cache. Entries unused for a week are
evicted whatever the limit. See [Storage limits](../guides/remote-cache.md).

## `s3`

An optional object configuring S3-compatible remote storage. Without it qk
uses only the local cache. Put credentials in the environment or local
dotenv files rather than checked-in configuration.

```jsonc
{
  "s3": {
    "bucket": "my-task-cache",
    "region": "eu-north-1",
    "cacheKeyPrefix": "tasks/",
    "localMode": "read",
    "ciMode": "read-write"
  }
}
```

| Field | Type | Default / behavior |
| --- | --- | --- |
| `bucket` | string | Required |
| `region` | string | Required |
| `endpoint` | URL string | `https://s3.<region>.amazonaws.com` |
| `forcePathStyle` | boolean | `false`; use `true` for path-style storage |
| `cacheKeyPrefix` | string | Empty; qk appends `qk/v4/` |
| `accessKeyId` | string | Falls back to `AWS_ACCESS_KEY_ID` |
| `secretAccessKey` | string | Falls back to `AWS_SECRET_ACCESS_KEY` |
| `localMode` | string | `read-write` outside CI |
| `ciMode` | string | `read-write` in CI |
| `uploadMode` | string | `wait`; qk can detach uploads with `background` |

Modes are `read-write`, `read` (or `read-only`), and `no-cache`.
`NX_POWERPACK_CACHE_MODE` overrides the selected mode.
`uploadMode` is `"wait"` (the default) or `"background"`;
`QK_REMOTE_UPLOAD_MODE` overrides it. See [Remote caching](../guides/remote-cache.md)
for detached uploads, logs and lifecycle behavior.
`AWS_SESSION_TOKEN` supplies a temporary credential token.
`--skip-remote-cache`, `NX_SKIP_REMOTE_CACHE=true` and
`NX_DISABLE_REMOTE_CACHE=true` disable the remote store.
`encryptionKey` and SSO profiles are unsupported. Unusable remote stores
are reported; local caching continues. See [Remote caching](../guides/remote-cache.md).

## `qk:storeServer`

An optional object sending the remote cache to a
[store server](../guides/remote-cache.md#store-server) instead of S3. Nx does
not read it, so when it is set qk uses it and ignores `s3`, which can stay
for Nx's own cache plugin. The token comes from `QK_STORE_SERVER_TOKEN`.

```jsonc
{
  "qk:storeServer": { "url": "https://cache.example.com" }
}
```

| Field | Type | Default / behavior |
| --- | --- | --- |
| `url` | URL string | Required; `QK_STORE_SERVER_URL` overrides it |
| `localMode` | string | `read-write` outside CI |
| `ciMode` | string | `read-write` in CI |
| `uploadMode` | string | `wait`; qk can detach uploads with `background` |

Modes and upload modes, and the variables that override them, are as for `s3`.

## `nx.local.json`

A machine-specific overlay on `nx.json`. `targetDefaults` merge per entry,
`namedInputs` merge by name, and other settings replace their checked-in
values. Add the file to your workspace's `.gitignore`. Nx does not read it.
Every run reports the overlay, and every task's key includes what it changes,
as for nx.json.
