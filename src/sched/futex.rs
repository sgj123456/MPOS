//! Futexes: the waiting primitive threads are built out of.
//!
//! A futex is a thirty-two-bit word in the caller's own memory plus a rule: a
//! task may sleep on the word's *address* if the word still holds the value it
//! expected, and another task may wake whoever sleeps there. Everything a
//! libc's threads are made of - mutexes, condition variables, join - is that
//! one primitive used carefully, which is why a kernel that has it can run a
//! real thread library, and a kernel that only pretends to have it cannot:
//! `pthread_mutex_lock` would return without ever blocking, and two threads
//! would walk into the same critical section together.
//!
//! The kernel's part is small: a list of (address, task) pairs. What makes it
//! correct is the *order* - a waiter is registered before the value is
//! re-checked, so a wake that lands between the check and the sleep is not
//! lost. That ordering lives in the table below, which is why the table is
//! separate from the sleeping and can be tested without a scheduler.

use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::TaskId;

/// One task asleep on one address.
struct Waiter {
    address: usize,
    task: TaskId,
    /// Set by a wake. The task itself is asleep in `block_until`, so this is
    /// the condition it polls - a wake does not have to find it anywhere else.
    woken: bool,
}

static WAITERS: Mutex<RefCell<Vec<Waiter>>> = Mutex::new(RefCell::new(Vec::new()));

/// Take a place in the queue for `address`, before the value is re-checked.
fn register(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        WAITERS.borrow(cs).borrow_mut().push(Waiter {
            address,
            task,
            woken: false,
        });
    });
}

/// Leave the queue, woken or not.
fn unregister(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| !(waiter.address == address && waiter.task == task));
    });
}

/// Whether this task's place has been marked woken.
fn is_woken(address: usize, task: TaskId) -> bool {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .any(|waiter| waiter.address == address && waiter.task == task && waiter.woken)
    })
}

/// Mark up to `count` waiters on `address` as woken, and say how many. A wake
/// with nobody there is not an error: it is the common case.
fn mark_woken(address: usize, count: usize) -> usize {
    let mut woken = 0;
    critical_section::with(|cs| {
        for waiter in WAITERS.borrow(cs).borrow_mut().iter_mut() {
            if waiter.address == address && !waiter.woken && woken < count {
                waiter.woken = true;
                woken += 1;
            }
        }
    });
    woken
}

/// The tasks waiting on an address, for a wake to find them.
fn waiters_on(address: usize) -> Vec<TaskId> {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .filter(|waiter| waiter.address == address)
            .map(|waiter| waiter.task)
            .collect()
    })
}

/// How many tasks are waiting on an address.
pub fn waiting_on(address: usize) -> usize {
    waiters_on(address).len()
}

/// The word a futex call names, as it is right now: the caller's own memory,
/// read through the direct map. An address the caller does not have mapped is
/// not a word, so it reads as zero - which no waiter ever expected, and which
/// sends the caller back to look again rather than to sleep.
#[cfg(target_os = "none")]
fn word_at(address: usize) -> u32 {
    match super::user_frame_at(address) {
        Some(phys) => unsafe {
            core::ptr::read_volatile(crate::mm::phys_to_virt(phys) as *const u32)
        },
        None => 0,
    }
}

#[cfg(not(target_os = "none"))]
fn word_at(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// `FUTEX_WAIT`: sleep while the word at `address` still holds `expected`.
///
/// Returns zero when woken by a wake, `EAGAIN` when the word had already
/// changed - which is not a failure but the caller's own signal that it should
/// look again rather than sleep.
#[cfg(target_os = "none")]
pub fn wait(address: usize, expected: u32) -> isize {
    let current = super::current();

    // Registered first, checked second: the other order has a window in which
    // a wake goes to nobody and the sleeper never hears it.
    register(address, current);

    if word_at(address) != expected {
        unregister(address, current);
        // The errno constants are already negative: negating one here would
        // hand the caller +11 and turn "look again" into a nonsense success.
        return crate::syscall::err::EAGAIN;
    }

    super::block_until(|| is_woken(address, current));
    unregister(address, current);
    0
}

/// `FUTEX_WAKE`: wake up to `count` tasks sleeping on `address`.
#[cfg(target_os = "none")]
pub fn wake(address: usize, count: usize) -> usize {
    let woken = mark_woken(address, count);
    if woken > 0 {
        // A task inside `block_until` sleeps with a deadline of "never", so it
        // has to be told to look again.
        for task in waiters_on(address) {
            super::wake(task);
        }
    }
    woken
}

/// Wake everyone asleep on `address` - what a thread's exit does for whoever
/// is joining it.
pub fn wake_all(address: usize) -> usize {
    #[cfg(target_os = "none")]
    {
        wake(address, usize::MAX)
    }
    #[cfg(not(target_os = "none"))]
    {
        mark_woken(address, usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task id the tests can use without a scheduler.
    fn task(n: usize) -> TaskId {
        TaskId(0x100 + n)
    }

    #[test]
    fn a_wake_reaches_only_the_address_it_names_and_only_as_many_as_asked() {
        let (a, b) = (0x1000usize, 0x2000usize);
        register(a, task(1));
        register(a, task(2));
        register(b, task(3));

        assert_eq!(waiting_on(a), 2);
        assert_eq!(mark_woken(a, 1), 1, "one waiter, though two are there");
        assert_eq!(mark_woken(a, 1), 1, "the second is asleep, not woken");
        assert_eq!(mark_woken(a, 1), 0, "both are marked now");
        assert_eq!(mark_woken(b, 1), 1, "the other address has its own queue");

        unregister(a, task(1));
        unregister(a, task(2));
        unregister(b, task(3));
        assert_eq!(waiting_on(a), 0);
        assert_eq!(waiting_on(b), 0);
    }

    #[test]
    fn a_wake_that_arrives_before_the_sleep_is_not_lost() {
        // The ordering the whole primitive rests on: the waiter is in the
        // queue before the value is re-checked, so a wake landing in between
        // marks a waiter that has not slept yet - and the sleep that follows
        // finds the mark instead of waiting for ever.
        let address = 0x3000usize;
        let me = task(7);
        register(address, me);
        assert_eq!(mark_woken(address, 1), 1, "the wake arrives early");
        assert!(is_woken(address, me), "and is still there to be seen");
        unregister(address, me);
        assert!(!is_woken(address, me));
    }

    #[test]
    fn a_task_that_never_waited_is_never_woken() {
        let address = 0x4000usize;
        assert_eq!(mark_woken(address, 1), 0);
        assert!(!is_woken(address, task(9)));
    }
}
