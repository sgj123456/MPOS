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
- [ ] [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)

**Networking (Phase 4)**

- [x] 4.1 AF_UNIX — both socket types, with per-message credentials and fd passing; the  
  abstract namespace is served, `unshare`-free lifetime and all (`t-unix`, §4.1.1)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [ ] [~] 4.3 TCP — loopback, listener, `SO_REUSEADDR` and `SO_LINGER{on,0}`'s reset done; buffer sizes record-only (§4.3)
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
- [ ] [~] 6.3 Namespaces — UTS, MNT, **PID**, **NET**, **TIME** and **USER** landed, each with its pin  
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
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
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
- [ ] **the window's other 3840 bytes**: PCIe extended capabilities are outside every reader (§7.6a)

**Wayland compositor (weston) support** — verified by `t-*`; GUI end-to-end is `cargo xtask run --mode gui`:

- [x] peer credentials + fd passing · libinput classification · device discovery · `wl_shm` buffers · framebuffer present · DRM/KMS surface (§4.1, §7.1, §7.2)
- [x] the screen has an owner — `VT_SETMODE` negotiation, revocable DRM master (§7.1, `t-seat`)
- [x] seatd survives a re-run; Mesa GBM+EGL+GLES in software; parked evdev readers woken by the service task; `EVIOCGRAB` per-owner (§7.1, §7.2, R.7)
- [x] the aarch64 GL path's EL0 traps answered — `t-el0-sysreg` (§15.36)
- [x] the session environment — `XDG_RUNTIME_DIR` ships in the shared `core/` overlay, so both machines' login shells have it (R.7's gap, closed by §15.36/§15.37)
- [x] weston's EL0 write fault above its stack — resolved by `resolve_stack_growth` on the aarch64 EL0 data-abort path; the desktop comes up whole (R.7, §15.36)
- [ ] [~] the cursor plane drives a host pointer: hotspot cap accepted, `HOTSPOT_X`/`HOTSPOT_Y` published per-client, and binding/moving posts virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR` on the cursor queue (`t-drm`, `t-drm-props`) — kernel path built and gated green; reading the pointer back on the QEMU window after a gui weston boot is what closes it (§7.4, R.7)
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
- [ ] [~] a framebuffer object's own properties: `OBJ_GETPROPERTIES` answers for one (an empty  
  list, which is what keeps a client walking the card working — niri after kmscube needed it),  
  where Linux publishes `width`, `height`, `format` and `modifier` on an fb (§7.4)
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
- [x] 10.6 serial TX ring · [ ] 10.7 sub-tick sleeps (LAPIC one-shot) (§10.6, §10.7)

**Architectures (Phase 9)**

- [ ] [~] 9.1 aarch64 complete boot — boots real Alpine to a login prompt, every service ok, consoles reach the wire (§9.1, §13)
- [ ] 9.2 RISC-V support (§9.2)
- [ ] [~] 13 Machine-seam purity + aarch64 parity — 13.1 and 13.3–13.31 landed, 13.2 in progress (§13); boot-side half is Phase 15 (§15)
- [x] the boot is one flow — one `start`, one sequence, probes ask both transports (§15.8)
- [x] the kernel travels inside the loader — the ESP carries one bootable file (§15.9)

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
   IPC and CGROUP each wait on the thing they would isolate not  
   existing yet (SysV/POSIX message queues; a cgroup tree), so they are cheap the day that lands  
   and worthless before it. Nested namespaces pin the  
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
   gamma request's `EINVAL`, and the test read it as a refusal to remove the buffer. What remains  
   is the one measurement a `t-*` program cannot make: boot against the window (`cargo xtask run --mode gui`, or `--mode terminal` with root on the serial line as §15.36 drove it), start weston,  
   and read the pointer rendering. The serial log should carry the new `virtio-gpu: … cursorq=1`  
   token — and it does (2026-10-18, the GL card: `virtio-gpu: virgl=1 context_init=1 cursorq=1  
   capsets=2 cached=2 mask=0x6`, weston's `cursor planes: yes` beside it, §7.4). What no session can  
   do yet is *move* a pointer and read it back: `run` wires `-monitor none`, so neither `screendump`  
   nor `input-send-event` is reachable from the host side — the confirmation stays an eye's until a  
   monitor is wired to one. The other residual stays named rather than claimed — the no-cursorq  
   fallback branch, which neither configured machine can exercise (§7.4).
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
the semantics still incomplete are TCP's record-only buffer sizes  
(`SO_SNDBUF`/`SO_RCVBUF`); the two namespace kinds with no entry at all are IPC and cgroup.

**A FIFO is a channel now, not a file** (`mknodat(S_IFIFO)` served 2026-10-16, `docs/uapi/` §5.1  
item 26): its `open` runs Linux's **rendezvous** — a reader and a writer meet at the name, a  
peerless open parks (or is `ENXIO` under `O_NONBLOCK`), a parked reader counts as a reader so the  
arriving writer releases it, and the buffer is discarded when both counts reach zero — and the one  
shape that open used to leave behind is closed too (`docs/uapi/` §5.1 item 28, landed 2026-10-17): a  
**fatal** signal now *ends* the park instead of ending the task inside it, so the count comes back  
before the death, which happens at the syscall gate — Linux's own order, where `err_rd:` runs on  
`ERESTARTSYS` and the task dies after the syscall has unwound. Still unwritten, and separate:  
`mknod`'s `dev` argument — a node made here carries no device number, because a number-to-driver  
table is what would make the number mean a device.

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
takes — the clause a `chown` honours and a `chmod` does not. Two shapes stay named in §5.2:  
`fallocate`'s resize and a write through a mapping.

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
