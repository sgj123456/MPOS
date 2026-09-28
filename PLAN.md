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
- [~] 4.3 TCP — the loopback and the listener are done; three options are not
  - [x] the loopback is a real wire — `lo` is an interface over smoltcp's own
        loopback device carrying `127.0.0.0/8`, and a socket's interface is
        decided by the first address it is given, because the stack now keeps
        one `Fabric` (interface + device + socket set) per wire and smoltcp has
        no way to move a socket between them (`t-udp-loopback`). Raw/ICMP
        sockets deliberately stay on eth0's (§4.3.1)
  - [x] `listen`/`accept` on IPv4 — a listener is a *pool* of listening sockets
        plus the queue of the ones that completed, refilled after every poll;
        the contract is Linux's, each rule measured first, and closing a
        listener `FIN`s its unaccepted queue because an unsolicited `RST` is
        not a packet smoltcp can send (§4.3.2, `t-tcp-listen`)
  - [ ] `SO_REUSEADDR` (the TIME_WAIT rebind allowance, now that the listening
        side exists to observe it) / `SO_LINGER{on,0}`'s egress reset / buffer
        sizes still record-only (§4.3.3)
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)

**Memory (Phase 5)**
- [x] anonymous **demand paging** — a `mmap` is an address space and a page
      arrives on the fault that needs it; `mincore` answers from the page tables,
      so an untouched range reads as mapped and not resident (`t-mmap-demand`)
- [x] 5.1 Shared anon `mmap` — an unnamed object, so aliased frames cross a plain
      `fork` (`t-mmap-shared-fork`)
- [x] 5.2 `madvise` — `MADV_DONTNEED` releases the pages and the next touch
      refills them; `MADV_FREE` and a shared mapping's cross-process drop stay
      deliberate no-ops and say why (§5.2)
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
- [~] 7.1 Display — `/dev/fb0` (mmap + FBIOGET_* + PAN/VSYNC present) and
      `/dev/dri/card0` + `/dev/dri/renderD128` (resources, dumb buffers, addfb2,
      atomic commits, page-flip + vblank events, PRIME both ways) — which with
      Mesa's GBM and EGL is weston-on-GL, in software. No 3D/virgl, no sync
      objects, no format modifiers, no EDID (§7.1, §7.4)
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
- [x] Mesa `GBM`+`EGL`+GLES — served through `/dev/dri/renderD128` and PRIME, in
      software (llvmpipe); it is how weston's DRM backend runs at all
- [ ] 3D / virgl / venus (`DRM_VIRTGPU_*`) — deliberately out of scope, and the
      one thing that would let niri run (§7.4)
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
      and none (`t-udev-worker`). Getting there took two findings, and the first
      draft of this line named the wrong channel, so the correction is the
      interesting part: the *worker* channel is a **`SOCK_DGRAM`** pair
      (`socketpair(AF_LOCAL, SOCK_DGRAM|SOCK_CLOEXEC, 0, worker_watch)`, read
      straight off eudev's `udevd.c`) whose whole message is **zero bytes** -
      `struct worker_message` has no members, so `sizeof` is 0, the worker's
      `loop_write` is a zero-length `write(2)`, and the sender's
      `SCM_CREDENTIALS` *are* the payload the manager looks its worker up by.
      The kernel therefore has to (a) queue a zero-byte message and (b) hand it
      over to a `recvmsg` whose iovec has length zero - and (b) was `EINVAL` for
      every worker return, which left udevd's loop sitting on the failing read,
      spinning "failed to receive message: Invalid argument", with every device
      on the machine untagged. Both halves of the rule, and the `read(2)`-versus-
      `recv(2)` distinction beside it (`read` with a zero-length buffer touches
      nothing; `recvmsg` consumes), were measured against Linux before being
      written. The earlier `SOCK_SEQPACKET` work - a *paired* seqpacket end
      carrying messages, so that `send_dgram`'s "does this end carry messages?"
      test stops asking "is this a `SOCK_DGRAM`?" - was real and stays; it is the
      shape the *control* socket's connections have, but it was never the channel
      the log came from.
      **Verified on the desktop image**: after the fix a boot's `dmesg` carries
      *zero* "failed to receive message" lines (it was ~282, one per worker
      return), so the worker channel itself is closed. The boot also says what is
      next, and it is not the tag: `worker [11] did not accept message -22
      (Invalid argument), kill it` - see the item below.
  - [x] **the manager's handoff to a worker** — `udev_monitor_send_device(monitor,
        worker->monitor, dev)` hands the device over `NETLINK_KOBJECT_UEVENT`, and
        the family had no write side at all: the call is `sendmsg` with a 12-byte
        `struct sockaddr_nl`, which the send path read as a `sockaddr_in` and
        refused for being short of 16 bytes (`EINVAL`), after which the manager
        killed every worker that would not take its message. The uevent family
        now carries userland messages: a port id per socket (the process's pid
        when it is free, a unique number otherwise - Linux's order of preference,
        and libudev reads its own back with `getsockname`), unicast by `nl_pid`,
        group delivery for a send with no port id, the sender's address and
        credentials stamped on every delivery, and the payload left opaque
        because the wire format is libudev's. **Verified on the desktop image**:
        `/dev/input/event0` now carries `ID_INPUT=1`, `ID_INPUT_KEY=1` and
        `ID_INPUT_KEYBOARD=1`, `/run/udev/data/c13:64` holds them, and the kernel
        log has no handoff failures left (§4.4 has the measured rules,
        `t-uevent` the pins)
  - [x] **and the handoff was only the first of three reasons libinput would not
        take the device** - the other two are in the interfaces a compositor
        *reads* rather than in the events, and neither is a defect the log
        names. `evdev_device_have_same_syspath` re-derives a device's `/dev`
        node from its own `fstat` devnum and refuses it unless the string it
        resolves is the one udev gave it, so `/sys/dev/char/M:m` has to point
        at the *device* and not at another link to it - it pointed at
        `../../class/input/event0`, the two answers disagreed, and the keyboard
        was dropped without a word. The links carry the device tree's own path
        now (`../../devices/virtual/...`), block devices excepted because this
        tree has no device directory for them. And `EVIOCGPHYS`/`EVIOCGUNIQ`
        answered `ENOTTY` for the strings a virtio input device does not have,
        where `libevdev`'s `libevdev_set_fd` tolerates that failure only for
        `ENOENT` - the errno Linux's own `str_to_user` returns for a NULL
        attribute - so the wrong one made the whole read fatal, and libinput
        gave up with "failed to create input device" for every node on the
        machine (§7.2; `t-evdev` pins the `phys`/`uniq` answer *and* the link's
        one-hop target - against the string udev knows the device by, so the
        comparison libinput makes is the thing held, not the spelling of the
        path)
  - [x] **and a node's hang-up had to become sayable** - the wait udev parks on
        reads only that. `spawn_wait` watches its child's pipe with epoll, takes
        a read of 0 as "look again" and ends on `EPOLLHUP` alone, so an
        end-of-stream reported only as `EPOLLIN` left the device manager sitting
        on every `IMPORT{program}` rule until its own event timeout expired.
        `Inode` therefore answers `poll_hup`/`poll_err` beside `poll_ready`: a
        pipe's read end calls a lost writer a hang-up and its write end calls a
        lost reader an error, while a socketpair folds the peer's departure into
        the hang-up and stays readable beside it, because `unix_poll` sets
        `EPOLLIN` and `EPOLLHUP` together where `pipe_poll` drops the read
        condition. `poll`, `select` and `epoll` report the pair whether or not
        the caller asked for them, which is Linux's rule and sometimes the only
        report a parked waiter will ever get (§1.1, `t-epoll` pins the shape)
  - [x] **and the read itself had to stop parking** - the fourth gap, and the one
        the other three hid. `libinput` opens every evdev node with `O_NONBLOCK`
        and pumps it from `epoll`; this node's read never looked at the flag, so
        the first read with an empty record window parked on the service task's
        wakeup list. weston therefore created `event0`, logged it as a keyboard,
        and then stopped - no pointer, no failure, its event loop gone with
        nobody left to answer the input it was waiting for. `EAGAIN` is what
        Linux answers there and what libinput's loop is built around; the bit is
        mirrored into the node by the same `sync_nonblocking` every other
        blocking node here is told by, so nothing invented a second route for it
        (043c9c6). **Verified on the desktop image in the mode weston actually
        runs in** (`cargo xtask run --mode weston`; the kernel console is off the
        scanout, so the serial line is trustworthy as well as readable): one run
        logs `event0 - QEMU Virtio Keyboard: is tagged by udev as: Keyboard ...
        device is a keyboard`, `event1 - QEMU Virtio Tablet: is tagged by udev
        as: Mouse ... device is a pointer`, `libinput: configuring device` for
        both, and both associated with output `Virtual-1` - where before it was
        `failed to create input device` twice and `no input devices found`.
        That run is an operator's, though, and a compositor's keyboard is too
        much to leave resting on one: `t-evdev` pins the read rule in the gate -
        an empty node opened `O_NONBLOCK`, and the same node with the bit set
        through `fcntl(F_SETFL)` afterwards, each answering `EAGAIN` instead of
        parking - so a fifth time this regresses, the sweep says so and no one
        has to watch weston's log for the silence
  - [ ] `SOCK_SEQPACKET` still carries no boundaries over a *named* socket (the
        stream this family has), which is the one documented divergence left on
        the path from `udevadm` to a tagged device node

**Compositors beyond weston (niri, wlroots)** — what a *second* compositor family
needs on top of the line above. The analysis is §7.4, and none of it has been
run, so these are steps rather than results:
- [~] the DRM surface a **wlroots pixman** compositor needs — a primary node with
      `SET_MASTER`, `DRM_CAP_DUMB_BUFFER`, `CREATE_DUMB`/`MAP_DUMB`,
      `PRIME_HANDLE_TO_FD`, `ADDFB`/`MODE_ATOMIC` — is already in place, which
      makes `WLR_RENDERER=pixman` the cheapest second-compositor experiment there
      is; no wlroots compositor has been booted yet, and that is the whole of
      what is missing (§7.4)
- [ ] **threads** — `clone(CLONE_THREAD)` answers `ENOSYS`, and every compositor
      that is not single-threaded, every Mesa worker pool and every Vulkan ICD
      wants one (§6.4, §7.4)
- [ ] DRM sync objects — the `SYNCOBJ_*` family, including `SYNCOBJ_EVENTFD`
      (which Smithay probes before it will expose `wp_linux_drm_syncobj_v1`),
      with `IN_FENCE_FD`/`OUT_FENCE_PTR` given the behaviour their names promise
      (§7.4)
- [ ] `DRM_VIRTGPU_*` 3D — the only path that makes `is_software()` false, and so
      the only one that lets niri, which rejects software EGL outright, run at
      all; virgl and venus are both built on it (§7.4)
- [ ] the property families a compositor finds by name: `EDID`, `non-desktop`,
      `max bpc`, `panel orientation`, `vrr_capable`/`VRR_ENABLED`,
      `GAMMA_LUT`/`GAMMA_LUT_SIZE`, `IN_FORMATS` with
      `DRM_CAP_ADDFB2_MODIFIERS`, `FB_DAMAGE_CLIPS`, `OUT_FENCE_PTR`, and
      `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` — refused today, and asked for
      precisely by the virtualized drivers (§7.4)
- [ ] `MODE_GETFB2`, the `MODE_CREATE_LEASE` family, `MODE_CURSOR`/`MODE_CURSOR2`
      and the legacy gamma pair (§7.4)
- [ ] `/proc/<pid>/maps` — absent, and read by Mesa's allocator paths, LLVM and
      most crash reporters (§Remaining)

**Security (Phase 8)**
- [~] 8.0 **a userspace process could panic the kernel** - fixed for the case
      that was reproducing (a ramfs file grew as one `Vec`, and a 128 MiB
      contiguous `realloc` is an order the buddy allocator never serves), and
      tmpfs now has a byte budget answering `ENOSPC`; demand paging also took
      "maps too much" off the list, since a mapping is now an address space and
      backs nothing until it is touched. Still open: the global allocator's
      failure path is `panic!`, so any other unbounded user-driven growth still
      ends the machine (§8.0)
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
- [~] 10.4 Pipes + per-frame allocations — the pipe ring, the raw-receive
      temporary and the *handover* all landed, with numbers in §10.4 (a
      two-process pipeline now runs at the pipe's own speed); the frame pool is
      not, and is the last item left here
- [~] 10.5 ext4/virtio-blk read path — the set-associative cache landed with its
      device-read counts in §10.5; readahead and blk pipelining are each other's
      prerequisite and neither is measurable until the test machine has a disk
- [x] 10.6 Serial TX ring (drained by the TX-held interrupt; the mirror drops
      rather than stalls) · [ ] 10.7 Sub-tick sleeps (LAPIC one-shot)

**Architectures (Phase 9 — far future)**
- [ ] 9.1 aarch64 complete boot · [ ] 9.2 RISC-V support

**Architecture debt (Phase 11 — audit done, Step A landed)**
- [x] 11.A subtract: the orphan/duplicate machine modules, the leftover `[msgtrace]` in the
      dispatcher, the aarch64 linker script that does not exist (§11.2 Step A)
- [ ] 11.B the device seam: one handle instead of six `Inode::as_*` downcasts, each device's
      ioctl contract moved down to its driver, user-pointer validation moved to `mm` (§11)
- [ ] 11.C per-open-file resources get a registry, so `sched` stops naming `net`/`vfs`/`drivers`
      on task exit (§11)
- [ ] 11.D the terminal emulator and the console mux move out of `vfs` (§11)
- [ ] 11.F the `target_os = "none"` gate gets a written criterion, and the unused host test
      module behind it is settled (§11)
- [ ] 11.G a depgraph ratchet, so the measured cycle count cannot grow back (§11)

### Suggested next moves (highest impact, tractable)

Landed since this list was written: `flock` is owned by the open file
description while record locks stay per-process (`t-filelock`), `EVIOCGRAB` is
per-descriptor with Linux's `EBUSY` and dies on close (`t-evdev`), ext4 updates
`atime` under `relatime`, two board entries that were already true are marked
so (AF_UNIX `SOCK_DGRAM` transfer, the serial TX ring), and evdev's read rule is
finished: a reader that is not the grabber is told nothing while the grab is
held, and two readers of one node each keep their own copy (§7.2, `t-evdev`).

**And the wake latency, which was the first item on this list, is fixed** - one
tick on every blocking wait, because the boot task's `park()` halted the core
while staying runnable. `wait4` went from 7.6 ms to tens of microseconds and a
two-process pipe handover from 7 MiB/s to 10 168 MiB/s, with `fork` unmoved;
§10.2 and §10.4 have the numbers, `t-forkcost` now *asserts* the bound (§10.7),
and it is the one item that was worth more than everything else in the
performance family put together.

**Two more of the list's own items have landed since it was written**, and both
were checked against the code rather than taken from their sections' headers:
per-message cmsg boundaries on `SOCK_STREAM` (§4.1 - the pipe ring tags each
handover with the stream position its send ended at and a read *stops* there, so
descriptors come back with their own bytes; `t-unix-scm` pins it at line 610,
"one byte, not two"), and ASLR (§8.1, whose own section records the four
landmarks, the stacked draws, and the `ET_EXEC`-must-not-move rule).

**And the udevd worker channel is *actually* answered now**, which needed the
shape read off eudev's source rather than guessed from the log: the channel is a
`SOCK_DGRAM` pair carrying a **zero-byte** message whose credentials are the
payload, so the rule that had to exist is "a message receive with no data room
consumes the message and answers 0" (§4.1). A `listen` backlog turned out to be
one deeper than the number it names in the same sitting - eudev's control socket
is created with `listen(sock, 0)` - and that is fixed and pinned too (§4.1).
Both were measured against Linux first, and both are files the board's own entry
had described the wrong way round.

**The step behind it landed the same way**: the manager's handoff to a worker
runs over `NETLINK_KOBJECT_UEVENT`, and that family had no write side, so the
device manager killed every worker it could not hand an event to. It has one now
- port ids, unicast, groups, stamped credentials, measured rule by rule and
pinned by both `uevent`'s host tests and `t-uevent` (§4.4) - and the desktop
image ends the chain the plan started from: `/dev/input/event0` carries
`ID_INPUT_KEYBOARD=1`, which is the tag libinput classifies a keyboard by. What
is left on that path is one divergence, not a blocker: a *named* `SOCK_SEQPACKET`
connection is served as the byte stream this family has, which is the board's
remaining bullet.

**Getting the tag took three more of the same kind of reading**, and each one is
a place where a compositor's question was answered with the wrong *shape*
rather than with no answer: `/sys/dev/char/M:m` pointed at the class entry
instead of the device the class names (a string comparison libinput makes),
`EVIOCGPHYS`/`EVIOCGUNIQ` said `ENOTTY` where `libevdev` accepts only `ENOENT`
for what a device does not have, and a pipe reported its writer's departure as
`EPOLLIN` rather than as the `EPOLLHUP` that udev's `spawn_wait` ends on. The
board's own two bullets carry each, and none of the three needs a subsystem:
they are an errno, a link target and a node's answer to a question nothing here
was asking before (§7.2, §1.1).

What is left, in the order that keeps each step verifiable on its own. **The
list's first two items are done**: §4.3's listener landed on top of the loopback
that had to come first, so a program inside the machine can now serve one - both
halves measured against Linux and pinned by `t-tcp-listen`, and the section
rewritten around them (§4.3.1-§4.3.2) - and anonymous demand paging, with the two
things it unblocked, is §5.1/§5.2's rewrite (`t-mmap-demand`,
`t-mmap-shared-fork`).

1. **Anonymous demand paging** (§5.1 + §5.2) - the prerequisite both items named,
   and the only one that changed the mm fault path for everything. Landed: a
   `mmap` is an address space, a page arrives on the fault that needs it, and
   `MADV_DONTNEED` gives it back again (`t-mmap-demand`).
2. **`seccomp` FILTER's plumbing** (§8.2 - its runner has landed and is
   host-tested; what is left is the per-task list, the two install paths and the
   gate, which is a bounded piece of wiring rather than a feature), then
   **namespaces** (§6.3) - a set of decisions (which namespaces, and what each
   one must observably isolate) that should be made before code.
3. **§10.5's disk-backed instrument, then the frame pool** - the two remaining
   members of the performance family with a plan and no number. Both are
   measurable today, and each wants a before/after figure with the disk in
   place - and the number is allowed to say "no".
4. **`SO_REUSEADDR` at `bind`/`listen`** (§4.3.3) - small, and newly *possible*:
   a `TIME_WAIT` rebind is a fact about the listening side, which now exists.
   The other two options left in that section need things this kernel cannot do
   yet (an unsolicited reset on egress; smoltcp buffers sized after the set is
   built), and each says so where it is defined.

---

## Current Status

- **~229 Linux x86_64 syscalls** implemented
- **Uniprocessor** — single core, `critical_section::Mutex<RefCell<T>>`
- **Signals** — handlers delivered via `rt_sigframe`, mask enforcement, `sigqueue` siginfo payloads
- **Filesystems**: ramfs, ext2/ext4 (write-through), devfs, procfs, sysfs, devpts
- **Networking**: smoltcp TCP/UDP/ICMP/ARP over virtio-net and a real loopback
  (`lo` = `127.0.0.0/8` over smoltcp's loopback device, one socket set per
  interface); IPv4 TCP listens and accepts (a local server runs); AF_INET +
  AF_UNIX
  (path `SOCK_STREAM`, with `SCM_RIGHTS` fd passing + `SO_PEERCRED`) + netlink
  (route and uevent, each with its read and write sides)
- **Display / input**: virtio-gpu 2D scanout exposed as `/dev/fb0` (mmap +
  fbdev ioctls + pan/vsync) and a DRM/KMS `/dev/dri/card0` with its
  `/dev/dri/renderD128` (UAPI ABI, dumb buffers, addfb2, atomic commits,
  page-flip + vblank, PRIME both ways) — GBM, EGL and GLES on top of it, in
  software, which is how weston's DRM backend runs; virtio-input evdev
  (`/dev/input/eventN`, capability ioctls, and events that belong to the
  description holding `EVIOCGRAB`) with a keyboard + tablet, and `/dev/uinput`
  for a program that needs an input device of its own. What the *next*
  compositor family needs is §7.4.
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
- **`/proc/<pid>/maps`** — not a syscall and not yet a file: the per-process
  directory carries `fd`, `stat` and `cmdline` and stops there. It is listed
  here because it is ABI, not a feature of a debugger: Mesa's allocator paths,
  LLVM and most crash reporters read it, and anything that reports where a
  mapping came from has nowhere to look (§7.4).
- Extended attributes (`getxattr`/`setxattr`/`listxattr`/`removexattr` + `l`/`f`
  forms, 188–199) — **answered**: arguments and target are validated (so a bad
  path is still `ENOENT`/`EFAULT`, a bad fd `EBADF`), then `EOPNOTSUPP`, because
  no filesystem here stores xattrs. Wiring them stops the `ENOSYS` fall-through
  that `fastfetch`/probing callers reacted to; a real xattr store remains future.

**Foundational behaviours (implemented syscalls, incomplete semantics):**
- **4.3** the family has a listener and a loopback now: `listen`/`accept` on
  IPv4 are served by a pool of listening sockets (`t-tcp-listen`), `lo` is a
  real interface over smoltcp's loopback device (`t-udp-loopback`), and the
  stack keeps one socket set per wire. Socket options: `TCP_NODELAY` and
  `SO_KEEPALIVE` reach the live smoltcp socket (Nagle off; probe timer armed at
  Linux's default interval) and `getsockopt` reads back every stored option;
  `SO_REUSEADDR` (moot with one binder), buffer sizes and `SO_LINGER` are still
  record-only.
- **4.4** both halves of netlink are written: the route half reconfigures the
  live stack (`RTM_NEWADDR`/`NEWROUTE`/`NEWLINK`, §4.4), and the uevent half now
  carries userland messages — port ids, unicast, groups and credentials — which
  is what udevd's handoff to a worker needs (§4.4's closing note).
- **5.1 / 5.2** closed. A mapping is a VMA (`mm::vma`) and the page behind an
  address arrives on the fault that needs it, which is what let
  `MAP_SHARED|MAP_ANONYMOUS` become an unnamed `Memfd` whose aliased frames
  cross a plain `fork`, and `MADV_DONTNEED` release pages that the next touch
  refills (`t-mmap-demand`, `t-mmap-shared-fork`). What stays a no-op says why
  in §5.2 - `MADV_FREE` has no pressure model to honour, and a shared mapping's
  pages are dropped for the caller's mapping rather than for the object, which
  needs a reverse map of mappings this kernel does not keep.
- **6.3** namespaces: `unshare`/`setns` validate but return `EOPNOTSUPP`; no
  real mount/pid/net namespace isolation.
- **4.1** AF_UNIX is answered for `SOCK_STREAM` (see Phase 4), including
  `SCM_RIGHTS` descriptor handover and `SCM_CREDENTIALS`/`SO_PEERCRED` peer
  authentication — a real dbus broker can now both connect its clients and
  name them — and for `SOCK_DGRAM`, message-for-message. What the family still
  lacks is message boundaries on a *named* `SOCK_SEQPACKET` connection.

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

The hang-up and error conditions are the pair that is *not* an interest:
`Inode::poll_hup`/`poll_err` answer them, and `poll`, `select` and `epoll`
report them to every waiter whether or not it asked, because they are often
the last report a descriptor will ever give — udev's `spawn_wait` ends on
`EPOLLHUP` and nothing else (§7.2), so a pipe that stayed quiet about its
writer's exit parked the device manager until its own timeout expired.
`t-epoll` pins the shape in the guest and `vfs/pipe`'s tests pin the answers
on both ends of a pipe and of a socketpair.

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
implicit rendezvous at a socket nobody listened on; ext4 refuses socket
nodes (`EOPNOTSUPP` - `/run` is ramfs); `mknod(S_IFSOCK)` still refuses, so
only `bind` creates a socket name; `getpeername` answers `AF_UNIX` with an
empty path (accepted peers carry no name). `SOCK_DGRAM` unix sockets are
served whole-message (`t-unix-dgram`), and so is a `SOCK_SEQPACKET` *pair*;
a *named* `SOCK_SEQPACKET` connection is served as the byte stream this
family has, which is the one divergence left in the family.

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

**A message may be zero bytes long, and receiving one may offer no room.** The
rule sounds like a corner and is not: eudev's worker channel is a
`socketpair(AF_LOCAL, SOCK_DGRAM|SOCK_CLOEXEC, 0, ...)` whose whole message is
zero bytes - `struct worker_message` has no members, so the worker's
`loop_write` is a zero-length `write(2)`, and the credentials that arrive with
it *are* the payload the manager looks its worker up by - and the manager's
`recvmsg` therefore offers an iovec whose length is zero. That shape answered
`EINVAL`, which left udevd's loop stuck on the failing read and every device on
the machine untagged (the board's `t-udev-worker` entry carries the whole
story). What the family now does is Linux's, measured one call at a time before
it was written: a `recv`/`recvfrom`/`recvmsg` with a zero-byte data buffer
**consumes** the front message and answers 0, still delivering
`SCM_CREDENTIALS` when the reader asked and there is room (`MSG_CTRUNC` when
there is not), and `MSG_TRUNC` when the message was longer than the buffer -
the message's own length is its boundary, not the caller's buffer. `read(2)` is
the exception, and it is a *file-level* one rather than a socket rule:
`vfs_read` short-circuits a zero-length count, so `read(fd, buf, 0)` touches
nothing at all while `recv(fd, buf, 0)` takes a message. `netlink` follows the
same rule (a `NETLINK_USERSOCK` socket that sent itself a message was the
measurement).

**And a `listen` backlog is one deeper than the number it names.** Linux's
admission test is `queue_len > backlog`, so `listen(fd, 0)` is a queue of one -
which this kernel read as "never admits" until eudev's control socket turned up
created with exactly `listen(uctrl->sock, 0)` (`udev-ctrl.c`), leaving `udevadm`
unable to reach the daemon at all. Measured on the host: backlogs 0..=3
admitted 1, 2, 3 and 4 connections before the next `connect` answered `EAGAIN`.
`t-unix` pins the depth and the room an `accept` returns; the divergence that
remains is the *unlistened* name, which is still `ECONNREFUSED`.

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

### 4.3 TCP: the loopback, the listener, and the options — **PARTIAL**

Three things live in this section and two of them are done. The **loopback**
(4.3.1) and **`listen`/`accept`** (4.3.2) are real; the **socket options**
(4.3.3) are the part still open, and of the six, three steer the stack and three
are stored and read back without changing a packet.

#### 4.3.1 The loopback is a real wire — **DONE**

`lo` used to be a name the link dump filled in: `rtnl` reported `127.0.0.1/8`
because a reader expects it there, and a segment sent to that address left by the
NIC and was never seen again. It is now an interface over **smoltcp's own
`Loopback` device** (gated on `alloc`, which this crate already enables)
carrying `127.0.0.0/8`, with no routes and its own up flag - so the device this
section kept promising as "fifty lines" turned out to be zero lines, and what was
left was the shape.

The shape is that **smoltcp cannot route it with one socket set.** Its route
lookup is *destination-only* - `Interface::route` answers `in_same_network(dst)`
or the best matching route (`iface/interface/mod.rs:1010,1014`) - and
`Interface::poll` walks every socket in the set it is handed, so a shared set
would have eth0 claim a `127.0.0.1` socket and put the segment on the wire,
whether or not `127.0.0.0/8` was added to eth0. Two interfaces therefore mean two
socket sets, and smoltcp has no operation that moves a socket between them.

So `Stack` is two `Fabric`s (interface + device + set + its own up flag, with
`sockets_mut(home)`/`poll_home(home)` resolving a handle), a socket's channel
carries a `home`, and the home is decided by the first call that names an
address - `bind`, `connect`, `sendto` - through one rule: `127.0.0.0/8` is the
loopback's, everything else (the wildcard included) is eth0's. A socket created
before then is *relocated* by swapping it for a fresh one in the right set, which
is only ever done to a socket nothing has used yet (`Closed` for TCP, `!bound`
for UDP) and always before the call that would consume it.

Two boundaries, recorded rather than hidden. A **raw socket never moves**: its
destination is a per-call argument, so one socket cannot serve two sets - which
is what keeps `ping 127.0.0.1` behaving exactly as it did, travelling the virtual
wire where slirp answers it, as `t-netlat` measures. And a **wildcard listener**
lives on eth0's fabric, so a loopback client does not reach it; the bound
`127.0.0.1` case is the supported one.

`t-udp-loopback` is the instrument: two datagram sockets in one process
exchanging a payload over `127.0.0.1`, an unbound sender relocated by its
destination (with the sender named back *on the loopback*, which is the assertion
a stack that forgot the move fails), and a datagram to a port nobody holds
answered by nothing at all.

#### 4.3.2 `listen` and `accept` — **DONE**

smoltcp has no listening socket: a `tcp::Socket` is *one* connection, and one
that completes a handshake stops listening. Its own `listen` doc points at the
answer - a pool, re-listed as each connection completes - so a listener here is a
**pool** of sockets in `Listen` (at most `LISTEN_POOL_MAX = 4`, because each is
80 KiB of buffers) plus the **queue** of the ones that completed, kept in
`Stack::listeners` and serviced after every poll: a socket that left `Listen` for
`Established` or `CloseWait` becomes a queued connection, one whose handshake
died is dropped and replaced, and the pool is refilled from what the queue leaves
room for. A full queue refills nothing, so the next `SYN` finds no listener and
is dropped - Linux's own answer to a full backlog - rather than being answered
and then abruptly refused.

The contract is Linux's, and each rule was measured on this host's kernel before
it was written:

| Call | Answer |
|---|---|
| `listen` on an unbound socket | succeeds: it implicitly binds `0.0.0.0` and an ephemeral port, and `getsockname` reports `0.0.0.0:port` |
| `listen` on a bound socket | succeeds; `getsockname` reports the bound name |
| `listen` again | succeeds, re-setting the backlog |
| `listen` on a connected socket | `EINVAL` |
| `bind` on a listening socket | `EINVAL` |
| `accept` with nothing queued, blocking | waits, woken by the poll that fills the queue |
| `accept` with nothing queued, non-blocking | `EAGAIN` |
| `accept` on a bound-but-not-listening socket | `EINVAL` |
| `recv` on a listener | `ENOTCONN` |
| `send` on a listener | `EPIPE` (recorded, not pinned: Linux also raises `SIGPIPE` here, and this kernel raises no signal for a broken channel at all) |
| `getpeername` on a listener | `ENOTCONN` |
| `shutdown` on a listener | succeeds for either half, and the queue is left alone |
| accepted `getsockname`/`getpeername` | the connection's own and its peer's names |
| `connect` where nothing listens | `ECONNREFUSED` - smoltcp resets an unclaimed segment, so the loopback answers rather than timing out |
| backlog admission | `queue_len > backlog`, one deeper than the number it names - the rule §4.1 measured on the unix family |

`accept4`'s `SOCK_CLOEXEC`/`SOCK_NONBLOCK` are honoured for this family too; they
had been accepted and dropped while the refusal was the whole implementation.
`/proc/net/tcp` reports a listener as one `LISTEN` row on its bound address (the
pool behind it is not a connection and has no row), and `poll`/`select`/`epoll`
report a listener readable exactly when the queue is non-empty - the same
question `accept` parks on, so a waiter that is woken cannot find the queue empty.

What closing a listener does with the connections still in its queue is a
recorded divergence: they are closed, which gets a `FIN` out through the ordinary
teardown discipline, where Linux sends `RST`. That is the same limitation
`SO_LINGER{on,0}` has below - smoltcp's resets are emitted in reply to an
arriving packet, so an unsolicited reset is not something this stack can send.

`t-tcp-listen` pins the whole shape: a server bound to `127.0.0.1:0`, a client
that connects over the loopback, the accept with both names read back, bytes both
ways, the client's *half*-close read as end-of-stream while the other direction
still carries, a second sequential connection (which is what fails if the pool
never refills), and every refusal in the table above.

#### 4.3.3 The options — **PARTIAL** (three still record-only)

`setsockopt`/`getsockopt` are no longer a no-op: each socket's registry `Entry`
carries a `SockOpts`, and the common options round-trip with the correct ABI —
`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_SNDBUF`, `SO_RCVBUF`, `TCP_NODELAY` (`SOL_TCP`)
as 4-byte ints, `SO_LINGER` as an 8-byte `struct linger` (in-out length honoured),
plus `SO_TYPE`/`SO_ERROR`. `SO_RCVBUF`/`SO_SNDBUF` report the stack's default
(64 KiB) until changed; unknown options are accepted and read back as 0.

*Remaining (real effect, into smoltcp):* three. `SO_REUSEADDR` is not applied at
`bind`/`listen` - it needs an allowance for a rebind over a socket still in
`TIME_WAIT`, which is now observable because the listening side exists (4.3.2).
`SO_LINGER` is not honoured at `close` - see 4.3.4 for why the reset it asks for
is not something this stack can send. And `SO_RCVBUF`/`SO_SNDBUF` cannot be
honoured per socket at all until smoltcp's buffers stop being sized once at set
construction. Two that used to be on this list are not: `TCP_NODELAY` reaches
the live socket (`set_nagle_enabled`), and `SO_KEEPALIVE` arms smoltcp's probe
timer at Linux's default interval.

#### 4.3.4 `close(2)`, and the teardown the descriptor could not wait for — **DONE**

Measured rather than assumed: a plain `close(2)` reached `net::free_entry`, which
called `sockets.remove(handle)` on the spot, while smoltcp only puts the `FIN` out
from its poll loop after the socket is marked closing (`shutdown(2)` did call
`close()`; the last close did not). A closing socket is now marked and parked in
`Stack::teardown` - bounded, because a peer that stops answering may cost a slot
but not the set - and `reap_teardown` frees it after the poll that carries its
last segment, so the descriptor is gone when the caller asks and the wire still
gets its four packets. `t-tcp-close` + a host-side listener through slirp
(`10.0.2.2:4599` → host `127.0.0.1:4599`) shows the peer receiving the bytes
*and* end-of-stream instead of sitting until it gives up. (The socket to reap
lives in one of two sets now, so the list carries its home - see 4.3.1.)

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
the `FIN` the peer gets now. Honouring it needs an actual reset on egress - the
same missing primitive that makes closing a listener `FIN` its unaccepted queue
instead of resetting it (4.3.2).

*Remaining after those:* `SO_REUSEADDR` applied at `bind`/`listen` — a rebind
allowance over a socket still in `TIME_WAIT`, which the listener above is what
makes observable at all, and which is the next thing in this section; 
`SO_KEEPALIVE` probe packets; and a reset on egress for `SO_LINGER{on, 0}`.
`SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per
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
`SOCK_DGRAM` had to *open* (musl's `if_nametoindex` asks for it as the bare
handle for its own `SIOCGIFINDEX`; the datagram transfer itself has since
landed - see §4.1); and `IFLA_TXQLEN`/`IFLA_MTU` are native-order words, the
endianness addresses are not.

**Changes**:
- `RTM_NEWADDR`/`RTM_DELADDR`: `ip addr add`/`del`, applied to the stack
- `RTM_NEWROUTE`/`RTM_DELROUTE`: `ip route add`/`del`/`replace`
- `RTM_NEWLINK`/`RTM_SETLINK`: link up/down (a real poll gate), MTU, name
- `ip addr`/`ip route`/`ip link` dumps read the live tables, not constants
- The `ifreq` ioctl set `ip link set` and `if_nametoindex` ride on

**The other netlink protocol has no write side. That was the last blocker on the
udevd path, and it is written now.** `NETLINK_ROUTE` was already applied to the
live stack; `NETLINK_KOBJECT_UEVENT` was a read-only channel the kernel
publishes into, and udev needs the userspace half of it: the device manager
hands an event to one of its workers with `udev_monitor_send_device(monitor,
worker->monitor, dev)` - a `sendmsg(2)` on a uevent socket carrying a 12-byte
`struct sockaddr_nl` - and publishes the processed device to the group every
listener joined. The send path here read that address as a `sockaddr_in`
(`len < SOCKADDR_IN_LEN`) and answered `EINVAL`, so the handoff failed and udevd
killed the worker that refused it: `worker [11] did not accept message -22
(Invalid argument), kill it`, with no device tagged while it happened.

What the family grew, and it is measured rather than recalled - `libudev-monitor.c`
for the shape udev uses, this host's kernel for the semantics:

| Question | Measured answer |
|---|---|
| where a port id comes from | the process's pid when it is free, a hash-derived id when it is not (libudev's own comment: "usually, but not necessarily the pid"); an unbound socket is given one by its first send |
| binding a port id another socket holds | `EADDRINUSE`; binding twice (even to add a group) is `EINVAL` |
| a unicast | delivered to the port id; the receiver's `msg_name` is the *sender's* `nl_pid` with `nl_groups == 0`; an unknown or closed port id is `ECONNREFUSED`; the sender never receives its own |
| a group send | goes to the **lowest** group in the mask (Linux computes `ffs` of it: `nl_groups = 6` reaches group 2 and not group 4); the receiver is told the *destination group* as `msg_name.nl_groups`; the sender is not told; an empty group is not an error - the byte count comes back anyway, which is what libudev's "a multicast `ECONNREFUSED` would be expected" tolerates |
| a send with no port id and no group | accepted, and addressed to the kernel |
| a 12-byte `sockaddr_nl` | the shape that works; a short address or a family that is not `AF_NETLINK` is `EINVAL` (not `EAFNOSUPPORT`) |
| credentials | with `SO_PASSCRED` every received message carries `SCM_CREDENTIALS` with the sender's real ids; without it there is no control data at all |
| `getsockname` before bind | the all-zero address, not an error |

The policy is `uevent`'s and is host-tested - nine tests over port ids, unicast,
groups (including the `ffs` artifact), the full-queue `ENOBUFS` a handoff must
hear rather than lose, and the autobind - while `t-uevent` pins the shape
end-to-end in the guest: two sockets, `SO_PASSCRED`, a `sendmsg` to the port id
`getsockname` reported, the payload, the source address and the credentials, and
the three refusals. **And the boot says the same:** `/dev/input/event0` carries
`ID_INPUT=1`, `ID_INPUT_KEY=1` and `ID_INPUT_KEYBOARD=1`, and
`/run/udev/data/c13:64` holds them - which is the tag libinput classifies a
keyboard by, and the end of the chain that began with a worker's zero-byte
message.

---

## Phase 5: Memory Management Enhancements (Medium Priority)

### 5.1 Shared Anonymous mmap — **DONE**

`mmap(MAP_SHARED | MAP_ANONYMOUS)` is a *shared object*, not a private range:
the mapping is a frame-backed VMA over a fresh unnamed `Memfd` - the same frame
store `memfd_create` hands out, with real pages grown on demand, refcounted and
released when the last mapping of it goes. Nobody holds a descriptor for it and
nobody needs to: the mapping's VMA is its only name, and `fork` copies that.

That is what makes the aliasing survive a plain `fork`. Both processes keep the
same object, and the one rule that would have broken it is stated where the walk
is: **`fork` does not mark a frame-backed range's pages copy-on-write**, and it
does not harden them in the parent either. A copy-on-write flag there would turn
the first store on either side into a private frame, which is the sharing the
mechanism exists to keep. The `memfd` and dma-buf mappings get the same rule, so
"aliased frames don't cross a plain `fork`" is no longer a documented divergence
anywhere. `t-mmap-shared-fork` pins both directions - the child's write read by
the parent after a `wait4`, and the parent's write after the `fork` read by a
child the pipe held back - because only the second fails for a mapping that was
copied before the fork.

### 5.2 madvise — **DONE**

`madvise` answers `EINVAL` for a range whose start is not page-aligned (Linux's
rule) and recognises the `MADV_*` advisories, of which one now does something:
`MADV_DONTNEED` walks the range and drops the pages a VMA covers, leaving the
mapping in place so the next access is an ordinary fault. What that fault
produces is what the backing decides, which is Linux's rule in Linux's words -
"repopulating the memory contents from ... the underlying mapped file ... or
zero-fill-on-demand pages for anonymous private mappings". A private anonymous
page comes back freshly zeroed (its frame is gone, so a new one is allocated);
a page of a frame-backed range comes back from the object behind it, which still
holds it. `t-madvise` pins both, and `t-mmap-demand` shows the release and the
refill across a 64 MiB arena.

The other advisories stay the recognised no-ops they are, and two of them are
deliberate rather than unfinished:

* **`MADV_FREE`** keeps its no-op. Its contract is that the pages *may* be
  reclaimed but their contents are still there until memory pressure arrives,
  and this kernel has no pressure model - returning the frames immediately would
  be wrong rather than merely different.
* **`MADV_DONTNEED` on a shared mapping** does the per-mapping half of what Linux
  does. Linux drops the pages for everyone mapped to the object, which needs the
  object's reverse map of mappings (`i_mmap`) to find them; this kernel's object
  has no such index, and dropping the frames while another mapping still points
  at them would be a use-after-free rather than a saving. So the pages here are
  dropped for the caller's mapping and the object keeps its own copy, which the
  next fault finds - the "repopulate from the underlying mapped file" half of
  the promise, which is the half that is observable without the index. Nothing
  beyond that is claimed; `t-madvise` pins the single-process case and
  `t-mmap-shared-fork` the fork case.

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

One request in this family is deliberately not answered, and it is the largest
hole left in the process ABI: **`clone(CLONE_THREAD)`** — thread creation —
answers `ENOSYS`. Everything without it is built (the process-shaped clone, with
the tid writes, the TLS base and the signal frame the flags name), and a thread
group would share an address space that the scheduler's exit and space
accounting does not model yet — this kernel's address spaces are owned by one
task. Nothing in the shipped image needs a thread, which is how it has lasted;
everything larger does — a compositor that is not single-threaded, Mesa's worker
pools (`LP_NUM_THREADS`), any Vulkan ICD, `tracy_client`, `async-io` under
niri's D-Bus feature (§7.4). The refusal goes through the dispatcher's
`note_enosys` table with the other two of its kind, so a program that asks is
told once rather than left waiting.

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
- **DRM/KMS (`/dev/dri/card0` and `/dev/dri/renderD128`, `src/drivers/drm.rs`)**
  — a virtual single-CRTC card speaking the Linux UAPI exactly
  (`drm.h`/`drm_mode.h`): `VERSION`, `GET_CAP` (`DUMB_BUFFER`, `PRIME`,
  `TIMESTAMP_MONOTONIC`, `CRTC_IN_VBLANK_EVENT`, the cursor size),
  `SET_CLIENT_CAP`, `MODE_GETRESOURCES`, `GETCRTC`/`SETCRTC`,
  `GETENCODER`/`GETCONNECTOR`/`GETPLANE`/`GETPLANERESOURCES`,
  `GETPROPERTY`/`SETPROPERTY`/`GETPROPBLOB`/`CREATE_BLOB`/`DESTROY_BLOB`,
  `ADDFB`/`ADDFB2`/`RMFB`/`GETFB`, `PAGE_FLIP`, `MODE_ATOMIC`,
  `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`, `SET_MASTER`/`DROP_MASTER`, and GEM
  dumb buffers (`CREATE_DUMB`/`MAP_DUMB`/`DESTROY_DUMB`) whose `mmap` aliases
  real pages with the same per-mapping frame reference discipline the memfd
  uses. A page flip is applied to the scanout and reports back as a
  flip-completion `drmEvent` readable (and pollable) on the card fd.
- **The render node, PRIME and the GL stack.** `/dev/dri/renderD128` is the
  same card state under the render minor, which is the node GBM opens to
  allocate; `PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE` (both directions, so
  `DRM_CAP_PRIME` answers `IMPORT|EXPORT`) turn a dumb buffer into a dma-buf
  and back, and `src/vfs/dmabuf.rs` is the object that crosses. That is the
  whole of what Mesa's GBM needs to hand a buffer to the card, and it is why
  the image boots **weston on DRM + GL** (`weston-backend-drm` ships only the
  GL renderer; the driver behind it is llvmpipe — the software GL that
  `mesa-dri-gallium` provides). So "the presentation path a pixman compositor
  needs" is no longer the whole of it: **GBM, EGL and GLES are served**, in
  software. What is still absent is **3D/virgl** (no `VIRTGPU_*`), **sync
  objects**, **format modifiers**, and the property and metadata families a
  compositor reads by name - all of it the subject of §7.4.

v1 divergences: one fixed connector/crtc/encoder/plane; one synthesized mode
(the GPU's size at 60 Hz, `PREFERRED|DRIVER`), no `EDID` blob and
`mm_width`/`mm_height` of zero; vblank is synthesized at the frame beat (no
hardware interrupt); dumb-buffer mmap uses a `handle << log2(PAGES_PER_BUFFER)`
offset encoding rather than a DRM VMA manager; no per-client DRM leasing
(`MODE_CREATE_LEASE` and its family); `IN_FENCE_FD` exists and is stored but no
fence is ever waited on, so a compositor that passes one is told nothing about
it - and no `OUT_FENCE_PTR` at all. `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` and
the colour-pipeline cap are refused `EOPNOTSUPP`, the first of which the
virtualized drivers are the ones that ask for.

**The console's text grid, and the window it reports.** The framebuffer console
paints an 8x16 bitmap grid sized from the scanout (1024x768 is 128x48 cells,
1280x800 is 160x50, `MAX_ROWS` capping the rows at 64), and a virtual console
now *reports that grid* as its window: `TIOCGWINSZ` on `/dev/ttyN` asks
`Grid::rows/cols`, because the framebuffer console **is** that terminal's
emulator and the grid **is** the window. It used to answer a constant 80x25,
which is how a full-screen program came to draw into the top-left corner of the
screen - `vim` laid out 80 of 128 columns and 25 of 48 rows and left the rest
blank, which is the bug that reported it. `/dev/tty0` and `/dev/console` name
the console *currently shown*, so they answer the same way; both fall back to
80x25 when no screen exists, which is also every headless boot.

**The other half of that pair is `TIOCSWINSZ`**, and it is the one that matters
where the kernel *cannot* know the size: a terminal emulator in userspace - one
inside weston, say - opens a `/dev/ptmx` pair and names its window, and the
program it runs reads that back on the slave. So a pty keeps a window of its own
(25x80 until an emulator says otherwise) and reports it, while the console
stores a set size and goes on answering its screen - the screen is what is being
painted, and a number that disagreed with it would be the same lie the
corner-drawing bug came from. A *change* raises `SIGWINCH` to the foreground
group, by the same route a `Ctrl-C` takes: a pipeline is one job and every
process in it has the same window, and the signal is the only way a running
full-screen program learns it has been resized. `stty rows`/`cols` and `resize`
are what that pair is for.

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

**Two more findings came out of the same boot, and both are about what a
*classifier* asks rather than about input.** `EVIOCGPHYS`/`EVIOCGUNIQ` answered
`ENOTTY` for the strings a virtio input device does not have, and `libevdev`'s
`libevdev_set_fd` tolerates a failure there only when the errno is `ENOENT` -
which is what Linux's own `str_to_user` returns for a NULL attribute - so the
wrong one made the whole read fatal, far from the ioctl that caused it: libinput
gave up with "failed to create input device" for *every* node on the machine,
and a compositor came up with a screen and no keyboard. And the
`/sys/dev/char/M:m` / `/sys/dev/block/M:m` links pointed at the class entry
rather than at the device that entry names; a reader resolves them to identify a
node, and `evdev_device_have_same_syspath` compares the *strings* a device's own
`fstat` devnum and the path udev gave it resolve to, so two answers that disagree
is a device libinput drops silently. They carry the *device tree's* path now
(`../../devices/virtual/...`), block devices excepted because this tree has no
device directory for them. `t-evdev` pins the two errnos; the sysfs side is
`src/vfs/sysfs.rs`'s own tests.

### 7.3 AHCI/SATA (stretch)

**Why**: Real hardware support beyond virtio.

- AHCI HBA driver
- ATA IDENTIFY, READ SECTORS, WRITE SECTORS
- GPT partition detection (already have partition scanner)

---

### 7.4 The next compositor: niri, wlroots, and the renderer families — **ANALYSIS**

§7.1–§7.2 answer *weston*: DRM/KMS to present on, evdev for input, `memfd` +
`SCM_RIGHTS` for buffers, and Mesa's software GL through GBM and EGL. That is one
family, and a narrow one - the wlroots architecture with its GL renderer. This
section is what the *other* families want, read off their sources and the kernel
UAPI rather than measured here, and it says so: nothing below has been executed
on the machine, which is why it is a plan rather than a claim. The two
architectures fail for different reasons, and the surprise is which one is cheap.

#### What wlroots' pixman route already has

`WLR_RENDERER=pixman` on the DRM backend never creates an EGL context at all;
`wlr_allocator_autocreate` walks gbm → shm → **drm dumb** → udmabuf and, with no
GBM to be had, settles on the dumb allocator. Its requirements are short and the
card meets every one: `/dev/dri/card0` opened and `SET_MASTER` held (its
`drmIsMaster` check), the `DUMB_BUFFER` capability, `CREATE_DUMB`/`MAP_DUMB`/
`DESTROY_DUMB` with an `mmap(MAP_SHARED)` over the result,
`PRIME_HANDLE_TO_FD` to hand that buffer on, and `ADDFB`/`PAGE_FLIP` or
`MODE_ATOMIC` to put it on the screen. Formats are `LINEAR` or `INVALID` only,
which is what this card produces; `EDID`, modifiers and leases are each optional
to it, and the empty DRM lease it prefers for reopening a node falls back to a
plain `open` on `EINVAL`/`EOPNOTSUPP`, by its own source. So
`WLR_RENDERER=pixman sway` is the cheapest second-compositor experiment there is,
and it should be the first thing this plan does.

What it also needs is not a kernel object: `$XDG_RUNTIME_DIR`, libinput's udev
tagging (present, §7.2), and a seat. `seatd` is in the image and enabled, the
card arrives from the seat manager as a descriptor passed over its own unix
socket - `SCM_RIGHTS`, which this kernel already has (`t-unix-scm`) - and weston
takes its card that way today, which is the model to follow. All of it is rootfs
and init work.

#### What niri needs, and the two things that stop it

niri's TTY backend is `GpuManager<GbmGlesBackend<GlesRenderer>>` and nothing
else - there is no pixman renderer in it - and, worse for this kernel, it
*rejects* the renderer that is available:

    ensure!(!egl_device.is_software(), "software EGL renderers are skipped");

So on llvmpipe the render node is left `None`, no primary renderer is ever
created, and `connector_connected` cannot obtain one: there is no output at all.
That is a deliberate upstream choice (software EGL devices "segfault on importing
dmabufs from other renderers"), not a bug to route around, so **niri here
requires a hardware-style GL device**: `virtio-gpu` 3D. The `DRM_VIRTGPU_*`
family - `GET_PARAM`, `RESOURCE_CREATE`, `RESOURCE_INFO`, `SUBMIT_3D`,
`EXECBUFFER`, `MAP`, `TRANSFER_{TO,FROM}_HOST`, `CONTEXT_INIT`, resource blobs -
is what virgl (and venus, for Vulkan) is built on, and it is the largest single
item in this section.

Two smaller blockers sit behind it. **Threads**: `clone(CLONE_THREAD)` is
`ENOSYS` (§6.4), and a real compositor spawns them - `tracy_client`'s
profiler thread, `async-io` under the D-Bus feature, and Mesa's own worker pools
(`LP_NUM_THREADS`; one CPU makes llvmpipe's default single-threaded, but the path
exists, and Vulkan ICDs use it unconditionally). **Explicit sync**: Smithay will
not expose `wp_linux_drm_syncobj_v1` at all unless `DRM_IOCTL_SYNCOBJ_EVENTFD`
answers, and its DRM plane config writes `IN_FENCE_FD` when a fence is present -
a property this card accepts and then ignores, with no `OUT_FENCE_PTR` beside it.
A compositor that never gets the protocol will not pass a fence, so nothing is
*wrong* today; it does mean a client's buffer is read with no synchronisation
primitive at all, which is the implicit-fence boundary `src/vfs/dmabuf.rs`
already names as v1's.

What niri does **not** require is worth as much as what it does, because each one
is work this plan does not have to do: no `EDID` blob (`make`/`model`/`serial`
become "Unknown", and the physical size is `unwrap_or((0, 0))`), no `non-desktop`,
`max bpc`, `panel orientation`, `vrr_capable` or `VRR_ENABLED` (each looked up by
name and defaulted, with a `debug!` at most), no gamma (`GAMMA_LUT` is tried,
then the legacy `MODE_SETGAMMA`, then given up on), and no DRM lease (the global
is created and no-ops without it). What it *does* need is for the properties it
finds by name to exist under those names - and the half of `GETPROPERTY` that
reports the name is what such an interface depends on and cannot fake. This card
answers the names it needs: `CRTC_ID`, `ACTIVE`, `MODE_ID`, `FB_ID`, `type`,
`SRC_*`/`CRTC_*`, `pixel blend mode`, `IN_FENCE_FD`, `DPMS`.

#### The renderer families, and what each asks of the kernel

- **llvmpipe** (software GL - the driver behind today's weston). The JIT needs
  anonymous memory made executable: `MAP_PRIVATE|MAP_ANON` and then `mprotect`
  `RW`→`RX` or straight `RWX`, which `mmap`/`mprotect` already allow. It wants
  threads for `LP_NUM_THREADS`, a writable `$XDG_CACHE_HOME/mesa_shader_cache`,
  and `madvise(MADV_DONTNEED)` to give its scratch back - which is real now
  (§5.2), so the scratch it releases is memory the machine gets back rather than
  a mapping that only grows.
- **lavapipe** (software Vulkan) is the same stack one layer up, plus the Vulkan
  loader and an ICD manifest: userspace work that puts `vulkaninfo`-class probing
  and a Vulkan-based renderer within reach on a machine with no GPU 3D.
- **virgl and venus** (virtio-gpu 3D, and Vulkan over it) are `DRM_VIRTGPU_*`
  again. This is the only path that makes `is_software()` false, so it is on
  niri's critical path - and on lavapipe's performance path rather than its
  correctness one. It is also the only item here that needs the GPU's own
  command stream rather than the card's bookkeeping.
- **pixman** (software 2D) needs a buffer and a mapping and nothing else, which
  is the whole reason the wlroots route above is the cheap one.

#### The gap, as a list

Devices and ioctls, roughly in the order the compositors reach for them:
`MODE_CREATE_LEASE`/`LIST_LESSEES`/`GET_LEASE`/`REVOKE_LEASE`; `MODE_GETFB2`;
`CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE`; `MODE_CURSOR`/`MODE_CURSOR2` and
`MODE_GETGAMMA`/`MODE_SETGAMMA` (the legacy pair niri falls back to); the
`SYNCOBJ_*` family (`CREATE`, `DESTROY`, `HANDLE_TO_FD`, `FD_TO_HANDLE`, `WAIT`,
`SIGNAL`, `RESET`, `QUERY`, `TRANSFER`, `TIMELINE_WAIT`, `TIMELINE_SIGNAL`,
`EVENTFD`) with `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE`; and
`DRM_VIRTGPU_*`.

Properties and caps, cheap one at a time and load-bearing in aggregate: `EDID`
(a blob - with non-zero `mm_width`/`mm_height` and a `subconnector`, if anything
is ever to show a monitor's name), `non-desktop`, `max bpc`,
`panel orientation`, `vrr_capable`/`VRR_ENABLED` with
`DRM_CAP_ASYNC_PAGE_FLIP` beside them, `GAMMA_LUT`/`GAMMA_LUT_SIZE`,
`FB_DAMAGE_CLIPS` (the blob that lets a compositor say *what* changed instead of
sending whole frames), `OUT_FENCE_PTR`, `IN_FORMATS` with
`DRM_CAP_ADDFB2_MODIFIERS`, and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` - refused
today, and asked for precisely by the virtualized drivers.

Syscalls and memory, the parts this section cannot fix from inside the display
driver: **threads** (`clone(CLONE_THREAD)`, §6.4); `/proc/<pid>/maps`, absent and
read by Mesa's allocator paths, LLVM and most crash reporters; and the frame pool
(§10.4), since the renderer families allocate scratch in tens of megabytes where
the current per-frame accounting was never sized for it. Anonymous demand paging
and `MADV_DONTNEED` used to be on that list - they are §5.1/§5.2's rewrite now,
and the scratch a renderer gives back is memory the machine can use again.

Services, which are rootfs work: `sway`/`niri` with their libraries
(`libdisplay-info`, `libseat`, `xkbcommon`, `pango`/`cairo` and the fonts *and
writable font cache* those need), `vulkan-loader` with an ICD, and the
compositor-starting half weston already models.

#### Order

Each step stands on its own, which is the only reason the order is this one:

1. **A wlroots compositor on pixman.** Rootfs and init only; no kernel change is
   expected. `WLR_RENDERER=pixman` on `/dev/dri/card0`, and whatever the run says
   is missing becomes `t-drm`'s next case. Cheapest possible answer to "does a
   second compositor family run here", and a better rendering instrument than
   weston into the bargain.
2. **The property and cap families above.** No new subsystem: `GETPROPERTY`,
   `GETPROPBLOB` and one client cap, each observable from userspace
   (`drm_info`, `modetest`) before any compositor is involved.
3. **DRM sync objects**, with `IN_FENCE_FD`/`OUT_FENCE_PTR` given the behaviour
   their names promise - which is what makes explicit sync real rather than
   merely accepted.
4. **Threads** (§6.4) - not a display item, but on the critical path of every
   compositor that is not single-threaded and every Vulkan driver.
5. **`DRM_VIRTGPU_*`, then virgl in the image.** The largest item and the only
   one that makes niri possible.
6. **`/proc/<pid>/maps`** and the frame pool: the supporting cast, wanted by
   everything above and by nothing in particular. (`MADV_DONTNEED` was on this
   line; §5.2 has it now.)

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
failure path is still `panic!`, so any *other* unbounded user-driven growth - a
`Vec` in another filesystem path, the page tables a process that touches enough of
what it mapped will need - still ends the machine. Anonymous demand paging
(§5.1/§5.2) took the cheapest half of that away: mapping too much is now a
statement about addresses rather than a claim on memory, so the
map-a-tebibyte-and-never-touch-it shape this item was written around costs one
VMA and nothing else. What remains is the work the old plan named: make
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
- `SECCOMP_MODE_FILTER` — **done and wired**. The program, its validation and
  its evaluation are `src/seccomp.rs` (classic BPF with the packet idioms left
  out, one description of the instruction set shared by the validator and the
  interpreter, forward-only jumps that must land inside the program, an
  out-of-range load ending the program with `KILL_THREAD`, and the stacked
  action order compared as a *signed* field so `KILL_PROCESS` outranks `ALLOW`);
  nine host tests run real filter shapes over it. The plumbing that reaches it is
  in too: `Task::seccomp_filters` is a cumulative per-thread list inherited across
  `fork` and kept across `exec`; `seccomp(2)`'s op 1 and
  `prctl(PR_SET_SECCOMP, 2)` both go through one `install_seccomp_filter`, which
  copies the program out of the caller's memory and validates it *in full* before
  anything is stored (`EINVAL` for a malformed program, `EFAULT` for an unreadable
  one, `EACCES` for a caller that is not root, `EINVAL` for any flag - `TSYNC` is
  not modelled and a caller must not think it got it); a machine-wide armed flag
  keeps an unfiltered syscall to one relaxed load; and the gate acts on the answer
  *before* the table runs - allow/log/trace pass, `ERRNO` fails the call, `TRAP`
  raises `SIGSYS`, the kill actions and anything unimplemented refuse it - with
  `PR_GET_SECCOMP` answering 2 once a filter is installed. **Still missing**: its
  guest test (`t-prctl` is where the pins belong) and the action probe, which
  answers the empty set rather than the truth about what this kernel supports.

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

**The cost of a `fork` is the mapping; the `wait4` after it was the clock.**
Taking the arena away cuts the fork from 351 µs to 17 µs - twenty-one times -
and used to leave the `wait4` where it was, at *eight milliseconds either way*.
An earlier draft read the second row as the child's exit and its 8192 frame
references coming home (`release_user_frames`), and the third row refuted it: a
parent reaping a child that owned nothing paid exactly the same 8 ms.

**What that 8 ms was, and why it mattered more than anything in this section.**
One tick of this build's 250 Hz clock, on *every* blocking wait, and the cause
was the boot task. `park()` - where the boot task spends the machine's life -
halted the core with `wait_for_interrupt` instead of blocking in the scheduler,
which left a permanently *runnable* task at NORMAL priority: the picker handed
it the CPU, `hlt` stopped the core while holding it, and a task that has halted
can only be taken off it by an interrupt - of which the one that reliably
arrives is the clock. Anything woken while the idle task was in that state (or
behind it in the ready queue) waited for the next tick. A probe in the scheduler
recording the wake-to-resume latency in whole ticks is what named it: 604 of 640
wakes took *exactly one* tick, never two, while the scheduler itself never went
idle, so the task was ready and the picker was choosing somebody else first.
Parking the boot task in the scheduler puts the idle halt back in the
scheduler's own loop, which is where the virtio-net handler's comment already
assumed it was - "in the scheduler's own context ... the scheduler's next turn
picks the woken task on its own". That is the whole fix: one call site.

| `t-forkcost`, 200 cycles | before | after |
|---|---|---|
| `wait4`, 32 MiB parent | 7 648 682 ns | 1 760 ns - 100 µs |
| `wait4`, no arena | 7 980 378 ns | 13 046 ns - 100 µs |
| `fork`, 32 MiB parent | 351 052 ns | 223 983 - 298 868 ns |

The `fork` row is the control and it is the mapping: it did not move. The `wait4`
rows are now the child's own exit and a picker turn instead of a clock tick, and
the *range* is the honest shape of that figure rather than noise in it: a parent
that reaches `wait4` after its child has already exited does not block at all and
reads ~2 µs, one that gets there first blocks and reads ~100 µs on a loaded
machine - and one of them can print *below* the clock's resolution, where a
negative figure is the TSC fraction's noise floor and not a negative time. What
matters for both is the same: they are tens of microseconds against a tick of
4 ms, which is what `t-forkcost` now *asserts* (§10.7). §10.4 carries the same
fix's effect where it was largest.

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

**The second row was the finding, and it was bigger than the change.** A
two-process transfer did not move at all when the ring landed - 2047.5 ms to
2047.4 ms - and the arithmetic said why: 2047 ms over 256 chunks is **8.0 ms per
64 KiB chunk**. The reading taken at the time was "two ticks per handover", and
the count was right while the unit was wrong: a chunk is not one handover but
*two blocking waits* - the writer waiting for room in a full pipe, the reader
waiting for data in an empty one - so the figure was **one tick per wait**, and
one tick was the cost of *any* wake on this machine (§10.2). The bytes were
never the cost. `t-pipecost` is the instrument, and it was deliberately in the
suite so the fix had a number waiting for it:

| 16 MiB through one pipe | before | after |
|---|---|---|
| one process, 4096-byte units (data path only) | 6.5 ms - 2 478 MiB/s | 1.6 ms - **10 206 MiB/s** |
| two processes, 64 KiB chunks (a pipeline) | 2047.3 ms - 7 MiB/s | 1.6 ms - **10 168 MiB/s** |

The second row is the whole point of the pair: the same 16 MiB, through the same
pipe, now crosses 256 task boundaries at *the same speed as the copy that
crosses none*. Nothing about the pipe changed for this - the fix is the boot
task's park (§10.2) - and it is the clearest statement of what a wake used to
cost, because the pipeline was previously 1 300 times slower than the data path
it was built out of.

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

### 10.7 Sub-Tick Sleeps — planned (the wake half is done, and was not this)

**The wake latency was filed here and did not belong here; it is fixed.** Two
measurements said a *woken* task - not sleeping on a deadline, but parked in
`block_until` and made runnable by an event - did not resume for what looked
like two ticks:

| measured | before | after | instrument |
|---|---|---|---|
| `wait4` reaping a child, whatever that child owned | ~8.0 ms, unchanged when the child's 32 MiB address space is taken away | 1.8 µs | `t-forkcost` (§10.2) |
| a 64 KiB pipe handover between two processes | 8.0 ms per chunk, 7 MiB/s | 6.1 µs per chunk, 10 168 MiB/s | `t-pipecost` (§10.4) |

Neither was a sleep rounding to a tick, which is what this section is for: the
woken task was *ready* and the picker was not choosing it, because the boot
task's `park()` halted the core while remaining runnable at NORMAL priority and
a halted task is only taken off the CPU by an interrupt. The scheduler never
went idle once during the whole suite, which is the observation that separates
"the task is not runnable" from "the task is runnable and somebody else is
running" - and a probe recording wake-to-resume in *ticks* named the last one
directly: 604 of 640 wakes took exactly one tick. §10.2 has the fix and the
numbers.

**The bound is now written, in the one place it is honest.** `t-forkcost`
asserts that *both* of its `wait4` rows - each a 200-round average - are under
one clock tick. A wake must be a scheduler turn and not a deadline, and the
before-figures above are *clock-quantized*: they sat at two ticks (7.6-8.1 ms)
in every run, because a wait that ends on the clock cannot land between ticks.
So the bound sits ~40× above the fixed figure (tens of microseconds) and below a
broken one that cannot drift down to meet it. `t-pipecost` still only *reports*,
deliberately: its figures are throughputs, and a bound that holds under KVM can
be a flaky failure under TCG, which is its own stated rule. The wake is where
the quantized claim can be made, and it is the claim that mattered.

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

## Phase 11: Architectural Debt — **AUDIT DONE, NOTHING FIXED**

Everything above this phase is about *features*: what the kernel cannot yet do. This phase is
about the *shape* of the kernel that does them, and it opens with a measurement rather than an
opinion, because "bad architecture" is not a finding until something counts it.

`ARCHITECTURE.md` is the companion document: it draws the module graph, the boot sequence and
the syscall path, and its §9 mirrors the findings below. This section is the work list.

**Nothing in this phase has been implemented.** No file was edited to produce it. It is a plan.

---

### 11.0 How the audit was made

Reproducible in a few seconds, and worth re-running after any structural change:

```
# module-level edges: map every file to its top-level module, collect crate::<mod> refs
python3 - <<'PY'
import os,re,collections
files=[os.path.join(r,f) for r,_,fs in os.walk('src') for f in fs if f.endswith('.rs')]
top=lambda p:(lambda q:q[0][:-3] if len(q)==1 else q[0])(os.path.relpath(p,'src').split(os.sep))
edges=collections.Counter()
for p in files:
    for m in re.finditer(r'crate::([a-z_][a-z0-9_]*)',open(p,encoding='utf8',errors='ignore').read()):
        if m.group(1)!=top(p): edges[(top(p),m.group(1))]+=1
for (a,b),w in sorted(edges.items()): print(f"{a:12} -> {b:12} {w}")
for (a,b) in sorted(edges):
    if (b,a) in edges and a<b: print("CYCLE",a,b)
PY

# god files
find src -name '*.rs' | xargs wc -l | sort -rn | head -12

# orphan modules: a file under arch/x86_64 that mod.rs does not declare
cat src/arch/x86_64/mod.rs
```

The three numbers that came out of it, re-measured after Step A (§11.2):

| Measurement | Result |
|---|---|
| Kernel size | 80 239 lines across 114 `.rs` files (plus `boot/` 1 225 in 4 files and `xtask/` 4 396) |
| Host unit tests | 501 `#[test]` in `src/`, of which 498 run in the host build - the other three are `net/socket.rs`'s, behind the gate V-11 names - and 29 in `xtask/` |
| Mutual module pairs (a cycle of length 2) | **21**, among the eleven modules that appear in one: `arch`, `console`, `drivers`, `mm`, `net`, `pci`, `rand`, `sched`, `syscall`, `vfs`, `uevent` |

The audit was published with 79 095 lines across 116 files, 491 + 25 tests and *23* pairs. Re-running
its own script corrects two of those and the correction is worth keeping: the tree has grown since
(the batch that lands with this document), and **the pair count was one too high** - the list
`ARCHITECTURE.md` §6 prints has 22 entries, which is what the re-measurement finds before Step A.
Step A then takes it to 21, for the reason §11.1.7 records: one of those pairs came from a file the
compiler never saw.

That last row is the finding. At module granularity the kernel is one strongly connected
component: there is no layer that can be compiled, tested or refactored on its own. Every
other item below is a *cause* of that number, which is why the fixes are ordered to reduce it
rather than to make any single file prettier.

---

### 11.1 The findings

Severity is about cost-to-change, not about user-visible bugs: none of these is a defect a
program can observe today.

#### 11.1.1 — V-1: the `Inode` downcast chain (severity: high)

`src/vfs/mod.rs:453–546` gives the central VFS trait a method per concrete device and network
type:

```rust
fn as_fb(&self) -> Option<&crate::drivers::virtio::gpu::FbDev> { None }
fn as_evdev(&self) -> Option<&crate::drivers::virtio::input::EvdevNode> { None }
fn as_uinput(&self) -> Option<&crate::drivers::uinput::Uinput> { None }
fn as_drm(&self) -> Option<&crate::drivers::drm::DrmCard> { None }
fn as_vsock(&self) -> Option<&crate::net::vsock::VsockSock> { None }
fn as_unix(&self) -> Option<&crate::net::unix::UnixSock> { None }
```

This is the textbook abstraction leak: the VFS — which is supposed to know only that a node has
a name, a kind, bytes and an owner — names six device drivers and two network socket types, and
the *syscall* layer reaches through these to drive a device. Adding a seventh device type means
editing the VFS trait, so the VFS can never be compiled without `drivers` and `net`, and
neither can anything that depends on the VFS. It is the largest single contributor to the
23-cycle count.

#### 11.1.2 — V-2: device ioctl ABI lives in the syscall layer (severity: high)

`src/syscall/linux/fs.rs` is 6 311 lines and is the second-largest file in the tree. It is not
only path-carrying file calls; it is also the switchboard for every device's ioctl contract:

| Device | Where its ABI is parsed |
|---|---|
| fbdev (`FBIOGET_*`, `FBIOPAN_DISPLAY`, `FBIO_WAITFORVSYNC`) | `fs.rs` around `:3561` |
| RTC (`RTC_RD_TIME`, `RTC_SET_TIME`) | `fs.rs` around `:3677` |
| DRM/KMS (`drm_ioctl` dispatch) | `fs.rs` around `:3751` |
| uinput (`UI_SET_*`, `UI_DEV_SETUP`, `UI_DEV_CREATE`) | `fs.rs` around `:4001` |
| TTY, block, evdev, termios | `fs.rs` throughout |

A driver change therefore lands in the syscall layer, and the driver cannot state its own
contract next to the code that implements it. The dispatch routing counts make the split
explicit: 135 of the 247 names are handled by `fs.rs`, 50 by `proc.rs`, 50 by `posix.rs`, 12 by
`abi.rs`.

#### 11.1.3 — V-3: the scheduler does other subsystems' cleanup (severity: high)

`sched/runtime.rs` is 6 397 lines — the largest file in the tree — and on task teardown it
reaches into four unrelated subsystems:

```rust
crate::net::socket::close_sockets_at_exit(&files);
crate::vfs::inotify::close_at_exit(&files);
crate::drivers::virtio::input::close_at_exit(&files);
crate::drivers::uinput::close_at_exit(&files);
```

plus `vfs::locks::release_process` and `vfs::mount::fs_id_of_inode` in the same path, and
`binfmt::shebang`, `vfs::read_file`, `vfs::tty::global` and `vfs::tty::by_dev_id` in the exec
path. The scheduler owns *tasks*; it should not know that inotify exists. Each new
per-open-file resource adds a line here, and each line makes `sched` unbuildable without `net`,
`vfs` and `drivers`.

#### 11.1.4 — V-4: the terminal emulator and console live inside the VFS (severity: medium)

`src/vfs/term/` holds the cell grid, the key handling and an embedded 4 KiB `vga8x16.bin` font.
`src/vfs/tty.rs` (2 359 lines), `src/vfs/pty.rs`, `src/vfs/devpts.rs` and `src/vfs/vcmux.rs`
implement the console, and `src/console.rs` — the kernel's own logging console — reaches *into*
the filesystem layer to draw:

```rust
crate::vfs::term::with_screen(active_grid(), |g| g.feed(bytes));
crate::drivers::video::render::notify();
```

The direction is wrong: a terminal emulator is not a filesystem. `/dev/tty` being a node is a
filesystem concern; how a cell is painted and which of six virtual consoles is active is not.

#### 11.1.5 — V-5 and V-9: user-pointer validation owned from below (severity: medium)

`drivers/drm.rs:595,741` calls `crate::syscall::buffer_ok(...)`, and `net/socket.rs` /
`net/unix.rs` call `crate::syscall::buffer_ok` and `err_of` dozens of times. A driver validating
a *user* pointer is an inversion — the driver is below the syscall layer in every other respect
— and it exists because the helper happens to live there. The helper itself is fine; its home
is not.

#### 11.1.6 — V-6: a temporary trace in the dispatch hot path — **DONE**

`syscall/linux/mod.rs::trace_msg` was self-labelled:

> TEMPORARY (udevd worker-channel hunt): ... Remove once the site is known.

It fired on every failed `sendto`/`recvfrom`/`sendmsg`/`recvmsg` and printed up to 60
`[msgtrace]` lines from inside the dispatcher. The hunt it belonged to is recorded as finished
in §4.1's udevd note, so it was leftover scaffolding standing in the one path every syscall
crosses. **Removed, and the four arms are plain calls again** (§11.2 Step A).

The class is worth watching, because it happened twice: the same hunt later left a second
instrument behind - a `[recvtrace]` wrapper around `net::socket::handle_recvmsg` (the original
function renamed `recvmsg_inner`, a logging body around it, and `console.rs`'s `DMESG_CAP`
temporarily raised so the ring would not evict the lines). It was still there when the session
that added it ended, with the bug it was hunting found by reading eudev's source instead: the
`recvmsg` that failed was `EINVAL` for a *zero-length iovec* (§4.1). **Removed**, and the
wrapper collapsed back into one function; the per-task cost of the shape is that a diagnostic
in the dispatcher or the socket layer is easy to leave, and neither is in anybody's diff when
the session ends.

#### 11.1.7 — V-7: orphan and duplicate machine modules — **DONE**

| File | Was |
|---|---|
| `src/arch/x86_64/serial.rs` (119 lines) | **not declared in `arch/x86_64/mod.rs`** → never compiled. A near-duplicate of `drivers/serial.rs` (324 lines), same doc header |
| `src/arch/x86_64/qemu.rs` (23 lines) | Not declared → never compiled. **Byte-identical** to `drivers/qemu.rs` |
| `src/arch/x86_64/pci.rs` | Declared, compiled, **referenced nowhere**. A stale earlier copy of `drivers/pci.rs` (discovery-only, one function per slot, bus 0) |
| `src/arch/x86_64/rtc.rs` vs `src/drivers/rtc.rs` | Both live and both used — but the names collided and the split was not obvious from either name |

Three modules named `pci`, two named `rtc`, two named `serial`, two named `qemu`; two of the
twelve were dead and one was a duplicate, which is how a reader learns to distrust the tree.

**All three files are deleted** and the module declaration with them; `drivers::rtc` is renamed
`drivers::rtc_dev` (the node), which leaves `arch::x86_64::rtc` as the only thing called `rtc`
and the clock as the one that owns the name. The `console.rs` module header, which described
itself as formatting over `arch::serial`, now names `drivers::serial`.

*One number fell out of the deletion that is worth keeping.* §11.0's extraction counts mutual
module pairs by reading every file under `src/`, and it read the two orphans even though the
compiler never did: the `arch <-> vfs` pair existed **only** because `arch/x86_64/serial.rs`
mentioned `crate::vfs`. Deleting it takes the count from 22 to 21 - a reminder that an audit by
text search measures the tree, not the build, which is the same lesson V-7 itself is about.

#### 11.1.8 — V-8: the aarch64 configuration points at a missing file — **DONE**

`.cargo/config.toml` set `-C link-arg=--script=arch/aarch64/linker.ld`; only
`arch/x86_64/linker.ld` exists. The target could not link. §9.1 calls aarch64 "under
development", which understates it: there is no linker script, and `src/arch/mod.rs` says the
modules will be declared "when they are written rather than now".

**The profile is gone** (`[target.aarch64-unknown-none]` in `.cargo/config.toml`, and the target
in `rust-toolchain.toml`), with a comment in its place saying what it was and where the port's
real prerequisites are (§9.1). Re-adding it is one line, next to a linker script that exists -
which is the point: the configuration no longer advertises a target that cannot be built. The
alternative the step offered - writing the linker script now - would have been a claim about a
port nobody has started, and this step is subtraction.

#### 11.1.9 — V-10: the handoff ABI is written twice (severity: accepted)

`boot/src/handoff.rs` and `src/boot/handoff.rs` are separate definitions of one record, and
`BOOT_STACK_CANARY` is spelled out in `src/main.rs` as well as in the loader. This is deliberate
— the loader is a UEFI application on a different target and cannot link the kernel crate — and
it is documented on both sides. Recorded so it reads as a decision rather than an oversight.

#### 11.1.10 — V-11: a host test module behind a never-host-compiled parent (severity: low)

`src/lib.rs:48` gates `pub mod net` on `target_os = "none"`, so the module is absent from every
host build — while `src/net/socket.rs:3527` annotates its test module
`#[cfg(all(test, not(target_os = "none")))]`, i.e. "run me on the host". The annotation is only
ever evaluated in a build that does not include the file, so those three tests have never run.
`net`'s doc comment says its tests "are the conformance programs that run on the machine",
which is true of the rest of `net` and false of this file.

#### 11.1.11 — Sizes, for the record

| File | Lines |
|---|---|
| `src/sched/runtime.rs` | 6 397 |
| `src/syscall/linux/fs.rs` | 6 311 |
| `src/net/socket.rs` | 3 642 |
| `src/vfs/ext4/mod.rs` | 3 391 |
| `src/syscall/linux/proc.rs` | 3 039 |
| `src/syscall/linux/abi.rs` | 2 903 |
| `src/drivers/drm.rs` | 2 371 |
| `src/vfs/tty.rs` | 2 359 |
| `src/vfs/procfs.rs` | 1 996 |
| `src/vfs/mount.rs` | 1 681 |
| `src/main.rs` | 1 625 |

Long files are a symptom here, not the disease: `ext4/mod.rs` is long because a filesystem is
large, and splitting it would buy nothing. `sched/runtime.rs` and `syscall/linux/fs.rs` are long
*because each holds several subsystems' concerns*, which is what 11.1.2 and 11.1.3 are about.
That distinction is what the plan below is built on: split by responsibility, not by length.

#### 11.1.12 — V-12: the in-flight refactor, and the green baseline it was blocking (resolved)

This item was written while the tree did not build: an in-flight refactor (the `user/abitest`
programs moving into `syscall/` and `integration/` subdirectories, plus edits across `vfs/ext4`,
`vfs/tty.rs` and the syscall layer) left `cargo test -p mpos --lib --target
x86_64-unknown-linux-gnu` **failing to compile** — `Tty`/`TtyOutput` unresolved in `vfs/tty.rs`
and a borrow error in `vfs/ext4/super.rs:501` — and it said no step below should begin until
that work was finished or reverted, because every acceptance criterion here is "the host tests
still pass".

**It is finished and committed, and the baseline is green.** `cargo xtask test` reports `498
passed` host unit tests, `29` from xtask, `=== SUMMARY pass=120 fail=0` from the guest suite, and
QEMU exit `33` (the clean debug exit). The refactor is what the batch of commits ending at this
one carries: the suite split into two tiers, and the session's kernel work (seccomp's filter
mode, the terminal's window, ramfs link counts, the ext4 host tests, a cross-task user copy that
could reach a supervisor page, and the boot task's park that cost a clock tick on every wake).

So the prerequisite is met and **Step A may begin**. It is the one item in this phase that needs
no green baseline anyway — it is a deletion — but it has one now, which is what makes its own
acceptance criterion ("the host test suite and a boot are unchanged") checkable.

---

### 11.2 The remediation plan

Ordered so each step is independently verifiable, behaviour-preserving, and leaves the machine
bootable. Every step states its acceptance criterion as something a test can decide.

The dependency direction to converge on (also in `ARCHITECTURE.md` §9):

```
drivers, net, blk, pci, rand, uevent   ->  may use arch, mm, sched, sync
      vfs, sched                       ->  may use mm, arch, sync, blk, pci
     syscall                           ->  may use vfs, sched, mm, net, drivers
       main                            ->  may use everything
```

Two hard rules: **nothing below `syscall` names `syscall`**, and **nothing outside `vfs` names
the VFS's concrete device types**.

---

#### Step A — Subtract: dead modules, the leftover trace, the phantom target (V-6, V-7, V-8) — **DONE**

*No behaviour change. Do it first: it is a deletion, and it removes the clutter that makes the
rest of the audit hard to read.*

| Change | Where |
|---|---|
| Delete `src/arch/x86_64/serial.rs`, `src/arch/x86_64/qemu.rs` (uncompiled) and `src/arch/x86_64/pci.rs` (compiled, unreferenced) | `src/arch/x86_64/` |
| Delete `trace_msg` and its call site from the dispatcher | `src/syscall/linux/mod.rs` |
| Either add `arch/aarch64/linker.ld` or drop the `aarch64-unknown-none` rustflags and target, so the configuration stops naming a file that does not exist | `.cargo/config.toml`, `rust-toolchain.toml` |
| Disambiguate the colliding names: `drivers/rtc.rs` → `drivers/rtc_dev.rs` (the *node*), leaving `arch::x86_64::rtc` as the clock; `drivers/qemu.rs` becomes the only `qemu` | `src/drivers/` |

**Acceptance:** `git grep 'arch::x86_64::serial\|arch::x86_64::qemu\|arch::x86_64::pci'` returns
nothing (the one stale doc-comment mention in `console.rs` is corrected); the host test suite and
a boot are unchanged apart from the absence of the `[msgtrace]` lines; `.cargo/config.toml` no
longer references a nonexistent path.
**Risk:** the only real risk is deleting something referenced through a macro. Grep first; for
two of the three files the absence of a `mod` declaration is proof enough.

**Done, and what the run said.** All four rows landed, one commit, no behaviour change.

- The three files are gone and `pub mod pci;` with them.
- `trace_msg` and its four call sites are gone; the socket arms are plain calls again, and the
  only sign the trace existed is that a failed `sendto` no longer prints a line.
- The aarch64 profile is gone from **both** files rather than the linker script being written:
  §11.1.8 has the reasoning, and the short form is that this step subtracts.
- `drivers/rtc_dev` is the rename, done with `git mv` so the history follows it, and its module
  header now says why the name carries `_dev` (`arch::x86_64::rtc` is the clock).

`git grep` for the three old paths returns nothing; the host suite is still **498 passed, 29 in
xtask**; `cargo xtask test` ends the same way it did before the change - `=== SUMMARY pass=120
fail=0` and QEMU exit **33** - which is the "unchanged apart from the `[msgtrace]` lines"
criterion, checked by running it rather than by reading the diff.

The one number that moved is the pair count, from 22 to 21, and §11.1.7 says why: the extraction
reads files the compiler never saw.

---

#### Step B — Give devices a seam the VFS can hold without naming them (V-1, V-2, V-5)

The largest step, and the one that pays for the others: it removes the largest cause of the
23-cycle count.

**B1 — Replace the six `as_*` downcasts with one typed handle.** Introduce a `vfs::DevNode` so
that a char-device node carries an `Arc<dyn DevOps>` whose *own* module defines the operations
(`read`, `write`, `ioctl(cmd, arg)`). The trait lives in `vfs`; the implementations live in
`drivers`/`net` and are *installed* at boot by the code that owns them. The syscall layer then
calls `node.dev().ioctl(nr, arg)` instead of matching on a device type. (A single
`as_any`/`downcast_ref` pair over `core::any::Any` is the smaller alternative; either is fine —
the point is that `vfs` stops naming the types.)

**B2 — Move each device's ioctl contract down to its driver.** For each family in 11.1.2
(fbdev, DRM, RTC, uinput, tty, evdev, block), move the request-number constants, struct layouts
and validation into the driver module as `pub fn ioctl(req, arg, from_user) -> isize` (or a small
table), leaving `syscall/linux/fs.rs` with the dispatch to that entry point. The *pure* parts stay
host-compiled and keep their tests; `drivers/drm.rs` already carries most of the DRM UAPI, so it
is closer to this shape than it looks.

**B3 — Move the user-pointer helper out of `syscall`.** `buffer_ok`/`err_of` answer "is this range
readable/writable by the current task", which is an `mm` question. Add
`mm::user::{validate, copy_in, copy_out}` and have `syscall`, `net` and `drivers` all call that.
This deletes V-5 and V-9 at once.

**Acceptance:**
- `crate::drivers::` and `crate::net::` no longer appear in `src/vfs/mod.rs` (the six `as_*`
  methods are gone).
- `crate::syscall::` no longer appears anywhere under `src/drivers/` or `src/net/`.
- `wc -l src/syscall/linux/fs.rs` drops materially from 6 311.
- Every host test still passes, and `cargo xtask test` still boots with every `/bin/t-*`
  reporting `=== PASS` — with `t-drm`, `t-drm-args`, `t-fbdev`, `t-evdev`, `t-ioctl`, `t-seat`,
  `t-prime` and `t-unix*` as the programs that specifically exercise the moved paths.
**Risk:** this touches every device path at once. Do it one device at a time (fbdev first — the
smallest contract; DRM last — the largest), keeping that device's conformance program as the
gate, and never batch two devices into one commit.

---

#### Step C — Let subsystems own their per-open-file resources (V-3)

`close_at_exit` ×4 in the scheduler becomes a **registry**: the scheduler keeps a list of hooks
that each subsystem registers at boot, and on task exit it walks the list, naming nothing but its
own trait. A subsystem that registers nothing costs nothing.

For the exec path, decide per edge rather than abstracting for symmetry: `binfmt::shebang` and
`vfs::read_file` are genuinely needed to load an image, so either accept the `vfs` edge and
document it as allowed ("the scheduler loads images from a filesystem" is not a leak in the way
that "the scheduler closes sockets" is), or hold a narrow `ImageSource` trait. The rule to apply:
*is this the scheduler's business, or somebody else's?* — per-open-file resource ownership is
somebody else's; reading an executable is the scheduler's.

**Acceptance:** `sched/runtime.rs` contains no `crate::net::`, no `crate::drivers::` and no
`crate::vfs::(inotify|locks)` reference; a task that held a socket, an inotify watch, an evdev
grab and a uinput device is still cleaned up correctly (`t-inotify`, `t-evdev`, `t-filelock`,
`t-unix-scm` are the gates); `wc -l src/sched/runtime.rs` drops from 6 397.
**Risk:** teardown order matters — a descriptor's resource must go before the table entry. Pin
the order with a host test on the registry itself, then keep the guest tests as the end-to-end
gate.

---

#### Step D — Move the terminal emulator out of the VFS (V-4)

Create `src/term/` (grid, keys, font, escape parsing) and `src/console/vcmux`, leaving in `vfs`
only what is a filesystem fact: `/dev/tty*` and `/dev/pts/*` nodes, the line discipline as the
node's behaviour, and `termios`. `console` then depends on `term`, and `vfs` depends on neither
`console` nor the render task.

**Acceptance:** `vfs/` contains no font asset, no escape-sequence parser and no `vcmux`;
`crate::vfs::term` is gone; `console.rs` no longer reaches into `vfs` for the grid; the three
`vfs/term` test modules keep passing after the move (they are pure, so the move is mechanical);
`cargo xtask run --mode mirror` still paints the same screen and `Ctrl-A 1..6` still switches
virtual consoles.
**Risk:** `vfs/term/vga8x16.bin` is `include_bytes!`-ed — a missed path is a compile error, which
is the good outcome.

---

#### Step E — Split `fs.rs` along its real seams (follow-up to B2)

After B2 the file should be much smaller. What remains is still two jobs: the path-carrying calls
(`open`, `stat`, `getdents64`, …) and the device-ioctl entry point. Split into
`syscall/linux/file.rs` and `syscall/linux/dev.rs`, or leave it once B2 has removed the
switchboard — whichever the size says.

**Acceptance:** the dispatch routing counts are unchanged, no handler changed meaning, host tests
unchanged.

---

#### Step F — Make the target gate a rule, not a habit (V-11 + the `ARCHITECTURE.md` §8 drift)

1. Settle the `net` case: either declare `pub mod net` for the host too — which means moving the
   pure parts of `net/socket.rs` out from under the gate, since `smoltcp` is gated in
   `Cargo.toml` — or delete the unreachable test module and say so. Pick one; do not leave the
   annotation lying.
2. Write the criterion down (`AGENTS.md` §4 and a header comment in `src/lib.rs`): *a module is
   host-compiled when its logic touches no hardware, no port I/O, no virtio ring and no real
   clock — and then its dependencies must be host-compiled too, transitively.* The transitive
   half is the part that gets forgotten.
3. Add a cheap mechanical check: a host test in `xtask` that walks `src/**/*.rs` and fails when a
   `#[cfg(test)]` module sits in a file whose nearest declaring parent is gated
   `target_os = "none"`. Crude, but it catches the V-11 class without a compiler change.

**Acceptance:** the check fails on today's `net/socket.rs` and passes after the fix;
`cargo test -p mpos-xtask` covers it.

---

#### Step G — Keep the graph from regressing

Once A–F have landed, freeze the result. Add to `xtask` a `depgraph` subcommand (or a host test)
that re-runs §11.0's extraction and asserts:

- no edge from a lower layer into `syscall` (`drivers`, `net`, `blk`, `pci`, `rand`, `uevent`,
  `vfs`, `sched` must not reference `crate::syscall::`);
- no `crate::drivers::` or `crate::net::` reference from `vfs/mod.rs`;
- the measured cycle count is at or below the recorded baseline.

The baseline is what makes it useful: a **ratchet**, not a cliff. Store it as a constant next to
the check with a comment naming the date and the number, so a reduction is a commit somebody can
make and a regression is a failing test.

**Acceptance:** `cargo xtask depgraph` reports the current edge and cycle counts and fails when
either worsens.

---

### 11.3 Order, and what is deliberately not in this plan

**Do this, in this order:**

1. ~~**V-12** — finish or revert the in-flight refactor.~~ **Done**: the tree builds and the
   whole gate is green (§11.1.12), so everything below is now verifiable.
2. ~~**Step A** — pure subtraction, ~5 files, one commit, no behaviour change.~~ **Done**
   (§11.2 Step A): the three dead modules, the `[msgtrace]` trace and the aarch64 profile are
   gone, and the pair count it was measured with fell from 22 to 21.
3. **Step B** — the device seam, one device at a time. This is where the 23 becomes a real
   number, and it is the largest single piece of work in the phase.
4. **Step C** — the resource registry.
5. **Step D** — the terminal emulator move.
6. **Step E** — the `fs.rs` split, if B2 has not already made it unnecessary.
7. **Step F** — the gate criterion and the check for it.
8. **Step G** — the ratchet.

Steps A, D and F are mechanical and could be done by anyone with an afternoon; B and C need
someone who knows the device and scheduler paths. Nothing here has to land before the feature
work in Phases 1–10 — but every feature lands *cheaper* after B and C, which is the argument for
doing them before the next device or the next per-open-file resource is added.

**Deliberately not in this plan:**

- **Rewriting the `Inode` interface.** The trait itself is fine; it is the six device methods
  hanging off it that are wrong. B1 removes them and stops there.
- **A message bus, an async runtime, or a generic HAL trait forest.** The kernel is uniprocessor
  and synchronous by design, and `ARCHITECTURE.md` §7's locking rules exist because that is what
  makes the design legible. This phase is about *who names whom*, not about adding indirection.
- **Splitting files because they are long.** `vfs/ext4/mod.rs` (3 391 lines) stays as it is.
- **Restructuring `src/main.rs` (1 625 lines).** The boot sequence, the milestone checks and the
  panic handler do belong in the binary; if it ever becomes worth splitting, the checks move to
  `src/main/checks.rs`, and that is a tidiness commit rather than an architecture one.
- **Fixing V-10.** Accepted, documented, and cheaper to live with than a shared crate across two
  targets would be.

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
| **P11: Architectural debt** | A done; B–C ~3–5 weeks; D–G ~1 week | High (maintainability) — the number was 22 mutual module pairs, and Step A took it to 21 | Every later phase: nothing can be tested or refactored in isolation (§11) |

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

**P11 Step A is done** — a deletion, landed on the green baseline the tree now has. P11's Steps B and C are the ones worth doing before the next device driver or the next
per-open-file resource is added, because each of those makes the debt one line larger.

**For the display work specifically**, §7.4 carries its own order, and its first step is not a
kernel change at all: a wlroots compositor on `WLR_RENDERER=pixman` is expected to run on the
DRM surface as it stands, which makes "boot one and read what it says" the cheapest way to find
out what that analysis has wrong.

The first two phases (epoll/timerfd + signal delivery) are the highest impact — they unlock the majority of real-world Linux userspace programs.
