//! Task scheduling.
//!
//! Split the way the rest of the kernel is: `policy` decides which task runs
//! next and knows nothing about registers, so it is unit-tested on the host. The
//! runtime below it owns the task table and the stacks, and calls into
//! `arch::context` to move between them.

pub mod exit;
pub mod futex;
pub mod policy;
pub mod signal;

#[cfg(target_os = "none")]
mod runtime;

pub use policy::{ARGS_BYTES_MAX, ARGV_MAX, Priority, TaskId};
#[cfg(target_os = "none")]
pub use runtime::spawn_init;
#[cfg(target_os = "none")]
pub use runtime::{
    CloneRequest, INIT_PID, ITIMER_PROF, ITIMER_REAL, ITIMER_VIRTUAL, Identity, ItimerSnapshot,
    ItimerState, MAX_TASKS, MapFacts, Redirect, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD_ID,
    STACK_ORDER, SpawnError, Stop, TIMER_ABSTIME, TaskSnapshot, TimerNotify, WaitEnd, WaitEvent,
    WaitPark, acquire_controlling_tty, become_session_leader, block_until, block_until_child_event,
    block_until_code, block_until_deadline, block_until_interruptible, brk, charge_cpu,
    check_itimer, child_event, child_event_pending, child_exited_unreaped, claim_anon_gaps, clone,
    continued_report_pending, controlling_tty_of_current, credentials_of, current, current_blocked,
    current_comm_bytes, current_cpu_ticks, current_credentials, current_cwd, current_cwd_path,
    current_disposition, current_exe, current_fds, current_groups, current_identity,
    current_is_root, current_is_seccomp_strict, current_map_user, current_owner, current_pending,
    current_pgid, current_real_owner, current_rlimit, current_stack_range, current_tgid,
    current_tls, current_umask, current_uts, deliver_pending_signal, descriptor_numbers,
    descriptors_of, detach_traced, device_not_available, discard_exit_code, exec_replace,
    exec_trap, exit, exit_code, exit_group, fork, fork_trap, get_mmap_cursor, group_members,
    group_stop, has_children, has_deliverable_signal, has_live_child, has_seccomp_filter,
    has_tracer, init, insert_vma, is_child, is_stopped, itimer_snapshot, itimer_swap,
    kernel_lazy_accessible, madvise_dontneed, map_anon, map_facts, map_frames, may_inspect,
    membarrier_registrations, move_user_page, munmap_user, nice_of, oom_score_adj_of, parent_of,
    pause, permanent_tasks, pgid_of, poll_any_child, poll_any_child_event, populate_user_page_at,
    preempt, process_members, protect_user, protect_user_range, read_sigchld, read_user,
    read_user_of, record_eager_range, register_membarrier, release_user_frames, reset_disposition,
    resolve_anon_fault, resolve_cow_fault, resolve_script, resolve_stack_growth, resume_traced,
    rlimits_of, robust_head, run_seccomp_filters, send_signal, send_signal_group, send_signal_info,
    send_signal_process, session_of_tty, set_brk_base, set_clear_tid, set_current_blocked,
    set_current_comm, set_current_identity, set_current_rlimit, set_current_tls, set_current_umask,
    set_cwd, set_cwd_path, set_mmap_cursor, set_nice, set_oom_score_adj, set_pgid, set_robust_head,
    set_seccomp_filter, set_seccomp_strict, set_signal_action, set_signal_alt_stack, set_trace_fpu,
    set_trace_options, set_trace_regs, set_tracer, signal_action, signal_alt_stack, sleep_ticks,
    snapshot, snapshots, space_root_of, spawn, spawn_permanent, spawn_user, started,
    stop_at_syscall, stop_of, take_child_event, take_continued_report, take_deliverable_siginfo,
    take_deliverable_signal, take_exit_code, take_exit_code_current, take_fatal_signal,
    take_pending_event, take_signal_in_set, take_stop_report, take_vmas, task_count,
    thread_snapshots, timer_alloc, timer_arm, timer_overrun, timer_remove, timer_state, tls_of,
    trace_fpu, trace_options, trace_regs, trace_signal_stop, trace_syscall, tracer_of,
    try_spawn_user, uid_members, unmap_user, unshare_uts, user_frame_at, user_mapped,
    user_range_free, vfork, vma_gaps, vma_prot_at, wait_any_child, wait_any_child_event,
    wait_for_interrupt, wake, with_current_credentials, write_user, write_user_of, yield_now,
};

/// A host build has no processes, so no creator: root, which is what a node
/// with no owner model reports anyway.
#[cfg(not(target_os = "none"))]
pub fn current_owner() -> (u32, u32) {
    (0, 0)
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
    core::sync::atomic::AtomicU32::new(crate::syscall::DEFAULT_UMASK);

#[cfg(not(target_os = "none"))]
pub fn current_umask() -> u32 {
    HOST_UMASK.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(target_os = "none"))]
pub fn set_current_umask(mask: u32) {
    HOST_UMASK.store(mask, core::sync::atomic::Ordering::Relaxed);
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
            .get_or_insert_with(crate::uts::init)
            .clone()
    })
}

/// The one-process form of `unshare(CLONE_NEWUTS)`: the host build's process
/// moves to a copy of the namespace it was in, so a test can see the copy is
/// independent. Done in one critical section - the copy's own `critical_section`
/// is the reentrant kind on the host and a counting disable on the machine -
/// because the slot must not be read and written across a gap.
#[cfg(not(target_os = "none"))]
pub fn unshare_uts() {
    critical_section::with(|cs| {
        let mut slot = HOST_UTS.borrow(cs).borrow_mut();
        let fresh = slot.get_or_insert_with(crate::uts::init).copied();
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
