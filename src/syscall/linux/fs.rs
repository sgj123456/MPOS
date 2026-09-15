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
fn mode_of(meta: &Metadata) -> u32 {
    match meta.kind {
        NodeKind::File => 0o100_644,    // regular file rw-r--r--
        NodeKind::Dir => 0o040_755,     // directory rwxr-xr-x
        NodeKind::CharDev => 0o020_666, // character device rw-rw-rw-
        NodeKind::Symlink => 0o120_777, // symbolic link rwxrwxrwx
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
            // a later `fchdir` can give the cwd a name `getcwd` reads back.
            // Files are left unlabelled: only a directory can be a cwd.
            if let Some(label) = label
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
                && file.metadata().kind == crate::vfs::NodeKind::Dir
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
    let meta = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file.metadata(),
        Err(e) => return err_of(e),
    };
    write_stat(buf, &meta)
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

/// `fstatfs(fd, buf)` - filesystem facts for whatever `fd` is open on. Every
/// descriptor lives on the one ext2 root filesystem, so the answer is the
/// same for all of them; only the fd's validity is checked.
pub(super) fn fstatfs(args: [usize; ARG_COUNT]) -> isize {
    if let Err(e) = crate::sched::current_fds().clone_at(args[0] as i32) {
        return err_of(e);
    }
    write_statfs(args[1])
}

/// `statfs(path, buf)` - the path spelling of `fstatfs`.
pub(super) fn statfs(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => write_statfs(args[1]),
        Err(e) => err_of(e),
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
    // `lstat` is the one call that wants the link and not its target, so it
    // walks to the parent and looks the name up there - a resolver that
    // followed links would hand back the very thing it was told not to.
    let meta = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        match crate::vfs::global().resolve_no_follow(&path, base) {
            Ok(inode) => inode.metadata(),
            Err(e) => return err_of(e),
        }
    } else {
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => inode.metadata(),
            Err(e) => return err_of(e),
        }
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
    // No atomic rename primitive underneath: copy the bytes, then drop the
    // old name. A crash in between leaves both names, never none.
    let data = match crate::vfs::read_file_opt(&old_path, Some(old_base.clone())) {
        Ok(d) => d,
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
    let (inode, replacing) = match dir.create(&name, NodeKind::File) {
        Ok(inode) => (inode, false),
        // Renaming onto an existing file replaces it - unless the caller
        // asked for NOREPLACE, which is the one flag that protects the name.
        Err(crate::vfs::VfsError::Exists) => {
            if flags & RENAME_NOREPLACE != 0 {
                return err::EEXIST;
            }
            match crate::vfs::resolve_opt(&new_path, Some(new_base)) {
                Ok(inode) => (inode, true),
                Err(e) => return err_of(e),
            }
        }
        Err(e) => return err_of(e),
    };
    // Replacing means replacing: the name ends up on a file whose contents
    // are the source's, and *only* the source's. Writing over the target from
    // offset zero would leave whatever tail was longer than the source, so
    // the file would be the source followed by the old file's leftovers - and
    // a plain `rename` over a longer name is the ordinary way a cache, a log
    // or a download directory replaces what it had.
    //
    // Only when there was something there to replace: a name that was just
    // created is already empty, and filesystems are not obliged to offer a
    // resize at all.
    if replacing && let Err(e) = inode.truncate(0) {
        return err_of(e);
    }
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
    match parent.symlink(name, &target) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `fcntl(fd, cmd, arg)` - descriptor flags are stored, the rest is a stub.
///
/// `F_GETFD`/`F_SETFD` round-trip through the fd table: startup code probes
/// them and reads back what it wrote. `F_GETFL` answers `O_RDONLY` - opens
/// here are untyped - and `F_SETFL` accepts and forgets. A command Linux has
/// no meaning for answers `EINVAL`, not success.
pub(super) fn fcntl(args: [usize; ARG_COUNT]) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
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
        match deadline {
            // Nobody is going to wake us on a timer, so sleep until a node
            // does. The loop re-checks: another reader may have taken it.
            None => crate::sched::block_until(|| check_ready(fds, &nodes) > 0),
            Some(_) => crate::sched::sleep_ticks(1),
        }
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
                crate::sched::block_until(|| {
                    watches
                        .iter()
                        .any(|watch| matches!(watch_ready(watch), (true, _) | (_, true)))
                });
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

/// Write the 120-byte Linux x86_64 `struct statfs`.
///
/// Same rule as `write_stat`: the offsets are the ABI, and every field is a
/// word except the two-`u32` `f_fsid`. The root filesystem is the one ext2
/// there is, so the answer is constant. The block counts are coarse - no
/// superblock statistics are carried into the kernel's VFS - but they say
/// "roughly half of 8 MiB free", which is what an apk caller asking before a
/// download wants to hear.
fn write_statfs(buf: usize) -> isize {
    if !buffer_ok(buf, 120, true, true) {
        return err::EFAULT;
    }
    const EXT2_SUPER_MAGIC: u64 = 0xEF53;
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 120);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u64).write_volatile(EXT2_SUPER_MAGIC); // f_type
        (at(8) as *mut u64).write_volatile(4096); // f_bsize
        (at(16) as *mut u64).write_volatile(2048); // f_blocks (8 MiB)
        (at(24) as *mut u64).write_volatile(1024); // f_bfree
        (at(32) as *mut u64).write_volatile(1024); // f_bavail
        (at(40) as *mut u64).write_volatile(512); // f_files
        (at(48) as *mut u64).write_volatile(256); // f_ffree
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
        0x540F | 0x541E => {
            // TIOCGPGRP / TIOCSPGRP: the terminal's foreground process group.
            //
            // Job control is built on exactly this pair. A shell asks who owns
            // the terminal, claims it for each job it starts, and takes it back
            // when the job finishes - and a kernel that cannot answer makes
            // every shell print `can't access tty; job control turned off` and
            // then run as though it were talking to a file.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };

            crate::println!(
                "[TIOC{}PGRP] fd={fd} foreground={:?}",
                if request == 0x540F { "G" } else { "S" },
                tty.foreground()
            );
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
        NodeKind::File | NodeKind::Dir | NodeKind::Symlink => 0,
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
    if crate::vfs::resolve_opt(&old_path, Some(old_base)).is_err() {
        return err::ENOENT;
    }
    let _ = (new_base, new_path);
    err::EPERM
}

/// `mknod(path, mode, dev)` — create a device or FIFO node.
///
/// There are no device nodes to create outside devfs, and a caller without
/// CAP_MKNOD gets `EPERM` for it on Linux too.
pub(super) fn handle_mknod(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let _ = (path, cwd);
    err::EPERM
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
