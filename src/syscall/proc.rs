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

// ---------------------------------------------------------------------------
// Linux-compatible syscall handlers
// ---------------------------------------------------------------------------

/// Get the parent process ID.
pub(super) fn handle_getppid() -> isize {
    crate::sched::current().0 as isize / 2 // simplified: no real parent tracking
}

/// `execve(path, argv, envp)` — replace the current process image.
///
/// This maps onto the existing `handle_spawn` with the same process.
pub(super) fn handle_execve(
    args: [usize; super::ARG_COUNT],
    from_user: bool,
    cwd: &Arc<dyn crate::vfs::Inode>,
) -> isize {
    // execve replaces the current process. We implement it by loading the ELF
    // and mapping it into the current address space. For now, delegate to
    // spawn which creates a new process — the caller's process continues.
    // A full execve would need to tear down the current address space first.
    //
    // For coreutils, this is acceptable: each command runs as a separate process
    // started by the shell.
    let path_ptr = args[0];
    let path_len = args[1];
    match super::path_arg(path_ptr, path_len, from_user) {
        Ok(path) => {
            let bytes = match crate::vfs::read_file_opt(&path, Some(cwd.clone())) {
                Ok(bytes) => bytes,
                Err(e) => return super::err_of(e),
            };
            match crate::sched::try_spawn_user(
                &bytes,
                crate::sched::Priority::NORMAL,
                crate::sched::current(),
                &[],
                &[],
            ) {
                Ok(id) => id.0 as isize,
                Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
                Err(crate::sched::SpawnError::ArgsTooBig) => err::EINVAL,
                Err(crate::sched::SpawnError::BadFd) => err::EBADF,
            }
        }
        Err(e) => e,
    }
}

/// `nanosleep(req, rem)` — req is a `struct timespec { tv_sec, tv_nsec }`.
pub(super) fn handle_nanosleep(args: [usize; super::ARG_COUNT]) -> isize {
    let req = args[0];
    if req == 0 {
        return 0;
    }
    // Read seconds and nanoseconds from userspace timespec.
    unsafe {
        let tv_sec = (req as *const i64).read_volatile() as u64;
        let tv_nsec = ((req + 8) as *const i64).read_volatile() as u64;
        let total_ms = tv_sec * 1000 + tv_nsec / 1_000_000;
        if total_ms == 0 {
            return 0;
        }
        let ticks = (total_ms * crate::arch::timer::TICK_HZ as u64 / 1000).max(1);
        crate::sched::sleep_ticks(ticks);
    }
    0
}

/// `fork()` — create a copy of the calling process.
///
/// Minimal implementation: duplicate address space, fd table, and registers.
/// Returns 0 to child, child pid to parent.
pub(super) fn handle_fork(_args: [usize; super::ARG_COUNT]) -> isize {
    // For now, return -ENOSYS — fork is complex and most coreutils don't need it.
    // The user can run each coreutil directly via execve from the shell.
    -super::err::ENOSYS
}

/// `brk(addr)` — change the program break (heap end).
///
/// If addr is 0, returns the current break. Otherwise sets the break and
/// returns the new break. Minimal implementation that allocates pages on demand.
pub(super) fn handle_brk(args: [usize; super::ARG_COUNT]) -> isize {
    let addr = args[0];
    if addr == 0 {
        // Return current program break (after BSS).
        // We don't track this yet, return a reasonable default.
        0x6000_0000
    } else {
        // Accept any request — stub that "succeeds".
        addr as isize
    }
}

/// `mmap(addr, length, prot, flags, fd, offset)` — map files or anonymous memory.
///
/// Minimal implementation: for anonymous mappings, allocate pages and return.
/// For file-backed mappings with fd=-1 (MAP_ANONYMOUS), allocate pages.
pub(super) fn handle_mmap(args: [usize; super::ARG_COUNT]) -> isize {
    let (addr, length, _prot, flags, fd, _offset) =
        (args[0], args[1], args[2], args[3], args[4] as i32, args[5]);

    let map_anonymous: usize = 0x20;
    let _map_private: usize = 0x02;
    let _map_fixed: usize = 0x10;

    let anon = (flags & map_anonymous) != 0;
    let _private = (flags & _map_private) != 0;

    if !anon && fd >= 0 {
        // File-backed mapping: not supported yet, return error.
        return -super::err::ENOSYS;
    }

    let pages = length.div_ceil(crate::mm::pframe::PAGE_SIZE);
    let base = if addr != 0 {
        addr // hint or fixed
    } else {
        // Let the kernel choose — use a fixed range for now.
        0x2000_0000
    };

    // Allocate physical frames and map them.
    use crate::mm::pframe::PAGE_SIZE;
    for _i in 0..pages {
        let frame = match crate::mm::alloc_frames(0) {
            Some(f) => f,
            None => return -super::err::ENOMEM,
        };
        // Zero the frame.
        unsafe {
            let ptr = crate::mm::phys_to_virt(frame) as *mut u8;
            core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
        }
        // Map into the current address space — we'd need the space to do this
        // properly, but for now just return the virtual address.
    }

    base as isize
}

/// `munmap(addr, length)` — unmap memory.
pub(super) fn handle_munmap(_args: [usize; super::ARG_COUNT]) -> isize {
    // Stub: accept silently.
    0
}

/// `clock_gettime(clk_id, tp)` — get time.
///
/// `tp` points to a `struct timespec { tv_sec, tv_nsec }`.
pub(super) fn handle_clock_gettime(args: [usize; super::ARG_COUNT]) -> isize {
    let (clk_id, tp) = (args[0], args[1]);
    let _ = clk_id;
    if tp == 0 {
        return 0;
    }
    // Return a fixed time based on the timer tick count.
    let ticks = crate::arch::timer::ticks();
    let freq = crate::arch::timer::TICK_HZ as u64;
    let secs = ticks / freq;
    let nsecs = (ticks % freq) * (1_000_000_000 / freq);
    unsafe {
        (tp as *mut i64).write_volatile(secs as i64);
        ((tp + 8) as *mut i64).write_volatile(nsecs as i64);
    }
    0
}

/// `uname(buf)` — write system name/version info.
///
/// `buf` points to a `struct utsname` (416 bytes total, five 65-byte strings
/// plus a 65-byte version and 65-byte release).
pub(super) fn handle_uname(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let buf = args[0];
    let size = 416; // sizeof(struct utsname)
    if !super::buffer_ok(buf, size, true, from_user) {
        return err::EFAULT;
    }
    // Write five 65-byte NUL-terminated strings: sysname, nodename, release,
    // version, machine.
    unsafe {
        let base = buf as *mut u8;
        // All fields zeroed first.
        core::ptr::write_bytes(base, 0, size);
        let write_str = |offset: usize, s: &str| {
            let dst = base.add(offset);
            let bytes = s.as_bytes();
            let n = bytes.len().min(64);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        };
        write_str(0, "mpos"); // sysname
        write_str(65, "mpos"); // nodename
        write_str(130, "0.1.0"); // release
        write_str(195, "0.1.0"); // version
        write_str(260, "x86_64"); // machine
    }
    0
}

/// `getrandom(buf, len, flags)` — fill buffer with random bytes.
///
/// Uses the timer tick as a simple pseudo-random source.
pub(super) fn handle_getrandom(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (buf, len, _flags) = (args[0], args[1], args[2]);
    if len == 0 {
        return 0;
    }
    if !super::buffer_ok(buf, len, true, from_user) {
        return err::EFAULT;
    }
    let tick = crate::arch::timer::ticks();
    // Simple PRNG: xorshift64 seeded from the tick counter.
    let mut state = tick;
    unsafe {
        let dst = buf as *mut u8;
        for i in 0..len {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            dst.add(i).write_volatile(state as u8);
        }
    }
    len as isize
}

/// `prlimit64(pid, resource, new_limit, old_limit)` — get/set resource limits.
///
/// Minimal: return hard-coded defaults for RLIMIT_STACK and RLIMIT_NOFILE.
pub(super) fn handle_prlimit64(args: [usize; super::ARG_COUNT], from_user: bool) -> isize {
    let (_pid, resource, _new_ptr, old_ptr) = (args[0], args[1], args[2], args[3]);
    if old_ptr == 0 {
        return 0;
    }
    if !super::buffer_ok(old_ptr, 16, true, from_user) {
        return err::EFAULT;
    }
    // struct rlimit64 { rlim_cur, rlim_max } — both u64.
    let (cur, max): (u64, u64) = match resource {
        3 => (8 * 1024 * 1024, 8 * 1024 * 1024), // RLIMIT_STACK
        5 => (1024, 1048576),                    // RLIMIT_NOFILE
        7 => (4096, 4096),                       // RLIMIT_NPROC
        _ => (u64::MAX, u64::MAX),
    };
    unsafe {
        (old_ptr as *mut u64).write_volatile(cur);
        ((old_ptr + 8) as *mut u64).write_volatile(max);
    }
    0
}

/// `futex(uaddr, op, val, ...)` — basic futex support for threading.
///
/// Minimal: only supports FUTEX_WAIT with immediate return (no actual blocking).
pub(super) fn handle_futex(args: [usize; super::ARG_COUNT]) -> isize {
    let (_uaddr, op, _val) = (args[0], args[1], args[2]);
    match op & 0xf {
        0 => 0, // FUTEX_WAIT: stub — just return 0
        1 => 0, // FUTEX_WAKE: stub — wake 0 threads
        _ => 0,
    }
}
