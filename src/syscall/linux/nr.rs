//! Linux syscall numbers, selected per target architecture.
//!
//! x86_64 and aarch64 do not share a numbering *or* a set: aarch64 uses the
//! asm-generic ABI, where the legacy `open`/`stat`/`poll`/`select`/`fork` calls
//! are gone entirely and every surviving number differs. The two tables are
//! therefore whole files, pulled in by `#[cfg]` - not one list with per-name
//! `#[cfg]` on each value - because the divergence is the ABI, not a handful of
//! rows. Each table holds the same `pub const` names, so `dispatch`'s match
//! arms and the tests below keep addressing `nr::NAME` unchanged; only the
//! numbers differ by machine. `include!` (rather than `mod`) is used so this
//! stays one addressable file. The std-side test suite takes its numbers from
//! `libc::SYS_*` instead, which is the other direction of the same guarantee:
//! a table the kernel wrote and a table the tests wrote could agree and still
//! both be wrong, and two sources that cannot be reconciled cannot hide that.
//!
//! `include!` pastes the file's contents into this module verbatim. On the host
//! and x86_64 targets the x86 table is compiled, and the tests below - every
//! number pinned against the x86_64 ABI one assertion at a time - run against
//! it. On aarch64 the asm-generic table is compiled instead.

#[cfg(target_arch = "aarch64")]
include!("nr_aarch64.rs");
#[cfg(not(target_arch = "aarch64"))]
include!("nr_x86_64.rs");

pub use mpos_abi::{
    CLONE_CHILD_CLEARTID, CLONE_CHILD_SETTID, CLONE_CLEAR_SIGHAND, CLONE_DETACHED, CLONE_FILES,
    CLONE_FS, CLONE_INTO_CGROUP, CLONE_NAMESPACES, CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWNET,
    CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWTIME, CLONE_NEWUSER, CLONE_NEWUTS, CLONE_PARENT,
    CLONE_PARENT_SETTID, CLONE_PIDFD, CLONE_SETTLS, CLONE_SIGHAND, CLONE_SYSVSEM, CLONE_THREAD,
    CLONE_UNTRACED, CLONE_VFORK, CLONE_VM,
};

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
        assert_eq!(TIMER_CREATE, 222);
        assert_eq!(TIMER_SETTIME, 223);
        assert_eq!(TIMER_GETTIME, 224);
        assert_eq!(TIMER_GETOVERRUN, 225);
        assert_eq!(TIMER_DELETE, 226);
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
        // The POSIX message-queue family, whose numbers are consecutive on both
        // machines - a transcription error here is a family that silently
        // answers `ENOSYS` because the dispatch arm never matches.
        assert_eq!(MQ_OPEN, 240);
        assert_eq!(MQ_UNLINK, 241);
        assert_eq!(MQ_TIMEDSEND, 242);
        assert_eq!(MQ_TIMEDRECEIVE, 243);
        assert_eq!(MQ_NOTIFY, 244);
        assert_eq!(MQ_GETSETATTR, 245);
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

    /// The eight namespace flags, pinned apart from the sharing flags above
    /// because one of them would fail that loop's own rule.
    ///
    /// `CLONE_NEWTIME` is `0x80`, inside the low byte the sharing flags stay
    /// clear of - so a table written from the pattern "a `CLONE_NEW*` is a bit
    /// in the high half" gets it wrong, and this file got it wrong: it read
    /// `CLONE_NEWTIME` as `CLONE_SETTLS` and `CLONE_NEWNS` as `CLONE_THREAD`.
    /// The two errors compound into the one shape that is worst to debug, an
    /// *answered* call: a program asking to unshare the mount namespace was
    /// told `EINVAL` as though the bit were undefined, while a program passing
    /// `CLONE_THREAD` was refused a namespace it never asked for. Both are
    /// measured on a real kernel and pinned here.
    #[test]
    fn namespace_flags_match_include_uapi_linux_sched_h() {
        assert_eq!(CLONE_NEWTIME, 0x0000_0080);
        assert_eq!(CLONE_NEWNS, 0x0002_0000);
        assert_eq!(CLONE_NEWCGROUP, 0x0200_0000);
        assert_eq!(CLONE_NEWUTS, 0x0400_0000);
        assert_eq!(CLONE_NEWIPC, 0x0800_0000);
        assert_eq!(CLONE_NEWUSER, 0x1000_0000);
        assert_eq!(CLONE_NEWPID, 0x2000_0000);
        assert_eq!(CLONE_NEWNET, 0x4000_0000);

        for bit in [
            CLONE_NEWTIME,
            CLONE_NEWNS,
            CLONE_NEWCGROUP,
            CLONE_NEWUTS,
            CLONE_NEWIPC,
            CLONE_NEWUSER,
            CLONE_NEWPID,
            CLONE_NEWNET,
        ] {
            assert_eq!(bit.count_ones(), 1, "namespace bit {bit:#x} is not one bit");
            assert_ne!(
                CLONE_NAMESPACES & bit,
                0,
                "{bit:#x} is missing from the mask"
            );
        }
        assert_eq!(CLONE_NAMESPACES.count_ones(), 8);

        // The overlap that makes the table easy to write wrongly, asserted as
        // the fact it is rather than left as an absence: this is the only
        // namespace bit that shares the `CSIGNAL` byte.
        assert_eq!(CLONE_NEWTIME & 0xff, 0x80);

        // And no *sharing* flag is also a namespace, which is the specific
        // mistake - `CLONE_THREAD` written where `CLONE_NEWNS` belongs.
        for bit in [
            CLONE_VM,
            CLONE_FS,
            CLONE_FILES,
            CLONE_SIGHAND,
            CLONE_THREAD,
            CLONE_SYSVSEM,
        ] {
            assert_eq!(
                CLONE_NAMESPACES & bit,
                0,
                "{bit:#x} is spelled as both a sharing flag and a namespace"
            );
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
        assert_eq!(FCHMODAT2, 452);
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
