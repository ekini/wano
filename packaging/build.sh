#!/bin/sh
# Builds and publishes the release artifacts in containers: this machine's glibc
# is newer than the target's, so a binary built natively here would not start on
# Ubuntu, and there is no snapd to build or upload a snap with.
#
#   packaging/build.sh deb              -> dist/wano_<version>-1_amd64.deb  (Ubuntu 24.04+)
#   packaging/build.sh snap             -> dist/wano_<version>_amd64.snap
#   packaging/build.sh publish [chan]   uploads that snap, released to edge unless told otherwise
#   packaging/build.sh store <args>     any other snapcraft command: login, register wano, status wano
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

# The image's entrypoint is snapcraft itself, and it expects the project in
# /project, so both of these take only the arguments. Store commands need a
# terminal for the login flow and a home for the credentials it leaves behind;
# SNAPCRAFT_STORE_CREDENTIALS is honoured too, for a login exported elsewhere.
pack() {
    docker run --rm -v "$root:/project" -v wano-target:/project/target "$SNAPCRAFT" \
        pack --destructive-mode --output dist
}

store() {
    mkdir -p "$HOME/.local/share/snapcraft"
    docker run --rm -it -v "$root:/project" \
        -v "$HOME/.local/share/snapcraft:/root/.local/share/snapcraft" \
        -e SNAPCRAFT_STORE_CREDENTIALS "$SNAPCRAFT" "$@"
}

case ${1:-} in
deb)
    builder
    build 'cargo deb --locked --output dist/'
    ;;
snap)
    builder
    build 'cargo build --locked --release'
    pack
    ;;
publish)
    snap=$(ls -t "$root"/dist/wano_*.snap 2>/dev/null | head -1)
    test -n "$snap" || {
        echo "no snap in dist/: run $0 snap first" >&2
        exit 1
    }
    store upload --release="${2:-edge}" "dist/$(basename "$snap")"
    exit 0
    ;;
store)
    shift
    store "$@"
    exit 0
    ;;
*)
    sed -n '5,9s/^# \?//p' "$0" >&2
    exit 2
    ;;
esac

# The containers run as root; hand back what they left in the tree.
docker run --rm -v "$root:/src" "$BUILDER" \
    sh -c "cd /src && chown -R $owner dist parts stage prime overlay .craft 2>/dev/null || true"
ls -l "$root/dist"
