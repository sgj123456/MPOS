//! t-rt-sigaction - the four-word disposition record, and the numbers nobody
//! may be given one for.
//!
//! A musl binary's strace shows four `rt_sigaction` calls on the way up: a
//! query of the untouched `SIGSEGV` and `SIGBUS` dispositions
//! (`rt_sigaction(SIGSEGV, NULL, {SIG_DFL}, 8)`), `SIGPIPE` set to `SIG_IGN` -
//! which Rust std does at startup and panics with "cannot ignore SIGPIPE" if it
//! fails, so a 0 here is load-bearing - and `SIGSEGV`/`SIGBUS` armed with
//! `SA_SIGINFO|SA_ONSTACK` on top of the alt stack `sigaltstack` installed.
//!
//! Every call must answer 0. A query must also actually *write* the caller's
//! record: the test seeds it with sentinels, because a query that answers 0 and
//! writes nothing hands musl uninitialized memory. Beyond the strace's own
//! calls the refusal set is pinned - signal 0 is not a signal, numbers above
//! `_NSIG` (64) are not signals, and `SIGKILL`/`SIGSTOP` cannot be given a
//! disposition - beside the range stage A/B opened: `SIGRTMIN` (32) and
//! `SIGRTMAX` (64) are queryable and installable, which is what makes the
//! real-time tests beside this one possible.
//!
//! # Why the whole program is the raw door
//!
//! The subject *is* the four-word record and the refusals at the kernel's
//! boundary, and a libc's `sigaction` is neither. `libc::sigaction` is the
//! machine's *user-space* struct - a 128-byte `sigset_t` in the middle of it -
//! which its wrapper converts into the kernel's four words on the way down and
//! back again on the way up; and that wrapper validates the signal number
//! itself, refusing 0 and 65 in userspace before the syscall, and on musl
//! refusing its own three real-time numbers (32, 33, 34) as well. So the
//! refusals, the seeded record and the `EFAULT` probe would all be measuring
//! musl. This is clause (c) on the record's shape and clause (b) on the
//! wrapper's answers, measured the way batch 12 measured `fstat`'s and batch 17
//! measured `epoll_create`'s.
//!
//! The record the raw door hands over is therefore declared here:
//! `{handler, flags, restorer, mask}`, 32 bytes, the mask one 8-byte word -
//! and `sigsetsize` is the `8` the ABI names. That shape is *both* machines'
//! and was measured on both before it was written down: on x86_64 this host's
//! kernel fills a query's record in exactly those four slots, and on arm64 the
//! crate's own static `libc.a` builds the same record for its own
//! `__libc_sigaction` (`{handler@0, flags@8, restorer@16, mask@24}`, then
//! `SYS_rt_sigaction` 134 with `sigsetsize` 8 and `0x04000000` ORed into the
//! flags). So the no-std version's `lin::SigAction` was right on both machines;
//! what changes is that the *numbers* now come from libc's per-arch table
//! (`SYS_rt_sigaction`: 13 / 134) and the signal names from libc's.
//!
//! # The runtime moved under this test, and that changed one claim
//!
//! The no-std version read the *fresh process's* `SIGSEGV` disposition and
//! demanded `SIG_DFL`. That is no longer what a caller finds: Rust std arms
//! `SIGSEGV` and `SIGBUS` at startup - the strace lines quoted above - so the
//! claim that a query *writes what it found* is made where the value is
//! knowable (a number the runtime leaves alone), and the `SIGSEGV` query keeps
//! the half that is about the kernel: it must overwrite every seeded word.
//! The two guard dispositions are saved and handed back on the way out, because
//! overwriting them changes the runtime this program runs inside rather than a
//! fixture of its own.
//!
//! # The two the kernel keeps for itself, and the query still served
//!
//! Linux refuses `SIGKILL`/`SIGSTOP` only when a disposition is being *set* -
//! `act && sig_kernel_only(sig)` in `do_sigaction` - so a bare query of either
//! is served and reports `SIG_DFL` (measured on this host, and on both machines
//! now). This kernel refused the query too until 2026-10-16, which is the
//! shape a caller that probes a disposition before setting one cannot recover
//! from: it takes a fallback it did not need. The claims below pin Linux's
//! split on **both** machines - a null-act query of either is `0` with a
//! cleared record, an install of either is `EINVAL` - and `docs/uapi/` §5.2
//! no longer carries the divergence.

use mpos_user_tests::Report;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

/// The sentinel every word of an out-record is seeded with, so a query that
/// writes nothing is a query the caller can *see* did nothing.
const SEED: usize = 0x5a5a_5a5a_5a5a_5a5a;

/// `SA_RESTORER`, the flag that says the word beside the handler holds a valid
/// `rt_sigreturn` trampoline. `libc` names it for no machine this crate builds
/// for (measured: the crate defines the constant under redox alone), so it is
/// written here - the one number in this program whose source is not a
/// maintained table. Both machines' `libc.a` send it: musl's `__libc_sigaction`
/// ORs `0x04000000` into the flags word on x86_64 and on arm64 alike, and fills
/// the restorer slot, which is the shape copied below.
const SA_RESTORER: usize = 0x0400_0000;

/// The kernel's `struct sigaction` - the four words `rt_sigaction` copies in
/// and out. `[repr(C)]` because the kernel reads this memory, and not
/// `libc::sigaction`, which is the machine's *user-space* struct (a 128-byte
/// mask and a different order).
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelSigaction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

/// `rt_sigaction(sig, act, old, sigsetsize)`, with `sigsetsize` the ABI's 8.
///
/// The number comes from libc's per-arch table and the three arguments are the
/// caller's own words - which is the whole point of taking the raw door here.
fn rt_sigaction(sig: i32, act: *const KernelSigaction, old: *mut KernelSigaction) -> isize {
    // SAFETY: `act` and `old` are this process's records (or null, which the
    // ABI reads as "not interested"), and the size word is the ABI's 8.
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigaction,
            sig as libc::c_long,
            act as usize as libc::c_long,
            old as usize as libc::c_long,
            8 as libc::c_long,
        ) as isize
    }
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// A record with every word seeded, for the queries below.
fn seeded() -> KernelSigaction {
    KernelSigaction {
        handler: SEED,
        flags: SEED,
        restorer: SEED,
        mask: SEED as u64,
    }
}

/// The `rt_sigreturn` a handler returns through. This program never delivers a
/// signal, so the trampoline is never entered; what matters is that the slot
/// holds a real address, which is the word musl's own wrapper supplies.
extern "C" fn restorer() {
    // SAFETY: never reached - no handler in this program is ever invoked.
    unsafe {
        libc::syscall(libc::SYS_rt_sigreturn);
    }
}

extern "C" fn handler(_sig: libc::c_int) {}

fn main() {
    println!("t-rt-sigaction");
    let mut r = Report::new();
    let restorer_addr = restorer as extern "C" fn() as usize;
    let handler_addr = handler as extern "C" fn(libc::c_int) as usize;

    // --- the query shape ---------------------------------------------------
    //
    // The running process's SIGSEGV is std's own guard by the time this runs,
    // which is why the *value* is not claimed here: what the kernel owes the
    // caller is that it writes the record it was handed. A fresh-process
    // SIG_DFL is claimed on the next number, which nothing has touched.
    let mut old = seeded();
    r.check(
        "querying the running process's SIGSEGV disposition succeeds",
        rt_sigaction(libc::SIGSEGV, core::ptr::null(), &mut old) == 0,
    );
    r.check(
        "and the query overwrites every seeded word of the record",
        old.handler != SEED && old.flags != SEED && old.restorer != SEED && old.mask != SEED as u64,
    );
    let mut old = seeded();
    r.check(
        "querying a number nothing has touched (SIGUSR1) succeeds",
        rt_sigaction(libc::SIGUSR1, core::ptr::null(), &mut old) == 0,
    );
    r.check(
        "and it reports SIG_DFL with clear flags and mask",
        old.handler == libc::SIG_DFL && old.flags == 0 && old.mask == 0,
    );

    // --- the runtime's own installs, re-made -------------------------------
    let act = KernelSigaction {
        handler: libc::SIG_IGN,
        flags: SA_RESTORER,
        restorer: restorer_addr,
        mask: 0,
    };
    r.check(
        "installing SIG_IGN on SIGPIPE succeeds",
        rt_sigaction(libc::SIGPIPE, &act, core::ptr::null_mut()) == 0,
    );

    // std's guard-stack pair: SIGSEGV and SIGBUS armed with SA_ONSTACK. Both
    // dispositions are saved here and handed back at the end (the shape
    // t-sigaltstack gives the same kind of change).
    let guard = KernelSigaction {
        handler: handler_addr,
        flags: (libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART) as usize | SA_RESTORER,
        restorer: restorer_addr,
        mask: 0,
    };
    let mut saved_segv = seeded();
    let mut saved_bus = seeded();
    r.check(
        "installing the SIGSEGV handler succeeds",
        rt_sigaction(libc::SIGSEGV, &guard, &mut saved_segv) == 0,
    );
    r.check(
        "installing the SIGBUS handler succeeds",
        rt_sigaction(libc::SIGBUS, &guard, &mut saved_bus) == 0,
    );

    // --- the refusal set ---------------------------------------------------
    //
    // Two questions, and Linux asks them separately: is this a *signal*
    // (`1..=_NSIG`, 64) - asked whether or not a record is handed over, since
    // signal 0 and the numbers past `_NSIG` name nothing at all - and may it
    // be *given* a disposition - asked only when `act` is non-null, because
    // `SIGKILL`/`SIGSTOP` are the kernel's own. A bare query of either is
    // served and reports `SIG_DFL` (measured on this host:
    // `rt_sigaction(SIGKILL, NULL, &old, 8)` answers 0 and clears the record),
    // while an install of either is `EINVAL`.
    r.check(
        "rt_sigaction on signal 0 is EINVAL",
        refused(
            rt_sigaction(0, core::ptr::null(), core::ptr::null_mut()),
            libc::EINVAL,
        ),
    );
    r.check(
        "rt_sigaction past _NSIG (64) is EINVAL",
        refused(
            rt_sigaction(65, core::ptr::null(), core::ptr::null_mut()),
            libc::EINVAL,
        ),
    );
    for (name, sig) in [("SIGKILL", libc::SIGKILL), ("SIGSTOP", libc::SIGSTOP)] {
        let mut old = seeded();
        r.check(
            &format!("a bare query of {name} is served"),
            rt_sigaction(sig, core::ptr::null(), &mut old) == 0,
        );
        r.check(
            &format!("...and it reports SIG_DFL with clear flags and mask"),
            old.handler == libc::SIG_DFL && old.flags == 0 && old.mask == 0,
        );
        let install = KernelSigaction {
            handler: handler_addr,
            flags: SA_RESTORER,
            restorer: restorer_addr,
            mask: 0,
        };
        r.check(
            &format!("installing a disposition on {name} is EINVAL"),
            refused(
                rt_sigaction(sig, &install, core::ptr::null_mut()),
                libc::EINVAL,
            ),
        );
    }

    // --- the range stage A/B opened ----------------------------------------
    //
    // The real-time numbers are 32..=64 on the wire, whatever a libc's own
    // `SIGRTMIN` says. A never-set one reports SIG_DFL with a clean mask rather
    // than EINVAL, and a handler can be installed on it - which is what every
    // test beside this one relies on.
    const RTMIN: i32 = 32;
    let mut old = seeded();
    r.check(
        "querying the fresh SIGRTMIN disposition succeeds",
        rt_sigaction(RTMIN, core::ptr::null(), &mut old) == 0,
    );
    r.check(
        "it reports SIG_DFL with clear flags and mask",
        old.handler == libc::SIG_DFL && old.flags == 0 && old.mask == 0,
    );
    let act = KernelSigaction {
        handler: handler_addr,
        flags: libc::SA_SIGINFO as usize | SA_RESTORER,
        restorer: restorer_addr,
        mask: 0,
    };
    r.check(
        "installing a handler on SIGRTMIN+3 (35) succeeds",
        rt_sigaction(RTMIN + 3, &act, core::ptr::null_mut()) == 0,
    );
    r.check(
        "rt_sigaction on SIGRTMAX (64) is accepted",
        rt_sigaction(64, core::ptr::null(), core::ptr::null_mut()) == 0,
    );

    // --- a record pointer nobody can read ----------------------------------
    //
    // The no-std version named the fixed `0x5000_0000`, because its static-PIE
    // image could not be there. Under a dynamic musl that address may name a
    // real mapping, so the page is one this process mapped `PROT_NONE` and
    // hands back afterwards - unreadable by construction on both machines (the
    // shape `t-rt-sigprocmask` gives the same probe).
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
            r.check(
                "an unreadable act pointer is EFAULT",
                refused(
                    rt_sigaction(
                        libc::SIGPIPE,
                        addr as *const KernelSigaction,
                        core::ptr::null_mut(),
                    ),
                    libc::EFAULT,
                ),
            );
            let _ = unsafe { munmap(addr, len) };
        }
        Err(_) => r.check("an unreadable act pointer is EFAULT", false),
    }

    // Hand the runtime its guard dispositions back.
    let _ = rt_sigaction(libc::SIGSEGV, &saved_segv, core::ptr::null_mut());
    let _ = rt_sigaction(libc::SIGBUS, &saved_bus, core::ptr::null_mut());

    r.finish()
}
