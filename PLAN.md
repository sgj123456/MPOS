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
- [~] P2 Signal delivery — frame · mask · siginfo · interruptible waits (§2)
  - [x] a trapped signal ends a blocking terminal read with `EINTR` and one
        nobody trapped and nobody cares about does not (`t-tty-eintr`); the
        `VMIN`/`VTIME` pair decides what a read waits for at all
        (`t-tty-vtime`) (§2.5)
  - [x] `SA_RESTART` is acted on: an interrupted call whose disposition carries
        it is *rewound* through the frame the handler runs on, so the caller
        never sees the `EINTR` (`t-sa-restart`), and the calls Linux never
        restarts — `poll`, `select`, `epoll_wait`, the sleeps — still answer it
        (§2.5)
  - [x] `O_NONBLOCK` on a terminal: the measured `EAGAIN`/partial-byte table,
        through `open`, `fcntl(F_SETFL)` and `ioctl(FIONBIO)`, on the slave and
        on the master's own read (`t-tty-nonblock`, host tests) (§2.5)
  - [x] every other blocking wait a signal can end: pipe read/write, eventfd,
        signalfd, inotify, unix datagram/accept/connect, the pty write
        back-pressure, the AF_PACKET and route-netlink reads, `wait4`/`waitid`,
        and `nanosleep`/`clock_nanosleep` with the remainder in `rem`, each
        rewound under `SA_RESTART` where Linux rewinds it — including
        `ioctl`'s per-request verdict for the two DRM sync-object waits
        (`t-wait-eintr`, `t-sa-restart`, `t-drm-sync`). Named remainders: the
        console's serial transmit wait, whose park is still fatal-signal-only,
        and `FBIO_WAITFORVSYNC`, which discards its `WaitEnd` so a signal cannot
        end it at all (§2.5)
- [~] P3 Filesystem — hard links · locking · timestamps · perms · /proc (§3)
  - [x] `flock` owned by the open file description, record locks by the process
        (`t-filelock`) · read/write deliberately *not* enforced: Linux advisory
        locks never refuse an `open`ed read, and mandatory mode is a deprecated
        per-file opt-in (§3.2)
  - [x] ext4 `atime` bumped on read, with Linux's `relatime` rule so a read
        that sees no change costs no device write (§3.3)
  - [~] inode attribute flags — `FS_IOC_GETFLAGS`/`SETFLAGS`, `lsattr`/`chattr`'s
        word, with `IMMUTABLE` and `APPEND` *enforced* rather than stored
        (`t-fsflags`; ext4 persists `i_flags`, ramfs keeps the same subset) · the
        flags this kernel cannot honour are `EOPNOTSUPP`, never stored (§12.4b)
  - [x] the files the machine's `ls*` tools open: `/proc/modules`,
        `/proc/self/mountinfo` (and `/proc/<pid>/mounts`), `/sys/block` with a
        partition's `size`/`partition`, and an empty `/sys/bus/usb/devices`
        (`t-mountinfo`, `t-fsflags`) (§12.4b)

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
- [~] 4.3 TCP — the loopback, the listener and `SO_REUSEADDR` are done; two
      options are not
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
  - [x] `SO_REUSEADDR` at `bind`/`listen` — and the rule it gates is *not* "the
        new socket asked": measured on Linux, a holder that is **listening**
        keeps its name whatever either side says, while any other holder
        (a `TIME-WAIT` 4-tuple, an established connection, a bound socket)
        yields it only to a caller that set the option **and** whose holder had
        set it too — which is why Linux copies the option onto an accepted
        socket, and why a restart comes back onto its own port (`t-tcp-listen`)
        (§4.3.3)
  - [ ] `SO_LINGER{on,0}`'s egress reset / buffer sizes still record-only
        (§4.3.3)
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
  - [~] the compositor's **mouse cursor**: the plane is now driven, and what is left is a compositor
        that hands it an empty image. Measurements taken in one boot each on a `--mode gui` image with
        the card instrumented:
          * *The plane was unusable, and that was this kernel's doing.* weston builds one atomic state
            per plane and `drm_plane_state_add_props` adds `SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H` and
            `CRTC_W`/`CRTC_H` to every one of them without asking whether the plane has them; a handle
            of 0 reaches the card as property 0, is refused, and takes the whole commit down with it.
            The cursor plane published only `type`, `FB_ID`, `CRTC_ID`, `CRTC_X`/`CRTC_Y` and
            `IN_FORMATS`, so that is what happened: weston set `FB_ID` to **0** twice and never gave the
            kernel an image — the pointer moved, clicked, and was never drawn. Publishing the four
            rectangles (and honouring them by *being* the image: this card's cursor cannot scale or crop,
            so the identity is the only rectangle a commit may name and anything else is `EINVAL`) is
            the fix, and the same instrumented boot then shows weston binding `FB_ID` 100, then 101, and
            the kernel compositing the image at the pointer (`cursor.rs`'s blend, unchanged).
          * *What is left is a compositor that uploads nothing.* The same boot, with the contents of the
            bound buffer counted as it is composed: with `cursor-theme`/`cursor-size` in `weston.ini` the
            buffer is **all zero** (0 opaque pixels) on every commit, and with the theme commented out it
            carries the built-in arrow (5 opaque pixels, white) — the small cursor this card has always
            shown, now arriving through the hardware plane. So the kernel half is complete and measured,
            and the themed cursor is libweston's cursor path: a named gap, not a stub.
  - [x] **which consoles the kernel has is a `console=` list**, and nothing about
        how the machine was launched reaches it: `mpos.headless` /
        `mpos.nomirror` / `mpos.mirror` / `mpos.compositor` are gone, the kernel
        reads `console=ttyS0` (the wire) and `console=tty0` (the virtual console
        on the framebuffer), gives the log to every console named and parks on the
        last — so mirror is that list with the framebuffer last, gui is the
        framebuffer alone, and terminal and weston are the same one-word list
        (§7.1c). Two gaps are named there: `console=ttyN` pinning is refused
        rather than approximated, and `/dev/console` has no preferred-console
        plumbing.
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

**Wayland compositor (weston) support** — the cross-cutting work a rootful
compositor needs, all verified by `t-*` (GUI end-to-end is the operator's run of
`cargo xtask run --mode gui`):
- [x] Peer credential + fd passing (SCM_RIGHTS/`SO_PEERCRED`) — Stage 0 (see §4.1)
- [x] libinput device classification — evdev capability ioctls — Stage 1, `t-evdev`
- [x] Device discovery — `/dev/input`, `/sys/class/input`, input uevents — Stage 2
- [x] `wl_shm` buffers — `memfd_create` + frame-aliased shared mappings — Stage 3, `t-memfd-shm`
- [x] Framebuffer present — `FBIOPAN_DISPLAY`/`FBIO_WAITFORVSYNC` + a pointer
      (virtio-tablet) + `--mode weston` (kernel frees the scanout) — Stage 4, `t-fbdev`
  - [x] a pointer with real artwork — `weston.ini` names `cursor-theme=
        breeze_cursors` and a `cursor-size`; `breeze-cursors` was already in the
        image, but with no theme named weston drew only the fixed ~16px arrow
        libwayland-cursor builds in, which is what made the cursor look tiny and
        the theme "not applied" (a shell-config fix, not a kernel one)
- [x] DRM/KMS surface — `/dev/dri/card0` (UAPI ioctl + struct ABI, dumb buffers,
      page-flip, vblank events) for DRM-backend compositors — Stage 5, `t-drm`
- [x] **the screen has an owner** — `VT_SETMODE`'s `VT_PROCESS` negotiation
      (relsig → `VT_RELDISP`, bounded by a deadline and by a second `^A`), a DRM
      master bound to a console and revocable from it, `EACCES` on every
      presentation request from a session that lost its console, and `POLLHUP` on
      the card fd; before this a `^A` to a text VT lost the screen back to
      weston on the next mouse move (§7.1, `t-seat`)
  - [x] **and that owner is never invisible.** A master taken by a holder with no
        controlling tty — seatd handing weston the card, or a launch chain that
        skipped `TIOCSCTTY` — used to bind to *no console* at all, and an unbound
        master could not be revoked: `master_ok` passed it, `painted()` said the
        text console was free to draw, and the two raced on one scanout (the mouse
        that repaints a text VT, the shell-and-compositor flicker). `SET_MASTER`
        now binds to the console on the screen when the taker names none, so every
        held master has a console to lose.
  - [x] **and the negotiation tracks the screen.** `complete_switch` advances the
        pure handover table's `active` alongside the `ACTIVE` atomic; left at its
        boot value it asked console 1's `VT_PROCESS` promise on every switch after
        the first, so a compositor's `relsig` went to the wrong session (or none).
  - [x] **and the keyboard follows the screen, not the grab.** A keystroke reaches
        the shell unless the active console is in `KD_GRAPHICS` — the compositor's
        own `KDSETMODE` word that the screen is its to draw — so a compositor's
        typing does not land on the shell behind it whether or not libinput has
        taken `EVIOCGRAB` yet; a `--mode gui` text console (`KD_TEXT`) keeps its
        keyboard (`input::service`)
  - [x] **and the promise belongs to a process, not to a terminal.**
        `relsig`/`acqsig` go to the process that wrote `VT_SETMODE` - Linux's
        `vc->vt_pid`, delivered with `kill_pid` - and not to whichever group holds
        the foreground when the signal goes out. Two rules come with it: the last
        close of the descriptor that wrote a promise does *not* end it (seatd's
        `vt_open` sets the mode and closes the tty before the compositor has the
        screen, so clearing on close unmakes the claim in the same breath it is
        made), and a promise whose keeper has died is ended at the delivery that
        cannot land - Linux's `reset_vc`: `VT_AUTO`, no signals, `KD_TEXT`, the
        switch proceeds. Before this, quitting weston with `^C` handed `SIGUSR2` to
        the compositor or to the login shell - `ash` calling it "User defined
        signal 2" while the desktop was dying mid-release - and the seat was left
        occupied, so `./start-weston.sh` could not be run a second time. Measured
        against `drivers/tty/vt/{vt.c,vt_ioctl.c}`; pinned by `vtmode`'s host tests
        and by `t-seat` (§7.1)
  - [x] **and it comes back in graphics mode.** seatd opens the console, writes `VT_SETMODE` and closes
        the descriptor — writing it with `relsig` and `acqsig` both **zero**, the shape a seat manager
        that watches the console itself rather than waiting to be told. So the handover has no keeper to
        signal, `release_console` takes Linux's own `reset_vc` road, and the console lands back in
        `KD_TEXT`. What Linux does not do is let the word matter afterwards; there a master drives the
        screen because it is the master. Here the veto the text console paints under is
        `master_holds_screen`, and that asks `KD_GRAPHICS` as one of its three facts — so a handover
        nobody answered cost the compositor its screen for good: the text grid repainted over the desktop
        and every `MODE_ATOMIC` was answered `EACCES`. The master's own console is now a graphics console
        again the moment it is the one on the screen and a live master still holds the card; a master that
        has let go, one on another console, and one whose console is not the one being shown all leave the
        text console its screen.
  - [x] **and `VT_ACTIVATE` does not wait**, because Linux's is `set_console()`:
        the request is recorded, the console callback runs the negotiation, and the
        ioctl returns 0. The sleeping form here was attributed to a `vt_release`
        timeout that is not in the current console code at all - and it would have
        deadlocked a seat manager once the promise was aimed where Linux aims it,
        since the process asking for the switch is the keeper of the answer the
        wait is for. `VT_WAITACTIVE` is the one console ioctl that sleeps, and
        stays so; a console number the machine has not got is `ENXIO` (§7.1,
        `t-seat`)
  - [x] **a terminal question is answered by the emulator that owns the screen, and
        by exactly one of them.** Measured on Linux: a pty pair's line discipline
        forwards `ESC [ 6 n`, `5 n`, `c`, `? 6 n`, `OSC 11 ; ?` and `DCS 1 + q`
        byte for byte to the master and answers none of them (6 of 6 runs). This
        kernel answered everywhere - and so told a shell on a `--mode terminal`
        boot where its cursor was according to a screen nobody drew. `fish`'s
        prompt spacing pads the dangling line out to the *reported* column count
        (measured: `stty cols 40` turned the pad from 80 cells to 40 while the
        report still counted 80), so on any window wider than the guess nothing
        wraps and the `CR` + `ESC [ K` that close the block erase the line
        `echo -n hello` had just printed. `Attached` now decides per write from who
        reads the master, and the cursor a report answers with is the cursor the
        grid paints: both drive one `term::cursor::Cursor` - window size, scroll
        region, deferred autowrap, scroll at the bottom, `DECSTBM`, save/restore,
        `ESC D/E/M`, alternate screen, and the echo of what was typed, which the old
        private model never saw. Pinned by the 37-stream report/grid parity test and
        the answering-and-forwarding pair on the host, and by `t-tty-query` in the
        guest (§7.1b). Three gaps named there: the answering half needs a boot with
        a framebuffer to be pinnable at all; a serial line's real width is not
        knowable from the guest, which is also Linux's answer; and one byte is still
        counted as one cell, which is wrong for UTF-8 and needs a measurement first.
  - [x] **a last-resort stack for `#DF`.** No IST slot existed anywhere and the TSS
        carried `RSP0` alone, so a fault whose kernel stack could not take a frame -
        overflowed off its bottom, or walked over by a wild write - promoted to
        `#DF`, which could not push either, and the third fault reset the CPU into
        QEMU's `-no-reboot` exit without the kernel writing one byte: a machine that
        vanished with no panic was structurally unreportable. `#DF` now runs on its
        own 32 KiB stack, so `fatal()` names the vector and the `rip` (remaining,
        named: no NMI or machine-check handler is installed, so only `#DF` has the
        last resort - §7.1)
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
  - [x] and the device is the owner's while it holds it: a reader that is not
        the grabber is told nothing - not the records, and not a readable answer
        either - because every client has a *position* in the device's window
        rather than a turn at one shared queue. Two readers of an *ungrabbed*
        node each see every event, which the shared queue could not do either
        (whoever read first consumed it for everybody)
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
      `ARCHITECTURE.md` §2/§3 are unchanged (AGENTS.md §4 rule 5). Pinned by the
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
      `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE` are `1`; the gaps left
      are named in §7.4 (an out-fence that is already through, and no
      `TIMELINE`-flag behaviour to give the two flags this host's header has)
- [~] `DRM_VIRTGPU_*` 3D — the only path that makes `is_software()` false, and so
      the only one that lets niri, which rejects software EGL outright, run at
      all; virgl and venus are both built on it (§7.4). **The family is served
      now, 2D and 3D both**: all eleven requests are dispatched,
      `RESOURCE_CREATE` makes a real device resource across the new mailbox to
      the render task, the GL card negotiates `VIRGL` and the driver says so in
      its own words — `virtio-gpu: virgl=1 context_init=1 capsets=2 cached=2
      mask=0x6`, every number `user/staging/virtgpu-probe/` measured on Linux —
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
- [ ] 9.1 aarch64 complete boot (stages 3-8 remain) · [ ] 9.2 RISC-V support

**Architecture debt (Phase 11 — audit done, Steps A/B/C/D/E/F/G landed, HAL S1 landed (§11.4), V-1/V-4/V-13 closed)**
- [x] 11.A subtract: the orphan/duplicate machine modules, the leftover `[msgtrace]` in the
      dispatcher, the aarch64 linker script that does not exist (§11.2 Step A)
- [x] 11.E leaf crates: the seven pure, downward-only leaves (`err`, `pci`, `date`, `binfmt`,
      `cred`, `uts`, `seccomp`) moved out of `src/` into `crates/*` as workspace members,
      re-exported from `src/lib.rs` under their old names so no caller changed (`pub use
      mpos_err as err;`). The point is enforcement, not tidiness: a leaf crate cannot name the
      kernel, so the downward-only rule (AGENTS §4 rule 5, and the V-* edges this table keeps
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
- [x] 11.F the `target_os = "none"` gate gets a written criterion (`src/lib.rs`, `AGENTS.md` §4),
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
  reds that `AGENTS.md` had to call "load flakes"; they, and `t-udp`/`t-tcp-close`, are now run
  by name. A default red is therefore the kernel's, which is the only property that makes the
  gate worth reading.
* **`t-times`'s racy assertion is fixed, and the fix is the assertion.** One gate run of this
  §'s work was red on `t-times`'s *the elapsed clock advances across a spin* while the check
  directly after it — that `tms_utime` grew — passed, and a re-run was green. The mechanism was
  in the arithmetic and not in the kernel: `times()` returns the wall tick count and
  `tms_utime` the process's own CPU tick count, both scaled by the same truncating
  `ticks * 100 / TICK_HZ` (`handle_times`), so the spin — whose exit condition is the *CPU*
  counter crossing a `USER_HZ` step — can compare two wall readings that floor to the same
  value. The kernel's answers are Linux's; the assertion was the unsound part, and it is now
  two checks where there was one. Monotonicity is asserted over the spin, which is all a spin
  can prove, and the *strict* advance is pinned where it is deterministic: `times(NULL)` read
  around a 50 ms `nanosleep`, which is five `USER_HZ` steps at the ABI's 100 Hz and so cannot
  be truncated away.

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
4. ~~**`SO_REUSEADDR` at `bind`/`listen`** (§4.3.3)~~ **Landed.** It was the one
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
  much as the caller's - §4.3.3), while buffer sizes and `SO_LINGER` are still
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
| identity (setreuid…setfsgid, capget/capset), scheduling (sched_setparam…sched_getattr, getpriority/setpriority), memory (msync, mincore, mlock family), machine (personality, getcpu, settimeofday, clock_settime, unshare, setns, seccomp, setdomainname), signals (tgkill, rt_sigqueueinfo, rt_sigsuspend, pause) | — | Done | `src/syscall/linux/posix.rs` |
| `vhangup` | — | Done | dispatched, both cases honest. With no controlling terminal it answers `ENOTTY` (Linux's answer, gated by the corrected `t-posix`); with a controlling terminal it answers `EOPNOTSUPP` — the hang-up itself (tty re-init + `EIO` on open fds) is `PLAN.md` §12.2 B4, a tty-layer feature the kernel does not perform, so it refuses rather than fakes the success. The nice round-trip that used to sit on this row (`getpriority`/`setpriority`) landed in Phase 12 §12.2 B5 and moved back to `Done`. `src/syscall/linux/posix.rs` |
| `accept4`, `sendmmsg`, `recvmmsg` | 288/307/299 | Done | `src/net/socket.rs` |
| `rt_sigpending` | 127 | Done | `src/syscall/linux/posix.rs`; reads `current_pending()` |
| `io_uring_setup/enter/register` | 425-427 | Deferred | per the original phase's own call |


---

## Phase 2: Signal Delivery (High Priority) — **DONE**, with three named remainders

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

*Real-time signals — landed (stages A+B+C):* the pending set is now a canonical
64-bit map (signal `n` at bit `n-1`, so the libc wire set and the kernel's
internal set are one shape), the disposition table spans `1..=64` so `rt_sigaction`
installs a real-time signal and `kill`/`tgkill`/`rt_sigqueueinfo`/`timer_create`
accept the range (only `0`, `>64`, `SIGKILL`/`SIGSTOP` refused, as Linux), and
real-time signals are **queued** — a per-task bounded FIFO ring carries every copy
of `SIGRTMIN..=SIGRTMAX` in order with its own `si_value`, whether raised by
`sigqueue` or `kill`, while standard `1..=31` still coalesce. This closes the
"`rt_sigaction` answers `EINVAL` for `32..=64`" and "`33..=64` have no queueable
number" gaps. One bounded divergence remains and is named, not hidden: the ring is
fixed-length per task, so a flood beyond `RT_QUEUE_LEN` drops the extra copy
silently, where Linux's limit is a per-user `RLIMIT_SIGPENDING` and a full
`sigqueue` returns `EAGAIN`. Pinned by `t-rt-signal-queue` (three `sigqueue`s
drained FIFO with distinct values, two `kill`s yielding two copies, a handler run
once per queued copy) and the `RtQueue` host unit tests.

### 2.4 Verification

`user/abitest/src/bin/t-signal-delivery.rs` runs a handler end-to-end in
QEMU: it asserts the handler fires once and the interrupted `kill` resumes
with its own return (2.1); that a blocked signal is held, reported pending
by `rt_sigpending`, and delivered on unblock (2.2); and that a `sigqueue`
payload arrives as `SI_QUEUE` with its `si_value` intact (2.3). The host
side pins the `si_code` values and the siginfo table.

The two waits a signal has to reach are pinned by
`user/abitest/src/bin/syscall/t-tty-eintr.rs` (a trapped `SIGALRM` ends a
blocked terminal read with `EINTR`, the handler having run first, and the
retried read still gets the line typed while it was not reading; the same
timer with `SIG_IGN` does *not* interrupt it) and
`t-tty-vtime.rs` (the `VMIN`/`VTIME` table, timing included).

The rest of the family is pinned the same way, one program per half of the
rule: `t-sa-restart.rs` (the rewind, its counterfactual, and the calls Linux
never restarts — `poll` and `nanosleep` among them), `t-tty-nonblock.rs` (the
`O_NONBLOCK` table, and which end of a pty pair each bit belongs to), and
`t-wait-eintr.rs` (every other wait, each checked twice: the signal ends it with
the handler having run, and the same call then answers normally when the thing
it waited for arrives).

### 2.5 Interruptible waits, and `VMIN`/`VTIME` — implemented

A blocking read of a terminal is the wait almost every interactive program
sits in, so its two rules are the two most expensive ones to get wrong.

**It ends on a signal the caller will actually run a handler for.** Measured
on Linux, per signal, with `strace`: `SIGALRM`, `SIGWINCH` and `SIGCHLD` each
give `read(...) = ? ERESTARTSYS`, then the handler, then
`rt_sigreturn({mask=[]}) = -1 EINTR` — the interrupting is universal, not
per-signal. And measured in the other direction: a signal whose *default
action* is to be ignored, still at `SIG_DFL`, does not interrupt anything —
a `SIGCHLD` from an exiting child, a `SIGWINCH` nobody trapped, a `SIGURG`, a
`SIGCONT` each left the read blocked for its full watchdog. So
`table_has_deliverable` skips `SIG_IGN` **and** `SIG_DFL`-with-an-ignore
default; without that second half a shell's own read of its terminal would
come back `EINTR` after every job it runs.

The kernel's own answer is `-EINTR` with nothing consumed, which is what
`VfsError::Interrupted` (→ `err_of` → `EINTR`) carries from the node up to the
dispatch, and `CharDevice::read` returns a `VfsResult` for the same reason: a
0 on the read side means end of input, and a program told its terminal closed
stops reading instead of retrying after its handler ran. `block_until` stays
what it always was — fatal signals only — for the waits that are the
scheduler's or the boot task's own; the interruptible ones go through
`block_until_deadline`, which is also where a `VTIME` clock is read.

**`VMIN`/`VTIME` is an inter-byte clock, not a deadline on the read.** The man
page is ambiguous about what `VTIME` times; the table below is what a pty
answer was, on Linux, for the cases a text editor branches on (all reproduced
by `LineDiscipline::read_wait`'s host tests):

| mode | `VMIN`/`VTIME` | queued | Linux answered |
|---|---|---|---|
| raw | `0`/`0` | nothing | 0 bytes, 0 ms |
| raw | `0`/`0` | 3 bytes land at 100 ms | 0 bytes at 0 ms — the poll does not wait for them |
| raw | `1`/`0` | nothing | blocked (watchdog `EINTR` at 2 s) |
| raw | `1`/`0` | 1 byte at 100 ms | 1 byte at 100 ms |
| raw | `0`/`5` | nothing | 0 bytes at 503 ms |
| raw | `0`/`5` | 3 bytes at 100 ms | 3 bytes at 100 ms |
| raw | `0`/`2` | 1 byte at 100 ms | 1 byte at 100 ms, not at 300 |
| raw | `2`/`5` | 1 byte at 100 ms | 1 byte at 604 ms — `VTIME` after the **byte** |
| raw | `2`/`5` | bytes at 100 ms and 300 ms | 2 bytes at 304 ms |
| raw | `2`/`5` | bytes at 100 ms and 900 ms | first read 1 byte at 608 ms; second read 1 byte at 1300 ms |
| raw | `3`/`5` | 5 bytes at 100 ms | 5 bytes at 100 ms — `VMIN` is a floor |
| raw | `3`/`5` | 2 bytes at 100 ms | 2 bytes at 604 ms |
| raw | `5`/`0` | 2 bytes at 100 ms | blocked at 2 s with the 2 bytes still queued |
| canonical | `1`/`5` | "abc", no newline | blocked at 4 s |
| canonical | `1`/`5` | "abc" at 0, newline at 1200 ms | 4 bytes at 1200 ms |
| canonical | `0`/`5` | nothing, then `^D` | 0 bytes at the instant of the `^D`; the next read blocks again |
| raw + `O_NONBLOCK` | `0`/`5` | nothing | `-1/EAGAIN` at 0 ms — `O_NONBLOCK` outranks the clock |

Three consequences the previous code got wrong, all of them the same freeze:

- readiness was `out.len() >= VMIN.max(1)`, with the comment that `VMIN == 0`
  would "make every read return nothing at all, which reads as end-of-file".
  `VMIN == 0` is not an EOF: it is the poll, and the answer is 0 bytes without
  waiting. busybox `vi` and `vim` set `VMIN=0/VTIME=1` to tell a lone `ESC`
  from the start of an escape sequence, so pressing escape wedged them with the
  terminal still open.
- `VTIME` was read by nothing anywhere in the kernel, so no read could ever
  answer a timeout.
- the wait was `block_until`, which wakes for a signal and then goes back to
  sleep. `vim`'s timeouts are `SIGALRM`, and a resize is `SIGWINCH`; both
  arrived, were seen, and were thrown away.

Named gaps this leaves, and the three that have since closed:

- **`SA_RESTART` — closed.** The rewind is the frame's, in the shape this
  section predicted: `build_frame` stores `sc::RIP = regs.rcx - 2` (the
  `syscall` instruction is two bytes) and `sc::RAX = nr`, so `rt_sigreturn`
  resumes *on* the call with the caller's own arguments - they are in the frame
  the handler has not touched - and `rt_sigreturn` itself needed no change,
  because the frame *is* the record. Three things have to agree, and each is
  measured: the call came back `EINTR` (in this kernel: a wait ended with
  nothing consumed), the disposition being delivered carries `SA_RESTART`, and
  `abi::restartable(nr)` lists the call. Measured on Linux 7.2 (2026-10-01, a
  terminal read whose `SIGALRM` fired at 100 ms and whose byte arrived at
  400 ms): **with the flag** the handler runs at 100 ms and the *read* returns
  the byte at 400 ms; **without it** the same call is `-1/EINTR` at 100 ms and
  a retry gets the byte. `poll(2)`, `nanosleep(2)` and `epoll_wait(2)` answer
  `-1/EINTR` at 100 ms *with* the flag set, which is `signal(7)`'s
  never-restarted set and the reason `restartable` is an allow-list rather than
  "everything but those" — a rewind re-executes the call, so one that must not
  be run twice is wrong to include even if it never returns `EINTR` today.
  `t-sa-restart` pins both halves.
  - The flag is also what made `nanosleep`/`clock_nanosleep` become
    *interruptible*: a never-restarted call that cannot be interrupted is inert,
    and this one could not be - it slept through a signal and answered 0.
    Linux's answer is `-1/EINTR` with the time *left* in the second pointer, and
    that remainder is the whole reason the pointer exists: musl's `sleep()` is
    `nanosleep(&ts, &ts)` in a loop, so without it every signal doubles the
    sleep. Both are pinned, the remainder included.
- **`O_NONBLOCK` on a terminal — closed**, with the table below as its spec.
  Measured on Linux 7.2 (2026-10-01, a pty slave in raw mode with `O_NONBLOCK`
  set):

  | mode | queued | answered |
  |---|---|---|
  | `0`/`0` | nothing | **0 bytes** — it is the poll, and it never waited |
  | `1`/`0`, `0`/`5`, `2`/`5` | nothing | `-1/EAGAIN` **at 0 ms** — the clock is not consulted |
  | `2`/`5`, `5`/`0` | fewer than `VMIN` | **those bytes**, immediately: a minimum is a floor on a *wait* |
  | canonical | a partial line | `-1/EAGAIN` |
  | canonical | a complete line | the line |
  | canonical `0`/`0` | a partial line | `-1/EAGAIN` — canonical consults neither |
  The rule lives in `LineDiscipline::read_wait`, which is where the blocking
  table already lived and where a host test can hold both columns: a fourth
  input (`O_NONBLOCK`) and a fourth answer (`ReadWait::WouldBlock`). `Tty`
  carries the description's bit, `sync_nonblocking` is what puts it there, and
  the pty *master*'s bit goes to the pair rather than to the slave's terminal -
  the order matters, because `PtyMaster::as_tty` answers the slave's terminal
  (that is what makes `sudo`'s `TCGETS` on a master work). `t-tty-nonblock`
  pins the table, the `open`/`F_SETFL`/`FIONBIO` routes and the two ends.
- **The other bare `block_until` waits — closed**, and the list was longer than
  this section knew: `pipe.rs` (read, and write as a partial count),
  `eventfd.rs`, `signalfd.rs`, `inotify.rs`, `unix.rs` (the datagram queue, the
  connect-room wait and the accept queue), the pty slave's write back-pressure,
  and the AF_PACKET and route-netlink reads - the same wait behind two more
  descriptors, which `udhcpc`'s retry loop and `ip`'s own event loop are built
  on. `sched::block_until_interruptible` names the shape; a *write* that already
  stored bytes reports the partial count rather than `EINTR`, which is Linux's
  rule and the one that cannot duplicate bytes when the caller retries.
  `t-wait-eintr` pins each wait twice: the signal ends it with the handler
  having run, *and* the same call answers normally once the thing it waited for
  arrives, so a kernel that broke the data path to pass the first check fails
  the second. What is left of the family, each named rather than half-served:

  - **A pty write cut inside an expanded `CR LF`** re-offers that one input
    byte, so its carriage return lands twice. The count is in the writer's own
    units by construction (`input_prefix`), and the alternative - counting the
    byte - would lose the line feed; both render the same line.
  - **`wait4(2)`/`waitid(2)` — LANDED.** Their park (`block_until_child_event`
    and its siblings) was fatal-signal-only, so a caught signal woke the task and
    the park swallowed the wake. They now park on `block_until_on_interruptible`,
    which checks `table_has_deliverable` on the same borrow, and a catchable
    signal ends the wait with `EINTR`; the syscall gate restarts it under
    `SA_RESTART`, so `restartable` now lists `WAIT4`/`WAITID` — the exclusion it
    carried was precisely because the park never answered `EINTR`. Measured on the
    host (Linux 7.2.7: 2 s child, `SIGALRM` at 100 ms — `SA_RESTART` → the child's
    pid at 2000 ms, flag clear → `-1/EINTR` at 100 ms) and pinned in the guest by
    `t-wait-eintr` (a blocked `wait4` and `waitid` both end with `EINTR`, handler
    run, then the retried call still collects the child — the counterfactual that
    the wait path is not broken). The `SA_RESTART` rewind for the set that *does*
    restart stays in `t-sa-restart`.
  - **The console's serial transmit wait** (`console::write_bytes` parks on ring
    space), the same shape on the one wire the kernel writes from its own tasks.
  - **`ioctl(2)`'s per-request verdict — LANDED.** Linux rewinds an interrupted
    request only when its driver returned `-ERESTARTSYS`, so `SA_RESTART` restarts
    `DRM_IOCTL_SYNCOBJ_WAIT`/`..._TIMELINE_WAIT` and most other requests not at
    all. `abi::ioctl_restartable` now names exactly those two (magic `'d'`, command
    `0xc3`/`0xca` — the only requests this kernel's park answers `EINTR` for), and
    `restart_verdict` reads the request word (`regs.rsi`) for `nr::IOCTL` where it
    reads the number elsewhere; `build_frame`'s rewind re-runs the saved arguments
    unchanged, and the wait's `timeout_ns` is an absolute deadline, so a rewind can
    neither extend it nor lose it. Grounded in the driver's own return code (Linux
    `drm_syncobj.c` parks the fence and returns `-ERESTARTSYS` on a signal); pinned
    in the guest by `t-drm-sync`, which blocks a `WAIT_FOR_SUBMIT`, fires a
    `SIGALRM` into it from a `setitimer`, and has a forked child submit the fence
    afterwards — the wait answers `EINTR` without `SA_RESTART` and `0` with it, so
    a kernel missing the rewind (which gives `EINTR` in both) fails the second
    branch. The one remaining ioctl *wait* gap is `FBIO_WAITFORVSYNC`, which
    discards its `WaitEnd` (`ioctl.rs`) and so is not signal-interruptible at all —
    a separate §2.5 item, not a restart verdict. **Gated:** `cargo xtask test`
    ends `=== SUMMARY pass=141 fail=0 (141 built)` with QEMU exit 33 and
    `t-drm-sync === PASS` — the branch that proves the pin is load-bearing is B,
    which answers `0`; a kernel without the per-request verdict answers `EINTR`
    there too and fails it.
- **Canonical mode ignores both** — `VMIN` included. That is POSIX's wording
  ("non-canonical") and the measured 4-second block above, so nothing is
  missing here; it is recorded because the pair *looks* like it applies.

**And writing the pin for the pty's back-pressure found the bound unenforced.**
The pty master's buffer is capped at `TO_MASTER_MAX` (64 KiB) and a writer that
fills it is supposed to park - but the *guard* that chose between parking and
pushing straight through asked `crate::arch::interrupts_enabled()`, and that
question is false in an interrupt handler **and inside every syscall**
(`MSR_FMASK` masks `IF` on `syscall` entry). So a userland `write(2)` to a pty
slave never parked and never met the cap at all, while a kernel task's did:
measured by `t-wait-eintr`, a 96 KiB write returned 96 KiB and grew the buffer
past its 64 KiB bound — the worst-case rule (§4 item 4) unenforced on a queue a
user program fills. The context is a parameter now, and only a call site can
answer it: `Wire::Task` for `write(2)` and the terminal's own answers,
`Wire::Interrupt` for the echo of a byte that has just arrived (which must not
park - it runs in the handler that carried it). A full buffer then ends the
write with the errnos Linux gives it: the partial count, `EAGAIN` under
`O_NONBLOCK`, `EINTR` under a signal, and the count is in the writer's own units
(`input_prefix` maps the wire's bytes back, since `ONLCR` makes one `LF` two).


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
- **devfs nodes keep modes and owners now.** Before this a `chmod` on any device
  node answered success and stored nothing — udev's `MODE=`/`GROUP=` rules were
  logs, and `stat` showed a number no check ever read. Every `/dev` inode class
  (the generic stream node, the ttys, the DRM card and its leases, evdev,
  `uinput`, `fb0`, `ptmx`) carries a `devfs::NodePerms` and answers
  `keeps_modes() == true`, so the triads below decide what the device really
  means. Devices are born `0666 root:root` — the number `stat` already gave them —
  where Linux's devtmpfs births `0600` and lets udev widen it; that difference is
  the named remainder, and it needs per-class birth modes and group-name
  resolution, not another mechanism.
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

**And a *negative* backlog is `SOMAXCONN`, not a refusal.** `clamp_backlog` used
to answer `EINVAL` for one, which reads as the man page's rule and is not
Linux's: `__sys_listen` compares the argument to `SOMAXCONN` as an *unsigned*
int, so every negative value lands on the ceiling rather than being refused.
Measured on the host before the rule was written: `listen(fd, -1)` returns 0 for
`AF_UNIX` and `AF_INET` alike, and `ss -lx`/`ss -ltn` then report the backlog as
4096 - `net.core.somaxconn`, not the zero a "negative means a closed queue"
reading would leave. Nor is it a number anybody has to go looking for: `-1` is
what Rust's std passes on Linux from `UnixListener::bind` *and*
`TcpListener::bind`, so every Rust server and compositor asks for it on every
bind - and the `EINVAL` it got here is where niri stopped (§7.4). The rule lives
in one place, `net::unix::clamp_backlog`, which the IPv4 pool reads as well: that
arm had read `-1` as **zero**, a queue one deep, which is the same defect in the
other family's clothes. Pinned from both sides, `t-unix` and `t-tcp-listen`.

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

Three things live in this section, and the **loopback** (4.3.1) and
**`listen`/`accept`** (4.3.2) are done. The **socket options** (4.3.3) are the
part still open, and of the six, four now steer the stack - `TCP_NODELAY`,
`SO_KEEPALIVE`, `SO_REUSEADDR`'s bind arbitration - while two are stored and read
back without changing a packet.

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

#### 4.3.3 The options — **PARTIAL** (two still record-only)

`setsockopt`/`getsockopt` are no longer a no-op: each socket's registry `Entry`
carries a `SockOpts`, and the common options round-trip with the correct ABI —
`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_SNDBUF`, `SO_RCVBUF`, `TCP_NODELAY` (`SOL_TCP`)
as 4-byte ints, `SO_LINGER` as an 8-byte `struct linger` (in-out length honoured),
plus `SO_TYPE`/`SO_ERROR`. `SO_RCVBUF`/`SO_SNDBUF` report the stack's default
(64 KiB) until changed; unknown options are accepted and read back as 0.

**`SO_REUSEADDR` is applied at `bind`/`listen`, and the rule it gates is not the
one the man page suggests.** That is the finding, so the measurement comes
first. On this host's kernel, `127.0.0.1`, with the holder's own
`SO_REUSEADDR` and the new socket's varied independently, and with the holder in
each of the states a name can be in:

| the name is held by | holder's `SO_REUSEADDR` | new socket's | `bind` |
|---|---|---|---|
| a listening socket, open | either | either | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | no | **yes** | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | no | `EADDRINUSE` |
| a `TIME-WAIT` 4-tuple | yes | yes | **succeeds** |
| an established connection, listener closed | either | not both | `EADDRINUSE` |
| an established connection, listener closed | yes | yes | **succeeds** |
| a bound, never-connected socket | yes | yes | **succeeds** |

Three things fall out of that, and two of them are corrections to what this
option is usually described as doing:

* **The new socket's word is not enough.** The row where the holder did not set
  the option is `EADDRINUSE` *however loudly the new socket asks*. What makes a
  restart work is the other direction: Linux copies `SO_REUSEADDR` onto a socket
  when it is **accepted** (measured, `inet_csk_clone_lock`), so the listener's
  option is inherited by the connections that will later sit in `TIME-WAIT` on
  its port. A restart comes back because the *dead connections* had the option,
  not because the new socket does.
* **The exemption is not about `TIME-WAIT`.** A bound-but-unconnected socket and
  a live established connection both yield their name to a caller when both
  sides asked - so a port can be rebound while a connection is still open on it,
  and a listener can then *share* that name with it (measured: the new listener
  answers a fresh client while the old connection keeps its own). `TIME-WAIT` is
  simply the state this is usually noticed in.
* **A listener never yields.** Two live listeners on one name is what
  `SO_REUSEPORT` is for - it is not implemented here, and setting `SO_REUSEADDR`
  on *both* is not a substitute on Linux either (measured).

So `net::Stack::tcp_name_taken` is one clause over one table of holders: a
listening holder always conflicts; every other holder conflicts unless both it
and the caller set the option. The names it compares are the registry's recorded
ones plus, for a connection that recorded none (an accepted socket answers
`getsockname` from its own 4-tuple), the socket's own endpoint - and
`names_overlap` is the address half of the rule: the same port and either the
same address or the wildcard on one side, so `127.0.0.1:80` and `127.0.0.2:80`
coexist while either is refused beside `0.0.0.0:80` (measured). The holder's own
flag has to outlive its descriptor, so `Stack::teardown` carries it beside the
socket it named. The ephemeral pick behind a `bind(port 0)` (and behind
`listen`'s implicit bind) goes through the same predicate, which is what keeps an
implicit bind from landing on a `TIME-WAIT` port it would then have to refuse for
a name the caller never chose.

`t-tcp-listen` carries all of it - thirteen checks, every one of which is a line
of the table, plus "the restarted listener is *usable*" (a fresh client connects
to the taken-back name and its bytes arrive) and "a listener can share a name
with an open connection, and both serve". The whole test also runs on a Linux
host unchanged, which is the instrument: the same assertions that the guest must
satisfy are the ones a real kernel satisfies.

*Remaining (real effect, into smoltcp):* two. `SO_LINGER` is not honoured at
`close` - see 4.3.4 for why the reset it asks for is not something this stack can
send. `SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per socket at all until
smoltcp's buffers stop being sized once at set construction. Three that used to
be on this list are not: `TCP_NODELAY` reaches the live socket
(`set_nagle_enabled`), `SO_KEEPALIVE` arms smoltcp's probe timer at Linux's
default interval, and `SO_REUSEADDR` is the rule above.

Two named gaps ride with the rule rather than the option. The predicate lives in
`net/mod.rs`, which is gated `target_os = "none"` (`src/lib.rs:48`), so it has no
host unit test - the guest program is the pin, the same situation `AF_PACKET`'s
parsers are in (§4.5). And **smoltcp's `TIME-WAIT` is ten seconds**
(`CLOSE_DELAY`) where Linux's is sixty (`TCP_TIMEWAIT_LEN`), so this machine
gives a name back six times sooner. The rule about *who* may take it is
identical; only the wait is short.

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

*Remaining after those:* `SO_KEEPALIVE` probe packets; and a reset on egress for
`SO_LINGER{on, 0}`. `SO_RCVBUF`/`SO_SNDBUF` cannot be honoured per
socket as things stand: smoltcp sizes its buffers when the socket set is built,
so a real answer needs either a set rebuilt per requested size or a limit applied
in the mpos read/write queue - and until one of those exists the options should
say so rather than round-trip a number that changes nothing. `SO_REUSEADDR` used
to be on this line and is not any more: it is applied at `bind`/`listen` now
(§4.3.3), and the rebind allowance it gates turned out to be a rule about the
*holder's* option as much as the caller's.

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

### 4.5 `AF_PACKET` — the link layer as a socket — **PARTIAL**, two gaps named

The kernel had been the last word on the machine's address with no door for
userland to overrule it: `10.0.2.15` was a boot-time constant and `EAFNOSUPPORT`
was the answer to every client that wanted to negotiate one. `src/net/packet.rs`
opens that door in the shape Linux spells: `socket(AF_PACKET, SOCK_DGRAM,
htons(proto))`, a tap hung on the NIC seam (`NicDevice::receive` hands every
frame to the sockets *before* the stack decides whose business it is — which is
the point: a BOOTP reply for an addressless machine is nobody's business in the
IP sense), `sendto` with a `sockaddr_ll` whose Ethernet header this kernel
builds from the device's own MAC, per-socket bounded rings with the oldest-frame
drop, and `bind`/`getsockname`/`recvfrom` answering through the same
`sockaddr_ll` (`linux/if_packet.h`'s real offsets: 20 bytes, address at 12).
`SOL_PACKET` membership requests succeed because the tap already sees every
frame of the socket's type; every other level is `ENOPROTOOPT`, Linux's own
answer.

The named gaps, each with what it waits on:

* `SOCK_RAW` — frames *with* their header on both sides. It is `EOPNOTSUPP`
  because a caller that writes its own header writes a source MAC this kernel
  cannot vouch for; the honest shape needs the header passed through untouched
  and the length checked, not invented.
* The boot still has no client. The *mechanism* is what `t-dhcp` pins — a
  hand-built DHCPDISCOVER to the broadcast and slirp's BOOTREPLY read back with
  its transaction id — but wiring `busybox udhcpc -s` into OpenRC (and giving
  the selftest initramfs one too, or its network tests ride the static default)
  is the remaining move before the static bring-up can be retired. Until it
  happens the static 10.0.2.15 is a boot *default* over a live interface, not
  a decision the kernel reserves: `RTM_NEWADDR` and now `AF_PACKET` both
  overrule it.
* The parsers over `sockaddr_ll` and the Ethernet frame have no host-side pin.
  `AGENTS.md` §4 asks every new parser for a fuzz target, and `net/packet.rs`
  has none — not from oversight but from the §6 trap that gates the whole `net`
  module on `target_os = "none"` (`src/lib.rs:48`), so neither a host unit test
  nor a `fuzz/` target can even reach it, and `net/socket.rs`'s annotated test
  module stands as the standing proof of that (V-11). The only pin today is
  `t-dhcp` on the machine. The gap closes when the wire-format halves —
  `sockaddr_ll` decode, header parse, ring accounting — move to an ungated pure
  module the way `evdev` and `uinput` already do, and gain a fuzz target there.
  Until then this line is the recorded reason, not a silent exemption.

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

### 6.3 Namespace Support — **UTS BUILT; THE OTHER THREE DESIGNED**

Two calls (`unshare`, `setns`), eight kinds of namespace, and one of each kind
until this pass. Most of this section is still the *decision record* rather than
a to-do list, because the decisions are what §6.3 kept being deferred for: which
of the eight this kernel can isolate observably, in what order they are worth
building, and what each one costs the layers that would have to grow a
per-namespace answer. **The first of the four decisions has since been built** -
`CLONE_NEWUTS`, in `src/uts.rs` and the `Task` field that holds it - so the
table below reads "build first" as done and the rest as designed. "Step 1,
landed", at the end of this section, is what the step actually cost, what it
measured, and what it deliberately did not do.

**The rule that decides the list is §4's rule 4** - refuse rather than
approximate. A namespace that isolates nothing is not a namespace, it is a
promise, and `EOPNOTSUPP` is what a promise is worth: `unshare(CLONE_NEWIPC)` on
a machine with no SysV IPC objects and no POSIX message queues would hand back a
"new IPC namespace" whose contents are exactly the old one's, and a caller that
believes it stops looking. So each of the eight is decided by *what a program
could observe*, and only the four with something observable behind them are on
the build list.

| flag | what it must observably isolate | verdict |
|---|---|---|
| `CLONE_NEWUTS` | `uname`'s `nodename`/`domainname`: `sethostname` in one namespace must not be visible in another, through **all three** doors | **built** - "Step 1, landed" below |
| `CLONE_NEWNS` | the mount table *and the root*: a `mount` in one namespace invisible in another, and `chroot`/`pivot_root` giving a container its own `/` | **build second** |
| `CLONE_NEWPID` | the numbers a process *sees*: a child of the new namespace is pid 1 to itself, and `/proc` is numbered per reader | **build third - the expensive one** |
| `CLONE_NEWNET` | interfaces, addresses, routes and the socket table: a socket bound in one namespace is not reachable from another | **build fourth** |
| `CLONE_NEWUSER` | uid/gid mapping, and the capability set *within* the namespace | **refused** until there is a mapping to store |
| `CLONE_NEWIPC` | SysV IPC objects and POSIX message queues | **refused**: this kernel has neither, so it isolates nothing |
| `CLONE_NEWCGROUP` | the cgroup tree a process sees | **refused**: there is no cgroup tree here to see |
| `CLONE_NEWTIME` | `CLOCK_MONOTONIC`/`BOOTTIME` offsets per namespace | **refused**: one tick clock, and no per-namespace offset to store |

**UTS: the state is two strings, and the cost is that they have three doors.**
`nodename` and `domainname` are one process-wide pair today (`vfs::procfs`'s
`HOSTNAME`/`DOMAINNAME` statics), read by `sethostname`/`setdomainname`,
`uname`'s `nodename`, and `/proc/sys/kernel/{hostname,domainname}` - kept in one
home precisely so the three cannot disagree. A namespace makes that home *the
caller's*: a `Uts { nodename, domainname }` the task owns, with `fork`/`clone`
inheriting the reference (a child shares its parent's namespace until it
unshares) and `unshare` **copying** it, which is Linux's `copy_utsname` - the
new namespace starts with the old one's contents, so a `sethostname` afterwards
is invisible to the parent. This is the one whose whole cost is the plumbing;
it is first because it is small, and because it is the shape every other
namespace repeats.

**MNT: the one that costs the VFS — the root half has landed, the table has not.**
The per-process *root* — the prerequisite this whole item was deferred on — is
done. A task carries its own root (`sched::Task.root`, inherited on `fork`/`clone`,
set by `chroot(2)` through `sched::set_root`), and the one absolute-path walk
(`Vfs::resolve_walk`) begins, clamps `..`, and restarts absolute symlinks at *the
caller's* root rather than the machine's. A `chroot` now re-homes the calling
process and its future children — `t-chroot` has a child chroot, confirm its jail,
and prove the parent's view of the same `/tmp` is untouched, the `t-ns-uts` shape
for a per-task view. `set_global_root` is gone; what the boot's `switch_root` used
it for it now gets by inheritance (pid 1 is every process's ancestor). **The mount
table is still a single global**, and `pivot_root(2)`/`MS_MOVE` act on it
machine-wide — which *is* Linux's answer with one mount namespace, and stays
named until the next step. What is left: move the table onto the task too (an
`Arc<MountNamespace>` holding the entries, the root having already moved), have
resolution ask *the caller's* namespace for its mounts, and let
`unshare(CLONE_NEWNS)` clone the tree the way `copy_mnt_ns` does — a `mount` in the
child then invisible to the parent, and `pivot_root` per-namespace, which is what
gives a container its own `/`. Two things stay named refusals rather than silent half-answers: **mount
propagation** (`MS_SHARED`/`MS_PRIVATE`/`MS_SLAVE` - a mount event crossing
namespaces is a second mechanism, and ignoring the flag would silently change
what "unshare" meant), and **`/proc/mounts`** having to render the reader's
namespace rather than the machine's.

**PID: expensive because of the vocabulary, not the feature.** A pid here *is*
the task slot - `sched::policy::TaskId(pub usize)`, the number `getpid` returns,
the number `/proc/<pid>` is named after - and the slot is **reused**. A pid
namespace needs the thing Linux built for exactly this and this kernel does not
have: a `struct pid` whose lifetime is its own, a per-namespace number mapped to
it, and `/proc` rendering the numbers the *reader's* namespace gives. It is also
where the reusable-slot problem §7.1 already records for `VT_SETMODE`'s promise
("Linux holds a refcounted `struct pid`, so a promise can never be inherited by
an unrelated process later given the same number") would have to be solved once
and properly, rather than twice. The semantics to hold, none of them optional:
`unshare(CLONE_NEWPID)` renumbers the caller's *children* and not the caller
(the process that asked keeps its own pid - it is the first child that is 1);
`setns` into a pid namespace has the same "children only" effect; and `/proc`
must be per-namespace, which means the pid directory for a process is a
different name in two namespaces that both can see it.

**NET: one `Stack`, and the device is the reason the list is short.** `net::NET`
is one interface pair (`eth0` + `lo`), one address table, one route table and one
socket registry. The record is already shaped like something a second copy would
work (`Stack` owns its fabrics, and the two-fabric split landed for the
loopback), so a net namespace is a second instance with the socket registry
keyed to it - but the *wire* is one virtio NIC with one MAC and one queue, and a
namespace cannot take it from the machine. So the first target is a namespace
whose `lo` is its own and which has no wire, which is what a container's private
network is, and what makes a socket in it unreachable from the parent. Moving a
physical device between namespaces is a later question and is refused until
there is a place to move it to.

**`setns`, and how a namespace is named.** Linux reaches a namespace through a
descriptor opened on `/proc/<pid>/ns/<kind>`; no namespace here can be named
that way, so the call answers `EINVAL` for an open descriptor and `EBADF` for a
closed number - all it can learn. The design follows from the table: each
namespace object becomes an inode whose open descriptor *is* the namespace (so
two descriptors on one `/proc/<pid>/ns/uts` name one object, with one `st_ino`),
`setns` checks the descriptor's type against `nstype` (a mismatch is `EINVAL`,
Linux's answer), and `NS_GET_USERNS`/`NS_GET_PARENT` are out of scope until
there is a user namespace for them to point at.

**Two decisions about the call itself, rather than about a namespace.**

* A flags word naming a namespace this kernel does not serve is refused **whole**
  - `EOPNOTSUPP`, and nothing is unshared. Linux's `ksys_unshare` is not atomic:
  by the time a later flag fails, an earlier one's unshare has already happened.
  This is a deliberate divergence, and it is the direction §4.4 prefers - a
  caller that asked for two namespaces and got an error learns that *neither*
  happened, instead of being left halfway with nothing saying so. It also means
  the served set can grow without a caller ever observing a half-built word.
* The flag vocabulary stays Linux's, which is now true in the handler and pinned
  by `t-posix` (see below). When a namespace is served, its own flag stops
  answering `EOPNOTSUPP` and starts answering Linux's `EPERM` for an
  unprivileged caller - the same `CAP_SYS_ADMIN` gate Linux puts on the seven
  privileged kinds.

**Verification, one namespace at a time, before the next one starts.** Each of
the four lands with its own `t-ns-<kind>` program, and the shape is the same for
all four - *the child's view differs and the parent's does not*, because the
parent's half is the assertion a "namespace" that is only a shared global
fails:

| program | the unshared side | the parent's side |
|---|---|---|
| `t-ns-uts` | a child that unshares reports `inside`/`inside.test` through `uname` **and** both sysctls, and a task that unshares keeps the names it had | the parent that did not unshare still reports the old names, its own three doors still agree, and a child forked *before* a refused mixed word keeps sharing the parent's namespace |
| `t-ns-mnt` | `mount` a tmpfs at a directory, then read it back | the parent's `/proc/mounts` does not carry it, and the directory is empty there |
| `t-ns-pid` | the first child is pid 1 to itself (`getpid`), and `/proc/1` names it | the real number is unchanged, and `/proc/<that number>` still names it |
| `t-ns-net` | a socket bound to a port is reachable from inside | the same port is free in the parent's namespace, and a connect to it is refused |

Each also pins its refusals: an unprivileged caller (`setresuid`-dropped) gets
`EPERM` for a served namespace, an unserved one still answers `EOPNOTSUPP`, and
a flags word mixing the two is refused whole.

**Order, and why it is that one.** UTS, then MNT, then PID, then NET: each step
is independently verifiable (its `t-ns-*` above), and none of them needs the next
one to be true. UTS is first because it is the cheapest complete example of the
shape - a per-task object, an inheritance rule, a copy on `unshare`, three doors
- and because it exercises the "refused whole" rule with nothing at stake. MNT
is second because it is the one containers actually need and because its
prerequisite (a per-process root) is a divergence worth fixing on its own. PID
is third because it is the most expensive and the least optional for anything
that runs an init inside; NET fourth because a namespace with `lo` alone is
already useful and the wire is a separate question. USER, IPC, CGROUP and TIME
stay refused, each with the reason in the table above, and each is worth
revisiting only when the thing it would isolate exists (§8.2's seccomp and §7.4's
compositor work are both more urgent).

**Step 1, landed: UTS.** The shape the paragraph above predicted was the shape it
took, which is worth recording because it is the claim this section was written
to test: a `Uts` object holding the two names (`src/uts.rs`, not target-gated,
so the copy rule is host-tested), a `Task::uts: Arc<Uts>` field, `fork`/`clone`/
`spawn_thread`/`try_spawn_user` inheriting the `Arc` by reference, and
`unshare(CLONE_NEWUTS)` replacing the caller's with `Uts::copied()` - Linux's
`copy_utsname`, so the new namespace *starts* from the old one's names and only
then diverges.

**The three doors were the whole of the cost, and one of them was already
broken.** `sethostname`/`setdomainname` wrote a pair of process-wide `String`s
in `vfs::procfs`; `uname`'s `nodename` read them; and `/proc/sys/kernel/hostname`
read them. Making the home *the caller's* was the edit. What the edit found is
that the domain half of the pair had never been wired: `uname` reported the
literal `(none)` and `/proc/sys/kernel/domainname` hardcoded the same string on
its read *and dropped every write*, so `setdomainname(2)` had no observable
effect through any door at all. Both now read the caller's namespace, which is
what makes the three agree about *which* namespace they are answering for - the
property `t-ns-uts` holds with one `Doors::agree()`.

**The capability gate is on all four doors, measured rather than recalled.**
Run as a non-root caller on this host's kernel (7.2.7): `sethostname(NULL, 100)`,
`sethostname(name, -1)` and `sethostname(name, 65)` all answer **`EPERM`** - so
the `CAP_SYS_ADMIN` check comes *before* the length check and before the pointer
check, not beside them - and the same holds for `setdomainname`, for
`unshare(CLONE_NEWUTS)` and for every `CLONE_NEW*` bit. The kernel's test for the
capability is the effective-uid one the entropy ioctls already use. The sysctl
files get their own gate for a different reason: on Linux the *file's mode*
(0644) is what refuses a non-root writer, and procfs here keeps no per-node
modes, so the capability is checked in the write policy and answers `EACCES` -
the errno Linux's own door gives. That the mode is not really there to check is a
named divergence below.

Two measurements decided the flag word's shape, and both are Linux's:

| as a non-root caller | Linux 7.2.7 | this kernel |
|---|---|---|
| `unshare(CLONE_NEWUTS)` | `EPERM` | `EPERM` |
| `unshare(CLONE_NEWUTS\|CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP`, **nothing unshared** |
| `unshare(CLONE_NEWNS)` | `EPERM` | `EOPNOTSUPP` |
| `unshare(CLONE_NEWUTS\|0x1000)` | `EINVAL` | `EINVAL` |
| `unshare(CLONE_THREAD)` | `0` | `0` |

The two rows that are not Linux's are the deliberate ones: a namespace this
kernel does not *serve* is `EOPNOTSUPP` rather than `EPERM`, because `EPERM`
would send the caller looking for a capability that does not help; and the
unserved bit is decided before any grant, so a mixed word unshares nothing.
Linux's `ksys_unshare` does not work that way - it walks the word and a later
failure leaves an earlier unshare done - and the atomicity is what `t-ns-uts`'s
witness child pins: it is forked *before* the refused call, so a rename the
parent makes afterwards is a rename it sees only if the two are still one
namespace. The whole verdict is a pure function over the word and the caller's
privilege (`abi::unshare_verdict`), which is where the four host tests argue with
it.

**Named gaps, each with what it waits on.**

* **`clone(CLONE_NEWUTS)`** - the *child* getting the copy - is still `EINVAL`:
  `validate_clone_flags` refuses namespace bits wholesale, and it says so. Serving
  it is a separate decision (where in `copy_process`'s order the copy happens,
  and what `CLONE_NEWUTS|CLONE_THREAD` means for a group that shares an address
  space) and it wants its own measurement, so the `EINVAL` stands as "this call
  cannot do that here" rather than as a half-built grant.
* **The six thread-group flags still answer `0` for a caller that has siblings.**
  Linux refuses with `EINVAL` there, because the descriptor table or the handler
  table really is shared. This kernel answers `0` for every caller, which is
  Linux's answer only for a single-threaded one - and since threads exist now
  (`t-clone`), the old comment's premise ("a process is its own only thread") is
  no longer universally true. Splitting a resource off from one's siblings is
  what is not modelled.
* **`setns` still cannot name a namespace**, and that is now the interesting
  half: `/proc/<pid>/ns/<kind>` does not exist, so there is no descriptor for the
  call to check a type against, and it answers `EINVAL`/`EBADF` - all it can
  learn. The design above (each namespace object an inode whose open descriptor
  *is* the namespace) is unchanged and still the plan for the second step.
* **`/proc/sys/kernel/{hostname,domainname}` are not mode-policed.** procfs keeps
  no per-node modes, so the capability check in the write policy is what refuses
  a non-root writer, and it answers `EACCES` - the right errno by the right
  argument for the wrong mechanism.

**And the flag table under all of this was wrong until this pass.** A caller does
not send a flag *name*; it sends the number `unshare(2)` documents, and this read
`CLONE_NEWNS` as `0x1_0000` - which is `CLONE_THREAD` - and `CLONE_NEWTIME` as
`0x8_0000`, which is `CLONE_SETTLS` and is not a namespace at all. A program
asking to unshare the mount namespace was told `EINVAL`, as though it had sent
something undefined, while a program passing `CLONE_THREAD` was told
`EOPNOTSUPP`, as though it had asked for a mount namespace. Nothing caught it
because the pin in `t-posix` had been written from the same misremembering, so
the test agreed with the bug - the case rule 1's carve-out is for, and the test
moved with the measurement.

The measurements, one call per child on this host's kernel, and the table the
implementation now holds:

| flags | Linux | why |
|---|---|---|
| `0x2_0000` (`CLONE_NEWNS`) | `EPERM` | a namespace flag, refused for want of `CAP_SYS_ADMIN` |
| `0x80` (`CLONE_NEWTIME`) | `EPERM` | the same; Linux puts this one in the *low* byte |
| `0x8_0000` (`CLONE_SETTLS`) | `EINVAL` | not a namespace and not something `unshare` can do |
| `CLONE_VM`/`FS`/`FILES`/`SIGHAND`/`THREAD`/`SYSVSEM` | `0` | thread-group splits, and a single-threaded caller has nothing to split |
| `CLONE_PIDFD`, `CLONE_IO` | `EINVAL` | real flags, meaningless to `unshare` |
| `CLONE_PIDFD\|CLONE_NEWNS` | `EINVAL` | an invalid bit outranks a namespace beside it |

So the eight namespaces are `0x80`, `0x2_0000`, `0x0200_0000` … `0x4000_0000`;
the six thread-group flags are a genuine no-op for a caller with no sibling to
split from, which is what the measurement above was taken on and what Linux
answers for a single-threaded process; and `EINVAL` is decided before the
namespace refusal, which is Linux's order. All of it is pinned by `t-posix`.

Two of those rows have moved since, and one of them is now an overstatement.
`CLONE_NEWUTS` no longer answers `EOPNOTSUPP` - it is the one namespace this
kernel serves, and the row above is the measurement the *unserved* seven still
give ("Step 1, landed" earlier in this section). And "a process is its own only
thread" stopped being true when threads landed (§6.4): the six thread-group flags
are a no-op for the *unshared* caller the table was measured on, and for a caller
that does have siblings Linux answers `EINVAL` where this kernel still answers
`0`. That is a named gap in the "Step 1, landed" list rather than a claim.

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

**Threads are real, and this section said otherwise for longer than it should
have.** The paragraph here used to read "one request in this family is
deliberately not answered, and it is the largest hole left in the process ABI:
`clone(CLONE_THREAD)` — thread creation — answers `ENOSYS`". That had stopped
being true at commit `58c6947`, which landed a thread group (`Mm`: the frames
behind the group's mappings, how many threads run on them, the paging root a
thread switches to, and where a leaving thread says so), `spawn_thread` reached
from `sched::clone`, `CLONE_CHILD_CLEARTID` writing zero and waking the futex
there - which is what `pthread_join` sleeps on - `exit_group(2)` ending every
thread of a group while `exit(2)` ends only the caller, and `getpid` answering
the group id while `gettid` answers the task's own. `t-clone` pins it and sits in
the default sweep: a thread's write is visible to its creator, which is the whole
difference between a thread and a fork. The stale paragraph was contradicted by a
green test the gate ran every time, which is the lesson worth keeping from it.

What genuinely remains is narrower, and each item is named rather than implied:

* **`CLONE_SIGHAND` is accepted and the handler table is still copied.** Linux
  shares it under that flag; here a handler installed by one thread is not
  installed for its siblings. Nothing in this system's userland does that yet, so
  a *copy* is the honest version of "not shared" - but it is a divergence and not
  a completion.
* **`CLONE_VM` without `CLONE_THREAD` is answered with a copy.** That is
  `posix_spawn`'s shape met precisely (such a child only ever reads the caller's
  memory before `execve`), and the doc comment at `sched::clone` says why a copy
  beats a share there - but it is not the shared address space the flag names.
* **The rest of the thread ABI** is wired where a caller needs it: the tid
  writes, the TLS base, the signal mask inherited across `clone`/`fork`,
  `get_robust_list`, and the per-thread `/proc/<pid>/task` listing
  (`thread_snapshots`).

---

## Phase 7: Device Drivers — **7.1, 7.2 and 7.3 are done; §7.4 has two steps left**

The device work is finished: display, input, and now storage. §7.4 is a different
kind of thing - it is *analysis* of the compositors beyond weston, written from
their sources and the UAPI rather than measured here, and two of its six ordered
steps remain (§7.4's own list). One item under 7.1 is also still open and is
written down there rather than here: weston's desktop-shell does not paint its
pointer, and the measurements say so far that it is not this kernel's doing.

Two things this phase measured are worth carrying forward, because both were
*surprises that booting found rather than reading*: the cursor plane's composition
is the one piece of arithmetic on the presentation path and lives in
`crates/gpu/src/cursor.rs` with host tests, and the sync object family's semantics
disagree with the upstream sources in the one place that decides its design, which
is why they were measured on a real card before any of it was written (§7.4). The
third, from §7.3, is the same shape one layer down: a driver whose every line is a
register access has no host test to fail, so its two wrong decisions - `PxSIG` is
not the presence question, and an unpartitioned disk still has a name - were found
only by booting it.

**Gate for the phase, quoted from its own summary**: host `618 passed`,
`mpos-xtask 36 passed`, guest `=== SUMMARY pass=132 fail=0 (132 built)`, QEMU exit
33, `cargo fmt --check` clean and `cargo clippy` clean on the bare-metal target and
on `mpos-xtask`.

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
  `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`, and GEM dumb buffers
  (`CREATE_DUMB`/`MAP_DUMB`/`DESTROY_DUMB`) whose `mmap` aliases real pages with
  the same per-mapping frame reference discipline the memfd uses. A page flip is
  applied to the scanout and reports back as a flip-completion `drmEvent`
  readable (and pollable) on the card fd. `SET_MASTER`/`DROP_MASTER` are a real
  claim rather than a success: the master belongs to an open file description,
  is bound to the console the caller is on, and loses the screen - `EACCES` on
  every presentation request, the queued events drained, `POLLHUP` on the card
  fd - the moment that console is switched away from (§7.1).
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
`mm_width`/`mm_height` of zero; a flip-completion event is stamped
with the frame's landing (the render task's present), not the commit, and the
card fd announces it - but there is no vertical blank and no `WAIT_VBLANK`; dumb-buffer mmap uses a `handle << log2(PAGES_PER_BUFFER)`
offset encoding rather than a DRM VMA manager; leasing is a restricted
descriptor but `DRM_MODE_SEAL_ALL` and `LEASE_IMMUTABLE` are recorded and not
enforced, so a sealed lease can still be revoked and a sealed object still
accepts a mode change; and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` and
the colour-pipeline cap are refused `EOPNOTSUPP`, the first of which the
virtualized drivers are the ones that ask for.

**The two fences a commit carries are no longer that list's example.**
`IN_FENCE_FD` used to be "stored but nothing waits on it", and there was no
`OUT_FENCE_PTR` at all; both are now real - a commit handed an acquire fence is
ordered behind it before a pixel is copied, and a commit asked for an out-fence
writes back a descriptor the caller can wait on - and the `SYNCOBJ_*` family
that gives them something to name is dispatched. See "What the sync object
family answers" below for what the family means here, which is measured rather
than assumed.

**The scanout follows the host's display size.** The resolution used to be the
one `GET_DISPLAY_INFO` answered at boot and nothing else, which is a bug the
moment a QEMU window is resized or fullscreened: the host keeps the size it now
wants in the GPU's own device configuration (`events_read`, with
`VIRTIO_GPU_EVENT_DISPLAY` set and a config-change interrupt raised), and a guest
that never reads it goes on driving a 1024x768 scanout inside a 1920x1080 window.
What that looks like is a desktop confined to the top-left corner with a pointer
that cannot leave it - the report this fixes - because the window is showing the
guest's screen unscaled and the compositor's output *is* the guest's screen.

So the once-a-tick virtio service task now reads that one word
(`virtio::gpu::take_display_event`, an MMIO read, not a control-queue round trip:
`GET_DISPLAY_INFO` is only spent once the bit says there is a reason), and hands
the work to the render task, which owns the device. The task re-reads the size,
rebuilds the scanout resource at it (`change_resolution`), repins the new frames,
copies what was on the screen into the new one - the old block is still pinned
across the teardown, deliberately, so the pixels survive the swap - returns the
old block to the allocator (unless userland mapped the pixels, in which case it
stays pinned: a stale mapping that writes into recycled frames is worse than a
leaked screen), publishes the new geometry to `/dev/fb0` and the DRM card, and
rebuilds the console's grids to the new cell count.

And it tells userland, which is the half that makes the pointer work: a mode
change is announced as a **`change` uevent carrying `HOTPLUG=1`** on the card
device (`drm::display_changed`) - the DRM uapi has no hotplug event type, and
`libdrm`'s `drmHandleEvent` refuses a type it does not know - and a compositor's
udev monitor re-probes its connectors on exactly that word (weston's DRM backend
logs "[udev] HOTPLUG event"), finds the new mode from `GETCONNECTOR`/`GETCRTC`,
and re-modesets. `Event` gained the `extra` key list for those subsystem words,
which is where Linux's `envp` extras live too.

Two costs are real and named: a resize costs the console's scrollback (the grids
are rebuilt from the log ring, as at boot), and a drag's stream of window events
is coalesced by the render beat into one rebuild per frame rather than one per
event.

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
80x25 when there is no screen at all - a machine with no display, which is not the
same thing as a boot that merely keeps its log on the serial wire (§7.1c).

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

**Who may paint the screen: `VT_PROCESS` and a master that can be taken away.**
The symptom was specific and it was measured by hand rather than by a test: on a
`--mode weston` boot, switching to a text console and then *moving the mouse*
gave the compositor its screen back. Three absences produced that, and they were
all in the same place - nobody had ever asked the question "is this screen
yours?" on a compositor boot:

* `src/drivers/video/render.rs` tested `present_only ||` **before**
  `vcmux::painted()`, so on a `mpos.compositor` boot the veto existed and was
  never consulted.
* Every presentation request (`SETCRTC`, `PAGE_FLIP`, `MODE_ATOMIC`, a
  `SETPROPERTY` that moves a plane, `RMFB`) applied its pixels and *then* asked
  the render task to flush, unconditionally - so a refused flip would still have
  painted.
* `SET_MASTER`/`DROP_MASTER`/`AUTH` answered `0` and recorded nothing,
  `VT_SETMODE` accepted any struct without reading it, `VT_GETMODE` always
  reported `VT_AUTO`, and `VT_RELDISP` always agreed. A session therefore could
  not be told it had lost anything, and could not refuse.

The ABI is pinned in `src/vfs/vtmode.rs`, which holds no clock, no tasks and no
signals so that a host build can argue with the whole negotiation: `struct
vt_mode`'s eight bytes (`mode, waitv, relsig, acqsig, frsig`, padding
zero-and-read-zero on `VT_GETMODE`, which is a `_IOR` of exactly that size), the
`EINVAL` set `<linux/vt.h>` uses (unknown mode, `relsig == acqsig`, a signal
number that is not a signal), and the rule that a refused request is *settled*
rather than left pending - a refusal is modelled apart from a release precisely
so that an unrelated later switch cannot be forced through by a timer that is
counting a conversation nobody is having. Note that `VT_SETMODE` is
`_IOW('V', 2, struct vt_mode)` while every other request in the family is `_IO`:
a kernel that decoded the size from the group rather than the request would read
zero bytes of a caller that wrote eight, and `t-seat` now writes the `_IOW` form
for exactly that reason.

The mechanism is in `src/vfs/vcmux.rs`, and both halves of it were then measured
against `drivers/tty/vt/vt.c` and `drivers/tty/vt/vt_ioctl.c`, which moved them.

`^A <digit>` and `VT_ACTIVATE` share one decision
(`vtmode::Table::request_switch`) and **neither one waits**. Linux's `VT_ACTIVATE`
is `set_console()`: record `want_console`, schedule the console callback - which
is `change_console()`, and that is where the releasing console's `relsig` goes out -
and return 0. There is no `vt_release` sleep anywhere in the current console code,
and a waiting `VT_ACTIVATE` is not merely un-Linux-like, it deadlocks a seat
manager: `vt_switch` writes `VT_SETMODE(VT_PROCESS)` on the console it is standing
on and then asks for a different one, so the promise that owes the answer is the
asker's own, and the asker is parked inside the call its answer would unblock.
`VT_WAITACTIVE` is the one console ioctl that sleeps - `vt_event_waitqueue`,
interruptible, no timeout - and it still does.

Leaving a `VT_PROCESS` console sends its `relsig` - `SIGUSR1`/`SIGUSR2`, named in
`src/sched/signal.rs`, both fatal by default - **to the process that wrote
`VT_SETMODE`, and to nobody else**. That is Linux's `vc->vt_pid`: `vt_setmode`
stores `get_pid(task_pid(current))`, and `vt_acquire`/`change_console` deliver with
`kill_pid(vc->vt_pid, …)`. Aiming those two signals at the console's *foreground
group* was this kernel's earlier shape, and it is why
`cargo xtask run --mode weston` could not be run twice: seatd's `vt_open` is
`open /dev/ttyN`, `VT_SETMODE`, `KDSKBMODE`, `KDSETMODE`, `close(ttyfd)` - so the
group in the foreground when a handover signal went out was weston's (which is
what `ash` reporting `User defined signal 2`, i.e. `strsignal(12)`, was saying when
the desktop was being quit with `^C`) or the login shell's, and neither has a
`SIGUSR2` handler for a promise it never made. Two rules come out of the same
measurement and both are pinned in `vtmode`'s host tests: a promise is **not**
ended by the last close of the descriptor that wrote it - that close is the normal
case, and clearing there would unmake the claim in the breath it was made - and a
promise whose keeper has died ends where Linux ends it, at the delivery that cannot
land, running Linux's `reset_vc` (`VT_AUTO`, no signals named, and the console back
to `KD_TEXT` so the text that replaces it is actually visible) instead of waiting
on a ghost.

The deadline that bounds a *wedged* keeper - 15000 ticks, `RELEASE_TIMEOUT_TICKS` -
is this kernel's own, not Linux's: with no timeout and no answer, Linux's pending
switch simply sits until the session speaks or the box is rebooted. Keeping a bound,
plus the operator's second `^A` forcing the switch through, is what stops one stuck
compositor owning the machine. `frsig` is dropped on the way in, as Linux drops it
("the frsig is ignored, so we set it to 0"), so a refusal is heard only as the
requester's own `VT_ACTIVATE` return or a `VT_GETSTATE`; and a console number the
machine does not have is `ENXIO`, not `EINVAL`.

What that still leaves as named gaps: Linux holds a refcounted `struct pid`, so a
promise can never be inherited by an unrelated process later given the same
number, while this kernel stores the reusable task slot - closing that wants a
per-task generation count; `VT_SETMODE` here refuses `relsig == acqsig` and a
signal number outside 1..=31, both of which Linux accepts without looking; and the
only vector with an IST stack is `#DF` (see §11.1's last-resort entry), with no
NMI or machine-check handler installed at all.

The master is now a claim rather than a formality. It belongs to the *open file
description* (`File::ofd`), because that is the only identity that survives the
way a seat manager actually hands over a card - `SCM_RIGHTS` passes the
descriptor, and seatd's and weston's fds are one description - and it is bound to
the console its taker was on, read from the controlling tty at `SET_MASTER` time.
`AUTH` answers `EACCES` to a non-master and `EINVAL` to the master, which is
inverted from what one would write by instinct and is exactly what libdrm's
`drmIsMaster` reads. When the bound console stops being the one on the screen -
which is *only* ever measured as `holds_screen()`: master's console, on the
screen, in `KD_GRAPHICS` - the card drains its queued events and reports
`POLLHUP`, because a compositor parked in `poll` inside `drmHandleEvent` waiting
for a flip that will now never complete is not merely frozen, it is frozen with
the keyboard pointed at it. Releasing the master on the last close follows the
`EVIOCGRAB` precedent beside it, for the same reason: a killed compositor must
not hold a screen from beyond its own grave.

Two judgement calls are load-bearing and both came out of watching what broke
first. A master *not* bound to any console - a test program with no controlling
tty - is allowed to keep driving and is never allowed to veto the text console,
so `master_vc()` and `master_holds_screen()` are different questions answered
separately. And the render task now builds the six console grids on a compositor
boot too, because taking the veto away from `present_only` without giving the
text VT something to show replaces a stolen screen with a black one; `takeover()`
is still not called, so the kernel log and the `^A` keystroke keep riding the
serial wire and `--mode weston` keeps its promise about which window is which.
The grids are fed but not drawn until somebody holds the card, which is what
keeps the pre-seat window empty rather than filling it with a boot log the mode
says belongs to the compositor. Input follows the same fact from the other side:
`virtio/input.rs` delivers a grabbed node's events only while the grabbing
session holds the screen, and hands the keystrokes to the console when it does
not - the complement of a rule that used to be merely "grabbed means not the
console", which left a released VT with a keyboard that reached nobody.

Rejected: reordering `present_only` and `painted()` on its own, which fixes the
symptom by handing the loser the screen and the winner nothing; and refusing a
flip *after* committing it, which is what the ordering at the four presentation
sites would have made natural, and which paints the frame it just refused.

**Closing the remaining gaps to Linux.** The shape above was deliberately looser
than Linux in six places, and each is now the ABI it imitates rather than an
approximation of it:

* `/dev/tty0` is its own inode, `(4, 0)`, and every open of it resolves through
  `Inode::open_replacement` to the console actually on the screen - so `ttyname`
  on a descriptor opened that way says `tty3`, and a foreground-process-group
  rule has something to be a rule *about*. It used to be the console tty
  registered a second time, which made it `(4, 64)` and left the `(4, 0)` arm of
  `Tty::vc_number` unreachable.
* `VT_SETMODE`, `VT_RELDISP`, `VT_ACTIVATE`, `VT_WAITACTIVE`, `VT_WAITUNTIL`
  and `VT_OPENQRY` ask
  who is calling: the terminal's own foreground job, or superuser. Without that,
  any process holding a `/dev/tty` fd could answer a compositor's `relsig` for
  it, or write its mode. `KDSETMODE` keeps Linux's second door as well - the
  holder of the card may set a console's video mode, because the driver that owns
  the screen is what says what is drawn on it.
* `KDSKBMODE(K_OFF)` now swallows keystrokes, *for the virtual console only*. The
  first attempt at this dropped them in the console mux, which also carries the
  serial line, and produced a machine that could be locked out of its own shell;
  the mode belongs to the console's keyboard, and the serial wire is the
  machine's, so `virtio/input.rs` consults it and `vcmux::forward` does not, and
  a session's last tty close restores `K_XLATE` alongside the mode it already
  restored.
* `GET_MAGIC` mints a per-descriptor, one-shot token and `AUTH_MAGIC` is the
  master's to spend, which is how a compositor lets a client's descriptor
  page-flip without making it a master. A spent token is gone, an unknown one is
  `EACCES`, and closing a descriptor retires both its tokens and its
  authorisation - a token's whole purpose is to name a file description, and a
  freed one would name whatever lived at that address next.
* `SET_MASTER` and `DROP_MASTER` are `void`, which is not the shape one writes by
  instinct: taking the card *steals* it and ends the previous holder's claim,
  because `SET_MASTER` is also the recovery path for a compositor that lost its
  console to a `^A` and just won it back. A refusal there would leave the screen
  permanently promised to a session that no longer has it.
* `MODE_CREATE_LEASE` / `LIST_LESSES` / `GET_LEASE` / `REVOKE_LEASE` exist: a
  lease is the same card state behind a descriptor restricted to a list of object
  ids, only the master may mint one, a lease may not lease out what it was not
  given, and revoking empties it while leaving the descriptor open, which is what
  Linux's dead file private looks like from the holder's side. `IMMUTABLE` and
  `SEAL_ALL` are recorded and reported and *not* enforced - the honest
  half-measure, since sealing there refuses property changes this synthesized
  card does not have.

**Those four requests were numbered wrongly, and it is the one bug in this file
that no test in the repository could have caught.** They were written as
`0xd3`..`0xd6`; `<drm.h>` puts the family at `0xc6`..`0xc9`, and both copies of
the header on the development host - `/usr/include/drm/drm.h` and libdrm's -
agree. Thirteen too high means the request misses every arm of the dispatch
`match` and lands in the `_ => -25` catch-all, so a lease cost a real caller an
`ENOTTY`: libdrm's `drmModeCreateLease` sends `0xc6`, and it never reached
`create_lease` at all. The doc comment beside the constants said the numbers came
from `<drm_mode.h>`, which is where they do not live - ioctl numbers are
`<drm.h>`'s - and that is most of how it happened.

`t-drm-lease` was green throughout for a reason worth stating: it built its four
requests from *the same constants the driver matched on*, so the test and the
driver agreed about a number no userspace ever sends. A test whose expectation is
drawn from the value it is checking agrees with any value at all, which makes it
no test. The fix is therefore two-sided. The constants are `drm.h`'s, and the
test now compares each request word against a **literal from the header** -
`ioc_d(0xc6, 24)` and its three siblings - written out in the check rather than
read from the constant beside it, so the four disagree the moment either side
drifts. The other sixty-odd `NR_*` constants in the file were checked the same
way against the same header and all agree; this was the only family that did not.

And a flip-completion event is now queued *after* `request_flush_blocking()`
returns, so its timestamp is the frame's landing rather than the commit, and the
card fd gains a wake list (`poll_subscribe`) instead of relying on the tick
re-check `poll_select`'s comment lists it under. `FBIO_WAITFORVSYNC` waits for
that boundary with a 40 ms budget. It is called a landing everywhere in the code,
because there is no display controller here to be in blank; `WAIT_VBLANK` and its
sequence requests are still `ENOTTY` rather than an answer that would be a
clock this kernel does not have.

### 7.1b What the console answers, and what it deliberately does not

A terminal question (`ESC [ 6 n`, `ESC [ 5 n`, `ESC [ c`, an `OSC 10/11 ; ?`, a
`DCS 1 + q`) is answered by exactly one thing: the emulator that owns the screen
those bytes are drawn on. Which one that is, is a per-terminal question, and this
kernel used to answer it wrong in the one direction that is invisible until a
program is damaged by it.

The measurement that settled it, on Linux against a pty pair - slave in raw mode,
`ECHO` off, `VMIN=0/VTIME=1`, each of six sequences written to the slave and both
ends read: `ESC [ 6 n`, `ESC [ ? 6 n`, `ESC [ 5 n`, `ESC [ c`, `OSC 11 ; ? BEL`
and `DCS 1 + q … ST` each arrived at the master **byte for byte**, and **none** of
them was answered (6 of 6 runs, no queue at the slave within `VTIME`). A pty is a
wire; the emulator holding the master has the cursor, the window and the
capabilities. `drivers/tty/vt/vt.c` answers the same questions for a VT because
there the kernel *is* the emulator.

What this kernel did instead was answer everywhere, and `fish` showed the cost.
Measured on the guest (`--mode terminal`, `fish 4.8.1`, `TERM=linux`, root's shell
on the serial console): the shell's own reader log says
`Writing terminal command QueryCursorPosition` then `Received cursor position
report y: 24 x: 0` - the question was answered here, and never reached the wire,
because answering also swallows it. And `stty size` reported `25 80` while
`fish`'s prompt-spacing block padded the dangling line out to exactly the
*reported* column count: `stty cols 40` changed the block from 80 filler cells to
40, while the cursor report kept counting against 80. On a window wider than the
number the kernel guessed, the filler never reaches the margin, no wrap happens,
and the `CR` + `ESC [ K` that close the block erase the line the command printed -
which is why `echo -n hello` disappeared under the next prompt here and did not on
a real terminal, whose emulator knows its own width and answers truthfully.

So the rule is now `src/vfs/tty.rs`'s `Attached`, decided per write by
`Tty::kernel_draws`, and it is not "is this a virtual console" - all six consoles
are pty pairs too, and what distinguishes them is who reads the master:

| the bytes land on | who answers a question |
|---|---|
| a grid the kernel has made (`term::has_screen`, i.e. a framebuffer boot) | the kernel, and the question is kept off the wire |
| the serial wire alone, no grid (`--mode terminal`) | the person's terminal, reached by the forwarded question |
| a `/dev/pts` master a process holds | that process's emulator |
| `Discard` - a terminal with nothing behind it | the kernel, because silence there is a hang |

The second half of the same fix: the cursor a report answers with is now the
cursor the screen is drawn with. Both the discipline and `term::Grid` drive one
`crates/term/src/cursor.rs` - the window size, the scroll region, deferred autowrap,
line feed at the region's bottom, `DECSTBM`, save/restore (`ESC 7/8`, `CSI s/u`),
`ESC D/E/M`, the alternate screen - and
`vfs::tty::tests::the_report_and_the_screen_never_disagree_about_the_cursor`
feeds 37 streams through both and asserts one pair of numbers out of each. The
discipline's own `Screen` is gone: it was a 25x80 guess, pinned at row 24 forever
because it clamped instead of scrolling, blind to `TIOCSWINSZ`, and it did not see
the echo of what was typed at it (echo now goes through the same tracking).

What is answered is now exactly what `drivers/tty/vt/vt.c` answers, and the rest
is a gap named here rather than approximated:

* `csi_ECMA`'s `case 'c'`: `if (!vc->vc_par[0]) respond_ID(tty);`, and
  `respond_ID`'s entire answer is the VT102 id `"\033[?6c"`. So `ESC [ c` and
  `ESC [ 0 c` are answered, and nothing else in that family is.
* `csi_DEC`'s `case 'c'` is **not a question at all**: `CSI ? Ps c` is the console's
  *cursor-type set* (`vc_cursor_type = CUR_SIZE(par…)`). This is what the bug that
  found this section was made of: the `linux` terminfo the editors here read
  builds its cursor capabilities out of sets - `civis=\E[?25l\E[?1c`,
  `cnorm=\E[?25h\E[?0c`, `cvvis=\E[?25h\E[?8c` - and vim writes `civis` on every
  command line it paints and again as it leaves. Answering a set queues a reply no
  one asked and no one reads; it waits in the input queue as typed text for
  whoever reads next, so quitting vim left `9c9c` on the prompt and busybox tried
  to run it (`-sh: 9c9c: not found`). Measured with `strace` on the guest: two
  `write(1, "…\033[?1c…")` with no `read(0, …)` after them, and two answers of
  `\033[?62;1;2;6;7;8;9c` queued.
* `csi_ECMA`/`csi_DEC` both handle `case 'n'` with the same 5/6 test, so
  `ESC [ ? 6 n` is the same question as `ESC [ 6 n`. It was not treated as one:
  the swallow was measured off the raw parameter *bytes* (`params.first()` is
  `?`) while the answer was measured off the parsed ones, so the private form left
  unanswered - a caller waiting on a reply the console does send. `first_query_param`
  is now the one parse both halves read.
* `EPgt`/`EPeq`/`EPlt` sequences - which is `CSI > c`, the secondary Device
  Attributes request - reach neither handler, so the console does not answer them
  and neither does this one. It travels to the wire instead of being answered with
  an identity invented here.

Two gaps with names, not stubs: the cursor type a `CSI ? Ps c` asks for is not
drawn (there is no `vc_cursor_type` behind this console's cursor, which has one
state - `Grid::cursor_visible`), and a `--mode terminal` boot with a real terminal
on the far end therefore does not get the blinking or shape that sequence asks
for. Pinned on the host by `a_cursor_type_set_is_forwarded_and_never_answered`,
`a_device_attributes_query_is_answered_and_not_forwarded`,
`a_secondary_device_attributes_query_is_forwarded_not_answered`,
`a_query_to_a_terminal_the_kernel_does_not_draw_travels_unanswered` and
`a_query_to_a_screen_the_kernel_draws_is_answered_and_kept_off_the_wire`, and in
the guest by `t-tty-query`, which drives one end of a pty pair as the program and
reads the other as the emulator - so "answered and not forwarded" and "not
answered, and therefore forwarded" are two different observable queues rather than
a reading of the same one. Quitting vim on the built image, which is how this was
reported, now leaves the prompt empty; it used to leave `9c9c` and
`-sh: 9c9c: not found`.

Three more, each with the prerequisite it is waiting for:

* **The answering half cannot be pinned by a booted test.** `cargo xtask test`
  boots with no display, so no grid exists, so every question a booted program
  asks of a `/dev/pts` or a console pair is forwarded - which is the right answer
  for that boot and pins nothing about the case where the kernel draws the screen.
  `t-tty-query` covers the forwarding half end to end; the answering half rests on
  the host tests above until the gate grows a framebuffer boot.
* **A serial console's width is not knowable from the guest**, and Linux has no
  better answer for a `ttyS0` either: `stty cols 120` is the fix a person types,
  and a forwarding boot no longer needs it, because the terminal at the other end
  reports its own geometry now. What *was* wrong - guessing `80` and then also
  answering the question so that nothing could correct it - is fixed. A terminal
  that wants to say its size (`IAC SB WILLSYNCD?`-style negotiation, or `ESC [ 2048 ) 0`)
  is not implemented and is not pretended to be.
* **Cell counting for a non-ASCII glyph is unmeasured, so it is unfixed.** Both
  models count one byte as one cell, which is what `Grid` has always drawn and what
  the cursor inherits from it - consistent, and wrong for a UTF-8 terminal in
  exactly the way a `⏎` (3 bytes, 1 cell) shows up. Aligning it needs a
  measurement of wide/narrow cell advance on a real Linux console first; that, not
  a per-byte guess, is the number to copy. Same for `CSI S`/`T`, `L`/`M`, `@`/`*`,
  `P`/`X`, `DECSTR`, `DECCOLM` and origin mode (`DECOM`), which neither model
  implements: a sequence nobody acts on moves nobody's cursor, so the report and
  the screen still agree, and each stays a gap rather than a half-implementation.

* **The kernel's line editor is gone; `n_tty`'s is what remains.** The canonical
  mode used to keep a 64-line history for the up arrow, walk it with the arrow
  and `^P`/`^N`, offer `^A`/`^B`/`^E`/`^F`/`^K`/`^L`/`^T`, and recognise an
  `ESC [ A` as one key rather than as characters — all of it a *line editor's*
  job on Linux, where `n_tty` claims exactly ERASE/KILL/WERASE/REPRINT/LNEXT and
  types every other byte into the line, arrows' `ESC [ A` included. The shells
  and editors on this image (fish, vi) run the terminal raw and bring their own
  editing, so the kernel's editor served nobody and disagreed with `read(2)`
  about what a canonical line contains. Canonical mode now: unclaimed control
  bytes are characters (echoed as their `ECHOCTL` `^X` form, and `ESC [ A`
  reaches a canonical reader inside its line), kill/werase erase from the end
  with `\b \b`, and `^R` reprints the line without replaying a prompt the
  discipline never owned. Pinned in the host tests
  (`a_typed_escape_sequence_reaches_the_reader_with_the_line`,
  `unclaimed_control_bytes_are_typed_into_the_line`,
  `vreprint_redraws_the_pending_line`) and in the guest by
  `t-tty-query`'s cooked half.

---

### 7.1c Which consoles the kernel has — **DONE**, two gaps named

**The kernel used to be told how it was launched, which is not a fact about a
machine.** `cargo xtask run --mode terminal|gui|mirror|weston` picked one of four
words — `mpos.headless`, `mpos.nomirror`, `mpos.mirror`, `mpos.compositor` — and
the kernel re-derived its whole console policy from it: whether the framebuffer
console was *the* console, whether the serial wire also got the bytes, and whether
the GPU was engaged at all. The first three are one question in three costumes.
The fourth — "is there a window?" — is a fact about the *host*, and the machine
cannot discover it either: a QEMU booted with `-display none` still has a
virtio-gpu and still answers `GET_DISPLAY_INFO` with a mode (that is the note the
headless flag used to carry to justify itself). So the old shape had a word that
was neither a machine fact, like a device, nor a Linux one.

What replaced it is the parameter Linux has always used for exactly this:
`console=`. `src/boot/console_list.rs` reads it — allocation-free, because it runs
at the top of the boot task before the heap exists — and the rules are Linux's:

* every `console=` word names a console, and the log goes to all of them;
* the **last** one is preferred: its writes park until the hardware has taken them,
  where a peer sink is written best-effort, so a slow host terminal can never
  throttle the console somebody is reading;
* a value naming a console this machine does not have is reported and ignored, as
  Linux reports and ignores one.

Two names are accepted: `ttyS0`, the wire, and `tty0`, the virtual console on the
screen (Linux's spelling for "the console currently displayed"). The four run modes
became four lists — `console=ttyS0` for terminal and weston, `console=tty0` for gui,
`console=ttyS0 console=tty0` for mirror — so mirror stopped being a flag pair and
became an *order*, and terminal and weston became the same list, because what
differs between them (a compositor driving the scanout) is runtime, not boot-time.

**Four lists were one list too many, and three modes are the honest count.** That
last sentence is the reason the fourth word went away: a mode that tells the guest
exactly what another mode tells it was never a fourth machine. `weston`/`compositor`
is deleted, and `gui` — which had been the fourth shape, `console=tty0` with the
serial line sent to `/dev/null` — is the wire-alone list with a window on it. What
is left is the two questions a mode can actually answer: is there a window, and is
the framebuffer one of the guest's consoles. So `terminal` and `gui` share
`console=ttyS0` and differ only in `-display`; `gui` and `mirror` share the QEMU
wiring and differ only in one `console=tty0`. Every mode now puts the wire on stdio,
which deletes a branch rather than adding one: the mode that discarded the serial
line discarded the kernel log with it, and a log nobody can read is not a machine
anybody wants — that was the one mode whose wiring and console list could disagree
about the wire, and `run_modes_round_trip_and_name_their_console_list` used to spend
half its assertions policing that agreement.

`weston` is *not* kept as a synonym for `gui`, and that cost a shape change in the
config layer. `[qemu] mode` used to fall back to `mirror` when it could not parse
the word (so a `cargo xtask test` boot, which attaches no window, would not fail on
a typo it never uses) — a fallback that is now the wrong answer, because the retired
word names a machine whose consoles differ from the default's. `QemuConfig` therefore
stores the key as text (`mode_text`) and `run_mode()` resolves it at the two places
that boot a window: absent or blank means `mirror`, an unknown word is an error. The
image marker keeps its `build/console.mode` name and its old contents read as "no
marker": an image baked as `weston` costs one boot-file refresh to rename it, and the
`mpos.cmdline` it rewrites is already `gui`'s.

Records elsewhere in this file that say `--mode weston` are measurements of the boot
that name described, and are not rewritten. The reading of an old `--mode gui` record
now splits by what it describes: one that says the console is *on* the framebuffer —
line 308's "with the serial line discarded", the `KD_TEXT` console of line 228 — and
today's `gui` agree again (see the reversal record below); one that describes the
window left for a compositor and the log on the wire was the a0015ba interim shape,
and its machine is today's `--mode terminal` with a window, or `--mode mirror` for
the console-in-both-places.

**The rule the design rests on, and what was not measured.** "The log goes to
every console named, and the last is preferred" is `kernel-parameters.txt`. The
reading this kernel depends on is stronger: a console *not* named is not a console
at all. That is what makes `console=ttyS0` alone mean "the wire and nothing else",
and therefore what lets `--mode terminal` be a one-word list instead of needing a
headless flag. It is the rule in `register_console()`, which drops a console the
command line did not ask for once a preferred console has been chosen — **read out
of the console registration code, not measured on a machine here.** The half that
*was* measured has its own record above: `SET_MASTER`, `VT_PROCESS` and
`KDSETMODE` were measured against `vt.c`/`vt_ioctl.c` when the seat landed.

**The display device is engaged on every boot now.** A wire-only boot used to
`drop` the GPU and never hand it to the render task; that was the same flag doing a
second job. With the flag gone the task is spawned whenever the probe finds a
device, because `/dev/fb0` is a device a program may open and a session may claim
the card, whatever the kernel's own console list says. What the list decides is
only whether the kernel's *log* moves onto the screen.

**The grids are made whenever there is a display.** Six grids sized to the
scanout (160x50 cells on a 1280x800 window) are made when the render task starts,
in every mode, exactly as the compositor path always did; the console list decides
only whether the kernel's *log* moves onto them. The alternative was tried first
and rejected on measurement: make them lazily, on the first
`DRM_IOCTL_SET_MASTER`, since that is the moment the scanout can be handed to a
text console and therefore the moment a released VT has to be drawable — six grids
cheaper on a boot that never draws. `t-ioctl` caught what that costs: it failed
with `the console reports 25 rows (got 50, want 25)` and `80 columns (got 160,
want 80)`, because the tests that take DRM master (`t-seat`, `t-drm*`) run earlier
in the same suite, so the console's window *changed under it* mid-run. A console
whose geometry depends on who holds the card is neither Linux's answer — fbcon
binds to the display whatever the DRM master is doing — nor a stable one, and the
test was right to say so. What it pins now is the rule: the console's window is the
screen's grid in cells whenever there is a screen (checked against the
framebuffer's own `FBIOGET_VSCREENINFO` and the 8x16 font), and 25x80 only when
there is none. The cost is real and accepted: six grids on every boot that has a
display, including a `cargo xtask test` boot that never shows them, which is what a
machine whose display device is up pays.

**`cargo xtask test` on this change.** All milestone markers (`M0_OK` … `M8_OK`,
`All ready`), 131 user programs built, 114 `=== RUN`, 112 `=== PASS` - and the run
does **not** reach `=== SUMMARY`, because `t-tty-vtime` hangs and times it out (the
pre-existing red `AGENTS.md` records, unchanged by this work). One FAIL:
`t-stdresolve`, 19 of 20 live DNS rounds, one `baidu.com` lookup answering
`EAI_AGAIN` while this host was also running the build; every other network program
passed and it passed in the previous run, which is the shape `AGENTS.md` says to
read as load. `t-ioctl` passes on the new rule (the log shows `the console's rows
are the screen's`). The 14 programs after `t-tty-vtime` in name order never ran -
`t-dsp`, `t-udev-worker`, `t-uevent`, `t-umask`, `t-uname`, `t-unix`, `t-unix-dgram`,
`t-unix-scm`, `t-unlinkat`, `t-wait4`, `t-waitid`, `t-write`, `t-writev`, `t-xattr` -
which is a limit of this tree and not of this change: the hang is where the suite
stopped before it too.

**Two gaps with names, not stubs.**

* `console=tty1`..`console=tty6` — Linux's "pin the log to that VT" — is *refused*
  rather than approximated: this console writes to the VT that is on screen, which
  is `tty0`'s meaning, so answering the word while doing something else would be
  the private dialect the conventions refuse. The value is reported as unknown and
  the boot proceeds on the consoles it did name.
* `/dev/console` has no preferred-console plumbing: it follows the console
  currently on screen, and the list's preference decides only the write policy.
  Linux makes the last `console=` both the preferred console's device and the
  routing rule; only the second half exists here.

**`gui` moved back onto the framebuffer, with the wire kept — a deliberate
reversal, not the old `/dev/null` mistake.** The paragraph above folded `gui` onto
`console=ttyS0` (wire-alone list, window attached) and deleted the shape that put
the text console on the screen. A user asked for that shape back: the whole machine
in the window — boot log, virtual consoles, login prompt and shells all rendered
there, the host terminal quiet. So `gui` now bakes `console=tty0` (the parser always
supported the framebuffer-alone list; the note above that "no run mode bakes this
list any more" is reversed). What makes this *not* the discarded-and-deaf mode the
collapse removed is where the wire goes: not stdio (the terminal stays quiet) and not
`/dev/null` (the log survives). It is a launcher-side copy to `build/serial.log`,
because three writers reach the port whatever the guest's console list says — the UEFI
loader prints to COM1 before the kernel runs, the kernel banner prints before the
`console=` parse, and a panic forces the serial wire back. The mode may silence the
terminal; it may not make it deaf. `gui` and `mirror` therefore now differ in the
guest's list *and* in the host wiring, so `a_mode_word...retired`'s claim that three
words own three distinct lists holds again, and the QEMU test renamed to
`a_mode_switches_the_window_and_where_the_wire_is_read` pins that `gui` puts the wire
on a file and not on stdio. The retired `weston` boot-file refresh now rewrites the
line for real (`console=ttyS0` → `console=tty0`) rather than only renaming the marker.
**Not yet re-booted:** the `gui` and `mirror` windows were not re-driven after this
change; the in-window-console behaviour is read off the `render.rs`/`wire_bytes`
`vt_is_sink` gate and the getty-on-`tty1` wiring, the same caveat the a0015ba record
carries — a manual boot of `cargo xtask run` (now defaulting to `gui`) is what closes
it.

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
it (`crates/evdev`) instead of handing each client a buffer of its own the way
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

The policy sits in `crates/evdev` rather than in the driver, because the driver
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
record, and the self-test host sends no keystrokes. `crates/uinput/src/lib.rs` (the
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

### 7.2b `/dev/dsp`: the OSS parameters belong to the writer — **DONE**

The virtio-sound driver used to decide everything at boot — S16LE, two channels,
44.1 kHz "or the nearest", and a header comment that openly said no userland
existed to ask. On Linux those numbers are *negotiated per open*, through the
`'P'`-typed `SNDCTL_DSP_*` family in `linux/soundcard.h`, and a program that
cannot ask cannot fall back either. The family now answers on `/dev/dsp`:

* `SPEED`, `STEREO`, `CHANNELS` and `SETFMT` (and `GETFMTS`, `GETBLKSIZE`,
  `GETOSPACE`/`GETISPACE`, `RESET`/`SYNC`/`POST`) — with the rounding rule read
  off the header's semantics: a rate that is not on the device's list moves the
  stream to the *nearest one it has* — nearest by relative distance, which is
  what `44100 → 48000` and `10000 → 11025` agree on — and the ioctl answers what
  the stream settled on rather than what was asked (`abi::oss_round_rate`,
  host-tested). A real reconfiguration is a stop → `SET_PARAMS` → prepare →
  start cycle on the device itself, and a refused one restores the old
  parameters and answers `EINVAL`.
* The refusals are answers too: `SETFRAGMENT`/`SUBDIVIDE` (the buffer geometry
  is this driver's `SET_PARAMS` constant today) and `NONBLOCK` (the hand-off
  blocks in the device; a flag that promised otherwise would be the lie this
  kernel's rules put above the feature) each answer `EINVAL` by name.
* Pinned by the request words' `_IOC` recomputation (host, `abi::oss_tests`) and
  end-to-end in the guest (`t-dsp`, including `fstat` showing a `chmod` that
  stuck).
  **This bullet was untrue when first written and is true only now** — see §12.4a. The
  self-test machine never registered `/dev/dsp`, because the driver searched for the sound
  card at the wrong PCI ID, so `t-dsp` failed wholesale from its first `open`: this entry
  claimed an end-to-end guest pin for a node that did not exist. The host `_IOC` pin was
  always real; the guest half landed with the ID fix and the user-address DMA fix that
  followed it.

### 7.3 AHCI/SATA (stretch) — **DONE: the driver, the second disk, and what measuring it corrected**

**Why**: real hardware support beyond virtio. Every block device this kernel had
driven hung off a virtio transport, so the storage path was only ever exercised on
a machine that already has the paravirtualisation the kernel needs for everything
else. A SATA controller is the other half of that sentence: a device with its own
register file, its own DMA rules and its own failure modes, on hardware QEMU
presents with one `-device`.

**What it is not**: it is not on the compositor path. §7.4's list is about pixels
and none of it touches storage. It is here because the kernel's answer to "what is
a disk" was *one driver wide*, and a second driver is what turns that answer into
a claim rather than an accident. Deliberately out of scope: NCQ, ATAPI, port
multipliers, hotplug and 28-bit LBA. One controller, one port, `IDENTIFY`,
`READ DMA EXT`, `WRITE DMA EXT`, `FLUSH CACHE EXT`.

#### A disk has a *name* now, and that is the part to settle first

`crate::uevent::Numbering::of_boot_disk` is the one function the whole system
agrees on for what a block device is called: `/dev` builds its nodes from it,
`/sys/class/block/*/dev` prints it, `/proc/partitions` lists it, and a uevent
listener matches an event against it. It is hard-coded - `vda`, major 254
(`VIRTIO_BLK_MAJOR`), the boot disk's partitions - and every one of those names
hangs under a hard-coded devpath, `/devices/virtio/block/<name>`.

That is right while there is one disk and wrong the moment there are two. Linux's
answer is the one to copy rather than invent, because it is the answer userland
already reads: a SCSI/SATA disk is `sda`, major **8**, minor 0, partitions at
1.., and `/dev/vda` and `/dev/sda` are two devices no program treats as synonyms.
A kernel that called a SATA disk `vda` would be publishing a private dialect in
the one place §4.3 cares about most - a name.

| | virtio-blk | AHCI |
|---|---|---|
| name | `vda`, `vda1`… | `sda`, `sda1`… |
| major | 254 (`VIRTIO_BLK_MAJOR`) | 8 (`SCSI_DISK_MAJOR`) |
| minor | 0 for the disk, 1.. for partitions | the same rule |
| devpath | `/devices/virtio/block/vda3` | `/devices/ahci/block/sda` |

So the work has two halves and the smaller one comes first, which is §4 rule 7's
own advice ("the refactor lands first, green on its own, then the change"): the
numbering becomes the numbering of *the disks this machine has*, built from the
block devices that actually came up, with the devpath a fact carried per disk
rather than derived from the name. That refactor changes nothing while there is
one disk - and it is testable anyway, because `sysfs`'s and `procfs`'s host tests
already build `Numbering` literals by hand, so a *second* disk's rendering in
`/proc/partitions` and under `/sys/class/block` can be pinned with no hardware at
all.

#### The driver, mechanically

- **Probe by class code, not by vendor.** `crate::pci::find` matches vendor and
  device ids, which is what a virtio device has and what a SATA controller does
  not: the class is `01`/`06`/`01` - mass storage, SATA, AHCI 1.0 - and
  `Device::class_code()` already computes exactly that. Enumerating
  `pci::devices()` and matching the class is the whole of the discovery.
- **The register file is BAR5** (`ABAR`), a memory BAR, so it is *not* in the
  direct map: `mm::map_mmio` is the existing window for exactly this, the same
  call `virtio`'s `Hal` makes for its own BAR. `COMMAND_MEMORY_SPACE` and
  `COMMAND_BUS_MASTER` have to be turned on first, as `VirtioBlk::init` does.
- **Bring-up is the spec's reset sequence**: set `GHC.AE`, clear `GHC.HR`, wait
  for it to clear, read `CAP`/`PI`, and for the first implemented port whose
  `PxSSTS.DET` says "device present, PHY up" (3) run `PxCMD.ST` off → `FRE` off →
  set `PxCLB`/`PxFB` → `FRE` on → `ST` on. Every wait is bounded, because this
  runs on the boot path of a machine that may have no such disk: a controller that
  never answers has to end in `None` rather than in a `hlt`.
- **Four aligned allocations**, each its own frames: the command list (1 KiB,
  1 KiB-aligned), the received-FIS area (256 B, 256-byte-aligned), one command
  table whose command header is 128-byte-aligned, and a page-sized bounce buffer
  for the data - the same single physically-contiguous page the [`BlockDev`]
  contract already promises every caller, so this driver is not the one that has
  to copy twice. `mm::alloc_frames` + `mm::phys_to_virt` is the pair `HalImpl`
  already uses.
- **One command at a time.** `PxCI` takes one bit; the driver waits for it to
  clear and *then* reads `PxTFD`/`PxIS` for the error bits rather than treating
  the bit clearing as success. An ATA error reports itself in `PxTFD.ERR` with the
  command already retired, so a driver that checked only the bit would call a
  failed read a successful one - the shape §4.2 forbids.
- **`IDENTIFY` is the discovery** (`0xEC`, the device's 256 words read into the
  bounce page): the LBA48 capacity in words 100..103 becomes
  `capacity_sectors`, and a device that will not identify itself is a plain
  `None` - there is no honest way to present one.
- **`flush` is `FLUSH CACHE EXT`** (`0xEA`), answering `true` only when the
  command completed without an error, which is what `BlockDev::flush` asks of a
  device that has a cache.

#### The machine, and the pin

The self-test machine boots its ESP through virtio-blk and must keep doing so: the
point of the SATA disk is that there are *two* disks, not that the boot moved. So
the test boot gains an `ich9-ahci` controller with one raw disk image attached
(`-device ich9-ahci,id=ahci`, a `-drive …,if=none`, and an `ide-hd` on `ahci.0`),
which is a change in `xtask/src/qemu.rs`'s `Boot::Esp` arm plus a file for
`xtask/src/selftest.rs` to make - not `machine.toml`'s `extra_args`, which would
leave the wiring invisible to the tree the gate is run from.

`main.rs` tries virtio-blk first and AHCI second, so a machine with either boots
and a machine with both has both. The pin is a guest program that opens
`/dev/sda`, reads sector 0, writes a sector and reads it back, checks
`/proc/partitions` names it with major 8, and checks that the capacity `IDENTIFY`
reported is the capacity the reads can actually reach - a capacity that is
*reported* but not *readable* being the failure a wrong `PxCLB`/`PRDT` setup
really has.

#### One thing that is a choice rather than a gap

The controller's interrupt is not wired to anything, and `BlockDev` has no
interrupt-driven path: the driver polls `PxCI`. That is honest rather than
temporary - a uniprocessor kernel driving one command at a time has nothing to do
while it waits - and it is the same choice `virtio`'s synchronous paths already
make.

#### What was built, and the two things the machine corrected

`src/drivers/ahci.rs` is the driver: probe by class code, BAR5 through
`mm::map_mmio`, the spec's reset sequence, stop-the-port-then-point-it-at-our-memory
bring-up, four aligned DMA pages, one command at a time through slot 0, and
`IDENTIFY`/`READ DMA EXT`/`WRITE DMA EXT`/`FLUSH CACHE EXT`. The numbering
refactor it needed landed first (`c1e5932`), `main.rs` brings up virtio-blk and
then AHCI so a machine can have both, and the test machine gets an `ich9-ahci`
with a 4 MiB disk beside its ESP. `t-ahci` is the pin - 16 checks over
`/dev/sda` - because every line of the driver is a volatile access to hardware
and no host test can reach any of it.

**Two things the first working version got wrong, both found by booting it, and
both worth a line because they are the kind of mistake the design could not have
predicted.** The first is `PxSIG`. The design said "refuse anything that is not
the ATA signature `0x00000101`", which is what the spec's table suggests. The
measured controller reports **`0xffffffff` on the port that has the disk on it**
until that port has been brought up, so the gate rejected every port and the boot
printed no SATA disk at all. The signature is not the presence question -
`PxSSTS.DET` is, and it said `0x113` ("device present, PHY up") correctly all
along - so the driver now refuses exactly one thing, the ATAPI signature
`0xeb140101`, and lets `IDENTIFY` decide what a disk is. That is also the shape
Linux's `ata_dev_classify` has: ATAPI, SEMB, PM and ZAC are named, and everything
else is a disk.

The second is the **unpartitioned disk**, which the numbering refactor deliberately
left as it was and flagged (§7.3 said the driver would answer it). The old rule
gated a disk's whole name on its having a partition table, so an unpartitioned
disk had no `/dev/sda`, no line in `/proc/partitions` and nothing a program could
open - including the partitioner that would have put a table on it. The test
image is 4 MiB of zeros with no table, so this arrived immediately: `/dev/sda`
did not exist and `t-ahci` failed at its first check. The rule is now the one
Linux has - the whole disk is numbered whatever is on it - with a host test in
`uevent` for the empty-table case, and that is why the numbering carries the
disk's own size rather than deriving a partition's.

#### The third thing the machine corrected: what the driver costs a boot

`cargo xtask run` stopped printing anything after `virtio-blk: vda, ...` for
**28.4 s** before the next line, which reads as a hang and is not one - the boot
completes. Measured on a copy of the image with the same wiring and a
timestamped serial log: the gap sits between `virtio-blk: vda, 4194304 sectors`
(`main.rs:374`) and `blk: no data partition on this disk` (`main.rs:418`), which
is `drivers::ahci::init()`'s window. Removing the device with
`-machine q35,sata=off` makes the same two lines **2 ms** apart, so the cost is
the SATA probe, and the QEMU monitor's `info pci` names the device: a plain
`-machine q35` already carries an ICH9 SATA controller at `00:1f.2`
(`8086:2922`) with nothing attached to it.

What the driver does with that controller, read out of an instrumented boot:

```
ahci-probe: 00:1f.2 bar=0x81084000/0x1000 GHC=0x80000000 CAP_PI=0x3f
ahci-probe:   port 0 SSTS=0x0    port 1 SSTS=0x0    port 2 SSTS=0x113  <-- DET == 3
ahci-wait: PxCI retire FAILED after 7067 ticks          = 28.3 s
ahci-dead: TFD=0x441 CI=0x1 IS=0x40000001 SIG=0xeb140101 SSTS=0x113 CMD=0xc017
```

Port 2 answers `PxSSTS.DET == 3` - "device present, PHY up" - with no device
behind it, the driver brings it up, issues `IDENTIFY`, and **`PxCI` never
retires**. It did not need to be waited on: the same read shows `PxIS.TFES` set
(bit 30 of `0x40000001`) and `PxTFD.ERR` set (bit 0 of `0x441`), i.e. the port
answered "that command failed" on the first read and the wait was looking only at
the retirement bit, which is what `identify`'s own error check - at
`drivers/ahci.rs:274`, *after* the wait - would have seen immediately. The
`PxSIG` had even become `0xeb140101`, the one signature this driver says it
cannot drive; it is read before bring-up, when the same port still reads
`0xffffffff`.

So two faults, one of them about the number:

1. **The wait asked one register a question two registers answer.** Fixed in
   `crates/ahci` (the `mpos-ahci` crate; `drivers/ahci.rs` is behind
   `#[cfg(target_os = "none")]`, so a decision inside it has no reachable test -
   the `pci`/`drivers::pci` and `evdev`/`drivers::evdev` split again):
   `PortStatus::waiting` stops on *either* answer, retired or refused. Five host
   tests carry the measured words above, and one of them - the phantom port -
   fails with the old `PxCI`-only rule.
2. **`WAIT_TRIES = 5_000_000` was a count nobody had costed.** Its comment claimed
   "tens of milliseconds ... on a dead one"; measured, one volatile MMIO read of
   an emulated register under KVM is ~5.7 µs, so five million of them is 28.3 s.
   The count shape stays, and the reason is now written beside it rather than
   assumed: the identical wait runs while a mounted filesystem reads through
   `BlockDev`, which holds the port inside `critical_section::with`, so
   interrupts are masked and the 250 Hz tick counter does not advance - a
   deadline in ticks there would never expire, while reads always do. What a
   count buys therefore depends on what a read costs, on this machine ~1000×
   more than on silicon, which is the honest weakness of the shape and is the
   reason the budget is no longer the only bound.

Measured after, same wiring on an independent copy of the image: **28.412 s → 0.001 s**, and
`t-ahci === PASS` on a machine that does have a disk, where a healthy command
retires at try 0-2 (also measured), so the patience that is left is not being
spent. **Why no test ever caught this:** `xtask/src/qemu.rs:168` gives the
selftest machine an `ich9-ahci` *with* a disk at a lower BDF than the chipset's
own, and the controller loop returns the first disk it finds - so the built-in
empty port is never probed there, while `cargo xtask run` boots a machine whose
only SATA controller *is* the empty one. A machine shape no test machine has.

**The two gaps the first version of this fix left, both closed in the same
investigation, both measured:**

- **A request longer than one page panicked the driver.** `udevd` asking a new
  SATA disk what is on it reaches `AhciDisk::read_at` with **262144 bytes**, and
  a 6144-byte unaligned superblock probe reaches it with **6656**; both ended the
  machine on `check_span`'s assert, because the block node's read hands the
  driver the *whole* covering window of the caller's request
  (`vfs/devfs.rs::DevBlockNode::read_at`) and virtio-blk has always accepted any
  sector-aligned length. The one-page promise was this driver's own, never
  `BlockDev`'s, and §4.3 says whose limit it therefore is: the driver's.
  `AhciPort::issue` now builds **one PRDT entry per bounce page** - a command
  carries `BOUNCE_PAGES` = 32 pages, 128 KiB, and a longer request is more
  commands, planned by `crate::ahci::chunks` - so `PRDTL` says how many runs the
  device fills and the total goes in `PRDBC`. `check_span` keeps only what is
  still a contract, the sector boundary on both offset and length, which is the
  pair virtio-blk asserts too. The pin is `t-ahci`: a 16 KiB write and read
  (four runs) and then **one `pread` of 1 MiB** - eight commands, 32 runs each -
  checked against a pattern that is a function of the byte's *index*, because the
  failure this exists to catch is a table whose completion bit is on every
  rather than only the last descriptor: that one reads back perfect with three
  quarters of the buffer unwritten, and a uniform pattern would not notice. The
  `DBC` encoding and the plan are host-tested in `crate::ahci`, including the run
  a 22-bit count cannot say (refused, not truncated).
- **A bring-up that failed kept its memory.** `release()` gives back the command
  list, the FIS area, the command table and the bounce, and it calls `stop()`
  first: a controller still running its engine reads a list that has gone back to
  the allocator. This is not a tidy-up on a hypothetical machine - the `q35`
  controller at `00:1f.2` walks exactly this path on every boot of `cargo xtask
  run`, so the phantom port used to cost 35 pages per boot, and would have cost
  35 more for each dead port a bigger controller has.

**And one claim this section made about the same scratch boots, retracted.** A
qcow2 overlay with `build/mpos-uefi-x86_64.img` as its backing file stopped
printing after `mpos: handing the machine to /init`, and it did so with the
pre-fix driver and with `-machine q35,sata=off` as well, which looked like a
userspace stall worth naming as a separate item. It is not one: **the image's own
guest was running while the overlay read it.** The run wiring opens its disk with
`file.locking=off`, so nothing stopped a second QEMU from taking a live
filesystem as its backing file, and what that boot froze on was another machine
writing underneath it. The same image copied to an independent qcow2 with
`qemu-img convert` and booted on the same wiring with the same kernel runs the
whole way: `Seeding 4096 bits and crediting`, `Saving 4096 bits of creditable
seed for next boot`, `[ ok ]`, no `[ !! ]`, and no `EOPNOTSUPP` anywhere on the
wire. Kept rather than deleted for two reasons: a scratch overlay of an image that
is *also* booted is not a look at that machine but a fight between two of them,
and the bisect that ruled the block layer out (`sata=off`, old driver, same
freeze) is exactly what made the wrong conclusion look earned. A later clean
check - `-machine q35,sata=off` on a freshly converted copy - reaches
`mpos login:`, which is the sentence that ends it.

**What the failed port is allowed to take with it.** `release()` gives a dead
port's pages back only after `stop()` has seen the engine and the FIS receiver
come down. That conditional is not caution for its own sake: a port whose `CR`
never clears still holds `PxCLB` and `PxFB`, so freeing those frames hands the
buddy system a page the *device is writing to*, and the corruption surfaces as a
bad page-cache block or a process's altered data far from here, with no line
pointing back. The measured case stops cleanly (`PxCMD` quiet, `PxCI` empty), so
the leak is the branch and not the rule; the rule is that the branch must not
free. An abandoned attempt to force the point - `SCTL.DI`, re-initialising device
detection to make a stuck port let go - was written, measured as never needed on
this machine, and removed again rather than kept as untested code.

**Gated**: host `629 passed`, `mpos-xtask 36 passed`, guest
`=== SUMMARY pass=132 fail=0 (132 built)` with QEMU exit 33, and `t-ahci === PASS`
over its wide transfers. End to end, on two fresh `qemu-img convert` copies of
the shipped image booted on the run machine's own wiring:

- the shape `cargo xtask run` boots, whose only SATA controller is the empty
  built-in one - `Seeding 4096 bits and crediting`, `Saving 4096 bits of
  creditable seed for next boot`, `mpos login:`, no `[ !! ]` and no
  `Unable to seed` on the wire;
- the same with a real 64 MiB `ide-hd` on an added `ich9-ahci` - `ahci: sda`,
  then the same seedrng lines and the same login prompt, and **no panic** where
  `udevd`'s 262144-byte probe used to end the machine.

One loose end, recorded rather than tidied: a single boot of the empty-controller
shape stopped after `Seeding 4096 bits and crediting` and printed nothing more for
45 s. It has not reproduced since - four later boots of that shape, and two of the
disk-attached shape, all reached the login prompt - so it is an unreproduced
observation and not a diagnosis, and the `release()` rule above is a safety
measure argued from what a stuck engine owns, not a fix claimed for it. If it is
seen again, the thing to read first is whether that port's `PxCMD.CR` came down.

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

#### What niri needs, and the two things that still stop it

**Where it stopped first, measured.** niri 26.04, booted in the guest, does not
reach the renderer at all - it dies in the TTY backend's setup with

    thread 'main' panicked at src/main.rs:186:6:
    called `Result::unwrap()` on an `Err` value: error initializing the TTY backend
    Caused by:
        0: error opening the primary GPU DRM node
        1: the provided file descriptor does not refer to a DRM node

which is `DrmNode::from_path("/dev/dri/card0")` failing its *identity* check.
niri's own helper had already found the card through eudev, and the node itself
was fine - the right path, an `st_rdev` of 226:0 - because the `drm` crate on
Linux asks sysfs a second question:

    let path = format!("/sys/dev/char/{}:{}/device/drm", major(dev), minor(dev));
    stat(path.as_str()).is_ok()

That is: the node's parent GPU, and the parent's `drm/` directory, whose entries
are the node *names* of one GPU. This tree had everything about the card except
that chain - the node, its class entry, the number's index - so a perfectly good
card was refused. It now has it (`src/vfs/sysfs.rs`: each node's `device` link,
and the parent's `drm/{card0,renderD128}`), which is also the directory
`node_with_type(NodeType::Render)` reads to hand niri `/dev/dri/renderD128` for
`NIRI_PRIMARY_GPU`-free startup. Worth knowing when reading a compositor log:
libdrm's test for the same question is weaker - `fstat` plus a look for
`/dev/dri/card*` - which is why weston never noticed the difference.

**Where it stopped second, measured the same way.** A second boot of niri 26.04,
on a kernel carrying that chain, stopped on the same line of the same panic. The
tree was right - `/sys/dev/char/226:0`, its `device` link, the parent's
`drm/{card0,renderD128}` listing - and the card was still refused as "not a DRM
node", because resolving that path is not one lookup: `226:0` is a link to
`../../devices/virtual/drm/card0`, whose `device` is a link to `../../virtio-gpu`,
and the second link's two `..` have to reach the directory that names `card0` -
a node four levels down. The walk answered `..` by *searching* the tree for the
parent (`Vfs::parent_of`), a walk bounded at 512 directories that returns nothing
when the bound is reached. On the guest's root filesystem - 483 directories at
depth four or less, before the pseudo-filesystems are counted - that budget is
spent before the search gets that deep, so the `..` stood still, `virtio-gpu` was
looked up inside `card0`, and the answer was `ENOENT`. Measured by the host test
that now pins it: the same chain resolves with 480 directories under the root and
fails at 500. The hole was that a splice threw the walk's chain away (`base =
here`) and re-derived it by searching; it now keeps it, and the search is left
where it belongs - a `..` whose base is a cwd, whose ancestors nothing has
walked. niri has not been booted since *that* fix either, so what it meets next
is again read off its source rather than measured, and it is the renderer below.

> **Update, measured (2026-10-01): it is not the renderer next, and the two
> paragraphs after this one are not where it stops.** Rebooted with the sysfs
> chain in place, niri gets past the DRM-node identity check and reaches
> `niri::backend::tty: using as the render node: "/dev/dri/renderD128"` - and
> stops there, *before* any EGL device is created, so no
> `software EGL renderers are skipped` line is ever printed and the process does
> not exit with an error. It stays alive in state `R` with **zero voluntary
> context switches**: a tight loop, never a wait. What it loops on is the Wayland
> display socket's lock file - the same unit forever on the same name:
>
> ```text
> stat("/run/user/0/wayland-1.lock", ...) = 0
> open("/run/user/0/wayland-1.lock", O_RDWR|O_CREAT|O_TRUNC|O_CLOEXEC, 0660) = 22
> fcntl(22, F_SETFD, FD_CLOEXEC) = 0
> close(21) = 0
> flock(22, LOCK_EX|LOCK_NB) = 0
> fstat(22, {st_mode=S_IFREG|0640, st_size=0, ...}) = 0
> ```
>
> `/run/user/0` holds exactly one file, `wayland-1.lock`, zero bytes, with no
> `wayland-2.lock` and no socket: the name never advances, so this is not a scan
> for a free `wayland-N` but a **lock-and-verify retry that never converges**,
> and the verify is the `fstat`/`stat` pair. Two measurements say the loop is the
> caller's and not libwayland's: the open carries `O_TRUNC` and is followed by an
> explicit `fcntl(F_SETFD)`, which libwayland's `wl_socket_lock` does not do
> (checked against `wayland-server.c` 1.26.0 *and* by disassembling the guest's
> own `libwayland-server.so.0.26.0`); and **niri links no libwayland at all** -
> `readelf -d` lists `libinput`, `libseat`, `libgbm`, `libudev`, `libpixman`,
> `libxkbcommon` and `libc`, and no `libwayland-*` - so it uses
> `wayland-backend`'s pure-Rust server, which is where the loop lives.
>
> **The bug it was spinning on is this kernel's, and it is fixed.** `stat(path)`
> and `fstat(fd)` disagreed about `st_dev` for one and the same file: measured by
> `t-stat-consistency` on `/tmp/t-stat-consistency.lock`, `fstat` answered
> `dev=0 ino=669` where `stat` answered `dev=1 ino=669`. The inode agreed; the
> device did not. `stat` looks the filesystem up by path, and a descriptor cannot
> - and an inode does not name its own filesystem, so the fallback
> (`fs_id_of_inode`) can only match a mount by its *root* inode, which a plain
> file inside the mount is not. `File` therefore records the filesystem id at
> `open`, where the path is known, and `fstat` prefers it; both doors now answer
> the same pair. Pinned by `user/abitest/src/bin/syscall/t-stat-consistency.rs`
> (the two doors, the `O_CREAT|O_TRUNC|O_RDWR` order a lock protocol uses, and
> both `/tmp` and `/run/user/0`).
>
> **The three further defects the diagnosis raised are now settled - one not a
> defect, one fixed, one re-attributed.** First, `/proc/<pid>/fd` readdir does
> *not* skip descriptors: built to the question, `t-fd-list` opens a contiguous
> run of twelve descriptors and finds every one of them in the listing, along
> with the three standard ones. The compositor's `0, 1, 17, 18` was a process
> that had opened and closed `2..=16`, which is what a compositor's startup
> looks like - so the listing is a pin now and the observation is withdrawn.
> Second, the kernel panic **is** a defect and is fixed: `pf_diag` prints the
> faulting instruction's own bytes, and it read them at `rip` *without asking
> whether that page was mapped* - and on an instruction-fetch fault `rip` is
> the faulting address itself, so the read faulted again from ring 0, at the
> same `CR2` with the U/S bit now clear, and the machine died of
> `KERNEL PANIC: #PF` while describing a *user* fault. The read is guarded by
> the same `leaf_pte` question the rest of the diagnostic asks, and the message
> now says when the bytes are not there to print. Measured before and after by
> `t-segv`, which jumps into a `PROT_NONE` page: the run that used to panic now
> prints `PFDIAG ... bytes=[] (empty: the page holding rip is the absent one)`,
> the child dies of `SIGSEGV`, and the parent - and so the machine - runs on.
> That program also faults the common way (a store through a null pointer), so
> a fix that refused to read *any* instruction bytes would fail it. Third, the
> non-root `ENOSYS` is not a kernel gap: the guest's `libseat.so.1` contains no
> `syscall` instruction at all (every call goes through libc), and its backends
> are `noop`, `seatd` and `logind` - there is no `builtin` - so "Function not
> implemented" is libseat's **noop** backend, the one it falls back to when no
> seat daemon answers. The image's own configuration is right (`seatd` is in
> `/etc/runlevels/default`, runs `-g seat`, and `mpos` is in `seat` in
> `/etc/group`), so the next step is to watch the daemon rather than the kernel:
> `rc-service seatd status`, `ls -l /run/seatd.sock` and `id mpos` on a boot
> where the non-root run fails.
>
> What has *not* changed is the conclusion two paragraphs below: a non-software
> EGL device still means `virtio-gpu` 3D, and `DRM_VIRTGPU_*` is still the
> largest single item in this section. What the fix above buys is that niri gets
> past its socket setup to the place that check lives, instead of spinning
> forever before it. **And that item is reachable on this host**, which was
> worth checking before calling it out of scope: QEMU here (11.1.1) carries the
> 3D device under a *different name* - `virtio-gpu-gl-pci`, with `virtio-vga-gl`
> and a rutabaga/gfxstream variant beside it - and
> `qemu-system-x86_64 -display egl-headless,gl=on -device virtio-gpu-gl-pci`
> starts, so the host can present a virgl-capable GPU. The `virtio-gpu-pci` the
> machine boots today has no `virgl` property at all, so 3D is a *machine* change
> plus a kernel one: `machine.toml` would name the GL device and a GL display
> backend, and the kernel would grow the `DRM_VIRTGPU_*` family and the 3D
> command queue behind it. That is a feature rather than a defect - it is why it
> is not in the fix above - but it is not blocked by the host.

> **Update, measured (2026-10-01, the same boot one call further on): the stop
> after the lock file was `listen(fd, -1)`, and this kernel answered `EINVAL`
> where Linux answers 0.** With the `stat`/`fstat` pair agreeing, the
> lock-and-verify loop converged, the name was taken, and niri died one line
> later - still `src/niri.rs:2435`, now on the socket's own error:
>
> ```text
> thread 'main' panicked at src/niri.rs:2435:67:
> called `Result::unwrap()` on an `Err` value: Io(Os { code: 22, kind: InvalidInput, ... })
> ```
>
> That line is `ListeningSocketSource::new_auto().unwrap()`, the `Err` is a
> `BindError::Io`, and the failing call is inside wayland-server 0.31's
> `ListeningSocket::bind_absolute`, which after the lock loop does exactly three
> things: `stat` the socket path, `UnixListener::bind`, `set_nonblocking`. Rust's
> std spells the bind as `socket(AF_UNIX, SOCK_STREAM)` + `bind(fd, addr, 2 +
> strlen + 1)` - 24 bytes for `/run/user/0/wayland-1`, not the
> `sizeof(sockaddr_un)` of 110 a C caller passes - and then `listen(fd, -1)`,
> because on Linux it is the *kernel* that caps a backlog. Measured in the guest
> with a twenty-line C probe making the same calls: `bind(..., addrlen 24)` ok,
> `listen(..., -1)` **`EINVAL`**, `listen(..., 0)`, `(..., 128)` and `(..., 4096)`
> all ok. So the short `addrlen` was not the problem and the negative backlog is
> the whole of it.
>
> `clamp_backlog` refused a negative backlog on purpose ("negative is `EINVAL`"),
> and that reading of the rule is wrong in the one place that matters: Linux's
> `__sys_listen` compares the argument to `SOMAXCONN` as an *unsigned* int, so
> every negative lands on the ceiling instead of being refused. Measured on this
> host before writing it: `listen(fd, -1)` returns 0 for `AF_UNIX` and `AF_INET`,
> and `ss -lx`/`ss -ltn` then report the backlog as 4096 - `net.core.somaxconn`,
> not the zero a "negative means a closed queue" reading would leave. Nor is it a
> corner case anyone had to go looking for: `-1` is what Rust's std passes from
> `UnixListener::bind` *and* `TcpListener::bind`. The rule lives once now
> (`src/net/unix.rs::clamp_backlog`) and the IPv4 pool reads it too - that arm had
> read `-1` as **zero**, a queue one deep, the same defect in the other family's
> clothes. §4.1 carries the rule; `t-unix` and `t-tcp-listen` pin it, each shown
> red on the kernel built before it.
>
> **Where niri stops now, measured the same way.** Rebuilt and booted with the
> fix, `ListeningSocketSource::new_auto()` returns and the two lines that were
> never reached appear:
>
> ```text
> INFO niri: listening on Wayland socket: wayland-1
> INFO niri: IPC listening on: /run/user/0/niri.wayland-1.11.sock
> ```
>
> `/run/user/0` holds `wayland-1` as a real socket (`srwxr-xr-x`, plus its lock
> file and that IPC socket) where the boot before left a zero-byte lock and
> nothing else. The next stop is a *different* call, one line above those in the
> log:
>
> ```text
> WARN niri::backend::tty: error adding primary node device, display-only devices
> may not work: DRM access error: Error loading resource handles on device
> `Some("/dev/dri/card0")` (Bad address (os error 14))
> ```
>
> `EFAULT` from the primary node's resource-handle load, and niri is gone within
> the twenty seconds the probe watched (the renderer refusal below is the rest of
> that story).
>
> **That call was this kernel's, and it is fixed.** `Error loading resource
> handles` is smithay's wrapper around drm-rs's `resource_handles()`, which is
> two calls to `DRM_IOCTL_MODE_GETRESOURCES`: a *sizing* call whose pointers are
> all null and counts all zero, then a *fill* call whose pointers come from the
> `Vec`s reserved at the counts the first call reported (`drm-ffi`'s
> `get_resources`). An empty `Vec`'s `as_mut_ptr()` is Rust's dangling non-null
> pointer - address 4 for `u32` - so the fill call carries `count_fbs = 0` beside
> a non-null `fb_id_ptr`, and this kernel read that pointer as "somewhere the
> caller asked me to write" and answered `EFAULT`. The card has no framebuffers
> before its first commit, so `count_fbs` is 0 on *every* first enumeration:
> nothing about the card was wrong, the call was.
>
> Linux's rule, measured on this host first with a C probe that makes both calls:
> the sizing call answers 0 with the counts (`fbs=0 crtcs=4 conns=8 encs=13` on
> the host's card), and the fill call answers 0 as well - with `fb_id_ptr = 4`
> and `count_fbs = 0` - because its copy sits inside the count test, so a
> zero-length array is never dereferenced. `get_resources`' two arms skip an
> empty array the same way now (`!fbs.is_empty()`, `!ids.is_empty()`), and the
> *poisoned pointer* rule is untouched: a count that asks for a real id at an
> address that is not the caller's is still `EFAULT`, which is what the
> `t-drm-args` check beside it pins from the other side.
>
> Pinned by `t-drm`, whose own enumeration does the two-call idiom against a card
> that has no framebuffer yet - the only state in which this can be observed at
> all, which is why `t-drm-args` cannot carry it (by the time that program runs,
> `t-drm` has created one, so its `count_fbs` is 1 and the arm never fires). The
> kernel built before the fix prints `FAIL: the fill call succeeds with an empty
> array beside a dangling pointer (got -14, want 0)`; the one after it passes.
>
> **The call after that one is the property list, and this time the failure was
> in the caller.** With the primary node added, niri died one line later - no log
> message, a panic in drm-rs itself:
>
> ```text
> thread 'main' (28) panicked at .../drm-0.14.1/src/control/mod.rs:497:29:
> index out of bounds: the len is 0 but the index is 0
> ```
>
> That line is `match values[0]`, which drm-rs evaluates as soon as a property's
> flags carry `DRM_MODE_PROP_OBJECT` - it reads the property's single value to
> learn *what kind* of object the property names. Linux gives every `OBJECT`
> property exactly one such value (`drm_property_create_object` stores the type
> tag), and this kernel had the planes' `FB_ID` and `CRTC_ID` advertising an
> empty list, with the connector's `CRTC_ID` carrying a lone `0` - a value drm-rs
> maps to `ValueType::Unknown` rather than to `CRTC`. So the properties were
> never *wrong* enough to fail a caller's check; they were wrong enough to make
> one index off the end of its own array.
>
> Measured through libdrm on this host's card before writing it: the connector's
> `CRTC_ID` and both planes' `CRTC_ID` answer `count_values = 1`,
> `values[0] = 0xcccccccc` (`DRM_MODE_OBJECT_CRTC`), and a plane's `FB_ID`
> `0xfbfbfbfb` (`DRM_MODE_OBJECT_FB`) - `0x40` (`OBJECT`) in the flags of all
> three, with `0x80000000` (`ATOMIC`) beside it. The three properties now carry
> their tag, and `OBJECT_FB` joins the three tags the file already had
> (`DRM_MODE_OBJECT_*` out of `<drm_mode.h>`, all four now matching it).
>
> Pinned by `t-drm-props`, which asks each of the four by name and checks flags,
> count and value. Counterfactual, measured in a worktree at the same commit:
> the kernel built before the fix fails it, the one after passes.
>
> **Named, not fixed: the `ATOMIC` flag.** Those properties are
> `ATOMIC | OBJECT` on Linux, and Linux *hides* them from a client that has not
> set `DRM_CLIENT_CAP_ATOMIC`; this kernel accepts the cap and stores nothing, so
> the flag is absent and the properties are shown to every client. Closing it
> honestly means the cap becoming per-file state and the property lists filtered
> by it - a change nothing yet asks for (niri sets the cap), so it is written
> down here rather than half-done.
>
> One instrument note for whoever takes it: `strace -f niri` ended *two* earlier
> boots in `KERNEL PANIC: #GP` from ring 0 (err `0x0`, `rip 0xffffffff8004b01f`,
> with `0xffffffff8013aede` at `rsp+0x0c0`), while `strace /bin/true` in this
> boot exited 0. So it is not "any ptrace on this kernel" - it is something a
> traced compositor reaches, and it is a defect of this kernel and not of niri: a
> user process must not be able to panic the machine. Not diagnosed here;
> recorded with the two boots that saw it. The same boot also logs
> `error spawning "waybar": Os { code: 12, kind: OutOfMemory }`, which is not
> what a missing binary answers (`ENOENT`) and is worth a look of its own.

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

> **Update, measured (2026-10-02): that item landed, and what stops eglinfo is
> not it.** The 3D half is answered — `VIRGL` and `CONTEXT_INIT` negotiated, the
> `CTX_*`, `RESOURCE_CREATE_3D`, `SUBMIT_3D`, `TRANSFER_*_3D` and `GET_CAPSET*`
> commands dispatched, the cap sets cached at bring-up, `EXECBUFFER` and
> `TRANSFER_FROM_HOST` made real — and on the GL machine the driver states the
> result in its own words:
>
> ```text
> virtio-gpu: virgl=1 context_init=1 capsets=2 cached=2 mask=0x6
> ```
>
> Every number there is the one the Linux measurement below produced, and
> `cached=2` is the one that carries weight: it is both the `GET_CAPSET_INFO` and
> the `GET_CAPSET` round trip completing. It is not enough to produce a renderer,
> and the reason is a *different* hole, measured the same way on
> `virtio-gpu-gl-pci` with `-display egl-headless,gl=on`:
>
> - **every EGL platform eglinfo can initialise reports software.** `eglinfo -p
>   surfaceless` and the device and GBM platforms answer `OpenGL renderer string:
>   llvmpipe (LLVM 23.1.2, 128 bits)` — the software rasteriser — on a card with
>   3D behind it.
> - **Mesa's loader says what it wanted, and did not get it.** It prints
>   `MESA-LOADER: failed to retrieve device information` once per lookup, and the
>   message list inside the shipped `libEGL.so.1` names the two other ways out:
>   `MESA-LOADER: device is not located on the PCI bus`, and `failed to get
>   driver name for fd %d` / `DRI2: failed to get driver name`.
> - **The card's parent device answers nothing.** `/sys/dev/char/226:0/device`
>   resolves to `/sys/devices/virtual/virtio-gpu`, and on it `vendor`, `device`,
>   `subsystem`, `driver`, `modalias` and `uevent` are *each* `ENOENT`; the
>   directory holds one entry, `drm/`. The chain below the node is right —
>   `/sys/dev/char/226:0/device/drm/` lists `card0 -> ../../drm/card0` and
>   `renderD128 -> ../../drm/renderD128`, each `dev` reading back `226:0` /
>   `226:128`, and `drmGetNodeTypeFromFd` answers `PRIMARY` for `card0` and
>   `RENDER` for `renderD128`. The *minor* is modelled; the *device* is not.
> - **libdrm agrees, and this is the sharpest number.** A twenty-line `gd` probe
>   against `libdrm.so.2` (2.134.0) answers `drmGetDevices2(0, devs, 16) = 0` —
>   it enumerates *no* device — where Linux returns the card; `kmscube` says the
>   same thing in words, `no drm device found!` then `could not open drm device`;
>   and `drmGetVersion(fd)` names the driver **`mpos`**, where Linux names it
>   `virtio_gpu`.
>
> **Two interpositions say what the fix is not.** Interposing `drmGetVersion` to
> return `virtio_gpu` (confirmed applied: the probe then prints it), and
> interposing `open`/`openat` to hand `device/vendor` and `device/device` the
> values Linux's card reports (`0x1af4` / `0x1050`), each changed *nothing* — the
> renderer stayed llvmpipe, and the loader kept refusing. So Mesa is not choosing
> on the version name, and not on the PCI ids alone: it is refusing a device
> whose parent chain has no bus identity at all — no `subsystem` link above all,
> which is what `devices/virtual/...` literally means. The direction of the
> remaining work is therefore the device model rather than the command queue:
> the card has to hang off a device that says what it is, as Linux's sits three
> levels down (`.../0000:00:01.0/virtio0/drm/card0`, with the PCI function's
> `vendor`/`device` and the virtio device's `subsystem`). Until that exists
> `is_software()` is true, no renderer eglinfo can report is anything but
> llvmpipe, and this section's first bar is unreachable however complete the 3D
> family is.

> **Update, measured (2026-10-02, the same day, one instrument later): the gate
> is libdrm's device information, not the sysfs files the loader reads
> directly.** The design the block above argued — the card needs a bus identity
> in sysfs — was tested rather than built, by a recorder that logs every `/sys`
> path a caller opens or resolves (`LD_PRELOAD` over `open`/`openat`/
> `readlink`; the alternative, `strace -f`, is the instrument this file already
> records panicking the machine). It says something narrower, and it says it
> twice:
>
> - **on the guest** the loader asks exactly three questions about the render
>   node, in this order — `open /sys/dev/char/226:128/device/vendor`,
>   `open …/device/device`, `readlink …/device/subsystem` — and all three
>   `ENOENT`;
> - **on this host**, whose Mesa is the same 26.2.3 and whose libdrm is the same
>   2.134.0, a working card (amdgpu) is asked **only for the `subsystem` link**:
>   `readlink /sys/dev/char/226:128/device/subsystem -> ../../../../bus/pci`.
>   The loader never reads `vendor` or `device` at all.
>
> So the guest is not missing a file the loader wants for its own sake; it is on
> a *fallback* path, taken because libdrm's `drmGetDevice2` — which is where the
> PCI id comes from on a working system — could not describe the card. That
> reading fits the library's own imports (`libgallium` pulls `drmGetVersion`,
> `drmGetDevice2`, `drmGetDevices2` and `drmGetNodeTypeFromFd` from libdrm; with
> udev built in, `drmGetDevice2` answers out of the *udev* database rather than
> by reading sysfs at run time) and fits the number this section already has:
> **`drmGetDevices2` returns 0**.
>
> Serving the three questions through the same preload settles it as a test.
> With `device/vendor` reading `0x1af4`, `device/device` `0x1050` and
> `device/subsystem` resolving to `/sys/bus/pci`, the renderer is **still
> llvmpipe**, and the loader asks nothing further under `/sys`. Two candidate
> gates are therefore ruled out by measurement instead of argument: the kernel
> driver *name* (`drmGetVersion` forced to `virtio_gpu`, confirmed applied — no
> change) and the per-node sysfs identity (served — no change). What is left is
> the layer underneath both: what `udev` was told about the card.
>
> **The design is re-opened on that, and the next measurement is named.** Dump
> the guest's udev database entry for `c226:0` and set it beside Linux's for the
> same card, then find the property libdrm's udev path needs and does not get.
> `DEVTYPE=drm_minor` is absent from the node's `uevent` today (measured: it
> carries `ACTION`, `DEVPATH`, `SUBSYSTEM`, `DEVNAME`, `MAJOR`, `MINOR` and
> nothing else), and `ID_PATH` and the `PCI_*` keys a udev rule would add have
> never been looked at. Nothing in the device model changes until that dump is
> in hand: the sysfs chain the block above proposed would have been built to
> satisfy a caller that is not the one failing.
>
> Two instrument notes, both machine defects, recorded because they cost this
> measurement three reboots. The guest's console — tty1, carried over the wire
> by the kernel's console mux, since `/etc/inittab` puts no getty on ttyS0 —
> **stops accepting input and stops emitting output after a session of heavy
> commands**, with the machine still running and `info status` still answering.
> And the 9p share is exported to a bus the guest enumerates — `0000:00:09.0` is
> there — while the boot prints `virtio-9p: no share on the bus`, so there is no
> file channel into the guest to work around the first defect with.

> **Update, measured and then built (2026-10-02, the same night): the chain the
> caller reads, and what this tree now answers.** The first recorder was
> incomplete, and that is why its answer was too: libdrm opens sysfs through
> `fopen64`, `open64`, `opendir` and `stat64`, and the first version hooked only
> `open`/`openat`/`readlink` - so *libdrm's own reads were invisible to it*, and
> what it showed was Mesa's fallback rather than the parse that fails. With
> those four doors covered, the same recorder run against a working card on this
> host gives the target, in this order:
>
> ```text
> opendir   /dev/dri
> stat64    /dev/dri/{card1,renderD128}
> open      /dev/dri/renderD128
> open64    /sys/dev/char/226:128/device/vendor
> open64    /sys/dev/char/226:128/device/device
> stat64    /sys/dev/char/226:128/device/drm
> readlink  /sys/dev/char/226:128/device/subsystem -> ../../../../bus/pci
> fopen64   /sys/dev/char/226:128/uevent
> fopen64   /sys/devices/pci0000:00/.../0000:65:00.0/{vendor,device,
>           subsystem_vendor,subsystem_device,uevent}
> ```
>
> The guest answers the first four and stops: `device/vendor`, `device/device`
> and `device/subsystem` are `ENOENT` there, because the node's `device` link
> lands on `/sys/devices/virtual/virtio-gpu` - a directory holding one child
> (`drm/`) and no attributes at all.
>
> **Built, and pinned by a host test that fails on the old tree**: the card now
> hangs off the display-class PCI function, so `device` leads to a directory
> that carries `vendor`, `device`, `subsystem_vendor`, `subsystem_device` and
> `uevent`; `add_pci_attributes` gained the `subsystem` link it never had (which
> is what libdrm resolves *before* it parses anything else, and gives up when it
> is missing); and the parent's `drm/` listing was moved with it. That last part
> had a bug worth recording: the listing's entries climb from the *parent*, one
> level deeper than the node's own, and leaving them at the old depth made them
> dangle - a listing whose entries resolve to nothing, which a reader walking
> `/sys` (udev does, at coldplug) follows into nothing. The test now resolves
> them, not merely lists them.
>
> **The section's first bar is still open, and this is the part not to paper
> over**: `eglinfo` has not been re-run on the GL card with this kernel. Every
> attempt died on the console defect above - a multi-kilobyte line is truncated
> mid-send, at 8 bytes every 60 ms as much as at 16 every 20 - so the run that
> would answer "non-software renderer" has not happened. What *is* measured: the
> tree answers every question the working card's log asks, a host test pins it,
> and the gate is green on a machine with a display-class GPU
> (`pass=140 fail=0`, exit 33).
>
> **And one more machine defect, found by bisection rather than assumed**:
> `udev failed to start` on this machine, and it is **not** this change - the
> same image built with the change stashed fails identically, and the gate never
> sees it because the gate boots an initramfs and not Alpine. It is its own
> named gap, and it is now the reason a *stock* boot reports no udev at all.

> **Update, measured (2026-10-02): the card is found now, and Mesa says where it
> still stops.** The identity chain was built and the acceptance was run - not
> over the console, which cannot be driven (below), but from a script in
> `/etc/local.d` that runs the probes at boot and leaves their output on the
> disk, where the host reads it with `qemu-nbd` and a mount. That technique is
> the instrument; the files were temporary and are not in the tree.
>
> With the chain in place, on `virtio-gpu-gl-pci`:
>
> ```text
> devpath=/sys/devices/pci0000:00/0000:00:04.0
> PCI_ID=1AF4:1050  PCI_SUBSYS_ID=1AF4:1100  PCI_SLOT_NAME=0000:00:04.0
> subsys=../../../bus/pci
> drmlist=card0 renderD128
> ```
>
> - **`kmscube` no longer says `no drm device found!`.** It initialises EGL 1.5,
>   prints its EGL and GL information, and is still running when a 25 s timeout
>   kills it (`rc=143`). The device is found, opened and driven, so that part of
>   Phase E's second bar moved.
> - **The renderer is still `llvmpipe`, and Mesa names the next gap itself**:
>   `MESA-EGL: warning: egl: failed to create dri2 screen`, then
>   `MESA-EGL: warning: DRI2: failed to create screen`, on the GBM platform and
>   on surfaceless. It says the same with `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`
>   forced - so the driver *is* reached and its **screen creation** is what
>   fails. That is the item now: not the identity, not the command family, but
>   whatever the virtio_gpu driver's screen creation asks for and does not get.
> - **A bug the first version of the chain had, worth the paragraph**: the card
>   hung off the *first* display-class function, and a q35 machine has the
>   firmware's own VGA (`1234:1111`) before the `virtio-gpu` it was given - same
>   class, so class alone chose the VGA. Mesa read it and said so:
>   `pci id for fd 3: 1234:1111, driver (null)`, the id of a display adapter it
>   has no driver for. The choice is now the function a driver bound to (the VGA
>   has none), pinned by a fixture that puts the VGA first - and **the honest
>   shape is the driver telling sysfs where its own device is rather than sysfs
>   guessing from class and driver**, which is owed.
> - **`DRM_IOCTL_VERSION` now answers the driver's name, `virtio_gpu`**, where it
>   answered `"mpos"`. Measured *not* to be the gate - with the name corrected
>   the renderer does not move - but landed anyway: Linux's `drm_version.name`
>   is `drm_dev->driver->name`, Mesa keys its kmsro table on it (`libgallium`
>   carries `virtio_gpu` beside `kms_swrast`), and a private spelling in a field
>   a caller identifies the driver by is the shape §4.3 forbids. `t-drm` carries
>   the new expectation.
>
> **And the console defect, sharpened by measurement.** It is not length: after
> the *first* command the receiver is dead. `^A 2` - the mux's own switch, which
> narrates `[tty2]` on the wire when it works - produces nothing at all, so the
> bytes never reach the mux; the forwarder is not stuck
> (`poll_schedule_timeout`, its normal select); and the machine is idle, not
> looping (`HLT=1`, `RIP` in kernel space). A machine that cannot be typed at
> after one command cannot be measured through, which is why the boot-time
> instrument above exists.

#### Measured: what Linux's card answers, request by request

The family is written from an observation rather than from the UAPI header,
because the part worth knowing is not in the header at all: **which requests a
card with no 3D refuses, and with which errno.** The instrument and the full
table live in `user/staging/virtgpu-probe/` - Alpine's netboot `virt` kernel in
a twenty-line initramfs, booted on `virtio-gpu-pci` and then on
`virtio-gpu-gl-pci` + `-display egl-headless,gl=on`. (`virt` and not `lts` for a
measured reason: the `virt` netboot initramfs already carries `virtio-gpu.ko`,
`drm.ko`, `drm_kms_helper.ko` and a `modules.dep` whose closure covers them, so
the 218 MB `modloop-lts` is not needed.) Linux 6.12.110-0-virt, QEMU 11.1.1.

Four things decide the shape, and each is a way a plausible design is wrong:

- **Nothing in the family is `ENOTTY`.** `virtio_gpu_ioctls[]` is registered
  unconditionally, so every request is *answered* even on a card with no 3D -
  and they are answered differently. On `virtio-gpu-pci`: `GETPARAM(1)`
  (3D_FEATURES) is `0`; `GET_CAPS`, `EXECBUFFER` and `TRANSFER_FROM_HOST` are
  `ENOSYS`; `CONTEXT_INIT`, `RESOURCE_CREATE_BLOB` and a 3D `RESOURCE_CREATE`
  are `EINVAL`; while `TRANSFER_TO_HOST`, `WAIT` and `MAP` **succeed**. One
  errno for the whole family would send a caller down a path it cannot recover
  from, which is §4's wrong-shape failure in its plainest form.
- **`CONTEXT_INIT` answers `EEXIST` on a virgl card**, because the driver
  creates a context *implicitly* on first use, so an explicit request arrives
  second. `GETPARAM(7)` reports the capset mask `6` (VIRGL and VIRGL2), and the
  device announces two cap sets: id 1 max-version 1 size 308, id 2 max-version
  2 size 1408.
- **`RESOURCE_CREATE`'s `target` is the switch.** Without 3D only `2` is
  accepted, with `depth`, `nr_samples`, `last_level` and `array_size` each
  capped at 1 (`EINVAL` otherwise); `4` is `EINVAL` there and succeeds with
  virgl.
- **`RESOURCE_INFO` answers `size = 4096`** for a 64x64 B8G8R8A8 resource
  created with `size = 0` - the driver's floor is a page, not `w * h * 4`.

What the instrument does **not** do is capture *Mesa's* own sequence: that wants
a full Alpine userland with `virtio_gpu_dri.so` and a tracer, not the twenty-line
initramfs, so "which request the winsys makes in which order" is still a source
reading rather than an observation here. The directory's README says so, so it
cannot be mistaken for measured.

#### The two decisions the kernel work starts from

With the measurement in hand the kernel work has two forks already settled, and
both are written down because each rejects an option that looks cheaper.

**The driver becomes this kernel's, not the crate's.** `DRM_VIRTGPU_*` is
carried by the virtio-gpu *control queue*, and that queue lives inside
`virtio_drivers::device::gpu::VirtIOGpu` - whose command set stops at `GET_EDID`,
which negotiates no `VIRGL` feature, and which exposes no accessor for the queue.
So the family cannot be added from outside the crate. The two ways in are (a)
vendor `virtio-drivers` 0.13.0 into `vendor/` with a `[patch.crates-io]`, keeping
the 2D path bit-identical, or (b) own the GPU driver in
`src/drivers/virtio/gpu_ctrl.rs` on the crate's *public*
`VirtQueue`/`Hal`/`Transport`, keeping the crate for every other device. **(b) is
the choice**, and the reason is measured rather than aesthetic: the kernel calls
exactly **four** things of that crate's GPU device - `new`, `resolution()`,
`change_resolution()` and `flush()` - and uses neither its EDID parsing (413
lines the DRM layer replaces with its own synthesized blob) nor its virtio cursor
queue (the DRM cursor is a real second plane, `drm_cursor.rs`). Vendoring ~13,000
lines to add ~400 would make every future crate bump a merge, while (b) puts the
new ABI facts in a host-testable module, which is where §4.6 wants them. What
(b) puts at risk is a working, pinned 2D path - and the pins (`t-fbdev`, `t-drm`,
the boot's own display probe) are what guard it.

**(b) has landed**, as the refactor §7's rule wants landed first: the same
commands in the same order, so the machine boots exactly as it did and the pins
above are the evidence rather than an assertion. `src/drivers/virtio/gpu_ctrl.rs`
owns the transport and the control queue; `crates/gpu/src/abi.rs` holds the wire
vocabulary and is host-compiled, with eight tests pinning the command numbers
against `<linux/virtio_gpu.h>`'s literals, every struct size, the header's field
offsets and the feature word. Three things narrowed deliberately rather than
carried over, each because the crate accepted something it never used: `EDID` is
no longer negotiated (the 413-line parser behind it was never called), the
cursor queue is no longer configured (nothing sends cursor commands - the DRM
cursor is a plane), and the feature word is now `WANTED_FEATURES` rather than
"everything the crate listed". The gate after it: `=== SUMMARY pass=137 fail=0
(137 built)`, QEMU exit 33 - the same line the tree printed before the change,
which is what a behaviour-preserving refactor owes.

What that leaves for the 3D work is additive: the commands, and the bits that
gate them.

**A 3D submission reaches the device through the render task.** The render task
takes the device out of `DEVICE` at its start and is its sole owner; that is what
makes `flush` single-caller, as `video/render.rs`'s own header says. A
compositor's `EXECBUFFER` runs on another task and so cannot touch it. The
pattern to reuse already exists and is the same shape: `request_flush_blocking`
is a request counter the task polls plus a parked-waiter list, and the 3D path is
that handshake generalized into a mailbox. The rejected alternative - handing the
device to the ioctl path under a shared lock - would make `flush` multi-caller
and put a control-queue round trip on the synchronous syscall path while the task
may be mid-flush.

**And the family cannot be half-served**, which is why this section is a plan and
not a landed change. The measurement makes it explicit: Linux's no-3D card
*succeeds* at `RESOURCE_CREATE` (target 2), `MAP`, `WAIT` and `TRANSFER_TO_HOST`,
and answers `ENOENT` only for handles it genuinely does not have. This card has
no device resources at all, so serving the refusals alone would hand `ENOENT`
back for handles a Linux card accepts and refuse a `RESOURCE_CREATE` Linux
completes - a *nearly right* family, which §4 says is worse than an absent one.
The family therefore lands whole, on top of the control queue, or it does not
land.

Two smaller blockers used to sit behind it, and **the first is no longer one:
threads are done** (`clone(CLONE_THREAD)`, §6.4 - a task on the caller's memory
and descriptors, with `CLONE_CHILD_CLEARTID` and a real futex wake for
`pthread_join`). That matters here precisely because a real compositor spawns
them: `tracy_client`'s profiler thread, `async-io` under the D-Bus feature, and
Mesa's own worker pools (`LP_NUM_THREADS`; one CPU makes llvmpipe's default
single-threaded, but the path exists, and Vulkan ICDs use it unconditionally).
**Explicit sync is the one that remains**: Smithay will
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
`MODE_GETFB2` ✅; `CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` ✅ (refused, with the
reason); `MODE_CURSOR`/`MODE_CURSOR2` and `MODE_GETGAMMA`/`MODE_SETGAMMA` (the
legacy pair niri falls back to) - both ✅, the gamma pair refused with the reason
and the cursor pair driving a real second plane; the
`SYNCOBJ_*` family (`CREATE`, `DESTROY`, `HANDLE_TO_FD`, `FD_TO_HANDLE`, `WAIT`,
`SIGNAL`, `RESET`, `QUERY`, `TRANSFER`, `TIMELINE_WAIT`, `TIMELINE_SIGNAL`,
`EVENTFD`) with `DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE`; and
`DRM_VIRTGPU_*`.

**Four of those requests are answered now, and three of the four answers are
refusals - deliberately.** `MODE_GETFB2` (0xce) is a real implementation:
`struct drm_mode_fb_cmd2` goes in as an `fb_id` and comes back carrying the
geometry, the fourcc, the modifier flag and the four plane arrays, which is the
one thing libdrm's `drmModeGetFB2` exists for that `GETFB` cannot say. Two
details there are the same lesson this section already records twice. First, a
legacy `ADDFB` names its layout as bits per pixel plus depth because its struct
predates fourcc codes, so `GETFB2` on such a framebuffer reports a format the
card *derived*; `legacy_fb_format` derives it exactly the way
`drm_mode_legacy_fb_format` does for the two depths this card can scan out (24 ->
`XRGB8888`, 32 -> `ARGB8888`), which are the two `ADDFB2` already maps, so the
two doors report the same fourcc for the same pixels rather than two dialects of
one fact. Second, the three planes this card does not have are written as
*zeros*, not skipped: libdrm reads `handles`/`pitches`/`offsets`/`modifier` as
four-element arrays whose meaningful length is "how many are non-zero", so a
kernel that left them alone would describe a four-plane buffer over addresses the
caller never provided. `t-drm` pins the round trip, both spellings, and the zeroed
planes.

`MODE_GETGAMMA`/`MODE_SETGAMMA` (0xa4/0xa5) are refused `EINVAL`, and that is the
same decision as the `GAMMA_LUT`/`GAMMA_LUT_SIZE` absence above rather than a
separate one: there is no colour ramp to read or write. A caller is told the shape
before it asks, because `GETCRTC` now writes `gamma_size` 0 - it wrote nothing
there at all before, which left a `malloc`ed struct's garbage in a field libdrm
reads, the identical failure the mode-name comment in `write_mode` describes.
`EINVAL` is Linux's answer for a CRTC whose `gamma_size` is zero. **It is the one
number in this file taken from Linux's source rather than measured on a card, and
it is recorded as such:** no machine here has a CRTC without a gamma table to
measure - the amdgpu CRTC next door reports `GAMMA_LUT_SIZE` 4096 - and in the
3.7 tree `drm_mode_setgamma` answers `-EINVAL` to a `gamma_size` the CRTC cannot
hold and `-ENOSYS` only to a *missing callback*, a distinction a CRTC with no
table never reaches. A refusal rather than an accepted no-op, because `OK` from
`MODE_SETGAMMA` is a compositor believing it applied a colour transformation no
pixel will ever show.

`CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` (0x3b/0x3c) are refused `ENOTTY`, which
is §7.1's recorded answer for the other vblank door - `WAIT_VBLANK` and its
sequence facility - and the same argument decides both: both requests are about a
*clock*, and this card's only boundary is a frame it presented.
`drivers::video::render::landings` states the constraint in its own comment,
"Calling it a vblank counter would be a lie the ABI cannot tell", and `st.seq`
counts *commits* rather than periodic boundaries, so there is no truthful number
to put in the reply. Naming the two in the dispatch `match` is what makes the
answer this card's rather than the catch-all's; two spellings of "I cannot tell
you about vblanks" that disagreed would be the private dialect `AGENTS.md` §4.3
forbids.

All four also gained the argument check that `arg_shape` exists to perform, and
that is the only *behaviour* that changes for the two refused pairs: Linux's ioctl
layer copies every DRM struct in before a handler can refuse it, so a caller whose
pointer is not its own memory now hears `EFAULT` where before these answered
`ENOTTY` without ever looking at the struct. `t-drm-args` pins that per request,
beside the errnos.

**One sentence earlier in this section is now known to be wrong.** It says
`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is "the one thing that *is* missing without
a hardware reason". Measured with `modetest -p` on the same amdgpu card: each
CRTC has four planes, the cursor planes report `formats: AR24` and `gamma size 0`,
and their **only** properties are `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` -
there is no `HOTSPOT_X` and no `HOTSPOT_Y` anywhere in the machine. So a real card
with four real cursor planes refuses that cap's premise too, which makes it a
driver opt-in rather than the compositor-facing family the sentence implied - the
same finding, and the same correction, as `FB_DAMAGE_CLIPS` and `OUT_FENCE_PTR`
further down that table.

**The cursor is a plane now, and the cap is still refused for the reason the
measurement gives.** The card has a second plane whose `type` is Cursor, listed by
`GETPLANERESOURCES` and described by `GETPLANE`; the `plane_id` a `GETPLANE`
carries finally selects *which* plane answers, where it used to be ignored
entirely and every question was answered with the primary plane - a bug on its own,
and a blocker for a second plane, since a compositor finds its cursor plane by
asking about each id and reading `type` back. `MODE_CURSOR` and `MODE_CURSOR2` are
the legacy door to the same plane; they are in the `PRESENTATION` list with the
other requests that put pixels on the screen, because a pointer that keeps moving
after the master loses its console is pixels.

`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` stays refused, and the measurement is why:
`modetest -p` reports `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` as the *only*
properties on any of the real card's four cursor planes, with no `HOTSPOT_X` or
`HOTSPOT_Y` anywhere in the machine. Having a cursor plane is a different question
from publishing hotspot properties, and the hotspot itself is not lost - it is in
`MODE_CURSOR2`'s struct, which this card honours, and a compositor that needs it
subtracts it from the position it asks for. So the cap is refused because there is
no property for it to be about, which is exactly what a real card's cursor planes
measure as.

**The composition is the one thing here that is arithmetic, so it is the one thing
in its own host-tested module.** There is no display controller to composite an
overlay, so the cursor is blended into the scanout block by the same pass that fills
it: `crates/gpu/src/cursor.rs` holds that blend - `ARGB8888` over `BGRA`, premultiplied,
hotspot-pinned, clipped at all four edges in one intersection computed up front -
with ten `cargo test` cases beside it, following `drm_blob.rs` and `evdev.rs`. The
driver calls it at the end of every `copy_to_scanout`, which is what makes the two
orderings come out right without either side knowing about the other: a
compositor's flip lands *under* the pointer rather than erasing it, and a
`MODE_CURSOR` move re-copies and re-blends, so the pointer moves without the
compositor redrawing anything. The `ARGB8888` in that sentence is the format the
fourcc correction above had to land first.

**Two gaps in this area are named rather than filled.** The cursor plane publishes
`type`, `FB_ID`, `CRTC_ID`, `CRTC_X`, `CRTC_Y` and its own `IN_FORMATS` - no
`SRC_*`/`CRTC_W`/`CRTC_H`, because this card's cursor has no scaler and no crop and
a published-then-ignored property is the silent success `AGENTS.md` §4 forbids. A
compositor that wants to crop or scale a cursor will not find the properties and
will do it itself, which is the honest outcome but is narrower than a real card. And
an unknown CRTC in `MODE_CURSOR` is `EINVAL`, which is what this file has always
answered for an object it cannot use, where Linux's `drm_crtc_find` says `ENOENT`;
the whole file would have to move together for that to change, so the cursor follows
the file rather than splitting it. The cursor plane work is where the cap's answer belongs,
and it had this measurement to build on rather than a guess.

Properties and caps, cheap one at a time and load-bearing in aggregate: `EDID`
(a blob - with non-zero `mm_width`/`mm_height` and a `subconnector`, if anything
is ever to show a monitor's name), `non-desktop`, `max bpc`,
`panel orientation`, `vrr_capable`/`VRR_ENABLED` with
`DRM_CAP_ASYNC_PAGE_FLIP` beside them, `GAMMA_LUT`/`GAMMA_LUT_SIZE`,
`FB_DAMAGE_CLIPS` (the blob that lets a compositor say *what* changed instead of
sending whole frames), `OUT_FENCE_PTR`, `IN_FORMATS` with
`DRM_CAP_ADDFB2_MODIFIERS`, and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` - refused
today, and asked for precisely by the virtualized drivers.

**The list above was written from the sources, and measuring it corrected three
of its entries.** `modetest -c -p` against the real card on the development host
(AMD `amdgpu`, DRM interface 3.64.0, Linux 7.2.7) is the instrument, and what it
reports is the shape each property really has:

| property | object | flags | shape as measured |
|---|---|---|---|
| `EDID` | connector | immutable blob | 128 bytes + extension blocks |
| `DPMS` | connector | enum | `On=0 Standby=1 Suspend=2 Off=3` |
| `link-status` | connector | enum | `Good=0 Bad=1` |
| `non-desktop` | connector | **immutable range** | `values 0 1` - a range, *not* the bool the sources suggest |
| `max bpc` | connector | range | `values 8 16`, value 16 |
| `vrr_capable` | **connector** | immutable range | `values 0 1`; every DP/eDP output carries it, with value 0 on the ones whose sink is not VRR |
| `subconnector` | connector | immutable enum | `Unknown=0 VGA=1 DVI-D=3 DP=10 HDMI=11 Native=15 Wireless=18`, and only on connectors that have one (the eDP output has none) |
| `VRR_ENABLED` | **CRTC** | range | `values 0 1` |
| `GAMMA_LUT` / `GAMMA_LUT_SIZE` | **CRTC** | blob / immutable range | size reports `values 0 4294967295`, value 4096 |
| `IN_FORMATS` | plane | immutable blob | |
| `FB_DAMAGE_CLIPS` | plane | - | **absent on a real card**, so it is a driver opt-in rather than a family a compositor can rely on |
| `OUT_FENCE_PTR` | plane | - | **absent on a real card** for the same reason |

Two consequences for this plan, and both change what "add the property" means.
`non-desktop` and `vrr_capable` are **ranges with two values**, so a card that
answered them as a `PROP_BOOL` would give a reader a shape it does not expect -
and `vrr_capable` belongs on the connector while `VRR_ENABLED` belongs on the
CRTC, which the sources' prose does not make obvious. And the last two rows are
the reminder the whole section is written around: a property family a
*compositor* looks up by name is one the driver may legitimately not have, so
the honest answer for this card is absence plus the `drm_info`-visible reason -
not a blob property invented to make the list look complete.

**What was then built, and what was left absent on purpose.** The card now
publishes four of them, in `src/drivers/drm.rs`, each one true:

| property | object | shape here | why |
|---|---|---|---|
| `EDID` | connector | immutable blob | a real 128-byte EDID 1.4 base block from `crate::gpu::blob`, rebuilt when the host resizes the window - the detailed timing *is* the advertised mode |
| `non-desktop` | connector | immutable range 0..1, value 0 | this is a panel, not a headset |
| `max bpc` | connector | range 8..8, value 8 | the scanout is eight bits per channel; a range of one value means a write of 16 is `EINVAL` rather than a success that changes no pixels |
| `vrr_capable` | connector | immutable range 0..1, value 0 | the refresh rate cannot vary, and saying so is what stops a compositor asking |
| `IN_FORMATS` | plane | immutable blob | one linear modifier over the two formats `GETPLANE` reports and `ADDFB2` accepts - one array feeds all three, so they cannot drift |

The connector's `mm_width`/`mm_height` now come from the EDID's own centimetre
fields times ten (the relation the measurement found: 30 cm and 19 cm reported
as 300x190), and `subpixel` is written as the 0 a real connected connector
reports.

`DRM_CAP_ADDFB2_MODIFIERS` is answered 1 *and* made true: `ADDFB2` takes
`DRM_MODE_FB_MODIFIERS` with `DRM_FORMAT_MOD_LINEAR` and refuses any other
modifier with `EINVAL` - which is the `IN_FORMATS` list saying the same thing in
the other direction, and the reason the two are pinned together. A cap answered
1 over an `ADDFB2` that refused every modifier is a caller building a buffer it
cannot bind. `DRM_CAP_ASYNC_PAGE_FLIP` is answered 0 and *named* in the match
rather than left to the catch-all: tearing flips are not missing here, they are
a presentation mode this card does not have.

**And the format list itself was wrong in two ways at once, both found by
arithmetic against `<drm_fourcc.h>`.** `fourcc_code` packs the four named
characters low byte first, so the card's `DRM_FORMAT_ARGB8888` - `0x3432_4152` -
decodes to "RA24", which is `DRM_FORMAT_RGBA8888`; the real `ARGB8888` is
`0x3432_5241`. `DRM_FORMAT_ABGR8888` had the same slip, `0x3432_4142` being
"BA24" (`BGRA8888`) rather than "AB24". So `ADDFB2` was accepting two formats it
never named and **refusing the one it did** - and since a cursor buffer is
`ARGB8888` by definition, that is why the cursor plane below could not have bound
one.

Correcting the names exposed the second problem. `XRGB8888` ("XR24") and
`XBGR8888` ("XB24") are the *same* 32-bit word read the other way round in
memory: B,G,R,X against R,G,B,X. Presentation here is a verbatim row copy into a
BGRA block and nothing in the path swaps channels the way a real plane's scaler
does, so a card accepting both would show one of them with red and blue
exchanged - silently, at full frame rate. The accepted set is therefore narrowed
to the two spellings whose bytes are the order the scanout actually holds,
`XRGB8888` and `ARGB8888`, and the other two are refused rather than accepted
into wrong pixels. `GETPLANE`, `IN_FORMATS` and `ADDFB2` all read the one
`PLANE_FORMATS` array, so all three moved together.

The byte order is measured, not assumed, and the instrument is this repository's
own: `crate::term::blit`'s `put` writes a pixel as blue, green, red, 0, and that
console has been showing correct colours since it was written. It agrees with
`drivers::virtio::gpu`'s module comment ("BGRA, row-major") and with
`B8G8R8A8_UNORM` being virtio-gpu's default scanout format. A real card lists
`RA24` as well, because real hardware converts; this one does not, and now says
so instead of pretending.

`GAMMA_LUT`/`GAMMA_LUT_SIZE`, `VRR_ENABLED`, `panel orientation`,
`subconnector`, `FB_DAMAGE_CLIPS` and `OUT_FENCE_PTR` are **absent**, and each
absence is the measurement's rather than an omission: a real card carries
gamma and VRR properties because its driver can honour them, carries
`subconnector` and `panel orientation` only on connectors that have one (the
measured eDP output has neither), and carries neither of the last two at all.
A property whose write changes nothing is the silent success `AGENTS.md` §4
forbids. The one thing that *is* missing without a hardware reason is
`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`, and it belongs with the cursor plane in
the `MODE_CURSOR` work rather than here.

Syscalls and memory, the parts this section cannot fix from inside the display
driver: `/proc/<pid>/maps` **landed** (§Remaining has the file and the two
divergences it carries) - a crash reporter now has somewhere to look for a
program's base address; and the frame pool (§10.4), since the renderer families
allocate scratch in tens of megabytes where the current per-frame accounting was
never sized for it. Threads used to head
this list and are §6.4's now - landed. Anonymous demand paging
and `MADV_DONTNEED` used to be on that list - they are §5.1/§5.2's rewrite now,
and the scratch a renderer gives back is memory the machine can use again.

Services, which are rootfs work: `sway`/`niri` with their libraries
(`libdisplay-info`, `libseat`, `xkbcommon`, `pango`/`cairo` and the fonts *and
writable font cache* those need), `vulkan-loader` with an ICD, and the
compositor-starting half weston already models.

#### What the sync object family answers, measured and then built

`SYNCOBJ_*` is the last item on the list above besides `DRM_VIRTGPU_*`, and its
design is now written from measurement rather than from the sources, because the
two disagree about the one question the whole family turns on: **what a syncobj
that holds no fence means.** Four instrument programs were run against the real
card (`/dev/dri/renderD128`, AMD `amdgpu`, DRM interface 3.64.0, Linux 7.2.7) -
one for creation, destruction and the descriptor handover, one for the wait
family, one for the transfer and eventfd edges, and one for the three rules the
first three could not separate between them. Everything below is a line one of
them printed.

**This host's header is ahead of the upstream one in two ways a caller notices.**
`struct drm_syncobj_wait` and `struct drm_syncobj_timeline_wait` have each grown
a trailing `__u64 deadline_nsec`, so they are 40 and 48 bytes rather than the 32
and 40 the older headers describe, and
`DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE` / `..._FD_TO_HANDLE_FLAGS_TIMELINE`
exist at `1 << 1` on both directions. The first is not a detail: a program built
against the old layout sends a request word whose encoded size is eight bytes
short, and the entire wait family answers `EINVAL` - which is what the first
instrument run measured, in full, before anyone noticed. It is why every number
below is quoted with the struct it came from.

**A syncobj either holds a fence or it does not, and "does not" is a refusal
rather than a success.** This is the divergence from the sources, which treat a
fence-less binary syncobj as already signaled because there is nothing to wait
for. Measured here, every request that needs something to wait on refuses a
fence-less object instead:

| on a fence-less object | measured |
|---|---|
| `WAIT`, one handle, timeout `0` or `-1` | `EINVAL` (no blocking without `WAIT_FOR_SUBMIT`) |
| `WAIT` with `WAIT_FOR_SUBMIT` | blocks; `ETIME` when the timeout expires |
| `TIMELINE_WAIT` at a point no fence covers | `EINVAL` |
| `TIMELINE_WAIT` with `WAIT_FOR_SUBMIT` | success as soon as *a* fence exists, whatever the point; `ETIME` otherwise |
| `HANDLE_TO_FD` with `EXPORT_SYNC_FILE` | `EINVAL` |
| `EVENTFD` | accepted, and never fires |

So the state a syncobj is in is not "signaled or not" but "has something behind
it or not", and the operations that *create* one are the whole of how it changes:
`CREATE` with `SIGNALED`, `SIGNAL`, `TIMELINE_SIGNAL`, `TRANSFER` into it, and
`FD_TO_HANDLE` with `IMPORT_SYNC_FILE` each leave a fence behind; `CREATE`
without the flag and `RESET` take it away. A **timeline** is not a kind chosen at
creation - `CREATE` has no timeline flag - but a syncobj that has had a point
submitted to it: `TIMELINE_SIGNAL` on a binary syncobj is `ok` and makes it one,
and `TIMELINE_WAIT` on a syncobj that never had a point is `EINVAL`. `QUERY`
reports the highest point submitted (0 for one that never was), which is why
`RESET` on a timeline reads back 0, and why `SIGNAL` - which a timeline also
accepts - leaves the point where it found it.

The rest of the family, as measured: `TRANSFER` requires only that *one* of its
two objects hold a fence, otherwise `EINVAL`; it both copies the fence and sets
the destination's point to `dst_point` when the destination is a timeline, and it
does not care whether `dst_point` is above or below the point already there.
`HANDLE_TO_FD` with `flags = 0` answers a **syncobj descriptor** - the anonymous
file that *is* the syncobj, which is what `wp_linux_drm_syncobj_v1` passes between
processes - while `EXPORT_SYNC_FILE` answers a **sync-file** descriptor. The two
are told apart by the import: `FD_TO_HANDLE` with `flags = 0` requires a syncobj
descriptor and installs a second handle for the same object, while
`IMPORT_SYNC_FILE` requires a sync-file descriptor **and** a `handle` that names
an existing syncobj to replace - `handle = 0`, which is how the other direction
asks for a *new* handle, is `ENOENT` here. A program that follows the descriptor
matrix wrong gets `EINVAL` from the other direction's entry point, not a second
object. `EVENTFD` registers a one-shot per call: the eventfd's counter is
incremented by one when the registered point first becomes covered, immediately
if it already is, once per registration (registering twice and then signaling
once reads back `2`), and never if the syncobj is destroyed first.

Two consequences for the work, and they are what the implementation is shaped
around. **First, nothing on this machine can produce an unsignaled fence** - the
only fence a client can obtain is a signaled stub, which the instruments confirm
by never once seeing a wait block on an object that had one - so the whole
"asynchronous" half of the family collapses into "has a fence yet or not", and
that is a truth about this card rather than a simplification of it: there is no
engine here to be in flight, which is the same fact `WAIT_VBLANK` and the CRTC
sequence pair are refused on. **Second, `WAIT`/`TIMELINE_WAIT` are the only
blocking requests in the family**, which is what makes them the two that need the
scheduler: a `WAIT_FOR_SUBMIT` parks until some other request leaves a fence
behind, and `EVENTFD` is the same wait expressed as a counter for a caller with
an event loop - which is why it is the probe Smithay makes before it will expose
`wp_linux_drm_syncobj_v1` at all.

`DRM_CAP_SYNCOBJ` and `DRM_CAP_SYNCOBJ_TIMELINE` are both answered `1` by the
real card, so this card answering `1` is a statement it has to make true rather
than a capability it can decline.

**What was then built, and the one place it is deliberately narrower.** The
twelve requests are dispatched, `crates/gpu/src/sync.rs` holds the state machine with
nine host tests and `t-drm-sync` pins 103 checks in the booted machine - the
family's whole shape, the descriptor handover in both directions, a waiter woken
by *another task* rather than merely told it would be, and the two commit fences
end to end. Two names are worth putting beside the work. The **handle table is
per open file description**, which is what makes two clients each have a handle 1
and is the reason `SYNCOBJ_HANDLE_TO_FD` is the request the wayland protocol is
built on; a record dies with its description, and the object itself lives on
while a descriptor names it. And the **out-fence a commit writes back is already
through** when the caller has it, because this card presents synchronously: the
flush has returned before the commit does, so the honest fence is a retired one.
That is a narrower answer than a real card's - a caller cannot use it to learn
about a frame that is still in flight, because there is no in-flight frame - and
it is the same absence as `DRM_CAP_ASYNC_PAGE_FLIP` being 0.

**Gate, quoted from its own summary**: host `615 passed`; `mpos-xtask 36 passed`;
guest `=== SUMMARY pass=131 fail=0 (131 built)`, QEMU exit 33, `9 of 140 user
tests left out by tests.toml` (the same nine as before, `t-drm-sync` being a
tenth program that runs). `t-drm-sync` printed `=== PASS` over 103 checks.

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
3. ~~**DRM sync objects**~~ **Landed**, with `IN_FENCE_FD`/`OUT_FENCE_PTR` given
   the behaviour their names promise - which is what makes explicit sync real
   rather than merely accepted. The step was measured before it was written, and
   the measurement is the part worth keeping: see "What the sync object family
   answers" above for what this card means by a sync object and how it differs
   from the sources.
4. ~~**Threads** (§6.4)~~ **Landed** - not a display item, but on the critical path
   of every compositor that is not single-threaded and every Vulkan driver, so it
   is worth knowing the step is behind us before the property families.
5. ~~**`DRM_VIRTGPU_*`**~~ **Landed, 2D and 3D both** — the largest item, and the
   only one that makes niri possible. **"Then virgl in the image" is the part
   still open, and it is upstream of the 3D queue**: Mesa does not reach the
   driver at all, because libdrm cannot describe the card — `drmGetDevices2`
   returns 0. That chain is **built** now, from a working card's own sysfs reads
   (§7.4's third update): the node hangs off the display-class PCI function, and
   PCI functions carry the `subsystem` link they never had. What is left here is
   a *measurement*, not a build: the GL-card run that says whether the renderer
   is off `llvmpipe`, which the console defect below has so far killed every
   attempt at.
6. ~~**`/proc/<pid>/maps`**~~ and the frame pool: the supporting cast, wanted by
   everything above and by nothing in particular. (`MADV_DONTNEED` was on this
   line, and §5.2 has it now; `maps` is §Remaining's, and it is written - see
   the board's entry for the two things it needed, the loader's recorded ranges
   and the `MAP_SHARED` bit the perms column reports.)

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
- `SECCOMP_MODE_FILTER` — **done, wired, and measured action by action**. The
  program, its validation and its evaluation are `src/seccomp.rs` (classic BPF
  with the packet idioms left out, one description of the instruction set shared
  by the validator and the interpreter, forward-only jumps that must land inside
  the program, an out-of-range load ending the program with `KILL_THREAD`, and
  the stacked action order compared as a *signed* field so `KILL_PROCESS`
  outranks `ALLOW`); the plumbing is `Task::seccomp_filters` — a cumulative
  per-thread list inherited across `fork` and kept across `exec` — reached
  through one `install_seccomp_filter` from both `seccomp(2)`'s op 1 and
  `prctl(PR_SET_SECCOMP, 2)`, which copies the program out of the caller and
  validates it *in full* before anything is stored (`EINVAL` malformed, `EFAULT`
  unreadable, `EACCES` for a caller that is not root, `EINVAL` for any flag), a
  machine-wide armed flag that keeps an unfiltered syscall to one relaxed load,
  and a gate that acts on the answer *before* the table runs.

**The eight answers were then measured**, one child per action and a control
beside every table, because four of them were not what this kernel did:

| Action | What Linux does (measured, 7.2.7) | Before | Now |
|---|---|---|---|
| `ALLOW`, `LOG` | the call proceeds | same | same |
| `ERRNO\|d` | returns `-d` — and `d == 0` is a *successful* 0 | same | same |
| `TRAP` | no handler: killed by `SIGSYS`. With one: the handler runs, the call never happens, and the caller reads its own `rax` back (`getppid` answered 110, `getpid` 39 — the number, not a value) | `SIGSYS`, then `-ENOSYS` | `SIGSYS`, then `nr` |
| `KILL_THREAD` | killed by **`SIGSYS`**, not `SIGKILL` | `SIGKILL` | `SIGSYS` |
| `KILL_PROCESS` | killed by **`SIGSYS`** | `SIGKILL` | `SIGSYS` |
| `TRACE`, nobody tracing | the call fails with **`ENOSYS`** | allowed | `ENOSYS` |
| `USER_NOTIF`, no listener | the call fails with `ENOSYS` | `ENOSYS` | same |
| anything unrecognised | killed by `SIGSYS` — the man page's "treated as `KILL_PROCESS`", confirmed here | `-ENOSYS` returned | killed |

Three drafts of that measurement were wrong in ways that produced plausible
numbers, which is why the control exists and why it is written down: judging
*every* syscall with the action measures libc's collapse rather than the kernel
(every line read `SIGSEGV`); `BPF_LD|BPF_W|BPF_ABS` is **0x20** and not 0x00, and
writing 0x00 makes it `ld #0` — after which every conditional filter quietly
judged nothing; and a parent that reads its child's pipe to EOF before waiting
hangs for ever when that child is *stopped* holding the write end. The control —
filtering `getpid` to `EPERM` must fail `getpid` with errno 1 while `gettid`
answers normally — caught each in turn.

- `SECCOMP_GET_ACTION_AVAIL` — **done, and it answers the truth**. Supported
  ⇒ 0, with the caller's own word left alone (Linux's return value carries the
  answer; there is nothing to write back); not implemented — today only
  `USER_NOTIF`, whose purpose is to hand the call to a userspace supervisor this
  machine cannot summon — or carrying *any* data bits, however supported the
  action in them is (`ERRNO|1` is refused, measured), ⇒ `EOPNOTSUPP`; a `flags`
  it would have to ignore ⇒ `EINVAL`; an unreadable pointer ⇒ `EFAULT`. Before
  this it answered success and wrote 0 for every question, so a caller that
  asked learned nothing at all — and since the probe exists precisely because
  Linux kills the process for an action it does not know, answering "no" to
  everything was at least survivable, while answering "yes" to something not
  implemented would not have been.
- **Pins.** `t-seccomp` is new and holds one line per fact: the probe's table
  including the data-bits refusal, one child per fatal action (each is killed by
  signal 31 and each must run before this process installs anything, because a
  filter is inherited and the more restrictive of several decides), the two
  refusals that leave the caller alive answering `ENOSYS`, `LOG` letting the call
  through, then install + the `ERRNO` answer + `PR_GET_SECCOMP` reading 2 + a
  second filter judging another number (the list is cumulative) + inheritance in
  a child that installs nothing + stacking, proved by a `TRAP` installed on top
  of an inherited `ERRNO` winning the decision. `t-posix`'s probe line — it used
  to assert the empty-set answer — now expects `EOPNOTSUPP` for a word naming no
  action at all.
- **Remaining, named: `PTRACE_EVENT_SECCOMP`.** The third case of `TRACE` is a
  tracer that has set `PTRACE_O_TRACESECCOMP` getting its own stop, carrying the
  action's data in `PTRACE_GETEVENTMSG`. This kernel cannot produce it, so
  `PTRACE_SETOPTIONS` refuses the bit with the same `EINVAL` Linux gives for one
  it does not know — every traced caller is therefore the no-option kind, whose
  behaviour *is* Linux's. Building that stop is what would make `TRACE` mean
  notify rather than allow.
- **Not measured, and recorded rather than guessed**: strict mode's own violation
  signal. `seccomp(SECCOMP_SET_MODE_STRICT)` answers `EINVAL` on this machine for
  every argument shape tried, including after `PR_SET_NO_NEW_PRIVS` and with
  `args == NULL`, which is what an outer sandbox filtering `seccomp(op = 0)`
  looks like. Strict therefore keeps `SIGKILL`, the documented answer, and
  `t-posix`'s pin of it stands unchanged.

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

Current status: **stages 1a-1f, 2, 5a, 5b, 6a, 6b, 6c, 6d, 6e landed and `hal`
compiles for aarch64: the machine boots the
way the x86_64 one does, the kernel owns its own address space, two kernel tasks
can suspend and resume each other, the machine reads its own description of
itself, it reaches its devices through a window in its *own* half of the address
space - the firmware's thirty-two virtio-MMIO transports are found through the
tree and read back - code now runs at EL0 with an address space of its own,
traps into the kernel and is answered, and that address space is a thing a
process can live in rather than only enter: its mappings can be protected,
shared, copied on write and taken back, and its tables are returned when it
goes. On top of that the kernel can now answer the questions a *running* process
asks of a space (6c), it reads a real wall epoch off the PL031 (6d), it turns on
user SIMD, masks a line and waits on an interrupt (6e) - and with the `hal` seam
split into six baseline concerns plus three optional capabilities, aarch64
implements the six it has and `crate::hal` compiles for it.**
`cargo xtask boot-aarch64`
builds the kernel (`aarch64-unknown-none`), the loader
(`aarch64-unknown-uefi`), lays out an ESP with `EFI/BOOT/BOOTAA64.EFI` and boots
it under AAVMF; the loader reads `mpos.elf` off the volume, applies its
`R_AARCH64_RELATIVE` relocations against `KERNEL_VIRT_BASE`, builds four-level
tables from the firmware's own memory map and branches into the higher half at
**EL1**; then the kernel brings the frame allocator up from the record and
rebuilds the address space from `mm::vm::kernel_mappings`, which is the same
plan x86_64's `install` consumes. Measured, from one of three consecutive runs
that print every marker - the two numbers that move between runs, `47944` and
`47946` frames, differ by one page of the firmware's own noise, and every number
below is identical across those runs, the two frame addresses stage 6b prints
included:

```
mpos-boot: kernel image: 98 KiB
mpos-boot: initramfs: 11360 KiB
mpos-boot: RAM: 248 MiB
mpos-boot: device tree: 1052672 bytes at phys 0x44699000
mpos-boot: kernel: 104 KiB at phys 0x447da000, virt 0xffffffff80000000, entry 0xffffffff80011618
mpos-boot: mapped 496 MiB of physical memory, identity and through the direct map at 0xffff800000000000
mpos-boot: device window 0x8000000..0xc000000 mapped at its own address and at 0xffffffff40000000, Device and not executable
mpos-boot: page tables: 325 pages of them
mpos 0.1.0 (aarch64) booting
console: PL011 at 0xffffffff41000000
image: text 74 KiB, rodata 20 KiB, data 8 KiB (102 KiB total)
handoff: mpos-boot 0.1.0, record at 0x456ba020, image phys 0x447da000 -> virt 0xffffffff80000000, direct map at 0xffff800000000000
memory: 14 regions, 187 MiB the allocator may use
cmdline: "console=ttyAMA0"
exception level: EL1
devices: the firmware's tree, 1052672 bytes at 0xffff800044699000
devices: memory 0x40000000..0x50000000 (256 MiB), every region the record calls usable is inside it
devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)
devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window
M4_DEVICES_OK
pmm: 5 zones, 47944 frames (187 MiB of 187 MiB usable)
interrupts: GICv2 (distributor 0xffffffff40000000), vector table at 0xffffffff8000e000, timer on INTID 30
page tables: 165 frames (660 KiB), TTBR0 0x434e6000, TTBR1 0x434e6000 (the kernel's own, read back from the registers)
plan: 7 mappings, all 63626 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)
direct map: 0xffff800043458000 -> 0x43458000, rw and it stores
device window: PL011 0xffffffff41000000 -> 0x9000000, Device, through the kernel half at 0xffffffff40000000
no low half: 0x447da000 is unreachable, and so is the device window at 0x8000000 that used to live there
M3_TABLES_OK
virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device
virtio: the first is at 0xa003e00, device 4, vendor 0x554d4551
M5_VIRTIO_OK
timer: CNTFRQ_EL0 62500000 Hz, tick at 250 Hz (250000 counts apart)
M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz), 109192656 ns of counter
contexts: two kernel tasks alternating
ABABABABABABAM2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it
space: 4 pages mapped, walked in order and read back through the MMU; two protected, one copy-on-write and resolved into a frame of its own (0x43476000) with the parent's 0x4346a000 left alone; two shared between spaces and written through one of them; two unmapped and given back
space: dropping an active space gave back all 4 frames its tables took, and the page it mapped not among them
M6_AS_OK
paging: active/kernel roots told apart; a named-root walk user-gated and unmapped-refused; uaccess read+write vs unmapped vs the kernel half; the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing
M6_PAGING_OK
wallclock: PL031 reached through the kernel half; boot anchored to a live UTC epoch; the counter read-only, so write_epoch refuses; the software anchor moves on set_boot_epoch
M6_WALL_OK
cpu: CPACR.FPEN full access after enable_user_simd; a distributor line enabled then masked and read back; idle() woken by the clock and returning with interrupts off
M6_CPU_OK
EL0: the kernel was on SP_EL1 (SPSel 1)
EL0: root 0x43479000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000
EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7
M6_EL0_OK
```

The `devices:` lines are stage 5a, and they are a *check* rather than a
printout: every number in them was read out of the firmware's device tree and
compared against the constant the kernel is compiled with for that device - the
PL011's base, both of the GIC's frames, the timer's line, and the RAM the boot
record calls usable. A tree is not trusted to be right; it is what asks whether
this kernel's idea of the machine still is. The `187 MiB` and the `47944 frames`
against the last stage's `191 MiB`/`48954` are 3.66 MiB of this harness's own
device, 264 KiB of loader page tables and 16 KiB of kernel that stage 6b's check
added (text 62 -> 74 KiB, rodata 16 -> 20: the failure lines are most of it), and
all three are named below rather than
folded into "the tree costs a megabyte": the tree is 1052672 bytes for 7160 bytes
of structure and 569 of strings, and the loader copies what the tree says it is.

Stage 5b is the two lines between `M3_TABLES_OK` and `M5_VIRTIO_OK`, and it is
the first time this machine has *used* a number the firmware gave it rather than
checking one. The device window - PL011 and GIC included - moved out of `TTBR0`
and into the kernel half at `0xffffffff40000000`, which is where x86_64's
`mm::MMIO_BASE` already was, so the low half now holds nothing at all: the
`no low half:` line is the check that says so, and it asks about the device
window's *physical* address as well as the image's. `M5_VIRTIO_OK` then reads
four identity registers out of each of the tree's thirty-two `virtio,mmio`
windows through that new alias, which is what makes the tree actionable: a driver
now has somewhere to go. Three of those four registers are load-bearing
measurements rather than decoration. **The magic and version are checked** -
`0x74726976` and 2, the only layout this kernel's driver crate reads - and
**version is a choice the harness has to make**: measured, `virt`'s
`virtio-mmio` transports answer version **1**, the legacy register map, unless
QEMU is told `-global virtio-mmio.force-legacy=false`, which is now in the
command line beside `acpi=off` and for the same kind of reason. And **the tree
cannot say which window carries a device**: QEMU publishes all thirty-two nodes
whether or not anything is attached (measured by decompiling the tree with and
without `-device virtio-rng-device` - the two differ only in the firmware's own
random seeds), and the device does not land in the first of them either; it
answers at `0xa003e00`, the *last*. So a driver must read each transport, which
is the whole reason the check reads them.

Stage 6a is the last four lines of the log, and it is a change of kind rather
than of degree: everything above this stage ran at EL1, and this is the first
time this kernel has been *below* itself. What makes EL0 different is not the
privilege level as such but where the address space comes from - a program's code
and stack are in the half `TTBR0_EL1` names, the kernel's are in `TTBR1_EL1`'s,
and the two registers are what this stage is the first to tell apart. Until now
both pointed at the *kernel's* root, which worked only because stage 5b had left
the low half empty; so a user space is not an arrangement imposed on the kernel,
it is the state the kernel was already in being handed to somebody else. The
check is built the way stage 1c's and stage 2's were - the smallest program that
can be wrong in every way this mechanism can, and nothing that can only be wrong
for a reason the mechanism does not have yet. That program is forty bytes: it
puts the answer page's address in a *callee-saved* register, writes its own value
through it, calls a number no system call has, writes the answer back through the
same register, and leaves through `exit_group`. The verdict is read out of a page
the *program* wrote, because a trap is only observable by the code that answers
it and the honest way to check an answer is from the side that receives it: the
kernel reads back `0xaaaa` (the value the program wrote before it called
anything) and `0xffffffffffffffda` (`-ENOSYS`, the value the kernel put in `x0`
and the program read back). Nothing on the kernel side can fake either number.

**One measurement came out of that and changed the gate, and it is a difference
between the two machines rather than a bug.** `ELR_EL1` for a Supervisor Call
already holds the address of the instruction *after* the `svc`, so a gate that
steps over it - which is what the x86_64 twin's shape suggests, where `SA_RESTART`
subtracts the two bytes back - skips an instruction the program never got to run.
The first run of this check did exactly that, and the machine said so: the `svc`
is at `0x200010` and the trap reported `pc` `0x200014`, so the program's next
instruction (the store of the kernel's answer) was stepped over and the page came
back with a zero where the gate's answer should have been. The gate does not
advance `pc` at all now, and that is written down beside it.

Stage 6b is the two `space:` lines before `M6_AS_OK`, and it is the difference
between an address space a program can be *put into* and one it can *live in*.
What 6a did not have is any way to change a space once it existed, and that is
most of what a `fork`, an `mmap` and a `munmap` are made of: taking a mapping
back, changing one's permissions in place, walking what is there, and the
copy-on-write half that lets two processes share a frame until one of them writes
it. All of it lands in `paging.rs`'s `UserSpace`, and the encoding does not change
at all - what changes is that the file now writes *and erases*. The signatures are
deliberately `crate::hal::AddressSpaceOps`' rather than the type's own, because
writing them to the trait's shapes is what will make the impl delegation when
`hal` reaches this machine; the trait itself cannot be implemented yet, since
`crate::hal` is x86_64-only until all six of its concern traits exist.

Three things in those two lines are worth reading. The resolved frame,
`0x43476000`, is a frame the *child* space took for itself when it first wrote a
page it was sharing with the parent - and the parent's frame, `0x4346a000`, is
printed beside it because "the child copied" and "the parent was left alone" are
two claims and only one of them is about the child. Four frames is what a space's
tables cost here (a root and three levels down to the page), and dropping an
*active* space gives all four back and not the page it mapped: measured as an
exact count, because both ways of getting it wrong are off by a specific number,
and a leak would otherwise look like a machine that simply works.

The check reads every answer back through three different doors, and that is
deliberate: the space's *own* tables for what the kernel intends, a store to a
user address with that space switched in for what the MMU enforces, and the
frame's own bytes through the direct map for what actually happened. An address
space is exactly the thing where the first door alone is not enough - 6a's own
measurement, where a table the kernel had written was not the table the CPU
walked, is the same lesson one register down. It also has to be able to *fail*,
so all three of the mistakes it exists to catch were made on purpose before this
stage was accepted: a `resolve_cow` with the copy removed, a `Drop` that leaked
its tables, and a copy-on-write page marked without the read-only bit. Each one
stopped the boot with its own `M6_AS_FAILED` line naming the claim that broke -
and the first run of the check, on code that was correct, failed on the check's
own arithmetic instead (three pages left where it had counted two), which is why
the count it prints is a count rather than a list of addresses.

The `plan:` line is the strongest claim either machine's boot makes about its
own page tables: every mapping `mm::vm::kernel_mappings` names is walked page by
page and each page is asked to translate to the frame and permissions the plan
gave it - so a disagreement is a failure whichever side is wrong, and the direct
map coming back `rw` is the same plan that gives x86_64's `rw`.

The `-kernel` bring-up of 1b-1d is **deleted**, not left beside this: the
`aarch64-unknown-none` block is `relocation-model=pic` + `-pie` like the x86_64
one, the linker script has no base address and no `.stack`, and
`src/main/aarch64.rs` is an `extern "C" fn _start(x0)` with no assembly stub.
Nothing above `arch` is ported yet; `src/lib.rs`'s header carries the stage
ledger, which is the record of which module un-gates when.

This section used to say "kernel ELF compiles but boot flow incomplete", which
was false in both halves. Nothing compiled for aarch64 - V-8 (§11.1.8) removed
the `[target.aarch64-unknown-none]` block that named a linker script which had
never existed - and "incomplete" hid the real shape of the work, which is not
one boot sequence but a second ABI. Measured: the x86-specific surface is
*concentrated* (port I/O in seven files, no ACPI consumer, no IOAPIC, no HPET,
no i8042), but `src/syscall/linux/` is 11028 lines and `src/drivers/` 14481, and
that is the long tail, not the machine layer. `src/` also carried only 17
`target_arch` gates, so everything else was gated on `target_os = "none"` alone
- true for both bare-metal targets - and tried to compile for aarch64 and could
not.

So the order below is the measured one. Each stage is its own commit and is
verified by the means named; none of them claims a green from compilation alone.

**Why stage 2's user-entry half is at stage 6.** The table used to put
`UserFrame` (`x0..x30`/`pc`/`pstate`) and the `eret` that enters EL0 in stage 2
with the kernel switch. They moved, and the reason is not tidiness: every field
of that struct gets its order and its meaning from `user_regs_struct`, which is
x86_64-only until stage 6, so a frame written at stage 2 would be a struct
nothing could check against anything - and its `eret` sequence has no user
address space to enter until stage 6 builds one. Writing them early would have
produced exactly the shape this port refuses: code that looks finished and has
never been run. What stage 2 keeps is the half that *is* checkable at this stage,
and it is checked - the boot prints `M2_CONTEXT_OK`. **Stage 6a is where both
arrived, and neither was written before it could be run**: the frame's field
order is the ABI's own `user_pt_regs`, and the `eret` enters a space that the
same stage builds and the same boot walks. The aarch64 numbers are not the
x86_64 ones - `exit_group` is 94 here against 231 there - so a `nr.rs` written
at stage 6a would still have been a table nothing could check against; the one
number the gate answers is spelled beside it and named as such.

| # | Stage | Verified by |
|---|---|---|
| 1a | Build plumbing: `rust-toolchain.toml` targets, `src/arch/aarch64/linker.ld`, `[target.aarch64-unknown-none]`, the gate list and the stage ledger | **landed**: `cargo build --target aarch64-unknown-none` links; the x86 gate stayed green |
| 1b | The machine body: PL011 at `0x0900_0000`, the `_start` stub, the banner | **landed**: `qemu-system-aarch64 -M virt -kernel` printed the banner - the bring-up instrument 1e deletes, named as such at the time |
| 1c | Exception vectors (`VBAR_EL1`), GICv2, the generic timer (`CNTPCT_EL0`/`CNTP_CVAL_EL0`) | **landed**: `M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz)`, three runs of three, and the check's failure branch names the timer's, the distributor's and the CPU interface's registers rather than hanging |
| 1d | 4-level page tables, `TTBR0_EL1`/`TTBR1_EL1`, `TCR_EL1`/`MAIR_EL1`/`SCTLR_EL1` | **landed**: `page tables: 12 frames (48 KiB)`, `M3_TABLES_OK` (image `r-x`/`r--`/`rw-` at both its addresses, direct map written and read back, null page absent), and the clock still counts to 25 with the MMU on |
| 1e | The aarch64 loader: `boot/src/vm/aarch64.rs` (the descriptor format, `MAIR`/`TCR`/`SCTLR`), `boot/src/main.rs`'s aarch64 `enter` and PL011, `boot/src/elf.rs` learns `EM_AARCH64`/`R_AARCH64_RELATIVE`, the kernel relinked as a higher-half PIE, `src/arch/aarch64/paging.rs` becomes a reader of the loader's tables, `cargo xtask boot-aarch64` | **landed**: AAVMF boots the real ESP image, the kernel reaches the higher half and prints `M3_TABLES_OK` (the loader's `TTBR0`/`TTBR1`, the image's W^X, the direct map written and read back, the PL011 device) and `M1_TIMER_OK: 25 interrupts in 100 ms`; the `-kernel` instrument is deleted |
| 1f | The kernel's own tables: `src/arch/aarch64/paging.rs` regains a builder, built from `mm::vm::kernel_mappings(info, sections)` out of frames from `mm::alloc_frames`; `mm::init` runs on aarch64; the boot switches `TTBR0_EL1`/`TTBR1_EL1` onto it | **landed**: `pmm: 5 zones, 49209 frames`, `page tables: 166 frames (664 KiB)`, `plan: 7 mappings, all 63616 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)`, `no identity map: 0x44bcf000 is unreachable, only the device window lives low`, then `M3_TABLES_OK` and `M1_TIMER_OK` |
| 2 | Kernel contexts: `src/arch/aarch64/context.rs` - `Context` with the callee-saved `x19..x28`/`fp`/`lr` on the task's own stack, `FpuState` (`q0..q31`, `FPCR`, `FPSR`), `bootstrap` and its trampoline, `switch` | **landed**: `M2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it`, and the letters above it alternate `ABABAB` |
| 2b | *Not a stage* - the **user** frame (`x0..x30`/`pc`/`pstate`) and its `eret` entry moved to stage 6, where the ABI that gives those fields their order and their meaning is. See the note below the table | - |
| 3 | Un-gate `sched` (the TLS `MSR` write moves behind `hal`/`arch`) | `cargo build --release --target aarch64-unknown-none` with `sched` in - and see the correction below the table: this row is **not** one edit, and it waits on 4, 5 and 6 |
| 4 | Un-gate `console` and `vfs` (serial-only console first) | a ramfs root with the initramfs untarred |
| 5a | The device tree: `crates/fdt` (the reader, host-tested), `Handoff.dtb_phys`/`dtb_len` appended after `regions`, the loader copying the firmware's blob out of the UEFI configuration table, and `arch::aarch64::check_devices` holding the tree up against the machine's own constants | **landed**: `cargo xtask boot-aarch64` prints `devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)`, `devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window` and `M4_DEVICES_OK`, and the tree's memory node is compared with every region the record calls usable |
| 5b | The device window in the kernel half, and the tree's transports read back: `paging::DEVICE_PHYS`/`DEVICE_VIRT` split, `boot/src/vm.rs` mapping the window at both, `pl011`/`gic` compiled against the alias with their physical bases kept for the tree check, and `arch::aarch64::check_virtio` reading every `virtio,mmio` window's MagicValue/Version/DeviceID/VendorID | **landed**: `M3_TABLES_OK` now stands under `no low half: ... and so is the device window at 0x8000000 that used to live there`, and `M5_VIRTIO_OK` under `virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device`, across three consecutive green runs |
| 5 | Drivers: virtio-MMIO found through the tree, PL011 as the serial device, GIC line routing | virtio-blk and virtio-net up - **the discovery half of this row is 5b and is done; what remains is the drivers themselves**, whose transport is `virtio_drivers::transport::mmio` reached at `paging::device_virt(phys, len)` |
| 6a | The entry to EL0: `paging::UserSpace` (a root of its own for `TTBR0_EL1`, user page mappings, the switch that installs it), `context::UserFrame` (`x0..x30`/`sp`/`pc`/`pstate`, in the ABI's own `user_pt_regs` order) with `enter_user` and `leave_for_kernel`, and the vector entry for a trap from a lower EL - which saves the whole register file and can *return*, unlike the fatal synchronous one | **landed**: `EL0: root 0x4346c000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000`, `EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7` and `M6_EL0_OK`, across four consecutive green runs |
| 6b | The address-space operations, in `crate::hal::AddressSpaceOps`' shapes: `map`/`try_map`/`unmap`/`protect`/`map_cow`/`map_shared`/`resolve_cow`/`walk_user_pages`/`set_cow_bit`/`set_cow_bit_quietly`/`flush_if_active`/`phys_for_virt` and the `Drop` built out of `unmap`; the copy-on-write marker in descriptor bit 55 (software-reserved there, `RES0` to the walker) with the read-only bit the CPU enforces set beside it, and `USER_END` as `TTBR0`'s own span rather than x86_64's bit-47 split | **landed**: `M6_AS_OK` under `space: 4 pages mapped, walked in order and read back through the MMU; ...` and `space: dropping an active space gave back all 4 frames its tables took`, across three consecutive green runs - and each of the three mistakes the check exists to catch (a resolve with no copy, a `Drop` that leaks its tables, a marker set without the read-only bit) was made on purpose and stopped the boot by name |
| 6c | The `hal::Paging` methods that need a *running* process, at module level so the `impl` is delegation: `active_root`/`kernel_root` (the `TTBR0`/`TTBR1` statics), `switch_to_root`/`switch_to_kernel` (over the private `load_root0`), `phys_for_virt_of`/`user_phys_for_virt_of` (over `walk_from`, the second gating on `USER`), `user_accessible`/`is_cow_page` (over the leaf walk + bit 55), `write_user_u32` (through a named root, no live switch), and `map_mmio` (asserts the fixed window covers the range - `virt` has no dynamic MMIO) | **landed**: `M6_PAGING_OK` under `paging: active/kernel roots told apart; ... the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing` - each claim read back against the hardware it names, and a kernel-half address rejected by `user_accessible` because a user pointer is a `TTBR0` address |
| 6d | The `hal::WallClock`: a PL031 driver reached through the kernel-half window (`0x0901_0000`), `boot_epoch` anchored to a read of the counter at init, `set_boot_epoch` moving the software anchor, `write_epoch` refusing (the counter is host-shadowed and read-only on QEMU's model, so there is no persistent register) | **landed**: `M6_WALL_OK` - and the design changed by measurement: the plan said this machine had no wall source, but reading `RTC_DR` at boot gave the host's real UTC epoch (`CR=1`, already counting) and a programmatic write did not stick, so `boot_epoch` reads it like x86 reads the CMOS and `write_epoch` answers `false` rather than pretending |
| 6e | The `hal::Cpu`/`hal::Interrupts` primitives the machine lacked: `enable_user_simd` (`CPACR_EL1.FPEN=0b11`, read-modify-write), `idle` (`daifclr;wfi;daifset`), `gic::disable` (clear `GICD_ICENABLER`) and `is_enabled`, the serial line's `SERIAL_INTID=33`, and the `Line -> INTID` mapper | **landed**: `M6_CPU_OK` - FPEN read back, the serial's distributor bit set then cleared and read back, and `idle` woken by an armed timer tick (28->29) returning with interrupts masked |
| 6f | `hal` compiles for aarch64: the arch-neutral split moves x86's optional capabilities (`Msi`, `FpuTrap`, `Segments`) out of the baseline traits, `Current` un-gates to `target_os = "none"`, and `arch/aarch64` writes `impl {Clock,WallClock,Cpu,Interrupts,Paging,Contexts,Platform} for Current` plus `AddressSpaceOps for UserSpace`, the ops traits, and a full-register `enter_frame` that `check_el0` and the trait `enter_user`/`resume_user` share | **landed**: `cargo clippy --target aarch64-unknown-none` compiles `hal` for aarch64 (the deliverable; the debug aarch64 *build* cannot link, so clippy is the check - `AGENTS.md` §3), boot-aarch64 still prints all ten markers, and the x86 gate + depgraph ratchet are unchanged. The `to_user_regs`/`from_user_regs` `user_pt_regs` field-order pin and the COW-fault resolver wait on stage 6's ABI caller and stage 3's scheduler - named below, not stubbed |
| 6g | The kernel's own ELF loader honours the machine it is given: `mm/elf.rs` reads `e_machine`, accepts `EM_X86_64`/`EM_AARCH64` (refuses any other, where it used to validate nothing), selects the arch's `R_*_RELATIVE` kind from the file rather than hardcoding `R_X86_64_RELATIVE`, and reports `Image.machine` so the per-arch `execve` caller can reject a foreign-but-known ISA. An aarch64 static-pie now has its relocations read instead of silently dropped | **landed**: host unit tests `an_aarch64_image_gets_its_relative_relocations_read`, `an_image_for_an_unknown_machine_is_refused`, `a_file_that_names_no_machine_is_refused`, `an_x86_64_image_reports_its_machine` pass (509 unit tests total), and `cargo xtask test`'s 142 user tests still exec real x86 binaries through this parser - so the x86 path is unchanged and the aarch64 path is pinned by a test that fails on the old type-8-only filter. `USER_LIMIT` is still x86_64's `1<<47` (aarch64's user span is `paging::USER_END`); taking it from the machine is a named follow-on, conservative for now because aarch64 statics place far below it |
| 6 | The rest of the aarch64 Linux ABI: `nr.rs` per arch (the numbers are the machine's - `exit_group` is 94 here and 231 on x86_64), signal frames, `TPIDR_EL0` TLS, `PTRACE_GETFPREGS`'s view of `FpuState`, the `Paging` methods that need a running process, and the `hal` impls those two complete | the `t-*` conformance programs run on aarch64 |
| 7 | aarch64 userspace and the image: `aarch64-unknown-linux-musl` builds, ESP layout for AAVMF, arch-aware `xtask` | `cargo xtask test` passes on aarch64 |
| 8 | The gate: `xtask test`/`run` grow an arch selection | a green aarch64 gate quoted with its summary line and exit code |

**The order below the machine layer is not the order of the table, and that was
measured rather than argued.** Row 3 reads as one edit - "un-gate `sched`: the
TLS `MSR` write moves behind `hal`/`arch`" - and it is not. Widening that gate
alone and building reports **50 errors, of which three are the `wrmsr`**; the
rest are 23 missing `vfs`, 9 missing `syscall`, 7 missing `hal`, 2 missing
`drivers` and a `println!`, because `sched` names all four directly. So `sched`
cannot be un-gated before 4 (`vfs`), 5 (`drivers`) and 6 (the ABI) have landed,
and `hal` - which `sched` wants for the TLS write and which everything else
wants for the machine - cannot compile for aarch64 until all six of its traits
exist, which needs stage 6's `Paging` user half and a `WallClock` this machine
has no driver for. The next work is therefore **5, not 3**, and the table's
numbering is the plan's order rather than the order it can be done in.

**And stage 5's drivers turned out to be on the same side of the knot**, which
is stage 5b's own measurement rather than a guess: un-gating `drivers` alone is
**190 errors** whose long tail is `crate::vfs` (eight imports, from the device
nodes, `uinput`, `rtc_dev` and the DRM card), `crate::uaccess`, `crate::sched`
and `mm::map_mmio`; un-gating `console` alone is **18**, and they are
`drivers::serial`, `drivers::video::render` and `sched`, so the console cannot
move without the drivers it writes through. The only cut in the knot is `hal`,
and the only thing standing between `hal` and compiling is the *user* half of the
machine - which is stage 6. That is why 6a and 6b came next rather than 5's
drivers, and why the drivers' turn is after the rest of 6 rather than before it:
the tree walk 5b landed is the half of "virtio-MMIO found through the tree" that
does not need `vfs`, and the transport that opens a device is the half that does.

**And the profile matters: the aarch64 debug build does not link.** `cargo
build --target aarch64-unknown-none` fails with `relocation R_AARCH64_ABS64
cannot be used against local symbol` out of the *sysroot's* `liballoc`, which is
not built for a PIE; only `--release` links, and `--release` is what `xtask`
builds, so no gate has ever noticed. Row 3's verification command used to be the
bare one; it says `--release` now, and `AGENTS.md` §3 carries the same warning.

Named gaps the port carries rather than hides, each with its home:

- **The `-kernel` bring-up was an instrument, and it is gone.** Stages 1b-1d
  entered at a fixed physical address with the MMU off, because that is the
  smallest thing that proves the toolchain, the linker script and the machine
  body agree. Stage 1e deleted it and boots from the ESP through AAVMF, which is
  the promise this section makes: a second machine with the *same* boot path,
  not a shortcut past it. Deleting it meant more than removing a QEMU flag -
  the image had to become relocatable (`relocation-model=pic`, `-pie`, a script
  with no base address), the stack had to move from the script's `.stack` to one
  the loader allocates, and `_start` had to stop being an assembly stub. What is
  left of the instrument is one entry in `AGENTS.md`'s trap table saying not to
  bring it back.
- **The exception level is measured rather than assumed, and it survived the
  move to the real boot path.** QEMU 11.1.1 with `-M virt` and a bare aarch64 ELF
  entered the `-kernel` image with `x0 = 0` and `sp = 0` (the banner printed
  `x0`, and QEMU's own `-d cpu` log agreed). The AAVMF path is a different
  machine in every respect - a UEFI application is entered with the firmware's
  calling convention, not a register of our choosing - and it lands at **EL1**
  as well, which is what the kernel's `CurrentEL` check now reports and what its
  EL1-only vectors and physical timer need. A machine that arrived at EL2 would
  stop with a line saying so rather than configure the wrong one.
- **The tree is read now - and it does not arrive in a register, nor under the
  name this section used to assume.** `x0` is the handoff record's register on
  both machines. Under `virt` + AAVMF the tree is published in the **UEFI
  configuration table**, which is firmware data that does not survive
  `exit_boot_services`, so the loader reads the header's `totalsize` there and
  copies that many bytes into frames of its own (stage 5a). Two corrections came
  out of doing it, and both were measured rather than reasoned. First, that
  table has **no entry for a tree at all** when QEMU builds ACPI tables, which
  `-M virt` does by default on aarch64: the firmware advertised `ACPI2`
  (`8868e871-e4f1-11d3-bc22-0080c73c8881`) among the nine tables the loader
  prints, and no device-tree GUID. The boot therefore selects
  `-machine virt,acpi=off`, which is the same machine with the other
  description of itself, and the tree is the one this port reads. A real aarch64
  server would hand over ACPI and no tree; reading *that* is a gap this port
  names rather than half-does, and `check_devices` failing by name is what makes
  it visible instead of silent. Second, the cost: `totalsize` is 1052672 bytes
  for a tree of 7160 bytes of structure and 569 of strings, and copying what the
  tree says it is costs one MiB of the machine - the difference between `192
  MiB` and `191 MiB` of usable RAM in the log above.
- **The GICv2 bring-up is three bits across two frames, and the wrong value is
  silent.** Measured on this machine, and recorded because the register values
  are load-bearing: the timer's line needs `GICD_CTLR` *and* `GICC_CTLR` bit 1
  set - group 1 is what an IRQ *is*, so a line left in group 0 with only bit 0
  written is pending forever and raises nothing - and `GICC_CTLR.AckCtl` (bit 2)
  as well. Without AckCtl the acknowledge reads **1022** instead of 30, which by
  the GICv2 specification is its answer when the pending interrupt is group 1
  and the acknowledging access is secure; the interrupt is therefore never
  retired, `nIRQ` stays asserted, and the vector is re-entered every few hundred
  nanoseconds. That is not "the clock is broken" from the outside - it is a
  clock that looks *very fast*, and it cost a 1022/30 reading to tell apart.
  Linux programs the same `0x7`. The check's failure branch now prints the five
  registers that separate the three causes.
- **The machine addresses are hardcoded, and now they are checked at every
  boot.** QEMU virt puts PL011 at `0x0900_0000`, the GIC at
  `0x0800_0000`/`0x0801_0000` and RAM at `0x4000_0000`, and those are what the
  kernel is compiled with: the tree does not yet *supply* them. What stage 5a
  made it do is ask whether they are still right - `check_devices` reads each
  node back and compares it with the *physical* bases the kernel keeps for that
  (`pl011::PHYS`, `gic::DIST_PHYS`/`gic::CPU_PHYS`, `gic::TIMER_INTID` - the
  addresses the kernel *reaches* those registers at are stage 5b's
  `pl011::BASE`/`gic::DIST`/`gic::CPU`, and they are deliberately different
  numbers now), and the boot stops with `M4_DEVICES_FAILED` on any
  disagreement. A machine that moves a device is now a boot failure rather than
  a driver faulting on its first register. What is still hardcoded is everything
  with no constant to compare against, and stage 5b took the direction that
  matters for discovery: the 32 virtio-MMIO windows, whose addresses are in the
  tree and nowhere else, were **counted and bounds-checked** and are now
  **read** - `check_virtio` walks them, translates each into the kernel half and
  reads the transport's own identity registers, so a window with nothing behind
  it is a named `M5_VIRTIO_FAILED` rather than a driver that times out later. Two
  things about that read are measurements rather than assumptions, and both are
  recorded because a driver inherits them: the transports answer **version 1**
  (the legacy register map) unless the machine is told
  `-global virtio-mmio.force-legacy=false`, and the tree cannot say which window
  has a device - all thirty-two nodes are published either way, and the attached
  device answers from the *last* window, not the first. Still missing is the
  routing of anyone's interrupt line: the PL011's own is SPI 1 (INTID 33) and
  nothing decodes it, because the console is a polled writer until stage 4.
  The window deliberately does not start at zero - a null pointer that silently
  works is the one thing x86_64's own `check_arch` asserts cannot happen.
- **The kernel's address space is its own now, and the window that was open
  across 1e is closed.** Two things went together and are worth keeping as one
  measurement: the loader builds its identity map and its direct map with a
  single call and a single set of flags, so on the loader's tables both come out
  `rwx` - a window x86_64 closes the moment its kernel installs its own tables
  and stage 1e could not. Stage 1f closes it: the plan gives the direct map
  `READ | WRITE`, the boot log prints `direct map ... rw and it stores`, and the
  check walks every one of the 63616 pages the plan names to say so. What the
  loader's tables are still needed for is the length of the jump: the kernel
  cannot build its own until the frame allocator is up, and the frame allocator
  needs the direct map.
- **The device window is in the kernel half now, and the low half is empty.**
  Stage 5b closed this one, and it was closed by changing where the registers are
  reached rather than by naming the collision better: the kernel mapped 64 MiB
  from `0x0800_0000` - PL011 and GIC - at the addresses the devices physically
  are, which is inside `TTBR0`, the half a process is going to own. It maps them
  at `0xffffffff40000000` instead, the same hole x86_64's `mm::MMIO_BASE` uses,
  and `check_paging` now asserts both halves of that: the alias translates to the
  right frame as Device, and neither the image's physical address nor the device
  window's is reachable any more. What is *not* closed is the mechanism: the
  window is a fixed 64 MiB range mapped whole at boot, because `mm::map_mmio`
  goes through `hal::Paging` and `hal` cannot compile for this machine until the
  last of its six traits exists (`Paging`'s user half and a `WallClock` are what
  6b leaves), so the dynamic allocator cannot exist.
  A device outside the tree's window is refused by `paging::device_virt` rather
  than mapped, which is the honest answer for a machine whose devices are a
  firmware-described range - and not the general one a PCIe aarch64 server needs.
- **Stage 1d's RAM window is gone; the map is the firmware's and the plan's.**
  The 1 GiB ceiling it mapped "because the code that would care had not arrived"
  is replaced twice over: stage 1e's loader walks the EFI memory map region by
  region, and stage 1f has the kernel rebuild the address space from that record
  by way of `mm::vm::kernel_mappings`. This boot reports `14 regions, 192 MiB
  the allocator may use` out of 248 MiB of RAM, `pmm: 5 zones, 49209 frames`.
  The floor the whole thing stands on is `mm::init`, which is machine-neutral
  and simply had no aarch64 caller until 1f gave it one.
- **`hal` compiles for aarch64, and the "two blockers" this note used to name were
  an understatement.** Until stage 6f the module was x86_64-only for a *structural*
  reason this ledger missed: `hal::Current` gated on `target_arch = "x86_64"` so the
  module could not even be named here, and four capability groups were x86-shaped
  *inside* the baseline traits. A caller survey found them (MSI, segment selectors,
  the lazy-FPU trap protocol, and the first-user-entry shape) and that every one of
  their callers lived in a module aarch64 does not compile yet. So rather than make a
  second machine fake answers (`AGENTS.md` §4 rule 4), stage 6f split them into three
  *optional* capability traits - `Msi`, `FpuTrap`, `Segments` - that only x86's
  `Current` implements, un-gated `Current` to `target_os = "none"`, and had
  `arch/aarch64` write the six baseline impls. The two things this note claimed were
  the *last* blockers were in fact the easy half: the running-process `Paging` methods
  (6c), the `WallClock` (6d), and `Cpu`/`Interrupts` primitives (6e) - each a real
  body read back against the hardware it names, not a stub. `crate::hal` now compiles
  for aarch64 under `cargo clippy --target aarch64-unknown-none`.
  What remains is *below* the gate, not blocking it: the trait `enter_user`/`resume_user`
  and `set_kernel_stack` share the `enter_frame` that `check_el0` now exercises, but
  the running-process caller that schedules tasks is stage 3; `to_user_regs`/`from_user_regs`
  write the aarch64 `user_pt_regs` order but their field-order pin against `ptrace` is
  stage 6's ABI; the copy-on-write fault resolver (the next bullet) needs a faulting
  task. The x86-only `Msi`/`FpuTrap`/`Segments` are *not implemented here on purpose* -
  a `virt`+GIC machine has no MSI controller, no lazy FPU and no segments, and the
  sched/runtime code that wants them un-gates at stage 3 with its own arch branch.
- **A copy-on-write page can be resolved, and nothing faults into the resolver
  yet.** Stage 6b landed the operation - `resolve_cow` copies the frame, rebinds
  the descriptor and answers `(old, new)` - and the check drives it in the order a
  `fork` would, but the other half is deliberately absent: a data abort from EL0
  on a read-only page has to be recognised as a copy-on-write fault, resolved and
  the faulting instruction retried, and today the EL0 vector treats anything that
  is not an `svc` as fatal. That belongs with the `Paging` methods that take a
  *running* process - `is_cow_page` is what such a handler asks first - which is
  the other remaining `hal` blocker, so no code was written for it here and a page
  in a user space cannot yet be shared by two processes that are both running.
- **No ACPI consumer exists to reuse.** `virt` + AAVMF passes a *device tree*,
  so device discovery is new DTB-parsing code in stage 5, not a port of an
  existing table walk.
- **`rand` has no aarch64 hardware source yet**, so `seed()` takes its
  deterministic arm there; `RNDR` (where present) and the boot-epoch mix land
  with stage 5's drivers.
- **A host-compiled module is excluded from the aarch64 build without being
  excluded from the host.** `sched`/`vfs`/`syscall`/`uaccess` are the host unit
  suite's, so their gate is `any(not(target_os = "none"), target_arch =
  "x86_64")` - "the host, or x86_64 bare metal" - not `target_arch =
  "x86_64"`, which would have silently deleted 434 host tests while compiling
  cleanly.

**The older prerequisite list, kept for the record** (all six still stand, and
the stage table above is where each lands): the `VBAR_EL1` exception vector
table, a GICv2/v3 driver, 4 KiB pages through `TTBR0_EL1`/`TTBR1_EL1`, PSCI for
reboot/shutdown, the generic timer, and a PL011 or SBSA UART.

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
| A SATA probe that ends in an answer instead of a budget | `drivers/ahci.rs`, `crates/ahci` | **done, measured** |

**The probe's cost, before and after.** Not a throughput change but the same
ledger, because it is a device wait that the boot pays: on a `q35` machine whose
built-in SATA controller has no disk on it, the gap between `virtio-blk: vda, ...`
and `blk: no data partition ...` in the boot log was **28.412 s** and is now
**0.001 s**, measured on the same image copy with a timestamped serial log
(`-machine q35,sata=off` gives the same 2 ms, which is what named the probe as the
cost). The healthy path is unchanged and still measured - a command on a port
that really has a device retires at try 0-2 - and `t-ahci` is the pin that says
so. `PLAN.md` §7.3 holds the register dump, why a count of MMIO reads is still
the shape rather than a tick deadline, and the two gaps that were left open there
and are now closed.

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

## Phase 11: Architectural Debt — **AUDIT DONE; STEPS A–G LANDED, AND V-1/V-4/V-13 CLOSED** (D.2b evaluated & rejected as no-value; surviving `drivers ↔ vfs` pair is its own finding)

Everything above this phase is about *features*: what the kernel cannot yet do. This phase is
about the *shape* of the kernel that does them, and it opens with a measurement rather than an
opinion, because "bad architecture" is not a finding until something counts it.

`ARCHITECTURE.md` is the companion document: it draws the module graph, the boot sequence and
the syscall path, and its §9 mirrors the findings below. This section is the work list.

**The audit itself was written without editing a file** — every finding below was read out of the
tree, and each says where. What has landed since is marked on the finding and on the step: **A**
(the dead modules and the leftover trace), **E** (the seven pure leaves as crates) and **F** (the
gate's written criterion and the check for it). B, C, D and G are open.

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

**The same extraction runs as a check now** (§11.2 Step G, `cargo xtask depgraph`), and running it
that way found a third correction of the same kind: this script matches *text*, so an edge it
reports can be a doc comment. Measured with `//` comments stripped — i.e. who names whom in code —
the tree had **20** mutual pairs, not 21: `uaccess → syscall` is a mention in a doc comment, and
`vfs → uaccess` with it. The ratchet holds the code number — **19** since Step C, which took
`sched → net` out and with it one pair — and the three baselines it records (re-measured
2026-10-02) are in §11.2 Step G.

That last row is the finding. At module granularity the kernel is one strongly connected
component: there is no layer that can be compiled, tested or refactored on its own. Every
other item below is a *cause* of that number, which is why the fixes are ordered to reduce it
rather than to make any single file prettier.

---

### 11.1 The findings

Severity is about cost-to-change, not about user-visible bugs: none of these is a defect a
program can observe today.

#### 11.1.1 — V-1: the `Inode` downcast chain (severity: high) — **CLOSED**

`src/vfs/mod.rs:453–546` gave the central VFS trait a method per concrete device and network
type:

```rust
fn as_fb(&self) -> Option<&crate::drivers::virtio::gpu::FbDev> { None }
fn as_evdev(&self) -> Option<&crate::drivers::virtio::input::EvdevNode> { None }
fn as_uinput(&self) -> Option<&crate::drivers::uinput::Uinput> { None }
fn as_drm(&self) -> Option<&crate::drivers::drm::DrmCard> { None }
fn as_vsock(&self) -> Option<&crate::net::vsock::VsockSock> { None }
fn as_unix(&self) -> Option<&crate::net::unix::UnixSock> { None }
```

(The list grew to nine while the audit was being written — `as_syncobj_fd`, `as_sync_file` and
`as_packet` are the same shape and arrived with the DRM sync-object and AF_PACKET work.)

This is the textbook abstraction leak: the VFS — which is supposed to know only that a node has
a name, a kind, bytes and an owner — names six device drivers and three network socket types,
and the *syscall* layer reaches through these to drive a device. Adding a seventh device type
means editing the VFS trait, so the VFS can never be compiled without `drivers` and `net`, and
neither can anything that depends on the VFS. It was the largest single contributor to the
23-cycle count.

**Closed by Step B1 (§11.2)**: all nine are gone and `src/vfs/mod.rs` names `crate::drivers::`
and `crate::net::` zero times. What replaced them is one type-erased handle that the *caller*
names a type through — `core::any::Any` as a supertrait plus a single upcast — so the question
"is this node an `FbDev`?" is asked by the layer that knows what an `FbDev` is, and the trait
that answers it never hears the name. Adding a tenth device type now touches no VFS code.

**What it deliberately did not fix**, so the next reader does not expect it: the
`drivers ↔ vfs` mutual pair stands. `vfs` still names `drivers` 17 times and `net` 4, from
`vcmux`'s render/drm calls, `procfs`/`sysfs`'s PCI config reads and `p9`'s virtio-9p device —
each of which is a *different* finding (V-4's remaining half, and the PCI-config and 9p doors),
with its own step. V-1 was the downcast cause and it is the one that is closed.

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

#### 11.1.3 — V-3: the scheduler does other subsystems' cleanup (severity: high) — **MOSTLY CLOSED**

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

#### 11.1.4 — V-4: the terminal emulator and console live inside the VFS (severity: medium) — **CLOSED** (the `vfs ↔ console` pair is broken; emulator + VC vocabulary + `vtmode` + active-console number in the `term` leaf; see §11.2 Step D)

`crates/term/` (the `mpos-term` crate) holds the cell grid, the key handling and an embedded 4 KiB `vga8x16.bin` font.
`src/vfs/tty.rs` (2 359 lines), `src/vfs/pty.rs`, `src/vfs/devpts.rs` and `src/vfs/vcmux.rs`
implement the console, and `src/console.rs` — the kernel's own logging console — reaches *into*
the filesystem layer to draw:

```rust
crate::term::with_screen(active_grid(), |g| g.feed(bytes));
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

The re-audit at the AF_PACKET/sound landing measured the inversion's bulk differently from how
it was first described: most of the ~110 below-`syscall` references were not the two helpers at
all but the **errno constants** — `net/*` (100+), `sched/*` (17), `vfs/epoll.rs` /
`vfs/timerfd.rs`, `drivers/virtio/sound.rs` (7) and `uevent.rs` each reaching up for a minus
sign. That half is now **closed**: the table lives at the crate root as `crate::err`
(`src/err.rs`, re-exported through `syscall` so the layer's own paths are unchanged), and
`uevent`'s second upward edge — `self_cred` reading the task table — moved to
`net::socket::task_identity`, an allowed `net→sched` edge, with `uevent::bind` taking the
preferred port id as a parameter so the bus stays host-testable. What *remains* of V-5 and V-9
is exactly `buffer_ok`/`err_of`: 24 `err_of` calls in `net/socket.rs`/`net/unix.rs` and 2
`buffer_ok` calls in `drivers/drm.rs` plus `net/*`'s pointer checks. B3 below is that move,
and it is now the whole of what those violations are.

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

#### 11.1.10 — V-11: a host test module behind a never-host-compiled parent (severity: low) — **DONE**

`src/lib.rs` gates `pub mod net` on `target_os = "none"`, so the module is absent from every
host build — while `src/net/socket.rs` annotated a test module
`#[cfg(all(test, not(target_os = "none")))]`, i.e. "run me on the host". The annotation was only
ever evaluated in a build that did not include the file, so those three tests had never run.
`net`'s doc comment says its tests "are the conformance programs that run on the machine",
which is true of the rest of `net` and was false of this file.

**Settled by Step F, and the reading that settled it was of what the three tests pinned rather
than of how to make them run.** They were `send`/`recv` on a `socketpair`, the
`sendmsg`/`recvmsg` shapes on the same channel, and its two refusals (`EISCONN` for a
destination, `EAGAIN` for `MSG_DONTWAIT` on an empty one) — and every one of those is pinned,
line for line and in more detail, by `user/tests/src/bin/t-sockmsg.rs`, a program the sweep
*does* boot (its own header names the caller it exists for: sudo's monitor channel). The module
is deleted, the pins are where they always actually were, and the criterion and its mechanical
check are §11.2 Step F's below.

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

#### Step B — Give devices a seam the VFS can hold without naming them (V-1, V-2, V-5) — **DONE (B1, B2, B3)**

The largest step, and the one that pays for the others: it removes the largest cause of the
23-cycle count.

**B1 — Replace the nine `as_*` downcasts with one type-erased handle. — DONE.** The plan offered
a `vfs::DevNode` carrying an `Arc<dyn DevOps>` with `read`/`write`/`ioctl(cmd, arg)`, installed at
boot, or a single `as_any`/`downcast_ref` pair over `core::any::Any`, and said either was fine
because "the point is that `vfs` stops naming the types". **The second shape was taken, and the
reason is worth recording because the first reads like the better design.** `DevOps` does not fit
the nine it had to replace. Six of them are device downcasts the syscall layer uses to reach an
ioctl *the device already owns* (B2 gave every family its own `ioctl`), and the other three are
`net` recovering its own `UnixSock`/`VsockSock`/`PacketSock` from an `Inode` it minted itself —
not an ioctl question at all, so a `DevOps` covering them would have to be a god-trait. Worse,
`dev().ioctl(...)` *reorders the dispatcher*: every device would have to answer `ENOTTY` for every
family it does not own, instead of the dispatcher routing by family first. That is the plan's own
named top risk, "this touches every device path at once", and it buys nothing the downcast does
not.

The shape actually taken, in `src/vfs/mod.rs`:

```rust
impl dyn Inode {
    pub fn as_any(&self) -> &dyn core::any::Any { self }
}
pub trait Inode: Send + Sync + core::any::Any { ... }
```

Two properties make it nearly free. `Any` is implemented for every `'static` type already, so
none of the tree's 52 `impl Inode for` blocks grew a method — and it could not have been a default
body on the trait, because coercing `&Self` to a trait object needs `Self: Sized`, which a `dyn
Inode` is not. And the body is a plain trait upcast (stable since Rust 1.86), so it is the
vtable's own supertrait entry rather than a lookup. Call sites read
`inode.as_any().downcast_ref::<FbDev>()`, which answers `Some` exactly where the `as_*` method it
replaced answered `Some` — the same `TypeId` comparison the old override (`Some(self)`) and
default (`None`) made, just moved from the trait into `core`.

Migration order followed the plan — one device per commit, fbdev first and DRM last — which is
also the order of the seven commits: fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the
DRM card's three node types (`as_drm`, `as_syncobj_fd`, `as_sync_file`) together, since they are
one device. Each commit deleted its `as_*` method and its `impl` overrides, so the ratchet fell
as it went: `vfs/mod.rs -> drivers|net` 9 → 8 → 7 → 6 → 5 → 4 → 3 → 0, and the constant in
`xtask/src/depgraph.rs` is now 0. Every commit was verified with a bare-metal build, fmt, clippy on
both targets, and the full gate; five of the seven have their device's conformance program in the
default sweep (`t-fbdev`; `t-evdev`/`t-ioctl`; `t-unix`/`t-unix-dgram`/`t-unix-scm`; `t-drm`/
`t-drm-args`/`t-drm-lease`/`t-drm-sync`; `t-prime`). Two do not: AF_PACKET's is `t-rawicmp`, which
`tests.toml` puts in `external`, and was run by name (`--groups external`, green); AF_VSOCK has
**no** conformance program at all — a test boot deliberately gets no host vsock bus — so its
guarantee is the type check plus the equivalence above, and saying so is better than implying a
test ran.

**Acceptance — met.** `crate::drivers::` and `crate::net::` do not appear in `src/vfs/mod.rs`;
`cargo xtask depgraph` prints `vfs/mod.rs -> drivers|net 0/0` and `Within the ratchet: no edge has
appeared and no count has grown.`; and the full gate on the finished tree (2026-10-02, after all
seven device commits) is:

```
Unit tests: the kernel and its leaf crates (host)   528 passed (kernel) + 161 in the twelve
                                                   leaf crates + 47 xtask = 736 passed,
                                                   0 failed
self-test guest: === SUMMARY pass=140 fail=0 (140 built)
self-test guest: 10 of 150 user tests left out by tests.toml
self-test guest: exit 33 (33 is the clean debug exit)
```

Two of the original criteria are **not** met, deliberately: `wc -l src/syscall/linux/fs.rs` did not
drop from B1 (it *rose*, by the imports that now name the device types — the 212-line drop was
B2's, and a downcast is not a line count), and the mutual pair count stayed 18 *then* (D.2a later
took it to 17 — see Step D), because `drivers`
and `net` still name `vfs` and `vfs` still names them from `vcmux`/`procfs`/`sysfs`/`p9`. Those
references are V-4's and the PCI-config/9p doors' — different findings, different steps; B1 closed
the downcast cause, which is what V-1 was.

**B2 — Move each device's ioctl contract down to its driver.** For each family in 11.1.2
(fbdev, DRM, RTC, uinput, tty, evdev, block), move the request-number constants, struct layouts
and validation into the driver module as `pub fn ioctl(req, arg, from_user) -> isize` (or a small
table), leaving `syscall/linux/fs.rs` with the dispatch to that entry point. The *pure* parts stay
host-compiled and keep their tests; `drivers/drm.rs` already carries most of the DRM UAPI, so it
is closer to this shape than it looks.

**B3 — Move the user-pointer helper out of `syscall`.** `buffer_ok`/`err_of` answer "is this range
readable/writable by the current task", which is an `mm` question. Add
`mm::user::{validate, copy_in, copy_out}` and have `syscall`, `net` and `drivers` all call that.
This deletes V-5 and V-9 at once. (Done ahead of it, at the AF_PACKET/sound landing: the *other*
thing below `syscall` was after — the errno table — moved out to `crate::err` (§11.1.5), and
`uevent`'s task-table reach went with it (V-14). What B3 owns now is exactly `buffer_ok`/`err_of`.)
**LANDED (this pass) — V-9 closed.** The two helpers moved to a crate-root leaf `src/uaccess.rs`
(`buffer_ok`/`user_accessible`/`MAX_IO` plus the machine-gated `err_of`), mirroring how `err` was
relocated. The shape is `crate::uaccess`, not `mm::user`: `err_of` maps a `VfsError` (a `vfs` type)
and `buffer_ok` walks `arch::paging` — neither is an `mm` concept — and a root leaf is what lets any
layer below `syscall` reach it without naming the ABI layer. The 99 below-`syscall` call-sites in
`net/{socket,unix,packet,vsock}.rs` and `drivers/drm.rs` now name `crate::uaccess::*`; `syscall`
re-exports the same names for its own handlers, so its ~350 internal sites and `CallerScan` are
unchanged. V-5's `drm` half is closed the same way (the `sound.rs`→`syscall::linux::abi` edge is the
V-2 ioctl-ABI concern, not B3's). This is the prerequisite for B2 (kd/evdev 接受-拒绝下沉到驱动), now
unblocked. Verified: bare-metal build green + host suite `570 passed` + bare-metal clippy clean; the
`cfg(target_os = "none")` split of `err_of`/`user_accessible` moved intact, applying V-11's lesson.
**B2 (evdev 下沉到驱动) — attempted, destabilized by a latent DMA bug, root-caused, then LANDED.** Sinking
`evdev_ioctl` into `EvdevNode::ioctl` (the input driver, mirroring the `'d`→`drivers::drm::drm_ioctl`
precedent, with fd-resolution kept at the syscall boundary) was byte-for-byte the same logic and
compiled for both targets, yet the guest panicked *reproducibly* mid-suite: the `assert!` in
`mm::virt_to_phys` (`src/mm/mod.rs:97`) fired on a user address handed to it from a `write`-path
caller (`virt_to_phys(0x3f…): not an address from the direct map`), aborting before the run ended
cleanly. Reverting the move restored `=== SUMMARY pass=135 fail=1` with no panic. A pure,
behaviour-preserving relocation changes no ioctl answer, so this is a **latent memory bug** *exposed*
by the code-layout change, not an evdev-logic defect — and the caller is now *located*, not guessed:
of `virt_to_phys`'s four live call sites, the one that runs on a `write` (nr 1) path with a buffer
pointer is the virtio DMA helper `drivers/virtio/hal.rs:59` `virt_to_phys(buffer.as_ptr())` (the
framebuffer callers are GPU-only, `free_frames` gets a direct-map address back); i.e. the virtio-net
TX / sound DMA path handing `virt_to_phys` a buffer that is not in the direct map — the *same*
net/audio area as the in-flight `t-dhcp`/`t-dsp` reds and the user's active `net/*` + `sound.rs`
work. **Resolved the same session:** the sound driver's wrong PCI id (`0x1055`→`0x1059`) and its
user-address DMA hand-off were fixed, the gate reached `=== SUMMARY pass=136 fail=0`, and the evdev
relocation was re-applied on that clean tree — bare-metal build green, host suite `570 passed`, and
**guest `pass=136 fail=0` with `t-evdev`/`t-ioctl`/`t-seat` all green**, proving `EvdevNode::ioctl`
answers identically to the old switchboard. So B2-evdev **LANDED**: the `'E`-typed ioctl serve is now
owned by the input driver, fd-resolution kept at the syscall boundary (mirroring `'d`→`drm`).
**B2-kd also LANDED:** the KD accept/reject serve moved to `Tty::kd_ioctl` (the console device in
`vfs/tty.rs`), with fd→tty→vc resolution and the DRM `card_grabbed_by_current()` check kept at the
syscall boundary and `owns_card` handed *down* as an argument. That is what dissolves the earlier
"gated behind V-4" worry I recorded here: because the card answer comes in as a parameter,
`vfs::tty` never names `drivers`, so no forbidden `vfs→drivers` edge is created, and the KD decode
leaves the switchboard (V-2) exactly as evdev's did. `request_arg` (its only user) went with it.
Verified **guest `pass=136 fail=0`** with `t-ioctl`/`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query`
all green; an intermediate `pass=134 fail=2` was the `t-netlat`/`t-times` timing-flake pair (both
passed on the re-run — load flakes, not the move). The `'V'` family (`vt_ioctl`) still sits in `fs.rs`:
it too reads live console state in `vfs/vcmux`, and would follow the same hand-off shape; left as-is.
The lesson stands: a behaviour-preserving relocation can only be trusted once the gate is green on the
*current* tree, since a layout shift unmasks unrelated latent bugs.

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

#### Step C — Let subsystems own their per-open-file resources (V-3) — **DONE**

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

**Done, and it took the first mutual pair out of the graph.** The registry is
`src/sched/exit.rs`: an append-only list of `fn(&[Arc<File>], owner: u32)` hooks, walked from
`runtime.rs`'s exit path before the batch is parked for dropping (the reference counts still say
which description is the last one until then, which is what `vfs::file::for_each_last` reads).
Registration happens in `main.rs`, in the order the resources have to go, because the order the
list is walked in *is* the registration order: locks first (a lock is keyed by `(device, inode)`
and the inode is reached *through* a descriptor), then the state keyed by the open file
description — the socket, the inotify watch, the evdev reader position, and the uinput device a
program made for itself. `main.rs` is the wiring, so it is allowed to know all five; the
scheduler knows none of them.

Each subsystem's hook now takes the same two things the scheduler can offer. Two of them had to
say so in their signature rather than being wrapped — `net::socket::close_sockets_at_exit` was
`pub(crate)`, which a binary crate cannot see, and is `pub` now — and `vfs::locks` gained
`release_on_process_exit`, which is the loop that used to sit in the teardown: *which of these
descriptors names an inode this process still holds a lock on* is a locking question, and
`vfs::mount::fs_id_of_inode` is a vfs one.

**Measured, and the number that matters.** `sched → net` is **gone** (1 → 0), which is what
removed the `net ↔ sched` pair: `net → sched` is 114 references of ordinary parking and stays,
but a one-way edge is not a cycle. Mutual pairs 20 → **19**, and the ratchet constant moved down
with it — a reduction is a commit that lowers the baseline, which is what makes the ratchet read
as progress rather than as a fence. `sched → vfs` also fell 26 → 22 (locks, mount and inotify),
and `sched → drivers` 4 → 2.

**The acceptance is met on the exit path and not on the exec path, deliberately.** The two
remaining `crate::drivers::` references in `runtime.rs` are on `execve`'s `FD_CLOEXEC` pass,
which tells the input drivers to forget a reader position keyed by an address the new image will
reuse. That is a *different event* with a *different* pair of subsystems to tell, and Step C's
own text says the exec path should decide per edge rather than be abstracted for symmetry — so
the `drivers ↔ sched` pair stands, named in `depgraph.rs` beside the rule, rather than being
made to disappear behind a general mechanism that would run the socket, watch and lock hooks on
an event that is not a process ending. The three `crate::vfs::(inotify|locks)` references the
acceptance names are gone.

**Pinned by the registry's own host test** (`hooks_run_in_registration_order_and_an_empty_list_is_not_a_special_case`):
two hooks that record what they were handed, asserted to run in registration order with both the
batch and the owner, plus the empty list being a no-op rather than a case the teardown has to ask
about. It is one test rather than two on purpose — the registry and the recorder are both
process-global, and the first draft's second test failed for exactly that reason. The end-to-end
gate stayed the guest's: `t-inotify`, `t-evdev`, `t-filelock` and `t-unix-scm` all report
`=== PASS`, and the whole sweep after these Phase-11 changes is `=== SUMMARY pass=140 fail=0
(140 built)` with QEMU exit `33`.

---

#### Step D — Move the terminal emulator out of the VFS (V-4) — **CLOSED** (emulator + VC vocabulary + `vtmode` + the active-console number all in the `term` leaf; the `vfs ↔ console` pair broken by D.2a; D.2b evaluated and rejected)

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

**Half of it has landed, and the half that landed is the half that was a crate.** Step E's pattern
applied to the emulator: `src/vfs/term.rs` and `src/vfs/term/` (blit, cursor, font, keymap and the
4 KiB `vga8x16.bin`) are `crates/term` — `mpos-term`, a leaf that cannot name the kernel — and
`src/lib.rs` re-exports it as `crate::term`. That is the plan's `src/term/` with the enforcement
added, and it is the acceptance criterion in the form it can be checked in: `crate::vfs::term` no
longer exists anywhere, `vfs/` holds no font asset and no escape-sequence parser, and the three
`vfs/term` test modules moved with the code and run in the new crate (32 pins, unchanged).

The one place this differs from the plan's own sketch is *who moves*: the plan said `console`
"then depends on `term`", and the extraction made that literal — `console.rs` (3 references),
`drivers/video/render.rs` (3) and `drivers/virtio/input.rs` (2) now name `crate::term`, which is
what takes 3 references off `console → vfs` (4 → 1) and 5 off the largest single edge in the tree
(`drivers → vfs`, measured 71 by the ratchet afterwards). `vfs::tty` reaches for the same crate,
which is right: the line discipline and the grid must agree about the cursor (§7.1b's whole
finding is that they must not each keep one).

**What remains is the mux, and it is not mechanical.** `vcmux.rs` looks like the same shape and is
not: it reaches *out* to `console::write_bytes`/`vc_output`/`grids_live`, to
`sched::{spawn_permanent, block_until_deadline, wake, send_signal_process}` and to
`drivers::{video::render, drm::master_held}`. Moving the file to `console/` would exchange a
`vfs → console` edge for a `console → vfs` one and leave the pair where it was; what had to move
first is the shared vocabulary — the console count and the KD mode words are console facts that
`pty`/`tty`/`vtmode` happened to define.

**D.1 — the shared vocabulary moved. LANDED (this pass, green).** `VC_COUNT` (was
`vfs::pty::VC_COUNT`) and the `KD_TEXT`/`KD_GRAPHICS`/`K_RAW`/`K_XLATE`/`K_MEDIUMRAW`/`K_UNICODE`/
`K_OFF` mode words (were `vfs::tty::*`) now live in `crates/term/src/vc.rs` as `crate::term::vc::*`.
`term` is a leaf that `vfs`, `console` and the drivers already depend on, so naming it is downward
for all of them — this is the prerequisite the paragraph above named, and it is what lets the next
sub-commit move the `vcmux` *file* without re-creating the pair. A pure relocation: `drivers/video`,
`drivers/devices` and `drivers/virtio/input` stop naming `vfs::pty`/`vfs::tty` for these constants;
`vfs::{tty,pty,vcmux}` name `crate::term::vc` instead. Verified behaviour-preserving — bare-metal
build green, host suite unchanged at `528 passed`, `cargo xtask depgraph` reports "no edge appeared
and no count has grown" (`crate::term` stays out of the mutual-pair set, and `vfs/mod.rs →
drivers|net` stays 0), and the full gate ends `=== SUMMARY pass=140 fail=0`, exit **33**, with
`t-seat`/`t-tty-eintr`/`t-tty-vtime`/`t-tty-query`/`t-ioctl`/`t-drm-lease` (the programs that read
the KD/KD-mode state through the moved path) all `=== PASS`.

**D.1b — the handover table moved too. LANDED (this pass, green).** `vfs::vtmode` (the pure
`VT_AUTO`/`VT_PROCESS` release/decision table — `VtMode`, `Decision`, `Ack`, `Promise`, `Signal`,
`Release`, `Table`, `RELEASE_TIMEOUT_TICKS`) is a self-contained state machine: it named no kernel
module at all (only `alloc::vec`), so like the `evdev`/`uinput`/`term` extractions before it it is a
leaf lifted out so a host build can argue with it. It now lives at `crates/term/src/vtmode.rs` as
`crate::term::vtmode` — the same leaf that holds the VC vocabulary, since `term`'s own header claims
"which of the six virtual consoles is active" as its business. `vfs::vcmux` and `vfs::tty` name it
there; `vfs/mod.rs` no longer declares it. Host coverage preserved: the 17 `vtmode` tests moved from
the kernel's `-p mpos` run (528 → 511) into `-p mpos-term` (32 → 49), and `cargo xtask depgraph` still
reports no edge appeared and no count grew (`vfs/mod.rs → drivers|net` stays 0; `term` is out of the
mutual-pair set).

**D.2a — the pair is broken. LANDED (this pass, green), and the premise corrected.** The plan (and
the paragraph above) assumed the pair stood on the mux's `vfs → console`/`vfs → drivers` *calls*. The
measurement says otherwise: a mutual pair collapses the moment *either* direction hits zero
(`xtask/src/depgraph.rs:104-110`), and `console → vfs` was a **single** reference — `src/console.rs`'s
`active_grid()` reading `crate::vfs::vcmux::active()` for "which console is on screen." So the cheap
inversion was not the mux's outbound calls at all; it was that one inbound read. The active-console
number — a lone `AtomicU32` — moved from `vfs::vcmux` to the `term` leaf (`crate::term::vc::active`/
`set_active`, which already declares "which of the six virtual consoles is active" its business, like
D.1/D.1b before it). `console.rs` and the drivers now read `crate::term::vc::active`; the mux writes
it there and reads it there too. `console → vfs` = 0, the pair collapsed to a one-way `vfs → console`,
and **mutual pairs fell 18 → 17** — the first number Phase 11 has lowered since Step C, ratcheted by
`BASELINE_MUTUAL_PAIRS = 17`. Behaviour-preserving: bare-metal build green, host suites unchanged
(`511` kernel + `49` term + `47` xtask), `cargo xtask depgraph` reports `console ↔ vfs` gone and no
new edge, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit 33, with `t-seat`/`t-tty-*`/
`t-drm`/`t-fbdev` (the programs that read and switch the active console through the moved state) all
PASS.

**D.2b — evaluated and REJECTED. Step D CLOSED at its acceptance criterion.** The remaining item was
relocating the `vcmux.rs` *file* out of `vfs` (into `console`), inverting its outbound `vfs → console`
(`write_bytes`/`vc_output`/`grids_live`) and `vfs → drivers` (`video::render::notify`/`drm::master_held`)
calls in the hand-down/`Waker` shape Step B2 established. Measuring it this pass showed no sound,
valuable landing, so it is not done:

- **The move would regress the layering.** `console` is a designed leaf — `AGENTS.md` §4 lets it name
  only `arch`, `boot` and `term`. `vcmux` names `vfs`(`pty`/`tty`), `drivers`(`drm`/`video`) and
  `sched`; relocating it to `console` turns `console` into a mid-layer that depends on all three, the
  opposite of V-4. A bare move first *re-creates* `console → vfs` from the mux's 7 `pty`/`tty` calls
  (`lookup`/`pairs`/`master_ready`/`take_master` and `tty::global().receive_byte`), undoing D.2a.
- **A mux-only inversion yields zero ratchet gain.** Current `cargo xtask depgraph` (measured):
  `console ↔ vfs` is **already gone**; `vfs → console` is 8 references but only 5 are in `vcmux` — the
  other 3 are `tty.rs` (2) and `ext4` (1), so the mux cannot zero the edge; and `vfs → drivers` is 17,
  of which only 7 are `vcmux` (the other 10 are `procfs`/`sysfs`/`p9` device-state reads). So the
  surviving `drivers ↔ vfs` mutual pair is not `vcmux`'s to break — it is the procfs/sysfs/9p door, a
  different finding (§11.1).
- **The cost is real:** the change threads boot-registered hooks through the seat/DRM/render pump — the
  subsystem whose "nearly-right" regressions (untagged devices, a dropped pointer) this repo has
  repeatedly paid for — for no measurable improvement.

Per §4 ("refuse rather than approximate", "high cohesion — locate the module that owns the concept"),
Step D is **closed**: the dependency cycle it targeted (`vfs ↔ console`) is broken, the terminal
emulator, VC vocabulary, `vtmode` and the active-console number all live in the `term` leaf, and the
`vcmux` file stays in `vfs` where its pty/tty orchestration coherently belongs. The residual `drivers ↔
vfs` pair is recorded against its own finding, not against Step D.

---

#### Step E — Split `fs.rs` along its real seams (follow-up to B2) — **LANDED**

After B2 the file should be much smaller. What remains is still two jobs: the path-carrying calls
(`open`, `stat`, `getdents64`, …) and the device-ioctl entry point. Split into
`syscall/linux/file.rs` and `syscall/linux/dev.rs`, or leave it once B2 has removed the
switchboard — whichever the size says.

**Acceptance:** the dispatch routing counts are unchanged, no handler changed meaning, host tests
unchanged.

**Landed.** The size said split (B2 shrank the ioctl arms but the file was still 6833 lines). It went
to a directory module `src/syscall/linux/fs/` rather than two top-level files, because that is the
shape that keeps `syscall/linux/mod.rs`'s ~95 `fs::<handler>` references byte-for-byte identical — the
acceptance criterion in the form it can be checked: `fs/mod.rs` holds the module header, the one helper
both halves call (`sync_nonblocking`), and re-exports the entry points (`pub(super) use file::*;
pub(super) use ioctl::*;`), so the dispatch still names `fs::handle_read`, `fs::handle_ioctl`, … with no
change upstream. `fs/file.rs` (5523 lines) is the path-carrying handlers and their private helpers
(`Xattr`, `base_of`, `record_lock`/`lock_take` with both cfg bodies moved together, the `flock_off`
submodule); `fs/ioctl.rs` (1281 lines) is `handle_ioctl` and the contiguous per-family routers
(`family_nr`/`vt_ioctl`/`kd_ioctl`/`fs_flags_ioctl`/`uinput_ioctl`/`evdev_ioctl`/`ifreq_index`/
`ifreq_flags`). The measurement showed the seam is unusually clean: the entire device-ioctl footprint is
one contiguous region, and exactly one helper crosses it. `fs` is machine-only
(`#[cfg(target_os = "none")] pub mod fs`), so the kernel host build never compiles it — which is why
this is verified by the bare-metal build and the QEMU gate rather than the host suite. Pure relocation:
`cargo build --target x86_64-unknown-none` green with clippy back to its 29-warning baseline (no new
lint), `cargo xtask depgraph` unchanged (mutual pairs 17/17 — `fs` lives inside `syscall`, so no module
edge moves), host suite unchanged at 511, and the full gate ends `=== SUMMARY pass=140 fail=0`, exit
**33** — the path/file and every ioctl-family conformance program (`t-ioctl`, `t-drm`, `t-fbdev`,
`t-tty-*`, `t-seat`, `t-hwclock`, …) pass on the relocated handlers.


---

#### Step F — Make the target gate a rule, not a habit (V-11 + the `ARCHITECTURE.md` §8 drift) — **DONE**

*The one step in this phase that is pure rule-making: it changes no behaviour and removes no
cycle, and it exists because the gate's failures are silent.*

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

**Done, and the three answers the step asked for.**

*Which way `net` went, and why it is the cheap way rather than the lazy one.* The plan offered
"host-compile it, or delete the module and say so" as if they were equivalents; they are not, and
the tie-breaker was coverage rather than effort. Making `net` host-compilable means a device seam
for `drivers::virtio::net::VirtioNet` and a clock seam for `arch::timer` (10 machine references
across `net/mod.rs`, `net/socket.rs` and `net/vsock.rs`, plus `smoltcp` moved out from under the
`target_os = "none"` dependency) — real Phase-11 B work, not Step F work. The three tests, read
instead of assumed, turned out to be `t-sockmsg`'s subject: `user/tests/src/bin/t-sockmsg.rs`
pins `send`/`recv` both ways, the `sendmsg`/`recvmsg` gather and its "no address, no control
data, no flags" answer, `MSG_DONTWAIT` on an empty pair as `EAGAIN`, and a destination on a
connected pair as `EISCONN` — the same four claims, in a program that boots. So the module is
deleted and §11.1.10 records where its coverage lives; a comment at the end of `net/socket.rs`
says the same to the next reader, and names the check that keeps a fourth from being written.

*The criterion is written down in one place and named from the others*: `src/lib.rs`'s header
carries it (with the transitive half spelled out and the two ways it fails silently), `AGENTS.md`
§4 points at it and keeps its "when you gate a module, check what gates its tests" instruction,
and `ARCHITECTURE.md` §8 states it and records V-11's closure.

*The check is `xtask/src/gates.rs`, and it is pinned against itself.* `dead_host_tests` walks
`src/lib.rs`'s `mod` declarations, resolves each to `dir/name.rs` or `dir/name/mod.rs` the way
the compiler does, and carries the gate down — so a file under an ungated-but-gated-child module
is caught as well as one declared gated directly. It is run at the head of `cargo xtask test`,
so the failure is a named error rather than a test count nobody reads, and its four unit tests
include the counterfactual this kind of checker needs: a two-file tree with the *same*
`#[cfg(all(test, not(target_os = "none")))]` in one gated and one ungated file, where the first
must be reported and the second must not. Two more pin the gate arithmetic — the ungated sibling,
and the boot handoff's own `#[cfg(all(any(target_os = "none", test), target_arch = "x86_64"))]`,
which names the host by way of `test` and so must *not* read as machine-only.

*Verified, in the counterfactual order the plan asked for:* with the check in place and the module
still present, `cargo test -p mpos-xtask` failed on exactly one violation — `src/net/socket.rs`,
gated by `crate::net::socket`; after the deletion the same command is `42 passed`, the bare-metal
build is green with the same single pre-existing warning, and the host kernel suite is unchanged.

---

#### Step G — Keep the graph from regressing — **DONE**

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

**Done — and one correction to the method, which is the interesting part.** The ratchet is
`xtask/src/depgraph.rs`, reachable as `cargo xtask depgraph` and also run as a host test, so it is
inside `cargo xtask test` rather than beside it. It measures the tree the way §11.0's script does
— every file under `src`, each mapped to its top-level module, every `crate::<mod>` reference an
edge — with **one deliberate difference: `//` comments are stripped before matching.** That was
not a tidy-up; it is what makes the number mean "who names whom". §11.0 matches text, which is why
the audit it published counted three edges that are prose: `uaccess → syscall` is a doc comment
(and `vfs → uaccess` with it), and the third pair `arch ↔ vfs` came from a file the compiler never
saw at all (§11.1.7's deleted `arch/x86_64/serial.rs`). Measured both ways on this tree: **23**
mutual pairs by text, **20** in code. The ratchet holds the code number, because a rule that fires
on a sentence is a rule people learn to word around.

**The rules are ratchets rather than absolute refusals, and that is a statement about where in
the plan this landed.** Steps A, E and F are done and B/C are not, so two of the three rules are
*not yet* zero, and a check born red is one nobody reads. Each constant names the call sites that
keep it non-zero and the step that removes them:

| Rule | Measured 2026-10-02 | What kept it there | Removed by |
|---|---|---|---|
| lower layer → `syscall` | `drivers` 1, `sched` 10, the rest 0 | `drivers/virtio/sound.rs`'s one `use crate::syscall::linux::abi` (V-5's remainder); `sched/runtime.rs`'s `crate::syscall::linux::{abi, nr}`, `MMAP_BASE` and `DEFAULT_UMASK` | B2 (the ioctl ABI), B/C (the mapping base and the umask are scheduler state) |
| `vfs/mod.rs` → devices | 9 | the nine `Inode::as_*` downcasts (V-1) | **Done** (B1): 0 |
| mutual pairs | 20 | the whole `vfs ↔ sched ↔ syscall ↔ net ↔ drivers` cluster | B, C, D |

`arch` and `main` are deliberately **not** in the first rule: the `syscall` gate exists to enter
the dispatcher and `main` is the wiring, so both edges are the design. A layer with a baseline of
zero — `net`, `blk`, `pci`, `rand`, `uevent`, `vfs` — fails on its *first* reference, which is
where the ratchet earns its name.

Four unit tests pin the instrument itself, because a measurement that cannot tell two shapes apart
agrees with whatever it is pointed at (§12.1 A8's lesson, in a different currency): a reference in
a doc comment is not an edge; a cycle of two is reported once and a one-way edge is not a pair; a
service naming `syscall` with no baseline fails; and the `vfs/mod.rs` rule is silent at its
recorded number and fails one past it. The check has never been red on this tree, and it has been
shown able to go red on each of the three rules.

---

### 11.3 Order, and what is deliberately not in this plan

**Do this, in this order:**

1. ~~**V-12** — finish or revert the in-flight refactor.~~ **Done**: the tree builds and the
   whole gate is green (§11.1.12), so everything below is now verifiable.
2. ~~**Step A** — pure subtraction, ~5 files, one commit, no behaviour change.~~ **Done**
   (§11.2 Step A): the three dead modules, the `[msgtrace]` trace and the aarch64 profile are
   gone, and the pair count it was measured with fell from 22 to 21.
3. ~~**Step B** — the device seam, one device at a time.~~ **Done** (§11.2 Step B): B3 moved the
   user-pointer helpers to the `uaccess` leaf, B2 put every device family's ioctl contract next to
   the state it answers from, and B1 (fbdev, evdev, uinput, AF_PACKET, AF_VSOCK, AF_UNIX, then the
   DRM family last) deleted the nine `Inode::as_*` downcasts, taking `vfs/mod.rs → drivers|net`
   from 9 to 0. The pair count did not move with it — that is V-4's and the PCI-config/9p doors'
   share of the cluster, not the downcasts'.
4. ~~**Step C** — the resource registry.~~ **Done** (§11.2 Step C): the exit hooks live in
   `sched::exit`, registered by `main.rs`, and the `net ↔ sched` pair is gone.
5. ~~**Step D** — the terminal emulator move.~~ **Done** (§11.2 Step D): D.1/D.1b/D.2a moved the VC
   vocabulary, the pure `vtmode` table and the active-console number into `crate::term`, collapsing the
   `vfs ↔ console` mutual pair — **mutual pairs 18 → 17**, the first ratchet drop since Step C; gate
   green `pass=140 fail=0`. **D.2b (relocate the `vcmux` file to `console`) was measured and rejected**
   — `console` is a leaf the mux's `vfs`/`drivers`/`sched` naming would violate, and a mux-only
   inversion moves no ratchet (`vfs → console` and `vfs → drivers` each have non-`vcmux` callers). Step
   D's dependency cycle is closed; the surviving `drivers ↔ vfs` pair is tracked as its own finding.
6. ~~**Step E** — the `fs.rs` split, if B2 has not already made it unnecessary.~~ **Done** (§11.2 Step E):
   the file was still 6833 lines, so it split along the path/file ↔ device-ioctl seam into
   `src/syscall/linux/fs/{mod,file,ioctl}.rs`; `mod.rs` re-exports the entry points so `mod.rs`'s
   `fs::<handler>` dispatch is unchanged. Gate green `pass=140 fail=0`, depgraph unchanged (17/17).
7. ~~**Step F** — the gate criterion and the check for it.~~ **Done** (§11.2 Step F): the
   criterion is in `src/lib.rs`/`AGENTS.md`, the dead test module is deleted with its coverage
   named, and `xtask/src/gates.rs` fails the sweep on the shape.
8. ~~**Step G** — the ratchet.~~ **Done** (§11.2 Step G): `cargo xtask depgraph` measures the
   graph with comments stripped and holds three recorded baselines; it is also a host test, so
   `cargo xtask test` runs it.

Steps A, E, F and G were mechanical (a day each) and landed; B, C and D needed the device/scheduler/console
paths and have also landed (D's remaining D.2b file-move was measured and rejected as a layering
regression with no ratchet value — §11.2). **Phase 11 is closed except one surviving mutual pair: `drivers
↔ vfs`, which is the `procfs`/`sysfs`/`p9` device-state door (§11.1), not any Step.** Nothing here had to
land before the feature work in Phases 1–10 — but every feature lands *cheaper* after B and C, which is
the argument for having done them first.

**Deliberately not in this plan:**

- **Rewriting the `Inode` interface.** The trait itself is fine; it is the nine device and socket
  methods that hung off it that were wrong. B1 removed them, added one type-erased handle the
  caller names a type through, and stopped there.
- ~~**A generic HAL trait forest.**~~ **Reversed — see §11.4.** What this note rejected was a
  trait per function; §11.4 lands *six* traits for the six hardware concerns the kernel has,
  with the platform tag owned by `hal` so the seam adds no dependency cycle. The reasoning
  against indirection still holds and is honoured: the forwards are static, there is no `dyn`
  and no allocation, and a call monomorphises to the direct one it replaced.
- **A message bus or an async runtime.** The kernel is uniprocessor and synchronous by design,
  and `ARCHITECTURE.md` §7's locking rules exist because that is what makes the design legible.
  This phase is about *who names whom*, not about adding indirection.
- **Splitting files because they are long.** `vfs/ext4/mod.rs` (3 391 lines) stays as it is.
- **Restructuring `src/main.rs` (1 625 lines).** The boot sequence, the milestone checks and the
  panic handler do belong in the binary; if it ever becomes worth splitting, the checks move to
  `src/main/checks.rs`, and that is a tidiness commit rather than an architecture one.
- **Fixing V-10.** Accepted, documented, and cheaper to live with than a shared crate across two
  targets would be.

### 11.4 The HAL: a machine seam under `arch/` — **CLOSED (S1–S5 LANDED)**

Phase 11 is "who names whom"; this is the largest remaining piece of it, deferred above until
now. Goal: everything above the machine layer compiles against *capability traits* instead of
`arch::x86_64::*`, so a second architecture is `arch/<arch>/` and nothing else (§9.1/§9.2), and
no architecture-specific type crosses the boundary without a compile error.

**Shape** (`src/hal/mod.rs`): six traits for the six hardware concerns — `Clock`, `WallClock`,
`Cpu` (landed), then `Interrupts`, `Paging`, `Contexts` with associated types — plus the platform
tag `Current` and static one-line forwards. The tag is `hal`'s own type and `arch/<arch>/`
implements the traits *for* it, so the seam is a one-way `arch → hal` arrow and adds no cycle:
`cargo xtask depgraph` measured **17 → 15** mutual pairs on S1 (`ARCHITECTURE.md` §6).

**What it does not abstract, and must not.** The Linux ABI *is* the register layout:
`user_regs_struct`, the signal frame, the `syscall`/`iretq` gate are x86_64 by definition (on
aarch64 the same rows are `x0..x30`/`pc`/`pstate` behind `svc`). That stays in `arch/<arch>/`;
wrapping it would be the private dialect `AGENTS.md` §4 rule 3 forbids. "No leakage" means a
port writes `arch/<arch>/` and touches no shared file — not that a register frame becomes
neutral.

**Stages**, each its own commit and green alone (AGENTS §4 rule 7):

| Stage | Contents | Status |
|---|---|---|
| S1 | `hal` + `Clock`/`WallClock`/`Cpu`/`Current`/forwards; the `arch::timer`/`rtc`/`idle`/`halt` spread rerouted | **landed** — pairs 17 → 15 |
| S2 | `Interrupts` (`Line`, `unmask`/`mask`/`eoi`, `init_message_signalled`, `net_msi_vector`): `serial`, `virtio/net`, `lapic` | **landed** — pairs 15 → 14; gate `pass=142 fail=0` exit 33 |
| S3 | `Paging` (`Space` assoc + `AddressSpaceOps`, `install`/`map_mmio`/`translate`/`user_accessible`/`is_cow_page`/`write_user_u32`/roots/switch/`phys_for_virt_of`); `PageTaken` moves to `mm::vm`; migrates `sched`, `mm`, `uaccess`, `syscall`, `main` | **landed** — pairs 14 → 14 (a swap: `arch ↔ mm` out, `hal ↔ mm` in); gate `pass=142 fail=0` exit 33 |
| S4 | `Contexts`: `Context`/`Fpu`/`UserFrame` as associated types behind `ContextOps`/`FpuOps`/`UserFrameOps`; `sched` and the ABI layer stop naming `arch` | **landed** — pairs 14 → 12 (`arch ↔ sched`, `arch ↔ syscall` gone); gate `pass=142 fail=0` exit 33 |
| S5 | close the door: the depgraph rule is an allow-list (`arch`/`hal`/`main`/`lib`, everything else at 0), and `arch/x86_64`'s submodules are `pub(crate)` behind a four-item facade | **landed** |

S2–S4 touch the trap, page-fault and context-switch paths, so each needs the full QEMU gate,
not host+clippy alone. S1 was pure re-routing — host tests, both clippy targets and the depgraph
were the whole measurement, and its own reds (none) would have been a compile error, not a
machine bug.

**S4's design finding, found by measuring rather than by planning it.** The plan above said the
register-frame knowledge would move into `arch/<arch>`. Measuring what is left showed why only
half of it can: after S1–S3 exactly two modules still name `crate::arch::` — `sched` (17, the
saved `Context`, `FpuState`, `UserContext` and the switch/FPU/bootstrap calls) and
`syscall/linux/proc.rs` (4: `gdt::user_selectors` twice and the `UserContext` literal).

The frame's **constructor** can move down — building a frame from the gate's trap record is a
pure machine fact, and moving it is what removes the `SegmentSelector` from the ABI layer. But
the frame's conversion **into** the Linux `user_regs_struct` array
(`abi::uregs`, `context_regs`/`context_from_regs`/`capture_frame`) cannot: `arch` naming
`crate::syscall::linux::abi` is an `arch → syscall` edge, the one direction this whole design
forbids and the one the depgraph's first rule exists to catch. So the array conversion stays in
the ABI layer, and the frame type must instead expose the operations that layer needs
(`to_regs`/`from_regs` with the array as an associated `Regs` type, `ip`/`sp`/`set_result`,
`forked`). That also means `ContextOps` has to hand out its `Fpu` (`fpu()`/`fpu_mut()`, with
`type Context: ContextOps<Fpu = Self::Fpu>` tying the two associated types together), because
`sched` stores `Context` and pokes its FPU half on every switch.

S4's accepted shape, then: `hal::Contexts` with `Context`/`Fpu`/`UserFrame` as associated types
behind `ContextOps`/`FpuOps`/`UserFrameOps`, the aliases and `FPU_BYTES` as crate-level consts in
`hal` (an associated const in an array length needs `generic_const_exprs`), the frame
*constructor* moved into `arch`, and the ABI array conversion left where the ABI is. That
takes both remaining counts to 0 and is what lets S5 close the modules.

---

## Phase 12: ABI truthfulness — **AUDIT DONE, NOTHING FIXED**

Phase 11 is about *who names whom*: the shape of the tree, with no user-visible defect. This
phase is the other half of the review question every change here gets asked (`AGENTS.md` §7):
*is that Linux's answer, or a shape that looks like it?* A gap that is honestly refused
(`ENOSYS`/`EOPNOTSUPP`/`EINVAL`) is already Linux's answer for a kernel that cannot do the call,
so it is not a defect and takes no work here. What this phase collects is the cases that are
**not** honestly refused — a call or an `ioctl` that is *accepted*, answers success, and then
does less than Linux did. Those are the ones `AGENTS.md` §4 calls the single worst thing this
codebase can do: a program comes to believe something false and stops asking.

The audit method mirrors §11.0 — reproducible, and worth re-running: grep every syscall and
device-`ioctl` arm that returns `0`/`ESUCCESS` and read what it actually did; compare against
the Linux behaviour, which for each entry below is either measured here or marked with its
confidence. Three of the entries (A1, B1, B2) were re-confirmed against the source in the
working tree; the rest are as found by the sweep and each carries the same bar — confirm on a
real Linux before landing a change that could send a program down a fallback it would not
otherwise take.

Two hard truths the audit surfaced, before any individual line:

- **The board is not currently green.** `AGENTS.md` §3 records the working tree's in-flight tty
  read-path rewrite as red (`t-tty-vtime` hangs, `t-tty-eintr` fails two checks, `t-netlat` is a
  load flake). That is a *newer* state than §11.1.12's "green", which described the last
  committed baseline (Step A). **Nothing in this phase is verifiable until the gate is green
  again**, so restoring it is task zero, ahead of B and C.
- **Some of these are recorded as `Done` where they are not.** `PLAN.md`'s own table lists
  `getpriority` as done; it returns a hardcoded 20 with no round-trip (B5). The board being true
  is a standing rule (§8 of `AGENTS.md`), so correcting it is part of this phase, not a
  side effect.

### 12.0 The dependency direction this phase assumes

No new dependency design: the target layering is already recorded in §11.2 and mirrored in
`ARCHITECTURE.md` §9, and this phase works *within* it, not against it. The two rules that
matter for the fixes below are the ones §11.2 states: **nothing below `syscall` names
`syscall`**, and **nothing outside `vfs` names the VFS's concrete device types**. The device-
`ioctl` truthfulness fixes (A2, A3) are the same moves §11 Step B2 makes — a request number's
"accept or refuse" decision belongs with the driver that owns the device, not in the
`syscall/linux/fs.rs` switchboard — so wherever a fix here and a Phase-11 step touch the same
code, land them together rather than editing the switchboard twice.

### 12.1 Group A — silent success on a question Linux refuses

Purely narrowing changes with a clear Linux answer; each is "refuse honestly", the cheapest and
lowest-risk kind, and the most direct violation of §4. Land with a host test and a probe.

| # | Where | Linux's answer | What this kernel does | Fix |
|---|---|---|---|---|
| A1 | `abi.rs` `handle_prctl` catch-all `_ => 0` | unknown option `EINVAL`; `PR_GET_NAME` returns `comm` | **LANDED.** Name pair round-trips `comm`; catch-all now `_ => EINVAL` (Linux's `prctl` `default:`), the old blanket `0` removed. Backing individual unmodelled options with real state is optional future work. |
| A2 | `fs.rs:4362` `kd_ioctl` catch-all `_ => 0` | read-family fills the buffer; unknown `EINVAL`/`ENOTTY` | **LANDED as an honest closed-set refusal (the §4.2 fake removed); per-device serve remains B2.** `KDGKBTYPE` (`0x33`) now writes the constant `KB_101` Linux answers (`fs.rs`, pinned by `t-ioctl`): before this it fell through to the accept and returned `0` with the caller's byte uninitialised — a false-success, not a hardware gap, because its answer is a context-free constant Linux gives whatever is plugged in. **`Enumerating the `K` family (`/usr/include/linux/kd.h`) showed the `_ => 0` accept still caught the READS and the SETTERS alike**: `KDGETLED` (0x31), `KDGKBMETA` (0x62), `KDGKBENT` (0x46), `KDGKBSENT` (0x48), `KDGKBDIACR`/`KDGKBDIACRUC` (0x4A/0xFA), `GIO_*` font/cmap/scattermap — each returning `0` with the caller's `int`/`struct kbentry`/`struct kbdiacrs` left as stack garbage — plus the `KDSETLED`/`KDSKBLED`/`PIO_*`/speaker writes claiming an effect on hardware with no 8042 controller / a compiled-in keymap / a DRM-owned screen. The old catch-all was the single worst §4.2 shape the objective forbids (success-for-nothing, and success-with-garbage for the reads). This pass **replaces `_ => 0` with a closed set** (Linux's own rule): a number inside the `K` command block (`0x2F..=0x71 | 0xFA..=0xFB`) is a real console ioctl this device declines → `EOPNOTSUPP`; a number outside it is not a console ioctl → `ENOTTY`. This is a *refusal*, not a fabricated value — it sidesteps the "guess the byte width / answer EINVAL on a table Linux serves" trap the earlier pass named, because it returns no data at all. | Confirmed safe: no guest program or the boot sequence calls any kd setter/getter except the five handled explicitly (`KDSETMODE`/`KDGETMODE`/`KDSKBMODE`/`KDGKBMODE`/`KDGKBTYPE`), and the full 136-program gate stays green with `t-seat`/`t-drm-lease`/`t-ioctl` all `=== PASS` — so the prior "a seat manager treats a failure here as the console being unusable" premise was, like A3's, unmeasured and wrong (systemd-logind toggles LEDs best-effort and ignores the error). The *structural* move — serving these per-device from the console's live LED/keymap/font state — stays Phase 11 B2 (V-2 ioctl-to-driver) with the measured Linux values recorded in §12.1 A2 so B2 implements rather than guesses. **Residual after this pass: none faking success** — every remaining `K` command is honestly refused; B2 upgrades the refusals to real per-device answers. Pinned by `t-ioctl` (KDGETLED/KDSETLED → `EOPNOTSUPP`, `0x4B80` out-of-block → `ENOTTY`), all counterfactual against the old blanket `0`. |
| A3 | `fs.rs` evdev write requests | `EVIOCREVOKE` revokes access; FF/keycode answered per-device | was: `EVIOCSFF`/`EVIOCSREP`/`EVIOCREVOKE`/`KEYCODE` all return 0 doing nothing — **`EVIOCREVOKE` silently did not revoke** (a security semantic). Now: **`EVIOCREVOKE` implemented for real**; **FF + keycode refused with Linux's measured errno**; **repeat stored + reported per-device** | **`EVIOCREVOKE` LANDED + gated.** `EVIOCREVOKE` **corrected to Linux's actual contract this pass** (authoritative `evdev.c`: `case EVIOCREVOKE: if (p) return -EINVAL; else return evdev_revoke(...)`): the arg must be **NULL** (any non-NULL → `EINVAL`) and the revoke is **one-way** (no restore ioctl; the caller closes/reopens). The prior arm read `arg` as a *value* (`1`=revoke, `0`=**restore**, `>1`→`EINVAL`) - the wrong shape: a correct `ioctl(fd, EVIOCREVOKE, NULL)` (arg `0`) hit the restore branch and did the *opposite* of revoke, and MPOS exposed a reversibility Linux never had. Now `buf != 0` → `EINVAL`, `buf == 0` → revoke; the entry check still lets a revoked fd call it again (idempotent `0`). Per-ofd revoke state still lives device-side (`crates/evdev`/`EvdevNode`), `ENODEV` on read + every other ioctl; `set_revoked(false)` remains a device-internal/host-test capability, not an ioctl. Pinned by host `a_revoked_client_reads_nothing_until_it_is_revoked_back` (device abstraction) and `t-evdev` re-pinned to `EVIOCREVOKE(NULL)`→revoke→read/ioctl `ENODEV`, non-NULL→`EINVAL`, idempotent `0`, stays revoked (no reverse). **FF + KEYCODE LANDED + gated (measured).** The earlier "a refusal could stop `weston`/`libinput`" premise was *unmeasured and wrong*: probing real Linux (`/dev/input/event0` non-FF and `event3` keyboard) shows Linux itself answers `ENOSYS` to `EVIOCSFF`/`EVIOCRMFF` (guarded on `test_bit(EV_FF)`, which no evdev here sets and this kernel has no effect store for) and `EINVAL` to `EVIOCSKEYCODE`/`_V2`/`EVIOCGKEYCODE`/`_V2` on a node with no per-device keycodes array (the console keymap is compiled in, not an input-device array) — so refusing with those errnos *is* Linux's answer for this device shape, and a program that runs on Linux already handles it. `fs.rs` now returns `ENOSYS`/`EINVAL` where it used to return `0` (a §4.2 false success); `EVIOCGEFFECTS` stays `give(&0)` because Linux returns the count `0` even on a no-FF device (measured). **REPEAT LANDED + gated (measured).** `EvdevNode` now carries a `rep_delay`/`rep_period` pair (defaulted to Linux's own `250`/`33`); `EVIOCGREP` reports the device's real `(delay, period)` when it advertises `EV_REP` and `ENOSYS` otherwise, `EVIOCSREP` stores the two ints (buffer validated → `EFAULT` on a bad pointer, as Linux's `get_user` answers) so a set round-trips through a get. The old `give(&[])`/`ENOTTY` left the caller's ints untouched - success for nothing stored. Pinned by `t-evdev`: FF→ENOSYS, keycode set/get→EINVAL, `EV_FF` bitmap empty, and the `EVIOCSREP(600,50)`→`EVIOCGREP`=(600,50) round-trip (or `ENOSYS` where the device lacks `EV_REP`) — all counterfactual, gated `t-evdev === PASS` twice. **Pressed-state reads LANDED + gated (measured).** `EVIOCGKEY` (0x18), `EVIOCGLED` (0x19), `EVIOCGSND` (0x1a), `EVIOCGSW` (0x1b) now write `min(len, bitmap_bytes)` zero bytes (this kernel tracks no pressed state) and **return that count** - Linux's measured contract: KEY bitmap 96, LED/SND/SW 8, identical on a mouse and a keyboard, so it is the global `<TYPE>_MAX` `BITS_TO_LONGS(MAX+1)*8` size, not the device's used codes; a short buffer is filled short. `EVIOCGMTSLOTS` (0x0a) → `EINVAL` (measured: a node with no multi-touch; none here is a multitouch device). All replaced the old `give(&[])` (bare `0`, caller buffer untouched - success for nothing written). Pinned by `t-evdev` (KEY=96 + overwrite-the-sentinel + short-buffer=8 + SW=8 + MTSLOTS=EINVAL, all counterfactual). The evdev `'E'` family is now honest end to end (reads, state, keycode, ff, repeat, grab, revoke, unknown→ENOTTY); the *structural* relocation of the decode into the device file (V-2 "syscall ioctl-switchboard") is the Phase 11 B2 architecture move. **`EVIOCREVOKE` arg-shape verification RESOLVED** (same session): the open question above was settled against authoritative kernel source (`git.kernel.org` plain `evdev.c`) - Linux demands a NULL arg and the revoke is one-way - and MPOS's value-arg/reversible model was indeed a wrong-shape divergence, now corrected and `t-evdev` re-pinned. The host could not measure it directly (session devices grabbed, `/dev/uinput` needs root), so the kernel source - not memory - decided it, per §4.1. |
| A4 | `fs.rs` `handle_signalfd` | `SFD_CLOEXEC` sets `FD_CLOEXEC`; unknown flags `EINVAL` | only `SFD_NONBLOCK` recognised; `SFD_CLOEXEC` dropped, unknown flags accepted silently | honour `SFD_CLOEXEC`, `EINVAL` on the unknown bits. |
| A5 | `proc.rs:2158` `mmap` flags | `MAP_FIXED_NOREPLACE` → `EEXIST` on collision; `MAP_HUGETLB` with no huge pages → `ENOMEM` | unknown/reserved flags ignored; `MAP_FIXED_NOREPLACE` (`0x100000`) treated as a plain hint and re-located on collision — breaks the "this address or fail" contract; `MAP_HUGETLB` silently gives a 4K page | validate the bits; `NOREPLACE` must fail with `EEXIST`; `HUGETLB` must be `ENOMEM` here. |
| A6 | `abi.rs` `rseq` registration | registers per-thread `rseq` state; well-formed registration → `0` | (was) returned `0` for a valid call, wrote none of the `cpu_id`/`node_id`/`mm_cid` fields, no unregister | **LANDED as `ENOSYS`.** The dispatch arm is removed, so `rseq` falls through the canonical `_ => ENOSYS` (§4.2). The old arm was a §4.2 success-for-a-thing-not-done: it answered `0` while writing no field the ABI contract requires. Measured (Linux 7.2 host: `AT_RSEQ_FEATURE_SIZE` 33 / `AT_RSEQ_ALIGN` 64) that glibc registers at length 33, which the old `len != 32` arm `EINVAL`'d anyway — so the accept honoured neither real glibc nor the contract; and the libc this machine runs (musl) never calls `rseq`. `ENOSYS` is the honest "not available" a probing libc falls back from. Pinned by `t-rseq` (re-pinned to `ENOSYS`; the prior pin had encoded the false-success, so per rule 1 the test changed with the measurement). *Closing it for real* = a per-thread registration that writes `cpu_id`/`node_id`/`mm_cid`, maintains them across preemption, and honours `RSEQ_FLAG_UNREGISTER` — a sched feature, named below. |
| A7 | `proc.rs` `PTRACE_SETSIGINFO` → `0` | stores the supplied `siginfo_t` so the tracee receives it verbatim (`si_code`/`si_addr` for a planted `SIGSEGV`) | accepted and discarded for any `pid`: `PTRACE_GETSIGINFO` synthesises a siginfo from the stop, and the tracee's delivered signal is the one the *resume* names, so the old arm read nothing and answered `0` - even for a live-but-untraced pid where Linux answers `ESRCH` | **LANDED as an honest refusal + validation + gated.** Closing it *for real* needs `siginfo_t` carried through the stop/resume (a sched+ptrace feature, its own commit) - the kernel's stop model carries signal+event, not a queued siginfo. So this pass removes the §4.2 fake rather than guessing the queue: the arm now runs Linux's own error precedence - `stop_of(id)` → `ESRCH` for a not-a-stopped-tracee, `buffer_ok(data, 128)` → `EFAULT` for an unbacked struct (Linux copies it first), then `EOPNOTSUPP` for the carry it cannot do. Pinned by `t-ptrace-attach`: `PTRACE_SETSIGINFO` on a live but untraced child is `ESRCH`, **not a silent `0`** (counterfactual - the old `=> 0` answered success for it). Gated `t-ptrace-attach === PASS`. |
| A8 | `fs.rs` `RNDZAPENTCNT`/`RNDCLEARPOOL` (+ `RNDADDENTROPY`) → `0` | gate the mutating entropy ioctls on `CAP_SYS_ADMIN` (`EPERM` otherwise) and, for a privileged clear, do the thing or say so | returned `0` for the write requests while `RNDGETENTCNT` stayed pinned at a fixed 4096 (this kernel's random source is an always-seeded ChaCha20 stream, not a drainable accumulator) | **LANDED + gated (measured).** Probed real Linux on `/dev/urandom`: unprivileged `RNDZAPENTCNT`/`RNDCLEARPOOL`/`RNDADDENTROPY` all answer **`EPERM`** (gated on `CAP_SYS_ADMIN`), and the check precedes the buffer copy. `fs.rs` now (1) returns `EPERM` before touching the buffer when `euid != 0`, matching Linux exactly for the unprivileged path; (2) for a privileged `RNDADDENTROPY`/`RNDADDTOENTCNT` keeps `0` - honest, because the pool is already full so a credit is a genuine no-op the observable "entropy available" state does not contradict; (3) for a privileged `RNDZAPENTCNT`/`RNDCLEARPOOL` returns **`EOPNOTSUPP`** - there is no drainable count to zero, so §4.4 refuses rather than the old `0` that claimed a clearing `RNDGETENTCNT` (still full) contradicted one moment later. Pinned by `t-ioctl`: root `RNDADDENTROPY`→`0` (unchanged), root `RNDZAPENTCNT`→`EOPNOTSUPP`, and a `setresuid`-dropped `euid` making `RNDADDENTROPY`/`RNDZAPENTCNT`→`EPERM` then restored — all counterfactual (the old handler returned `0` regardless). `RNDGETENTCNT`→4096 and `FIONREAD`→512 pins unchanged. Gated `t-ioctl === PASS`. **But the shape this row landed was wrong, and its wrongness is a boot failure:** the family was dispatched on the command byte, and the arms named the wrong numbers - `RNDADDENTROPY` is `_IOW('R', 0x03, int [2])` = `0x40085203` (nr 3), which this row's arm answered as the drain, and `t-ioctl`'s "RNDADDENTROPY" constant was `0x40085202`, which is `RNDGETPOOL`. The whole-word contract, the measurements behind it and the seedrng evidence are in §12.1's A8-correction bullet; the row above stands as what was measured *then*, with the `CAP_SYS_ADMIN` gate and the `EOPNOTSUPP` refusals still true. |

**Landed this pass (each with its counterfactual pin, AGENTS.md checklist rule 4):**

- **A4 signalfd** — `handle_signalfd` now honours `SFD_CLOEXEC` (sets `FD_CLOEXEC`) and answers
  `EINVAL` on any bit outside `{SFD_NONBLOCK, SFD_CLOEXEC}`. Pinned in `t-signalfd` (the unknown-flag
  `EINVAL` and the `F_GETFD` close-on-exec readback both fail without the change). `t-signalfd` `===
  PASS`.
- **A5 mmap** — `handle_mmap_impl` now treats `MAP_FIXED_NOREPLACE` as "this address or fail"
  (`EEXIST` on an occupied range instead of relocating), refuses `MAP_FIXED | MAP_FIXED_NOREPLACE`
  with `EINVAL`, and refuses `MAP_HUGETLB` with `ENOMEM` instead of a silent 4 KiB page. It does
  **not** add a strict unknown-flag whitelist, because Linux accepts `MAP_STACK`/`MAP_NORESERVE`/
  `MAP_POPULATE` as no-ops and musl's thread stacks rely on that — a blunt "reject everything else"
  would itself be a non-Linux answer. Pinned in `t-mmap` (NOREPLACE exact-address success, `EEXIST`
  on collision, conflict `EINVAL`, `HUGETLB` `ENOMEM`). All four `t-mmap*` programs `=== PASS`.
- **vt family sweep (`vt_ioctl`, `'V'`) — `VT_SENDSIG` fixed, `ENOTTY` (measured).** Probed on a current console: `VT_SENDSIG` (0x04) → `ENOTTY` (the command was removed from the vt driver, so it falls through the kernel switch), while MPOS's arm returned `0` for a command that does nothing anywhere — a §4.2 false success on dead input. Now `0x04 => err::ENOTTY`; pinned by `t-seat` (counterfactual). `VT_DISALLOCATE` measured `0` on a free console (matches MPOS); the `EBUSY`-for-the-in-use-console case is an untested edge and left. `VT_RESIZE`/`VT_RESIZEX` measured `EPERM` for a non-foreground caller, but the owner case (where Linux resizes) could not be tested from here (the probe was not tty1's foreground job) and MPOS has a deliberate "the compositor owns the window size, `TIOCGWINSZ` carries it" design — so these are NOT flipped blind (§4.1); the honest non-owner `EPERM` is a candidate follow-up once the owner/handover behavior is pinned, recorded rather than guessed.

- **Flag-word audit (the A4 `signalfd` discipline, applied kernel-wide).** Swept every flag-bearing syscall for a silently-ignored or unvalidated flag bit (§4.2 — a flag the kernel accepted and dropped is a promise not kept). `dup3`, `pipe2`, `epoll_create1`, `timerfd_create`, `inotify_init1`, `renameat2`, `openat2`, `preadv2`/`pwritev2`, `splice`, `msync`, `clone`, `signalfd` all already refuse unknown bits with `flags & !(KNOWN) != 0 → EINVAL`. **One exception: `eventfd2`** dropped `EFD_CLOEXEC` (never applied close-on-exec — the exact leak A4 closed for `SFD_CLOEXEC`) and accepted any unknown bit. Fixed `handle_eventfd`: refuse an unknown bit `EINVAL` before creating, honour `EFD_CLOEXEC` via `fd_flags_set(FD_CLOEXEC)`, keep `EFD_SEMAPHORE → EOPNOTSUPP` (the whole-count-take refusal, unchanged). Pinned by `t-eventfd` (unknown bit `0x40` → `EINVAL`; `eventfd(0, EFD_CLOEXEC)` → `F_GETFD` shows `FD_CLOEXEC`) — both counterfactual (the old code returned a plain fd, no `EINVAL`, no CLOEXEC). Gated `t-eventfd === PASS`. **A second exception surfaced later: `unlinkat`** routed only on `AT_REMOVEDIR` and silently accepted any other flag bit, so `unlinkat(fd, path, 0x1000)` removed the name and answered `0` where Linux gives `EINVAL` *and leaves the file* (measured) - the wrong shape of answer, since a caller that expected a refusal got a deletion. Fixed `unlinkat` to reject `flags & !AT_REMOVEDIR != 0` with `EINVAL` first, the way Linux words it. Pinned by `t-unlinkat` (unknown flag `0x1000` → `EINVAL`, and the file survives the refused unlink) — counterfactual (the old code deleted and returned 0). Gated `t-unlinkat === PASS`.

- **`madvise` advice-vocabulary audit (the same discipline, applied to a small-integer selector rather than a flag word).** `handle_madvise` acted on `MADV_DONTNEED` and returned `0` for **every** other `advice`, including numbers Linux itself rejects with `EINVAL` — the §4.2 accept-and-drop in its clearest form (the handler's own doc claimed unrecognised numbers were "refused", but the code did not). Measured on the current host (`madvise` over a mapped anonymous page, advice `0..=110`): Linux accepts exactly `{0,1,2,3,4,8,10..24,102,103}` and gives `EINVAL` for the retired slots `5,6,7`, for `9` (`MADV_REMOVE`, invalid on an anonymous range), for `100/101` (`HWPOISON`/`SOFT_OFFLINE`, need privilege) and for everything past the block. Added the host-testable `abi::madvise_advice_is_known(advice)` gate and wired it before the handler does any work; `MADV_DONTNEED` keeps its page-drop, the recognised hints still answer `0` (truthful — best-effort with no observable durable promise here), unrecognised numbers now `EINVAL`. Pinned by host `madvise_advice_gate_matches_linux_known_set` and `t-madvise` (advice `5` → `EINVAL`, `0xffff` → `EINVAL`, `MADV_WILLNEED` `3` → `0`) — the two `EINVAL` lines counterfactual against the old silent `0`. Gated `t-madvise === PASS` (the DONTNEED page-drop and shared-preserve pins still green alongside).

- **`posix_fadvise` (`fadvise64`) advice-vocabulary audit — the same closed-set discipline.** `handle_fadvise64` looked up only that the descriptor was open and returned `0`, ignoring `advice` entirely and accepting it on a pipe — the `nr.rs` comment even read "every advice is accepted and ignored, which is what Linux does for the advice it does not act on", which is true of the *recognised* hints and false of the rest. Measured on the current host (glibc `posix_fadvise`, x86_64 `fadvise64(fd, offset, len, advice)`, answered `0`/`-errno` directly): advice `NORMAL(0)`..`NOREUSE(5)` → `0`; `6` and `99` → `EINVAL`; an unopened number → `EBADF`; a pipe → `ESPIPE`. Fixed the handler to `clone_at` (→ `EBADF`), then gate the advice on `0..=5` (`EINVAL` past it), then refuse a pipe with `ESPIPE`; a recognised hint still answers `0` (truthful, this kernel prefetches nothing). Corrected the `nr.rs` comment to name the closed vocabulary. Pinned by a new `t-fadvise` (advice `0`/`4`/`5` → `0`, `6`/`99` → `EINVAL`, bad fd → `EBADF`, pipe → `ESPIPE`) — the `EINVAL` and `ESPIPE` lines counterfactual against the old silent `0`. Gated `t-fadvise === PASS`; the guest gate is now **136 built**.

- **`clock_gettime`/`clock_nanosleep` clock-id vocabulary audit — the closed-set discipline on the clock selector.** `handle_clock_gettime` returned wall-clock time for **any** `clk_id` outside its monotonic list (a fall-through, not a refusal), so `clock_gettime(999, &ts)` answered `0` with a plausible number for a clock this kernel never named; `handle_clock_nanosleep` discarded `clk_id` (`_clk_id`) and slept on the default regardless, answering `0` for a request to sleep on a nonexistent clock. Measured on the host: `clock_gettime` accepts exactly `{0..=9, 11}` and gives `EINVAL` for the `10` gap and every id from `12` up (`42`, `999`); `clock_nanosleep(999, …)` is likewise `EINVAL`, while an unrecognised *flag* bit past `TIMER_ABSTIME` is ignored by Linux too — so only the clock id, not the flags, needed a gate. Both handlers now reject an id outside the set with `EINVAL` before any other work (in `gettime`, before the `tp` check, matching Linux's `EINVAL`-before-`EFAULT`). `clock_nanosleep`'s `TIMER_ABSTIME` → `EINVAL` is left as-is: an absolute sleep is a *feature* this kernel declines, not a success it fakes, and a refusal with a real errno is the honest direction. Pinned by `t-clock-gettime` (`clock_gettime(10)`/`(999)` → `EINVAL`, `(11 / TAI)` → `0`, `clock_nanosleep(999)` → `EINVAL`, `(CLOCK_MONOTONIC)` → `0`) — the two `EINVAL` lines counterfactual against the old silent `0`. Gated `t-clock-gettime === PASS`.

- **`memfd_create` flag-vocabulary gate.** `handle_memfd_create` validated only the name and looked just at `MFD_CLOEXEC`, creating the descriptor for **any** flag word — an unknown bit got an fd, and `MFD_HUGETLB` got an ordinary (non-huge) memfd the caller believed was huge-backed: two §4.2 success-for-a-thing-not-done answers. Measured on the host: `memfd_create` accepts `{MFD_CLOEXEC, MFD_ALLOW_SEALING, MFD_HUGETLB}` plus the `MFD_HUGE_*` size bits (only with `MFD_HUGETLB`) and rejects an unknown bit (`0x1000`) and size-bits-without-`MFD_HUGETLB` with `EINVAL`. Fixed the handler to gate `flags & !KNOWN != 0` (or size-bits without `MFD_HUGETLB`) → `EINVAL`, and to refuse `MFD_HUGETLB` with `EOPNOTSUPP` - this kernel has no hugepage backing, so the honest answer is a refusal the caller can fall back from, not a fake ordinary fd. Pinned by `t-memfd-shm` (unknown `0x1000` → `EINVAL`, `MFD_HUGETLB` → `EOPNOTSUPP`, `MFD_ALLOW_SEALING` alone still hands back an fd) — the `EINVAL`/`EOPNOTSUPP` lines counterfactual. Gated `t-memfd-shm === PASS`.

- **`getrandom` flag-word gate.** `handle_getrandom` discarded `flags` entirely (`_flags`) and filled the buffer for any flag word — an unknown `GRND_*` bit got random bytes and a success where Linux answers `EINVAL` (§4.2 accept-and-drop). The three valid flags are genuinely satisfied by this kernel's one always-ready, non-blocking generator (`GRND_NONBLOCK`/`GRND_RANDOM`/`GRND_AUTOCONF` — measured accepted on the host, with `0x100` rejected), so they stay accepted; an unknown bit now `EINVAL`, checked before the `len == 0` short-circuit (measured: a bad flag with `len == 0` still errors). Pinned by the host `getrandom` test (extended: `0x100` → `EINVAL` at `len 8` and at `len 0`, `GRND_AUTOCONF` accepted) — the `EINVAL` lines counterfactual against the old discard. Gated at baseline (`pass=134 fail=2`; boot unaffected — getrandom is on the stack-canary path and every program still runs).

- **`statx` flag word: unknown bits refused and `AT_SYMLINK_NOFOLLOW` honoured.** `statx` bound `flags` to `_flags` — it honoured `AT_EMPTY_PATH` but not the vocabulary or `AT_SYMLINK_NOFOLLOW`. Two faults: an unknown flag bit (e.g. `0x10000`) was accepted (Linux `EINVAL`; measured accepted set exactly `{0x100, 0x800, 0x1000, 0x2000, 0x4000}`), and every statx followed the last symlink, so an `lstat`-shaped statx (which uutils `ls -l`/`stat` reach for) reported the *target's* metadata while claiming success — a wrong-node answer, not merely less-than. The path branch now splits on `AT_SYMLINK_NOFOLLOW` exactly as `fstatat` does (`resolve_no_follow` vs `resolve_opt`), and a `statx_flags_are_known` predicate rejects bits outside the set. The sync bits (`AT_STATX_FORCE_SYNC`/`_DONT_SYNC`) and `AT_NO_AUTOMOUNT` stay accepted: one non-networked filesystem has nothing to sync or automount, so they cannot be violated (a legal no-op, like `mlock`'s). Pinned by host `statx_flag_gate_matches_linux_known_set` and `t-statx` (unknown `0x10000`→`EINVAL`; a symlink to a regular file reports `S_IFLNK` with `NOFOLLOW`, `S_IFREG` without) — the `EINVAL` and `S_IFLNK` lines counterfactual. Gated `t-statx === PASS` (`pass=134 fail=2`).

- **`open` honours `O_NOFOLLOW`.** The bit was not defined anywhere in the tree, so `openat(path, O_NOFOLLOW)` silently dropped it and `open_opt` followed the final-component symlink — the caller's "do not traverse a link here" guarantee (the classic world-writable-`/tmp` defence) was accepted with success and not kept: a §4.2 accept-and-drop with security meaning. Measured on the host: `O_NOFOLLOW = 0o400000`, `open(symlink, O_NOFOLLOW)` → `ELOOP`, `open(regular, O_NOFOLLOW)` succeeds, intermediate components still resolve. `openat` now defines the bit (`abi::O_NOFOLLOW`) and, when set, walks the parent without following (`resolve_no_follow`) and answers `ELOOP` if the final name is a symlink; the check is gated solely on `flags & O_NOFOLLOW`, so every normal open is untouched. Pinned by `t-openat` (a symlink made to the test file: O_NOFOLLOW → `ELOOP`, the same open without it still follows, O_NOFOLLOW on the regular file opens) — the `ELOOP` line counterfactual. Gated `t-openat === PASS` (the one extra run-red was `t-stdresolve`, the documented intermittent DNS flake, with no causal path to an open-flag check normal opens never reach).
  A **dangling** final component is the case that decides whether the no-follow walk is honest —
  `lstat` of `slink -> /does-not-exist` must answer the *link* (`S_IFLNK`), not `ENOENT` for its
  missing target, and `O_NOFOLLOW` on it must be `ELOOP`, not a followed open. Measured on a current
  host (`stat(path, AT_SYMLINK_NOFOLLOW)` and `os.stat(follow_symlinks=False)` both succeed). It
  holds here because `resolve_no_follow` looks the last name up with `Inode::lookup`, which never
  reads a target, and ramfs stores the link node itself — pinned by the host
  `resolving_a_name_without_following_it_still_handles_dot`, now extended with the dangling link so
  the property is asserted rather than assumed.

- **`ftruncate` enforces the descriptor's access mode.** The B1 access-mode enforcement covered `read`/`write`, but `ftruncate` resized any regular-file descriptor regardless of how it was opened — an `O_RDONLY` fd could truncate. Measured on the host: Linux refuses `ftruncate` on a read-only descriptor with `EINVAL` and leaves the size unchanged (note the errno differs from `write`'s `EBADF`). The handler now checks the recorded `O_ACCMODE` and refuses `ACC_RDONLY` with `EINVAL`; a non-path descriptor (pipe/socket/anon) defaults permissive and is caught by the regular-file test. Pinned by `t-truncate` (`ftruncate` on an `O_RDONLY` fd → `EINVAL` with the size unchanged at 13; the same fd `O_WRONLY` → `0`) — the `O_RDONLY` line counterfactual (the old handler resized it). Gated `t-truncate === PASS` (pipe `EOPNOTSUPP` divergence still holds). The same write-mode rule now covers `fallocate`, which resized a regular file through an `O_RDONLY` descriptor: it returns `EBADF` (measured host: `fallocate(O_RDONLY)` is `EBADF`, nothing grows), pinned by `t-fallocate` (`fallocate` on an `O_RDONLY` fd → `EBADF`; the O_RDWR file still allocates; pipe `ENODEV`/closed-`EBADF` preserved).



- **A6 rseq — refused with `ENOSYS`, after all (the earlier "NOT refused" is reversed).**
  A prior pass kept the accept and downgraded the gap to "write `cpu_id` later," reasoning that
  `t-rseq` pinned a `0` and rule 1 protects the test. That reasoning was wrong on two counts, and
  the correction is this pass. First, `0`-with-no-fields-written is the §4.2 violation the batch
  exists to remove, not a scope boundary — `t-rseq` had *encoded the false-success*, and rule 1's
  own carve-out (a test whose expectation is wrong vs Linux changes *with a measurement*) applies.
  Second, a measurement against a current host (Linux 7.2, `AT_RSEQ_FEATURE_SIZE` 33,
  `AT_RSEQ_ALIGN` 64) showed real glibc registers at length **33**, which the old `len != 32` arm
  rejected with `EINVAL` — so the accept served neither the musl binaries this machine actually runs
  (which never call `rseq`) nor a modern glibc's real startup; it satisfied only a synthetic length-32
  probe. The honest answer for a kernel that carries no per-thread registration is `ENOSYS`, the
  documented "rseq unavailable" fallback a probing libc is written to take. The syscall *number* stays
  in `nr.rs` (append-only ABI); only the hollow dispatch arm is gone. Doing `rseq` for real — writing
  and maintaining `cpu_id`/`node_id`/`mm_cid` across preemption, plus `RSEQ_FLAG_UNREGISTER` — is the
  named gap, a sched feature in its own commit.

- **A1 prctl — the name pair landed.** `PR_SET_NAME` (15) and `PR_GET_NAME` (16) are now real, not
  accepted-and-dropped: `set_current_comm` stores the caller's bytes in the group's `comm` slot (`Mm`,
  the same slot `comm_of` and `/proc`'s `comm` read) and `PR_GET_NAME` hands them back, NUL-padded to
  sixteen; with no name set it reports the derived image basename, as Linux does. A null buffer is
  `EFAULT` in either direction.
  - **The truncation half was first written backwards and has been corrected by measurement.** The
    landing note claimed a name with no NUL inside the sixteen-byte `TASK_COMM_LEN` slot is refused
    (`strnlen_user`→`EFAULT`), not truncated. Measured on this host with a buffer whose terminator
    cannot be an accident — sixteen non-NUL bytes with a non-NUL seventeenth — `PR_SET_NAME` answers
    **`0`** and `PR_GET_NAME` reads back **fifteen** characters: Linux copies at most the slot and
    forces its last byte to NUL. So the arm refused a case Linux accepts, which is the §4.2
    wrong-shape answer in the direction that hurts: a program that named itself sixteen bytes long
    would take its "the kernel would not name me" fallback here and not on Linux. The handler now
    truncates. Pinned twice — the host `prctl_stores_and_reports_the_process_name` (accepts, reads
    back fifteen, slot ends in NUL) and `t-prctl`'s new line for the same shape in the booted
    machine, the counterfactual for the `EFAULT`.
  - What Linux *does* refuse is the copy reaching memory the caller does not own: a name placed eight
    bytes from a page whose successor is unmapped answers `EFAULT`, while the same start with its NUL
    before the crossing answers `0` (measured). The byte-at-a-time scan that stops at the terminator
    is exactly that shape, and it re-validates at every page boundary the scan crosses — which is why
    the scan is bounded to the slot and does not read ahead for a terminator it will not use.
  `t-prctl` `=== PASS`. Only the catch-all `_ => 0` half is deferred, below.

**Deferred with its prerequisite named (not attempted blindly):**

- **A1 prctl catch-all — LANDED + gated.** The catch-all `_ => 0` was a §4.2 false success (a `0`
  claiming an unmodelled option was handled). It now returns `EINVAL`, matching Linux's `prctl`
  `default:` arm in `kernel/sys.c`. §4.2 lists `EINVAL` as the sanctioned honest answer for an
  argument the kernel refuses, and §4.4 prefers a refusal to a quiet fake — so blanket-`EINVAL` for
  every option *not backed by real state* is the objective-correct posture ("unknown → `EINVAL`"),
  not a wrong-shape answer. The prior worry that this "needs a measured whitelist so a blunt refuse
  doesn't break a workload" was over-cautious: the gate that boots the whole initramfs runs the real
  programs and stays green (`pass=133 fail=2`, reds the unrelated in-flight net/audio), proving no
  running program depended on the old `0`.   `t-prctl`'s "unknown answers 0" pin was corrected to
  "unknown → `EINVAL`" (it had encoded mpos's false-success, not Linux).
  **Then the four options that had real Linux behaviour behind them were given that behaviour.**
  The prerequisite turned out to be exactly what the earlier note said it was, and it is now built:
  `src/cred.rs` is a per-process credential model - the four capability sets, the dumpable tri-state
  and `KEEPCAPS` - stored on the thread group beside `comm`, with the measured Linux rules inside it
  and *consumers* rather than a stored no-op. `capget` reports the stored sets instead of a constant
  and `capset` stores them (Linux's `cap_capset` asks `CAP_SETPCAP` first and, holding it, accepts
  anything, which is why root's own lowering/raising round-trips); a uid/gid change re-derives the
  sets - the effective one cleared on a drop and restored from permitted on a return to root, the
  permitted one kept while any of the six ids is root's and cleared by a full drop unless `KEEPCAPS`
  (all four measured on 7.2.7); `execve` gives an `euid` of 0 its bounding set back, so a dropped
  bounding capability is the way to take one from a root program for good; and `ptrace_may_access`
  is consulted by `PTRACE_ATTACH` **and** `process_vm_readv`/`writev`, so the dumpable flag now
  decides something. `PR_SET_DUMPABLE`/`PR_GET_DUMPABLE`, `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` and
  `PR_CAPBSET_READ`/`PR_CAPBSET_DROP` were then moved out of the catch-all and answered with the
  measured contract (only 0/1 settable, 2 and 3 `EINVAL`; read 1/0 for `0..=40` and `EINVAL` past
  it; `DROP` asks the capability *before* the range, so an unprivileged `100` is `EPERM` where
  root's is `EINVAL`). Named divergences, both conservative: no file capabilities are read at
  `exec`, so a capability Linux would grant from a binary here is simply not held; and the dumpable
  reset uses `SUID_DUMP_DISABLE`, the sysctl's default, where the machine these were measured on had
  it set to 2. Pinned by thirteen host tests in `cred.rs`, `t-prctl`'s option table and `t-posix`'s
  `capset`/`capget` round-trip.
- **A2/A3 kd + the remaining evdev writes** — paired with Phase-11 Step B2 (move each device's ioctl
  accept/refuse down to the driver). Editing the `syscall/linux/fs.rs` ioctl switchboard for them now
  would add calls along the V-2 edge the next refactor removes, against checklist rules 5 and 7 (a
  refactor and a behaviour change in one diff). Land them with Step B, one device at a time.
  **`EVIOCREVOKE` is the exception that landed now**: it had one unambiguous Linux shape (a per-ofd
  revoke the driver reads as `ENODEV`), its state belongs device-side in `crates/evdev` where B2 wants
  it anyway (so this is relocation-aligned, not switchboard-widening), and it is the security
  false-success §4 ranks worst — so removing it did not wait for the refactor. The `EVIOCSFF`/`REP`/
  `KEYCODE` writes and the whole `kd` family do need the device's own view and stay with B2.

### 12.2 Group B — accepted, but the full contract is not implemented

These need a real behaviour, not just a refusal, so each carries more risk: a narrowing change
sends the caller down a path it would not otherwise take. Confirm on Linux first; land with a
conformance program.

| # | Where | Linux's answer | What this kernel does | Fix / decision |
|---|---|---|---|---|
| **B1** | `fs.rs:2760`/`2793` `handle_read`/`handle_write` (+ `readv`/`writev`); `opened_for_write` in `vfs/file.rs:195` was read only by `inotify.rs` | writing an `O_RDONLY` fd → `EBADF`; reading a `W`-only fd → `EBADF` | was: access mode enforced at `open` but never afterwards — `open(O_RDONLY)` then `write()` succeeded | **LANDED + gated.** `File` now carries the open's `O_ACCMODE` in a dedicated `access` field (default `RDWR`, so sockets/pipes/anon stay permissive); `File::read`/`File::write` - the single chokepoint every user read/write/readv/writev/pwrite/splice/copy reaches the inode through - refuse a mismatch with `EBADF` (`VfsError::BadFd`), before even a zero-length access. `F_SETFL` cannot reopen a refused direction (separate field). Pinned by a host test (`vfs/file.rs` `access_mode_is_enforced_after_open`) and confirmed by the guest: `t-fill-ramfs` had been reading an `O_WRONLY` fd back (Linux answers that `EBADF` too) - a test expectation that was wrong for Linux, corrected to open `O_RDWR`; it and the rest of the fd-using suite pass. |
| **B2** | IPv4 tails at `socket.rs` `recvfrom`/`recvmsg` (compute `nonblock` from `O_NONBLOCK` only) | `MSG_PEEK` does not consume; `MSG_DONTWAIT` earns `EAGAIN` | **was: `MSG_PEEK` on TCP/UDP consumed the datagram and returned success** (silent data corruption); `MSG_DONTWAIT` ignored | **LANDED + gated.** All four IP recv/send tails now route the wait through `dontwait(fd, flags)` (honours `MSG_DONTWAIT`), and the two recv tails return `EOPNOTSUPP` on `MSG_PEEK` - the same honest refusal `AF_UNIX`/`NETLINK`/`PACKET` already give. Real `MSG_PEEK` over IP remains a named sub-gap. Pinned by the new `t-sock-flags` (peek `EOPNOTSUPP`, `DONTWAIT`→`EAGAIN`; hangs without the fix). `pass=131 fail=2`, no net regression. |
| B3 | `abi.rs` `get_robust_list` `ENOSYS` while `set_robust_list` was accepted | x86_64 has the call and returns the registered head | was: asymmetric — `set` accepted-and-discarded, `get` refused | **LANDED + host-gated.** `set_robust_list` now files the head on the process's `Mm` (a `robust_head` next to the membarrier registration); `get_robust_list` returns it with `sizeof(robust_list_head)` = 24, `ESRCH` for a foreign pid, `EFAULT` on null outputs. Pinned by the host round-trip in `abi.rs` `startup_registrations_check_their_abi_sizes`. The remaining gap is *walking* the list to release held futexes at thread exit (named below), not the registration pair. **Do not add head-pointer validation here**: measured on a current host, `set_robust_list` answers `0` for `NULL`, for a low non-`NULL` value, for an unmapped in-range address and even for a kernel-space address, because the kernel deliberately never touches the head it is handed — it files the pointer, and the dereference belongs to exit, where a bad one is handled as the user's own fault. Storing any value is therefore Linux's answer, not an unchecked input. The one thing Linux does check, `len != sizeof(struct robust_list_head)` → `EINVAL`, this kernel already refuses. |
| B4 | `posix.rs` `vhangup` → constant 0 | hangs up `current->signal->tty`, answers `0`; **answers `ENOTTY` when the caller has no controlling terminal** (`drivers/tty/tty_io.c` `SYSCALL_DEFINE0(vhangup)`) | was: `0` for every caller — a false success for the (common) no-terminal caller, not just the privileged one | **Landed and gated — both cases honest.** `vhangup` queries the controlling terminal: none → `ENOTTY` (Linux's answer, what the self-test programs reach — `t-posix` corrected from `0` to `ENOTTY`, rule 1's measurement exception); present → `EOPNOTSUPP`, because the kernel has no tty-layer hang-up (re-init + `EIO` on open fds) and §4.2 forbids faking the success. A logout/getty does its own fd teardown next regardless, so the honest refusal costs nothing it had. The full Linux behaviour (actually hanging up) remains the open feature; the prior "Linux answers `0` for root, don't refuse" reasoning conflated the two cases — measured here: the no-terminal caller does not get `0`. The `EOPNOTSUPP` branch is reached only with an acquired terminal (login path), which no headless self-test does, so it is gate-unexercised — the gate verifies the change is regression-free, not that branch's value. |
| B5 | `posix.rs` `setpriority` discarded, `getpriority` constant 20 | round-trips the nice value; `PRIO_PROCESS`/`PGRP`/`USER` sets over the resolved set, `20 - min(nice)` get, `ESRCH` on empty/gone, `EINVAL` on range/which | was: no round-trip; board wrongly said `Done` | **LANDED + gated.** `nice` is now a per-task field (mirroring `oom_score_adj`: stored honestly, reported truthfully, no scheduling-weight effect — that stays a named sub-gap), inherited across `fork`. The three `which` modes resolve via `current`/`pgid_of`/`group_members`/`uid_members`. Board corrected (the row split back to `Done`, leaving `vhangup` as the only `Partial`). Pinned by `t-priority` (round-trip −5→25 / +3→17, group set seen by process get, range `EINVAL`, gone pid `ESRCH`, bad which `EINVAL`). |
| B5b | `posix.rs` `sched_setattr` dropped the nice, `sched_getattr` reported a hardcoded 0 | the modern door onto the same per-task nice: `sched_setattr` writes it, `sched_getattr` reads it, both `SCHED_OTHER`-only, nice −20..19, `ESRCH`/`EINVAL`/`EOPNOTSUPP` on the shapes Linux names | was: `sched_setattr` returned `0` while discarding `sched_nice`; `sched_getattr` wrote size/policy and hardcoded nice 0 | **LANDED + gated (same pass as B5's slot).** These are the extensible-attributes door onto the *very* per-task `nice` B5 made real — `sched_setattr`'s `SCHED_OTHER` arm now calls `set_nice(target, nice)` (the value round-trips through `getpriority`), and `sched_getattr` reports `nice_of(target)`, not a constant. Hardcoding 0 in `getattr` was the identical wrong-as-a-lie `getpriority` had before B5 (§4.2). The `attr`+16 `sched_nice` field is the only one this policy carries; deadline/rt policies 1..6 stay `EOPNOTSUPP`, unknown policy or out-of-range nice `EINVAL`, flags ≠ 0 `EINVAL`, `pid` naming no task `ESRCH`, struct `< 48` bytes `EINVAL`. Pinned by `t-priority` (cross-door: `setpriority(-5)`→`sched_getattr` reads −5; `sched_setattr(-7)`→`getpriority` 27 and `sched_getattr` −7; nice 25 and 40-byte struct `EINVAL`) — every line counterfactual against the drop/hardcode. No new board `Partial`: this row was inside §1.3's scheduling `Done` lump, corrected by naming the gap here. |
| B5c | `posix.rs` `sched_setscheduler` accepted a nonzero `SCHED_OTHER` priority and returned `0` | the `SCHED_OTHER` arm validates the passed `struct sched_param` priority the way `sched_setparam` does: zero → the no-op success, nonzero → `EINVAL`, null/unbacked → `EFAULT` | was: `SCHED_OTHER => 0` ignored `args[2]` entirely | **LANDED + gated.** `sched_setparam` already refused a nonzero priority; the `setscheduler` door took the same `sched_param` and did not, so `sched_setscheduler(0, SCHED_OTHER, &{prio=5})` answered success where Linux answers `EINVAL`. Measured on a real host (gcc probe of glibc `sched_setscheduler`): `prio=5`→`EINVAL`, `prio=0`→`0`, matching this kernel's `setparam`. The `SCHED_OTHER` arm now mirrors `setparam`'s `buffer_ok`→`EFAULT` and priority→`EINVAL` checks before its success; the refused RT policies keep validating the param before `EOPNOTSUPP`. Pinned by `t-posix` (`setscheduler(SCHED_OTHER, prio 0)`→0, `prio 5`→EINVAL, `setparam(0)`→0, `setparam(7)`→EINVAL, then `getparam` still 0 after the refused sets) — the `prio 5`/`prio 7` lines are counterfactual (the old `SCHED_OTHER` arm returned 0 for both). Inside §1.3's scheduling `Done` lump. |
| B6 | `proc.rs` `times()`/`getrusage()` `tms`/rusage | advances with CPU time, split user/system, with reaped-child totals | **LANDED + gated.** The timer interrupt charges the running process's `Mm` user or system by the ring it interrupted (`frame.code_segment == user CS`); `times()` reports `tms_utime`/`tms_stime`/`tms_cutime`/`tms_cstime` in `USER_HZ` ticks (the 250 Hz internal clock scaled `×100/TICK_HZ`, matching the `CLK_TCK` musl hardcodes) and `getrusage(RUSAGE_SELF)`/`(RUSAGE_CHILDREN)` report the same as real `timeval`s. A reaped child's CPU folds into the parent's child totals at `reap()` (`absorb_child`: parent's `cutime += child.utime + child.cutime`), Linux's accumulate-on-`wait`. Pinned by `t-times`: a user spin raises `utime` not `stime` (the split fails if attribution is wrong), monotonic, sane `tv_usec`, and fork→burn→wait makes `RUSAGE_CHILDREN`/`tms_cutime` grow. The fourteen byte/page counters stay zero (not measured — an honest gap, not an invented number). |

### 12.3 Group C — rationale comments that no longer describe the code

Cosmetic to a caller, but they mislead the next reviewer, so fix in the same commits as the
behaviour they describe:

**Landed (all three corrected):**

- `mod.rs` `sigaction` doc said "a signal handler is never called" — corrected to say a caught
  signal builds an `rt_sigframe` and runs the handler, with the one honest caveat kept: `SA_RESTART`
  is still recorded-but-not-honoured (§2.5).
- `fs.rs` `unlinkat` doc said `AT_REMOVEDIR` was "ignored" — corrected to say it selects `rmdir`
  vs `unlink`, which the body already does.
- The `getpriority`/`setpriority` and `vhangup` "Done" lump in §1.3 — split out of the `Done` row
  into a `Partial` row that names B4/B5 as the open gaps (the behaviour itself is still a
  silent-accept; only the board claim was fixed here).

### 12.4 Order

1. **Restore the green gate.** Fix `t-tty-eintr`'s two test-side expectation bugs — `AGENTS.md`
   §3 already states the kernel's answers (`EINTR`, 3 bytes) are the measured Linux ones and the
   test compares against `-lin::EINTR` (`+4`) and expects two bytes for a three-byte `"hi\n"` —
   and reconcile `t-tty-vtime` with the in-flight `VMIN`/`VTIME`/`EINTR` read-path rewrite. Treat
   a lone red `t-netlat` as load. **This blocks everything below.**
2. **Group A** — the honest-refusal batch. Each is small, host-testable, and purely narrowing.
   Pair A2/A3 with Phase-11 Step B2 where they touch the same device-`ioctl` code.
3. **Group B** — the contract-completion batch, one behaviour at a time, each pinned by a `t-*`
   program and gated on the full run. B1 first (largest blast radius, most important), then B2.
4. **Group C** — fold the comment fixes into the commit that changes the behaviour they describe.

### 12.4a Root cause of the step-1 block — `t-tty-vtime` (diagnosed, fix designed)

Measured on the current tree: the guest suite reaches 115 verdicts and stops inside
`t-tty-vtime`, right after `ok: VMIN=2/VTIME=5 is accepted` — it never reaches a `=== SUMMARY`.
`t-tty-eintr` now PASSes (the two expectation bugs `AGENTS.md` §3 recorded are already corrected
in the tree: it compares against `lin::EINTR` (`-4`) and expects 3 bytes for `"hi\n"`). So the
single remaining red is this one hang, and it is a *wiring* bug, not a policy bug: `read_wait`
(`tty.rs:1163`) returns the correct `VMIN>0`/`VTIME` decision and its host tests pass.

The bug is an agreement mismatch between `read_blocking` and `block_until_deadline`:

- `block_until_deadline(cond, deadline)` (`runtime.rs:2868`) is a **condition loop**: it returns
  only when `cond()` is true, a signal is deliverable, or `timer::ticks() >= deadline`; on any
  other wake its schedule closure re-parks it.
- For `VMIN>0` with an empty queue, `read_wait` returns `UntilInput`, which `read_blocking`
  maps to `deadline = u64::MAX` with `cond = has_ready()` (tty.rs:1741,1755). `has_ready()` in
  non-canonical mode needs `out.len() >= VMIN`, so one partial byte leaves it false.
- `receive_byte` (tty.rs:1550-1556) *does* wake the reader for a partial byte
  (`byte_restarts_clock()` → clone the waiters → `sched::wake(id)`), but `block_until_deadline`
  wakes, re-tests `has_ready()` (still false), sees `deadline == MAX`, and **re-parks** — so the
  clock-restart wake is swallowed and the read never re-derives the real `UntilTick(last_byte +
  span)` deadline. It sleeps forever.

The `tty.rs` comment "a wake from anything else is simply another look" (read_blocking:1757)
assumes every wake surfaces to the caller; `block_until_deadline` does not honour that.

**Fix (additive, no change to the shared scheduler primitive):** give `LineDiscipline` an
`input_count: u64` bumped in `note_input_tick`, and for a pass whose wait is clock-restarting
(`byte_restarts_clock()`), snapshot `base = input_count` before parking and pass the predicate
`has_ready() || input_count != base`. A byte arrival then changes `input_count`, ends the park,
and `read_blocking` re-derives `UntilTick(last_byte + span)` — which arms the real deadline and
delivers the partial bytes one `VTIME` after the **last** byte, matching the measurement. This
cannot busy-spin (the predicate only flips on a real arrival) and leaves the canonical, `VTIME==0`
and `VMIN==0` parks on their current `has_ready()` predicate untouched.

**Landed and verified.** The fix is in `tty.rs` (an `input_count: u64` on `LineDiscipline`
bumped in `note_input_tick`, and a `byte_restarts_clock()` read parks on
`has_ready() || input_moved(base)`). After it, `cargo xtask test` runs to completion:
`t-tty-vtime`, `t-tty-eintr` and `t-tty-query` all report `=== PASS`, the run reaches
`=== SUMMARY pass=130 fail=2`, and QEMU exits 33. The two remaining reds are **not** tty and
**not** Phase 12 — `t-dsp` and `t-dhcp` are other in-flight new programs: `t-dsp` fails wholesale
because the self-test machine boots with `-audiodev none` (no `/dev/dsp`), and `t-dhcp` is the
not-yet-wired DHCP boot path (§4.5 board). `t-netlat` passed this run. So the green-gate work
*in Phase 12's scope* is done; the gate reaches a summary, and `fail={t-dsp, t-dhcp}` is the
known non-Phase-12 baseline the Group-A regression check compares against.

**Both of those diagnoses were wrong, and both reds are now closed.** The `-audiodev none`
explanation is the mistake worth writing down: it predicted a machine with no sound card, and a
machine with no card is a legitimate thing to be red about. Measured against the very QEMU this
gate boots, `-audiodev none,id=snd0 -device virtio-sound-pci` puts the card on the bus regardless
(`info pci` reports `1af4:1059`) — that backend silences the *audio*, never the device. The real
cause was this kernel searching for the wrong PCI ID, so the honest statement is "the driver never
found a card that was there": a bug, not a machine's absence. See §12.4a.

### 12.4a The last two reds closed: `/dev/dsp` and the DHCP answer — **DONE, gate green `pass=136 fail=0`**

The pair `fail={t-dsp, t-dhcp}` was the gate's standing red baseline since it was first quoted.
Neither red was a missing pin: each program *was* the pin, and each was pinning a real defect —
one in the kernel, one in the test's own construction. `cargo xtask test` now reaches
**`=== SUMMARY pass=136 fail=0 (136 built)`** with QEMU exit 33.

**`t-dsp` — two kernel bugs, found in that order, the second hidden by the first.**

1. **The sound card's PCI ID was wrong.** `DEVICE_SOUND` read `0x1055`. Measured two ways against
   the machine this kernel boots on: `/usr/include/linux/virtio_ids.h` gives `VIRTIO_ID_SOUND = 25`,
   and the modern virtio PCI ID is `0x1040 + that` = `0x1059`; QEMU's own `info pci` for
   `-device virtio-sound-pci,audiodev=snd0` reports `Audio controller: PCI device 1af4:1059`. At
   `0x1055` the lookup never matched, `init` took its `Probe::Absent` branch — which prints
   nothing, correctly, because a machine with no card is a quieter boot — and `/dev/dsp` was never
   registered. The whole program then cascaded: `open` `ENOENT`, every ioctl on the closed
   descriptor `EBADF`, `chmod` `ENOENT`.
   **The trap this exposes is worth generalising**: an ID looked up on the *absent* branch is
   invisible by construction — the only trace of a wrong ID is the device that never appears. The
   driver behind it was complete and correct the whole time, which is why §7.2b could truthfully
   say the family was implemented while the node it serves did not exist. Audited the other five
   virtio IDs the same way against `info pci`: `rng 0x1005`, `console 0x1003`, `net 0x1000`,
   `9p 0x1009`, `gpu 0x1050`, `input 0x1052` are all exactly what QEMU exposes. Left as they are.
2. **The first real write to `/dev/dsp` panicked the kernel.** With the card found, the boot reached
   `virtio-sound: 44100 Hz, 2 channels at /dev/dsp` and `t-dsp`'s frame-buffer write died on
   `virt_to_phys(0x3fa25c60): not an address from the direct map`, with the syscall ring showing
   `write(fd 3, 0x3fa25c60)` — a *user* address. Cause: `handle_write` builds its `&[u8]` straight
   over the caller's buffer, `DspDev::write` passes it to `VirtioSnd::play`, and
   `drivers::virtio::hal` maps the DMA buffer with `mm::virt_to_phys`, which is only defined on the
   direct map. Every other consumer of that slice only *reads* the bytes, so nothing else had ever
   needed the address to be the kernel's — sound was the first driver to hand one to the device.
   Fixed by copying into a per-device one-period staging buffer allocated at probe from the kernel
   heap, so the address handed to the crate is always the kernel's. That is also Linux's shape:
   ALSA's dsp write path `copy_from_user`s into the DMA ring instead of mapping the caller's pages.
   The copy is one period (4 KiB) at a time, so its size is a constant of this driver and not a
   number the caller steers (§4 rule 4). A refused piece stays a short write, so the
   `CharDevice` byte-count contract is unchanged.
   Pinned by `t-dsp`'s `a frame buffer writes through` (512 bytes, the counterfactual: before the
   staging buffer the same write ends the machine) and by the rest of the program, which was red
   wholesale and is green now. **No new host unit test was possible for either half** — the ID
   table lives in a `cfg(target_os = "none")` module and the panic needs the machine's page tables,
   so the guest program is the pin, which is what §5 prescribes for a behaviour only the machine
   can show.

**`t-dhcp` — the defect was in the test, not the kernel.** `sendto` returned all 292 bytes ("the
DISCOVER goes out whole") and slirp never answered. The kernel sends exactly the bytes it is handed,
so the bytes were the suspect, and they were wrong in two places: the IPv4 destination was written
at offset 20 (the UDP port pair) instead of 16, so it was then overwritten by the port numbers and
the packet left with destination `0.0.0.0`; and the magic cookie and options were written at their
*BOOTP-relative* offsets applied to the absolute buffer, so the cookie landed 28 bytes early, inside
the BOOTP `file` field, and no options followed it. A BOOTP request with no cookie is not a BOOTP
request, and slirp was right to ignore it. Fixed by naming the two bases (`IP_DST = 16`,
`BOOTP = 28`) and deriving every field from them. The expectations were **not** touched — "an answer
arrives within five seconds", "the reply is a BOOTREPLY", "and it carries our transaction id" are the
host's promise and the kernel's job, and they now pass on the machine.
This is the §1 case where the test was genuinely wrong: the *construction* was defective, so it
never asked the question it meant to ask. Rule 1's carve-out covers exactly this, and the line is
drawn in the other direction from "weaken the assertion to make it pass" — no assertion was
relaxed, the input was corrected so the assertion finally means something.
What it pins: `AF_PACKET`'s receive path delivers a frame the IP layer never claimed (`net/mod.rs`
calls `packet::deliver` before the stack decides the frame is any of its business), and the reply's
`op`/`xid` survive that path byte-honest.

**Gate, quoted from its own summary**: host `570 passed`; `mpos-xtask 29 passed`; guest
`=== SUMMARY pass=136 fail=0 (136 built)`, QEMU exit 33, boot line
`virtio-sound: 44100 Hz, 2 channels at /dev/dsp`, `t-dsp === PASS`, `t-dhcp === PASS`.
The gate has no known red baseline any more; a new red is a regression, full stop. (The
`29 passed` and the `136 built` are that run's numbers; `tests.toml` has since taken nine
programs out of the *default* sweep — every one of them is still built — so a re-run prints
`=== SUMMARY pass=127 fail=0 (127 built)` with `9 of 136 user tests left out by tests.toml`,
and xtask's own suite is at `36 passed`. See the Phase 12 board entry for what the split is and
why.)

**Re-run with this pass's two commits on top** - the machine's `gpu` key and the §7.4
measurement, neither of which touches the kernel or the image: xtask's own suite `38 passed`,
guest `=== SUMMARY pass=137 fail=0 (137 built)` with QEMU exit `33` and `10 of 147 user tests
left out by tests.toml`. Worth quoting for one reason beyond the green: the test boot's own
QEMU line still reads `-device virtio-gpu-pci`, which is the new key's *default*, so the sweep
boots exactly the machine it booted before - and that is why a machine key written for an
interactive `run` cannot silently move the gate.

### 12.4b The machine's own tools, and the inode flag word — **DONE, gate green `pass=139 fail=0`**

Reported from the shell of the running image, four commands that could not run:

```
mpos:~# lsattr          lsattr: reading ./README: Not a tty
mpos:~# lsblk           lsblk: can't read '/proc/self/mountinfo': No such file or directory
mpos:~# lsusb           lsusb: /sys/bus/usb/devices: No such file or directory
mpos:~# lsmod           lsmod: /proc/modules: No such file or directory
```

Four different causes, and none of them was the command. The identity of the
commands came first, because everything else depends on it: **all four are
busybox 1.38.0-r7 applets** — `/bin/lsblk`, `/bin/lsattr`, `/usr/bin/lsusb` and
`/sbin/lsmod` are symlinks to `/bin/busybox`, created by the `busybox --install
-s` that apk's post-install trigger runs inside the build's chroot. That is not
what Alpine's package index says (`/bin/lsblk` there is util-linux's, `/usr/bin/lsusb`
is usbutils'), and it is why every interface below was *measured against the
binary in this image* rather than reasoned from a man page - the image was mounted
read-only and each applet run under `chroot`, against the host's `/sys`, against
`/proc` files written by hand, and against synthetic `/sys` trees built to this
kernel's shape.

Three of the four causes were files that did not exist, and the fourth was worse:
a *number* that was wrong. `lsattr`'s `Not a tty` is musl's `strerror(ENOTTY)` -
the `'f'` ioctl family had no handler. `lsblk` died on `/proc/self/mountinfo`,
which this kernel did not have at all. `lsusb` and `lsmod` asked for a directory
and a file that were not there. And `lsblk` would have died a second time on
`/sys/block`, which was also absent - measured: without it, `can't open
'/sys/block'` and exit 1, whatever else the tree holds.

**The one that was not a missing file: `st_dev`.** `lsblk` builds its MOUNTPOINTS
column by matching mountinfo's third field against the `dev` numbers
`/sys/class/block/<name>/dev` publishes. This kernel reported the mount table's
own attachment counter there - `/` was `0`, the first mount `1` - so even a
faithfully-shaped mountinfo would have left that column blank, and the same field
is what `df`'s `Filesystem` column and `findmnt` key on. Measured with a mockup
before the change: the device table came out right and the mount points column
came out empty. Linux answers with the *superblock's device*, and `mount(2)`
resolves the source to one (`lookup_bdev`); the same resolution now happens at
the same place here, and the number travels with the mount. A pseudo-filesystem
gets an anonymous number - major 0, from its own counter, as Linux gives one -
because `0` is not a candidate: a `dev_t` of zero means "no device" and two of
those compare equal whatever serves them. The mount's attachment identity is kept
as well, and the two are used for what each is for: the identity keys locks, unix
sockets and the `EXDEV` decision, the device number is what `stat` reports. The
same change found `/dev/vda2` reporting `st_rdev` of 0 - `DevBlockNode` had never
implemented `rdev` - so `ls -l /dev/vda2` had no `254, 2` to print either.

**The flag word, and what makes it worth having.** `FS_IOC_SETFLAGS` that stored
what it was handed would have closed this report and been worthless: `chattr +i`
would have protected nothing. So the two bits that mean something are *enforced* -
`IMMUTABLE` refuses the write, the write-open, the truncate, the removal, the
rename, the second link and every metadata change; `APPEND` refuses everything a
truncation would do and every write that is not going to the end - in the helpers
that already exist for refusals, which is how `unlink`, `rmdir` and the
copy-and-unlink `rename` is emulated with all refuse together. The four remaining
flags are accepted because something reads them: `NOATIME` where the access time
would move, `NODUMP` because its whole meaning is a promise *to userland* that
reads it back, `SYNC`/`DIRSYNC` because a write-through filesystem already keeps
that promise. Everything else is `EOPNOTSUPP`, including every bit Linux's own
`FS_*` vocabulary names and this kernel cannot keep - the list is in "Remaining
ABI support" above, and `AGENTS.md` §4's second governing rule - "answer
truthfully, or refuse" - is why it is a refusal rather than a stored bit.

**Two corrections that only the pins could produce.** Writing the enforcement test
first caught the kernel: `may_create_in` had been given an append-only check, and
Linux's `may_create` has none - an append-only directory "can only be added to",
so a name *may* be created in it and only removal is refused. And the gate caught
a lie older than this work: both `/proc/mounts` and `/proc/self/mountinfo` left
the root filesystem out entirely, because the root's record required a *source*
and only a promoted mount ever supplied one - so `/` was reported as not mounted
at all, to every `df`, every `findmnt`, and `switch_root`'s own grep. Linux prints
the filesystem's type there (`rootfs / rootfs rw 0 0`), and now so does this.

**Measured, on the real image.** `cargo xtask run --mode terminal`, then at the
login:

```
mpos:~# lsattr /root/README /root/start-weston.sh
--------------e----- /root/README
--------------e----- /root/start-weston.sh
mpos:~# lsblk
NAME            MAJ:MIN  SIZE TYPE MOUNTPOINTS
vda             254:0   2048M disk
vda1            254:1    128M part
vda2            254:2   1918M part /
mpos:~# lsusb; echo $?
0
mpos:~# lsmod; echo $?
Module                  Size  Used by    Not tainted
0
mpos:~# df -h /
Filesystem                Size      Used Available Use% Mounted on
/dev/vda2                 1.9G    916.1M   1001.9M  48% /
mpos:~# chattr +i /root/README; lsattr /root/README
----i---------e----- /root/README
mpos:~# echo x >> /root/README
-sh: can't create /root/README: Operation not permitted
mpos:~# rm /root/README
rm: can't remove '/root/README': Operation not permitted
mpos:~# chattr -i /root/README; echo ok >> /root/README
```

`lsattr` prints what it prints on this host's ext4; `lsblk` fills the column that
was blank; `lsusb` and `lsmod` are the honest-empty answers; and `chattr +i` is a
switch that really closes and really opens.

**Gate, quoted from its own summary**: host `665 passed`; xtask `36 passed`; guest
`=== SUMMARY pass=139 fail=0 (139 built)`, QEMU exit `33`, with `t-fsflags` and
`t-mountinfo` among the 139 and `10 of 149 user tests left out by tests.toml`.
One thing the gate does *not* cover, named rather than assumed: the block tree's
shape. The test machine boots with no disk at all, so `/sys/block` has nothing in
it to walk; the shape of a disk's entry, its partitions and their
`size`/`partition` attributes is pinned by host tests that build the tree from a
numbering they write down, and the real thing is what the run above shows.

### 12.5 Deliberately not in this phase

- Anything already honestly refused. `fanotify`/`keyctl`/`userfaultfd`/namespaces and the
  `_ => ENOSYS` dispatch arm are correct answers, not gaps to close here.
- `SO_LINGER`/buffer sizes (`record-only`) and `/proc/<pid>/smaps` (its sibling
  `maps` is answered now) — already tracked in §4.3 / the ABI board; this phase
  does not duplicate them.
  (`SO_REUSEADDR` was on this list and is not any more: it is applied at
  `bind`/`listen`, §4.3.3.)
- Real `MSG_PEEK` over TCP/UDP, real `rseq` state, real per-task CPU accounting: named here as
  sub-gaps, planned where the prerequisite (peek-preserving delivery, the clock, the
  scheduler's own timekeeping) is scheduled.

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

**Recommended order**: P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7 -> P8 -> P9

**P11 Step A is done** — a deletion, landed on the green baseline the tree now has. P11's Steps B and C are the ones worth doing before the next device driver or the next
per-open-file resource is added, because each of those makes the debt one line larger.

**For the display work specifically**, §7.4 carries its own order, and its first step is not a
kernel change at all: a wlroots compositor on `WLR_RENDERER=pixman` is expected to run on the
DRM surface as it stands, which makes "boot one and read what it says" the cheapest way to find
out what that analysis has wrong.

The first two phases (epoll/timerfd + signal delivery) are the highest impact — they unlock the majority of real-world Linux userspace programs.
