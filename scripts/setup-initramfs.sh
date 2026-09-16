#!/bin/sh
# Write the init script into the initramfs tree alpine-make-rootfs built.
#
# Runs with $ROOTFS pointing at the tree. The kernel unpacks this tree as its
# first root and runs /init as pid 1 - so this file is the machine's first
# userland, and everything the kernel no longer does itself happens here:
# mounting the canonical directories, mounting the real root, and handing the
# machine over.
set -e

mkdir -p "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/new_root"

cat > "$ROOTFS/init" <<'INIT'
#!/bin/sh
# The early environment. The kernel mounted nothing: these four are here
# because this script put them there.
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devfs devfs /dev

# The real root, by the device name the kernel's devfs published.
mount -t ext2 /dev/vda3 /new_root

# Move the root, park the initramfs at /old_root inside it, and let go of
# the parking place: after the umount the initramfs's memory goes back.
pivot_root /new_root /new_root/old_root
umount -l /old_root
rmdir /old_root

# The machine's own init. Everything from here on - OpenRC, the services,
# the login prompt - belongs to the root filesystem.
exec /sbin/init
INIT
chmod +x "$ROOTFS/init"
