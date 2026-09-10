//! The device filesystem: named character devices over a flat directory.
//!
//! A character device is a stream, not bytes at offsets - so the [`Inode`]
//! offset contract does not fit it, and [`DevNode`] bridges the two by
//! ignoring the offset and letting the open description's cursor do the
//! sequencing. Reading a device through a `File` therefore reads the stream
//! in order, which is what every caller expects.
//!
//! Devices are registered before the filesystem is mounted and never change
//! after; directories here have no `create`, because making a device node
//! means having a driver for it, and that is the kernel's decision, not a
//! caller's.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// The whole surface of a character device, in the only units that make sense
/// for one: bytes in, bytes out.
///
/// `read` returning 0 is end of input, matching the `Inode` contract; there is
/// no blocking yet, so devices with nothing to say say so immediately.
pub trait CharDevice: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> usize;
    fn write(&self, buf: &[u8]) -> usize;
}

/// One registered device, as an inode.
struct DevNode {
    ino: Ino,
    dev: Arc<dyn CharDevice>,
}

impl Inode for DevNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            // Streams have no size; reporting one would be a lie.
            size: 0,
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(self.dev.read(buf))
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(self.dev.write(buf))
    }
}

/// The one directory, holding every device by name.
struct DevRoot {
    inodes: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
    next_ino: AtomicU64,
}

impl Inode for DevRoot {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            self.inodes
                .borrow(cs)
                .borrow()
                .get(name)
                .cloned()
                .ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            let inodes = self.inodes.borrow(cs).borrow();
            Ok(inodes
                .keys()
                .map(|name| (name.clone(), NodeKind::CharDev))
                .collect())
        })
    }
}

/// The filesystem itself: a flat name space of devices.
pub struct DevFs {
    root: Arc<DevRoot>,
}

impl DevFs {
    pub fn new() -> Arc<DevFs> {
        Arc::new(DevFs {
            root: Arc::new(DevRoot {
                inodes: Mutex::new(RefCell::new(BTreeMap::new())),
                next_ino: AtomicU64::new(1),
            }),
        })
    }

    /// Add a device under `name`. Panics on a duplicate: device names are as
    /// fixed as the drivers behind them.
    pub fn register(&self, name: &str, dev: Arc<dyn CharDevice>) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let node: Arc<dyn Inode> = Arc::new(DevNode { ino, dev });
        critical_section::with(|cs| {
            let prev = self
                .root
                .inodes
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
            assert!(prev.is_none(), "device {name} registered twice");
        });
    }
}

impl FileSystem for DevFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use critical_section::Mutex;

    use super::{CharDevice, DevFs};
    use crate::vfs::{File, FileSystem, NodeKind, VfsError};

    /// Emits its bytes once, then end of input.
    struct Source {
        bytes: &'static [u8],
        taken: AtomicUsize,
    }

    impl CharDevice for Source {
        fn read(&self, buf: &mut [u8]) -> usize {
            let start = self.taken.fetch_add(buf.len(), Ordering::Relaxed);
            if start >= self.bytes.len() {
                return 0;
            }
            let end = (start + buf.len()).min(self.bytes.len());
            buf[..end - start].copy_from_slice(&self.bytes[start..end]);
            end - start
        }

        fn write(&self, _buf: &[u8]) -> usize {
            0
        }
    }

    /// Keeps everything written to it, for the test to inspect.
    struct Sink {
        got: Mutex<core::cell::RefCell<Vec<u8>>>,
    }

    impl CharDevice for Sink {
        fn read(&self, _buf: &mut [u8]) -> usize {
            0
        }

        fn write(&self, buf: &[u8]) -> usize {
            critical_section::with(|cs| {
                self.got.borrow(cs).borrow_mut().extend_from_slice(buf);
            });
            buf.len()
        }
    }

    fn sink() -> Arc<Sink> {
        Arc::new(Sink {
            got: Mutex::new(core::cell::RefCell::new(Vec::new())),
        })
    }

    #[test]
    fn devices_appear_by_name_and_report_their_kind() {
        let fs = DevFs::new();
        fs.register(
            "null",
            Arc::new(Source {
                bytes: b"",
                taken: AtomicUsize::new(0),
            }),
        );
        let root = fs.root();

        let null = root.lookup("null").unwrap();
        let meta = null.metadata();
        assert_eq!(meta.kind, NodeKind::CharDev);
        assert_eq!(meta.size, 0);
        assert_eq!(root.lookup("nope").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn a_stream_reads_in_order_through_the_description_cursor() {
        let fs = DevFs::new();
        fs.register(
            "src",
            Arc::new(Source {
                bytes: b"abcdef",
                taken: AtomicUsize::new(0),
            }),
        );

        let file = File::new(fs.root().lookup("src").unwrap());
        let mut buf = [0u8; 4];
        // The offset the inode layer passes is ignored; the description's
        // cursor is what walks the stream.
        assert_eq!(file.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(file.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(file.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn writes_land_in_the_device() {
        let fs = DevFs::new();
        let dev = sink();
        fs.register("sink", dev.clone());

        let file = File::new(fs.root().lookup("sink").unwrap());
        assert_eq!(file.write(b"hi").unwrap(), 2);
        assert_eq!(file.write(b" there").unwrap(), 6);
        critical_section::with(|cs| {
            assert_eq!(&*dev.got.borrow(cs).borrow(), b"hi there");
        });
    }

    #[test]
    #[should_panic = "registered twice"]
    fn registering_the_same_name_twice_is_a_bug() {
        let fs = DevFs::new();
        fs.register("null", sink());
        fs.register("null", sink());
    }

    #[test]
    fn directories_here_have_no_create() {
        let fs = DevFs::new();
        assert_eq!(
            fs.root().create("node", NodeKind::CharDev).err(),
            Some(VfsError::Unsupported)
        );
    }

    #[test]
    fn read_dir_lists_every_registered_device() {
        let fs = DevFs::new();
        fs.register("null", sink());
        fs.register("sink", sink());

        let listed = fs.root().read_dir().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed.iter().all(
                |(name, kind)| *kind == NodeKind::CharDev && (name == "null" || name == "sink")
            )
        );
    }
}
