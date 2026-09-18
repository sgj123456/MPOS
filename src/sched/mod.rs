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
    CloneRequest, INIT_PID, ITIMER_PROF, ITIMER_REAL, ITIMER_VIRTUAL,
    ItimerSnapshot, ItimerState, MAX_TASKS, Redirect, STACK_ORDER, SpawnError, TaskSnapshot,
    WaitEnd,
    block_until,
    block_until_code, block_until_deadline, brk, check_itimer, child_exited_unreaped, clone, current, current_blocked,
    current_cwd, current_cwd_path, current_disposition, current_exe, current_fds, current_map_user,
    current_pgid, current_tgid, current_tls, deliver_pending_signal,
    descriptors_of,
    discard_exit_code,
    read_sigchld,
    snapshot, snapshots, thread_snapshots,
    exec_replace, exit, exit_code, exit_group, fork, get_mmap_cursor, group_members, has_children,
    has_live_child, init, is_child, itimer_snapshot, itimer_swap, move_user_page, parent_of, pause,
    permanent_tasks, pgid_of, poll_any_child, preempt, protect_user, read_user,
    release_user_frames, reset_disposition, resolve_cow_fault, resolve_script,
    resolve_stack_growth, send_signal, send_signal_group, set_brk_base, set_clear_tid,
    set_current_blocked, set_current_tls, set_cwd, set_cwd_path, set_mmap_cursor, set_pgid,
    set_signal_action, set_signal_alt_stack, signal_action, signal_alt_stack, sleep_ticks, spawn,
    spawn_permanent, spawn_user, take_deliverable_signal, take_exit_code,
    take_fatal_signal,
    take_signal_in_set, task_count, try_spawn_user, unmap_user, user_frame_at, user_range_free,
    wait_any_child, wait_for_interrupt, wake, write_user, yield_now,
};
