//! Linux x86_64 syscall number mapping and POSIX handlers.
//!
//! When a program compiled for Linux (`x86_64-unknown-linux-musl`) runs on
//! mpos, it enters through the `syscall` instruction with Linux numbers.
//! This module maps those numbers onto handlers in [`super::proc`]
//! (Linux-compatible memory/process/time calls) and [`fs`] (path-carrying
//! calls in Linux argument shapes).

pub mod nr;

/// The path-carrying calls, in Linux argument shapes: NUL-terminated paths
/// and dirfd-led resolution.
#[cfg(target_os = "none")]
pub mod fs;

/// Linux-compatible memory and process handlers (mmap/munmap/mremap/brk,
/// execve/exit/wait, getpid/kill, nanosleep, ...). Kept here at the same
/// level as `fs` so the dispatch can reach both without the deleted custom
/// modules they used to share a parent with.
#[cfg(target_os = "none")]
pub(crate) mod proc;

#[cfg(target_os = "none")]
pub fn dispatch(nr: u64, args: [usize; 6], from_user: bool, regs_ptr: *mut ()) -> isize {
    // Trace the first 30 syscalls per process for musl debug.
    static SYSCALL_COUNTS: [core::sync::atomic::AtomicUsize; 64] =
        const { [const { core::sync::atomic::AtomicUsize::new(0) }; 64] };
    let pid = crate::sched::current().0;
    let n = SYSCALL_COUNTS[pid].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if n < 30 {
        crate::println!(
            "[syscall #{}] pid={} nr={} args=[{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}]",
            n,
            pid,
            nr,
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5]
        );
    }

    match nr {
        // --- file I/O: Linux argument shapes ---
        nr::READ => fs::handle_read(args, from_user),
        nr::WRITE => fs::handle_write(args, from_user),
        nr::OPEN => fs::open(args, &cwd()),
        nr::OPENAT => fs::openat(args, &cwd()),
        nr::CLOSE => fs::handle_close(args),
        nr::FSTAT => fs::fstat(args),
        nr::NEWFSTATAT => fs::newfstatat(args, &cwd()),
        nr::STATX => fs::statx(args, &cwd()),
        nr::LSEEK => fs::handle_lseek(args),
        nr::GETDENTS64 => fs::handle_getdents64(args, from_user),
        nr::DUP => fs::handle_dup(args),
        nr::DUP2 => fs::handle_dup2(args),
        nr::DUP3 => fs::handle_dup3(args),
        nr::PIPE => fs::handle_pipe(args, from_user),
        nr::PIPE2 => {
            const O_CLOEXEC: usize = 0o200_0000;
            if args[1] & !O_CLOEXEC != 0 {
                err::EINVAL
            } else {
                fs::handle_pipe(args, from_user)
            }
        }
        nr::UNLINKAT => fs::unlinkat(args, &cwd()),
        // The legacy path forms, man(2) conventions musl/glibc call: each is
        // its *at equivalent at the caller's cwd.
        nr::UNLINK => fs::unlinkat([fs::AT_FDCWD as usize, args[0], 0, 0, 0, 0], &cwd()),
        nr::MKDIR => fs::mkdirat([fs::AT_FDCWD as usize, args[0], args[1], 0, 0, 0], &cwd()),
        nr::RENAME => fs::renameat2(
            [
                fs::AT_FDCWD as usize,
                args[0],
                fs::AT_FDCWD as usize,
                args[1],
                0,
                0,
            ],
            &cwd(),
        ),
        nr::MKDIRAT => fs::mkdirat(args, &cwd()),
        nr::RENAMEAT2 => fs::renameat2(args, &cwd()),
        nr::GETCWD => fs::handle_getcwd(args, from_user),
        nr::CHDIR => fs::chdir(args),
        nr::FCHDIR => fs::handle_fchdir(args),
        nr::FCNTL => fs::fcntl(args),
        nr::IOCTL => fs::handle_ioctl(args, from_user),
        nr::WRITEV => fs::handle_writev(args, from_user),
        nr::READV => fs::handle_readv(args, from_user),
        nr::ACCESS => fs::access(args, &cwd()),
        nr::FACCESSAT => fs::faccessat(args, &cwd()),
        nr::READLINK => fs::readlink(args, &cwd()),

        // --- process ---
        nr::EXECVE => proc::handle_execve(args, &cwd(), regs_ptr),
        nr::EXIT => proc::handle_exit(args),
        nr::EXIT_GROUP => proc::handle_exit(args),
        // Linux's wait4 is (pid, *status, options): the options word sits in
        // slot 2, where mpos's own wait keeps none. Reshape before reusing the
        // handler, or the status pointer is read as the flags.
        nr::WAIT4 => proc::handle_wait([args[0], args[2], 0, 0, 0, 0]),
        nr::GETPID => proc::handle_getpid(),
        nr::GETTID => proc::handle_getpid(),
        nr::GETPPID => proc::handle_getppid(),
        nr::SCHED_YIELD => proc::handle_yield(),
        nr::NANOSLEEP => proc::handle_nanosleep(args, from_user),
        nr::CLOCK_NANOSLEEP => proc::handle_clock_nanosleep(args, from_user),
        nr::KILL => proc::handle_kill(args),
        nr::TKILL => proc::handle_kill(args),
        nr::CLONE => proc::handle_clone(args),
        nr::FORK | nr::VFORK => proc::handle_fork(args),

        // --- memory ---
        nr::BRK => proc::handle_brk(args),
        nr::MMAP => proc::handle_mmap(args),
        nr::MUNMAP => proc::handle_munmap(args),
        nr::MPROTECT => proc::handle_mprotect(args),
        nr::MREMAP => proc::handle_mremap(args),
        nr::MADVISE => 0, // stub: success

        // --- stubs ---
        nr::POLL | nr::PPOLL => fs::poll(args),
        nr::SIGALTSTACK => sigaltstack(args),
        nr::CLOCK_GETTIME => proc::handle_clock_gettime(args),
        nr::GETTIMEOFDAY => proc::handle_gettimeofday(args),
        nr::UNAME => proc::handle_uname(args, true),
        nr::GETUID | nr::GETGID | nr::GETEUID | nr::GETEGID => 0,
        nr::SETUID | nr::SETGID | nr::SETEUID | nr::SETSID => err::EPERM,
        nr::RT_SIGACTION => sigaction(args),
        nr::RT_SIGPROCMASK => rt_sigprocmask(args),
        nr::RT_SIGRETURN => 0,
        nr::ARCH_PRCTL => handle_arch_prctl(args),
        nr::GETRANDOM => proc::handle_getrandom(args, true),
        nr::PRLIMIT64 => proc::handle_prlimit64(args, true),
        nr::FUTEX => proc::handle_futex(args),
        nr::SET_TID_ADDRESS => proc::handle_getpid(),
        nr::SCHED_GETAFFINITY => proc::handle_sched_getaffinity(args),
        nr::SET_ROBUST_LIST => proc::handle_set_robust_list(args),
        nr::RSEQ => proc::handle_rseq(args),
        nr::CLEANUP_AFTER_KTHREAD => 0,

        _ => super::err::ENOSYS,
    }
}

use super::err;
use crate::sched::current_cwd as cwd;

const ARCH_SET_GS: usize = 0x1001;
const ARCH_SET_FS: usize = 0x1002;
const ARCH_GET_FS: usize = 0x1003;
const ARCH_GET_GS: usize = 0x1004;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;

/// The 32-byte `struct sigaction` of the Linux x86_64 ABI, in musl's
/// `sigsetsize = 8` shape: handler, flags, restorer, then one 8-byte mask.
/// `handler` is a user address, or `SIG_DFL` (0) / `SIG_IGN` (1).
#[repr(C)]
#[derive(Clone, Copy)]
struct UserSigAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

/// `rt_sigaction(sig, act, old, sigsetsize)`.
///
/// Either pointer may be null, and each is used on its own: `act` records a
/// disposition, `old` reports the one in force. A query has to *write* its
/// answer - answering 0 and leaving the struct alone hands the caller the
/// bytes it happened to have there, which for a fresh process means sentinels,
/// not `SIG_DFL`.
///
/// The disposition is recorded per process and is not yet acted on: no
/// handler is ever called, because no user frame is built for one. A signal
/// still kills the process it lands on. What is stored is therefore read back
/// verbatim rather than interpreted.
fn sigaction(args: [usize; 6]) -> isize {
    let (sig, act, old) = (args[0], args[1], args[2]);
    // Linux's refusal set: signal 0 is not a signal, 32 and up are real-time
    // signals this kernel has no numbers for, and SIGKILL/SIGSTOP cannot be
    // given a disposition by anyone.
    if !crate::sched::signal::catchable(sig) {
        return err::EINVAL;
    }
    let id = crate::sched::current();
    if act != 0 {
        if !super::buffer_ok(act, core::mem::size_of::<UserSigAction>(), false, true) {
            return err::EFAULT;
        }
        let incoming = unsafe { core::ptr::read(act as *const UserSigAction) };
        let action = crate::sched::signal::Action {
            handler: incoming.handler,
            flags: incoming.flags,
            restorer: incoming.restorer,
            mask: incoming.mask,
        };
        if !crate::sched::set_signal_action(id, sig as u8, action) {
            return err::ESRCH;
        }
    }
    if old != 0 {
        if !super::buffer_ok(old, core::mem::size_of::<UserSigAction>(), true, true) {
            return err::EFAULT;
        }
        let Some(previous) = crate::sched::signal_action(id, sig as u8) else {
            return err::ESRCH;
        };
        let outgoing = UserSigAction {
            handler: previous.handler,
            flags: previous.flags,
            restorer: previous.restorer,
            mask: previous.mask,
        };
        unsafe { core::ptr::write(old as *mut UserSigAction, outgoing) };
    }
    0
}

/// `SS_DISABLE`, the `ss_flags` bit that says "there is no alternate stack".
const SS_DISABLE: i32 = 2;

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`.
///
/// mpos keeps no per-process mask - every signal is delivered the moment it
/// arrives - so a change is validated and then forgotten, and every query
/// reports an empty mask. The validation is still real: Linux refuses an
/// unknown `how`, a `sigsetsize` other than the ABI's 8, a `set` that cannot
/// be read, and an `oldset` that cannot be written. A query must *write* its
/// answer - a fresh process owes its caller zeros, not the sentinels it
/// happened to have there.
fn rt_sigprocmask(args: [usize; 6]) -> isize {
    const SIG_SETMASK: usize = 2;
    let (how, set, old, sigsetsize) = (args[0], args[1], args[2], args[3]);
    if sigsetsize != 8 {
        return err::EINVAL;
    }
    if how > SIG_SETMASK {
        return err::EINVAL;
    }
    if set != 0 && !super::buffer_ok(set, 8, false, true) {
        return err::EFAULT;
    }
    if old != 0 {
        if !super::buffer_ok(old, 8, true, true) {
            return err::EFAULT;
        }
        unsafe { (old as *mut u64).write_volatile(0) };
    }
    0
}

/// The 24-byte `stack_t` of the Linux x86_64 ABI.
#[repr(C)]
#[derive(Clone, Copy)]
struct UserStackT {
    ss_sp: usize,
    ss_flags: i32,
    pad: i32,
    ss_size: usize,
}

/// `sigaltstack(ss, old)`.
///
/// As with `rt_sigaction`, a query has to *write* its answer: the caller seeds
/// the out-struct, and a fresh process must read back `SS_DISABLE` with no
/// stack rather than the sentinels it happened to have there.
///
/// An install is recorded per process. Nothing switches to the stack yet - a
/// signal still kills rather than calling a handler - but a stack the caller
/// asked for is a fact about the process, and Linux reports it back.
fn sigaltstack(args: [usize; 6]) -> isize {
    let (ss, old) = (args[0], args[1]);
    let id = crate::sched::current();
    if ss != 0 {
        if !super::buffer_ok(ss, core::mem::size_of::<UserStackT>(), false, true) {
            return err::EFAULT;
        }
        let incoming = unsafe { core::ptr::read(ss as *const UserStackT) };
        // Linux refuses a flag word with bits it does not know before it
        // looks at anything else.
        if incoming.ss_flags & !SS_DISABLE != 0 {
            return err::EINVAL;
        }
        if incoming.ss_flags & SS_DISABLE != 0 {
            if !crate::sched::set_signal_alt_stack(id, None) {
                return err::ESRCH;
            }
        } else {
            // Linux answers ENOMEM, not EINVAL, for a stack too small for a
            // signal frame, and the same for an unset base.
            if incoming.ss_sp == 0 || incoming.ss_size < crate::sched::signal::MINSIGSTKSZ {
                return err::ENOMEM;
            }
            let stack = crate::sched::signal::AltStack {
                sp: incoming.ss_sp,
                size: incoming.ss_size,
            };
            if !crate::sched::set_signal_alt_stack(id, Some(stack)) {
                return err::ESRCH;
            }
        }
    }
    if old != 0 {
        if !super::buffer_ok(old, core::mem::size_of::<UserStackT>(), true, true) {
            return err::EFAULT;
        }
        let outgoing = match crate::sched::signal_alt_stack(id) {
            Some(stack) => UserStackT {
                ss_sp: stack.sp,
                ss_flags: 0,
                pad: 0,
                ss_size: stack.size,
            },
            None => UserStackT {
                ss_sp: 0,
                ss_flags: SS_DISABLE,
                pad: 0,
                ss_size: 0,
            },
        };
        unsafe { core::ptr::write(old as *mut UserStackT, outgoing) };
    }
    0
}

/// `arch_prctl(code, addr)`.
///
/// The SET forms load the base register straight from `addr`; the GET forms
/// write the base into `*(u64 *)addr` and answer 0, which is the shape the
/// man page and every libc wrapper agree on - a base returned in `rax`
/// instead would leave the caller reading whatever its buffer already held.
fn handle_arch_prctl(args: [usize; 6]) -> isize {
    let (cmd, addr) = (args[0], args[1]);
    match cmd {
        ARCH_SET_GS | ARCH_SET_FS => {
            let (msr, value) = (
                if cmd == ARCH_SET_FS {
                    MSR_FS_BASE
                } else {
                    MSR_GS_BASE
                },
                addr,
            );
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") msr,
                    in("eax") value as u32,
                    in("edx") (value >> 32) as u32,
                );
            }
            0
        }
        ARCH_GET_FS | ARCH_GET_GS => {
            let msr = if cmd == ARCH_GET_FS {
                MSR_FS_BASE
            } else {
                MSR_GS_BASE
            };
            let (mut lo, mut hi): (u32, u32);
            unsafe {
                core::arch::asm!(
                    "rdmsr",
                    in("ecx") msr,
                    out("eax") lo,
                    out("edx") hi,
                );
            }
            if !super::buffer_ok(addr, 8, true, true) {
                return err::EFAULT;
            }
            let val = ((hi as u64) << 32) | (lo as u64);
            unsafe { (addr as *mut u64).write_volatile(val) };
            0
        }
        _ => err::EINVAL,
    }
}
