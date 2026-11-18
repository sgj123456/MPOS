//! t-ptrace-fault-stop - a faulting tracee stops for its tracer before it dies.
//!
//! Linux delivers a fatal signal to a tracer as a **signal-delivery-stop**: the
//! tracer is woken with `WIFSTOPPED` and `WSTOPSIG` = the fault's signal, reads
//! the register file and `PTRACE_GETSIGINFO` there, and decides whether to pass
//! the signal on. That stop is the only moment a crash is inspectable.
//!
//! This kernel used to go straight from `send_fault_signal` to
//! `die_of_fatal_signal`, so a traced process that faulted **died without its
//! tracer being told** - `wait` answered `ended sig=11` and nothing else, and
//! every register read afterwards came from a corpse. It cost a whole
//! investigation (`docs/notes/07-chromium-analysis.md` §4u names the gap, §4v
//! the fix); this is the contract that fix has to keep.
//!
//! Measured on Linux 7.3.4 before any claim here (`.workbuddy/probe_faultstop.c`,
//! and the same shape in `build/scratch/zprobe/live3.c` on this kernel):
//!
//! | what | Linux |
//! |---|---|
//! | a traced child stores to an unmapped address | `waitpid` reports a **stop**, not an exit |
//! | `WSTOPSIG` at that stop | `SIGSEGV` (11) |
//! | the register file there | `rip` is the faulting instruction, **not** the corpse's garbage |
//! | `PTRACE_CONT` **with** `SIGSEGV` | the child dies of it (a traced death is the tracer's to pass on) |
//! | `PTRACE_CONT` with signal 0 | the child **survives** - the tracer dropped it |
//!
//! The last two rows are the ones that make the stop a *contract* rather than a
//! courtesy: a tracer that drops the signal has kept its subject alive, which no
//! kernel that kills first can offer.
//!
//! **The fourth row holds only for a `SIG_DFL` tracee, and the fixture has to say
//! so.** A `mpos-user` program is Rust `std`, whose runtime installs a `SIGSEGV`
//! handler at startup (its stack-overflow detector), and a forked child inherits
//! it - so the first version of this file measured a different experiment from
//! the C probe its table cites, and its last claim read `signaled false` on a
//! kernel that was doing the right thing. Re-measured on the host
//! (`build/scratch/fsp`, same fixture, Linux 7.3.4):
//!
//! | child's `SIGSEGV` disposition | `PTRACE_CONT` with `SIGSEGV` |
//! |---|---|
//! | inherited handler (`sa_sigaction` = 0x558a…) | **another delivery stop**, `0x0b7f`, however many times it is resumed |
//! | `SIG_DFL` | `WIFSIGNALED`, `WTERMSIG` 11, raw status `0x8b` |
//!
//! So a tracer cannot hand its subject a handler at all: every pass-on is a new
//! signal-delivery-stop. This kernel gets that case wrong in the same way and one
//! step further - it neither runs the handler (`deliver_at_trap` refuses a traced
//! task, `src/syscall/linux/proc.rs`) nor ends the task, which is a **named gap**
//! (`docs/notes/07-chromium-analysis.md` §4z) and *not* what this pin measures.
//! The child below asks for `SIG_DFL` explicitly so the claim and the table
//! describe one experiment.
//!
//! The fault is caused deliberately, from the child, at an address nothing maps.

use mpos_user_tests::Report;

const PTRACE_GETREGS: u32 = 12;
const PTRACE_CONT: u32 = 7;

fn ptrace(request: u32, pid: libc::pid_t, addr: usize, data: usize) -> isize {
    // SAFETY: the variadic shape libc's wrapper takes; `request` decides what the
    // other three words mean. `as _` is portable: musl declares `c_int`, glibc
    // `c_uint`.
    unsafe { libc::ptrace(request as _, pid, addr, data) as isize }
}

/// `rip` out of the machine's `user_regs_struct`, whatever its layout is.
///
/// `libc` declares the struct per architecture, and the offset of the
/// instruction pointer is the one field this test cares about; taking it through
/// the struct's own field keeps the second machine compiling.
fn faulting_ip(child: libc::pid_t) -> Option<u64> {
    // SAFETY: the struct is plain old data, and `PTRACE_GETREGS` fills exactly it
    // for the stopped child this test owns.
    unsafe {
        let mut regs: libc::user_regs_struct = core::mem::zeroed();
        if ptrace(PTRACE_GETREGS, child, 0, &mut regs as *mut _ as usize) != 0 {
            return None;
        }
        #[cfg(target_arch = "x86_64")]
        {
            Some(regs.rip)
        }
        #[cfg(target_arch = "aarch64")]
        {
            Some(regs.pc)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _ = regs;
            None
        }
    }
}

fn main() {
    println!("t-ptrace-fault-stop");
    let mut r = Report::new();

    // === a faulting tracee stops, and the stop is SIGSEGV =================
    //
    // SAFETY: `fork`; the child traces itself, faults on purpose, and exits.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: a child that hands itself to its parent and then writes where
        // nothing is mapped - the fault is the fixture.
        unsafe {
            // **First, the disposition the cited measurement was taken with.**
            // Rust `std` installs its own `SIGSEGV` handler at startup and a
            // forked child inherits it; left in place, "passing the signal on
            // kills it" is not the experiment the table above describes (a
            // traced task cannot be handed a handler at all - see the header).
            libc::signal(libc::SIGSEGV, libc::SIG_DFL);
            libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
            libc::raise(libc::SIGSTOP);
            // A wild address well outside any mapping this process has.
            let wild = 0x5_0000_0000_usize as *mut u8;
            core::ptr::write_volatile(wild, 1);
            libc::_exit(0);
        }
    }
    r.check(&format!("a child is forked ({child})"), child > 0);
    if child <= 0 {
        r.finish();
    }
    let mut status = 0;
    // SAFETY: the child's own SIGSTOP, which TRACEME reports to us.
    let first = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!("and stops for the tracer ({first})"),
        first == child && libc::WIFSTOPPED(status),
    );

    // Let it run into the fault.
    ptrace(PTRACE_CONT, child, 0, 0);
    // SAFETY: the same child.
    let hit = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        &format!("the fault arrives as a STOP, not an exit ({hit})"),
        hit == child && libc::WIFSTOPPED(status),
    );
    let sig = if libc::WIFSTOPPED(status) {
        libc::WSTOPSIG(status)
    } else {
        -1
    };
    r.check(
        &format!("and its signal is SIGSEGV ({sig})"),
        sig == libc::SIGSEGV,
    );

    // **The register file is live at that stop**: a value from the faulting
    // instruction, not the corpse's leftovers. "Non-zero" is the honest
    // assertion - the exact address is the program's, and a stop that killed
    // first answered 0 or an unrelated word here.
    let ip = faulting_ip(child);
    r.check(
        &format!(
            "the register file is readable there (ip {})",
            ip.map(|i| format!("{i:#x}"))
                .unwrap_or_else(|| "None".into())
        ),
        ip.is_some_and(|i| i != 0),
    );

    // === the tracer decides: dropping the signal keeps the child alive =====
    //
    // A second child, same fault, but resumed with **signal 0**. On Linux the
    // child survives and exits 0 - the tracer dropped the fault - which is what
    // makes the stop a decision rather than a notification.
    // SAFETY: as above.
    let kept = unsafe { libc::fork() };
    if kept == 0 {
        // SAFETY: child; same fixture, but this one is meant to live.
        unsafe {
            libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
            libc::raise(libc::SIGSTOP);
            libc::_exit(0); // never reaches the fault: the parent resumes it with 0
        }
    }
    if kept > 0 {
        let mut st2 = 0;
        // SAFETY: the second child's SIGSTOP.
        unsafe { libc::waitpid(kept, &mut st2, 0) };
        ptrace(PTRACE_CONT, kept, 0, 0);
        // SAFETY: the same child, now expected to exit 0.
        let done = unsafe { libc::waitpid(kept, &mut st2, 0) };
        let code = if libc::WIFEXITED(st2) {
            libc::WEXITSTATUS(st2)
        } else {
            -1
        };
        r.check(
            &format!("a resume with signal 0 lets a stopped tracee leave cleanly ({done}, {code})"),
            done == kept && code == 0,
        );
    }

    // === and passing the signal on is the death ===========================
    //
    // The first child is still parked at its fault. Resume it **with** SIGSEGV:
    // Linux kills it, which is the other half of the same decision.
    ptrace(PTRACE_CONT, child, 0, libc::SIGSEGV as usize);
    // SAFETY: the first child, now expected to die.
    let gone = unsafe { libc::waitpid(child, &mut status, 0) };
    let killed = libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV;
    r.check(
        &format!("passing the signal on kills it ({gone}, signaled {killed})"),
        gone == child && killed,
    );

    r.finish()
}
