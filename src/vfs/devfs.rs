//! The device filesystem: named character devices over a flat directory.
//!
//! A character device is a stream, not bytes at offsets - so the [`Inode`]
//! offset contract does not fit it, and [`DevNode`] bridges the two by
//! ignoring the offset and letting the open description's cursor do the
//! sequencing. Reading a device through a `File` therefore reads the stream
//! in order, which is what every caller expects.
//!
//! Devices are registered before the filesystem is mounted; the node a
//! driver registered is the driver's, and a caller cannot make one by name -
//! making a device node means having a driver for it. What a caller *can*
//! make here are directories (mdev's persistent-name pass) and socket nodes:
//! real devtmpfs lets `bind(2)` create a `SOCK_FILE` in `/dev`, and busybox
//! syslog's `/dev/log` is exactly that - a name whose only content is the
//! unix registry's key, so the node needs no stream face at all.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// The whole surface of a character device, in the only units that make sense
/// for one: bytes in, bytes out.
///
/// `read` returning `Ok(0)` is end of input, matching the `Inode` contract. A
/// device with nothing to say yet may block instead: the tty does, and it is
/// the device's own business to decide, not this trait's.
///
/// A read therefore answers a `VfsResult`, because a blocking device has a
/// third answer a byte count cannot carry: a signal ended the wait, and the
/// caller must be told `EINTR` rather than be handed 0 - which on the read side
/// means the terminal closed. A program that blocked in `read` on its own
/// terminal and had it interrupted by its `SIGALRM` timeout is exactly the
/// caller that cannot afford to be told its terminal is gone.
pub trait CharDevice: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize>;
    fn write(&self, buf: &[u8]) -> usize;

    /// The error a write should report instead of succeeding, if any.
    ///
    /// A stream reports "how many bytes landed", and there is no count that
    /// means "the write was refused". `/dev/full` is the device that needs
    /// one: its whole purpose is to be written to and to answer ENOSPC, and a
    /// write that returned zero bytes would read as a successful short write
    /// rather than as a full disk - the failure the device exists to provoke.
    fn write_error(&self) -> Option<VfsError> {
        None
    }

    /// The device's own `ioctl` answer, if this device speaks any request:
    /// the full `_IOC` word and the caller's argument, whose validity as a
    /// buffer the syscall layer has already checked. `None` - the answer for
    /// every device without an ioctl surface - says the request is as
    /// unknown here as it would be on Linux, and the syscall answers
    /// `ENOTTY`. A device that has the request and refuses *it* answers
    /// `Some(EINVAL)`, because refusal is the device's answer to give, not
    /// the dispatcher's.
    fn ioctl(&self, _request: u32, _arg: usize) -> Option<isize> {
        None
    }
}

/// The mode and ownership one device node keeps, behind the same
/// `critical_section` door everything else in this filesystem uses.
///
/// A device's *existence* belongs to its driver, but its permissions belong
/// to the machine: Linux's devtmpfs creates each node with the driver's
/// default mode - a strict one, `0600 root:root`, until the filesystem predates
/// the mount - and it is udev's rules that hand back `0660 root:tty` and
/// friends. For those rules to be more than a log line, `chmod(2)` and
/// `chown(2)` on a device node have to *stick*: a node that answers success
/// while keeping nothing is the silent approximation this kernel's rules put
/// above any convenience, and it is what `stat` reported a mode that no check
/// ever read.
///
/// A device inode that is its own `Inode` implementation - the tty, the DRM
/// card, an event node - holds one of these and forwards the five trait
/// methods to it; that is the whole of the mode story for that node.
pub struct NodePerms {
    mode: Mutex<RefCell<u32>>,
    owner: Mutex<RefCell<(u32, u32)>>,
}

impl NodePerms {
    /// The node's mode at birth, and its owner. Numbers are the low twelve
    /// permission bits only; the kind bits belong to [`Metadata::kind`].
    pub const fn new(mode: u32, uid: u32, gid: u32) -> NodePerms {
        NodePerms {
            mode: Mutex::new(RefCell::new(mode)),
            owner: Mutex::new(RefCell::new((uid, gid))),
        }
    }

    /// The five answers an `Inode` forwards - see [`Inode::keeps_modes`].
    /// `keeps` is true by construction: a node carrying one of these is a
    /// node that stores what it is told.
    pub fn keeps(&self) -> bool {
        true
    }

    pub fn mode(&self) -> u32 {
        critical_section::with(|cs| *self.mode.borrow(cs).borrow())
    }

    pub fn set_mode(&self, mode: u32) {
        critical_section::with(|cs| *self.mode.borrow(cs).borrow_mut() = mode & 0o7777);
    }

    pub fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| *self.owner.borrow(cs).borrow())
    }

    pub fn set_owner(&self, uid: u32, gid: u32) {
        critical_section::with(|cs| *self.owner.borrow(cs).borrow_mut() = (uid, gid));
    }
}

/// One registered device, as an inode.
struct DevNode {
    ino: Ino,
    dev: Arc<dyn CharDevice>,
    /// The device number `st_rdev` reports - the identity libc's `ttyname`
    /// matches descriptors by when it walks `/dev`.
    dev_id: (u32, u32),
    /// What `chmod(2)`/`chown(2)` on this node leave behind, and what the
    /// permission checks read. Devices are born `0666 root:root` here - the
    /// mode `stat` already reported for them before any of this existed, and
    /// the number udev's rules then tighten or hand out by group. The named
    /// gap in `NOTES.md` keeps Linux's *devtmpfs* birth (`0600` until udev);
    /// what this commit ends is the silence, not that difference.
    perms: NodePerms,
}

impl Inode for DevNode {
    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.dev_id)
    }

    fn keeps_modes(&self) -> bool {
        self.perms.keeps()
    }

    fn mode(&self) -> u32 {
        self.perms.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.perms.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.perms.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.perms.set_owner(uid, gid);
    }

    fn as_chardev(&self) -> Option<&dyn CharDevice> {
        Some(&*self.dev)
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
        self.dev.read(buf)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // A device that refuses writes does so before the write is offered to
        // it: the stream interface has no way to say no, and a device whose
        // answer is always ENOSPC should not have its bytes copied first.
        if let Some(error) = self.dev.write_error() {
            return Err(error);
        }
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
    /// The device number `st_rdev` reports, and the number a mount of this
    /// node takes as its filesystem's `st_dev`.
    ///
    /// A block device without one is a node whose own identity is missing: `ls
    /// -l /dev/vda2` has no `254, 2` to print it by, `stat(2)` on it answers
    /// nothing about which device it is, and a mounted filesystem has no way to
    /// say which storage it came from - which is exactly the link `lsblk` and
    /// `df` follow to put a partition and its mount point on one line.
    dev_id: (u32, u32),
}

impl Inode for DevBlockNode {
    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.dev_id)
    }

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
        let mut window = vec![0; covered as usize];
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
        let mut window = vec![0; covered as usize];
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

    /// Make a directory - or a socket name - under `/dev`. Real devtmpfs lets
    /// userland make directories too - `mkdir` answered ENOSYS here, and the
    /// script Alpine's mdev runs for every hotplugged disk builds `/dev/block`
    /// and `/dev/disk` before it links anything into them, so its whole
    /// persistent-name pass died one syscall in. Sockets are the other half:
    /// devtmpfs permits `bind(2)` of a `SOCK_FILE` here, and busybox syslog
    /// does exactly that for `/dev/log` on every boot. Device nodes stay the
    /// driver's decision; a fifo would need a pipe face to be worth making.
    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        match kind {
            NodeKind::Dir | NodeKind::Socket => {}
            _ => return Err(VfsError::Unsupported),
        }
        critical_section::with(|cs| {
            let mut inodes = self.inodes.borrow(cs).borrow_mut();
            if inodes.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = if kind == NodeKind::Dir {
                Arc::new(DevDirNode {
                    ino,
                    children: Mutex::new(RefCell::new(BTreeMap::new())),
                    next_ino: self.next_ino.clone(),
                })
            } else {
                Arc::new(DevSocketNode { ino })
            };
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

/// A socket name made by `bind(2)` - `/dev/log`.
///
/// It is a nameplate, not a pipe: the unix registry resolves a connect to a
/// live socket through this node's `(fs id, ino)` key, so the node itself
/// carries only its identity and kind. Opening the name is opening the
/// socket - which is decided by the registry lookup, not by anything here.
struct DevSocketNode {
    ino: Ino,
}

impl Inode for DevSocketNode {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::Socket,
            size: 0,
        }
    }
}

/// A plain directory under `/dev` - `/dev/pts`, `/dev/shm` - which exists to
/// be a mountpoint, and the subdirectories mdev makes below `/dev/disk`.
/// Mountpoint directories hold nothing and refuse every name: what lands on
/// them is a whole filesystem. The ones userland creates hold children, so
/// the node carries a table and answers lookups, creation, linking and
/// unlinking for them.
pub(crate) struct DevDirNode {
    ino: u64,
    children: Mutex<RefCell<BTreeMap<String, Arc<dyn Inode>>>>,
    next_ino: Arc<AtomicU64>,
}

impl DevDirNode {
    /// Place an already-built inode under this directory. This is how a
    /// driver that owns its node objects - the evdev devices, each with its
    /// own read/ioctl face - gets them into `/dev/input/` rather than the
    /// `/dev` root, which is the path udev and `libinput` look in.
    pub(crate) fn insert(&self, name: &str, inode: Arc<dyn Inode>) {
        critical_section::with(|cs| {
            self.children
                .borrow(cs)
                .borrow_mut()
                .insert(String::from(name), inode);
        });
    }

    /// Take a node back out of this directory, and say whether it was there.
    ///
    /// A device userland made and then destroyed needs this: its name was
    /// allocated to it for as long as it exists, and a `/dev/input/eventN` that
    /// outlives its device is a name that opens onto nothing - and one the next
    /// device of that kind can never be given, since names here are not reused
    /// by accident.
    pub(crate) fn remove(&self, name: &str) -> bool {
        critical_section::with(|cs| self.children.borrow(cs).borrow_mut().remove(name).is_some())
    }
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
        match kind {
            NodeKind::Dir | NodeKind::Socket => {}
            _ => return Err(VfsError::Unsupported),
        }
        critical_section::with(|cs| {
            let mut children = self.children.borrow(cs).borrow_mut();
            if children.contains_key(name) {
                return Err(VfsError::Exists);
            }
            let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
            let node: Arc<dyn Inode> = if kind == NodeKind::Dir {
                Arc::new(DevDirNode {
                    ino,
                    children: Mutex::new(RefCell::new(BTreeMap::new())),
                    next_ino: self.next_ino.clone(),
                })
            } else {
                Arc::new(DevSocketNode { ino })
            };
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
            perms: NodePerms::new(0o666, 0, 0),
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
        announce(name, subsystem_of(name), "/devices/virtual", 0, 0);
    }

    /// Add an empty directory under `name`, for the mountpoints userland
    /// expects `/dev` to carry: an init mounting devpts asks for `/dev/pts`
    /// the way it asks for `/proc`, and a devtmpfs that cannot hold the
    /// directory sends it looking for a tmpfs to mount a devpts on.
    pub(crate) fn register_dir(&self, name: &str) -> Arc<DevDirNode> {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let node = Arc::new(DevDirNode {
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
                .insert(String::from(name), node.clone() as Arc<dyn Inode>);
            assert!(prev.is_none(), "device {name} registered twice");
        });
        node
    }

    /// Add a symbolic link under `name`.
    ///
    /// `/dev/fd`, `/dev/stdin`, `/dev/stdout` and `/dev/stderr` are links on
    /// Linux - into `/proc/self/fd` and the three descriptors inside it - and
    /// a script that names `/dev/stdout` is naming a descriptor, not a device:
    /// `echo x > /dev/stdout` has to land wherever the shell's fd 1 goes. A
    /// device node there would be a second, unrelated thing that happened to
    /// answer to the same name.
    pub fn register_symlink(&self, name: &str, target: &str) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        critical_section::with(|cs| {
            let prev = self.root.inodes.borrow(cs).borrow_mut().insert(
                String::from(name),
                Arc::new(DevSymlinkNode {
                    ino,
                    target: String::from(target),
                }) as Arc<dyn Inode>,
            );
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
    ///
    /// The number the node carries comes from the system's numbering, asked by
    /// name - the same lookup the `add` event this publishes is built from, so
    /// `/dev/vda2`'s `st_rdev`, the `dev` attribute `/sys/class/block/vda2`
    /// publishes and the numbers a listener matches the event by are one fact
    /// with one source rather than three copies of it. A caller therefore does
    /// not pass a major and a minor in: only the disk's own driver knows which
    /// major its disks are numbered under, and it has already said so.
    pub fn register_block(&self, name: &str, dev: Arc<dyn crate::blk::BlockDev>) {
        let ino = self.root.next_ino.fetch_add(1, Ordering::Relaxed);
        let dev_id = crate::uevent::Numbering::of_this_machine()
            .find(name)
            .map(|(major, minor, _whole)| (major, minor))
            .unwrap_or((0, 0));
        let node: Arc<dyn Inode> = Arc::new(DevBlockNode { ino, dev, dev_id });
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
fn announce(name: &str, subsystem: &'static str, devpath: &str, major: u32, minor: u32) {
    crate::uevent::publish(&crate::uevent::Event {
        action: "add",
        devpath: alloc::format!("{devpath}/{name}"),
        subsystem,
        devname: String::from(name),
        major,
        minor,
        extra: &[],
    });
}

/// Announce a block device, with the numbering `/sys` and `/dev` agree on:
/// a listener matches an event against a device by exactly these numbers.
fn announce_block(name: &str) {
    let numbering = crate::uevent::Numbering::of_this_machine();
    let (major, minor, _whole) = numbering.find(name).unwrap_or((0, 0, false));
    // The tree the name hangs under comes from the numbering, which got it from
    // the disk's own driver: a virtio disk and a SATA disk are the same shape
    // here and not the same device, and only the driver knows which it is.
    let devpath = numbering
        .devpath_of(name)
        .unwrap_or("/devices/virtual/block");
    announce(name, "block", devpath, major, minor);
}

/// The subsystem a device belongs to, by the name it is registered under.
///
/// These are Linux's own subsystem names - `/sys/class/tty`, `/sys/class/mem`,
/// `/sys/class/misc` - because a listener sorts events by them, and an
/// invented name is an event it cannot place.
fn subsystem_of(name: &str) -> &'static str {
    match name {
        // Both serial names, because this table is asked by *name* about a node
        // that is the console tty on whichever machine published it: `ttyS0` is
        // x86_64's spelling of the first serial port and `ttyAMA0` aarch64's, and
        // `drivers::devices::standard_devfs` registers exactly one of them.
        "tty" | "console" | "ttyS0" | "ttyAMA0" => "tty",
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
    use crate::vfs::{File, FileSystem, NodeKind, VfsError, VfsResult};

    /// Emits its bytes once, then end of input.
    struct Source {
        bytes: &'static [u8],
        taken: AtomicUsize,
    }

    impl CharDevice for Source {
        fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
            let start = self.taken.fetch_add(buf.len(), Ordering::Relaxed);
            if start >= self.bytes.len() {
                return Ok(0);
            }
            let end = (start + buf.len()).min(self.bytes.len());
            buf[..end - start].copy_from_slice(&self.bytes[start..end]);
            Ok(end - start)
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
        fn read(&self, _buf: &mut [u8]) -> VfsResult<usize> {
            Ok(0)
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
        let _bus = crate::uevent::test_bus::take();
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
        let _bus = crate::uevent::test_bus::take();
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
        let _bus = crate::uevent::test_bus::take();
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
        let _bus = crate::uevent::test_bus::take();
        let fs = DevFs::new();
        fs.register("null", sink());
        fs.register("null", sink());
    }

    #[test]
    fn chmod_on_a_device_sticks_and_is_policed() {
        // The whole point of the node storing a mode: an answer of success
        // to `chmod` means the number is really there afterwards, and the
        // permission checks read it - which, before this, they could not,
        // and a udev `MODE=` rule was a line of logs and nothing else.
        let _bus = crate::uevent::test_bus::take();
        let fs = DevFs::new();
        fs.register("sink", sink());
        let node = fs.root().lookup("sink").unwrap();
        assert!(node.keeps_modes(), "a device node keeps what it is told");
        assert_eq!(node.mode(), 0o666, "and is born at the Linux stream mode");

        let writable_by_anyone = (1u32, 1u32);
        assert!(crate::vfs::may_as(writable_by_anyone, &node, crate::vfs::W_OK).is_ok());

        node.set_mode(0o600);
        assert_eq!(node.mode(), 0o600);
        assert_eq!(
            crate::vfs::may_as(writable_by_anyone, &node, crate::vfs::W_OK).err(),
            Some(VfsError::Access),
            "the tightened mode is what the next check sees"
        );
        assert!(
            crate::vfs::may_as((0, 0), &node, crate::vfs::W_OK).is_ok(),
            "and root still reaches it, the way CAP_DAC_OVERRIDE does"
        );

        // chown moves the triads along with it: the new owner reads what the
        // old root grant took away.
        node.set_owner(1, 1);
        assert_eq!(node.owner(), (1, 1));
        assert!(crate::vfs::may_as(writable_by_anyone, &node, crate::vfs::W_OK).is_ok());
    }

    #[test]
    fn directories_make_dirs_and_sockets_only() {
        let fs = DevFs::new();
        // A device node - and any other kind - is still the driver's decision.
        assert_eq!(
            fs.root().create("node", NodeKind::CharDev).err(),
            Some(VfsError::Unsupported)
        );
        assert_eq!(
            fs.root().create("plain", NodeKind::File).err(),
            Some(VfsError::Unsupported)
        );
        // A socket name is the caller's: bind(2) of /dev/log arrives here.
        let sock = fs.root().create("log", NodeKind::Socket).unwrap();
        assert_eq!(sock.metadata().kind, NodeKind::Socket);
        assert_eq!(
            fs.root().lookup("log").unwrap().metadata().ino,
            sock.metadata().ino
        );
        assert_eq!(
            fs.root().create("log", NodeKind::Socket).err(),
            Some(VfsError::Exists)
        );
        // unlink retires the name, and the same name is bindable again -
        // syslog's restart path is create, unlink, create.
        fs.root().unlink("log").unwrap();
        assert_eq!(fs.root().lookup("log").err(), Some(VfsError::NoEntry));
        assert!(fs.root().create("log", NodeKind::Socket).is_ok());
    }

    #[test]
    fn read_dir_lists_every_registered_device() {
        let _bus = crate::uevent::test_bus::take();
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
