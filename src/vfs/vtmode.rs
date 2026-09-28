//! The virtual-console handover: who may switch, and what has to happen first.
//!
//! Like [`crate::evdev`], this module is the *policy* half of a bare-metal
//! device lifted out so a host build can argue with it. The device half is
//! `crate::vfs::vcmux`, which owns the clock, the tasks and the signal
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
//!   the console releasing, sends its foreground group `relsig`, and does not
//!   move the screen until that session calls `VT_RELDISP` to agree (or
//!   `VT_RELDISP(0)` to refuse). A session that wants the screen at all - which
//!   is any compositor - is in this mode, because the alternative is a text
//!   console painting over its frames between two keystrokes.
//!
//! Two things make the mode worth its complexity, and both are the reason it is
//! recorded rather than ignored:
//!
//! * **The wait is bounded.** A session that is killed, or that simply never
//!   installed the handler its `relsig` names, would otherwise strand every
//!   later switch - including the operator's own. So a release carries a
//!   deadline ([`RELEASE_TIMEOUT_TICKS`], resolved by the caller, which owns the
//!   clock), and the escape hatch is not the clock at all: a *second* `^A` to
//!   the same console forces it through. Neither depends on the session
//!   cooperating.
//! * **One negotiation at a time.** A refused release ends it rather than
//!   parking a queue, so there is at most one [`Release`] in flight, not a
//!   pending stack per console.
//!
//! The `EINVAL` set of `VT_SETMODE` is Linux's: an unknown mode, `relsig ==
//! acqsig`, or a signal number that is not a signal. And like Linux, the
//! console *on the screen* is the one that gets consulted - which is what makes
//! `VT_SETMODE` a promise about this session's own screen rather than an
//! instruction to whoever happens to be holding it.

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

/// How long a release may be waited for, in clock ticks: Linux's
/// `wait_event_interruptible_timeout(..., 60 * HZ)` in `vt_release`, counted
/// from the tick the `relsig` went out. This kernel's clock is 250 Hz, so the
/// figure is 15000 ticks.
///
/// The number is Linux's because the wait it bounds is Linux's wait, and a
/// shorter one would silently let a compositor be killed mid-page-flip on a
/// machine that is merely slow. Nothing here depends on it for reachability:
/// the operator's escape is `^A` typed twice, which forces the switch on the
/// spot, and a session's escape is that its own `relsig` handler answers
/// `VT_RELDISP`. Both are faster than the timer by design, so the timer is the
/// last resort rather than the ordinary path - and the ordinary path for a
/// *person* (the console mux's own wait) is a kernel task that re-examines the
/// negotiation whether or not anybody answers.
pub const RELEASE_TIMEOUT_TICKS: u64 = 60 * 250;

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
    pub fn validate(&self) -> Result<(), ModeError> {
        if self.mode != VT_AUTO && self.mode != VT_PROCESS {
            return Err(ModeError::BadMode);
        }
        if !signal_ok(self.relsig) || !signal_ok(self.acqsig) || !signal_ok(self.frsig) {
            return Err(ModeError::BadSignal);
        }
        // Linux refuses this outright: one process group cannot be told two
        // opposite things by the same number, and a session that meant
        // "no signal" for both would have written zeroes.
        if self.relsig != 0 && self.relsig == self.acqsig {
            return Err(ModeError::SameSignal);
        }
        Ok(())
    }
}

/// Why a `VT_SETMODE` was refused. Each of these is `EINVAL` to the caller;
/// the split exists so a host test can pin that the *reason* was checked, not
/// just that something failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeError {
    /// `mode` is neither `VT_AUTO` nor `VT_PROCESS`.
    BadMode,
    /// A signal field names something that is not a signal.
    BadSignal,
    /// `relsig == acqsig`.
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
    /// not that console. `frsig` goes to `to`'s foreground group, if it named
    /// one.
    Refused {
        refusing: u32,
        to: u32,
        frsig: Option<u8>,
    },
    /// You are the console on the screen, asking for the screen you already
    /// have: `VT_ACTIVATE`'s successful no-op, and the one case where a
    /// standing refusal is lifted.
    Already { to: u32 },
    /// The console on the screen is in `VT_PROCESS`: send `relsig` to its
    /// foreground group and leave the screen where it is until it answers.
    BeginRelease {
        release: Release,
        relsig: Option<u8>,
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

    /// `VT_SETMODE`: record a console's mode.
    ///
    /// `None` means `vc` is not one of this machine's consoles.
    ///
    /// Leaving `VT_PROCESS` clears a negotiation this console was holding up: a
    /// session that stops asking to be consulted has, by that act, agreed. That
    /// is what makes `^A` work again after a compositor exits without cleaning
    /// up after itself.
    pub fn set_mode(&mut self, vc: u32, mode: VtMode) -> Result<(), Option<ModeError>> {
        if vc == 0 || self.modes.get(vc as usize).is_none() {
            return Err(None);
        }
        mode.validate().map_err(Some)?;
        self.modes[vc as usize] = mode;
        if mode.mode != VT_PROCESS {
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
                frsig: self.signal(self.refusing, |m| m.frsig),
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
            relsig: self.signal(release.from, |m| m.relsig),
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
        if self.signal(release.from, |m| m.relsig).is_none() {
            return Some(release.started);
        }
        Some(release.started + RELEASE_TIMEOUT_TICKS)
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

    /// A negotiation that will never be answered, ended by the console's owner
    /// letting go of the terminal - a getty respawning over a dead session, a
    /// compositor's fd closing. Without it the next `^A` waits on a ghost.
    ///
    /// The mode goes back to `VT_AUTO` with it, which is what Linux does when
    /// the last descriptor of a session closes: a promise to answer `relsig`
    /// belongs to a process, and the process is gone. Leaving `VT_PROCESS`
    /// behind would strand the next switch on a promise nobody is alive to keep.
    pub fn abandon(&mut self, vc: u32) {
        if let Some(release) = self.release
            && release.from == vc
        {
            self.release = None;
        }
        if self.refusing == vc {
            self.refusing = 0;
        }
        if vc != 0 && self.modes.len() > vc as usize {
            self.modes[vc as usize] = VtMode::AUTO;
        }
    }

    fn signal(&self, vc: u32, which: impl Fn(&VtMode) -> u8) -> Option<u8> {
        self.modes.get(vc as usize).map(which).filter(|s| *s != 0)
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
        let t = Table::new(FOUR);
        assert_eq!(t.active(), 1);
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
        assert_eq!(t.mode(0), None);
        assert_eq!(t.mode(FOUR + 1), None);
    }

    /// `VT_SETMODE`'s `EINVAL` set, each reason on its own so a policy that
    /// stops checking one of them fails a named test rather than a merged one.
    #[test]
    fn setmode_refuses_the_modes_linux_refuses() {
        let mut t = Table::new(FOUR);
        assert_eq!(
            t.set_mode(
                1,
                VtMode {
                    mode: 7,
                    ..VtMode::AUTO
                }
            ),
            Err(Some(ModeError::BadMode))
        );
        assert_eq!(
            t.set_mode(
                1,
                VtMode {
                    frsig: 32,
                    ..VtMode::AUTO
                }
            ),
            Err(Some(ModeError::BadSignal))
        );
        assert_eq!(
            t.set_mode(1, process(10, 10)),
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
                }
            ),
            Ok(())
        );
        // A console this machine does not have is EINVAL, not a silent no-op.
        assert_eq!(t.set_mode(FOUR + 1, VtMode::AUTO), Err(None));
        assert_eq!(t.set_mode(0, VtMode::AUTO), Err(None));
        assert_eq!(
            t.mode(1),
            Some(VtMode {
                mode: VT_PROCESS,
                ..VtMode::AUTO
            })
        );
        t.set_mode(1, VtMode::AUTO).unwrap();
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
    }

    /// An accepted mode has to *read back*, or a compositor's screen is painted
    /// over by the text console one keystroke later and nothing was remembered.
    #[test]
    fn an_accepted_mode_reads_back() {
        let mut t = Table::new(FOUR);
        let m = process(10, 12);
        assert_eq!(t.set_mode(1, m), Ok(()));
        assert_eq!(t.mode(1), Some(m));
        assert_eq!(t.mode(2), Some(VtMode::AUTO));
    }

    /// The whole point: leaving a `VT_PROCESS` console is a request, and the
    /// screen does not move until that console answers.
    #[test]
    fn switching_away_from_a_process_console_asks_first() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12)).unwrap();
        let d = t.request_switch(3, 100, false).unwrap();
        assert_eq!(
            d,
            Decision::BeginRelease {
                release: Release {
                    from: 1,
                    to: 3,
                    started: 100
                },
                relsig: Some(10)
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
        t.set_mode(1, process(10, 12)).unwrap();
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
        t.set_mode(1, process(10, 12)).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(2, true), Ack::NotReleasing);
        assert_eq!(t.release_ack(3, true), Ack::NotReleasing);
        assert_eq!(t.active(), 1);
    }

    /// A refusal holds the screen and is *repeated* to anyone else who asks,
    /// with `frsig` - which is how a dead compositor stops being a machine no
    /// one can move.
    #[test]
    fn a_refusal_holds_the_screen_until_its_owner_lets_go() {
        let mut t = Table::new(FOUR);
        t.set_mode(
            1,
            VtMode {
                frsig: 21,
                ..process(10, 12)
            },
        )
        .unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(1, false), Ack::Refused { to: 3 });
        assert_eq!(t.active(), 1);
        assert_eq!(t.refusing(), 1);
        // Nobody is waiting any more, so nothing will expire.
        assert_eq!(t.release(), None);
        assert_eq!(t.deadline(), None);
        assert_eq!(t.expired(10_000), None);
        // Another console asking gets the refusal, and tty1's `frsig`.
        assert_eq!(
            t.request_switch(4, 200, false).unwrap(),
            Decision::Refused {
                refusing: 1,
                to: 4,
                frsig: Some(21)
            }
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
                relsig: Some(10)
            }
        );
    }

    /// The operator's escape hatch is a keystroke, not a timer: `^A` to the
    /// console already being asked forces the switch through without an answer.
    #[test]
    fn forcing_ends_the_negotiation_immediately() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12)).unwrap();
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
        t.set_mode(1, process(0, 12)).unwrap();
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
        t.set_mode(1, process(10, 12)).unwrap();
        t.request_switch(2, 0, false).unwrap();
        assert_eq!(t.deadline(), Some(RELEASE_TIMEOUT_TICKS));
        assert_eq!(t.expired(RELEASE_TIMEOUT_TICKS - 1), None);
        assert_eq!(t.expired(RELEASE_TIMEOUT_TICKS), Some(2));
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

    /// A session that exits without putting its mode back must not strand the
    /// screen: letting go of the console ends the wait it started, and the next
    /// request is forced rather than negotiated, because there is nobody left
    /// to answer.
    #[test]
    fn abandoning_a_console_ends_the_wait_it_started() {
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12)).unwrap();
        t.request_switch(3, 100, false).unwrap();
        t.abandon(1);
        assert_eq!(t.release(), None);
        assert_eq!(t.mode(1), Some(VtMode::AUTO));
        assert_eq!(
            t.request_switch(3, 300, false).unwrap(),
            Decision::SwitchNow { to: 3 }
        );
        // Dropping back to VT_AUTO does the same thing from the other side.
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12)).unwrap();
        t.request_switch(3, 100, false).unwrap();
        t.set_mode(1, VtMode::AUTO).unwrap();
        assert_eq!(t.release(), None);
        // And a console that refused, then lost its session, stops refusing:
        // the veto belonged to a process that is gone.
        let mut t = Table::new(FOUR);
        t.set_mode(1, process(10, 12)).unwrap();
        t.request_switch(3, 100, false).unwrap();
        assert_eq!(t.release_ack(1, false), Ack::Refused { to: 3 });
        assert_eq!(t.refusing(), 1);
        t.abandon(1);
        assert_eq!(t.refusing(), 0);
    }
}
