//! `/sys`: the sysfs tree.
//!
//! On Linux this is where drivers publish themselves, and it is a large and
//! growing surface. This kernel has a handful of drivers with fixed names, and
//! every one of them is already reachable through `/dev` - so what sysfs has
//! to be here is *mountable and truthful*: a directory tree that says "there
//! is nothing else to discover" rather than one that pretends.
//!
//! That is not a placeholder for later so much as the honest size of the
//! answer: a tree that enumerates devices would be a second copy of the devfs
//! registry, and userland that wanted the devices already has it.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// A sysfs directory. No entries are generated and none can be made: what
/// this tree says is fixed by what the kernel has, and the kernel has names
/// enough in `/dev`.
struct SysDir {
    ino: Ino,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl SysDir {
    fn empty(ino: Ino) -> Arc<SysDir> {
        Arc::new(SysDir {
            ino,
            entries: Mutex::new(RefCell::new(BTreeMap::new())),
        })
    }

    fn add(&self, name: &str, node: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
        });
    }
}

impl Inode for SysDir {
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
            let entries = self.entries.borrow(cs).borrow();
            Ok(entries
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }
}

/// The filesystem itself: a small fixed tree.
pub struct SysFs {
    root: Arc<SysDir>,
    next_ino: Mutex<RefCell<Ino>>,
}

impl SysFs {
    pub fn new() -> Arc<SysFs> {
        let fs = Arc::new(SysFs {
            root: SysDir::empty(0),
            next_ino: Mutex::new(RefCell::new(1)),
        });
        let ino = fs.ino();
        fs.root.add("class", SysDir::empty(ino));
        let ino = fs.ino();
        fs.root.add("block", SysDir::empty(ino));
        let ino = fs.ino();
        fs.root.add("devices", SysDir::empty(ino));
        let ino = fs.ino();
        let kernel = SysDir::empty(ino);
        kernel.add(
            "ostype",
            Arc::new(SysFile {
                ino: fs.ino(),
                content: String::from(env!("CARGO_PKG_NAME")),
            }),
        );
        fs.root.add("kernel", kernel);
        fs
    }

    fn ino(&self) -> Ino {
        critical_section::with(|cs| {
            let next = self.next_ino.borrow(cs);
            let mut next = next.borrow_mut();
            let ino = *next;
            *next += 1;
            ino
        })
    }
}

/// A read-only file with fixed content - the only kind sysfs has here.
struct SysFile {
    ino: Ino,
    content: String,
}

impl Inode for SysFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: self.content.len() as u64,
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let bytes = self.content.as_bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + buf.len()).min(bytes.len());
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        Ok(end - start)
    }
}

impl FileSystem for SysFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "sysfs"
    }
}
