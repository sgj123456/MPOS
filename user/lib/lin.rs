//! The Linux-ABI test harness for the `t_*` programs under `user/bin`.
//!
//! mpos serves two syscall surfaces. The compact `int 0x80` ABI of
//! `user/lib/sys.rs` carries mpos's own numbers; the `syscall`-instruction
//! ABI of `src/syscall/linux/` carries Linux x86_64 numbers and is what musl
//! binaries - `musl_hello` in particular - speak. These tests target the
//! second surface, so the stubs here issue real `syscall` instructions with
//! Linux numbers taken straight from the kernel's
//! `src/syscall/linux/nr.rs`: kernel and tests cannot disagree about a
//! number without one of them failing to build.
//!
//! Scope of every check: exactly the contract the `musl_hello` strace shows.
//! Where the strace hands the kernel an out-pointer (old sigaction, old
//! altstack, ...), the test seeds it with sentinels and requires the kernel
//! to have overwritten them - a query that answers 0 but writes nothing
//! hands musl uninitialized memory, which is a bug, not support. What the
//! strace does not exercise - fault enforcement, real signal delivery,
//! alt-stack switching on a fault - no test claims.
//!
//! Verdict protocol: one `ok:` or `FAIL:` line per check, a final `PASS` or
//! `FAILED` line, exit 0 when everything passed, 1 when anything failed,
//! 101 from the panic path.

#![allow(dead_code)]

use core::arch::{asm, global_asm};

/// The Linux numbers, straight from the kernel source: one table, no drift.
#[path = "../../src/syscall/linux/nr.rs"]
pub mod nr;

global_asm!(
    // Process entry, byte-identical in intent to user/lib/sys.rs: the kernel
    // lands here with the argument block at [rsp]; lift argc and argv into
    // the argument registers and call into Rust. The trailing `ud2` is
    // unreachable: mpos_user_main is `-> !`.
    ".globl _start",
    ".p2align 4",
    "_start:",
    "    xor ebp, ebp",
    "    mov rdi, [rsp]",
    "    lea rsi, [rsp + 8]",
    "    call mpos_user_main",
    "    ud2",
);

// ---------------------------------------------------------------------------
// Raw `syscall` stubs. The instruction destroys rcx and r11, so the stubs
// name them as outputs; nothing else is clobbered. The default asm
// assumptions (memory may be read or written) cover the out-pointers the
// arguments carry.
// ---------------------------------------------------------------------------

fn sys0(number: u64) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys1(number: u64, a1: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys2(number: u64, a1: usize, a2: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys3(number: u64, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys4(number: u64, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys5(number: u64, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn sys6(number: u64, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

// ---------------------------------------------------------------------------
// errno values, already negated the way the kernel returns them. Mirrors the
// `err` module of src/syscall/mod.rs.
// ---------------------------------------------------------------------------

pub const EPERM: isize = -1;
pub const ENOENT: isize = -2;
pub const EBADF: isize = -9;
pub const EAGAIN: isize = -11;
pub const ENOMEM: isize = -12;
pub const EFAULT: isize = -14;
pub const EINVAL: isize = -22;
pub const ENOSYS: isize = -38;

// ---------------------------------------------------------------------------
// Linux x86_64 ABI constants the tests pass or check.
// ---------------------------------------------------------------------------

pub const PROT_NONE: usize = 0;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const PROT_EXEC: usize = 4;

pub const MAP_PRIVATE: usize = 0x02;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANONYMOUS: usize = 0x20;
pub const MAP_STACK: usize = 0x20000;
/// mmap's failure value.
pub const MAP_FAILED: isize = -1;

pub const SIGBUS: i32 = 7;
pub const SIGSEGV: i32 = 11;
pub const SIGPIPE: i32 = 13;
/// First real-time signal; the strace shows SIGRT_1 and SIGRT_2 unblocked.
pub const SIGRT_1: i32 = 34;
pub const SIGRT_2: i32 = 35;

pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;
pub const SA_SIGINFO: usize = 0x0000_0004;
pub const SA_RESTORER: usize = 0x0400_0000;
pub const SA_ONSTACK: usize = 0x0800_0000;
pub const SA_RESTART: usize = 0x1000_0000;

pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;

pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

/// arch_prctl operations.
pub const ARCH_SET_GS: usize = 0x1001;
pub const ARCH_SET_FS: usize = 0x1002;
pub const ARCH_GET_FS: usize = 0x1003;
pub const ARCH_GET_GS: usize = 0x1004;

/// openat: paths are relative to the caller's cwd.
pub const AT_FDCWD: i32 = -100;
pub const O_RDONLY: usize = 0;
pub const O_CLOEXEC: usize = 0x8000_0;
/// descriptor flags for F_GETFD/F_SETFD.
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const FD_CLOEXEC: usize = 1;
/// access modes.
pub const R_OK: usize = 4;

/// lseek origins.
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

/// prlimit64 resources.
pub const RLIMIT_STACK: i32 = 3;
pub const RLIM64_INFINITY: u64 = u64::MAX;

/// file type bits of st_mode / stx_mode.
pub const S_IFMT: u32 = 0xF000;
pub const S_IFREG: u32 = 0x8000;
pub const S_IFDIR: u32 = 0x4000;

/// statx request mask bits.
pub const STATX_TYPE: u32 = 0x1;
pub const STATX_SIZE: u32 = 0x200;
pub const STATX_BASIC_STATS: u32 = 0x7ff;

/// the rseq signature glibc registers with.
pub const RSEQ_SIG: u32 = 0x5305_3053;

// ---------------------------------------------------------------------------
// ABI structures, laid out exactly as musl lays them out on x86_64.
// ---------------------------------------------------------------------------

/// One `struct pollfd`: descriptor, requested events, reported events.
#[repr(C)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// The 32-byte `struct sigaction` musl hands rt_sigaction with
/// `sigsetsize = 8`: handler, flags, restorer, then one 8-byte mask.
#[repr(C)]
pub struct SigAction {
    pub handler: usize,
    pub flags: usize,
    pub restorer: usize,
    pub mask: u64,
}

/// Linux x86_64 `struct stat` (144 bytes): the layout fstat/newfstatat fill.
#[repr(C)]
pub struct Stat {
    pub st_dev: u64,        // 0
    pub st_ino: u64,        // 8
    pub st_nlink: u64,      // 16
    pub st_mode: u32,       // 24
    pub st_uid: u32,        // 28
    pub st_gid: u32,        // 32
    pub _pad: u32,          // 36
    pub st_rdev: u64,       // 40
    pub st_size: i64,       // 48
    pub st_blksize: i64,    // 56
    pub st_blocks: i64,     // 64
    pub st_atime: i64,      // 72
    pub st_atime_nsec: i64, // 80
    pub st_mtime: i64,      // 88
    pub st_mtime_nsec: i64, // 96
    pub st_ctime: i64,      // 104
    pub st_ctime_nsec: i64, // 112
    pub _unused: [u64; 3],  // 120..144
}

/// `struct statx_timestamp`: seconds, nanoseconds, reserved slot.
#[repr(C)]
pub struct StatxTimestamp {
    pub sec: i64,
    pub nsec: u32,
    pub _pad: i32,
}

/// Linux `struct statx` through stx_mnt_id (152 bytes); callers pass a
/// larger zeroed buffer.
#[repr(C)]
pub struct Statx {
    pub stx_mask: u32,             // 0
    pub stx_blksize: u32,          // 4
    pub stx_attributes: u64,       // 8
    pub stx_nlink: u32,            // 16
    pub stx_uid: u32,              // 20
    pub stx_gid: u32,              // 24
    pub stx_mode: u16,             // 28
    pub _spare0: u16,              // 30
    pub stx_ino: u64,              // 32
    pub stx_size: u64,             // 40
    pub stx_blocks: u64,           // 48
    pub stx_attributes_mask: u64,  // 56
    pub stx_atime: StatxTimestamp, // 64
    pub stx_btime: StatxTimestamp, // 80
    pub stx_ctime: StatxTimestamp, // 96
    pub stx_mtime: StatxTimestamp, // 112
    pub stx_rdev_major: u32,       // 128
    pub stx_rdev_minor: u32,       // 132
    pub stx_dev_major: u32,        // 136
    pub stx_dev_minor: u32,        // 140
    pub stx_mnt_id: u64,           // 144
}

/// `struct rlimit64`: soft limit, hard limit.
#[repr(C)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

/// `struct robust_list_head` is a single pointer for the syscall's sake;
/// glibc's real head is 24 bytes.
#[repr(C)]
pub struct RobustListHead {
    pub next: usize,
}

/// glibc's `struct rseq` (32 bytes), registered at startup.
#[repr(C)]
pub struct Rseq {
    pub cpu_id_start: u32,
    pub cpu_id: u32,
    pub rseq_cs: u64,
    pub flags: u32,
    pub node_id: u32,
    pub mm_cid: u32,
    pub _pad: u32,
}

/// `stack_t`: pointer, flags (padded to the pointer's width), size.
#[repr(C)]
pub struct StackT {
    pub ss_sp: usize,
    pub ss_flags: i32,
    pub _pad: u32,
    pub ss_size: usize,
}

// ---------------------------------------------------------------------------
// Syscall wrappers. Thin on purpose: each names the Linux number once and
// nothing else.
// ---------------------------------------------------------------------------

pub fn write(fd: i32, buf: &[u8]) -> isize {
    sys3(nr::WRITE, fd as usize, buf.as_ptr() as usize, buf.len())
}

/// Write every byte, in as many syscalls as it takes.
pub fn write_all(fd: i32, mut buf: &[u8]) -> isize {
    let mut total = 0isize;
    while !buf.is_empty() {
        let n = write(fd, buf);
        if n <= 0 {
            return if total == 0 { n } else { total };
        }
        total += n;
        buf = &buf[n as usize..];
    }
    total
}

/// Terminate the whole process - what musl's `exit` ends in.
pub fn exit_group(code: i32) -> ! {
    sys1(nr::EXIT_GROUP, code as usize);
    unreachable!("EXIT_GROUP never returns")
}

pub fn getpid() -> isize {
    sys0(nr::GETPID)
}

/// `poll(fds, timeout)`: nfds comes from the slice length.
pub fn poll(fds: &mut [PollFd], timeout: i32) -> isize {
    sys3(
        nr::POLL,
        fds.as_mut_ptr() as usize,
        fds.len(),
        timeout as usize,
    )
}

/// `rt_sigaction` with musl's `sigsetsize = 8`; either pointer may be null.
pub fn rt_sigaction(sig: i32, act: *const SigAction, old: *mut SigAction) -> isize {
    sys4(
        nr::RT_SIGACTION,
        sig as usize,
        act as usize,
        old as usize,
        8,
    )
}

/// `rt_sigprocmask` with musl's `sigsetsize = 8`; either pointer may be null.
pub fn rt_sigprocmask(how: i32, set: *const u64, old: *mut u64) -> isize {
    sys4(
        nr::RT_SIGPROCMASK,
        how as usize,
        set as usize,
        old as usize,
        8,
    )
}

/// `sigaltstack`; either pointer may be null.
pub fn sigaltstack(ss: *const StackT, old: *mut StackT) -> isize {
    sys2(nr::SIGALTSTACK, ss as usize, old as usize)
}

pub fn arch_prctl(code: usize, addr: usize) -> isize {
    sys2(nr::ARCH_PRCTL, code, addr)
}

/// Register the clear-child-tid slot; the return is the thread id.
pub fn set_tid_address(tidptr: *mut i64) -> isize {
    sys1(nr::SET_TID_ADDRESS, tidptr as usize)
}

/// `mmap` in its full six-argument form. `fd = -1` for anonymous maps.
pub fn mmap(addr: usize, len: usize, prot: usize, flags: usize, fd: i32, off: usize) -> isize {
    sys6(nr::MMAP, addr, len, prot, flags, fd as isize as usize, off)
}

pub fn munmap(addr: usize, len: usize) -> isize {
    sys2(nr::MUNMAP, addr, len)
}

pub fn mprotect(addr: usize, len: usize, prot: usize) -> isize {
    sys3(nr::MPROTECT, addr, len, prot)
}

/// The program break: 0 asks where it is, anything else moves it.
pub fn brk(addr: usize) -> isize {
    sys1(nr::BRK, addr)
}

/// Replace the image: NUL-terminated path, argv and envp both
/// NULL-terminated pointer arrays.
pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    sys3(nr::EXECVE, path as usize, argv as usize, envp as usize)
}

/// Wait for a child. Linux returns the child's pid and encodes the exit
/// code in `*status`; mpos returns the exit code directly - the tests pin
/// whichever side of that divergence the syscall under test needs.
pub fn wait4(pid: usize, status: *mut i32, options: usize) -> isize {
    sys4(nr::WAIT4, pid, status as usize, options, 0)
}

/// Read up to `buf.len()` bytes from an open descriptor.
pub fn read(fd: i32, buf: &mut [u8]) -> isize {
    sys3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len())
}

/// Open a NUL-terminated path relative to `dirfd`; flags carry O_CLOEXEC
/// and friends, mode is ignored for reads. Linux passes no length - the
/// path's NUL is the terminator, and that is the contract the tests pin.
pub fn openat(dirfd: i32, path: &[u8], flags: usize, mode: usize) -> isize {
    sys4(
        nr::OPENAT,
        dirfd as usize,
        path.as_ptr() as usize,
        flags,
        mode,
    )
}

pub fn close(fd: i32) -> isize {
    sys1(nr::CLOSE, fd as usize)
}

/// Move the file position; returns the new absolute offset.
pub fn lseek(fd: i32, offset: isize, whence: i32) -> isize {
    sys3(nr::LSEEK, fd as usize, offset as usize, whence as usize)
}

/// Stat an open descriptor into a 144-byte Linux `struct stat`.
pub fn fstat(fd: i32, buf: *mut Stat) -> isize {
    sys2(nr::FSTAT, fd as usize, buf as usize)
}

/// Stat a path relative to `dirfd` into a 144-byte Linux `struct stat`;
/// flags carries AT_SYMLINK_NOFOLLOW and friends.
pub fn newfstatat(dirfd: i32, path: &[u8], buf: *mut Stat, flags: usize) -> isize {
    sys4(
        nr::NEWFSTATAT,
        dirfd as usize,
        path.as_ptr() as usize,
        buf as usize,
        flags,
    )
}

/// Stat a path into `struct statx`; the buffer must be zeroed so absent
/// fields read as absent.
pub fn statx(dirfd: i32, path: &[u8], flags: usize, mask: u32, buf: *mut Statx) -> isize {
    sys5(
        nr::STATX,
        dirfd as usize,
        path.as_ptr() as usize,
        flags,
        mask as usize,
        buf as usize,
    )
}

/// Check reachability of a NUL-terminated path; mode is R_OK and friends.
pub fn access(path: &[u8], mode: usize) -> isize {
    sys2(nr::ACCESS, path.as_ptr() as usize, mode)
}

/// Descriptor flag manipulation - the F_GETFD/F_SETFD pair and friends.
pub fn fcntl(fd: i32, cmd: i32, arg: usize) -> isize {
    sys3(nr::FCNTL, fd as usize, cmd as usize, arg)
}

/// Read the target of a symlink into `buf`, without a NUL; the return is
/// the byte count, truncated to `buf.len()` like Linux does.
pub fn readlink(path: &[u8], buf: &mut [u8]) -> isize {
    sys3(
        nr::READLINK,
        path.as_ptr() as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

/// Fill `buf` with `buf.len()` random bytes; flags carries GRND_NONBLOCK.
pub fn getrandom(buf: &mut [u8], flags: usize) -> isize {
    sys3(nr::GETRANDOM, buf.as_mut_ptr() as usize, buf.len(), flags)
}

/// The calling thread's id - the process's pid for the main thread.
pub fn gettid() -> isize {
    sys0(nr::GETTID)
}

/// Query (new = null) or set a resource limit; the old limit lands in the
/// 16-byte `struct rlimit64`.
pub fn prlimit64(pid: i32, resource: i32, new: *const Rlimit, old: *mut Rlimit) -> isize {
    sys4(
        nr::PRLIMIT64,
        pid as usize,
        resource as usize,
        new as usize,
        old as usize,
    )
}

/// Ask which CPUs the caller may run on: fills the mask, returns the CPU
/// count it covers.
pub fn sched_getaffinity(pid: i32, mask: *mut u64) -> isize {
    sys3(nr::SCHED_GETAFFINITY, pid as usize, 8, mask as usize)
}

/// Register the robust-futex list head; Linux pins len to 24.
pub fn set_robust_list(head: *const RobustListHead, len: usize) -> isize {
    sys2(nr::SET_ROBUST_LIST, head as usize, len)
}

/// Register this thread's restartable-sequence block and signature.
pub fn rseq(rseq: *mut Rseq, len: usize, flags: usize, sig: u32) -> isize {
    sys4(nr::RSEQ, rseq as usize, len, flags, sig as usize)
}

// ---------------------------------------------------------------------------
// Verdict output.
// ---------------------------------------------------------------------------

pub fn puts(bytes: &[u8]) {
    let _ = write_all(1, bytes);
}

/// A signed word in decimal, for failure lines that quote a return value.
pub fn puts_isize(v: isize) {
    let mut buf = [0u8; 21];
    let mut i = buf.len();
    let neg = v < 0;
    let mut u = v.unsigned_abs();
    loop {
        i -= 1;
        buf[i] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    puts(&buf[i..]);
}

/// One named check that pins an exact expected value.
pub fn check_eq(failed: &mut bool, name: &[u8], got: isize, want: isize) {
    if got == want {
        puts(b"ok:   ");
        puts(name);
        puts(b"\n");
    } else {
        puts(b"FAIL: ");
        puts(name);
        puts(b" (got ");
        puts_isize(got);
        puts(b", want ");
        puts_isize(want);
        puts(b")\n");
        *failed = true;
    }
}

/// One named check that pins a predicate.
pub fn check_true(failed: &mut bool, name: &[u8], cond: bool) {
    if cond {
        puts(b"ok:   ");
        puts(name);
        puts(b"\n");
    } else {
        puts(b"FAIL: ");
        puts(name);
        puts(b"\n");
        *failed = true;
    }
}

/// End of a test: summarize and leave with the verdict in the exit code.
pub fn done(failed: bool) -> ! {
    if failed {
        puts(b"FAILED\n");
        exit_group(1);
    }
    puts(b"PASS\n");
    exit_group(0);
}

/// The panic path every test's `#[panic_handler]` lands in.
pub fn die(msg: &[u8]) -> ! {
    let _ = write_all(2, msg);
    exit_group(101)
}
