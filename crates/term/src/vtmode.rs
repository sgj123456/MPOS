//! The virtual-console handover: who may switch, and what has to happen first.
//!
//! Like the `evdev` leaf, this module is the *policy* half of a bare-metal
//! device lifted out so a host build can argue with it. The device half is
//! `vfs::vcmux`, which owns the clock, the tasks and the signal
//! delivery; everything here is arithmetic over a table of per-console modes
//! plus one in-flight switch. That split is what lets the interesting part -
//! "a session that promised to answer `VT_RELDISP` must be waited for, and a
//! session that dies while being waited for must not lock the machine" - be
//! tested at all, because none of it needs hardware.
//!
//! # The contract
//!
//! A console is in one of two modes (`struct vt_mode` in `<linux/vt.h>`):
//!
//! * `VT_AUTO` - the default: switching to or away from it is unconditional, and
//!   the kernel does as asked.
//! * `VT_PROCESS` - switching *away* is a request, not an act. The kernel marks
//!   the console releasing, sends `relsig` to the process that promised to answer
//!   it, and does not move the screen until that session calls `VT_RELDISP` to
//!   agree (or `VT_RELDISP(0)` to refuse). A session that wants the screen at all -
//!   which is any compositor - is in this mode, because the alternative is a text
//!   console painting over its frames between two keystrokes.
//!
//! Two things make the mode worth its complexity, and both are the reason it is
//! recorded rather than ignored:
//!
//! * **The wait is bounded.** A session that is killed, or that simply never
//!   installed the handler its `relsig` names, would otherwise strand every
//!   later switch - including the operator's own. So a release carries a
//!   deadline ([`RELEASE_TIMEOUT_NS`], resolved by the caller, which owns the
//!   clock), and the escape hatch is not the clock at all: a *second* `^A` to
//!   the same console forces it through. Neither depends on the session
//!   cooperating.
//! * **One negotiation at a time.** A refused release ends it rather than
//!   parking a queue, so there is at most one [`Release`] in flight, not a
//!   pending stack per console.
//!
//! The one `EINVAL` Linux checks is an unknown `mode`; it looks at none of the
//! four signal bytes. This kernel also refuses `relsig == acqsig` and a signal
//! number that is not a signal, which are two refusals Linux does not make - kept
//! because a promise either of them describes cannot be kept, and named as a
//! deviation rather than left to be discovered as a surprise. And like Linux, the
//! console *on the screen* is the one that gets consulted - which is what makes
//! `VT_SETMODE` a promise about this session's own screen rather than an
//! instruction to whoever happens to be holding it.
//!
//! # Who a promise belongs to
//!
//! `VT_PROCESS` is a promise made by a *process*, and Linux keeps it as one:
//! `VT_SETMODE` stores `vc->vt_pid = get_pid(task_pid(current))`
//! (`vt_ioctl.c`), and the two signals that carry the handover are sent with
//! `kill_pid(vc->vt_pid, vc->vt_mode.relsig, 1)` and the same for `acqsig` - to
//! that process, never to whichever group happens to hold the terminal.
//!
//! That is not a detail. A seat manager opens the console, writes
//! `VT_SETMODE(VT_PROCESS)`, and **closes the descriptor again** before the
//! compositor it serves ever touches the screen - seatd's `vt_open()` is exactly
//! that sequence - so the promise outlives the fd that made it by design, and it
//! is the process, not the terminal, that answers `VT_RELDISP`. Aimed at the
//! console's foreground group instead, `acqsig` - which every seat manager names
//! as `SIGUSR2` - lands on whatever session is in the foreground: the compositor
//! being killed on its way out instead of being told its screen came free, or a
//! shell that never asked about the screen at all and has no `SIGUSR2` handler.
//!
//! And a promise whose process is gone must stop being a promise. Linux notices
//! at the moment it would have to send: `kill_pid` answering nonzero means the
//! keeper died, so `reset_vc()` puts the console back to `VT_AUTO`, `KD_TEXT`
//! and no signals, and the switch proceeds *now* rather than waiting out the
//! 60-second window for an answer no living process can give. Both halves are
//! here: the table records the keeper and hands its number out beside every
//! signal ([`Promise`]), and [`Table::reset`] is `reset_vc`'s table half.
//!
//! `frsig` is ignored, as Linux ignores it ("the frsig is ignored, so we set it
//! to 0"): `VT_SETMODE` stores zero there, `VT_GETMODE` reads zero back, and a
//! refusal sends nothing at all - the requester learns of it from its own
//! `VT_ACTIVATE`, which answers `EBUSY`.

use alloc::vec;
use alloc::vec::Vec;

/// `VT_AUTO` - switching happens without asking this console's session.
pub const VT_AUTO: u8 = 1;
/// `VT_PROCESS` - this console's session is asked first, by `relsig`.
pub const VT_PROCESS: u8 = 0;

/// The largest signal number a `struct vt_mode` can name. `SIGRTMAX` aside,
/// Linux's `vt_mode` fields are bytes and the valid set is 1..=31, so anything
/// else is an `EINVAL` from `VT_SETMODE` rather than a signal nobody can raise.
pub const MAX_SIGNAL: u8 = 31;

/// How long a release may be waited for, in clock ticks: 60 seconds at this
/// kernel's 250 Hz.
///
/// **This bound is ours, not Linux's, and the difference is worth stating**: the
/// current console code has no timeout on a process-controlled switch at all.
/// `change_console()` sends `relsig` and returns; if the session never answers
/// `VT_RELDISP`, the pending switch simply sits there until the session does answer
/// or the box is rebooted. What Linux does notice is a *dead* keeper - `kill_pid`
/// fails and `reset_vc` runs - and that half this kernel has too, at the same
/// moment (`vfs::vcmux` aims the signal at the process and resets the
/// console when nobody is left to receive it).
///
/// So the deadline is what the mux has always been for: a session that is wedged
/// rather than dead - holding the screen, answering nothing, blocking nothing -
/// must not cost a person the use of the machine. It is the last resort, not the
/// ordinary path: the ordinary paths are the session's own `VT_RELDISP`, the
/// operator's second `^A`, and the reset on a keeper that has died.
///
/// Nothing here depends on the figure for reachability, and a shorter one would
/// silently kill a compositor mid-page-flip on a machine that is merely slow.
/// The release wait: sixty seconds, in the nanoseconds of the clock the
/// caller passes to [`Table::expired`]. It was once spelled in ticks against
/// the tick counter; the unit moved with the 2026-10-24 two-clocks fix
/// (`docs/notes/` §6.23), because a deadline a park can disagree with is
/// worth less than none.
pub const RELEASE_TIMEOUT_NS: u64 = 60_000_000_000;

/// Whether a signal number is one a `vt_mode` may name. Zero is "no signal",
/// which every field accepts.
fn signal_ok(sig: u8) -> bool {
    sig == 0 || sig <= MAX_SIGNAL
}

/// `struct vt_mode`, the eight bytes `VT_SETMODE` writes and `VT_GETMODE`
/// reads back:
///
/// ```text
///   0  u8  mode      VT_AUTO or VT_PROCESS
///   1  u8  waitv     the session wants VT_WAITACTIVE to sleep
///   2  u8  relsig    the signal that says "hand the screen back"
///   3  u8  acqsig    the signal that says "the screen is yours"
///   4  u8  frsig     the signal that says "the console you asked for refused"
///   5..8  padding
/// ```
///
/// The padding is part of the ABI: `VT_GETMODE` is a `_IOR` of exactly eight
/// bytes, and a caller that sizes its struct from the request reads whatever is
/// in `5..8`, so this answers zeroes rather than whatever the last caller left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VtMode {
    pub mode: u8,
    pub waitv: u8,
    pub relsig: u8,
    pub acqsig: u8,
    pub frsig: u8,
}

impl VtMode {
    /// The mode a console that has never been spoken to is in: automatic, no
    /// signals, nothing waiting.
    pub const AUTO: VtMode = VtMode {
        mode: VT_AUTO,
        waitv: 0,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };

    /// Read the bytes `VT_SETMODE` was given. The ioctl layer hands over
    /// `min(sizeof(struct vt_mode), the request's size)` bytes, so a shorter
    /// slice is a caller that wrote a smaller struct than Linux's, and the
    /// fields it did not write read as zeroes - which is also what they *are*.
    pub fn from_bytes(bytes: &[u8]) -> VtMode {
        let g = |i: usize| bytes.get(i).copied().unwrap_or(0);
        VtMode {
            mode: g(0),
            waitv: g(1),
            relsig: g(2),
            acqsig: g(3),
            frsig: g(4),
        }
    }

    /// The same, for `VT_GETMODE`. Always the full eight bytes.
    pub fn to_bytes(self) -> [u8; 8] {
        [
            self.mode,
            self.waitv,
            self.relsig,
            self.acqsig,
            self.frsig,
            0,
            0,
            0,
        ]
    }

    /// Whether this mode is one `VT_SETMODE` would accept.
    ///
    /// `frsig` is not looked at, because Linux ignores it: `VT_SETMODE` copies
    /// the caller's struct and then writes zero over that one field, so a number
    /// there is neither stored nor acted on, and refusing the call for it would
    /// be this kernel inventing a rule the caller's own man page does not have.
    pub fn validate(&self) -> Result<(), ModeError> {
        if self.mode != VT_AUTO && self.mode != VT_PROCESS {
            return Err(ModeError::BadMode);
        }
        if !signal_ok(self.relsig) || !signal_ok(self.acqsig) {
            return Err(ModeError::BadSignal);
        }
        // Linux refuses neither of these; this kernel does, for the reason in
        // the variant's own note - one keeper cannot be told two opposite things
        // by the same number. Named as a deviation in `NOTES.md`.
        if self.relsig != 0 && self.relsig == self.acqsig {
            return Err(ModeError::SameSignal);
        }
        Ok(())
    }
}

/// Why a `VT_SETMODE` was refused. Each of these is `EINVAL` to the caller;
/// the split exists so a host test can pin that the *reason* was checked, not
/// just that something failed.
///
/// Only [`ModeError::BadMode`] is Linux's refusal: `vt_setmode` checks
/// `tmp.mode != VT_AUTO && tmp.mode != VT_PROCESS` and nothing else about the
/// four signal bytes. The other two are this kernel refusing a call Linux would
/// accept, kept because both describe a promise that cannot be kept - and named
/// as a deviation in `NOTES.md` rather than left to be discovered as a surprise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeError {
    /// `mode` is neither `VT_AUTO` nor `VT_PROCESS`. Linux's `EINVAL`.
    BadMode,
    /// A signal field names something that is not a signal. Not Linux's - it
    /// checks nothing here, and a number over `SIGRTMAX` only reaches the caller
    /// as `kill_pid`'s failure at the next handover.
    BadSignal,
    /// `relsig == acqsig`. Not Linux's either: one keeper cannot be told two
    /// opposite things by the same number, and a session that meant "no signal"
    /// for both would have written zeroes.
    SameSignal,
}

/// The switch being negotiated: the console on the screen, where the screen is
/// meant to go when it answers, and the tick the `relsig` went out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Release {
    pub from: u32,
    pub to: u32,
    pub started: u64,
}

/// Which of a console's two handover signals a caller wants. There is no third:
/// Linux ignores `frsig`, and a refusal is answered by the requester's own
/// `VT_ACTIVATE` returning `EBUSY`, not by a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// "Hand the screen back" - the answer comes as `VT_RELDISP`.
    Release,
    /// "The screen is yours" - what a compositor uses to re-take the card.
    Acquire,
}

/// A signal the console layer owes, and the process it is aimed at: Linux's
/// `kill_pid(vc->vt_pid, vc->vt_mode.relsig, 1)` as one value, so the two cannot
/// be separated by a caller that remembers the number and forgets who named it.
///
/// A signal here is only ever sent to `pid`. That is the whole point of the
/// struct: the promise was made by one process, and a handover signal that
/// follows the *terminal* instead would strike whichever session is in the
/// foreground when it goes out - a compositor on its way out, or a shell that
/// never asked to be consulted and has no handler for `SIGUSR2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Promise {
    /// The signal number the promise named.
    pub sig: u8,
    /// The thread group that wrote `VT_SETMODE` - the only thing `sig` may be
    /// sent to. Linux holds a refcounted `struct pid`; this kernel has the slot
    /// number, which a later process can be given after the keeper dies (see the
    /// named gap in `NOTES.md`).
    pub pid: u32,
}

/// What the caller of [`Table::request_switch`] is told to do next. The table
/// never moves the screen itself: making that the caller's one move is what
/// keeps the framebuffer, the DRM master and the old console's reader waking
/// together instead of in three places that can disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to negotiate - or there was, and it is over. Make `to` the
    /// console on the screen.
    SwitchNow { to: u32 },
    /// A switch is already waiting on exactly this console: do not send the
    /// `relsig` a second time.
    Waiting { release: Release },
    /// `to` is refused, because the console on the screen said no and `to` is
    /// not that console. **Nothing is sent** - Linux ignores `frsig` and sends
    /// no refusal signal at all; the requester learns of the refusal from its own
    /// `VT_ACTIVATE`, which answers `EBUSY`, or from `VT_GETSTATE`.
    Refused { refusing: u32, to: u32 },
    /// You are the console on the screen, asking for the screen you already
    /// have: `VT_ACTIVATE`'s successful no-op, and the one case where a
    /// standing refusal is lifted.
    Already { to: u32 },
    /// The console on the screen is in `VT_PROCESS`: send `relsig` to the
    /// process that promised, and leave the screen where it is until it answers.
    /// `None` is a console that cannot be asked - it named no signal - which the
    /// caller must treat as a release that is already overdue rather than a wait
    /// nobody can end.
    BeginRelease {
        release: Release,
        relsig: Option<Promise>,
    },
}

/// The per-console modes, plus the one negotiation in flight.
///
/// Consoles are indexed 1-based, like everything else in the console layer;
/// slot zero exists because `/dev/tty0` means "the current one", which is not a
/// console that can hold a mode, so a lookup outside `1..=count` answers `None`
/// and the ioctl turns that into `EINVAL`.
pub struct Table {
    modes: Vec<VtMode>,
    /// Which process made each console's `VT_PROCESS` promise, 0 for nobody.
    /// Kept beside the mode rather than inside it because `VtMode` is the wire
    /// struct - eight bytes that `VT_GETMODE` writes back - and the keeper is not
    /// on the wire: Linux stores it in `struct vc_data`, not in `vt_mode`.
    keepers: Vec<u32>,
    active: u32,
    /// The negotiation in flight, if any.
    release: Option<Release>,
    /// The console that last refused to give up the screen, or 0 for nobody.
    /// Kept apart from `release` on purpose: a refusal is *settled* - nothing
    /// is being waited for and nothing will expire - and modelling it as an
    /// open release is how an unrelated later switch gets forced through by a
    /// timer that was counting a conversation nobody is having any more.
    refusing: u32,
}

impl Table {
    /// A machine with `count` virtual consoles, all automatic, console 1 on the
    /// screen - which is what the boot leaves behind.
    pub fn new(count: u32) -> Table {
        Table {
            modes: vec![VtMode::AUTO; count as usize + 1],
            keepers: vec![0; count as usize + 1],
            active: 1,
            release: None,
            refusing: 0,
        }
    }

    /// The console on the screen.
    pub fn active(&self) -> u32 {
        self.active
    }

    /// The negotiation in flight, if any.
    pub fn release(&self) -> Option<Release> {
        self.release
    }

    /// The console that refused to give up the screen, or 0.
    pub fn refusing(&self) -> u32 {
        self.refusing
    }

    /// `VT_GETMODE`, and `None` for a number that is not one of this machine's
    /// consoles - which is what Linux answers `EINVAL` for. Console 0 is the
    /// current-console *alias*, not a console that can hold a mode, so it is
    /// deliberately not in the table even though slot zero exists.
    pub fn mode(&self, vc: u32) -> Option<VtMode> {
        if vc == 0 {
            return None;
        }
        self.modes.get(vc as usize).copied()
    }

    /// Whether console `vc` is the one a `relsig` is outstanding on.
    pub fn is_releasing(&self, vc: u32) -> bool {
        self.release.is_some_and(|r| r.from == vc)
    }

    /// `VT_SETMODE`: record a console's mode, and the process `pid` that made it
    /// its promise.
    ///
    /// `None` means `vc` is not one of this machine's consoles.
    ///
    /// Linux ignores `frsig` - `vc->vt_mode = tmp; /* the frsig is ignored, so we
    /// set it to 0 */` - so what is *stored* here has it zeroed, and that is what
    /// `VT_GETMODE` reads back. A caller that set it (seatd sets `SIGIO`) cannot
    /// be told a refusal by that number, because no refusal signal is ever sent.
    ///
    /// Leaving `VT_PROCESS` clears a negotiation this console was holding up: a
    /// session that stops asking to be consulted has, by that act, agreed. That
    /// is what makes `^A` work again after a compositor exits without cleaning
    /// up after itself.
    pub fn set_mode(&mut self, vc: u32, mode: VtMode, pid: u32) -> Result<(), Option<ModeError>> {
        if vc == 0 || self.modes.get(vc as usize).is_none() {
            return Err(None);
        }
        mode.validate().map_err(Some)?;
        let stored = VtMode { frsig: 0, ..mode };
        self.modes[vc as usize] = stored;
        self.keepers[vc as usize] = if stored.mode == VT_PROCESS { pid } else { 0 };
        if stored.mode != VT_PROCESS {
            if let Some(release) = self.release
                && release.from == vc
            {
                self.release = None;
            }
            if self.refusing == vc {
                self.refusing = 0;
            }
        }
        Ok(())
    }

    /// The promise console `vc` holds: the signal `which` names and the process
    /// it must be sent to, or `None` when there is no promise, no signal, or no
    /// keeper to receive it. `None` is the answer that ends a wait rather than
    /// starting one - see [`Table::deadline`].
    pub fn promise(&self, vc: u32, which: Signal) -> Option<Promise> {
        let mode = self.modes.get(vc as usize)?;
        if mode.mode != VT_PROCESS {
            return None;
        }
        let sig = match which {
            Signal::Release => mode.relsig,
            Signal::Acquire => mode.acqsig,
        };
        if sig == 0 {
            return None;
        }
        let pid = self.keepers.get(vc as usize).copied().unwrap_or(0);
        if pid == 0 {
            return None;
        }
        Some(Promise { sig, pid })
    }

    /// `reset_vc`, the table half: a promise whose keeper has died is no
    /// promise, so the console goes back to automatic with no signals named and
    /// no keeper, and anything it was holding up is over. Linux's other half -
    /// the console's `KD_TEXT` - belongs to the caller, which owns the screen.
    ///
    /// Returns whether the console had anything to take away, so the caller does
    /// not repaint a console that was already automatic.
    ///
    /// This and the keeper writing `VT_AUTO` itself (`seat.c`'s `vt_close` does
    /// exactly that on the way out) are the only two ways a promise ends. There is
    /// deliberately **no** hook on the last close of the console's descriptor: a
    /// seat manager opens the console, writes `VT_SETMODE(VT_PROCESS)` and closes
    /// the fd before the compositor it serves ever touches the screen - `vt_open`
    /// is open, set mode, set keyboard, set graphics, `close(ttyfd)` - so ending a
    /// promise at that close, even one ended only by the process that wrote it,
    /// would erase the claim the moment it is made and put the compositor's screen
    /// back in the hands of whatever the text console draws next. The promise is a
    /// process's, not a descriptor's; that is what Linux's `vc->vt_pid` is for, and
    /// its own comment on the alternative says why it was not taken ("tracking tty
    /// count may be undesirable").
    pub fn reset(&mut self, vc: u32) -> bool {
        if vc == 0 || self.modes.get(vc as usize).is_none() {
            return false;
        }
        let had = self.modes[vc as usize].mode == VT_PROCESS;
        self.modes[vc as usize] = VtMode::AUTO;
        self.keepers[vc as usize] = 0;
        if let Some(release) = self.release
            && release.from == vc
        {
            self.release = None;
        }
        if self.refusing == vc {
            self.refusing = 0;
        }
        had
    }

    /// `VT_ACTIVATE`, and the `^A <digit>` path: ask for console `to` to be the
    /// one on the screen, at tick `now`.
    ///
    /// `force` is the operator's word - a `^A` to a console that is already
    /// being asked - and it ends the negotiation without an answer, because an
    /// escape hatch that depends on a timer is not an escape hatch.
    ///
    /// `None` means `to` is not one of this machine's consoles (`EINVAL`).
    pub fn request_switch(&mut self, to: u32, now: u64, force: bool) -> Option<Decision> {
        if to == 0 || self.modes.get(to as usize).is_none() {
            return None;
        }
        if to == self.active {
            // Asking for the screen you are on always succeeds, and it is the
            // one thing that lifts a refusal the console on the screen itself
            // raised - otherwise a session that typed its own `VT_ACTIVATE`
            // would be told no by its own previous no.
            self.refusing = 0;
            return Some(Decision::Already { to });
        }
        if self.refusing != 0 && self.refusing != to {
            return Some(Decision::Refused {
                refusing: self.refusing,
                to,
            });
        }
        // Already negotiating. The target moves - a person who types `^A 3`
        // then `^A 4` wants tty4, not a queue - but the console on the screen
        // is asked exactly once, because a second `relsig` to a session that is
        // already doing its best to answer the first is how a compositor kills
        // itself twice on the way out.
        if let Some(release) = self.release {
            if force {
                self.release = None;
                self.refusing = 0;
                return Some(Decision::SwitchNow { to });
            }
            let release = Release { to, ..release };
            self.release = Some(release);
            return Some(Decision::Waiting { release });
        }
        let leaving = self.modes[self.active as usize];
        if leaving.mode != VT_PROCESS || force {
            self.refusing = 0;
            return Some(Decision::SwitchNow { to });
        }
        let release = Release {
            from: self.active,
            to,
            started: now,
        };
        self.release = Some(release);
        Some(Decision::BeginRelease {
            release,
            relsig: self.promise(release.from, Signal::Release),
        })
    }

    /// `VT_RELDISP` from console `vc`: a nonzero argument agrees, zero refuses.
    ///
    /// [`Ack::NotReleasing`] is what Linux answers `EINVAL` for, and it matters
    /// that it is an error rather than a success: a session answering a request
    /// that was never made would be completing someone else's switch.
    pub fn release_ack(&mut self, vc: u32, agree: bool) -> Ack {
        let Some(release) = self.release else {
            return Ack::NotReleasing;
        };
        if release.from != vc {
            return Ack::NotReleasing;
        }
        self.release = None;
        if !agree {
            self.refusing = vc;
            return Ack::Refused { to: release.to };
        }
        self.refusing = 0;
        Ack::Complete {
            from: vc,
            to: release.to,
        }
    }

    /// Move the screen, once the negotiation is over by any route: agreed,
    /// forced, or expired.
    pub fn complete_switch(&mut self, to: u32) {
        self.active = to;
        self.release = None;
        self.refusing = 0;
    }

    /// The tick the in-flight release stops being worth waiting for, or `None`
    /// when nothing is in flight. The caller parks on this, so the arithmetic
    /// lives here rather than being duplicated at the wait and hoping the two
    /// stay together.
    ///
    /// A release whose console named no `relsig` was never going to be answered
    /// at all, so its deadline is the tick it started on: a session that set
    /// `VT_PROCESS` and left the signal zero must not own the screen for the
    /// length of a timer it can never be rescued by.
    pub fn deadline(&self) -> Option<u64> {
        let release = self.release?;
        if self.promise(release.from, Signal::Release).is_none() {
            return Some(release.started);
        }
        Some(release.started + RELEASE_TIMEOUT_NS)
    }

    /// Whether the in-flight release has run out the time it was given at tick
    /// `now`, returning the console to switch to. [`Table::deadline`] is the
    /// same question asked without a tick, and the caller completes the switch
    /// itself either way.
    pub fn expired(&mut self, now: u64) -> Option<u32> {
        let deadline = self.deadline()?;
        if now < deadline {
            return None;
        }
        let to = self.release?.to;
        self.release = None;
        self.refusing = 0;
        Some(to)
    }
}

/// [`Table::release_ack`]'s answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    /// The console on the screen agreed: make `to` the console, and tell it the
    /// screen is its session's now (`acqsig`).
    Complete { from: u32, to: u32 },
    /// The console refused to give the screen up. It does not move.
    Refused { to: u32 },
    /// Nobody asked this console to release anything - Linux's `EINVAL`.
    NotReleasing,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUR: u32 = 4;
    /// The process that makes every promise in these tests - Linux's
    /// `task_pid(current)` at the moment of `VT_SETMODE`.
    const KEEPER: u32 = 7;
    /// Some other process, which must never be able to let go of a promise that
    /// is not its own.
    const STRANGER: u32 = 8;

    fn process(rel: u8, acq: u8) -> VtMode {
        VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: rel,
            acqsig: acq,
            frsig: 0,
        }
    }

    /// `<linux/vt.h>`: the struct is eight bytes and the field order is the
    /// wire order, because a musl caller compiles this layout and `VT_GETMODE`
    /// is a `_IOR` of `sizeof(struct vt_mode)`.
    #[test]
    fn the_mode_survives_its_wire_format() {
        let m = VtMode {
            mode: VT_PROCESS,
            waitv: 1,
            relsig: 10,
            acqsig: 12,
            frsig: 21,
        };
        assert_eq!(m.to_bytes(), [0, 1, 10, 12, 21, 0, 0, 0]);
        assert_eq!(VtMode::from_bytes(&m.to_bytes()), m);
        // A caller that wrote a shorter struct reads zeroes it never wrote.
        assert_eq!(
            VtMode::from_bytes(&[VT_AUTO]),
            VtMode {
                mode: VT_AUTO,
                ..VtMode::AUTO
            }
        );
    }

    /// A console nobody has spoken to is automatic and answers `VT_GETMODE`
    /// with what a stock Linux box answers with.
    #[test]
    fn a_fresh_console_is_automatic() {
        let mut t = Table::new(FOUR);
        assert_eq!(t.active(), 1);
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
        assert_eq!(t.mode(0), None);
        assert_eq!(t.mode(FOUR + 1), None);
        // Nothing promised, so nothing to send anywhere.
        assert_eq!(t.promise(1, Signal::Release), None);
        assert!(!t.reset(1));
    }

    /// `VT_SETMODE`'s refusals, each reason on its own so a policy that stops
    /// checking one of them fails a named test rather than a merged one. Only the
    /// first is Linux's.
    #[test]
    fn setmode_refuses_the_modes_it_refuses() {
        let mut t = Table::new(FOUR);
        assert_eq!(
            t.set_mode(
                1,
                VtMode {
                    mode: 7,
                    ..VtMode::AUTO
                },
                KEEPER
            ),
            Err(Some(ModeError::BadMode))
        );
        assert_eq!(
            t.set_mode(
                1,
                VtMode {
                    relsig: 32,
                    ..VtMode::AUTO
                },
                KEEPER
            ),
            Err(Some(ModeError::BadSignal))
        );
        assert_eq!(
            t.set_mode(1, process(10, 10), KEEPER),
            Err(Some(ModeError::SameSignal))
        );
        // Two zeroes are the same number and not the same request: that pair
        // means "no signal at all", which is legal.
        assert_eq!(
            t.set_mode(
                1,
                VtMode {
                    mode: VT_PROCESS,
                    ..VtMode::AUTO
                },
                KEEPER
            ),
            Ok(())
        );
        // A console this machine does not have is EINVAL, not a silent no-op.
        assert_eq!(t.set_mode(FOUR + 1, VtMode::AUTO, KEEPER), Err(None));
        assert_eq!(t.set_mode(0, VtMode::AUTO, KEEPER), Err(None));
        assert_eq!(
            t.mode(1),
            Some(VtMode {
                mode: VT_PROCESS,
                ..VtMode::AUTO
            })
        );
        t.set_mode(1, VtMode::AUTO, KEEPER).unwrap();
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
    }

    /// An accepted mode has to *read back*, or a compositor's screen is painted
    /// over by the text console one keystroke later and nothing was remembered.
    /// `frsig` is the field that does not read back: Linux ignores it.
    #[test]
    fn an_accepted_mode_reads_back() {
        let mut t = Table::new(FOUR);
        let m = process(10, 12);
        assert_eq!(t.set_mode(1, m, KEEPER), Ok(()));
        assert_eq!(t.mode(1), Some(m));
        assert_eq!(t.mode(2), Some(VtMode::AUTO));
        // "the frsig is ignored, so we set it to 0" - `vt_setmode`, measured.
        // A caller that named one (seatd names `SIGIO`) reads zero back, and no
        // refusal is ever sent with it.
        let with_frsig = VtMode {
            frsig: 29,
            ..process(10, 12)
        };
        assert_eq!(t.set_mode(2, with_frsig, KEEPER), Ok(()));
        assert_eq!(
            t.mode(2),
            Some(VtMode {
                frsig: 0,
                ..process(10, 12)
            })
        );
        // And a number that is not a signal is not refused either, for the same
        // reason: it is dropped before it can be stored or sent.
        assert_eq!(
            t.set_mode(
                3,
                VtMode {
                    frsig: 200,
                    ..process(10, 12)
                },
                KEEPER
            ),
            Ok(())
        );
        assert_eq!(t.mode(3).unwrap().frsig, 0);
    }

    /// The promise names its keeper: `relsig` and `acqsig` are aimed at the
    /// process that wrote `VT_SETMODE`, and nothing else. This is the whole of
    /// why a seat manager can set the mode on a console and close the descriptor,
    /// and why the signal cannot land on whichever session is in the foreground
    /// when it goes out.
    #[test]
    fn a_promise_names_the_process_that_made_it() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        assert_eq!(
            t.promise(1, Signal::Release),
            Some(Promise {
                sig: 10,
                pid: KEEPER
            })
        );
        assert_eq!(
            t.promise(1, Signal::Acquire),
            Some(Promise {
                sig: 12,
                pid: KEEPER
            })
        );
        // A signal the console never named is no promise to anyone.
        assert_eq!(t.promise(2, Signal::Release), None);
        // Leaving VT_PROCESS retires the keeper with the mode: a later promise is
        // a later process's, not this one's.
        t.set_mode(1, VtMode::AUTO, STRANGER).unwrap();
        assert_eq!(t.promise(1, Signal::Release), None);
    }

    /// A `VT_PROCESS` console whose keeper is gone - a promise recorded with no
    /// process behind it, which is what a keeper of 0 means here - has nobody to
    /// ask, so it must not own the screen for the length of a timer.
    #[test]
    fn a_promise_with_no_keeper_is_not_a_wait() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), 0).unwrap();
        assert_eq!(t.mode(1).unwrap().mode, VT_PROCESS);
        assert_eq!(t.promise(1, Signal::Release), None);
        assert_eq!(
            t.request_switch(3, 100, false).unwrap(),
            Decision::BeginRelease {
                release: Release {
                    from: 1,
                    to: 3,
                    started: 100
                },
                relsig: None
            }
        );
        assert_eq!(t.deadline(), Some(100));
        assert_eq!(t.expired(100), Some(3));
    }

    /// The whole point: leaving a `VT_PROCESS` console is a request, and the
    /// screen does not move until that console answers.
    #[test]
    fn switching_away_from_a_process_console_asks_first() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        let d = t.request_switch(3, 100, false).unwrap();
        assert_eq!(
            d,
            Decision::BeginRelease {
                release: Release {
                    from: 1,
                    to: 3,
                    started: 100
                },
                relsig: Some(Promise {
                    sig: 10,
                    pid: KEEPER
                })
            }
        );
        // Nothing has moved yet, and the same request now says "already asked".
        assert_eq!(t.active(), 1);
        assert!(t.is_releasing(1));
        assert!(!t.is_releasing(3));
        assert_eq!(
            t.request_switch(3, 120, false).unwrap(),
            Decision::Waiting {
                release: Release {
                    from: 1,
                    to: 3,
                    started: 100
                }
            }
        );
    }

    /// A session that answers gets what it asked for: the screen moves only at
    /// the answer, and the answer is what names the target.
    #[test]
    fn an_agreed_release_completes_the_switch() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(1, true), Ack::Complete { from: 1, to: 3 });
        assert!(!t.is_releasing(1));
        t.complete_switch(3);
        assert_eq!(t.active(), 3);
        // A second answer to the same request is EINVAL, not a second switch.
        assert_eq!(t.release_ack(1, true), Ack::NotReleasing);
    }

    /// `VT_RELDISP` is an answer to a question, so a console that was never
    /// asked cannot use it to take the screen.
    #[test]
    fn only_the_releasing_console_may_answer() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(2, true), Ack::NotReleasing);
        assert_eq!(t.release_ack(3, true), Ack::NotReleasing);
        assert_eq!(t.active(), 1);
    }

    /// A refusal holds the screen and is *repeated* to anyone else who asks.
    /// Nothing is sent: Linux ignores `frsig` and sends no refusal signal, and
    /// the requester's own `VT_ACTIVATE` is what answers `EBUSY`.
    #[test]
    fn a_refusal_holds_the_screen_until_its_owner_lets_go() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(1, false), Ack::Refused { to: 3 });
        assert_eq!(t.active(), 1);
        assert_eq!(t.refusing(), 1);
        // Nobody is waiting any more, so nothing will expire.
        assert_eq!(t.release(), None);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.expired(10_000), None);
        // Another console asking gets the refusal, and nothing else.
        assert_eq!(
            t.request_switch(4, 200, false).unwrap(),
            Decision::Refused { refusing: 1, to: 4 }
        );
        // The console that refused asking for its own screen back is the
        // one case that lifts it.
        assert_eq!(
            t.request_switch(1, 210, false).unwrap(),
            Decision::Already { to: 1 }
        );
        assert_eq!(t.refusing(), 0);
        assert_eq!(
            t.request_switch(4, 220, false).unwrap(),
            Decision::BeginRelease {
                release: Release {
                    from: 1,
                    to: 4,
                    started: 220
                },
                relsig: Some(Promise {
                    sig: 10,
                    pid: KEEPER
                })
            }
        );
    }

    /// The operator's escape hatch is a keystroke, not a timer: `^A` to the
    /// console already being asked forces the switch through without an answer.
    #[test]
    fn forcing_ends_the_negotiation_immediately() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(
            t.request_switch(3, 110, true).unwrap(),
            Decision::SwitchNow { to: 3 }
        );
        assert_eq!(t.release(), None);
        t.complete_switch(3);
        assert_eq!(t.active(), 3);
    }

    /// A release with no signal to wait for was never going to be answered, so
    /// it is already overdue rather than owning the screen for two seconds.
    #[test]
    fn a_release_nobody_can_answer_expires_at_once() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(0, 12), KEEPER).unwrap();
        t.request_switch(2, 100, false).unwrap();
        assert_eq!(t.deadline(), Some(100));
        assert_eq!(t.expired(100), Some(2));
        assert_eq!(t.release(), None);
    }

    /// And one with a signal gets the whole window - a compositor mid-flip
    /// keeps its frames, a compositor that is gone does not keep the machine.
    #[test]
    fn a_signalled_release_gets_the_whole_window() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(2, 0, false).unwrap();
        assert_eq!(t.deadline(), Some(RELEASE_TIMEOUT_NS));
        assert_eq!(t.expired(RELEASE_TIMEOUT_NS - 1), None);
        assert_eq!(t.expired(RELEASE_TIMEOUT_NS), Some(2));
    }

    /// `VT_AUTO` is the mode nothing here disturbs: the default machine
    /// switches exactly as it did before the negotiation existed.
    #[test]
    fn an_automatic_console_switches_without_being_asked() {
        let mut t = Table::new(FOUR);
        assert_eq!(
            t.request_switch(4, 0, false).unwrap(),
            Decision::SwitchNow { to: 4 }
        );
        t.complete_switch(4);
        assert_eq!(t.active(), 4);
        assert_eq!(
            t.request_switch(4, 1, false).unwrap(),
            Decision::Already { to: 4 }
        );
    }

    /// A number that is not one of this machine's consoles is `EINVAL`, from
    /// whichever door is asked.
    #[test]
    fn a_console_that_does_not_exist_is_refused() {
        let mut t = Table::new(FOUR);
        assert_eq!(t.request_switch(0, 0, false), None);
        assert_eq!(t.request_switch(FOUR + 1, 0, false), None);
        t.complete_switch(9);
        assert_eq!(t.expired(0), None);
    }

    /// A promise is ended by its keeper writing `VT_AUTO`, or by the layer
    /// noticing the keeper is gone - and by nothing else. Nothing in this table
    /// ends one on a descriptor close, because the process that makes it (a seat
    /// manager) closes that descriptor before the screen is anyone's: an
    /// end-on-close rule of any shape erases the claim in the same breath as it is
    /// made, which is how `VT_PROCESS` becomes a mode that does nothing.
    #[test]
    fn a_promise_ends_only_when_its_keeper_hands_it_back() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();

        // A switch that goes through, a refusal, an expiry: none of them quietly
        // drop the promise, because it is not the negotiation's to give up.
        assert!(t.is_releasing(1));
        assert_eq!(
            t.promise(1, Signal::Release),
            Some(Promise {
                sig: 10,
                pid: KEEPER
            })
        );
        assert_eq!(t.release_ack(1, false), Ack::Refused { to: 3 });
        assert_eq!(t.refusing(), 1);
        assert_eq!(
            t.promise(1, Signal::Acquire),
            Some(Promise {
                sig: 12,
                pid: KEEPER
            })
        );

        // A second process writing the mode replaces the promise wholesale - the
        // keeper changes with it, which is Linux's `put_pid(old); vt_pid = new`.
        t.set_mode(1, process(10, 12), STRANGER).unwrap();
        assert_eq!(
            t.promise(1, Signal::Release),
            Some(Promise {
                sig: 10,
                pid: STRANGER
            })
        );

        // And the keeper's own word ends it.
        t.set_mode(1, VtMode::AUTO, STRANGER).unwrap();
        assert_eq!(t.promise(1, Signal::Release), None);
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
        // ...or the layer discovering there is nobody to ask.
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 200, false).unwrap();
        assert!(t.is_releasing(1));
        assert!(t.reset(1));
        assert_eq!(t.release(), None);
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
    }

    /// The other way a promise dies: the keeper is noticed gone at the moment a
    /// signal would have gone to it, which is Linux's `kill_pid` failure followed
    /// by `reset_vc`. Everything the console was holding up has to stop being
    /// held up, or one dead compositor strands the machine.
    #[test]
    fn resetting_a_console_takes_the_promise_and_the_wait() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12), KEEPER).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert!(t.is_releasing(1));
        // A console that was already automatic has nothing to take away.
        assert!(!t.reset(2));
        assert!(t.reset(1));
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
        assert_eq!(t.promise(1, Signal::Release), None);
        assert_eq!(t.promise(1, Signal::Acquire), None);
        assert_eq!(t.release(), None);
        assert_eq!(t.deadline(), None);
        // And the acqsig that would have fired when the screen came to console 1
        // has nowhere to go either.
        assert_eq!(
            t.request_switch(3, 200, false).unwrap(),
            Decision::SwitchNow { to: 3 }
        );
    }
}
