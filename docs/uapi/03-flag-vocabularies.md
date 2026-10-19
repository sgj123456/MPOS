# UAPI §3

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md).
Section numbers are unchanged from the original `UAPI.md`.

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
| `memfd_create` flags | `fs.rs` | Served: unknown `EINVAL`, `MFD_HUGETLB` `EOPNOTSUPP` (no hugepage backing) |
| `getrandom` flags | `abi.rs` | Served: three `GRND_*` accepted, unknown `EINVAL` |
| `statx` flags | `abi.rs` + `fs.rs` | Served: unknown `EINVAL`, `AT_SYMLINK_NOFOLLOW` honoured |
| `open` `O_NOFOLLOW` | `fs.rs` | Served: `ELOOP` on a symlink final component |
| `mmap` flags | `proc.rs` | Served: `MAP_FIXED_NOREPLACE` → `EEXIST`, conflict `EINVAL`, `MAP_HUGETLB` `ENOMEM`; `MAP_STACK`/`NORESERVE`/`POPULATE` accepted as the no-ops Linux makes them |
| `prctl` options | `abi.rs` + `src/cred.rs` | Served for the name pair, the seccomp pair, `PR_SET/GET_DUMPABLE`, `PR_SET/GET_KEEPCAPS` and `PR_CAPBSET_READ`/`DROP`; every other option is honestly `EINVAL` |
| inode attribute flags (`FS_IOC_SETFLAGS`' word) | `src/vfs/mod.rs` (`INODE_FL_VISIBLE` / `INODE_FL_MODIFIABLE`) | Served for the six this kernel keeps — `IMMUTABLE`, `APPEND`, `NODUMP`, `NOATIME`, `SYNC`, `DIRSYNC` — of which the first two are enforced and the ext4 driver's three (`EXTENT`, `INDEX`, `HUGE_FILE`) are readable and not writable; every other bit `EOPNOTSUPP`, and a refused word is *not stored* |
| the flags a *filesystem* has no attribute word for | `src/vfs/mod.rs` (`Inode::flags` default) | Refused `ENOTTY` on both requests — Linux's answer for a filesystem with no `fileattr` half: procfs, sysfs, devfs, a pipe, a socket |

---

## 3a. The pseudo-filesystem files a program can open

The other half of what userspace reads is not a call but a **file**. Every name below is one the
kernel renders; an absent name is `ENOENT`, which is how a probing caller learns the truth. Owner
is `src/vfs/procfs.rs` / `src/vfs/sysfs.rs` unless stated.

### `/proc` — per-process and system facts

The rendering code is `src/vfs/procfs.rs`; the pins are the programs that read each file back.
A name not listed here is `ENOENT`, which is itself a Linux answer for a kernel built without
that feature — the *named absences* below say which of those this is.

| File | Verdict | Pin |
|---|---|---|
| `/proc/<pid>/maps` | Served in Linux's six-field format; an image's and a private file mapping's line carries no `offset`/`inode`, and `smaps`/`numa_maps` are not written — both divergences named, with their reason | `t-maps`; `docs/notes/` R.6 |
| `/proc/self/cmdline`, `comm`, `status`, `stat`, `statm`, `limits`, `environ`, `oom_score_adj`, `fd/`, `cwd`, `exe`, `root`, `task/` | Served as the renderings `procfs.rs` writes; `environ` landed late because OpenRC mounted over a live `/proc` without it (`docs/notes/` §13.30). The whole set is reachable at a **thread's own `/proc/<tid>`** too, where `stat`'s field 1 and `status`'s `Pid`/`NSpid` are the tid's and `Tgid`/`NStgid` stay the group's (`docs/uapi/` §5.1 item 64) | read by the suite's own process tests (`t-getpid`, `t-set-tid-address`, `t-fd-list`); `t-tid-dir` |
| `/proc/cpuinfo`, `meminfo`, `uptime`, `loadavg`, `version`, `partitions`, `devices`, `filesystems`, `misc`, `modules`, `execdomains`, `kmsg`, `tty/drivers`, `tty/ldisc` | Served as the neutral renderings; `modules` is the empty file a module-less kernel prints (§R.Remaining) | `t-mountinfo` (the shape), `t-syslog` (the kmsg door) |
| `/proc/interrupts`, `softirqs`, `slabinfo`, `vmstat`, `zoneinfo`, `buddyinfo`, `pagetypeinfo`, `cgroups`, `schedstat`, `pressure`, `stat`, `diskstats`, `swaps`, `consoles`, `crypto`, `io_uring` | **Not present** — `ENOENT`. These need counters or engines this machine does not keep (no SMP interrupt accounting, no slab walker, no cgroup engine); refusing by absence is Linux's own answer when a feature is out of the build, and none of these has a caller on either image today | *(no pin — the absence is the claim; if a program ever reads one, it lands with its row)* |
| `/proc/net/route`, `tcp`, `udp` | Served as the static tables this stack keeps; the ones with no rows print their header only. `tcp`/`udp` are the **reader's** namespace's table (`current_net_ns()`): a socket bound in one namespace is a row there and absent from another's | `t-netlink`, `t-ifreq` (the tables), `t-ns-net` (the per-namespace rows) |
| `/proc/net/dev` | Served with live per-interface counters — rx/tx bytes and packets counted at the smoltcp device seam, §10.4's instrument door. Linux's exact column widths, `lo` honestly zeros (loopback never crosses the seam), plus one named divergence: two words appended past the last standard column, `mtu <n> pool=<hits>:<misses>` | `t-netstats` (the counts and the tail word); the renderer's column shape is pinned by procfs's host test |
| `/proc/net/arp`, `sockstat`, `snmp` | **Not present** — `ENOENT`. The row they would carry has no table behind them yet; refusing by absence beats a header-only file that invents a shape (§5's rule) | *(no pin — the absence is the claim)* |
| `/proc/sys/kernel/{hostname,domainname,ostype,osrelease,pid_max,threads-max,ngroups_max,panic}`, `/proc/sys/kernel/random/{poolsize,urandom_min_res_cov}` | Served; `hostname`/`domainname` are writable and read back through `uname` — one fact, two doors (§6.3's UTS) | `t-ns-uts`, `t-uname` |
| `/proc/sys/net/ipv4/*` — `tcp/{tcp_syncookies,tcp_rfc1337}`, `ping_group_range`, `conf/all/{accept_redirects,secure_redirects,accept_source_route,rp_filter}` | Served as the values this stack holds; today every one of them is a read-only stub at Linux's default, so a write lands nowhere — named here rather than hidden, until a knob has a real effect to land in | `t-netlink` (the route half), `t-rawicmp` (the TTL path) |
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
| `/sys/bus/pci/devices/<BDF>/{vendor,device,class,config,resource,driver}` | Served behind `mpos_pci_config`; a machine without the capability builds the tree from the neutral model instead of refusing the filesystem ([05b-small-measured.md §5.2](05b-small-measured.md))) | `t-ahci` (the bound driver), `t-drm` (the card's function) |
| `/sys/bus/usb/devices` | The empty directory a machine with no USB controller registers — `lsusb` prints nothing and exits 0 (§R.Remaining) | `t-mountinfo` |
| `/sys/fs/cgroup/**`, `/sys/kernel/slab`, `/sys/module/**`, `/sys/firmware/**` | Not present — no cgroup engine, no slab accounting, no loadable modules; the absence is the honest state, named rather than stubbed | *(no pin — same rule as above)* |

### `getsockopt(2)` / `setsockopt(2)` options

Owner: `src/net/socket.rs` (`handle_setsockopt` / `handle_getsockopt`, with the unix-family and
uevent branches inside them). The two doors are deliberately asymmetric, and the asymmetry is
measured behaviour rather than an oversight: **`setsockopt` refuses** an option the family does
not model with Linux's `ENOPROTOOPT` — a caller that *sets* something must hear when it did not
take; **`getsockopt` answers zero** for an unmodelled option, matching the permissive read Linux
gives a value that was never set. An unknown *level* is `ENOPROTOOPT` on both doors.

| Level · option | `setsockopt` | `getsockopt` | Pin |
|---|---|---|---|
| `SOL_SOCKET` · `SO_TYPE`, `SO_ERROR` | n/a (read-only) | Served — what `socket()` was told; the pending error, zero here | `t-sockopt`, `t-sockmsg` |
| `SOL_SOCKET` · `SO_REUSEADDR` | Served — applied at `bind`/`listen`, and copied onto an accepted socket as Linux's `inet_csk_clone_lock` does (§4.3.3) | Reads back what is stored | `t-sockopt`, `t-tcp-listen` |
| `SOL_SOCKET` · `SO_KEEPALIVE` | Served — arms smoltcp's probe timer at Linux's default interval | Reads back | *(no pin yet — the option's effect is pinned through `t-tcp-close`; the read-back itself is untested)* |
| `SOL_TCP` · `TCP_NODELAY` | Served — Nagle switched off on the live socket | Reads back | `t-tcp` (the data path), `t-netlat` (the timing it changes) |
| `SOL_IP` · `IP_RECVTTL` | Served (§4.2) | Reads back | `t-rawicmp` |
| `SOL_SOCKET` · `SO_PASSCRED` (unix + uevent sockets) | Served — every read carries the peer's `SCM_CREDENTIALS`; the unix pair's `uid`/`gid` are the sender's **stored** ids mapped through the *reader's* user namespace and its `pid` through the reader's *pid* namespace, while the uevent pair's `uid`/`gid` render through the reader's user namespace too (the kernel's own zero record included) and its `pid` is the *sender's* number in the sender's own namespace, resolved once at send (§6.3) | Reads back | `t-unix-scm`, `t-uevent`, `t-ns-user`, `t-ns-pid` |
| `SOL_SOCKET` · `SO_PEERCRED` (unix) | n/a (read-only) | Served — the peer's maker pid/uid/gid; the `uid`/`gid` are the stored ids mapped through the *reader's* user namespace at the read and the `pid` through the reader's *pid* namespace (`pid_vnr`'s rule, `0` for a peer in an ancestor; §6.3) | `t-unix-scm`, `t-socketpair`, `t-ns-user`, `t-ns-pid` |
| `SOL_SOCKET` · `SO_SNDBUF`, `SO_RCVBUF` | Accepted and stored — **record-only**; [05c-needs-prerequisite.md §5.3](05c-needs-prerequisite.md) names the prerequisite (`SO_REUSEPORT` refuses, `ENOPROTOOPT`) | Reads back the stored word | *(no pin — record-only state has no observable to assert against yet)* |
| `SOL_SOCKET` · `SO_LINGER` | Served — the one eight-byte option (`struct linger`); `{on, 0}` makes the last `close` a **reset** (`abort()`), where `{0, _}` and `{on, >0}` close orderly (§4.3.4) | Reads back the stored pair, eight bytes wide | `t-tcp-listen` (the `ECONNRESET`/`EPIPE` the peer reads), `t-tcp-close` (the shape) |
| `SOL_PACKET` · `PACKET_ADD_MEMBERSHIP`/`DROP_MEMBERSHIP` | Served (§4.5's tap) | as set | `t-dhcp` |
| anything else at `SOL_SOCKET`/`SOL_TCP`/`SOL_IP` | Refused `ENOPROTOOPT` — Linux's answer for an option this stack has no story for (`SO_REUSEPORT` is one: `t-sock-flags` pins the refusal) | reads 0 (the permissive rule above) | `t-sockopt`, `t-sock-flags` |
| an unknown level (`SOL_TLS`, `SOL_XDP`, …) | Refused `ENOPROTOOPT` on both doors | same | `t-sockopt` |
