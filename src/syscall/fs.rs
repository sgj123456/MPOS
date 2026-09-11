//! File-related syscall handlers: READ, WRITE, OPEN, CLOSE, CREATE, UNLINK,
//! LISTDIR, DUP2, CHDIR, PIPE.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::err;

/// Read from a file descriptor.
pub(super) fn handle_read(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
) -> isize {
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
pub(super) fn handle_write(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
) -> isize {
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
pub(super) fn handle_chdir(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
) -> isize {
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
pub(super) fn handle_pipe(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
) -> isize {
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
