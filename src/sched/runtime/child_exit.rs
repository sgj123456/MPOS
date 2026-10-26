use super::INIT_PID;
use super::WaitEnd;
use super::WaitEvent;
use super::WaitPark;
use super::child_mm::Mm;
use super::child_mm::{
    child_alive_for, populate_user_page_at, resolve_anon_fault, resolve_cow_fault,
};
use super::child_signal::{current_credentials, force_signal, notify_parent_of_death};
use super::load::{initial_stack_rsp, interpreter_bytes, load_user, write_arg_block};
use super::task::{
    ChildKind, RETIRED, RETIRED_CAP, Retired, SpawnError, Stack, TIMER_SLOTS, Table, Task, Tracing,
    WaitFor, block_until_on_interruptible, current, new_layout, schedule, with_table,
};
use super::trace::{attach_new_child, fork_event, kind_visible, unreported_stop, waitable};
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

        // Whether this is the **process's** death or only a thread's. Linux's
        // `group_dead` is the group's last thread leaving, and the parent hears
        // about the process only then - a thread that is not last leaves no
        // corpse, no record and no signal, and a leader that is not last is the
        // process's zombie, unreapable until the group ends. Measured whole on
        // a current host (`.workbuddy/probe_exit_group_thread.c`): a worker's
        // `exit_group(7)` makes the parent reap the *leader* with `7` (A), the
        // control's leader does the same (B), a leader that leaves on
        // `pthread_exit` first is reaped only when the group dies and with the
        // *last thread's* code - `7` under an `exit_group`, `5` under a plain
        // `exit` (C and C2).
        let tgid = t.task(id).tgid;
        let group_dead = !t
            .tasks
            .iter()
            .flatten()
            .any(|other| other.tgid == tgid && !other.exited);

        // The dying process's `SEM_UNDO` adjustments - Linux's `exit_sem`,
        // applied when the list's last holder leaves. Taken here rather than
        // through `sysv_sem`'s own door because this closure already holds the
        // task table, and the wake is handed in (`Table::wake`) for the same
        // reason: `sched::wake` would take it a second time.
        if let Some(list) = t.task_mut(id).sem_undo.take()
            && group_dead
            && alloc::sync::Arc::strong_count(&list) == 1
        {
            crate::sched::sysv_sem::apply_undo(&list, |woken| {
                t.sched.wake(woken);
            });
        }

        // The death's core-dump classification, computed here because it is a
        // fact about the *dying* process's own `dumpable` flag, which
        // `release_exited` below takes away: Linux's `WCOREDUMP`/`CLD_DUMPED`
        // is the signal's default action (`crate::abi::signal_dumps_core`)
        // **and** that flag - `.workbuddy/probe_wcore_dumpable.c` measures the
        // second half, a `SIGSEGV` death being `CLD_DUMPED` at dumpable 1 or 2
        // and `CLD_KILLED` at `PR_SET_DUMPABLE 0`. Derived once and carried on
        // both records, so a `wait4` reader and a `waitid` reader cannot
        // classify one death two ways.
        let dumped = code < 0
            && crate::abi::signal_dumps_core((-code) as i32)
            && t.task(id).mm.credentials().dumpable != 0;

        // The CPU a `wait`'s `struct rusage` will report for this child, read
        // here while the `mm` is still the live group's and carried on the
        // record: the slot - and with it the counters - is free by the time a
        // `wait` collects the death.
        let (utime, stime) = t.task(id).mm.cpu_both();

        // The process's death is recorded once, when its last thread leaves,
        // and it is **the leader's** record: the pid a parent can name is the
        // process's, not whichever thread happened to be last. The code and the
        // core-dump classification are the dying thread's - Linux's
        // `do_notify_parent` reads `tsk->exit_code` from the task that is
        // leaving, while `si_pid` is `task_tgid_nr_ns(tsk)` - and so is the
        // real uid (`task_uid(tsk)`). The **exit signal is the leader's**: a
        // thread is born with `-1` on Linux precisely so its death cannot
        // announce anything, and the measurement is a `clone(SIGUSR1)` group
        // whose worker ends it - the parent holds a `SIGUSR1`, not a `SIGCHLD`
        // (scenario D). A thread that is not last records nothing at all.
        if group_dead {
            let leader = t.task(tgid);
            // The numbers this process is known by at every level of its own
            // chain, taken while the slot still owns them: a `wait` answers
            // the number **its** namespace gives, and by the time it reads the
            // record the registration is gone (`Retired::numbers`).
            let numbers = crate::sched::pid_chain_of(tgid);
            record_retired(Retired {
                id: tgid,
                code,
                numbers,
                pgid: leader.pgid,
                // Which kind of child this was, for the `__WCLONE`/`__WALL`
                // half of a wait's option word (`ChildKind`) - the record is
                // what a wildcard wait asks, and the task is gone by then.
                exit_signal: leader.exit_signal,
                // The real uid, taken here because the slot is what holds it:
                // the record outlives the task, and `waitid` reads its answer
                // from the record (`Retired::uid`).
                uid: t.task(id).identity.ruid,
                dumped,
                utime,
                stime,
                tracer: leader.trace.tracer,
            });
        }

        // A pid namespace's init takes its namespace with it - Linux's
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
        //
        // Only the group's **last** thread is the process's death, and the
        // parent is the leader's. A thread's `parent` link is its group's
        // parent (`CLONE_THREAD` implies `CLONE_PARENT`), so the link alone
        // would announce every worker's exit as a child's; Linux gates the
        // announcement on `group_dead` (measured: a `fork` child's death
        // queues `SIGCHLD`, a thread's queues nothing -
        // `.workbuddy/probe_children_owner.c`), and the process's own death is
        // announced with the leader's pid even when a worker was the last to
        // leave (`.workbuddy/probe_exit_group_thread.c` A and C).
        let parent = if group_dead {
            t.task(tgid).parent
        } else {
            None
        };
        if let Some(parent) = parent {
            t.sched.wake(parent);
            // The signal the child was born to report, not just the wake. A
            // parent sitting in `wait(2)` needs only the wake - the code it
            // waits for is on file here - but a child's `clone` byte says
            // *which* signal its death should carry, and Linux's
            // `do_notify_parent` sends that one: a `fork`'s child reports
            // `SIGCHLD`, a `clone(SIGTERM)` child reports `SIGTERM`, and a
            // `clone(0)` child reports nothing at all. This kernel used to send
            // `SIGCHLD` through every door whatever the byte said - the half
            // `docs/uapi/` §5.2 named - so a `clone(0)` death raised a spurious
            // `SIGCHLD` and a `clone(SIGTERM)` death announced the wrong
            // signal. [`notify_parent_of_death`] now asks the parent's own
            // disposition and mask; its doc carries the rule and the
            // measurement (`.workbuddy/probe_exitsig.c`). The **wake** above is
            // unconditional and stays so: a parent parked in `wait4(__WCLONE)`
            // for a `clone(0)` child - which no signal will ever reach - is
            // woken by nothing else.
            //
            // The auto-reap half stays named rather than done (`docs/uapi/`
            // §5.2): this kernel keeps the zombie, so a parent that ignores its
            // death signal and never waits holds a slot where Linux holds none.
            //
            // The child as the record names it: the number the **group leader**
            // has *in the dying task's own namespace* - Linux's
            // `task_tgid_vnr(tsk)` read from the last thread, which is the
            // leader's number because a thread carries its group's `tgid`. A
            // parent in the same namespace reads back the number `getpid` gave
            // the process; a parent one level out reads the group's own number,
            // which is Linux's wart for a child in a deeper namespace than its
            // reader. The uid is the dying task's real one, the same field the
            // retired record carries for `waitid`'s `si_uid` (`task_uid(tsk)`).
            let (child_pid, child_uid) = {
                let child = t.task(id);
                (
                    child.pid_ns.number_of(tgid.0).unwrap_or(tgid.0 as u32) as i32,
                    child.identity.ruid as i32,
                )
            };
            // The child's CPU time, in the `USER_HZ` ticks `si_utime`/
            // `si_stime` carry. Linux's `do_notify_parent` fills them from
            // `nsec_to_clock_t(tsk->utime + tsk->signal->utime)`, and the
            // group's counters here are the same sum: `cpu_utime`/`cpu_stime`
            // are group-wide, so a thread that already exited has been folded
            // in and a single-threaded child's totals are its own. The `mm`'s
            // `Arc` outlives `release_exited` above - only its frames are freed
            // - and a task with `exited` set is never scheduled again, so no
            // further tick is charged and the number read here is final.
            // Measured with `.workbuddy/probe_sigchld_cputime.c` (Linux 7.2.9):
            // ~100 ms of CPU is `si_utime` 9, a child that only exits is
            // `0`/`0`, and the same pair comes back through a handler frame and
            // a `signalfd` read.
            let (utime, stime, _, _) = t.task(id).mm.cpu_all();
            let utime = crate::abi::clock_ticks_to_user_hz(utime, crate::hal::TICK_HZ as u64);
            let stime = crate::abi::clock_ticks_to_user_hz(stime, crate::hal::TICK_HZ as u64);
            // The leader's byte, not the dying thread's: a `CLONE_THREAD` task
            // is born with no exit signal on Linux precisely so its death
            // announces nothing, and the process's byte is the one it was
            // forked with. Measured with a `clone(SIGUSR1)` group whose worker
            // ends it - the parent holds a SIGUSR1, not a SIGCHLD
            // (`.workbuddy/probe_exit_group_thread.c`, D).
            let sig = t.task(tgid).exit_signal;
            notify_parent_of_death(
                t.task_mut(parent),
                sig,
                signal::SigInfo::child(sig, child_pid, child_uid, code, dumped, utime, stime),
            );
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
        // nor wait for anyone. Linux's `forget_original_parent` hands them to
        // an **alive task of the dying task's group** when one is left, and to
        // init otherwise; both halves are measured on a current host.
        // `.workbuddy/probe_children_owner.c` has a thread exit with its child
        // still running: the child moved onto the leader's `children` list and
        // its three parent doors went on naming the group.
        // `.workbuddy/probe_leader_exit.c` is the mirror - a leader leaves on
        // `pthread_exit` while its worker still runs, and the child moves onto
        // the *worker's* list, its `PPid` still reading the group because that
        // is the worker's `tgid`. Init is the last resort, and init itself is
        // exempt: it has no parent to lose.
        //
        // No `!task.exited` filter: a zombie child's parent names a slot too,
        // and slots are reused - the next process to occupy this one would
        // inherit the dead process's children, which `wait4`'s tables read as
        // its own (`t-signalfd` was handed four records for one death, three
        // naming earlier tenants of the slot).
        if id != INIT_PID {
            // ...and when the group has no one left, the nearest **subreaper**
            // above the dying task takes the children, which is what
            // `PR_SET_CHILD_SUBREAPER` buys a shell: its grandchildren whose own
            // parent died come to it instead of to init. Measured on a host -
            // `wait(-1)` reaps the grandchild with the flag set, and answers
            // `ECHILD` without it.
            let reaper = alive_thread_of(t, tgid, id)
                .or_else(|| nearest_subreaper(t, id))
                .unwrap_or(INIT_PID);
            let mut adopted = false;
            // `PR_SET_PDEATHSIG`'s delivery: Linux's `forget_original_parent`
            // sends each child the signal it asked for at the moment the parent
            // it names dies - the same walk that hands the child to a new
            // parent, because "the parent is gone" is exactly what both halves
            // are about. The signal is a **kernel raise** (`force_signal`, the
            // `SEND_SIG_PRIV` shape), and the child is woken here because that
            // primitive wakes nobody.
            let mut woken: Vec<TaskId> = Vec::new();
            for (slot, task) in t.tasks.iter_mut().enumerate() {
                let Some(task) = task.as_mut() else { continue };
                if task.parent == Some(id) {
                    task.parent = Some(reaper);
                    adopted = true;
                    if task.pdeathsig != 0 && !task.exited {
                        force_signal(task, task.pdeathsig as u8);
                        woken.push(TaskId(slot));
                    }
                }
            }
            for child in woken {
                t.sched.wake(child);
            }
            if adopted {
                t.sched.wake(reaper);
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

/// An alive task of the group `tgid` other than `dying`, preferring the leader.
///
/// The candidate list `forget_original_parent` looks for: a thread that exits
/// hands its children to the leader (measured), a leader that exits hands them
/// to a worker, and only a group with no live task left lets init take them.
/// Preferring the leader is not a preference for its own sake - it is the
/// answer a reader then *names*, since `parent_group` reports the adopter's
/// `tgid`, and the leader's `tgid` is the group.
/// The nearest **subreaper** above `from` - Linux's
/// `find_new_reaper`/`has_child_subreaper` walk, one task at a time.
///
/// It starts at the dying task's parent and stops at init, which is the last
/// resort anyway; a marked ancestor answers with an alive task of its group (a
/// thread of it if the leader has already gone, because the *process* is what
/// adopts).
fn nearest_subreaper(t: &Table, from: TaskId) -> Option<TaskId> {
    let mut next = t.task(from).parent?;
    // A bounded walk: the chain is a tree rooted at init, and the bound is the
    // task table itself - the shape a cycle would need to survive it.
    for _ in 0..t.tasks.len() {
        if next == INIT_PID {
            return None;
        }
        let leader = t.task(next).tgid;
        if t.task(leader).subreaper {
            if let Some(alive) = alive_thread_of(t, leader, next) {
                return Some(alive);
            }
            if !t.task(leader).exited {
                return Some(leader);
            }
        }
        next = t.task(next).parent?;
    }
    None
}

fn alive_thread_of(t: &Table, tgid: TaskId, dying: TaskId) -> Option<TaskId> {
    let alive = |slot: usize| {
        slot != dying.0
            && t.tasks
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|task| !task.exited && task.tgid == tgid)
    };
    if alive(tgid.0) {
        return Some(tgid);
    }
    t.tasks
        .iter()
        .enumerate()
        .find(|(slot, _)| alive(*slot))
        .map(|(slot, _)| TaskId(slot))
}

/// The exit code of the most recent task with this id to have exited, if any.
///
/// Recorded at exit, not at reap: the reader may come a tick later, after the
/// slot has already been freed and handed to someone else.
///
/// This is the kernel's look; it takes nothing, so a milestone can read the
/// same code twice. A process collecting its child's status goes through
/// `wait_child_event` (which consumes the record) or [`take_exit_code`] for the
/// pid it named.
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
pub(crate) fn take_retired_for(t: &Table, id: TaskId, me: TaskId) -> Option<Retired> {
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
            return Some(retired.remove(at));
        }
        if retired[at].tracer == Some(me) {
            retired[at].tracer = None;
            return Some(retired[at].clone());
        }
        Some(retired.remove(at))
    })
}

/// Collect `id`'s exit code regardless of tracer ownership. The boot task's
/// use for init, whose death nobody reaps, and nothing else.
pub fn take_exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|retired| retired.id == id)?;
        Some(retired.remove(at).code)
    })
}

/// The pid and code of the oldest of `parent`'s children to have exited and
/// not yet been collected, written as a `NO_CODE` marker when the caller only
/// wants to know that a child died.
///
/// Nothing is consumed here: the event scan holds the table while it looks, and
/// the record is taken afterwards, by pid, in [`take_exit_code`]. A read-only
/// look keeps this function usable from the `block_until` closure, which must
/// not wake anyone.
pub(crate) fn any_child_code(
    t: &Table,
    parent: TaskId,
    kind: ChildKind,
) -> Option<(TaskId, isize)> {
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
                        .is_some_and(|_| waitable(t, id, parent, kind));
                ours && id != parent
                    && kind.sees(retired.exit_signal)
                    && retired.tracer.is_none_or(|tracer| tracer == parent)
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

fn record_retired(record: Retired) {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        if retired.len() == RETIRED_CAP {
            retired.remove(0);
        }
        retired.push(record);
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
    // Linux's `fork` is `clone(SIGCHLD)`: the child exists to be waited for, and
    // the byte it is born to report is what makes it a "non-clone" child on the
    // other end (`ChildKind`). A default request carries no byte - a bare
    // `clone(0)` - which is a *different* call and a different kind.
    spawn_copy(
        state,
        CloneRequest {
            exit_signal: signal::SIGCHLD,
            ..CloneRequest::default()
        },
    )
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
            // `CLONE_VM|CLONE_VFORK|SIGCHLD` is the whole word on Linux, and the
            // byte is the half `wait` reads back.
            exit_signal: signal::SIGCHLD,
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
#[derive(Clone, Default)]
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
    /// The IPC namespace to give the child, or `None` to let it inherit its
    /// creator's - `CLONE_NEWIPC`'s half, filled in by the syscall layer for the
    /// same reason [`Self::net_ns`] is: making one means naming the identity
    /// layer, and `sched` decides nothing about what a flag word means.
    ///
    /// Measured on a current host (`.workbuddy/probe_clone_ipc.c`, as root
    /// inside a user namespace): the **child** moves into the fresh namespace
    /// and the parent keeps its own - the parent's key still resolves to the
    /// parent's queue while the child finds the key absent and numbers its first
    /// queue `0` - and `clone3`'s own flag field behaves identically.
    pub ipc_ns: Option<alloc::sync::Arc<crate::vfs::ns::IpcNs>>,
    /// The group the child is to be born in, when `clone3`'s
    /// `CLONE_INTO_CGROUP` named one - `None` for every other door, which
    /// leaves the child where its parent is grouped.
    ///
    /// Resolved by the syscall layer (a descriptor's table is there, and the
    /// group node is `vfs::cgroupfs`'s to hand out), and consumed *inside*
    /// the spawn's critical section below: the child is on the ready queue
    /// before `clone` returns, so a membership write after it would be a
    /// window in which the child reads the parent's group.
    ///
    /// Measured on a current host before it was built
    /// (`.workbuddy/probe_clone3_cgroup*.c`, Linux 7.2.11): the child and its
    /// own fork both read the target group in `/proc/<pid>/cgroup`, the
    /// caller's own line does not move, and a `CLONE_NEWCGROUP` beside the
    /// flag roots the new namespace at the **child's** group (the target),
    /// not the caller's.
    pub cgroup: Option<alloc::sync::Arc<crate::vfs::cgroupfs::CgDir>>,
    /// The signal the child is born to report - `clone`'s low byte, which the
    /// syscall layer split off the flag word before it got here
    /// (`abi::validate_clone_flags`) and which `wait` reads back through
    /// [`ChildKind`]. `fork`/`vfork` pass `SIGCHLD`; a bare `CloneRequest`
    /// carries `0`, which is `clone(0)`'s own answer - a child that reports
    /// nothing, and so a "clone" child. `CLONE_THREAD` never reaches the field:
    /// [`spawn_thread`] gives the thread `SIGCHLD` for the reason its own
    /// `exit_signal` line gives.
    pub exit_signal: u8,
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
        space_owner,
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
        sem_undo,
    ) = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        // The address space a `fork` copies is the *group's*, and a thread owns
        // none of its own: asking the calling task's slot answers "no space" for
        // every non-leader thread, which is `ENOMEM` to a `fork` Linux serves.
        // The same lookup `resolve_cow_fault` and `/proc` make (`space_owner`),
        // so the three cannot disagree about whose memory the group runs on.
        let space_owner = t.space_owner(parent_id);
        let owner = t.task(space_owner);
        let parent_has_space = owner.space.is_some();
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
        if let Some(space) = &owner.space {
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
        // `CLONE_SYSVSEM` shares the semaphore undo list (Linux's
        // `copy_semundo`: one reference up on that flag, and `NULL` - no list at
        // all - without it). That is why `pthread_create` shares its process's
        // adjustments while a plain `fork` inherits none.
        let sem_undo = if request.flags & crate::abi::CLONE_SYSVSEM != 0 {
            parent.sem_undo.clone()
        } else {
            None
        };
        (
            parent_id,
            space_owner,
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
            sem_undo,
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
            // Owned by the *creating* task's user namespace: the child is made
            // by its parent, and `setns` into it is asked about that one.
            let owner = Arc::clone(&t.task(parent_id).user_ns);
            crate::pidns::PidNs::child(
                &t.task(parent_id).pid_ns_children,
                crate::vfs::ns::mint_owned(owner),
            )
        } else {
            Arc::clone(&t.task(parent_id).pid_ns_children)
        };
        super::adopt_pid(TaskId(slot), &child_pid_ns);

        // Which user namespace this child is *born* in. Unlike the pid kind
        // there is no children's slot: `CLONE_NEWUSER` moves whoever is in it,
        // and this path is the child's side of that door - the child is the
        // first holder of a namespace of its own. Linux's `create_user_ns` runs
        // for the child (`copy_creds`), so the child starts *already in* the
        // fresh namespace, its stored ids inherited from the parent but its
        // table empty: measured, its own `getuid` reads the overflow id until a
        // map is written, and either the parent (in the namespace's parent) or
        // the child itself may write it (`t-ns-user`'s child-side claims, the
        // observation in `.workbuddy/probe-userns9.c`). Without the flag the
        // reference is handed on, as every fork since the beginning did.
        let child_user_ns = if request.flags & crate::abi::CLONE_NEWUSER != 0 {
            // The parent's namespace is the one being *left*, so it is the
            // child's owner - the same rung a `setns` into the child asks about.
            let parent = Arc::clone(&t.task(parent_id).user_ns);
            crate::userns::UserNs::fresh(
                crate::vfs::ns::mint_owned(Arc::clone(&parent)),
                parent,
                t.task(parent_id).identity.euid,
            )
        } else {
            Arc::clone(&t.task(parent_id).user_ns)
        };

        // Which time namespace this child is born into: its creator's
        // `time_ns_children`, and that one alone fills *both* of the child's
        // slots. There is no `CLONE_NEWTIME` branch here because a real kernel
        // does not serve that flag to `clone` at all - it is `EINVAL`, measured,
        // so a time namespace is only ever made by `unshare` (§6.3).
        //
        // Sealing it in the same breath is the freeze rule: the moment a task is
        // in a namespace its offsets stop being writable, which is what makes
        // "set the offsets before the first fork" a rule rather than a
        // suggestion. The machine's namespace is born sealed and this is a
        // no-op on it.
        let child_time_ns = Arc::clone(&t.task(parent_id).time_ns_children);
        child_time_ns.freeze();

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
        if let Some(parent_space) = &mut t.task_mut(space_owner).space {
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
            sem_undo,
            pdeathsig: 0,
            subreaper: false,
            no_new_privs: t.task(parent_id).no_new_privs,
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
                // `CLONE_NEWUSER`: the child is born in the namespace this
                // word made, and *making* a user namespace is the grant - the
                // full capability set, scoped to it. Applied to the credentials
                // just inherited, through the same call `unshare`'s and `setns`'
                // doors make (`Credentials::on_enter_user_ns`), so the three
                // cannot drift. Measured: the child reads the full set the
                // instant it exists, before any map is written
                // (`.workbuddy/probe-userns9.c`).
                if request.flags & crate::abi::CLONE_NEWUSER != 0 {
                    mm.with_credentials(|c| c.on_enter_user_ns());
                }
                mm
            },
            released: false,
            tgid: TaskId(slot),
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            // The mask is inherited, as POSIX says a child's is: a program
            // that blocks a signal and then forks expects the child to start
            // with it blocked too. A mask is not a disposition - it is the
            // caller saying "not now", and a child that un-blocked what its
            // parent held would take a signal the parent meant to handle
            // itself.
            blocked: t.task(parent_id).blocked,
            // The dispositions are inherited too, which is Linux's `copy_sig`
            // (measured on a current host, `.workbuddy/probe_sig_inherit.c`):
            // a handler installed before a `fork` runs in the child, and a
            // `SIG_IGN` stays ignored. `clone3`'s `CLONE_CLEAR_SIGHAND` asks
            // for the table an `exec` would leave instead - every slot at
            // `SIG_DFL` - and the handle is the same one an `exec` uses.
            dispositions: if request.flags & crate::abi::CLONE_CLEAR_SIGHAND != 0 {
                signal::Actions::default()
            } else {
                t.task(parent_id).dispositions
            },
            // The alternate signal stack crosses a `fork`/`clone` too: Linux's
            // child reads the same `ss_sp` and size the parent set, measured
            // on a current host (`.workbuddy/probe_altstack_fork.c`: both
            // processes reported the parent's pointer). `exec` is the door
            // that drops it, not the fork.
            alt_stack: t.task(parent_id).alt_stack,
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
                // Made by the parent, so owned by the parent's user namespace.
                let owner = Arc::clone(&t.task(parent_id).user_ns);
                match &t.task(parent_id).fs_ns {
                    Some(table) => Some(table.copy(owner)),
                    // The machine's table copied: a child of an unshared
                    // parent gets its own private view of the same world.
                    None => Some(crate::vfs::mount::machine_table().copy(owner)),
                }
            } else {
                t.task(parent_id).fs_ns.clone()
            },
            parent: Some(parent_id),
            // The byte the caller's flag word carried - the `clone` door's own
            // low byte, and what `wait`'s `__WCLONE`/`__WALL` read back at the
            // other end (`ChildKind`). `fork`/`vfork` hand in `SIGCHLD`, which
            // is the same call on Linux.
            exit_signal: request.exit_signal,
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
                let owner = Arc::clone(&t.task(parent_id).user_ns);
                t.task(parent_id)
                    .uts
                    .copied(crate::vfs::ns::mint_owned(owner))
            } else {
                Arc::clone(&t.task(parent_id).uts)
            },
            // Born in the namespace computed above, and its own children will
            // be born in the same one until it unshares - so both fields start
            // out equal, which is what makes an ordinary `fork` a no-op here
            // and `CLONE_NEWPID` the only thing that moves either.
            pid_ns: Arc::clone(&child_pid_ns),
            pid_ns_children: child_pid_ns,
            // Born in the namespace computed above, in *both* slots: it reads
            // its own clocks through the namespace its creator was pointing its
            // children at, and its own children will be born in the same one
            // until it unshares.
            time_ns: Arc::clone(&child_time_ns),
            time_ns_children: child_time_ns,
            // One value, not two: a user namespace has no children's slot, so a
            // child is born in the one computed above - its creator's, or the
            // fresh namespace `CLONE_NEWUSER` made for it.
            user_ns: child_user_ns,
            // The network namespace: the creator's, unless this word asked for a
            // fresh one (`CLONE_NEWNET`, minted by the syscall layer, which is the
            // layer allowed to name `net`).
            net_ns: request.net_ns.or(t.task(parent_id).net_ns),
            // The same for `ipcs`: the objects a child is born with are its
            // creator's, and the clone door has no `CLONE_NEWIPC` to ask for
            // others (`validate_clone_flags` refuses every namespace bit, and
            // `unshare` is the door this kind is served through).
            ipc_ns: request.ipc_ns.or_else(|| t.task(parent_id).ipc_ns.clone()),
            // The cgroup view-root: the creator's, unless this word asked
            // for a fresh namespace - whose root is the group the creator
            // is in *now*, served as `/` (`crate::vfs::cgroupfs` carries
            // the tree and the render rule). Beside `CLONE_INTO_CGROUP` the
            // root is the **target**, which is the child's own group: Linux
            // roots a fresh cgroup namespace at the group of the task the
            // namespace is made for, and with the flag that is not the
            // parent's group any more (measured: `INTO|NEWCGROUP` into
            // `/mposprobe/a` leaves the child reading `0::/`).
            cgroup_ns: if request.flags & crate::abi::CLONE_NEWCGROUP != 0 {
                Some(match &request.cgroup {
                    Some(target) => alloc::sync::Arc::from(target.path().as_str()),
                    // Rooted at the group the *parent* is in now - the real
                    // one, membership and not view (the header carries the
                    // rule).
                    None => crate::vfs::cgroupfs::ns_root(parent_id.0),
                })
            } else {
                t.task(parent_id).cgroup_ns.clone()
            },
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

        // Where a process child's `clear_child_tid` word lives. Linux records it
        // for **any** `clone` that names `CLONE_CHILD_CLEARTID`, the fork-shaped
        // one included, and a `clone` child reads it back through
        // `PR_GET_TID_ADDRESS` - which is how `t-prctl`'s claim found that this
        // path never set it while the thread path did (`docs/notes/` §6.20).
        if request.flags & crate::abi::CLONE_CHILD_CLEARTID != 0 {
            t.tasks[slot]
                .as_ref()
                .expect("the child was just built")
                .mm
                .set_clear_tid(request.child_tid);
        }

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
        // A child is born where its parent is grouped - Linux's `copy_process`
        // puts the child in `current`'s cgroup - and the cgroup view follows
        // the field the task struct carries. The membership map is keyed by
        // slot with the generation taken at the write, so a dead parent's
        // slot cannot leak its group to a child born in it later.
        //
        // `clone3`'s `CLONE_INTO_CGROUP` is the one word that overrides the
        // inheritance, and the override is written here for the same reason
        // the inheritance is: the child is pushed ready two lines down.
        match &request.cgroup {
            Some(target) => crate::vfs::cgroupfs::join_group(child_id.0, target),
            None => crate::vfs::cgroupfs::inherit_membership(parent_id, child_id),
        }
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

        // `exec` resets a caught disposition to `SIG_DFL` and leaves an
        // ignored one alone (POSIX; measured on a current host: a `SIGUSR1`
        // handler read back `SIG_DFL`, a `SIGUSR2` `SIG_IGN` stayed). The
        // blocked mask and the pending set cross the image - Linux flushes
        // neither - so only the handler table's caught slots move. The
        // alternate stack belongs to the old image and is dropped.
        task.dispositions.reset_caught();
        task.alt_stack = None;

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
        // Read before the slot is taken: the task literal that follows is built
        // after `alloc_slot` has borrowed the table mutably.
        let inherited_parent = parent.parent;
        // `CLONE_SYSVSEM` shares the semaphore undo list (Linux's
        // `copy_semundo`: a refcount up on that flag, and `NULL` - no list at
        // all - without it). That is why a `pthread_create` shares its
        // process's adjustments while a plain `fork` inherits none of them.
        let sem_undo = if request.flags & crate::abi::CLONE_SYSVSEM != 0 {
            parent.sem_undo.clone()
        } else {
            None
        };
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
        // A thread is its process for the clocks too, and the subtlety is that
        // a thread reads the namespace its process is *in* - not the one the
        // process points its children at. Those are the same namespace for every
        // process that has not unshared, and they differ for one that has: a new
        // thread of such a process must go on reading the clocks the process
        // reads, which is why this is `time_ns` and not `time_ns_children`. No
        // freeze is needed either way - the namespace a thread lands in already
        // has a task in it (the one asking), so it was sealed before this.
        let time_ns = alloc::sync::Arc::clone(&parent.time_ns);
        let time_ns_children = alloc::sync::Arc::clone(&parent.time_ns_children);
        // A user namespace has no children's slot, so the distinction above does
        // not arise: a thread reads its ids through the namespace its process is
        // in, which is this one value.
        let user_ns = alloc::sync::Arc::clone(&parent.user_ns);
        let net_ns = parent.net_ns;
        let ipc_ns = parent.ipc_ns.clone();
        // A thread joins its process's view of the cgroup tree, like every
        // namespace above (and why the verdict refuses a thread with one of
        // its own).
        let cgroup_ns = parent.cgroup_ns.clone();
        // A thread is created, not forked, so it starts with none either. The
        // subreaper flag *is* the process's, so the thread reads it from the
        // leader and never carries one of its own.
        let pdeathsig = 0u32;
        let subreaper = false;
        // A thread joins its process's promise: no new privileges either.
        let no_new_privs = parent.no_new_privs;
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
            sem_undo,
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
            // The creator's parent, not the creator: Linux's `kernel_clone`
            // turns `CLONE_THREAD` into `CLONE_PARENT`, so "threads share the
            // parent". The link answers the `PPid`-shaped doors - and only
            // those, because a thread is on no `children` list and its death
            // notifies nobody (`wait`'s rule asks `task.tgid == id`).
            // Measured on a current host (`.workbuddy/probe_children_owner.c`):
            // a worker thread read its *group's* parent through `getppid`,
            // `status` and `stat` while its own tid was the one it ran as.
            parent: inherited_parent,
            // A thread never notifies anyone of its death on Linux, so
            // `copy_process` rewrites this byte away for `CLONE_THREAD`
            // whatever `pthread_create` passed - and musl passes a word with
            // no signal at all. This kernel's threads are not children either
            // (the line above), so the byte is unobservable through `wait`;
            // it is `SIGCHLD` so that the one place a thread *does* leave a
            // record (its group's `Retired`) cannot be read as a "clone"
            // child's by a default `wait` that names its slot.
            exit_signal: signal::SIGCHLD,
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
            // ...and the same clocks, by reference, for the reason above.
            time_ns,
            time_ns_children,
            user_ns,
            // A thread is its process: the same stack, by reference - the rule
            // `CLONE_THREAD` follows for every namespace above it (and why the
            // verdict refuses a thread with a namespace of its own).
            net_ns: net_ns.clone(),
            // A thread is its process here too: one set of tables.
            ipc_ns,
            cgroup_ns,
            pdeathsig,
            subreaper,
            no_new_privs,
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
        // A thread joins its process's group, exactly as it joins every
        // other per-process fact (`spawn_thread` copies by reference).
        crate::vfs::cgroupfs::inherit_membership(parent_id, child_id);
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
///
/// The caller need not be the group's leader, and the parent's answer does not
/// depend on which thread it was: the group's death is announced from the
/// caller's `exit` as the **leader's**, with the caller's code, exactly once
/// (`.workbuddy/probe_exit_group_thread.c` A: a worker's `exit_group(7)` is
/// reaped as the leader with `7`). What the caller's own exit cannot do is
/// reparent a *sibling's* children - a thread ended here never runs the exit
/// path - so that walk is done here, before the group's last task leaves: no
/// task of the group will be alive to adopt them, which is why they go to init
/// (scenario E of `.workbuddy/probe_exit_group_thread.c` measured the rule on a
/// current host for the caller's own child; `t-exit-group` pins the sibling's
/// case, which is the one that needs this walk).
pub fn exit_group(code: isize) -> ! {
    let me = current();
    let tgid = current_tgid();
    with_table(|t| {
        let mut ended: Vec<TaskId> = Vec::new();
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
            // A sibling ended here never runs the exit path, so its hold on the
            // group's undo list has to go now: the last thread's `exit` applies
            // the list, and a stale `Arc` would keep it from being the last.
            task.sem_undo = None;
            t.sched.retire(id);
            ended.push(id);
        }
        // A sibling ended above never runs the exit path, so this is the only
        // place its children can be handed on. No task of the group will be
        // alive when the caller's own `exit` runs, and a child whose parent
        // slot is gone is collected by no `wait` - the parentage test reads
        // through that slot - so they go to init, the nearest thing this
        // kernel has to Linux's child subreaper (scenario E of
        // `.workbuddy/probe_exit_group_thread.c`, and `t-exit-group`'s
        // sibling fixture for the case this walk alone covers).
        let mut adopted = false;
        for task in t.tasks.iter_mut().flatten() {
            if task.parent.is_some_and(|parent| ended.contains(&parent)) {
                task.parent = Some(INIT_PID);
                adopted = true;
            }
        }
        if adopted {
            t.sched.wake(INIT_PID);
        }
    });
    exit(code)
}

/// Name where the running task's exit is announced: the address a leaving
/// thread writes zero to and wakes, which is what a joiner sleeps on.
/// The running task's `clear_child_tid` word, or 0 when `clone` never gave it
/// one - `PR_GET_TID_ADDRESS`'s whole answer.
///
/// Measured on a current host (`.workbuddy/probe_tidaddr.c`): a `clone` child
/// with `CLONE_CHILD_CLEARTID` reads back exactly the address its creator passed
/// as `child_tid`, a plain process answers 0 (a libc may have set one for its own
/// bookkeeping, which is why the value is a *report* and not a constant), and the
/// call refuses a pointer it cannot write with `EFAULT` while ignoring arguments
/// past the second.
#[cfg(target_os = "none")]
pub fn current_tid_address() -> usize {
    with_table(|t| t.task(t.current).mm.clear_tid().unwrap_or(0))
}

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
        //
        // The number is the child's **own namespace's**: Linux stores
        // `task_pid_vnr(p)`, so a `CLONE_NEWPID` child reads `1` while its
        // parent's `CLONE_PARENT_SETTID` word and `fork` answer carry the
        // parent's number for it (measured, `.workbuddy/probe_pid_numbers.c`,
        // Linux 7.2.11: word `1`, parent's word and wait `8`).
        if child_tid != 0 {
            let mine = crate::sched::own_pid_number(TaskId(slot));
            unsafe { (child_tid as *mut i32).write_volatile(mine as i32) };
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

/// Is there something to report about one of `parent`'s children **that `sel`
/// names and `kind` admits**, given what the caller asked to hear about? `t` is
/// borrowed rather than taken because this is also the blocking condition, which
/// must not change anything.
///
/// The order is stops, then deaths, then resumes. A death comes last of the
/// three because it cannot be reported at all until a stop that precedes it has
/// been; a resume comes last because it is the least interesting thing that can
/// have happened, and a tracer that is told about one when a stop was waiting
/// has lost the stop.
fn scan_child_event(
    t: &Table,
    parent: TaskId,
    sel: WaitFor,
    kind: ChildKind,
    stops: bool,
    continues: bool,
    exited: bool,
) -> Option<WaitEvent> {
    let children = || {
        t.tasks.iter().enumerate().filter_map(|(slot, task)| {
            let task = task.as_ref()?;
            // A slot this task's own id names is not its child: `any_child_code`
            // makes the same exclusion for the same reason.
            let id = TaskId(slot);
            (waitable(t, id, parent, kind) && id != parent && sel.names_task(id, task))
                .then_some((id, task))
        })
    };
    if let Some(stopped) = unreported_stop(t, parent, sel, kind, stops) {
        return Some(stopped);
    }
    if continues {
        for (id, task) in children() {
            if task.trace.continued {
                let (utime, stime) = task.mm.cpu_both();
                return Some(WaitEvent::Continued { id, utime, stime });
            }
        }
    }
    // Deaths last, and read out of the retired list exactly as the exit-only
    // `wait` reads them - the same records, so a caller that asks about stops
    // cannot lose an exit code by asking. A record still owned by a tracer is
    // the tracer's alone to see.
    //
    // `exited` is the caller's `WEXITED`: a wait that asked only for stops or
    // resumes must not be answered about a dead child, and Linux answers
    // `ECHILD` for one rather than "nothing yet" (measured,
    // `.workbuddy/probe_waitid_classes.c` and `probe_pidfd6.c`: `P_PID` and
    // `P_PIDFD` both answer `ECHILD` for a zombie under `WSTOPPED` alone).
    if !exited {
        return None;
    }
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
                        .is_some_and(|_| waitable(t, id, parent, kind));
                ours && id != parent
                    && sel.names_retired(retired, kind)
                    && retired.tracer.is_none_or(|tracer| tracer == parent)
            })
            .map(|retired| WaitEvent::Exited {
                id: retired.id,
                code: retired.code,
                numbers: retired.numbers.clone(),
                uid: retired.uid,
                dumped: retired.dumped,
                utime: retired.utime,
                stime: retired.stime,
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
            let retired = take_retired_for(t, id, t.current)?;
            // The slot goes with the record, as Linux's `release_task` runs
            // inside `do_wait`: once a `wait` has answered for a child, the
            // child is gone - out of `/proc`, and off its parent's `children`
            // file. This kernel's reaper only buries what has no record left,
            // so without this the corpse lingered until the next scheduler
            // turn, and a reading taken in between still saw it. Measured on
            // the sweep (`t-tid-dir`'s "the reap is what takes it off" and the
            // leader's `children` file, both red before this line existed).
            t.reap();
            Some(WaitEvent::Exited {
                id,
                code: retired.code,
                numbers: retired.numbers,
                uid: retired.uid,
                dumped: retired.dumped,
                utime: retired.utime,
                stime: retired.stime,
            })
        }
        WaitEvent::Stopped {
            id,
            signal,
            event,
            traced,
            utime,
            stime,
        } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            let stop = task.trace.stop.as_mut()?;
            if stop.reported {
                return None;
            }
            stop.reported = true;
            Some(WaitEvent::Stopped {
                id,
                signal,
                event,
                traced,
                utime,
                stime,
            })
        }
        WaitEvent::Continued { id, utime, stime } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            if !task.trace.continued {
                return None;
            }
            task.trace.continued = false;
            Some(WaitEvent::Continued { id, utime, stime })
        }
    }
}

/// Block until a child of `parent` that `sel` names has something to report,
/// and collect it. The one door every child-waiting call goes through: `wait4`
/// passes `WaitFor::Any` (or resolves its own pid first), `waitid` passes the
/// selector its `idtype` named.
pub fn wait_child_event(
    parent: TaskId,
    sel: WaitFor,
    kind: ChildKind,
    stops: bool,
    continues: bool,
    nowait: bool,
    exited: bool,
) -> WaitPark<WaitEvent> {
    if matches!(
        block_until_on_interruptible(|t| scan_child_event(
            t, parent, sel, kind, stops, continues, exited
        )
        .is_some()),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    match with_table(|t| {
        let event = scan_child_event(t, parent, sel, kind, stops, continues, exited)?;
        if nowait {
            Some(event)
        } else {
            take_event(t, event)
        }
    }) {
        Some(event) => WaitPark::Event(event),
        None => WaitPark::NoChild,
    }
}

/// The non-blocking half: is there anything to collect right now?
///
/// `nowait` is Linux's `WNOWAIT` (`waitid`'s bit): report what is there and
/// leave it there, so the next wait reports it again. Nothing *in the scan*
/// depends on it - a look is a look - so it is passed to the same take step and
/// answers the same event either way; what the flag decides is whether the
/// event survives the report.
pub fn poll_child_event(
    parent: TaskId,
    sel: WaitFor,
    kind: ChildKind,
    stops: bool,
    continues: bool,
    nowait: bool,
    exited: bool,
) -> Option<WaitEvent> {
    with_table(|t| {
        let event = scan_child_event(t, parent, sel, kind, stops, continues, exited)?;
        if nowait {
            Some(event)
        } else {
            take_event(t, event)
        }
    })
}

/// Block until *any* child of `parent` has something to report, and collect it.
pub fn wait_any_child_event(
    parent: TaskId,
    kind: ChildKind,
    stops: bool,
    continues: bool,
) -> WaitPark<WaitEvent> {
    wait_child_event(parent, WaitFor::Any, kind, stops, continues, false, true)
}

/// The wildcard half of [`poll_child_event`].
pub fn poll_any_child_event(
    parent: TaskId,
    kind: ChildKind,
    stops: bool,
    continues: bool,
) -> Option<WaitEvent> {
    poll_child_event(parent, WaitFor::Any, kind, stops, continues, false, true)
}

/// Whether `parent` has a child `sel` names at all - one still running, or one
/// whose record is waiting to be collected.
///
/// This is the question between `ECHILD` and a wait that parks for ever: a
/// selector that matches nobody can never be answered, and a caller told to
/// sleep would sleep with nothing coming. Linux asks the same thing of the same
/// selector, which is why `P_PGID` naming a group with no child of ours is a
/// measurement (`ECHILD`) rather than a design choice.
pub fn has_matching_children(parent: TaskId, sel: WaitFor, kind: ChildKind, exited: bool) -> bool {
    with_table(|t| {
        child_alive_for(t, parent, sel, kind)
            || (exited
                && critical_section::with(|cs| {
                    RETIRED.borrow(cs).borrow().iter().any(|retired| {
                        let id = retired.id;
                        let free = t.tasks[id.0].is_none();
                        let ours = free
                            || t.tasks[id.0]
                                .as_ref()
                                .is_some_and(|_| waitable(t, id, parent, kind));
                        ours && id != parent
                            && sel.names_retired(retired, kind)
                            && retired.tracer.is_none_or(|tracer| tracer == parent)
                    })
                }))
    })
}

/// The same question for one named child, which is what `waitpid` asks. The
/// asker is the running task; `child_event` says what *it* may hear.
pub fn child_event(id: TaskId, kind: ChildKind, stops: bool, continues: bool) -> Option<WaitEvent> {
    with_table(|t| child_event_in(t, id, t.current, kind, stops, continues))
}

/// [`child_event`] against a table the caller already holds - the shape the
/// blocking condition needs, since it runs inside `schedule`'s borrow.
fn child_event_in(
    t: &Table,
    id: TaskId,
    me: TaskId,
    kind: ChildKind,
    stops: bool,
    continues: bool,
) -> Option<WaitEvent> {
    let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
    // The named door asks the kind here rather than at its caller: a "clone"
    // child is not something a wait without `__WCLONE` may be answered about,
    // stop or death alike (measured, `.workbuddy/probe_wclone.c` B2/B3).
    if !kind_visible(task, me, kind) {
        return None;
    }
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
        // Read here, inside the table borrow, so the CPU is the stopped child's
        // and not whatever the slot is doing a moment later - the same reason
        // its identity is read in the borrow.
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
    if task.trace.continued && continues {
        let (utime, stime) = task.mm.cpu_both();
        return Some(WaitEvent::Continued { id, utime, stime });
    }
    None
}

/// Collect `id`'s stop or resume, if there is one waiting.
pub fn take_child_event(
    id: TaskId,
    kind: ChildKind,
    stops: bool,
    continues: bool,
) -> Option<WaitEvent> {
    let event = child_event(id, kind, stops, continues)?;
    with_table(|t| take_event(t, event))
}

/// Whether `id` has anything a `wait` with these flags could report.
pub fn child_event_pending(id: TaskId, kind: ChildKind, stops: bool, continues: bool) -> bool {
    child_event(id, kind, stops, continues).is_some()
}

/// Block the running task until `id` has something to report, then collect it.
///
/// The stop-aware relative of [`block_until_code`]. The condition is read
/// through the borrow `block_until_on` already holds for the same reason it is
/// there: a closure taking its own critical section inside `schedule`'s would
/// nest one inside another and hand interrupts back early.
pub fn block_until_child_event(
    id: TaskId,
    kind: ChildKind,
    stops: bool,
    continues: bool,
) -> WaitPark<WaitEvent> {
    // No child of ours is in that slot any more, and no record is waiting: this
    // is `ECHILD`, not a park.
    //
    // A `wait4(pid)` names a child by slot, and a slot is **reused** - so the
    // number can outlive the child it named and now belong to somebody else's
    // process. Without this check the predicate below answers "nothing yet" for
    // ever: `child_event_in` reads `t.tasks[id.0]` and finds a live stranger (no
    // stop, no death), while `exit_code(id)` finds nothing because the real
    // child's record was already collected. The caller then sleeps with
    // `u64::MAX` on a child that no longer exists - which is exactly how a boot
    // stops for ever one line after `Populating /dev ... [ ok ]`
    // (`docs/notes/` §7 R.9: `openrc` in `wait4(7)`, `alive=true`, `code=None`,
    // `RETIRED` empty).
    //
    // `waitable` is the same parent-and-kind question `child_event_in` asks
    // before reporting anything, so a slot that *is* still this caller's child -
    // a zombie's, a live one's, or a tracer's tracee - parks exactly as before.
    // Linux answers `ECHILD` here for the same reason: its `struct pid` outlives
    // the task, so a pid that is gone is gone and is never re-pointed at a
    // stranger.
    if !with_table(|t| waitable(t, id, t.current, kind) || exit_code(id).is_some()) {
        return WaitPark::NoChild;
    }
    if matches!(
        block_until_on_interruptible(|t| {
            exit_code(id).is_some()
                || child_event_in(t, id, t.current, kind, stops, continues).is_some()
        }),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    if let Some(event) = take_child_event(id, kind, stops, continues) {
        return WaitPark::Event(event);
    }
    match take_retired_current(id) {
        Some(retired) => WaitPark::Event(WaitEvent::Exited {
            id,
            code: retired.code,
            numbers: retired.numbers,
            uid: retired.uid,
            dumped: retired.dumped,
            utime: retired.utime,
            stime: retired.stime,
        }),
        None => WaitPark::NoChild,
    }
}

/// [`take_retired_for`] for the running task - the shape a `waitpid(pid)`
/// call lands in, where the collector is the caller itself.
///
/// Returns the whole record, not just the code, because `wait4`'s status word
/// owes the same core-dump classification (`Retired::dumped`) `waitid` reads
/// into `si_code`: `finish_wait` needs both fields, and taking the record is
/// what makes them this collector's.
pub(crate) fn take_retired_current(id: TaskId) -> Option<Retired> {
    with_table(|t| {
        let retired = take_retired_for(t, id, t.current)?;
        // The slot goes with the record, as Linux's `release_task` runs inside
        // `do_wait`: once a `wait` has answered for a child, the child is gone -
        // out of `/proc`, and off its parent's `children` file. This is the
        // path a *blocking* `wait4(pid)` takes, which does not go through
        // `take_event`; both doors reap here for the same reason.
        t.reap();
        Some(retired)
    })
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
