//! Linux x86_64 syscall number mapping and new POSIX handlers.
//!
//! When a program compiled for Linux (`x86_64-unknown-linux-musl`) runs on
//! mpos, it enters through the `syscall` instruction with Linux numbers.
//! This module maps those numbers onto the existing mpos handlers where
//! possible and implements the rest.

pub mod nr;

#[cfg(target_os = "none")]
pub fn dispatch(nr: u64, args: [usize; 6]) -> isize {
    match nr {
        // --- file I/O (reuse mpos fs handlers) ---
        nr::READ => super::fs::handle_read(args, true),
        nr::WRITE => super::fs::handle_write(args, true),
        nr::OPEN => super::fs::handle_open(args, true, &cwd()),
        nr::OPENAT => super::fs::handle_openat(args, true, &cwd()),
        nr::CLOSE => super::fs::handle_close(args),
        nr::FSTAT | nr::NEWFSTATAT => super::fs::handle_fstat(args, true, &cwd()),
        nr::LSEEK => super::fs::handle_lseek(args),
        nr::GETDENTS64 => super::fs::handle_getdents64(args, true),
        nr::DUP => super::fs::handle_dup(args),
        nr::DUP3 => super::fs::handle_dup3(args),
        nr::PIPE2 => super::fs::handle_pipe(args, true),
        nr::UNLINKAT => super::fs::handle_unlinkat(args, true, &cwd()),
        nr::MKDIRAT => super::fs::handle_mkdirat(args, true, &cwd()),
        nr::RENAMEAT2 => super::fs::handle_renameat2(args, true, &cwd()),
        nr::GETCWD => super::fs::handle_getcwd(args, true),
        nr::CHDIR => super::fs::handle_chdir(args, true),
        nr::FCHDIR => super::fs::handle_fchdir(args),
        nr::FCNTL => super::fs::handle_fcntl(args),
        nr::IOCTL => super::fs::handle_ioctl(args, true),
        nr::WRITEV => super::fs::handle_writev(args, true),
        nr::READV => super::fs::handle_readv(args, true),
        nr::ACCESS | nr::FACCESSAT => 0, // stub: allow everything

        // --- process ---
        nr::EXECVE => super::proc::handle_execve(args, true, &cwd()),
        nr::EXIT => super::proc::handle_exit(args),
        nr::EXIT_GROUP => super::proc::handle_exit(args),
        nr::WAIT4 => super::proc::handle_wait(args),
        nr::GETPID => super::proc::handle_getpid(),
        nr::GETTID => super::proc::handle_getpid(),
        nr::GETPPID => super::proc::handle_getppid(),
        nr::YIELD => super::proc::handle_yield(),
        nr::NANOSLEEP | nr::CLOCK_NANOSLEEP => super::proc::handle_nanosleep(args),
        nr::KILL => super::proc::handle_kill(args),
        nr::TKILL => super::proc::handle_kill(args),
        nr::FORK => super::proc::handle_fork(args),

        // --- memory ---
        nr::BRK => super::proc::handle_brk(args),
        nr::MMAP => super::proc::handle_mmap(args),
        nr::MUNMAP => super::proc::handle_munmap(args),
        nr::MPROTECT => 0, // stub: success
        nr::MADVISE => 0,  // stub: success

        // --- stubs ---
        nr::CLOCK_GETTIME => super::proc::handle_clock_gettime(args),
        nr::GETTIMEOFDAY => super::proc::handle_clock_gettime(args),
        nr::UNAME => super::proc::handle_uname(args, true),
        nr::GETUID | nr::GETGID | nr::GETEUID | nr::GETEGID => 0,
        nr::SETUID | nr::SETGID | nr::SETEUID | nr::SETSID => err::EPERM,
        nr::RT_SIGACTION => 0,   // stub: success
        nr::RT_SIGPROCMASK => 0, // stub: success
        nr::RT_SIGRETURN => 0,
        nr::ARCH_PRCTL => 0, // stub: success
        nr::GETRANDOM => super::proc::handle_getrandom(args, true),
        nr::PRLIMIT64 => super::proc::handle_prlimit64(args, true),
        nr::FUTEX => super::proc::handle_futex(args),
        nr::SET_TID_ADDRESS => super::proc::handle_getpid(),
        nr::CLEANUP_AFTER_KTHREAD => 0,

        _ => {
            // crate::println!("linux: unhandled syscall nr={nr}");
            super::err::ENOSYS
        }
    }
}

use super::err;
use crate::sched::current_cwd as cwd;
