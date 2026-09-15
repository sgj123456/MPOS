#!/usr/bin/env bash
# Fetch the Alpine pieces the root filesystem is built from.
#
# One artifact, cached under build/alpine so a rebuild does not touch the
# network again:
#
#   * alpine-minirootfs - a complete, minimal Alpine root tree: /etc, /lib, the
#     skeleton directories, and one busybox with a few hundred names pointing at
#     it. This is what "the root filesystem" means here.
#
# There used to be a second: `busybox-static`, swapped in over the busybox the
# minirootfs ships, because that one is a dynamic PIE asking for
# /lib/ld-musl-x86_64.so.1 and the kernel had no PT_INTERP support to load it.
# The kernel loads interpreters now, and the swap turned out to cost a working
# HTTPS: the static build's `wget` cannot complete a TLS handshake, while the
# shipped busybox delegates TLS to the `ssl_client` applet the minirootfs also
# ships. So the tree is Alpine's, unmodified.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/build/alpine"

# The official CDN is reachable but slow from here; the mirror is the same
# bytes. Override with ALPINE_MIRROR if you have a better one.
MIRROR="${ALPINE_MIRROR:-https://mirrors.aliyun.com/alpine/latest-stable}"

RELEASE="3.24.0"

mkdir -p "$DEST"

fetch() {
    local url="$1" out="$2"
    if [[ -f "$out" ]]; then
        echo "cached $(basename "$out")"
        return
    fi
    echo "fetching $(basename "$out")"
    curl -sSL --fail --retry 3 -o "$out.part" "$url"
    mv "$out.part" "$out"
}

fetch "$MIRROR/releases/x86_64/alpine-minirootfs-$RELEASE-x86_64.tar.gz" \
      "$DEST/alpine-minirootfs.tar.gz"

echo "$DEST"
