//! t-fault-sicode - a page fault's `si_code` answers the *mapping's* question:
//! does a VMA describe this address, and did it refuse the access?
//!
//! Linux's discriminator is measured, not assumed (`build/scratch/protcode/`,
//! Linux 7.2.14): a VMA covers the address and refuses the access answers
//! `SEGV_ACCERR` (2), and only an address no VMA describes answers
//! `SEGV_MAPERR` (1). Four refused-access classes agree - a read of a
//! `PROT_NONE` page, a write to a `PROT_READ` page, an instruction fetch from a
//! non-executable page, and a write to a read-only *file* mapping all read
//! `code=2` - while a never-mapped read and a read from the hole between two
//! mappings both read `code=1`.
//!
//! This kernel used to derive the code from the page-table error word's `P`
//! bit, which is a different question and answers wrong for the most common
//! self-repair case: a `PROT_NONE` page has *no present entry*, so the CPU says
//! "not present" and the kernel told the handler `SEGV_MAPERR` - the branch
//! whose documented meaning is "nothing to repair", when `mprotect` was exactly
//! what would have fixed it. Item 282 asks the mapping instead
//! (`docs/notes/` §6.162).
//!
//! The counterfactual is that `P`-bit reading: it passes the two `MAPERR`
//! claims below and reddens the two `ACCERR` ones - because a `PROT_NONE` page
//! and an unmapped page are the same word to the CPU, which is precisely why
//! the CPU's word cannot answer this question.
//!
//! Each trial is a forked child with an `SA_SIGINFO` handler that records
//! `si_code` and `si_addr` and then `_exit`s, so nothing depends on a resumed
//! handler (`t-fault-at-trap` pins that half). `MPOS_ON_HOST=1` runs the same
//! fixture against real Linux.
//!
//! This is x86_64's own entry: aarch64's refused EL0 faults still die without
//! queueing a record at all (`docs/notes/` §6.159's named gap), so the program
//! joins `tests.toml`'s `[aarch64] gaps` rather than claiming an answer there.

#[cfg(target_arch = "x86_64")]
use core::cell::Cell;
#[cfg(target_arch = "x86_64")]
use mpos_user_tests::Report;
#[cfg(target_arch = "x86_64")]
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicU64, Ordering};

/// `SEGV_MAPERR` / `SEGV_ACCERR`, the two answers Linux's rule can give.
#[cfg(target_arch = "x86_64")]
const SEGV_MAPERR: i32 = 1;
#[cfg(target_arch = "x86_64")]
const SEGV_ACCERR: i32 = 2;

/// Where the handler records what it saw: **shared memory**, because a
/// `fork`ed child's own memory is private. A plain static in the child would be
/// a different word from the parent's, so the parent would read zeros and call
/// a correct kernel wrong - measured, this file's first version did exactly
/// that on real Linux. The C probes that measured the rule printed from inside
/// the handler for the same reason; a shared page is the same trick without
/// depending on the writer's buffer surviving `_exit`.
///
/// `[0]` is the number of times a handler ran, `[1]` its `si_code`, `[2]` its
/// `si_addr`.
#[cfg(target_arch = "x86_64")]
const SLOT_RAN: usize = 0;
#[cfg(target_arch = "x86_64")]
const SLOT_CODE: usize = 1;
#[cfg(target_arch = "x86_64")]
const SLOT_ADDR: usize = 2;
#[cfg(target_arch = "x86_64")]
static SEE: AtomicU64 = AtomicU64::new(0);

/// The shared record's address, for the handler to find.
#[cfg(target_arch = "x86_64")]
fn seen() -> *mut u64 {
    SEE.load(Ordering::SeqCst) as *mut u64
}

#[cfg(target_arch = "x86_64")]
extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    let _ = sig;
    let at = seen();
    if !info.is_null() && !at.is_null() {
        // SAFETY: `info` is the record this `SA_SIGINFO` handler was given and
        // `at` is the shared page this trial mapped before forking.
        unsafe {
            at.add(SLOT_RAN).write_volatile(1);
            at.add(SLOT_CODE).write_volatile((*info).si_code as u64);
            at.add(SLOT_ADDR)
                .write_volatile((*info).si_addr() as usize as u64);
        }
    }
    // The faulting instruction must not run again: no repair here, only the
    // record. `_exit` from a handler is how the C probes read the same fact.
    unsafe { libc::_exit(0) }
}

/// Install the handler this program's trials rely on.
#[cfg(target_arch = "x86_64")]
fn install_handler() {
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction =
            handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, core::ptr::null_mut());
    }
}

/// Run one trial in a forked child and answer what its handler recorded, or
/// `None` when the child never faulted.
#[cfg(target_arch = "x86_64")]
fn trial(
    r: &mut Report,
    label: &str,
    prepare: impl FnOnce(),
    body: impl FnOnce(),
) -> Option<(i32, u64)> {
    // Fresh per trial, shared with the child, and zeroed so "the handler never
    // ran" is a fact the parent can see rather than an initial value.
    // SAFETY: a one-page mapping of zeroed shared memory.
    let shared = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            4096,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
        )
    };
    let shared = match shared {
        Ok(p) => p,
        Err(_) => {
            r.check(&format!("{label}: the shared record was mapped"), false);
            return None;
        }
    };
    SEE.store(shared as usize as u64, Ordering::SeqCst);
    // `prepare` runs **after** the shared page is mapped, because a trial that
    // makes a hole has to make it last: a page freed before this `mmap` is a
    // page the allocator hands straight back, and the "hole" stops being one.
    // That is exactly how this file's first version read `MAPERR` as a failure
    // on real Linux - measured, and why the order is a parameter.
    prepare();
    // SAFETY: a plain `fork`; the child either runs `body` or exits.
    let child = unsafe { libc::fork() };
    if child == 0 {
        install_handler();
        body();
        // Reaching here means the trial never faulted, which is a failed
        // precondition rather than an answer.
        unsafe { libc::_exit(70) }
    }
    let mut status = 0;
    // SAFETY: `status` outlives the call and the child is ours.
    let waited = unsafe { libc::wait4(child, &mut status, 0, core::ptr::null_mut()) };
    if waited < 0 {
        r.check(&format!("{label}: the child was waited for"), false);
        return None;
    }
    if libc::WIFSIGNALED(status) {
        r.check(
            &format!("{label}: the handler ran (not a default death)"),
            false,
        );
        return None;
    }
    if libc::WEXITSTATUS(status) == 70 {
        r.check(&format!("{label}: the trial faulted at all"), false);
        return None;
    }
    // SAFETY: the mapping is still ours and the child is finished.
    let at = shared as *const u64;
    let ran = unsafe { at.add(SLOT_RAN).read_volatile() };
    if ran != 1 {
        r.check(
            &format!("{label}: the handler ran exactly once (ran={ran})"),
            false,
        );
        return None;
    }
    unsafe {
        Some((
            at.add(SLOT_CODE).read_volatile() as i32,
            at.add(SLOT_ADDR).read_volatile(),
        ))
    }
}

/// Read one byte through a pointer the compiler cannot fold away.
#[cfg(target_arch = "x86_64")]
unsafe fn read_volatile(p: *const u8) -> u8 {
    unsafe { p.read_volatile() }
}

/// Write one byte through a pointer the compiler cannot fold away.
#[cfg(target_arch = "x86_64")]
unsafe fn write_volatile(p: *mut u8, v: u8) {
    unsafe { p.write_volatile(v) }
}

#[cfg(target_arch = "x86_64")]
fn main() {
    let mut r = Report::new();
    let host = mpos_user_tests::host_run();
    let page = 4096usize;

    // A `PROT_NONE` mapping, and a `PROT_READ` one, both anonymous and both
    // *described* by a VMA: Linux answers `SEGV_ACCERR` for each.
    let none = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            page,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    };
    let read_only = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            page,
            ProtFlags::READ,
            MapFlags::PRIVATE,
        )
    };

    match (none, read_only) {
        (Ok(none), Ok(ro)) => {
            // 1. A read of a `PROT_NONE` page. This is the row the `P` bit
            // gets wrong: the leaf is absent, so the CPU says not-present.
            if let Some((code, addr)) = trial(
                &mut r,
                "PROT_NONE read",
                || {},
                || unsafe {
                    let _ = read_volatile(none as *const u8);
                },
            ) {
                r.check(
                    "PROT_NONE read: a mapping describes it, so the code is SEGV_ACCERR",
                    code == SEGV_ACCERR,
                );
                r.check(
                    "PROT_NONE read: si_addr is the page, not the bus address",
                    addr == none as usize as u64,
                );
            }

            // 2. A write to a `PROT_READ` page: a present leaf whose `W` bit
            // is clear, which the `P`-bit reading happened to get right.
            if let Some((code, addr)) = trial(
                &mut r,
                "write to PROT_READ",
                || {},
                || unsafe {
                    write_volatile(ro as *mut u8, 1);
                },
            ) {
                r.check(
                    "write to PROT_READ: the code is SEGV_ACCERR",
                    code == SEGV_ACCERR,
                );
                r.check(
                    "write to PROT_READ: si_addr is the page",
                    addr == ro as usize as u64,
                );
            }
        }
        _ => {
            r.check("the two described mappings were made", false);
        }
    }

    // 3. A never-mapped address: nothing describes it, so `SEGV_MAPERR`. This
    // is the row that keeps the answer honest in the other direction - a
    // kernel that simply answered `ACCERR` for everything reddens here.
    let wild = 0x1000usize;
    if let Some((code, addr)) = trial(
        &mut r,
        "never-mapped read",
        || {},
        || unsafe {
            let _ = read_volatile(wild as *const u8);
        },
    ) {
        r.check(
            "never-mapped read: nothing describes it, so the code is SEGV_MAPERR",
            code == SEGV_MAPERR,
        );
        r.check(
            "never-mapped read: si_addr is the address that was read",
            addr == wild as u64,
        );
    }

    // 4. The hole between two mappings: mapped either side, absent in the
    // middle. The nearest VMA is one page away, and the answer must still be
    // `MAPERR` - "a VMA is nearby" is not "a VMA describes this address".
    // The hole: three pages mapped, the middle one given back. Both
    // neighbours stay mapped, so the nearest VMA is one page away - and the
    // answer must still be `MAPERR`, because "a VMA is nearby" is not "a VMA
    // describes this address".
    let hole = Cell::new(0usize);
    let made = Cell::new(false);
    if let Some((code, addr)) = trial(
        &mut r,
        "hole between mappings",
        || {
            let span = page * 3;
            let three = unsafe {
                mmap_anonymous(
                    core::ptr::null_mut(),
                    span,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::PRIVATE,
                )
            };
            if let Ok(base) = three {
                let at = base as usize + page;
                made.set(unsafe { rustix::mm::munmap(at as *mut core::ffi::c_void, page).is_ok() });
                hole.set(at);
            }
        },
        || unsafe {
            let _ = read_volatile(hole.get() as *const u8);
        },
    ) {
        let hole = hole.get();
        let made = made.get();
        r.check("hole between mappings: the middle page was unmapped", made);
        r.check(
            "hole between mappings: the code is SEGV_MAPERR",
            code == SEGV_MAPERR,
        );
        r.check(
            "hole between mappings: si_addr is the hole",
            addr == hole as u64,
        );
    }

    if host {
        println!("note: this host's answers are Linux's; the guest arm pins the kernel's");
    }
    r.finish()
}

#[cfg(not(target_arch = "x86_64"))]
fn main() {
    println!("t-fault-sicode: skipped (aarch64 has no ESR->signal map yet)");
}
