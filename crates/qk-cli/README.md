# qk

qk (quick) is a standalone task runner written in Rust. It reads
Nx-compatible workspace configuration and supports dependency ordering,
affected selection, task caching and run history.

## npm installation

```sh
npm install --global runqk
qk --help
```

The npm package is `runqk`; its command is `qk`. Platform binaries are
installed as optional dependencies from the `@runqk` organization.
Supported platforms are Linux x64 and arm64, macOS arm64 and Windows x64.

```sh
qk show projects
qk run web:build
qk run-many -t build,test --parallel 4
qk affected -t build,test --base main
```

The `qk-cli` 0.0.0 package on crates.io is a metadata-only prerelease, not
an installable CLI. npm installation becomes available with the first
published `runqk` release.

Licensed under either MIT or Apache-2.0, at your option.
