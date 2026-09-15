#!/usr/bin/env bash
# The whole gate, in order: formatting, clippy on both targets, host unit tests,
# then a QEMU boot with assertions on the serial log.
#
# Limine's own diagnostics never reach the serial port, so a marker-less log
# means the failure is in the bootloader, not the kernel; use
# scripts/screendump.sh to look at the display in that case.
#
# Usage: scripts/test.sh [marker ...]     (default: M0_OK M1_OK M2_OK M3_OK)
# PROFILE=release tests the optimised image instead of the debug one.
# Exit status is non-zero if any stage fails, any marker is missing, or the
# kernel panicked.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-x86_64}"
PROFILE="${PROFILE:-debug}"
HOST_TARGET="${HOST_TARGET:-x86_64-unknown-linux-gnu}"
TIMEOUT="${TIMEOUT:-30}"
LOG="build/serial-$ARCH-$PROFILE.log"

case "$ARCH" in
    x86_64)  TRIPLE="x86_64-unknown-none" ;;
    aarch64) TRIPLE="aarch64-unknown-none" ;;
    *) echo "unknown ARCH: $ARCH" >&2; exit 1 ;;
esac

MARKERS=("$@")
if [[ ${#MARKERS[@]} -eq 0 ]]; then
    MARKERS=("M0_OK" "M1_OK" "M2_OK" "M3_OK" "M4_OK" "M6_OK" "M7_OK" "M8_OK" "M9_OK" "M10_OK" "M11_OK" "M12_OK" "M13_OK" "M13_PIPE" "M13_REDIR" "M13_shell" "STDARGS_OK" "APK_OK" "M14_1" "URANDOM_16")
    # Every conformance test the shell script runs, asserted by its own
    # verdict. The milestone markers above say the *system* came up; these
    # say each ABI test passed - without them a `t_*` regression fails
    # quietly inside a green gate, because the suite's verdicts only ever
    # reached the serial log. The one exception is t_exit_group, whose
    # whole contract is the exit code it leaves.
    CONF=("t_getpid" "t_gettid" "t_getuid" "t_uname" "t_arch_prctl" "t_getrandom" \
        "t_sched_getaffinity" "t_set_robust_list" "t_rseq" "t_set_tid_address" \
        "t_openat" "t_read" "t_write" "t_lseek" "t_readv" "t_writev" "t_fstat" \
        "t_newfstatat" "t_statx" "t_access" "t_faccessat" "t_readlink" "t_fcntl" \
        "t_dup" "t_dup2" "t_dup3" "t_ioctl" "t_getdents64" "t_pipe2" "t_mkdirat" \
        "t_renameat2" "t_rename" "t_rmdir" "t_unlinkat" "t_getcwd" "t_chdir" "t_brk" "t_mmap" \
        "t_mprotect" "t_munmap" "t_mremap" "t_madvise" "t_rt_sigaction" \
        "t_rt_sigprocmask" "t_sigaltstack" "t_kill" "t_clock_gettime" \
        "t_nanosleep" "t_poll" "t_prlimit64" "t_futex" "t_execve" "t_fork" \
        "t_wait4" "t_clone" "t_socket" "t_sockopt" "t_tcp" "t_udp" "t_socketpair")
    for t in "${CONF[@]}"; do
        MARKERS+=("${t}_0")
    done
    MARKERS+=("t_exit_group_42" "STDSPAWN_0")
fi

mkdir -p build

# Formatting and lints are part of the gate. Clippy runs twice because the two
# targets see different code: the kernel target has no test harness at all, and
# the host target compiles the `#[cfg(test)]` modules the kernel target never
# sees. `--profile test` is what switches `cfg(test)` on for the host lib.
# The kernel clippy names its target explicitly: the repository default is the
# x86_64 kernel, and this script must lint whichever ARCH it is testing.
if ! cargo fmt --check; then
    echo "FAIL  formatting; run cargo fmt"
    exit 1
fi
if ! cargo clippy --quiet --lib --bins --target "$TRIPLE" -- -D warnings; then
    echo "FAIL  clippy on the kernel target"
    exit 1
fi
if ! cargo clippy --quiet --lib --profile test --target "$HOST_TARGET" -- -D warnings; then
    echo "FAIL  clippy on the host tests"
    exit 1
fi

# Half the tests run on the host: everything arch-agnostic in the kernel is in
# the library and gated so it builds as an ordinary native crate.
if ! cargo test --lib --target "$HOST_TARGET"; then
    echo "FAIL  host unit tests"
    exit 1
fi

if ! PROFILE="$PROFILE" "$ROOT/scripts/mkdisk.sh" >/dev/null; then
    echo "FAIL  kernel packaging failed; refusing to test a stale image"
    PROFILE="$PROFILE" "$ROOT/scripts/mkdisk.sh" 2>&1 | tail -30
    exit 1
fi

# Subshell + discarded stderr keeps bash's job-control noise out of the output.
( ARCH="$ARCH" TIMEOUT="$TIMEOUT" "$ROOT/scripts/run.sh" >"$LOG" 2>&1 ) 2>/dev/null
rc=$?

# QEMU exit statuses, one pair per arch:
#   x86_64: isa-debug-exit maps the guest value v to host status (v << 1) | 1,
#           so EXIT_SUCCESS (0x10) lands on 33 and EXIT_FAILURE (0x11) on 35.
case "$ARCH" in
    x86_64)  readonly CLEAN_EXIT=33; readonly PANIC_EXIT=35 ;;
    *)       readonly CLEAN_EXIT=0;  readonly PANIC_EXIT=1 ;;
esac

status=0
for marker in "${MARKERS[@]}"; do
    if grep -qF "$marker" "$LOG"; then
        echo "PASS  $marker"
    else
        echo "FAIL  missing marker: $marker"
        status=1
    fi
done

# From M13 on, the kernel does not exit: it hands the machine to init and
# stays up, so the watchdog killing QEMU is how a passing run ends. What makes
# it a pass is the markers, not the exit status - which is why the marker loop
# above runs first and this only has to tell "up and running" from "stuck".
case "$rc" in
    "$CLEAN_EXIT") ;;
    "$PANIC_EXIT")
        echo "FAIL  kernel exited with panic status"
        status=1
        ;;
    124 | 137)
        if [[ $status -ne 0 ]]; then
            echo "FAIL  QEMU was stopped by the ${TIMEOUT}s watchdog with markers missing"
        else
            echo "PASS  the system stayed up until the ${TIMEOUT}s watchdog stopped it"
        fi
        ;;
    *)
        echo "FAIL  unexpected QEMU exit status $rc"
        status=1
        ;;
esac

if grep -qF 'KERNEL PANIC' "$LOG"; then
    echo "FAIL  kernel panicked"
    grep -A3 'KERNEL PANIC' "$LOG"
    status=1
fi

# Init dying is not a panic, but it is the end of the system: the kernel halts
# rather than run a machine with nobody to start programs on.
if grep -qF 'INIT_EXITED' "$LOG"; then
    echo "FAIL  init exited"
    status=1
fi

if [[ $status -ne 0 ]]; then
    echo "--- serial log ($LOG) ---"
    cat "$LOG"
fi

exit $status
