//! The Linux-ABI test harness for the `t_*` programs under `user/bin`.
//!
//! mpos serves two syscall surfaces, both through the one `syscall` gate:
//! the compact ABI of `user/lib/sys.rs` carries mpos's own numbers tagged
//! with "mpos" in the top 32 bits, and the ABI of `src/syscall/linux/`
//! carries Linux x86_64 numbers raw and is what musl binaries speak. These
//! tests target the second surface, so the
//! stubs here issue real `syscall` instructions with
//! Linux numbers taken straight from the kernel's
//! `src/syscall/linux/nr.rs`: kernel and tests cannot disagree about a
//! number without one of them failing to build.
//!
//! Scope of every check: the aspects each syscall's man page pins - the
//! success shape, the errno for every refusal the kernel claims to make,
//! and the edge cases the ABI fixes (zero-length buffers, sentinel-seeded
//! out-parameters, lowest-free-fd, flag validation). Where the man page's
//! answer would need a mechanism mpos does not have - symlink resolution,
//! signal delivery, cross-process shared mappings - the test pins the
//! documented mpos approximation instead, and the doc comment says which.
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

/// The process arguments, read out of the block the kernel laid on the stack.
pub struct Args {
    pub argc: usize,
    pub argv: *const *const u8,
}

impl Args {
    /// Argument `i` as bytes, without its terminator.
    pub fn get(&self, i: usize) -> &[u8] {
        assert!(i < self.argc, "argv[{i}] is past the end");
        unsafe {
            let start = *self.argv.add(i);
            let mut len = 0usize;
            while *start.add(len) != 0 {
                len += 1;
            }
            core::slice::from_raw_parts(start, len)
        }
    }
}

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
pub const ESRCH: isize = -3;
pub const EINTR: isize = -4;
pub const E2BIG: isize = -7;
pub const ENOEXEC: isize = -8;
pub const EBADF: isize = -9;
pub const ECHILD: isize = -10;
pub const EAGAIN: isize = -11;
pub const ENOMEM: isize = -12;
pub const EFAULT: isize = -14;
pub const EEXIST: isize = -17;
pub const ENODEV: isize = -19;
pub const ENOTDIR: isize = -20;
pub const EISDIR: isize = -21;
pub const EINVAL: isize = -22;
pub const ENOTTY: isize = -25;
pub const ENOSPC: isize = -28;
pub const ERANGE: isize = -34;
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
/// fstatat/statx by descriptor: an empty path names the dirfd itself.
pub const AT_EMPTY_PATH: usize = 0x1000;
/// unlinkat: the path is a directory to be removed.
pub const AT_REMOVEDIR: usize = 0x200;
pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_RDWR: usize = 2;
pub const O_CREAT: usize = 0o100;
pub const O_EXCL: usize = 0o200;
pub const O_DIRECTORY: usize = 0o200_000;
pub const O_CLOEXEC: usize = 0x8000_0;
/// descriptor flags for F_GETFD/F_SETFD.
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
pub const FD_CLOEXEC: usize = 1;
/// access modes.
pub const F_OK: usize = 0;
pub const R_OK: usize = 4;
pub const W_OK: usize = 2;
pub const X_OK: usize = 1;

/// lseek origins.
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

/// renameat2 flag bits.
pub const RENAME_NOREPLACE: usize = 0x1;
pub const RENAME_EXCHANGE: usize = 0x2;

/// getdents64 d_type values.
pub const DT_DIR: u8 = 4;
pub const DT_CHR: u8 = 2;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// ioctl requests.
pub const TIOCGWINSZ: usize = 0x5413;
pub const TCGETS: usize = 0x5401;
pub const TCSETS: usize = 0x5402;

/// wait4/waitpid options.
pub const WNOHANG: usize = 1;

/// signals beyond the three the strace showed.
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;
pub const SIGSTOP: i32 = 19;

/// clock ids.
pub const CLOCK_REALTIME: usize = 0;
pub const CLOCK_MONOTONIC: usize = 1;

/// madvise advice values.
pub const MADV_NORMAL: usize = 0;
pub const MADV_SEQUENTIAL: usize = 1;
pub const MADV_DONTNEED: usize = 4;

/// futex operations.
pub const FUTEX_WAIT: usize = 0;
pub const FUTEX_WAKE: usize = 1;

/// prlimit64 resources.
pub const RLIMIT_STACK: i32 = 3;
pub const RLIMIT_NOFILE: i32 = 5;
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

/// One `struct iovec`: base pointer and length, as readv/writev scatter.
#[repr(C)]
pub struct Iovec {
    pub base: *mut u8,
    pub len: usize,
}

/// `struct timespec` as the ABI lays it out: seconds, nanoseconds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeSpec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// `struct timeval`: seconds, microseconds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TimeVal {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// `struct winsize` for TIOCGWINSZ.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// `struct utsname`, six 65-byte fields: 390 bytes.
#[repr(C)]
pub struct Utsname {
    pub bytes: [u8; 390],
}

impl Utsname {
    pub fn new() -> Self {
        Utsname { bytes: [0; 390] }
    }

    /// Field `i` as bytes up to (not including) its NUL.
    pub fn field(&self, i: usize) -> &[u8] {
        let start = i * 65;
        let end = start + 65;
        let stop = self.bytes[start..end]
            .iter()
            .position(|&b| b == 0)
            .map_or(end, |p| start + p);
        &self.bytes[start..stop]
    }
}

impl Default for Utsname {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Wait for a child: `pid` names it (`-1` or `0` means any child), `status`
/// receives the encoded exit and may be null, and `options` carries `WNOHANG`.
/// Returns the child's pid, or a negative errno.
pub fn wait4(pid: usize, status: *mut i32, options: usize) -> isize {
    sys4(nr::WAIT4, pid, status as usize, options, 0)
}

/// `waitpid(pid, status, options)` - the same call under its other name.
pub fn waitpid(pid: usize, status: *mut i32, options: usize) -> isize {
    wait4(pid, status, options)
}

/// `WIFEXITED`: the child ended by calling `exit`.
pub fn wifexited(status: i32) -> bool {
    (status & 0x7f) == 0
}

/// `WEXITSTATUS`: the low byte of the code the child passed to `exit`. Only
/// meaningful when [`wifexited`] holds.
pub fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

/// `WIFSIGNALED`: the child was killed by a signal rather than exiting.
pub fn wifsignaled(status: i32) -> bool {
    ((status & 0x7f) + 1) >> 1 > 0
}

/// `WTERMSIG`: the signal that killed the child. Only meaningful when
/// [`wifsignaled`] holds.
pub fn wtermsig(status: i32) -> i32 {
    status & 0x7f
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

/// Duplicate a descriptor onto the lowest free number.
pub fn dup(oldfd: i32) -> isize {
    sys1(nr::DUP, oldfd as usize)
}

/// The descriptor the next `open` will hand back: the lowest free one.
///
/// Discovered, not assumed. A process inherits whatever its parent had open -
/// mpos's own shell hands over exactly 0, 1 and 2, but a real one like Alpine's
/// ash keeps a descriptor of its own, and 3 is then taken. A test that wants
/// "the lowest free fd" has to ask which one that is, or it is really asserting
/// which shell started it.
pub fn free_fd() -> isize {
    let fd = dup(1);
    if fd >= 0 {
        close(fd as i32);
    }
    fd
}

/// Duplicate `oldfd` onto `newfd`, closing whatever `newfd` named.
pub fn dup2(oldfd: i32, newfd: i32) -> isize {
    sys2(nr::DUP2, oldfd as usize, newfd as usize)
}

/// [`dup2`] with flags - `O_CLOEXEC` is the one Linux accepts.
pub fn dup3(oldfd: i32, newfd: i32, flags: usize) -> isize {
    sys3(nr::DUP3, oldfd as usize, newfd as usize, flags)
}

/// Create a pipe; `flags` carries `O_CLOEXEC` and friends.
pub fn pipe2(fds: &mut [i32; 2], flags: usize) -> isize {
    sys2(nr::PIPE2, fds.as_mut_ptr() as usize, flags)
}

/// Remove `path` relative to `dirfd`; `AT_REMOVEDIR` removes a directory.
pub fn unlinkat(dirfd: i32, path: &[u8], flags: usize) -> isize {
    sys3(nr::UNLINKAT, dirfd as usize, path.as_ptr() as usize, flags)
}

/// Make a directory at `path` relative to `dirfd`; mode is ignored.
pub fn mkdirat(dirfd: i32, path: &[u8], mode: usize) -> isize {
    sys3(nr::MKDIRAT, dirfd as usize, path.as_ptr() as usize, mode)
}

/// Rename `old` to `new`, both relative to their dirfds; `flags` carries
/// `RENAME_NOREPLACE` and friends.
pub fn renameat2(
    olddirfd: i32,
    old: &[u8],
    newdirfd: i32,
    new: &[u8],
    flags: usize,
) -> isize {
    sys6(
        nr::RENAMEAT2,
        olddirfd as usize,
        old.as_ptr() as usize,
        newdirfd as usize,
        new.as_ptr() as usize,
        flags,
        0,
    )
}

/// Check reachability of `path` relative to `dirfd`.
pub fn faccessat(dirfd: i32, path: &[u8], mode: usize) -> isize {
    sys3(nr::FACCESSAT, dirfd as usize, path.as_ptr() as usize, mode)
}

/// Write the cwd path into `buf`, NUL-terminated; the return is the buffer.
pub fn getcwd(buf: &mut [u8]) -> isize {
    sys2(nr::GETCWD, buf.as_mut_ptr() as usize, buf.len())
}

/// Move to the directory `path` names; relative paths start at the cwd.
pub fn chdir(path: &[u8]) -> isize {
    sys1(nr::CHDIR, path.as_ptr() as usize)
}

/// Move to the directory the descriptor names.
pub fn fchdir(fd: i32) -> isize {
    sys1(nr::FCHDIR, fd as usize)
}

/// Scatter read: fill each iovec in turn, stopping early at end-of-file.
pub fn readv(fd: i32, iov: &mut [Iovec]) -> isize {
    sys3(nr::READV, fd as usize, iov.as_mut_ptr() as usize, iov.len())
}

/// Scatter write: write each iovec in turn.
pub fn writev(fd: i32, iov: &[Iovec]) -> isize {
    sys3(nr::WRITEV, fd as usize, iov.as_ptr() as usize, iov.len())
}

/// Read directory entries in the Linux getdents64 format into `buf`.
pub fn getdents64(fd: i32, buf: &mut [u8]) -> isize {
    sys3(nr::GETDENTS64, fd as usize, buf.as_mut_ptr() as usize, buf.len())
}

/// One parsed getdents64 record: the fields callers branch on.
pub struct Dirent64<'a> {
    pub d_ino: u64,
    pub d_off: u64,
    pub d_type: u8,
    pub name: &'a [u8],
}

/// Parse the first record of a getdents64 buffer, with its length.
///
/// The record count is whatever the caller can walk with this: `d_reclen`
/// steps record to record until the byte count the syscall returned runs out.
pub fn parse_dirent64(buf: &[u8]) -> Option<(Dirent64<'_>, usize)> {
    if buf.len() < 19 {
        return None;
    }
    let d_ino = u64::from_le_bytes(buf[0..8].try_into().ok()?);
    let d_off = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    let reclen = u16::from_le_bytes(buf[16..18].try_into().ok()?) as usize;
    if reclen < 19 || reclen > buf.len() {
        return None;
    }
    let d_type = buf[18];
    let name = &buf[19..reclen];
    let stop = name.iter().position(|&b| b == 0)?;
    Some((
        Dirent64 {
            d_ino,
            d_off,
            d_type,
            name: &name[..stop],
        },
        reclen,
    ))
}

/// Device ioctl: `TIOCGWINSZ`, `TCGETS`, and friends.
pub fn ioctl(fd: i32, request: usize, arg: usize) -> isize {
    sys3(nr::IOCTL, fd as usize, request, arg)
}

/// `uname(buf)`: fill the six 65-byte fields of `struct utsname`.
pub fn uname(buf: &mut Utsname) -> isize {
    sys1(nr::UNAME, buf.bytes.as_mut_ptr() as usize)
}

/// `clock_gettime(clk_id, tp)`.
pub fn clock_gettime(clk_id: usize, tp: &mut TimeSpec) -> isize {
    sys2(nr::CLOCK_GETTIME, clk_id, tp as *mut TimeSpec as usize)
}

/// `gettimeofday(tv, tz)`.
pub fn gettimeofday(tv: &mut TimeVal) -> isize {
    sys2(nr::GETTIMEOFDAY, tv as *mut TimeVal as usize, 0)
}

/// `nanosleep(req, rem)`.
pub fn nanosleep(req: &TimeSpec, rem: &mut TimeSpec) -> isize {
    sys2(
        nr::NANOSLEEP,
        req as *const TimeSpec as usize,
        rem as *mut TimeSpec as usize,
    )
}

/// `clock_nanosleep(clk_id, flags, req, rem)`.
pub fn clock_nanosleep(clk_id: usize, flags: usize, req: &TimeSpec, rem: &mut TimeSpec) -> isize {
    sys4(
        nr::CLOCK_NANOSLEEP,
        clk_id,
        flags,
        req as *const TimeSpec as usize,
        rem as *mut TimeSpec as usize,
    )
}

/// `kill(pid, sig)`: signal 0 probes for liveness.
pub fn kill(pid: i32, sig: i32) -> isize {
    sys2(nr::KILL, pid as usize, sig as usize)
}

/// The id of whoever spawned this process, 0 when there is none.
pub fn getppid() -> isize {
    sys0(nr::GETPPID)
}

/// `getuid`, and its gid/euid/egid siblings: 0, always, on a one-user kernel.
pub fn getuid() -> isize {
    sys0(nr::GETUID)
}

pub fn getgid() -> isize {
    sys0(nr::GETGID)
}

pub fn geteuid() -> isize {
    sys0(nr::GETEUID)
}

pub fn getegid() -> isize {
    sys0(nr::GETEGID)
}

/// `mremap(old, old_size, new_size, flags, new_address)`.
pub fn mremap(
    old_addr: usize,
    old_len: usize,
    new_len: usize,
    flags: usize,
    new_addr: usize,
) -> isize {
    sys5(nr::MREMAP, old_addr, old_len, new_len, flags, new_addr)
}

/// `MREMAP_MAYMOVE`: the mapping may be moved to satisfy the resize.
pub const MREMAP_MAYMOVE: usize = 0x1;
/// `MREMAP_FIXED`: move to exactly the address named in the fifth argument.
pub const MREMAP_FIXED: usize = 0x2;

/// `madvise(addr, len, advice)` - accepted and answered 0.
pub fn madvise(addr: usize, len: usize, advice: usize) -> isize {
    sys3(nr::MADVISE, addr, len, advice)
}

/// `futex(uaddr, op, val)`: the WAIT/WAKE pair musl's locks lean on.
pub fn futex(uaddr: *mut i32, op: usize, val: i32) -> isize {
    sys3(nr::FUTEX, uaddr as usize, op, val as usize)
}

/// `fork()`: the caller is duplicated. Returns 0 in the child and the child's
/// pid in the parent, and both come back here.
pub fn fork() -> isize {
    sys0(nr::FORK)
}

/// `clone(flags, child_stack, parent_tid, child_tid, tls)`: `fork` with the
/// caller naming what the child shares instead of copying.
pub fn clone(
    flags: usize,
    child_stack: usize,
    parent_tid: *mut i32,
    child_tid: *mut i32,
    tls: usize,
) -> isize {
    sys5(
        nr::CLONE,
        flags,
        child_stack,
        parent_tid as usize,
        child_tid as usize,
        tls,
    )
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

// ---------------------------------------------------------------------------
// Sockets: the AF_INET protocol family
// ---------------------------------------------------------------------------
//
// A kernel's network stack is only as real as the programs that speak a
// protocol over it, and a protocol is only exercised by a program that
// builds its own packets - which is what the tests behind these wrappers
// do. The shapes are Linux's: `struct sockaddr_in` is sixteen bytes of
// family, port, address and padding, and every port travels in network
// order.

pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM: usize = 2;
pub const SOL_SOCKET: usize = 1;
pub const SO_REUSEADDR: usize = 2;
pub const SO_TYPE: usize = 3;
pub const SO_ERROR: usize = 4;
/// sizeof(struct sockaddr_in).
pub const SOCKADDR_IN_LEN: usize = 16;

/// A pointer-sized view of `sockaddr_in`, for the calls that take the
/// struct as a pointer and a length word.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddrIn {
    pub family: u16,
    /// Network byte order, as the wire has it.
    pub port: u16,
    pub addr: [u8; 4],
    pub zero: [u8; 8],
}

impl SockAddrIn {
    /// Name `addr` and `port`, with the port converted to network order.
    pub fn new(addr: [u8; 4], port: u16) -> Self {
        SockAddrIn {
            family: AF_INET,
            port: port.to_be(),
            addr,
            zero: [0; 8],
        }
    }

    /// The port in host order - what a caller compares against.
    pub fn port(&self) -> u16 {
        u16::from_be(self.port)
    }
}

const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_SENDTO: u64 = 44;
const SYS_RECVFROM: u64 = 45;
const SYS_SHUTDOWN: u64 = 48;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_GETSOCKNAME: u64 = 51;
const SYS_GETPEERNAME: u64 = 52;
const SYS_SETSOCKOPT: u64 = 54;
const SYS_GETSOCKOPT: u64 = 55;

/// `socket(domain, type, protocol)` — a descriptor for a channel of the
/// named family and kind.
pub fn socket(domain: usize, typ: usize, protocol: usize) -> isize {
    sys3(SYS_SOCKET, domain, typ, protocol)
}

/// `connect(fd, addr, len)` — open a stream to the named endpoint, or name
/// the peer a datagram socket sends to by default.
pub fn connect(fd: i32, addr: &SockAddrIn) -> isize {
    sys3(
        SYS_CONNECT,
        fd as usize,
        addr as *const SockAddrIn as usize,
        SOCKADDR_IN_LEN,
    )
}

/// `bind(fd, addr, len)` — name the local end of a socket.
pub fn bind(fd: i32, addr: &SockAddrIn) -> isize {
    sys3(
        SYS_BIND,
        fd as usize,
        addr as *const SockAddrIn as usize,
        SOCKADDR_IN_LEN,
    )
}

/// `listen(fd, backlog)` — say this stream socket takes connections.
pub fn listen(fd: i32, backlog: usize) -> isize {
    sys2(SYS_LISTEN, fd as usize, backlog)
}

/// `sendto(fd, buf, flags, addr, len)` — send, to `addr` when one is named.
pub fn sendto(fd: i32, buf: &[u8], flags: usize, addr: Option<&SockAddrIn>) -> isize {
    let (addr_ptr, addr_len) = match addr {
        Some(a) => (a as *const SockAddrIn as usize, SOCKADDR_IN_LEN),
        None => (0, 0),
    };
    sys6(
        SYS_SENDTO,
        fd as usize,
        buf.as_ptr() as usize,
        buf.len(),
        flags,
        addr_ptr,
        addr_len,
    )
}

/// `recvfrom(fd, buf, flags, addr, len)` — receive, and name the sender
/// when slots for it are given.
pub fn recvfrom(
    fd: i32,
    buf: &mut [u8],
    flags: usize,
    addr: Option<&mut SockAddrIn>,
) -> isize {
    // The length word is in-out: the kernel writes back how much of the
    // struct it filled, so it has to be a real mutable local and not the
    // constant it starts out equal to.
    let mut addr_len = SOCKADDR_IN_LEN;
    let (addr_ptr, len_ptr) = match addr {
        Some(a) => (
            a as *mut SockAddrIn as usize,
            &mut addr_len as *mut usize as usize,
        ),
        None => (0, 0),
    };
    sys6(
        SYS_RECVFROM,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        flags,
        addr_ptr,
        len_ptr,
    )
}

/// `getsockname(fd, addr, len)` — the local end this socket ended up with.
pub fn getsockname(fd: i32, addr: &mut SockAddrIn) -> isize {
    let mut addr_len = SOCKADDR_IN_LEN;
    sys3(
        SYS_GETSOCKNAME,
        fd as usize,
        addr as *mut SockAddrIn as usize,
        &mut addr_len as *mut usize as usize,
    )
}

/// `getpeername(fd, addr, len)` — the far end, for a connected socket.
pub fn getpeername(fd: i32, addr: &mut SockAddrIn) -> isize {
    let mut addr_len = SOCKADDR_IN_LEN;
    sys3(
        SYS_GETPEERNAME,
        fd as usize,
        addr as *mut SockAddrIn as usize,
        &mut addr_len as *mut usize as usize,
    )
}

/// `setsockopt(fd, level, optname, val, len)`.
pub fn setsockopt(fd: i32, level: usize, optname: usize, val: &i32) -> isize {
    sys5(
        SYS_SETSOCKOPT,
        fd as usize,
        level,
        optname,
        val as *const i32 as usize,
        4,
    )
}

/// `getsockopt(fd, level, optname, val, len)`.
pub fn getsockopt(fd: i32, level: usize, optname: usize, val: &mut i32) -> isize {
    sys5(
        SYS_GETSOCKOPT,
        fd as usize,
        level,
        optname,
        val as *mut i32 as usize,
        &mut 4usize as *mut usize as usize,
    )
}

/// `shutdown(fd, how)` — close one direction of a channel, or both.
pub fn shutdown(fd: i32, how: usize) -> isize {
    sys2(SYS_SHUTDOWN, fd as usize, how)
}

// ---------------------------------------------------------------------------
// The calls an ordinary libc reaches for
// ---------------------------------------------------------------------------

const SYS_PREAD64: u64 = 17;
const SYS_PWRITE64: u64 = 18;
const SYS_FSYNC: u64 = 74;
const SYS_CHMOD: u64 = 90;
const SYS_FCHMOD: u64 = 91;
const SYS_TIME: u64 = 201;
const SYS_TIMES: u64 = 100;
const SYS_GETRUSAGE: u64 = 98;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_WAITID: u64 = 247;
const SYS_GETRLIMIT: u64 = 160;

/// `pread64(fd, buf, count, offset)` — read at a position, leaving the
/// descriptor's own position where it was.
pub fn pread(fd: i32, buf: &mut [u8], offset: i64) -> isize {
    sys4(
        SYS_PREAD64,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        offset as usize,
    )
}

/// `pwrite64(fd, buf, count, offset)` — write at a position, likewise.
pub fn pwrite(fd: i32, buf: &[u8], offset: i64) -> isize {
    sys4(
        SYS_PWRITE64,
        fd as usize,
        buf.as_ptr() as usize,
        buf.len(),
        offset as usize,
    )
}

/// `fsync(fd)` — flush a descriptor to its storage.
pub fn fsync(fd: i32) -> isize {
    sys1(SYS_FSYNC, fd as usize)
}

/// `chmod(path, mode)` — set permission bits on a path.
pub fn chmod(path: &[u8], mode: usize) -> isize {
    sys2(SYS_CHMOD, path.as_ptr() as usize, mode)
}

/// `fchmod(fd, mode)` — the same, by descriptor.
pub fn fchmod(fd: i32, mode: usize) -> isize {
    sys2(SYS_FCHMOD, fd as usize, mode)
}

/// `time(tloc)` — seconds since the epoch, written through `tloc` when
/// it is given.
pub fn time(tloc: Option<&mut i64>) -> isize {
    let ptr = match tloc {
        Some(slot) => slot as *mut i64 as usize,
        None => 0,
    };
    sys1(SYS_TIME, ptr)
}

/// `times(buf)` — CPU time in clock ticks. `buf` is a four-`clock_t`
/// `struct tms`; the return value is ticks since boot.
pub fn times(buf: Option<&mut [u8; 32]>) -> isize {
    let ptr = match buf {
        Some(b) => b.as_mut_ptr() as usize,
        None => 0,
    };
    sys1(SYS_TIMES, ptr)
}

/// `getrusage(who, usage)` — resource counters, zeroed when unmeasured.
pub fn getrusage(who: usize, usage: &mut [u8; 144]) -> isize {
    sys2(SYS_GETRUSAGE, who, usage.as_mut_ptr() as usize)
}

/// `clock_getres(clk_id, res)` — the resolution a clock can deliver.
pub fn clock_getres(clk_id: usize, res: &mut TimeSpec) -> isize {
    sys2(SYS_CLOCK_GETRES, clk_id, res as *mut TimeSpec as usize)
}

/// `getrlimit(resource, rlim)` — one resource limit.
pub fn getrlimit(resource: usize, rlim: &mut Rlimit) -> isize {
    sys2(SYS_GETRLIMIT, resource, rlim as *mut Rlimit as usize)
}

/// `waitid(idtype, id, infop, options, rusage)` — wait for one child and
/// describe it as a `siginfo_t`.
pub fn waitid(idtype: usize, id: usize, infop: &mut [u8; 128], options: usize) -> isize {
    sys5(
        SYS_WAITID,
        idtype,
        id,
        infop.as_mut_ptr() as usize,
        options,
        0,
    )
}

// --- the calls that were already there and had no test ---------------------

const SYS_PPOLL: u64 = 271;
const SYS_SELECT: u64 = 23;
const SYS_PSELECT6: u64 = 270;
const SYS_STAT: u64 = 4;
const SYS_LSTAT: u64 = 6;
const SYS_SYMLINK: u64 = 88;
const SYS_UMASK: u64 = 95;
const SYS_TRUNCATE: u64 = 76;
const SYS_FTRUNCATE: u64 = 77;
const SYS_FLOCK: u64 = 73;
const SYS_SENDFILE: u64 = 40;
const SYS_GETPPID: u64 = 110;
const SYS_LINKAT: u64 = 265;
const SYS_MKNOD: u64 = 133;
/// 82 is the legacy `rename`; 264 is `renameat`, which leads with two
/// dirfds. They are different calls with different register layouts, which
/// is why both are here.
const SYS_RENAME: u64 = 82;
const SYS_RENAMEAT: u64 = 264;

/// `ppoll(fds, nfds, timeout, sigmask)` — poll with a nanosecond timeout.
pub fn ppoll(fds: &mut [PollFd], timeout: &TimeSpec) -> isize {
    sys5(
        SYS_PPOLL,
        fds.as_mut_ptr() as usize,
        fds.len(),
        timeout as *const TimeSpec as usize,
        0,
        8,
    )
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)` — the original
/// wait, in microseconds. `timeout` is a `TimeVal`.
pub fn select(
    nfds: usize,
    readfds: Option<&mut [u8]>,
    timeout: Option<&TimeVal>,
) -> isize {
    let (rfds, _) = match readfds {
        Some(set) => (set.as_mut_ptr() as usize, set.len()),
        None => (0, 0),
    };
    let tv = match timeout {
        Some(t) => t as *const TimeVal as usize,
        None => 0,
    };
    sys5(SYS_SELECT, nfds, rfds, 0, 0, tv)
}

/// `pselect6(nfds, ..., timeout, sigmask)` — select with a `timespec`.
pub fn pselect6(nfds: usize, readfds: Option<&mut [u8]>, timeout: &TimeSpec) -> isize {
    let (rfds, _) = match readfds {
        Some(set) => (set.as_mut_ptr() as usize, set.len()),
        None => (0, 0),
    };
    sys6(
        SYS_PSELECT6,
        nfds,
        rfds,
        0,
        0,
        timeout as *const TimeSpec as usize,
        0,
    )
}

/// `stat(path, buf)` — the legacy path-shaped stat, no dirfd.
pub fn stat(path: &[u8], buf: *mut Stat) -> isize {
    sys2(SYS_STAT, path.as_ptr() as usize, buf as usize)
}

/// `lstat(path, buf)` — the same, but naming a link rather than its target.
pub fn lstat(path: &[u8], buf: *mut Stat) -> isize {
    sys2(SYS_LSTAT, path.as_ptr() as usize, buf as usize)
}

/// `symlink(target, linkpath)` — name a new symbolic link.
pub fn symlink(target: &[u8], linkpath: &[u8]) -> isize {
    sys2(SYS_SYMLINK, target.as_ptr() as usize, linkpath.as_ptr() as usize)
}

/// `umask(mask)` — set the file-creation mask, returning the old one.
pub fn umask(mask: usize) -> isize {
    sys1(SYS_UMASK, mask)
}

/// `truncate(path, len)` — set a file's length by path.
pub fn truncate(path: &[u8], len: usize) -> isize {
    sys2(SYS_TRUNCATE, path.as_ptr() as usize, len)
}

/// `ftruncate(fd, len)` — the same, by descriptor.
pub fn ftruncate(fd: i32, len: usize) -> isize {
    sys2(SYS_FTRUNCATE, fd as usize, len)
}

/// `flock(fd, op)` — advisory file locking.
pub fn flock(fd: i32, op: usize) -> isize {
    sys2(SYS_FLOCK, fd as usize, op)
}

/// `sendfile(out, in, offset, count)` — copy between descriptors.
pub fn sendfile(out: i32, input: i32, count: usize) -> isize {
    sys4(SYS_SENDFILE, out as usize, input as usize, 0, count)
}


/// `rename(old, new)` — the legacy two-path spelling.
pub fn rename(old: &[u8], new: &[u8]) -> isize {
    sys2(SYS_RENAME, old.as_ptr() as usize, new.as_ptr() as usize)
}

/// `renameat(olddirfd, old, newdirfd, new)` — the four-argument spelling
/// that carries its own dirfds. This is the one `apk` uses to move a
/// fetched index into the cache.
pub fn renameat(olddirfd: i32, old: &[u8], newdirfd: i32, new: &[u8]) -> isize {
    sys4(
        SYS_RENAMEAT,
        olddirfd as usize,
        old.as_ptr() as usize,
        newdirfd as usize,
        new.as_ptr() as usize,
    )
}

/// `linkat(olddirfd, old, newdirfd, new, flags)` — a second name for a file.
pub fn linkat(olddirfd: i32, old: &[u8], newdirfd: i32, new: &[u8], flags: usize) -> isize {
    sys5(
        SYS_LINKAT,
        olddirfd as usize,
        old.as_ptr() as usize,
        newdirfd as usize,
        new.as_ptr() as usize,
        flags,
    )
}

/// `mknod(path, mode, dev)` — ask for a device node.
pub fn mknod(path: &[u8], mode: usize, dev: usize) -> isize {
    sys3(SYS_MKNOD, path.as_ptr() as usize, mode, dev)
}
