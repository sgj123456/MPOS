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

/// Hangup: the terminal a process was using is going away. The default
/// action is the table's plain terminate - which is what makes a session's
/// `TIOCNOTTY` a way to end the jobs left on a line.
pub const SIGHUP: u8 = 1;
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
/// An instruction the CPU cannot execute: x86_64's `#UD` (`ud2`), and a64's
/// undefined-instruction exception. Measured on Linux 7.2.14
/// (`build/scratch/trapsig/w.c`): a child that executes `ud2` with no handler
/// dies `WIFSIGNALED`/`WTERMSIG=4`, and a `SA_SIGINFO` handler reads
/// `si_code=2` (`ILL_ILLOPN`).
pub const SIGILL: u8 = 4;
/// A bad memory alignment or a bus error: x86_64's `#AC`. Measured on Linux
/// 7.2.14 (`build/scratch/trapsig/ac.c`): setting EFLAGS.AC from ring 3 and
/// loading an odd address dies `WTERMSIG=7` (the same unaligned load with AC
/// clear exits normally), so the vector's signal is neither `SIGSEGV` nor a
/// guess.
pub const SIGBUS: u8 = 7;
/// An arithmetic exception: x86_64's `#DE` (integer divide by zero), and the
/// aarch64 FP traps this kernel does not enable. Measured on Linux 7.2.14
/// (`build/scratch/trapsig/w.c`): a `div` by zero dies `WTERMSIG=8`, and a
/// handler reads `si_code=1` (`FPE_INTDIV`).
pub const SIGFPE: u8 = 8;
/// A bad memory reference. Userspace reaches it through the machine's own fault
/// handlers, which is why this list did not need the number for a long time -
/// but the kernel raises it too, for the one case where *it* decides a task's
/// memory is unusable rather than the mmu: a `rseq` registration whose block
/// this machine can no longer read, or a task still inside its armed span on a
/// second tick (`crate::sched::rseq`'s `Tick::Kill`, Linux's own answer to an
/// unreadable section, `docs/notes/` §6.35).
pub const SIGSEGV: u8 = 11;
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
/// Urgent data on a socket, or an asynchronous I/O event. Nothing happens by
/// default - Linux's `sig_kernel_ignore` names it beside `SIGCHLD` and
/// `SIGWINCH` - and that default is the reason it is here at all: a
/// `clone` child may be born to report it, and the death-notify rule asks
/// whether a `SIG_DFL` disposition would ignore it ([`ignored_by_default`]).
/// Measured on a host (`.workbuddy/probe_exitsig.c`): a `clone(SIGURG)` child's
/// death is dropped at `SIG_DFL` unblocked and delivered while blocked.
pub const SIGURG: u8 = 23;
/// The terminal's window changed size. Nothing happens by default - a program
/// that cares catches it and redraws, and one that does not carries on - which
/// is why it is one of the table's three "do nothing" rows, beside `SIGCHLD`
/// and `SIGURG`.
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

/// Whether `sig` is a real-time signal. Standard signals `1..=31` coalesce
/// (the ABI says a second `kill` before the first is handled changes nothing);
/// real-time signals `32..=64` are queued — Linux delivers every `sigqueue`'d
/// copy, in order, each with its own payload. The queue that makes that true is
/// [`RtQueue`].
///
/// ```
/// use mpos::sched::signal::{is_rt, SIGRTMIN, SIGRTMAX, SIGTERM};
///
/// assert!(!is_rt(SIGTERM)); // 15 — a standard signal, coalesced
/// assert!(is_rt(SIGRTMIN)); // 32 — the first queued one
/// assert!(is_rt(SIGRTMAX)); // 64 — the last bit the pending word can hold
/// assert!(!is_rt(31)); // the boundary: 31 is standard, 32 is real-time
/// ```
pub fn is_rt(sig: u8) -> bool {
    (SIGRTMIN..=SIGRTMAX).contains(&sig)
}

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
    ///
    /// ```
    /// use mpos::sched::signal::{Action, SA_NODEFER, SIGINT, bit};
    ///
    /// let action = Action { handler: 0x1000, ..Action::default() };
    /// let mask = action.handler_mask(SIGINT, bit(3)); // caller had SIGQUIT blocked
    /// assert_eq!(mask & bit(SIGINT), bit(SIGINT)); // the handled signal blocks itself
    /// assert_ne!(mask & bit(3), 0); // and the caller's own block stays
    /// let nodefer = Action { flags: SA_NODEFER, ..action };
    /// assert_eq!(nodefer.handler_mask(SIGINT, 0), 0); // unless SA_NODEFER says otherwise
    /// ```
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

    /// `exec`'s rule: a caught signal returns to `SIG_DFL`, while an ignored
    /// one stays ignored - which is what makes `nohup` and a shell's
    /// `SIG_IGN` across `exec` work. Measured on a current host
    /// (`.workbuddy/probe_sig_inherit.c`, Linux 7.2.11): after `exec`, a
    /// `SIGUSR1` handler read back `SIG_DFL` and a `SIGUSR2` `SIG_IGN` read
    /// back `SIG_IGN`.
    pub fn reset_caught(&mut self) {
        for slot in &mut self.0 {
            if !slot.is_ignore() {
                *slot = Action::default();
            }
        }
    }
}

/// Linux's `sighand_struct`: the handler table a **thread group** shares.
///
/// A "disposition is per process" claim and a "per task" implementation look
/// identical until a thread installs a handler: Linux points every thread of a
/// group at one `sighand_struct` (`CLONE_SIGHAND`, implied by `CLONE_THREAD`),
/// so a handler installed by any thread is the handler the leader reads back.
/// Measured on a current host (`.workbuddy/probe_thread_sighand.c`, Linux
/// 7.2.11): a `pthread` that installs `SIGUSR1` leaves the main thread reading
/// that same address, and a `fork` child inherits the table as a copy - the
/// parent changing one afterwards does not move the child's.
///
/// The handle is one `Arc` because that is the ABI's shape rather than four
/// fields happening to agree: `CLONE_SIGHAND` is the promise that two tasks
/// share *one object*, and a fork is the promise that the child got its own.
/// The lock is a leaf `critical_section::Mutex` for the reason
/// [`crate::sched::FsStruct`]'s is - the table is read from inside a task-table
/// borrow and from outside it, so it cannot borrow the table for storage. The
/// critical section is re-entrant by construction (`arch/*/critical.rs` reads
/// the mask before disabling it), which is what makes a nested read safe.
pub struct SignalHand {
    actions: critical_section::Mutex<core::cell::RefCell<Actions>>,
}

impl SignalHand {
    /// A table of its own holding `actions` - a fresh process, or a fork that
    /// copied its parent's.
    pub fn new(actions: Actions) -> alloc::sync::Arc<SignalHand> {
        alloc::sync::Arc::new(SignalHand {
            actions: critical_section::Mutex::new(core::cell::RefCell::new(actions)),
        })
    }

    /// A copy of this table in a handle of its own: Linux's `copy_sighand`,
    /// which is what a plain `fork` does and what `exec` needs to break the
    /// sharing a thread group had.
    pub fn copy(&self) -> alloc::sync::Arc<SignalHand> {
        SignalHand::new(self.snapshot())
    }

    /// What would happen if `sig` arrived now.
    pub fn get(&self, sig: u8) -> Action {
        critical_section::with(|cs| self.actions.borrow(cs).borrow().get(sig))
    }

    /// Record a disposition. Shared: every task holding this handle sees it.
    pub fn set(&self, sig: u8, action: Action) {
        critical_section::with(|cs| self.actions.borrow(cs).borrow_mut().set(sig, action));
    }

    /// `exec`'s rule, applied to the shared table the caller is leaving.
    pub fn reset_caught(&self) {
        critical_section::with(|cs| self.actions.borrow(cs).borrow_mut().reset_caught());
    }

    /// The table's contents, for a copy that must be one snapshot.
    pub fn snapshot(&self) -> Actions {
        critical_section::with(|cs| *self.actions.borrow(cs).borrow())
    }
}

/// Whether `sig` names a signal at all: Linux's `valid_signal`, the range
/// `1..=_NSIG` (64). Signal 0 and the numbers above `_NSIG` are not signals,
/// and a call naming one is refused whether or not it hands over a
/// disposition.
///
/// ```
/// use mpos::sched::signal::{is_signal, MAX_SIGNAL, SIGKILL};
///
/// assert!(is_signal(SIGKILL as usize)); // 9 is a signal
/// assert!(!is_signal(0)); // the kill-existence probe, never pending
/// assert!(is_signal(MAX_SIGNAL as usize)); // 64 = _NSIG, still in range
/// assert!(!is_signal(65)); // above _NSIG there is no name to give one
/// ```
pub fn is_signal(sig: usize) -> bool {
    (1..SIGNALS).contains(&sig)
}

/// Whether `sig` can be *given* a disposition: every signal but the two the
/// kernel keeps for itself.
///
/// Linux asks this only when one is being set - `act && sig_kernel_only(sig)`
/// in `do_sigaction` - so a bare query of `SIGKILL`/`SIGSTOP` is served and
/// reports `SIG_DFL` (measured on a current host:
/// `rt_sigaction(SIGKILL, NULL, &old, 8)` answers `0` and clears the record),
/// while an install of either is `EINVAL`. A process must not be able to make
/// itself unkillable or unstoppable, but it may ask what would happen.
///
/// ```
/// use mpos::sched::signal::{SIGINT, SIGKILL, SIGSTOP, catchable};
///
/// assert!(catchable(SIGINT as usize));
/// assert!(!catchable(SIGKILL as usize)); // no disposition, queryable all the same
/// assert!(!catchable(SIGSTOP as usize));
/// assert!(!catchable(0)); // not a signal at all
/// ```
pub fn catchable(sig: usize) -> bool {
    is_signal(sig) && sig != SIGKILL as usize && sig != SIGSTOP as usize
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
///
/// ```
/// use mpos::sched::signal::{
///     DefaultAction, SIGCHLD, SIGCONT, SIGINT, SIGSTOP, SIGURG, SIGWINCH, default_action,
/// };
///
/// assert_eq!(default_action(SIGINT), DefaultAction::Terminate);
/// assert_eq!(default_action(SIGSTOP), DefaultAction::Stop);
/// assert_eq!(default_action(SIGCONT), DefaultAction::Continue);
/// // The table's "do nothing" rows: a wait(2) needs no signal to be woken, and
/// // a resized window or a socket's urgent data are nobody's business unless
/// // they catch it.
/// assert_eq!(default_action(SIGCHLD), DefaultAction::Ignore);
/// assert_eq!(default_action(SIGURG), DefaultAction::Ignore);
/// assert_eq!(default_action(SIGWINCH), DefaultAction::Ignore);
/// ```
pub fn default_action(sig: u8) -> DefaultAction {
    match sig {
        // Linux's `sig_kernel_ignore`: the signals whose default action is to
        // do nothing. `SIGCONT`'s default is a *state change* - it resumes a
        // stopped task - and so is a row of its own below. `SIGURG` is here
        // because `ignored_by_default` must answer it, not because this kernel
        // raises it anywhere.
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// Whether `sig`'s default action is to stop the task.
pub fn stops_by_default(sig: u8) -> bool {
    default_action(sig) == DefaultAction::Stop
}

/// The signal a synchronous x86_64 CPU exception ends a process with.
///
/// A CPU exception is not one event with one answer: Linux 7.2.14 measured
/// (`build/scratch/trapsig/w.c`, `ac.c`, `g.c`) ends a child with `SIGFPE` for
/// `#DE` (integer divide by zero), `SIGTRAP` for `#BP` (`int3`), `SIGILL` for
/// `#UD` (`ud2`), `SIGBUS` for `#AC` (EFLAGS.AC set from ring 3, then an
/// unaligned load), and `SIGSEGV` for `#GP` (`hlt`, `mov %cr0`) and `#PF`.
/// A parent's `wait` reads that number back through `WTERMSIG`, and a program
/// that installs a handler for one of them must not be told a different one
/// died - so the vector, not the trap door, decides it.
///
/// The map lives here rather than in a machine file because it is kernel
/// policy over the ABI's signal numbers: `arch/` declares `mpos-abi` as no
/// dependency of its own, and the machine layer reaches the answer through
/// `hal::kernel::signal_for_trap` so it keeps no second copy of the table.
/// Unknown vectors answer `SIGSEGV`: every vector this kernel installs is
/// listed, and a fault path that could fail to name a signal would be worse
/// than one that reports a memory fault for a trap it does not know.
///
/// A synchronous *fault* is not a `#PF`: the page-fault path keeps its own
/// `SIGSEGV` because its resolvers decide whether the address was the
/// process's before any signal is raised (`docs/notes/` §6.159). Vector 14
/// is deliberately absent from the match below: a `#PF` never reaches this
/// door.
pub fn signal_for_trap(vector: u8) -> u8 {
    match vector {
        0 => SIGFPE,  // #DE divide error
        3 => SIGTRAP, // #BP breakpoint (`int3`)
        6 => SIGILL,  // #UD invalid opcode
        17 => SIGBUS, // #AC alignment check
        _ => SIGSEGV, // #SS, #NP, #GP and every vector we do not install
    }
}

/// Whether a signal with **no handler** and **nothing blocking it** would be
/// discarded rather than acted on: Linux's `sig_kernel_ignore`.
///
/// The set is `{SIGCHLD, SIGURG, SIGWINCH}` - the [`DefaultAction::Ignore`]
/// rows of the table above - and one caller turns on every member of it: the
/// signal a dying `clone` child reports to its parent names `SIG_DFL` here to
/// mean "do not bother my parent with this" ([`SigInfo::child`]'s delivery rule
/// lives in `sched::runtime::child_signal::notify_parent_of_death`).
///
/// Measured one exit signal at a time on a host, `SIG_DFL` and unblocked:
/// `SIGURG` and `SIGWINCH` deliver nothing (`.workbuddy/probe_exitsig.c`, G1
/// and J), while `SIGTERM` and `SIGUSR1` kill and **`SIGCONT` resumes a stopped
/// parent** (`.workbuddy/probe_exitsig_cont.c` - the one J cannot see, because a
/// delivered `SIGCONT` has no fatal default and its helper survives either
/// way). So `SIGCONT` is *not* in the set, whatever its "resume" disposition
/// might suggest, and the predicate is not "the action is `Ignore` *or*
/// `Continue`".
///
/// ```
/// use mpos::sched::signal::{SIGCONT, SIGTERM, SIGWINCH, ignored_by_default};
///
/// assert!(ignored_by_default(SIGWINCH)); // a DefaultAction::Ignore row
/// assert!(!ignored_by_default(SIGCONT)); // resumes a stopped task, not a no-op
/// assert!(!ignored_by_default(SIGTERM)); // terminates at SIG_DFL
/// ```
pub fn ignored_by_default(sig: u8) -> bool {
    default_action(sig) == DefaultAction::Ignore
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
/// `SI_MESGQ`: a POSIX message queue's notification, the code a `mq_notify`
/// registration's signal carries. Measured on a current host
/// (`.workbuddy/probe_mqueue.c`): the handler reads `si_code = -3` and the
/// registration's `sigval`.
pub const SI_MESGQ: i32 = -3;
/// `SI_TKILL` is `tgkill`'s own code, and it is the one negative code a
/// caller may not hand to `rt_sigqueueinfo`/`pidfd_send_signal`: Linux's
/// `do_rt_sigqueueinfo` treats it like a positive code (`EPERM`), because it
/// names a sender the kernel itself would have to have produced. Measured on
/// a current host (`.workbuddy/probe_qinfo.c`): `si_code=-6` is `EPERM` while
/// `-1`, `-2`, `-7`, `-8` and `-100` are accepted and preserved.
pub const SI_TKILL: i32 = -6;

/// `si_code` values for a **synchronous CPU exception** - the fault-shaped
/// branch of the union, not a sender's. Each is measured on Linux 7.2.14
/// (`build/scratch/siginfo/faultcodes.c`, one forked child per trial with an
/// `SA_SIGINFO` handler that prints and `_exit(0)`s), beside the vector that
/// produces it:
///
/// * `SEGV_MAPERR` 1 - a null store / a read from an unmapped page, `si_addr`
///   is the address (`0x10` for the null store).
/// * `SEGV_ACCERR` 2 - a write to a `PROT_READ` page or a read of `PROT_NONE`,
///   `si_addr` is the address.
/// * `ILL_ILLOPN` 2 - `ud2` (`#UD`), `si_addr` is the faulting `rip`.
/// * `FPE_INTDIV` 1 - integer divide by zero (`#DE`), `si_addr` is `rip`.
/// * `BUS_ADRALN` 1 - an unaligned load with EFLAGS.AC set (`#AC`); Linux's
///   `si_addr` is nil here and the detail sits in `si_addr_lsb` instead.
/// * `SI_KERNEL` 0x80 - `int3` (`#BP`) and `hlt` (`#GP`): the kernel raised
///   the signal with no fault address, so `si_addr` is nil.
///
/// A trap door used to queue `SI_USER` (code 0) with the sender's pid where
/// `si_addr` belongs, so a handler asking "which address faulted" was told a
/// pid. The named gaps are the machine-specific `_sigfault` members
/// `si_addr_lsb`/`si_addr_bnd`/`si_pkey`: this kernel has no MPX, no pkeys and
/// no per-fault alignment bit, so those slots stay zero (`docs/notes/` §6.160).
pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const ILL_ILLOPN: i32 = 2;
pub const FPE_INTDIV: i32 = 1;
pub const BUS_ADRALN: i32 = 1;

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
    /// `si_status`: the exit code of a child that exited, or the signal that
    /// ended one. Zero for every other sender.
    ///
    /// It shares its word of the union with [`value`](Self::value) - the ABI
    /// puts `si_status` and `si_value` at the same offset - so which one a
    /// record carries is decided by its [`code`](Self::code)'s class rather
    /// than by which field is set. Both are carried here rather than
    /// overlapped, because the two doors that render this record (a handler's
    /// `siginfo_t` and a `signalfd`'s `ssi_status`) read them apart; the class
    /// test lives in one place, `crate::hal::SignalInfo::chld_union`.
    pub status: i32,
    /// The child's CPU time in `USER_HZ` ticks - `si_utime`/`si_stime`, the
    /// other half of the `_sigchld` member, and zero for every other sender
    /// (no other class has these fields). They are stored **already
    /// converted** from this kernel's internal ticks, because the sender
    /// computes them once at the child's death and every renderer writes the
    /// same unit; the conversion is `crate::abi::clock_ticks_to_user_hz`.
    pub utime: u64,
    pub stime: u64,
    /// `si_addr`: the faulting address a synchronous CPU exception names, at
    /// the same wire offset 16 as a sender's `si_pid`/`si_uid`. Zero for a
    /// signal a sender sent. See [`crate::hal::SignalInfo::addr`].
    pub addr: u64,
    /// Whether a CPU fault produced this record rather than a sender. The
    /// union renderer needs this because `SEGV_MAPERR` (1) and `CLD_EXITED`
    /// (1) share a code: `(signo, code)` alone cannot tell the two layouts
    /// apart, but the signal's origin can.
    pub fault: bool,
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
        status: 0,
        utime: 0,
        stime: 0,
        addr: 0,
        fault: false,
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
            status: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            fault: false,
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
            status: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            fault: false,
        }
    }

    /// The record a caller handed in through `rt_sigqueueinfo`/`pidfd_send_signal`.
    ///
    /// Linux copies the caller's `si_errno`, `si_pid`, `si_uid` and `si_value`
    /// verbatim (measured, `.workbuddy/probe_qinfo.c`: 4242/4242 and the value
    /// arrive unchanged), and the **code** is the caller's too - every
    /// negative code except [`SI_TKILL`], not just `SI_QUEUE`. This is why the
    /// two doors cannot reuse [`SigInfo::queue`], which writes the sender's
    /// identity where the caller's record is the contract.
    pub fn signed(sig: u8, errno: i32, code: i32, pid: i32, uid: i32, value: u64) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno,
            code,
            pid,
            uid,
            value,
            status: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            fault: false,
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
            status: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            fault: false,
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
            status: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            fault: false,
        }
    }

    /// The record a **child's death** carries to its parent.
    ///
    /// `sig` is not always `SIGCHLD`: it is the child's own `exit_signal`, the
    /// low byte its `clone` carried, which Linux's `do_notify_parent` sends
    /// verbatim (`SIGTERM` for a `clone(SIGTERM)` child, `SIGCHLD` for a
    /// `fork`'s). The byte `0` never reaches here - a child born to report
    /// nothing delivers nothing - so this is always a real number.
    ///
    /// `code` is the kernel's own record of the death - the exit code, or the
    /// negated signal number - and the `(si_code, si_status)` pair it becomes
    /// is `crate::abi::exit_chld`'s, the same pair `waitid(2)` hands over for
    /// the same corpse: `CLD_EXITED` with the code, or `CLD_KILLED`/`CLD_DUMPED`
    /// with the signal. `dumped` is the death's core-dump classification, which
    /// `exit_chld` needs because it is not a fact about `code` alone (the
    /// signal's default action *and* the child's `dumpable`); the death path
    /// computes it once for both this record and the retired one behind
    /// `waitid`. Measured on a current host through every door that renders it
    /// (`.workbuddy/probe_signalfd_chld.c`, Linux 7.2.9): a child that exits 7
    /// is `CLD_EXITED` with `si_status` 7 in a `signalfd` record, in a
    /// handler's `siginfo_t` and in `sigtimedwait`'s, a child killed by
    /// `SIGKILL` is `CLD_KILLED` with `si_status` 9, and one killed by a
    /// core-dumping signal is `CLD_DUMPED` with the signal
    /// (`.workbuddy/probe_coredump_signals.c`).
    ///
    /// `uid` is the child's **real** id, which is what Linux fills here
    /// (`task_uid`, not the effective one) and what the retired record carries
    /// for `waitid`'s `si_uid` as well, so the two doors name the same owner.
    ///
    /// `utime`/`stime` are the child's CPU in `USER_HZ` ticks, `si_utime` and
    /// `si_stime`. Linux's `do_notify_parent` fills them from
    /// `nsec_to_clock_t(tsk->utime + tsk->signal->utime)` (and the system
    /// pair), so the group's totals are the wrong-side-up thing to read: the
    /// caller hands them in already converted, because the sender computes them
    /// once at the death where the `mm` still holds the counters. Measured on a
    /// current host (`.workbuddy/probe_sigchld_cputime.c`, Linux 7.2.9): a child
    /// that burns ~100 ms arrives as `si_utime` 9, a child that only exits as
    /// `0`/`0`, the same pair through a handler's `siginfo_t` and a `signalfd`
    /// read - and `waitid` does **not** carry them, its record having no such
    /// field (the probe reads zero through every `waitid` door).
    pub fn child(
        sig: u8,
        pid: i32,
        uid: i32,
        code: isize,
        dumped: bool,
        utime: u64,
        stime: u64,
    ) -> SigInfo {
        let (code, status) = crate::abi::exit_chld(code, dumped);
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code,
            pid,
            uid,
            value: 0,
            status,
            utime,
            stime,
            addr: 0,
            fault: false,
        }
    }

    /// The record a **synchronous CPU exception** raises, rather than a sender.
    ///
    /// This is the one record that takes the `_sigfault` branch of the
    /// `siginfo_t` union: `si_addr` lives at wire offset 16, where a `kill`'s
    /// `si_pid`/`si_uid` would sit, and +24..+48 stay zero. `fault: true` is
    /// what tells the renderer that, because `SEGV_MAPERR` and `CLD_EXITED`
    /// are both code 1 - a `(signo, code)` test alone cannot choose. The
    /// measured layout: Linux 7.2.14's null store renders
    /// `signo=11 code=1 addr=0x10` with the first 48 bytes
    /// `0b 00 00 00 00 00 00 00 / 01 00 00 00 00 00 00 00 /
    /// 10 00 00 00 00 00 00 00 / 00 …` (`build/scratch/siginfo/wire.c`).
    pub fn fault(sig: u8, code: i32, addr: usize) -> SigInfo {
        SigInfo {
            signo: sig as i32,
            errno: 0,
            code,
            pid: 0,
            uid: 0,
            value: 0,
            status: 0,
            utime: 0,
            stime: 0,
            addr: addr as u64,
            fault: true,
        }
    }
}

/// The `(signal, record)` a synchronous x86_64 CPU exception must queue.
///
/// The trap doors know the vector, the faulting address (`CR2` for a `#PF`,
/// zero for a trap with no address), the interrupted `rip`, and - for a page
/// fault - whether the page-table error word says *protection* rather than
/// *not present*. Linux's `si_code` and `si_addr` for each are measured, not
/// guessed (`build/scratch/siginfo/faultcodes.c`, Linux 7.2.14):
///
/// | vector | signal | `si_code` | `si_addr` |
/// |---|---|---|---|
/// | 0 `#DE` | `SIGFPE` | `FPE_INTDIV` 1 | `rip` |
/// | 3 `#BP` | `SIGTRAP` | `SI_KERNEL` 0x80 | 0 |
/// | 6 `#UD` | `SIGILL` | `ILL_ILLOPN` 2 | `rip` |
/// | 14 `#PF` | `SIGSEGV` | `SEGV_MAPERR` 1 / `SEGV_ACCERR` 2 | `cr2` |
/// | 17 `#AC` | `SIGBUS` | `BUS_ADRALN` 1 | 0 |
/// | 11/12/13 and unknown | `SIGSEGV` | `SI_KERNEL` 0x80 | 0 |
///
/// The signal half is [`signal_for_trap`] (Item 279, `docs/notes/` §6.159);
/// this is the *payload* half, which a `SI_USER` record could not carry. The
/// map is host-compiled so a host unit test can pin every row without booting
/// a machine, and the machine layer reaches it through
/// `hal::kernel::send_fault_signal` so it keeps no second copy.
///
/// What is still not done, named: handing this record to a handler **at the
/// trap**. §2.13 built `deliver_at_trap` for the PIT's GPR-preserving stub;
/// the six `x86-interrupt` exception entries have no such frame, so a fault's
/// handler is entered only if a timer tick intervenes - measured 2/16 in a
/// guest (`build/scratch/siginfo/rel.c`, `docs/notes/` §6.159). The record's
/// shape is pinned here and by the boot kunit regardless; aarch64's ESR→signal
/// decoding is the other named gap.
pub fn fault_record(vector: u8, addr: usize, rip: usize, protection: bool) -> (u8, SigInfo) {
    let (code, fault_addr) = match vector {
        0 => (FPE_INTDIV, rip), // #DE divide error
        3 => (SI_KERNEL, 0),    // #BP breakpoint (`int3`)
        6 => (ILL_ILLOPN, rip), // #UD invalid opcode
        14 => {
            let code = if protection { SEGV_ACCERR } else { SEGV_MAPERR };
            (code, addr)
        } // #PF page fault
        17 => (BUS_ADRALN, 0),  // #AC alignment check
        _ => (SI_KERNEL, 0),    // #SS, #NP, #GP and unknown
    };
    let sig = signal_for_trap(vector);
    (sig, SigInfo::fault(sig, code, fault_addr))
}

/// How many queued real-time records one task can hold. This ring is where the
/// records live, so it is also the ceiling on what a caller can queue: the
/// resource limit (`RLIMIT_SIGPENDING`) may *lower* the bound but the storage
/// cannot be raised past this without growing memory per task (the worst-case
/// rule, `AGENTS.md` §4). `default_rlimits()` budgets the resource at exactly
/// this number so a caller reading `getrlimit` is told the truth about it, and
/// the host test `rlimits_report_the_startup_defaults_and_refuse_unknown_rows`
/// pins the two together.
pub const RT_QUEUE_LEN: usize = 16;

/// A bounded FIFO of pending real-time signal records.
///
/// Standard signals coalesce — one [`SigInfo`] slot per number, a second
/// `kill` before the first is handled is a no-op. Real-time signals do not:
/// Linux queues *every* copy, whether raised by `sigqueue` (each carrying its
/// own `si_value`) or by `kill`, and delivers them first-in-first-out. This ring
/// is that queue, kept per task. It is deliberately pure and small so the
/// ordering and the "clear the pending bit only when the last copy of a number
/// has drained" rule are unit-testable here rather than only in the machine-only
/// delivery path that embeds it.
#[derive(Clone, Copy, Debug)]
pub struct RtQueue {
    q: [SigInfo; RT_QUEUE_LEN],
    len: usize,
}

impl RtQueue {
    pub const fn new() -> RtQueue {
        RtQueue {
            q: [SigInfo::ZERO; RT_QUEUE_LEN],
            len: 0,
        }
    }

    pub const fn empty(&self) -> bool {
        self.len == 0
    }

    /// How many records are queued. This is the quantity `RLIMIT_SIGPENDING`
    /// bounds, counted across every task of one user — Linux's per-user
    /// `user_struct::sigpending`.
    pub const fn queued(&self) -> usize {
        self.len
    }

    /// Is at least one queued record for `sig` still pending? The caller keeps
    /// the `Pending` bit set exactly while this is true, so a blocked wait is
    /// woken once per copy and the bit clears when the number drains.
    pub fn holds(&self, sig: u8) -> bool {
        self.q[..self.len].iter().any(|e| e.signo == sig as i32)
    }

    /// Append a record. `false` when the ring is full, and the caller turns
    /// that into the answer its door owes: `sigqueue`/`rt_sigqueueinfo` are
    /// **`EAGAIN`** (Linux's `send_sigqueue`), while `kill`/`tgkill` deliver
    /// nothing and still report success — measured: at `RLIMIT_SIGPENDING` a
    /// `kill` of a real-time number returns 0 (`.workbuddy/probe_sigpending.c`)
    /// and the extra copies are dropped. The `false` here is the whole of the
    /// "the queue could not take it" fact; which door reports it is the
    /// syscall layer's business.
    ///
    /// ```
    /// use mpos::sched::signal::{RT_QUEUE_LEN, RtQueue, SigInfo};
    ///
    /// let mut q = RtQueue::new();
    /// assert!(q.empty());
    /// for _ in 0..RT_QUEUE_LEN {
    ///     assert!(q.push(SigInfo::user(34, 1, 0))); // queued, room left
    /// }
    /// assert_eq!(q.queued(), RT_QUEUE_LEN);
    /// assert!(!q.push(SigInfo::user(34, 1, 0))); // the ring is bounded, not grown
    /// ```
    pub fn push(&mut self, info: SigInfo) -> bool {
        if self.len == RT_QUEUE_LEN {
            return false;
        }
        self.q[self.len] = info;
        self.len += 1;
        true
    }

    /// Remove and return the oldest queued record for `sig`, preserving the
    /// order of the rest. `None` when `sig` has nothing queued.
    ///
    /// ```
    /// use mpos::sched::signal::{RtQueue, SigInfo};
    ///
    /// let mut q = RtQueue::new();
    /// // Two copies of 34 (each with its own payload) and one of 35 between them.
    /// q.push(SigInfo::queue(34, 1, 0, 7));
    /// q.push(SigInfo::queue(35, 1, 0, 8));
    /// q.push(SigInfo::queue(34, 1, 0, 9));
    /// // Real-time signals are queued, not coalesced: FIFO per number, and the
    /// // bit stays held while any copy remains — which is what `holds` answers.
    /// assert_eq!(q.pop(34).map(|i| i.value), Some(7));
    /// assert!(q.holds(34));
    /// assert_eq!(q.pop(34).map(|i| i.value), Some(9));
    /// assert!(!q.holds(34));
    /// assert!(q.pop(34).is_none());
    /// assert_eq!(q.pop(35).map(|i| i.value), Some(8)); // its turn was not skipped
    /// ```
    pub fn pop(&mut self, sig: u8) -> Option<SigInfo> {
        let at = self.q[..self.len]
            .iter()
            .position(|e| e.signo == sig as i32)?;
        let info = self.q[at];
        for j in at..self.len - 1 {
            self.q[j] = self.q[j + 1];
        }
        self.len -= 1;
        self.q[self.len] = SigInfo::ZERO;
        Some(info)
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
///
/// ```
/// use mpos::sched::signal::{SIGRTMAX, bit};
///
/// assert_eq!(bit(1), 1); // signal n is bit n-1, not bit n
/// assert_eq!(bit(9), 1 << 8);
/// assert_eq!(bit(SIGRTMAX), 1u64 << 63); // signal 64 fills the word's last bit
/// assert_eq!(bit(0), 0); // the existence probe has no bit
/// ```
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
///
/// ```
/// use mpos::sched::signal::{SIGINT, SIGKILL, add, bit, take};
///
/// let mut set = 0;
/// assert_eq!(take(&mut set), None); // nothing pending
/// add(&mut set, SIGKILL);
/// add(&mut set, SIGINT);
/// assert_eq!(take(&mut set), Some(SIGINT)); // lowest number first
/// assert_eq!(set, bit(SIGKILL)); // and only what was taken leaves
/// assert_eq!(take(&mut set), Some(SIGKILL));
/// assert_eq!(take(&mut set), None);
/// ```
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
        assert_eq!(SIGILL, 4);
        assert_eq!(SIGTRAP, 5);
        assert_eq!(SIGBUS, 7);
        assert_eq!(SIGFPE, 8);
        assert_eq!(SIGKILL, 9);
        assert_eq!(SIGUSR1, 10);
        assert_eq!(SIGUSR2, 12);
        assert_eq!(SIGPIPE, 13);
        assert_eq!(SIGTERM, 15);
        assert_eq!(SIGCHLD, 17);
        assert_eq!(SIGCONT, 18);
        assert_eq!(SIGSTOP, 19);
        assert_eq!(SIGTSTP, 20);
        assert_eq!(SIGURG, 23);
        assert_eq!(SIGWINCH, 28);
        // The `si_code` words a fault's handler reads (`signal(7)`,
        // `build/scratch/siginfo/faultcodes.c`, Linux 7.2.14). `SEGV_MAPERR`
        // and `FPE_INTDIV` sharing 1 is why the union's discriminator is the
        // record's origin, not `(signo, code)`.
        assert_eq!(SEGV_MAPERR, 1);
        assert_eq!(SEGV_ACCERR, 2);
        assert_eq!(ILL_ILLOPN, 2);
        assert_eq!(FPE_INTDIV, 1);
        assert_eq!(BUS_ADRALN, 1);
        assert_eq!(SI_KERNEL, 0x80);
    }

    /// A fault record takes the `_sigfault` union, not a sender's shape. The
    /// counterfactual is `SigInfo::kernel`/`SigInfo::user`: both leave
    /// `fault` false and `addr` zero, so every claim below reddens if the
    /// trap door goes back to `send_signal`.
    #[test]
    fn a_cpu_fault_record_names_its_code_and_address() {
        let (sig, info) = fault_record(14, 0x10, 0xdead, false);
        assert_eq!(sig, SIGSEGV);
        assert_eq!(info.signo, SIGSEGV as i32);
        assert_eq!(info.code, SEGV_MAPERR);
        assert_eq!(info.addr, 0x10);
        assert!(info.fault);
        assert_eq!(info.pid, 0);
        assert_eq!(info.uid, 0);
        assert_eq!(info.value, 0);
        assert_eq!(info.status, 0);
        assert_eq!(SigInfo::fault(SIGTRAP, SI_KERNEL, 0).code, SI_KERNEL);
    }

    /// Every vector's measured `(signal, si_code, si_addr)` row. A map that
    /// answers `SI_USER` (0) for a fault, or puts a pid where `si_addr`
    /// belongs, reddens here on the host without booting a machine.
    #[test]
    fn every_trap_vector_queues_its_own_fault_record() {
        let rip = 0x4012_3456usize;
        let cases: [(u8, u8, i32, usize); 9] = [
            (0, SIGFPE, FPE_INTDIV, rip),     // #DE: si_addr = rip
            (3, SIGTRAP, SI_KERNEL, 0),       // #BP: int3
            (6, SIGILL, ILL_ILLOPN, rip),     // #UD: si_addr = rip
            (11, SIGSEGV, SI_KERNEL, 0),      // #NP
            (12, SIGSEGV, SI_KERNEL, 0),      // #SS
            (13, SIGSEGV, SI_KERNEL, 0),      // #GP: hlt
            (14, SIGSEGV, SEGV_MAPERR, 0x10), // #PF: not present
            (17, SIGBUS, BUS_ADRALN, 0),      // #AC
            (255, SIGSEGV, SI_KERNEL, 0),     // unknown: still total
        ];
        for (vector, sig, code, addr) in cases {
            let (got_sig, info) = fault_record(vector, 0x10, rip, false);
            assert_eq!(got_sig, sig, "vector {vector} answered the wrong signal");
            assert_eq!(
                info.code, code,
                "vector {vector} answered the wrong si_code"
            );
            assert_eq!(
                info.addr as usize, addr,
                "vector {vector} answered the wrong si_addr"
            );
            assert!(
                info.fault,
                "vector {vector} did not mark its record as a fault"
            );
        }
        // A protection violation is the one row the caller's error word decides.
        let (_, accerr) = fault_record(14, 0x7fff_0000, rip, true);
        assert_eq!(accerr.code, SEGV_ACCERR);
        assert_eq!(accerr.addr, 0x7fff_0000);
        assert_ne!(accerr.code, SEGV_MAPERR);
    }

    /// The counterfactual for a trap door that queues `SIGSEGV` for every
    /// vector: each measured Linux verdict is its own signal, and the two
    /// protection vectors beside them stay `SIGSEGV` rather than inheriting a
    /// neighbour's answer.
    #[test]
    fn a_trap_vector_ends_the_process_with_its_own_signal() {
        assert_eq!(signal_for_trap(0), SIGFPE); // #DE: div by zero
        assert_eq!(signal_for_trap(3), SIGTRAP); // #BP: int3
        assert_eq!(signal_for_trap(6), SIGILL); // #UD: ud2
        assert_eq!(signal_for_trap(17), SIGBUS); // #AC: unaligned load
        assert_eq!(signal_for_trap(11), SIGSEGV); // #NP
        assert_eq!(signal_for_trap(12), SIGSEGV); // #SS
        assert_eq!(signal_for_trap(13), SIGSEGV); // #GP: hlt
        assert_eq!(signal_for_trap(2), SIGSEGV); // no IDT entry: still total
        assert_eq!(signal_for_trap(255), SIGSEGV);
        // A trap is not a page fault. The #PF door is a separate path whose
        // resolvers decide the address's meaning before it signals.
        assert_ne!(signal_for_trap(0), signal_for_trap(14));
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
        // And urgent data on a socket is nobody's business either - the third
        // `sig_kernel_ignore` row, which `ignored_by_default` needs.
        assert_eq!(default_action(SIGURG), DefaultAction::Ignore);
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

    /// The set the death-notify rule reads: a `clone` child's exit signal whose
    /// default a parent would ignore is dropped at `SIG_DFL` and delivered once
    /// blocked. Linux's `sig_kernel_ignore` is `SIGCHLD`, `SIGURG` and
    /// `SIGWINCH` - measured one helper per signal on a host
    /// (`.workbuddy/probe_exitsig.c` J) - and `SIGCONT` is deliberately not one
    /// of them: a `clone(SIGCONT)` child's death *resumes* a stopped parent on
    /// Linux (`.workbuddy/probe_exitsig_cont.c`), so it is delivered.
    #[test]
    fn only_the_three_sig_kernel_ignore_signals_are_ignored_by_default() {
        for sig in [SIGCHLD, SIGURG, SIGWINCH] {
            assert!(ignored_by_default(sig), "signal {sig} is not ignored");
        }
        for sig in [SIGCONT, SIGUSR1, SIGHUP, SIGTERM, SIGINT] {
            assert!(!ignored_by_default(sig), "signal {sig} is ignored");
        }
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

    /// `exec` resets a caught disposition and keeps an ignored one - the rule
    /// that makes `nohup` and a shell's `SIG_IGN` survive an `exec`, measured
    /// on a current host (`.workbuddy/probe_sig_inherit.c`).
    #[test]
    fn exec_resets_caught_dispositions_and_keeps_ignored_ones() {
        let mut actions = Actions::default();
        actions.set(
            SIGUSR1,
            Action {
                handler: 0x4000,
                flags: SA_RESTART,
                restorer: 0x5000,
                mask: bit(SIGINT),
            },
        );
        actions.set(
            SIGUSR2,
            Action {
                handler: SIG_IGN,
                ..Action::default()
            },
        );
        actions.reset_caught();
        assert!(actions.get(SIGUSR1).is_default());
        assert!(actions.get(SIGUSR2).is_ignore());
        assert!(actions.get(SIGTERM).is_default());
    }

    /// The two questions `rt_sigaction` asks *separately*: is this a signal at
    /// all, and may it be given a disposition. They differ on exactly the
    /// kernel's two - nameable but not installable - which is what makes a
    /// bare query of either a served call rather than `EINVAL`.
    #[test]
    fn a_signal_is_nameable_and_installable_except_for_the_kernels_two() {
        assert!(is_signal(SIGKILL as usize) && !catchable(SIGKILL as usize));
        assert!(is_signal(SIGSTOP as usize) && !catchable(SIGSTOP as usize));
        // Neither question's subject at all: both answer no.
        assert!(!is_signal(0) && !catchable(0));
        assert!(!is_signal(65) && !catchable(65));
        // The rest of the range is both, the real-time span included.
        for sig in [SIGTERM as usize, 32, 64, SIGTRAP as usize, SIGCONT as usize] {
            assert!(is_signal(sig) && catchable(sig), "signal {sig}");
        }
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

    #[test]
    fn is_rt_spans_only_the_real_time_range() {
        assert!(!is_rt(1));
        assert!(!is_rt(SIGRTMIN - 1));
        assert!(is_rt(SIGRTMIN));
        assert!(is_rt(SIGRTMAX));
        assert!(!is_rt(SIGRTMAX + 1));
    }

    /// The whole reason `RtQueue` exists: three `sigqueue`s of one number all
    /// come back, in the order sent, each with its own `si_value` — where a
    /// coalescing slot would hand back one (the last) record.
    #[test]
    fn rt_queue_delivers_every_copy_in_fifo_order_with_its_own_value() {
        let mut q = RtQueue::new();
        assert!(q.push(SigInfo::queue(SIGRTMIN + 3, 7, 0, 0x101)));
        assert!(q.push(SigInfo::queue(SIGRTMIN + 3, 7, 0, 0x102)));
        assert!(q.push(SigInfo::queue(SIGRTMIN + 3, 7, 0, 0x103)));
        assert!(q.holds(SIGRTMIN + 3));
        assert_eq!(q.pop(SIGRTMIN + 3).map(|r| r.value), Some(0x101));
        assert_eq!(q.pop(SIGRTMIN + 3).map(|r| r.value), Some(0x102));
        assert!(
            q.holds(SIGRTMIN + 3),
            "one copy still queued -> still holds"
        );
        assert_eq!(q.pop(SIGRTMIN + 3).map(|r| r.value), Some(0x103));
        assert!(!q.holds(SIGRTMIN + 3));
        assert_eq!(q.pop(SIGRTMIN + 3), None);
    }

    /// A `kill` (no payload) of an RT number queues alongside a `sigqueue` of
    /// the same number — Linux does not coalesce real-time signals however they
    /// were raised — and popping one number leaves the other's order intact.
    #[test]
    fn rt_queue_keeps_numbers_independent_and_ordered() {
        let mut q = RtQueue::new();
        let a = SIGRTMIN; // 32
        let b = SIGRTMIN + 1; // 33
        assert!(q.push(SigInfo::user(a, 5, 0)));
        assert!(q.push(SigInfo::queue(b, 5, 0, 0xBEEF)));
        assert!(q.push(SigInfo::user(a, 6, 0)));
        assert_eq!(
            q.pop(a).map(|r| (r.signo, r.code, r.pid)),
            Some((a as i32, SI_USER, 5))
        );
        assert_eq!(
            q.pop(a).map(|r| (r.signo, r.code, r.pid)),
            Some((a as i32, SI_USER, 6))
        );
        assert!(!q.holds(a));
        // b untouched by a's pops, still first-in-line for b.
        assert_eq!(q.pop(b).map(|r| r.value), Some(0xBEEF));
    }

    /// The bounded ring: `RT_QUEUE_LEN` pushes land, the next is dropped and
    /// reports it (the caller must not grow memory on a flood).
    #[test]
    fn rt_queue_drops_and_reports_when_full() {
        let mut q = RtQueue::new();
        for i in 0..RT_QUEUE_LEN {
            assert!(q.push(SigInfo::queue(SIGRTMIN, 1, 0, i as u64)), "push {i}");
        }
        assert!(!q.push(SigInfo::queue(SIGRTMIN, 1, 0, 0xffff)));
        assert_eq!(
            q.pop(SIGRTMIN).map(|r| r.value),
            Some(0),
            "FIFO survives overflow"
        );
    }

    #[test]
    fn rt_queue_starts_empty() {
        assert!(RtQueue::new().empty());
        assert!(!RtQueue::new().holds(SIGRTMIN));
    }
}
