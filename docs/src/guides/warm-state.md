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
- `env` sets variables for the task, with `{projectRoot}`, `{workspaceRoot}`
  and `{warm}`, a directory qk keeps for the task in the worktree's state,
  outside the working tree. Under Nx these variables are not set, so tools
  that only cache when told to keep their default behaviour there.

Warm state is restored before the task runs, on a cache miss and for
targets that are not cacheable, and never on a hit. A group already present
on disk is left alone, since it is the newest for that checkout; otherwise
it comes from a save in the store linked worktrees share. Each worktree
keeps its own save, and a restore takes the worktree's own before the most
recent other worktree's; the eight most recent saves of a task are kept.
Without one there it comes from the remote store: the current branch's
save, else the default branch's (nx.json `defaultBase`, else `main`). The
branch comes from `GITHUB_HEAD_REF` or `GITHUB_REF_NAME` in CI, else from
git; a checkout with no branch reads the default branch's state but saves
none remotely. `remote: false` keeps a target's warm state local. It is
saved after successful runs only; files unchanged since the last save or
restore are recognised by their metadata and not read again.
Restored files are dated to the Unix epoch, older than anything in the
checkout, so a tool that compares timestamps, as `tsc --build` does, checks
the sources against them rather than taking them as up to date.
`mtimes: "preserve"` restores a worktree's own save with the modification
times it was saved with, which tools that rebuild whatever looks newer than
its outputs, as Ninja does, need to skip unchanged work; another worktree's
save is still dated to the epoch. State that records the checkout's absolute
path, such as CMake build directories, is useless or harmful in another
worktree: `portable: false` restores only the worktree's own save and keeps
it out of the remote store.

A step that regenerates a directory can take build state with it, as
`expo prebuild --clean` deletes an Android project together with Gradle's
build directories. `survive` names such dependencies:

```jsonc
"android": {
  "dependsOn": ["prebuild-android"],
  "qk:warm": {
    "paths": ["{projectRoot}/android/.gradle", "{projectRoot}/android/app/build"],
    "survive": ["prebuild-android"]
  }
}
```

Before a named dependency runs, or is restored from the cache, qk moves the
task's `paths` into the worktree's state, and after it finishes, whether it
succeeded or not, moves them back over whatever it left there. They keep
their files and modification times without being stored or copied, and a
run that is stopped moves them back at the next one. An entry names a target
in the task's project, or `project:target`, and applies to the dependency in
any configuration. What the dependency writes beside the paths stays.

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
value. A save is restored when its key matches whole, or, with `restoreKeys`,
when it matches in that many leading parts; one that matches whole is
preferred. Among saves that suit equally, the worktree's own comes first,
then the one made at the commit nearest behind `HEAD` in Git history, then
the newest, so a worktree cut from `main` starts from `main`'s state rather
than the last feature branch's.

A `group` shares one `{warm}` directory, and its saves, between every target
that names it, as one compiler cache serves several builds:

```jsonc
"qk:warm": {
  "group": "ccache",
  "env": { "CCACHE_DIR": "{warm}/ccache", "CCACHE_BASEDIR": "{workspaceRoot}" }
}
```

A group keeps no `outputs` or `paths`, which belong to one task. Its
targets may run at once and use the directory together; restoring and
saving it are serialized, and a save holds everything the directory held.
`save: "background"` saves after the task has reported, so its dependents
start without waiting for a large group to be stored; the run waits for the
save before it ends. Other tasks may change the paths meanwhile, so the save
can hold files from after the task ran, and files removed before it reads
them are left out. Groups over
their `maxSize` are not saved. Warm state counts toward the
cache's size limit and is evicted with it. `--skip-cache` neither restores
nor saves it, and leaves the variables unset. Two tasks in one run cannot
keep the same path. A tool must validate its own cache, as Metro, `tsc` and
Next do: qk only guarantees that warm state never changes a key or a hit.
