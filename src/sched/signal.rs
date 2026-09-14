//! The signal numbers, the pending-bit arithmetic, and each process's
//! dispositions.
//!
//! Delivery is the runtime's business; this module is only the vocabulary, kept
//! on its own so host tests can pin the numbers and the bit math without a
//! task table. The set is deliberately small: what the kernel itself raises
//! (SIGPIPE on a broken pipe) and what a process could be told (SIGINT,
//! SIGTERM, SIGKILL). More arrive with the tty's foreground-process concept.
//!
//! Every signal here is fatal by default. A disposition can be *recorded*
//! (`rt_sigaction`) but not yet *honoured*: no user frame is ever built for a
//! handler, so "delivered" and "dies of" are still the same thing. Recording
//! them is not busywork - it is what lets a query answer `SIG_DFL` with a
//! zeroed struct, and later what lets a handler become real without changing
//! the syscall's shape.

/// Interrupt from the keyboard, once there is a keyboard.
pub const SIGINT: u8 = 2;
/// Write on a pipe with no one left to read it.
pub const SIGPIPE: u8 = 13;
/// The clock a program asked for: `alarm`, `setitimer(ITIMER_REAL)`.
pub const SIGALRM: u8 = 14;
/// Termination, the polite kind.
pub const SIGTERM: u8 = 15;
/// Termination, the kind that cannot be caught, blocked or ignored.
pub const SIGKILL: u8 = 9;
/// Stop, the other kind that cannot be caught.
pub const SIGSTOP: u8 = 19;

/// How many signal numbers a process can hold a disposition for. `Pending` is
/// a `u32`, so 1..=31 are the numbers this kernel can name at all; the zero
/// slot is left in so a signal number indexes its own entry.
pub const SIGNALS: usize = 32;

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
    /// Take the default action: terminate. Every signal this kernel names is
    /// fatal by default, so "default" and "die of it" are the same thing.
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
/// numbers above 31, and the two the kernel reserves for itself.
pub fn catchable(sig: usize) -> bool {
    (1..SIGNALS).contains(&sig) && sig != SIGKILL as usize && sig != SIGSTOP as usize
}

/// The alternate stack `sigaltstack` installs, if any.
///
/// `sp` is the region's base, as the caller gave it - not its top, which is
/// where a stack pointer would start. Nothing switches to it yet: a handler
/// would have to be about to run, and none ever is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltStack {
    pub sp: usize,
    pub size: usize,
}

/// The smallest alternate stack Linux accepts; a smaller one could not hold a
/// signal frame, and Linux answers `ENOMEM` rather than truncating it.
pub const MINSIGSTKSZ: usize = 2048;

/// A pending-signal set: one bit per signal number.
pub type Pending = u32;

pub fn bit(sig: u8) -> Pending {
    1 << (sig % 32)
}

pub fn add(set: &mut Pending, sig: u8) {
    *set |= bit(sig);
}

/// Take the lowest-numbered pending signal out of the set, if any.
pub fn take(set: &mut Pending) -> Option<u8> {
    if *set == 0 {
        return None;
    }
    let sig = (*set).trailing_zeros() as u8;
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
        assert_eq!(SIGKILL, 9);
        assert_eq!(SIGPIPE, 13);
        assert_eq!(SIGTERM, 15);
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
}
