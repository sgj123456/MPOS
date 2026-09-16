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

# alpine-make-rootfs insists on root - it sets file ownership for the rootfs
# and runs `apk --root` against it - and so does `mke2fs -d`, which copies the
# tree with its ownership intact. Re-exec under sudo rather than failing: the
# environment goes along, so cargo and the mirror keep working.
if [[ "$(id -u)" != "0" ]]; then
    echo "mkdisk builds device images and sets file ownership, so it runs as root."
    exec sudo -E env "HOME=$HOME" "PATH=$PATH" "$0" "$@"
fi

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
PACK="$ROOT/build/pack-$ARCH"
ESP_TREE="$ROOT/build/esp-tree-$ARCH"
BOOT_TREE="$ROOT/build/boot-tree-$ARCH"
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

# --- the two trees, both built by alpine-make-rootfs ------------------------
#
#   rootfs     the system the machine runs: Alpine with OpenRC, whose boot
#              runlevel mounts /proc, /sys and /dev, brings /run up, and
#              whose `localmount` works through /etc/fstab. The kernel mounts
#              none of these any more.
#   initramfs  the early environment: busybox, apk and an `/init` script that
#              mounts the canonical directories on the temporary root, mounts
#              the real rootfs from /dev/vda3, pivot_roots into it and execs
#              /sbin/init. Every mount after that is userland's decision,
#              made through `mount(2)` against the device nodes the devfs
#              publishes.
#
# Both runs execute a setup script (below) with $ROOTFS pointed at the tree;
# no chroot, so the only privilege used is that the files are owned by root.
ALPINE_BRANCH=v3.24
ALPINE_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/alpine
MAKE_ROOTFS="$ROOT/alpine-make-rootfs/alpine-make-rootfs"

# alpine-make-rootfs drives `apk`; the host has none, so the static build
# comes off the same mirror the packages do, out of the package that carries
# it. (The script's own default source is gitlab, which not every build
# sandbox reaches.)
APK_BIN="$(command -v apk || true)"
if [[ -z "$APK_BIN" ]]; then
    echo "fetching apk.static from $ALPINE_MIRROR"
    curl -sL -o "$PACK/apk-tools-static.apk" \
        "$ALPINE_MIRROR/$ALPINE_BRANCH/main/x86_64/apk-tools-static-3.0.8-r0.apk"
    mkdir -p "$PACK/apktools"
    # The package's headers carry apk's own PAX keys, which tar reads as
    # unknown and warns about; they hold nothing the extraction needs.
    tar -xzf "$PACK/apk-tools-static.apk" -C "$PACK/apktools" \
        --warning=no-unknown-keyword sbin/apk.static
    mv "$PACK/apktools/sbin/apk.static" "$PACK/apk.static"
    chmod +x "$PACK/apk.static"
    APK_BIN="$PACK/apk.static"
fi
export APK="$APK_BIN"
export APK_OPTS=""
# The guest's network path, when the build host reaches the network through a
# loopback proxy: setup-rootfs.sh writes it into /etc/profile for the guest.
# `${http_proxy:-}` because the build may run with the variable unset, and
# `set -u` turns an unbound reference into an error.
if [[ -n "${http_proxy:-}" ]]; then
    export GUEST_HTTP_PROXY_PORT="${http_proxy##*:}"
else
    export GUEST_HTTP_PROXY_PORT=""
fi

# The user programs the sweep runs come from the workspace: mpos-native
# no_std forms and, for the migrated tests, real musl binaries speaking std
# and rustix. They go into *both* trees - the sweep runs from the initramfs,
# and a booted machine wants its own tests on hand.
USER_TARGET="x86_64-unknown-linux-musl"
cargo build --release -p mpos-user --target "$USER_TARGET"
USER_BINS="target/$USER_TARGET/release"

copy_programs() {
    local dest="$1"
    mkdir -p "$dest/bin"
    for bin in "$USERBIN"/*; do
        cp "$bin" "$dest/bin/$(basename "$bin")"
    done
    if [[ -d "$USER_BINS" ]]; then
        for bin in "$USER_BINS"/*; do
            # Only real executables: the same directory holds `.d` depfiles
            # and incremental artefacts.
            [[ -f "$bin" && -x "$bin" ]] || continue
            name="$(basename "$bin")"
            [[ "$name" == *.* ]] && continue
            cp "$bin" "$dest/bin/$name"
        done
    fi
    # A cross-compiled uutils coreutils, when the host happens to have one.
    if [[ -f /tmp/coreutils/target/x86_64-unknown-linux-musl/release/coreutils ]]; then
        cp /tmp/coreutils/target/x86_64-unknown-linux-musl/release/coreutils "$dest/bin/coreutils"
    fi
    # Stripped: the initramfs is one ustar that Limine loads into memory and
    # the kernel unpacks page by page, so a test binary's symbol tables cost
    # boot time and boot volume for nothing. These programs are the tests,
    # not the thing under test.
    strip "$dest/bin/"* 2>/dev/null || true
}

# --- rootfs -----------------------------------------------------------------
ROOTFS="$PACK/rootfs"
# The previous run's tree is root-owned with a 0700 /root (Alpine's own
# baselayout), which makes cleaning it up depend on who is running this. Make
# it deletable by whoever runs the build before taking it down.
chmod -R a+rX "$ROOTFS" 2>/dev/null || true
rm -rf "$ROOTFS"
sh "$MAKE_ROOTFS" \
    --branch "$ALPINE_BRANCH" \
    --mirror-uri "$ALPINE_MIRROR" \
    --packages "openrc" \
    "$ROOTFS" "$ROOT/scripts/setup-rootfs.sh"

copy_programs "$ROOTFS"

# --- initramfs --------------------------------------------------------------
#
# Built by Alpine's own mkinitfs, inside the Alpine docker image: that is what
# produces the standard initramfs layout - busybox and its applets, the
# mkinitfs helper scripts, a /init - and it is the tool the distribution uses
# for its own kernels.
#
# Two facts shape what happens to its output:
#
#   * mkinitfs writes a gzipped cpio archive, and this kernel's unpacker reads
#     ustar. The archive is extracted on the host and repacked below.
#   * its /init finds the root device over netlink uevents and mounts what
#     /proc/cmdline names. This kernel has no uevent socket, so the /init that
#     runs is scripts/initramfs-init - injected here, with mkinitfs's own kept
#     at /init.mkinitfs for the day that changes.
#
# Without a docker daemon the tree is built by alpine-make-rootfs instead: the
# same shape, a different producer, and the same injected init.
if [[ -n "${MPOS_INITRAMFS_DIR:-}" ]]; then
    INITRAMFS_DIR="$(mktemp -d "${MPOS_INITRAMFS_DIR%/}.XXXXXX")"
else
    INITRAMFS_DIR="$PACK/initramfs_root"
    rm -rf "$INITRAMFS_DIR"
fi
mkdir -p "$INITRAMFS_DIR"

DOCKER_OUT="$PACK/docker"
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    echo "initramfs: mkinitfs, in alpine:$ALPINE_BRANCH"
    rm -rf "$DOCKER_OUT"
    mkdir -p "$DOCKER_OUT"
    docker run --rm -v "$DOCKER_OUT:/out" "alpine:latest" sh -euxc '
        apk add --no-cache mkinitfs
        # The kernel-version argument names where modules would be read from.
        # This kernel has none, and an empty directory is enough for mkinitfs
        # to write the archive it is asked for.
        mkdir -p /lib/modules/0.0.0
        printf "features=\"base\"\n" > /etc/mkinitfs/mkinitfs.conf
        mkinitfs -o /out/initramfs.cpio.gz 0.0.0
    '
    python3 "$ROOT/scripts/cpio-extract.py" "$DOCKER_OUT/initramfs.cpio.gz" "$INITRAMFS_DIR"
else
    echo "initramfs: no docker daemon; building the tree with alpine-make-rootfs"
    sh "$MAKE_ROOTFS" \
        --branch "$ALPINE_BRANCH" \
        --mirror-uri "$ALPINE_MIRROR" \
        --packages "alpine-release" \
        "$INITRAMFS_DIR"
fi

# The canonical directories, the parking place for the real root, the release
# stamp the boot's own check reads, and mkinitfs's init kept aside.
ROOTFS="$INITRAMFS_DIR" \
    ROOTFS_RELEASE="$(cat "$PACK/rootfs/etc/alpine-release" 2>/dev/null || echo "$ALPINE_BRANCH")" \
    sh "$ROOT/scripts/setup-initramfs.sh"

# The init that runs, and the static apk the boot's ABI check runs: the
# initramfs has no package store to consult for either.
install -m 0755 "$ROOT/scripts/initramfs-init" "$INITRAMFS_DIR/init"
if [[ -f "$APK_BIN" ]]; then
    mkdir -p "$INITRAMFS_DIR/sbin"
    cp "$APK_BIN" "$INITRAMFS_DIR/sbin/apk"
    chmod +x "$INITRAMFS_DIR/sbin/apk"
fi

copy_programs "$INITRAMFS_DIR"

tar --format=ustar --owner=root --group=root --numeric-owner \
    -cf "$BOOT_TREE/boot/initramfs.tar" -C "$INITRAMFS_DIR" .
chmod -R a+rwX "$INITRAMFS_DIR" 2>/dev/null || true
rm -rf "$INITRAMFS_DIR" "$DOCKER_OUT" 2>/dev/null || true

# The ESP carries only the bootloader and its config. Limine looks for
# limine.conf in the ESP root.
cp third_party/limine/BOOTX64.EFI "$ESP_TREE/EFI/BOOT/BOOTX64.EFI"
cp limine.conf "$ESP_TREE/limine.conf"

# The boot partition carries the kernel, initramfs, and a copy of limine.conf.
# This is what Limine loads files from via boot():.
cp "$KERNEL" "$BOOT_TREE/boot/mpos.elf"
cp limine.conf "$BOOT_TREE/limine.conf"

# The root filesystem image: the tree alpine-make-rootfs built, laid down at
# format time by mke2fs. 384 MiB - a full Alpine with OpenRC and the test
# programs wants more than the 256 the minirootfs got.
ROOTFS_MIB=384
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
BOOT_MIB=192
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

# The trees this build made are root-owned, and the next build takes them
# down. Leave them world-traversable so cleanup never depends on who ran the
# last build - a 0700 /root inside a build tree is the previous run's business,
# not the next one's.
chmod -R a+rwX "$PACK" "$ESP_TREE" "$BOOT_TREE" 2>/dev/null || true

# The final images are root-owned too, and run.sh opens them as the invoking
# user; without this QEMU fails with "Could not open ...: Permission denied".
# World-writable on the whole-image files only - everything else under build/
# is covered above.
chmod a+rw "$ROOTFS_IMG" "$DATA" "$ESP" "$BOOT_IMG" "$DISK" 2>/dev/null || true
