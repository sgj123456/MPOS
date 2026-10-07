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
#[path = "../../../src/syscall/linux/nr.rs"]
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

/// A raw syscall through the harness's own gate, for the calls no typed
/// wrapper has earned yet (socket by family number today).
pub fn raw_sys(number: u64, a1: usize, a2: usize, a3: usize) -> isize {
    sys3(number, a1, a2, a3)
}

/// The two-argument form of the same escape.
pub fn raw_sys2(number: u64, a1: usize, a2: usize) -> isize {
    sys2(number, a1, a2)
}

/// The five- and six-argument forms: `setsockopt` and `sendmsg` shaped
/// calls, whose later registers the three-argument escape cannot fill.
pub fn raw_sys5(number: u64, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
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

/// The whole six-argument escape: `sendto` and `recvfrom`, whose address
/// and address-length pair lives in `r9`.
pub fn raw_sys6(
    number: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
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
/// EADDRINUSE: a bind whose name is taken, and the address a socket
/// cannot share.
pub const EADDRINUSE: isize = -98;
/// ENXIO: the file doors' answer at a socket's name.
pub const ENXIO: isize = -6;
/// ECONNREFUSED, EISCONN, ENOTCONN: the connect-side refusals a unix
/// socket hands out.
pub const ECONNREFUSED: isize = -111;
pub const EISCONN: isize = -106;
pub const ENOTCONN: isize = -107;
pub const ESRCH: isize = -3;
pub const EINTR: isize = -4;
pub const E2BIG: isize = -7;
pub const ENOEXEC: isize = -8;
pub const EBADF: isize = -9;
pub const ECHILD: isize = -10;
pub const EAGAIN: isize = -11;
pub const ENOMEM: isize = -12;
pub const EFAULT: isize = -14;
pub const EBUSY: isize = -16;
pub const EEXIST: isize = -17;
pub const ENODEV: isize = -19;
pub const ENOTDIR: isize = -20;
pub const EISDIR: isize = -21;
pub const EINVAL: isize = -22;
pub const ENOTTY: isize = -25;
pub const EIO: isize = -5;
pub const ENOSPC: isize = -28;
pub const ESPIPE: isize = -29;
pub const ERANGE: isize = -34;
pub const ELOOP: isize = -40;
pub const ENOSYS: isize = -38;
/// The errnos of the POSIX completion set: permission, cross-device,
/// unsupported operation, and the cancellation a CANCEL_ON_SET timer
/// reports.
pub const EACCES: isize = -13;
pub const EXDEV: isize = -18;
pub const EOPNOTSUPP: isize = -95;
pub const ECANCELED: isize = -125;

// ---------------------------------------------------------------------------
// Linux x86_64 ABI constants the tests pass or check.
// ---------------------------------------------------------------------------

pub const PROT_NONE: usize = 0;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 0x01;
/// `memfd_create(2)` flags and the `fcntl` sealing commands.
pub const MFD_CLOEXEC: usize = 0o001;
pub const MFD_ALLOW_SEALING: usize = 0o002;
pub const F_ADD_SEALS: usize = 1033;
pub const F_GET_SEALS: usize = 1034;
pub const F_SEAL_SHRINK: usize = 0x02;
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
/// The terminal's window changed size. It is what a full-screen program
/// redraws on, and it is raised by `TIOCSWINSZ`.
pub const SIGWINCH: i32 = 28;
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
/// O_NOFOLLOW: refuse to follow a symbolic link at the final path component.
pub const O_NOFOLLOW: usize = 0o40_0000;
pub const O_CLOEXEC: usize = 0x8000_0;
/// `O_NONBLOCK`: `pipe2`'s other flag, and the one bit `F_SETFL`/`F_GETFL`
/// carries that changes how a descriptor behaves.
pub const O_NONBLOCK: usize = 0o4000;
/// descriptor flags for F_GETFD/F_SETFD.
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
/// `FIONBIO`: set or clear `O_NONBLOCK` through `ioctl` rather than `fcntl`.
/// The request Rust's std uses on Linux to put a pipe in non-blocking mode
/// before it polls it, which is why it has to exist at all.
pub const FIONBIO: usize = 0x5421;
pub const FD_CLOEXEC: usize = 1;
/// access modes.
pub const F_OK: usize = 0;
pub const R_OK: usize = 4;
pub const W_OK: usize = 2;
pub const X_OK: usize = 1;
/// `AT_SYMLINK_NOFOLLOW` / `AT_EACCESS`: the flags the flagged forms take.
pub const AT_SYMLINK_NOFOLLOW: usize = 0x100;
pub const AT_EACCESS: usize = 0x200;

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
pub const TIOCSWINSZ: usize = 0x5414;
pub const TCGETS: usize = 0x5401;
pub const TCSETS: usize = 0x5402;
/// The pty master's pair: the slave's `/dev/pts` number, and the lock
/// `grantpt` clears.
pub const TIOCGPTN: usize = 0x80045430;
pub const TIOCSPTLCK: usize = 0x40045431;

/// wait4/waitpid options.
pub const WNOHANG: usize = 1;

/// signals beyond the three the strace showed.
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;
pub const SIGSTOP: i32 = 19;
/// `SIGCONT`: the one signal that is an instruction to run rather than to
/// stop. A stopped task does not act on anything else first - it is not
/// scheduled - so resuming is what makes any later signal reach it.
pub const SIGCONT: i32 = 18;
pub const SIGINT: i32 = 2;
pub const SIGALRM: i32 = 14;
/// `si_code` values a handler reads out of `siginfo_t`: a bare `kill`, a
/// `sigqueue` carrying a payload, and an interval timer's expiry.
pub const SI_USER: i32 = 0;
pub const SI_QUEUE: i32 = -1;
/// `SI_TIMER`: the record a POSIX timer's expiry carries - the code musl's
/// `SIGEV_THREAD` helper checks before it calls a callback.
pub const SI_TIMER: i32 = -2;

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

/// prlimit64 resources: `asm-generic/resource.h`'s numbers, which is what a
/// caller's `prlimit64` speaks. `RLIMIT_NOFILE` is 7, not the 5 it was written
/// as here - 5 is `RLIMIT_RSS`, and the table used to hold the descriptor
/// ceiling on that row (fixed with the table; see `crates/abi`'s
/// `default_rlimits`).
pub const RLIMIT_STACK: i32 = 3;
pub const RLIMIT_NOFILE: i32 = 7;
pub const RLIM64_INFINITY: u64 = u64::MAX;

/// file type bits of st_mode / stx_mode.
pub const S_IFMT: u32 = 0xF000;
pub const S_IFREG: u32 = 0x8000;
pub const S_IFDIR: u32 = 0x4000;

/// statx request mask bits.
pub const STATX_TYPE: u32 = 0x1;
pub const STATX_SIZE: u32 = 0x200;
/// A birth time, which this kernel does not record and must not claim.
pub const STATX_BTIME: u32 = 0x800;
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

/// Linux `struct statx`: 256 bytes, the same layout on every machine.
///
/// The length is the struct and not a head of it: Linux zeroes all 256 bytes and
/// copies them out, so a caller must hand over 256 - a kernel that writes only
/// through `stx_mnt_id` leaves the caller's own bytes in the tail, and a program
/// that reads a field out there reads its own memory. The fields after the
/// direct-I/O alignments are the spares Linux keeps for future expansion.
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
    pub stx_dio_mem_align: u32,    // 152
    pub stx_dio_offset_align: u32, // 156
    pub _spare: [u64; 12],         // 160..256
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

/// The `siginfo_t` a `SA_SIGINFO` handler is handed: signo at 0, `si_code` at
/// 8, and - for a `sigqueue` - the `union sigval` at 24. Padded to the ABI's
/// full 128 bytes so it can also be built on the stack and passed to
/// `rt_sigqueueinfo`.
#[repr(C)]
pub struct SigInfo {
    pub signo: i32, // 0
    pub errno: i32, // 4
    pub code: i32,  // 8
    pub _pad: i32,  // 12
    pub pid: i32,   // 16
    pub uid: i32,   // 20
    pub value: u64, // 24 - `union sigval`
    pub _rest: [u8; 128 - 32],
}

impl SigInfo {
    pub fn zero() -> Self {
        SigInfo {
            signo: 0,
            errno: 0,
            code: 0,
            _pad: 0,
            pid: 0,
            uid: 0,
            value: 0,
            _rest: [0; 128 - 32],
        }
    }
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

/// `struct termios`, the shape `TCGETS` and `TCSETS` carry: four flag words, a
/// line-discipline number, the control characters, then the two speeds.
///
/// Spelled once, here, because putting a terminal into raw mode is not one
/// test's business: it is what every caller of the read side has to do to ask
/// what the kernel then answers - a full-screen program, a `VMIN`/`VTIME` wait,
/// an interrupt key - and a second spelling of the same ABI in a second program
/// is a second answer to a question the ABI already settled.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; 32],
    pub ispeed: u32,
    pub ospeed: u32,
}

/// `c_lflag` bits a test names.
pub const ISIG: u32 = 0x0001;
pub const ICANON: u32 = 0x0002;
pub const ECHO: u32 = 0x0008;

/// `c_cc` indices a test names. `VMIN`/`VTIME` are the pair a non-canonical
/// read is built on, and they are only meaningful in that mode: see
/// `t-tty-vtime`.
pub const VINTR: usize = 0;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;

/// `struct itimerval`: the reload interval, then the time left to run.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Itimerval {
    pub it_interval: TimeVal,
    pub it_value: TimeVal,
}

/// The wall-clock interval timer, the one that answers with `SIGALRM`.
pub const ITIMER_REAL: usize = 0;

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

/// Leave, and only this thread. `exit(2)` ends the caller; its siblings in the
/// same process carry on, which is the difference `pthread_exit` is made of.
/// `exit_group` is the one that ends them all.
pub fn exit(code: i32) -> ! {
    sys1(nr::EXIT, code as usize);
    unreachable!("EXIT never returns")
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

// Creating a thread has to be written in assembly, and this is why: the child
// does not start at a function - it resumes at the *instruction after the
// syscall*, on the stack its creator handed over. Every register and every
// stack slot the calling function was using belongs to the caller's frame, so
// a compiled wrapper that read them back would be reading its creator's stack.
// A libc's `__clone` therefore lays what the thread needs at the top of the
// new stack and lets the child pop it, and so does this one:
//
//   * `rdi` = flags, `rsi` = the new stack top (with entry and argument at it),
//     `rdx` = the slot the child's tid is parked in;
//   * the child (rax == 0 on return) pops the entry point and its argument,
//     calls it - with the stack the caller chose - and leaves with `exit(2)`,
//     which ends that thread and not the process.
global_asm!(
    ".globl mpos_thread_create",
    "mpos_thread_create:",
    "  mov r10, rdx", // child_tid
    "  xor edx, edx", // no parent_tid slot
    "  xor r8d, r8d", // no TLS descriptor
    "  mov eax, 56",  // SYS_clone
    "  syscall",
    "  test eax, eax",
    "  jnz 1f",  // the parent returns with the tid
    "  pop rax", // the thread: entry point
    "  pop rdi", // and its argument
    "  call rax",
    "  xor edi, edi",
    "  mov eax, 60", // SYS_exit, this thread only
    "  syscall",
    "1: ret",
);

unsafe extern "C" {
    fn mpos_thread_create(flags: usize, stack_top: usize, child_tid: usize) -> isize;
}

/// Start a thread that runs `entry(arg)` on `stack_top`.
///
/// `stack_top` is the first byte *past* the caller's mapping, and the two
/// values the thread needs are written just below it: the red zone a function
/// may use above its stack pointer has to stay inside the mapping, which is
/// why an argument at the very edge is a fault and not a stack.
pub fn thread_create(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    arg: usize,
    tid_slot: *mut i32,
) -> isize {
    let top = (stack_top - 16) & !0xf;
    unsafe {
        let cells = top as *mut usize;
        cells.write(entry as usize);
        cells.add(1).write(arg);
        mpos_thread_create(
            nr::CLONE_VM
                | nr::CLONE_FS
                | nr::CLONE_FILES
                | nr::CLONE_SIGHAND
                | nr::CLONE_THREAD
                | nr::CLONE_CHILD_SETTID,
            top,
            tid_slot as usize,
        )
    }
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

/// `memfd_create(name, flags)` — an anonymous shared buffer as a descriptor.
/// `name` is passed verbatim, so the caller supplies its trailing NUL.
pub fn memfd_create(name: &[u8], flags: usize) -> isize {
    raw_sys(319, name.as_ptr() as usize, flags, 0)
}

pub fn mprotect(addr: usize, len: usize, prot: usize) -> isize {
    sys3(nr::MPROTECT, addr, len, prot)
}

/// The size of the stack [`thread_stack_top`] hands a spawned thread: 32 KiB,
/// the anonymous mapping `t-clone` and `t-futex` both give their thread.
pub const THREAD_STACK_BYTES: usize = 0x8000;

/// A fresh thread stack, answered as the address to spawn it on - the first
/// byte *past* the mapping, because `mpos_thread_create`'s child pops its entry
/// point from just below there. A negative answer is the mapping's own errno.
///
/// This is the spawn scaffold `t-clone` and `t-futex` share: map the stack,
/// report it, and call [`thread_create`] with the top.
pub fn thread_stack_top() -> isize {
    let base = mmap(
        0,
        THREAD_STACK_BYTES,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    if base <= 0 {
        base
    } else {
        base + THREAD_STACK_BYTES as isize
    }
}

/// The address the read/write EFAULT tests map their `PROT_NONE` guard page at:
/// far from the image base and the kernel's mmap bump, and the same in every
/// test that wants one.
pub const GUARD_PAGE: usize = 0x5000_0000;

/// Map [`GUARD_PAGE`] read-unreachable and answer its address, or a negative
/// errno. A `PROT_NONE` page is the stand-in for the unmapped page Linux's own
/// EFAULT tests use: the kernel may neither read nor write it, which is what
/// makes a `read` into it and a `write` out of it both `EFAULT`.
pub fn guard_page() -> isize {
    mmap(
        GUARD_PAGE,
        4096,
        PROT_NONE,
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
        -1,
        0,
    )
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
/// Give the CPU up, and let the scheduler choose again. A thread waiting for
/// another one to make progress yields rather than spinning at full speed.
pub fn sched_yield() -> isize {
    sys0(nr::SCHED_YIELD)
}

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

/// `posix_fadvise(fd, offset, len, advice)` — the raw `fadvise64` call. At the
/// syscall ABI it answers `0` or a negative errno directly (the libc wrapper
/// negates it), and the x86_64 argument order is `(fd, offset, len, advice)`.
pub fn fadvise64(fd: i32, offset: usize, len: usize, advice: usize) -> isize {
    sys4(nr::FADVISE64, fd as usize, offset, len, advice)
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
pub fn renameat2(olddirfd: i32, old: &[u8], newdirfd: i32, new: &[u8], flags: usize) -> isize {
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

/// The flagged spelling of [`faccessat`]: `flags` carries
/// `AT_SYMLINK_NOFOLLOW` and `AT_EACCESS`.
pub fn faccessat2(dirfd: i32, path: &[u8], mode: usize, flags: usize) -> isize {
    sys4(
        nr::FACCESSAT2,
        dirfd as usize,
        path.as_ptr() as usize,
        mode,
        flags,
    )
}

// --- the epoll family ------------------------------------------------------

/// The `EPOLL_EVENTS` bits, in the numbering the headers spell.
pub const EPOLLIN: u32 = 0x001;
pub const EPOLLPRI: u32 = 0x002;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDNORM: u32 = 0x040;
pub const EPOLLWRNORM: u32 = 0x100;
pub const EPOLLRDHUP: u32 = 0x2000;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;

/// The three `epoll_ctl` operations.
pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;

/// `EPOLL_CLOEXEC`, the one flag `epoll_create1` takes.
pub const EPOLL_CLOEXEC: usize = 0o200_0000;

/// One `struct epoll_event` of the x86_64 ABI: twelve bytes and *packed* -
/// `data` sits at byte 4, not 8, because a u32 and a u64 share the struct
/// with no alignment padding between them. Reading the fields through
/// `epoll_wait`'s answer is the test that pins those offsets.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

/// `epoll_create(size)`: the size argument is ignored but must be positive.
pub fn epoll_create(size: i32) -> isize {
    sys1(nr::EPOLL_CREATE, size as usize)
}

/// `epoll_create1(flags)`.
pub fn epoll_create1(flags: usize) -> isize {
    sys1(nr::EPOLL_CREATE1, flags)
}

/// `epoll_ctl(epfd, op, fd, event)`: add, change or drop one entry. `DEL`
/// tolerates a null event.
pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> isize {
    sys4(
        nr::EPOLL_CTL,
        epfd as usize,
        op as usize,
        fd as usize,
        event as usize,
    )
}

/// `epoll_wait(epfd, events, maxevents, timeout)`: collect up to
/// `maxevents` records, `timeout` in milliseconds (-1 waits for ever).
pub fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout_ms: i32) -> isize {
    sys4(
        nr::EPOLL_WAIT,
        epfd as usize,
        events as usize,
        maxevents as usize,
        timeout_ms as usize,
    )
}

// --- the timerfd family ----------------------------------------------------

/// `TFD_*`: the create flags and the settime flags, in the header's
/// numbering - the create pair are the O_ flags under other names.
pub const TFD_NONBLOCK: usize = 0x800;
pub const TFD_CLOEXEC: usize = 0o200_0000;
pub const TFD_TIMER_ABSTIME: i32 = 0x1;
pub const TFD_TIMER_CANCEL_ON_SET: i32 = 0x2;

/// The further clocks `timerfd_create` accepts, in the clockids the ABI
/// numbers. CLOCK_REALTIME and CLOCK_MONOTONIC are already named above,
/// for the clock_gettime tests.
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_BOOTTIME: i32 = 7;

/// `struct itimerspec`: the interval first, the initial value second - the
/// reverse of `struct itimerval`, and the thing a guess gets wrong.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Itimerspec {
    pub interval: TimeSpec,
    pub value: TimeSpec,
}

impl Itimerspec {
    pub fn new() -> Self {
        Itimerspec {
            interval: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            value: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        }
    }
}

impl Default for Itimerspec {
    fn default() -> Self {
        Self::new()
    }
}

/// `timerfd_create(clockid, flags)`.
pub fn timerfd_create(clockid: i32, flags: usize) -> isize {
    sys2(nr::TIMERFD_CREATE, clockid as usize, flags)
}

/// `timerfd_settime(fd, flags, new_value, old_value)`: arm, re-arm or
/// disarm; the old setting lands in `old_value` when it is non-null.
pub fn timerfd_settime(
    fd: i32,
    flags: i32,
    new_value: &Itimerspec,
    old_value: *mut Itimerspec,
) -> isize {
    sys4(
        nr::TIMERFD_SETTIME,
        fd as usize,
        flags as usize,
        new_value as *const Itimerspec as usize,
        old_value as usize,
    )
}

/// `timerfd_gettime(fd, curr_value)`: what is left, and the interval.
pub fn timerfd_gettime(fd: i32, curr_value: &mut Itimerspec) -> isize {
    sys2(
        nr::TIMERFD_GETTIME,
        fd as usize,
        curr_value as *mut Itimerspec as usize,
    )
}

// --- POSIX interval timers (timer_create(2)) --------------------------------

/// `SIGRTMIN`: the first real-time signal, and the number musl's
/// `SIGEV_THREAD` helper waits on. This kernel queues and masks it; a
/// disposition for it is still refused.
pub const SIGRTMIN: i32 = 32;

/// The `sigev_notify` values `timer_create` accepts. `SIGEV_THREAD` (2) is a
/// libc invention - it never reaches the kernel, which sees the
/// `SIGEV_THREAD_ID` timer the libc built underneath it.
pub const SIGEV_NONE: i32 = 0;
pub const SIGEV_SIGNAL: i32 = 1;
pub const SIGEV_THREAD_ID: i32 = 4;

/// `TIMER_ABSTIME`: `timer_settime`'s absolute-deadline flag.
pub const TIMER_ABSTIME: usize = 1;

/// `struct sigevent` as musl lays it out: the `union sigval` first, then the
/// signal number and the notification kind, then the union whose
/// `sigev_notify_thread_id` sits at offset 16 - the shape the tid arrives in.
#[repr(C)]
pub struct SigEvent {
    pub value: u64,
    pub signo: i32,
    pub notify: i32,
    pub tid: i32,
    pub _pad: i32,
}

impl SigEvent {
    pub fn new() -> Self {
        SigEvent {
            value: 0,
            signo: 0,
            notify: SIGEV_NONE,
            tid: 0,
            _pad: 0,
        }
    }
}

/// `timer_create(clockid, sigevent, timer_t *)`: make a POSIX interval timer.
/// The id comes back as the `int` the kernel hands out.
pub fn timer_create(clockid: usize, event: *const SigEvent, id: *mut i32) -> isize {
    sys3(nr::TIMER_CREATE, clockid, event as usize, id as usize)
}

/// `timer_settime(timerid, flags, new_value, old_value)`.
pub fn timer_settime(
    id: i32,
    flags: usize,
    new_value: &Itimerspec,
    old_value: *mut Itimerspec,
) -> isize {
    sys4(
        nr::TIMER_SETTIME,
        id as usize,
        flags,
        new_value as *const Itimerspec as usize,
        old_value as usize,
    )
}

/// `timer_gettime(timerid, curr_value)`.
pub fn timer_gettime(id: i32, curr_value: &mut Itimerspec) -> isize {
    sys2(
        nr::TIMER_GETTIME,
        id as usize,
        curr_value as *mut Itimerspec as usize,
    )
}

/// `timer_getoverrun(timerid)`.
pub fn timer_getoverrun(id: i32) -> isize {
    sys1(nr::TIMER_GETOVERRUN, id as usize)
}

/// `timer_delete(timerid)`.
pub fn timer_delete(id: i32) -> isize {
    sys1(nr::TIMER_DELETE, id as usize)
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)`: take a signal out of the
/// pending set without running anything - the shape a libc's `sigwaitinfo`
/// and the helper thread behind `SIGEV_THREAD` both use.
pub fn rt_sigtimedwait(set: *const u64, info: *mut SigInfo, timeout: *const TimeSpec) -> isize {
    sys4(
        nr::RT_SIGTIMEDWAIT,
        set as usize,
        info as usize,
        timeout as usize,
        8,
    )
}

// --- inotify(7) -------------------------------------------------------------

/// `inotify_init1`'s flags: the `O_` pair under the header's other names.
pub const IN_CLOEXEC: usize = 0o200_0000;
pub const IN_NONBLOCK: usize = 0x800;

/// The event bits, as `linux/inotify.h` numbers them - the test asserts
/// against the kernel's own constants through these literals.
pub const IN_ACCESS: u32 = 0x0000_0001;
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const IN_OPEN: u32 = 0x0000_0020;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;

/// One decoded `struct inotify_event`: the header fields and the name as
/// a fixed array with its length (NUL-stripped). `from_bytes` walks one
/// record off a read buffer.
pub struct InotifyRecord {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub name: [u8; 256],
    pub name_len: usize,
}

impl InotifyRecord {
    /// The name as bytes.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Parse the first record of `buf`, returning it and the record's
    /// total length. A buffer that holds less than one whole record, or
    /// whose length field runs past the data, answers `None`.
    pub fn from_bytes(buf: &[u8]) -> Option<(InotifyRecord, usize)> {
        if buf.len() < 16 {
            return None;
        }
        let wd = i32::from_ne_bytes(buf[..4].try_into().unwrap());
        let mask = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
        let cookie = u32::from_ne_bytes(buf[8..12].try_into().unwrap());
        let len = u32::from_ne_bytes(buf[12..16].try_into().unwrap()) as usize;
        let total = 16 + len;
        if buf.len() < total || len > 256 {
            return None;
        }
        let mut name = [0u8; 256];
        let mut name_len = 0;
        for &b in &buf[16..total] {
            if b == 0 {
                break;
            }
            name[name_len] = b;
            name_len += 1;
        }
        Some((
            InotifyRecord {
                wd,
                mask,
                cookie,
                name,
                name_len,
            },
            total,
        ))
    }
}

/// `inotify_init1(flags)`: the event queue.
pub fn inotify_init1(flags: usize) -> isize {
    sys1(nr::INOTIFY_INIT1, flags)
}

/// `inotify_add_watch(fd, path, mask)`: watch what the path names.
pub fn inotify_add_watch(fd: i32, path: &[u8], mask: u32) -> isize {
    sys3(
        nr::INOTIFY_ADD_WATCH,
        fd as usize,
        path.as_ptr() as usize,
        mask as usize,
    )
}

/// `inotify_rm_watch(fd, wd)`: stop watching.
pub fn inotify_rm_watch(fd: i32, wd: i32) -> isize {
    sys2(nr::INOTIFY_RM_WATCH, fd as usize, wd as usize)
}

// --- the file calls of the completion set ----------------------------------

/// The `FALLOC_FL_*` mode bits, in the numbering `linux/falloc.h` spells.
pub const FALLOC_FL_KEEP_SIZE: usize = 0x01;
pub const FALLOC_FL_PUNCH_HOLE: usize = 0x02;
pub const FALLOC_FL_COLLAPSE_RANGE: usize = 0x08;
pub const FALLOC_FL_ZERO_RANGE: usize = 0x10;
pub const FALLOC_FL_INSERT_RANGE: usize = 0x20;
pub const FALLOC_FL_UNSHARE_RANGE: usize = 0x40;

/// `fallocate(fd, mode, offset, len)`: lay out a byte range's storage.
pub fn fallocate(fd: i32, mode: usize, offset: usize, len: usize) -> isize {
    sys4(nr::FALLOCATE, fd as usize, mode, offset, len)
}

// --- the posix family: identity, scheduling, memory, machine ---------------

/// `setreuid(ruid, euid)`: -1 names the id that stays.
pub fn setreuid(ruid: isize, euid: isize) -> isize {
    sys2(nr::SETREUID, ruid as usize, euid as usize)
}

/// `setresuid(ruid, euid, suid)`: each -1 names the id that stays.
pub fn setresuid(ruid: isize, euid: isize, suid: isize) -> isize {
    sys3(nr::SETRESUID, ruid as usize, euid as usize, suid as usize)
}

/// `setresgid(rgid, egid, sgid)`: each -1 names the id that stays.
pub fn setresgid(rgid: isize, egid: isize, sgid: isize) -> isize {
    sys3(nr::SETRESGID, rgid as usize, egid as usize, sgid as usize)
}

/// `setuid(uid)`: one id, root's whole identity change.
pub fn setuid(uid: isize) -> isize {
    sys1(nr::SETUID, uid as usize)
}

/// `getresuid(ruid, euid, suid)`: three words out.
pub fn getresuid(out: &mut [u64; 3]) -> isize {
    sys3(
        nr::GETRESUID,
        &raw mut out[0] as usize,
        &raw mut out[1] as usize,
        &raw mut out[2] as usize,
    )
}

/// `getresgid(rgid, egid, sgid)`: three words out.
pub fn getresgid(out: &mut [u64; 3]) -> isize {
    sys3(
        nr::GETRESGID,
        &raw mut out[0] as usize,
        &raw mut out[1] as usize,
        &raw mut out[2] as usize,
    )
}

/// `struct __user_cap_header_struct` of `capget(2)`/`capset(2)`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CapHeader {
    pub version: u32,
    pub pid: i32,
}

/// `capget(header, data)`: the capability state, into up to 24 bytes of
/// `__user_cap_data_struct` records.
pub fn capget(header: &mut CapHeader, data: *mut u8) -> isize {
    sys2(nr::CAPGET, header as *mut CapHeader as usize, data as usize)
}

/// `capset(header, data)`: store the capability state, from up to 24 bytes of
/// `__user_cap_data_struct` records.
pub fn capset(header: &mut CapHeader, data: *mut u8) -> isize {
    sys2(nr::CAPSET, header as *mut CapHeader as usize, data as usize)
}

/// `getpriority(which, who)`: the raw syscall's 40..1 encoding, where the
/// glibc wrapper's nice value is 20 minus the answer.
pub fn getpriority(which: usize, who: usize) -> isize {
    sys2(nr::GETPRIORITY, which, who)
}

/// `setpriority(which, who, prio)`.
pub fn setpriority(which: usize, who: usize, prio: isize) -> isize {
    sys3(nr::SETPRIORITY, which, who, prio as usize)
}

/// `sched_setattr(attr, flags)` on the calling thread (`pid` 0). `attr` is a
/// 48-byte `struct sched_attr`: `size` at offset 0, `sched_policy` at 4,
/// `sched_nice` at 16.
pub fn sched_setattr(attr: *const u8, flags: usize) -> isize {
    sys3(nr::SCHED_SETATTR, 0, attr as usize, flags)
}

/// `sched_getattr(attr, size, flags)` on the calling thread (`pid` 0). The
/// kernel writes its answer into `attr` up to the smaller of `size` and the
/// 48-byte version it knows.
pub fn sched_getattr(attr: *mut u8, size: usize, flags: usize) -> isize {
    sys4(nr::SCHED_GETATTR, 0, attr as usize, size, flags)
}

/// `sched_getscheduler(pid)`.
pub fn sched_getscheduler(pid: isize) -> isize {
    sys1(nr::SCHED_GETSCHEDULER, pid as usize)
}

/// `sched_getparam(pid, param)`: one `int` priority out.
pub fn sched_getparam(pid: isize, param: *mut i32) -> isize {
    sys2(nr::SCHED_GETPARAM, pid as usize, param as usize)
}

/// `sched_setparam(pid, param)`: `param` is a one-`int` `struct sched_param`
/// carrying `sched_priority`.
pub fn sched_setparam(pid: isize, param: *const i32) -> isize {
    sys2(nr::SCHED_SETPARAM, pid as usize, param as usize)
}

/// `sched_setscheduler(pid, policy, param)`: `param` is the same one-`int`
/// `struct sched_param`.
pub fn sched_setscheduler(pid: isize, policy: usize, param: *const i32) -> isize {
    sys3(nr::SCHED_SETSCHEDULER, pid as usize, policy, param as usize)
}

/// `sched_get_priority_max(policy)` / `sched_get_priority_min(policy)`.
pub fn sched_get_priority_max(policy: usize) -> isize {
    sys1(nr::SCHED_GET_PRIORITY_MAX, policy)
}

pub fn sched_get_priority_min(policy: usize) -> isize {
    sys1(nr::SCHED_GET_PRIORITY_MIN, policy)
}

/// `getcpu(cpu, node)`: two `u32`s out, either pointer null-able.
pub fn getcpu(cpu: *mut u32, node: *mut u32) -> isize {
    sys3(nr::GETCPU, cpu as usize, node as usize, 0)
}

/// `msync(addr, len, flags)`.
pub fn msync(addr: usize, len: usize, flags: usize) -> isize {
    sys3(nr::MSYNC, addr, len, flags)
}

/// `mincore(addr, len, vec)`: one residency byte per page out.
pub fn mincore(addr: usize, len: usize, vec: *mut u8) -> isize {
    sys3(nr::MINCORE, addr, len, vec as usize)
}

/// `mlock(addr, len)` / `munlock(addr, len)`.
pub fn mlock(addr: usize, len: usize) -> isize {
    sys2(nr::MLOCK, addr, len)
}

pub fn munlock(addr: usize, len: usize) -> isize {
    sys2(nr::MUNLOCK, addr, len)
}

/// `mlockall(flags)` / `munlockall()`.
pub fn mlockall(flags: usize) -> isize {
    sys1(nr::MLOCKALL, flags)
}

pub fn munlockall() -> isize {
    sys0(nr::MUNLOCKALL)
}

/// `personality(persona)`: 0xffffffff queries.
pub fn personality(persona: usize) -> isize {
    sys1(nr::PERSONALITY, persona)
}

/// `vhangup()`.
pub fn vhangup() -> isize {
    sys0(nr::VHANGUP)
}

/// `seccomp(op, flags, args)`.
pub fn seccomp(op: usize, flags: usize, args: usize) -> isize {
    sys3(nr::SECCOMP, op, flags, args)
}

/// `unshare(flags)`.
pub fn unshare(flags: usize) -> isize {
    sys1(nr::UNSHARE, flags)
}

/// `setns(fd, nstype)`.
pub fn setns(fd: i32, nstype: usize) -> isize {
    sys2(nr::SETNS, fd as usize, nstype)
}

/// `sethostname(name, len)`: name the caller's UTS namespace.
///
/// The name is the `len` bytes at `name`, not a C string - which is what Linux
/// copies, and what makes a name with a NUL inside it a name rather than a
/// shorter name.
pub fn sethostname(name: &[u8]) -> isize {
    sys2(nr::SETHOSTNAME, name.as_ptr() as usize, name.len())
}

/// `setdomainname(name, len)`: name the domain of the caller's UTS namespace.
pub fn setdomainname(name: &[u8]) -> isize {
    sys2(nr::SETDOMAINNAME, name.as_ptr() as usize, name.len())
}

/// `tgkill(tgid, tid, sig)`.
pub fn tgkill(tgid: isize, tid: isize, sig: usize) -> isize {
    sys3(nr::TGKILL, tgid as usize, tid as usize, sig)
}

/// `rt_sigpending(set, sigsetsize)`: the pending mask out.
pub fn rt_sigpending(set: *mut u64, sigsetsize: usize) -> isize {
    sys2(nr::RT_SIGPENDING, set as usize, sigsetsize)
}

/// `rt_sigreturn()`: the frame `rt_sigaction`'s handler left on the stack,
/// restored. A test's `SA_RESTORER` trampoline is a function that only calls
/// this; it never returns.
pub fn rt_sigreturn() -> isize {
    sys0(nr::RT_SIGRETURN)
}

/// `rt_sigqueueinfo(tgid, sig, info)`: `sigqueue`'s syscall spelling. The
/// `si_code` and `si_value` the caller wrote in `info` reach the target's
/// handler.
pub fn rt_sigqueueinfo(tgid: isize, sig: i32, info: &SigInfo) -> isize {
    sys3(
        nr::RT_SIGQUEUEINFO,
        tgid as usize,
        sig as usize,
        info as *const SigInfo as usize,
    )
}

/// `copy_file_range(in, off_in, out, off_out, len, flags)`: the offsets
/// are in-out `loff_t *`s, or null for the descriptors' own cursors.
pub fn copy_file_range(
    in_fd: i32,
    off_in: *mut i64,
    out_fd: i32,
    off_out: *mut i64,
    len: usize,
    flags: usize,
) -> isize {
    sys6(
        nr::COPY_FILE_RANGE,
        in_fd as usize,
        off_in as usize,
        out_fd as usize,
        off_out as usize,
        len,
        flags,
    )
}

/// The `RWF_*` advice bits of `preadv2`/`pwritev2`.
pub const RWF_HIPRI: usize = 0x1;
pub const RWF_DSYNC: usize = 0x2;
pub const RWF_SYNC: usize = 0x4;
pub const RWF_NOWAIT: usize = 0x8;
pub const RWF_APPEND: usize = 0x10;
pub const RWF_NOAPPEND: usize = 0x20;
pub const RWF_ATOMIC: usize = 0x40;
pub const RWF_DONTCACHE: usize = 0x80;

/// `preadv2(fd, iov, iovcnt, offset, flags)`: `offset` of -1 is the
/// cursor-carrying spelling.
pub fn preadv2(fd: i32, iov: &[Iovec], offset: i64, flags: usize) -> isize {
    sys5(
        nr::PREADV2,
        fd as usize,
        iov.as_ptr() as usize,
        iov.len(),
        offset as usize,
        flags,
    )
}

/// `pwritev2(fd, iov, iovcnt, offset, flags)`.
pub fn pwritev2(fd: i32, iov: &[Iovec], offset: i64, flags: usize) -> isize {
    sys5(
        nr::PWRITEV2,
        fd as usize,
        iov.as_ptr() as usize,
        iov.len(),
        offset as usize,
        flags,
    )
}

/// `syslog(type, buf, len)` - `klogctl(3)`'s door: 2 reads (destructively),
/// 3 reads the tail, 4 reads and clears, 5 clears, 9 and 10 are the sizes.
pub fn syslog(kind: i32, buf: *mut u8, len: usize) -> isize {
    sys3(nr::SYSLOG, kind as usize, buf as usize, len)
}

/// Write the cwd path into `buf`, NUL-terminated; the return is the buffer.
pub fn getcwd(buf: &mut [u8]) -> isize {
    sys2(nr::GETCWD, buf.as_mut_ptr() as usize, buf.len())
}

/// Move to the directory `path` names; relative paths start at the cwd.
pub fn chdir(path: &[u8]) -> isize {
    sys1(nr::CHDIR, path.as_ptr() as usize)
}

/// Make `path` the calling process's root - what absolute paths begin at and
/// what `..` at the top clamps to. It does not move the machine's root, and it
/// does not by itself move the cwd; the caller `chdir("/")` after it.
pub fn chroot(path: &[u8]) -> isize {
    sys1(nr::CHROOT, path.as_ptr() as usize)
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
    sys3(
        nr::GETDENTS64,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
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

/// `tcgetattr(fd)`: the terminal's settings, or `None` when the descriptor is
/// not a terminal (or the kernel says otherwise).
pub fn tcgetattr(fd: i32) -> Option<Termios> {
    let mut tio = Termios {
        iflag: 0,
        oflag: 0,
        cflag: 0,
        lflag: 0,
        line: 0,
        cc: [0; 32],
        ispeed: 0,
        ospeed: 0,
    };
    (ioctl(fd, TCGETS, &raw mut tio as usize) == 0).then_some(tio)
}

/// `tcsetattr(fd, TCSANOW, tio)`: `TCSETS` is that trio, and the kernel this
/// suite runs on has no output queue to drain, so `when` is not worth a word.
pub fn tcsetattr(fd: i32, tio: &Termios) -> isize {
    ioctl(fd, TCSETS, &raw const *tio as usize)
}

/// `setitimer(which, new, old)`: arm an interval timer, and take back what was
/// there. `new` of `None` is the man page's "disarm", spelled as a zero struct -
/// which is what `old` must still be written, so the zero lives in a binding.
pub fn setitimer(which: usize, new: Option<&Itimerval>, old: &mut Itimerval) -> isize {
    let zero = Itimerval {
        it_interval: TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let new_ptr = match new {
        Some(t) => t as *const Itimerval as usize,
        None => &zero as *const Itimerval as usize,
    };
    sys3(
        nr::SETITIMER,
        which,
        new_ptr,
        old as *mut Itimerval as usize,
    )
}

/// Milliseconds on the monotonic clock, which is the only clock a test of a
/// wait can measure against: the wall clock can be set from under it.
pub fn monotonic_ms() -> u64 {
    let mut ts = TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if clock_gettime(CLOCK_MONOTONIC, &mut ts) != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

/// Sleep, in milliseconds - the delay a test needs to have bytes land *after*
/// the read they are answering has started.
pub fn sleep_ms(ms: u64) {
    let req = TimeSpec {
        tv_sec: (ms / 1000) as i64,
        tv_nsec: ((ms % 1000) as i64) * 1_000_000,
    };
    let mut rem = TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let _ = nanosleep(&req, &mut rem);
}

/// `/dev/pts/<n>` spelled into `buf` from the number `TIOCGPTN` answered, NUL
/// terminated, returned as the slice to open. The caller keeps the storage
/// because this suite's programs are `no_std` and have no heap of their own.
///
/// One spelling, because minting a pair and opening its slave is not one test's
/// business: it is how any program here gets a terminal it can drive from both
/// ends.
pub fn pts_path<'a>(n: usize, buf: &'a mut [u8]) -> &'a [u8] {
    const PREFIX: &[u8] = b"/dev/pts/";
    let at = PREFIX.len();
    buf[..at].copy_from_slice(PREFIX);
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut rest = n;
    loop {
        digits[len] = b'0' + (rest % 10) as u8;
        rest /= 10;
        len += 1;
        if rest == 0 {
            break;
        }
    }
    for (i, &d) in digits[..len].iter().rev().enumerate() {
        buf[at + i] = d;
    }
    buf[at + len] = 0;
    &buf[..at + len + 1]
}

/// `mount(source, target, fstype, flags, NULL)`: hang a filesystem, or -
/// with `MS_BIND` - a second name on a tree already mounted. The three path
/// arguments must be NUL-terminated by the caller, as every other path here.
pub fn mount(source: &[u8], target: &[u8], fstype: &[u8], flags: usize) -> isize {
    sys5(
        nr::MOUNT,
        source.as_ptr() as usize,
        target.as_ptr() as usize,
        fstype.as_ptr() as usize,
        flags,
        0,
    )
}

/// `umount2(target, flags)`.
pub fn umount2(target: &[u8], flags: usize) -> isize {
    sys2(nr::UMOUNT2, target.as_ptr() as usize, flags)
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

/// `setpgid(pid, pgid)`, with man(2)'s zeros: `pid` zero is the caller and
/// `pgid` zero is the group led by it, so `setpgid(0, 0)` is what a shell says
/// in a freshly forked child before it `exec`s - and what gives that job a
/// process group of its own, so a signal aimed at the group reaches the job
/// and not the shell that started it.
pub fn setpgid(pid: i32, pgid: i32) -> isize {
    sys2(nr::SETPGID, pid as usize, pgid as usize)
}

/// The group this process is in (`getpgrp`), and its ask-about-another form
/// (`getpgid`). A pid of zero means the caller in the second, which makes the
/// two the same question.
pub fn getpgrp() -> isize {
    sys0(nr::GETPGRP)
}

pub fn getpgid(pid: i32) -> isize {
    sys1(nr::GETPGID, pid as usize)
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

/// `madvise(addr, len, advice)` - recognised advice answered 0, an advice
/// outside the vocabulary answered EINVAL.
pub fn madvise(addr: usize, len: usize, advice: usize) -> isize {
    sys3(nr::MADVISE, addr, len, advice)
}

/// `futex(uaddr, op, val)`: the WAIT/WAKE pair musl's locks lean on.
///
/// All six of Linux's arguments are passed even though this spelling names
/// three of them. `futex(2)` reads the timeout in the fourth and `val3` in the
/// fifth, and the registers behind them are *not* cleared by the syscall
/// instruction: leaving them alone hands the kernel whatever the last call
/// happened to leave in `r10` and `r8`, which it then reads as a `timespec *`
/// and refuses with `EFAULT`. A real `libc` passes all six; so does this.
pub fn futex(uaddr: *mut i32, op: usize, val: i32) -> isize {
    sys6(nr::FUTEX, uaddr as usize, op, val as usize, 0, 0, 0)
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)`: every argument the man page
/// spells, for the operations - the bitset forms above all - whose meaning
/// lives in the ones [`futex`] leaves at zero.
pub fn futex6(
    uaddr: *mut i32,
    op: usize,
    val: i32,
    timeout: usize,
    uaddr2: usize,
    val3: usize,
) -> isize {
    sys6(
        nr::FUTEX,
        uaddr as usize,
        op,
        val as usize,
        timeout,
        uaddr2,
        val3,
    )
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
const SYS_RECVMSG: u64 = 47;
const SYS_SENDMSG: u64 = 46;
const SYS_SOCKETPAIR: u64 = 53;
const SYS_SETSOCKOPT: u64 = 54;
const SYS_GETSOCKOPT: u64 = 55;

/// `struct iovec`: one segment of a scatter-gather buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoVec {
    pub base: usize,
    pub len: usize,
}

/// `struct msghdr` of the x86_64 ABI: 56 bytes, with the padding word
/// after `msg_namelen` the ABI carries.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MsgHdr {
    pub msg_name: usize,
    pub msg_namelen: u32,
    pub _pad: u32,
    pub msg_iov: usize,
    pub msg_iovlen: usize,
    pub msg_control: usize,
    pub msg_controllen: usize,
    pub msg_flags: usize,
}

/// `recvmsg(fd, msg, flags)`.
pub fn recvmsg(fd: i32, msg: &mut MsgHdr, flags: usize) -> isize {
    raw_sys(SYS_RECVMSG, fd as usize, msg as *mut MsgHdr as usize, flags)
}

/// `setsockopt(fd, level, optname, &val, 4)` for the int-valued options.
pub fn setsockopt_int(fd: i32, level: usize, opt: usize, val: i32) -> isize {
    raw_sys5(
        SYS_SETSOCKOPT,
        fd as usize,
        level,
        opt,
        &val as *const i32 as usize,
        4,
    )
}

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
pub fn recvfrom(fd: i32, buf: &mut [u8], flags: usize, addr: Option<&mut SockAddrIn>) -> isize {
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

// --- AF_UNIX SCM_RIGHTS / SCM_CREDENTIALS test surface ---

/// `struct cmsghdr`, x86_64: a `size_t` length then two `int`s, with the data
/// starting at a word boundary (offset 16).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Cmsghdr {
    pub cmsg_len: usize,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// `CMSG_ALIGN`: round a control-buffer byte count up to the ABI's word size.
pub const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}
/// `CMSG_LEN(len)`: the value to store in `cmsg_len`.
pub const fn cmsg_len(len: usize) -> usize {
    16 + len
}
/// `CMSG_SPACE(len)`: the room a message with `len` data bytes occupies.
pub const fn cmsg_space(len: usize) -> usize {
    cmsg_align(16 + len)
}

/// `SOL_SOCKET` control-message types and the `SO_PASSCRED`/`SO_PEERCRED`
/// option numbers, and the two `recvmsg` flags the SCM path reads.
pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;
pub const SO_PASSCRED: usize = 16;
pub const SO_PEERCRED: usize = 17;
pub const MSG_CMSG_CLOEXEC: usize = 0x10000;
pub const MSG_CTRUNC: usize = 0x8;
/// `MSG_DONTWAIT` as the Linux uapi spells it - `0x40`. (`0x4000` is
/// `MSG_CMSG_CLOEXEC` on this arch; the values are easy to swap by eye.)
pub const MSG_DONTWAIT: usize = 0x40;
pub const MSG_PEEK: usize = 0x2;

/// The control message that carries one descriptor over a unix socket: a
/// `struct cmsghdr` (`cmsg_len`, `SOL_SOCKET`, `SCM_RIGHTS`) followed by the
/// `int` fd in its data, laid out by hand because there is no libc here to
/// build it.
///
/// `t-memfd-shm` and `t-prime` both hand a handle to a forked child this way,
/// and both used to spell the same 32-byte slot out themselves.
#[repr(C, align(8))]
pub struct ScmRights {
    w: [usize; 4],
}

impl ScmRights {
    pub const fn new() -> Self {
        ScmRights { w: [0; 4] }
    }

    /// The slot's address, for a `MsgHdr`'s `msg_control`.
    pub fn control_ptr(&mut self) -> usize {
        self.w.as_mut_ptr() as usize
    }

    /// Write `fd` into the slot and answer the `msg_controllen` to pass with
    /// it - `CMSG_SPACE(4)`, one header plus one `int`.
    ///
    /// # Safety
    /// The slot must stay put and alive until the `sendmsg`/`recvmsg` that is
    /// handed [`ScmRights::control_ptr`] has returned; the kernel reads or
    /// writes it for the duration of that call.
    pub unsafe fn one_fd(&mut self, fd: i32) -> usize {
        unsafe {
            let b = self.w.as_mut_ptr() as *mut u8;
            core::ptr::write_bytes(b, 0, 32);
            (b as *mut usize).write_unaligned(cmsg_len(4));
            (b.add(8) as *mut i32).write_unaligned(SOL_SOCKET as i32);
            (b.add(12) as *mut i32).write_unaligned(SCM_RIGHTS);
            (b.add(16) as *mut i32).write_unaligned(fd);
            cmsg_space(4)
        }
    }

    /// The descriptor a `recvmsg` wrote back into the slot.
    pub fn recv_fd(&self) -> i32 {
        unsafe { *((self.w.as_ptr() as *const u8).add(16) as *const i32) }
    }
}

impl Default for ScmRights {
    fn default() -> Self {
        Self::new()
    }
}

/// `sendmsg(fd, msg, flags)`.
pub fn sendmsg(fd: i32, msg: &mut MsgHdr, flags: usize) -> isize {
    sys3(SYS_SENDMSG, fd as usize, msg as *mut MsgHdr as usize, flags)
}

/// `socketpair(domain, type, flags, sv)` — two connected unix ends.
pub fn socketpair(domain: usize, typ: usize, flags: usize, sv: &mut [i32; 2]) -> isize {
    sys4(SYS_SOCKETPAIR, domain, typ, flags, sv.as_mut_ptr() as usize)
}

/// `getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &ucred, &mut 12)` — the peer's
/// pid, uid and gid, read into three words.
pub fn getsockopt_cred(fd: i32, out: &mut [u32; 3]) -> isize {
    let mut len: usize = 12;
    sys5(
        SYS_GETSOCKOPT,
        fd as usize,
        SOL_SOCKET,
        SO_PEERCRED,
        out as *mut [u32; 3] as usize,
        &mut len as *mut usize as usize,
    )
}

/// `getsockopt(fd, level, opt, &mut i32)` — a 4-byte int option read back.
pub fn getsockopt_int(fd: i32, level: usize, opt: usize, val: &mut i32) -> isize {
    let mut len: usize = 4;
    sys5(
        SYS_GETSOCKOPT,
        fd as usize,
        level,
        opt,
        val as *mut i32 as usize,
        &mut len as *mut usize as usize,
    )
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
pub fn select(nfds: usize, readfds: Option<&mut [u8]>, timeout: Option<&TimeVal>) -> isize {
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
    sys2(
        SYS_SYMLINK,
        target.as_ptr() as usize,
        linkpath.as_ptr() as usize,
    )
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

/// `struct sockaddr_un` as the kernel's parser reads it: family at zero,
/// the path from two. `bytes` returns exactly the `addrlen` a call should
/// carry: the family, the path and one NUL.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

pub const AF_UNIX: usize = 1;

impl SockAddrUn {
    /// The address for one path: NUL-terminated, minimal `addrlen`. A path
    /// over 107 characters cannot fit the field, and the builder refuses
    /// (0 length answers "no address" at the wrappers).
    pub const fn new(path: &[u8]) -> (SockAddrUn, usize) {
        let mut a = SockAddrUn {
            sun_family: 1,
            sun_path: [0; 108],
        };
        if path.len() > 107 {
            return (a, 0);
        }
        let mut i = 0;
        while i < path.len() {
            a.sun_path[i] = path[i];
            i += 1;
        }
        a.sun_path[path.len()] = 0;
        (a, 3 + path.len())
    }

    /// A raw, hand-spelled address: family bytes and a body that may be
    /// NUL at the front (the abstract namespace, which this kernel
    /// refuses) or lack a terminator - shapes only the parser's own
    /// verdict answers.
    pub const fn raw(family: u16, body: &[u8]) -> (SockAddrUn, usize) {
        let mut a = SockAddrUn {
            sun_family: family,
            sun_path: [0; 108],
        };
        let mut i = 0;
        while i < body.len() && i < 108 {
            a.sun_path[i] = body[i];
            i += 1;
        }
        (a, 2 + i)
    }
}

/// `bind` with a `sockaddr_un` of the given length.
pub fn bind_un(fd: i32, addr: &SockAddrUn, len: usize) -> isize {
    sys3(
        nr::BIND,
        fd as usize,
        addr as *const SockAddrUn as usize,
        len,
    )
}

/// `connect` with a `sockaddr_un`.
pub fn connect_un(fd: i32, addr: &SockAddrUn, len: usize) -> isize {
    sys3(
        nr::CONNECT,
        fd as usize,
        addr as *const SockAddrUn as usize,
        len,
    )
}

/// `accept(fd, addr, addrlen)` - either output may be absent (null), which
/// is how the "I do not care who it is" form is spelled.
pub fn accept4_un(fd: i32, addr: *mut SockAddrUn, len: *mut u32, flags: usize) -> isize {
    sys4(nr::ACCEPT4, fd as usize, addr as usize, len as usize, flags)
}

// ---------------------------------------------------------------------------
// The completion set and the calls those tests brought with them
// ---------------------------------------------------------------------------

/// The `struct open_how` of `openat2(2)`: the flags, the create mode and the
/// path-resolution the caller wants, each a `__u64`.
#[repr(C)]
pub struct OpenHow {
    pub flags: u64,
    pub mode: u64,
    pub resolve: u64,
}

impl OpenHow {
    pub const fn new(flags: usize, mode: usize, resolve: usize) -> Self {
        OpenHow {
            flags: flags as u64,
            mode: mode as u64,
            resolve: resolve as u64,
        }
    }
}

/// `RESOLVE_*`: the path-resolution bits of `openat2(2)`.
pub const RESOLVE_NO_XDEV: usize = 0x01;
pub const RESOLVE_NO_MAGICLINKS: usize = 0x02;
pub const RESOLVE_NO_SYMLINKS: usize = 0x04;
pub const RESOLVE_BENEATH: usize = 0x08;
pub const RESOLVE_IN_ROOT: usize = 0x10;
pub const RESOLVE_CACHED: usize = 0x20;

/// `openat2(dirfd, path, how, size)` - `size` is `sizeof(struct open_how)`.
pub fn openat2(dirfd: i32, path: &[u8], how: &OpenHow, size: usize) -> isize {
    sys4(
        nr::OPENAT2,
        dirfd as usize,
        path.as_ptr() as usize,
        how as *const OpenHow as usize,
        size,
    )
}

/// The 120-byte Linux x86_64 `struct statfs`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Statfs {
    pub f_type: i64,
    pub f_bsize: i64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: [i32; 2],
    pub f_namelen: i64,
    pub f_frsize: i64,
    pub f_flags: i64,
    pub f_spare: [i64; 4],
}

impl Statfs {
    pub const fn new() -> Self {
        Statfs {
            f_type: 0,
            f_bsize: 0,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            f_files: 0,
            f_ffree: 0,
            f_fsid: [0; 2],
            f_namelen: 0,
            f_frsize: 0,
            f_flags: 0,
            f_spare: [0; 4],
        }
    }
}

impl Default for Statfs {
    fn default() -> Self {
        Self::new()
    }
}

/// `statfs(path, buf)`.
pub fn statfs(path: &[u8], buf: &mut Statfs) -> isize {
    sys2(
        nr::STATFS,
        path.as_ptr() as usize,
        buf as *mut Statfs as usize,
    )
}

/// `fstatfs(fd, buf)`.
pub fn fstatfs(fd: i32, buf: &mut Statfs) -> isize {
    sys2(nr::FSTATFS, fd as usize, buf as *mut Statfs as usize)
}

/// `fdatasync(fd)` - the flush the data alone needs.
pub fn fdatasync(fd: i32) -> isize {
    sys1(nr::FDATASYNC, fd as usize)
}

/// `syncfs(fd)` - flush the filesystem one descriptor belongs to.
pub fn syncfs(fd: i32) -> isize {
    sys1(nr::SYNCFS, fd as usize)
}

/// The `SPLICE_F_*` flag bits.
pub const SPLICE_F_MOVE: usize = 0x01;
pub const SPLICE_F_NONBLOCK: usize = 0x02;
pub const SPLICE_F_MORE: usize = 0x04;
pub const SPLICE_F_GIFT: usize = 0x08;

/// `splice(fd_in, off_in, fd_out, off_out, len, flags)`: the offsets are
/// `loff_t *`s, passed raw here so a caller can spell "null" as 0 - which the
/// pipe end requires and the file end reads as "the descriptor's own cursor".
pub fn splice(
    fd_in: i32,
    off_in: usize,
    fd_out: i32,
    off_out: usize,
    len: usize,
    flags: usize,
) -> isize {
    sys6(
        nr::SPLICE,
        fd_in as usize,
        off_in,
        fd_out as usize,
        off_out,
        len,
        flags,
    )
}

/// `EFD_*`: the create flags `eventfd(2)` takes.
pub const EFD_SEMAPHORE: usize = 0x1;
pub const EFD_NONBLOCK: usize = 0x800;
pub const EFD_CLOEXEC: usize = 0o200_0000;

/// `eventfd(initval, flags)` - a counter a process can wait on.
pub fn eventfd(initval: u32, flags: usize) -> isize {
    sys2(nr::EVENTFD, initval as usize, flags)
}

/// `SFD_*`: the flags `signalfd4` takes.
pub const SFD_NONBLOCK: usize = 0x800;
pub const SFD_CLOEXEC: usize = 0o200_0000;

/// `signalfd(fd, mask, sizemask)` - the original spelling, with no flags.
pub fn signalfd(fd: i32, mask: &[u8; 8], sizemask: usize) -> isize {
    sys3(nr::SIGNALFD, fd as usize, mask.as_ptr() as usize, sizemask)
}

/// `signalfd4(fd, mask, sizemask, flags)`.
pub fn signalfd4(fd: i32, mask: &[u8; 8], sizemask: usize, flags: usize) -> isize {
    sys4(
        nr::SIGNALFD4,
        fd as usize,
        mask.as_ptr() as usize,
        sizemask,
        flags,
    )
}

/// The 128-byte `struct signalfd_siginfo`, with the four words a reader
/// branches on: the signal, its code and the pid it came from.
#[repr(C)]
pub struct SignalFdInfo {
    pub bytes: [u8; 128],
}

impl SignalFdInfo {
    pub const fn new() -> Self {
        SignalFdInfo { bytes: [0; 128] }
    }
    pub fn signo(&self) -> i32 {
        let b = &self.bytes[0..4];
        i32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    }
    pub fn code(&self) -> i32 {
        let b = &self.bytes[8..12];
        i32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    }
    pub fn pid(&self) -> i32 {
        let b = &self.bytes[12..16];
        i32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    }
}

impl Default for SignalFdInfo {
    fn default() -> Self {
        Self::new()
    }
}

/// The `SIGCHLD`/`CLD_EXITED` pair a `signalfd` read reports.
pub const SIGCHLD: i32 = 17;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;

/// One `i32` word out of a 128-byte `siginfo_t` / `signalfd_siginfo`, at the
/// byte offset the x86_64 ABI puts it at: `si_signo` 0, `si_code` 8, `si_pid`
/// 16, `si_status` 24.
pub fn siginfo_i32(buf: &[u8; 128], at: usize) -> i32 {
    i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// `process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)`.
pub fn process_vm_readv(pid: i32, local: &[Iovec], remote: &[Iovec], flags: usize) -> isize {
    sys6(
        nr::PROCESS_VM_READV,
        pid as usize,
        local.as_ptr() as usize,
        local.len(),
        remote.as_ptr() as usize,
        remote.len(),
        flags,
    )
}

/// `process_vm_writev(...)` - the same, in the other direction.
pub fn process_vm_writev(pid: i32, local: &[Iovec], remote: &[Iovec], flags: usize) -> isize {
    sys6(
        nr::PROCESS_VM_WRITEV,
        pid as usize,
        local.as_ptr() as usize,
        local.len(),
        remote.as_ptr() as usize,
        remote.len(),
        flags,
    )
}

/// The `PR_*` options `prctl(2)` names in this kernel.
pub const PR_SET_NAME: usize = 15;
pub const PR_GET_NAME: usize = 16;
pub const PR_GET_SECCOMP: usize = 21;
pub const PR_SET_SECCOMP: usize = 22;
/// The dumpable flag and the capability options, the four `prctl` options
/// whose state lives in the process's credential record.
pub const PR_GET_DUMPABLE: usize = 3;
pub const PR_SET_DUMPABLE: usize = 4;
pub const PR_SET_KEEPCAPS: usize = 8;
pub const PR_GET_KEEPCAPS: usize = 9;
pub const PR_CAPBSET_READ: usize = 23;
pub const PR_CAPBSET_DROP: usize = 24;
/// `CAP_LAST_CAP` on x86_64: `CAP_CHECKPOINT_RESTORE` is the last one named.
pub const CAP_LAST_CAP: usize = 40;
pub const SECCOMP_MODE_STRICT: usize = 1;

/// `prctl(op, a2, a3, a4, a5)`.
pub fn prctl(op: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    sys5(nr::PRCTL, op, a2, a3, a4, a5)
}

/// `waitid`'s `idtype` and `options` words.
pub const P_ALL: usize = 0;
pub const P_PID: usize = 1;
pub const WEXITED: usize = 4;
pub const WSTOPPED: usize = 2;
pub const WCONTINUED: usize = 8;
pub const WNOWAIT: usize = 0x0100_0000;

/// `AT_SYMLINK_FOLLOW`: `linkat`'s "follow the source name" flag.
pub const AT_SYMLINK_FOLLOW: usize = 0x400;

/// `symlinkat(target, newdirfd, linkpath)`.
pub fn symlinkat(target: &[u8], newdirfd: i32, linkpath: &[u8]) -> isize {
    sys3(
        nr::SYMLINKAT,
        target.as_ptr() as usize,
        newdirfd as usize,
        linkpath.as_ptr() as usize,
    )
}

/// `readlinkat(dirfd, path, buf)`.
pub fn readlinkat(dirfd: i32, path: &[u8], buf: &mut [u8]) -> isize {
    sys4(
        nr::READLINKAT,
        dirfd as usize,
        path.as_ptr() as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

/// The `XATTR_*` flags `setxattr` takes.
pub const XATTR_CREATE: usize = 0x1;
pub const XATTR_REPLACE: usize = 0x2;

/// `setxattr(path, name, value, size, flags)`.
pub fn setxattr(path: &[u8], name: &[u8], value: &[u8], flags: usize) -> isize {
    sys5(
        nr::SETXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
        value.as_ptr() as usize,
        value.len(),
        flags,
    )
}

/// `lsetxattr(...)` - the same, naming a link rather than its target.
pub fn lsetxattr(path: &[u8], name: &[u8], value: &[u8], flags: usize) -> isize {
    sys5(
        nr::LSETXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
        value.as_ptr() as usize,
        value.len(),
        flags,
    )
}

/// `fsetxattr(fd, name, value, size, flags)`.
pub fn fsetxattr(fd: i32, name: &[u8], value: &[u8], flags: usize) -> isize {
    sys5(
        nr::FSETXATTR,
        fd as usize,
        name.as_ptr() as usize,
        value.as_ptr() as usize,
        value.len(),
        flags,
    )
}

/// `getxattr(path, name, value, size)`.
pub fn getxattr(path: &[u8], name: &[u8], value: &mut [u8]) -> isize {
    sys4(
        nr::GETXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
        value.as_mut_ptr() as usize,
        value.len(),
    )
}

/// `lgetxattr(...)`.
pub fn lgetxattr(path: &[u8], name: &[u8], value: &mut [u8]) -> isize {
    sys4(
        nr::LGETXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
        value.as_mut_ptr() as usize,
        value.len(),
    )
}

/// `fgetxattr(fd, name, value, size)`.
pub fn fgetxattr(fd: i32, name: &[u8], value: &mut [u8]) -> isize {
    sys4(
        nr::FGETXATTR,
        fd as usize,
        name.as_ptr() as usize,
        value.as_mut_ptr() as usize,
        value.len(),
    )
}

/// `listxattr(path, list, size)`.
pub fn listxattr(path: &[u8], list: &mut [u8]) -> isize {
    sys3(
        nr::LISTXATTR,
        path.as_ptr() as usize,
        list.as_mut_ptr() as usize,
        list.len(),
    )
}

/// `llistxattr(...)`.
pub fn llistxattr(path: &[u8], list: &mut [u8]) -> isize {
    sys3(
        nr::LLISTXATTR,
        path.as_ptr() as usize,
        list.as_mut_ptr() as usize,
        list.len(),
    )
}

/// `flistxattr(fd, list, size)`.
pub fn flistxattr(fd: i32, list: &mut [u8]) -> isize {
    sys3(
        nr::FLISTXATTR,
        fd as usize,
        list.as_mut_ptr() as usize,
        list.len(),
    )
}

/// `removexattr(path, name)`.
pub fn removexattr(path: &[u8], name: &[u8]) -> isize {
    sys2(
        nr::REMOVEXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
    )
}

/// `lremovexattr(path, name)`.
pub fn lremovexattr(path: &[u8], name: &[u8]) -> isize {
    sys2(
        nr::LREMOVEXATTR,
        path.as_ptr() as usize,
        name.as_ptr() as usize,
    )
}

/// `fremovexattr(fd, name)`.
pub fn fremovexattr(fd: i32, name: &[u8]) -> isize {
    sys2(nr::FREMOVEXATTR, fd as usize, name.as_ptr() as usize)
}

// The calls this kernel still answers `ENOSYS` for. They are deliberately not
// in `nr.rs`: no dispatch arm names them, and the point of the probe test is
// that the *number* is what Linux uses while the kernel has no arm for it.
pub const SYS_KEYCTL: u64 = 250;
pub const SYS_FANOTIFY_INIT: u64 = 300;
pub const SYS_FANOTIFY_MARK: u64 = 301;
pub const SYS_USERFAULTFD: u64 = 323;
pub const SYS_IO_URING_SETUP: u64 = 425;
pub const SYS_IO_URING_ENTER: u64 = 426;
pub const SYS_IO_URING_REGISTER: u64 = 427;

/// The file types `S_IFMT` can name that the syscall tests branch on beyond
/// the regular/directory pair already above.
pub const S_IFLNK: u32 = 0xA000;
pub const S_IFSOCK: u32 = 0xC000;

/// The two `open` flags the newer file tests pass: truncate on open, and
/// append-mode writes.
pub const O_TRUNC: usize = 0o1000;
pub const O_APPEND: usize = 0o2000;

/// The superblock magics `statfs` reports, exactly as the kernel's
/// `fstype_magic` spells them.
pub const RAMFS_MAGIC: u64 = 0x858458F6;
pub const TMPFS_MAGIC: u64 = 0x01021994;
pub const EXT_SUPER_MAGIC: u64 = 0x0000EF53;
pub const PROC_SUPER_MAGIC: u64 = 0x00009FA0;
pub const SYSFS_MAGIC: u64 = 0x62656572;
