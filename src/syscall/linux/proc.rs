//! Linux-compatible syscall handlers: the ones that need a running machine.
//! Memory: `mmap`, `munmap`, `mremap`, `mprotect`, `brk`. Time: `nanosleep`,
//! `clock_gettime`, `gettimeofday`, `setitimer`. Identity: `getpid`, `gettid`,
//! `getppid`, `kill`/`tkill`. Process: `execve`, `exit`/`exit_group`, `wait4`,
//! `fork` (clone), `uname`, `futex`, process groups.
//!
//! The calls whose whole behaviour is argument shaping and table answers -
//! `prctl`, `umask`, `getrandom`, the rlimit family, the affinity and
//! registration calls - live in [`super::abi`], where the host tests reach
//! them; this module keeps the parts that sleep, switch, or touch the
//! machine.

use alloc::string::String;
use alloc::sync::Arc;

use super::abi;
use super::nr;
use crate::sched::TaskId;
use crate::syscall::{ARG_COUNT, argv_arg, buffer_ok, cstr_arg, err, err_of};

pub(super) use crate::syscall::MMAP_BASE;

/// The register frame on the kernel stack, matching the layout pushed by the
/// `syscall` entry stub in `arch/x86_64/syscall.rs`. Fields are named after
/// the user registers they hold; offsets must match the push order exactly.
/// [`handle_execve`] patches the return address and stack pointer through it,
/// and [`handle_fork`] reads the whole thing to rebuild the child's register
/// file.
///
/// Keep this in step with `Regs` in `arch/x86_64/syscall.rs` - the two describe
/// one frame, and the stub's epilogue is written against those offsets. The
/// `_filler` below is the stub's extra word, a second copy of `rax` pushed so
/// the nine `pop`s in the epilogue stop exactly on the caller's RSP; leaving it
/// out here would make every field above it name the word below the one it
/// means, and a `fork` child would resume with its parent's `rdi` in `r12` and
/// its `rsi` as the return address.
/// The user instruction pointer of the call being dispatched: the `rcx` the
/// syscall gate saved, which is where `sysretq` will resume. `seccomp_data`
/// carries it, and a filter that inspects it - one checking which of its own
/// call sites is running - reads the value the kernel recorded rather than a
/// guess.
#[cfg(target_os = "none")]
pub(super) fn user_ip(regs_ptr: *mut ()) -> u64 {
    let frame = unsafe { &*(regs_ptr as *const RegsFrame) };
    frame.rcx as u64
}

#[repr(C)]
struct RegsFrame {
    rax: usize,      // [rsp+0]   return value / syscall number
    r11: usize,      // [rsp+8]   user RFLAGS (sysretq restores from here)
    rcx: usize,      // [rsp+16]  user RIP (sysretq restores from here)
    r9: usize,       // [rsp+24]
    r8: usize,       // [rsp+32]
    r10: usize,      // [rsp+40]
    rdx: usize,      // [rsp+48]
    rsi: usize,      // [rsp+56]
    rdi: usize,      // [rsp+64]
    user_rsp: usize, // [rsp+72]  user RSP (restored from here at sysretq)
    /// Not a register: the stub's padding word, never read back. See the
    /// comment on `Regs`.
    _filler: usize, // [rsp+80]
    r12: usize,      // [rsp+88]
    r15: usize,      // [rsp+96]
    r14: usize,      // [rsp+104]
    r13: usize,      // [rsp+112]
    rbx: usize,      // [rsp+120]
    rbp: usize,      // [rsp+128]
}

// ---- signals: the frame a handler runs on -------------------------------
//
// A handler cannot be entered by a `jmp`: it has to be able to `ret` back into
// the kernel, and the kernel has to be able to restore a register file that no
// longer exists by then. Both halves of that are a structure written on the
// process's own stack and read back by `rt_sigreturn`, in a layout the libc's
// trampoline knows by heart. Everything below is that layout.

/// Byte offsets of the fields of `struct sigcontext`, in the Linux x86_64
/// order. They are not a choice: `__restore_rt` hands the structure back to
/// `rt_sigreturn`, and this kernel reads the saved registers from exactly
/// where the libc expects them to be.
mod sc {
    pub const R8: usize = 0;
    pub const R9: usize = 8;
    pub const R10: usize = 16;
    pub const R12: usize = 32;
    pub const R13: usize = 40;
    pub const R14: usize = 48;
    pub const R15: usize = 56;
    pub const RDI: usize = 64;
    pub const RSI: usize = 72;
    pub const RBP: usize = 80;
    pub const RBX: usize = 88;
    pub const RDX: usize = 96;
    pub const RAX: usize = 104;
    pub const RSP: usize = 120;
    pub const RIP: usize = 128;
    pub const EFLAGS: usize = 136;
    /// The structure is padded out to here, whatever this kernel fills in.
    pub const BYTES: usize = 256;
}

/// `SI_USER`: the signal came from `kill`, not from the hardware. It is what a
/// handler reads out of `si_code` to tell a deliberate signal from a fault.
const SI_USER: i32 = 0;

fn put_word(bytes: &mut [u8], at: usize, value: usize) {
    bytes[at..at + 8].copy_from_slice(&(value as u64).to_le_bytes());
}

fn put_dword(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_word(bytes: &[u8], at: usize) -> usize {
    usize::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Write the frame a handler will run on and point the task at it.
///
/// `restart` is the call number to run again after the handler returns, or
/// `None` when this delivery resumes the interrupted code instead - see
/// [`restart_verdict`]. A restart is what `SA_RESTART` asks for, and the frame
/// is the whole of how it happens: `rt_sigreturn` puts the registers back, so
/// pointing the saved `rip` back at the `syscall` instruction with the number
/// in `rax` *is* the rewind. The arguments need nothing extra - they are still
/// in the registers this frame saves (the handler has not run yet, and it runs
/// on its own copy of them), which is why `rt_sigreturn` hands back the
/// caller's original arguments for free.
///
/// Returns `false` when the frame cannot be written - the stack is not mapped,
/// or not writable - which leaves the caller to take the default action rather
/// than to lose the signal.
fn build_frame(
    regs_ptr: *mut (),
    sig: u8,
    action: &crate::sched::signal::Action,
    info: &crate::sched::signal::SigInfo,
    ret: isize,
    restart: Option<u64>,
) -> bool {
    use crate::sched::signal::{
        SA_ONSTACK, SA_RESETHAND, SA_RESTORER, SIGFRAME_BYTES, SIGFRAME_SIGINFO, SIGFRAME_UCONTEXT,
        UCONTEXT_MCONTEXT, UCONTEXT_SIGMASK,
    };

    let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };

    // A handler runs on the process's own stack, unless the disposition asked
    // for the alternate one. Either way the frame goes just below where the
    // stack pointer was: the interrupted code gets its memory back untouched
    // when the handler returns.
    let mut sp = regs.user_rsp;
    let alt = crate::sched::signal_alt_stack(crate::sched::current());
    if action.flags & SA_ONSTACK != 0
        && let Some(stack) = alt
    {
        sp = stack.sp + stack.size;
    }

    // A function is entered with its return address at `rsp % 16 == 8`; a
    // handler is entered the same way, with the restorer standing in for the
    // `call` that would have put it there.
    let frame = ((sp - SIGFRAME_BYTES) & !15) - 8;

    let mut bytes = [0u8; SIGFRAME_BYTES];
    // The restorer - where the handler's `ret` goes. Without it the handler
    // would return into whatever it found on the stack, and the process could
    // never leave the handler at all.
    if action.flags & SA_RESTORER == 0 || action.restorer == 0 {
        // The ABI makes this field mandatory on x86_64; a disposition without
        // one cannot be run.
        return false;
    }
    put_word(&mut bytes, 0, action.restorer);

    // The saved register file. `rax` is the value the interrupted syscall
    // returned, which is what `rt_sigreturn` puts back.
    let mcontext = SIGFRAME_UCONTEXT + UCONTEXT_MCONTEXT;
    put_word(&mut bytes, mcontext + sc::R8, regs.r8);
    put_word(&mut bytes, mcontext + sc::R9, regs.r9);
    put_word(&mut bytes, mcontext + sc::R10, regs.r10);
    put_word(&mut bytes, mcontext + sc::R12, regs.r12);
    put_word(&mut bytes, mcontext + sc::R13, regs.r13);
    put_word(&mut bytes, mcontext + sc::R14, regs.r14);
    put_word(&mut bytes, mcontext + sc::R15, regs.r15);
    put_word(&mut bytes, mcontext + sc::RDI, regs.rdi);
    put_word(&mut bytes, mcontext + sc::RSI, regs.rsi);
    put_word(&mut bytes, mcontext + sc::RBP, regs.rbp);
    put_word(&mut bytes, mcontext + sc::RBX, regs.rbx);
    put_word(&mut bytes, mcontext + sc::RDX, regs.rdx);
    // The interrupted call's answer, or - for a call the disposition asked to
    // run again - the call's number, which is what `syscall` reads when the
    // rewound frame resumes on it.
    put_word(
        &mut bytes,
        mcontext + sc::RAX,
        restart.map_or(ret as usize, |nr| nr as usize),
    );
    // rcx and r11 were destroyed by the `syscall` that is being interrupted:
    // the CPU puts the return address in rcx and the flags in r11 on the way
    // in, so there is nothing of the caller's left to save.
    put_word(&mut bytes, mcontext + sc::RSP, regs.user_rsp);
    // The `syscall` instruction is two bytes (`0f 05`), so the return address
    // the CPU left in `rcx` is two past the instruction a restart resumes on.
    put_word(
        &mut bytes,
        mcontext + sc::RIP,
        restart.map_or(regs.rcx, |_| regs.rcx.wrapping_sub(2)),
    );
    put_word(&mut bytes, mcontext + sc::EFLAGS, regs.r11);

    // `uc_stack` describes the alternate stack, as the handler will see it
    // through `sigaltstack(NULL, ...)`.
    let stack_at = SIGFRAME_UCONTEXT + 16;
    if let Some(stack) = alt {
        put_word(&mut bytes, stack_at, stack.sp);
        put_word(&mut bytes, stack_at + 16, stack.size);
    }

    // The mask to put back on the way out - what the process had blocked when
    // the signal arrived. A libc reads this as the wire's `sigset_t`, so it
    // goes out through the same conversion `rt_sigprocmask` brings its sets in
    // through; `rt_sigreturn` converts on the way back.
    let blocked = crate::sched::current_blocked();
    put_word(
        &mut bytes,
        SIGFRAME_UCONTEXT + UCONTEXT_SIGMASK,
        crate::sched::signal::to_wire(blocked) as usize,
    );

    // A `siginfo_t`: signo, errno, code, and - for a signal that names a
    // sender - the pid and uid, with the `sigqueue` payload in the union at
    // offset 24. The offsets are Linux x86_64's, so a handler written against
    // `<signal.h>` reads the right fields.
    let si = SIGFRAME_SIGINFO;
    bytes[si..si + 4].copy_from_slice(&info.signo.to_le_bytes());
    bytes[si + 4..si + 8].copy_from_slice(&info.errno.to_le_bytes());
    bytes[si + 8..si + 12].copy_from_slice(&info.code.to_le_bytes());
    put_dword(&mut bytes, si + 16, info.pid);
    put_dword(&mut bytes, si + 20, info.uid);
    put_word(&mut bytes, si + 24, info.value as usize);

    if !crate::sched::write_user(frame, &bytes) {
        return false;
    }

    // Enter the handler with the three arguments the ABI gives it: the signal
    // number, a `siginfo_t`, and the `ucontext` to hand back.
    regs.rcx = action.handler;
    regs.user_rsp = frame;
    regs.rdi = sig as usize;
    regs.rsi = frame + SIGFRAME_SIGINFO;
    regs.rdx = frame + SIGFRAME_UCONTEXT;

    crate::sched::set_current_blocked(action.handler_mask(sig, blocked));
    if action.flags & SA_RESETHAND != 0 {
        crate::sched::reset_disposition(sig);
    }
    true
}

/// Whether a signal about to be delivered should run the interrupted call
/// again after its handler returns, and the call number to run.
///
/// Three things have to agree, and each is measured rather than assumed:
///
/// * the call came back `EINTR`, which in this kernel means a blocking wait
///   ended with nothing consumed. A call that succeeded - or failed for any
///   other reason - is done, and re-running it would be a second call;
/// * the disposition being delivered has `SA_RESTART`. The default harvester
///   has already run, and what is left reaching this point is either a handler
///   or an ignored signal, so the flags read here are the ones the handler
///   will run under;
/// * the call is one Linux rewinds at all ([`super::abi::restartable`]) -
///   `poll`, `select`, `epoll_wait` and the sleeps answer `EINTR` whatever the
///   disposition says, and rewinding them would be a handler loop that never
///   returns.
///
/// What the caller sees when all three hold, measured on Linux 7.2 with a
/// terminal read whose `SIGALRM` fired at 100 ms and whose byte arrived at
/// 400 ms: the handler runs at 100 ms, the *read* returns the byte at 400 ms,
/// and `EINTR` never appears. Without the flag the same call answers
/// `-1/EINTR` at 100 ms.
fn restart_verdict(nr: u64, ret: isize, action: &crate::sched::signal::Action) -> Option<u64> {
    let asked = action.flags & crate::sched::signal::SA_RESTART != 0;
    (ret == err::EINTR && asked && super::abi::restartable(nr)).then_some(nr)
}

/// Run a pending signal's handler, or take its default action.
///
/// Called on the way out of every syscall, holding the register frame the task
/// will `sysretq` through. That frame is why this is the place: entering a
/// handler means rewriting the task's whole return context, and a syscall is
/// the only moment the kernel has it in a form it can both read and write.
/// The clock can deliver a signal with no handler, but not one with a handler,
/// for exactly this reason.
///
/// Returns `true` when a handler was entered, so the caller can report a
/// reassuring zero to the interrupted call instead of its own return value -
/// which is now the handler's to see when it returns.
pub(super) fn deliver_signal(regs_ptr: *mut (), nr: u64, ret: isize) -> bool {
    if regs_ptr.is_null() {
        return false;
    }
    // A traced task's signals are its tracer's business before they are its
    // own: the tracer is told which one is about to arrive and decides whether
    // it does. The signal stays pending across that stop, so what the tracer
    // leaves behind - the same signal, a different one, or nothing at all - is
    // what the lines below go on to find.
    if crate::sched::has_tracer(crate::sched::current()) {
        let mut saved = capture_frame(regs_ptr, ret as usize);
        crate::sched::trace_signal_stop(&mut saved);
        apply_frame(regs_ptr, &saved);
    }
    let Some((sig, info)) = crate::sched::take_deliverable_siginfo() else {
        return false;
    };
    let action = crate::sched::current_disposition(sig);
    if action.is_ignore() {
        return false;
    }
    if action.is_default() {
        // With no handler the default action table decides, and it does not
        // give one answer. The great majority of signals terminate; `SIGCONT`
        // resumes and `SIGSTOP` stops. Before this kernel had a stop state to
        // be in, "no handler" and "dies of it" really were the same thing.
        match crate::sched::signal::default_action(sig) {
            crate::sched::signal::DefaultAction::Terminate => {
                crate::sched::exit(-(sig as isize));
            }
            crate::sched::signal::DefaultAction::Ignore => return false,
            // Arriving at a task that is *running*: it is already going, so the
            // signal has said everything it had to say. The state it changes -
            // ending a stop - is the sender's to change, because a stopped task
            // is not here to receive it.
            crate::sched::signal::DefaultAction::Continue => return false,
            crate::sched::signal::DefaultAction::Stop => {
                let mut saved = capture_frame(regs_ptr, ret as usize);
                crate::sched::group_stop(&mut saved, sig);
                apply_frame(regs_ptr, &saved);
                return false;
            }
        }
    }
    if !build_frame(
        regs_ptr,
        sig,
        &action,
        &info,
        ret,
        restart_verdict(nr, ret, &action),
    ) {
        // Nothing to run, or nowhere to put the frame - and the default action
        // for every signal this kernel names is to die of it, with the number
        // in the exit code so the parent can tell which one it was.
        crate::sched::exit(-(sig as isize));
    }
    true
}

/// `rt_sigreturn` — the trampoline's call, and the only way out of a handler.
///
/// The libc's `__restore_rt` is entered by the handler's `ret`, so `rsp` has
/// already moved past the restorer word; the frame therefore starts eight
/// bytes below where this call finds the stack pointer.
pub(super) fn handle_rt_sigreturn(regs_ptr: *mut ()) -> isize {
    use crate::sched::signal::{SIGFRAME_UCONTEXT, UCONTEXT_MCONTEXT, UCONTEXT_SIGMASK};

    if regs_ptr.is_null() {
        return err::EFAULT;
    }
    let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };
    let frame = regs.user_rsp.wrapping_sub(8);
    let mcontext = frame + SIGFRAME_UCONTEXT + UCONTEXT_MCONTEXT;

    let mut bytes = [0u8; sc::BYTES];
    if !crate::sched::read_user(mcontext, &mut bytes) {
        return err::EFAULT;
    }

    regs.r8 = get_word(&bytes, sc::R8);
    regs.r9 = get_word(&bytes, sc::R9);
    regs.r10 = get_word(&bytes, sc::R10);
    regs.r12 = get_word(&bytes, sc::R12);
    regs.r13 = get_word(&bytes, sc::R13);
    regs.r14 = get_word(&bytes, sc::R14);
    regs.r15 = get_word(&bytes, sc::R15);
    regs.rdi = get_word(&bytes, sc::RDI);
    regs.rsi = get_word(&bytes, sc::RSI);
    regs.rbp = get_word(&bytes, sc::RBP);
    regs.rbx = get_word(&bytes, sc::RBX);
    regs.rdx = get_word(&bytes, sc::RDX);
    // `sysretq` takes the return address from rcx and the flags from r11, and
    // no saved value can go there: whatever the process had in them before the
    // `syscall` was destroyed by the instruction itself.
    regs.rcx = get_word(&bytes, sc::RIP);
    regs.user_rsp = get_word(&bytes, sc::RSP);
    // Interrupts have to be on when the process resumes; everything else about
    // the flags is the process's own business.
    regs.r11 = get_word(&bytes, sc::EFLAGS) | 0x200;

    let mut mask = [0u8; 8];
    if crate::sched::read_user(frame + SIGFRAME_UCONTEXT + UCONTEXT_SIGMASK, &mut mask) {
        // The frame's saved mask is the wire's `sigset_t` — a libc reads it as
        // one — so it comes back through the same conversion the install went
        // out through.
        crate::sched::set_current_blocked(crate::sched::signal::from_wire(u64::from_le_bytes(
            mask,
        )));
    }

    get_word(&bytes, sc::RAX) as isize
}

/// Terminate the current process.
pub(super) fn handle_exit(args: [usize; ARG_COUNT]) -> isize {
    crate::sched::exit(args[0] as isize);
}

/// Terminate the current *process*: every thread of it, not just the caller.
///
/// This is the call a libc makes when the program ends - returning from
/// `main`, calling `exit`, dying of a signal - and Linux ends the whole thread
/// group with it. A thread that means only to end *itself* calls `exit(2)`
/// (or returns into `pthread_exit`), which leaves its siblings running.
pub(super) fn handle_exit_group(args: [usize; ARG_COUNT]) -> isize {
    crate::sched::exit_group(args[0] as isize);
}

/// Yield the CPU to the scheduler.
pub(super) fn handle_yield() -> isize {
    crate::sched::yield_now();
    0
}

/// Get the current process ID.
pub(super) fn handle_getpid() -> isize {
    // The *group's* id: a process has one pid however many threads it runs.
    // `gettid` is the call that answers a thread's own.
    crate::sched::current_tgid().0 as isize
}

/// `gettid()` - the calling *thread*'s own id.
///
/// This used to be an alias for `getpid`, which was harmless while every
/// process was one thread and became wrong the moment threads existed: a
/// thread that asks its own id must not be told the process's.
pub(super) fn handle_gettid() -> isize {
    crate::sched::current().0 as isize
}

// `prctl(option, ...)` - the miscellaneous process-control call.
//
// `PR_SET_NAME` and `PR_GET_NAME` (15/16) are the ones userland actually
// exercises here - `mdev` names itself so `ps` shows a tidy label, and a
// `ps` is not built yet. The options that would touch privileges, dump
// core flags or endianness touch things this kernel does not model, so
// they are accepted and dropped: a caller checking the exit status wants
// success, and one reading a value gets zero.
//
// `prctl` lives in `abi`: the option table is argument shaping, and that is
// the half the host tests read.

/// `set_tid_address(tidptr)` - name where this thread's exit is announced.
///
/// It answers the caller's tid, and remembers the address: a thread that
/// leaves writes zero there and wakes whoever waits on it
/// (`CLONE_CHILD_CLEARTID` and `set_tid_address` are the same promise made two
/// ways, which is why a libc uses one for the threads it creates and the other
/// for the thread it started as).
pub(super) fn handle_set_tid_address(args: [usize; ARG_COUNT]) -> isize {
    if args[0] != 0 {
        crate::sched::set_clear_tid(args[0]);
    }
    crate::sched::current().0 as isize
}

/// `wait4(pid, status, options)` — collect a child's exit.
///
/// Linux's contract, which this follows: the *return value* is the pid of the
/// child that was collected (or a negative errno), and the exit is described by
/// the word written through `status`. That split is what lets a shell tell "a
/// child exited with status 3" from "no child exited yet".
///
/// `status` is encoded the way every libc's `WIFEXITED`/`WEXITSTATUS` read it: a
/// normal exit puts its low byte in bits 15:8, and a death by signal puts the
/// signal number in bits 6:0. A `status` of 0 is legal and means "not
/// interested", which is what init passes when it only wants children reaped.
///
/// Which child is named follows the man page:
/// * `pid > 0` - that child;
/// * `pid == 0` - any child in the caller's process group; mpos has no process
///   groups, so this widens to "any child", as does `pid == -1`;
/// * any other negative pid - the same, since the group it names does not exist.
///
/// `WNOHANG` is honoured as Linux defines it: a child that has not exited yet is
/// answered with 0, not an error, and only a caller with no child at all gets
/// `ECHILD`.
///
/// What there is to report is wider than "it died". A child that was traced and
/// stopped, or that stopped on a `SIGSTOP` the caller asked to hear about with
/// `WUNTRACED`, is also an answer - and for a tracer that is the *usual* answer,
/// since a traced child spends most of its life stopped. `__WALL` and the other
/// Linux-only option bits arrive in the same word and need no interpreting here:
/// they widen which children count, and this kernel already counts every one it
/// can be asked about.
pub(super) fn handle_wait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, status, options) = (args[0] as isize, args[1], args[2]);
    let me = crate::sched::current();
    let no_hang = options & abi::WNOHANG != 0;
    // `WUNTRACED` is what a *parent* needs to hear about a stop; a *tracer* does
    // not, because a stop of its tracee is its business whether it asked or not.
    // That distinction lives in the event scan, which reports a stop that is
    // `traced` unconditionally - so what is passed here is only the parent's
    // half of the question.
    let stops = options & abi::WUNTRACED != 0;
    let continues = options & abi::WCONTINUED != 0;

    // Every child: the pid == -1 form, and every other group form this kernel
    // cannot tell apart for lack of process groups.
    if pid <= 0 {
        // A caller with no children at all has nothing to wait for: Linux
        // answers ECHILD, and blocking here would sleep forever - OpenRC
        // parks exactly that way after its last helper exits.
        let collected = if no_hang {
            crate::sched::poll_any_child_event(me, stops, continues)
        } else if crate::sched::has_children(me) {
            crate::sched::wait_any_child_event(me, stops, continues)
        } else {
            None
        };
        if let Some(event) = collected {
            return finish_wait_event(event, status, from_user);
        }
        // Nothing to report: an error only when there is nobody left to
        // wait for, and otherwise the "none yet" zero.
        if no_hang && crate::sched::has_children(me) {
            return 0;
        }
        return err::ECHILD;
    }

    // A named child. The bound keeps a wild pid from indexing the table.
    if pid as usize >= crate::sched::MAX_TASKS {
        return err::ECHILD;
    }
    let id = TaskId(pid as usize);

    // A stop first, then a death: a tracee that stopped on its way to exiting
    // has news of both, and the stop is the earlier news and the one that
    // gates everything after it.
    if let Some(event) = crate::sched::take_child_event(id, stops, continues) {
        return finish_wait_event(event, status, from_user);
    }
    // A code already on file means the child is a zombie: it is collected now,
    // blocking or not. Tracer-owned records stay with the tracer: this caller
    // collecting through a tracer's corpse is the double-report that hung
    // strace.
    if let Some(code) = crate::sched::take_exit_code_current(id) {
        return finish_wait(id, code, status, from_user);
    }
    if !crate::sched::is_child(id, me) {
        return err::ECHILD;
    }
    if no_hang {
        return 0;
    }

    match crate::sched::block_until_child_event(id, stops, continues) {
        Some(event) => finish_wait_event(event, status, from_user),
        None => err::ECHILD,
    }
}

/// The three shapes a successful `wait` returns, each encoded into `*status` the
/// way its `W`-macro decodes it: a death, a stop, or a resume.
fn finish_wait_event(event: crate::sched::WaitEvent, status: usize, from_user: bool) -> isize {
    match event {
        crate::sched::WaitEvent::Exited { id, code } => finish_wait(id, code, status, from_user),
        crate::sched::WaitEvent::Stopped { id, signal, event } => {
            write_wait_status(id, abi::stop_status(signal, event), status, from_user)
        }
        crate::sched::WaitEvent::Continued { id } => {
            write_wait_status(id, abi::CONTINUED_STATUS, status, from_user)
        }
    }
}

/// Encode an already-formed status word and answer the child's pid. The
/// counterpart of [`finish_wait`] for the two events that are not a death.
fn write_wait_status(id: TaskId, word: i32, status: usize, from_user: bool) -> isize {
    if status != 0 {
        if !buffer_ok(status, 4, true, from_user) {
            return err::EFAULT;
        }
        unsafe { (status as *mut i32).write_unaligned(word) };
    }
    id.0 as isize
}

/// The two outputs of a successful `wait`: the child's pid as the return value,
/// and the exit encoded into `*status`. A null `status` is "not interested" and
/// writes nothing, which is what the man page says.
fn finish_wait(id: TaskId, code: isize, status: usize, from_user: bool) -> isize {
    if status != 0 {
        if !buffer_ok(status, 4, true, from_user) {
            return err::EFAULT;
        }
        // The W-exit-status encoding is the ABI the libc's macros decode;
        // it is spelled in `abi`, tested on the host.
        unsafe { (status as *mut i32).write_unaligned(abi::wait_status(code)) };
    }
    id.0 as isize
}

// ---------------------------------------------------------------------------
// ptrace(2): the tracer's half
// ---------------------------------------------------------------------------

/// The tracee's user register file, in `user_regs_struct` order, as the gate's
/// frame will restore it.
///
/// Two slots do not hold what their names say. `syscall` parked the caller's
/// return address in `rcx` and its flags in `r11` - the instruction destroys
/// both registers - so those two frame words are `RIP` and `EFLAGS` in the
/// structure a tracer reads. Taking them at face value would hand `strace` a
/// `rip` that is really a flags word.
///
/// `rax` is passed in rather than read: at a syscall entry the frame's `rax`
/// holds the call's number, and at an exit it holds that *same* number, because
/// the gate writes the result into the slot only after the dispatch has
/// returned. The caller is the only one that knows which value belongs there.
fn capture_frame(regs_ptr: *mut (), rax: usize) -> [u64; abi::uregs::COUNT] {
    use abi::uregs::*;

    let frame = unsafe { &*(regs_ptr as *const RegsFrame) };
    let (code, data) = crate::arch::gdt::user_selectors();
    let mut regs = [0u64; COUNT];
    regs[R15] = frame.r15 as u64;
    regs[R14] = frame.r14 as u64;
    regs[R13] = frame.r13 as u64;
    regs[R12] = frame.r12 as u64;
    regs[RBP] = frame.rbp as u64;
    regs[RBX] = frame.rbx as u64;
    regs[R11] = frame.r11 as u64;
    regs[R10] = frame.r10 as u64;
    regs[R9] = frame.r9 as u64;
    regs[R8] = frame.r8 as u64;
    regs[RAX] = rax as u64;
    regs[RCX] = frame.rcx as u64;
    regs[RDX] = frame.rdx as u64;
    regs[RSI] = frame.rsi as u64;
    regs[RDI] = frame.rdi as u64;
    regs[ORIG_RAX] = frame.rax as u64;
    regs[RIP] = frame.rcx as u64;
    regs[CS] = code.0 as u64;
    regs[EFLAGS] = frame.r11 as u64;
    regs[RSP] = frame.user_rsp as u64;
    regs[SS] = data.0 as u64;
    // The thread-local base is a CPU register, not a frame word: the gate
    // reinstalls it from the task on every switch, so the task is where it is
    // read from - and where a `PTRACE_SETREGS` puts it back.
    regs[FS_BASE] = crate::sched::current_tls() as u64;
    // The segment registers and the user GS base, written out rather than left
    // to the array's zero: long mode ignores DS, ES, FS and GS - their contents
    // select nothing - and nothing here ever sets a user GS base, so zero is
    // what the CPU would ignore anyway. FS *is* meaningful, and it is not a
    // segment at all but the thread-local base above; a tracer reading this
    // structure sees the base in `FS_BASE` and a selector it may ignore in `FS`.
    regs[DS] = 0;
    regs[ES] = 0;
    regs[FS] = 0;
    regs[GS] = 0;
    regs[GS_BASE] = 0;
    regs
}

/// Write a register file back into the gate's frame and answer the value the
/// gate should return to ring 3.
///
/// The inverse of [`capture_frame`], including the two swapped slots: `rcx` gets
/// `RIP` because `sysretq` takes the return address from there, and `r11` gets
/// `EFLAGS` for the same reason. `rax` is *returned* instead of written, and
/// that is not a shortcut: the gate stores the dispatch's result into that slot
/// after this function's caller returns, so the only way a tracer's
/// `PTRACE_SETREGS` can change what the tracee sees in `rax` is for the new
/// value to *be* the result.
fn apply_frame(regs_ptr: *mut (), regs: &[u64; abi::uregs::COUNT]) -> usize {
    use abi::uregs::*;

    let frame = unsafe { &mut *(regs_ptr as *mut RegsFrame) };
    frame.r15 = regs[R15] as usize;
    frame.r14 = regs[R14] as usize;
    frame.r13 = regs[R13] as usize;
    frame.r12 = regs[R12] as usize;
    frame.rbp = regs[RBP] as usize;
    frame.rbx = regs[RBX] as usize;
    frame.r11 = regs[EFLAGS] as usize;
    frame.r10 = regs[R10] as usize;
    frame.r9 = regs[R9] as usize;
    frame.r8 = regs[R8] as usize;
    frame.rcx = regs[RIP] as usize;
    frame.rdx = regs[RDX] as usize;
    frame.rsi = regs[RSI] as usize;
    frame.rdi = regs[RDI] as usize;
    frame.user_rsp = regs[RSP] as usize;
    if crate::sched::current_tls() != regs[FS_BASE] as usize {
        crate::sched::set_current_tls(regs[FS_BASE] as usize);
    }
    regs[RAX] as usize
}

/// The tracee's half of a syscall-entry stop: show the tracer the call before it
/// is made, and answer the arguments the tracee will actually make it with.
///
/// Re-reading the argument registers rather than returning the ones handed in is
/// the point of the exercise: a tracer that rewrote `rdi` at the stop meant to
/// change the call, and a kernel that then made the call with the old value
/// would be ignoring it.
pub(crate) fn syscall_entry_stop(
    regs_ptr: *mut (),
    nr: u64,
    args: [usize; ARG_COUNT],
) -> [usize; ARG_COUNT] {
    if regs_ptr.is_null() || !crate::sched::has_tracer(crate::sched::current()) {
        return args;
    }
    let (ip, sp) = {
        let frame = unsafe { &*(regs_ptr as *const RegsFrame) };
        (frame.rcx as u64, frame.user_rsp as u64)
    };
    let mut regs = capture_frame(regs_ptr, nr as usize);
    let wide = args.map(|arg| arg as u64);
    crate::sched::stop_at_syscall(true, &mut regs, nr, &wide, ip, sp);
    apply_frame(regs_ptr, &regs);
    let frame = unsafe { &*(regs_ptr as *const RegsFrame) };
    [
        frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
    ]
}

/// The tracee's half of a syscall-exit stop. The result goes in and comes back,
/// possibly rewritten: at this stop `rax` *is* the result, and a tracer that
/// changed it changed what the tracee sees.
pub(crate) fn syscall_exit_stop(
    regs_ptr: *mut (),
    nr: u64,
    args: [usize; ARG_COUNT],
    ret: isize,
) -> isize {
    if regs_ptr.is_null() || !crate::sched::has_tracer(crate::sched::current()) {
        return ret;
    }
    let (ip, sp) = {
        let frame = unsafe { &*(regs_ptr as *const RegsFrame) };
        (frame.rcx as u64, frame.user_rsp as u64)
    };
    let mut regs = capture_frame(regs_ptr, ret as usize);
    let wide = args.map(|arg| arg as u64);
    crate::sched::stop_at_syscall(false, &mut regs, nr, &wide, ip, sp);
    apply_frame(regs_ptr, &regs) as isize
}

/// Read one `struct iovec` - a base and a length - out of the caller's memory.
fn read_iovec(at: usize) -> Option<(usize, usize)> {
    let mut raw = [0u8; 16];
    if !crate::sched::read_user(at, &mut raw) {
        return None;
    }
    Some((
        usize::from_le_bytes(raw[0..8].try_into().unwrap()),
        usize::from_le_bytes(raw[8..16].try_into().unwrap()),
    ))
}

/// `ptrace(request, pid, addr, data)` — one task observing and steering another.
///
/// The request word is the call's whole shape: `pid` is the target, and `addr`
/// and `data` mean whatever that particular request says they mean. What is
/// implemented here is the subset a tracer actually reaches - everything
/// `strace` and a debugger need for syscall tracing, memory reading and register
/// editing - and the requests beyond it answer `EIO`, which is Linux's answer
/// for a request it does not know.
///
/// The tracee does the stopping; this is only the tracer's half, and the two
/// meet in the scheduler. `PTRACE_TRACEME` writes the link from the tracee's own
/// call, `PTRACE_ATTACH` writes it from here and stops the target, and from then
/// on every syscall boundary and every signal in the tracee's life arrives here
/// as a `wait(2)` event.
#[allow(clippy::too_many_lines)]
pub(super) fn handle_ptrace(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (request, pid, addr, data) = (args[0], args[1] as isize, args[2], args[3]);
    let me = crate::sched::current();

    if request == abi::PTRACE_TRACEME {
        // The tracee hands *itself* over, and the only tracer it may name is the
        // task that spawned it. A task with no parent - init, or a kernel task
        // that somehow got here - has nobody to hand itself to.
        return match crate::sched::parent_of(me) {
            Some(parent) if crate::sched::set_tracer(me, parent) => 0,
            _ => err::EPERM,
        };
    }

    if pid <= 0 || pid as usize >= crate::sched::MAX_TASKS {
        return err::ESRCH;
    }
    let id = TaskId(pid as usize);

    if request == abi::PTRACE_ATTACH {
        // Attaching is the pair of a `TRACEME` and a `SIGSTOP`: the link is
        // written by the tracer rather than offered by the tracee, and the stop
        // that follows is what makes the new tracer's first `wait` return.
        //
        // The permission question comes first, because it is the one Linux asks
        // before it writes anything: `ptrace_may_access`, whose policy is
        // `crate::cred`. A live task that is not this caller's thread group
        // needs a matching uid *and* a dumpable target, unless the caller holds
        // `CAP_SYS_PTRACE` - which root does, so nothing that attached before
        // stops attaching. What the check now honours is the process that made
        // itself non-dumpable with `PR_SET_DUMPABLE(0)`, which is a thing a
        // program on this machine can finally do. A pid that names no live task
        // is still `ESRCH`, and the three-way answer keeps the two errnos
        // apart.
        match crate::sched::may_inspect(id) {
            None => return err::ESRCH,
            Some(false) => return err::EPERM,
            Some(true) => {}
        }
        if !crate::sched::set_tracer(id, me) {
            return err::ESRCH;
        }
        crate::sched::send_signal(id, crate::sched::signal::SIGSTOP);
        return 0;
    }

    // Everything else has to find a link already made. The check is `ours`
    // rather than "is my tracee" because of a race that is not hypothetical:
    // a tracer sets its options in the instruction after `fork`, and the child -
    // which is the side that calls `TRACEME` - may not have reached it yet. A
    // child of the caller is close enough, since that is the only task that
    // could be calling `TRACEME` on this caller a moment later.
    let ours = crate::sched::tracer_of(id) == Some(me) || crate::sched::parent_of(id) == Some(me);
    if !ours {
        return err::ESRCH;
    }

    match request {
        // A word out of the tracee's memory. `PEEKTEXT` and `PEEKDATA` are the
        // same request to a machine without separate I and D spaces, which is
        // every machine this kernel runs on.
        abi::PTRACE_PEEKTEXT | abi::PTRACE_PEEKDATA => {
            let Some(root) = crate::sched::space_root_of(id) else {
                return err::ESRCH;
            };
            if !buffer_ok(data, 8, true, from_user) {
                return err::EFAULT;
            }
            let mut word = [0u8; 8];
            if !crate::sched::read_user_of(root, addr, &mut word) {
                return err::EIO;
            }
            unsafe { (data as *mut u64).write_unaligned(u64::from_le_bytes(word)) };
            0
        }
        // And the writing half. The value arrives in `data` by value, not
        // through a pointer - one of the few places `ptrace`'s arguments do not
        // mean what they look like.
        abi::PTRACE_POKETEXT | abi::PTRACE_POKEDATA => {
            let Some(root) = crate::sched::space_root_of(id) else {
                return err::ESRCH;
            };
            if !crate::sched::write_user_of(root, addr, &(data as u64).to_le_bytes()) {
                return err::EIO;
            }
            0
        }
        // The `user` structure's register area. `addr` is a *byte offset* into
        // it, and on x86_64 the registers are its first member, so an offset
        // inside the register file is the only one that names anything here.
        abi::PTRACE_PEEKUSER | abi::PTRACE_POKEUSER => {
            // `addr` is a caller-supplied byte offset; adding the 8-byte word
            // length must not wrap, or a huge offset slips past the bound here
            // and `regs[addr / 8]` indexes out of the register file. Overflow
            // (checked_add yields None) is out of range, same as too-large.
            if addr
                .checked_add(8)
                .is_none_or(|end| end > abi::uregs::BYTES)
            {
                return err::EIO;
            }
            let Some(mut regs) = crate::sched::trace_regs(id) else {
                return err::ESRCH;
            };
            if request == abi::PTRACE_PEEKUSER {
                if !buffer_ok(data, 8, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { (data as *mut u64).write_unaligned(regs[addr / 8]) };
                return 0;
            }
            regs[addr / 8] = data as u64;
            if crate::sched::set_trace_regs(id, &regs) {
                0
            } else {
                err::ESRCH
            }
        }
        abi::PTRACE_GETREGS => {
            let Some(regs) = crate::sched::trace_regs(id) else {
                return err::ESRCH;
            };
            if !buffer_ok(data, abi::uregs::BYTES, true, from_user) {
                return err::EFAULT;
            }
            abi::write_user_regs(data, &regs);
            0
        }
        abi::PTRACE_SETREGS => {
            if !buffer_ok(data, abi::uregs::BYTES, false, from_user) {
                return err::EFAULT;
            }
            if crate::sched::set_trace_regs(id, &abi::read_user_regs(data)) {
                0
            } else {
                err::ESRCH
            }
        }
        // The same register set, named by number and reached through an
        // `iovec`. Newer tracers prefer this spelling; `strace` tries it first
        // and falls back to `GETREGS`, so both answering is what keeps it from
        // printing an error on its way to working.
        abi::PTRACE_GETREGSET | abi::PTRACE_SETREGSET => {
            if addr != abi::NT_PRSTATUS {
                return err::EIO;
            }
            let Some((base, len)) = read_iovec(data) else {
                return err::EFAULT;
            };
            let room = len.min(abi::uregs::BYTES);
            if room < abi::uregs::BYTES {
                // A short buffer is not a request for a short answer: the
                // register set is one structure, and half of it is a tracer
                // acting on registers it did not read.
                return err::EIO;
            }
            if request == abi::PTRACE_GETREGSET {
                let Some(regs) = crate::sched::trace_regs(id) else {
                    return err::ESRCH;
                };
                if !buffer_ok(base, room, true, from_user) {
                    return err::EFAULT;
                }
                abi::write_user_regs(base, &regs);
                return 0;
            }
            if !buffer_ok(base, room, false, from_user) {
                return err::EFAULT;
            }
            if crate::sched::set_trace_regs(id, &abi::read_user_regs(base)) {
                0
            } else {
                err::ESRCH
            }
        }
        // The syscall this stop is a boundary of. `addr` is the size of the
        // buffer at `data` and not an address - the other place `ptrace`'s
        // arguments do not mean what they look like. A buffer too small for the
        // structure is `EINVAL`, because it has a union in it and a truncated
        // answer would describe the wrong thing rather than less of the right
        // one.
        abi::PTRACE_GET_SYSCALL_INFO => {
            if addr < abi::SYSCALL_INFO_BYTES {
                return err::EINVAL;
            }
            if !buffer_ok(data, abi::SYSCALL_INFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            match crate::sched::trace_syscall(id) {
                Some((entry, nr, call_args, ip, sp)) => {
                    // At an exit stop the result is in the saved register file,
                    // where `syscall_exit_stop` put it - so a tracer that
                    // rewrote `rax` sees its own value here too.
                    let ret = crate::sched::trace_regs(id)
                        .map_or(0, |regs| regs[abi::uregs::RAX] as i64)
                        as isize;
                    abi::write_syscall_info(
                        data,
                        entry,
                        nr,
                        &call_args,
                        ip,
                        sp,
                        if entry { 0 } else { ret },
                    );
                }
                None => {
                    // Not a syscall stop at all. The operation byte says exactly
                    // that, which is far more use to a tracer than the previous
                    // call's arguments offered as if they were this stop's.
                    //
                    // `ip` and `sp` are the task's own even here. The structure
                    // carries them at every stop, and a tracer reads them as the
                    // proof that the kernel filled it in: `strace`'s startup
                    // probe rejects a `NONE` answer whose `ip` or `sp` is zero:
                    //
                    //     if (psi_size < expected_none_size ||
                    //         info->op != PTRACE_SYSCALL_INFO_NONE ||
                    //         !info->arch ||
                    //         !info->instruction_pointer ||
                    //         !info->stack_pointer) {
                    //             debug_func_msg("... signal stop mismatch");
                    //
                    // and having rejected it, it stops asking for this
                    // structure for the rest of the session and assumes the
                    // stops alternate instead. That assumption is one dropped
                    // stop away from reporting every return value as the *next*
                    // call's number - which is exactly what it did.
                    let (ip, sp) = crate::sched::trace_regs(id).map_or((0, 0), |regs| {
                        (regs[abi::uregs::RIP], regs[abi::uregs::RSP])
                    });
                    abi::write_syscall_info(data, true, 0, &[0; 6], ip, sp, 0);
                    unsafe { (data as *mut u8).write_unaligned(abi::SYSCALL_INFO_NONE) };
                }
            }
            abi::SYSCALL_INFO_BYTES as isize
        }
        abi::PTRACE_SETOPTIONS => {
            // The bits this kernel understands. A request naming one it does
            // not is `EINVAL` rather than a shrug: the tracer would otherwise
            // wait for a stop that would never come, which is the worst kind of
            // wrong answer.
            const KNOWN: usize = abi::PTRACE_O_TRACESYSGOOD
                | abi::PTRACE_O_TRACEFORK
                | abi::PTRACE_O_TRACEVFORK
                | abi::PTRACE_O_TRACECLONE
                | abi::PTRACE_O_TRACEEXEC
                | abi::PTRACE_O_TRACEVFORKDONE
                | abi::PTRACE_O_TRACEEXIT
                | abi::PTRACE_O_EXITKILL;
            if data & !KNOWN != 0 {
                return err::EINVAL;
            }
            if crate::sched::set_trace_options(id, data) {
                0
            } else {
                err::ESRCH
            }
        }
        // The message an event carries, which the stop itself holds: a fork, a
        // vfork or a clone reports the *new child's* pid there, an exec the tid
        // the task had before it exec'd, and every other kind of stop carries
        // nothing because there is no event for a message to belong to.
        //
        // Reading it off the stop rather than off the task is what makes the
        // answer belong to the question. A tracer asks this while the tracee
        // sits in a particular stop, and a task that has been stopped twice has
        // two different answers - only the second of which is still true of it.
        //
        // The value is an `unsigned long`, not the four bytes a pid needs:
        // `ptrace(2)` says so, and a tracer that declares it as one has eight
        // bytes there to be written.
        abi::PTRACE_GETEVENTMSG => {
            let Some(stop) = crate::sched::stop_of(id) else {
                return err::ESRCH;
            };
            if !buffer_ok(data, 8, true, from_user) {
                return err::EFAULT;
            }
            unsafe { (data as *mut u64).write_unaligned(stop.message) };
            0
        }
        // The `siginfo_t` of the stop.
        //
        // Which kind of stop it is *is* the `si_code`, and that is not
        // decoration: a tracer tells the three kinds of `SIGTRAP` stop apart by
        // it. A ptrace event carries `SIGTRAP | (event << 8)`, a syscall
        // boundary carries plain `SIGTRAP`, and a signal that was sent to the
        // tracee carries the code of whoever sent it.
        //
        // Answering `SI_USER` for all three - which this used to - hands a
        // tracer an exec event that reads as "somebody sent the tracee a
        // SIGTRAP", and a tracer that acts on that difference does not finish
        // the `execve` the event was reporting.
        abi::PTRACE_GETSIGINFO => {
            let Some(stop) = crate::sched::stop_of(id) else {
                return err::ESRCH;
            };
            if !buffer_ok(data, abi::SIGINFO_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let code = if stop.event != 0 {
                // A ptrace event: the event number sits above the signal.
                ((stop.event as i32) << 8) | crate::sched::signal::SIGTRAP as i32
            } else if stop.delivering.is_some() {
                SI_USER
            } else {
                // A syscall boundary, or the exec trap when the tracer did not
                // ask for the event: a trap, not a signal anybody sent.
                crate::sched::signal::SIGTRAP as i32
            };
            unsafe { core::ptr::write_bytes(data as *mut u8, 0, abi::SIGINFO_BYTES) };
            unsafe {
                (data as *mut i32).write_unaligned(stop.signal as i32);
                ((data + 8) as *mut i32).write_unaligned(code);
            }
            0
        }
        // `PTRACE_SETSIGINFO`: the tracer would replace the `siginfo_t` the
        // tracee sees on the next delivery. This kernel does not carry a custom
        // siginfo into a signal frame - the tracee's delivered signal is the one
        // named on the resume, synthesized from its number - so the honest answer
        // is `EOPNOTSUPP`, the §4.4 "an action this machine does not implement"
        // refusal a probing tracer falls back from. The old arm returned `0` for
        // any `pid` while reading nothing: a not-a-tracee got a success where
        // Linux answers `ESRCH`, a bad buffer a success where Linux answers
        // `EFAULT` (it copies the struct first), and a good one a success that
        // silently discarded it - the worst §4.2 shape of all three. The checks
        // run in Linux's order: attach state, then the copy, then the refusal.
        abi::PTRACE_SETSIGINFO => {
            if crate::sched::stop_of(id).is_none() {
                return err::ESRCH;
            }
            if !buffer_ok(data, abi::SIGINFO_BYTES, false, from_user) {
                return err::EFAULT;
            }
            err::EOPNOTSUPP
        }
        // Letting the tracee go on. `SYSCALL` re-arms the syscall trace and
        // `CONT` does not; `SINGLESTEP` is answered as a `SYSCALL` because this
        // kernel has no single-step trap, and a tracer that asked for one gets
        // stops at the next boundaries rather than none at all.
        abi::PTRACE_CONT | abi::PTRACE_SYSCALL | abi::PTRACE_SINGLESTEP => {
            let arm = request != abi::PTRACE_CONT;
            if crate::sched::resume_traced(id, data as u8, arm) {
                0
            } else {
                err::ESRCH
            }
        }
        // `PTRACE_KILL` is a `SIGKILL` that is also a resume: a stopped tracee
        // cannot die where it stands, so killing it means letting it run far
        // enough to be killed.
        abi::PTRACE_KILL => {
            if crate::sched::resume_traced(id, crate::sched::signal::SIGKILL, false) {
                0
            } else {
                err::ESRCH
            }
        }
        abi::PTRACE_DETACH => {
            if crate::sched::detach_traced(id, data as u8) {
                0
            } else {
                err::ESRCH
            }
        }
        // The floating-point register file. The state exists per task like the
        // integer one does - it has to, or two programs would share one physical
        // register file - and on x86_64 the ABI's `user_fpregs_struct` *is* the
        // 512-byte `fxsave` image, so what a tracer gets here is the same block
        // the context switch moves.
        //
        // The size is not a parameter the way it is for `GET_SYSCALL_INFO`: that
        // structure has a union in it whose length the caller chooses, while
        // this one is fixed, and a buffer smaller than the registers is a tracer
        // about to act on registers it did not read (`EFAULT` here, checked by
        // `buffer_ok`'s length).
        abi::PTRACE_GETFPREGS => {
            if !buffer_ok(data, abi::FPREGS_BYTES, true, from_user) {
                return err::EFAULT;
            }
            let Some(fpu) = crate::sched::trace_fpu(id) else {
                return err::ESRCH;
            };
            abi::write_user_fpregs(data, &fpu);
            0
        }
        abi::PTRACE_SETFPREGS => {
            if !buffer_ok(data, abi::FPREGS_BYTES, false, from_user) {
                return err::EFAULT;
            }
            let fpu = abi::read_user_fpregs(data);
            if crate::sched::set_trace_fpu(id, &fpu) {
                0
            } else {
                err::ESRCH
            }
        }
        // A request this kernel does not know: Linux's own answer, and the one
        // a tracer's feature probe is written to read.
        _ => err::EIO,
    }
}

/// `process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)` —
/// bulk memory reading across processes.
///
/// `strace` reaches for this before falling back to `PTRACE_PEEKDATA`: a word
/// per call is fine for a `stat` structure and hopeless for a long `execve`
/// argument vector. The pair of iovec arrays are in the *caller's* memory and
/// the iovecs themselves name the two address spaces - local ones here, remote
/// ones in the target.
pub(super) fn handle_process_vm_readv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, local, local_count, remote, remote_count, flags) = (
        args[0] as isize,
        args[1],
        args[2],
        args[3],
        args[4],
        args[5],
    );
    if flags != 0 {
        return err::EINVAL;
    }
    if pid <= 0 || pid as usize >= crate::sched::MAX_TASKS {
        return err::ESRCH;
    }
    let id = TaskId(pid as usize);
    let Some(root) = crate::sched::space_root_of(id) else {
        return err::ESRCH;
    };
    // The same question `PTRACE_ATTACH` asks, because it is the same thing:
    // Linux refuses a read of another process's memory that a ptrace attach
    // could not get - `EPERM` for a task outside the caller's thread group with
    // no uid in common, or one that is not dumpable.
    if crate::sched::may_inspect(id) == Some(false) {
        return err::EPERM;
    }
    match transfer_iovs(
        local,
        local_count,
        remote,
        remote_count,
        from_user,
        Cross::FromRemote,
        |at, bytes| crate::sched::read_user_of(root, at, bytes),
    ) {
        Ok(moved) => moved as isize,
        Err(e) => e,
    }
}

/// The writing half: `process_vm_writev`.
pub(super) fn handle_process_vm_writev(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (pid, local, local_count, remote, remote_count, flags) = (
        args[0] as isize,
        args[1],
        args[2],
        args[3],
        args[4],
        args[5],
    );
    if flags != 0 {
        return err::EINVAL;
    }
    if pid <= 0 || pid as usize >= crate::sched::MAX_TASKS {
        return err::ESRCH;
    }
    let id = TaskId(pid as usize);
    let Some(root) = crate::sched::space_root_of(id) else {
        return err::ESRCH;
    };
    // As in the reading half: the write is gated on the same question, so a
    // process cannot write into memory it may not read.
    if crate::sched::may_inspect(id) == Some(false) {
        return err::EPERM;
    }
    match transfer_iovs(
        local,
        local_count,
        remote,
        remote_count,
        from_user,
        Cross::ToRemote,
        |at, bytes| crate::sched::write_user_of(root, at, bytes),
    ) {
        Ok(moved) => moved as isize,
        Err(e) => e,
    }
}

/// Which way the bytes cross in a `process_vm_*` call. The two calls differ in
/// nothing else - the same two iovec arrays, walked the same way - so one
/// routine does both and the flag decides which side is read and which written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cross {
    /// `process_vm_readv`: the remote task's memory is the source.
    FromRemote,
    /// `process_vm_writev`: the remote task's memory is the destination.
    ToRemote,
}

/// Walk two iovec arrays in step and hand each pair to `move_remote`, answering
/// how many bytes crossed.
///
/// The two arrays are *not* required to be the same shape: Linux walks them as
/// two cursors and moves the smaller of the two remainders at each step, which
/// is how a caller can name one long range on one side and several short ones on
/// the other. Pairing the iovecs up by index instead, which is the obvious
/// simplification, silently mangles exactly those transfers.
fn transfer_iovs(
    local: usize,
    local_count: usize,
    remote: usize,
    remote_count: usize,
    from_user: bool,
    cross: Cross,
    mut move_remote: impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<usize, isize> {
    const IOVEC_BYTES: usize = 16;
    const CHUNK: usize = 512;
    if local == 0 || remote == 0 {
        return Err(err::EFAULT);
    }
    if local_count > 1024 || remote_count > 1024 {
        return Err(err::EINVAL);
    }
    if !buffer_ok(local, local_count * IOVEC_BYTES, false, from_user)
        || !buffer_ok(remote, remote_count * IOVEC_BYTES, false, from_user)
    {
        return Err(err::EFAULT);
    }

    let mut moved = 0usize;
    let mut piece = [0u8; CHUNK];
    let (mut li, mut ri) = (0usize, 0usize);
    let (mut lat, mut lleft) = (0usize, 0usize);
    let (mut rat, mut rleft) = (0usize, 0usize);

    loop {
        if lleft == 0 {
            if li >= local_count {
                break;
            }
            let Some((base, len)) = read_iovec(local + li * IOVEC_BYTES) else {
                return Err(err::EFAULT);
            };
            li += 1;
            lat = base;
            lleft = len;
        }
        if rleft == 0 {
            if ri >= remote_count {
                break;
            }
            let Some((base, len)) = read_iovec(remote + ri * IOVEC_BYTES) else {
                return Err(err::EFAULT);
            };
            ri += 1;
            rat = base;
            rleft = len;
        }
        // A zero-length iovec is a step, not a stop: the cursor above has
        // already moved past it, and the loop comes back round for the next.
        let chunk = lleft.min(rleft).min(CHUNK);
        if chunk == 0 {
            continue;
        }
        // Only the local side is the caller's own memory, so only it is checked
        // with the pointer rules every other syscall uses. The remote side
        // belongs to another process entirely: `move_remote` reaches it by
        // translation and is the one that knows whether it is reachable.
        let local_ok = if cross == Cross::FromRemote {
            buffer_ok(lat, chunk, true, from_user)
        } else {
            buffer_ok(lat, chunk, false, from_user)
        };
        if !local_ok {
            // A transfer that stopped partway is not a failure. Linux answers
            // with however many bytes did cross, and a tracer reading a string
            // that ends beside an unmapped page depends on exactly that -
            // answering `EFAULT` where a short answer was available turns a
            // truncated string into an error. Only a transfer that moved
            // nothing at all is reported as one.
            return if moved > 0 {
                Ok(moved)
            } else {
                Err(err::EFAULT)
            };
        }
        let ok = match cross {
            // Reading *from* the remote task: it fills the buffer, the caller
            // gets it. Reading the local side first here - which looks like the
            // same statement in a different order - would take the caller's
            // uninitialised bytes for the answer and then overwrite them.
            Cross::FromRemote => {
                move_remote(rat, &mut piece[..chunk])
                    && crate::sched::write_user(lat, &piece[..chunk])
            }
            Cross::ToRemote => {
                crate::sched::read_user(lat, &mut piece[..chunk])
                    && move_remote(rat, &mut piece[..chunk])
            }
        };
        if !ok {
            // The same rule as above: a page that could not be reached ends the
            // transfer where it stands rather than discarding what crossed.
            return if moved > 0 {
                Ok(moved)
            } else {
                Err(err::EFAULT)
            };
        }
        moved += chunk;
        lat += chunk;
        lleft -= chunk;
        rat += chunk;
        rleft -= chunk;
    }
    Ok(moved)
}

/// Get the parent process ID.
///
/// The task table records who spawned whom in ring 3, so the answer is real:
/// the spawner's id, or 0 for init and the tasks nobody spawned - which is
/// also what Linux reports for init.
pub(super) fn handle_getppid() -> isize {
    crate::sched::parent_of(crate::sched::current()).map_or(0, |parent| parent.0 as isize)
}

/// The identity a set-uid or set-group-id exec switches to, or `None` when the
/// file grants neither.
///
/// The bits are read from the file that is about to run, and everything they
/// do not name is left as the caller had it: a set-uid-only file raises the
/// effective *user* id and leaves the group alone. The saved ids move with the
/// effective ones, because that is what lets the program drop the privilege
/// and take it back - the dance a helper does around the one call it needs
/// root for, and the reason a saved id exists at all.
fn setuid_identity(inode: &Arc<dyn crate::vfs::Inode>) -> Option<crate::sched::Identity> {
    // Only a regular file carries the bits: a directory or a device named by
    // `execve` is refused long before this point, and a filesystem that keeps
    // no modes has no bits to read.
    if inode.metadata().kind != crate::vfs::NodeKind::File || !inode.keeps_modes() {
        return None;
    }
    let mode = inode.mode();
    let owner = inode.owner();
    let mut id = crate::sched::current_identity();
    let mut changed = false;
    if mode & crate::vfs::S_ISUID != 0 && id.euid != owner.0 {
        id.euid = owner.0;
        id.suid = owner.0;
        changed = true;
    }
    if mode & crate::vfs::S_ISGID != 0 && id.egid != owner.1 {
        id.egid = owner.1;
        id.sgid = owner.1;
        changed = true;
    }
    if changed { Some(id) } else { None }
}

/// `execve(path, argv, envp)` — replace the current process image.
///
/// Parses the ELF, tears down the old address space, loads the new image,
/// and patches the syscall return frame so the process enters the new image
/// on `sysretq`. This is a real execve: it replaces the caller and never
/// returns.
/// `execve(path, argv, envp)` — replace the calling process's image.
///
/// The one piece of interpretation the kernel does for a program: a file that
/// begins with `#!` is not an executable, it is the *name* of the thing that
/// runs it plus the file to hand it. Resolving that here rather than in a
/// library is what makes `./script.sh` work where `sh script.sh` does, and it
/// is the difference between a package's install trigger running and the
/// install failing with `Exec format error`.
pub(super) fn handle_execve(
    args: [usize; ARG_COUNT],
    cwd: &Arc<dyn crate::vfs::Inode>,
    regs_ptr: *mut (),
) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // An argument is not a pathname either. Linux bounds both arrays by
    // `MAX_ARG_STRLEN` - 32 pages - and nothing else: a program is handed
    // whatever the caller wrote, and `fish`'s completion machinery passes an
    // `awk` script several kilobytes long as one argument. A pathname's
    // 255-byte ceiling here refused it with `Filename too long` before `awk`
    // ever ran, which is the same mistake the environment's line below
    // records - made in the array that is *not* paths.
    let mut argv = match argv_arg(args[1], crate::syscall::MAX_ARG_STR) {
        Ok(argv) => argv,
        Err(e) => return e,
    };
    if argv.is_empty() {
        let name = path.rsplit('/').next().unwrap_or(&path);
        argv.push(String::from(name));
    }
    // The environment's strings are not paths: a real compiler's
    // COLLECT_GCC_OPTIONS runs past 255 bytes, and Linux bounds envp by
    // the argument block's total, not by a pathname's ceiling.
    let envp = match argv_arg(args[2], crate::syscall::MAX_ARG_STR) {
        Ok(envp) => envp,
        Err(e) => return e,
    };
    // The name has to be executable by the caller before a byte of it is
    // read, and the check comes after the argument strings for the reason
    // Linux puts it there: a caller with a bad `argv` pointer hears EFAULT
    // about its own arguments, not EACCES about a file it never reached.
    let inode = match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    if let Err(e) = crate::vfs::may_access(&inode, crate::vfs::X_OK) {
        return err_of(e);
    }
    let bytes = match crate::vfs::read_file_opt(&path, Some(cwd.clone())) {
        Ok(bytes) => bytes,
        Err(e) => return err_of(e),
    };
    // A `#!` file is a script rather than a program, and Linux does not honour
    // the set-uid bits on one: the bits that count belong to the interpreter,
    // and a kernel that honoured the script's would let anyone who can write
    // into a set-uid-owned script choose what runs with that authority.
    let is_script = bytes.starts_with(b"#!");

    // A script is run by its interpreter, and the kernel is where that is
    // discovered: the file's first two bytes say so. The chain is followed
    // exactly as the kernel's own spawns follow it - one resolver, so the two
    // paths can never disagree about what a script is.
    let (bytes, argv) = match crate::sched::resolve_script(bytes, argv, Some(cwd.clone()), &path) {
        Ok(r) => r,
        Err(crate::sched::SpawnError::Loop) => return err::ELOOP,
        // Linux reports a missing interpreter as "the file was not found",
        // not as a malformed executable: the script itself was fine, and it
        // is the path it names that could not be followed.
        Err(crate::sched::SpawnError::NoInterpreter) => return err::ENOENT,
        Err(_) => return err::ENOEXEC,
    };

    // A set-uid or set-group-id file promises that the program runs with the
    // file's authority rather than the caller's, and honouring that promise is
    // what makes an installed helper work at all: a `doas` that never becomes
    // root refuses to run for exactly the reason it was installed.
    //
    // The switch happens before the exec so the auxv the new image reads
    // describes the identity it actually has, and it is undone when the exec
    // fails - Linux does not hand the privilege to a process that never became
    // the program, and leaving it raised would be a way to ask for root with a
    // file that will not load.
    let before = crate::sched::current_identity();
    let switched = if is_script {
        None
    } else {
        setuid_identity(&inode)
    };
    if let Some(ref id) = switched {
        crate::sched::set_current_identity(id.clone());
    }
    let outcome = crate::sched::exec_replace(&bytes, &argv, &envp, &path);
    if outcome.is_err() && switched.is_some() {
        crate::sched::set_current_identity(before);
    }
    match outcome {
        Ok((entry, new_rsp)) => {
            // Patch the syscall return frame: when sysretq fires, RIP = entry
            // and RSP = new_rsp, so the process enters the new image.
            //
            // The Regs layout (from syscall.rs):
            //   [rsp+0]  rax (return value)
            //   [rsp+8]  r11 (user RFLAGS)
            //   [rsp+16] rcx (user RIP)  ← patch this
            //   ...args...
            //   [rsp+72] user_rsp         ← patch this
            //
            // sysretq takes RIP from rcx and RFLAGS from r11.
            if !regs_ptr.is_null() {
                let regs = unsafe { &mut *(regs_ptr as *mut RegsFrame) };
                regs.rcx = entry; // sysretq will jump here
                regs.user_rsp = new_rsp; // new stack pointer
            }
            // A traced task traps here, on the *new* image's first instruction:
            // by the time `execve` could otherwise "return" the old program is
            // gone, so the trap is the only honest way to tell a tracer that the
            // exec happened. It is also what a tracer is waiting for the very
            // first time - `strace`'s child calls `PTRACE_TRACEME` and then
            // `execv`s, and this stop is how the tracer learns the program
            // started.
            if !regs_ptr.is_null() && crate::sched::has_tracer(crate::sched::current()) {
                let mut regs = capture_frame(regs_ptr, 0);
                crate::sched::exec_trap(&mut regs);
                apply_frame(regs_ptr, &regs);
            }
            0 // execve succeeds; the frame patches make it "not return"
        }
        Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
        Err(crate::sched::SpawnError::ArgsTooBig) => err::E2BIG,
        Err(crate::sched::SpawnError::BadFd) => err::EBADF,
        // Linux reports a missing or unloadable interpreter as "the file was
        // not found", not as a malformed executable: the program itself was
        // fine, and it is the path it names that could not be followed.
        Err(crate::sched::SpawnError::NoInterpreter) => err::ENOENT,
        Err(crate::sched::SpawnError::Loop) => err::ELOOP,
    }
}

/// `nanosleep(req, rem)` — req is a `struct timespec { tv_sec, tv_nsec }`.
///
/// The request is validated the way Linux validates it: the caller's `req`
/// must be readable and `tv_nsec` must stay below a billion. `rem` is never
/// written — nothing interrupts the sleep, and Linux writes the remaining
/// time only when it does.
pub(super) fn handle_nanosleep(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let req = args[0];
    if req == 0 {
        return 0;
    }
    sleep_for_timespec(req, args[1], from_user)
}

/// `clock_nanosleep(clk_id, flags, req, rem)`.
///
/// The relative form (flags = 0) sleeps exactly as `nanosleep` does, whatever
/// clock it names: mpos keeps one clock. `TIMER_ABSTIME` has no absolute time
/// to be measured against and answers `EINVAL` rather than sleeping the wrong
/// duration.
pub(super) fn handle_clock_nanosleep(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const TIMER_ABSTIME: usize = 0x1;
    let (clk_id, flags, req) = (args[0], args[1], args[2]);
    // A clock this kernel does not keep is `EINVAL` before anything else, the way
    // Linux answers it (measured: `clock_nanosleep(999, ...)` is `EINVAL`; only
    // the ids `clock_gettime` names are sleepable). The old handler discarded
    // `clk_id`, so a request to sleep on a nonexistent clock slept on the
    // default and answered `0` - a success for a clock never named. (Unrecognised
    // *flag* bits past `TIMER_ABSTIME` are ignored here and by Linux alike, per
    // the same measurement, so they need no gate.)
    if !matches!(clk_id, 0..=9 | 11) {
        return err::EINVAL;
    }
    if flags & TIMER_ABSTIME != 0 {
        return err::EINVAL;
    }
    if req == 0 {
        return 0;
    }
    // `clock_nanosleep(clk_id, flags, req, rem)` - the remainder rides in the
    // fourth argument, one along from `nanosleep`'s.
    sleep_for_timespec(req, args[3], from_user)
}

/// Sleep out the `struct timespec` at `req`, with Linux's validation: the
/// pointer must be readable and `tv_nsec` must stay below a second.
///
/// A catchable signal ends the sleep with `EINTR`, and `rem` - when the caller
/// left one - is handed the time that was *left*, which is the whole reason the
/// call takes a second pointer. Measured on Linux: a 5 s `nanosleep` with a
/// `SIGALRM` at 100 ms answers `-1/EINTR` at 100 ms with `rem` holding about
/// 4.9 s, and it does so whether or not the disposition has `SA_RESTART` -
/// `nanosleep` is on `signal(7)`'s never-restarted list, so it is the `rem`
/// rather than a rewind that makes a retry cheap. Missing it is visible in
/// musl, whose `sleep()` is `nanosleep(&ts, &ts)` in a loop: without the
/// remainder, every signal doubles the sleep the caller asked for.
///
/// The remaining-time write is validated at the moment it happens, not up
/// front, which is Linux's order too: a `rem` nobody could write is not an
/// error until there is something to put in it, and then the failing copy is
/// what comes back (`EFAULT`).
fn sleep_for_timespec(req: usize, rem: usize, from_user: bool) -> isize {
    if !buffer_ok(req, 16, false, from_user) {
        return err::EFAULT;
    }
    let (tv_sec, tv_nsec) = unsafe {
        (
            (req as *const i64).read_unaligned() as u64,
            ((req + 8) as *const i64).read_unaligned() as u64,
        )
    };
    // The `tv_nsec < 1s` check and the truncate-to-milliseconds fold are the
    // man page's contract, spelled in `abi` - nanosleep truncates where
    // ppoll rounds up, and that distinction lives in the one place.
    let total_ms = match abi::nanosleep_ms(tv_sec, tv_nsec) {
        Ok(ms) => ms,
        Err(e) => return e,
    };
    if total_ms == 0 {
        return 0;
    }
    let ticks = (total_ms * crate::arch::timer::TICK_HZ as u64 / 1000).max(1);
    let hz = crate::arch::timer::TICK_HZ as u64;
    let deadline = crate::arch::timer::ticks().saturating_add(ticks);
    match crate::sched::block_until_deadline(|| false, deadline) {
        crate::sched::WaitEnd::Interrupted => {
            if rem != 0 {
                if !buffer_ok(rem, 16, true, from_user) {
                    return err::EFAULT;
                }
                let left = deadline.saturating_sub(crate::arch::timer::ticks());
                let (sec, nsec) = abi::timespec_from_ticks(left, hz);
                unsafe {
                    (rem as *mut i64).write_unaligned(sec);
                    ((rem + 8) as *mut i64).write_unaligned(nsec);
                }
            }
            err::EINTR
        }
        // The clock ran out: the condition is never true, so the only other
        // ending is the deadline itself.
        _ => 0,
    }
}

/// `fork()` — duplicate the calling process.
///
/// The child returns 0 from this call and the parent gets the child's pid; both
/// carry on from the instruction after the `syscall`. `regs_ptr` is the gate's
/// register frame, and it is the whole reason this handler needs it: the child
/// must resume with exactly the caller's registers, so the kernel has to
/// reconstruct that file and hand it to the new task.
///
/// A null frame means the call did not come through the `syscall` gate (the
/// kernel's own in-process tests, for instance), and there is no register file
/// to resume from; that is an honest `EINVAL` rather than a half-made child.
///
/// `vfork` reaches this same handler and is answered as a `fork`. That is a
/// divergence in one visible respect only: Linux suspends the calling process
/// until the child execs or exits, and this does not. Everything else holds - the
/// child is a real process with its own image, and the `fork`-then-`execve` pair
/// every `vfork` user writes behaves identically. Suspending the parent is a
/// scheduling feature for an optimisation this kernel does not make (there is no
/// page-table sharing to avoid), so there is nothing to gain by faking it.
pub(super) fn handle_fork(nr: u64, _args: [usize; ARG_COUNT], regs_ptr: *mut ()) -> isize {
    let ctx = match user_context(regs_ptr, 0) {
        Some(ctx) => ctx,
        None => return err::EINVAL,
    };
    let child = if nr == super::nr::VFORK {
        crate::sched::vfork(&ctx)
    } else {
        crate::sched::fork(&ctx)
    };
    if child > 0 {
        tell_tracer_of_child(regs_ptr);
    }
    child
}

/// Tell the tracer, if there is one, that this task has just made a child.
///
/// The `(event, message)` pair is what the scheduler recorded at the creation,
/// taken once - see `sched::attach_new_child`, which also stops the child
/// before it can run. The *stop* is raised here rather than at the creation
/// because a stop is a change to the registers this task will return through,
/// and the frame those live in is reachable only from the syscall layer. Doing
/// it here also puts it in the right place in the call: `PTRACE_EVENT_CLONE`
/// belongs inside the `clone` that caused it, and the call's ordinary exit stop
/// still follows, exactly as it does after an exec trap.
fn tell_tracer_of_child(regs_ptr: *mut ()) {
    if regs_ptr.is_null() {
        return;
    }
    let Some((event, child)) = crate::sched::take_pending_event(crate::sched::current()) else {
        return;
    };
    let mut regs = capture_frame(regs_ptr, 0);
    crate::sched::fork_trap(&mut regs, event, child);
    apply_frame(regs_ptr, &regs);
}

/// Build the ring-3 register file a process would resume with, from the syscall
/// gate's frame, with `rax` set to `rax` - zero for a fork child, and the new
/// image's entry value for an `execve` that has nothing else to restore.
///
/// Returns `None` when there is no frame: a call made outside the gate has no
/// registers to name.
fn user_context(regs_ptr: *mut (), rax: usize) -> Option<crate::arch::context::UserContext> {
    if regs_ptr.is_null() {
        return None;
    }
    let regs = unsafe { &*(regs_ptr as *const RegsFrame) };
    let (code, data) = crate::arch::gdt::user_selectors();
    Some(crate::arch::context::UserContext {
        // `syscall` parked the return address in rcx and the caller's flags in
        // r11; those two are what an `iretq` needs to reproduce the state the
        // instruction left behind.
        rip: regs.rcx,
        rsp: regs.user_rsp,
        rflags: regs.r11,
        rax,
        rbx: regs.rbx,
        rbp: regs.rbp,
        cs: code.0 as usize,
        ss: data.0 as usize,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        rdi: regs.rdi,
        rsi: regs.rsi,
        rdx: regs.rdx,
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
    })
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)` — create a process or
/// a thread.
///
/// On x86_64 the arguments arrive as: `rdi` = flags, `rsi` = the child's stack,
/// `rdx` = where the parent wants the child's tid written, `r10` = where the
/// child wants its own tid written, `r8` = the child's TLS descriptor.
///
/// The call is `fork` with knobs: `fork` is `clone` with a zero flag word, and
/// each set bit names a part of the caller the child should *share* instead of
/// copying. The flag word is checked the way Linux checks it - an unknown bit,
/// or a combination whose pieces need each other (`CLONE_THREAD` implies
/// `CLONE_SIGHAND`, which implies `CLONE_VM`), is `EINVAL` before anything is
/// created.
///
/// A process-shaped request - anything without `CLONE_THREAD` - is built here in
/// full: the child returns from the call with 0 in `rax` on the stack the caller
/// chose, with the tid writes and the TLS the flags name. `CLONE_THREAD` asks for
/// a thread, and the scheduler refuses that one with `ENOSYS`: a shared address
/// space needs reference counting this kernel's owned-by-one-task address spaces
/// do not have, and a child that only *looks* like a thread would be worse than
/// an honest refusal. `CLONE_VM` without `CLONE_THREAD` is the spawn shape and is
/// honoured - see [`crate::sched::clone`].
#[allow(clippy::too_many_lines)]
pub(super) fn handle_clone(args: [usize; ARG_COUNT], regs_ptr: *mut ()) -> isize {
    handle_clone_impl(args, regs_ptr)
}

fn handle_clone_impl(args: [usize; ARG_COUNT], regs_ptr: *mut ()) -> isize {
    let (flags, child_stack, parent_tid, child_tid, tls) =
        (args[0], args[1], args[2], args[3], args[4]);

    // The flag word is checked before anything is created: the low byte is
    // the child's exit signal rather than a flag (a word like
    // `CLONE_VM|CLONE_VFORK|SIGCHLD` - exactly what a libc's `posix_spawn`
    // passes - carries a plain signal number in it), the known bits are the
    // ones this kernel understands, the THREAD⇒SIGHAND⇒VM dependency chain
    // holds, and a tid flag without a place to write the tid is refused.
    // All of it is the clone(2) man page's contract, spelled in `abi` and
    // tested on the host.
    let (exit_signal, flags) = match abi::validate_clone_flags(flags, parent_tid, child_tid) {
        Ok(split) => split,
        Err(e) => return e,
    };
    // A fork child is announced by its parent's `wait`, not by a signal, so the
    // exit signal is accepted and not acted on. `wait4` is how every caller in
    // this system collects a child; the byte is kept in the ABI rather than
    // honoured because there is no delivery path a shell could not get from
    // waiting anyway.
    let _ = exit_signal;

    let ctx = match user_context(regs_ptr, 0) {
        Some(ctx) => ctx,
        None => return err::EINVAL,
    };

    // A thread's stack is its own, so `child_stack` picks where it starts; a
    // process ignores the argument and returns from the call on its own stack,
    // exactly as `fork` does.
    let request = crate::sched::CloneRequest {
        flags,
        child_stack,
        child_tid,
        tls,
    };
    let child = crate::sched::clone(&ctx, request);
    if child < 0 {
        return child;
    }

    // CLONE_PARENT_SETTID: the child's tid goes into the *parent's* memory, so
    // the write happens here, before the child has run.
    if flags & nr::CLONE_PARENT_SETTID != 0 {
        unsafe { (parent_tid as *mut i32).write_unaligned(child as i32) };
    }
    tell_tracer_of_child(regs_ptr);
    child
}

/// Send a signal to a process, or to a whole process group. Used by `kill` and
/// `tkill`.
///
/// Linux reads the id's *sign*: a positive value names one process, -1 names
/// every process the caller may signal (which this kernel reads as the
/// caller's own group), any other negative value names a group by number, and
/// zero names the caller's group. A shell leans on the negative case to stop a
/// job - and on the signal-0 case, which delivers nothing and only asks
/// whether the target is there.
pub(super) fn handle_kill(args: [usize; ARG_COUNT]) -> isize {
    let (id, sig) = (args[0] as isize, args[1]);
    if sig > 31 {
        return err::EINVAL;
    }
    let delivered = match id {
        n if n < 0 => {
            let pgid = if n == -1 {
                crate::sched::current_pgid()
            } else {
                TaskId(n.unsigned_abs())
            };
            crate::sched::send_signal_group(pgid, sig as u8)
        }
        0 => crate::sched::send_signal_group(crate::sched::current_pgid(), sig as u8),
        n => crate::sched::send_signal(TaskId(n as usize), sig as u8),
    };
    if delivered { 0 } else { err::ESRCH }
}

/// `setpgid(pid, pgid)` — move a task into a process group. A `pgid` of zero
/// means "the group led by `pid`", and both `pid` and `pgid` of zero are just
/// the caller.
pub(super) fn handle_setpgid(args: [usize; ARG_COUNT]) -> isize {
    let (pid, pgid) = (args[0] as isize, args[1] as isize);
    let me = crate::sched::current();
    let target = match pid {
        0 => me,
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    let group = match pgid {
        0 => target,
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    crate::sched::set_pgid(target, group)
}

/// `setsid()` — leave the caller's session and become a session leader with
/// its own process group. This kernel has process groups but no sessions, so
/// the whole of the call is: refuse if the caller already leads its group
/// (every member of it would have to move), otherwise make it the leader of a
/// group of its own.
///
/// This is what a getty calls before opening its tty - and busybox init's
/// respawned gettys are what put a login prompt on the serial line.
pub(super) fn handle_setsid(_args: [usize; ARG_COUNT]) -> isize {
    let me = crate::sched::current();
    // The caller is already the leader of its own group: on Linux every group
    // member would have to move for the session to split, so this is refused
    // rather than silently accepted.
    if crate::sched::pgid_of(me) == Some(me) {
        return err::EPERM;
    }
    match crate::sched::set_pgid(me, me) {
        0 => {
            // The session half of the call: the caller leads its own session
            // from here, and the controlling terminal it held belongs to the
            // session it just left. A getty re-acquires one with TIOCSCTTY
            // immediately after; that is the whole reason the two calls
            // travel together.
            crate::sched::become_session_leader();
            me.0 as isize
        }
        e => e,
    }
}

/// `getsid(pid)` — the session a process belongs to.
///
/// This kernel has no sessions; a process's group leader stands in for one.
/// That is the answer a libc `setsid()` reads back to check its own work -
/// busybox getty calls setsid, then getsid, and an ENOSYS here turns into
/// "setsid: Function not implemented" on a getty that actually succeeded.
pub(super) fn handle_getsid(args: [usize; ARG_COUNT]) -> isize {
    let pid = args[0] as isize;
    let target = match pid {
        0 => crate::sched::current(),
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };
    // Linux answers any live pid; only a dead one is ESRCH.
    match crate::sched::pgid_of(target) {
        Some(pgid) => pgid.0 as isize,
        None => err::ESRCH,
    }
}

/// `getpgid(pid)` and `getpgrp()` — which group a task is in.
pub(super) fn handle_getpgid(args: [usize; ARG_COUNT]) -> isize {
    let pid = args[0] as isize;
    let target = match pid {
        0 => crate::sched::current(),
        n if n > 0 => TaskId(n as usize),
        _ => return err::EINVAL,
    };

    match crate::sched::pgid_of(target) {
        Some(pgid) => pgid.0 as isize,
        None => err::ESRCH,
    }
}

// ---- init's own calls: reboot, sigtimedwait, umask, sendfile -------------
//
// None of these had a handler while the machine was handed straight to a
// test script, and the first thing that ran after the handover - busybox
// init - asked for all of them within a second of starting.

// The `cmd` words of `reboot(2)` - magic numbers, not flag bits - and their
// classification into policy / stop / unknown live in `abi`, tested on the
// host. This side keeps only what a stop word *does*.

/// `reboot(cmd)` — what init asks of the machine.
///
/// The two policy words (ctrl-alt-del's disposition) are recorded nowhere and
/// answered with success, because this kernel has no console keyboard to wire
/// them to and nobody else reads them back. The three stop words end the
/// machine: QEMU's debug exit is the closest thing to a reset this
/// environment has, and `-no-reboot` turns it into a clean shutdown either
/// way. Anything else is refused, as Linux refuses it.
pub(super) fn handle_reboot(args: [usize; ARG_COUNT]) -> isize {
    // `reboot(magic, magic2, cmd, arg)`: the magic words come first and the
    // command is the third argument. Reading `args[0]` as the command reads
    // `LINUX_REBOOT_MAGIC1` instead, classifies it as unknown and refuses -
    // which is a `poweroff -f` that prints "Invalid argument" and leaves the
    // machine up, and a test run that never reaches its end.
    match abi::reboot_command(args) {
        Ok(abi::RebootCmd::Policy) => 0,
        Ok(abi::RebootCmd::Stop) => {
            let cmd = args[2];
            crate::println!("mpos: reboot(0x{cmd:x}) - handing the machine back");
            crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_SUCCESS);
        }
        // A command no kernel knows, whether or not it came with the magic.
        Ok(abi::RebootCmd::Unknown) => err::EINVAL,
        Err(e) => e,
    }
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — wait for one signal
/// from a set and consume it without running any handler.
///
/// This is the door a daemon's main loop walks through: busybox init blocks
/// everything but SIGCHLD and parks here, so a child's exit is noticed the
/// moment the signal arrives rather than on a poll timer. Two kernel-side
/// rules give it its shape. The signal is taken straight off the pending
/// set: no disposition is consulted, which is what lets a daemon eat its own
/// SIGTERM. And the two unblockable signals are silently stripped from the
/// set, as Linux does: init waits on a full set minus SIGCHLD, which names
/// them both, and refusing the call would stop init from ever running.
///
/// A NULL timeout waits forever; a zero one checks and answers `EAGAIN`.
/// A fatal signal that arrives from *outside* the set still kills the waiter
/// on each pass, exactly as `block_until` does for every blocking call.
pub(super) fn handle_rt_sigtimedwait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    /// The bytes of `siginfo_t` on x86_64.
    const SIGINFO_BYTES: usize = 128;

    let (set_ptr, info, timeout, sigsetsize) = (args[0], args[1], args[2], args[3]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    let mut wanted = 0u64;
    if set_ptr != 0 {
        if !buffer_ok(set_ptr, 8, true, from_user) {
            return err::EFAULT;
        }
        wanted = unsafe { (set_ptr as *const u64).read_unaligned() };
    }
    // The wire's set, converted to the kernel's mask arithmetic — the same
    // boundary `rt_sigprocmask` works at.
    let wanted = crate::sched::signal::from_wire(wanted)
        & !(crate::sched::signal::bit(crate::sched::signal::SIGKILL))
        & !(crate::sched::signal::bit(crate::sched::signal::SIGSTOP));

    // The timeout is a `struct timespec`, read to a tick deadline. A zero
    // deadline is "check once" - the sleeps below never run and `EAGAIN`
    // comes back the first time round.
    let deadline = if timeout != 0 {
        if !buffer_ok(timeout, 16, true, from_user) {
            return err::EFAULT;
        }
        let sec = unsafe { (timeout as *const i64).read_unaligned() };
        let nsec = unsafe { ((timeout + 8) as *const i64).read_unaligned() };
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return err::EINVAL;
        }
        let ms = sec.saturating_mul(1_000) + nsec / 1_000_000;
        Some(crate::arch::timer::ticks() + ms as u64 * crate::arch::timer::TICK_HZ as u64 / 1_000)
    } else {
        None
    };

    loop {
        if let Some((sig, record)) = crate::sched::take_signal_in_set(wanted) {
            if info != 0 {
                if !buffer_ok(info, SIGINFO_BYTES, false, from_user) {
                    return err::EFAULT;
                }
                // The record the signal was queued with, not a generic one:
                // `si_code` is what a waiter branches on - musl's
                // `SIGEV_THREAD` helper only runs its callback for
                // `SI_TIMER`, which is the whole difference between a POSIX
                // timer working and a program that hangs waiting for a
                // timeout the kernel said it set.
                //
                // The `union` at offset 16 is written in the shape every
                // sender here uses - `si_pid`/`si_uid` for a `kill`, and the
                // `union sigval` at 24 for both that and a timer, which is
                // where both variants put it. A `SI_TIMER` record's
                // `si_tid`/`si_overrun` slots (16 and 20) are left zero: no
                // caller in this image reads them, and `timer_getoverrun` is
                // the ABI the count is asked through.
                unsafe {
                    core::ptr::write_bytes(info as *mut u8, 0, SIGINFO_BYTES);
                    (info as *mut i32).write_unaligned(record.signo); // si_signo
                    ((info + 4) as *mut i32).write_unaligned(record.errno); // si_errno
                    ((info + 8) as *mut i32).write_unaligned(record.code); // si_code
                    ((info + 16) as *mut i32).write_unaligned(record.pid); // si_pid
                    ((info + 20) as *mut i32).write_unaligned(record.uid); // si_uid
                    // The `union sigval`, at the offset the `si_value` of a
                    // queued signal lives in the ABI's layout - the same word
                    // `sigqueue`'s handler reads.
                    ((info + 24) as *mut u64).write_unaligned(record.value);
                }
            }
            return sig as isize;
        }
        if let Some(sig) = crate::sched::take_fatal_signal() {
            crate::sched::exit(-(sig as isize));
        }
        if let Some(end) = deadline
            && crate::arch::timer::ticks() >= end
        {
            return err::EAGAIN;
        }
        crate::sched::sleep_ticks(1);
    }
}

// `umask` and its creation mask live in `abi`: a set-and-report with no
// filesystem consumer is pure argument shaping.

/// `sendfile(out, in, offset, count)` — copy bytes between descriptors in the
/// kernel. The in-descriptor must be a file (anything `read_at` reaches); the
/// out-descriptor gets the bytes wherever a `write` would put them.
///
/// `offset` is an in-out `loff_t *`: NULL uses and advances the in-file's own
/// cursor, otherwise the position lives in the caller's word. The one caller
/// on this system asks for a short copy from a file to the console, and that
/// is the shape this serves.
pub(super) fn handle_sendfile(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (out_fd, in_fd, offset_ptr, count) = (args[0] as i32, args[1] as i32, args[2], args[3]);
    let fds = crate::sched::current_fds();
    let out = match fds.clone_at(out_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let input = match fds.clone_at(in_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    if count == 0 {
        return 0;
    }

    let mut pos: u64 = if offset_ptr != 0 {
        if !buffer_ok(offset_ptr, 8, true, from_user) {
            return err::EFAULT;
        }
        unsafe { (offset_ptr as *const u64).read_unaligned() }
    } else {
        input.pos()
    };

    let mut sent: usize = 0;
    let mut chunk = [0u8; 4096];
    while sent < count {
        let want = (count - sent).min(chunk.len());
        let read = if offset_ptr != 0 {
            input.inode().read_at(pos, &mut chunk[..want])
        } else {
            input.read(&mut chunk[..want])
        };
        let n = match read {
            Ok(n) => n,
            Err(e) => {
                if sent > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if n == 0 {
            break;
        }
        let w = match out.write(&chunk[..n]) {
            Ok(w) => w,
            Err(e) => {
                if sent > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if w == 0 {
            break;
        }
        pos += w as u64;
        sent += w;
    }

    if offset_ptr != 0 {
        if !buffer_ok(offset_ptr, 8, false, from_user) {
            return err::EFAULT;
        }
        unsafe { (offset_ptr as *mut u64).write_unaligned(pos) };
    }
    sent as isize
}

/// `madvise(addr, length, advice)` — a hint about a mapping.
///
/// A range that does not begin on a page boundary is not one the kernel can
/// locate, so it answers `EINVAL` the way Linux does.
///
/// `MADV_DONTNEED` is the one advice whose contract is about the pages rather
/// than a hint, and it is real now that a mapping is an address space: it drops
/// the pages the range has, and the next access refills them. What "refill"
/// means is what the backing decides, which is exactly Linux's rule - a private
/// anonymous page comes back as a fresh zero-filled one, and a page of a shared
/// or file-backed mapping comes back from the object behind it. The other
/// advisories stay the recognised no-ops they are, and `MADV_FREE` is
/// deliberately among them: its "the pages may be reclaimed but the contents are
/// still there until pressure" contract is a policy this kernel has no pressure
/// model for, and handing the frames back immediately would be *wrong* rather
/// than merely different.
pub(super) fn handle_madvise(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;
    let (addr, len, advice) = (args[0], args[1], args[2]);
    if addr & (PAGE_SIZE - 1) != 0 {
        return err::EINVAL;
    }
    // An advice the vocabulary does not name is EINVAL, the way Linux answers
    // a number no one gave it. Returning `0` for one was the accept-and-drop
    // this gate removes: a caller probing a hint it does not expect to be
    // supported gets the honest refusal and falls back, rather than a success
    // for a request this kernel was never asked to keep.
    if !super::abi::madvise_advice_is_known(advice) {
        return err::EINVAL;
    }
    if advice == super::abi::MADV_DONTNEED && len != 0 {
        // A range that runs past the address space is clamped rather than
        // allowed to take the machine down: the pages beyond it do not exist,
        // and there is nothing there to drop.
        let end = addr.saturating_add(len).min(USER_LIMIT);
        if end > addr {
            crate::sched::madvise_dontneed(addr, end - addr);
        }
    }
    0
}

/// `brk(addr)` — change the program break (heap end).
pub(super) fn handle_brk(args: [usize; ARG_COUNT]) -> isize {
    let addr = args[0];
    crate::sched::brk(addr)
}

use crate::mm::vm::MapFlags;

// The `mmap`/`mprotect`/`mremap` flag bits are Linux's numbering, pinned in
// `abi` with the rest of the flag words.
use super::abi::{
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_HUGETLB, MAP_SHARED, MREMAP_FIXED,
    MREMAP_MAYMOVE, PROT_EXEC, PROT_WRITE,
};

/// One past the highest address a process may map: the lower half of the
/// canonical address space, which is where every user mapping lives.
const USER_LIMIT: usize = 1 << 47;

/// `mmap(addr, length, prot, flags, fd, offset)` — map memory, from a file or
/// from nothing.
///
/// What a mapping *is* depends on what backs it, and there are three shapes:
///
/// * **Anonymous.** Private memory is a range of the process's own, zero-filled
///   on first touch; `MAP_SHARED` makes it an unnamed object (`Memfd`) the pages
///   alias, so a `fork`ed child and its parent watch each other's writes through
///   it. Either way nothing is allocated here - the mapping is the address space,
///   and the pages arrive on the faults that need them (§5.1).
/// * **Frame-backed.** A node that answers [`crate::vfs::Inode::frame_backed`] -
///   a `memfd` descriptor, a dma-buf, the GPU framebuffer, a DRM dumb buffer -
///   hands out its own pages, and the mapping *is* those pages: a program
///   drawing into its mapping of the framebuffer is drawing on the screen. The
///   pages come from the object on the fault that asks for them, so the aliasing
///   the eager version gave is kept without mapping a screenful up front.
/// * **An ordinary file.** Read in at the moment the mapping is made: the pages
///   are private copies of what the file held, and nothing tracks the file
///   afterwards. That is the whole of `MAP_PRIVATE` for a reader, and it is what
///   the dynamic linker needs - it maps a shared object read-only, parses it,
///   and then `mprotect`s the pieces it wants writable.
///
/// The address handling is Linux's, and two details of it are load-bearing for
/// a real libc:
///
/// * `MAP_FIXED` *takes* the range. Whatever was mapped there is dropped first,
///   so the call cannot fail because something was in the way. musl's mallocng
///   depends on this: it claims the guard page of its heap by mapping
///   `PROT_NONE` over the first page of a range it has just grown with `brk`.
///   A kernel that refuses - or worse, asserts - breaks every musl program.
/// * Without `MAP_FIXED` the address is only a hint, and a hint that is still
///   in use is simply not taken: the mapping goes wherever there is room.
///
/// `PROT_NONE` maps a range the process may not reach rather than leaving it
/// unmapped, so a later `mprotect` can hand it back with its permissions
/// changed - and, since the range has no pages either way, with nothing lost in
/// between.
pub(super) fn handle_mmap(args: [usize; ARG_COUNT]) -> isize {
    handle_mmap_impl(args)
}

fn handle_mmap_impl(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (addr, length, prot, flags, fd, offset) =
        (args[0], args[1], args[2], args[3], args[4] as i32, args[5]);

    let anon = flags & MAP_ANONYMOUS != 0;
    // Shared and private anonymous memory are different objects now. A private
    // range is the process's own pages, zero-filled on first touch. A *shared*
    // one is an anonymous object the mapping aliases - the unnamed `Memfd`
    // built below - so two mappings of it, or a parent and a child across a
    // `fork`, watch each other's writes through the very same frames. That is
    // the §5.1 gap closed: before the VMA table the mapping was a private range
    // that happened to be shared between threads, and a plain `fork` gave the
    // child a copy.
    let shared = flags & MAP_SHARED != 0;
    let fixed = flags & MAP_FIXED != 0;
    let no_replace = flags & MAP_FIXED_NOREPLACE != 0;

    if length == 0 {
        return err::EINVAL;
    }
    // `MAP_FIXED` takes the range and `MAP_FIXED_NOREPLACE` refuses to - asking
    // for both is not a thing a caller can mean, and Linux says so with `EINVAL`
    // rather than picking one.
    if fixed && no_replace {
        return err::EINVAL;
    }
    // There is no huge-page pool here. The honest answer to a caller that asked
    // for one is the `ENOMEM` Linux gives when the reserve is unavailable - not
    // a silent 4 KiB mapping that would let the caller believe it got huge pages.
    if flags & MAP_HUGETLB != 0 {
        return err::ENOMEM;
    }

    // What the mapping is filled from. The descriptor is ignored when the
    // mapping is anonymous, as Linux ignores it.
    let backing = if anon {
        None
    } else {
        // The offset of a file mapping is a page number, not a byte count, so
        // one that is not page-aligned has no meaning to honour.
        if !offset.is_multiple_of(PAGE_SIZE) {
            return err::EINVAL;
        }
        match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => Some(file),
            Err(_) => return err::EBADF,
        }
    };

    let len = crate::mm::pframe::align_up(length, PAGE_SIZE);

    // An address a process can actually use: inside the lower half, on a page
    // boundary, and not wrapping.
    let want = (addr != 0
        && addr.is_multiple_of(PAGE_SIZE)
        && addr.checked_add(len).is_some_and(|end| end < USER_LIMIT))
    .then_some(addr);

    let base = match (fixed, want) {
        // Linux: a fixed request *takes* the range, replacing what is there.
        (true, Some(addr)) => {
            crate::sched::munmap_user(addr, len);
            addr
        }
        // A fixed request for an address that cannot be honoured is an error,
        // not something to paper over by mapping elsewhere: the caller is
        // relying on the address.
        (true, None) => return err::EINVAL,
        // `MAP_FIXED_NOREPLACE`: the address is mandatory, but an occupied range
        // is refused with `EEXIST` rather than relocated. This is the whole
        // point of the flag over a plain hint - the caller will not accept the
        // mapping landing anywhere else - so an in-use address must not be
        // silently moved to a free run below.
        (false, Some(addr)) if no_replace => {
            if crate::sched::user_range_free(addr, len) {
                addr
            } else {
                return err::EEXIST;
            }
        }
        // Without `MAP_FIXED` the address is only a hint, and one that is
        // already in use is simply not taken.
        (false, Some(addr)) if crate::sched::user_range_free(addr, len) => addr,
        _ => match free_range(len, crate::sched::get_mmap_cursor()) {
            Some((base, new_cursor)) => {
                crate::sched::set_mmap_cursor(new_cursor);
                base
            }
            None => return err::ENOMEM,
        },
    };

    // Readable is what a present page is; `USER` is what makes it reachable
    // from ring 3 at all, and `PROT_NONE` is exactly the absence of that bit.
    let mut mf = MapFlags::READ;
    if prot & PROT_WRITE != 0 {
        mf |= MapFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        mf |= MapFlags::EXEC;
    }
    if prot != 0 {
        mf |= MapFlags::USER;
    }

    // The mapping is recorded, not filled: the address space *is* the mapping,
    // and the page behind an address appears the first time it is touched. That
    // is what makes a large `mmap` a statement about addresses rather than a
    // claim on memory - and it is the whole of §5.1/§5.2's prerequisite, since
    // a page that can arrive on a fault is a page `MADV_DONTNEED` may take away
    // again.
    match &backing {
        // A shared anonymous mapping is an *object*: an unnamed `Memfd`, which
        // is the same frame store a `memfd_create` descriptor is - real pages,
        // grown on demand, refcounted, and released when the last mapping of it
        // goes. Nobody holds a descriptor for this one, and nobody needs to:
        // the mapping's VMA is its only name, and `fork` copies that. Every
        // property the memfd path already has (aliasing, sharing, release) is
        // inherited rather than rebuilt for the anonymous case.
        None if shared => {
            let object = crate::vfs::memfd::Memfd::new(super::fs::next_anon_ino(), 0);
            object.set_len(len).ok();
            // Named as Linux names a mapping of a memfd with no name of its
            // own, which is what this object is: `/memfd: (deleted)` - measured
            // on Linux 7.2.7 for `mmap(MAP_SHARED, memfd_create("", 0))`. Linux
            // reaches the same *shape* through `/dev/zero (deleted)`, which is
            // an artefact of its own implementation; the object here really is a
            // memfd, and this is the truthful name for it.
            crate::sched::map_frames(
                base,
                len,
                mf,
                crate::vfs::MappedInode::new(object),
                0,
                true,
                Some(alloc::sync::Arc::from("/memfd: (deleted)")),
            );
        }
        // Private anonymous memory: frames of the process's own, zero-filled on
        // first touch.
        None => crate::sched::map_anon(base, len, mf),
        // A node that owns real pages: the mapping aliases them, and the page
        // each fault needs comes from `mmap_frame`. This is the memfd, the
        // framebuffer and a dma-buf - and, since a wl_shm pool and a scanout
        // both live or die by aliasing, the only shape those can be.
        Some(file) if file.inode().frame_backed() => {
            // The name the mapping reports, taken from the descriptor now
            // because taking it later means searching the filesystem tree for
            // the inode (see `File::display_name`). A descriptor that was never
            // opened by a path has none, and gets a blank column - which is what
            // Linux prints for a mapping it cannot name either.
            let name = file
                .label()
                .map(|path| alloc::sync::Arc::from(path.as_str()));
            crate::sched::map_frames(
                base,
                len,
                mf,
                crate::vfs::MappedInode::new(file.inode()),
                offset,
                shared,
                name,
            );
        }
        // An ordinary file: the bytes are read in now, because a mapping of a
        // file this kernel does not page is a copy of what the file held and
        // nothing tracks the file afterwards. That is the whole of a reader's
        // `MAP_PRIVATE`, and what the dynamic linker maps a shared object with
        // before `mprotect`ing the parts it wants writable.
        Some(file) => {
            let inode = file.inode();
            for page in (base..base + len).step_by(PAGE_SIZE) {
                // The pages of a replaced range are gone; these are fresh
                // ones, zeroed as POSIX promises, and the tail of a mapping
                // past the end of the file keeps the zeros a short read leaves.
                let frame = match crate::mm::alloc_frames(0) {
                    Some(frame) => frame,
                    None => return err::ENOMEM,
                };
                let dest = unsafe {
                    core::slice::from_raw_parts_mut(
                        crate::mm::phys_to_virt(frame) as *mut u8,
                        PAGE_SIZE,
                    )
                };
                dest.fill(0);

                let start = offset + (page - base);
                let mut done = 0;
                while done < PAGE_SIZE {
                    match inode.read_at((start + done) as u64, &mut dest[done..]) {
                        Ok(0) => break,
                        Ok(n) => done += n,
                        Err(_) => break,
                    }
                }

                if crate::sched::current_map_user(page, frame, mf) < 0 {
                    return err::ENOMEM;
                }
            }
            // Every page is present, so this mapping has no VMA and no fault
            // will ever describe it; it is recorded beside the table so that
            // `/proc/<pid>/maps` can report it - under the name of the file it
            // was copied from, which is the one thing about it a reader wants.
            let name = file
                .label()
                .map(|path| alloc::sync::Arc::from(path.as_str()));
            crate::sched::record_eager_range(base, base + len, mf, name);
        }
    }

    base as isize
}

/// `mremap(old, old_size, new_size, flags, new_address)` — resize or move a
/// mapping.
///
/// The pages are the *same frames* before and after: a move re-maps them at the
/// new address and takes the old address away, with nothing copied and nothing
/// given back to the allocator. That is what lets a caller keep what a mapping
/// held across the call, which is the whole point of asking.
///
/// Without `MREMAP_MAYMOVE` the mapping can only be resized where it is: a
/// shrink always works, a grow works when the pages above are free, and
/// anything else answers `ENOMEM`. That last answer is load-bearing: musl's
/// `pthread_getattr_np` finds the bottom of a stack by probing with `mremap`
/// and reading `ENOMEM` as "still inside the mapping" and a failure of any
/// other kind as "here is the edge".
pub(super) fn handle_mremap(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (old_addr, old_len, new_len, flags, new_addr) =
        (args[0], args[1], args[2], args[3], args[4]);

    if old_len == 0 || new_len == 0 || !old_addr.is_multiple_of(PAGE_SIZE) {
        return err::EINVAL;
    }
    let old = crate::mm::pframe::align_up(old_len, PAGE_SIZE);
    let new = crate::mm::pframe::align_up(new_len, PAGE_SIZE);
    if old_addr
        .checked_add(old)
        .is_none_or(|end| end >= USER_LIMIT)
    {
        return err::EINVAL;
    }
    let may_move = flags & MREMAP_MAYMOVE != 0;
    let to_fixed = flags & MREMAP_FIXED != 0;
    // Linux: asking for a destination is asking to move there.
    if to_fixed && !may_move {
        return err::EINVAL;
    }

    // Every page has to be part of the mapping: mremap moves mappings, and a
    // hole has nothing in it to move. A *lazily*-mapped page counts as mapped
    // even though no frame is behind it, which is why the two questions are
    // asked separately - the frame travels if there is one, and the VMA travels
    // either way, so an untouched page of the old range becomes an untouched
    // page of the new one rather than an `EFAULT`. Whether each page is
    // *reachable from ring 3* is kept, so a `PROT_NONE` page stays one.
    let mut frames = alloc::vec::Vec::new();
    for page in (old_addr..old_addr + old).step_by(PAGE_SIZE) {
        if !crate::sched::user_mapped(page) {
            return err::EFAULT;
        }
        let frame = crate::sched::user_frame_at(page);
        let reachable = match frame {
            Some(_) => crate::arch::paging::user_accessible(page, PAGE_SIZE, false),
            // No page to read the bits from, so the VMA is what says whether
            // the address was the process's to reach.
            None => {
                crate::sched::vma_prot_at(page).is_some_and(|prot| prot.contains(MapFlags::USER))
            }
        };
        frames.push((frame, reachable));
    }

    if !to_fixed {
        // A shrink, or a grow into the pages just above.
        if new <= old {
            crate::sched::munmap_user(old_addr + new, old - new);
            return old_addr as isize;
        }
        if crate::sched::user_range_free(old_addr + old, new - old) {
            // The grown tail is fresh anonymous memory: the range is claimed
            // and its pages arrive as they are touched, which is the same
            // zeros the eager mapping used to hand over.
            crate::sched::map_anon(
                old_addr + old,
                new - old,
                MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            );
            return old_addr as isize;
        }
        if !may_move {
            return err::ENOMEM;
        }
    }

    // Move: either into the range the caller named, which is taken the way
    // `MAP_FIXED` takes one, or into the first free run big enough.
    let dest = if to_fixed {
        if new_addr % PAGE_SIZE != 0
            || new_addr
                .checked_add(new)
                .is_none_or(|end| end >= USER_LIMIT)
            // A destination overlapping the source would have its pages freed
            // underneath the move. Linux allows it; this does not, and says so.
            || new_addr < old_addr + old && old_addr < new_addr + new
        {
            return err::EINVAL;
        }
        crate::sched::munmap_user(new_addr, new);
        new_addr
    } else {
        match free_range(new, crate::sched::get_mmap_cursor()) {
            Some((dest, new_cursor)) => {
                crate::sched::set_mmap_cursor(new_cursor);
                dest
            }
            None => return err::ENOMEM,
        }
    };

    // The mapping's *description* comes out of the old place before anything
    // moves: the VMAs are what remember which object a page comes from, and a
    // range that arrived lazily at the old address must arrive lazily at the
    // new one rather than as somebody else's memory.
    let taken = crate::sched::take_vmas(old_addr, old);
    let delta = dest as isize - old_addr as isize;

    // The pages go down before the old ones come away, so a mapping that
    // cannot be placed leaves the original intact rather than half moved. A
    // page that was never touched has no frame to carry, and is placed by the
    // VMA reinserted below.
    for (index, (frame, reachable)) in frames.iter().enumerate() {
        let Some(frame) = frame else {
            continue;
        };
        let mut mf = MapFlags::READ | MapFlags::WRITE;
        if *reachable {
            mf |= MapFlags::USER;
        }
        if !crate::sched::move_user_page(dest + index * PAGE_SIZE, *frame, mf) {
            return err::ENOMEM;
        }
    }
    for vma in taken {
        crate::sched::insert_vma(
            vma.start.wrapping_add_signed(delta),
            vma.end.wrapping_add_signed(delta),
            vma.prot,
            vma.kind,
        );
    }
    crate::sched::map_anon(
        dest + old,
        new - old,
        MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
    );
    // The frames moved with the mapping, so this frees nothing.
    crate::sched::unmap_user(old_addr, old);

    dest as isize
}

/// The first run of `len` bytes, page-aligned and below the mmap base, that
/// this process has nothing mapped in.
///
/// The search runs downward and the cursor is the *bottom* of the last mapping
/// placed: the next anonymous mapping goes just below the previous one, which
/// is the direction Linux's mmap region grows in. `cursor` is the per-process
/// mmap cursor; the returned value is the updated cursor to store back.
fn free_range(len: usize, cursor: usize) -> Option<(usize, usize)> {
    use crate::mm::pframe::PAGE_SIZE;

    // A stale cursor from an old layout can only ever sit above the base; the
    // clamp keeps the first placement of every process at the top.
    let mut top = cursor.min(MMAP_BASE);
    loop {
        let bottom = top.checked_sub(len)? & !(PAGE_SIZE - 1);
        if crate::sched::user_range_free(bottom, len) {
            return Some((bottom, bottom));
        }
        top = bottom;
    }
}

/// `munmap(addr, length)` — drop mappings.
///
/// Linux rounds the range outwards to whole pages and ignores holes, which is
/// what [`crate::sched::munmap_user`] does. Both halves of the mapping go: the
/// VMAs, which is what makes the address the process's, and the frames the
/// pages held, which come back to the allocator because nothing can reach them
/// once the entries are gone. A page of a lazily-mapped range that was never
/// touched has no frame to release, and releasing the VMA is the whole of
/// giving it back.
pub(super) fn handle_munmap(args: [usize; ARG_COUNT]) -> isize {
    let (addr, length) = (args[0], args[1]);
    if !addr.is_multiple_of(crate::mm::pframe::PAGE_SIZE) || length == 0 {
        return err::EINVAL;
    }
    crate::sched::munmap_user(addr, length);
    0
}

/// `mprotect(addr, len, prot)` — change what a range of pages permits.
pub(super) fn handle_mprotect(args: [usize; ARG_COUNT]) -> isize {
    use crate::mm::pframe::PAGE_SIZE;

    let (addr, len, prot) = (args[0], args[1], args[2]);
    if !addr.is_multiple_of(PAGE_SIZE) {
        return err::EINVAL;
    }
    let len = crate::mm::pframe::align_up(len, PAGE_SIZE);
    if len == 0 {
        return 0;
    }

    let mut mf = MapFlags::READ;
    if prot & PROT_WRITE != 0 {
        mf |= MapFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        mf |= MapFlags::EXEC;
    }
    if prot != 0 {
        mf |= MapFlags::USER;
    }
    crate::sched::protect_user_range(addr, len, mf);
    0
}

/// The x86_64 `struct itimerval`: two `struct timeval`s - `it_value` first,
/// `it_interval` second - 32 bytes in all.
const ITIMERVAL_SIZE: usize = 32;

/// Write `snapshot` out as a `struct itimerval`, zeroed for a disarmed one.
/// Second granularity is what the kernel's clock supports, so the usec
/// fields stay zero.
/// # Safety
/// `at` must be a writable user address for 32 bytes, as the callers checked.
unsafe fn write_itimerval(at: usize, snapshot: Option<crate::sched::ItimerSnapshot>) {
    let (value, interval) = match snapshot {
        Some(s) => (s.remaining_ticks, s.interval_ticks),
        None => (0, 0),
    };
    let hz = crate::arch::timer::TICK_HZ as u64;
    // `{ it_interval, it_value }`, the same order the reader above uses.
    let words: [u64; 4] = [interval * 1000 / hz, 0, value * 1000 / hz, 0];
    unsafe {
        for (n, word) in words.iter().enumerate() {
            (at as *mut u64).add(n).write_unaligned(*word);
        }
    }
}

/// `setitimer(which, new_value, old_value)`.
///
/// Only `ITIMER_REAL` ever fires: it counts the wall-clock ticks the board
/// actually has. The virtual and profiling timers would need per-task CPU
/// accounting, which does not exist yet, so they are installed and read
/// back faithfully but never expire - the shape a caller that probes and
/// then cancels cannot distinguish from the real thing.
pub(super) fn handle_setitimer(args: [usize; ARG_COUNT]) -> isize {
    let (which, new_ptr, old_ptr) = (args[0], args[1], args[2]);
    if which > crate::sched::ITIMER_PROF {
        return err::EINVAL;
    }
    if old_ptr != 0 && !buffer_ok(old_ptr, ITIMERVAL_SIZE, true, true) {
        return err::EFAULT;
    }
    let (value_ticks, interval_ticks) = if new_ptr == 0 {
        // A NULL new_value is Linux's spelling of "just tell me the old one".
        (0, 0)
    } else {
        if !buffer_ok(new_ptr, ITIMERVAL_SIZE, false, true) {
            return err::EFAULT;
        }
        // `struct itimerval` is `{ it_interval, it_value }` - the *interval*
        // first, the value second. Reading them the other way round turns a
        // one-shot `it_value` into the interval and the interval into the
        // value, and the common `{0, 1s}` spelling - `ping`'s, and every
        // alarm-style use - then reads as "disarm": the timer is never armed
        // and whatever waited on it waits forever.
        let (isec, iusec, vsec, vusec) = unsafe {
            let word = |n: usize| (new_ptr as *const u64).add(n).read_unaligned();
            (word(0), word(1), word(2), word(3))
        };
        // The `tv_usec < 1s` checks and the round-up fold into milliseconds
        // are the man page's contract, spelled in `abi`.
        let (value_ms, interval_ms) = match abi::itimerval_ms(vsec, vusec, isec, iusec) {
            Ok(pair) => pair,
            Err(e) => return e,
        };
        let hz = crate::arch::timer::TICK_HZ as u64;
        (value_ms * hz / 1000, interval_ms * hz / 1000)
    };
    let new = if value_ticks == 0 {
        // A zero it_value disarms, exactly as the man page words it.
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks,
            deadline: crate::arch::timer::ticks() + value_ticks.max(1),
        })
    };
    let old = crate::sched::itimer_swap(which, new);
    if old_ptr != 0 {
        unsafe { write_itimerval(old_ptr, old) };
    }
    0
}

/// `getitimer(which, curr_value)`.
pub(super) fn handle_getitimer(args: [usize; ARG_COUNT]) -> isize {
    let (which, ptr) = (args[0], args[1]);
    if which > crate::sched::ITIMER_PROF {
        return err::EINVAL;
    }
    if !buffer_ok(ptr, ITIMERVAL_SIZE, true, true) {
        return err::EFAULT;
    }
    unsafe { write_itimerval(ptr, crate::sched::itimer_snapshot(which)) };
    0
}

/// `alarm(seconds)`.
///
/// The one-shot spelling of `setitimer(ITIMER_REAL)`. The answer is the time
/// the previous alarm had left, rounded up to seconds as the man page words
/// it - zero when no alarm was set.
pub(super) fn handle_alarm(args: [usize; ARG_COUNT]) -> isize {
    let seconds = args[0] as u64;
    let hz = crate::arch::timer::TICK_HZ as u64;
    let new = if seconds == 0 {
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks: 0,
            deadline: crate::arch::timer::ticks() + (seconds * hz).max(1),
        })
    };
    match crate::sched::itimer_swap(crate::sched::ITIMER_REAL, new) {
        Some(s) if s.remaining_ticks > 0 => s.remaining_ticks.div_ceil(hz) as isize,
        _ => 0,
    }
}

/// The bytes of `struct sigevent` on x86_64: `sigev_value` (a `union sigval`),
/// the signal number, the notification kind, and the union that `SIGEV_THREAD_ID`
/// reads its `sigev_notify_thread_id` out of at offset 16.
const SIGEVENT_SIZE: usize = 32;
/// The bytes of `struct itimerspec`: two `struct timespec`s, `it_interval`
/// then `it_value`.
const ITIMERSPEC_SIZE: usize = 32;

/// `timer_create(clockid, sigevent*, timer_t*)` — make a POSIX interval timer.
///
/// The shape a libc actually sends, measured rather than read off the man
/// page: musl's `SIGEV_THREAD` — what `timer_create` with a callback compiles
/// to — is a *helper thread* blocked in `sigwaitinfo`, plus a kernel timer
/// with `sigev_notify = SIGEV_THREAD_ID` aimed at that thread's tid carrying
/// `SIGRTMIN`. So the notification kinds this kernel must honour to make a
/// callback timer work are `SIGEV_THREAD_ID` (below) and `SIGEV_NONE`; the
/// bare `SIGEV_SIGNAL` is honoured too, aimed at the creating task.
///
/// Two divergences are recorded here rather than hidden. A timer belongs to
/// the *task* that created it, not to its thread group: `timer_delete` from a
/// sibling finds nothing, where Linux's process-wide table would. And a
/// process-directed expiry (`SIGEV_SIGNAL`) goes to the creating task rather
/// than Linux's "the first thread that does not block it" — a difference no
/// caller in this image can observe, because the one program that uses the
/// family (vim, through musl) aims its timers at a thread.
///
/// A `clockid` this kernel has no clock for — the CPU-time clocks, whose
/// accounting does not exist here — is `EINVAL`, as is an out-of-range
/// `sigev_notify`, a signal number the kernel cannot queue, or a
/// `SIGEV_THREAD_ID` naming a thread outside the caller's group: a timer
/// aimed at another process's thread is a way to signal a process the caller
/// may have no right to signal.
pub(super) fn handle_timer_create(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    use crate::sched::{SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD_ID, TimerNotify};

    const CLOCK_REALTIME: usize = 0;
    const CLOCK_MONOTONIC: usize = 1;
    const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
    const CLOCK_MONOTONIC_RAW: usize = 4;
    const CLOCK_REALTIME_COARSE: usize = 5;
    const CLOCK_MONOTONIC_COARSE: usize = 6;
    const CLOCK_BOOTTIME: usize = 7;
    const CLOCK_TAI: usize = 11;

    let (clockid, event, out) = (args[0], args[1], args[2]);
    // The clocks whose timeout this kernel can honour. `REALTIME` and
    // `MONOTONIC` (and `BOOTTIME`/`TAI`, which name the same tick counter with
    // a different epoch) are measured against the one clock the machine keeps.
    //
    // The rest are refused with the errno Linux itself uses, measured on a
    // 6.x kernel rather than read off a man page: the CPU-time clocks (2, 3)
    // and the raw/coarse pair (4, 5, 6) answer `EOPNOTSUPP` there - "this
    // clock exists, and this timer cannot be built on it" - and the alarm
    // clocks (8, 9) answer `EPERM` because they need `CAP_WAKE_ALARM`, a
    // capability this kernel does not model. A number no clock has is
    // `EINVAL`, which is also Linux's answer. Refusing the CPU clocks is
    // deliberate: this kernel keeps no per-task CPU accounting, and a timer
    // that was accepted and silently never fired would be worse than one that
    // says so.
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_TAI => {}
        CLOCK_PROCESS_CPUTIME_ID
        | 3 /* CLOCK_THREAD_CPUTIME_ID */
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | 8 /* CLOCK_REALTIME_ALARM */
        | 9 /* CLOCK_BOOTTIME_ALARM */ => return err::EOPNOTSUPP,
        _ => return err::EINVAL,
    }
    if out == 0 || !buffer_ok(out, 4, true, from_user) {
        return err::EFAULT;
    }

    // A NULL `sigevent` is the man page's own spelling of the default: a
    // `SIGALRM` to the process with a null payload.
    let (signo, value, notify) = if event == 0 {
        (crate::sched::signal::SIGALRM, 0u64, TimerNotify::Signal)
    } else {
        if !buffer_ok(event, SIGEVENT_SIZE, false, from_user) {
            return err::EFAULT;
        }
        let (value, signo, kind, tid) = unsafe {
            (
                (event as *const u64).read_unaligned(),
                ((event + 8) as *const i32).read_unaligned(),
                ((event + 12) as *const i32).read_unaligned(),
                ((event + 16) as *const i32).read_unaligned(),
            )
        };
        let kind = kind as u32;
        if kind != SIGEV_NONE && kind != SIGEV_SIGNAL && kind != SIGEV_THREAD_ID {
            return err::EINVAL;
        }
        if kind == SIGEV_NONE {
            (0, value, TimerNotify::None)
        } else {
            // The range this kernel can queue: 1..=31 plus `SIGRTMIN` (32),
            // which is the number musl's helper waits on. A disposition
            // cannot be installed for 32 — that gap is unchanged — but a
            // *wait* for it is what the timer machinery needs, and it has it.
            if signo <= 0 || signo > crate::sched::signal::SIGRTMIN as i32 {
                return err::EINVAL;
            }
            if kind == SIGEV_THREAD_ID {
                if tid <= 0 {
                    return err::EINVAL;
                }
                (
                    signo as u8,
                    value,
                    TimerNotify::Thread(TaskId(tid as usize)),
                )
            } else {
                (signo as u8, value, TimerNotify::Signal)
            }
        }
    };

    // The tid is validated against the caller's thread group inside the
    // scheduler's table, where the group is known.
    let slot = match crate::sched::timer_alloc(signo, value, notify) {
        Ok(slot) => slot,
        Err(e) => return e,
    };
    // The id the caller holds. Linux hands out an `int`; a libc widens it into
    // its `timer_t`, so four bytes out is the whole answer.
    unsafe { (out as *mut i32).write_unaligned(slot as i32) };
    0
}

/// `timer_settime(timerid, flags, new_value, old_value)`.
///
/// `TIMER_ABSTIME` is `EINVAL`, for the same reason `clock_nanosleep` refuses
/// it: this kernel keeps one clock and has no absolute time domain to measure
/// an absolute deadline against. A relative deadline is what every caller in
/// this image asks for, and the honest refusal is the man page's own errno.
/// Both fields of the new `itimerspec` are validated (a `tv_nsec` outside
/// `[0, 1e9)`, a negative second) before anything is armed, and a zero
/// `it_value` disarms without clearing the interval, exactly as Linux's does.
pub(super) fn handle_timer_settime(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    use crate::sched::TIMER_ABSTIME;
    let (id, flags, new_ptr, old_ptr) = (args[0], args[1], args[2], args[3]);
    if flags & !(TIMER_ABSTIME as usize) != 0 {
        return err::EINVAL;
    }
    if flags & TIMER_ABSTIME as usize != 0 {
        return err::EINVAL;
    }
    if new_ptr == 0 || !buffer_ok(new_ptr, ITIMERSPEC_SIZE, false, from_user) {
        return err::EFAULT;
    }
    if old_ptr != 0 && !buffer_ok(old_ptr, ITIMERSPEC_SIZE, true, from_user) {
        return err::EFAULT;
    }
    let (isec, insec, vsec, vnsec) = unsafe {
        let word = |n: usize| (new_ptr as *const i64).add(n).read_unaligned();
        (word(0), word(1), word(2), word(3))
    };
    let hz = crate::arch::timer::TICK_HZ as u64;
    // `struct itimerspec` is `{ it_interval, it_value }` — the interval first.
    let interval_ticks = match abi::timespec_ticks(isec, insec, hz) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let value_ticks = match abi::timespec_ticks(vsec, vnsec, hz) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let deadline = if value_ticks == 0 {
        None
    } else {
        Some(crate::arch::timer::ticks() + value_ticks)
    };
    // The old state is read and replaced under one table borrow, so a caller
    // that arms and reads back in one call cannot see a state that never
    // existed.
    let Some((old_interval, old_remaining)) = crate::sched::timer_arm(id, interval_ticks, deadline)
    else {
        return err::EINVAL;
    };
    if old_ptr != 0 {
        unsafe { write_itimerspec(old_ptr, old_interval, old_remaining) };
    }
    0
}

/// `timer_gettime(timerid, curr_value)` — the interval and the time left, in
/// a `struct itimerspec`. A disarmed or expired-then-one-shot timer answers a
/// zero value with its interval intact, which is what Linux reports.
pub(super) fn handle_timer_gettime(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (id, ptr) = (args[0], args[1]);
    if ptr == 0 || !buffer_ok(ptr, ITIMERSPEC_SIZE, true, from_user) {
        return err::EFAULT;
    }
    let Some((interval, remaining)) = crate::sched::timer_state(id) else {
        return err::EINVAL;
    };
    unsafe { write_itimerspec(ptr, interval, remaining) };
    0
}

/// `timer_getoverrun(timerid)` — how many expiries arrived while the previous
/// signal was still unread.
pub(super) fn handle_timer_getoverrun(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::timer_overrun(args[0]) {
        Some(count) => count as isize,
        None => err::EINVAL,
    }
}

/// `timer_delete(timerid)` — free the timer. The id dies with it.
pub(super) fn handle_timer_delete(args: [usize; ARG_COUNT]) -> isize {
    if crate::sched::timer_remove(args[0]) {
        0
    } else {
        err::EINVAL
    }
}

/// Write `(interval, remaining)` ticks out as a `struct itimerspec`, the
/// interval at offset 0 and the value at 16.
/// # Safety
/// `at` must be a writable user address for 32 bytes, as the callers checked.
unsafe fn write_itimerspec(at: usize, interval_ticks: u64, remaining_ticks: u64) {
    let hz = crate::arch::timer::TICK_HZ as u64;
    let nsec_per_tick = 1_000_000_000 / hz;
    let split = |ticks: u64| (ticks / hz, (ticks % hz) * nsec_per_tick);
    let (isec, insec) = split(interval_ticks);
    let (vsec, vnsec) = split(remaining_ticks);
    unsafe {
        for (n, word) in [isec, insec, vsec, vnsec].iter().enumerate() {
            (at as *mut u64).add(n).write_unaligned(*word);
        }
    }
}

/// `sysinfo(info)` - fill the 112-byte Linux x86_64 `struct sysinfo`.
///
/// The numbers come from counters the kernel keeps anyway: uptime from the
/// tick count, memory from the frame allocator, the process count from the
/// task table. Loads, swap and the high-memory halves report zero - nobody
/// here measures them.
pub(super) fn handle_sysinfo(args: [usize; ARG_COUNT]) -> isize {
    let buf = args[0];
    const SYSINFO_SIZE: usize = 112;
    if !buffer_ok(buf, SYSINFO_SIZE, true, true) {
        return err::EFAULT;
    }
    let hz = crate::arch::timer::TICK_HZ as u64;
    let ticks = crate::arch::timer::ticks();
    let stats = crate::mm::frame_stats();
    let page = 4096u64;
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, SYSINFO_SIZE);
        let word = |offset: usize| (buf + offset) as *mut u64;
        word(0).write_unaligned(ticks / hz); // uptime
        word(32).write_unaligned(stats.total as u64 * page); // totalram
        word(40).write_unaligned(stats.free as u64 * page); // freeram
        (buf as *mut u16)
            .add(40)
            .write_unaligned(crate::sched::task_count() as u16); // procs
        (buf as *mut u32).add(26).write_unaligned(1); // mem_unit: bytes
    }
    0
}

/// `clock_gettime(clk_id, tp)` — get time.
///
/// `tp` points to a `struct timespec { tv_sec, tv_nsec }`.
///
/// Two clocks are distinguished the way Linux draws the line: the REALTIME
/// family answers with wall time - the RTC's boot anchor plus the tick count -
/// and everything monotonic answers with uptime alone. The distinction is the
/// contract TLS libraries live on: certificate validity is checked against
/// REALTIME and must see the calendar, while monotonic callers must never see
/// that anchor jump under them.
pub(super) fn handle_clock_gettime(args: [usize; ARG_COUNT]) -> isize {
    const CLOCK_MONOTONIC: usize = 1;
    const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
    const CLOCK_THREAD_CPUTIME_ID: usize = 3;
    const CLOCK_MONOTONIC_RAW: usize = 4;
    const CLOCK_MONOTONIC_COARSE: usize = 6;
    const CLOCK_BOOTTIME: usize = 7;
    let (clk_id, tp) = (args[0], args[1]);
    // The clock id is checked before the destination, the way Linux is: a clock
    // the kernel does not keep is `EINVAL` whatever the `tp` is. Measured on the
    // host, `clock_gettime` accepts exactly `{0..=9, 11}` (`REALTIME`,
    // `MONOTONIC`, the two cputime clocks, `MONOTONIC_RAW`, the two coarse
    // clocks, `BOOTTIME`, the two alarm clocks and `TAI`) and answers `EINVAL`
    // for the `10` gap and every id from `12` up - including garbage like `42`
    // and `999`. The old handler fell through to wall-clock time for any id that
    // was not one of its monotonic list, so `clock_gettime(999, &ts)` returned `0`
    // with a number - the §4.2 success-for-a-thing-not-done: a caller probing
    // whether a clock exists was handed a plausible reading for a clock this
    // kernel never named.
    if !matches!(clk_id, 0..=9 | 11) {
        return err::EINVAL;
    }
    if tp == 0 {
        return err::EFAULT;
    }
    // The timespec goes into the caller's memory two words deep; a non-null but
    // unmapped pointer must answer EFAULT here, as `clock_getres` and every
    // other writer does - not be written through blindly.
    if !buffer_ok(tp, 16, true, true) {
        return err::EFAULT;
    }
    // Return the elapsed time, anchored to the RTC's wall time for the
    // REALTIME clocks. `since_boot_nanos` interpolates within the current tick
    // via the TSC, so a fast syscall reports a true sub-millisecond delta
    // instead of snapping to the 10 ms grid the tick counter alone gives.
    let elapsed = crate::arch::timer::since_boot_nanos();
    let monotonic = matches!(
        clk_id,
        CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_MONOTONIC_RAW
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
    );
    let wall = if monotonic {
        0
    } else {
        crate::arch::rtc::boot_epoch()
    };
    let secs = wall + elapsed / 1_000_000_000;
    let nsecs = elapsed % 1_000_000_000;
    unsafe {
        (tp as *mut i64).write_unaligned(secs as i64);
        ((tp + 8) as *mut i64).write_unaligned(nsecs as i64);
    }
    0
}

/// `gettimeofday(tv, tz)` — wall time as `struct timeval { tv_sec, tv_usec }`.
///
/// The same tick the `clock_gettime` family reports, in the microsecond unit
/// this call's struct spells: a `tv_usec` that stayed at nanosecond scale
/// would answer a million-fold too much to every caller that trusts it.
pub(super) fn handle_gettimeofday(args: [usize; ARG_COUNT]) -> isize {
    let tv = args[0];
    if tv == 0 {
        return err::EFAULT;
    }
    // Same as `clock_gettime`: the `timeval` is two words written into the
    // caller's pointer, so a non-null but unaddressable one is EFAULT.
    if !buffer_ok(tv, 16, true, true) {
        return err::EFAULT;
    }
    // gettimeofday has no monotonic variant - every caller wants the wall
    // clock, so the RTC anchor rides along here unconditionally. The TSC-
    // interpolated elapsed time gives a microsecond the 10 ms tick never could.
    let elapsed = crate::arch::timer::since_boot_nanos();
    let secs = crate::arch::rtc::boot_epoch() + elapsed / 1_000_000_000;
    let usecs = (elapsed % 1_000_000_000) / 1000;
    unsafe {
        (tv as *mut i64).write_unaligned(secs as i64);
        ((tv + 8) as *mut i64).write_unaligned(usecs as i64);
    }
    0
}

/// `sethostname(name, len)` — give the caller's namespace the name `uname`
/// reports.
///
/// It used to answer 0 and remember nothing, which is why every boot began
/// with the same name and why `hostname` and `uname -n` could not be made to
/// agree. The name is kept where `/proc/sys/kernel/hostname` reads it, so the
/// two are one fact: a service that writes the file and a program that calls
/// `uname` see the same string.
///
/// It is the *namespace's* name and not the machine's, which is what Linux has
/// said since UTS namespaces existed: the name lives in `crate::uts` and this
/// writes the running task's copy of it, so a program that called
/// `unshare(CLONE_NEWUTS)` first renames only what is inside its own namespace
/// (`t-ns-uts` pins both halves).
pub(super) fn handle_sethostname(args: [usize; ARG_COUNT]) -> isize {
    let ptr = args[0];
    let len = args[1];
    // `CAP_SYS_ADMIN`, and it is checked **first**: measured on a real kernel as
    // a non-root caller, `sethostname(NULL, 100)` answers `EPERM` - not `EFAULT`
    // for the null pointer and not `EINVAL` for the length. The capability on a
    // UTS namespace is the one Linux gates every namespace's contents on, so it
    // outranks the argument checks rather than sitting beside them. The
    // effective-uid test is the one the entropy ioctls use for the same
    // capability.
    if crate::sched::current_identity().euid != 0 {
        return err::EPERM;
    }
    // Longer than a `utsname` field is refused rather than cut: the name is
    // kept in that field, and a caller that asked for seventy bytes would be
    // given back a name it never chose without being told it had been
    // shortened. EINVAL is what Linux answers for exactly this.
    if len > 64 {
        return err::EINVAL;
    }
    let mut bytes = [0u8; 64];
    if !crate::sched::read_user(ptr, &mut bytes[..len]) {
        return err::EFAULT;
    }
    let Ok(name) = core::str::from_utf8(&bytes[..len]) else {
        return err::EINVAL;
    };
    crate::vfs::procfs::set_hostname(name);
    0
}

/// `uname(buf)` — write system name/version info.
///
/// `buf` points to a `struct utsname`: six 65-byte NUL-terminated strings -
/// sysname, nodename, release, version, machine, domainname - 390 bytes in
/// all, which is the size every caller passes and the size this validates.
pub(super) fn handle_uname(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let buf = args[0];
    let size = 390; // 6 fields × 65 bytes
    if !buffer_ok(buf, size, true, from_user) {
        return err::EFAULT;
    }
    unsafe {
        let base = buf as *mut u8;
        // All fields zeroed first.
        core::ptr::write_bytes(base, 0, size);
        let write_str = |offset: usize, s: &str| {
            let dst = base.add(offset);
            let bytes = s.as_bytes();
            let n = bytes.len().min(64);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        };
        // sysname is "Linux": it is the answer to "what kernel is this", and
        // every configure script and package description that asks is asking
        // whether the answer is Linux. The kernel's own name goes in
        // `release`, which is where `uname -r` looks for it.
        write_str(0, "Linux");
        // nodename: the hostname, from the same place `/proc/sys/kernel/
        // hostname` reads it. Two answers to one question, kept in one place
        // so they cannot disagree - `hostname` and `uname -n` have to say
        // the same thing or a script that checks one against the other fails.
        write_str(65, &crate::vfs::procfs::hostname());
        // `uname -r` must answer the version the kernel was built with - it
        // is the same `CARGO_PKG_VERSION` `/proc/sys/kernel/osrelease`
        // reports, and the name of the `/lib/modules/<release>` directory
        // the image carries for modprobe, which xtask reads from the root
        // Cargo.toml (see `utils::kernel_release`). Three readers, one number.
        write_str(130, env!("CARGO_PKG_VERSION")); // release
        write_str(195, "#1 SMP PREEMPT mpos"); // version
        write_str(260, "x86_64"); // machine
        // domainname: read from the caller's namespace, the same one
        // `setdomainname(2)` writes and `/proc/sys/kernel/domainname` reports.
        // It used to be the constant `(none)` here, which made the third door
        // disagree with the other two: a program that set a domain and then
        // asked `uname` was told the domain it had just set was `(none)`.
        write_str(325, &crate::vfs::procfs::domainname());
    }
    0
}

// `getrandom` fills from the same stream `/dev/urandom` reads; the handler
// lives in `abi`, where the host tests can exercise the boundary cases
// against a real slice.
// The rlimit family and the affinity/registration calls (`sched_getaffinity`,
// `sched_setaffinity`, `set_robust_list`, `get_robust_list`, `rseq`) are
// likewise in `abi`: their whole behaviour is argument shaping and table
// answers, and this module keeps only what has to touch the machine.

/// `futex(uaddr, op, val, utime, uaddr2, val3)` — the waiting primitive
/// threads are built on.
///
/// A real `FUTEX_WAIT` and `FUTEX_WAKE`, which is what a mutex needs, plus
/// the bitset forms a condvar and `thread::park_timeout` spell their timed
/// waits in: `FUTEX_WAIT_BITSET` sleeps until a wake, an absolute deadline -
/// `NULL` meaning never - or a changed word, whichever comes first, and
/// `FUTEX_WAKE_BITSET` wakes only the sleepers whose bits intersect the
/// waker's. Rust's std writes every timed park in the bitset form, so a
/// kernel without it parks a thread and never hears the alarm.
///
/// `val3` is the *sixth* argument. The fifth is `uaddr2`, which only the
/// requeue operations read and this kernel does not serve; reading `val3`
/// from its slot made every bitset wait see the zero `uaddr2` passes and
/// answer `EINVAL` - the answer a libc's lock retries on, forever, which is
/// how a program as lock-heavy as fish could spin before its first banner.
pub(super) fn handle_futex(args: [usize; ARG_COUNT]) -> isize {
    let (uaddr, op, val, timeout_ptr, val3) = (args[0], args[1], args[2], args[3], args[5]);
    if !buffer_ok(uaddr, 4, false, true) {
        return err::EFAULT;
    }
    // The operation is the low seven bits; above them sit FUTEX_PRIVATE_FLAG,
    // which says which table to look in - this kernel has exactly one - and
    // FUTEX_CLOCK_REALTIME, which only renames the clock an absolute timeout
    // reads, since every clock here answers off the one tick counter.
    let command = op & 0x7f;
    let realtime = op & nr::FUTEX_CLOCK_REALTIME != 0;

    match command {
        nr::FUTEX_WAIT | nr::FUTEX_WAIT_BITSET => {
            let bitset = if command == nr::FUTEX_WAIT_BITSET {
                if val3 == 0 {
                    return err::EINVAL;
                }
                val3 as u32
            } else {
                crate::sched::futex::BITSET_ANY
            };
            // FUTEX_WAIT's timeout counts from now; the bitset form names a
            // point on a clock, monotonic unless the realtime flag says the
            // wall clock instead.
            let deadline = if timeout_ptr != 0 {
                match futex_deadline(timeout_ptr, command == nr::FUTEX_WAIT, realtime) {
                    Ok(deadline) => deadline,
                    Err(e) => return e,
                }
            } else {
                None
            };
            crate::sched::futex::wait_bitset(uaddr, val as u32, bitset, deadline)
        }
        nr::FUTEX_WAKE => crate::sched::futex::wake(uaddr, val) as isize,
        nr::FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return err::EINVAL;
            }
            crate::sched::futex::wake_bitset(uaddr, val, val3 as u32) as isize
        }
        // Linux has a dozen operations this kernel does not serve, and they are
        // not all the same kind of absence. `FUTEX_REQUEUE` and its compare
        // spelling are how a libc's `pthread_cond_broadcast` hands its waiters
        // to a mutex in one step; `FUTEX_LOCK_PI` and `FUTEX_UNLOCK_PI` are what
        // musl's `LOCK`/`UNLOCK` become once a process that *had* threads is
        // single-threaded again - and musl does not read the result, so a
        // refusal there is a lock that silently does not lock. Saying so once,
        // with the operation's name, is what makes the gap visible; the answers
        // themselves have not been decided here.
        other => {
            super::note_missing_futex_op(other);
            err::ENOSYS
        }
    }
}

/// Turn a futex `struct timespec *` into a tick deadline for
/// [`handle_futex`]. `relative` (FUTEX_WAIT's spelling) counts from now - on
/// whichever clock the flags name, since a duration is a duration; only the
/// bitset waits name a point on a clock, monotonic by default or the wall
/// clock when `realtime` is set - the same wall clock
/// `clock_gettime(CLOCK_REALTIME)` answers, so a caller comparing the two
/// agrees with itself. A deadline already in the past comes back as `Some(0)`
/// and the wait answers `ETIMEDOUT` without sleeping.
fn futex_deadline(ts: usize, relative: bool, realtime: bool) -> Result<Option<u64>, isize> {
    if !buffer_ok(ts, 16, false, true) {
        return Err(err::EFAULT);
    }
    // The caller's timespec sits in *its* memory at whatever alignment its
    // own layout gave it - the kernel only asked that 16 readable bytes be
    // there - so the reads must not assume the 8-byte alignment the type
    // would have in kernel space.
    let sec = unsafe { (ts as *const i64).read_unaligned() };
    let nsec = unsafe { ((ts + 8) as *const i64).read_unaligned() };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(err::EINVAL);
    }
    let hz = crate::arch::timer::TICK_HZ as u128;
    let now_ticks = crate::arch::timer::ticks() as u128;
    let now_ns = now_ticks * 1_000_000_000 / hz;
    let target_ns = sec as u128 * 1_000_000_000 + nsec as u128;
    let delta_ns = if relative {
        target_ns as i128
    } else if realtime {
        // The wall clock is the boot epoch plus the tick counter, which is
        // exactly what clock_gettime(CLOCK_REALTIME) reports.
        let wall_ns = crate::arch::rtc::boot_epoch() as u128 * 1_000_000_000 + now_ns;
        target_ns as i128 - wall_ns as i128
    } else {
        // The monotonic clock *is* the tick counter.
        target_ns as i128 - now_ns as i128
    };
    if delta_ns <= 0 {
        return Ok(Some(0));
    }
    Ok(Some(
        now_ticks as u64 + (delta_ns as u128 * hz / 1_000_000_000) as u64 + 1,
    ))
}

// `sched_getaffinity` / `sched_setaffinity` / the robust-list and rseq
// registrations live in `abi` - see the note above `futex`.

// ---------------------------------------------------------------------------
// The calls an ordinary libc reaches for
// ---------------------------------------------------------------------------

/// `time(tloc)` — seconds since the epoch.
///
/// The original spelling of the clock, and the one a program that only
/// wants `now` still reaches for: `date(1)`, a TLS library's first sanity
/// check, a log line. Same wall clock `clock_gettime(CLOCK_REALTIME)`
/// answers - the RTC anchor plus uptime. `tloc`, when given, is written
/// through as well as returned, which is the shape the man page specifies.
pub(super) fn handle_time(args: [usize; ARG_COUNT]) -> isize {
    let tloc = args[0];
    let secs = crate::arch::rtc::boot_epoch()
        + crate::arch::timer::ticks() / crate::arch::timer::TICK_HZ as u64;
    if tloc != 0 {
        if !buffer_ok(tloc, 8, true, true) {
            return err::EFAULT;
        }
        unsafe {
            (tloc as *mut i64).write_unaligned(secs as i64);
        }
    }
    secs as isize
}

/// Convert an internal tick count (the PIT runs at `TICK_HZ`) into the `USER_HZ`
/// clock ticks Linux's `times()` reports. Linux truncates; so does this. `100`
/// is the `CLK_TCK` musl hardcodes and hands to `sysconf(_SC_CLK_TCK)`, so a
/// caller converting ticks to seconds divides by 100 and must get the same unit
/// back from the kernel - otherwise its CPU arithmetic is off by `TICK_HZ/100`.
fn clock_ticks_to_user_hz(ticks: u64) -> u64 {
    ticks * 100 / crate::arch::timer::TICK_HZ as u64
}

/// `times(buf)` — this process's CPU time, in clock ticks.
///
/// `struct tms { clock_t tms_utime, tms_stime, tms_cutime, tms_cstime; }`, four
/// `long`s. The timer interrupt charges the running task's group `Mm` user or
/// system by the ring it interrupted, so `tms_utime`/`tms_stime` are the calling
/// process's real split CPU, reported in `USER_HZ` ticks; a reaped child's CPU
/// folds into `tms_cutime`/`tms_cstime` at the reap (Linux accumulates on
/// `wait`). The returned value is the elapsed clock, in the same `USER_HZ`
/// unit, so `(utime+stime)/elapsed` is unit-correct.
pub(super) fn handle_times(args: [usize; ARG_COUNT]) -> isize {
    let buf = args[0];
    let elapsed = clock_ticks_to_user_hz(crate::arch::timer::ticks());
    if buf == 0 {
        return elapsed as isize;
    }
    if !buffer_ok(buf, 32, true, true) {
        return err::EFAULT;
    }
    let (utime, stime, cutime, cstime) = crate::sched::current_cpu_ticks();
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 32);
        let put = |off: usize, ticks: u64| {
            ((buf + off) as *mut i64).write_unaligned(clock_ticks_to_user_hz(ticks) as i64)
        };
        put(0, utime);
        put(8, stime);
        put(16, cutime);
        put(24, cstime);
    }
    elapsed as isize
}

/// `getrusage(who, usage)` — resource use for self or for reaped children.
///
/// `struct rusage` opens with two `struct timeval`s (`ru_utime` then `ru_stime`)
/// and follows with fourteen `long` counters. `RUSAGE_SELF` reports the calling
/// process's real split CPU as the timer accumulated it; a `timeval` is absolute
/// wall-clock, so the tick count is divided by `TICK_HZ` for seconds and the
/// remainder scaled to microseconds. The fourteen byte/page counters are not
/// measured, so they stay zero - an honest "nothing measured" rather than a
/// plausible number. `RUSAGE_CHILDREN` reports the CPU of children already waited
/// for, which the reap folded into this process's child totals.
pub(super) fn handle_getrusage(args: [usize; ARG_COUNT]) -> isize {
    const RUSAGE_SELF: usize = 0;
    const RUSAGE_CHILDREN: usize = -1isize as usize;
    const RUSAGE_SIZE: usize = 144; // 2 timevals + 14 longs
    let (who, usage) = (args[0], args[1]);
    if who != RUSAGE_SELF && who != RUSAGE_CHILDREN {
        return err::EINVAL;
    }
    if !buffer_ok(usage, RUSAGE_SIZE, true, true) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(usage as *mut u8, 0, RUSAGE_SIZE);
    }
    // `RUSAGE_SELF` is the process's own split CPU; `RUSAGE_CHILDREN` is the
    // CPU of children already waited for, which the reap folded into the
    // parent's child totals.
    let (utime, stime) = match who {
        RUSAGE_SELF => {
            let (u, s, _, _) = crate::sched::current_cpu_ticks();
            (u, s)
        }
        _ => {
            let (_, _, cu, cs) = crate::sched::current_cpu_ticks();
            (cu, cs)
        }
    };
    let hz = crate::arch::timer::TICK_HZ as u64;
    unsafe {
        let put = |off: usize, ticks: u64| {
            ((usage + off) as *mut i64).write_unaligned((ticks / hz) as i64);
            ((usage + off + 8) as *mut i64).write_unaligned(((ticks % hz) * 1_000_000 / hz) as i64);
        };
        put(0, utime); // ru_utime {sec, usec}
        put(16, stime); // ru_stime {sec, usec}
    }
    0
}

/// `clock_getres(clk_id, res)` — the resolution a clock can deliver.
///
/// `struct timespec { tv_sec, tv_nsec }`. The tick counter is the only
/// source of time there is, so the honest resolution is one tick - a
/// caller that asks is told what it will actually get, rather than the
/// nanosecond a modern Linux advertises. An unknown clock id is `EINVAL`.
pub(super) fn handle_clock_getres(args: [usize; ARG_COUNT]) -> isize {
    const CLOCK_REALTIME: usize = 0;
    const CLOCK_MONOTONIC: usize = 1;
    const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
    const CLOCK_THREAD_CPUTIME_ID: usize = 3;
    const CLOCK_MONOTONIC_RAW: usize = 4;
    const CLOCK_REALTIME_COARSE: usize = 5;
    const CLOCK_MONOTONIC_COARSE: usize = 6;
    const CLOCK_BOOTTIME: usize = 7;
    let (clk_id, res) = (args[0], args[1]);
    if !matches!(
        clk_id,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_MONOTONIC_RAW
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
    ) {
        return err::EINVAL;
    }
    if res != 0 {
        if !buffer_ok(res, 16, true, true) {
            return err::EFAULT;
        }
        let hz = crate::arch::timer::TICK_HZ as u64;
        unsafe {
            (res as *mut i64).write_unaligned(0);
            ((res + 8) as *mut i64).write_unaligned((1_000_000_000 / hz) as i64);
        }
    }
    0
}

/// `waitid(idtype, id, infop, options, rusage)` — wait for one child and
/// describe it as a `siginfo_t`.
///
/// The modern wait: instead of a pid and a packed status word, `WEXITED`
/// and friends say which transitions count and the child arrives as an
/// info record - `si_pid`, `si_status`, and a `si_code` saying whether it
/// exited, was killed, or was stopped. `WNOHANG` answers zero with
/// `si_pid` cleared when no child is ready, which is how a caller polls
/// without blocking.
pub(super) fn handle_waitid(args: [usize; ARG_COUNT]) -> isize {
    use super::abi::SIGINFO_BYTES;
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const WNOHANG: usize = 1;
    const WEXITED: usize = 4;

    let (idtype, _id, infop, options) = (args[0], args[1] as isize, args[2], args[3]);
    if idtype != P_ALL && idtype != P_PID {
        return err::EINVAL;
    }
    if !buffer_ok(infop, SIGINFO_BYTES, true, true) {
        return err::EFAULT;
    }
    // Nothing asked for means nothing to report - not a blank wait.
    if options & WEXITED == 0 {
        unsafe {
            core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
        }
        return 0;
    }

    let me = crate::sched::current();
    let collected = crate::sched::poll_any_child(me);
    let (pid, code) = match collected {
        Some(found) => found,
        None if options & WNOHANG != 0 => {
            // A poll that found nobody: success, with si_pid cleared so the
            // caller can tell "no child yet" from "this child".
            unsafe {
                core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
            }
            return 0;
        }
        None => {
            // Blocking, so wait for somebody - but only if there *is*
            // somebody. A caller with no child at all is `ECHILD`, not a
            // sleep that nothing can ever end; `wait4` has asked this guard
            // all along, and `waitid` was the one arm without it, so a
            // second wait for an already-collected child parked forever.
            if !crate::sched::has_children(me) {
                return err::ECHILD;
            }
            // `poll_any_child` already reported nobody ready; park until one
            // of them finishes.
            match crate::sched::wait_any_child(me) {
                Some(found) => found,
                None => return err::ECHILD,
            }
        }
    };

    // The siginfo layout - SIGCHLD as the signo, CLD_EXITED or CLD_KILLED
    // as the code, pid and status in their fields - is the ABI the libc's
    // `W` macros and `waitid` wrappers read; it is spelled in `abi`, tested
    // on the host.
    abi::write_waitid_info(infop, pid.0 as u32, code)
}

// The legacy `getrlimit`/`setrlimit` pair, `prlimit64` and `membarrier`
// live in `abi` with the rest of the table-answer calls.
