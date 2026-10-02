//! What a process's last descriptor hand-over has to tell the subsystems.
//!
//! The scheduler owns *tasks*: the table, the stacks, the deadlines, the signal
//! state. It does not own sockets, watches, evdev clients, uinput devices or
//! record locks - and before this module existed it named every one of them on
//! the exit path, which is what made `sched` unbuildable without `net`, `vfs`
//! and `drivers` (`PLAN.md` §11.1.3, V-3). Each of those is a *per-open-file
//! resource*: state that lives exactly as long as some process holds the
//! descriptor, and that has to be given up at the moment the last reference goes
//! - `do_exit`'s moment, not the reaper's (`PLAN.md` Phase 6).
//!
//! So the ownership is inverted here: a subsystem that has such state registers
//! a hook, and the teardown walks the list without knowing what is on it. The
//! hook signature is the whole of what the scheduler can offer - the batch of
//! descriptors that has just become unreferenced, and the identity of the
//! process that held them - and a subsystem that needs neither can ignore both.
//!
//! **The order is registration order, and it is load-bearing.** A hook runs
//! before the batch is dropped, because most of these resources identify
//! ownership by reference count (`vfs::file::for_each_last`) and the count stops
//! saying who owns what the moment the batch is released. Registration happens
//! in `main.rs`, in the order the resources have to go: locks first (they are
//! keyed by `(device, inode)` and the inode is reached *through* a descriptor),
//! then the socket, inotify, evdev and uinput state keyed by the open file
//! description. A hook that ran after the drop would find nothing to release,
//! which is the failure the order exists to prevent - and the reason the
//! registry is worth a host test rather than a comment.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::vfs::File;

/// One subsystem's release of the per-open-file state a process was holding.
///
/// `files` is every descriptor the process had, in table order, *before* they
/// are dropped; `owner` is the thread-group id, which is what a record lock is
/// owned by. A hook that has no use for one of them says so by naming it `_`.
pub type ExitHook = fn(files: &[Arc<File>], owner: u32);

/// The registered hooks, in the order they run.
static HOOKS: Mutex<RefCell<Vec<ExitHook>>> = Mutex::new(RefCell::new(Vec::new()));

/// Add a subsystem's hook. Called from the boot sequence, and append-only: the
/// order the resources have to be released in *is* the registration order.
pub fn register(hook: ExitHook) {
    critical_section::with(|cs| {
        HOOKS.borrow(cs).borrow_mut().push(hook);
    });
}

/// Tell every registered subsystem that these descriptors are going away.
///
/// Called from the scheduler's exit path, which is the only caller and the
/// reason this is not `pub`: the list is walked under no lock (a hook takes its
/// own, and calling one with this one held is how one lock comes to depend on
/// another), so the snapshot is taken first.
pub(crate) fn run(files: &[Arc<File>], owner: u32) {
    let hooks: Vec<ExitHook> = critical_section::with(|cs| HOOKS.borrow(cs).borrow().clone());
    for hook in hooks {
        hook(files, owner);
    }
}

/// Empty the list, so one test's registrations cannot be seen by the next.
///
/// Test-only, and deliberately not a way to *un*register in production: nothing
/// in the kernel stops owning a per-open-file resource, so nothing should be
/// able to leave the list.
#[cfg(test)]
pub(crate) fn clear() {
    critical_section::with(|cs| HOOKS.borrow(cs).borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// What a test hook records. A hook is a plain `fn`, so it cannot capture -
    /// which is the same restriction the real hooks live under, and the reason
    /// this state is a static rather than a closure's environment.
    static SEEN: Mutex<RefCell<Vec<(&'static str, usize, u32)>>> =
        Mutex::new(RefCell::new(Vec::new()));

    fn note(tag: &'static str, files: &[Arc<File>], owner: u32) {
        critical_section::with(|cs| {
            SEEN.borrow(cs).borrow_mut().push((tag, files.len(), owner));
        });
    }

    fn first(files: &[Arc<File>], owner: u32) {
        note("first", files, owner);
    }

    fn second(files: &[Arc<File>], owner: u32) {
        note("second", files, owner);
    }

    fn recorded() -> Vec<(&'static str, usize, u32)> {
        critical_section::with(|cs| SEEN.borrow(cs).borrow().clone())
    }

    #[test]
    fn hooks_run_in_registration_order_and_an_empty_list_is_not_a_special_case() {
        // One test rather than two, because the registry and what the hooks
        // record are both process-global: two tests over one global are two
        // tests racing, which is how this file's own second draft failed. The
        // property under test is the pair the teardown leans on - a hook that
        // ran in the wrong order, or after the batch was dropped, would find a
        // reference count that no longer names its owner - plus the fact that
        // the host build registers nothing and the walk is therefore a no-op
        // rather than a special case the teardown has to ask about.
        clear();
        critical_section::with(|cs| SEEN.borrow(cs).borrow_mut().clear());

        run(&[], 1);
        assert!(
            recorded().is_empty(),
            "nothing registered is the host build, and is not an error"
        );

        register(first);
        register(second);
        run(&[], 42);

        assert_eq!(
            recorded(),
            vec![("first", 0, 42), ("second", 0, 42)],
            "registration order, with both the batch and the owner handed over"
        );
    }
}
