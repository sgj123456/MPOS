# §A Appendix — the evidence behind the status board

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

The nested notes and measurements that sat under `ROADMAP.md`'s checklist before
the split, in the same order. A reader following a `(§N)` pointer does not need this;
it is kept because each entry was a measurement made at a particular moment and some
are not restated in the phase sections above.

**Core (Phases 1–3)**
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
  - [x] `flock` owned by the open file description, record locks by the process
        (`t-filelock`) — and each released by the event Linux uses: the process's
        record locks on *any* close of a descriptor to the file, the description's
        `flock`/`F_OFD_*` lock on the close of the *last* one · read/write
        deliberately *not* enforced: Linux advisory locks never refuse an `open`ed
        read, and mandatory mode is a deprecated per-file opt-in (§3.2)
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
  - [x] per-send cmsg boundaries on `SOCK_STREAM`: each handover carries the
        stream position its send ended at, and a read stops there, so two sends
        of one byte each arrive as two deliveries with their own descriptors
        (`t-unix-scm`)
  - [x] a *data-less* `sendmsg`'s handover is delivered on the read that asks
        for it: the record is due the moment it is queued and no byte will ever
        arrive *with* it, so the read answers with the record and a zero-byte
        return instead of parking on a byte that cannot come (`t-unix-scm`)
  - [x] the loopback is a real wire — `lo` is an interface over smoltcp's own
        loopback device carrying `127.0.0.0/8`, and a socket's interface is
        decided by the first address it is given, because the stack now keeps
        one `Fabric` (interface + device + socket set) per wire and smoltcp has
        no way to move a socket between them (`t-udp-loopback`). Raw/ICMP
        sockets deliberately stay on eth0's (§3.3.1)
  - [x] `listen`/`accept` on IPv4 — a listener is a *pool* of listening sockets
        plus the queue of the ones that completed, refilled after every poll;
        the contract is Linux's, each rule measured first, and closing a
        listener **resets** its unaccepted queue (§3.3.2, `t-tcp-listen`)
  - [x] `SO_REUSEADDR` at `bind`/`listen` — and the rule it gates is *not* "the
        new socket asked": measured on Linux, a holder that is **listening**
        keeps its name whatever either side says, while any other holder
        (a `TIME-WAIT` 4-tuple, an established connection, a bound socket)
        yields it only to a caller that set the option **and** whose holder had
        set it too — which is why Linux copies the option onto an accepted
        socket, and why a restart comes back onto its own port (`t-tcp-listen`)
        (§3.3.3)
  - [x] `SO_LINGER{on,0}`'s egress reset — the last `close` of a socket carrying
        it is an `abort()`, so the peer reads `ECONNRESET` and writes `EPIPE`
        where an ordinary close is end-of-stream; the note that said smoltcp
        cannot send an unsolicited `RST` was wrong (§3.3.4, `t-tcp-listen`)
  - [x] buffer sizes are Linux's numbers, `SO_SNDBUF` bounds a datagram's send queue,
        and a stream's `SO_RCVBUF` is its receive window - inherited by the socket a
        listener accepts; a datagram's receive ring is still fixed (`docs/uapi/` §5.1
        items 58 and 98, §3.3.3)

**Memory (Phase 5)**

**Process (Phase 6)**

**ABI gaps → `ENOSYS` (remaining)**

**Drivers (Phase 7)**
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

**Wayland compositor (weston) support** — the cross-cutting work a rootful
  - [x] a pointer with real artwork — `weston.ini` names `cursor-theme=
        breeze_cursors` and a `cursor-size`; `breeze-cursors` was already in the
        image, but with no theme named weston drew only the fixed ~16px arrow
        libwayland-cursor builds in, which is what made the cursor look tiny and
        the theme "not applied" (a shell-config fix, not a kernel one)
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
  - [x] and the device is the owner's while it holds it: a reader that is not
        the grabber is told nothing - not the records, and not a readable answer
        either - because every client has a *position* in the device's window
        rather than a turn at one shared queue. Two readers of an *ungrabbed*
        node each see every event, which the shared queue could not do either
        (whoever read first consumed it for everybody)
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

**Security (Phase 8)**

**Performance (Phase 10)**

**Architectures (Phase 9 — aarch64 stages 1a-1f, 2, 5a, 5b and 6a landed: the
  - [x] 13.1 `hal` is arch-neutral: its 14 `target_arch` gates became capability flags
        (`mpos_msi`/`mpos_serial_ring`/`mpos_fpu_trap`/`mpos_segments`/`mpos_console_input`)
        declared per target in `.cargo/config.toml`; `src/hal` `target_arch` is now 0, both
        bare-metal clippy targets and the depgraph are green
  - [~] 13.2 callers stop spelling the architecture: `target_arch` outside `src/arch/`
        (250 at audit → 169 now). Landed: `console.rs` 10 → 0 (`mpos_framebuffer_console`,
        `mpos_serial_ring`), `uevent.rs` 4 → 0 (`mpos_net`), `mm/mod.rs` 4 → 0
        (`mpos_dynamic_mmio_window`), `sched/runtime.rs` 8 → 0 of the FPU-trap protocol
        (`mpos_fpu_trap`), `syscall`'s socket dispatch + read/write legs 22 → 0
        (`mpos_net`), `rand.rs` 2 → 0 (a new baseline `hal::Entropy`, the seed sources moved
        into `arch/<arch>/`). And a later slice took six more out for a *different* reason:
        the four `ifreq` link ioctls (`fs/ioctl.rs` 22 → 18) and the unix-socket half of
        `fs/mod.rs`'s nonblocking push (4 → 2, both of the remainder the input driver's, which
        is a real absence) had been gated when `net` was the first machine's alone, so §13.4
        made their gates **stale** - a deletion, not a flag. `user/tests/src/bin/t-ifreq.rs`
        now drives all six requests on both machines, which is what keeps a gate from going
        stale there unnoticed again (a read arm and a write arm of one family disagreeing is
        exactly what it catches). And the register-ABI relocation's first stage landed:
        `hal::Gate` is the **gate frame's** seam - the operations over the opaque `*mut ()`
        the syscall gate passes - so `handle_execve`'s two per-machine arms became one
        `hal::set_exec_return`, and the seccomp filter's instruction pointer comes from
        `hal::user_ip` (`proc.rs` 23 → 20). It is deliberately a *separate* trait from
        `UserFrameOps`: on x86_64 the gate's frame is the entry stub's register block on the
        kernel stack, on aarch64 it is the task's own `UserFrame`, and no type both machines
        share can be named without inventing one - which would be the leak the seam exists to
        remove. Introducing it also made `linux_dispatch`/`linux::dispatch` **`unsafe fn`**,
        which is what they always were: clippy's `not_unsafe_ptr_arg_deref` (deny) found a
        `pub` fn dereferencing its raw-pointer argument behind a safe signature. Both machines'
        gates and the kernel's own self-check calls now say so explicitly. The **signal frame**
        followed it (Stage 2): `hal::SignalFrame` (`enter`/`exit`) is the capability, the x86
        `rt_sigframe`/`sigcontext` layout moved into `src/arch/x86_64/signal.rs` with the
        `SIGFRAME_*`/`UCONTEXT_*` constants out of `sched::signal`, and `proc.rs`'s
        `build_frame`/`handle_rt_sigreturn` became policy over `hal` plus the two scheduler
        calls the machine cannot make - so `struct RegsFrame` is deleted and the signal family
        is gated on `mpos_signal_frame`, not on an architecture - and Stage 3b gave the flag a
        second declarer: `src/arch/aarch64/signal.rs` lays arm64's `rt_sigframe` out behind the
        same trait, so the second machine delivers handlers too (the pin is
        `t-signal-frame`, and it found three defects on the way: a thread pointer zeroed by the
        frame's own `apply_frame`, the handler's `x0` overwritten with the call's result, and
        `orig_x0` missing from the trap frame for `SA_RESTART`'s rewind). `lib.rs`'s 16 module gates and
        `main/`'s per-arch arms stay by design; still open: `syscall` 41 (`ioctl` 18, `file` 9,
        `mod` 5, `proc` 3, `nr` 2, `fs/mod` 2, `abi` 2), `drivers` 36, `vfs` 7, `mm/{heap,elf}`
        4, `logging` 6 (gate now *stale*) - `sched` names no architecture at all since the
        ptrace/stop block's width became a machine fact. Stage 3a of the same line is the
        EL0 trap's register preservation: `t-regs-across-syscall` measured that this
        machine destroyed a program's SIMD state on its own `svc` (one of 41 registers,
        `v0`), and both EL0 trap stubs now carry the interrupted `q0..q31`/`FPCR`/`FPSR` in
        their own frame. Nothing above the machine layer changed for it, which is why the
        count is untouched
  - [x] 13.3 aarch64 disk root: the ext4 `check_fs` mounts is **registered** with the block
        layer (`vda`, major 254, `/dev/vda`), `xtask` lays the repository's `rootfs/` overlay
        and the three init images on that disk, and the machine now **switches its root** onto
        it - `/` is the disk, the ramfs moves to `/oldroot`, `/dev` is re-mounted, and `/init`
        is read from the disk and run as pid 1 (`M7_DISK_OK`, `M7_ROOT_OK`). The init's whole
        file lifecycle runs on ext4, which is the strongest form the claim has. *Deviation*:
        the disk is a bare ext4, not a partitioned image, so `rootfs.toml`'s
        `image_size`/`esp_size` geometry is still x86_64-only - a partition table buys nothing
        for a machine whose firmware boots from the ESP
  - [x] 13.4 aarch64 `net`: **landed - the second machine runs the same stack over its
        own adapter**. `drivers::virtio` carries the transport-neutral `NetDevice`/`VsockDevice`
        contracts and `net`/`net::vsock` hold `Arc<dyn …>`, naming no transport;
        `VSOCK_PER_CONNECTION_BUFFER` moved beside the contract and the `/proc/net` collector
        took `procfs`'s own gate, not a new capability name. `mmio::MmioNet` is the MMIO
        adapter as a `NetDevice` (persistent, like `MmioBlk`: the ring it posts stays posted),
        `check_net` opens it once, asserts the MAC and the TX queue **through that handle** and
        publishes it, `board/aarch64.rs`'s `init_net` hands it to `net::init` +
        `net::spawn_service` (`M8_NET_STACK_OK`), and `lib.rs`'s gate is `target_os = "none"`
        with `mpos_net` declared by both machines. Measured: the aarch64 sweep stages 10 of
        the 14 std-side programs and passes **10/10** - including a TCP
        connect/accept/read-write round trip, UDP over the loopback and an `AF_UNIX` name -
        which is the pin this job chose (real EL0 programs, no kernel-side ping hook). The
        flip exposed three stale "`net` is x86_64's" gates in shared code: `close(2)`'s
        `close_last_socket` and `unlink`'s `unix::unbind_node` (`not(aarch64)`; the first was a
        measured port *leak* - `t-socket`'s 34567 still held when `t-tcp` bound it) and
        `ioctl`'s two `ifreq_*` helpers (aarch64 arms answering `None`, so interface ioctls fell
        to `ENODEV`). And `/tmp` is a tmpfs on this machine now, as on x86_64: ext4 cannot hold
        a socket node, which is what `t-unix-rustix` measured (`EOPNOTSUPP` from
        `create(name, SOCK_FILE)`) before the mount. What the `aarch64` group still names is
        harness-only: `t-stdspawn`/`t-socketpair` run another program out of a `/bin` this
        machine's root does not have — and 13.6 gave it one, so the group is empty now
  - [~] 13.5 `cargo xtask test --arch aarch64`: the **silent wrong-run is closed** —
        `test --arch aarch64` refuses explicitly (`test_sweep_machine`, pinned by an
        `mpos-xtask` unit test) instead of falling through to the x86_64 sweep and reporting a
        verdict for a machine it never booted. Next: the aarch64 sweep itself — build the user
        programs for aarch64, pack them with a generated `/init` that runs `/bin/t-*`, boot
        under AAVMF and parse the same `=== RUN/PASS/FAIL/SUMMARY`/`END`; a group that cannot
        run on aarch64 yet is named in `tests.toml` with its reason, never silently skipped.
        Landed: cargo builds the musl user programs for aarch64 (the target names `rust-lld`;
        the host `ld` rejects `--fix-cortex-a53-843419`, measured — 28 programs, static
        `ET_EXEC`), and the sweep is wired — a musl runner (`aarch64_init::build_sweep_init`) is
        packed as `/init` beside them and the run reads the same protocol. The first run is
        **red with the reason located**: the runner's `posix_spawn` takes an EL0 write fault
        (ESR `0x9200004f`, address `0x3fadafe0`) at `pc 0x272d90` — inside the runner's own R+X
        segment, so it is musl's spawn and not a test — and the machine loses its init, so no
        summary prints. **Fixed**: the handler read `WnR` from `ESR_EL1` bit 7 instead of bit 6,
        so every fault looked like a read and the COW resolver was never reached. The run now
        passes that point and stops at the next gap — two faults in the runner (a write to
        `0xffffffffffffff5c`, translation fault L1; a read of `0x58`, L2) — where the aarch64 ABI
        is still short. **Both fixed**: a write fault now reaches the COW resolver (the `WnR`
        bit), and the outgoing task's live thread pointer is captured on switch-out (musl
        installs its base with a direct `msr`, so the record had gone stale and the base was
        reset to 0 on the first switch after a spawn). The payload also stages only ELF files
        now — cargo's `.d` dep-info files start with `t-` and were being run as programs
        (`exit=127`). **The run completes**: 14 programs, `=== SUMMARY pass=0 fail=14`,
        `=== END`. **And the same class struck again, in the copy**: `spawn_copy` and
        `spawn_thread` inherited `Task::tls` — the *record* — rather than the base the parent was
        running with, so the sweep's first `posix_spawn` child (whose parent had set its own base
        after its last dispatch, so the record still said 0) ran with `TPIDR_EL0 = 0` and faulted
        writing musl's `errno` at the same `0xffffffffffffff5c`. arm64 Linux's `copy_thread`
        hands the child the parent's live `tp_value` when `CLONE_SETTLS` is absent; both call
        sites now ask the register (`running_tls`). Measured: `t-rename` and `t-stdspawn` — the
        two that faulted — reach their own code and fail on the *harness* (no `/tmp`), not on the
        kernel. **Classified, and the sweep is green on what this machine can serve**: of the
        14 std-side programs, 3 run and pass (`t-rename`, `t-rmdir`, `t-stdargs` —
        `=== SUMMARY pass=3 fail=0`, exit 0) and the other 11 are named in `tests.toml`'s new
        `aarch64` group with their reasons. The vehicle is a rule that matters as much as the
        list: **a group named after a machine is that machine's own** — invisible to every other
        machine's sweep — so naming them cannot take them out of the x86_64 gate (pinned: every
        one of the 11 still runs there by default). Ten are the socket family (this machine's
        table answers `socket`/`socketpair` with ENOSYS, measured: syscall 198/199 — §13.4's
        flip is what closes it) and `t-stdspawn` is a *harness* gap, not a kernel one: its
        subject is other programs (`/bin/true|echo|cat|false|sh`), which the x86 harness has
        from busybox and this machine's disk root has no `/bin` for — while the kernel's own
        spawn path is exercised on every program by the runner itself. Reaching three took the
        machine setup the x86 `/init` does with busybox: the runner now makes `/tmp` (the x86
        side mounts a tmpfs; this one has no busybox and a writable disk root, so a directory)
        and hands `t-stdargs` the argv the x86 harness's PATH lookup hands it
        (`arg0("t-stdargs") one two three`), and a `=== SETUP_FAIL` line now refuses the
        verdict outright rather than letting an unprepared machine read as failing tests.
        `--groups`/`--all-groups` were refused for this arm on the day, because the
        file's vocabulary then also named the `no_std` raw-syscall suite, which has no
        aarch64 port; that suite finished migrating and was deleted, and §15.38 made
        both arms ask the same question — the flags mean the same thing on either
        machine now, and the only group neither may ask for is a machine's own `gaps`
  - [x] 13.6 aarch64 `/bin`: **the harness gap closes and the second machine's own list is
        empty**. The gap was never the ABI — `t-stdspawn` runs
        `/bin/{true,false,echo,cat,sh}` and `t-socketpair` runs `/bin/cat`, while the
        machine's ext4 root carried no `/bin` at all (the payload's own programs sit flat at
        `/`). **The tools are built in-tree, not installed**: `xtask/src/aarch64_bin.rs` is
        five applets of one multi-call binary, compiled by the
        `rustc`/`aarch64-unknown-linux-musl` path the sweep's runner and its musl helper
        already require. Installing busybox — the x86 harness's answer — would have put an
        Alpine fetch *inside the aarch64 gate*, whose failure would read as a broken kernel;
        keeping the gate's reds about the kernel is what `tests.toml`'s groups exist for. `sh`
        is a small shell and its own comment says exactly which subset it implements
        (`;`/newline separation, `'`/`"`/`\` quoting, `<`/`>`/`>>`/`>&N`, the
        `echo`/`exit`/`cd`/`pwd` builtins, external programs by path or `PATH`) — honestly *not*
        busybox — and it moves real descriptors instead of modelling them, because
        `t-stdspawn` asserts the child put `captured-err` on **stderr**. The staging step also
        learned the two things this needed: a payload name may carry a directory (the kernel's
        tar unpacker already creates missing parents on demand) and every file it lays down is
        mode 0755, which `mkfs.ext4 -d` preserves — without that `/bin` would land 0644 and
        `execve` would answer `EACCES`. **Measured: the aarch64 sweep is
        `=== SUMMARY pass=12 fail=0`**, `t-stdspawn`'s `output() captured both of the child's
        streams` and `t-socketpair`'s `read back what the pair carried` among the passes; x86
        is unchanged (`pass=142 fail=0`, the same 10 left out). `tests.toml`'s `aarch64` group
        is **empty** and stays as an empty group - the mechanism is what names a gap, and the
        day a call cannot be served is the day a name goes back in. (That day came, and §15.38
        renamed the group `gaps` under `[aarch64]` when `tests.toml` took the three sections:
        `t-arch-prctl` is the one name in it, an architecture rather than a missing handler.)
        What remains outside the machine is `external`, and its reason is the internet
  - [x] 13.7 aarch64 `SerialRing`: the PL011's line (SPI 33) is routed through the GIC and the
        second machine queues its console output instead of waiting on the wire — with the queue
        itself lifted out of `drivers/serial.rs` into `drivers::tx_ring`, behind a three-method
        `Transmitter` both machines implement. `M5_SERIAL_TX_OK` counts what writers queued against
        what the *handler* handed over, so a polled write (or a line that never fired) fails rather
        than passing; x86_64's sweep is unchanged
  - [x] 13.8 aarch64 interactive shell: `run --arch aarch64` stops booting a hand-encoded read/echo
        loop and hands pid 1 to the machine's real shell — an in-tree `/init` that makes `/tmp` and
        `execve`s `/bin/sh`, with the `sh` applet reading a line at a time (prompt on a terminal,
        silent on a pipe) and `ls` added to the `/bin` image. The superseded `looping_init_elf` is
        deleted rather than left as dead code, and both new behaviours are pinned by `t-stdspawn`
        on **both** machines
  - [x] 13.9 aarch64 `uaccess`: the range check accepts a page the tables do not describe yet
        (the live VMA table answers for an absent page, a copy-on-write page counts as writable,
        `len == 0` is accessible — the three rules x86_64 has always had), and the **EL1**
        synchronous entry resolves the data aborts that fallback makes reachable, because the
        kernel fills a caller's buffer through the caller's own tables. Pinned by a new
        `t-lazy-buffer`         (a 1 MiB `mmap` read into whole, and a `fork` child reading into a page
        its parent wrote) on both machines; both failed with `EFAULT` before. It also found a
        **named gap**: a non-empty `std::fs::remove_dir_all` faults at `pc 0` on this machine,
        which nothing in either suite had exercised — refuted and closed by 13.10, whose cause
        was a `struct stat` sixteen bytes too long rather than the tree walk the fault looked like
  - [x] 13.10 aarch64 `struct stat`: the writer filled 144 bytes where this machine's struct is
        128, so the length is now the ABI's own (`STAT_BYTES`) — for the bounds check and the
        zero-fill as well as for the fields. Found by §13.9's named gap, and the fault was a
        return through a zeroed link register: musl's `stat`/`lstat`/`fstat` do not hand the
        kernel the caller's struct at all (`__fstatat` stats into one of its own on the stack and
        copies the fields out), so the sixteen extra bytes landed in the frame past *that* struct
        — which is why every program's own `stat` call had survived them, and why what died was
        `fdopendir`, whose frame held the return its caller needed. Pinned by a new
        `t-stat-buffer` (the kernel handed a guarded `struct stat` through
        `SYS_fstat`/`SYS_newfstatat`; it reports two guard words written against the old length)
        and by `t-rmtree` (a flat and a nested non-empty tree removed with
        `std::fs::remove_dir_all`), both on **both** machines
  - [x] 13.11 `statx` on Linux's layout: `struct statx` is 256 bytes and one layout on every
        machine, and the writer filled 152 of them with its clocks at 96/112/128 and its rdev
        pair at 72 — where Linux puts four 16-byte timestamps at 64/80/96/112, `stx_rdev_*` at
        128/132 and the file's device at 136/140. Found by §13.10's fix and named there rather
        than folded in: 152 bytes into a caller's 256 is an *under*-write, not a write past, so
        it needed its own measurement. The clients that read those fields are the ones that
        call `statx` explicitly — uutils' probes and musl's own wrapper — because Rust's `std`
        has no `statx` on this libc and `rustix`'s `stat` family only reaches for it on 32-bit
        targets. Pinned by a new `t-statx-agree` (one file's device, inode, mode, size, link
        count and three clocks, read through `statx` **and** through `lstat`, which have to be
        the same numbers) on **both** machines, and by the bare-metal `t-statx`, whose clocks
        and struct length were the two things it had deliberately skipped — and that pin found a
        second, older one on the way: x86_64's `struct stat` wrote `st_mtime` and `st_ctime` at
        80 and 88, where Linux has `st_atime_nsec` and `st_mtime`, so a program reading either
        clock read the next one along and `st_ctime` (104) read zero. The three clock seconds
        sit at 72/88/104 in *both* ABIs, so they now come out of the per-machine branches and are
        written once
  - [x] 13.12 aarch64 real Alpine userland: **the second machine boots a genuine Alpine `aarch64`
        installation to a login prompt**. `xtask/src/aarch64.rs` installs one (`apk --arch aarch64` under
        `fakeroot`, so the build stays unprivileged *and* the image keeps root ownership - `mke2fs -d` must
        run under the same `fakeroot` state or it writes a root filesystem owned by the build user, which is
        what `login` reads `/etc/shadow` with), lays it on the machine's bare ext4, and boots it: `busybox`
        init reads `/etc/inittab`, `openrc` runs its runlevels, and `getty` prints `localhost login:` on the
        PL011. The new `cargo xtask alpine-aarch64` is the verdict. It carries **no initramfs** - the disk
        root is the whole userland, and the kernel's root switch is what puts it in reach, so the handoff
        learned to boot a disk root without one. The Alpine root's `/init` (the new `rootfs-aarch64/` overlay)
        does the two things a cross-arch install cannot: it makes busybox's applet farm at first boot (the
        post-install script is the *target's* binary) and mounts `/proc`, `/sys`, `/tmp`, `/run` and `devpts`.
        **Three real defects came out of it, each fixed here** (see §13's table for the measurements): `USER_LIMIT`
        was x86_64's `1<<47` on a machine whose `mmap` cursor starts at `MMAP_BASE = 0xff00_0000_0000`, so every
        `MAP_FIXED` mapping a dynamic loader makes answered `EINVAL` and the first `dlopen` failed
        (`librc.so.1: Invalid argument`); `/proc` and `/sys` were behind a module-level `target_arch` gate whose
        deferral was over; and `boot::set_cmdline` was never called on this machine, so `/proc/cmdline` was
        empty. Also landed: the console tty's node is the *machine's* serial name (`ttyAMA0` here, `ttyS0` on
        x86_64 - one node, the name the psABI gives). Measured: `cargo xtask alpine-aarch64` exits 0 on
        `localhost login:`, x86 gate `pass=151 fail=0`, host 511, `boot-aarch64` green, depgraph 9/9, fmt clean.
        **Named, not fixed:** the virtual consoles (`tty1`..`tty4`) cannot reach the wire because this machine
        has no console multiplexer, so exact parity there needs the `vcmux` port - **closed by §13.16**;
        `hwclock`/`modules` are x86-only services the shared runlevel table still enables here (no
        `/dev/rtc`, no `/lib/modules`) - **closed by §13.15**; and the install writes no
        `/etc/hostname`, so the prompt says `localhost`, which still stands. (Its third named gap -
        `syslog` unable to start - was **not** a filesystem fact at all and is fixed by 13.13: the kernel's
        `open(2)` flag numbers were x86_64's, so arm64's `O_LARGEFILE` was read as `O_NOFOLLOW`.)
  - [x] 13.13 `open(2)`'s flag **numbers** are the machine's: arm64's `asm/fcntl.h`
        overrides `O_DIRECTORY`/`O_NOFOLLOW`/`O_DIRECT`/`O_LARGEFILE` before including
        the generic header, so the kernel's one table - x86_64's - read a *different
        flag from the one the caller set* on aarch64. The overlap is not academic:
        arm64's `O_LARGEFILE` (`0x20000`) **is** x86_64's `O_NOFOLLOW`, and musl's
        `open()` adds `O_LARGEFILE` to every open, so **every `open()` of a symbolic
        link on aarch64 answered `ELOOP`**. Found by §13.12's boot: `openrc` could not
        start its syslog service - `Error loading shared library libcap.so.2: Symbolic
        link loop`, the dynamic loader opening the versioned symlink
        `libcap.so.2 -> libcap.so.2.78`. Diagnosed by instrumenting the dispatcher to
        print the number and arguments of any syscall answering `ELOOP` (there is
        exactly one producer, the `O_NOFOLLOW` branch): the log gave
        `syscall 56 args [AT_FDCWD, ptr, 655360, 0, 0, 0]`, i.e. `openat` with
        `O_CLOEXEC|0x20000`, and the aarch64 `musl-dev` header named `0x20000` as
        `O_LARGEFILE`. `O_DIRECTORY` and `O_NOFOLLOW` are now per-machine (and
        `O_DIRECT`, `O_LARGEFILE` documented as the other two arm64 overrides;
        `O_LARGEFILE` stays undefined because the kernel ignores it, which is what
        matters). Pinned by `user/tests/src/bin/t-open-flags.rs` on **both** machines -
        it drives `libc`'s own constants through this kernel and fails on three checks
        on the old table (`open(symlink, O_RDONLY|O_CLOEXEC)` is `ELOOP` instead of
        opening, `open(file, O_DIRECTORY)` succeeds instead of `ENOTDIR`, and
        `O_TMPFILE`'s pair is refused with `EINVAL` instead of being read as the pair)
  - [x] 13.14 a boot check was leaving the wall clock at the year 36812: `check_wallclock`
        (the `M6_WALL_OK` check, which runs on **every** aarch64 boot - not only under
        `cargo xtask boot-aarch64`, because this machine's boot runs its stage checks as
        the boot's own verdict, in front of the sweep or a distribution's `init`) proves
        `set_boot_epoch` moves the anchor by setting it to `1 << 40` and **never put it
        back**, so every program after the check read a `clock_gettime(CLOCK_REALTIME)`
        seconds value of 1099511627776 - `36812-02-20`. That is what killed `syslogd`:
        musl's `asctime_r` reaches `a_crash()` when the timestamp it formats does not fit
        its 26-byte buffer, and "36812-02-20 00:36:16" is 26 characters. Fixed by
        restoring the anchor (and checking the restore), with the reason written into the
        check: a check must leave the machine as it found it. Pinned by
        `user/tests/src/bin/t-localtime.rs` on **both** machines - the first test either
        suite has had for `localtime`/`asctime` - which reads the anchor back as
        `CLOCK_REALTIME - CLOCK_MONOTONIC`, ranges every `struct tm` field, and fails on
        the old kernel at `the year is a live one (got 36812)` before dying in
        `asctime_r` exactly as `syslogd` did. Measured after: aarch64 sweep
        `pass=23 fail=0`, x86 gate `pass=153 fail=0 (153 built)`, `alpine-aarch64` exit 0
        with `syslog` **running and logging** (`/var/log/messages` carries `syslogd
        started: BusyBox v1.38.0` and every `init:`/`crond` line after it, written
        through the daemon), `boot-aarch64` green, host 512, depgraph 9/9, fmt clean,
        clippy 0 errors
  - [x] 13.15 the second machine's `hwclock` and `modules` - §13.12's last two named services:
        the RTC device node (`/dev/rtc0`/`/dev/rtc`, the `RTC_*` ioctls, and the module
        index the `modules` service asks `modprobe` about) was x86_64's by a stale gate,
        and the PL031 underneath it was wrongly believed to be a read-only counter. Both
        closed; the Alpine boot's runlevel output is now `[ ok ]` on every line, with no
        `[ !! ]` and no `modprobe` warning at all. Pinned by
        `user/tests/src/bin/t-rtc.rs` on **both** machines. Measured after: aarch64 sweep
        `pass=24 fail=0`, x86 gate `pass=154 fail=0 (154 built)`, `alpine-aarch64` exit 0
        with `Setting system clock using the hardware clock [UTC] ... [ ok ]` and
        `Loading modules ... [ ok ]`, `boot-aarch64` green, host 512, xtask 55,
        depgraph 9/9, fmt clean, clippy 0 errors

**Architecture debt (Phase 11 — audit done, Steps A/B/C/D/E/F/G landed, HAL S1 landed (§11.4), V-1/V-4/V-13 closed)**
  - [x] 13.16 the second machine's virtual consoles (`tty1`..`tty4` - the four the shared
        `/etc/inittab` starts, of the six `term::vc::VC_COUNT` mints) - the `vcmux` port, and the
        assembler bug it was blocked on.
        **The port.** Measured before anything was written: un-gating `vfs::vcmux` costs exactly
        **7** errors and all seven are `drivers::{video::render,drm}` - seven references in all: four
        `render::notify` calls, `drm::master_held`, and the `revoke_now`/`grant_and_resent` pair - so
        the port is that seam and nothing else.
        Written: the display half moved to four `crate::console` seams (`display_notify`,
        `display_master_held`, `display_handover`, `display_card_grabbed`), each with the
        `#[cfg(mpos_framebuffer_console)]` / `#[cfg(not(..))]` pair whose no-machine body is a no-op
        or the answer a machine with no card owes - which is the pattern `console`'s
        `paint_grids_if_live` already used, and the mux keeps the headless case it always had
        (`grids_live` narrates a switch on the wire as `[ttyN]`); the `'V'`/`'K'` ioctl arms and
        `Tty::{vt,kd}_ioctl` un-gated to `target_os = "none"`; `pl011::drain_input` calls
        `vcmux::route_input` instead of the console tty (the 16550's handler's exact shape); and
        `vcmux::init()` goes up before the console interrupt is armed, the order x86_64's boot uses.
        Both kernels and the loader build; fmt clean. Pinned by `user/tests/src/bin/t-vt.rs` on both
        machines: the ioctl words are read out of the shipped headers, the mode and keyboard-mode
        families are round trips over `/dev/tty1`, the `VT_*` family runs over `/dev/tty0` (a serial
        tty answers `ENOTTY` to it, which is what the harness hands the program), the switch is
        driven with `VT_ACTIVATE`/`VT_WAITACTIVE` and *seen* on the wire, and the test leaves the
        machine on the console it found (`VT_GETSTATE` was printed rather than asserted on - the
        layout divergence that row pointed at is §13.18, now closed, and `t-vt` asserts the three
        shorts).
        **The blocker, and what it actually was.** With the port in, the aarch64 stage-2 context
        check died at the same address on every run - `EXCEPTION: synchronous`,
        `ESR_EL1 0x8600000f` (EC 0x21, an instruction abort at the *same* EL),
        `ELR_EL1 = FAR_EL1 = 0xffffffff80110380`, deterministic over three boots. That address is
        `KERNEL_VIRT_BASE + mpos_context_round_trip`, and the linker's own map (from
        `-C link-arg=-Map=`) says why it was not executable: the `global_asm!` block that defines the
        EL0 test program (`arch/aarch64/mod.rs`) opens `.section .rodata.el0_program, "a"` and never
        switches back, and every `global_asm!` in a crate is emitted into *one* assembly unit - so
        the three code blocks in `arch/aarch64/context.rs`, which name no section at all, were
        assembled into that data section. The map's input section `.rodata.el0_program` is `0x284`
        bytes and holds `__aarch64_el0_program` and then `mpos_enter_frame`,
        `mpos_context_round_trip`, `mpos_context_switch` and `mpos_task_trampoline`: the program is
        `0x28` of them and the routines the other `0x25c`. `.rodata` is mapped read-only (the W^X
        plan), so the first **call** to one was an instruction abort.
        So the earlier reading of this crash was right about the bytes and wrong about the cause. The
        fault address *did* hold a function's prologue (`stp x29, x30, [sp, #-128]!` is
        `mpos_context_round_trip`'s first instruction) - that is exactly what a call into a
        non-executable page looks like: the routine itself, not a return address overwritten by a
        stolen stack (the boot-stack concern recorded as §13.17 is not a bug - the kernel already
        refuses that region, and 13.17 says so). And the trigger is not the port's
        code either: nothing the port adds runs before stage 2, and what decides the placement is
        which blocks share an assembly unit, which the port perturbed only by changing how much code
        there is to partition. At HEAD the same writing happened to land in `.text`, which is what
        made this latent rather than already broken - the search for a "size" threshold was looking
        for the wrong variable.
        **Fixed** in `arch/aarch64/{mod,context}.rs` - the data block enters its section with
        `.pushsection` and leaves it with `.popsection`, so it cannot move anything assembled after
        it, and every code block names `.section .text.context, "ax"` so its placement cannot depend
        on emission order at all - and **pinned in `arch/aarch64/linker.ld`**, which now asserts each
        of the four asm code symbols is inside `__text_start..__text_end`: measured, the assertion
        fails the *link* against the old sources and names all four, which is where the mistake was
        made rather than two layers later at boot. Measured after: `boot-aarch64` exit 0
        (`ABABABABABABAM2_CONTEXT_OK: 13 hand-overs`, chain through `INIT_EXITED`); the aarch64 sweep
        `pass=25 fail=0` exit 0 with `t-vt` staged and passing (was 24); the x86 gate
        `pass=155 fail=0 (155 built)` (was 154 - `t-vt` runs on both machines); `alpine-aarch64`
        exit 0 on `localhost login:`, its getty reporting `Kernel 0.1.0 on aarch64 (/dev/tty1)` and
        no `[ !! ]`; host 512; xtask 55; depgraph 9/9 within the ratchet (`vfs/mod.rs -> drivers|net`
        fell to 0, which the ratchet allows); fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none` and the aarch64 loader.
        **The general rule this bug leaves behind** - three defences, all now in place, for any
        assembly that switches section: enter a section with `.pushsection` and leave it with
        `.popsection`; name the section in every *code* block (`.section .text.context, "ax"`) so
        placement cannot depend on emission order at all; and let `arch/aarch64/linker.ld`'s
        `ASSERT`s catch it - they fail the link and name the symbol, which is where the mistake was
        made rather than two layers later at boot. A bug that appears "when the image grows" is
        often this one: what changed is which blocks share an assembly unit, not the size. The
        symptom class is a faulting fetch landing on a function prologue whose address reads as
        "code in `.rodata`" - which is exactly what it was.
  - [x] 13.17 **not a bug**: the kernel does not hand back its own boot stack, and the record was
        never offering it. Found on the way as "the loader offers the kernel the stack it is running
        on", and the first half of that is true - the record's regions *are* the frame allocator's
        zones, and the 64-page boot stack does sit inside one of them. What was missed is the *kind*,
        and `MemKind::is_allocatable` answers it in its own doc comment: only `Usable` is
        allocatable, and `BootloaderReclaimable` - the kind the stack's region has, `mpos-boot`
        allocating it as UEFI `LOADER_DATA` - is excluded *because* it holds the stack the kernel is
        still running on. Measured by printing the record at boot: `stack 0x44f07000 in region
        0x44d9d000..0x456d1000 kind BootloaderReclaimable allocatable false`, and `pmm: 5 zones`
        built out of the record's five `Usable` regions.
        **The tell was in the work itself.** The "24 MiB discrepancy" that held the fix back is not a
        discrepancy: the carve changes nothing the allocator sees. With it and without it,
        `test --arch aarch64` reports `181 MiB` / `46345 frames` and `boot-aarch64` reports `205 MiB`
        / `52575 frames` - the only movement is the record's region count, `16` against `14`, which is
        the three-way split and nothing else. The two *totals* are two different boots: the sweep's
        initramfs is `12890 KiB` against `boot-aarch64`'s `420 KiB`, and the loader pays for it out of
        the same `Usable` regions. A number that moves for a reason nobody can name is evidence that
        the change does nothing, not noise to file away - and here it was the only signal that said
        so, because the change was otherwise plausible and its stated premise was half right.
        **What lands instead** is the invariant the whole thing was about, asked where the record is
        read: `boot::handoff::collect` refuses a record that lists the boot stack as allocatable
        memory, by name and with the address. Measured against the counterfactual of admitting
        `BootloaderReclaimable` to `is_allocatable`: `KERNEL PANIC: the handoff offers the boot stack
        at 0x44eef000 as allocatable memory: the kernel would hand back the stack it is running on` -
        so the check is not vacuous, it is what stands between the kernel and a loader that describes
        the stack as `Usable`. The loader is left alone: carving a `Reserved` hole in a region the
        allocator never touches buys nothing. The two WIP stashes that hold that carve are therefore
        dead - their other half is the landed port, and the carve is the correction above - so
        `git stash drop` on both is safe whenever someone wants the list tidy.
  - [x] 13.18 `VT_GETSTATE`'s layout is not Linux's. Linux's `struct vt_stat` is
        `{ v_active, v_signal, v_state }`; this kernel used to write `{ v_mode, v_num, v_state }` - the
        *video mode* where Linux has the console that is on the screen, and the console number where
        Linux leaves `v_signal` alone (its use is `SIGIO` delivery, which this kernel does not do);
        the third short was already Linux's, `1 << (console - 1)`. Found while writing `t-vt`, which then
        printed the three shorts and asserted only that the call succeeds - a test that asserted this
        kernel's answer would have been pinning the divergence. **Landed.** The `'V'` nr 3 arm
        (`src/vfs/tty.rs`) writes `term::vc::active()` into `v_active`, leaves `v_signal` untouched, and
        writes `vcmux::state_mask()` into `v_state` — measured on Linux 7.2.8 with all three shorts
        pre-filled `0xAAAA`: `v_signal` came back `0xAAAA`, `v_active` 1, `v_state` 67. Both pins now
        assert the shape rather than describe it: `user/tests/src/bin/t-vt.rs` takes the sentinel route
        (`[0xAAAAu16; 3]`, so "never written" is observable) and checks `v_active` names a console,
        `v_state` carries `/dev/tty0`'s always-open bit, and `v_signal` reads back the sentinel;
        `t-seat` (std side) asserts the same three claims through `[0xAAu8; 6]`, which is the fill that
        actually reads back `0xAAAA`, and the console's *mode* moved to the door that answers it —
        `KDGETMODE`, since Linux's `vt_stat` has no mode field. A drift note for whoever edits these
        two blocks next: `t-vt`'s comment records that `0xAAu16` is `0x00AA`, so an array of those is
        *not* the sentinel the assertion compares against.
  - [x] 13.21 the neutral checks become **one copy**, and the second machine's clock is measured against
        them. Phase 15's first stages, landed here because each found a *machine* fact rather than a
        refactor.
        **What moved.** `check_mm`, `check_sched` (with its five scenarios and the run record),
        `check_vfs`, `check_ipc` and `ensure_child` left the x86_64 body for
        `src/board/checks/{mm,sched,vfs,ipc}.rs`, called by both bodies. The dispatch helper they share
        (`linkat`-shaped: one entry into `syscall::linux_dispatch` from kernel context) lost its
        `#[cfg(target_arch = "x86_64")]` with the move - another stale gate: it was written when only
        the first machine had a table to dispatch into. The init seam became **one signature**,
        `fn(&BootInfo) -> !`: the aarch64 machine layer no longer copies the archive's pointer and
        length into two statics (`INITRAMFS_PTR`/`INITRAMFS_LEN` are gone) but stashes the address of
        `boot`'s own `BootInfo` local - the same value the first machine's body is handed, whose
        slices point into the direct map and whose frame never returns - so both machines read
        `info.initramfs`. A misattached doc comment (the x86_64 handoff's prose sitting above
        `check_mm`) went back onto `handoff_to_init`. Measured: x86 gate `pass=157 fail=0 (157 built)`
        exit 33, aarch64 sweep `pass=27 fail=0`, host 512, both bare-metal clippy 0 errors, depgraph
        8/8 within the ratchet, fmt clean - the x86_64 numbers *identical* to the run before the
        move, which is what makes it a move.
        **What adopting them on aarch64 cost, and what it found.** `checks::mm` and `checks::sched`
        run green there (`M0_OK`, `M2_OK`, `M4_OK`, and the `pmm`/`hhdm`/slab/alternation lines), and
        the second of them found a defect the machine had carried since its scheduler landed: **the
        clock never preempted.** x86_64's timer handler does three things - `tick`, `charge_cpu`,
        `preempt`, with EOI between the last two - and the aarch64 IRQ path did only the first, so a
        task that never yielded kept the core for ever. Wiring the twin the same way made
        `checks::sched`'s `preempted` scenario pass ("a task that never yielded was preempted for 10
        ticks"), and then killed every EL0 program on the machine: the sweep's first test took a
        level-3 translation fault at `0x221af0`, and the verdict's `/musl` helper never printed its
        line. **That second failure is the real find.** A trap from EL0 leaves the program's resume
        state in `SP_EL0`/`ELR_EL1`/`SPSR_EL1` - *system registers*, not on the stack - and the EL0
        **interrupt** stub, unlike the sync one, saved none of them: it saved x0..x18/x29/x30 and the
        FP state and returned. That is sound only while an IRQ always returns to the context it
        interrupted, which is what it was; the moment the clock can preempt, the task is switched
        out, another task's traps overwrite those three registers, and this task's `eret` resumes
        into whatever they hold - i.e. into another address space's idea of a program counter. Fixed
        in the stub (three words saved after the FP block and written back before the `eret`, with
        the same "scratch registers before the register file" ordering the FP state needs, `#736`
        frame, and `TRAP_IRQ_STATUS_OFFSET` asserted beside the other layout constants). With it in,
        the sweep is `pass=27 fail=0` with preemption live, and the verdict's `/musl` helper runs.
        **What preemption then cost the verdict, and the second fix.** `M6_INIT_OK` was printed after
        the handoff, under the words "it has not run yet" - true only while nothing could preempt the
        boot task on the way. With the clock live, a tick between the spawn and the print lets a
        short init run to completion, and pid 1 leaving prints `INIT_EXITED` and **halts** the
        machine: measured, the marker the verdict polls for was simply absent while the init had
        clearly run. The spawn and the two lines now happen inside one interrupt-masked window, and
        the lines go out through the machine's *polled* writer because `println!`'s queue can park a
        writer and parking with interrupts off is a deadlock - the same rule the failure reports
        beside it already followed.
        **`checks::vfs`/`checks::ipc` on aarch64: shipped, once the stub below was fixed.** Both
        reach their own markers on this machine (`M6_OK`, `M7_OK`, `M8_IPC_OK` - the syscall gate,
        the ramfs root, a mounted tree, two tasks on one fd, the device set, pipes, the tty line
        discipline and signals), and `cargo xtask boot-aarch64` is green with them wired. The
        **verdict's** boot was not: it took an instruction abort at EL1 on the next preemption after
        them - `EC 0x21`, `ELR_EL1 == FAR_EL1` at a direct-mapped address (`0xffff800043e4f000`),
        which is where the inference in this paragraph went wrong. It read "the machine resumes into
        a page-table frame", and the sweep's address (`0xffff80004309c000`) was one; the verdict's is
        the *console's 559-byte transmit buffer*, a heap allocation. What the fault actually was is
        §13.22: the kernel's own interrupt stub did not keep the resume state of the context it
        preempted, so the task the clock took the core from resumed at whatever the last trap on the
        core had left in `ELR_EL1`/`SPSR_EL1`. Both families therefore went back into the aarch64
        body after that fix, which is why this entry's "measured after" lists their markers - and the
        payload-shape correlation (the sweep's 34 initramfs members against the verdict's 3) is a
        statement about *where the buffer landed and which task was preempted*, not about the checks'
        assertions.
        Measured after: `boot-aarch64` exit 0 through the full marker chain - the machine's own
        `M4_CONSOLE_OK` … `M6_INIT_OK`, `INIT_EXITED` and the init program's `MPOS_*` lines, plus the
        shared `M0_OK`, `M2_OK`, `M4_OK`, `M6_OK`, `M7_OK`, `M8_IPC_OK`; aarch64 sweep
        `pass=27 fail=0` exit 0; x86 gate `pass=157 fail=0 (157 built)` exit 0; host 512; depgraph
        8/8 within the ratchet; fmt clean; clippy 0 errors on both bare-metal targets.
  - [x] 13.22 the kernel's own interrupt stub keeps the resume state it interrupted - the EL1 half of
        §13.21's EL0 finding, and the abort that held two check families out.
        **What was seen.** `EXCEPTION: synchronous`, `ESR_EL1 0x000000008600000f` (EC 0x21, an
        instruction abort at the same EL), `ELR_EL1 == FAR_EL1`, at a page-aligned direct-map address
        just after `check_serial_tx` had put its 512-byte line on the wire:
        `0xffff800043e4f000` on the verdict's boot, `0xffff80004309c000` on the sweep's. It needed the
        two shared check families wired (§13.21) to appear at all, which is what made it read like a
        bug *in* them.
        **How it was localized.** Three measurements, none of them a guess:
        1. *QEMU's exception log* (`-d int`) showed the last two `eret`s before the abort both
           returning to `0xffffffff80108048` - the task trampoline's `mov x0, x19`, one instruction
           after it enables interrupts - with no exception in between. A `ret`/`br` into a data page
           would have shown as a fetch with no eret before it; an `eret` to a stale address shows
           exactly this.
        2. *The registers at the abort*, caught at its vector entry under `gdb` (`hbreak` on the EL1
           sync vector, `-S` so the breakpoint is armed before the kernel image exists): `x19 = 0x22f`
           (559), `x20 = 0xffff800043e4f000` (the console's transmit buffer), `x30 =
           mpos_task_trampoline + 16`. The CPU was executing the trampoline's `blr x20` with a *copy
           loop's* scratch registers - i.e. a context that had no business being in the trampoline at
           all.
        3. *The stack*, read upward from `SP_EL1` while the machine sat in the halt loop: the EL1 sync
           stub's 160-byte frame and its `bl sync` return address are where the faulting context's
           `x0..x18`/`x30` are, which is how the interrupted register file above was attributed to the
           boot task inside `check_serial_tx` (its `x1`/`x2`/`x10` all point into the 559-byte buffer).
        **The mechanism.** An exception from EL1 does not put the interrupted `PC` and `PSTATE` on the
        interrupted stack - the CPU writes them to `ELR_EL1`/`SPSR_EL1`, one pair per core - and the
        EL1 IRQ stub called `irq`, which ends in `preempt`, which *switches tasks*. Saving `x0..x18`,
        `x29` and `x30` is the call ABI's answer and it is not enough: the incoming task's own traps
        overwrite the pair, so a preempted kernel context's `eret` returned to whatever the last trap
        on the core had recorded. Measured consequence: the boot task, preempted mid-`check_serial_tx`,
        resumed into the task trampoline that another task's dispatch had left in `ELR_EL1`, ran its
        `blr x20` with the copy loop's `x20`, and fetched instructions from a heap buffer. §13.21 fixed
        the EL0 twin of this (three words: `SP_EL0`, `ELR_EL1`, `SPSR_EL1`) and the EL0 stub's own
        comment already argued the rule; the EL1 stub needed two words of the same shape and kept
        none.
        **The fix.** `arch/aarch64/vectors.rs`'s `__vec_irq` saves `ELR_EL1`/`SPSR_EL1` at
        `TRAP_EL1_IRQ_STATUS_OFFSET` (176, after the 176 bytes of registers it already saved; frame
        192, asserted beside the other layout constants) and writes them back through `x9`/`x10`
        before the register restore, which is the only window they can be used in. The EL1 **sync**
        stub deliberately still saves none, and it now says why: nothing on that path switches tasks
        (two resolvers that allocate and copy), and a resolver taught to block would need the pair.
        **Pins.** The boot-level counterfactual, measured: with the two families wired and this stub
        unfixed, `boot-aarch64` fails 3 runs out of 3 at `check_serial_tx` with the abort above; with
        the fix it is exit 0 end to end - and `xtask`'s verdict now *requires* `M6_OK`, `M7_OK` and
        `M8_IPC_OK` in the chain, so the wiring that reaches the preempting path cannot be dropped
        silently. What is *not* pinned is the stub itself: the natural clobber is a valid kernel text
        address (another task's own interrupted `PC`), so a non-fatal corruption is invisible, and a
        sharp pin would need an instrument that writes `ELR_EL1` - a machine register no kernel
        operation writes - which `AGENTS.md` §3 rule 4 does not buy. That gap is the reason the
        measurement above is written down with its addresses rather than summarized.
        Measured after: `boot-aarch64` exit 0 with the full chain plus `M6_OK`/`M7_OK`/`M8_IPC_OK`;
        aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 on `localhost login:`
        (the machine's own userland, checks off); x86 gate `pass=157 fail=0 (157 built)` exit 33; host
        512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none`, the aarch64 loader and xtask.
  - [x] 13.23 the boot's reporting, its exit hooks and its log sink become one copy each - Phase 15's
        stages 15.4 and 15.5, and the first capability gates the input stack's own switch.
        **What moved, and what was measured before it.** Five functions that were x86_64's alone:
        `banner`, `check_boot_stack`, `park` (`hal::halt` on the second machine instead),
        `report_stack_use` and `panic_context` (an empty `fn {}` on the second machine). Every line in
        them is a function of `BootInfo`, the scheduler, or a register read, so they are now
        `src/board/report.rs`, called by both bodies; the one machine question left in them -
        "which register holds the stack pointer" - became `hal::Cpu::stack_pointer` (`rsp` / `sp`), so
        the walk itself is not machine code. The panic walk also stopped carrying a *base constant*:
        it recognizes return addresses inside `arch::kernel_sections().text`, which is the exact
        answer rather than x86_64's `0xffff_ffff_8000_0000` floor, and one a port cannot get wrong.
        The boot-stack canary was spelled **twice** - once in `mpos-boot`, once in
        `board/x86_64.rs` - each with a comment claiming there was "no crate between" the two sides;
        the leaf both already import (`crates/bootinfo`, which carries `HANDOFF_MAGIC`) has it now, so
        the two ends cannot drift into a canary that reports a misspelling as an overflow. The second
        machine gains the check itself: measured on the verdict's boot, `boot stack: intact after the
        self-tests` and `boot stack: intact under hand_to_init` - the same two checkpoints, reading
        the same word the loader wrote through the same `boot_stack_base` field.
        **One exit-hook registry.** `steps::register_exit_hooks` is now the one list, in the one order
        (locks, then the state keyed by the open file description: socket, inotify watch, evdev node,
        uinput device), with the two input hooks under `mpos_input_registry`. Before it, x86_64
        registered five hooks plus two `FD_CLOEXEC` ones and aarch64 one - the socket's - so the
        second machine released a dying process's record locks and inotify watches never, and a
        `FD_CLOEXEC` evdev position could not exist there to be told about. The *stack's* gates moved
        with it: `drivers::virtio::input`, `drivers::uinput` and the `/dev/input` + `/dev/uinput`
        block in `drivers/devices.rs` were `#[cfg(target_arch = "x86_64")]` and are now
        `#[cfg(mpos_input_registry)]` - the capability the boot, sysfs' `build_input` and these hooks
        all ask, so a port that lands the input stack flips one switch instead of editing four files.
        **The net bring-up.** `net::init` + `net::spawn_service` + the "up at 10.0.2.15/24 …
        MAC" line were written in both bodies; they are `steps::bring_up_net(dev)` now, and each
        machine keeps only "find the adapter" (PCI bus vs device-tree MMIO window) and its own honest
        line when there is none. x86_64's line had no MAC and aarch64's did; the shared one prints it
        on both. The `mpos.selftest` question was the same story at one line's scale - a closure in
        one body, an `.any(..)` in the other - and it is `boot::cmdline_has(word)` now, next to
        `boot::cmdline` whose grammar it is: two boots that read the word differently are two boots
        that run different boot sequences for one command line.
        **The log sink, and a gate whose premise had expired.** `src/logging.rs`'s subscriber - the
        whole `tracing` sink - was `#[cfg(all(target_os = "none", target_arch = "x86_64"))]` with the
        reason written beside it: "aarch64 has neither [console nor clock] until its console (stage 4)
        and clock (stage 1c) land". Both landed stages ago and the gate stayed, so the second machine
        had **no sink at all** and every `tracing` line in shared code was silently dropped there. It
        is `#[cfg(target_os = "none")]` now - one subscriber, one level vocabulary, both machines -
        and the honest measurement is that this buys parity rather than output: the kernel has **0**
        `tracing`/`log` call sites on *either* machine, so nothing emits into the sink yet. The
        aarch64 kernel grows by it (1817 → 1843 KiB), and "the kernel's logging story is
        unbuilt" is the gap that leaves, named here rather than papered over by the flip.
        Measured after: `boot-aarch64` exit 0 with the full chain, the two `boot stack: intact` lines
        and the banner (`hhdm offset`, `memory map (16 regions)`, `allocatable: 205 MiB`) on the
        second machine's wire; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at
        `mpos login:` (its own userland, checks off, banner and canary check included); x86 gate
        `pass=157 fail=0 (157 built)` exit 33 with host 512 - and the x86_64 boot still printing
        `boot stack: intact after the self-tests` / `under spawn_init` from the shared function; both
        bare-metal targets and the aarch64 loader build; depgraph 8/8 within the ratchet; fmt clean;
        clippy 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and
        xtask.
  - [x] 13.24 the second machine's input stack is the **same registry**, and the port's flip is one
        error - Phase 15's stage 15.2, first class (the input registry).
        **Measured before the seam was designed** (§13.4's rule: measure the flip before designing the
        seam). Declaring `mpos_input_registry` for `[target.aarch64-unknown-none]` and building costs
        exactly **one** error: `E0432: unresolved import super::hal::transport_all` at
        `drivers/virtio/input.rs:40`, whose item is `#[cfg(target_arch = "x86_64")]` in
        `drivers/virtio/hal.rs`. The plan predicted **four** (three more in `vfs::sysfs::build_input`);
        those are gone because §13.23 flipped that block, the devfs block and the exit hooks from
        `target_arch` to the capability - so the flip is one word in `.cargo/config.toml` and one
        import, which is what the capability was introduced to mean. Everything else the second
        machine gains by declaring it (`drivers::uinput`, `/dev/input` + `/dev/uinput`,
        `/sys/class/input`) already compiles the moment the word is there.
        **The contract.** `drivers::virtio::InputDevice` is what the registry holds: the device's name,
        `ev_bits`, `abs_info`, `prop_bits` and `ids` - the capability picture a reader asks for before
        any event - and `pop_pending_event`. `VirtioInputDev<T: Transport>` is the one implementation
        of those six over the crate's `VirtIOInput`, so a second transport supplies a `T` and no second
        copy of them; `input.rs`'s registry, `read_caps` and `service` traded
        `VirtIOInput<HalImpl, PciTransport>` for `Arc<dyn InputDevice>`. The one door in is
        `input::register(devices)`, which takes the contract. `input::init()` - the PCI enumeration -
        remains the first machine's way in and is `#[cfg(target_arch = "x86_64")]`;
        `mmio::input_device(virt, size)` is the second's.
        **The device stays open.** `mmio::probe_input` opened the window, negotiated, read the device's
        configuration-space identity and dropped it - a proof that the transport reached the class,
        and nothing anyone could read. It is `mmio::input_device` now: the live handle goes to the
        registry, and from there it *is* the machine's keyboard. `check_input` (still in `devices()`,
        where the device tree is read and before there is a `/dev`) opens it and registers it, and the
        aarch64 boot's wire carries `virtio-input: event0: QEMU Virtio Keyboard`.
        **The marker moved, and why.** `M8_INPUT_OK` used to be printed by the transport probe. The pin
        this class can carry is the registry's own - event flow needs a keystroke and the harness is
        headless - and reading the registry *through a path* needs a mounted `/dev`, which exists only
        after `check_dev`. So the class's marker is `check_input_registry()`'s now, and it asserts three
        things: the registry is not empty, `/dev/input/event0` resolves through the mounted `/dev`, and
        the node reports `rdev() == (13, 64)` - evdev's own pair, the one udev keys its database by and
        libinput finds the device through, which a node reporting `0,0` fails on a machine that looks
        healthy everywhere else. `M8_INPUT_FAILED` is unchanged and still the terminator, used by both
        failure points (a window that will not open, a registry that reads back wrong).
        **The pump.** The task that harvests the device rings was wholly x86_64's (`spawn_service`),
        because it also drove `net::vsock` and the GPU's `VIRTIO_GPU_EVENT_DISPLAY`. It is gated on
        `mpos_input_registry` now, with those two halves `#[cfg(target_arch = "x86_64")]` where their
        drivers live - so both machines run one shared task's input half and the second machine does not
        grow a second copy of the loop. The aarch64 body starts it beside `init_net`, which is where a
        scheduler exists for it to park in.
        Measured after: `boot-aarch64` exit 0 with
        `M8_INPUT_OK: the evdev registry holds 1 device(s), /dev/input/event0 resolves as (13, 64)`,
        no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at `mpos
        login:` carrying the same two input lines (its checks run on every boot); x86 gate
        `pass=157 fail=0 (157 built)` exit 33 with host 512; depgraph 8/8 within the ratchet; fmt
        clean; clippy 0 errors on `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader
        and xtask.
  - [x] 13.25 the second machine's host-driven console is the same `/dev/hvc0` - Phase 15's stage
        15.2, second class (virtio-console).
        **Measured before the seam was designed** (§13.4's rule). Un-gating
        `drivers/virtio/console.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport`, the PCI probe helper the module's `init` reaches for. Nothing else in
        it is x86_64's - the `CharDevice` face, the blocking read and the write-through are pure
        policy over the crate's driver. The device side was measured too, because a class nobody can
        attach is not a class: `qemu-system-aarch64 -device help` answers `virtio-serial-device` (bus
        `virtio-bus`, i.e. the MMIO transport) and `virtconsole`, so the second machine can carry the
        same shape the first machine attaches on PCI (`virtio-serial-pci` + `virtconsole`).
        **The contract.** `drivers::virtio::ConsolePort` is what the kernel holds: `size`, `drain`,
        `send` - the three things `/dev/hvc0`'s stream face and the boot's own probe ask. The crate
        supports one port and fixes its queues at 0 and 1, so `virtio-serial-device` carrying a single
        `virtconsole` is exactly its shape. `VirtioHvc<T: Transport>` is the one implementation of
        the three over the one crate driver - so the second transport supplies a `T`, not a second
        copy - and it lives here rather than in `mod.rs` because this module is compiled on both
        machines, which `input` is not (it is capability-gated, and that is why its adapter is beside
        its contract). The global is `Arc<dyn ConsolePort>` now, `console::install(port)` is the one
        door, and `init_global()` - the PCI probe - is `#[cfg(target_arch = "x86_64")]`.
        **The machine.** `mmio::console_device(virt, size)` opens the window the device tree named as
        the same `VirtIOConsole` over `MmioTransport`. `check_console` (in `devices()`) finds the
        window whose `DeviceID` is `3`, opens it, installs it, and exercises the write face with a
        probe line the port's queue has to take - which claims nothing about a reader, because the far
        end is a host socket. `check_console_dev()` runs after `check_dev` and resolves both names the
        port answers to, `/dev/hvc0` and the `hvc` alias Linux keeps for "the first one", through the
        *mounted* `/dev`. The devfs gate went with the transport: the `#[cfg(target_arch = "x86_64")]`
        on the `hvc0` registration is deleted, so the node is published by whichever machine opened
        the port - absent, not faked, on a machine whose window never opened.
        **The marker.** `M8_CONSOLE_OK` is not a terminator and `M8_CONSOLE_FAILED` is;
        `xtask/src/aarch64.rs`'s `TERMINATORS` grew by that one line (24 → 25), and `aarch64_qemu`
        attaches the device: `-chardev socket,id=console0,path=build/aarch64-console.sock,server=on,wait=off`
        beside `-device virtio-serial-device -device virtconsole,chardev=console0`. The socket is the
        first machine's arrangement (`build/…/console.sock`): `wait=off` keeps a machine with no
        reader from blocking on the port it just opened.
        **Named, not fixed.** `/dev/hvc0`'s device number is `(0, 0)` on **both** machines, because
        `DevFs::register` gives a `CharDevice` no number - Linux's `hvc0` is `229:0`, and
        `register_id` is the door for a real one. The verdict therefore asserts the two names resolve
        and not a number, since there is no number to pin; fixing it would change the first machine's
        node too and belongs with whichever class next touches the tty numbering, not here.
        Measured after: `boot-aarch64` exit 0 with `virtio-console: up at /dev/hvc0` and
        `M8_CONSOLE_OK`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit
        0 at `mpos login:` carrying the same two console lines; x86 gate `pass=157 fail=0 (157 built)`
        exit 33 with host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask.
  - [x] 13.26 the second machine's display is the **same card and the same render task** - Phase 15's
        stage 15.2, third class (virtio-gpu + framebuffer console), and the one class whose seam was
        not one helper.
        **Measured before the seam was designed** (§13.4's rule), and the measurement was two-sided
        in a way the first two classes were not. Un-gating `virtio/gpu.rs` + `gpu_ctrl.rs` + `video/`
        + `drm/` and declaring `mpos_framebuffer_console` for aarch64 costs **3 errors**, all PCI-only
        hal helpers inside `gpu.rs`'s `init` (`transport`, `super::hal::device_config`,
        `super::hal::claim_drm_card`) - a number that reads like `net`'s and is not. The great majority
        of the stack needed nothing: **one** `pci::` reference in the whole 7.6 kLoC of `gpu.rs`,
        `gpu_ctrl.rs`, `drm.rs` (6680 lines) and `video/render.rs` (946), and `src/console.rs`'s
        framebuffer arm already had `#[cfg(not(mpos_framebuffer_console))]` no-ops on the other side.
        What the number hid is that **`virtio-drivers`' `Transport` is not object-safe** -
        `begin_init<F: Flags + BitAnd + Debug>` is a generic method and `VirtQueue::new`/`add_notify_wait_pop`
        take `&mut T where T: Transport` - so there is no `Box<dyn Transport>` shortcut and the type
        had to be erased somewhere else.
        **Where it was erased.** `gpu_ctrl.rs`'s `Gpu` became `Gpu<T: Transport>`, and the new
        `GpuDevice` trait names exactly the sixteen commands `video::render` sends (and nothing else:
        a method no caller has is a promise nothing keeps) with `impl<T: Transport + Send> GpuDevice
        for Gpu<T>` delegating to the inherent ones. `render.rs` - which owns the one device in a
        `static` and is the only context that may call `flush` - holds `Box<dyn GpuDevice>` and passes
        `&mut dyn GpuDevice` to its helpers, so the render task, the DRM/KMS layer above it and the
        framebuffer below it are one copy on both machines. That is the whole of the refactor: no
        command, no order and no default changed, which is what `t-fb` and `t-drm` in the x86_64
        sweep are the evidence for.
        **The machine's half.** `VirtioGpuDev::install(device, config)` is the one door - it takes the
        *device*, asks the host's resolution, builds the resource and pins the frames, so both
        machines share it. `gpu.rs::init_global()` stays the first machine's: it probes the PCI bus,
        records the function's device-configuration window (`super::hal::device_config`) and *claims*
        the function as the card's home for `sysfs` (`super::hal::claim_drm_card`). The second
        machine's is `mmio::gpu_device(virt, size)`: it opens the window the tree named, and its
        configuration window is one constant offset - `virt + 0x100`, the virtio-mmio device-specific
        space - where PCI has to walk capabilities to find the same bytes. `check_gpu` (in `init`,
        before `check_dev`) finds the window whose `DeviceID` is `16`, opens it, and spawns the render
        task; it stands there rather than in `devices()` because the task needs the scheduler
        `steps::start_scheduler` started, and before `check_dev` because that call is what publishes
        `/dev/fb0`. `check_gpu_dev()` runs after it and asserts three things: the card is installed,
        `/dev/fb0` resolves through the *mounted* `/dev`, and one row of pixels (`width * 4` bytes)
        reads back whole - a node whose length was a claim rather than a mapping short-reads there.
        It also asserts the render task is running, which is the part a class with no compositor
        could otherwise fake: `write(2)` to this node parks until that task has flushed, so a card
        installed without it is a node a writer blocks on for ever. A pixel *value* is deliberately
        not asserted - the buffer holds whatever the host last put there, and a boot has no way to
        say what that is; the write path is `t-fb`'s, and the sweep runs it on this machine.
        **The framebuffer console came with it.** `mpos_framebuffer_console` is declared for aarch64,
        so `src/console.rs`'s framebuffer arm is live there - the grids paint into the scanout and
        `grids_live()` still decides whether the kernel's log goes to the screen, so a boot whose
        console list names the wire alone is unaffected (which is why the verdict and Alpine boots
        read identically before and after).
        **Named, not fixed.** The second machine has no `/dev/dri/card0` and no `/sys/class/drm`: both
        hang off a *PCI function*, which only the first machine has, and `sysfs`'s `build_drm` finds
        its card through that mark. The card itself is the same device - `t-drm`'s subject - so this
        is a statement about the bus and the sysfs tree, not about the display. The DRM device node
        is the next class's problem if it is one at all.
        Measured after: `boot-aarch64` exit 0 with `virtio-gpu: 1280x800, 4000 KiB at /dev/fb0` (the
        host's own preferred mode, followed rather than assumed) and `M8_GPU_OK`, no `EXCEPTION`;
        aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at `mpos login:` carrying the
        same card lines; x86 gate `pass=157 fail=0 (157 built)` exit 33 with host 512 and `t-fb`/
        `t-drm` green; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines (aarch64 lib 47, x86_64 lib 1) - 7.6 kLoC of graphics code joined
        the second machine's build and added no lint.
  - [x] 13.27 the second machine's `/dev/dsp` is the same sound card - Phase 15's stage 15.2, fourth
        class (virtio-sound).
        **Measured before the seam was designed** (§13.4's rule). Un-gating
        `drivers/virtio/sound.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport` - the same single helper `console` cost, and for the same reason (the
        PCI probe is the only machine-specific line in the module). The device side was measured too:
        `qemu-system-aarch64 -device help` answers `virtio-sound-device` (bus `virtio-bus`, the MMIO
        transport), and the audio backend the first machine already uses (`-audiodev none,id=snd0` - a
        backend that consumes the stream and plays silence) is QEMU's, not the machine's.
        **The contract.** `drivers::virtio::SoundCard` is the three methods the device face and the
        boot actually ask for: `format`, `configure`, `play`. `VirtioSnd<T: Transport>` is the one
        implementation, so the card's whole OSS face - the parameter ioctls, the rate rounding, the
        period-sized staging buffer, the blocking hand-off - is one copy, and a second transport
        supplies only a `T`. `sound::install` is the one door; `install_probed` holds the probe's
        absent-vs-refused distinction (a machine with no card is a quieter boot, a card that answered
        and would not play is worth a line); `init_global()` - the PCI probe - is
        `#[cfg(target_arch = "x86_64")]`. The `Probe` enum became `pub` because `VirtioSnd::new` now
        returns it, which is the one API change the split needed.
        **The machine and the pin.** `mmio::sound_device(virt, size)` opens the window the tree named
        and hands it to `install_probed`, so the stream listing, the rate intersection, the format
        check and the initial `SET_PARAMS`/`prepare`/`start` are the first machine's code running over
        the second machine's transport. `check_sound` (in `devices()`) finds the window whose
        `DeviceID` is `25` and prints the same `44100 Hz, 2 channels` line the first machine prints;
        `check_sound_dev()` runs after `check_dev` and asserts the card is installed, `/dev/dsp`
        resolves through the *mounted* `/dev`, and the stream takes a 512-byte burst in full. That
        last one is the data path - `pcm_xfer` through the crate, into the host's `none` backend - and
        not just the config space, which is the one thing a node that merely resolves could otherwise
        fake. The burst is silence on purpose: the backend plays nothing, and a machine whose backend
        did play would still be asked for silence rather than noise.
        **Named gap, in this class's own coverage.** The OSS *parameter* face
        (`SNDCTL_DSP_SPEED`/`CHANNELS`/`SETFMT` and their cousins) is exercised by `user/abitest`'s
        `t-dsp`, which the aarch64 sweep does not carry - and `check_sound_dev` deliberately does not
        move the stream's rate, because a boot check that rebuilt the running stream would be changing
        the machine it is checking (§13.14's rule). So this class's marker says the card is there and
        moves bytes; it does not say the ioctls work on this machine.
        **One flake, measured and not believed.** The first x86_64 gate run after this change reported
        `FAIL t-clock-gettime` on its monotonicity line ("two reads never go backwards": two successive
        `CLOCK_REALTIME` reads, where the wall clock is the CMOS second plus a tick-derived fraction
        sampled around it). Nothing in this class touches the clock, and the re-run is
        `pass=157 fail=0 (157 built)` exit 33 - so it is a wall-clock flake, recorded rather than
        fixed: it is neither this class's subject nor new (the same shape as the `rand` flake
        `AGENTS.md` §5 already names).
        Measured after: `boot-aarch64` exit 0 with `virtio-sound: 44100 Hz, 2 channels at /dev/dsp`
        and `M8_SOUND_OK`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64`
        exit 0 at `mpos login:` carrying the same card lines; x86 gate `pass=157 fail=0 (157 built)`
        exit 33 with host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines.
  - [x] 13.28 the second machine's `AF_VSOCK` is the same family over the same kind of device - Phase
        15's stage 15.2, fifth class (virtio-vsock), and the one class whose device the host cannot
        always provide.
        **Measured before the seam was designed** (§13.4's rule). Un-gating `drivers/virtio/vsock.rs`
        for aarch64 costs **one** error: `E0432: unresolved import super::hal::transport` - the same
        single helper `console` and `sound` each cost. And, as with the input registry, the number hid
        the design: `VirtioVsock` held `PciTransport` *inside* its own struct (the trap `AGENTS.md` §5
        names), so the module could not compile for a second machine until the type was erased. The
        device side was measured too: `qemu-system-aarch64 -device help` answers `vhost-vsock-device`
        (bus `virtio-bus`, the MMIO transport) beside the PCI form, and the crate's
        `DeviceType::Socket` is `19` - the `DeviceID` an MMIO window answers.
        **The contract.** `drivers::virtio::VsockDevice` was already there (it is what
        `src/net/vsock.rs` holds), so this class needed no new contract: `VirtioVsock` became
        `VirtioVsock<T: Transport>` - the crate's `VirtIOSocket` over whatever `Transport` a machine
        opened - and its ten methods, which the family's phases, ports and accept queue all sit on,
        are one implementation over the one crate driver. The PCI probe became `init_global()` (a free
        function answering `Arc<dyn VsockDevice>`, `#[cfg(target_arch = "x86_64")]`), the module lost
        its `#[cfg(target_arch = "x86_64")]`, and `net::vsock::init(dev)` is handed the contract
        rather than a bus, so nothing above `drivers` learns which machine it is on.
        **The machine and the pin.** `mmio::vsock_device(virt, size)` opens the window the tree named
        and hands the device to `net::vsock::init`; the board's `check_vsock` (in `devices()`) finds
        the window whose `DeviceID` is `19`, prints the same `virtio-vsock: guest cid N answers on
        AF_VSOCK` line the first machine prints, brings the family up and asserts it answers the
        device's own CID - the one claim `getsockname` rests on - then prints `M8_VSOCK_OK`. The pump
        was already shared: `virtio::spawn_service`'s vsock half lost its `#[cfg(target_arch =
        "x86_64")]` and is now the contract's, like the input half before it (a machine whose device
        never probed has no family, and `net::vsock::service` returns without touching one).
        **The class no host can always provide, and the loud skip.** vsock is the one class this phase
        has driven whose device QEMU does **not** emulate in userspace: `vhost-vsock-*` is backed by
        the host's vhost kernel module, and QEMU *refuses to start* a machine naming the device without
        `/dev/vhost-vsock`. Attaching it unconditionally would stop `boot-aarch64` booting on a bare
        host - the trade `machine.toml` already refuses for the first machine ("a boot that never
        starts is worse than a machine with no vsock"). So xtask attaches
        `vhost-vsock-device,guest-cid=4` only when `/dev/vhost-vsock` exists (CID `4`, not the first
        machine's `3`, so the two machines' boots can be up at once without colliding), requires
        `M8_VSOCK_OK` only then, and otherwise prints which case it took - "the host has no
        /dev/vhost-vsock, so this boot attaches no vsock device and M8_VSOCK_OK is not required" - and
        its one-line summary says "seven devices" rather than "eight". The skip is never silent: the
        kernel says the absence in its own voice too (`virtio-vsock: no device on the tree; AF_VSOCK
        answers EAFNOSUPPORT`). `M8_VSOCK_FAILED` (a window that exists and will not open) is a
        terminator, `M8_VSOCK_OK` is not (`TERMINATORS` 27 → 28).
        **A correction found in passing.** `virtio::spawn_service` also drove the GPU's
        `VIRTIO_GPU_EVENT_DISPLAY` poll under the same `#[cfg(target_arch = "x86_64")]`, and its
        comment still said the second machine had "no GPU driver yet" - false since §13.26. That gate
        went with the vsock one, so the shared task now polls everything both machines have, and the
        poll is honest with no card (`CONFIG` is 0, so it answers `false` and costs one read a tick).
        Measured after: `boot-aarch64` exit 0 with `virtio-vsock: guest cid 4 answers on AF_VSOCK` and
        `M8_VSOCK_OK`, no `EXCEPTION`; the skip path was measured too (the host check forced false for
        one run): exit 0, the kernel's absence line, xtask's skip line, and a summary claiming seven
        devices and no `M8_VSOCK_OK`; aarch64 sweep `pass=27 fail=0` exit 0; `alpine-aarch64` exit 0 at
        `mpos login:` carrying the same vsock line; x86 gate `pass=157 fail=0 (157 built)` exit 33 with
        host 512; depgraph 8/8 within the ratchet; fmt clean; clippy 0 errors on
        `x86_64-unknown-none`, `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts
        unchanged on both machines (aarch64 lib 47, x86_64 lib 1).
  - [x] 13.29 the second machine's `/mnt/share` is the same host share - Phase 15's stage 15.2, sixth
        class (virtio-9p), and the one that cost three separate findings, two of them defects the
        first machine had carried all along.
        **Measured before the seam was designed** (§13.4's rule). Un-gating `drivers/virtio/fs9p.rs`
        *and* `vfs/p9.rs` for aarch64 costs **one** error: `E0432: unresolved import
        super::hal::transport_all` - the same single helper `console`/`sound`/`vsock` each cost. The
        design cost the number hid was the usual one: `Virtio9p` held `PciTransport` inside its own
        struct and `vfs/p9.rs` named `Arc<Virtio9p>`, so the type had to be erased again. The device
        side was measured too: `qemu-system-aarch64 -device help` answers `virtio-9p-device` (bus
        `virtio-bus`, the MMIO transport) beside the PCI form, and the crate's `DeviceType::_9P` is
        `9` - the `DeviceID` an MMIO window answers.
        **Finding 1: the probe had no caller.** `fs9p::init_global()` - the only function that fills
        the `DEVICES` registry - was called from nowhere in the tree. So `fs9p::devices()` was always
        empty, `mount_share()` printed `virtio-9p: no share on the bus` on **every** boot of **both**
        machines, and `/mnt/share` was never mounted - while QEMU exported `-virtfs
        ...,mount_tag=hostshare` on each of them. Measured in the logs (`build/serial.log` line 61,
        `build/test-serial.log` line 101) and by history (`git log -S 'fs9p::init_global'` never
        found the string, so the call never existed - the mount was written against a registry
        nothing filled). PLAN had the *symptom* on record - §6's DRM block already notes "the boot
        prints `virtio-9p: no share on the bus`, so there is no file channel into the guest" - as an
        instrument note, with no remediation. The driver, the client and the `mount -t 9p` dispatch
        were all complete and host-tested, and none of them had ever run.
        **Finding 2: the client speaks a dialect this host's QEMU refuses.** With the probe wired, the
        mount failed with `Unsupported`. Measured, in one boot, by asking for each dialect in turn:
        QEMU 11.1.1 answers `"unknown"` to a plain `9P2000` `Tversion`, and `9P2000.u` to a `9P2000.u`
        request (and `9P2000.L` to `.L`). So the plain dialect the client asked for is one no modern
        server serves - a second, independent reason the share was unreachable. The client now speaks
        **9P2000.u**: the version string, `n_uname` on `Tattach`, the `extension`/`n_uid`/`n_gid`/
        `n_muid` tail on `Twstat`'s `statu` record. Everything else on the wire is unchanged - walk,
        open, read, write, clunk, remove and `Ropen`/`Rcreate`'s `iounit` are the same in both
        dialects - and `parse_stat` reads only up to `name`, so `.u`'s tail is stepped over rather
        than parsed. `Rerror` still carries the prose `ename` the client maps by phrase; `.u`'s
        `ecode` is deliberately unused (the phrase mapping is what the host tests pin, and QEMU sends
        usable prose).
        **The seam.** `drivers::virtio::P9Device` is the two methods the client and the mount table
        ask for (`mount_tag`, `request`); `Virtio9p<T: Transport>` is the one implementation, so the
        fid table, the `Twalk`/`Tread` encoding and the `Inode` mapping above it are one copy whichever
        bus answered. `fs9p::install(devices)` is the one door; `init_global()` (the PCI probe) stays
        the first machine's; `mmio::p9_device(virt, size)` is the second's; `vfs/p9.rs` holds
        `Arc<dyn P9Device>`; and the `mount -t 9p` dispatch arm lost its `#[cfg(not(target_arch =
        "aarch64"))]`. `mount_share()` moved out of `board/x86_64.rs` into the shared
        `board/steps.rs`, and that move is what made the second machine's share possible at all: it
        was the *probe* that was missing there, not the mount, and a mount written twice would have
        hidden which.
        **The pins, one per machine's convention.** The second machine's is `check_p9` (in `devices()`
        - find every `DeviceID` 9 window, open it, `fs9p::install` the lot) plus `check_p9_dev()`
        after `switch_root()` - `/mnt/share` resolves and `mpos-9p-probe`, a file xtask writes into
        the exported directory (`utils::lay_p9_share`), reads back through the VFS: the bytes travel
        host -> 9P2000.u -> `vfs::p9` -> the mount table, so a share that merely mounted fails there.
        The first machine's is `user/abitest`'s `t-9p`: the boot's own `/mnt/share` serves the probe,
        a program's own `mount -t 9p hostshare /mnt/t-9p` reaches the same bytes over the syscall
        path, and an unexported tag answers `ENOENT`. It runs on the first machine only - the aarch64
        sweep stages the `std`-side suite, not `abitest` - and it is the program that fails, loudly,
        if the probe call is ever dropped again.
        **The message size was raised to 128 KiB (2026-10-21).** QEMU warned on every boot that
        mounted the share that the client's `msize` was 8192 or less; the client did ask 8192, and
        every read/write chunks by the negotiated value. Measured on the same wiring under TCG, a
        64 MiB read took 0.255 s (250.6 MB/s) at 8192 and 0.065-0.071 s (897-986 MB/s) at 131072,
        with QEMU's trace showing the server granting the ask and the warning gone. QEMU 11.1.2's
        fsdev has no `msize` option at all, so the client's ask is the only knob. The ask is
        `vfs/p9.rs`'s `PROPOSED_MSIZE`, pinned by a host test that fails below the warning line;
        the measurement is `docs/notes/` §10.9.
        **And the parser joins `fuzz/`.** `parse_stat` walks a record whose own length word decides
        where it ends, over bytes a server chose, and the listing reader steps by the count it
        answers - so it is now driven directly by `fuzz/fuzz_targets/p9_stat.rs`, asserting that a
        placed record lies inside its buffer. It is `pub` for that; `Stat` is `pub` with private
        fields to keep the interface honest (AGENTS §3 rule 2 - the p9 wire half had no target
        before, which this closes).
        Measured after: `boot-aarch64` exit 0 with `virtio-9p: "hostshare" exported over MMIO`,
        `"hostshare" mounted at /mnt/share` and `M8_P9_OK: /mnt/share is the host share over MMIO,
        mpos-9p-probe read back (39 bytes)`, no `EXCEPTION`; aarch64 sweep `pass=27 fail=0` exit 0;
        `alpine-aarch64` exit 0 at `mpos login:` carrying the same three lines; x86 gate `pass=158
        fail=0 (158 built)` exit 33 with host 513 (was 512; `t-9p` is the 158th) and the test boot's
        log carrying `virtio-9p: "hostshare" mounted at /mnt/share` and `=== PASS t-9p`; depgraph 8/8
        within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none`,
        `aarch64-unknown-none`, the aarch64 loader and xtask; warning counts unchanged on both
        machines (aarch64 lib 47, x86_64 lib 1); the new fuzz target compiles
        (`cargo check --bin p9_stat --target x86_64-unknown-linux-gnu`). `TERMINATORS` 28 → 29.
  - [x] 13.30 a procfs without `/proc/<pid>/environ` made OpenRC mount over a live `/proc` - the
        fourth `[ !! ]` the Alpine boot hid, and the first one to hide behind a *passing* verdict.
        OpenRC's `init.sh` decides whether `/proc` still needs mounting by reading
        `/proc/self/environ` twice under two different values of a variable it sets on the command
        line (`VAR=a md5sum /proc/self/environ` against `VAR=b md5sum ...`) and treating two
        *different* digests as proof that a live procfs is already there; identical digests mean the
        path is stale cruft in the root filesystem, which is the case the test exists to catch. This
        kernel answered `ENOENT` at that name, so `[ -e /proc/self/environ ]` failed, the check never
        ran, and OpenRC concluded "not mounted" and mounted over the procfs the disk `/init` had
        already mounted: `mount: mounting proc on /proc failed: Resource busy`, the `/proc` service
        reporting `[ !! ]` - while `cargo xtask alpine-aarch64` still reported success (the command
        §15.7 has since retired; the statement is this run's, not a usable check today), because it
        keys on `login:` and the runlevel carries on past a failed service either way. The gap was in
        the *ABI*, not the boot: `Task` already carried `argv` for `/proc/<pid>/cmdline` and had no
        `env` to carry the environment beside it, so `execve`'s `envp` laid the block on the initial
        stack and was then dropped. `Task` and `TaskSnapshot` now hold `env` - set by `spawn_init`,
        `try_spawn_user` and `exec_replace`, inherited by `fork` and by `CLONE_THREAD` - and
        `/proc/<pid>/environ` and `/proc/<tid>/environ` read it back NUL-separated, present but empty
        for a task that was never exec'd, which is Linux's own shape. The verdict grew the pin that
        was missing: `openrc_failure` scans the log for openrc's marker (stripping the escapes that
        make it coloured) and names the service by its own line, so a failed service now fails the
        command rather than sitting in a log nobody greps. Measured after: the same boot prints
        `* /proc is already mounted`, carries **no** `[ !! ]` line anywhere, and reaches `mpos login:`
        in 13 s, exit 0; `boot-aarch64` unchanged (full chain, `INIT_EXITED`); aarch64 sweep
        `pass=27 fail=0`; x86 gate `pass=158 fail=0 (158 built)` exit 33; the new xtask unit test
        `an_openrc_service_failure_is_named_by_its_own_message` pins the checker against the captured
        failing block, escapes and all; fmt clean.
  - [x] 13.31 `ptrace` on the second machine, and the two defects that hid behind the gate - the
        third of Phase 15's named differences, and the one a user found by hand. `dispatch` carried
        `#[cfg(not(target_arch = "aarch64"))] nr::PTRACE` and `handle_ptrace` the same attribute,
        read as "the request numbers and `user_regs_struct` are x86_64's". Half true: the request
        *words* are one protocol (the same numbers in `abi`'s table on both machines), and what is
        really the machine's is **which slot of the register file a role lives in** - so `abi::uregs`
        now has two columns (27 words of `user_regs_struct`, 34 of `user_pt_regs`), the handler reads
        roles rather than names (`RESULT`/`IP`/`STACK` in place of `RAX`/`RIP`/`RSP`),
        `FPREGS_BYTES` is 512 with the `fxsave` image on one machine and 528 with the
        `fpsr`/`fpcr` pair swapped on the other, and `AUDIT_ARCH` is the machine's - `strace` refuses
        a `GET_SYSCALL_INFO` answer whose `arch` is not the one it was built for. The three
        remaining x86-only names (`UREGS_COUNT`, `uregs_by_name`, and the x86 column itself) are
        `cfg`-gated with the boot check that reads them.
        **Two defects came out of the new test, both older than the port.** (1) `PTRACE_PEEKTEXT`/
        `PEEKDATA` and `PEEKUSER` wrote the word *through* `data` as if it were a buffer, where Linux
        answers it as the call's result - so `ptrace(PEEKDATA, pid, addr, 0)`, which is the form every
        libc makes, hit `buffer_ok(0, 8, ...)` and answered `EFAULT`. (2) On aarch64 the syscall
        **entry** stop's register round-trip put the call's number into the ABI's result slot, and on
        this machine that slot is `x0` - the call's first argument, not a number register (the number
        is in `x8`). Because the gate re-reads the arguments out of the frame after the stop, the
        tracee made its call with the number as `arg0`. Measured: the first syscall after
        `PTRACE_TRACEME` - a traced `write(2, ...)` - wrote to descriptor 94, and the `_exit` that
        followed became `exit_group(94)`, so the tracee vanished with its own syscall number as its
        exit status. The fix is a `Gate` operation that asks the question the slot actually poses
        (`syscall_entry_result_slot`): the number on x86_64, where `syscall` left it in `rax`, and
        the frame's own `x0` on aarch64.
        **Pin.** `user/tests/src/bin/t-ptrace.rs`, on **both** machines - `TRACEME` and the stop it
        produces, `GET_SYSCALL_INFO` at a signal stop (`NONE`, the machine's `arch`, the tracee's own
        `ip`/`sp`, which is what `strace`'s startup probe reads and rejects the whole structure over),
        `SETOPTIONS` and its refusal of an unknown bit, `GETREGSET`/`NT_PRSTATUS` with the `iovec`'s
        length written back, `PEEKDATA`/`PEEKUSER` answering a word, `SYSCALL` to a boundary stop
        whose `ENTRY` names the call and whose `EXIT` reports its return value, `CONT` and the
        tracee's own exit. `strace` itself ships in the second machine's image
        (`rootfs-aarch64.toml`) and was read back on it: `strace -f -e trace=openat ls /` printed
        `openat(AT_FDCWD, "/", O_RDONLY|O_LARGEFILE|O_CLOEXEC|O_DIRECTORY) = 3` and `-c` printed its
        summary table. Also fixed: `t-clone`'s thread wrote its release word (`THREAD_WROTE`) before
        the two values the reader then reads - a race in the test, invisible until this cut's extra
        test shifted the timing, and already right in `t-futex`.
        Measured after: `boot-aarch64` full chain + `INIT_EXITED`; aarch64 sweep `pass=28 fail=0`;
        x86 gate `pass=159 fail=0 (159 built)` exit 33; `alpine-aarch64` exit 0 at `mpos login:`
        with no `[ !! ]` line; `depgraph` within the ratchet; fmt clean.
  - [ ] 13.32 **observed, not diagnosed**: the boot's EL0 check died once under a load it does not
        control. 2026-10-25, one lane of a four-lane aarch64 sweep arm (the three sibling lanes and
        two later arms - one at the same tree, one at its predecessor - all green): after the
        check's own `EL0: root … active, program at 0x200000 (40 bytes)` line, a synchronous data
        abort, `ESR_EL1 0x96000047` (EC 0x25, from EL0), `FAR_EL1 0xffffffff802cd000` - the first
        page past `kernel_end`, and `ELR_EL1` inside `pl011`'s print loop, whose instruction at
        that address (`tbz`, on a register) cannot be the aborting access. So the dump is not
        trustworthy at face value: the fault's own report was written by a path whose state is
        already suspect, and the reading to trust is only the *shape* - a kernel-half address one
        page past the image, reached from the probe program's exception level. Nothing in the
        block-path landing of the same day runs before the scheduler, and the crash did not
        reproduce across two later full arms; the discipline is §6.22's - an observation with its
        readings, a re-run before believing it, and a second reading as where any diagnosis starts.
  - [x] 13.19 `reboot(2)` answers Linux's shape on **both** machines - two mechanisms behind one
        symptom, and the symptom was on every aarch64 boot.
        **What was seen.** `mpos: syscall 142 -> ENOSYS`, once, between the kernel's handoff and the
        login prompt of the second machine's Alpine boot. `142` is `__NR_reboot` on arm64 (it is
        `sched_setparam` on x86_64), and the caller is busybox `init`, which asks for
        `LINUX_REBOOT_CMD_CAD_OFF` about a second into its startup - the call that takes
        Ctrl-Alt-Del for the `::ctrlaltdel:` line of `/etc/inittab`. The kernel's own comment beside
        the handler had already measured that caller ("the first thing that ran after the handover -
        busybox init - asked for all of them within a second of starting"), and on x86_64 it is
        served.
        **Mechanism 1: a stale gate.** The dispatch arm and the handler were both
        `#[cfg(not(target_arch = "aarch64"))]`, written when a *stop* word ended at the x86_64
        `isa-debug-exit` port. That port is a `hal` capability now (`hal::exit_qemu`, which a machine
        without `mpos_qemu_debug_exit` answers by halting), and §14.3's S2 record says in as many
        words that this is what the second machine's `reboot` path should get "rather than a second
        body of code" - so the gate was a marker left behind by the capability, and nothing in the
        handler names a register, a port or a machine. Both gates deleted.
        **Mechanism 2: the words are 32 bits wide.** The pin written for mechanism 1 then failed on
        *both* machines, and not vacuously: `reboot(LINUX_REBOOT_CMD_CAD_ON)` was `EINVAL` while
        `CAD_OFF` (zero) was served. Linux's entry point is
        `reboot(int magic1, int magic2, unsigned int cmd, void *arg)`, so the kernel narrows each
        word to its low 32 bits - and it is the *libc's* prototype that decides what lands in the
        64-bit register: musl declares `reboot(int type)`, so every word at or above `2^31` arrives
        **sign-extended** (`0x89abcdef` reaches the kernel as `0xffffffff89abcdef`), which the
        full-word comparison read as a command no kernel knows. Three of the five words are up there -
        `CAD_ON`, `ENABLE_CAD` and `HALT_SYSTEM` - so busybox `init`'s shutdown half and `halt -f`
        were among the callers being refused, on x86_64 too, since the handler was written.
        `abi::reboot_command` now reads the three words as `u32`.
        **Pins.** `user/tests/src/bin/t-reboot-libc.rs` on both machines: it drives `libc::reboot`
        (the entry point a C program reaches, with musl supplying the magics), asserts both policy
        words are served, and asserts an unknown command is `EINVAL` rather than `ENOSYS` - the
        difference between "no such word" and "no such call". It never touches a stop word, and it
        skips as a non-root caller (see the named divergence below). Host-side,
        `abi::reboot_reads_its_words_as_linux_narrows_them` pins the narrowing itself, with the
        extension asserted to be *real* for the two words that have it, so the test cannot pass
        against a kernel that narrows nothing.
        **Named, not fixed:** the handler does not ask who is calling. Linux requires `CAP_SYS_BOOT`
        and answers `EPERM` to an unprivileged caller; this kernel answers `0` to anyone, on both
        machines. That is a capability check this kernel does not model, it is older than this entry,
        and the pin says so in its own header rather than asserting the divergence.
        Measured after: aarch64 sweep `pass=27 fail=0` exit 0 (`t-reboot-libc` staged and passing, was
        26); x86 gate `pass=157 fail=0 (157 built)` exit 33; host 512; depgraph 8/8 mutual pairs
        within the ratchet; fmt clean; clippy 0 errors on `x86_64-unknown-none` and
        `aarch64-unknown-none`.
  - [x] 13.20 the root slot knows its filesystem - the invariant a whole class of answers rests on,
        and the bug that broke the second machine's package manager.
        **What was seen.** On the aarch64 Alpine tree, every `apk update` answered
        `WARNING: updating and opening https://…/APKINDEX.tar.gz: Cross-device link` for both
        repositories and then `2 unavailable, 0 stale; 27 distinct packages available`: the index was
        fetched, could not be given its name, and was therefore re-fetched on every run. `apk`'s
        index write (`apk-tools`' `apk_ostream_to_file_safe`) is four steps -
        `openat(cache_fd, ".", O_RDWR|O_TMPFILE|O_CLOEXEC, 0644)`, write,
        `linkat(AT_FDCWD, "/proc/self/fd/N", cache_fd, "…tmp.PID", AT_SYMLINK_FOLLOW)`, then
        `renameat` into place - and the `EXDEV` is the kernel's answer to step 3, about an inode born
        in the very directory it was being linked into.
        **How it was localised, in three runs.** (1) `user/tests/src/bin/t-tmpfile-link.rs` drives
        those four steps as a program; its first aarch64 run was red with **errno 2** - not the bug,
        but the *harness*: the aarch64 sweep's runner mounts only `/tmp`, so `/proc/self/fd/N` did not
        resolve (the x86_64 harness's `/init` mounts `/proc` with busybox, and a real boot has one
        already). The test now mounts a procfs of its own when the payload has none, and says which
        it used. (2) With `/proc` present the run was red with **errno 18**, reproducing the report
        exactly. (3) One temporary `print!` in `handle_linkat` gave the whole answer in one line:
        `via_desc=true served=(false,false)` - the descriptor path *was* taken, and **both**
        `fs_serving_inode` calls returned `None`, which the identity test reads as "different
        filesystems". `fs_serving_inode` answers a mount's own root by that mount, and *anything
        else* by `root_fs` - so the root slot was empty.
        **Two omissions, one shape.** `Vfs::mount_root` recorded the root's type, device and mount id
        but never the filesystem itself; and the aarch64 boot attaches its disk through `Vfs::mount`,
        which recorded `fs: None` - a *name* attachment with no instance behind it - which
        `pivot_root` then promoted into the root slot. The first machine never hit it because its
        root is attached by **userland**: the initramfs's `mount -t ext4` goes through the `mount(2)`
        handler, which records the instance, and busybox `switch_root` promotes that with `MS_MOVE`.
        So the two machines' roots differed in exactly the way the report's instinct said they
        might - the second one assembles its root in the kernel, along a path the first machine's
        boot never takes.
        **Fix.** `Mount::fs` is an `Arc<dyn FileSystem>` rather than an `Option`: a mount record
        always names the filesystem it attaches, and the type is the invariant. `Vfs::mount_root` and
        `Vfs::mount` take the instance (every caller updated, including the host tests), `pivot_root`'s
        parked old-root entry carries the instance the replaced tree was served by, `bind_mount`
        borrows the serving instance or refuses (`NoRoot`), and the aarch64 board attaches its ext4 as
        `Arc<dyn FileSystem>` - the same shape the `mount(2)` handler builds.
        **The same root cause, two more symptoms on that machine, fixed with it.** `statfs("/")`'s
        block numbers and `mount -o remount,rw /` both read the root slot (the id-0 arm of
        `fs_serving_path`, and `Vfs::remount`), so both answered "no filesystem" there - the first as
        the syscall layer's fabricate-a-filesystem fallback, the second as a silent no-op. OpenRC's
        `root` service asks for the second on every boot.
        **Pin.** `t-tmpfile-link` runs on both machines: on aarch64 its root is the ext4 disk, so it
        drives all four steps for real and was red with `EXDEV` before the fix; on the x86_64 gate,
        whose root is the initramfs's ramfs, `O_TMPFILE` answers `EOPNOTSUPP` and the test drives
        `apk`'s own named-temporary fallback instead - and *says so*, because the ext4 half is the pin
        and the ramfs half is only the honest path that machine can run. That asymmetry is named here
        rather than hidden: the x86_64 gate has no ext4 root to test the `O_TMPFILE` half on.
        Measured after: aarch64 sweep `pass=27 fail=0` exit 0 (was 25, with this test red); x86 gate
        `pass=157 fail=0 (157 built)` exit 33; host 512; depgraph within the ratchet
        (`vfs/mod.rs -> drivers|net` 0/0, 8/8 mutual pairs); fmt clean; clippy 0 errors on both
        bare-metal targets.

**ABI truthfulness (Phase 12 — Group A landing, gate now runs to a summary)**

**The sweep's *set* is now a configuration, and its default is smaller on purpose.** Every test

  reds that `AGENTS.md` had to call "load flakes"; they, and `t-udp`/`t-tcp-close`, are now run
  by name. A default red is therefore the kernel's, which is the only property that makes the
  gate worth reading.
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

### R.Remaining (moved from ROADMAP's "Remaining ABI support") — what the dispatch still refuses, grouped by what it blocks

Syscalls the dispatch falls through to `ENOSYS` (each prints once via `note_enosys`), plus the foundational behaviours whose semantics are incomplete. These are the whole of what is left to reach "full Linux ABI".

**Machines that are not there, and the answers that are honest because of it.** No USB host controller, and no driver for one: `/sys/bus/usb/devices` is therefore an empty directory — which is what Linux registers on a machine with no USB — and `lsusb` prints nothing and exits 0; a tree with anything in it would be a device this kernel cannot enumerate (`t-mountinfo` pins the empty directory). No loadable modules: every driver is compiled into the image, so `/proc/modules` is an empty file — what `lsmod` reads as a header and no rows, exactly as it does on a Linux built with no modules; a file that said "cannot tell you" would be the worse answer, since `lsmod` has no vocabulary for it. The inode flags this kernel cannot keep are refused with `EOPNOTSUPP` rather than stored and ignored: compression, encryption, fs-verity, casefold, project inherit, inline data, DAX, no-COW, journal-data, tail-merge, top-of-tree, secure-delete/undelete, AFS magic naming, extended-attribute inodes and the sparse-tail bit — `INODE_FL_VISIBLE`/`INODE_FL_MODIFIABLE` in `src/vfs/mod.rs` is the list, each bit named so a caller is refused for the right reason, and the three the ext4 driver owns (the extent tree, the directory index, the block-counting unit) are readable and not writable (§12.4b).

**File-watch / service infrastructure (the big one for daemons).** `inotify_init1` (294)/`inotify_add_watch` (254)/`inotify_rm_watch` (255) are answered: a full watch queue in `src/vfs/inotify.rs` (event records, the `(dev, ino)` registry, the `IN_ISDIR`/oneshot/`IN_EXCL_UNLINK`/`IN_MASK_ADD`/`IN_MASK_CREATE` rules, the rename cookie pair, DELETE_SELF and IGNORED, one coalesced Q_OVERFLOW per drop episode), emitting from the create/delete/rename/link/open/close/write/attrib hooks of the syscall layer — `mdev`/`udev`, `dbus` and config re-readers can watch and hear. Deliberate v1 divergences: `IN_ACCESS` is never emitted (the read path does not bump atime), `IN_UNMOUNT` is not emitted (unmount does not track watches), a directory's `IN_MOVE_SELF` waits on directory rename, and a rename that replaces a name reports the `IN_DELETE` this kernel's copy actually performs. `fanotify_init` (300)/`fanotify_mark` (301) — less common (antivirus, some mount monitors); an earlier draft of this row read them as `302`/`303`, which is corrected: the header's numbers and the ones `t-enosys` walks are `300`/`301` (`docs/uapi/` §1.1). `keyctl` (250) — kernel keyring; needed by `keyutils`/some TLS/pam stacks.

**POSIX timers (222–226): implemented** (`t-timer-create`). The number used to be blamed for a hang was `vim`'s: its `timers` feature asks for 222 while drawing, and the failure after the refusal is what §2.5's follow-up is about — the redraw path then dereferenced a null timeout flag and died. What the family answers, measured on a 6.x Linux at the raw-syscall level so nobody has to re-derive it: `timer_create(CLOCK_REALTIME, NULL, &id)` succeeds and the expiry queues **`SIGALRM`** — a null `evp` is `SIGEV_SIGNAL` with `sigev_signo = SIGALRM` (measured by blocking `SIGALRM` and reading `rt_sigpending`), *not* the `SIGEV_NONE` an earlier revision of this note claimed; the id handed back is a small per-process integer, not a pointer, and a bogus id answers `EINVAL` on all five calls — *not* the `EBADF` the man page claims. The accepted clocks, measured: `REALTIME`(0), `MONOTONIC`(1), `PROCESS_CPUTIME_ID`(2), `THREAD_CPUTIME_ID`(3), `BOOTTIME`(7) and `TAI`(11) are accepted; `MONOTONIC_RAW`(4) and the two `*_COARSE` clocks answer `EOPNOTSUPP`; the two `*_ALARM` clocks answer `EPERM` (they need `CAP_WAKE_ALARM`); a number no clock has is `EINVAL`. This kernel accepts `REALTIME`/`MONOTONIC`/`BOOTTIME`/`TAI` — one tick clock under four names — and refuses the CPU-time clocks with `EOPNOTSUPP`, because it keeps no per-task CPU accounting: a timer accepted and never fired would be worse than one that says so. `sigev_notify` outside `{NONE, SIGNAL, THREAD_ID}` is `EINVAL` (including `SIGEV_THREAD`(2), a libc value that never reaches the kernel); a signal number runs `1..=64` on Linux, this kernel stops at `SIGRTMIN` (32) and refuses the range above it rather than accepting a number with no disposition storage behind it. `TIMER_ABSTIME` is `EINVAL` here, the same refusal `clock_nanosleep` makes: one clock, no absolute domain (§2 records that choice). `SIGEV_THREAD_ID` aims the expiry at one thread of the caller's group (a tid outside it is `EINVAL`, as on Linux); `timer_getoverrun` on a fresh timer answers 0. **The shape musl actually builds works end to end**: `timer_create(..., SIGEV_THREAD)` is a helper thread blocked in a wait plus a `SIGEV_THREAD_ID` timer carrying `SIGRTMIN`; this kernel queues the expiry with `SI_TIMER` siginfo and the caller's `sigval`, and `rt_sigtimedwait` hands the record back with `si_code` intact — the one field musl checks before calling the callback. That needed three things beyond the timer table: `SIGRTMIN` as a queueable number, a 64-bit pending mask, and the signal **mask inherited across `clone`/`fork`** — musl blocks `SIGRTMIN` before creating the helper and the helper is what waits on it. Divergences: a timer belongs to the *task* that created it, not to its thread group; ids are the lowest free slot (Linux never reuses an id); the expiry runs on the scheduler's tick, so a deadline is tick-quantised and a 50 ms arm reads back as 52 ms — rounded up, never early. The prerequisite named here was right: "a per-task list of armed clocks the scheduler's tick scan can run down" is `sched::Table::check_posix_timers`, behind its own armed flag, so a machine that never calls `timer_create` never scans for one.

**Other missing (lower priority).** `userfaultfd` (323) — lazy-copy / CRIU-style migration; niche. `tid_add_object`/`tgid_add_object` (rseq side-objects) — rare. Extended attributes (`getxattr`/`setxattr`/`listxattr`/`removexattr` + `l`/`f` forms, 188–199) are **answered**: arguments and target are validated (so a bad path is still `ENOENT`/`EFAULT`, a bad fd `EBADF`), then `EOPNOTSUPP`, because no filesystem here stores xattrs; wiring them stops the `ENOSYS` fall-through that `fastfetch`/probing callers reacted to, and a real xattr store remains future.

**Foundational behaviours (implemented syscalls, incomplete semantics).** 4.3: the family has a listener and a loopback now — `listen`/`accept` on IPv4 are served by a pool of listening sockets (`t-tcp-listen`), `lo` is a real interface over smoltcp's loopback device (`t-udp-loopback`), and the stack keeps one socket set per wire; `TCP_NODELAY` and `SO_KEEPALIVE` reach the live smoltcp socket (Nagle off; probe timer armed at Linux's default interval) and `getsockopt` reads back every stored option; `SO_REUSEADDR` is applied at `bind`/`listen` (the holder's option counts as much as the caller's — §3.3.3), `SO_LINGER{on, 0}` makes the last `close` a reset (the peer reads `ECONNRESET`/`EPIPE` — §3.3.4), while buffer sizes are Linux's numbers now and a datagram socket's `SO_SNDBUF` bounds its send queue (`docs/uapi/` §5.1 item 58, 2026-10-18; a stream's `SO_RCVBUF` is its receive window since item 98, inherited by the accepted socket, while a datagram's ring stays fixed, and the datagram-too-big line is the ring's until this stack fragments IP) — though the *blocking* send that waits on a full buffer is served now (`docs/uapi/` §5.1 item 27, 2026-10-16: a blocking UDP sender parks until the egress drains, the wait's wakeup source being the stack's own poll rather than a TX completion interrupt, measured whole on a current host first and pinned by `t-udpblock`; the park is bounded at three seconds because smoltcp keeps a datagram its egress could not dispatch where Linux's device drops the frame, and Linux's `EMSGSIZE` line is the maximum UDP datagram rather than the buffer, which is the buffer-size item's own half). 4.4: both halves of netlink are written — the route half reconfigures the live stack (`RTM_NEWADDR`/`NEWROUTE`/`NEWLINK`, §4.4), and the uevent half carries userland messages (port ids, unicast, groups and credentials), which is what udevd's handoff to a worker needs (§4.4's closing note). 5.1/5.2 closed: a mapping is a VMA (`mm::vma`) and the page behind an address arrives on the fault that needs it, which is what let `MAP_SHARED|MAP_ANONYMOUS` become an unnamed `Memfd` whose aliased frames cross a plain `fork`, and `MADV_DONTNEED` release pages that the next touch refills (`t-mmap-demand`, `t-mmap-shared-fork`); what stays a no-op says why in §5.2 — `MADV_FREE` has no pressure model to honour, and a shared mapping's pages are dropped for the caller's mapping rather than for the object, which needs a reverse map of mappings this kernel does not keep. 6.3 namespaces: UTS, MNT, PID, NET, TIME and USER are all served now, each with its own `t-ns-*` and each with its named residuals in §6.3; the two kinds with no entry are IPC and cgroup (`EOPNOTSUPP`). Terminal size, and the "only the first few dozen lines" report: a full-screen program lays itself out in the window `TIOCGWINSZ` names, so the answer is the display, not a default. Measured on Linux: a *fresh* pty pair nobody has sized answers **0 rows and 0 columns** (success, zeroed struct), `TIOCSWINSZ` on either end is visible on both because the window belongs to the pair, a resize of a pty whose slave is blocked in `read` runs its `SIGWINCH` through to that read as `EINTR`, and `TIOCGWINSZ` on a regular file is `ENOTTY`. This kernel answered 25x80 for an unsized pair when this entry was written — closer to what a program can survive, and a deliberate divergence — but answers the measured 0x0 now (`3369931`, 2026-10-03; the pin is `t-pty`), so the pty half of the divergence is closed. A virtual console answers the screen it paints on (§7.1's resize rule), whose grid is clamped at `MAX_ROWS = 64` while a 1024x768 console is 128x48. The clamp is what a "the file is longer than the screen" complaint lands on: it has to be re-measured on the current tree, because the seat/resize work in flight changed exactly this path, and `vi` on a large file was reported as showing a few dozen lines *and* freezing — the freezing half is now §2.5, and the layout half needs a boot to confirm. 4.1 AF_UNIX is answered for `SOCK_STREAM` (see Phase 4), including `SCM_RIGHTS` descriptor handover and `SCM_CREDENTIALS`/`SO_PEERCRED` peer authentication — a real dbus broker can now both connect its clients and name them — for `SOCK_DGRAM`, message-for-message, and for a *named* `SOCK_SEQPACKET` connection, whose accepted pair keeps its boundaries and descriptors (`docs/uapi/` §5.1 item 78); item 79 served the connected send's destination rule and item 80 the path-name type mismatch (`EPROTOTYPE`, with an abstract name of another type still `ECONNREFUSED`). Item 81 serves the last named hole: an unconnected or listening seqpacket socket's send answers `ENOTCONN`, whatever the name says, with the syscall layer's `sendto`/`sendmsg` validation split pinned. **`mknod`'s type field** is complete: the `S_IFIFO` arm makes a real **FIFO** (`docs/uapi/` §5.1 item 26, served 2026-10-16 — a *channel* whose `open` runs Linux's rendezvous) and a device node carries the caller's number in `st_rdev` through Linux's own encode/decode (`docs/uapi/` §5.1 item 71), opening it answering `ENXIO` because no driver stands behind any number this kernel can be given.
