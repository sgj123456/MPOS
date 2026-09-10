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

#[cfg(target_os = "none")]
pub use policy::{Priority, TaskId};
#[cfg(target_os = "none")]
pub use runtime::{
    ARGS_BYTES_MAX, ARGV_MAX, MAX_TASKS, STACK_ORDER, SpawnError, block_until, current, exit,
    exit_code, init, is_child, pause, permanent_tasks, preempt, send_signal, sleep_ticks, spawn,
    spawn_user, task_count, try_spawn_user, wake, yield_now,
};
