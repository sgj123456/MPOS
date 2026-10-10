# MPOS Roadmap

What MPOS is building, phase by phase, and where each phase stands: the objectives,
the checklist, the remaining work and the priority — and nothing else.

Every measurement, gate run, lesson and trap behind a claim here lives in
[`NOTES.md`](NOTES.md), which is where a design decision's reasoning belongs. Read
this file to know *what to do next*; read `NOTES.md` to know *why the code is the
way it is*. A `(§N)` pointer on a line here names the `NOTES.md` section that
carries that item's evidence.

---

## Status board

One-glance status of the whole roadmap. `[x]` done, `[~]` partial/served with a
named gap, `[ ]` not started. Each line points at its detail section below.
Full-ABI gaps still falling through to `ENOSYS` are collected under **ABI**.

Legend: `[x]` done · `[~]` partial · `[ ]` todo · *(blocked: …)* = needs a prerequisite first

**Core (Phases 1–3)**
- [x] P1 Syscall completion — epoll · timerfd · 67/68 other calls (§1)
- [~] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2)
- [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)

**Networking (Phase 4)**
- [x] 4.1 AF_UNIX — `SOCK_STREAM` bind/listen/connect/accept · data path ·
      `SCM_RIGHTS` · `SCM_CREDENTIALS` · `SO_PEERCRED`, and `SOCK_DGRAM`
      datagram transfer with whole-message boundaries and per-message
      `SCM_CREDENTIALS` for a `SO_PASSCRED` reader (`t-unix-scm`, `t-socketpair`)
- [x] 4.2 ICMP — raw socket, complete-packet delivery, `IP_RECVTTL` (§4.2)
- [~] 4.3 TCP — the loopback, the listener and `SO_REUSEADDR` are done; two
      options are not
- [x] 4.4 Netlink write side — `ip addr`/`route`/`link` applied live (§4.4)
- [~] 4.5 `AF_PACKET` — the link layer is a socket: `SOCK_DGRAM` `bind`/
      `sendto`/`recvfrom`/`getsockname`, a tap at the NIC seam that feeds a client
      before the IP layer has decided the frame is anybody's, and a userland DHCP
      client *can* ask for an address now (`t-dhcp`). Named gaps: `SOCK_RAW`'s
      header-included frames are `EOPNOTSUPP`, and the boot itself still ships no
      client — the static default stays until `udhcpc` is wired (§4.5)

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
- [x] teardown at **exit**, not at reap: an exited task gives back its address
      space and every descriptor the moment it exits, which is Linux's own
      moment for it (`do_exit` closes the files and tears the `mm` down before
      the parent is told). It used to hold all of it until `wait(2)` collected
      the corpse — so an unreaped pid held its frames (a killed compositor kept
      every llvmpipe arena it had allocated) and, worse, held its descriptors:
      the DRM master, a seat manager's connection, pipes, POSIX locks. That is
      what kept a fresh compositor out of the seat after a `kill`, and what
      could leave a pipeline reader waiting for an end-of-file that never came.
      Only the slot and the exit code stay now, which is all a zombie is.
- [~] 6.3 Namespaces — **UTS is built** (§6.3); MNT, PID and NET are designed
      and not built. Eight kinds, four of them with something observable to
      isolate, ordered UTS → MNT → PID → NET, each with its own `t-ns-*` pin that
      holds the child's view differ *and the parent's not* — `t-ns-uts` is the
      first of them. `unshare(CLONE_NEWUTS)` hands the caller a **copy** of its
      two names, and `sethostname`/`setdomainname`/`uname`/
      `/proc/sys/kernel/{hostname,domainname}` all read the caller's; the other
      seven answer `EOPNOTSUPP`, and `setns` still answers `EINVAL`/`EBADF`
      because no namespace can be named by a descriptor here (`/proc/<pid>/ns/*`
      does not exist). USER, IPC, CGROUP and TIME keep refusing, because a
      namespace with nothing behind it isolates nothing. The flag table is
      Linux's, which it was not before this work (a mount-namespace request had
      been answered `EINVAL`, because `CLONE_NEWNS` was spelled with
      `CLONE_THREAD`'s value)

**ABI gaps → `ENOSYS` (remaining)**
- [x] inotify · [x] `memfd_create` (anon shared buffers, §7/Stage 3) · [~] xattrs (validate → `EOPNOTSUPP`, no store) (§Remaining)
- [ ] `fanotify_init`/`fanotify_mark` · `keyctl` (kernel keyring)
- [ ] `userfaultfd` · `tid/tgid_add_object` (rseq) — niche
- [x] real-time signals `32..=64` (stages A+B+C) — the pending mask is a canonical 64-bit
      map, the disposition table spans `1..=64` so `rt_sigaction` installs an RT signal and
      `kill`/`tgkill`/`sigqueue`/`timer_create` name the range, and real-time signals are now
      **queued** (a bounded per-task FIFO ring delivers every copy with its own `si_value`;
      standard `1..=31` still coalesce, as Linux). Residual, named: the ring is fixed-length,
      so a flood past `RT_QUEUE_LEN` drops silently where Linux's per-user `RLIMIT_SIGPENDING`
      makes `sigqueue` return `EAGAIN` (§2.3; pinned by `t-rt-signal-queue` + `RtQueue` tests)
- [x] POSIX timers 222–226 — `vim`'s startup probe. The five calls are routed,
      the expiry scan is the scheduler's tick, and the `SIGEV_THREAD_ID` +
      `SIGRTMIN` shape musl builds for a callback timer works end to end; the
      shapes were re-measured on Linux 6.x (a null `evp` is
      `SIGEV_SIGNAL`/`SIGALRM`; CPU-time clocks are `EOPNOTSUPP`) (§Remaining)
- [ ] `io_uring_setup`/`enter`/`register` — deferred by design (§1.3)

**Drivers (Phase 7)**
- [~] 7.1 Display — `/dev/fb0` (mmap + FBIOGET_* + PAN/VSYNC present) and
      `/dev/dri/card0` + `/dev/dri/renderD128` (resources, dumb buffers, addfb2,
      atomic commits, page-flip + vblank events, PRIME both ways) — which with
      Mesa's GBM and EGL is weston-on-GL, in software — and a framebuffer belongs to the description that made it and is released with it (Linux's `drm_file_free` → `drm_fb_release`), so `GETRESOURCES` lists live framebuffers rather than ids a dead session left behind (niri could not open the card when it read the properties of one: “Error reading properties on device `/dev/dri/card0` (Invalid argument)”) — and a resized or
      fullscreened QEMU window *follows Linux's virtio-gpu semantics*: the kernel
      re-reads the host's size, advertises it as the connector's mode and fires a
      `HOTPLUG=1` uevent, then leaves the scanout alone until the KMS master (an
      atomic `MODE_ID` or legacy `SETCRTC`) commits it — and only that commit, run
      on the render task, drives `change_resolution`. A text console with no
      compositor to commit keeps the size it booted at, exactly as Linux's fbcon
      does. No 3D/virgl, no sync objects, no format modifiers, no EDID (§7.1, §7.4)
- [x] 7.2 Input — virtio-input evdev: capability ioctls (`EVIOCGBIT`/`EVIOCGABS`/
      …), `/dev/input/eventN` + `/sys/class/input` + `SUBSYSTEM=input` uevents, and
      events that belong to the description that grabbed them (§7.2)
- [x] 7.3 AHCI/SATA (stretch) — **done**, and the stretch item turned out to carry
      a real one: a SATA disk is a *second* disk, so the block layer learned to
      hold more than one and to name each under its own driver's name, major and
      device-tree position (`vda`/254/`/devices/virtio/block`, `sda`/8/`/devices/ahci/block`).
      The driver probes by class code, drives one command at a time through slot 0
      and checks `PxTFD` after `PxCI` clears rather than trusting the bit, and
      `t-ahci` pins the whole path on the machine. Two of its decisions were
      corrected by booting it - `PxSIG` is not the presence question, and an
      unpartitioned disk still has a name - and both are written down in §7.3
- [x] `/dev/uinput` — a virtual input device a userspace program creates and
      writes events into: the self-test's hands, so `t-evdev` pins *whose* event
      a record is on the machine and not only in the host unit tests. Gets a
      `/dev/input` node and an `add` uevent; no `/sys/class/input` entry (sysfs
      builds its tree once at boot), no `UI_ABS_SETUP`, no force feedback (§7.2)

- [x] the `/dev` node set is assembled by the drivers that own it: `devices::publish` is a
      registry a driver fills at the install door it already has, and `standard_devfs` names no
      driver — so a new device does not edit the builder and a probe's answer, not the builder's
      guess, decides whether a node exists (§7.5)

- [x] configuration space over MMIO (ECAM/MMCONFIG), the 0xcf8/0xcfc pair as the fallback:
      `crates/acpi` reads what the firmware names, the loader hands the kernel the root
      pointer, and `drivers::pci` maps the window one bus at a time and reads the bus
      through it — but only after walking the bus *both* ways and comparing the two
      snapshots function by function, so a window this machine's ports disagree with is
      refused and the ports stay (§7.6a)

- [ ] **the window's other 3840 bytes**: a function's configuration space is 4 KiB and this
      kernel still publishes 256 — the port mechanism's reach, `CFG_SIZE` — so PCIe extended
      capabilities are outside every reader. Not joined to the item above on purpose: it
      changes the size of a `/sys` file, and the window is what makes it *possible* rather
      than what makes it right (§7.6a)

**Wayland compositor (weston) support** — the cross-cutting work a rootful
compositor needs, all verified by `t-*` (GUI end-to-end is the operator's run of
`cargo xtask run --mode gui`):
- [x] Peer credential + fd passing (SCM_RIGHTS/`SO_PEERCRED`) — Stage 0 (see §4.1)
- [x] libinput device classification — evdev capability ioctls — Stage 1, `t-evdev`
- [x] Device discovery — `/dev/input`, `/sys/class/input`, input uevents — Stage 2
- [x] `wl_shm` buffers — `memfd_create` + frame-aliased shared mappings — Stage 3, `t-memfd-shm`
- [x] Framebuffer present — `FBIOPAN_DISPLAY`/`FBIO_WAITFORVSYNC` + a pointer
      (virtio-tablet) + `--mode weston` (kernel frees the scanout) — Stage 4, `t-fbdev`
- [x] DRM/KMS surface — `/dev/dri/card0` (UAPI ioctl + struct ABI, dumb buffers,
      page-flip, vblank events) for DRM-backend compositors — Stage 5, `t-drm`
- [x] **the screen has an owner** — `VT_SETMODE`'s `VT_PROCESS` negotiation
      (relsig → `VT_RELDISP`, bounded by a deadline and by a second `^A`), a DRM
      master bound to a console and revocable from it, `EACCES` on every
      presentation request from a session that lost its console, and `POLLHUP` on
      the card fd; before this a `^A` to a text VT lost the screen back to
      weston on the next mouse move (§7.1, `t-seat`)
- [x] `start-weston.sh` survives being run twice — it asks whether a seatd is
      *answering* rather than whether `/run/seatd.sock` exists (a socket path
      outlives the daemon that bound it, here as on Linux), removes the stale name
      before a new daemon tries to bind it, and starts seatd in its own session:
      the script `exec`s into weston, so a seatd backgrounded there shared the
      compositor's process group and `^C` on the console - which signals the whole
      foreground group - used to kill the seat manager along with the desktop
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
- [x] a `--mode gui` terminal takes keyboard input — with the serial line
      discarded the only path in is virtio-input, and a keystroke reaches the
      active console's shell unless a compositor is *both* grabbing the device and
      holding the screen; that conjunction, not the grab alone, is what takes the
      keyboard off the shell, so a plain GUI console (nothing has grabbed the
      device) is typable again — as is the same console after `^A` moves its
      session off the screen while a stale claim survives (§7.1, `input::service`)
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

**Compositors beyond weston (niri, wlroots)** — what a *second* compositor family
needs on top of the line above. The analysis is §7.4. One of these steps has now
been walked - niri 26.04 was booted in the guest and stopped on the first line of
it - so the entries say which they are:
- [x] the **sysfs side of a DRM node's identity**. A reader that holds nothing
      but a card's device number now reaches the GPU that node belongs to and
      that GPU's `drm/` listing (`/sys/dev/char/226:N/device/drm`), which is what
      the Rust `drm` crate's `is_device_drm` stats and what its `dev_path` reads
      by name to find the sibling node. niri's first stop was exactly this
      missing directory - it refused a card whose `/dev` node and `st_rdev` were
      both right as "the provided file descriptor does not refer to a DRM node"
      (§7.4). Pinned by a host test. **niri was booted again since**, and stopped
      on the same sentence of the same panic for a second reason: the chain was
      in the kernel and the card was still refused, because the `..` inside a
      link's own target could not be resolved on a machine-sized root filesystem
      (§7.4)
- [x] the **DRM card found by its driver's declaration, not by the class code**.
      The sysfs builder used to hang the card off the first display-class
      (`0x03`) function that had a driver bound; on a q35 that matched the
      firmware's own VGA before the `virtio-gpu`, and Mesa read the card back as
      `pci id for fd 3: 1234:1111, driver (null)` - a display adapter with no
      driver, where the card it held is `1af4:1050`. The fact is now *carried*
      rather than guessed: the virtio-gpu driver declares its own function in the
      shared `pci` record (`crate::pci::mark_drm_card`, reached through
      `crate::drivers::pci::mark_drm_card` and `virtio::hal::claim_drm_card` at
      GPU init), and `vfs::sysfs` reads `device.drm_card`. The dependency stays
      `drivers -> pci` and `vfs -> pci` - no new `drivers -> vfs` edge, so
      `ARCHITECTURE.md` §2/§3 are unchanged (AGENTS.md §3 rule 5). Pinned by the
      host test `the_card_hangs_off_the_function_its_driver_declared`, which is
      red against the class-and-driver guess (it hangs the card off a second
      bound display function enumerated first). Closes the "part still to do"
      that the earlier sysfs comment named (§7.4).
- [x] the **`..` in a symlink's target**: a relative target is spliced beside
      the link it came from, and the names walked to reach that link *are* the
      ancestors the target climbs - so the walk now keeps them instead of
      restarting from the link's directory and searching the whole tree for each
      parent (`Vfs::parent_of`, bounded, and silent when the bound is reached).
      It is not display work, and it was not only niri's problem: a root with 483
      directories at depth four or less spends that search's budget before it
      reaches any depth, so every `../../` link in `/sys` failed at once - the
      card's identity, the parent's node names, and the input nodes' `device`
      links with them (§7.4)
- [x] the **socket a compositor binds** — `bind` with Rust std's short `addrlen`
      (24 bytes, not the struct's 110) and `listen(fd, -1)`: a negative backlog is
      Linux's `SOMAXCONN`, not `EINVAL`, and the one rule is now read by the IPv4
      pool too, which had read `-1` as zero (`net::unix::clamp_backlog`, §4.1).
      This was niri's third stop (`ListeningSocketSource::new_auto`,
      `src/niri.rs:2435`), and the boot carrying the fix reaches `listening on
      Wayland socket: wayland-1` where the previous one left a zero-byte lock
      file and nothing else. Pinned by `t-unix` and `t-tcp-listen`, each shown red
      against the kernel built before it (§7.4)
- [x] the **primary node's resource enumeration** — `GETRESOURCES`' two-call
      idiom: a zero count beside the dangling non-null pointer an empty `Vec`
      carries is not `EFAULT`, because Linux copies only when there is something
      to copy. This was the call niri's `DrmDevice::new` stopped on once its
      socket existed (`resource_handles()`, "Bad address (os error 14)"), and it
      fires on every first enumeration - the card has no framebuffers before its
      first commit. Pinned by `t-drm` (§7.4)
- [x] the **property shapes a compositor sorts by** — every `OBJECT` property
      carries its one value, the object-type tag, because a caller reads
      `values[0]` the moment it sees the flag; drm-rs panicked with "index out of
      bounds: the len is 0 but the index is 0" where the planes' `FB_ID` and
      `CRTC_ID` advertised none. Pinned by `t-drm-props` (§7.4, which also names
      the `ATOMIC` flag this kernel does not model yet)
- [~] the DRM surface a **wlroots pixman** compositor needs — a primary node with
      `SET_MASTER`, `DRM_CAP_DUMB_BUFFER`, `CREATE_DUMB`/`MAP_DUMB`,
      `PRIME_HANDLE_TO_FD`, `ADDFB`/`MODE_ATOMIC` — is already in place, which
      makes `WLR_RENDERER=pixman` the cheapest second-compositor experiment there
      is; no wlroots compositor has been booted yet, and that is the whole of
      what is missing (§7.4)
- [x] **threads** — `clone(CLONE_THREAD)` puts the child on the caller's memory
      and its descriptor table, with `CLONE_CHILD_CLEARTID` + a real `futex`
      wake for `pthread_join`, `exit_group` on the group, and `getpid`/`gettid`
      answering the group and the task. Pinned by `t-clone` (§6.4). What the
      compositors beyond weston still want is elsewhere on this list
- [x] DRM sync objects — the `SYNCOBJ_*` family, including `SYNCOBJ_EVENTFD`
      (which Smithay probes before it will expose `wp_linux_drm_syncobj_v1`),
      with `IN_FENCE_FD`/`OUT_FENCE_PTR` given the behaviour their names promise
      (§7.4). The semantics were **measured on the real card before any of it was
      written**, and they disagree with the sources in the one place that decides
      the design: an object with no fence is refused rather than treated as
      ready. `crates/gpu/src/sync.rs` holds the state machine with nine host tests,
      `t-drm-sync` pins 103 checks including a waiter woken by another task, and
      `IN_FENCE_FD` is now waited for rather than stored and ignored.
      `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE` are `1`; the one gap left
      is named in §7.4 (an out-fence that is already through) - and the two
      `TIMELINE` flags are no longer one of them: the flags turned out not to be
      the no-op they were written down as, and three rounds of measurement on the
      card ended with all of it built. A non-zero `point` needs the sync_file
      form; with `TIMELINE` the export point must be one the object has reached;
      and an *import* lands the destination on the fence's own point - snapshotted
      at export - or on the caller's with the flag, which *raises* where the first
      *sets* and may even move a point backwards. Both import shapes leave the
      destination a timeline, point 0 included. Pinned by `t-drm-sync`, whose new
      checks ask `TIMELINE_WAIT` as well as reading the point back, since reading
      0 alone cannot tell "never had a point" from "just been given point 0"
- [~] `DRM_VIRTGPU_*` 3D — the only path that makes `is_software()` false, and so
      the only one that lets niri, which rejects software EGL outright, run at
      all; virgl and venus are both built on it (§7.4). **The family is served
      now, 2D and 3D both**: all eleven requests are dispatched,
      `RESOURCE_CREATE` makes a real device resource across the new mailbox to
      the render task, the GL card negotiates `VIRGL` and the driver says so in
      its own words — `virtio-gpu: virgl=1 context_init=1 capsets=2 cached=2
      mask=0x6`, every number the deleted `user/staging/virtgpu-probe/` measured on Linux —
      and `t-virtgpu` pins the lot, its 3D branch included, on the GL machine
      (`[qemu] gpu` names the card). Both forks §7.4 argued are settled and
      landed — the GPU driver is this kernel's rather than the crate's, and a
      submission reaches the device through the render task's mailbox, which the
      resource path already uses. **What blocked `is_software()` was not the 3D
      queue but the card's identity, and that is built now**: measured against a
      working card's own sysfs reads, the node's `device` link had to lead to
      the display-class PCI function — carrying `vendor`, `device`,
      `subsystem_vendor`, `subsystem_device`, `uevent` and the `subsystem` link
      libdrm resolves first — rather than to a `virtual` directory with no
      attributes at all. §7.4's third update has the log and the pins. **The
      renderer itself is still unverified**: the GL-card run that would answer
      "eglinfo reports a non-software device" has not happened, because the
      console defect below killed every attempt at it. That is the first thing
      on this entry now, and it is a measurement rather than a build.
- [~] the property families a compositor finds by name. **Four are built, and
      the list said seven**: `EDID` (a real 128-byte EDID 1.4 base block, and
      the connector's `mm_width`/`mm_height` are its own centimetre fields times
      ten), `non-desktop` and `vrr_capable` (both immutable *ranges* of 0 and 1,
      measured - not the bools the sources suggest), `max bpc` (a range of
      exactly one value: the scanout is 8 bpc and nothing can raise it), and
      `IN_FORMATS` on the plane with `DRM_CAP_ADDFB2_MODIFIERS` answered 1 -
      which `ADDFB2` then honours, taking `DRM_MODE_FB_MODIFIERS` with the
      linear modifier and refusing any other. Three are deliberately **absent**,
      each for the reason `modetest -c -p` against a real card gives: there is
      no gamma LUT to hold `GAMMA_LUT`/`GAMMA_LUT_SIZE` and no VRR engine behind
      `VRR_ENABLED`, so a property whose write changes nothing would be the §4.2
      lie; and `FB_DAMAGE_CLIPS` and `OUT_FENCE_PTR` are absent on a real card
      too, because they are driver opt-ins. `panel orientation` and
      `subconnector` are absent for the same measured reason - the real eDP
      output carries neither. Pinned by `t-drm-props` (53 checks) (§7.4)
- [x] the two blobs those properties point at, as pure encoders in
      `crates/gpu/src/blob.rs` with 15 host tests - `drivers/drm.rs` is kernel-only, so
      the bytes a real reader parses had to live somewhere a host can check
      them. Verified against `edid-decode` rather than only against the module's
      own decoder, which caught a real bug (a descriptor's first two bytes)
      (§7.4)
- [x] the `MODE_CREATE_LEASE` family — a lease is a card descriptor restricted
      to a list of object ids, mintable only by the master, unable to widen
      itself, revocable by its lessor, and readable back through `GET_LEASE`
      (`t-drm-lease`). **The four request numbers were wrong and are now
      `drm.h`'s**: they were written down as `0xd3`..`0xd6`, thirteen too high,
      so every real caller reached the dispatch catch-all and heard `ENOTTY` -
      see §7.4 (`t-drm-lease` now pins them against the header's literals)
- [x] the legacy KMS queries a compositor falls back to — `MODE_GETFB2`, which
      reads a framebuffer back with its fourcc, its modifier and its plane
      arrays (`t-drm`), and the two pairs this card *refuses by name* rather
      than by falling off the dispatch `match`: the legacy gamma pair (`EINVAL`;
      no LUT behind it) and `CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` (`ENOTTY`;
      no vblank behind them). `GETCRTC` now writes `gamma_size` 0 instead of
      leaving the caller's `malloc`ed bytes there, and all four requests gained
      the argument check Linux's ioctl layer does before a handler can refuse
      them (`t-drm-args`) (§7.4)
- [x] `MODE_CURSOR`/`MODE_CURSOR2` and the cursor plane they drive — a real
      second plane with `type` = Cursor that `GETPLANERESOURCES`/`GETPLANE`
      report, so a compositor drives the pointer through the kernel instead of
      drawing it into its own buffer (`t-drm`), the pointer composited by the one
      flusher in a host-tested pure function (`crates/gpu/src/cursor.rs`, 10 host tests),
      and `CRTC_X`/`CRTC_Y` that steer rather than merely record (`t-drm-props`).
      `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` stays refused, which the measurement
      shows is what a real card does (§7.4)
- [x] `/proc/<pid>/maps` — the file a crash reporter reads to find a program's
      base address: every mapping in Linux's six-field format, with the name
      column where Linux puts it (`t-maps`). Two named gaps: an image's and a
      private file mapping's line carries no `offset`/`inode` (this kernel reads
      such a file in rather than paging it, so no object is left to name), and
      `smaps` is not written (§7.4, §Remaining)

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
- [x] 8.3 user stack canary (`AT_RANDOM`) (§8.3)
- [~] 8.2 seccomp — STRICT latches; FILTER installs, evaluates and gates a real
      call, and every one of its eight actions was measured against Linux before
      being written (four were wrong here: the kills are `SIGSYS` and not
      `SIGKILL`, an unrecognised action kills like `KILL_PROCESS`, `TRACE` with
      nobody attached fails the call with `ENOSYS`, and a caught `SIGSYS` leaves
      the caller its own `rax`). The availability probe answers the truth about
      which actions are implemented. `t-seccomp` pins each. Remaining, named:
      **`PTRACE_EVENT_SECCOMP`** — the tracer stop with the option set; the
      option is refused rather than half-served, so `TRACE` today means "allow"
      (§8.2)

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
      prerequisite. **§7.3 moved this one step on**: the test machine now has a
      writable disk of its own (`/dev/sda` on an AHCI controller, 4 MiB, beside
      the read-only FAT ESP the boot comes from), so the block layer can be
      measured in the guest rather than only against a mock. What is still
      missing is a *filesystem* on it - a partition table and an ext4 - because
      what §10.5 wants to count is ext4's reads, and a bare device has no
      readahead to measure
- [x] 10.6 Serial TX ring (drained by the TX-held interrupt; the mirror drops
      rather than stalls) · [ ] 10.7 Sub-tick sleeps (LAPIC one-shot)

**Architectures (Phase 9 — aarch64 stages 1a-1f, 2, 5a, 5b and 6a landed: the
machine boots a real ESP image under AAVMF, builds its own page tables from
`kernel_mappings`, switches between kernel tasks, reads the firmware's device
tree back against its own constants, reaches its devices through a window in its
*own* half — the 32 virtio-MMIO transports are found through the tree and read
back — and now runs code at EL0 with an address space of its own, traps into the
kernel and is answered (§9.1); next is the rest of stage 6, which is the ABI and
the `AddressSpaceOps` that completes `hal` — not stage 5's drivers, which the
same measurement puts behind `vfs` and so behind `hal` too)**
- [~] 9.1 aarch64 complete boot — **the machine boots a real Alpine `aarch64` userland to a login prompt**
      (§13.12): `busybox` init on `/etc/inittab`, `openrc`'s runlevels, `getty`'s `localhost login:` on the
      PL011, `/proc` and `/sys` mounted, dynamic linking working, and a `cargo xtask alpine-aarch64` verdict
      that reads it - **with its logger running**: `syslog` starts and every service after it logs *through*
      it (§13.14) - and **with no failing service at all**: `/dev/rtc0` exists so `hwclock` sets the system
      clock, and `/lib/modules/<release>` exists so the `modules` service's `modprobe` is answered rather
      than warned through (§13.15) - and **with its shape the same as the first machine's**: the virtual
      consoles (`tty1`..`tty4`) reach the wire through the console multiplexer, which is both machines'
      since §13.16, so a getty on `tty1` prints to the serial port a login reads. Every line of the boot's
      runlevel output is `[ ok ]`, and of §13.12's "named, not fixed" list only the two prose-level gaps
      remain (the install writes no `/etc/hostname`, so the prompt says `localhost`, and the machine's
      `/proc/net` rows are the same table the first machine's is) · [~] 9.2 RISC-V support
- [ ] **13 Machine-seam purity + aarch64 parity — 13.1/13.3–13.17 landed, 13.19–13.31 landed, 13.18 open, 13.2 in progress (§13); the boot-side half is Phase 15 (§15), where 15.19–15.24 put the boot's self-tests in a test build, move every check family into a suite beside the module it tests, and take the `M*` marker vocabulary out of the boot log (§15)**
- [x] **The boot is one flow — the two machine bodies are gone (§15.8).** One `start` and one
      sequence for every machine: the device step is `drivers::virtio::probe`, which asks the PCI
      front-end *and* the memory-mapped windows the record's device tree describes, registering what
      answers through each class's own contract, so the boot flow carries no architecture at all.
      The kernel mounts nothing but its bootstrap ramfs — `/dev`, `/proc`, `/sys`, the real root and
      the host share belong to the early userland (`assets/initramfs/core/init`), which is what a
      distribution's mount tree is. The machines' per-boot device checks became a test-build
      read-back (`src/board/suites.rs` plus the shared `devices` suite), so a distribution boot
      neither pays for them nor can be halted by one (§13.14's rule)
- [x] **The kernel travels inside the loader — the ESP carries one bootable file (§15.9).** The
      kernel is DEFLATE-compressed into the loader's own image at build time (`boot/build.rs`) and
      inflated at boot (`boot/src/lib.rs`), which is Linux's `bzImage` shape: the stub is still a
      stub and the kernel is still not a firmware application, but the load path no longer begins
      with a second file. `initramfs.tar` and `mpos.cmdline` stay files — they are data the kernel
      unpacks itself, not programs the loader has to place and jump to. This is the *visible* half
      of the boot refactor's step 5 (`TEST-AND-BOOT-REFACTOR.md` §5.8) taken without its costs: one
      allocator serving two firmware phases, the `x18` clash, and the loss of the `-kernel`
      bare-metal path all remain unpaid, because they follow from the kernel being *started* by the
      firmware, not from its being *carried* by the loader. Its first version carried the *wrong*
      kernel into a self-test boot on the second machine (the build script's freshness rule read a
      mtime cargo's linking does not move forward) and the two-machine gate sat silent for fourteen
      minutes on it; the fix is at the build, because the run-time guard that would have named the
      mismatch is refused by the boot's own gate (§15.19)
- [x] **A process hands the card back when it ends, not only when it closes (§15.12).** The DRM
      master, its console binding, the client-capability set, the authorisations, the sync objects
      and the framebuffers were released by the `close(2)` hook alone, and `do_exit` never runs it:
      `Report::finish` is `std::process::exit`, which is `exit_group`, which unwinds nothing, so a
      description that ended without closing its card left its DRM state behind. Because an `ofd` is
      the description's *address*, the next description the allocator placed there inherited a claim
      it never made. Found when five `drm*` programs first shared one boot - `t-drm` broke
      `t-drm-args`, `t-drm-lease`, then `t-drm-props` - and fixed by registering
      `drivers::drm::close_at_exit` with `sched::exit` beside locks, the socket and the inotify
      watch, so the exit path delivers the property `master_closed`'s header already promised
- [x] **`ADDFB2` reads its format modifier at the offset the uapi gives it (§15.13).** The field is
      at 72, not 68: `offsets[4]` ends at 68 and 68..72 is the padding the `__u64` forces, measured
      against the installed headers. `add_fb2` read at 68 and `get_fb2` wrote at 68, and the no-std
      test it was written against spelled its offsets by hand in the same wrong place, so the two
      agreed with each other and with nothing else. Invisible while the only modifier anyone named
      was `LINEAR`, which is 0, because the padding reads as 0 too. Pinned by naming a vendor
      modifier whose low 32 bits are zero (`0x0100_0000_0000_0000`), so the offset - not the value -
      decides the answer: with the kernel back at 68 the claim fails `got 0, want -22`

**Architecture debt (Phase 11 — audit done, Steps A/B/C/D/E/F/G landed, HAL S1 landed (§11.4), V-1/V-4/V-13 closed)**
- [x] 11.A subtract: the orphan/duplicate machine modules, the leftover `[msgtrace]` in the
      dispatcher, the aarch64 linker script that does not exist (§11.2 Step A)
- [x] 11.E leaf crates: the seven pure, downward-only leaves (`err`, `pci`, `date`, `binfmt`,
      `cred`, `uts`, `seccomp`) moved out of `src/` into `crates/*` as workspace members,
      re-exported from `src/lib.rs` under their old names so no caller changed (`pub use
      mpos_err as err;`). The point is enforcement, not tidiness: a leaf crate cannot name the
      kernel, so the downward-only rule (AGENTS §3 rule 5, and the V-* edges this table keeps
      re-auditing) is a compile error for those seven, not a convention. `xtask test`'s host
      stage now names each crate with `-p` so the moved pins keep running (635 in-kernel + 50 in
      crates = the 685 the suite ran before). `mpos-pci`'s test fixtures (`Device::fixture`,
      `test_lock`) reach the kernel's `/sys`/`/proc` tests through a `test-helpers` feature that
      only a dev-dependency enables, so the shipped binary carries no scaffolding. Held back at the
      time were `gpu` and `evdev`/`uinput`, which took `sched::TaskId` + `sched::wake` for their
      waiters: they needed an injectable wake abstraction before they could leave the crate. Both
      have since left — `gpu` two bullets down, `evdev`/`uinput` with the same treatment
- [x] 11.E (continued) three more leaf crates of the same kind, extracted once the seven above had
      shown the pattern: `mpos-term` (the terminal emulator — grid, cursor, blitter, keymap, font;
      a cell grid is not a filesystem fact, §11.1.4), `mpos-ahci` (the AHCI port decode, whose only
      reader is machine-only) and `mpos-gpu` (the virtio-gpu wire ABI, the DRM blobs, the cursor
      blend and the sync-object state machine). `crate::term`, `crate::ahci` and `crate::gpu`
      resolve through `src/lib.rs`'s re-exports, and the call sites that *did* move are the ones
      whose edge was the point: `console`, `drivers/video/render` and `drivers/virtio/input` now
      name the `term` crate rather than `vfs::term`, which is 3 references off `console → vfs` and
      5 off `drivers → vfs`. The pins that travelled with them run in their own crates (`xtask
      test`'s host stage names them), so the host total only grows by the test the next bullet
      added
- [x] **`mpos-gpu` is the one that paid for itself twice**: it sat inside the strongly connected
      component by a single edge — `gpu/sync.rs` woke a parked waiter by calling `sched::wake` —
      so the crate boundary forced that edge to become a parameter, `gpu::sync::Waker`, implemented
      by `drivers::drm` where the task table is and by a recording struct in the tests. The wake
      had been behind `#[cfg(target_os = "none")]`, which is why no host test had ever asserted
      it; there is one now (`submitting_a_fence_wakes_every_parked_waiter_and_nobody_else`), and
      `t-drm-sync` keeps pinning the same claim on the machine end to end. This is the shape
      `evdev`/`uinput` took in the next bullet, and the reason it is written down here rather than
      only in the code: the abstraction is small, and what it buys is the difference between a
      rule a reviewer enforces and one the compiler enforces
- [x] **the input policy pair followed the same seam**: `mpos-evdev` (the record window and its
      clients) and `mpos-uinput` (the request vocabulary a program declares a uinput device with,
      which depends on the evdev crate for one constant — the size of a `struct input_event`).
      `src/evdev.rs`'s one kernel dependency was `sched::policy::TaskId` in its waiter list, and it
      never called `sched::wake` itself — the driver did — so the fix is `mpos-gpu`'s in a smaller
      size: `WaiterId` is an opaque number there, and `drivers/virtio/input.rs` converts at the
      four call sites it owns. `evdev` leaves the module graph entirely as a consequence: its
      former `→ sched` and `→ vfs` edges are gone from `cargo xtask depgraph`'s table, and what
      remains is the two crates being *named* by `drivers` and `syscall`, which is a cross-crate
      edge downward. Twelve leaf crates now, all but the input pair host-compiled and every one of
      them unable to name the kernel
- [x] **the block layer seam is the thirteenth**: `mpos-blk` — the `BlockDev` contract, the disk
      registry and the GPT/MBR decode — moved out of `src/blk` into `crates/blk`, re-exported as
      `pub use mpos_blk as blk;` so every `crate::blk::` caller is unchanged. It was already
      host-compiled and host-tested (13 tests), and the module's one kernel reference — `part.rs`
      sizing its transfer buffer from `mm::PAGE_SIZE` — is now `mpos_blk::PAGE_SIZE`, the bound
      belonging to the trait that states it rather than the frame allocator that sizes the same
      hardware fact. `blk` leaves the module graph as a source: `drivers`/`uevent`/`vfs` still
      *name* it (a cross-crate edge downward), it names nothing back, and no mutual pair moved
      (17/17)
- [x] 11.B the device seam. **B3 landed long ago** (`buffer_ok`/`err_of` moved to the
      `uaccess` leaf). **B2 is complete**: every device family this audit named states its
      own ioctl contract next to the state it answers from - `'d'` (DRM), `'E'` (`EvdevNode::ioctl`),
      `'K'` (`Tty::kd_ioctl`), `'U'` (`Uinput::ioctl`, with the family check moved into the
      `mpos-uinput` crate as `nr_of`) and `'V'` (`Tty::vt_ioctl`, with the
      caller's task/root/thread-group handed down as a `VtCaller` the way `kd_ioctl` takes
      `owns_card`). `syscall/linux/fs.rs` lost 212 lines to the last two; the only `vcmux`
      reference left in it is the tty family's `abandon`. **B1 is done**: the nine
      `Inode::as_*` downcasts are gone, replaced by one type-erased handle the *caller*
      names through - `pub trait Inode: Send + Sync + core::any::Any` plus
      `impl dyn Inode { fn as_any(&self) -> &dyn core::any::Any }`, a plain trait upcast -
      so `src/vfs/mod.rs` names `crate::drivers::`/`crate::net::` **zero** times (the
      ratchet constant follows, 9 -> 0). Removed one device per commit in the plan's order
      (fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the DRM card's three node
      types last); the shape decision, and why the plan's `DevOps` alternative was not the
      one taken, are in §11.2 Step B1. It does **not** remove the `drivers <-> vfs` pair -
      the remaining `vfs -> drivers|net` references are `vcmux`, `procfs`, `sysfs` and `p9`,
      which are V-4's half and the PCI-config/9p doors (§11.1.1)
- [x] 11.C per-open-file resources get a registry (`sched::exit`), so `sched` stops naming
      `net`/`vfs`/`drivers` on task exit: `sched → net` is gone (which removed the `net ↔ sched`
      mutual pair, 20 → 19), and the two `crate::drivers::` references left in `runtime.rs` are
      the *execve* `FD_CLOEXEC` pass, a different event, named rather than abstracted (§11.2 Step C)
- [x] 11.D the terminal emulator and the console mux move out of `vfs` (§11). **CLOSED at its
      acceptance criterion — the `vfs ↔ console` dependency pair is broken.** The emulator is the
      `mpos-term` leaf crate, and (D.1/D.1b/D.2a) its vocabulary, its pure `vtmode` handover table, and
      the active-console number all live in `crate::term` rather than `vfs` — so `console` reads "which
      console is on screen" from the leaf and the `console → vfs` edge fell to zero, collapsing the
      mutual pair: **mutual pairs 18 → 17** (the first ratchet drop since Step C; gate green
      `pass=140 fail=0`). D.2b (relocating the `vcmux` file itself into `console`) was measured and
      **rejected**: `console` is a leaf the mux's `vfs`/`drivers`/`sched` naming would violate, and a
      mux-only inversion moves no ratchet. The `vcmux` file stays in `vfs`, where its pty/tty
      orchestration coherently belongs; the surviving `drivers ↔ vfs` pair is its own finding. Recorded
      in `ARCHITECTURE.md` §9
- [x] 11.F the `target_os = "none"` gate gets a written criterion (`src/lib.rs`, `AGENTS.md` §3),
      and the unused host test module behind it is settled: `net/socket.rs`'s three tests were
      never compiled, and their claims are pinned by `t-sockmsg`, which boots, so the module is
      gone. `xtask/src/gates.rs` fails the gate on the shape from now on (§11.2 Step F)
- [x] the **`mm`/`vfs` seam** (V-13, the one finding the audit left outside A–G): `mm` named
      `vfs::Inode` for the object a frame-backed mapping aliases its pages from, which is what made
      `mm` and `vfs` a cycle. The trait it needed is `mm::vma::MappedObject` — the frame behind a
      page, the `(dev, ino)` pair `/proc/<pid>/maps` prints, and whether two mappings are of one
      object — and the VFS answers it for its inodes through `vfs::MappedInode`. `mm → vfs` is now
      0, and the pair went with it: **19 → 18**. The comparison that used to be `Arc::ptr_eq` is
      asked of the object instead (identity), which a trait object can do and a pointer cannot
      without a downcast; for the four frame-backed node types the pair *is* unique per object, so
      it is the same answer as a value, and the rule is pinned by a test that two handles to one
      object merge while two objects do not
- [x] 11.G a depgraph ratchet, so the measured cycle count cannot grow back: `cargo xtask
      depgraph` (also a host test, so it is inside `cargo xtask test`) re-runs §11.0's extraction
      with comments stripped and holds three recorded baselines — lower layer → `syscall`,
      `      vfs/mod.rs` → devices, mutual pairs — under `xtask/src/depgraph.rs` (§11.2 Step G)
- [x] **11.4 the HAL, stage 1**: `src/hal/` declares the machine capability traits (`Clock`,
      `WallClock`, `Cpu`) and the platform tag `Current`; `arch/x86_64` implements them for it,
      and every `arch::timer`/`rtc`/`idle`/`halt`/`enable_user_sse` call above the machine layer
      is now a `crate::hal` one. The tag is `hal`'s own type, so the seam adds **no** cycle
      (`arch → hal` only) and the measured **mutual pairs fell 17 → 15** — `arch ↔ console` and
      `arch ↔ net` collapsed to one-way edges, the second ratchet drop since Step C. Stages 2–5
      (interrupts, paging, contexts, the closed door) are in §11.4
- [x] **11.4 stage 2**: the interrupt controller is a capability. `crate::hal::Interrupts` names
      `Line::{Timer,Serial}` and asks `unmask`/`mask`/`eoi`, `init_message_signalled` and
      `net_msi_vector` — so the serial driver names the line it is on rather than PIC bit 4, and
      the net driver asks for the vector instead of naming `idt::NET_IRQ_VECTOR`. No raw IRQ or
      vector crosses the seam. `drivers → arch` fell to zero and **mutual pairs 15 → 14**; the
      full gate was green on this stage: `pass=142 fail=0 (142 built)`, exit 33
- [x] **11.4 stage 3**: paging is a capability. `crate::hal::Paging` carries `install`,
      `map_mmio`, `translate`, `user_accessible`, `is_cow_page`, `write_user_u32`, the roots and
      the two `switch_to_*`, plus `type Space: AddressSpaceOps` — the address-space method set
      (`map`/`try_map`/`unmap`/`protect`/`switch`/`map_cow`/`map_shared`/`resolve_cow`/
      `walk_user_pages`/`set_cow_bit*`/`flush_if_active`/`phys_for_virt`), which `hal::AddressSpace`
      aliases to. `PageTaken` moved to `mm::vm` beside `Mapping`, since it is part of what a
      mapping *is* rather than an x86 fact. `mm`, `uaccess`, `sched`, `syscall` and `main` stopped
      naming `arch::paging` entirely. The pair count **stayed at 14 while swapping which pair it
      is** — `arch ↔ mm` out (`mm` no longer names `arch`), `hal ↔ mm` in, argued in
      `ARCHITECTURE.md` §9 as the seam's neighbour rather than a leak. Gate green:
      `pass=142 fail=0 (142 built)`, exit 33
- [x] **11.4 stage 5's ratchet** (the closure waits for S4): `cargo xtask depgraph` gained a third
      recorded rule — references to `crate::arch::` from a layer that is not the machine layer
      (`sched` 17, `syscall` 4 today, both may only fall). It is what stops the door widening
      while S4 is written; measured by the same comment-stripped extraction as the other two, so
      the `drivers/rtc_dev.rs` doc link does not count. The design finding that reshaped S4 — the
      `abi::uregs` conversion cannot move into `arch` without an `arch → syscall` edge, so the
      frame must expose operations instead — is recorded in §11.4 beside the stage table
- [x] **11.4 stage 4**: the task context is a capability, and the door shut behind it.
      `crate::hal::Contexts` carries `type Context: ContextOps<Fpu = Self::Fpu>`, `type Fpu`,
      `type UserFrame`, the FPU save/restore/claim/trap calls, `bootstrap`/`switch`/
      `resume_user`/`enter_user`/`set_kernel_stack`, and `user_segments`. `sched` no longer names
      `arch` at all: `task.context.fpu` is `fpu()`/`fpu_mut()`, `FpuState::reset`/`from_bytes` are
      `hal::Fpu`, the two frame literals in `spawn_copy`/`spawn_thread` became
      `UserFrameOps::forked` plus `set_result(0)`, and `context_regs`/`context_from_regs` are now
      one-line calls onto `to_user_regs`/`from_user_regs` — the machine translating its own frame
      into the ABI's field order, which is the half of the convergence that *can* live in `arch`.
      `syscall/linux/proc.rs`'s `gdt::user_selectors` became `hal::user_segments()`. So
      **`sched → arch` and `syscall → arch` are both 0**, `arch ↔ sched` and `arch ↔ syscall`
      collapsed, and **mutual pairs went 14 → 12**; the depgraph's third rule became an allow-list
      (`arch`/`hal`/`main`/`lib`) rather than a ceiling, so a *new* module reaching for the machine
      layer fails at once. The duplication it left — the Linux register-file field order exists
      once in `arch/x86_64/context.rs` and once in `syscall/linux/abi.rs`, because one side is
      bare-metal and the other host-built — is *checked* on every boot in `M1` (`main.rs` asserts
      all nineteen named indices and the count against the ABI's table), which is what makes it a
      recorded agreement rather than a hope. Gate green: `pass=142 fail=0 (142 built)`, exit 33
- [x] **11.4 stage 5, the door itself**: `src/arch/mod.rs` is now a facade of four items —
      `kernel_sections`, `breakpoints_caught` and the two register-table constants the M1 check
      needs — and `arch/x86_64`'s submodules are `pub(crate)`, so a layer cannot reach
      `arch::paging` even by writing the path. Defence in depth behind the depgraph's allow-list,
      which catches the reference; this catches the *name*. Deleting the module glob made two
      genuinely dead helpers visible (`gdt::user_code`/`user_data`, reachable only through it) and
      they are gone, per V-7's rule that dead machine modules are debt. Gate green:
      `pass=142 fail=0 (142 built)`, exit 33

**ABI truthfulness (Phase 12 — Group A landing, gate now runs to a summary)**

Calls that are *accepted* and answer success but do less than Linux — the §4 "silent success"
class, absent from the board until now. Ordered §12.4. The green gate runs to a summary; the
latest measured run (after this session's honest-refusal work — `prctl` name + catch-all `EINVAL`,
`vhangup`'s `ENOTTY`, `times`/`getrusage` self+child CPU accounting, `rseq`→`ENOSYS`, the whole
evdev `'E'` family incl. the source-verified `EVIOCREVOKE` NULL/one-way correction, `kd`
`KDGKBTYPE`, `RND` entropy capability-gate + `EOPNOTSUPP`, `VT_SENDSIG`→`ENOTTY`, the `eventfd2`
flag audit, `madvise`'s, `posix_fadvise`'s, the `clock_gettime`/`clock_nanosleep` clock-id and `memfd_create`'s flag vocabulary gates (unrecognised → `EINVAL`, pipe → `ESPIPE`, `MFD_HUGETLB` → `EOPNOTSUPP`), `unlinkat`'s unknown-flag `EINVAL`, `PTRACE_SETSIGINFO`'s `ESRCH`/`EFAULT`/`EOPNOTSUPP` (A7), `kd_ioctl`'s `_ => 0` → closed-set `EOPNOTSUPP`/`ENOTTY` (the last ioctl fake-success removed; per-device serve stays B2), `sched_setattr`/`sched_getattr` reaching the same per-task `nice` B5 made real, and `sched_setscheduler`'s `SCHED_OTHER` priority validation) reports
`=== SUMMARY pass=134 fail=2 (136 built)` with QEMU exit `33`; each new pin (`t-prctl`, `t-posix`,
`t-times`, `t-rseq`, `t-ioctl`, `t-evdev`, `t-seat`, `t-eventfd`, `t-priority`, `t-madvise`, `t-fadvise`, `t-clock-gettime`, `t-unlinkat`, `t-memfd-shm`, `t-ptrace-attach`, `t-statx`, `t-openat`, `t-truncate`, `t-fallocate`) prints `=== PASS`. The two reds are the
in-flight `t-dhcp`/`t-dsp` — network and audio, on paths no Phase-12 change touches. **That pair is
now closed**: the run that closed it reached `=== SUMMARY pass=136 fail=0 (136 built)` with QEMU
exit `33` — see §12.4a for the sound card's wrong PCI ID and the user-address DMA panic behind
`t-dsp`, and for the DHCP test's own malformed DISCOVER behind `t-dhcp`.
(`t-netlat`/`t-stdresolve` flake in intermittently in their place under load — confirmed
network-timing reds in the concurrent net work; a confirming run showed `pass=133 fail=2`.)

**The sweep's *set* is now a configuration, and its default is smaller on purpose.** Every test
is still built, but `tests.toml` at the repository root names the programs the default sweep
leaves out, and the harness stages only what is selected: `external` (`t-dns`,
`t-rawicmp`, `t-stdresolve`, `t-udp`, `t-tcp-close` — all five need something outside the
machine),
`slow` (`t-fill-ramfs`, `t-forkcost`, `t-netlat`, `t-pipecost`) and `diagnostic` (`t-sh`).
`cargo xtask test --groups <a,b>` runs an explicit list, `--all-groups` runs everything, and a
group name the file does not define — or a name in the file that no built program answers to —
is an error rather than a smaller sweep. `t-rawicmp` joined `external` when it was measured
doing what that group is for: it pings a public address and compares the answer against what
the host's own `ping` gets, so the internet is one of its two halves — it blocked three of four
sweep runs while it was listed as a machine check, and the same kernel answered 3/3 from the
run machine minutes earlier (§2.5's run). The measured default is `=== SUMMARY pass=134 fail=0
(134 built)` with `10 of 144 user tests left out by tests.toml`, QEMU exit `33`.

Two things that follows from it, both worth stating because they change what a red means:

* **The internet is no longer in the default gate.** `t-dns` and `t-stdresolve` were the two
* **`t-times`'s racy assertion is fixed, and the fix is the assertion.** One gate run of this

- [x] 12.0 restore the green baseline — `t-tty-vtime`'s `VMIN>0`/`VTIME` read hung because
      `block_until_deadline` swallowed the clock-restart wake; fixed with an `input_count` the
      partial-byte park wakes on (`tty.rs`, §12.4a). `t-tty-vtime`/`t-tty-eintr` now `=== PASS`.
- [~] 12.A honest-refusal batch — `signalfd` `SFD_CLOEXEC` + unknown-flag `EINVAL` and `mmap`
      flag validation (`MAP_FIXED_NOREPLACE`→`EEXIST`, `MAP_FIXED|NOREPLACE`→`EINVAL`,
      `MAP_HUGETLB`→`ENOMEM`) **landed and gated** (`t-signalfd`, `t-mmap` carry the counterfactual
      pins). `rseq`→`ENOSYS` **landed** (reversing the earlier revert): the dispatch
      arm is removed so `rseq` falls through the canonical `_ => ENOSYS`. The old arm
      returned `0` for a 32-byte registration while writing none of the `cpu_id`/
      `node_id`/`mm_cid` fields the ABI puts in the caller's block — a §4.2 success-for-
      a-thing-not-done. Measured on a current host (Linux 7.2, `AT_RSEQ_FEATURE_SIZE`
      33, `AT_RSEQ_ALIGN` 64): glibc registers at length *33*, which the old `len != 32`
      arm `EINVAL`'d anyway, so it honoured neither real glibc nor the contract; and the
      libc this machine actually runs (musl) never calls `rseq`. `ENOSYS` is the truthful
      "not available" a probing libc falls back from; doing `rseq` for real is the named
      gap below. `t-rseq` re-pinned to assert `ENOSYS` (rule 1: the pin had encoded the
      false-success, so the test changed with the measurement, not the code). `prctl`
      `PR_SET_NAME`/`PR_GET_NAME` **landed and gated**: the name now round-trips through the group's
      `comm` slot (stored on `Mm`, the same slot `comm_of` and `/proc`'s `comm` read), Linux-faithful
      on the truncation and faults — a null buffer is `EFAULT` both ways, a name with no NUL inside
      the sixteen-byte `TASK_COMM_LEN` slot is refused (`strnlen_user`→`EFAULT`), and no set name
      falls back to the derived image basename — pinned by host `prctl_stores_and_reports_the_process_name`
      and `t-prctl` (the old pin asserted the "answers sixteen zeroes" divergence; corrected to the
      round-trip, the counterfactual). The `prctl` catch-all `_ => 0` → `EINVAL` **landed and gated**:
      every option not backed by real state is now refused `EINVAL`, matching Linux's `prctl` `default:`
      arm (kernel/sys.c) — the old blanket `0` was a §4.2 false success. `t-prctl` corrected from
      "unknown answers 0" to "unknown → `EINVAL`" (the gate, which boots the whole initramfs, is the
      evidence no running program depended on the `0`). Backing individual options
      (`PR_SET_DUMPABLE`/`PR_CAPBSET_*`/…) with real state remains optional future work; the objective's
      "unknown → `EINVAL`" itself is met. evdev `EVIOCREVOKE` **landed and gated**: per-ofd revoke lives
      device-side (`crates/evdev`), `read`/every-other-`ioctl` answer `ENODEV` once revoked; **corrected to Linux's actual contract** (authoritative `evdev.c`): the arg must be NULL (non-NULL → `EINVAL`) and the revoke is one-way (no restore ioctl) - the prior value-arg/`0`=restore model was a wrong-shape divergence.
      `t-evdev` + host test carry the counterfactual. **evdev `'E'` family fully honest
      this pass, measured on real Linux** (`/dev/input/event*`): `EVIOCSFF`/`EVIOCRMFF`→`ENOSYS`
      (Linux gates on `EV_FF`, no node here has it), `EVIOCS/GKEYCODE`(+`_V2`)→`EINVAL` (no per-device
      keycodes array), `EVIOCSREP` stores + `EVIOCGREP` reports the device's `(250,33)` gated on
      `EV_REP` (`EvdevNode` gained `rep_*`/`supports_type`), `EVIOCGKEY/LED/SND/SW` write `min(len,bitmap)`
      zeros **and return the count** (KEY=96, LED/SND/SW=8 — the global bitmap), `EVIOCGMTSLOTS`→`EINVAL`;
      all replaced `0`/`give(&[])` false-successes, pinned counterfactually in `t-evdev`. `kd_ioctl`
      `KDGKBTYPE`→`KB_101` (the one context-free-constant read). **The remaining `K` reads/setters no
      longer fake a success**: the old `_ => 0` catch-all (which returned `0` for `KDGETLED`/`KDGKBMETA`/
      keymap/font tables *with the caller's buffer left as stack garbage*, and for the setters claimed an
      effect on absent hardware) is replaced by a closed set - a number in the `K` command block is a real
      console ioctl this device declines (`EOPNOTSUPP`), one outside it is not a console ioctl (`ENOTTY`) -
      pinned counterfactually in `t-ioctl` (KDGETLED/KDSETLED→`EOPNOTSUPP`, out-of-block→`ENOTTY`); the
      full gate stays green so the seat-manager "refusal breaks the console" premise was, like A3's, wrong.
      *Serving* them per-device from live console state (rather than refusing) is the structural Phase 11
      B2 (V-2) move, with the measured Linux values recorded in §12.1 A2 so B2 implements rather than
      guesses. **Also this pass (selector/flag-word closure): `madvise`, `posix_fadvise`, `clock_gettime`/
      `clock_nanosleep`, `memfd_create`, `getrandom` and `unlinkat` now refuse an unknown advice/clock-id/
      flag word with the correct errno (measured), `posix_fadvise` a pipe with `ESPIPE`, `memfd_create`
      `MFD_HUGETLB` with `EOPNOTSUPP` (no hugepage backing); `sched_setattr`/`sched_getattr` apply+report
      the real per-task `nice` and `sched_setscheduler` refuses a nonzero `SCHED_OTHER` priority (B5b/B5c);
      `PTRACE_SETSIGINFO` validates attach/buffer then refuses the carry `EOPNOTSUPP` (A7). None fakes a
      success any more.**
      **A8 RND entropy ioctls landed + gated (measured on `/dev/urandom`):** the mutating requests are
      gated on `CAP_SYS_ADMIN` (unprivileged → `EPERM`, before the buffer copy, matching Linux); a
      privileged credit stays an honest `0` no-op on the always-full ChaCha stream, and a privileged
      `RNDZAPENTCNT`/`RNDCLEARPOOL` is refused `EOPNOTSUPP` rather than the old `0` that `RNDGETENTCNT`
      contradicted (`t-ioctl`: root ADD→0, root ZAP→`EOPNOTSUPP`, `setresuid`-dropped→`EPERM`)
      **A8 corrected (the boot's `seedrng: Unable to seed: Not supported`):** that pass left the family
      dispatched on `request & 0xff`, and the `'R'` command numbers it assigned were wrong by one — the
      arm written as `Some(3) | Some(4)  // RNDCLEARPOOL / RNDZAPENTCNT` is `RNDADDENTROPY` (nr 3) and
      `RNDZAPENTCNT` (nr 4), while `RNDCLEARPOOL` is nr 6 and nr 2 is the removed `RNDGETPOOL`. So the
      credit OpenRC's seedrng sends on every boot — `RNDADDENTROPY` = `_IOW('R', 0x03, int [2])` =
      `0x40085203`, verified as the binary's *only* ioctl immediate with `objdump` on
      `/usr/libexec/rc/sbin/seedrng` in this image's own rootfs — fell into the drain arm and got
      `EOPNOTSUPP` back. `t-ioctl`'s own `RNDADDENTROPY` constant was `0x40085202`, i.e. `RNDGETPOOL`,
      which pinned the wrong word and so passed for the wrong reason: a test that cannot tell a credit
      from a removed request agreed with a handler that could not either (§4.6, and the reason the pin
      is now a table of whole command words). Measured on real Linux 7.2.7, root and unprivileged, on
      `/dev/random` and `/dev/urandom`: the family switches on the **whole word** (`_IOW('R',0x03,int)`
      and `_IOWR('R',0x03,int[2])` are `EINVAL`), an unrecognised `'R'` number is `EINVAL` *before* the
      capability is asked (unprivileged `RNDGETPOOL` → `EINVAL`, `RNDZAPENTCNT` → `EPERM`), a credit's
      negative entropy count is `EINVAL` even beside an unusable length (`{-1,-1}`, `{-1,65536}`), a
      negative or unmapped length is `EFAULT` (`{8,-1}`, `{8,16MiB}`, `arg=NULL`), a zero length or zero
      count is `0`, a 1 MiB credit whose bytes really are there is `0`, and none of it is gated on the
      descriptor's mode (`RNDADDENTROPY` succeeds on an `O_RDONLY` fd). A `struct rand_pool_info`'s seed
      is *checked*, never copied — this pool has no accumulator to receive it — so the worst case a
      caller can steer is one page walk plus `buffer_ok`'s `MAX_IO` bound (the worst-case rule). Also
      measured: `RNDADDTOENTCNT`/`RNDZAPENTCNT`/`RNDCLEARPOOL`/`RNDRESEEDCRNG` are `EPERM` unprivileged
      and `0` for root, and an `'R'` word on `/dev/rtc0`, `/dev/tty1` or a regular file is `ENOTTY`,
      which is why the device gate stays in front of the command. **Both gaps this left open are now
      closed** (`UAPI.md` §5.1): (i) `RNDRESEEDCRNG` re-keys the generator for real and answers the `0`
      Linux answers root. `rand::reseed()` re-rolls the ChaCha key from a fresh `seed()` mixed with a
      draw from the current stream, so the state after the call is one no earlier output predicts and
      the re-key is never a fixed point even where `seed()` is (the host test build - which is what
      makes the key change pinnable there); an unprivileged caller still gets `EPERM`, asked first,
      as measured. `RNDZAPENTCNT`/`RNDCLEARPOOL` deliberately stay `EOPNOTSUPP` for root: there is
      still no drainable count, and a `0` would be the fake that `RNDGETENTCNT` contradicts one moment
      later. (ii) `FIONREAD` on the two entropy devices answers the `EINVAL` Linux answers - measured
      7.2.7, both devices, both users, `O_RDONLY` and `O_WRONLY`, and *before* the argument is read (a
      NULL or unmapped pointer is still `EINVAL`, not `EFAULT`) - not the 512. The rationale comment
      that justified the 512 was false for the reason this paragraph already gave: `seedrng` takes its
      seed length from `/proc/sys/kernel/random/poolsize` and sends one ioctl, the credit above.
      `handle_ioctl`'s `0x541B` arm and `t-ioctl`'s 512 pin moved together, as planned, and the pin
      now asserts `EINVAL` with a NULL-argument line beside it (the divergence is named in `UAPI.md`
      §5.2 for the two descriptors that *would* answer a count on Linux - a regular file and a pipe -
      which keep the honest `ENOTTY` here for now).
      **Gated:** host `620 passed`, guest `=== SUMMARY pass=132 fail=0 (132 built)` with QEMU exit 33,
      and 31 `'R'`-family lines in `t-ioctl`. Two of them are the counterfactuals that matter:
      `RNDADDENTROPY {64, 0} is accepted` and `RNDADDENTROPY with the seed bytes it declares is
      accepted` both answered `EOPNOTSUPP` before this change, and
      `RNDGETPOOL, which the kernel stopped answering` answered `0` — the old dispatch's `Some(2)` arm
      took a removed request as a credit and faked the success, which is the §4.2 shape this phase is
      about, arrived at from the other direction.
      **Re-gated after (i) and (ii):** host `635 passed`; guest `=== SUMMARY pass=134 fail=0
      (134 built)`, QEMU exit 33, `t-ioctl === PASS`.
      **A9 ioctl buffer-direction audit (this pass):** the whole `handle_ioctl` surface was swept for the
      one class Phase 12 is about - a request that answers success having done less than Linux. Every
      `_IOR`-style getter that fills the caller's buffer was checked against the `buffer_ok(…, write, …)`
      direction it validates: `TIOCGWINSZ`/`TIOCGPGRP`/`TIOCGSID`/`TIOCGPTN`/`TCGETS`, the `VT_OPENQRY`/
      `VT_GETMODE`, `RNDGETENTCNT`, `RTC_RD_TIME` and every evdev `give`/`give_state` correctly require a
      **writable** destination (Linux's `put_user` faults a read-only one with `EFAULT`), and each `_IOW`
      setter correctly requires only a readable one. The sole outlier is the block-device size trio
      (`BLKGETSIZE64`/`BLKGETSIZE`/`BLKSSZGET`): they write the size into `buf` but validated it `write=false`,
      so a read-only destination was accepted and filled where Linux answers `EFAULT`. **Fixed in the
      Phase-12 leftovers pass** (`UAPI.md` §5.1): the three validations read `true` now, so a `PROT_READ`
      destination faults as Linux's `put_user` does - measured on a real device, all three are `EFAULT`
      against a `PROT_READ` mapping. It remains **unpinned in the gate** for exactly the reason it was
      recorded rather than fixed the first time: the host `user_accessible` stub answers accessible for both
      directions, and the selftest boots **without a disk**, so no block node exists to open and reach the
      path. When a block node enters the test image the pin is a `t-ioctl` `PROT_READ`-buffer `EFAULT` line.
- [x] 12.B contract-completion batch, gated on the full run. **Landed and gated:** IPv4
      `MSG_PEEK`/`MSG_DONTWAIT` made consistent with the other families (`t-sock-flags`); access
      mode enforced after `open` - `File::read`/`write` refuse an `O_WRONLY` read / `O_RDONLY`
      write with `EBADF`, default permissive so sockets/pipes/anon are unaffected (host test +
      `t-fill-ramfs`, whose wrong-mode read-back was a Linux-wrong expectation, corrected to
      `O_RDWR`); the two write-shaped calls that are not `write(2)` — `ftruncate` (`EINVAL`) and
      `fallocate` (`EBADF`) — ask the *same* predicate through `File::may_write` rather than each
      re-spelling `access == ACC_RDONLY`, so a fourth write-shaped door cannot be enforced in one
      place and missed in the other; `get_robust_list` round-trips the `set`-registered head (host test); and
      `set/getpriority` round-trip a stored per-task nice across `PRIO_PROCESS`/`PGRP`/`USER`
      (`t-priority`), with the §1.3 board row corrected back to `Done`. `vhangup` answers `ENOTTY`
      with no controlling terminal (Linux's answer, gated by the corrected `t-posix`) and `EOPNOTSUPP`
      with one — it refuses rather than fakes the hang-up the tty layer does not perform (§12.2 B4). `times`/`getrusage`
      now report the running process's real CPU — user/system split by the interrupt ring, charged per
      tick to the group `Mm`, with a reaped child's CPU folded into `tms_cutime`/`cstime` and
      `RUSAGE_CHILDREN` at the reap (gated by `t-times`, including a fork→burn→wait child-aggregation
      pin). Every B-batch item is now either Linux-aligned or honestly refused, each pinned (§12.2)
- [x] 12.C stale rationale comments corrected: `sigaction` "no handler is ever called" (now runs
      the handler), `unlinkat` "`AT_REMOVEDIR` ignored" (now selects rmdir vs unlink), and the
      `getpriority`/`setpriority`/`vhangup` "Done" board lump split into a truthful `Partial` row
      (§12.3)

---

### Suggested next moves (highest impact, tractable)

Landed since this list was written: `flock` is owned by the open file
description while record locks stay per-process (`t-filelock`), `EVIOCGRAB` is
per-descriptor with Linux's `EBUSY` and dies on close (`t-evdev`), ext4 updates
`atime` under `relatime`, two board entries that were already true are marked
so (AF_UNIX `SOCK_DGRAM` transfer, the serial TX ring), and evdev's read rule is
finished: a reader that is not the grabber is told nothing while the grab is
held, and two readers of one node each keep their own copy (§7.2, `t-evdev`).

**A new item the `AF_PACKET` landing opened (§4.5)**: wire a DHCP client into
the boot — `udhcpc -s` under OpenRC *and* an address path for the selftest
initramfs — so the kernel's static default can retire from being the answer to
being the fallback. `t-dhcp` already proves the mechanism the client will ride.

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
list's first two items are done**: §3.3's listener landed on top of the loopback
that had to come first, so a program inside the machine can now serve one - both
halves measured against Linux and pinned by `t-tcp-listen`, and the section
rewritten around them (§3.3.1-§3.3.2) - and anonymous demand paging, with the two
things it unblocked, is §5.1/§5.2's rewrite (`t-mmap-demand`,
`t-mmap-shared-fork`).

1. **Anonymous demand paging** (§5.1 + §5.2) - the prerequisite both items named,
   and the only one that changed the mm fault path for everything. Landed: a
   `mmap` is an address space, a page arrives on the fault that needs it, and
   `MADV_DONTNEED` gives it back again (`t-mmap-demand`).
2. **namespaces** (§6.3) - **step 1 is built: UTS.** A task holds an `Arc` to the
   two names `uname` reports; `fork`/`clone` hand it on by reference and
   `unshare(CLONE_NEWUTS)` hands the caller a copy, so a `sethostname` inside is
   invisible outside - held by `t-ns-uts` on both sides, and by four host tests
   over the flag verdict. The section's own record now carries what the step
   cost, the two measurements behind the capability gate, and four named gaps
   (`clone(CLONE_NEWUTS)`; the six thread-group flags for a caller that has
   siblings; `setns`, which still cannot name a namespace; the sysctl files,
   which procfs does not mode-police). **MNT is next** and is the one containers
   actually need; its prerequisite — a per-process root, worth fixing on its own
   since `chroot` here used to move the whole machine's — **has landed**: a task
   carries its own root and the path walk jails to it (`t-chroot`). What is left
   of MNT is moving the mount *table* onto the task and `unshare(CLONE_NEWNS)`.
   `seccomp` FILTER's plumbing - the per-task list, the two install paths and
   the gate - has landed, was then measured action by action against Linux
   (four of its answers were wrong), and is pinned by `t-seccomp`. What is left
   of §8.2 is one named piece, the `PTRACE_EVENT_SECCOMP` stop, and nothing
   asks for it: `PTRACE_SETOPTIONS` refuses the option, so no tracer on this
   machine can be waiting for a stop it will not get (§8.2).
3. **§10.5's disk-backed instrument, then the frame pool** - the two remaining
   members of the performance family with a plan and no number. Both are
   measurable today, and each wants a before/after figure with the disk in
   place - and the number is allowed to say "no".
4. ~~**`SO_REUSEADDR` at `bind`/`listen`** (§3.3.3)~~ **Landed.** It was the one
   item here expected to be small, and the work was: the answer was not, because
   measuring it first showed the option does not mean what it is usually
   written up as. A holder that is *listening* never yields its name, and every
   other holder yields it only to a caller that set the option **and** whose
   holder had set it too - so a restart survives its own `TIME-WAIT`
   connections because Linux copies the option onto them at `accept`, not
   because the new socket asked. All thirteen answers are pinned by
   `t-tcp-listen`, which still runs unchanged on a Linux host. The other two
   options left in that section need things this kernel cannot do yet (an
   unsolicited reset on egress; smoltcp buffers sized after the set is built),
   and each says so where it is defined.

---

## Current Status

- **~229 Linux x86_64 syscalls** implemented
- **Uniprocessor** — single core, `critical_section::Mutex<RefCell<T>>`
- **Signals** — handlers delivered via `rt_sigframe`, mask enforcement, `sigqueue` siginfo payloads, and `SA_RESTART` acted on (an interrupted call is rewound
  through the handler's own frame); every blocking wait a catchable signal can
  end answers `EINTR` — terminals (`O_NONBLOCK` included), pipes, eventfd,
  signalfd, inotify, unix sockets, AF_PACKET, netlink and `nanosleep` with its
  remainder (§2.5)
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
- **Phases 1–3 complete**; 5.3 / 6.1 / 6.2 / 6.4 done; 4.1 / 4.2 / 4.4 / 5.1 done; 4.3 / 5.2 partial; 6.3 UTS built, MNT/PID/NET designed; 7.1 / 7.2 answered (Wayland-compositor support: see the TODO Board).

### Remaining ABI support (gaps + services)

Syscalls the dispatch still falls through to `ENOSYS` (each prints once via
`note_enosys`), grouped by what they block. These, plus the four foundational
behaviours below, are the whole of what is left to reach "full Linux ABI".

**Machines that are not there, and the answers that are honest because of it:**
- **No USB host controller, and no driver for one.** `/sys/bus/usb/devices` is
  therefore an empty directory - which is what Linux registers on a machine with
  no USB - and `lsusb` prints nothing and exits 0. A tree with anything in it
  would be a device this kernel cannot enumerate; the directory says what is true
  rather than what is hoped for. (`t-mountinfo` pins the empty directory.)
- **No loadable modules.** Every driver is compiled into the image, so
  `/proc/modules` is an empty file - what `lsmod` reads as a header and no rows,
  exactly as it does on a Linux built with no modules. A file that said "cannot
  tell you" would be the worse answer, since `lsmod` has no vocabulary for it.
- **The inode flags this kernel cannot keep** are refused with `EOPNOTSUPP`
  rather than stored and ignored: compression, encryption, fs-verity, casefold,
  project inherit, inline data, DAX, no-COW, journal-data, tail-merge, top-of-tree,
  secure-delete/undelete, AFS magic naming, extended-attribute inodes and the
  sparse-tail bit. `INODE_FL_VISIBLE`/`INODE_FL_MODIFIABLE` in `src/vfs/mod.rs`
  is the list, each bit named so a caller is refused for the right reason, and
  the three the ext4 driver owns (the extent tree, the directory index, the
  block-counting unit) are readable and not writable (§12.4b).

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
- `fanotify_init` (300) / `fanotify_mark` (301) — less common (antivirus, some
  mount monitors). (These two read `302`/`303` here until this line; the header's
  numbers and the ones `t-enosys` walks are `300`/`301`.)
- `keyctl` (250) — kernel keyring; needed by `keyutils`/some TLS/pam stacks.

**POSIX timers (`timer_create` 222, `timer_settime` 223, `timer_gettime` 224,
`timer_getoverrun` 225, `timer_delete` 226): implemented** (`t-timer-create`).
The number that used to be blamed for a hang was `vim`'s: its `timers` feature
asks for 222 while drawing, and the failure that followed the refusal is what
§2.5's follow-up is about — the redraw path then dereferenced a null timeout
flag and died. What the family answers, measured on a 6.x Linux at the
raw-syscall level so nobody has to re-derive it:

- `timer_create(CLOCK_REALTIME, NULL, &id)` succeeds and the expiry queues
  **`SIGALRM`** — a null `evp` is `SIGEV_SIGNAL` with `sigev_signo = SIGALRM`
  (measured by blocking `SIGALRM` and reading `rt_sigpending`), **not** the
  `SIGEV_NONE` an earlier revision of this note claimed. The id handed back is
  a small per-process integer, not a pointer, and a bogus id answers `EINVAL`
  on all five calls — **not** the `EBADF` the man page claims.
- The accepted clocks, measured: `REALTIME`(0), `MONOTONIC`(1),
  `PROCESS_CPUTIME_ID`(2), `THREAD_CPUTIME_ID`(3), `BOOTTIME`(7) and `TAI`(11)
  are accepted; `MONOTONIC_RAW`(4) and the two `*_COARSE` clocks answer
  **`EOPNOTSUPP`**; the two `*_ALARM` clocks answer `EPERM` (they need
  `CAP_WAKE_ALARM`); a number no clock has is `EINVAL`. This kernel accepts
  `REALTIME`/`MONOTONIC`/`BOOTTIME`/`TAI` — one tick clock under four names —
  and refuses the CPU-time clocks with `EOPNOTSUPP`, because it keeps no
  per-task CPU accounting: a timer accepted and never fired would be worse than
  one that says so.
- `sigev_notify` outside `{NONE, SIGNAL, THREAD_ID}` is `EINVAL` (including
  `SIGEV_THREAD`(2), which is a libc value that never reaches the kernel). A
  signal number runs `1..=64` on Linux; this kernel stops at `SIGRTMIN` (32),
  and refuses the range above it — the real-time gap below — rather than
  accepting a number with no disposition storage behind it.
- `TIMER_ABSTIME` is `EINVAL` here, the same refusal `clock_nanosleep` makes:
  one clock, no absolute domain (§2 records that choice).
- `SIGEV_THREAD_ID` aims the expiry at one thread of the caller's group (a tid
  outside it is `EINVAL`, as on Linux); `timer_getoverrun` on a fresh timer
  answers 0.
- **The shape musl actually builds works end to end.** `timer_create(...,
  SIGEV_THREAD)` is a helper thread blocked in a wait plus a `SIGEV_THREAD_ID`
  timer carrying `SIGRTMIN`; this kernel queues the expiry with `SI_TIMER`
  siginfo and the caller's `sigval`, and `rt_sigtimedwait` hands the record
  back with `si_code` intact — the one field musl checks before calling the
  callback. That needed three things beyond the timer table: `SIGRTMIN` as a
  queueable number, a 64-bit pending mask (the "64-bit pending masks
  throughout" this file asked for), and the signal **mask inherited across
  `clone`/`fork`** — musl blocks `SIGRTMIN` before creating the helper and the
  helper is what waits on it.
- Divergences: a timer belongs to the *task* that created it, not to its thread
  group; ids are the lowest free slot (Linux never reuses an id); the expiry
  runs on the scheduler's tick, so a deadline is tick-quantised and a 50 ms arm
  reads back as 52 ms — rounded up, never early.

The prerequisite named here was right: "a per-task list of armed clocks the
scheduler's tick scan can run down" is `sched::Table::check_posix_timers`,
behind its own armed flag, so a machine that never calls `timer_create` never
scans for one.

**Other missing (lower priority):**
- `userfaultfd` (323) — lazy-copy / CRIU-style migration; niche.
- `tid_add_object` / `tgid_add_object` (rseq side-objects) — rare.
- **`/proc/<pid>/maps` — answered.** The per-process directory carries it, in
  Linux's exact format: `start-end perms offset dev inode name`, addresses as
  lowercase hex, the offset zero-padded, `dev` as `%02x:%02x`, and the name in
  the fixed column Linux uses (73, measured across address and inode widths on
  7.2.7) when there is one - which is what makes the last field addressable
  whatever the width before it. The perms word's fourth character is the
  `MAP_SHARED` bit, so it is carried on the mapping rather than guessed from
  what backs it: a `memfd` mapped `MAP_PRIVATE` aliases the same frames and is
  still `p`. Three sources feed one file, because the address space is recorded
  in three places: the VMA table (`mmap`, `mprotect`, `brk`), a list of ranges
  the *loader* filled (see below), and the program break, which is what gives a
  range the `[heap]` name - applied by address, since this kernel's heap is an
  ordinary anonymous mapping. `[stack]` is recorded when the stack is laid out
  and stretched down as the stack grows, so a program reading its own maps from
  deep in a call is not told about a stack that stops above it.
  **What the work needed, and it is the interesting part:** `execve` maps an
  image's pages *eagerly* - it fills every page as it loads - so an image leaves
  **no VMAs at all**, and the same is true of a `MAP_PRIVATE` mapping of an
  ordinary file. A maps file built from the VMA table alone would therefore
  omit the one line a crash reporter actually wants: the program's own code.
  The loader's ranges and the private file mappings are recorded *beside* the
  table (`mm::vma::MappedRange`) rather than put in it, and the reason is the
  fault path rather than tidiness: recording them as `VmaKind::Anon` VMAs would
  make an address that should take a fault - one whose frame a `munmap` or a
  `MADV_DONTNEED` gave back - silently refill with zeros, which for a program's
  own code means blank bytes where the file's bytes belong. The cost of that
  choice is named with it: those ranges are the *load's* record and not the
  current state, so a `munmap` or `mprotect` over one of them - a dynamic
  program's `RELRO` pass does the second on every start - is not reflected in
  the report where a VMA would have been edited. `t-maps` pins the lines, not
  the mutation, so closing this needs a `forget_ranges`/`reprotect` on the
  recorded list before it can be pinned.
  **Two divergences, pinned as divergences:** an image's and a private file
  mapping's line reports `dev` `00:00` and inode `0`, because there is no object
  left to name - and `00:00` is what this kernel's own `stat` reports for every
  filesystem, so the two files agree rather than disagreeing. And a *shared
  anonymous* mapping is named `/memfd: (deleted)`, which is Linux's own name for
  a nameless `memfd` mapping (measured: `memfd_create("", 0)` + `MAP_SHARED`
  reports exactly that), because that is what the object here is; Linux reaches
  the same shape through `/dev/zero (deleted)`, which is an artefact of its own
  implementation.
  `memfd_create` now *keeps* the name it used to only validate, as
  `/memfd:<name> (deleted)` - the spelling both of its readers expect, measured
  on Linux - so a `wl_shm` pool's mapping says which buffer it is instead of
  leaving the column blank.
  Not written: `/proc/<pid>/smaps` (per-mapping `Rss`/`VmFlags` and the rest),
  and `/proc/<pid>/numa_maps`.
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
  `SO_REUSEADDR` is applied at `bind`/`listen` (the holder's option counts as
  much as the caller's - §3.3.3), while buffer sizes and `SO_LINGER` are still
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
- **6.3** namespaces: **UTS is served.** `unshare(CLONE_NEWUTS)` hands the caller
  a copy of its two names, and `sethostname`/`setdomainname`/`uname`/
  `/proc/sys/kernel/{hostname,domainname}` all read the caller's namespace -
  `t-ns-uts` holds both halves of that, the child's view differing *and* the
  parent's not. The other seven kinds and `setns` still refuse, now with Linux's
  real flag numbers and with the served/unserved distinction decided before
  anything is unshared. There is no mount, pid or net isolation. The build order
  - MNT, PID, NET next, each with its own `t-ns-*` - and what each kind must
  observably isolate are §6.3's, which now also carries what step 1 cost and the
  four gaps it left named.
- **Terminal size, and the "only the first few dozen lines" report.** A
  full-screen program lays itself out in the window `TIOCGWINSZ` names, so the
  answer is the display, not a default. Measured on Linux: a *fresh* pty pair
  nobody has sized answers **0 rows and 0 columns** (success, zeroed struct),
  `TIOCSWINSZ` on either end is visible on both because the window belongs to the
  pair, a resize of a pty whose slave is blocked in `read` runs its
  `SIGWINCH` through to that read as `EINTR`, and `TIOCGWINSZ` on a regular file
  is `ENOTTY`. This kernel answers 25x80 for an unsized pair instead of 0x0 —
  closer to what a program can survive, and a deliberate divergence — and a
  virtual console answers the screen it paints on (§7.1's resize rule), whose
  grid is clamped at `MAX_ROWS = 64` while a 1024x768 console is 128x48. The
  clamp is what a "the file is longer than the screen" complaint lands on: it has
  to be re-measured on the current tree, because the seat/resize work in flight
  changed exactly this path, and `vi` on a large file was reported as showing a
  few dozen lines *and* freezing — the freezing half is now §2.5, and the layout
  half needs a boot to confirm.
- **4.1** AF_UNIX is answered for `SOCK_STREAM` (see Phase 4), including
  `SCM_RIGHTS` descriptor handover and `SCM_CREDENTIALS`/`SO_PEERCRED` peer
  authentication — a real dbus broker can now both connect its clients and
  name them — and for `SOCK_DGRAM`, message-for-message. What the family still
  lacks is message boundaries on a *named* `SOCK_SEQPACKET` connection.

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
| **P12: ABI truthfulness** | gate restore ~days; Group A ~2 days; Group B ~1–2 weeks | High (correctness) — nine accepted-but-false answers, none recorded until now | Programs that trust a false success (§12) |
| **P13: Machine-seam purity + aarch64 parity** | estimate: purity 13.1–13.2 ~2–4 weeks; parity 13.3–13.5 the aarch64 ladder | High (the second machine becomes usable) / Medium (maintainability) | A third port (§9.2 RISC-V): every `target_arch` outside `arch/` is a file a port must edit (§13) |

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

**P11 Step A is done** — a deletion, landed on the green baseline the tree now has. P11's Steps B and C are the ones worth doing before the next device driver or the next
per-open-file resource is added, because each of those makes the debt one line larger.

**For the display work specifically**, §7.4 carries its own order, and its first step is not a
kernel change at all: a wlroots compositor on `WLR_RENDERER=pixman` is expected to run on the
DRM surface as it stands, which makes "boot one and read what it says" the cheapest way to find
out what that analysis has wrong.

The first two phases (epoll/timerfd + signal delivery) are the highest impact — they unlock the majority of real-world Linux userspace programs.
