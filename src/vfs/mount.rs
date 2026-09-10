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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::path;
use super::{FileSystem, Inode, NodeKind, VfsError, VfsResult};

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
        // The logical chain from `/` to the current node. Length 1 means "at
        // the root", where `..` stays put as POSIX says it must.
        let mut stack: Vec<Arc<dyn Inode>> = alloc::vec![start];

        for comp in path::components(path)? {
            match comp {
                "." => {}
                // POSIX: `/..` is `/`, so the last entry never pops.
                ".." => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                name => {
                    let here = stack.last().expect("the stack is never empty");
                    let child = here.lookup(name)?;
                    let child = critical_section::with(|cs| {
                        let mounts = self.mounts.borrow(cs).borrow();
                        match mounts.iter().find(|m| Arc::ptr_eq(&m.at, &child)) {
                            Some(m) => m.root.clone(),
                            None => child,
                        }
                    });
                    stack.push(child);
                }
            }
        }

        Ok(stack.pop().expect("the stack is never empty"))
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

#[cfg(test)]
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
}
