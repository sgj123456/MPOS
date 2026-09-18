//! The virtual filesystem: inodes, name edges, mounts, and path resolution.
//!
//! The seam here is the [`Inode`] trait. Everything above it - resolution, the
//! fd table, later syscalls - speaks only in `Arc<dyn Inode>`; everything below
//! it (ramfs now, devfs and tar initramfs next, ext2 after) implements the
//! trait over its own storage. Operations a filesystem cannot support keep
//! their default bodies, which return errors, so a read-only filesystem never
//! writes a stub for `write_at`.
//!
//! Dentries - the name-to-inode edges - have no cache of their own yet. Each
//! filesystem holds its edges directly (ramfs in per-directory maps), and
//! [`mount::Vfs::resolve`] is the only walker, treating every edge as fresh.
//! A shared dcache becomes worth its complexity when ext2 arrives with
//! on-disk directories worth caching; nothing before then is.
//!
//! [`mount::Vfs`] is instantiable rather than global so tests can build
//! isolated tables; the kernel holds one instance, exposed by [`mount_root`],
//! [`mount`] and [`resolve`].

pub mod devfs;
pub mod devpts;
pub mod eventfd;
pub mod ext2;
pub mod file;
pub mod mount;
pub mod path;
pub mod pipe;
pub mod procfs;
pub mod ramfs;
pub mod signalfd;
pub mod sysfs;
pub mod tar;
pub mod termios;
pub mod tty;

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
    /// EXDEV: a hard link whose two ends live on different filesystems -
    /// the one request `link(2)` refuses by design, since a name edge is
    /// the filesystem's own bookkeeping and cannot reach into another's.
    CrossDevice,
    /// A read or write that would block, on a descriptor the caller asked
    /// not to block on.
    Again,
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

    /// The `socketpair` end this node is, if it is one.
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

    /// The permission bits (the low twelve) the filesystem stores for this
    /// node, or `0` when it carries none - the caller then falls back to what
    /// the node's kind implies. ext2 carries the real thing, and OpenRC runs
    /// on it: it refuses to start any service script a `test -x` rejects, and
    /// a kernel that hands out one fixed mode per kind leaves every script
    /// 0644 and every runlevel silent.
    fn mode(&self) -> u32 {
        0
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
/// The instance owns the per-mount state (ramfs: its node graph; ext2 later:
/// its block device handle and on-disk counters) and hands out its root.
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
    let mut bytes = Vec::with_capacity(size);
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
pub fn read_file_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<Vec<u8>> {
    let inode = resolve_opt(path, start)?;
    if inode.metadata().kind != NodeKind::File {
        return Err(VfsError::IsDir);
    }
    let file = File::new(inode);
    let size = file.metadata().size as usize;
    let mut bytes = Vec::with_capacity(size);
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

/// [`unlink_path`](fn.unlink_path.html) with a caller-supplied start.
pub fn unlink_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    resolve_opt(parent, start)?.unlink(name)
}

/// Remove the empty directory named by `path`.
pub fn rmdir_path(path: &str) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    resolve(parent)?.rmdir(name)
}

/// [`rmdir_path`](fn.rmdir_path.html) with a caller-supplied start.
pub fn rmdir_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let (parent, name) = split_parent(path)?;
    resolve_opt(parent, start)?.rmdir(name)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::sync::Arc;

    use super::mount::Vfs;
    use super::{NodeKind, VfsError, split_parent};

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
        assert_eq!(split_parent("").err(), Some(VfsError::Invalid));
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
}
