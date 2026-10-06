//! t-reboot-libc - `reboot(2)`'s *policy* words are answered on both machines.
//!
//! busybox `init` asks for `LINUX_REBOOT_CMD_CAD_OFF` about a second into its
//! startup - it is how init takes Ctrl-Alt-Del for the `::ctrlaltdel:` line of
//! `/etc/inittab` - and `LINUX_REBOOT_CMD_CAD_ON` when it hands the machine
//! back. Neither ends anything: they are the two words that decide *whose*
//! keystroke is the three-finger salute, and Linux answers both with `0`.
//!
//! The second machine refused both. Its dispatch arm was gated
//! `#[cfg(not(target_arch = "aarch64"))]`, written when a *stop* word ended at
//! the x86_64 `isa-debug-exit` port - which is a `hal` capability now
//! (`hal::exit_qemu`, which a machine without the flag answers by halting). So
//! the refusal was a marker left behind by a capability that took the port's
//! place, and it was visible on the machine's very first real userspace:
//! `mpos: syscall 142 -> ENOSYS` printed on every aarch64 boot, once, between
//! the kernel's own handoff and the login prompt - `142` being
//! `__NR_reboot` on arm64, where x86_64's `142` is `sched_setparam`.
//!
//! What this pins is the shape a caller branches on: the two policy words are
//! *served*, and a command no kernel knows is `EINVAL` rather than `ENOSYS`
//! (Linux reads the command word and refuses it; it does not say "no such
//! call"). The stop words are deliberately not exercised - `RESTART`, `HALT`
//! and `POWER_OFF` end the machine on both sides of this test.
//!
//! Two facts worth naming. The call goes through `libc::reboot`, which is what
//! a C program - busybox init included - reaches for, and musl's wrapper is
//! what supplies the two magic words, so the test pins the *entry point* the
//! boot uses rather than this file's memory of the pair. And the kernel does
//! not ask who is calling: Linux requires `CAP_SYS_BOOT` and answers `EPERM` to
//! an unprivileged caller, where this handler answers `0` to anyone. That
//! divergence is pre-existing on both machines, is named here rather than
//! hidden, and is why the test asserts `0` only as root - which is the identity
//! a test sweep runs as.

use std::ffi::c_int;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// `libc::reboot(type)`, with `errno` read immediately.
fn reboot(command: c_int) -> Result<(), i32> {
    // SAFETY: `reboot` takes the command word and fills the two magic words
    // itself; the call has no argument this program owns. Only the *policy*
    // words below are passed, so no machine is asked to stop.
    let rc = unsafe { libc::reboot(command) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn main() {
    println!("t-reboot-libc");
    let mut r = Report { failed: false };

    if unsafe { libc::geteuid() } != 0 {
        // Linux's own answer for an unprivileged caller would be `EPERM`, and
        // this kernel does not model the capability - so the pin would be
        // asserting a divergence rather than the contract.
        println!("skip:  not root, so the policy words are not this kernel's to answer");
        println!("PASS");
        return;
    }

    // The word busybox init sends first: hand Ctrl-Alt-Del over. A machine that
    // answers `ENOSYS` here prints a missing-call line on every boot.
    match reboot(libc::LINUX_REBOOT_CMD_CAD_OFF) {
        Ok(()) => r.check("reboot(LINUX_REBOOT_CMD_CAD_OFF) is served (got 0)", true),
        Err(errno) => r.check(
            &format!(
                "reboot(LINUX_REBOOT_CMD_CAD_OFF) is served (got errno {errno}, not ENOSYS {})",
                libc::ENOSYS
            ),
            false,
        ),
    }
    // And the word it sends when it gives the machine back.
    match reboot(libc::LINUX_REBOOT_CMD_CAD_ON) {
        Ok(()) => r.check("reboot(LINUX_REBOOT_CMD_CAD_ON) is served (got 0)", true),
        Err(errno) => r.check(
            &format!("reboot(LINUX_REBOOT_CMD_CAD_ON) is served (got errno {errno})"),
            false,
        ),
    }
    // A command no kernel knows is refused *as a command*: `EINVAL`, the same
    // answer `poweroff -f`'s bad spelling gets, and never `ENOSYS` - which
    // would say the call is missing rather than that the word is.
    match reboot(0xdead_beefu32 as c_int) {
        Ok(()) => r.check(
            "reboot(0xdeadbeef) is EINVAL (got success, which no kernel owes it)",
            false,
        ),
        Err(errno) => r.check(
            &format!("reboot(0xdeadbeef) is EINVAL (got errno {errno})"),
            errno == libc::EINVAL,
        ),
    }

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
