//! The console multiplexer: one serial wire, six terminals.
//!
//! The machine has one wire to the world - COM1, `ttyS0`, where the kernel's
//! `tracing` log goes. The shells do not run there; they run on the virtual
//! consoles `/dev/tty1`..`tty6`, the slave ends of pty pairs. Something has to
//! stand in between, and this is it: it carries what the active console's
//! shell prints out to the serial wire, and it carries what is typed on the
//! serial wire in to that shell. Six terminals, one wire, in turn.
//!
//! # Switching
//!
//! `^A` followed by a digit `1`..`6` makes that console the active one, the
//! convention GNU screen taught every terminal veteran. `^A` typed twice
//! arrives as one `^A` at the shell, because a shell that cannot receive the
//! byte its key sent is not usable; any other byte after a lone `^A` sends
//! the `^A` on first and the byte after it, so `^A`-then-command is never
//! eaten.
//!
//! # Why the mux is a task
//!
//! Output has to be pumped: the slave prints, the bytes pile up on the master,
//! and only a task can carry them out - an interrupt handler cannot block, and
//! there is nothing else running at handoff time. The task parks until a
//! master has something, and the masters wake it the same way they wake any
//! reader, so an idle console costs nothing.
//!
//! # Switching is a negotiation, not an assignment
//!
//! A console whose session has put it in `VT_PROCESS` is asked before it is
//! switched away from: the mux sends that session its `relsig`, leaves the
//! screen where it is, and the switch completes when the session answers
//! `VT_RELDISP`. The policy - which console is being asked, which way a refusal
//! counts, how long an answer is worth waiting for - is
//! [`crate::vfs::vtmode`], which holds no clock, no tasks and no signals
//! precisely so a host build can test it. This file is where those meet the
//! machine: the table lives in a critical section, and every decision it
//! returns is *executed* after the section closes, because signalling a process
//! group and waking a parked ioctl both go through the task table, and the task
//! table takes a critical section of its own.
//!
//! That ordering is also why the mux task keeps the deadline: `^A 3` arrives
//! from the serial interrupt handler, which may queue a signal but must not
//! park, and a session's `VT_ACTIVATE` runs through the same
//! [`request_activate`] door - so a switch nobody answered is finished by the
//! one task that is always looking, and a session's activation and an
//! operator's keystroke cannot disagree about what switching means.
//!
//! # Why the wait cannot be left to the session
//!
//! A compositor that is killed, wedged or simply uninterested never answers
//! `VT_RELDISP`, and a screen held by a promise nobody will collect is a machine
//! you cannot read. So the mux parks with a deadline rather than forever
//! ([`crate::vfs::vtmode::RELEASE_TIMEOUT_TICKS`]) - and the deadline is the
//! backstop, not the answer: a second `^A` to the console already being asked
//! forces the switch through on the spot, by hand, at any time.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex as CsMutex;

use crate::sched::policy::TaskId;
use crate::term::vc::{K_XLATE, KD_GRAPHICS, KD_TEXT, VC_COUNT, active, set_active};
use crate::term::vtmode::{self, Decision, VtMode};

/// Each console's video mode (`KD_TEXT`/`KD_GRAPHICS`) and keyboard mode
/// (`K_XLATE`, `K_OFF`, ...), indexed 1-based like the consoles themselves.
///
/// Two arrays rather than a small struct per console because each is read on a
/// path that already exists and must not take a lock to find out: the render
/// task asking whether this console should be painted at all, and the mux
/// asking whether a keystroke belongs to a shell. `KDSETMODE` and `KDSKBMODE`
/// write them, which is what makes a seat's handover mean something instead of
/// being a request that was accepted and forgotten.
static KD_MODE: [AtomicU32; VC_COUNT as usize + 1] =
    [const { AtomicU32::new(KD_TEXT) }; VC_COUNT as usize + 1];
static KB_MODE: [AtomicU32; VC_COUNT as usize + 1] =
    [const { AtomicU32::new(K_XLATE) }; VC_COUNT as usize + 1];

/// The slot a console's mode lives in, or `None` for a number that is not one
/// of the consoles this machine has.
fn slot(vc: u32) -> Option<usize> {
    if vc == 0 || vc > VC_COUNT {
        None
    } else {
        Some(vc as usize)
    }
}

/// `KDSETMODE`: put a console in text or graphics mode. Refused for a console
/// that does not exist.
pub fn set_kd_mode(vc: u32, mode: u32) -> bool {
    match slot(vc) {
        Some(at) => {
            KD_MODE[at].store(mode, Ordering::Relaxed);
            // Taking the screen back is a change the render task has to notice
            // even though no console was switched: the framebuffer holds
            // whatever the compositor last drew, so the whole grid goes stale.
            crate::drivers::video::render::notify();
            true
        }
        None => false,
    }
}

/// `KDGETMODE`, and the answer for a console that is not one of ours is text -
/// there is nothing there to have taken the screen.
pub fn kd_mode(vc: u32) -> u32 {
    match slot(vc) {
        Some(at) => KD_MODE[at].load(Ordering::Relaxed),
        None => KD_TEXT,
    }
}

/// `KDSKBMODE`: which translation this console's keyboard is in.
pub fn set_kb_mode(vc: u32, mode: u32) -> bool {
    match slot(vc) {
        Some(at) => {
            KB_MODE[at].store(mode, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// `KDGKBMODE`, defaulting to `K_XLATE` as a console Linux never heard of would.
pub fn kb_mode(vc: u32) -> u32 {
    match slot(vc) {
        Some(at) => KB_MODE[at].load(Ordering::Relaxed),
        None => K_XLATE,
    }
}

/// The console a text VT may be drawn on, as a yes/no.
///
/// The one veto is a session that has both the card and the screen: its
/// `KD_GRAPHICS` console is the thing the compositor is painting, and drawing a
/// text grid over it is what a login prompt did to weston one keystroke at a
/// time. Anything else - no card open, a card whose master has no console
/// because its holder never took one, a master on a console that is not on the
/// screen - is the text console's, as it has always been.
pub fn painted() -> bool {
    !master_holds_screen()
}

/// Whether some session holds the card *and* holds the screen on the console it
/// took it on. `false` means the scanout is the text console's to draw.
pub fn master_holds_screen() -> bool {
    match master_vc() {
        None => false,
        // ...and only over the console it is on: a session that took the card
        // and then went away without ever putting its console in graphics mode
        // has a claim on the card, not on anybody's screen.
        Some(vc) => active() == vc && holds_screen(vc),
    }
}

/// Whether console `vc` is the one the DRM master session holds the screen on:
/// it has to be the console the master was taken on *and* be on the screen *and*
/// have been put in graphics mode by whoever is driving it.
///
/// All three, because each catches a different way a session can be granted a
/// screen it does not have: master on a console nobody activated, graphics mode
/// on a console that never asked for the card, an activation that never told
/// the card. A session that owns the screen is the only one the compositor's
/// frames are for; everything else belongs to the text console.
pub fn holds_screen(vc: u32) -> bool {
    master_vc() == Some(vc) && active() == vc && kd_mode(vc) == KD_GRAPHICS
}

/// The console a seat manager has made the DRM master's, 0 for nobody.
///
/// `DRM_IOCTL_SET_MASTER` is the moment a session says "this screen is mine to
/// drive", and the answer has to be recorded somewhere the *card* can be asked
/// about later: a compositor that loses its console must find its page flips
/// refused, and the only fact that says which console it was driving is the one
/// written here. It belongs to the open file description, not the process - a
/// seat manager hands the card to the compositor as a descriptor, and on Linux
/// the master travels with it - so the console is bound once, at the moment
/// master is taken, and survives the fd arriving somewhere else.
static MASTER_VC: AtomicU32 = AtomicU32::new(0);

/// Record, or clear with 0, the console the DRM master is on.
pub fn set_master_vc(vc: u32) {
    MASTER_VC.store(vc, Ordering::Relaxed);
}

/// The console the DRM master session is on, or `None`.
pub fn master_vc() -> Option<u32> {
    match MASTER_VC.load(Ordering::Relaxed) {
        0 => None,
        vc => Some(vc),
    }
}

/// The clock the negotiation measures in. `vtmode` deliberately takes a tick
/// rather than owning one, so the caller is this file.
fn now() -> u64 {
    crate::hal::ticks()
}

/// The handover table. `None` until [`init`] has run, which is before any
/// session exists: a `VT_GETMODE` that finds no table is a console that has not
/// been created yet, and answers the same `VT_AUTO` a fresh one would.
static VT: CsMutex<core::cell::RefCell<Option<vtmode::Table>>> =
    CsMutex::new(core::cell::RefCell::new(None));

/// Run `f` over the table, if there is one. Every read of a console's mode goes
/// through here, and every write leaves its *effect* to the caller: this returns
/// a value, it never signals, switches or parks.
fn vt_with<T: Copy>(f: impl FnOnce(&mut vtmode::Table) -> T) -> Option<T> {
    critical_section::with(|cs| VT.borrow(cs).borrow_mut().as_mut().map(f))
}

/// `VT_SETMODE`: record what console `vc`'s session promises about the screen,
/// and the process (`pid`) whose promise it is.
///
/// `None` is `EINVAL`: not one of this machine's consoles. `Some(Err(_))` is the
/// mode itself being nonsense (an unknown `mode`, a signal number that is not a
/// signal, `relsig == acqsig`), and a console with a stale promise is not
/// allowed to keep it - see [`vtmode::Table::set_mode`].
///
/// The pid is not decoration. Linux keeps it as `vc->vt_pid` and sends both
/// handover signals to *that process* (`kill_pid(vc->vt_pid, …)`), which is what
/// lets a seat manager write the mode on a console and close the descriptor
/// before the compositor ever opens one. Aimed at the terminal's foreground
/// group instead, `acqsig` - which every seat manager names as `SIGUSR2` - kills
/// whichever session happens to be in the foreground: the compositor leaving on
/// its own terms, or a shell that never asked to be consulted.
///
/// A mode that takes a console *out* of graphics ownership has to be repainted
/// now, exactly as `KDSETMODE` does: the scanout holds whatever the compositor
/// last drew, and nothing else will tell the render task it may have it back.
pub fn set_vt_mode(vc: u32, mode: VtMode, pid: u32) -> Result<(), Option<vtmode::ModeError>> {
    let outcome = vt_with(|t| t.set_mode(vc, mode, pid));
    match outcome {
        None => Err(None),
        Some(result) => {
            if result.is_ok() {
                crate::drivers::video::render::notify();
            }
            result
        }
    }
}

/// `VT_GETMODE`: a console this machine has never heard of is `VT_AUTO`, which
/// is the honest answer about a console that does not exist.
pub fn vt_mode(vc: u32) -> VtMode {
    vt_with(|t| t.mode(vc)).flatten().unwrap_or(VtMode::AUTO)
}

/// `VT_ACTIVATE`, and `^A <digit>`: ask for console `to` to be on the screen.
///
/// **Neither of them waits, and that is Linux's answer rather than a shortcut.**
/// `VT_ACTIVATE` is `set_console()`: it records `want_console`, schedules the
/// console callback and returns 0 (`vt_ioctl.c`); the callback is
/// `change_console()`, which is where the releasing console's `relsig` goes out,
/// and the switch happens later - when that session answers `VT_RELDISP`, or never,
/// if it does not. So the session that asks for a screen another console is
/// holding is *not* parked on the answer, and the same call is what an operator's
/// keystroke makes, from a context that could not sleep anyway.
///
/// That is also the only shape that works for a seat manager. seatd's `vt_switch`
/// writes `VT_SETMODE(VT_PROCESS)` on the console it is standing on and then calls
/// `VT_ACTIVATE` for the one it wants - so the promise that owes the `relsig` is
/// seatd's own, and a `VT_ACTIVATE` that slept on that answer would have the keeper
/// blocked in the very call its own answer is supposed to unblock. The promise is
/// now aimed at the process that wrote it, so a waiting `VT_ACTIVATE` would have
/// deadlocked the seat manager for the whole length of the release deadline.
///
/// `false` says the number is not one of this machine's consoles, which is what
/// Linux answers `ENXIO` for.
pub fn request_activate(to: u32, force: bool) -> bool {
    match vt_with(|t| t.request_switch(to, now(), force)) {
        Some(Some(decision)) => {
            apply(decision);
            true
        }
        // `None` inside: not one of this machine's consoles. `None` outside: no
        // console mux yet, which is the same answer from a session's side.
        _ => false,
    }
}

/// `VT_WAITACTIVE`: park until console `want` is the one on the screen.
///
/// Registered on the same list the release acks and the timeout force, because
/// the wait ends exactly when the switch does: a session that asked to be told
/// when it got the screen must be woken by the *answer*, not by the next tick
/// somebody else happens to cause.
pub fn wait_active(want: u32) {
    let me = crate::sched::current();
    add_active_waiter(want, me);
    crate::sched::block_until_deadline(|| active() == want, vt_deadline());
    remove_active_waiter(me);
}

/// The tasks parked per-console in `VT_WAITACTIVE`: Linux's `vt_event_waitqueue`
/// entry for a `VT_EVENT_SWITCH`, which is the one wait the console layer really
/// does sleep in. `VT_ACTIVATE` is not on that list, because Linux's
/// `VT_ACTIVATE` never sleeps - see [`request_activate`].
static ACTIVE_WAITERS: CsMutex<core::cell::RefCell<Vec<(u32, TaskId)>>> =
    CsMutex::new(core::cell::RefCell::new(Vec::new()));

fn add_active_waiter(want: u32, id: TaskId) {
    critical_section::with(|cs| {
        let mut v = ACTIVE_WAITERS.borrow(cs).borrow_mut();
        if !v.iter().any(|&(n, w)| n == want && w == id) {
            v.push((want, id));
        }
    });
}

fn remove_active_waiter(id: TaskId) {
    critical_section::with(|cs| {
        ACTIVE_WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|&(_, w)| w != id)
    });
}

/// Linux's `vc_wakeup_proc`: the negotiation reached an answer, so every task
/// that slept for it gets to look again. The lists are *not* drained here - a
/// sleeper re-checks its predicate and either returns (removing itself) or
/// parks again on the same entry, which is what keeps a wake from having to
/// race the re-registration.
fn wake_waiters() {
    let active: Vec<TaskId> = critical_section::with(|cs| {
        ACTIVE_WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .map(|&(_, id)| id)
            .collect()
    });
    for id in active {
        crate::sched::wake(id);
    }
    wake_task();
}

/// `VT_RELDISP`: console `vc` answers the request outstanding on it.
///
/// The return is the ioctl's: Linux answers success when the console was
/// indeed being asked - and agreed, which is what moved the screen - and
/// `EINVAL` when it was not. A refusal is a *successful* answer that changes
/// nothing, exactly as it is on Linux: `VT_RELDISP(0)` means "I am not letting
/// go", and the requester learns so from `VT_GETSTATE`, or from its `frsig`.
///
/// Only that console may answer, and only once.
pub fn release_ack(vc: u32, agree: bool) -> bool {
    if slot(vc).is_none() {
        return false;
    }
    match vt_with(|t| t.release_ack(vc, agree)) {
        Some(vtmode::Ack::Complete { to, .. }) => {
            complete_switch(to);
            true
        }
        // A refusal is a Linux `VT_RELDISP(0)`: the answer is "no", the ioctl
        // succeeds, and the requester wakes to find `-EBUSY` rather than sleep
        // out a timeout nobody is going to answer inside.
        Some(vtmode::Ack::Refused { .. }) => {
            wake_waiters();
            true
        }
        Some(_) | None => false,
    }
}

/// A session's last descriptor of console `vc` closed: take its word about the
/// *keyboard* back, so a `K_OFF` left behind by a process that has gone cannot
/// leave a respawned getty reading nothing.
///
/// What this does **not** do is drop the console's `VT_PROCESS` promise, and it is
/// worth saying why in the place where a reader would otherwise put it. A promise
/// belongs to the process that wrote `VT_SETMODE`, not to the descriptor, and the
/// process that writes it closes that descriptor on purpose: seatd's `vt_open` is
/// open, `VT_SETMODE(VT_PROCESS)`, `KDSKBMODE`, `KDSETMODE`, `close(ttyfd)` - all
/// before the compositor it serves has the screen. Clearing the promise here, even
/// only when the closer is the keeper, would unmake the claim in the same breath as
/// it is made, and the compositor's frames would be back to racing the text console
/// for one scanout. Linux ends a promise in two places and neither is a close: the
/// keeper writes `VT_AUTO` (`vt_close`), or `kill_pid` fails at the next handover
/// and `reset_vc` runs - which is [`reset_console`].
pub fn abandon(vc: u32) {
    set_kb_mode(vc, K_XLATE);
    wake_waiters();
}

/// The tick the release in flight stops being worth waiting for, or `u64::MAX`
/// when the screen is not promised to anyone. The mux parks on this.
pub fn vt_deadline() -> u64 {
    vt_with(|t| t.deadline()).flatten().unwrap_or(u64::MAX)
}

/// Carry out what the table decided. Kept apart from the decision so the
/// critical section is closed before anything touches the task table.
fn apply(decision: Decision) {
    match decision {
        Decision::SwitchNow { to } => complete_switch(to),
        // Asking for the screen you already have moves nothing; it only lifted
        // a refusal, which the table has already done.
        Decision::Already { .. } | Decision::Waiting { .. } => {}
        // Nothing is sent for a refusal. Linux ignores `frsig` - it writes zero
        // over the field at `VT_SETMODE` time - and the session that asked is the
        // one calling `VT_ACTIVATE`, which reads the refusal off its own return.
        Decision::Refused { .. } => {}
        Decision::BeginRelease { release, relsig } => {
            // A console that cannot be asked cannot be waited for either: no
            // signal named, or no process left holding the promise. Linux's
            // answer to the second is `kill_pid` failing and `reset_vc` running,
            // after which the switch is a plain one - so the table's deadline is
            // not what ends this wait, the absence of a keeper is.
            let Some(promise) = relsig else {
                reset_console(release.from);
                complete_switch(release.to);
                return;
            };
            if !send_promise(promise) {
                reset_console(release.from);
                complete_switch(release.to);
            }
        }
    }
}

/// Hand a promise its signal - to the process that wrote `VT_SETMODE`, and to
/// nothing else - and answer whether there was anybody left to hand it to. That
/// answer is the whole of how the console layer learns its promise has no keeper:
/// Linux gets it from `kill_pid`'s return value for the same reason.
fn send_promise(promise: vtmode::Promise) -> bool {
    crate::sched::send_signal_process(crate::sched::TaskId(promise.pid as usize), promise.sig)
}

/// Linux's `reset_vc`, as far as this layer owns it: the keeper is gone, so the
/// console stops claiming a screen and starts being drawn on again.
///
/// The `KD_TEXT` is the half that matters to the person at the keyboard. Linux's
/// comment on the same failure says why: "it saves the agony when the X server
/// dies and the screen remains blanked due to KD_GRAPHICS". Without it a dead
/// compositor leaves a machine whose text console is correct, woken, and
/// invisible.
fn reset_console(vc: u32) {
    if vt_with(|t| t.reset(vc)).unwrap_or(false) {
        set_kd_mode(vc, KD_TEXT);
        crate::drivers::video::render::notify();
        wake_waiters();
    }
}

/// Whether the console session the DRM master belongs to is the one on the
/// screen, which is the question an input device asks once a tick to decide
/// where its events go.
///
/// `false` means the screen has been released - a `^A` moved to a text console,
/// and the compositor was refused its page flips on the way - and so is the
/// word that lets the keyboard and the pointer go back to the shell instead of
/// continuing to pour into a session that can no longer show them. A card with
/// nobody holding it answers `true`: an evdev grab by itself is not a claim on
/// the screen, and a test or a console program that grabs a device must not
/// find its events dropped on the floor. A holder that never named a console
/// (no controlling terminal, which is what a test program has) keeps its input,
/// because it has no screen to lose either.
pub fn session_active() -> bool {
    !crate::drivers::drm::master_held() || master_holds_screen()
}

/// The console on the screen, once a negotiation has ended by any route: the
/// session agreed, the operator forced it, or the wait ran out.
fn complete_switch(to: u32) {
    if slot(to).is_none() {
        return;
    }
    switch(to);
    // Move the pure table's notion of the on-screen console in step with the
    // `ACTIVE` atomic. `request_switch` asks "is `to` the console already on the
    // screen" and "which console's `VT_PROCESS` promise do I honour" from the
    // table's `active`; if it is left at its boot value, every switch after the
    // first negotiates against console 1 no matter what is really shown - so a
    // compositor's release signal is sent to the wrong session, or not at all,
    // and the handover the whole model turns on silently stops happening.
    vt_with(|t| t.complete_switch(to));
    // The screen moved: every task parked in `VT_ACTIVATE` or `VT_WAITACTIVE`
    // was waiting for exactly this, and the mux has a new console to pump.
    wake_waiters();
    // The session that now has the screen is told so, if it asked to be told and
    // if the process that asked is still there to be told: `acqsig` is how a
    // compositor knows to re-take the DRM master and repaint, which is the
    // recovery path after a switch away. It goes to the keeper of the promise, as
    // Linux's `vt_acquire` sends it to `vc->vt_pid` - and if that process is gone,
    // Linux resets the console rather than leaving a claim nobody holds, which is
    // what puts the text back on the screen instead of behind a dead compositor's
    // graphics mode.
    let acquire = vt_with(|t| t.promise(to, vtmode::Signal::Acquire)).flatten();
    if let Some(promise) = acquire
        && !send_promise(promise)
    {
        reset_console(to);
    }
}

/// The mux task's own look at the negotiation: called every lap, because the
/// task already has a reason to wake at least as often as the deadline could
/// come due, and a wait that nobody re-examines is a wait that never ends.
fn expire_if_due() {
    if let Some(Some(to)) = vt_with(|t| t.expired(now())) {
        complete_switch(to);
    }
}

/// Wake the pump. A negotiation, a switch or a mode change has altered what it
/// should be looking at, and `block_until_deadline` parks with no other reason
/// to wake: a condition that becomes true without a wake is a condition the mux
/// will not read until something else happens.
fn wake_task() {
    let mux = MUX_TASK.load(Ordering::Relaxed);
    if mux != 0 {
        crate::sched::wake(TaskId(mux as usize));
    }
}

/// A `^A` has been seen and the next byte decides what it meant.
static PENDING_PREFIX: AtomicBool = AtomicBool::new(false);

/// Whether the mux is holding a `^A` and waiting for its follower. A keystroke
/// source that normally withholds bytes from the mux - because a compositor
/// owns the keyboard on the screen - must still hand over *this* one byte, so
/// `^A <digit>` completes and the operator is never locked out of the console
/// they cannot see. Read-only: the mux itself is what consumes the latch.
pub fn mux_wants() -> bool {
    PENDING_PREFIX.load(Ordering::Relaxed)
}

/// The mux task's id, so an input-time switch can wake it - the task parks on
/// the old console's master, and the switch changes what it should park on.
static MUX_TASK: AtomicU32 = AtomicU32::new(0);

/// Whether the mux is up. Until it is, COM1's input belongs to the console
/// tty - the boot checks type at it directly, and the handoff happens long
/// before any shell exists.
pub fn ready() -> bool {
    MUX_TASK.load(Ordering::Relaxed) != 0
}

// The virtual console currently on the wire is `crate::term::vc::active`/
// `set_active` — a shared number, not mux state, so the console and the drivers
// read it from the leaf instead of naming this module (V-4 D.2a).

/// The operator's `^A <digit>`, from any context, including an interrupt.
///
/// Nothing here blocks: a decision either moves the screen (an atomic store, a
/// wake, and a `notify` that only sets a flag) or asks a session for permission
/// (a signal queued into the task table, which is what a keystroke's echo
/// already does from this same handler). The wait belongs to the mux task, which
/// parks to the negotiation's deadline - so an unanswered `VT_RELDISP` costs
/// nobody's keystroke, and the operator always keeps a way to read the machine.
///
/// The session's own `VT_ACTIVATE` goes through this same door and for the same
/// reason - Linux's `VT_ACTIVATE` is `set_console()`, which schedules the switch
/// and returns without waiting for the answer - so an operator's keystroke and a
/// session's activation cannot disagree about what switching means.
///
/// A second `^A` naming the console already being asked is the force that ends
/// the negotiation without an answer, which is why the request carries the
/// console it was aimed at rather than a bare "switch requested" flag. On Linux
/// this is `CtrlAltFx`, the keyboard's own switch, which is handled in the
/// console driver and is *not* subject to `VT_PROCESS`; the escape hatch is a
/// kernel-side keystroke there too, so keeping ours is alignment, not a
/// deviation - what we do not offer is a way for a *session* to force it.
fn begin_activate(to: u32) {
    let force = vt_with(|t| t.release().is_some_and(|r| r.to == to)).unwrap_or(false);
    request_activate(to, force);
}

/// `VT_OPENQRY`: the lowest console that no one has taken over as their
/// terminal, or `None` when all of them are spoken for - which is what Linux
/// answers `ENXIO` for.
///
/// "Taken over" is asked of the console's own reader: a getty that has opened
/// `/dev/tty3` and is waiting on it has claimed that console for a session, and
/// one nobody has read from is still free for a seat manager to point a second
/// session at.
pub fn free_console() -> Option<u32> {
    (1..=VC_COUNT).find(|vc| {
        crate::vfs::pty::lookup(*vc).is_none_or(|pty| pty.slave().foreground().is_none())
    })
}

/// Spawn the pump task. Called once, at handoff: before init runs there is no
/// shell to carry output for, and after it runs there is no one else to do it.
pub fn init() {
    // The handover table exists from here on: a `VT_SETMODE` before this point
    // would have no console to write it to, because there is no console mux, no
    // pty pairs and nothing to signal.
    critical_section::with(|cs| {
        *VT.borrow(cs).borrow_mut() = Some(vtmode::Table::new(VC_COUNT));
    });
    let id = crate::sched::spawn_permanent(mux_task, crate::sched::Priority::NORMAL);
    MUX_TASK.store(id.0 as u32, Ordering::Relaxed);
    // The mapping is worth a line on the wire: six consoles whose slaves are
    // /dev/tty1..6 and whose masters only the kernel holds, and the key that
    // turns the one serial wire between them. A login that names its terminal
    // `tty1` and a host that sees one serial port are only reconcilable if
    // somebody says how.
    crate::console::write_bytes(
        b"console mux: shells on tty1..tty6 (pty-backed); ^A 1..6 switches; VT_PROCESS promises belong to the process that wrote them; #DF runs on an IST stack\n",
    );
}

fn mux_task() {
    let id = crate::sched::current();
    loop {
        let was_active = active();
        // A negotiation whose session never answered - because it died, or
        // because it never installed the handler `VT_SETMODE` named a signal
        // for - is ended here, by the only task that is guaranteed to keep
        // looking at the console.
        expire_if_due();
        // Join every pair's wake list, every lap. Membership is not cached
        // because it does not need to be: the lists are small, and a wake
        // that misses because a list was drained between laps stalls every
        // console for exactly as long as the mux stays asleep.
        for pty in crate::vfs::pty::pairs() {
            pty.subscribe_master(id);
        }
        // Parked to the negotiation's deadline rather than forever: while a
        // session is being asked, this task is what makes the wait bounded.
        crate::sched::block_until_deadline(
            || {
                active() != was_active
                    || crate::vfs::pty::lookup(was_active).is_some_and(|pty| pty.master_ready())
            },
            vt_deadline(),
        );
        // A switch: nothing to pump yet, the next lap reads the new console.
        if active() != was_active {
            continue;
        }
        // Drain this console's queue to the framebuffer. The sink is memory
        // now, so take more than one 512-byte wire-sized gulp per lap - but
        // with a budget, so a flood here can never keep the mux from looking at
        // whether the active console changed (that is what makes `^A` snappy).
        if let Some(pty) = crate::vfs::pty::lookup(was_active) {
            let mut budget = 64 * 1024;
            while budget > 0 {
                let Some(bytes) = pty.take_master(4096.min(budget)) else {
                    break;
                };
                budget -= bytes.len();
                crate::console::vc_output(was_active, &bytes);
                if bytes.len() < 4096 {
                    break;
                }
            }
        }
    }
}

/// One byte from COM1, already drained from the UART. Before the mux is up
/// this belongs to the console tty; after, to the active virtual console,
/// with `^A` held back for one byte to see whether a switch was asked for.
pub fn input(byte: u8) {
    if PENDING_PREFIX.swap(false, Ordering::Relaxed) {
        match byte {
            b'1'..=b'6' => {
                begin_activate((byte - b'0') as u32);
            }
            // `^A` twice is one `^A` at the shell, the way screen does it.
            0x01 => forward(0x01),
            // Any other follower: the `^A` was itself, and goes through too.
            other => {
                forward(0x01);
                forward(other);
            }
        }
        return;
    }
    if byte == 0x01 {
        PENDING_PREFIX.store(true, Ordering::Relaxed);
        return;
    }
    forward(byte);
}

fn forward(byte: u8) {
    // Note what does *not* happen here: a `KDSKBMODE(K_OFF)` the session on a
    // console wrote does not stop these bytes. On Linux that mode belongs to
    // the AT keyboard, and the machine's other input path - the serial line a
    // person is watching and typing at - carries on regardless. Turning the
    // keyboard off there is what a compositor does to the *text* console, and
    // the text console's half of that is the screen, not the wire: see
    // [`painted`], which is where the compositor's ownership is honoured. A
    // drop here instead left the machine with no way to reach its own shell
    // after weston exited, because nothing restores a mode the session set on
    // the way out.
    let Some(pty) = crate::vfs::pty::lookup(active()) else {
        return;
    };
    // The machine's very first reader is not on a virtual console: init runs
    // with its standard descriptors on the console tty, and so does any
    // emergency shell the initramfs drops to before a getty exists. Until a
    // reader has claimed the active console - `Tty::read` records its group -
    // typed bytes still belong to that first reader, or a boot that stops
    // halfway would take the keyboard with it.
    if pty.slave().foreground().is_none() {
        crate::vfs::tty::global().receive_byte(byte);
        return;
    }
    pty.slave().receive_byte(byte);
}

fn switch(to: u32) {
    if to > VC_COUNT {
        return;
    }
    set_active(to);
    // The pump is parked on the old console's master; the switch changed what
    // it waits for, and only a wake can make it look.
    wake_task();
    // Tell the renderer to show the new console now. This is the whole of the
    // switch being *visible* without another keystroke: the render task repaints
    // the newly active console the moment it wakes, and only a wake makes it
    // look. The gate is `grids_live`, not `gpu_mode`, because on a compositor
    // boot the kernel log never leaves the wire (`gpu_mode` is false there for
    // the machine's whole life) yet the text VTs the compositor releases still
    // have grids the renderer owns - and switching *to* or *away from* such a VT
    // has to repaint then, not on the next mouse move. A boot with no grids at
    // all (headless, or a serial console before any framebuffer) has nothing to
    // wake, and its switch is narrated on the wire below instead.
    if crate::console::grids_live() {
        crate::drivers::video::render::notify();
    }
    // The screen has moved, and with it the DRM master's right to drive it. Two
    // directions, each the complement of the other, mirroring Linux where the VT
    // switch itself hands the card over - the compositor is not trusted to notice:
    //   - Away from the master's console: `revoke_now` stops its pixels landing
    //     and hangs up the card fd, so it pauses rather than racing the text
    //     render for the scanout.
    //   - Back onto it: `grant_and_resent` clears the hang-up *and* re-presents the
    //     master's committed framebuffer, because a compositor re-commits only on
    //     its own input - a mouse move - and would otherwise leave the desktop
    //     hidden behind the text that filled the screen while it was away. The
    //     switch back is visible on the frame that switched, not on the next event.
    crate::drivers::drm::revoke_now();
    crate::drivers::drm::grant_and_resent();
    // On the shared serial wire a word in the kernel's own voice tells the eye
    // which console the next bytes belong to. Where a framebuffer is drawing the
    // consoles it is not needed - each console is its own screen, and the switch
    // itself is the join - so say nothing and let the redraw speak.
    if !crate::console::grids_live() {
        crate::console::write_bytes(alloc::format!("\r\n[tty{to}]\r\n").as_bytes());
    }
}

/// Route COM1's input: the mux's when it is up, the console tty's before.
/// The serial interrupt handler calls this for every byte it drains.
pub fn route_input(byte: u8) {
    if ready() {
        input(byte);
    } else {
        crate::vfs::tty::global().receive_byte(byte);
    }
}
