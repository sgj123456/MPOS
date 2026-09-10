#!/usr/bin/env bash
# Boot the kernel under QEMU with the serial port attached to stdout.
#
# ARCH selects the machine: x86_64 (default), aarch64.
# Set TIMEOUT=<seconds> to kill QEMU automatically (used by the test harness);
# unset means run until Ctrl-A X.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
MEM="${MEM:-1G}"

# x86_64 boots an ISO and mounts a disk.
case "$ARCH" in
    x86_64)
        IMAGE="build/mpos-x86_64.iso"
        BOOT=(-cdrom "$IMAGE"
              -drive file="build/disk-x86_64.img",format=raw,if=none,id=hd0
              -device virtio-blk-pci,drive=hd0,disable-modern=on)
        MACHINE=(-machine q35 -accel "${ACCEL:-tcg}"
                 -device isa-debug-exit,iobase=0xf4,iosize=0x04)
        QEMU=qemu-system-x86_64
        ;;
    aarch64)
        IMAGE="build/mpos-aarch64.elf"
        BOOT=(-kernel "$IMAGE")
        MACHINE=(-machine virt -cpu cortex-a72)
        QEMU=qemu-system-aarch64
        ;;
    *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

if [[ ! -f "$IMAGE" ]] || { [[ "$ARCH" == x86_64 ]] && [[ ! -f "build/disk-x86_64.img" ]]; }; then
    "$ROOT/scripts/mkiso.sh"
fi

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
