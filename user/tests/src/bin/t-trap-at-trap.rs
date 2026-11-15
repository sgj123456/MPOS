//! t-trap-at-trap - a handled CPU exception enters its handler *at the trap*,
//! with the program's own registers, and resumes where Linux says it does.
//!
//! Measured in a guest before Item 283 (`build/scratch/traptrap/t.c`, musl
//! static): `int3` and `ud2` resumed only because a PIT tick happened to hand
//! the queued signal over before the repeat guard fired, and `idiv` by zero
//! **never** did - three `divide error` lines, then `task 6 died of signal 8 it
//! could not be handed: trap 0 code 0x0 at 0x4013f6, the same fault 3 times
//! over, and a handler is installed`. The cause was the entry, not the signal:
//! `x86-interrupt` hides the general registers behind a prologue Rust cannot
//! address, so `deliver_at_trap` had no register file to rewrite and the signal
//! waited for the next syscall - which a faulting instruction never makes.
//!
//! Item 283 gives the seven vectors a process can see (`#DE` 0, `#BP` 3, `#UD`
//! 6, `#NP` 11, `#SS` 12, `#GP` 13, `#AC` 17) one register-preserving stub
//! each, and calls the §2.13 door from the trap itself. What this pin checks is
//! the whole of that contract for the three vectors a program can execute on
//! purpose, eight children each:
//!
//!   * the handler ran **once**, in every child - the deterministic half. Under
//!     the old entry this was a lottery whose odds were the tick rate;
//!   * `si_code` is the vector's own (`SI_KERNEL` 128 for `#BP`, `ILL_ILLOPN`
//!     2 for `#UD`, `FPE_INTDIV` 1 for `#DE`), and `si_addr` is the faulting
//!     instruction for the two that have one and **zero** for `#BP`;
//!   * the saved `rip` is Linux's own: `int3` leaves the address **after** the
//!     instruction (a handler returns without adjusting anything), while `ud2`
//!     leaves the faulting address (a handler must add its two bytes). Both are
//!     asserted by *resuming*: the child continues past the instruction and
//!     reports a word set after it. A frame rebuilt with the faulting address
//!     for `#BP` would re-execute the `int3` for ever, and one rebuilt with
//!     `rip + 2` for `#UD` would skip an instruction the program never wrote;
//!   * `#DE`'s handler cannot resume (the divisor is still zero), so its child
//!     leaves through `_exit(0)` - the claim there is that the handler ran at
//!     all, which the old entry failed.
//!
//! The Linux side of the table was measured on 7.2.14 (`build/scratch/traptrap/
//! {h,si}.c`): `int3` -> `SIGTRAP`, `si_code` 128, `si_addr` 0, saved `rip`
//! after the instruction; `ud2` -> `SIGILL`, `si_code` 2, `si_addr` = the
//! faulting `rip`; `idiv` by zero -> `SIGFPE`, `si_code` 1, `si_addr` = the
//! faulting `rip`. `MPOS_ON_HOST=1` runs this same fixture against real Linux
//! and passes, which is what makes the numbers a measurement rather than a
//! description of this kernel.
//!
//! Eight children per vector are the reliability claim: one child can get lucky
//! on a timer tick, eight in a row cannot.
//!
//! This is x86_64's own entry. The aarch64 machine has no `ESR`-to-signal map
//! yet (`docs/notes/` §6.159's named gap), so a refused fault there still dies
//! without queueing anything; the program therefore joins `tests.toml`'s
//! `[aarch64] gaps` rather than claiming an answer on that machine.

#[cfg(target_arch = "x86_64")]
use mpos_user_tests::Report;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// Which trap this child is about to take. Written before the fault and read by
/// the handler, both in the child's own address space.
#[cfg(target_arch = "x86_64")]
const MODE_BP: i32 = 0;
#[cfg(target_arch = "x86_64")]
const MODE_UD: i32 = 1;
#[cfg(target_arch = "x86_64")]
const MODE_DE: i32 = 2;

#[cfg(target_arch = "x86_64")]
static MODE: AtomicI32 = AtomicI32::new(MODE_BP);
#[cfg(target_arch = "x86_64")]
static RAN: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_CODE: AtomicI32 = AtomicI32::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_ADDR: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_RIP: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
const TRIALS: usize = 8;

/// Linux 7.2.14's `si_code` for each vector, measured through `WTERMSIG` and an
/// `SA_SIGINFO` handler (`build/scratch/traptrap/si.c`).
#[cfg(target_arch = "x86_64")]
const SI_KERNEL: i32 = 128;
#[cfg(target_arch = "x86_64")]
const ILL_ILLOPN: i32 = 2;
#[cfg(target_arch = "x86_64")]
const FPE_INTDIV: i32 = 1;

#[cfg(target_arch = "x86_64")]
extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    RAN.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        // SAFETY: `info` is the record the kernel filled for this SA_SIGINFO
        // handler, and its accessors are the machine's own.
        unsafe {
            SEEN_CODE.store((*info).si_code, Ordering::SeqCst);
            SEEN_ADDR.store((*info).si_addr() as usize as u64, Ordering::SeqCst);
        }
    }
    // The saved instruction pointer is what the frame must restore: `int3`
    // leaves the address after itself, the other two leave the faulting one.
    if !ctx.is_null() {
        // SAFETY: `ctx` is this signal's `ucontext_t`; `REG_RIP` indexes the
        // general-register block the kernel restored.
        unsafe {
            let uc = ctx as *mut libc::ucontext_t;
            SEEN_RIP.store(
                (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64,
                Ordering::SeqCst,
            );
        }
    }
    let _ = sig;
    match MODE.load(Ordering::SeqCst) {
        // `ud2` is two bytes and the CPU saved the address *of* the
        // instruction, so a handler that means to continue must step over it.
        // Linux does the same; a handler that returned here would re-execute
        // the `ud2` for ever.
        x if x == MODE_UD => {
            // SAFETY: `ctx` is the signal's own context, as above.
            unsafe {
                let uc = ctx as *mut libc::ucontext_t;
                (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] += 2;
            }
        }
        // `#DE`: the divisor is still zero, so nothing can be repaired and the
        // handler leaves the process rather than returning into the same trap.
        x if x == MODE_DE => unsafe { libc::_exit(0) },
        // `#BP`: the CPU pushed the address *after* the instruction, so a plain
        // return is the resume.
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn arm() {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    for sig in [libc::SIGTRAP, libc::SIGILL, libc::SIGFPE] {
        // SAFETY: a fully filled `sigaction` for numbers the wrapper serves;
        // musl fills the restorer slot itself (`t-signal-delivery`'s doc).
        let rc = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
        assert!(rc == 0, "installing a handler failed");
    }
}

/// One child's whole claim, as an exit code: 0 = every claim held, 10 + i says
/// which one did not.
#[cfg(target_arch = "x86_64")]
fn child(mode: i32) -> i32 {
    RAN.store(0, Ordering::SeqCst);
    SEEN_CODE.store(0, Ordering::SeqCst);
    SEEN_ADDR.store(0, Ordering::SeqCst);
    SEEN_RIP.store(0, Ordering::SeqCst);
    MODE.store(mode, Ordering::SeqCst);
    arm();

    let mut after: u64 = 0;
    match mode {
        x if x == MODE_BP => {
            // SAFETY: `int3` traps by definition; the block clobbers nothing
            // and the handler's return must land on the next instruction.
            unsafe {
                std::arch::asm!(
                    "int3",
                    "mov {a}, 1",
                    a = out(reg) after,
                    options(nostack)
                );
            }
        }
        x if x == MODE_UD => {
            // SAFETY: `ud2` is an unconditional invalid-opcode trap. The
            // handler adds two bytes to the saved `rip`, so the resume lands on
            // the instruction after it and `after` is written.
            unsafe {
                std::arch::asm!(
                    "ud2",
                    "mov {a}, 1",
                    a = out(reg) after,
                    options(nostack)
                );
            }
        }
        _ => {
            // SAFETY: `idiv` by a zero divisor raises `#DE`; the handler never
            // returns, so nothing after this line runs.
            unsafe {
                std::arch::asm!(
                    "xor edx, edx",
                    "mov eax, 1",
                    "xor ecx, ecx",
                    "idiv ecx",
                    lateout("eax") _,
                    lateout("edx") _,
                    lateout("ecx") _,
                    options(nostack)
                );
            }
        }
    }
    // Nothing above may be reordered past these reads of words the handler
    // wrote (the same barrier the `#PF` pin needed).
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    let ran = RAN.load(Ordering::SeqCst);
    let code = SEEN_CODE.load(Ordering::SeqCst);
    let addr = SEEN_ADDR.load(Ordering::SeqCst);
    let rip = SEEN_RIP.load(Ordering::SeqCst);

    if ran != 1 {
        return 10;
    }
    let expected = match mode {
        x if x == MODE_BP => SI_KERNEL,
        x if x == MODE_UD => ILL_ILLOPN,
        _ => FPE_INTDIV,
    };
    if code != expected {
        return 11;
    }
    match mode {
        // `#BP` has no faulting address (the CPU hands over the address after
        // the instruction, and nothing the program wrote faulted), so Linux
        // reports zero even though the saved `rip` is a real address.
        x if x == MODE_BP => {
            if addr != 0 {
                return 12;
            }
            if after != 1 {
                return 13;
            }
        }
        // The two real faults name the instruction: `si_addr` *is* the saved
        // `rip`, which is what a handler reads to find out where it was.
        _ => {
            if addr != rip {
                return 12;
            }
            if mode == MODE_UD && after != 1 {
                return 13;
            }
        }
    }
    0
}

fn main() {
    #[cfg(not(target_arch = "x86_64"))]
    {
        println!("t-trap-at-trap: skipped (the exception entries are x86_64's own)");
    }
    #[cfg(target_arch = "x86_64")]
    x86_main();
}

#[cfg(target_arch = "x86_64")]
fn x86_main() {
    println!("t-trap-at-trap");
    let mut r = Report::new();
    for (mode, name) in [
        (MODE_BP, "int3 (#BP)"),
        (MODE_UD, "ud2 (#UD)"),
        (MODE_DE, "idiv by zero (#DE)"),
    ] {
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
                unsafe { libc::_exit(child(mode)) };
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
            &format!(
                "{name}: all {TRIALS} handled traps entered their handler at the trap \
                 and resumed where Linux resumes (good={good} died={died} wrong={other})"
            ),
            good == TRIALS && died == 0 && other == 0,
        );
    }
    r.finish();
}
