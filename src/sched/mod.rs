//! Task scheduling.
//!
//! Split the way the rest of the kernel is: `policy` decides which task runs
//! next and knows nothing about registers, so it is unit-tested on the host. The
//! runtime below it owns the task table and the stacks, and calls into
//! `arch::context` to move between them.

pub mod exit;
pub mod fs;
pub mod futex;
pub mod policy;
pub mod signal;

#[cfg(target_os = "none")]
mod runtime;

/// The scheduler's suite for a test build: its cases live beside the module they
/// test, and the wiring layer's harness lists this `const` with the others
/// (`docs/NOTES.md` §15.21). Nothing in a shipping kernel compiles it.
#[cfg(all(target_os = "none", feature = "selftest"))]
pub mod kunit;

pub use fs::FsStruct;
pub use policy::{ARGS_BYTES_MAX, ARGV_MAX, Priority, TaskId};
/// How many tasks are alive forever, which is what [`kunit`]'s wait-for-the-other
/// tasks helper compares against.
///
/// Gated on the test build, and that is the finding this step ended at: with the
/// `vfs` suite no longer waiting on the task count, every reader of this function
/// is inside `sched::kunit` - so it is not part of the kernel's surface and not
/// even part of a shipping kernel's code. It was a hook, and it is now gone from
/// production rather than merely `pub(crate)` (`docs/NOTES.md` §15.22).
#[cfg(all(target_os = "none", feature = "selftest"))]
pub(crate) use runtime::permanent_tasks;
#[cfg(target_os = "none")]
pub use runtime::spawn_init;
#[cfg(target_os = "none")]
pub use runtime::{
    ChildKind, CloneRequest, INIT_PID, ITIMER_PROF, ITIMER_REAL, ITIMER_VIRTUAL, Identity,
    ItimerSnapshot, ItimerState, MAX_TASKS, MapFacts, Redirect, SIGEV_NONE, SIGEV_SIGNAL,
    SIGEV_THREAD_ID, STACK_ORDER, Sent, SpawnError, Stop, TIMER_ABSTIME, TaskSnapshot, TimerNotify,
    WaitEnd, WaitEvent, WaitFor, WaitPark, acquire_controlling_tty, adopt_pid,
    arm_diskstat_boot_namespace, become_session_leader, block_until, block_until_child_event,
    block_until_code, block_until_deadline, block_until_interruptible,
    block_until_interruptible_rolling_back, brk, charge_cpu, check_itimer, child_event,
    child_event_pending, child_visible_to, claim_anon_gaps, clone, continued_report_pending,
    controlling_tty_of_current, credentials_of, current, current_blocked, current_comm_bytes,
    current_cpu_ticks, current_credentials, current_cwd, current_cwd_path, current_diskstat,
    current_disposition, current_exe, current_fds, current_fs_context, current_groups,
    current_identity, current_is_root, current_is_seccomp_strict, current_is_session_leader,
    current_map_user, current_net_ns, current_owner, current_pending, current_pgid,
    current_pid_number, current_real_owner, current_rlimit, current_root, current_stack_range,
    current_tgid, current_tls, current_umask, current_user_ns, current_uts, deliver_pending_signal,
    descriptor_access, descriptor_numbers, descriptors_of, detach_traced, discard_exit_code,
    exec_replace, exit, exit_code, exit_group, fork, frames_range_free, get_mmap_cursor,
    group_members, has_children, has_deliverable_signal, has_live_child, has_matching_children,
    has_seccomp_filter, has_tracer, identity_of, init, insert_vma, is_child, is_running,
    is_stopped, itimer_snapshot, itimer_swap, kernel_lazy_accessible, madvise_dontneed, map_anon,
    map_facts, map_frames, mapping_name_at, may_inspect, may_mint_pid_ns, membarrier_registrations,
    move_eager_ranges, move_user_page, munmap_user, nice_of, ns_of, oom_score_adj_of,
    parent_group_of, parent_of, pause, pgid_of, pid_number_of, pid_numbers_of,
    poll_any_child_event, poll_child_event, populate_user_page_at, preempt, process_members,
    protect_user, protect_user_range, read_user, read_user_of, record_eager_range,
    register_membarrier, release_controlling_tty, release_pid, release_user_frames,
    reset_disposition, resident_pages, resolve_anon_fault, resolve_cow_fault, resolve_pid,
    resolve_script, resolve_stack_growth, resume_traced, rlimits_of, robust_head, root_ns,
    run_seccomp_filters, send_signal, send_signal_group, send_signal_info, send_signal_process,
    session_of, session_of_tty, set_brk_base, set_clear_tid, set_current_blocked, set_current_comm,
    set_current_identity, set_current_rlimit, set_current_tls, set_current_umask, set_cwd,
    set_cwd_path, set_mmap_cursor, set_net_ns, set_nice, set_oom_score_adj, set_pgid,
    set_robust_head, set_root, set_seccomp_filter, set_seccomp_strict, set_signal_action,
    set_signal_alt_stack, set_trace_options, set_tracer, setns_mnt, setns_pid, setns_time,
    setns_user, setns_uts, signal_action, signal_alt_stack, sleep_ticks, snapshot, snapshots,
    space_root_of, spawn, spawn_permanent, spawn_user, started, stop_of, take_child_event,
    take_continued_report, take_deliverable_siginfo, take_deliverable_signal, take_exit_code,
    take_fatal_signal, take_pending_event, take_signal_in_set, take_stop_report, take_vmas,
    task_count, thread_group_has_siblings, thread_snapshots, time_offsets, timer_alloc, timer_arm,
    timer_overrun, timer_remove, timer_state, tls_of, trace_options, trace_syscall, tracer_of,
    try_spawn_user, uid_members, unmap_user, unshare_files, unshare_fs, unshare_mnt, unshare_pid,
    unshare_time, unshare_user, unshare_uts, user_frame_at, user_mapped, vfork, vma_gaps,
    vma_kind_at, vma_prot_at, vma_range_free, wait_any_child_event, wait_child_event,
    wait_for_interrupt, wake, with_current_credentials, write_user, write_user_of, yield_now,
};

/// The `wait(2)` doors read the whole retired record, not just its code: a
/// death's core-dump classification (`CLD_DUMPED`/`WCOREDUMP`) is the child's
/// `dumpable` flag at death, which no live task holds any more. Re-exported
/// `pub(crate)` because it names the crate-private record type.
#[cfg(target_os = "none")]
pub(crate) use runtime::take_retired_current;

/// The stop accessors and the register-file snapshot are the machine's to build
/// (`NOTES.md` §13): their `runtime` definitions carry a register file sized by
/// `hal::Contexts::ABI_REG_COUNT` and never look inside it - which register a
/// value is in stays the machine's conversion. So they build on both machines;
/// what is still x86_64's is the *syscall handler* that drives them, because
/// that is where the x86 gate frame's shape lives. On a machine with no such
/// handler nothing ever sets a tracer, and these answer honestly.
///
#[cfg(target_os = "none")]
pub use runtime::{
    exec_trap, fork_trap, group_stop, seccomp_trace_armed, seccomp_trap, set_trace_fpu,
    set_trace_regs, stop_at_syscall, trace_fpu, trace_regs, trace_signal_stop,
};

/// The `#NM` vector's entry, named only where the machine traps a lazy FPU.
#[cfg(mpos_fpu_trap)]
pub use runtime::device_not_available;

/// A host build has no task table and no namespaces, so every task is in the
/// init namespace - whose numbers *are* the slots. That makes both translations
/// the identity here, which is exactly what they are on a machine that never
/// unshares, and it is why these two are host stand-ins rather than cfg-fenced
/// call sites in `vfs` and `syscall`.
#[cfg(not(target_os = "none"))]
pub fn pid_number_of(slot: TaskId) -> Option<u32> {
    Some(slot.0 as u32)
}

/// The same stand-in for the chain a `/proc/<pid>/status` reader is shown: with
/// no namespaces there is one level, so the chain is the slot - the single number
/// a machine that never unshared prints, which is what the machine-side function
/// answers for one too.
#[cfg(not(target_os = "none"))]
pub fn pid_numbers_of(slot: TaskId) -> alloc::vec::Vec<u32> {
    alloc::vec![slot.0 as u32]
}

#[cfg(not(target_os = "none"))]
pub fn resolve_pid(number: i64) -> Option<TaskId> {
    (number > 0).then_some(TaskId(number as usize))
}

/// A host build has no processes, so no creator: root, which is what a node
/// with no owner model reports anyway.
#[cfg(not(target_os = "none"))]
pub fn current_owner() -> (u32, u32) {
    (0, 0)
}

/// A host build has one process and it has never unshared, so it stands in the
/// machine's own user namespace - the identity map, whose parent is nobody.
#[cfg(not(target_os = "none"))]
pub fn current_user_ns() -> alloc::sync::Arc<crate::userns::UserNs> {
    crate::vfs::ns::machine_user_ns()
}

/// `ns_capable(ns, cap)`: is the running caller privileged over the namespace
/// `ns`?
///
/// The question Linux asks of an *object* - a namespace being joined by
/// `setns`, or the network namespace a packet socket is opened in - and the one
/// [`crate::cred::Credentials::has_effective`] cannot answer, because it is
/// asked of the caller's own namespace where this is asked of the object's.
/// The walk, its four clauses and the measured table are
/// [`crate::userns::Caller::capable_in`]'s; this is the three facts it needs,
/// read off the running task.
pub fn ns_capable_of(ns: &alloc::sync::Arc<crate::userns::UserNs>, cap: u32) -> bool {
    crate::userns::Caller {
        ns: current_user_ns(),
        euid: current_owner().0,
        effective: current_credentials().effective,
    }
    .capable_in(ns, cap)
}

/// A host build has no task table, so a process's credentials are one value -
/// the state a fresh process starts in. That is exactly what the `prctl`,
/// `capget`/`capset` and ptrace unit tests want to manipulate, and it reduces
/// to the machine's behaviour when there is one process.
#[cfg(not(target_os = "none"))]
static HOST_CREDENTIALS: critical_section::Mutex<core::cell::RefCell<crate::cred::Credentials>> =
    critical_section::Mutex::new(core::cell::RefCell::new(crate::cred::Credentials::initial()));

#[cfg(not(target_os = "none"))]
pub fn current_credentials() -> crate::cred::Credentials {
    critical_section::with(|cs| *HOST_CREDENTIALS.borrow(cs).borrow())
}

#[cfg(not(target_os = "none"))]
pub fn with_current_credentials<R>(f: impl FnOnce(&mut crate::cred::Credentials) -> R) -> R {
    critical_section::with(|cs| f(&mut HOST_CREDENTIALS.borrow(cs).borrow_mut()))
}

/// On the host there is one process, and it is the caller: every pid names it,
/// so the answer is "yes, live" and the policy folds to the caller inspecting
/// itself - which `ptrace_may_access` allows unconditionally.
#[cfg(not(target_os = "none"))]
pub fn may_inspect(_target: TaskId) -> Option<bool> {
    Some(true)
}

/// The same absence for the *real* ids, which is the pair `access(2)` asks
/// about: with no task table there is no identity, and root is the answer a
/// node with no owner model gives anyway.
#[cfg(not(target_os = "none"))]
pub fn current_real_owner() -> (u32, u32) {
    (0, 0)
}

/// A host build has one process, so its creation mask is one value - which is
/// exactly what a per-process mask reduces to here, and what lets the `umask`
/// tests read one call's answer back from the next.
#[cfg(not(target_os = "none"))]
static HOST_UMASK: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(crate::abi::DEFAULT_UMASK);

#[cfg(not(target_os = "none"))]
pub fn current_umask() -> u32 {
    HOST_UMASK.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_os = "none"))]
pub fn set_current_umask(mask: u32) {
    HOST_UMASK.store(mask, core::sync::atomic::Ordering::Relaxed);
}

/// Taking turns on the host build's process-global test state, for tests: the
/// creation mask above (and the strict-mode latch below) is one value for the
/// whole process, so a test that *sets* it races any test that reads a node's
/// birth mode through it - and the two run in parallel threads in one binary.
/// The crate has no `std` and the host build has no `spin`, so this is a lock
/// made of one atomic, which is all a test needs to keep another test's mask
/// out of its own.
#[cfg(all(test, not(target_os = "none")))]
pub mod test_lock {
    use core::sync::atomic::{AtomicBool, Ordering};

    static LOCK: AtomicBool = AtomicBool::new(false);

    pub struct Held;

    pub fn take() -> Held {
        while LOCK.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Held
    }

    impl Drop for Held {
        fn drop(&mut self) {
            LOCK.store(false, Ordering::Release);
        }
    }
}

/// A host build has one process and no dispatch to guard, but `prctl` and
/// `seccomp` are host-testable, so the strict-mode latch gets the same
/// single-value home the creation mask does: one way on, never off, so the
/// accessors can be checked against the state they are asked to keep.
#[cfg(not(target_os = "none"))]
static HOST_SECCOMP_STRICT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(not(target_os = "none"))]
pub fn current_is_seccomp_strict() -> bool {
    HOST_SECCOMP_STRICT.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_os = "none"))]
pub fn set_seccomp_strict() {
    HOST_SECCOMP_STRICT.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// The host build's filter list, the same one-value stand-in the strict latch
/// above uses. It is not a convenience: a filter's *validation* - which
/// instructions are known, which jumps land inside the program, whether it ends
/// by returning - is arithmetic over the caller's bytes, and the install path
/// that copies those bytes out of a `sock_fprog` is the same code on both
/// targets. Without this the whole install path would be kernel-only and no
/// host test could argue with it.
#[cfg(not(target_os = "none"))]
static HOST_SECCOMP_FILTERS: critical_section::Mutex<
    core::cell::RefCell<alloc::vec::Vec<crate::seccomp::Program>>,
> = critical_section::Mutex::new(core::cell::RefCell::new(alloc::vec::Vec::new()));

#[cfg(not(target_os = "none"))]
pub fn has_seccomp_filter() -> bool {
    critical_section::with(|cs| !HOST_SECCOMP_FILTERS.borrow(cs).borrow().is_empty())
}

#[cfg(not(target_os = "none"))]
pub fn set_seccomp_filter(program: crate::seccomp::Program) {
    crate::seccomp::arm();
    critical_section::with(|cs| HOST_SECCOMP_FILTERS.borrow(cs).borrow_mut().push(program));
}

#[cfg(not(target_os = "none"))]
pub fn run_seccomp_filters(data: &[u8; crate::seccomp::DATA_BYTES]) -> Option<u32> {
    critical_section::with(|cs| {
        let filters = HOST_SECCOMP_FILTERS.borrow(cs).borrow();
        if filters.is_empty() {
            return None;
        }
        let mut answer = crate::seccomp::RET_ALLOW;
        for program in filters.iter() {
            answer = crate::seccomp::more_restrictive(answer, program.run(data));
        }
        Some(answer)
    })
}

/// A host build's one process holds only the group it runs as, and the harness
/// runs as root - so every file's group is one it holds, which is what the
/// `chmod`/`chown` rules need to be told.
#[cfg(not(target_os = "none"))]
pub fn current_groups() -> [u32; 3] {
    [0, 0, 0]
}

/// Without the runtime there is no task table to keep a `membarrier`
/// registration in, and no threads for a barrier to be about: the host build of
/// the ABI is a test harness. Zero - "this process has registered nothing" - is
/// both the honest answer and the one that lets the handler's refusal of an
/// unregistered expedited barrier be tested.
#[cfg(not(target_os = "none"))]
pub fn membarrier_registrations() -> usize {
    0
}

/// The same absence, for the descriptor table `/proc/<pid>/fd` is listed from:
/// with no task table, no process has anything open. The machine build reads
/// the real table in `runtime`.
#[cfg(not(target_os = "none"))]
pub fn descriptor_numbers(_pid: usize) -> alloc::vec::Vec<usize> {
    alloc::vec::Vec::new()
}

/// And for the direction `/proc/<pid>/fd/<N>`'s own mode is derived from: no
/// descriptor is open, so no direction is recorded. The machine build reads the
/// live description in `runtime`.
#[cfg(not(target_os = "none"))]
pub fn descriptor_access(_pid: usize, _fd: usize) -> Option<u32> {
    None
}

#[cfg(not(target_os = "none"))]
pub fn register_membarrier(_barrier: usize) {}

/// The robust-list head, with no task table to keep it on. The host build is a
/// single-process test harness, so one slot stands for "the current process"
/// and lets `set_robust_list`/`get_robust_list` round-trip under test; the
/// machine build reads and writes the real per-group head in `runtime`.
#[cfg(not(target_os = "none"))]
static HOST_ROBUST_HEAD: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[cfg(not(target_os = "none"))]
pub fn robust_head() -> usize {
    HOST_ROBUST_HEAD.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_os = "none"))]
pub fn set_robust_head(head: usize) {
    HOST_ROBUST_HEAD.store(head, core::sync::atomic::Ordering::Relaxed);
}

/// The `PR_SET_NAME` slot with no task table to keep it on. The host build is
/// a single-process test harness, so one 16-byte array stands for "the current
/// process's comm" and lets `PR_SET_NAME` be read back by `PR_GET_NAME` under
/// test; the machine build reads and writes the group's real slot in `runtime`.
#[cfg(not(target_os = "none"))]
static HOST_COMM: [core::sync::atomic::AtomicU8; 16] =
    [const { core::sync::atomic::AtomicU8::new(0) }; 16];

#[cfg(not(target_os = "none"))]
pub fn set_current_comm(name: &[u8]) {
    for i in 0..16 {
        let byte = match name.get(i).copied() {
            Some(0) | None => 0,
            Some(b) => b,
        };
        HOST_COMM[i].store(byte, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(target_os = "none"))]
pub fn current_comm_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    for (slot, cell) in out.iter_mut().zip(HOST_COMM.iter()) {
        *slot = cell.load(core::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// With no timer interrupt on the host there is nothing to charge, and no group
/// to charge it to. `current_cpu_ticks` answers `(0, 0)`; the machine build reads
/// the running group's real counters in `runtime`.
#[cfg(not(target_os = "none"))]
pub fn charge_cpu(_in_user: bool) {}

#[cfg(not(target_os = "none"))]
pub fn current_cpu_ticks() -> (u64, u64, u64, u64) {
    (0, 0, 0, 0)
}

/// The host build's one process, so one slot stands for "the running task's
/// mount namespace" — the same arrangement `HOST_UTS` below uses for the names.
/// A host test that unshares gets a copy of the table it was in and can see the
/// copy is independent; nothing on the host drives a per-task table, because the
/// VFS tests resolve through explicit `MountTable` values instead.
#[cfg(not(target_os = "none"))]
static HOST_MNT: critical_section::Mutex<
    core::cell::RefCell<Option<alloc::sync::Arc<crate::vfs::mount::MountTable>>>,
> = critical_section::Mutex::new(core::cell::RefCell::new(None));

/// The host build's mount context: `(table, root)`, with the machine's own
/// table standing in until something unshares. The root half answers the
/// table's own root — there is no per-process chroot to read on the host.
/// The host build's mount context. `None` for the table: a host resolution has
/// no per-task namespace to consult, and every host caller already drives an
/// explicit `MountTable` value — which is exactly what `process_fs_context`'s
/// `None` arm means there ("this table"), so the same answer keeps the walk
/// honest on both targets. A host test that wants to see a namespace copy works
/// on two `MountTable` values directly (`MountTable::copy`), which is the shape
/// [`two_tables_do_not_see_each_others_mounts`] pins.
///
/// [`two_tables_do_not_see_each_others_mounts`]: crate::vfs::mount::MountTable::copy
#[cfg(not(target_os = "none"))]
pub fn current_fs_context() -> (
    Option<alloc::sync::Arc<crate::vfs::mount::MountTable>>,
    Option<alloc::sync::Arc<dyn crate::vfs::Inode>>,
) {
    (None, None)
}

/// The one-process form of `unshare(CLONE_NEWNS)` on the host: the process
/// moves to a copy of the table it was in, held in `HOST_MNT`. No host test
/// drives it today — the VFS tests pass tables explicitly — but it exists so
/// the machine body and the host body of this pair both compile (V-11's rule:
/// a module with two bodies has both move).
#[cfg(not(target_os = "none"))]
pub fn unshare_mnt() {
    critical_section::with(|cs| {
        let mut slot = HOST_MNT.borrow(cs).borrow_mut();
        let owner = current_user_ns();
        let copied = match &*slot {
            Some(table) => table.copy(owner),
            None => crate::vfs::mount::global().copy(owner),
        };
        *slot = Some(copied);
    });
}

/// The host build's one process, so one slot stands for "the running task's
/// namespace" and `unshare_uts` is observable without a task table - the same
/// arrangement the `comm` slot and the creation mask above use. It starts at
/// `uts::init()`, the namespace a boot's first task holds on the machine, so a
/// host test and the machine agree about where the names come from.
#[cfg(not(target_os = "none"))]
static HOST_UTS: critical_section::Mutex<
    core::cell::RefCell<Option<alloc::sync::Arc<crate::uts::Uts>>>,
> = critical_section::Mutex::new(core::cell::RefCell::new(None));

/// Whatever namespace the host build's single process is in. A host test that
/// names it is naming the process the whole host build *is*, which is what makes
/// the round trip through `sethostname`/`uname` testable at all; the machine
/// build reads the running task's in `runtime`.
#[cfg(not(target_os = "none"))]
pub fn current_uts() -> alloc::sync::Arc<crate::uts::Uts> {
    critical_section::with(|cs| {
        HOST_UTS
            .borrow(cs)
            .borrow_mut()
            .get_or_insert_with(|| crate::uts::init(crate::vfs::ns::mint()))
            .clone()
    })
}

/// The host build's diskstat answer: never armed, because no host build boots
/// with a disk to count and arming is a boot-step fact. The pair with
/// `runtime`'s bare-metal body must both compile (V-11).
#[cfg(not(target_os = "none"))]
pub fn current_diskstat() -> bool {
    false
}

/// The host build's arm step: nothing to arm on a machine with no task table
/// and no disk; see the bare-metal body for what it does there (V-11's rule:
/// a module with two bodies has both move).
#[cfg(not(target_os = "none"))]
pub fn arm_diskstat_boot_namespace() {}

/// The one-process form of `unshare(CLONE_NEWUTS)`: the host build's process
/// moves to a copy of the namespace it was in, so a test can see the copy is
/// independent. Done in one critical section - the copy's own `critical_section`
/// is the reentrant kind on the host and a counting disable on the machine -
/// because the slot must not be read and written across a gap.
#[cfg(not(target_os = "none"))]
pub fn unshare_uts() {
    critical_section::with(|cs| {
        let mut slot = HOST_UTS.borrow(cs).borrow_mut();
        let fresh = slot
            .get_or_insert_with(|| crate::uts::init(crate::vfs::ns::mint()))
            .copied(crate::vfs::ns::mint());
        *slot = Some(fresh);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule `unshare(CLONE_NEWUTS)` rests on, on the one-process build where
    /// it is observable without a task table: the caller keeps the names it had,
    /// and a `sethostname` afterwards moves only the caller.
    ///
    /// A single test because the two halves are one fact - a copy that lost the
    /// names would pass the second half and fail the first, and a "copy" that was
    /// really the same object would pass the first and fail the second.
    #[test]
    fn unshare_gives_a_copy_of_the_names_and_not_the_names_themselves() {
        let before = current_uts();
        before.set_nodename("host");
        before.set_domainname("example.test");

        unshare_uts();

        let after = current_uts();
        assert!(
            !alloc::sync::Arc::ptr_eq(&before, &after),
            "no copy was made"
        );
        assert_eq!(
            after.nodename(),
            "host",
            "the copy did not inherit the name"
        );
        assert_eq!(after.domainname(), "example.test");

        after.set_nodename("inside");
        assert_eq!(after.nodename(), "inside");
        assert_eq!(
            before.nodename(),
            "host",
            "naming the caller renamed the namespace it came from"
        );

        // Leave the process in an unnamed namespace so a later test in the same
        // binary does not read a name this one invented.
        after.set_nodename("");
        after.set_domainname("");
    }
}
