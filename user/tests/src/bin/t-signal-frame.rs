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
//!   callee-saved integers and, on aarch64, all thirty-two vector registers.
//!   The vector half is the counterfactual for the signal frame's
//!   `fpsimd_context` record: the handler is free to destroy the caller-saved
//!   ones, and only `rt_sigreturn` reading the record back into the trap frame
//!   returns them. (x86_64's frame carries no floating-point image at all - its
//!   `fpstate` is a pointer this kernel leaves null - so that half is checked
//!   on the second machine only, and the gap is named in `PLAN.md`.)
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

    // The aarch64-only half: the unwind record and the floating-point record
    // the frame carries. x86_64's `uc_mcontext` has a null `fpstate` pointer
    // and no record chain, so neither exists to check there - a named gap.
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
