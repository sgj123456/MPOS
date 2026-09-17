#!/usr/bin/env bash
# Build the kernel and package the whole system as one bootable disk image.
#
# Disk layout (GPT):
#   Partition 1: ESP (34 MiB, FAT32) - bootloader only
#   Partition 2: Boot (34 MiB, FAT32) - kernel + initramfs (Limine boot():)
#   Partition 3: Root (256 MiB, ext2) - root filesystem
#   Partition 4: Data (8 MiB, ext2) - mounted at /mnt
#
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

ARCH="x86_64"
PROFILE="${PROFILE:-debug}"

TRIPLE="x86_64-unknown-none"

if [[ "$PROFILE" == "release" ]]; then
    cargo build --release --target "$TRIPLE"
else
    cargo build --target "$TRIPLE"
fi

# Cargo puts artefacts in ./target unless CARGO_TARGET_DIR says otherwise -
# which is how a build on a slow or read-only tree points them elsewhere.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
KERNEL="$TARGET_DIR/$TRIPLE/$PROFILE/mpos"
if [[ ! -f "$KERNEL" ]]; then
    echo "error: expected kernel at $KERNEL" >&2
    exit 1
fi

mkdir -p build

"$ROOT/scripts/fetch-limine.sh"

# Everything the image is assembled from lives under two directories: `pack`
# holds what is *built* - the user programs, the unpacked root filesystem and
# the seed tree - and `tree` is the ESP itself, which ends up on the disk
# verbatim.
# Where the build goes. It defaults to `build/` beside the source, and can be
# pointed elsewhere - a slower disk, a directory that is already writable, a
# tree outside the repository - with MPOS_BUILD_DIR.
BUILD="${MPOS_BUILD_DIR:-$ROOT/build}"
PACK="$BUILD/pack-$ARCH"
ESP_TREE="$BUILD/esp-tree-$ARCH"
BOOT_TREE="$BUILD/boot-tree-$ARCH"
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
# No chroot is used, so the only privilege needed is that the files are owned
# by root.
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

# The user programs the sweep runs come from the workspace: mpos-native
# no_std forms and, for the migrated tests, real musl binaries speaking std
# and rustix. They go into *both* trees - the sweep runs from the initramfs,
# and a booted machine wants its own tests on hand.
USER_TARGET="x86_64-unknown-linux-musl"
cargo build --release -p mpos-user --target "$USER_TARGET"
USER_BINS="$TARGET_DIR/$USER_TARGET/release"

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
# A rootfs image to use verbatim: same idea as MPOS_INITRAMFS_TREE, for the
# other half. The tree build and mke2fs run are skipped entirely.
ROOTFS_MIB=384
if [[ -n "${MPOS_ROOTFS_IMAGE:-}" ]]; then
    ROOTFS_IMG="${MPOS_ROOTFS_IMAGE}"
    echo "rootfs: reusing the image at $ROOTFS_IMG"
else
ROOTFS="$PACK/rootfs"
# The previous run's tree is root-owned with a 0700 /root (Alpine's own
# baselayout), which makes cleaning it up depend on who is running this. Make
# it deletable by whoever runs the build before taking it down.
chmod -R a+rX "$ROOTFS" 2>/dev/null || true
rm -rf "$ROOTFS"
sh "$MAKE_ROOTFS" \
    --branch "$ALPINE_BRANCH" \
    --mirror-uri "$ALPINE_MIRROR" \
    --packages "openrc alpine-release ssl_client ca-certificates apk-tools" \
    "$ROOTFS"

copy_programs "$ROOTFS"

# mpos: serial console and unlocked root login.
#
# The stock inittab respawns gettys on virtual consoles this machine does not
# have as separate devices - busybox init's respawn entries fail six times and
# back off for five minutes, and the serial port sits with no login prompt on
# it. Root ships password-locked (`*`), and /etc/securetty lists tty names
# this kernel's ttyname(2) cannot answer yet, so login would refuse root
# while calling it a wrong password. One line, an empty password field, and
# no securetty: root logs in on ttyS0 with nothing to type.
cat > "$ROOTFS/etc/inittab" <<'EOT'
# /etc/inittab - serial-console edition for mpos

::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

# Put a getty on the serial port
ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100

# Stuff to do for the 3-finger salute
::ctrlaltdel:/sbin/reboot

# Stuff to do before rebooting
::shutdown:/sbin/openrc shutdown
EOT
sed -i 's/^root:[^:]*:/root::/' "$ROOTFS/etc/shadow"
rm -f "$ROOTFS/etc/securetty"
# The data-partition mountpoint the initramfs's move-mount pass asks for.
mkdir -p "$ROOTFS/mnt"

# OpenRC refuses to start any runlevel service whose script is not
# executable - and answers 0, so the failure is silent: `root` (whose whole
# job is remounting / read-write) never runs and every write answers EROFS.
# The tree alpine-make-rootfs produces loses the exec bit somewhere between
# apk and the image, so restore it here.
chmod +x "$ROOTFS"/etc/init.d/* 2>/dev/null || true

# No runlevel services are linked. OpenRC 0.63's service execution deadlocks
# on this kernel after the first real service runs - the fork/waitpid chain
# between openrc-run and the service's child shell never completes - so any
# service linked here freezes the boot after sysinit's built-in mounts.
# Everything these services would do is already handled: /dev comes up via
# the initramfs's devtmpfs, and the root is mounted writable by the kernel
# command line (`rootflags=rw`), which is what the `root` service would do.
# Reviving them means finding why openrc-run never reaps its child shells.

# The resolver config musl reads. QEMU's user network serves DNS on a fixed
# address - without this file getaddrinfo has no nameserver to ask and every
# hostname resolves to `bad address`.
mkdir -p "$ROOTFS/etc"
cat > "$ROOTFS/etc/resolv.conf" <<'EOT'
# QEMU user networking's built-in forwarder.
nameserver 10.0.2.3
EOT
fi

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
#     /proc/cmdline names. This kernel has no uevent socket; that init is what
#     runs, against the devfs the kernel publishes.
#
# Without a docker daemon the tree is built by alpine-make-rootfs instead: the
# same shape, a different producer.
if [[ -n "${MPOS_INITRAMFS_DIR:-}" ]]; then
    INITRAMFS_DIR="$(mktemp -d "${MPOS_INITRAMFS_DIR%/}.XXXXXX")"
else
    INITRAMFS_DIR="$PACK/initramfs_root"
    rm -rf "$INITRAMFS_DIR"
fi
mkdir -p "$INITRAMFS_DIR"

# An initramfs tree to use verbatim: one built earlier by this same script (or
# by the distribution's own mkinitfs), reused while the kernel is what is
# actually being iterated on.
DOCKER_OUT=$ROOT/build/docker_out
if [[ -n "${MPOS_INITRAMFS_TREE:-}" ]]; then
    echo "initramfs: reusing the tree at $MPOS_INITRAMFS_TREE"
    cp -a "${MPOS_INITRAMFS_TREE%/}/." "$INITRAMFS_DIR/"
elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    echo "initramfs: mkinitfs, in alpine:$ALPINE_BRANCH"
    rm -rf "$DOCKER_OUT"
    mkdir -p "$DOCKER_OUT"
    docker run --rm -v "$DOCKER_OUT:/out" "alpine:latest" sh -euxc '
        printf "%s\n" "'"$ALPINE_MIRROR"'/'"$ALPINE_BRANCH"'/main" > /etc/apk/repositories
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

if [[ -f "$APK_BIN" ]]; then
    mkdir -p "$INITRAMFS_DIR/sbin"
    cp "$APK_BIN" "$INITRAMFS_DIR/sbin/apk"
    chmod +x "$INITRAMFS_DIR/sbin/apk"
fi

copy_programs "$INITRAMFS_DIR"

tar --format=ustar --owner=root --group=root --numeric-owner \
    -cf "$BOOT_TREE/boot/initramfs.tar" -C "$INITRAMFS_DIR" .
chmod -R a+rwX "$INITRAMFS_DIR" 2>/dev/null || true
rm -rf "$INITRAMFS_DIR" "${DOCKER_OUT:-}" 2>/dev/null || true

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
if [[ -z "${MPOS_ROOTFS_IMAGE:-}" ]]; then
    ROOTFS_IMG="build/rootfs-$ARCH.img"
    rm -f "$ROOTFS_IMG"
    mke2fs -q -F -t ext2 -b 4096 -I 256 -d "$ROOTFS" "$ROOTFS_IMG" $((ROOTFS_MIB * 1024 / 4))
    rm -rf "$ROOTFS"
fi

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
