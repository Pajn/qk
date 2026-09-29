# Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

The Cargo workspace contains ten crates:

| Crate | Responsibility |
| --- | --- |
| `qk-config` | Workspace discovery, JSONC/YAML parsing, typed project configuration and normalization |
| `qk-graph` | Workspace edges, project selection, reverse reachability and graph export |
| `qk-taskgraph` | Dependency expansion, configuration selection and task DAG validation |
| `qk-executor` | Command preparation, environment, argument interpolation, process groups and output capture |
| `qk-runner` | Bounded task scheduling, dependency failure propagation and cancellation |
| `qk-lockfile` | pnpm v9 lockfile parsing and what each importer installs |
| `qk-cache` | Input hashing, cache entry storage, output restoration and log replay |
| `qk-affected` | Git change analysis and project/task affected selection |
| `qk-history` | SQLite run history, task records and cache-key explanations |
| `qk-cli` | Argument parsing and output; produces the `qk` binary |

Fixture and CLI tests use self-contained workspaces.

### Parity with Nx

`tools/parity/parity.mjs` measures qk against Nx. By default it uses the
synthetic workspace in `tools/parity/fixture` and the goldens committed in
`tools/parity/goldens`, captured with the Nx release pinned in
`tools/parity/nx`. It needs Node, and pnpm for the cases that run package
scripts:

```sh
node tools/parity/parity.mjs compare --qk target/release/qk
```

Each run builds a scratch git repository from the fixture. The cases in
`tools/parity/cases.json` compare task graphs, `show projects --affected`
for a change applied on a branch (the files in `tools/parity/changes/<case>`,
plus any deletions the case lists), and runs, which execute tasks without
cache and compare whether the run failed and which tasks ran. The comparison
requires byte-identical `show projects --json`, a `graph --file` equal after
normalisation, and `--graph` task graphs with the same tasks, with the same
target, root, outputs and cache, continuous and parallelism flags, and the
same dependencies and continuous dependencies. The differences it normalises away are listed at the
top of the script. An affected or run case that differs fails unless it
records the exact difference it accepts and why.

After changing the fixture or the pinned Nx, recapture and commit the
goldens:

```sh
(cd tools/parity/nx && pnpm install)
node tools/parity/parity.mjs capture
```

`capture --check` captures into a scratch directory and fails when the
committed goldens differ. CI runs both `compare` and `capture --check`.

The harness can also point at any workspace with its own Nx installation
and git history, reading cases from `<goldens>/parity.json`, with affected
cases given as `base` and `head` revisions:

```sh
node tools/parity/parity.mjs capture --workspace <dir> --goldens <dir>
node tools/parity/parity.mjs compare --workspace <dir> --goldens <dir> --qk target/release/qk
```

For an external workspace, capture also records Nx's median wall times, and
compare prints them beside qk's.

### Releases

Pushing a tag `v<version>` that matches the workspace version runs
`.github/workflows/release.yml`: it builds qk for Linux x64 and arm64, macOS
arm64 and Windows x64 and attaches the binaries to a GitHub release. When the
repository variable `NPM_PACKAGE_NAME` names the npm package and the secret
`NPM_TOKEN` can publish it, the workflow also publishes the npm packages
`tools/npm/package.mjs` assembles: the main package, whose `qk` bin runs the
binary for the platform, and one package per platform as an optional
dependency. Without the variable nothing is published to npm.

### Benchmarks

`tools/bench/bench.mjs` generates a workspace of 300 projects, 3,300 files
and a 3,000-package pnpm lockfile, populates the cache, and measures with
hyperfine: `show projects`, `graph`, `show projects --affected` with no
changes, and a fully cached `run-many`. It needs hyperfine, git and Node:

```sh
node tools/bench/bench.mjs --qk target/release/qk [--projects 300] [--json out.json] [--markdown out.md]
```

CI runs it on every push, writing the table to the job summary and the
numbers to an artifact.

GitHub Actions is configured for Linux, macOS and Windows. The lockfile is
checked in for reproducible dependency resolution.

See [Maintaining the documentation](documentation.md) for book builds and Pages setup.
