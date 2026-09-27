# MPOS Feature Roadmap

Missing essential features, prioritized by impact on real-world workload compatibility.

---

## Progress — TODO Board

One-glance status of the whole roadmap. `[x]` done, `[~]` partial/served with a
named gap, `[ ]` not started. Each line points at its detail section below.
Full-ABI gaps still falling through to `ENOSYS` are collected under **ABI**.

Legend: `[x]` done · `[~]` partial · `[ ]` todo · *(blocked: …)* = needs a prerequisite first

**Core (Phases 1–3)**
- [x] P1 Syscall completion — epoll · timerfd · 67/68 other calls (§1)
- [x] P2 Signal delivery — frame · mask · siginfo (§2)
- [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)
  - [x] `flock` owned by the open file description, record locks by the process
        (`t-filelock`) · read/write deliberately *not* enforced: Linux advisory
        locks never refuse an `open`ed read, and mandatory mode is a deprecated
        per-file opt-in (§3.2)
  - [x] ext4 `atime` bumped on read, with Linux's `relatime` rule so a read
        that sees no change costs no device write (§3.3)

**Networking (Phase 4)**
- [x] 4.1 AF_UNIX — `SOCK_STREAM` bind/listen/connect/accept · data path ·
      `SCM_RIGHTS` · `SCM_CREDENTIALS` · `SO_PEERCRED`, and `SOCK_DGRAM`
      datagram transfer with whole-message boundaries and per-message
      `SCM_CREDENTIALS` for a `SO_PASSCRED` reader (`t-unix-scm`, `t-socketpair`)
  - [x] per-send cmsg boundaries on `SOCK_STREAM`: each handover carries the
        stream position its send ended at, and a read stops there, so two sends
        of one byte each arrive as two deliveries with their own descriptors
        (`t-unix-scm`)
  - [x] a *data-less* `sendmsg`'s handover is delivered on the read that asks
        for it: the record is due the moment it is queued and no byte will ever
        arrive *with* it, so the read answers with the record and a zero-byte
        return instead of parking on a byte that cannot come (`t-unix-scm`)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [~] 4.3 TCP options — round-trip all; `TCP_NODELAY`/`SO_KEEPALIVE` reach smoltcp
  - [ ] `SO_REUSEADDR` / `SO_LINGER` / buffer sizes still record-only (§4.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)

**Memory (Phase 5)**
- [~] 5.1 Shared anon `mmap` — served; aliased frames don't cross a plain `fork`
- [~] 5.2 `madvise` — validated; `MADV_DONTNEED` no-op *(blocked: anon demand paging)*
- [x] 5.3 `process_vm_readv`/`writev` (§5.3)

**Process (Phase 6)**
- [x] 6.1 ptrace (§6.1) · [x] 6.2 real rlimits incl. NOFILE/STACK (§6.2) · [x] 6.4 identity/cap calls (§6.4)
- [ ] 6.3 Namespaces — `unshare`/`setns` return `EOPNOTSUPP` (no isolation) (§6.3)

**ABI gaps → `ENOSYS` (remaining)**
- [x] inotify · [x] `memfd_create` (anon shared buffers, §7/Stage 3) · [~] xattrs (validate → `EOPNOTSUPP`, no store) (§Remaining)
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
- [ ] `userfaultfd` · `tid/tgid_add_object` (rseq) — niche
- [ ] real-time signals 32..=64 (needs 64-bit pending masks throughout) (§2.3)
- [ ] `io_uring_setup`/`enter`/`register` — deferred by design (§1.3)

**Drivers (Phase 7)**
- [~] 7.1 Display — `/dev/fb0` (mmap + FBIOGET_* + PAN/VSYNC present) and a
      DRM/KMS card `/dev/dri/card0` (resources, dumb buffers, addfb, setcrtc,
      page-flip + vblank events) for a software/pixman compositor. No 3D/virgl,
      no `GBM`/`EGL` (§7.1)
- [x] 7.2 Input — virtio-input evdev: capability ioctls (`EVIOCGBIT`/`EVIOCGABS`/
      …), `/dev/input/eventN` + `/sys/class/input` + `SUBSYSTEM=input` uevents, and
      events that belong to the description that grabbed them (§7.2)
- [ ] 7.3 AHCI/SATA (stretch)
- [x] `/dev/uinput` — a virtual input device a userspace program creates and
      writes events into: the self-test's hands, so `t-evdev` pins *whose* event
      a record is on the machine and not only in the host unit tests. Gets a
      `/dev/input` node and an `add` uevent; no `/sys/class/input` entry (sysfs
      builds its tree once at boot), no `UI_ABS_SETUP`, no force feedback (§7.2)

**Wayland compositor (weston) support** — the cross-cutting work a rootful
compositor needs, all verified by `t-*` (GUI end-to-end is the operator's run of
`cargo xtask run --mode weston`):
- [x] Peer credential + fd passing (SCM_RIGHTS/`SO_PEERCRED`) — Stage 0 (see §4.1)
- [x] libinput device classification — evdev capability ioctls — Stage 1, `t-evdev`
- [x] Device discovery — `/dev/input`, `/sys/class/input`, input uevents — Stage 2
- [x] `wl_shm` buffers — `memfd_create` + frame-aliased shared mappings — Stage 3, `t-memfd-shm`
- [x] Framebuffer present — `FBIOPAN_DISPLAY`/`FBIO_WAITFORVSYNC` + a pointer
      (virtio-tablet) + `--mode weston` (kernel frees the scanout) — Stage 4, `t-fbdev`
- [x] DRM/KMS surface — `/dev/dri/card0` (UAPI ioctl + struct ABI, dumb buffers,
      page-flip, vblank events) for DRM-backend compositors — Stage 5, `t-drm`
- [ ] 3D / GL / virgl / Mesa-`GBM`+`EGL` — deliberately out of scope
- [x] a parked evdev reader - direct, `select`, or an `epoll` interest like a
      compositor's - is woken by the service task when a record lands, instead of
      at the next tick; the wait re-checks the queue as it parks, so an event
      between a failed read and the sleep cannot be lost
- [x] `EVIOCGRAB` is per-owner: a second descriptor is refused `EBUSY`, a
      re-claim by the holder is not, and the claim dies with its descriptor
      (`t-evdev`)
  - [x] and the device is the owner's while it holds it: a reader that is not
        the grabber is told nothing - not the records, and not a readable answer
        either - because every client has a *position* in the device's window
        rather than a turn at one shared queue. Two readers of an *ungrabbed*
        node each see every event, which the shared queue could not do either
        (whoever read first consumed it for everybody)
- [x] udevd's worker channel works, which is the difference between a keyboard
      and none: a `SOCK_SEQPACKET` *pair* had a message queue that `send_dgram`
      refused to write into, because the check asked "is this a `SOCK_DGRAM`?"
      rather than "does this end carry messages?" - and a message no one can
      send is a device that never gets an `ID_INPUT` property, which is what
      `weston.ini`'s `require-input=false` has been standing in for (`t-udev-worker`)
  - [ ] `SOCK_SEQPACKET` still carries no boundaries over a *named* socket (the
        stream this family has), which is the one documented divergence left on
        the path from `udevadm` to a tagged device node

**Security (Phase 8)**
- [~] 8.0 **a userspace process could panic the kernel** - fixed for the case
      that was reproducing (a ramfs file grew as one `Vec`, and a 128 MiB
      contiguous `realloc` is an order the buddy allocator never serves), and
      tmpfs now has a byte budget answering `ENOSPC`. Still open: the global
      allocator's failure path is `panic!`, so any other unbounded user-driven
      growth still ends the machine (§8.0)
- [x] 8.1 ASLR — `ET_DYN` load bias, interpreter base, stack top and the `mmap`
      region are drawn per `exec`; a fixed-address image keeps its own address,
      because that is what a fixed address means (§8.1)
- [~] 8.2 seccomp — `STRICT` done; `FILTER` *(blocked: BPF runner)*
- [x] 8.3 user stack canary (`AT_RANDOM`) (§8.3)

**Performance (Phase 10)**
- [x] 10.1 Network latency — event-driven waits + virtio-net MSI-X (`ping` 7.5→0.3 ms)
- [~] 10.2 Context-switch/fork cost — lazy FPU · one TLB flush/fork · smaller
      stacks all landed, with the numbers in §10.2; PCID recorded as a decision
      rather than attempted (the audit it needs is in §10.2)
- [~] 10.3 Syscall path — itimer-armed flag landed; the fault-driven user-buffer
      copy is not, and §10.3 says why the pre-walk it removes does not exist
- [~] 10.4 Pipes + per-frame allocations — the pipe ring and the raw-receive
      temporary landed, with numbers in §10.4; the frame pool is not, and §10.4
      also records the larger finding that a pipe *handover* costs two ticks
- [~] 10.5 ext4/virtio-blk read path — the set-associative cache landed with its
      device-read counts in §10.5; readahead and blk pipelining are each other's
      prerequisite and neither is measurable until the test machine has a disk
- [x] 10.6 Serial TX ring (drained by the TX-held interrupt; the mirror drops
      rather than stalls) · [ ] 10.7 Sub-tick sleeps (LAPIC one-shot)

**Architectures (Phase 9 — far future)**
- [ ] 9.1 aarch64 complete boot · [ ] 9.2 RISC-V support

### Suggested next moves (highest impact, tractable)

Landed since this list was written: `flock` is owned by the open file
description while record locks stay per-process (`t-filelock`), `EVIOCGRAB` is
per-descriptor with Linux's `EBUSY` and dies on close (`t-evdev`), ext4 updates
`atime` under `relatime`, two board entries that were already true are marked
so (AF_UNIX `SOCK_DGRAM` transfer, the serial TX ring), and evdev's read rule is
finished: a reader that is not the grabber is told nothing while the grab is
held, and two readers of one node each keep their own copy (§7.2, `t-evdev`).

What is left, in the order that keeps each step verifiable on its own:

1. **`SO_REUSEADDR` / `SO_LINGER` / buffer sizes** (§4.3) - self-contained, and
   each has a test that can only be written once the behaviour is real.
2. **Per-message cmsg boundaries on `SOCK_STREAM`** (§4.1) - the handover queue
   is already a FIFO *per send*; what it lacks is the byte offset each send
   finished at, so a read that swallows two sends reports one control chain.
   Needs the pipe ring to carry marks, which is why it is not a small patch.
3. **ASLR** (§8.1) - `ET_DYN` (static-pie) is already loaded and RELA-relocated
   at base 0, so the work is a load bias plus `AT_PHDR`/`AT_BASE`/`AT_ENTRY`
   shifted with it, then the stack and mmap gaps. Every static-pie binary in the
   image is a test of the auxv arithmetic.
4. **Anonymous demand paging** (§5.1 + §5.2) - the prerequisite both items name,
   and the only one that changes the mm fault path for everything.
5. **`seccomp` FILTER** (§8.2) then **namespaces** (§6.3) - one a BPF evaluator
   over `seccomp_data`, the other a set of decisions (which namespaces, and what
   each one must observably isolate) that should be made before code.
6. **The wake latency before anything else in this family** - the two
   measurements that disagree with their own plan both land on it: `wait4` costs
   8 ms whether the child owned 32 MiB or nothing (§10.2), and a pipe handover
   costs 8.0 ms for the same two ticks (§10.4). Both are 250 Hz ticks on a task
   that has already been woken, so a blocking syscall's round trip has a floor
   of ~8 ms on this machine and no amount of work removed from the syscall
   itself will move it. `t-forkcost` and `t-pipecost` are the instruments.
   After that, §10.5's disk-backed instrument and the frame pool, then each of
   these wants a before/after number - and the number is allowed to say "no".

---

## Current Status

- **~229 Linux x86_64 syscalls** implemented
- **Uniprocessor** — single core, `critical_section::Mutex<RefCell<T>>`
- **Signals** — handlers delivered via `rt_sigframe`, mask enforcement, `sigqueue` siginfo payloads
- **Filesystems**: ramfs, ext2/ext4 (write-through), devfs, procfs, sysfs, devpts
- **Networking**: smoltcp TCP/UDP/ICMP/ARP over virtio-net; AF_INET + AF_UNIX
  (path `SOCK_STREAM`, with `SCM_RIGHTS` fd passing + `SO_PEERCRED`) + netlink
  (read + uevent)
- **Display / input**: virtio-gpu 2D scanout exposed as `/dev/fb0` (mmap +
  fbdev ioctls + pan/vsync) and a DRM/KMS `/dev/dri/card0` (UAPI ABI, dumb
  buffers, page-flip + vblank) for a software compositor; virtio-input evdev
  (`/dev/input/eventN`, capability ioctls, and events that belong to the
  description holding `EVIOCGRAB`) with a keyboard + tablet, and `/dev/uinput`
  for a program that needs an input device of its own
- **Buffer sharing**: `memfd_create` whose `MAP_SHARED` mapping aliases real
  frames, so a compositor and a client sharing one descriptor see the same bytes
- **Scheduler**: 4-level priority + round-robin, COW fork, demand-paged stack
- **Network latency**: event-driven socket waits + virtio-net MSI-X interrupts;
  loopback `ping 127.0.0.1` 7.5 ms → 0.3 ms (10.1 done)
- **Phases 1–3 complete**; 5.3 / 6.1 / 6.2 / 6.4 done; 4.1 / 4.2 / 4.4 / 5.1 done; 4.3 / 5.2 partial; 6.3 stub; 7.1 / 7.2 answered (Wayland-compositor support: see the TODO Board).

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
`{owner, pid, class, kind, [start,end)}`, in `src/vfs/locks.rs` (the range math,
conflict rule and split-on-relock are host-tested). Both lock families live in
that one table but never see each other - a `class` of `Record` (`fcntl`
`F_SETLK`/`F_GETLK`/`F_SETLKW`) and `Whole` (`flock`) conflict only within the
class, as Linux defines the two to be independent. `F_SETLK` takes or drops a
byte range and refuses a blocked request with `EAGAIN`; `F_GETLK` reports the
first conflicting lock (its kind, range and holder pid) or clears to `F_UNLCK`;
`F_SETLKW` blocks and wakes on release. `flock(LOCK_SH|LOCK_EX|LOCK_UN)` covers
the whole file (`[0, infinity)`), shared/exclusive arbitration identical to the
record locks, and blocks unless `LOCK_NB`. A holder that dies is pruned on the
next grant/query.

`owner` is deliberately *not* the same thing in the two classes, because Linux
draws the line in two places: a record lock is owned by the thread group, a
`flock` by the open file description (the `Arc<File>` the fd table shares, so a
`dup` shares the lock and a second `open()` does not). `release_on_close`
applies the two different rules - POSIX's "closing any descriptor drops the
process's record locks", and "the flock dies with its own description" - and
`user/abitest/src/bin/t-filelock.rs` pins both, with the contending-second-`open`
and the sharing-`dup` cases side by side.

*Not a gap:* `read`/`write` do not consult the lock table, which is what Linux
does. These are **advisory** locks: nothing refuses a `read` that walks over
another process's `F_WRLCK`. Linux's mandatory mode is a per-file opt-in -
setgid with the group-exec bit cleared, on a mount that allows it - and is
documented as broken and deprecated; adopting it here would be a divergence
rather than a completion. What the two families *are* used for here is
cooperation: `apk` and `fish` take the locks, and callers that hold one refuse
the other.

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
the kernel keeps one cmsg boundary per send rather than per skb, a sender's
`SCM_CREDENTIALS` opt-in is accepted but the kernel always fills the peer's
real ids (no impersonation), and `SO_PEERCRED`/`SO_PASSCRED` are the unix
`connect` path's - a `socketpair` carries descriptors but not credentials.

A *data-less* `sendmsg` used to be the one deadlock left in the family: the
record rode a byte boundary and there was no byte, so the read parked on a
descriptor it already had. `PipeEnd::scm_due` now lets the read see that the
front record is due before it decides to wait, and the read returns the record
with zero bytes rather than waiting for the stream to advance - which is the
one case where a `recvmsg` on a stream can answer 0 and not mean end-of-stream,
so a receiver that takes a short read has to look at its control buffer before
concluding the peer is gone. That is the price of the record, and the
alternative was a hang; `t-unix-scm` pins both the delivery and the
non-blocking second read that follows it.

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

**Start here, in this order.** First the fact that bounds everything else in this
section: **IPv4 `listen(2)` is refused with `EOPNOTSUPP`**, pinned deliberately in
`user/tests/src/bin/t-tcp.rs` - the stack is outbound-only, no accept, so no
in-guest server can run and no local socket pair exists to observe a teardown
with. Adding `listen`/`accept` (a listening smoltcp socket + a `TcpSocketSet`
passive-open slot + an inode per accepted connection) is therefore the first
§4.3 item, not a footnote to it, and it is also what `dbus`, a local `nginx`, and
any `ssh` *server* on this machine would need.

**Second is done, and was measured rather than assumed**: a plain `close(2)`
reached `net::free_entry`, which called `sockets.remove(handle)` on the spot,
while smoltcp only puts the `FIN` out from its poll loop after the socket is
marked closing (`shutdown(2)` did call `close()`; the last close did not). A
closing socket is now marked and parked in `Stack::teardown` - bounded, because a
peer that stops answering may cost a slot but not the set - and `reap_teardown`
frees it after the poll that carries its last segment, so the descriptor is gone
when the caller asks and the wire still gets its four packets. `t-tcp-close` + a
host-side listener through slirp (`10.0.2.2:4599` → host `127.0.0.1:4599`) shows
the peer receiving the bytes *and* end-of-stream instead of sitting until it
gives up.

That producer went missing once, with the consumer, the bounded list and this
paragraph all still in place - `TEARDOWN_MAX` sitting unread was the only clue,
and a suite that passes says nothing, because `t-tcp-close` reports `skip:` and
passes whenever no host listener is up. Re-measured with the listener running:
both of its connections (the orderly close and the `SO_LINGER{on,0}` one) reach
`GOT-FIN`. The lesson is that the probe is only an instrument when somebody
holds it: run it with the listener up before believing this section.

`SO_LINGER{l_onoff=1, l_linger=0}` - "reset, discard unread" - is the one close
shape this stack still cannot do, and it is left orderly on purpose rather than
faked: smoltcp 0.13's `abort()` only moves the socket to `Closed` and transmits
nothing (its resets are reply-driven, emitted when a packet *arrives* for a
closed socket), so calling it would make the close *silent*, which is worse than
the `FIN` the peer gets now. Honouring it needs an actual reset on egress.

*Remaining after those:* `SO_REUSEADDR` applied at `bind`/`listen` (a TIME_WAIT
rebind allowance, which needs the listening side to exist first), `SO_KEEPALIVE`
probe packets, and a reset on egress for `SO_LINGER{on, 0}`. `SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per
socket as things stand: smoltcp sizes its buffers when the socket set is built,
so a real answer needs either a set rebuilt per requested size or a limit applied
in the mpos read/write queue - and until one of those exists the options should
say so rather than round-trip a number that changes nothing.

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

`PTRACE_DETACH` used to return 0 and leave the tracee stopped. The cause was not
the stop it was in but the one it had not reached: `PTRACE_ATTACH` injects a
`SIGSTOP`, and while the tracee is still inside its current syscall that signal
is merely *pending*. A detach that only ends an in-progress stop therefore hands
back a process which stops itself on the way out of that syscall - with no tracer
left, and nothing on the machine but a `SIGCONT` that could end it. The test
demonstrated it by hanging the machine for ten minutes with every earlier check
printing `ok:`, until a bounded `WNOHANG` poll replaced the blocking `waitpid`.

The three cases are Linux's, and were measured against it before being matched
(`t-ptrace-attach` pins all three):

| Sequence | Linux | Now |
|---|---|---|
| `attach`, `detach(0)`, never `wait`ing | target runs on | the pending stop signal is dropped by the detach |
| `attach`, report the stop, `detach(0)` | target runs on | the stop parked with `delivering` set is ended |
| `kill -STOP`, `attach`, `detach(0)` - or `detach(SIGCONT)` | stays stopped | the stop is left alone, and no wake is sent |

The third row is the one that decides the shape of the code: the signal a detach
injects goes to the *tracee*, not to its stop, so naming `SIGCONT` does not end a
group stop the user asked for - and a `wait` on that child stays empty. Which is
also why the wake is now conditional everywhere: a task parked in a group stop is
held there by the scheduler's sleep, so a detach that woke it would release the
very stop it had just decided to leave. The companion rule is in the signal
sender - a stop signal arriving for a task already in a group stop has nothing to
say to it, and must not wake it either.

Dropping the `SIGSTOP` *pending* bit in the third case is not an invention: after
`kill -STOP` + attach + `detach(0)`, one outside `kill -CONT` returns Linux's
process to running rather than re-stopping it, which is only possible if the
attach's own signal did not outlive the detach.

What is *not* modelled: Linux reports a `PTRACE_EVENT_STOP` to a tracer that
attaches to an already-stopped task, so that the tracer's first `wait` returns
before anything is detached. Here that stop stays the parent's news, which is why
the test attaches without waiting - the same thing `strace -p` on a suspended job
would see.

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

### 7.1 Display: virtio-gpu (framebuffer + DRM/KMS) — **ANSWERED** (2D / software)

Two faces of the same virtio-gpu scanout, both driven by the single render
task that owns the GPU (the only `flush()` caller).

- **Framebuffer (`/dev/fb0`)** — 2D scanout, BGRA32; `read`/`write` (write
  copies in and blocks on the through-flush), `mmap` of the real DMA frames
  (pinned), and the fbdev ioctls `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`.
  `FBIOPAN_DISPLAY` and `FBIO_WAITFORVSYNC` are answered, and on a compositor
  boot `FBIOPAN_DISPLAY` *presents* (drives the flush), which is what weston's
  fbdev backend uses per frame. `FBIOPUT_VSCREENINFO` is refused (a resolution
  change would tear down live mappings).
- **DRM/KMS (`/dev/dri/card0`, `src/drivers/drm.rs`)** — a virtual single-CRTC
  card speaking the Linux UAPI exactly (`drm.h`/`drm_mode.h`): `VERSION`,
  `GET_CAP` (`DUMB_BUFFER`), `SET_CLIENT_CAP`, `MODE_GETRESOURCES`,
  `GETCRTC`/`SETCRTC`, `GETENCODER`/`GETCONNECTOR`/`GETPLANE`,
  `ADDFB`/`RMFB`, `PAGE_FLIP`, `GETPROPERTY`/`SETPROPERTY`/`GETPROPBLOB`, and
  GEM dumb buffers (`CREATE_DUMB`/`MAP_DUMB`/`DESTROY_DUMB`) whose `mmap`
  aliases real pages with the same per-mapping frame reference discipline the
  memfd uses. A page flip is applied to the scanout and reports back as a
  flip-completion `drmEvent` readable (and pollable) on the card fd. This is
  the presentation path a **pixman/software** DRM compositor needs; there is
  **no 3D, no `GBM`/`EGL`, no `virgl`** - the deliberate boundary.

v1 divergences: one fixed connector/crtc/encoder/plane; vblank is synthesized
at the frame beat (no hardware interrupt); dumb-buffer mmap uses a
`handle << log2(PAGES_PER_BUFFER)` offset encoding rather than a DRM VMA
manager; `SO_PEERCRED`-style per-client DRM leasing and atomic modesetting are
absent (legacy `SETCRTC`/`PAGE_FLIP` only).

### 7.2 Input: virtio-input (evdev) — **DONE**

`src/drivers/virtio/input.rs` is a real evdev provider, and this is what makes
`libinput` accept a device. At `init` each device's capability picture is read
from the virtio config (`ev_bits` per event type, `abs_info` per axis,
`prop_bits`, `ids`) and frozen; the node answers `EVIOCGVERSION`, `EVIOCGID`,
`EVIOCGNAME`, `EVIOCGBIT(evtype)`, `EVIOCGABS(axis)` and `EVIOCGRAB` (the ioctl
dispatcher in `fs.rs`). Nodes live at `/dev/input/eventN` (major 13, minor
64+N), are announced with `SUBSYSTEM=input` uevents, and have
`/sys/class/input/eventN/{dev,uevent}` - so udev/`libinput` enumerate them the
way Linux's do. Keyboard events also feed the console line discipline as before.
The QEMU machine attaches `virtio-keyboard-pci` **and** `virtio-tablet-pci` (a
pointer). A reader - blocking, `select`, or an `epoll` interest - is woken by
the service task the moment a record lands, and a grab is arbitrated per open
file description: `EBUSY` to a second claimant, released when the claimant's
descriptor closes.

**Who a record belongs to** is Linux's other half, and it is done. While the
grab is held, a reader that is not the grabber is told nothing - neither the
records, nor a readable answer from `poll` - and two readers of an *ungrabbed*
node each see every event, which the single shared queue could not do either
(whoever read first consumed it for everybody).

The device keeps one *window* of records and gives every client a *position* in
it (`src/evdev.rs`) instead of handing each client a buffer of its own the way
Linux does: the same picture with the copying shared. A position is a sequence
number in the device's record stream rather than an index into the window, so
the window dropping its oldest record - the bound that keeps an unread device
from pinning memory forever - moves the floor under a client instead of shifting
what it has not read, and a client that fell further behind than the window is
long resumes at the oldest record still held: the accounting Linux does with
`SYN_DROPPED`. A client is an open file description (`File::ofd`), the same key
the grab and `flock` already arbitrate by; its position is taken when the
description opens the node and forgotten when the description's last descriptor
closes, so an address that is reused cannot inherit a reader's place. While the
grab is held the non-owners' positions are lifted past the records that were the
grabber's, so a release hands the window to nobody - the records the grabber did
not read are still its own.

The policy sits in `src/evdev.rs` rather than in the driver, because the driver
is bare metal and the policy is arithmetic: nine host tests pin it (two readers
each see every record; a client that appears late does not read the backlog; a
grab keeps the records from every other reader; a release does not hand the
window over; a position below the window resumes at the floor; only the clients
a record is *for* are woken - so a non-owner is not woken to find nothing; a
short buffer is `EINVAL`; a forgotten client starts at the end of the window;
the clientless read is not the grabber). What `t-evdev` adds on the machine is
the *shape* of the rule, because the host sends no keystrokes during a run:
`poll` on a non-owner is never ready while the grab is held, the owner's own
answer is unchanged by its grab, and - when a record was queued, which the test
reports rather than assumes - draining one reader leaves the other's copy
intact. That last one is the assertion a shared queue fails, so a run with a
record to work with does discriminate; a run without one says so in its own
output. A *blocking* read by a non-owner of a grabbed device is the one case
that cannot be asserted from inside the guest without a keystroke, and the
`readable`/`take` pair in the unit tests covers it.

One divergence, recorded rather than papered over: `read_at` with no client - a
kernel reader that never named a description, which is the VFS's own `read_file`
and a `splice` source - reads as a client of its own, keyed 0, an address no
description can have. `read`/`readv`/`pread` all arrive through `File::read` and
so carry the real description; the clientless reader sees only what arrives after
it first reads, and someone else's grab excludes it, which is what Linux's
`pread` on a grabbed device from a non-owner means.

**The self-test's hands: `/dev/uinput`.** Everything above could be pinned on
the machine except one thing - a rule about *whose* event a record is needs a
record, and the self-test host sends no keystrokes. `src/uinput.rs` (the
interface's arithmetic) and `src/drivers/uinput.rs` (the device) close that. A
program opens `/dev/uinput`, declares what its device can report
(`UI_SET_EVBIT`, `UI_SET_KEYBIT` and the rest of the family), names it
(`UI_DEV_SETUP`), and asks for it to exist (`UI_DEV_CREATE`) - at which point it
appears as the next free `/dev/input/eventN` with the ordinary evdev face, and
every whole `struct input_event` written to the descriptor is what that device
reports. `UI_DEV_DESTROY`, closing the descriptor, or the process exiting takes
it away again, node and all: the descriptor *is* the device, so a `dup` is one
device and a second `open` is another. The arithmetic - which request means which
bit, what a write has to be shaped like, how `struct uinput_setup` is laid out -
is ungated and has six host tests of its own; the device is bare metal, like the
driver it feeds.

`t-evdev` now makes a device, finds it by the name it gave it, and pins the rule
with records it reported itself: both readers see the frame; draining one leaves
the other its copy - the assertion a shared queue fails; while the grab is held
the owner is readable and the non-owner is not, and the owner reads its own
event; a release hands nothing over; the next record is the other reader's
again; and destroying the device takes the node out of `/dev/input` and ends the
stream for a descriptor still holding it. None of it depends on the host, so it
runs on every sweep.

What a uinput device does *not* get, recorded because it is the shape of the
thing and not a defect: a `/sys/class/input/eventN` tree - `sysfs` builds its own
tree once at boot and has no runtime registration - so `udev`/`libinput` will
not classify a device made this way. That is enough for the conformance suite,
which opens the node directly, and not enough for a compositor; it is a `sysfs`
job rather than an input one. `UI_ABS_SETUP` and force feedback are not
implemented either: a synthetic device has no axes and answers `EVIOCGABS`
`EINVAL`, which is the honest answer for one whose geometry nobody described.

Writing the test found an ABI defect that had been there since the ioctls were
added, and this work fixed it: `EVIOCGNAME` answered with the length *including*
the terminating NUL, and a virtio device's name kept the config's own terminator
*inside* the string - so a caller that used the answer as a length counted a byte
that is not part of the name (a 20-character keyboard name came back as 22).
Linux's answer is `strlcpy`'s: the name is what stands before its first NUL, a
size of zero is the length query (answered without copying), and the answer is
that length *excluding* the terminator even when the copy had to be cut short,
which is how a caller with a small buffer learns the name did not fit. The
standalone arm that served sizes whose low byte is zero - the 256-byte buffer
`libevdev` passes - answered differently from the family handler, so it is gone
and one branch answers for every size. `t-evdev` pins the rule without
hardcoding any host's name: the answer must equal the position of the first NUL
in the buffer it wrote.

### 7.3 AHCI/SATA (stretch)

**Why**: Real hardware support beyond virtio.

- AHCI HBA driver
- ATA IDENTIFY, READ SECTORS, WRITE SECTORS
- GPT partition detection (already have partition scanner)

---

## Phase 8: Security Hardening (Low Priority)

### 8.0 A userspace process could panic the kernel — **fixed: a file is not one buffer**

`dd if=/dev/zero of=/tmp/x bs=1M count=700` used to end in a kernel panic and
QEMU exit `35`. The cause is now read off the machine's own report rather than
inferred, and it was neither of the two things first written here:

    KERNEL PANIC: memory allocation of 134217728 bytes failed
      last 8 syscalls: nr 1 (write) ...

Not ramfs being unbounded, and not the ext4 block cache (fixed at 512 preallocated
slots). It was a **ramfs file's own storage**: one `Vec<u8>`, so growing a file
means a `realloc` for one *contiguous* run - doubling to 128 MiB - which is not a
shortage of memory but an order the buddy allocator never serves. The machine died
with bytes still in hand.

**Fixed** by the shape Linux uses: `Chunked`, a file as a list of 64 KiB chunks,
so growth allocates 64 KiB at a time however large the file gets
(`vfs/ramfs.rs`, `a_file_is_chunks_and_not_one_contiguous_run`). The invariant the
first version got wrong is worth stating because nothing else on the page says it:
**every chunk but the last is whole**, because a chunk that stops being last has
bytes after it now - a 5-byte interior chunk makes `len()` lie about everything
that follows, which three existing file tests caught at once.

Also landed, as the tmpfs behaviour this kernel genuinely lacked: a per-mount byte
budget, charged *before* allocating and answered with `ENOSPC`
(`Usage`, `a_full_mount_answers_enospc_instead_of_growing`), with room returned on
truncate-down and on the last reference dropping rather than on `unlink` - a
deleted-but-open file still occupies its bytes. The budget guard checks what the
machine could actually hand over: free frames **plus the heap's own cached
bytes**, since the heap keeps what it has taken and `free` never rises again.

`t-fill-ramfs` covers the end-to-end property (refused, not dead) and one
non-obvious trap it found: `ftruncate(0)` leaves the descriptor's offset where it
was, so the next write asks for a hundreds-of-megabytes hole and is refused
*correctly* - a test that forgets to `lseek` back measures the kernel's manners,
not its memory.

**Still open, and it is the rest of the original item**: the global allocator's
failure path is still `panic!`, so any *other* unbounded user-driven growth - the
page tables of a process that maps too much, a `Vec` in another filesystem path -
still ends the machine. The remaining work is step 2 and 3 of the old plan: make
user-facing allocation fallible end to end (`ENOMEM` out of the syscall, not a
panic) and give a process that runs the memory out something to lose besides the
kernel.

### 8.1 ASLR — **DONE** (four landmarks; `ET_EXEC`'s own address deliberately not one of them)

Measured on Linux before writing any of it, over 64 runs of the same program
each time: a position-independent binary's code base, its heap anchor and its
`mmap` page all move by amounts of the order of a tebibyte, and a process's
stack top moves too. The same program linked `-no-pie` kept its code address at
`0x401156` in every single run while its stack and its `mmap` page went on
moving. That last row is the shape of this feature: **a fixed-address image must
not be moved**, because the absolute addresses its own instructions carry were
chosen at link time, and "randomising" it produces a program that cannot start
rather than one that is hard to attack.

What is drawn per `exec`, in `Layout` (`src/sched/runtime.rs`):

| Landmark | Range | Notes |
|---|---|---|
| `ET_DYN` image load bias | a tebibyte's worth, page-aligned | `map_image` places every segment at `bias + vaddr`, and `AT_PHDR`/`AT_ENTRY`/`brk` are the *placed* addresses |
| The interpreter's base | the same, stacked below `mmap` | This is `libc` on this userland - a fixed base here is a supply of return addresses identical in every process |
| Stack top | 8 MiB | Linux bounds this by memory and rlimit, so the number is not portable; 8 MiB is what keeps the whole region clear of the image below |
| The `mmap` region's start | a tebibyte's worth | `mmap_cursor` starts here and grows down as before |

The regions are **stacked rather than drawn independently** - `mmap`'s start,
then the interpreter below it, then the image below that, with a fresh draw
between each pair - so no draw can land one region on top of another, and the
top of a placed image is always below `MMAP_BASE`, which is what keeps a bias
from walking a program's pages up into the kernel half. `fork` draws nothing: a
child that inherited its parent's pages at addresses of its own had would not
have inherited its parent's pages.

Relocation had to be settled to get here, and the answer is Linux's: **the
kernel relocates only an image that has no interpreter.** A dynamic one is
relocated by that interpreter, which finds the program's load address from
`AT_PHDR` - so a kernel that also applied the table would add the load bias to
every pointer twice. That pass used to run for both kinds, which was invisible
only while the bias was always zero, because writing `addend` twice and writing
`addend + 0` twice land in the same place.

`t-aslr` pins the parts a running program can see: it forks, `execve`s *itself*
in a reporting mode, and compares what two fresh runs say about themselves - the
stack and the `mmap` page differ, the code address and the heap anchor do not.
Self-exec is why it can do that without a shell, a fixture, or a runner that
passes arguments it does not pass to anything else. What it cannot cover is the
load bias, because every program in that initramfs is `ET_EXEC` and none of them
has an address the kernel is allowed to choose; that half is covered by the
desktop image, where every binary is a dynamic `ET_DYN` and a bias applied to
the mappings but not to `AT_PHDR` means the machine never reaches a login
prompt.

**Not done, and each one is a decision rather than an oversight**: no `vdso`
(there is none to move); the stack's range is our bound, not Linux's
memory-sized one; and the draws are 28 bits of page granularity where Linux
gives an `ET_DYN` base rather more. Adding `listen`/`accept`-shaped inbound TCP
(§4.3) and a `MAP_FIXED`-free `mmap` hole review would both want this layout
re-examined, which is worth knowing before either is attempted.

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

### 10.2 Context-Switch and Fork Cost — **PARTIAL** (three of four landed, measured)

**Why**: every task switch saved and restored 512 B of FPU state
(`fxsave`/`fxrstor` unconditionally), rewrote CR3 with a full TLB flush, and
`fork` walked the entire address space to mark pages COW with a flush after
every page. A busybox fork+exec paid all three.

| Change | Where | State |
|---|---|---|
| Save FPU state lazily: `CR0.TS` on the task entered, its `#NM` restores it | `arch/x86_64/context.rs`, `idt.rs`, `sched/runtime.rs` | **done** |
| One TLB flush per fork instead of per-page `protect` calls | `sched/runtime.rs`, `arch/x86_64/paging.rs` | **done** |
| Kernel stacks smaller than the fixed 64 KiB per task | `sched/runtime.rs` | **done** |
| Enable PCID (`CR4.PCIDE`), keep per-process TLB entries across switches | `arch/x86_64/paging.rs` | **not done** — the argument is below |

**The lazy FPU.** `mpos_context_switch` no longer touches the 512 bytes of
`FpuState` at the head of each `Context`; it sets `CR0.TS` on the task it
enters, and that task's first floating-point instruction traps into the `#NM`
handler, which clears the bit and loads the image. The saving half stays eager
and moved into Rust (`Table::hand_over_fpu`), because that is the last moment
at which the outgoing task's registers are uncontroversially its own. A task's
`fpu_live` flag says whether the hardware holds its state, so the FPU is written
out only when it was actually used: a program that never executes a vector
instruction - which is most of this image, and all of a shell's `fork`+`exec` -
pays one `mov cr0` per switch and nothing else. `fork`/`clone` read the
creator's image through `Table::inherit_fpu` (registers when live, its saved
block when not) and `execve` claims the registers before `fninit`, because a
task that never used the FPU is running with `TS` set. The property the whole
scheme rests on - that the register file belongs to a task and not to the CPU -
was already pinned by `t-fpu-across-switch`, which stores two distinguishable
bit patterns to `xmm7` across a `sched_yield`; it passes unchanged, and it
exercises the deferred restore on both sides.

**The fork flush.** `parent_space.protect(..) + set_cow_bit(..)` per page became
`set_cow_bit_quietly(..)` per page and one `flush_if_active()` after the loop.
What that removes is 16384 CR3 writes for a parent with 8192 pages, and the
per-page `update_flags` round trip that re-derived the PTE's flags from the
walk's own report - the quiet form sets the COW bit and clears `WRITE` in one
write and leaves the rest of the entry alone, which also stops the fork from
dropping `ACCESSED`/`DIRTY` on the parent's pages.

**What the machine says.** `t-forkcost` (new, and in the suite) runs the *same*
fork/exit/wait loop twice: once with a 32 MiB arena mapped in the parent, and
once after `munmap` has given it back. The second parent has a few pages of
address space instead of 8192, so anything that scales with the mapping shrinks
by three orders of magnitude.

| 220 cycles, 200 timed | `fork` | `wait4` |
|---|---|---|
| 32 MiB parent, per-page flush (the old shape) | 349.0 µs | 7650.8 µs |
| 32 MiB parent, one flush (this change) | 350.9 µs | 7648.9 µs |
| no arena at all, one flush | **17.0 µs** | 7982.8 µs |

Three things fall out of that, and the first two are corrections to what this
section said before the measurement existed.

**The fork flush change is not measurable, and the reason is not subtle.** Sixteen
thousand CR3 writes moved the fork by 0.5% - inside the noise - because a CR3
write is not a TLB flush on this machine: QEMU runs with `-accel kvm`, the
guest's translations are tagged (`VPID`), and loading a CR3 is a tag switch
rather than an invalidation. The change is kept because it is strictly less work
and the smaller of the two shapes, but nobody should read a speedup into it.

**The cost of a `fork` is the mapping, and the cost of the `wait4` after it is
not.** Taking the arena away cuts the fork from 351 µs to 17 µs - twenty-one
times - and leaves the `wait4` where it was, at *eight milliseconds either way*.
An earlier draft of this section read the second row as the child's exit and its
8192 frame references coming home (`release_user_frames`), and the third row
refutes it: a parent reaping a child that owned nothing pays exactly the same
8 ms. The frame release is not the cost.

**What 8 ms is, and why it matters more than anything in this section.** It is
two ticks of this build's 250 Hz clock, and it shows up in the two places a task
can park: the `wait4` above, and the pipe handover in §10.4, where a 64 KiB
transfer costs 8.0 ms for the same reason. Both are the disease §10.1 cured for
sockets - a blocking wait that ends on the scheduler's tick rather than on the
event that satisfies it (see §10.4, where the same 16 MiB moves at 7 MiB/s
behind a task boundary and 2611 MiB/s without one). It is a fixed latency on
every blocking call in the system rather than a cost proportional to any work,
which is why it is worth more than the flush, the marking, and the frame pool
put together. `t-forkcost`'s two-phase shape is the instrument for it: a change
that ends a wait on its event will move the top row's `wait4` and the bottom
row's together.

**Why PCID is not done, and it is a decision rather than an oversight.** Three
things have to be true before `CR4.PCIDE` pays for itself, and only the first is
straightforward. (1) Each address space needs a stable PCID and a way to
invalidate a *recycled* one - manageable, with 4096 of them and 64 task slots.
(2) The kernel half must be mapped `GLOBAL`, or every process carries its own
copy of the kernel's translations in the TLB: the `G` bit is only honoured on
the entry that maps a page, so making the kernel half global means setting it on
the bootloader's huge-page leaves, and `page_flags` deliberately leaves it off
today for exactly this reason. Without it, a PCID switch trades one flush for a
per-address-space replication of the kernel's working set. (3) Every existing
CR3 write has to be re-audited for *which* PCIDs it must invalidate: with
`PCIDE` on, `activate(root)` invalidates the PCID written and leaves the others
alone, so `map_mmio` and the kernel-half table walks - which is where a write
adds a mapping every address space must see - would leave stale entries in every
*other* PCID, which is a wrong translation rather than a slow one. On top of
that, the workload here is `fork`+`exec`, and every `exec` builds a fresh
`AddressSpace` with a fresh PCID, so the reuse PCID is paid for is rare by
construction: the win would be the parent's entries surviving a child's
lifetime, which the table above suggests is not where the time goes anyway.
Recorded rather than attempted, so the next person starts from the audit list
rather than from the idea.

### 10.3 Syscall Path — **PARTIAL** (the cheap half landed)

**Why**: every scheduler turn and every clock tick scanned all 64 task slots
for an armed interval timer whether any process had armed one, and every
user-buffer operation walked four page-table levels per page.

| Change | Where | State |
|---|---|---|
| A global "any itimer armed" flag; `check_itimers` skips the scan when clear | `sched/runtime.rs` | **done** |
| Validate user buffers with the copy itself (fault handler catches `EFAULT`) | `arch/x86_64/paging.rs`, `syscall/mod.rs` | **not done** — the argument is below |

**The itimer flag.** `ITIMER_ARMED` is set by `itimer_swap` - which is the only
place a timer is ever armed, because no task inherits one and `exec` clears them
- and recomputed *by the scan itself* when it runs. That ordering is the point:
a task that exits while armed disarms a timer without going through
`itimer_swap`, so a flag that was only ever "set on arm" would stay set for the
rest of the boot and quietly restore the full scan. Recomputing costs one extra
scan after the last timer goes away and makes the flag wrong only in the
direction of running the scan - never in the direction of skipping a timer that
is due. Only `ITIMER_REAL` is tracked, because it is the only one that is a
wall-clock deadline the scheduler has to honour.

**Why the fault-driven copy is not here.** The plan's premise does not survive
reading the code. There is no pre-walk separate from the copy: `read_user` and
`write_user` translate one page at a time *as they copy*, through the direct
map, and a read is therefore one walk per page, not two. Only the write
direction walks twice, and the second walk is the copy-on-write probe
(`is_cow_page` before `translate`) - which is a handful of loads against the
page-table entries the walk just touched, next to a `memcpy` of the same page.
Deferring all of it to the CPU's own address translation would mean the kernel
dereferencing a user pointer, and then a `#PF` taken in **ring 0** on a bad
pointer has to become `EFAULT` rather than a panic: that needs an
exception-table/fixup mechanism (identify the faulting RIP as inside the copy
routine, redirect the trap frame to a landing pad, resume), and getting the RIP
range check wrong turns a real kernel bug into a silent `EFAULT`. The safe shape
is kept: the kernel never dereferences a user pointer, so a bad one cannot fault
at all. What is left as a possible future is the one-walk dedup of the write
path, which is a strict reduction in work with no measurable claim attached.

### 10.4 Pipes and Per-Frame Allocations — **PARTIAL** (two of three, measured)

**Why**: pipe reads and writes moved bytes one at a time through a `VecDeque`
(`pop_front`/`extend(iter().copied())`), and the network path allocates and
frees a fresh `Vec` per frame in each direction.

| Change | Where | State |
|---|---|---|
| A ring buffer with slice copies for pipe data | `vfs/pipe.rs` | **done** |
| The raw receive temporary sized from the actual datagram, or on the stack | `net/socket.rs` | **done** |
| A pool of frame buffers behind the smoltcp device seam | `net/mod.rs` | not done |

**The pipe.** `PipeState::data` is now a [`Ring`] over a `Vec` whose live bytes
are `bytes[start..]`: storage taken lazily, reclaimed when the reader catches the
writer, and the dead prefix dropped with one `drain` on the pass where the
arrival would otherwise pass `CAPACITY`. A read is one slice copy and a write is
one `extend_from_slice`, where the old shape was 8192 deque operations for a
`PIPE_BUF` round trip. `t-pipecost` (new, and in the suite) measures it in two
phases, because the two turn out to answer different questions:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 61.7 ms - 259 MiB/s | 6.1 ms - **2611 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.5 ms - 7 MiB/s | 2047.4 ms - 7 MiB/s |

The first row is the change: ten times the throughput, which is what two slice
copies per 4096 bytes instead of 8192 deque operations buys.

**The second row is the finding, and it is bigger than the change.** A
two-process transfer did not move at all - 2047.5 ms to 2047.4 ms - and the
arithmetic says why: 2047 ms over 256 chunks is **8.0 ms per 64 KiB handover**,
which is exactly two of the 4 ms ticks this build runs at (250 Hz). The bytes
are not the cost; the *handover* is, and it is the same disease §10.1 cured for
sockets - a blocking wait that ends on the scheduler's tick rather than on the
event that satisfies it. It also puts the two rows side by side in the most
useful way possible: the same 16 MiB, through the same pipe, at 7 MiB/s when it
has to cross a task boundary and 2611 MiB/s when it does not. Whatever fixes the
handover is worth a factor of three hundred here, and nothing else in this
section is worth a factor of ten. `t-pipecost` is the instrument for it, and it
is deliberately in the suite so the fix has a number waiting for it.

**The raw receive.** `raw_recv` filled a fresh `alloc::vec![0u8; 1500]` and then
copied it into the caller's buffer. It now receives straight into `out`: no
allocation, no second pass over a full MTU, and the "packet larger than the
caller's buffer" case is `recv_slice`'s own `Truncated` - the same `EMSGSIZE`,
detected before anything has been allocated for it. Behaviourally identical for
every packet the MTU admits, which is all the old 1500-byte temporary could
hold either.

**Not done: the frame pool.** The three allocations the plan names for the
network path are the TX build, the RX copy and the raw temporary; the raw one is
gone, and the other two want a pool behind the smoltcp device seam that hands
buffers back rather than freeing them. That is a change to the seam itself
rather than to a call site, and it wants the same treatment as the two above: an
instrument first - a datagram-per-second count with and without the pool - so
that it is landed with a number rather than with a diff.

### 10.5 Filesystem and Block Read Path — **PARTIAL** (the verifiable one landed)

**Why**: ext4's cache was a 512-slot direct-mapped array that thrashes on large
working sets, path resolution reads a directory block per component on a miss,
and each 4 KiB write leaves for the device as eight 512-byte synchronous
requests. Write-through stays: crash visibility is a deliberate semantic, and
only the read side is being tuned.

| Change | Where | State |
|---|---|---|
| Set-associative cache with victim selection | `vfs/ext4/cache.rs` | **done** |
| Sequential readahead past the block being asked for | `vfs/ext4/` | not done |
| virtio-blk interrupt-driven completions and multi-request pipelining | `drivers/virtio/blk.rs` | not done |

**The cache.** Four-way set-associative, least-recently-used within a set, over
fast-lazily-allocated slots - and the set count kept at the 512 that the original
`ls` measurement justified, so the change is *only* about collision and not about
size. A block's set is `block % CACHE_SETS`; an empty way takes it first, and
otherwise the way whose block was asked for longest ago, dated on every access
including hits (dating only misses would make the policy "first loaded" rather
than "least recently used"). Slots no longer allocate their 4 KiB buffer at mount
- a slot takes its buffer the first time it holds a block - which is what makes
`CACHE_SLOTS` 2048 total slots (8 MiB, if a filesystem ever touches all of them)
affordable, and which also removes a fixed 2 MiB per mount that used to be paid
whether or not anything was read.

The instrument is a host unit test, not a clock, because a cache's quality is a
*count*: a mock `BlockDev` that counts reads per block and answers with a pattern
derived from the block number, so a slot handed back for the wrong block fails as
content rather than as slowness. The numbers are the same on every machine and
under TCG or KVM, which is why they are worth writing down:

| Trace | direct-mapped (before) | 4-way LRU (after) |
|---|---|---|
| four blocks a set-count apart, accessed round-robin, 4096 accesses | **4096 reads** (0% hits) | **4 reads** |
| a 2048-block sequential scan | 2048 reads | 2048 reads |
| one hot block read between 2048 scan blocks | <=5 reads of it | <=5 reads of it |

The first row is why the ways are there: a working set of four blocks that share
a set - four groups' inode tables, say - made a direct-mapped cache *not a
cache*, with every access evicting what the next one wanted. The second and third
rows are the traces that must not regress, and the second is also the honest
limit of this change: a sequential scan has no reuse in it, so no cache can help
it, and the two runs read the same 2048 blocks either way. 63 ext4 host tests
cover the driver through the new cache, including a byte-for-byte read of a large
file and a mount of an image the host's `mke2fs` made.

**Why the other two are not done, and it is one reason between them.** They
cannot be measured, and they are each other's prerequisite. `BlockDev::read_at`
is capped at one 4 KiB page per call by its own contract - "drivers bounce through
a single page-sized, physically contiguous buffer" - so readahead cannot ask the
device for fewer, larger reads; it can only issue the same reads earlier, and
this kernel's block driver is synchronous and polled, so "earlier" overlaps
nothing. Readahead becomes worth having exactly when item 3 gives the device
asynchronous completions it can be pipelined behind, and until then it is a
prefetch into a cache that the reader is about to miss anyway.

The measurement gap is the real prerequisite, and it is worth stating plainly:
the test machine boots the conformance suite *without a disk*, so no `t-*` program
exercises the block driver or the ext4 read path at all - the numbers above exist
only because the cache policy is pure enough to drive from a host test. Either
item would be landed blind. What both want first is a disk-backed test path: an
ext4 image attached to the test machine plus a program that reads a large file
from it and reports throughput, which is a `xtask` selftest change and a new
`t-*` - and which would give readahead and pipelining the same kind of instrument
`t-pipecost` gave the pipe. Recorded as the prerequisite rather than skipped.

### 10.6 Serial Output — **DONE**

**Why it mattered**: UART transmit used to be polled with the console lock held;
on real hardware a 60-byte line costs ~5 ms of machine-wide stall (QEMU's
emulated 16550 never blocks, which is why it does not show here). The kernel's
own hot paths log nothing, so this only bit on the day they did - which is what
a `bat` over a minified stylesheet turned into a machine that looked dead.

| Change | Where |
|---|---|
| A transmit ring drained by the TX-held interrupt; `write` only enqueues and parks on a full ring, re-arming before *every* park so a write larger than one drain burst cannot sleep unannounced | `drivers/serial.rs`, `console.rs` |
| The framebuffer console's serial **mirror** is best-effort: it hands bytes to the ring and returns, so a host terminal that cannot keep up drops bytes rather than throttling the window or the console mux | `console.rs` (`serial_mirror_now`) |

### 10.7 Sub-Tick Sleeps — planned

**Two measurements now say the gap is wider than "sleeps round to ticks".** A
task that is *woken* - not sleeping on a deadline, but parked in `block_until`
and made runnable by an event - does not resume for what looks like two ticks,
and it does so on two paths that were not in this section's list:

| measured | figure | instrument |
|---|---|---|
| `wait4` reaping a child, whatever that child owned | ~8.0 ms, unchanged when the child's 32 MiB address space is taken away | `t-forkcost` (§10.2) |
| a 64 KiB pipe handover between two processes | 8.0 ms per chunk, 7 MiB/s, while the same bytes move at 2611 MiB/s without a task boundary | `t-pipecost` (§10.4) |

Both are 8.0 ms at a 4 ms tick, and neither scales with any work the call does,
which is why they are worth more than anything else in this family: they are a
floor on every blocking syscall's round trip. The *mechanism is not identified* -
`Scheduler::wake` does push the task straight into the ready queue and
`Table::next_turn` does pick from it, so the two ticks are not obvious from the
code - and that is exactly why the two instruments are in the suite: whatever
fixes it has a number waiting, and a fix that explains it should be able to say
which of the two ticks it removed.

**Why**: the tick is the smallest unit a sleep can name (4 ms at 250 Hz).
Event-driven waits ended the network's dependence on it; the remaining
callers - `nanosleep`, timerfd, TCP retransmit pacing - still round to whole
ticks.

**What is already there, and what is actually missing**: reading time is *not*
the gap. `timer::since_boot_nanos()` already resolves below the tick from the
TSC, with `CYCLES_PER_TICK` calibrated from two observed ticks - so a
`nanosleep(200µs)` knows precisely when it is late. The gap is the *wake*:
`sleep_ticks` parks the task on the tick counter, so the smallest unit of
sleeping is still one PIT interrupt. Design the change around arming a deadline,
not around a finer clock.

**The order that stays bootable** - the timer is what the whole scheduler rests
on, so each step must be green before the next:

| Step | Change | Where |
|---|---|---|
| 1 | Program the LAPIC timer once, in **one-shot**, from a calibrated count: divide by 16, measure the bus cycles per tick against the PIT *before* enabling, and refuse to go sub-tick at all when the calibration says the LAPIC timer is not ticking (KVM/TCG differ here). | `arch/x86_64/lapic.rs`, `arch/x86_64/timer.rs` |
| 2 | A vector for it in the IDT, and a handler that wakes only the task whose deadline armed it - not the whole tick pass, which would put the cost back. Keep the PIT as the periodic floor so a lost one-shot cannot stall the machine: the tick still advances time, the one-shot only sharpens the wake-up. | `arch/x86_64/idt.rs`, `sched/` |
| 3 | `sleep_ns`, and have `nanosleep`/timerfd/`block_until_deadline` take it: below one remaining tick arm the one-shot, otherwise park on the tick as now. The deadline a wait already carries is in ticks today - widening it to nanoseconds is the edit everything else hangs on. | `sched/policy.rs`, `sched/runtime.rs`, `vfs/timerfd.rs` |
| 4 | Measure it: a guest test that sleeps 1 ms twenty times and asserts the elapsed `since_boot_nanos()` is near 20 ms and *not* near 80 ms (twenty 4 ms ticks). Without the number the change is a claim. | `user/abitest/src/bin/` |

**The trap to watch**: one-shot timers and *interrupt coalescing* fight. If a
one-shot is armed while another is pending, arming must not push the earlier
deadline out - a task whose wake-up got postponed by somebody else's sleep is a
latency bug that only shows under load, which is where it will be found first.

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
