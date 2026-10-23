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
//! drop visible to the next read) - and `PR_SET_PDEATHSIG`/`PR_GET_PDEATHSIG`,
//! whose state the **exit path** reads: the signal is delivered to a child when
//! the task it named as its parent dies, which is why the claims below include a
//! real delivery and not only the round-trip. Measured on a current host
//! (`.workbuddy/probe_pdeathsig.c`): 0 clears, anything up to `_NSIG` is
//! accepted (`SIGKILL` and `SIGSTOP` included), an out-of-range or negative
//! value is `EINVAL` with the previous one kept, a null getter pointer is
//! `EFAULT`, **`fork` clears it** and **`exec` keeps it**.
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
/// The descriptor the parent-death handler writes its answer to: stored where a
/// signal handler can reach it (no lock, no allocation), and set before the fork
/// whose child will write through it.
static REPORT_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Write one word to [`REPORT_FD`] - the only thing a signal handler here does,
/// which is why it is a raw `write`.
fn say_word(word: &[u8]) {
    let fd = REPORT_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, word.as_ptr().cast(), word.len()) };
    }
}

/// The parent-death handler: report and return, so the claim reads the report
/// rather than the task's death.
extern "C" fn on_parent_death(_sig: libc::c_int) {
    say_word(b"sig\n");
}

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

    // === the parent-death signal ============================================
    // A child that asks for one is sent it when the task it names as its parent
    // dies. The delivery is the claim that matters - a kernel that only stored
    // the number would pass the round-trip and do nothing - and it is observed
    // through a **handler**, not through the wait status: the orphan is
    // reparented to init when its parent dies, so this process is no longer the
    // one that reaps it.
    let mut value = 0xffff_ffffu32;
    err_is(
        &mut r,
        "PR_GET_PDEATHSIG with a null pointer is EFAULT",
        prctl(libc::PR_GET_PDEATHSIG, 0, 0, 0, 0),
        Errno::FAULT,
    );
    ok_is(
        &mut r,
        "PR_SET_PDEATHSIG(SIGUSR1) is accepted",
        prctl(libc::PR_SET_PDEATHSIG, libc::SIGUSR1 as usize, 0, 0, 0),
        0,
    );
    // The getter answers **0** and writes the signal through the pointer -
    // measured; an implementation that returned the number itself would fail
    // this claim and pass the next one.
    ok_is(
        &mut r,
        "PR_GET_PDEATHSIG answers 0 and the value arrives through the pointer",
        prctl(
            libc::PR_GET_PDEATHSIG,
            &mut value as *mut u32 as usize,
            0,
            0,
            0,
        ),
        0,
    );
    ok_is(
        &mut r,
        "the value the getter wrote is the signal",
        Ok(value as isize),
        libc::SIGUSR1 as isize,
    );
    err_is(
        &mut r,
        "a signal number past _NSIG is EINVAL",
        prctl(libc::PR_SET_PDEATHSIG, 100, 0, 0, 0),
        Errno::INVAL,
    );
    value = 0xffff_ffffu32;
    let _ = prctl(
        libc::PR_GET_PDEATHSIG,
        &mut value as *mut u32 as usize,
        0,
        0,
        0,
    );
    ok_is(
        &mut r,
        "and a refused set leaves the previous signal in place",
        Ok(value as isize),
        libc::SIGUSR1 as isize,
    );
    ok_is(
        &mut r,
        "PR_SET_PDEATHSIG(0) clears it",
        prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0),
        0,
    );
    value = 0xffff_ffffu32;
    let _ = prctl(
        libc::PR_GET_PDEATHSIG,
        &mut value as *mut u32 as usize,
        0,
        0,
        0,
    );
    ok_is(&mut r, "and the getter reads 0", Ok(value as isize), 0);

    // `fork` clears it, whatever the creator asked for.
    ok_is(
        &mut r,
        "the creator asks for one before forking",
        prctl(libc::PR_SET_PDEATHSIG, libc::SIGUSR2 as usize, 0, 0, 0),
        0,
    );
    let forked = unsafe { libc::fork() };
    if forked == 0 {
        let mut v = 0xffff_ffffu32;
        let rc = prctl(libc::PR_GET_PDEATHSIG, &mut v as *mut u32 as usize, 0, 0, 0);
        unsafe { libc::_exit(if rc.is_ok() && v == 0 { 0 } else { 1 }) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(forked, &mut status, 0) };
    r.check(
        "a fork child starts with no parent-death signal",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );
    let _ = prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0);

    // ...and the delivery itself. The middle process forks the victim, **waits
    // for it to be armed** and only then exits - a pipe is the ordering, not a
    // sleep: without it the middle can die before the victim has asked, and the
    // claim would read the race rather than the kernel. The victim reports
    // through a handler, so what it dies of never has to be guessed at.
    let armed = rustix::pipe::pipe();
    let say = rustix::pipe::pipe();
    match (armed, say) {
        (Ok((armed_r, armed_w)), Ok((tail, head))) => {
            use std::os::fd::AsRawFd;
            REPORT_FD.store(head.as_raw_fd(), std::sync::atomic::Ordering::Relaxed);
            let middle = unsafe { libc::fork() };
            if middle == 0 {
                let victim = unsafe { libc::fork() };
                if victim == 0 {
                    unsafe {
                        drop(armed_r);
                        libc::signal(
                            libc::SIGUSR1,
                            on_parent_death as *const () as libc::sighandler_t,
                        );
                        let _ = prctl(libc::PR_SET_PDEATHSIG, libc::SIGUSR1 as usize, 0, 0, 0);
                        // "Armed": the parent may now die.
                        let _ = libc::write(armed_w.as_raw_fd(), b"a".as_ptr().cast(), 1);
                        drop(armed_w);
                        // Bounded: a kernel that never delivers lets this elapse
                        // and the claim reads "none" rather than hanging the gate.
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        say_word(b"none\n");
                        libc::_exit(0);
                    }
                }
                unsafe {
                    drop(armed_w);
                    drop(head);
                    let mut one = [0u8; 1];
                    let _ = rustix::io::read(&armed_r, &mut one);
                    libc::_exit(0);
                }
            }
            drop(armed_w);
            drop(armed_r);
            drop(head);
            let mut said = [0u8; 8];
            let n = rustix::io::read(&tail, &mut said).unwrap_or(0);
            r.check(
                "a child whose parent died is sent the signal it asked for",
                said[..n].starts_with(b"sig"),
            );
            let mut status = 0;
            unsafe { libc::waitpid(middle, &mut status, 0) };
        }
        _ => r.check("the parent-death report pipes are made", false),
    }

    // === no new privileges ===================================================
    // One-way, and the flag a confinement leans on: with it set, a caller that
    // is not root may install a seccomp filter (the pair is walked in
    // `t-seccomp`). Measured on a current host (`.workbuddy/probe_nnp.c`): the
    // setter takes exactly `(1, 0, 0, 0)` - `2`, a non-zero later argument and a
    // clearing `0` are `EINVAL` - and the getter refuses a call whose unused
    // arguments are not zero, which is why every call below spells all five out.
    err_is(
        &mut r,
        "PR_GET_NO_NEW_PRIVS refuses a non-zero argument",
        prctl(libc::PR_GET_NO_NEW_PRIVS, 1, 0, 0, 0),
        Errno::INVAL,
    );
    ok_is(
        &mut r,
        "PR_SET_NO_NEW_PRIVS(1) is accepted",
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
        0,
    );
    ok_is(
        &mut r,
        "and the getter reads 1 back",
        prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
        1,
    );
    err_is(
        &mut r,
        "a setter argument of 2 is EINVAL",
        prctl(libc::PR_SET_NO_NEW_PRIVS, 2, 0, 0, 0),
        Errno::INVAL,
    );
    err_is(
        &mut r,
        "a non-zero fourth argument is EINVAL",
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0),
        Errno::INVAL,
    );
    err_is(
        &mut r,
        "and it cannot be cleared: set(0) is EINVAL",
        prctl(libc::PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0),
        Errno::INVAL,
    );
    ok_is(
        &mut r,
        "setting it again is idempotent",
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
        0,
    );
    let forked = unsafe { libc::fork() };
    if forked == 0 {
        let rc = prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        unsafe { libc::_exit(if rc == Ok(1) { 0 } else { 1 }) };
    }
    let mut nnp_status = 0;
    unsafe { libc::waitpid(forked, &mut nnp_status, 0) };
    r.check(
        "a fork child inherits the promise",
        libc::WIFEXITED(nnp_status) && libc::WEXITSTATUS(nnp_status) == 0,
    );

    // === the child subreaper =================================================
    // Whether an orphaned *descendant* is reparented to this process rather than
    // to init. Measured on a current host (`.workbuddy/probe_subreaper.c`): the
    // setter takes any `int` and stores a boolean (`7` reads back as `1`), a
    // null getter pointer is `EFAULT`, and `fork` does **not** inherit it.
    let mut sub = 0xffff_ffffu32;
    ok_is(
        &mut r,
        "PR_GET_CHILD_SUBREAPER answers 0 on a fresh process",
        prctl(
            libc::PR_GET_CHILD_SUBREAPER,
            &mut sub as *mut u32 as usize,
            0,
            0,
            0,
        ),
        0,
    );
    ok_is(
        &mut r,
        "and writes 0 through the pointer",
        Ok(sub as isize),
        0,
    );
    err_is(
        &mut r,
        "PR_GET_CHILD_SUBREAPER with a null pointer is EFAULT",
        prctl(libc::PR_GET_CHILD_SUBREAPER, 0, 0, 0, 0),
        Errno::FAULT,
    );
    ok_is(
        &mut r,
        "PR_SET_CHILD_SUBREAPER(7) is accepted",
        prctl(libc::PR_SET_CHILD_SUBREAPER, 7, 0, 0, 0),
        0,
    );
    sub = 0xffff_ffffu32;
    let _ = prctl(
        libc::PR_GET_CHILD_SUBREAPER,
        &mut sub as *mut u32 as usize,
        0,
        0,
        0,
    );
    ok_is(
        &mut r,
        "and reads back as the boolean it stores",
        Ok(sub as isize),
        1,
    );
    let forked = unsafe { libc::fork() };
    if forked == 0 {
        let mut v = 0xffff_ffffu32;
        let rc = prctl(
            libc::PR_GET_CHILD_SUBREAPER,
            &mut v as *mut u32 as usize,
            0,
            0,
            0,
        );
        unsafe { libc::_exit(if rc.is_ok() && v == 0 { 0 } else { 1 }) };
    }
    let mut sub_status = 0;
    unsafe { libc::waitpid(forked, &mut sub_status, 0) };
    r.check(
        "a fork child is not a subreaper",
        libc::WIFEXITED(sub_status) && libc::WEXITSTATUS(sub_status) == 0,
    );

    // ...and the behaviour: a grandchild whose own parent died comes here, and
    // is this process's to reap. The middle process forks the grandchild and
    // exits; the grandchild waits for that to have happened (a bounded sleep -
    // the test has already reaped the middle by the time it reads) and reports
    // the parent it ended up with.
    let report = rustix::pipe::pipe();
    match report {
        Ok((tail, head)) => {
            use std::os::fd::AsRawFd;
            let middle = unsafe { libc::fork() };
            if middle == 0 {
                let grand = unsafe { libc::fork() };
                if grand == 0 {
                    drop(tail);
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let ppid = unsafe { libc::getppid() } as u32;
                    unsafe {
                        libc::write(head.as_raw_fd(), (&ppid as *const u32).cast(), 4);
                        libc::_exit(0);
                    }
                }
                unsafe { libc::_exit(0) };
            }
            drop(head);
            let mut st = 0;
            unsafe { libc::waitpid(middle, &mut st, 0) };
            let mut said = [0u8; 4];
            let n = rustix::io::read(&tail, &mut said).unwrap_or(0);
            let ppid = u32::from_ne_bytes(said);
            ok_is(
                &mut r,
                "the orphaned grandchild names this process as its parent",
                Ok(ppid as isize),
                unsafe { libc::getpid() } as isize,
            );
            r.check(
                "and it is this process's to reap, not init's",
                n == 4 && unsafe { libc::waitpid(-1, &mut st, 0) } > 0,
            );
        }
        Err(_) => r.check("the subreaper report pipe is made", false),
    }

    // Unset: the same shape goes to init, so there is nothing here to reap.
    ok_is(
        &mut r,
        "the flag is cleared again",
        prctl(libc::PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0),
        0,
    );
    let middle = unsafe { libc::fork() };
    if middle == 0 {
        let grand = unsafe { libc::fork() };
        if grand == 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            unsafe { libc::_exit(0) };
        }
        unsafe { libc::_exit(0) };
    }
    let mut st = 0;
    unsafe { libc::waitpid(middle, &mut st, 0) };
    std::thread::sleep(std::time::Duration::from_millis(250));
    let reaped = unsafe { libc::waitpid(-1, &mut st, 0) };
    r.check(
        "without the flag the orphan goes to init: there is nothing here to reap",
        reaped == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
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
