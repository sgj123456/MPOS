//! The seven `sched_*` doors read their `pid` as a signed 32-bit `pid_t`, and a
//! negative one is `EINVAL` — not `ESRCH`.
//!
//! Measured on Linux 7.2.14 (`build/scratch/schedpid/vector.c`, musl static,
//! `syscall()` so the *full 64-bit word* reaches the kernel and the truncation
//! is the kernel's, not a userspace wrapper's). Each row is the errno of all
//! seven doors for one pid word; `0` means the call succeeded:
//!
//! ```text
//! -1       setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
//! -2       setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
//! 0        setparam=0  getparam=0  setsched=0  getsched=0  rr_int=0  setattr=0  getattr=0
//! 999999   setparam=3  getparam=3  setsched=3  getsched=3  rr_int=3  setattr=3  getattr=3
//! 2^32     setparam=0  getparam=0  setsched=0  getsched=0  rr_int=0  setattr=0  getattr=0
//! 2^32-1   setparam=22 getparam=22 setsched=22 getsched=22 rr_int=22 setattr=22 getattr=22
//! ```
//!
//! `2^32` truncates to `0` (self) and `2^32-1` truncates to `-1`, which is why
//! those two rows are the same as `0` and `-1`: the kernel reads a `pid_t`, not
//! the 64-bit register word. `build/scratch/schedpid/w.c` pinned the truncation
//! one value at a time (`4294967296 -> self`, `4294967295 -> EINVAL`,
//! `4294967297 -> pid 1`).
//!
//! MPOS read `args[0] as isize` — a 64-bit word — so a userspace `-1`
//! (`0xffff_ffff_ffff_ffff`) went to `resolve_pid` as a huge *positive* number
//! and every door answered `ESRCH`(-3). The `pid < 0` guard in the old
//! `live_thread` was dead code for exactly that reason. `2^32` did not become
//! self either — it was a number naming nobody, so `ESRCH` again.
//!
//! The fix is one shared verdict (`abi::sched_pid_verdict`, host-tested) that
//! truncates to `i32` first, answers `EINVAL` for a negative result, and lets
//! `sched_target` resolve zero to self and a positive number through
//! `resolve_pid` (ESRCH when nobody answers). This pin calls the raw syscalls
//! with the full 64-bit word so it witnesses the truncation in the kernel; a
//! libc wrapper such as `sched_getparam(pid as i32, ..)` would truncate in
//! userspace and hide the very bug. The seven doors are all wired through that
//! one verdict, so a row that agrees on all seven is the claim.
//!
//! The refusal-*ordering* differences between the doors (getparam checks its
//! buffer before the pid, rr_get_interval checks the pid first) are a separate
//! named gap; this pin only uses well-formed buffers, so no door's buffer
//! check is in play. Run with `MPOS_ON_HOST=1` the same rows measure Linux.

use mpos_user_tests::Report;

/// The errno a raw syscall answered: `0` for success, else the host's errno.
/// `libc::syscall` returns -1 and sets errno on failure, the shape every other
/// pin here uses (`t-rt-signal-queue`'s `try_queue`).
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

/// The seven doors for one pid word, in the order the dispatch arms are
/// written: setparam, getparam, setscheduler, getscheduler,
/// rr_get_interval, setattr, getattr.
fn seven(pid: libc::c_long) -> [i32; 7] {
    let mut sp = [0u8; 4];
    let mut ts = [0u8; 16];
    let mut attr = [0u8; 48];
    attr[0..4].copy_from_slice(&48u32.to_ne_bytes());
    // `sp` is `struct sched_param` (its four bytes are `sched_priority = 0`,
    // the one value SCHED_OTHER admits); `ts` is a 16-byte timespec; `attr` is
    // the 48-byte first `sched_attr` with `size = 48`, `policy = SCHED_OTHER`
    // and `nice = 0`. Passing four varargs to every door is harmless — a
    // syscall reads only the registers its own signature names.
    unsafe {
        let sp_p = sp.as_mut_ptr() as libc::c_long;
        let ts_p = ts.as_mut_ptr() as libc::c_long;
        let at_p = attr.as_mut_ptr() as libc::c_long;
        [
            raw(libc::SYS_sched_setparam, pid, sp_p, 0, 0),
            raw(libc::SYS_sched_getparam, pid, sp_p, 0, 0),
            // policy 0 = SCHED_OTHER, with its zero priority in `sp`.
            raw(libc::SYS_sched_setscheduler, pid, 0, sp_p, 0),
            raw(libc::SYS_sched_getscheduler, pid, 0, 0, 0),
            raw(libc::SYS_sched_rr_get_interval, pid, ts_p, 0, 0),
            raw(libc::SYS_sched_setattr, pid, at_p, 0, 0),
            raw(libc::SYS_sched_getattr, pid, at_p, 48, 0),
        ]
    }
}

fn main() -> ! {
    let mut r = Report::new();
    // (label, the full pid word, the errno every door must answer; 0 = success)
    let cases: &[(&str, i64, i32)] = &[
        ("-1", -1, 22),
        ("-2", -2, 22),
        ("0", 0, 0),
        ("999999", 999_999, 3),
        ("2^32", 4_294_967_296, 0),
        ("2^32-1", 4_294_967_295, 22),
    ];
    for (label, pid, want) in cases {
        let got = seven(*pid as libc::c_long);
        r.check(
            &format!("all seven sched doors answer {want} for pid {label} ({got:?})"),
            got.iter().all(|e| *e == *want),
        );
    }
    r.finish()
}
