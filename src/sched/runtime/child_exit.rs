use super::INIT_PID;
use super::WaitEnd;
use super::WaitEvent;
use super::WaitPark;
use super::child_mm::Mm;
use super::child_mm::{populate_user_page_at, resolve_anon_fault, resolve_cow_fault};
use super::child_signal::{current_credentials, force_signal, queue_signal};
use super::load::{initial_stack_rsp, interpreter_bytes, load_user, write_arg_block};
use super::task::{
    RETIRED, RETIRED_CAP, Retired, SIGCHLD_PENDING, SpawnError, Stack, TIMER_SLOTS, Table, Task,
    Tracing, block_until_on_interruptible, current, new_layout, schedule, with_table,
};
use super::trace::{attach_new_child, fork_event, unreported_stop, waitable};
use crate::abi::PTRACE_EVENT_CLONE;
use crate::hal;
use crate::hal::{AddressSpace, ContextOps, UserFrame, UserFrameOps};
use crate::mm::elf::{self};
use crate::mm::vm::MapFlags;
use crate::mm::vma::VmaKind;
use crate::mm::{self};
use crate::sched::policy::TaskId;
use crate::sched::signal;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

fn user_byte_address(virt: usize, write: bool) -> Option<usize> {
    let page = crate::mm::pframe::align_down(virt, crate::mm::PAGE_SIZE);
    if write && crate::hal::is_cow_page(page) && !resolve_cow_fault(page) {
        return None;
    }
    let (phys, flags) = match crate::hal::translate(virt) {
        Some(found) => found,
        // A page of a mapping the process has not touched yet. The walk that
        // just failed is the same walk the CPU would do, so the answer is the
        // fault handler's: bring the page in and try once more. A miss with no
        // VMA behind it is still a refusal - that is a process handing the
        // kernel an address it never mapped.
        None => {
            if !resolve_anon_fault(virt, write) {
                return None;
            }
            crate::hal::translate(virt)?
        }
    };
    if !flags.contains(MapFlags::USER) {
        return None;
    }
    if write && !flags.contains(MapFlags::WRITE) {
        return None;
    }
    Some(phys)
}

/// Write `bytes` into the running process at `virt`, one page at a time.
///
/// Returns `false` if any part of the range is not a page the process could
/// write itself - which is what makes an unmapped stack a refused signal
/// rather than a kernel panic.
pub fn write_user(virt: usize, bytes: &[u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = user_byte_address(at, true) else {
            return false;
        };
        let dest =
            unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(phys) as *mut u8, chunk) };
        dest.copy_from_slice(&bytes[done..done + chunk]);
        done += chunk;
    }
    true
}

/// Read `bytes` out of the user address space `root` names.
///
/// The cross-process twin of [`read_user`], and the one `PTRACE_PEEKDATA` and
/// `process_vm_readv` are built on. It goes through the direct map rather than
/// through a user pointer, because the address belongs to a *different* process:
/// the kernel may not dereference it as its own, and the page tables that
/// describe it are not the ones CR3 holds. Every frame in the machine is
/// reachable at the HHDM offset, which is what makes "translate here, read
/// there" possible at all.
///
/// A write of a page that is still copy-on-write is deliberately *not*
/// broken here. `strace` only reads; a debugger poking a page a forked tracee
/// still shares would want the copy broken first, and doing that needs the
/// tracee's own `Mm` accounting rather than a physical address. Said plainly
/// rather than half-done.
///
/// The walk asks for `USER` - [`hal::user_phys_for_virt_of`], not the bare
/// translation - because the address belongs to somebody else: every space
/// carries the kernel's own mappings above the user half, and a walk without
/// that bit answers for them too. A same-task caller has
/// [`user_byte_address`] instead, which makes the same refusal, and the
/// user-pointer check the syscall layer runs on its own arguments cannot answer
/// for tables CR3 does not hold.
pub fn read_user_of(root: usize, virt: usize, bytes: &mut [u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = resolve_page_of(root, at, false, true) else {
            return false;
        };
        let src =
            unsafe { core::slice::from_raw_parts(mm::phys_to_virt(phys) as *const u8, chunk) };
        bytes[done..done + chunk].copy_from_slice(src);
        done += chunk;
    }
    true
}

/// Write `bytes` into the user address space `root` names. The writing half of
/// [`read_user_of`], and so the same notes apply: the `USER` bit is asked for,
/// and a copy-on-write page is not broken.
pub fn write_user_of(root: usize, virt: usize, bytes: &[u8]) -> bool {
    write_user_of_at(root, virt, bytes, true)
}

/// [`write_user_of`] with the demand-paging half made optional.
///
/// `populate` is `false` for the one caller that already holds the task table -
/// the `exec`/spawn path writes the argument block into a freshly built stack
/// while `with_table` is on the stack, and taking it again to look for a miss
/// would be a second `&mut Table`. That stack's pages are mapped eagerly by the
/// loader, so the miss this gives up on cannot happen; the flag keeps the
/// impossibility checked rather than assumed.
pub(crate) fn write_user_of_at(root: usize, virt: usize, bytes: &[u8], populate: bool) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = resolve_page_of(root, at, true, populate) else {
            return false;
        };
        let dest =
            unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(phys) as *mut u8, chunk) };
        dest.copy_from_slice(&bytes[done..done + chunk]);
        done += chunk;
    }
    true
}

/// The frame behind `virt` in the address space `root` names, bringing the page
/// in first when it is one of a mapping nobody has touched yet.
///
/// The demand-paging twin of the `user_phys_for_virt_of` walk, and the reason
/// the cross-process copy paths had to change at all: the walk reads page
/// tables, and a lazy page has no entry to read - so a missing page that a VMA
/// covers is populated rather than reported as `EFAULT`.
fn resolve_page_of(root: usize, virt: usize, write: bool, populate: bool) -> Option<usize> {
    match hal::user_phys_for_virt_of(root, virt) {
        Some(phys) => Some(phys),
        None if populate => {
            if !populate_user_page_at(root, virt, write) {
                return None;
            }
            hal::user_phys_for_virt_of(root, virt)
        }
        None => None,
    }
}

/// Read `bytes` from the running process at `virt`, one page at a time.
pub fn read_user(virt: usize, bytes: &mut [u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = user_byte_address(at, false) else {
            return false;
        };
        let src =
            unsafe { core::slice::from_raw_parts(mm::phys_to_virt(phys) as *const u8, chunk) };
        bytes[done..done + chunk].copy_from_slice(src);
        done += chunk;
    }
    true
}

/// End the running task with an exit code. Its slot and stack are freed by the
/// scheduler, so this never returns and nothing needs to be cleaned up by the
/// caller.
pub fn exit(code: isize) -> ! {
    schedule(|t| {
        let id = t.current;
        if id == INIT_PID {
            // The one process the system cannot lose. Init is what waits for
            // everything else; with it gone there is nothing left to run the
            // machine's programs, and nothing to hand orphans to. The kernel
            // has no second init to start, so it says so and stops.
            crate::println!("INIT_EXITED: the system has no init left");
            crate::hal::halt();
        }
        t.task_mut(id).exited = true;
        record_retired(id, code, t.task(id).trace.tracer);

        // Linux's `forget_original_parent`: a process that exits hands every
        // child that was still its own to init. Without this a zombie keeps a
        // `parent` naming a *slot* — and slots are reused, so the next process
        // to occupy this one inherits the dead process's children: `read_sigchld`
        // filters by `task.parent == Some(me)`, and `wait4`'s tables do the same
        // by number. Measured, once the sweep's program count grew: `t-signalfd`
        // was handed four records for one death, three of them naming slots 8
        // and 10 — unreaped orphans of an *earlier* occupant of its own slot —
        // and the read before its fork answered a record where the claim is
        // `EAGAIN`.
        //
        // Init is always alive (the kernel stops the machine rather than let it
        // exit, below), so it is the only candidate. Linux also gives a dead
        // child to its *tracer* when one is attached, and sends SIGHUP+SIGCONT
        // to an orphaned process group; neither is modelled here, and this walk
        // is where the first of them would go.
        for slot in t.tasks.iter_mut().flatten() {
            if slot.parent == Some(id) {
                slot.parent = Some(INIT_PID);
            }
        }

        // And a pid namespace's init takes its namespace with it - Linux's
        // `zap_pid_ns_processes`. The membership it tests is the part worth
        // stating, because the obvious reading of it is wrong: a task dies when
        // it is **numbered in** the dying namespace, which includes every
        // namespace nested beneath it, not merely a task whose own namespace
        // this is. Both halves measured, and the second is what makes the first
        // a question:
        //
        // * a task one namespace down died with its ancestor's init
        //   (`.workbuddy/probe_pid1_nested.c`), so this is `covers`;
        // * the task that *unshared* the namespace - whose own children would go
        //   into it, but which is itself outside - **survived**
        //   (`.workbuddy/probe_pid1_zap.c`), so this is deliberately *not*
        //   extended to `pid_ns_children`. Adding that clause would kill the
        //   shell of an `unshare -fp`, which Linux does not.
        //
        // Fired for the **last thread** of the group (Linux's `group_dead`), and
        // never for the machine's own init: that exit does not reach this line,
        // because the halt above is what it is. The kill is a **forced** one
        // (`force_signal`), which is the only way a nested namespace's own init
        // can be ended by a rule its own members cannot use against it.
        //
        // One divergence, named rather than papered over: Linux loops here until
        // the namespace is empty, because a task killed in the middle of a
        // `fork` can leave a child behind. This is one pass over the table.
        let ns = Arc::clone(&t.task(id).pid_ns);
        let tgid = t.task(id).tgid;
        let group_dead = !t
            .tasks
            .iter()
            .flatten()
            .any(|o| o.tgid == tgid && !o.exited);
        if group_dead && ns.is_init(tgid.0) {
            // And the namespace is **sealed** as it is emptied: its init is gone
            // and Linux lets nothing be created in it again (a `fork` by a task
            // whose children go there is `ENOMEM`, measured -
            // `.workbuddy/probe_fork_after_init.c`). Sealing here rather than at
            // the init's *reap* is what keeps a window from existing at all: the
            // same moment that ends the tasks is the one that closes the door.
            ns.seal();
            let mut killed = alloc::vec::Vec::new();
            for (slot, task) in t.tasks.iter_mut().enumerate() {
                let Some(task) = task.as_mut() else { continue };
                if task.exited || !ns.covers(&task.pid_ns) {
                    continue;
                }
                force_signal(task, signal::SIGKILL);
                killed.push(TaskId(slot));
            }
            // The wakes come after the walk: every target is collected first, so
            // the table is borrowed once - and a wake needs the scheduler, which
            // the walk is already holding.
            for victim in killed {
                t.sched.wake(victim);
            }
        }

        // Everything the task owned is given back *here*, at its exit, which
        // is Linux's own moment for it (`do_exit` closes the files and tears
        // down the `mm` before the parent is told anything). Waiting for the
        // reap instead - which is where this used to happen, and what the
        // function's own doc said was wrong - left a corpse holding its whole
        // address space and every descriptor it had open for as long as nobody
        // waited for it. An unreaped pid 1-less orphan then held the DRM
        // master and its seat session against the next compositor, and a
        // process with a large `mm` (a compositor's llvmpipe arenas, say) kept
        // every frame it had allocated. Only the slot and the exit code stay
        // now, which is what a zombie is.
        t.release_exited();

        // Whoever was waiting on this task as its parent is woken here, in
        // the same critical section that made the condition true: either it
        // has not slept yet and will find the code on its next look, or it is
        // in the sleep queue this wake empties. There is no third state.
        let parent = t.task(id).parent;
        if let Some(parent) = parent {
            t.sched.wake(parent);
            // The real SIGCHLD, not just the wake. A parent sitting in
            // `wait(2)` needs only the wake - the code it waits for is on
            // file here - but a parent waiting any other way (a futex, a
            // handler of its own, `sigtimedwait`) is ended by the signal, and
            // without it such a caller sleeps for ever: musl's waitpid blocks
            // on a futex and lets its SIGCHLD handler do the waking.
            //
            // Queued when a handler would run, *and* when the parent has
            // SIGCHLD blocked - Linux's `sig_ignored()` discards a generated
            // signal only if it is unblocked and ignored, and says outright
            // that a blocked signal is never ignored. The second case is what
            // a blocked `sigtimedwait`/`sigwait`/`signalfd` on SIGCHLD needs;
            // without it a handler-less waiter hangs. Measured on Linux
            // 7.2.8: a process with no handler that blocks SIGCHLD, forks,
            // and calls `sigtimedwait` gets the signal back (`rc=17`,
            // `si_code=1`, `si_pid` the child), where the handler-only rule
            // used to leave it asleep for ever.
            let sigchld_bit = signal::bit(signal::SIGCHLD);
            let wants_sigchld = t
                .task(parent)
                .dispositions
                .get(signal::SIGCHLD)
                .is_handler()
                || t.task(parent).blocked & sigchld_bit != 0;
            if wants_sigchld {
                // The child as the record names it: the number it has *in its
                // own namespace* - Linux's `task_tgid_vnr(tsk)`. A parent in
                // the same namespace reads back the number `getpid` gave the
                // child; a parent one level out reads the child's own number,
                // which is Linux's wart for a child in a deeper namespace than
                // its reader.
                let child_pid = {
                    let child = t.task(id);
                    child
                        .pid_ns
                        .number_of(child.tgid.0)
                        .unwrap_or(child.tgid.0 as u32) as i32
                };
                queue_signal(
                    t.task_mut(parent),
                    signal::SIGCHLD,
                    signal::SigInfo::user(signal::SIGCHLD, child_pid, 0),
                );
            }
            // The console goes back to the parent's group as the child leaves
            // it: a shell waits, gets its prompt back, and reads again. A
            // job-controlled shell has already taken the terminal back with
            // `TIOCSPGRP` by then - this is the same answer arrived at without
            // it, for the programs that never ask.
            let tty = crate::vfs::tty::global();
            let left = t.task(id).pgid;
            if tty.foreground() == Some(left) {
                tty.set_foreground(Some(t.task(parent).pgid));
            }
        }

        // Its children outlive it, and a dead task cannot be anyone's parent -
        // nor wait for anyone. They go to init, which waits for everything.
        // Init itself is exempt: it has no parent to lose.
        if id != INIT_PID {
            let orphans: Vec<TaskId> = t
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(slot, task)| {
                    let task = task.as_ref()?;
                    (task.parent == Some(id) && !task.exited).then_some(TaskId(slot))
                })
                .collect();
            let adopted = !orphans.is_empty();
            for orphan in orphans {
                t.task_mut(orphan).parent = Some(INIT_PID);
            }
            if adopted {
                t.sched.wake(INIT_PID);
            }
        }

        // Anything this task was tracing is let go. A tracee whose tracer has
        // died has nobody left to resume it, and a task stopped with nobody to
        // resume it sits there for the machine's life - a worse outcome than an
        // unfinished trace, which is all that running on gives up.
        let tracees: Vec<TaskId> = t
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(slot, task)| {
                let task = task.as_ref()?;
                (task.trace.tracer == Some(id) && !task.exited).then_some(TaskId(slot))
            })
            .collect();
        for tracee in tracees {
            let task = t.task_mut(tracee);
            task.trace.tracer = None;
            task.trace.armed = false;
            task.trace.stop = None;
            task.trace.regs = None;
            t.sched.wake(tracee);
        }
        // Deaths the tracer never got to collect are released with it. A
        // record still owned by a dead tracer is uncollectable for ever - the
        // real parent's `wait` would pass over it every tick while the corpse
        // it names is already gone.
        critical_section::with(|cs| {
            for retired in RETIRED.borrow(cs).borrow_mut().iter_mut() {
                if retired.tracer == Some(id) {
                    retired.tracer = None;
                }
            }
        });
    });
    unreachable!("an exited task is never picked again")
}

/// The exit code of the most recent task with this id to have exited, if any.
///
/// Recorded at exit, not at reap: the reader may come a tick later, after the
/// slot has already been freed and handed to someone else.
///
/// This is the kernel's look; it takes nothing, so a milestone can read the
/// same code twice. A process collecting its child's status uses
/// [`wait_any_child`], which consumes the record, and [`take_exit_code`] for
/// the pid it named.
pub fn exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| retired.id == id)
            .map(|retired| retired.code)
    })
}

/// Collect `id`'s exit code for caller `me`, consuming the record. `None` when
/// no death has been recorded for that pid since its slot was last reused, or
/// when the record is still owned by a tracer other than `me`.
///
/// A record owned by `me` as its tracer is *released*, not removed: the code
/// stays on file, now visible to the real parent, whose own `wait` collects it
/// and removes the record for good. That is the tracer-first order the record
/// exists to keep - unless `me` is the real parent *as well*, the
/// `fork` + `PTRACE_TRACEME` shape every strace run has: there is no other
/// parent to release to, and leaving the record in place would report the same
/// death a second time to the same waiter, whose tcb is already gone. It comes
/// out instead.
pub(crate) fn take_exit_code_for(t: &Table, id: TaskId, me: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|retired| retired.id == id)?;
        if retired[at].tracer.is_some_and(|tracer| tracer != me) {
            return None;
        }
        let also_parent = retired[at].tracer == Some(me)
            && t.tasks[id.0]
                .as_ref()
                .is_none_or(|task| task.parent == Some(me));
        if also_parent {
            SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
            return Some(retired.remove(at).code);
        }
        if retired[at].tracer == Some(me) {
            retired[at].tracer = None;
            return Some(retired[at].code);
        }
        // The code is collected - so the signal about it can never be read
        // again, and a stale entry must not name a slot a new task owns.
        SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
        Some(retired.remove(at).code)
    })
}

/// Collect `id`'s exit code regardless of tracer ownership. The boot task's
/// use for init, whose death nobody reaps, and nothing else.
pub fn take_exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|retired| retired.id == id)?;
        SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
        Some(retired.remove(at).code)
    })
}

/// Block the caller until one of its children exits, then take that child's
/// pid and code. A child that is already gone is reaped from the records
/// without blocking; with none gone, the caller sleeps and is woken by
/// [`exit`]'s parent wake.
///
/// The caller must be a task: this is `wait`'s core, and nothing else calls
/// it.
/// Whether a child of the running task has exited and not yet been
/// collected - the question a `signalfd` for `SIGCHLD` answers. Nothing is
/// consumed: `wait(2)` is what collects.
/// Whether a child of the running task has an unread `SIGCHLD` - the question
/// a `signalfd` answers when polled. Nothing is consumed here.
pub fn child_exited_unreaped() -> Option<usize> {
    let me = current();
    with_table(|t| {
        critical_section::with(|cs| {
            SIGCHLD_PENDING
                .borrow(cs)
                .borrow()
                .iter()
                .rev()
                .find_map(|id| {
                    // The named child must still be ours: a slot a new task
                    // took over describes nobody's child any more.
                    t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| task.parent == Some(me))
                        .then(|| id.0)
                })
        })
    })
}

/// Read one pending `SIGCHLD` of the running task, as a `signalfd` read does:
/// the signal goes to the caller and is no longer pending, while the exit
/// code itself stays on file for `wait(2)` to collect. `None` when no child
/// of this task has an unread signal.
pub fn read_sigchld() -> Option<usize> {
    let me = current();
    with_table(|t| {
        critical_section::with(|cs| {
            let mut pending = SIGCHLD_PENDING.borrow(cs).borrow_mut();
            let at = pending.iter().rposition(|id| {
                t.tasks[id.0]
                    .as_ref()
                    .is_some_and(|task| task.parent == Some(me))
            })?;
            let id = pending.remove(at);
            Some(id.0)
        })
    })
}

pub fn wait_any_child(parent: TaskId) -> WaitPark<(TaskId, isize)> {
    // The condition is the table itself, so it is asked through the borrow
    // `block_until_on_interruptible` already holds rather than taking one of its own.
    if matches!(
        block_until_on_interruptible(|t| any_child_code(t, parent).is_some()),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    // Taken under the same borrow, not looked up again: a second lookup by pid
    // would race the slot being handed to a new task, and the record would go
    // to it instead.
    match with_table(|t| {
        any_child_code(t, parent)
            .and_then(|(id, _)| take_exit_code_for(t, id, parent).map(|code| (id, code)))
    }) {
        Some(found) => WaitPark::Event(found),
        None => WaitPark::NoChild,
    }
}

/// The pid and code of the oldest of `parent`'s children to have exited and
/// not yet been collected, written as a `NO_CODE` marker when the caller only
/// wants to know that a child died.
///
/// Nothing is consumed here: `wait_any_child` holds the table while it looks,
/// and the record is taken afterwards, by pid, in [`take_exit_code`]. A
/// read-only look keeps this function usable from the `block_until` closure,
/// which must not wake anyone.
pub(crate) fn any_child_code(t: &Table, parent: TaskId) -> Option<(TaskId, isize)> {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| {
                // The slot is spoken for by a live task: the record describes
                // an older tenant of the same number, so it is not this
                // parent's business.
                let id = retired.id;
                let free = t.tasks[id.0].is_none();
                let ours = free
                    || t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| waitable(task, parent));
                ours && id != parent && retired.tracer.is_none_or(|tracer| tracer == parent)
            })
            .map(|retired| (retired.id, retired.code))
    })
}

/// Give up on a record nobody will ever collect, so it cannot be mistaken for
/// a fresh death by the next `wait`. Used by the boot task for init, which it
/// hands to the system and never reaps.
pub fn discard_exit_code(id: TaskId) {
    take_exit_code(id);
}

fn record_retired(id: TaskId, code: isize, tracer: Option<TaskId>) {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        if retired.len() == RETIRED_CAP {
            let dropped = retired.remove(0);
            // A code nobody will collect has no signal left to read either.
            SIGCHLD_PENDING
                .borrow(cs)
                .borrow_mut()
                .retain(|p| *p != dropped.id);
        }
        retired.push(Retired { id, code, tracer });
        // The signal the exit carries: pending until a signalfd reader takes
        // it, independent of who eventually collects the code.
        let mut pending = SIGCHLD_PENDING.borrow(cs).borrow_mut();
        if !pending.contains(&id) {
            pending.push(id);
        }
    });
}

/// Whether a code is still waiting to be collected for `id`.
pub(crate) fn exit_code_pending(id: TaskId) -> bool {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .any(|retired| retired.id == id)
    })
}

/// Forget every retired code recorded for `id`.
///
/// Called the moment a slot is handed to a new task. The records describe
/// whoever held the slot before it was freed, and a `wait` on the new task
/// must never see them - an old death would otherwise pass for the new
/// task's exit before it has even run.
pub(crate) fn drop_retired(id: TaskId) {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow_mut()
            .retain(|retired| retired.id != id);
    });
}

/// The running task.
pub fn fork(state: &UserFrame) -> isize {
    spawn_copy(state, CloneRequest::default())
}

/// `vfork(2)`: the same copy of the calling process, labelled for the tracer.
///
/// Linux's `vfork` is `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, and that flag is
/// what makes the event a tracer is waiting for `PTRACE_EVENT_VFORK` rather
/// than `PTRACE_EVENT_FORK`. The sharing and the suspended parent are the parts
/// this kernel does not reproduce - see [`clone`] for why a copy meets the
/// contract better here than a share would - but which of the two events a
/// tracer is waiting for is not something it can recover for itself.
pub fn vfork(state: &UserFrame) -> isize {
    spawn_copy(
        state,
        CloneRequest {
            flags: crate::abi::CLONE_VFORK,
            ..CloneRequest::default()
        },
    )
}

/// What a `clone` asks for beyond "a copy of me": which flags, where the child's
/// stack is, where the child should record its own tid, and what TLS it runs
/// with.
///
/// A default request is a plain `fork`: no flags, the caller's own stack, no tid
/// write, the caller's TLS.
#[derive(Clone, Copy, Default)]
pub struct CloneRequest {
    pub flags: usize,
    pub child_stack: usize,
    pub child_tid: usize,
    pub tls: usize,
    /// The network namespace to give the child, or `None` to let it inherit its
    /// creator's.
    ///
    /// Filled in by the *syscall* layer, and that is not an accident of layering:
    /// making a namespace means calling `net::mint_namespace`, and `sched` must
    /// not name `net` (it would close a mutual dependency - see
    /// [`super::task::Task::net_ns`]). The syscall layer already decides what a
    /// flag word means, so it decides this too and hands the scheduler an
    /// already-made namespace.
    pub net_ns: Option<u64>,
}

/// The thread-local storage base the **task on this CPU** is running with.
///
/// This is not `Task::tls`. That record is written when a task is dispatched and
/// read back out of the register when it leaves, because a program can move its
/// own base with no system call for this kernel to see: aarch64's musl installs
/// its TLS with a direct `msr tpidr_el0`. Between those two points the record is
/// stale - and a `fork`/`clone` child inherits the base its parent is *running*
/// with, not the last one this kernel wrote down: Linux's `copy_thread` hands the
/// child the parent's live `tp_value` when `CLONE_SETTLS` is absent, and this is
/// that value. Measured, on the aarch64 sweep's first spawn: a child built from
/// the stale record (the parent had set its own base after its last dispatch, so
/// the record still said 0) ran with `TPIDR_EL0 = 0` and faulted writing musl's
/// `errno` at `0xffffffffffffff5c` (NULL minus 164).
///
/// Only ever call this for `current()`. The register belongs to the CPU, not to
/// `task`: naming any other task here would hand back the running one's base
/// under a name it does not belong to. A kernel task has no base of its own, so
/// there the record - zero - is the truth about it.
fn running_tls(task: &Task) -> usize {
    if task.space.is_some() || task.mm.root() != 0 {
        hal::thread_pointer()
    } else {
        task.tls
    }
}

/// Create a process that is a copy of the calling one, resuming from the very
/// call that made it.
///
/// This is the shared heart of `fork` and `clone`: `fork` is this with a default
/// request, and `clone` is this with the caller's flags filled in.
///
/// The address space is duplicated copy-on-write: both processes start out
/// sharing every frame with the writable pages marked read-only and a COW bit
/// set. The first write on either side faults, allocates a private frame, copies
/// the page, and restores write permission - so each process ends up with its own
/// copy at the moment it actually diverges, and never before.
///
/// The descriptor table is *copied*, not shared: the child names the same open
/// files under the same numbers, which is what a shell's `dup2`-then-`execve`
/// after a `fork` needs - and what it needs is for the parent's own `close` to
/// stay on the parent's side of the fork.
fn spawn_copy(state: &UserFrame, request: CloneRequest) -> isize {
    // Phase 1: Collect all parent state under one immutable borrow, then
    // release it before any mutations.
    let (
        parent_id,
        parent_has_space,
        parent_priority,
        parent_fds,
        parent_fs,
        parent_exe,
        parent_argv,
        parent_env,
        parent_brk_base,
        parent_brk_pages,
        parent_mmap_cursor,
        parent_tls,
        parent_pages,
        parent_vmas,
        parent_eager,
    ) = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        let parent_has_space = parent.space.is_some();
        let parent_priority = parent.priority;
        let parent_fds = Arc::clone(&parent.fds);
        let parent_fs = Arc::clone(&parent.fs);
        let parent_exe = parent.exe.clone();
        let parent_argv = alloc::sync::Arc::clone(&parent.argv);
        let parent_env = alloc::sync::Arc::clone(&parent.env);
        let parent_brk_base = parent.brk_base;
        let parent_brk_pages = parent.brk_pages;
        let parent_mmap_cursor = parent.mmap_cursor;
        // The parent's *live* base, not its record: see `running_tls`.
        let parent_tls = running_tls(parent);

        // Collect the parent's user page mappings. We need to do this while
        // holding the borrow, then release it before calling into AddressSpace.
        let mut parent_pages = Vec::new();
        if let Some(space) = &parent.space {
            space.walk_user_pages(|virt, phys, flags| {
                parent_pages.push((virt, phys, flags));
            });
        }
        // The parent's VMAs come across whole. The child's address space is the
        // parent's - the same ranges with the same permissions and the same
        // backing - so a mapping the parent had not touched is one the child has
        // not touched either, and costs nothing to duplicate. That is what makes
        // a `fork` of a process with a large untouched `mmap` cheap: only the
        // *present* pages below are walked at all.
        let parent_vmas = parent.mm.vma_snapshot();
        // And so does what the parent was *loaded* with: a child's address
        // space is its parent's, so `/proc/<pid>/maps` says the same thing on
        // either side of the fork.
        let parent_eager = parent.mm.eager_ranges();
        (
            parent_id,
            parent_has_space,
            parent_priority,
            parent_fds,
            parent_fs,
            parent_exe,
            parent_argv,
            parent_env,
            parent_brk_base,
            parent_brk_pages,
            parent_mmap_cursor,
            parent_tls,
            parent_pages,
            parent_vmas,
            parent_eager,
        )
    });

    // A task with no address space has no image to copy: `fork` from a kernel
    // task is meaningless, and Linux answers ENOMEM for a process it cannot
    // duplicate.
    if !parent_has_space {
        return crate::err::ENOMEM;
    }

    // Two refusals about the pid namespace this child would be born into, both
    // measured, and both asked *before* a slot or a namespace is made - Linux
    // builds the whole new `nsproxy` and swaps it in one step, so a refusal that
    // arrived half-way through would leave a state nobody asked for.
    //
    // * `CLONE_NEWPID` has the same precondition `unshare(CLONE_NEWPID)` has -
    //   `sched::may_mint_pid_ns`'s, which is one rule with two call sites: a
    //   process makes a pid namespace **once**, and a `clone` with the flag
    //   after one is `EINVAL`.
    // * and the namespace the child is born into must not be **sealed**: a
    //   namespace whose init has exited is `ENOMEM` to create a task in, which is
    //   the state a `fork` by the task that *unshared* it finds itself in once
    //   that namespace's init has died.
    let refused = with_table(|t| {
        let parent = t.task(t.current);
        if request.flags & crate::abi::CLONE_NEWPID != 0 && !super::pid::is_mintable(parent) {
            return crate::err::EINVAL;
        }
        if parent.pid_ns_children.is_sealed() {
            return crate::err::ENOMEM;
        }
        0
    });
    if refused != 0 {
        return refused;
    }

    // Phase 2: Allocate a slot and build the child state.
    let child_id = with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        // Which namespace this child is born into - and, under
        // `CLONE_NEWPID`, the namespace it *is*: Linux's `copy_process` puts
        // the child in its creator's `pid_ns_for_children`, and the flag hands
        // the child a fresh namespace of its own to be the init of. Either way
        // the child is registered in that namespace and every one above it,
        // which is what lets this parent `wait` for it and find it in `/proc`
        // under a number of its own.
        let child_pid_ns = if request.flags & crate::abi::CLONE_NEWPID != 0 {
            crate::pidns::PidNs::child(&t.task(parent_id).pid_ns_children, crate::vfs::ns::mint())
        } else {
            Arc::clone(&t.task(parent_id).pid_ns_children)
        };
        super::adopt_pid(TaskId(slot), &child_pid_ns);

        let stack = Stack::new();
        // Interrupts stay off in the child until its `iretq` restores the
        // parent's RFLAGS: a fork child resumes inside a syscall gate, and the
        // gate runs with IF clear. Enabling them any earlier would let the clock
        // fire on a task that has not reached user mode yet.
        let mut context = unsafe { hal::bootstrap(stack.top(), task_entry, slot, false) };
        // The child's floating-point registers are its creator's: they are part
        // of what `fork` and a thread-creating `clone` copy, and `bootstrap`
        // would otherwise hand over the architectural reset state. `inherit_fpu`
        // reads them out of the parent - from the registers while they are still
        // the parent's, from the parent's saved block when they are no longer.
        *context.fpu_mut() = t.inherit_fpu(parent_id);

        // The child's address space is not a copy of every page: a
        // *frame-backed* range - a memfd, a dma-buf, the framebuffer, or the
        // unnamed object behind `MAP_SHARED|MAP_ANONYMOUS` - is an object both
        // processes are supposed to watch through, so its present pages cross
        // the fork mapped writable and marked nothing. The ranges are collected
        // here, alongside the pages, so the two loops below agree about which
        // pages are which.
        let shared: Vec<(usize, usize)> = parent_vmas
            .iter()
            .filter(|vma| matches!(vma.kind, VmaKind::Frames { .. }))
            .map(|vma| (vma.start, vma.end))
            .collect();

        // Create the child's address space by sharing all parent pages COW.
        let mut child_space = AddressSpace::new();
        let mut child_frames = Vec::new();
        let child_root = child_space.root();

        for &(virt, phys, flags) in &parent_pages {
            // Two address spaces now own this frame; the count keeps it alive
            // until the last of them lets go.
            crate::mm::clone_frame_ref(phys);
            child_frames.push(phys);
            // A page of a shared object is mapped as itself, at the permissions
            // it already has. A page that is *already* copy-on-write is an
            // exception, and deliberately: an older `fork` marked it, and the
            // frame it would otherwise share is one that flag already governs.
            if !flags.contains(MapFlags::COW) && shared.iter().any(|&(s, e)| s <= virt && virt < e)
            {
                child_space.map_shared(virt, phys, flags);
            } else {
                // Map it in the child as COW: read-only for now, with the COW
                // bit marking it for the fault handler.
                child_space.map_cow(virt, phys, flags);
            }
        }

        // Make the parent's writable pages COW too, or the child's copy would be
        // the only read-only one and the parent would write straight into a frame
        // the child is also reading. Re-borrow: the phase-1 borrow is long gone.
        //
        // A page the walk reported as already COW is left alone: it is shared
        // with an older sibling and is protected already. Re-protecting it is
        // harmless, but the walk reported it without `WRITE`, so a test for
        // writability alone would skip it and leave the *parent* writable while
        // the child's copy stays read-only - which is the bug this whole dance
        // exists to avoid.
        //
        // One flush for the whole loop, at the end: the PTE write is what makes
        // the page read-only, but the CPU keeps using the translations it
        // already has until the TLB is dropped, and doing that per page asked
        // the machine to re-walk the parent's entire address space once for
        // every writable page it owns - the single largest cost in a fork of a
        // large process. Nothing runs on this address space between the first
        // `set_cow_bit_quietly` and the flush, because this *is* the task that
        // owns it, inside its own syscall.
        if let Some(parent_space) = &mut t.task_mut(parent_id).space {
            for &(virt, _phys, flags) in &parent_pages {
                if flags.contains(MapFlags::COW) || !flags.contains(MapFlags::WRITE) {
                    continue;
                }
                // A page of a shared object stays writable in the parent too.
                // Hardening it would not protect anything - the child's copy is
                // deliberately writable - and it would make the parent's *next*
                // write take a private copy of a frame the child is watching,
                // which is precisely the sharing this exists to keep.
                if shared.iter().any(|&(s, e)| s <= virt && virt < e) {
                    continue;
                }
                // Everything the page had except write permission, in one PTE
                // write: hardening it to `READ | USER` would silently take away
                // execute on a page that is both, and a COW copy made from it
                // later would stay non-executable forever.
                parent_space.set_cow_bit_quietly(virt);
            }
            parent_space.flush_if_active();
        }

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority: parent_priority,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(child_space),
            user: true,
            // The child does not start at the entry point: it returns from the
            // `fork` that made it, so it carries a whole register file instead.
            user_entry: None,
            // The child does not start at the entry point: it returns from the
            // `fork` that made it, so it carries a whole register file instead.
            // The stack is the caller's unless `clone` named another.
            user_resume: Some(state.forked(request.child_stack)),
            mm: {
                // A child's address space is its parent's, so what the parent
                // was *loaded* with is the child's report too - `/proc` on
                // either says the same thing.
                let mm = Mm::with_eager(TaskId(slot), child_root, child_frames, parent_eager);
                mm.set_vmas(parent_vmas);
                // The credentials come along, as Linux's `copy_creds` shares
                // them: a process that had dropped a bounding capability or
                // become non-dumpable must not hand its child the full set.
                mm.set_credentials(t.task(parent_id).mm.credentials());
                mm
            },
            released: false,
            tgid: TaskId(slot),
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            // The mask is inherited, as POSIX says a child's is: a program
            // that blocks a signal and then forks expects the child to start
            // with it blocked too. The *dispositions* above are deliberately
            // not inherited (this kernel starts every process at `SIG_DFL`),
            // but a mask is not a disposition - it is the caller saying "not
            // now", and a child that un-blocked what its parent held would
            // take a signal the parent meant to handle itself.
            blocked: t.task(parent_id).blocked,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            // The descriptors are the parent's *numbering* copied - both
            // processes share the open file descriptions but closing a number
            // on one side must not close it on the other - unless the caller
            // asked to keep sharing them, which is `CLONE_FILES` and what a
            // thread gets. Linux's `copy_files`/`copy_fd`.
            fds: if request.flags & crate::abi::CLONE_FILES != 0 {
                Arc::clone(&parent_fds)
            } else {
                Arc::new(parent_fds.duplicate())
            },
            // `CLONE_FS` keeps the process's filesystem context, so a thread
            // that `chdir`s moves its process; without it the child gets a copy
            // of its own, which is a plain `fork` and every `clone` that did not
            // name the flag. Linux's `copy_fs_struct` on exactly that bit.
            fs: if request.flags & crate::abi::CLONE_FS != 0 {
                Arc::clone(&parent_fs)
            } else {
                parent_fs.copy()
            },
            // The same mount table as its parent, by reference — see the `uts`
            // line below: a copy here would give `fork` the effect `unshare` is
            // for. A child that wants its own namespace asks for it explicitly.
            // `clone(CLONE_NEWNS)`: the child starts in a *copy* of the
            // parent's table (Linux's `copy_mnt_ns` on exactly this flag), so
            // what it mounts or peels never reaches the namespace it came from
            // — and the copy is of what the parent holds *now*, including any
            // namespace the parent itself unshared, which is the composition
            // Linux has. Without the flag the reference is handed on, as every
            // fork since the beginning did. Pinned by `t-ns-mnt`. A new group
            // needs no nsproxy for this: the child *is* a fresh group, so the
            // fresh `Arc` at creation is the whole mechanism — which is why MNT
            // lands here while CLONE_FS-style separation stays parked with the
            // shared-group handle.
            fs_ns: if request.flags & crate::abi::CLONE_NEWNS != 0 {
                match &t.task(parent_id).fs_ns {
                    Some(table) => Some(table.copy()),
                    // The machine's table copied: a child of an unshared
                    // parent gets its own private view of the same world.
                    None => Some(crate::vfs::mount::machine_table().copy()),
                }
            } else {
                t.task(parent_id).fs_ns.clone()
            },
            parent: Some(parent_id),
            pgid: t.task(parent_id).pgid,
            // A fork child is in the same session as its parent and holds the
            // same controlling terminal - the identity `/dev/tty` opens for
            // it, whatever its parent was doing when it copied itself.
            session: t.task(parent_id).session,
            ctty: t.task(parent_id).ctty,
            identity: t.task(parent_id).identity.clone(),
            // A fork child is in its parent's namespace, by reference: the two
            // go on reporting one set of names until one of them unshares. A
            // copy here would give a `fork` the effect `unshare` is for.
            // `clone(CLONE_NEWUTS)`: the child starts in a *copy* of its
            // parent's names (Linux's `copy_utsname` on exactly this flag), so
            // it can rename without the parent following — and the copy is of
            // what the parent holds *now*, including any namespace the parent
            // itself unshared, which is the composition Linux has. Without the
            // flag the reference is handed on, as every fork since the
            // beginning did. Pinned by `t-clone`.
            uts: if request.flags & crate::abi::CLONE_NEWUTS != 0 {
                t.task(parent_id).uts.copied(crate::vfs::ns::mint())
            } else {
                Arc::clone(&t.task(parent_id).uts)
            },
            // Born in the namespace computed above, and its own children will
            // be born in the same one until it unshares - so both fields start
            // out equal, which is what makes an ordinary `fork` a no-op here
            // and `CLONE_NEWPID` the only thing that moves either.
            pid_ns: Arc::clone(&child_pid_ns),
            pid_ns_children: child_pid_ns,
            // The network namespace: the creator's, unless this word asked for a
            // fresh one (`CLONE_NEWNET`, minted by the syscall layer, which is the
            // layer allowed to name `net`).
            net_ns: request.net_ns.or(t.task(parent_id).net_ns),
            oom_score_adj: t.task(parent_id).oom_score_adj,
            oom_score_adj_min: t.task(parent_id).oom_score_adj_min,
            nice: t.task(parent_id).nice,
            rlimits: t.task(parent_id).rlimits,
            seccomp_strict: t.task(parent_id).seccomp_strict,
            seccomp_filters: t.task(parent_id).seccomp_filters.clone(),
            brk_base: parent_brk_base,
            brk_pages: parent_brk_pages,
            mmap_cursor: parent_mmap_cursor,
            // The child's memory is its parent's, addresses included: a stack
            // that started somewhere else would leave the copied page tables
            // describing a region the child does not own.
            stack_top: t.task(parent_id).stack_top,
            exe: parent_exe,
            argv: parent_argv,
            // The child inherits the parent's environment, one reference
            // rather than a copy, exactly as it inherits `argv`: the two are
            // one block on the stack the child shares, and a `fork` that
            // cloned them would be copying what the next `execve` throws away.
            env: parent_env,
            // `CLONE_CHILD_SETTID` parks the child's tid where the child can
            // read it; without the flag there is nothing to record.
            child_tid: if request.flags & crate::abi::CLONE_CHILD_SETTID != 0 {
                request.child_tid
            } else {
                0
            },
            // `CLONE_SETTLS` names a TLS descriptor for the child alone;
            // otherwise the child inherits the caller's.
            tls: if request.flags & crate::abi::CLONE_SETTLS != 0 {
                request.tls
            } else {
                parent_tls
            },
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });

        let child_id = TaskId(slot);
        // A child born into a tracer's hands does not go on the ready queue: it
        // is stopped, and letting it run would be the one thing that stop exists
        // to prevent. Everything else about it is already built - `user_resume`
        // holds the register file it will come back from the call with - so the
        // resume is what puts it on the queue, exactly as it is for a tracee
        // stopped anywhere else.
        // A traced child is born stopped (its first syscall is a trace event the
        // tracer must see); an untraced one goes straight ready. This asks the
        // question on every machine - a machine whose syscall handler has no
        // `PTRACE_ATTACH` to write the link has no tracer, so the answer is a
        // plain "no" and the child simply starts ready. That is not a faked
        // ptrace: there is nothing on that machine to fake one for.
        let traced_child = attach_new_child(t, parent_id, child_id, fork_event(request.flags));
        if traced_child {
            t.sched.sleep(child_id, parent_priority, u64::MAX);
        } else {
            t.sched.push_ready(child_id, parent_priority);
        }
        t.sched.wake(parent_id);
        child_id
    });

    child_id.0 as isize
}

/// Replace the current process's image with a new ELF binary. Tears down the
/// old address space, loads the new ELF segments, resets process state, and
/// returns `(entry_point, user_rsp)` so the syscall stub can patch its return
/// frame and enter the new image on `sysretq`.
///
/// This does **not** modify the register frame itself — the caller (syscall
/// dispatch) does that.
pub fn exec_replace(
    bytes: &[u8],
    argv: &[String],
    env: &[String],
    exe: &str,
) -> Result<(usize, usize), SpawnError> {
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // A dynamically linked program names the interpreter that has to run
    // before it does. That is a path into the filesystem, and this is the only
    // place with both the path and the authority to follow it - which is why
    // the interpreter is read here rather than inside the loader.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    // A fresh layout with the fresh image. This is the moment ASLR belongs to:
    // a `fork` copies the parent's addresses because it copies its memory, but
    // an `exec` throws that memory away, so the new program is under no
    // obligation - and gets no benefit - from starting where the old one did.
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, exe, layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);

        // --- Tear down old address space ---
        // Drop the old AddressSpace (frees lower-half page tables).
        if let Some(old_space) = task.space.take() {
            drop(old_space);
        }
        // Give every page the old image owned back. `free_frames` does the
        // reference-count half itself, so a page a `fork` shared is only
        // returned to the allocator by whichever process drops the last claim.
        for frame in task.mm.take_frames() {
            crate::mm::free_frames(frame, 0);
        }
        // The task is about to be a *new* program: it is no longer a fork child
        // waiting to resume its parent's call, and a fresh image starts with no
        // TLS and no tid to record.
        task.user_resume = None;
        task.child_tid = 0;
        task.tls = 0;
        hal::set_thread_pointer(0);
        // And a fresh image starts with the floating-point registers reset, for
        // the reason POSIX says so and Linux does the same: the discarded image
        // must not be readable out of them, and its rounding mode must not leak
        // into the next program's arithmetic. Both copies are reset - the live
        // registers, because they are what the new program runs with until the
        // next switch, and the task's saved block, because that is what a switch
        // to it would restore.
        //
        // The claim comes first: `fninit` is a floating-point instruction, and
        // under the lazy scheme a task that never touched the FPU is running
        // with `CR0.TS` set, so resetting without claiming would trap here - in
        // the kernel, with the table lock held. Claiming is also the honest
        // record of what follows: once the reset block is in the live registers
        // this task genuinely owns them, and saying so saves the new image a
        // pointless fault in its first `memcpy`.
        #[cfg(mpos_fpu_trap)]
        {
            unsafe { hal::claim_live_fpu() };
            unsafe { hal::reset_live_fpu() };
        }
        *task.context.fpu_mut() = hal::Fpu::reset();
        #[cfg(mpos_fpu_trap)]
        {
            task.fpu_live = true;
        }

        // --- Build new address space ---
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            exe,
            interp_name,
        );

        write_arg_block(space.root(), user_rsp, argv, env, exe, &loaded);

        // --- Reset process state ---
        //
        // The new root is recorded in the task *before* the CPU is pointed at
        // it, and that order is load-bearing. A clock tick landing between the
        // two would send the dispatcher to `prepare`, which reads the task's
        // space to decide what to load - and a task still holding the address
        // space it has just dropped would be aimed at freed tables. Recording
        // first means the worst a tick can do is load the new root slightly
        // early, which is where the process is going anyway.
        let new_root = space.root();
        task.space = Some(space);
        task.user_entry = Some((loaded.entry, user_rsp));
        // The group's memory becomes the new image's. A process that execs
        // with threads running is a case Linux handles by ending them; this
        // kernel does not yet, and the threads that were there keep the old
        // group until they exit.
        //
        // The credentials cross the boundary with the process, and the exec
        // itself re-derives them: an `euid` of 0 regains the bounding set's
        // capabilities (measured - a root process that had lowered its
        // permitted set gets it back), which is what makes
        // `PR_CAPBSET_DROP` a promise that survives an `exec`. The dumpable
        // flag is deliberately *not* reset here: Linux's `setup_new_exec`
        // only moves it for a setuid image, and this kernel reads no file
        // capabilities or setuid bits at exec.
        let mut carried = task.mm.credentials();
        carried.on_exec(task.identity.euid);
        let next_mm = Mm::with_eager(id, new_root, loaded.frames, loaded.ranges);
        next_mm.set_credentials(carried);
        task.mm = next_mm;
        task.brk_base = loaded.brk;
        task.brk_pages = 0;
        task.mmap_cursor = layout.mmap_top;
        task.stack_top = loaded.stack_top;
        task.exe = alloc::string::String::from(exe);
        // The program that was running is being replaced, and so is the
        // command line that described it: a process that `exec`s is the same
        // pid with a new name, and `/proc/<pid>/cmdline` has to say the new
        // one from here on.
        task.argv = alloc::sync::Arc::new(argv.to_vec());
        // And the environment goes with it, for the same reason and in the
        // same breath: the old image's block is gone with its stack, so the
        // vector replaced here is the only record `/proc/<pid>/environ` can
        // answer from. The old `Arc` drops unless a `fork` still holds it.
        task.env = alloc::sync::Arc::new(env.to_vec());

        // Reset signal dispositions to SIG_DFL (POSIX requirement).
        task.dispositions = signal::Actions::default();
        task.alt_stack = None;
        task.signals = 0;

        // Close O_CLOEXEC file descriptors.
        {
            use crate::vfs::file::FD_CLOEXEC;
            let fds = &task.fds;
            // Collect fds to close (can't modify while iterating).
            let mut to_close = alloc::vec::Vec::new();
            for fd in 0..1024i32 {
                if let Ok(flags) = fds.fd_flags_get(fd)
                    && flags & FD_CLOEXEC != 0
                {
                    to_close.push(fd);
                }
            }
            // The descriptions are held until the batch is whole, because the
            // question each of them is asked - "is this the last reference?" -
            // can only be answered while every one of them is still counted.
            let mut closing = alloc::vec::Vec::new();
            for fd in to_close {
                if let Ok(file) = fds.clone_at(fd) {
                    closing.push(file);
                }
                let _ = fds.close(fd);
            }
            #[cfg(target_os = "none")]
            {
                // Not the exit hooks: this is `execve`'s `FD_CLOEXEC` pass, and
                // the process lives on. The cloexec list is told, which is the
                // two whose *reader position* is keyed by an address the new
                // image will reuse - the socket, watch and lock paths have their
                // own handling at close time and are exit-only. Walking the
                // registry rather than naming `drivers::virtio::input` and
                // `drivers::uinput` here is what severs the last `sched ->
                // drivers` edge; a machine with neither registered (aarch64 today)
                // walks an empty list, which is the host build's tested no-op.
                let owner = task.tgid.0 as u32;
                crate::sched::exit::run_cloexec(&closing, owner);
            }
            drop(closing);
        }

        // Point the CPU at the new image. Dropping the old address space left it
        // on the kernel's tables - a drop must, since it is about to free the
        // hierarchy the CPU is walking - and the kernel's tables do not map the
        // program's entry point. Nothing will call `prepare` for this task
        // either: it is not being dispatched, it is simply continuing, and the
        // next thing it does is `sysretq` into the image that was just loaded.
        hal::switch_to_root(new_root);

        Ok((loaded.entry, user_rsp))
    })
}

/// Resolve a copy-on-write page fault for the current task.
///
/// If the faulting address is mapped with the COW bit, the page is copied: the
/// task gets a private frame, the old one keeps its other owners. The frame
/// lists have to follow, or the new frame leaks and the old one is freed too many
/// times - so the old frame is swapped for the new in this task's list, and the
/// old frame's owner count is dropped by one.
///
/// Returns `false` if the fault cannot be resolved as COW, leaving the caller to
/// deliver SIGSEGV.
pub fn clone(state: &UserFrame, request: CloneRequest) -> isize {
    // The namespace kinds `clone` may carry, decided before anything is
    // created: privilege first, then the one kind whose copy is wired into the
    // child's construction below (`clone_namespace_verdict` names which, and
    // why the rest are refused whole rather than half-served). A thread-group
    // nsproxy is *not* needed for this — the child is a new group by
    // construction, so handing it a fresh `Arc` at creation is the whole
    // mechanism, which is why MNT lands here while CLONE_FS-style separation
    // stays parked with the shared-group handle.
    let privileged = current_credentials().has_effective(crate::cred::CAP_SYS_ADMIN);
    if let Err(e) = crate::abi::clone_namespace_verdict(request.flags, privileged) {
        return e;
    }
    if request.flags & crate::abi::CLONE_THREAD != 0 {
        return spawn_thread(state, request);
    }
    spawn_copy(state, request)
}

/// Create a thread: a task that runs on the calling task's memory, with the
/// calling task's descriptors, and its own stack.
///
/// This is what `fork` would be with nothing copied. The address space is not
/// duplicated and no frames are shared copy-on-write - the new task *is* the
/// same process, one more instruction pointer in it - so the group's frame
/// list stays as it is, and the group's count goes up by one: a thread that
/// leaves must not free memory its siblings are still using.
///
/// The child resumes inside the `clone` call its creator made, with `rax` zero
/// and `rsp` on the stack it was given, which is how a libc's thread entry
/// finds its way back to the function it was asked to run: it set that up on
/// the new stack before calling in.
fn spawn_thread(state: &UserFrame, request: CloneRequest) -> isize {
    // A thread is a task too, and Linux allocates its pid in the same chain a
    // `fork` walks - so the same seal applies: a namespace whose init has exited
    // takes no new tasks, a thread of a process still in it included. Asked
    // before anything is allocated, like `spawn_copy`'s two refusals.
    if with_table(|t| t.task(t.current).pid_ns.is_sealed()) {
        return crate::err::ENOMEM;
    }

    // A thread with no stack of its own would run on its creator's, and the
    // first thing either of them did would corrupt the other. Linux leaves
    // this to the caller; refusing is the kinder answer.
    if request.child_stack == 0 {
        return crate::err::EINVAL;
    }

    let child_id = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        let mm = parent.mm.clone();
        // A thread shares its process's descriptor *table* under `CLONE_FILES`
        // - which is what a libc's thread creation asks for, and the whole
        // reason `close` in one thread is visible to its siblings - and gets a
        // private copy without it, exactly as Linux's `copy_files` decides.
        let fds = if request.flags & crate::abi::CLONE_FILES != 0 {
            alloc::sync::Arc::clone(&parent.fds)
        } else {
            alloc::sync::Arc::new(parent.fds.duplicate())
        };
        let tgid = parent.tgid;
        let pgid = parent.pgid;
        let session = parent.session;
        let ctty = parent.ctty;
        let identity = parent.identity.clone();
        // A thread is its process: the same namespaces, by reference. Linux
        // makes the same promise from the other side - `CLONE_THREAD` shares
        // the creator's `nsproxy` - so a thread that unshared would split from
        // its own process, which is not what a thread is.
        let uts = alloc::sync::Arc::clone(&parent.uts);
        let pid_ns = alloc::sync::Arc::clone(&parent.pid_ns);
        let pid_ns_children = alloc::sync::Arc::clone(&parent.pid_ns_children);
        let net_ns = parent.net_ns;
        // `CLONE_FS` is what every `pthread_create` sets: a thread that
        // `chdir`s moves its process, which is Linux's answer (measured -
        // `.workbuddy/probe_unshare_flags.c`). A raw `clone(CLONE_THREAD)`
        // that did not name it gets a handle of its own.
        let fs = if request.flags & crate::abi::CLONE_FS != 0 {
            alloc::sync::Arc::clone(&parent.fs)
        } else {
            parent.fs.copy()
        };
        let oom_score_adj = parent.oom_score_adj;
        let oom_score_adj_min = parent.oom_score_adj_min;
        let nice = parent.nice;
        let rlimits = parent.rlimits;
        let seccomp_strict = parent.seccomp_strict;
        let seccomp_filters = parent.seccomp_filters.clone();
        let priority = parent.priority;
        let dispositions = parent.dispositions;
        // The new thread's signal mask is its creator's, copied: POSIX says a
        // thread inherits the creating thread's mask, and musl's `SIGEV_THREAD`
        // helper depends on it - `timer_create` blocks `SIGRTMIN` in the
        // creating thread and the helper *thread* is what waits on it, so a
        // thread that started unblocked would take the timer's signal as a
        // fatal default action instead of waiting for it.
        let blocked = parent.blocked;
        let parent_fs_ns = parent.fs_ns.clone();
        let brk_base = parent.brk_base;
        let brk_pages = parent.brk_pages;
        let mmap_cursor = parent.mmap_cursor;
        // The main stack belongs to the process, and so does where it ends: a
        // thread that grew one of its own pages past that bound would be growth
        // the leader's layout never reserved room for.
        let stack_top = parent.stack_top;
        let exe = parent.exe.clone();
        let argv = alloc::sync::Arc::clone(&parent.argv);
        let env = alloc::sync::Arc::clone(&parent.env);
        // The creator's *live* base, not its record: see `running_tls`.
        let parent_tls = running_tls(parent);

        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        // A thread is its process, so it is a member of the namespace the
        // process is in - not of the one the process's *children* go to, which
        // `unshare(CLONE_NEWPID)` may have moved somewhere else. That is the
        // same rule `CLONE_THREAD` follows for the names above it, and why the
        // verdict refuses `CLONE_THREAD | CLONE_NEWPID`: a thread cannot be the
        // init of a namespace its process is not in.
        // (`pid_ns` was cloned before the slot was taken, so this does not
        // hold the table's immutable borrow across `alloc_slot`.)
        super::adopt_pid(TaskId(slot), &pid_ns);

        let stack = Stack::new();
        // Interrupts stay off until the thread's `iretq` restores the flags it
        // was created with, exactly as a fork child's does.
        let mut context = unsafe { hal::bootstrap(stack.top(), task_entry, slot, false) };
        // The child's floating-point registers are its creator's, read the same
        // way `fork`'s child reads them: from the registers if the parent still
        // holds them, from its saved block if it does not.
        *context.fpu_mut() = t.inherit_fpu(parent_id);

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            // No address space of its own: the group's root is what the
            // dispatcher loads for it.
            space: None,
            // ...but it *is* a user task, and saying so is what makes it
            // visible at all: `user` is the flag `/proc` filters on, and it
            // measures "has an image", not "owns an address space" - a thread
            // runs the group's image. Without it `/proc/<pid>/task` lists the
            // leader alone while `/proc/<pid>/status` counts both, which is the
            // one place a `ps -T` and a `Threads:` line can disagree.
            user: true,
            user_entry: None,
            user_resume: Some({
                // The child returns from the call with zero, and lands on the
                // stack the caller provided - the one it pushed `func` and
                // `arg` onto before asking for a thread.
                let mut frame = state.forked(request.child_stack);
                frame.set_result(0);
                frame
            }),
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            // Inherited from the creator (see where `blocked` is read above):
            // the mask a thread starts with is the one the thread that made it
            // held, which is what makes `timer_create`'s `SIGEV_THREAD`
            // helper able to wait for `SIGRTMIN` at all.
            blocked,
            // Linux shares the handler table under `CLONE_SIGHAND`; this
            // kernel copies it, so a thread that installs a handler does not
            // install it for its siblings. Nothing in this system's userland
            // does that yet, and a copy is the honest version of "not shared".
            dispositions,
            alt_stack: None,
            fds,
            fs,
            // A thread is its process: the group's mount table is shared by all
            // of its tasks, so this is the creator's slot value, by reference —
            // Linux's `clone(CLONE_FS)` is what every thread creation sets here,
            // and unsharing a mount namespace from within a thread group is left
            // to the task that asks (`unshare` moves only the caller).
            fs_ns: parent_fs_ns,
            // Not a child: `wait` collects processes, and a thread is not one.
            // The process becomes collectable when its leader exits.
            parent: None,
            pgid,
            // A thread is its process: the same session, the same controlling
            // terminal its creator's group holds.
            session,
            ctty,
            // ...and the same identity, group and user words alike. The
            // creation mask comes with the `fs` handle above - the same one
            // under `CLONE_FS`, a copy without it.
            identity,
            uts,
            pid_ns,
            pid_ns_children,
            // A thread is its process: the same stack, by reference - the rule
            // `CLONE_THREAD` follows for every namespace above it (and why the
            // verdict refuses a thread with a namespace of its own).
            net_ns: net_ns.clone(),
            oom_score_adj,
            oom_score_adj_min,
            nice,
            rlimits,
            seccomp_strict,
            seccomp_filters,
            brk_base,
            brk_pages,
            mmap_cursor,
            stack_top,
            exe,
            argv,
            // A thread shares its process's environment, one reference as for
            // `argv`: the two live on the one stack the whole group is running.
            env,
            child_tid: if request.flags & crate::abi::CLONE_CHILD_SETTID != 0 {
                request.child_tid
            } else {
                0
            },
            tls: if request.flags & crate::abi::CLONE_SETTLS != 0 {
                request.tls
            } else {
                parent_tls
            },
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
            mm: {
                mm.join();
                mm
            },
            released: false,
            tgid,
        });

        if request.flags & crate::abi::CLONE_CHILD_CLEARTID != 0 {
            // Where a joiner sleeps. The thread clears it on the way out, and
            // wakes whoever is waiting there - which is the whole of what
            // `pthread_join` needs from the kernel.
            t.tasks[slot]
                .as_ref()
                .expect("the thread was just built")
                .mm
                .set_clear_tid(request.child_tid);
        }

        let child_id = TaskId(slot);
        // A thread born into a tracer's hands is stopped before its first
        // instruction for the same reason a process child is - and it matters
        // more here, because a thread that has run once is a thread whose first
        // syscalls are missing from the trace. As with a process child, a
        // machine with no tracer always answers no here.
        let traced_child = attach_new_child(t, parent_id, child_id, PTRACE_EVENT_CLONE);
        if traced_child {
            t.sched.sleep(child_id, priority, u64::MAX);
        } else {
            t.sched.push_ready(child_id, priority);
        }
        child_id
    });

    child_id.0 as isize
}

/// End every thread of the running task's group, then this one.
///
/// `exit_group(2)` is what a libc calls when the *process* decides to end -
/// `exit`, a return from `main`, an unhandled signal - and Linux ends every
/// thread with it. The siblings are marked exited here; the release pass
/// accounts for each of them, so the group's memory goes when the last one has
/// been accounted for, exactly as if they had all returned on their own.
pub fn exit_group(code: isize) -> ! {
    let me = current();
    let tgid = current_tgid();
    with_table(|t| {
        for slot in 0..t.tasks.len() {
            let id = TaskId(slot);
            if id == me {
                continue;
            }
            let Some(task) = t.tasks[slot].as_mut() else {
                continue;
            };
            if task.tgid != tgid || task.exited {
                continue;
            }
            // Never scheduled again - and taken out of every queue, including
            // the sleep queue, so the picker can never reach a task whose slot
            // the release pass is about to free. Waking it here would queue a
            // dead pid: the memory it accounted for is released by the pass,
            // not by a turn it never gets.
            task.exited = true;
            t.sched.retire(id);
        }
    });
    exit(code)
}

/// Name where the running task's exit is announced: the address a leaving
/// thread writes zero to and wakes, which is what a joiner sleeps on.
pub fn set_clear_tid(address: usize) {
    with_table(|t| t.task(t.current).mm.set_clear_tid(address));
}

/// The process id of the running task: the id of its thread group's leader.
///
/// `getpid` answers this from every thread - a process has one pid however
/// many threads it runs - while `gettid` answers the thread's own id, which is
/// what `gettid` is for.
pub fn current_tgid() -> TaskId {
    with_table(|t| t.task(t.current).tgid)
}

/// Where a fresh task's stack first lands. The arch trampoline calls this with
/// the task's slot number in the first argument register; the body it should
/// run, the register file it should resume, or the user image it should enter is
/// still in the table.
pub(crate) extern "C" fn task_entry(slot: usize) -> ! {
    let (body, user, resume, child_tid) = with_table(|t| {
        let task = t.task_mut(TaskId(slot));
        // The resume state is *borrowed*, not taken: `resume_user` never
        // returns, and the task's slot - and so the state inside it - stays put
        // for as long as the task exists. Copying it onto this stack would be
        // pointless work for a door that only opens one way.
        let resume = task
            .user_resume
            .as_ref()
            .map_or(core::ptr::null(), |state| state as *const UserFrame);
        (
            task.body.take(),
            task.user_entry.take(),
            resume,
            task.child_tid,
        )
    });
    if let Some(body) = body {
        body();
        exit(0);
    }
    if !resume.is_null() {
        // `clone(CLONE_CHILD_SETTID)` asked for the child's own tid to be
        // waiting where the child can read it. This runs on the child's address
        // space - the dispatcher has already loaded it - so the write lands
        // exactly where the caller asked.
        if child_tid != 0 {
            unsafe { (child_tid as *mut i32).write_volatile(slot as i32) };
        }
        // A forked child: come back from the `fork` call its parent made, with
        // `rax` zero. One way door, exactly as below.
        unsafe { hal::resume_user(resume) };
    }
    if let Some((entry, user_rsp)) = user {
        // One way door: the kernel stack this task arrived on stays behind as
        // nothing but the trap target, and the reaper eventually frees it.
        unsafe { hal::enter_user(entry, user_rsp) };
    }
    panic!("task {slot} has neither a body nor a user image")
}

/// Is there something to report about one of `parent`'s children, given what
/// the caller asked to hear about? `t` is borrowed rather than taken because
/// this is also the blocking condition, which must not change anything.
///
/// The order is stops, then deaths, then resumes. A death comes last of the
/// three because it cannot be reported at all until a stop that precedes it has
/// been; a resume comes last because it is the least interesting thing that can
/// have happened, and a tracer that is told about one when a stop was waiting
/// has lost the stop.
fn any_child_event(t: &Table, parent: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    let children = || {
        t.tasks.iter().enumerate().filter_map(|(slot, task)| {
            let task = task.as_ref()?;
            // A slot this task's own id names is not its child: `any_child_code`
            // makes the same exclusion for the same reason.
            (waitable(task, parent) && TaskId(slot) != parent).then_some((TaskId(slot), task))
        })
    };
    if let Some(stopped) = unreported_stop(t, parent, stops) {
        return Some(stopped);
    }
    if continues {
        for (id, task) in children() {
            if task.trace.continued {
                return Some(WaitEvent::Continued { id });
            }
        }
    }
    // Deaths last, and read out of the retired list exactly as the exit-only
    // `wait` reads them - the same records, so a caller that asks about stops
    // cannot lose an exit code by asking. A record still owned by a tracer is
    // the tracer's alone to see.
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| {
                let id = retired.id;
                let free = t.tasks[id.0].is_none();
                let ours = free
                    || t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| waitable(task, parent));
                ours && id != parent && retired.tracer.is_none_or(|tracer| tracer == parent)
            })
            .map(|retired| WaitEvent::Exited {
                id: retired.id,
                code: retired.code,
            })
    })
}

/// Turn a look into a collection: mark the stop reported, take the `SIGCONT`
/// flag, or consume the exit code. Everything this does is a change, which is
/// why it is a second pass over the table rather than part of the look.
fn take_event(t: &mut Table, event: WaitEvent) -> Option<WaitEvent> {
    match event {
        // The code in the event is a look, not a claim: the value that counts
        // is the one taken here, because between the look and the take another
        // waiter could have collected it.
        WaitEvent::Exited { id, .. } => {
            take_exit_code_for(t, id, t.current).map(|code| WaitEvent::Exited { id, code })
        }
        WaitEvent::Stopped { id, signal, event } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            let stop = task.trace.stop.as_mut()?;
            if stop.reported {
                return None;
            }
            stop.reported = true;
            Some(WaitEvent::Stopped { id, signal, event })
        }
        WaitEvent::Continued { id } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            if !task.trace.continued {
                return None;
            }
            task.trace.continued = false;
            Some(WaitEvent::Continued { id })
        }
    }
}

/// Block until a child of `parent` has something to report, and collect it.
/// The exit-only shape is `wait_any_child`, which this generalises.
pub fn wait_any_child_event(parent: TaskId, stops: bool, continues: bool) -> WaitPark<WaitEvent> {
    if matches!(
        block_until_on_interruptible(|t| any_child_event(t, parent, stops, continues).is_some()),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    match with_table(|t| {
        let event = any_child_event(t, parent, stops, continues)?;
        take_event(t, event)
    }) {
        Some(event) => WaitPark::Event(event),
        None => WaitPark::NoChild,
    }
}

/// The non-blocking half: is there anything to collect right now?
pub fn poll_any_child_event(parent: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    with_table(|t| {
        let event = any_child_event(t, parent, stops, continues)?;
        take_event(t, event)
    })
}

/// The same question for one named child, which is what `waitpid` asks. The
/// asker is the running task; `child_event` says what *it* may hear.
pub fn child_event(id: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    with_table(|t| child_event_in(t, id, t.current, stops, continues))
}

/// [`child_event`] against a table the caller already holds - the shape the
/// blocking condition needs, since it runs inside `schedule`'s borrow.
fn child_event_in(
    t: &Table,
    id: TaskId,
    me: TaskId,
    stops: bool,
    continues: bool,
) -> Option<WaitEvent> {
    let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
    if let Some(stop) = task.trace.stop
        && !stop.reported
        // Who may hear this stop - the same rule `unreported_stop` applies to
        // the wildcard wait: a ptrace stop is the tracer's, a group stop is
        // the parent's when `WUNTRACED` asked for it. Without the tracer check
        // a parent's plain `waitpid(pid)` collects a born-stop its tracer has
        // not even looked at yet.
        && (if stop.traced {
            task.trace.tracer == Some(me)
        } else {
            stops
        })
    {
        return Some(WaitEvent::Stopped {
            id,
            signal: stop.signal,
            event: stop.event,
        });
    }
    if task.trace.continued && continues {
        return Some(WaitEvent::Continued { id });
    }
    None
}

/// Collect `id`'s stop or resume, if there is one waiting.
pub fn take_child_event(id: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    let event = child_event(id, stops, continues)?;
    with_table(|t| take_event(t, event))
}

/// Whether `id` has anything a `wait` with these flags could report.
pub fn child_event_pending(id: TaskId, stops: bool, continues: bool) -> bool {
    child_event(id, stops, continues).is_some()
}

/// Block the running task until `id` has something to report, then collect it.
///
/// The stop-aware relative of [`block_until_code`]. The condition is read
/// through the borrow `block_until_on` already holds for the same reason it is
/// there: a closure taking its own critical section inside `schedule`'s would
/// nest one inside another and hand interrupts back early.
pub fn block_until_child_event(id: TaskId, stops: bool, continues: bool) -> WaitPark<WaitEvent> {
    if matches!(
        block_until_on_interruptible(|t| {
            exit_code(id).is_some() || child_event_in(t, id, t.current, stops, continues).is_some()
        }),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    if let Some(event) = take_child_event(id, stops, continues) {
        return WaitPark::Event(event);
    }
    match take_exit_code_current(id) {
        Some(code) => WaitPark::Event(WaitEvent::Exited { id, code }),
        None => WaitPark::NoChild,
    }
}

/// [`take_exit_code_for`] for the running task - the shape a `waitpid(pid)`
/// call lands in, where the collector is the caller itself.
pub fn take_exit_code_current(id: TaskId) -> Option<isize> {
    with_table(|t| take_exit_code_for(t, id, t.current))
}

/// The paging root `id` runs on: its own address space's, or its thread group's
/// for a thread. `None` for a kernel task, which has no user memory to reach.
pub fn space_root_of(id: TaskId) -> Option<usize> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        match &task.space {
            Some(space) => Some(space.root()),
            None if task.mm.root() != 0 => Some(task.mm.root()),
            None => None,
        }
    })
}

/// The thread-local storage base `id` runs with - the `fs_base` of a
/// `user_regs_struct` that belongs to somebody other than the caller.
pub fn tls_of(id: TaskId) -> Option<usize> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().map(|task| task.tls))
}
