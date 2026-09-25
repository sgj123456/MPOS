# XDG_RUNTIME_DIR - where a Wayland compositor keeps its socket, and where
# every client looks for it.
#
# Stock Alpine has elogind make this directory at login; this machine's seat
# manager is seatd, which does not - it hands out the card and the console,
# not a runtime directory. So the directory is made at boot by
# /etc/local.d/xdg-runtime.start, and every login shell is told where it is
# here. The variable is only defaulted: a session that was handed one (by a
# display manager, by a test) keeps what it was given.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
