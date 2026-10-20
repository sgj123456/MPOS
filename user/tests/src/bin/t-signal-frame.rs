//! t-signal-frame - the frame a handler runs on, and what it is handed.
//!
//! A signal handler is an *asynchronous* call: the program did not ask for it,
//! so the kernel has to hand the handler everything the interrupted code was
//! using and put it all back afterwards. This program is the pin for that on
//! both machines, and every check in it is a fact a real program branches on.
//!
//! The signal comes from a **sibling process**, and that is deliberate rather
//! than convenient: a signal this program raises at itself would be delivered
//! at the end of the `kill` call it made, while a sibling's arrives while the
//! program is parked in a `read` - the shape almost every real program is in.
//! What the pin then holds, in order:
//!
//! * `x0`/`rdi` is the signal number, and the `siginfo_t` names the sender
//!   (`si_code` `SI_USER`, `si_pid`, `si_uid`) - the three fields a handler
//!   branches on to tell a deliberate signal from a fault;
//! * `ucontext->uc_mcontext.pc` is the interrupted instruction itself, which is
//!   the address the kernel read out of the trap and wrote down: on aarch64 the
//!   instruction after the `svc`, and on x86_64 the one after `syscall` when
//!   nothing is being rewound;
//! * **every register the trapped call was using comes back** - the
//!   callee-saved integers and, on both machines, every vector register. The
//!   handler destroys the caller-saved vector registers as soon as it has read
//!   the frame, so only the frame's own record of the interrupted state can
//!   return them: aarch64's `fpsimd_context` in `uc_mcontext.__reserved`, and
//!   x86_64's `_fpstate`, which `sigcontext.fpstate` points at.
//! * **`SA_RESTART` really rewinds the call**: the same `read`, with a handler
//!   installed with the flag, is interrupted and then *runs again* - the second
//!   execution reads the bytes the sibling writes afterwards, which it can only
//!   do if every argument came back and the frame's `pc` was pointed at the
//!   trapped instruction rather than after it;
//! * **a fatal signal is a wait status**, not a mystery: a child that raises
//!   `SIGABRT`/`SIGSEGV` is reported to its parent as `WIFSIGNALED` with that
//!   signal, which is what every shell and supervisor reads.
//!
//! The trapped call is made from this program's own inline assembly, with the
//! registers stamped around it, because that is the only way to *hold* those
//! registers across it: a Rust call in between would be testing the compiler's
//! register allocator rather than the kernel. It also gives the pin its anchor -
//! the address of the trapped instruction, taken with the same assembler that
//! emitted it.

use std::process::exit;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_arch = "aarch64")]
use std::arch::asm;
#[cfg(target_arch = "x86_64")]
use std::arch::asm;

/// The stamp the registers are filled with; the same value on both machines.
const PATTERN: u64 = 0x0123_4567_89ab_cdef;

/// The signal the sibling raises.
const SIG: libc::c_int = libc::SIGUSR1;

/// `si_code` for a signal sent by `kill(2)`.
const SI_USER: i32 = 0;

// ---------------------------------------------------------------------------
// What the handler saw. The handler runs on the interrupted program's stack,
// while it is inside `read`, so the two share nothing but these words.
// ---------------------------------------------------------------------------

static RUNS: AtomicUsize = AtomicUsize::new(0);
static SEEN_SIGNO: AtomicUsize = AtomicUsize::new(0);
static SEEN_CODE: AtomicUsize = AtomicUsize::new(0);
static SEEN_PID: AtomicUsize = AtomicUsize::new(0);
static SEEN_UID: AtomicUsize = AtomicUsize::new(0);
static SEEN_PC: AtomicUsize = AtomicUsize::new(0);
/// `uc_mcontext.regs[29]`/`regs[30]` - the interrupted frame pointer and link
/// register - and the handler's own `x29`, read before anything can move it.
#[cfg(target_arch = "aarch64")]
static SEEN_FP: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "aarch64")]
static SEEN_LR: AtomicUsize = AtomicUsize::new(0);
/// Whether the handler's own frame chain reached the interrupted frame, walked
/// *inside* the handler - the only place it can be read. The frame lives below
/// `sp`, so the program is entitled to use that stack again the moment the
/// handler returns, and by the time `main` looks it holds something else.
#[cfg(target_arch = "aarch64")]
static SEEN_CHAIN: AtomicUsize = AtomicUsize::new(0);
static SEEN_HANDLER_FP: AtomicUsize = AtomicUsize::new(0);
/// What the `fpsimd_context` record in `uc_mcontext.__reserved` held.
#[cfg(target_arch = "aarch64")]
static SEEN_RECORD_MAGIC: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "aarch64")]
static SEEN_RECORD_SIZE: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "aarch64")]
static SEEN_RECORD_V8: AtomicUsize = AtomicUsize::new(0);

/// What `uc_mcontext.fpregs` named, and what the `_fpstate` it pointed at held:
/// the pointer itself, its alignment (a `fxsave`/`fxrstor` operand must be
/// 16-byte aligned), how many of the sixteen `xmm_space` slots held the stamp,
/// and the first slot that did not.
#[cfg(target_arch = "x86_64")]
static SEEN_FP_PTR: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_FP_ALIGN: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_FP_MATCHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_arch = "x86_64")]
static SEEN_FP_FIRST: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Set for the one delivery whose handler takes the image *away* again: the
/// `sigcontext.fpstate` word is nulled before returning, which Linux answers by
/// resetting the floating-point state rather than leaving the handler's
/// registers live.
#[cfg(target_arch = "x86_64")]
static NULL_FPSTATE: AtomicUsize = AtomicUsize::new(0);

/// The `uc_mcontext` a handler is handed, read out of `ucontext` the way the
/// operand's libc says it is laid out.
fn read_context(uc: *mut libc::c_void) {
    if uc.is_null() {
        return;
    }
    // SAFETY: `uc` is the `ucontext_t *` the kernel passed to a handler, which
    // is `libc::ucontext_t` by definition - the same structure the libc's own
    // `sigaction` documentation describes.
    let uc = uc as *mut libc::ucontext_t;
    #[cfg(target_arch = "aarch64")]
    unsafe {
        SEEN_PC.store((*uc).uc_mcontext.pc as usize, Ordering::SeqCst);
        SEEN_FP.store((*uc).uc_mcontext.regs[29] as usize, Ordering::SeqCst);
        SEEN_LR.store((*uc).uc_mcontext.regs[30] as usize, Ordering::SeqCst);
        // `uc_mcontext.__reserved`, where the kernel's record chain begins.
        // `libc`'s Rust view of `mcontext_t` loses the alignment of musl's
        // `long double __reserved[256]` (its `Padding<[u64; 512]>` is 8-aligned),
        // so the offset is computed here from the structure's own start: the
        // `sigcontext` fields end at 288, where the 16-aligned region begins.
        let reserved = (core::ptr::addr_of!((*uc).uc_mcontext) as *const u8).add(288);
        SEEN_RECORD_MAGIC.store(
            u32::from_le_bytes([
                *reserved,
                *reserved.add(1),
                *reserved.add(2),
                *reserved.add(3),
            ]) as usize,
            Ordering::SeqCst,
        );
        SEEN_RECORD_SIZE.store(
            u32::from_le_bytes([
                *reserved.add(4),
                *reserved.add(5),
                *reserved.add(6),
                *reserved.add(7),
            ]) as usize,
            Ordering::SeqCst,
        );
        // `struct fpsimd_context`: head, `fpsr`, `fpcr`, then `vregs`. Register
        // 8 is a 16-byte slot, and the stamp went into its low half.
        let v8 = reserved.add(16 + 8 * 16);
        let mut low = [0u8; 8];
        for (i, byte) in low.iter_mut().enumerate() {
            *byte = *v8.add(i);
        }
        SEEN_RECORD_V8.store(u64::from_le_bytes(low) as usize, Ordering::SeqCst);
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        SEEN_PC.store(
            (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as usize,
            Ordering::SeqCst,
        );
        // `struct sigcontext.fpstate` is a *pointer* on this machine: the
        // interrupted floating-point image is a separate area, which this kernel
        // puts at the frame's own end and points this word at. A null word is
        // Linux's "no image carried", so a non-null one is the first thing the
        // frame owes a handler that reads it.
        //
        // The word itself is at 184: twenty-three 8-byte general registers, then
        // the four 16-bit segment words, then
        // `err`/`trapno`/`oldmask`/`cr2`. The crate's `mcontext_t` loses the
        // name - its tail is a private array - so the offset is spelled here,
        // and it is the same one `arch/src/arch/x86_64/signal.rs` writes the
        // pointer through. Measured with `offsetof` against glibc's `<ucontext.h>`
        // on Linux 7.2: `uc_mcontext.fpregs` is 224 bytes into the `ucontext`,
        // and `uc_mcontext` is 40 in.
        let mcontext = core::ptr::addr_of!((*uc).uc_mcontext) as *const u8;
        let fp = mcontext.add(184).cast::<usize>().read() as *const u8;
        SEEN_FP_PTR.store(fp as usize, Ordering::SeqCst);
        SEEN_FP_ALIGN.store(fp as usize & 15, Ordering::SeqCst);
        if !fp.is_null() {
            // `struct _fpstate`'s field order is `fxsave`'s: `cwd`/`swd`/`twd`/
            // `fop`, `rip`, `rdp`, `mxcsr`/`mxcr_mask`, `st_space[32]`, then
            // `xmm_space[64]` at 160 - sixteen 16-byte slots, one per xmm
            // register, low half first.
            let mut matched = 0usize;
            let mut first = usize::MAX;
            for i in 0..16 {
                let slot = fp.add(160 + 16 * i);
                let mut low = [0u8; 8];
                for (j, byte) in low.iter_mut().enumerate() {
                    *byte = *slot.add(j);
                }
                if u64::from_le_bytes(low) == PATTERN {
                    matched += 1;
                } else if first == usize::MAX {
                    first = i;
                }
            }
            SEEN_FP_MATCHED.store(matched, Ordering::SeqCst);
            SEEN_FP_FIRST.store(first, Ordering::SeqCst);
        }
    }
}

/// The handler the parent installs: `SA_SIGINFO`, so it is handed the
/// `siginfo_t` and the `ucontext` the frame carries.
extern "C" fn handler(signo: libc::c_int, info: *mut libc::siginfo_t, uc: *mut libc::c_void) {
    SEEN_HANDLER_FP.store(frame_pointer() as usize, Ordering::SeqCst);
    RUNS.fetch_add(1, Ordering::SeqCst);
    SEEN_SIGNO.store(signo as usize, Ordering::SeqCst);
    unsafe {
        if !info.is_null() {
            SEEN_CODE.store((*info).si_code as usize, Ordering::SeqCst);
            // SAFETY: the accessors read the `_sifields` union at the offset
            // the ABI puts it at, which `t-rt-sigtimedwait` pins for both
            // machines as well.
            SEEN_PID.store((*info).si_pid() as usize, Ordering::SeqCst);
            SEEN_UID.store((*info).si_uid() as usize, Ordering::SeqCst);
        }
    }
    read_context(uc);
    // Now that the interrupted `x29`/`x30` are known, follow the chain the
    // kernel aimed this handler's `x29` at - *here*, because that is the only
    // moment those two words are the frame's.
    #[cfg(target_arch = "aarch64")]
    {
        let interrupted_fp = SEEN_FP.load(Ordering::SeqCst) as u64;
        let interrupted_lr = SEEN_LR.load(Ordering::SeqCst) as u64;
        let mut here = SEEN_HANDLER_FP.load(Ordering::SeqCst) as *const usize;
        // Two hops, and both are real: the handler is a Rust function, so `x29`
        // is either the record the kernel wrote - when the function needs no
        // frame of its own - or what its own prologue saved, with the record
        // one link out.
        for _ in 0..2 {
            if here.is_null() {
                break;
            }
            // SAFETY: the chain the kernel aimed `x29` at, one link at a time.
            let record = unsafe { core::slice::from_raw_parts(here, 2) };
            if record[0] as u64 == interrupted_fp && record[1] as u64 == interrupted_lr {
                SEEN_CHAIN.store(1, Ordering::SeqCst);
                break;
            }
            here = record[0] as *const usize;
        }
    }
    // Everything the frame owed is read. One delivery deliberately gives it
    // back: a null `sigcontext.fpstate` is a frame that names no image, and the
    // kernel's answer on Linux is to reset the floating-point state, not to
    // leave the handler's registers live (measured: zeroed xmm and `mxcsr`
    // 0x1f80). Nulling the word here is what makes `rt_sigreturn` take that
    // path, and the caller's stamps are what say whether it did.
    #[cfg(target_arch = "x86_64")]
    if NULL_FPSTATE.load(Ordering::SeqCst) != 0 && !uc.is_null() {
        // SAFETY: `uc` is the frame's `ucontext`, which lives above the
        // handler's own stack and is the caller's to scribble on; `fpstate` is
        // its word at 224 (see `read_context`).
        unsafe { (uc as *mut u8).add(224).cast::<usize>().write(0) };
    }
    // Now destroy the vector registers, the way any ordinary function may: every
    // one of them is caller-saved here, so the interrupted code is only entitled
    // to them back if the frame kept a copy.
    clobber_vector_registers();
}

/// Wipe every vector register the interrupted code could have been holding.
///
/// A handler is an asynchronous call, and the machine's vector registers are all
/// caller-saved: nothing in the ABI stops a handler - or a library it calls -
/// from using them. So the only way an interrupted loop keeps its accumulators
/// across a signal is the kernel having saved them in the frame. Writing a value
/// that is *not* the stamp is what makes that observable after `rt_sigreturn`.
///
/// The callee-saved half (`v8..v15`) may be put back by the compiler on the way
/// out of this function, and that is fine: the caller-saved half is dead by
/// definition and nothing will restore it, so a frame that carried no image is
/// caught either way.
#[cfg(target_arch = "aarch64")]
fn clobber_vector_registers() {
    // SAFETY: the block only writes registers it declares, none of which is an
    // operand of anything live across it.
    unsafe {
        asm!(
            "movi v0.16b, #0", "movi v1.16b, #0", "movi v2.16b, #0", "movi v3.16b, #0",
            "movi v4.16b, #0", "movi v5.16b, #0", "movi v6.16b, #0", "movi v7.16b, #0",
            "movi v8.16b, #0", "movi v9.16b, #0", "movi v10.16b, #0", "movi v11.16b, #0",
            "movi v12.16b, #0", "movi v13.16b, #0", "movi v14.16b, #0", "movi v15.16b, #0",
            "movi v16.16b, #0", "movi v17.16b, #0", "movi v18.16b, #0", "movi v19.16b, #0",
            "movi v20.16b, #0", "movi v21.16b, #0", "movi v22.16b, #0", "movi v23.16b, #0",
            "movi v24.16b, #0", "movi v25.16b, #0", "movi v26.16b, #0", "movi v27.16b, #0",
            "movi v28.16b, #0", "movi v29.16b, #0", "movi v30.16b, #0", "movi v31.16b, #0",
            out("v0") _, out("v1") _, out("v2") _, out("v3") _,
            out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _,
            out("v28") _, out("v29") _, out("v30") _, out("v31") _,
        );
    }
}

/// The same, on the first machine: `pcmpeqd xmmN, xmmN` fills each register with
/// ones, a value the stamp is not.
#[cfg(target_arch = "x86_64")]
fn clobber_vector_registers() {
    // SAFETY: as the aarch64 block.
    unsafe {
        asm!(
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1", "pcmpeqd xmm2, xmm2",
            "pcmpeqd xmm3, xmm3", "pcmpeqd xmm4, xmm4", "pcmpeqd xmm5, xmm5",
            "pcmpeqd xmm6, xmm6", "pcmpeqd xmm7, xmm7", "pcmpeqd xmm8, xmm8",
            "pcmpeqd xmm9, xmm9", "pcmpeqd xmm10, xmm10", "pcmpeqd xmm11, xmm11",
            "pcmpeqd xmm12, xmm12", "pcmpeqd xmm13, xmm13", "pcmpeqd xmm14, xmm14",
            "pcmpeqd xmm15, xmm15",
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
        );
    }
}

/// The handler's own `x29`. The kernel aims it at the frame's unwind record, so
/// a backtrace started here walks into the interrupted code.
#[cfg(target_arch = "aarch64")]
fn frame_pointer() -> u64 {
    let fp: u64;
    // SAFETY: reading `x29` has no side effects.
    unsafe {
        asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags));
    }
    fp
}

#[cfg(not(target_arch = "aarch64"))]
fn frame_pointer() -> u64 {
    0
}

/// The registers a stamped call gives back: a name and a value each.
type Stamp = Vec<(&'static str, u64)>;

/// Where the trapped instruction is, what the call answered, and the registers
/// as they came back from it.
struct Trapped {
    /// The address of the `svc`/`syscall` itself, which the frame's `pc` must
    /// name (or name plus the instruction's width, when the call is not
    /// rewound).
    at: u64,
    ret: i64,
    stamp: Stamp,
}

/// The machine's own width for the instruction that traps: `svc` is four bytes
/// on aarch64 and `syscall` is two on x86_64, and the trap leaves the return
/// address *after* it - which is exactly the number `SA_RESTART` rewinds.
#[cfg(target_arch = "aarch64")]
const TRAP_INSTRUCTION_BYTES: u64 = 4;
#[cfg(target_arch = "x86_64")]
const TRAP_INSTRUCTION_BYTES: u64 = 2;

/// A blocking `read(2)` made from this program's own code, with every register
/// the kernel must give back stamped around it.
///
/// One `asm!` block holds the stamp, the trap and the read-back, because the
/// registers have to survive *between* them and any Rust in the middle would be
/// the compiler's business rather than the kernel's. The operands are what
/// stamps them: an `inout` register is written before the block and read after
/// it, so what this returns is what the trap left behind.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn stamped_blocking_read(fd: libc::c_int, buf: *mut u8, len: usize) -> Trapped {
    let nr = libc::SYS_read as u64;
    let (at, ret);
    let (r20, r21, r22, r23, r24, r25, r26, r27, r28);
    let (
        v0,
        v1,
        v2,
        v3,
        v4,
        v5,
        v6,
        v7,
        v8,
        v9,
        v10,
        v11,
        v12,
        v13,
        v14,
        v15,
        v16,
        v17,
        v18,
        v19,
        v20,
        v21,
        v22,
        v23,
        v24,
        v25,
        v26,
        v27,
        v28,
        v29,
        v30,
        v31,
    );
    // SAFETY: the block is one `svc` with a valid number and arguments this
    // process owns; every register it touches is named as an operand.
    unsafe {
        asm!(
            "adr {at}, 2f",
            "2:",
            "svc #0",
            // `x8` must be *declared*, not just written: `mov x8, ...` with an
            // undeclared `x8` would let the compiler keep a live value there,
            // and the trap would hand the program a corrupted register - which
            // is exactly the bug this pin exists to catch, one level up.
            in("x8") nr,
            at = out(reg) at,
            inlateout("x0") fd as i64 => ret,
            in("x1") buf,
            in("x2") len,
            // `x19` is LLVM's own and cannot be an operand; the rest of the
            // callee-saved set is here, and the vector registers below are the
            // half the frame's `fpsimd_context` record exists for.
            inout("x20") PATTERN => r20,
            inout("x21") PATTERN => r21,
            inout("x22") PATTERN => r22,
            inout("x23") PATTERN => r23,
            inout("x24") PATTERN => r24,
            inout("x25") PATTERN => r25,
            inout("x26") PATTERN => r26,
            inout("x27") PATTERN => r27,
            inout("x28") PATTERN => r28,
            inout("v0") PATTERN => v0,
            inout("v1") PATTERN => v1,
            inout("v2") PATTERN => v2,
            inout("v3") PATTERN => v3,
            inout("v4") PATTERN => v4,
            inout("v5") PATTERN => v5,
            inout("v6") PATTERN => v6,
            inout("v7") PATTERN => v7,
            inout("v8") PATTERN => v8,
            inout("v9") PATTERN => v9,
            inout("v10") PATTERN => v10,
            inout("v11") PATTERN => v11,
            inout("v12") PATTERN => v12,
            inout("v13") PATTERN => v13,
            inout("v14") PATTERN => v14,
            inout("v15") PATTERN => v15,
            inout("v16") PATTERN => v16,
            inout("v17") PATTERN => v17,
            inout("v18") PATTERN => v18,
            inout("v19") PATTERN => v19,
            inout("v20") PATTERN => v20,
            inout("v21") PATTERN => v21,
            inout("v22") PATTERN => v22,
            inout("v23") PATTERN => v23,
            inout("v24") PATTERN => v24,
            inout("v25") PATTERN => v25,
            inout("v26") PATTERN => v26,
            inout("v27") PATTERN => v27,
            inout("v28") PATTERN => v28,
            inout("v29") PATTERN => v29,
            inout("v30") PATTERN => v30,
            inout("v31") PATTERN => v31,
            out("x3") _,
            out("x4") _,
            out("x5") _,
            out("x6") _,
            out("x7") _,
            out("x9") _,
            out("x10") _,
            out("x11") _,
            out("x12") _,
            out("x13") _,
            out("x14") _,
            out("x15") _,
            out("x16") _,
            out("x17") _,
            out("x18") _,
            out("x30") _,
        );
    }
    let mut stamp: Stamp = vec![
        ("x20", r20),
        ("x21", r21),
        ("x22", r22),
        ("x23", r23),
        ("x24", r24),
        ("x25", r25),
        ("x26", r26),
        ("x27", r27),
        ("x28", r28),
    ];
    let names = [
        "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "v13",
        "v14", "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23", "v24", "v25", "v26",
        "v27", "v28", "v29", "v30", "v31",
    ];
    let values = [
        v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15, v16, v17, v18, v19,
        v20, v21, v22, v23, v24, v25, v26, v27, v28, v29, v30, v31,
    ];
    for (name, value) in names.into_iter().zip(values) {
        stamp.push((name, value));
    }
    Trapped { at, ret, stamp }
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn stamped_blocking_read(fd: libc::c_int, buf: *mut u8, len: usize) -> Trapped {
    let nr = libc::SYS_read as u64;
    let (at, ret);
    let (r12, r13, r14, r15);
    let (x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15);
    // SAFETY: as the aarch64 block.
    unsafe {
        asm!(
            "lea {at}, [rip + 2f]",
            "2:",
            "syscall",
            at = out(reg) at,
            inlateout("rax") nr => ret,
            in("rdi") fd as i64,
            in("rsi") buf,
            in("rdx") len,
            // `rbx` is LLVM's own and cannot be an operand, and `rbp` is the
            // frame pointer; the rest of the callee-saved set is here.
            inout("r12") PATTERN => r12,
            inout("r13") PATTERN => r13,
            inout("r14") PATTERN => r14,
            inout("r15") PATTERN => r15,
            inout("xmm0") PATTERN => x0,
            inout("xmm1") PATTERN => x1,
            inout("xmm2") PATTERN => x2,
            inout("xmm3") PATTERN => x3,
            inout("xmm4") PATTERN => x4,
            inout("xmm5") PATTERN => x5,
            inout("xmm6") PATTERN => x6,
            inout("xmm7") PATTERN => x7,
            inout("xmm8") PATTERN => x8,
            inout("xmm9") PATTERN => x9,
            inout("xmm10") PATTERN => x10,
            inout("xmm11") PATTERN => x11,
            inout("xmm12") PATTERN => x12,
            inout("xmm13") PATTERN => x13,
            inout("xmm14") PATTERN => x14,
            inout("xmm15") PATTERN => x15,
            out("rcx") _,
            out("r11") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
        );
    }
    let stamp: Stamp = vec![
        ("r12", r12),
        ("r13", r13),
        ("r14", r14),
        ("r15", r15),
        ("xmm0", x0),
        ("xmm1", x1),
        ("xmm2", x2),
        ("xmm3", x3),
        ("xmm4", x4),
        ("xmm5", x5),
        ("xmm6", x6),
        ("xmm7", x7),
        ("xmm8", x8),
        ("xmm9", x9),
        ("xmm10", x10),
        ("xmm11", x11),
        ("xmm12", x12),
        ("xmm13", x13),
        ("xmm14", x14),
        ("xmm15", x15),
    ];
    Trapped { at, ret, stamp }
}

/// A sibling process that raises the signal and then, separately, puts bytes in
/// the pipe - the two facts `SA_RESTART` tells apart.
struct Sibling {
    read_fd: libc::c_int,
    pid: libc::pid_t,
}

fn spawn_sibling() -> Sibling {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a two-word array this process owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        panic!("pipe: {}", std::io::Error::last_os_error());
    }
    // SAFETY: the child either `_exit`s or writes to descriptors it owns; it
    // shares nothing mutable with the parent.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The parent reaches its `read` first, so the signal really lands on a
        // parked task; then the handler has time to run before the bytes land,
        // which is what makes the second `read` meaningful.
        sleep_ms(100);
        // SAFETY: `getppid`/`kill` on this process's parent.
        unsafe {
            libc::kill(libc::getppid(), SIG);
        }
        sleep_ms(150);
        let bytes = b"ping";
        // SAFETY: `fds[1]` is the write end this child owns.
        unsafe {
            libc::write(fds[1], bytes.as_ptr().cast(), 4);
            libc::_exit(0);
        }
    }
    if pid < 0 {
        panic!("fork: {}", std::io::Error::last_os_error());
    }
    // SAFETY: the parent keeps only the read end.
    unsafe { libc::close(fds[1]) };
    Sibling {
        read_fd: fds[0],
        pid,
    }
}

fn sleep_ms(ms: i64) {
    let ts = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: `ts` is a `timespec` this call owns, and the second pointer is
    // null ("do not tell me the remainder").
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
}

/// Install the handler, with or without `SA_RESTART`.
fn install(with_restart: bool) {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    // The ABI's own shape: a handler address in the `sa_sigaction` slot.
    action.sa_sigaction = handler as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO | if with_restart { libc::SA_RESTART } else { 0 };
    // SAFETY: `action` is a fully filled `sigaction` for a catchable signal.
    let rc = unsafe { libc::sigaction(SIG, &action, std::ptr::null_mut()) };
    if rc != 0 {
        panic!("sigaction: {}", std::io::Error::last_os_error());
    }
}

/// The report shape the std-side tests share.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_eq(&mut self, name: &str, got: u64, want: u64) {
        self.check(
            &format!("{name} (got {got:#x}, want {want:#x})"),
            got == want,
        );
    }
}

fn main() {
    println!("t-signal-frame");
    let mut r = Report { failed: false };
    let uid = unsafe { libc::getuid() };

    // --- an asynchronous signal, with no rewind ---------------------------
    install(false);
    RUNS.store(0, Ordering::SeqCst);
    let sibling = spawn_sibling();
    let mut buf = [0u8; 8];
    let trapped = stamped_blocking_read(sibling.read_fd, buf.as_mut_ptr(), buf.len());

    r.check(
        &format!(
            "a signal from a sibling ended the parked read (ret {}, want {})",
            trapped.ret,
            -(libc::EINTR as i64)
        ),
        trapped.ret == -(libc::EINTR as i64),
    );
    r.check_eq(
        "the handler ran once",
        RUNS.load(Ordering::SeqCst) as u64,
        1,
    );
    r.check_eq(
        "and was handed the signal number",
        SEEN_SIGNO.load(Ordering::SeqCst) as u64,
        SIG as u64,
    );
    r.check_eq(
        "with si_code naming a deliberate sender",
        SEEN_CODE.load(Ordering::SeqCst) as u64,
        SI_USER as u64,
    );
    r.check_eq(
        "and si_pid naming the sibling",
        SEEN_PID.load(Ordering::SeqCst) as u64,
        sibling.pid as u64,
    );
    r.check_eq(
        "and si_uid this process's uid",
        SEEN_UID.load(Ordering::SeqCst) as u64,
        uid as u64,
    );
    r.check_eq(
        "the frame records the instruction after the trapped svc",
        SEEN_PC.load(Ordering::SeqCst) as u64,
        trapped.at + TRAP_INSTRUCTION_BYTES,
    );

    // Every register the trapped call was using came back.
    let broken: Vec<String> = trapped
        .stamp
        .iter()
        .filter(|(_, got)| *got != PATTERN)
        .map(|(name, got)| format!("{name} = {got:#018x}"))
        .collect();
    r.check(
        &format!(
            "all {} stamped registers came back ({})",
            trapped.stamp.len(),
            if broken.is_empty() {
                "none lost".to_string()
            } else {
                broken.join(", ")
            }
        ),
        broken.is_empty(),
    );

    // The machine-specific half: where the frame keeps the interrupted
    // floating-point state, and what it holds. Both machines carry it; the ABI
    // puts it in a different shape, and each shape is read where its own libc
    // says it is.
    #[cfg(target_arch = "aarch64")]
    {
        r.check(
            &format!(
                "the handler's frame chain reaches the interrupted frame \
                 (x29 {x29:#x}, interrupted fp {fp:#x}, lr {lr:#x})",
                x29 = SEEN_HANDLER_FP.load(Ordering::SeqCst),
                fp = SEEN_FP.load(Ordering::SeqCst),
                lr = SEEN_LR.load(Ordering::SeqCst),
            ),
            SEEN_CHAIN.load(Ordering::SeqCst) == 1,
        );
        r.check_eq(
            "the floating-point record's magic is FPSIMD_MAGIC",
            SEEN_RECORD_MAGIC.load(Ordering::SeqCst) as u64,
            0x4650_8001,
        );
        r.check_eq(
            "and its size is the record's own 528",
            SEEN_RECORD_SIZE.load(Ordering::SeqCst) as u64,
            528,
        );
        r.check_eq(
            "and it holds the interrupted v8",
            SEEN_RECORD_V8.load(Ordering::SeqCst) as u64,
            PATTERN,
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        // `sigcontext.fpstate` names the image rather than holding it, so the
        // first question is whether there is one at all - a handler that reads
        // `uc_mcontext.fpregs` must not be handed a null.
        r.check(
            &format!(
                "the frame names a floating-point image (fpstate {:#x})",
                SEEN_FP_PTR.load(Ordering::SeqCst)
            ),
            SEEN_FP_PTR.load(Ordering::SeqCst) != 0,
        );
        // `fxrstor` takes a 16-byte-aligned operand, and a libc that keeps a
        // ucontext and later loads it back with `setcontext` is entitled to use
        // the pointer this way.
        r.check_eq(
            "and it is 16-byte aligned",
            SEEN_FP_ALIGN.load(Ordering::SeqCst) as u64,
            0,
        );
        // `struct _fpstate` is the `fxsave` block, so its `xmm_space` at 160 is
        // where the interrupted xmm registers are; the handler read them before
        // destroying the live ones.
        let matched = SEEN_FP_MATCHED.load(Ordering::SeqCst);
        let how = if matched == 16 {
            "all sixteen held the stamp".to_string()
        } else {
            format!(
                "{matched} did, first missing #{}",
                SEEN_FP_FIRST.load(Ordering::SeqCst)
            )
        };
        r.check(
            &format!("and its xmm_space holds the interrupted xmm registers ({how})"),
            matched == 16,
        );
    }

    // Drain what the sibling wrote and collect it, so the next session starts
    // from a clean machine.
    // SAFETY: the read end this process owns, and a buffer it owns.
    let drained = unsafe { libc::read(sibling.read_fd, buf.as_mut_ptr().cast(), 8) };
    r.check(
        &format!("the sibling's bytes were still in the pipe ({drained})"),
        drained == 4 && &buf[..4] == b"ping",
    );
    // SAFETY: the read end this process owns.
    unsafe { libc::close(sibling.read_fd) };
    let mut status = 0 as libc::c_int;
    // SAFETY: a child this process forked, and a status word it owns.
    let reaped = unsafe { libc::waitpid(sibling.pid, &mut status, 0) };
    r.check("the sibling is reaped", reaped == sibling.pid);

    // --- the same signal, with SA_RESTART ---------------------------------
    install(true);
    RUNS.store(0, Ordering::SeqCst);
    let sibling = spawn_sibling();
    let mut buf = [0u8; 8];
    let trapped = stamped_blocking_read(sibling.read_fd, buf.as_mut_ptr(), buf.len());
    r.check(
        &format!(
            "with SA_RESTART the interrupted read ran again and got the bytes (ret {}, buf {:?})",
            trapped.ret,
            core::str::from_utf8(&buf[..4.min(buf.len())]).unwrap_or("?")
        ),
        trapped.ret == 4 && &buf[..4] == b"ping",
    );
    r.check_eq(
        "the handler still ran once",
        RUNS.load(Ordering::SeqCst) as u64,
        1,
    );
    r.check_eq(
        "and the frame it was handed points at the trapped instruction itself",
        SEEN_PC.load(Ordering::SeqCst) as u64,
        trapped.at,
    );
    let broken: Vec<String> = trapped
        .stamp
        .iter()
        .filter(|(_, got)| *got != PATTERN)
        .map(|(name, got)| format!("{name} = {got:#018x}"))
        .collect();
    r.check(
        &format!(
            "and the rewound call gave the same {} registers back ({})",
            trapped.stamp.len(),
            if broken.is_empty() {
                "none lost".to_string()
            } else {
                broken.join(", ")
            }
        ),
        broken.is_empty(),
    );
    // SAFETY: the read end this process owns.
    unsafe { libc::close(sibling.read_fd) };
    // SAFETY: a child this process forked, and a status word it owns.
    let reaped = unsafe { libc::waitpid(sibling.pid, &mut status, 0) };
    r.check("and the second sibling is reaped", reaped == sibling.pid);

    // --- a frame that names no floating-point image (x86_64) ---------------
    //
    // `sigcontext.fpstate` is a pointer, and a null one is a frame that carries
    // no image. Linux's `rt_sigreturn` answers that by resetting the
    // floating-point state rather than leaving the handler's registers live: the
    // probe run behind this test's `NO_RESTORE` measurement came back with
    // zeroed xmm registers and `mxcsr` 0x1f80, the architectural init state.
    // Leaving the handler's values in place instead would corrupt the interrupted
    // code silently, which is the failure this whole frame exists to prevent.
    #[cfg(target_arch = "x86_64")]
    {
        NULL_FPSTATE.store(1, Ordering::SeqCst);
        install(false);
        RUNS.store(0, Ordering::SeqCst);
        let sibling = spawn_sibling();
        let mut buf = [0u8; 8];
        let trapped = stamped_blocking_read(sibling.read_fd, buf.as_mut_ptr(), buf.len());
        r.check(
            "a signal still ends the read when its handler strips the image",
            trapped.ret == -(libc::EINTR as i64),
        );
        let mut vectors_live = Vec::new();
        let mut integers_lost = Vec::new();
        for (name, got) in &trapped.stamp {
            if name.starts_with("xmm") {
                if *got != 0 {
                    vectors_live.push(format!("{name} = {got:#x}"));
                }
            } else if *got != PATTERN {
                integers_lost.push(format!("{name} = {got:#x}"));
            }
        }
        r.check(
            &format!(
                "and the frame's missing image resets the vector registers ({})",
                if vectors_live.is_empty() {
                    "all cleared".to_string()
                } else {
                    vectors_live.join(", ")
                }
            ),
            vectors_live.is_empty(),
        );
        r.check(
            &format!(
                "while the integer file came back all the same ({})",
                if integers_lost.is_empty() {
                    "none lost".to_string()
                } else {
                    integers_lost.join(", ")
                }
            ),
            integers_lost.is_empty(),
        );
        NULL_FPSTATE.store(0, Ordering::SeqCst);
        // SAFETY: the read end this process owns.
        unsafe { libc::close(sibling.read_fd) };
        // SAFETY: a child this process forked, and a status word it owns.
        let reaped = unsafe { libc::waitpid(sibling.pid, &mut status, 0) };
        r.check("and the third sibling is reaped", reaped == sibling.pid);
    }

    // --- a fatal signal is a wait status ----------------------------------
    for (name, signal) in [("SIGABRT", libc::SIGABRT), ("SIGSEGV", libc::SIGSEGV)] {
        // SAFETY: the child raises the signal at itself and dies of it; the
        // parent waits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::raise(signal);
                libc::_exit(0);
            }
        }
        let mut status = 0 as libc::c_int;
        // SAFETY: a child this process forked, and a status word it owns.
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        r.check(
            &format!(
                "a child that dies of {name} is reported as WIFSIGNALED with WTERMSIG {name} (status {status:#x})"
            ),
            reaped == pid
                && libc::WIFSIGNALED(status)
                && libc::WTERMSIG(status) == signal,
        );
    }

    if r.failed {
        println!("t-signal-frame: FAILED");
        exit(1);
    }
    println!("t-signal-frame: all passed");
}
