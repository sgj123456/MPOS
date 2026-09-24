#!/bin/sh
# Start weston on mpos via its DRM backend.
#
# weston's DRM backend reaches the card through a *seat*, not directly: it asks
# libseat to open a session, and libseat has two ways to do that on Linux -
# systemd's logind (which this image has no D-Bus for) and seatd. So this
# script brings seatd up and points weston at it. seatd owns the console (the
# kernel's VT_* / KD_* ioctls) and the DRM master (the card's SET_MASTER), and
# hands both to weston, which then drives /dev/dri/card0 with the pixman
# software renderer - CPU pixels, no GL, the display path mpos actually builds.
#
# Run it from a compositor boot (cargo xtask run --console compositor):
#     ./start-weston.sh [extra weston args...]

set -eu

# weston needs a private runtime dir owned by the user it runs as (root here).
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/0}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"

SOCK=/run/seatd.sock

# Bring up a seatd if one is not already listening. `-g wheel` lets the wheel
# group (this user is in it) reach the socket.
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

echo "start-weston: backend=drm-backend.so renderer=pixman seat=seatd" >&2
exec weston --backend=drm-backend.so --renderer=pixman "$@"
