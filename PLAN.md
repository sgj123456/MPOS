
# MPOS Feature Roadmap

Missing essential features, prioritized by impact on real-world workload compatibility.

---

## Current Status

- **~229 Linux x86_64 syscalls** implemented
- **Uniprocessor** — single core, `critical_section::Mutex<RefCell<T>>`
- **Signals** — handlers delivered via `rt_sigframe`, mask enforcement, `sigqueue` siginfo payloads
- **Filesystems**: ramfs, ext2/ext4 (write-through), devfs, procfs, sysfs, devpts
- **Networking**: smoltcp TCP/UDP/ICMP/ARP over virtio-net; AF_INET + AF_UNIX
  (path `SOCK_STREAM`) + netlink (read + uevent)
- **Scheduler**: 4-level priority + round-robin, COW fork, demand-paged stack
- **Network latency**: event-driven socket waits + virtio-net MSI-X interrupts;
  loopback `ping 127.0.0.1` 7.5 ms → 0.3 ms (10.1 done)
- **Phases 1–3 complete**; 5.3 / 6.1 / 6.2 / 6.4 done; 4.1 / 4.2 / 4.4 / 5.1 done; 4.3 / 5.2 partial; 6.3 stub.

### Remaining ABI support (gaps + services)

Syscalls the dispatch still falls through to `ENOSYS` (each prints once via
`note_enosys`), grouped by what they block. These, plus the four foundational
behaviours below, are the whole of what is left to reach "full Linux ABI".

**File-watch / service infrastructure (the big one for daemons):**
- `inotify_init1` (294) / `inotify_add_watch` (254) / `inotify_rm_watch` (255) —
  **answered**: a full watch queue in `src/vfs/inotify.rs` (event records,
  the `(dev, ino)` registry, the `IN_ISDIR`/oneshot/`IN_EXCL_UNLINK`/
  `IN_MASK_ADD`/`IN_MASK_CREATE` rules, the rename cookie pair, DELETE_SELF
  and IGNORED, one coalesced Q_OVERFLOW per drop episode), emitting from the
  create/delete/rename/link/open/close/write/attrib hooks of the syscall
  layer. `mdev`/`udev`, `dbus` and config re-readers can watch and hear.
  Deliberate v1 divergences: `IN_ACCESS` is never emitted (the read path
  does not bump atime), `IN_UNMOUNT` is not emitted (unmount does not track
  watches), a directory's `IN_MOVE_SELF` waits on directory rename, and a
  rename that replaces a name reports the `IN_DELETE` this kernel's copy
  actually performs.
- `fanotify_init` (302) / `fanotify_mark` (303) — less common (antivirus, some
  mount monitors).
- `keyctl` (250) — kernel keyring; needed by `keyutils`/some TLS/pam stacks.

**Other missing (lower priority):**
- `userfaultfd` (323) — lazy-copy / CRIU-style migration; niche.
- `tid_add_object` / `tgid_add_object` (rseq side-objects) — rare.
- Extended attributes (`getxattr`/`setxattr`/`listxattr`/`removexattr` + `l`/`f`
  forms, 188–199) — **answered**: arguments and target are validated (so a bad
  path is still `ENOENT`/`EFAULT`, a bad fd `EBADF`), then `EOPNOTSUPP`, because
  no filesystem here stores xattrs. Wiring them stops the `ENOSYS` fall-through
  that `fastfetch`/probing callers reacted to; a real xattr store remains future.

**Foundational behaviours (implemented syscalls, incomplete semantics):**
- **4.3** socket options: `TCP_NODELAY` and `SO_KEEPALIVE` now reach the live
  smoltcp socket (Nagle off; probe timer armed at Linux's default interval).
  `getsockopt` reads back every stored option. Still record-only (no stack
  effect): `SO_REUSEADDR` (moot with one binder), buffer sizes, `SO_LINGER`.
- **4.4** netlink is read-only: `ip addr add` / `ip route add` / `ip link set`
  get an ACK but do not actually reconfigure the stack (RTM_NEWADDR/NEWROUTE/
  NEWLINK write side).
- **5.1 / 5.2** `MAP_SHARED|MAP_ANONYMOUS` is served but aliased frames do not
  cross a plain `fork`, and `MADV_DONTNEED` does not free pages — both need
  anonymous **demand paging** + per-VMA backing tracking in the `#PF` handler.
- **6.3** namespaces: `unshare`/`setns` validate but return `EOPNOTSUPP`; no
  real mount/pid/net namespace isolation.
- **4.1** AF_UNIX is answered for `SOCK_STREAM` (see Phase 4), including
  `SCM_RIGHTS` descriptor handover and `SCM_CREDENTIALS`/`SO_PEERCRED` peer
  authentication — a real dbus broker can now both connect its clients and
  name them. What the family still lacks is `SOCK_DGRAM` datagram transfer.

---

## Phase 1: Syscall Completion — **DONE**

The POSIX completion set landed: epoll, timerfd, the `*at` relatives and
modern file forms, the identity/capability family, scheduling, memory
advice, and the machine-level calls — every one implemented per its man
page, with flag values, struct layouts and errnos pinned by host-side
tests. What remains of the phase is listed at the end.

### 1.1 epoll (Critical) — implemented in `src/vfs/epoll.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `epoll_create` | 213 | Done (size ignored, must be positive) |
| `epoll_create1` | 291 | Done (EPOLL_CLOEXEC) |
| `epoll_ctl` | 233 | Done (ADD/MOD/DEL, EXCLUSIVE rules, ELOOP nesting ≤ 5) |
| `epoll_wait` | 232 | Done (ms timeout; -1 forever, 0 immediate) |
| `epoll_pwait` | 281 | Done (sigmask validated) |
| `epoll_pwait2` | 441 | Done (timespec timeout, sigsetsize == 8) |

Beyond the plan: level and edge trigger (`EPOLLET` with dip tracking),
one-shot (`EPOLLONESHOT` with MOD re-arm), `EPOLLEXCLUSIVE` vocabulary,
`ELOOP` cycle detection for epoll-on-epoll, `EPERM` for regular files,
close-time entry reaping, and readiness from the same `poll_ready` the
poll(2) family consults — so a wait hears about data the instant it
arrives.

### 1.2 timerfd (High) — implemented in `src/vfs/timerfd.rs`

| Syscall | Nr | Status |
|---------|-----|--------|
| `timerfd_create` | 283 | Done (5 clockids, TFD_NONBLOCK/CLOEXEC) |
| `timerfd_settime` | 286 | Done (ABSTIME, CANCEL_ON_SET, old_value) |
| `timerfd_gettime` | 287 | Done (remaining + interval as itimerspec) |

Beyond the plan: an O(1) interval expiry (division, not a loop), a
saturating expiration counter, and the `TFD_TIMER_CANCEL_ON_SET`
registry the `clock_settime` handler walks — the read answers `ECANCELED`
until re-arm.

### 1.3 Other Missing Syscalls — done, 67 of 68

| Syscall | Nr | Status | Notes |
|---------|-----|--------|-------|
| `copy_file_range` | 326 | Done | EXDEV across mounts, in-out offsets, overlap EINVAL |
| `preadv2` / `pwritev2` | 327/328 | Done | RWF_APPEND honoured, -1 = fd cursor, unknown flag EOPNOTSUPP |
| `fallocate` | 285 | Done | mode 0 extends for real; KEEP_SIZE accepted; interior ops EOPNOTSUPP |
| `openat2` | 437 | Done | open_how by size, mode-without-O_CREAT EINVAL, RESOLVE_* EOPNOTSUPP |
| `faccessat2` | 439 | Done | X_OK from stored mode bits, NOFOLLOW honoured |
| `creat`, `utime`, `fchmodat`, `mknodat`, `futimesat`, `readahead`, `splice`, `syncfs`, `syslog` | — | Done | `src/syscall/linux/fs.rs` |
| identity (setreuid…setfsgid, capget/capset), scheduling (sched_setparam…sched_getattr, getpriority/setpriority), memory (msync, mincore, mlock family), machine (personality, vhangup, getcpu, settimeofday, clock_settime, unshare, setns, seccomp, setdomainname), signals (tgkill, rt_sigqueueinfo, rt_sigsuspend, pause) | — | Done | `src/syscall/linux/posix.rs` |
| `accept4`, `sendmmsg`, `recvmmsg` | 288/307/299 | Done | `src/net/socket.rs` |
| `rt_sigpending` | 127 | Done | `src/syscall/linux/posix.rs`; reads `current_pending()` |
| `io_uring_setup/enter/register` | 425-427 | Deferred | per the original phase's own call |


---

## Phase 2: Signal Delivery (High Priority) — **DONE**

Signals are now delivered to installed handlers, not just recorded. A
signal that a process catches builds an `rt_sigframe` on the user stack,
redirects the return into the handler, and resumes the interrupted code
when the handler returns through `rt_sigreturn`.

### 2.1 Signal Frame Delivery — implemented in `src/syscall/linux/proc.rs`

`build_frame` writes the x86_64 `rt_sigframe` (restorer, `sigcontext`,
`siginfo_t`, saved mask), points `RIP`/`RSP`/`RDI`/`RSI`/`RDX` at the
handler and its three arguments, and honours `SA_ONSTACK`, `SA_RESTORER`
and `SA_RESETHAND`. `deliver_signal` runs on every syscall return
(`src/syscall/mod.rs`) and, for `SIG_DFL`, consults the `default_action`
table (terminate / stop / continue / ignore). `handle_rt_sigreturn` restores
the whole register file and the blocked mask.

### 2.2 Signal Mask Enforcement — implemented

`rt_sigprocmask` records the mask and delivery skips blocked signals
(`take_deliverable_siginfo`, `deliverable`), which stay pending until
unblocked; the unblocking syscall's own return is what runs them.
`rt_sigreturn` puts the pre-handler mask back. `rt_sigpending` answers the
pending set independently of the blocked set.

### 2.3 Signal Queue / siginfo — implemented

Each pending signal carries a `SigInfo` record (index into a
`[SigInfo; SIGNALS]` slot table on the task — correct for the non-queued
standard signals 1..=31 this kernel names). `kill`/`tgkill` deliver
`SI_USER` with the sender's pid/uid; `sigqueue`/`rt_sigqueueinfo` deliver
`SI_QUEUE` with the caller's `si_value` word through to the handler; the
`setitimer` clock delivers `SI_TIMER`. `build_frame` writes the real record
(`si_signo`/`si_errno`/`si_code`/`si_pid`/`si_uid`/`si_value`) instead of a
fixed `SI_USER`.

*Not done (future, real-time signals):* numbers 32..=64 and true multi-copy
queuing of the same signal need 64-bit masks throughout; the current
`Pending = u32` vocabulary and `SIGNALS = 32` table bound the signal set.

### 2.4 Verification

`user/abitest/src/bin/t-signal-delivery.rs` runs a handler end-to-end in
QEMU: it asserts the handler fires once and the interrupted `kill` resumes
with its own return (2.1); that a blocked signal is held, reported pending
by `rt_sigpending`, and delivered on unblock (2.2); and that a `sigqueue`
payload arrives as `SI_QUEUE` with its `si_value` intact (2.3). The host
side pins the `si_code` values and the siginfo table.

---

## Phase 3: Filesystem Enhancements (Medium Priority)

### 3.1 Hard Links — **DONE**

`linkat` creates a real second name (ext4 adds a dirent and bumps `i_links_count`;
ramfs shares the `Arc<dyn Inode>`), `unlink` decrements and frees only at zero,
and `stat`/`statx` now report the true `st_nlink` (ext4 via `Inode::nlink()`,
other kinds falling back to the `.`/`..` convention).

**Changes**:
- Add `link_count()` and `add_link()` / `rm_link()` to `Inode` trait
- ext2/ext4: implement `i_links_count` in on-disk inode, decrement on unlink, free when zero
- ramfs: share `Arc<dyn Inode>` across directory entries
- Test: `ln file1 file2 && echo test >> file2 && cat file1` shows same content

### 3.2 File Locking (fcntl F_SETLK) — **DONE**

POSIX record locks are real: a global table keyed by `(dev, ino)`, each entry
`{owner (tgid), class, kind, [start,end)}`, in `src/vfs/locks.rs` (the range
math, conflict rule and split-on-relock are host-tested). Both lock families live
in that one table but never see each other - a `class` of `Record` (`fcntl`
`F_SETLK`/`F_GETLK`/`F_SETLKW`) and `Whole` (`flock`) conflict only within the
class, as Linux defines the two to be independent. `F_SETLK` takes or drops a
byte range and refuses a blocked request with `EAGAIN`; `F_GETLK` reports the
first conflicting lock (its kind, range and holder pid) or clears to `F_UNLCK`;
`F_SETLKW` blocks and wakes on release. `flock(LOCK_SH|LOCK_EX|LOCK_UN)` covers
the whole file (`[0, infinity)`), shared/exclusive arbitration identical to the
record locks, and blocks unless `LOCK_NB`. A holder that dies is pruned on the
next grant/query, and closing a descriptor drops that process's locks on the
file.

*Remaining:* `read`/`write` do not yet enforce against held locks; and `flock`
is tracked per-process rather than per-open-file-description, so two independent
`open`s in one process that Linux would let coexist are arbitrated as one here.

**Changes**:
- Add `locks: Vec<FileLock>` to `File` or per-inode lock table
- `F_SETLK`: record lock (F_RDLCK or F_WRLCK) for (pid, fd, offset, len)
- `F_GETLK`: scan for conflicts, return `F_UNLCK` if none
- `F_SETLKW`: block until lock available
- `read`/`write`: check for conflicting locks (F_WRLCK blocks reads, F_RDLCK blocks writes)
- `close`: release all locks for the closing fd

### 3.3 File Timestamps — **DONE** (ext4; ramfs legacy calls remain)

`utimensat`/`futimens` now store the access and modification clocks on ext4
(`Inode::set_times`), stamping `ctime` to the current time as the metadata
change it is; `stat`/`statx` report the three clocks separately (ext4 keeps
`i_atime`/`i_mtime`/`i_ctime` on disk) instead of collapsing them to mtime.
`open`/`write`/`truncate` bump mtime/ctime; a fresh inode is created with all
three set.

*Remaining:* `atime` is not bumped on read (ext4 carries it but no read path
updates it). ramfs/tmpfs now keeps one clock - stamped at creation and by
writes, truncates and directory-entry changes - which is what stopped
OpenRC's deptree freshness check from reading every `/run` file as 1970.
The legacy `utime` and `futimesat` now store
through the same `Inode::set_times` path.

**Changes**:
- Add `atime`, `mtime`, `ctime` fields to `Inode` trait (or a `Metadata` struct)
- ext2/ext4: store in `i_atime`, `i_mtime`, `i_ctime` on disk
- ramfs: store in `RamInode` behind mutex
- `stat` returns real timestamps
- `open`/`write`/`read`/`mkdir`/`unlink` update mtime/ctime

### 3.4 Permission and Ownership Model — **DONE** (fully Linux-aligned)

`chmod`/`chown` store real values and every access path enforces Linux's model:
- ext4 persists `i_uid`/`i_gid`/`i_mode` (read into `Inode::mode`/`owner`, written by `set_owner`/`set_mode`); ramfs keeps them in memory.
- `stat`/`fstat` return the stored uid/gid/mode.
- `open()`/`truncate` and the create/remove/`link`/`rename` paths call `vfs::may_access` → `may_as(current_owner, ..)`: root (euid 0) gets Linux's `CAP_DAC_OVERRIDE` — every check passes **except** execute on a non-directory with no x-bit for anyone (so even root gets `EACCES` running a `0644` file, and `access(X_OK)` on one is `EACCES`); otherwise the owner/group/other `rwx` triads gate `R_OK`/`W_OK`/`X_OK`, with the group triad matched against the effective gid **and the supplementary groups**, exactly like Linux.
- Directory **search** (`may_search`, `X_OK` on each path component) is enforced during resolution (`mount.rs`, `open`), not just on the final node.
- The sticky bit gates name removal; `access`/`faccessat` use real ids unless `AT_EACCESS`; `setuid`/`setgid` execution and `chmod`'s clearing of those bits by a non-owner follow Linux.
- A filesystem that keeps no modes (`keeps_modes() == false`) is not policed, so invented permissions never refuse.
- Host-tested: `a_triad_decides_who_may_do_what` (incl. supplementary groups), `root_passes_everything_but_execute_on_a_file_nobody_made_runnable`, `may_as_answers_for_the_node_and_the_identity_it_is_given`.

### 3.5 /proc Enhancements — **DONE**

procfs also carries the top-level `uptime`, `loadavg`, `cpuinfo`, `version`,
`filesystems`, `partitions`, `devices`, `stat` and the `/proc/sys` knob tree
(`kernel.ostype/osrelease/hostname/pid_max/...`), per-pid `statm` and `limits`,
and `/proc/net/tcp` + `/proc/net/udp` - the live socket table `ss`/`netstat`
read. The `/proc/net` rows come from `net::collect_net_sockets` walking the
registry under the stack lock (local/remote endpoints, the numeric TCP state,
inode) and a pure `format_net` renders Linux's exact column layout (address as
little-endian hex, `:port`, state byte) - the formatter is host-tested
(`the_net_table_prints_addresses_and_state_as_linux_does`).

---

## Phase 4: Networking Enhancements (Medium Priority)

### 4.1 AF_UNIX Domain Sockets — **ANSWERED** (SOCK_STREAM)

The path family is real: `src/net/unix.rs` carries `socket(AF_UNIX,
SOCK_STREAM)` through `bind` (which creates a genuine `S_IFSOCK` node in
ramfs, visible to `stat`/`ls -l`/`find`, and `open` on which answers
`ENXIO`), `listen` with a clamped backlog, `connect` by resolved path
(symlinks and mounts ride the normal resolution), `accept`/`accept4`,
`getsockname` echoing the bound path, `shutdown`, and the full data path -
a connected unix socket *is* a `pipe::DuplexEnd` half, so `read`/`write`/
`sendmsg`/`recvmsg`/`poll`/`epoll` all speak to it unchanged. The bind
table is keyed by `(fs_id, ino)` like locks and inotify watches, so
`unlink` of the name retires the binding (hooked in `unlinkat`) while the
listener itself survives - exactly how a daemon outlives its own stale
socket file. Permissions: bind needs write+search on the parent (the
mkdirat rule), connect needs `W_OK` on the node.

Deliberate divergences from Linux (each asserted in `t-unix`/
`t-unix-rustix`): the abstract namespace answers `EINVAL`; a connect to a
bound-but-never-listened name is `ECONNREFUSED` rather than Linux's
implicit backlog-0 rendezvous; ext4 refuses socket nodes (`EOPNOTSUPP` -
`/run` is ramfs); `mknod(S_IFSOCK)` still refuses, so only `bind` creates a
socket name; `getpeername` answers `AF_UNIX` with an empty path (accepted
peers carry no name). `SOCK_DGRAM` unix sockets now *open* - musl's
`if_nametoindex` asks for one as the handle for its own `SIOCGIFINDEX`
ioctl, and a refusal turned every device-name lookup into "can't find
device" - but the datagram transfer itself is the documented v1 gap.

**`SCM_RIGHTS` / `SCM_CREDENTIALS` / `SO_PEERCRED` are now answered** (see
`t-unix-scm`): a unix (or socketpair) `sendmsg` can hand live descriptors
across, `recvmsg` installs them into the reader's table (respecting
`RLIMIT_NOFILE`, and `MSG_CMSG_CLOEXEC`), `SO_PASSCRED` makes every read
carry an `SCM_CREDENTIALS`, and `getsockopt(SO_PEERCRED)` returns the peer's
pid/uid/gid - the identity a dbus broker authenticates on. The handover rides
the connection's `PipeState` beside the bytes it travels with, so a descriptor
never taken is closed when the queue drops, and the peer's credentials are
captured at `connect`/`accept`/`socketpair`. v1 divergences: on `SOCK_STREAM`
the kernel keeps one cmsg boundary per send rather than per skb (a data-less
`sendmsg`'s handover is delivered on the next read), a sender's
`SCM_CREDENTIALS` opt-in is accepted but the kernel always fills the peer's
real ids (no impersonation), and `SO_PEERCRED`/`SO_PASSCRED` are the unix
`connect` path's - a `socketpair` carries descriptors but not credentials.

### 4.2 ICMP Improvements — **DONE**

`socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` is served by a raw smoltcp socket
(`alloc_socket` → `add_raw` + `ip_to_smoltcp_protocol`), giving `ping` its echo
request/reply path; the `t-udp` resolver test exercises the raw/UDP read path.
The socket now speaks Linux's exact shape both ways: `sendto` takes the
protocol payload and the kernel builds the IPv4 header (no `IP_HDRINCL`
needed), and receives deliver the **complete IP packet** - the earlier
header-stripped delivery is what made busybox's `ping` print a ttl wandering
0..255 (it reads `iphdr->ttl` at offset 8, which fell inside the echo data).
`IP_RECVTTL` is honoured too (setsockopt stored, the answer attached as a
24-byte cmsg, and `msg_controllen` zeroed when a message carries no control
data). `t-rawicmp` pins the delivered bytes; `t-dns`/`t-stdresolve` pin the
resolver's UDP path (12/12 raw queries, 20/20 musl `getaddrinfo`).

Current: auto echo reply only. No user-facing ICMP socket.

**Changes**:
- `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` creates raw ICMP socket
- `sendto`/`recvfrom` on raw ICMP socket
- Enable `ping` command to work

### 4.3 TCP Socket Options — **PARTIAL** (stored + reported; not yet honored)

`setsockopt`/`getsockopt` are no longer a no-op: each socket's registry `Entry`
carries a `SockOpts`, and the common options round-trip with the correct ABI —
`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_SNDBUF`, `SO_RCVBUF`, `TCP_NODELAY` (`SOL_TCP`)
as 4-byte ints, `SO_LINGER` as an 8-byte `struct linger` (in-out length honoured),
plus `SO_TYPE`/`SO_ERROR`. `SO_RCVBUF`/`SO_SNDBUF` report the stack's default
(64 KiB) until changed; unknown options are accepted and read back as 0.

*Remaining (real effect, into smoltcp):* `SO_REUSEADDR` not yet applied at
`bind`/`listen` (no TIME_WAIT rebind allowance), `TCP_NODELAY` not wired to
disable Nagle coalescing, `SO_KEEPALIVE` not issuing probe packets, and
`SO_LINGER` not honoured at `close`. The flags are stored and queried but do not
yet change packet behaviour.

### 4.4 Netlink Write Operations — **DONE**

The write side is real: `ip addr add/del`, `ip route add/del/replace`,
`ip link set eth0 up|down|mtu N|name X` are parsed (`ifaddrmsg`/`rtmsg`/
`ifinfomsg` + `rtattr` walks) and applied to the live stack - smoltcp's own
address and route tables, so a route added is a route the packet path
follows. `ip link set eth0 down` stops the interface from being polled:
frames neither leave nor arrive, and a `ping` through it loses 100% until
`up` restores it. Dumps read the same live state, so what was written is
what `ip addr`/`ip route`/`ip link` print - with the Linux shapes filled in
(`IFLA_BROADCAST`, `IFLA_MTU`, `IFLA_QDISC "noqueue"`, `IFLA_TXQLEN`,
`IFA_F_PERMANENT`, secondary-address labels `eth0:1`).

Errnos match Linux: `EEXIST` on a duplicate, `EADDRNOTAVAIL`/`ESRCH` on a
delete of something absent, `ENODEV` on an unknown index, `EAFNOSUPPORT`
off-family, `ENOBUFS` when the fixed route table (four entries) is full.
The error rides back in `nlmsgerr.error` **negated** - the kernel's own
convention; writing it positive made busybox print the message for an errno
no table knows ("No error information") instead of "File exists".

Deliberate v1 divergences (each asserted in `t-netlink`): an exact-
duplicate `ip addr add` succeeds instead of `EEXIST` (the boot's own
idempotent re-configuration stays quiet); a route with no gateway is
`EINVAL` (smoltcp delivers the interface's own subnets implicitly, so an
on-link prefix has no second expression); `RTM_DELLINK` and `ip link add`
are `EOPNOTSUPP` (no dynamic interfaces); `lo`'s address and MTU are fixed
(flags and read-back work, the stack has no loopback device behind them).
The `ip link set` flags path also works through the **ioctls** busybox
reaches it by - `SIOCGIFFLAGS`/`SIOCSIFFLAGS`/`SIOCSIFMTU`/`SIOCGIFINDEX`
(the one musl's `if_nametoindex` needs), `SIOCGIFMTU`, `SIOCGIFTXQLEN`/
`SIOCGIFHWADDR` (whose numbers 0x8942/0x8927 had been swapped, answering
the MAC for a txqueue-length request). Two helper gaps this opened: AF_UNIX
`SOCK_DGRAM` now *opens* (musl's `if_nametoindex` asks for it as the bare
handle for its own `SIOCGIFINDEX`), with the datagram transfer still the
documented gap; and `IFLA_TXQLEN`/`IFLA_MTU` are native-order words, the
endianness addresses are not.

**Changes**:
- `RTM_NEWADDR`/`RTM_DELADDR`: `ip addr add`/`del`, applied to the stack
- `RTM_NEWROUTE`/`RTM_DELROUTE`: `ip route add`/`del`/`replace`
- `RTM_NEWLINK`/`RTM_SETLINK`: link up/down (a real poll gate), MTU, name
- `ip addr`/`ip route`/`ip link` dumps read the live tables, not constants
- The `ifreq` ioctl set `ip link set` and `if_nametoindex` ride on

---

## Phase 5: Memory Management Enhancements (Medium Priority)

### 5.1 Shared Anonymous mmap — **DONE** (served; single-space caveat)

`mmap(MAP_SHARED | MAP_ANONYMOUS)` no longer returns `ENOSYS` - it is served as
an anonymous mapping of the process's memory. Threads sharing one address space
(`CLONE_VM`) genuinely see each other's writes through it, which is what modern
code uses shared-anonymous mappings for. A plain `fork`ed child sees a copy
(the pages are not aliased through a shared backing object); true parent/child
shared frames would need an anonymous `Inode` shared across mappings - the one
nuance this kernel does not model.

### 5.2 madvise — **PARTIAL** (validated; page-release blocked on demand paging)

`madvise` is no longer a blanket `0`: it answers `EINVAL` for a range whose
start is not page-aligned (Linux's rule), and recognises the `MADV_*` advisories
as successful no-ops.

*Blocked:* `MADV_DONTNEED` does **not** yet free pages, because this kernel maps
regions eagerly and the page-fault handler grows only the stack - there is no
anonymous demand-paging to re-populate a dropped page, so freeing one would make
the next access fault into a SIGSEGV. Real `MADV_DONTNEED` (and the read-ahead
hints) first need general anonymous on-demand paging in the `#PF` handler.

### 5.3 Process_vm_readv / Process_vm_writev — **DONE**

`process_vm_readv(310)` and `process_vm_writev(311)` are implemented in
`proc.rs` (`handle_process_vm_readv`/`_writev`), walking two iovec arrays in
step to move bytes between the caller and a remote task's address space.
Used by debuggers and checkpoint/restore tools.

---

## Phase 6: Process Management Enhancements (Low-Medium Priority)

### 6.1 ptrace (Debugger Support) — **DONE**

`handle_ptrace` (proc.rs) implements the debugger set: `TRACEME`, `ATTACH`,
`DETACH`, `CONT`, `SYSCALL`, `SINGLESTEP`, `KILL`, `INTERRUPT`,
`PEEK`/`POKE` TEXT/DATA/USER, `GET`/`SETREGS`, `GET`/`SETREGSET`,
`GET`/`SETFPREGS`, `GET`/`SETSIGINFO`, `SETOPTIONS`, `GETEVENTMSG`, wired to the
task's `Tracing` state and the `waitid` stop reports. Required for gdb/strace.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `ptrace` | 101 | Process trace |

**Required for**: gdb, strace, ltrace

**Implementation plan**:
- `PTRACE_TRACEME`: child sets `traced = true`, sends `SIGSTOP` to self
- `PTRACE_ATTACH`/`PTRACE_SEIZE`: attach to target process
- `PTRACE_PEEKDATA`/`PTRACE_POKEDATA`: read/write target memory
- `PTRACE_GETREGS`/`PTRACE_SETREGS`: read/write target registers
- `PTRACE_SINGLESTEP`: set TF (trap flag) in RFLAGS, deliver `SIGTRAP` on next instruction
- `PTRACE_CONT`: resume execution, optionally delivering a signal
- On any stop, target enters `TASK_TRACED` state and parent is notified via `waitid`

### 6.2 Process Resource Limits (Real) — **DONE**

`getrlimit`/`setrlimit`/`prlimit64` now operate on a real per-process table
rather than returning constants:
- A `[Rlimit; RLIM_NLIMITS]` table is stored on `Task`, seeded by
  `default_rlimits()` (8 MiB stack, 1024/1M `NOFILE`, 4096 `NPROC`, infinity
  elsewhere) and **inherited whole across fork and clone** (exec leaves it).
- `setrlimit`'s rule is real: `rlimit_admit` rejects soft-above-hard with
  `EINVAL` and an unprivileged raise of the hard ceiling with `EPERM` (root may);
  the accepted pair is stored. `RLIM_INFINITY` is `u64::MAX`.
- `getrlimit`/`prlimit64(pid 0)` read the caller's own table; `prlimit64` on
  another live pid can read it (else `ESRCH`) but not write it (`EPERM`).
- `/proc/<pid>/limits` renders the table in the `ulimit` layout.
- The pure rule + defaults are host-tested (`setrlimit_admits_by_privilege`).

*Both remaining resources are now enforced:*
- **`RLIMIT_NOFILE`**: the descriptor table carries the soft ceiling (seeded 1024,
  synced from `setrlimit(5)`, copied on fork); `install`/`install_at_least`
  refuse a descriptor at or above it with `EMFILE` instead of growing unbounded.
  Every fd-creating syscall propagates it, and `pipe`/`socketpair` return the
  first descriptor if the second is refused. Host-tested by
  `the_descriptor_table_honours_its_ceiling`.
- **`RLIMIT_STACK`**: `resolve_stack_growth` reads the faulting task's soft limit
  as the stack floor (SIGSEGV beyond it; capped at the reserved region for a
  raised/infinite one).

### 6.3 Namespace Support (Minimal) — **stub**

`unshare`/`setns` exist and validate the `CLONE_NEW*` bits (unknown → `EINVAL`),
but a request to actually enter a namespace returns `EOPNOTSUPP` - there is one
of each kind and no isolation machinery behind it. Real `CLONE_NEWNS` mount
namespaces (or any other) are not implemented; the syscalls fail cleanly rather
than pretend. (Not one-shot implementable: needs per-namespace mount/root/pid
state threaded through the VFS and scheduler, and guest verification.)

### 6.4 Additional Process Syscalls — **DONE**

All wired to real handlers in `posix.rs`/`proc.rs`: `getpgrp`, `setreuid`,
`setregid`, `getresuid`, `getresgid`, `capget`, `capset`.

| Syscall | Nr | Description |
|---------|-----|-------------|
| `getpgrp` | 111 | Implemented, verify correctness |
| `setreuid` | 113 | Set real/effective UID |
| `setregid` | 114 | Set real/effective GID |
| `getresuid` | 118 | Get real/effective/saved UID |
| `getresgid` | 119 | Get real/effective/saved GID |
| `capget` | 125 | Get capability state |
| `capset` | 126 | Set capability state |

---

## Phase 7: Device Drivers (Low Priority)

### 7.1 virtio-gpu

**Why**: Graphical output, desktop environment.

- VirtIO GPU driver (2D mode first, 3D stretch)
- Framebuffer mapped to userspace via mmap
- `ioctl` DRM interface (`DRM_IOCTL_MODE_GETRESOLUTION`, `DRM_IOCTL_MODE_SETCRTC`)
- `/dev/dri/card0` via devfs

### 7.2 virtio-input

**Why**: Keyboard/mouse without serial.

- VirtIO input driver (evdev protocol)
- Event queue: `struct input_event { time, type, code, value }`
- `/dev/input/event0` via devfs
- Keyboard scancode -> keycode mapping

### 7.3 AHCI/SATA (stretch)

**Why**: Real hardware support beyond virtio.

- AHCI HBA driver
- ATA IDENTIFY, READ SECTORS, WRITE SECTORS
- GPT partition detection (already have partition scanner)

---

## Phase 8: Security Hardening (Low Priority)

### 8.1 ASLR (Address Space Layout Randomization)

**Changes**:
- Randomize mmap base address per process (currently fixed)
- Randomize stack base address
- PIE support already exists (`ET_DYN` ELF) — just randomize load address
- Use `rand::next_u64()` for randomization seed

### 8.2 seccomp

| Syscall | Nr | Description | Status |
|---------|-----|-------------|--------|
| `seccomp` | 317 | Secure computing mode | Done (STRICT) |
| `prctl(PR_SET_SECCOMP/PR_GET_SECCOMP)` | 157 | Legacy latch spelling | Done (STRICT) |

- `SECCOMP_MODE_STRICT` — **done**. Latched via `seccomp(2)` (`op=0, flags=0,
  args=NULL`) or `prctl(PR_SET_SECCOMP, 1)`, and read back with
  `prctl(PR_GET_SECCOMP)` (0 unconfined, 1 strict). It is the real Linux
  allow-list, not a block-list: the thread may make only `read`, `write`,
  `exit` and `sigreturn`; every other syscall is fatal (`SIGKILL`), matching
  Linux's `do_exit(SIGKILL)` on a strict violation. The latch is per-task,
  cannot be cleared, survives `exec` and is inherited across `fork`/`clone`.
- `SECCOMP_MODE_FILTER` — **future**: needs a BPF runner; refused with
  `ENOSYS` so a caller never mistakes it for being confined.

### 8.3 User Stack Canary — **DONE** (kernel side)

`AT_RANDOM` is now filled from the kernel ChaCha20 pool (RDRAND + boot-epoch +
tick seeded) at every exec — **16 random, per-process, per-boot bytes** — which
is exactly what musl/glibc derive `__stack_chk_guard` from, so a stack-protector
canary is now unpredictable (it was a fixed constant before, defeating the point).
The fail path (`__stack_chk_fail` → `abort()` → `SIGABRT`) is musl's own and
needs no kernel work; `-fstack-protector-strong` is a user-build flag, not the
kernel's. Kernel-side stack protection is therefore complete.

---

## Phase 9: Architecture Expansion (Far Future)

### 9.1 aarch64 Complete Boot

Current status: kernel ELF compiles but boot flow incomplete.

**Needed**:
- AArch64 exception vector table (`VBAR_EL1`)
- GICv2/v3 interrupt controller driver
- ARM virtual memory (4KB pages, 3/4-level tables via `TTBR0_EL1`/`TTBR1_EL1`)
- PSCI for power management (reboot, shutdown)
- Timer via `CNTP_EL0` (ARM generic timer)
- UART via PL011 or SBSA

### 9.2 RISC-V Support

- SBI interface for boot/runtime services
- PLIC interrupt controller
- Sv39/Sv48 page tables
- `mtime`/`mtimecmp` timer
- UART via SIFIVE_UART or NS16550

---

## Phase 10: Performance

One number framed this phase: `ping 127.0.0.1` measured 20 ms on the 100 Hz
build (7.5 ms at the current 250 Hz) against the host's 0.03 ms. The cause
was architectural rather than slow code - every blocking network wait slept a
whole clock tick per round, arrived frames were discovered only by tick-rate
polling because the virtio-net adapter was never interrupted, and an AF_INET
socket had no wakeup source at all (pipes and AF_UNIX already did). Each
subsystem below names the same shape of fix: make the event end the wait.

### 10.1 Network Latency — **DONE**

- Blocking socket calls (`recv`/`send`/`connect`, TCP/UDP/raw) park on their
  own readiness through `block_until_deadline` instead of sleeping a tick per
  round; signals still end them as `EINTR`. Sockets register with the stack
  through `poll_subscribe`, and every poll wakes the registered list.
- `poll(2)`/`select(2)` park the same way, with the tick kept as the bound
  for descriptors that have no waker behind them (uevent, signalfd,
  eventfd). An unclocked `epoll_wait` over a network socket - which
  previously would sleep forever, having no wakeup source - now ends with
  the event (`t-epoll-socket`).
- The virtio-net adapter is armed for MSI-X: the vector table entry is
  programmed by hand and every queue bound to vector 0 through the common
  configuration, whose BAR is 64-bit and sits above 4 GiB (reading only the
  low dword maps ordinary RAM, and register "writes" round-trip through it
  while the device hears nothing). The handler polls, wakes the waiters,
  EOIs the local APIC, and preempts so the woken task runs immediately
  rather than at the next tick. The tick-rate polls all remain, so a machine
  where the interrupt cannot be armed runs exactly as it did.
- `sendto`/UDP/TCP emit immediately after queueing rather than at the next
  poll; smoltcp's clock reads the TSC instead of the tick counter; `xtask`
  builds release by default (the loader gained the `wcslen` its release
  codegen calls for).
- Measured by `t-netlat` (20 ICMP echo rounds to 127.0.0.1): 7.5 ms → 0.3 ms
  average, 74 µs floor - the remainder is slirp's own path through the host.

### 10.2 Context-Switch and Fork Cost — planned

**Why**: every task switch saves and restores 512 B of FPU state
(`fxsave`/`fxrstor` unconditionally), rewrites CR3 with a full TLB flush (no
PCID, no global pages), and `fork` walks the entire address space to mark
pages COW with two PTE writes per page. A busybox fork+exec pays all three.

| Change | Where |
|---|---|
| Enable PCID (`CR4.PCIDE`), keep per-process TLB entries across switches | `arch/x86_64/paging.rs` |
| Save FPU state lazily: set TS, save on first `#NM`, restore on switch in | `arch/x86_64/context.rs` |
| One TLB flush per fork instead of per-page `protect` calls | `sched/runtime.rs` |
| Kernel stacks smaller than the fixed 64 KiB per task | `sched/runtime.rs` |

### 10.3 Syscall Path — planned

**Why**: every syscall pays a scan of all 64 task slots for itimers whether
any is armed, and every user-buffer operation walks four page-table levels
per page to validate it. Both are fixed costs on the hottest path in the
kernel.

| Change | Where |
|---|---|
| A global "any itimer armed" flag; `check_itimers` skips the scan when clear | `sched/runtime.rs`, `syscall/mod.rs` |
| Validate user buffers with the copy itself (fault handler catches `EFAULT`) instead of a pre-walk | `arch/x86_64/paging.rs`, `syscall/mod.rs` |

### 10.4 Pipes and Per-Frame Allocations — planned

**Why**: pipe reads and writes move bytes one at a time through a `VecDeque`
(`pop_front`/`extend(iter().copied())`), and the network path allocates and
frees a fresh `Vec` per frame in each direction (TX build, RX copy, raw
recv's 1500-byte temporary).

| Change | Where |
|---|---|
| Slice copies (`extend_from_slice`) or a ring buffer for pipe data | `vfs/pipe.rs` |
| A pool of frame buffers behind the smoltcp device seam | `net/mod.rs` |
| The raw receive temporary sized from the actual datagram, or on the stack | `net/socket.rs` |

### 10.5 Filesystem and Block Read Path — planned

**Why**: ext4's cache is a 512-slot direct-mapped array that thrashes on
large working sets, path resolution reads a directory block per component on
a miss, and each 4 KiB write leaves for the device as eight 512-byte
synchronous requests. Write-through stays: crash visibility is a deliberate
semantic, and only the read side is being tuned.

| Change | Where |
|---|---|
| Sequential readahead past the block being asked for | `vfs/ext4/` |
| Set-associative cache with victim selection | `vfs/ext4/cache.rs` |
| virtio-blk interrupt-driven completions and multi-request pipelining (the MSI-X ground is laid in 10.1) | `drivers/virtio/blk.rs` |

### 10.6 Serial Output — planned

**Why**: UART transmit is polled with the console lock held; on real
hardware a 60-byte line costs ~5 ms of machine-wide stall (QEMU's emulated
16550 never blocks, which is why it does not show here). The kernel's own
hot paths log nothing - this matters the day they do.

| Change | Where |
|---|---|
| A transmit ring drained by the TX-held interrupt; `write` only enqueues | `drivers/serial.rs`, `arch/x86_64/idt.rs` |

### 10.7 Sub-Tick Sleeps — planned

**Why**: the tick is the smallest unit a sleep can name (4 ms at 250 Hz).
Event-driven waits ended the network's dependence on it; the remaining
callers - `nanosleep`, timerfd, TCP retransmit pacing - still round to whole
ticks.

| Change | Where |
|---|---|
| LAPIC one-shot timer programmed per sleep, PIT kept as the periodic floor | `arch/x86_64/` (new `lapic-timer`), `sched/policy.rs` |

---

## Priority Summary

| Phase | Effort | Impact | Blocks |
|-------|--------|--------|--------|
| **P1: epoll + timerfd** | ~2 weeks | Critical | Most network services |
| **P2: Signal delivery** | ~2 weeks | Critical | Shell, bash, most programs |
| **P3: FS enhancements** | ~2 weeks | High | Hard links, timestamps, locking |
| **P4: Networking** | ~2 weeks | High | AF_UNIX, ping, TCP options |
| **P5: Memory** | ~1 week | Medium | Shared memory, madvise |
| **P6: Process** | ~2 weeks | Medium | Debugging, rlimits |
| **P7: Drivers** | ~2 weeks | Low | GUI, input |
| **P8: Security** | ~1 week | Low | ASLR, seccomp |
| **P9: Architectures** | ~4 weeks | Low | aarch64, RISC-V |
| **P10: Performance** | 10.1 done; rest ~3 weeks | High (10.2-10.3) / Medium (rest) | Interactive latency, throughput |

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

The first two phases (epoll/timerfd + signal delivery) are the highest impact — they unlock the majority of real-world Linux userspace programs.
