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
