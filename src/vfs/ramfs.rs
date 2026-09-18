//! An in-memory filesystem: the reference implementation of [`Inode`], the
//! substrate for the M8 initramfs, and the shape ext2 will be compared
//! against.
//!
//! Each node owns its interior mutability as `critical_section::Mutex` over a
//! `RefCell`, which is all the exclusion a preemptible single core needs and
//! the same arrangement the frame allocator uses. Inode numbers come from a
//! global counter: nothing consumes them across filesystems yet, and ext2 will
//! report its on-disk numbers regardless.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn fresh_ino() -> Ino {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// A file: bytes with no structure of their own.
struct RamFile {
    ino: Ino,
    data: Mutex<RefCell<Vec<u8>>>,
}

impl Inode for RamFile {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: critical_section::with(|cs| self.data.borrow(cs).borrow().len() as u64),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        critical_section::with(|cs| {
            let data = self.data.borrow(cs).borrow();
            // An offset at or past the end reads nothing, which is how EOF
            // looks; clamping makes that fall out of the arithmetic.
            let start = offset.min(data.len() as u64) as usize;
            let end = (start + buf.len()).min(data.len());
            buf[..end - start].copy_from_slice(&data[start..end]);
            Ok(end - start)
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        critical_section::with(|cs| {
            let mut data = self.data.borrow(cs).borrow_mut();
            let start = offset as usize;
            if start > data.len() {
                // A hole before the write starts reads back as zeros, as POSIX
                // says an offset past end of file does.
                data.resize(start, 0);
            }
            let end = start + buf.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(buf);
            Ok(buf.len())
        })
    }

    /// `truncate(2)`: shrinking drops the tail, growing adds zeros - which is
    /// exactly what `Vec::resize` does, fill value included.
    fn truncate(&self, len: u64) -> VfsResult<()> {
        let len = usize::try_from(len).map_err(|_| VfsError::FileTooBig)?;
        critical_section::with(|cs| {
            self.data.borrow(cs).borrow_mut().resize(len, 0);
        });
        Ok(())
    }
}

/// A directory: name edges, aka the dentries of this filesystem.
/// A symbolic link: nothing but the path it stands for.
struct RamSymlink {
    ino: Ino,
    target: String,
}

impl Inode for RamSymlink {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Symlink,
            size: self.target.len() as u64,
        }
    }

    fn readlink(&self) -> VfsResult<String> {
        Ok(self.target.clone())
    }
}

/// A device node a `mknod` made: a name carrying a device kind.
///
/// No driver stands behind it - on this machine the nodes an initramfs's init
/// pre-seeds (`/dev/null`, `/dev/ptmx`) are placeholders that live exactly as
/// long as it takes the `devtmpfs` mount to cover them with real devices.
/// Reads answer end of file and writes vanish, which is the honest behaviour
/// of a null device and a harmless one for anything nobody has opened yet.
struct RamDev {
    ino: Ino,
}

impl Inode for RamDev {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

struct RamDir {
    ino: Ino,
    entries: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
}

impl Inode for RamDir {
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

    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        let node: Arc<dyn Inode> = match kind {
            NodeKind::File => Arc::new(RamFile {
                ino: fresh_ino(),
                data: Mutex::new(RefCell::new(Vec::new())),
            }),
            NodeKind::Dir => Arc::new(RamDir {
                ino: fresh_ino(),
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
            // A device node mknod made: a placeholder until a devtmpfs mount
            // covers the directory with driver-backed devices.
            NodeKind::CharDev | NodeKind::BlockDev => Arc::new(RamDev {
                ino: fresh_ino(),
            }),
            // A link is made with `Inode::symlink`, which carries the one thing
            // a link cannot exist without: what it points at.
            NodeKind::Symlink => return Err(VfsError::Unsupported),
        };
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        let node: Arc<dyn Inode> = Arc::new(RamSymlink {
            ino: fresh_ino(),
            target: String::from(target),
        });
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    /// Drop the name edge. The node the name pointed at lives on while any
    /// open descriptor or other edge holds it; the name is free at once.
    ///
    /// A directory is refused, as it is on every other filesystem and as Linux
    /// answers: `unlink` is for names that hold contents, and a directory holds
    /// `.` and its children. Removing one through this call would be a
    /// recursive delete arriving through the wrong door - it is what `rmdir`
    /// exists to say no to.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        let child = self.lookup(name)?;
        if child.metadata().kind == NodeKind::Dir {
            return Err(VfsError::IsDir);
        }
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            entries.remove(name).map(|_| ()).ok_or(VfsError::NoEntry)
        })
    }

    /// A hard link: a second name edge to the node another name already
    /// reaches. Here that is literally all one is - the same `Arc` under a
    /// second key - so both names read, write and truncate the same bytes,
    /// and the data survives either name being unlinked while one edge
    /// remains. Directories are refused, as `link(2)` does: a cycle of name
    /// edges is not something a tree this shape can survive.
    fn link(&self, name: &str, target: &Arc<dyn Inode>) -> VfsResult<()> {
        if target.metadata().kind == NodeKind::Dir {
            return Err(VfsError::Invalid);
        }
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            if entries.contains_key(name) {
                return Err(VfsError::Exists);
            }
            entries.insert(alloc::string::String::from(name), target.clone());
            Ok(())
        })
    }

    /// Drop the name edge to an **empty** directory.
    ///
    /// Nothing here counts links, so the whole of an `rmdir` is: the name must
    /// be a directory, and it must have nothing under it. The child's own
    /// listing is the only witness for the second, which is why this asks the
    /// node rather than the table.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        let child = self.lookup(name)?;
        if child.metadata().kind != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        if !child.read_dir()?.is_empty() {
            return Err(VfsError::NotEmpty);
        }
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            entries.remove(name).map(|_| ()).ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            let entries = self.entries.borrow(cs).borrow();
            // The map's order is sorted, which is as good an order as any.
            Ok(entries
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }
}

/// A mounted ramfs instance: one root directory and everything under it.
pub struct RamFs {
    root: Arc<RamDir>,
}

impl RamFs {
    pub fn new() -> Arc<RamFs> {
        Arc::new(RamFs {
            root: Arc::new(RamDir {
                ino: fresh_ino(),
                entries: Mutex::new(RefCell::new(BTreeMap::new())),
            }),
        })
    }
}

impl FileSystem for RamFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "ramfs"
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::RamFs;
    use crate::vfs::{FileSystem, NodeKind, VfsError};

    #[test]
    fn create_and_lookup_round_trips_names() {
        let fs = RamFs::new();
        let root = fs.root();
        root.create("etc", NodeKind::Dir).unwrap();
        let etc = root.lookup("etc").unwrap();
        assert_eq!(etc.metadata().kind, NodeKind::Dir);
        assert_eq!(root.lookup("nope").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn creating_an_existing_name_is_an_error_and_keeps_the_old_node() {
        let fs = RamFs::new();
        let root = fs.root();
        let first = root.create("f", NodeKind::File).unwrap();
        assert_eq!(
            root.create("f", NodeKind::File).err(),
            Some(VfsError::Exists)
        );
        let again = root.lookup("f").unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn files_read_and_write_at_offsets_and_track_size() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File).unwrap();

        assert_eq!(file.write_at(0, b"hello").unwrap(), 5);
        assert_eq!(file.metadata().size, 5);
        assert_eq!(file.write_at(7, b"!").unwrap(), 1);
        assert_eq!(file.metadata().size, 8);

        let mut buf = [0u8; 16];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"hello\0\0!");
        // A read starting past the end is simply empty.
        assert_eq!(file.read_at(100, &mut buf).unwrap(), 0);
    }

    #[test]
    fn short_reads_report_where_the_file_ends() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File).unwrap();
        file.write_at(0, b"abc").unwrap();

        let mut buf = [0u8; 8];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(file.read_at(2, &mut buf).unwrap(), 1);
    }

    #[test]
    fn truncate_shrinks_drops_the_tail_and_grows_a_hole() {
        let fs = RamFs::new();
        let file = fs.root().create("f", NodeKind::File).unwrap();
        file.write_at(0, b"0123456789").unwrap();

        file.truncate(4).unwrap();
        assert_eq!(file.metadata().size, 4);
        let mut buf = [0u8; 16];
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"0123", "the tail is gone, not just hidden");

        file.truncate(8).unwrap();
        assert_eq!(file.metadata().size, 8);
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..4], b"0123");
        assert_eq!(&buf[4..8], b"\0\0\0\0", "the new space reads as zeros");

        file.truncate(0).unwrap();
        assert_eq!(file.metadata().size, 0);
        assert_eq!(file.read_at(0, &mut buf).unwrap(), 0);
    }

    #[test]
    fn rmdir_takes_an_empty_directory_and_refuses_the_rest() {
        let fs = RamFs::new();
        let root = fs.root();

        // A file is not a directory, and `rmdir` is not `unlink` by another
        // name: the two have to stay apart or a recursive delete is one typo
        // away from a file.
        root.create("f", NodeKind::File).unwrap();
        assert_eq!(root.rmdir("f").err(), Some(VfsError::NotDir));
        assert!(root.lookup("f").is_ok(), "and the file is untouched");

        let dir = root.create("d", NodeKind::Dir).unwrap();
        dir.create("inner", NodeKind::File).unwrap();
        assert_eq!(root.rmdir("d").err(), Some(VfsError::NotEmpty));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        // And the converse, from the other side: `unlink` refuses a directory,
        // so neither call can be made to do the other's work by name.
        assert_eq!(root.unlink("d").err(), Some(VfsError::IsDir));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        dir.unlink("inner").unwrap();
        root.rmdir("d").unwrap();
        assert_eq!(root.lookup("d").err(), Some(VfsError::NoEntry));

        // A name that was never there answers for itself, not for the
        // directory it would have been in.
        assert_eq!(root.rmdir("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn directories_reject_file_operations() {
        let fs = RamFs::new();
        let root = fs.root();
        assert_eq!(root.read_at(0, &mut []).unwrap_err(), VfsError::Unsupported);
        assert_eq!(root.write_at(0, b"x").unwrap_err(), VfsError::Unsupported);
    }

    #[test]
    fn read_dir_names_every_entry_with_its_kind() {
        let fs = RamFs::new();
        let root = fs.root();
        root.create("b", NodeKind::Dir).unwrap();
        root.create("a", NodeKind::File).unwrap();

        let listed = root.read_dir().unwrap();
        assert_eq!(listed.len(), 2, "every entry appears exactly once");
        assert!(listed.iter().any(|(n, k)| n == "a" && *k == NodeKind::File));
        assert!(listed.iter().any(|(n, k)| n == "b" && *k == NodeKind::Dir));

        // A file has no entries to list.
        let file = root.lookup("a").unwrap();
        assert_eq!(file.read_dir().unwrap_err(), VfsError::NotDir);
    }

    #[test]
    fn unlink_removes_the_name_and_frees_it_for_reuse() {
        let fs = RamFs::new();
        let root = fs.root();
        let file = root.create("f", NodeKind::File).unwrap();
        file.write_at(0, b"x").unwrap();

        root.unlink("f").unwrap();
        assert_eq!(root.lookup("f").err(), Some(VfsError::NoEntry));
        // The name is gone but the node survives behind the Arc, writable.
        assert_eq!(file.write_at(0, b"y").unwrap(), 1);
        // The freed name can be created again, as a fresh node.
        let fresh = root.create("f", NodeKind::File).unwrap();
        assert!(!Arc::ptr_eq(&file, &fresh));
        // Removing a name that is not there answers NoEntry.
        assert_eq!(root.unlink("f").err(), None);
        assert_eq!(root.unlink("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn deep_trees_nest_through_the_trait_alone() {
        let fs = RamFs::new();
        let root = fs.root();
        let a = root.create("a", NodeKind::Dir).unwrap();
        let b = a.create("b", NodeKind::Dir).unwrap();
        let f = b.create("f", NodeKind::File).unwrap();
        f.write_at(0, b"deep").unwrap();
        assert_eq!(
            root.lookup("a")
                .unwrap()
                .lookup("b")
                .unwrap()
                .lookup("f")
                .unwrap()
                .read_at(0, &mut [0u8; 4])
                .unwrap(),
            4
        );
    }
}
