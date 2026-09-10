//! The signal numbers and the pending-bit arithmetic.
//!
//! Delivery is the runtime's business; this module is only the vocabulary, kept
//! on its own so host tests can pin the numbers and the bit math without a
//! task table. The set is deliberately small: what the kernel itself raises
//! (SIGPIPE on a broken pipe) and what a process could be told (SIGINT,
//! SIGTERM, SIGKILL). More arrive with the tty's foreground-process concept.
//!
//! Every signal here is fatal by default. There are no handlers yet - kernel
//! tasks have no user frame to return to - so "delivered" and "dies of" are
//! the same thing for now.

/// Interrupt from the keyboard, once there is a keyboard.
pub const SIGINT: u8 = 2;
/// Write on a pipe with no one left to read it.
pub const SIGPIPE: u8 = 13;
/// Termination, the polite kind.
pub const SIGTERM: u8 = 15;
/// Termination, the kind that cannot be caught, blocked or ignored.
pub const SIGKILL: u8 = 9;

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

#[cfg(test)]
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
