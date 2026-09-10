#!/usr/bin/env bash
# Build the kernel and package it for the target machine.
#
# ARCH selects the target: x86_64 (default), aarch64.
# PROFILE=release builds an optimised image.
#
# x86_64 gets a bootable ISO (Limine).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
PROFILE="${PROFILE:-debug}"

case "$ARCH" in
    x86_64)  TRIPLE="x86_64-unknown-none" ;;
    aarch64) TRIPLE="aarch64-unknown-none" ;;
    *) echo "unknown ARCH: $ARCH (want x86_64|aarch64)" >&2; exit 1 ;;
esac

if [[ "$PROFILE" == "release" ]]; then
    cargo build --release --target "$TRIPLE"
else
    cargo build --target "$TRIPLE"
fi

KERNEL="target/$TRIPLE/$PROFILE/mpos"
if [[ ! -f "$KERNEL" ]]; then
    echo "error: expected kernel at $KERNEL" >&2
    exit 1
fi

mkdir -p build

if [[ "$ARCH" == "x86_64" ]]; then
    "$ROOT/scripts/fetch-limine.sh"

    STAGE="build/iso-$ARCH"
    rm -rf "$STAGE"
    mkdir -p "$STAGE"

    # The user programs: compiled standalone with rustc rather than through
    # cargo, so they do not inherit the kernel's linker script. Statically
    # linked at a fixed base - exactly the ET_EXEC image the kernel's loader
    # accepts. Each includes the kernel's syscall table by path, so the two
    # can never disagree about a number; the shared libc in user/lib comes
    # along the same way.
    USERBIN="$STAGE/userbin"
    mkdir -p "$USERBIN"
    for src in "$ROOT"/user/bin/*.rs; do
        name="$(basename "$src" .rs)"
        rustc --edition 2024 -C panic=abort \
            --target x86_64-unknown-none \
            -C relocation-model=static \
            -C link-arg=--image-base=0x400000 \
            -C link-arg=--gc-sections \
            -o "$USERBIN/$name" \
            "$src" || exit 1
    done

    # The root filesystem: the initramfs tree plus the built programs, so the
    # source tree stays free of build artifacts.
    ROOTFS="$STAGE/rootfs"
    cp -r "$ROOT/initramfs" "$ROOTFS"
    mkdir -p "$ROOTFS/bin"
    cp "$USERBIN"/* "$ROOTFS/bin/"

    cp "$KERNEL"                             "$STAGE/mpos.elf"
    cp limine.conf                           "$STAGE/limine.conf"
    # The initramfs: an uncompressed ustar archive, so the kernel's parser
    # needs no decompressor and no format detection.
    tar --format=ustar -C "$ROOTFS" -cf "$STAGE/initramfs.tar" .
    cp third_party/limine/limine-bios-cd.bin "$STAGE/"
    cp third_party/limine/limine-uefi-cd.bin "$STAGE/"
    # limine-bios-cd.bin is only stages 1-2; it loads this as stage 3 by searching
    # the ISO root (or /boot, /limine, /boot/limine).
    cp third_party/limine/limine-bios.sys    "$STAGE/"

    ISO="build/mpos-$ARCH.iso"

    # Two El Torito boot entries: BIOS (default platform 0x00) and UEFI (0xef), so
    # the same image boots under SeaBIOS and under OVMF.
    #
    # This is cdrtools syntax - genisoimage on this system is a symlink to mkisofs.
    # cdrkit instead spells the UEFI entry `-e limine-uefi-cd.bin`; there is no `-e`
    # here, the alternate image is a second `-b` after the alt-boot/platform flags.
    mkisofs -quiet -o "$ISO" \
        -iso-level 3 -R -J \
        -b limine-bios-cd.bin -no-emul-boot -boot-load-size 4 -boot-info-table \
        -eltorito-alt-boot -eltorito-platform 0xef -b limine-uefi-cd.bin -no-emul-boot \
        "$STAGE"

    echo "built $ISO ($(du -h "$ISO" | cut -f1))"

    # The disk the kernel mounts: a small ext2 image holding seed files laid
    # down by mke2fs on the host, so the kernel reads something it did not
    # write before it starts writing files of its own. 8 MiB, 1 KiB blocks,
    # 128-byte inodes - the shape the ext2 driver and the gate assert on.
    SEED="$STAGE/seed"
    rm -rf "$SEED"
    mkdir -p "$SEED/docs"
    printf 'ext2 seed: formatted by mke2fs on the host, read by mpos\n' > "$SEED/seed.txt"
    printf 'nested file\n' > "$SEED/docs/note.txt"
    DISK="build/disk-$ARCH.img"
    rm -f "$DISK"
    truncate -s 8M "$DISK"
    mke2fs -q -F -t ext2 -b 1024 -I 128 -N 64 -d "$SEED" "$DISK" 8192
    echo "built $DISK ($(du -h "$DISK" | cut -f1))"
else
    cp "$KERNEL" "build/mpos-$ARCH.elf"
    echo "built build/mpos-$ARCH.elf"
fi
