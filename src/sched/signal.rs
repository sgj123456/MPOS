//! The signal numbers, the pending-bit arithmetic, and each process's
//! dispositions.
//!
//! Delivery is the runtime's business; this module is only the vocabulary, kept
//! on its own so host tests can pin the numbers and the bit math without a
//! task table. The set is deliberately small: what the kernel itself raises
//! (SIGPIPE on a broken pipe) and what a process could be told (SIGINT,
//! SIGTERM, SIGKILL). More arrive with the tty's foreground-process concept.
//!
//! A disposition is both recorded (`rt_sigaction`) and *honoured*: the syscall
//! return path builds an `rt_sigframe` for a handler (`build_frame`,
//! `crate::syscall::linux::proc`) and `rt_sigreturn` comes back out of it, so
//! "delivered" and "dies of it" are no longer the same thing. What a signal
//! does when the task has no handler is the [`default_action`] table, which is
//! why that table exists at all rather than a single "fatal" answer.

/// Interrupt from the keyboard, once there is a keyboard.
pub const SIGINT: u8 = 2;
/// The hardware's trap, and the signal `ptrace` reports a stop with. Every
/// stop a tracer sees is one of these: a syscall boundary, an `exec`, a
/// signal that arrived, or a single step. It is also the one a `SIGTRAP`
/// handler would run for, though nothing here raises it any other way.
pub const SIGTRAP: u8 = 5;
/// The keyboard's quit — `VQUIT`, `^\`, a `SIGINT` whose default is more
/// forceful. On Linux this is where a core dump comes from; this kernel has
/// no dump to write, so the default action is the plain terminate every
/// other unnamed signal answers with, and a program that wants the
/// distinction catches it.
pub const SIGQUIT: u8 = 3;
/// Termination, the kind that cannot be caught, blocked or ignored.
pub const SIGKILL: u8 = 9;
/// The two the kernel has no opinion about, which is precisely why the console
/// uses them: a virtual terminal in `VT_PROCESS` mode is told to hand the
/// screen back by `relsig` and told it has the screen again by `acqsig`, and
/// those are `SIGUSR1` and `SIGUSR2` in every seat manager's `struct vt_mode`.
/// Fatal by default - a session that wants to keep its display must install a
/// handler, which is the agreement `VT_SETMODE` makes.
pub const SIGUSR1: u8 = 10;
pub const SIGUSR2: u8 = 12;
/// Write on a pipe with no one left to read it.
pub const SIGPIPE: u8 = 13;
/// The clock a program asked for: `alarm`, `setitimer(ITIMER_REAL)`.
pub const SIGALRM: u8 = 14;
/// Termination, the polite kind.
pub const SIGTERM: u8 = 15;
/// A syscall was made that a `SECCOMP_MODE_FILTER` program refused with
/// `SECCOMP_RET_TRAP`. It terminates by default like any other unnamed signal,
/// and the `siginfo_t` a handler is given names the call in `si_syscall` - a
/// program that catches it is expected to be one that installed the filter.
pub const SIGSYS: u8 = 31;
/// A child has stopped or exited. The default action ignores it - which is
/// why a kernel can boot without ever queueing one - but a shell that
/// installs a handler of its own waits for exactly this signal, and a task
/// exit that only wakes a `wait(2)`-blocked parent leaves that parent
/// asleep for ever.
pub const SIGCHLD: u8 = 17;
/// Continue. The default action is to resume a stopped task, which is the
/// only thing that undoes a stop; it also clears pending stop signals, so a
/// `SIGSTOP` followed by a `SIGCONT` does not leave the task stopping again
/// the moment it resumes.
pub const SIGCONT: u8 = 18;
/// Stop, the other kind that cannot be caught.
pub const SIGSTOP: u8 = 19;
/// The keyboard's stop, and its two relatives from the tty: a background job
/// reading or writing a terminal it does not own. All three stop by default,
/// like `SIGSTOP`, and all three are catchable, unlike it.
pub const SIGTSTP: u8 = 20;
pub const SIGTTIN: u8 = 21;
pub const SIGTTOU: u8 = 22;
/// The terminal's window changed size. Nothing happens by default - a program
/// that cares catches it and redraws, and one that does not carries on - which
/// is why it joins `SIGCHLD` as the table's other "do nothing".
pub const SIGWINCH: u8 = 28;

/// How many signal numbers a process can hold a disposition for: the zero slot
/// plus `1..=64` (`_NSIG`), so a signal number indexes its own entry. `Pending`
/// is a `u64` in the canonical layout (signal `n` at bit `n-1`), so all of
/// `1..=64` are nameable — the standard set `1..=31` and the real-time range
/// `SIGRTMIN..=SIGRTMAX` alike.
pub const SIGNALS: usize = 65;

/// The highest signal number Linux names (`_NSIG`). Anything above it is
/// `EINVAL`; `0` is the `kill`-existence probe, handled before this bound.
pub const MAX_SIGNAL: u8 = 64;

/// `SIGRTMIN`, the first real-time signal and the one musl's `SIGEV_THREAD`
/// timers target: a kernel without it is one on which every
/// `timer_create`-with-a-callback program — vim among them — fails its
/// timeout setup and dies on the redraw path that follows the failure.
pub const SIGRTMIN: u8 = 32;

/// `SIGRTMAX`, the last real-time signal and the last bit the canonical
/// `Pending` word can hold (signal `n` at bit `n-1`, so signal 64 is bit 63).
pub const SIGRTMAX: u8 = 64;

/// How many `siginfo_t` slots a task keeps: one per index `0..=64`, so every
/// nameable signal has a record slot — the standard signals coalesce into
/// theirs, and the timer expiry that targets a thread is handed back complete
/// with `SI_TIMER` in `si_code`, the field musl's helper thread checks before
/// it calls the callback.
pub const SIGINFO_SLOTS: usize = SIGNALS;

/// The two dispositions that are not a function address: take the default
/// action, or do nothing at all.
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

/// `SA_*` bits the kernel acts on rather than merely records.
///
/// `SA_RESTORER` is not optional on x86_64 - a handler cannot return on its
/// own, and musl passes the trampoline address that leads back to the kernel
/// through `rt_sigreturn` in this field.
pub const SA_RESTORER: usize = 0x0400_0000;
pub const SA_ONSTACK: usize = 0x0800_0000;
pub const SA_NODEFER: usize = 0x4000_0000;
pub const SA_RESETHAND: usize = 0x8000_0000;

/// `SA_RESTART`: the caller asked the interrupted syscall to be run again after
/// its handler returns, rather than to come back as `EINTR`.
///
/// Honoured, and the half of it that is decided here is just the flag: the
/// *rewind* belongs to the frame `build_frame` writes, because that frame is
/// the record of the call's own arguments. `strace` on Linux 7.2 sees
/// `read(...) = ? ERESTARTSYS`, then `rt_sigreturn({mask=[]}) = 0` followed by
/// a genuine second `read` with the caller's original arguments; here the
/// frame points its saved `rip` back at the `syscall` instruction (two bytes
/// below the return address) with the call's number in `rax`, so `rt_sigreturn`
/// resumes on the call rather than after it. The other half - which calls are
/// rewound at all - is [`crate::syscall::linux::abi::restartable`], where the
/// measured never-restarted set (`poll`, `select`, `epoll_wait`, the sleeps)
/// lives.
pub const SA_RESTART: usize = 0x1000_0000;

/// The bytes of `struct rt_sigframe` on x86_64, which is what the kernel
/// pushes on a handler's stack:
///
/// ```text
///   0  char *pretcode      the restorer - where the handler returns to
///   8  struct ucontext     uc_flags, uc_link, stack_t, sigcontext, sigmask
/// 312  struct siginfo      signo, errno, code, and a lot of padding
/// ```
///
/// The three parts are laid out the way the Linux ABI lays them out, because
/// the libc's `__restore_rt` trampoline is what reads them back: it issues
/// `rt_sigreturn` with `rsp` pointing just past `pretcode`, and the kernel
/// finds the saved registers at a fixed offset from there.
pub const SIGFRAME_BYTES: usize = 440;
/// Offset of `uc_mcontext` (a `struct sigcontext`) inside the frame - that is,
/// `sizeof(char *) + sizeof(unsigned long) + sizeof(uc_link) + sizeof(stack_t)`.
pub const SIGFRAME_UCONTEXT: usize = 8;
/// Offset of the `struct sigcontext` within the `ucontext`.
pub const UCONTEXT_MCONTEXT: usize = 40;
/// Offset of the saved signal mask within the `ucontext`.
pub const UCONTEXT_SIGMASK: usize = 296;
/// Offset of the `struct siginfo` within the frame.
pub const SIGFRAME_SIGINFO: usize = 312;

/// One signal's disposition, in the shape `rt_sigaction` reads and writes.
///
/// The zero value is `SIG_DFL` with no flags, no restorer and an empty mask -
/// which is exactly what a query of an untouched disposition has to report, so
/// a fresh process can simply start with zeroed slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Action {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or a user-mode function address.
    pub handler: usize,
    /// `SA_*` bits. Kept verbatim: the kernel itself acts on none of them yet.
    pub flags: usize,
    /// Where musl's trampoline lives, handed back to a handler that runs.
    pub restorer: usize,
    /// The mask a handler would run under.
    pub mask: u64,
}

/// A process's dispositions, indexed by signal number.
#[derive(Clone, Copy, Debug)]
pub struct Actions([Action; SIGNALS]);

impl Default for Actions {
    fn default() -> Actions {
        Actions([Action::default(); SIGNALS])
    }
}

impl Action {
    /// Run the signal's default action, whatever the [`default_action`] table
    /// says that is: for most of them terminate, but not for the stop and
    /// resume group, which is the reason this is not simply "fatal".
    pub fn is_default(&self) -> bool {
        self.handler == SIG_DFL
    }

    /// Discard the signal.
    pub fn is_ignore(&self) -> bool {
        self.handler == SIG_IGN
    }

    /// Calls `self.handler`. Every address above `SIG_IGN` is one, since the
    /// two sentinels are 0 and 1.
    pub fn is_handler(&self) -> bool {
        self.handler > SIG_IGN
    }

    /// The mask a handler runs under: what the caller had blocked, plus what
    /// the disposition asks for, plus the signal itself - which cannot arrive
    /// again while it is being handled unless `SA_NODEFER` says otherwise.
    pub fn handler_mask(&self, sig: u8, blocked: Pending) -> Pending {
        let mut mask = blocked | self.mask as Pending;
        if self.flags & SA_NODEFER == 0 {
            mask |= bit(sig);
        }
        mask
    }
}

impl Actions {
    /// What would happen if `sig` arrived now. Signals the kernel cannot name
    /// answer `SIG_DFL`, which is the truth about them too.
    pub fn get(&self, sig: u8) -> Action {
        self.0.get(sig as usize).copied().unwrap_or_default()
    }

    /// Record a disposition. Out-of-range numbers are silently ignored: the
    /// callers validate first, and there is no slot to write.
    pub fn set(&mut self, sig: u8, action: Action) {
        if let Some(slot) = self.0.get_mut(sig as usize) {
            *slot = action;
        }
    }
}

/// Whether `sig` can have a disposition at all: Linux refuses signal 0, the
/// numbers above `_NSIG` (64), and the two the kernel reserves for itself.
pub fn catchable(sig: usize) -> bool {
    (1..SIGNALS).contains(&sig) && sig != SIGKILL as usize && sig != SIGSTOP as usize
}

/// What a signal does when no handler is installed for it.
///
/// The module's opening comment used to say every signal here was fatal by
/// default, and for the set it named that was true. A tracer needs the two
/// that are not: `SIGSTOP` stops rather than kills, and `SIGCONT` is the only
/// way back from a stop. Spelling the table out is what keeps a `kill -STOP`
/// from being a `kill -9` with a misleading name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAction {
    /// Die of it, the number in the exit code.
    Terminate,
    /// Hold the task where it stands until something resumes it. The
    /// something is `SIGCONT` or a `PTRACE_CONT`, and the hold is what
    /// `wait(2)` reports as `WIFSTOPPED`.
    Stop,
    /// Resume a stopped task. The signal itself means nothing else: a task
    /// that is not stopped ignores it.
    Continue,
    /// Do nothing at all. `SIGCHLD` is here because a `wait(2)` needs no
    /// signal to be woken - the parent wake is direct - so queueing it would
    /// only make a shell's `wait` return `EINTR` for a death it just reaped.
    Ignore,
}

/// The default action table, as `signal(7)` spells it.
///
/// Everything not named terminates: that is the honest default for the
/// signals this kernel can name, and the ones a program actually catches
/// (`SIGINT`, `SIGTERM`, `SIGPIPE`, `SIGALRM`) are in that group.
pub fn default_action(sig: u8) -> DefaultAction {
    match sig {
        SIGCHLD | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// Whether `sig`'s default action is to stop the task.
pub fn stops_by_default(sig: u8) -> bool {
    default_action(sig) == DefaultAction::Stop
}

/// The alternate stack `sigaltstack` installs, if any.
///
/// `sp` is the region's base, as the caller gave it - not its top, which is
/// where a stack pointer would start. A handler installed with `SA_ONSTACK`
/// runs on it, which is what `build_frame` switches the frame's `rsp` to
/// (`crate::syscall::linux::proc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltStack {
    pub sp: usize,
    pub size: usize,
}

/// The smallest alternate stack Linux accepts; a smaller one could not hold a
/// signal frame, and Linux answers `ENOMEM` rather than truncating it.
pub const MINSIGSTKSZ: usize = 2048;

/// `si_code` values: what a handler reads out of `siginfo_t` to tell one
/// arrival from another. `SI_USER` is a bare `kill`; `SI_QUEUE` is `sigqueue`
/// carrying a payload; `SI_TIMER` is a POSIX interval timer; `SI_KERNEL` is
/// the hardware raising a fault.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_QUEUE: i32 = -1;
pub const SI_TIMER: i32 = -2;

/// The part of a `siginfo_t` this kernel can honour for the signals it names:
/// the number, where it came from, and - for `sigqueue` - the one word of the
/// caller's payload that survives to the handler. Standard signals are not
/// queued, so one record per signal number is exactly the storage the ABI
/// implies, and it is why this is a slot in an array rather than a list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigInfo {
    pub signo: i32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: i32,
    /// The `union sigval` a `sigqueue` sent, or zero for a plain `kill`.
    pub value: u64,
}

impl SigInfo {
    /// The zero record: no signal. An empty pending slot looks like this.
    pub const ZERO: SigInfo = SigInfo {
        signo: 0,
        errno: 0,
        code: 0,
        pid: 0,
        uid: 0,
        value: 0,
    };

    /// A `kill`/`tgkill` record: `SI_USER`, from `pid`/`uid`, no payload.
    pub fn user(sig: u8, pid: i32, uid: i32) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code: SI_USER,
            pid,
            uid,
            value: 0,
        }
    }

    /// A `sigqueue` record: `SI_QUEUE` carrying the caller's `value`.
    pub fn queue(sig: u8, pid: i32, uid: i32, value: u64) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code: SI_QUEUE,
            pid,
            uid,
            value,
        }
    }

    /// A record for a signal the kernel raised itself - a fault, a broken pipe.
    pub fn kernel(sig: u8) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code: SI_KERNEL,
            pid: 0,
            uid: 0,
            value: 0,
        }
    }

    /// A record for a `setitimer`/timerfd expiry: `SI_TIMER`, no payload.
    pub fn timer(sig: u8) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code: SI_TIMER,
            pid: 0,
            uid: 0,
            value: 0,
        }
    }
}

/// A pending-signal set: one bit per signal number, with bit *n* naming
/// signal *n*.
///
/// The layout is the kernel's own, and deliberately not the wire's: a libc's
/// `sigset_t` holds signal *n* in bit *n-1*, and every syscall that crosses
/// the boundary converts through [`from_wire`] / [`to_wire`] rather than
/// storing one side's bits under the other side's reading. The two spellings
/// are one shift apart, and a kernel that mixed them would block the
/// neighbouring signal to the one asked for — an off-by-one that no test
/// inside the kernel could see, because both sides of it speak the same
/// wrong language. The ABI tests pin the *wire* half by number.
pub type Pending = u64;

/// The bit a signal occupies in the kernel's `Pending` set.
///
/// Canonical Linux layout: signal *n* is bit *n-1*, so signals `1..=64` fill the
/// `u64` end to end (`_NSIG`), with no spare bit and no collision. Signal 0 has no
/// bit (it is the `kill`-existence probe, never pending).
pub fn bit(sig: u8) -> Pending {
    if sig == 0 { 0 } else { 1u64 << (sig - 1) }
}

pub fn add(set: &mut Pending, sig: u8) {
    *set |= bit(sig);
}

/// A libc's `sigset_t` as it arrives on the syscall boundary: signal *n* in
/// bit *n-1*.
///
/// The kernel stores the pending set in that same layout (see [`bit`]), so the
/// conversion is the identity — the wire and the internal set are one and the
/// same shape, which is why a disposition's `sa_mask` (kept in wire form) and
/// `blocked` (the pending shape) can be combined without a shift.
pub fn from_wire(raw: u64) -> Pending {
    raw
}

/// The kernel's pending set as a libc's `sigset_t`: signal *n* back into bit
/// *n-1*, which is the shape every caller of `rt_sigprocmask`'s `oldset` and
/// `rt_sigpending` reads. Identity for the same reason as [`from_wire`].
pub fn to_wire(set: Pending) -> u64 {
    set
}

/// Take the lowest-numbered pending signal out of the set, if any.
pub fn take(set: &mut Pending) -> Option<u8> {
    if *set == 0 {
        return None;
    }
    let sig = (*set).trailing_zeros() as u8 + 1;
    *set &= !bit(sig);
    Some(sig)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_pinned() {
        // These are the traditional values; the libc will bind them as is.
        assert_eq!(SIGINT, 2);
        assert_eq!(SIGQUIT, 3);
        assert_eq!(SIGTRAP, 5);
        assert_eq!(SIGKILL, 9);
        assert_eq!(SIGUSR1, 10);
        assert_eq!(SIGUSR2, 12);
        assert_eq!(SIGPIPE, 13);
        assert_eq!(SIGTERM, 15);
        assert_eq!(SIGCHLD, 17);
        assert_eq!(SIGCONT, 18);
        assert_eq!(SIGSTOP, 19);
        assert_eq!(SIGTSTP, 20);
        assert_eq!(SIGWINCH, 28);
    }

    /// `signal(7)`: the default action of a signal with no handler is what
    /// decides whether `kill -STOP` stops and `kill -CONT` resumes, or is a
    /// `kill -9` wearing the wrong name.
    #[test]
    fn the_default_action_table_matches_signal_7() {
        assert_eq!(default_action(SIGINT), DefaultAction::Terminate);
        assert_eq!(default_action(SIGTRAP), DefaultAction::Terminate);
        assert_eq!(default_action(SIGKILL), DefaultAction::Terminate);
        assert_eq!(default_action(SIGTERM), DefaultAction::Terminate);
        assert_eq!(default_action(SIGCHLD), DefaultAction::Ignore);
        // A resize must not kill the program being resized, so this is the
        // other number whose default action is to do nothing.
        assert_eq!(default_action(SIGWINCH), DefaultAction::Ignore);
        assert_eq!(default_action(SIGCONT), DefaultAction::Continue);
        // The console's handover signals kill a session that did not install a
        // handler, and that is the point: `VT_SETMODE(VT_PROCESS)` is a
        // promise to answer `VT_RELDISP`, and a session that cannot keep it
        // must not be trusted with the screen.
        assert_eq!(default_action(SIGUSR1), DefaultAction::Terminate);
        assert_eq!(default_action(SIGUSR2), DefaultAction::Terminate);
        for sig in [SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
            assert_eq!(default_action(sig), DefaultAction::Stop);
            assert!(stops_by_default(sig));
        }
        assert!(!stops_by_default(SIGINT));
    }

    /// `SIGKILL` and `SIGSTOP` are the two the kernel keeps for itself: a
    /// `rt_sigaction` for either must not take, or a process could make
    /// itself unstoppable (or unkillable).
    #[test]
    fn only_uncatchable_signals_are_refused_a_disposition() {
        assert!(!catchable(SIGKILL as usize));
        assert!(!catchable(SIGSTOP as usize));
        assert!(!catchable(0));
        // The real-time range is installable from stage B on: SIGRTMIN (32) and
        // SIGRTMAX (64) are catchable, while 0, >_NSIG, and SIGKILL/SIGSTOP are
        // not.
        assert!(catchable(32));
        assert!(catchable(64));
        assert!(!catchable(65));
        assert!(catchable(SIGTRAP as usize));
        assert!(catchable(SIGCONT as usize));
        assert!(catchable(SIGTSTP as usize));
    }

    #[test]
    fn take_returns_the_lowest_signal_first() {
        let mut set = 0;
        add(&mut set, SIGTERM);
        add(&mut set, SIGINT);
        assert_eq!(take(&mut set), Some(SIGINT));
        assert_eq!(take(&mut set), Some(SIGTERM));
        assert_eq!(take(&mut set), None);
    }

    #[test]
    fn adding_twice_still_takes_once() {
        let mut set = 0;
        add(&mut set, SIGKILL);
        add(&mut set, SIGKILL);
        assert_eq!(take(&mut set), Some(SIGKILL));
        assert_eq!(take(&mut set), None);
    }

    /// The records a handler reads out of `si_code` are the ABI's, so a
    /// `sigqueue` payload is distinguishable from a bare `kill` - which is the
    /// whole point of Phase 2.3.
    #[test]
    fn siginfo_codes_are_pinned() {
        assert_eq!(SI_USER, 0);
        assert_eq!(SI_QUEUE, -1);
        assert_eq!(SI_TIMER, -2);
        assert_eq!(SI_KERNEL, 0x80);
        let q = SigInfo::queue(SIGTERM, 7, 0, 0x1234);
        assert_eq!(q.code, SI_QUEUE);
        assert_eq!(q.value, 0x1234);
        assert_eq!(q.signo, SIGTERM as i32);
        assert_eq!(SigInfo::user(SIGTERM, 7, 0).code, SI_USER);
        assert_eq!(SigInfo::user(SIGTERM, 7, 0).value, 0);
        assert_eq!(SigInfo::timer(SIGALRM).code, SI_TIMER);
    }
}
