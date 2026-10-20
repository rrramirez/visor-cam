#!/usr/bin/env bash
# Builds, tests and lints the whole workspace on Linux inside Docker.
#
#   scripts/linux-check.sh            # build + test + clippy
#   scripts/linux-check.sh cargo ...  # run any command in the container
#
# The cargo registry and the Linux target dir (target/linux) live in named
# volumes so they survive between runs and never clobber the host's macOS
# `target/`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
image="visor-linux-check"

docker build -q -t "$image" -f "$root/docker/linux.Dockerfile" "$root/docker" >/dev/null

run() {
    docker run --rm \
        -v "$root:/src" \
        -v visor-cargo-registry:/usr/local/cargo/registry \
        -v visor-cargo-git:/usr/local/cargo/git \
        -v visor-linux-target:/src/target/linux \
        -e CARGO_TARGET_DIR=/src/target/linux \
        -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}" \
        -w /src \
        "$image" "$@"
}

if [ "$#" -gt 0 ]; then
    run "$@"
    exit
fi

run bash -euxc '
    cargo build --workspace
    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
'
