//! Mounts and path resolution.
//!
//! Resolution walks the tree one component at a time, keeping the logical
//! chain from the root in a stack. The stack is what makes `..` correct across
//! mounts: entering `/mnt` (a mountpoint) pushes the mounted filesystem's
//! root, so `..` from inside that filesystem pops straight back to the
//! mountpoint's parent without any special case - the entry that would have
//! needed a cross-filesystem parent pointer is simply not on the stack.
//!
//! The table is a plain instance, not a global, so tests can build isolated
//! trees; the kernel's own table lives in [`STATIC`] behind the free functions.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::path;
use super::{FileSystem, Inode, NodeKind, VfsError, VfsResult, resolve_opt};

/// How many links one resolution may follow before it is called a cycle.
/// Linux's own limit is 40.
const SYMLINK_HOPS: usize = 40;

/// One active mount: `at` is the directory inode in the parent filesystem,
/// `root` is the root of the mounted one.
struct Mount {
    at: Arc<dyn Inode>,
    root: Arc<dyn Inode>,
    /// The name `mount(2)` was given for this filesystem, and the source it
    /// named - "/dev/vda3" for an ext2 partition, "proc" for procfs. Both are
    /// what /proc/mounts reports, and /proc/mounts is how userland finds out
    /// what is mounted where, which is the only record there is.
    fstype: alloc::string::String,
    source: alloc::string::String,
}

/// A set of mounts with a root, and the resolver over them.
pub struct Vfs {
    root: Mutex<RefCell<Option<Arc<dyn Inode>>>>,
    mounts: Mutex<RefCell<Vec<Mount>>>,
    /// What `/` is, for /proc/mounts. The root is not in the mount table - it
    /// is reached directly - so its own record lives here, filled in by
    /// `pivot_root` from the mount that carried the new root in.
    root_fstype: Mutex<RefCell<Option<alloc::string::String>>>,
    root_source: Mutex<RefCell<Option<alloc::string::String>>>,
}

impl Vfs {
    pub const fn new() -> Vfs {
        Vfs {
            root: Mutex::new(RefCell::new(None)),
            mounts: Mutex::new(RefCell::new(Vec::new())),
            root_fstype: Mutex::new(RefCell::new(None)),
            root_source: Mutex::new(RefCell::new(None)),
        }
    }

    /// Install the root filesystem. Once, at boot: a second call is a bug.
    pub fn mount_root(&self, fs: &dyn FileSystem) {
        critical_section::with(|cs| {
            let mut root = self.root.borrow(cs).borrow_mut();
            assert!(root.is_none(), "the root filesystem is already mounted");
            *root = Some(fs.root());
        });
    }

    /// The root inode, for the one caller that needs to start a walk from
    /// "the beginning" without a path: a task whose cwd was never set.
    pub fn root_inode(&self) -> Arc<dyn Inode> {
        critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .expect("no root filesystem is mounted")
    }

    /// Mount `fs` at the directory named by `path`.
    ///
    /// The mountpoint is found through its parent, not by resolving `path`
    /// outright: resolving would walk through whatever is already mounted
    /// there and lose the directory that is supposed to be mounted on.
    pub fn mount(&self, path: &str, fs: &dyn FileSystem) -> VfsResult<()> {
        if critical_section::with(|cs| self.root.borrow(cs).borrow().is_none()) {
            return Err(VfsError::NoRoot);
        }
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rfind('/') {
            Some(i) => (&path[..=i], &trimmed[i + 1..]),
            // `/` itself: always mounted, and roots do not stack here yet.
            None if trimmed.is_empty() => return Err(VfsError::Busy),
            // Relative with no cwd to resolve it against.
            None => return Err(VfsError::Invalid),
        };
        if name.is_empty() || name == "." || name == ".." {
            return Err(VfsError::Invalid);
        }

        let parent = self.resolve(parent_path)?;
        let at = parent.lookup(name)?;
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, &at)) {
                return Err(VfsError::Busy);
            }
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fs.name()),
                source: alloc::string::String::from("none"),
            });
            Ok(())
        })
    }

    /// Mount `fs` on an already-resolved directory, as `mount(2)` does: the
    /// syscall has the mountpoint in its hand and the filesystem type and
    /// source as the caller spelled them, and both go into the record that
    /// /proc/mounts later reports.
    pub fn mount_at(
        &self,
        at: Arc<dyn Inode>,
        fs: &dyn FileSystem,
        fstype: &str,
        source: &str,
    ) -> VfsResult<()> {
        if at.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if mounts.iter().any(|m| Arc::ptr_eq(&m.at, &at)) {
                return Err(VfsError::Busy);
            }
            mounts.push(Mount {
                at,
                root: fs.root(),
                fstype: alloc::string::String::from(fstype),
                source: alloc::string::String::from(source),
            });
            Ok(())
        })
    }

    /// Unmount whatever is mounted on `target`, given the mountpoint's own
    /// inode - the shape `umount2(2)` arrives in, having resolved the path
    /// against its caller's cwd.
    pub fn unmount_resolved(&self, target: &Arc<dyn Inode>) -> VfsResult<()> {
        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if let Some(pos) = mounts.iter().position(|m| Arc::ptr_eq(&m.root, target)) {
                mounts.remove(pos);
                Ok(())
            } else {
                Err(VfsError::Busy)
            }
        })
    }

    /// Unmount the filesystem mounted at `path`.
    ///
    /// The path must name a mountpoint - a directory that has a filesystem
    /// mounted on it. The mount is removed from the mount table, and subsequent
    /// accesses to files under `path` will see the underlying directory in the
    /// parent filesystem instead.
    pub fn unmount(&self, path: &str) -> VfsResult<()> {
        if critical_section::with(|cs| self.root.borrow(cs).borrow().is_none()) {
            return Err(VfsError::NoRoot);
        }
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            return Err(VfsError::Invalid);
        }

        // Resolve the path to find the mountpoint's inode. We need to find
        // the mount entry that corresponds to this path.
        let target = self.resolve(trimmed)?;

        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            if let Some(pos) = mounts.iter().position(|m| Arc::ptr_eq(&m.root, &target)) {
                mounts.remove(pos);
                Ok(())
            } else {
                Err(VfsError::Busy)
            }
        })
    }

    /// Replace the root filesystem: move `new_root` to `/`, and move the old
    /// root to `put_old` inside the new root, so it can be unmounted afterward.
    ///
    /// `put_old` has to be an existing directory in the new root, as Linux
    /// requires: the point of the operation is that nothing is left in the
    /// air, and a mount point invented on the caller's behalf would be a
    /// directory nobody knows to remove.
    ///
    /// The old root is *mounted* at `put_old` rather than merely dropped. That
    /// keeps it reachable until the caller says otherwise, and it is the one
    /// reference that decides when the initramfs's memory can be given back.
    /// Dropping it here instead would leave the tree alive in whatever still
    /// pointed into it, with no name left to reach it by - a leak with no
    /// handle. Detaching it is the caller's next step, and a deliberate one.
    pub fn pivot_root(&self, new_root: &str, put_old: &str) -> VfsResult<()> {
        let old_root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        let new_root_inode = self.resolve(new_root.trim_end_matches('/'))?;

        // `put_old` is walked *inside* the new root: it names a directory in
        // the tree that is about to become `/`, not one in the tree being
        // replaced. The canonical spelling - `pivot_root /new_root
        // /new_root/old_root` - names it from the caller's tree, so the new
        // root's own prefix comes off before the walk.
        let new_root_trimmed = new_root.trim_end_matches('/');
        let put_old = put_old.trim_start_matches('/');
        let put_old = match put_old.strip_prefix(new_root_trimmed.trim_start_matches('/')) {
            Some(rest) => rest.trim_start_matches('/'),
            None => put_old,
        };
        let mut put_old_inode = new_root_inode.clone();
        if !put_old.is_empty() {
            for component in put_old.split('/') {
                if component.is_empty() || component == "." {
                    continue;
                }
                put_old_inode = put_old_inode.lookup(component)?;
            }
        }
        if put_old_inode.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }

        critical_section::with(|cs| {
            let mut mounts = self.mounts.borrow(cs).borrow_mut();
            // The root's own identity comes from the mount that made
            // `new_root` reachable: after the pivot that mount is gone, but
            // /proc/mounts still has to say what `/` is and what it came from.
            if let Some(entry) = mounts
                .iter()
                .find(|m| Arc::ptr_eq(&m.root, &new_root_inode))
                .map(|m| (m.fstype.clone(), m.source.clone()))
            {
                *self.root_fstype.borrow(cs).borrow_mut() = Some(entry.0);
                *self.root_source.borrow(cs).borrow_mut() = Some(entry.1);
            }
            // Every mount in the table was attached somewhere in the old tree -
            // /dev, /tmp, the new root itself - and none of them can be reached
            // once the root is replaced. Keeping them would leave entries
            // anchored at nodes nothing resolves to, holding whole filesystems
            // alive for good. The new tree starts with the one mount below plus
            // whatever the caller attaches next.
            mounts.clear();
            mounts.push(Mount {
                at: put_old_inode.clone(),
                root: old_root,
                fstype: alloc::string::String::from("ramfs"),
                source: alloc::string::String::from("initramfs"),
            });
            *self.root.borrow(cs).borrow_mut() = Some(new_root_inode);
        });
        Ok(())
    }

    /// Walk `path` from the root, crossing mounts as they are met.
    pub fn resolve(&self, path: &str) -> VfsResult<Arc<dyn Inode>> {
        let start = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        self.resolve_from(path, start)
    }

    /// Walk `path` from `start` instead of the root - which is what a
    /// relative path means, and why a process carrying a cwd needs this
    /// rather than [`resolve`](Self::resolve).
    ///
    /// An absolute path ignores `start` entirely: `/` means the root, not
    /// "wherever this happens to be". The mount crossing is the same walk as
    /// ever, one node deeper in the tree.
    pub fn resolve_from(&self, path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
        let absolute = path.starts_with('/');
        let start = if absolute {
            critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
                .ok_or(VfsError::NoRoot)?
        } else {
            start
        };
        // A relative path is walked as the absolute path it names from where
        // it starts, so the one `components` - which splits absolute paths -
        // covers both. The joined form is owned, and lives for the walk.
        let joined;
        let path = if absolute {
            path
        } else {
            joined = format!("/{path}");
            joined.as_str()
        };

        // The names still to walk. Owned, because meeting a link rewrites what
        // is left: the link's target goes in front of the names that followed
        // it, and the whole thing is walked again.
        let mut todo: VecDeque<String> = path::components(path)?
            .map(String::from)
            .collect::<Vec<_>>()
            .into();
        // Where the walk restarts from, which a link's target can move: an
        // absolute target goes back to the root, a relative one stays beside
        // the link it came from.
        let mut base = start;
        let mut hops = 0;

        loop {
            // The logical chain from `base`. Length 1 means "at `base`", where
            // `..` stays put as POSIX says it must.
            let mut stack: Vec<Arc<dyn Inode>> = alloc::vec![base.clone()];
            let mut met_link = false;

            while let Some(comp) = todo.pop_front() {
                match comp.as_str() {
                    "." => {}
                    // POSIX: `/..` is `/`, so the last entry never pops. The
                    // first `..` is the one an inode cannot answer: the stack
                    // holds the walk so far, and at the base there is no walk
                    // yet, so the base's own parent has to be looked for.
                    ".." => {
                        if stack.len() > 1 {
                            stack.pop();
                        } else if stack[0].metadata().kind != NodeKind::Dir {
                            // `file/..` is not "the parent of a file": a name
                            // that is not a directory has no `..` to go to, and
                            // an ordinary walk would have said so by looking a
                            // name up in it.
                            return Err(VfsError::NotDir);
                        } else if let Some(parent) = self.parent_of(&stack[0]) {
                            stack[0] = parent;
                        }
                    }
                    name => {
                        let here = stack.last().expect("the stack is never empty").clone();
                        let child = here.lookup(name)?;
                        let child = critical_section::with(|cs| {
                            let mounts = self.mounts.borrow(cs).borrow();
                            match mounts.iter().find(|m| Arc::ptr_eq(&m.at, &child)) {
                                Some(m) => m.root.clone(),
                                None => child,
                            }
                        });
                        if child.metadata().kind == NodeKind::Symlink {
                            // Splice the target in front of whatever was still
                            // to come and start over. `here`, not the link, is
                            // what a relative target is relative to.
                            let target = child.readlink()?;
                            // `components` only splits absolute paths, so a
                            // relative target is walked as the absolute path
                            // it names from `here` - which is what makes it
                            // relative to the link's own directory.
                            let owned;
                            let walk = if target.starts_with('/') {
                                target.as_str()
                            } else {
                                owned = format!("/{target}");
                                owned.as_str()
                            };
                            let mut next: VecDeque<String> = path::components(walk)?
                                .map(String::from)
                                .collect::<Vec<_>>()
                                .into();
                            next.extend(todo);
                            todo = next;
                            base = if target.starts_with('/') {
                                critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
                                    .ok_or(VfsError::NoRoot)?
                            } else {
                                here
                            };
                            met_link = true;
                            break;
                        }
                        stack.push(child);
                    }
                }
            }

            if !met_link {
                return Ok(stack.pop().expect("the stack is never empty"));
            }
            // ELOOP: a link chain this long is a cycle, and walking it any
            // further would just be a slower way to hang.
            hops += 1;
            if hops > SYMLINK_HOPS {
                return Err(VfsError::Invalid);
            }
        }
    }

    /// The path by which `target` is reached, found by walking down from the
    /// root. The same search [`Self::parent_of`] makes, carrying the names it
    /// came by - which is what /proc/mounts needs, because a mount is recorded
    /// by the inode it sits on and /proc reports it by name.
    ///
    /// `"/"` for the root itself, `None` for a node the tree cannot name: a
    /// mounted filesystem's root reached from outside its mount point, or a
    /// node whose other name the caller did not use.
    pub fn path_of(&self, target: &Arc<dyn Inode>) -> Option<alloc::string::String> {
        const VISITS_MAX: usize = 512;

        let root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())?;
        if Arc::ptr_eq(&root, target) {
            return Some(alloc::string::String::from("/"));
        }
        let mut queue: VecDeque<(Arc<dyn Inode>, alloc::string::String)> =
            VecDeque::from([(root, alloc::string::String::new())]);
        let mut visits = 0usize;
        while let Some((dir, prefix)) = queue.pop_front() {
            visits += 1;
            if visits > VISITS_MAX {
                return None;
            }
            let Ok(entries) = dir.read_dir() else {
                continue;
            };
            for (name, kind) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(child) = dir.lookup(&name) else {
                    continue;
                };
                let reached = critical_section::with(|cs| {
                    let mounts = self.mounts.borrow(cs).borrow();
                    mounts
                        .iter()
                        .find(|m| Arc::ptr_eq(&m.at, &child))
                        .map(|m| m.root.clone())
                        .unwrap_or_else(|| child.clone())
                });
                let path = alloc::format!("{prefix}/{name}");
                if Arc::ptr_eq(&reached, target) {
                    return Some(path);
                }
                if kind == NodeKind::Dir {
                    queue.push_back((reached, path));
                }
            }
        }
        None
    }

    /// Every mount, as /proc/mounts reports them: source, mountpoint, type.
    /// The root comes first, then the table in mounting order.
    ///
    /// This is the only record of what is mounted where that userland can
    /// read, and the thing `mountinfo` and `mount -a` are built on.
    pub fn mount_table(
        &self,
    ) -> Vec<(
        alloc::string::String,
        alloc::string::String,
        alloc::string::String,
    )> {
        // The table is copied out first: naming each mount point walks the
        // tree, and that walk takes the same lock. Nested takes of the one
        // critical section are not something to lean on.
        let (root_fstype, root_source, entries) = critical_section::with(|cs| {
            let fstype = self.root_fstype.borrow(cs).borrow().clone();
            let source = self.root_source.borrow(cs).borrow().clone();
            let entries: Vec<(Arc<dyn Inode>, alloc::string::String, alloc::string::String)> = self
                .mounts
                .borrow(cs)
                .borrow()
                .iter()
                .map(|m| (m.at.clone(), m.source.clone(), m.fstype.clone()))
                .collect();
            (fstype, source, entries)
        });
        let mut lines = Vec::new();
        if let (Some(fstype), Some(source)) = (root_fstype, root_source) {
            lines.push((source, alloc::string::String::from("/"), fstype));
        }
        for (at, source, fstype) in entries {
            if let Some(path) = self.path_of(&at) {
                lines.push((source, path, fstype));
            }
        }
        lines
    }

    /// The directory that names `target`, found by walking down from the root.
    ///
    /// A node does not know its parent. A directory is a *name edge* in
    /// something else and nothing points back up, so the only way to go up from
    /// an inode is to look for the edge that names it - which is a walk of the
    /// tree. It is what makes `..` mean anything from a cwd, since a relative
    /// path starts at an inode and not at a name.
    ///
    /// A mounted filesystem is handled by the edge that carries it: the search
    /// steps into the mount and compares against the mounted root, so `..` out
    /// of `/dev` lands on `/` rather than nowhere.
    ///
    /// `None` for the root (nothing names it), for a node reached by one of
    /// several names that the caller did not use, and for a tree too large to
    /// look through. The first is what POSIX asks for; the others leave `..`
    /// where it was, which is the honest answer when the tree cannot say.
    fn parent_of(&self, target: &Arc<dyn Inode>) -> Option<Arc<dyn Inode>> {
        // Bounded: the answer is near the top for every cwd a program actually
        // holds, and searching a large tree without limit to resolve one `..`
        // is a worse trade than a `..` that does not move.
        const VISITS_MAX: usize = 512;

        let root = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())?;
        let mut queue: VecDeque<Arc<dyn Inode>> = VecDeque::from([root]);
        let mut visits = 0usize;
        while let Some(dir) = queue.pop_front() {
            visits += 1;
            if visits > VISITS_MAX {
                return None;
            }
            let Ok(entries) = dir.read_dir() else {
                continue;
            };
            for (name, kind) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let Ok(child) = dir.lookup(&name) else {
                    continue;
                };
                // What resolving `name` here would actually give: the mount's
                // root when the name is a mount point, the child otherwise.
                let reached = critical_section::with(|cs| {
                    let mounts = self.mounts.borrow(cs).borrow();
                    mounts
                        .iter()
                        .find(|m| Arc::ptr_eq(&m.at, &child))
                        .map(|m| m.root.clone())
                        .unwrap_or_else(|| child.clone())
                });
                if Arc::ptr_eq(&reached, target) {
                    return Some(dir.clone());
                }
                if kind == NodeKind::Dir {
                    queue.push_back(reached);
                }
            }
        }
        None
    }

    /// The directory holding `path`, and the name within it - without following
    /// a link the name turns out to name.
    ///
    /// This is the shape `readlink`, `symlink`, `unlink` and `mount` need: they
    /// all act on the *name*, and a resolver that followed it would hand back
    /// what the name points at instead of the name itself.
    ///
    /// The two halves are separated rather than returned as a node because a
    /// caller that is about to *make* it needs the directory, and one that is
    /// about to *read* it needs the name it will look up.
    pub fn resolve_parent(&self, path: &str) -> VfsResult<(Arc<dyn Inode>, String)> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() {
            return Err(VfsError::Invalid);
        }
        let parent = self.resolve(parent_path)?;
        Ok((parent, String::from(name)))
    }

    /// Resolve `path` without following a symbolic link in its **last**
    /// component - what `lstat`, `readlink` and `symlink` need, and the one
    /// thing the ordinary walk cannot give them.
    ///
    /// Everything before the last name is walked normally, links and all, so
    /// `a/b/c` really looks inside the `b` that `a` leads to. The last name is
    /// then looked up in its parent and handed back unfollowed. That
    /// distinction is the whole point: `lstat` on a link must answer for the
    /// link, and `symlink` onto a name that is already a link must report the
    /// name as taken rather than quietly writing beside its target.
    ///
    /// `.` and `..` are the two names that are not entries of the directory
    /// they appear in - `.` *is* that directory, and neither can be a link -
    /// so resolving them is ordinary resolution. They are also the names every
    /// directory-listing tool asks for, which is why treating them as
    /// unnameable broke `ls -l`.
    pub fn resolve_no_follow(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<Arc<dyn Inode>> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() || name == "." || name == ".." {
            return self.resolve_from(path, start);
        }
        let parent = self.resolve_from(parent_path, start)?;
        if parent.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        parent.lookup(name)
    }
}

/// Split `/a/b/c` into `/a/b` and `c`; `/a` into `/` and `a`.
fn split_last(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => (if i == 0 { "/" } else { &trimmed[..i] }, &trimmed[i + 1..]),
        None => (".", trimmed),
    }
}

impl Default for Vfs {
    fn default() -> Vfs {
        Vfs::new()
    }
}

static STATIC: Vfs = Vfs::new();

/// The kernel's one instance, for callers the free functions do not cover -
/// the initramfs unpacker, which walks and creates through a `&Vfs`.
pub fn global() -> &'static Vfs {
    &STATIC
}

/// Install the kernel's root filesystem. Once, at boot.
pub fn mount_root(fs: &dyn FileSystem) {
    STATIC.mount_root(fs);
}

/// Mount `fs` at the directory named by `path` in the kernel's tree.
pub fn mount(path: &str, fs: &dyn FileSystem) -> VfsResult<()> {
    STATIC.mount(path, fs)
}

/// Unmount the filesystem mounted at `path`.
///
/// The path must name a mountpoint - a directory that has a filesystem mounted
/// on it. The mount is removed from the mount table, and subsequent accesses
/// to files under `path` will see the underlying directory in the parent
/// filesystem instead.
pub fn unmount(path: &str) -> VfsResult<()> {
    STATIC.unmount(path)
}

/// Replace the root filesystem: move `new_root` to `/`, and move the old root
/// to `put_old` inside the new root, so it can be unmounted afterward.
///
/// This is the kernel's pivot_root(2): after this call, `/` refers to what was
/// previously `new_root`, and `put_old` holds the old root filesystem's
/// contents, ready to be unmounted.
pub fn pivot_root(new_root: &str, put_old: &str) -> VfsResult<()> {
    STATIC.pivot_root(new_root, put_old)
}

/// Mount `fs` on the directory `target` already resolves to, recording the
/// type and source as the caller spelled them. This is the shape `mount(2)`
/// arrives in, and the reason it exists as a syscall at all: which filesystems
/// are mounted where is userland's decision to make.
pub fn mount_at(target: &str, fs: &dyn FileSystem, fstype: &str, source: &str) -> VfsResult<()> {
    mount_at_opt(target, None, fs, fstype, source)
}

/// [`mount_at`](fn.mount_at.html) resolved against a caller's cwd.
pub fn mount_at_opt(
    target: &str,
    start: Option<Arc<dyn Inode>>,
    fs: &dyn FileSystem,
    fstype: &str,
    source: &str,
) -> VfsResult<()> {
    let at = resolve_opt(target, start)?;
    STATIC.mount_at(at, fs, fstype, source)
}

/// Unmount the filesystem at `path`, resolved against a caller's cwd.
pub fn unmount_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let resolved = resolve_opt(path, start)?;
    STATIC.unmount_resolved(&resolved)
}

/// Every mount, as /proc/mounts reports it.
pub fn mount_table() -> Vec<(
    alloc::string::String,
    alloc::string::String,
    alloc::string::String,
)> {
    STATIC.mount_table()
}

/// Resolve `path` in the kernel's tree.
pub fn resolve(path: &str) -> VfsResult<Arc<dyn Inode>> {
    STATIC.resolve(path)
}

/// Resolve `path` starting from `start` - a process's cwd. An absolute path
/// ignores the start and begins at the root, as `/` must.
pub fn resolve_at(path: &str, start: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    STATIC.resolve_from(path, start)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::Vfs;
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    fn tree_with_root() -> Arc<Vfs> {
        let vfs = Arc::new(Vfs::new());
        vfs.mount_root(&*RamFs::new());
        vfs
    }

    #[test]
    fn resolution_walks_names_and_dots() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("a", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/a")
            .unwrap()
            .create("f", NodeKind::File)
            .unwrap();

        let f = vfs.resolve("/a/./f").unwrap();
        let again = vfs.resolve("/a/../a/f").unwrap();
        assert!(Arc::ptr_eq(&f, &again));
        // `/..` at the root stays at the root, as POSIX demands.
        let root = vfs.resolve("/").unwrap();
        assert!(Arc::ptr_eq(&vfs.resolve("/..").unwrap(), &root));

        assert_eq!(vfs.resolve("/a/missing").err(), Some(VfsError::NoEntry));
        // A file in the middle of a path is not a directory.
        assert_eq!(vfs.resolve("/a/f/x").err(), Some(VfsError::NotDir));
    }

    #[test]
    fn mounts_replace_the_child_with_the_mounted_root() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("mnt", NodeKind::Dir)
            .unwrap();
        let inner = RamFs::new();
        inner.root().create("hello", NodeKind::File).unwrap();
        vfs.mount("/mnt", &*inner).unwrap();

        let mnt = vfs.resolve("/mnt").unwrap();
        assert!(Arc::ptr_eq(&mnt, &inner.root()));
        assert!(Arc::ptr_eq(
            &vfs.resolve("/mnt/hello").unwrap(),
            &inner.root().lookup("hello").unwrap()
        ));

        // `..` from inside the mount lands on the mountpoint's parent, then
        // walks back in through the mount edge.
        let back = vfs.resolve("/mnt/hello/../../mnt").unwrap();
        assert!(Arc::ptr_eq(&back, &mnt));
    }

    #[test]
    fn mounting_twice_at_one_point_is_busy_and_files_are_not_mountpoints() {
        let vfs = tree_with_root();
        vfs.resolve("/")
            .unwrap()
            .create("d", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/")
            .unwrap()
            .create("f", NodeKind::File)
            .unwrap();
        let inner = RamFs::new();

        vfs.mount("/d", &*inner).unwrap();
        assert_eq!(vfs.mount("/d", &*inner).unwrap_err(), VfsError::Busy);
        assert_eq!(vfs.mount("/f", &*inner).unwrap_err(), VfsError::NotDir);
    }

    #[test]
    fn resolving_before_a_root_exists_is_noroot() {
        let vfs = Vfs::new();
        assert_eq!(vfs.resolve("/").err(), Some(VfsError::NoRoot));
        assert_eq!(vfs.mount("/", &*RamFs::new()).err(), Some(VfsError::NoRoot));
    }
    /// Build `/bin/busybox` as a file and `/bin/ls`, `/bin/sh` as links to it -
    /// the shape a busybox rootfs is made of.
    fn tree_with_busybox() -> Arc<Vfs> {
        let vfs = tree_with_root();
        let bin = vfs
            .resolve("/")
            .unwrap()
            .create("bin", NodeKind::Dir)
            .unwrap();
        let busybox = bin.create("busybox", NodeKind::File).unwrap();
        busybox.write_at(0, b"busybox").unwrap();
        bin.symlink("ls", "/bin/busybox").unwrap();
        bin.symlink("sh", "busybox").unwrap();
        vfs
    }

    #[test]
    fn resolution_follows_an_absolute_and_a_relative_link() {
        let vfs = tree_with_busybox();
        let busybox = vfs.resolve("/bin/busybox").unwrap();

        let ls = vfs.resolve("/bin/ls").unwrap();
        assert!(
            Arc::ptr_eq(&ls, &busybox),
            "an absolute link was not followed"
        );
        // A relative link is relative to the directory holding it, not to the
        // root and not to the caller.
        let sh = vfs.resolve("/bin/sh").unwrap();
        assert!(
            Arc::ptr_eq(&sh, &busybox),
            "a relative link was not followed"
        );
        assert_eq!(ls.metadata().kind, NodeKind::File);
    }

    #[test]
    fn a_link_in_the_middle_of_a_path_is_followed() {
        let vfs = tree_with_busybox();
        vfs.resolve("/")
            .unwrap()
            .symlink("binlink", "/bin")
            .unwrap();
        let busybox = vfs.resolve("/bin/busybox").unwrap();
        // The names after the link still have to be walked once it resolves.
        assert!(Arc::ptr_eq(&vfs.resolve("/binlink/ls").unwrap(), &busybox));
        assert!(vfs.resolve("/binlink/../bin/sh").is_ok());
    }

    #[test]
    fn a_link_cycle_is_an_error_and_not_a_hang() {
        let vfs = tree_with_root();
        let root = vfs.resolve("/").unwrap();
        root.symlink("a", "b").unwrap();
        root.symlink("b", "a").unwrap();
        assert_eq!(vfs.resolve("/a").err(), Some(VfsError::Invalid));
    }

    #[test]
    fn resolve_parent_names_the_link_itself() {
        let vfs = tree_with_busybox();
        let (parent, name) = vfs.resolve_parent("/bin/ls").unwrap();
        assert_eq!(name, "ls");
        // The link, not what it points at: this is what readlink needs.
        let link = parent.lookup(&name).unwrap();
        assert_eq!(link.metadata().kind, NodeKind::Symlink);
        assert_eq!(link.readlink().unwrap(), "/bin/busybox");
    }

    #[test]
    fn readlink_on_a_file_that_is_not_a_link_is_an_error() {
        let vfs = tree_with_busybox();
        let (parent, name) = vfs.resolve_parent("/bin/busybox").unwrap();
        assert_eq!(
            parent.lookup(&name).unwrap().readlink().err(),
            Some(VfsError::Invalid)
        );
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod cwd_tests {
    use alloc::sync::Arc;

    use super::Vfs;
    use crate::vfs::NodeKind;
    use crate::vfs::ramfs::RamFs;

    #[test]
    fn a_relative_path_starts_at_the_given_node() {
        let vfs = Arc::new(Vfs::new());
        vfs.mount_root(&*RamFs::new());
        vfs.resolve("/")
            .unwrap()
            .create("bin", NodeKind::Dir)
            .unwrap();
        vfs.resolve("/bin")
            .unwrap()
            .create("cat", NodeKind::File)
            .unwrap();
        vfs.resolve("/")
            .unwrap()
            .create("etc", NodeKind::Dir)
            .unwrap();

        let bin = vfs.resolve("/bin").unwrap();
        // "." from /bin is /bin.
        let here = vfs.resolve_from(".", bin.clone()).unwrap();
        assert!(Arc::ptr_eq(&here, &bin));

        // A file relative to /bin resolves; one relative to the root does not
        // exist there.
        let cat = vfs.resolve_from("cat", bin.clone()).unwrap();
        assert_eq!(cat.metadata().kind, NodeKind::File);
        assert!(vfs.resolve_from("etc", bin).is_err(), "etc is not in /bin");
    }
}
