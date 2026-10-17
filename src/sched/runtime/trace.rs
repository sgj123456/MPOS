use super::WaitEvent;
use super::child_signal::queue_signal;
use super::task::schedule;
use super::task::with_table;
use super::task::{Stop, SyscallStop, Table, Task, WaitFor, current};
use crate::abi::{
    PTRACE_EVENT_CLONE, PTRACE_EVENT_EXEC, PTRACE_EVENT_FORK, PTRACE_EVENT_NONE,
    PTRACE_EVENT_SECCOMP, PTRACE_EVENT_VFORK, PTRACE_O_TRACECLONE, PTRACE_O_TRACEEXEC,
    PTRACE_O_TRACEFORK, PTRACE_O_TRACESECCOMP, PTRACE_O_TRACESYSGOOD, PTRACE_O_TRACEVFORK,
    SYSCALL_TRAP,
};
use crate::hal;
use crate::hal::{ABI_REG_COUNT, AbiRegs, ContextOps, UserFrame, UserFrameOps};
use crate::sched::policy::TaskId;
use crate::sched::signal;

pub fn has_tracer(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.tracer.is_some())
    })
}

/// Whether `id` is stopped: present, alive, and sitting in a stop.
pub fn is_stopped(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.stop.is_some())
    })
}

/// Bind `id` to `tracer`. Returns false when the task is gone or has exited,
/// which the syscall layer reports as `ESRCH`.
pub fn set_tracer(id: TaskId, tracer: TaskId) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.tracer = Some(tracer);
            true
        }
        _ => false,
    })
}

/// The task tracing `id`, if any.
pub fn tracer_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref()?.trace.tracer)
}

/// The `PTRACE_O_*` word `id`'s tracer set.
pub fn trace_options(id: TaskId) -> usize {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .map_or(0, |task| task.trace.options)
    })
}

/// Record the options a tracer asked for. False when the task is gone.
pub fn set_trace_options(id: TaskId, options: usize) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.options = options;
            true
        }
        _ => false,
    })
}

/// The register file `id` will resume with, if it is stopped.
///
/// A task that has never been out of the kernel has no snapshot to hand back:
/// a snapshot is what [`park`] takes, and nothing has parked it yet. What it has
/// instead is `user_resume` - the state the one-way door into ring 3 will read -
/// and that *is* its register file, not a description of one. The two cases are
/// told apart by `regs` being empty, which for a stopped task happens exactly
/// when nothing has parked it; [`attach_new_child`] is what creates such a task.
pub fn trace_regs(id: TaskId) -> Option<AbiRegs> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        task.trace.regs.or_else(|| {
            // Only a *stopped* task that never ran: a running task's registers
            // are nobody's to report, which is why the stop is part of the
            // question rather than implied by it.
            let stopped = task.trace.stop.and(task.user_resume.as_ref())?;
            Some(context_regs(stopped, task.tls))
        })
    })
}

/// The register file a task that has never been out of the kernel would resume
/// with, in the order `PTRACE_GETREGS` reports it.
///
/// This is where the machine's frame meets the ABI's `user_regs_struct`, and
/// the machine side does the translation ([`UserFrameOps::to_user_regs`]): which
/// ABI field a register goes in is exactly what differs between one machine's
/// frame and another's, and the three slots this kernel has nothing for - no
/// syscall is in flight, so `orig_rax` is nothing; and `rcx`/`r11` are the two
/// the `syscall` instruction itself clobbers - are left zero there, the value
/// the ABI reads for a register nobody can be relying on.
fn context_regs(state: &UserFrame, tls: usize) -> AbiRegs {
    let mut regs = [0u64; ABI_REG_COUNT];
    state.to_user_regs(tls, &mut regs);
    regs
}

/// The other direction: turn a register file a tracer may have rewritten back
/// into the state the one-way door into ring 3 reads.
fn context_from_regs(regs: &AbiRegs) -> UserFrame {
    UserFrame::from_user_regs(regs)
}

/// Replace the register file `id` will resume with. False when the task is
/// gone, or is not stopped - a running task's registers are not the kernel's
/// to rewrite from another task, and `PTRACE_SETREGS` on a running tracee is
/// `ESRCH` on Linux too.
pub fn set_trace_regs(id: TaskId, regs: &AbiRegs) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited && task.trace.stop.is_some() => {
            match (task.trace.regs, task.user_resume.as_mut()) {
                // A task stopped before its first instruction has no snapshot
                // to replace: what a tracer edits here *is* the state the door
                // will read, so the edit has to land there or nowhere. The rule
                // is the same one `trace_regs` reads by, seen from the writing
                // side.
                (None, Some(state)) => *state = context_from_regs(regs),
                _ => task.trace.regs = Some(*regs),
            }
            true
        }
        _ => false,
    })
}

/// The floating-point registers `id` is stopped with, as the 512-byte image
/// `PTRACE_GETFPREGS` and `PTRACE_SETFPREGS` move.
///
/// A stopped task's saved block is current: every switch out of a task writes
/// its live registers there, and a task that is stopped has been switched out at
/// least once since it last ran.
pub fn trace_fpu(id: TaskId) -> Option<[u8; hal::FPU_BYTES]> {
    with_table(|t| {
        // Only the lazy scheme has a saved block that can be stale; on a machine
        // that swaps the FPU eagerly the switch has already written it, so there
        // is nothing to make current and no `sync_fpu` to call.
        #[cfg(mpos_fpu_trap)]
        t.sync_fpu(id);
        t.tasks
            .get(id.0)?
            .as_ref()
            .map(|task| task.context.fpu().to_bytes())
    })
}

/// Replace the floating-point registers `id` will resume with. The same rule as
/// [`set_trace_regs`]: only a stopped task's state is the kernel's to rewrite.
pub fn set_trace_fpu(id: TaskId, bytes: &[u8; hal::FPU_BYTES]) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited && task.trace.stop.is_some() => {
            *task.context.fpu_mut() = hal::Fpu::from_bytes(bytes);
            true
        }
        _ => false,
    })
}

/// What a `PTRACE_GET_SYSCALL_INFO` reports about the syscall `id` is stopped
/// at, as `(entry, nr, args, ip, sp)`.
pub fn trace_syscall(id: TaskId) -> Option<(bool, u64, [u64; 6], u64, u64)> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        let stop = task.trace.stop?;
        // A stop at a syscall boundary describes itself. Any *other* stop - a
        // signal arriving, an exec - is described by the call the tracee was in
        // the middle of, which is the one it last entered: Linux reads that
        // answer out of the registers, and the entry record is the same answer
        // kept somewhere a stopped task can still be asked.
        let entry_stop = stop.syscall.map(|syscall| syscall.entry);
        let syscall = stop.syscall.or(task.trace.syscall)?;
        Some((
            entry_stop.unwrap_or(true),
            syscall.nr,
            syscall.args,
            syscall.ip,
            syscall.sp,
        ))
    })
}

/// The stop `id` is sitting in, without collecting it: what a `wait` looks at
/// while it decides whether there is anything to report.
pub fn stop_of(id: TaskId) -> Option<Stop> {
    with_table(|t| t.tasks.get(id.0)?.as_ref()?.trace.stop)
}

/// Collect `id`'s stop: mark it reported and hand it back. This is the moment
/// the news stops being news, and it is deliberately separate from looking.
pub fn take_stop_report(id: TaskId) -> Option<Stop> {
    with_table(|t| {
        let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
        let stop = task.trace.stop.as_mut()?;
        if stop.reported {
            return None;
        }
        stop.reported = true;
        Some(*stop)
    })
}

/// Take `id`'s `WIFCONTINUED` flag, if a `SIGCONT` set one and no `wait` has
/// reported it.
pub fn take_continued_report(id: TaskId) -> bool {
    with_table(|t| {
        let Some(task) = t.tasks.get_mut(id.0).and_then(Option::as_mut) else {
            return false;
        };
        let flag = task.trace.continued;
        task.trace.continued = false;
        flag
    })
}

/// Whether a `SIGCONT` has ended a stop of `id` and nobody has been told.
pub fn continued_report_pending(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.continued)
    })
}

/// The oldest unreported stop among `parent`'s children **that `sel` names**,
/// if one is reportable.
///
/// `requested` is `WUNTRACED`, and it is not the whole condition: a traced
/// child's stop is reported to its *tracer* whether or not the tracer asked for
/// stops, because the tracer is the only thing that can end it and a tracer
/// that could not hear about one would deadlock on its first `wait`. That is
/// what `stop.traced` says, and it is why `strace`'s very first `waitpid` - with
/// no option bits at all - still returns.
pub(crate) fn unreported_stop(
    t: &Table,
    parent: TaskId,
    sel: WaitFor,
    requested: bool,
) -> Option<WaitEvent> {
    for (slot, task) in t.tasks.iter().enumerate() {
        let Some(task) = task.as_ref() else { continue };
        let id = TaskId(slot);
        if !waitable(task, parent) || id == parent || !sel.names_task(id, task) {
            continue;
        }
        let Some(stop) = task.trace.stop else {
            continue;
        };
        if stop.reported {
            continue;
        }
        // Who may hear this stop. A ptrace stop belongs to the *tracer* - the
        // real parent waiting without `WUNTRACED` must never see one, and not
        // with `WUNTRACED` either: Linux keeps ptrace-stops out of the real
        // parent's `wait` entirely, because the parent learning "child
        // stopped" while the tracer owns the corpse leaves the parent acting
        // on a state it can do nothing about. `strace -f timeout ...` hung a
        // whole class of runners exactly here, when a born-stopped fork child
        // was reported to a parent that was only collecting exits. A plain
        // group stop (a `SIGSTOP` of job control) is the opposite: it is the
        // parent's news, and only with `WUNTRACED`.
        if stop.traced {
            if task.trace.tracer != Some(parent) {
                continue;
            }
        } else if !requested {
            continue;
        }
        // The scan is ascending, so the first match is the lowest id: two
        // children stopping in the same tick are reported in a fixed order
        // rather than whichever order the table happened to hand out.
        let (utime, stime) = task.mm.cpu_both();
        return Some(WaitEvent::Stopped {
            id,
            signal: stop.signal,
            event: stop.event,
            traced: stop.traced,
            utime,
            stime,
        });
    }
    None
}

/// Whether `id` is one `me` may `wait` for.
///
/// Two things make a task waitable, and Linux treats them the same way: being
/// `me`'s child, and being traced by `me`. The second is not a convenience -
/// `PTRACE_ATTACH` makes a tracer responsible for a task it did not spawn, and
/// the only way that task's stops and death can be collected is by waiting for
/// them. Having one rule means the six places that scan for "my children" do
/// not each grow a second predicate, and cannot disagree with each other.
pub(crate) fn waitable(task: &Task, me: TaskId) -> bool {
    task.parent == Some(me) || task.trace.tracer == Some(me)
}

/// Whether `id` is a child of `parent` - the check that keeps a `wait` from
/// blocking on someone else's child.
pub fn is_child(id: TaskId, parent: TaskId) -> bool {
    with_table(|t| {
        t.tasks[id.0]
            .as_ref()
            .is_some_and(|task| !task.exited && waitable(task, parent))
    })
}

/// The lowest unblocked, non-ignored pending signal of `task` - the set
/// [`take_deliverable_signal`] draws from, without taking anything.
///
/// This is what lets the delivery path decide whether there is a signal to
/// report to a tracer before it commits to consuming one.
fn deliverable(task: &Task) -> Option<u8> {
    let mut ready = task.signals & !task.blocked;
    while let Some(sig) = signal::take(&mut ready) {
        if task.dispositions.get(sig).is_ignore() {
            continue;
        }
        return Some(sig);
    }
    None
}

/// Park the running task in `stop` and return only when something resumes it.
///
/// The register file goes in and comes back out: it is stored where
/// `PTRACE_GETREGS` can read it and `PTRACE_SETREGS` can change it, and the
/// caller gets whatever the tracer left there. That is the whole mechanism
/// behind "stop the program, change its registers, let it go".
///
/// Both the tracer and the parent are woken rather than one being chosen
/// between. For a tracer's stop only the tracer can do anything about it, and
/// for a group stop only the parent - but the two are one test apart, and a
/// wake delivered to a task that is not waiting is a no-op.
pub(crate) fn park(regs: &mut AbiRegs, stop: Stop) {
    let id = current();
    let snapshot = *regs;
    schedule(|t| {
        let (tracer, parent) = {
            let task = t.task_mut(id);
            task.trace.stop = Some(stop);
            task.trace.regs = Some(snapshot);
            (task.trace.tracer, task.parent)
        };
        if let Some(tracer) = tracer {
            t.sched.wake(tracer);
        }
        if let Some(parent) = parent {
            t.sched.wake(parent);
        }
        let priority = t.task(id).priority;
        // `u64::MAX` is "until someone wakes me". A stop has no clock on it:
        // a tracee that gave up waiting would break the protocol, and a group
        // stop is ended by `SIGCONT` and nothing else.
        t.sched.sleep(id, priority, u64::MAX);
    });
    *regs = with_table(|t| t.task(id).trace.regs).unwrap_or(snapshot);
}

/// The stop a successful `execve` leaves a traced task in.
///
/// There are two shapes, and which one applies is the tracer's own choice via
/// `PTRACE_O_TRACEEXEC`:
///
/// * With the option set, the call is interrupted mid-flight by an event stop
///   carrying `PTRACE_EVENT_EXEC`, and *the call's exit stop still follows it*.
///   That ordering is Linux's, and it is worth spelling out because it is
///   counter-intuitive - the ptrace(2) man page walks through it:
///   `we get PTRACE_EVENT_EXEC ... we issue PTRACE_SYSCALL ... we get
///   syscall-exit-stop ... PID0 <... execve resumed> ) = 0`. A tracer that
///   treats the event as the call's completion finds its enter/exit bookkeeping
///   one stop ahead from then on, and every later return value read off the
///   following call - the shape `strace`'s output had before this was right.
/// * Without it, the trap is the completion notice: the exit hop that follows
///   has nothing left to say, which `at_exec_exit` records for
///   [`stop_at_syscall`] to skip. The trap is a plain `SIGTRAP` - the classic
///   exec trap every legacy tracer is written against.
///
/// In both shapes the syscall record is left as the *entry* one: the record is
/// only ever written at syscall stops, so at this stop it still describes the
/// call in flight, which is what `PTRACE_GET_SYSCALL_INFO` should say here too.
pub fn exec_trap(regs: &mut AbiRegs) {
    let id = current();
    let options = with_table(|t| t.task(id).trace.options);
    if options & PTRACE_O_TRACEEXEC != 0 {
        let entry = with_table(|t| t.task(id).trace.syscall);
        park(
            regs,
            Stop {
                signal: signal::SIGTRAP,
                event: PTRACE_EVENT_EXEC,
                traced: true,
                reported: false,
                // The former tid: an exec does not change the task's number
                // here, so it is the same one - but it is the *question* a
                // tracer asks with `PTRACE_GETEVENTMSG` after an exec, and the
                // answer has to come from the stop rather than from whoever
                // happens to be reading it.
                message: id.0 as u64,
                delivering: None,
                syscall: entry,
                group: false,
            },
        );
        return;
    }
    with_table(|t| t.task_mut(id).trace.at_exec_exit = true);
    let entry = with_table(|t| t.task(id).trace.syscall);
    park(
        regs,
        Stop {
            signal: signal::SIGTRAP,
            event: PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: None,
            syscall: entry.map(|call| SyscallStop {
                entry: false,
                ..call
            }),
            group: false,
        },
    );
}

/// Which `PTRACE_EVENT_*` a child's creation is, read off the `clone` flags
/// that asked for it.
///
/// The three-way split is Linux's, and it is not a detail a tracer can recover
/// on its own: `fork`, `vfork` and a thread-creating `clone` all arrive at the
/// kernel as one syscall with different flags, and a tracer that asked about
/// threads but not about processes needs to be told which one it just got.
pub(crate) fn fork_event(flags: usize) -> u32 {
    if flags & crate::abi::CLONE_THREAD != 0 {
        PTRACE_EVENT_CLONE
    } else if flags & crate::abi::CLONE_VFORK != 0 {
        PTRACE_EVENT_VFORK
    } else {
        PTRACE_EVENT_FORK
    }
}

/// Hand a brand-new child to its creator's tracer, if the tracer asked about
/// this kind of child. True when it did.
///
/// Two things have to happen together, and Linux does both inside its own fork
/// path. The child is **attached to the same tracer and born stopped**, so that
/// nothing it does can happen before the tracer has looked at it; and the
/// creator is left **owing its tracer an event stop**, which the syscall layer
/// raises a few lines later, where the frame is.
///
/// Born stopped is what makes `strace -f` work at all. A thread that runs even
/// once before its tracer's first look has already made a syscall the trace
/// will not contain, and a tracer left to guess where the thread was is worse
/// off than one that is simply told.
pub(crate) fn attach_new_child(t: &mut Table, parent: TaskId, child: TaskId, event: u32) -> bool {
    let bit = match event {
        PTRACE_EVENT_FORK => PTRACE_O_TRACEFORK,
        PTRACE_EVENT_VFORK => PTRACE_O_TRACEVFORK,
        PTRACE_EVENT_CLONE => PTRACE_O_TRACECLONE,
        _ => return false,
    };
    let (tracer, options) = {
        let parent = t.task(parent);
        (parent.trace.tracer, parent.trace.options)
    };
    // No tracer, or one that did not ask about this kind of child: the child is
    // nobody's business but its parent's, and it starts running.
    if tracer.is_none() || options & bit == 0 {
        return false;
    }

    let child_task = t.task_mut(child);
    child_task.trace.tracer = tracer;
    // The options come with the child. Linux copies them, and it matters: a
    // tracer sets its options once, on the process it started, and every thread
    // that process creates afterwards is traced with those same options without
    // the tracer saying so again.
    child_task.trace.options = options;
    child_task.trace.stop = Some(Stop {
        // The `SIGSTOP` of being newly attached - the same stop `PTRACE_ATTACH`
        // gives, and the one a tracer is written to swallow. Linux reports
        // `PTRACE_EVENT_STOP` instead when the tracer attached with
        // `PTRACE_SEIZE`; this kernel has no `SEIZE`, so it is the older shape.
        signal: signal::SIGSTOP,
        event: PTRACE_EVENT_NONE,
        traced: true,
        reported: false,
        message: 0,
        // Nothing is pending delivery: the child was not sent a signal, it was
        // born here. A tracer that resumes it *with* `SIGSTOP` gets one
        // delivered, which is what Linux does with that too.
        delivering: None,
        syscall: None,
        // A stop signal's stop, so a `SIGCONT` can end it.
        group: true,
    });
    // It has no saved register file, because it has never been out of the
    // kernel: the registers it will resume with are the ones it was built with,
    // and `trace_regs` reads those out of `user_resume`.
    child_task.trace.regs = None;

    t.task_mut(parent).trace.pending_event = Some((event, child.0 as u64));
    true
}

/// The event stop a task owes its tracer for a child it has just made, taken
/// once. See `Tracing::pending_event`.
pub fn take_pending_event(id: TaskId) -> Option<(u32, u64)> {
    with_table(|t| {
        let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
        task.trace.pending_event.take()
    })
}

/// Hold the running task where it is so its tracer can be told that it made a
/// child, and which child.
///
/// Called from inside the `fork`/`clone`/`vfork` call rather than after it, for
/// the same reason [`exec_trap`] is called inside `execve`: the stop describes
/// a moment in the middle of a syscall, and the call's own exit stop still
/// follows it. `message` is the child's pid, which is what
/// `PTRACE_GETEVENTMSG` answers at this stop.
pub fn fork_trap(regs: &mut AbiRegs, event: u32, message: u64) {
    // The syscall record is deliberately left alone: it still describes the
    // call in flight, and a tracer that asks what this stop is a boundary of
    // should hear "the clone that has not returned yet".
    let entry = with_table(|t| t.task(current()).trace.syscall);
    park(
        regs,
        Stop {
            signal: signal::SIGTRAP,
            event,
            traced: true,
            reported: false,
            message,
            delivering: None,
            syscall: entry,
            group: false,
        },
    );
}

/// Whether the running task's tracer asked for the seccomp event.
///
/// A `SECCOMP_RET_TRACE` decision needs somewhere for the call to go: with no
/// tracer, or a tracer that did not name `PTRACE_O_TRACESECCOMP`, there is
/// nobody to hand it to, and Linux answers `ENOSYS` (measured on 7.2). This is
/// the question the syscall gate asks before deciding between the stop and the
/// refusal.
pub fn seccomp_trace_armed() -> bool {
    let id = current();
    with_table(|t| {
        let trace = &t.task(id).trace;
        trace.tracer.is_some() && trace.options & PTRACE_O_TRACESECCOMP != 0
    })
}

/// Hold the running task so its tracer can be told that a `SECCOMP_RET_TRACE`
/// filter selected this call, before the call is made.
///
/// `message` is the filter answer's data half, which is what
/// `PTRACE_GETEVENTMSG` answers at this stop (measured: a `RET_TRACE|0x1234` on
/// `gettid` reported `0x1234`). The syscall record is left alone for the same
/// reason [`fork_trap`] leaves it: the call has not run yet, and the tracer's
/// `PTRACE_GET_SYSCALL_INFO` should describe it.
pub fn seccomp_trap(regs: &mut AbiRegs, message: u64) {
    let entry = with_table(|t| t.task(current()).trace.syscall);
    park(
        regs,
        Stop {
            signal: signal::SIGTRAP,
            event: PTRACE_EVENT_SECCOMP,
            traced: true,
            reported: false,
            message,
            delivering: None,
            syscall: entry,
            group: false,
        },
    );
}

/// Stop the running task at a syscall boundary, if its tracer asked for one.
///
/// Called twice per syscall - once before the handler runs and once after -
/// because that is the pair of moments a tracer wants to see. The `armed` flag
/// is cleared here rather than at the resume: it is the stop that consumes the
/// tracer's request, and the tracer re-arms it, which is what makes an entry
/// stop and an exit stop two events rather than one repeated.
pub fn stop_at_syscall(
    entry: bool,
    regs: &mut AbiRegs,
    nr: u64,
    args: &[u64; 6],
    ip: u64,
    sp: u64,
) {
    let id = current();
    let (armed, options, exec_exit) = with_table(|t| {
        let trace = &t.task(id).trace;
        (
            trace.armed && trace.tracer.is_some(),
            trace.options,
            trace.at_exec_exit,
        )
    });
    if !armed {
        return;
    }
    if !entry && exec_exit {
        // Legacy tracer (no `PTRACE_O_TRACEEXEC`): the exec trap *was* the
        // call's completion notice. See `exec_trap`. With the option set this
        // flag is never raised, and the exit stop follows the event stop the
        // way ptrace(2) describes.
        with_table(|t| t.task_mut(id).trace.at_exec_exit = false);
        return;
    }
    let stop_syscall = with_table(|t| {
        let task = t.task_mut(id);
        // The entry's arguments are remembered across the pair of stops, so the
        // exit half can describe the call as well as the entry half can.
        if entry {
            task.trace.at_exec_exit = false;
            task.trace.syscall = Some(SyscallStop {
                nr,
                args: *args,
                ip,
                sp,
                entry: true,
            });
            task.trace.syscall
        } else {
            let previous = task.trace.syscall.filter(|syscall| syscall.entry);
            task.trace.syscall = Some(SyscallStop {
                nr: previous.map_or(nr, |syscall| syscall.nr),
                args: previous.map_or(*args, |syscall| syscall.args),
                ip,
                sp,
                entry: false,
            });
            task.trace.syscall
        }
    });
    // `TRACESYSGOOD` is the difference between a stop a tracer can recognise
    // and one it has to guess at, so it is applied where the signal is chosen
    // rather than stored anywhere.
    let signal = signal::SIGTRAP
        | if options & PTRACE_O_TRACESYSGOOD != 0 {
            SYSCALL_TRAP
        } else {
            0
        };
    park(
        regs,
        Stop {
            signal,
            event: PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: None,
            syscall: stop_syscall,
            group: false,
        },
    );
}

/// Report the running task's pending signal to its tracer before it is
/// delivered, and hold the task there until the tracer answers.
///
/// The signal stays pending across the stop. The tracer's resume replaces it -
/// with the signal it names, or with nothing at all - and what is left behind is
/// what the delivery path a few lines further on finds. A tracer that names
/// nothing therefore drops the signal, which is the point: that is how a
/// debugger "swallows" a signal, and how `strace` reports one without letting it
/// through twice.
pub fn trace_signal_stop(regs: &mut AbiRegs) {
    let id = current();
    let traced = with_table(|t| t.task(id).trace.tracer.is_some());
    if !traced {
        return;
    }
    let Some(sig) = with_table(|t| deliverable(t.task(id))) else {
        return;
    };
    // Not a syscall stop, and it must not be reported as one: a tracer that
    // asked about the syscall will find nothing and say so, rather than
    // reading the previous call's arguments back and calling them this stop's.
    with_table(|t| t.task_mut(id).trace.syscall = None);
    park(
        regs,
        Stop {
            signal: sig,
            event: PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: Some(sig),
            syscall: None,
            // A stop reported *for a stop signal* is the tracee's own stop, and
            // a `SIGCONT` ends it. Linux gets there in two steps - the tracer
            // passes the signal on and the task then enters a group stop - and
            // this kernel reports the signal-delivery stop and the group stop as
            // one event. The observable consequence is the one that matters:
            // `kill -CONT` must be able to get a task out of a `SIGSTOP`, and
            // anything that stopped it that way has only that way back.
            group: signal::stops_by_default(sig),
        },
    );
}

/// Stop the running task on a stop signal with nobody tracing it, and hold it
/// there until a `SIGCONT` ends the stop.
///
/// This is the non-ptrace half of "stopped": `kill -STOP`, and Ctrl-Z once
/// there is a keyboard. The parent is told with `WUNTRACED`, which is the only
/// reason `wait` has the flag at all.
pub fn group_stop(regs: &mut AbiRegs, sig: u8) {
    let id = current();
    // A traced task's group stop is its tracer's business as well as its
    // parent's. Linux reports it to the tracer, and it has to: a group stop
    // nobody is told about is a task nothing will ever resume, because the only
    // other way out is a `SIGCONT` and the tracee is inside a tracer that has no
    // shell to send one.
    let traced = with_table(|t| t.task(id).trace.tracer.is_some());
    park(
        regs,
        Stop {
            signal: sig,
            event: PTRACE_EVENT_NONE,
            traced,
            reported: false,
            message: 0,
            delivering: None,
            syscall: None,
            group: true,
        },
    );
}

/// Let a stopped task go on: `PTRACE_CONT`, `PTRACE_SYSCALL`, `PTRACE_DETACH`
/// and the scheduler all end up here.
///
/// `signal` is what the tracer wants delivered, zero for nothing - and that
/// zero is not a no-op. A stop entered to report a signal replaces that signal
/// with whatever the tracer names, so naming nothing is how a tracer drops it.
/// `arm` re-arms the syscall trace.
///
/// Deliberately permissive about a task that is not stopped: a tracer may arm
/// the trace or set options before its tracee has reached its first syscall
/// boundary, and answering `ESRCH` for that would be technically right and
/// useless. A task that is running simply gets the arm, and stops at the next
/// boundary as asked.
pub fn resume_traced(id: TaskId, signal: u8, arm: bool) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            if let Some(reported) = task.trace.stop.and_then(|stop| stop.delivering) {
                task.signals &= !signal::bit(reported);
            }
            task.trace.stop = None;
            task.trace.armed = arm;
            if signal != 0 {
                queue_signal(task, signal, signal::SigInfo::user(signal, 0, 0));
            }
            t.sched.wake(id);
            true
        }
        _ => false,
    })
}

/// Break the tracer link: `PTRACE_DETACH`, and the tracer's own death.
///
/// Whether the tracee comes back with this is not one answer but three, and they
/// are told apart by whose stop it is - see the body. What is always true is
/// that the *trace* is over: the link, the arming and the signal the attach
/// injected all go, because a tracee that kept them would be waiting on a tracer
/// that has gone.
pub fn detach_traced(id: TaskId, signal: u8) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.tracer = None;
            task.trace.armed = false;
            // Measured against Linux, a detach has three cases:
            //
            // * `attach` then `detach(0)` without ever `wait`ing, and the target
            //   runs on. The `SIGSTOP` the attach injected is still merely
            //   pending here, and dropping it is the whole difference: kept, the
            //   target stops itself on the way out of whatever syscall it is in,
            //   with no tracer left and nothing on the machine but a `SIGCONT`
            //   that could end that stop.
            // * The same, after the stop has been reported. That stop is the one
            //   parked with `delivering` set, so ending it is the first case one
            //   step later.
            // * A process the user stopped *before* the attach stays stopped
            //   after the detach, and stays stopped even when the detach names
            //   `SIGCONT` - the signal a detach injects goes to the tracee, not
            //   to its stop. That stop is the task's own, and only a `SIGCONT`
            //   somebody actually sends ends it.
            if task
                .trace
                .stop
                .is_some_and(|stop| stop.delivering.is_some())
            {
                task.trace.stop = None;
            }
            for stop_sig in [
                signal::SIGSTOP,
                signal::SIGTSTP,
                signal::SIGTTIN,
                signal::SIGTTOU,
            ] {
                task.signals &= !signal::bit(stop_sig);
            }
            if signal != 0 {
                queue_signal(task, signal, signal::SigInfo::user(signal, 0, 0));
            }
            // The register file stays where the tracer left it: Linux writes a
            // `PTRACE_SETREGS` straight into the task's own frame, so edits made
            // before the detach belong to the task from then on. `trace.regs` is
            // only read back by the park this task is leaving, and the next stop
            // overwrites it with a fresh snapshot.
            //
            // Waking follows the same three-way split, and for the third case it
            // is the thing to *not* do: a stopped task is held asleep by `park`,
            // so a wake here would release the very stop the detach was told to
            // leave alone.
            if task.trace.stop.is_none() {
                t.sched.wake(id);
            }
            true
        }
        _ => false,
    })
}
