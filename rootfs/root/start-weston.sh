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
# It therefore has to get two things right about a seat manager that outlives
# the desktop it serves, and both of them were bugs here once: it must ask
# whether a seatd is *answering* rather than whether `/run/seatd.sock` exists
# (a socket path survives its owner's death), and it must not start one inside
# weston's own process group (`^C` signals the whole foreground group, and the
# script `exec`s into weston, so the two shared a group - quitting the desktop
# killed the seat manager with it, and the next run found only the stale
# socket). See the notes at each step below.
#
# Run it from a gui boot (cargo xtask run --mode gui), at root's login prompt on
# the serial terminal - the window stays the compositor's to fill, so it is blank
# until this script runs weston:
#     ./start-weston.sh [extra weston args...]

set -eu

# weston needs a private runtime dir owned by the user it runs as (root here).
# The boot already made it; this covers the case where weston is started
# before that ran, or by an account whose directory was never made.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/0}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"

SOCK=/run/seatd.sock
PIDFILE=/run/seatd.pid

# Is there a seatd *answering*? A socket file is not the daemon: a unix socket's
# path stays in the filesystem after its owner dies (here and on Linux - nothing
# unlinks it), so `[ -e $SOCK ]` says only that some seatd once bound this name.
# A test on the file alone was the bug: quit the desktop, the seatd that this
# script started is gone, the stale `$SOCK` is left behind, and every later run
# dials a name nobody listens on - libseat reads the instant end-of-file as
# "Could not poll connection: I/O error" and gives up on the seat.
seatd_alive() {
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [ -z "$pid" ]; then
        pid="$(pidof seatd 2>/dev/null | cut -d' ' -f1 || true)"
    fi
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

if ! seatd_alive; then
    mkdir -p /run
    # Drop the name the dead daemon left, or the new one cannot bind it and
    # answers EADDRINUSE - which is how "restart the seat manager" turns into "no
    # seat manager at all, and a socket file that looks like one".
    rm -f "$SOCK"
    # `setsid`, and this is the other half of the same bug. A non-interactive
    # shell has no job control, so a background job it starts stays in the
    # shell's own process group - and this shell `exec`s into weston, so that
    # group *is* the compositor's. `^C` on the console signals the whole
    # foreground group (`src/vfs/tty.rs`: `send_signal_group`), which used to
    # take the seat manager down with the desktop it was serving. A new session
    # keeps `^C` an ordinary keystroke for weston alone.
    setsid seatd -g wheel </dev/null >/tmp/seatd.log 2>&1 &
    # Give the daemon a moment to bind the socket before weston connects.
    i=0
    while [ ! -e "$SOCK" ] && [ "$i" -lt 20 ]; do
        sleep 1
        i=$((i + 1))
    done
    if [ ! -e "$SOCK" ]; then
        echo "start-weston: seatd did not create $SOCK" >&2
        echo "--- /tmp/seatd.log ---" >&2
        cat /tmp/seatd.log >&2
        exit 1
    fi
fi

# Tell libseat (used by weston and by seatd-launch) where to find the seat.
export SEATD_SOCK="$SOCK"

echo "start-weston: backend=drm-backend.so renderer=gl seat=seatd" >&2
exec weston --backend=drm-backend.so --renderer=gl "$@"
