//! Linux-ABI file handlers: the path-carrying calls in Linux argument shapes.
//!
//! Linux passes paths as NUL-terminated strings - one pointer, no length
//! word - and many calls lead with a dirfd the path is relative to. That is
//! a different register layout from the mpos ABI's `(ptr, len)` pairs, so
//! these handlers decode the Linux shapes and speak to the VFS directly;
//! the mpos handlers in [`super::super::fs`] keep their own callers.

use alloc::string::String;
use alloc::sync::Arc;

use crate::syscall::{ARG_COUNT, buffer_ok, cstr_arg, err, err_of};
use crate::vfs::{Inode, Metadata, NodeKind};

/// `openat`'s magic dirfd: the path is relative to the caller's cwd.
const AT_FDCWD: i32 = -100;
/// `fstatat` by descriptor: an empty path names the dirfd itself.
const AT_EMPTY_PATH: usize = 0x1000;

/// The inode a dirfd-led path resolves from: the descriptor's directory, or
/// the caller's cwd for `AT_FDCWD`.
fn base_of(dirfd: i32, cwd: &Arc<dyn Inode>) -> Result<Arc<dyn Inode>, isize> {
    if dirfd == AT_FDCWD {
        return Ok(cwd.clone());
    }
    let file = crate::sched::current_fds()
        .clone_at(dirfd)
        .map_err(err_of)?;
    if file.metadata().kind != NodeKind::Dir {
        return Err(err::ENOTDIR);
    }
    Ok(file.inode())
}

/// The mode bits `struct stat` and `struct statx` carry for a node.
fn mode_of(meta: &Metadata) -> u32 {
    match meta.kind {
        NodeKind::File => 0o100_644,    // regular file rw-r--r--
        NodeKind::Dir => 0o040_755,     // directory rwxr-xr-x
        NodeKind::CharDev => 0o020_666, // character device rw-rw-rw-
    }
}

/// `open(path, flags, mode)` - the flags/mode pair is accepted and ignored,
/// as for every open here: opens are read/write-capable and nothing is
/// inherited across an exec that does not happen.
pub(super) fn open(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::open_opt(&path, Some(cwd.clone())) {
        Ok(fd) => fd as isize,
        Err(e) => err_of(e),
    }
}

/// `openat(dirfd, path, flags, mode)`.
pub(super) fn openat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::open_opt(&path, Some(base)) {
        Ok(fd) => fd as isize,
        Err(e) => err_of(e),
    }
}

/// `fstat(fd, buf)` - the fd's description, not a path, is what is statted.
pub(super) fn fstat(args: [usize; ARG_COUNT]) -> isize {
    let (fd, buf) = (args[0] as i32, args[1]);
    let meta = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file.metadata(),
        Err(e) => return err_of(e),
    };
    write_stat(buf, &meta)
}

/// `newfstatat(dirfd, path, buf, flags)` - stat by path, relative to `dirfd`.
pub(super) fn newfstatat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (dirfd, path_ptr, buf, flags) = (args[0] as i32, args[1], args[2], args[3]);
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // AT_EMPTY_PATH is the fstatat spelling of "stat the descriptor".
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        let meta = match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => file.metadata(),
            Err(e) => return err_of(e),
        };
        return write_stat(buf, &meta);
    }
    let base = match base_of(dirfd, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let meta = match crate::vfs::resolve_opt(&path, Some(base)) {
        Ok(inode) => inode.metadata(),
        Err(e) => return err_of(e),
    };
    write_stat(buf, &meta)
}

/// `statx(dirfd, path, flags, mask, buf)` - the extended stat uutils calls.
///
/// Every `STATX_BASIC_STATS` bit the caller asked for is filled; timestamps
/// are not, and the mask says so. The struct is zeroed through `stx_mnt_id`
/// (152 bytes) first, so fields the caller did not ask about read as absent.
pub(super) fn statx(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    const STATX_SIZE: usize = 152;

    let (dirfd, path_ptr, _flags, mask, buf) =
        (args[0] as i32, args[1], args[2], args[3] as u32, args[4]);
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let meta = if path.is_empty() {
        match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => file.metadata(),
            Err(e) => return err_of(e),
        }
    } else {
        let base = match base_of(dirfd, cwd) {
            Ok(base) => base,
            Err(e) => return e,
        };
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => inode.metadata(),
            Err(e) => return err_of(e),
        }
    };

    if !buffer_ok(buf, STATX_SIZE, true, true) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, STATX_SIZE);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u32).write_volatile(mask & STATX_BASIC_STATS); // stx_mask
        (at(4) as *mut u32).write_volatile(512); // stx_blksize
        (at(16) as *mut u32).write_volatile(if meta.kind == NodeKind::Dir { 2 } else { 1 }); // stx_nlink
        (at(28) as *mut u16).write_volatile(mode_of(&meta) as u16); // stx_mode
        (at(32) as *mut u64).write_volatile(meta.ino); // stx_ino
        (at(40) as *mut u64).write_volatile(meta.size); // stx_size
        (at(48) as *mut u64).write_volatile(meta.size.div_ceil(512) as u64); // stx_blocks
    }
    0
}

/// `access(path, mode)` - is the path there and reachable? Mode is ignored:
/// one user, no permission bits.
pub(super) fn access(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `faccessat(dirfd, path, mode, flags)` - the dirfd form of access.
pub(super) fn faccessat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(base)) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `chdir(path)`.
pub(super) fn chdir(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_at(&path, cwd.clone()) {
        Ok(dir) if dir.metadata().kind == NodeKind::Dir => {
            crate::sched::set_cwd(dir);
            0
        }
        Ok(_) => err::ENOTDIR,
        Err(e) => err_of(e),
    }
}

/// `unlinkat(dirfd, path, flags)` - `AT_REMOVEDIR` is accepted and ignored:
/// removal goes through the same unlink either way.
pub(super) fn unlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::unlink_opt(&path, Some(base)) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `mkdirat(dirfd, path, mode)` - mode is ignored.
pub(super) fn mkdirat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let (parent, name) = match crate::vfs::split_parent(&path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent, Some(base)) {
        Ok(d) => d,
        Err(e) => return err_of(e),
    };
    match parent.create(name, NodeKind::Dir) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)`.
pub(super) fn renameat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let old_base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let old_path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let new_base = match base_of(args[3] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let new_path = match cstr_arg(args[4], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // No atomic rename primitive underneath: copy the bytes, then drop the
    // old name. A crash in between leaves both names, never none.
    let data = match crate::vfs::read_file_opt(&old_path, Some(old_base.clone())) {
        Ok(d) => d,
        Err(e) => return err_of(e),
    };
    let (parent_dir, name) = match new_path.rsplit_once('/') {
        Some((parent, name)) => (if parent.is_empty() { "/" } else { parent }, name),
        None => ("/", new_path.as_str()),
    };
    let dir = match crate::vfs::resolve_opt(parent_dir, Some(new_base.clone())) {
        Ok(d) => d,
        Err(e) => return err_of(e),
    };
    let inode = match dir.create(name, NodeKind::File) {
        Ok(inode) => inode,
        // Renaming onto an existing file replaces it: resolve and overwrite.
        Err(crate::vfs::VfsError::Exists) => {
            match crate::vfs::resolve_opt(&new_path, Some(new_base)) {
                Ok(inode) => inode,
                Err(e) => return err_of(e),
            }
        }
        Err(e) => return err_of(e),
    };
    if crate::vfs::File::new(inode).write(&data).is_err() {
        return err::ENOSPC;
    }
    let _ = crate::vfs::unlink_opt(&old_path, Some(old_base));
    0
}

/// `readlink(path, buf, size)` - what a symlink points at, and on mpos that
/// is exactly one name: `/proc/self/exe`, the image the process came from.
pub(super) fn readlink(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (buf, size) = (args[1], args[2]);
    if size == 0 {
        return err::EINVAL;
    }
    if !buffer_ok(buf, size, true, true) {
        return err::EFAULT;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let target: String = if path == "/proc/self/exe" {
        crate::sched::current_exe()
    } else {
        // The VFS has no symlinks, so any other existing path is a valid
        // answer to "not a symlink", and a missing one answers ENOENT.
        match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
            Ok(_) => return err::EINVAL,
            Err(e) => return err_of(e),
        }
    };
    let n = target.len().min(size);
    unsafe {
        core::ptr::copy_nonoverlapping(target.as_ptr(), buf as *mut u8, n);
    }
    n as isize
}

/// `fcntl(fd, cmd, arg)` - descriptor flags are stored, the rest is a stub.
///
/// `F_GETFD`/`F_SETFD` round-trip through the fd table: startup code probes
/// them and reads back what it wrote. `F_GETFL` answers `O_RDONLY` - opens
/// here are untyped - and `F_SETFL` accepts and forgets.
pub(super) fn fcntl(args: [usize; ARG_COUNT]) -> isize {
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const FD_CLOEXEC: u32 = 1;

    let (fd, cmd, arg) = (args[0] as i32, args[1], args[2]);
    match cmd {
        F_GETFD => match crate::sched::current_fds().fd_flags_get(fd) {
            Ok(flags) => flags as isize,
            Err(e) => err_of(e),
        },
        F_SETFD => {
            if arg & !(FD_CLOEXEC as usize) != 0 {
                return err::EINVAL;
            }
            match crate::sched::current_fds().fd_flags_set(fd, arg as u32) {
                Ok(()) => 0,
                Err(e) => err_of(e),
            }
        }
        F_GETFL | F_SETFL => 0,
        _ => 0,
    }
}

/// One `struct pollfd` on x86_64: `fd`, `events`, then `revents`.
const POLLFD_SIZE: usize = 8;
/// Where `revents` sits inside a `struct pollfd`.
const POLLFD_REVENTS: usize = 6;

/// `poll(fds, nfds, timeout)`.
///
/// Nothing here ever waits, and nothing is ever reported ready: mpos has no
/// wakeup from a descriptor to a sleeping poller, and every caller in the
/// strace asks with `events = 0, timeout = 0` - the sanitizing probe Rust
/// std runs over fds 0-2 at startup.
///
/// The array is nonetheless *written*: `revents` is an out-parameter, and a
/// caller that seeded it with a sentinel must not read its own bytes back.
/// An unimplemented event is reported as "not ready", which is the honest
/// answer for a kernel that cannot deliver the event yet.
pub(super) fn poll(args: [usize; ARG_COUNT]) -> isize {
    let (fds, nfds) = (args[0], args[1]);
    if nfds == 0 {
        return 0;
    }
    let len = match nfds.checked_mul(POLLFD_SIZE) {
        Some(len) => len,
        None => return err::EINVAL,
    };
    if !buffer_ok(fds, len, true, true) {
        return err::EFAULT;
    }
    for index in 0..nfds {
        let revents = (fds + index * POLLFD_SIZE + POLLFD_REVENTS) as *mut i16;
        unsafe { revents.write_volatile(0) };
    }
    0
}

/// Write the 144-byte Linux x86_64 `struct stat` for `meta`.
///
/// The field offsets are the ABI, so they are spelled as the byte offsets of
/// the struct rather than as indices into a `u64` view - an off-by-one word
/// here is invisible to every caller that only reads `st_mode`, and silently
/// answers "empty file" to the ones that ask for `st_size`.
fn write_stat(buf: usize, meta: &Metadata) -> isize {
    if !buffer_ok(buf, 144, true, true) {
        return err::EFAULT;
    }
    let nlink: u64 = if meta.kind == NodeKind::Dir { 2 } else { 1 };
    let size = meta.size;
    let blocks: u64 = size.div_ceil(512);

    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 144);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u64).write_volatile(meta.ino); // st_dev (the one device there is)
        (at(8) as *mut u64).write_volatile(meta.ino); // st_ino
        (at(16) as *mut u64).write_volatile(nlink); // st_nlink
        (at(24) as *mut u32).write_volatile(mode_of(meta)); // st_mode
        // st_uid, st_gid, the pad word and st_rdev stay 0: one user, no
        // devices named here.
        (at(48) as *mut u64).write_volatile(size); // st_size
        (at(56) as *mut u64).write_volatile(512); // st_blksize
        (at(64) as *mut u64).write_volatile(blocks); // st_blocks
        // Timestamps (offsets 72..120) stay 0: no clock is kept per inode.
    }
    0
}
