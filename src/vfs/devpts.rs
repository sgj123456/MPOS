//! `devpts`: the pseudo-terminal filesystem.
//!
//! Linux mounts this at `/dev/pts`, and an initramfs's init does too - then
//! gets its login on a pty. This kernel has no pseudo-terminal driver yet, so
//! the honest tree is a directory with a `pts` subdirectory and nothing in it:
//! a program that looks for a terminal pair finds none, by name, rather than
//! finding one that cannot allocate.
//!
//! What the mount buys is the thing the caller actually asked for - `/dev/pts`
//! exists, is a mount point, and reports itself as `devpts` in /proc/mounts -
//! so a boot script that mounts it and moves on keeps moving.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

struct PtsDir {
    ino: Ino,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl PtsDir {
    fn new(ino: Ino) -> Arc<PtsDir> {
        Arc::new(PtsDir {
            ino,
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
        })
    }

    fn add_dir(&self, name: &str, ino: Ino) -> Arc<PtsDir> {
        let dir = PtsDir::new(ino);
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), dir.clone());
        });
        dir
    }
}

impl Inode for PtsDir {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow()
                .get(name)
                .cloned()
                .ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            Ok(self
                .entries
                .borrow(cs)
                .borrow()
                .keys()
                .map(|name| (name.clone(), NodeKind::Dir))
                .collect())
        })
    }
}

/// The devpts root: a `pts` directory, which is where the terminals would be.
pub struct DevPts {
    root: Arc<PtsDir>,
}

impl DevPts {
    pub fn new() -> Arc<DevPts> {
        let root = PtsDir::new(1);
        let _pts = root.add_dir("pts", 2);
        Arc::new(DevPts { root })
    }
}

impl FileSystem for DevPts {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "devpts"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_has_the_directory_terminals_would_live_in() {
        let fs = DevPts::new();
        let pts = fs.root().lookup("pts").expect("/dev/pts");
        assert_eq!(pts.metadata().kind, NodeKind::Dir);
        assert!(
            pts.read_dir().unwrap().is_empty(),
            "no terminal pairs exist to list"
        );
        assert_eq!(fs.name(), "devpts");
    }
}
