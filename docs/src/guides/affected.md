# Running affected tasks

For an opt-in consumer-specific comparison of shared sources, see
[Project change projections](affected-projections.md), and for leaving out
projects whose entry points do not import a change, see
[Import reachability](affected-reachability.md). Ordinary selection and task
cache keys retain the behavior described here.

```sh
qk show projects --affected --base main
qk show affected web --base main
qk affected -t build,test --base main
qk affected -t build,test --granularity task --base main
```

As in Nx, changed files are those between the merge base of `--base` and
`--head`, or with no head, between the merge base and the working tree,
including uncommitted and untracked files. The base defaults to `NX_BASE`,
then nx.json `defaultBase`, then `main`; the head to `NX_HEAD`. `--files`,
`--uncommitted` and `--untracked` replace the comparison. A file named with
`--files` counts as changed whole: its revisions are not compared, so a JSON
or lockfile change is not narrowed to what changed inside it. Files matching
the root `.gitignore` or `.nxignore` are left out. Untracked files are those
Git lists as untracked and not ignored, so an `.nxignore` negation does not
bring back one `.gitignore` excludes, though it does for a file named with
`--files`. As in Nx, a base or head starting with `-` is refused, since git
would read it as an option.

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
`nx.json` touches every project. A file named by a `{workspaceRoot}` input
touches the projects declaring it; an input without glob syntax, such as
`{workspaceRoot}/generated`, names that file or everything under it. A deleted
file that Nx's default plugins create projects from, any `project.json` or
`package.json` or a root lockfile, touches every project, since what depended
on a removed project is unknown. Path mappings changed in the root tsconfig
touch the projects they point into, and any other change to it every project.

A change to the root lockfile (`pnpm-lock.yaml`, `pnpm-lock.yml`,
`package-lock.json`, `yarn.lock`, `bun.lock` or `bun.lockb`) touches projects
by nx.json's `pluginsConfig["@nx/js"].projectsAffectedByDependencyUpdates`:
every project under `"all"`, Nx's default, or the projects a list of
selectors names. Under `"auto"`, a `pnpm-lock.yaml` change touches the
projects whose importer installs something different, by the same installed
sets the cache keys use; another lockfile, or one qk cannot compare, touches
every project.

In the root `package.json`, a changed dependency touches the projects
installing that package, or the workspace project it names, and an
`@types/` package also the package it types. A changed override, under
`overrides`, `resolutions` or `pnpm.overrides`, touches the projects
installing the package its selector names: the last key, past a pnpm
`parent>` and without a `@range`. A removed dependency, a change to a global
package (`nx` and the plugins nx.json loads), an override naming no installed
package, or a file that cannot be compared touches every project. Installed
packages are read from `pnpm-lock.yaml`; without it, a changed dependency
touches every project. Every project depending on a touched project is
affected too.

Some rules deliberately differ from Nx:

- Under `"auto"`, the installed sets leave out projects that install exactly
  what they did before, which Nx reports, and catch transitive changes such as
  a dependency's dependency moving version, which Nx can miss.
- A list of selectors touches its projects only when a lockfile changed. Nx
  touches them on every change, lockfile or not, which reads as a bug; its
  task selection checks for a lockfile change, as qk does.
- A change to the file nx.json `extends` touches every project, as a change to
  nx.json does. Nx does not look at that file, although it configures the
  workspace as much as nx.json does.

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
`pnpm-workspace.yaml` changed outside its resolution keys, when a dependency
project's `package.json` changed in a field its key reads or its version moved
into or out of the range the task's project declares for it, when a project
manifest was deleted, or when it reads the declared outputs of an affected
task, through `dependentTasksOutputFiles` or a fileset that selects them. As
in Nx, depending on a task without reading its outputs carries nothing, so
a task ordered after a build but reading only sources is not affected by the
build's changes. A file the
task's inputs select counts by any change to it, as in its key. These are the
same inputs its cache key reads, so an unaffected task would be a cache hit,
except that env and runtime inputs count as unchanged, since the base
revision's environment is unknowable. A test-only change then reaches the
test tasks and not the builds whose `production` inputs exclude tests.
A continuous task a task depends on, such as the dev server an e2e task runs
against, counts its inputs as the task's own instead, and so do those of
the continuous tasks it depends on in turn. A `dependsOn` entry with
`inputs: false` leaves them out, unless another entry names the same task
without it.
`project` stays the default, where Nx 24 selects tasks; see
[Nx compatibility](../compatibility.md). `qk show tasks -t <targets> --affected`
lists the affected tasks with their first reason, or the whole analysis with
`--json`.

`--json` gives the whole analysis, or for one project its path with the
typed reasons, including the snapshot keys a lockfile change added,
removed or changed.

## Project selectors

Project selectors are read as Nx's `findMatchingProjects` reads them,
wherever Nx reads them: `--projects` and `--exclude`, `implicitDependencies`,
`dependsOn` and `inputs` `projects`, `targetDefaults` `filter.projects` and a
`projectsAffectedByDependencyUpdates` array.

- A project name selects that project. A glob selects the names it matches;
  globs are minimatch globs, so `*` and `?` do not cross a `/`
  (`@scope/*`, not `@scope*`, selects `@scope/ui`) and `**` does.
- `tag:<pattern>` selects by tag, `directory:<glob>` by project root and
  `name:<pattern>` by name only. A pattern without a label is tried as a
  name, then as a root when no name matched, so `libs/*` selects the projects
  directly under `libs`.
- A plain pattern that is no project name and no glob selects the names
  containing it as a word, ignoring case: `ui` selects `ui`, `@scope/ui` and
  `ui_kit`, but not `ui-e2e`, as `-` and `@` join words.
- Patterns apply in order, each adding what it matches or, prefixed with `!`,
  removing it: `'!tag:e2e,app-e2e'` keeps `app-e2e`. When the first pattern
  excludes, selection starts with every project.
- `--exclude` removes what its patterns match among the projects already
  selected.
- Empty patterns and `nx-cloud:` references to other workspaces are skipped.
  A selection matching nothing is empty. As in Nx, `show project` takes a
  name that is no project as a pattern, which must match exactly one.

The `projects` of an input are read by Nx's native hasher, which differs:
a first `!` pattern starts from nothing, and a plain name matches only that
name.

Quote globs so the shell does not expand them. Commas delimit CLI selectors,
so brace globs containing commas are not supported on the command line.

