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
pub mod epoll;
pub mod eventfd;
pub mod ext4;
pub mod file;
pub mod locks;
pub mod mount;
pub mod path;
pub mod pipe;
pub mod procfs;
pub mod pty;
pub mod ramfs;
pub mod signalfd;
pub mod sysfs;
pub mod tar;
pub mod termios;
pub mod timerfd;
pub mod tty;
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
}

pub type VfsResult<T> = Result<T, VfsError>;

/// One node of one filesystem, in the only form the rest of the kernel sees.
///
/// Every method has a default that fails, so each filesystem overrides exactly
/// what its storage can do.
pub trait Inode: Send + Sync {
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

    /// What this node can do without blocking, for `poll(2)`.
    ///
    /// The default is "everything": a node that never blocks - a file, a
    /// directory, a device that always has an answer - is always ready. Only
    /// the nodes that can make a caller wait say otherwise.
    fn poll_ready(&self) -> Ready {
        Ready::ALL
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

/// The root override, Linux's `CAP_DAC_OVERRIDE`: root passes every check
/// without consulting the owner or the triads, with the one exception the
/// capability keeps - execute on a non-directory still needs an execute bit set
/// for someone, so even root cannot run a file nobody ever marked runnable, and
/// `access(X_OK)` on a `0644` file is `EACCES` for the superuser too. This is
/// Linux's exact behaviour; the earlier "root bypasses all" simplification is
/// dropped in favour of matching it.
fn root_override(mode: u32, want: u32, is_dir: bool) -> bool {
    want & X_OK == 0 || is_dir || mode & 0o111 != 0
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
    let mode = inode.mode();
    let allowed = if me.0 == 0 {
        root_override(mode, want, inode.metadata().kind == NodeKind::Dir)
    } else {
        permitted(mode, inode.owner(), want, me, &crate::sched::current_groups())
    };
    if allowed {
        Ok(())
    } else {
        Err(VfsError::Access)
    }
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
    use alloc::sync::Arc;

    use super::mount::Vfs;
    use super::{
        NodeKind, R_OK, VfsError, W_OK, X_OK, may_as, permitted, root_override, split_parent,
    };

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
        // it first, and that is ENOTDIR.
        assert_eq!(
            vfs.resolve_from("../elsewhere", inside).err(),
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
        assert_eq!(
            vfs.resolve_no_follow("missing", root).err(),
            Some(VfsError::NoEntry)
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
    fn root_passes_everything_but_execute_on_a_file_nobody_made_runnable() {
        // Root overrides the triads outright...
        assert!(root_override(0o000, R_OK | W_OK, false));
        // ...including search on a directory.
        assert!(root_override(0o000, X_OK, true));
        // Linux's `CAP_DAC_OVERRIDE` keeps one exception: execute on a plain
        // file needs an x bit for someone, and root is not above it.
        assert!(!root_override(0o666, X_OK, false));
        assert!(root_override(0o111, X_OK, false));
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

        // The owner may read it, a stranger may not, and root may regardless.
        assert_eq!(may_as((10, 10), &file, R_OK).ok(), Some(()));
        assert_eq!(may_as((11, 11), &file, R_OK).err(), Some(VfsError::Access));
        assert_eq!(may_as((0, 0), &file, R_OK).ok(), Some(()));
        // A node the filesystem kept no mode for is not policed.
        let bare = root.create("bare", NodeKind::File).unwrap();
        assert_eq!(may_as((11, 11), &bare, R_OK | W_OK | X_OK).ok(), Some(()));
    }
}
