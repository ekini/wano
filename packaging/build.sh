#!/bin/sh
# Builds the release artifacts in containers: this machine's glibc is newer than
# the target's, so a binary built natively here would not start on Ubuntu.
#
#   packaging/build.sh deb    -> dist/wano_<version>-1_amd64.deb   (Ubuntu 24.04+)
#   packaging/build.sh snap   -> dist/wano_<version>_amd64.snap
set -eu

RUST=1.90.0
BUILDER=wano-build
SNAPCRAFT=ghcr.io/canonical/snapcraft:8_core24

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
owner=$(id -u):$(id -g)
mkdir -p "$root/dist"

# Nothing links against wayland, xkbcommon or EGL at build time — they are all
# dlopened — so no dev packages are needed. rustup is, because 24.04 ships
# rustc 1.75 and this crate is edition 2024.
builder() {
    docker build -q -t "$BUILDER" - >/dev/null <<EOF
FROM ubuntu:24.04
RUN apt-get -qq update \
 && apt-get -qq install -y --no-install-recommends ca-certificates curl build-essential \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/root/.cargo/bin:\$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain $RUST \
 && cargo install cargo-deb --locked
EOF
}

# target/ and the registry live in volumes, so the host's own build tree is left
# alone and a second run does not recompile the world.
build() {
    docker run --rm \
        -v "$root:/src" -v wano-target:/src/target -v wano-registry:/root/.cargo/registry \
        -w /src "$BUILDER" sh -euc "$1"
}

case ${1:-} in
deb)
    builder
    build 'cargo deb --locked --output dist/'
    ;;
snap)
    builder
    build 'cargo build --locked --release'
    # The image's entrypoint is snapcraft itself and it expects the project in
    # /project, so it takes only the args.
    docker run --rm -v "$root:/project" -v wano-target:/project/target "$SNAPCRAFT" \
        pack --destructive-mode --output dist
    ;;
*)
    echo "usage: $0 deb|snap" >&2
    exit 2
    ;;
esac

# The containers run as root; hand back what they left in the tree.
docker run --rm -v "$root:/src" "$BUILDER" \
    sh -c "cd /src && chown -R $owner dist parts stage prime overlay .craft 2>/dev/null || true"
ls -l "$root/dist"
