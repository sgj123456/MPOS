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
use crate::hal::UserFrameOps;
use crate::sched::TaskId;
use crate::syscall::{ARG_COUNT, argv_arg, buffer_ok, cstr_arg, err, err_of};

pub(super) use crate::syscall::MMAP_BASE;

/// The ABI's register file, at the machine's width - the same type the
/// scheduler carries a stopped task's registers in (`hal::AbiRegs`), so the two
/// layers spell it from the machine's own count and not from each other.
type AbiRegs = crate::hal::AbiRegs;

/// `SI_USER`: the signal came from `kill`, not from the hardware. It is what a
/// handler reads out of `si_code` to tell a deliberate signal from a fault.
const SI_USER: i32 = 0;

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
/// The frame's *shape* is the machine's (`hal::SignalFrame`); what stays here is
/// the policy around it - which stack, which mask, which restorer - and the two
/// scheduler calls a delivery needs, which the machine cannot make: writing the
/// bytes into the process's own memory, and installing the handler's mask.
///
/// Returns `false` when the frame cannot be written - the stack is not mapped,
/// or not writable, or the disposition has no restorer - which leaves the
/// caller to take the default action rather than to lose the signal.
#[cfg(mpos_signal_frame)]
fn build_frame(
    regs_ptr: *mut (),
    sig: u8,
    action: &crate::sched::signal::Action,
    info: &crate::sched::signal::SigInfo,
    ret: isize,
    restart: Option<u64>,
) -> bool {
    let alt = crate::sched::signal_alt_stack(crate::sched::current());
    let blocked = crate::sched::current_blocked();
    let delivery = crate::hal::SignalDelivery {
        handler: action.handler,
        // A disposition with no restorer cannot be run at all on this machine -
        // the ABI makes the frame's `pretcode` mandatory - and `SA_RESTORER` is
        // the disposition's own statement that it has one. `None` is what tells
        // the machine's frame builder there is nothing to write.
        restorer: (action.flags & crate::sched::signal::SA_RESTORER != 0 && action.restorer != 0)
            .then_some(action.restorer),
        restart,
        blocked_wire: crate::sched::signal::to_wire(blocked),
        info: crate::hal::SignalInfo {
            signo: info.signo,
            errno: info.errno,
            code: info.code,
            pid: info.pid,
            uid: info.uid,
            value: info.value,
            status: info.status,
            utime: info.utime,
            stime: info.stime,
        },
        alt: alt.map(|stack| (stack.sp, stack.size)),
        on_alt: action.flags & crate::sched::signal::SA_ONSTACK != 0,
    };
    // The interrupted call's register file, with `ret` - its own answer - in the
    // result slot. That is what the frame saves, or what the restart number
    // replaces.
    let mut regs = capture_frame(regs_ptr, ret as usize);
    let mut bytes = [0u8; crate::hal::SIGNAL_FRAME_BYTES];
    // `regs_ptr` goes in as well, because a machine may keep the interrupted
    // floating-point state in its own trap frame rather than in the task's
    // record - aarch64 does, and the record there belongs to the context switch
    // (`SignalFrame::enter`'s `frame` parameter).
    // SAFETY: `regs_ptr` is the gate's own frame for this task, which is the
    // contract `hal::enter_signal_frame` documents.
    let at = unsafe { crate::hal::enter_signal_frame(&mut regs, &mut bytes, &delivery, regs_ptr) };
    let Some(at) = at else {
        return false;
    };
    if !crate::sched::write_user(at.sp, &bytes) {
        return false;
    }
    // `regs` now describes the handler's first instruction, and `apply_frame` is
    // what puts it into the gate frame - the return address, the stack, and the
    // three argument registers, which only the machine knows where to put.
    apply_frame(regs_ptr, &regs);
    crate::sched::set_current_blocked(action.handler_mask(sig, blocked));
    if action.flags & crate::sched::signal::SA_RESETHAND != 0 {
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
/// * the call is one Linux rewinds at all - `abi::restartable` for every
///   syscall but `ioctl`, whose verdict is per *request*
///   (`abi::ioctl_restartable`) because the driver's own return decides it.
///   `poll`, `select`, `epoll_wait` and the sleeps answer `EINTR` whatever the
///   disposition says, and rewinding them would be a handler loop that never
///   returns.
///
/// What the caller sees when all three hold, measured on Linux 7.2 with a
/// terminal read whose `SIGALRM` fired at 100 ms and whose byte arrived at
/// 400 ms: the handler runs at 100 ms, the *read* returns the byte at 400 ms,
/// and `EINTR` never appears. Without the flag the same call answers
/// `-1/EINTR` at 100 ms.
#[cfg(mpos_signal_frame)]
fn restart_verdict(
    nr: u64,
    request: u32,
    ret: isize,
    action: &crate::sched::signal::Action,
) -> Option<u64> {
    let asked = action.flags & crate::sched::signal::SA_RESTART != 0;
    // `ioctl` is the one call whose verdict is per request rather than per
    // number: `restartable(nr)` deliberately leaves `nr::IOCTL` out, and the
    // request word (the second argument, `regs.rsi`) decides instead.
    let restartable = if nr == nr::IOCTL {
        abi::ioctl_restartable(request)
    } else {
        abi::restartable(nr)
    };
    (ret == err::EINTR && asked && restartable).then_some(nr)
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
#[cfg(mpos_signal_frame)]
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
    // The second argument, which is the request word for an `ioctl` and
    // meaningless for every other number `restart_verdict` asks about. The
    // frame is still the syscall-entry one here: the tracer and group-stop
    // paths above returned, and `build_frame` - which rewrites it - runs only
    // after this reads it.
    // SAFETY: `regs_ptr` is the gate's own frame for this task, which is
    // the contract `hal::syscall_args` documents.
    let request = unsafe { crate::hal::syscall_args(regs_ptr) }[1] as u32;
    if !build_frame(
        regs_ptr,
        sig,
        &action,
        &info,
        ret,
        restart_verdict(nr, request, ret, &action),
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
/// Where the frame starts depends on the trampoline, and that is the machine's
/// (`SignalFrame::frame_at`): x86_64's `__restore_rt` is entered by the
/// handler's `ret`, so the restorer word sits at `rsp` and the frame is eight
/// bytes below it; aarch64's restorer is reached through `x30` and pushed
/// nothing, so the frame *is* what `sp` points at. Reading that frame back and
/// restoring the registers it saved is the machine's too; the mask it puts back
/// is the scheduler's.
#[cfg(mpos_signal_frame)]
pub(super) fn handle_rt_sigreturn(regs_ptr: *mut ()) -> isize {
    // A null frame is a call the kernel made on its own behalf: there is no
    // stack pointer to find a frame at.
    let Some(ctx) = (unsafe { crate::hal::user_context(regs_ptr, 0) }) else {
        return err::EFAULT;
    };
    let frame = crate::hal::signal_frame_at(ctx.sp());
    let mut bytes = [0u8; crate::hal::SIGNAL_FRAME_BYTES];
    if !crate::sched::read_user(frame, &mut bytes) {
        return err::EFAULT;
    }
    // Seeded from the running task's own register file, so the slots the frame
    // does not carry - the thread pointer above all - come back unchanged.
    let mut regs = capture_frame(regs_ptr, 0);
    // SAFETY: as `build_frame`'s, for the `rt_sigreturn` trap's own frame.
    let mask = unsafe { crate::hal::exit_signal_frame(&bytes, &mut regs, regs_ptr) };
    // The frame's saved mask is the wire's `sigset_t` - a libc reads it as one -
    // so it comes back through the same conversion the install went out
    // through.
    crate::sched::set_current_blocked(crate::sched::signal::from_wire(mask));
    // The answer is the interrupted call's own value, which the restored file
    // carries in `rax`: the gate writes it into the result slot and `sysretq`
    // resumes the interrupted instruction with it.
    apply_frame(regs_ptr, &regs) as isize
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
    // `gettid` is the call that answers a thread's own. The number is the
    // caller's namespace's, and the leader is a member of it by construction (a
    // thread is in its process's namespaces), so the fallback is unreachable -
    // it is here so a machine that somehow disagreed would answer a number
    // rather than stop.
    let tgid = crate::sched::current_tgid();
    crate::sched::pid_number_of(tgid).unwrap_or_else(crate::sched::current_pid_number) as isize
}

/// `gettid()` - the calling *thread*'s own id.
///
/// This used to be an alias for `getpid`, which was harmless while every
/// process was one thread and became wrong the moment threads existed: a
/// thread that asks its own id must not be told the process's.
pub(super) fn handle_gettid() -> isize {
    // The thread's *own* number, in the namespace it stands in - a thread's tid
    // is namespaced exactly as a process's pid is.
    crate::sched::current_pid_number() as isize
}

/// The task a caller's `pid` names, **in the caller's own namespace**.
///
/// Every call that takes a pid goes through this instead of building a `TaskId`
/// from the number, and that is the half of the feature that keeps a namespace
/// from being a footgun: a pid here *is* a task-table slot, a namespace gives
/// that slot a different number, so a number the caller wrote has to be read in
/// the namespace the caller is standing in. Without it, a program whose
/// `getpid()` answered 1 inside a namespace and which then called
/// `kill(getpid(), sig)` - which is what a program does with its own pid - would
/// reach the machine's slot 1, the boot's init.
///
/// `None` is "names nothing here", which every caller turns into its own refusal
/// (`ESRCH`, or `ECHILD` for `wait`). It also replaces the old
/// `pid >= MAX_TASKS` bounds: those were the *init* namespace's way of saying
/// "no such task", and a namespace's numbers are its own, so the table lookup
/// is the only honest bound.
pub(crate) fn pid_task(pid: isize) -> Option<TaskId> {
    crate::sched::resolve_pid(pid as i64)
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
    // `wait4`'s fourth argument is where the child's `struct rusage` goes. It is
    // read here and threaded to every answer a **report** produces - a death, a
    // stop and a resume alike - because Linux fills it for all three; the
    // "nothing yet" zero and the refusals leave it alone. See `write_wait_rusage`.
    let rusage = args[3];
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
        let collected: Option<crate::sched::WaitEvent> = if no_hang {
            crate::sched::poll_any_child_event(me, stops, continues)
        } else if crate::sched::has_children(me) {
            match crate::sched::wait_any_child_event(me, stops, continues) {
                crate::sched::WaitPark::Event(event) => Some(event),
                crate::sched::WaitPark::NoChild => None,
                crate::sched::WaitPark::Interrupted => return err::EINTR,
            }
        } else {
            None
        };
        if let Some(event) = collected {
            return finish_wait_event(event, status, rusage, from_user);
        }
        // Nothing to report: an error only when there is nobody left to
        // wait for, and otherwise the "none yet" zero.
        if no_hang && crate::sched::has_children(me) {
            return 0;
        }
        return err::ECHILD;
    }

    // A named child, resolved in the caller's namespace: a number that names
    // nothing there is nobody to wait for, which is `ECHILD` and not a wrong
    // task. The old `MAX_TASKS` bound went with the translation - it was the
    // init namespace's spelling of "no such task" (see `pid_task`).
    let Some(id) = pid_task(pid) else {
        return err::ECHILD;
    };

    // A stop first, then a death: a tracee that stopped on its way to exiting
    // has news of both, and the stop is the earlier news and the one that
    // gates everything after it.
    if let Some(event) = crate::sched::take_child_event(id, stops, continues) {
        return finish_wait_event(event, status, rusage, from_user);
    }
    // A code already on file means the child is a zombie: it is collected now,
    // blocking or not. Tracer-owned records stay with the tracer: this caller
    // collecting through a tracer's corpse is the double-report that hung
    // strace.
    if let Some(retired) = crate::sched::take_retired_current(id) {
        return finish_wait(
            id,
            retired.code,
            retired.dumped,
            (retired.utime, retired.stime),
            status,
            rusage,
            from_user,
        );
    }
    if !crate::sched::is_child(id, me) {
        return err::ECHILD;
    }
    if no_hang {
        return 0;
    }

    match crate::sched::block_until_child_event(id, stops, continues) {
        crate::sched::WaitPark::Event(event) => finish_wait_event(event, status, rusage, from_user),
        crate::sched::WaitPark::Interrupted => err::EINTR,
        crate::sched::WaitPark::NoChild => err::ECHILD,
    }
}

/// The three shapes a successful `wait` returns, each encoded into `*status` the
/// way its `W`-macro decodes it: a death, a stop, or a resume.
///
/// All three also fill the caller's `struct rusage` when it asked for one - the
/// event carries the child's CPU for exactly that (`WaitEvent`'s own note).
fn finish_wait_event(
    event: crate::sched::WaitEvent,
    status: usize,
    rusage: usize,
    from_user: bool,
) -> isize {
    match event {
        crate::sched::WaitEvent::Exited {
            id,
            code,
            dumped,
            utime,
            stime,
            ..
        } => finish_wait(id, code, dumped, (utime, stime), status, rusage, from_user),
        // `traced` is `waitid`'s question, not this door's: the status word
        // `WSTOPSIG`/`WIFSTOPPED` read says "stopped" either way, and a traced
        // stop's `PTRACE_EVENT_*` is what carries the rest.
        crate::sched::WaitEvent::Stopped {
            id,
            signal,
            event,
            utime,
            stime,
            ..
        } => write_wait_status(
            id,
            abi::stop_status(signal, event),
            (utime, stime),
            status,
            rusage,
            from_user,
        ),
        crate::sched::WaitEvent::Continued { id, utime, stime } => write_wait_status(
            id,
            abi::CONTINUED_STATUS,
            (utime, stime),
            status,
            rusage,
            from_user,
        ),
    }
}

/// Encode an already-formed status word and answer the child's pid. The
/// counterpart of [`finish_wait`] for the two events that are not a death.
fn write_wait_status(
    id: TaskId,
    word: i32,
    cpu: (u64, u64),
    status: usize,
    rusage: usize,
    from_user: bool,
) -> isize {
    if status != 0 {
        if !buffer_ok(status, 4, true, from_user) {
            return err::EFAULT;
        }
        unsafe { (status as *mut i32).write_unaligned(word) };
    }
    match write_wait_rusage(rusage, cpu, from_user) {
        Some(e) => e,
        None => wait_answer(id),
    }
}

/// The pid a `wait` answers with: the child's number **in the caller's
/// namespace**. This is the one place both wait shapes answer a pid - `wait4`'s
/// return value and `waitid`'s record go through it - so the number a parent
/// reads is the number it can use in `kill`, and a parent one level out reads
/// the number it can see rather than the child's own.
fn wait_answer(id: TaskId) -> isize {
    crate::sched::pid_number_of(id).map_or(id.0 as isize, |n| n as isize)
}

/// The two outputs of a successful `wait`: the child's pid as the return value,
/// and the exit encoded into `*status`. A null `status` is "not interested" and
/// writes nothing, which is what the man page says.
///
/// `dumped` is the death's core-dump classification, taken off the record with
/// the code: the `WCOREDUMP` bit is the same `CLD_DUMPED` fact `waitid` writes
/// into `si_code`, so one death cannot read two ways.
///
/// `cpu` is the dead group's own-plus-children tick pair, off the record for the
/// same reason. `rusage` goes after `*status`, which is the order Linux's own
/// wrapper uses (`copy_to_user` runs once the child is reported, after the wait
/// wrote the status) - so a caller that hands an unwritable `rusage` pointer is
/// answered `EFAULT` with its status word already written and the child already
/// collected. Measured, `.workbuddy/probe_wait4_rusage.c` scenario N.
fn finish_wait(
    id: TaskId,
    code: isize,
    dumped: bool,
    cpu: (u64, u64),
    status: usize,
    rusage: usize,
    from_user: bool,
) -> isize {
    if status != 0 {
        if !buffer_ok(status, 4, true, from_user) {
            return err::EFAULT;
        }
        // The W-exit-status encoding is the ABI the libc's macros decode;
        // it is spelled in `abi`, tested on the host.
        unsafe { (status as *mut i32).write_unaligned(abi::wait_status(code, dumped)) };
    }
    match write_wait_rusage(rusage, cpu, from_user) {
        Some(e) => e,
        None => wait_answer(id),
    }
}

/// Fill the caller's `struct rusage` for a child a `wait` just reported, or
/// answer `None` when there is nothing to do.
///
/// A null pointer is "not interested" on both wait doors - the shape rustix's
/// `waitpid` passes - and writes nothing. Otherwise the child's own-plus-
/// children CPU goes into the two opening `timeval`s; see [`write_rusage`].
///
/// Only a *report* fills it. Linux's `wait4` copies the struct when the wait
/// returned a pid and `waitid` when it reported an event, so the `WNOHANG`
/// "nothing yet" zero and `ECHILD` leave the caller's bytes alone (measured,
/// `.workbuddy/probe_wait4_rusage.c` scenarios J, K, L and M, Linux 7.2.9: all
/// four leave a 0xAA-filled struct untouched).
fn write_wait_rusage(ptr: usize, cpu: (u64, u64), from_user: bool) -> Option<isize> {
    if ptr == 0 {
        return None;
    }
    write_rusage(ptr, cpu.0, cpu.1, from_user)
}

/// Write `struct rusage` from a pair of internal clock ticks: the two opening
/// `struct timeval`s (`ru_utime` then `ru_stime`) over an otherwise zeroed
/// struct.
///
/// Both wait doors and `getrusage` call this, because seconds-and-microseconds
/// off a tick count is one derivation and not three. The fourteen `long`
/// byte/page counters stay zero - this kernel measures CPU and nothing else, so
/// an honest zero rather than a plausible number, which is the residual
/// `getrusage` has carried since it landed and now carries here too.
///
/// `None` when the struct was written, or the errno to answer with when the
/// pointer is not writeable. A failed check leaves the caller's bytes alone; the
/// zeroing happens after it.
fn write_rusage(usage: usize, utime: u64, stime: u64, from_user: bool) -> Option<isize> {
    const RUSAGE_BYTES: usize = 144; // 2 timevals + 14 longs
    if !buffer_ok(usage, RUSAGE_BYTES, true, from_user) {
        return Some(err::EFAULT);
    }
    let hz = crate::hal::TICK_HZ as u64;
    unsafe {
        core::ptr::write_bytes(usage as *mut u8, 0, RUSAGE_BYTES);
        let put = |off: usize, ticks: u64| {
            ((usage + off) as *mut i64).write_unaligned((ticks / hz) as i64);
            ((usage + off + 8) as *mut i64).write_unaligned(((ticks % hz) * 1_000_000 / hz) as i64);
        };
        put(0, utime); // ru_utime {sec, usec}
        put(16, stime); // ru_stime {sec, usec}
    }
    None
}

// ---------------------------------------------------------------------------
// ptrace(2): the tracer's half
// ---------------------------------------------------------------------------

/// The tracee's user register file, in the ABI's field order, as the gate's
/// frame will restore it.
///
/// The file itself is the machine's to read, and this asks for it through
/// `hal`, which is the only layer that knows which slot of the gate frame
/// holds which register.
///
/// `result` is passed in rather than read: it is the value the ABI's result
/// slot must *show* at the stop being taken. At a syscall exit that is the
/// call's return value, which the gate has not written into the frame yet; at
/// an entry it is whatever the machine answers for that moment
/// ([`crate::hal::syscall_entry_result_slot`]) - the call's number on x86_64,
/// where `syscall` left it in `rax`, and the first argument on aarch64, where
/// the number is in `x8` and the slot was never the number's to begin with.
/// The caller is the only one that knows which of those it is holding.
///
/// The file is written into a zeroed buffer, so the slots the machine leaves
/// alone - the segment registers long mode ignores, and the user GS base this
/// kernel never sets - read as the zero the CPU would ignore anyway.
fn capture_frame(regs_ptr: *mut (), result: usize) -> AbiRegs {
    let mut regs = [0u64; crate::hal::ABI_REG_COUNT];
    // SAFETY: the caller hands the gate's own frame for this task, which is the
    // contract `hal::capture_user_regs` documents.
    unsafe {
        crate::hal::capture_user_regs(regs_ptr, result, crate::sched::current_tls(), &mut regs);
    }
    regs
}

/// Write a register file back into the gate's frame and answer the value the
/// gate should return to ring 3.
///
/// The inverse of [`capture_frame`], and likewise the machine's to perform:
/// `hal`'s `restore_user_regs` puts each slot where its own frame needs it - on
/// x86_64 that means `rcx`/`r11`, which is where `sysretq` takes the return
/// address and the flags from - and answers the ABI's `rax`. That value is
/// *returned* rather than written into the frame, and that is not a shortcut:
/// the gate stores the dispatch's result into the `rax` slot after this
/// function's caller returns, so the only way a tracer's `PTRACE_SETREGS` can
/// change what the tracee sees there is for the new value to *be* the result.
///
/// The thread pointer comes back separately, because installing it is the
/// scheduler's and not the machine's - and because not every machine's register
/// file *has* one. It is seeded with the value the task is running with, so a
/// machine that has no such slot leaves it alone rather than answering with a
/// zero the caller invented; x86_64's frame carries `fs_base` and overwrites it
/// either way. Measured: with a zero seed, the first signal handler delivered on
/// aarch64 left the task's `TPIDR_EL0` at 0 and the very next `println!` faulted
/// reading musl's `errno` at `0x58` - NULL plus the offset the libc keeps it at.
fn apply_frame(regs_ptr: *mut (), regs: &AbiRegs) -> usize {
    let mut tls = crate::sched::current_tls();
    // SAFETY: as [`capture_frame`].
    let rax = unsafe { crate::hal::restore_user_regs(regs_ptr, regs, &mut tls) };
    if crate::sched::current_tls() != tls {
        crate::sched::set_current_tls(tls);
    }
    rax as usize
}

/// The call's number *after* a tracer has seen the frame, written back into the
/// frame the gate reads.
///
/// The write-back is the whole of making a redirect stick. On x86_64
/// `restore_user_regs` does not carry `orig_rax` - the number and the result
/// share `rax`, and the result is the gate's to write after the handler - so a
/// number a tracer wrote into the register file would be dropped here, before
/// the gate ever saw it. On aarch64 `restore_user_regs` already carried `x8`,
/// and this writes the same value a second time rather than a different one.
fn number_after_stop(regs_ptr: *mut (), regs: &AbiRegs) -> u64 {
    let nr = regs[abi::uregs::NUMBER];
    // SAFETY: the same gate frame `capture_frame` was handed.
    unsafe { crate::hal::set_syscall_number(regs_ptr, nr) };
    nr
}

/// The tracee's half of a syscall-entry stop: show the tracer the call before it
/// is made, and answer the number and the arguments the tracee will actually
/// make it with.
///
/// Re-reading both rather than returning what was handed in is the point of the
/// exercise: a tracer that rewrote `rdi` - or the number - at the stop meant to
/// change the call, and a kernel that then made the call with the old value
/// would be ignoring it. Redirecting the number is what a syscall-table sandbox
/// is built from, and Linux honours it at this stop (measured on the developer's
/// 7.2 kernel: a judged `getpid` whose number slot was rewritten to `getppid`
/// answered its parent's pid, and the stop after it read the new number back).
pub(crate) fn syscall_entry_stop(
    regs_ptr: *mut (),
    nr: u64,
    args: [usize; ARG_COUNT],
) -> (u64, [usize; ARG_COUNT]) {
    if !crate::sched::has_tracer(crate::sched::current()) {
        return (nr, args);
    }
    // A null frame is a call the kernel made on its own behalf: no register
    // file exists for a tracer to have rewritten.
    let Some(ctx) = (unsafe { crate::hal::user_context(regs_ptr, 0) }) else {
        return (nr, args);
    };
    let (ip, sp) = (ctx.ip() as u64, ctx.sp() as u64);
    // What the ABI's result slot must read at *this* moment, which is not the
    // number on both machines: see `Gate::syscall_entry_result_slot`. Asking the
    // machine rather than writing `nr` here is what keeps the tracee's first
    // argument - the same slot on aarch64 - from being replaced by the call's
    // number, a substitution the discharge below would then make the call with.
    // SAFETY: the same gate frame `ctx` came from.
    let slot = unsafe { crate::hal::syscall_entry_result_slot(regs_ptr, nr as usize) };
    let mut regs = capture_frame(regs_ptr, slot);
    let wide = args.map(|arg| arg as u64);
    crate::sched::stop_at_syscall(true, &mut regs, nr, &wide, ip, sp);
    apply_frame(regs_ptr, &regs);
    // Both halves of the call are the frame's now, not the ones handed in.
    let nr = number_after_stop(regs_ptr, &regs);
    // SAFETY: the same gate frame `capture_frame` was just handed.
    let args = unsafe { crate::hal::syscall_args(regs_ptr) };
    (nr, args)
}

/// The tracee's half of a seccomp stop: hold the task before a
/// `SECCOMP_RET_TRACE`-selected call is made, and let the tracer see the frame.
///
/// The frame is still the syscall-entry one, so the result slot is what
/// `syscall_entry_stop` leaves there - the call's number, not a result. That is
/// why this asks the machine for the slot rather than writing a zero: a tracer
/// that reads `rax` at a seccomp stop on x86_64 must find the syscall number,
/// exactly as it would at an entry stop.
pub(crate) fn seccomp_trace_stop(regs_ptr: *mut (), nr: u64, message: u64) {
    // A null frame is a call the kernel made on its own behalf, with no
    // register file for a tracer to look at. The call proceeds untraced rather
    // than faulting on the null.
    if regs_ptr.is_null() {
        return;
    }
    // SAFETY: `regs_ptr` is the gate's own frame for this task, which is the
    // contract the callers of the syscall stops already hold.
    let slot = unsafe { crate::hal::syscall_entry_result_slot(regs_ptr, nr as usize) };
    let mut regs = capture_frame(regs_ptr, slot);
    crate::sched::seccomp_trap(&mut regs, message);
    apply_frame(regs_ptr, &regs);
    // A tracer that redirected the call rewrote the number slot, and it has to
    // reach the frame the gate reads next - see [`number_after_stop`] for why
    // the frame does not carry it on its own.
    number_after_stop(regs_ptr, &regs);
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
    if !crate::sched::has_tracer(crate::sched::current()) {
        return ret;
    }
    let Some(ctx) = (unsafe { crate::hal::user_context(regs_ptr, 0) }) else {
        return ret;
    };
    let (ip, sp) = (ctx.ip() as u64, ctx.sp() as u64);
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
///
/// Served on both machines, and the gate it used to carry was the stale kind.
/// What the request words name is one protocol (`abi`'s table is the same
/// numbers on both), and the only part that is really the machine's is *which
/// slot of the register file a role lives in* - the result, the instruction
/// pointer, the stack pointer - which `abi::uregs` answers per column
/// (`RESULT`/`IP`/`STACK`). Everything else here reads the target through the
/// scheduler (`trace_regs`, `set_trace_regs`, `read_user_of`), and both machines
/// implement `hal::capture_user_regs`/`restore_user_regs`, so nothing below
/// names a register.
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

    let Some(id) = pid_task(pid) else {
        return err::ESRCH;
    };

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
        //
        // The word *is* the return value. `data` is not a buffer this call
        // fills, which is why a tracer's `ptrace` binding has a variadic shape
        // and why a caller reads the result rather than passing somewhere to put
        // it: measured, the version that wrote to `*(u64 *)data` answered
        // `EFAULT` to the `ptrace(PEEKDATA, pid, addr, 0)` every libc actually
        // makes, because zero is not a writable address.
        abi::PTRACE_PEEKTEXT | abi::PTRACE_PEEKDATA => {
            let Some(root) = crate::sched::space_root_of(id) else {
                return err::ESRCH;
            };
            let mut word = [0u8; 8];
            if !crate::sched::read_user_of(root, addr, &mut word) {
                return err::EIO;
            }
            // Signed, because that is the width the ABI answers with and the
            // dispatcher carries; a word whose top bit is set is a negative
            // `long` to the caller, which is Linux's own behaviour and why a
            // tracer checks `errno` rather than the sign.
            i64::from_le_bytes(word) as isize
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
        // it, and on both machines the register file is that structure's first
        // member (`user_regs_struct` here, `user_pt_regs` there), so an offset
        // inside the register file is the only one that names anything.
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
                // And this one answers the same way `PEEKDATA` does: the word is
                // the result, not a value written through a pointer.
                return regs[addr / 8] as isize;
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
            if len < abi::uregs::BYTES {
                // A short buffer is not a request for a short answer: the
                // register set is one structure, and half of it is a tracer
                // acting on registers it did not read. (Linux truncates to what
                // the caller offered; refusing is the stricter reading of the
                // same rule and the one a tracer cannot mistake.)
                return err::EIO;
            }
            if request == abi::PTRACE_GETREGSET {
                let Some(regs) = crate::sched::trace_regs(id) else {
                    return err::ESRCH;
                };
                if !buffer_ok(base, abi::uregs::BYTES, true, from_user) {
                    return err::EFAULT;
                }
                abi::write_user_regs(base, &regs);
            } else {
                if !buffer_ok(base, abi::uregs::BYTES, false, from_user) {
                    return err::EFAULT;
                }
                if !crate::sched::set_trace_regs(id, &abi::read_user_regs(base)) {
                    return err::ESRCH;
                }
            }
            // The length that was actually moved goes back into the caller's
            // `iovec`, the way Linux leaves it: it is how a tracer tells a full
            // answer from a truncated one, and a kernel that left the field
            // untouched would be answering a question the caller cannot read.
            if !buffer_ok(data, 16, true, from_user) {
                return err::EFAULT;
            }
            unsafe { ((data + 8) as *mut u64).write_unaligned(abi::uregs::BYTES as u64) };
            0
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
                    // rewrote the result slot sees its own value here too.
                    let ret = crate::sched::trace_regs(id)
                        .map_or(0, |regs| regs[abi::uregs::RESULT] as i64)
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
                        (regs[abi::uregs::IP], regs[abi::uregs::STACK])
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
            // wrong answer. The option table is the ABI's, so its mask is too.
            if data & !abi::PTRACE_OPTIONS_KNOWN != 0 {
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
        // register file - and what a tracer gets here is the same block the
        // context switch moves, spelled the way each machine's ABI spells it:
        // on x86_64 `user_fpregs_struct` *is* the 512-byte `fxsave` image, while
        // aarch64's `user_fpsimd_state` is the switch's 528 bytes with the
        // `fpsr`/`fpcr` pair in the other order (`abi`'s conversion says where).
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
///
/// Served on both machines, and the gate it used to carry was stale: nothing
/// here is x86_64's. It walks the two arrays ([`transfer_iovs`], always
/// ungated), asks the scheduler for the target's address-space root
/// ([`crate::sched::space_root_of`]) and the permission `PTRACE_ATTACH` would
/// ask ([`crate::sched::may_inspect`]), and hands each chunk to
/// [`crate::sched::read_user_of`]. The `ptrace` *handler* beside it stays
/// gated because the x86 request numbers and `user_regs_struct` really are the
/// architecture's; this pair is not that, and the aarch64 number table has
/// named `PROCESS_VM_READV`/`PROCESS_VM_WRITEV` all along.
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
    let Some(id) = pid_task(pid) else {
        return err::ESRCH;
    };
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

/// The writing half: `process_vm_writev`. Served wherever the reading half is,
/// for the same reason and with the same permission check.
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
    let Some(id) = pid_task(pid) else {
        return err::ESRCH;
    };
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
    // The parent's number *in the caller's namespace* - which is Linux's rule
    // and the one a container's init meets: the task that unshared has a parent
    // outside the new namespace, and the new namespace has no number for it, so
    // the answer is 0 rather than a number from a namespace the caller cannot
    // see.
    crate::sched::parent_of(crate::sched::current())
        .and_then(crate::sched::pid_number_of)
        .map_or(0, |n| n as isize)
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
            // The old image is gone; `execve` must not *return* into it. It
            // returns normally only as a number, so the way it takes the process
            // to the new program is to rewrite the trap frame the syscall gate
            // will `ret`/`eret` out through - the entry becomes the new program's
            // first instruction, the stack pointer its new stack, and the caller
            // sees no return value at all.
            //
            // *Which* registers say that is the machine's, and each machine's
            // `hal::Gate` knows its own, so what is left here is the one
            // machine-neutral thing there is to say about this call: it does not
            // return.
            if !regs_ptr.is_null() {
                // SAFETY: `regs_ptr` is the gate's own frame for this task.
                unsafe { crate::hal::set_exec_return(regs_ptr, entry, new_rsp) };
            }
            // A traced task traps here, on the *new* image's first instruction:
            // by the time `execve` could otherwise "return" the old program is
            // gone, so the trap is the only honest way to tell a tracer that the
            // exec happened. It is also what a tracer is waiting for the very
            // first time - `strace`'s child calls `PTRACE_TRACEME` and then
            // `execv`s, and this stop is how the tracer learns the program
            // started. A no-op for a task no tracer has touched.
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
    let ticks = (total_ms * crate::hal::TICK_HZ as u64 / 1000).max(1);
    let hz = crate::hal::TICK_HZ as u64;
    let deadline = crate::hal::ticks().saturating_add(ticks);
    match crate::sched::block_until_deadline(|| false, deadline) {
        crate::sched::WaitEnd::Interrupted => {
            if rem != 0 {
                if !buffer_ok(rem, 16, true, from_user) {
                    return err::EFAULT;
                }
                let left = deadline.saturating_sub(crate::hal::ticks());
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
    // SAFETY: the caller hands the gate's own frame for this task, which is the
    // contract `hal::user_context` documents.
    let ctx = match unsafe { crate::hal::user_context(regs_ptr, 0) } {
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

    // SAFETY: the caller hands the gate's own frame for this task.
    let ctx = match unsafe { crate::hal::user_context(regs_ptr, 0) } {
        Some(ctx) => ctx,
        None => return err::EINVAL,
    };

    // A thread's stack is its own, so `child_stack` picks where it starts; a
    // process ignores the argument and returns from the call on its own stack,
    // exactly as `fork` does.
    // A fresh network namespace for the child. Inherited descriptors are no
    // problem: a socket remembers the stack it was made in (`net`'s `HOMES`), so
    // one the child inherits goes on working against *that* stack, exactly as it
    // does on Linux. Minted here rather than in the scheduler, because making a
    // namespace means naming `net` and `sched` must not (see `Task::net_ns`).
    let net_ns = if flags & crate::abi::CLONE_NEWNET != 0 {
        Some(crate::net::mint_namespace())
    } else {
        None
    };
    let request = crate::sched::CloneRequest {
        flags,
        child_stack,
        child_tid,
        tls,
        net_ns,
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
    // The tracer announcement is a no-op for a task no tracer has touched,
    // which is every task on a machine without one: `tell_tracer_of_child`
    // finds no pending event and returns.
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
    if sig > crate::sched::signal::MAX_SIGNAL as usize {
        return err::EINVAL;
    }
    let delivered = match id {
        n if n < 0 => {
            let pgid = if n == -1 {
                crate::sched::current_pgid()
            } else {
                // A group number is a *pid* number in the caller's namespace -
                // the number of the process that leads the group - so it is
                // resolved there too and the group is the one that task leads.
                let Some(leader) = pid_task(n.unsigned_abs() as isize) else {
                    return err::ESRCH;
                };
                match crate::sched::pgid_of(leader) {
                    Some(pgid) => pgid,
                    None => return err::ESRCH,
                }
            };
            crate::sched::send_signal_group(pgid, sig as u8)
        }
        0 => crate::sched::send_signal_group(crate::sched::current_pgid(), sig as u8),
        n => match pid_task(n) {
            Some(id) => crate::sched::send_signal(id, sig as u8),
            None => return err::ESRCH,
        },
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
        n if n > 0 => match pid_task(n) {
            Some(id) => id,
            None => return err::ESRCH,
        },
        _ => return err::EINVAL,
    };
    let group = match pgid {
        0 => target,
        n if n > 0 => match pid_task(n) {
            Some(id) => id,
            None => return err::ESRCH,
        },
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
            crate::sched::pid_number_of(me).unwrap_or_else(crate::sched::current_pid_number)
                as isize
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
        n if n > 0 => match pid_task(n) {
            Some(id) => id,
            None => return err::ESRCH,
        },
        _ => return err::EINVAL,
    };
    // Linux answers any live pid; only a dead one is ESRCH. The id answered is
    // the *caller's namespace's* number for the group's leader, which is the
    // number the caller could use in `kill(-n)`.
    match crate::sched::pgid_of(target).and_then(crate::sched::pid_number_of) {
        Some(n) => n as isize,
        None => err::ESRCH,
    }
}

/// `getpgid(pid)` and `getpgrp()` — which group a task is in.
pub(super) fn handle_getpgid(args: [usize; ARG_COUNT]) -> isize {
    let pid = args[0] as isize;
    let target = match pid {
        0 => crate::sched::current(),
        n if n > 0 => match pid_task(n) {
            Some(id) => id,
            None => return err::ESRCH,
        },
        _ => return err::EINVAL,
    };

    match crate::sched::pgid_of(target).and_then(crate::sched::pid_number_of) {
        Some(n) => n as isize,
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
/// them to and nobody else reads them back. They are also the words a boot
/// actually sends: busybox `init` asks for `RB_DISABLE_CAD` a second into its
/// startup, so a machine that refuses the call prints a missing-call line on
/// every boot. The three stop words end the machine: QEMU's debug exit is the
/// closest thing to a reset this environment has, and `-no-reboot` turns it
/// into a clean shutdown either way. Anything else is refused, as Linux refuses
/// it.
///
/// Served on both machines. Its gate was the stale kind the entries around it
/// had: it was written when a stop word ended at the x86_64 `isa-debug-exit`
/// port, and that port is a `hal` capability now - a machine without
/// `mpos_qemu_debug_exit` halts, which is what `reboot` means on a machine with
/// no reset device. Nothing here reads a register frame, so there was never a
/// second body of code for the second machine to need, only a marker left
/// behind when the capability took the port's place.
pub(super) fn handle_reboot(args: [usize; ARG_COUNT]) -> isize {
    // `reboot(magic, magic2, cmd, arg)`: the magic words come first and the
    // command is the third argument. Reading `args[0]` as the command reads
    // `LINUX_REBOOT_MAGIC1` instead, classifies it as unknown and refuses -
    // which is a `poweroff -f` that prints "Invalid argument" and leaves the
    // machine up, and a test run that never reaches its end.
    let cmd = match abi::reboot_command(args) {
        // A command no kernel knows, whether or not it came with the magic.
        Ok(abi::RebootCmd::Unknown) => return err::EINVAL,
        Ok(cmd) => cmd,
        Err(e) => return e,
    };
    // Linux's gate, after the magic and before the command (`CAP_SYS_BOOT`):
    // a caller without it answers `EPERM` for every command, the CAD policy
    // included. Everything in this image that reboots is root and holds it.
    //
    // The namespace is the **initial** one for every caller this kernel can
    // have, which is the measured half of Linux's rule: `ns_capable` asks the
    // *pid* namespace's owner, and a caller with no pid namespace of its own -
    // which is every task here, and what the probe ran as - is refused by the
    // initial one (`.workbuddy/probe_capable_scope.sh`: a user-namespace root
    // gets `EPERM` for any command). The refinement is named in `docs/uapi/`
    // §5.2: a task in a pid namespace of its own asks *that* namespace's owner,
    // which this word cannot express. Nor is the order Linux's own, and that is
    // measured too: the same probe answers `EPERM` for a command whose magic is
    // invalid, so Linux asks the capability *before* the magic where this
    // kernel asks the magic first (also named in §5.2).
    if !crate::sched::current_credentials().capable(crate::cred::CAP_SYS_BOOT) {
        return err::EPERM;
    }
    if let abi::RebootCmd::Policy = cmd {
        return 0;
    }
    // The harness's test status, not an errno and not a machine: a run that
    // ends here reached its end, and the exit code is how the gate reads
    // that. Without a debug-exit device this halts.
    crate::println!("mpos: reboot(0x{:x}) - handing the machine back", args[2]);
    crate::hal::exit_qemu(crate::hal::TEST_EXIT_OK)
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
///
/// Served on both machines. Its gate was of the same stale kind as
/// `process_vm_*`'s: everything it touches - the pending set, the blocked
/// mask, the tick clock, the `siginfo_t` it fills in - is either the
/// scheduler's or the generic ABI's, and the second machine's number table has
/// named `RT_SIGTIMEDWAIT` (137 against x86_64's 128) all along. Nothing here
/// consults a register frame, which is what the ptrace and signal-*handler*
/// gates beside it are actually about.
pub(super) fn handle_rt_sigtimedwait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    /// The bytes of `siginfo_t`: 128 on both machines, because the structure
    /// Linux reports a queued signal in is the generic `kernel_siginfo` -
    /// `si_signo`/`si_errno`/`si_code` at 0/4/8, `si_pid`/`si_uid` at 16/20 and
    /// the `union sigval` at 24 - and not one architecture's spelling of it.
    const SIGINFO_BYTES: usize = 128;

    let (set_ptr, info, timeout, sigsetsize) = (args[0], args[1], args[2], args[3]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    // The set is not optional here, which is where this call parts company with
    // `rt_sigprocmask`: that one reads a null set as "just report the mask I
    // already have", while a wait has nothing to wait for without one. Linux
    // copies the set in, so a null pointer is `EFAULT` there; so it is here,
    // rather than being quietly read as an empty set.
    if set_ptr == 0 {
        return err::EFAULT;
    }
    if !buffer_ok(set_ptr, 8, true, from_user) {
        return err::EFAULT;
    }
    let wanted = unsafe { (set_ptr as *const u64).read_unaligned() };
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
        Some(crate::hal::ticks() + ms as u64 * crate::hal::TICK_HZ as u64 / 1_000)
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
                // word at 24 that its *class* selects: the `union sigval` for
                // every sender that names one, and `si_status` for a child's
                // death, which is the record a blocked `SIGCHLD` carries
                // (`signal::SigInfo::child`). The class test is the same one
                // the machines' frame writers use (`hal::SignalInfo::chld_union`),
                // because it is one rule about one struct.
                //
                // A `SI_TIMER` record's `si_tid`/`si_overrun` slots (16 and 20)
                // are left zero: no caller in this image reads them, and
                // `timer_getoverrun` is the ABI the count is asked through.
                let wire = crate::hal::SignalInfo {
                    signo: record.signo,
                    errno: record.errno,
                    code: record.code,
                    pid: record.pid,
                    uid: record.uid,
                    value: record.value,
                    status: record.status,
                    utime: record.utime,
                    stime: record.stime,
                };
                unsafe {
                    core::ptr::write_bytes(info as *mut u8, 0, SIGINFO_BYTES);
                    (info as *mut i32).write_unaligned(record.signo); // si_signo
                    ((info + 4) as *mut i32).write_unaligned(record.errno); // si_errno
                    ((info + 8) as *mut i32).write_unaligned(record.code); // si_code
                    ((info + 16) as *mut i32).write_unaligned(record.pid); // si_pid
                    ((info + 20) as *mut i32).write_unaligned(record.uid); // si_uid
                    if wire.chld_union() {
                        ((info + 24) as *mut i32).write_unaligned(record.status); // si_status
                        // The child's CPU, the rest of the `_sigchld` member:
                        // `si_utime`/`si_stime`, each an 8-byte `clock_t`, at
                        // Linux's offsets 32 and 40. `sigtimedwait` renders the
                        // same record a handler's frame does, so both must carry
                        // them - the record is the sender's, the door is not.
                        ((info + 32) as *mut u64).write_unaligned(record.utime); // si_utime
                        ((info + 40) as *mut u64).write_unaligned(record.stime); // si_stime
                    } else {
                        // The `union sigval`, at the offset the `si_value` of a
                        // queued signal lives in the ABI's layout - the same
                        // word `sigqueue`'s handler reads.
                        ((info + 24) as *mut u64).write_unaligned(record.value);
                    }
                }
            }
            return sig as isize;
        }
        if let Some(sig) = crate::sched::take_fatal_signal() {
            crate::sched::exit(-(sig as isize));
        }
        if let Some(end) = deadline
            && crate::hal::ticks() >= end
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
// One past the highest address a process may map: the *machine's* user span, not
// a shared figure. The two machines' values and the reason for them are written
// down in `mm::elf`, beside `MMAP_BASE`; this used to be a private `1 << 47`
// here, and on aarch64 that made every `MAP_FIXED` mapping above bit 47 - the
// mapping a dynamic loader makes for a shared library - answer `EINVAL`.
use crate::mm::elf::USER_LIMIT;

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
/// * Without `MAP_FIXED` the address is only a hint, and one that is still
///   *mapped* (a frame behind it) is simply not taken: the mapping goes
///   wherever there is room. Linux's actual rule for a plain hint is finer
///   than that, and now pinned here too: an unmapped hint whose range merely
///   overlaps an existing VMA - even a lazily-mapped hole inside it - is
///   refused with `EINVAL`, and only a range no VMA describes at all is
///   honoured exactly (`NOTES.md` §15.18). This matters because the same
///   overlap makes `brk(mm_brk)` answer `EINVAL` afterwards, which is how a
///   libc that wants a fixed-address region recovers: try the break, get the
///   refusal, map with `MAP_FIXED_NOREPLACE` instead.
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
            if crate::sched::vma_range_free(addr, len) {
                addr
            } else {
                return err::EEXIST;
            }
        }
        // Without `MAP_FIXED` the address is a hint, and Linux takes it
        // exactly only when no VMA describes any part of the range: a hint
        // over anything that is somebody's address is *declined and
        // relocated*, not refused. What the hint answers to is the
        // descriptions, not the frames - a lazily-mapped span whose pages
        // have never arrived is as much in the way as a populated one - and
        // that is the measurement `NOTES.md` §15.18 exists for: a frame-only
        // check took a busy hint's answer for the whole rule, while real
        // Linux relocates a hint inside a lazy `PROT_NONE` span exactly as it
        // relocates one over a live mapping (re-measured on the run machine:
        // both answer a fresh page elsewhere, neither answers `EINVAL`).
        (false, Some(addr)) if !crate::sched::vma_range_free(addr, len) => {
            match free_range(len, crate::sched::get_mmap_cursor()) {
                Some((base, new_cursor)) => {
                    crate::sched::set_mmap_cursor(new_cursor);
                    base
                }
                None => return err::ENOMEM,
            }
        }
        // A hint with nothing in the way is taken exactly: free of
        // descriptions, and therefore free of frames too.
        (false, Some(addr)) => addr,
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
    // *reachable from ring 3* travels with it, and the VMA's permission word
    // is the record of that: `handle_mmap` stores exactly the caller's prot
    // into it (`PROT_NONE` is the absence of `USER`), every fault populates
    // from the VMA's own flags, and `mprotect` rewrites both halves together.
    // Reading the live PTE as well would disagree with the VMA for a lazily
    // mapped page - no entry exists yet to read - and refusing such a page on
    // its absent bits was this loop's bug (`NOTES.md` §15.18).
    let mut frames = alloc::vec::Vec::new();
    for page in (old_addr..old_addr + old).step_by(PAGE_SIZE) {
        if !crate::sched::user_mapped(page) {
            return err::EFAULT;
        }
        let frame = crate::sched::user_frame_at(page);
        let reachable =
            crate::sched::vma_prot_at(page).is_some_and(|prot| prot.contains(MapFlags::USER));
        frames.push((frame, reachable));
    }

    if !to_fixed {
        // A shrink, or a grow into the pages just above.
        if new <= old {
            crate::sched::munmap_user(old_addr + new, old - new);
            return old_addr as isize;
        }
        // Linux grows only into pages its VMA says are free; the same
        // description check as the plain hint above. A grow over untouched
        // space inside an existing mapping is `ENOMEM` without MAYMOVE - what
        // musl's `pthread_getattr_np` reads as "still inside the mapping"
        // (`NOTES.md` §15.18).
        if crate::sched::vma_range_free(old_addr + old, new - old) {
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
    // The eagerly-mapped ranges travel with the pages: `take_vmas` returns none
    // for them (they carry no VMA), and a description left behind would hold
    // the old address and leave the new one placeable - the bug the occupancy
    // question was just taught to see.
    crate::sched::move_eager_ranges(old_addr, old, delta);

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

/// The first run of `len` bytes at or below `cursor` that no VMA describes,
/// page-aligned, with the cursor to store back for the next placement.
///
/// The search runs downward and the cursor is the *bottom* of the last mapping
/// placed: the next anonymous mapping goes just below the previous one, which
/// is the direction Linux's mmap region grows in. `cursor` is the per-process
/// mmap cursor; the returned value is the updated cursor to store back.
///
/// The question is asked of the *descriptions*, not the frames (§15.18): a
/// lazily-mapped hole has no pages yet and is still somebody's address, and a
/// placement into it would leave two VMAs over one range - the fault then
/// resolves against whichever description comes first, and a `PROT_NONE` one
/// wins an access its neighbour was granted. A frame-only walk did exactly
/// that, and every arena test whose fixture is a lazy `PROT_NONE` mapping read
/// the collision as a wrong address or a `SIGSEGV`.
fn free_range(len: usize, cursor: usize) -> Option<(usize, usize)> {
    use crate::mm::pframe::PAGE_SIZE;

    // A stale cursor from an old layout can only ever sit above the base; the
    // clamp keeps the first placement of every process at the top.
    let mut top = cursor.min(MMAP_BASE);
    loop {
        let floor = top.checked_sub(len)? & !(PAGE_SIZE - 1);
        // One window: everything from `floor` up to the *starting* cursor.
        // Asking only the current step's slice would clip a hole that begins
        // inside it and runs past its bottom edge - which is exactly the
        // shape §15.18's arena has: a lazy `PROT_NONE` span whose upper half
        // no description covers. The walk takes the highest run that fits.
        for (hole_start, hole_end) in crate::sched::vma_gaps(floor, cursor.saturating_sub(floor)) {
            let start = hole_start.next_multiple_of(PAGE_SIZE);
            if start.checked_add(len).is_some_and(|end| end <= hole_end) {
                return Some((start, start));
            }
        }
        top = floor;
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
    // The address itself is validated here, before anything is protected: a
    // caller that asked about a range starting mid-page gets `EINVAL` rather
    // than a silently rounded one. (musl's own `mprotect` *does* round, so a
    // program that wants the refusal has to ask through the raw door - which
    // is what `t-mprotect` does.)
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
    let hz = crate::hal::TICK_HZ as u64;
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
        let hz = crate::hal::TICK_HZ as u64;
        (value_ms * hz / 1000, interval_ms * hz / 1000)
    };
    let new = if value_ticks == 0 {
        // A zero it_value disarms, exactly as the man page words it.
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks,
            deadline: crate::hal::ticks() + value_ticks.max(1),
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
    let hz = crate::hal::TICK_HZ as u64;
    let new = if seconds == 0 {
        None
    } else {
        Some(crate::sched::ItimerState {
            interval_ticks: 0,
            deadline: crate::hal::ticks() + (seconds * hz).max(1),
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
            // The range this kernel can name: 1..=64 (`_NSIG`). musl's helper
            // waits on `SIGRTMIN` (32), and now any real-time number in the
            // range may be the timer's signal, with a disposition installable
            // for it too (stage B).
            if signo <= 0 || signo > crate::sched::signal::MAX_SIGNAL as i32 {
                return err::EINVAL;
            }
            if kind == SIGEV_THREAD_ID {
                // The thread the timer notifies, named in the caller's
                // namespace; a number that names nothing there is a request
                // this kernel cannot arm rather than a timer aimed at whichever
                // task wears that slot.
                let Some(thread) = pid_task(tid as isize) else {
                    return err::EINVAL;
                };
                (signo as u8, value, TimerNotify::Thread(thread))
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
    let hz = crate::hal::TICK_HZ as u64;
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
        Some(crate::hal::ticks() + value_ticks)
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
    let hz = crate::hal::TICK_HZ as u64;
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
    let hz = crate::hal::TICK_HZ as u64;
    let ticks = crate::hal::ticks();
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
    let elapsed = crate::hal::since_boot_nanos();
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
        crate::hal::boot_epoch()
    };
    // The caller's time namespace moves some of these clocks and none of the
    // others, which is the whole point of the kind and is measured rather than
    // guessed (§6.3, `.workbuddy/probe-timens5.c`/`7.c`): the three monotonic
    // clocks that are not the coarse *raw* hardware counter all read through the
    // monotonic offset, `BOOTTIME` through its own, and REALTIME, TAI and the
    // two cputime clocks are untouched - a certificate's validity must not move
    // because a namespace wanted an earlier uptime, and CPU time is not a clock
    // anybody re-bases.
    //
    // The offset is added to the *nanosecond* total rather than to each half, so
    // a negative offset cannot hand userland a negative `tv_nsec`: the
    // normalization below is `div_euclid`/`rem_euclid`, which is what keeps the
    // two fields in the shape a `timespec` promises. Zero for every task that
    // never entered a namespace, so an ordinary clock read is unchanged.
    let (monotonic_offset, boottime_offset) = crate::sched::time_offsets();
    let offset = match clk_id {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => monotonic_offset,
        CLOCK_BOOTTIME => boottime_offset,
        _ => 0,
    };
    let total = (wall as i64) * 1_000_000_000 + elapsed as i64 + offset;
    let secs = total.div_euclid(1_000_000_000);
    let nsecs = total.rem_euclid(1_000_000_000);
    unsafe {
        (tp as *mut i64).write_unaligned(secs);
        ((tp + 8) as *mut i64).write_unaligned(nsecs);
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
    let elapsed = crate::hal::since_boot_nanos();
    let secs = crate::hal::boot_epoch() + elapsed / 1_000_000_000;
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
    crate::sched::current_uts().set_nodename(name);
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
        write_str(65, &crate::sched::current_uts().nodename());
        // `uname -r` must answer the version the kernel was built with - it
        // is the same `CARGO_PKG_VERSION` `/proc/sys/kernel/osrelease`
        // reports, and the name of the `/lib/modules/<release>` directory
        // the image carries for modprobe, which xtask reads from the root
        // Cargo.toml (see `utils::kernel_release`). Three readers, one number.
        write_str(130, env!("CARGO_PKG_VERSION")); // release
        write_str(195, "#1 SMP PREEMPT mpos"); // version
        // `uname -m` must answer the machine the kernel is *built for*, not a
        // constant: a configure script or a `uname -m` test that gets
        // "x86_64" from an aarch64 kernel misdetects the host and picks the
        // wrong binaries. `cfg!` selects at compile time, so each machine says
        // its own true name and neither build pays a branch.
        write_str(
            260,
            if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "x86_64"
            },
        ); // machine
        // domainname: read from the caller's namespace, the same one
        // `setdomainname(2)` writes and `/proc/sys/kernel/domainname` reports.
        // It used to be the constant `(none)` here, which made the third door
        // disagree with the other two: a program that set a domain and then
        // asked `uname` was told the domain it had just set was `(none)`.
        write_str(325, &crate::sched::current_uts().domainname());

        // §10.5's private disk-counter protocol *overrides two fields of the
        // answer above*; it does not replace it. On a machine booted for the
        // measurement (`xtask test`, whose command line carries `mpos.diskstat`
        // exactly when the sweep stages `t-diskbench`), `release` answers the
        // magic word (`abi::DISKSTAT_OSRELEASE_MAGIC`) and `version` carries one
        // row per registered disk, `<name>:<reads>:<writes>,` - rendered *here*,
        // at read time, because the counters move while the machine runs. A
        // *private* ABI word, disclosed in `crates/abi` and the UAPI table: no
        // real kernel's osrelease is a number, so nothing that parses these
        // fields can collide with it, and there is no other door for these
        // counts - `/proc/partitions` is column-parsed by tools and this kernel
        // has no sysfs attribute store.
        //
        // The two fields, and only the two: an armed boot still answers Linux's
        // sysname, this machine's name and the caller's nodename and
        // domainname, because every other program in the sweep reads them. The
        // first draft returned from here *before* writing them, and three
        // programs measured the zeroed fields as failures (`t-uname`'s three
        // claims, `t-ns-uts`'s four door claims and `t-clone`'s NEWUTS read-back
        // - the sweep arms the protocol whenever `t-diskbench` is staged, which
        // is the default).
        if crate::sched::current_diskstat() {
            let (release, version) = abi::diskstat_uts_fields();
            write_str(130, &release);
            write_str(195, &version);
        }
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
    let hz = crate::hal::TICK_HZ as u128;
    let now_ticks = crate::hal::ticks() as u128;
    let now_ns = now_ticks * 1_000_000_000 / hz;
    let target_ns = sec as u128 * 1_000_000_000 + nsec as u128;
    let delta_ns = if relative {
        target_ns as i128
    } else if realtime {
        // The wall clock is the boot epoch plus the tick counter, which is
        // exactly what clock_gettime(CLOCK_REALTIME) reports.
        let wall_ns = crate::hal::boot_epoch() as u128 * 1_000_000_000 + now_ns;
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
    let secs = crate::hal::boot_epoch() + crate::hal::ticks() / crate::hal::TICK_HZ as u64;
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
///
/// The rule now lives in `crates/abi` (`clock_ticks_to_user_hz`), because the
/// scheduler's exit path asks the same question for a `SIGCHLD` record's
/// `si_utime`/`si_stime` and cannot name this module. This is the machine's
/// `TICK_HZ` supplied to that leaf, so the two report one unit - a second
/// multiply here is how a `times(2)` total and a child's record come to differ.
fn clock_ticks_to_user_hz(ticks: u64) -> u64 {
    crate::abi::clock_ticks_to_user_hz(ticks, crate::hal::TICK_HZ as u64)
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
    let elapsed = clock_ticks_to_user_hz(crate::hal::ticks());
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
    // `RUSAGE_*` are `int`s in the uapi, and Linux's generated syscall wrapper
    // reads the argument as one: the register's low 32 bits, sign-extended. So
    // `RUSAGE_CHILDREN` reaches a kernel as either `0x0000_0000_ffff_ffff` - what
    // a caller that wrote a 32-bit `-1` puts there, which is every compiled call
    // and every `syscall(2)` vararg - or `0xffff_ffff_ffff_ffff` for a caller
    // that sign-extended it itself. Comparing the whole 64-bit word accepts only
    // the second, so a libc's ordinary `getrusage(RUSAGE_CHILDREN, ..)` was
    // refused `EINVAL` here; the no-std suite spelled the sign-extended form and
    // so never saw it. Truncating to the `int` the ABI names accepts both, which
    // is what the migrated `t-times` pins.
    const RUSAGE_SELF: i32 = 0;
    const RUSAGE_CHILDREN: i32 = -1;
    let (who, usage) = (args[0] as u32 as i32, args[1]);
    if who != RUSAGE_SELF && who != RUSAGE_CHILDREN {
        return err::EINVAL;
    }
    // A null `usage` here is not "not interested" as it is on the wait doors -
    // `getrusage` has no other answer to give - so it goes to the writer like
    // any other pointer and comes back `EFAULT` from the check, which is what
    // Linux's own `copy_to_user` answers.
    //
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
    write_rusage(usage, utime, stime, true).unwrap_or_default()
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
        let hz = crate::hal::TICK_HZ as u64;
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
/// The modern wait: instead of a pid and a packed status word, `WEXITED` and
/// friends say which transitions count and the child arrives as an info record
/// - `si_pid`, `si_status`, and a `si_code` saying whether it exited, was
/// killed, stopped or resumed. `WNOHANG` answers zero with `si_pid` cleared
/// when no child is ready, which is how a caller polls without blocking.
///
/// # Which child, in Linux's four idtypes
///
/// `idtype` and `id` name the subject, and the three selectors Linux serves are
/// one vocabulary here (`sched::WaitFor`), shared with `wait4`'s named child:
/// `P_ALL` is any child, `P_PID` is that child's pid *in the caller's
/// namespace* (`resolve_pid` - the same resolution `kill` uses), and `P_PGID`
/// is the group `id` names, with `0` meaning the caller's own (`current_pgid`).
/// The fourth, `P_PIDFD`, is answered `EINVAL` and named as a gap rather than
/// guessed at: this kernel has no pidfd family to resolve a descriptor through
/// (`docs/uapi/` §5.3), so a descriptor here names no task at all.
///
/// # The options word, and the two refusals it carries
///
/// Linux validates the whole word and the requested *event classes* before it
/// looks at any child: an unknown bit and a call that names none of
/// `WEXITED`/`WSTOPPED`/`WCONTINUED` are both `EINVAL` (measured on a current
/// host, both with the record cleared), where this handler used to answer 0 to
/// the second - a success for a wait nothing could satisfy, and the one shape a
/// caller cannot tell from "no event yet". The record is cleared *before* the
/// refusal, in Linux's order, so a caller that seeded one cannot read its own
/// bytes back as an answer.
///
/// The three Linux-only bits (`__WNOTHREAD`, `__WALL`, `__WCLONE`) are accepted
/// and ignored, which is `wait4`'s treatment and a named divergence: `__WCLONE`
/// *narrows* to children cloned with an exit signal other than `SIGCHLD`, and
/// this kernel's tasks carry no exit signal to ask (`docs/uapi/` §5.2).
///
/// # The fifth argument
///
/// `rusage` is filled like `wait4`'s for the same events and the same way (see
/// [`write_wait_rusage`]) - with one ordering difference, and it is Linux's own:
/// the `struct rusage` is copied **before** the info record, so an unwritable
/// `rusage` pointer is `EFAULT` with the record left as the caller wrote it,
/// where `wait4` has by then already written `*status` (measured,
/// `.workbuddy/probe_wait4_rusage.c` scenarios N and O, Linux 7.2.9).
pub(super) fn handle_waitid(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    use super::abi::{CLD_CONTINUED, CLD_STOPPED, CLD_TRAPPED, SIGINFO_BYTES};
    use crate::sched::{WaitFor, WaitPark, signal};

    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const P_PGID: usize = 2;
    const WNOHANG: usize = 0x1;
    const WSTOPPED: usize = 0x2;
    const WEXITED: usize = 0x4;
    const WCONTINUED: usize = 0x8;
    const WNOWAIT: usize = 0x0100_0000;
    const __WNOTHREAD: usize = 0x2000_0000;
    const __WALL: usize = 0x4000_0000;
    const __WCLONE: usize = 0x8000_0000;
    /// Every bit Linux's own `waitid` accepts; anything else is an unknown
    /// request rather than a request to ignore.
    const KNOWN: usize =
        WNOHANG | WSTOPPED | WEXITED | WCONTINUED | WNOWAIT | __WNOTHREAD | __WALL | __WCLONE;
    /// The three classes of event a caller can ask to hear about. A call that
    /// asks for none of them is refused rather than answered.
    const CLASSES: usize = WEXITED | WSTOPPED | WCONTINUED;

    let (idtype, id, infop, options) = (args[0], args[1] as isize, args[2], args[3]);
    let rusage = args[4];
    if !buffer_ok(infop, SIGINFO_BYTES, true, from_user) {
        return err::EFAULT;
    }
    // Cleared first, refused second - Linux's order, measured: both of the
    // refusals below leave a zeroed record behind rather than the caller's own
    // seeded bytes.
    unsafe {
        core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
    }
    if options & !KNOWN != 0 || options & CLASSES == 0 {
        return err::EINVAL;
    }
    let me = crate::sched::current();
    let sel = match idtype {
        P_ALL => WaitFor::Any,
        P_PID => match pid_task(id) {
            Some(id) => WaitFor::Pid(id),
            None => return err::ECHILD,
        },
        // A group is named by its leader's number, resolved the way
        // `setpgid`/`getpgid` resolve one; `0` is the caller's own group, and
        // a negative id is not a group at all (measured: `P_PGID, -1` is
        // `EINVAL`). A group whose leader is gone has no number to resolve
        // here, so it reads as none of this caller's children.
        P_PGID => {
            if id < 0 {
                return err::EINVAL;
            }
            let group = if id == 0 {
                crate::sched::current_pgid()
            } else {
                match pid_task(id) {
                    Some(leader) => leader,
                    None => return err::ECHILD,
                }
            };
            WaitFor::Pgid(group)
        }
        // `P_PIDFD` (3), and anything else: no pidfd family exists here, so no
        // descriptor can name a task, and an idtype outside the four Linux
        // defines is a request this kernel cannot read (`docs/uapi/` §5.3).
        _ => return err::EINVAL,
    };

    let stops = options & WSTOPPED != 0;
    let continues = options & WCONTINUED != 0;
    let no_hang = options & WNOHANG != 0;
    // `WNOWAIT` reports without collecting: the event stays where the next
    // wait will find it, which is what a caller that wants to look before it
    // reaps asked for.
    let nowait = options & WNOWAIT != 0;

    let collected = if no_hang {
        crate::sched::poll_child_event(me, sel, stops, continues, nowait)
    } else if crate::sched::has_matching_children(me, sel) {
        match crate::sched::wait_child_event(me, sel, stops, continues, nowait) {
            WaitPark::Event(event) => Some(event),
            WaitPark::Interrupted => return err::EINTR,
            WaitPark::NoChild => None,
        }
    } else {
        None
    };

    let Some(event) = collected else {
        // Nothing to report, and the two "nothing"s are different answers:
        // `WNOHANG` on a child that has not done anything yet is the zero the
        // caller polls for (with the record above left cleared), while a
        // selector that matches no child at all is `ECHILD` - the wait could
        // never be answered. `wait4` draws the line in the same place.
        if no_hang && crate::sched::has_matching_children(me, sel) {
            return 0;
        }
        return err::ECHILD;
    };

    // The siginfo layout, and the `CLD_*` code each event class writes, are
    // the ABI the libc's `W` macros and `waitid` wrappers read; it is spelled
    // in `abi`, tested on the host.
    //
    // `si_uid` comes with the event, and the two paths have it differently: a
    // death outlives its task slot, so the record it was collected from carries
    // the uid, while a stop or a resume names a child that is still in the
    // table and is asked through its identity. Linux fills it for all three
    // (measured: `CLD_EXITED`, `CLD_STOPPED`, `CLD_TRACED` and `CLD_CONTINUED`
    // all report the child's real uid) and reports `cred->uid` - the real id,
    // not the effective one.
    let (child, si_code, status, uid, cpu) = match event {
        crate::sched::WaitEvent::Exited {
            id,
            code,
            uid,
            dumped,
            utime,
            stime,
        } => {
            let (si_code, status) = abi::exit_chld(code, dumped);
            (id, si_code, status, uid, (utime, stime))
        }
        // A stop under a tracer and a stop for the parent are two codes
        // (measured: `CLD_TRAPPED` and `CLD_STOPPED`), and the event's own word
        // cannot tell them apart - a ptrace signal-delivery stop carries
        // `PTRACE_EVENT_NONE`, exactly as a group stop does.
        crate::sched::WaitEvent::Stopped {
            id,
            signal,
            traced,
            utime,
            stime,
            ..
        } => (
            id,
            if traced { CLD_TRAPPED } else { CLD_STOPPED },
            signal as i32,
            live_uid(id),
            (utime, stime),
        ),
        crate::sched::WaitEvent::Continued { id, utime, stime } => (
            id,
            CLD_CONTINUED,
            signal::SIGCONT as i32,
            live_uid(id),
            (utime, stime),
        ),
    };
    // The rusage goes first, which is Linux's order and not this handler's:
    // its wrapper copies the struct and only then writes the record, so a caller
    // whose `rusage` pointer cannot be written is answered `EFAULT` before the
    // record is touched. The record has, however, been *cleared* above - mpos
    // clears up front to get the refusal paths right - so the bytes that survive
    // are zeros rather than the caller's own, the one visible difference this
    // ordering leaves and the reason the clear is not simply moved here.
    if let Some(e) = write_wait_rusage(rusage, cpu, from_user) {
        return e;
    }
    // The child's number in the *caller's* namespace, the same answer `wait4`
    // returns: a reader one level out sees the number it can use, and a reader
    // inside the child's own namespace sees 1.
    abi::write_waitid_info(infop, wait_answer(child) as u32, uid, si_code, status)
}

/// The real uid of a child that is still in the task table, for the two event
/// classes that are reported while it is alive. Zero when there is nothing left
/// to ask - a task that exited and was reaped between the event and this read
/// - which is the same zero a cleared record carries.
fn live_uid(id: crate::sched::TaskId) -> u32 {
    crate::sched::identity_of(id)
        .map(|identity| identity.ruid)
        .unwrap_or(0)
}

// The legacy `getrlimit`/`setrlimit` pair, `prlimit64` and `membarrier`
// live in `abi` with the rest of the table-answer calls.
