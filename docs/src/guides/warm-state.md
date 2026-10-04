# Reusing warm state

A target can keep scratch state that makes it faster to rerun, without that
state ever being part of a result, with a `qk:warm` key at target level
(beside `inputs` and `outputs`, not in `options`; Nx ignores it):

```jsonc
"tsc": { "qk:warm": { "outputs": true } },
"build-android": {
  "qk:warm": {
    "paths": ["{projectRoot}/node_modules/.cache/babel"],
    "env": { "METRO_CACHE_DIR": "{warm}/metro" },
    "maxSize": "2GB"
  }
}
```

- `outputs: true` restores the task's previous outputs before it runs, so
  an incremental tool finds its last build, such as `tsc --build` its
  `tsbuildinfo`.
- `paths` are workspace scratch paths, kept beside the task's entries. They
  are never inputs. Like outputs, they may be globs with a fixed directory
  prefix, and a path starting with `!` excludes what it matches from the
  saves, such as packaged artifacts that every build rewrites; excluded
  files are not inputs either.
- `env` sets variables for the task, with absolute directory tokens
  `{projectRoot}`, `{workspaceRoot}` and `{warm}`, a directory qk keeps for the task in the worktree's state,
  outside the working tree. Under Nx these variables are not set, so tools
  that only cache when told to keep their default behaviour there.

Warm state is restored before the task runs, on a cache miss and for
targets that are not cacheable, and never on a hit. A group already present
on disk is left alone, since it is the newest for that checkout; otherwise
it comes from the worktree's own save. With `portable: true`, it can also
come from the store linked worktrees share or from the remote store. Each worktree
keeps its own save, and a restore takes the worktree's own before the most
recent other worktree's; the eight most recent saves of a task are kept.
With sharing enabled and no suitable local save, it comes from the remote store: the current branch's
save, else the default branch's (nx.json `defaultBase`, else `main`). The
branch comes from `GITHUB_HEAD_REF` or `GITHUB_REF_NAME` in CI, else from
git; a checkout with no branch reads the default branch's state but saves
none remotely. `remote: false` keeps a target's warm state local. It is
saved after successful runs only; files unchanged since the last save or
restore are recognised by their metadata and not read again.
New warm files are copied with bounded concurrency and left to the operating
system to flush to disk. Warm state is rebuildable: a power loss may discard
a save. Blobs are still verified before restoration, and a failed warm
restore does not prevent the target from running.
The timestamp and path policies below distinguish reusable tool state from
verified result outputs.

## Result hits and warm state

A result-cache hit means qk has already verified the task's inputs and can
restore its outputs without running it. Restored result files receive the
time restoration started, after the dependencies finished. This lets tools
such as `tsc --build` see valid outputs newer than their inputs. Outputs
already held unchanged on disk retain their timestamps unless dependency
outputs were renewed.

Warm state helps when the task must run. It can belong to an older input
set, so qk dates restored files to the Unix epoch by default and lets the
tool validate them. `mtimes: "preserve"` applies only to the worktree that
saved the state. Another worktree's or a remote save still receives epoch
timestamps; this setting never promises an up-to-date build in a different
checkout. Preserving times also preserves the tool's timestamp assumptions:
Make 3.81 can miss a rapid source edit within its output timestamp's
one-second resolution. Validate source changes as well as unchanged builds
before using preserved times for that tool.

## Choosing state for a tool

Separate final deliverables from the tool's build history. Restoring an
executable or library does not restore Cargo's fingerprints, Ninja's log,
or Xcode's build database. Those tools can rebuild even when the final
artifact has a fresh timestamp.

The following guidance comes from local restoration and relocation checks
with TypeScript 6.0, Make 3.81, Ninja 1.10, CMake 4.4, Xcode 27, Cargo 1.98,
Metro 0.84, Gradle 9.3, Jest 30.5 with babel-jest, and Vitest 5.0.2. Tool versions, plugins, generated rules and compiler
flags can change whether state relocates. The configurations are starting
points to verify on your project.

| Tool and state | Same-worktree configuration | Different checkout path |
| --- | --- | --- |
| TypeScript build outputs and `.tsbuildinfo` | `outputs: true`; keep default epoch times; opt into `portable: true` after relocation checks | Relative build-info paths relocated and sources were revalidated. Custom generators or absolute paths need their own checks. |
| Make objects | Complete object/output paths, `mtimes: "preserve"`, `portable: false` | Relative rules rebuilt correctly, but epoch objects did not skip compilation. |
| Ninja objects and logs | Complete build directory, `mtimes: "preserve"`, `portable: false` | Relative rules rebuilt correctly. Absolute paths in generated rules can bind the state to the old checkout. |
| CMake build directory | Complete build directory, `mtimes: "preserve"`, `portable: false` | `CMakeCache.txt` retained the original source and build paths; relocation failed. Configure a new build directory. |
| Xcode build intermediates | Explicit intermediate/DerivedData paths, `mtimes: "preserve"`, `portable: false` | A relative-source project rebuilt correctly but compiled again. CMake-generated projects also retained CMake's absolute paths. |
| Cargo target directory | Actual target directory, `mtimes: "preserve"`, `portable: false` | A simple crate rebuilt correctly, but was `Dirty` rather than `Fresh`. Build scripts and generated absolute paths need separate checks. |
| Metro transform cache | `portable: true` after validation; default epoch times; configure the tool's cache store | Reused transforms at a different project root with the same transformer/configuration. Its file-map cache is separate. |
| Jest transform cache | Configure `cacheDirectory`; default epoch times worked; `portable: false` | babel-jest transformed modules again because the configuration/cache identity included checkout paths. |
| Vitest filesystem module cache | Enable `fsModuleCache`, configure `fsModuleCachePath`; default epoch times worked; `portable: false` | Module-cache keys included absolute module IDs and the Vite root; transforms ran again. |
| Gradle project history and build outputs | Complete local state, `mtimes: "preserve"`, `portable: false` | A Java project rebuilt correctly; copied history alone did not make compilation up to date. Native/CMake state can impose stricter path requirements. |

### Jest and Vitest

Warm transform caches avoid repeating compilation while the test runner still
executes tests. In controlled two-module fixtures, Jest/babel-jest and Vitest
with filesystem module caching each performed two transforms on a cold run,
zero after restoring the same checkout's cache, and one after changing a
source module. Both epoch and preserved timestamps worked; preserving times
provided no extra transform hits. Every run executed the test and asserted
the current source value.

Linked-worktree and remote restores at a different checkout path each
performed both transforms again. These caches therefore use
`portable: false` in the recipes below. Remote checks used an isolated local
S3-protocol store on the same platform with empty local cache directories;
they do not establish portability across machines or operating systems.

For Jest, configure its [cache directory](https://jestjs.io/docs/configuration#cachedirectory-string)
explicitly rather than trying to capture the operating system's temporary
directory:

```js
// jest.config.cjs
module.exports = {
  cacheDirectory: '<rootDir>/.cache/jest',
};
```

```jsonc
"test": {
  "qk:warm": {
    "paths": ["{projectRoot}/.cache/jest"],
    "portable": false
  }
}
```

For Vitest, a Vite cache directory alone did not persist source transforms
between processes in the tested installation. Enable the
[filesystem module cache](https://vitest.dev/config/fsmodulecache)
explicitly; defaults and option locations vary by version. Vitest 5 uses
`test.fsModuleCache` and `test.fsModuleCachePath`; versions that expose them
under `test.experimental` need that nesting instead.

```js
// vitest.config.mjs
export default {
  test: {
    fsModuleCache: true,
    fsModuleCachePath: '.cache/vitest-modules',
  },
};
```

```jsonc
"test": {
  "qk:warm": {
    "paths": ["{projectRoot}/.cache/vitest-modules"],
    "portable": false
  }
}
```

Ignore these scratch directories in Git. In multi-project configurations,
use the actual Jest root or Vitest cache root when declaring the warm path.
Vitest plugins that read external files or options may need a
[cache key generator](https://vitest.dev/config/fsmodulecache#known-issues)
or an opt-out to invalidate transforms correctly. Include those inputs in
qk's task inputs too: task-result invalidation and transform-cache
invalidation each need to cover them.

For example, keep Cargo's local target directory with the worktree:

```jsonc
"compile-rust": {
  "executor": "nx:run-commands",
  "options": { "command": "cargo build --target-dir target" },
  "qk:warm": {
    "paths": ["{workspaceRoot}/target"],
    "mtimes": "preserve",
    "portable": false,
    "key": ["{workspaceRoot}/rust-toolchain.toml", "{workspaceRoot}/Cargo.lock"]
  }
}
```

Use the directory the command actually writes; for a task with a project
working directory, `target` may need `{projectRoot}` instead. Include every
intermediate and history file needed by the tool. Excluding final
artifacts from warm paths is reasonable only when the tool can regenerate
them while reusing the remaining state.

`survive` keeps local state across a dependency that deletes its directory,
without resetting its timestamps. Combine it with local-only state for
native prebuilds. Use a `key` for the relevant toolchain and configuration;
do not use `restoreKeys` to relax a compatibility boundary that the tool
cannot validate.

## Sharing state across worktrees and CI

`portable: false` is the default: warm state is saved and restored only in
its own worktree. Set `portable: true` to permit another worktree's save and,
when `remote` is enabled, remote exchange. It does not rewrite absolute paths
inside files or make incompatible tools accept them. `remote: false` disables
remote exchange while still permitting other worktrees when `portable` is true.

**Migration:** configurations that previously relied on implicit cross-worktree
or remote warm-state sharing must now set `portable: true` explicitly. Validate
the tool's relocation behavior before opting in. Same-worktree warm restores
and local/remote task-result caching keep their existing behavior.

Prefer a tool's content-addressed compiler/transform/task cache when the
full build directory is tied to a checkout. ccache and Gradle's task-output
cache are separate from the local build history in the table above.
A `group` can keep a shared compiler cache, but cannot also declare
`paths` or `outputs`. Use separate targets when the two kinds of state need
different portability policies.

Before enabling shared warm state:

1. Build in one worktree and remove the intended warm paths.
2. Verify an own-worktree restore, including which compiler/transform steps
   the tool actually runs rather than qk's replayed log.
3. Restore into a worktree at another absolute path. Change a source there
   and verify the resulting artifact uses that source, not the first
   checkout's files.
4. Repeat with an empty local qk store and remote warm state. Match the
   operating system, architecture, toolchain and relevant configuration,
   then vary whichever dimensions the cache is intended to support.

Controlled same-platform S3-protocol checks restored TypeScript build
information, Metro transforms and Cargo state to another path. Metro reused
its transforms; TypeScript revalidated sources and Cargo compiled again.
Separate ccache and Gradle task-cache checks produced compiler/task cache
hits after both linked-worktree and remote restores. The Gradle task-cache
check used an isolated cache directory, so it did not rely on a preexisting
user-wide cache.

Transporting state successfully is not evidence of an incremental hit or
of cross-platform portability. A production store, credentials and tool
configuration still need their own verification.

### Portable transform and task caches

For Metro, configure its transform cache explicitly:

```jsonc
"bundle": {
  "qk:warm": {
    "portable": true,
    "env": { "METRO_CACHE_DIR": "{warm}/metro-transform" },
    "maxSize": "1GB"
  }
}
```

Metro does not automatically read that variable. In the Metro configuration,
pass it to `FileStore`, retaining a default when running without qk:

```javascript
const os = require('node:os')
const path = require('node:path')
const { FileStore } = require('metro-cache')

const cacheStores = [
  new FileStore({
    root: process.env.METRO_CACHE_DIR ?? path.join(os.tmpdir(), 'metro-cache'),
  }),
]
```

Use `cacheStores` in the existing Metro configuration. File-map state is a
separate cache; sharing transform results does not promise that Metro will
skip discovery in a new checkout. Match the transformer, configuration and
dependencies, and test plugins that use absolute source paths.

For Gradle's task-output cache, keep its content-addressed objects in a
shared group rather than copying the project's whole `.gradle` directory:

```jsonc
"compile-java": {
  "executor": "nx:run-commands",
  "options": { "command": "./gradlew compileJava --build-cache" },
  "qk:warm": {
    "group": "gradle-task-cache",
    "portable": true,
    "env": { "GRADLE_BUILD_CACHE_DIR": "{warm}/task-cache" },
    "maxSize": "1GB"
  }
}
```

The Gradle settings must consume this variable. For example, in
`settings.gradle`:

```groovy
def warmBuildCache = System.getenv('GRADLE_BUILD_CACHE_DIR')
buildCache {
    local {
        if (warmBuildCache != null) {
            directory = file(warmBuildCache)
        }
    }
}
```

A task must support Gradle caching and declare its inputs correctly. Its
path-sensitivity rules and toolchain decide whether another checkout can
reuse the entry. This recipe does not make the project's execution history,
configuration cache, or native CMake directories portable.

## Finding local state

Native builds write to places that are hard to guess. `qk warm suggest
<project:target>` runs the task, and what it depends on, in the sandbox's
audit mode and lists the directories it wrote outside its declared outputs,
with how much each holds. Each written path is shown under its topmost
directory that holds no source file, tracked or not ignored, since that is
a directory the build keeps apart from the checkout; writes beside sources
are left out. It takes the options `qk run` does and needs macOS, where the
sandbox can report rather than refuse.

## Keeping state across destructive dependencies

A step that regenerates a directory can take build state with it, as
`expo prebuild --clean` deletes an Android project together with Gradle's
build directories. `survive` names such dependencies:

```jsonc
"android": {
  "dependsOn": ["prebuild-android"],
  "qk:warm": {
    "paths": [
      "{projectRoot}/android/.gradle",
      "{projectRoot}/android/build",
      "{projectRoot}/android/app/.cxx",
      "{projectRoot}/android/app/build"
    ],
    "survive": ["prebuild-android"],
    "mtimes": "preserve",
    "portable": false
  }
}
```

Before a named dependency runs, or is restored from the cache, qk moves the
task's `paths` into the worktree's state, and after it finishes, whether it
succeeded or not, moves them back over whatever it left there. They keep
their files and modification times without being stored or copied.
Dependencies named this way that run at once share the move: the first
moves the paths, the last moves them back. A path is only moved back where
its parents are directories inside the workspace; when the dependency put a
link or a file in their place, the path stays aside, and the next run moves
it back after that run's dependency, keeping the task's other paths across
that dependency as usual. A run that is stopped leaves its paths
aside the same way. An entry names a target in the task's project, or
`project:target`, and applies to the dependency in any configuration. What
the dependency writes beside the paths stays.

Stash ownership records are replaced atomically after syncing the new file.
Unreadable, invalid or missing metadata leaves saved contents in place and
reports a recovery warning. This protects stopped-process recovery; it does
not promise durability across power loss.

## Matching toolchains and configurations

A `key` restricts restores to saves that suit the checkout, such as those
built with the same toolchain:

```jsonc
"qk:warm": {
  "paths": ["{projectRoot}/android/app/.cxx"],
  "key": ["{workspaceRoot}/pnpm-lock.yaml", { "env": "ANDROID_NDK_VERSION" }],
  "restoreKeys": 1
}
```

Each part is a file, by its content, or an environment variable, by its
value as the task receives it, including the target's `options.env`. A save is restored when its key matches whole, or, with `restoreKeys`,
when it matches in that many leading parts; one that matches whole is
preferred. Among saves that suit equally, the worktree's own comes first,
then, with `portable: true`, the one made at the commit nearest behind
`HEAD` in Git history, then the newest, so a worktree cut from `main` starts
from `main`'s state rather
than the last feature branch's.

## Shared compiler caches

A `group` shares one `{warm}` directory, and its saves, between every target
that names it, as one compiler cache serves several builds:

```jsonc
"qk:warm": {
  "group": "ccache",
  "portable": true,
  "env": { "CCACHE_DIR": "{warm}/ccache", "CCACHE_BASEDIR": "{workspaceRoot}" }
}
```

The ccache example supplies absolute directories. Relative compiler inputs
and `CCACHE_BASEDIR` allowed cache hits across tested checkout paths. Debug
information and flags containing absolute paths may need compiler path
remapping; test that the reused object has the intended paths and contents.

A group keeps no `outputs` or `paths`, which belong to one task. Its
targets may run at once and use the directory together; restoring and
saving it are serialized, and a save holds everything the directory held.

`save: "background"` saves after the task has reported, so its dependents
start without waiting for a large group to be stored; the run waits for the
save before it ends. Other tasks may change the paths meanwhile, so the save
can hold files from after the task ran, and files removed before it reads
them are left out.

Groups over their `maxSize` are not saved. Warm state counts toward the
cache's size limit and is evicted with it. `--skip-cache` neither restores
nor saves it, and leaves the variables unset. Two tasks in one run cannot
keep the same path. A tool must validate its own cache, as Metro, `tsc` and
Next do: qk only guarantees that warm state never changes a key or a hit.
