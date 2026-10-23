# MPOS Roadmap

What MPOS is building, phase by phase, and where each phase stands: the objectives,  
the checklist, the remaining work and what to do next — and nothing else.

Every measurement, gate run, lesson and trap behind a claim here lives in the  
[`notes/`](notes/index.md) evidence library, which is where a design decision's reasoning belongs.  
Read this file to know *what to do next*; read `notes/` to know *why the code is the way it is*.  
A `(§N)` pointer on a line here names the section that carries that item's evidence; routing is  
[`notes/index.md`](notes/index.md). An item whose evidence has not been written down says  
*(no evidence file yet)* — that is the roadmap's debt against the notes, not a pointer into them.  
The board states each line as a conclusion plus its pointer: what a line *does*, how it was  
measured and what it cost stays in `notes/`.

Legend: `[x]` done · `[~]` partial · `[ ]` todo · *(blocked: …)* = needs a prerequisite first

---


## Status board

One-glance status of the whole roadmap.

**Core (Phases 1–3)**

- [x] P1 Syscall completion — epoll · timerfd · 67/68 other calls (§1)
- [ ] [~] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2)
- [x] a signal is sent by someone who may send it — `kill`/`tkill`/`tgkill`/`rt_sigqueueinfo`  
  ask Linux's `kill_ok_by_cred` and `CAP_KILL` in the **target's** user namespace, and the record  
  door keeps the caller's `si_code`, `si_pid`, `si_uid` and `si_value` rather than overwriting  
  them (§2.6, item 91)
- [x] a child inherits its parent's signal dispositions — `fork`/`clone` copies the
  parent's handler table and alternate stack, `clone3`'s `CLONE_CLEAR_SIGHAND` serves
  the `SIG_DFL` table instead with `CLEAR|SIGHAND` `EINVAL`, and `exec` resets only the
  caught slots, keeping `SIG_IGN` and the pending set (§2.7, item 96)
- [x] a write to a broken channel is `EPIPE`, and the `SIGPIPE` behind it — a pipe, a
  stream pair and a listener's send run the handler before the call returns (`SI_USER`),
  `MSG_NOSIGNAL` suppresses it, a blocked signal waits pending, and the measured
  exemptions hold: a seqpacket send is `EPIPE` silently, a datagram's dead peer is
  `ECONNREFUSED` once and then `ENOTCONN` (§4.1.13, item 97)
- [ ] [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3) — and  
  `fstatfs` names the filesystem the descriptor was *opened on*, the two doors agreeing (§3.6, item 89),
  each instance now carrying a non-zero `f_fsid` instead of a zero every mount shared (§3.7, item 90)

**Networking (Phase 4)**

- [x] 4.1 AF_UNIX — both socket types, with per-message credentials and fd passing; the  
  abstract namespace is served, `unshare`-free lifetime and all (`t-unix`, §4.1.1) — and  
  a path ends at its first NUL, the bytes behind it being the caller's storage (§4.1.5) —  
  and the addresses answer Linux's three shapes, the peer's being the connect's record (§4.1.6) —  
  and a second `connect` moves a datagram's peer, the name being resolved before the refusal (§4.1.7) —  
  and `listen` is the named call, a connected socket not being a listener (§4.1.8) — and  
  `shutdown` is a flag on the socket rather than a handshake with a peer (§4.1.9), and the  
  "implicit rendezvous" it used to name as a divergence was never Linux's shape (§4.1.10) — and  
  `accept`'s address is the connector's, its two pointers asked separately (§4.1.11) — and a  
  peer's close is a hang-up, not silence: `POLLIN|POLLHUP` on stream and seqpacket alike,  
  read 0 after the queued messages, and the wake udev's settle used to wait 120 s for (§4.1.12) —  
  and the sender's half of that close is Linux's too: `EPIPE` with the `SIGPIPE` raised  
  before the call returns, `MSG_NOSIGNAL` to suppress it, and the seqpacket/datagram  
  exemptions as measured (§4.1.13, item 97)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [x] 4.3 TCP — loopback, listener, and the six options; a stream's `SO_RCVBUF` is its receive
  window, inherited by the socket a listener accepts (§4.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)
- [x] 4.5 `AF_PACKET` — link layer as a socket, userland DHCP can ask; both types served —  
  `SOCK_DGRAM` (payload shape) and `SOCK_RAW` (whole frames in, the caller's own header out) —  
  and the wire format's host pin landed (`crates/link`, `fuzz/link_wire`, §4.5); the raw-IP  
  door (`IPPROTO_RAW`) is complete, its unicast half closing with a next-hop/ARP resolver  
  (`src/net/arp.rs`) now (§4.5)

**Memory (Phase 5)**

- [x] anonymous demand paging — a `mmap` is an address space; `mincore` reads the page tables (§5.1, §5.2)
- [x] 5.1 Shared anon `mmap` — aliased frames cross a plain `fork` (§5.1)
- [x] 5.2 `madvise` — `MADV_DONTNEED` releases and refills; deliberate no-ops say why (§5.2)
- [x] 5.3 `process_vm_readv`/`writev` (§5.3)

**Process (Phase 6)**

- [x] 6.1 ptrace (§6.1) · [x] 6.2 real rlimits incl. NOFILE/STACK (§6.2) · [x] 6.4 identity/cap calls, threads via `clone(CLONE_THREAD)` (§6.4)
- [x] teardown at exit, not at reap (§6, R.15)
- [x] a thread is its group's child — its parent is the group's parent, its death notifies nobody,
  a dying task hands its children to an alive task of its group, and a wait walks the caller's
  group (§15.85)
- [x] a group's death is its last thread's — a worker's `exit_group` is reaped as the leader, a
  leader's own exit is a zombie until its group ends, and the death carries the leader's byte
  (§15.86)
- [x] a pidfd names a process, not a number — `pidfd_open` records the slot **and** its tenant
  generation, so a reused pid reads "gone"; `pidfd_send_signal` keeps §2.6's permission rule and
  its four scopes, `waitid(P_PIDFD)` resolves the descriptor and honours `O_NONBLOCK`/`EAGAIN`,
  and poll/epoll see `POLLIN` at group death and `POLLHUP` only at the reap (§6.8, item 92)
- [x] a pidfd can fetch a descriptor — `pidfd_getfd` duplicates the target's **open file
  description** into the caller's table, with Linux's refusal order (flags, pidfd, task,
  authority, target fd), `FD_CLOEXEC` unconditionally and `ESRCH` for a zombie or a leader
  that exited under a live worker (§6.9, item 93)
- [x] the door that names its own child — `clone3` (435) serves a versioned
  `struct clone_args` and `CLONE_PIDFD`, so a process and a descriptor naming it
  come back in one call (no `pidfd_open` window); `stack` is the child region's
  **base**, the size decides which fields exist, and `CLONE_DETACHED` is refused
  as Linux's own `clone3` refuses it (§6.10, item 94)
- [x] an anonymous descriptor has no file type — `eventfd`, `timerfd`, `signalfd`,
  `epoll`, `inotify` and `pidfd` report `st_mode` with the `S_IF*` bits clear
  (`0600`, or `0700` for a pidfd), `/proc/self/fd` reads back its own
  `anon_inode:[…]` name (`anon_inode:inotify` being the unbracketed one), and a
  zero-length read answers Linux's errno, `inotify` consulting its queue first
  (§6.11, item 99)
- [ ] [~] 6.3 Namespaces — UTS, MNT, **PID**, **NET**, **TIME**, **USER** and **IPC** landed, each with its pin  
  (`t-ns-uts`, `t-ns-mnt`, `t-ns-pid`, `t-ns-net`, `t-ns-time`, `t-ns-user`): the numbers a process  
  sees are the  
  reader's namespace's,  
  both directions of the translation and `/proc` rendered per reader (§6.3); **the task a namespace  
  is spoken for by has rules of its own** — its members cannot end it (only an ancestor's  
  SIGKILL/SIGSTOP, or a signal it has a handler for), and its death takes every task it speaks for,  
  nested namespaces included (§6.3, measured), the kind's own **creation rule** is Linux's too - a  
  process makes a pid namespace **once**, and one whose init has exited takes no tasks (§6.3,  
  measured) - and `/proc/<pid>/status`'s `NStgid`/`NSpid`/`NSpgid`/`NSsid` carry the **chain** of  
  numbers the task is known by rather than the reader's one (§6.3, measured); a child cloned with  
  `CLONE_NEWNET` gets a stack of its own - its own `lo`, no wire, and a port space the machine's  
  cannot see - and the kind is served by all three calls, with **a socket remembering the stack it  
  was made in** (`net`'s handle map): a socket outlives an `unshare`, an inherited descriptor keeps  
  working in a cloned child, and a descriptor handed over by `SCM_RIGHTS` arrives intact and  
  answers the sender's address (§6.3). Beside the IP stack, **`AF_UNIX`'s names are the namespace's  
  where Linux makes them so**: an abstract name belongs to the namespace of the socket that holds  
  it (the name binds again in another, and neither namespace can dial the other's), a *path* name  
  is not namespaced at all and crosses both ways, and what keys the abstract half is the socket's  
  namespace rather than the calling task's (§6.3, measured). A device can move: `eth0` enters a  
  namespace (`ip link set eth0 netns N`, served 2026-10-16 - down and unconfigured on arrival,  
  which is Linux's shape measured, §6.3); a vsock stack stays machine-wide - which is Linux's own  
  shape, measured, with the transport question named (§6.3); and **the clocks can be moved**:  
  `unshare(CLONE_NEWTIME)` arms the caller's *children* slot and never moves the caller, the offsets  
  are written through `/proc/self/timens_offsets` and frozen by the first task to enter, a child's  
  `MONOTONIC`/`MONOTONIC_RAW`/`MONOTONIC_COARSE`/`BOOTTIME` read through them while REALTIME, TAI  
  and the cputime clocks do not, and `setns` is the second door into the same slot (`t-ns-time`  
  walks it in 46 claims, the shape measured whole before any of it was built, §6.3). The other  
  two kinds Linux has left - IPC and cgroup - have no entry at all here (§6.3, §6.6), which is  
  `vfs::ns::Entry::ALL` against `namespaces(7)`'s list of eight. **USER joined on 2026-10-16**: it  
  moves its caller like NET, the map starts empty so the caller lands on the overflow id, `uid_map`  
  takes one self-naming line and then answers `EPERM`, and the four `getuid`-family readers  
  translate the stored id through the task's namespace (`t-ns-user`, 46 claims). The same  
  translation renders what a *file* reports now: `stat`/`fstat`/`fstatat`/`statx` map the stored  
  owner through the reader's namespace, so a file created before an unshare reads the stored id  
  from the machine map and the inside id from the new one, `statx` agreeing with `stat` (§6.3,  
  measured on the host first). It renders what a *socket* reports too (2026-10-16): `SO_PEERCRED`  
  and the `SCM_CREDENTIALS` a read carries map the peer's stored ids through the reading task's  
  namespace, at the read rather than at capture (§6.3, measured on the host first) - and the same  
  struct's `pid` through the reader's *pid* namespace, `pid_vnr`'s rule, so a reader inside one  
  names a peer there and reads `0` for a peer it cannot see (2026-10-16, `t-ns-pid`). And the rung's  
  other half landed too: entering a namespace **is the  
  grant** - `unshare`/`setns` hand the maker the full effective, permitted and bounding sets,  
  measured on the host across all four doors, so a caller that drops its capabilities first reads  
  them back the instant it is in a namespace it made (§6.3). And the **child-side creation** landed  
  (2026-10-16): `clone(CLONE_NEWUSER)` makes a child born already inside a namespace of its own -  
  the overflow id until its map is written, the full set at birth, and the parent's `CLONE_NEWUSER`  
  lifting the capability gate for the other kinds in the same word (§6.3, measured on the host  
  first). And the **netlink/uevent family's own `ucred`** landed (2026-10-16): its ids render  
  through the reading task's user namespace - the kernel's synthetic zero record included - and its  
  pid is the *sender's* number in the sender's own pid namespace, resolved once at send, which is  
  also the port id a zero-`nl_pid` bind prefers (`t-uevent`, §6.3)

**ABI gaps → `ENOSYS` (remaining)**

- [x] inotify · [x] `memfd_create` (§7 Stage 3) · [~] xattrs (validate → `EOPNOTSUPP`, no store) (§R.Remaining)
- [x] POSIX message queues — `mq_open`/`mq_unlink`/`mq_timedsend`/`mq_timedreceive`/
  `mq_notify`/`mq_getsetattr`, priority-ordered and bounded by `RLIMIT_MSGQUEUE`;
  `t-mqueue` pins 41 claims measured on the host first (§6.12) — and **System V
  shared memory** — `shmget`/`shmat`/`shmdt`/`shmctl` with Linux's slot-and-
  generation numbers, marked-not-removed `IPC_RMID`, an attachment count folded
  out of every address space (so a `fork` counts), and `/proc/sysvipc/shm`;
  `t-sysv-shm` pins 61 claims measured on the host first (§6.13, item 101) — and
  **System V semaphores** — `semget`/`semop`/`semtimedop`/`semctl` with waiting,
  `SEM_UNDO` applied at the process's death and split by
  `unshare(CLONE_SYSVSEM)`, and `/proc/sysvipc/sem`; `t-sysv-sem` pins 70 claims
  and seven host tests (§6.14, item 102) — and **System V message queues** -
  `msgget`/`msgsnd`/`msgrcv`/`msgctl` with Linux's FIFO-plus-selection rule
  (`type < 0` takes the *lowest* fitting type, not the front-most), a receive
  that waits for a message it can fit (`E2BIG` only under `IPC_NOWAIT`),
  `MSG_COPY` by queue index, the `msg_qbytes` budget with its asymmetric
  `IPC_SET` gate (a raise needs `CAP_SYS_RESOURCE`, a lowering below the
  queued bytes is accepted), a receive's `msgsz` gated by its sign (`LONG_MAX`
  accepted, `LONG_MAX + 1` `EINVAL`) with an unwritable destination losing the
  message to `EFAULT` as Linux loses it, and `/proc/sysvipc/msg`;
  `t-sysv-msg` pins 74 claims and eight host tests (§6.15, item 103).
  `CLONE_NEWIPC` **landed on 2026-10-23** - the kind moves its caller, the four
  tables key their state by it, a number restarts at 0 in a fresh namespace (so
  the pin compares objects rather than numbers), and the namespace is an `Arc`
  whose last holder - a task or a `ns/ipc` descriptor - frees those tables
  (`t-ns-ipc`, 39 claims, §6.16); `CLONE_NEWCGROUP` is the last kind with
  nothing behind it
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
- [x] the extensible `clone` door — `clone3` (435) with a versioned argument
  struct, and `CLONE_PIDFD` delivering the child **and a descriptor naming it** in
  one call; `CLONE_DETACHED` refused as Linux's own `clone3` refuses it, the
  `stack` being the *base* of the child's region rather than `clone(2)`'s stack
  pointer (§6.10, item 94)
- [ ] `process_madvise(2)` (440) — the pidfd family's one `ENOSYS` left, probed and
  deferred: its one working advice is refused for another process and the rest are
  hints this kernel answers as no-ops (§6.10); the open/getfd/send/wait doors are
  served (§6.8/§6.9, items 92–93)
- [ ] `userfaultfd` · `tid/tgid_add_object` (rseq) — niche
- [x] real-time signals `32..=64` — queued with per-copy `si_value`, bounded by the target user's `RLIMIT_SIGPENDING` (a full `sigqueue` is `EAGAIN`, `kill` drops and answers 0) (§2.3)
- [x] POSIX timers 222–226 — routed, tick-scanned, musl's `SIGEV_THREAD_ID` shape works (§R.Remaining)
- [ ] `io_uring_setup`/`enter`/`register` — deferred by design (§1.3)

**Drivers (Phase 7)**

- [ ] [~] 7.1 Display — `/dev/fb0` + DRM/KMS nodes, dumb buffers, atomic commits, PRIME both ways; weston-on-GL in software; no EDID-driven modes (§7.1, §7.4)
- [x] 7.2 Input — virtio-input evdev, capability ioctls, `/dev/input/eventN` + uevents (§7.2)
- [x] 7.3 AHCI/SATA — second disk named under its own driver, pinned by `t-ahci` (§7.3)
- [x] `/dev/uinput` — a virtual input device from userspace, with its node and `add` uevent (§7.2)
- [x] the `/dev` node set is assembled by the drivers that own it (§7.5)
- [x] configuration space over MMIO (ECAM/MMCONFIG), refused unless both mechanisms agree (§7.6a)
- [x] **the window's other 3840 bytes**: a function publishes `dev->cfg_size` — 256, or the
  full 4096 when the device answers for its extended space at 0x100 and is not aliasing its
  standard header — through `/sys/.../config` and `/proc/bus/pci`, read and written at the
  extended offsets (§7.6a, `t-pci`)
- [x] **the second machine's configuration space too**: aarch64 declares `mpos_pci_config` now
  that the device tree's ECAM window reaches the bus — the config nodes answer 256 or 4096
  bytes per device there as here, read and written through the window rather than a port pair,
  and the test machine carries one NVMe function so the PCIe half is pinned on both machines
  (§7.6c, `t-pci`)

**Wayland compositor (weston) support** — verified by `t-*`; GUI end-to-end is `cargo xtask run --mode gui`:

- [x] peer credentials + fd passing · libinput classification · device discovery · `wl_shm` buffers · framebuffer present · DRM/KMS surface (§4.1, §7.1, §7.2)
- [x] the screen has an owner — `VT_SETMODE` negotiation, revocable DRM master (§7.1, `t-seat`)
- [x] seatd survives a re-run; Mesa GBM+EGL+GLES in software; parked evdev readers woken by the service task; `EVIOCGRAB` per-owner (§7.1, §7.2, R.7)
- [x] the aarch64 GL path's EL0 traps answered — `t-el0-sysreg` (§15.36)
- [x] the session environment — `XDG_RUNTIME_DIR` ships in the shared `core/` overlay, so both machines' login shells have it (R.7's gap, closed by §15.36/§15.37)
- [x] weston's EL0 write fault above its stack — resolved by `resolve_stack_growth` on the aarch64 EL0 data-abort path; the desktop comes up whole (R.7, §15.36)
- [ ] [~] the cursor plane drives a host pointer: the card, the queue, the transfer, the read-back and the aliasing are all cleared by weston-free probes, and the image's emptiness is now **measured at its source** — an interposer logging `gbm_bo_write`'s own input reads `buf=00000000…`: weston writes 16384 zero bytes and the write faithfully copies them. `gbm_bo_write` also only fires when the sprite changes, so a session where desktop-shell never comes up shows no write at all. Controls: a magenta block bound by a weston-free probe reaches the host (`nz=3072`); write-only mappings (`PROT_WRITE` alone, which is what weston asks for) alias across processes; every handle slot works; a plain process's `gbm_bo_write` lands. Remaining: weston's own cursor image, which it writes as 16 KiB of zeros into an object Mesa does not make the plane's backing — the epoll report fix that completed desktop-shell's startup does not touch it (§7.4, R.7)
- [x] epoll reports `revents & interest`: `EPOLLRDNORM` used to be set beside every `EPOLLIN`, so `weston-desktop-shell` — which registers its timerfd for `EPOLLIN` alone — printed `unexpected timerfd events 41` (hex) for every expiry, never read its timer and never finished starting up. Fixed in `vfs::epoll::report`; pinned by a timerfd case in `t-epoll` and by three host tests whose expectations were themselves the bug (§1.1)
- [ ] [~] 3D / virgl / venus (`DRM_VIRTGPU_*`) — served and pinned, and *negotiated* on a  
  `virtio-gpu-gl-pci` boot (`virgl=1 capsets=2`); the renderer is still `llvmpipe`, because  
  Mesa cannot identify the node its devpath names, and the GL card is not the gate's default  
  yet (QEMU returns no clean exit) (§7.4, R.15)

**Compositors beyond weston (niri, wlroots)** — analysis in §7.4:

- [x] the sysfs side of a DRM node's identity — the node's own **devpath** is a child of the  
  function its driver declared now, since libudev's parent walk follows the path and not the  
  `device` link (§7.4); the card found by its driver's declaration; the `..` in a symlink's  
  target (§7.4)
- [x] the socket a compositor binds; `GETRESOURCES`' two-call idiom; property shapes (§7.4)
- [x] DRM sync objects incl. timeline flags, measured on the real card (§7.4, `t-drm-sync`)
- [x] the lease family with `drm.h`'s request numbers; legacy KMS queries refused by name; cursor plane driven (§7.4, `t-drm-lease`, `t-drm-args`)
- [x] `/proc/<pid>/maps` — three sources, two named divergences (§7.4, R.6)
- [x] niri booted end to end on the 3D card: the card negotiates its cap sets so  
  `DRM_VIRTGPU_GET_CAPS` is served rather than refused, and `niri`/`xwayland-satellite` are  
  image packages now, so the compositor's own two remaining warnings are the renderer's  
  rather than a missing file's (§7.4)
- [x] a resource is nameable by the context that names it: `EXECBUFFER` attaches what its  
  `bo_handles` name to the submitting file description's context, and each 3D transfer does the  
  same for its one resource, so the renderer's own batches survive — the boot that used to lose  
  three 852011-byte batches to `Illegal resource` under `initializing the primary renderer` now  
  prints no such line and reaches `putting output Virtual-1` with both sockets up (§7.4,  
  `t-virtgpu`'s two claims, red before it)
- [x] and the frame reaches the display the way Linux's does: a compositor's framebuffer is  
  **scanned out of the device's own copy of its resource** (`SET_SCANOUT` + `RESOURCE_FLUSH` of  
  the fb's own id, `client_scanout`/`scan_out_client`) rather than copied out of guest pages  
  that were never written — measured as 4096000 zero bytes of committed 1280x800 framebuffer  
  while niri drew into it (`.workbuddy/probe-fb-pixels`), and the predicate is the resource's  
  origin (`Resource::host_pixels`)
- [x] and the client's resources are *created* the way Linux creates them: on a card that  
  negotiated 3D every `RESOURCE_CREATE` goes through `RESOURCE_CREATE_3D` (`three_d =
  card().virgl`), because the 2D command is resource/format/width/height and cannot carry the  
  `bind`, `array_size`, `last_level`, `nr_samples` or `flags` a virgl client asks for — the  
  host's GL answered that mismatch with `GL_INVALID_OPERATION` (1282) and dropped kmscube's draws  
  (device pixels for one of its framebuffers: 0 → 4020654, and the three `vrend` lines gone)
- [x] a framebuffer is the description's object, and it has no properties: Linux's
  `GETRESOURCES` walks `file_priv->fbs`, so another client's framebuffer is not in the
  list at all, and `OBJ_GETPROPERTIES` on an fb is `EINVAL` (`width`/`height`/`format`/
  `modifier` are `struct drm_framebuffer` fields, reported by `GETFB2`) — the empty-list
  answer this card used for the device-wide list was the wrong half of both (§7.4, item 68)
- [x] the picture itself, in the window: **kmscube's cube and weston's desktop are both visible on  
  the GL card** (watched, 2026-10-14) — a client that renders into a client resource and has the  
  kernel scan it out, and a compositor whose pixels are the guest's, each through its own present  
  path (`docs/notes/` §7.4)
- [ ] niri's own frame: it commits a framebuffer it never draws into (device pixels 0 of 4096000),  
  with the kernel scanning out exactly that resource — two named leads, its renderer built on  
  `card0` rather than the render node it resolved, and its submissions naming other resources  
  (§7.4)
- [ ] [~] the property families a compositor finds by name — four built, three deliberately absent (§7.4, `t-drm-props`)
- [ ] [~] the DRM surface a wlroots pixman compositor needs — in place, never booted (§7.4)
- [ ] the renderer Mesa would have to pick: `is_software()` stays true until its loader can  
  identify the card, which the node's `virtual/…` devpath is the standing lead for (§7.4)

**Security (Phase 8)**

- [ ] [~] 8.0 allocation-failure panic — reproducing case fixed, tmpfs budget landed; global allocator still panics (§8.0, R.8)
- [x] 8.1 ASLR · [x] 8.3 user stack canary (§8.1, §8.3)
- [x] 8.2 seccomp — STRICT + FILTER with all eight actions measured, the `PTRACE_EVENT_SECCOMP` stop landed, and a tracer rewriting the frame now redirects the call at that stop and at an ordinary `PTRACE_SYSCALL` entry stop — the `-1` sentinel cancelling it instead, and a filter judging the number the tracer wrote (§8.2)

**Performance (Phase 10)**

- [x] 10.1 network latency — event-driven waits + MSI-X (§10.1)
- [ ] [~] 10.2 context-switch/fork cost — lazy FPU, one TLB flush, smaller stacks; PCID recorded as a decision (§10.2)
- [ ] [~] 10.3 syscall path — itimer flag landed; fault-driven user-buffer copy not (§10.3)
- [x] 10.4 pipes + per-frame allocations — ring, handover and the seam frame pool all landed; the rate-pair measurement is recorded (§10.4)
- [ ] [~] 10.5 ext4/virtio-blk read path — cache, wide transfer and the demand run landed and measured; interrupt-driven completions measured and re-scoped, multi-request pipelining dropped; the first machine's boot-disk figure still open (§10.5)
- [x] 10.6 serial TX ring · [x] 10.7 sub-tick sleeps — a deadline is a nanosecond one
  now, and the first machine wakes a wait *between* two clock ticks from the LAPIC's
  one-shot: fifty 1 ms sleeps are 52-59 ms instead of 200 ms, a 25 ms sleep is 25.1 ms
  instead of 28.4, and an interrupted sleep's `rem` is a nanosecond remainder. The
  second machine has no sub-tick wake yet and says so (`SUB_TICK_WAKE` is false there,
  and `t-sleep-subtick` is in its `gaps`) (§10.7)
- [x] the gate's own clock — `cargo xtask test` measured **80.7 s → 6.5 s (12.4x)** warm: a
  stack that nothing advanced (§15.92), wait budgets that were the clock's resolution rather
  than bounds (§15.93), and a sweep whose host half runs beside its guests and whose programs
  are dealt over lanes by their measured cost (§15.94)
- [x] the gate reports itself — a live lane dashboard and an end-of-run report (stage times,
  every lane's programs/verdict/heaviest, the ten slowest programs, and the delta against the
  last green run), with the plain linesbyte-for-byte unchanged when stdout is not a terminal (§15.95)

**Architectures (Phase 9)**

- [ ] [~] 9.1 aarch64 complete boot — boots real Alpine to a login prompt, every service ok, consoles reach the wire (§9.1, §13)
- [ ] 9.2 RISC-V support (§9.2)
- [ ] [~] 13 Machine-seam purity + aarch64 parity — 13.1 and 13.3–13.31 landed, 13.2 in progress (§13); boot-side half is Phase 15 (§15)
- [x] the boot is one flow — one `start`, one sequence, probes ask both transports (§15.8)
- [x] the kernel travels inside the loader — the ESP carries one bootable file (§15.9)
- [x] the self-test sweep is many machines' — the host half beside the guests, one lane per
  core, and a deal that follows what the last run measured (§15.94)

**Architecture debt (Phase 11)** — audit done, Steps A–G landed, V-1/V-4/V-13 closed (§11):

- [x] 11.A subtract · 11.B device seam · 11.C per-open-file registry · 11.D terminal out of `vfs` · 11.F host-test criterion · 11.G depgraph ratchet (§11.2)
- [x] the `mm`/`vfs` seam (V-13) — `MappedObject` trait (§11)
- [x] twelve leaf crates + `mpos-blk` as the thirteenth — enforcement by compile error (§11.2 Step E, R.11.E)
- [x] 11.4 the HAL, stages S1–S5 — machine seam enforced by the compiler (§11.4)
- [x] the two largest files stop being single files — `sched/runtime/` and `drivers/drm/`, one child  
  per responsibility, with the two `arg_shape` rows a mechanical move dropped caught by `t-drm-args`  
  and restored (R.11.F)

**ABI truthfulness (Phase 12)** — Group A landed, gate runs to a summary (§12):

- [x] 12.0 green baseline restored (§12.4a)
- [ ] [~] 12.A honest-refusal batch — landed and gated through the buffer-direction audit (§12.1, R.12)
- [x] 12.B contract-completion batch (§12.2) · [x] 12.C stale rationale comments corrected (§12.3)

---


### Suggested next moves (highest impact, tractable)

In the order that keeps each step verifiable on its own:

1. **namespaces** (§6.3) — UTS, MNT, PID, NET and TIME are landed, and the ladder's remaining work  
   is named per rung: a **wire that can move** into a namespace has landed (2026-10-16) -  
   `ip link set eth0 netns N` moves the device, and it arrives **down and unconfigured** because  
   an address is the namespace's and not the device's (measured on Linux first; `t-ns-net` walks a  
   whole round trip, §6.3); and the **clocks can be moved** too (2026-10-16) - `unshare` arms the  
   caller's children slot, the offsets are written through `/proc/self/timens_offsets` and frozen  
   by the first task to enter, and `t-ns-time` walks the round trip both ways with the caller's own  
   readings pinned unmoved (§6.3). So what is left here is the **two** kinds with no entry at all  
   (IPC and cgroup - two of `namespaces(7)`'s eight), and of the three this item once named, the one  
   it picked is the one whose absence is felt by everything else (a container  
   runtime's first call is `unshare(CLONE_NEWUSER)`, and uid mapping is what the other two are  
   *scoped* by), which also made it the most expensive - it touched the credential model rather  
   than adding a namespace object. **USER landed on 2026-10-16**, its whole shape measured first  
   (§6.3, five probes run as root *and* unprivileged: `unshare` moves the caller, an unprivileged  
   caller may make one, the caller lands on the overflow id because the map starts empty, it holds  
   the **full** capability set inside the moment it is made - "root inside" - `uid_map` takes a  
   single self-naming line once and nothing else, `gid_map` waits for `setgroups=deny`, there is no  
   `user_for_children`, `clone` is a door on Linux and `setns` into one's own is `EINVAL`). What  
   landed is the map object and the translation at the four `getuid`-family readers, pinned in 23  
   claims, and then the same translation at the four `stat`-family doors (2026-10-16) - a file  
   created before an unshare reads the stored id from the machine map and the inside id from the  
   new one, `statx` agreeing with `stat` (§6.3, measured on the host first). A third surface, the  
   socket's, landed on 2026-10-16: `SO_PEERCRED` and the `SCM_CREDENTIALS` a read carries map the  
   peer's stored ids through the reading task's namespace, at the read rather than at capture  
   (§6.3, measured on the host first). And the rung's other  
   half, the **capability grant**, landed on 2026-10-16 as well: entering a namespace is itself the  
   grant, so `unshare`/`setns` hand the maker the full effective, permitted and bounding sets  
   (`t-ns-user`, a caller that drops first reads them back), measured across all four  
   doors on the host (§6.3). The **child-side creation** of `clone(CLONE_NEWUSER)` landed the same  
   day: a child born already inside a namespace of its own, the overflow id until its map is  
   written, the full set at birth, and the parent's `CLONE_NEWUSER` lifting the capability gate for  
   the other kinds beside it (`t-ns-user`, 46 claims, §6.3). And the socket's `ucred.pid` landed the  
   same day - the pid-namespace half of the same struct, rendered with `pid_vnr`'s rule so a reader  
   inside a namespace names a peer there and reads `0` for one it cannot see (`t-ns-pid`, §6.3).  
   The **netlink/uevent family's own `ucred`** followed (2026-10-16): its ids render through the  
   reading task's user namespace - the kernel's synthetic zero record included - and its pid is the  
   *sender's* number in the sender's own pid namespace, resolved once at send, which is also the  
   port id a zero-`nl_pid` bind prefers (three probes; `t-uevent`, §6.3). So nothing of the `ucred`  
   stays named now.  
   **IPC's wait is over in full** (2026-10-23): the four families landed
   (§6.12-§6.15) and then the namespace itself (§6.16) - `unshare`/`setns` move
   the caller, the four tables are one slot space per namespace, a number
   restarts at 0 inside one, and `t-ns-ipc` holds the pair for all four
   families, and the namespace is an `Arc` whose last holder (a task or a
   descriptor) frees its tables. All three doors serve it - `unshare`, `setns` and `clone`, the last
   giving the child a namespace of its own - so what stays named there is
   **CGROUP**, the last rung - cheap only the day the tree it would
   isolate lands, and worthless before it. Nested namespaces pin the  
   *numbers* two levels deep now, as the zap's reach already did. The `/proc/net` reader's rows were  
   per-namespace but unexercised; `t-ns-net` reads the file on both sides of the boundary now  
   (2026-10-15). Two more left this list on  
   2026-10-15: the pid namespace's own creation rules (one per process, and a sealed one takes no  
   tasks - both refused now with the errno Linux uses), and `NSpid`'s single number, which is the  
   chain in all four `NS*` fields now. `F_GETLK`'s `l_pid` is the reader's number too, from  
   2026-10-15 - and its `F_OFD_*` neighbours are served, in the same record domain, from the same  
   day (§3.2). `AF_UNIX` moved off this list  
   (2026-10-15) and vsock left it with a measurement instead of an implementation, both recorded in  
   §6.3. Beside it: seccomp's `PTRACE_EVENT_SECCOMP` stop landed (2026-10-15), and a tracer  
   rewriting the frame there redirects the call now (2026-10-16) - as it does at an ordinary  
   `PTRACE_SYSCALL` entry stop, from the same day, pinned counterfactually in `t-ptrace` (§8.2:  
   the exit stop answers the parent's pid with the re-read and the tracee's own without it).  
   What was unnamed there is closed now (2026-10-16): the `-1` sentinel that *cancels* a call  
   rather than replacing it is measured and pinned at both stops - the call does not run and the  
   tracee reads `-ENOSYS` - while a write at the *exit* stop is a no-op, and the ordering the  
   redirect implies is Linux's too (a `SECCOMP_RET_ERRNO` filter judges the number the tracer  
   wrote, not the one the program asked for). Nothing of the number slot stays unnamed (§8.2).
2. **interrupt-driven block completions** (§10.5's last row) — measured and re-scoped; §10.5  
   carries the structure change, its pin and what each machine's clock can see.
3. **the weston pointer on the window** (R.7, §7.4) — the root cause is fixed in code: the cursor  
   queue is configured when the device has one, `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is accepted  
   with `HOTSPOT_X`/`HOTSPOT_Y` published per-client, and binding or moving the plane hands the  
   pointer to the host over virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR`; the software blend is now only  
   the no-cursorq fallback. Host tests, both bare-metal builds and `depgraph` are green and the pins  
   (`t-drm`, `t-drm-props`) cover the cap gating and the `RMFB` dangling-resource fix. Both turned  
   red on the sweep and are green again after three faults it found (2026-10-13): `OBJ_SETPROPERTY`'s  
   type check named `PLANE_ID` but not `CURSOR_PLANE_ID`, so every write to a cursor property was  
   `EINVAL` while the read of the same property reported the old value; the cursor plane's property  
   list carried `HOTSPOT_X` *and* `HOTSPOT_Y` but its count stopped one short of the second, so a  
   client that had set the cap was shown one of the two; and `t-drm`'s `NR_RMFB` was `0xA4`, which  
   is `MODE_GETGAMMA` and not `MODE_RMFB` (`0xAF`, read out of `drm.h`) - the card was answering a  
   gamma request's `EINVAL`, and the test read it as a refusal to remove the buffer. The measurement  
   a `t-*` program cannot make is answered: `run` binds a QMP socket  
   (`build/<arch>-9p-share/qmp.sock`), `input-send-event` moves the pointer, the pointer bitmap is  
   read back over VNC's `CursorWithAlpha` pseudo-encoding (`screendump` answers `no surface` on the  
   GL backends), and a whole weston session on the GL card shows the desktop up — the epoll report  
   fix (§1.1) removed the startup stall — with its cursor plane bound through atomic. **What the  
   plane receives is empty at its source**: weston's `gbm_bo_write` copies 16 KiB of zeros into an  
   object Mesa does not make the plane's backing, while a weston-free control's image reaches the  
   host. The remaining gap is inside Mesa's virtio-gpu GBM allocation for the cursor BO, upstream of  
   this kernel (§7.4); the no-cursorq fallback branch stays named, since neither configured machine  
   can exercise it.
4. **the display path specifically** — §7.4 carries its own order, and the step it named first is a  
   **dead end, measured 2026-10-18**: a wlroots compositor on `WLR_RENDERER=pixman` stops in  
   `wlr_allocator_autocreate` (`[wlr] [render/allocator/allocator.c:168] Failed to create  
   allocator` / `[sway/server.c:306]`) on the image's `wlroots 0.20.2` — that renderer is the one  
   with no allocator to be handed, while the same binary with its default renderer gets past the  
   line. What the same session found in its place: `sway 1.12` fails `Unable to bind IPC socket` on  
   the DRM backend and headless alike (the errno is the next measurement), and takes a user-mode  
   NULL dereference it survives (§7.4 carries both instruction sites).
5. **the renderer's device identity** — **answered** (2026-10-18). The devpath this item's own lead  
   named has landed (2026-10-13: `/sys/devices/pci0000:00/0000:00:04.0/drm/card0`, which `udevadm  
   info -a -n /dev/dri/card0` walks up to `SUBSYSTEMS=="pci" / DRIVERS=="virtio-pci"`), and Mesa is  
   on the card rather than on llvmpipe: `virgl (AMD Radeon 780M Graphics (radeonsi, phoenix, ACO,  
   DR...))`, Mesa 26.2.4, with weston's `Using GL renderer` behind it (§7.4). What keeps the GL card  
   from being the gate's default is the *host's* Mesa teardown crash, not a refusal this kernel  
   could answer (`machine.toml`'s key and §7.4 carry it), so the split stays.

---

## Current Status

Two machines boot the same flow to a real Alpine userland; the ABI answers Linux's shape across  
syscalls, signals, filesystems, networking, memory, devices and security, with the honest refusals  
named. What is true today — counts, measurements, gates — is in [`notes/`](notes/index.md); this  
section states direction only: the frontier is the two namespace kinds with no entry left (IPC and
cgroup), the second compositor family (the wlroots one, whose first compositor to come up here is
still unfound — `sway` binds no IPC socket and the GL card is already Mesa's, §7.4), the block
layer's completion path, and closing the remaining ABI gaps below.

---


## Remaining ABI support (gaps + services)

The full inventory — grouped by what they block, with each refusal's reason — is  
[notes/appendix-status-board.md](notes/appendix-status-board.md)'s **R.Remaining**. In one line each:  
`fanotify`/`keyctl`/`userfaultfd`/rseq side-objects stay `ENOSYS`; xattrs validate then `EOPNOTSUPP`;  
the semantics still incomplete are a datagram socket's `SO_RCVBUF` - reported in Linux's  
shape but not what sizes the receive ring (item 58; a stream's is now its window, item 98) -  
and the datagram line itself, which is the ring's rather than Linux's 65507 until this stack  
fragments IP; the one namespace kind with no entry at all is cgroup - IPC's four families
(§6.12-§6.15) and the namespace object that isolates them (§6.16) are all
served, so cgroup waits alone, on the tree it would hold.  

**A stream's receive buffer is its window, and a listener hands it to the socket it accepts**
(`docs/uapi/` §5.1 item 98, landed 2026-10-21): item 58 made `SO_RCVBUF` Linux's number, but on a
stream the number governed nothing — a listener's option stayed on the listener and every accepted
connection was built with the default ring, where Linux copies `sk_rcvbuf` onto the accepted socket
(`inet_csk_clone_lock`) and sizes the window from it. Measured whole on a current host first
(`.workbuddy/probe_tcp_rcvbuf.c`, Linux 7.2.11, a server that never reads): ask 2048 is inherited
as 4096 and absorbs 8192 bytes, 32768 as 65536 and absorbs 47104, 262144 as 524288 and absorbs
212886, and the answer is the same whether the option was set before or after `listen`. Served by
`tcp_rx_bytes` (half the stored figure), `add_tcp_sized`, `Listener::rx_bytes` through the pool,
`set_listener_rx` for a post-`listen` change, `settle_tcp` for a relocated socket, and an `accept`
that copies the stored number into the accepted socket. The pin is `t-sockbuf` grown to 18 claims
(two inheritances and three sized listeners pushed until they refuse; guest 18432/49152/147456,
host 8192/47104/286561). It also measured a send bug, and that fix ships with it: `tcp_send`
parked on a **partial** `send_slice` even under `MSG_DONTWAIT`, so a 4096-byte send against a
2048-byte window never returned; it now answers the count it queued, as Linux's `tcp_sendmsg`
does. Gate: x86_64 `pass=198 fail=0`; aarch64 `pass=197 fail=0`; `mpos-kunit` 32/0 on both. Named:
the datagram ring is still fixed (item 58), a connected stream's own `setsockopt` cannot resize
the live ring, the ring is capped at 128 KiB under §8.0's panicking allocator, and a stream's
`SO_SNDBUF` still does not size its send ring.

**A thread is its group's child: `CLONE_THREAD` implies `CLONE_PARENT`** (`docs/uapi/` §5.1  
item 66, landed 2026-10-19): item 65's own residual. A thread's `real_parent` on Linux is the  
parent of the group it joined, so a worker's `getppid`, `status`'s `PPid` and `stat`'s field 4  
all name the group's parent — where this kernel's thread carried no link at all and all three  
read `0`. Three behaviours move with the link and all three were measured on a current host  
first: a thread's death notifies nobody; a dying task hands its children to an alive task of its  
group before init (a thread's child to the leader, a leader's to a live worker — the mirror  
needed its own probe); and `wait` walks every thread of the caller's group, so the leader  
collects a child its worker forked. Served by the clone-path link, `waitable`'s group-and-leader  
rule (and `child_alive_for` asking the same question), a leader gate on the death path's  
notify/wake/console, `alive_thread_of`'s reaper choice, and a reap-time CPU fold only for  
leaders. Pin: `t-tid-dir` 37 → 45 claims, four counterfactuals one per half. What it named — a  
non-leader's `exit_group(2)` announcing nothing to its parent — landed as item 67 below.

**The group's last thread is the process's death** (`docs/uapi/` §5.1 item 67, landed  
2026-10-19): item 66's named residual, and the answer to *when* a process exists to be waited  
for. Linux's `group_dead` is the group's **last** thread leaving, so a non-leader's  
`exit_group(2)` is the process's death — the parent reaps the **leader**, with the dying thread's  
code — and a leader that leaves first on the thread door is a zombie (`Z`, `WNOHANG` 0, not  
`ECHILD`) until its group ends. Measured on a current host first  
(`.workbuddy/probe_exit_group_thread.c`, Linux 7.2.11, five scenarios): a worker's  
`exit_group(7)` reaps the leader with 7; a leader that `pthread_exit`s first is reaped only at  
the group's death and with the **last thread's** code (7 under `exit_group`, 5 under plain  
`exit`); a `clone(SIGUSR1)` group's death is invisible to a plain wait, collected by `__WCLONE`,  
and delivers the **leader's** byte (SIGUSR1, not SIGCHLD); and a thread's child is handed to a  
live task outside the dead group. Served by `exit`'s `group_dead` gate (the record names `tgid`,  
with the dying thread's code and uid and the leader's byte), `trace::group_has_live_task` with  
`is_child`/`child_alive_for` admitting a zombie leader, and `exit_group`'s reparent walk for a  
sibling's children. Pin: `t-exit-group` 4 → 22 claims, four counterfactuals one per rule.

**A device node made by `mknod` carries the number `mknod` was given** (`docs/uapi/` §5.1
item 71, landed 2026-10-20): the last thing standing in front of the call's `dev` argument, and one
§5.2 had charged to a prerequisite it does not need — a number-to-driver table, which is what would
make a number *mean* a device and not what it takes to keep one. Made here, such a node used to
report `st_rdev` `0:0` whatever number the caller named, was called a **character** device even
when asked for from `S_IFBLK`, and opened to read end-of-file and swallow writes — which is what
`/dev/null` does exactly, so a caller could not tell the device that is not there from the one that
is. Measured on a current host first (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11, every row on a
tmpfs and again on an ext4): `makedev(136, 300)` reports `st_rdev` `0x0010882c` and decodes back to
`136:300` — the row the old packing would have read as `136:44`, another device's number, as every
pts above 255 is — `254:70000` reports `0x1110fe70` on a node `stat` calls a *block* device, only
the argument's low 32 bits are the number at all, `4095:1048575` fills the word whole, `statx`'s two
unencoded halves agree, and `0666` under `umask 027` leaves `0640` with the type field untouched.
The pair rides with the kind through `Inode::create` — one door rather than two, because a device
node made without one answers `stat` about a device nobody made — both `mknod` doors unpack the
argument with Linux's `new_decode_dev`, `struct stat` writes it through the inverse, and the node
that keeps it answers **`ENXIO`** from every file door: Linux's own word for a number no driver
owns, and here for all of them, since reaching a driver is still what this kernel's number does not
do. Pins: `t-mknod`'s five new claims (its `ok:` lines identical on the two machines, and every one
of them green on the host's Linux first) and `vfs::ramfs`'s two new unit tests.

**`MSG_PEEK` looks without taking, and the stack's own `peek` was the missing door**
(`docs/uapi/` §5.1 item 72, landed 2026-10-20): the last flag-word item on the IP receive path, and
the one §5.3 had parked behind a prerequisite that never existed — `tcp::Socket::peek` and
`udp::Socket::peek` have been in the stack all along, and what was missing was the reading of two
flags. Both doors refused `MSG_PEEK` with `EOPNOTSUPP` (honest while a peek could not be
preserved) and `t-sock-flags` pinned that. Measured whole on a current host first
(`.workbuddy/probe_msg_peek{,2,3,4}.c`, Linux 7.2.x): a datagram peek does not consume and a short
one leaves the message whole, `MSG_TRUNC` answers the datagram's own length, and a zero-length
non-peek receive *does* consume (a zero-length peek does not); a stream's `MSG_TRUNC` is a
different rule — it answers the count it would have copied and copies **nothing**, consuming what
it counted unless asked to peek — and a zero-length stream receive answers 0 (even when a signal
ends its wait) without taking a byte. `recvmsg` reports a cut datagram with `MSG_TRUNC` in
`msg_flags` whether or not the caller asked, says nothing for a stream, and still names the sender
on a peek. The fix keeps the copy's length apart from the message's (`RecvAnswer { copied, full,
endpoint }`), threads the two flags through `recv_common`, reads with smoltcp's `peek` and an
explicit dequeue, and copies datagrams by hand — which also replaces the `EMSGSIZE` a short
non-peek datagram used to draw with Linux's truncation. Pins: the new `t-msg-peek` (47 claims,
green on the host first, the two `MSG_TRUNC` readings among them) and a rewritten `t-sock-flags`
whose peek-refusal claim the served answer made false. Named: a **raw** socket's peek (the host
has no `CAP_NET_RAW`, so it stays the honest refusal), the other families' peeks, and `recvmsg`'s
zero-length data room, which predates this item. Gate: x86_64 `pass=190 fail=0`, aarch64
`pass=189 fail=0`, `mpos-kunit` 32/0 on both, both pins' `ok:` lines byte-identical; the
counterfactual (every peek refused again, one line) is `pass=188 fail=2`, exactly the two
pins. The pass also fixed `t-ns-net`'s handover fixture, which connected before the child's
listener announced itself - a race the new socket timing lost on aarch64.

**A `recvmsg`'s data room is the whole chain, and a stream's fault is not a datagram's**
(`docs/uapi/` §5.1 item 73, landed 2026-10-20): the item the `MSG_PEEK` landing above left named.
Every receive door but the message door takes one buffer, so `recvmsg` had read the **first
non-empty iovec** and stopped — a `4+4+2` chain got four bytes of a ten-byte message and answered
4 — and a zero-length data room was `EINVAL` for every family. Measured whole on a current host
first (`.workbuddy/probe_recvmsg_iov.c`, Linux 7.2.x): a datagram fills the chain in order and
stops where it ends, sets `MSG_TRUNC` in `msg_flags` without being asked, answers its own length
when the flag *is* asked for, and a zero-length chain consumes it; a stream has no message to cut
— `MSG_TRUNC` counts the bytes it would have copied, copies none and consumes them, a zero-length
chain answers 0 and takes nothing — and a fault on a segment costs a datagram its message but
leaves a stream's bytes queued. An iovec array longer than `UIO_MAXIOV` is `EMSGSIZE` on both
doors, where the send side answered `EINVAL`. Served by one `DataRoom` (the array read once, no
copy at all when the chain has a single non-empty segment) and by `RecvAnswer`'s third length —
`written`, 0 in the one shape whose count is not a copy. Pins: the new `t-recvmsg-iov` (56 claims,
green on the host first, `ok:` for `ok:` identical on both machines); the counterfactual (the
first-iovec rule restored, two lines) is `pass=190 fail=1` with 21 of those claims red, and every
single-segment, zero-length and `EMSGSIZE` claim still green. Gate: x86_64 `pass=191 fail=0`,
aarch64 `pass=190 fail=0`, `mpos-kunit` 32/0 on both. Named: a **raw** socket's zero-length
receive keeps its `EINVAL` and the **vsock** branch still reads its first segment — neither could
be measured on the host this contract came from.

**A raw socket's receive is a datagram's, and its hop count is read off the packet**
(`docs/uapi/` §5.1 item 74, landed 2026-10-20): the two receive shapes item 73 left named, both of
them named around a host that holds no `CAP_NET_RAW` - a zero-length data room answered `EINVAL`
and `MSG_PEEK` answered `EOPNOTSUPP`, because no probe could open the door. The probe makes the
capability instead of waiting for it: `unshare(CLONE_NEWUSER|CLONE_NEWNET)` hands the whole set
inside a user namespace of its own, and a network namespace that namespace owns is one
`ns_capable(CAP_NET_RAW)` answers for (item 43's rule from the other side). `lo` up, ICMP echo
*replies* to itself (type 0, the one the kernel's own ICMP input does not answer, so one send is
one packet), and every answer is the datagram's (`.workbuddy/probe_raw_recvmsg.c`, Linux 7.2.11):
a zero-length room answers 0 with `MSG_TRUNC` and consumes, no iovec at all answers 0 and
consumes, a 4-byte segment of a 36-byte packet answers 4 with `MSG_TRUNC` **unasked** and 36 when
the flag is asked, `MSG_PEEK` copies without taking through a full segment and a zero-length room
alike, a chain 4+`PROT_NONE` is `EFAULT` with the packet gone, 1025 segments is `EMSGSIZE` with
the packet still queued. The `IP_RECVTTL` record rides along even when the buffer cut the header
short, so the octet is read off the packet (`RecvAnswer::ttl`) rather than off the copy - and the
same measurement corrected two words this kernel had been writing: `cmsg_len` is the unpadded
`CMSG_LEN(4)` (20; 24 is the space it occupies, which is what `msg_controllen` reports) and
`cmsg_type` is `IP_TTL` (2), not the `IP_RECVTTL` (12) that asks for it, so `t-rawicmp`'s
control-data claim now reads the host's own answers. Pin: the new `t-raw-recvmsg` (50 claims,
green on the host first - the host run makes its own door as the probe does - and `ok:` for `ok:`
identical on both machines); its first guest run lost one echo beyond slirp and went red on a
packet that never came, so `queue_one` now drains, sends and confirms with a bounded peek, and the
claims it serves are named "an echo is queued" because that is what the helper promises. Gate:
x86_64 `pass=198 fail=0`, aarch64 `pass=197 fail=0`, `mpos-kunit` 32/0 on both; the
counterfactual (the two refusals restored) is the two programs this item touched going red and
nothing else. Named: the **vsock** branch still reads its chain's first segment only.

**The x86_64 signal frame carries the interrupted floating-point image, and a frame that names
none resets it** (`docs/uapi/` §5.1 item 75, landed 2026-10-20): the first machine's `rt_sigframe`
was 440 bytes, `sigcontext.fpstate` was null, and `rt_sigreturn` restored only the general
registers — so a handler that used an `xmm` register (all sixteen are caller-saved, so any
function may) resumed the interrupted code with the handler's values, a frame that named the
state and did not carry it. Measured whole on a current host first
(`.workbuddy/probe_signal_fp{,2}.c`, Linux 7.2.11): the 512 bytes `fpstate` names are the
`fxsave` block verbatim (ten bytes differ from a live `fxsave`, all at 464, the `_fpx_sw_bytes`
XSAVE header this machine has no extended state for; clearing the whole 96-byte padding still
restores the legacy area), and a frame whose pointer the handler nulls is accepted and
**resets** the state — zeroed `xmm`, `mxcsr` `0x1f80` — rather than leaving the handler's
registers live. The image now sits at frame offset 440 (`SIGFRAME_BYTES` 440 → 952, 16-aligned),
`sigcontext.fpstate` at `mcontext + 184` points at it, `SignalDelivery` carries
`fpu: Option<[u8; FPU_BYTES]>` from the lazy record (`sched::trace_fpu`) and `SignalFrame::exit`
answers `SignalFrameExit`, with a null pointer given `FpuState::reset()`. Pin: `t-signal-frame`
17 → 24 claims on x86_64, the handler now clobbering every vector register after reading the
frame; the counterfactual (the kernel files reverted) is `pass=190 fail=1` with exactly five of
its claims red, the alignment claim passing vacuously on the null pointer beside the non-null
one. Gate: x86_64 `pass=191 fail=0`, aarch64 `pass=190 fail=0`, `mpos-kunit` 32/0 on both.

**AF_UNIX's receive doors look without taking, and a datagram's `MSG_TRUNC` is its own length**
(`docs/uapi/` §5.1 item 76, landed 2026-10-20): both flags were refused `EOPNOTSUPP` on every
unix door. Measured whole on a current host first (`.workbuddy/probe_unix_peek{,2,3,4,5,6,7}.c`,
Linux 7.2.x): a stream peek copies without taking and is repeatable; a stream's `MSG_TRUNC`
changes nothing (it copies and consumes as the plain read does); a datagram peek leaves the
front message and reports its own length under `MSG_TRUNC`, a zero-length peek does not consume
while a zero-length non-peek receive does, and `recvmsg` sets `MSG_TRUNC` in `msg_flags` unasked;
a peeked `SCM_RIGHTS` record installs its descriptors and stays queued, so the following read
installs a second set; and a zero-byte handover answers `EAGAIN` on `recv`/`recvfrom`. Served by
`PipeEnd::peek_once`/`peek_scm`, `UnixSock::peek_dgram`, and a `RecvDgram` carrying the whole
length. Pin: `t-unix-peek`, 63 claims green on a host first, plus two `vfs::pipe` host tests;
the counterfactual (the four refusals back) is `pass=191 fail=1` with 18 of its claims red by
name. Gate: x86_64 `pass=192 fail=0`, aarch64 `pass=191 fail=0`, `mpos-kunit` 32/0 on both.
Named: `NETLINK`/`AF_PACKET`/vsock peeks, vsock's first-segment `recvmsg`, and a zero-byte
stream handover being receivable through `recvmsg` here where Linux answers `EAGAIN`.

**Netlink's and `AF_PACKET`'s receive doors look without taking, and a truncated answer names
its own length** (`docs/uapi/` §5.1 item 77, landed 2026-10-21): item 76's named residual on the
two families that were not reading the flag word. A `NETLINK_ROUTE` `recvfrom`/`recvmsg` popped
the front answer whatever `MSG_PEEK` said and returned the copy whatever `MSG_TRUNC` said, the
uevent socket's `recvmsg` did the same, and every `AF_PACKET` door refused `MSG_PEEK` with
`EOPNOTSUPP`. Measured whole on a current host first (`.workbuddy/probe_netlink_peek.c` over a
`RTM_GETLINK` dump; `.workbuddy/probe_packet_peek.c` inside
`unshare(CLONE_NEWUSER|CLONE_NEWNET)` with `lo` up and one frame the socket sends itself): a
peek answers the front's length and leaves it, `MSG_TRUNC` makes the return the message's own
length (3020 through 8 bytes on netlink, 16 on one frame) while the copy stays short, a short
receive answers the room and consumes, `msg_flags` reports the cut unasked, and a zero-length
room answers 0 - consuming without the peek and leaving the message with it. Served by
`rtnl::peek` + `recv_blocking(handle, look)`, `link::Frames::peek` with
`PacketSock::try_peek`/`peek_blocking`, and `uevent::peek`, with each of the five doors reading
the flag word. Pin: `t-netlink`'s 14 new claims, `t-uevent`'s 5, `t-dhcp`'s 4 (external group,
both packet doors, green there at `pass=199 fail=0`) and one `mpos-link` unit test; the
counterfactual (every `MSG_TRUNC` return put back to the copy's length, the peeks kept) is
x86_64 `pass=190 fail=2` - `t-netlink` and `t-uevent` red at the two length claims by name.
Putting the netlink/uevent peek back makes the same tests park on a queue the old take had
already drained, so that arm was observed and stopped rather than counted. Gate: x86_64
`pass=192 fail=0`, aarch64 `pass=191 fail=0`, `mpos-kunit` 32/0 on both. Named: the **vsock** family, measured in the same pass
(`.workbuddy/probe_vsock_recv.c` over `VMADDR_CID_LOCAL`, after `modprobe vsock_loopback`):
Linux serves its peek, ignores its `MSG_TRUNC` (4 of 10 answers 4 and consumes the four), uses
the whole iovec chain (4+4+2 answers 10) and answers 0 for a zero-length room without taking
anything - and serving the peek needs a look at the device seam, which the crate's private ring
does not offer, with no vsock peer on the sweep machine to pin it against.

**A named `SOCK_SEQPACKET` connection keeps its message boundaries, and its messages carry
their descriptors** (`docs/uapi/` §5.1 item 78, landed 2026-10-21): the last divergence in the
AF_UNIX family. A listener reached by a name handed `accept` a `DuplexEnd` byte channel, so the
one type whose description is "message boundaries over a stream" carried a byte stream for
exactly the connections that had a name (a `socketpair` half already carried messages).
Measured whole on a current host first (`.workbuddy/probe_unix_seqpacket{,2,3}.c`, Linux 7.2.11):
3 then 5 bytes read back as 3 then 5, a 4+4+2 `sendmsg` chain is one message of 10, a short read
discards the rest of that message, `MSG_TRUNC` returns the message's own length while `MSG_PEEK`
leaves it, a zero-length send is a message while `read(fd, buf, 0)` is a no-op,
`recvfrom`/`recvmsg` with a zero-length room consume, `SCM_RIGHTS` rides the message (a peek
installs its own set and the take a second, distinct one), and a path-name type mismatch is
`EPROTOTYPE` (an abstract name answers `ECONNREFUSED` from its type-keyed lookup). Served by a
`Pending` queue that holds either a stream end or an already-paired message socket,
`connect_messages`, the existing `recvq` machinery, `Dgram`'s own `fds`, and `read_at`'s
zero-length no-op. Pin: `t-unix-seqpacket`, 58 claims, green on a host first; `t-unix-dgram`'s
claim now says "listens" rather than "is served as a stream". Gate: x86_64 `pass=193 fail=0`,
aarch64 `pass=192 fail=0`, `mpos-kunit` 32/0 on both. Named at landing and served by item 79:
`sendto` naming a destination on an already-connected seqpacket socket answered `EISCONN` where
Linux sends to the peer.

**A send's destination on a connected AF_UNIX socket is the type's rule: a seqpacket connection
drops it, a datagram connection honours it** (`docs/uapi/` §5.1 item 79, landed 2026-10-21):
item 78's named residual, which turned out to be two opposite rules wearing one sentence. Both
message families answered `EISCONN` to any named send on a connected socket. Measured whole on a
current host first (`.workbuddy/probe_unix_sendto_connected{,2}.c`,
`probe_unix_sendto_family_only.c`, `probe_unix_sendto_long_name.c`, Linux 7.2.11): a connected
**seqpacket** socket sends to its peer whatever the name says — the listener's own name, another
live socket, a dead path, a dead abstract name, a stream listener, a family-only `sockaddr_un`
and an `addrlen` of 110 or 128 all send, because Linux clears `msg_name` before the shared
datagram send looks at it (a bad pointer is still `EFAULT`, and >128 still `EINVAL`, from the
syscall layer's copy) — while a connected **datagram** socket resolves and validates the name
exactly as an unconnected one does: another live socket receives, a dead path is `ENOENT`, a dead
abstract name and a regular file are `ECONNREFUSED`, a stream listener is `EPROTOTYPE`, a
family-only address is `EINVAL`, and the pinned peer only decides a send with no name. A stream
pair keeps `EISCONN`. Served by `UnixSock::ignores_send_address` (connected seqpacket only), a
`copy_send_address` that performs the copy bounds/readability check without interpreting the
bytes, and `send_dgram`'s two branches. Pins: `t-unix-seqpacket` 58 -> 66 claims (the three
ignored destinations, the 129-byte `EINVAL` and the empty queue) and six new claims in
`t-unix-dgram` (its `EISCONN` divergence claim gone), both green on a host first. Gate:
x86_64 `pass=193 fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (the
old rule restored): `t-unix-dgram`'s first named-send claim is `EISCONN` and the blocking read
behind it parks, so that arm was stopped rather than counted. Named: an unconnected (or
listening) seqpacket socket's named send is `ENOTCONN` on Linux; the family's message doors are
not reached by one here, so its answer is unchanged.

**A datagram aimed at a path naming another type is `EPROTOTYPE`; an abstract name of another
type stays `ECONNREFUSED`** (`docs/uapi/` §5.1 item 80, landed 2026-10-20): item 79's named
residual. A datagram send or connect answered `ECONNREFUSED` for any live socket without a
message queue, so a path naming a stream or sequenced-packet listener looked like a dead name.
Measured whole on a current host first (`.workbuddy/probe_unix_dgram_wrongtype.c`, Linux 7.2.11):
a **path** naming a live socket of another type is `EPROTOTYPE` (91) for `sendto` and `connect`,
stream or seqpacket listener, connected sender or not; an **abstract** name of another type is
`ECONNREFUSED` (111) in every shape, because that table is keyed by the socket's type and the
lookup misses before any comparison. Served by `send_dgram`'s path branch (type compared after
the liveness upgrade) and `connect_to`'s datagram arm (`WrongType` instead of `Refused`). Pin:
`t-unix-dgram`, 49 -> 57 claims (two divergence claims become `EPROTOTYPE`, four new path/
abstract claims plus four setup claims), green on a host first. Gate: x86_64 `pass=193 fail=0`;
aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (old rule restored): x86_64
`pass=192 fail=1`, exactly the four path claims red with `ECONNREFUSED` (111) where the pin wants
`EPROTOTYPE` (91), abstract half green, sweep completes.

**A seqpacket socket with no peer sends nothing, whatever the name says: `ENOTCONN`**
(`docs/uapi/` §5.1 item 81, landed 2026-10-20): item 79's other named residual. An unconnected
or listening `SOCK_SEQPACKET` socket has message doors but no peer; the doors recognised only
sockets that carried messages, so the shape fell through to `ENOTSOCK` (88), a named `sendto`
included. Measured whole on a current host first
(`.workbuddy/probe_unix_seqpacket_unconnected{,_msg}.c`, Linux 7.2.11, unconnected and
bound+listening alike): `write`, `sendto` with no name, and `sendto` naming a live same-type
listener, a dead path, a regular file, a family-only address or a live abstract name all answer
`ENOTCONN` (107) - the name is not looked at, and a NULL pointer with a length is still "no
name"; `sendto` still refuses 129 bytes (`EINVAL`) and a bad pointer (`EFAULT`); `sendmsg`
imports the header, name bytes, iovec array and control buffer (`EFAULT`), without bounding
`msg_namelen` to 128 and without reading the iovec data, so a bad iovec base is `ENOTCONN`.
Served by `UnixSock::unconnected_seqpacket` and a branch in each message door. Pin:
`t-unix-seqpacket`, 69 -> 88 host-run claims, green on a host first. Gate: x86_64 `pass=193
fail=0`; aarch64 `pass=192 fail=0`; `mpos-kunit` 32/0 on both. Counterfactual (branch removed):
`pass=192 fail=1`, exactly the 17 message-door claims red with `ENOTSOCK` (88), `write` still
green, sweep completes.

**A descriptor's filesystem is the one it was opened on, not the root's** (`docs/uapi/` §5.1
item 89, landed 2026-10-21): `fstatfs(2)` answered by inode identity - a mount's own root inode
named that mount and everything else fell through to `/` - so every regular file, every directory
below a mountpoint and every `/proc/<pid>` file reported the root filesystem while `statfs(2)` on
the same path reported the mount that serves it. Two doors, one file, two answers, and `df` read a
mount's free space as the root's. Measured whole on a current host first
(`.workbuddy/probe_fstatfs.c`, Linux 7.2.11): Linux asks the vfsmount the open took, so `/` and
`/etc/hostname` both answer the machine's ext4, `/proc` and `/proc/self/status` both answer procfs,
`/dev/shm` and a file written into it both answer tmpfs, and `fstatfs(fd)` equals `statfs(path)` in
every row. The door now keys on the device number the open recorded (`File::dev`, the record
`fstat`'s `st_dev` already answered from, which is why *that* field agreed between the two doors
while nothing else did), and `MountTable::point_of_dev` walks `mount_points` - the table
`serving_point` reads forward - taking the deepest record carrying it, so the reverse lookup is the
forward one's mirror rather than a second rule that can drift from it. Pin: `t-statfs`, 24 -> 26
claims green on the host first and byte-identical on both machines; counterfactual (the inode-based
body restored) is `pass=204 fail=1` with exactly those claims red and everything else green. The
first draft's `/tmp`-shaped claims were **dropped rather than kept**: the first machine mounts a
ramfs at `/tmp` over a ramfs root, so they were green with the fix reverted too - a claim answered
alike either way is not evidence, and what replaced them compares the mountpoint's descriptor with
the one for a file inside it, which catches the old answer on either machine.

**Every mount now carries an `f_fsid` of its own instead of a zero they all shared** (`docs/uapi/`
§5.1 item 90, landed 2026-10-21): `f_type` names the *kind* of filesystem - two ramfs mounts, or
two ext4 mounts, share it - so a caller comparing two mounts through `statfs(2)` had nothing to
compare, and `f_fsid` is the field Linux's own man page offers for exactly that. This kernel wrote
zeros. Measured whole on a current host first (`.workbuddy/probe_fsid.c`, Linux 7.2.11):
`sizeof(struct statfs)` 120, `f_fsid` at 56, every row non-zero, `fstatfs(fd) == statfs(path)`
throughout - and the values split two ways, which is the whole constraint: `/proc` answers `0x18`,
**exactly its own `st_dev`** through Linux's `new_encode_dev`, while `/`, `/sys` and `/tmp` answer
UUID hashes that differ per machine and per boot. So the item pins the structure (non-zero, shared
by every name one instance serves, different between two live mounts) and says plainly that it does
not pin a value. The landing gives the field one source rather than a second identity beside
`st_dev`: `abi::encode_fsid` spells `new_encode_dev` (checked against a C program first), and both
doors feed it the device number they already report - `File::dev` for `fstatfs`, `dev_of_path` for
`statfs` - so the field and `st_dev` cannot disagree about which filesystem answered. Pin:
`t-statfs`, 26 -> 36 claims, green on the host first and byte-identical on both machines; the new
ones read a caller-owned 120-byte buffer by offset because neither wrapper exposes the field.
Counterfactual (the field cleared again) reddened the `write_statfs` host unit test first, then
`pass=204 fail=1` with exactly the three non-zero/different claims red - the two *equality* claims
stay green against zeros by construction, and are kept for the half-fix they do catch, not claimed
as the discriminating ones.

**A socket's buffer size is Linux's number, and a datagram queue is the size it names**  
(`docs/uapi/` §5.1 item 58, landed 2026-10-18): the pair the status board carried as  
record-only was wrong in both halves - the figure stored was the caller's rather than Linux's,  
and the figure governed nothing. Measured whole on a current host first  
(`.workbuddy/probe_sockbuf.c`, Linux 7.2.11): `setsockopt` stores **twice** the ask and  
`getsockopt` reports it (2304 -> 4608, 10000 -> 20000, 100000 -> 200000), each option has its  
own floor applied to the doubled figure (`SO_SNDBUF` 4608, `SO_RCVBUF` 2304 - asking 2048 for  
the receive side reads 4096, the doubling again above half the floor), the ceiling is the  
sysctl maximum applied to the *ask* (ten million reads back twice four mebibytes), and a  
negative ask is not an error but "as much as allowed" (`-1` answers 0 and reads 8388608). A  
fresh socket reports its family's own default - 212992 for both datagram options, 16384 and  
131072 for a stream. The governing half is measured too, with the dead-neighbour fill: 1400-  
byte datagrams stop after 2 at the floor, 4 at 9216, 9 at 20000 and 57 at 131072, while a  
5600-byte datagram is accepted into a socket reading 4608 - so the buffer bounds the queue and  
is not the datagram line. `net::sock_buffer_size` is the rule, `udp_send` asks it before the  
enqueue and `udp_room_for` asks it again (the park's predicate and the refusal must be one  
question), the datagram ring rose from 4 KiB to 16 KiB and from four packet slots to sixteen so  
that the buffer can be the smaller number, and the grace for an empty queue is Linux's. Pins:  
`t-sockbuf` (a new program, 12 claims, green on a host and on both machines) and `t-udpblock`'s  
fill, which now stops where the buffer's number says. Named at the time: `SO_RCVBUF` is a number  
and not a window (the receive ring is a fixed 16 KiB - a stream's became its window in item 98,  
a datagram's is still fixed), the datagram-too-big line is still the ring's rather than 65507  
(no IP fragmentation), and a floor buffer admits three here against Linux's two (payload  
accounting against skb truesize).

**Giving the terminal back is the session's act, and only the session's** (`docs/uapi/` §5.1  
item 57, landed 2026-10-18): `TIOCNOTTY` is the residual item 56 named and did not serve. Linux  
answers two different things about *who* asks, measured whole on a current host first  
(`.workbuddy/probe_tty_notty.c`, six scenarios, each in a session of its own): a session leader's  
call gives up the **session's** terminal - its foreground group hears a `SIGCONT` and a `SIGHUP`  
(in that order, and again with the group already in front so no `TIOCSPGRP` could have sent the  
`SIGCONT`), the caller itself hears nothing when it is not in that group, the terminal keeps no  
foreground, `/dev/tty` is `ENXIO` for every task that still named it, a second call is `ENOTTY`,  
and the pair is **not** hung up (a `read` answers `EAGAIN`, a `write` lands) - while any other  
member's call is `0` too and clears only its own record, leaving the leader's `/dev/tty` opening  
and the terminal still naming the session's group. A descriptor that is not the held terminal is  
`ENOTTY`: the master end of the pair (a different terminal from the one `TIOCSCTTY` bound), a  
process with no controlling terminal, and a leader that never claimed one. The arm is `0x5422`,  
`sched::release_controlling_tty` does the writing (session-wide for the leader, caller-only  
otherwise), and `signal::SIGHUP` joins the vocabulary. Pin: `t-pty`'s fourth section, 18 claims  
(67 printed in the file, was 49); counterfactually six of them go red by name and the two  
refusal rows stay green.

**A terminal's foreground group is the session's, and a reader is not its owner** (`docs/uapi/`  
§5.1 item 56, landed 2026-10-18): the last divergence §5.2's terminal entry carried. Linux keeps a  
tty's foreground group strictly as *session state* — set when a session leader makes the tty its  
controlling terminal (`__proc_set_tty`) or by `TIOCSPGRP` from that session, and never by a read —  
while this kernel answered with the reader: `Tty::read` recorded the reading task's group as the  
terminal's foreground, so a program that read a tty it never claimed *became* its owner (the typed  
`SIGINT`/`SIGQUIT`/`SIGTSTP` and the resize `SIGWINCH` reached it, and `tcgetpgrp` named it), and  
a pair no session held answered `0` and accepted any group from anyone. Measured whole on a current  
host first (`.workbuddy/probe_tty_fg.c`, `probe_tty_fg2.c`, Linux 7.2.11, every scenario in a  
session of its own): a pair nobody holds answers `ENOTTY` on the *slave* and `0` on its *master*  
(Linux asks the controlling-terminal question of the slave end only, which is what lets a  
supervisor holding only the master learn whether it may do job control), `TIOCSPGRP` is `ENOTTY`  
there and from any process in another session, and from the owning session the refusals are ordered  
and distinguishable — `EFAULT` for a bad pointer, `EINVAL` for a negative number, `ESRCH` for one  
that names no process, `EPERM` for a group in another session, with the group it would not take  
left where it was. Reading is not claiming: a process in another session reads the slave and the  
answer does not move. Two shapes stay named rather than built, both measured: a background group  
that tries to change the terminal is `ENOTTY` when its group is orphaned and served when `SIGTTOU`  
is ignored (this kernel sends no terminal-generated `SIGTTOU`), and a background *read* of the  
terminal's own session is `SIGTTIN`'s business. `t-pty` gains 17 claims (54 in the file, was 37),  
all green on a current host first; the counterfactual puts back the two old halves and turns  
exactly nine of them red, including the reader's.

**A `poll` with no descriptors is a wait, and `pause()` is that call on arm64** (`docs/uapi/` §5.1  
item 55, landed 2026-10-18): a zero-`nfds` `poll`/`ppoll` answered `0` at once where Linux waits  
out the timeout (`poll(NULL, 0, 100)` answers after 100.1 ms; with no timeout it waits for ever).  
It mattered because musl builds `pause()` as exactly that call on aarch64 - Linux has no `pause`  
number there - so a program idling in `pause()` returned immediately and spun. Found by item 54's  
fixture, whose parked child exited at once on the second machine while every `Rss` in its file  
read zero.

**`/proc/<pid>/smaps` is served, and the three numbers this kernel cannot derive are a rule**  
(`docs/uapi/` §5.1 item 54, landed 2026-10-18): each record's header is that mapping's `maps` line  
byte for byte (both renderings of one gatherer, `procfs::mapping_lines`), then Linux's twenty-four  
field lines in Linux's order. `Pss`, the shared/private resident buckets and `Referenced` are the  
three this kernel's model cannot answer - no mapcount, no dirty bit with a page cache, no accessed  
bit - and each reads what the model does support rather than a fabricated number.

**A wait's two writes go where Linux's go — the wait, the rusage, then the record** (`docs/uapi/`  
§5.1 item 51, landed 2026-10-18): item 34 gave both wait doors the caller's `struct rusage`; what it  
left was *where* the writes sit, which only a caller with a pointer the kernel cannot write to can  
see. Linux collects (or refuses) first, copies the rusage when it has an event to report, and writes  
the six-word `siginfo_t` last on every path — so a bad pointer costs the caller its `EFAULT`  
*after* the child is gone, a refusal still writes the record (six zeros, and nothing else: the pad  
at 12 and every byte from 28 on stay the caller's), and the record's own `EFAULT` outranks `EINVAL`  
and `ECHILD`. This door cleared 128 bytes up front and refused before the wait, which is exactly the  
two divergences the entry named — a zeroed record where Linux leaves the caller's bytes, and a child  
still uncollected. The handler is now a collector that touches no pointer plus two writers in  
Linux's order; the record is written word by word (a record whose tail is unmapped is still served,  
and one whose word straddles the boundary stops there with the words before it written), and the  
rusage goes through the page-by-page copy that is `copy_to_user`, so a straddling one leaves its  
head written. That door's `*status` was already in this order, and the `wait4` four-byte word it left  
named in `docs/uapi/` §5.2 was measured rather than assumed (`docs/uapi/` §5.1 item 53, landed  
2026-10-18): Linux's `put_user` there is **atomic** — a word that straddles the page end is `EFAULT`  
with not one byte written, where the record beside it is the range copy that leaves its head behind —  
so this door's whole-word check followed by one store was already the right shape and no kernel code  
moved. The fourteen shapes are claims in `t-wait4` now (33 new, 84 in the file), which is what keeps  
a later "just copy four bytes" from landing quietly.

**`__WCLONE` narrows a wait to the kind of child `clone` named** (`docs/uapi/` §5.1 item 50, landed  
2026-10-18): `clone`'s low byte is the signal a child's death should report, and Linux reads it back  
at the other end — a child born to report `SIGCHLD` and one born to report nothing or something else  
are two kinds, and `wait4`/`waitid` only answer about the kind the option word admits (`__WCLONE` the  
clone kind alone, `__WALL` both, neither the non-clone kind alone). This kernel accepted the bit and  
reported every child, so a clone child was a child Linux says the caller does not have. The byte now  
survives on the task (and on the retired record, for the wildcard wait), and `sched::ChildKind` rides  
in the one eligibility rule every scan asks, so the seven places that look for "my children" cannot  
disagree. `fork`/`vfork` are `clone(SIGCHLD)` and unchanged, which is why every existing wait answers  
as it did. The narrowing is `ECHILD` — not `WNOHANG`'s 0 and not a park — so the blocking shape is  
pinned too. One thing stays named in `docs/uapi/` §5.2 — `__WNOTHREAD` is still
accepted-and-ignored — and the byte's *delivery* half, item 50's other end, **landed 2026-10-18**
(`docs/uapi/` §5.1 item 59): a child's death now carries the signal its `clone` byte named rather
than a constant `SIGCHLD`. The pin also caught a bug beside it: both doors read `options` as a  
whole register, where Linux declares it an `int` — so a C caller's `__WCLONE` (bit 31, sign-extended)  
was `EINVAL`, and the word is narrowed to 32 bits now.

**A `clone` child's own exit signal is what its death reports** (`docs/uapi/` §5.1 item 59, landed  
2026-10-18): item 50 read `clone`'s low byte at the *waiting* end; this is the other end — Linux's  
`do_notify_parent` sends the byte's own signal, so a `fork`'s child reports `SIGCHLD`, a  
`clone(SIGTERM)` child reports a `SIGTERM` with the child's pid, real uid and exit code, and a  
`clone(0)` child reports **nothing**, where this kernel raised `SIGCHLD` through every door. Measured  
whole on a current host first (`.workbuddy/probe_exitsig.c`, Linux 7.2.11, one scenario per  
disposition, the byte the only variable): the delivery predicate is not what either half of it  
suggests — `SIG_IGN` **drops** a non-`SIGCHLD` byte even when blocked (the one place "a blocked  
signal is never ignored" does not hold) while a blocked `SIGCHLD` is queued, and the set of exit  
signals Linux ignores by default is exactly `sig_kernel_ignore` (`{SIGCHLD, SIGURG, SIGWINCH}`), so  
a `clone(SIGURG)` death is dropped at `SIG_DFL` unblocked while a `clone(SIGTERM)` death at `SIG_DFL`  
unblocked **kills** the parent — and `SIGCONT` is *not* in it, which only its own probe can show (a  
`clone(SIGCONT)` child's death resumes a **stopped** parent; `probe_exitsig_cont.c`). Served by  
`sched::notify_parent_of_death` (the rule and the measurement) beside `signal::ignored_by_default`;  
pinned by the new program `t-clone-exit` (41 claims now — 29 here, plus item 63's twelve — green on  
a host first, byte-identical on both machines). The wake in the death path is unconditional and stays  
so — a parent parked in `wait4(__WCLONE)` for a `clone(0)` child is woken by nothing else. The  
residual this paragraph used to name is closed: **the record's own byte as a *report* landed  
2026-10-19** (`docs/uapi/` §5.1 item 63) — `/proc/<pid>/stat`'s field 38 carries the task's own  
byte, and `-1` for a thread.

**A read of a watched file reports `IN_ACCESS`** (`docs/uapi/` §5.1 item 60, landed 2026-10-19):  
the inotify module's own header had carried `IN_ACCESS` as a deliberate non-event since it landed,  
on a rationale that had gone stale — "the read path does not bump atime" — while the ext4 read path  
had since grown the `relatime` bump, and a read is an access whatever the stored time then says.  
Linux's `fsnotify_access` sits at the syscall boundary, not in the VFS, because the kernel's own  
readers (`read_file`, the `execve` ELF load) reach `File::read` too and `kernel_read` reports  
nothing. Measured whole on a current host first (`.workbuddy/probe_inotify_access.c`, Linux 7.2.11),  
one `IN_ACCESS` per successful read-family or copy call that moved bytes — `read`/`pread`/a  
two-iovec `readv` (one, not per iovec)/`sendfile`/`copy_file_range`/file→pipe `splice` — with  
nothing for a read at EOF, a zero-length read, a failed read, a directory read, or an `mmap` page  
fault, and `IN_MODIFY` (no `IN_ACCESS`) for the pipe→file `splice`. The owner is the syscall layer  
(`syscall::linux::fs::file::notice_access`, one helper the three read doors and the three copy doors  
ask) over `vfs::inotify::emit_access`; pinned by `t-inotify`'s 22 new claims (24 → 46, byte-identical  
on both machines) beside the host unit test. The shape it named as still open — a write made by  
`copy_file_range`/`splice` invisible to `IN_MODIFY` — is closed by item 61 below.

**A write of a watched file reports `IN_MODIFY`, once per call** (`docs/uapi/` §5.1 item 61, landed  
2026-10-19): the write half of the same rule, and item 60's own named residual. A `copy_file_range`  
or a pipe→file `splice` wrote through `Inode::write_at`, below the `File::write` chokepoint that  
reports, so a watcher on the destination heard nothing — and the `File::write` comment claiming every  
copy door reached it "so the event is said once" was false for two of them. Measured whole on a  
current host first (`.workbuddy/probe_inotify_modify.c`, Linux 7.2.11): one `IN_MODIFY` per call,  
however many pieces carried it — a two-iovec `writev`/`pwritev`/`pwritev2` reports one (not per  
iovec), a 10 KB `sendfile` (three chunks) one, a 10 KB `copy_file_range` one on the destination while  
its source reports none, and a pipe→file `splice` one. The fix is a `write_quiet` on `File`  
(`remove_privs` still runs, only the report is held) the vectored and copy doors write their call  
through, each saying the one event itself from the syscall layer's `notice_modify(fd)`, the mirror of  
`notice_access` (`handle_writev`, its body moved to `writev_inner`, plus `handle_sendfile`,  
`handle_copy_file_range`, `handle_splice` when the output is not a pipe). Pinned by `t-inotify`'s 21  
new claims (46 → 67, byte-identical on both machines) beside the host unit test. The module's other  
two departures (`IN_UNMOUNT`, a watched directory's `IN_MOVE_SELF`) stay named in `docs/uapi/` §5.2.

**`numa_maps` and `smaps_rollup`: the two other spellings of `smaps`' accounting** (`docs/uapi/`  
§5.1 item 62, landed 2026-10-19): `/proc/<pid>` had `maps` and, since item 54, `smaps` — and  
neither of the two files `docs/uapi/` §5.2 had already measured, so `numastat -p`, `numactl` and  
every reader that wants a total rather than a walk found no file. Measured whole on a current host  
first (`.workbuddy/probe_numamaps*.c`, `probe_rollup.c`, Linux 7.2.11, one node), with two probes'  
worth of care about the fixture: **`/tmp` is tmpfs here**, so a "file" mapping made there is a shmem  
mapping whose folios are dirty from the moment the probe wrote them (a read-only mapping reported  
`dirty=4`), and **adjacent anonymous mappings merge**, so two shapes became one VMA and one of them  
looked absent — every shape is fenced by `PROT_NONE` pages now. `numa_maps` is one line per mapping  
in `maps`' order (the gate VMA excepted, and that is Linux's own: `[vsyscall]` is not a VMA of the  
address space): the **start alone, with no `-end`**, the policy — `default`, since `set_mempolicy`  
is not served — and then the name, which is `file=<path>` (a space `\\040`, a tab `\\011`), `heap`,  
`stack`, or nothing at all for a name that is not a path. A range with no resident page prints the  
policy and **no number at all**, not `N0=0`. `smaps_rollup` is one record for the whole address  
space: a header that is the **bounds** of the space — `<first start>-<last end> ---p 00000000  
00:00 0`, `[rollup]` in the same column 73 a `maps` line names its mapping — then twenty-two field  
lines, which is `smaps`' twenty-five less the three per-mapping sizes and the three per-mapping  
flags, plus `Pss_Anon`/`Pss_File`/`Pss_Shmem`. Its numbers are the sums of `smaps`' (`Rss` exactly,  
2096 and 2096; `Pss` not — 278 against 281, because each record's is rounded before it is added).  
Both renderers sit over the records `maps` already gathers and `resident_lines` is the one place  
the residency is counted, so the three files cannot disagree. Pins: `t-numamaps`, a new program, 20  
claims, and `t-smaps`' ten more (33 → 43) — all green on a current host first and byte-identical on  
both machines; the counterfactual (the two nodes unregistered) turns sixteen of them red by name.  
Named rather than approximated: `mapped=` and `mapmax=` are absent (no mapcount, the fact `Pss`  
already names), the `stack` word is absent (this kernel names the heap only), a read-only  
object-backed range says `dirty=N` where Linux says `mapped=N` (no dirty bit), and `Pss_Shmem` is  
zero (a `memfd`'s pages are an object's here). One divergence runs the other way and is worth its  
own sentence: **reading an anonymous page gathers nothing on Linux** — the read fault is satisfied  
with the shared zero page — where this kernel materialises a frame and reports it.

**`/proc/<pid>/stat`'s `exit_signal` is the task's own byte, and a thread's is `-1`** (`docs/uapi/`  
§5.1 item 63, landed 2026-10-19): the field item 50 and item 59 both named and left standing ("a  
rendering nothing pins"), and the last named thing on the `clone` byte's ladder. Linux stores the byte  
`clone` carried on the task, rewrites it to **`-1`** for a task created with `CLONE_THREAD` (a thread  
never notifies anyone of its death), and prints what it stored — so the field is a *signed* word  
carrying a number no signal byte can. Measured whole on a current host first  
(`.workbuddy/probe_exit_signal_field.c`, Linux 7.2.11), the byte the only variable: a `fork`'s child  
`17`, a raw `clone(0)` child `0`, a `clone(SIGTERM)` child `15`, a **thread** `-1` (the same answer  
its `task/<tid>` spelling and, on Linux, the tid's own `/proc/<tid>/stat` give), the *leader* of a  
threaded process its own `17`, and a **zombie** keeps the byte it died with. This kernel answered the  
constant `17` through every record, so a supervisor reading a `clone(0)` child's record or a thread's  
was told a `SIGCHLD` no death of theirs would ever deliver. The report is *derived* rather than read  
off the stored byte, because this kernel's threads deliberately store `SIGCHLD` (the one place a  
thread does leave a record is its group's `Retired`, and a default `wait` that named that slot must  
not read it as a "clone" child's) — so `vfs::procfs::exit_signal_field(byte, is_thread)` is the rule  
and `sched::TaskSnapshot` carries the byte: one field, one predicate, one rendering. The file is  
`stat` and not `status`: the two paragraphs that named this said "`/proc/<pid>/status`'s  
`exit_signal` line", and Linux's `status` has **no such line at all** (measured in the same run),  
which is corrected here and in `docs/uapi/`. Pin: `t-clone-exit`'s 12 new claims (29 → 41 — each  
record read from a **live** child, the leader's control, the thread's `-1`, and the zombie's  
state-and-byte pair) plus the host unit test  
`a_tasks_exit_signal_field_is_its_byte_or_minus_one_for_a_thread`, which needs no machine under it.  
The named shape this paragraph used to carry — a thread has no `/proc/<tid>` directory — is served  
now, by item 64 below.

**A thread's tid names a `/proc` directory, and a dead thread names nothing** (`docs/uapi/` §5.1 item  
64, landed 2026-10-19): the gap item 63's pin found and §5.2 carried, and the last name a thread's  
record had. This tree's `/proc` directory was a *leader*'s (`pid_exists` asked `snapshot(pid).pid ==  
pid`), so `/proc/<tid>` was `ENOENT` and a thread's record was reachable through  
`<pid>/task/<tid>/stat` alone. The question §5.2 said to settle first — which of the two names a  
*program* uses — is measured, on a current host under `strace` (`.workbuddy/probe_tid_dir.c`, Linux  
7.2.11): `ps -eLf` walks the group, with **306** opens of the *relative* name `task` beside not one  
absolute `/proc/<pid>/task/<tid>` path, while `gdb -p <tid>` opens **`/proc/<tid>/status`** outright —  
so the `task/` spelling is `procps`'s and the tid's own is a debugger's. Measured whole in the same  
probe, a live thread of a live group: the tid's directory carries the **process** file set — its names  
are the leader's exactly, 54 against 54 — where the `task/` spelling is the narrower *thread* set  
(47: `children` in, eight process-only names out), which is Linux's own  
`base_stuff`/`tid_base_stuff` split; `readdir /proc` still lists no tid; `stat`'s field 1 is the tid  
and field 38 `-1`, where the leader's record says the pid and its own byte; `status` is the pair  
(`Tgid` the group, `Pid`/`NSpid` the thread); and the *task's own* structures are what the tid's  
directory reads — a thread that `unshare(CLONE_FS)`s and `chdir`s reports its own `cwd` there while  
its leader keeps the group's, and one that `unshare(CLONE_FILES)`s lists its new descriptor under  
`/proc/<tid>/fd` alone, which is the half a leader-shaped answer gets wrong. A lookup also has to be  
Linux's gate rather than merely a wider one, so the measurement came with a clause: a task is  
unhooked from the namespace it was numbered in **at its exit** unless it leads its group  
(`__unhash_process`), so a dead thread is out of `/proc/<pid>/task` and its tid names nothing —  
`vfs::procfs::tid_has_a_directory`, one clause, asked at the two funnels that *name* a task  
(`process_dir` and `thread_ids`), because this kernel's table holds a dead thread's *slot* until the  
slot is reused. Pin: **`t-tid-dir`**, a new program, **26 claims**, green on a current host first  
(26 of 26, five identical runs) and byte-identical on both machines, plus `t-clone-exit`'s one more  
(41 → 42 — item 63's `-1` is now read through *both* spellings) and the host unit test  
`a_dead_threads_tid_names_nothing_and_a_zombie_leaders_does`. Gate: x86_64 `pass=188 fail=0 (188  
built)`, aarch64 `pass=187 fail=0`, `mpos-kunit 32 passed, 0 failed` on both. Counterfactual (the  
lookup back to `pid_exists`, the filter dropped): stage 1 untouched at 578, the sweep  
`pass=186 fail=2 (188 built)` with 18 red claims by name, and the one that stays green either way is  
the tid-names-nothing claim — it is the *live* readings that carry the item. One thing stays named in  
`docs/uapi/` §5.2: `children` is the *task's* child list on Linux and the process's here, measured —  
a child forked by a non-leader thread appears under that thread's `task/<tid>/children` alone.

**A task's `children` are its own, and the parent a record names is its group** (`docs/uapi/` §5.1  
item 65, landed 2026-10-19): item 64's residual, and the last word on *which task* a record points  
at. `children` is reached through the *group* — `/proc/<pid>/children` and `/proc/<tid>/children` are  
both `ENOENT` even for a task that has children, because the name is the thread set's alone — and it  
lists the children **that task** forked, where this kernel answered the process's through every tid's  
file. The same probe settled the link's other half, which no entry had measured: a child forked by a  
non-leader thread is named to its **group** — `status`'s `PPid`, `stat`'s field 4 and `getppid` all  
read the group's number (`.workbuddy/probe_children_owner.c`, Linux 7.2.11) — where this kernel named  
the forking thread's tid. `vfs::procfs::child_slots` is the rule (keyed on the link, leaders only, a  
corpse included until the reap), `children_of` writes Linux's own bytes (`<pid> ` per child, a  
zero-byte file for none) and asks the *task's* directory clause as its gate, which retired  
`pid_exists`, `sched::TaskSnapshot` carries the link beside a `ppid` that is now the parent's group,  
and `getppid` asks the new `sched::parent_group_of`. The fixture itself then found **two kernel  
defects**, both fixed here and both Linux's answer: `fork` from a non-leader thread answered  
`ENOMEM` (it asked the calling task's `space`, which a thread does not own — the group's is the  
leader's slot), and a reaped child lingered in the table until the next scheduler `reap()`, where  
Linux's `release_task` runs inside `do_wait`. Pin: `t-tid-dir`'s **11 new claims** (26 → 37),  
green on a current host first (37 of 37, three runs), plus the host unit test  
`a_children_file_lists_the_children_the_task_itself_forked` (the gate figures are §15.84's). One  
thing stayed named in `docs/uapi/` §5.2: a **thread's** own parent — this kernel's thread carried  
no link at all (`parent: None`, so its three doors read `0`), where Linux gives it its group's  
parent (`CLONE_THREAD` implies `CLONE_PARENT` — measured, a thread reads the group's parent  
through all three doors). It landed the next day as item 66 (§15.85), with the behaviours a link  
change moves: a thread's death notifies nobody, a task's exit hands its children to an alive task  
of its group, and `wait` walks every thread of the caller's group.

**The `F_GETFL` word is the caller's own** (`docs/uapi/` §5.1 item 49, landed 2026-10-18): Linux  
answers `filp->f_flags`, so a caller that compares the word against what it opened with agrees — its  
mode, `O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_SYNC`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` — where  
this kernel answered the mode plus `O_APPEND` and `O_NONBLOCK` and nothing else. The open now records  
the word it was asked for (`File::born`), minus exactly what the open *spends* (the creation flags  
and `O_CLOEXEC`, which is a descriptor flag), and `F_SETFL` still reaches only its own two bits — the  
split Linux's mask makes rather than a rule of this kernel. Two members of Linux's word stay  
deliberately missing and named in `docs/uapi/` §5.2: `O_LARGEFILE`, which this kernel neither defines  
nor invents, and the three bits its `SETFL_MASK` is narrower by (`O_ASYNC`, `O_DIRECT`, `O_NOATIME`  
are reported when an open asked for them but cannot be set afterwards).

**The device tree removes a directory it made** (`docs/uapi/` §5.1 item 48, landed 2026-10-18): the  
`mpos: syscall 84 (path /dev/dri/by-path) -> EOPNOTSUPP` a boot printed once per boot was udev's  
per-boot idiom — remove that directory, then publish the `by-path` links under it again — meeting a  
filesystem that had no `rmdir` at all and an `unlink` that took a directory as readily as a device  
node, so the two doors were not a split. Both nodes own the door now, with Linux's order in front of  
the name going (`ENOTDIR` for a kind that is not a directory, `ENOTEMPTY` for one that still holds  
something), a refused `rmdir` therefore takes nothing away, `unlink` answers `EISDIR` where it used to  
remove, and the mount point's `EBUSY` was already the mount layer's (item 45). The boot that logged  
the line is quiet now and its `by-path` links are published. One shape stays named in `docs/uapi/`  
§5.2: a driver's own node cannot be re-made through `mknod` here — the device tree's names come from  
the drivers.

**A directory is not a byte stream** (`docs/uapi/` §5.1 item 47, landed 2026-10-18): its four read  
doors answer `EISDIR` — the *kind* before the *count*, so even a zero-length read of one is refused  
where the same count on a file is `0` — where this kernel answered `EOPNOTSUPP`, and `sendfile`'s  
*source* question answers `EINVAL` beside it, a different word for a different reason (the copy is  
a splice, and a directory has none). The write half needs no word of its own: a directory cannot be  
opened writable at all, and that refusal is where its `EISDIR` comes from.

**A FIFO is a channel now, not a file** (`mknodat(S_IFIFO)` served 2026-10-16, `docs/uapi/` §5.1  
item 26): its `open` runs Linux's **rendezvous** — a reader and a writer meet at the name, a  
peerless open parks (or is `ENXIO` under `O_NONBLOCK`), a parked reader counts as a reader so the  
arriving writer releases it, and the buffer is discarded when both counts reach zero — and the one  
shape that open used to leave behind is closed too (`docs/uapi/` §5.1 item 28, landed 2026-10-17): a  
**fatal** signal now *ends* the park instead of ending the task inside it, so the count comes back  
before the death, which happens at the syscall gate — Linux's own order, where `err_rd:` runs on  
`ERESTARTSYS` and the task dies after the syscall has unwound. `mknod`'s `dev` argument **carried no device number until item 71 landed on 2026-10-20** — a node
made here now reports the pair the caller named and `open(2)` of it answers `ENXIO`, no driver stands behind any number `mknod` can be given — this kernel's devices are objects a
driver published under a name, rather than entries in a table keyed by that word.

**A procfs/sysfs node reports its own word, not the kind's default** (`docs/uapi/` §5.1 item 37,  
landed 2026-10-17): both trees are filesystems that keep no modes, so every directory answered  
`0755`, every file `0644` and every link `0777` whatever Linux's creator had said about the node.  
Measured whole on a current host first, the word now follows each node's own *shape* — which turns  
out to be Linux's rule on both trees (`proc_create` with no mode means `S_IRUGO`; `DEVICE_ATTR_RO`  
against `DEVICE_ATTR_RW`): a proc directory `0555`, a file nothing can write `0444`, one that can  
`0644`, `/proc/<pid>/environ` `0400` where its neighbour `cmdline` is `0444`, `/proc/<pid>/ns`  
`0511`, the live `/proc/bus/pci/<bus>/<dev>.<func>` window `0644`, `/proc/cmdline`'s `st_size` its  
own line's length, `/sys` itself `0555` while every directory under it is `0755`, a read-only  
attribute `0444` and a writable one (`uevent`, the door `udevadm trigger` writes) `0644`. Three  
things stay named in `docs/uapi/` §5.2: the read-only nodes Linux makes writable, the fact that a  
word here is a *report* rather than a gate (procfs polices nothing), and the three names whose  
*kind* is a directory here where Linux answers a link. The same pass' measurement turned up a fourth  
divergence of its own, now named in §5.2: the no-follow resolution splits the last name off its  
parent, so the final component's mount crossing never happens and `lstat("/proc")` answers the  
directory the mount covers.

**A device node asks `CAP_MKNOD` now** (`docs/uapi/` §5.1 item 36, landed 2026-10-17): exactly the  
two device types do — `S_IFREG`/`S_IFIFO`/`S_IFSOCK` nodes are served to a caller holding not one  
capability — in Linux's place in the order (after the type, the name, the name's existence `EEXIST`  
and the directory's own `EACCES`), with the character node whose `dev` is `0` served too, because  
that is how overlayfs spells a whiteout. The corner this paragraph used to name — Linux asks  
`capable(CAP_MKNOD)`, which is the **initial** user namespace, where this kernel's checks asked the  
caller's own — **closed 2026-10-17** (`docs/uapi/` §5.1 item 38): the capability model now carries  
the two questions Linux asks (`capable()` for the initial namespace, `has_effective()` for the  
caller's own), and the four gates whose Linux rule is `capable()` ask it — `mknod`, `FS_IOC_SETFLAGS`  
at the driver's/`fileattr_set`'s door, `reboot` and `syslog` — measured one namespace at a time  
(a user-namespace root is served `chroot` and `unshare(CLONE_NEWNS)` and refused those four). Two  
scopes the one word cannot express stay named in §5.2 with their measurements, and both landed on  
2026-10-18: `setns`'s and `AF_PACKET`'s ask the *object's* namespace (§5.1 item 42, the paragraph  
below), and `CAP_DAC_OVERRIDE` asks the *file's* filesystem's —
**whose DAC half landed the same day** (`docs/uapi/` §5.1 item 39): the permission model's override  
is now Linux's two capabilities (`CAP_DAC_OVERRIDE` for a read and a write on a non-directory  
whatever its bits are, `CAP_DAC_READ_SEARCH` for a read on its own, execute only where some execute  
bit exists) asked of the file's namespace through `Credentials::capable`, which is where the two  
passes meet. **And the *owner-shaped* calls landed on 2026-10-18** (`docs/uapi/` §5.1 item 40):  
`chmod`, `chown` and the `utimes` family ask `inode_owner_or_capable` — the node's own uid, or  
`CAP_FOWNER` — rather than "is the caller root"; `CAP_CHOWN` is what a `chown` naming *another*  
owner asks, and naming the ids the node already holds is still a `chown` rather than a no-op;  
`CAP_FSETID`, not the uid `0`, is what stands in for being in a file's group when a `chmod` would  
keep the set-group-id bit; and the timestamps call turned out to be **two** questions, since write  
permission buys the "both clocks to now" form (`EACCES` when it is refused) and nothing buys an  
explicit pair (`EPERM`). The same pass measured what a `chown` does to the two set-id bits and  
found this kernel's rule wrong in the other direction: they go for **every** caller, capability or  
not — even a `chown` that names nothing at all, and even root holding the whole set — with two  
exceptions, a set-group-id that carries no group execute (a locking mark, not an authority) and a  
directory. **And the same capability's third door landed with it** (`docs/uapi/` §5.1 item 41): a  
**write** to a file costs it the set-uid and set-group-id bits — a write, a `truncate`, an  
`ftruncate` and an `open`'s `O_TRUNC` alike — and `CAP_FSETID` is the only thing that spares them,  
which is the reason a set-uid program cannot be handed over by being edited in place. Measured,  
three things belong to the rule rather than to the caller: a write that writes *nothing* costs  
nothing, owning the file buys nothing (a caller writing another user's world-writable file loses the  
bits too), and a set-group-id that carries no group execute is a mandatory locking mark that no door  
takes — the clause a `chown` honours and a `chmod` does not. Two shapes stayed named in §5.2 that  
night: `fallocate`'s resize and a write through a mapping. **The first of them closed the next day,  
and turned out to be wider than a resize** (`docs/uapi/` §5.1 item 52): `fallocate` is a write even  
when it writes no byte. Linux's `vfs_fallocate` ends **every** successful path in `file_modified`,  
so the set-id bits go and the clocks move for a `KEEP_SIZE` allocation and for a range already  
inside the file exactly as for a grow — measured with the `mtime` pinned to a known second first,  
on ext4 and tmpfs agreeing — while an immutable file refuses the call with `EPERM` before anything  
is allocated and an append-only one is served, which is `may_modify`'s question and not  
`may_alter`'s. This door reached the inode's `truncate` directly and passed none of it. One shape  
stays named: a write through a mapping, which Linux does not strip either.

**And the two object-shaped scopes landed the same night** (`docs/uapi/` §5.1 item 42): a capability
question about a namespace *object* asks that object's owner, not the caller's own standing —
`setns` asks `ns_capable(ns->user_ns, CAP_SYS_ADMIN)` about the namespace being **joined** and
`AF_PACKET` asks the same of the network namespace the socket is opened in, so a caller inside a
bare user namespace holding the whole set *in it* is refused the mount namespace it is standing in
(the no-op every other kind serves, which is what proves the question comes first) and served one
made inside its own. `vfs::ns`'s new owner table records whose each namespace is where it is made —
with a user namespace its own owner, Linux's `ns_common::user_ns` — and `userns::Caller::capable_in`
is the walk: the target is the caller's and the set is the answer; the machine's own reached from
below is not privileged over it; the caller *made* the next one down and holds every capability in
it whether or not its own set carries the bit; otherwise up one rung. That rung's last named gap
was **closed the same day** (`docs/uapi/` §5.1 item 44): a namespace descriptor *is* the namespace
now — the procfs link's followed name denotes a namespace-shaped file (`vfs::ns::NsFile`) holding
the object resolved at the open, so `setns` joins the namespace the descriptor named rather than
wherever its maker stands by then, and `stat`/`fstat` report a regular `0444` file whose `st_ino`
is the namespace's **own** number, which is what Linux's `nsfs` inode is
(`.workbuddy/probe_ns_fd_stat.c`: `lstat` symlink `0777`, `stat`/`fstat` regular `0444` with
`st_ino` `4026531832`, size 0, `read` `EINVAL`, `lseek` `ESPIPE`, a write-ish open `EPERM` for
root and unprivileged callers alike and `O_APPEND` served; `.workbuddy/probe_ns_fd_pin.c`: the
three readlinks `4026531832`/`4026532932`/`4026531832`). `lstat` and `readlink` keep answering
live, because they stop at the link — the split Linux makes.

**And the third raw door was measured and closed with it** (`docs/uapi/` §5.1 item 43): the *same*
question in the door `ping` and a DHCP client take — `AF_INET`'s `SOCK_RAW` — which asked the
caller's own effective set for `CAP_NET_RAW` where Linux asks the network namespace's owner, exactly
as `AF_PACKET` does. The measurement is the other probe in the same three positions
(`.workbuddy/probe_rawip_ns.c`, Linux 7.2.9): the machine's root served, a caller inside a bare user
namespace holding the whole set *in it* refused `EPERM`, the same caller served the moment it has a
network namespace of its own — so the refusal was never about the set. Both raw doors now ask one
helper (`net::socket`'s `net_ns_capable`), because the two are one question and a second copy of it
is a second place to drift; `t-ns-net`'s section walks both doors in one child. Nothing else moved:
the capability is still asked **before** the protocol is judged, and a root caller is privileged
over the machine's own network namespace, so no guest answer that existed before changed.

**A mount point answers the mount, and a name a mount sits on may not be taken away**  
(`docs/uapi/` §5.1 item 45, landed 2026-10-18) — the last named divergence in the filesystem's own  
resolution, and the one §5.2's entry had called "not a two-line fix" because one helper answered two  
questions: the no-follow resolver looked the last name up *in its parent*, so the crossing the  
walking resolver performs on every child it looks up never happened for the final component  
(`lstat("/proc")` answered the ramfs directory the initramfs made, `0755`, where `stat` answered the  
mounted root's `0555`), while the make-or-remove doors want that same covered name — Linux's `rmdir`  
of a mount point is `EBUSY`, not the removal of the mounted root. **Measured whole on a current host  
first**, in nine sections (`.workbuddy/probe_mountpoint_doors.c`, Linux 7.2.9): the reading and  
altering doors *cross* the mount — `lstat`, `stat`, `statx` with and without  
`AT_SYMLINK_NOFOLLOW`, `newfstatat`, `open`, `faccessat2`, `chmod`, `utimensat` — because the  
crossing is not what `LOOKUP_NOFOLLOW` turns off (the flag is about links, and a mount is stepped  
into whatever the flag says); a plain bind covers the crossing up, since a non-recursive bind clones  
no submounts, which is why the crossing has to come from the walk's own state; the make-or-remove  
doors answer `EBUSY` for the name (`rmdir`, `unlinkat(AT_REMOVEDIR)`, `rename`) and `EEXIST` for a  
make (`mkdir`, `mknod`, `symlink`); and the questions ahead of that refusal are ordered — the type  
pair first (`rename(file, mounted dir)` is `EISDIR`, `rename(mounted dir, file)` `ENOTDIR`), then the  
permission half, then the mount point, which is what an unprivileged caller in a `0555` directory  
holding a mount point reads as `EACCES` for all of `rmdir`, `unlink` and `rename`. What landed is  
one mode on the walk (`resolve_walk`'s `LastLink::Follow`/`NoFollow`, which the no-follow resolver  
now *is*) plus one question at the refusal (`is_mount_point`, a fact about the node rather than the  
view, asked last in `may_remove` and on either end of `renameat2`); the old parent-and-lookup body  
stays as `resolve_entry`, which is exactly what the three doors whose question is a *name* want.  
`t-mountpoint` is the new pin, 37 claims on both machines — it mounts its own tmpfs, so it needs no  
fixture from the boot — and `t-proc-modes` now claims `/proc` and `/sys` through both doors (20  
claims, was 18). Two findings stay named rather than served: a `rename` between a bind's own view  
and the outer tree is `EXDEV` on Linux and is still served here, and a *file* mount point — the only  
case where `unlink`'s half of the refusal is reachable — cannot exist in this kernel at all, because  
every mount it can make lands on a directory.

**The three procfs names Linux makes links are links** (`docs/uapi/` §5.1 item 46, landed  
2026-10-18) — the third and last named consequence of item 37, and the one that program's own header  
carried from the other side: `/proc/self`, `/proc/thread-self` and `/proc/mounts` are `0777` links  
on Linux, where this kernel answered a directory (a mirror of the reading process) for the first two  
and a plain `0444` file for the third. **Measured whole on a current host first**  
(`.workbuddy/probe_proc_self_links.c`), and two of its questions are the ones a table cannot guess.  
The target text is the reader's number **in the pid namespace the procfs instance belongs to** — a  
process whose `getpid()` is 1 inside a fresh pid namespace still reads the outer number from the  
machine's `/proc`, and reads `1` from a `/proc` it mounts itself inside it. And the `st_size` follows  
how the name was *made* rather than what it answers: 0 for the two live targets (registered empty)  
and 11 for `/proc/mounts`, the length of `self/mounts`. The node was already here — `ProcLink`,  
whose `0777` mode and live target are this measurement's answer — so the landing is a `registered`  
length, a `fixed_link` helper, the two names built from `link(|| ...)` through the number this tree  
names the reader by, and the deletion of `MirrorDir` with its last two callers; every name reached  
*through* a link (`/proc/self/status`, `/proc/self/fd/N`, `/dev/stdin`) reaches the node it reached  
before. `t-proc-modes` gains 15 claims (35 in the file, was 20), `DT_LNK` in the listing included,  
and all of them pass on a current host first. One residual stays named: this tree's `/proc` names a  
task by the *reader's* pid namespace throughout, where Linux keys an instance to the namespace it  
was mounted for — the same answer for the container case, and different for a task reading a `/proc`  
mounted outside its own namespace.



**A child's death is one `SIGCHLD` record, and it is Linux's** (`docs/uapi/` §5.1 item 29, landed  
2026-10-17): the second book this kernel kept for "an exit whose signal is unread" is deleted — the  
exit queues the real signal with `CLD_EXITED`/`CLD_KILLED` and the status beside it, and a death the  
parent never asked to be signalled for leaves nothing readable (measured on a host first, all four  
dispositions). One semantics stays named there: an **ignored** child is reaped by Linux and kept as a  
zombie here. Three more this paragraph used to name are closed: a record now carries the child's CPU  
in **`si_utime`/`si_stime`** (`docs/uapi/` §5.1 item 33, landed 2026-10-17 — `USER_HZ` ticks, through  
the descriptor, a handler's frame and `sigtimedwait`, while `waitid` carries none, which is Linux's  
own answer), a signalled death is  
now **`CLD_DUMPED` with `WCOREDUMP`** when the signal's default action dumps core *and the child was  
dumpable* (`docs/uapi/` §5.1 item 32, landed 2026-10-17) — a bit that follows the signal rather than  
a file written, which is what the measurement corrected in this kernel's own comment about it — and  
**`wait4`/`waitid` fill their `struct rusage`** (`docs/uapi/` §5.1 item 34, landed 2026-10-17): the  
child's CPU for every event they *report* — a death, a stop and a resume alike, and not for the  
`WNOHANG` "nothing yet" zero — with Linux's own order on a bad pointer (`wait4` writes `*status`  
first, `waitid` copies the record first), and the last residual of that family named rather than  
approximated in `docs/uapi/` §5.2 (the `waitid` record is cleared up front, so a bad `rusage` leaves  
zeros where Linux leaves the caller's bytes).

One of those "buffer sizes" is served now, and the *blocking* half of it was the last of it: a  
**blocking UDP send waits for the send buffer** (`docs/uapi/` §5.1 item 27, landed 2026-10-16) — the  
wakeup source it was waiting for turned out to be the stack's own poll rather than a TX completion  
interrupt, and what the two earlier shapes got wrong was the *condition* (the metadata ring alone,  
which is true while the enqueue still refuses on the payload ring — a park that returns at once, and  
so spins). Measured whole on a current host first: with the buffer full behind a neighbour that never  
answers ARP, three datagrams go in and the fourth blocking send sleeps 3072-3098 ms and then returns  
it, where the non-blocking one is `EAGAIN` at once. Three things stay named: Linux's `EMSGSIZE` line  
is the maximum UDP datagram (65507), not the send buffer, so a datagram wider than `SO_SNDBUF` is  
taken there and refused here — that is the buffer-size item below, since this kernel's payload ring  
is 4 KiB; the park is **bounded** at three seconds (the window Linux's own neighbour resolution  
takes), because smoltcp keeps a datagram its egress could not dispatch where Linux's device drops the  
frame and releases the charge, so an unbounded park would be a hang; and the ring's wraparound, which  
the condition cannot see and which clears as the buffer drains. The MTU story that program also caught  
is a test-side lesson, not a gap: it lives with §10.4's measurement.

---

*The phase-by-phase effort/impact table this file used to end with is retired: P1–P9's estimates  
predated Phases 12–15 landing, and "Recommended order: P1 → … → P9" orders work the status board  
above marks done. What is open lives in the board and the moves above; `docs/notes/`'s appendix  
carries the per-phase history.*
