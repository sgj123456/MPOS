#!/bin/sh
# vim: set ts=4 sw=4:
# SPDX-FileCopyrightText: © 2017 Jakub Jirutka <jakub@jirutka.cz>
# SPDX-FileCopyrightText: © 2026 the mpos authors
# SPDX-License-Identifier: MIT
#
# Usage: scripts/mkdisk.sh
#
# Build mpos and package the whole system as one bootable UEFI disk image.
#
# Derived from alpine-make-vm-image 0.13.4 (github.com/alpinelinux/
# alpine-make-vm-image), which builds a bootable Alpine disk image by mounting
# a fresh image, installing `alpine-base` into it and configuring a bootloader.
# The disk layout, the mounting, the package installation, the chroot and the
# system configuration are that script's, unchanged in substance. Two things
# are not:
#
#   * **There is no Alpine kernel.** Where the original installs `linux-virt`,
#     runs `mkinitfs` and points a bootloader at the result, this one builds
#     the kernel in this repository - `src/` for the kernel and `boot/` for the
#     EFI loader that starts it - and puts both on the EFI System Partition.
#     Nothing from Alpine's `linux-*` or `mkinitfs` packages is installed.
#
#   * **There are no options.** Every knob the original exposed is pinned here:
#     x86_64, UEFI, ext2, one image path, one size, one mirror. The machine this
#     builds for is the machine this repository is: there is no second
#     configuration to select between, so a flag for choosing one would only be
#     a way to build something nobody has tested. The environment variables the
#     original honoured are still honoured, because they are how a build in a
#     sandbox points at a mirror it can reach.
#
# Disk layout (GPT):
#   Partition 1: ESP (128 MiB, FAT32) - the loader, the kernel, the initramfs
#   Partition 2: Root (the rest, ext2) - the Alpine root filesystem
#
# UEFI mode only. A BIOS image would need a bootloader in the MBR and a kernel
# that a 16-bit loader can start, and mpos is a 64-bit EFI application.
#
# The script builds the kernel as whoever invoked it - cargo and rustup keep
# their caches in $HOME, and a root-owned target/ is a directory the user can no
# longer write to - and then re-executes itself under sudo for the parts that
# create device nodes and set file ownership.

set -eu

# Some distros (e.g. Arch Linux) does not have /bin or /sbin in PATH.
PATH="$PATH:/usr/sbin:/usr/bin:/sbin:/bin"

readonly PROGNAME='mkdisk'
readonly ARCH='x86_64'
readonly IMAGE_FILE='build/mpos-uefi-x86_64.img'
readonly IMAGE_SIZE='2G'
readonly ALPINE_BRANCH="${ALPINE_BRANCH:-latest-stable}"
# readonly ALPINE_MIRROR="${ALPINE_MIRROR:-http://dl-cdn.alpinelinux.org/alpine}"
readonly ALPINE_MIRROR="${ALPINE_MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/alpine}"
readonly ROOTFS='ext2'
readonly SERIAL_PORT='ttyS0'
# What the root filesystem is called *inside the guest*. Not the host's NBD
# device: the image is built through `/dev/nbd0p2` and booted as `/dev/vda2`,
# and the command line is read by the guest.
readonly ROOT_DEVICE='/dev/vda2'
readonly PACKAGES="${PACKAGES:-openrc alpine-release ssl_client ca-certificates apk-tools}"

# Sizes, in MiB. The ESP holds the loader, a kernel image and an initramfs, and
# the smallest FAT32 volume is about 33 MiB; 128 is what a UEFI layout is
# normally given, and a kernel with debug symbols fits twice over.
readonly ESP_MIB=128

# Where the build goes.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
readonly ROOT
PROFILE="${PROFILE:-debug}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BUILD="${MPOS_BUILD_DIR:-$ROOT/build}"

# Alpine APK keys for verification of packages for x86_64.
readonly ALPINE_KEYS='
4a6a0840:MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA1yHJxQgsHQREclQu4Ohe\nqxTxd1tHcNnvnQTu/UrTky8wWvgXT+jpveroeWWnzmsYlDI93eLI2ORakxb3gA2O\nQ0Ry4ws8vhaxLQGC74uQR5+/yYrLuTKydFzuPaS1dK19qJPXB8GMdmFOijnXX4SA\njixuHLe1WW7kZVtjL7nufvpXkWBGjsfrvskdNA/5MfxAeBbqPgaq0QMEfxMAn6/R\nL5kNepi/Vr4S39Xvf2DzWkTLEK8pcnjNkt9/aafhWqFVW7m3HCAII6h/qlQNQKSo\nGuH34Q8GsFG30izUENV9avY7hSLq7nggsvknlNBZtFUcmGoQrtx3FmyYsIC8/R+B\nywIDAQAB
5261cecb:MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAwlzMkl7b5PBdfMzGdCT0\ncGloRr5xGgVmsdq5EtJvFkFAiN8Ac9MCFy/vAFmS8/7ZaGOXoCDWbYVLTLOO2qtX\nyHRl+7fJVh2N6qrDDFPmdgCi8NaE+3rITWXGrrQ1spJ0B6HIzTDNEjRKnD4xyg4j\ng01FMcJTU6E+V2JBY45CKN9dWr1JDM/nei/Pf0byBJlMp/mSSfjodykmz4Oe13xB\nCa1WTwgFykKYthoLGYrmo+LKIGpMoeEbY1kuUe04UiDe47l6Oggwnl+8XD1MeRWY\nsWgj8sF4dTcSfCMavK4zHRFFQbGp/YFJ/Ww6U9lA3Vq0wyEI6MCMQnoSMFwrbgZw\nwwIDAQAB
6165ee59:MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAutQkua2CAig4VFSJ7v54\nALyu/J1WB3oni7qwCZD3veURw7HxpNAj9hR+S5N/pNeZgubQvJWyaPuQDm7PTs1+\ntFGiYNfAsiibX6Rv0wci3M+z2XEVAeR9Vzg6v4qoofDyoTbovn2LztaNEjTkB+oK\ntlvpNhg1zhou0jDVYFniEXvzjckxswHVb8cT0OMTKHALyLPrPOJzVtM9C1ew2Nnc\n3848xLiApMu3NBk0JqfcS3Bo5Y2b1FRVBvdt+2gFoKZix1MnZdAEZ8xQzL/a0YS5\nHd0wj5+EEKHfOd3A75uPa/WQmA+o0cBFfrzm69QDcSJSwGpzWrD1ScH3AK8nWvoj\nv7e9gukK/9yl1b4fQQ00vttwJPSgm9EnfPHLAtgXkRloI27H6/PuLoNvSAMQwuCD\nhQRlyGLPBETKkHeodfLoULjhDi1K2gKJTMhtbnUcAA7nEphkMhPWkBpgFdrH+5z4\nLxy+3ek0cqcI7K68EtrffU8jtUj9LFTUC8dERaIBs7NgQ/LfDbDfGh9g6qVj1hZl\nk9aaIPTm/xsi8v3u+0qaq7KzIBc9s59JOoA8TlpOaYdVgSQhHHLBaahOuAigH+VI\nisbC9vmqsThF2QdDtQt37keuqoda2E6sL7PUvIyVXDRfwX7uMDjlzTxHTymvq2Ck\nhtBqojBnThmjJQFgZXocHG8CAwEAAQ==
58199dcc:MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA3v8/ye/V/t5xf4JiXLXa\nhWFRozsnmn3hobON20GdmkrzKzO/eUqPOKTpg2GtvBhK30fu5oY5uN2ORiv2Y2ht\neLiZ9HVz3XP8Fm9frha60B7KNu66FO5P2o3i+E+DWTPqqPcCG6t4Znk2BypILcit\nwiPKTsgbBQR2qo/cO01eLLdt6oOzAaF94NH0656kvRewdo6HG4urbO46tCAizvCR\nCA7KGFMyad8WdKkTjxh8YLDLoOCtoZmXmQAiwfRe9pKXRH/XXGop8SYptLqyVVQ+\ntegOD9wRs2tOlgcLx4F/uMzHN7uoho6okBPiifRX+Pf38Vx+ozXh056tjmdZkCaV\naQIDAQAB
616ae350:MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAyduVzi1mWm+lYo2Tqt/0\nXkCIWrDNP1QBMVPrE0/ZlU2bCGSoo2Z9FHQKz/mTyMRlhNqTfhJ5qU3U9XlyGOPJ\npiM+b91g26pnpXJ2Q2kOypSgOMOPA4cQ42PkHBEqhuzssfj9t7x47ppS94bboh46\nxLSDRff/NAbtwTpvhStV3URYkxFG++cKGGa5MPXBrxIp+iZf9GnuxVdST5PGiVGP\nODL/b69sPJQNbJHVquqUTOh5Ry8uuD2WZuXfKf7/C0jC/ie9m2+0CttNu9tMciGM\nEyKG1/Xhk5iIWO43m4SrrT2WkFlcZ1z2JSf9Pjm4C2+HovYpihwwdM/OdP8Xmsnr\nDzVB4YvQiW+IHBjStHVuyiZWc+JsgEPJzisNY0Wyc/kNyNtqVKpX6dRhMLanLmy+\nf53cCSI05KPQAcGj6tdL+D60uKDkt+FsDa0BTAobZ31OsFVid0vCXtsbplNhW1IF\nHwsGXBTVcfXg44RLyL8Lk/2dQxDHNHzAUslJXzPxaHBLmt++2COa2EI1iWlvtznk\nOk9WP8SOAIj+xdqoiHcC4j72BOVVgiITIJNHrbppZCq6qPR+fgXmXa+sDcGh30m6\n9Wpbr28kLMSHiENCWTdsFij+NQTd5S47H7XTROHnalYDuF1RpS+DpQidT5tUimaT\nJZDr++FjKrnnijbyNF8b98UCAwEAAQ==
'

# Static apk-tools, used when the host has no apk of its own. Pinned, because
# the image has to be buildable from a machine with nothing Alpine on it and a
# build input that changes under it is not an input. The pin is only a
# preference: the branch moves, `apk-tools` is rebuilt with it, and the version
# that was current a year ago is not on the mirror any more - so a fetch that
# 404s falls back to whichever version the mirror is actually serving.
readonly APK_TOOLS_VERSION='3.0.8'

KERNEL=''
LOADER=''
INITRAMFS=''
temp_dir=''
mount_dir=''
disk_dev=''

die() {
	printf '\033[1;31mERROR:\033[0m %s\n' "$@" >&2  # bold red
	exit 1
}

einfo() {
	printf '\n\033[1;36m> %s\033[0m\n' "$@" >&2  # bold cyan
}

# Cleans the build machine. This function is executed before exiting the script.
cleanup() {
	set +eu
	trap '' EXIT HUP INT TERM  # unset trap to avoid loop

	cd /
	if [ -d "$temp_dir" ]; then
		rm -Rf "$temp_dir"
	fi
	if [ "$mount_dir" ]; then
		umount_recursively "$mount_dir" \
			|| die "Failed to unmount $mount_dir; unmount it and disconnect $disk_dev manually"
		rm -Rf "$mount_dir"
	fi
	if [ "$disk_dev" ] && ! [ -b "$IMAGE_FILE" ]; then
		# The partition nodes first: `qemu-nbd --disconnect` removes the whole
		# device, and a partition node left pointing at a deleted parent is a
		# stale entry the next run would count as a device in use.
		partx -d "$disk_dev" >/dev/null 2>&1 ||:
		qemu-nbd --disconnect "$disk_dev" \
			|| die "Failed to disconnect $disk_dev; disconnect it manually"
	fi
}

_apk() {
	"$APK" $APK_OPTS "$@"
}

# Attaches the specified image as a NBD block device and prints its path.
#
# `--format=raw` is passed rather than left to probing, and not only to quiet
# the warning: without it qemu-nbd refuses to write the first 512 bytes of a
# raw image, and the first 512 bytes are where sfdisk puts the GPT header. The
# build would then be told the disk could not be partitioned, with the reason
# left behind in a warning it had already scrolled past.
attach_image() {
	local image="$1"
	local format="${2:-raw}"
	local disk_dev

	disk_dev=$(get_available_nbd) || {
		modprobe nbd max_part=16 2>/dev/null ||:
		sleep 1
		disk_dev=$(get_available_nbd)
	} || die 'No available nbd device found!'

	# qemu-nbd's own message is the useful one - "Failed to disconnect",
	# "Could not open", "Permission denied" - so it is left going to stderr and
	# only the failure is reported here.
	qemu-nbd --connect="$disk_dev" --cache=writeback \
		--format="$format" "$image" || return 1

	sleep 1
	echo "$disk_dev"
}

# Prints UUID of filesystem on the specified block device.
blk_uuid() {
	local dev="$1"
	blkid "$dev" | sed -En 's/.*\bUUID="([^"]+)".*/\1/p'
}

# Creates the GPT partition table: an EFI System Partition and a system one.
#
# This is the UEFI half of the original's `create_gpt`, and the only one left:
# the BIOS layout it also carried named the bootloader's partition "LegacyBIOS
# Bootable", which is a flag nothing on this image would look at.
create_gpt() {
	local dev="$1"

	printf '%s\n' \
		'label: gpt' \
		'name=efi,type=U,size=128M,bootable' \
		'name=system,type=L' \
		| sfdisk "$dev"
}

# Writes Alpine APK keys embedded in this script into directory $1.
#
# The file *name* is load-bearing, not decoration: apk decides whether a
# repository index is trusted by looking for a key whose filename matches the
# name in the index's signature, and every Alpine index is signed as
# `alpine-devel@lists.alpinelinux.org-<key id>.rsa.pub`. A key written under any
# other name is present but never consulted - apk reports the index as
# `UNTRUSTED signature` and then, because it cannot enumerate a repository it
# does not trust, answers `no such package` for every name in the world file.
# That is the whole failure this naming avoids.
dump_alpine_keys() {
	local dest_dir="$1"
	local content id line

	mkdir -p "$dest_dir"
	for line in $ALPINE_KEYS; do
		id=${line%%:*}
		content=${line#*:}

		printf -- "-----BEGIN PUBLIC KEY-----\n$content\n-----END PUBLIC KEY-----\n" \
			> "$dest_dir/alpine-devel@lists.alpinelinux.org-$id.rsa.pub"
	done
}

# Prints path of available nbdX device, or returns 1 if not any.
#
# The nodes are created if the module did not create them. `modprobe nbd` only
# registers the block driver: it does not populate /dev, and on a machine
# without nbd-client - which is the package that ships the udev rule doing it -
# /dev/nbd0 onwards never appears at all. The original script assumed they were
# there, so on such a machine it looked for a device that could not exist.
#
# Device numbers are from the kernel's own registration rather than guessed:
# nbd is a dynamic major whose number is in /proc/devices, and the minors run
# 0..nbds_max-1. Creating a node that already exists is left to fail silently -
# whoever made it owns it, and it is the same device either way.
NBD_MAJOR=0
get_available_nbd() {
	local dev minor

	if [ "$NBD_MAJOR" -eq 0 ]; then
		NBD_MAJOR=$(sed -n 's/^ *\([0-9]*\) nbd$/\1/p' /proc/devices)
	fi
	[ -n "$NBD_MAJOR" ] || return 1

	for minor in $(seq 0 15); do
		dev="/dev/nbd$minor"

		if [ ! -e "$dev" ]; then
			mknod "$dev" b "$NBD_MAJOR" "$minor" 2>/dev/null || continue
		fi

		# An unconnected device reports size 0; a connected one does not.
		if [ "$(blockdev --getsize64 "$dev" 2>/dev/null)" -eq 0 ]; then
			echo "$dev"; return 0
		fi
	done
	return 1
}

# Attaches the specified image as a NBD block device and prints its path.
#
# `--format=raw` is passed rather than left to probing, and not only to quiet
# the warning: without it qemu-nbd refuses to write the first 512 bytes of a
# raw image, and the first 512 bytes are where sfdisk puts the GPT header. The
# build would be told the disk could not be partitioned, with the reason in a
# warning it had already scrolled past.
# Binds the directory $1 at the mountpoint $2 and sets propagation to private.
mount_bind() {
	mkdir -p "$2"
	mount --bind "$1" "$2"
	mount --make-private "$2"
}

# Prepares chroot at the specified path.
prepare_chroot() {
	local dest="$1"

	mkdir -p "$dest"/proc
	mount -t proc none "$dest"/proc
	mount_bind /dev "$dest"/dev
	mount_bind /sys "$dest"/sys

	install -D -m 644 /etc/resolv.conf "$dest"/etc/resolv.conf
}

# Unmounts all filesystem under the specified directory tree.
umount_recursively() {
	local mount_point="$1"
	test -n "$mount_point" || return 1

	cat /proc/mounts \
		| cut -d ' ' -f 2 \
		| grep "^$mount_point" \
		| sort -r \
		| xargs -r -n 1 umount
}

# ---------------------------------------------------------------------------
# The kernel: build it, and package the early environment it boots into.
# ---------------------------------------------------------------------------

# Builds the kernel and its EFI loader, and pins where they landed.
#
# `boot/` is built for x86_64-unknown-uefi and the kernel for
# x86_64-unknown-none, so this is two cargo invocations and not one: they are
# different machines to rustc, with different object formats and different
# entry points, and only the loader knows about the firmware.
build_kernel() {
	einfo "Building the kernel ($PROFILE)"

	if [ "$PROFILE" = 'release' ]; then
		cargo build --release -p mpos || die 'The kernel did not build'
		cargo build --release -p mpos-boot --target x86_64-unknown-uefi \
			|| die 'The EFI loader did not build'
	else
		cargo build -p mpos || die 'The kernel did not build'
		cargo build -p mpos-boot --target x86_64-unknown-uefi \
			|| die 'The EFI loader did not build'
	fi

	kernel_paths

	[ -f "$KERNEL" ] || die "Expected the kernel at $KERNEL"
	[ -f "$LOADER" ] || die "Expected the EFI loader at $LOADER"

	echo "kernel: $KERNEL ($(du -h "$KERNEL" | cut -f1))" >&2
	echo "loader: $LOADER ($(du -h "$LOADER" | cut -f1))" >&2
}

# Where the two built artefacts land. Deterministic from $PROFILE and
# $TARGET_DIR, so the second pass over this script - the one running as root -
# can name them without having run cargo itself.
kernel_paths() {
	KERNEL="$TARGET_DIR/x86_64-unknown-none/$PROFILE/mpos"
	LOADER="$TARGET_DIR/x86_64-unknown-uefi/$PROFILE/mpos-boot.efi"
}

# Builds the early userspace the kernel unpacks as its first root filesystem.
#
# The kernel does not mount the disk itself. It unpacks this archive over a
# ramfs, runs `/init` out of it, and that script mounts the real root filesystem
# and pivots into it. So this tree needs exactly three things: a shell, the
# tools to mount with, and the script.
#
# The tree is built with the same static `apk` the rest of this script uses, and
# not with Alpine's `mkinitfs`: mkinitfs exists to package kernel modules and a
# uevent-driven root-finding init, and mpos has neither modules nor a uevent
# socket. What it would produce is an init that waits for a device to appear that
# this kernel has no way to announce, wrapped around an archive format the
# kernel's unpacker does not read.
make_initramfs() {
	local dir="$1"

	einfo 'Building the initramfs'

	rm -rf "$dir"
	mkdir -p "$dir/etc/apk/keys" "$dir/proc" "$dir/sys" "$dir/dev" "$dir/sysroot"

	cat > "$dir/etc/apk/repositories" <<-EOF
		$ALPINE_MIRROR/$ALPINE_BRANCH/main
		$ALPINE_MIRROR/$ALPINE_BRANCH/community
	EOF
	if [ -d "$KEYS_DIR" ]; then
		cp "$KEYS_DIR"/* "$dir/etc/apk/keys/"
	else
		dump_alpine_keys "$dir/etc/apk/keys"
	fi

	# --no-scripts: the package scripts are written for a chroot and this tree
	# is not one yet, so their device nodes and service registrations would
	# fail. What they also do is not optional, though: busybox's post-install
	# is what creates the applet links - /bin/mount, /bin/cat and the rest of
	# the several hundred names that are symlinks back to /bin/busybox. With
	# the script skipped the tree has busybox and no way to call it, and an
	# /init that runs `mount` and `cat` dies on its first line.
	#
	# So the install is repeated below by hand, running busybox against the
	# tree it was just unpacked into.
	_apk add --root "$dir" --arch "$ARCH" --initdb --no-scripts \
		--update-cache busybox alpine-baselayout apk-tools \
		|| die 'The initramfs packages did not install'

	# busybox --install lays down every applet as a symlink in one pass,
	# resolving the target list from its own compiled-in defaults plus
	# /etc/busybox-paths.d. It has to run as the binary in the tree, with the
	# tree as its root path, so that the links point where this initramfs will
	# look for them rather than at the host's busybox.
	chroot "$dir" /bin/busybox --install -s \
		|| die 'Could not install the busybox applet links'

	# The one mount the initramfs cannot do without: /dev/pts and /proc are
	# mounted onto the *real* root by this script, but /proc is on the command
	# line's critical path, so it comes up first.
	cat > "$dir/init" <<-'EOF'
		#!/bin/sh
		# Mount the pseudo-filesystems on the temporary root, mount the root
		# filesystem the kernel command line names, and hand the machine over.
		#
		# This runs as pid 1 on a ramfs, with the archive the kernel unpacked as
		# its whole world. Everything it does before `switch_root` is gone with
		# that ramfs, and everything it mounts *onto the new root* is not.

		export PATH=/sbin:/usr/sbin:/bin:/usr/bin

		# Before the command line is read, because /proc/cmdline is where it
		# is read from, and before anything else because every tool below
		# wants one of these three.
		mount -t proc     proc     /proc
		mount -t sysfs    sysfs    /sys
		mount -t devtmpfs devtmpfs /dev

		root=''
		rootflags='rw'
		for arg in $(cat /proc/cmdline); do
			case "$arg" in
				root=*)      root="${arg#root=}" ;;
				rootflags=*) rootflags="${arg#rootflags=}" ;;
			esac
		done

		if [ -z "$root" ]; then
			echo "initramfs: no root= on the command line; staying here"
			exec /bin/sh
		fi

		mkdir -p /sysroot
		if ! mount -t ext2 -o "$rootflags" "$root" /sysroot; then
			echo "initramfs: cannot mount $root; staying here"
			exec /bin/sh
		fi

		# The directories the real init expects to find already mounted. What
		# is mounted here survives the pivot; what is created here does not,
		# so each one is made on the new root, not on the ramfs.
		for dir in proc sys dev tmp run; do
			mkdir -p "/sysroot/$dir"
		done

		mount -t proc     proc     /sysroot/proc
		mount -t sysfs    sysfs    /sysroot/sys
		mount -t tmpfs    tmpfs    /sysroot/tmp
		mount -t tmpfs    tmpfs    /sysroot/run
		mount --move /dev /sysroot/dev

		exec switch_root /sysroot /sbin/init
	EOF
	chmod 755 "$dir/init"

	# A getty that survives the pivot needs /dev/console, which devtmpfs gives
	# it, and nothing in this tree needs a password file: the archive is not
	# where anyone logs in.
	ln -sf /bin/busybox "$dir/bin/sh" 2>/dev/null ||:

	tar --format=ustar --owner=0 --group=0 --numeric-owner \
		-cf "$INITRAMFS" -C "$dir" . || die 'The initramfs did not pack'

	echo "initramfs: $INITRAMFS ($(du -h "$INITRAMFS" | cut -f1))" >&2
	rm -rf "$dir"
}

# Writes the kernel command line the loader hands the kernel.
#
# `root=` and `rootflags=` are the whole of it. The kernel itself mounts
# neither: it passes this string to `/proc/cmdline` and the initramfs's init
# reads it, which is why the root filesystem's name appears here and nowhere in
# the kernel. `rootflags=rw` is not a default that could be left out - the
# standard boot mounts the root read-only and remounts it writable from a
# service, and OpenRC does not get far enough to run that service on this
# kernel, so the root is mounted writable once and stays that way.
write_cmdline() {
	local path="$1"

	printf 'root=%s rootflags=rw console=%s\n' "$ROOT_DEVICE" "$SERIAL_PORT" > "$path"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if [ "$(id -u)" != 0 ]; then
	einfo 'Building the kernel before taking root'

	# The kernel is built here, as the user, rather than after the re-exec
	# below. Cargo writes into $CARGO_HOME and the rustup toolchains in $HOME,
	# and running either as root leaves a tree the user cannot write to again -
	# which is the state a build that re-execs first ends in.
	build_kernel

	einfo 'Re-executing under sudo to build the image'
	exec sudo -E env "HOME=$HOME" "PATH=$PATH" MPOS_ARTEFACTS_READY=yes "$0"
fi

kernel_paths
if [ "${MPOS_ARTEFACTS_READY:-}" != 'yes' ]; then
	# Root is the wrong user to build as - cargo's caches live in $HOME - but
	# running the script straight as root is a reasonable thing to do, so it
	# builds rather than refusing.
	build_kernel
fi

command -v cargo >/dev/null || die 'cargo is not on PATH'

trap 'cleanup' EXIT
trap 'cleanup; exit 130' HUP INT TERM

START_TIME=$(date +%s)

#-----------------------------------------------------------------------
einfo 'Installing needed packages on this machine'

mkdir -p "$BUILD"
# Scratch space, and why it is not simply /tmp: the initramfs tree is built
# here, and it reaches a few tens of megabytes once apk has unpacked musl,
# busybox and apk-tools into it. A /tmp that is a small tmpfs - which is what
# several distributions, and this build sandbox, give you - runs out in the
# middle of the extraction, and apk reports it as three unrelated packages
# failing rather than as a full disk. `$TMPDIR` is honoured so a caller can
# point it somewhere roomy; the build directory is the fallback, because it is
# already the one place this script is known to have write access to.
temp_root="${TMPDIR:-$BUILD}"
temp_dir="$(mktemp -d "$temp_root/$PROGNAME.XXXXXX")"
DEPS_DIR="$temp_dir/deps"

[ -d /etc/apk/keys ] && KEYS_DIR='/etc/apk/keys' || KEYS_DIR=''
APK_BIN="$(command -v apk 2>/dev/null || true)"

if [ -n "$APK_BIN" ]; then
	APK="$APK_BIN"
	APK_OPTS='--no-progress'
	_apk add --no-progress qemu-img e2fsprogs dosfstools sfdisk
else
	APK_OPTS='--no-progress'

	# No apk on the host: fetch the static build, which is the same tool the
	# image's own packages are installed with below. It comes off the package
	# mirror rather than the gitlab release the original script points at,
	# because the mirror is the host this build already has to reach for every
	# other package.
	einfo "No apk on this machine; fetching apk.static"
	pkg_dir="$ALPINE_MIRROR/$ALPINE_BRANCH/main/x86_64"
	apk_pkg="$temp_dir/apk-tools-static.apk"
	uri="$pkg_dir/apk-tools-static-$APK_TOOLS_VERSION-r0.apk"
	if ! wget -T 60 --no-verbose -O "$apk_pkg" "$uri"; then
		# The pin is stale. Ask the mirror what it has and take the newest.
		uri=$(wget -T 60 --no-verbose -O - "$pkg_dir/" 2>/dev/null \
			| sed -n 's/.*\(apk-tools-static-[0-9][^-]*-r[0-9]*\.apk\).*/\1/p' \
			| sort -V | tail -1)
		[ -n "$uri" ] \
			|| die "No apk-tools-static at $pkg_dir, and no apk on PATH"
		wget -T 60 --no-verbose -O "$apk_pkg" "$pkg_dir/$uri" \
			|| die 'Could not fetch apk-tools-static; put apk on PATH and try again'
	fi
	mkdir -p "$temp_dir/apktools"
	tar -xzf "$apk_pkg" -C "$temp_dir/apktools" \
		--warning=no-unknown-keyword sbin/apk.static \
		|| die 'The apk-tools-static package did not extract'
	APK="$temp_dir/apktools/sbin/apk.static"
	chmod +x "$APK"
fi

# Everything the image is assembled with. `qemu-nbd` is here rather than assumed
# because the failure it causes without it - no nbd device to attach - reads as
# a kernel or permissions problem, and it is neither. `partx` and `blkid` come
# from util-linux and are what the partition table and the fstab are read back
# with.
for tool in qemu-img qemu-nbd sfdisk partx blkid mkfs.fat mkfs.ext2; do
	command -v "$tool" >/dev/null \
		|| die "$tool is not installed and there is no apk to install it with"
done

#-----------------------------------------------------------------------
einfo "Building the initramfs"
INITRAMFS="$BUILD/initramfs.tar"
make_initramfs "$temp_dir/initramfs"
[ -f "$KERNEL" ] || die "No kernel at $KERNEL; run this script as the user who owns the tree"

#-----------------------------------------------------------------------
if [ ! -f "$IMAGE_FILE" ] && [ ! -b "$IMAGE_FILE" ]; then
	einfo "Creating a disk image of size $IMAGE_SIZE"
	qemu-img create -f raw "$IMAGE_FILE" "$IMAGE_SIZE"
fi

#-----------------------------------------------------------------------
if [ -b "$IMAGE_FILE" ]; then
	echo "Provided target $IMAGE_FILE is a block device" >&2
	disk_dev="$IMAGE_FILE"
else
	einfo "Attaching image $IMAGE_FILE as a NBD device"
	# `raw` rather than letting qemu-nbd probe: the image was made by
	# `qemu-img create -f raw` a few lines up, and a format qemu-nbd has to
	# guess at is one it will refuse to write block 0 of.
	disk_dev=$(attach_image "$IMAGE_FILE" raw) \
		|| die 'Could not attach the image'
fi

#-----------------------------------------------------------------------
einfo 'Creating GPT partition table'

create_gpt "$disk_dev"

esp_dev="${disk_dev}p1"
root_dev="${disk_dev}p2"

# The partition nodes are asked for rather than waited for. `sfdisk` writes the
# table and tells the kernel, and on a machine running udev the nodes for
# nbdXp1/nbdXp2 appear on their own - but this disk's nbd node may have been
# created by hand a moment ago (see get_available_nbd), and a node udev never
# saw appear is one it has no event to make partitions for. `partx` asks the
# kernel directly, so it works either way; the retry is for the kernel having
# accepted the table but not yet published it.
partx -a "$disk_dev" 2>/dev/null ||:
for _ in 1 2 3 4 5 6 7 8 9 10; do
	[ -b "$esp_dev" ] && [ -b "$root_dev" ] && break
	sleep 1
	partx -a "$disk_dev" 2>/dev/null ||:
done
[ -b "$esp_dev" ] && [ -b "$root_dev" ] \
	|| die "The kernel did not publish $esp_dev and $root_dev"

#-----------------------------------------------------------------------
einfo 'Creating filesystem(s)'

# The ESP gets FAT32 with the smallest legal clusters: the loader, the kernel
# and the initramfs are three files, and 128 MiB of 512-byte clusters is room
# for them many times over.
mkfs.fat -F32 -n EFI "$esp_dev" >/dev/null
# syslinux 6.0.3 cannot boot from ext4 w/ 64bit feature enabled, and this
# kernel's ext2 driver reads the classic layout; -E nodiscard because these
# blocks are a sparse file and discarding them is wasted work.
mkfs.ext2 -L root -O ^64bit -E nodiscard "$root_dev" >/dev/null

root_uuid=$(blk_uuid "$root_dev")
# Mount points go under the same scratch root as the initramfs tree: this is
# where the root filesystem is staged, so it needs the room the same way, and
# the two lifetimes overlap.
mount_dir="$(mktemp -d "$temp_root/$PROGNAME.mnt.XXXXXX")"

#-----------------------------------------------------------------------
einfo "Mounting the image at $mount_dir"

mount "$root_dev" "$mount_dir"
install -d -m 000 "$mount_dir"/boot
mount -t vfat "$esp_dev" "$mount_dir"/boot

#-----------------------------------------------------------------------
einfo 'Installing the base system'

cd "$mount_dir"

mkdir -p etc/apk/keys
cat > etc/apk/repositories <<-EOF
	$ALPINE_MIRROR/$ALPINE_BRANCH/main
	$ALPINE_MIRROR/$ALPINE_BRANCH/community
EOF
if [ -d "$KEYS_DIR" ]; then
	cp "$KEYS_DIR"/* etc/apk/keys/
else
	dump_alpine_keys etc/apk/keys/
fi

_apk add --root . --arch "$ARCH" --update-cache --initdb alpine-base

_apk add --root . e2fsprogs dosfstools

prepare_chroot .

#-----------------------------------------------------------------------
einfo 'Installing the kernel'

# Where Alpine's `linux-virt` and `mkinitfs` would go. Three files, all of them
# built out of this repository a moment ago, and the ESP mounted at /boot is
# where the firmware looks for every one of them.
#
# `BOOTX64.EFI` under EFI/BOOT is the path a UEFI firmware falls back to when
# nothing has written a boot entry into its non-volatile memory - which is
# exactly the state of a machine that has never booted this disk. The firmware
# reads it, it reads the two files beside it, and the disk boots. There is no
# boot entry to install and no configuration file to get out of step with the
# files themselves.
mkdir -p boot/EFI/BOOT
install -m 644 "$LOADER" boot/EFI/BOOT/BOOTX64.EFI
install -m 644 "$KERNEL" boot/mpos.elf
install -m 644 "$INITRAMFS" boot/initramfs.tar
write_cmdline boot/mpos.cmdline

#-----------------------------------------------------------------------
einfo 'Configuring the system'

cat > etc/fstab <<-EOF
	# <fs>		<mountpoint>	<type>	<opts>		<dump/pass>
	UUID=$root_uuid	/		$ROOTFS	relatime	0 1
EOF

# The ESP is where the firmware reads from and nothing else mounts it: the
# loader is copied in at build time and never changes while the machine runs.
# An fstab entry for it would only be a mount that can fail.

cat > etc/network/interfaces <<-EOF
	auto eth0
	iface eth0 inet dhcp

	post-up /etc/network/if-post-up.d/*
	post-down /etc/network/if-post-down.d/*
EOF
mkdir -p etc/network/if-post-up.d
mkdir -p etc/network/if-post-down.d

# Serial console and an unlocked root.
#
# The stock inittab respawns gettys on virtual consoles this machine does not
# have as separate devices - busybox init's respawn entries fail six times and
# back off for five minutes, and the serial port sits with no login prompt on
# it. Root ships password-locked (`*`), and /etc/securetty lists tty names this
# kernel's ttyname(2) cannot answer yet, so login would refuse root while
# calling it a wrong password. One line, an empty password field, and no
# securetty: root logs in on ttyS0 with nothing to type.
cat > etc/inittab <<'EOF'
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
EOF
sed -i 's/^root:[^:]*:/root::/' etc/shadow
rm -f etc/securetty

# OpenRC refuses to start any runlevel service whose script is not
# executable - and answers 0, so the failure is silent: `root` (whose whole
# job is remounting / read-write) never runs and every write answers EROFS.
chmod +x etc/init.d/* 2>/dev/null || true

# No runlevel services are linked. OpenRC 0.63's service execution deadlocks
# on this kernel after the first real service runs - the fork/waitpid chain
# between openrc-run and the service's child shell never completes - so any
# service linked here freezes the boot after sysinit's built-in mounts.
# Everything these services would do is already handled: /proc, /sys and /dev
# are mounted by the initramfs's init onto this root before it pivots into it,
# /tmp and /run by the same script, and the root is mounted writable by
# `rootflags=rw` on the command line, which is what the `root` service would
# do. Reviving them means finding why openrc-run never reaps its child shells.

# The resolver config musl reads. QEMU's user network serves DNS on a fixed
# address - without this file getaddrinfo has no nameserver to ask and every
# hostname resolves to `bad address`.
cat > etc/resolv.conf <<-'EOF'
	# QEMU user networking's built-in forwarder.
	nameserver 10.0.2.3
EOF

mkdir -p mnt

#-----------------------------------------------------------------------
if [ "$PACKAGES" ]; then
	einfo 'Installing additional packages'
	_apk add --root . $PACKAGES
fi

#-----------------------------------------------------------------------
einfo 'Completed'
cd - >/dev/null
ls -lh "$IMAGE_FILE"

END_TIME=$(date +%s)
printf '\nBuilt in %s seconds. Boot it with:\n\n' "$((END_TIME - START_TIME))"
printf '  scripts/run.sh\n'
printf '\n'
