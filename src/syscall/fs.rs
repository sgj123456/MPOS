//! File-related syscall handlers: READ, WRITE, OPEN, CLOSE, CREATE, UNLINK,
//! LISTDIR, DUP2, CHDIR, PIPE.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::err;

/// Read from a file descriptor.
pub(super) fn handle_read(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !super::buffer_ok(buf, len, true, from_user) {
        return if len > super::MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if len == 0 {
        return match crate::vfs::read(fd as i32, &mut []) {
            Ok(_) => 0,
            Err(e) => super::err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match crate::vfs::read(fd as i32, bytes) {
        Ok(n) => n as isize,
        Err(e) => super::err_of(e),
    }
}

/// Write to a file descriptor.
pub(super) fn handle_write(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !super::buffer_ok(buf, len, false, from_user) {
        return if len > super::MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if len == 0 {
        return match crate::vfs::write(fd as i32, &[]) {
            Ok(_) => 0,
            Err(e) => super::err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
    match crate::vfs::write(fd as i32, bytes) {
        Ok(n) => n as isize,
        Err(e) => super::err_of(e),
    }
}

/// Open a file.
pub(super) fn handle_open(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (ptr, len) = (args[0], args[1]);
    match super::path_arg(ptr, len, from_user) {
        Ok(path) => match crate::vfs::open_opt(&path, Some(cwd.clone())) {
            Ok(fd) => fd as isize,
            Err(e) => super::err_of(e),
        },
        Err(e) => e,
    }
}

/// Close a file descriptor.
pub(super) fn handle_close(args: [usize; super::ARG_COUNT]) -> isize {
    match crate::vfs::close(args[0] as i32) {
        Ok(()) => 0,
        Err(e) => super::err_of(e),
    }
}

/// Create a file or directory.
pub(super) fn handle_create(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let path = match super::path_arg(args[0], args[1], from_user) {
        Ok(path) => path,
        Err(e) => return e,
    };
    if let Err(e) = crate::vfs::create_file_opt(&path, Some(cwd.clone())) {
        return super::err_of(e);
    }
    match crate::vfs::open_opt(&path, Some(cwd.clone())) {
        Ok(fd) => fd as isize,
        Err(e) => super::err_of(e),
    }
}

/// Remove a filesystem entry.
pub(super) fn handle_unlink(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let path = match super::path_arg(args[0], args[1], from_user) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::unlink_opt(&path, Some(cwd.clone())) {
        Ok(()) => 0,
        Err(e) => super::err_of(e),
    }
}

/// Change working directory.
pub(super) fn handle_chdir(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let path = match super::path_arg(args[0], args[1], from_user) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let start = crate::sched::current_cwd();
    match crate::vfs::resolve_at(&path, start) {
        Ok(dir) if dir.metadata().kind == crate::vfs::NodeKind::Dir => {
            crate::sched::set_cwd(dir);
            0
        }
        Ok(_) => err::ENOTDIR,
        Err(e) => super::err_of(e),
    }
}

/// Duplicate a file descriptor.
pub(super) fn handle_dup2(args: [usize; super::ARG_COUNT]) -> isize {
    let (old, new) = (args[0] as i32, args[1] as i32);
    if new < 0 {
        return err::EBADF;
    }
    match crate::vfs::dup2(old, new) {
        Ok(fd) => fd as isize,
        Err(e) => super::err_of(e),
    }
}

/// List directory entries.
pub(super) fn handle_listdir(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (ptr, len, buf, buf_len) = (args[0], args[1], args[2], args[3]);
    let path = match super::path_arg(ptr, len, from_user) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let entries = match crate::vfs::read_dir_opt(&path, Some(cwd.clone())) {
        Ok(entries) => entries,
        Err(e) => return super::err_of(e),
    };

    use crate::syscall::nr;
    let mut stream = Vec::new();
    for (name, kind) in &entries {
        stream.push(match kind {
            crate::vfs::NodeKind::File => nr::kind::FILE,
            crate::vfs::NodeKind::Dir => nr::kind::DIR,
            crate::vfs::NodeKind::CharDev => nr::kind::CHARDEV,
        });
        stream.extend_from_slice(name.as_bytes());
        stream.push(0);
    }
    if buf_len == 0 {
        return stream.len() as isize;
    }
    if buf_len < stream.len() {
        return err::EINVAL;
    }
    if !super::buffer_ok(buf, stream.len(), true, from_user) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(stream.as_ptr(), buf as *mut u8, stream.len());
    }
    stream.len() as isize
}

/// Create a pipe.
pub(super) fn handle_pipe(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let out = args[0];
    if out == 0 || !super::buffer_ok(out, 2 * core::mem::size_of::<i32>(), true, from_user) {
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

// ---------------------------------------------------------------------------
// Linux-compatible syscall handlers
// ---------------------------------------------------------------------------

/// Linux `fstat` / `newfstatat`: fill a minimal `stat` structure.
///
/// The struct layout matches `struct stat` on Linux x86_64.
pub(super) fn handle_fstat(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (fd_or_dirfd, path_ptr, path_len, stat_buf) = (args[0], args[1], args[2], args[3]);

    // newfstatat: dirfd + path + buf
    // fstat: fd + buf (path_ptr=path_len=0 in our mapping)
    let meta = if path_ptr != 0 || path_len != 0 {
        // newfstatat — resolve path relative to dirfd
        let _ = fd_or_dirfd;
        let path = match super::path_arg(path_ptr, path_len, from_user) {
            Ok(p) => p,
            Err(e) => return e,
        };
        match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
            Ok(inode) => inode.metadata(),
            Err(e) => return super::err_of(e),
        }
    } else {
        // fstat — look up by fd
        match crate::sched::current_fds().clone_at(fd_or_dirfd as i32) {
            Ok(file) => file.metadata(),
            Err(e) => return super::err_of(e),
        }
    };

    if !super::buffer_ok(stat_buf, 144, true, from_user) {
        return err::EFAULT;
    }
    write_stat(stat_buf, &meta);
    0
}

/// Write a Linux `struct stat` (144 bytes on x86_64) to userspace.
fn write_stat(buf: usize, meta: &crate::vfs::Metadata) {
    let mode: u32 = match meta.kind {
        crate::vfs::NodeKind::File => 0o100_644, // regular file rw-r--r--
        crate::vfs::NodeKind::Dir => 0o040_755,  // directory rwxr-xr-x
        crate::vfs::NodeKind::CharDev => 0o020_666,
    };
    let nlink: u64 = if meta.kind == crate::vfs::NodeKind::Dir {
        2
    } else {
        1
    };
    let size = meta.size;
    let blksize: i64 = 512;
    let blocks: i64 = (size).div_ceil(512) as i64;

    // Zero the whole struct.
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 144);
    }

    let s = buf as *mut u64;
    unsafe {
        // struct stat on x86_64 Linux:
        *s.add(0) = meta.ino; // st_dev (use ino as dev too)
        *s.add(1) = meta.ino; // st_ino
        *s.add(2) = nlink; // st_nlink
        *s.add(3) = mode as u64; // st_mode
        *s.add(4) = 0; // st_uid
        *s.add(5) = 0; // st_gid
        *s.add(6) = 0; // st_rdev
        *s.add(7) = size; // st_size
        *s.add(8) = blksize as u64; // st_blksize
        *s.add(9) = blocks as u64; // st_blocks
        // Fields 10..14 are timestamps (atime, mtime, ctime) — left as zero.
    }
}

/// `lseek(fd, offset, whence)`.
pub(super) fn handle_lseek(args: [usize; super::ARG_COUNT]) -> isize {
    let (fd, offset, whence) = (args[0] as i32, args[1] as i64, args[2] as i32);
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return super::err_of(e),
    };
    let new_pos: u64 = match whence {
        0 => offset as u64,                                    // SEEK_SET
        1 => file.pos().wrapping_add(offset as u64),           // SEEK_CUR
        2 => file.metadata().size.wrapping_add(offset as u64), // SEEK_END
        _ => return err::EINVAL,
    };
    if offset < 0 && whence != 2 && (offset.unsigned_abs() > file.pos()) {
        return err::EINVAL;
    }
    file.set_pos(new_pos);
    new_pos as isize
}

/// `getdents64(fd, buf, len)` — Linux directory listing format.
pub(super) fn handle_getdents64(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, buf_len) = (args[0] as i32, args[1], args[2]);
    if !super::buffer_ok(buf, buf_len, true, from_user) {
        return err::EFAULT;
    }

    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return super::err_of(e),
    };
    let entries = match file.inode().read_dir() {
        Ok(e) => e,
        Err(e) => return super::err_of(e),
    };

    // Linux getdents64 format:
    //   u64 d_ino
    //   u64 d_off
    //   u16 d_reclen
    //   u8  d_type
    //   u8  d_name[] (NUL-terminated, padded to 8-byte alignment)
    let mut offset = 0usize;
    for (idx, (name, kind)) in entries.iter().enumerate() {
        let reclen = (19 + name.len() + 1 + 7) & !7; // header 18 bytes + name + NUL, aligned
        if offset + reclen > buf_len {
            if offset == 0 {
                return err::EINVAL;
            }
            break;
        }
        let d_type = match kind {
            crate::vfs::NodeKind::File => 8,    // DT_REG
            crate::vfs::NodeKind::Dir => 4,     // DT_DIR
            crate::vfs::NodeKind::CharDev => 2, // DT_CHR
        };
        unsafe {
            let base = (buf + offset) as *mut u8;
            // d_ino
            (base as *mut u64).write_volatile(idx as u64 + 1);
            // d_off (virtual offset)
            ((base.add(8)) as *mut u64).write_volatile((idx + 1) as u64);
            // d_reclen
            ((base.add(16)) as *mut u16).write_volatile(reclen as u16);
            // d_type
            base.add(18).write_volatile(d_type);
            // d_name
            let name_bytes = name.as_bytes();
            core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), base.add(19), name_bytes.len());
            base.add(19 + name_bytes.len()).write_volatile(0);
        }
        offset += reclen;
    }
    offset as isize
}

/// `dup(oldfd)` — duplicate, picking the lowest free fd.
pub(super) fn handle_dup(args: [usize; super::ARG_COUNT]) -> isize {
    let old = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return super::err_of(e),
    };
    crate::sched::current_fds().install(file) as isize
}

/// `dup3(oldfd, newfd, flags)` — like dup2 but with flags (O_CLOEXEC ignored).
pub(super) fn handle_dup3(args: [usize; super::ARG_COUNT]) -> isize {
    let (old, new, _flags) = (args[0] as i32, args[1] as i32, args[2] as i32);
    if old == new {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return super::err_of(e),
    };
    match crate::sched::current_fds().install_at(file, new) {
        Ok(()) => new as isize,
        Err(e) => super::err_of(e),
    }
}

/// `openat(dirfd, path, flags, mode)` — simplified: ignores flags/mode.
pub(super) fn handle_openat(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (_dirfd, path_ptr, path_len, _flags) = (args[0], args[1], args[2], args[3]);
    let path = match super::path_arg(path_ptr, path_len, from_user) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::open_opt(&path, Some(cwd.clone())) {
        Ok(fd) => fd as isize,
        Err(e) => super::err_of(e),
    }
}

/// `unlinkat(dirfd, path, flags)` — simplified: AT_REMOVEDIR not supported.
pub(super) fn handle_unlinkat(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (_dirfd, path_ptr, path_len, _flags) = (args[0], args[1], args[2], args[3]);
    let path = match super::path_arg(path_ptr, path_len, from_user) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::unlink_opt(&path, Some(cwd.clone())) {
        Ok(()) => 0,
        Err(e) => super::err_of(e),
    }
}

/// `mkdirat(dirfd, path, mode)` — mode ignored.
pub(super) fn handle_mkdirat(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (_dirfd, path_ptr, path_len, _mode) = (args[0], args[1], args[2], args[3]);
    let path = match super::path_arg(path_ptr, path_len, from_user) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (parent, name) = match crate::vfs::split_parent(&path) {
        Ok(p) => p,
        Err(e) => return super::err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent, Some(cwd.clone())) {
        Ok(d) => d,
        Err(e) => return super::err_of(e),
    };
    match parent.create(name, crate::vfs::NodeKind::Dir) {
        Ok(_) => 0,
        Err(e) => super::err_of(e),
    }
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)` — simplified.
pub(super) fn handle_renameat2(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (_old_dirfd, old_ptr, old_len, _new_dirfd, new_ptr, new_len) =
        (args[0], args[1], args[2], args[3], args[4], args[5]);
    let old_path = match super::path_arg(old_ptr, old_len, from_user) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_path = match super::path_arg(new_ptr, new_len, from_user) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Simple implementation: copy then unlink old.
    let data = match crate::vfs::read_file_opt(&old_path, Some(cwd.clone())) {
        Ok(d) => d,
        Err(e) => return super::err_of(e),
    };
    if let Some(slash_pos) = new_path.rfind('/') {
        let (parent_dir, _) = new_path.split_at(slash_pos);
        let dir = match crate::vfs::resolve_opt(parent_dir, Some(cwd.clone())) {
            Ok(d) => d,
            Err(e) => return super::err_of(e),
        };
        let name = &new_path[slash_pos + 1..];
        match dir.create(name, crate::vfs::NodeKind::File) {
            Ok(inode) => {
                let f = crate::vfs::File::new(inode);
                let _ = f.write(&data);
            }
            Err(e) => return super::err_of(e),
        }
    }
    let _ = crate::vfs::unlink_opt(&old_path, Some(cwd.clone()));
    0
}

/// `getcwd(buf, size)` — write cwd path into buffer.
pub(super) fn handle_getcwd(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (buf, size) = (args[0], args[1]);
    if !super::buffer_ok(buf, size, true, from_user) || size < 2 {
        return err::EINVAL;
    }
    let cwd = crate::sched::current_cwd();
    let path = build_path(&cwd);
    if path.len() + 1 > size {
        return err::ERANGE;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, path.len());
        *((buf + path.len()) as *mut u8) = 0;
    }
    buf as isize
}

fn build_path(inode: &Arc<dyn crate::vfs::Inode>) -> alloc::string::String {
    // Walk up the tree from root to find the path. Simple approach:
    // just return "/" for now — full path resolution needs parent pointers.
    let meta = inode.metadata();
    if meta.ino == 1 {
        return alloc::string::String::from("/");
    }
    alloc::string::String::from("/")
}

/// `fchdir(fd)` — change cwd to the directory named by fd.
pub(super) fn handle_fchdir(args: [usize; super::ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return super::err_of(e),
    };
    if file.metadata().kind != crate::vfs::NodeKind::Dir {
        return err::ENOTDIR;
    }
    crate::sched::set_cwd(file.inode());
    0
}

/// `fcntl(fd, cmd, arg)` — minimal: F_GETFL returns 0, F_SETFL is a no-op.
pub(super) fn handle_fcntl(args: [usize; super::ARG_COUNT]) -> isize {
    let (_fd, cmd, _arg) = (args[0], args[1], args[2]);
    match cmd {
        1 => 0, // F_GETFD
        2 => 0, // F_SETFD
        3 => 0, // F_GETFL
        4 => 0, // F_SETFL
        _ => 0,
    }
}

/// `ioctl(fd, request, ...)` — handle terminal queries, stub the rest.
pub(super) fn handle_ioctl(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (_fd, request) = (args[0] as i32, args[1] as u32);
    match request {
        0x5413 => {
            // TIOCGWINSZ: report terminal size 80x25.
            let buf = args[2];
            if super::buffer_ok(buf, 8, true, from_user) {
                unsafe {
                    (buf as *mut u16).write_volatile(25); // ws_row
                    (buf as *mut u16).add(1).write_volatile(80); // ws_col
                    (buf as *mut u16).add(2).write_volatile(0); // ws_xpixel
                    (buf as *mut u16).add(3).write_volatile(0); // ws_ypixel
                }
                0
            } else {
                err::EFAULT
            }
        }
        0x5401 => {
            // TCGETS: return a minimal termios struct (raw, no echo).
            let buf = args[2];
            if super::buffer_ok(buf, 60, true, from_user) {
                unsafe {
                    core::ptr::write_bytes(buf as *mut u8, 0, 60);
                }
                0
            } else {
                err::EFAULT
            }
        }
        0x5402 => 0, // TCSETS: accept silently
        _ => err::ENOTTY,
    }
}

/// `writev(fd, iov, iovcnt)` — scatter write.
pub(super) fn handle_writev(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16; // sizeof(iovec) = ptr(8) + len(8)
    if !super::buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
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
        if !super::buffer_ok(ptr, len, false, from_user) {
            return err::EFAULT;
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        match crate::vfs::write(fd, bytes) {
            Ok(n) => total += n as isize,
            Err(e) => {
                if total > 0 {
                    return total;
                }
                return super::err_of(e);
            }
        }
    }
    total
}

/// `readv(fd, iov, iovcnt)` — scatter read.
pub(super) fn handle_readv(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16;
    if !super::buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
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
        if !super::buffer_ok(ptr, len, true, from_user) {
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
                return super::err_of(e);
            }
        }
    }
    total
}
