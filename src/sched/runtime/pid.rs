//! The PID namespace's scheduler half: whose namespace a task is in, and the
//! two directions of the translation between a number and a slot.
//!
//! The object itself is the `pidns` leaf (`crates/pidns`), which knows a slot
//! from a number and nothing about tasks. What is here is everything that needs
//! the task table: which namespace the running task is in, which one its
//! children are created in, and the answers the syscall layer asks for -
//! "what number does this task have where I am standing" and "which task is
//! this number where I am standing".
//!
//! # Two fields, because Linux has two
//!
//! A task carries [`Task::pid_ns`] (the namespace its *own* numbers come from)
//! and [`Task::pid_ns_children`] (the one its children are created in). They are
//! separate for exactly one reason, and it is the whole shape of the call:
//! `unshare(CLONE_NEWPID)` replaces the second and not the first, so the caller
//! keeps the number it had (`getpid` is unchanged) while the next child is 1 in
//! a namespace of its own. One field could not say that: a caller that replaced
//! its own namespace would have to be registered in the new one to have a
//! `getpid` at all, which is the opposite of what the call does. Linux keeps the
//! same pair (`nsproxy->pid_ns_for_children` and the namespace the task's own
//! `struct pid` was allocated in).
//!
//! # A number is not a slot
//!
//! Every translation here is one-way at a time and answers `Option`, because the
//! failure is the point: a number that means nothing *in the reader's namespace*
//! is `ESRCH` (or a `None` a caller turns into one), and the alternative - the
//! number indexing the task table directly - is a call acting on an unrelated
//! process. In the init namespace the two are the same number, which is why none
//! of this is observable until something unshares.

use super::task::{Table, Task, with_table};
use crate::pidns::PidNs;
use crate::sched::policy::TaskId;
use alloc::sync::Arc;
use core::cell::RefCell;
use critical_section::Mutex;

/// The init namespace: the one a boot starts in, whose numbers *are* the task
/// slots.
///
/// Created on first use rather than at a fixed point in the boot, for the same
/// reason `uts::init` is: the first task to need it is the boot task, which
/// exists before anything that could own a table of namespaces. Every task
/// created without an `unshare` in its ancestry holds this one, so a machine
/// that never unshares cannot tell this module from the line it replaced -
/// [`PidNs::root`] answers by identity and stores nothing.
static ROOT: Mutex<RefCell<Option<Arc<PidNs>>>> = Mutex::new(RefCell::new(None));

/// The init namespace, created on first use.
pub fn root_ns() -> Arc<PidNs> {
    critical_section::with(|cs| {
        ROOT.borrow(cs)
            .borrow_mut()
            .get_or_insert_with(|| PidNs::root(crate::vfs::ns::mint()))
            .clone()
    })
}

/// The namespace the running task's own numbers come from - what `getpid`
/// answers in, and what every answer *about* this task is expressed in when it
/// is read by somebody in the same namespace.
#[cfg(target_os = "none")]

/// The namespace the running task's *children* are created in: its own until an
/// `unshare(CLONE_NEWPID)` or a `setns` moved it, which is the fact that makes
/// both calls renumber children rather than the caller.
#[cfg(target_os = "none")]

/// The number the running task is known by where it stands: `getpid(2)` and
/// `gettid(2)`'s answer.
///
/// A task that is not in its own namespace's map cannot happen by construction -
/// a task is registered where it was created - but the fallback is its slot
/// rather than a panic: an answer that is merely the global number is a bug to
/// find, and a machine that stops is not.
#[cfg(target_os = "none")]
pub fn current_pid_number() -> u32 {
    with_table(|t| {
        let me = t.current;
        t.task(me).pid_ns.number_of(me.0).unwrap_or(me.0 as u32)
    })
}

/// The number the *reading* task's namespace knows `slot` by, if it is a member
/// of it at all.
///
/// `None` is a real answer and not a failure: it is what `getppid(2)` answers a
/// 0 for (the parent stayed outside the namespace), and what stops a `/proc`
/// directory from appearing for a reader that has no business seeing it.
#[cfg(target_os = "none")]
pub fn pid_number_of(slot: TaskId) -> Option<u32> {
    with_table(|t| t.task(t.current).pid_ns.number_of(slot.0))
}

/// The number *one named task* has in one named namespace, for the answers that
/// are about somebody else: `/proc/<pid>/stat`'s first field, and the id a
/// parent reads out of a wait.
#[cfg(target_os = "none")]

/// Resolve a number the caller wrote into a task, **in the caller's namespace**.
///
/// This is the direction that keeps a namespace from being a footgun: a number
/// that names nothing where the caller stands is `None`, so a program whose
/// `getpid()` answered 1 and then calls `kill(1, …)` reaches its own namespace's
/// pid 1 - if there is one - rather than the machine's slot 1.
///
/// Liveness is asked of the table as well, because the init namespace's map is
/// an identity function and not a map: `task_of` there answers for a slot whose
/// task has long been reaped, and the caller's answer for that must be "nobody"
/// rather than a slot number that happens to be free.
#[cfg(target_os = "none")]
pub fn resolve_pid(number: i64) -> Option<TaskId> {
    if number <= 0 {
        return None;
    }
    with_table(|t| {
        let slot = t.task(t.current).pid_ns.task_of(number as u32)?;
        live(t, slot)?;
        Some(TaskId(slot))
    })
}

/// Whether a slot is a live task, the question `resolve_pid` and the `/proc`
/// funnels both ask before naming one.
pub(crate) fn live(t: &Table, slot: usize) -> Option<TaskId> {
    t.tasks.get(slot).and_then(|entry| entry.as_ref())?;
    Some(TaskId(slot))
}

/// Register a freshly created task in its namespace and every namespace above
/// it, answering the number its own namespace chose (1 for the first child of an
/// `unshare(CLONE_NEWPID)`).
///
/// The whole chain is registered because a task is a member of all of them: that
/// is what lets the parent, one level out, `wait` for the child and name it in
/// `/proc` under the number it can see.
pub fn adopt_pid(slot: TaskId, ns: &Arc<PidNs>) -> u32 {
    ns.allocate(slot.0)
}

/// Give the running task a namespace for its children: `unshare(CLONE_NEWPID)`.
///
/// The caller is **not** moved and not registered: its own `getpid` keeps
/// answering what it answered, and the namespace it came from still names it.
/// The new one is empty and starts at 1, so the first child is its init - which
/// is the entire observable effect of the call, and why the pin for it is a
/// child asking its own `getpid` rather than the parent's.
///
/// Only the calling task moves. A thread of the same process keeps pointing at
/// the parent's namespace for its children, which is the per-task `nsproxy`
/// Linux has and the same rule [`unshare_uts`] states: `unshare` splits the
/// caller, and a sibling that wanted to come along would have to say so.
///
/// The caller must satisfy [`may_mint_pid_ns`] - the syscall layer asks it
/// before calling this, because Linux refuses the whole word rather than moving
/// some of it.
pub fn unshare_pid() {
    with_table(|t| {
        let me = t.current;
        let parent = Arc::clone(&t.task(me).pid_ns);
        t.task_mut(me).pid_ns_children = PidNs::child(&parent, crate::vfs::ns::mint());
    });
}

/// Whether the running task may make a **new pid namespace**: the one
/// precondition a pid namespace has, and `unshare(CLONE_NEWPID)` and
/// `clone(CLONE_NEWPID)` share it.
///
/// Linux's `copy_pid_ns` asks it, and it is a measurement rather than a reading:
/// the caller's **own** namespace must be the one its children would go to. That
/// holds for a task that never unshared (both are where it was born) and for one
/// *created in* the namespace it is in (a child of an `unshare`, whose own
/// numbers and children's namespace are the same one) - and stops holding the
/// moment the call succeeds, because then its own numbers and its children's
/// come from different namespaces. So a process makes a pid namespace **once**:
/// a second `unshare(CLONE_NEWPID)` is `EINVAL`, and so is a
/// `clone(CLONE_NEWPID)` after one (both measured:
/// `.workbuddy/probe_pid_ns_again.c`; `docs/notes/` §6.3).
///
/// A task whose namespace is already sealed is refused for the same reason from
/// the other side - there is nothing left of that namespace to make another
/// inside - but that refusal is `ENOMEM` and belongs to task creation, not here.
pub fn may_mint_pid_ns() -> bool {
    with_table(|t| is_mintable(t.task(t.current)))
}

/// [`may_mint_pid_ns`]'s question about **one named task**, for the path that
/// asks it of a task other than the running one: `clone`'s parent is a task in
/// the table, and asking about *it* rather than about `current` keeps the call
/// making a namespace and the call checking for it one expression.
pub(crate) fn is_mintable(task: &Task) -> bool {
    task.pid_ns.ns_id() == task.pid_ns_children.ns_id()
}

/// Join a PID namespace a descriptor names: `setns(fd, CLONE_NEWPID)`'s half.
///
/// A join rather than a copy, exactly as [`setns_mnt`] is: the caller's
/// *children* from here on are members of that namespace, so a second caller
/// joining the same descriptor shares the numbering with the first, and the
/// numbers already handed out inside it do not move. Like `unshare`, the caller
/// itself keeps the number it had.
#[cfg(target_os = "none")]
pub fn setns_pid(ns: Arc<PidNs>) {
    with_table(|t| {
        let me = t.current;
        t.task_mut(me).pid_ns_children = ns;
    });
}

/// Forget a reaped task's numbers at every level. Called from `Table::reap` with
/// the task's own namespace, before the slot is freed.
pub fn release_pid(task: &Task, slot: usize) {
    task.pid_ns.release(slot);
}
