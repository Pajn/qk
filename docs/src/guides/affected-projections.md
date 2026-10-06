# Project change projections

A schema or another shared source can change without changing what its
consumers use. An affected profile lets a workspace adapter compare a
consumer-specific representation at two revisions. qk replaces the declared
source changes with changed artifact paths before selecting projects and
following their dependencies.

For example, a runtime profile can compare generated JavaScript rather than
all generated TypeScript. A type-only change can then leave preview deployments
unselected while ordinary affected selection still selects type checking.
Changes to runtime values, such as generated operations or possible-type maps,
still count when the adapter includes them in its snapshot.

Profiles are opt-in and apply to project selection. They do not change task
cache keys, cached results, or task-level affected selection.

```sh
qk show projects --affected --affected-profile runtime --base main --head HEAD --json
qk show affected web --affected-profile runtime --base main --head HEAD
```

`--profile` remains the performance-timing flag. `--affected-profile` selects
a named change projection.

## Configure a profile

Add `affectedProfiles` to `nx.json`:

```json
{
  "affectedProfiles": {
    "runtime": {
      "projections": [
        {
          "name": "generated-runtime",
          "command": ["node", "tools/runtime-snapshot.mjs"],
          "sources": ["schemas/**", "operations/**", "generator.config.json"],
          "outputs": ["apps/*/generated/**", "packages/*/generated/**"],
          "timeoutSeconds": 60
        }
      ]
    }
  }
}
```

Each projection has these settings:

| Setting | Meaning |
| --- | --- |
| `name` | Nonempty name, unique within the profile; used in explanations |
| `command` | Nonempty executable-and-arguments array; qk does not invoke a shell |
| `sources` | Nonempty array of workspace-relative globs naming source changes this adapter may replace |
| `outputs` | Nonempty array of workspace-relative globs permitting artifact paths in the manifest |
| `timeoutSeconds` | Per adapter invocation and per worktree checkout, integer from 1 to 3600; defaults to 60 |

Patterns use `/`, with `*` confined to one path segment and `**` spanning
segments. Absolute paths, backslashes, `.` and `..` segments are rejected.
Colons and negation patterns are rejected too. `{workspaceRoot}` tokens are not supported here.
Unknown settings and overlapping ownership of a changed source are configuration
errors. They stop selection rather than silently applying a different profile.

List every source whose change needs this interpretation, including operations
and generator configuration. Sources not covered by a projection keep their
normal affected behavior. A projection is skipped when none of its sources
changed. When it runs, it must return the **complete artifact manifest**, not
only artifacts it believes changed.

## Adapter protocol, version 1

qk resolves base/head to commits and runs **head's adapter for both snapshots**.
Each invocation gets a fresh detached head worktree for adapter code and a
separate fresh worktree for revision data. The command runs in the head
adapter's workspace directory. Read inputs and generate outputs under
`revisionRoot`, rather than assuming the current directory contains revision data.
Changing the adapter without changing `nx.json` therefore still uses the same
committed adapter to interpret both revisions. The original
checkout, branch, dirty files and generated output directories are left alone.
Worktree registration is temporary; qk removes the worktrees after comparison
or failure. Each projection gets fresh checkouts, so another adapter's writes
do not become its inputs.

The command receives one JSON object on stdin:

```json
{
  "version": 1,
  "workspaceRoot": "/workspace/project",
  "revisionRoot": "/tmp/qk-revision/checkout",
  "adapterRoot": "/tmp/qk-adapter/checkout",
  "revision": "0123456789abcdef0123456789abcdef01234567"
}
```

`workspaceRoot` is the original workspace, useful for locating a pinned tool
installation. `revisionRoot` is the isolated workspace containing data at the
requested commit. `adapterRoot` is the fresh head workspace and the command's
working directory. For a workspace nested inside a Git repository, all three
roots name the workspace subdirectory. Relative executable paths and arguments
are interpreted from `adapterRoot`; bare executable names are resolved through
PATH. An adapter imported by a Node script also comes from head.

qk expands `{workspaceRoot}`, `{revisionRoot}` and `{adapterRoot}` inside each
command argument, without shell parsing. For example, a wrapper can use
`["node", "tools/snapshot.mjs", "--input-root", "{revisionRoot}"]`.
Use `{adapterRoot}` for committed adapter code and `{workspaceRoot}` for tools
installed in the original workspace. Pointing adapter code at `{workspaceRoot}`
explicitly opts into the original checkout's code, including dirty edits.
These tokens apply to `command`, not `sources` or `outputs`.
qk supplies no task metadata or task sandbox; the adapter inherits the runner
environment and permissions. Git repository-local environment variables,
such as `GIT_DIR`, `GIT_INDEX_FILE` and per-repository config overrides, are
removed from qk's Git and adapter subprocesses so hook invocation cannot redirect
them to another checkout.

Write exactly one manifest to stdout. Write progress and diagnostics to stderr,
which qk forwards without mixing them into selection JSON:

```json
{
  "version": 1,
  "artifacts": {
    "apps/web/generated/operations.js": "sha256:035ad...",
    "packages/client/generated/possible-types.js": "sha256:cb900..."
  }
}
```

Artifact names are normalized workspace-relative paths. They must match
`outputs` and cannot name workspace or project metadata such as `nx.json`,
`package.json`, `project.json` or lockfiles. Empty segments, `.` or `..`,
backslashes, colons and NUL characters are rejected. Fingerprints are opaque,
nonempty strings of at most 1024 bytes; a content digest is a good choice. Fingerprints must be deterministic across
checkouts: exclude timestamps and absolute temporary paths, and normalize
ordering when it has no meaning in the representation.
Duplicate artifact names, unknown response fields, unsupported protocol
versions, and manifests larger than 1 MiB are rejected.

Artifacts can be virtual: the named file need not exist in the original
checkout, and it can be Git-ignored generated output. qk maps each changed
artifact path to the most specific containing project, then propagates through
normal project dependencies. Choose artifact names in the consumer's project
when that consumer is the unit of change. Naming a shared package artifact
instead affects that package and its dependents.

qk compares the union of both manifests. A changed fingerprint, newly present
artifact, or removed artifact contributes a changed path. The adapter must
include shared runtime artifacts as well as per-consumer artifacts; leaving
an artifact out of both snapshots cannot establish that it is unchanged.

## Tools and generated code

Disposable checkouts contain revision files and Git metadata. qk does not
install packages, initialize submodules, copy `node_modules`, or run checkout
hooks. Put external tools on PATH or let the adapter explicitly locate a
pinned installation through `workspaceRoot`. For a Node adapter, dependencies
can be resolved with `createRequire`:

```js
import { createRequire } from 'node:module'
import { join } from 'node:path'
const requireTool = createRequire(join(request.workspaceRoot, 'package.json'))
const ts = requireTool('typescript')
```

Use the same chosen tool installation to interpret both revisions. qk does
not reproduce historical installations. Installation metadata changing in
the comparison causes conservative fallback, and there is no persisted
projection cache in this version.

For a generator adapter:

1. Read the revision's generator configuration from `revisionRoot`.
2. Run the pinned generator there. Send its stdout to stderr so it cannot
   corrupt the manifest. Clean its declared generated output directories first
   if tracked generated files could otherwise survive a deletion.
3. Read every relevant generated output. For a JavaScript-runtime profile,
   erase TypeScript types using the chosen compiler and fingerprint the
   resulting JavaScript; keep non-TypeScript runtime files in the manifest too.
4. Emit workspace-relative output names and fingerprints.

The representation defines what the profile promises. Equal generated
JavaScript is not proof that types remain valid, that validation passes, or
that a complete bundle is equivalent. Keep validation and type checking on
ordinary affected selection. A broader deployment profile must account for
all other deployment inputs through normal changes or additional projections.

Adapters are trusted executable workspace tooling. Selecting a profile can
execute the adapter code committed at head with runner permissions.
Use an appropriate CI execution environment when reviewing untrusted changes.
The adapter must write generated files only in `revisionRoot`; qk's disposable
worktrees are isolation of files, not a permission sandbox.

## Conservative fallback and explanations

qk retains **all original changes for the profile** if any active adapter fails,
times out, emits invalid output, conflicts with another adapter's artifacts,
or emits an artifact overlapping a replaced source. This prevents a partial
successful projection from hiding changes after another projection failed.
Adapter process groups are stopped on exit and on failure, including timeout;
worktrees are then removed. Each worktree checkout also has its own
`timeoutSeconds` deadline; a stalled checkout is a comparison fallback, and
its process group and any registered worktree are cleaned up. Each active
projection creates four full checkouts (base and head data, plus fresh head
code for each invocation), so checkout cost can be significant in large
repositories. Adapter roots are writable and are not reused between runs.
An adapter that deliberately escapes its process
group is outside this cleanup contract.

Projection also falls back when:

- The comparison has no explicit committed head (`--head` or `NX_HEAD`).
- `--files`, `--stdin`, `--uncommitted` or `--untracked` replaces the revision diff.
- Workspace configuration or root installation metadata differs between commits.
- The current workspace metadata differs from the committed head.
- A declared source change would replace protected workspace/project metadata.
- A revision cannot be checked out, including an absent workspace subdirectory.

An untracked or Git-ignored `nx.local.json` selects the same merged profile for
both snapshots and is exempt from the metadata check. It is not copied into
revision worktrees. If an adapter needs local settings, read the common
original workspace's override through `workspaceRoot`. Tracked overrides
remain protected: changes between commits or dirty edits trigger fallback.

Fallback retains all original changes and prints a warning to stderr by default.
For preview CI, use `--fail-on-projection-fallback=adapter` to fail only when an
adapter exits unsuccessfully, cannot start, times out, emits invalid output or
undeclared artifacts, or conflicts with another adapter's artifacts:

```sh
qk show projects --affected --affected-profile runtime --base main --head HEAD \
  --fail-on-projection-fallback=adapter --json
```

Configuration or installation changes remain successful conservative
comparisons in this mode, including a commit first introducing a profile.
They print a one-line notice to stderr naming the projection and the reason,
so the job log explains why conservative selection built more previews.
Explanations also retain the reason. This lets normal dependency updates create
previews while output drift, such as a new generated project outside `outputs`,
fails CI.

The bare `--fail-on-projection-fallback` flag and
`--fail-on-projection-fallback=all` fail on **every** fallback condition,
including configuration changes and checkout failures.
Both modes require `--affected-profile` and a committed head via `--head` or
`NX_HEAD`. Missing head, `--files`, `--stdin`, `--uncommitted` or `--untracked`
are invocation errors in either strict mode, even when no declared sources
changed. Default mode keeps conservative fallback with a diagnostic for
unsupported comparisons. A valid comparison with no changed sources still
skips the adapter successfully. Invalid profile configuration always fails
selection.

`qk show affected --json` includes
`projections` with each adapter's status (`applied`, `skipped` or `fallback`),
replaced source paths, changed artifact paths and any fallback detail.
Fallback reports also include `fallbackKind`: `adapter` for adapter execution
or manifest failures, and `comparison` for configuration, installation,
unsupported comparison or checkout conditions. Text
explanations show both the original changed-file count (after ignore rules)
and the count after projection, followed by the changed artifacts. Explanation
JSON includes `originalFiles` and `files` when a profile is selected, including
when explaining a single project. `files` contains the paths used for selection.
Project-list JSON stays a plain array.

`--affected-profile` is rejected by task-level affected selection and
`show tasks --affected`. Task dependency propagation and cache inputs keep
their ordinary meanings.

## Try the example

The `examples/affected-projections` workspace uses only Node's standard library. Its adapter fingerprints each consumer's
`runtime` object in a JSON contract and ignores its `types` object. It does
not run a real code generator; the small representation makes the protocol
and project propagation visible.

Copy `examples/affected-projections` outside the qk repository, then:

```sh
git init -b main
git add .
git commit -m 'Initial contract'
```

Edit `schemas/contract.json`, change only `types.id`, and commit it. Ordinary
selection includes `schema` and `web`; the runtime profile returns an empty
array:

```sh
qk show projects --affected --base HEAD^ --head HEAD --json
qk show projects --affected --affected-profile runtime --base HEAD^ --head HEAD --json
```

Next change `runtime.operation` and commit it. The runtime profile includes
`web`, because `apps/web/generated/contract.js` has a different fingerprint.
`qk show affected web` with the same profile and revisions explains why.
