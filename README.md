# qk

**qk**, short for **quick**, is a standalone task runner written in Rust.
It reads Nx-compatible workspace configuration and executes tasks with
dependency ordering, affected selection and a local cache shared by Git
worktrees. It also supports S3-compatible remote caching, reusable warm state
and run history.

## Documentation

The [documentation](docs/src/index.md) contains guides and reference pages:

- [Getting started](docs/src/getting-started.md)
- [Running tasks](docs/src/guides/running-tasks.md) and [affected selection](docs/src/guides/affected.md)
- [Local caching](docs/src/guides/cache.md), [remote caching](docs/src/guides/remote-cache.md) and [warm state](docs/src/guides/warm-state.md)
- [CLI reference](docs/src/reference/cli.md)
- [Workspace configuration](docs/src/reference/workspace.md), [project configuration](docs/src/reference/projects.md) and [target configuration](docs/src/reference/targets.md)
- [Nx compatibility](docs/src/compatibility.md)
- [Architecture](docs/src/architecture.md) and [development](docs/src/development.md)

The [implementation plan](docs/task-runner-design.md) records design decisions and
development milestones; the book documents the current implementation.

## Try it

Install a current stable Rust toolchain, then run from this repository:

```sh
cargo run -p qk-cli -- --workspace examples/basic show projects
cargo run -p qk-cli -- --workspace examples/basic show project web --json
cargo run -p qk-cli -- --workspace examples/basic show projects --projects 'tag:scope:*' --json
cargo run -p qk-cli -- --workspace examples/basic graph --file -
cargo run -p qk-cli -- --workspace examples/basic run codegen:smoke
```

Inspection and the `codegen:smoke` task are self-contained; no Node
installation, package installation, Nx process is required. Other example targets demonstrate package scripts
and require pnpm when executed.

To install the binary locally:

```sh
cargo install --path crates/qk-cli --locked
qk --help
qk --workspace /path/to/your/workspace show projects --json
```

Inside a workspace, omit `--workspace`. qk searches ancestors for `nx.json`,
`pnpm-workspace.yaml` or a root `package.json` with `workspaces`. A standalone
`project.json`, or a package with an `nx` object, is also supported when no
parent workspace marker exists.

## Build and preview the documentation

```sh
cargo install mdbook --version 0.5.4 --locked
mdbook serve docs --open
```

The book includes search and builds into the ignored `docs/book/` directory.
The documentation workflow checks pull requests and deploys the default
branch to GitHub Pages. See [Pages setup and documentation maintenance](docs/src/documentation.md)
for the one-time repository setting and contribution instructions.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

See [Development](docs/src/development.md) for Nx parity checks, releases
and benchmarks.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
