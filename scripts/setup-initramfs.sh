#!/bin/sh
# Make the initramfs tree the shape the kernel needs to boot from.
#
# Runs with $ROOTFS pointing at the tree, which the caller (mkdisk.sh) has
# already built - with Alpine's mkinitfs when a docker daemon is about, with
# alpine-make-rootfs otherwise. What is added here is only what the kernel
# itself requires:
#
#   /proc /sys /dev   the canonical directories, as empty mountpoints. The
#                     kernel mounts none of them; the injected init does.
#   /new_root         where the real root filesystem is mounted before the
#                     switch.
#   /etc/alpine-release, /etc/motd
#                     the two files the boot's own check reads to prove the
#                     archive unpacked whole. mkinitfs's tree carries neither.
#
# The init script itself is installed by the caller from
# scripts/initramfs-init, so there is exactly one copy of it.
set -e

mkdir -p "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/new_root" \
    "$ROOTFS/etc" "$ROOTFS/sbin" "$ROOTFS/bin"

# mkinitfs ships its own /init: the standard Alpine one, which finds the root
# device through netlink uevents and mounts what /proc/cmdline names. This
# kernel has no uevent socket yet, so that script cannot find anything - but
# it is kept beside the one that runs, because the day the kernel grows
# netlink support is the day using it becomes a one-line change.
if [ -f "$ROOTFS/init" ]; then
    cp "$ROOTFS/init" "$ROOTFS/init.mkinitfs"
fi

# The release stamp, taken from the root filesystem the same boot will switch
# to: one Alpine, one version to report.
if [ ! -f "$ROOTFS/etc/alpine-release" ]; then
    echo "${ROOTFS_RELEASE:-3.24.1}" > "$ROOTFS/etc/alpine-release"
fi
if [ ! -s "$ROOTFS/etc/motd" ]; then
    printf '%s\n' "mpos initramfs" > "$ROOTFS/etc/motd"
fi
