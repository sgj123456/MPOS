//! The system-call surface: a number table the libc will pin, and the dispatch
//! that turns a register block into a call.
//!
//! The numbers are mpos's own. POSIX fixes interfaces, not numbers, so the
//! table is free to stay small and dense. The register convention, though, is
//! worth borrowing: number in `rax`, arguments in `rdi, rsi, rdx, r10, r8, r9`,
//! result in `rax`. `rcx` and `r11` are deliberately absent from the argument
//! list because x86_64's `syscall` instruction destroys them, so a later move
//! from the interrupt gate to that instruction cannot break the convention.
//! Errors come back as negative errno values in `rax`.
//!
//! The table itself is arch-agnostic and unit-tested on the host. [`dispatch`]
//! runs the handlers, which need a console and a scheduler under them, so it
//! only exists on the machine.

/// The syscall numbers, in a file of their own so user programs can include it.
pub mod nr;

/// Linux x86_64 syscall number mapping.
#[cfg(target_os = "none")]
pub mod linux;

/// File-related syscall handlers (READ, WRITE, OPEN, CLOSE, CREATE, UNLINK,
/// LISTDIR, DUP2, CHDIR, PIPE).
#[cfg(target_os = "none")]
pub mod fs;

/// Process-related syscall handlers (EXIT, YIELD, GETPID, SLEEP, SPAWN, WAIT,
/// KILL).
#[cfg(target_os = "none")]
pub mod proc;

/// The errno values [`dispatch`] returns, already negated for `rax`.
pub mod err {
    pub const ENOENT: isize = -2;
    pub const ESRCH: isize = -3;
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
    pub const ENOIOCTLCMD: isize = -515;
    pub const EWOULDBLOCK: isize = -11;
    pub const EINTR: isize = -4;
    pub const ENOTTY: isize = -25;
    pub const E2BIG: isize = -7;
    pub const ENAMETOOLONG: isize = -36;
}

/// How many register arguments a syscall may pass.
pub const ARG_COUNT: usize = 6;

/// Upper bound on a single read or write.
#[cfg(target_os = "none")]
pub(crate) const MAX_IO: usize = 1 << 20;

/// Maximum length of a path argument.
#[cfg(target_os = "none")]
pub(crate) const MAX_PATH: usize = 255;

/// Map a VFS error onto the errno [`dispatch`] returns.
#[cfg(target_os = "none")]
pub(crate) fn err_of(e: crate::vfs::VfsError) -> isize {
    use crate::vfs::VfsError;
    match e {
        VfsError::NoEntry => err::ENOENT,
        VfsError::NotDir => err::ENOTDIR,
        VfsError::IsDir => err::EISDIR,
        VfsError::Exists => err::EEXIST,
        VfsError::Busy => err::EBUSY,
        VfsError::BadFd => err::EBADF,
        VfsError::NoRoot | VfsError::Invalid => err::EINVAL,
        VfsError::NoSpace => err::ENOSPC,
        VfsError::FileTooBig => err::EFBIG,
        VfsError::BrokenPipe => err::EPIPE,
        VfsError::Unsupported => err::ENOSYS,
    }
}

/// Validate a user buffer: length within bounds and pages accessible.
#[cfg(target_os = "none")]
pub(crate) fn buffer_ok(buf: usize, len: usize, write: bool, from_user: bool) -> bool {
    len <= MAX_IO && (!from_user || crate::arch::paging::user_accessible(buf, len, write))
}

/// Parse a path argument from a (ptr, len) pair into an owned String.
#[cfg(target_os = "none")]
pub(crate) fn path_arg(
    ptr: usize,
    len: usize,
    from_user: bool,
) -> Result<alloc::string::String, isize> {
    if len == 0 || len > MAX_PATH {
        return Err(err::EINVAL);
    }
    if !buffer_ok(ptr, len, false, from_user) {
        return Err(err::EFAULT);
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    core::str::from_utf8(bytes)
        .map(alloc::string::String::from)
        .map_err(|_| err::EINVAL)
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
#[cfg(target_os = "none")]
pub(crate) fn cstr_arg(ptr: usize, from_user: bool) -> Result<alloc::string::String, isize> {
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
        let b = unsafe { (p as *const u8).read_volatile() };
        if b == 0 {
            break;
        }
        bytes.push(b);
        if bytes.len() > MAX_PATH {
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
#[cfg(target_os = "none")]
pub(crate) fn argv_arg(ptr: usize) -> Result<alloc::vec::Vec<alloc::string::String>, isize> {
    if ptr == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let mut out = alloc::vec::Vec::new();
    let mut p = ptr;
    loop {
        if !buffer_ok(p, core::mem::size_of::<usize>(), false, true) {
            return Err(err::EFAULT);
        }
        let entry = unsafe { (p as *const usize).read_volatile() };
        if entry == 0 {
            break;
        }
        if out.len() >= crate::sched::ARGV_MAX {
            return Err(err::E2BIG);
        }
        out.push(cstr_arg(entry, true)?);
        p += core::mem::size_of::<usize>();
    }
    Ok(out)
}

/// Dispatch an mpos syscall number - the kernel's own table, the numbers in
/// [`nr`]. Linux numbers go to [`linux_dispatch`]; the `syscall` gate tells
/// the two apart by the "mpos" tag in the number's top 32 bits.
#[cfg(target_os = "none")]
pub fn mpos_dispatch(nr: u64, args: [usize; ARG_COUNT], from_user: bool) -> isize {
    // A signal that arrived while the task was running is delivered before it
    // gets to ask for anything.
    crate::sched::deliver_pending_signal();

    // Where a caller's relative paths start.
    let cwd = crate::sched::current_cwd();

    match nr {
        // -- process --
        nr::EXIT => proc::handle_exit(args),
        nr::YIELD => proc::handle_yield(),
        nr::GETPID => proc::handle_getpid(),
        nr::SLEEP => proc::handle_sleep(args),
        nr::KILL => proc::handle_kill(args),
        nr::SPAWN => proc::handle_spawn(args, from_user, &cwd),
        nr::WAIT => proc::handle_wait(args),

        // -- file system --
        nr::READ => fs::handle_read(args, from_user),
        nr::WRITE => fs::handle_write(args, from_user),
        nr::OPEN => fs::handle_open(args, from_user, &cwd),
        nr::CLOSE => fs::handle_close(args),
        nr::CREATE => fs::handle_create(args, from_user, &cwd),
        nr::UNLINK => fs::handle_unlink(args, from_user, &cwd),
        nr::CHDIR => fs::handle_chdir(args, from_user),
        nr::DUP2 => fs::handle_dup2(args),
        nr::LISTDIR => fs::handle_listdir(args, from_user, &cwd),
        nr::PIPE => fs::handle_pipe(args, from_user),

        _ => err::ENOSYS,
    }
}

/// Dispatch a Linux x86_64 syscall number - the numbers a musl binary
/// speaks, in `src/syscall/linux/nr.rs`. mpos's own numbers go to
/// [`mpos_dispatch`].
#[cfg(target_os = "none")]
pub fn linux_dispatch(nr: u64, args: [usize; ARG_COUNT]) -> isize {
    crate::sched::deliver_pending_signal();
    linux::dispatch(nr, args)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{err, nr};

    #[test]
    fn numbers_are_pinned() {
        assert_eq!(nr::EXIT, 0);
        assert_eq!(nr::READ, 1);
        assert_eq!(nr::WRITE, 2);
        assert_eq!(nr::YIELD, 3);
        assert_eq!(nr::GETPID, 4);
        assert_eq!(nr::SLEEP, 5);
        assert_eq!(nr::PIPE, 6);
        assert_eq!(nr::KILL, 7);
        assert_eq!(nr::OPEN, 8);
        assert_eq!(nr::CLOSE, 9);
        assert_eq!(nr::SPAWN, 10);
        assert_eq!(nr::WAIT, 11);
        assert_eq!(nr::LISTDIR, 12);
        assert_eq!(nr::DUP2, 13);
        assert_eq!(nr::CREATE, 14);
        assert_eq!(nr::UNLINK, 15);
        assert_eq!(nr::CHDIR, 16);
    }

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
