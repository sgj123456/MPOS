# UAPI §1

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md);
section numbers are unchanged from the original `UAPI.md`.

## 1. The syscall table

`src/syscall/linux/nr.rs` selects the machine's table — `nr_x86_64.rs` names **256** x86_64
syscall numbers, `nr_aarch64.rs` the same **256** names under aarch64's numbers (the two tables
are one name set; only the words differ). The dispatch in `src/syscall/linux/mod.rs` has an
explicit arm for **255** of them plus a `_ => ENOSYS` catch-all for the rest, deliberately
crowded and honest (`AGENTS.md` §3.2): a libc probes optional calls by number and reads `ENOSYS`
as "not available here".

### 1.0 Every served call, one row each

The full inventory of the dispatch, so the review question can be asked of the whole surface.
**Owner** is the handler module inside `src/syscall/linux/` (`fs` = `fs/file.rs` + `fs/ioctl.rs`,
`net` = the socket family forwarded to `src/net`, `kernel` = a direct answer in the dispatch);
**Nr x86** / **Nr arm** are the two machines' numbers from the `nr_*` tables. Behaviour claims
live where measured: `docs/notes/` (§1–§12) for the per-family record, §1.1 for refusals, §1.2
for sub-gaps, and [this library's other files](index.md) for errnos, flags and layouts (§2/§3/§4).
A call absent from those is served as its phase section records.

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
| `shmget` | 29 | ipc | 194 |
| `shmat` | 30 | ipc | 196 |
| `shmctl` | 31 | ipc | 195 |
| `dup` | 32 | fs | 23 |
| `dup2` | 33 | fs | 64006 |
| `pause` | 34 | posix | 64033 |
| `nanosleep` | 35 | proc | 101 |
| `mq_open` | 240 | ipc | 6.12 |
| `mq_unlink` | 241 | ipc | 6.12 |
| `mq_timedsend` | 242 | ipc | 6.12 |
| `mq_timedreceive` | 243 | ipc | 6.12 |
| `mq_notify` | 244 | ipc | 6.12 |
| `mq_getsetattr` | 245 | ipc | 6.12 |
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
| `semget` | 64 | ipc | 190 |
| `semop` | 65 | ipc | 193 |
| `semctl` | 66 | ipc | 191 |
| `shmdt` | 67 | ipc | 197 |
| `msgget` | 68 | ipc | 186 |
| `msgsnd` | 69 | ipc | 189 |
| `msgrcv` | 70 | ipc | 188 |
| `msgctl` | 71 | ipc | 187 |
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
| `semtimedop` | 220 | ipc | 192 |
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
| `pidfd_send_signal` | 424 | proc | 424 |
| `pidfd_open` | 434 | proc | 434 |
| `clone3` | 435 | proc | 435 |
| `openat2` | 437 | fs | 437 |
| `pidfd_getfd` | 438 | proc | 438 |
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

Not refusals, not wrong answers: the call is real and one documented corner of Linux's contract
is not. Each is tracked in its phase section; the list is here so it is countable.

| Syscall | Sub-gap | Where |
|---|---|---|
| `rt_sigqueueinfo` / `sigqueue` | real-time signals `32..=64` are fully queued (stage C: every copy delivered FIFO with its own `si_value`), and the queue's bound is Linux's: the quantity is the **target user's** queued total and the limit is the target's `RLIMIT_SIGPENDING`, `sigqueue` answering `EAGAIN` at it where `kill`/`tgkill` drop the copy and answer 0. The storage is the per-task ring, so the resource's *default* is the ring's size rather than Linux's `max_threads / 2`, and raising the limit past the ring buys no room — the one bounded corner, named | `docs/notes/` §2.3 |
| `socket` | **every raw door asks `CAP_NET_RAW` first** (landed 2026-10-12; the door itself is served as before). Measured on the host with `.workbuddy/probe_rawip.c`, a caller that dropped to an unprivileged id: `SOCK_RAW` with ICMP, with TCP and with a protocol number nothing has are all `EPERM`, and `AF_PACKET` is `EPERM` for both its types — so the capability is asked **before** the protocol is judged, and a caller cannot read the ordering off one refusal. **Whose namespace it is asked *of* was corrected on 2026-10-18** (`docs/uapi/` §5.1 item 43): Linux asks `ns_capable(sock_net(sk)->user_ns, CAP_NET_RAW)` — the **network namespace's** owner — of `AF_PACKET` and of `AF_INET`'s `SOCK_RAW` alike, where this kernel asked the caller's own effective set; a caller inside a bare user namespace, holding the whole set *in it*, was served both where Linux refuses both, and is served both here only once it has a network namespace of its own (`.workbuddy/probe_ns_owner.c` for one door, `.workbuddy/probe_rawip_ns.c` for the other, Linux 7.2.9). A root caller is privileged over the machine's own network namespace, so no guest answer moved | `t-caps` (both machines) · `t-ns-net`'s raw-door section (both machines) |
| `mknod` / `mknodat` | **a device node asks `CAP_MKNOD`** (landed 2026-10-17), in Linux's place in the order — after the type, the name, the name's existence (`EEXIST`) and the directory's writability (`EACCES`) — so exactly `S_IFCHR`/`S_IFBLK` ask and only they are refused: `S_IFREG`/`S_IFIFO`/`S_IFSOCK` nodes are served to a caller holding nothing, and so is the character node whose `dev` is `0` (the whiteout overlayfs spells that way, and the exception is the character type's alone). The corner: Linux's `capable(CAP_MKNOD)` is the **initial** user namespace, and this kernel asked the caller's own — so a caller that had entered a user namespace was served here where Linux refuses. **Closed 2026-10-17** (`docs/uapi/` §5.1 item 38): `Credentials::capable` is the initial-namespace question now, and `mknod` is one of the four gates that ask it (`fileattr_set`'s immutable bit, `reboot` and `syslog` are the others), with the three shapes the model still cannot express named in §5.2 (`crates/cred`, §6.3, measured in `.workbuddy/probe_mknod_userns.sh` and `.workbuddy/probe_capable_scope.sh`) | `t-mknod`'s capability section (both machines), `docs/uapi/` §5.2 |

| `uname` | **private protocol on an armed boot only.** A kernel launched with `mpos.diskstat` on its command line — which nothing but `cargo xtask test` ever writes, and only when the sweep stages `t-diskbench` — answers `release` with `0x6d706f7300000001` (`abi::DISKSTAT_OSRELEASE_MAGIC`) and `version` with live per-disk counters as `#1 <name>:<reads>:<writes>,…`. An ordinary boot answers Linux's shape. The word is disclosed here because it is ABI-visible: no real kernel's osrelease is a number, so no caller can collide with it, and there is no other door for these counts (`/proc/partitions` is column-parsed by tools; this kernel has no sysfs attribute store). Pinned by `t-diskbench`'s cold/warm device-read claims (§10.5) | `docs/notes/` §10.5 |
| `mmap` | `MAP_HUGETLB` is `ENOMEM` (an honest refusal); `MAP_HUGETLB` backing is the missing half | §3 below |
| `madvise` | `MADV_FREE` and a shared mapping's cross-process drop are deliberate no-ops | `docs/notes/` §5.2 |
| `posix_fadvise` | recognised advices are accepted as the no-ops they are (this kernel prefetches nothing) | `docs/notes/` §12.1 |
| `vhangup` | with a controlling terminal it answers `EOPNOTSUPP`: the tty re-init + `EIO` hang-up is not implemented | `docs/notes/` §12.2 B4 |
| `getpriority` / `setpriority` / `sched_setattr` | the `nice` round-trips truthfully but carries no scheduling-weight effect | `docs/notes/` §12.2 B5/B5b |
| `setxattr` family | arguments validated, then `EOPNOTSUPP`: no filesystem here stores xattrs | `docs/notes/` R.Remaining |
| `ptrace` | `PTRACE_EVENT_SECCOMP` stop delivered for a `SECCOMP_RET_TRACE` call when the tracer set `PTRACE_O_TRACESECCOMP` (event 7, data half in `GETEVENTMSG`); a `PTRACE_SETREGS` at that stop *or* at an ordinary `PTRACE_SYSCALL` entry stop rewrites the number and arguments the call runs with, the `-1` sentinel included — it *cancels* the call (`-ENOSYS`, no side effect) rather than replacing it, and a filter judges the number the tracer wrote rather than the one the program asked for; a write at the *exit* stop is a no-op (`getpid` → `getppid`, the cancel, and the exit no-op all measured on Linux first); `PTRACE_SETSIGINFO` validates then `EOPNOTSUPP` (no siginfo carried through a stop) | `docs/notes/` §8.2, §12.1 A7 |
| `unshare` / `setns` | **MNT served at step 1** — `unshare(CLONE_NEWNS)` copies the caller's mount table (`t-ns-mnt`: child mounts invisible to parent, parent's later mounts invisible to child, umount peels only the copy, and `/proc/mounts` + `/proc/self/mountinfo` render *the caller's* table rather than the machine's); UTS served; privilege is asked **before** the kind (Linux's order), so a caller without `CAP_SYS_ADMIN` hears `EPERM` for *any* namespace word and only a capable one learns that IPC/CGROUP are `EOPNOTSUPP` (USER, served since 2026-10-16, is the one namespace bit the capability gate does not cover); `clone(CLONE_NEWNS|CLONE_NEWUTS)` copies that kind into the new child (`t-clone`, `t-ns-mnt`), as does `clone` for every other served kind (PID, NET and USER below), while IPC and cgroup are refused whole; **`CLONE_NEWPID` is served by all three calls** (`t-ns-pid`), and it is the one kind whose subject is a *number*: `unshare(CLONE_NEWPID)`/`setns(fd, CLONE_NEWPID)` renumber the caller's **children** and not the caller (its `getpid` is unchanged, and `/proc/<its number>` still names it), the first child is pid 1 to itself with `getppid` 0 (its parent stayed outside), and that child is still visible to the parent under the number the parent's own namespace gives it - a task is a member of every namespace from the root down to its own. The other half is what keeps renumbering from being a footgun: **every pid a caller writes and every pid it is handed back is read in the caller's namespace**, so `kill`/`tgkill`/`rt_sigqueueinfo`/`wait4`/`waitid`/`setpgid`/`getpgid`/`getsid`/`setsid`/`ptrace`/`process_vm_readv`/`writev`/the priority, affinity and robust-list pairs/`capget`/`capset`/`timer_create`'s `SIGEV_THREAD_ID`/`TIOCSPGRP`/`TIOCGPGRP`/`F_GETLK`'s `l_pid` all translate, and a number that names nothing there is `ESRCH` (or `ECHILD` for `wait`) rather than whichever task wears that slot - the difference between a correct program in a namespace and a program that signals the machine's init. **The task a namespace is spoken for by has rules of its own** (`t-ns-pid` holds all of them), and so does the pid namespace's **own creation rule**: its caller's namespace must be the one its children would go to, so a process makes one only **once** - a second `unshare(CLONE_NEWPID)` and a `clone(CLONE_NEWPID)` after one are `EINVAL`, and a *sealed* namespace (its init has exited) answers `ENOMEM` to any task creation, a thread's included (all measured): its own members cannot end it - a sibling's `kill(1, SIGKILL)`, `SIGTERM`, `SIGSEGV` and `SIGBUS` all answer **0** and leave it alive, because the check is on delivery and not on permission - while a signal it has installed a **handler** for is delivered like any other, and a SIGKILL from an **ancestor** namespace lands where a member's could not; and its death takes the namespace with it, SIGKILLing every task it speaks for, nested namespaces included (measured, and `t-ns-pid` holds a nested one: the task two levels down dies too, which is what a membership test reading only "the namespace this task is in" fails). A kernel raise *instead of* an event is a sender like any other here (an `alarm` an init armed itself is discarded), while a **synchronous fault** ends it - the arch's fault handler does that itself when the disposition is default. `/proc` follows the reader: a `/proc/<n>` directory exists for the numbers *the reader's* namespace gives, `stat`/`status` print those same numbers (`Pid:`/`Tgid:`/`PPid:`), while the `NS*` fields carry the **chain** the task is known by - `NStgid`/`NSpid`/`NSpgid`/`NSsid` list it outermost first, so a task one namespace in reads `NSpid:\t4231\t1`, a task two namespaces in reads three - its own namespace's `1` last, the same line from either side (`t-ns-pid`) - and a machine's task reads one number, the same line for every reader (`t-ns-pid`, measured) and `/proc/<pid>/ns/pid` names the namespace the task **is in** while a `setns` against it moves the one its children go to (Linux's `pidns_get`/`pidns_install` asymmetry). **`CLONE_NEWNET` is the fourth kind**: `clone(CLONE_NEWNET)` gives the child a stack of its own - `lo` alone, up and carrying `127.0.0.1/8`, and a port space the machine's cannot see. `t-ns-net` holds the pair: the child binds a listener on its own loopback and connects to it, while the very port it holds is free in the parent's namespace and a connect to it there is refused; `ip link set eth0 up` inside answers `ENODEV` (a namespace has no wire, and this kernel has nowhere to move one), `ip link set eth0 netns N` **moves it**: the device leaves with its name, MTU, MAC and ifindex and arrives **down and unconfigured**, because an address belongs to the namespace rather than to the device - measured on Linux first, and `t-ns-net` walks the whole round trip and the return trip (it is the machine's only NIC, so the pin restores it before it asserts anything), and an `RTM_NEWADDR` naming `lo` answers `EOPNOTSUPP` - which is *why* the namespace's loopback is born addressed rather than born empty as Linux's is. **`unshare(CLONE_NEWNET)` and `setns(fd, CLONE_NEWNET)` are served** - both move the *caller*, Linux's rule for this kind and the opposite of PID's - and **a socket remembers the stack it was made in** (`net`'s handle map), so the consequences are Linux's: a socket bound before the move still answers its own port after it, a child cloned with the flag keeps its inherited descriptors working against their own stacks, and a descriptor handed over by `SCM_RIGHTS` arrives intact across a namespace boundary and answers the *sender's* address (`t-ns-net` holds all three). What is left of this kind is outside the socket table: a vsock stack stays machine-wide - which is Linux's own shape rather than a gap (its port table is machine-global too: the same port is `EADDRINUSE` in a second namespace, while a fresh one binds), so what is named there is a transport question this host cannot ask. `AF_UNIX` is the family the measurement moved the other way: its **abstract** names belong to the namespace of the socket that holds them (one namespace's name is `ECONNREFUSED` in another, the same name binds again there, and it is the *socket's* namespace rather than the calling task's), while a **path** name is not namespaced at all and crosses both ways (`t-ns-net` holds all five claims; `docs/notes/` §6.3). **`CLONE_NEWTIME` is the fifth kind, and its subject is a *clock***: `unshare(CLONE_NEWTIME)` and `setns(fd, CLONE_NEWTIME)` both replace the caller's `time_ns_for_children` and **never move the caller** - its own `/proc/self/ns/time` is unchanged and its own readings never move - so what is armed is the namespace its *children* are made in. The offsets are written through `/proc/<pid>/timens_offsets` (the *children's* slot's file, mode `0644 root:root`, two lines `monotonic <sec> <nsec>` / `boottime <sec> <nsec>` in either order, a partial write leaving the other clock alone, and `EINVAL` for an unknown clock name, a field outside `0..1e9` or an empty write), and they **freeze** the instant anything enters the namespace - `EACCES` from the first task onwards, which is why a caller's order is unshare, write, fork. What the offsets move is the four uptime readings - `MONOTONIC`, `MONOTONIC_RAW` and `MONOTONIC_COARSE` through the monotonic offset, `BOOTTIME` through its own - and **nothing else**: `REALTIME`, `TAI` and the two cputime clocks are untouched, because a certificate's validity must not shift because a namespace wanted an earlier uptime. `clone(CLONE_NEWTIME)` is not a door on any kernel: the bit is `0x80`, entirely inside the low byte `clone(2)` reads as the exit signal, so the number a caller can actually pass (`0x91`) is refused `EINVAL` as a *signal* and not as a namespace (`t-ns-time` holds all of this in 46 claims, and a machine that has never unshared reads two zero lines and refuses the write). A *thread* cannot be given a namespace of its own here, and the word says so instead of dropping the flag: `CLONE_THREAD|CLONE_NEWPID` is `EINVAL` (`clone(2)`'s own ERRORS list calls that pair invalid - "CLONE_NEWPID and one (or both) of CLONE_THREAD or CLONE_PARENT"), and `CLONE_THREAD` with a namespace this kernel *can* give a process (`CLONE_NEWNS`/`CLONE_NEWUTS`) is `EOPNOTSUPP` - Linux serves that pair, `sched::spawn_thread` cannot (a thread copies its process's namespaces by reference), and until this rule the flag was accepted and silently ignored (`t-clone` holds both). Named gaps: nested namespaces are pinned two levels deep for the *numbers* now (`t-ns-pid`, 2026-10-15); **`setns(fd, nstype)` joins by descriptor** — a mount or UTS namespace from `/proc/<pid>/ns/<kind>` is joined **by reference** (mounts made afterwards are seen by every member of that namespace, and the caller's root moves to the namespace's root; Linux's `mntns_install`), `nstype` must be 0 or exactly the descriptor's kind (a sibling's bit, two bits and a non-namespace word are all `EINVAL`, measured), a descriptor that is not a namespace is `EINVAL` and a number that names nothing is `EBADF`, and `CAP_SYS_ADMIN` is asked **after** those (the same order a host answers in); `/proc/<pid>/ns` lists **only** the kinds this kernel has (mnt, net, pid, pid_for_children, time, time_for_children, user, uts), the rest being `ENOENT` exactly as they are on a kernel built without them; the six thread-group flags are Linux's too — `CLONE_FS` and `CLONE_FILES` **split** the caller's own slot (a copy of the filesystem context and of the descriptor table, so a thread that unshared stops sharing its `chdir` and its `close` with its process, where without the unshare both reach it), `CLONE_SYSVSEM` **splits** the caller's undo list, carrying the records it already had (measured: a process that records a `SEM_UNDO` adjustment, unshares and exits still has it applied - `docs/notes/` §6.14), and `CLONE_VM`/`CLONE_SIGHAND`/`CLONE_THREAD` are `EINVAL` **once a sibling exists** where all six are `0` for a single-threaded caller (measured, one child per flag, `t-unshare-fs`). **The user namespace's translation reaches `stat` as well as the `getuid` family** (2026-10-16): `stat`/`fstat`/`fstatat`/`statx` report `st_uid`/`st_gid` as the stored owner mapped through the **reader's** namespace, so a file created before an `unshare(CLONE_NEWUSER)` reads the stored id under the machine map and the inside id under the new one, and `statx` agrees with `stat` (`t-ns-user`). **Entering a user namespace is itself the grant** (2026-10-16): `unshare(CLONE_NEWUSER)` and `setns(fd, CLONE_NEWUSER)` hand the maker the full effective, permitted and **bounding** sets with nothing inheritable - measured across all four doors on Linux, so a caller that clears its capabilities first reads them back the instant it is in a namespace it made, and a `unshare(CLONE_NEWNS)` that needs `CAP_SYS_ADMIN` then succeeds (`t-ns-user`); and `clone(CLONE_NEWUSER)` makes a child born already inside a namespace of its own (the overflow id until its map is written, the full set at birth), a word carrying `CLONE_NEWUSER` lifting the capability gate for the other kinds beside it (`t-ns-user`, §6.3); and a socket's `ucred.pid` is rendered through the **reader's pid namespace** (`pid_vnr`'s rule: a peer inside reads its number there, one in an ancestor reads `0`, `t-ns-pid`); and the **netlink/uevent family's own `ucred`** is Linux's too (2026-10-16): its `uid`/`gid` render through the reading task's user namespace - the kernel's own synthetic zero record included, so a listener that recognises kernel origin by `uid 0` is in the machine namespace - while its `pid` is the *sender's* number in the sender's own pid namespace, resolved once at send, which is also the port id a zero-`nl_pid` bind prefers (`t-uevent`, §6.3) | `docs/notes/` §6.3, §6.6 |
| `pivot_root` | `chroot(2)` is now per-process — the task carries its own root and the path walk jails to it (`t-chroot`); `pivot_root(2)` and `MS_MOVE` still act on the single global mount table, which *is* Linux's answer with one mount namespace, and only become per-process with `unshare(CLONE_NEWNS)` | `docs/notes/` §6.3 |
| `process_vm_readv` / `writev` | served for the address transfer, and now gated by `ptrace_may_access` — a caller must share a uid with the target (or hold `CAP_SYS_PTRACE`) and the target must be dumpable. Linux's group-id comparison and its LSM hook are not modelled | [05-work-list.md §5.1 item 4](05-work-list.md) |
| `/proc/<pid>/maps` | served; an image's and a private file mapping's line carries no `offset`/`inode`; the permission column is the **grant** (`PROT_NONE` is `---p`, where the VMA's own `READ` bit means "present"); `smaps` is served too (each record's header *is* this line; `Pss`/`Pss_Dirty`/`Private_Dirty` are `Rss` and `Shared_*` zero, since no mapcount, dirty bit or page cache exists here); `numa_maps`/`smaps_rollup` are not written | `docs/notes/` §7.4, §R.6; 05-work-list.md §5.1 item 54 |

---
