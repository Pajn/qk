#!/bin/sh
# Builds the recorder's kernel program, `crates/qk-trace/bpf/trace.bpf.c`,
# into the objects committed beside it, in `Containerfile`'s image.
#
#   tools/trace/build.sh
#
# Needs Podman.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
image=localhost/qk-trace-build
podman build --quiet --tag "$image" --file "$root/tools/trace/Containerfile" "$root/tools/trace" >/dev/null
# SELinux, where the host enforces it, denies a container the repository it
# mounts unless labelling is turned off.
exec podman run --rm --security-opt label=disable --volume "$root":/qk "$image" crates/qk-trace/bpf/build.sh
