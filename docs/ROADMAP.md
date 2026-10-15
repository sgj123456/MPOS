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
- [~] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2)
- [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)

**Networking (Phase 4)**
- [x] 4.1 AF_UNIX — both socket types, with per-message credentials and fd passing; the
  abstract namespace is served, `unshare`-free lifetime and all (`t-unix`, §4.1.1)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [~] 4.3 TCP — loopback, listener and `SO_REUSEADDR` done; two options record-only (§4.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)
- [~] 4.5 `AF_PACKET` — link layer as a socket, userland DHCP can ask; the wire format's host
  pin landed (`crates/link`, `fuzz/link_wire`, §4.5); `SOCK_RAW` frames `EOPNOTSUPP`; the
  raw-IP door (`IPPROTO_RAW`) is open with named gaps left *(in flight)* (§4.5)

**Memory (Phase 5)**
- [x] anonymous demand paging — a `mmap` is an address space; `mincore` reads the page tables (§5.1, §5.2)
- [x] 5.1 Shared anon `mmap` — aliased frames cross a plain `fork` (§5.1)
- [x] 5.2 `madvise` — `MADV_DONTNEED` releases and refills; deliberate no-ops say why (§5.2)
- [x] 5.3 `process_vm_readv`/`writev` (§5.3)

**Process (Phase 6)**
- [x] 6.1 ptrace (§6.1) · [x] 6.2 real rlimits incl. NOFILE/STACK (§6.2) · [x] 6.4 identity/cap calls, threads via `clone(CLONE_THREAD)` (§6.4)
- [x] teardown at exit, not at reap (§6, R.15)
- [~] 6.3 Namespaces — UTS, MNT, **PID** and **NET** landed, each with its pin (`t-ns-uts`,
  `t-ns-mnt`, `t-ns-pid`, `t-ns-net`): the numbers a process sees are the reader's namespace's,
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
  namespace rather than the calling task's (§6.3, measured). What NET still cannot do is move a
  device: `eth0` cannot enter a namespace, and a vsock stack stays machine-wide - which is Linux's
  own shape, measured, with the transport question named (§6.3); the other four kinds Linux has -
  user, IPC, cgroup and time - have no entry at all here (§6.3, §6.6), which is `vfs::ns::Kind::ALL`
  against `namespaces(7)`'s list of eight

**ABI gaps → `ENOSYS` (remaining)**
- [x] inotify · [x] `memfd_create` (§7 Stage 3) · [~] xattrs (validate → `EOPNOTSUPP`, no store) (§R.Remaining)
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
- [ ] `userfaultfd` · `tid/tgid_add_object` (rseq) — niche
- [x] real-time signals `32..=64` — queued with per-copy `si_value`, bounded ring (§2.3)
- [x] POSIX timers 222–226 — routed, tick-scanned, musl's `SIGEV_THREAD_ID` shape works (§R.Remaining)
- [ ] `io_uring_setup`/`enter`/`register` — deferred by design (§1.3)

**Drivers (Phase 7)**
- [~] 7.1 Display — `/dev/fb0` + DRM/KMS nodes, dumb buffers, atomic commits, PRIME both ways; weston-on-GL in software; no EDID-driven modes (§7.1, §7.4)
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
- [~] the cursor plane drives a host pointer: hotspot cap accepted, `HOTSPOT_X`/`HOTSPOT_Y` published per-client, and binding/moving posts virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR` on the cursor queue (`t-drm`, `t-drm-props`) — kernel path built and gated green; reading the pointer back on the QEMU window after a gui weston boot is what closes it (§7.4, R.7)
- [~] 3D / virgl / venus (`DRM_VIRTGPU_*`) — served and pinned, and *negotiated* on a
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
- [~] a framebuffer object's own properties: `OBJ_GETPROPERTIES` answers for one (an empty
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
- [~] the property families a compositor finds by name — four built, three deliberately absent (§7.4, `t-drm-props`)
- [~] the DRM surface a wlroots pixman compositor needs — in place, never booted (§7.4)
- [ ] the renderer Mesa would have to pick: `is_software()` stays true until its loader can
  identify the card, which the node's `virtual/…` devpath is the standing lead for (§7.4)

**Security (Phase 8)**
- [~] 8.0 allocation-failure panic — reproducing case fixed, tmpfs budget landed; global allocator still panics (§8.0, R.8)
- [x] 8.1 ASLR · [x] 8.3 user stack canary (§8.1, §8.3)
- [~] 8.2 seccomp — STRICT + FILTER with all eight actions measured; `PTRACE_EVENT_SECCOMP` is the one named remainder (§8.2)

**Performance (Phase 10)**
- [x] 10.1 network latency — event-driven waits + MSI-X (§10.1)
- [~] 10.2 context-switch/fork cost — lazy FPU, one TLB flush, smaller stacks; PCID recorded as a decision (§10.2)
- [~] 10.3 syscall path — itimer flag landed; fault-driven user-buffer copy not (§10.3)
- [x] 10.4 pipes + per-frame allocations — ring, handover and the seam frame pool all landed; the rate-pair measurement is recorded (§10.4)
- [~] 10.5 ext4/virtio-blk read path — cache, wide transfer and the demand run landed and measured; interrupt-driven completions measured and re-scoped, multi-request pipelining dropped; the first machine's boot-disk figure still open (§10.5)
- [x] 10.6 serial TX ring · [ ] 10.7 sub-tick sleeps (LAPIC one-shot) (§10.6, §10.7)

**Architectures (Phase 9)**
- [~] 9.1 aarch64 complete boot — boots real Alpine to a login prompt, every service ok, consoles reach the wire (§9.1, §13)
- [ ] 9.2 RISC-V support (§9.2)
- [~] 13 Machine-seam purity + aarch64 parity — 13.1 and 13.3–13.31 landed, 13.2 in progress (§13); boot-side half is Phase 15 (§15)
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
- [~] 12.A honest-refusal batch — landed and gated through the buffer-direction audit (§12.1, R.12)
- [x] 12.B contract-completion batch (§12.2) · [x] 12.C stale rationale comments corrected (§12.3)

---

### Suggested next moves (highest impact, tractable)

In the order that keeps each step verifiable on its own:

1. **namespaces** (§6.3) — UTS, MNT, PID and NET are landed, and the ladder's remaining work
   is named per rung: a **wire that can move** into a namespace (`eth0` still cannot enter one),
   nested namespaces pin the *numbers* only one level deep (the zap's reach is pinned two levels
   now), and then the four kinds with no entry at all
   (user, IPC, cgroup, time - four of `namespaces(7)`'s eight). The `/proc/net` reader's rows were
   per-namespace but unexercised; `t-ns-net` reads the file on both sides of the boundary now
   (2026-10-15). Two more left this list on
   2026-10-15: the pid namespace's own creation rules (one per process, and a sealed one takes no
   tasks - both refused now with the errno Linux uses), and `NSpid`'s single number, which is the
   chain in all four `NS*` fields now. `F_GETLK`'s `l_pid` is the reader's number too, from
   2026-10-15 - and its `F_OFD_*` neighbours are served, in the same record domain, from the same
   day (§3.2). `AF_UNIX` moved off this list
   (2026-10-15) and vsock left it with a measurement instead of an implementation, both recorded in
   §6.3. Beside it: seccomp's one named piece, the `PTRACE_EVENT_SECCOMP` stop (§8.2).
2. **interrupt-driven block completions** (§10.5's last row) — measured and re-scoped; §10.5
   carries the structure change, its pin and what each machine's clock can see.
3. **what is left of the DHCP chain** (§4.5) — the raw-IP door's rules, cause and reverted
   attempt are §4.5's; the door is open through the device path the revert demanded, and the
   client is **verified end to end on the machine** (2026-10-13: `udhcpc: broadcasting
   discover` → `lease of 10.0.2.15 obtained from 10.0.2.2, lease time 86400`). It took two
   calls besides the door: the receive is an `AF_PACKET` **`recvmsg`** (measured on the host
   with `strace`, not the `read`/`recvfrom` `t-dhcp` had been pinning), and the seam's own
   ethertype constant was written with the wrong endian detour, so no door was ever filled.
   `t-dhcp` now reads one answer with each of the two calls. The wire-format parsers' missing
   host-side pin **closed** (2026-10-13): the `sockaddr_ll` decode, the Ethernet header and the
   frame ring moved to the `crates/link` leaf (`fuzz/link_wire`), so what is left is the two
   named gaps: `SOCK_RAW` frames and the unicast half of the raw send.
4. **the weston pointer on the window** (R.7, §7.4) — the root cause is fixed in code: the cursor
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
   is the one measurement a `t-*` program cannot make: boot against the window (`cargo xtask run
   --mode gui`, or `--mode terminal` with root on the serial line as §15.36 drove it), start weston,
   and read the pointer rendering. The serial log should carry the new `virtio-gpu: … cursorq=1`
   token. Two residuals stay named rather than claimed: the window confirmation itself, and the
   no-cursorq fallback branch, which neither configured machine can exercise (§7.4).
5. **the display path specifically** — §7.4 carries its own order, and its first step is not a
   kernel change at all: boot a wlroots compositor on `WLR_RENDERER=pixman` and read what it
   says; it is the cheapest way to find what the analysis has wrong.
6. **the renderer's device identity** (§7.4's last update) — niri now boots on a 3D card and the
   card negotiates its cap sets, so the compositor's remaining refusal is Mesa's: it still picks
   `llvmpipe` and names why (`egl: failed to create dri2 screen`). The lead is the DRM node's own
   sysfs path — `/sys/devices/virtual/drm/card0` while its `device` link reaches the PCI function,
   and libudev's parent walk follows the devpath. Beside it, the GL card cannot be the gate's
   default yet: the sweep passes on it but QEMU returns no clean debug exit. Both are §7.4's, with
   the numbers.

---

## Current Status

Two machines boot the same flow to a real Alpine userland; the ABI answers Linux's shape across
syscalls, signals, filesystems, networking, memory, devices and security, with the honest refusals
named. What is true today — counts, measurements, gates — is in [`notes/`](notes/index.md); this
section states direction only: the frontier is namespaces (PID/NET), the second compositor family
(3D/virgl verification and the weston write fault), the block layer's completion path, and closing
the remaining ABI gaps below.

---

## Remaining ABI support (gaps + services)

The full inventory — grouped by what they block, with each refusal's reason — is
[notes/appendix-status-board.md](notes/appendix-status-board.md)'s **R.Remaining**. In one line each:
`fanotify`/`keyctl`/`userfaultfd`/rseq side-objects stay `ENOSYS`; xattrs validate then `EOPNOTSUPP`;
the semantics still incomplete are TCP's two record-only options, `SO_LINGER`'s egress reset,
buffer sizes, MNT/PID/NET isolation, and the unsized-pty window divergence.

One of those "buffer sizes" is named down to the line: a **blocking UDP send does not wait for the
send buffer** (`EAGAIN` where Linux waits) — the gap, its tried shapes and their measurements are
`docs/uapi/` §5.2's entry; it closes when the TX ring gets a completion wakeup. The MTU story that
program also caught is a test-side lesson, not a gap: it lives with §10.4's measurement.

---

*The phase-by-phase effort/impact table this file used to end with is retired: P1–P9's estimates
predated Phases 12–15 landing, and "Recommended order: P1 → … → P9" orders work the status board
above marks done. What is open lives in the board and the moves above; `docs/notes/`'s appendix
carries the per-phase history.*
