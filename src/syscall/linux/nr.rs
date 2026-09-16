//! Linux x86_64 syscall numbers.
//!
//! These are the numbers musl/glibc uses when calling `syscall(N, ...)`.

pub const POLL: u64 = 7;
pub const READ: u64 = 0;
pub const WRITE: u64 = 1;
pub const OPEN: u64 = 2;
pub const CLOSE: u64 = 3;
pub const STAT: u64 = 4;
pub const FSTAT: u64 = 5;
pub const LSTAT: u64 = 6;
pub const LSEEK: u64 = 8;
pub const MMAP: u64 = 9;
pub const MPROTECT: u64 = 10;
pub const MREMAP: u64 = 25;
pub const MUNMAP: u64 = 11;
pub const BRK: u64 = 12;
pub const RT_SIGACTION: u64 = 13;
pub const RT_SIGPROCMASK: u64 = 14;
pub const RT_SIGRETURN: u64 = 15;
pub const IOCTL: u64 = 16;
pub const READV: u64 = 19;
pub const WRITEV: u64 = 20;
pub const ACCESS: u64 = 21;
pub const PIPE: u64 = 22;
pub const DUP: u64 = 32;
pub const DUP2: u64 = 33;
pub const NANOSLEEP: u64 = 35;
pub const GETPID: u64 = 39;
pub const CLONE: u64 = 56;
pub const FORK: u64 = 57;
pub const VFORK: u64 = 58;
pub const EXECVE: u64 = 59;
pub const EXIT: u64 = 60;
pub const WAIT4: u64 = 61;
pub const KILL: u64 = 62;
pub const UNAME: u64 = 63;
pub const FCNTL: u64 = 72;
pub const SELECT: u64 = 23;
pub const GETITIMER: u64 = 36;
pub const ALARM: u64 = 37;
pub const SETITIMER: u64 = 38;
pub const FLOCK: u64 = 73;
pub const TRUNCATE: u64 = 76;
pub const FTRUNCATE: u64 = 77;
pub const GETCWD: u64 = 79;
pub const CHDIR: u64 = 80;
pub const FCHDIR: u64 = 81;
/// The legacy path calls - `rename(2)`, `mkdir(2)`, `unlink(2)` - are what
/// musl/glibc call for the plain names; each is its `*at` equivalent at
/// `AT_FDCWD`, which the dispatch reshapes.
pub const RENAME: u64 = 82;
pub const MKDIR: u64 = 83;
/// `rmdir(2)` - remove an empty directory. Not `unlink` with a flag: a
/// directory that still holds something has to be refused, and Linux keeps
/// the two operations apart so that it can be.
pub const RMDIR: u64 = 84;
pub const UNLINK: u64 = 87;
pub const RENAMEAT: u64 = 264;
pub const MKDIRAT: u64 = 258;
pub const UNLINKAT: u64 = 263;
pub const READLINKAT: u64 = 267;
pub const READLINK: u64 = 89;
pub const SYMLINK: u64 = 88;
pub const SYMLINKAT: u64 = 266;
pub const GETTIMEOFDAY: u64 = 96;
pub const SET_ROBUST_LIST: u64 = 273;
pub const GETUID: u64 = 102;
pub const GETGID: u64 = 104;
pub const SETUID: u64 = 105;
pub const SETGID: u64 = 106;
pub const GETEUID: u64 = 107;
pub const GETEGID: u64 = 108;
// There is no `seteuid` in the x86_64 table: 109 is `setpgid`, and a libc
// implements `seteuid` with `setresuid(-1, euid, -1)`. An earlier version of
// this file gave the number to `seteuid` anyway, which nothing noticed while
// both answered `EPERM` - and which collided the moment process groups were
// added.
pub const GETPPID: u64 = 110;
/// Process groups: the set a terminal's interrupt is aimed at, and the thing
/// a shell needs before it will run jobs at all.
pub const SETPGID: u64 = 109;
pub const GETPGRP: u64 = 111;
pub const SETSID: u64 = 112;
pub const GETPGID: u64 = 121;
pub const GETDENTS64: u64 = 217;
/// `sendfile(out, in, offset, count)`: copy bytes between descriptors in the
/// kernel. Something in the init path asks for it once.
pub const SENDFILE: u64 = 40;
/// `umask(mask)`: the file-creation mask, stored and reported.
pub const UMASK: u64 = 95;
/// `rt_sigtimedwait(set, info, timeout, sigsetsize)`: wait for one signal out
/// of a set and consume it without running any handler. Busybox init's main
/// loop parks here.
pub const RT_SIGTIMEDWAIT: u64 = 128;
/// `reboot(cmd)`: init asks the machine to stop, and toggles the
/// ctrl-alt-del policy at start-up.
pub const REBOOT: u64 = 169;
/// `sethostname(name, len)`: what `/bin/hostname -F /etc/hostname` calls
/// during init's sysinit. There is one machine and no network identity to
/// keep distinct, so the name is accepted and remembered nowhere.
pub const SETHOSTNAME: u64 = 170;
/// `sync()`: flush dirty pages. A no-op here - every write already landed -
/// but init's shutdown path calls it and should not hear ENOSYS.
pub const SYNC: u64 = 162;
// --- sockets ---
// The whole family, in x86_64 order. mpos speaks AF_INET only: musl's DNS
// resolver works over UDP (syslog's AF_UNIX probe is answered EAFNOSUPPORT,
// which is how it falls back to the console), and apk talks plain HTTP over
// TCP, so stream and datagram sockets are the whole story.
pub const SOCKET: u64 = 41;
pub const CONNECT: u64 = 42;
pub const ACCEPT: u64 = 43;
pub const SENDTO: u64 = 44;
pub const RECVFROM: u64 = 45;
pub const SENDMSG: u64 = 46;
pub const RECVMSG: u64 = 47;
pub const SHUTDOWN: u64 = 48;
pub const BIND: u64 = 49;
pub const LISTEN: u64 = 50;
pub const GETSOCKNAME: u64 = 51;
pub const GETPEERNAME: u64 = 52;
pub const SOCKETPAIR: u64 = 53;
pub const SETSOCKOPT: u64 = 54;
pub const GETSOCKOPT: u64 = 55;
pub const GETTID: u64 = 186;
pub const TKILL: u64 = 200;
pub const FUTEX: u64 = 202;
pub const SCHED_GETAFFINITY: u64 = 203;
pub const SET_TID_ADDRESS: u64 = 218;
pub const CLOCK_GETTIME: u64 = 228;
pub const CLOCK_NANOSLEEP: u64 = 230;
pub const EXIT_GROUP: u64 = 231;
pub const OPENAT: u64 = 257;
pub const NEWFSTATAT: u64 = 262;
pub const FACCESSAT: u64 = 269;
pub const DUP3: u64 = 292;
pub const PIPE2: u64 = 293;
pub const PRLIMIT64: u64 = 302;
pub const RENAMEAT2: u64 = 316;
pub const GETRANDOM: u64 = 318;
pub const STATX: u64 = 332;
pub const RSEQ: u64 = 334;
pub const ARCH_PRCTL: u64 = 158;
pub const SIGALTSTACK: u64 = 131;
pub const STATFS: u64 = 137;
pub const FSTATFS: u64 = 138;
pub const SYSINFO: u64 = 99;
pub const UTIMENSAT: u64 = 280;
pub const UTIMES: u64 = 235;
pub const MADVISE: u64 = 28;
pub const PPOLL: u64 = 271;
pub const SCHED_YIELD: u64 = 24; // sched_yield(2)
pub const CLEANUP_AFTER_KTHREAD: u64 = 999;

// --- the calls a real libc reaches for that were missing -------------------
//
// Each number below is the x86_64 one from the syscall table. The list is
// what an ordinary static musl binary asks for across a `apk update`: the
// offset read/write pair, the metadata setters, the wall clock, `waitid` and
// the resource reports. They are here because the answer to each used to be
// ENOSYS, which turns a program's ordinary control flow into an error path.
pub const PREAD64: u64 = 17;
pub const PWRITE64: u64 = 18;
pub const PREADV: u64 = 295;
pub const PWRITEV: u64 = 296;
/// `fsync(2)` / `fdatasync(2)`: flush a descriptor's dirty data.
pub const FSYNC: u64 = 74;
pub const FDATASYNC: u64 = 75;
pub const CHMOD: u64 = 90;
pub const FCHMOD: u64 = 91;
pub const CHOWN: u64 = 92;
pub const FCHOWN: u64 = 93;
/// `fchownat(2)` - the *at form of the owner change, with the flag that says
/// whether the *link* or its target is meant.
pub const FCHOWNAT: u64 = 260;
/// `lchown(2)` - the owner of the *link*, not of what it points at. A tool
/// unpacking an archive sets both, and reaches for this one first: x86_64 has
/// this as a syscall of its own, and busybox calls it directly.
pub const LCHOWN: u64 = 94;
/// `mknod(2)`: create a filesystem node. mpos has no device nodes.
pub const MKNOD: u64 = 133;
/// `mount(2)`: put a filesystem on a directory. This is the call that hands
/// the mount table to userland - the kernel builds none of /proc, /sys, /dev
/// or /run on its own any more, because a system whose own init cannot mount
/// its directories is a system with a kernel doing init's job.
pub const MOUNT: u64 = 165;
/// `umount2(2)`: take one down. The lazy and force flags are accepted and
/// mean the same thing here as an immediate detach, which is what this
/// kernel's unmount already is.
pub const UMOUNT2: u64 = 166;
/// `pivot_root(2)`: move the root to `new_root`, parking the old one at
/// `put_old`. This is the call an initramfs makes to hand the machine over.
/// (155, not 160-166's neighbourhood: those are setrlimit and the mount pair.)
pub const PIVOT_ROOT: u64 = 155;
/// `chroot(2)`: change what `/` means. `switch_root` uses it after moving the
/// root, to drop the process - and here, the namespace - out of the initramfs.
pub const CHROOT: u64 = 161;
/// `signalfd(2)`: a descriptor that reports signals - child exits here.
pub const SIGNALFD: u64 = 282;
/// `eventfd(2)`: a counter a process can wait on.
pub const EVENTFD: u64 = 290;
/// `link(2)` / `linkat(2)`: a second name for an inode.
pub const LINK: u64 = 86;
pub const LINKAT: u64 = 265;
/// `time(2)`: seconds since the epoch, the original spelling of the clock.
pub const TIME: u64 = 201;
/// `times(2)`: this process's CPU time, in clock ticks.
pub const TIMES: u64 = 100;
/// `getrusage(2)`: resource use for self or for reaped children.
pub const GETRUSAGE: u64 = 98;
/// `clock_getres(2)`: the resolution a clock can actually deliver.
pub const CLOCK_GETRES: u64 = 229;
/// `waitid(2)`: the modern wait - one child, named by id type, with an
/// `siginfo_t` instead of a bare status word.
pub const WAITID: u64 = 247;
/// The legacy rlimit pair; `prlimit64` is the one musl prefers, but old
/// binaries and busybox applets still ask by these numbers.
pub const GETRLIMIT: u64 = 97;
pub const SETRLIMIT: u64 = 160;
/// `pselect6(2)`: `select` with a signal mask and a nanosecond timeout.
pub const PSELECT6: u64 = 270;
/// `fadvise64(2)`: a hint about future access. Nothing here prefetches, so
/// every advice is accepted and ignored - which is what Linux does for the
/// advice it does not act on.
pub const FADVISE64: u64 = 221;
/// `get_robust_list(2)`: the read half of the robust-futex list. Threads
/// are not implemented, so there is no list to hand back.
pub const GET_ROBUST_LIST: u64 = 274;
/// `membarrier(2)`: a memory barrier across the machine. Single core, so
/// the barrier is what a compiler fence already gives.
pub const MEMBARRIER: u64 = 324;

// `clone(2)` flag bits, as the Linux x86_64 ABI numbers them. Each one names a
// part of the caller that the child **shares** instead of copying; a zero flag
// word is therefore plain `fork`.
pub const CLONE_VM: usize = 0x0000_0100;
pub const CLONE_FS: usize = 0x0000_0200;
pub const CLONE_FILES: usize = 0x0000_0400;
pub const CLONE_SIGHAND: usize = 0x0000_0800;
pub const CLONE_VFORK: usize = 0x0000_4000;
pub const CLONE_PARENT: usize = 0x0000_8000;
pub const CLONE_THREAD: usize = 0x0001_0000;
pub const CLONE_SYSVSEM: usize = 0x0004_0000;
pub const CLONE_SETTLS: usize = 0x0008_0000;
pub const CLONE_PARENT_SETTID: usize = 0x0010_0000;
pub const CLONE_CHILD_CLEARTID: usize = 0x0020_0000;
pub const CLONE_CHILD_SETTID: usize = 0x0100_0000;
