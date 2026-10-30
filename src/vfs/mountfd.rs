//! The descriptor `open_tree(2)` hands out: a mount, not a file.
//!
//! Linux's `open_tree` answers an `O_PATH` descriptor whose subject is a
//! *mount*: `fstat` reports the mount root's directory (measured 040755 on a
//! tmpfs mount, both for the plain and for the `OPEN_TREE_CLONE` spelling),
//! `readlink /proc/self/fd/N` reports where that mount stands (`/tmp/ma2/a`
//! for a plain descriptor, `/` for a detached clone, whose own tree's root is
//! what its path is), and `move_mount` is the call that *moves* one: a
//! detached clone can be attached anywhere, while a descriptor that does not
//! name a mount point answers `EINVAL` there.
//!
//! This kernel's mount table attaches an inode subtree under a name
//! (`MountTable::bind_mount`), so the descriptor can hold exactly that: the
//! inode the mount's root answers with, plus the path the table knew it by.
//! Attaching it is a bind of that inode under the target name - the same
//! operation `mount(2)`'s `MS_BIND` makes name - which is why the two calls
//! share one implementation rather than growing a second mount model.
//!
//! A clone is *detached* here in the only sense this table has: nothing names
//! it until `move_mount` does, and until then it is reachable only through
//! its descriptor. The recursive half of `open_tree`'s contract (a subtree's
//! own mounts coming along) is not one this model can tell apart from the
//! subtree itself - a `bind_mount` of a directory already shows whatever is
//! mounted under it - so `OPEN_TREE_CLONE|AT_RECURSIVE` is served as the same
//! one-mount clone and the difference is named in `docs/notes/`.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{FileSystem, Inode, Metadata, VfsError, VfsResult};

/// The inode numbers descriptors of this class report. A mount descriptor is
/// not a file in any tree, so no filesystem can hand out a number that
/// collides with it.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x9600_0000);

/// One `open_tree(2)` descriptor.
pub struct MountFd {
    /// The inode the mount's root answers with.
    root: Arc<dyn Inode>,
    /// What `/proc/<pid>/fd/<n>` reports: the mounted path for a plain
    /// `open_tree`, and `/` for a detached clone (its own tree's root).
    display: String,
    /// The path the mount stood at when the descriptor was made - the name
    /// `bind_mount` needs to find the serving filesystem when the clone is
    /// attached, which a detached clone can no longer be asked for.
    source_path: String,
    /// Whether this descriptor names a mount that can be attached elsewhere:
    /// a clone always can, a plain descriptor only when its path was a mount
    /// point (`move_mount`'s measured `EINVAL` for anything else).
    attachable: bool,
    /// The read-only promise the source mount carried, carried on so that
    /// attaching a clone of a read-only mount does not silently widen it.
    read_only: bool,
    /// A filesystem that exists but has never been mounted - what
    /// `fsmount(2)` hands out. Attaching one is not a bind (there is no
    /// second name for it to alias) but a first mount, which is why this is
    /// carried here rather than invented as a pivot in the syscall layer.
    fresh: Option<FreshMount>,
    ino: u64,
}

/// The filesystem an `fsmount(2)` descriptor names, with the facts the mount
/// table needs to attach it: what type it is, the source name the caller gave,
/// and whether it is read-only.
pub struct FreshMount {
    /// The filesystem the context's `FSCONFIG_CMD_CREATE` built.
    pub fs: Arc<dyn FileSystem>,
    /// The type `fsopen` named, recorded on the mount the way `mount(2)`'s
    /// `fstype` argument is.
    pub fstype: String,
    /// The source name (`/dev/vda3`, a 9p tag, or the type's own name).
    pub source: String,
    /// Whether the mount's attributes made it read-only.
    pub read_only: bool,
}

impl FreshMount {
    pub fn new(
        fs: Arc<dyn FileSystem>,
        fstype: String,
        source: String,
        read_only: bool,
    ) -> FreshMount {
        FreshMount {
            fs,
            fstype,
            source,
            read_only,
        }
    }
}

impl MountFd {
    pub fn new(
        root: Arc<dyn Inode>,
        display: String,
        source_path: String,
        attachable: bool,
        read_only: bool,
    ) -> Arc<MountFd> {
        Arc::new(MountFd {
            root,
            display,
            source_path,
            attachable,
            read_only,
            fresh: None,
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The descriptor `fsmount(2)` hands out: a filesystem that exists and is
    /// attached nowhere. Its display is `/` - a detached mount's own tree's
    /// root, which is what a clone reports too - and it is attachable by
    /// construction, because attaching it is the only thing it is for.
    pub fn fresh(mount: FreshMount, display: String) -> Arc<MountFd> {
        let root = mount.fs.root();
        let read_only = mount.read_only;
        let source = mount.source.clone();
        Arc::new(MountFd {
            root,
            display,
            source_path: source,
            attachable: true,
            read_only,
            fresh: Some(mount),
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The never-mounted filesystem this descriptor names, when it names one -
    /// how `move_mount` tells an `fsmount` descriptor from an `open_tree`
    /// clone.
    pub fn fresh_mount(&self) -> Option<&FreshMount> {
        self.fresh.as_ref()
    }

    /// The inode a `move_mount` of this descriptor attaches.
    pub fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    /// The name `bind_mount` records as the mount's source.
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// Whether `move_mount` may consume this descriptor as its empty-path
    /// source (Linux's `EINVAL` for a descriptor that names no mount).
    pub fn attachable(&self) -> bool {
        self.attachable
    }

    /// Whether the mount it names was read-only.
    /// The name `/proc/<pid>/fd/<n>` answers with, kept so `open_tree(2)` of a
    /// descriptor's own file reports what the first descriptor did.
    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

impl Inode for MountFd {
    fn metadata(&self) -> Metadata {
        // The subject is the mount's root, so `stat` on the descriptor asks
        // the same question a path into the mount would: same inode, same
        // kind, same size.
        let meta = self.root.metadata();
        Metadata {
            ino: self.ino,
            kind: meta.kind,
            size: meta.size,
        }
    }

    fn fd_name(&self) -> Option<String> {
        Some(self.display.clone())
    }

    /// The descriptor names a mount, not a byte stream: reading or writing it
    /// is `EBADF`, the answer an `O_PATH` descriptor gets.
    /// Measured on Linux: `read(2)` on a mount descriptor is `EBADF`, even
    /// though `fstat` reports the root directory - the object has no file
    /// behind it, so the kind's own rule must not answer first.
    fn refuses_file_io(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::BadFd)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::BadFd)
    }

    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn Inode>> {
        Err(VfsError::NotDir)
    }

    #[cfg(target_os = "none")]
    fn as_mount_fd(&self) -> Option<&MountFd> {
        Some(self)
    }
}

impl core::fmt::Debug for MountFd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountFd")
            .field("display", &self.display)
            .field("attachable", &self.attachable)
            .finish()
    }
}
