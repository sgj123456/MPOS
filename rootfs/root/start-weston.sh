#!/bin/sh
# Start weston on mpos via its DRM backend, with the GL renderer.
#
# weston's DRM backend reaches the card through a *seat*, not directly: it
# asks libseat to open a session, and libseat has two ways to do that on Linux -
# systemd's logind (which this image has no D-Bus for) and seatd. seatd owns
# the console (the kernel's VT_* / KD_* ioctls) and the DRM master (the card's
# SET_MASTER), and hands both to weston, which then drives /dev/dri/card0.
# Rendering is Mesa's software GL (llvmpipe) over GBM dumb buffers - no
# GPU-side GL - so the renderer is gl, the one module the Alpine weston
# actually ships (its pixman renderer is listed in --help but has no module in
# the package).
#
# A stock boot already has seatd running as a service (`SERVICES` in
# `xtask/src/rootfs.rs`), so on an image built from this tree plain `weston`
# works from any login shell: /etc/profile.d/xdg-runtime.sh points
# XDG_RUNTIME_DIR at the directory /etc/local.d/xdg-runtime.start made, and
# libseat finds /run/seatd.sock. This script is for a machine where the
# service is not up - a live image that predates it, a runlevel that was
# emptied - and brings seatd up itself before handing over.
#
# Run it from a weston boot (cargo xtask run --mode weston):
#     ./start-weston.sh [extra weston args...]

set -eu

# weston needs a private runtime dir owned by the user it runs as (root here).
# The boot already made it; this covers the case where weston is started
# before that ran, or by an account whose directory was never made.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/0}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"

SOCK=/run/seatd.sock

# Bring up a seatd if the service is not already listening. `-g wheel` lets the
# wheel group (this user is in it) reach the socket.
if [ ! -e "$SOCK" ]; then
    mkdir -p /run
    seatd -g wheel &
    # Give the daemon a moment to bind the socket before weston connects.
    i=0
    while [ ! -e "$SOCK" ] && [ "$i" -lt 20 ]; do
        sleep 1
        i=$((i + 1))
    done
    if [ ! -e "$SOCK" ]; then
        echo "start-weston: seatd did not create $SOCK" >&2
        exit 1
    fi
fi

# Tell libseat (used by weston and by seatd-launch) where to find the seat.
export SEATD_SOCK="$SOCK"

echo "start-weston: backend=drm-backend.so renderer=gl seat=seatd" >&2
exec weston --backend=drm-backend.so --renderer=gl "$@"
