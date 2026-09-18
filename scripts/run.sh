#!/bin/bash
# vim: set ts=4 sw=4:
# SPDX-License-Identifier: MIT
#
# Usage: scripts/run.sh
#
# Boot the disk image `mkdisk.sh` builds, under QEMU with the serial port
# attached to this terminal and nothing else.
#
# There is one machine here - x86_64, UEFI, the image at one path - so there is
# nothing to pass and nothing to choose. `mkdisk.sh` pins the same things, and a
# launcher that offered a way to vary them could only produce a combination
# nothing has ever booted.
#
# There is no kernel to hand the firmware either. The image carries its own boot
# chain in its EFI System Partition - BOOTX64.EFI, the kernel beside it, the
# initramfs beside that - so the disk is the only thing QEMU is given. What has
# to be supplied from outside is the *firmware*: QEMU has no flash of its own,
# and a machine without UEFI never reaches the first instruction of the loader.
#
# If the image is missing it is built and this script stops there, leaving the
# boot to a second run: `mkdisk.sh` re-execs itself under sudo, and holding that
# through a QEMU that then runs the guest as root is a worse trade than one more
# command.

set -euo pipefail

# The tree this script belongs to, from the script's own path rather than the
# working directory - so it boots the same image wherever it is invoked from.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Where the image is, in the same terms mkdisk.sh uses: `build/` beside the
# source, movable with MPOS_BUILD_DIR so a build on a slow or read-only tree can
# be pointed somewhere else. The two scripts have to agree on it, or this one
# looks for an image the other did not write.
BUILD_DIR="${MPOS_BUILD_DIR:-$ROOT/build}"
readonly BUILD_DIR
readonly IMAGE="$BUILD_DIR/mpos-uefi-x86_64.img"
readonly MEM="${MEM:-1024}"

# OVMF, the UEFI firmware QEMU runs instead of SeaBIOS. The distributions put it
# in one of a handful of places and none of them is standard; the 4 MiB build is
# the one that matters here, because the smaller 2 MiB one cannot map the kernel
# and the initramfs at the addresses the loader asks for.
#
# Its two halves are separate files: a code half that is read-only and a
# variables half that is written to. The variables half is copied into the build
# directory once per run rather than used in place - firmware that cannot write
# a variable refuses to start at all, and writing to /usr is not this script's
# business.
find_ovmf() {
	local dir
	for dir in /usr/share/OVMF/x64 /usr/share/edk2/x64 /usr/share/OVMF \
				/usr/share/edk2-ovmf/x64; do
		if [[ -f "$dir/OVMF_CODE.4m.fd" && -f "$dir/OVMF_VARS.4m.fd" ]]; then
			echo "$dir"
			return 0
		fi
	done
	echo 'error: no OVMF firmware found; looked in /usr/share/OVMF/x64, /usr/share/edk2/x64,' >&2
	echo '       /usr/share/OVMF and /usr/share/edk2-ovmf/x64 for OVMF_CODE.4m.fd' >&2
	return 1
}

command -v qemu-system-x86_64 >/dev/null \
	|| { echo 'error: qemu-system-x86_64 is not installed' >&2; exit 1; }

OVMF="$(find_ovmf)" || exit 1

# VARS carries the firmware's persistent variables, and this machine's are not
# worth keeping: a disk that has never booted has no boot entry written down,
# which is exactly the state the loader is built for.
#
# `build/` is where mkdisk.sh puts the image too, and that script runs as root -
# so on a tree where it has run, this copy is the first thing to notice if the
# build directory has ended up owned by someone this script cannot replace a
# file as. Said plainly, because `cp: permission denied` on a path the script
# just created sends the reader looking in the wrong place.
VARS="$BUILD_DIR/OVMF_VARS.4m.fd"
mkdir -p "$BUILD_DIR"
if ! cp -f "$OVMF/OVMF_VARS.4m.fd" "$VARS" 2>/dev/null; then
	echo "error: cannot write $VARS; $BUILD_DIR is not writable" >&2
	echo '       mkdisk.sh runs as root and can leave it owned by root; fix with' >&2
	echo "         sudo chown -R \$(id -un) $BUILD_DIR" >&2
	exit 1
fi

if [[ ! -f "$IMAGE" ]]; then
	# Building is mkdisk.sh's job and this returns the moment it is done rather
	# than booting the result: mkdisk.sh re-execs itself under sudo, and
	# holding that through a QEMU - which then runs the guest as root - is a
	# worse trade than running this script one more time.
	echo "run: $IMAGE is missing; building it" >&2
	"$ROOT/scripts/mkdisk.sh"
	echo >&2
	echo "run: $IMAGE is built; run this script again to boot it" >&2
	exit 0
fi

# An image older than the kernel or the loader it was built from boots the
# *previous* code, and says nothing about it. That reads exactly like a fix
# that did not work: the guest panics at the same address, the same way, and
# the obvious conclusion is that the change was wrong when in fact it was
# never in the image at all. So the staleness is checked and reported, and the
# answer to "I rebuilt and it still panics" stops being a mystery.
#
# A warning rather than a rebuild, because rebuilding re-execs into sudo: a
# script the user ran as themselves should say what is wrong and let them
# choose, not take root on its own.
stale=''
for artifact in \
	"${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-unknown-none/${PROFILE:-debug}/mpos" \
	"${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-unknown-uefi/${PROFILE:-debug}/mpos-boot.efi"
do
	if [[ -f "$artifact" && "$artifact" -nt "$IMAGE" ]]; then
		stale="$artifact"
	fi
done
if [[ -n "$stale" ]]; then
	echo "run: warning: $IMAGE predates $stale" >&2
	echo "run:          it boots the code from before that build, which will" >&2
	echo "run:          look like a fix that changed nothing. Rebuild first:" >&2
	echo "run:            ./scripts/mkdisk.sh" >&2
	echo >&2
fi

# The accelerator. KVM is the one that makes this usable - a TCG boot spends
# minutes where KVM spends seconds - but it needs /dev/kvm, which a container
# usually does not have, so it is used when it is there and fallen back from
# when it is not. ACCEL from the environment wins over both.
ACCEL="${ACCEL:-}"
if [[ -z "$ACCEL" ]]; then
	if [[ -w /dev/kvm ]]; then
		ACCEL=kvm
	else
		ACCEL=tcg
	fi
fi

cmd=(
	qemu-system-x86_64

	# q35 rather than the default i440fx: it is the chipset a UEFI machine
	# actually has, and its PCIe topology is what the virtio devices below are
	# written against.
	-machine q35 -accel "$ACCEL" -m "$MEM"

	# The firmware, as its two halves. The code half is read only and the
	# variables half is the copy made above.
	-drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF/OVMF_CODE.4m.fd"
	-drive if=pflash,format=raw,unit=1,file="$VARS"

	# The disk. Locking off because the image is this repository's own file,
	# nothing else has it open, and this environment refuses the fcntl lock
	# QEMU asks for - which QEMU treats as fatal rather than as a warning.
	#
	# No `disable-modern`: the kernel's virtio drivers are the
	# `virtio-drivers` crate's, and they speak VirtIO 1.0 through exactly the
	# capabilities that option turns off.
	-drive file="$IMAGE",format=raw,if=none,id=hd0,file.locking=off
	-device virtio-blk-pci,drive=hd0

	# User-mode networking: slirp answers as 10.0.2.2, forwards DNS at
	# 10.0.2.3, hands out addresses over its own DHCP, and asks the host for
	# nothing but IP. The resolver address it serves is the one written into
	# the image's /etc/resolv.conf.
	-netdev user,id=n0,hostname=mpos
	-device virtio-net-pci,netdev=n0
)

# The serial port goes to stdout and the monitor does not: mixing them puts
# QEMU's own prompt in the middle of the guest's console, and the guest console
# is the whole point of watching this. For interactive debugging - monitor,
# Ctrl-A C, Ctrl-A X - swap these three flags for `-nographic`.
cmd+=(-display none -monitor none -serial stdio -no-reboot)

# Printed because a boot that goes wrong is usually read off this line: the
# firmware found, the accelerator in use, and the flags the guest actually got.
echo "+ ${cmd[*]}" >&2
exec "${cmd[@]}"
