#!/usr/bin/env bash
# Boot the kernel with a QEMU monitor socket and dump the display to
# build/screen-$ARCH.png.
#
# Limine and the UEFI firmware report their own failures (no boot entry, bad
# config, unsupported revision) only on the display, never on the serial port.
# When scripts/test.sh shows an empty serial log, this is how you find out why.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
SECONDS_TO_WAIT="${WAIT:-12}"
DISK="build/linux-$ARCH.img"
SOCK="/tmp/mpos-mon-$ARCH.sock"
PPM="/tmp/mpos-screen-$ARCH.ppm"
PNG="build/screen-$ARCH.png"

[[ -f "$DISK" ]] || "$ROOT/scripts/mkdisk.sh" >/dev/null

rm -f "$SOCK" "$PPM" "$PNG"

case "$ARCH" in
    x86_64)
        QEMU=qemu-system-x86_64
        OVMF_FW="${OVMF_FW:-/usr/share/OVMF/x64}"
        cp -f "$OVMF_FW/OVMF_VARS.4m.fd" build/OVMF_VARS.4m.fd
        MACHINE=(-machine q35
                 -drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_FW/OVMF_CODE.4m.fd"
                 -drive if=pflash,format=raw,unit=1,file=build/OVMF_VARS.4m.fd
                 -drive file="$DISK",format=raw,if=none,id=hd0,file.locking=off
                 -device virtio-blk-pci,drive=hd0,disable-modern=on
                 -net none)
        ;;
    aarch64)
        QEMU=qemu-system-aarch64
        DISK="build/mpos-$ARCH.elf"
        MACHINE=(-machine virt -cpu cortex-a72 -kernel "$DISK")
        ;;
    *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

"$QEMU" "${MACHINE[@]}" -m 1G -display none \
    -monitor "unix:$SOCK,server,nowait" \
    -serial file:build/serial-$ARCH.log \
    -no-reboot >/dev/null 2>&1 &
QPID=$!

sleep "$SECONDS_TO_WAIT"
{
    printf 'screendump %s\n' "$PPM"
    sleep 2
    printf 'quit\n'
    sleep 1
} | nc -U "$SOCK" >/dev/null 2>&1 || true

sleep 1
kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true

if [[ ! -f "$PPM" ]]; then
    echo "error: screendump produced nothing" >&2
    exit 1
fi

magick "$PPM" -resize 1400x "$PNG"
echo "wrote $PNG"
