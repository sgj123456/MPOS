# §7 Phase 7: Device Drivers — **7.1, 7.2 and 7.3 are done; §7.4 has two steps left**

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

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
host draws the pointer instead, so `paint_cursor` is skipped (`host_cursor_live()` gates
the one call site) and a move costs one small cursor-queue command rather than a
full-frame gather and flush. The `ARGB8888` in that sentence is the format the fourcc
correction above had to land first.

**One gap in this area is named rather than filled.** An unknown CRTC in
`MODE_CURSOR` is `EINVAL`, which is what this file has always answered for an
object it cannot use, where Linux's `drm_crtc_find` says `ENOENT`; the whole file
would have to move together for that to change, so the cursor follows the file
rather than splitting it. A second gap is named by necessity rather than choice: both
configured machines' virtio-gpu devices expose a cursor queue, so the no-cursorq
software-fallback branch cannot be exercised on this host - it is kept because a
device without a second queue needs it, and it is stated as untested-here rather than
claimed as measured.

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
atomic cursor path is complete by inspection; reading the pointer back on the window, and
settling whether weston upscales its image to the cap this card answers (see R.7), is what
closes it.

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

**What is still open, and it is a size rather than a smell.** A function's configuration
space is 4 KiB and this kernel still publishes `CFG_SIZE` = 256 - the *port* mechanism's
reach - so PCIe extended capabilities are outside every reader even now that the window
could reach them. Deliberately not joined to this one: a `/sys/.../config` file's size would
start depending on which mechanism the firmware named, and "the window makes it possible" is
not "the window makes it right". It is a `docs/ROADMAP.md` item of its own.


---

### R.7 (moved from ROADMAP's status board) — the weston seat chain, the compositor's cursor and console, and the gaps that have since closed

**`start-weston.sh` survives being run twice.** It asks whether a seatd is *answering* rather than whether `/run/seatd.sock` exists (a socket path outlives the daemon that bound it, here as on Linux), removes the stale name before a new daemon tries to bind it, and starts seatd in its own session: the script `exec`s into weston, so a seatd backgrounded there shared the compositor's process group and `^C` on the console — which signals the whole foreground group — used to kill the seat manager along with the desktop.

**A parked evdev reader — direct, `select`, or an `epoll` interest like a compositor's — is woken by the service task when a record lands**, instead of at the next tick; the wait re-checks the queue as it parks, so an event between a failed read and the sleep cannot be lost.

**`--mode gui` keyboard rule, stated exactly.** A keystroke reaches the active console's shell unless a compositor is *both* grabbing the device and holding the screen; that conjunction, not the grab alone, is what takes the keyboard off the shell, so a plain GUI console (nothing has grabbed the device) is typable again — as is the same console after `^A` moves its session off the screen while a stale claim survives (§7.1, `input::service`).

**The pointer composited by the one flusher**, in a host-tested pure function (`crates/gpu/src/cursor.rs`, 10 host tests); `CRTC_X`/`CRTC_Y` steer rather than merely record (`t-drm-props`), and on a device with a cursor queue the host draws the pointer instead - `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` is accepted and `HOTSPOT_X`/`HOTSPOT_Y` published behind it, and binding or moving the plane posts virtio-gpu `UPDATE_CURSOR`/`MOVE_CURSOR` (§7.4). The software blend remains the no-cursorq fallback.

**The two blobs the property families point at, as pure encoders** in `crates/gpu/src/blob.rs` with 15 host tests — `drivers/drm.rs` is kernel-only, so the bytes a real reader parses had to live somewhere a host can check them. Verified against `edid-decode` rather than only against the module's own decoder, which caught a real bug (a descriptor's first two bytes) (§7.4).

**Gap closed — the aarch64 session environment.** A login shell on the aarch64 image had no `XDG_RUNTIME_DIR` (measured 2026-10-11) and weston started from it stopped at `fatal: failed to add socket`. The files that provide it now ship in the shared `core/` overlay, so both machines' login shells have it (§15.36/§15.37).

**Gap closed — weston's EL0 write fault, and the cursor it was hiding.** With a runtime dir weston used to reach `Compositor capabilities:`, create `wayland-1`, load `desktop-shell.so` and launch both helper processes, then take a genuine EL0 **write** fault one page above its stack pointer (`ESR 0x92000047`, a level-3 translation fault with `WnR` set) that the resolver refused for want of a VMA (§15.36's last note). The desktop now comes up whole: the fault is resolved by `resolve_stack_growth` on the aarch64 EL0 data-abort path (`arch/src/arch/aarch64/vectors.rs`, alongside the COW and anon resolvers), so the push past the stack boundary grows the stack instead of ending the task. What that boot then showed was the pointer never appearing even though the desktop rendered — and two causes were behind it, both now fixed. First, this card's cursor plane had failed to publish the rectangles weston names unconditionally; §7.4 carries the measurement and the fix (the plane publishes `SRC_*`/`CRTC_W`/`CRTC_H` and honours them as identity-or-refuse). Second, and the reason a *correctly bound* plane still drew nothing on a virtualized display: the card refused `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` while still enumerating the cursor plane, so weston reported `cursor planes: yes`, bound the plane through atomic, and the host - which only draws a virtualized cursor handed a hotspot-correct image over the virtio-gpu cursor queue - showed nothing. The card drove its own software blend into the scanout and never configured the device's cursor queue at all. The fix routes the pointer to the host: the cursor queue is configured when the device has one (probed at bring-up, published as `Card::cursor_q`), the hotspot cap is accepted and `HOTSPOT_X`/`HOTSPOT_Y` are published per-client, and binding or moving the plane posts `UPDATE_CURSOR`/`MOVE_CURSOR` on the cursor queue (`Job::SetCursor`/`MoveCursor`) so the host composites the pointer over whatever frame the guest presents; the software blend remains only as the fallback for a device with no second queue. Pins: `t-drm-props` proves the cap + hotspot enumeration gating, `t-drm` proves an `RMFB` of the cursor's framebuffer takes the plane's binding down (no dangling host resource). **Window confirmation is the remaining step:** the kernel path is complete and gated green on the host and both bare-metal builds, but reading the pointer back on the QEMU window after a `cargo xtask run --mode gui` weston boot is what closes this gap for real, and the no-cursorq fallback branch cannot be exercised on either configured machine (§7.4 names that as a gap).
