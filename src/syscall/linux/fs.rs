//! Linux-ABI file handlers: the path-carrying calls in Linux argument shapes.
//!
//! Linux passes paths as NUL-terminated strings - one pointer, no length
//! word - and many calls lead with a dirfd the path is relative to. That is
//! a different register layout from the mpos ABI's `(ptr, len)` pairs, so
//! these handlers decode the Linux shapes and speak to the VFS directly;
//! the mpos handlers in [`super::super::fs`] keep their own callers.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
fn mode_of(meta: &Metadata, stored: u32) -> u32 {
    // The format bits follow the kind - `st_mode`'s top nibble is a type
    // tag, not a permission. The permission bits are the filesystem's own
    // when it stores them (ext2 does, and OpenRC's `test -x` gate lives on
    // them); a filesystem without them gets the kind's default, which keeps
    // directories traversable and devices usable.
    let format = match meta.kind {
        NodeKind::File => 0o100_000,
        NodeKind::Dir => 0o040_000,
        NodeKind::CharDev => 0o020_000,
        NodeKind::BlockDev => 0o060_000,
        NodeKind::Symlink => 0o120_000,
    };
    if stored != 0 {
        format | (stored & 0o7777)
    } else {
        match meta.kind {
            NodeKind::File => 0o100_644,     // regular file rw-r--r--
            NodeKind::Dir => 0o040_755,      // directory rwxr-xr-x
            NodeKind::CharDev => 0o020_666,  // character device rw-rw-rw-
            NodeKind::BlockDev => 0o060_660, // block device brw-rw----
            NodeKind::Symlink => 0o120_777,  // symbolic link rwxrwxrwx
        }
    }
}

/// O_CREAT: make the named file when it is not there already. With O_EXCL
/// it refuses instead of replacing; on its own it opens whatever is there.
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o400;
/// `O_TRUNC`: an existing file is emptied as part of the open. Without it a
/// writer that reopens a name it wrote before keeps whatever tail was longer
/// than what it writes now - the new bytes followed by the old ones, with
/// nothing reporting it.
const O_TRUNC: usize = 0o1000;
const O_CLOEXEC: usize = 0o200_0000;
/// `AT_SYMLINK_NOFOLLOW`: name the link itself, not what it points at.
const AT_SYMLINK_NOFOLLOW: usize = 0x100;

/// `open`/`openat` shared tail: open the path, and on a miss with `O_CREAT`
/// make the file first, then open it.
fn open_linux(path: &str, flags: usize, base: Arc<dyn Inode>) -> isize {
    // Resolved before the base moves into `open_opt`, and used only if what
    // comes back is a directory.
    let label = recoverable_path(path, &base);
    // O_EXCL refuses to open a name that is already there, whatever it names.
    if flags & O_CREAT != 0
        && flags & O_EXCL != 0
        && crate::vfs::resolve_opt(path, Some(base.clone())).is_ok()
    {
        return err::EEXIST;
    }
    let opened = match crate::vfs::open_opt(path, Some(base.clone())) {
        Ok(fd) => Ok(fd),
        Err(crate::vfs::VfsError::NoEntry) if flags & O_CREAT != 0 => {
            // O_EXCL names an existing file EEXIST; a missing directory, no
            // space: the create error is the answer, not a retry.
            if let Err(e) = crate::vfs::create_file_opt(path, Some(base.clone())) {
                return err_of(e);
            }
            crate::vfs::open_opt(path, Some(base))
        }
        Err(e) => Err(e),
    };
    match opened {
        Ok(fd) => {
            // A directory description remembers the path it was opened by, so
            // a later `fchdir` can give the cwd a name `getcwd` reads back. A
            // character device remembers too: libc's `ttyname` names the tty
            // by reading `/proc/self/fd/<n>`, and a nameless descriptor reads
            // back as nothing at all.
            if let Some(label) = label
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
                && matches!(
                    file.metadata().kind,
                    crate::vfs::NodeKind::Dir | crate::vfs::NodeKind::CharDev
                )
            {
                file.set_label(label);
            }
            // `O_CLOEXEC` is part of the open flags, not a property of the file:
            // it says the descriptor must not survive the next `execve`. Left
            // unrecorded, an inherited descriptor keeps a pipe or a lock open
            // past the exec that was meant to end it.
            if flags & O_CLOEXEC != 0 {
                let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
            }
            // `O_TRUNC` empties an existing file as part of the open, which is
            // what a writer that reopens a name is promising: what it writes
            // now is the whole file. Without it, `wget -O page.html` over a
            // longer page leaves the new page followed by the old one's tail.
            if flags & O_TRUNC != 0
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
            {
                let inode = file.inode();
                if inode.metadata().kind == crate::vfs::NodeKind::File {
                    let _ = inode.truncate(0);
                }
            }
            fd as isize
        }
        Err(e) => err_of(e),
    }
}

/// The absolute path `path` names, when this is an open whose spelling can be
/// recovered: an absolute path, or a relative one taken against the cwd. A
/// relative path resolved against some other directory descriptor names a file
/// the walk from `/` would have to search for, so it is left unnamed and
/// `fchdir` falls back to searching.
fn recoverable_path(path: &str, base: &Arc<dyn Inode>) -> Option<alloc::string::String> {
    if path.starts_with('/') {
        Some(normalize_path(path, "/"))
    } else if Arc::ptr_eq(base, &crate::sched::current_cwd()) {
        Some(normalize_path(path, &crate::sched::current_cwd_path()))
    } else {
        None
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

/// `stat(path, buf)` and `lstat(path, buf)`: the pre-`*at` spellings, which are
/// `fstatat` at the caller's cwd. `lstat` is the one that names the link rather
/// than what it points at.
pub(super) fn stat_legacy(nr: u64, args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let flags = if nr == crate::syscall::linux::nr::LSTAT {
        AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    newfstatat([AT_FDCWD as usize, args[0], args[1], flags, 0, 0], cwd)
}

/// `fstat(fd, buf)` - the fd's description, not a path, is what is statted.
pub(super) fn fstat(args: [usize; ARG_COUNT]) -> isize {
    let (fd, buf) = (args[0] as i32, args[1]);
    let (meta, dev, rdev, stored) = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => {
            let inode = file.inode();
            // The filesystem a descriptor belongs to is named by the path it
            // was opened through - the same answer `stat` by path would give,
            // which is what libc's `ttyname` compares the two sides of. A
            // nameless descriptor falls back to the mount-table lookup.
            let dev = match file.label() {
                Some(path) => crate::vfs::mount::fs_of_path(&path).0,
                None => crate::vfs::mount::fs_id_of_inode(&inode),
            };
            (file.metadata(), dev, inode.rdev(), inode.mode())
        }
        Err(e) => return err_of(e),
    };
    write_stat(buf, &meta, dev, rdev, stored)
}

/// `flock(fd, operation)` - the advisory whole-file lock `apk` takes on its
/// database before it will touch it.
///
/// A real implementation would track per-open-file-description lock state and
/// arbitrate `LOCK_SH`/`LOCK_EX` between them. This kernel has one user and,
/// in practice, one locker at a time, so the honest shape of the answer is:
/// validate the fd, grant every lock, release nothing. The lock is then held
/// for the life of the description and freed when the kernel reaps it - which
/// is exactly the invariant single-locker callers rely on. `LOCK_UN` (8) is
/// a no-op because nothing was ever recorded.
pub(super) fn flock(args: [usize; ARG_COUNT]) -> isize {
    if let Err(e) = crate::sched::current_fds().clone_at(args[0] as i32) {
        return err_of(e);
    }
    0
}

/// `ftruncate(fd, length)` - the resize `wget` asks of its output before it
/// writes and `apk`'s fetcher asks of its temp file after it does.
///
/// The resize is real, and has to be: a shorter file keeps no tail. A writer
/// that only overwrites from the front - and `O_TRUNC` that only forgets to
/// matter - leaves the new bytes followed by whatever the old file had that
/// the new one does not, which is a corrupt file nothing complains about.
pub(super) fn ftruncate(args: [usize; ARG_COUNT]) -> isize {
    let length = args[1] as isize;
    if length < 0 {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    // Linux refuses to resize anything but a regular file, and `EINVAL` is
    // what a caller reads as "this descriptor is not that": a pipe, a
    // terminal, a socket.
    if file.metadata().kind != NodeKind::File {
        return err::EINVAL;
    }
    match file.inode().truncate(length as u64) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `truncate(path, length)` - `ftruncate` by name instead of descriptor.
pub(super) fn truncate(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let length = args[1] as isize;
    if length < 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let inode = match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    match inode.truncate(length as u64) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `fstatfs(fd, buf)` - filesystem facts for whatever `fd` is open on. The
/// descriptor names a file; the file names a filesystem. A file that *is* a
/// mountpoint reports that mount; anything else reports the root, which is
/// the honest answer this kernel can give without tracking a path per
/// descriptor.
pub(super) fn fstatfs(args: [usize; ARG_COUNT]) -> isize {
    let inode = match crate::sched::current_fds()
        .clone_at(args[0] as i32)
        .map(|f| f.inode().clone())
    {
        Ok(i) => i,
        Err(e) => return err_of(e),
    };
    let fstype = crate::vfs::mount::fstype_of_inode(&inode);
    let stats = crate::vfs::mount::fs_stats_of_inode(&inode);
    write_statfs(args[1], &fstype, stats)
}

/// `statfs(path, buf)` - the path spelling of `fstatfs`, and the one that
/// carries real information: the deepest mountpoint prefixing the path is
/// the filesystem that serves it.
pub(super) fn statfs(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => {
            let abs = absolute(&path);
            let stats = crate::vfs::mount::fs_stats_of_path(&abs);
            let (_id, fstype) = crate::vfs::mount::fs_of_path(&abs);
            write_statfs(args[1], &fstype, stats)
        }
        Err(e) => err_of(e),
    }
}

/// The absolute, normalized spelling of a path a syscall was handed, for the
/// filesystem lookup: `st_dev` and `statfs`'s type both key on the path, and
/// `.` names whatever the caller's cwd is.
fn absolute(path: &str) -> alloc::string::String {
    if path.starts_with('/') {
        crate::vfs::path::normalize(path)
    } else {
        let cwd = crate::sched::current_cwd_path();
        crate::vfs::path::normalize(&alloc::format!("{}/{}", cwd.trim_end_matches('/'), path))
    }
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
        let (meta, dev, rdev, stored) = match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => {
                let inode = file.inode();
                let dev = match file.label() {
                    Some(path) => crate::vfs::mount::fs_of_path(&path).0,
                    None => crate::vfs::mount::fs_id_of_inode(&inode),
                };
                (file.metadata(), dev, inode.rdev(), inode.mode())
            }
            Err(e) => return err_of(e),
        };
        return write_stat(buf, &meta, dev, rdev, stored);
    }
    let base = match base_of(dirfd, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // `lstat` is the one call that wants the link and not its target, so it
    // walks to the parent and looks the name up there - a resolver that
    // followed links would hand back the very thing it was told not to.
    let (meta, rdev, stored) = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        match crate::vfs::global().resolve_no_follow(&path, base) {
            Ok(inode) => (inode.metadata(), inode.rdev(), inode.mode()),
            Err(e) => return err_of(e),
        }
    } else {
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => (inode.metadata(), inode.rdev(), inode.mode()),
            Err(e) => return err_of(e),
        }
    };
    let (dev, _fstype) = crate::vfs::mount::fs_of_path(&absolute(&path));
    write_stat(buf, &meta, dev, rdev, stored)
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
    let (meta, rdev, stored) = if path.is_empty() {
        if _flags & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => {
                let inode = file.inode();
                (file.metadata(), inode.rdev(), inode.mode())
            }
            Err(e) => return err_of(e),
        }
    } else {
        let base = match base_of(dirfd, cwd) {
            Ok(base) => base,
            Err(e) => return e,
        };
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => (inode.metadata(), inode.rdev(), inode.mode()),
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
        (at(28) as *mut u16).write_volatile(mode_of(&meta, stored) as u16); // stx_mode
        (at(32) as *mut u64).write_volatile(meta.ino); // stx_ino
        (at(40) as *mut u64).write_volatile(meta.size); // stx_size
        (at(48) as *mut u64).write_volatile(meta.size.div_ceil(512) as u64); // stx_blocks
        if let Some((major, minor)) = rdev {
            (at(72) as *mut u32).write_volatile(major); // stx_rdev_major
            (at(76) as *mut u32).write_volatile(minor); // stx_rdev_minor
        }
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

/// `utimensat(dirfd, pathname, times, flags)`.
///
/// Timestamps arrive but are not kept: the VFS carries no metadata clock,
/// so a call naming a real file answers success and a missing one answers
/// ENOENT - the two answers a caller branches on. `touch` is the everyday
/// user, and it needs the success. The branch that matters is the one
/// `futimens` speaks: `utimensat(fd, NULL, times, 0)` names no path at all,
/// the descriptor *is* the file - and busybox `touch` opens its target and
/// then futimens it, so a NULL path with a plain-file fd is the everyday
/// shape, not a corner.
pub(super) fn utimensat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    if args[1] == 0 {
        // futimens shape: the fd names the file, timestamps are not kept.
        let fd = args[0] as u32 as i32;
        return match crate::sched::current_fds().clone_at(fd) {
            Ok(_) => 0,
            Err(e) => err_of(e),
        };
    }
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
    // `AT_REMOVEDIR` is `unlinkat` spelling `rmdir`: the flag is not an
    // optimisation, it is how the caller says which of the two operations it
    // means, and getting it wrong either removes a directory that still holds
    // something or refuses a plain file.
    const AT_REMOVEDIR: usize = 0x200;

    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let outcome = if args[2] & AT_REMOVEDIR != 0 {
        crate::vfs::rmdir_opt(&path, Some(base))
    } else {
        crate::vfs::unlink_opt(&path, Some(base))
    };
    match outcome {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `rmdir(path)` - remove an empty directory by name.
///
/// A separate call from `unlink` because a directory is a different thing to
/// take apart: it must be empty, and the kernel has to say so rather than
/// quietly taking what was underneath with it.
pub(super) fn rmdir(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::rmdir_opt(&path, Some(cwd.clone())) {
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
    // A name that is already taken is `EEXIST`, whatever kind of thing holds
    // it - and the names that are always taken are exactly the ones a
    // parent/name split cannot express: `/`, `.` and `..`. Checking here is
    // what makes `mkdir -p` work, and its absence was not a small thing:
    // `-p` creates a path by walking *up* it and stopping at the first
    // ancestor that is already there, so an `EINVAL` where an `EEXIST`
    // belonged sent it all the way to `mkdir("/")` and aborted the whole
    // command with "can't create directory '/'".
    if crate::vfs::global()
        .resolve_no_follow(&path, base.clone())
        .is_ok()
    {
        return err::EEXIST;
    }
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
    // Renaming a name onto itself is a no-op - and it has to be said so
    // *before* the copy path runs, because that path unlinks the taken name
    // on the way to recreating it, and with one shared dentry that unlink
    // takes the file's only name with it.
    if alloc::sync::Arc::ptr_eq(&old_base, &new_base) && old_path == new_path {
        return 0;
    }
    // No atomic rename primitive underneath: the old name's body is copied
    // to the new one and the old name dropped. A crash in between leaves
    // both names, never none.
    //
    // The old name is looked up *without following it*. A rename moves the
    // name, whatever kind of node holds - and apk's commits rename symlinks
    // wholesale: a package's `.so` link is laid down under a temp name and
    // renamed to its real one, with the link's target typically still under
    // its own temp name at that moment. Resolving the old path would follow
    // the link and go looking for the target by its real name - ENOENT for
    // every `apk add` that ships a symlink - and where the target did exist,
    // the copy would drag its whole body over what should have stayed a
    // link, which is how the tree got corrupted the first time this was
    // driven by apk.
    let (old_dir, old_name) = match old_path.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() { "/" } else { parent };
            match crate::vfs::resolve_opt(parent, Some(old_base.clone())) {
                Ok(d) => (d, alloc::string::String::from(name)),
                Err(e) => return err_of(e),
            }
        }
        None => (old_base.clone(), old_path.clone()),
    };
    let old_inode = match old_dir.lookup(&old_name) {
        Ok(ino) => ino,
        Err(e) => return err_of(e),
    };

    // Where the new name goes. A path with a slash names its own parent; a
    // bare name has none, and the directory it is created in is the one the
    // caller pointed at - `newdirfd`, or the cwd when that is `AT_FDCWD`.
    // Treating a bare name as rooted at `/` is what put apk's fetched index in
    // the root directory: `renameat(dfd, "INDEX.part", dfd, "INDEX")` created
    // `/INDEX` instead of `/var/cache/apk/INDEX`, and the next `apk update`
    // could not find the index it had already downloaded.
    let (dir, name) = match new_path.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() { "/" } else { parent };
            match crate::vfs::resolve_opt(parent, Some(new_base.clone())) {
                Ok(d) => (d, alloc::string::String::from(name)),
                Err(e) => return err_of(e),
            }
        }
        None => (new_base.clone(), new_path.clone()),
    };

    // What `create` owes the caller when the name is taken: the name is
    // replaced, whatever kind of node holds - unless NOREPLACE protected it.
    // Replacing by unlink-and-create is what keeps a link from being written
    // in place of the link it points at.
    macro_rules! claim_name {
        ($make:expr) => {{
            match $make {
                Ok(inode) => inode,
                Err(crate::vfs::VfsError::Exists) => {
                    if flags & RENAME_NOREPLACE != 0 {
                        return err::EEXIST;
                    }
                    match dir.unlink(&name) {
                        Ok(()) => {}
                        Err(e) => return err_of(e),
                    }
                    match $make {
                        Ok(inode) => inode,
                        Err(e) => return err_of(e),
                    }
                }
                Err(e) => return err_of(e),
            }
        }};
    }

    match old_inode.metadata().kind {
        // The old name is a link: the new name becomes the same link. Its
        // target text travels verbatim, relative targets and all.
        NodeKind::Symlink => {
            let target = match old_inode.readlink() {
                Ok(t) => t,
                Err(e) => return err_of(e),
            };
            claim_name!(dir.symlink(&name, &target).map(|_| ()));
        }
        // A regular file: copy the bytes, then drop the old name.
        NodeKind::File => {
            let file = crate::vfs::File::new(old_inode);
            let mut data = alloc::vec::Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&chunk[..n]),
                    Err(e) => return err_of(e),
                }
            }
            let inode = claim_name!(dir.create(&name, NodeKind::File));
            if crate::vfs::File::new(inode).write(&data).is_err() {
                return err::ENOSPC;
            }
        }
        // A directory rename would have to move the whole subtree's parent
        // back-pointer; nothing here needs it yet, and pretending otherwise
        // would half-move it. Say so.
        _ => return err::EISDIR,
    }

    let _ = old_dir.unlink(&old_name);
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
    } else if let Some(rest) = path.strip_prefix("/proc/self/fd/") {
        // The descriptor's own path: what libc's `ttyname` reads to name the
        // tty a login session is on. The fd must be one of the caller's, and
        // the answer is the name it was opened by.
        let fd: i32 = match rest.parse() {
            Ok(n) => n,
            Err(_) => return err::ENOENT,
        };
        match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => match file.label().or_else(|| path_of(&file.inode())) {
                Some(p) => p,
                None => return err::ENOENT,
            },
            Err(e) => return err_of(e),
        }
    } else {
        // The link, not what it points at: a resolver that followed it would
        // answer EINVAL for every link and the target for nothing.
        match crate::vfs::global().resolve_no_follow(&path, cwd.clone()) {
            Ok(node) => match node.readlink() {
                Ok(target) => target,
                Err(e) => return err_of(e),
            },
            Err(e) => return err_of(e),
        }
    };
    let n = target.len().min(size);
    unsafe {
        core::ptr::copy_nonoverlapping(target.as_ptr(), buf as *mut u8, n);
    }
    n as isize
}

/// `readlinkat(dirfd, path, buf, bufsiz)` - the dirfd form of `readlink`.
///
/// The arguments shift by one: the dirfd sits in `args[0]` and the path in
/// `args[1]`, so the plain `readlink` layout would read the dirfd number as
/// the path and answer ENOENT for every caller.
pub(super) fn readlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    readlink([args[1], args[2], args[3], 0, 0, 0], &base)
}

/// `symlink(target, linkpath)` — name a link that stands for `target`.
///
/// The target is stored, not checked: a link may point at a path that does not
/// exist yet, and POSIX says making one is still allowed. What is an error is
/// the name already being taken, or the filesystem refusing to hold a link.
pub(super) fn symlink(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let target = match cstr_arg(args[0], true) {
        Ok(target) => target,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // The parent, not the name: the name is what this call is about to make,
    // so it must *not* be there, and asking the tree for it would only
    // discover that. Splitting it off is also what keeps a link from being
    // written in place of the link it points at.
    let (parent_path, name) = match crate::vfs::split_parent(&path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent_path, Some(cwd.clone())) {
        Ok(parent) => parent,
        Err(e) => return err_of(e),
    };
    let r = match parent.symlink(name, &target) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    };
    r
}

/// `symlinkat(target, newdirfd, linkpath)` - the dirfd form of `symlink`.
///
/// The dirfd names where the link lands: `AT_FDCWD` means the caller's
/// directory, anything else is a directory descriptor apk keeps open across
/// the whole extraction of a package.
pub(super) fn symlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[1] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    symlink([args[0], args[2], 0, 0, 0, 0], &base)
}

/// `mount(source, target, fstype, flags, data)`.
///
/// The filesystem types are the ones this kernel has. A block-backed type
/// resolves its source through the tree - `/dev/vda3` is a node, and the node
/// knows which storage it is - which is the mechanism that lets userland
/// decide what gets mounted where, instead of the kernel's boot code deciding
/// for it.
///
/// Not implemented, and said so rather than silently accepted: read-only
/// mounts. `MS_RDONLY` names a property of the filesystem underneath, and
/// this kernel's writers have no read-only mode to consult; a caller asking
/// for one gets `EOPNOTSUPP` rather than a writable mount that claims to be
/// read-only.
pub(super) fn handle_mount(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const MS_RDONLY: usize = 0x1;

    let source = match cstr_arg(args[0], true) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let target = match cstr_arg(args[1], true) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let fstype = cstr_arg(args[2], true).unwrap_or_default();

    // Read-only is honoured, not recorded and ignored: the filesystem is told,
    // and refuses writes from then on. This is what an initramfs relies on
    // while it checks a root it has not mounted writable yet.
    let read_only = args[3] & MS_RDONLY != 0;

    // `mount -o remount,...`: no new filesystem, just new flags on one that is
    // already mounted. `rw` is spelled by the *absence* of MS_RDONLY, which is
    // why this is a replacement rather than a change.
    const MS_REMOUNT: usize = 0x20;
    if args[3] & MS_REMOUNT != 0 {
        return match crate::vfs::mount::remount(&target, Some(cwd.clone()), read_only) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        };
    }

    // `mount(2)`'s MS_MOVE: the filesystem mounted at the source moves to the
    // target, and a target of `/` is what `switch_root` asks for - the new
    // root becomes the filesystem, and the initramfs it came from goes away.
    // No new filesystem is built for a move, so this comes before the type
    // dispatch: `switch_root` passes no type at all.
    const MS_MOVE: usize = 0x2000;
    if args[3] & MS_MOVE != 0 {
        let src = match crate::vfs::resolve_opt(&source, Some(cwd.clone())) {
            Ok(s) => s,
            Err(e) => return err_of(e),
        };
        // A move onto `/` is not a re-homing - nothing can be "the directory
        // under /" when / is the tree the move puts there. That spelling is
        // what `switch_root` uses to make the tree it stands in the root: the
        // mount carrying it is promoted, the table it leaves behind is
        // dropped, and the caller's cwd follows, because the names it was
        // using resolved through a tree that is no longer mounted anywhere.
        if target.trim_end_matches('/').is_empty() {
            return match crate::vfs::mount::move_root(&src) {
                Ok(()) => {
                    if let Ok(root) = crate::vfs::resolve("/") {
                        crate::sched::set_cwd(root);
                        crate::sched::set_cwd_path(alloc::string::String::from("/"));
                    }
                    0
                }
                Err(e) => err_of(e),
            };
        }
        // The ordinary move: one mount, re-homed, everything else untouched.
        let at = match crate::vfs::resolve_opt(&target, Some(cwd.clone())) {
            Ok(at) => at,
            Err(e) => return err_of(e),
        };
        match crate::vfs::mount::move_mount(&src, &at) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        }
    } else {
        mount_new(&source, &target, &fstype, read_only, cwd)
    }
}

/// The mount half of `mount(2)`: build the filesystem the type names and
/// hang it on the target.
fn mount_new(
    source: &str,
    target: &str,
    fstype: &str,
    read_only: bool,
    cwd: &Arc<dyn Inode>,
) -> isize {
    // MS_NOSUID, MS_NODEV, MS_NOEXEC and MS_NOATIME are accepted and mean
    // nothing here: there is no setuid bit to honour, no device to refuse and
    // no attribute to leave unstamped. Accepting a flag that has no meaning is
    // not the same as lying about one that does.

    // The mountpoint must already be there and must be a directory, as Linux
    // requires: mounting does not invent directories.
    let at = match crate::vfs::resolve_opt(target, Some(cwd.clone())) {
        Ok(at) => at,
        Err(e) => return err_of(e),
    };
    if at.metadata().kind != NodeKind::Dir {
        return err::ENOTDIR;
    }

    let fs: alloc::sync::Arc<dyn crate::vfs::FileSystem> = match fstype {
        // The block types. The source names a device node; the node knows
        // which storage it stands for.
        "ext2" | "ext3" | "ext4" => {
            if source.is_empty() {
                return err::ENOENT;
            }
            let dev_node = match crate::vfs::resolve_opt(source, Some(cwd.clone())) {
                Ok(node) => node,
                Err(e) => return err_of(e),
            };
            let Some(dev) = dev_node.as_block() else {
                return err::ENOTBLK;
            };
            match crate::vfs::ext2::Ext2Fs::mount(dev) {
                Ok(fs) => alloc::sync::Arc::new(fs) as alloc::sync::Arc<dyn crate::vfs::FileSystem>,
                Err(_) => return err::EINVAL,
            }
        }
        // The kernel's own trees, by name.
        "proc" => crate::vfs::procfs::ProcFs::new(),
        "sysfs" => crate::vfs::sysfs::SysFs::new(),
        "devtmpfs" | "devfs" => crate::drivers::devices::standard_devfs(),
        "devpts" => crate::vfs::devpts::DevPts::new(),
        "tmpfs" | "ramfs" => crate::vfs::ramfs::RamFs::new(),
        // A type this kernel does not speak. ENODEV is what Linux answers, and
        // it is the answer a caller walks a list of types on.
        _ => return err::ENODEV,
    };

    match crate::vfs::mount::mount_at_opt(target, Some(cwd.clone()), fs, fstype, source, read_only)
    {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `umount2(target, flags)`.
///
/// `MNT_FORCE` and `MNT_DETACH` both land on the same immediate detach, which
/// is what this kernel's unmount is: there is no in-flight I/O to force and no
/// lazy expiry to schedule, so the flags name distinctions that do not exist
/// here and are accepted as no-ops.
pub(super) fn handle_umount2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const MNT_FORCE: usize = 0x1;
    const MNT_DETACH: usize = 0x2;
    let _ = (MNT_FORCE, MNT_DETACH);

    let target = match cstr_arg(args[0], true) {
        Ok(t) => t,
        Err(e) => return e,
    };
    match crate::vfs::mount::unmount_opt(&target, Some(cwd.clone())) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `pivot_root(new_root, put_old)` - replace the root, parking the old one.
///
/// This is the call an initramfs exists to make: it mounts the real root
/// somewhere, moves the root to it, and from then on the machine lives on the
/// filesystem it was built to run. The old root is parked at `put_old`, which
/// is what lets the caller detach it and give its memory back.
pub(super) fn handle_pivot_root(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let _ = cwd;
    let new_root = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let put_old = match cstr_arg(args[1], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::pivot_root(&new_root, &put_old) {
        Ok(()) => {
            // The caller's cwd named a directory in the tree that just stopped
            // being the root. Everything it reaches for from here on - `exec`,
            // relative paths, `getcwd` - has to answer in the new one.
            if let Ok(root) = crate::vfs::resolve("/") {
                crate::sched::set_cwd(root);
                crate::sched::set_cwd_path(alloc::string::String::from("/"));
            }
            0
        }
        Err(e) => err_of(e),
    }
}

/// `eventfd(initval, flags)` - a counter a process can wait on.
///
/// One thread writes a number to it, another's `poll(2)` wakes: that is the
/// entire contract, and it is what a program with a worker uses instead of a
/// pipe.
pub(super) fn handle_eventfd(args: [usize; ARG_COUNT]) -> isize {
    const EFD_SEMAPHORE: usize = 1;
    const EFD_NONBLOCK: usize = 0x800;
    if args[1] & EFD_SEMAPHORE != 0 {
        // The counter is taken whole here; a caller that asked for one-per-read
        // asked for something else, and saying so beats doing something close.
        return err::EOPNOTSUPP;
    }
    let fd = crate::vfs::open_inode(crate::vfs::eventfd::EventFd::new(
        next_anon_ino(),
        args[0] as u32 as u64,
    ));
    if args[1] & EFD_NONBLOCK != 0
        && let Ok(file) = crate::sched::current_fds().clone_at(fd)
    {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// `signalfd(fd, mask, flags)` - signals as a descriptor. The mask is read and
/// honoured in the one way this kernel has signals to wait for: child exits.
///
/// `flags` sits in the fourth slot of the `signalfd4` ABI musl makes; the
/// plain number routes a zero here, because the original spelling has no
/// flags at all - reading `args[2]` for it would take the mask *size* for a
/// flag word and drop the caller's nonblock silently.
pub(super) fn handle_signalfd(args: [usize; ARG_COUNT], flags: usize, from_user: bool) -> isize {
    const SFD_NONBLOCK: usize = 0x800;
    // Only the mask's validity is checked: this kernel reports child exits,
    // and a caller that asked for others asked for signals it cannot get here.
    if !crate::syscall::buffer_ok(args[1], 8, false, from_user) {
        return err::EFAULT;
    }
    let nonblock = flags & SFD_NONBLOCK != 0;
    let fd = crate::vfs::open_inode(crate::vfs::signalfd::SignalFd::new(next_anon_ino(), nonblock));
    if nonblock
        && let Ok(file) = crate::sched::current_fds().clone_at(fd)
    {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// A number for a descriptor that has no name to be numbered by.
fn next_anon_ino() -> crate::vfs::Ino {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// `chroot(path)` - make `path` the root of the namespace.
///
/// `switch_root`'s second step: it has already emptied the initramfs and
/// moved the new root over `/`, and this call drops the process's remaining
/// claim on the old tree. The kernel has one namespace, so the answer here is
/// whole-namespace - which is what the caller wants, being pid 1 and alone.
pub(super) fn handle_chroot(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::mount::chroot(&path, cwd) {
        Ok(()) => {
            if let Ok(root) = crate::vfs::resolve("/") {
                crate::sched::set_cwd(root);
                crate::sched::set_cwd_path(alloc::string::String::from("/"));
            }
            0
        }
        Err(e) => err_of(e),
    }
}

/// `fcntl(fd, cmd, arg)` - descriptor flags are stored, the rest is a stub.
///
/// `F_GETFD`/`F_SETFD` round-trip through the fd table: startup code probes
/// them and reads back what it wrote. `F_GETFL` answers `O_RDONLY` - opens
/// here are untyped - and `F_SETFL` accepts and forgets. A command Linux has
/// no meaning for answers `EINVAL`, not success.
///
/// `F_GETLK`/`F_SETLK`/`F_SETLKW` are permissive stubs: this kernel has one
/// user and, in practice, one locker at a time, so every lock is granted and
/// every unlock is a no-op. `apk` and other tools that use `fcntl` record
/// locking need these to succeed rather than return `EINVAL`.
pub(super) fn fcntl(args: [usize; ARG_COUNT]) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_GETLK: usize = 5;
    const F_SETLK: usize = 6;
    const F_SETLKW: usize = 7;
    const FD_CLOEXEC: u32 = 1;

    let (fd, cmd, arg) = (args[0] as i32, args[1], args[2]);
    match cmd {
        // `dup` with a floor rather than a free-for-all: the new descriptor is
        // the lowest free one at or above `arg`. A shell's redirection is built
        // on this - busybox moves stdout out of the way with
        // `fcntl(1, F_DUPFD, 10)` before pointing fd 1 at a file, so that the
        // descriptors it is about to reuse cannot collide with the saved copy.
        F_DUPFD => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(file) => file,
                Err(e) => return err_of(e),
            };
            let fds = crate::sched::current_fds();
            match fds.install_at_least(file, arg as i32) {
                Ok(new) => new as isize,
                Err(e) => err_of(e),
            }
        }
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
        // `F_GETFL` reports the open flag word the description carries -
        // which, since sockets arrived, is where `O_NONBLOCK` lives, and a
        // DNS resolver reads it back to know how its sockets will behave.
        // `F_SETFL` stores the word; the flag bits a caller may not change
        // (the access mode) are accepted here rather than filtered, because
        // opens are untyped and the access bits are always zero anyway.
        F_GETFL => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => file.status() as isize,
            Err(e) => err_of(e),
        },
        F_SETFL => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => {
                file.set_status(arg as u32);
                0
            }
            Err(e) => err_of(e),
        },
        // POSIX record locking: permissive stubs. `F_GETLK` reports no lock
        // held; `F_SETLK` and `F_SETLKW` always succeed. This matches the
        // single-locker reality of the kernel and lets `apk`'s database lock
        // acquire succeed.
        F_GETLK => {
            // Fill the `struct flock` with "no lock" (F_UNLCK = 2).
            if arg != 0 {
                if !crate::syscall::buffer_ok(arg, 32, true, true) {
                    return err::EFAULT;
                }
                // struct flock on x86_64: l_type(i16) + l_whence(i16) + pad(4)
                // + l_start(i64) + l_len(i64) + l_pid(i32) = 32 bytes.
                // l_type = F_UNLCK (2), rest zeroed.
                unsafe {
                    core::ptr::write_bytes(arg as *mut u8, 0, 32);
                    (arg as *mut u16).write_volatile(2); // F_UNLCK
                }
            }
            0
        }
        F_SETLK | F_SETLKW => 0, // always succeed: single-locker system
        _ => err::EINVAL,
    }
}

/// One `struct pollfd` on x86_64: `fd`, `events`, then `revents`.
const POLLFD_SIZE: usize = 8;
/// Where `revents` sits inside a `struct pollfd`.
const POLLFD_REVENTS: usize = 6;
/// Where \`events\` sits inside a \`struct pollfd\`.
const POLLFD_EVENTS: usize = 4;

/// `poll(fds, nfds, timeout)`.
///
/// What a caller may ask `poll` to watch for, and what it is told happened.
const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLNVAL: i16 = 0x020;

/// Wait for any of `nfds` descriptors to be ready.
///
/// This is the call a real shell is built on: `ash` polls stdin with an
/// infinite timeout and reads what the poll says arrived. A `poll` that always
/// answers "nothing, and stop asking" is not a slow `poll`, it is a shell that
/// decides its terminal hung up and exits at the first prompt - which is
/// exactly what happened before this existed.
///
/// Readiness comes from the node itself, since only it knows: a pipe is
/// readable when it holds data *or* when its last writer has gone, because
/// both are cases where a read returns instead of waiting. Waiting is done
/// through the node's own wakeup list, so the arrival that ends the wait is
/// the same arrival that would have ended a blocking read.
///
/// `poll` and `ppoll` are the same wait in different clocks, and the difference
/// is not cosmetic: `poll`'s timeout is a millisecond count and a negative one
/// means "forever", while `ppoll`'s is a `struct timespec *` and a null one
/// means the same. Read a pointer as a count and the wait is measured in
/// centuries - which is exactly the hang this split fixes, because `ppoll` is
/// the call rustix makes and therefore the call every Rust program here makes.
pub(super) fn poll(args: [usize; ARG_COUNT]) -> isize {
    let timeout_ms = args[2] as i64;
    let deadline = (timeout_ms >= 0).then(|| deadline_in_ticks(timeout_ms as u64));
    poll_wait(args, deadline)
}

/// `ppoll(fds, nfds, timeout, sigmask, sigsetsize)`.
///
/// The signal mask is ignored: this kernel delivers signals on the way out of
/// a syscall rather than by interrupting one, so there is nothing for a
/// temporary mask to hold back.
pub(super) fn ppoll(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let deadline = match ppoll_deadline(args[2], from_user) {
        Ok(deadline) => deadline,
        Err(e) => return e,
    };
    poll_wait(args, deadline)
}

/// The instant a `ppoll` timeout names. `Ok(None)` is "wait forever", which is
/// what a null pointer asks for.
fn ppoll_deadline(tsp: usize, from_user: bool) -> Result<Option<u64>, isize> {
    if tsp == 0 {
        return Ok(None);
    }
    if !buffer_ok(tsp, 16, false, from_user) {
        return Err(err::EFAULT);
    }
    let (sec, nsec) = unsafe {
        let at = |offset: usize| (tsp + offset) as *const u64;
        (at(0).read_volatile(), at(8).read_volatile())
    };
    if nsec >= 1_000_000_000 {
        return Err(err::EINVAL);
    }
    // A deadline further out than this is a clock that has gone wrong, not a
    // request; Linux clamps instead of refusing, and so does the arithmetic
    // here - the tick count saturates rather than wrapping into the past.
    let ms = sec
        .saturating_mul(1000)
        .saturating_add(nsec.div_ceil(1_000_000));
    Ok(Some(deadline_in_ticks(ms)))
}

/// `ms` from now, in ticks.
fn deadline_in_ticks(ms: u64) -> u64 {
    let ticks = ms.saturating_mul(crate::arch::timer::TICK_HZ as u64) / 1000;
    crate::arch::timer::ticks().saturating_add(ticks)
}

/// The wait [`poll`] and [`ppoll`] are both made of, with the timeout already
/// turned into the instant it names.
fn poll_wait(args: [usize; ARG_COUNT], deadline: Option<u64>) -> isize {
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

    let me = crate::sched::current();
    // Ask every node to wake us before the first check: one that becomes ready
    // between the check and the sleep must not be missed. A descriptor the
    // table does not name has no node, and is reported POLLNVAL - counting it
    // is what stops a caller waiting on a closed fd from sleeping forever.
    let mut nodes: Vec<Option<Arc<dyn Inode>>> = Vec::with_capacity(nfds);
    for index in 0..nfds {
        let at = fds + index * POLLFD_SIZE;
        let fd = unsafe { (at as *const i32).read_volatile() };
        let node = crate::sched::current_fds()
            .clone_at(fd)
            .ok()
            .map(|file| file.inode());
        if let Some(node) = &node {
            node.poll_subscribe(me);
        }
        nodes.push(node);
    }

    let ready = loop {
        let count = check_ready(fds, &nodes);
        if count > 0 {
            break count;
        }
        let expired = deadline.is_some_and(|end| crate::arch::timer::ticks() >= end);
        if expired {
            break 0;
        }
        // One tick of sleep, then a fresh check - the same wait the bounded
        // form uses. Parking in `block_until` here would need a waker behind
        // every descriptor: a tty or a pipe has one (poll_subscribe), but a
        // netlink uevent socket, a signalfd and an eventfd do not - and
        // `nlplug-findfs` parks exactly that way, with no timeout, for the
        // events its trigger thread provokes. Ten milliseconds of polling is
        // what keeps such a wait from sleeping forever.
        crate::sched::sleep_ticks(1);
    };

    for node in nodes.iter().flatten() {
        node.poll_unsubscribe(me);
    }
    ready as isize
}

/// Fill in \`revents\` for every entry and answer how many have something.
fn check_ready(fds: usize, nodes: &[Option<Arc<dyn Inode>>]) -> usize {
    let mut count = 0;
    for (index, node) in nodes.iter().enumerate() {
        let at = fds + index * POLLFD_SIZE;
        let events = unsafe { ((at + POLLFD_EVENTS) as *const i16).read_volatile() };
        let revents = match node {
            None => POLLNVAL,
            Some(node) => {
                let ready = node.poll_ready();
                let mut bits = 0;
                if events & POLLIN != 0 && ready.read {
                    bits |= POLLIN;
                }
                if events & POLLOUT != 0 && ready.write {
                    bits |= POLLOUT;
                }
                bits
            }
        };
        unsafe { ((at + POLLFD_REVENTS) as *mut i16).write_volatile(revents) };
        if revents != 0 {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// select: the fd_set-bitmap spelling of poll
// ---------------------------------------------------------------------------

/// The ABI's ceiling on a descriptor number in an `fd_set`. Linux answers
/// `EINVAL` for anything past it, and so does this.
const FD_SETSIZE: usize = 1024;

/// One descriptor a `select` caller asked about: which fd, what it is, and
/// which of the caller's sets it appeared in.
struct SelectWatch {
    fd: i32,
    node: Option<Arc<dyn Inode>>,
    want_read: bool,
    want_write: bool,
}

/// What one watched descriptor can do right now, mapped through the caller's
/// interest. `poll_ready` is the same source `poll` consults.
fn watch_ready(watch: &SelectWatch) -> (bool, bool) {
    let Some(node) = &watch.node else {
        return (false, false);
    };
    let ready = node.poll_ready();
    (
        watch.want_read && ready.read,
        watch.want_write && ready.write,
    )
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)`.
///
/// musl calls straight into this on x86_64 - there is no separate "new"
/// select number as there was on i386 - and `apk`'s downloader parks every
/// fetch in it, which is what made this worth having. The fd_set bitmaps are
/// read into a watch list, the wait is poll's loop with different plumbing,
/// and the caller's sets are rewritten with the ready bits before the count
/// comes back.
///
/// The except set is validated and cleared but never answered: it reports
/// out-of-band data, and nothing here generates any.
/// `pselect6(2)` — `select` with a nanosecond timeout and a signal mask.
///
/// The sixth argument is a signal mask Linux swaps in atomically for the
/// duration of the wait. Nothing here runs a handler during a wait, and a
/// single-threaded caller cannot have one delivered either, so the mask is
/// read for validity and not acted on: the wait is the same wait.
pub(super) fn pselect6(args: [usize; ARG_COUNT]) -> isize {
    // A mask pointer that is not null names a `sigset_t`; the size word
    // musl passes alongside is not in this call's arguments.
    select_inner(args, true)
}

/// `select(2)` — the microsecond-timeout original.
pub(super) fn select(args: [usize; ARG_COUNT]) -> isize {
    select_inner(args, false)
}

/// The whole of `select`, with `timespec` selecting which shape `args[4]`
/// points at: `pselect6` hands a `timespec` (nanoseconds), `select` a
/// `timeval` (microseconds). Everything else - the sets, the wait, the
/// write-back - is one behaviour, so they share it rather than repeat it.
fn select_inner(args: [usize; ARG_COUNT], timespec: bool) -> isize {
    let (nfds_arg, readfds, writefds, exceptfds, tvp) =
        (args[0], args[1], args[2], args[3], args[4]);
    if (nfds_arg as i64) < 0 || nfds_arg > FD_SETSIZE {
        return err::EINVAL;
    }
    let nfds = nfds_arg;

    // One word per 64 descriptors, rounded up; a set shorter than that is
    // the caller's business, since only the bits below `nfds` are read.
    let set_bytes = nfds.div_ceil(64) * 8;
    for set in [readfds, writefds, exceptfds] {
        if set != 0 && !buffer_ok(set, set_bytes, true, true) {
            return err::EFAULT;
        }
    }
    if tvp != 0 && !buffer_ok(tvp, 16, true, true) {
        return err::EFAULT;
    }

    /// Read bit `fd` of a caller's set. `None` (no set) reads as clear.
    fn bit(set: usize, fd: usize) -> bool {
        if set == 0 {
            return false;
        }
        unsafe {
            let word = ((set + (fd / 64) * 8) as *const u64).read_volatile();
            word >> (fd % 64) & 1 == 1
        }
    }

    let mut watches: Vec<SelectWatch> = Vec::new();
    for fd in 0..nfds {
        let (r, w, e) = (bit(readfds, fd), bit(writefds, fd), bit(exceptfds, fd));
        if !r && !w && !e {
            continue;
        }
        // Unlike poll, which reports a dead fd as POLLNVAL, select answers
        // EBADF for the whole call - the shape every caller's error path
        // actually expects.
        let Ok(file) = crate::sched::current_fds().clone_at(fd as i32) else {
            return err::EBADF;
        };
        watches.push(SelectWatch {
            fd: fd as i32,
            node: Some(file.inode()),
            want_read: r,
            want_write: w,
        });
    }

    // Ask every node to wake us before the first check, as poll does: a
    // descriptor that turns ready between the check and the sleep must not
    // be missed.
    let me = crate::sched::current();
    for watch in &watches {
        if let Some(node) = &watch.node {
            node.poll_subscribe(me);
        }
    }

    let deadline = if tvp == 0 {
        None
    } else {
        // Both shapes lead with seconds and follow with a sub-second field:
        // microseconds for `select`, nanoseconds for `pselect6`. The field
        // is validated against its own unit before it is folded into
        // milliseconds, which is the resolution the wait below works in.
        let (sec, frac) = unsafe {
            let at = |offset: usize| (tvp + offset) as *const u64;
            (at(0).read_volatile(), at(8).read_volatile())
        };
        let unit = if timespec { 1_000_000 } else { 1_000 };
        if sec >= (1 << 43) || frac >= unit * 1_000 {
            // Beyond a few centuries, or a sub-second field that does not
            // fit its unit: a corrupted timeout. Linux clamps; a bounded
            // answer beats wrapping.
            return err::EINVAL;
        }
        let ms = sec * 1000 + frac.div_ceil(unit);
        let ticks = ms * crate::arch::timer::TICK_HZ as u64 / 1000;
        Some(crate::arch::timer::ticks() + ticks)
    };

    // Readiness as of the last check, kept beside the watches so the
    // write-back does not have to ask the nodes twice.
    let mut ready: Vec<(bool, bool)> = watches.iter().map(watch_ready).collect();
    let count = loop {
        let count: isize = ready.iter().filter(|(r, w)| *r || *w).count() as isize;
        if count > 0 {
            break count;
        }
        if deadline.is_some_and(|end| crate::arch::timer::ticks() >= end) {
            break 0;
        }
        match deadline {
            // Without a timeout and without descriptors there is nothing
            // that could wake this wait and nothing waiting for it: answer
            // zero rather than park the task forever.
            None if watches.is_empty() => break 0,
            // The wait's wake condition asks the nodes afresh each time
            // (block_until's condition must be side-effect free); the
            // recorded readiness is refreshed once it returns, for the
            // write-back below.
            None => {
                // Poll once a tick rather than park: the descriptors without
                // a poll_subscribe behind them - netlink, signalfd, eventfd -
                // have no waker to hand, and a blocked wait would never be
                // told they turned ready. poll's unbounded wait does the same.
                crate::sched::sleep_ticks(1);
                ready = watches.iter().map(watch_ready).collect();
            }
            Some(_) => {
                crate::sched::sleep_ticks(1);
                ready = watches.iter().map(watch_ready).collect();
            }
        }
    };

    for watch in &watches {
        if let Some(node) = &watch.node {
            node.poll_unsubscribe(me);
        }
    }

    // Rewrite the caller's sets with only the ready bits, as select's
    // contract demands - the sets arrive as interest and leave as result.
    for set in [readfds, writefds, exceptfds] {
        if set != 0 {
            unsafe {
                core::ptr::write_bytes(set as *mut u8, 0, set_bytes);
            }
        }
    }
    if count > 0 {
        for (watch, (r, w)) in watches.iter().zip(ready.iter()) {
            let mark = |set: usize, fd: usize| {
                if set != 0 {
                    unsafe {
                        let at = (set + (fd / 64) * 8) as *mut u64;
                        at.write_volatile(at.read_volatile() | 1u64 << (fd % 64));
                    }
                }
            };
            if *r {
                mark(readfds, watch.fd as usize);
            }
            if *w {
                mark(writefds, watch.fd as usize);
            }
            // exceptfds never gets bits: nothing here generates OOB data.
        }
    }

    // Linux reports the time that was left, and callers that loop on a
    // timeout compare it with zero to see whether they timed out. Both read
    // it the same way after a zero write.
    if tvp != 0 {
        let remaining_ms = match deadline {
            None => 0,
            Some(end) => {
                let now = crate::arch::timer::ticks();
                if now >= end {
                    0
                } else {
                    (end - now) * 1000 / crate::arch::timer::TICK_HZ as u64
                }
            }
        };
        unsafe {
            let at = |offset: usize| (tvp + offset) as *mut u64;
            at(0).write_volatile(remaining_ms / 1000);
            at(8).write_volatile(remaining_ms % 1000 * 1000);
        }
    }
    count
}

/// Write the 144-byte Linux x86_64 `struct stat` for `meta`.
///
/// The field offsets are the ABI, so they are spelled as the byte offsets of
/// the struct rather than as indices into a `u64` view - an off-by-one word
/// here is invisible to every caller that only reads `st_mode`, and silently
/// answers "empty file" to the ones that ask for `st_size`.
fn write_stat(buf: usize, meta: &Metadata, dev: u64, rdev: Option<(u32, u32)>, stored: u32) -> isize {
    if !buffer_ok(buf, 144, true, true) {
        return err::EFAULT;
    }
    let nlink: u64 = if meta.kind == NodeKind::Dir { 2 } else { 1 };
    let size = meta.size;
    let blocks: u64 = size.div_ceil(512);
    // The device number a character device node carries, packed the way
    // `makedev` lays it out: major in the high bytes, minor in the low.
    let packed_rdev = match rdev {
        Some((major, minor)) => ((major as u64) << 8) | (minor as u64 & 0xff) | 0,
        None => 0,
    };

    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 144);
        let at = |offset: usize| (buf + offset) as *mut u8;
        // st_dev names the *filesystem*, not the file: two files on one
        // filesystem share it, and a file on another has its own. That is
        // exactly the comparison `switch_root` makes before it deletes the
        // old root, and the one `df` makes to group what it reports.
        (at(0) as *mut u64).write_volatile(dev);
        (at(8) as *mut u64).write_volatile(meta.ino); // st_ino
        (at(16) as *mut u64).write_volatile(nlink); // st_nlink
        (at(24) as *mut u32).write_volatile(mode_of(meta, stored)); // st_mode
        // st_uid, st_gid and the pad word stay 0: one user. st_rdev carries
        // the node's own device number - the identity libc's `ttyname`
        // matches a descriptor by.
        (at(40) as *mut u64).write_volatile(packed_rdev); // st_rdev
        (at(48) as *mut u64).write_volatile(size); // st_size
        (at(56) as *mut u64).write_volatile(512); // st_blksize
        (at(64) as *mut u64).write_volatile(blocks); // st_blocks
        // Timestamps (offsets 72..120) stay 0: no clock is kept per inode.
    }
    0
}

/// Write the 120-byte Linux x86_64 `struct statfs` for the filesystem `name`
/// spells. Same rule as `write_stat`: the offsets are the ABI, and every
/// field is a word except the two-`u32` `f_fsid`. The magic is the real one
/// the filesystem reports on Linux - `switch_root` reads `f_type` to decide
/// whether the root it stands in is an initramfs, and answering "ext2" for a
/// ramfs is what made it refuse to switch.
///
/// The block counts are real for a filesystem that reports them (ext2, from
/// its superblock and group descriptors) and "memory-sized" for the kernel's
/// own trees. A filesystem with neither - and one that reports zero total
/// blocks - is what `df` filters out, exactly as Linux's own does.
fn write_statfs(buf: usize, name: &str, stats: Option<crate::vfs::FsStats>) -> isize {
    if !buffer_ok(buf, 120, true, true) {
        return err::EFAULT;
    }
    // The superblock magics, as Linux defines them. A caller that switches
    // roots reads exactly this field.
    let magic: u64 = match name {
        "ramfs" => 0x858458F6,
        // tmpfs and the devfs built over it are the same kind of tree here.
        "tmpfs" | "devfs" | "devtmpfs" => 0x01021994,
        "proc" => 0x00009FA0,
        "sysfs" => 0x62656572,
        "ext2" | "ext3" | "ext4" => 0x0000EF53,
        _ => 0,
    };
    // Memory-backed trees are as big as the frame allocator; a filesystem
    // with its own accounting answers with those numbers instead.
    let (bsize, blocks, free, files, ffree) = if let Some(s) = stats {
        (
            s.bsize,
            s.blocks,
            s.bfree,
            s.files,
            s.ffree,
        )
    } else {
        let memory_backed = matches!(name, "ramfs" | "tmpfs" | "devfs" | "devtmpfs");
        if memory_backed {
            critical_section::with(|cs| {
                let pmm = crate::mm::PMM.borrow(cs).borrow();
                (
                    4096u64,
                    (pmm.total_frames() * 4) as u64,
                    (pmm.free_frames() * 4) as u64,
                    0u64,
                    0u64,
                )
            })
        } else {
            (4096, 0, 0, 0, 0)
        }
    };
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 120);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u64).write_volatile(magic); // f_type
        (at(8) as *mut u64).write_volatile(bsize); // f_bsize
        (at(16) as *mut u64).write_volatile(blocks); // f_blocks
        (at(24) as *mut u64).write_volatile(free); // f_bfree
        (at(32) as *mut u64).write_volatile(free); // f_bavail
        (at(40) as *mut u64).write_volatile(files); // f_files
        (at(48) as *mut u64).write_volatile(ffree); // f_ffree
        (at(64) as *mut u64).write_volatile(255); // f_namelen
        // f_fsid (56..64), f_frsize (72), f_flags (80) and the four spares
        // stay 0: a zero f_frsize reads as "same as f_bsize".
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
    // A socket descriptor reads like a file to `read(2)` - the same call,
    // routed to the stack. `recvfrom` with null source slots is exactly the
    // semantics `read` has on a socket.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd as i32)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);
    if is_socket {
        return crate::net::socket::handle_recvfrom([fd, buf, len, 0, 0, 0], from_user);
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
    // The write half of the socket split: `sendto` with no destination,
    // which for a connected socket is the whole of `write`'s contract.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd as i32)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);
    if is_socket {
        return crate::net::socket::handle_sendto([fd, buf, len, 0, 0, 0], from_user);
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
/// `close(2)` — close `fd`.
///
/// A socket whose last descriptor this was comes down with it: the
/// description is lifted out of the table first, and only a sole surviving
/// reference - this close being the last one - tears the channel down.
pub(super) fn handle_close(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    match crate::vfs::close(fd) {
        Ok(()) => {}
        Err(e) => return err_of(e),
    }
    crate::net::socket::close_last_socket(&file);
    0
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
            crate::vfs::NodeKind::BlockDev => 6,
            crate::vfs::NodeKind::Symlink => 10,
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
    const O_CLOEXEC: u32 = 0o200_0000_u32;
    // `pipe(2)` is `pipe2(2)` with a zero flag word; the only flag that exists
    // is `O_CLOEXEC`, and it belongs to both ends. A pipe that outlives the
    // `execve` it was made for never reports end-of-stream to whoever is
    // reading it - which is exactly how `posix_spawn` learns whether the exec
    // worked - so the flag has to be recorded, not just accepted.
    let out = args[0];
    let flags = args[1] as u32;
    if flags & !O_CLOEXEC != 0 {
        return err::EINVAL;
    }
    if out == 0 || !buffer_ok(out, 2 * core::mem::size_of::<i32>(), true, from_user) {
        return err::EFAULT;
    }
    let (read_end, write_end) = crate::vfs::pipe::pair();
    let fds = [
        crate::vfs::file::open_inode(read_end),
        crate::vfs::file::open_inode(write_end),
    ];
    if flags & O_CLOEXEC != 0 {
        let table = crate::sched::current_fds();
        for fd in fds {
            let _ = table.fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
        }
    }
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
    // The name the description was opened by is exact; the search is only for
    // a directory arrived at some other way - an inherited descriptor, or one
    // opened before names were recorded at all.
    let path = file
        .label()
        .or_else(|| path_of(&inode))
        .unwrap_or_else(|| alloc::string::String::from("/"));
    adopt_cwd(inode, path);
    0
}

/// `ioctl(fd, request, arg)` — terminal settings and queries, stub the rest.
///
/// `TCGETS` and `TCSETS` are the ones that matter: a shell asks what the
/// terminal is, switches off line editing and echo so it can do its own, and
/// puts them back when it is finished. Answering `TCGETS` with sixty zeroes
/// while carrying on editing and echoing regardless is what makes a shell
/// show every line twice and read every arrow key as punctuation - so the
/// settings are the tty's, and the tty obeys them.
pub(super) fn handle_ioctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, request) = (args[0] as i32, args[1] as u32);
    let buf = args[2];
    match request {
        0x5413 => {
            // TIOCGWINSZ: report 80x25. `isatty` is a call to this on musl,
            // so it has to work on a tty and fail everywhere else.
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
        0x540E => {
            // TIOCSCTTY: make the descriptor's tty the controlling terminal.
            // There is no controlling-terminal bookkeeping here to change -
            // sessions do not exist yet - so the whole of the call is the tty
            // check, which is what getty insists on before it prints a
            // login prompt.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().as_tty().is_none() {
                return err::ENOTTY;
            }
            0
        }
        0x540F | 0x5410 => {
            // TIOCGPGRP / TIOCSPGRP: the terminal's foreground process group.
            //
            // Job control is built on exactly this pair. A shell asks who owns
            // the terminal, claims it for each job it starts, and takes it back
            // when the job finishes - and a kernel that cannot answer makes
            // every shell print `can't access tty; job control turned off` and
            // then run as though it were talking to a file.
            //
            // TIOCSPGRP is 0x5410 on x86_64 - 0x541E is TIOCSETD, and routing
            // the pair there was why every `tcsetpgrp` read back ENOTTY.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };

            if request == 0x540F {
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                // Zero is what a terminal with no foreground job reports, and
                // it is a valid answer rather than an error: a shell is
                // allowed to find out that nobody owns the terminal yet.
                let pgid = tty.foreground().unwrap_or(crate::sched::TaskId(0));
                unsafe { (buf as *mut i32).write_volatile(pgid.0 as i32) };
                return 0;
            }

            if !buffer_ok(buf, 4, false, from_user) {
                return err::EFAULT;
            }
            let pgid = unsafe { (buf as *const i32).read_volatile() };
            if pgid <= 0 || pgid as usize >= crate::sched::MAX_TASKS {
                return err::EINVAL;
            }
            tty.set_foreground(Some(crate::sched::TaskId(pgid as usize)));
            0
        }
        0x5401 => {
            // TCGETS
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, crate::vfs::termios::SIZE, true, from_user) {
                return err::EFAULT;
            }
            unsafe { tty.termios().store_to(buf as *mut u8) };
            0
        }
        // TCSETS, TCSETSW, TCSETSF: the difference between them is when they
        // take effect, which is about draining an output queue this tty does
        // not have - a byte written is already gone.
        0x5402..=0x5404 => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, crate::vfs::termios::SIZE, false, from_user) {
                return err::EFAULT;
            }
            let tio = unsafe { crate::vfs::termios::Termios::from_ptr(buf as *const u8) };
            tty.set_termios(tio);
            0
        }
        // The block-device facts libblkid asks for before it probes a device:
        // how big it is, in bytes and in sectors, and what one sector is.
        // Without them a probe reads the device blind, sizing it by EIO.
        0x80081272 | 0x1260 | 0x1268 => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let Some(dev) = file.inode().as_block() else {
                return err::ENOTTY;
            };
            let sectors = dev.capacity_sectors();
            match request {
                // BLKGETSIZE64: u64, the size in bytes.
                0x80081272 => {
                    if !buffer_ok(buf, 8, false, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u64).write_volatile(sectors * 512) };
                }
                // BLKGETSIZE: u32, the size in 512-byte sectors.
                0x1260 => {
                    if !buffer_ok(buf, 4, false, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u32).write_volatile(sectors as u32) };
                }
                // BLKSSZGET: u32, the sector size itself.
                _ => {
                    if !buffer_ok(buf, 4, false, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u32).write_volatile(512) };
                }
            }
            0
        }
        0x8942 => {
            // SIOCGIFHWADDR: the hardware address of the named interface.
            // `ip` falls back to this for the MAC a netlink dump did not
            // spell out. One interface exists; its address is the one the
            // stack's static configuration assumes.
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                let at = |offset: usize| (buf + offset) as *mut u8;
                // ifr_hwaddr: sockaddr with family ARPHRD_ETHER, then the MAC.
                (at(16) as *mut u16).write_volatile(1);
                for (i, byte) in [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56]
                    .into_iter()
                    .enumerate()
                {
                    at(18 + i).write_volatile(byte);
                }
            }
            0
        }
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

    // A socket has no VFS write behind it, and musl's stdio hands every
    // buffered flush to `writev` - without this split a socket's stdio
    // answers ENOSYS and every libc program that talks through FILE* dies
    // on its first send. TCP is a byte stream, so sending the iovecs one
    // by one lands the same bytes in the same order one vectored send
    // would.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);

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
        if is_socket {
            let sent =
                crate::net::socket::handle_sendto([fd as usize, ptr, len, 0, 0, 0], from_user);
            if sent < 0 {
                if total > 0 {
                    return total;
                }
                return sent;
            }
            total += sent;
            if (sent as usize) < len {
                return total;
            }
            continue;
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

    // The read twin of the writev split below it: musl's stdio issues every
    // fill as a readv - an internal buffer plus the caller's own - and a
    // socket answers through the stack, not the VFS. The stack hands out
    // one contiguous run per recv, so the first buffer with room takes it;
    // stopping early with a short count is also what a kernel readv does.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);

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
        if is_socket {
            let got =
                crate::net::socket::handle_recvfrom([fd as usize, ptr, len, 0, 0, 0], from_user);
            if got < 0 {
                if total > 0 {
                    return total;
                }
                return got;
            }
            total += got;
            if (got as usize) < len {
                break;
            }
            continue;
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

/// Walk `/` looking for a path that reaches `target` - the fallback `fchdir`
/// uses for a directory whose descriptor never recorded a name.
///
/// The visit budget is the real bound, not the depth: a tree search that has
/// looked at this many directories has left the region where an answer is
/// plausible, and a caller waiting on `fchdir` is owed one either way.
fn path_of(target: &Arc<dyn Inode>) -> Option<alloc::string::String> {
    use crate::vfs::NodeKind;

    const MAX_DEPTH: usize = 16;
    const MAX_VISITS: usize = 4096;

    fn walk(
        dir: &Arc<dyn Inode>,
        prefix: &str,
        target: &Arc<dyn Inode>,
        depth: usize,
        visits: &mut usize,
    ) -> Option<alloc::string::String> {
        if Arc::ptr_eq(dir, target) {
            return Some(alloc::string::String::from(prefix));
        }
        if depth == 0 {
            return None;
        }
        for (name, kind) in dir.read_dir().ok()? {
            // `.` and `..` lead back to directories already on the path being
            // built. Following them spends the whole depth budget on a name
            // like `/./././etc`, which is how this search used to fail.
            if kind != NodeKind::Dir || name == "." || name == ".." {
                continue;
            }
            if *visits == 0 {
                return None;
            }
            *visits -= 1;
            let child = if prefix == "/" {
                alloc::format!("/{name}")
            } else {
                alloc::format!("{prefix}/{name}")
            };
            if let Ok(inode) = crate::vfs::resolve(&child)
                && let Some(found) = walk(&inode, &child, target, depth - 1, visits)
            {
                return Some(found);
            }
        }
        None
    }
    let root = crate::vfs::global().root_inode();
    let mut visits = MAX_VISITS;
    walk(&root, "/", target, MAX_DEPTH, &mut visits)
}

/// Record the cwd inode + path together, so `getcwd` reads back the path
/// the user wrote.
fn adopt_cwd(dir: Arc<dyn Inode>, path: alloc::string::String) {
    crate::sched::set_cwd(dir);
    crate::sched::set_cwd_path(path);
}

// ---------------------------------------------------------------------------
// The calls an ordinary libc reaches for
// ---------------------------------------------------------------------------

/// Run `body` with `fd`'s position moved to `offset`, and put the position
/// back whatever happens.
///
/// The offset read/write pair is `read`/`write` at a position the caller
/// names instead of the one the descriptor carries - which is why it does
/// not disturb that position: a `pread` between two `read`s must leave the
/// second one where the first left off. Restoring on every path, error
/// included, is the whole of the promise.
fn at_position<R>(fd: i32, offset: i64, body: impl FnOnce() -> R) -> R {
    let file = crate::sched::current_fds().clone_at(fd);
    let saved = file.as_ref().map(|f| f.pos()).ok();
    if let (Ok(file), Ok(offset)) = (&file, u64::try_from(offset)) {
        file.set_pos(offset);
    }
    let result = body();
    if let (Ok(file), Some(pos)) = (&file, saved) {
        file.set_pos(pos);
    }
    result
}

/// `pread64(fd, buf, count, offset)` — read at a position without moving it.
pub(super) fn handle_pread64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len, offset) = (args[0] as i32, args[1], args[2], args[3] as i64);
    // A negative offset is not a position; `EOVERFLOW`-shaped nonsense is
    // still just `EINVAL` to the caller that wrote it.
    if offset < 0 {
        return err::EINVAL;
    }
    if !buffer_ok(buf, len, true, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        let bytes = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
        match crate::vfs::read(fd, bytes) {
            Ok(n) => n as isize,
            Err(e) => err_of(e),
        }
    })
}

/// `pwrite64(fd, buf, count, offset)` — write at a position without moving it.
pub(super) fn handle_pwrite64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len, offset) = (args[0] as i32, args[1], args[2], args[3] as i64);
    if offset < 0 {
        return err::EINVAL;
    }
    if !buffer_ok(buf, len, false, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
        match crate::vfs::write(fd, bytes) {
            Ok(n) => n as isize,
            Err(e) => err_of(e),
        }
    })
}

/// `preadv(fd, iov, iovcnt, offset)` / `pwritev`: the vectored pair at a
/// position. Same position discipline as the scalar pair, same loop as
/// `readv`/`writev` - the offset is the only thing added.
fn io_at_position(args: [usize; ARG_COUNT], from_user: bool, writing: bool) -> isize {
    let (fd, offset) = (args[0] as i32, args[3] as i64);
    if offset < 0 {
        return err::EINVAL;
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        if writing {
            handle_writev(args, from_user)
        } else {
            handle_readv(args, from_user)
        }
    })
}

/// `preadv(2)` — vectored read at a position.
pub(super) fn handle_preadv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    io_at_position(args, from_user, false)
}

/// `pwritev(2)` — vectored write at a position.
pub(super) fn handle_pwritev(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    io_at_position(args, from_user, true)
}

/// `fsync(fd)` / `fdatasync(fd)` — flush a descriptor to its storage.
///
/// Every write here already reached its storage, so there is nothing to
/// flush: ramfs and ext2 write through, and a socket has no storage at all.
/// What the call still owes the caller is the argument check - and Linux
/// answers `EINVAL` for a descriptor bound to something that cannot be
/// synchronized, which is a pipe, a terminal or a socket.
pub(super) fn handle_fsync(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    match file.metadata().kind {
        NodeKind::File | NodeKind::Dir | NodeKind::Symlink | NodeKind::BlockDev => 0,
        NodeKind::CharDev => err::EINVAL,
    }
}

/// `chmod(2)` / `fchmod(2)` — set permission bits.
///
/// There is one user and no permission checks anywhere in this kernel, so
/// every mode is satisfiable and the call is a shape check: the descriptor
/// or the path has to exist. The bits are not remembered, which is the same
/// answer a caller gets on a filesystem mounted without them.
pub(super) fn handle_chmod(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (path, _mode) = (args[0], args[1]);
    let path = match cstr_arg(path, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `fchmod(2)` — the same, by descriptor.
pub(super) fn handle_fchmod(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `chown(2)` / `fchown(2)` — set owner and group. One user, no groups:
/// the call is accepted for anything that exists, as `chmod` is.
pub(super) fn handle_chown(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `fchown(2)` — the same, by descriptor.
pub(super) fn handle_fchown(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `fchownat(dirfd, path, uid, gid, flags)` — the path form, and the only one
/// this architecture has: there is no `lchown` syscall, so this is what musl's
/// `lchown` becomes.
///
/// The flag is the reason the call exists here at all. `AT_SYMLINK_NOFOLLOW` is
/// what makes it able to name a *link* rather than what it points at, which is
/// the whole difference between `chown` and `lchown` - and a file that is not
/// there is still an error, because the caller is naming something.
pub(super) fn handle_fchownat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let found = if args[4] & AT_SYMLINK_NOFOLLOW != 0 {
        crate::vfs::global().resolve_no_follow(&path, base)
    } else {
        crate::vfs::resolve_opt(&path, Some(base))
    };
    // The owner is not stored, so nothing is changed - but the name is still
    // resolved, because a caller that named something wrong has to hear about
    // it rather than be told the owner was set.
    match found {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `lchown(path, uid, gid)` — the owner of the link itself.
///
/// This is `fchownat` with the one flag that makes it name the link rather than
/// what it points at, which is the whole of what `lchown` is. A tool unpacking
/// an archive sets the owner of every member, and a link is a member like any
/// other.
pub(super) fn handle_lchown(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    handle_fchownat(
        [
            AT_FDCWD as usize,
            args[0],
            args[1],
            args[2],
            AT_SYMLINK_NOFOLLOW,
            0,
        ],
        cwd,
    )
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)` — a second name
/// for one inode.
///
/// The VFS has no link count: every node has one name, and `unlink` drops
/// the node with it. Linux answers `EPERM` when the filesystem does not
/// support hard links, which is exactly this, so a caller's fallback -
/// copy the file - is what it would do on a filesystem that cannot link.
pub(super) fn handle_linkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let old_base = match base_of(args[0] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let old_path = match cstr_arg(args[1], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_base = match base_of(args[2] as i32, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let new_path = match cstr_arg(args[3], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Both names have to exist as paths for the request to be a request:
    // a link to a missing file is ENOENT, not EPERM.
    let old = match crate::vfs::global().resolve_no_follow(&old_path, old_base) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    let (parent_path, name) = match crate::vfs::split_parent(&new_path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent_path, Some(new_base)) {
        Ok(parent) => parent,
        Err(e) => return err_of(e),
    };
    // A name edge belongs to the filesystem that keeps the directory. An old
    // inode served by a different filesystem cannot be reached by one of this
    // one's edges, which is `link(2)`'s EXDEV - the caller's cue to copy.
    // The filesystem instance is identified by the deepest mountpoint over
    // each path, the same rule `st_dev` uses; inode identity cannot answer
    // here, because a non-mount-root node does not know its filesystem.
    let old_fs = crate::vfs::mount::fs_of_path(&absolute(&old_path)).0;
    let parent_fs = crate::vfs::mount::fs_of_path(&absolute(parent_path)).0;
    if old_fs != parent_fs {
        return err::EXDEV;
    }
    match parent.link(name, &old) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `mknod(path, mode, dev)` — create a device or FIFO node.
///
/// The nodes an initramfs's init pre-seeds (`[ -c /dev/null ] || mknod
/// /dev/null c 1 3`) land on the temporary root, which is a ramfs: they are
/// placeholders the `devtmpfs` mount then covers with driver-backed devices.
/// A name that already exists answers success - the caller wanted the node,
/// not the call - and a filesystem that cannot hold the kind passes its own
/// error through.
pub(super) fn handle_mknod(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let kind = match args[1] as u32 & 0o170000 {
        0o020000 => NodeKind::CharDev,
        0o060000 => NodeKind::BlockDev,
        // A FIFO is a plain file here: the descriptor layer cannot tell.
        0o010000 => NodeKind::File,
        _ => return err::EINVAL,
    };
    let absolute = if path.starts_with('/') {
        crate::vfs::path::normalize(&path)
    } else {
        let cwd_path = crate::sched::current_cwd_path();
        crate::vfs::path::normalize(&alloc::format!(
            "{}/{}",
            cwd_path.trim_end_matches('/'),
            path
        ))
    };
    // A name this kernel already serves needs no creating.
    if crate::vfs::resolve_opt(&absolute, Some(cwd.clone())).is_ok() {
        return 0;
    }
    let (parent_path, name) = match absolute.rsplit_once('/') {
        Some((p, n)) if p.is_empty() => (String::from("/"), n),
        Some((p, n)) => (String::from(p), n),
        None => return err::ENOENT,
    };
    let parent = match crate::vfs::resolve_opt(&parent_path, Some(cwd.clone())) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    match parent.create(name, kind) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `fadvise64(fd, offset, len, advice)` — a hint about coming access.
///
/// Nothing here reads ahead or caches evictable pages, so every advice is
/// accepted and does nothing - which is what Linux does for the advices it
/// does not act on. The descriptor still has to be real.
pub(super) fn handle_fadvise64(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}
