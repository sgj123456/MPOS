#!/usr/bin/env bash
# Build the kernel and package the whole system as one bootable disk image.
#
# The image is a partitioned disk, the way an installed Linux system is: a GPT
# with an EFI system partition at the front carrying the bootloader, the kernel
# and the root filesystem, and a small ext2 partition behind it that the kernel
# mounts as /mnt. QEMU boots it directly - no ISO, no optical drive, and no loop
# device on the host, which is why the FAT filesystem is written by
# scripts/mkfat.py rather than by mkfs.vfat plus a mount.
#
# ARCH selects the target: x86_64 (default), aarch64.
# PROFILE=release builds an optimised image.
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

if [[ "$ARCH" != "x86_64" ]]; then
    cp "$KERNEL" "build/mpos-$ARCH.elf"
    echo "built build/mpos-$ARCH.elf"
    exit 0
fi

"$ROOT/scripts/fetch-limine.sh"

# Everything the image is assembled from lives under two directories: `pack`
# holds what is *built* - the user programs, the unpacked root filesystem and
# the seed tree - and `tree` is the ESP itself, which ends up on the disk
# verbatim.
PACK="build/pack-$ARCH"
TREE="build/esp-tree-$ARCH"
rm -rf "$PACK" "$TREE"
mkdir -p "$PACK" "$TREE/EFI/BOOT" "$TREE/boot"

# The user programs: compiled standalone with rustc rather than through
# cargo, so they do not inherit the kernel's linker script. Statically
# linked at a fixed base - exactly the ET_EXEC image the kernel's loader
# accepts. Two kinds live here, and the case below picks between them: the
# mpos-native programs, which include the kernel's syscall table by path so
# the two can never disagree about a number and take the shared libc in
# user/lib along the same way; and the Linux ones, which are built for musl
# and reach the kernel through a real libc's own startup - those are the
# programs that hold the Linux ABI honest.
#
# Nothing here duplicates what the root filesystem already provides. A program
# mpos implements itself *and* Alpine ships - cat, echo, ls, sh and the rest of
# the coreutils - was deleted rather than shadowed: two implementations of
# `cat` means the one that runs is decided by an install-order accident, and
# the gate would be testing whichever won.
USERBIN="$PACK/userbin"
mkdir -p "$USERBIN"
for src in "$ROOT"/user/bin/*.rs; do
    name="$(basename "$src" .rs)"
    case "$name" in
        hello|init|t_stdargs|t_stdspawn)
            rustc --edition 2024 \
                --target x86_64-unknown-linux-musl \
                -C target-feature=+crt-static \
                -C relocation-model=static \
                -C link-arg=-no-pie \
                -C link-arg=-Wl,-z,norelro \
                -o "$USERBIN/$name" \
                "$src" || exit 1
            ;;
        *)
            rustc --edition 2024 -C panic=abort \
                --target x86_64-unknown-none \
                -C relocation-model=static \
                -C link-arg=--image-base=0x400000 \
                -C link-arg=--gc-sections \
                -o "$USERBIN/$name" \
                "$src" || exit 1
            ;;
    esac
done

# The root filesystem is Alpine's minirootfs, not a tree of this
# repository's making: the whole point of the port is that mpos boots into
# a real, ordinary userland layout - /etc, /lib, /usr, /var and a /bin full
# of names pointing at one busybox.
ALPINE="$(bash "$ROOT/scripts/fetch-alpine.sh" | tail -1)"
ROOTFS="$PACK/rootfs"
rm -rf "$ROOTFS"
mkdir -p "$ROOTFS"
tar -xzf "$ALPINE/alpine-minirootfs.tar.gz" -C "$ROOTFS" \
    --no-same-owner --no-same-permissions

# Swap the dynamic busybox for the static-pie one. The shipped busybox asks
# for /lib/ld-musl-x86_64.so.1, and mpos has no PT_INTERP support - it would
# refuse every applet. Same tree, same applets, runnable.
mkdir -p "$ALPINE/busybox"
tar -xzf "$ALPINE"/busybox-static-*.apk -C "$ALPINE/busybox" 2>/dev/null
cp "$ALPINE/busybox/bin/busybox.static" "$ROOTFS/bin/busybox"

# mpos's own programs go in beside Alpine's, and there are only two left: the
# kernel's `/sbin/init`, which is the one that knows about this kernel's
# process model, and `hello`, which M12 runs and Alpine has no equivalent of.
# Everything else mpos once shipped - sh, cat, echo, ls, true, false, sleep -
# is Alpine's now, and the source for each was deleted so there is exactly one
# of each on the system. `t_*` are the ABI conformance tests.
mkdir -p "$ROOTFS/bin"
for bin in "$USERBIN"/*; do
    name="$(basename "$bin")"
    cp "$bin" "$ROOTFS/bin/$name"
done
# Cross-compiled musl coreutils (uutils) binary, when available.
MUSL_COREUTILS="/tmp/coreutils/target/x86_64-unknown-linux-musl/release/coreutils"
if [[ -f "$MUSL_COREUTILS" ]]; then
    cp "$MUSL_COREUTILS" "$ROOTFS/bin/coreutils"
fi
# The mountpoint devfs is mounted over. Git cannot hold an empty directory,
# so the tree cannot be trusted to carry it.
mkdir -p "$ROOTFS/dev"

# --- what mpos needs Alpine's tree to say about itself ----------------------
#
# Alpine's shipped /etc/inittab assumes a bank of virtual consoles
# (tty1..tty6) and an openrc that is not in a minirootfs: left alone, init
# spends its life printing "can't run /sbin/openrc" and "can't open
# /dev/ttyN" and respawning both. This machine has one console - the serial
# port - and one thing to run on it, which is a shell that init keeps alive.
cat > "$ROOTFS/etc/inittab" <<'EOF'
# mpos: the serial console is the only console there is. No openrc in a
# minirootfs and no tty1..tty6 behind it - one shell, respawned forever.
::sysinit:/bin/hostname -F /etc/hostname
::respawn:-/bin/sh
::ctrlaltdel:/sbin/reboot
::shutdown:/bin/sync
EOF

# The network address is fixed, because there is exactly one shape the
# network takes here: QEMU's user-mode stack, which answers as 10.0.2.2 and
# forwards DNS at 10.0.2.3. The kernel claims 10.0.2.15/24 on its own; what
# userland has to be told is only who resolves names. The repositories are
# the plain-HTTP CDN - apk can speak TLS, but the handshake's retransmit
# timers have no business being part of a gate's variance.
cat > "$ROOTFS/etc/resolv.conf" <<'EOF'
nameserver 10.0.2.3
EOF
cat > "$ROOTFS/etc/apk/repositories" <<'EOF'
http://dl-cdn.alpinelinux.org/alpine/v3.24/main
http://dl-cdn.alpinelinux.org/alpine/v3.24/community
EOF

# The kernel, its configuration, and the root filesystem as one uncompressed
# ustar archive - the shape the kernel's own parser reads, with no decompressor
# and no format detection. The tree itself is not copied into the image: the
# archive is the root filesystem, and the ESP has no use for a second copy.
cp "$KERNEL" "$TREE/boot/mpos.elf"
cp limine.conf "$TREE/limine.conf"
tar --format=ustar -C "$ROOTFS" -cf "$TREE/boot/initramfs.tar" .
rm -rf "$ROOTFS"
cp third_party/limine/BOOTX64.EFI "$TREE/EFI/BOOT/BOOTX64.EFI"

# The EFI system partition, as its own image. It has to hold the root
# filesystem archive as well as the kernel, and that archive is the whole
# unpacked Alpine tree - around 70 MiB, since ustar stores it uncompressed so
# the kernel needs no decompressor to read it - so 128 MiB leaves room to grow
# without making the image awkward to copy.
ESP_MIB=128
ESP="build/esp-$ARCH.img"
rm -f "$ESP"
python3 "$ROOT/scripts/mkfat.py" "$ESP" "${ESP_MIB}M" MPOS-ESP "$TREE"

# The disk the kernel mounts as /mnt: a small ext2 image holding seed files
# laid down by mke2fs on the host, so the kernel reads something it did not
# write before it starts writing files of its own. 8 MiB, 1 KiB blocks,
# 128-byte inodes - the shape the ext2 driver and the gate assert on.
SEED="$PACK/seed"
rm -rf "$SEED"
mkdir -p "$SEED/docs"
printf 'ext2 seed: formatted by mke2fs on the host, read by mpos\n' > "$SEED/seed.txt"
printf 'nested file\n' > "$SEED/docs/note.txt"
DATA="build/data-$ARCH.img"
rm -f "$DATA"
truncate -s 8M "$DATA"
mke2fs -q -F -t ext2 -b 1024 -I 128 -N 64 -d "$SEED" "$DATA" 8192

# Finally the disk itself: a GPT, the ESP at 1 MiB - the offset every tool
# aligns the first partition to - and the data partition behind it. The
# partition images are spliced in whole, which is why both were built as
# standalone files first.
DISK="build/linux-$ARCH.img"
DATA_MIB=8
TOTAL_MIB=$((1 + ESP_MIB + DATA_MIB + 1))  # +1 MiB of head and tail for the GPT
rm -f "$DISK"
truncate -s "${TOTAL_MIB}M" "$DISK"
parted -s "$DISK" mklabel gpt
parted -s "$DISK" unit MiB mkpart ESP fat32 1 $((1 + ESP_MIB))
parted -s "$DISK" set 1 esp on
parted -s "$DISK" unit MiB mkpart root ext2 $((1 + ESP_MIB)) $((1 + ESP_MIB + DATA_MIB))

dd if="$ESP" of="$DISK" bs=1M seek=1 conv=notrunc status=none
dd if="$DATA" of="$DISK" bs=1M seek=$((1 + ESP_MIB)) conv=notrunc status=none

echo "built $DISK ($(du -h "$DISK" | cut -f1))"
parted -s "$DISK" unit MiB print
