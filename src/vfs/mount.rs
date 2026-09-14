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
use super::{FileSystem, Inode, NodeKind, VfsError, VfsResult};

/// How many links one resolution may follow before it is called a cycle.
/// Linux's own limit is 40.
const SYMLINK_HOPS: usize = 40;

/// One active mount: `at` is the directory inode in the parent filesystem,
/// `root` is the root of the mounted one.
struct Mount {
    at: Arc<dyn Inode>,
    root: Arc<dyn Inode>,
}

/// A set of mounts with a root, and the resolver over them.
pub struct Vfs {
    root: Mutex<RefCell<Option<Arc<dyn Inode>>>>,
    mounts: Mutex<RefCell<Vec<Mount>>>,
}

impl Vfs {
    pub const fn new() -> Vfs {
        Vfs {
            root: Mutex::new(RefCell::new(None)),
            mounts: Mutex::new(RefCell::new(Vec::new())),
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
            });
            Ok(())
        })
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
                    // POSIX: `/..` is `/`, so the last entry never pops.
                    ".." => {
                        if stack.len() > 1 {
                            stack.pop();
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

    /// The directory holding `path`, and the name within it - without following
    /// a link the name turns out to name.
    ///
    /// This is the shape `readlink`, `symlink`, `unlink` and `mount` need: they
    /// all act on the *name*, and a resolver that followed it would hand back
    /// what the name points at instead of the name itself.
    pub fn resolve_parent(&self, path: &str) -> VfsResult<(Arc<dyn Inode>, String)> {
        let start = critical_section::with(|cs| self.root.borrow(cs).borrow().clone())
            .ok_or(VfsError::NoRoot)?;
        self.resolve_parent_from(path, start)
    }

    /// [`resolve_parent`](Self::resolve_parent) from `start` rather than the
    /// root, which is what a relative path in a process with a cwd needs.
    pub fn resolve_parent_from(
        &self,
        path: &str,
        start: Arc<dyn Inode>,
    ) -> VfsResult<(Arc<dyn Inode>, String)> {
        let (parent_path, name) = split_last(path);
        if name.is_empty() || name == "." || name == ".." {
            return Err(VfsError::Invalid);
        }
        let parent = self.resolve_from(parent_path, start)?;
        Ok((parent, String::from(name)))
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
