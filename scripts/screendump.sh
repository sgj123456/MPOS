#!/usr/bin/env bash
# Boot the kernel with a QEMU monitor socket and dump the display to
# build/screen-$ARCH.png.
#
# Limine reports its own failures (missing stage 3, bad config, unsupported
# revision) only on the display, never on the serial port. When scripts/test.sh
# shows an empty serial log, this is how you find out why.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
SECONDS_TO_WAIT="${WAIT:-12}"
ISO="build/mpos-$ARCH.iso"
SOCK="/tmp/mpos-mon-$ARCH.sock"
PPM="/tmp/mpos-screen-$ARCH.ppm"
PNG="build/screen-$ARCH.png"

[[ -f "$ISO" ]] || "$ROOT/scripts/mkiso.sh" >/dev/null

rm -f "$SOCK" "$PPM" "$PNG"

case "$ARCH" in
    x86_64)  QEMU=qemu-system-x86_64;  MACHINE=(-machine q35) ;;
    aarch64) QEMU=qemu-system-aarch64; MACHINE=(-machine virt -cpu cortex-a72) ;;
    *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

"$QEMU" "${MACHINE[@]}" -m 1G -display none \
    -monitor "unix:$SOCK,server,nowait" \
    -serial file:build/serial-$ARCH.log \
    -no-reboot -cdrom "$ISO" >/dev/null 2>&1 &
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
