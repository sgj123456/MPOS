//! t-rt-tgsigqueueinfo - `rt_tgsigqueueinfo(2)`: queue a signal with a payload
//! at **one thread**.
//!
//! The sibling of `rt_sigqueueinfo` (129), which names a *process* and lets the
//! group pick a thread that may take the copy. It was undeclared, so the
//! dispatch answered `ENOSYS` - and a program that wants one specific thread to
//! hear a payload has no other door.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{tgsq,tgsq2,tgsq3,tgsq4,
//! tgsq5}.c`) before any claim:
//!
//! | what | Linux |
//! |---|---|
//! | `(tgid, tid)` naming one thread | served, and the payload arrives on it |
//! | `tgid` from another group than `tid` | **`ESRCH`** - the pair must agree |
//! | `tgid <= 0` or `tid <= 0` | `EINVAL` (malformed, not missing) |
//! | a positive number naming nothing | `ESRCH` |
//! | `sig == 0` | **`0`, queues nothing** (an existence probe) |
//! | `sig > 64` | `EINVAL` |
//! | `uinfo == NULL` | `EFAULT` |
//! | `si_signo` in the record ≠ `sig` | the **argument** wins |
//! | any `si_code` value | accepted and copied verbatim |
//!
//! **The pair agreeing is the claim that separates this door from its
//! sibling**: `rt_sigqueueinfo` takes one number and asks whether it names a
//! process; this one takes two and must find they name one thread. A tgid and a
//! tid from different groups is `ESRCH` even though both are live.
//!
//! `SIGUSR1` is blocked for the whole program, because a queued signal that is
//! not blocked ends the process - the first probe attempt died of exactly that
//! (`exit 138`). The drain at the end is what proves the payload landed.
//!
//! Numbers from libc's table: `rustix` binds no `sigqueueinfo` at all.

use mpos_user_tests::Report;

/// The record's layout, which is the ABI: `si_signo`, `si_errno`, `si_code`,
/// the sender's pid and uid, then the value.
const SIGINFO_BYTES: usize = 128;

fn tid() -> i32 {
    // SAFETY: `gettid` takes no argument and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

/// A `struct siginfo_t` the call can read: `si_signo`, `si_code = SI_QUEUE`,
/// and a value. Written as bytes so the offsets are the ABI's rather than a
/// Rust struct's.
fn record(sig: i32, code: i32, value: i32) -> [u8; SIGINFO_BYTES] {
    let mut b = [0u8; SIGINFO_BYTES];
    b[0..4].copy_from_slice(&sig.to_ne_bytes());
    b[8..12].copy_from_slice(&code.to_ne_bytes());
    b[24..28].copy_from_slice(&value.to_ne_bytes());
    b
}

/// `rt_tgsigqueueinfo(tgid, tid, sig, uinfo)` - the words passed **sign
/// extended**, because a `-1i32` handed to a `usize` argument becomes
/// `0xFFFFFFFF` and the kernel reads it as a huge positive pid.
fn tgsq(tgid: i32, tid: i32, sig: i32, uinfo: usize) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_rt_tgsigqueueinfo,
            tgid as isize,
            tid as isize,
            sig as isize,
            uinfo,
        ) as isize
    }
}

fn block_usr1() {
    // SAFETY: a stack `sigset_t` this function owns, and `sigprocmask` only
    // reads it.
    unsafe {
        let mut ss: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut ss);
        libc::sigaddset(&mut ss, libc::SIGUSR1);
        libc::sigprocmask(libc::SIG_BLOCK, &ss, core::ptr::null_mut());
    }
}

fn main() {
    println!("t-rt-tgsigqueueinfo");
    let mut r = Report::new();

    // Blocked before anything is queued: an unblocked signal would end the
    // process and every later claim with it.
    block_usr1();
    let me = unsafe { libc::getpid() };
    let my_tid = tid();
    let rec = record(libc::SIGUSR1, -1, 42);
    let rec_ptr = rec.as_ptr() as usize;

    // === the served shape ==================================================
    r.check(
        &format!(
            "(self tgid, self tid) is served ({})",
            tgsq(me, my_tid, libc::SIGUSR1, rec_ptr)
        ),
        tgsq(me, my_tid, libc::SIGUSR1, rec_ptr) == 0,
    );

    // === the pair must name one thread =====================================
    //
    // Both numbers are live - `me` is this process, `my_tid` is this thread -
    // and the call still refuses them together with a *different* tgid, which
    // is the claim the sibling door has no analogue for.
    r.check(
        "a tgid from another group than the tid is ESRCH",
        refused(tgsq(1, my_tid, libc::SIGUSR1, rec_ptr), libc::ESRCH),
    );
    // And a tid that names nothing, beside the right tgid.
    r.check(
        "a tid naming nothing is ESRCH",
        refused(tgsq(me, 99999, libc::SIGUSR1, rec_ptr), libc::ESRCH),
    );

    // === non-positive numbers are malformed, not missing ===================
    for bad in [0i32, -1] {
        r.check(
            &format!("tgid={bad} is EINVAL"),
            refused(tgsq(bad, my_tid, libc::SIGUSR1, rec_ptr), libc::EINVAL),
        );
        r.check(
            &format!("tid={bad} is EINVAL"),
            refused(tgsq(me, bad, libc::SIGUSR1, rec_ptr), libc::EINVAL),
        );
    }

    // === signal and record validation ======================================
    r.check(
        "a signal number past 64 is EINVAL",
        refused(tgsq(me, my_tid, 65, rec_ptr), libc::EINVAL),
    );
    r.check(
        "a NULL record is EFAULT",
        refused(tgsq(me, my_tid, libc::SIGUSR1, 0), libc::EFAULT),
    );
    // Signal 0 is an existence probe: the pair resolves and nothing is queued.
    r.check(
        "sig == 0 is served and queues nothing",
        tgsq(me, my_tid, 0, rec_ptr) == 0,
    );

    // === the payload arrives, and the argument's signal wins ===============
    //
    // `record()` writes `si_signo = SIGUSR1` above, so this proves the value
    // travels; the mismatch case then writes a *different* `si_signo` and
    // checks the argument's signal is what gets delivered.
    let mut got: libc::siginfo_t = unsafe { core::mem::zeroed() };
    // SAFETY: a `sigset_t` and a `siginfo_t` this test owns; the timeout is
    // NULL, so the call waits for a signal that has already been queued.
    unsafe {
        let mut ss: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut ss);
        libc::sigaddset(&mut ss, libc::SIGUSR1);
        let n = libc::syscall(
            libc::SYS_rt_sigtimedwait,
            &ss,
            &mut got,
            core::ptr::null::<libc::timespec>(),
            8usize,
        );
        r.check(
            &format!("the queued signal was taken by the named thread's group ({n})"),
            n == libc::SIGUSR1 as i64,
        );
        r.check(
            &format!("and it carries SI_QUEUE ({})", got.si_code),
            got.si_code == -1,
        );
    }
    // A differing `si_signo` in the buffer is **overridden** by the argument.
    let mismatch = record(libc::SIGUSR2, -1, 7);
    r.check(
        "a record whose si_signo disagrees is served",
        tgsq(me, my_tid, libc::SIGUSR1, mismatch.as_ptr() as usize) == 0,
    );
    // SAFETY: as above; the request was for SIGUSR1, so that is what must come
    // out even though the buffer said SIGUSR2.
    unsafe {
        let mut ss: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut ss);
        libc::sigaddset(&mut ss, libc::SIGUSR1);
        let n = libc::syscall(
            libc::SYS_rt_sigtimedwait,
            &ss,
            &mut got,
            core::ptr::null::<libc::timespec>(),
            8usize,
        );
        r.check(
            &format!("and the ARGUMENT's signal is what arrives ({n})"),
            n == libc::SIGUSR1 as i64,
        );
    }

    // The unblocked drain, so this program does not leave SIGUSR1 pending for
    // whatever runs next in the lane.
    // SAFETY: the mask this program set in `block_usr1`.
    unsafe {
        let mut ss: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut ss);
        libc::sigprocmask(libc::SIG_UNBLOCK, &ss, core::ptr::null_mut());
    }
    r.finish()
}
