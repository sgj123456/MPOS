//! Task scheduling.
//!
//! Split the way the rest of the kernel is: `policy` decides which task runs
//! next and knows nothing about registers, so it is unit-tested on the host. The
//! runtime below it owns the task table and the stacks, and calls into
//! `arch::context` to move between them.

pub mod policy;
pub mod signal;

#[cfg(target_os = "none")]
mod runtime;

pub use policy::{Priority, TaskId};
#[cfg(target_os = "none")]
pub use runtime::spawn_init;
#[cfg(target_os = "none")]
pub use runtime::{
    ARGS_BYTES_MAX, ARGV_MAX, INIT_PID, MAX_TASKS, Redirect, STACK_ORDER, SpawnError, block_until,
    block_until_code, brk, current, current_cwd, current_cwd_path, current_exe, current_fds,
    current_map_user, deliver_pending_signal, discard_exit_code, exit, exit_code, exec_replace,
    fork, get_mmap_cursor, has_live_child, init, is_child, move_user_page, parent_of, pause,
    permanent_tasks, poll_any_child, preempt, protect_user, release_user_frames, resolve_cow_fault,
    send_signal, set_brk_base, set_cwd, set_cwd_path, set_mmap_cursor, set_signal_action,
    set_signal_alt_stack, signal_action, signal_alt_stack, sleep_ticks, spawn, spawn_user,
    take_exit_code, task_count, try_spawn_user, unmap_user, user_frame_at, user_range_free,
    wait_any_child, wait_for_interrupt, wake, yield_now,
};
