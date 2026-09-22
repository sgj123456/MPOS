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
    /// The wake-side bits this sleeper answers. `FUTEX_BITSET_MATCH_ANY` for
    /// a plain `FUTEX_WAIT`; a bitset waiter names its own, and a
    /// `FUTEX_WAKE_BITSET` reaches only the sleepers whose bits intersect it.
    bitset: u32,
}

static WAITERS: Mutex<RefCell<Vec<Waiter>>> = Mutex::new(RefCell::new(Vec::new()));

/// Take a place in the queue for `address`, before the value is re-checked.
fn register(address: usize, task: TaskId, bitset: u32) {
    critical_section::with(|cs| {
        WAITERS.borrow(cs).borrow_mut().push(Waiter {
            address,
            task,
            woken: false,
            bitset,
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

/// The bitset that answers every wake: what a plain `FUTEX_WAIT` sleeps
/// under, and what `FUTEX_WAKE` is shorthand for.
pub const BITSET_ANY: u32 = u32::MAX;

/// Mark up to `count` waiters on `address` as woken, and say how many. A wake
/// with nobody there is not an error: it is the common case. This is the
/// match-any form - `FUTEX_WAKE` reaches every sleeper on the address,
/// whatever bitset they slept under.
///
/// The host model of a wake (`wake_all`'s test path and the unit tests below)
/// marks with this; the kernel path reaches sleepers through `wake`, so on the
/// `none` target it has no caller.
#[cfg(not(target_os = "none"))]
fn mark_woken(address: usize, count: usize) -> usize {
    mark_woken_bitset(address, count, BITSET_ANY)
}

/// The bitset-filtering half: only sleepers whose own bits share a bit with
/// the waker's are marked, which is `FUTEX_WAKE_BITSET`'s whole contract.
fn mark_woken_bitset(address: usize, count: usize, bitset: u32) -> usize {
    let mut woken = 0;
    critical_section::with(|cs| {
        for waiter in WAITERS.borrow(cs).borrow_mut().iter_mut() {
            if waiter.address == address
                && !waiter.woken
                && woken < count
                && waiter.bitset & bitset != 0
            {
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
    // Unaligned, not volatile: the caller names the address, and nothing in
    // the ABI promises a futex word sits on a four-byte boundary - a `u32`
    // packed behind a byte in some other process's struct is still a word it
    // may wait on. Volatile would add nothing here anyway: the point is not to
    // defeat an optimisation, it is to read what is there right now.
    match super::user_frame_at(address) {
        Some(phys) => unsafe {
            core::ptr::read_unaligned(crate::mm::phys_to_virt(phys) as *const u32)
        },
        None => 0,
    }
}

#[cfg(not(target_os = "none"))]
fn word_at(address: usize) -> u32 {
    unsafe { core::ptr::read_unaligned(address as *const u32) }
}

/// `FUTEX_WAIT`: sleep while the word at `address` still holds `expected`.
///
/// Returns zero when woken by a wake, `EAGAIN` when the word had already
/// changed - which is not a failure but the caller's own signal that it should
/// look again rather than sleep.
#[cfg(target_os = "none")]
pub fn wait(address: usize, expected: u32) -> isize {
    wait_bitset(address, expected, BITSET_ANY, None)
}

/// `FUTEX_WAIT_BITSET`: the timed and bitset-addressed spelling of the same
/// sleep. `deadline` is a tick deadline - `None` waits for a wake and nothing
/// else - and `bitset` selects which wakes may end the sleep.
///
/// Returns zero when woken, `EAGAIN` when the word had already changed, and
/// `ETIMEDOUT` when the deadline passed first: the three answers every
/// condvar, mutex and `thread::park_timeout` on top of it has to tell apart.
#[cfg(target_os = "none")]
pub fn wait_bitset(address: usize, expected: u32, bitset: u32, deadline: Option<u64>) -> isize {
    let current = super::current();

    // Registered first, checked second: the other order has a window in which
    // a wake goes to nobody and the sleeper never hears it.
    register(address, current, bitset);

    if word_at(address) != expected {
        unregister(address, current);
        // The errno constants are already negative: negating one here would
        // hand the caller +11 and turn "look again" into a nonsense success.
        return crate::syscall::err::EAGAIN;
    }

    // The sleep names the deadline, not "never" when there is one: the
    // clock's own tick can end the wait, and a wake ends it sooner. The
    // unregister runs before the answer is spelled, so a sleeper that
    // answers ETIMEDOUT leaves no place in the queue behind.
    //
    // A pending catchable signal ends the wait with EINTR, and the signal is
    // left where it is: the syscall gate delivers it on the way out, where
    // the handler's frame can be built. Sleeping through it would hang a
    // caller whose own handler is the thing meant to end the wait - musl's
    // waitpid blocks on a futex and relies on its SIGCHLD handler to change
    // the word - and musl's __wait retries an EINTR against the
    // handler-updated word, which then answers EAGAIN.
    let woken =
        super::block_until_deadline(|| is_woken(address, current), deadline.unwrap_or(u64::MAX));
    unregister(address, current);
    match woken {
        super::WaitEnd::Condition => 0,
        super::WaitEnd::Deadline => crate::syscall::err::ETIMEDOUT,
        super::WaitEnd::Interrupted => crate::syscall::err::EINTR,
    }
}

/// `FUTEX_WAKE`: wake up to `count` tasks sleeping on `address`.
#[cfg(target_os = "none")]
pub fn wake(address: usize, count: usize) -> usize {
    wake_bitset(address, count, BITSET_ANY)
}

/// `FUTEX_WAKE_BITSET`: wake up to `count` tasks on `address` whose own
/// bitset shares a bit with the waker's. A sleeper under different bits
/// stays asleep, exactly as the man page spells it.
#[cfg(target_os = "none")]
pub fn wake_bitset(address: usize, count: usize, bitset: u32) -> usize {
    let woken = mark_woken_bitset(address, count, bitset);
    if woken > 0 {
        // A task inside `block_until` sleeps with a deadline of "never", so it
        // has to be told to look again. Sleeping waiters under other bits are
        // woken too and simply fall back asleep - the check above decides who
        // counts, this only makes sure the counted ones are looking.
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
        register(a, task(1), BITSET_ANY);
        register(a, task(2), BITSET_ANY);
        register(b, task(3), BITSET_ANY);

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
    fn a_bitset_wake_reaches_only_intersecting_sleepers() {
        let a = 0x5000usize;
        register(a, task(1), 0b0001);
        register(a, task(2), 0b0010);
        register(a, task(3), BITSET_ANY);

        // Waking bit 0 reaches the 0b0001 sleeper and the match-any one, and
        // leaves the 0b0010 sleeper asleep.
        assert_eq!(mark_woken_bitset(a, usize::MAX, 0b0001), 2);
        assert!(is_woken(a, task(1)));
        assert!(!is_woken(a, task(2)));
        assert!(is_woken(a, task(3)));

        // The bits still cover who is left: waking bit 1 now finds the one
        // sleeper it names, and a second pass finds nobody.
        assert_eq!(mark_woken_bitset(a, usize::MAX, 0b0010), 1);
        assert_eq!(mark_woken_bitset(a, usize::MAX, 0b0010), 0);
        assert!(is_woken(a, task(2)));

        // A plain wake is match-any: it counts sleepers under any bits.
        register(a, task(4), 0b0100);
        assert_eq!(mark_woken(a, usize::MAX), 1);

        for t in 1..=4usize {
            unregister(a, task(t));
        }
    }

    #[test]
    fn a_wake_that_arrives_before_the_sleep_is_not_lost() {
        // The ordering the whole primitive rests on: the waiter is in the
        // queue before the value is re-checked, so a wake landing in between
        // marks a waiter that has not slept yet - and the sleep that follows
        // finds the mark instead of waiting for ever.
        let address = 0x3000usize;
        let me = task(7);
        register(address, me, BITSET_ANY);
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
