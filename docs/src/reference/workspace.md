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
change to an extended file inside the workspace affects every project and
every task's key, as a change to nx.json does. It is keyed by its real path,
so a package linked into `node_modules`, as pnpm links them, is keyed where
it is installed.

## `namedInputs`

An object mapping names to arrays of [input declarations](inputs-outputs.md).
Defaults to an empty object. Without an explicit definition, `default` means
`{projectRoot}/**/*`. Projects override workspace definitions by name.
A named input cannot itself declare `dependencies` or `projects`.

## `targetDefaults`

An object of target defaults; defaults to empty. Matching keys are tried in
this order: executor, target name, then matching target-name globs, longest
first. The first key with a matching entry wins; keys do not all accumulate.
Project target definitions override the selected defaults.

Each value may be a target object or an array of target objects. Array entries
may have `filter.projects` (names, globs, `tag:` and `!` selectors),
`filter.plugin` (`nx/core/project-json` or `nx/core/package-json`), and
`filter.executor`. Matching entries merge in array order, later ones winning.
An entry naming an executor different from the target's own is left out,
although its key still wins.

Options merge by key and each named configuration merges by key. Nested
values and arrays are replaced. Changing executor drops options and
configurations belonging to the previous executor.

## `defaultBase`

A revision string for affected selection and remote warm state's default
branch. The fallback is `main`. Affected selection resolves `--base`, then
`NX_BASE`, then this setting. See [Affected selection](../guides/affected.md).

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

## `cacheDirectory`

A path relative to the workspace root, or an absolute path, overriding the
default cache location. `NX_CACHE_DIRECTORY` takes precedence. qk stores its
entries in `qk/v1` beneath this directory so it can coexist with Nx.
History and worktree state keep their default locations. A relative override
gives each worktree its own cache.

Without an override, the cache lives in `<git common dir>/qk/cache/v1`, or
`.qk/cache/v1` outside Git. Use `qk cache path` to inspect it.

## `maxCacheSize`

A byte count or size such as `"2GB"`. Units `KB`, `MB` and `GB` use powers of
1024; `0` means unlimited. `NX_MAX_CACHE_SIZE` takes precedence. The default
is a tenth of the disk holding the cache. See [Storage limits](../guides/remote-cache.md).

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
| `cacheKeyPrefix` | string | Empty; qk appends `qk/v1/` |
| `accessKeyId` | string | Falls back to `AWS_ACCESS_KEY_ID` |
| `secretAccessKey` | string | Falls back to `AWS_SECRET_ACCESS_KEY` |
| `localMode` | string | `read-write` outside CI |
| `ciMode` | string | `read-write` in CI |

Modes are `read-write`, `read` (or `read-only`), and `no-cache`.
`NX_POWERPACK_CACHE_MODE` overrides the selected mode.
`AWS_SESSION_TOKEN` supplies a temporary credential token.
`--skip-remote-cache`, `NX_SKIP_REMOTE_CACHE=true` and
`NX_DISABLE_REMOTE_CACHE=true` disable the remote store.
`encryptionKey` and SSO profiles are unsupported. Unusable remote stores
are reported; local caching continues. See [Remote caching](../guides/remote-cache.md).

## `nx.local.json`

A machine-specific overlay on `nx.json`. `targetDefaults` merge per entry,
`namedInputs` merge by name, and other settings replace their checked-in
values. Add the file to your workspace's `.gitignore`. Nx does not read it.
Every run reports the overlay and every task's key includes it.
