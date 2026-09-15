#!/usr/bin/env bash
# Build the kernel and package the whole system as one bootable disk image.
#
# Disk layout (GPT):
#   Partition 1: ESP (34 MiB, FAT32) - bootloader only
#   Partition 2: Boot (34 MiB, FAT32) - kernel + initramfs (Limine boot():)
#   Partition 3: Root (256 MiB, ext2) - root filesystem
#   Partition 4: Data (8 MiB, ext2) - mounted at /mnt
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
ESP_TREE="build/esp-tree-$ARCH"
BOOT_TREE="build/boot-tree-$ARCH"
rm -rf "$PACK" "$ESP_TREE" "$BOOT_TREE"
mkdir -p "$PACK" "$ESP_TREE/EFI/BOOT" "$BOOT_TREE/boot"

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
        hello|t_stdargs|t_stdspawn)
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

# Alpine's own busybox is kept, exactly as the distribution ships it.
#
# It used to be swapped for the `busybox-static` build, because the dynamic one
# asks for /lib/ld-musl-x86_64.so.1 and the kernel had no PT_INTERP support, so
# every applet was refused. The kernel loads interpreters now - `apk` is a
# dynamically linked program and runs - and the swap turned out to cost more
# than it saved: the static build's `wget` has an HTTPS path that cannot
# complete a handshake, while the shipped busybox delegates TLS to the
# `ssl_client` applet the minirootfs ships as /bin/ssl_client. With the swap in
# place, `wget https://...` failed on a machine whose TLS was never at fault.
#
# So: the stock busybox, the stock applet links, and HTTPS that works.

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
# The programs the workspace builds: ordinary Rust binaries for musl with
# `std` and `rustix`, statically linked. They are the ones under
# `user/src/bin`, and cargo builds them all in one call - which is the point
# of the workspace. Anything here wins over a same-named program compiled
# from `user/bin` above, because that is the hand-built no_std form a
# migrated test leaves behind.
# Always release, whatever the kernel is built as. A debug-profile musl
# binary is ~9 MB of debug info per program, and the root filesystem is one
# uncompressed ustar archive that has to fit in the ESP - four fat programs
# are enough to push the tree past it, which shows up as a root filesystem
# that unpacks short of its own /etc. These programs are the tests, not the
# thing under test: nobody reads their symbols.
USER_TARGET="x86_64-unknown-linux-musl"
cargo build --release -p mpos-user --target "$USER_TARGET"
USER_BINS="target/$USER_TARGET/release"
if [[ -d "$USER_BINS" ]]; then
    for bin in "$USER_BINS"/*; do
        # Only real executables: the same directory holds `.d` depfiles,
        # incremental artefacts and the build scripts' outputs.
        [[ -f "$bin" && -x "$bin" ]] || continue
        name="$(basename "$bin")"
        [[ "$name" == *.* ]] && continue
        cp "$bin" "$ROOTFS/bin/$name"
    done
fi

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

# The development sandbox only lets HTTP out through a loopback proxy, and
# QEMU's slirp answers the guest's 10.0.2.2 with the host's loopback - so
# the same proxy, renumbered, is what the guest's fetchers (apk, busybox
# wget) use. The port comes from the build environment's own http_proxy:
# it rotates per sandbox invocation, which is why the image is rebuilt in
# the same command that runs the probe. Outside such a sandbox nothing is
# appended and the guest talks to the network directly.
if [[ -n "${http_proxy:-}" ]]; then
    PROXY_PORT="${http_proxy##*:}"
    cat >> "$ROOTFS/etc/profile" <<EOF

# mpos build-time wiring: this host reaches the network only through a
# loopback proxy, and slirp answers 10.0.2.2 with that loopback.
export http_proxy=http://10.0.2.2:${PROXY_PORT}
export https_proxy=http://10.0.2.2:${PROXY_PORT}
EOF
fi

# The ESP carries only the bootloader and its config. Limine looks for
# limine.conf in the ESP root. FAT32 requires at least ~33 MiB.
cp third_party/limine/BOOTX64.EFI "$ESP_TREE/EFI/BOOT/BOOTX64.EFI"
cp limine.conf "$ESP_TREE/limine.conf"

# The boot partition carries the kernel, initramfs, and a copy of limine.conf.
# This is what Limine loads files from via boot():.
cp "$KERNEL" "$BOOT_TREE/boot/mpos.elf"
cp limine.conf "$BOOT_TREE/limine.conf"

# A minimal initramfs: the kernel unpacks it as the temporary root, then
# pivot_root's to the real rootfs on partition 3. The archive needs to exist
# and be valid; its contents are minimal because the kernel creates most
# directories it needs (new_root, old_root) dynamically.
INITRAMFS_DIR="$PACK/initramfs_root"
rm -rf "$INITRAMFS_DIR"
mkdir -p "$INITRAMFS_DIR/dev"
tar --format=ustar --owner=root --group=root --numeric-owner -cf "$BOOT_TREE/boot/initramfs.tar" -C "$INITRAMFS_DIR" .
rm -rf "$INITRAMFS_DIR"

# The root filesystem: Alpine's minirootfs as an ext2 image. This is the
# real root filesystem that the kernel will pivot_root to after booting
# from the initramfs. Use mke2fs -d to populate at format time.
ROOTFS_MIB=256
ROOTFS_IMG="build/rootfs-$ARCH.img"
rm -f "$ROOTFS_IMG"
mke2fs -q -F -t ext2 -b 4096 -I 256 -d "$ROOTFS" "$ROOTFS_IMG" $((ROOTFS_MIB * 1024 / 4))
rm -rf "$ROOTFS"

# The data partition: a small ext2 image holding seed files laid down by
# mke2fs on the host, so the kernel reads something it did not write before
# it starts writing files of its own. 8 MiB, 1 KiB blocks, 128-byte inodes
# - the shape the ext2 driver and the gate assert on.
SEED="$PACK/seed"
rm -rf "$SEED"
mkdir -p "$SEED/docs"
printf 'ext2 seed: formatted by mke2fs on the host, read by mpos\n' > "$SEED/seed.txt"
printf 'nested file\n' > "$SEED/docs/note.txt"
DATA="build/data-$ARCH.img"
rm -f "$DATA"
truncate -s 8M "$DATA"
mke2fs -q -F -t ext2 -b 1024 -I 128 -N 64 -d "$SEED" "$DATA" 8192

# The EFI system partition, as its own image: just the bootloader. FAT32
# requires at least ~33 MiB (65525 clusters with FAT overhead).
ESP_MIB=34
ESP="build/esp-$ARCH.img"
rm -f "$ESP"
python3 "$ROOT/scripts/mkfat.py" "$ESP" "${ESP_MIB}M" MPOS-ESP "$ESP_TREE"

# The boot partition: kernel, limine config, and initramfs archive. FAT32,
# because Limine's boot(): protocol only supports FAT. FAT32 requires at
# least ~33 MiB (65525 clusters with FAT overhead).
BOOT_MIB=64
BOOT_IMG="build/boot-$ARCH.img"
rm -f "$BOOT_IMG"
python3 "$ROOT/scripts/mkfat.py" "$BOOT_IMG" "${BOOT_MIB}M" MPOS-BOOT "$BOOT_TREE"

# Finally the disk itself: a GPT with four partitions.
#   Partition 1: ESP (34 MiB, FAT32) - bootloader only
#   Partition 2: Boot (34 MiB, FAT32) - kernel + initramfs
#   Partition 3: Root (256 MiB, ext2) - root filesystem
#   Partition 4: Data (8 MiB, ext2) - mounted at /mnt
DISK="build/linux-$ARCH.img"
DATA_MIB=8
TOTAL_MIB=$((1 + ESP_MIB + BOOT_MIB + ROOTFS_MIB + DATA_MIB + 1))  # +1 MiB of head and tail for the GPT
rm -f "$DISK"
truncate -s "${TOTAL_MIB}M" "$DISK"
parted -s "$DISK" mklabel gpt
parted -s "$DISK" unit MiB mkpart ESP fat32 1 $((1 + ESP_MIB))
parted -s "$DISK" set 1 esp on
# Partition 2 is what `boot():/` in limine.conf names.
parted -s "$DISK" unit MiB mkpart boot fat32 $((1 + ESP_MIB)) $((1 + ESP_MIB + BOOT_MIB))
# Partition 3 is the root filesystem ext2.
parted -s "$DISK" unit MiB mkpart rootfs ext2 \
    $((1 + ESP_MIB + BOOT_MIB)) $((1 + ESP_MIB + BOOT_MIB + ROOTFS_MIB))
# Partition 4 is the data partition.
parted -s "$DISK" unit MiB mkpart data ext2 \
    $((1 + ESP_MIB + BOOT_MIB + ROOTFS_MIB)) $((1 + ESP_MIB + BOOT_MIB + ROOTFS_MIB + DATA_MIB))

dd if="$ESP" of="$DISK" bs=1M seek=1 conv=notrunc status=none
dd if="$BOOT_IMG" of="$DISK" bs=1M seek=$((1 + ESP_MIB)) conv=notrunc status=none
dd if="$ROOTFS_IMG" of="$DISK" bs=1M seek=$((1 + ESP_MIB + BOOT_MIB)) conv=notrunc status=none
dd if="$DATA" of="$DISK" bs=1M seek=$((1 + ESP_MIB + BOOT_MIB + ROOTFS_MIB)) conv=notrunc status=none

echo "built $DISK ($(du -h "$DISK" | cut -f1))"
parted -s "$DISK" unit MiB print
