//! Linux's five buffered `sched_*` doors share one first guard, and it runs
//! **before** the pid is looked up.
//!
//! Measured on Linux 7.2.14 (`build/scratch/schedorder/matrix.c`, `edges.c`,
//! `ambig.c`, `sizes.c`, musl static, raw `syscall()` so no userspace wrapper
//! truncates the pid or pre-validates the buffer). The five doors are
//! `sched_getparam`, `sched_setparam`, `sched_setscheduler`, `sched_getattr`,
//! `sched_setattr`; the common guard is literally `if (!param || pid < 0)
//! return -EINVAL;`. That is why:
//!
//! ```text
//!                       NULL buffer        valid buffer        bad (0x1) ptr
//! self  (pid 0)          EINVAL (22)       success (0)         EFAULT (14)
//! nobody(pid 999999)     EINVAL (22)       ESRCH (3)           (door-specific)
//! negative (pid -1)      EINVAL (22)       EINVAL (22)         EINVAL (22)
//! ```
//!
//! The **NULL + nobody → EINVAL** row is the ordering claim: ESRCH from a
//! failed pid lookup must NOT win over the NULL buffer's EINVAL. §6.114 fixed
//! the negative-pid half (it made `sched_pid_verdict` refuse a truncated
//! negative word); this pin covers the other half — the NULL buffer — which
//! MPOS answered EFAULT (self) or ESRCH (nobody) because it looked the pid up
//! and reached `buffer_ok` first.
//!
//! `sched_rr_get_interval` is the deliberate **exception**: it has no such
//! guard, so its pid lookup comes first — `rr_get_interval(0, NULL)` is EFAULT
//! and `rr_get_interval(999999, NULL)` is ESRCH. §6.114 already kept that
//! order, and the control claims below assert it stays that way, so this fix
//! cannot be "apply the guard to all seven doors".
//!
//! Out of scope, named and measured but not asserted here (a separate intent):
//! `sched_getattr`/`sched_setattr`'s `size`/`flags` guards, the `usize >
//! PAGE_SIZE` ceiling, `sched_setparam`'s priority-range check, and the
//! door-specific EFAULT/ESRCH order for a *non-NULL bad* pointer. This pin
//! uses only NULL, well-formed, and one clearly-bad pointer on self.
//! Run with `MPOS_ON_HOST=1` the same rows measure Linux.

use mpos_user_tests::Report;

/// The errno a raw syscall answered: `0` for success, else the host's errno.
unsafe fn raw(
    nr: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
) -> i32 {
    let r = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if r < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

/// The five buffered doors for one (pid, buffer) pair, in the order
/// getparam, setparam, setscheduler(SCHED_OTHER), getattr, setattr.
/// `null` chooses a NULL buffer; otherwise well-formed structs are used
/// (`sched_param` with priority 0, and a 48-byte `sched_attr` with
/// `size = 48`, `policy = SCHED_OTHER`, `nice = 0`). Passing four varargs to
/// every door is harmless — a syscall reads only its own named registers.
fn buffered(pid: libc::c_long, null: bool) -> [i32; 5] {
    let mut sp = [0u8; 4];
    let mut attr = [0u8; 48];
    attr[0..4].copy_from_slice(&48u32.to_ne_bytes());
    let sp_p = if null {
        0
    } else {
        sp.as_mut_ptr() as libc::c_long
    };
    let at_p = if null {
        0
    } else {
        attr.as_mut_ptr() as libc::c_long
    };
    unsafe {
        [
            raw(libc::SYS_sched_getparam, pid, sp_p, 0, 0),
            raw(libc::SYS_sched_setparam, pid, sp_p, 0, 0),
            raw(libc::SYS_sched_setscheduler, pid, 0, sp_p, 0),
            raw(libc::SYS_sched_getattr, pid, at_p, 48, 0),
            raw(libc::SYS_sched_setattr, pid, at_p, 0, 0),
        ]
    }
}

/// The five buffered doors with a clearly-bad (non-NULL) pointer, on self.
/// A bad pointer must still be EFAULT — NOT the EINVAL the NULL guard gives.
fn buffered_badptr() -> [i32; 5] {
    let one = 1 as libc::c_long;
    let pid = 0 as libc::c_long;
    unsafe {
        [
            raw(libc::SYS_sched_getparam, pid, one, 0, 0),
            raw(libc::SYS_sched_setparam, pid, one, 0, 0),
            raw(libc::SYS_sched_setscheduler, pid, 0, one, 0),
            raw(libc::SYS_sched_getattr, pid, one, 48, 0),
            raw(libc::SYS_sched_setattr, pid, one, 0, 0),
        ]
    }
}

/// `sched_setscheduler` with a NULL param across the policies it knows. The
/// early guard fires before the policy is even looked at, so all four answer
/// EINVAL on Linux (measured in `edges.c`).
fn setsched_null_policies() -> [i32; 4] {
    let pid = 0 as libc::c_long;
    unsafe {
        [
            raw(libc::SYS_sched_setscheduler, pid, 0, 0, 0), // SCHED_OTHER
            raw(libc::SYS_sched_setscheduler, pid, 1, 0, 0), // SCHED_FIFO
            raw(libc::SYS_sched_setscheduler, pid, 2, 0, 0), // SCHED_RR
            raw(libc::SYS_sched_setscheduler, pid, 999, 0, 0), // unknown policy
        ]
    }
}

/// The exception door, `sched_rr_get_interval`: pid lookup comes first.
fn rr_null(pid: libc::c_long) -> i32 {
    unsafe { raw(libc::SYS_sched_rr_get_interval, pid, 0, 0, 0) }
}

fn main() -> ! {
    let mut r = Report::new();
    let self_pid = 0 as libc::c_long;
    let nobody = 999_999 as libc::c_long;
    let negative = -1i64 as libc::c_long;

    // The two discriminating rows: without the shared NULL guard these read
    // [14 x 5] (self) and [3 x 5] (nobody), not [22 x 5].
    let self_null = buffered(self_pid, true);
    r.check(
        &format!("a NULL buffer is EINVAL on self, before any pid lookup ({self_null:?})"),
        self_null.iter().all(|e| *e == 22),
    );
    let nobody_null = buffered(nobody, true);
    r.check(
        &format!("a NULL buffer is EINVAL even for a nonexistent pid ({nobody_null:?})"),
        nobody_null.iter().all(|e| *e == 22),
    );
    let neg_null = buffered(negative, true);
    r.check(
        &format!("a NULL buffer is EINVAL for a negative pid too ({neg_null:?})"),
        neg_null.iter().all(|e| *e == 22),
    );

    // Control: with a well-formed buffer the pid lookup still decides.
    let self_valid = buffered(self_pid, false);
    r.check(
        &format!("a valid buffer on self succeeds ({self_valid:?})"),
        self_valid.iter().all(|e| *e == 0),
    );
    let nobody_valid = buffered(nobody, false);
    r.check(
        &format!("a valid buffer on a nonexistent pid is ESRCH ({nobody_valid:?})"),
        nobody_valid.iter().all(|e| *e == 3),
    );

    // A bad *non-NULL* pointer is EFAULT, not the NULL guard's EINVAL.
    let bad = buffered_badptr();
    r.check(
        &format!("a bad non-NULL pointer is EFAULT ({bad:?})"),
        bad.iter().all(|e| *e == 14),
    );

    // The guard fires before the policy is examined, for every policy.
    let policies = setsched_null_policies();
    r.check(
        &format!("a NULL param is EINVAL on every setscheduler policy ({policies:?})"),
        policies.iter().all(|e| *e == 22),
    );

    // The exception door keeps §6.114's pid-first order: no NULL guard there.
    let rr_self = rr_null(self_pid);
    let rr_nobody = rr_null(nobody);
    r.check(
        &format!("rr_get_interval keeps pid-first (NULL+self {rr_self}, NULL+nobody {rr_nobody})"),
        rr_self == 14 && rr_nobody == 3,
    );

    r.finish()
}
