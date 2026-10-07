//! t-rt-sigprocmask - one test for rt_sigprocmask, which a musl binary's
//! strace shows once: Rust std unblocking SIGRT_1 and SIGRT_2 at startup
//! (`rt_sigprocmask(SIG_UNBLOCK, [RT_1 RT_2], NULL, 8) = 0`).
//!
//! The test issues that exact call - the signal set as one 8-byte mask,
//! bit n holding signal n+1 - and then restores an empty mask, the other
//! call shape a libc cannot avoid around handler setup. Both must answer
//! 0. A query that answers 0 but leaves the caller's mask alone hands musl
//! garbage, so the written answer is part of the contract.
//!
//! Beyond the strace's two calls, the refusal set is pinned: an unknown
//! `how`, a `sigsetsize` other than the ABI's 8 (that one goes through a
//! raw call because every wrapper pins its own size), a `set` that cannot
//! be read, and a query that must write an empty mask over a
//! sentinel-seeded out-word. `SIG_BLOCK` with a null `set` is allowed -
//! the set is optional.
//!
//! # Why the whole program is the raw door
//!
//! The subject *is* the four-word shape: sigsetsize as the caller names it,
//! the raw 64-bit word as both the in- and out-parameter. rustix's
//! `thread::signal_unblock` takes a typed `SigSet` and pins its own size;
//! libc's `sigprocmask` hides the fourth word behind musl's default. That
//! is clause (c) on the shape and clause (a) on the word, measured the same
//! way batch 7 measured `faccessat2`. The number comes from libc's table
//! (`SYS_rt_sigprocmask`: 14 / 135), which agrees with this kernel's own
//! tables at the same values.
//!
//! One fixture change came with the move: the unreadable-set probe no longer
//! sits at the no-std suite's fixed `0x5000_0000`, because under a dynamic
//! musl that address belongs to nobody in particular - it could name a real
//! mapping. It is now a page this process has mapped `PROT_NONE` and handed
//! back, which is unreadable by construction on both machines.

use mpos_user_tests::Report;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

/// SIGRT_1 = 34 lives in bit 33, SIGRT_2 = 35 in bit 34.
const RT_PAIR: u64 = (1u64 << 33) | (1u64 << 34);

const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;

fn rt_sigprocmask(how: usize, set: *const u64, old: *mut u64, sigsetsize: usize) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            how,
            set as usize,
            old as usize,
            sigsetsize,
        ) as isize
    }
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-rt-sigprocmask");
    let mut r = Report::new();

    let set = RT_PAIR;
    let ret = rt_sigprocmask(SIG_UNBLOCK, &set, core::ptr::null_mut(), 8);
    r.check("unblocking SIGRT_1 and SIGRT_2 succeeds", ret == 0);

    let empty: u64 = 0;
    let ret = rt_sigprocmask(SIG_SETMASK, &empty, core::ptr::null_mut(), 8);
    r.check("restoring an empty mask succeeds", ret == 0);

    // `how` is a two-value enum (0..=2); anything past it is refused.
    let ret = rt_sigprocmask(5, &set, core::ptr::null_mut(), 8);
    r.check("an unknown how of 5 is EINVAL", refused(ret, libc::EINVAL));

    // The ABI pins sigsetsize to 8 - the kernel validates the size before
    // it looks at anything else.
    let ret = rt_sigprocmask(SIG_BLOCK, &set, core::ptr::null_mut(), 16);
    r.check("a sigsetsize of 16 is EINVAL", refused(ret, libc::EINVAL));

    // A query has to write its answer: the seeded sentinel must be gone,
    // replaced by the empty mask a fresh process owes.
    let mut old: u64 = 0x0000_DEAD_BEEF;
    let ret = rt_sigprocmask(SIG_SETMASK, core::ptr::null(), &mut old, 8);
    r.check("querying the mask with a null set succeeds", ret == 0);
    r.check(
        "the query writes the empty mask over the sentinel",
        old == 0,
    );

    // The set is optional: changing nothing through SIG_BLOCK with a null
    // set is a valid, successful call.
    let ret = rt_sigprocmask(SIG_BLOCK, core::ptr::null(), core::ptr::null_mut(), 8);
    r.check("SIG_BLOCK with a null set is allowed", ret == 0);

    // A set pointer that names no readable mapping is EFAULT, not a crash.
    // The page is PROT_NONE for exactly this reason (rustix's own rule:
    // "for PROT_NONE, use ProtFlags::empty()"), and unmapped after.
    let len = 4096usize;
    match unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            len,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    } {
        Ok(addr) => {
            let ret = rt_sigprocmask(SIG_BLOCK, addr as *const u64, core::ptr::null_mut(), 8);
            r.check(
                "an unreadable set pointer is EFAULT",
                refused(ret, libc::EFAULT),
            );
            let _ = unsafe { munmap(addr, len) };
        }
        Err(_) => {
            r.check("an unreadable set pointer is EFAULT", false);
        }
    }

    r.finish()
}
