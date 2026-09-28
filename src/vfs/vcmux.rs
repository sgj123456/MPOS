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
use crate::vfs::vtmode::{self, Decision, VtMode};

/// The virtual console currently on the wire, 1-based.
static ACTIVE: AtomicU32 = AtomicU32::new(1);

/// Each console's video mode (`KD_TEXT`/`KD_GRAPHICS`) and keyboard mode
/// (`K_XLATE`, `K_OFF`, ...), indexed 1-based like the consoles themselves.
///
/// Two arrays rather than a small struct per console because each is read on a
/// path that already exists and must not take a lock to find out: the render
/// task asking whether this console should be painted at all, and the mux
/// asking whether a keystroke belongs to a shell. `KDSETMODE` and `KDSKBMODE`
/// write them, which is what makes a seat's handover mean something instead of
/// being a request that was accepted and forgotten.
static KD_MODE: [AtomicU32; crate::vfs::pty::VC_COUNT as usize + 1] =
    [const { AtomicU32::new(crate::vfs::tty::KD_TEXT) }; crate::vfs::pty::VC_COUNT as usize + 1];
static KB_MODE: [AtomicU32; crate::vfs::pty::VC_COUNT as usize + 1] =
    [const { AtomicU32::new(crate::vfs::tty::K_XLATE) }; crate::vfs::pty::VC_COUNT as usize + 1];

/// The slot a console's mode lives in, or `None` for a number that is not one
/// of the consoles this machine has.
fn slot(vc: u32) -> Option<usize> {
    if vc == 0 || vc > crate::vfs::pty::VC_COUNT {
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
        None => crate::vfs::tty::KD_TEXT,
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
        None => crate::vfs::tty::K_XLATE,
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
    master_vc() == Some(vc) && active() == vc && kd_mode(vc) == crate::vfs::tty::KD_GRAPHICS
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
    crate::arch::timer::ticks()
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

/// `VT_SETMODE`: record what console `vc`'s session promises about the screen.
///
/// `None` is `EINVAL`: not one of this machine's consoles. `Some(Err(_))` is the
/// mode itself being nonsense (an unknown `mode`, a signal number that is not a
/// signal, `relsig == acqsig`), and a console with a stale promise is not
/// allowed to keep it - see [`vtmode::Table::set_mode`].
///
/// A mode that takes a console *out* of graphics ownership has to be repainted
/// now, exactly as `KDSETMODE` does: the scanout holds whatever the compositor
/// last drew, and nothing else will tell the render task it may have it back.
pub fn set_vt_mode(vc: u32, mode: VtMode) -> Result<(), Option<vtmode::ModeError>> {
    let outcome = vt_with(|t| t.set_mode(vc, mode));
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
/// `false` says the number is not one of this machine's consoles, which is what
/// Linux answers `EINVAL` for. **This one never blocks**, and that is the
/// difference between the two doors: `^A` arrives from the serial interrupt
/// handler, which cannot sleep, so an operator's keystroke starts the
/// negotiation and the mux task's deadline finishes it. A session's own
/// `VT_ACTIVATE` sleeps instead - see [`wait_activate`], which is Linux's
/// `vt_release`.
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

/// What a session's `VT_ACTIVATE` ended up being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activated {
    /// The console is on the screen now - either nothing had to be asked, or it
    /// was asked and answered.
    Taken,
    /// The console already had the screen.
    Already,
    /// The console on the screen refused to give it up. Linux's `vt_release`
    /// answers `-EBUSY` for this, and the refusal stands until the holding
    /// session lets go.
    Refused,
    /// The screen never came free inside the wait. Linux gives the requester
    /// `-EBUSY` too, and *completes the switch* on the timeout - the wait is
    /// bounded, not permanent.
    TimedOut,
    /// A signal arrived while waiting. The negotiation is still in flight
    /// (Linux leaves it there as well: `relsig` was sent, and the console on the
    /// screen has not answered yet), so a retry sees the same wait, not a new
    /// `relsig`.
    Interrupted,
    /// Not one of this machine's consoles.
    Inval,
}

/// `VT_ACTIVATE`: ask for console `to`, and sleep until the answer.
///
/// Linux's `vt_release`: send the releasing console its `relsig`, park the
/// *requesting task* on `wait_event_interruptible_timeout(..., 60*HZ)`, and let
/// the console's `VT_RELDISP` - or the timeout - wake it. The requester sleeps,
/// not the kernel: a session that activates a console a compositor is holding
/// discovers the refusal by being told, not by polling `VT_GETSTATE` and hoping.
pub fn wait_activate(to: u32) -> Activated {
    let started = now();
    let Some(decision) = (match vt_with(|t| t.request_switch(to, started, false)) {
        Some(Some(decision)) => Some(decision),
        // `None` inside: not one of this machine's consoles; `None` outside: no
        // console mux yet, which from a session's side is the same answer.
        _ => None,
    }) else {
        return Activated::Inval;
    };
    let mut release_from = match decision {
        Decision::SwitchNow { to } => {
            complete_switch(to);
            return Activated::Taken;
        }
        Decision::Already { .. } => return Activated::Already,
        Decision::Refused { .. } => {
            apply(decision);
            return Activated::Refused;
        }
        // Ours is the release already in flight; it was signalled for whoever
        // asked first, so we join the wait rather than asking the console twice.
        Decision::Waiting { release } => release.from,
        Decision::BeginRelease { release, .. } => {
            apply(decision);
            release.from
        }
    };
    add_release_waiter(crate::sched::current());
    let end = started + vtmode::RELEASE_TIMEOUT_TICKS;
    loop {
        let released =
            vt_with(|t| t.active() == to || t.release().is_none_or(|r| r.from != release_from))
                .unwrap_or(true);
        if released {
            remove_release_waiter(crate::sched::current());
            return Activated::Taken;
        }
        match crate::sched::block_until_deadline(|| false, end) {
            crate::sched::WaitEnd::Deadline => {
                remove_release_waiter(crate::sched::current());
                // The timeout does not abandon the switch: the mux's own
                // expiry completes it a moment later, which is Linux's order
                // (`vt_release` returns and `vt_activate` goes on).
                return Activated::TimedOut;
            }
            crate::sched::WaitEnd::Interrupted => {
                remove_release_waiter(crate::sched::current());
                return Activated::Interrupted;
            }
            // Woken for some other reason - the console answered, or someone
            // else's switch moved the screen. Re-read the table, and notice if
            // the console being asked has changed underneath us (a second
            // release started while we slept is a second answer to wait for).
            crate::sched::WaitEnd::Condition => {
                if let Some(from) = vt_with(|t| t.release().map(|r| r.from)).flatten() {
                    release_from = from;
                }
            }
        }
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

/// The tasks parked in `VT_ACTIVATE`, and the tasks parked per-console in
/// `VT_WAITACTIVE`. Both are Linux's `vc_wait` queue, split in two because the
/// predicates differ: one waits for the *release* to end, the other for a named
/// console to be the one on the screen.
static RELEASE_WAITERS: CsMutex<core::cell::RefCell<Vec<TaskId>>> =
    CsMutex::new(core::cell::RefCell::new(Vec::new()));
static ACTIVE_WAITERS: CsMutex<core::cell::RefCell<Vec<(u32, TaskId)>>> =
    CsMutex::new(core::cell::RefCell::new(Vec::new()));

fn add_release_waiter(id: TaskId) {
    critical_section::with(|cs| {
        let mut v = RELEASE_WAITERS.borrow(cs).borrow_mut();
        if !v.contains(&id) {
            v.push(id);
        }
    });
}

fn remove_release_waiter(id: TaskId) {
    critical_section::with(|cs| RELEASE_WAITERS.borrow(cs).borrow_mut().retain(|&w| w != id));
}

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
    let release = critical_section::with(|cs| RELEASE_WAITERS.borrow(cs).borrow().clone());
    for id in release {
        crate::sched::wake(id);
    }
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

/// Let go of a console: a session's last descriptor closed, so any promise it
/// made about this screen dies with it.
pub fn abandon(vc: u32) {
    vt_with(|t| t.abandon(vc));
    // A `KDSKBMODE` was a session's word about *its* keyboard, and the session
    // is gone: the console takes its keyboard mode back, or a `K_OFF` written on
    // the way out would leave a respawned getty with no keys to read.
    set_kb_mode(vc, crate::vfs::tty::K_XLATE);
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
        Decision::Already { .. }
        | Decision::Waiting { .. }
        | Decision::Refused { frsig: None, .. } => {}
        // The console asking was told no, by the console on the screen. Its
        // `frsig` is the only way it learns that without polling.
        Decision::Refused {
            to,
            frsig: Some(sig),
            ..
        } => signal_group(to, sig),
        Decision::BeginRelease { release, relsig } => {
            // A console that named `VT_PROCESS` but no `relsig` can never
            // answer, so there is nothing to send: the table has already made
            // its deadline *now*, and the mux's next look forces the switch.
            if let Some(sig) = relsig {
                signal_group(release.from, sig);
            }
        }
    }
}

/// Send `sig` to the foreground process group of console `vc`, if it has one.
///
/// A console with no session reading it has nobody to tell, which is also the
/// common case for a `frsig` on a machine with one login: the refusal is
/// recorded either way, and the next `VT_GETSTATE` says where the screen is.
fn signal_group(vc: u32, sig: u8) {
    let Some(pty) = crate::vfs::pty::lookup(vc) else {
        return;
    };
    pty.slave().signal_console(sig);
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
    // The screen moved: every task parked in `VT_ACTIVATE` or `VT_WAITACTIVE`
    // was waiting for exactly this, and the mux has a new console to pump.
    wake_waiters();
    // The session that now has the screen is told so, if it asked to be told:
    // that is what a compositor's `acqsig` handler is for - it re-takes the DRM
    // master and repaints, which is the recovery path after a switch and away.
    let acqsig = vt_with(|t| t.mode(to))
        .flatten()
        .filter(|m| m.mode == vtmode::VT_PROCESS);
    if let Some(m) = acqsig
        && m.acqsig != 0
    {
        signal_group(to, m.acqsig);
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

/// The mux task's id, so an input-time switch can wake it - the task parks on
/// the old console's master, and the switch changes what it should park on.
static MUX_TASK: AtomicU32 = AtomicU32::new(0);

/// Whether the mux is up. Until it is, COM1's input belongs to the console
/// tty - the boot checks type at it directly, and the handoff happens long
/// before any shell exists.
pub fn ready() -> bool {
    MUX_TASK.load(Ordering::Relaxed) != 0
}

/// The virtual console currently on the wire, 1-based.
pub fn active() -> u32 {
    ACTIVE.load(Ordering::Relaxed)
}

/// The operator's `^A <digit>`, from any context, including an interrupt.
///
/// Nothing here blocks: a decision either moves the screen (an atomic store, a
/// wake, and a `notify` that only sets a flag) or asks a session for permission
/// (a signal queued into the task table, which is what a keystroke's echo
/// already does from this same handler). The wait belongs to the mux task, which
/// parks to the negotiation's deadline - so an unanswered `VT_RELDISP` costs
/// nobody's keystroke, and the operator always keeps a way to read the machine.
///
/// The session's own `VT_ACTIVATE` does wait, and waits differently: see
/// [`wait_activate`]. The two share the decision (`vtmode::Table::request_switch`)
/// and differ only in who sleeps.
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
    (1..=crate::vfs::pty::VC_COUNT).find(|vc| {
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
        *VT.borrow(cs).borrow_mut() = Some(vtmode::Table::new(crate::vfs::pty::VC_COUNT));
    });
    let id = crate::sched::spawn_permanent(mux_task, crate::sched::Priority::NORMAL);
    MUX_TASK.store(id.0 as u32, Ordering::Relaxed);
    // The mapping is worth a line on the wire: six consoles whose slaves are
    // /dev/tty1..6 and whose masters only the kernel holds, and the key that
    // turns the one serial wire between them. A login that names its terminal
    // `tty1` and a host that sees one serial port are only reconcilable if
    // somebody says how.
    crate::console::write_bytes(
        b"console mux: shells on tty1..tty6 (pty-backed); ^A 1..6 switches\n",
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
    if to > crate::vfs::pty::VC_COUNT {
        return;
    }
    ACTIVE.store(to, Ordering::Relaxed);
    // The pump is parked on the old console's master; the switch changed what
    // it waits for, and only a wake can make it look.
    wake_task();
    // On the framebuffer, tell the renderer to show the new console now.
    if crate::console::gpu_mode() {
        crate::drivers::video::render::notify();
    }
    // On the shared serial wire a word in the kernel's own voice tells the eye
    // which console the next bytes belong to. On the framebuffer it is not
    // needed - each console is its own screen, and the switch itself is the
    // join - so say nothing and let the redraw speak.
    if !crate::console::gpu_mode() {
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
