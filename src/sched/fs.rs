//! The process's filesystem context: what Linux keeps in `fs_struct`.
//!
//! Four facts a process shares with its siblings until somebody asks to split
//! them apart - the root its absolute paths begin at, the directory its relative
//! paths name things from, the path that directory was reached by, and its
//! file-creation mask. They live together in one reference-counted object, and
//! that is the whole point of the type: Linux's `CLONE_FS` is the promise that
//! a thread shares them with the process that made it, `unshare(CLONE_FS)` is
//! the promise that the caller gets its own, and both are statements about *one
//! object being shared or copied* rather than about four fields happening to
//! hold equal values.
//!
//! Measured on a current host (7.2.9), because the sharing direction is the one
//! thing about this that is easy to get backwards: a thread that `chdir`s to
//! `/tmp` moves *its process's* cwd - the creating thread's own `getcwd` follows
//! it - and after the thread calls `unshare(CLONE_FS)` its `chdir` leaves the
//! creating thread where it was. So the default is share, and the call is what
//! splits. The probe is `.workbuddy/probe_unshare_flags.c`.
//!
//! **Why one lock for all four, and a leaf lock at that.** The values are read
//! and written from inside a scheduler borrow (`with_table`) *and* from outside
//! it, so the handle cannot borrow the task table for its own storage; it
//! carries a `critical_section::Mutex` of its own, which interrupts-disabled
//! makes a leaf lock that no other holder can be spinning on. One lock rather
//! than four because [`FsStruct::copy`] has to be a *snapshot*: four independent
//! locks would let another task's `chdir` land between the reads, and a copy
//! that mixed one task's cwd with another's path is exactly the half-split this
//! type exists to make impossible.
//!
//! **What is not here**: the mount namespace. Linux keeps `fs_struct` and
//! `nsproxy` apart, and so does this: `CLONE_FS` shares these four facts,
//! `CLONE_THREAD` shares the mount table, and `unshare(CLONE_NEWNS)` splits only
//! the calling task's - which is why the scheduler keeps that slot beside this
//! handle rather than in it.

use alloc::string::String;
use alloc::sync::Arc;
use core::cell::RefCell;
use critical_section::Mutex;

/// One process's filesystem context, shared by reference and split by copy.
pub struct FsStruct {
    state: Mutex<RefCell<FsState>>,
}

struct FsState {
    /// The root absolute paths begin at. `None` is the machine's root, which is
    /// where every task starts: the VFS is mounted long after the first tasks
    /// exist, so the inode cannot be grabbed up front.
    root: Option<Arc<dyn crate::vfs::Inode>>,
    /// The directory relative paths name things from. `None` is the root, on the
    /// same terms.
    cwd: Option<Arc<dyn crate::vfs::Inode>>,
    /// The path `cwd` was reached by, as the caller spelled it out - what
    /// `getcwd` reports. `None` means "never left `/`", which is the truth for
    /// every task that has not asked to be anywhere else.
    cwd_path: Option<String>,
    /// The file-creation mask, `0o022` until something calls `umask`.
    umask: u32,
}

impl FsStruct {
    /// A fresh context: no root, no cwd, and the given creation mask. This is a
    /// process at birth and a kernel task alike, since both start at the machine
    /// root with nothing to unwind.
    pub fn new(umask: u32) -> Arc<FsStruct> {
        Arc::new(FsStruct {
            state: Mutex::new(RefCell::new(FsState {
                root: None,
                cwd: None,
                cwd_path: None,
                umask,
            })),
        })
    }

    /// A copy: the same four facts in a handle of its own, unrelated to this one
    /// from here on. Linux's `copy_fs_struct`, and the whole of what
    /// `unshare(CLONE_FS)` does - the caller keeps what it had and stops sharing
    /// it, which is why an empty copy would be a way to lose the machine.
    pub fn copy(&self) -> Arc<FsStruct> {
        critical_section::with(|cs| {
            let state = self.state.borrow(cs).borrow();
            Arc::new(FsStruct {
                state: Mutex::new(RefCell::new(FsState {
                    root: state.root.clone(),
                    cwd: state.cwd.clone(),
                    cwd_path: state.cwd_path.clone(),
                    umask: state.umask,
                })),
            })
        })
    }

    /// The directory relative paths name things from, `None` for the root.
    pub fn cwd(&self) -> Option<Arc<dyn crate::vfs::Inode>> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().cwd.clone())
    }

    /// Move this context's cwd. Checked to be a directory by the caller; this
    /// only remembers - and, because the context is shared, moves every task
    /// holding it.
    pub fn set_cwd(&self, dir: Arc<dyn crate::vfs::Inode>) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().cwd = Some(dir));
    }

    /// The path cwd was reached by, `None` when it has never moved.
    pub fn cwd_path(&self) -> Option<String> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().cwd_path.clone())
    }

    /// Remember the path cwd was reached by. The caller normalizes it; this only
    /// stores.
    pub fn set_cwd_path(&self, path: String) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().cwd_path = Some(path));
    }

    /// The root absolute paths begin at, `None` for the machine's own.
    pub fn root(&self) -> Option<Arc<dyn crate::vfs::Inode>> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().root.clone())
    }

    /// Re-home the tasks holding this context: make `dir` the root their
    /// absolute paths begin at. Checked to be a directory by the caller; this
    /// only remembers. The machine's own root is left standing - this is a
    /// per-context view, not a namespace.
    pub fn set_root(&self, dir: Arc<dyn crate::vfs::Inode>) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().root = Some(dir));
    }

    /// The file-creation mask - what `umask(2)` reports.
    pub fn umask(&self) -> u32 {
        critical_section::with(|cs| self.state.borrow(cs).borrow().umask)
    }

    /// Set the file-creation mask - what `umask(2)` does. Shared like the rest,
    /// so a thread's `umask` is its process's.
    pub fn set_umask(&self, mask: u32) {
        critical_section::with(|cs| self.state.borrow(cs).borrow_mut().umask = mask);
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::FsStruct;
    use crate::vfs::FileSystem;
    use crate::vfs::NodeKind;
    use crate::vfs::ramfs::RamFs;

    fn dir() -> alloc::sync::Arc<dyn crate::vfs::Inode> {
        let fs = RamFs::new();
        fs.root()
            .create("d", NodeKind::Dir, None)
            .expect("a directory to move into")
    }

    /// A copy keeps what was there and then diverges - the two halves of
    /// `copy_fs_struct`, and the reason a single test holds them: a copy that
    /// *lost* the values would pass the divergence half and fail the keep half,
    /// and one that was really the same handle would do the reverse.
    #[test]
    fn a_copy_keeps_the_facts_and_then_diverges() {
        let original = FsStruct::new(0o022);
        let d = dir();
        original.set_cwd(d.clone());
        original.set_cwd_path(alloc::string::String::from("/d"));
        original.set_root(d.clone());
        original.set_umask(0o077);

        let copy = original.copy();
        assert!(
            !alloc::sync::Arc::ptr_eq(&original, &copy),
            "no copy was made"
        );
        assert!(copy.cwd().is_some(), "the copy did not inherit the cwd");
        assert_eq!(copy.cwd_path().as_deref(), Some("/d"));
        assert!(copy.root().is_some(), "the copy did not inherit the root");
        assert_eq!(copy.umask(), 0o077, "the copy did not inherit the mask");

        // And a write to one is invisible to the other, in both directions.
        copy.set_umask(0o002);
        assert_eq!(copy.umask(), 0o002);
        assert_eq!(original.umask(), 0o077, "the copy renamed its parent");
        original.set_cwd_path(alloc::string::String::from("/somewhere-else"));
        assert_eq!(
            copy.cwd_path().as_deref(),
            Some("/d"),
            "the parent renamed its copy"
        );
    }

    /// Sharing is sharing: two holders of one handle see each other's writes.
    /// This is what `CLONE_FS` promises and what the type has to make true
    /// rather than approximate - a thread's `chdir` moving its process is a
    /// claim about one object, not about equal values.
    #[test]
    fn holders_of_one_handle_see_each_others_writes() {
        let shared = FsStruct::new(0o022);
        let also = alloc::sync::Arc::clone(&shared);
        shared.set_cwd_path(alloc::string::String::from("/tmp"));
        assert_eq!(also.cwd_path().as_deref(), Some("/tmp"));
        also.set_umask(0o077);
        assert_eq!(shared.umask(), 0o077);
    }
}
