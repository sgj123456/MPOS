//! t-sh - diagnostic: an interactive busybox ash, exec'd straight onto the
//! console terminal (the machine's own serial node), with the test as its
//! parent ghost.
//!
//! What this pins down is where an interactive shell stops before its first
//! prompt. The child's standard descriptors are the console tty, so the
//! prompt it prints - or the warning it prints instead - lands on the serial
//! wire unfiltered. The parent then reads the child's /proc stat and prints
//! it: the state letter says whether the shell is stopped (a signal it never
//! handled), sleeping (a syscall that never ended), running, or gone.
//!
//! # The migration
//!
//! The doors: `libc::fork` for the creation (rustix binds none - batch 18's
//! reason, the one `t-execve` and `t-forkcost` give), `libc::execve` for the
//! image (rustix's is in the doc-hidden `runtime` module - clause (a) again),
//! `libc::setpgid`, `libc::kill` and `libc::sched_yield` for the three words
//! the no-std harness spelled by hand, and `rustix::process::waitpid` -
//! `wait4` with a null `rusage` - for the reap. The open rides rustix's
//! `openat`; the dup2s go raw because the target descriptors 0/1/2 have no
//! `&mut OwnedFd` for `rustix::io::dup2` to name (clause (c), the shape
//! `t-dup3` records).
//!
//! One fixture fact moves with the program rather than being re-decided here:
//! the console node is the machine's psABI name for its first serial port -
//! `ttyS0` on a 16550, `ttyAMA0` on the PL011 - which is exactly what the
//! kernel registers it as (`src/drivers/devices.rs`, one node either way). A
//! hardcoded `/dev/ttyS0` would open nothing on the second machine, and this
//! program is in no *machine-named* group: both arms' payloads carry it.
//!
//! It stays in `diagnostic`, which is the one reason-group both arms honour
//! (`xtask/src/sweep.rs`'s `hand_read`): it is not a check at all. Its verdict
//! covers only the four claims the parent can actually make (the tty opened,
//! the fork happened, the child's `/proc` entry read, the child was reaped);
//! the shell's own behaviour is the text on the wire, for a person to read when
//! a shell problem needs looking at.

use std::ffi::{CStr, CString};
use std::io::Write;

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::read;
use rustix::process::{Pid, WaitOptions, waitpid};

/// The console node, per machine - see the header for why it cannot be one name.
#[cfg(target_arch = "x86_64")]
const TTY: &CStr = c"/dev/ttyS0";
#[cfg(target_arch = "aarch64")]
const TTY: &CStr = c"/dev/ttyAMA0";

const BUSYBOX: &CStr = c"/bin/busybox";

/// The third whitespace-separated field of a `/proc/<pid>/stat` line: Linux's
/// state letter. `T` is stopped, `S` sleeping, `R` running.
fn state_letter(bytes: &[u8]) -> u8 {
    let mut field = 0;
    let mut in_field = false;
    for &b in bytes {
        if b == b' ' || b == b'\n' {
            if in_field {
                in_field = false;
                field += 1;
            }
        } else {
            if !in_field {
                in_field = true;
                if field == 2 {
                    return b;
                }
            }
            if field == 2 {
                return b;
            }
        }
    }
    b'?'
}

fn main() {
    println!("t-sh: spawning an interactive ash on the console tty");
    let mut r = Report::new();

    let tty = openat(CWD, TTY, OFlags::RDWR, Mode::empty());
    r.check(
        &format!("the console tty opens ({:?})", tty.as_ref().err()),
        tty.is_ok() || host_run(),
    );

    // SAFETY: `fork` in a single-threaded program; the child only calls
    // async-signal-safe work, `dup2`, and `execve`, which replaces it whole.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The child's own process group first, and this is the load-bearing
        // part of the whole test. An interactive shell that finds itself in
        // the background of its controlling terminal raises `SIGTTIN` to stop
        // the group it is in - which until now is the group it *inherited*,
        // and `t-sh` is in it too. The stop then takes the test down with the
        // shell and the run ends with nothing printed at all. This is no more
        // than what every shell does before it `exec`s a job.
        let got = unsafe { libc::setpgid(0, 0) };
        println!("t-sh: child setpgid = {got}");
        let _ = std::io::stdout().flush();

        // The child: stdio onto the console terminal, then the shell. The
        // raw fd is -1 when the open failed, and the dup2s then fail the same
        // way the no-std version's did - the exec below is the claim.
        let child_tty = tty.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1);
        unsafe {
            libc::dup2(child_tty, 0);
            libc::dup2(child_tty, 1);
            libc::dup2(child_tty, 2);
        }

        let arg0 = c"busybox";
        let arg1 = c"sh";
        let argv: [*const libc::c_char; 3] = [arg0.as_ptr(), arg1.as_ptr(), core::ptr::null()];
        let path_env = c"PATH=/bin";
        let term_env = c"TERM=vt100";
        let envp: [*const libc::c_char; 3] =
            [path_env.as_ptr(), term_env.as_ptr(), core::ptr::null()];
        unsafe {
            let _ = libc::execve(BUSYBOX.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }
        // Only reachable when the exec failed; leave through `exit_group`, the
        // door that cannot flush the stdout copy this child inherited.
        unsafe {
            libc::syscall(libc::SYS_exit_group, 127 as libc::c_long);
            libc::_exit(127);
        }
    }
    r.check("the shell was forked", pid > 0);
    // The parent says it too, so the job's group does not depend on which of
    // the two reached the call first. Either may find the other already there.
    unsafe {
        libc::setpgid(pid, pid);
    }

    // Give the shell a moment, then report its /proc state.
    println!("t-sh: parent probing");
    for _ in 0..50 {
        unsafe { libc::sched_yield() };
    }
    println!("t-sh: yields done");

    let path = CString::new(format!("/proc/{pid}/stat")).expect("a pid path has no interior NUL");
    let fd = openat(CWD, &path, OFlags::RDONLY, Mode::empty());
    r.check("the child's /proc entry opens", fd.is_ok());
    if let Ok(fd) = fd.as_ref() {
        let mut buf = [0u8; 192];
        match read(fd, &mut buf) {
            Ok(got) if got > 0 => {
                // The no-std version wrote the raw bytes, newline included.
                let _ = std::io::stdout().write_all(&buf[..got]);
                println!("t-sh: state = {}", state_letter(&buf[..got]) as char);
            }
            _ => println!("t-sh: no stat to read"),
        }
    }

    // The shell has no stdin worth waiting on and nobody to type to it: left
    // alive it holds the console and outlives the test that spawned it. SIGCONT
    // first, because a stopped task does not run its own death on SIGKILL.
    unsafe {
        libc::kill(pid, libc::SIGCONT);
        libc::kill(pid, libc::SIGKILL);
    }
    let want = Pid::from_raw(pid);
    let reaped = matches!(
        waitpid(want, WaitOptions::empty()),
        Ok(Some((got, _))) if Some(got) == want
    );
    r.check("the shell was reaped", reaped);
    r.finish();
}
