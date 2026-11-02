//! t-fault-report - a userland fault is filed under the program's own name.
//!
//! Pinned here: when a process takes a fault it cannot survive, the kernel log
//! gains one line, and that line says **which program** faulted. Linux's shape
//! (`docs/notes/` §6.73, measured on 7.2.11) is
//!
//! ```text
//! comm[tid]: segfault at <addr> ip <ip> sp <sp> error <bits>
//! ```
//!
//! and the program name is the half that matters: a boot log that says
//! `task=10` describes a row of the scheduler's table, which a reader does not
//! have, while `udevd[312]` names the program they were looking at. This
//! kernel printed `user #PF at 0x10 code=…` - correct, and unusable for exactly
//! that reason.
//!
//! Three shapes, because Linux has three and they are not interchangeable:
//!
//! * a read through a null pointer - `segfault at 0 … error 4`;
//! * a `#GP` with no faulting address - the `traps:` line, which carries no
//!   `at <addr>` because the machine never named one (a ring-3 `hlt` is the
//!   shortest way to take one);
//! * and the program name in both, since that is the whole point.
//!
//! The child sets its own `comm` first (`PR_SET_NAME`), so the name searched
//! for is one this program invented: a report that fell back to the binary's
//! path or to a slot number cannot accidentally match it.
//!
//! # Why the doors are raw
//!
//! `syslog(2)` is the raw door for the same reason `t-syslog` gives: every
//! answer is a count, a zero or an errno. `fork`/`wait4` are raw because the
//! claim is a *child's death* observed through the status word. Under
//! `MPOS_ON_HOST` this sandbox's Linux answers `EPERM` for every `syslog` type
//! (`dmesg_restrict`), so the log claims are reported as unmeasurable there
//! rather than failing - the pin keeps pinning this kernel, whose answers are
//! what the claims were written against.

use mpos_user_tests::{Report, host_run};

/// The signal an uncaught fault delivers.
const SIGSEGV: i32 = 11;
/// `PR_SET_NAME`, and the name the faulting child gives itself. Short enough
/// to survive `comm`'s 16-byte cap with room for the terminator.
const PR_SET_NAME: i32 = 15;
const CHILD_NAME: &[u8] = b"tfaultchild\0";

/// `syslog(type, buf, len)` through the raw door: the byte count or `-errno`.
fn klog(kind: i32, buf: *mut u8, len: usize) -> isize {
    // SAFETY: `buf` is either null or this program's own buffer, which
    // outlives the call; the kernel writes only inside `len`.
    let r = unsafe {
        libc::syscall(
            libc::SYS_syslog,
            kind as libc::c_long,
            buf as libc::c_long,
            len as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// The whole log ring, non-destructively (`READ_ALL`), or `None` when the
/// door refuses - which is this sandbox's Linux, not the machine under test.
fn log_text() -> Option<String> {
    let mut buf = vec![0u8; 16384];
    let got = klog(3, buf.as_mut_ptr(), buf.len());
    if got <= 0 {
        return None;
    }
    buf.truncate(got as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Whether `hay` holds a line containing every one of `needles`.
fn has_line(hay: &str, needles: &[&str]) -> bool {
    hay.lines()
        .any(|line| needles.iter().all(|needle| line.contains(needle)))
}

/// Fork a child that runs `body` and then dies of `signal`, and answer the
/// `(pid, status)` the parent reaped.
///
/// The child is given `CHILD_NAME` before it runs, so the report it earns
/// carries a name this program chose.
fn reap_a_child(body: impl FnOnce()) -> (i32, i32) {
    // SAFETY: `fork` in a single-threaded test program. The child never
    // returns from `body` - it either faults or is killed by `raise`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed: {pid}");
    if pid == 0 {
        // SAFETY: `prctl` with `PR_SET_NAME` reads a NUL-terminated string this
        // program owns; the child is about to die, so nothing outlives it.
        unsafe {
            libc::prctl(PR_SET_NAME, CHILD_NAME.as_ptr(), 0, 0, 0);
        }
        body();
        // A child that reached here took no fault, so make it die anyway: an
        // exited child is a clean status the assertions below can tell apart
        // from the signal they are looking for.
        unsafe { libc::_exit(7) };
    }
    let mut status = 0i32;
    // SAFETY: `wait4` on the pid just forked, with a status word this call owns.
    let reaped = unsafe { libc::wait4(pid, &mut status, 0, core::ptr::null_mut()) };
    assert_eq!(reaped, pid, "wait4 reaped {reaped}, wanted {pid}");
    (pid, status)
}

/// Whether `status` is a death by `signal`.
fn died_of(status: i32, signal: i32) -> bool {
    status & 0x7f == signal
}

fn main() {
    println!("t-fault-report");
    let mut r = Report::new();

    // A read through a null pointer: Linux files `segfault at 0 … error 4`,
    // and the 4 is the fault word's own "user, read, not present".
    let (pid, status) = reap_a_child(|| {
        // SAFETY: deliberately dereferencing null - that is the fault under
        // test, and this process is the one that dies of it.
        unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) };
    });
    r.check(
        "a read through null ends the child with SIGSEGV",
        died_of(status, SIGSEGV),
    );
    match log_text() {
        None => {
            println!(
                "note: this platform will not hand over the kernel log (EPERM) - the log claims are not measurable here"
            );
            r.check("the kernel log names the program that faulted", true);
        }
        Some(log) => {
            let name = String::from_utf8_lossy(CHILD_NAME.split(|b| *b == 0).next().unwrap_or(b""))
                .into_owned();
            let named = alloc_format(&name, pid);
            r.check(
                "the kernel log names the program that faulted, and its thread",
                log.contains(&named),
            );
            r.check(
                "and files the fault in Linux's shape: at, ip, sp, error",
                has_line(&log, &[&named, "segfault at 0 ", " ip ", " sp ", " error "]),
            );
            // The error column is the machine's word. x86_64's is the page
            // fault's: bit 0 present, 1 write, 2 user. A user read of an
            // absent page is 4, and a write is 6 - the two rows a reader uses
            // to tell "dereferenced a bogus pointer" from "stored through it".
            // The error column is the *machine's* word, so the value is
            // claimed only where it was measured: x86_64's page-fault code, on
            // Linux 7.2.11 (`docs/notes/` §6.73). This machine has no Linux to
            // measure against, so here the claim is the column's presence and
            // the value stays the machine's own business - printing a number
            // and pinning it to another architecture's vocabulary is how a
            // report starts lying in a shape that reads correct.
            #[cfg(target_arch = "x86_64")]
            r.check(
                "and the error column says user, read, not present (4)",
                has_line(&log, &[&named, "segfault at ", " error 4"]),
            );
            #[cfg(not(target_arch = "x86_64"))]
            r.check(
                "and the error column carries this machine's own fault word",
                has_line(&log, &[&named, "segfault at ", " error "]),
            );
        }
    }

    // The second shape, and it is the *first machine's* shape only: `traps:
    // … general protection fault` is what x86_64 has a `#GP` for. AArch64 has
    // no equivalent trap class - an EL0 `brk` is a breakpoint (`SIGTRAP`) and
    // an undefined instruction is `SIGILL`, neither of which is a "general
    // protection fault" and neither of which Linux's arm64 port files under
    // `traps:`. Claiming it on the second machine would be a claim about a
    // shape that machine does not have, so the claim is x86_64's and the
    // second machine says so out loud rather than passing silently.
    #[cfg(target_arch = "x86_64")]
    {
        let (trap_pid, trap_status) = reap_a_child(|| {
            // SAFETY: `hlt` at ring 3 is the privilege violation under test.
            unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
        });
        r.check(
            "a ring-3 hlt ends the child with SIGSEGV",
            died_of(trap_status, SIGSEGV),
        );
        match log_text() {
            None => r.check(
                "a trap with no address is filed in Linux's traps shape",
                true,
            ),
            Some(log) => {
                let name =
                    String::from_utf8_lossy(CHILD_NAME.split(|b| *b == 0).next().unwrap_or(b""))
                        .into_owned();
                let named = alloc_format(&name, trap_pid);
                r.check(
                    "a trap with no address is filed in Linux's traps shape",
                    has_line(&log, &[&named, "traps:", "general protection fault"]),
                );
                // The negative half, stated against the trap lines themselves
                // rather than against the whole log: asking "does any line hold
                // these three words" answers yes for any *other* line that
                // happens to carry two of them, which is a different question.
                let trap_lines: Vec<&str> = log
                    .lines()
                    .filter(|line| line.contains("general protection fault"))
                    .collect();
                r.check(
                    "and it carries no invented faulting address",
                    !trap_lines.is_empty() && trap_lines.iter().all(|line| !line.contains(" at ")),
                );
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    println!("note: the traps: shape is x86_64's - this machine has no general protection fault");

    // The third shape: the same absent page, written instead of read. The
    // `error` column changes to 6, and that is the whole reason the column is
    // worth printing - 4 says the program *read* a bad pointer, 6 says it
    // *stored* through one, and those are different bugs.
    //
    // Deliberately not the "a handler swallowed it" case: measured on Linux, a
    // SIGSEGV handler that returns re-faults the same instruction forever, so
    // that child hangs rather than ending, and a claim built on it would be a
    // claim about Linux that Linux does not make. What Linux does do - file
    // nothing for a fault it survives - needs `siglongjmp` out of the handler,
    // which is a test of the signal frame rather than of this report.
    let (write_pid, write_status) = reap_a_child(|| {
        // SAFETY: deliberately storing through null - the fault under test.
        unsafe { core::ptr::write_volatile(core::ptr::null_mut::<u64>(), 1) };
    });
    r.check(
        "a store through null ends the child with SIGSEGV",
        died_of(write_status, SIGSEGV),
    );
    match log_text() {
        None => r.check(
            "and the error column says user, write, not present (6)",
            true,
        ),
        Some(log) => {
            let name = String::from_utf8_lossy(CHILD_NAME.split(|b| *b == 0).next().unwrap_or(b""))
                .into_owned();
            let named = alloc_format(&name, write_pid);
            #[cfg(target_arch = "x86_64")]
            r.check(
                "and the error column says user, write, not present (6)",
                has_line(&log, &[&named, "segfault at ", " error 6"]),
            );
            #[cfg(not(target_arch = "x86_64"))]
            r.check(
                "and a store carries a different word than a read did",
                has_line(&log, &[&named, "segfault at ", " error "]),
            );
        }
    }

    let _ = host_run();
    r.finish();
}

/// `comm[tid]`, the prefix both of Linux's lines carry.
fn alloc_format(name: &str, tid: i32) -> String {
    format!("{name}[{tid}]")
}
