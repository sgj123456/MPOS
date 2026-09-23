//! Linux x86_64 syscall numbers.
//!
//! These are the numbers musl/glibc uses when calling `syscall(N, ...)`.

pub const READ: u64 = 0;
pub const WRITE: u64 = 1;
pub const OPEN: u64 = 2;
pub const CLOSE: u64 = 3;
pub const STAT: u64 = 4;
pub const FSTAT: u64 = 5;
pub const LSTAT: u64 = 6;
pub const POLL: u64 = 7;
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
/// `getsid(2)`: which session a process belongs to. This kernel keeps no
/// sessions - a process's group leader stands in, which is the answer a libc
/// `setsid()` reads back to check its own work.
pub const GETSID: u64 = 124;
/// `setgroups(2)`: the supplementary groups, kept per process on the
/// identity and reported by `getgroups(2)` - the set a `:group` rule in
/// `doas.conf` matches against, and what login's `initgroups` installs.
pub const SETGROUPS: u64 = 116;
pub const GETGROUPS: u64 = 115;
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

/// The futex operations this kernel speaks. The operation is the low seven
/// bits of the `op` argument; the rest carries flags like
/// `FUTEX_PRIVATE_FLAG`, which say which table to look in - and this kernel
/// has exactly one.
pub const FUTEX_WAIT: usize = 0;
pub const FUTEX_WAKE: usize = 1;
pub const FUTEX_WAIT_BITSET: usize = 9;
pub const FUTEX_WAKE_BITSET: usize = 10;
pub const FUTEX_PRIVATE_FLAG: usize = 128;
/// The flag that renames a futex timeout's clock from monotonic to realtime.
/// This kernel reads every clock off one tick counter, so the flag changes
/// only the *spelling* of the deadline - see `futex_deadline` - never the
/// table it waits in.
pub const FUTEX_CLOCK_REALTIME: usize = 256;
pub const SCHED_SETAFFINITY: u64 = 203;
pub const SCHED_GETAFFINITY: u64 = 204;
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
/// `prctl(2)`: the miscellaneous process-control call. `mdev` and friends
/// call it for `PR_SET_NAME`; the options that touch privileges this kernel
/// does not model are answered with success, which is what a caller checking
/// the exit status wants.
pub const PRCTL: u64 = 157;
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
// The extended-attribute family (188-199), every operation in the three
// spellings: by path, by path without following a final symlink (`l`-prefixed,
// operating on the link itself), and by descriptor (`f`-prefixed). None of this
// kernel's filesystems store xattrs, so all of them answer `EOPNOTSUPP` once
// their target resolves - see the handlers in `fs`.
pub const SETXATTR: u64 = 188;
pub const LSETXATTR: u64 = 189;
pub const FSETXATTR: u64 = 190;
pub const GETXATTR: u64 = 191;
pub const LGETXATTR: u64 = 192;
pub const FGETXATTR: u64 = 193;
pub const LISTXATTR: u64 = 194;
pub const LLISTXATTR: u64 = 195;
pub const FLISTXATTR: u64 = 196;
pub const REMOVEXATTR: u64 = 197;
pub const LREMOVEXATTR: u64 = 198;
pub const FREMOVEXATTR: u64 = 199;
/// `signalfd(2)`: a descriptor that reports signals - child exits here.
pub const SIGNALFD: u64 = 282;
/// `signalfd4(2)`: the signalfd spelling musl actually makes. The original
/// `signalfd` number answers on x86_64 too, but every libc in use goes
/// through the `4` form, flags and all - a caller whose descriptor never
/// existed polls a `POLLNVAL` hole and spins.
pub const SIGNALFD4: u64 = 289;
/// `eventfd(2)`: a counter a process can wait on. The number is the
/// `eventfd2` spelling - the flags-taking form every libc makes.
pub const EVENTFD: u64 = 290;
/// `link(2)` / `linkat(2)`: a second name for an inode.
pub const LINK: u64 = 86;
pub const LINKAT: u64 = 265;
/// `time(2)`: seconds since the epoch, the original spelling of the clock.
pub const TIME: u64 = 201;
/// `times(2)`: this process's CPU time, in clock ticks.
pub const TIMES: u64 = 100;
/// `ptrace(2)`: one task observing and steering another. This is the whole of
/// what a debugger, a tracer or an `strace` is built on.
pub const PTRACE: u64 = 101;
/// `process_vm_readv(2)` / `process_vm_writev(2)`: bulk cross-process memory
/// access, named by iovec pairs rather than one word per call.
pub const PROCESS_VM_READV: u64 = 310;
pub const PROCESS_VM_WRITEV: u64 = 311;
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

// --- the POSIX completion set ----------------------------------------------
//
// The calls below are the ones a Linux table has numbers for and this
// kernel did not answer: the epoll and timerfd subsystems, the *at
// relatives and modern forms of the file calls, and the classic POSIX
// set - identity, scheduling, signals, memory advice - that a libc probes
// on its way through anything. Every number is the x86_64 one; every one
// is pinned in the tests below against the kernel's table.

/// `epoll_create(2)`: the original spelling. The size argument is ignored
/// (and has been since Linux 2.6.8) but must be positive.
pub const EPOLL_CREATE: u64 = 213;
/// `epoll_create1(2)`: the flag-taking spelling every libc makes.
pub const EPOLL_CREATE1: u64 = 291;
/// `epoll_ctl(2)`: add, change or drop one entry of an interest list.
pub const EPOLL_CTL: u64 = 233;
/// `epoll_wait(2)`: collect the events of an interest list.
pub const EPOLL_WAIT: u64 = 232;
/// `epoll_pwait(2)`: the wait with a signal mask swapped in.
pub const EPOLL_PWAIT: u64 = 281;
/// `epoll_pwait2(2)`: the pwait pair's nanosecond-timeout spelling.
pub const EPOLL_PWAIT2: u64 = 441;
/// `timerfd_create(2)`: a timer that reports its expirations through a
/// descriptor - the wait `systemd`'s timer units and every poll loop with
/// a deadline are built on.
pub const TIMERFD_CREATE: u64 = 283;
/// `inotify_init1(2)`: an event queue for filesystem news, with the flag
/// spelling. `mdev`/`udev`, `dbus` and every config re-reader start here.
pub const INOTIFY_INIT1: u64 = 294;
/// `inotify_add_watch(2)`: watch one inode for the events a mask names.
pub const INOTIFY_ADD_WATCH: u64 = 254;
/// `inotify_rm_watch(2)`: drop one watch by its descriptor.
pub const INOTIFY_RM_WATCH: u64 = 255;
/// `timerfd_settime(2)`: arm, re-arm or disarm one.
pub const TIMERFD_SETTIME: u64 = 286;
/// `timerfd_gettime(2)`: what is left of the current span, and the interval.
pub const TIMERFD_GETTIME: u64 = 287;
/// `fallocate(2)`: arrange a file's storage before writing it.
pub const FALLOCATE: u64 = 285;
/// `copy_file_range(2)`: a copy the kernel makes without the caller's
/// buffers in the middle.
pub const COPY_FILE_RANGE: u64 = 326;
/// `preadv2(2)` / `pwritev2(2)`: the vectored pair at a position, with
/// the RWF_* advice words.
pub const PREADV2: u64 = 327;
pub const PWRITEV2: u64 = 328;
/// `openat2(2)`: openat with the extensible `struct open_how`.
pub const OPENAT2: u64 = 437;
/// `faccessat2(2)`: the access check with the flags the original could
/// not take.
pub const FACCESSAT2: u64 = 439;
/// `creat(2)`: open, spelled the way the 1970s spelled it.
pub const CREAT: u64 = 85;
/// `utime(2)`: the seconds-only spelling of the timestamps call.
pub const UTIME: u64 = 132;
/// `fchmodat(2)`: the permission bits, relative to a dirfd.
pub const FCHMODAT: u64 = 268;
/// `mknodat(2)`: create a node, relative to a dirfd.
pub const MKNODAT: u64 = 259;
/// `futimesat(2)`: the timestamps call with the dirfd the future took.
pub const FUTIMESAT: u64 = 261;
/// `readahead(2)`: a promise that the range is coming.
pub const READAHEAD: u64 = 187;
/// `splice(2)`: move bytes between a pipe and anything else.
pub const SPLICE: u64 = 275;
/// `syncfs(2)`: flush the filesystem one descriptor belongs to.
pub const SYNCFS: u64 = 306;
/// `syslog(2)` / `klogctl(3)`: the kernel log ring, `dmesg`'s and a
/// `klogd`'s door.
pub const SYSLOG: u64 = 103;
/// `setdomainname(2)`: the name `uname -n`'s neighbour reports.
pub const SETDOMAINNAME: u64 = 171;
/// `setreuid(2)` / `setregid(2)`: the real-and-effective pair, where -1
/// names the id that stays.
pub const SETREUID: u64 = 113;
pub const SETREGID: u64 = 114;
/// `setresuid(2)` / `getresuid(2)` and the gid pair: the real, effective
/// and saved ids, three words in and three out.
pub const SETRESUID: u64 = 117;
pub const GETRESUID: u64 = 118;
pub const SETRESGID: u64 = 119;
pub const GETRESGID: u64 = 120;
/// `setfsuid(2)` / `setfsgid(2)`: the filesystem ids. The answer is
/// always the previous one, successful or not - the man page's own
/// observation, and the only shape a caller can probe with.
pub const SETFSUID: u64 = 122;
pub const SETFSGID: u64 = 123;
/// `capget(2)` / `capset(2)`: the capability state.
pub const CAPGET: u64 = 125;
pub const CAPSET: u64 = 126;
/// `rt_sigpending(2)`: which signals are waiting.
pub const RT_SIGPENDING: u64 = 127;
/// `rt_sigqueueinfo(2)`: `kill` with a `siginfo_t` attached.
pub const RT_SIGQUEUEINFO: u64 = 129;
/// `rt_sigsuspend(2)`: swap the mask and wait for a signal outside it.
pub const RT_SIGSUSPEND: u64 = 130;
/// `pause(2)`: the original "wait for a signal" - a sleep that only a
/// delivery ends.
pub const PAUSE: u64 = 34;
/// `tgkill(2)`: a signal to one thread, named by its group and itself.
pub const TGKILL: u64 = 234;
/// `personality(2)`: the process's execution environment word. The only
/// value this kernel has is the default one.
pub const PERSONALITY: u64 = 135;
/// `getpriority(2)` / `setpriority(2)`: the nice value, by process,
/// group or user.
pub const GETPRIORITY: u64 = 140;
pub const SETPRIORITY: u64 = 141;
/// The `sched_(get|set)(param|scheduler)` family: the scheduling policy
/// and its parameter, where this kernel has one policy and the zero
/// parameter it takes.
pub const SCHED_SETPARAM: u64 = 142;
pub const SCHED_GETPARAM: u64 = 143;
pub const SCHED_SETSCHEDULER: u64 = 144;
pub const SCHED_GETSCHEDULER: u64 = 145;
/// `sched_get_priority_max(2)` / `sched_get_priority_min(2)`: the
/// priority range a policy's band allows.
pub const SCHED_GET_PRIORITY_MAX: u64 = 146;
pub const SCHED_GET_PRIORITY_MIN: u64 = 147;
/// `sched_rr_get_interval(2)`: the round-robin slice, as a timespec.
pub const SCHED_RR_GET_INTERVAL: u64 = 148;
/// `sched_setattr(2)` / `sched_getattr(2)`: the extensible scheduling
/// attributes - the EDF/DEADLINE-shaped successor to the family above.
pub const SCHED_SETATTR: u64 = 314;
pub const SCHED_GETATTR: u64 = 315;
/// `msync(2)`: a file mapping's pages back to the file they came from.
/// The mappings here are private copies made at map time, so the sync is
/// the range check and nothing more.
pub const MSYNC: u64 = 26;
/// `mincore(2)`: which pages of a range are resident. Everything this
/// kernel maps is resident - it reads file pages in eagerly - so the
/// answer is a vec of ones and a success.
pub const MINCORE: u64 = 27;
/// The `mlock(2)` family: pages pinned into memory. Nothing here pages
/// out, so every page is already pinned and the calls are their
/// validations and a success.
pub const MLOCK: u64 = 149;
pub const MUNLOCK: u64 = 150;
pub const MLOCKALL: u64 = 151;
pub const MUNLOCKALL: u64 = 152;
/// `mlock2(2)`: the mlock pair's flag-taking form.
pub const MLOCK2: u64 = 325;
/// `vhangup(2)`: virtually hang up the controlling terminal. There is no
/// controlling-terminal bookkeeping to hang up.
pub const VHANGUP: u64 = 153;
/// `getcpu(2)`: which processor and node the caller runs on.
pub const GETCPU: u64 = 309;
/// `settimeofday(2)` / `clock_settime(2)`: set the wall clock.
pub const SETTIMEOFDAY: u64 = 164;
pub const CLOCK_SETTIME: u64 = 227;
/// `unshare(2)` / `setns(2)`: enter and leave namespaces. This kernel
/// has one of each.
pub const UNSHARE: u64 = 272;
pub const SETNS: u64 = 308;
/// `seccomp(2)`: the secure-computing mode.
pub const SECCOMP: u64 = 317;
/// `accept4(2)`: accept with flags. The flag-less original is 43.
pub const ACCEPT4: u64 = 288;
/// `sendmmsg(2)` / `recvmmsg(2)`: the multiple-message socket pair.
pub const SENDMMSG: u64 = 307;
pub const RECVMMSG: u64 = 299;

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
/// Historical no-op. Linux removed its meaning years ago but still accepts
/// the bit, and musl's `pthread_create` puts it in every thread's flag word -
/// refusing it refuses every pthread in the system.
pub const CLONE_DETACHED: usize = 0x0040_0000;
/// Historical no-op, same treatment: accepted so a caller from a kernel that
/// still sends it is not refused for a bit that does nothing.
pub const CLONE_UNTRACED: usize = 0x0080_0000;
pub const CLONE_CHILD_SETTID: usize = 0x0100_0000;

// ---------------------------------------------------------------------------
// Tests: every number against the Linux x86_64 syscall table
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers themselves are the ABI: a wrong constant here silently
    /// answers the wrong call - a `renameat` number answering `rmdir`, say,
    /// deletes what it was asked to move - and nothing downstream can catch
    /// it, because every layer below trusts the dispatch. So the whole table
    /// is pinned against the syscall(2) man page's x86_64 column, one
    /// assertion per name, and a typo reads as a red test rather than a
    /// mysterious ENOSYS (or worse, the wrong handler) in a shell.
    #[test]
    fn syscall_numbers_match_the_x86_64_table() {
        // Process and file descriptors.
        assert_eq!(READ, 0);
        assert_eq!(WRITE, 1);
        assert_eq!(OPEN, 2);
        assert_eq!(CLOSE, 3);
        assert_eq!(STAT, 4);
        assert_eq!(FSTAT, 5);
        assert_eq!(LSTAT, 6);
        assert_eq!(POLL, 7);
        assert_eq!(LSEEK, 8);
        assert_eq!(MMAP, 9);
        assert_eq!(MPROTECT, 10);
        assert_eq!(MUNMAP, 11);
        assert_eq!(BRK, 12);
        assert_eq!(RT_SIGACTION, 13);
        assert_eq!(RT_SIGPROCMASK, 14);
        assert_eq!(RT_SIGRETURN, 15);
        assert_eq!(IOCTL, 16);
        assert_eq!(PREAD64, 17);
        assert_eq!(PWRITE64, 18);
        assert_eq!(READV, 19);
        assert_eq!(WRITEV, 20);
        assert_eq!(ACCESS, 21);
        assert_eq!(PIPE, 22);
        assert_eq!(SELECT, 23);
        assert_eq!(SCHED_YIELD, 24);
        assert_eq!(MREMAP, 25);
        assert_eq!(MADVISE, 28);
        assert_eq!(DUP, 32);
        assert_eq!(DUP2, 33);
        assert_eq!(NANOSLEEP, 35);
        assert_eq!(GETITIMER, 36);
        assert_eq!(ALARM, 37);
        assert_eq!(SETITIMER, 38);
        assert_eq!(GETPID, 39);
        assert_eq!(SENDFILE, 40);
        // The socket family Linux numbers 41..55, in order.
        assert_eq!(SOCKET, 41);
        assert_eq!(CONNECT, 42);
        assert_eq!(ACCEPT, 43);
        assert_eq!(SENDTO, 44);
        assert_eq!(RECVFROM, 45);
        assert_eq!(SENDMSG, 46);
        assert_eq!(RECVMSG, 47);
        assert_eq!(SHUTDOWN, 48);
        assert_eq!(BIND, 49);
        assert_eq!(LISTEN, 50);
        assert_eq!(GETSOCKNAME, 51);
        assert_eq!(GETPEERNAME, 52);
        assert_eq!(SOCKETPAIR, 53);
        assert_eq!(SETSOCKOPT, 54);
        assert_eq!(GETSOCKOPT, 55);
        assert_eq!(CLONE, 56);
        assert_eq!(FORK, 57);
        assert_eq!(VFORK, 58);
        assert_eq!(EXECVE, 59);
        assert_eq!(EXIT, 60);
        assert_eq!(WAIT4, 61);
        assert_eq!(KILL, 62);
        assert_eq!(UNAME, 63);
        assert_eq!(FCNTL, 72);
        assert_eq!(FLOCK, 73);
        assert_eq!(FSYNC, 74);
        assert_eq!(FDATASYNC, 75);
        assert_eq!(TRUNCATE, 76);
        assert_eq!(FTRUNCATE, 77);
        assert_eq!(GETCWD, 79);
        assert_eq!(CHDIR, 80);
        assert_eq!(FCHDIR, 81);
        assert_eq!(RENAME, 82);
        assert_eq!(MKDIR, 83);
        assert_eq!(RMDIR, 84);
        assert_eq!(LINK, 86);
        assert_eq!(UNLINK, 87);
        assert_eq!(SYMLINK, 88);
        assert_eq!(READLINK, 89);
        assert_eq!(CHMOD, 90);
        assert_eq!(FCHMOD, 91);
        assert_eq!(CHOWN, 92);
        assert_eq!(FCHOWN, 93);
        assert_eq!(LCHOWN, 94);
        assert_eq!(UMASK, 95);
        assert_eq!(GETTIMEOFDAY, 96);
        assert_eq!(GETRLIMIT, 97);
        assert_eq!(GETRUSAGE, 98);
        assert_eq!(SYSINFO, 99);
        assert_eq!(TIMES, 100);
        assert_eq!(PTRACE, 101);
        assert_eq!(PROCESS_VM_READV, 310);
        assert_eq!(PROCESS_VM_WRITEV, 311);
        assert_eq!(GETUID, 102);
        assert_eq!(GETGID, 104);
        assert_eq!(SETUID, 105);
        assert_eq!(SETGID, 106);
        assert_eq!(GETEUID, 107);
        assert_eq!(GETEGID, 108);
        assert_eq!(SETPGID, 109);
        // The xattr family occupies the whole 188-199 block on x86_64, in the
        // set/get/list/remove order, each plain then `l`- then `f`-prefixed.
        assert_eq!(SETXATTR, 188);
        assert_eq!(LSETXATTR, 189);
        assert_eq!(FSETXATTR, 190);
        assert_eq!(GETXATTR, 191);
        assert_eq!(LGETXATTR, 192);
        assert_eq!(FGETXATTR, 193);
        assert_eq!(LISTXATTR, 194);
        assert_eq!(LLISTXATTR, 195);
        assert_eq!(FLISTXATTR, 196);
        assert_eq!(REMOVEXATTR, 197);
        assert_eq!(LREMOVEXATTR, 198);
        assert_eq!(FREMOVEXATTR, 199);
        assert_eq!(GETPPID, 110);
        assert_eq!(GETPGRP, 111);
        assert_eq!(SETSID, 112);
        assert_eq!(GETGROUPS, 115);
        assert_eq!(SETGROUPS, 116);
        assert_eq!(GETPGID, 121);
        assert_eq!(GETSID, 124);
        assert_eq!(SIGALTSTACK, 131);
        assert_eq!(MKNOD, 133);
        assert_eq!(STATFS, 137);
        assert_eq!(FSTATFS, 138);
        assert_eq!(PIVOT_ROOT, 155);
        assert_eq!(PRCTL, 157);
        assert_eq!(ARCH_PRCTL, 158);
        assert_eq!(SETRLIMIT, 160);
        assert_eq!(CHROOT, 161);
        assert_eq!(SYNC, 162);
        assert_eq!(MOUNT, 165);
        assert_eq!(UMOUNT2, 166);
        assert_eq!(REBOOT, 169);
        assert_eq!(SETHOSTNAME, 170);
        assert_eq!(FADVISE64, 221);
        // The identity and futex block.
        assert_eq!(SET_TID_ADDRESS, 218);
        assert_eq!(GETDENTS64, 217);
        assert_eq!(GETTID, 186);
        assert_eq!(TKILL, 200);
        assert_eq!(TIME, 201);
        assert_eq!(FUTEX, 202);
        assert_eq!(SCHED_SETAFFINITY, 203);
        assert_eq!(SCHED_GETAFFINITY, 204);
        assert_eq!(RT_SIGTIMEDWAIT, 128);
        assert_eq!(CLOCK_GETRES, 229);
        assert_eq!(CLOCK_GETTIME, 228);
        assert_eq!(CLOCK_NANOSLEEP, 230);
        assert_eq!(EXIT_GROUP, 231);
        assert_eq!(UTIMES, 235);
        assert_eq!(WAITID, 247);
        // The *at family and the modern additions.
        assert_eq!(OPENAT, 257);
        assert_eq!(MKDIRAT, 258);
        assert_eq!(FCHOWNAT, 260);
        assert_eq!(NEWFSTATAT, 262);
        assert_eq!(UNLINKAT, 263);
        assert_eq!(RENAMEAT, 264);
        assert_eq!(LINKAT, 265);
        assert_eq!(SYMLINKAT, 266);
        assert_eq!(READLINKAT, 267);
        assert_eq!(FACCESSAT, 269);
        assert_eq!(PSELECT6, 270);
        assert_eq!(PPOLL, 271);
        assert_eq!(SET_ROBUST_LIST, 273);
        assert_eq!(GET_ROBUST_LIST, 274);
        assert_eq!(UTIMENSAT, 280);
        assert_eq!(SIGNALFD, 282);
        assert_eq!(SIGNALFD4, 289);
        assert_eq!(EVENTFD, 290);
        assert_eq!(PREADV, 295);
        assert_eq!(PWRITEV, 296);
        assert_eq!(DUP3, 292);
        assert_eq!(PIPE2, 293);
        assert_eq!(PRLIMIT64, 302);
        assert_eq!(RENAMEAT2, 316);
        assert_eq!(GETRANDOM, 318);
        assert_eq!(RSEQ, 334);
        assert_eq!(STATX, 332);
        assert_eq!(MEMBARRIER, 324);
    }

    /// `clone(2)`: the flag bits, as the same table numbers them. The
    /// dependency checks and the low-byte exit signal are `abi`'s business;
    /// here only the bit values are pinned.
    #[test]
    fn clone_flag_bits_match_the_x86_64_abi() {
        assert_eq!(CLONE_VM, 0x0000_0100);
        assert_eq!(CLONE_FS, 0x0000_0200);
        assert_eq!(CLONE_FILES, 0x0000_0400);
        assert_eq!(CLONE_SIGHAND, 0x0000_0800);
        assert_eq!(CLONE_VFORK, 0x0000_4000);
        assert_eq!(CLONE_PARENT, 0x0000_8000);
        assert_eq!(CLONE_THREAD, 0x0001_0000);
        assert_eq!(CLONE_SYSVSEM, 0x0004_0000);
        assert_eq!(CLONE_SETTLS, 0x0008_0000);
        assert_eq!(CLONE_PARENT_SETTID, 0x0010_0000);
        assert_eq!(CLONE_CHILD_CLEARTID, 0x0020_0000);
        assert_eq!(CLONE_CHILD_SETTID, 0x0100_0000);
        assert_eq!(CLONE_DETACHED, 0x0040_0000);
        assert_eq!(CLONE_UNTRACED, 0x0080_0000);
        // Every flag bit clears the low byte's worth - the exit signal
        // cannot masquerade as a flag, and no flag can masquerade as one.
        for bit in [
            CLONE_VM,
            CLONE_FS,
            CLONE_FILES,
            CLONE_SIGHAND,
            CLONE_VFORK,
            CLONE_PARENT,
            CLONE_THREAD,
            CLONE_SYSVSEM,
            CLONE_SETTLS,
            CLONE_PARENT_SETTID,
            CLONE_CHILD_CLEARTID,
            CLONE_CHILD_SETTID,
            CLONE_DETACHED,
            CLONE_UNTRACED,
        ] {
            assert!(bit & 0xff == 0, "flag bit {bit:#x} reaches into CSIGNAL");
            assert!(bit.count_ones() == 1, "flag bit {bit:#x} is not one bit");
        }
    }

    /// `futex(2)`: the operation numbers and the private-table flag. The
    /// ops are a plain table on the man page - WAIT_BITSET is not a bit
    /// combination of WAIT and anything, so each number is pinned on its
    /// own.
    #[test]
    fn futex_op_numbers_match_the_man_page() {
        assert_eq!(FUTEX_WAIT, 0);
        assert_eq!(FUTEX_WAKE, 1);
        assert_eq!(FUTEX_WAIT_BITSET, 9);
        assert_eq!(FUTEX_WAKE_BITSET, 10);
        assert_eq!(FUTEX_PRIVATE_FLAG, 128);
        // The private flag lives above the operation field, which the
        // dispatch masks off before matching - it must never collide with
        // an operation number's seven bits.
        assert_eq!(FUTEX_PRIVATE_FLAG & 0x7f, 0);
    }

    /// `CLEANUP_AFTER_KTHREAD` is mpos's own bookkeeping number, not a Linux
    /// call: it must stay clear of every Linux number this kernel names, so a
    /// stray request for it can never be mistaken for a real call. It sits
    /// past the ENOSYS dedup window too, which is fine - an internal number
    /// has no userland caller to dedupe.
    #[test]
    fn cleanup_after_kthread_is_an_internal_number() {
        assert_eq!(CLEANUP_AFTER_KTHREAD, 999);
        assert!(CLEANUP_AFTER_KTHREAD > STATX, "collides with a Linux call");
        assert!(
            CLEANUP_AFTER_KTHREAD >= 256,
            "inside the mark_missing window"
        );
    }

    /// The POSIX completion set, pinned against the kernel's x86_64 table
    /// one assertion per name - same rule as the table above, applied to
    /// the calls that complete the surface: the epoll and timerfd
    /// subsystems, the *at and modern forms of the file calls, and the
    /// classic set a libc probes on its way through anything.
    #[test]
    fn posix_completion_numbers_match_the_x86_64_table() {
        // The epoll family: the original create, the flag-taking one, and
        // the wait in all three spellings.
        assert_eq!(EPOLL_CREATE, 213);
        assert_eq!(EPOLL_CREATE1, 291);
        assert_eq!(EPOLL_CTL, 233);
        assert_eq!(EPOLL_WAIT, 232);
        assert_eq!(EPOLL_PWAIT, 281);
        assert_eq!(EPOLL_PWAIT2, 441);
        // The timerfd triple.
        assert_eq!(TIMERFD_CREATE, 283);
        assert_eq!(TIMERFD_SETTIME, 286);
        assert_eq!(TIMERFD_GETTIME, 287);
        // The inotify triple: the queue, the watch in and the watch out.
        assert_eq!(INOTIFY_INIT1, 294);
        assert_eq!(INOTIFY_ADD_WATCH, 254);
        assert_eq!(INOTIFY_RM_WATCH, 255);
        // File I/O: allocate, copy, the vectored pair with advice, the
        // extensible open, and the *at and legacy spellings.
        assert_eq!(FALLOCATE, 285);
        assert_eq!(COPY_FILE_RANGE, 326);
        assert_eq!(PREADV2, 327);
        assert_eq!(PWRITEV2, 328);
        assert_eq!(OPENAT2, 437);
        assert_eq!(FACCESSAT2, 439);
        assert_eq!(CREAT, 85);
        assert_eq!(UTIME, 132);
        assert_eq!(FCHMODAT, 268);
        assert_eq!(MKNODAT, 259);
        assert_eq!(FUTIMESAT, 261);
        assert_eq!(READAHEAD, 187);
        assert_eq!(SPLICE, 275);
        assert_eq!(SYNCFS, 306);
        assert_eq!(SYSLOG, 103);
        assert_eq!(SETDOMAINNAME, 171);
        // Identity: the re, res and fs spellings, and the capabilities.
        assert_eq!(SETREUID, 113);
        assert_eq!(SETREGID, 114);
        assert_eq!(SETRESUID, 117);
        assert_eq!(GETRESUID, 118);
        assert_eq!(SETRESGID, 119);
        assert_eq!(GETRESGID, 120);
        assert_eq!(SETFSUID, 122);
        assert_eq!(SETFSGID, 123);
        assert_eq!(CAPGET, 125);
        assert_eq!(CAPSET, 126);
        // Signals: pending, queued, suspended, paused, and one thread's.
        assert_eq!(RT_SIGPENDING, 127);
        assert_eq!(RT_SIGQUEUEINFO, 129);
        assert_eq!(RT_SIGSUSPEND, 130);
        assert_eq!(PAUSE, 34);
        assert_eq!(TGKILL, 234);
        // The personality word and the nice value.
        assert_eq!(PERSONALITY, 135);
        assert_eq!(GETPRIORITY, 140);
        assert_eq!(SETPRIORITY, 141);
        // The scheduling family, classic and extensible.
        assert_eq!(SCHED_SETPARAM, 142);
        assert_eq!(SCHED_GETPARAM, 143);
        assert_eq!(SCHED_SETSCHEDULER, 144);
        assert_eq!(SCHED_GETSCHEDULER, 145);
        assert_eq!(SCHED_GET_PRIORITY_MAX, 146);
        assert_eq!(SCHED_GET_PRIORITY_MIN, 147);
        assert_eq!(SCHED_RR_GET_INTERVAL, 148);
        assert_eq!(SCHED_SETATTR, 314);
        assert_eq!(SCHED_GETATTR, 315);
        // Memory: sync, residency, and the lock family.
        assert_eq!(MSYNC, 26);
        assert_eq!(MINCORE, 27);
        assert_eq!(MLOCK, 149);
        assert_eq!(MUNLOCK, 150);
        assert_eq!(MLOCKALL, 151);
        assert_eq!(MUNLOCKALL, 152);
        assert_eq!(MLOCK2, 325);
        // The machine: which cpu, the wall clock, and the names a
        // namespace would need.
        assert_eq!(VHANGUP, 153);
        assert_eq!(GETCPU, 309);
        assert_eq!(SETTIMEOFDAY, 164);
        assert_eq!(CLOCK_SETTIME, 227);
        assert_eq!(UNSHARE, 272);
        assert_eq!(SETNS, 308);
        assert_eq!(SECCOMP, 317);
        // The socket trio.
        assert_eq!(ACCEPT4, 288);
        assert_eq!(SENDMMSG, 307);
        assert_eq!(RECVMMSG, 299);
    }
}
