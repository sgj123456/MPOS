//! Task scheduling.
//!
//! Split the way the rest of the kernel is: `policy` decides which task runs
//! next and knows nothing about registers, so it is unit-tested on the host. The
//! runtime below it owns the task table and the stacks, and calls into
//! `arch::context` to move between them.

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
    ItimerState, MAX_TASKS, Redirect, STACK_ORDER, SpawnError, Stop, TaskSnapshot, WaitEnd,
    WaitEvent, acquire_controlling_tty, become_session_leader, block_until,
    block_until_child_event, block_until_code, block_until_deadline, brk, check_itimer,
    child_event, child_event_pending, child_exited_unreaped, clone, continued_report_pending,
    controlling_tty_of_current, current, current_blocked, current_cwd, current_cwd_path,
    current_disposition, current_exe, current_fds, current_identity, current_map_user,
    current_pending, current_pgid, current_tgid, current_tls, deliver_pending_signal,
    descriptors_of, detach_traced, discard_exit_code, exec_replace, exec_trap, exit, exit_code,
    exit_group, fork, fork_trap, get_mmap_cursor, group_members, group_stop, has_children,
    has_live_child, has_tracer, init, is_child, is_stopped, itimer_snapshot, itimer_swap,
    move_user_page, parent_of, pause, permanent_tasks, pgid_of, poll_any_child,
    poll_any_child_event, preempt, protect_user, read_sigchld, read_user, read_user_of,
    release_user_frames, reset_disposition, resolve_cow_fault, resolve_script,
    resolve_stack_growth, resume_traced, send_signal, send_signal_group, set_brk_base,
    set_clear_tid, set_current_blocked, set_current_identity, set_current_tls, set_cwd,
    set_cwd_path, set_mmap_cursor, set_pgid, set_signal_action, set_signal_alt_stack,
    set_trace_fpu, set_trace_options, set_trace_regs, set_tracer, signal_action, signal_alt_stack,
    sleep_ticks, snapshot, snapshots, space_root_of, spawn, spawn_permanent, spawn_user,
    stop_at_syscall, stop_of, take_child_event, take_continued_report, take_deliverable_signal,
    take_exit_code, take_fatal_signal, take_pending_event, take_signal_in_set, take_stop_report,
    task_count, thread_snapshots, tls_of, trace_fpu, trace_options, trace_regs, trace_signal_stop,
    trace_syscall, tracer_of, try_spawn_user, unmap_user, user_frame_at, user_range_free, vfork,
    wait_any_child, wait_any_child_event, wait_for_interrupt, wake, write_user, write_user_of,
    yield_now,
};
