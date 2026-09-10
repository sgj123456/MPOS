#!/usr/bin/env bash
# Fetch the pinned Limine release binaries into third_party/limine.
#
# The tag must match boot::limine::BASE_REVISION: Limine v9.6.7 defines
# SUPPORTED_BASE_REVISION 3, which is what the kernel requests.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/third_party/limine"
TAG="${LIMINE_TAG:-v9.6.7-binary}"
REPO="https://github.com/limine-bootloader/limine.git"

ARTIFACTS=(
    limine-bios-cd.bin
    limine-uefi-cd.bin
    limine-bios.sys
    limine.h
    BOOTX64.EFI
    BOOTRISCV64.EFI
    BOOTAA64.EFI
)

mkdir -p "$DEST"

missing=0
for a in "${ARTIFACTS[@]}"; do
    [[ -f "$DEST/$a" ]] || missing=1
done
if [[ $missing -eq 0 ]]; then
    echo "limine $TAG artifacts already present in third_party/limine"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "cloning limine $TAG"
git clone --quiet --depth 1 --branch "$TAG" "$REPO" "$TMP/limine"

for a in "${ARTIFACTS[@]}"; do
    if [[ ! -f "$TMP/limine/$a" ]]; then
        echo "error: $TAG does not contain $a" >&2
        exit 1
    fi
    cp "$TMP/limine/$a" "$DEST/$a"
done

echo "installed ${#ARTIFACTS[@]} limine artifacts from $TAG"
