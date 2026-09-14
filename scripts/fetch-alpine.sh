#!/usr/bin/env bash
# Fetch the Alpine pieces the root filesystem is built from.
#
# Two artifacts, both cached under build/alpine so a rebuild does not touch the
# network again:
#
#   * alpine-minirootfs - a complete, minimal Alpine root tree: /etc, /lib, the
#     skeleton directories, and one busybox with a few hundred names pointing at
#     it. This is what "the root filesystem" means here.
#   * busybox-static    - the same busybox linked without a dynamic loader.
#
# The second one is not redundant. The busybox inside the minirootfs is a
# dynamic PIE that asks for /lib/ld-musl-x86_64.so.1, and mpos's loader has no
# PT_INTERP support: it would refuse every applet. Swapping in the static-pie
# build keeps the tree Alpine's and makes the programs in it runnable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/build/alpine"

# The official CDN is reachable but slow from here; the mirror is the same
# bytes. Override with ALPINE_MIRROR if you have a better one.
MIRROR="${ALPINE_MIRROR:-https://mirrors.aliyun.com/alpine/latest-stable}"

# Bump these together: busybox-static has to come from the same release as the
# minirootfs, or the version skew shows up as applets that are not there.
RELEASE="3.24.0"
BUSYBOX="busybox-static-1.37.0-r31.apk"

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
fetch "$MIRROR/main/x86_64/$BUSYBOX" \
      "$DEST/$BUSYBOX"

echo "$DEST"
