//! Process-related syscall handlers: EXIT, YIELD, GETPID, SLEEP, SPAWN, WAIT, KILL.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::err;
use crate::sched::TaskId;

/// Terminate the current process.
pub(super) fn handle_exit(args: [usize; super::ARG_COUNT]) -> isize {
    crate::sched::exit(args[0] as isize);
}

/// Yield the CPU to the scheduler.
pub(super) fn handle_yield() -> isize {
    crate::sched::yield_now();
    0
}

/// Get the current process ID.
pub(super) fn handle_getpid() -> isize {
    crate::sched::current().0 as isize
}

/// Sleep for N milliseconds.
pub(super) fn handle_sleep(args: [usize; super::ARG_COUNT]) -> isize {
    let ms = args[0] as u64;
    if ms == 0 {
        return 0;
    }
    let ticks = (ms.saturating_mul(crate::arch::timer::TICK_HZ as u64) / 1000).max(1);
    crate::sched::sleep_ticks(ticks);
    0
}

/// Send a signal to a process.
pub(super) fn handle_kill(args: [usize; super::ARG_COUNT]) -> isize {
    let (id, sig) = (args[0], args[1]);
    if sig > 31 {
        return err::EINVAL;
    }
    if crate::sched::send_signal(TaskId(id), sig as u8) {
        0
    } else {
        err::ESRCH
    }
}

/// Load and execute a program.
pub(super) fn handle_spawn(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    let (path_ptr, path_len, blob_ptr, blob_len) = (args[0], args[1], args[2], args[3]);
    let path = match super::path_arg(path_ptr, path_len, from_user) {
        Ok(path) => path,
        Err(e) => return e,
    };

    // The argument blob is NUL-terminated strings packed back to back.
    let mut argv: Vec<String> = Vec::new();
    if blob_ptr != 0 && blob_len != 0 {
        if blob_len > crate::sched::ARGS_BYTES_MAX {
            return err::EINVAL;
        }
        if !super::buffer_ok(blob_ptr, blob_len, false, from_user) {
            return err::EFAULT;
        }
        let bytes = unsafe { core::slice::from_raw_parts(blob_ptr as *const u8, blob_len) };
        let mut rest = bytes;
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            let field = &rest[..end];
            match core::str::from_utf8(field) {
                Ok(s) if !s.is_empty() => argv.push(String::from(s)),
                _ => return err::EINVAL,
            }
            if end == rest.len() {
                break;
            }
            rest = &rest[end + 1..];
        }
        if argv.len() > crate::sched::ARGV_MAX {
            return err::EINVAL;
        }
    }
    if argv.is_empty() {
        let name = path.rsplit('/').next().unwrap_or(&path);
        argv.push(String::from(name));
    }

    // File descriptor redirections: pairs of (from, to).
    let mut redirects: Vec<crate::sched::Redirect> = Vec::new();
    let (redir_ptr, redir_count) = (args[4], args[5]);
    if redir_count > 0 {
        const REDIRECT_MAX: usize = 8;
        if redir_count > REDIRECT_MAX {
            return err::EINVAL;
        }
        let bytes = redir_count * 2 * size_of::<i32>();
        if !super::buffer_ok(redir_ptr, bytes, false, from_user) {
            return err::EFAULT;
        }
        let pairs =
            unsafe { core::slice::from_raw_parts(redir_ptr as *const i32, redir_count * 2) };
        for pair in pairs.as_chunks::<2>().0 {
            let (from, to) = (pair[0], pair[1]);
            if from < 0 || to < 0 {
                return err::EBADF;
            }
            redirects.push((from, to));
        }
    }

    let bytes = match crate::vfs::read_file_opt(&path, Some(cwd.clone())) {
        Ok(bytes) => bytes,
        Err(e) => return super::err_of(e),
    };
    match crate::sched::try_spawn_user(
        &bytes,
        crate::sched::Priority::NORMAL,
        crate::sched::current(),
        &argv,
        &redirects,
    ) {
        Ok(id) => id.0 as isize,
        Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
        Err(crate::sched::SpawnError::ArgsTooBig) => err::EINVAL,
        Err(crate::sched::SpawnError::BadFd) => err::EBADF,
    }
}

/// Wait for a child process.
pub(super) fn handle_wait(args: [usize; super::ARG_COUNT]) -> isize {
    let (pid, flags) = (args[0], args[1]);
    const WNOHANG: usize = 1;
    let no_hang = flags & WNOHANG != 0;
    let me = crate::sched::current();
    if pid == usize::MAX {
        if no_hang {
            return match crate::sched::poll_any_child(me) {
                Some((_id, code)) => code,
                None => err::EAGAIN,
            };
        }
        return match crate::sched::wait_any_child(me) {
            Some((_id, code)) => code,
            None => err::ECHILD,
        };
    }
    if pid == 0 || pid >= crate::sched::MAX_TASKS {
        return err::ECHILD;
    }
    let id = TaskId(pid);
    if crate::sched::is_child(id, me) && !no_hang {
        crate::sched::block_until_code(id);
    }
    match crate::sched::take_exit_code(id) {
        Some(code) => code,
        None if no_hang => err::EAGAIN,
        None => err::ECHILD,
    }
}
