# MPOS Roadmap

What MPOS is building, phase by phase, and where each phase stands: the objectives,
the checklist, the remaining work and the priority — and nothing else.

Every measurement, gate run, lesson and trap behind a claim here lives in the
[`notes/`](notes/index.md) evidence library, which is where a design decision's reasoning belongs.
Read this file to know *what to do next*; read `notes/` to know *why the code is the way it is*.
A `(§N)` pointer on a line here names the section that carries that item's evidence; routing is
[`notes/index.md`](notes/index.md). Lines marked *(no evidence file yet)* are open gaps whose
measurement has not been written down — they are the roadmap's debt against the notes, not
pointers into them.

Legend: `[x]` done · `[~]` partial · `[ ]` todo · *(blocked: …)* = needs a prerequisite first

---

## Status board

One-glance status of the whole roadmap. Each line is a conclusion plus its evidence pointer.

**Core (Phases 1–3)**
- [x] P1 Syscall completion — epoll · timerfd · 67/68 other calls (§1)
- [~] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2)
- [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)

**Networking (Phase 4)**
- [x] 4.1 AF_UNIX — `SOCK_STREAM` + `SCM_RIGHTS`/`SCM_CREDENTIALS`/`SO_PEERCRED`, `SOCK_DGRAM` with per-message credentials (§4.1)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [~] 4.3 TCP — loopback, listener and `SO_REUSEADDR` done; two options record-only (§4.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)
- [~] 4.5 `AF_PACKET` — link layer as a socket, userland DHCP can ask (`t-dhcp`); `SOCK_RAW` frames `EOPNOTSUPP`, boot ships no client (§4.5)

**Memory (Phase 5)**
- [x] anonymous demand paging — a `mmap` is an address space; `mincore` reads the page tables (§5.1, §5.2)
- [x] 5.1 Shared anon `mmap` — aliased frames cross a plain `fork` (§5.1)
- [x] 5.2 `madvise` — `MADV_DONTNEED` releases and refills; deliberate no-ops say why (§5.2)
- [x] 5.3 `process_vm_readv`/`writev` (§5.3)

**Process (Phase 6)**
- [x] 6.1 ptrace (§6.1) · [x] 6.2 real rlimits incl. NOFILE/STACK (§6.2) · [x] 6.4 identity/cap calls, threads via `clone(CLONE_THREAD)` (§6.4)
- [x] teardown at exit, not at reap — an exited task gives back its address space and every descriptor immediately (§6, R.15)
- [~] 6.3 Namespaces — UTS built; **MNT steps 1 and 2 built**: the mount table is per-task (`MountTable` as a value), `unshare(CLONE_NEWNS)` copies it and `t-ns-mnt` pins both directions of divergence, `clone(CLONE_NEWNS|CLONE_NEWUTS)` copies per-kind into the child (`t-clone`), and a namespace is now nameable by a **descriptor** — `/proc/<pid>/ns/{mnt,uts}` are openable links (`kind:[id]`, one number pool) and `setns` joins by reference, so a caller adopts a namespace another process made (`t-ns-setns` pins the surface, the measured refusals, and the share-vs-copy difference). PID/NET designed, ordered UTS → MNT → PID → NET; the other six kinds have no entry at all, which is the surface a kernel built without them has; **MNT is complete** — threads share the process's filesystem context (root, cwd, path, mask) and its descriptor table, `unshare(CLONE_FS)`/`CLONE_FILES` split the caller's own slot, and the three thread-group flags a sibling makes impossible (`CLONE_VM`/`CLONE_SIGHAND`/`CLONE_THREAD`) are `EINVAL` where all six are `0` alone, measured one child per flag (`t-unshare-fs`); `t-ns-setns` pins that a namespace and its contents outlive the process that made them, and every path-taking syscall resolves in the caller's *own* tree — its mount namespace and its root — where `t-caller-paths` pins one claim per handler (`lstat`/`statx`/`readlink`/`O_NOFOLLOW` inside a namespace, `mkdir` inside a jail) (§6.3, §6.6)

**ABI gaps → `ENOSYS` (remaining)**
- [x] inotify · [x] `memfd_create` (§7 Stage 3) · [~] xattrs (validate → `EOPNOTSUPP`, no store) (§Remaining)
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
- [ ] `userfaultfd` · `tid/tgid_add_object` (rseq) — niche
- [x] real-time signals `32..=64` — queued with per-copy `si_value`; ring fixed-length, flood past `RT_QUEUE_LEN` drops silently (§2.3)
- [x] POSIX timers 222–226 — routed, tick-scanned, musl's `SIGEV_THREAD_ID` shape works end to end (§Remaining)
- [ ] `io_uring_setup`/`enter`/`register` — deferred by design (§1.3)

**Drivers (Phase 7)**
- [~] 7.1 Display — `/dev/fb0` + DRM/KMS card & render node, dumb buffers, atomic commits, PRIME both ways; weston-on-GL in software; no EDID-driven modes (§7.1, §7.4)
- [x] 7.2 Input — virtio-input evdev, capability ioctls, `/dev/input/eventN` + uevents (§7.2)
- [x] 7.3 AHCI/SATA — second disk named under its own driver, pinned by `t-ahci` (§7.3)
- [x] `/dev/uinput` — a virtual input device from userspace, with its `/dev` node and `add` uevent (§7.2)
- [x] the `/dev` node set is assembled by the drivers that own it (§7.5)
- [x] configuration space over MMIO (ECAM/MMCONFIG), refused unless both mechanisms agree (§7.6a)
- [ ] **the window's other 3840 bytes**: PCIe extended capabilities are outside every reader (`CFG_SIZE` = 256) (§7.6a)

**Wayland compositor (weston) support** — verified by `t-*`; GUI end-to-end is `cargo xtask run --mode gui`:
- [x] peer credentials + fd passing · libinput classification · device discovery · `wl_shm` buffers · framebuffer present · DRM/KMS surface (§4.1, §7.1, §7.2)
- [x] the screen has an owner — `VT_SETMODE`'s `VT_PROCESS` negotiation, revocable DRM master, `EACCES`/`POLLHUP` for a lost session (§7.1, `t-seat`)
- [x] seatd survives a re-run; Mesa GBM+EGL+GLES in software; parked evdev readers woken by the service task; `EVIOCGRAB` per-owner (§7.1, §7.2, R.7)
- [x] the aarch64 GL path's EL0 traps answered (`ctr_el0`, `DC CVAU`/`IC IVAU`) — `t-el0-sysreg` (§15.36)
- [ ] the aarch64 *session*: no `XDG_RUNTIME_DIR` from a login shell — *(no evidence file yet; R.7)* (§15.36)
- [ ] weston then takes a genuine EL0 write fault above its stack — *(no evidence file yet; R.7)* (§15.36)
- [ ] 3D / virgl / venus (`DRM_VIRTGPU_*`) — served and pinned, renderer itself unverified; the one thing that lets niri run (§7.4, R.15)

**Compositors beyond weston (niri, wlroots)** — analysis in §7.4:
- [x] the sysfs side of a DRM node's identity; the card found by its driver's declaration; the `..` in a symlink's target (§7.4)
- [x] the socket a compositor binds (short `addrlen`, negative backlog); `GETRESOURCES`' two-call idiom; property shapes (§7.4)
- [x] DRM sync objects incl. timeline flags, measured on the real card (§7.4, `t-drm-sync`)
- [x] the lease family with `drm.h`'s request numbers; legacy KMS queries refused by name; cursor plane driven (§7.4, `t-drm-lease`, `t-drm-args`)
- [x] `/proc/<pid>/maps` — three sources, two named divergences (§7.4, R.6)
- [~] the property families a compositor finds by name — four built, three deliberately absent (§7.4, `t-drm-props`)
- [~] the DRM surface a wlroots pixman compositor needs — in place, never booted (§7.4)

**Security (Phase 8)**
- [~] 8.0 allocation-failure panic — reproducing case fixed, tmpfs budget landed; global allocator still panics (§8.0, R.8)
- [x] 8.1 ASLR · [x] 8.3 user stack canary (§8.1, §8.3)
- [~] 8.2 seccomp — STRICT + FILTER with all eight actions measured; `PTRACE_EVENT_SECCOMP` is the one named remainder (§8.2)

**Performance (Phase 10)**
- [x] 10.1 network latency — event-driven waits + MSI-X (§10.1)
- [~] 10.2 context-switch/fork cost — lazy FPU, one TLB flush, smaller stacks; PCID recorded as a decision (§10.2)
- [~] 10.3 syscall path — itimer flag landed; fault-driven user-buffer copy not (§10.3)
- [x] 10.4 pipes + per-frame allocations — ring, handover and the seam frame pool all landed, and the **rate pair is measured**: the pool does **not** move the rate (before `12138`/`20372` datagrams/s, after `11768` and `19652` — the before sits inside the after's own 0.8–3.9 % run-to-run spread), while `pool=1036:0` and the counterfactual's failing warm-up pin say the steady path never allocates and its volume is bounded at 64 KiB (§10.4)
- [~] 10.5 ext4/virtio-blk read path — cache landed; the disk-backed instrument stands and its first green run is `cold=16387 warm=0` (a 64 MiB sequential read costs its own 16384 blocks plus three metadata reads; the fitting file's warm pass costs none); the next countable change is **not** readahead — the count is `blocks + metadata` by construction and `read_at` is handed one 4 KiB block per call, so readahead fetches the same blocks *earlier* and removes nothing; what moves the number is a **wider transfer** (ext4 fetching a run into the cache in one call, with the block layer's bound raised to a size the drivers already serve in one command — AHCI's 128 KiB via a 32-entry PRDT) (§10.5)
- [x] 10.6 serial TX ring · [ ] 10.7 sub-tick sleeps (LAPIC one-shot) (§10.6, §10.7)

**Architectures (Phase 9)**
- [~] 9.1 aarch64 complete boot — boots real Alpine to a login prompt, every service ok, consoles reach the wire (§9.1, §13)
- [ ] 9.2 RISC-V support (§9.2)
- [~] 13 Machine-seam purity + aarch64 parity — 13.1/13.3–13.18 and 13.19–13.31 landed, 13.2 in progress (§13); boot-side half is Phase 15 (§15)
- [x] the boot is one flow — one `start`, one sequence, probes ask both transports (§15.8)
- [x] the kernel travels inside the loader — the ESP carries one bootable file (§15.9)

**Architecture debt (Phase 11)** — audit done, Steps A–G landed, V-1/V-4/V-13 closed (§11):
- [x] 11.A subtract · 11.B device seam · 11.C per-open-file registry · 11.D terminal out of `vfs` · 11.F host-test criterion · 11.G depgraph ratchet (§11.2)
- [x] the `mm`/`vfs` seam (V-13) — `MappedObject` trait, pairs 19 → 18 (§11)
- [x] twelve leaf crates + `mpos-blk` as the thirteenth — enforcement by compile error (§11.2 Step E, R.11.E)
- [x] 11.4 the HAL, stages S1–S5 — machine seam enforced by the compiler, pairs 17 → 12 (§11.4)

**ABI truthfulness (Phase 12)** — Group A landed, gate runs to a summary (§12):
- [x] 12.0 green baseline restored (§12.4a)
- [~] 12.A honest-refusal batch — landed and gated through A9's buffer-direction audit (§12.1, R.12)
- [x] 12.B contract-completion batch (§12.2) · [x] 12.C stale rationale comments corrected (§12.3)

---

### Suggested next moves (highest impact, tractable)

In the order that keeps each step verifiable on its own:

1. **namespaces** (§6.3) — UTS built, per-process root landed (`t-chroot`), **MNT landed through step 2**: the table moved onto the task, `unshare(CLONE_NEWNS)` copies it (`t-ns-mnt`), and `/proc/<pid>/ns/{mnt,uts}` + `setns` let a namespace be *named by a descriptor* and joined by reference (`t-ns-setns`). What is left of MNT is nothing — CLONE_FS-style separation landed: threads share the process's filesystem context and descriptor table, `unshare(CLONE_FS)`/`CLONE_FILES` split the caller's own slot, and the three flags a sibling makes impossible answer `EINVAL` (`t-unshare-fs`, §6.3); the ladder's next rung (PID, then NET) is designed and unbuilt, and seccomp's one named piece left is the `PTRACE_EVENT_SECCOMP` stop (§8.2).
2. **§10.5's wider block transfer** — both instruments landed (`/proc/net/dev` + `t-netstats`; diskstat + `t-diskbench`), §10.4's pool landed behind its instrument and its rate pair is measured (and says "no": the pool buys no throughput, §10.4). §10.5's own next item is re-scoped by measurement: readahead cannot move the count (it is `blocks + metadata` by construction, one 4 KiB block per `read_at` call), so what is countable is a **wider transfer** — ext4 fetching a contiguous run in one call, with the block layer's bound raised to what a driver really serves in one command (AHCI already takes 128 KiB through its PRDT; virtio-blk's crate is the one to check). The pair to beat is (16387, 0), and the numbers are allowed to say "no".
3. **wire a DHCP client into the boot** (§4.5) — `udhcpc -s` under OpenRC *and* an address path for the selftest initramfs, so the static default retires to being the fallback; `t-dhcp` already proves the mechanism.
4. **the aarch64 session + weston's write fault** (§15.36) — the two lines above marked *(no evidence file yet)*; measure first, then write the entry.
5. **the display path specifically** — §7.4 carries its own order, and its first step is not a kernel change at all: boot a wlroots compositor on `WLR_RENDERER=pixman` and read what it says; it is the cheapest way to find what the analysis has wrong.

---

## Current Status

Two machines boot the same flow to a real Alpine userland; the ABI answers Linux's shape across
syscalls, signals, filesystems, networking, memory, devices and security, with the honest refusals
named. What is true today — counts, measurements, gates — is in [`notes/`](notes/index.md); this
section states direction only: the frontier is namespaces (MNT), the second compositor family
(3D/virgl verification), the aarch64 desktop session, and closing the remaining ABI gaps below.

---

## Remaining ABI support (gaps + services)

The full inventory — grouped by what they block, with each refusal's reason — is
[notes/appendix-status-board.md](notes/appendix-status-board.md)'s **R.Remaining**. In one line each:
`fanotify`/`keyctl`/`userfaultfd`/rseq side-objects stay `ENOSYS`; xattrs validate then `EOPNOTSUPP`;
the semantics still incomplete are TCP's two record-only options, `SO_LINGER`'s egress reset,
buffer sizes, `MADV_FREE`, MNT/PID/NET isolation, and the unsized-pty window divergence.

Two of those "buffer sizes" are now named down to the line, both found by `t-netstats` (the
first program of the default sweep that sends an IP packet off the machine, `docs/notes/` §10.4):
a **blocking UDP send does not wait for the send buffer** — `EAGAIN` where Linux waits, because
there is no event for the device's TX completion to wait on (`docs/uapi/` §5.2). What the same
program *also* measured is a test-side lesson rather than a gap: its first failures were a link
MTU an earlier program in the sweep had left at 1400, which no datagram of its burst could fit
under. Both halves are now pinned by that program (it asks the MTU and refuses to report a stack
failure for a link that cannot carry its datagram; `t-netlink` puts the MTU back).

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
| **P11: Architectural debt** | A done; B–C ~3–5 weeks; D–G ~1 week | High (maintainability) | Every later phase (§11) |
| **P12: ABI truthfulness** | gate restore ~days; Group A ~2 days; Group B ~1–2 weeks | High (correctness) | Programs that trust a false success (§12) |
| **P13: Machine-seam purity + aarch64 parity** | estimate: purity 13.1–13.2 ~2–4 weeks; parity 13.3–13.5 the aarch64 ladder | High (the second machine becomes usable) / Medium (maintainability) | A third port (§9.2 RISC-V) (§13) |

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

**P11 Step A is done.** P11's Steps B and C are the ones worth doing before the next device driver
or the next per-open-file resource is added, because each of those makes the debt one line larger.
