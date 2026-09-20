//! The system-call surface: Linux x86_64 numbers and the dispatch that turns
//! a register block into a call.
//!
//! The register convention is the Linux one: number in `rax`, arguments in
//! `rdi, rsi, rdx, r10, r8, r9`, result in `rax`. `rcx` and `r11` are
//! deliberately absent from the argument list because x86_64's `syscall`
//! instruction destroys them, so a later move from the interrupt gate to
//! that instruction cannot break the convention. Errors come back as
//! negative errno values in `rax`.
//!
//! The dispatch runs the handlers, which need a console and a scheduler
//! under them, so it only exists on the machine. The Linux module's *number
//! table* and its pure *ABI half* (`linux::abi`: flag values, struct
//! layouts, argument shaping) compile on the host too - that is where the
//! man-page contract is unit-tested, and where a change to a flag bit or a
//! struct offset reads as a red test instead of a mystery on the machine.

/// Linux x86_64 syscall number mapping and POSIX handlers.
pub mod linux;

/// Base address for user mmap: where every process's mmap cursor starts.
///
/// The cursor only moves downward from here, so the mmap region grows from
/// the top of the address space toward the program image. That shape -
/// heap low, huge hole, mmap high - is what Linux gives a static program,
/// and musl's allocator notices the difference: it sizes its regions from
/// what the kernel hands back, and a base in the low gigabytes makes those
/// regions collide with the break.
pub const MMAP_BASE: usize = 0x0000_7f00_0000_0000;

/// The errno values [`linux::dispatch`] returns, already negated for `rax`.
pub mod err {
    pub const ENOENT: isize = -2;
    pub const ESRCH: isize = -3;
    /// EIO: an I/O error. `ptrace` answers it for a request the kernel does
    /// not know or a tracee address it cannot reach, which is exactly how Linux
    /// spells both - and a debugger distinguishes them from `EFAULT`, which
    /// means the *caller's* pointer was bad.
    pub const EIO: isize = -5;
    pub const ENXIO: isize = -6;
    /// ECANCELED: the operation was undone by something outside it. The
    /// one user is a `TFD_TIMER_CANCEL_ON_SET` timer whose real-time clock
    /// was set underneath it - timerfd_create(2) says the next read(2) fails
    /// with exactly this, and a caller distinguishes it from EINVAL, which
    /// would mean the buffer it handed over was the problem.
    pub const ECANCELED: isize = -125;
    /// EACCES: permission denied. What `access(2)`'s family answers for the
    /// one check this kernel can refuse - an execute asked of a file whose
    /// stored mode has no execute bit - which is the question a shell's
    /// `test -x` is really asking.
    pub const EACCES: isize = -13;
    pub const ENOEXEC: isize = -8;
    pub const EBADF: isize = -9;
    pub const ECHILD: isize = -10;
    pub const EFAULT: isize = -14;
    pub const EBUSY: isize = -16;
    pub const EEXIST: isize = -17;
    /// EAGAIN: try again later. What a `wait` that was asked not to block
    /// answers when the child it was asked about is still running.
    pub const EAGAIN: isize = -11;
    pub const ENOTDIR: isize = -20;
    /// ENODEV: no such device. What an `mmap` that asked for a mapping mpos
    /// has no way to build - a file-backed one - is answered with.
    pub const ENODEV: isize = -19;
    pub const EISDIR: isize = -21;
    pub const EINVAL: isize = -22;
    pub const ENOMEM: isize = -12;
    pub const EFBIG: isize = -27;
    pub const ENOSPC: isize = -28;
    pub const ERANGE: isize = -34;
    pub const EPIPE: isize = -32;
    pub const ENOSYS: isize = -38;
    pub const EPERM: isize = -1;
    pub const EOPNOTSUPP: isize = -95;
    pub const EROFS: isize = -30;
    /// EXDEV: a hard link across two filesystems. The one request `link(2)`
    /// cannot honor by design, and the answer that tells a caller to fall
    /// back to copy-and-delete.
    pub const EXDEV: isize = -18;
    pub const ENOIOCTLCMD: isize = -515;
    pub const EWOULDBLOCK: isize = -11;
    pub const EINTR: isize = -4;
    pub const ENOTTY: isize = -25;
    pub const E2BIG: isize = -7;
    pub const ENAMETOOLONG: isize = -36;
    /// ELOOP: a chain of `#!` interpreters that leads back into itself. The
    /// bound that produces it is what ends the chain; nothing else can.
    pub const ELOOP: isize = -40;
    /// ENOTBLK: a mount source that is not a block device.
    pub const ENOTBLK: isize = -15;
    /// ENOTEMPTY: `rmdir` on a directory that still names something. Linux
    /// has a separate call from `unlink` precisely so that this can be said
    /// instead of "the directory was removed, and so was everything in it".
    pub const ENOTEMPTY: isize = -39;
    // --- socket family ---
    /// EMSGSIZE: a datagram is larger than the socket can carry as one packet.
    pub const EMSGSIZE: isize = -90;
    /// EDESTADDRREQ: a send on an unconnected datagram socket named no address.
    pub const EDESTADDRREQ: isize = -89;
    /// ENOTSOCK: the descriptor names something that is not a socket.
    pub const ENOTSOCK: isize = -88;
    /// ENOPROTOOPT: the option level or name is not one the kernel knows.
    pub const ENOPROTOOPT: isize = -92;
    /// EPROTONOSUPPORT: the protocol number does not match the socket type.
    pub const EPROTONOSUPPORT: isize = -93;
    /// ESOCKTNOSUPPORT: the socket type itself is one the kernel does not know.
    pub const ESOCKTNOSUPPORT: isize = -94;
    /// EAFNOSUPPORT: the address family is not one this kernel speaks. What
    /// musl's syslog gets for its `AF_UNIX` probe, so it falls back to writing
    /// the console directly.
    pub const EAFNOSUPPORT: isize = -97;
    /// EADDRINUSE: the local address is already bound by another socket.
    pub const EADDRINUSE: isize = -98;
    /// EADDRNOTAVAIL: the requested local address cannot be assigned here.
    pub const EADDRNOTAVAIL: isize = -99;
    /// ENETDOWN: the network interface is not up.
    pub const ENETDOWN: isize = -100;
    /// ENETUNREACH: the network named is not reachable from here. What the
    /// IPv6 stub answers at `connect`, so a caller walking a getaddrinfo
    /// list falls through to the IPv4 answer the way Linux lets it.
    pub const ENETUNREACH: isize = -101;
    /// ENOTCONN: the socket is not connected, and the call needs it to be.
    pub const ENOTCONN: isize = -107;
    /// EISCONN: the socket is already connected, and `connect` was asked again.
    pub const EISCONN: isize = -106;
    /// ESHUTDOWN: the socket has been shut down for that direction.
    pub const ESHUTDOWN: isize = -108;
    /// ETIMEDOUT: the peer never answered in time.
    pub const ETIMEDOUT: isize = -110;
    /// ECONNREFUSED: the peer reset the connection attempt.
    pub const ECONNREFUSED: isize = -111;
    /// EALREADY: a nonblocking connect is still in flight.
    pub const EALREADY: isize = -114;
    /// EINPROGRESS: a nonblocking connect has been started and is not done.
    pub const EINPROGRESS: isize = -115;
}

/// How many register arguments a syscall may pass.
pub const ARG_COUNT: usize = 6;

/// Upper bound on a single read or write.
pub(crate) const MAX_IO: usize = 1 << 20;

/// Maximum length of a path argument.
pub(crate) const MAX_PATH: usize = 255;

/// Map a VFS error onto the errno [`linux::dispatch`] returns.
#[cfg(target_os = "none")]
pub(crate) fn err_of(e: crate::vfs::VfsError) -> isize {
    use crate::vfs::VfsError;
    match e {
        VfsError::NoEntry => err::ENOENT,
        VfsError::NotDir => err::ENOTDIR,
        VfsError::IsDir => err::EISDIR,
        VfsError::Exists => err::EEXIST,
        VfsError::NotEmpty => err::ENOTEMPTY,
        VfsError::Busy => err::EBUSY,
        VfsError::BadFd => err::EBADF,
        VfsError::NoRoot | VfsError::Invalid => err::EINVAL,
        VfsError::NoSpace => err::ENOSPC,
        VfsError::FileTooBig => err::EFBIG,
        VfsError::BrokenPipe => err::EPIPE,
        VfsError::Unsupported => err::EOPNOTSUPP,
        // EROFS: what Linux answers a write to a filesystem mounted
        // read-only, and what userland checks for before remounting.
        VfsError::ReadOnly => err::EROFS,
        // EIO: damage the filesystem can see. Reported rather than panicked on
        // for the same reason Linux reports it - a caller can be told, and a
        // machine that stops on one bad directory entry cannot tell anyone.
        VfsError::Corrupt => err::EIO,
        // EXDEV: a hard link whose two ends live on different filesystems.
        VfsError::CrossDevice => err::EXDEV,
        // EAGAIN, which is also EWOULDBLOCK: they are one errno here.
        VfsError::Again => err::EWOULDBLOCK,
        // ECANCELED: a timerfd whose CANCEL_ON_SET clock was set - the
        // read that would have counted its expirations reports the undoing
        // instead, which is timerfd_create(2)'s wording of the case.
        VfsError::Canceled => err::ECANCELED,
        // ENXIO: the device's name answers nothing for this caller. `/dev/tty`
        // without a controlling terminal is the whole case - POSIX's own
        // answer, and the one a shell reads as "job control off".
        VfsError::NoDevice => err::ENXIO,
    }
}

/// Validate a user buffer: length within bounds and pages accessible.
pub(crate) fn buffer_ok(buf: usize, len: usize, write: bool, from_user: bool) -> bool {
    len <= MAX_IO && (!from_user || user_accessible(buf, len, write))
}

/// The page-walk half of [`buffer_ok`]: is the range mapped and, for a read
/// performed by the kernel on the caller's behalf, writable?
///
/// On the machine this is the page table's answer. The host build has no
/// page tables - the tests that reach this layer hand it real memory and
/// call with `from_user = false` where the distinction matters - so the stub
/// answers "accessible" and the length bound above stays the real gate.
#[cfg(target_os = "none")]
fn user_accessible(buf: usize, len: usize, write: bool) -> bool {
    crate::arch::paging::user_accessible(buf, len, write)
}

#[cfg(not(target_os = "none"))]
fn user_accessible(_buf: usize, _len: usize, _write: bool) -> bool {
    true
}

/// Parse a NUL-terminated string argument - the Linux ABI's path shape - into
/// an owned String.
///
/// The string's own NUL is the length word, so the kernel cannot know up
/// front how far the caller means it to run. The scan therefore validates one
/// page at a time and stops at the first terminator, which keeps a short
/// string that ends inside its last mapped page from being rejected for the
/// unmapped bytes beyond it. A string with no NUL within [`MAX_PATH`]
/// characters answers `ENAMETOOLONG`, the errno Linux uses for exactly that.
pub(crate) fn cstr_arg(ptr: usize, from_user: bool) -> Result<alloc::string::String, isize> {
    cstr_limit(ptr, from_user, MAX_PATH)
}

/// [`cstr_arg`] for the strings of an `execve`'s `envp` and `argv` arrays,
/// whose limit is not a path's. Linux caps a *pathname* at 255 bytes per
/// component, but an environment string is only bounded by the argument
/// block's total - `COLLECT_GCC_OPTIONS` and friends from a real compiler
/// run past 255 bytes easily - and refusing one is what makes `gcc` fail
/// with `posix_spawn: Filename too long` before it ever runs a subprocess.
pub(crate) fn cstr_env(ptr: usize, from_user: bool) -> Result<alloc::string::String, isize> {
    cstr_limit(ptr, from_user, MAX_ARG_STR)
}

/// The per-string ceiling of `execve`'s arrays. Linux's own limit is
/// `MAX_ARG_STRLEN` = 32 pages minus a byte - 131071 - for each of argv and
/// envp; this takes the same shape at a scale the argument block can hold.
pub(crate) const MAX_ARG_STR: usize = 4095;

fn cstr_limit(ptr: usize, from_user: bool, limit: usize) -> Result<alloc::string::String, isize> {
    if ptr == 0 {
        return Err(err::EFAULT);
    }
    let mut bytes = alloc::vec::Vec::new();
    let mut p = ptr;
    loop {
        // A page-table entry covers a whole page, so validating the first
        // byte of one validates every byte of it.
        if (p & 0xfff == 0 || p == ptr) && !buffer_ok(p, 1, false, from_user) {
            return Err(err::EFAULT);
        }
        let b = unsafe { (p as *const u8).read_unaligned() };
        if b == 0 {
            break;
        }
        bytes.push(b);
        if bytes.len() > limit {
            return Err(err::ENAMETOOLONG);
        }
        p += 1;
    }
    core::str::from_utf8(&bytes)
        .map(alloc::string::String::from)
        .map_err(|_| err::EINVAL)
}

/// Parse a NULL-terminated array of NUL-terminated strings - the Linux ABI's
/// argv shape - into an owned vector. An absent array (`NULL`) is empty.
/// `limit` is the per-string ceiling: [`MAX_PATH`] for the `argv` a caller
/// spells as paths, [`MAX_ARG_STR`] for the environment.
pub(crate) fn argv_arg(
    ptr: usize,
    limit: usize,
) -> Result<alloc::vec::Vec<alloc::string::String>, isize> {
    if ptr == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let mut out = alloc::vec::Vec::new();
    let mut p = ptr;
    loop {
        if !buffer_ok(p, core::mem::size_of::<usize>(), false, true) {
            return Err(err::EFAULT);
        }
        let entry = unsafe { (p as *const usize).read_unaligned() };
        if entry == 0 {
            break;
        }
        if out.len() >= crate::sched::ARGV_MAX {
            return Err(err::E2BIG);
        }
        out.push(cstr_limit(entry, true, limit)?);
        p += core::mem::size_of::<usize>();
    }
    Ok(out)
}

/// Dispatch a Linux x86_64 syscall number - the numbers a musl binary
/// speaks, in `src/syscall/linux/nr.rs`.
///
/// `from_user` is `true` when the dispatch was reached through the
/// `syscall` gate from ring 3 (the normal case) and `false` when the
/// kernel is calling a handler on its own behalf: the gate validates
/// user pointers, and the kernel's own buffers do not need that walk.
///
/// `regs_ptr` is the raw pointer to the register frame on the kernel
/// stack, used by execve to patch the return address and stack pointer
/// so the process enters the new image on `sysretq`.
#[cfg(target_os = "none")]
pub fn linux_dispatch(
    nr: u64,
    args: [usize; ARG_COUNT],
    from_user: bool,
    regs_ptr: *mut (),
) -> isize {
    // Fire an interval timer that came due while the task was in ring 3. The
    // clock covers a spinning task and the scheduler covers an idle core;
    // this covers the third state - a task in and out of syscalls - so no
    // caller waits a whole slice past its deadline.
    crate::sched::check_itimer();
    // The two moments a tracer wants to see: the call before it is made and its
    // result before it is returned. Both are ordinary stops in the task table,
    // and both are no-ops - one branch on a flag - for every task no tracer has
    // touched, which is every task on a machine without a tracer running.
    let args = linux::proc::syscall_entry_stop(regs_ptr, nr, args);
    let ret = linux::dispatch(nr, args, from_user, regs_ptr);
    let ret = linux::proc::syscall_exit_stop(regs_ptr, nr, args, ret);
    // Signals are delivered here rather than before the call: a handler has to
    // see the *result* of the syscall it interrupted, and the result does not
    // exist until the call has returned. Jumping into a handler means
    // rewriting the frame the task will `sysretq` through, which is why this
    // is also the only delivery point that can run one that needs a frame.
    if linux::deliver_signal(regs_ptr, ret) {
        // The interrupted call's value belongs to the handler now: it is on
        // the frame, waiting for `rt_sigreturn` to put it back.
        return 0;
    }
    ret
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::err;

    #[test]
    fn errors_match_the_errno_values_they_name() {
        assert_eq!(err::ENOENT, -2);
        assert_eq!(err::ESRCH, -3);
        assert_eq!(err::ENOEXEC, -8);
        assert_eq!(err::EBADF, -9);
        assert_eq!(err::ECHILD, -10);
        assert_eq!(err::EAGAIN, -11);
        assert_eq!(err::EFAULT, -14);
        assert_eq!(err::EBUSY, -16);
        assert_eq!(err::EEXIST, -17);
        assert_eq!(err::ENOTDIR, -20);
        assert_eq!(err::EISDIR, -21);
        assert_eq!(err::EINVAL, -22);
        assert_eq!(err::EFBIG, -27);
        assert_eq!(err::ENOSPC, -28);
        assert_eq!(err::EPIPE, -32);
        assert_eq!(err::ENOSYS, -38);
    }
}
