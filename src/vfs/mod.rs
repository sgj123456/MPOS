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
pub mod ext2;
pub mod file;
pub mod mount;
pub mod path;
pub mod pipe;
pub mod ramfs;
pub mod tar;
pub mod tty;

pub use file::{File, RawFd, close, dup2, open, open_opt, read, write};
pub use mount::{global, mount, mount_root, resolve, resolve_at};

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

    /// The names in this directory, each with the kind of node it names. The
    /// order is the filesystem's own; "." and ".." are included where the
    /// filesystem keeps them, as POSIX readdir reports them. A non-directory
    /// has no names, and the default answers `NotDir` for one.
    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        let _ = self;
        Err(VfsError::NotDir)
    }
}

/// A mounted filesystem instance: what other systems would call a superblock.
///
/// The instance owns the per-mount state (ramfs: its node graph; ext2 later:
/// its block device handle and on-disk counters) and hands out its root.
pub trait FileSystem: Send + Sync {
    fn root(&self) -> Arc<dyn Inode>;
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
/// `/f` is the root and `f`; a path with no slash has no parent here to name,
/// and `.` and `..` are directories that already exist, not names to make.
fn split_parent(path: &str) -> VfsResult<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let Some(slash) = trimmed.rfind('/') else {
        return Err(VfsError::Invalid);
    };
    let name = &trimmed[slash + 1..];
    if name.is_empty() || name == "." || name == ".." {
        return Err(VfsError::Invalid);
    }
    let parent = if slash == 0 { "/" } else { &trimmed[..slash] };
    Ok((parent, name))
}

/// [`split_parent`](fn.split_parent.html) for a caller that carries a cwd:
/// a bare relative name - no slash at all - has the cwd itself as its
/// parent, which is exactly what `./name` spells. Only a caller with a cwd
/// can say that; the kernel's own callers keep the strict split.
fn split_parent_with_cwd(path: &str, has_cwd: bool) -> VfsResult<(&str, &str)> {
    if !path.contains('/') {
        // A bare name next to a cwd: "." from the cwd is the cwd.
        return if has_cwd {
            Ok((".", path))
        } else {
            Err(VfsError::Invalid)
        };
    }
    split_parent(path)
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
    let (parent, name) = split_parent_with_cwd(path, start.is_some())?;
    resolve_opt(parent, start)?.create(name, NodeKind::File)
}

/// [`unlink_path`](fn.unlink_path.html) with a caller-supplied start.
pub fn unlink_opt(path: &str, start: Option<Arc<dyn Inode>>) -> VfsResult<()> {
    let (parent, name) = split_parent_with_cwd(path, start.is_some())?;
    resolve_opt(parent, start)?.unlink(name)
}
