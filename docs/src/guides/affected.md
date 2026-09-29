# Running affected tasks

```sh
qk show projects --affected --base main
qk show affected web --base main
qk affected -t build,test --base main
qk affected -t build,test --granularity task --base main
```

As in Nx 23, changed files are those between the merge base of `--base` and
`--head`, or with no head, between the merge base and the working tree,
including uncommitted and untracked files. The base defaults to `NX_BASE`,
then nx.json `defaultBase`, then `main`; the head to `NX_HEAD`. `--files`,
`--uncommitted` and `--untracked` replace the comparison. Files matching the
root `.gitignore` or `.nxignore` are left out.

Unlike Nx, when the base is a branch with an upstream, such as `main`
following `origin/main`, and the head left the upstream later than the local
branch, the base is that later point. Otherwise a local `main` that has not
been updated would count everything that landed on `origin/main` since as
changes. When the compared commits still include some already on the default
branch's upstream, as with an explicit `--base` older than where the head
branched, `affected` and `show affected` warn. On the default branch itself
the range is its own history, and nothing is flagged.

## How changes reach projects

A changed file touches the project whose root most specifically contains it.
`nx.json` touches every project; a file named by a `{workspaceRoot}` input
touches the projects declaring it; a deleted `project.json` or
`package.json` touches every project. Dependencies changed in the root
`package.json` touch the projects installing the package, and path mappings
changed in the root tsconfig touch the projects they point into. Every
project depending on a touched project is affected too.

Two rules deliberately differ from Nx:

- A `pnpm-workspace.yaml` change confined to resolution keys (`catalog`,
  `catalogs`, `overrides`, `patchedDependencies` and the like) reaches
  projects only through the lockfile.
- Under `projectsAffectedByDependencyUpdates: "auto"`, a `pnpm-lock.yaml`
  change touches the projects whose importer installs something different,
  by the same installed sets the cache keys use. This leaves out projects
  that install exactly what they did before, which Nx reports, and catches
  transitive changes such as a dependency's dependency moving version,
  which Nx can miss. Other modes behave as in Nx.

`show projects --affected` prints projects in graph order; Nx prints its
traversal order, so compare the two as sets.

## Explaining the result

`qk show affected` takes the same change options and explains the result.
Without a project it lists every affected project with the first reason it
is touched, or the dependency it is affected through. With a project it
prints a shortest dependency path to a touched project, the project's other
affected dependencies, and every reason the touched project is touched. For
a lockfile reason it lists what the importer installs differently, by
package: versions that moved first, then packages added or removed, then
those that kept their version but resolved different peers or patch, then
those whose dependencies changed as a consequence.

```text
$ qk show affected web --base HEAD^ --head HEAD
2 changed files between 3f1c0a9e2b7d and HEAD.
The base is where HEAD left HEAD^, 1 commit back.
web is affected because it depends on ui:
  web -> ui (static)
ui is touched:
  what packages/ui installs changed in pnpm-lock.yaml (3 packages)
      react (direct): 19.0.0 -> 19.1.0
      react-dom (direct): 19.0.0: peers or patch changed
      scheduler: dependencies or integrity changed
```

## Selecting affected tasks by inputs

With `--granularity task`, `qk affected` selects tasks instead of projects: a
task is affected when a changed file is one of its resolved inputs, when what
its lockfile importers or `externalDependencies` install changed, when
`pnpm-workspace.yaml` changed outside its resolution keys, when a project
manifest was deleted, or when it depends on an affected task. These are the
same inputs its cache key reads, so an unaffected task would be a cache hit,
except that env and runtime inputs count as unchanged, since the base
revision's environment is unknowable. A test-only change then reaches the
test tasks and not the builds whose `production` inputs exclude tests.
`project` stays the default, as in Nx. `qk show tasks -t <targets> --affected`
lists the affected tasks with their first reason, or the whole analysis with
`--json`.

`--json` gives the whole analysis, or for one project its path with the
typed reasons, including the snapshot keys a lockfile change added,
removed or changed.

## Project selectors

Project selectors support `*`, `?`, character classes and `tag:<glob>`.
Repeat `--projects` or use commas to combine selectors. Prefix a selector
with `!` to exclude it; exclusions win regardless of order. With only
exclusions, selection starts with all projects. A selector matching nothing
returns an empty list; `show project` requires an existing exact name.
Quote globs so the shell does not expand them. Commas delimit CLI selectors,
so brace globs containing commas are not supported on the command line.
