#!/bin/bash
# Build the probe and boot it on a real Linux, twice: once on a virtio-gpu card
# with no 3D behind it, once on the GL card. The output is the measurement the
# kernel's DRM_VIRTGPU_* family is designed from (NOTES.md §7.4).
#
# Not part of `cargo xtask`: it measures Linux, not this kernel, and it wants
# the network and a Linux guest. Run it by hand when the answers are needed.
#
#   ./run.sh            both machines
#   ./run.sh nogl       just virtio-gpu-pci
#   ./run.sh gl         just virtio-gpu-gl-pci + -display egl-headless,gl=on
#
# Needs: gcc (or musl-gcc), cpio, gzip, qemu-system-x86_64, curl, sudo for the
# initramfs repack (Alpine's tree contains root-owned entries).

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-/tmp/virtgpu-probe}
base=https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/netboot

# Alpine's *virt* kernel, not lts: the netboot initramfs already carries
# virtio-gpu.ko, drm.ko, drm_kms_helper.ko and a modules.dep whose closure
# covers them, so the 218 MB lts modloop is not needed for this measurement at
# all. That is the whole reason the guest below boots in seconds.
mkdir -p "$work"
cd "$work"
for f in vmlinuz-virt initramfs-virt; do
    [ -s "$f" ] || curl -sSL --retry 5 -o "$f" "$base/$f"
done

# The probe: static, so the guest needs no libc of ours. glibc-static is fine -
# a static binary runs on musl.
gcc -static -O2 -o probe "$here/probe.c"

# The guest's entire userspace: mount the pseudo-filesystems, load the one
# driver under test, run the probe. Alpine's own /init would spend its time
# looking for boot media and an apkovl, each a way for the boot to fail for a
# reason that is not the virtio-gpu driver.
rm -rf root && mkdir root
sudo sh -c "gunzip -c initramfs-virt | (cd root && cpio -idm --quiet)"
sudo cp probe root/probe
sudo cp "$here/probe-init" root/init
sudo chmod 755 root/init
(cd root && sudo sh -c 'find . | cpio -o -H newc --quiet | gzip -9') > initramfs-probe

run() {
    local name=$1 device=$2 display=$3
    echo "=== $name ($device${display:+, display=$display}) ==="
    # `-serial stdio` rather than `-nographic`: -nographic implies -display none,
    # and a GL device with no GL display backend is the one combination that
    # cannot work.
    qemu-system-x86_64 -m 2048 -smp 2 -no-reboot \
        -kernel vmlinuz-virt -initrd initramfs-probe \
        -append "console=ttyS0 quiet" \
        -device "$device" -display "$display" \
        -serial stdio -monitor none > "$name.log" 2>&1 || true
    sed -n '/=== kernel:/,/=== measurement guest done/p' "$name.log"
    echo "    (full output in $work/$name.log)"
}

case "${1:-both}" in
nogl) run nogl virtio-gpu-pci none ;;
gl) run gl virtio-gpu-gl-pci "egl-headless,gl=on" ;;
*) run nogl virtio-gpu-pci none; run gl virtio-gpu-gl-pci "egl-headless,gl=on" ;;
esac
