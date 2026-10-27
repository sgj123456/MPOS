# UAPI §3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md);
section numbers are unchanged from the original `UAPI.md`.

## 3. Flag words and closed vocabularies

`docs/notes/` §12.1 records a swept pass over these, one landing per vocabulary. A flag the
kernel accepts and drops is a promise not kept, and a *number* it accepts and ignores is the
same failure in a different type.

| Vocabulary | Where | Verdict |
|---|---|---|
| `sched_*` flags, `pipe2`, `dup3`, `epoll_create1`, `timerfd_create`, `inotify_init1`, `renameat2`, `openat2`, `preadv2`/`pwritev2`, `splice`, `msync`, `clone`, `signalfd` | `src/syscall/linux/` | Served: unknown bits `EINVAL` |
| `eventfd2` | `src/vfs/eventfd.rs` | Served: unknown bit `EINVAL`, `EFD_CLOEXEC` applied |
| `unlinkat` | `fs.rs` | Served: unknown bit `EINVAL` and the file survives |
| `madvise` advices | `abi::madvise_advice_is_known` | Served: Linux's exact known set, measured |
| `posix_fadvise` advices | `fs.rs` | Served: `0..=5` known, `EINVAL` past, pipe `ESPIPE` |
| `clock_gettime`/`clock_nanosleep` clock ids | `proc.rs` | Served: `{0..=9,11}` accepted, `EINVAL` past |
| timeout `struct timespec`/`timeval` (`nanosleep`, `clock_nanosleep`, `ppoll`, `setitimer`, `timer_settime`) | `abi::timespec_valid` / `timeval_valid` | Served: Linux's `timespec64_valid`/`timeval_valid` — a negative second, a sub-second field outside `[0, 1e9)` (timespec) or `[0, 1e6)` (timeval), or a second past `KTIME_SEC_MAX` is `EINVAL`; a NULL request is `EFAULT`, never a successful zero sleep (`t-sleep-subtick`, `docs/notes/` §10) |
| `read`/`write`/`pread`/`pwrite` counts, and each `readv`/`writev` iovec | `fs/file.rs` (`stream_len`) | Served: a count above the kernel's per-call window is a **short transfer**, never `EINVAL` (`EFAULT` for a bad range is still `EFAULT`) — Linux's `MAX_RW_COUNT` shape with this kernel's one-`MAX_IO` bound; `t-read` writes and drains 2 MiB through the short-count loop |
| `memfd_create` flags | `fs.rs` | Served: unknown `EINVAL`, `MFD_HUGETLB` `EOPNOTSUPP` (no hugepage backing) |
| `getrandom` flags | `abi.rs` | Served: three `GRND_*` accepted, unknown `EINVAL` |
| `statx` flags | `abi.rs` + `fs.rs` | Served: unknown `EINVAL`, `AT_SYMLINK_NOFOLLOW` honoured |
| `open` `O_NOFOLLOW` | `fs.rs` | Served: `ELOOP` on a symlink final component |
| `mmap` flags | `proc.rs` | Served: `MAP_FIXED_NOREPLACE` → `EEXIST`, conflict `EINVAL`, `MAP_HUGETLB` `ENOMEM`; `MAP_STACK`/`NORESERVE`/`POPULATE` accepted as the no-ops Linux makes them |
| `prctl` options | `abi.rs` + `src/cred.rs` | Served for the name pair, the seccomp pair, `PR_SET/GET_DUMPABLE`, `PR_SET/GET_KEEPCAPS`, `PR_CAPBSET_READ`/`DROP` and `PR_SET/GET_PDEATHSIG` (0 clears, `0..=_NSIG` accepted, out-of-range `EINVAL` with the old value kept, a null getter `EFAULT`; **`fork` clears it and `exec` keeps it**, and the exit path delivers it when the named parent dies - `t-prctl`, `docs/notes/` §2.9) `PR_GET_TID_ADDRESS` (the word `clone(CLONE_CHILD_CLEARTID)` names: a `clone` child reads back exactly the `child_tid` its creator passed, a pointer it cannot write is `EFAULT`, arguments past the second are ignored; `t-prctl`, §6.20), `PR_SET_NO_NEW_PRIVS`/`PR_GET_NO_NEW_PRIVS` (the one-way promise: the setter takes exactly `(1, 0, 0, 0)` - `2`, a non-zero later argument and a clearing `0` are `EINVAL` - the getter answers the boolean and refuses a non-zero argument, `fork` inherits it and `exec` keeps it; the seccomp door reads it, so a non-root caller that could only be refused `EACCES` now installs a filter after setting it - `t-prctl`, `t-seccomp`, §6.19), `PR_SET_CHILD_SUBREAPER`/`PR_GET_CHILD_SUBREAPER` (any `int` stores a boolean, a null getter is `EFAULT`, `fork` does not inherit it and `exec` keeps it, and the exit path reparents an orphaned descendant to the marked process - `t-prctl`, §6.18) and `PR_SET_VMA`/`PR_SET_VMA_ANON_NAME` (a mapping's `[anon:<name>]`, with a sub-range *splitting* the mapping, a null pointer clearing it, `EINVAL` for a name of 80 bytes or a misaligned start, `ENOMEM` for an unmapped range and **`EBADF`** for a file-backed one; `t-maps`, §6.17 - this kernel is stricter for a `memfd`-backed range, named there); every other option is honestly `EINVAL` - and three of those are *decisions* with numbers rather than gaps: `PR_SET_TIMERSLACK` (its promise is a slack finer than this kernel's tick, so it could not be honoured), `PR_SET_PTRACER` (Linux's own `EINVAL` for a kernel built without Yama, which this is) and `PR_SET_THP_DISABLE` (nothing reads it: there are no huge pages here) - `docs/notes/` §6.21 |
| pidfd flags | `proc.rs` (`handle_pidfd_open`, `handle_pidfd_getfd`, `handle_pidfd_send_signal`) | Served: `pidfd_open` accepts `O_NONBLOCK` alone — `O_CLOEXEC` is `EINVAL` because `FD_CLOEXEC` is unconditional — `pidfd_getfd`'s `flags` must be **zero** (any other word `EINVAL`), and the send scope is one of `{0, 1, 2, 4}`, every combination being `EINVAL` (3, 5, 6, 7, 8 measured) |
| `clone3` flags | `abi.rs` (`clone3_verdict`) + `proc.rs` | Served: `clone`'s vocabulary (`CLONE_VM`/`FS`/`FILES`/`SIGHAND`/`VFORK`/`PARENT`/`THREAD`/`SYSVSEM`/`SETTLS`/`PARENT_SETTID`/`CHILD_SETTID`/`CHILD_CLEARTID`/`UNTRACED`/the namespace kinds), plus `CLONE_PIDFD`; `CLONE_DETACHED` — `clone(2)`'s historical no-op — is `EINVAL` here as Linux's own `clone3` makes it; `CLONE_CLEAR_SIGHAND` is `EOPNOTSUPP`; `CLONE_INTO_CGROUP` is **served** (item 105) - the `cgroup` word names a cgroup2 *directory* the child is born in (`EBADF` for any other node, `ENOENT` for a removed group or one outside the caller's cgroup namespace, `EACCES` from the common ancestor's `cgroup.procs`), and beside `CLONE_THREAD` it is `EOPNOTSUPP`; `CLONE_PTRACE` is `EINVAL` (this kernel's unknown-bit rule, named in §5.1 item 94). `clone(2)` accepts `CLONE_INTO_CGROUP` and **ignores** it, which is Linux's own answer for a door with no descriptor argument |
| inode attribute flags (`FS_IOC_SETFLAGS`' word) | `src/vfs/mod.rs` (`INODE_FL_VISIBLE` / `INODE_FL_MODIFIABLE`) | Served for the six this kernel keeps — `IMMUTABLE`, `APPEND`, `NODUMP`, `NOATIME`, `SYNC`, `DIRSYNC` — of which the first two are enforced and the ext4 driver's three (`EXTENT`, `INDEX`, `HUGE_FILE`) are readable and not writable; every other bit `EOPNOTSUPP`, and a refused word is *not stored* |
| the flags a *filesystem* has no attribute word for | `src/vfs/mod.rs` (`Inode::flags` default) | Refused `ENOTTY` on both requests — Linux's answer for a filesystem with no `fileattr` half: procfs, sysfs, devfs, a pipe, a socket |

---

## 3a. The pseudo-filesystem files a program can open

The other half of what userspace reads is not a call but a **file**. Every name below is one the
kernel renders; an absent name is `ENOENT`, which is how a probing caller learns the truth.

### `/proc` — per-process and system facts

The rendering code is `src/vfs/procfs.rs`; the pins are the programs that read each file back.
The *named absences* below say which `ENOENT`s are a Linux answer for a kernel built without the
feature.

| File | Verdict | Pin |
|---|---|---|
| `/proc/<pid>/maps` | Served in Linux's six-field format; an image's and a private file mapping's line carries no `offset`/`inode`, and `smaps`/`numa_maps` are not written — both divergences named, with their reason | `t-maps`; `docs/notes/` R.6 |
| `/proc/self/cmdline`, `comm`, `status`, `stat`, `statm`, `limits`, `environ`, `oom_score_adj`, `fd/`, `cwd`, `exe`, `root`, `task/` | Served as the renderings `procfs.rs` writes; `environ` landed late because OpenRC mounted over a live `/proc` without it (`docs/notes/` §13.30). The whole set is reachable at a **thread's own `/proc/<tid>`** too, where `stat`'s field 1 and `status`'s `Pid`/`NSpid` are the tid's and `Tgid`/`NStgid` stay the group's (`docs/uapi/` §5.1 item 64) | read by the suite's own process tests (`t-getpid`, `t-set-tid-address`, `t-fd-list`); `t-tid-dir` |
| `/proc/cpuinfo`, `meminfo`, `uptime`, `loadavg`, `version`, `partitions`, `devices`, `filesystems`, `misc`, `modules`, `execdomains`, `kmsg`, `tty/drivers`, `tty/ldisc` | Served as the neutral renderings; `cpuinfo`'s **shape is the machine's** (x86 `model name` vs ARM64 `CPU implementer`/`CPU part`, read from `MIDR_EL1`); `modules` is the empty file a module-less kernel prints (§R.Remaining) | `t-cpuinfo` (both machines), `t-mountinfo` (the shape), `t-syslog` (the kmsg door) |
| `/proc/interrupts`, `softirqs`, `slabinfo`, `vmstat`, `zoneinfo`, `buddyinfo`, `pagetypeinfo`, `cgroups`, `schedstat`, `pressure`, `stat`, `diskstats`, `swaps`, `consoles`, `crypto`, `io_uring` | **Not present** — `ENOENT`. These need counters or engines this machine does not keep (no SMP interrupt accounting, no slab walker, no cgroup engine); refusing by absence is Linux's own answer when a feature is out of the build, and none of these has a caller on either image today | *(no pin — the absence is the claim; if a program ever reads one, it lands with its row)* |
| `/proc/net/route`, `tcp`, `udp` | Served as the static tables this stack keeps; the ones with no rows print their header only. `tcp`/`udp` are the **reader's** namespace's table (`current_net_ns()`): a socket bound in one namespace is a row there and absent from another's | `t-netlink`, `t-ifreq` (the tables), `t-ns-net` (the per-namespace rows) |
| `/proc/net/dev` | Served with live per-interface counters — rx/tx bytes and packets counted at the smoltcp device seam, §10.4's instrument door. Linux's exact column widths, `lo` honestly zeros (loopback never crosses the seam), plus one named divergence: two words appended past the last standard column, `mtu <n> pool=<hits>:<misses>` | `t-netstats` (the counts and the tail word); the renderer's column shape is pinned by procfs's host test |
| `/proc/net/arp`, `sockstat`, `snmp` | **Not present** — `ENOENT`. The row they would carry has no table behind them yet; refusing by absence beats a header-only file that invents a shape (§5's rule) | *(no pin — the absence is the claim)* |
| `/proc/sys/kernel/{hostname,domainname,ostype,osrelease,pid_max,threads-max,ngroups_max,panic}`, `/proc/sys/kernel/random/{poolsize,urandom_min_res_cov}` | Served; `hostname`/`domainname` are writable and read back through `uname` — one fact, two doors (§6.3's UTS) | `t-ns-uts`, `t-uname` |
| `/proc/sys/net/ipv4/*` — `tcp/{tcp_syncookies,tcp_rfc1337}`, `ping_group_range`, `conf/all/{accept_redirects,secure_redirects,accept_source_route,rp_filter}` | Served as the values this stack holds; today every one is a read-only stub at Linux's default, so a write lands nowhere — named rather than hidden, until a knob has a real effect to land in | `t-netlink` (the route half), `t-rawicmp` (the TTL path) |
| `/proc/sys/fs/{protected_hardlinks,protected_symlinks,file-max}`, `/proc/sys/user/*`, `/proc/sys/vm/*` | The security pair and `file-max` are served; the rest are absent — `ENOENT`, honest until the engine behind them exists | *(no pin beyond the mount-shape test)* `t-mountinfo` |

### `/sys` — the device tree

Owner `src/vfs/sysfs.rs`; the tree is assembled once at boot from what the drivers registered
(§7.5), so a node exists exactly where a probe answered.

| Path | Verdict | Pin |
|---|---|---|
| `/sys/kernel/{hostname,ostype}`, `/sys/power/state` | Served as the neutral renderings | `t-mountinfo` |
| `/sys/devices/**` — the positions `drivers::register_block_disk` and the input/display owners publish (`/devices/virtio/block`, `/devices/ahci/block`), with `uevent` files carrying `SUBSYSTEM=`/`MAJOR=`/`MINOR=` | Served — the position is the driver's own declaration, not the builder's guess (§7.5) | `t-evdev`, `t-drm`, `t-ahci` |
| `/sys/class/input/eventN/{device,capabilities/}`, `/sys/bus/virtio/devices`, `/sys/block/<name>/{dev,size,uevent}`, `/sys/block/<name>/<part>/partition`, `/sys/dev/char/M:m`, `/sys/dev/block/M:m` | Served; the `device` link leads to the *device*, not the class entry (§7.4's correction), and a relative target's `..` resolves by keeping the walk's chain (§7.4) | `t-evdev`, `t-mountinfo` |
| `/sys/class/drm/card0/{device,drm/,uevent}`, `/sys/class/block/vda1/dev` | Served as far as the card's identity goes; the parent chain carries the bus attributes libdrm resolves first (§7.4's update) | `t-drm`, `t-fbdev` |
| `/sys/bus/pci/devices/<BDF>/{vendor,device,class,config,resource,driver}` | Served behind `mpos_pci_config` on both machines — x86_64 through the port pair or the firmware's window, aarch64 through the device tree's ECAM window since 2026-10-19 (§7.6c); a machine without the capability builds the tree from the neutral model instead of refusing the filesystem ([05b-small-measured.md §5.2](05b-small-measured.md)) | `t-ahci` (the bound driver), `t-drm` (the card's function), `t-pci` (both config doors) |
| `/sys/bus/usb/devices` | The empty directory a machine with no USB controller registers — `lsusb` prints nothing and exits 0 (§R.Remaining) | `t-mountinfo` |
| `/sys/fs/cgroup/**` | **Present as a mount, not as `/sys`'s child**: `mount -t cgroup2` serves the control-group tree (`src/vfs/cgroupfs.rs`) — the nine files per group, membership moves, the two removal rules — organizational only, no controllers (`t-cgroupfs`). Nothing creates it under `/sys` on either image; a reader that wants it mounts it, which is also how the tests reach it | `t-cgroupfs` |
| `/sys/kernel/slab`, `/sys/module/**`, `/sys/firmware/**` | Not present — no slab accounting, no loadable modules | *(no pin — same rule as above)* |

### `getsockopt(2)` / `setsockopt(2)` options

Owner: `src/net/socket.rs` (`handle_setsockopt` / `handle_getsockopt`, with the unix-family and
uevent branches inside them). The two doors are deliberately asymmetric, measured behaviour
rather than an oversight: **`setsockopt` refuses** an option the family does not model with
Linux's `ENOPROTOOPT` — a caller that *sets* something must hear when it did not take;
**`getsockopt` answers zero** for an unmodelled option, matching the permissive read Linux gives
a value that was never set. An unknown *level* is `ENOPROTOOPT` on both doors.

| Level · option | `setsockopt` | `getsockopt` | Pin |
|---|---|---|---|
| `SOL_SOCKET` · `SO_TYPE`, `SO_ERROR` | n/a (read-only) | Served — what `socket()` was told; the pending error, zero here | `t-sockopt`, `t-sockmsg` |
| `SOL_SOCKET` · `SO_REUSEADDR` | Served — applied at `bind`/`listen`, and copied onto an accepted socket as Linux's `inet_csk_clone_lock` does (§4.3.3) | Reads back what is stored | `t-sockopt`, `t-tcp-listen` |
| `SOL_SOCKET` · `SO_KEEPALIVE` | Served — arms smoltcp's probe timer at Linux's default interval | Reads back | *(no pin yet — the option's effect is pinned through `t-tcp-close`; the read-back itself is untested)* |
| `SOL_TCP` · `TCP_NODELAY` | Served — Nagle switched off on the live socket | Reads back | `t-tcp` (the data path), `t-netlat` (the timing it changes) |
| `SOL_IP` · `IP_RECVTTL` | Served (§4.2) | Reads back | `t-rawicmp` |
| `SOL_SOCKET` · `SO_PASSCRED` (unix + uevent sockets) | Served — every read carries the peer's `SCM_CREDENTIALS`; the unix pair's `uid`/`gid` are the sender's **stored** ids mapped through the *reader's* user namespace and its `pid` through the reader's *pid* namespace, while the uevent pair's `uid`/`gid` render through the reader's user namespace too (the kernel's own zero record included) and its `pid` is the *sender's* number in the sender's own namespace, resolved once at send (§6.3) | Reads back | `t-unix-scm`, `t-uevent`, `t-ns-user`, `t-ns-pid` |
| `SOL_SOCKET` · `SO_PEERCRED` (unix) | n/a (read-only) | Served — the peer's maker pid/uid/gid; the `uid`/`gid` are the stored ids mapped through the *reader's* user namespace at the read and the `pid` through the reader's *pid* namespace (`pid_vnr`'s rule, `0` for a peer in an ancestor; §6.3) | `t-unix-scm`, `t-socketpair`, `t-ns-user`, `t-ns-pid` |
| `SOL_SOCKET` · `SO_SNDBUF`, `SO_RCVBUF` | Served (`docs/uapi/` §5.1 items 58, 98) — Linux's numbers are stored (ask doubled, per-option floor, sysctl ceiling); `SO_SNDBUF` bounds a datagram's send queue, and `SO_RCVBUF` is a stream's receive window, inherited by the socket a listener accepts, while a datagram's receive ring stays fixed. `SO_REUSEPORT` still refuses, `ENOPROTOOPT` | Reads back Linux's number; a stream's window is half of it (capped at 128 KiB) | `t-sockbuf` (the transformations, the inheritance and the sizing), `t-udpblock` (the queue the send buffer bounds) |
| `SOL_SOCKET` · `SO_LINGER` | Served — the one eight-byte option (`struct linger`); `{on, 0}` makes the last `close` a **reset** (`abort()`), where `{0, _}` and `{on, >0}` close orderly (§4.3.4) | Reads back the stored pair, eight bytes wide | `t-tcp-listen` (the `ECONNRESET`/`EPIPE` the peer reads), `t-tcp-close` (the shape) |
| `SOL_PACKET` · `PACKET_ADD_MEMBERSHIP`/`DROP_MEMBERSHIP` | Served (§4.5's tap) | as set | `t-dhcp` |
| anything else at `SOL_SOCKET`/`SOL_TCP`/`SOL_IP` | Refused `ENOPROTOOPT` — Linux's answer for an option this stack has no story for (`SO_REUSEPORT` is one: `t-sock-flags` pins the refusal) | reads 0 (the permissive rule above) | `t-sockopt`, `t-sock-flags` |
| an unknown level (`SOL_TLS`, `SOL_XDP`, …) | Refused `ENOPROTOOPT` on both doors | same | `t-sockopt` |
