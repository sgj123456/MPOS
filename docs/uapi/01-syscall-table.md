# UAPI §1

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

## 1. The syscall table

`src/syscall/linux/nr.rs` selects the machine's table — `nr_x86_64.rs` names **252** x86_64
syscall numbers, `nr_aarch64.rs` the same **252** names under aarch64's numbers (the two tables
are one name set; only the words differ). The dispatch in `src/syscall/linux/mod.rs` has an
explicit arm for **251** of them plus a `_ => ENOSYS` catch-all for the rest. That catch-all is
deliberately crowded and is an honest answer (`AGENTS.md` §3.2): a libc probes optional calls by
number and reads `ENOSYS` as "not available here".

### 1.0 Every served call, one row each

The full inventory of what the dispatch answers, so that the review question can be asked of the
whole surface at once. **Owner** is the handler module inside `src/syscall/linux/` (`fs` =
`fs/file.rs` + `fs/ioctl.rs`, `net` = the socket family forwarded to `src/net`, `kernel` = a
direct answer in the dispatch itself); **Nr x86** / **Nr arm** are the two machines' numbers from
the `nr_*` tables. A row's *behaviour* claim lives where it was measured: the per-family
implementation record is `docs/notes/` (§1–§12), the refusals are §1.1 below, the named sub-gaps
are §1.2, and each call's verdict-bearing detail — errnos, flag vocabularies, layouts — is in
[this library's other files](index.md) (§2 ioctls, §3 flags, §4 layouts). A call not listed there
is served as its phase section records it.

| Syscall | Nr x86 | Owner | Nr arm |
|---|---|---|---|
| `read` | 0 | fs | 63 |
| `write` | 1 | fs | 64 |
| `open` | 2 | fs | 64000 |
| `close` | 3 | fs | 57 |
| `stat` | 4 | fs | 64001 |
| `fstat` | 5 | fs | 80 |
| `lstat` | 6 | fs | 64002 |
| `poll` | 7 | fs | 64003 |
| `lseek` | 8 | fs | 62 |
| `mmap` | 9 | proc | 222 |
| `mprotect` | 10 | proc | 226 |
| `munmap` | 11 | proc | 215 |
| `brk` | 12 | proc | 214 |
| `rt_sigaction` | 13 | proc | 134 |
| `rt_sigprocmask` | 14 | proc | 135 |
| `rt_sigreturn` | 15 | proc | 139 |
| `ioctl` | 16 | fs | 29 |
| `pread64` | 17 | fs | 67 |
| `pwrite64` | 18 | fs | 68 |
| `readv` | 19 | fs | 65 |
| `writev` | 20 | fs | 66 |
| `access` | 21 | fs | 64004 |
| `pipe` | 22 | fs | 64005 |
| `select` | 23 | fs | 64009 |
| `sched_yield` | 24 | proc | 124 |
| `mremap` | 25 | proc | 216 |
| `msync` | 26 | posix | 227 |
| `mincore` | 27 | posix | 232 |
| `madvise` | 28 | proc | 233 |
| `dup` | 32 | fs | 23 |
| `dup2` | 33 | fs | 64006 |
| `pause` | 34 | posix | 64033 |
| `nanosleep` | 35 | proc | 101 |
| `getitimer` | 36 | proc | 102 |
| `alarm` | 37 | proc | 64010 |
| `setitimer` | 38 | proc | 103 |
| `getpid` | 39 | proc | 172 |
| `sendfile` | 40 | proc | 71 |
| `socket` | 41 | net | 198 |
| `connect` | 42 | net | 203 |
| `accept` | 43 | net | 202 |
| `sendto` | 44 | net | 206 |
| `recvfrom` | 45 | net | 207 |
| `sendmsg` | 46 | net | 211 |
| `recvmsg` | 47 | net | 212 |
| `shutdown` | 48 | net | 210 |
| `bind` | 49 | net | 200 |
| `listen` | 50 | net | 201 |
| `getsockname` | 51 | net | 204 |
| `getpeername` | 52 | net | 205 |
| `socketpair` | 53 | net | 199 |
| `setsockopt` | 54 | net | 208 |
| `getsockopt` | 55 | net | 209 |
| `clone` | 56 | proc | 220 |
| `fork` | 57 | proc | 64007 |
| `vfork` | 58 | proc | 64008 |
| `execve` | 59 | proc | 221 |
| `exit` | 60 | proc | 93 |
| `wait4` | 61 | proc | 260 |
| `kill` | 62 | proc | 129 |
| `uname` | 63 | proc | 160 |
| `fcntl` | 72 | fs | 25 |
| `flock` | 73 | fs | 32 |
| `fsync` | 74 | fs | 82 |
| `fdatasync` | 75 | fs | 83 |
| `truncate` | 76 | fs | 45 |
| `ftruncate` | 77 | fs | 46 |
| `getcwd` | 79 | fs | 17 |
| `chdir` | 80 | fs | 49 |
| `fchdir` | 81 | fs | 50 |
| `rename` | 82 | fs | 64011 |
| `mkdir` | 83 | fs | 64012 |
| `rmdir` | 84 | fs | 64013 |
| `creat` | 85 | fs | 64030 |
| `link` | 86 | fs | 64026 |
| `unlink` | 87 | fs | 64014 |
| `symlink` | 88 | fs | 64016 |
| `readlink` | 89 | fs | 64015 |
| `chmod` | 90 | fs | 64020 |
| `fchmod` | 91 | fs | 52 |
| `chown` | 92 | fs | 64021 |
| `fchown` | 93 | fs | 55 |
| `lchown` | 94 | fs | 64022 |
| `umask` | 95 | abi | 166 |
| `gettimeofday` | 96 | proc | 169 |
| `getrlimit` | 97 | abi | 163 |
| `getrusage` | 98 | proc | 165 |
| `sysinfo` | 99 | proc | 179 |
| `times` | 100 | proc | 153 |
| `ptrace` | 101 | proc | 117 |
| `getuid` | 102 | posix | 174 |
| `syslog` | 103 | fs | 116 |
| `getgid` | 104 | posix | 176 |
| `setuid` | 105 | posix | 146 |
| `setgid` | 106 | posix | 144 |
| `geteuid` | 107 | posix | 175 |
| `getegid` | 108 | posix | 177 |
| `setpgid` | 109 | proc | 154 |
| `getppid` | 110 | proc | 173 |
| `getpgrp` | 111 | proc | 64017 |
| `setsid` | 112 | proc | 157 |
| `setreuid` | 113 | posix | 145 |
| `setregid` | 114 | posix | 143 |
| `getgroups` | 115 | posix | 158 |
| `setgroups` | 116 | posix | 159 |
| `setresuid` | 117 | posix | 147 |
| `getresuid` | 118 | posix | 148 |
| `setresgid` | 119 | posix | 149 |
| `getresgid` | 120 | posix | 150 |
| `getpgid` | 121 | proc | 155 |
| `setfsuid` | 122 | posix | 151 |
| `setfsgid` | 123 | posix | 152 |
| `getsid` | 124 | proc | 156 |
| `capget` | 125 | posix | 90 |
| `capset` | 126 | posix | 91 |
| `rt_sigpending` | 127 | posix | 136 |
| `rt_sigtimedwait` | 128 | proc | 137 |
| `rt_sigqueueinfo` | 129 | posix | 138 |
| `rt_sigsuspend` | 130 | posix | 133 |
| `sigaltstack` | 131 | proc | 132 |
| `utime` | 132 | fs | 64031 |
| `mknod` | 133 | fs | 64023 |
| `personality` | 135 | posix | 92 |
| `statfs` | 137 | fs | 43 |
| `fstatfs` | 138 | fs | 44 |
| `getpriority` | 140 | posix | 141 |
| `setpriority` | 141 | posix | 140 |
| `sched_setparam` | 142 | posix | 118 |
| `sched_getparam` | 143 | posix | 121 |
| `sched_setscheduler` | 144 | posix | 119 |
| `sched_getscheduler` | 145 | posix | 120 |
| `sched_get_priority_max` | 146 | posix | 125 |
| `sched_get_priority_min` | 147 | posix | 126 |
| `sched_rr_get_interval` | 148 | posix | 127 |
| `mlock` | 149 | posix | 228 |
| `munlock` | 150 | posix | 229 |
| `mlockall` | 151 | posix | 230 |
| `munlockall` | 152 | posix | 231 |
| `vhangup` | 153 | posix | 58 |
| `pivot_root` | 155 | fs | 41 |
| `prctl` | 157 | abi | 167 |
| `arch_prctl` | 158 | proc | 64018 |
| `setrlimit` | 160 | abi | 164 |
| `chroot` | 161 | fs | 51 |
| `sync` | 162 | kernel | 81 | (`SYNC` answers 0 directly — every write already reached its storage (ramfs/ext2 write through))
| `settimeofday` | 164 | posix | 170 |
| `mount` | 165 | fs | 40 |
| `umount2` | 166 | fs | 39 |
| `reboot` | 169 | proc | 142 |
| `sethostname` | 170 | proc | 161 |
| `setdomainname` | 171 | posix | 162 |
| `gettid` | 186 | proc | 178 |
| `readahead` | 187 | fs | 213 |
| `setxattr` | 188 | fs | 5 |
| `lsetxattr` | 189 | fs | 6 |
| `fsetxattr` | 190 | fs | 7 |
| `getxattr` | 191 | fs | 8 |
| `lgetxattr` | 192 | fs | 9 |
| `fgetxattr` | 193 | fs | 10 |
| `listxattr` | 194 | fs | 11 |
| `llistxattr` | 195 | fs | 12 |
| `flistxattr` | 196 | fs | 13 |
| `removexattr` | 197 | fs | 14 |
| `lremovexattr` | 198 | fs | 15 |
| `fremovexattr` | 199 | fs | 16 |
| `tkill` | 200 | proc | 130 |
| `time` | 201 | proc | 64027 |
| `futex` | 202 | proc | 98 |
| `sched_setaffinity` | 203 | abi | 122 |
| `sched_getaffinity` | 204 | abi | 123 |
| `epoll_create` | 213 | fs | 64028 | (legacy door; aarch64 has no number for it (`nr_aarch64.rs`))
| `getdents64` | 217 | fs | 61 |
| `set_tid_address` | 218 | proc | 96 |
| `fadvise64` | 221 | fs | 223 |
| `timer_create` | 222 | proc | 107 |
| `timer_settime` | 223 | proc | 110 |
| `timer_gettime` | 224 | proc | 108 |
| `timer_getoverrun` | 225 | proc | 109 |
| `timer_delete` | 226 | proc | 111 |
| `clock_settime` | 227 | posix | 112 |
| `clock_gettime` | 228 | proc | 113 |
| `clock_getres` | 229 | proc | 114 |
| `clock_nanosleep` | 230 | proc | 115 |
| `exit_group` | 231 | proc | 94 |
| `epoll_wait` | 232 | fs | 64029 |
| `epoll_ctl` | 233 | fs | 21 |
| `tgkill` | 234 | posix | 131 |
| `utimes` | 235 | fs | 64019 |
| `waitid` | 247 | proc | 95 |
| `inotify_add_watch` | 254 | fs | 27 |
| `inotify_rm_watch` | 255 | fs | 28 |
| `openat` | 257 | fs | 56 |
| `mkdirat` | 258 | fs | 34 |
| `mknodat` | 259 | fs | 33 |
| `fchownat` | 260 | fs | 54 |
| `futimesat` | 261 | fs | 64032 |
| `newfstatat` | 262 | fs | 79 |
| `unlinkat` | 263 | fs | 35 |
| `renameat` | 264 | fs | 38 |
| `linkat` | 265 | fs | 37 |
| `symlinkat` | 266 | fs | 36 |
| `readlinkat` | 267 | fs | 78 |
| `fchmodat` | 268 | fs | 53 |
| `faccessat` | 269 | fs | 48 |
| `pselect6` | 270 | fs | 72 |
| `ppoll` | 271 | fs | 73 |
| `unshare` | 272 | posix | 97 |
| `set_robust_list` | 273 | abi | 99 |
| `get_robust_list` | 274 | abi | 100 |
| `splice` | 275 | fs | 76 |
| `utimensat` | 280 | fs | 88 |
| `epoll_pwait` | 281 | fs | 22 |
| `signalfd` | 282 | fs | 64024 |
| `timerfd_create` | 283 | fs | 85 |
| `fallocate` | 285 | fs | 47 |
| `timerfd_settime` | 286 | fs | 86 |
| `timerfd_gettime` | 287 | fs | 87 |
| `accept4` | 288 | net | 242 |
| `signalfd4` | 289 | fs | 74 |
| `eventfd` | 290 | fs | 19 |
| `epoll_create1` | 291 | fs | 20 |
| `dup3` | 292 | fs | 24 |
| `pipe2` | 293 | fs | 59 |
| `inotify_init1` | 294 | fs | 26 |
| `preadv` | 295 | fs | 69 |
| `pwritev` | 296 | fs | 70 |
| `recvmmsg` | 299 | net | 243 |
| `prlimit64` | 302 | abi | 261 |
| `syncfs` | 306 | fs | 267 |
| `sendmmsg` | 307 | net | 269 |
| `setns` | 308 | posix | 268 |
| `getcpu` | 309 | posix | 168 |
| `process_vm_readv` | 310 | proc | 270 |
| `process_vm_writev` | 311 | proc | 271 |
| `sched_setattr` | 314 | posix | 274 |
| `sched_getattr` | 315 | posix | 275 |
| `renameat2` | 316 | fs | 276 |
| `seccomp` | 317 | posix | 277 |
| `getrandom` | 318 | abi | 278 |
| `memfd_create` | 319 | fs | 279 |
| `membarrier` | 324 | abi | 283 |
| `mlock2` | 325 | posix | 284 |
| `copy_file_range` | 326 | fs | 285 |
| `preadv2` | 327 | fs | 286 |
| `pwritev2` | 328 | fs | 287 |
| `statx` | 332 | fs | 291 |
| `openat2` | 437 | fs | 437 |
| `faccessat2` | 439 | fs | 439 |
| `epoll_pwait2` | 441 | fs | 441 |
| `fchmodat2` | 452 | fs | 452 |
| `cleanup_after_kthread` | 999 | kernel | 999 | (the kernel's own kthread-cleanup hook, named by number so the ABI stays Linux-shaped; there is no Linux syscall at 999 on either machine's table)

### 1.1 Deliberately `ENOSYS` — refused, pinned

`user/abitest/src/bin/syscall/t-enosys.rs` walks these and asserts `ENOSYS`, plus a console
note printed once per number (`note_enosys`):

| Syscall | Number | Why refused | Prerequisite to serve |
|---|---|---|---|
| `keyctl` | 250 | Kernel keyring | A key store and the `KEYCTL_*` command set |
| `fanotify_init` | 300 | Filesystem-wide event notification | A fanotify watch/mark engine and permission events |
| `fanotify_mark` | 301 | " | " |
| `userfaultfd` | 323 | Userspace page-fault handling | Fault forwarding to a user task + `UFFDIO_*` |
| `io_uring_setup` | 425 | Async I/O rings | A ring ABI, SQ/CQ, registration — deferred by design (§1.3) |
| `io_uring_enter` | 426 | " | " |
| `io_uring_register` | 427 | " | " |

> The numbers for `fanotify` above are the header's (`300`/`301`), and `docs/notes/`'s board
> entry that says `302`/`303` is a typo for them — `t-enosys` pins `300`/`301`, which is
> what a caller sends.

`rseq` (334) is the eighth: like the seven above it has no dispatch arm, but unlike them it
*once had* one that returned `0` without writing any of the fields the ABI contract puts in
the caller's block. §3.2 ranks that shape worst, so the arm was removed; the number stays
named (append-only) and `t-rseq` pins `ENOSYS`. Serving it needs a per-thread registration
whose `cpu_id`/`node_id`/`mm_cid` are maintained across preemption, plus
`RSEQ_FLAG_UNREGISTER` — a scheduler feature (`docs/notes/` §12.1 A6).

### 1.2 Served with a named sub-gap

Not refusals, not wrong answers: the call is real, and one documented corner of Linux's
contract is not. Each is tracked in its phase section; the list is here so it is countable.

| Syscall | Sub-gap | Where |
|---|---|---|
| `rt_sigqueueinfo` / `sigqueue` | real-time signals `32..=64` are now fully queued (stage C: every copy delivered FIFO with its own `si_value`); the one divergence is overflow — a full per-task ring drops the extra copy silently, where Linux's per-user `RLIMIT_SIGPENDING` makes `sigqueue` return `EAGAIN` | `docs/notes/` §2.3 |
| `socket` | **every raw door asks `CAP_NET_RAW` first** (landed 2026-10-12; the door itself is served as before). Measured on the host with `.workbuddy/probe_rawip.c`, a caller that dropped to an unprivileged id: `SOCK_RAW` with ICMP, with TCP and with a protocol number nothing has are all `EPERM`, and `AF_PACKET` is `EPERM` for both its types — so the capability is asked **before** the protocol is judged, and a caller cannot read the ordering off one refusal. Asked of the caller's *effective set*, like the namespace gates (§15.18). A root caller holds it, so no guest answer moved | `t-caps` (both machines) |

| `uname` | **private protocol on an armed boot only.** A kernel launched with `mpos.diskstat` on its command line — which nothing but `cargo xtask test` ever writes, and only when the sweep stages `t-diskbench` — answers `release` with `0x6d706f7300000001` (`abi::DISKSTAT_OSRELEASE_MAGIC`) and `version` with live per-disk counters as `#1 <name>:<reads>:<writes>,…`. An ordinary boot answers Linux's shape. The word is disclosed here because it is ABI-visible: no real kernel's osrelease is a number, so no caller can collide with it, and there is no other door for these counts (`/proc/partitions` is column-parsed by tools; this kernel has no sysfs attribute store). Pinned by `t-diskbench`'s cold/warm device-read claims (§10.5) | `docs/notes/` §10.5 |
| `mmap` | `MAP_HUGETLB` is `ENOMEM` (an honest refusal); `MAP_HUGETLB` backing is the missing half | §3 below |
| `madvise` | `MADV_FREE` and a shared mapping's cross-process drop are deliberate no-ops | `docs/notes/` §5.2 |
| `posix_fadvise` | recognised advices are accepted as the no-ops they are (this kernel prefetches nothing) | `docs/notes/` §12.1 |
| `vhangup` | with a controlling terminal it answers `EOPNOTSUPP`: the tty re-init + `EIO` hang-up is not implemented | `docs/notes/` §12.2 B4 |
| `getpriority` / `setpriority` / `sched_setattr` | the `nice` round-trips truthfully but carries no scheduling-weight effect | `docs/notes/` §12.2 B5/B5b |
| `setxattr` family | arguments validated, then `EOPNOTSUPP`: no filesystem here stores xattrs | `docs/notes/` R.Remaining |
| `ptrace` | `PTRACE_EVENT_SECCOMP` refused (no tracer stop); `PTRACE_SETSIGINFO` validates then `EOPNOTSUPP` (no siginfo carried through a stop) | `docs/notes/` §8.2, §12.1 A7 |
| `unshare` / `setns` | **MNT served at step 1** — `unshare(CLONE_NEWNS)` copies the caller's mount table (`t-ns-mnt`: child mounts invisible to parent, parent's later mounts invisible to child, umount peels only the copy, and `/proc/mounts` + `/proc/self/mountinfo` render *the caller's* table rather than the machine's); UTS served; privilege is asked **before** the kind (Linux's order), so a caller without `CAP_SYS_ADMIN` hears `EPERM` for *any* namespace word and only a capable one learns that NET (and USER/IPC/CGROUP/TIME) are `EOPNOTSUPP`; `clone(CLONE_NEWNS|CLONE_NEWUTS)` copies that kind into the new child (`t-clone`, `t-ns-mnt`), every other kind refused whole; **`CLONE_NEWPID` is served by all three calls** (`t-ns-pid`), and it is the one kind whose subject is a *number*: `unshare(CLONE_NEWPID)`/`setns(fd, CLONE_NEWPID)` renumber the caller's **children** and not the caller (its `getpid` is unchanged, and `/proc/<its number>` still names it), the first child is pid 1 to itself with `getppid` 0 (its parent stayed outside), and that child is still visible to the parent under the number the parent's own namespace gives it - a task is a member of every namespace from the root down to its own. The other half is what keeps renumbering from being a footgun: **every pid a caller writes and every pid it is handed back is read in the caller's namespace**, so `kill`/`tgkill`/`rt_sigqueueinfo`/`wait4`/`waitid`/`setpgid`/`getpgid`/`getsid`/`setsid`/`ptrace`/`process_vm_readv`/`writev`/the priority, affinity and robust-list pairs/`capget`/`capset`/`timer_create`'s `SIGEV_THREAD_ID`/`TIOCSPGRP`/`TIOCGPGRP` all translate, and a number that names nothing there is `ESRCH` (or `ECHILD` for `wait`) rather than whichever task wears that slot - the difference between a correct program in a namespace and a program that signals the machine's init. `/proc` follows the reader: a `/proc/<n>` directory exists for the numbers *the reader's* namespace gives, `stat`/`status` print those same numbers, and `/proc/<pid>/ns/pid` names the namespace the task **is in** while a `setns` against it moves the one its children go to (Linux's `pidns_get`/`pidns_install` asymmetry). `CLONE_THREAD|CLONE_NEWPID` is `EINVAL` - a thread cannot be the init of a namespace its process is not in. Named gaps: a namespace's pid-1 death does not zap the namespace, nested namespaces are pinned at one level, `/proc/<pid>/status`'s `NSpid` shows the reader's number rather than the chain, and a record lock's `l_pid` is the machine's number; **`setns(fd, nstype)` joins by descriptor** — a mount or UTS namespace from `/proc/<pid>/ns/<kind>` is joined **by reference** (mounts made afterwards are seen by every member of that namespace, and the caller's root moves to the namespace's root; Linux's `mntns_install`), `nstype` must be 0 or exactly the descriptor's kind (a sibling's bit, two bits and a non-namespace word are all `EINVAL`, measured), a descriptor that is not a namespace is `EINVAL` and a number that names nothing is `EBADF`, and `CAP_SYS_ADMIN` is asked **after** those (the same order a host answers in); `/proc/<pid>/ns` lists **only** the kinds this kernel has (mnt, pid, uts), the rest being `ENOENT` exactly as they are on a kernel built without them; the six thread-group flags are Linux's too — `CLONE_FS` and `CLONE_FILES` **split** the caller's own slot (a copy of the filesystem context and of the descriptor table, so a thread that unshared stops sharing its `chdir` and its `close` with its process, where without the unshare both reach it), `CLONE_SYSVSEM` is accepted as the no-op it is with no SysV semaphore undo list here, and `CLONE_VM`/`CLONE_SIGHAND`/`CLONE_THREAD` are `EINVAL` **once a sibling exists** where all six are `0` for a single-threaded caller (measured, one child per flag, `t-unshare-fs`) | `docs/notes/` §6.3, §6.6 |
| `pivot_root` | `chroot(2)` is now per-process — the task carries its own root and the path walk jails to it (`t-chroot`); `pivot_root(2)` and `MS_MOVE` still act on the single global mount table, which *is* Linux's answer with one mount namespace, and only become per-process with `unshare(CLONE_NEWNS)` | `docs/notes/` §6.3 |
| `process_vm_readv` / `writev` | served for the address transfer, and now gated by `ptrace_may_access` — a caller must share a uid with the target (or hold `CAP_SYS_PTRACE`) and the target must be dumpable. Linux's group-id comparison and its LSM hook are not modelled | [05-work-list.md §5.1 item 4](05-work-list.md) |
| `/proc/<pid>/maps` | served; an image's and a private file mapping's line carries no `offset`/`inode`; `smaps`/`numa_maps` are not written | `docs/notes/` §7.4 |

---
