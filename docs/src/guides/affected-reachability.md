# Import reachability

A task is affected when one of its inputs changes, whether or not its code
uses what changed. An affected profile with `reachability` leaves out a task
whose entry points do not import the change, so a change to a corner of a
shared package that only one app uses does not select every app's builds.
Imports are followed by [fallout](https://github.com/Pajn/fallout).

```sh
qk show projects --affected --affected-profile reach --base main --head HEAD
qk show tasks -t build test --affected --affected-profile reach --base main --head HEAD
```

The profile is opt-in. It decides tasks, and a project by its tasks: a
project is left out only when every task of it a change affects is. A target
can also name [cases](#cases) it runs separately. Ordinary selection and
cache keys do not change. Use it where a wrongly skipped task is recovered
cheaply, such as deciding which app builds a pull request runs, and keep
ordinary selection for checks and for the default branch.

## Configure it

Name the settings tasks are decided by under `qk:reachability` in `nx.json`,
and enable reachability in a profile. A profile can also contain
projections, which are applied first.

```json
{
  "qk:reachability": {
    "app": {
      "anchors": ["{projectRoot}/src/main.tsx", "{projectRoot}/vite.config.ts"],
      "sources": ["{projectRoot}/src/**/*.{ts,tsx}", "{workspaceRoot}/packages/*/src/**/*.{ts,tsx}"]
    },
    "unit": {
      "anchors": ["{projectRoot}/test/setup.ts"],
      "cases": ["{projectRoot}/src/**/*.test.{ts,tsx}"],
      "sources": ["{projectRoot}/src/**/*.{ts,tsx}", "{projectRoot}/test/**/*.ts", "{workspaceRoot}/packages/*/src/**/*.{ts,tsx}"]
    }
  },
  "qk:affectedProfiles": {
    "reach": { "reachability": { "default": "app" } }
  }
}
```

A target takes part through its `qk:reachability`, in its project's
configuration or from `targetDefaults`:

| Value | Meaning |
| --- | --- |
| absent | The profile's `default` config, or none without one |
| a name | That config from `nx.json` |
| `false` | None, even with a default: ordinary selection decides it |
| an object | Settings of its own, as a named config declares them |

```json
{
  "name": "app",
  "targets": {
    "build": {},
    "test": { "qk:reachability": "unit" },
    "lint": { "qk:reachability": false }
  }
}
```

`"reachability": true` enables the profile without a default, so only
targets that name a config or declare one take part.

| Setting | Meaning |
| --- | --- |
| `anchors` | Globs naming the files every run of the task depends on through their imports: entry points, build configuration, a test harness |
| `cases` | Globs naming files the task runs separately, each decided on its own. See [Cases](#cases) |
| `sources` | Nonempty globs naming the files whose changes matter only through imports |

A config needs `anchors`, `cases` or both, and `sources`. Paths take
`{projectRoot}` and `{workspaceRoot}`, expanded for the task's project, so
one config serves every project laid out alike. They take `!` exclusions: a
file matches when an inclusion matches it and no exclusion does, in any
order. A `package.json` is never a source, since it decides what imports
resolve to.

Anchor what each task runs, not only what the app ships. An app's build
reads its entry points; its unit tests also read their harness and helpers,
which the app never imports. A test target narrowed by the app's anchors
would be left out when a helper changes.

List code in `sources`, not every file under a directory. A GraphQL schema,
a translation catalogue or a tool's configuration reaches a build through a
generator or a tool rather than an import, so as sources they would let a
change that matters leave the task out. Leave test files in: a changed test
reaches the task only if an anchor or case imports it, while a file left out
of `sources` keeps the task whenever it changes.

```json
"sources": [
  "{projectRoot}/src/**/*.{ts,tsx,js,jsx}",
  "{workspaceRoot}/packages/**/*.{ts,tsx,js,jsx}",
  "!{workspaceRoot}/packages/**/*.config.{ts,js}"
]
```

An end-to-end target that drives an app rather than importing it can name
the app's anchors with `{workspaceRoot}`, so the two are decided alike. Every
target's settings are read whenever the profile is selected, so a mistake is
reported before a change needs it. `qk:reachability` on a project rather than
a target is refused.

## When a task is left out

Under the profile, a task with settings that ordinary task selection affects
is decided by its changed inputs:

1. **A changed input that is not a source runs the whole task**, as do lockfile
   installs, a deleted manifest and an affected task it depends on, whose
   outputs it may read. The task's ordinary inputs are what make this sound:
   declare its harness, its workflow and anything else that changes how it
   runs as inputs, outside `sources`. A dependency's `package.json` whose
   fields dependents read are unchanged, such as one with only a new
   `version` or `scripts`, is the exception: it does not count. A new or
   moved dependency does.
2. **An anchor that imports a change runs the whole task.** fallout searches
   from each anchor through the imports, at file granularity, comparing each
   changed file with its base version. A change made only of TypeScript types
   does not count, since it does not change what a bundler emits.
3. **An import the search cannot place runs the whole task.** If fallout
   cannot resolve an import the repository answers for, such as a relative
   path or a workspace package that is not linked, the change may be behind
   it.
4. **Otherwise the task is left out**, and with it any task affected only
   through it, or narrowed to its [cases](#cases) that import a change.

Anything that prevents an answer runs the whole task and says why: anchors or
cases that match no file, a `fallout.toml` that cannot be read, or a head
revision that is not the checkout.

## Project selection

Under the profile, project selection decides each affected project by its
tasks: every target of it, with the tasks they depend on. A project is left
out when the profile left out every task of it that a change affects. One
with a task that still runs is kept, as is one none of whose affected tasks
the profile decides. Its dependents are decided by their own tasks.

A project with many targets is left out rarely, since any of its tasks
without settings keeps it. To ask about the targets a job runs, such as which
apps' builds a pull request needs, select projects by those targets instead:

```sh
qk show projects --affected --affected-targets build,export --affected-profile reach --json
```

This lists the projects where a task of one of the targets is affected, by
its inputs and then by the profile. A profile with projections cannot be used
here, since task selection reads declared inputs.

## Cases

A target can name `cases`: files it runs separately, such as the cases of a
visual regression suite or a project's test files, each decided on its own.

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

Anchors are what every case runs inside, such as a suite's shell, so an
anchor that imports a change runs every case. Otherwise the cases that import
a change, or a changed case itself, are selected. With none, the task is left
out.

`qk show tasks -t <targets> --affected --affected-profile <name>` lists each
selected case with the change it imports, and `--json` adds a `reachability`
object with each task's decision.

### Running only the selected cases

`qk affected -t <targets> --granularity task --affected-profile <name>` runs
the affected tasks. A task narrowed to some of its cases receives
`QK_AFFECTED_CASES`, the absolute path of a temporary file naming those cases
one per line, each relative to the workspace root. Read the file at that path
as it is, from any working directory. The variable is absent when the whole task
runs, so a task that does not read it runs every case, which is always safe.

```sh
if [ -n "$QK_AFFECTED_CASES" ]; then
  suite --only "$(paste -sd, "$QK_AFFECTED_CASES")"
else
  suite
fi
```

Such a task runs without the cache, reads and writes both: its result covers
the selected cases alone, so it must not stand for a run of every case, and
tasks depending on it run uncached too. A suite's own rules, such as running
the cases that show translated copy when a translation catalogue changes, stay
in the suite, which can add cases to the list but should not drop them.

## Requirements

- **The head must be the checkout.** fallout reads files from disk, so with
  `--head` naming another commit, or with uncommitted changes to tracked files,
  every task runs. Run selection in the checkout of the revision being
  tested, as CI does.
- **Install workspace packages first.** Imports between workspace packages
  resolve through `node_modules`. Without them, those imports are gaps and
  nothing is left out: the profile is safe and saves nothing.
- **Only static imports are followed.** What a bundler adds through its
  configuration, native code and requests built at run time are not seen,
  which is why only changes to `sources` can leave a task out.
- **Bundler settings come from `fallout.toml`.** Which `exports` conditions
  and `package.json` fields an app's bundler reads, and its import aliases,
  are declared in a `fallout.toml` beside the app. See fallout's
  documentation.

## Explaining the result

`qk show affected` lists the projects the profile left out after the affected
ones, and notes for each kept project with a narrowed task which task runs
and why: the anchor that imports a change, the import that could not be
resolved, or the change imports do not carry. With a project, it shows what
the profile decided for each of its affected tasks, or the chain of imports
that reached one. `--json` adds a `reachability` object with each decision.
`qk show tasks --affected` does the same for tasks.

```text
$ qk show affected --affected-profile reach --base main --head HEAD
1 changed file between 3f1c0a9e2b7d and HEAD.
other  depends on lib
lib    touched: libs/lib/src/unused.ts changed
Left out by import reachability:
  app-e2e  left out: none of its 1 affected task runs
  app      left out: none of its 1 affected task runs
```

A workspace can record this beside ordinary selection without acting on it,
and compare what it would have left out with what later broke on the default
branch, before letting it skip anything.
