#!/usr/bin/env bash
# Boot the kernel under QEMU with the serial port attached to stdout.
#
# x86_64 boots the disk image built by mkdisk.sh under UEFI: the image carries
# its own bootloader in an EFI system partition, so there is nothing to attach
# but the disk itself.
#
# ARCH selects the machine: x86_64 (default), aarch64.
# Set TIMEOUT=<seconds> to kill QEMU automatically (used by the test harness);
# unset means run until Ctrl-A X.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
MEM="${MEM:-1G}"

# OVMF, the UEFI firmware QEMU runs instead of SeaBIOS. Its code half is read
# only and its variable half has to be writable, so the variables are copied
# into build/ once per run - firmware that cannot write a variable refuses to
# boot at all.
ovmf_dir() {
    local dir
    for dir in /usr/share/OVMF/x64 /usr/share/edk2/x64 /usr/share/OVMF \
               /usr/share/edk2-ovmf/x64; do
        if [[ -f "$dir/OVMF_CODE.4m.fd" && -f "$dir/OVMF_VARS.4m.fd" ]]; then
            echo "$dir"
            return 0
        fi
    done
    return 1
}

case "$ARCH" in
    x86_64)
        IMAGE="build/linux-x86_64.img"
        [[ -f "$IMAGE" ]] || "$ROOT/scripts/mkdisk.sh"
        OVMF_FW="$(ovmf_dir)" || {
            echo "error: no OVMF firmware found (looked in /usr/share/OVMF/x64)" >&2
            exit 1
        }
        VARS="build/OVMF_VARS.4m.fd"
        cp -f "$OVMF_FW/OVMF_VARS.4m.fd" "$VARS"
        BOOT=(-drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_FW/OVMF_CODE.4m.fd"
              -drive if=pflash,format=raw,unit=1,file="$VARS"
              # Locking off because the image is ours alone and this sandbox
              # refuses the fcntl lock QEMU asks for, which it treats as a
              # fatal error rather than a warning.
              -drive file="$IMAGE",format=raw,if=none,id=hd0,file.locking=off
              # No `disable-modern`: the virtio drivers in the kernel are the
              # `virtio-drivers` crate's, and they speak VirtIO 1.0 through the
              # capabilities that option turns off.
              -device virtio-blk-pci,drive=hd0)
        MACHINE=(-machine q35 -accel "${ACCEL:-tcg}"
                 -device isa-debug-exit,iobase=0xf4,iosize=0x04
                 # The user-mode network: slirp answers as 10.0.2.2, forwards
                 # DNS at 10.0.2.3, and needs nothing of the host but IP.
                 -netdev user,id=n0,hostname=mpos
                 -device virtio-net-pci,netdev=n0)
        QEMU=qemu-system-x86_64
        ;;
    aarch64)
        IMAGE="build/mpos-aarch64.elf"
        [[ -f "$IMAGE" ]] || "$ROOT/scripts/mkdisk.sh"
        BOOT=(-kernel "$IMAGE")
        MACHINE=(-machine virt -cpu cortex-a72)
        QEMU=qemu-system-aarch64
        ;;
    *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

# Clean serial-only stream: the test harness greps this log for pass/fail
# markers, so QEMU's monitor must not be multiplexed onto it. For interactive
# debugging swap these three flags for `-nographic` (exit with Ctrl-A X).
CMD=("$QEMU" "${MACHINE[@]}" -m "$MEM"
     -display none -monitor none -serial stdio
     -no-reboot
     "${BOOT[@]}")

if [[ -n "${TIMEOUT:-}" ]]; then
    CMD=(timeout --signal=KILL "$TIMEOUT" "${CMD[@]}")
fi

echo "+ ${CMD[*]}" >&2
exec "${CMD[@]}"
