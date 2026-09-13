//! Linux-ABI file handlers: the path-carrying calls in Linux argument shapes.
//!
//! Linux passes paths as NUL-terminated strings - one pointer, no length
//! word - and many calls lead with a dirfd the path is relative to. That is
//! a different register layout from the mpos ABI's `(ptr, len)` pairs, so
//! these handlers decode the Linux shapes and speak to the VFS directly;
//! the mpos handlers in [`super::super::fs`] keep their own callers.

use alloc::string::String;
use alloc::sync::Arc;

use crate::syscall::{ARG_COUNT, MAX_IO, buffer_ok, cstr_arg, err, err_of};
use crate::vfs::{Inode, Metadata, NodeKind};

/// `openat`'s magic dirfd: the path is relative to the caller's cwd.
pub(super) const AT_FDCWD: i32 = -100;
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

/// O_CREAT: make the named file when it is not there already. With O_EXCL
/// it refuses instead of replacing; on its own it opens whatever is there.
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o400;

/// `open`/`openat` shared tail: open the path, and on a miss with `O_CREAT`
/// make the file first, then open it.
fn open_linux(path: &str, flags: usize, base: Arc<dyn Inode>) -> isize {
    // O_EXCL refuses to open a name that is already there, whatever it names.
    if flags & O_CREAT != 0
        && flags & O_EXCL != 0
        && crate::vfs::resolve_opt(path, Some(base.clone())).is_ok()
    {
        return err::EEXIST;
    }
    match crate::vfs::open_opt(path, Some(base.clone())) {
        Ok(fd) => fd as isize,
        Err(crate::vfs::VfsError::NoEntry) if flags & O_CREAT != 0 => {
            // O_EXCL names an existing file EEXIST; a missing directory, no
            // space: the create error is the answer, not a retry.
            if let Err(e) = crate::vfs::create_file_opt(path, Some(base.clone())) {
                return err_of(e);
            }
            match crate::vfs::open_opt(path, Some(base)) {
                Ok(fd) => fd as isize,
                Err(e) => err_of(e),
            }
        }
        Err(e) => err_of(e),
    }
}

/// `open(path, flags, mode)` - the mode is ignored, as for every open here:
/// opens are read/write-capable and nothing is inherited across an exec that
/// does not happen.
pub(super) fn open(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    open_linux(&path, args[1], cwd.clone())
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
    open_linux(&path, args[2], base)
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
    // An empty path names the descriptor, but only when the caller said so:
    // Linux requires AT_EMPTY_PATH and answers ENOENT for the unannounced form.
    let meta = if path.is_empty() {
        if _flags & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
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
///
/// Relative components are normalized against the task's cwd *path* and then
/// resolved absolutely: `..` above the cwd needs the way the cwd was named,
/// which the inode walk alone cannot answer (it has no parent edges).
pub(super) fn chdir(args: [usize; ARG_COUNT]) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let canonical = normalize_path(&path, &crate::sched::current_cwd_path());
    match crate::vfs::resolve(&canonical) {
        Ok(dir) if dir.metadata().kind == NodeKind::Dir => {
            adopt_cwd(dir, canonical);
            0
        }
        Ok(_) => err::ENOTDIR,
        Err(e) => err_of(e),
    }
}

/// The normalized absolute path `path` names, relative components resolved
/// against `base`. `.` drops out, `..` pops one component, repeated slashes
/// collapse — the shape `getcwd` owes and `chdir` remembers.
fn normalize_path(path: &str, base: &str) -> alloc::string::String {
    let joined = if path.starts_with('/') {
        alloc::string::String::from(path)
    } else if base.ends_with('/') {
        alloc::format!("{base}{path}")
    } else {
        alloc::format!("{base}/{path}")
    };
    let mut parts: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for component in joined.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return alloc::string::String::from("/");
    }
    let mut out = alloc::string::String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    out
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
///
/// `RENAME_NOREPLACE` refuses to take a name that is already taken; the
/// exchange and whiteout forms have no implementation underneath and answer
/// `EINVAL`, as Linux does for a flag combination it cannot serve.
pub(super) fn renameat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const RENAME_NOREPLACE: usize = 0x1;
    const RENAME_EXCHANGE: usize = 0x2;
    const RENAME_WHITEOUT: usize = 0x4;

    let flags = args[4];
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
        || flags & (RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
    {
        return err::EINVAL;
    }
    let old_base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let old_path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let new_base = match base_of(args[2] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let new_path = match cstr_arg(args[3], true) {
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
        // Renaming onto an existing file replaces it - unless the caller
        // asked for NOREPLACE, which is the one flag that protects the name.
        Err(crate::vfs::VfsError::Exists) => {
            if flags & RENAME_NOREPLACE != 0 {
                return err::EEXIST;
            }
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
/// here are untyped - and `F_SETFL` accepts and forgets. A command Linux has
/// no meaning for answers `EINVAL`, not success.
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
        _ => err::EINVAL,
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

// ---------------------------------------------------------------------------
// File-descriptor calls: the syscalls whose argument is an fd, not a path.
// These are the same shape every libc hands its wrappers: (fd, ...) and
// nothing more.
// ---------------------------------------------------------------------------

/// `read(2)` — read up to `buf.len()` bytes from `fd` into `buf`.
pub(super) fn handle_read(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !buffer_ok(buf, len, true, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if len == 0 {
        return match crate::vfs::read(fd as i32, &mut []) {
            Ok(_) => 0,
            Err(e) => err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match crate::vfs::read(fd as i32, bytes) {
        Ok(n) => n as isize,
        Err(e) => err_of(e),
    }
}

/// `write(2)` — write `buf` to `fd`.
pub(super) fn handle_write(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !buffer_ok(buf, len, false, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if len == 0 {
        return match crate::vfs::write(fd as i32, &[]) {
            Ok(_) => 0,
            Err(e) => err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
    match crate::vfs::write(fd as i32, bytes) {
        Ok(n) => n as isize,
        Err(e) => err_of(e),
    }
}

/// `close(2)` — close `fd`.
pub(super) fn handle_close(args: [usize; ARG_COUNT]) -> isize {
    match crate::vfs::close(args[0] as i32) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `lseek(fd, offset, whence)` — move the file position.
///
/// `whence` is `SEEK_SET` (0), `SEEK_CUR` (1), `SEEK_END` (2). Anything
/// else, or a position that would land before byte 0, is `EINVAL` rather
/// than wrapping — the same refusal the man page spells out.
pub(super) fn handle_lseek(args: [usize; ARG_COUNT]) -> isize {
    let (fd, offset, whence) = (args[0] as i32, args[1] as i64, args[2] as i32);
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let cur = file.pos() as i64;
    let size = file.metadata().size as i64;
    let new_pos = match whence {
        0 => Some(offset),
        1 => cur.checked_add(offset),
        2 => size.checked_add(offset),
        _ => None,
    };
    match new_pos {
        Some(pos) if pos >= 0 => {
            file.set_pos(pos as u64);
            pos as isize
        }
        _ => err::EINVAL,
    }
}

/// `getdents64(fd, buf, len)` — Linux directory listing format.
pub(super) fn handle_getdents64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, buf_len) = (args[0] as i32, args[1], args[2]);
    if !buffer_ok(buf, buf_len, true, from_user) {
        return err::EFAULT;
    }

    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let entries = match file.inode().read_dir() {
        Ok(e) => e,
        Err(e) => return err_of(e),
    };
    let start = file.pos() as usize;
    if start >= entries.len() {
        return 0;
    }

    // Linux getdents64 format:
    //   u64 d_ino, u64 d_off, u16 d_reclen, u8 d_type, u8 d_name[NUL] (8-aligned).
    let mut offset = 0usize;
    for (idx, (name, kind)) in entries.iter().enumerate().skip(start) {
        let reclen = (19 + name.len() + 1 + 7) & !7;
        if offset + reclen > buf_len {
            if offset == 0 {
                return err::EINVAL;
            }
            break;
        }
        let d_type = match kind {
            crate::vfs::NodeKind::File => 8,
            crate::vfs::NodeKind::Dir => 4,
            crate::vfs::NodeKind::CharDev => 2,
        };
        unsafe {
            let base = (buf + offset) as *mut u8;
            (base as *mut u64).write_volatile(idx as u64 + 1);
            ((base.add(8)) as *mut u64).write_volatile((idx + 1) as u64);
            ((base.add(16)) as *mut u16).write_volatile(reclen as u16);
            base.add(18).write_volatile(d_type);
            let name_bytes = name.as_bytes();
            core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), base.add(19), name_bytes.len());
            base.add(19 + name_bytes.len()).write_volatile(0);
        }
        offset += reclen;
        file.set_pos((idx + 1) as u64);
    }
    offset as isize
}

/// `dup(oldfd)` — duplicate onto the lowest free fd.
pub(super) fn handle_dup(args: [usize; ARG_COUNT]) -> isize {
    let old = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    crate::sched::current_fds().install(file) as isize
}

/// `dup2(oldfd, newfd)` — make `newfd` name what `oldfd` names.
pub(super) fn handle_dup2(args: [usize; ARG_COUNT]) -> isize {
    let (old, new) = (args[0] as i32, args[1] as i32);
    if new < 0 {
        return err::EBADF;
    }
    match crate::vfs::dup2(old, new) {
        Ok(fd) => fd as isize,
        Err(e) => err_of(e),
    }
}

/// `dup3(oldfd, newfd, flags)` — `dup2` with `O_CLOEXEC`.
///
/// `old == new` is `EINVAL`, the only flag accepted is `O_CLOEXEC`, anything
/// else is `EINVAL`.
pub(super) fn handle_dup3(args: [usize; ARG_COUNT]) -> isize {
    const O_CLOEXEC: i32 = 0o200_0000;
    let (old, new, flags) = (args[0] as i32, args[1] as i32, args[2] as i32);
    if old == new || flags & !O_CLOEXEC != 0 {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    match crate::sched::current_fds().install_at(file, new) {
        Ok(()) => new as isize,
        Err(e) => err_of(e),
    }
}

/// `pipe(fds)` — write `[read_end, write_end]` into `fds`.
pub(super) fn handle_pipe(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let out = args[0];
    if out == 0 || !buffer_ok(out, 2 * core::mem::size_of::<i32>(), true, from_user) {
        return err::EFAULT;
    }
    let (read_end, write_end) = crate::vfs::pipe::pair();
    let fds = [
        crate::vfs::file::open_inode(read_end),
        crate::vfs::file::open_inode(write_end),
    ];
    unsafe {
        let slot = out as *mut i32;
        slot.write(fds[0]);
        slot.add(1).write(fds[1]);
    }
    0
}

/// `getcwd(buf, size)` — write cwd path into buffer.
pub(super) fn handle_getcwd(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (buf, size) = (args[0], args[1]);
    if !buffer_ok(buf, size, true, from_user) || size < 2 {
        return err::EINVAL;
    }
    let path = crate::sched::current_cwd_path();
    if path.len() + 1 > size {
        return err::ERANGE;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, path.len());
        *((buf + path.len()) as *mut u8) = 0;
    }
    buf as isize
}

/// `fchdir(fd)` — change cwd to the directory the descriptor names.
pub(super) fn handle_fchdir(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    if file.metadata().kind != crate::vfs::NodeKind::Dir {
        return err::ENOTDIR;
    }
    let inode = file.inode();
    let path = path_of(&inode).unwrap_or_else(|| alloc::string::String::from("/"));
    adopt_cwd(inode, path);
    0
}

/// `ioctl(fd, request, arg)` — terminal queries, stub the rest.
pub(super) fn handle_ioctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (_fd, request) = (args[0] as i32, args[1] as u32);
    match request {
        0x5413 => {
            // TIOCGWINSZ: report 80x25.
            let buf = args[2];
            if buffer_ok(buf, 8, true, from_user) {
                unsafe {
                    (buf as *mut u16).write_volatile(25);
                    (buf as *mut u16).add(1).write_volatile(80);
                    (buf as *mut u16).add(2).write_volatile(0);
                    (buf as *mut u16).add(3).write_volatile(0);
                }
                0
            } else {
                err::EFAULT
            }
        }
        0x5401 => {
            // TCGETS: minimal termios, all zeroed.
            let buf = args[2];
            if buffer_ok(buf, 60, true, from_user) {
                unsafe { core::ptr::write_bytes(buf as *mut u8, 0, 60) };
                0
            } else {
                err::EFAULT
            }
        }
        0x5402 => 0, // TCSETS
        _ => err::ENOTTY,
    }
}

/// `writev(fd, iov, iovcnt)` — scatter write.
pub(super) fn handle_writev(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16;
    if !buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
        return err::EFAULT;
    }

    let mut total: isize = 0;
    for i in 0..iov_cnt {
        let base = iov_ptr + i * iov_size;
        let (ptr, len): (usize, usize) = unsafe {
            (
                (base as *const usize).read(),
                ((base + 8) as *const usize).read(),
            )
        };
        if len == 0 {
            continue;
        }
        if !buffer_ok(ptr, len, false, from_user) {
            return err::EFAULT;
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        match crate::vfs::write(fd, bytes) {
            Ok(n) => total += n as isize,
            Err(e) => {
                if total > 0 {
                    return total;
                }
                return err_of(e);
            }
        }
    }
    total
}

/// `readv(fd, iov, iovcnt)` — scatter read.
pub(super) fn handle_readv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16;
    if !buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
        return err::EFAULT;
    }

    let mut total: isize = 0;
    for i in 0..iov_cnt {
        let base = iov_ptr + i * iov_size;
        let (ptr, len): (usize, usize) = unsafe {
            (
                (base as *const usize).read(),
                ((base + 8) as *const usize).read(),
            )
        };
        if len == 0 {
            continue;
        }
        if !buffer_ok(ptr, len, true, from_user) {
            return err::EFAULT;
        }
        let bytes = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) };
        match crate::vfs::read(fd, bytes) {
            Ok(n) => {
                total += n as isize;
                if n < len {
                    break;
                }
            }
            Err(e) => {
                if total > 0 {
                    return total;
                }
                return err_of(e);
            }
        }
    }
    total
}

/// Walk `/` looking for the path that reaches `target`. Used by `fchdir` to
/// give the new cwd a name `getcwd` can read back.
fn path_of(target: &Arc<dyn Inode>) -> Option<alloc::string::String> {
    use crate::vfs::NodeKind;
    fn walk(
        dir: &Arc<dyn Inode>,
        prefix: &str,
        target: &Arc<dyn Inode>,
        depth: usize,
    ) -> Option<alloc::string::String> {
        if Arc::ptr_eq(dir, target) {
            return Some(alloc::string::String::from(prefix));
        }
        if depth == 0 {
            return None;
        }
        for (name, kind) in dir.read_dir().ok()? {
            if kind != NodeKind::Dir {
                continue;
            }
            let child = if prefix == "/" {
                alloc::format!("/{name}")
            } else {
                alloc::format!("{prefix}/{name}")
            };
            if let Ok(inode) = crate::vfs::resolve(&child)
                && let Some(found) = walk(&inode, &child, target, depth - 1)
            {
                return Some(found);
            }
        }
        None
    }
    let root = crate::vfs::global().root_inode();
    walk(&root, "/", target, 16)
}

/// Record the cwd inode + path together, so `getcwd` reads back the path
/// the user wrote.
fn adopt_cwd(dir: Arc<dyn Inode>, path: alloc::string::String) {
    crate::sched::set_cwd(dir);
    crate::sched::set_cwd_path(path);
}
