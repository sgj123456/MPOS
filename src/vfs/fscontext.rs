//! The descriptor `fsopen(2)` hands out: a filesystem *context*, not a file.
//!
//! Linux splits "make a filesystem" from "put it somewhere": `fsopen` names the
//! type, `fsconfig` speaks the type's options, `fsmount` turns the finished
//! context into a mount descriptor, and `move_mount` is what attaches it. This
//! kernel's `mount(2)` builds the `FileSystem` and attaches it in one call, so
//! the context is the missing middle: a description of a filesystem that does
//! not exist yet.
//!
//! The descriptor's own shape was measured on Linux 7.2.11 before it was
//! written (`.workbuddy/probe_mntapi*.c`; the table is `docs/notes/` §3.16):
//! `fstat` reports a 0-byte regular file with mode `0600`, `read(2)` answers
//! `ENODATA` (the context has nothing to read back and says so - the same
//! `ENODATA` an extended attribute's absence answers), `write(2)` is `EINVAL`,
//! `pread`/`lseek` are `ESPIPE`, and `fcntl(F_GETFL)` reports `O_RDWR`.
//!
//! The state machine is Linux's, measured: options may be set until
//! `FSCONFIG_CMD_CREATE`, after which every `SET_*` - and a second `CREATE` -
//! answer `EBUSY` (the creation is a one-way door), while `fsmount` before the
//! creation is `EINVAL` and a second `fsmount` from one context is `EBUSY`.
//! `FSCONFIG_CMD_CREATE_EXCL` is the same creation when no superblock exists
//! to reuse, which is every creation this kernel makes.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{FileSystem, Inode, Metadata, NodeKind, VfsError, VfsResult};

/// The inode numbers these descriptors report. A filesystem context is not a
/// file in any tree, so no filesystem can hand out a number that collides.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x9700_0000);

/// Linux's `FSOPEN_CLOEXEC`, the only bit `fsopen`'s flag word carries: every
/// other bit was measured answering `EINVAL` (`0x8000_0000` on Linux 7.2.11).
pub const FSOPEN_CLOEXEC: u32 = 0x1;

/// Linux's `FSCONFIG_*` command numbers, from `<linux/mount.h>`.
pub const FSCONFIG_SET_FLAG: u32 = 0;
pub const FSCONFIG_SET_STRING: u32 = 1;
pub const FSCONFIG_SET_BINARY: u32 = 2;
pub const FSCONFIG_SET_PATH: u32 = 3;
pub const FSCONFIG_SET_PATH_EMPTY: u32 = 4;
pub const FSCONFIG_SET_FD: u32 = 5;
pub const FSCONFIG_CMD_CREATE: u32 = 6;
pub const FSCONFIG_CMD_RECONFIGURE: u32 = 7;
pub const FSCONFIG_CMD_CREATE_EXCL: u32 = 8;

/// Linux's `FSMOUNT_CLOEXEC` and `FSMOUNT_NAMESPACE`: the whole of what
/// `fsmount`'s own flag word carries. A mount *attribute* (`MOUNT_ATTR_RDONLY`
/// and friends) is that call's third argument, not this word - passing
/// `MS_RDONLY` here is `EINVAL`, measured.
pub const FSMOUNT_CLOEXEC: u32 = 0x1;
pub const FSMOUNT_NAMESPACE: u32 = 0x2;

/// One context's state, under the lock the whole kernel shares.
struct State {
    fstype: String,
    /// What the type's `source` option named, when it took one. An empty
    /// string is a type whose source is not a path ("tmpfs", "proc").
    source: String,
    /// The options spoken so far, as `(key, value)` pairs. They are joined
    /// into one comma-separated word at creation time, which is the shape the
    /// filesystems' own parsers take - the same string `mount(2)`'s data
    /// argument carries.
    options: Vec<(String, String)>,
    /// The filesystem the creation built, once `FSCONFIG_CMD_CREATE` has.
    created: Option<Arc<dyn FileSystem>>,
    /// Whether `fsmount` has spent this context. One context makes one mount:
    /// a second `fsmount` is `EBUSY`, measured.
    mounted: bool,
}

/// One `fsopen(2)` descriptor.
pub struct FsContext {
    fstype: String,
    state: critical_section::Mutex<RefCell<State>>,
    ino: u64,
}

impl FsContext {
    pub fn new(fstype: String) -> Arc<FsContext> {
        Arc::new(FsContext {
            fstype: fstype.clone(),
            state: critical_section::Mutex::new(RefCell::new(State {
                fstype,
                source: String::new(),
                options: Vec::new(),
                created: None,
                mounted: false,
            })),
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The type `fsopen` named - the question the options are validated
    /// against, so it is answered without the lock.
    pub fn fstype(&self) -> &str {
        &self.fstype
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        critical_section::with(|cs| f(&mut self.state.borrow(cs).borrow_mut()))
    }

    /// Speak one string option. `source` is the one key every type shares: it
    /// names whatever the type's source is (a block device's path, a 9p tag,
    /// nothing at all), which is why it is not kept in the option list.
    pub fn set_option(&self, key: &str, value: &str) -> VfsResult<()> {
        self.with(|state| {
            if state.created.is_some() {
                return Err(VfsError::Busy);
            }
            if key == "source" {
                state.source = value.to_string();
            } else {
                state.options.push((key.to_string(), value.to_string()));
            }
            Ok(())
        })
    }

    /// Speak one flag option - a key with no value. The filesystems here take
    /// none today, which is why no caller reaches this with a key that means
    /// something; it refuses the way Linux refuses an option its type does not
    /// know (`EINVAL`, measured on a tmpfs context).
    pub fn set_flag(&self, key: &str) -> VfsResult<()> {
        self.with(|state| {
            if state.created.is_some() {
                return Err(VfsError::Busy);
            }
            match key {
                // Linux's own answer for a flag-shaped option: it is not one
                // this kernel's filesystems read.
                _ => Err(VfsError::Invalid),
            }
        })
    }

    /// The option word the filesystem's parser will take, comma-joined the way
    /// `mount(2)`'s data argument is.
    pub fn data(&self) -> String {
        self.with(|state| {
            let mut data = String::new();
            for (key, value) in &state.options {
                if !data.is_empty() {
                    data.push(',');
                }
                data.push_str(key);
                if !value.is_empty() {
                    data.push('=');
                    data.push_str(value);
                }
            }
            data
        })
    }

    pub fn source(&self) -> String {
        self.with(|state| state.source.clone())
    }

    /// Record the filesystem the creation built. A second creation is
    /// `EBUSY`: the one-way door is the state machine, not a policy.
    pub fn create(&self, fs: Arc<dyn FileSystem>) -> VfsResult<()> {
        self.with(|state| {
            if state.created.is_some() {
                return Err(VfsError::Busy);
            }
            state.created = Some(fs);
            Ok(())
        })
    }

    /// Hand the created filesystem to `fsmount`, spending the context.
    /// `EINVAL` before a creation, `EBUSY` after one mount - both measured.
    pub fn take_for_mount(&self) -> VfsResult<Arc<dyn FileSystem>> {
        self.with(|state| {
            let fs = state.created.clone().ok_or(VfsError::Invalid)?;
            if state.mounted {
                return Err(VfsError::Busy);
            }
            state.mounted = true;
            Ok(fs)
        })
    }

    /// Whether the creation has happened - the test `fsconfig`'s option
    /// commands run before doing anything else.
    pub fn is_created(&self) -> bool {
        self.with(|state| state.created.is_some())
    }
}

impl Inode for FsContext {
    fn metadata(&self) -> Metadata {
        // A 0-byte file: the context is an object with no bytes behind it, and
        // `fstat` says so rather than inventing a size.
        Metadata {
            ino: self.ino,
            kind: NodeKind::File,
            size: 0,
        }
    }

    /// `read(2)` on a context answers `ENODATA` - measured. There is nothing
    /// to read back: the options are write-only, and Linux's answer says the
    /// absence is a fact about the object rather than about the buffer.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NoData)
    }

    /// `write(2)` is `EINVAL` - measured. The options have their own door
    /// (`fsconfig`), and a byte stream would be a second dialect for them.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn Inode>> {
        Err(VfsError::NotDir)
    }

    #[cfg(target_os = "none")]
    fn as_fs_context(&self) -> Option<&FsContext> {
        Some(self)
    }

    /// A context has no name of its own: `/proc/<pid>/fd/<n>` reports the
    /// anonymous inode Linux reports for the same descriptor.
    fn fd_name(&self) -> Option<String> {
        Some(String::from("anon_inode:[fscontext]"))
    }
}

impl core::fmt::Debug for FsContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FsContext")
            .field("fstype", &self.fstype)
            .field("created", &self.is_created())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Arc<FsContext> {
        FsContext::new(String::from("tmpfs"))
    }

    #[test]
    fn an_option_is_a_key_and_a_value_until_the_creation() {
        let ctx = context();
        ctx.set_option("size", "1M").unwrap();
        assert_eq!(ctx.data(), String::from("size=1M"));
        // `source` is the type's own name, not an option in the word.
        ctx.set_option("source", "tmpfs").unwrap();
        assert_eq!(ctx.source(), String::from("tmpfs"));
        assert_eq!(ctx.data(), String::from("size=1M"));
    }

    #[test]
    fn the_creation_is_a_one_way_door() {
        let ctx = context();
        let fs: Arc<dyn FileSystem> = crate::vfs::ramfs::RamFs::new();
        ctx.set_option("size", "1M").unwrap();
        // Nothing has been created yet, so nothing can be mounted.
        assert!(matches!(ctx.take_for_mount(), Err(VfsError::Invalid)));
        ctx.create(fs).unwrap();
        assert!(ctx.is_created());
        // A second creation, and any option after the first, answer `EBUSY` -
        // Linux's measured answer for a context that has already made a
        // superblock.
        let again: Arc<dyn FileSystem> = crate::vfs::ramfs::RamFs::new();
        assert!(matches!(ctx.create(again), Err(VfsError::Busy)));
        assert!(matches!(ctx.set_option("size", "2M"), Err(VfsError::Busy)));
        assert!(matches!(ctx.set_flag("nosuid"), Err(VfsError::Busy)));
    }

    #[test]
    fn one_context_makes_one_mount() {
        let ctx = context();
        let fs: Arc<dyn FileSystem> = crate::vfs::ramfs::RamFs::new();
        ctx.create(fs).unwrap();
        assert!(ctx.take_for_mount().is_ok());
        // The context is spent: a second `fsmount` is Linux's measured `EBUSY`.
        assert!(matches!(ctx.take_for_mount(), Err(VfsError::Busy)));
    }

    #[test]
    fn a_context_has_no_bytes_to_read() {
        let ctx = context();
        let mut buf = [0u8; 4];
        // Measured: `read(2)` answers `ENODATA` and `write(2)` `EINVAL`.
        assert!(matches!(ctx.read_at(0, &mut buf), Err(VfsError::NoData)));
        assert!(matches!(ctx.write_at(0, b"x"), Err(VfsError::Invalid)));
        let meta = ctx.metadata();
        assert_eq!(meta.kind, NodeKind::File);
        assert_eq!(meta.size, 0);
    }
}
