# §7 Phase 7: Device Drivers — **7.1, 7.2 and 7.3 are done; §7.4 has two steps left**

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 7: Device Drivers — **7.1, 7.2 and 7.3 are done; §7.4 has two steps left**

The device work is finished: display, input, and now storage. §7.4 is a different
kind of thing - it is *analysis* of the compositors beyond weston, written from
their sources and the UAPI rather than measured here, and two of its six ordered
steps remain (§7.4's own list). One item under 7.1 is also still open and is
written down there rather than here: weston's desktop-shell does not paint its
pointer. The shell itself starts now that the epoll report bug is fixed (§1.1);
what remains is its cursor image, and the measurements say that is not this
kernel's doing.

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
accepts a mode change - and the *values* of those two flags are not defined by
any header on the development machine either, which is why the enforcement has
never been written: they are the one thing in `drivers/drm.rs` accepted on a
shape argument rather than a measurement. That is worth naming next to the
`NR_MODE_CREATE_LEASE` mistake three paragraphs above, because the two failures
look alike and do not behave alike: a wrong **request number** reaches the
dispatch catch-all and answers `ENOTTY`, loudly, while a wrong **flag** sits
inside the accepted mask, stores something, and hands the caller back what it
asked for; and `MODE_CREATE_LEASE` is the master's, so the accepting host's card
cannot supply the values either. If a header with them turns up, this is the
place to start; and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` and
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
(0x0 until an emulator says otherwise — Linux's own answer for an unsized pair,
`t-pty`) and reports it, while the console
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
the one place §3.3 cares about most - a name.

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

**What the mask leaves behind is the machine's.** Measured 2026-10-12 by
`src/drivers/kunit.rs`'s `block.what_a_read_costs_the_clock` (written for §10.5's
last row), and it sharpens the sentence above rather than correcting it: no handler
can run *inside* a mask on either machine, so a deadline in ticks still cannot expire
there — but what the count says *afterwards* differs. `arch/x86_64/timer.rs`'s `tick`
is `TICKS.fetch_add(1)`, an interrupt count, so the periods a mask spans are lost from
it for good; `arch/aarch64/timer.rs`'s adds the periods its own timer counter says
have passed, so it **catches up**. Measured across a 41 ms mask (one 64 MiB read):
**`11/10` ticks** on the second machine, where a pure interrupt count could have
delivered at most one. A `busy_wait_ms` deadline in ticks is therefore still the wrong
shape on both - and the *machine* to ask about what a stalled read cost the clock is
the first one.

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
  `BlockDev`'s, and §3.3 says whose limit it therefore is: the driver's.
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
> **Update, measured (2026-10-09, three `libdrm` probes against the host's own
> card): the rule above is right, and narrower than the sentence sounds.** Run on
> two open file descriptions of `/dev/dri/card1` so the cap's *scope* is measured
> rather than assumed:
>
> - **The list is filtered, per open file description.** A CRTC lists **6**
>   properties on a client that has not set the cap and **9** on one that has -
>   `ACTIVE`, `MODE_ID` and `OUT_FENCE_PTR` appear only for the capped client, each
>   carrying the `ATOMIC` flag - and the uncapped descriptor still lists 6 after the
>   capped one set the cap, so the state is the *file's*.
> - **Asking by id is not filtered.** `drmModeGetProperty` answers for an atomic
>   property's id on the uncapped client and reports the flag: what Linux gates is
>   the *enumeration*, not the door. That is the half a reader gets wrong from the
>   sentence above, and it is why the fix belongs in the list builder rather than in
>   `GETPROPERTY`.
> - **Which properties carry it**: on a plane `FB_ID` and `CRTC_ID` are
>   `ATOMIC | OBJECT`, and so are `SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H` and
>   `CRTC_X`/`CRTC_Y`/`CRTC_W`/`CRTC_H` (with `IN_FENCE_FD`, `FB_DAMAGE_CLIPS`),
>   while `type`, `IN_FORMATS`, `IN_FORMATS_ASYNC`, `zpos`, `rotation`,
>   `COLOR_ENCODING` and `COLOR_RANGE` are **not**; on a connector only `CRTC_ID`
>   is, and `EDID`, `DPMS`, `non-desktop`, `max bpc` and `vrr_capable` are not. So
>   this card's object-typed properties, the plane's position/size family and the
>   CRTC's `ACTIVE`/`MODE_ID` are exactly the set to mark.
> - **One property is unmeasured**: this kernel's `pixel blend mode`. The host's
>   plane lists no blend property at all, so it stays unmarked and says so rather
>   than being marked because the shape looks like its neighbours.
>
> The change that follows is five pieces: `PROP_ATOMIC` (`1 << 31`, `drm_mode.h`'s
> number); a per-open-file-description cap keyed on the same `ofd` the framebuffers
> already carry as `owner` (`set_client_cap` takes it; the release path prunes it
> beside `release_framebuffers`); the flag on the properties above; a filter in
> `obj_get_properties` (`OBJ_GETPROPERTIES`) that skips them for a client without the
> cap; and a pin in `t-drm-props` - the plane's `FB_ID` absent from the list before
> the cap and present with the flag after it, on the same descriptor, with a second
> descriptor as the control that the state is the file's.
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

> **Update, landed (2026-10-09): the five pieces are in, and the probe that wrote
> them settled two things the sentence above did not say.** A second instrument on
> the host's card - every property of every connector, CRTC and plane with its
> flags, on a descriptor that had set the cap, beside one that had not - answers
> both questions the five-piece list left open:
>
> - **The connector door filters too.** `drmModeGetConnector` reported **17**
>   properties to the uncapped descriptor and **18** to the capped one, on the same
>   card, with `CRTC_ID` the difference - the CRTC's 6-and-9 above is the same rule
>   seen through `OBJ_GETPROPERTIES`. So the skip belongs in *both* list builders,
>   which is also what this file's own comment demands of `CONNECTOR_PROPS`: a
>   client that read a connector from one door and not the other would otherwise be
>   handed two different connectors.
> - **`IN_FENCE_FD` and `OUT_FENCE_PTR` are atomic properties**, which the sentence
>   above left ambiguous by naming them only in a parenthetical. Measured: the
>   plane's `IN_FENCE_FD` reports `0x80000080` (`ATOMIC | SIGNED_RANGE`) and the
>   CRTC's `OUT_FENCE_PTR` `0x80000002` (`ATOMIC | RANGE`). Both are marked.
>
> What landed (`drivers/drm.rs`): `PROP_ATOMIC` (`1 << 31`) beside one `ATOMIC_PROPS`
> list that *both* the reported flag and the two filters read, so the shape a caller
> sees and the set the cap hides cannot drift apart; `DrmState::atomic_ofds`, keyed
> on the same `ofd` the framebuffers carry as `owner`, written by `set_client_cap`
> (a value of 0 *un*sets it, which is the request weston makes on teardown) and
> pruned in `master_closed` beside `release_framebuffers`; the skip applied through
> one `without_atomic` that compacts the ids and their parallel values in place, in
> `get_connector` and in `obj_get_properties` alike; and `get_property` left
> deliberately *un*filtered, so a caller that asks for an atomic property's id on an
> uncapped descriptor is still answered, and still told the flag. Pinned by
> `t-drm-props`, which reads both doors on one descriptor with a second descriptor
> as the control and asserts the change is *exactly* the atomic set - nothing the
> uncapped list had is missing from the capped one, and everything it gained is
> flagged. `t-drm` and `t-drm-sync` now set the cap before they read an atomic
> property by name or by index, which is what a compositor does: weston 16 has no
> legacy path left and could not read its own frame properties without it.
>
> **Two things this deliberately did not do.** `MODE_ATOMIC` itself is not gated on
> the cap here, and whether Linux gates it is **unmeasured** - the probe that would
> have asked got `EACCES` from *both* descriptors, because a compositor holds the
> master on the development host and the master check answers first; settling it
> needs a card with nothing on it, and taking the master off a live desktop is not a
> thing to do to one. And `pixel blend mode` stays unmarked, for the reason the list
> above gives: the host's planes carry no blend property at all, so there is no
> measurement to follow rather than a shape to copy. One older difference is named
> here rather than changed: Linux spells `CRTC_X`/`CRTC_Y` `SIGNED_RANGE` (`0x80`)
> where this card spells them `RANGE`, which is about the property's *shape* and not
> its visibility, and is older than this change.

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
>   a caller identifies the driver by is the shape §3.3 forbids. `t-drm` carries
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

> **Update, measured against a real machine (2026-10-09): the `uevent` surface now
> says what Linux says, and the DRM nodes carry the word their rules match on.**
> The block above named the next measurement - the guest's udev database entry,
> set beside Linux's - because "nothing in the device model changes until that dump
> is in hand". It is still not in hand: reading it needs a boot of the *desktop*
> rootfs, and this build directory has never held one (no Alpine root, no disk
> image), so the run that would produce it is a full rootfs install plus a GL boot
> rather than a measurement. What *was* in hand is the other half of that
> comparison, and it does not need a guest at all: the accepting host's own card.
>
> Dumped side by side, `/sys/class/drm/*/uevent` and `/run/udev/data/c226:*` on a
> working card say three things this tree did not:
>
> - **Every DRM *node* carries `DEVTYPE=drm_minor`** - `card1` and `renderD128`
>   alike - and a connector directory carries `DEVTYPE=drm_connector`; the class's
>   `card1-DP-1` and the like have no `MAJOR`/`MINOR`/`DEVNAME` at all, because
>   they are not device nodes. This is the word the record already flagged as
>   absent from our node's `uevent`, and it is the one udev's own rules key on:
>   the host's database entry has `ID_PATH=pci-0000:65:00.0`, `ID_PATH_TAG`,
>   `ID_FOR_SEAT=drm-pci-0000_65_00_0` and the tags `seat`, `master-of-seat` and
>   `uaccess` on the **primary card**, `ID_PATH`/`ID_PATH_TAG` and no groups at
>   all on the render node, and a `by-path` symlink beside the node. Only the
>   primary gets a seat, which is worth knowing before wiring anything to the
>   render node.
> - **The `uevent` *file* is not the netlink message.** `ACTION`, `DEVPATH` and
>   `SUBSYSTEM` are the message's prologue and appear in no `uevent` file on the
>   machine: the card's is four lines (`MAJOR`, `MINOR`, `DEVNAME`,
>   `DEVTYPE`), a PCI function's is its six `DRIVER`/`PCI_*` keys, an input node's
>   is `MAJOR`/`MINOR`/`DEVNAME` with no `DEVTYPE` at all. Ours printed exactly the
>   three words Linux keeps for the wire, and dropped `Event::extra` - so
>   `DEVTYPE` could not have appeared in it however the node were built.
> - **A device with no node announces no node number.** Our message carried
>   `MAJOR=0 MINOR=0` for a PCI function, which is not a missing answer but an
>   answer: it is the `DEVNAME` mistake, in the other two keys.
>
> Built: the file is now the device number plus the subsystem's own keys (the
> extras the message already had), the DRM nodes carry `DEVTYPE=drm_minor`, and
> the block nodes carry `DEVTYPE=disk`/`partition` - which this builder already
> knew as `entry.whole`, and which the host's block nodes all have, beside a
> `DISKSEQ` this card has no source for and so does not invent. Pinned by three
> host tests, one per bullet, the first asserting the card's file *key for key*
> rather than by substring.
>
> **What this does not claim.** Whether it is enough for Mesa is *not* measured:
> the gate boots an initramfs, so what it proves is that nothing moved, and the
> `eglinfo` run is still the bar this section has been unable to clear. What is
> now true is narrower and worth having: the inputs the working card's udev rules
> match on are present, where before the card was a DRM node that no seat rule
> could match at all.

> **Update, measured in the guest (2026-10-09, the same day): the first bar is
> still not cleared, and the run that would not clear it also says why.** The
> GL-card acceptance this section has been unable to take has now been taken -
> desktop rootfs, `virtio-gpu-gl-pci`, `-display egl-headless,gl=on` (which QEMU
> accepts on this host: measured, by it running), and a boot-time hook streaming
> its findings out `ttyS0`. What it says:
>
> - **The renderer is still `llvmpipe`, and Mesa names its own fallback**:
>   `EGL driver name: kms_swrast`, with `MESA-EGL: warning: egl: failed to create
>   dri2 screen` and `DRI2: failed to create screen` above it on both the
>   surfaceless and the GBM platform. So `is_software()` is true, niri still
>   cannot run here, and the section's first bar is *unmet* rather than
>   unverified - which is the difference this run was for.
> - **The chain the earlier sessions built is right, seen from inside**: both
>   nodes carry `DEVTYPE=drm_minor` in their `uevent` files (the change above,
>   confirmed in the guest rather than by a host test), the card's `device` link
>   reaches `/sys/devices/pci0000:00/0000:00:04.0`, and that directory carries
>   `vendor`, `device`, `subsystem_vendor`, `subsystem_device`, `subsystem`,
>   `uevent` and a `MODALIAS` reading `pci:v00001AF4d00001050...` - the PCI
>   identity a loader asks for, answered.
> - **And two things are still not right, which is the part worth having.** First,
>   **udev never started**: the boot runlevel brings up syslog, crond, dbus and
>   seatd, and no udev, and udev's database has *no entry at all* for `c226:0` or
>   `c226:128` - `cat /run/udev/data/...` is silent. That is the layer libdrm's
>   device information comes from, and on this boot it does not exist; the earlier
>   note that "a *stock* boot reports no udev at all" now has the dump beside it.
>   Second, **the node says it is virtual**: `udevadm info -q all` reports
>   `DEVPATH=/devices/virtual/drm/card0`, where a working card's devpath runs
>   through its PCI function. The `device` *link* is right and the node's own
>   *position* is not - and udev's path rules walk the devpath rather than the
>   link, so no `ID_PATH`, `ID_FOR_SEAT` or seat tag can be produced however
>   correct the link is. That is the "device model rather than the command queue"
>   direction this section predicted, now with the one field to change named.
>
> **The next step is therefore specific**, and still two parts: give the DRM node
> its home *under* the device it belongs to, so `DEVPATH` is the bus path, and
> make this image actually start udev. Then re-run this probe - whose technique is
> worth keeping, because it cost four boots to find: `cargo xtask all` builds the
> desktop image (`build/mpos-uefi-x86_64.img`, which name is why the first search
> for it came up empty), a temporary `/etc/local.d/zz-probe.start` streaming to
> `tee /root/probe.txt /dev/ttyS0` is the instrument, `qemu.gpu` and
> `qemu.display` are the two keys that make the card GL and headless, and
> `cargo xtask run --mode mirror` is the mode - because a `gui` boot makes `tty0`
> the *only* console and leaves the wire quiet, so the first probe was painted
> where no host could read it.
>
> **And one defect found on the way, named rather than fixed**: the first version
> of that hook wrote through the 9p share, and the guest wedged with QEMU
> answering `qemu-system-x86_64: Failed to decode VirtFS request type 114`. The
> request is `Tcreate` in this client's own `9P2000.u` numbering, so the dialect is
> agreed and the *body* is what the server cannot parse - a guest writing through
> this kernel's 9p client does not survive, and a boot script that ends with such a
> write never returns.
>
> **Fixed, and the fix is one field.** `Tcreate` in this dialect ends with
> `extension[s]`, which the *server* unmarshals - so a body that stops at `mode` is
> not a message the server decodes and refuses, it is one it cannot decode **at
> all**, and an undecodable request is answered with silence. `WSTAT`, a few lines
> above it in the same file, had sent the same empty string all along; the create
> path was the one that did not, which is why nothing else on it failed.
>
> The counterfactual is measured rather than argued, and it is the part worth
> keeping: with the field added, the host's complaint moves from `114` to **`126`**.
> `Tcreate` is decoded now, and `Twstat` is the *next* request that test reached -
> so the same path carries a second undecodable body, in a message whose `.u`
> layout reads correctly line by line. That one is named as the next item rather
> than guessed at, and the guest test that found both is deliberately **not**
> landed with the fix: a caller waiting on an answer that never comes *hangs*, and a
> gate that times out is worse than one that reports. The pin that can land is a
> host test on the encoded bytes, which is what landed.

> **Update, landed: the two dumb-buffer divergences are closed lazily.** The table
> below measured three answers this card gave that Linux's does not, and two of them
> had one fix: `RESOURCE_INFO`'s `res_handle` was 0 for a plain dumb buffer where
> Linux names a resource, and `TRANSFER_TO_HOST` on one was `EINVAL` where Linux
> completes the copy. The reason the fix was deferred is in `crates/gpu/src/
> virtgpu.rs`'s header - making `CREATE_DUMB` create the device resource, as Linux's
> `virtio_gpu_mode_dumb_create` does, "would make every dumb buffer eagerly backed and
> add a device round trip to a path the console, weston and the fbdev node all
> allocate through". So the resource is made **lazily** instead, on the first request
> that addresses the handle as one (`drivers/drm.rs`'s `back_dumb_with_a_resource`):
> `RESOURCE_INFO` and `TRANSFER_TO_HOST` answer as Linux's do, and a caller that never
> asks pays neither the eager backing nor the round trip. That is a difference in this
> implementation and not in the ABI - the id is opaque and no request reports whether
> a resource exists without asking about it - and the pages materialised are the ones
> the scanout has been reading, so the transfer copies the pixels the caller drew.
> Pinned by `t-drm`, which now asserts a dumb buffer's `RESOURCE_INFO` is non-zero,
> that asking twice names the same resource, that the transfer completes, and that a
> handle this card never made is still `ENOENT`. The third divergence - a transfer
> whose rectangle does not fit the backing the device was given is refused where Linux
> would read past it - stays, and is a refusal rather than a different answer.

#### Measured: what Linux's card answers, request by request

The family is written from an observation rather than from the UAPI header,
because the part worth knowing is not in the header at all: **which requests a
card with no 3D refuses, and with which errno.** The instrument and the full
table lived in `user/staging/virtgpu-probe/` - that scratch tree is deleted, so
what remains of the instrument is this paragraph and the table below: Alpine's
netboot `virt` kernel in
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
you about vblanks" that disagreed would be the private dialect `AGENTS.md` §3.3
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

**The cursor is a plane, and it drives a host pointer.** The card has a second
plane whose `type` is Cursor, listed by `GETPLANERESOURCES` and described by
`GETPLANE`; the `plane_id` a `GETPLANE` carries finally selects *which* plane
answers, where it used to be ignored entirely and every question was answered with
the primary plane - a bug on its own, and a blocker for a second plane, since a
compositor finds its cursor plane by asking about each id and reading `type` back.
`MODE_CURSOR` and `MODE_CURSOR2` are the legacy door to the same plane; they are in
the `PRESENTATION` list with the other requests that put pixels on the screen,
because a pointer that keeps moving after the master loses its console is pixels.

**`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is now accepted, and the earlier refusal was
measured against the wrong card.** An older paragraph here kept the cap refused on the
strength of `modetest -p` reporting `type`, `IN_FORMATS` and `IN_FORMATS_ASYNC` as the
only properties on a real **amdgpu**'s cursor planes - no `HOTSPOT_X`/`HOTSPOT_Y`
anywhere. That measurement is correct for amdgpu and was the wrong instrument for this
device: virtio-gpu is one of the *virtualized* drivers the cap exists for, and Linux's
own virtio-gpu publishes `HOTSPOT_X`/`HOTSPOT_Y` on its cursor plane behind exactly
this cap. A weston boot made the consequence concrete: with the cap refused but the
cursor plane still enumerated, weston reported `cursor planes: yes`, bound the plane,
and got nothing on the window - the host renders a virtualized cursor only when the
guest hands it a hotspot-correct image, and a guest that never took the cap could not.
The cap is therefore recorded per open-file like `DRM_CLIENT_CAP_ATOMIC`, the two
hotspot properties are published on the cursor plane gated to a client that set it, and
the pointer point is carried through to the host (`Job::SetCursor`), which draws it.
The coordinate convention moved with the path: the kernel subtracts the hotspot from
the position before telling the device where the image's top-left goes, so accepting
the cap does not shift the pointer.

**The composition is the one thing here that is arithmetic, so it is the one thing
in its own host-tested module - and it is now the *fallback*, not the primary path.**
`crates/gpu/src/cursor.rs` holds the software blend - `ARGB8888` over `BGRA`,
premultiplied, hotspot-pinned, clipped at all four edges in one intersection computed
up front - with ten `cargo test` cases beside it, following `drm_blob.rs` and
`evdev.rs`. On a device with no cursor queue the driver calls it at the end of every
`copy_to_scanout`, which is what makes the two orderings come out right without either
side knowing about the other: a compositor's flip lands *under* the pointer rather than
erasing it, and a `MODE_CURSOR` move re-copies and re-blends, so the pointer moves
without the compositor redrawing anything. On a device that *has* a cursor queue the
host draws the pointer instead, so `paint_cursor` is skipped (`host_cursor_live` gates
the one call site) and a move costs one small cursor-queue command rather than a
full-frame gather and flush. The `ARGB8888` in that sentence is the format the fourcc
correction above had to land first.


**The cursor queue is used whenever the device has one; the blend is only for a card without it — and the measurement that settled it is the GL card's.** `host_cursor_live` was briefly narrowed to require `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` on the reasoning that a client which cannot name its hotspot has no honest placement for its image. That was wrong, and the GL card showed why: with the predicate narrowed, weston posted **no** `UPDATE_CURSOR` at all and the pointer vanished completely, where before it at least appeared. The hotspot is a *placement* question — the kernel sends position minus hotspot, which is the identity when no hotspot was set — not a reason to abandon the queue. The predicate is `card().cursor_q` again - one accessor, `host_cursor_live`, read from both the locked and the unlocked call sites.

**What the queue path was then measured to carry, and where the pointer actually goes missing.** With the predicate restored, on `virtio-gpu-gl-pci` (`virgl=1 context_init=1 cursorq=1`), weston binds its cursor plane through an atomic commit (`mpos-dbg: show_cursor want=show 64x64 at 639,399`) and the kernel posts it — but the image in the guest's own pages is **empty**: `cursor show 64x64 entries=4 guest nonzero=0` for every show, and QEMU re-sends a 64x64 all-transparent cursor over VNC (`CursorWithAlpha`, −239). The control that clears the card and the kernel of blame is a client of this repository's own (`.workbuddy/cursorprobe.c`): it fills a 64x64 ARGB buffer itself, calls `MODE_CURSOR`, and the same accounting reads `guest nonzero=2048 rows=[1024,1024,0,…] cols=[0,0,512,512,512,512,0,0]` — a 32×32 block at (16,16), exactly what it drew — reaching the host as 1024 non-transparent pixels. So the device, the resource backing, the transfer, the cursor queue and the read-back are all sound on the GL card, and **what is empty is weston's own cursor drawing**.

**The theme is not a switch between hardware and software cursors — it selects the
*artwork*, and weston's fallback artwork is a 2×2 white square.** Both halves measured on
`virtio-gpu-gl-pci`, no theme versus a named one, with the cursor plane bound through
atomic in both cases:

| `cursor-theme` | what reaches the guest's cursor pages | what the host gets |
|---|---|---|
| `breeze_cursors` named | every show: **0** non-zero bytes | an all-transparent 64x64 cursor (VNC `CursorWithAlpha`, −239) |
| absent | every show: **17** bytes, at image offsets `[0..8, 256..259, 263..267]`, all `0xFF` | a 64x64 cursor whose only non-transparent pixels are a 2x2 white block at (0,0) |

17 bytes is not a drawn arrow — it is two 8-byte runs on the image's first two rows,
i.e. a 2x2 opaque white square at the image's top-left corner, which is exactly weston's
built-in fallback when no theme resolves. That is the "small cursor" visible with no
theme named, and it is drawn at the pointer position because the hotspot is (0,0): the
pointer *is* the square's corner. With `breeze_cursors` named the buffer is **empty**
instead, so nothing is visible at all.

**Re-posting `UPDATE_CURSOR` on a move was tried and measured *not* to work, so the move
path posts `MOVE_CURSOR` again — and what settled it is a probe that needs no compositor.**
The move path did post `MOVE_CURSOR` alone, and the theory was that a hardware-cursor
plane scans its buffer live, so a compositor whose fill lands *between* posts would keep
repositioning an empty image (weston's first bind does come before its first repaint).
The re-post was made, then tested by `.workbuddy/curprobe.c` — one process that owns
the card, fills a 64x64 dumb buffer itself and walks four phases, read back through
VNC's `CursorWithAlpha` pseudo-encoding:

| phase | what the guest did | the host's pointer bitmap afterwards |
|---|---|---|
| P1 | fill opaque red, bind (`CURSOR_BO\|MOVE`) | 64x64, `R=FF` everywhere — **red** |
| P2 | fill a green 32x32 quadrant, `MOVE` only | still **red** |
| P3 | fill opaque blue, re-bind the *same* framebuffer | still **red** |
| P4 | bind a *second* framebuffer over the same blue buffer | **blue** |

So the image refreshes only when a transfer does. QEMU's `update_cursor` copies the
resource's *host-side* image — `virtio_gpu_update_cursor_data` is
`memcpy(s->current_cursor->data, pixman_image_get_data(res->image), 4096 * 4)`, no
format conversion and no per-pixel work — and only `TRANSFER_TO_HOST_2D` refills that
image. An `UPDATE_CURSOR` without a transfer re-sends what the last transfer left,
which is a bare move in a more expensive costume. Linux gates both the transfer and the
update on `fb != old_fb` and sends `MOVE_CURSOR` otherwise (`virtio_gpu_cursor_plane_update`),
so `MOVE_CURSOR` is what this driver sends. P1 is also the control that clears the card:
a filled buffer's colour reaches the host intact on both `virtio-gpu-pci` and
`virtio-gpu-gl-pci` (the alpha lane reads zero at the VNC client, which is that
client's own pixel format carrying no alpha, not the image).

**Where the pointer is actually lost: weston's cursor image is empty before it
reaches the card.** Kernel accounting over a whole weston session with pointer
moves (`.workbuddy/wcur.py`): weston makes exactly two 64x64 buffers, and they
are the only buffers in the session that arrive through `CREATE_DUMB` rather
than `VIRTGPU_RESOURCE_CREATE` — Mesa's `gbm_dri_bo_create` falls back to a dumb
buffer for anything asked with `GBM_BO_USE_WRITE`. Both are `ADDFB2`'d (fb 100,
fb 101) and bound *alternately*, so every pointer move takes the fresh path —
create, attach, transfer, update — and every one of those transfers reads
`nz=0`. The shim that logs the **caller's own source buffer** at
`gbm_bo_write` names the reason: `@@@PRE h3
buf=00000000000000000000000000000000` — weston writes 16384 *zero* bytes, and
the write honestly copies them. The read-back through a fresh `MAP_DUMB`
agrees, and the shim's own write through a second, writable mapping of the same
buffer lands (`own_write_landed=64/64`), so the buffer is reachable and nothing
is lost in flight.

Four controls clear everything downstream of that, each measured:

| control | result |
|---|---|
| a weston-free probe binding a magenta 32x32 block (`clientcursor.c`) | `cursor show … nz=3072` kernel-side; the host's pointer bitmap carries it |
| a `PROT_WRITE`-only mapping written and read **cross-process** (`curprobe.c` P8/P9) | `nz=16384`, the first byte round-trips — write-only maps, which is what weston asks for (measured `prot=0x2`), alias the same frames as a reader's `MAP_DUMB` |
| the same image bound on handle 2, 3 and 4 | each colour reaches the host — the slot is not it |
| `gbm_bo_write` from a plain process, handle 1 and handle 2 (`gbmprobe.c`) | the magenta block lands (`nz=3072 magenta=1024`) |

`gbm_bo_write` also fires only **sometimes**: weston writes the image when the
sprite changes, so a session in which desktop-shell never comes up shows no
write at all. The empty image is therefore upstream of this kernel — no client
has ever created a pointer surface with a sprite, and weston keeps committing
an all-zero buffer.

**The shell's own blocker was the epoll report, and fixing it completes the
startup (§1.1).** The 120-second stall that made desktop-shell look absent was
`vfs::epoll::report` ORing `EPOLLRDNORM` into every readable report: the shell
registers its timerfd for `EPOLLIN` alone, printed `unexpected timerfd events
41`, never read the timer, and never finished. With `revents & interest` in
place, the same boot sequence the earlier sessions used (weston 16, DRM
backend, `--renderer=gl`, a 1280x800 `Virtual-1`) now shows the whole chain:
the shell reads the registry, binds `xdg_wm_base`/`wl_seat`/`wl_shm`/
`zwp_linux_dmabuf_v1`, commits its panel (`wl_surface#19` damage `0,0,1280,32`
repeatedly), and drives the pointer (`wl_pointer#29.set_cursor(5,
wl_surface#26, 5, 5)` over more than two minutes of motion, with `enter`/
`leave` between its surfaces). The old `unexpected timerfd events 41` line is
gone from the log. So desktop-shell completes, and the one thing still missing
is its cursor *image* — the empty-buffer measurement above, which is upstream
(Mesa's virtio-gpu GBM allocation).

**The instrument chain that isolated it, each link measured on the same boot:** a control
client filling its own 64x64 buffer reaches the host as 1024 opaque pixels (card, queue,
transfer, VNC read-back all sound); `gbm_bo_write` from a plain process lands in the
kernel's pages (`nz=2048`) and reaches the host (`GBM` sound); the whole wl_shm chain —
memfd, fd pass over a unix socket, second process's `MAP_SHARED` mapping, both directions
of visibility — answers `SHM-CHAIN OK`; and 400 half-second samples of weston's own cursor
buffers across a session with pointer moves read `nz=0` every time — so the fill never
reached the bound backing, whatever weston's own `gbm_bo_write` did.

**The exit-path park — the flaky hang found and fixed.** A guest probe that exits with
the cursor plane bound hung in its own exit, forever: the probe's output file completed
(including the fork repro's last line), but the marker after the process exited never
came, on three consecutive runs. Split with bounded probes at both ends of the job: the
exiting task printed `hide: posting SetCursor 0`, the render task printed `job picked up`
and `device done rc=Ok` - and the caller's `done` wake never arrived. The mechanism: the
exit path marks the task `.exited` **first** (child_exit's first line) and releases the
address space after it - so the DRM file release runs while the task is already dead to
the scheduler. `render::job` parked the dying task in `block_until`, the completion wake
landed on a task the scheduler has stopped running ("a task with `exited` set is never
scheduled again" - the same rule this file's `release_exited` comment states), and the
process hung in exit; the parent's `wait` never returned. Fixed in `render::job`: a task
the scheduler has stopped running posts its device command fire-and-forget (queued in
order, the render task woken, the result dropped) instead of parking - safe because none
of the exit path's teardown commands reads the buffer backing they are about to release
(the hide is `id=0`, which loads no image; the unrefs and detaches are bookkeeping).
Measured: the same probe now exits cleanly, its parent's `wait` returns, and the whole
diagnostic sequence completes.

**Fork is innocent, measured with its own repro.** The suspicion that fork's copy-on-write
diverted the compositor's post-fork writes to a private copy died on a direct test: a
guest probe (`.workbuddy/gbmprobe.c`) maps and touches its cursor buffer **before** a
fork, the parent writes a second pattern through the same GBM BO **after** it, and the
child — a fresh \`MAP_DUMB\` read of the same handle — reports the parent's post-fork
pattern (\`child sees first8: bb 00 00 ff\`). Post-fork GBM writes land. The divergence is
specific to what weston's process does around its write, not to fork itself. (Also
measured en route: a \`MODE_CURSOR\` from a non-master is refused \`EACCES\` — the
presentation gate working as designed.)

**And weston's own write was watched directly, which is where the trail ends.** An
`LD_PRELOAD` shim interposing `gbm_bo_write` (resolving the real one through an explicit
`dlopen("libgbm.so.1")` — musl's `RTLD_NEXT` failed, measured) caught weston calling it
**once, `rc=0`, `count=16384`** — the full 64x64 image, successfully written. Yet the
buffer the plane binds reads zeros, and neither the `gbm_bo_create`/`gbm_bo_create_with_modifiers`
nor the `gbm_bo_get_handle`/`gbm_bo_get_handle_for_plane` entry points fired during the
session — the cursor BOs come through a creation path none of the interposed symbols
name. So weston writes 16 KiB of cursor image into an object that is *not* the backing of
the framebuffer the plane binds, on this Mesa/weston stack; the card, the ABI, the dumb
buffer, the map, the write-through, GBM's own write path and the wl_shm chain are all
cleared by controls. The next step is inside Mesa's virtio-gpu GBM allocation for cursor
BOs (which allocation the write lands in, and which one `ADDFB2` binds), and it is
upstream of this repository.

**One flaky hang met on the way, and its cause is the epoll bug above:** in one run of
several, weston stopped after `launching weston-desktop-shell` — no repaint, no cursor
binds — while the kernel and the watchers kept running; the next boot was clean. That is
the timerfd report the shell refuses (`unexpected timerfd events 41`, §1.1), and it was
intermittent because whether the shell's first timer expiry arrived before its next
`poll` depended on scheduling. With `revents & interest` in place the stall is gone and
the shell's panel and `set_cursor` traffic are continuous; the resource-creation flood
that precedes it (dozens of `target 0, 8x1` virgl objects) is not implicated.

**The plane and renderer paths, and the one thing that really switches them.** weston's
DRM backend uses the **cursor plane** when it has a `gbm_cursor_fb` to hand `ADDFB2`, and
the **renderer** otherwise (logging `cursor buffers unavailable, using gl cursors`).
Neither `cursor-theme` nor `cursor-size` touches that choice — they only pick artwork — so
on this machine the switch that matters is the *card*, because the software blend needs a
frame whose pixels are the guest's:

- `virtio-gpu-pci` (2D, no virgl): the frame is a guest dumb buffer, `copy_to_scanout`
  copies it and `paint_cursor` blends the arrow into the scanout — measured working
  (`paint_cursor at (639,399) = [255,255,255,255]`, visible in the VNC framebuffer);
- `virtio-gpu-gl-pci` (virgl): the frame is a *device* resource (`Resource::host_pixels`),
  `copy_to_scanout` returns early and the blend is structurally unavailable, so only the
  cursor queue can show a pointer — and that is the path whose image weston supplies
  empty or as the 2×2 stub.

**The software blend is a fallback that a device-frame cannot use, and that is now stated.** `copy_to_scanout` returns early for a framebuffer whose pixels are the device's (`fb_pixels_are_the_devices`), because the guest pages below it were never written; `paint_cursor` therefore never runs for such a frame, and blending into the driver's own scanout would not reach the display anyway while a client resource is being scanned out. On the GL card weston's frame *is* such a resource, so the blend is structurally unavailable there — it remains correct and measured for a card with no cursor queue and a guest-pixel frame (the 2D card, where `mpos-dbg: paint_cursor at (639,399) = [255,255,255,255]` showed the pointer in the VNC framebuffer).

**One gap in this area is named rather than filled.** An unknown CRTC in
`MODE_CURSOR` is `EINVAL`, which is what this file has always answered for an
object it cannot use, where Linux's `drm_crtc_find` says `ENOENT`; the whole file
would have to move together for that to change, so the cursor follows the file
rather than splitting it. The software-blend branch, once reachable only on a device with
no cursor queue (which neither configured machine has), is now exercised on this host
too: a client that binds the cursor plane without the hotspot cap takes that path by
design, and weston is exactly that client - measured 2026-10-21, the blend's pixels read
back out of the scanout over VNC.

**The other one - "the cursor plane publishes no `SRC_*`/`CRTC_W`/`CRTC_H`" - was
wrong, and the weston desktop caught it.** A wlroots or weston atomic state carries
one `drm_plane_state` for *every* plane, and `drm_plane_state_add_props` names
`SRC_X`/`SRC_Y`/`SRC_W`/`SRC_H` and `CRTC_W`/`CRTC_H` on each without first asking
whether the plane has them. So when the compositor built its cursor-plane state, those
six lookups returned handle 0; the commit reached the card as property 0, was refused,
and took the whole commit down - weston logged `couldn't set plane state` /
`atomic: couldn't compile atomic state`, and the pointer moved and clicked correctly
but was never seen, because the plane stayed empty. The paragraph above had said a
compositor wanting to crop or scale would "do it itself"; the measured truth is that
weston does not ask first - it names the rectangles unconditionally, and a card that
has not published them fails the commit rather than getting a smaller state. The plane
now publishes all eleven (`type`, `FB_ID`, `CRTC_ID`, `CRTC_X`/`CRTC_Y`, the six
rectangles and its own `IN_FORMATS`). They are *honoured*, not published-then-ignored,
which is what keeps the `AGENTS.md` §3 rule the original paragraph invoked intact:
this card's cursor has no scaler and no crop, so `Commit::cursor_rect_ok` accepts only
the identity rectangle (the image whole, at its own size) and refuses a real crop or
scale with `EINVAL` - the same answer a non-scaling hardware cursor plane gives. The kernel's
atomic cursor path is complete by inspection; reading the pointer back on the window is
what closed it - see the hotspot measurement above, which is the same boot answering it.

Properties and caps, cheap one at a time and load-bearing in aggregate: `EDID`
(a blob - with non-zero `mm_width`/`mm_height` and a `subconnector`, if anything
is ever to show a monitor's name), `non-desktop`, `max bpc`,
`panel orientation`, `vrr_capable`/`VRR_ENABLED` with
`DRM_CAP_ASYNC_PAGE_FLIP` beside them, `GAMMA_LUT`/`GAMMA_LUT_SIZE`,
`FB_DAMAGE_CLIPS` (the blob that lets a compositor say *what* changed instead of
sending whole frames), `OUT_FENCE_PTR`, `IN_FORMATS` with
`DRM_CAP_ADDFB2_MODIFIERS`, and `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` - asked for
precisely by the virtualized drivers, which is why this card accepts it (§7.4's cursor
account above).

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

> **Update (2026-10-09): the rule the `max bpc` row carries applies to every
> property that advertises a shape, and two of them were not keeping it.** A write
> outside the range or enum a property publishes is `EINVAL`, and an *accepted*
> write is what a caller reading the property back is told. So `DPMS` 9 and
> `ACTIVE` 2 are refused now rather than recorded and handed back as numbers their
> own enums have no name for - and each shares *one* value list with the read path
> (`DPMS_MODES`, `ACTIVE_VALUES`), so the shape advertised and the shape a write
> must match cannot drift.
>
> `pixel blend mode` was the other way round and worse: the write was dropped
> while the read path answered `Pre-multiplied` unconditionally, so a caller that
> wrote `Coverage` was told the write succeeded and then read its own answer
> contradicted - a property disagreeing with itself rather than merely ignoring
> the write, and it made `set_object_prop`'s own sentence ("every property here is
> state a compositor reads back") false of the one property that sentence was
> written for. It records the value now. Nothing here *composites* - the scanout
> is the compositor's finished frame, so the pixels are pre-multiplied by
> construction - which makes the value state rather than a mode, the same honesty
> `DPMS` and `ACTIVE` have.
>
> The same audit found the two *doors* disagreeing about one property: `MODE_ID`
> is a blob id, and the commit path has always refused one this card cannot show
> (`mode_fits`) - while `SETPROPERTY`/`OBJ_SETPROPERTY` recorded any number at all,
> so a caller could write an id naming nothing and read it back as the CRTC's
> current mode. That is the shape this file already warns about in the other
> direction ("a property settable through one and refused by the other is a client
> that read the property list and still cannot write what it read"), and the fix is
> not to share the handler - that was already true - but to share the *test*: the
> write arm now asks `mode_fits`.
>
> All three are pinned by `t-drm-props` - which reads the card's own mode from
> `GETCONNECTOR` and stores it as the blob `MODE_ID` names rather than assuming what
> a mode is. One doc-only fix rides with them: `DRM_CAP_CURSOR_WIDTH`'s
> note said "this card has no cursor plane, so nothing here consumes the size",
> which the cursor plane's landing had made false - and a stale premise is worse
> than a stale number, because it is the sentence a reader trusts. The cap and the
> two checks that take a cursor image now name the same `CURSOR_SIZE`.

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
A property whose write changes nothing is the silent success `AGENTS.md` §3
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

> **Update, measured and partly built (2026-10-09): the `*_FLAGS_TIMELINE` gap is
> narrower than "no behaviour", and it has a shape now.** The residual this
> section carried was that the two
> `DRM_SYNCOBJ_{HANDLE_TO_FD,FD_TO_HANDLE}_FLAGS_TIMELINE` flags - `1 << 1` in
> both directions, and in both headers installed on this host - had nothing to
> give them. Four probes on the same card, sweeping every combination of flags
> and `point` over a binary object, a timeline object, and both descriptors the
> export side makes, say what they mean: the flags are not a no-op, they are what
> makes the `point` field *mean* something.
>
> - **A non-zero `point` needs the sync_file form.** `HANDLE_TO_FD` with `flags =
>   0` or `TIMELINE` alone and `point = 5` is `EINVAL`, while the same point with
>   `EXPORT_SYNC_FILE` succeeds. The other direction holds the same rule:
>   `FD_TO_HANDLE` with `point = 5` and no `IMPORT_SYNC_FILE` is `EINVAL`.
> - **With `TIMELINE` the point is a *timeline* point**, so the export must name
>   one the object has reached: a binary object - which has no point at all -
>   refuses any non-zero point, and an object at 5 answers 0, 1, 3 and 5 and
>   refuses 6 and 100. Without `TIMELINE` the number is not read at all, and that
>   is what says the flag is what turns it into a point rather than the number by
>   itself: `EXPORT_SYNC_FILE` with point 5 on a *binary* object succeeds.
> - **On the import side the flag chooses which point lands.** Importing the
>   `sync_file` of a timeline at 9 into an object at 4 leaves that object at
>   **9** - the point the fence was made at - and adding `TIMELINE` to the same
>   request leaves it at the caller's **7** instead. So the flag's meaning there
>   is "the point *in this request* is the point to land on".
>
> The three refusals are built (`drivers/drm.rs`, each direction's own rule) and
> pinned by `t-drm-sync`, whose new checks include the 5-against-6 pair that
> separates "the object is a timeline" from "the object has reached *this* point".
>
> **The third bullet was built too, one measurement later, and the further
> measurement is what made it possible.** The paragraph above left it open because
> an import then had nothing to set the destination's point *from*: a `Fence` here
> is a retired flag with no point to inherit. Three more probes on the same card
> answered the questions that had to come first, and each answer is a rule rather
> than a detail:
>
> - **The two import shapes are two rules, not one with a variant.** With
>   `TIMELINE` the request names the point and it *raises*, like every other point
>   writer here - a destination at 4 imported at 2 stays at **4**, and at 6 goes to
>   **6**. Without the flag the *fence's* own point lands and it is **set**, not
>   raised: importing a plain object's `sync_file` into an object at 4 leaves it at
>   **0**, which is the one door in this family that may move a point backwards.
> - **The fence's point is the exporting object's, snapshotted at export.** An
>   export *asked for* `point = 5` from an object at 9 lands a later import on
>   **9** - so the number the request named is not the number that travels. That is
>   why the descriptor carries the point rather than a reference to the object: the
>   object may move on, and a descriptor already handed out does not.
> - **An import makes the destination a timeline, point 0 included.** A plain
>   object refuses `TIMELINE_WAIT`; after an import of a *plain* object's
>   `sync_file` it answers it from point 0. That is what makes the first bullet
>   observable at all, and it is why "reads 0" cannot be read as "never a
>   timeline": the two shapes read the same and answer differently.
>
> Built as `SyncObj::replace` in `crates/gpu/src/sync.rs` - one method, two host
> tests, one per half - and `SyncFileFd`'s `point` field in `drivers/drm.rs`.
> Pinned by `t-drm-sync`, whose new checks ask `TIMELINE_WAIT` as well as reading
> the point back, because reading 0 alone cannot tell "never had a point" from
> "just been given point 0".

**The sweep that followed found three faults, and one of them was the test's own.** With the
cap path landed, `t-drm` and `t-drm-props` ran red on the next sweep - which is what the pins
are for. `a HOTSPOT_X write reads back (got 0, want 4)`, `and HOTSPOT_Y` and
`RMFB removes the cursor's framebuffer (got -22, want 0)` were the three. Two were the kernel's.
`OBJ_SETPROPERTY`'s type check named `(OBJECT_PLANE, PLANE_ID)` and not
`(OBJECT_PLANE, CURSOR_PLANE_ID)`, so every write to a cursor property was refused with `EINVAL`
while the read of the same property still answered the old value - a half-written door, which is
worse than a closed one, because a client that reads back what it wrote concludes the write
worked. And the cursor plane's property list carried `HOTSPOT_X` and `HOTSPOT_Y` while its count
stopped one short of the second, so a client that had set the cap was shown one of the two; the
first fault's fix has to narrow the "shown" arm to the primary plane as well, or the cursor
plane's `FB_ID` would walk into `copy_to_scanout` and paint the cursor image over the whole
scanout. The third fault was the test's: `t-drm`'s `NR_RMFB` was `0xA4`, which is
`MODE_GETGAMMA` - an ioctl number encodes the command *and* the argument size, so the wrong one
is answered with the gamma request's `EINVAL`, and a test that reads that as "the card refuses
to remove this framebuffer" is reading a refusal to a question it never asked. `MODE_RMFB` is
`0xAF`, read out of `/usr/include/libdrm/drm.h` rather than written from memory, which is the
same lesson the lease family already carried. The sweep after the fixes: `pass=175 fail=0`, both
programs green, `rustfmt` clean.

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
   PCI functions carry the `subsystem` link they never had. The *measurement* this
   line was waiting for — the GL-card run that says whether the renderer is off
   `llvmpipe` — **has been taken (2026-10-13, the last update in this section),
   and the answer is no**: the card negotiates `capsets=2` and Mesa still answers
   `llvmpipe`, with `MESA-EGL: egl: failed to create dri2 screen` above it. What
   is left is the node's own devpath, which still reads
   `/sys/devices/virtual/drm/card0` where libudev's parent walk needs it under
   the PCI function.
6. ~~**`/proc/<pid>/maps`**~~ and the frame pool: the supporting cast, wanted by
   everything above and by nothing in particular. (`MADV_DONTNEED` was on this
   line, and §5.2 has it now; `maps` is §Remaining's, and it is written - see
   the board's entry for the two things it needed, the loader's recorded ranges
   and the `MAP_SHARED` bit the perms column reports.)

---

> **Update, measured (2026-10-13): the GL-card run happened, and it answers two of
> the three things it was for.** Taken on a host that now has
> `/dev/dri/renderD128` - so `-display egl-headless,gl=on -device
> virtio-gpu-gl-pci` starts, and each backend was checked by running it rather
> than assumed - booting the disk image and reading the guest over the wire
> (`cargo xtask run --mode terminal`, driven on stdin), which is the shape this
> section's earlier updates used.
>
> - **The 3D half is negotiated for real.** The kernel's own line reads
>   `virtio-gpu: virgl=1 context_init=1 cursorq=1 capsets=2 cached=2 mask=0x6`
>   where the shipped 2D card reads `virgl=0 ... capsets=0`. That closes the
>   `DRM_VIRTGPU_GET_CAPS` question from the other end: with cap sets present the
>   request is *served*, and no `mpos: ioctl(...) -> ENOSYS` line appears anywhere
>   in that boot. On the 2D card the same request is `ENOSYS` because
>   `num_capsets == 0`, which is Linux's own gate (`virtio_gpu_get_caps_ioctl`)
>   and therefore not a gap this kernel could close by answering something else.
> - **And the renderer is still `llvmpipe`.** `eglinfo -B` on that boot reports
>   `OpenGL core profile renderer: llvmpipe (LLVM 23.1.3, 128 bits)` on the GBM
>   and surfaceless platforms, with `MESA-EGL: warning: egl: failed to create dri2
>   screen` / `DRI2: failed to create screen` above it. So `is_software()` is
>   still true and niri still refuses it - `failed to initialize renderer, falling
>   back to primary gpu: software EGL renderers are skipped`, then `error adding
>   primary node device, display-only devices may not work: no allocator available
>   for device`. The same two lines as on the 2D card, which is the point of
>   taking the run.
> - **The identity chain is built; the node's own path is not.**
>   `readlink -f /sys/class/drm/card0` is `/sys/devices/virtual/drm/card0` while
>   `readlink /sys/dev/char/226:0/device` is `../../../pci0000:00/0000:00:04.0`,
>   and `vendor`/`device` read `0x1af4`/`0x1050` *through the class link*. The PCI
>   identity is therefore reachable by following the link, while the devpath -
>   which is what libudev's parent walk follows - still says `virtual`. That is
>   this section's earlier lead, now with both paths side by side.
>
> **The GL card is one key away and is not the shipped default, and both reasons
> are measured rather than assumed.** With `qemu.gpu = "virtio-gpu-gl-pci"` the
> sweep *passes* on it (`=== SUMMARY pass=169 fail=0`, `mpos-kunit 30 passed, 0
> failed`) - but QEMU returns no clean debug exit (`QEMU exited with -1 (33 is the
> clean debug exit)`), so the gate refuses to call the run green, which is the gate
> doing its job; and virglrenderer rejects a resource the guest asks for
> (`vrend_resource_create, Illegal resource parameters, error: Cube map:
> unexpected array size 1`). Telling "the card" apart from "the headless GL
> backend the gate must name beside it" is the next measurement; until then the
> shipped machine stays 2D and `machine.toml` carries the whole reading.
>
> Two smaller things the same boots measured. **`niri` and `xwayland-satellite`
> are packages in the image now** (`rootfs.toml`): the compositor had been
> installed by hand in a guest, which is why it reported `error spawning
> xwayland-satellite ... No such file or directory` - a warning about a binary the
> image did not carry, not a kernel answer. With the package installed that line
> is gone and a *different* one takes its place, which is a new finding rather
> than a fix: `error opening X11 sockets, disabling xwayland-satellite
> integration: error binding to abstract socket ... Invalid argument (os error
> 22)` - an abstract `AF_UNIX` bind answered `EINVAL`. Named here, not explained:
> `t-unix` pins abstract names, so the shape to find is what *this name* does
> differently. **And a boot that renders nowhere cannot name `none` on a GL card**
> - QEMU refuses the device beside it - so `xtask/src/qemu.rs` derives the
> headless GL backend from the card for a `terminal` run and for a `test`/sweep
> boot rather than asking a second key for it; without that a GL machine cannot be
> booted headlessly at all, which the first attempt at this run proved by failing
> to start.
>
> **The page faults that flood a desktop boot on an image with `niri` installed
> are a different bug, and they are fixed.** The kernel's "is this address free?"
> question could not see the mappings that carry no VMA, so a second mapping asked
> for with such an address as its *hint* was placed over the first - and the write
> that followed had a resolver for neither a demand fill (the page is present) nor
> a copy-on-write split (it carries no marker), so the task was killed. It is the
> reason a desktop boot with `niri` installed showed `PFDIAG` floods and dead
> `weston-*` clients, and it is why "install niri" looked like the trigger: what
> matters is *a library whose two mapping passes meet on one page*. `docs/notes/`
> §5.4 carries the whole chain (`/lib/ld-musl-x86_64.so.1` writing a relocated
> slot, the twenty shapes that all behave as Linux does, and `t-mmap-hint`, the
> pin that fails without the fix and passes with it).
>
> **And the card's devpath is Linux's now, which was necessary and is not
> sufficient** (2026-10-13). The node used to hang under
> `/sys/devices/virtual/drm/card0` with a `device` link to the function, and
> libudev's parent walk follows the **devpath** rather than a link - so the card
> described itself as a virtual device with no bus behind it. It is now what
> Linux registers: a *child* of the function its driver declared
> (`/sys/devices/pci0000:00/0000:00:04.0/drm/card0`, measured on the machine, and
> pinned by `sysfs`'s `the_cards_devpath_runs_through_its_function`; a machine
> whose bus names no display function keeps the `virtual/virtio-gpu` stand-in,
> because there is no parent to name then).
>
> **What that did not do is produce a renderer**: `eglinfo -B` on the same boot
> still answers `llvmpipe` and still prints `MESA-EGL: warning: egl: failed to
> create dri2 screen` / `DRI2: failed to create screen`. So the devpath was the
> *lead* this section recorded and not the whole cause, and the difference is now
> measured rather than assumed. The same boot also clears the image: 
> `/usr/lib/dri/virtio_gpu_dri.so` is installed (beside `libEGL.so.1` and
> `libgbm.so.1`), so Mesa has the card's own driver and fails *inside* its screen
> creation. The next measurement is that failure with the loader talking -
> `LIBGL_DEBUG=verbose MESA_DEBUG=1 eglinfo -B`, then
> `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`, which separates "the loader cannot
> find the driver" from "the driver cannot open the device".
>
> **A route to niri *today*, read off its source rather than measured here
> (2026-10-13): the TTY backend is not niri's only one.** `src/backend/mod.rs`
> carries `Tty`, `Winit` and `Headless`, and the Winit backend opens a 1280x800
> *window* in whatever compositor is already running (`WindowAttributesWayland`,
> the surface named `niri`), its renderer an EGL context on that window. The
> `ensure!(!egl_device.is_software())` check lives in the TTY backend's
> `GpuManager` and **not** in `winit.rs`, so llvmpipe is enough there - which is
> the whole difference. Selection is environmental, and `main.rs` says it in as
> many words: with `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY` set the
> *winit* backend is chosen (`--session` removes them to force TTY). smithay's
> `backend_winit` is behind no feature flag in niri's `Cargo.toml`, so the
> image's build carries it, and the image carries what an EGL context on a
> Wayland window needs - `libEGL.so.1`, `libgbm.so.1`,
> `libwayland-{client,server,egl}`. The recipe is `weston` first (which runs on
> llvmpipe today) and then, from a shell that can see weston's socket,
> `WAYLAND_DISPLAY=wayland-1 niri`. **Untried in the guest**: this paragraph is
> the source and the package list, not a boot.

---

### 7.5 The device set is assembled by the drivers that own it — **LANDED**

`drivers::devices::standard_devfs` used to decide which hardware nodes to publish by naming the
drivers: `if let Some(rng) = crate::drivers::virtio::rng::global()`, and the same shape for the
console port, the sound card, the framebuffer, the input registry. That is registration reaching
into the drivers, and one file to edit for every new device; it also let the two drift, because the
builder had to know which of a driver's globals were populated (the aarch64 entropy node was a
second `#[cfg]` arm here until §15.2 unified the transport).

Inverted: `devices::publish(owner, nodes)` fills a registry, and `standard_devfs` drains it without
naming a single device. Each probed driver publishes its own nodes at the one install door it
already had — `rng::install` (the path both buses reach, the PCI `init_global` and the MMIO
`rng_device`), `console::install`, `sound::install`, `VirtioGpuDev::install`, `input::register` — so
the probe's answer decides whether a node exists, at the place the probe happens. Publishing is
idempotent per owner, so a driver that comes up on two buses, or retries, cannot produce two nodes
under one name.

What deliberately did not move is the distinction the nodes themselves draw: the core set (the
console, the ttys, the entropy *pair*, the null/zero/full sinks, `kmsg`, `ptmx`, the `/proc` links)
is the device interface every machine has rather than the result of a probe, and `rtc0`/`rtc`, the
`/dev/dri` pair and the disk projection stay in `devices.rs` for the same reason — the subsystem
exists whether or not a device answered. The entropy pipe's `CharDevice` adapter moved to
`virtio/rng.rs` with its node; the others already lived in their drivers.

Measured: host tests `mpos` 517; x86_64 `cargo xtask test` `=== SUMMARY pass=160 fail=0 (160 built)`
with QEMU exit 33; aarch64 `cargo xtask test --arch aarch64` `=== SUMMARY pass=143 fail=0`; both
bare-metal builds with no new warning; `cargo xtask depgraph` within the ratchet, mutual pairs 8/8 —
the inversion adds no module edge, because a driver calling `crate::drivers::devices` stays inside
the `drivers` module. The aarch64 boot check that resolves `/dev/hwrng` and `/dev/input` through the
mounted tree (`check_dev`, `M7_DEV_OK`) is the end-to-end evidence that the driver-published nodes
are the ones userland sees.

### 7.6 Configuration space over MMIO: what the firmware names, and the reserved word that hid it — **LANDED**

x86_64 reads configuration space through the 0xcf8/0xcfc port pair (mechanism 1), which
reaches 256 bytes per function - the address register has eight bits of offset - so a
device's PCIe extended capabilities are outside it. A PCIe machine may instead decode
configuration space as *memory* (ECAM, or MMCONFIG: one 4 KiB page per function, one
1 MiB window per bus) and say where in ACPI's MCFG table. Enumerating over MMIO with the
port pair as the fallback starts with reading that table.

Neither prerequisite existed. The kernel had **no ACPI support at all** - the machine that
describes itself with tables was the one whose description the kernel ignored - and the
loader's configuration-table walk knew only the device-tree GUID. Both are in now:

- `crates/acpi`: the root pointer (both checksums, and the extended one is a condition on
  using a revision-2 pointer), an SDT header validated against the bytes that arrived, the
  RSDT/XSDT walk, and MCFG's windows down to the per-function offset. Fourteen host tests,
  and `fuzz/acpi_tables` (3.4M executions clean).
- the loader finds the root pointer in the firmware's configuration table (the ACPI 2.0
  GUID first, the 1.0 one second) and validates it *there*, while the firmware's memory is
  certainly still mapped, before writing `rsdp_phys` into the record. The field is appended
  after the device-tree pair for the reason that field's own note gives. The tables are not
  copied, unlike the device tree: the memory map reports them ACPI-reclaimable, which the
  frame allocator never hands out, so the kernel reads them through the direct map.
- `drivers::pci` walks them and says what it found. On this host, in the sweep's own boot:
  `pci: MCFG names ECAM at 0xe0000000, segment 0, buses 0..=255, 256 MiB`.

**The first version of the parser was wrong, and the boot is what caught it.** MCFG keeps
**eight reserved bytes** between its header and its first entry. A reader that starts the
entry loop at the header's end reads the reserved word as an entry's base, the entry's base
as its *segment*, and the entry's end bus as its own - and the answer that produces is not
a machine, it is a plausible-looking one: `base 0x0, buses 0..=224` where the window is
really `0xe0000000, 0..=255`. The host tests agreed with the bug because their fixtures
were built the same way, which is the lesson worth keeping: a fixture that encodes the
same mistake as the code proves nothing about the format. Both are fixed, and the wrong
shape is a test of its own now (`the_reserved_word_is_not_read_as_an_entry`).

A second, smaller one was the same class: the first walk validated the root table from a
36-byte read, so the table's own declared length looked like an overrun and every table
was refused. Reading a table is necessarily staged - the length is in the header and the
checksum is over the whole table - so `crates/acpi` grew a `declared_length` door that
says what the header claims *without* validating it, and `pci::read_table` reads the
length, then the table, then validates the whole of it.

**What was deliberately left for the next slice, and why it was a step with a size.** With
the tables read, the window was *named* and not yet *used*. Switching reads to it means
mapping it, and this machine's MCFG covers 256 buses - **256 MiB of address space**. Mapping
that eagerly, for a walk that touches two or three buses, is the wrong trade; the next step
was a per-bus mapping (1 MiB on demand), an equivalence check that ECAM and mechanism 1
answer the same for the same function before the switch is trusted, and mechanism 1 kept as
the fallback for a machine whose firmware names no window at all. It is §7.6a, immediately
below.

### 7.6a The switch itself: mapped a bus at a time, and trusted only after both mechanisms agreed — **LANDED**

The three pieces the paragraph above named are in, and the shape they took is worth
recording because two of them are not the obvious ones.

**One address, two mechanisms.** `drivers::pci`'s six accessors (`read8`/`read16`/`read32`
and their writers) are now a dispatch rather than a port pair: they ask a window for the
register's kernel-half address and fall back to the ports when there is none
(`window_byte` → `slice`). The port bodies are the same code they were, renamed
(`port_read32`/`port_write32`/…), and every caller above - `config_read`/`config_write`,
`Func`, `read_function`, `read_bars`, `scan_bus`, and through them `virtio`'s
`PciTransport` and the `config` files under `/sys` and `/proc` - is unchanged, because the
mechanism is not its business. The masks are load-bearing and identical either way:
`off & 0xfc` for a dword, `& 0xfe` for a word, nothing for a byte, so "which mechanism read
this" cannot change which bytes were touched.

**A bus at a time.** `slice(bus)` maps that bus's mebibyte on the first access to it, into
this machine's MMIO window (`hal::map_mmio_window`), and remembers where in a table indexed
by bus number - `[usize; 256]`, with zero meaning "not yet mapped", which is unambiguous
rather than a convention because no mapping this machine hands out is ever at address zero.
A slice is never handed back: the bus is walked for the machine's life and a driver probing
an hour later asks for the same one. The map itself happens *outside* the critical section
that reads the table, because mapping allocates frames and writes page tables and that is
not work to do with interrupts off. The two steps cannot disagree: `slice` is the only
writer of a window's `slices`, `use_window` and `use_ports` run at most once each and only
at boot, and a task preempted between them could only find the entry still zero and map the
same bytes a second time. A window that does not cover a bus, or a machine with no room left
for a mapping, answers `None` and that access goes through the ports - the honest refusal
rather than an address nothing answers for.

**The check, and why it is a check rather than a preference.** `init` walks the bus over the
ports *first* - the mechanism that asks nothing of the firmware, and therefore the one known
to work - and, if the firmware named a window, walks it again through the window and
compares the two `Vec<Device>`s, which compare element for element because the model already
derives `PartialEq` (`crates/pci`). Equal: the window is kept, and from then on discovery
and the `config` files read memory. Different: `first_difference` names the first function
the two walks disagreed about (or says that they did not even find the same *number* of
them), the ports are put back, and the boot log says so. A window that starts above bus 0 is
refused for a sharper reason than "it cannot serve the walk": the check would be *vacuous*.
`enumerate` walks from bus 0, so every access on it would fall back to the ports, the two
snapshots would agree by construction, and the boot would claim a window it never read a byte
through - the one failure mode a check like this must not have. That is also why
the `Ecam` struct this module used to keep is **gone**: `discover_ecam` now returns
`crate::acpi::EcamRange` itself, so "does the window cover this bus" and "where is that bus's
slice" are answered by the host-tested crate instead of by a kernel-side copy of the same
four fields.

Making the layout one definition rather than two is `crates/acpi`'s new `function_offset`
(device and function bits inside a bus's mebibyte), which `EcamRange::offset` is now the bus
term plus - so a kernel mapping one bus at a time and a reader walking the whole window
cannot drift, and a window that does not start at bus zero is the case that would have
shown it. Host test: `a_bus_slice_and_a_whole_window_are_the_same_layout`; the fuzz target
reaches it through `EcamRange::offset`.

**Measured.** On the sweep's own machine, in its boot log:

    pci: MCFG names ECAM at 0xe0000000, segment 0, buses 0..=255, 256 MiB
    pci: ECAM and the port pair agree on all 14 functions; configuration space is over MMIO
    pci: 14 functions enumerated

and then the whole sweep through the switched mechanism: x86_64 `cargo xtask test`
`=== SUMMARY pass=160 fail=0 (160 built)` with QEMU exit 33 - which is the evidence that
matters, because every driver in it (virtio over `PciTransport`, AHCI, the GPU and its
DRM card) probed its device through the window. aarch64 is untouched: `drivers::pci` is
x86_64's module, and that arm's `pass=143 fail=0` stands. Host tests `mpos-acpi` 14 → 15.
`cargo xtask depgraph` within the ratchet (the switch adds no module edge: `drivers::pci`
already called `hal` and `acpi`).

**A rustfmt catch-up rode along, and it is named rather than hidden.** `cargo fmt --check`
was red at `HEAD`: the two previous commits had landed unformatted code
(`EcamRange::offset`'s over-long signature line, and two spots in `drivers::devices`'s new
`Publisher` drain and `virtio/rng.rs`'s import order). The first two are in files this slice
touches anyway; the last two are their own commit, because a formatter pass and a mechanism
switch have no common *why* (`AGENTS.md` §3's one-intent rule).

### 7.6b The other 3840 bytes: the device says how long its configuration space is — **LANDED**

The follow-on §7.6a named was written as "a size rather than a smell": the window reaches a
full 4 KiB per function, and every reader still stopped at 256. The doubt it recorded - that
a `/sys/.../config` file's size would start depending on which mechanism the firmware named -
dissolved once the rule was measured: Linux's size is the **device's** answer, and the probe
that asks it uses whichever mechanism is in use. A port-only machine therefore answers 256
for every function, exactly as a machine with no window should.

**Measured on a current host first** (`.workbuddy/probe_pci_ext.c`, Linux 7.2.11). Of the
host's functions, `/sys/bus/pci/devices/*/config` was 4096 bytes for every PCIe function and
every host bridge (class `0x0600`), and 256 for the IOMMU, the SMBus controller and the LPC
bridge - the three that are neither. As root, the 4096-byte files answered at 0x100, 0xfff
and 0xff8 (a 16-byte read at 0xff8 returned 8), and 0x1000 was EOF; a 256-byte file was EOF
at 0x100. The kernel source says why (`drivers/pci/probe.c`, `pci_cfg_space_size`): a PCIe
function, a host bridge and a PCI-X mode 2 bridge are asked for the dword at 0x100; all ones
means the extended space is not reachable, and the standard header repeated at every 0x100
boundary means a bridge is aliasing its low 256 bytes (`pci_ext_cfg_is_aliased`). Everything
else is 256 without a question. The same probe found the second, unclaimed rule: a caller
without `CAP_SYS_ADMIN` in the initial user namespace is clamped to the first **64** bytes
(`pci_read_config`'s `size = 64`), which this kernel does not copy and `t-pci` names.

**Served** by `crate::pci::cfg_space_size` (the rule, host-tested with synthetic probes and
the alias case), `crate::pci::find_capability` (the standard capability list, 48 nodes),
`Device::cfg_size` (probed once at enumeration, Linux's `dev->cfg_size`), and the six
accessors widened to twelve-bit offsets. `config_read`/`config_write` take the function's
size; `/sys/.../config` and `/proc/bus/pci/...` carry it in their metadata and pass it
through. With no window, an access above 0xff answers the absent-function value (all ones,
or a dropped write) rather than folding onto the low 256 bytes.

**Two traps the machine found, and both were ours.**

* **The alignment masks were still eight bits wide.** `window_byte` was correct, but
  `read32` masked the offset with `0xfc` and `read16` with `0xfe` - literals that were fine
  while an offset was a `u8` and that clear bits 8..15 once it is not. Every extended read
  folded back onto 0x000, so the 0x100 probe answered the vendor/device dword and the rule
  called it an alias. The masks are `!0x3`/`!0x1` now, and restoring the old ones turns
  `t-pci` red (counterfactual below). This is the one shape a widened type can hide: the
  arithmetic compiled, the address looked right, and the device answered - with the wrong
  register.
* **q35's own devices publish no extended space.** Measured on the gate: all fourteen of its
  pre-existing functions are conventional PCI (none carries capability id 0x10), so `t-pci`
  could pin only the 256 side. A bare `pcie-root-port` does not help either - QEMU answers
  its 0x100 with the standard header repeated, the aliasing case the rule *should* refuse.
  The test machine now carries one `nvme,serial=...` controller (no `drive=`, so no
  namespace and no block device); its AER capability at 0x100 is a real extended answer. An
  installed disk does not get it: this is the bus the pins describe.

**Measured.** x86_64 `cargo xtask test`: `pass=189 fail=0 (189 built)` - `t-pci`'s seven
claims all `ok:` - `mpos-kunit 32 passed, 0 failed`, exit 33. aarch64 `pass=188 fail=0`,
`mpos-kunit 32 passed, 0 failed`, `=== END`; `t-pci` skips there because that machine's
`config` files have no read path (`mpos_pci_config` is x86_64's), which is a pre-existing
limit this item names rather than moves. Host tests `mpos-pci` 19 → 22. `cargo fmt --check`
clean; clippy 77 (x86_64) and 82 (aarch64), the standing baseline; `depgraph` within the
ratchet.

### 7.6c The second machine's configuration space too: the capability was the missing word — **LANDED**

§7.6b closed with one skip named: on aarch64 `t-pci` printed `skip: this machine's
configuration space is not readable through this path`, because `mpos_pci_config` was the
first machine's flag. The skip was honest and the reason was not the bus. The boot log
already said

    pci: the tree names ECAM at 0x4010000000, buses 0..=255, 256 MiB
    pci: ECAM is this machine's only mechanism and the walk read 2 functions through it

so enumeration, the `Device` model, `cfg_size`, `/sys/bus/pci` and `/proc/bus/pci` were
already the second machine's; only the read/write halves of the two `config` attributes were
compiled out, and the trait's default body answered `EOPNOTSUPP` (the kernel's own line on
the red run below is `mpos: syscall 67 (fd 3) -> EOPNOTSUPP`, 67 being `pread64`).

**The missing word, not a mechanism.** `--cfg mpos_pci_config` joins aarch64's rustflags in
`.cargo/config.toml`. The mechanism was already the machine's: x86_64 has mechanism 1's port
pair and usually a firmware-named window, aarch64 has the window its device tree names, and
the accessors already answer an absent function when neither serves an access. The comment
that said "a `virt` machine has no PCI on the MMIO path this port drives" was true before
`discover_ecam_dt` and is false now; it names the window and the capability's meaning
instead, and §15's capability table loses its fourth missing entry.

**One PCIe function for the second machine's pin.** Once the files were readable, the
machine's functions were the host bridge (asked by the rule, with an empty extended space)
and the `ich9-ahci` controller (conventional PCI, 256 bytes) - neither can pin "a PCIe
function publishes 4096 bytes". The test boot now attaches `nvme,serial=mpos-pci-ext` with no
`drive=` (so no namespace and no block device), the same device and reason as the first
machine's `Boot::Esp` arm. It rides a `test_machine` parameter threaded through
`virt_machine_args`/`boot_args`: the sweep's boot gets it, a session boots the installed
machine's device list and does not, and the parameter has its own host test
(`the_second_machines_test_boot_carries_one_pcie_function`, 123 xtask tests in all).

**The skip was the wrong shape for a machine that publishes a bus.** `t-pci` used to return
early when the first `config` file could not be read. It makes that a claim now - `every
published function's configuration space is readable through this path` - because a machine
that enumerates functions and then answers nothing for their registers is publishing a tree
it cannot serve. The only skip left is the host's own `CAP_SYS_ADMIN` clamp, guarded by
`host_run()`; the claim and the length rule are the two that go red on the counterfactual.

**Measured.** aarch64 `cargo xtask test`: `=== SUMMARY pass=188 fail=0`, `mpos-kunit 32
passed, 0 failed`, `=== END`; `t-pci`'s **eight** claims all `ok:` against **3** functions,
the NVMe's included (`a PCIe function publishes the 4096-byte extended space`), and the boot
line is `pci: ECAM is this machine's only mechanism and the walk read 3 functions through
it`. x86_64 unchanged and green: `pass=189 fail=0 (189 built)`, `t-pci`'s eight claims
against 15 functions. Host tests 579; `mpos-xtask` 123. `cargo fmt --check` clean; clippy 77
(x86_64) and **80** (aarch64 - two fewer than §7.6b's 82, because the capability makes the
config pair live code rather than dead); `depgraph` within the ratchet, mutual pairs 8/8.

**Counterfactual, every other pin kept.** Removing `--cfg mpos_pci_config` from aarch64 and
re-running the sweep: `pass=187 fail=1`, `=== FAIL t-pci`, and exactly the pair above is red
(`every published function's configuration space is readable through this path`, `3 config
attributes are as long as their device says, 256 or 4096`); the conventional and extended
claims then skip, because no file could be read to classify. Green source restored
byte-for-byte (`diff -q`) and the final aarch64 run repeats `pass=188 fail=0`.


---

### R.7 (moved from ROADMAP's status board) — the weston seat chain, the compositor's cursor and console, and the gaps that have since closed

**`start-weston.sh` survives being run twice.** It asks whether a seatd is *answering* rather than whether `/run/seatd.sock` exists (a socket path outlives the daemon that bound it, here as on Linux), removes the stale name before a new daemon tries to bind it, and starts seatd in its own session: the script `exec`s into weston, so a seatd backgrounded there shared the compositor's process group and `^C` on the console — which signals the whole foreground group — used to kill the seat manager along with the desktop.

**A parked evdev reader — direct, `select`, or an `epoll` interest like a compositor's — is woken by the service task when a record lands**, instead of at the next tick; the wait re-checks the queue as it parks, so an event between a failed read and the sleep cannot be lost.

**`--mode gui` keyboard rule, stated exactly.** A keystroke reaches the active console's shell unless a compositor is *both* grabbing the device and holding the screen; that conjunction, not the grab alone, is what takes the keyboard off the shell, so a plain GUI console (nothing has grabbed the device) is typable again — as is the same console after `^A` moves its session off the screen while a stale claim survives (§7.1, `input::service`).

**The pointer composited by the one flusher**, in a host-tested pure function (`crates/gpu/src/cursor.rs`, 10 host tests); `CRTC_X`/`CRTC_Y` steer rather than merely record (`t-drm-props`), and on a device with a cursor queue the host draws the pointer instead - `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is accepted and `HOTSPOT_X`/`HOTSPOT_Y` published behind it, and binding or moving the plane posts virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR` (§7.4). The software blend remains the no-cursorq fallback.

**The two blobs the property families point at, as pure encoders** in `crates/gpu/src/blob.rs` with 15 host tests — `drivers/drm.rs` is kernel-only, so the bytes a real reader parses had to live somewhere a host can check them. Verified against `edid-decode` rather than only against the module's own decoder, which caught a real bug (a descriptor's first two bytes) (§7.4).

**Gap closed — the aarch64 session environment.** A login shell on the aarch64 image had no `XDG_RUNTIME_DIR` (measured 2026-10-11) and weston started from it stopped at `fatal: failed to add socket`. The files that provide it now ship in the shared `core/` overlay, so both machines' login shells have it (§15.36/§15.37).

**Gap closed — weston's EL0 write fault, and the cursor it was hiding.** With a runtime dir weston used to reach `Compositor capabilities:`, create `wayland-1`, load `desktop-shell.so` and launch both helper processes, then take a genuine EL0 **write** fault one page above its stack pointer (`ESR 0x92000047`, a level-3 translation fault with `WnR` set) that the resolver refused for want of a VMA (§15.36's last note). The desktop now comes up whole: the fault is resolved by `resolve_stack_growth` on the aarch64 EL0 data-abort path (`arch/src/arch/aarch64/vectors.rs`, alongside the COW and anon resolvers), so the push past the stack boundary grows the stack instead of ending the task. What that boot then showed was the pointer never appearing even though the desktop rendered — and two causes were behind it, both now fixed. First, this card's cursor plane had failed to publish the rectangles weston names unconditionally; §7.4 carries the measurement and the fix (the plane publishes `SRC_*`/`CRTC_W`/`CRTC_H` and honours them as identity-or-refuse). Second, and the reason a *correctly bound* plane still drew nothing on a virtualized display: the card refused `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` while still enumerating the cursor plane, so weston reported `cursor planes: yes`, bound the plane through atomic, and the host - which only draws a virtualized cursor handed a hotspot-correct image over the virtio-gpu cursor queue - showed nothing. The card drove its own software blend into the scanout and never configured the device's cursor queue at all. The fix routes the pointer to the host: the cursor queue is configured when the device has one (probed at bring-up, published as `Card::cursor_q`), the hotspot cap is accepted and `HOTSPOT_X`/`HOTSPOT_Y` are published per-client, and binding or moving the plane posts `UPDATE_CURSOR`/`MOVE_CURSOR` on the cursor queue (`Job::SetCursor`/`MoveCursor`) so the host composites the pointer over whatever frame the guest presents; the software blend remains only as the fallback for a device with no second queue. Pins: `t-drm-props` proves the cap + hotspot enumeration gating, `t-drm` proves an `RMFB` of the cursor's framebuffer takes the plane's binding down (no dangling host resource). **Window confirmation is the remaining step:** the kernel path is complete and gated green on the host and both bare-metal builds, but reading the pointer back on the QEMU window after a `cargo xtask run --mode gui` weston boot is what closes this gap for real, and the no-cursorq fallback branch cannot be exercised on either configured machine (§7.4 names that as a gap).

**Gap closed - niri comes up on the GL machine, and the four answers that stood between `virtio-gpu-gl-pci` and a picture.** Measured 2026-10-14, on the same host, by driving the guest's own shell (`.workbuddy/guest-probe7.py` and `-9`: the script the guest runs is a *file* it reads off the 9p share, because a long command line typed into a serial console is delivered only halfway once a compositor takes the console into `KD_GRAPHICS`; its output is written to the guest's own root and read back out of the image with `qemu-nbd` + `debugfs`, because nothing written through 9p has survived a power-off). Every one of the four was found by asking the guest rather than by reading Mesa, and none was where the symptom appeared:

1. **`VERSION` answered `4.0.0`, and Mesa's virgl winsys refuses any major but 0** - `virgl_drm_get_version` returns `-EINVAL` for `version_major != 0`, so the whole virgl path went down before the card's identity was ever asked, Mesa fell back to `llvmpipe`, and a compositor that refuses a software EGL device had no renderer at all. It answers `0.0.0` now, and the minor being 0 is the same honesty: it is the fence-fd feature version, and this kernel's `EXECBUFFER` refuses `VIRTGPU_EXECBUF_FENCE_FD_OUT`, so a card that said 1 there would promise the one thing it then answers `EINVAL` on. Linux answers 0.1.0 because its execbuffer has the fence.
2. **A resource id and the GEM handle were two numbers.** A resource id is a field the guest's *command stream* spells, and it spells it with the handle `RESOURCE_CREATE` answered - but ids came from a `next_res_id` counter that only went up while handles went back into the table, so the two drifted apart at the first released handle and the stream named resources the device had never been told about. `vrend` answered `Illegal resource <n>` and Mesa's render process stopped: no picture, for a reason three layers away from the number that caused it. One number now - the slot `RESOURCE_CREATE` claims *is* the id it puts on the wire, claimed inside one critical section because the device is talked to outside every lock - and the driver's own two resources moved out of the clients' range entirely (`RESOURCE_FB` was **1**, the bottom of the handle table).
3. **`OBJ_GETPROPERTIES` refused the encoder this card itself advertises.** `GETRESOURCES` names `ENCODER_ID` and `GETCONNECTOR` names it back, but the request only knew the connector, the CRTC and the two planes, so the one object with nothing to give was answered `EINVAL` - and to a client enumerating what the card listed, a refusal is not an empty list, it is a card that will not open: `error adding primary node device, display-only devices may not work: DRM access error: Error reading properties on device Some("/dev/dri/card0") (Invalid argument (os error 22))`. It is smithay's `add_props`/`map_props` walking the handles and calling `drmModeObjectGetProperties` on each. The encoder answers with a count of 0 now.
4. **And `VERSION` checked the pointer of a field it had nothing to copy into.** A caller that asked for the lengths and was told zero reserves no room, and hands the second call the pointer an empty `Vec` carries - non-null, and nowhere near a mapping. Linux's `copy_to_user` of zero bytes cannot fault; this card validated the address anyway and answered `EFAULT`, which is what stopped niri *after* it had found the connector and picked a mode: `Failed to query drm driver on device Some("/dev/dri/card0") (Bad address (os error 14))`. Zero bytes is not an address to check now - the same shape `get_resources` already carried for its own zero-length lists.

With all four, on the GL machine, niri's own words: `got render node: renderD128`, `initializing the primary renderer`, `new connector: Virtual-1 "PNP(MPO) MPOS Virtual 0x00000001"`, `picking mode: 1280x800 ... vrefresh: 60`, `putting output Virtual-1 at x=0 y=0`, `listening on Wayland socket: wayland-1`, `listening on X11 socket: :0` - and no `error adding primary node device` anywhere in it. What is left in that log is the guest's, not the card's: no `waybar` in the image, no `org.freedesktop.login1`/`locale1` on the bus, no default xcursor icon. `eglinfo -B` on the same boot answers `OpenGL core profile renderer: virgl (AMD Radeon 780M Graphics (radeonsi, phoenix, ACO, ...))` on the GBM platform - the host's own GPU, where every earlier boot answered `llvmpipe` under two `failed to create dri2 screen` lines - and `kmscube` renders on it for the whole of its 20s window.

**The 2D card gets three of the four and stops on the one it cannot fix.** On it niri now reads properties and loads the primary node, and then says `failed to initialize renderer, falling back to primary gpu: software EGL renderers are skipped` and `no allocator available for device` - which is a true statement about `llvmpipe`, not a card bug, and is why the GL card is what a compositor needs here. Everything except the renderer is now the same on both machines.

**The GL card is the shipped machine now, and the gate is the one boot that overrides it.** On it the sweep is green - `=== SUMMARY pass=170 fail=0`, `mpos-kunit 30 passed, 0 failed` - but QEMU then dies: SIGSEGV at `egl_cleanup` (QEMU's own `ui-opengl.so`) inside the **host's** `libgallium 26.2.4`, during `qemu_cleanup` *after* the guest wrote its debug exit (`coredumpctl gdb`: `#15 ... egl_cleanup (ui-opengl.so + 0x656f)` / `#16 qemu_display_cleanup` / `#17 qemu_cleanup`). A host Mesa teardown crash, not a refusal this kernel could answer, so the gate's insistence on exit 33 reads `-1` and `cargo xtask test` cannot be green on it. So the split is by *who is booting*: `machine.toml` ships the GL card, because a boot a human watches is the one that wants a renderer, and `selftest::boot_tests` overrides it back to the 2D card - the same place, and the same shape, as the vsock bus it already drops - with the reading above carried in the override and in the key's own comment. A run on this host gets `virgl (AMD Radeon 780M ...)` and niri; the sweep gets `-device virtio-gpu-pci -display none` and its exit 33.

Pins, each one held against the kernel that had the bug - a pin that cannot go red is not a pin: `t-drm` for the `VERSION` triple *and* for the zero-room pointer (`got -14, want 0` against the old kernel), `t-virtgpu` for "the resource id **is** the handle" - for a resource a client made and for the one a dumb buffer gets backed with late, the two paths that used to disagree - and `t-drm-props` for the encoder (`got -22, want 0`). One line the GL boot still prints that nobody needs to chase: the suite's own 3D create asks for a cube-map target with `array_size` 1, which is not a legal cube map, and virglrenderer prints `vrend_resource_create, Illegal resource parameters, error: Cube map: unexpected array size 1` - and answers it anyway, so nothing fails. **That line is not noise, and the "nothing fails" was this section's mistake** - see the third measurement below: what it says is that a create the renderer *refused* was answered as success, so the guest holds a handle to a resource the device never made.

**Gap closed - "no picture" was one number a context had not been told about.** The four answers above let niri come up and take `Virtual-1`; it still painted nothing, and the only thing that said why was QEMU's stderr - above niri's own log, three lines of it, three times over:

    vrend_renderer_transfer_iov: context error reported 2 "mpos" Illegal resource 1
    context 2 failed to dispatch TRANSFER3D: 22
    vrend_decode_ctx_submit_cmd: context error reported 2 "mpos" Illegal command buffer 852011

`vrend` resolves a resource **for the context that names it** (`vrend_renderer_ctx_res_lookup`; the context struct's own comment calls its table "resource bounds to this context"), and here a context is one open file description. A command stream that names something its context has not been told about is *dropped* - the whole batch, 852011 bytes of it - over that one number; the batch in this log is the renderer's first, right after `initializing the primary renderer`. Linux keeps a file's context told about its resources at the two moments a file can come to hold one: when it creates one (`virtio_gpu_object_create`, which is `RESOURCE_CREATE_3D` whenever the card has 3D, whatever the target) and when it *opens* a handle to one another file made (`virtio_gpu_gem_object_open` - the only sender of `CTX_ATTACH_RESOURCE` in the host's own `virtio-gpu.ko`, with `virtio_gpu_gem_object_close` sending the mirror). **QEMU forwards exactly that pair and nothing else**: in `hw-display-virtio-gpu-gl.so`, `virgl_renderer_ctx_attach_resource` has one call site - the `CTX_ATTACH_RESOURCE` handler - and both create handlers call `virgl_renderer_resource_create` and return without attaching anything.

This card had one of those three ways wired: `Job::Create3d` attaches, and it is reached only when `target == 4`, the cube map. Mesa's GBM buffers are `PIPE_TEXTURE_2D`, so they took the 2D path - which attached nothing at all, and left the guest's own buffer nameable by *nobody*.

**Measured, because the guest cannot see it.** QEMU answers every submit `OK_NODATA` and discards what virglrenderer made of the stream (`virtio_gpu_virgl_process_cmd`'s submit arm calls `virgl_renderer_submit_cmd` and jumps to the response without reading its result), so the ioctl returns 0 for a batch the host threw away - and a `TRANSFER3D` the host drops leaves the guest's own pages exactly as they were. So `t-virtgpu` reads **pixels**: it creates a `target == 2` resource, maps its pages, writes a marker over them, and submits a one-command stream whose `TRANSFER3D` from the host copies the host's copy of the resource into those very pages. The marker gone is a command that ran; the marker still there is a batch that was dropped. Two claims, one question asked twice: the file that *made* the resource, and a second file that imported its descriptor (`PRIME_HANDLE_TO_FD` then `PRIME_FD_TO_HANDLE` - what a compositor does with a buffer it renders through, Mesa's winsys asking `RESOURCE_INFO` for the imported handle's id and then spelling that id in its own streams). On the kernel before this change **both were red**, each with its own `Illegal resource 2` in QEMU's stderr - including the file that made the resource, which is the statement that nobody could name it.

**The fix is the door every submission passes through.** `EXECBUFFER` resolves its `bo_handles` (it already did, for `ENOENT`, which is Linux's answer for a handle the file does not hold) and hands their resource ids to `attach_resources`, which tells the submitting context about the ones it has not been told about yet; the pairs sent are remembered per context, so only the first stream to name a resource pays a control-queue round trip. The 3D transfers (`TRANSFER_TO_HOST_3D`/`FROM_HOST_3D`, one resource and no handle list) make the same statement for their own request - and the pin's second claim is not a second mechanism: it is the same door answering a file that came by the resource as an imported handle rather than by creating it. Attaching there rather than at Linux's two doors is the bug's own shape: one of the three ways a resource reaches a client's hands was wired, and the door that cannot be missed is the one every submission goes through.

**And the release sends the mirror, which here is not optional.** Linux's `virtio_gpu_gem_object_close` tells the file's context to let go of the object; this card has to as well, for a reason Linux does not have: a resource id here *is* a handle slot, and `virtio_resource_create` claims the lowest free one - so the record "this context has been told about id N" would be a lie the moment a released id came back, and the context would send no attach for the resource that landed on N. `destroy_handle` clears the id out of every context that held it, in the same section that takes the resource out of the handle table, and pushes `CTX_DETACH_RESOURCE` for each of them *before* the unref that follows - push order, so the device never holds a binding to an object it has already let go of. `t-virtgpu`'s third claim is that reuse: create, name, release, create again on the released id, name it from the file whose context was told about the *old* one, and read the pixels.

After all three, the same niri boot that printed three `Illegal resource` lines prints **none** - renderer initialised, connector found, mode picked, `putting output Virtual-1 at x=0 y=0`, `wayland-1` and `X11 socket: :0` listening: the same words as before, with the batch that used to die under them intact. Pins: `t-virtgpu`'s three claims, red on the kernel that had the bug (each with QEMU's line beside it) and green on this one. The control (the file that *made* the resource) is deliberately one of them - "a pin that cannot go red is not a pin" cuts both ways here, because without it a red cross-file claim could not be told apart from a resource that does not exist.

**Gap closed — a framebuffer is the description's object, and it has no properties.** The fifth object this walk asked about turned out to be two facts, and the empty list above was the wrong half of both. Measured whole on a current host first (`.workbuddy/probe_drm_fb_owner.c` and `probe_fb_props.c`, Linux 7.2.11):

* **`GETRESOURCES`'s framebuffer list is the open file description's.** A child that made fb 464 read `count_fbs=1` and `ids=464` — its own — while the parent holding a second description read `count_fbs=0`; after the parent made fb 465 its own list read `1`/`465` and the child's stayed `464`. The reason is the kernel's own list: `drm_mode_getresources` walks `file_priv->fbs`, while the crtc/connector/encoder lists are the card's (with the lease filter). This card had walked `st.fbs` device-wide.
* **An fb has no property list, so `OBJ_GETPROPERTIES` on one is `EINVAL`.** The description's own fb, another description's fb, and the same id under `OBJECT_ANY` all answered `errno 22` on the host. A framebuffer's `base.properties` is null, and the mode-object door refuses an object without one. The `width`, `height`, `format` and `modifier` this section once called fb properties are `struct drm_framebuffer` fields; `GETFB2` is the request that reports them.

Served by `get_resources` filtering `st.fbs` on `Framebuffer::owner` — the open file description `ADDFB`/`ADDFB2` already record, and the release path already prunes, so the visibility and the lifetime are one fact — and by deleting the `(OBJECT_FB, _)` arm from `obj_get_properties`, which leaves the `_ => None` that answers `EINVAL`. The niri failure the empty list worked around was the *first* fact, not the second: niri walked the ids `GETRESOURCES` gave it and found **kmscube's** fb 100, an object it may not name; on Linux that id is not in its list at all. The empty list had kept the walk alive by answering for an object that should never have been handed over. Pin: `t-drm`'s four claims — the list answers, the description sees the fb it made, a second description does not, and `OBJ_GETPROPERTIES` on an fb is `EINVAL` — each red on the rule it pins (`docs/uapi/` §5.1 item 68).

**And what is left of niri is niri's own: it commits a framebuffer it never drew into.** Every kernel-side answer it needs is now right — it loads its card, reads properties, picks a mode, creates resources, commits and flips, and the kernel scans out *its own* resource (`virtio-gpu: scanning out the compositor's resource 4`) — and the screen is still black because **that resource has no pixels in it**: `.workbuddy/probe-fb-pixels` reads its device copy back through a `TRANSFER3D` and counts zero of 4096000 bytes, in every run. What the instrumented boot then shows, in order of how it was found:

- **Its renderer is on `card0`, not the render node it asked for.** niri's own words are `using as the render node: "/dev/dri/renderD128"`, and smithay's are `Initializing OpenGL ES Renderer native="/dev/dri/card0" platform="PLATFORM_GBM_KHR"` with `GL Renderer: "virgl (AMD Radeon 780M …)"` — a choice on smithay's side (it resolved the render node and then built the renderer on the primary device), and one that still renders through the host's GPU.
- **Its submissions name the wrong resources.** With a per-submission line in `virtgpu_execbuffer` (`ofd`, the resource ids its `bo_handles` named, and the framebuffer the CRTC was on), its only large batch — 11736 bytes, before the CRTC was bound — names resources **1 and 2**, and later ones name 3/5/6; the framebuffer it commits (id 100, resource 4, a client resource the card's own predicate agrees is device-rendered) is named by **one 56-byte `TRANSFER3D` that reads it back from the host** and by nothing that writes to it.
- **And it is not waiting for anything the kernel withholds.** It asks for no flip *event* (the kernel queues none while it runs), and its `debug { wait-for-frame-completion-before-queueing false }` — tried in a config directory of niri's own, the image's config untouched — changes nothing: the buffer is still empty.

So the last step is *niri's* side of the device rather than this kernel's, and the proof is that the same kernel shows **kmscube**'s frame on the same card through the same renderer: a client that renders into a client resource and has the kernel scan that resource out is *exactly* what kmscube does, and its cube is visible. Two named leads for whoever picks it up: the renderer's device (card0 rather than `renderD128`), and whatever smithay does between `next_buffer()` and `commit` on a device whose buffers are all the same card's.

**And one thing seen once and deliberately not chased: `weston` started while `niri` holds the card takes a `SIGSEGV` inside a helper of weston's own.** Two compositors, one card — weston alone brings its desktop up, and after niri it dies in `/usr/libexec/weston-desktop-shell` (or `-keyboard`) with a userspace fault at address `0x28`, the instruction at `rip` being `mov 0x28(%r9), %rax` with `r9` null and page 0 unmapped: a null field in *its* code, not a page the kernel handed out. Recorded with the fault's own words rather than explained — nothing in this pass showed the kernel answering that sequence wrong, and chasing a second compositor's error path is its own piece of work.

**What this pass found that is not this kernel's to fix: a create the renderer refuses is answered as success.** The suite's own 3D create is `target == 4` with `array_size == 1` - a cube map with one face, which vrend refuses (`vrend_resource_create, Illegal resource parameters, error: Cube map: unexpected array size 1`) - and QEMU answers it `OK_NODATA` anyway (its create handlers ignore `virgl_renderer_resource_create`'s result), so the guest holds a handle to a resource the device never made. It is the same *false success* class as the four answers above, one layer lower: this kernel cannot see the refusal, and a caller that named such a resource would get an `Illegal resource` line for a resource the create told it it had. Nothing names that one, which is why the line sat here read as noise; the swallow is QEMU's, and what a guest can do about it is witness an *effect* - which is why the pin above is written in pixels.

**Gap closed - and the frame itself, which took three answers and two mistakes of my own.** With the batches surviving, niri came up, took `Virtual-1`, listened on both sockets - and the window stayed black. What followed was measured at each step rather than reasoned about, and every one of the three real causes was somewhere other than where the symptom was.

**First: the frame was never the guest's to copy.** `.workbuddy/probe-fb-pixels` walks the card's own doors (`GETRESOURCES` -> `GETFB` -> `VIRTGPU_MAP` -> `mmap`) and counts what a framebuffer has in guest memory. Before niri, `count_fbs=0` (the kernel's scanout is not a framebuffer - the control); while niri was drawing into its committed 1280x800 framebuffer, that buffer's **4096000 bytes were all zero**. They were never written: Mesa's virgl winsys draws into the device's own copy of the resource (its texture) and its export path hands the guest a handle and *no readback* (`virgl_drm_winsys_resource_get_handle` - flink, KMS handle or PRIME fd, and nothing else). This kernel's presentation was `copy_to_scanout`: the framebuffer's **guest pages copied onto its own scanout resource**, which for every compositor on this card carried black. Linux never copies - `virtio_gpu_cmd_set_scanout` names the framebuffer's own `hw_res_handle` and `virtio_gpu_cmd_resource_flush` repaints it - so now `drivers/drm.rs`'s `client_scanout` answers "whose frame belongs on the screen", the render task presents it with `Gpu::scan_out_client` (SET_SCANOUT + RESOURCE_FLUSH of that resource, **no transfer**: the device already has the pixels), and `flush` gained the hand-back that points the scanout at `RESOURCE_FB` again when the kernel's own frame is the one going up (a compositor's exit leaves the console its screen instead of its last frame). The predicate is the resource's **origin**, not its target or format: `Resource::host_pixels`, true for a resource a client made through this card's 3D-ioctl family on a card that negotiated 3D (the driver behind that call is Mesa's virgl, which draws into the device's copy), false for a dumb buffer (a software rasteriser maps it and stores into the pages) and for an imported dma-buf. `copy_to_scanout` keeps its `fb_fits` check and answers "accepted" without copying for a device-held frame, so every caller that used its answer to accept a flip still does.

**Second: nothing was being drawn at all, and the create command was why.** kmscube - the smallest possible client, one resource and a cube - printed its EGL/GL banner and then, in among its frame counter:

    vrend_check_no_error: context error reported 1 "mpos" Unknown 1282
    context 1 failed to dispatch DRAW_VBO: 22

`1282` is `GL_INVALID_OPERATION` on the *host*, raised by vrend's own use of a resource the guest had described differently - and vrend reports it against the *next* batch it decodes, which is why the line names a command that is not the one at fault (786440 bytes of draws dropped, and nothing on the screen to show for it). The description was wrong because of a rule this card had invented: `three_d = target == 4`, i.e. only a cube map was made with `RESOURCE_CREATE_3D`, and everything Mesa asks for is `target == 2` (or 0). `virtio_gpu_resource_create_2d` is `resource_id`, `format`, `width`, `height` and nothing else, so Mesa's `bind` (render target, depth attachment, vertex buffer), `array_size`, `last_level`, `nr_samples` and `flags` had nowhere to go. Linux's rule is not the target: `virtio_gpu_object_create` picks `virtio_gpu_cmd_resource_create_3d` whenever `has_virgl_3d`. With `three_d = card().virgl` the same kmscube reads, from the guest's side of the device, `device_pixels_nonzero=0` before and `4020654` after (with byte values in gradients - a frame drawn, not a buffer cleared), and the three vrend lines are gone. It also puts the client's transfers on the 3D command, which is Linux's other half of the same rule and the reason the attach fix above is load-bearing for them.

**Third: my own predicate asked for a master, and this card's rule is that an unbound card serves its holder.** With both of those in, niri's own boot still showed no `scanning out` line, so the present step was still choosing the kernel's framebuffer - and a temporary `[dbg]` line in that step said exactly what it was choosing from:

    [dbg] present: painted=false resent=false pending=true client=None master=false scanout=(100, 1280, 800, true, true)

`host_pixels` was true, the framebuffer was there, a flush was pending - and `master=false`. Requiring a master was wrong for the reason `master_ok`'s own comment gives: a presentation request from an *unbound* card is served, "which is what lets a client's first `SETCRTC` land before its seat manager's `SET_MASTER`" - kmscube never calls it at all, and its requests were being accepted all along (`pending=true` is the receipt). What keeps a *lost* master off the screen is not that condition either: its presentation requests are refused from then on, and the console it lost the screen to paints - and a console that painted wins the present step. With the condition gone the same line reads `client=Some((1, 1280, 800))` and the boot prints, twice, `virtio-gpu: scanning out the compositor's resource 1` / `... 4` - kmscube's two double-buffered framebuffers, the frames the device holds, on the display.

**One thing this work paid for that is not about the picture: the console's own input.** Every measurement above was driven by typing one short line into the guest's serial console, and *two of those lines arrived as their first seven characters* - `sh /mnt/share/pin.sh` became `sh /mnt` twice, and the run then did nothing at all while the harness waited for a marker that could not come. What was measured on the way is the shape of it: the same line sent **in one write** arrives whole and runs, while the same line sent two characters at a time with a pause between (a pace *slower* than a person's) is truncated at the same place. A console that drops bytes for being typed slowly is a console bug rather than a probe bug, and it is not this section's: it belongs to the serial line's receive path (§7.1's console work) and is recorded here because this is where it was found, with both logs (`.workbuddy/pin-*-session.log`) as the measurement.

What is *not* measured here is still the picture itself: that is the window's, and it is the one step a headless run cannot take. What a headless run *can* say - and now does - is that the device holds a drawn frame for the client, that the scanout names that client's own resource, and that no batch is dropped on the way.

**Gap closed - a second compositor, and the tag that means "the id is what I am naming".** Measured 2026-10-14 on the GL machine, from the guest's own console: `hyprland --i-am-really-stupid` (Hyprland v0.54.3, Aquamarine 0.12.0, apk-installed in the guest) reached `Creating the AsyncResourceGatherer!` and died with `terminate called after throwing an instance of 'std::runtime_error'  what():  CBackend::create() failed!` - and it had written its own log to the guest's root (`/root/.cache/hyprland/hyprlandCrashReport1.txt`, read back out of the image with `qemu-nbd` + `debugfs`, the same instrument as the work above). That log says where, one layer below the message:

    drm: Atomic supported, using atomic for modesetting
    drm: found 1 CRTCs
    drm: found 2 planes
    ERR ... drm: aqPlane->init for plane 36 failed
    ERR ... drm: Failed initializing resources
    ERR ... DRM Backend failed
    ERR ... Wayland backend cannot start: wl_display_connect failed (is a wayland compositor running?)
    CRIT ... Cannot open backend: no allocator available

`36` is this card's primary plane, and it failed in `SDRMPlane::init` - which reads a plane's properties **twice**: once by kind (`getDRMPlaneProps`, `DRM_MODE_OBJECT_PLANE`) and then, for the `type` value it must know before it can call a plane the primary one, through `getDRMProp` - and `getDRMProp` is `drmModeObjectGetProperties(fd, obj, **DRM_MODE_OBJECT_ANY**)`. That tag is Linux saying "the **id** is what I am naming, not the kind": `__drm_mode_object_find` resolves the id in the card's one object table and compares the type *only when the caller named one*. This card matched on the pair, so the tag fell through to the same `EINVAL` a wrong kind gets, `getDRMProp` answered false, and the plane's init returned before it had read a single format - with no line of its own to say so, which is why the failure looked like it was about plane 36's existence rather than about a read of it. The tag is not an Aquamarine quirk: it is how a client reads a property's *value* rather than its shape.

The fix is that resolution, in both object-property doors (`obj_get_properties`, `obj_set_property`): `object_kind(st, id)` answers what an id *is* - the four fixed ids, plus the framebuffers whose ids are the caller's own to allocate - and an id nothing holds is still `EINVAL`, so the tag is a lookup and not a licence.

Pins (`t-drm-props`), each held against the kernel that had the bug: every object read through both doors and compared (`the CRTC/connector/encoder/primary plane/cursor plane answers DRM_MODE_OBJECT_ANY, with its own kind's list`), the `type` **value** read the way Aquamarine reads it, and the two refusals that keep the tag honest - an id no object holds, and a plane's id under the connector's kind. On the kernel before the change **six of the eight were red** (`FAIL: the CRTC answers DRM_MODE_OBJECT_ANY, with its own kind's list`, and its four siblings plus the `type` value; serial log `.workbuddy/serial-prefix.log`), and the two refusals were green on both - which is the control: what was missing was a resolution, not a door.

**Gap closed - a framebuffer holds its buffer, so a compositor may release the handle it bound.** Measured 2026-10-14 on the GL machine, with Hyprland past the plane-property gap above and its commits still refused. Its own console said only that something was refused, four times:

    mpos-dbg: EINVAL from fb_fits_at fb=100 fit=1280x800 ...

and the reason is not in that line, because three separate conditions refuse a framebuffer there. So the line was made to carry the judgement whole, and the run that read it back was driven the way the earlier rounds are: `cargo xtask run --mode terminal`, the script a **file the guest reads off the 9p share** (`sh /mnt/share/hypr.sh`), the answers written to the guest's own root with a `sync` behind each step, and read back with `qemu-nbd --read-only` + `debugfs`. One instrument was fixed on the way and it is the reason the first attempt found nothing: the harness **read the console throughout** the compositor's life, because the previous round left the pty unread through a plain `sleep` while the compositor was printing - the diagnosis *is* the kernel's own lines, and they arrive in exactly that window.

What the line then said, in one refusal:

    fb=100 fit=1280x800 programmed=1280x800 advertised=(1280, 800) mode_blob=Some(1)
    test_only=true fb=(handle,width,height,pitch,bpp,format)=Some((11, 1280, 800, 5120, 32, 875713112))
    handle_live=false

The framebuffer is **exactly the mode's size** - 1280x800 at 32 bpp, pitch 5120, `XR24` (`875713112` is the fourcc in the uapi's `('X'<<0|'R'<<8|'2'<<16|'4'<<24)` spelling) - so the geometry is not the problem and neither is the format. The only failing condition is `handle_live=false`: **the handle the framebuffer was made from no longer names a buffer.** `test_only=true` three times and `false` once is Aquamarine's own sequence (a test commit, then the real one), both refused; the guest's own probe agreed from the other side (`probe-fb-pixels`: `fb[0] id=100: 1280x800 handle=11: MAP refuses: -2`, while the same probe had read niri's live `handle=4` in the runs above).

Why the handle was gone is a fact about the idiom, not about Hyprland: Mesa's GBM allocator and Aquamarine make a scanout buffer, bind it with `ADDFB2`, and then **release the handle** - the framebuffer is all the kernel needs to flip it. Linux's `drm_framebuffer_init` takes a reference on the GEM object for exactly that reason: the file's handle is one holder of the object and the framebuffer is another, and `drm_gem_handle_delete` drops only the handle. This card's framebuffer was a *record of a handle number*, and `destroy_handle` took the buffer out of the table with the name - so the id stayed listed by `GETRESOURCES` while every commit naming it was refused, which is a compositor with no picture and nothing to read. niri escaped it only because Mesa's virgl winsys keeps its handle open, which is why the same kernel has been showing niri's and kmscube's frames all along.

The fix makes the two questions different ones. The handle is the client's **name**; the buffer is the object. `Dumb` grows `fb_refs` (one per framebuffer, taken in `insert_fb` and given back by `drop_fb_hold` - on `RMFB` and on the description closing, the two doors a framebuffer leaves by) and `handle_closed` (the name is gone, the buffer is not). Every request that takes a *handle* now asks `handle_names_buffer` and refuses a released name - `MAP`, `WAIT`, `RESOURCE_INFO`, both transfers, a submit's `bo_handles`, `PRIME_HANDLE_TO_FD` - which is Linux's answer for a handle its file no longer has, while the framebuffer keeps reading the buffer straight out of the table. The device's teardown (`CTX_DETACH` mirror, then `Unref`) moved with the buffer to its **last** holder, through `take_buffer`/`tell_buffer_gone`, because a resource the host is still scanning out must not be unref'd while the name goes - and the host cursor's own teardown is deliberately *not* taken down by the release: the binding belongs to the framebuffer, which is still there, and its own release is what takes it down.

Pins (`t-drm`), each held against the kernel that had the bug - and the claims run **before** the magic handshake, which is itself a measurement (see below): a dumb buffer and a `RESOURCE_CREATE`d one are both bound, their handles released, and then the framebuffer is *driven* (`the framebuffer is still driven - the buffer outlived its name`, `and it still flips` and its completion, `the resource-backed framebuffer drives as well`); the name being gone is the other half (`GEM_CLOSE refuses the name it has already released`, `VIRTGPU_MAP refuses the released name`); and the hold is visible through the one request that can see it, because `RESOURCE_CREATE` lands on the **lowest free handle** (`and it does not take the name the framebuffer is holding`, and then, after `RMFB`, `and it lands on the name the framebuffer was holding`). On the kernel before the change **five of them were red** - every drive claim with `(got -22, want 0)`, and both slot claims (`got 3, want 2`) - while the controls stayed green on both: `GETFB still answers for it`, the released name refused twice, and no map for it (serial log `.workbuddy/serial-held-reverse.log`). With the fix: 170/170, kunit 30/30, exit 33.

**And one thing the new claims measured by landing in the wrong place, recorded rather than chased.** Written where they were first written - after the file's `SET_MASTER` - every *presentation* claim came back `EACCES`, while `GETFB`, `GEM_CLOSE`, `CREATE_DUMB` and the whole `VIRTGPU_*` family were served: the card had been **revoked**, and the branch is `master_ok`'s second one (`!vcmux::holds_screen(vc)` → `revoke()`), not a missing authorisation - `is_authorized` already accepts the master itself (`is_master(ofd) || AUTH_OFD.contains(ofd)`). In a headless sweep there is no virtual console on the screen, so a holder that takes the master with `SET_MASTER` locks *itself* out of presenting; on Linux the master is allowed by `drm_is_current_master` regardless. Nothing on the compositor path hits it (Hyprland's commits reached the framebuffer check, i.e. past this gate), and no claim in this file presented after `SET_MASTER` before now - so it is named here with the measurement and left alone rather than changed without one. The claims moved above the handshake, where an unbound card serves its holder, which is the state the rest of the file's KMS claims already relied on.

**Gap closed - `GETCRTC` reports the mode the scanout drives, and the difference is what makes a compositor modeset at all.** Measured 2026-10-14 on the GL machine, on the guest's own console, with the handle gap above fixed and Hyprland still refusing to appear:

    virtio-gpu: host wants 1910x1135; still driving 1280x800 until a compositor commits it
    mpos-dbg: EINVAL from fb_fits_at fb=100 fit=1280x800 programmed=1280x800
              advertised=(1910, 1135) mode_blob=None test_only=true
              fb=(handle,width,height,pitch,bpp,format)=Some((11, 1910, 1135, 7640, 32, 875713112))
              handle_live=true

Three things changed from the round before, and each one closes a door on a guess. `handle_live=true` says the framebuffer-holds-its-buffer fix holds. The framebuffer is now **1910x1135**, the size the host is asking for, so the buffer is not stale. And `mode_blob=None` is the fact the whole reading turns on: a first enable of an output carries the CRTC's `MODE_ID` - Aquamarine computes `data.modeset` from the committed state and a modeset is what puts the blob in the request - while a **page flip does not**. Four `test_only=true` then the real ones, all without a mode, is a compositor that believes the output is already configured at 1910x1135 and is only pushing frames through it; and each frame was refused because the scanout was still programmed at 1280x800.

Why it believed that is in Aquamarine, and the source says it plainly. `SDRMConnector::connect` takes the CRTC's current mode from `getCurrentMode()`, which reads the CRTC's `MODE_ID` **property value** and falls back to `drmModeGetCrtc` when that value is unset - `GETCRTC` on this card - and then walks `connector->modes`, calling `output->state->setMode(aqMode)` **only for a mode that `memcmp`s different** from it. This card listed one mode, the host's new size, and answered `GETCRTC` with that same size, so nothing differed and `setMode` was never called: the one commit that would have driven the scanout to 1910x1135 was the commit the comparison suppressed. The compositor locked itself out through a lie in a *read*.

The lie was in `get_crtc` alone - it answered `advertised_mode_dims()` where every other use of that function is about what the connector *offers* (`GETCONNECTOR`'s mode list, the `EDID`, `mode_fits`). A CRTC reports what it is driving; the two sizes are the same number except in the window between the host asking for a new one and a compositor committing it, which is the window `resize_to`'s own comment already described - the code just did not keep the promise in the one door that answers "what mode is this CRTC at". `get_crtc` now answers `programmed_mode_dims()`, the scanout's own geometry, so the offered mode and the driven mode differ exactly when they really do differ, and a compositor that wants the new size says so. Nothing else moved: `mode_fits` accepts both sizes, which is what lets a commit that keeps the geometry pass as before.

**The pin is a boot self-test, and it is the only shape that can carry this claim.** No user program can reach the state: `desired` - the size the host is asking for - has exactly one writer, the display event, and no ioctl spells it. Nor can the gate's own boot reach it: the two sizes are read from `GET_DISPLAY_INFO` in the same call at boot (`VirtioGpuDev::install`), so they start equal, and they come apart only when a **host window** changes shape underneath a running machine - which the sweep and every probe in this file have no way to do, because a test boot is windowless (`-display none`) and no event is ever raised. So the kernel carries it: `src/drivers/drm/kunit.rs`, a suite whose case writes the asked-for size into the field the display event writes, calls the door, and asks which size came back. It needed one thing of the harness - a phase that says "the display card is open and its render task is running", because the card's owner is a *task* and the card is therefore up a step after `Phase::Vfs` rather than with the other devices - and `Phase::Display` is that step, fired in `flow.rs` where the card is spawned, whether or not a card was found (a machine with no card reaches the same step, and its case reports rather than passing silently, which is the shape the block suite already uses for a clock that cannot see a masked window).

Held against the kernel that had the bug: the switch to `advertised_mode_dims()` in that one line makes the case report **`GETCRTC reported 1910x1135: the scanout is driving 1280x800 while the host is asking for 1910x1135`** and the boot's tally go from `30 passed, 0 failed` to `30 passed, 1 failed` - with all 170 user tests still green, so the red is this case and nothing else (serial log `.workbuddy/serial-crtc-reverse.log`). With the fix: 170/170, kunit 31/31, exit 33.

**Left unmeasured, and named so the next round starts here: the vrend object errors in that same run.** After the refusals, the same console carried

    got error from kernel - expect bad rendering 2
    Dropping rendering due to missing shaders: mpos
    vrend_set_single_sampler_view: context error reported 2 "mpos" Illegal handle 52
    vrend_object_bind_blend: context error reported 2 "mpos" Illegal handle 46
    context 2 failed to dispatch DRAW_VBO: 131

which is vrend failing to find *objects* - sampler views, blend states, shaders - in the context that is drawing. Their shape says the objects were never created rather than created and dropped: a create is a `CREATE_OBJECT` command inside a submitted stream, and vrend abandons the **rest of a batch** when it hits a command it cannot dispatch, so one un-attachable id early in a stream would take every object created later in that same stream with it. What is *not* yet measured is which of the two it is - a batch vrend refused, or a create that never reached it - and the reading that would settle it does not need a compositor: a submit of a hand-built stream, one `CREATE_OBJECT` and then a draw that names it, under the same context the compositor uses. It is left unmeasured because the run above cannot separate the two, and because a compositor whose commits are accepted may not walk this path at all - the errors arrived *after* the first refusal, in the stretch where Aquamarine was already reporting a broken render, so the next run on this kernel is what decides whether this family survives the fix.



**A handle is a name the *file* holds, and that is what kept the compositor black.**

The vrend object errors the round above left unmeasured were not a second bug and
were not about objects being dropped. That line is **Mesa's** - not Hyprland's and
not Aquamarine's: `strings libgallium-26.2.4.so`, dumped out of the guest's own
`/usr/lib` (44 MB, one hit), is where it lives, and it sits in the virgl winsys'
submit path beside `failure to add relocation` and `waiting got error - %d, slow
gpu or hang?`. The number it appends is the errno of the `DRM_VIRTGPU_EXECBUFFER`
that failed, and the errno was `2` - **`ENOENT`**. Mesa marks the winsys failed on
it and drops all rendering, which is the black screen on *both* compositors and
which is why nothing about it moved when the mode and framebuffer work landed.

The card's only `ENOENT` on that road is the `bo_handles` loop in
`virtgpu_execbuffer`, and it answers one number for three states. So the loop now
names the state (this round's `mpos-dbg: EXECBUFFER refused: handle {h} - {why}`,
and it carries the open file description), and one boot answered it:

    EXECBUFFER refused: handle 38 - the name was released and a framebuffer is holding the buffer (num_handles=10, bytes=5176, ofd=...3914384)
    EXECBUFFER refused: handle 39 - the name was released and a framebuffer is holding the buffer (num_handles=10, bytes=5176, ofd=...3914384)

The PRIME traces from that same boot are the whole story, and there are **three
file descriptions** in them:

    PRIME export <- handle 9   (ofd=...21702800, ino=37632)
    PRIME import -> handle 39  (ofd=...3914384,  ino=37632, reused=false)
    PRIME import -> handle 39  (ofd=...3912848,  ino=37632, reused=true)
    name 39 released           (ofd=...3912848)
    EXECBUFFER ... handle 39   (ofd=...3914384)    <- refused ENOENT

**Linux's handle table is per `drm_file`; this card's was per card.** The import
of a dma-buf this card had exported answered the *exporting* handle (the `prime`
records are keyed by `ino` and the dedup is global), so a second file came away
naming the very number a first file was already using - and then that second file
did the thing Linux tells clients to do: `ADDFB` the buffer, `GEM_CLOSE` the
handle, and keep flipping the framebuffer. On Linux that takes the *caller's* name
away and nothing else (`drm_gem_handle_delete` drops one handle out of one
`drm_file`; the object lives while a framebuffer or another file's handle
references it). Here one flag said "released" for the whole card, so the first
file's own name died with it, and every batch it submitted came back `ENOENT`.
The import also left the buffer with no device resource of its own (`resource:
None`), so the alias was the only thing that had made it nameable at all - which
is why the second file's import was `reused=true` and had to *stay* that way.

The fix is the fact, not a flag: `Dumb.handle_closed: bool` became
`Dumb.holders: Vec<u64>` - one entry per open file description that names the
buffer - written by the create or import that hands a handle out
(`Dumb::named_by`), removed by `GEM_CLOSE` for the caller alone, and freeing the
buffer only when it is the last name *and* no framebuffer holds it. An import that
finds the `ino` already known now adds the importing file's own name to the object
instead of handing back somebody else's, and the `prime` records are dropped with
the **object** (`take_buffer`) rather than with the name, so a later import still
finds the buffer after every name is gone - which is what a dma-buf descriptor
promises on Linux.

**The pin lives in `t-prime`, and where it lives is part of the measurement.**
The claim was first written into `t-virtgpu`'s second-file block - the same shape,
and the file whose subject it is - and the gate went green **on the kernel with
the bug still in it**. The reason is in that program's own output: the sweep's
card is a `virtio-gpu-pci`, its `3D_FEATURES` is 0, and the whole second-file
region sits in the 3D tail that a card without virgl never reaches (`ok: EXECBUFFER
is ENOSYS - there is no context to submit to` and then straight to `PASS`). The
claim has to run on either card, so it went to `t-prime`, which walks the same
PRIME road on a 2D device: a second file opens `renderD128`, imports the
descriptor, and gives its name back. Held against the kernel that had the bug, the
boot reports

    ok:    a second file imports the same descriptor
    ok:    and gives back the name it was given
    FAIL:  while this file's own name still stands (got -2, res 0, want 9)

- the two `ok:` lines are the control, green on both kernels, and the red is the
fix's own behaviour - with `=== FAIL t-prime` (169 of 170) and the kernel's own
suites still 31/31. With the fix: `ok: while this file's own name still stands
(got 0, res 9, want 9)`, **170/170, kunit 31/31, exit 33**.

And the compositor is the end-to-end reading, on a headless GL machine with the
fixed kernel: `got error from kernel - expect bad rendering` appears **zero**
times, there is no vrend error of any kind, and the framebuffers the compositor
made hold real pixels in their device copies

    fb[0] id=100: 1280x800 handle=10 device_pixels_nonzero=4096000 first=Some(0) some_bytes=[17,21,34,37,53,108,181,255]
    fb[3] id=103: 1280x800 handle=11 device_pixels_nonzero=4089188 ...
    fb[4] id=104: 1280x800 handle=9  device_pixels_nonzero=4096000 ...

where the round before had `MAP refuses: -2` and `guest_pages_nonzero=0` on all of
them - and the presentation door is carrying them, 154 times each:

    virtio-gpu: scanning out the compositor's resource 9 / 10 / 11

**Left unmeasured, and named so the next round starts here.** A gate still asks
"does *some* file hold this name" (`!holders.is_empty()`) rather than "does *this*
file", because only `GEM_CLOSE`, the import and the export have the open file
description in hand; a file that released its own name can therefore still use the
number while another file holds it, where Linux would answer `ENOENT` from its own
handle table. No client does that - a client that released a handle does not use it
again, and the compositor's whole idiom is the opposite - and the `ok:` control
above is what the difference looks like from the outside. `handle_names_buffer` has
two callers, so making it per-file is a small change when a reading wants it; it is
recorded here rather than guessed at.

## The desktop boot's black screen after a host resize - the card answered a size it was not driving

Measured 2026-10-14 on the first machine's GL card, a window opened at 952x1135
(that is the size the window manager gave it) against a card booted at 1280x800.
The kernel had been narrating the whole failure once per frame, and the line reads
like a size complaint:

    mpos-dbg: EINVAL from fb_fits_at fb=100 fit=1280x800 programmed=1280x800 advertised=(952, 1135) mode_blob=None test_only=false fb=(handle,width,height,pitch,bpp,format)=Some((10, 952, 1135, 3808, 32, 875713112)) handle_live=true     x461

Three things in it are load-bearing, and each is a measurement. The framebuffer is
**952x1135** - the size the host asked for, not a stale one. It is refused for not
fitting **1280x800**, and `programmed` says the card believes it drives 1280x800.
And the same boot's own log says what the device is *actually* pointed at:

    virtio-gpu: scanning out the compositor's resource 10

so the screen *was* at 952x1135 - the compositor's own resource. The card had two
answers to "what size is the screen", and the wrong one won.

Where the two answers live: `dims()` reads the `Scanout` struct, which only
`resized()` writes - and `resized()` is called from the **resize** path
(`render.rs::resize_to`) alone. A compositor's resource is presented through
`render.rs`'s present step, which calls `Gpu::scan_out_client(resource, width,
height)`: the size is in hand there and was never recorded. `programmed_mode_dims()`,
`GETCRTC` and `fb_fits_at`'s ceiling are all built from `dims()`, so all three went
on answering the private framebuffer's geometry while the device scanned out a
bigger resource.

What made that fatal is the compositor's own startup, and it is the reading §二
turned on: `SDRMConnector::connect` takes `getCurrentMode()` - the CRTC's `MODE_ID`
blob if it is set, `GETCRTC` otherwise - and calls `setMode` **only for a mode whose
`drmModeModeInfo` `memcmp`s different** from it. A raw-ioctl probe run from the
guest's own console, with the compositor killed, printed both doors at once:

    GETCRTC: mode_valid=1 mode=1280x800 1280x800 clock=61440 fb_id=100
      MODE_ID = 1
      MODE_ID blob 1: 68 bytes, 952x1135 clock=64831 name=952x1135
    getCurrentMode takes the MODE_ID blob path (value 1)
    connect(): connector 952x1135 vs current 952x1135 -> memcmp EQUAL -> setMode NOT called

The compositor believed the output was already at 952x1135 - which, on the screen,
it was - so it sent no mode commit, and every frame it did send was judged against
1280x800 and refused. The probe could also do by hand what the compositor had not:
an atomic commit of `ACTIVE` + `MODE_ID` came back `rc=0` and moved `GETCRTC` to
952x1135, at which point the refusals stopped - the measurement that named the
field.

The fix is one invariant: **the size the card reports as driven is the size of the
resource it is presenting.** `VirtioGpuDev` keeps a `presented` field; the present
step sets it with `presenting_client(width, height)` when it points the device at a
client's resource, and clears it with `presenting_own_framebuffer()` when the screen
goes back to the driver's own buffer; `driven_dims()` returns it ahead of the
private framebuffer's geometry. `drivers::drm`'s `programmed_mode_dims()` answers
`driven_dims()`, so `GETCRTC` and the fit check follow it. `dims()` itself still
reports the private framebuffer - that is what `/dev/fb0`'s block and
`FBIOGET_VSCREENINFO` are about.

Counterfactual, both directions, same machine and same compositor: with the old
reading, **461** `fb_fits_at` refusals in one run and no picture; with the new one,
**0**, and the compositor's resources cycling in the scanout
(`scanning out the compositor's resource 10/11/9/10/...`). The probe agrees on the
new state: `GETCRTC: mode=952x1135` where it had said 1280x800.

The pin is in the boot self-test rather than in a `t-` program, and the reason is
the same one that put the `GETCRTC` case there: the *state* this is about - a
compositor's resource on the screen - is what a compositor's hold on the card
creates, and the render task consults `client_scanout()` only when the console is
not painting. A console-owned test boot therefore shows the driver's own
framebuffer, and `drm::kunit`'s `the_crtc_reports_the_resource_on_the_screen` puts
the field where the render task would put it and asks the door for the answer (half
the driven size, retried, because the render task repoints the scanout on its own
beat). A first draft put the claim in `t-drm` and the aarch64 sweep caught it:

    FAIL:  GETCRTC reports the size of the resource on the screen (width) (got 1280, want 96)
    FAIL:  ... and its height, not this driver's framebuffer's (got 800, want 96)

- which is the gate saying that a claim about a compositor's state cannot live in a
program that never has one. Recorded (with the gate) as `15-boot-flow.md` §15.39.

## A session's own two prerequisites, and what the GL card answers on it — the pixman probe the plan named is a dead end

Measured 2026-10-18 on this host, the first machine, `cargo xtask run --arch x86_64 --mode terminal`
with a root shell on the serial line, against the image baked 2026-10-17 00:59 — 25 commits behind
`HEAD`, and every card fact below is one that image carries: the whole GL-card thread this section
ends on (the devpath, the resource/handle and `VERSION` fixes) landed on 2026-10-13..15, before it.

**Two prerequisites stand between a session and a boot on a host in this state, and neither is
written down here yet.** The host's **running** kernel has no `nbd` — `uname -r` says
`7.2.9-arch1-1` while `/lib/modules` carries `7.2.11-arch1-1`'s — so `qemu-nbd` cannot be attached
and `run`'s privileged refresh stops early, verbatim: `modprobe: FATAL: Module nbd not found in
directory /lib/modules/7.2.9-arch1-1` / `Error: nbd is not registered in /proc/devices` /
`Error: The privileged part of the build failed`. And `qemu.vsock = true` makes the first machine's
wiring attach `vhost-vsock-pci,guest-cid=3` **unconditionally**, so QEMU refuses to start at all:
`Could not open '/dev/vhost-vsock': No such file or directory` (this host's node is a stale one
renamed aside; the *second* machine's arm asks before attaching — `qemu.rs`'s
`Path::new("/dev/vhost-vsock").exists()` — and `machine.toml`'s key documents itself as the switch
for the first). The session booted by touching the image's stamp and turning the vsock key off for
the length of it, and `xtask` said what the stamp move costs: `run: warning:
.../mpos-uefi-x86_64.img predates .../mpos — it boots the code from before that build, which will
look like a fix that changed nothing.` Neither prerequisite blocks `cargo xtask test`, whose payload
does not ride the installed image; what they block is a session with the Alpine root.

**What the card answers — the part the plan had wrong.** On this boot the GL card is Mesa's, not
llvmpipe's:

    virtio-gpu: 1280x800, 4000 KiB at /dev/fb0
    virtio-gpu: virgl=1 context_init=1 cursorq=1 capsets=2 cached=2 mask=0x6
    OpenGL core profile renderer: virgl (AMD Radeon 780M Graphics (radeonsi, phoenix, ACO, DR...))
    OpenGL core profile version: 4.3 (Core Profile) Mesa 26.2.4

and `weston` comes up whole on it — `Using GL renderer`, `cursor planes: yes`, `Output 'Virtual-1'
enabled with head(s) Virtual-1`, `Loading module '/usr/lib/weston/desktop-shell.so'`, both helpers
launched — where this section's earlier boots had `llvmpipe` under two `failed to create dri2
screen` lines. That `cursorq=1` is the token the pointer item wanted, and the card stops churning
once it is idle: the 39 `RESOURCE_CREATE` lines of its startup are all the session produced (0 more
over the next 20 s). The sysfs shape libudev reads is Linux's too, measured in the guest:
`/sys/class/drm/card0` resolves to `/sys/devices/pci0000:00/0000:00:04.0/drm/card0`, `card0/device`
to the function, `device/vendor` is `0x1af4` and `device/device` `0x1050`, `device/subsystem`
reaches `../../../bus/pci`, and `udevadm info -a -n /dev/dri/card0` walks up to
`SUBSYSTEMS=="pci" / DRIVERS=="virtio-pci"` carrying both ids — so the parent walk the devpath fix
was for really does resolve. `/sys/dev/char/226:0` also resolves to the card and its `device/drm/`
lists `card0` and `renderD128`, which is the directory libdrm's render-node lookup
(`drmGetRenderDeviceNameFromFd`) opens. **One attribute is worth a second look**: the function's
`enable` reads `0` where Linux's reads `1` for a device whose memory decoding is on.

**The plan's next step for the display path is a dead end, and this is the measurement that says
so.** The roadmap's item 4 read "boot a wlroots compositor on `WLR_RENDERER=pixman` and read what it
says; it is the cheapest way to find what the analysis has wrong". On this image it stops before it
renders anything:

    WLR_RENDERER=pixman sway
    00:00:00.041 [ERROR] [wlr] [render/allocator/allocator.c:168] Failed to create allocator
    00:00:00.041 [ERROR] [sway/server.c:306] Failed to create allocator

The image carries `wlroots0.20-0.20.2-r1`, and the *same binary with its default renderer* gets past
that line (to the different failure below) — so what refuses here is the pixman renderer: it is the
one with no allocator for `wlr_allocator_autocreate` to hand back. A probe that cannot reach the
renderer measures nothing about it.

**And a wlroots compositor still does not come up here, for a reason that is not the renderer.**
`sway 1.12` on the same boot, with the DRM backend and with `WLR_BACKENDS=headless` alike:

    00:00:00.128 [ERROR] [sway/ipc-server.c:106] Unable to bind IPC socket

The line carries no `: <strerror>` suffix — what `wlr_log_errno` appends when it has one — and the
sequence it stands for is `unlink` of a stale path then `bind` of
`$XDG_RUNTIME_DIR/sway-ipc.<uid>.<pid>.sock` (`/run/user/0` exists, root, mode 0700). So the next
measurement is that errno, from `strace -f /usr/bin/sway` — the image has `strace`, and the
`/root/*.log` files beside it are an earlier session's own. Each of the same runs also takes a
user-mode page fault at address 0 and *survives* it:

    user #PF at 0x4 code=PageFaultErrorCode(USER_MODE) rip=0x7e49c621b680
    PFDIAG page=0x0 NOT MAPPED task=18 rip=0x7e49c621b680 mapped=[0x0..0x1000) bytes=[44, 8b, 46, 04, 89, c2, 31, c0, 41, 39, c0, 7e, 17, 48, 8b, 4e]
    user #PF at 0x8c code=PageFaultErrorCode(USER_MODE) rip=0x7d54933974a4
    PFDIAG page=0x0 NOT MAPPED task=38 rip=0x7d54933974a4 mapped=[0x0..0x1000) bytes=[8b, 85, 8c, 00, 00, 00, 31, db, 85, c0, 0f, 89, e5, 00, 00, 00]

The bytes name the accesses — `mov r8d, [rsi+4]` with `rsi` 0, and `mov eax, [rbp+0x8c]` with `rbp`
0 — and `mapped=[0x0..0x1000)` is the empty run around the hole rather than a VMA at zero, so both
are plain NULL dereferences of a pointer some call answered 0 for. The tasks are still alive
afterwards (`pgrep` finds the same pids), which is this kernel's own path for a task that installed
a `SIGSEGV` handler — report, `send_signal(11)`, return — so which caller answered 0 is not yet
named. The two instruction sites are the lead, and they recur (a third run, `rip=0x7dc7095aa680`,
the same `0x4` access at a different ASLR base).

**The control channel is wired now, and what it answered** (2026-10-18, the same day: `xtask`'s
`qemu.rs` binds `build/<arch>-9p-share/qmp.sock`, `server=on,wait=off`, the console socket's shape).
`info mice` names the machine's pointers — `Mouse #2: QEMU PS/2 Mouse` and `* Mouse #4: QEMU Virtio
Tablet (absolute)`, the tablet being the *active* one — and `input-send-event` moves it: two `abs`
events per position (this QEMU's schema wants `{"axis": "x"|"y", "value": …}`, one axis per event)
are answered `{}`. So *driving* a compositor's pointer from a session is solved.

**Reading it back is not, and the obstacle is QEMU's.** `screendump` answers
`GenericError: no surface` on *both* display backends this tree wires for the GL card —
`egl-headless` (terminal mode) and `sdl,gl=on` (mirror) — because a GL-accelerated scanout lives in
a texture QEMU's dump path does not expose. A readable surface needs a **non-GL** display: the 2D
card (`sdl` without `gl`), VNC, or a host-side capture of the window. Which of those also exercises
the cursor queue is a *question and not an answer*: weston on the 2D card renders with llvmpipe
(measured, §7.4's earlier rounds), but whether its pointer takes that card's cursor queue or the
software blend is what the card's own `cursorq` token would say — and the quotes above elide that
token for the 2D card, which is the first thing a 2D-card boot should read.
 Two session facts beside it: an SDL window **grabs the session's pointer** (its title
becomes `QEMU - Press Ctrl-Alt-G to exit grab`), so a mirror/gui session takes the operator's mouse
until the window loses focus — which is why `terminal` mode stayed the driveable one — and the
window's own size is the host's, narrated by this kernel as `host wants 952x1135; still driving
1280x800 until a compositor commits it`, which is the resize item above working.

**And one gap the same boot walked into — closed 2026-10-18.** udev maintains `/dev/dri/by-path` by
removing the directory and publishing its links again, and the removal was refused here:
`mpos: syscall 84 (path /dev/dri/by-path) -> EOPNOTSUPP` (84 is `rmdir`). `devfs` served the
`mkdir` (its `create` accepted a directory) while the `remove` it inherited was the trait's default,
so only the second half of the idiom was missing — and the same door split was missing on `unlink`,
which took a directory as readily as a device node. Both nodes own `rmdir` now and `unlink` answers
`EISDIR` (`docs/uapi/` §5.1 item 48, `t-devtmpfs`); a boot of the same image prints no `mpos: syscall`
line at all and `/dev/dri/by-path` holds its two links, so the pass that makes that directory and
then removes it again runs whole.

### R.9 Two concurrent `exec`s deadlock the parked-transfer path — **observed, not diagnosed** (2026-10-25)

Found while chasing a desktop-image boot that stops at OpenRC's `Starting udev ...`
(this session's own commits are **not** involved: the same hang reproduces on
`1adfb6e`, the commit before them, with a `rootfs --force-full` image laid from
scratch).

**The reproduction is small and deterministic.** In a script run as a boot
`::sysinit:` action on the x86_64 image, fork **two or more** background children
that each `execve` a binary from the ext4 root and wait:

```sh
( /bin/echo one >/tmp/y1 ) &
( /bin/echo two >/tmp/y2 ) &
sleep 4
# both children are still `{sh}`, state S, /tmp/y1 and /tmp/y2 never appear
```

The readings, one variable apart (`.workbuddy` scripts are the session's own
`/tmp`-style probes and were not kept; the runs are `build/run-diag{4,5,6,7}.log`):

| shape | result |
|---|---|
| one background child that only runs shell builtins | runs, and `wait` on it returns 0 |
| **one** background child that `exec`s a binary | **runs** (`/tmp/x2` written) |
| foreground `exec` (the script's own `ps`, `ls`, `/bin/echo`) | runs |
| **two or more concurrent** background children that `exec` | **all of them stop before the exec completes**: state `S`, argv still the shell's, files never written, while the forking shell's `sleep` wakes and runs on |

So the fork survives, the child runs user code, and the child stops *inside*
`execve` — which reads the program through the ext4 cache and the parked
virtio-blk transfer (§10.5, `src/vfs/ext4/cache.rs`'s `FillClaim`/`RunClaim` and
`src/drivers/virtio/blk.rs`'s `begin_request`/`wait_for_device`/`finish_request`).
The bug is therefore in one of those two hand-offs *under contention*; a single
transfer is fine, concurrent ones are not. This kernel's x86_64 gate cannot see
it: that arm's root is an initramfs, so no program it runs is read through the
block device. The aarch64 arm's root *is* a disk, but no test in the suite forks
concurrent readers.

**First candidate, found and fixed - and it was not this one.** The ext4 cache
has two scratch buffers (one fill, one run) and a second claimant of a *different*
block waits for the scratch to come back. That wait registered **nowhere**: the
per-slot waiter lists only cover tasks waiting on *that block*, so the end of a
fill drained the slot's list and left the scratch waiter asleep with no deadline
to end it. That is a real deadlock and it is fixed in
`vfs::ext4::cache`'s new `claim_waiters` list (the end of any claim wakes it),
pinned by the host test `a_waiter_for_the_scratch_is_released_by_a_claims_end` -
whose counterfactual is the old `end_fill`/`end_run`, where the assertion fails
because the waiter is never handed back.

It does **not** cure this reproduction: with that fix built in, four concurrent
`exec`s still stop in `execve` (state `S`, no transfer in flight) and the desktop
boot still stops at `Starting udev ...`.

A second candidate was tried and is also ruled out, this one a real bug on its
own terms: the block layer's completion wait parks with `u64::MAX` as its sleep
deadline, so the "one poll per period for a wire that lost the wake" its own
comment promises can never happen - a missed completion interrupt would sleep
for ever. Giving that wait a deadline (`block_until_uninterruptible_nanos`)
changes nothing about this reproduction either, so it landed nowhere: an
unproven fix for a symptom it does not fix is not worth keeping, and the real
park is still unnamed.

So the missing instrument was built: `/proc/<pid>/wchan` used to answer a
hardcoded `0`, and it now answers **which wait the task last slept in**. The
mechanism is small and permanent, not a debug print: `sched::task::PARK_SITE` is
one atomic byte per task slot, written at the entry of each park primitive
(`block_until_on`, `block_until_nanos`, `block_until_uninterruptible`,
`block_until_interruptible_rolling_back`, `block_until_code`), and `wchan`
prints its number - the field keeps Linux's shape (a number there, an address)
and `sched::task::PARK_SITES` is the vocabulary that names it. A running task
keeps its last site, which is exactly what the question needs: `ps` asks about
tasks that are *asleep*.

**The first reading, and what it rules in.** With the instrument in the same
reproduction - four background children, each `exec`ing a binary off the ext4
root - all four answered `wchan = 4` = `block_until_uninterruptible`. That rules
out every other park in the tree (wait4, poll, pipe, futex, pty, net: each has
its own primitive or its own site) and puts the stuck tasks in the one wait the
two earlier candidates lived in: the block mailbox's completion/claim wait
(`drivers::virtio::blk`) and the ext4 cache's claim wait (`vfs::ext4`,
`claim_park::wait`, which calls this primitive too). The instrument does not yet
tell those two apart, because both enter the same primitive.

**The second reading, and the one place it points.** The primitive grew a
site-carrying variant (`block_until_uninterruptible_at`), and `claim_park::wait`
now names its three callers - a slot's own claim (7), the fill/run scratch (8), a
set with no free way (9). The same boot answers:

    pid 7..10  comm=sh  state=S  wchan=8     (all four)

so the four are waiting for the **ext4 scratch** - not the block mailbox, not a
slot, not a set. That is a sharper place than it looks: the scratch is one flag
(`fill_busy`/`run_busy`) set by `begin_fill`/`begin_run` and cleared only by
`end_fill`/`end_run`, and with four waiters parked on it **no task is in the
block mailbox and none is running the transfer** - so the flag is out with no
claimer visibly doing anything, which means the claim's owner either re-entered
the cache while holding it (a self-deadlock: `begin_fill` inside a path that is
already inside `Go`) or left without an `end_*`. The former is the shape to look
for first, and it is checkable: record the claim's owner task id in the cache and
print it beside the site, so `wchan` says not just "waiting for the scratch" but
"waiting for the scratch *of task N*" - and a cycle through N is the whole
diagnosis.

**The re-entry reading came back empty - and the reproduction passed.** The next
instrument was re-entry detection: each task slot records whether it is inside a
claim's transfer, and a scratch wait taken from that state reports site `10`
instead of `8`. The same boot then wrote all four files
(`one|two|three|four`, children reaped, nothing stuck) - so the shape did not
reproduce at all that time, with nothing in the tree that could have fixed it:
the change added two stores around the transfer and nothing else. A change that
only adds timing and hides a deadlock is a **race**, not a fix: the earlier
readings (4, then 8) were losses of it, and the extra rd/wr timing was enough to
hide it.

That matters more than it sounds. The same boot still stopped at
`Starting udev ...`, so **the boot's stall is not the same event as the
reproduction**: a deadlock in the concurrent-exec shape is one way to park a task
in the claim path, not the only one, and the boot may be failing differently.

**The next measurement is aimed at the boot, not the shape.** Put the detached
dumper *in front of* the real sysinit: the inittab's `::sysinit:` runs
`/root/watch.sh`, which starts `( sleep 20; ps; cat every /proc/<pid>/stat
field 35 ) &` and then `exec /sbin/openrc sysinit`. At the moment udev has
stalled, the serial log then carries the **stuck task's own park site** - the one
number this whole instrument was built to produce, taken from the boot that
actually fails.

**The boot's stall is a livelock, not a park.** The idle-loop dump never printed
(8 s of "nothing ran" never happened), so the next instrument was a heartbeat
from the tick: every five seconds, one line saying how long ago the last task
switch was and which slot made it. During the udev stall it reads:

    * Starting udev ...
    mpos: heartbeat age=1ms last slot=5 pid=5 parked=0 (0)
    mpos: heartbeat age=0ms last slot=5 pid=5 parked=0 (0)
    mpos: heartbeat age=2ms last slot=4 pid=4 parked=0 (0)
    ...

Two tasks - slots 4 and 5 - switch with an age of **0 to 3 ms**, both **not
parked**. So nothing in the boot is asleep: the machine is running two tasks that
make no progress, i.e. a spin or a two-task livelock, not the parked-transaction
deadlock the concurrent-exec reproduction produces. That is the single most useful
thing this session's instrument has said, and it re-explains the earlier
readings: an idle-path dump could never fire, `strace -p` could never attach
cleanly, and the process table looked empty because nothing was *waiting* - the
udev service's two processes were busy.

**Next**: name them and find the spin. The heartbeat knows the slot; the next
line should carry the task's `comm` (the table holds it), and the spin itself is
then in one of the two processes' user or kernel code - `openrc-run`'s shell and
whatever it started. The candidate to read first is the one this session has
already measured twice: a wait whose deadline re-arms in the past (the §6.24
trap's shape), which spins a task at 100% instead of parking it.

**The spinning task has no image.** The heartbeat's next line carried the
last-ran task's `comm` and image path. At the stall it reads, over and over:

    mpos: heartbeat age=0ms last slot=5 pid=5 parked=0 (0) comm= exe=<empty>

`comm` and `exe` are both empty, and `pid_number_of` answers 5 for that slot -
so the process being switched to has **no name set and no exec'd image**: it is
a `fork` child that never completed its `exec`, and it is *running* (parked=0,
age 0-4 ms) rather than waiting. That is the boot's stall in one line: the udev
service's fork-then-exec child is spinning before the exec, so the service never
starts, the parent waits, and the machine looks hung while burning a core.

The same fork/exec seam produced the concurrent-exec readings earlier in this
section - there the child *parked* in the ext4 claim path (wchan 4, then 8);
here it *spins* before any image is installed. The next step is to make the
heartbeat say where: the last syscall the spinning task took (a per-task
`last_syscall` word costs one store on the entry path and names the loop), and
then to read that loop's code. The prime suspect stays §6.24's shape - a wait
whose deadline is rebuilt in the past so every park returns at once - now
suspected on the exec side of the seam rather than the claim side.

**And the spinning task makes no syscalls.** The next line added the last-ran
task's last syscall number (one store on the entry path). At the stall it reads
`syscall=0` for both slots, over and over:

    mpos: heartbeat age=0ms last slot=5 pid=5 parked=0 (0) syscall=0 comm= exe=

No image, no name, no syscalls, and the scheduler keeps switching between them
with interrupts on (the heartbeat comes from the tick). Two readings follow, and
they are the ones to build the next step on: the two slots are **kernel-context
work** - nothing user-side can run without an image, and nothing that spins in a
syscall can report a syscall count of zero - while the boot's user processes must
therefore be *parked* elsewhere. So the machine is not deadlocked; it is running
kernel code in a loop, and the user tasks that should be driving udev are asleep.

The next heartbeat should therefore print the **whole slot table** (id, pid, park
site, syscall) rather than the last-ran line: one boot then shows both halves at
once - which slots are parked where (`4`/`7`/`8`, the claim path) and which two
kernel slots are looping - and the looping pair's identity is the remaining
question.

**The whole table, and the line that settles it.** The heartbeat now prints every
live slot - id, pid, park site, last syscall, and which one was switched to. At
the udev stall:

    mpos:   slot 1 pid 1 park 4 (block_until_uninterruptible) syscall 61
    mpos:   slot 4 pid 4 park 0 syscall 0
    mpos:   slot 5 pid 5 park 0 syscall 0  <-- running
    mpos:   slot 6 pid 6 park 4 (block_until_uninterruptible) syscall 61
    mpos:   slot 7 pid 7 park 2 (block_until_nanos) syscall 7
    mpos:   slot 8 pid 8 park 8 (ext4_scratch_wait) syscall 4
    mpos:  slot 10 pid 10 park 2 (block_until_nanos) syscall 281

`syscall 61` is `wait4` (init and the service's parent waiting for children),
`syscall 7`/`281` the pollers (a daemon's event loop), and **slot 8 is parked at
the ext4 scratch**, which is the deadlock shape the concurrent-exec reproduction
found - now read from the boot itself. The two slots switched to are kernel work
(no image, no syscalls), so the machine is otherwise idle: everything that should
drive udev is parked, and one of those parks is the scratch.

**What is left to name is the claim's owner.** The scratch flag is held and no
live slot is in a transfer, so the owner took the claim and did not come back for
it. The instrument for that is one field: the cache records the owner's task id in
`begin_fill`/`begin_run`, clears it in `end_*`, and the heartbeat prints it on the
`ext4_scratch_wait` line. If the id names a slot that is not live, the claim
outlived its task; if it names one of the parked slots, the cycle is visible
without another guess.

**The claim is not out - the waiter is wrong.** The heartbeat now names the fill
and run scratch holders beside the table. At the stall:

    mpos: heartbeat age=0ms fill-owner=none run-owner=none -- live slots:
    mpos:   slot 9 pid 9 park 8 (ext4_scratch_wait) syscall 21

`fill-owner=none run-owner=none` means **no claim is in progress anywhere**, and
slot 9 is nonetheless asleep at `ext4_scratch_wait`. That cannot be a wait on a
held flag: with both flags false, the park's own condition -
`!fill_busy()` / `!run_busy()` - is *already true*, and a park primitive that
would be entered now returns at its first check. So the task is not waiting for a
claim that exists; it is **asleep on a wake that never came**, and the flag it
was waiting for was cleared without waking it.

That reframes the whole section. The ext4 cache's scratch wait is not losing a
claim; it is losing a **wake**. Its registered list (`claim_waiters`, from the
first fix) is drained inside `end_fill`/`end_run`, and the drainer's wake is
handed outside every lock - so the lost wake is in the window between the
registration and the park, in the one place a *condition* cannot save it: the
waiter registers on a list the drainer has already walked. The next reading is
therefore not another "who" but a pair of counters: how many tasks registered on
`claim_waiters` and how many were handed back, plus a mirror of the two flags in
the heartbeat - a slot parked at 8 with a false flag and a registration the
drainer never saw is that window, printed.

**The flags are false while the waiter sleeps: a lost wake, proven.** The
heartbeat now mirrors both scratch flags (the cache writes them beside the owner
records). The stall reads:

    mpos: heartbeat age=1ms fill-owner=none run-owner=none fill-busy=false run-busy=false
    mpos:   slot 9 pid 9 park 8 (ext4_scratch_wait) syscall 21

Both flags false, no owner, and a task asleep on the wait whose condition is
`!fill_busy()` / `!run_busy()`. A park primitive entered now would return at its
first check, so the task is not waiting for anything: **the wake it needed was
lost**, and every reasoning that follows has to start there rather than at "a
claim outlived its holder".

Where the wake can be lost is now a short list, and the next reading is the one
that shortens it to one line: a **registered/drained counter pair** on the
cache's `claim_waiters` (how many tasks registered, how many the drainer handed
back). If they differ while a slot sleeps as above, the registration never
reached the drainer - the window between `wait_for_claim` (under the cache
borrow) and the park; if they are equal, the wake *was* handed to a task that was
running at the time and the bug is in the park's own re-check. Both are one boot
away, and the fix follows from which one it is.

**The counters, and why they do not settle it yet.** The first pair read at the
stall:

    mpos:   claim registrations=1 handed-back=3

More handed back than registered, which is *not* the lost-wake signature - and it
is the instrument's own fault, not the kernel's: registrations are counted at the
two `WaitScratch` call sites only, while the `WaitSet` branch registers too.
Counted at the funnel instead - inside `Cache::wait_for_claim`, which every
registration goes through, and inside `end_fill`/`end_run` for what they drain -
the pair becomes meaningful: registered > handed back is the window, equal is the
park's own re-check.

**And the shape that would make the window unnecessary.** A claim that is taken
and never ended leaves `fill_busy`/`run_busy` set with no owner - exactly the
state the stall shows - and today the only thing that clears the flags is the
holder running `end_fill`/`end_run` itself. Making the claim release the scratch
**on drop** (a `FillClaim` that borrows the cache, `Drop` clearing the flag and
handing the waiters back) removes the whole class: a claim dropped on any path -
an early return, an exec, a die - cannot outlive its transfer. That is the fix to
make first, and the counters are how its absence would be caught again.

**The funnel counters answer it: the wake was handed back - and the task slept
anyway.** Counted at `Cache::wait_for_claim` and at the two `end_*` drains (one
registration, one hand-back per pass), the stall reads:

    mpos:   claim registrations=1 handed-back=1

Equal. So the waiter's registration *did* reach the drainer, the drainer *did*
hand it back, and `claim_park::wake` ran with that id - and the task is still
asleep at `ext4_scratch_wait`, with both flags false. The broken half is
therefore the **park's own re-check**, not the list.

That closes the loop this section opened: there is exactly one wake per
registration, it is delivered, and the sleeper either never observed it or
observed it and slept again. The next change is on the wait's own shape - a
scratch park must re-register **and** re-check before every sleep, so no single
lost observation can strand it - with a host pin for the property (a waiter that
is woken with its condition true must return, and one that is woken with its
condition false must park again *and* stay registered).

**The wake counters do not isolate it; they do say what to count next.** At the
stall:

    mpos:   claim registrations=1 handed-back=1
    mpos:   wakes calls=7215 hits=6890 misses=325

Most wakes hit, and the claim list's own hand-back is the single event we care
about, so a global hit/miss pair cannot separate it from the boot's other 7000
wakes. The reading that can is narrower, and it is the next one: count **the
claim wake specifically** (its hit or miss) and count **how many times each slot
has parked at a claim site**. That pair settles the last two candidates this
section has left:

* a hit, a wake, and a *second* park at the same site - the waiter slept again
  after a new claim took the scratch, and in the code as it stands (one
  registration per wait) the second sleep is unregistered: the re-register shape
  tried on 2026-10-25 is then right but has to cover the *first* re-entry, and its
  first run did not, so the reason needs the per-slot count to be seen rather than
  assumed;
* a miss, and a park - the wake found the waiter running and the waiter's own
  re-check failed, which puts the bug in `block_until_uninterruptible_at`'s
  second look rather than in the list.

Both counters are three lines each; the note in the roadmap's open list records
this as the next measurement for the udev stall.

**The answer, and it is in the scheduler.** With per-slot park counts and a
claim-specific wake result in the heartbeat, the stall reads:

    mpos:   claim registrations=1 handed-back=1
    mpos:   wakes calls=7227 hits=6899 misses=328 claim-hits=3 claim-misses=0
    mpos:   slot 9 pid 9 park 8 (ext4_scratch_wait) syscall 59 parks=540

`claim-misses=0`: **every** wake the claim path issued found its waiter asleep -
`Scheduler::wake` answered "yes, I woke it" every time. One registration, one
hand-back, a hit, and the task is still parked. So the wait list is right, the
wake is right, and the task that was woken is still not running: the failure is
between `wake` returning `true` and the picker running that task again.

That is the last structural reduction this investigation can make, and it moves
the target out of `vfs/ext4` entirely: **a task woken from an uninterruptible
park (`sleep(id, priority, u64::MAX)`) is marked runnable but not picked**. The
next read is the scheduler's own `sleep`/`wake` pair - which run queue a sleep
with no deadline lands in, and whether `wake` puts it back on one - and the pin
for the fix is a host-testable scheduler case if the queue bookkeeping is
host-compiled, otherwise a boot self-test.

**The fix, and the boot moves past `udev`.** The claim wait was re-arming its
wake-up registration **once**, before the first park; a waiter woken and sent back
to sleep (a second claim took the scratch) was off the list forever after that.
The primitive now has an armed variant - `block_until_uninterruptible_armed(site,
cond, arm)` runs `arm` before **every** sleep - and `claim_park::wait` passes the
registration through it. With that:

    * Starting udev ...
     [ ok ]
     * Generating a rule to create a /dev/root symlink ...
     [ ok ]
     * Populating /dev with existing devices through uevents ...

and the heartbeat's list counters are balanced for the first time
(`registrations=4286 handed-back=4286`, `claim-misses=0`). The desktop boot no
longer stops at `udev`; it reaches udev's own trigger phase, which is where the
next reading starts.

**And the next stall is visible in the same instrument.** During the trigger the
heartbeat holds

    mpos: heartbeat ... fill-owner=slot 9/pid 9 ... fill-busy=true

with the list counters frozen: the fill scratch is out to task 9 and that
transfer never completes, while the machine keeps waking things (the global wake
count climbs ~700/s). The claim path is now correct; the block transfer inside it
is the next suspect, and the heartbeat already names the owner - so the next
measurement is that task's park site from the slot table (site 4, the mailbox, or
elsewhere) and then the mailbox's completion path.

**What the next stall is, read off the same table.** During udev-trigger's stall
the slot table names it:

    mpos:   slot 9 pid 9 park 4 (block_until_uninterruptible) syscall 59 parks=1915
    mpos: heartbeat ... fill-owner=slot 9/pid 9 fill-busy=true

syscall 59 is `execve`, and pid 9 is inside one, holding the fill scratch, with
**1915 parks** recorded - it is parking and waking over and over rather than
finishing the transfer. The claim list is balanced and every claim wake hits, so
the hand-off to the task is working; what does not happen is the *device
transfer* the fill is waiting for. The next suspect is therefore the block
mailbox's completion path (the request-submit / used-ring / wake half), not the
ext4 claim: a transfer that is never retired would park its reader for ever while
everything else in the machine wakes normally, which is what the counters show.

**Gate, this fix.** x86_64 `pass=210 fail=0` (210 built), aarch64 `pass=209
fail=0` (209 built), `mpos-kunit 32 passed, 0 failed` on both; host unit suite
green. The first aarch64 arm lost lane 2 to the documented §13.32 boot-EL0
observation (`EXCEPTION: synchronous`, `FAR_EL1` the page past `kernel_end`), its
QEMU was killed and the arm re-run alone - the second run is the one quoted, as
that trap prescribes.

**The block wait gets the clock its own comment promised - and the boot moves
again.** `blk.rs`'s completion wait used `block_until_uninterruptible`, whose park
sleeps with `u64::MAX`: its "one poll per period for a wire that lost the wake"
could never happen, because nothing woke the task to look at the clock. With

    block_until_uninterruptible_until(site, cond, deadline)

the wait ends at the deadline, the used ring is polled, and the transfer retries.
The desktop boot with it now passes **both** `Starting udev ...` and udev's own
trigger:

    * Starting udev ...                        [ ok ]
    * Generating a rule to create a /dev/root symlink ...  [ ok ]
    * Populating /dev with existing devices through uevents ...  [ ok ]

and stops afterwards with a new shape in the heartbeat: `fill-owner=slot 6/pid 6
fill-busy=true`, a dozen fresh processes (pids 9-18) all parked at
`ext4_scratch_wait`, and the owner in the block wait (site 4). So the remaining
stall is a **device transfer that is submitted and never retired** - the whole
ext4 claim path is now visibly healthy (registrations and hand-backs within the
number of parked waiters, every claim wake hitting), and the queue is behind one
transfer that the used ring never answers.

The next measurement is the block driver's own: count submits, IRQs, retirements
and poll timeouts inside `blk_irq`/`blk.rs` and print them from the transfer path
or in the heartbeat's header - a submit with no IRQ and no used-ring entry names
the kick/doorbell or the IRQ-ack as the culprit, which is the file's own
"interrupt-driven completions" work from `3107e71`.

**The stall after the trigger names a claim whose holder is gone.** The
heartbeat at that stall reads

    mpos: heartbeat ... fill-owner=slot 6/pid 6 ... fill-busy=true -- live slots:
    ... slot 5 pid 5 ... slot 10 pid 10 ... (no slot 6)

`fill-owner` names slot 6 and the slot table does not contain it: the task took
the fill scratch inside its transfer arm and **died there**, so `end_fill` never
ran and the flag outlived its holder. Every later reader then queues on a scratch
nobody holds - which is the dozen `ext4_scratch_wait` entries the same heartbeat
prints, all of them waiting on a claim whose owner no longer exists.

The fix is to record the holder **in the cache** (`fill_owner`/`run_owner`) and
make `fill_busy()`/`run_busy()` answer "out **and its holder is alive**", so a
dead claim's scratch is free for the next asker (`sched::is_exited` is the
liveness question; the host build answers alive, where no claim outlives its
arm). That was written and tried: the boot does not regress with it (still
reaches udev-trigger and stops after it), so it is **not** what the boot is
waiting on now - and an unproven change is not landed. It is the next fix to
make, with a boot self-test as its pin (a task that dies holding a claim must
leave the scratch usable), and the stall it does not address is the one after
`Populating /dev ... [ ok ]`, whose heartbeat has not yet been read.

**Two leaks tried, neither moves the post-trigger stall.** Both were written,
built and booted, and neither changed where the boot stops:

* a **claim whose holder died** leaves `fill-busy`/`run-busy` set (the heartbeat
  read exactly that once: `fill-owner=slot 6/pid 6` with slot 6 absent), so the
  fix is the holder recorded in the cache and the busy predicates asking
  `sched::is_exited`;
* the **block mailbox's holder** can die the same way (`BUSY` set, `WAITER`
  naming a dead task), so the fix is the same answer in `blk_irq::begin_request`.

With either in place the boot still stops after `Populating /dev ... [ ok ]`, and
the stall's own table says why neither is *this* stall: `fill-owner=slot 10/pid 10
fill-busy=true`, slot 10 **alive** and parked at site 4 with `parks=33201` -
a task cycling through the device-claim wait at the timed park's 4 ms period
while a dozen others queue at `ext4_slot_wait` (7). So the mailbox is held by a
*living* task that is itself waiting for something else - a cycle between the
claim and the device - not by a corpse.

Both leak fixes are **not landed**: each is right on its own terms and each is
unproven against the boot, and the repo's rule is that an unproven fix does not
land. They are recorded here as the next work, with the pin each needs (a boot
self-test: the state a dead holder leaves must be reclaimable). The stall to read
next is the cycle the table shows: which task holds the mailbox that slot 10 is
waiting for, and what *it* is waiting on - the heartbeat already prints every
slot's park site, so the answer is in the next boot's table.

**The cycle, and an instrument bug that hid it.** With the mailbox mirrored into
the heartbeat and the device-claim wait given its own park site (11), the stall
after udev-trigger reads:

    mpos: heartbeat ... blk-busy=true blk-waiter=slot 6 -- live slots:
    ... no slot 6 ...
    mpos:   slot 9 pid 9 park 11 (blk_claim_wait) syscall 0 parks=21814
    mpos:   slot 10 pid 10 park 11 (blk_claim_wait) syscall 4 parks=29768

So the **mailbox is claimed by slot 6, which is not in the live table** - a task
that claimed the device and died before retiring, exactly the leak shape the fill
scratch showed. Two reclaims were then tried (the claim's holder recorded in the
cache and the mailbox's holder liveness), singly and together, and the boot still
stops at the same line - so the mailbox's *recorded* holder is not the whole
story.

And the instrument itself has a hole worth recording: `complete_irq` swaps
`WAITER` to 0 and the heartbeat's mirror was only written on claim and retirement,
so `blk-waiter=slot 6` may name a claim that has already been completed (the
owner died between the interrupt and its own `finish_request`). The mirror now
clears on `complete_irq`, so the next boot's header distinguishes "claimed by
slot N" from "completed, waiting for a retirement that will never come" - and that
distinction is the next reading.

**The distribution, four runs.** The stall is a race, so the reading that matters
is where the *stock* desktop image stops over several boots (each 160 s, the same
binary, nothing else touching the machine):

    RUN 1: * Populating /dev with existing devices through uevents ...
    RUN 2: * Populating /dev with existing devices through uevents ...
    RUN 3: * Populating /dev with existing devices through uevents ...
    RUN 4: * Populating /dev with existing devices through uevents ...

Four of four reach udev's trigger and stop there - past `Starting udev ...` and
past the /dev/root rule, consistently, which is the shape the two fixes
(`claim re-arm`, the block wait's deadline) bought. The earlier boot that stopped
back at `Starting udev ...` is the same race landing differently, not a different
stage.

What comes next is the *handoff out of* `openrc sysinit`: nothing after the
trigger prints a `*` line, so either sysinit's own exit or the inittab's next line
(`openrc boot`) is where it stops. The heartbeat's table at that moment names
parked slots but not their commands, and the next instrument change is therefore
one field: **each slot's `comm` in the table**, so the stuck tasks can be named -
`openrc`, `udevadm`, a shell or a daemon - instead of inferred from a park site.

**The queue is udev's workers, and the claim's holder is a slot that is gone.**
With each slot named in the table, the post-trigger stall reads:

    mpos: heartbeat ... fill-owner=slot 6/pid 6 fill-busy=true blk-busy=true blk-waiter=none
    mpos:   slot 10 pid 10 name=/sbin/udevd  park 8 (ext4_scratch_wait) syscall 4 parks=4121
    mpos:   slot 11 pid 11 name=/sbin/udevd  park 8 (ext4_scratch_wait) syscall 3 parks=1670
    ... (slots 12-17 the same, and more) ...
    (no slot 6 in the list)

Eight and more udev workers are queued on the fill scratch, the claim's recorded
owner is slot 6, and slot 6 is not in the table. `blk-busy=true` with
`blk-waiter=none` says the mailbox's completion was already taken by the handler,
so what is missing is the **retirement** - the owner died between the completion
and its own `finish_request`, leaving `BUSY` set and the fill scratch out. That is
one event, not two: the task holding the fill is the task that died in the device
wait.

So the next question is not "which park" any more - it is **why that task dies
inside the transfer**. The reading that answers it is three lines in the exit path
(`sched::exit`, where the task's descriptors are released): if the dying task is
the recorded fill owner or the mailbox's waiter, print one line with its slot,
pid, name and the mailbox's state. One boot then says whether a *fatal signal*
reached a task mid-transfer, or whether the "death" is a mirror that was never
cleared.

**No task dies holding the claim - so the claim is leaked by a path that is not
death.** A temporary line in `sched::exit` printed whenever the exiting task was
the recorded fill/run holder or the mailbox's waiter. At the stall it printed
**zero times**, while the heartbeat still said

    mpos: heartbeat ... fill-owner=slot 6/pid 6 fill-busy=true blk-busy=true blk-waiter=none
    (no slot 6 in the live table)

So the owning slot is absent, the fill is still out, and **nothing went through
`exit()` while holding it**. The holder-leak theory is therefore wrong in the form
it was tried in (which is why both reclaims failed to move the boot): the claim is
not outlived by a dead task, it is *never given back* by a path that does not die -
the `Go` arm of the read loop either never reaches its `end_fill`/`note_fill_owner(None)`
pair, or the slot table's absence means something else entirely (a slot reused, a
mapping read wrong).

The next instrument is the claim's own lifecycle rather than its holder: a
**begin/end counter pair on `begin_fill`/`end_fill`** (and the run pair), printed
in the heartbeat. `begin - end == 1` with the claim out proves that a claim was
taken and never ended, and the same pair says when it happens relative to the
stall - which is the smallest fact this section still lacks.

**The lifecycle pair, and the leak is intermittent.** `begin_fill`/`end_fill` (and
the run pair) are counted now and printed in the heartbeat. One boot reads

    mpos: ... fill=5457/5457(out=0) run=0/0(out=0)   blk-busy=false blk-waiter=none

- every fill taken was given back, no mailbox claim outstanding - and that boot
stops at the same place the stalled ones do (`Starting udev ...` again). So the
leak is **not** on every boot: the two boots that reached udev's trigger leaked a
fill (the one whose owner slot 6 was gone), this one did not.

Together with the zero exit-path hits, the shape is now: the claim is leaked
**sometimes**, by a path that neither dies nor reaches `end_fill`, and the run
scratch never leaks at all in these readings. The next reading is the same pair
sampled at the *moment* `begin - end` first becomes 1 (print one line when
`note_fill_begin` makes the pair unequal, with the slot and syscall) - a single
line that dates the leak instead of inferring it after the fact.

**The leak is dated: taken in `exit_group`, and the flag is not the whole of it.**
The lifecycle pair now records when, by whom and in which syscall the outstanding
fill claim was taken. The stalled boot reads

    mpos: ... fill=9145/9144(out=1 age=69989ms slot=6 syscall=231) run=0/0(out=0)

`syscall 231` on x86_64 is **`exit_group`**, and the claim is 70 seconds old: a
task that was exiting took the fill scratch in a filesystem read on its way out
and never came back to `end_fill`. That is why the exit-path diagnostic found no
*death* holding the claim (the task had already marked itself exiting) and why the
holder-liveness reclaim did not move the boot: the flag is only half the claim.
The other half is the **slot the claim marked `loading`**, and `begin_fill` picks
a way with `victim`, which skips a loading slot - so with the leaked claim's slot
still marked, `begin_fill` answers `None` and the reader parks on `WaitSet`
(`set_in_flight`), which nothing will ever clear. That is the state the udev
workers queue in.

So the fix to make is a reclaim that takes **both halves**: the cache records the
claim's slot (not just the owner), and a claim whose holder is gone clears its
slot's `loading`, the flag, and the scratch before the next asker proceeds. The
same for the run claim. Until then the boot stops after udev-trigger on the boots
that hit this race.

**The full reclaim was written and does not fire: the contradiction to read
next.** Recording the claim's slot in the cache and clearing both halves
(`forget_abandoned`: the flag, the owner, and the way the claim marked `loading`,
plus a bounded 20 ms re-arming claim wait) compiled, passed the host suite, and
did **not** free the leaked claim:

    mpos: ... fill-owner=slot 6/pid 6 fill-busy=true blk-busy=true blk-waiter=none
    mpos:   ... fill=9109/9108(out=1 age=104899ms slot=6 syscall=231)
    (no slot 6 in the live table)

So `forget_abandoned` ran with `fill_busy` true and did not free it, which can only
mean `holder_alive(Some(TaskId(6)))` answered **true** - i.e. `sched::task_alive(6)`
found a live, non-exited task in slot 6, while the heartbeat's table (whose liveness
test is the same `!task.exited`) does not print it. One of the two is wrong, and the
next reading is one line in `forget_abandoned`: the holder's slot, `task_alive`, the
task's `exited` flag and its pid, printed the first time a claim is found abandoned.

Two smaller things also came out of the attempt and are recorded rather than lost:
the re-arming wait makes `claim_waiters` accumulate duplicates (`registrations=15465
handed-back=13`), so registration has to become idempotent per task; and the
unproven attempt is not landed, per the rule.

**The two reclaims, and the two races they leave visible.** With both claims given
back in full (the cache also clearing the ways a dead claim marked `loading`, the
mailbox abandoning a claim whose owner is gone or whose completion nobody
retired), a boot that previously carried a fill claim out for 100+ seconds with
udev's workers queued behind it now reads

    mpos: heartbeat ... fill-owner=none fill-busy=false blk-busy=false ...
    mpos:   ... fill=5457/5457(out=0 none) run=0/0(out=0)

- no claim out, nothing abandoned - and *that* boot stops back at
`Starting udev ...`, while others reach the trigger and stop there. So the
desktop image has (at least) two races: one around udev's own service start that
leaves no claim behind at all, and the claim leak after the trigger that these
reclaims close. Closing the second is what makes the first readable as what it
is - a stall with the machine idle (two kernel slots switching, every userland
task parked) rather than a queue behind a dead claim.

The pin still owed for the reclaims is a boot self-test: leave a claim with a
holder that has exited and assert the next asker gets the scratch. It is named
here because it is not written yet.

**Gate, the two reclaims.** x86_64 `pass=210 fail=0` (210 built), aarch64
`pass=209 fail=0` (209 built), `mpos-kunit 32 passed, 0 failed` on both; host unit
suite 637 / 0 failed. The first aarch64 arm lost lane 0 to the documented §13.32
boot-EL0 observation (`EXCEPTION: synchronous`, `FAR_EL1` the page past
`kernel_end`); the lane's QEMU was killed and the arm re-run alone, and the re-run
- `pass=209 fail=0`, exit 0 - is the one quoted, as that trap prescribes.

**The other race, named: the udev init script livelocks in `poll`.** The boot
that stops at `Starting udev ...` with no claim outstanding has this table:

    slot 1  pid 1  name=/sbin/init                     park 4  syscall 61 (wait4)
    slot 6  pid 6  name=/sbin/openrc                   park 4  syscall 61 (wait4)
    slot 7  pid 7  name=/etc/init.d/udev               park 2  syscall 7  (poll) parks=30856
    slot 8  pid 8  name=/usr/libexec/rc/sh/openrc-run.sh park 4 syscall 61 (wait4)
    slot 10 pid 10 name=/sbin/udevd                    park 2  syscall 281 (epoll)

so the chain is `openrc` waiting for `openrc-run.sh`, which waits for the service's
own shell `/etc/init.d/udev` - and **that shell is cycling through `poll` thirty
thousand times**, not blocked in it. The daemon itself (`/sbin/udevd`, epoll) is
already up, so this is not a start that failed; it is the service *script* looping
in `poll`, and since `poll` is a nanosleep-shaped park (site 2) every iteration is
a park, which is what the count measures.

That is a different bug from everything in this section: not a lost wake, not a
leaked claim, but a **`poll` that returns immediately instead of blocking** (or
reports ready for a descriptor that has nothing to give), which turns a caller's
wait into a spin. The next reading is the poll's own: count, per task, how many
`poll`s return `ready` with nothing to read, and print that with the slot table -
one boot then says whether it is a zero-length read, a wrong readiness mask, or a
descriptor whose `poll` implementation always claims readable.

**Correction to the reading above: the poll is not necessarily returning
immediately.** `parks=30856` over a stall of roughly a hundred seconds is one park
every ~3 ms, which is the interval `poll_wait`'s own loop is built to have: it
parks until `min(deadline, next tick)` so a wait whose watched descriptors have no
waker still re-checks each tick. So the service script is in an ordinary poll
loop, and what matters is **what it is waiting for and why that never happens** -
not a spurious readiness. The candidates, in order: `start-stop-daemon`'s
readiness wait (a pidfile or a pipe the daemon is expected to signal), a netlink
uevent socket, or the trigger's own settle. The next instrument is therefore on
the poll's *arguments*, not its count: one line per task the first time a poll
parks on descriptors none of which is a known waker, naming the fd numbers and
their kinds.

**What the scripts' polls watch, read once per task.** A temporary line at the
top of `poll_wait` printed the call's arguments the first time each task polled.
The two shapes in the stalled boot are:

    mpos: poll slot=7  nfds=1 timeout_ms=None   poll[0] fd=0 events=0x1 kind=0
    mpos: poll slot=19 nfds=1 timeout_ms=Some(60978) poll[0] fd=4 events=0x1 kind=7
    mpos: poll slot=16 nfds=1 timeout_ms=Some(60965) poll[0] fd=4 events=0x1 kind=7

Slot 7 is the **udev service script's own shell** (`/etc/init.d/udev`), and it
polls **its stdin**, descriptor 0, for `POLLIN`, with **no timeout** - while the
udev event workers (16, 19) poll an anonymous descriptor with a sixty-second
timeout, which is a daemon worker's ordinary loop. A service *script* waiting
forever for readable stdin is what a `read` in a pipeline or a command inheriting
the console would do, and it is the shape that would make openrc wait for ever
while udevd itself is healthy.

The next measurement is that descriptor's identity: what `/etc/init.d/udev` has on
fd 0 at that moment (the console, a pipe from a command substitution, or a closed
descriptor the kernel is reporting readable). The temporary line was removed after
this reading, as debug output does not ship.

**The poller is `openrc-run`, and what it watches is always-ready.** Reading the
service script settles one thing and sharpens the next: `/etc/init.d/udev` has no
`read` of its own, and its interpreter is `/sbin/openrc-run` - which is the process
whose `comm` openrc sets to the script path for `ps` friendliness. So the task
polling **fd 0 for `POLLIN` with no timeout** is `openrc-run`, waiting on its own
stdin.

And the default `VfsNode::poll_ready` is `Ready::ALL` - every node that never
blocks, regular files included, is *always* ready. A caller polling an always-ready
descriptor is told "ready" at every call; if its read then says end-of-file and it
does not treat that as the end, its loop is a spin. That is the shape to check
next, and it is checkable from the poll side without guessing: when a task polls
the same descriptor ready more than ~100 times, print the fd, its kind, the
readiness reported (read/write/hup/err) and the events asked for - one line that
tells an always-ready file from a hang-up nobody consumed.

The temporary per-task argument dump was removed after its reading; this note is
what survives of it, because debug output does not ship.

**The spin detector, and the reading that moves the question to fd 0.** The poll
side now says one line when a task is told the same descriptor is ready a hundred
times over:

    mpos: poll spin slot=7 fd=0 events=0x1 kind=0 read=true write=true hup=false err=false

Slot 7 is `openrc-run` running `/etc/init.d/udev`, the descriptor is **fd 0**, the
node is a plain **File** (`kind=0`), and the answer is *ready* with no hang-up - so
the kernel is behaving as Linux does (a regular file is always pollable), and the
spin is the caller's loop over a descriptor that is always ready.

So the remaining question is not poll's answer but **what fd 0 is** for
`openrc-run`: on a Linux boot the sysinit action's stdin is a tty (busybox init
opens the console), where a read blocks; here it is a node this kernel calls a
plain file and that is always ready, so a read returns end-of-file at once and the
loop turns. The next measurement is that descriptor's identity - inode number, the
filesystem it belongs to and its size - printed once beside the spin line, which
says whether it is the script, `/dev/null`, or a console node this tree serves as a
file.

**fd 0's identity: a small regular file, and the shape of a leaked descriptor.**
The spin line now carries the descriptor's identity:

    mpos: poll spin slot=7 fd=0 events=0x1 kind=0 ino=749 size=204 fs=0
          read=true write=true hup=false err=false

so what `openrc-run` polls on stdin is a **regular file of 204 bytes, mode 0755**
on the root filesystem. Scanning the installed image for that size names the only
two candidates, and the executable mode picks one out: **a service script** (204
bytes, 0755) - not the console, not `/dev/null`, not a tty.

That is not a file a sysinit action should have on stdin, and the shape that
produces it is a **descriptor inherited from an earlier script execution**: if
exec'ing a script leaves its file on a low descriptor (0 among them), the next
process in the chain - `openrc`, then each service's `openrc-run` - inherits it,
and every poll on "stdin" becomes a poll on a regular file, which is always ready,
which is the spin this section has been chasing.

The next measurement is one line in `execve`'s script path: the descriptor the
script file was opened as, and whether it is closed (or CLOEXEC) before the
interpreter is entered. A script executed by `openrc` must not leave its own file
open on 0.

**The spinner is named: `gendepends.sh`, with a service script on fd 0.** The spin
line now carries the process too:

    mpos: poll spin slot=7 exe=/usr/libexec/rc/sh/gendepends.sh fd=0 events=0x1
          kind=0 ino=749 size=204 fs=0 read=true write=true hup=false err=false

`gendepends.sh` is openrc's dependency-cache script, and the 204-byte file on its
stdin is `/etc/init.d/crond` - which the script itself explains: its line 74 is

    read one two three <"$RC_SERVICE"

so the shell runs `read` with **a service script redirected onto fd 0**, and that
descriptor is a regular file, which `poll` answers "always ready" to - Linux's own
answer. The remaining question is therefore inside that call: which code polls fd 0
here (a `read` implementation, a `poll` from the script, or a child), and why it
re-polls instead of consuming to end-of-file. The kernel's half of the shape is
Linux's; whatever comes next is on the userspace side of this descriptor, and the
line to read it is the poll's caller, which the next instrument can name by
printing the task's last syscall beside the spin (as the heartbeat already does for
its table).

**The last reading: `rt_sigprocmask` then `poll`, over and over.** The spin line
carries the syscall before the poll now:

    mpos: poll spin slot=7 exe=/usr/libexec/rc/sh/gendepends.sh before=14
          fd=0 ... kind=0 ino=749 size=204 read=true write=true hup=false err=false

`14` on x86_64 is **`rt_sigprocmask`**, so the loop is signal-mask → poll → signal-
mask → poll, on a **regular file** on fd 0 (the service script `gendepends.sh`'s
line 74 redirects onto stdin). The kernel's answers are Linux's: `poll` on a
regular file is always ready, and a `poll` with a saved/restored mask is a `ppoll`
shape.

So the remaining question is the userspace one - which construct in that shell both
blocks its signal mask and polls stdin - and it is answerable *outside* this kernel:
`gendepends.sh` and the shell it runs can be read from the image, and busybox's
`sh` builtins (`read`, `select`, `wait`) are the candidate callers. Nothing in the
kernel's poll, file readiness or mask handling is implicated by these readings; the
script is polling a file it will read to end-of-file, and the re-poll is the
caller's.

This is where the session's tools stop: the desktop boot reaches udev-trigger and
then stalls in `gendepends.sh`, the user's original `Starting udev ...` stall is
fixed, four wake-up/claim defects are fixed and gated, and the remaining one is a
userspace loop whose shape is now printed on the wire.

**The script has no poll of its own, so the loop is in the shell's `read`.** Read
from the installed image, `gendepends.sh` contains exactly one wait-shaped line -
line 74's `read one two three <"$RC_SERVICE"` - and no `select`, `poll`, `-t` or
`wait`. So the `rt_sigprocmask` → `poll` loop on fd 0 is inside **busybox's `read`
builtin** as it reads a *regular file*, which is not the shape a plain `read` of a
file should have: a file read is a `read(2)` that returns the line or 0 at EOF.

That moves the question to one measurable comparison: what a plain `read` of a
regular file does on a real Linux here (a host probe), against what our kernel's
`read` returns to that builtin - and the fastest way to see our side is to run
`gendepends.sh` under `strace` in a booted guest, which this session has done
before for other scripts. If the shell is polling, it is because our `read`
answered something a file never answers (an `EAGAIN`, or a short read that looks
like a wait), and that answer is the bug to fix.

**Handoff.** The desktop image now boots past `Starting udev ...` and udev's
trigger, and stalls in `gendepends.sh` reading service scripts under busybox
`read`. The instruments to read it are in the tree (`/proc/<pid>/wchan`, the slot
heartbeat with names and syscalls, the poll-spin line with the descriptor's
identity and the syscall before the poll). The next step is the strace of that
script in a booted guest, plus a host measurement of `read` on a regular file at
EOF.

**The minimal reproduction does not reproduce.** In a booted guest with a shell,
the three shapes a plain file-read takes under busybox `sh` all succeed:

    read a b c < /etc/init.d/crond                  -> rc=0
    exec 0</etc/init.d/crond; read a b c            -> rc=0
    while read a b c < /etc/init.d/crond; do...     -> rc=0, reads the shebang

and the only spin the new detector reported in that session was the *interactive
shell* polling its console (`fd=0 kind=3` - a character device, not a file), which
is a shell waiting for input and not the bug.

So `gendepends.sh`'s loop is not "a shell reads a regular file" - that works. What
differs in the failing context is the environment the script runs under (its fds,
its `RC_*` variables and the commands it runs after the `read`), and the next
measurement is the script run *as openrc runs it*: `strace -f` of
`gendepends.sh` inside a booted guest, with the same environment, to see the
sequence around the `rt_sigprocmask`/`poll` loop rather than a hand-made shape of
it.

**What the script writes, and the step openrc never finished.** Reading the whole
of `gendepends.sh` from the image puts the spin in a protocol: lines 17-51 define
`config`/`need`/`use`/`before`/`after`/`provide`/`keyword` to **write dependency
records to fd 3**, and for each service a subshell does `exec 3>&1 1>&2` (fd 3 is
the parent's pipe) and then `. "$_dir/$RC_SVCNAME"` - so the service's own
`depend()` lands on fd 3.

And the boot log shows what that step did:

    * Caching service dependencies ...
    * /dev will not be mounted due to user request
    ...
    * Populating /dev with existing devices through uevents ...  [ ok ]

There is **no `[ ok ]` after "Caching service dependencies"**, yet openrc went on
to devfs and udev. So the dependency builder started, its result record never
completed, and openrc did not wait for it as a finished step - while the builder
itself is still alive at the end of the boot, spinning in a `rt_sigprocmask` →
`poll` loop on a service script on fd 0.

That gives the next measurement two concrete ends of one wire: the **pipe** the
records travel on (fd 3 in the builder, read by openrc) and the builder's own
`read`/`source` of a service script. The shape to test is the pair together - a
writer whose reads are fine but whose records never arrive, or a reader that
finished early - and the cheap way to see it is a guest boot with the same script
run under `strace -f` and its fd 3 piped to a file, which is what the handoff says.

**Run standalone, the script completes; run under openrc, it spins.** In a booted
guest with a shell, `RC_SCRIPTDIRS=/etc/init.d sh /usr/libexec/rc/sh/gendepends.sh
3>/root/deps.out` runs to its end (the trace was cut short by a *strace* crash, not
by the script: this kernel's `PTRACE_GET_SYSCALL_INFO` reports the operation as
the exit one on entry, `ptrace_syscall_info.op == 2`, and the tracee then dies on a
user `#PF` at `0x8c` - the same ptrace shape this session found earlier, and a bug
of its own).

So the script alone is fine and the difference is the context openrc gives it: the
environment it exports per service and, most of all, **fd 3 being a live pipe to
openrc**. The spin is therefore most likely either

* a **write into fd 3 that never completes** (openrc not draining the pipe while it
  waits, and the subshell blocked in `write`), which would show as a park in a
  pipe wait rather than in `read`; or
* a **read from a service script on fd 0** in a context where the shell has taken
  the same path differently - but that path works standalone, so the environment
  has to be the difference.

The next measurement is the faithful one: prepare the same pipe openrc does (a
reader that drains slowly), export the variables openrc exports, and run the script
with `strace` **after** fixing `PTRACE_GET_SYSCALL_INFO`'s op value, because until
that is fixed strace cannot be the instrument for any script on this kernel.

**The ptrace op, read as far as it goes without a machine.** `PTRACE_GET_SYSCALL_INFO`
writes the operation byte from `trace_syscall(id)`, whose answer is
`stop.syscall.entry` - set `true` by `stop_at_syscall(entry = true, ...)` and
`false` by its exit half - and `abi::write_syscall_info` maps those to
`PTRACE_SYSCALL_INFO_ENTRY` (1) and `..._EXIT` (2). Both halves look right in
isolation, so the mismatch strace reports ("entering, ptrace_syscall_info.op == 2")
is most likely a **sequence** fault rather than a field one: a stop that was never
delivered (or was delivered twice), after which strace's state machine is one
phase behind - and its very next report is an entry sample answered with an exit.

The two candidates the code points at are the `armed` flag (a tracer re-arms it on
resume, and `stop_at_syscall` consumes it at the stop: a resume that clears it
without delivering a stop would drop one) and the `at_exec_exit` path, which
returns early without a stop for a legacy tracer. Either is one boot away from an
answer with the existing instruments - the syscall the tracee was in and the stop's
own report are both printable - but not from this session's remaining context.

**The pin to write, before the fix.** The right next artifact is not a strace run
in a guest - it is a **`t-*` conformance program**, because the gate can run it on
both machines and because a ptrace bug is exactly the kind the repo demands a pin
for. The shape:

```
t-ptrace-syscall-info
  fork a child that calls getpid() once;
  the parent: PTRACE_TRACEME/ATTACH, wait for the first stop,
  PTRACE_SETOPTIONS(PTRACE_O_TRACESYSGOOD), PTRACE_SYSCALL;
  wait: expect SIGTRAP|0x80;
  PTRACE_GET_SYSCALL_INFO -> assert op == PTRACE_SYSCALL_INFO_ENTRY (1)
                              and nr == the syscall the child is entering;
  PTRACE_SYSCALL again; wait; assert op == EXIT (2) and that `rval` is the result;
  finally PTRACE_KILL / DETACH and reap.
```

It fails today at the first assertion - `op == 2` on an entering stop, which is
what strace reports - so it is a counterfactual pin by construction: with the stop
sequence fixed it passes, and with the sequence broken it is red in the gate rather
than in a hand-run. The fix it pins is one of the two paths named above (`armed`'s
consume/re-arm pair or the legacy `at_exec_exit` early return), and the test's
first failing assertion also *says which*: whether the entry stop was missing
altogether (op was the previous call's exit) or the wrong op was written for a stop
that existed.

**The pin, run once: its expectations were the wrong half.** The program above was
written and run in the gate before landing, and it answered something better than a
red: the operation byte is **correct at both stops** -

    ok:    the entry stop says PTRACE_SYSCALL_INFO_ENTRY
    ok:    the exit stop says PTRACE_SYSCALL_INFO_EXIT
    FAIL:  the entry stop names the call being entered (getpid)
    FAIL:  the exit stop carries the call's result (the child's pid)

- and the two failures are the *test's* assumption, not the kernel's. The child is
stopped by `raise(SIGSTOP)` and resumes into the signal trampoline, so the first
syscall boundary after the resume is **`rt_sigreturn`**, not the `getpid` the test
wants to see; the kernel named that call correctly and reported its completion
correctly. The design therefore needs to walk stops until the entry's `nr` is the
call it made (expecting `rt_sigreturn` first), rather than assume the first stop is
it.

The program was reverted rather than landed: it fails, and a red default sweep
means the kernel is broken - the opposite of what this one says. The corrected
shape is what the next attempt writes, and the three checks it already passed are
the evidence that `PTRACE_GET_SYSCALL_INFO`'s op handling is not the bug the strace
crash suggested - the tracer's own desynchronisation has another cause and the next
instrument is the *stop sequence* (how many stops per syscall a `PTRACE_SYSCALL`
tracee produces), which the same test can measure by counting boundaries between
the resume and the `getpid` it is waiting for.

**The sequence, measured - and the stop that lied.** `t-ptrace-stops` printed the
boundaries of a `PTRACE_SYSCALL` tracee:

    stop 1: ENTRY nr=14      stop 2: EXIT rval=0
    stop 3: ENTRY nr=39      stop 4: EXIT rval=7      (getpid, the child's pid)
    stop 5: ENTRY nr=110     stop 6: EXIT rval=6      (getppid, the parent's)
    stop 7: ENTRY nr=231     child finished after 7 stop(s), 4 entr(ies)

So the pairs are correct, the numbers are the calls the child made, and a call that
never returns (`exit_group`) has an entry stop and no exit stop, which is Linux's
own shape. The desynchronisation therefore had to be a stop that is **not** a
syscall boundary answering as if it were one - and this kernel did exactly that:
`trace_syscall` fell back to the last call the task had entered for any other stop
(a signal delivery, the attach's `SIGSTOP`, an exec event) and answered `ENTRY`
with a stale number. `strace`'s startup probe sends a signal, asks for the
structure at that stop, and **rejects** an operation that is not
`PTRACE_SYSCALL_INFO_NONE` (its own check, quoted in `proc.rs`), then assumes the
stops alternate - one dropped stop away from treating every return as the next
call.

The fix is one fallback removed: `trace_syscall` describes a syscall only at a
syscall stop, and a signal stop answers `NONE`. The pin still owed is the pair
`t-ptrace-syscall-info` was trying to be: the NONE answer at a signal stop, and the
entry/exit pair walked *to* the call the child made (expecting `rt_sigreturn`
first), and the smoke test is `strace /bin/true` in a guest - which is what
motivated all of this.

**Where the ptrace thread stands, and what the gate did.** The fix and its pin are
committed (`fdc69f7`, `6add34c`); the pin's six checks were read green off a lane
(`a signal stop answers PTRACE_SYSCALL_INFO_NONE`, `the NONE answer still carries
the tracee's ip and sp`, `the entry stop names the call the child made (getpid)`,
`the exit stop carries that call's result`), and the smoke test the note asked for
passed on the real machine: `strace -o ... /bin/true` exits 0 with an 11-line trace
ending in `exit_group(0 <unfinished ...` / `+++ exited with 0 +++`, and
`strace -f /bin/sh -c true` exits 0 with 27 lines - where before the fix the same
attempt reported `ptrace_syscall_info.op == 2` and killed its tracee on a user page
fault at `0x8c`.

The x86 gate arm, however, aborted on **lane 1's 900 s timeout**, and that lane's
own serial log shows a *finished* sweep (`=== SUMMARY pass=34 fail=0`, `=== END`,
`mpos: reboot(0x4321fedc)`). A lane whose machine concluded but whose harness lost
the exit is this repository's documented flake shape, and its prescription is: kill
the lane's QEMU, re-run alone, quote the green run. **That re-run has not happened** -
the working tree at that moment carried another worker's in-progress
`drivers/virtio` block/poll changes (`drain_abandoned`, `note_blk_submit`, a kunit
suite), and a gate run builds the working tree, so a red there would not be a
verdict on this thread. The gate line for `fdc69f7`/`6add34c` is therefore **owed**,
to be taken on a tree that contains only the ptrace change.

**The pin was right and the gate proved it: a stop the tracer never hears.** On a
clean worktree at the ptrace fix the x86 arm reached the sweep and then **lane 3
timed out at 900 s**, and that lane's own table names the hang precisely:

    mpos:  slot 6 pid 6 name=/bin/t-ptrace-syscall-info  park 2  syscall 61 parks=89
    mpos:  slot 7 pid 7 name=/bin/t-ptrace-syscall-info  park 2  syscall 39 parks=34

Slot 6 is the test's parent parked in **`wait4`**, slot 7 its child parked with
**`getpid`** as its syscall - so the tracee has been resumed with `PTRACE_SYSCALL`,
has reached the entry of `getpid`, and is stopped there while the tracer waits for a
stop that is never reported. That is the *second* of the two candidates this section
had left for strace's desynchronisation (a stop that is missing, rather than one
whose operation byte is wrong), and it reproduces: the same pin passed in an earlier
run and hung in this one, so the delivery/report of a syscall stop between
`PTRACE_SETOPTIONS(PTRACE_O_TRACESYSGOOD)` and the first `PTRACE_SYSCALL` is racy.

The pin is therefore **reverted rather than landed**: it hangs the sweep, and a red
default sweep says the kernel is broken - which is true, but the fix for *this* one
is not written yet, and a hanging test is not a pin but a stop-the-line. The next
fix is on the stop's report side: a task parked in its syscall stop must make its
tracer's `wait4` answer, and the reading above (parent in `wait4`, tracee stopped in
the call) is the counterfactual the eventual pin will stand on. The ptrace fix that
did land (`fdc69f7`) stays: its own evidence is the smoke test on the real machine,
which the note above records.

**Where the missing report is *not*.** Reading the stop's two halves narrows the
search without closing it. `trace::park` **does** wake the tracer
(`t.sched.wake(tracer)`) before sleeping the tracee with no deadline, and the
collect side's predicate (`child_event_in`) asks what it should: the stop is
unreported, and for a traced stop it is reportable only to the task recorded as its
tracer (`task.trace.tracer == Some(me)`, which the same function's comment already
explains is what keeps a parent from collecting a born-stop its tracer has not
looked at).

So neither the wake nor the filter is obviously wrong, and the hang must be in
between: the tracer woke, asked, was told there is nothing, and parked again - which
means the question it asked was not the one `child_event_in` answers. The reading
that would close it is one line in `wait4`'s wake path: the child id it is waiting
on, the result its predicate gave after the wake, and the child's `stop.reported`
and `stop.traced` at that instant. That is the cheapest instrument left for this
bug, and until it is taken the fix would be a guess about which of the three
functions in the chain (`park`, the wake, `child_event_in`) is the one losing the
event.

**The gate line, finally taken - and it is red for two things this thread did not
touch.** On a clean worktree at the ptrace commits (no other worker's changes, pin
reverted), the x86_64 arm ran every lane to completion and then failed the arm:

    User tests: 210 programs over 6 lane(s)
    lane 0..5: 35 programs each, exit 33                       (every lane concluded)
    self-test (x86_64): mpos-kunit 32 passed, 0 failed

with two claims red inside the lanes:

    lane 1: FAIL:  clone(CLONE_NEWCGROUP) starts a child, view-rooted where its parent is grouped
    lane 5: FAIL:  the reply wakes the epoll wait
            FAIL:  the echo reply reads back        (t-epoll-socket, exit=1)

Neither is the ptrace change: `PTRACE_GET_SYSCALL_INFO` answers an operation byte,
and these are a cgroup-view assertion in `t-clone` and a socket/epoll wake in
`t-epoll-socket`. So **the gate at this commit was red before this thread's work**
(the earlier "self-test failed" runs were these, not the lane timeouts), and the
ptrace fix cannot take a green line until they are green or understood.

What that means for the ledger: the ptrace fix's evidence is the smoke test on the
real machine (strace of `/bin/true` and `strace -f` of `sh -c true`, both exit 0 with
complete traces, where before the fix the same attempt reported
`ptrace_syscall_info.op == 2` and killed its tracee). The two reds above are the next
work - each wants its own reading: `t-clone`'s cgroup assertion against what
`CLONE_NEWCGROUP` is supposed to root, and `t-epoll-socket`'s wake against the
socket's `poll_subscribe`/wake path (which is the same wake machinery this whole
section has been chasing).

**`t-epoll-socket` is read, and the wake was never broken: the fixture asked an
address the emulated wire cannot answer (measured).** The reading came from the
gate log's own per-claim lines rather than any new instrument, once a run was
allowed to keep its lane logs (`build/test-lane-5/serial.log`, the x86_64 arm at
`f83c948`, repeated on a clean worktree to the same verdict - so it is
reproducible, not a flake):

    ok:    the raw ICMP socket opens          ok:    the child is reaped
    ok:    epoll_create1 hands back ...       ok:    the child exited clean
    ok:    ADD of the socket succeeds         FAIL:  the reply wakes the epoll wait
    ok:    fork succeeds                      FAIL:  the echo reply reads back

`the child exited clean` is the decisive one: the child `_exit`s 0 only when
`sendto` returned **28**, so the echo request *was* handed to the machine whole,
and the reply never came back. That separates the two halves the verdict line
could not - it is not the wake path losing an event the stack had, it is the
stack never being given one.

Why, measured rather than argued. A raw socket here lives on eth0 and never on the
loopback interface (this file's two-fabrics note: one socket cannot serve two
sets, and a raw socket's destination is a per-call argument), so an echo aimed at
`127.0.0.1` leaves by the virtual wire and is slirp's to answer - and QEMU's
user-mode backend does not carry ICMP at all (`Documentation/Networking`:
*"ping (ICMP) will not work"*). The packet is dropped host-side and no reply can
arrive whatever this kernel does. Confirmed end to end on the gate's own test
machine, one program in one lane of the sweep's own boot wiring, by changing that
one address and nothing else:

| peer the child pings | claims |
|---|---|
| `127.0.0.1` (what the test had) | `pass=0 fail=1` - the two reply claims red, the other seven green |
| `10.0.2.2` (slirp's gateway, which it answers for the host's loopback) | `pass=1 fail=0`, **9/9**, exit 33 |

Both runs are the same kernel binary and the same initramfs apart from that
constant, and the second is `=== END`-clean. So the wake this program exists to
pin - a parked `epoll_wait` ended by an arriving IP datagram, which is the
regression that once cost the network stack its frames - answers correctly; what
was wrong was the test's choice of address, and the earlier "9/9 in batch 34" was
a measurement of a different wire, not a regression since. The program's header
now says which address is asked and why, carrying this table, so the next reader
does not re-open the wake path for it. (The sibling question - whether slirp
answers the *gateway* under every host's QEMU build - is the fixture's now-named
dependency, the same shape `t-dhcp`/`t-dns` carry by living in the `external`
group; it is measured green on this host and the program names the address it
needs rather than assuming one.)

**The green gate line, taken at last - and the second red was not one.** With the
`t-epoll-socket` fixture corrected, a clean worktree at that commit and the x86_64
arm run whole:

    User tests: 210 programs over 6 lane(s)
    lane 0..5: 35 programs each, exit 33
    self-test (x86_64): pass=210 fail=0 (210 built)
    self-test (x86_64): mpos-kunit 32 passed, 0 failed
    exit 0

So the ptrace fix (`fdc69f7`) now stands on a green arm rather than only on the
`strace` smoke test, and `t-ns-mnt`'s cgroup claim - the ledger's other red - is
green in the same run: it is a **flake of the same lane-and-one-claim shape** the
`t-posix` priority claim has (`docs/notes/` §6.18's entry, and this file's
known-traps row), not a live bug, and a re-run is the answer to it as it is there.

One thing the two reds' reports *did* turn up, worth a line because it is a
weakness no claim currently catches: `t-ns-mnt`'s cgroup claim forks through the
raw `libc::syscall(SYS_clone, flags, 0)` - **two** arguments - while the same file's
other four clone claims go through its own `clone_raw`, which passes all five
(`flags, stack, parent_tid, child_tid, tls`). x86_64's `clone` reads five registers,
so the three the short call does not name are whatever the variadic trampoline left
in them: a garbage `child_tid` pointer is a birth that fails or faults for a reason
the claim would report as the kernel refusing the namespace. That is the shape of a
nondeterministic single-lane failure, and it is the first thing to rule out if the
claim goes red again - the fix, when it is taken, is to spell the call the way the
file's own helper already does.

**The cgroup claim's one-lane red, found and closed - and it was the test answering
both ways.** Running `t-ns-mnt` alone on the gate's own test machine turned the
earlier reading on its head: the claim printed **both** lines,

    ok:    clone(CLONE_NEWCGROUP) starts a child, view-rooted where its parent is grouped
    FAIL:  clone(CLONE_NEWCGROUP) starts a child, view-rooted where its parent is grouped
    FAILED
    PASS

and the harness counted `PASS`, because `Report::finish` is what prints the verdict
and the *last* one wins. Two things were wrong and both are this file's own
lessons, missed in this one claim:

1. **The clone's door named two arguments of the five x86_64's `clone` reads**
   (`flags, stack, parent_tid, child_tid, tls`), so `parent_tid`/`child_tid`/`tls`
   were whatever the variadic trampoline left in them - a garbage `child_tid` is a
   birth that fails for a reason the claim reads as the kernel refusing the
   namespace. Every other clone in the file goes through its own `clone_raw` with
   all five; this one did not.
2. **The child was never told to leave.** This clone is process-shaped (no stack),
   so the child returns from `clone_raw` and runs the parent's remainder - printing
   the claim a second time with `cloned_cgroup == 0`, i.e. a `FAIL` from the child
   beside the parent's `ok`. The file's two branches above already exit from the
   child (`libc::syscall(SYS_exit, 0)` then `std::process::exit(0)`); this one was
   added without that guard, which is exactly the `t-memfd-shm` rule the migration
   wrote down (a `check` after the fork is printed by both processes).

Either alone is enough to make the claim answer two ways, and which report landed
last is what a reader saw - so the 2026-10-26 gate's "one lane, one claim" red was
this, not a kernel answer and not an unrelated flake. Fixed both ways and measured
on the same machine, one program in one lane: one `ok:` for the claim, no second
line, `=== PASS t-ns-mnt`, `=== SUMMARY pass=1 fail=0`, exit 33.

The reading matters beyond this claim: a flake whose *own program* can print both a
pass and a fail is not the same thing as a flake in a service (the `t-posix`
priority and `t-clock-gettime` entries), and the two want different responses -
those are re-run and quoted green, this one had a bug to find.

The arm with **both** fixes in, on a clean worktree at `482127d`:

    User tests: 210 programs over 6 lane(s)
    lane 0..5: 35 programs each, exit 33
    self-test (x86_64): pass=210 fail=0 (210 built)
    self-test (x86_64): mpos-kunit 32 passed, 0 failed
    exit 0

So the ptrace thread that opened this section ends with a green x86_64 arm, a
named cause for each of the two reds it inherited, and no unproven fix in the
tree - which is what the section's whole ledger was for.

**The descriptor-restore candidate is measured and eliminated (`t-fd-restore`).** The
ledger above left two open questions for the `gendepends.sh` stall: which userspace
construct polls, and whether the redirect's own save/restore works. The second one is
answerable on its own and is now answered - it works - so it is off the list.

The pin is a new program, `t-fd-restore` (15 claims, ungroupped: it needs nothing
but the machine). It spells the exact shape `gendepends.sh` line 74 uses - save the
descriptor with `F_DUPFD_CLOEXEC`, `dup2` the file over it, read, `dup2` the
original back on the same number - and pins the invariant a caller depends on
rather than the mechanics `t-dup2` already covers: the round trip is symmetric, and
round N+1 starts from the state round N left. A hundred rounds, then a look at what
a *later* reader finds on that descriptor.

Measured: **15/15 on the guest, 15/15 on the host**, `=== PASS t-fd-restore`. Read
with the two host probes
(`.workbuddy/probe_fd_restore.c`, `.workbuddy/probe_pipe_ino.c`): a `for f in *;
do read a b c <"$f"; done` loop on a current Linux restores fd 0 to the shell's own
description after every iteration, and reads a different file each time - the
kernel here does the same, so **neither the restore nor a file's readiness at
end-of-file is the stall's cause**.

Three things the writing of it cost, each worth more than the claims:

1. **The first draft pinned nothing.** Deleting the restore from the loop left the
   program green, because the draft re-made the save slot from stdin every round -
   and a fresh `dup` of stdin *is* the original whatever the last round left
   behind, so the restore was redundant. Fixed by making the slot once, outside the
   loop; with that, deleting the restore turns `after the loop the descriptor is
   the shell's own stdin again` red. A claim that survives the change it names is
   the failure mode rule 4 exists for, and only the counterfactual run shows it.
2. **A pipe has no `st_ino` here.** The first fixture used a pipe for the
   "shell's own stdin", and the guest answered `st_ino == 0` where a current Linux
   answers a nonzero inode shared by both ends (`probe_pipe_ino.c`). The fixture
   claim failed, so every claim about the kernel that run was worthless - the
   program went red for a reason that was not the subject. A regular file is the
   stand-in now, and it is also the truer one: the stall's own line recorded
   `kind=0` (a file) on the descriptor.
3. **Three scripts must have three distinct first words.** The first draft had two
   beginning `#!/sbin/openrc-run`, so the "each round read the file it opened" claim
   could not tell three reads from two, and failed on a correct kernel.

Where that leaves the stall: the two questions this could answer are answered and
both are negative - the restore works and `read` at end-of-file is 0. What remains
is the one the ledger named first, and it is now the *only* candidate: which
userspace construct in busybox's `read` builtin blocks its signal mask
(`rt_sigprocmask`, syscall 14) and polls a regular file, and why it re-polls instead
of consuming to end-of-file. That is a question about the shell, and it is
answerable by reading busybox's `read` rather than this kernel.

The arm with `t-fd-restore` in it, on a clean worktree at `9161020`:

    User tests: 211 programs over 6 lane(s)
    lane 0: 36 programs, exit 33   (the new one rides lane 0)
    lane 1..5: 35 programs each, exit 33
    self-test (x86_64): pass=211 fail=0 (211 built)
    exit 0

one more than the 210 of the run before it, which is the added program and
nothing else.

**The userland candidate narrowed to the shell's own binary, measured off the image.**
With the kernel side eliminated, the remaining question was which userspace construct
polls - and the image answers most of it without a boot:

* the shell is **busybox 1.38.0** (`strings /bin/busybox`: `BusyBox v1.38.0
  (2026-10-25 19:58:26 UTC)`), and the *same binary* run here on a current Linux
  completes the loop shape cleanly - `for f in *; do read a b c <"$f"; done` over
  three scripts, `iterations=3`, rc=0 - including with openrc's own fd dance in
  front of it (`exec 3>&1 1>&2`, stdin `/dev/null`). So the shell, the loop and the
  redirect are all sound; nothing about the construct is wrong on a correct kernel.
* `gendepends.sh` itself (dumped from the image's root partition, 130 lines) has
  exactly one wait-shaped line - line 74's `read one two three <"$RC_SERVICE"` - and
  the script it heads with is `#!/bin/sh`, so its own `case` arm at 78-89 is the
  `#!` one.
* openrc is **0.63.2** (`/sbin/openrc`'s strings carry `0.63.2`, `RC_VERSION`, and a
  `poll: %s` error path beside `fork: %s`), so the `rt_sigprocmask` → `poll` pair the
  spin line recorded is openrc's own child-waiting, not a shell construct.

That last point is the reading that matters: `rt_sigprocmask` immediately before a
`poll` on a *file* descriptor is what a parent does around waiting for a child with a
saved mask - and openrc forks (`fork: %s`, `waitpid: %s`, `pipe2: %s` all in its
strings). So the spin is most likely openrc's own wait loop over a `pipe2`/`poll`,
with `fd=0` in the instrument's line naming the *inherited* descriptor rather than
the one the poll is on, and the mask dance being `sigprocmask`-around-`poll` the way
`pselect`/`ppoll` spells it.

What is *not* yet measured is which of those it is, and the difference matters: an
openrc-side loop is userspace's to fix, while a `ppoll` whose mask handling this
kernel gets wrong is ours. The one instrument that separates them is already in the
tree - the poll line prints the descriptor *and* the syscall before it - and it needs
`fd=` to be the number the poll was actually called with, which the next step is to
check it prints (it currently reports the *watched* descriptor, which for a `ppoll`
with a one-element array is what we want, but for a multi-descriptor poll would be
the first entry only). Until that is confirmed, the ledger's shape stops here rather
than guessing between the two.

The arm behind that instrument change, on a clean worktree at `35533e9`:

    lane 0: 36 programs, exit 33 · lane 1..5: 35 each, exit 33
    self-test (x86_64): pass=211 fail=0 (211 built)
    self-test (x86_64): mpos-kunit 32 passed, 0 failed
    exit 0

**The block layer's deadlock, read off the wire: a used-ring head nobody pops.** The
boot on a clean worktree at HEAD does **not** reach a login - 400 s of `cargo xtask
run` times out with the wire silent after `Populating /dev ... [ ok ]` - and the two
instruments that were finally pointed at it name the failure exactly.

The first of them was missing until now. The heartbeat fired only from the syscall
gate and the preempt path, so a machine where **every** task is parked printed
nothing at all, which is precisely the shape a deadlock has. Firing it from
`wait_for_interrupt` as well fixed that, and the very next boot said:

    mpos: heartbeat fill-owner=slot 10/pid 10 alive=true exited=false
                    fill-busy=true blk-busy=true blk-waiter=slot 10
    mpos:   fill=8805/8803(out=2 age=111304ms slot=10 syscall=4) run=0/0(out=0)
    mpos:   slot 10 pid 10 /sbin/udevd  park 4 (block_until_uninterruptible) parks=31670
    mpos:   slot 11..18 pid 11..18 /sbin/udevd  park 7 (ext4_slot_wait) parks=3..1670

so one udevd worker had held the ext4 fill claim for **111 seconds** while eight
siblings queued behind it, and it was itself parked waiting on a block transfer.
`parks=31670` at a 4 ms period is that task re-parking on the loss-recovery poll.

The second instrument says why the transfer never ends - the wait loop's own report,
printed once when it has gone 100 periods without an answer:

    mpos: blk stall: me=8 token=1 rounds=50  ring=Some(0) busy=true waiter=8
    mpos: blk stall: me=8 token=1 rounds=1000 ring=Some(0) busy=true waiter=8

**`token=1`, `ring=Some(0)`.** The submitted request owns token 1; the used ring's
head holds **token 0**, and it never moves. `virtio-drivers`' `pop_used(token, ..)` is
strictly FIFO - it reads `used.ring[last_used_idx]` and answers `Error::WrongToken`
when the id there is not the token asked for, and it advances `last_used_idx` only on
a successful pop. So one entry completed but left unpopped pins the head, and every
later transfer waits for a token that can never arrive. `peek_used() == Some(token)` -
the whole of this driver's wait condition - can never become true again, while
`busy=true waiter=8` means nothing else can even try. It is a self-lock: the owner of
the mailbox is the task waiting for the head to clear.

Which completion was left unpopped is not yet measured, and the shape the next
reading wants is one line where the pop happens, printing `peek_used()` afterwards:
the pop is inside `critical_section` after the wait, so a leftover there means the
device completed an entry the driver never saw (an abandoned submission from a
`transfer_sync` caller, or a completion delivered for a request whose task left the
loop by a path that does not pop). What is already certain is that the ring cannot
recover on its own: nothing in this driver pops an entry it did not submit, so a
single stray completion wedges the disk for the life of the boot.

Why the previous fixes did not reach it, which is the honest accounting: `93865d7`
and `4e927ce` both belong to the ext4 **claim** layer, and in this boot that layer is
healthy - `claim registrations=7041 handed-back=7041`, `claim-misses=0`. The stall was
one level *below* the claims, in the mailbox and the ring they sit on, and it only
became readable once the heartbeat could fire on an all-parked machine.
