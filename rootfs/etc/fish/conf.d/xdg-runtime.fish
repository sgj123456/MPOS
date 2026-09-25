# XDG_RUNTIME_DIR - where a Wayland compositor keeps its socket.
#
# The same value /etc/profile.d/xdg-runtime.sh gives the POSIX shells, for
# the shell this image gives the unprivileged account. Fish sources every
# /etc/fish/conf.d/*.fish at startup, and only sets the variable when the
# session was not handed one already.
if not set -q XDG_RUNTIME_DIR
    set -gx XDG_RUNTIME_DIR /run/user/(id -u)
end
