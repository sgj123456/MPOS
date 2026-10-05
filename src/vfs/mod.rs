//! The virtual filesystem: inodes, name edges, mounts, and path resolution.
//!
//! The seam here is the [`Inode`] trait. Everything above it - resolution, the
//! fd table, later syscalls - speaks only in `Arc<dyn Inode>`; everything below
//! it (ramfs, devfs, the tar initramfs, the ext2/ext4 driver) implements the
//! trait over its own storage. Operations a filesystem cannot support keep
//! their default bodies, which return errors, so a read-only filesystem never
//! writes a stub for `write_at`.
//!
//! Dentries - the name-to-inode edges - have no cache of their own yet. Each
//! filesystem holds its edges directly (ramfs in per-directory maps), and
//! [`mount::Vfs::resolve`] is the only walker, treating every edge as fresh.
//! A shared dcache becomes worth its complexity with the disk-backed
//! filesystems, whose directories are worth caching; the memory ones
//! are their own cache.
//!
//! [`mount::Vfs`] is instantiable rather than global so tests can build
//! isolated tables; the kernel holds one instance, exposed by [`mount_root`],
//! [`mount`] and [`resolve`].

pub mod devfs;
pub mod devpts;
#[cfg(target_os = "none")]
pub mod dmabuf;
pub mod epoll;
pub mod eventfd;
pub mod ext4;
pub mod file;
pub mod inotify;
pub mod locks;
#[cfg(target_os = "none")]
pub mod memfd;
pub mod mount;
/// 9P client. Reached only through `drivers::virtio::fs9p`, which is x86_64's
/// until the aarch64 drivers land (`PLAN.md` §9.1), and mounted by the gated
/// `mount -t 9p` factory in `syscall` - no core vfs module constructs it.
#[cfg(target_arch = "x86_64")]
pub mod p9;
pub mod path;
pub mod pipe;
/// `/proc`: the process tree, `/proc/net`'s socket table (built from
/// `crate::net`, which both machines have) and `/proc/bus/pci`.
///
/// Built on both machines since `/proc` became something the second machine
/// needs: a real init reads it (`openrc` mounts and reads it on every boot), and
/// nothing in the filesystem's own logic is x86_64's. The parts that *are*
/// machine-specific sit behind capability flags rather than an architecture
/// gate - the PCI config-space read/write pair behind `mpos_pci_config`, since a
/// machine may truthfully have no PCI bus - so a machine without one builds the
/// tree from the neutral `crate::pci` model (empty on aarch64) and never reaches
/// the absent method.
pub mod procfs;
pub mod pty;
pub mod ramfs;
pub mod signalfd;
/// `/sys`: the device tree `udev` and every `/sys/class` reader walk. Same
/// machine arrangement as `procfs`: the class directories are shared, and the
/// leaves that are x86_64's - the PCI config pair and the evdev registry
/// `/sys/class/input` enumerates - sit behind `mpos_pci_config` and
/// `mpos_input_registry`.
pub mod sysfs;
pub mod tar;
pub mod termios;
pub mod timerfd;
pub mod tty;
/// The console multiplexer: one serial wire, six terminals, and the switch a seat
/// manager drives between them.
///
/// Both machines since `PLAN.md` §13.16. It names no driver: what it needs from
/// the display - wake the render task, who holds the card, hand the card over - is
/// asked through `crate::console`'s `display_*` seams, which answer "nothing to do"
/// on a machine with no framebuffer. Those seven references to
/// `drivers::{video::render,drm}` were the whole of what kept it x86_64's, and with
/// them gone the second machine's `/dev/tty1`..`tty6` - which have always existed,
/// as pty-backed slaves - are terminals a login can reach over the wire.
///
/// Kept out of the host build as before (its non-driver half, the handover table,
/// is the `term` crate's `vtmode` and is host-tested there).
#[cfg(target_os = "none")]
pub mod vcmux;

pub use file::{File, RawFd, close, dup2, open, open_inode, open_opt, read, write};
pub use mount::{global, mount, mount_root, pivot_root, resolve, resolve_at, unmount};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// An inode number. Unique within the filesystem that minted it.
pub type Ino = u64;

/// What a node is, as far as this kernel cares for now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    /// A character device: reads and writes go to a driver, not storage.
    CharDev,
    /// A block device: random-access storage, addressed by offset. The kernel
    /// exposes the disk and its partitions this way, which is what puts the
    /// `mount` decision in the hands of userland - mounting "the device named
    /// by this path" is a question only the node can answer.
    BlockDev,
    /// A symbolic link: a name holding another path, followed by resolution
    /// and *not* by whoever asks for the link itself. A whole userspace can be
    /// built out of these - Alpine's `/bin` is one busybox and three hundred
    /// names pointing at it.
    Symlink,
    /// A socket: a name whose "contents" are a connection endpoint, created
    /// by `bind(2)` and answered by `connect(2)`. It is a node so that the
    /// file system owns its namespace - `ls -l` shows `srw-rw-rw-`,
    /// `find -type s` finds it, and unlinking it retires the name while the
    /// listening socket behind it lives on by its descriptors, exactly as
    /// Linux behaves. Nothing reads or writes one through the file doors;
    /// opening a socket path answers `ENXIO` here and `connect` reaches the
    /// socket layer through the bind table.
    Socket,
}

/// What a descriptor can do right now, for `poll(2)`.
///
/// `read` is "a read would not block", which includes end-of-file: a pipe whose
/// last writer has gone is readable, and reading it is what reports the end.
/// `write` is "a write would not block", which on a pipe whose last reader has
/// gone is also true - the write is what reports `EPIPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ready {
    pub read: bool,
    pub write: bool,
}

impl Ready {
    /// Everything: the answer for a node that never blocks, which is most of
    /// them. A regular file is always readable and always writable.
    pub const ALL: Ready = Ready {
        read: true,
        write: true,
    };
    /// Nothing: a pipe with no data and a writer still attached.
    pub const NONE: Ready = Ready {
        read: false,
        write: false,
    };
}

/// The facts about a node that outlive any particular open of it.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub ino: Ino,
    pub kind: NodeKind,
    /// Bytes for a file; directories report 0 rather than pretending.
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsError {
    /// ENOENT: a named entry does not exist.
    NoEntry,
    /// ENOTDIR: a path component turned out not to be a directory.
    NotDir,
    /// EISDIR: a file operation named a directory.
    IsDir,
    /// EEXIST: an entry of that name is already there.
    Exists,
    /// ENOTEMPTY: `rmdir` named a directory that still holds something.
    NotEmpty,
    /// EBUSY: the mountpoint is already mounted.
    Busy,
    /// EBADF: no such open file description.
    BadFd,
    /// The tree has no root filesystem mounted yet.
    NoRoot,
    /// EINVAL: the path is unusable, e.g. relative with no cwd yet.
    Invalid,
    /// ENOSPC: the filesystem has no free blocks or inodes left.
    NoSpace,
    /// EFBIG: a write would push the file past the filesystem's size limit.
    FileTooBig,
    /// EPIPE: a pipe's read end is gone.
    BrokenPipe,
    /// The filesystem does not support this operation.
    Unsupported,
    /// A write to a filesystem that was mounted read-only.
    ReadOnly,
    /// EIO: the filesystem says something that cannot be true - a directory
    /// entry pointing at an inode with no links left, say. Linux's answer for
    /// damage it can see but cannot repair, and a better one than either
    /// alternative: taking the machine down over it, or freeing the inode a
    /// second time and handing its blocks to two owners.
    Corrupt,
    /// EXDEV: a hard link whose two ends live on different filesystems -
    /// the one request `link(2)` refuses by design, since a name edge is
    /// the filesystem's own bookkeeping and cannot reach into another's.
    CrossDevice,
    /// A read or write that would block, on a descriptor the caller asked
    /// not to block on.
    Again,
    /// EMFILE: allocating a new descriptor would put it at or above the
    /// process's `RLIMIT_NOFILE` soft ceiling - it has used its allowance.
    TooManyDescriptors,
    /// ECANCELED: an operation undone by something outside it - a timer
    /// whose clock was set underneath it, the `TFD_TIMER_CANCEL_ON_SET`
    /// case of timerfd_create(2), where the read that would have counted
    /// expirations reports the cancellation instead.
    Canceled,
    /// EACCES: the caller's identity may not do this to the node - the
    /// directory it is writing to, or the file it is opening, says so.
    Access,
    /// EPERM: the operation is refused for a reason the node's own bits
    /// cannot express. One case: removing a name from a *sticky* directory
    /// (`/tmp`, `1777`) that belongs to somebody else - which is the whole
    /// point of the bit, and the answer Linux gives.
    NotPermitted,
    /// ENXIO: the device's name answers nothing for this caller. `/dev/tty`
    /// is the one case: a controlling terminal is a property of the opening
    /// process, and a process that has acquired none cannot open the name at
    /// all - POSIX's answer, and the one a shell reads as "job control off"
    /// rather than as a terminal it does not own.
    NoDevice,
    /// ENODEV: the descriptor's access to its device has been revoked. An
    /// evdev file description on which `EVIOCREVOKE` was asked is dead to its
    /// reader - every later `read` and `ioctl` answers `ENODEV` - which is
    /// Linux's `evdev_read`/`evdev_ioctl` behaviour for a client carrying the
    /// revoke bit, and the seat mechanism that retakes a device from a
    /// compositor that no longer owns it.
    Revoked,
    /// ENOTCONN: an operation that needs a connection found none. The
    /// listening and unnamed ends of an AF_UNIX socket read through the
    /// file door answer this - Linux's `read(2)` on a socket that is not
    /// connected - rather than the `Unsupported` "this kind of file has no
    /// reads at all", which would misdescribe a socket one call before it
    /// connects.
    NotConnected,
    /// EINTR: a wait this node was about to take was ended by a signal the
    /// caller can catch, and **nothing was consumed** - no byte read, no byte
    /// written, no offset moved. A node reports this rather than answering a
    /// count, because both of the counts it could otherwise pick are lies: 0
    /// on the read side means end of input, which is how this kernel's tty
    /// once mistook a timeout for a closed terminal, and a full count would
    /// claim bytes that never arrived.
    ///
    /// The measured Linux answer to a blocking terminal `read` interrupted by
    /// a catchable `SIGALRM`, `SIGWINCH` or `SIGCHLD` is `-1/EINTR` with the
    /// handler having run first, for every one of those signals - the wait is
    /// interruptible as a rule, not per signal.
    Interrupted,
}

pub type VfsResult<T> = Result<T, VfsError>;

/// The concrete type behind a node, for a caller that already knows which type
/// it is asking about.
///
/// A plain trait upcast - the [`core::any::Any`] supertrait on [`Inode`] is
/// what makes it one - followed by the caller's own `downcast_ref::<T>()`.
/// The *caller* is what names `T`, and the caller is the layer that owns `T`:
/// `syscall/linux/fs.rs` asks for the device it is about to drive, `net` for
/// its own socket family, a driver for its own device. Nothing in this module
/// names any of them.
///
/// A node that is not a `T` answers `None`, which is the same "not this kind of
/// node" the `as_*` method this replaced answered; the difference is that the
/// question is typed where it is asked rather than in the trait. That is the
/// whole of Phase 11 B1: before it, the trait carried a method per concrete
/// device and socket type, so it could not be compiled without naming every
/// driver, and a seventh device type meant editing the filesystem layer
/// (`PLAN.md` §11.1.1, V-1).
impl dyn Inode {
    pub fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// One node of one filesystem, in the only form the rest of the kernel sees.
///
/// Every method has a default that fails, so each filesystem overrides exactly
/// what its storage can do.
///
/// The [`core::any::Any`] supertrait is what lets a holder of `Arc<dyn Inode>`
/// reach the concrete type it knows is behind the node without this trait ever
/// naming that type - see `impl dyn Inode`'s `as_any`. It costs no impl in the
/// tree a line, because `Any` is implemented for every `'static` type already.
pub trait Inode: Send + Sync + core::any::Any {
    fn metadata(&self) -> Metadata;

    /// The named child of this directory.
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        let _ = name;
        Err(VfsError::NotDir)
    }

    /// Create a child of this directory. The kind is advisory: a filesystem
    /// that cannot represent a kind returns [`VfsError::Unsupported`].
    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        let _ = (name, kind);
        Err(VfsError::Unsupported)
    }

    /// Create a file with no name, in this directory, and hand back the node
    /// it is: `O_TMPFILE`.
    ///
    /// The caller writes it whole and then links it into the tree under the
    /// name it wants - `apk` refreshes a repository index exactly this way, so
    /// a fetch cannot be seen half-written. No directory entry holds it yet,
    /// so its link count starts at zero and [`Inode::link`] is what gives it
    /// one.
    ///
    /// The default refuses, which is what a filesystem without the feature
    /// answers and what sends a caller down the named-temporary-and-rename
    /// path it keeps for exactly this. One difference from Linux is worth
    /// naming: an unnamed file that is never linked keeps its blocks here,
    /// because nothing reclaims it when the last descriptor closes.
    fn create_tmpfile(&self) -> VfsResult<Arc<dyn Inode>> {
        Err(VfsError::Unsupported)
    }

    /// Drop the child called `name` from this directory. The node it named
    /// lives on while anything still names it; the last name going is what
    /// frees it.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        let _ = name;
        Err(VfsError::Unsupported)
    }

    /// Drop the **empty** directory called `name`.
    ///
    /// A separate operation from [`Self::unlink`] because a directory is not
    /// just a name: it holds its own `.` and every child's `..`, so removing
    /// one is a different piece of accounting, and it is only legal when there
    /// is nothing underneath. Answering `NotEmpty` is the whole reason the
    /// kernel has a separate `rmdir` - a call that removed a directory and its
    /// contents would be a recursive delete wearing the wrong name.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        let _ = name;
        Err(VfsError::Unsupported)
    }

    /// Whether this node has a *position* at all - whether `lseek(2)` on a
    /// descriptor for it can succeed.
    ///
    /// A regular file and a directory do; a pipe, a socket and a terminal do
    /// not, and Linux answers those `ESPIPE`. That refusal is load-bearing
    /// rather than pedantic: the way a reader gives back bytes it has taken
    /// too many of is to seek backwards, so a kernel that reports a position
    /// for something that has none makes the caller believe the push-back
    /// worked and throw the bytes away. `fish`'s `read` is exactly that
    /// caller, which is why a `while read` loop over a pipe stopped after one
    /// line here.
    fn seekable(&self) -> bool {
        true
    }

    /// Read at an absolute offset. Returns how many bytes landed in `buf`;
    /// fewer than asked means end of file was reached.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let _ = (offset, buf);
        Err(VfsError::Unsupported)
    }

    /// Write at an absolute offset, extending as needed. Returns bytes stored.
    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let _ = (offset, buf);
        Err(VfsError::Unsupported)
    }

    /// Resize the file to `len` bytes, the way `truncate(2)` means it: a
    /// shorter file loses its tail, a longer one gains a hole that reads as
    /// zeros, and the name keeps pointing at the same file either way.
    ///
    /// It is not a convenience. A writer that opens a name it has written
    /// before - `wget -O page.html` over yesterday's page, `rename` replacing
    /// a name that is already there - replaces the *contents*, and a writer
    /// that can only overwrite from the front leaves whatever tail was longer
    /// than the new data. The file is then corrupt in a way nothing reports:
    /// the new bytes, followed by the old ones nobody overwrote.
    fn truncate(&self, len: u64) -> VfsResult<()> {
        let _ = len;
        Err(VfsError::Unsupported)
    }

    /// The tty this node is, if it is one.
    ///
    /// A device is more than the bytes it reads and writes: `TCGETS` and
    /// `TCSETS` ask the terminal itself, and a descriptor can only say which
    /// terminal by handing back the node it points at. Everything else is not
    /// a terminal, and the termios ioctls answer `ENOTTY` for it.
    fn as_tty(&self) -> Option<&crate::vfs::tty::Tty> {
        None
    }

    /// The pty pair this node belongs to, if it is a pty master.
    ///
    /// `/dev/ptmx`'s answers are about the *pair*, not a terminal: `TIOCGPTN`
    /// names the number the slave will appear under in `/dev/pts`, and
    /// `TIOCSPTLCK` is the lock `grantpt` clears. Only a master can answer
    /// them, and the syscall layer has nothing but the inode to ask.
    fn as_pty(&self) -> Option<&crate::vfs::pty::Pty> {
        None
    }

    /// What opening this node really hands back, when the node is a door
    /// rather than a thing.
    ///
    /// `/dev/ptmx` is one name with no fixed device behind it: every open
    /// mints a *new* pty pair and returns that pair's master, the way
    /// `posix_openpt` does in one step. A shared inode cannot be "a different
    /// thing each time", so the opener is told what the name meant this time.
    /// `Ok(None)` - the default - opens the node itself; a locked pty's slave
    /// answers [`VfsError::Corrupt`] (`EIO`), which is exactly what Linux
    /// opens with.
    fn open_replacement(&self) -> VfsResult<Option<alloc::sync::Arc<dyn Inode>>> {
        Ok(None)
    }

    /// The socketpair end this node is, if it is one.
    ///
    /// A descriptor that carries a connected stream has one operation a file
    /// does not: `shutdown`, which closes one direction and leaves the other
    /// running. It is the only way a program can say "that was all I had to
    /// send" without also saying "stop reading", and the syscall layer has
    /// nothing but an `Arc<dyn Inode>` to ask with. Same shape as [`Self::as_tty`],
    /// for the same reason.
    fn as_duplex(&self) -> Option<&crate::vfs::pipe::DuplexEnd> {
        None
    }

    /// The pipe end this node is, if it is one.
    ///
    /// `splice(2)`'s contract names a pipe specifically - one of its two
    /// descriptors must be one, and a pipe forbids the explicit offsets a
    /// file takes - which is a question only the node can answer. Same
    /// shape as [`Self::as_tty`], for the same reason.
    fn as_pipe(&self) -> Option<&crate::vfs::pipe::PipeEnd> {
        None
    }

    /// The node is an eventfd, if it is one - the one place `F_SETFL`'s
    /// `O_NONBLOCK` bit has to reach an inode that blocks on read. Same shape
    /// as [`Inode::as_pipe`], for the same reason.
    fn as_eventfd(&self) -> Option<&crate::vfs::eventfd::EventFd> {
        None
    }

    /// The descriptor this node is a second name for, if it is one.
    ///
    /// `/proc/self/fd/3` and the `/dev/stdin`, `/dev/stdout` and `/dev/stderr`
    /// links that point into it are not files: opening one opens the
    /// descriptor it names, sharing its file and its position, which is the
    /// whole point - a `> /dev/stdout` has to write to wherever standard
    /// output goes, and a program listing `/dev/fd` is looking at what it has
    /// open. Resolution hands back the link's own inode, so this is how the
    /// opener is told what the name really meant.
    fn as_fd(&self) -> Option<usize> {
        None
    }

    /// The epoll instance this node is, if it is one.
    ///
    /// `epoll_ctl` and `epoll_wait` arrive holding nothing but a descriptor
    /// number, and the only way from a number to the interest list it names
    /// is through the inode it opens - which is the same shape as
    /// [`Self::as_tty`], for the same reason: the syscall layer must be able
    /// to ask the node what it really is, without knowing which filesystem
    /// or registry minted it.
    fn as_epoll(&self) -> Option<&crate::vfs::epoll::Epoll> {
        None
    }

    /// The timer this node is, if it is one.
    ///
    /// `timerfd_settime` and `timerfd_gettime` are the timerfd(2) pair the
    /// same way `epoll_ctl` is the epoll one: a descriptor number is all they
    /// hold, and the timer behind the number is reachable only through the
    /// inode. See [`Self::as_epoll`].
    fn as_timerfd(&self) -> Option<&crate::vfs::timerfd::TimerFd> {
        None
    }

    /// The inotify descriptor this node is, if it is one.
    ///
    /// `inotify_add_watch` and `inotify_rm_watch` hold nothing but a
    /// descriptor number, and the queue behind it is reachable only here -
    /// the same shape as [`Self::as_timerfd`].
    fn as_inotify(&self) -> Option<&crate::vfs::inotify::InotifyInstance> {
        None
    }

    /// The block device this node stands for, if it is one.
    ///
    /// `mount(2)` is told "mount this device on that directory" and receives
    /// the device as a *path*. Only the node can turn that into storage, which
    /// is what hands the mount decision - which filesystems exist, and what is
    /// mounted where - to userland rather than keeping it in the kernel's boot
    /// code.
    fn as_block(&self) -> Option<alloc::sync::Arc<dyn crate::blk::BlockDev>> {
        None
    }

    /// The network-stack handle this node carries, if it is a socket.
    ///
    /// Sockets have no filesystem storage behind them - they are anonymous
    /// channels that happen to use descriptors - so they do not fit any of
    /// the methods above. What they need is a way for the syscall layer,
    /// holding nothing but an `Arc<dyn Inode>`, to ask: is this a socket, and
    /// if so, which one? Zero is "not a socket"; the socket registry in
    /// `net` numbers its entries from one.
    fn socket_handle(&self) -> usize {
        0
    }

    /// The physical frame `mmap` should map for page `index` of this node,
    /// if the node owns memory it can share rather than copy.
    ///
    /// The kernel's file mappings are read-in copies - right for every file
    /// a program merely reads, and wrong for the one thing that must not be
    /// a copy: a framebuffer, where a program drawing into its mapping is
    /// drawing *on the screen*. A node that owns shareable pages answers
    /// with their physical address and the mapping becomes those very
    /// frames; the rest answer `None` and get the copy the contract always
    /// promised. The node's own protection against those frames being
    /// recycled - the pin, for the GPU's DMA - is the node's business,
    /// which is why this is a method and not a type.
    fn mmap_frame(&self, _index: u64) -> Option<usize> {
        None
    }

    /// Whether [`Self::mmap_frame`] is this node's real answer rather than the
    /// default "no".
    ///
    /// `mmap` asks *before* it maps a single page, and that is what makes such
    /// a mapping lazy: a range of a node that answers here is recorded as a
    /// frame-backed VMA and populated a page at a time from [`Self::mmap_frame`]
    /// as the program touches it - the same aliasing the eager loop used to
    /// give, with the pages arriving on the fault that needs them rather than
    /// all of them up front. A node that does not override this gets the
    /// read-in copy, which is what every ordinary file wants.
    fn frame_backed(&self) -> bool {
        false
    }

    /// The `memfd` this node is, if it is one - how `fcntl`'s sealing
    /// (`F_ADD_SEALS`/`F_GET_SEALS`) reaches the seal state of an anonymous
    /// shared buffer. Gated with the `memfd` module, which the host has no
    /// page allocator to build.
    #[cfg(target_os = "none")]
    fn as_memfd(&self) -> Option<&crate::vfs::memfd::Memfd> {
        None
    }

    /// The dma-buf this node is, if it is one - how the DRM card's PRIME
    /// import recognizes a descriptor that names shareable pages (and hands
    /// the geometry and the pages back to build a GEM handle from), and how
    /// the dma-buf ioctls reach a buffer without knowing who exported it.
    /// Same shape as [`Self::as_memfd`], for the same reason. Gated with
    /// the `dmabuf` module.
    #[cfg(target_os = "none")]
    fn as_dmabuf(&self) -> Option<&crate::vfs::dmabuf::DmaBuf> {
        None
    }

    /// The character stream this node wraps, if it is one - how a device
    /// family's ioctls reach the device without the dispatcher knowing which
    /// device implements them: the `'P'`-typed OSS requests on `/dev/dsp`
    /// ask the *stream* for its rate and format, and the stream is the one
    /// that knows.
    fn as_chardev(&self) -> Option<&dyn devfs::CharDevice> {
        None
    }

    /// The AF_UNIX socket this node is, if it is one - the same question
    /// [`Self::socket_handle`] puts to the network registry, answered with
    /// this module's own slot number. The unix syscalls hold nothing but a
    /// descriptor; the listener's queue and the bind record live behind this
    /// id in `net::unix`, and zero means "not a unix socket".
    fn unix_id(&self) -> usize {
        0
    }

    /// The AF_VSOCK socket this node is, if it is one - the id the close
    /// paths retire the socket by, numbered exactly like [`Self::unix_id`]:
    /// zero means "not a vsock socket", and the family's registry holds the
    /// rest.
    fn vsock_id(&self) -> usize {
        0
    }

    /// The connected stream channel behind this node, if any.
    ///
    /// A socketpair end *is* a [`DuplexEnd`](crate::vfs::pipe::DuplexEnd);
    /// a connected AF_UNIX socket *has* one, and forwards its whole file
    /// face through this handle. The message syscalls route by it, which is
    /// how both spellings of a connected unix stream read, write, poll and
    /// half-close through exactly the same pipe machinery.
    fn unix_end(&self) -> Option<alloc::sync::Arc<crate::vfs::pipe::DuplexEnd>> {
        None
    }

    /// What this node can do without blocking, for `poll(2)`.
    ///
    /// The default is "everything": a node that never blocks - a file, a
    /// directory, a device that always has an answer - is always ready. Only
    /// the nodes that can make a caller wait say otherwise.
    fn poll_ready(&self) -> Ready {
        Ready::ALL
    }

    /// Whether the node's peer has gone for good: `POLLHUP`, `EPOLLHUP`.
    ///
    /// A different question from [`Self::poll_ready`], which answers what a
    /// call would *do*, and the two must not be folded together: Linux raises
    /// the hang-up and error conditions whether or not the caller asked for
    /// them, and a waiter that only ever hears "readable" cannot tell a slow
    /// writer from one that has finished. udev's `spawn_wait` is exactly such
    /// a waiter - read off its own binary, it watches its child's pipe with
    /// epoll and ends the wait on `EPOLLHUP` alone, *never* on a read that
    /// answered 0 - so a kernel that never raises the bit leaves the device
    /// manager parked on every `IMPORT{program}` rule until its own event
    /// timeout expires. That was the difference between a keyboard tagged at
    /// boot and the same keyboard tagged two minutes later.
    ///
    /// Default false: a node with no peer to lose - a file, an eventfd - has
    /// nothing to hang up.
    fn poll_hup(&self) -> bool {
        false
    }

    /// Whether the node's peer is gone in the other direction: `POLLERR`,
    /// `EPOLLERR`. Same contract as [`Self::poll_hup`], and the same reason for
    /// being asked separately.
    fn poll_err(&self) -> bool {
        false
    }

    /// Ask to be woken when this node may have become ready, and stop asking.
    ///
    /// `poll` calls this before it sleeps and the matching `unsubscribe` after,
    /// on every node it was asked about. The default does nothing, which is
    /// right for a node that is always ready: there is no event to miss.
    fn poll_subscribe(&self, id: crate::sched::policy::TaskId) {
        let _ = id;
    }

    fn poll_unsubscribe(&self, id: crate::sched::policy::TaskId) {
        let _ = id;
    }

    /// [`Self::read_at`] on behalf of a named client.
    ///
    /// Almost every node answers the same thing whoever is asking, and the
    /// default says so. A node whose answer *depends on the client* overrides
    /// this: an evdev node is the one that does today, because the events in it
    /// belong to whoever holds `EVIOCGRAB` - so a read there has to know who is
    /// asking before it can even decide whether to wait.
    ///
    /// The identity is the open file description's own address
    /// ([`crate::vfs::File::ofd`]), which is the key the same arbitrations
    /// already use: one description is one client, whatever names it.
    fn read_at_client(&self, offset: u64, buf: &mut [u8], client: u64) -> VfsResult<usize> {
        let _ = client;
        self.read_at(offset, buf)
    }

    /// [`Self::write_at`] on behalf of a named client, with the same reason and
    /// the same identity as [`Self::read_at_client`].
    ///
    /// A write needs it for the mirror of the read's problem: an evdev node's
    /// records belong to a grab, and a `/dev/uinput` description *is* a device -
    /// what gets written has to land in the device belonging to the description
    /// that wrote it, which an offset cannot say.
    fn write_at_client(&self, offset: u64, buf: &[u8], client: u64) -> VfsResult<usize> {
        let _ = client;
        self.write_at(offset, buf)
    }

    /// [`Self::poll_ready`] for a named client. A node whose readiness differs
    /// between its clients overrides this; the default is the clientless
    /// answer, which is right for every node that has only one readiness to
    /// report.
    fn poll_ready_client(&self, client: u64) -> Ready {
        let _ = client;
        self.poll_ready()
    }

    /// [`Self::poll_subscribe`] for a named client: the wait is registered as
    /// *this* client's, so a wake can be held back from a client that has
    /// nothing coming - which is what keeps a non-owner of a grabbed evdev
    /// device asleep instead of waking on every one of the owner's events.
    fn poll_subscribe_client(&self, client: u64, id: crate::sched::policy::TaskId) {
        let _ = client;
        self.poll_subscribe(id);
    }

    /// Where a symbolic link points. Anything that is not a link answers
    /// [`VfsError::Invalid`], which is what turns into `EINVAL` from
    /// `readlink(2)` - the error Linux gives for reading a non-link.
    ///
    /// The target is a path in the filesystem's own terms, not a resolved one:
    /// `bin/ls` may point at `/bin/busybox` or at `busybox`, and which of the
    /// two it is decides how it resolves. Only the filesystem knows.
    fn readlink(&self) -> VfsResult<String> {
        Err(VfsError::Invalid)
    }

    /// The device number this node carries, as `st_rdev` reports it. Only
    /// character and block devices answer - `ttyname` in libc identifies its
    /// descriptor by walking `/dev` and matching this number, and a node
    /// without one matches nothing.
    fn rdev(&self) -> Option<(u32, u32)> {
        None
    }

    /// The node's last write, in unix seconds - what `st_mtime` reports and
    /// what a freshness check decides by. `apk` judges a cached repository
    /// index this way, and an epoch of zero reads as 1970: infinitely stale,
    /// which turned every `apk add` into a re-fetch.
    ///
    /// The default is zero. A virtual filesystem whose nodes are not
    /// persisted has no timestamp to report - `stat`-ing a pipe was always
    /// epoch anything - and a persisted one that does keep times says so by
    /// overriding this.
    fn mtime(&self) -> u64 {
        0
    }

    /// The node's last read, in unix seconds - `st_atime`. The default reports
    /// the mtime, which is the honest answer for a filesystem that keeps one
    /// clock, and matches what a `stat` used to collapse all three to.
    fn atime(&self) -> u64 {
        self.mtime()
    }

    /// The node's last metadata change, in unix seconds - `st_ctime`. The
    /// default again reports the mtime; a persisted filesystem that keeps the
    /// word on disk (ext2's `i_ctime`) overrides it.
    fn ctime(&self) -> u64 {
        self.mtime()
    }

    /// How many directory names reach this node - `st_nlink`. A hard link is
    /// the second name for one inode, and the count is what tells a caller a
    /// file has more than one. The default `0` means "this filesystem does not
    /// track the count", and the `stat` writer then falls back to the kind's
    /// convention (a directory's `.` and `..`). A filesystem that keeps
    /// `i_links_count` overrides this with the real number.
    fn nlink(&self) -> u64 {
        0
    }

    /// Record how many names reach this node - the writing half of
    /// [`Self::nlink`], and the bookkeeping [`Self::link`] owes.
    ///
    /// The two halves are separate calls because they live on separate nodes:
    /// the *directory* makes the second edge and is the only node that can see
    /// both ends, while the number belongs to the *target*. A filesystem that
    /// stores the count on disk - ext4's `i_links_count` - moves it inside its
    /// own `link` and `unlink` and has nothing to add here; this is for the
    /// in-memory filesystems, whose nodes hold the number themselves.
    ///
    /// The default does nothing, like [`Self::set_mode`] on a filesystem that
    /// keeps no modes: a node that does not track a count cannot be told one.
    fn set_nlink(&self, links: u64) {
        let _ = links;
    }

    /// The permission bits (the low twelve) the filesystem stores for this
    /// node, or `0` when it carries none - the caller then falls back to what
    /// the node's kind implies. The disk filesystems carry the real thing,
    /// and OpenRC runs on them
    /// on it: it refuses to start any service script a `test -x` rejects, and
    /// a kernel that hands out one fixed mode per kind leaves every script
    /// 0644 and every runlevel silent.
    fn mode(&self) -> u32 {
        0
    }

    /// Whether this filesystem keeps permission bits at all.
    ///
    /// [`mode`](Inode::mode) answers zero both for a node nobody set a mode on
    /// and for one deliberately set to `0000`, and a permission check cannot
    /// tell those apart from the number alone. So the filesystem is asked
    /// directly: one that keeps modes is policed exactly, and one that keeps
    /// none refuses nothing rather than inventing permissions for nodes nobody
    /// set any on.
    fn keeps_modes(&self) -> bool {
        false
    }

    /// Give the node the permission bits an archive or a `chmod` named.
    ///
    /// Not `chmod(2)` - that is a syscall with a caller to answer - but the
    /// way a filesystem that stores modes learns them from something that
    /// already knew: the initramfs unpack, whose members carry a mode in
    /// their ustar header and whose `/bin` is useless when they are dropped.
    /// The default keeps nothing, which is honest for a filesystem with
    /// nowhere to put it; a caller that only *offers* the bits has to be
    /// willing to hear no.
    fn set_mode(&self, _mode: u32) {}

    /// The user and group that own this node, as `stat` reports them.
    ///
    /// `(0, 0)` - root - is the answer for a filesystem that keeps no owner:
    /// a ramfs the kernel built for itself, or a node from before there was a
    /// second user. The disk filesystems carry the real thing.
    fn owner(&self) -> (u32, u32) {
        (0, 0)
    }

    /// Store the owner. The default keeps nothing, like [`Inode::set_mode`]:
    /// the node then answers `stat` as root's, which is what every node was
    /// before this existed.
    fn set_owner(&self, _uid: u32, _gid: u32) {}

    /// The node's attribute flags: Linux's `FS_*_FL` vocabulary, the word
    /// `FS_IOC_GETFLAGS` returns and `lsattr` prints.
    ///
    /// The default refuses, and the refusal is an answer rather than a gap:
    /// Linux answers `ENOTTY` for a filesystem whose inode operations have no
    /// `fileattr` half, which is every filesystem here that stores no such
    /// flags - procfs, sysfs, devfs, a pipe, a socket. A caller that hears the
    /// refusal falls back to its own answer; a caller told "no flags" would be
    /// told something false about a file it asked a real question about.
    fn flags(&self) -> VfsResult<u32> {
        Err(VfsError::Unsupported)
    }

    /// Replace the node's attribute flags, on the same terms as
    /// [`Inode::flags`] and with the same default refusal.
    ///
    /// An implementation refuses what it cannot *keep*: a flag stored and not
    /// enforced is worse than a flag refused, because the caller that set it
    /// stops looking. It also ignores nothing - a change to a flag the
    /// filesystem owns is a refusal, not a silent no-op.
    fn set_flags(&self, _flags: u32) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }

    /// Set the node's access and modification clocks - what `utimensat`/`futimens`
    /// do when a caller (or `touch`) names times. The change clock moves with
    /// the metadata, so a filesystem that keeps the three stores a fresh ctime
    /// itself and takes only these two. The default refuses, for a filesystem
    /// with nowhere to keep a time.
    fn set_times(&self, _atime: u64, _mtime: u64) -> VfsResult<()> {
        Err(VfsError::Unsupported)
    }

    /// Name a new symbolic link in this directory. Separate from [`Inode::create`]
    /// because a link is made of a target, not just a kind - there is nothing
    /// useful to create without one.
    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        let _ = (name, target);
        Err(VfsError::Unsupported)
    }

    /// Hard-link `target` into this directory under `name`: a second name for
    /// the very same inode, the inode's link count going up by one. Linux's
    /// apk commits every extracted library this way - the versioned file is
    /// written once and every ABI name is linked to it - so a filesystem
    /// without this installs half of every package.
    fn link(&self, name: &str, _target: &Arc<dyn Inode>) -> VfsResult<()> {
        let _ = name;
        Err(VfsError::Unsupported)
    }

    /// The names in this directory, each with the kind of node it names. The
    /// order is the filesystem's own; "." and ".." are included where the
    /// filesystem keeps them, as POSIX readdir reports them. A non-directory
    /// has no names, and the default answers `NotDir` for one.
    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        let _ = self;
        Err(VfsError::NotDir)
    }
}

/// An inode as `mm`'s *mapped object*: what a frame-backed mapping aliases its
/// pages from.
///
/// `mm` owns the address space, and a mapping of a `memfd`, a dma-buf, the
/// framebuffer or a DRM dumb buffer is a memory fact it has to keep - but the
/// thing behind those pages is a VFS inode, and an address space is not a file.
/// Before this wrapper existed, `mm` named [`Inode`] directly (`ARCHITECTURE.md`
/// V-13), which is what made `mm` and `vfs` a cycle; now it names
/// [`crate::mm::vma::MappedObject`] - three questions, no files - and this is where
/// the three answers come from, since the outer layer is the one allowed to know
/// both sides of the seam.
///
/// The comparison is the interesting one. `mm` merges two adjacent mappings of
/// the same object into the one line Linux prints, and it used to decide that
/// with `Arc::ptr_eq` on the two inode pointers. A trait object cannot be
/// compared by pointer across a crate-ish boundary without a downcast, so the
/// trait asks the object instead, and the default answer - equal
/// [`identity`](crate::mm::vma::MappedObject::identity) - is the same fact as a
/// value. For these four node types it is also unique per object: a `memfd` and
/// a dma-buf carry their own inode number, and the framebuffer and each dumb
/// buffer are devfs nodes that carry one too.
pub struct MappedInode(Arc<dyn Inode>);

impl MappedInode {
    /// Wrap an inode for a frame-backed mapping. The mapping holds the inner
    /// inode alive for as long as the VMA lives, which is what keeps a `memfd`'s
    /// frames from being released while somebody is still drawing into them.
    pub fn new(inode: Arc<dyn Inode>) -> Arc<dyn crate::mm::vma::MappedObject> {
        Arc::new(MappedInode(inode))
    }
}

impl crate::mm::vma::MappedObject for MappedInode {
    fn frame_at(&self, index: u64) -> Option<usize> {
        self.0.mmap_frame(index)
    }

    fn identity(&self) -> ((u32, u32), u64) {
        (self.0.rdev().unwrap_or((0, 0)), self.0.metadata().ino)
    }
}

/// The block/inode accounting `statfs(2)` reports, as the filesystem itself
/// counts it: `blocks`/`bfree`/`bavail` in `bsize`-sized blocks, `files`/
/// `ffree` in inodes.
#[derive(Clone, Copy)]
pub struct FsStats {
    pub bsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
}

/// A mounted filesystem instance: what other systems would call a superblock.
///
/// The instance owns the per-mount state (ramfs: its node graph; the disk
/// filesystems: the block device handle and on-disk counters) and hands
/// out its root.
pub trait FileSystem: Send + Sync {
    fn root(&self) -> Arc<dyn Inode>;

    /// The space accounting `statfs(2)` reports, in the filesystem's own
    /// block units. `None` - the default - says "no real numbers": the
    /// syscall layer then answers what it can for the type, and a `df` that
    /// reads zero total blocks filters the mount out, as Linux's own does.
    fn statfs(&self) -> Option<FsStats> {
        None
    }

    /// The name `mount(2)` spells this filesystem by, and the one /proc/mounts
    /// reports. A filesystem that cannot be named cannot be mounted from
    /// userland, which is the whole point of the call.
    fn name(&self) -> &'static str;

    /// Whether this filesystem was mounted read-only, and which is asked to
    /// become so or stop being so. `mount -o ro` is not a comment on how the
    /// caller intends to behave: it is a promise the filesystem keeps by
    /// refusing writes, and an initramfs relies on it while it checks the
    /// root it has not yet trusted.
    ///
    /// The default is "writable, and nothing to do": a filesystem with no
    /// writes to refuse - procfs, devfs, an empty sysfs - has nothing to say.
    fn read_only(&self) -> bool {
        false
    }

    fn set_read_only(&self, _read_only: bool) {}
}

/// List the directory named by `path`, in one call. The convenience the
/// syscall layer wants: it has a path, not an inode.
pub fn read_dir(path: &str) -> VfsResult<Vec<(String, NodeKind)>> {
    resolve(path)?.read_dir()
}

/// Read a whole file into memory, sized by its metadata. The loader's way in:
/// spawn hands the bytes to the ELF parser, which wants one contiguous slice.
/// A directory names no bytes, so it is refused.
pub fn read_file(path: &str) -> VfsResult<Vec<u8>> {
    let inode = resolve(path)?;
    if inode.metadata().kind != NodeKind::File {
        return Err(VfsError::IsDir);
    }
    let file = File::new(inode);
    let size = file.metadata().size as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| VfsError::NoSpace)?;
    let mut chunk = [0u8; 4096];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// Split `path` into its parent directory and the name at the end - the two
/// halves of every make-or-remove, since the name edge lives in the parent.
///
/// A bare name has no slash in it, and that does not make it unnameable: it
/// sits beside whatever directory the caller is in, which `.` spells exactly.
/// Resolving `.` against the caller's cwd is what turns `mkdir a` into `mkdir
/// /where/the/shell/is/a`, and getting this wrong is why it used to answer
/// `EINVAL`. The caller resolves the parent against its cwd, so this returns
/// `.` rather than reaching for the root.
///
/// The last name is still refused if it is `.` or `..`: those name a directory
/// that already exists, not an entry to make or remove.
pub(crate) fn split_parent(path: &str) -> VfsResult<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(slash) => (&trimmed[..slash], &trimmed[slash + 1..]),
        None => (".", trimmed),
    };
    // A path that is *empty* names no file, which is a missing file rather than
    // a malformed request: Linux answers ENOENT for it, the same answer any
    // other unresolvable path gets. The cases below are the other kind - `.`,
    // `..` and a bare `/` all name directories that exist, so refusing to make
    // or remove one is about the operation, not about the name.
    if path.is_empty() {
        return Err(VfsError::NoEntry);
    }
    if name.is_empty() || name == "." || name == ".." {
        return Err(VfsError::Invalid);
    }
    Ok((parent, name))
}

/// Make an empty file at `path` and hand back the inode. `Exists` when the
/// name is taken.
pub fn create_file(path: &str) -> VfsResult<Arc<dyn Inode>> {
    let (parent, name) = split_parent(path)?;
    resolve(parent)?.create(name, NodeKind::File)
}

/// Remove the name `path`. `NoEntry` when nothing answers to it.
pub fn unlink_path(path: &str) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    resolve(parent)?.unlink(name)
}

/// The same walks with a caller-supplied start for relative paths. `None`
/// is the root, which is what every kernel caller means; a process passes
/// its cwd, and absolute paths ignore it, as `/` must.
pub fn resolve_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<Arc<dyn Inode>> {
    match start {
        Some(dir) => resolve_at(path, dir),
        None => resolve(path),
    }
}

/// [`read_dir`](fn.read_dir.html) with a caller-supplied start.
pub fn read_dir_opt(
    path: &str,
    start: Option<Arc<dyn Inode>>,
) -> VfsResult<Vec<(String, NodeKind)>> {
    resolve_opt(path, start)?.read_dir()
}

/// [`read_file`](fn.read_file.html) with a caller-supplied start.
///
/// The capacity is reserved with the fallible form: a file bigger than the
/// kernel's largest contiguous allocation — a 40 MB compiler, say — comes
/// back as `NoSpace` rather than dying in the allocator, and the execve
/// that reached for it answers ENOMEM instead of taking the machine down.
pub fn read_file_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<Vec<u8>> {
    let inode = resolve_opt(path, start)?;
    if inode.metadata().kind != NodeKind::File {
        return Err(VfsError::IsDir);
    }
    let file = File::new(inode);
    let size = file.metadata().size as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| VfsError::NoSpace)?;
    let mut chunk = [0u8; 4096];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// [`create_file`](fn.create_file.html) with a caller-supplied start.
pub fn create_file_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<Arc<dyn Inode>> {
    let (parent, name) = split_parent(path)?;
    resolve_opt(parent, start)?.create(name, NodeKind::File)
}

/// The bit set `open(2)` and `access(2)` share: read, write, execute - the
/// numbers `access(2)` takes as its mode, and the ones each permission triad
/// holds in its low three bits.
pub const R_OK: u32 = 4;
pub const W_OK: u32 = 2;
pub const X_OK: u32 = 1;
/// The sticky bit: a directory whose entries only its own owner (and each
/// entry's owner) may remove.
const S_ISVTX: u32 = 0o1000;
/// The set-user-id and set-group-id bits. `chmod` writes them, `execve` reads
/// them, and the rules about who may keep them belong to both: a caller who
/// is not in a file's group cannot grant the group's authority to it.
pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;

// ---------------------------------------------------------------------------
// The inode attribute flags: `lsattr`'s columns, and `FS_IOC_GETFLAGS`' word.
// ---------------------------------------------------------------------------

// Linux's names and Linux's values (`<linux/fs.h>`), because the number is the
// UAPI: a caller passes these bits through `FS_IOC_SETFLAGS` and reads them back
// through `FS_IOC_GETFLAGS`, and `lsattr` prints them by *position*. A private
// value would be a private dialect of a vocabulary userland already knows.
//
// They are grouped by what this kernel does with them, because that is what a
// reader needs from this list: the first six are the ones a caller may change
// and this kernel *honours*; the next three are the block layout the ext4 driver
// owns and only reports; the rest are named so that a caller passing one is
// refused for the right reason - `EOPNOTSUPP`, "this filesystem cannot do that"
// - rather than by falling off the end of a mask.

/// Append-only: the file may only be written at its end, and may not be
/// removed, renamed or truncated. Honoured by [`may_truncate`] and by
/// [`File::write`](crate::vfs::file::File).
pub const FS_APPEND_FL: u32 = 0x0000_0020;
/// Do not include this file in a `dump(8)` archive. Nothing in the kernel acts
/// on it, and nothing should: the flag's whole meaning is a promise *to
/// userland*, which reads it with `GETFLAGS` and skips the file. Honouring it
/// is storing it.
pub const FS_NODUMP_FL: u32 = 0x0000_0040;
/// Do not update the file's access time. Read where the access time would
/// otherwise move.
pub const FS_NOATIME_FL: u32 = 0x0000_0080;
/// Writes to this file are synchronous. Honoured by construction: this kernel's
/// filesystems are write-through - every `write_at` reaches the device before it
/// returns - so the promise this flag makes is one that is already kept.
pub const FS_SYNC_FL: u32 = 0x0000_0008;
/// Directory operations in this directory are synchronous, on the same terms as
/// [`FS_SYNC_FL`].
pub const FS_DIRSYNC_FL: u32 = 0x0001_0000;
/// Immutable: the file may not be written, truncated, removed, renamed or
/// linked, and its metadata may not be changed. Set and cleared only by a
/// caller holding `CAP_LINUX_IMMUTABLE`.
pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;

/// The block pointers of this inode are an extent tree rather than a block map.
/// Owned by the ext4 driver, which sets it on every file it creates: a caller
/// may read it (it is what `lsattr` prints as `e`) and may not change it.
pub const FS_EXTENT_FL: u32 = 0x0008_0000;
/// This directory is indexed (has a hash tree). Owned by the driver for the same
/// reason, and reported in the same way.
pub const FS_INDEX_FL: u32 = 0x0000_1000;
/// `i_blocks` counts filesystem blocks rather than 512-byte sectors. Owned by
/// the driver, which sets it when a file outgrows what sector counting can say.
pub const FS_HUGE_FILE_FL: u32 = 0x0004_0000;

/// Secure deletion: the data is overwritten when the file is removed. Named so
/// that a caller asking for it is refused; nothing here overwrites anything.
pub const FS_SECRM_FL: u32 = 0x0000_0001;
/// Undelete: the data is kept after removal. Refused - nothing here keeps it.
pub const FS_UNRM_FL: u32 = 0x0000_0002;
/// Transparent compression, on the HFS+/btrfs model. Refused.
pub const FS_COMPR_FL: u32 = 0x0000_0004;
/// Written with the dirty bit set, for a filesystem that tracks a commit point.
/// Refused: this kernel's write-through filesystems have none.
pub const FS_DIRTY_FL: u32 = 0x0000_0100;
/// The head of a compressed cluster. Refused with the rest of compression.
pub const FS_COMPRBLK_FL: u32 = 0x0000_0200;
/// Do not compress this file. Refused with the rest of compression.
pub const FS_NOCOMP_FL: u32 = 0x0000_0400;
/// Encrypted inode. Refused: there is no keyring to encrypt against.
pub const FS_ENCRYPT_FL: u32 = 0x0000_0800;
/// Magic naming for the AFS/Solaris directory layout. Refused.
pub const FS_IMAGIC_FL: u32 = 0x0000_2000;
/// Journal the file's data rather than writing it through. Refused: there is no
/// journal.
pub const FS_JOURNAL_DATA_FL: u32 = 0x0000_4000;
/// Partially-written blocks are not merged into a neighbouring file's tail.
/// Refused: this kernel's allocator has no tail-merging to suppress.
pub const FS_NOTAIL_FL: u32 = 0x0000_8000;
/// The directory is the top of its own hierarchy. Refused: it is an allocation
/// hint for a directory tree this kernel does not build.
pub const FS_TOPDIR_FL: u32 = 0x0002_0000;
/// The file is protected by fs-verity. Refused: there is no Merkle tree here.
pub const FS_VERITY_FL: u32 = 0x0010_0000;
/// The inode's extended attributes are shared between inodes. Refused: there is
/// no extended-attribute storage.
pub const FS_EA_INODE_FL: u32 = 0x0020_0000;
/// The file's last block is known to be sparse. Refused: the driver tracks
/// holes its own way.
pub const FS_EOFBLOCKS_FL: u32 = 0x0040_0000;
/// Copy-on-write is off for this file. Refused: no btrfs-style CoW to switch.
pub const FS_NOCOW_FL: u32 = 0x0080_0000;
/// Direct access to the storage, bypassing the page cache. Refused: there is no
/// page cache to bypass, and no DAX device to map.
pub const FS_DAX_FL: u32 = 0x0200_0000;
/// The file's data is stored in its inode. Refused: no inode in this kernel has
/// room, and the driver would have to move the data to honour it.
pub const FS_INLINE_DATA_FL: u32 = 0x1000_0000;
/// New children inherit this directory's project id. Refused: there are no
/// project ids.
pub const FS_PROJINHERIT_FL: u32 = 0x2000_0000;
/// Names in this directory are compared case-insensitively. Refused: the
/// directory hashing here is byte-exact, and folding it afterwards would need a
/// second name index.
pub const FS_CASEFOLD_FL: u32 = 0x4000_0000;

/// Every flag an inode here can *report*.
///
/// `GETFLAGS` on a filesystem that stores flags answers with these and nothing
/// else, and `SETFLAGS` refuses any bit outside the set outright: a flag this
/// kernel does not know is one it cannot keep, and answering "yes" to it would
/// leave the caller believing something false.
pub const INODE_FL_VISIBLE: u32 = FS_IMMUTABLE_FL
    | FS_APPEND_FL
    | FS_NODUMP_FL
    | FS_NOATIME_FL
    | FS_SYNC_FL
    | FS_DIRSYNC_FL
    | FS_EXTENT_FL
    | FS_INDEX_FL
    | FS_HUGE_FILE_FL;

/// The subset of [`INODE_FL_VISIBLE`] a caller may change, and the whole of what
/// this kernel *enforces*.
///
/// The three the driver owns are visible and not modifiable, which is ext4's own
/// split (`EXT4_FL_USER_VISIBLE` against `EXT4_FL_USER_MODIFIABLE`) and for the
/// same reason: clearing `FS_EXTENT_FL` on a file whose blocks are an extent tree
/// does not turn the tree back into a block map, it makes the file unreadable.
/// A caller that changes one is refused rather than quietly obeyed, because a
/// silently-ignored `chattr` is a caller that stops checking.
pub const INODE_FL_MODIFIABLE: u32 =
    FS_IMMUTABLE_FL | FS_APPEND_FL | FS_NODUMP_FL | FS_NOATIME_FL | FS_SYNC_FL | FS_DIRSYNC_FL;

/// The inode's attribute flags, or zero for a filesystem that has none.
///
/// A filesystem with no `fileattr` half - procfs, sysfs, a pipe - refuses
/// `GETFLAGS`, which is the honest answer, and it also has no flags for the
/// checks below to police. This is the one place that difference is spelled.
pub fn flags_of(inode: &Arc<dyn Inode>) -> u32 {
    inode.flags().unwrap_or(0)
}

/// Whether `inode` is immutable.
pub fn is_immutable(inode: &Arc<dyn Inode>) -> bool {
    flags_of(inode) & FS_IMMUTABLE_FL != 0
}

/// Whether `inode` may only be written at its end.
pub fn is_append_only(inode: &Arc<dyn Inode>) -> bool {
    flags_of(inode) & FS_APPEND_FL != 0
}

/// Whether the running process may change `inode`'s contents or metadata at all.
///
/// One check for both, because Linux's `IMMUTABLE` covers both and a single
/// helper is how they stay in step: `chmod` on an immutable file is refused for
/// the same reason a write to it is, and a kernel that policed only the write
/// would let the file's *meaning* be changed out from under the flag that
/// protects it. There is no capability carve-out, either - `CAP_LINUX_IMMUTABLE`
/// is what *clears the flag*, and a process that holds it has the way out
/// anybody else would need to be given. `chattr(1)`'s own words are "most of the
/// metadata cannot be modified", and the way to unprotect a file is to unprotect
/// it.
pub fn may_modify(inode: &Arc<dyn Inode>) -> VfsResult<()> {
    if is_immutable(inode) {
        return Err(VfsError::NotPermitted);
    }
    Ok(())
}

/// Whether the running process may change `inode`'s *extent*: cut it short or
/// lengthen it, remove it, or give it another name.
///
/// A stricter question than [`may_modify`], and the difference between the two
/// flags that this kernel honours. An append-only file may be added to and not
/// cut back, may not be removed and may not be hard-linked a second time - so
/// both flags refuse all four - while only `IMMUTABLE` reaches the metadata.
/// That is `chattr(1)`'s own split: it says of `a` that the file "can only be
/// opened in append mode for writing", and of `i` that "most of the metadata
/// cannot be modified". Linux spells the same two rules as `may_delete`,
/// `may_linkat` and the truncating half of `setattr_prepare`; this is all of
/// them because all of them are one question.
pub fn may_alter(inode: &Arc<dyn Inode>) -> VfsResult<()> {
    if is_immutable(inode) || is_append_only(inode) {
        return Err(VfsError::NotPermitted);
    }
    Ok(())
}

/// What a filesystem should store when a caller asks to move an inode's flags
/// from `old` to `new`, or the refusal that stands in its place.
///
/// One rule for every filesystem that keeps flags, because it is Linux's and not
/// any driver's: a bit outside what this kernel can hold is `EOPNOTSUPP` - "this
/// filesystem cannot do that", the answer `chattr` prints and the one that sends
/// the caller to its fallback - and a *change* to a bit the filesystem owns
/// (`owned`: the extent tree, the directory index, the block-counting unit) is
/// refused rather than quietly ignored, because a caller whose `chattr` appeared
/// to work and did nothing stops checking.
///
/// The returned word carries the owned bits over as they stand, so a filesystem
/// cannot lose its layout by trusting the caller's argument.
pub fn apply_flag_change(old: u32, new: u32, owned: u32) -> VfsResult<u32> {
    if new & !(INODE_FL_MODIFIABLE | owned) != 0 {
        return Err(VfsError::Unsupported);
    }
    if (old ^ new) & owned != 0 {
        return Err(VfsError::Unsupported);
    }
    Ok((old & !INODE_FL_MODIFIABLE) | (new & INODE_FL_MODIFIABLE))
}

/// Whether `mode`, owned by `owner`, lets `me` do `want` - where `want` is the
/// same bit set `open(2)` uses: 4 read, 2 write, 1 execute.
///
/// The triad comparison alone: whether the filesystem keeps a mode at all is
/// [`Inode::keeps_modes`], which the callers below ask first. A `mode` of zero
/// here means every triad is empty and nothing is permitted, which is what
/// `chmod 000` means. `groups` is the caller's supplementary group list, so the
/// group triad applies when the node's gid is any of them - not just the
/// effective gid - the way Linux matches membership.
fn permitted(mode: u32, owner: (u32, u32), want: u32, me: (u32, u32), groups: &[u32]) -> bool {
    let bits = if me.0 == owner.0 {
        (mode >> 6) & 7
    } else if me.1 == owner.1 || groups.contains(&owner.1) {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    bits & want == want
}

/// The root override: uid 0 is checked by Linux's `CAP_DAC_OVERRIDE` shape
/// rather than passed outright.
///
/// Linux folds this into `capable(CAP_DAC_OVERRIDE)` in `fs/namei.c`: read or
/// write passes when *any* triad carries the bit, execute only when some
/// execute bit exists at all - so `access(X_OK)` on a `0644` file is `EACCES`
/// even for the superuser. mpos used to drop that exception, on the reasoning
/// that a single-user image whose root does the packaging has no use for a
/// check that can tell it "you may not run this". A distribution's tree
/// changed what the exception costs: Alpine ships real modes everywhere, and
/// the outright pass answers *success* where Linux answers `EACCES`, which is
/// the one thing a caller cannot fall back around. So the exception stands,
/// and its exact predicate lives in [`may_as`]'s root branch.
fn root_override() -> bool {
    true
}

/// The permission bits the checks may conclude from `inode`'s mode word.
///
/// A filesystem that keeps modes stores the low twelve bits and nothing
/// else - ext4 masks its on-disk word, devtmpfs' `NodePerms` holds a plain
/// number - so a zero there *is* `chmod 000`: every triad empty, nothing
/// permitted, root included, because Linux's `CAP_DAC_OVERRIDE` grants
/// read/write only where some triad carries the bit and execute only where
/// some execute bit exists. A filesystem that keeps no modes answers zero
/// for a different reason - it has no permissions for a check to conclude
/// anything about - and [`may_as`] passes those nodes untouched; refusing
/// them would invent permissions nobody set. The tell is `keeps_modes()`,
/// not the number: the same word means two things, and a check cannot tell
/// them apart by looking at it.
pub fn effective_mode(inode: &Arc<dyn Inode>) -> u32 {
    inode.mode()
}

/// Whether the process whose ids are `me` may do `want` to `node`.
///
/// The identity is a parameter rather than something read here, because the
/// `access` calls disagree about which pair to use: `access(2)` asks about the
/// **real** ids - "would *this user* be able to" - while `faccessat2` with
/// `AT_EACCESS` asks about the effective ones. Everything else asks with the
/// effective pair, which is what the wrappers below do.
pub fn may_as(me: (u32, u32), inode: &Arc<dyn Inode>, want: u32) -> VfsResult<()> {
    // A filesystem that keeps no modes is not policed: refusing would mean
    // inventing permissions for nodes nobody set any on.
    if !inode.keeps_modes() {
        return Ok(());
    }
    // Neither is root: uid 0 passes every check - Linux's
    // `capable(CAP_DAC_OVERRIDE)` in `fs/namei.c`, spelled below. Read/write
    // whenever any triad carries the bit, execute only when some execute bit
    // exists: a directory whose every bit of execute is missing stays denied
    // to root too, which is what the rule says. Asked for several kinds at
    // once (`may_create_in`'s write+search), it passes when *each* requested
    // kind has its somewhere - the same sentence one bit at a time.
    let allowed = if me.0 == 0 && root_override() {
        let mode = effective_mode(inode);
        if mode == 0 {
            true
        } else {
            let any = (mode >> 6) | (mode >> 3) | mode;
            (want & R_OK == 0 || any & 4 != 0)
                && (want & W_OK == 0 || any & 2 != 0)
                && (want & X_OK == 0 || any & 1 != 0)
        }
    } else {
        permitted(
            effective_mode(inode),
            inode.owner(),
            want,
            me,
            &current_groups(),
        )
    };
    if allowed {
        Ok(())
    } else {
        Err(VfsError::Access)
    }
}

/// The group list permission checks consult: the running task's three saved
/// gids, straight from `sched` - whose host build answers the empty list a
/// kernel caller that runs as no task would. Root's override makes it moot
/// either way.
fn current_groups() -> [u32; 3] {
    crate::sched::current_groups()
}

/// Whether the running process may do `want` to `node`, asked for its
/// effective ids - the check every syscall that touches a path makes.
pub fn may_access(inode: &Arc<dyn Inode>, want: u32) -> VfsResult<()> {
    may_as(crate::sched::current_owner(), inode, want)
}

/// Whether the running process may **search** `dir`: look a name up in it.
/// Execute on a directory is the permission to walk through it, and it is
/// what every path component is checked against.
pub fn may_search(dir: &Arc<dyn Inode>) -> VfsResult<()> {
    may_access(dir, X_OK)
}

/// Whether the running process may **create or remove a name** in `dir`: an
/// entry is a write to the directory, and finding where to put it is a search.
pub fn may_create_in(dir: &Arc<dyn Inode>) -> VfsResult<()> {
    // No flag check here, and that is Linux's shape rather than an omission:
    // `may_create` asks for `MAY_WRITE | MAY_EXEC` and nothing else, so a name
    // *may* be added to an append-only directory. What the flag forbids is
    // giving a name up, which is [`may_remove`]'s half - an append-only
    // directory is one that may only be added to, and both halves of that
    // sentence are load-bearing.
    may_access(dir, W_OK | X_OK)
}

/// Whether the running process may remove the name that `target` is, out of
/// the directory `parent`.
///
/// Two rules, and the second is the one that makes a shared directory
/// shareable. The caller needs **write permission on the directory**: a name
/// is an entry in it, and the file's own mode says nothing about whether its
/// entry may go - which is why `rm` on a read-only file inside a writable
/// directory succeeds, and why the check belongs here rather than on the file.
/// And when the directory is **sticky** (`/tmp`, `1777`), only the file's
/// owner, the directory's owner or root may remove it: without that half, any
/// user can delete any other user's work out of a directory everyone can
/// write, which is precisely what the bit exists to prevent.
///
/// The caller is the running process, asked for its effective ids: a filesystem
/// cannot know who is calling, and a permission check is a fact about the
/// caller, not about the node.
pub fn may_remove(parent: &Arc<dyn Inode>, target: &Arc<dyn Inode>) -> VfsResult<()> {
    // The inode's own flags first, and before the directory's: Linux's
    // `may_delete` refuses an immutable or append-only *target* whatever the
    // directory says, which is the whole of what the `i` someone set on a file
    // is supposed to mean. It is the same question [`may_alter`] answers, asked
    // here rather than at each of the callers - `unlink`, `rmdir`, and the
    // copy-and-unlink that `rename` is emulated with - so that all three refuse
    // together.
    may_alter(target)?;
    // Then the directory's own flag, and *before* the permission bits because
    // that is the order Linux's `may_delete` asks them in - an append-only
    // directory refuses the removal whether or not the caller could otherwise
    // write there, so the refusal is `EPERM` rather than `EACCES`.
    if is_append_only(parent) {
        return Err(VfsError::NotPermitted);
    }
    let me = crate::sched::current_owner();
    // Write permission on the directory, through the same core every other
    // check uses - so root passes here as everywhere, and the kernel's own
    // paths, which run as root, are not refused the mount table's writes.
    may_as(me, parent, W_OK)?;
    // The sticky half reads the directory's own mode rather than its triads,
    // so it is asked directly; root is not subject to it.
    let mode = parent.mode();
    if mode & S_ISVTX != 0 && me.0 != 0 && me.0 != target.owner().0 && me.0 != parent.owner().0 {
        return Err(VfsError::NotPermitted);
    }
    Ok(())
}

/// [`unlink_path`](fn.unlink_path.html) with a caller-supplied start.
pub fn unlink_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    let dir = resolve_opt(parent, start)?;
    may_remove(&dir, &dir.lookup(name)?)?;
    dir.unlink(name)
}

/// Remove the empty directory named by `path`.
pub fn rmdir_path(path: &str) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    let dir = resolve(parent)?;
    may_remove(&dir, &dir.lookup(name)?)?;
    dir.rmdir(name)
}

/// [`rmdir_path`](fn.rmdir_path.html) with a caller-supplied start.
pub fn rmdir_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    let dir = resolve_opt(parent, start)?;
    may_remove(&dir, &dir.lookup(name)?)?;
    dir.rmdir(name)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// The one rule every filesystem with flags shares, and the three answers
    /// it can give.
    ///
    /// It is worth a test of its own because it is where a *silent* no-op would
    /// live: a `set_flags` that stored the word it was handed would pass every
    /// round-trip test in the tree while quietly dropping the extent bit on a
    /// file whose blocks are an extent tree - the exact shape of bug the
    /// conventions call the worst thing this codebase can do.
    #[test]
    fn a_flag_change_is_kept_refused_or_ignored_by_rule() {
        // A change within the modifiable set is kept, whole.
        assert_eq!(
            apply_flag_change(0, FS_NODUMP_FL | FS_NOATIME_FL, 0),
            Ok(FS_NODUMP_FL | FS_NOATIME_FL)
        );
        // One that clears a flag clears it.
        assert_eq!(apply_flag_change(FS_IMMUTABLE_FL, 0, 0), Ok(0));
        // A bit this kernel cannot keep is refused, and the caller is told
        // rather than obeyed.
        assert_eq!(
            apply_flag_change(0, FS_COMPR_FL, 0),
            Err(VfsError::Unsupported)
        );
        // A change to a bit the filesystem owns is refused too - `owned` here
        // standing in for ext4's extent, index and huge-file bits.
        const OWNED: u32 = FS_EXTENT_FL | FS_INDEX_FL | FS_HUGE_FILE_FL;
        assert_eq!(
            apply_flag_change(FS_EXTENT_FL, 0, OWNED),
            Err(VfsError::Unsupported),
            "clearing the extent bit would not make the file a block map"
        );
        // Passing an owned bit back unchanged is not a change, and is what
        // every `chattr` does - it reads the word, edits one bit, writes the
        // whole word back - so it has to be allowed.
        assert_eq!(
            apply_flag_change(FS_EXTENT_FL, FS_EXTENT_FL | FS_NODUMP_FL, OWNED),
            Ok(FS_EXTENT_FL | FS_NODUMP_FL),
            "the owned bit is carried over, not dropped"
        );
    }

    use alloc::sync::Arc;

    use super::mount::Vfs;
    use super::{NodeKind, R_OK, VfsError, W_OK, X_OK, may_as, permitted, split_parent};

    #[test]
    fn a_bare_name_is_a_name_beside_the_caller() {
        // The shape `mkdir a` uses, and the one that must not answer EINVAL:
        // with no slash there is nothing to split off, so the parent is the
        // caller's own directory and resolving `.` against its cwd finds it.
        assert_eq!(split_parent("a").ok(), Some((".", "a")));
        assert_eq!(split_parent("a/b").ok(), Some(("a", "b")));
        assert_eq!(split_parent("/a").ok(), Some(("/", "a")));
        assert_eq!(split_parent("/a/b/").ok(), Some(("/a", "b")));
        assert_eq!(split_parent("./a").ok(), Some((".", "a")));
    }

    #[test]
    fn the_last_name_cannot_be_a_directory_that_already_exists() {
        // `.` and `..` name directories, not entries to make or remove. A path
        // that ends in one is asking to create or delete a directory by
        // accident, so it is refused rather than resolved to something.
        assert_eq!(split_parent(".").err(), Some(VfsError::Invalid));
        assert_eq!(split_parent("..").err(), Some(VfsError::Invalid));
        assert_eq!(split_parent("/a/.").err(), Some(VfsError::Invalid));
        assert_eq!(split_parent("/").err(), Some(VfsError::Invalid));
        // An empty path is the different case: it names nothing at all, and
        // "nothing there" is what ENOENT means. Linux answers ENOENT for it -
        // and a tracer's startup probe rests on that, checking `chdir("")` for
        // -ENOENT to decide whether this kernel reports syscall results honestly.
        assert_eq!(split_parent("").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn a_relative_path_resolves_from_the_directory_it_is_given() {
        let vfs = Vfs::new();
        let fs = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*fs);
        let root = vfs.resolve("/").unwrap();
        let here = root.create("here", NodeKind::Dir).unwrap();
        let sub = here.create("sub", NodeKind::Dir).unwrap();
        let inside = here.create("f", NodeKind::File).unwrap();
        // The fixture's own root is a filesystem built before any create ran,
        // so it keeps no mode and stands outside the check. This test walks
        // *out* of directories - `..` asks their search bit on the way, and a
        // trailing one climbs to the parent by searching the tree from that
        // root - so give the tree the traversable bits a real one arrives
        // with; without them every climb below reads Access from the birth
        // mode this fixture never set.
        here.set_mode(0o755);
        sub.set_mode(0o755);
        root.set_mode(0o755);
        // And say what the birth mode is, counterfactually: a created node
        // now carries its kind's default minus the mask, so a directory made
        // through `create` needs nothing said about it to be walked through.
        assert_eq!(here.mode(), 0o755);
        assert_eq!(inside.mode(), 0o644);

        // The same name means the same node, whether it is spelled from the
        // root or from the directory it is in.
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("f", here.clone()).unwrap(),
            &inside
        ));
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("./f", here.clone()).unwrap(),
            &inside
        ));
        // An absolute path ignores the start entirely, as `/` must.
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("/here/f", root.clone()).unwrap(),
            &inside
        ));
        // `..` has to actually go up. This is the part an inode cannot do on
        // its own - nothing points from a directory back to its parent - so it
        // is the case that makes the resolver search rather than pop.
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("../f", sub.clone()).unwrap(),
            &inside
        ));
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("../..", sub.clone()).unwrap(),
            &root
        ));
        // `..` at the root stays at the root, as POSIX says.
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("..", root.clone()).unwrap(),
            &root
        ));
        // A file has no `..`: an ordinary walk would have to look a name up in
        // it first, and that is ENOTDIR. Asked as the *last* component, though,
        // Linux's `link_path_walk` never looks the final name up - it hands it
        // to the operation, which is why `open("/etc/hostname/../x")` reads
        // ENOTDIR while `stat("/etc/hostname/..")` answers the parent. The
        // chain here holds only `inside`, so the pop leaves it standing; the
        // parent above a start the walk never descended from is what the
        // bounded search supplies, and the answer is the root it began at.
        //
        // The start node itself is not searched: on the machine a relative
        // path begins at a process's cwd, which is a directory by the time
        // `fchdir` put it there, and this fixture's start is a plain file with
        // no execute bit for anyone. Pinning the shape rather than the policy
        // keeps Linux's contract - the last component is never looked up -
        // without asserting anything about walking *through* a file.
        let popped = vfs.resolve_from("../elsewhere", inside.clone());
        match popped {
            Ok(node) => assert!(
                Arc::ptr_eq(&node, &root),
                "a trailing `..` from a directory cwd climbs to the parent"
            ),
            Err(VfsError::Access) => {}
            Err(e) => panic!(
                "a trailing `..` never answers {e:?}: that would be a lookup \
                 of the last component, which Linux does not perform"
            ),
        }
        // With a name after it, the walk leaves the file and reaches its
        // parent - `here`, which this fixture made traversable - and only then
        // looks the trailing name up. The climb is through two nodes: the
        // search of the plain start (Access, because a file carries no
        // execute bit for anyone), and with real bits on it, the ENOTDIR that
        // says a non-directory has no children to look a name in.
        assert_eq!(
            vfs.resolve_from("../elsewhere/f", inside.clone()).err(),
            Some(VfsError::Access)
        );
        inside.set_mode(0o755);
        assert_eq!(
            vfs.resolve_from("../elsewhere/f", inside).err(),
            Some(VfsError::NotDir)
        );
    }

    #[test]
    fn resolving_a_name_without_following_it_still_handles_dot() {
        // `lstat(".")` is what a directory-listing tool asks first, and it is
        // the case a parent/name split cannot express. It must answer the
        // directory itself rather than refuse.
        let vfs = Vfs::new();
        let fs = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*fs);
        let root = vfs.resolve("/").unwrap();
        let here = root.create("here", NodeKind::Dir).unwrap();
        here.create("f", NodeKind::File).unwrap();

        assert!(Arc::ptr_eq(
            &vfs.resolve_no_follow(".", here.clone()).unwrap(),
            &here
        ));
        assert!(Arc::ptr_eq(
            &vfs.resolve_no_follow("..", here.clone()).unwrap(),
            &root
        ));
        // And the no-follow walk still does not follow, which is the whole
        // reason it exists.
        let link = here.symlink("link", "f").unwrap();
        assert!(Arc::ptr_eq(
            &vfs.resolve_no_follow("link", here.clone()).unwrap(),
            &link
        ));
        assert!(Arc::ptr_eq(
            &vfs.resolve_from("link", here.clone()).unwrap(),
            &here.lookup("f").unwrap()
        ));
        // A *dangling* link is the case that decides whether the no-follow walk
        // is honest: `lstat` of it answers the link, not `ENOENT` for its missing
        // target, and `open(..., O_NOFOLLOW)` answers `ELOOP` rather than
        // following into a target that is not there. Both hang on this walk
        // looking the last name up without ever reading its target - measured on
        // Linux, where `stat(symlink, AT_SYMLINK_NOFOLLOW)` on
        // `slink -> /does-not-exist` succeeds and reports `S_IFLNK`.
        let dangling = here.symlink("gone", "/nowhere").unwrap();
        assert_eq!(
            dangling.metadata().kind,
            NodeKind::Symlink,
            "the no-follow walk must reach the link, not its absent target"
        );
        assert!(Arc::ptr_eq(
            &vfs.resolve_no_follow("gone", here.clone()).unwrap(),
            &dangling
        ));
        // The following walk is the one that refuses, exactly as `stat(2)` does.
        assert_eq!(
            vfs.resolve_from("gone", here.clone()).err(),
            Some(VfsError::NoEntry)
        );
        assert_eq!(
            vfs.resolve_no_follow("missing", root).err(),
            Some(VfsError::NoEntry)
        );
    }

    /// A *relative* symlink reached through an absolute path, in a
    /// subdirectory - the shape a distribution's `libfoo.so.2 -> libfoo.so.2.78`
    /// has, and the one the resolver used to get wrong.
    ///
    /// The existing test above only resolves a relative link as the *first*
    /// component of its path (`resolve_from("link", here)`), where the link's
    /// directory is the start the walk was handed. Here the link sits two
    /// directories down an absolute path, and a relative target has to be
    /// spliced in front of the names still to walk **without** disturbing the
    /// directory the walk has already established - which is exactly the case
    /// that was unpinned.
    #[test]
    fn a_relative_link_deep_in_an_absolute_path_resolves_to_its_target() {
        let vfs = Vfs::new();
        let fs = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*fs);
        let root = vfs.resolve("/").unwrap();
        let usr = root.create("usr", NodeKind::Dir).unwrap();
        let lib = usr.create("lib", NodeKind::Dir).unwrap();
        let real = lib.create("libcap.so.2.78", NodeKind::File).unwrap();
        lib.symlink("libcap.so.2", "libcap.so.2.78").unwrap();

        assert!(
            Arc::ptr_eq(&vfs.resolve("/usr/lib/libcap.so.2").unwrap(), &real),
            "a relative target is relative to the link's own directory"
        );
    }

    #[test]
    fn a_triad_decides_who_may_do_what() {
        // Owner (uid 7, gid 7), mode 0640: the owner reads and writes, the
        // group reads, and a stranger gets nothing.
        assert!(permitted(0o640, (7, 7), R_OK, (7, 7), &[]));
        assert!(permitted(0o640, (7, 7), W_OK, (7, 7), &[]));
        assert!(!permitted(0o640, (7, 7), X_OK, (7, 7), &[]));
        assert!(permitted(0o640, (7, 7), R_OK, (9, 7), &[]));
        assert!(!permitted(0o640, (7, 7), W_OK, (9, 7), &[]));
        assert!(!permitted(0o640, (7, 7), R_OK, (9, 9), &[]));
        // The requested set has to be covered whole: read *and* write needs
        // both bits, which is what `open(O_RDWR)` asks for.
        assert!(!permitted(0o440, (7, 7), R_OK | W_OK, (7, 7), &[]));
        assert!(permitted(0o660, (7, 7), R_OK | W_OK, (7, 7), &[]));
        // An empty mode is `chmod 000`: every triad is empty, so nothing is
        // permitted. That is a different thing from a filesystem that keeps
        // no modes at all, which is what `keeps_modes` tells apart - and why
        // the check cannot be made by looking at the number.
        assert!(!permitted(0, (7, 7), R_OK, (7, 7), &[]));
        assert!(!permitted(0, (7, 7), R_OK, (9, 9), &[]));
        // Linux matches the group triad against the supplementary groups too:
        // uid 9 is no group member by `egid` (9), but a member via the list, so
        // the group's read bit applies to it.
        assert!(permitted(0o640, (7, 7), R_OK, (9, 9), &[7]));
        assert!(!permitted(0o640, (7, 7), W_OK, (9, 9), &[7]));
    }

    #[test]
    fn may_as_answers_for_the_node_and_the_identity_it_is_given() {
        let vfs = Vfs::new();
        let fs = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*fs);
        let root = vfs.resolve("/").unwrap();
        let file = root.create("f", NodeKind::File).unwrap();
        file.set_mode(0o600);
        file.set_owner(10, 10);

        // The owner may read it, a stranger may not. Root's answer is no
        // longer "regardless" - that reading was the pass-everything shape
        // `root_passes_where_any_bit_of_the_kind_exists` replaced: uid 0 here
        // passes because the *owner* triad carries the read bit, which is
        // Linux's `CAP_DAC_OVERRIDE` sentence, not an exception to it.
        assert_eq!(may_as((10, 10), &file, R_OK).ok(), Some(()));
        assert_eq!(may_as((11, 11), &file, R_OK).err(), Some(VfsError::Access));
        assert_eq!(may_as((0, 0), &file, R_OK).ok(), Some(()));
        // A node the filesystem kept no mode for is not policed. A created
        // node now *has* a birth mode, so the shape has to be said: an
        // explicit zero word means `chmod 000`, and only a filesystem that
        // answers `keeps_modes()` false stands outside the check. The test's
        // own root directory is exactly one - `RamFs::new` builds it before
        // any create runs, and its word stays unset.
        assert!(!root.keeps_modes());
        assert_eq!(may_as((11, 11), &root, R_OK | W_OK | X_OK).ok(), Some(()));
    }

    #[test]
    fn root_passes_where_any_bit_of_the_kind_exists() {
        // The measured Linux rule (`capable(CAP_DAC_OVERRIDE)` in
        // `fs/namei.c`): superuser read/write passes when *any* triad carries
        // the bit, execute only when some execute bit exists. This kernel's
        // override used to be "uid 0 passes every check outright", and on a
        // single-user image whose root does everything that reads the same -
        // until a distribution's tree arrives with real modes. Alpine's `/bin`
        // is `0755` and every component of a path root walks carries its
        // owner-read bit, so the old shape and the new one agree there; what
        // changed is the fine print, and the old exception-less pass made it
        // *less* honest, not more: it answered success for an execute nobody
        // set. The pin is the counterfactual: without the branch, `(0,0)`
        // against these nodes is `Access` where Linux says yes, and `Ok`
        // where Linux says no.
        let vfs = Vfs::new();
        let fs = crate::vfs::ramfs::RamFs::new();
        vfs.mount_root(&*fs);
        let root = vfs.resolve("/").unwrap();

        let dir = root.create("d", NodeKind::Dir).unwrap();
        dir.set_mode(0o644); // owner rw-, group r--, other r--: no x anywhere
        assert_eq!(
            may_as((0, 0), &dir, X_OK).err(),
            Some(VfsError::Access),
            "root gets no exception where no execute bit exists at all"
        );
        dir.set_mode(0o645); // other gains --x
        assert_eq!(
            may_as((0, 0), &dir, X_OK).ok(),
            Some(()),
            "one execute bit anywhere is enough for root"
        );

        let file = root.create("f", NodeKind::File).unwrap();
        file.set_mode(0o600); // owner rw- only
        assert_eq!(
            may_as((0, 0), &file, R_OK).ok(),
            Some(()),
            "root reads what any triad can read"
        );
        assert_eq!(
            may_as((0, 0), &file, W_OK).ok(),
            Some(()),
            "and writes what any triad can write"
        );
        assert_eq!(
            may_as((0, 0), &file, X_OK).err(),
            Some(VfsError::Access),
            "but never invents an execute permission"
        );
        // A non-root caller falls through to triads exactly as before:
        // this is the half that must not move.
        assert_eq!(may_as((11, 11), &file, R_OK).err(), Some(VfsError::Access));
        assert_eq!(may_as((11, 11), &dir, X_OK).err(), Some(VfsError::Access));

        // The sentence one bit at a time: `may_create_in` asks write *and*
        // search together, and Linux's override passes it when each kind has
        // its somewhere - here write in the owner triad, execute only in
        // other's. An all-or-nothing reading of the pair would refuse what
        // `link_path_walk` grants.
        dir.set_mode(0o601);
        assert_eq!(
            may_as((0, 0), &dir, W_OK | X_OK).ok(),
            Some(()),
            "each requested kind needs only its own bit to exist"
        );
        // And a node that stores no bits at all stays outside the check even
        // under a filesystem that keeps modes once they are said - the shape
        // the kernel's own trees carry before anything chmods them.
        let untouched = crate::vfs::ramfs::RamFs::new();
        assert!(!untouched.root().keeps_modes());
        assert_eq!(
            may_as((0, 0), &untouched.root(), X_OK).ok(),
            Some(()),
            "a root nobody set a mode on is not refused to root"
        );
    }
}
