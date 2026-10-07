# Import reachability

A project is affected when a project it depends on changes, whether or not its
code uses what changed. An affected profile with `reachability` leaves out a
project whose entry points do not import the change, so a change to a corner of
a shared package that only one app uses does not select every app's builds.
Imports are followed by [fallout](https://github.com/Pajn/fallout).

```sh
qk show projects --affected --affected-profile reach --base main --head HEAD
qk show affected app --affected-profile reach --base main --head HEAD
```

The profile is opt-in. It applies to project selection, and with
`--granularity task` to task selection, where a target can also name
[cases](#task-selection-and-cases) it runs separately. Ordinary selection and
cache keys do not change. Use it where a wrongly skipped project is recovered
cheaply, such as deciding which app builds a pull request runs, and keep
ordinary selection for checks and for the default branch.

## Configure it

Enable reachability in a profile in `nx.json`. A profile can also contain
projections, which are applied first.

```json
{
  "qk:affectedProfiles": {
    "reach": { "reachability": true }
  }
}
```

A project takes part by declaring `qk:reachability` in its `project.json`, or
under `nx` in its `package.json`:

```json
{
  "name": "app",
  "qk:reachability": {
    "anchors": ["{projectRoot}/src/main.tsx", "{projectRoot}/vite.config.ts"],
    "sources": ["{projectRoot}/src/**/*", "{workspaceRoot}/packages/*/src/**/*"]
  }
}
```

| Setting | Meaning |
| --- | --- |
| `anchors` | Nonempty array of globs naming the files the project depends on through their imports: entry points and build configuration |
| `sources` | Nonempty array of globs naming the files whose changes matter only through imports |

Both take `{projectRoot}` and `{workspaceRoot}`. A `package.json` is never a
source, since it decides what imports resolve to. A project without
`qk:reachability` is never left out. An end-to-end project that drives an app
rather than importing it can name the app's anchors, so the two are decided
alike; without its own settings, it is left out only when it is affected
through the app alone. Settings are read for every project whenever the
profile is selected, so a mistake is reported before a change needs it.

## When a project is left out

Under the profile, a project with `qk:reachability` that ordinary selection
affects is left out when all of these hold:

1. **It is affected only through its dependencies.** None of its own files
   changed, and nothing touches every project, such as `nx.json` or the root
   tsconfig.
2. **Every change it is affected through is a source.** Each touched project
   it depends on, directly or not, is touched only by changed files its
   `sources` match. Lockfile installs, a dependency's manifests, build
   configuration and any other file keep it, because no import carries them.
3. **No anchor imports a changed file.** fallout searches from each anchor
   through the imports, at file granularity, comparing each changed file with
   its base version. A change made only of TypeScript types does not count,
   since it does not change what a bundler emits.
4. **The search lost no edge.** If fallout cannot resolve an import the
   repository answers for, such as a relative path or a workspace package that
   is not linked, the project is kept: the change may be behind it.

A project left out does not carry selection on. Its dependents are affected
through it only if another selected dependency, or their own files, affect
them.

Anything that prevents an answer keeps the project and says why: anchors that
match no file, a `fallout.toml` that cannot be read, or a head revision that
is not the checkout.

## Task selection and cases

With `--granularity task`, the profile narrows tasks whose target declares
`qk:reachability`. A target can name `cases` too: files it runs separately,
such as the cases of a visual regression suite, each decided on its own.

```json
{
  "targets": {
    "visual": {
      "inputs": ["default", "^default", "{workspaceRoot}/.github/workflows/visual.yml"],
      "qk:reachability": {
        "anchors": ["{projectRoot}/visual/shell.tsx"],
        "cases": ["{projectRoot}/visual/cases/**/*.tsx"],
        "sources": ["{projectRoot}/src/**/*", "{projectRoot}/visual/**/*.tsx", "{workspaceRoot}/packages/*/src/**/*"]
      }
    }
  }
}
```

A target needs `anchors`, `cases` or both, and `sources`. A task that ordinary
task selection affects is then decided by its changed inputs:

1. **A changed input that is not a source runs the whole task**, as do lockfile
   installs, a deleted manifest and an affected task it depends on, whose
   outputs it may read. The task's ordinary inputs are what make this sound:
   declare the suite's harness, its workflow and anything else that changes
   how every case runs as inputs, outside `sources`.
2. **An anchor that imports a change runs the whole task**, as does an import
   it cannot resolve. Anchors are what every case runs inside, such as a
   suite's shell.
3. **Otherwise each case is decided on its own**: the cases that import a
   change, or a changed case itself, are selected. With none, the task is left
   out, and with it any task affected only through it.

`qk show tasks -t <targets> --affected --affected-profile <name>` lists each
selected case with the change it imports, and `--json` adds a `reachability`
object with each task's decision. A task with selected cases still runs every
case when qk runs it.

## Requirements

- **The head must be the checkout.** fallout reads files from disk, so with
  `--head` naming another commit, or with uncommitted changes to tracked files,
  every project is kept. Run selection in the checkout of the revision being
  tested, as CI does.
- **Install workspace packages first.** Imports between workspace packages
  resolve through `node_modules`. Without them, those imports are gaps and
  nothing is left out: the profile is safe and saves nothing.
- **Only static imports are followed.** What a bundler adds through its
  configuration, native code and requests built at run time are not seen,
  which is why only changes to `sources` can leave a project out.
- **Bundler settings come from `fallout.toml`.** Which `exports` conditions
  and `package.json` fields an app's bundler reads, and its import aliases,
  are declared in a `fallout.toml` beside the app. See fallout's
  documentation.

## Explaining the result

`qk show affected` lists the projects the profile left out after the affected
ones, and notes for each kept project why: the anchor that imports a change,
the import that could not be resolved, or the change imports do not carry.
With a project, it shows the anchors searched and the changes they did not
reach, or the chain of imports that reached one. `--json` adds a
`reachability` object with each decision.

```text
$ qk show affected --affected-profile reach --base main --head HEAD
1 changed file between 3f1c0a9e2b7d and HEAD.
other  depends on lib
lib    touched: libs/lib/src/unused.ts changed
Left out by import reachability:
  app-e2e  left out: affected only through app, left out too
  app      left out: none of its 1 anchor imports a change
```

A workspace can record this beside ordinary selection without acting on it,
and compare what it would have left out with what later broke on the default
branch, before letting it skip anything.
