//! t-prctl - the process-control call's option table.
//!
//! Pinned here, per prctl(2) and `linux/prctl.h`: the name pair round-trips -
//! `PR_SET_NAME` stores the caller's bytes and `PR_GET_NAME` reads them back,
//! NUL-terminated inside the sixteen-byte `TASK_COMM_LEN` slot - and a name that
//! fills that slot whole is *truncated and accepted*, not refused; a fresh
//! process is unconfined (`PR_GET_SECCOMP` answers 0); entering the BPF filter
//! mode with no program is refused with `EFAULT` before anything is installed;
//! and an option the kernel does not model is refused with `EINVAL`, the way
//! Linux's `prctl` default arm answers a number it does not know.
//!
//! The four options whose state the kernel keeps are pinned shape by shape:
//! `PR_SET_DUMPABLE`/`PR_GET_DUMPABLE` (0 and 1 accepted, 2 and 3 `EINVAL`, and a
//! refused set leaves the flag alone), `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` (0 and
//! 1 only, and the getter reads back what the setter stored), and
//! `PR_CAPBSET_READ`/`PR_CAPBSET_DROP` (1 or 0 for 0..=40, `EINVAL` past it, and a
//! drop visible to the next read).
//!
//! # The doors, and why this file has two
//!
//! `rustix` binds three of the option groups - `thread::{name, set_name}`,
//! `thread::{get_keep_capabilities, set_keep_capabilities}` and
//! `thread::secure_computing_mode` - so those go through it, and the number each
//! sends is the crate's own table rather than this file's memory.
//!
//! The rest of the table is spelled through the raw door,
//! `libc::syscall(libc::SYS_prctl, ..)` with the option numbers taken from
//! `libc::PR_*`, and that is not a shortcut but the point: the *option number*
//! is half of every claim below (7 is the keepcaps getter, 9 is not; 40 is the
//! last capability and 41 is not an option operand at all), and `rustix` binds
//! neither `PR_*_DUMPABLE` nor `PR_CAPBSET_*`, so a program that wants those
//! cannot reach them through it. `libc`'s `PR_*` table is the maintained
//! per-arch uapi mirror, which is exactly what writing the numbers down here
//! would replace with a guess - the rule `t-process-vm-libc` states for the
//! calls `rustix` does not bind at all.
//!
//! # What the migration found
//!
//! `PR_GET_KEEPCAPS` is **7** in `linux/prctl.h`; 9 is `PR_GET_FPEMU`. This
//! kernel had written the pair down as 8/9 - the getter as the setter plus one,
//! the shape a reader expects - and the no-std suite's `lin.rs` carried the same
//! 9, so the two agreed on a number no Linux program sends and the divergence hid
//! behind their agreement. `rustix`'s `get_keep_capabilities` sends the real 7,
//! so the two `get_keep_capabilities` claims below failed against the unfixed
//! kernel; the fix is one constant in `syscall::linux::abi`. The old wrong
//! identity is named where it used to be, so the counterfactual is pinned rather
//! than lost.
//!
//! Two claims the no-std version made are retired rather than translated, because
//! they were about its own scaffolding: the 0xAA-seeded buffer that proved the
//! read-back was the kernel's write is not needed when `rustix` allocates the
//! buffer, and "the slot ends in NUL" is now made where the slot itself is the
//! subject - the raw pair below, which sets and reads the sixteen bytes through
//! `libc::PR_*` so both halves are one builder. The strict-mode latch is
//! deliberately *not* tested here (the process dies on its next non-allowlisted
//! syscall), and that destructive path belongs to `t-posix`.

use mpos_user_tests::{Report, host_run};
use rustix::io::Errno;
use rustix::thread::{
    SecureComputingMode, get_keep_capabilities, name, secure_computing_mode, set_keep_capabilities,
    set_name,
};
use std::ffi::CString;

/// `CAP_LAST_CAP` is `CAP_CHECKPOINT_RESTORE` (40). `libc` carries the individual
/// capability numbers but not this one, so it is written here - and the number
/// *is* the claim: 40 is a capability and 41 is not.
const CAP_LAST_CAP: libc::c_int = 40;

/// `TASK_COMM_LEN`: the fixed slot the two name options speak of.
const NAME_LEN: usize = 16;

/// `prctl(2)` through the raw door, with the option number coming from the
/// caller's `libc::PR_*` rather than from a constant this file owns.
fn prctl(op: libc::c_int, a2: usize, a3: usize, a4: usize, a5: usize) -> Result<isize, Errno> {
    let ret = unsafe { libc::syscall(libc::SYS_prctl, op, a2, a3, a4, a5) };
    if ret == -1 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(ret as isize)
    }
}

/// A claim about an answer that must be a specific success value.
fn ok_is(r: &mut Report, label: &str, got: Result<isize, Errno>, want: isize) {
    match got {
        Ok(v) if v == want => r.check(label, true),
        Ok(v) => r.check(&format!("{label} (got Ok({v}), want {want})"), false),
        Err(e) => r.check(&format!("{label} (got {e}, want {want})"), false),
    }
}

/// A claim about a refusal, spelled with the answer when it is the wrong one.
fn err_is(r: &mut Report, label: &str, got: Result<isize, Errno>, want: Errno) {
    match got {
        Err(e) if e == want => r.check(label, true),
        Err(e) => r.check(&format!("{label} (got {e}, want {want})"), false),
        Ok(v) => r.check(&format!("{label} (got Ok({v}), want {want})"), false),
    }
}

fn main() {
    let mut r = Report::new();

    // === the name pair, through rustix =======================================
    // The round-trip is the behaviour this program exists to pin: `PR_SET_NAME`
    // used to fall through a `_ => 0` arm - accepted and dropped - and
    // `PR_GET_NAME` answered sixteen zeroes, so the read-back would have found no
    // name.
    let short = CString::new("renamed").expect("no interior NUL");
    r.check("PR_SET_NAME accepts a name", set_name(&short).is_ok());
    match name() {
        Ok(got) => {
            r.check("PR_GET_NAME succeeds", true);
            r.check(
                "the name round-trips: PR_GET_NAME reads back what PR_SET_NAME stored",
                got.as_bytes() == b"renamed",
            );
        }
        Err(e) => {
            r.check("PR_GET_NAME succeeds", false);
            r.check(
                &format!("the name round-trips (PR_GET_NAME failed: {e})"),
                false,
            );
        }
    }

    // A name that fills the whole slot is truncated and accepted, not refused.
    // Measured on Linux: sixteen bytes with no NUL inside `TASK_COMM_LEN` answer
    // `0`, and `PR_GET_NAME` then reads back fifteen. `rustix`'s `set_name` takes
    // a `CStr`, so the seventeenth byte is the terminator the caller must supply;
    // the kernel still reads only the sixteen it owns.
    let long = CString::new([b'x'; NAME_LEN].as_slice()).expect("no interior NUL");
    r.check(
        "PR_SET_NAME truncates a name that fills the slot, it does not refuse it",
        set_name(&long).is_ok(),
    );
    match name() {
        Ok(got) => r.check(
            "fifteen characters survive the cut",
            got.as_bytes() == [b'x'; NAME_LEN - 1],
        ),
        Err(e) => r.check(&format!("fifteen characters survive the cut ({e})"), false),
    }

    // The slot itself, through the raw pair so both halves are one builder:
    // `PR_GET_NAME` writes exactly `NAME_LEN` bytes and the sixteenth is NUL -
    // which is the truncation seen from the buffer rather than from a `CString`.
    let mut slot = [0xAAu8; NAME_LEN];
    ok_is(
        &mut r,
        "the raw PR_SET_NAME accepts a short name",
        prctl(libc::PR_SET_NAME, short.as_ptr() as usize, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "the raw PR_GET_NAME succeeds",
        prctl(libc::PR_GET_NAME, slot.as_mut_ptr() as usize, 0, 0, 0),
        0,
    );
    r.check(
        "the slot is sixteen bytes and the byte after the name is NUL",
        slot[..7] == *b"renamed" && slot[7] == 0,
    );

    // === seccomp, the answers that leave the caller alive ====================
    // `PR_GET_SECCOMP`: a fresh process is unconfined. The one door here that
    // needs a caveat in a host run: a harness that supervises its children with a
    // seccomp filter of its own makes `PR_GET_SECCOMP` answer 2 (`.workbuddy`'s
    // probe host measured exactly that), which is the harness's confinement and
    // not the kernel's answer to this claim. The claim is the guest's; a host run
    // prints what it found instead.
    match secure_computing_mode() {
        Ok(SecureComputingMode::Disabled) => {
            r.check("PR_GET_SECCOMP answers 0 while unconfined", true)
        }
        Ok(other) => {
            if host_run() {
                println!(
                    "note: this host run is under a supervisor's own seccomp filter, \
                     so PR_GET_SECCOMP answers {other:?}"
                );
            }
            r.check("PR_GET_SECCOMP answers 0 while unconfined", host_run());
        }
        Err(e) => r.check(
            &format!("PR_GET_SECCOMP answers 0 while unconfined ({e})"),
            false,
        ),
    }

    // `PR_SET_SECCOMP` with the filter mode and no program: the install path
    // reads the program *before* it decides anything, which is what makes this
    // `EFAULT` rather than a success that confined nobody. (A well-formed program
    // is `t-seccomp`'s subject.) Spelled raw: `rustix`'s
    // `set_secure_computing_mode` passes no program pointer at all, which is the
    // shape the claim is about not having.
    err_is(
        &mut r,
        "PR_SET_SECCOMP with the filter mode and no program answers EFAULT",
        prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as usize,
            0,
            0,
            0,
        ),
        Errno::FAULT,
    );

    // === the dumpable flag ===================================================
    ok_is(
        &mut r,
        "PR_GET_DUMPABLE starts at SUID_DUMP_USER (1)",
        prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0),
        1,
    );
    ok_is(
        &mut r,
        "PR_SET_DUMPABLE(0) is accepted",
        prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "and PR_GET_DUMPABLE reads it back",
        prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "PR_SET_DUMPABLE(1) is accepted",
        prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "and reads back as SUID_DUMP_USER",
        prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0),
        1,
    );
    // 2 is the kernel's own SUID_DUMP_ROOT and is not settable through this
    // door: measured, `PR_SET_DUMPABLE(2)`, `(3)` and `(-1)` are all `EINVAL`.
    err_is(
        &mut r,
        "PR_SET_DUMPABLE(2) is EINVAL - SUID_DUMP_ROOT is not settable",
        prctl(libc::PR_SET_DUMPABLE, 2, 0, 0, 0),
        Errno::INVAL,
    );
    err_is(
        &mut r,
        "PR_SET_DUMPABLE(3) is EINVAL",
        prctl(libc::PR_SET_DUMPABLE, 3, 0, 0, 0),
        Errno::INVAL,
    );
    ok_is(
        &mut r,
        "and a refused set left the flag alone",
        prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0),
        1,
    );

    // === KEEPCAPS, and the number this batch changed =========================
    // The getter is `rustix`'s, so it sends 7 - the number `linux/prctl.h` gives
    // `PR_GET_KEEPCAPS`. These two are the claims that failed against the kernel
    // whose table said 9.
    match get_keep_capabilities() {
        Ok(false) => r.check("PR_GET_KEEPCAPS starts clear", true),
        Ok(true) => r.check("PR_GET_KEEPCAPS starts clear (got set)", false),
        Err(e) => r.check(&format!("PR_GET_KEEPCAPS starts clear ({e})"), false),
    }
    r.check(
        "PR_SET_KEEPCAPS(1) is accepted",
        set_keep_capabilities(true).is_ok(),
    );
    match get_keep_capabilities() {
        Ok(true) => r.check("and reads back set", true),
        Ok(false) => r.check("and reads back set (still clear)", false),
        Err(e) => r.check(&format!("and reads back set ({e})"), false),
    }
    // Measured: the accepted values are 0 and 1; 2 is EINVAL. Raw, so the claim
    // names the number 8 as the setter's own.
    err_is(
        &mut r,
        "PR_SET_KEEPCAPS(2) is EINVAL",
        prctl(libc::PR_SET_KEEPCAPS, 2, 0, 0, 0),
        Errno::INVAL,
    );
    r.check(
        "PR_SET_KEEPCAPS(0) clears it again",
        set_keep_capabilities(false).is_ok(),
    );
    // The counterfactual, named: 9 is where the getter used to be, and it is
    // `PR_GET_FPEMU` - an option this machine does not serve, so Linux answers
    // `EINVAL` too (measured). The claim holds on both machines.
    err_is(
        &mut r,
        "option 9 is not the keepcaps getter - it is EINVAL",
        prctl(9, 0, 0, 0, 0),
        Errno::INVAL,
    );
    ok_is(
        &mut r,
        "and option 7 is - it answers the flag, not EINVAL",
        prctl(libc::PR_GET_KEEPCAPS, 0, 0, 0, 0),
        0,
    );

    // === the capability bounding set =========================================
    ok_is(
        &mut r,
        "PR_CAPBSET_READ(CAP_CHOWN) is 1 to start",
        prctl(libc::PR_CAPBSET_READ, 0, 0, 0, 0),
        1,
    );
    ok_is(
        &mut r,
        "PR_CAPBSET_READ(CAP_LAST_CAP) is 1",
        prctl(libc::PR_CAPBSET_READ, CAP_LAST_CAP as usize, 0, 0, 0),
        1,
    );
    err_is(
        &mut r,
        "PR_CAPBSET_READ past the last capability is EINVAL",
        prctl(libc::PR_CAPBSET_READ, (CAP_LAST_CAP + 1) as usize, 0, 0, 0),
        Errno::INVAL,
    );
    ok_is(
        &mut r,
        "PR_CAPBSET_DROP(CAP_CHOWN) is accepted for root",
        prctl(libc::PR_CAPBSET_DROP, 0, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "and the next read shows it gone",
        prctl(libc::PR_CAPBSET_READ, 0, 0, 0, 0),
        0,
    );
    // Measured on Linux: the capability is asked *before* the number, so a
    // number past the last one is `EINVAL` for root and `EPERM` for a caller
    // without `CAP_SETPCAP`.
    err_is(
        &mut r,
        "PR_CAPBSET_DROP past the last capability is EINVAL",
        prctl(libc::PR_CAPBSET_DROP, (CAP_LAST_CAP + 1) as usize, 0, 0, 0),
        Errno::INVAL,
    );

    // === an option the handler does not model ================================
    // Refused the way Linux's `prctl` default arm answers a number it does not
    // know - not the silent `0` this kernel used to hand back for every
    // unmodelled option.
    err_is(
        &mut r,
        "an unknown option is refused with EINVAL",
        prctl(0x7fff, 0, 0, 0, 0),
        Errno::INVAL,
    );

    r.finish()
}
