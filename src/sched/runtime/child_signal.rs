use super::STARTED;
use super::child_exit::any_child_code;
use super::child_mm::child_alive;
use super::procfs::comm_of;
use super::task::MAX_TASKS;
use super::task::with_table;
use super::task::{Identity, Table, Task};
use crate::sched::policy::TaskId;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

pub(crate) fn table_has_deliverable(t: &Table) -> bool {
    let task = t.task(t.current);
    let mut ready = task.signals & !task.blocked;
    while let Some(sig) = crate::sched::signal::take(&mut ready) {
        let action = task.dispositions.get(sig);
        if action.is_ignore()
            || (action.is_default()
                && matches!(
                    crate::sched::signal::default_action(sig),
                    crate::sched::signal::DefaultAction::Ignore
                ))
        {
            continue;
        }
        return true;
    }
    false
}

/// Whether a signal that would interrupt a blocking wait is pending, without
/// taking it off the pending set.
///
/// This is the question a blocking syscall asks before it sleeps: Linux
/// answers an interruptible wait with `EINTR` when a catchable, unblocked
/// signal is pending, and leaves the signal itself for the syscall gate to
/// deliver on the way out - the handler runs there, where a register file
/// exists to build its frame on. Discarded signals - `SIG_IGN`, and `SIG_DFL`
/// for a signal whose default action is to be ignored - are not pending
/// interruptions and do not count, same as in [`take_deliverable_signal`].
pub fn has_deliverable_signal() -> bool {
    with_table(|t| table_has_deliverable(t))
}

/// Whether `parent` has a child at all - one still running, or one that has
/// exited and is waiting to be collected.
///
/// `wait` needs the distinction to tell `WNOHANG`'s two "nothing to report"
/// answers apart: a child that has simply not exited yet is a 0, while having no
/// child at all is `ECHILD`.
pub fn has_children(parent: TaskId) -> bool {
    with_table(|t| child_alive(t, parent) || any_child_code(t, parent).is_some())
}
fn discarded_when_generated(task: &Task, sig: u8) -> bool {
    sig != crate::sched::signal::SIGCONT && task.dispositions.get(sig).is_ignore()
}

/// Whether a signal being generated for `task` is discarded because `task` is
/// the **init of its pid namespace** and this is a signal it would take by
/// default.
///
/// Linux's rule, and every clause of it is measured on the host rather than read
/// off a page (`.workbuddy/probe_pid1*.c`; `docs/notes/` §6.3). Four things
/// reach an init that has no handler for the signal, and everything else with a
/// default disposition is dropped:
///
/// * a signal it has a **handler** for is delivered like any other (measured:
///   the handler ran, and the `kill` answered 0 exactly as it does for one that
///   is discarded) - which is why this tests the disposition and not the signal
///   number;
/// * **SIGKILL and SIGSTOP from an ancestor namespace**: `forced` below, and
///   nothing else is let through by it - a SIGTERM from that same ancestor
///   namespace was still ignored, and from inside its own namespace a SIGKILL is
///   ignored too (measured: `kill(1, SIGKILL)`, `kill(1, SIGTERM)` and
///   `kill(1, SIGBUS)` from a sibling all return 0 and all leave the init
///   alive);
/// * a **blocked** signal, which is not judged here at all - Linux checks the
///   blocked masks before it asks this question ("blocked signals are never
///   ignored"), measured with an init that blocks SIGTERM and asks for it with
///   `sigtimedwait`. Without the clause an init could not use
///   `sigwait`/`sigtimedwait`/`signalfd` for what its members send, which is
///   exactly how a program that wants them *as data* reads them;
/// * a **synchronous fault**, also not judged here: the arch's fault handler
///   ends the task itself when the disposition is default (measured: a
///   null-pointer fault killed a namespace's init), because a discarded fault
///   signal would return the task to the faulting instruction for ever.
///
/// A kernel raise *instead of* an event - a timer expiry, a closed pipe, an
/// `alarm` the init armed itself - is **not** forced, and is measured to be
/// discarded: SIGALRM's default action is to terminate, and an init that armed
/// one without a handler survived it. So the kernel is a sender like any other
/// here, and only `force_signal` speaks for the kernel's forced kills.
///
/// The discarded signal still answers success to its sender, because that is
/// what Linux answers: the check is on delivery, not on permission. That is the
/// point of the rule - it exists so that a member of a namespace cannot take its
/// init down by accident, not so that it can *tell* that it may not.
///
/// One clause of Linux's `sig_ignored` is deliberately not here, and is named in
/// `docs/notes/` §6.3 rather than guessed at: a **traced** task does not ignore
/// signals either (`return !t->ptrace`), and whether a traced init receives one
/// this way has not been measured here.
fn init_discards(task: &Task, sig: u8, forced: bool) -> bool {
    let kernel_only = sig == crate::sched::signal::SIGKILL || sig == crate::sched::signal::SIGSTOP;
    task.pid_ns.is_init(task.tgid.0)
        && task.blocked & crate::sched::signal::bit(sig) == 0
        && task.dispositions.get(sig).is_default()
        && !(forced && kernel_only)
}

/// Add `sig` to `task`'s pending set and store the `siginfo_t` a handler will
/// read for it. The one place a bit and its record are set together, so no
/// raise site can leave a handler looking at a zeroed `signo`.
///
/// An ignored signal never gets here in effect: `task`'s disposition decides,
/// and the kernel's own raises (a timer expiry, a broken pipe, a child's exit)
/// follow the same rule a `kill` does - including against a namespace's init,
/// which is measured to survive an `alarm(2)` it armed itself.
pub(crate) fn queue_signal(task: &mut Task, sig: u8, info: crate::sched::signal::SigInfo) {
    if discarded_when_generated(task, sig) || init_discards(task, sig, false) {
        return;
    }
    enqueue(task, sig, info);
}

/// Add `sig` to `task`'s pending set as a **forced** kernel raise - Linux's
/// `force_sig`, the `SEND_SIG_PRIV` of a `do_exit`'s neighbourhood rather than a
/// sender's `kill` - and wake nobody: the callers wake their own targets.
///
/// The one kernel path that reaches past a pid namespace init's protection
/// ([`init_discards`]), which is why it exists: a namespace's death is delivered
/// with SIGKILL, and a nested namespace's *own* init is one of its targets - a
/// task whose members cannot end it, so a non-forced kill would leave a
/// namespace running inside one that is gone.
pub(crate) fn force_signal(task: &mut Task, sig: u8) {
    if discarded_when_generated(task, sig) || init_discards(task, sig, true) {
        return;
    }
    enqueue(task, sig, crate::sched::signal::SigInfo::kernel(sig));
}

/// Put a raised signal where its ABI says it belongs. A standard signal
/// coalesces into `siginfo[sig]` — a second copy before the first is handled
/// changes nothing. A real-time signal is pushed onto the per-task FIFO ring so
/// every copy survives with its own `si_value`, and its `Pending` bit stays set
/// while the ring still holds one of that number. A real-time push onto a full
/// ring is dropped: the bound that keeps a `sigqueue` flood from growing memory
/// (the worst-case rule).
fn enqueue(task: &mut Task, sig: u8, mut info: crate::sched::signal::SigInfo) {
    if crate::sched::signal::is_rt(sig) {
        // Stamp the number so `RtQueue::pop`/`holds` can match it later; a
        // user `siginfo_t` could otherwise carry an unrelated `si_signo`.
        info.signo = sig as i32;
        if task.rtq.push(info) {
            crate::sched::signal::add(&mut task.signals, sig);
        }
    } else {
        crate::sched::signal::add(&mut task.signals, sig);
        if (sig as usize) < crate::sched::signal::SIGINFO_SLOTS {
            task.siginfo[sig as usize] = info;
        }
    }
}

/// Send `sig` to `id` with an `SI_USER` record naming the caller. The plain
/// `kill`/`tgkill` shape; a `sigqueue` goes through [`send_signal_info`] so it
/// can carry a payload.
pub fn send_signal(id: TaskId, sig: u8) -> bool {
    let (pid, uid) = with_table(|t| {
        let cur = t.task(t.current);
        (cur.tgid.0 as i32, cur.identity.ruid as i32)
    });
    send_signal_info(id, sig, crate::sched::signal::SigInfo::user(sig, pid, uid))
}

/// Send `sig` to `id`, recording `info` as the `siginfo_t` a handler will read.
///
/// Returns `false` when the task does not exist or has already exited, which
/// the syscall layer reports as `ESRCH`.
///
/// `sig` of 0 is the POSIX existence probe: it delivers nothing and only
/// reports liveness. With no handler machinery there is nothing to deliver to
/// anyway: the bit waits in the task's pending set, and the task dies at its
/// next blocking point.
pub fn send_signal_info(id: TaskId, sig: u8, info: crate::sched::signal::SigInfo) -> bool {
    with_table(|t| {
        // The first look is immutable: whether the signal is delivered,
        // discarded or refused, the answer is read off the same facts.
        let Some(task) = t.tasks.get(id.0).and_then(Option::as_ref) else {
            return false;
        };
        if task.exited {
            return false;
        }
        if sig == 0 {
            // The existence probe delivers nothing. Every answer it has is in
            // the `true` above.
            return true;
        }
        // POSIX 2.4.3: a stop signal generated for a member of an orphaned
        // process group is discarded. The group is one whose members' parents
        // are all gone or all in other sessions - a group nothing could
        // un-stop, because the only un-stop is a SIGCONT from outside and no
        // same-session parent outside the group is left to send one. A shell
        // that finds itself in the background of its terminal raises SIGTTIN
        // to its own group exactly this way; discarding it is the difference
        // between "job control off" and a login that freezes forever.
        // `SIGSTOP` is not on the list: it is uncatchable, and `kill -STOP`
        // stays exactly what it says.
        if crate::sched::signal::stops_by_default(sig)
            && sig != crate::sched::signal::SIGSTOP
            && orphaned_group(t, task.pgid)
        {
            return true;
        }
        // And a namespace's init is not endable from inside its own namespace.
        // The `forced` half is Linux's: a sender in an **ancestor** namespace
        // may push SIGKILL/SIGSTOP through, and that is the whole of it - a
        // SIGTERM from that same ancestor is still discarded. The kernel's own
        // raises are not forced either (measured: an `alarm` an init armed
        // itself is discarded); `force_signal` is what speaks for those. The
        // signal is discarded, not refused: the `true` below is the answer
        // Linux gives, and a `kill` dropped this way still returns 0.
        let forced = t.task(t.current).pid_ns.is_ancestor_of(&task.pid_ns);
        if init_discards(task, sig, forced) {
            return true;
        }
        let Some(task) = t.tasks.get_mut(id.0).and_then(Option::as_mut) else {
            return false;
        };
        // An ignored signal is discarded as it is generated: the process has
        // said it does not want it, and a bit left pending would come back to
        // life - with its *default* action - the moment the disposition
        // changed. `SIGCONT` is exempt, and the state change it carries is
        // below this: ignoring a resume does not mean "stay stopped".
        if !discarded_when_generated(task, sig) {
            enqueue(task, sig, info);
        }
        // `SIGCONT` is the one signal whose effect is on the *state* of a task        // rather than on its pending set, and this is the only place a task's
        // state can be changed from outside it.
        if sig == crate::sched::signal::SIGCONT {
            // It cancels the stop signals still queued, so that a task woken
            // by it does not stop again on the way out.
            for stop_sig in [
                crate::sched::signal::SIGSTOP,
                crate::sched::signal::SIGTSTP,
                crate::sched::signal::SIGTTIN,
                crate::sched::signal::SIGTTOU,
            ] {
                task.signals &= !crate::sched::signal::bit(stop_sig);
            }
            // And it ends a group stop. A *traced* stop is deliberately not
            // ended here: only the tracer may resume its tracee, and a
            // `SIGCONT` that resumed one would not break the stop so much as
            // the protocol the stop exists for.
            if task.trace.stop.is_some_and(|stop| stop.group) {
                task.trace.stop = None;
                task.trace.continued = true;
            }
        }
        // A stop signal has nothing to say to a task that is already stopped,
        // and waking it would say the opposite of the signal: the park a group
        // stop sits in is held by the scheduler's sleep, so a wake releases it
        // and the task runs on with a stop nobody ended. `kill -STOP` on a
        // *running* task still has to wake it, or the stop waits for the next
        // syscall boundary - and one that arrives while it is parked in a
        // `PTRACE_ATTACH`'s own queue is the case a debugger's target is in.
        let held = crate::sched::signal::stops_by_default(sig)
            && task.trace.stop.is_some_and(|stop| stop.group);
        if !held {
            t.sched.wake(id);
        }
        true
    })
}

/// Whether `pgid` is an orphaned process group: no live member has a live
/// parent in the same session outside the group.
///
/// That is POSIX's definition, in the words this kernel can now honour. The
/// parent that keeps a group un-orphaned is one `wait`ing in the same
/// session but a different group - the shell behind a job. A getty's session
/// has no such parent: the login shell's parent, login, is in the group, and
/// login's own parent, init, sits in another session - so a login shell that
/// raises a stop signal to itself has it discarded rather than stranded
/// forever with the screen frozen behind it.
fn orphaned_group(t: &Table, pgid: TaskId) -> bool {
    t.tasks.iter().flatten().all(|task| {
        !(task.pgid == pgid
            && task
                .parent
                .and_then(|parent| t.tasks.get(parent.0).and_then(Option::as_ref))
                .is_some_and(|parent| {
                    !parent.exited && parent.session == task.session && parent.pgid != pgid
                }))
    })
}

/// The process group the running task belongs to.
pub fn current_pgid() -> TaskId {
    with_table(|t| t.task(t.current).pgid)
}

/// The group `id` is in, or `None` when the task is gone.
pub fn pgid_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().map(|task| task.pgid))
}

/// Bind the running task's controlling terminal, by the terminal's device id.
///
/// `TIOCSCTTY` is the caller. Binding is two facts at once: `/dev/tty` opens
/// onto this terminal for this process from now on, and - the Linux
/// `__proc_set_tty` behaviour - the terminal's foreground goes to the
/// acquiring process's group, so the getty that just became a session leader
/// also owns the line before anyone reads or writes it.
pub fn acquire_controlling_tty(dev_id: (u32, u32)) {
    with_table(|t| {
        t.task_mut(t.current).ctty = Some(dev_id);
    });
    // Outside the table borrow: the foreground is the tty's own state, and
    // `set_foreground` takes its own critical section.
    if let Some(tty) = crate::vfs::tty::by_dev_id(dev_id) {
        tty.set_foreground(Some(current_pgid()));
    }
}

/// The controlling terminal of the running task, by device id - what an
/// open of `/dev/tty` resolves to. `None` is a process that has acquired
/// none, and there is no default to fall back on: a controlling terminal is
/// the process's own history, not a machine-wide answer.
#[cfg(target_os = "none")]
pub fn controlling_tty_of_current() -> Option<(u32, u32)> {
    with_table(|t| t.task(t.current).ctty)
}

/// The identity of the running task: the real, effective and saved ids of
/// its user and group. This is what `getuid`'s family reads and what
/// `setuid`'s family writes, after the syscall layer has applied the man
/// page's rules - the rules live there, because they are answers about
/// calls, and only the storage lives here.
#[cfg(target_os = "none")]
pub fn current_identity() -> Identity {
    with_table(|t| t.task(t.current).identity.clone())
}

/// The six ids of an [`Identity`], as [`crate::cred`] compares them.
fn ids_of(identity: &Identity) -> crate::cred::Ids {
    crate::cred::Ids {
        ruid: identity.ruid,
        euid: identity.euid,
        suid: identity.suid,
        rgid: identity.rgid,
        egid: identity.egid,
        sgid: identity.sgid,
    }
}

/// The running process's credentials, as a snapshot.
pub fn current_credentials() -> crate::cred::Credentials {
    with_table(|t| t.task(t.current).mm.credentials())
}

/// The user namespace the running task reads its ids through.
///
/// Every observation of an id goes through this: `getuid`'s family translates
/// the stored id it holds, and the moment the caller is in a namespace with an
/// empty table (which is the state `unshare(CLONE_NEWUSER)` leaves it in) it
/// reads [`crate::userns::NO_MAP_ID`] for everything, its own id included. A task
/// on a machine that never unshared is in the machine's namespace, whose identity
/// map makes this a pass-through.
pub fn current_user_ns() -> Arc<crate::userns::UserNs> {
    with_table(|t| Arc::clone(&t.task(t.current).user_ns))
}

/// Change one field of the running process's credentials in place. The
/// read-modify-write is the table lock's, so two of these cannot interleave.
pub fn with_current_credentials<R>(f: impl FnOnce(&mut crate::cred::Credentials) -> R) -> R {
    with_table(|t| t.task(t.current).mm.with_credentials(f))
}

/// Another task's credentials, or `None` when no such task is live.
pub fn credentials_of(id: TaskId) -> Option<crate::cred::Credentials> {
    with_table(|t| {
        let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
        (!task.exited).then(|| task.mm.credentials())
    })
}

/// Another task's user and group identity, or `None` when no such task is
/// live.
///
/// The shape [`credentials_of`] has, and it exists for the same reason: the
/// caller is not the task, so `current_identity` cannot answer. `waitid`
/// writes a *stopped* or *continued* child's real uid into its record, and
/// those two events are reported while the child is still in the table - a
/// death, which is not, carries the uid on the record instead
/// ([`crate::sched::Retired`]).
pub fn identity_of(id: TaskId) -> Option<Identity> {
    with_table(|t| {
        let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
        (!task.exited).then(|| task.identity.clone())
    })
}

/// May the running task inspect `target` - the question `PTRACE_ATTACH` and
/// `process_vm_readv`/`writev` both ask? The policy is
/// [`crate::cred::ptrace_may_access`]; this gathers the two sides of it.
///
/// Three answers rather than two, because the callers owe different errnos:
/// `None` when no such task is live (`ESRCH`), `Some(false)` when it is live
/// and refused (`EPERM`), `Some(true)` when it may be inspected. A task always
/// passes for its own thread group, so a thread reading its own memory through
/// `process_vm_*` is unaffected.
pub fn may_inspect(target: TaskId) -> Option<bool> {
    with_table(|t| {
        let me = t.current;
        let them = t.tasks.get(target.0).and_then(Option::as_ref)?;
        if them.exited {
            return None;
        }
        let same_group = t.task(me).tgid == them.tgid;
        Some(crate::cred::ptrace_may_access(
            ids_of(&t.task(me).identity),
            &t.task(me).mm.credentials(),
            ids_of(&them.identity),
            &them.mm.credentials(),
            same_group,
        ))
    })
}

/// Store the running task's identity. The caller has validated the change;
/// this is the table's half of the bargain.
///
/// It is also where the credential rules hang, because every path that moves a
/// uid or gid - `setuid`/`setresuid`/`setreuid` and their group spellings -
/// lands here and nowhere else. A `setgroups` also lands here, with the same
/// six ids, which is why the rule asks whether any of them actually moved
/// before touching a capability: Linux does not clear a capability because a
/// process re-declared its supplementary groups.
#[cfg(target_os = "none")]
pub fn set_current_identity(identity: Identity) {
    with_table(|t| {
        let id = t.current;
        let old = t.task(id).identity.clone();
        let mm = Arc::clone(&t.task(id).mm);
        let (old_ids, new_ids) = (ids_of(&old), ids_of(&identity));
        t.task_mut(id).identity = identity;
        mm.with_credentials(|c| c.on_identity_change(old_ids, new_ids));
    });
}

/// The running task's file-creation mask - what `umask(2)` reports, and what
/// every create masks the mode it was given with.
pub fn current_umask() -> u32 {
    with_table(|t| t.task(t.current).fs.umask())
}

/// Store the running task's file-creation mask. The syscall layer has already
/// narrowed it to the twelve bits a mode can hold. It lands on the process's
/// shared [`crate::sched::FsStruct`], so a thread's `umask` is its process's.
pub fn set_current_umask(mask: u32) {
    with_table(|t| t.task(t.current).fs.set_umask(mask));
}

/// The OOM score adjustment of `pid` and the floor of it, as
/// `/proc/<pid>/oom_score_adj` reads the pair. `None` when no such task exists
/// - which is what makes the file go away with the process it described.
#[cfg(target_os = "none")]
pub fn oom_score_adj_of(pid: usize) -> Option<(i32, i32)> {
    with_table(|t| {
        t.tasks
            .get(pid)
            .and_then(Option::as_ref)
            .map(|tk| (tk.oom_score_adj, tk.oom_score_adj_min))
    })
}

/// Write `pid`'s OOM score adjustment, taking the floor down with it when the
/// new value asks for a more protective one - Linux's `sig->oom_score_adj_min`,
/// the record of the most protective value this process has ever been given.
/// `false` says the process is gone, so a write racing an exit reports the
/// vanished file instead of pretending to have changed it.
///
/// Who may lower the floor is not decided here: that is a rule about the
/// writer, and the file answers it where the errnos are mapped.
#[cfg(target_os = "none")]
pub fn set_oom_score_adj(pid: usize, value: i32) -> bool {
    with_table(|t| match t.tasks.get_mut(pid) {
        Some(Some(task)) => {
            task.oom_score_adj = value;
            if value < task.oom_score_adj_min {
                task.oom_score_adj_min = value;
            }
            true
        }
        _ => false,
    })
}

/// `pid`'s nice value, or `None` when no such live task exists. Backs
/// `getpriority`'s `PRIO_PROCESS`.
#[cfg(target_os = "none")]
pub fn nice_of(pid: usize) -> Option<i32> {
    with_table(|t| {
        t.tasks
            .get(pid)
            .and_then(Option::as_ref)
            .filter(|tk| !tk.exited)
            .map(|tk| tk.nice)
    })
}

/// Set `pid`'s nice value. `false` says the process is gone (or was never
/// there), which `setpriority` reports as `ESRCH`.
#[cfg(target_os = "none")]
pub fn set_nice(pid: usize, value: i32) -> bool {
    with_table(|t| match t.tasks.get_mut(pid) {
        Some(Some(task)) if !task.exited => {
            task.nice = value;
            true
        }
        _ => false,
    })
}

/// The live tasks whose real uid is `ruid`. Backs `getpriority`/`setpriority`'s
/// `PRIO_USER`.
#[cfg(target_os = "none")]
pub fn uid_members(ruid: u32) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.identity.ruid == ruid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Record the running process's `comm` from `PR_SET_NAME`: `name` is the
/// caller's bytes, stored NUL-terminated within the 16 that `TASK_COMM_LEN`
/// allows. This is what makes `PR_SET_NAME` do something - and `/proc`'s `comm`
/// and `PR_GET_NAME` report it - instead of accepting and dropping.
#[cfg(target_os = "none")]
pub fn set_current_comm(name: &[u8]) {
    with_table(|t| t.task(t.current).mm.set_comm_name(name));
}

/// The running process's `comm` as the 16 bytes `PR_GET_NAME` hands back: the
/// `PR_SET_NAME` bytes verbatim if one was set, else the derived basename packed
/// NUL-terminated - matching Linux, which reports the image name, not nothing.
#[cfg(target_os = "none")]
pub fn current_comm_bytes() -> [u8; 16] {
    with_table(|t| {
        let task = t.task(t.current);
        let stored = task.mm.comm_name_bytes();
        if stored[0] != 0 {
            return stored;
        }
        let derived = comm_of(task);
        let mut out = [0u8; 16];
        for (slot, &b) in out.iter_mut().zip(derived.as_bytes().iter().take(15)) {
            *slot = b;
        }
        out
    })
}

/// The UTS namespace the running task reads its names from - `uname`'s
/// `nodename` and `domainname`, and what `/proc/sys/kernel/{hostname,domainname}`
/// report.
///
/// The `Arc` is cloned out of the table and the table's lock dropped before the
/// caller asks the namespace anything, so reading a name cannot happen with the
/// scheduler's lock held while it allocates the `String` it hands back. That is
/// the same shape as `current_fds` above, and for the same reason.
#[cfg(target_os = "none")]
pub fn current_uts() -> Arc<crate::uts::Uts> {
    with_table(|t| Arc::clone(&t.task(t.current).uts))
}

/// Whether the running task's UTS namespace answers `uname`'s
/// `release`/`version` with §10.5's private disk-counter protocol.
///
/// One read per caller, lock held only to clone an `Arc`, exactly like
/// [`current_uts`] - and the same reason: the caller may format strings after
/// this returns, which allocates, which must not happen under the table lock.
/// The network namespace the running task resolves through, or `None` for the
/// machine's own.
///
/// A plain number, not the object: `net` names `sched` (it wakes the tasks parked
/// on a socket), so `sched` returning a `net` type would make the dependency
/// mutual - and `net` is the layer that resolves this number to a stack, which is
/// the only place the stack needs to exist.
#[cfg(target_os = "none")]
pub fn current_net_ns() -> Option<u64> {
    with_table(|t| t.task(t.current).net_ns)
}

/// Move the running task into a network namespace: the `clone(CLONE_NEWNET)`
/// child's own assignment (the stack itself is `net`'s, minted before this is
/// called, because the scheduler may not name that layer).
#[cfg(target_os = "none")]
pub fn set_net_ns(ns: Option<u64>) {
    with_table(|t| {
        let me = t.current;
        t.task_mut(me).net_ns = ns;
    });
}

#[cfg(target_os = "none")]
pub fn current_diskstat() -> bool {
    with_table(|t| t.task(t.current).uts.diskstat())
}

/// Arm §10.5's disk-counter protocol on the boot task's UTS namespace.
///
/// Called once, by the boot flow, before pid 1 exists — and it lands on the
/// *init namespace*, which every later task inherits by reference, so one call
/// arms the machine. A task that unshares its UTS namespace copies the flag
/// with the names: the payload is rendered from the live counters at read
/// time, so a private namespace inside an armed boot still reads real numbers.
///
/// The arming goes through `uts::arm_init`, not through the task table: the
/// boot flow runs this before `sched::init()` adopts the boot task, when there
/// is no table entry for `t.current` to name yet (the boot task is adopted
/// into slot 0 only there), and `Table::task` panics on an absent slot. The
/// init namespace is what every later task is created with (`create_at` hands
/// each new task `uts::init()`), so arming it before any task exists reaches
/// every task afterwards — which is the same inheritance the doc above
/// describes, just taken from the namespace rather than from a table row.
#[cfg(target_os = "none")]
pub fn arm_diskstat_boot_namespace() {
    crate::uts::arm_init();
}

/// Give the running task a namespace of its own: `unshare(CLONE_NEWUTS)`.
///
/// The new namespace is a **copy** of the old one - Linux's `copy_utsname`,
/// reached from `create_new_namespaces` - so the caller keeps reporting the names
/// it was already reporting and can now change them without the namespace it came
/// from following. The second half is the whole feature: a caller handed an
/// *empty* namespace would have its own `uname` change under it at the instant of
/// the call, which is not what a program asking for its own namespace is asking
/// for, and would make the call a way to lose state rather than to isolate it.
///
/// Only the calling task moves. A thread of the same process is deliberately left
/// pointing at the old namespace, which is the same thing Linux's per-task
/// `nsproxy` means - `unshare` splits the *caller* from its process, and a thread
/// that wanted to drag its siblings along would have to say so.
#[cfg(target_os = "none")]
pub fn unshare_uts() {
    with_table(|t| {
        let fresh = t.task(t.current).uts.copied(crate::vfs::ns::mint());
        t.task_mut(t.current).uts = fresh;
    });
}

/// The namespace one **named** task is in, for `/proc/<pid>/ns/<kind>`.
///
/// The twin of [`current_fs_context`] for a pid, and the reason this exists is
/// the whole reason that surface does: `/proc/<pid>/ns` is about *another*
/// process, so a reader has to be able to ask for a task it is not. `None` when
/// nothing answers to that pid (the directory checked a moment ago; this is the
/// same answer one instant later, which is what a name about live state means
/// here), and `None` for a task that has never unshared resolves to the
/// machine's own table and names - the namespace every process starts in.
pub fn ns_of(pid: usize, entry: crate::vfs::ns::Entry) -> Option<crate::vfs::ns::Namespace> {
    use crate::vfs::ns::Slot;
    with_table(|t| {
        let task = t.tasks.get(pid).and_then(|slot| slot.as_ref())?;
        Some(match (entry.kind, entry.slot) {
            (crate::vfs::ns::Kind::Mount, _) => crate::vfs::ns::Namespace::Mount(
                task.fs_ns
                    .clone()
                    .unwrap_or_else(crate::vfs::mount::machine_table),
            ),
            (crate::vfs::ns::Kind::Uts, _) => crate::vfs::ns::Namespace::Uts(task.uts.clone()),
            // Linux's `pidns_get` answers the namespace the task *is in* - the
            // one its own number comes from - while a `setns` against this
            // descriptor moves the namespace its *children* go to. The
            // asymmetry is Linux's, and it is what makes `setns` on
            // `/proc/self/ns/pid` a no-op that still means "my children stay
            // where they are". `/proc/<pid>/ns/pid_for_children` is the other
            // slot and answers the other half with no asymmetry at all.
            (crate::vfs::ns::Kind::Pid, Slot::Own) => {
                crate::vfs::ns::Namespace::Pid(task.pid_ns.clone())
            }
            (crate::vfs::ns::Kind::Pid, Slot::ForChildren) => {
                crate::vfs::ns::Namespace::Pid(task.pid_ns_children.clone())
            }
            // A task in the machine's namespace holds no number of its own, so
            // the link names the machine's - minted in the identity layer, which
            // both this layer and `net` may name.
            (crate::vfs::ns::Kind::Net, _) => crate::vfs::ns::Namespace::Net(
                task.net_ns.unwrap_or_else(crate::vfs::ns::machine_net_id),
            ),
            // Both slots, and here the two really do differ for a task that has
            // unshared: `time` is the namespace its own clocks come from and
            // `time_for_children` the one the next child will be born under,
            // which is the namespace `/proc/<pid>/timens_offsets` writes.
            (crate::vfs::ns::Kind::Time, Slot::Own) => {
                crate::vfs::ns::Namespace::Time(task.time_ns.clone())
            }
            (crate::vfs::ns::Kind::Time, Slot::ForChildren) => {
                crate::vfs::ns::Namespace::Time(task.time_ns_children.clone())
            }
            // One slot, because the kind has only one: `CLONE_NEWUSER` moves the
            // caller, so there is no `user_for_children` name and both spellings
            // - there is only one - answer the task's own namespace.
            (crate::vfs::ns::Kind::User, _) => {
                crate::vfs::ns::Namespace::User(task.user_ns.clone())
            }
        })
    })
}

/// Join a mount namespace: `setns(fd, CLONE_NEWNS)`'s scheduler half, and the
/// deliberate opposite of [`unshare_mnt`] - a join *adopts* the namespace the
/// descriptor holds rather than copying the caller's, so the caller and every
/// process already in that namespace see each other's mounts from here on
/// (Linux's `mntns_install`). Joining the namespace one is already in is
/// therefore a no-op that still succeeds, which is what `setns` on
/// `/proc/self/ns/mnt` is for everybody who has never unshared.
///
/// The caller's root moves with it, for the reason [`MountTable::ns_root`]
/// states. The caller's *cwd* does not: Linux re-resolves it by path inside the
/// new namespace, and a directory the new namespace cannot reach is left where
/// it is - the inode stays valid either way, and the re-resolution is named as
/// not modelled in `docs/notes/` rather than half-done here.
pub fn setns_mnt(table: Arc<crate::vfs::mount::MountTable>) {
    with_table(|t| {
        let me = t.current;
        let root = table.ns_root();
        t.task_mut(me).fs_ns = Some(table);
        if let Some(root) = root {
            t.task(me).fs.set_root(root);
        }
    });
}

/// Join a UTS namespace: `setns(fd, CLONE_NEWUTS)`'s half. The names come with
/// the object, by reference - the caller now reads and renames the namespace
/// the descriptor named, and the one it left is untouched.
pub fn setns_uts(uts: Arc<crate::uts::Uts>) {
    with_table(|t| {
        let me = t.current;
        t.task_mut(me).uts = uts;
    });
}

/// Give the running task's **children** a time namespace of their own:
/// `unshare(CLONE_NEWTIME)`.
///
/// The new namespace is **empty** - zero offsets - and the caller does not move
/// into it. Both halves are Linux's and both are measurable: `create_time_ns`
/// makes a fresh one rather than a copy, and it is `time_ns_for_children` that
/// the call replaces, so the caller's own `CLOCK_MONOTONIC` is untouched
/// whatever the offsets are set to afterwards. That is why the order a userland
/// caller has to use is *unshare, write the offsets, fork* - and why a test that
/// read the caller's own clock here would see nothing and prove nothing.
///
/// Only the calling task's children move, the rule [`unshare_uts`] states and
/// `unshare_pid` follows: `unshare` splits the caller, and a sibling that wanted
/// the same namespace would have to ask too.
#[cfg(target_os = "none")]
pub fn unshare_time() {
    with_table(|t| {
        let me = t.current;
        let fresh = crate::timens::TimeNs::fresh(crate::vfs::ns::mint());
        t.task_mut(me).time_ns_children = fresh;
    });
}

/// Join a time namespace a descriptor names: `setns(fd, CLONE_NEWTIME)`'s half.
///
/// The same slot `unshare` replaces - `nsset->nsproxy->time_ns_for_children` in
/// Linux's `timens_install`, which is a join rather than a copy and which does
/// **not** move the caller either. Measured: `setns` onto a namespace that
/// already has a process in it answers `0` (the freeze governs the offsets
/// file, not who may come in), and the caller's own clocks go on reading the
/// namespace it was in.
#[cfg(target_os = "none")]
pub fn setns_time(ns: Arc<crate::timens::TimeNs>) {
    with_table(|t| {
        let me = t.current;
        t.task_mut(me).time_ns_children = ns;
    });
}

/// Move the running task into a user namespace of its own:
/// `unshare(CLONE_NEWUSER)`.
///
/// **The caller moves** - the shape `CLONE_NEWNET` has and the opposite of the
/// two for-children kinds - which is why this sets `user_ns` itself rather than a
/// children's slot. The namespace it lands in is **empty**, so the caller's own
/// ids have no inside name until it writes the map: measured, `getuid()` reads
/// `65534` between this call and the first `uid_map` write (§6.3).
///
/// **Making the namespace is the grant.** The caller holds the full capability
/// set the instant it is in the namespace it made, whatever it held before -
/// which is why this door asks for no capability of its own and why an
/// unprivileged caller can use it at all. The rule, its four measured doors
/// and what it deliberately does *not* touch are [`crate::cred::Credentials::on_enter_user_ns`]'s;
/// it is applied here, beside the move, so the two cannot drift apart.
#[cfg(target_os = "none")]
pub fn unshare_user() {
    with_table(|t| {
        let me = t.current;
        let mm = Arc::clone(&t.task(me).mm);
        let fresh = crate::userns::UserNs::fresh(crate::vfs::ns::mint());
        t.task_mut(me).user_ns = fresh;
        mm.with_credentials(|c| c.on_enter_user_ns());
    });
}

/// Join a user namespace a descriptor names: `setns(fd, CLONE_NEWUSER)`'s half.
///
/// A join, not a copy, and it moves the caller like [`unshare_user`] does. The
/// one measured divergence from every other kind's `setns` is that joining the
/// namespace the caller is **already in** is refused `EINVAL` rather than being
/// the no-op `0` MNT's and TIME's are - which is why this answers a `Result`
/// where its siblings answer nothing.
///
/// The join is a grant as well as a move: entering the namespace hands the
/// caller the full set in it, measured the same as `unshare`'s
/// (`on_enter_user_ns`, probe-userns7.c scenario E - a caller holding only
/// `CAP_SYS_ADMIN`, the one the join needs, read the full set back afterwards).
#[cfg(target_os = "none")]
pub fn setns_user(ns: Arc<crate::userns::UserNs>) -> Result<(), isize> {
    with_table(|t| {
        let me = t.current;
        if Arc::ptr_eq(&t.task(me).user_ns, &ns) {
            return Err(crate::err::EINVAL);
        }
        let mm = Arc::clone(&t.task(me).mm);
        t.task_mut(me).user_ns = ns;
        mm.with_credentials(|c| c.on_enter_user_ns());
        Ok(())
    })
}

/// The offsets the running task's own clocks are read through, as
/// `(monotonic, boottime)` nanoseconds. Zero on both for every task that never
/// entered a time namespace, which is every ordinary task on the machine.
///
/// Asked of the task's `time_ns` and not of its `time_ns_children`: this is the
/// namespace the caller is *in*, which is the one its `clock_gettime` reads, and
/// a task that unshared sees its own clocks unmoved precisely because those two
/// slots differ.
pub fn time_offsets() -> (i64, i64) {
    with_table(|t| t.task(t.current).time_ns.offset_nanos())
}

/// Charge one clock tick of CPU time to the running task's process group,
/// attributed user or system by the ring the timer interrupt took the core out
/// of. Called from the tick handler before it preempts, so the ticks land on the
/// task that was actually running. Two atomic increments - no allocation, no
/// sleep, bounded, as `AGENTS.md` rule 5 requires of interrupt context.
#[cfg(target_os = "none")]
pub fn charge_cpu(in_user: bool) {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    // Read `current` through the same `get(...).and_then(as_ref)` the panic
    // path uses, not `task()` - `task()` asserts a slot exists and the tick can
    // land while `current` names a slot that is momentarily empty. Nobody to
    // charge means the tick is simply not attributed; the machine is whole.
    with_table(|t| {
        if let Some(task) = t.tasks.get(t.current.0).and_then(|slot| slot.as_ref()) {
            task.mm.charge_tick(in_user);
        }
    });
}

/// The running process's CPU time in internal clock ticks, `(user, system)`.
/// `times()` and `getrusage(RUSAGE_SELF)` read it and convert to their own
/// units; there is no per-thread split because there are no threads here, and a
/// group's `Mm` is where the whole process's ticks gather. `(0, 0)` before the
/// scheduler runs or if `current` names no live task.
#[cfg(target_os = "none")]
pub fn current_cpu_ticks() -> (u64, u64, u64, u64) {
    with_table(
        |t| match t.tasks.get(t.current.0).and_then(|slot| slot.as_ref()) {
            Some(task) => task.mm.cpu_all(),
            None => (0, 0, 0, 0),
        },
    )
}

/// The group ids the running task holds - real, effective and saved.
///
/// Linux asks this as "is the file's group one of the caller's?", and this
/// kernel keeps no supplementary groups, so the three words are the whole of
/// the answer.
pub fn current_groups() -> [u32; 3] {
    let id = current_identity();
    [id.rgid, id.egid, id.sgid]
}

/// `setsid`'s kernel half beyond the group move: the caller becomes the
/// leader of its own session and loses the controlling terminal, which
/// belongs to the session it just left. The getty that calls this re-acquires
/// a terminal with `TIOCSCTTY` immediately after; everything else that
/// `setsid`s meant to leave its terminal behind anyway.
/// The effective user and group of the running process - the ids a file it
/// creates belongs to.
///
/// A *filesystem* asks, because that is where a new node is stamped and there
/// is no way to stamp it later without walking the tree: the creator's
/// identity is not a property of the name, it is a property of the moment.
/// The effective ids are the ones Linux uses, not the real ones.
pub fn current_owner() -> (u32, u32) {
    let id = current_identity();
    (id.euid, id.egid)
}

/// The *real* ids of the running task, which is the pair `access(2)` asks
/// about rather than the effective one above. The distinction is the whole
/// point of the call: a setuid program asking whether the user who invoked
/// it could open a file must not be answered with its own raised
/// privileges.
pub fn current_real_owner() -> (u32, u32) {
    let id = current_identity();
    (id.ruid, id.rgid)
}

/// Whether the running task is superuser, by its real id - the test Linux
/// substitutes for `CAP_SYS_ADMIN` on a kernel without namespaces, which is what
/// the console's ioctls ask when a caller is not the terminal's own foreground
/// session. Real rather than effective because that is the id `tty_ioctl`'s
/// checks read (`uid_eq(tty_audit_uid(...))` aside, the console has never been a
/// place where a setuid bit bought you someone else's screen).
pub fn current_is_root() -> bool {
    current_identity().ruid == 0
}

pub fn become_session_leader() {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        task.session = id;
        task.ctty = None;
    });
}

/// The session id of the session whose controlling terminal is `dev_id`, or
/// `None` when no task holds that terminal - what `TIOCGSID` reports, and what
/// `tcgetsid` is.
///
/// A job-control shell asks this before it decides whether the terminal is
/// *its* terminal to claim: the session leader that owns the line is the one
/// that may take it, and a shell that cannot find out has to assume it may
/// not. The answer is the leader's id, which is the session id everywhere in
/// the ABI.
pub fn session_of_tty(dev_id: (u32, u32)) -> Option<usize> {
    with_table(|t| {
        t.tasks
            .iter()
            .flatten()
            .find(|task| !task.exited && task.ctty == Some(dev_id))
            .map(|task| task.session.0)
    })
}

/// Everyone in a process group, for a signal that has to reach all of them at
/// once - which is what makes Ctrl-C stop a whole pipeline rather than one
/// program of it.
pub fn group_members(pgid: TaskId) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.pgid == pgid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Send `sig` to every member of a process group. Answers whether it reached
/// anyone, which is what `kill` reports for a group.
pub fn send_signal_group(pgid: TaskId, sig: u8) -> bool {
    let mut delivered = false;
    for id in group_members(pgid) {
        delivered |= send_signal(id, sig);
    }
    delivered
}

/// The live tasks of one thread group: the process that a pid names.
pub fn process_members(tgid: TaskId) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.tgid == tgid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Linux's `kill_pid(vc->vt_pid, sig, 1)`: hand `sig` to one *process* - every
/// live thread of the group `tgid` leads - and answer the question the console
/// layer actually acts on: whether that process still exists at all.
///
/// The answer matters more than the delivery. `VT_SETMODE`'s `VT_PROCESS` is a
/// promise made by a process, and Linux's `change_console` and `vt_acquire` send
/// its `relsig`/`acqsig` to the recorded pid precisely so that the death of that
/// process is *heard* - as `kill_pid` failing - at the next handover. That is
/// what lets a dead compositor's screen be taken back on the spot (`reset_vc`,
/// and the switch falls through as if the console had been `VT_AUTO`) instead of
/// stranding it behind a timer nobody living can answer. A sender that reported
/// only success would leave the console with no way to learn its promise has no
/// keeper left, and a promise aimed at a *terminal* rather than at the process
/// that made it is worse still: it kills whichever session happens to be in the
/// foreground when the signal goes out.
pub fn send_signal_process(tgid: TaskId, sig: u8) -> bool {
    let mut delivered = false;
    for id in process_members(tgid) {
        delivered |= send_signal(id, sig);
    }
    delivered
}

/// `setpgid(pid, pgid)` — put a task into a process group, creating the group
/// when the caller names the task's own id.
///
/// Linux's rules, less the session half: the target has to exist, the group
/// has to exist or be the target itself, and the caller may only move itself
/// or one of its own children. The one that matters in practice is a shell
/// calling `setpgid(0, 0)` in a freshly forked child, which is what gives a
/// job a group of its own to be signalled through.
pub fn set_pgid(id: TaskId, pgid: TaskId) -> isize {
    with_table(|t| {
        let Some(target) = t.tasks.get(id.0).and_then(Option::as_ref) else {
            return crate::err::ESRCH;
        };
        let (parent, leader_of) = (target.parent, target.pgid);
        if pgid.0 >= MAX_TASKS {
            return crate::err::EPERM;
        }
        // A caller may move itself, or a child that has not yet exec'd - and
        // the second is the case a shell actually uses.
        if id != t.current && parent != Some(t.current) {
            return crate::err::ESRCH;
        }
        // A group leader cannot leave the group it leads: the group is named
        // after it, and there would be nothing left to name.
        if leader_of == id && id != pgid {
            return crate::err::EPERM;
        }
        // The group must already exist - either because this task is about to
        // become its leader, or because somebody is in it.
        let exists = pgid == id || t.tasks.iter().flatten().any(|task| task.pgid == pgid);
        if !exists {
            return crate::err::EPERM;
        }
        t.task_mut(id).pgid = pgid;
        0
    })
}

/// What `id` would do if `sig` arrived now. `None` when the task is gone.
pub fn signal_action(id: TaskId, sig: u8) -> Option<crate::sched::signal::Action> {
    with_table(|t| {
        t.tasks
            .get(id.0)?
            .as_ref()
            .map(|task| task.dispositions.get(sig))
    })
}

/// Record `sig`'s disposition for `id`. `false` when the task is gone or has
/// already exited - a disposition nobody will ever read is not worth storing.
pub fn set_signal_action(id: TaskId, sig: u8, action: crate::sched::signal::Action) -> bool {
    with_table(
        |t| match t.tasks.get_mut(id.0).and_then(|slot| slot.as_mut()) {
            Some(task) if !task.exited => {
                task.dispositions.set(sig, action);
                true
            }
            _ => false,
        },
    )
}

/// The alternate signal stack `id` installed, if any. `None` both for a
/// process that never asked for one and for a task that no longer exists -
/// neither has a stack to report.
pub fn signal_alt_stack(id: TaskId) -> Option<crate::sched::signal::AltStack> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().and_then(|task| task.alt_stack))
}

/// Install (`Some`) or clear (`None`) `id`'s alternate signal stack. `false`
/// when the task is gone, which the syscall layer reports as `ESRCH`.
pub fn set_signal_alt_stack(id: TaskId, stack: Option<crate::sched::signal::AltStack>) -> bool {
    with_table(
        |t| match t.tasks.get_mut(id.0).and_then(|slot| slot.as_mut()) {
            Some(task) if !task.exited => {
                task.alt_stack = stack;
                true
            }
            _ => false,
        },
    )
}

/// Take one pending signal off the running task, skipping the ones it has
/// blocked: a blocked signal stays pending until it is unblocked, which is the
/// whole reason `rt_sigprocmask` exists.
///
/// Signals whose disposition is `SIG_IGN` are dropped here rather than
/// reported, since there is nothing anyone could do with them.
pub fn take_deliverable_signal() -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = crate::sched::signal::take(&mut ready) {
            if task.dispositions.get(sig).is_ignore() {
                continue;
            }
            task.signals &= !crate::sched::signal::bit(sig);
            return Some(sig);
        }
        None
    })
}

/// As [`take_deliverable_signal`], but also handing back the `siginfo_t` the
/// sender attached, so the delivery path can build a handler's frame with the
/// real record - `si_code`, `si_pid`, the `sigqueue` payload - rather than a
/// fixed `SI_USER`.
pub fn take_deliverable_siginfo() -> Option<(u8, crate::sched::signal::SigInfo)> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = crate::sched::signal::take(&mut ready) {
            if task.dispositions.get(sig).is_ignore() {
                continue;
            }
            let info = if crate::sched::signal::is_rt(sig) {
                match task.rtq.pop(sig) {
                    Some(info) => info,
                    None => continue,
                }
            } else {
                let slot = (sig as usize).min(crate::sched::signal::SIGINFO_SLOTS - 1);
                let info = task.siginfo[slot];
                task.siginfo[slot] = crate::sched::signal::SigInfo::ZERO;
                info
            };
            // The bit clears only when the last copy of this number has drained:
            // a real-time signal stays pending while the ring still holds one, so
            // every queued copy wakes a handler (and is not lost to a single clear).
            if !task.rtq.holds(sig) {
                task.signals &= !crate::sched::signal::bit(sig);
            }
            return Some((sig, info));
        }
        None
    })
}

/// Take one pending signal off the running task if it is in `mask`, whatever
/// its disposition says, and drop it from the pending set.
///
/// This is the `sigtimedwait` half of the signal machinery, and it is
/// deliberately different from [`take_deliverable_signal`]: a signal consumed
/// here never runs a handler and never takes its default action - not even for
/// `SIGKILL`, whose whole point is that it cannot be caught anywhere else. A
/// blocked signal in `mask` is exactly the one that can be waited for, which
/// is what makes `sigwait` work.
pub fn take_signal_in_set(
    mask: crate::sched::signal::Pending,
) -> Option<(u8, crate::sched::signal::SigInfo)> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & mask;
        match crate::sched::signal::take(&mut ready) {
            Some(sig) => {
                let record = if crate::sched::signal::is_rt(sig) {
                    match task.rtq.pop(sig) {
                        Some(r) => r,
                        None => return None,
                    }
                } else {
                    // The record comes out with the bit it belongs to: the
                    // `siginfo_t` a waiter reads has to be the one the sender
                    // left, not a fresh `SI_USER` one - musl's `SIGEV_THREAD`
                    // helper checks `si_code == SI_TIMER` before running the
                    // callback a POSIX timer exists to make.
                    let slot = (sig as usize).min(crate::sched::signal::SIGINFO_SLOTS - 1);
                    let r = task.siginfo[slot];
                    task.siginfo[slot] = crate::sched::signal::SigInfo::ZERO;
                    r
                };
                if !task.rtq.holds(sig) {
                    task.signals &= !crate::sched::signal::bit(sig);
                }
                Some((sig, record))
            }
            None => None,
        }
    })
}

/// Take a pending signal that has to end the task, wherever it happens to be.
///
/// This is what a blocked task can act on by itself: the default action for
/// every signal this kernel names is to die of it. A signal with a handler is
/// left pending - it needs the register frame on the way out of a syscall to
/// run on, and a task sitting in a blocking call has not reached one yet.
pub fn take_fatal_signal() -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = crate::sched::signal::take(&mut ready) {
            // Read from a copy, so a signal that is skipped stays pending.
            if task.dispositions.get(sig).is_handler() {
                continue;
            }
            // Of the signals with no handler, only the ones whose default
            // action is to terminate end a task sitting in a blocking call.
            // `SIGSTOP` is not one of them - it stops, and stopping a task
            // that is already asleep changes nothing - and `SIGCONT` is not
            // one of them either. Before there was a stop state to be in,
            // "no handler" and "dies of it" really were the same thing.
            if crate::sched::signal::default_action(sig)
                != crate::sched::signal::DefaultAction::Terminate
            {
                continue;
            }
            task.signals &= !crate::sched::signal::bit(sig);
            return Some(sig);
        }
        None
    })
}

/// What the running task would do about `sig`.
pub fn current_disposition(sig: u8) -> crate::sched::signal::Action {
    with_table(|t| t.task(t.current).dispositions.get(sig))
}

/// Forget a disposition, putting it back to `SIG_DFL`. `SA_RESETHAND` asks
/// for exactly this the moment a handler is entered, which is what keeps a
/// one-shot handler re-entrant-safe.
pub fn reset_disposition(sig: u8) {
    with_table(|t| {
        let id = t.current;
        let mut actions = t.task(id).dispositions;
        actions.set(sig, crate::sched::signal::Action::default());
        t.task_mut(id).dispositions = actions;
    });
}

/// The signals the running task has blocked.
pub fn current_blocked() -> crate::sched::signal::Pending {
    with_table(|t| t.task(t.current).blocked)
}

/// The signals the running task has waiting: the pending set as
/// `rt_sigpending(2)` reads it - a mask of what has arrived, not of what
/// is blocked. The two sets are independent: a signal can be both pending
/// and blocked, and this answer names it either way.
pub fn current_pending() -> crate::sched::signal::Pending {
    with_table(|t| t.task(t.current).signals)
}

/// Replace the running task's blocked set. `rt_sigprocmask` and the return
/// from a handler are its two callers.
pub fn set_current_blocked(mask: crate::sched::signal::Pending) {
    with_table(|t| {
        let id = t.current;
        t.task_mut(id).blocked = mask;
    });
}

// ---------------------------------------------------------------------------
// ptrace: the tracer link, the stops, and what a wait collects
// ---------------------------------------------------------------------------
//
// A tracer and its tracee are two tasks in a protocol, and the protocol has
// exactly two directions:
//
//   * the tracee stops - at a syscall boundary, on a signal, on `execve` - and
//     parks itself until something resumes it;
//   * the tracer sits in `wait(2)` until one of its children has something to
//     report, reads the stop, and resumes it.
//
// Both directions are ordinary task-table operations. The tracee parks in the
// same sleep queue every other blocking call uses, with a deadline of "never"
// and a `stop` record beside it; the tracer's `wait` blocks on the same
// condition mechanism, with the stop record as its condition. There is no new
// synchronisation primitive here, and that is the point: the whole of what makes
// this safe is the rule the rest of the module already follows - publish the
// state and park the waiter in one critical section, so a waiter that has not
// looked yet finds it on its next look.
