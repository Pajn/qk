#!/bin/sh
# Builds the recorder's kernel program for each architecture qk records on.
# The objects are committed beside the source, so that building qk needs no
# clang; CI rebuilds them in the same image and checks they match. Run it
# through `tools/trace/build.sh`, which runs it in that image with the
# repository at `/qk`: the objects name their source's path, so they match
# only when built from the same one.
set -eu
cd "$(dirname "$0")"
for arch in x86 arm64; do
    # asm/types.h, which linux/types.h includes, is in the build host's
    # multiarch directory; every architecture's defines the same types.
    clang -O2 -g -target bpf -D__TARGET_ARCH_$arch \
        -I "/usr/include/$(uname -m)-linux-gnu" -c trace.bpf.c -o trace.$arch.o
    # Debug line information makes the object depend on where it was built;
    # the BTF the loader relocates by stays.
    llvm-strip -g trace.$arch.o
done
