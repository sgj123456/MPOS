//! t-syslog - the kernel log's own door, klogctl's numbers.
//!
//! Pinned here, per syslog(2): the counter types answer the unread byte
//! count and the buffer's capacity, the read-all type copies the tail of
//! the log without consuming it, a read with a null buffer is EINVAL
//! before anything else, the console-level type validates its level
//! against 1..=8, the bookkeeping types are accepted, and a type outside
//! the eleven is EINVAL. The destructive read (type 2) waits until the
//! log is nonempty and is not exercised here: the ring is userland's to
//! fill, and a test that parked in it would outlive the machine.
//!
//! # The migration, and the one divergence the host measured
//!
//! The door is the raw door - `libc::syscall(SYS_syslog, ..)` - because
//! every answer is the *number* the door itself gives: counts, zeros and
//! errnos, with no wrapper between the claim and the kernel.
//! `SYS_syslog` is `libc`'s own per-machine number (103 here, 116 on the
//! second machine), the last no-std literal this program carried.
//!
//! **What the host measured is a gate this kernel does not have.** The
//! probe (`.workbuddy/probe_syslog.c`) cannot get past Linux's privilege
//! check: this sandbox's real Linux answers `EPERM` for *every* type -
//! the counters and the bookkeeping types included - because
//! `dmesg_restrict` is on and a container's root does not hold
//! `CAP_SYSLOG` in the initial user namespace. The gate comes *first*:
//! even an unknown type (99), which this kernel answers `EINVAL`, gets
//! `EPERM` from the host, because Linux validates permission before it
//! validates the type. None of the log's answers are measurable here,
//! so under `MPOS_ON_HOST=1` every check accepts the `EPERM` the host
//! actually gave and says so; the gate keeps pinning this kernel, whose
//! own answers are the ones the claims were written against. The one
//! number that is mpos's by design - `SIZE_BUFFER`'s 16 KiB - is stated
//! as such: a privileged host would answer its own (dynamic) ring size,
//! so a host run accepts what the host said and prints it.
//!
//! The rest is mechanical: `EINVAL`/`EPERM` come from `libc`, and the
//! buffer is this program's own 512 bytes - the read-all type never
//! consumes, so the same buffer may be asked twice.
//!
//! **The gate this kernel now has, too.** Since the credential model's
//! wiring (`t-caps` is the program that walks it), mpos asks `CAP_SYSLOG`
//! at the same place Linux does - before the type, so its unknown-type
//! `EINVAL` sits behind the gate exactly as the host's `EPERM` does - and
//! the root caller the sweep runs as holds it, so every answer pinned here
//! is unchanged. The host run's divergence is now two gates agreeing on
//! placement and differing only on who holds the key.

use mpos_user_tests::{Report, host_run, in_user_namespace};

/// `syslog(type, buf, len)` through the raw door: the byte count, `0`,
/// or `-errno` on failure - the syscall's own answers.
fn syslog(kind: i32, buf: *mut u8, len: usize) -> isize {
    // SAFETY: `buf` is either null (the non-readers) or this program's
    // own buffer, which outlives the call; the kernel writes only inside
    // the length it is handed.
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

/// The host's privilege gate: this sandbox's Linux answers `EPERM`
/// before it looks at anything else (`.workbuddy/probe_syslog.c`), so a
/// host run cannot measure the log at all. Accepted under `MPOS_ON_HOST`
/// only - the gate keeps pinning this kernel's answers.
fn privileged_host(answer: isize) -> bool {
    host_run() && answer == -(libc::EPERM as isize)
}

fn main() {
    println!("t-syslog");
    let mut r = Report::new();

    // The counters: the unread bytes are non-negative, and the buffer is
    // the capacity this kernel keeps, 16 KiB - a number mpos designed,
    // which a privileged host would replace with its own ring size.
    let unread = syslog(9, core::ptr::null_mut(), 0);
    r.check(
        "SIZE_UNREAD answers a count",
        unread >= 0 || privileged_host(unread),
    );
    let cap = syslog(10, core::ptr::null_mut(), 0);
    if host_run() && cap != 16384 {
        println!("note: the host answers SIZE_BUFFER = {cap} (its own ring, or the EPERM gate)");
        r.check(
            "SIZE_BUFFER answers the capacity (host run: the host's own answer accepted)",
            true,
        );
    } else {
        r.check("SIZE_BUFFER answers the capacity, 16 KiB", cap == 16384);
    }

    // The read-all: the tail of the log, non-destructively, so the same
    // question asked twice has the same answer.
    let mut log = [0u8; 512];
    let got = syslog(3, log.as_mut_ptr(), 512);
    r.check("READ_ALL answers a count", got >= 0 || privileged_host(got));
    let again = syslog(3, log.as_mut_ptr(), 512);
    r.check("READ_ALL did not consume the log", again == got);

    // The validations: a null buffer for the readers is EINVAL before
    // anything else - and on the host the privilege gate answers EPERM
    // even here, permission coming before every other check.
    let null_read_all = syslog(3, core::ptr::null_mut(), 512);
    r.check(
        "READ_ALL with a null buffer answers EINVAL",
        null_read_all == -(libc::EINVAL as isize) || privileged_host(null_read_all),
    );
    let null_read = syslog(2, core::ptr::null_mut(), 512);
    r.check(
        "READ with a null buffer answers EINVAL",
        null_read == -(libc::EINVAL as isize) || privileged_host(null_read),
    );
    // The console-level type checks its level against 1..=8.
    let level0 = syslog(8, core::ptr::null_mut(), 0);
    r.check(
        "CONSOLE_LEVEL with level 0 answers EINVAL",
        level0 == -(libc::EINVAL as isize) || privileged_host(level0),
    );
    let level9 = syslog(8, core::ptr::null_mut(), 9);
    r.check(
        "CONSOLE_LEVEL with level 9 answers EINVAL",
        level9 == -(libc::EINVAL as isize) || privileged_host(level9),
    );
    let level5 = syslog(8, core::ptr::null_mut(), 5);
    r.check(
        "CONSOLE_LEVEL with level 5 succeeds",
        level5 == 0 || privileged_host(level5),
    );

    // The bookkeeping types: open, close, console on and off, and the
    // clear, which answers success and empties the tail.
    let close = syslog(0, core::ptr::null_mut(), 0);
    r.check("CLOSE succeeds", close == 0 || privileged_host(close));
    let open = syslog(1, core::ptr::null_mut(), 0);
    r.check("OPEN succeeds", open == 0 || privileged_host(open));
    let off = syslog(6, core::ptr::null_mut(), 0);
    r.check("CONSOLE_OFF succeeds", off == 0 || privileged_host(off));
    let on = syslog(7, core::ptr::null_mut(), 0);
    r.check("CONSOLE_ON succeeds", on == 0 || privileged_host(on));
    let clear = syslog(5, core::ptr::null_mut(), 0);
    r.check("CLEAR succeeds", clear == 0 || privileged_host(clear));
    let tail = syslog(3, log.as_mut_ptr(), 512);
    r.check(
        "the tail after a clear is empty",
        tail == 0 || privileged_host(tail),
    );

    // A type outside the eleven: EINVAL - where the host, whose
    // permission check runs first, answers EPERM even for this.
    let unknown = syslog(99, core::ptr::null_mut(), 0);
    r.check(
        "an unknown type answers EINVAL",
        unknown == -(libc::EINVAL as isize) || privileged_host(unknown),
    );

    // ---- the namespace the capability belongs to ---------------------------
    //
    // The gate is `capable(CAP_SYSLOG)` - the **initial** user namespace - and
    // not the caller's own. The two differ for a caller that has entered a user
    // namespace of its own, which holds the whole capability set *in it* (the
    // rung's measured grant, §6.3) and is refused the ring anyway. Measured on
    // a current host first (`.workbuddy/probe_capable_scope.sh`), where the
    // answer is `EPERM` both outside and inside - the initial namespace's own
    // caller holds no `CAP_SYSLOG` in this sandbox either, so the *pair* is
    // what discriminates: inside the namespace the capability is demonstrably
    // held (the same child may `unshare(CLONE_NEWNS)`) and the refusal stands.
    let size = in_user_namespace(|| syslog(10, core::ptr::null_mut(), 0) as i32);
    r.check(
        &format!("the ring's size inside a user namespace is EPERM (got {size:?})"),
        size == Some(-libc::EPERM),
    );
    // And the same refusal for a type that is *not* a type: the capability is
    // asked before the type is, which is the ordering the section above pins
    // from the privileged side.
    let unknown = in_user_namespace(|| syslog(99, core::ptr::null_mut(), 0) as i32);
    r.check(
        &format!("and so is an unknown type there, before EINVAL (got {unknown:?})"),
        unknown == Some(-libc::EPERM),
    );

    r.finish()
}
