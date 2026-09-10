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
    ARGS_BYTES_MAX, ARGV_MAX, INIT_PID, MAX_TASKS, STACK_ORDER, SpawnError, block_until,
    block_until_code, current, current_fds, deliver_pending_signal, discard_exit_code, exit,
    exit_code, has_live_child, init, is_child, pause, permanent_tasks, preempt, send_signal,
    sleep_ticks, spawn, spawn_user, take_exit_code, task_count, try_spawn_user, wait_any_child,
    wait_for_interrupt, wake, yield_now,
};
