//! t-fault-at-trap - a handled `#PF` enters its handler *at the fault*, fixes
//! the mapping, and resumes the same instruction with every register intact.
//!
//! Measured on Linux 7.2.14 (`build/scratch/pfdeliver/rel.c`, `resume.c`,
//! `resume8.c`), a `PROT_NONE` page whose handler calls `mprotect` and returns:
//! the handler runs on **every** child (16/16), `si_addr` names the page, and
//! the store whose fault started all this completes with a live value in
//! callee-saved `rbx` still holding `0x12345678abcdef00`. The earlier reading
//! of this kernel was 2/16: the `x86-interrupt` page-fault entry hid the
//! general registers, so the queued `SIGSEGV` could only be handed over at the
//! *next PIT tick*; the remaining fourteen children re-faulted to death before
//! one arrived.
//!
//! Item 281 gives `#PF` its own register-preserving asm stub and calls the
//! §2.13 `deliver_at_trap` seam from the fault itself. The pin's shape is the
//! one the measurement proved: a handler that merely `_exit`s would not pin
//! the frame, because it never returns through `rt_sigreturn`, and a null
//! store would not pin the resume, because there is no mapping to repair.
//! What is checked here, eight children at a time, is that the handler ran
//! once, saw the page's own `si_addr` and a fault-shaped `si_code`, repaired
//! the page, and that the original store then completed with the callee-saved
//! word intact. The C probe held it in `rbx`; Rust's inline asm cannot name
//! that register (LLVM reserves it on this target), so the Rust pin holds the
//! same kind of word in `r12`. Either one pins the general-register half of
//! the frame.
//!
//! The eight children are the reliability claim: without the trap-time door
//! this suite reads 2/16, not a clean 0. A single child can get lucky on a
//! timer tick; eight in a row cannot. `MPOS_ON_HOST=1` runs the same fixture
//! against real Linux, whose handler is synchronous and passes too.
//!
//! The `si_code` claim accepts both fault-shaped answers: Linux reports
//! `SEGV_ACCERR` (2) for a `PROT_NONE` page because the VMA exists, while
//! this kernel's `#PF` door reports `SEGV_MAPERR` (1) because its leaf was
//! absent. The wrong answer is `SI_USER` (0), which is what the old sender
//! record put there; that divergence is named in `docs/notes/` §6.160.
//!
//! This is x86_64's own entry: the aarch64 `el0_sync` frame is already a full
//! `UserFrame`, but its refused faults still die without queueing a signal
//! (`docs/notes/` §6.159's named gap). This program therefore joins
//! `tests.toml`'s `[aarch64] gaps` rather than claiming an answer there.

#[cfg(target_arch = "x86_64")]
use mpos_user_tests::Report;
#[cfg(target_arch = "x86_64")]
use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap_anonymous, mprotect, munmap};
#[cfg(target_arch = "x86_64")]
use std::arch::asm;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// What the handler saw. Atomics are the honest way to share a word across an
/// asynchronous frame boundary: a plain `static mut` lets the compiler cache
/// the main line's read across the handler's write.
#[cfg(target_arch = "x86_64")]
static RAN: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_CODE: AtomicI32 = AtomicI32::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_ADDR: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static PAGE: AtomicU64 = AtomicU64::new(0);

/// A bit pattern no compiler would invent, held in `r12` across the fault.
#[cfg(target_arch = "x86_64")]
const KEEP: u64 = 0x1234_5678_abcd_ef00;
#[cfg(target_arch = "x86_64")]
const TRIALS: usize = 8;

#[cfg(target_arch = "x86_64")]
extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    let _ = sig;
    RAN.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        // SAFETY: `info` is the record the kernel filled in for this
        // `SA_SIGINFO` handler; the accessors are the machine's own.
        unsafe {
            SEEN_CODE.store((*info).si_code, Ordering::SeqCst);
            SEEN_ADDR.store((*info).si_addr() as usize as u64, Ordering::SeqCst);
        }
    }
    let page = PAGE.load(Ordering::SeqCst) as *mut libc::c_void;
    // The handler's whole job is to make the instruction it interrupted
    // retryable. It must not skip the store: a frame that resumed *after*
    // the faulting instruction would leave the page zero and pass the
    // handler-ran claim while breaking the ABI.
    let rc = unsafe { mprotect(page, 4096, MprotectFlags::READ | MprotectFlags::WRITE) };
    assert!(rc.is_ok(), "the handler could not repair the page");
}

#[cfg(target_arch = "x86_64")]
fn arm() {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves; musl
    // fills the restorer slot itself (measured in `t-signal-delivery`'s doc).
    let rc = unsafe { libc::sigaction(libc::SIGSEGV, &act, std::ptr::null_mut()) };
    assert!(rc == 0, "installing the SIGSEGV handler failed");
}

/// One child's whole claim, as an exit code. The inline asm holds `r12` across
/// the store: a Rust call in between would be testing the compiler's register
/// allocator rather than the kernel's signal frame.
#[cfg(target_arch = "x86_64")]
fn child() -> i32 {
    let page = unsafe {
        mmap_anonymous(
            std::ptr::null_mut(),
            4096,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    };
    let page = match page {
        Ok(p) => p as usize as u64,
        Err(_) => return 71,
    };
    PAGE.store(page, Ordering::SeqCst);
    RAN.store(0, Ordering::SeqCst);
    SEEN_CODE.store(0, Ordering::SeqCst);
    SEEN_ADDR.store(0, Ordering::SeqCst);
    arm();

    let mut keep = KEEP;
    let mut value: u64 = 0;
    // SAFETY: `page` is one mapped page this child owns. The store is the
    // faulting instruction the handler must resume into. The `inout("r12")`
    // binding holds the callee-saved word in that register across the whole
    // block, so the handler sees `KEEP` there at the moment of the fault.
    unsafe {
        asm!(
            "mov rsi, {p}",
            "mov byte ptr [rsi], 42",
            "movzx {v}, byte ptr [rsi]",
            p = in(reg) page,
            v = out(reg) value,
            lateout("rsi") _,
            inout("r12") keep,
            options(nostack)
        );
    }
    // A compiler barrier: nothing above may be reordered past these reads of
    // words the handler wrote (the C probe's first version lost exactly here).
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    let ran = RAN.load(Ordering::SeqCst);
    let code = SEEN_CODE.load(Ordering::SeqCst);
    let addr = SEEN_ADDR.load(Ordering::SeqCst);
    let ok = ran == 1 && value == 42 && keep == KEEP && addr == page && (code == 2 || code == 1);
    let _ = unsafe { munmap(page as *mut libc::c_void, 4096) };
    if ok { 0 } else { 70 }
}

fn main() {
    #[cfg(not(target_arch = "x86_64"))]
    {
        println!("t-fault-at-trap: skipped (the #PF entry is x86_64's own)");
        return;
    }
    #[cfg(target_arch = "x86_64")]
    x86_main();
}

#[cfg(target_arch = "x86_64")]
fn x86_main() {
    println!("t-fault-at-trap");
    let mut r = Report::new();
    let mut good = 0;
    let mut died = 0;
    let mut other = 0;
    for _ in 0..TRIALS {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            r.check("a child was forked", false);
            break;
        }
        if pid == 0 {
            unsafe { libc::_exit(child()) };
        }
        let mut status = 0i32;
        let waited = unsafe { libc::wait4(pid, &raw mut status, 0, std::ptr::null_mut()) };
        r.check("the parent waited for the child", waited == pid);
        if status & 0x7f != 0 && status & 0x7f != 0x7f {
            died += 1;
        } else if (status >> 8) & 0xff == 0 {
            good += 1;
        } else {
            other += 1;
        }
    }
    r.check(
        &format!("all {TRIALS} handled faults resumed at the trap (good={good} died={died} other={other})"),
        good == TRIALS && died == 0 && other == 0,
    );
    r.finish();
}
