# Nx compatibility

qk reads a bounded subset of Nx 23 configuration. It runs without invoking
Nx or loading Nx plugins. The implementation, fixtures and parity tests
establish supported behavior; preserving unknown metadata during inspection
does not mean qk executes it.

## Supported executors

`nx:run-commands`, `nx:run-script` and `nx:noop` are executable.
Other executor names remain inspectable but fail execution preflight.

## Project dependencies

Package dependencies create workspace graph edges when their ranges are
`workspace:…`, `*`, a `file:` path to the package, or a semver range the
package's version satisfies (including prereleases by npm's rules).
Ranges such as `catalog:`, npm aliases, or ranges outside the workspace
version do not create an edge. Names are looked up by dependency key, so
`"ui": "workspace:@scope/ui@*"` does not link to `@scope/ui`.
When a name occurs in multiple sections, precedence is `dependencies`,
`devDependencies`, `peerDependencies`, then `optionalDependencies`.
`implicitDependencies` adds selected project edges.

## Deliberate differences

- A pnpm lockfile change under `projectsAffectedByDependencyUpdates: "auto"`
  follows installed sets, avoiding projects whose installations are unchanged
  and including transitive installation changes Nx can miss.
- Resolution-only pnpm workspace changes reach tasks through the lockfile.
- A tracked upstream can adjust the affected base to avoid counting changes
  already landed upstream.
- `readyWhen` accepts readiness text from either stdout or stderr.
- `project.local.json`, `nx.local.json`, `qk:threads` and `qk:warm` are qk
  extensions. Nx does not apply them.
- Input negated-group exclusions apply to the pattern they came from in qk;
  Nx applies them across a project's patterns. qk can include extra inputs.

See [Affected selection](guides/affected.md), [Inputs and outputs](reference/inputs-outputs.md)
and [Running tasks](guides/running-tasks.md) for the detailed rules.

## Limits

This is a subset of the design's compatibility surface. There is no Nx
parity claim yet. In particular:

- The graph includes **workspace projects only**, as `nx graph --file`
  does. `--external` adds a node per installation in the pnpm lockfile,
  `npm:<name>@<version>` with peers and patch in the version, with edges from
  projects to what they install directly and between installations.
- Nx plugins and inferred targets are outside the design's scope.
- qk has its own live output panel and limited stdin/foreground terminal
  support for a single requested task. It does not provide pseudo-terminals
  or implement Nx release commands. The npm package name is not chosen yet.

The compatibility baseline is documented in Nx's
[project configuration](https://nx.dev/docs/reference/project-configuration)
and [workspace configuration](https://nx.dev/docs/reference/nx-json)
references. The bounded subset above and fixture tests define qk's current
behavior; new Nx features are not automatically supported.
