//! The task table, the stacks tasks run on, and the scheduler task that runs
//! between them.
//!
//! # Two invariants, both about aliasing
//!
//! Tasks switch *inside* the interrupts-off window that guards the table, so a
//! `&mut Table` that outlived a switch would be aliased by the next task's own
//! `&mut Table`. Two `&mut` to one value is undefined behaviour even if only one
//! of them is running at a time, so no borrow here is ever alive across
//! [`context::switch`]. Every access is either inside a [`with_table`] closure,
//! which cannot switch, or the single statement at one of the two switch sites,
//! where the borrow is a temporary that dies at the semicolon.
//!
//! A [`Context`] must never move either, because a suspended task's saved stack
//! pointer lives inside it and the switch is handed a pointer to it. That is why
//! the table is a fixed-size array: growing a `Vec` would relocate every
//! suspended task's context.
//!
//! # Why everything goes through the scheduler task
//!
//! A task that gives up the CPU switches to the scheduler, which then picks the
//! next task, rather than switching to it directly. The extra hop is what makes
//! reaping possible: a task cannot free the stack it is standing on, so the one
//! that exits hands over and the scheduler buries it from a stack of its own.

mod child_exit;
mod child_mm;
mod child_signal;
mod load;
mod pid;
mod procfs;
mod task;
mod trace;

pub use child_exit::{
    CloneRequest, block_until_child_event, child_event, child_event_pending, child_exited_unreaped,
    clone, current_tgid, discard_exit_code, exec_replace, exit, exit_code, exit_group, fork,
    has_matching_children, poll_any_child_event, poll_child_event, read_sigchld, read_user,
    read_user_of, set_clear_tid, space_root_of, take_child_event, take_exit_code,
    take_exit_code_current, tls_of, vfork, wait_any_child_event, wait_child_event, write_user,
    write_user_of,
};
pub use child_mm::*;
pub use child_signal::*;
pub use load::{init, resolve_script, spawn, spawn_init, spawn_user, try_spawn_user};
pub use pid::*;
pub use procfs::*;
pub use task::*;
pub use trace::*;

use crate::sched::policy::TaskId;
use core::sync::atomic::{AtomicBool, AtomicUsize};

/// Task stacks are `2^STACK_ORDER` pages. The kernel stack has to hold the trap
/// frames that stack up above a task the clock preempted *and* whatever the
/// deepest syscall path needs - and `execve` is by far the deepest: it reads a
/// whole program image, hands it to a parser, and builds a new address space
/// underneath all of that.
///
/// The figure depends on the build, because that path's frame sizes do. A
/// release image gets Linux's own 16 KiB (`STACK_ORDER` 2), which is what the
/// 64 ring-3 programs in the image are booted and measured against. A debug
/// image keeps 64 KiB: unoptimised frames are several times larger, and 16 KiB
/// was measured to be too few for `execve` there. The symptom of getting it
/// wrong is the worst kind - the overflow eats the fault handler's own frame,
/// so the machine triple-faults with nothing said - which is why the two
/// figures are the ones that have been booted rather than the ones that look
/// smallest.
#[cfg(debug_assertions)]
pub const STACK_ORDER: usize = 4;
#[cfg(not(debug_assertions))]
pub const STACK_ORDER: usize = 2;
pub(crate) const STACK_SIZE: usize = crate::mm::PAGE_SIZE << STACK_ORDER;

/// Written at the bottom of every stack, which is where a runaway task arrives
/// last, and checked when the task is reaped. It catches overflow after the fact
/// rather than preventing it; preventing it needs a guard page, and the direct
/// map is not something to punch holes in for one task's benefit.
pub(crate) const STACK_CANARY: u64 = 0x6d70_6f73_5f73_746b;

/// Whether any task may have an `ITIMER_REAL` armed.
///
/// The hot-path half of [`Table::check_itimers`]: the scan of all 64 slots runs
/// on every scheduler turn and every clock tick, and all but a handful of
/// programs in the image never arm a timer at all. Set here by the one thing
/// that arms one ([`itimer_swap`]) and cleared by the scan itself when it finds
/// none - so it can be wrong only in the direction of "run the scan", never in
/// the direction of "skip a timer that is due".
///
/// Only `ITIMER_REAL` is tracked, because only `ITIMER_REAL` is a wall-clock
/// deadline the scheduler has to honour; the virtual and profiling timers are
/// charged against consumed CPU time and are checked where that time is counted.
pub(crate) static ITIMER_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// The same flag for the `timer_create` family, and for the same reason: the
/// scan in [`Table::check_posix_timers`] is skipped entirely while no task has
/// a POSIX timer armed, which is the state of a machine running anything but
/// the program that asked for one. Set by the arm call, cleared by the scan
/// when it finds none - wrong only in the safe direction, "run the scan".
pub(crate) static POSIX_TIMERS_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Slot 0 belongs to whatever was running when [`init`] was called. It adopts
/// that stack instead of getting one, so there is nothing of ours to free.
pub(crate) const BOOT_SLOT: usize = 0;

/// The next slot after the boot task's. Slots below it stay empty rather than
/// first come, first served, so a pid never depends on how many tasks the
/// boot code happened to start: slot 1 is reserved for init, and the tests
/// that ran before it still get pids they can assert on.
///
/// The top slot belongs to the scheduler, which is never queued and never
/// exits; tasks reach it by switching to it. Why the top rather than 1: the
/// first process the kernel starts is init, and a pid is a slot number, so
/// init has to land on 1 - which is free exactly because the scheduler sits
/// out of the way at the far end. See [`Table::alloc_slot`].
pub(crate) const FIRST_ALLOC_SLOT: usize = 2;
pub(crate) const SCHED_ID: TaskId = TaskId(MAX_TASKS - 1);

/// The first user process the kernel starts: init, and the parent every
/// orphan ends up with.
pub const INIT_PID: TaskId = TaskId(1);

/// Permanent tasks: the boot task and the scheduler. Anything above this count
/// is a task the tests started and have not finished reaping.
/// The two tasks that are alive from the beginning: the boot task and the
/// scheduler. Only [`permanent_tasks`] reads it, and that is a test-build door -
/// so this is a test-build constant, and nothing in a shipping kernel counts
/// permanent tasks at all (`docs/NOTES.md` §15.22).
#[cfg(feature = "selftest")]
pub(crate) const PERMANENT_TASKS: usize = 2;

/// Tasks started as permanent since boot - see [`spawn_permanent`]. Kept apart
/// from the constant because it changes at runtime, and [`permanent_tasks`] is
/// the sum of the two.
pub(crate) static PERMANENT_EXTRA: AtomicUsize = AtomicUsize::new(0);

/// Set once [`init`] has built the table. Before that the clock still ticks but
/// [`preempt`] has nothing to switch to and must leave the running code alone,
/// which is what lets every milestone before this one keep working unchanged.
pub(crate) static STARTED: AtomicBool = AtomicBool::new(false);

pub(crate) static TABLE: critical_section::Mutex<Shared> =
    critical_section::Mutex::new(Shared::new(Table::new()));

/// Where a user stack ends. Programs are linked around 0x400000, so 1 GiB
/// leaves the whole image below it and everything in between for a heap once
/// user memory grows one (M11+). The stack itself grows down from here.
pub(crate) const USER_STACK_TOP: usize = 0x4000_0000;
/// How much of the stack exists before the program runs. This is where a stack
/// *starts*, not how big one is: everything below it is mapped on first touch,
/// up to [`USER_STACK_MAX`].
pub(crate) const USER_STACK_PAGES: usize = 16;
/// How far down the main stack may grow. Linux's default `RLIMIT_STACK`, and
/// the whole reason a program may call deeper than any fixed reservation: the
/// region extends as it is used.
pub(crate) const USER_STACK_MAX: usize = 8 * 1024 * 1024;

/// How far below [`USER_STACK_TOP`] a program's stack may start, in pages.
///
/// This is the one randomisation range that is *not* Linux's number: Linux
/// bounds the stack's movement by a fraction of the machine's memory and of
/// `RLIMIT_STACK`, so the range is a property of the box rather than of the
/// ABI. Measured over 64 runs here it came out at about 16 GiB, which this
/// kernel's layout cannot imitate - the stack top is 1 GiB and the program sits
/// below it. Eight MiB is the largest movement that keeps the whole stack clear
/// of the image no matter how far `RLIMIT_STACK` lets it grow.
pub(crate) const ASLR_STACK_PAGES: usize = 1 << 11;

/// How far below its landmark a randomised load address or `mmap` region may
/// sit, in pages: a tebibyte's worth.
///
/// The number comes from measuring Linux rather than from rounding: over 64
/// runs of the same position-independent binary, its code base, its `mmap`
/// region and its heap anchor each moved across a range of this order, and the
/// same program linked `-no-pie` moved its stack and `mmap` region but kept its
/// code address at `0x401156` every time. Matching that last half is the point:
/// randomising a fixed-address image's placement would break it, because the
/// addresses in its own instructions are the ones it was linked to run at.
pub(crate) const ASLR_REGION_PAGES: usize = 1 << 28;
