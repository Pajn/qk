# Ultracache fixture

A workspace for `crates/qk-cli/tests/ultracache.rs`. `app:build` reads its
`src` files, lists `icons` through a shell glob, looks for an optional
`config.local`, checks that `src/checked.txt` exists without reading it,
and lists its own directory. It keys `{projectRoot}/**/*`,
so without a recording any edit in `app` misses; with one, only what the
build read, listed or looked for does.
