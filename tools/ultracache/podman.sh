#!/bin/sh
# Runs qk's tests in a Linux container, where strace can record tasks.
# Arguments go to `cargo test`; by default it runs the Ultracache tests.
#
#   tools/ultracache/podman.sh
#   tools/ultracache/podman.sh -p qk-cli --test execution input_analysis
#
# Needs Podman. The build and Cargo's registry live in volumes, apart from
# the host's target directory.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
image=localhost/qk-ultracache
podman build --quiet --tag "$image" --file "$root/tools/ultracache/Containerfile" "$root/tools/ultracache" >/dev/null
if [ "$#" -eq 0 ]; then
    set -- -p qk-cli --test ultracache
fi
# strace traces the task's processes, which the default seccomp profile and
# capabilities of a container forbid. `--init` reaps the processes tests
# orphan, which would linger as zombies under cargo as the first process.
# Linking qk's tests takes more memory
# than a small Podman machine has with every job at once and full debug info,
# so both are reduced unless set.
exec podman run --rm --init \
    --cap-add SYS_PTRACE \
    --security-opt seccomp=unconfined \
    --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
    --env CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-line-tables-only}" \
    --volume "$root":/qk \
    --volume qk-ultracache-target:/qk/target \
    --volume qk-ultracache-cargo:/usr/local/cargo/registry \
    --volume qk-ultracache-rustup:/usr/local/rustup \
    "$image" cargo test --locked "$@"
