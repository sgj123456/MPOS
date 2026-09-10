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
    pub const ENOTDIR: isize = -20;
    pub const EISDIR: isize = -21;
    pub const EINVAL: isize = -22;
    pub const EFBIG: isize = -27;
    pub const ENOSPC: isize = -28;
    pub const EPIPE: isize = -32;
    pub const ENOSYS: isize = -38;
}

/// How many register arguments a syscall may pass.
pub const ARG_COUNT: usize = 6;

/// The directory a path names its last component in, and that component.
///
/// Everything above the last slash is the parent, which the caller resolves;
/// what follows is the name to make or remove. `/f` is the root and `f`.
///
/// A name is never a path component - `.` and `..` name directories that
/// already exist, and neither can be made or removed by this route. A path
/// with no slash at all has no parent to speak of: there is no cwd to resolve
/// it against, so it is refused rather than guessed at.
///
/// Returns the errno, not a VFS error, because it is only ever called to
/// check an argument from ring 3.
fn split_parent(path: &str) -> Result<(&str, &str), isize> {
    let trimmed = path.trim_end_matches('/');
    let Some(slash) = trimmed.rfind('/') else {
        return Err(err::EINVAL);
    };
    let name = &trimmed[slash + 1..];
    if name.is_empty() || name == "." || name == ".." {
        return Err(err::EINVAL);
    }
    let parent = if slash == 0 { "/" } else { &trimmed[..slash] };
    Ok((parent, name))
}

/// Run syscall number `nr` with the register arguments and return the result.
///
/// `from_user` says whether the gate was knocked on from ring 3, which decides
/// how much a pointer argument can be trusted: a kernel task's pointers are
/// kernel pointers, a user task's are only addresses until every page they
/// touch has been walked in the caller's own tables.
///
/// `EXIT` never returns: the task ends inside the gate.
#[cfg(target_os = "none")]
pub fn dispatch(nr: u64, args: [usize; ARG_COUNT], from_user: bool) -> isize {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::sched::TaskId;
    use crate::vfs::VfsError;

    // Upper bound on a single read or write, so a bad length is an error
    // rather than a run off the end of memory.
    const MAX_IO: usize = 1 << 20;

    // Map a VFS error onto the errno `dispatch` returns. The kernel's own
    // callers see the rich value; ring 3 sees only the number.
    fn err_of(e: VfsError) -> isize {
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

    // Every buffer syscall starts the same way: trust the length no further
    // than MAX_IO and, from user mode, no further than the caller's own page
    // tables. `write` says whether the pages must be user-writable.
    fn buffer_ok(buf: usize, len: usize, write: bool, from_user: bool) -> bool {
        len <= MAX_IO && (!from_user || crate::arch::paging::user_accessible(buf, len, write))
    }

    // A path argument: bytes at (ptr, len), length-capped, owned text on the
    // way out. Everything past this point trusts it as a path.
    const MAX_PATH: usize = 255;
    fn path_arg(ptr: usize, len: usize, from_user: bool) -> Result<String, isize> {
        if len == 0 || len > MAX_PATH {
            return Err(err::EINVAL);
        }
        if !buffer_ok(ptr, len, false, from_user) {
            return Err(err::EFAULT);
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| err::EINVAL)
    }

    // A signal that arrived while the task was running is delivered before it
    // gets to ask for anything: the same rule as `block_until`, applied at the
    // other door into the kernel.
    crate::sched::deliver_pending_signal();

    match nr {
        nr::EXIT => crate::sched::exit(args[0] as isize),
        nr::READ => {
            let (fd, buf, len) = (args[0], args[1], args[2]);
            if !buffer_ok(buf, len, true, from_user) {
                return if len > MAX_IO {
                    err::EINVAL
                } else {
                    err::EFAULT
                };
            }
            // An empty read touches no memory but still names an fd. fd 0 is
            // the tty and blocks for a line; fd 1 and 2 are the console.
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
        nr::WRITE => {
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
        nr::YIELD => {
            crate::sched::yield_now();
            0
        }
        nr::GETPID => crate::sched::current().0 as isize,
        nr::SLEEP => {
            let ms = args[0] as u64;
            if ms == 0 {
                return 0;
            }
            let ticks = (ms.saturating_mul(crate::arch::timer::TICK_HZ as u64) / 1000).max(1);
            crate::sched::sleep_ticks(ticks);
            0
        }
        nr::PIPE => {
            // args[0] names two i32 slots: the read end, then the write end.
            let out = args[0];
            if out == 0 || !buffer_ok(out, 2 * core::mem::size_of::<i32>(), true, from_user) {
                return err::EFAULT;
            }
            let (read_end, write_end) = crate::vfs::pipe::pair();
            // The table hands out slots lowest-first, so the pair is adjacent -
            // not that a correct caller may assume it.
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
        nr::KILL => {
            let (id, sig) = (args[0], args[1]);
            if sig > 31 {
                return err::EINVAL;
            }
            // sig 0 is the POSIX existence probe: nothing is delivered.
            if crate::sched::send_signal(TaskId(id), sig as u8) {
                0
            } else {
                err::ESRCH
            }
        }
        nr::OPEN => {
            let (ptr, len) = (args[0], args[1]);
            match path_arg(ptr, len, from_user) {
                Ok(path) => match crate::vfs::open(&path) {
                    Ok(fd) => fd as isize,
                    Err(e) => err_of(e),
                },
                Err(e) => e,
            }
        }
        nr::CLOSE => match crate::vfs::close(args[0] as i32) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        },
        nr::SPAWN => {
            let (path_ptr, path_len, blob_ptr, blob_len) = (args[0], args[1], args[2], args[3]);
            let path = match path_arg(path_ptr, path_len, from_user) {
                Ok(path) => path,
                Err(e) => return e,
            };

            // The argument blob is NUL-terminated strings packed back to
            // back, argv[0] first. No blob means the program runs with just
            // its own name, the way exec's argv contract starts every count.
            let mut argv: Vec<String> = Vec::new();
            if blob_ptr != 0 && blob_len != 0 {
                if blob_len > crate::sched::ARGS_BYTES_MAX {
                    return err::EINVAL;
                }
                if !buffer_ok(blob_ptr, blob_len, false, from_user) {
                    return err::EFAULT;
                }
                let bytes = unsafe { core::slice::from_raw_parts(blob_ptr as *const u8, blob_len) };
                let mut rest = bytes;
                while !rest.is_empty() {
                    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                    let field = &rest[..end];
                    match core::str::from_utf8(field) {
                        // An empty field - a doubled NUL or a leading one -
                        // is not an argument the loader can place.
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

            let bytes = match crate::vfs::read_file(&path) {
                Ok(bytes) => bytes,
                Err(e) => return err_of(e),
            };
            match crate::sched::try_spawn_user(
                &bytes,
                crate::sched::Priority::NORMAL,
                crate::sched::current(),
                &argv,
            ) {
                Ok(id) => id.0 as isize,
                Err(crate::sched::SpawnError::NotExecutable) => err::ENOEXEC,
                Err(crate::sched::SpawnError::ArgsTooBig) => err::EINVAL,
            }
        }
        nr::WAIT => {
            let pid = args[0];
            let me = crate::sched::current();
            // -1 is POSIX's "any child", and it is what init wants: it starts
            // a shell, waits for whichever child ends, and starts another.
            // `usize::MAX` is how a negative of any width arrives here.
            if pid == usize::MAX {
                return match crate::sched::wait_any_child(me) {
                    Some((_id, code)) => code,
                    None => err::ECHILD,
                };
            }
            if pid == 0 || pid >= crate::sched::MAX_TASKS {
                return err::ECHILD;
            }
            let id = TaskId(pid);
            // Only a live child of ours is worth blocking on. Anything else -
            // another task's child, a pid that never existed - either left a
            // record behind or gets ECHILD.
            if crate::sched::is_child(id, me) {
                crate::sched::block_until_code(id);
            }
            match crate::sched::take_exit_code(id) {
                Some(code) => code,
                None => err::ECHILD,
            }
        }
        nr::DUP2 => {
            // Duplicating a descriptor onto a number already in use is the
            // POSIX way to redirect: the target is closed first, silently.
            let (old, new) = (args[0] as i32, args[1] as i32);
            if new < 0 {
                return err::EBADF;
            }
            match crate::vfs::dup2(old, new) {
                Ok(fd) => fd as isize,
                Err(e) => err_of(e),
            }
        }
        // The two ways ring 3 makes and unmakes a name. Both go through the
        // parent directory, because that is where the edge lives: a file is
        // reachable by name only as long as its directory says so.
        nr::CREATE => {
            let path = match path_arg(args[0], args[1], from_user) {
                Ok(path) => path,
                Err(e) => return e,
            };
            let (parent, name) = match split_parent(&path) {
                Ok(parts) => parts,
                Err(e) => return e,
            };
            match crate::vfs::resolve(parent)
                .and_then(|dir| dir.create(name, crate::vfs::NodeKind::File))
            {
                // The caller gets a descriptor, not the inode: a name it just
                // made is one it almost always means to write.
                Ok(_) => match crate::vfs::open(&path) {
                    Ok(fd) => fd as isize,
                    Err(e) => err_of(e),
                },
                Err(e) => err_of(e),
            }
        }
        nr::UNLINK => {
            let path = match path_arg(args[0], args[1], from_user) {
                Ok(path) => path,
                Err(e) => return e,
            };
            let (parent, name) = match split_parent(&path) {
                Ok(parts) => parts,
                Err(e) => return e,
            };
            match crate::vfs::resolve(parent).and_then(|dir| dir.unlink(name)) {
                Ok(()) => 0,
                Err(e) => err_of(e),
            }
        }
        nr::LISTDIR => {
            let (ptr, len, buf, buf_len) = (args[0], args[1], args[2], args[3]);
            let path = match path_arg(ptr, len, from_user) {
                Ok(path) => path,
                Err(e) => return e,
            };
            let entries = match crate::vfs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) => return err_of(e),
            };

            // Stream form: per entry one kind byte, the name, a NUL. A
            // caller that passes a zero length gets the size it needs, so
            // the buffer is never guessed at twice.
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
            if !buffer_ok(buf, stream.len(), true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(stream.as_ptr(), buf as *mut u8, stream.len());
            }
            stream.len() as isize
        }
        _ => err::ENOSYS,
    }
}

#[cfg(test)]
mod tests {
    use super::{err, nr};

    #[test]
    fn numbers_are_pinned() {
        // The libc binds these literal values; renumbering is a breaking change.
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
    }

    #[test]
    fn a_path_splits_into_the_directory_and_the_name() {
        // Whatever comes after the last slash is the name; everything before
        // it is a path to walk, and `/` is a path like any other.
        assert_eq!(super::split_parent("/f"), Ok(("/", "f")));
        assert_eq!(super::split_parent("/a/b"), Ok(("/a", "b")));
        assert_eq!(super::split_parent("/a/b/"), Ok(("/a", "b")));
        assert_eq!(super::split_parent("/a/b///"), Ok(("/a", "b")));

        // A name is never a path component, and a path with no directory in
        // it has no parent to name - there is no cwd to resolve it against.
        assert_eq!(super::split_parent("f"), Err(super::err::EINVAL));
        assert_eq!(super::split_parent("/"), Err(super::err::EINVAL));
        assert_eq!(super::split_parent("/a/."), Err(super::err::EINVAL));
        assert_eq!(super::split_parent("/a/.."), Err(super::err::EINVAL));
    }

    #[test]
    fn errors_match_the_errno_values_they_name() {
        assert_eq!(err::ENOENT, -2);
        assert_eq!(err::ESRCH, -3);
        assert_eq!(err::ENOEXEC, -8);
        assert_eq!(err::EBADF, -9);
        assert_eq!(err::ECHILD, -10);
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
