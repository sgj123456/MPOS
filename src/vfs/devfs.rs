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
/// `read` returning 0 is end of input, matching the `Inode` contract. A
/// device with nothing to say yet may block instead: the tty does, and it is
/// the device's own business to decide, not this trait's.
pub trait CharDevice: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> usize;
    fn write(&self, buf: &[u8]) -> usize;
}

/// One registered device, as an inode.
struct DevNode {
    ino: Ino,
    dev: Arc<dyn CharDevice>,
    /// The device number `st_rdev` reports - the identity libc's `ttyname`
    /// matches descriptors by when it walks `/dev`.
    dev_id: (u32, u32),
}

impl Inode for DevNode {
    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.dev_id)
    }
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

/// A block device, as an inode: the disk and each of its partitions get a
/// name here, which is what makes them reachable from userland at all.
///
/// Reading one is reading the storage at an offset - a block device is the one
/// device whose `read_at` offset means something, and the one inode whose
/// [`Inode::as_block`] answers, because `mount(2)` receives its source as a
/// *path* and has to arrive at storage through the tree.
struct DevBlockNode {
    ino: Ino,
    dev: Arc<dyn crate::blk::BlockDev>,
}

impl Inode for DevBlockNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::BlockDev,
            size: self.dev.capacity_sectors() * 512,
        }
    }

    fn as_block(&self) -> Option<Arc<dyn crate::blk::BlockDev>> {
        Some(self.dev.clone())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        // Reads past the end are an empty read, the same answer a file gives:
        // a caller sizing the device by reading it gets a stop rather than a
        // panic out of the driver underneath.
        let end = self.dev.capacity_sectors() * 512;
        if offset >= end {
            return Ok(0);
        }
        let len = buf.len().min((end - offset) as usize);
        if len == 0 {
            return Ok(0);
        }
        // A block device moves whole sectors from a sector boundary, and a
        // caller reading one does not: probing a filesystem reads the few
        // hundred bytes of a superblock out of the middle of the first
        // sector. The sector-sized window covering the request is read, and
        // the part that was asked for is copied out of it.
        let (first, covered) = window_for(offset, len as u64, end);
        let mut window = Vec::new();
        window.resize(covered as usize, 0);
        self.dev.read_at(first, &mut window);
        let from = (offset - first) as usize;
        buf[..len].copy_from_slice(&window[from..from + len]);
        Ok(len)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let end = self.dev.capacity_sectors() * 512;
        if offset >= end {
            return Err(VfsError::NoSpace);
        }
        let len = buf.len().min((end - offset) as usize);
        if len == 0 {
            return Ok(0);
        }
        // The read twin of the window above, with the same reason: a write
        // that does not cover whole sectors has to read them first, or it
        // would erase the bytes around what it was given.
        let (first, covered) = window_for(offset, len as u64, end);
        let mut window = Vec::new();
        window.resize(covered as usize, 0);
        let at = (offset - first) as usize;
        self.dev.read_at(first, &mut window);
        window[at..at + len].copy_from_slice(&buf[..len]);
        self.dev.write_at(first, &window);
        Ok(len)
    }
}

/// A sector, the unit a block device moves.
const SECTOR: u64 = 512;

/// The sector-aligned window that covers `offset..offset + len` of a device
/// whose last byte is at `end`: where it starts and how many bytes it holds.
///
/// Both answers are in whole sectors because that is the only shape the
/// device underneath accepts, and the window is clipped to the device because
/// a read of its last partial sector must not ask for the sector after it.
fn window_for(offset: u64, len: u64, end: u64) -> (u64, u64) {
    let first = offset - offset % SECTOR;
    let last = offset + len;
    let wanted = last + (SECTOR - last % SECTOR) % SECTOR;
    (first, (wanted.min(end) - first).max(SECTOR))
}

/// The one directory, holding every device by name.
struct DevRoot {
    inodes: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
    next_ino: Arc<AtomicU64>,
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
            // The kind each name actually carries: a listing that calls the
            // directories char devices is how `ls /dev` lies.
            Ok(inodes
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }

    /// Make a directory under `/dev`. Real devtmpfs lets userland make
    /// directories too - `mkdir` answered ENOSYS here, and the script Alpine's
    /// mdev runs for every hotplugged disk builds `/dev/block` and
    /// `/dev/disk` before it links anything into them, so its whole
    /// persistent-name pass died one syscall in.
    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        if kind != NodeKind::Dir {
            return Err(VfsError::Unsupported);
        }
        critical_section::with(|cs| {
            let mut inodes = self.inodes.borrow(cs).borrow_mut();
            if inodes.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = Arc::new(DevDirNode {
                ino,
                children: Mutex::new(RefCell::new(BTreeMap::new())),
                next_ino: self.next_ino.clone(),
            });
            inodes.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    /// Name a symbolic link under `/dev`. mdev's persistent-storage script
    /// links every hotplugged disk into `/dev/block` and `/dev/disk` - without
    /// this the whole pass died on the first `ln -sf`.
    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            let mut inodes = self.inodes.borrow(cs).borrow_mut();
            if inodes.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = Arc::new(DevSymlinkNode {
                ino,
                target: String::from(target),
            });
            inodes.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    /// Drop the child called `name`. `rm -f` in mdev's link script needs this,
    /// and a name that was registered by a driver goes with it - the driver
    /// still owns the device, the name was just one way to reach it.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        critical_section::with(|cs| {
            self.inodes
                .borrow(cs)
                .borrow_mut()
                .remove(name)
                .map(|_| ())
                .ok_or(VfsError::NoEntry)
        })
    }
}

/// A symbolic link under `/dev`, as mdev's persistent-name pass makes: the
/// target is a path, carried verbatim for [`Inode::readlink`].
struct DevSymlinkNode {
    ino: Ino,
    target: String,
}

impl Inode for DevSymlinkNode {
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

/// A plain directory under `/dev` - `/dev/pts`, `/dev/shm` - which exists to
/// be a mountpoint, and the subdirectories mdev makes below `/dev/disk`.
/// Mountpoint directories hold nothing and refuse every name: what lands on
/// them is a whole filesystem. The ones userland creates hold children, so
/// the node carries a table and answers lookups, creation, linking and
/// unlinking for them.
struct DevDirNode {
    ino: u64,
    children: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
    next_ino: Arc<AtomicU64>,
}

impl Inode for DevDirNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow()
                .get(name)
                .cloned()
                .ok_or(VfsError::NoEntry)
        })
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        critical_section::with(|cs| {
            let children = self.children.borrow(cs).borrow();
            Ok(children
                .iter()
                .map(|(name, node)| (name.clone(), node.metadata().kind))
                .collect())
        })
    }

    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        if kind != NodeKind::Dir {
            return Err(VfsError::Unsupported);
        }
        critical_section::with(|cs| {
            let mut children = self.children.borrow(cs).borrow_mut();
            if children.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = Arc::new(DevDirNode {
                ino,
                children: Mutex::new(RefCell::new(BTreeMap::new())),
                next_ino: self.next_ino.clone(),
            });
            children.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        critical_section::with(|cs| {
            let mut children = self.children.borrow(cs).borrow_mut();
            if children.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = Arc::new(DevSymlinkNode {
                ino,
                target: String::from(target),
            });
            children.insert(String::from(name), node.clone());
            Ok(node)
        })
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow_mut()
                .remove(name)
                .map(|_| ())
                .ok_or(VfsError::NoEntry)
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
                next_ino: Arc::new(AtomicU64::new(1)),
            }),
        })
    }

    /// Add a device under `name`. Panics on a duplicate: device names are as
    /// fixed as the drivers behind them.
    ///
    /// Registering a device also *announces* it: userland that has to find a
    /// device - an initramfs looking for its root, udev building `/dev` -
    /// learns about it from the event, and a device that is never announced
    /// is one nothing can find.
    pub fn register(&self, name: &str, dev: Arc<dyn CharDevice>) {
        self.register_id(name, dev, 0, 0)
    }

    /// [`register`](Self::register) with the device number the node reports
    /// as `st_rdev` - the tty names need real ones, because libc's `ttyname`
    /// identifies a descriptor by matching the number against `/dev`'s nodes.
    pub fn register_id(&self, name: &str, dev: Arc<dyn CharDevice>, major: u32, minor: u32) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let node: Arc<dyn Inode> = Arc::new(DevNode {
            ino,
            dev,
            dev_id: (major, minor),
        });
        critical_section::with(|cs| {
            let prev = self
                .root
                .inodes
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
            assert!(prev.is_none(), "device {name} registered twice");
        });
        announce(name, subsystem_of(name), 0, 0);
    }

    /// Add an empty directory under `name`, for the mountpoints userland
    /// expects `/dev` to carry: an init mounting devpts asks for `/dev/pts`
    /// the way it asks for `/proc`, and a devtmpfs that cannot hold the
    /// directory sends it looking for a tmpfs to mount a devpts on.
    pub fn register_dir(&self, name: &str) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let node: Arc<dyn Inode> = Arc::new(DevDirNode {
            ino,
            children: Mutex::new(RefCell::new(BTreeMap::new())),
            next_ino: self.root.next_ino.clone(),
        });
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

    /// Add an already-built inode under `name`.
    ///
    /// The tty is the reason this exists: a real devfs's tty names are all
    /// doors into one line discipline, whose termios and `as_tty` behaviour a
    /// bare character-device wrapper cannot answer - getty's `TIOCSCTTY`,
    /// a shell's `TCGETS` and every `isatty` ask the node itself.
    pub fn register_node(&self, name: &str, inode: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            let prev = self
                .root
                .inodes
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), inode);
            assert!(prev.is_none(), "device {name} registered twice");
        });
    }

    /// Add a block device under `name` - the disk itself, or one partition of
    /// it. These are the names `mount(2)` is pointed at, and the reason the
    /// choice of what to mount belongs to userland.
    pub fn register_block(&self, name: &str, dev: Arc<dyn crate::blk::BlockDev>) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let node: Arc<dyn Inode> = Arc::new(DevBlockNode { ino, dev });
        critical_section::with(|cs| {
            let prev = self
                .root
                .inodes
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), node);
            assert!(prev.is_none(), "device {name} registered twice");
        });
        announce_block(name);
    }
}

/// Announce a device on the event bus.
///
/// A device that is never announced is one no listener can find - which is
/// what an initramfs looking for its root does. The bus itself lives with the
/// network stack's module tree, so on the host (where there is no network
/// stack and nothing listening) this is a no-op.
fn announce(name: &str, subsystem: &'static str, major: u32, minor: u32) {
    let devpath = if subsystem == "block" {
        alloc::format!("/devices/virtio/block/{}", name)
    } else {
        alloc::format!("/devices/virtual/{}", name)
    };
    crate::uevent::publish(&crate::uevent::Event {
        action: "add",
        devpath,
        subsystem,
        devname: String::from(name),
        major,
        minor,
    });
}

/// Announce a block device, with the numbering `/sys` and `/dev` agree on:
/// a listener matches an event against a device by exactly these numbers.
fn announce_block(name: &str) {
    let numbering = crate::uevent::Numbering::of_boot_disk();
    let (major, minor, _whole) = numbering.find(name).unwrap_or((0, 0, false));
    announce(name, "block", major, minor);
}

/// The subsystem a device belongs to, by the name it is registered under.
///
/// These are Linux's own subsystem names - `/sys/class/tty`, `/sys/class/mem`,
/// `/sys/class/misc` - because a listener sorts events by them, and an
/// invented name is an event it cannot place.
fn subsystem_of(name: &str) -> &'static str {
    match name {
        "tty" | "console" | "ttyS0" => "tty",
        "null" | "zero" | "random" | "urandom" => "mem",
        _ => "misc",
    }
}

impl FileSystem for DevFs {
    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn name(&self) -> &'static str {
        "devfs"
    }
}

#[cfg(all(test, not(target_os = "none")))]
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
