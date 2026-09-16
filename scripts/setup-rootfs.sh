#!/bin/sh
# Configure the root filesystem alpine-make-rootfs just built.
#
# Runs with $ROOTFS pointing at the tree (no chroot): alpine-make-rootfs
# executes it after the base system is installed and before cleanup. What it
# says about the machine is the whole of the machine's self-image - who runs
# init, what the console is, where the other partitions go.
set -e

# The serial port is the only console. init runs OpenRC's sysinit and boot
# runlevels - which mount /dev, /proc and /sys, and bring up /run - and then
# keeps a login prompt alive on ttyS0. No tty1..tty6: there is no video
# console this kernel drives.
cat > "$ROOTFS/etc/inittab" <<'INITTAB'
# mpos: one serial console, one OpenRC.
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100
::shutdown:/sbin/openrc shutdown
::ctrlaltdel:/sbin/reboot
INITTAB

# Root logs in without a password: this is a development board, not a
# multi-user machine, and the serial port is behind the machine's own case.
grep -q '^ttyS0$' "$ROOTFS/etc/securetty" 2>/dev/null || echo ttyS0 >> "$ROOTFS/etc/securetty"

echo mpos > "$ROOTFS/etc/hostname"

# What OpenRC's `localmount` brings up. /boot is deliberately absent: it is
# FAT32 and this kernel has no FAT driver, and its contents - the bootloader,
# the kernel, the initramfs - are only read at boot, by the bootloader.
# /dev, /proc, /sys and /run are deliberately absent too: they are not in
# fstab because they are not filesystems with a home, they are the canonical
# directories OpenRC's own services mount.
cat > "$ROOTFS/etc/fstab" <<'FSTAB'
# <fs>      <mountpoint> <type> <opts>          <dump> <pass>
/dev/vda4   /mnt         ext2   defaults        0      0
FSTAB

# The network address is fixed: QEMU's user-mode stack answers as 10.0.2.2
# and forwards DNS at 10.0.2.3.
cat > "$ROOTFS/etc/resolv.conf" <<'EOF'
nameserver 10.0.2.3
EOF

# The repositories the machine installs from, once it is running.
cat > "$ROOTFS/etc/apk/repositories" <<'EOF'
http://dl-cdn.alpinelinux.org/alpine/v3.24/main
http://dl-cdn.alpinelinux.org/alpine/v3.24/community
EOF

# apk needs /var/cache/apk for its lock file and package cache.
mkdir -p "$ROOTFS/var/cache/apk" "$ROOTFS/var/lib/apk"

# The boot runlevel: the services that mount the canonical directories and
# bring the machine's self-image up. devfs mounts /dev, procfs /proc, sysfs
# /sys; OpenRC itself mounts /run before any of them; localmount works
# through /etc/fstab above.
mkdir -p "$ROOTFS/etc/runlevels/boot" "$ROOTFS/etc/runlevels/sysinit"
for svc in devfs procfs sysfs hostname bootmisc localmount; do
    ln -sf "/etc/init.d/$svc" "$ROOTFS/etc/runlevels/boot/$svc"
done

# Build-sandbox wiring. When the build host reaches the network only through
# a loopback proxy, QEMU's slirp answers the guest's 10.0.2.2 with that
# proxy, and apk learns it from /etc/profile. Outside such a sandbox nothing
# is written and the guest talks to the network directly.
if [ -n "${GUEST_HTTP_PROXY_PORT:-}" ]; then
    cat >> "$ROOTFS/etc/profile" <<PROFILE

# mpos build-time wiring: this host reaches the network only through a
# loopback proxy, and slirp answers 10.0.2.2 with that loopback.
export http_proxy=http://10.0.2.2:${GUEST_HTTP_PROXY_PORT}
export https_proxy=http://10.0.2.2:${GUEST_HTTP_PROXY_PORT}
PROFILE
fi
