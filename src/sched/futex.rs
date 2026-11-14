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
    /// The address this place sits on *now*. A requeue moves it without the
    /// waiter's knowledge - that is the call's whole point - so this field can
    /// differ from the word the caller submitted.
    address: usize,
    /// The address the caller submitted. A requeue rewrites `address` and
    /// leaves this alone, so a sleeper that was moved can still be named by the
    /// wait that asked for it: the `FUTEX_WAITV` answer is an index into the
    /// caller's array, and that array still names the original words.
    asked: usize,
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
            asked: address,
            task,
            woken: false,
            bitset,
        });
    });
}

/// Leave the queue, woken or not. Test-only: the wait paths release their
/// places with `unregister_task`, because a requeue may have moved them.
#[cfg(test)]
fn unregister(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| !(waiter.address == address && waiter.task == task));
    });
}

/// Whether this task's place on one address has been marked woken. Test-only:
/// a wait polls [`is_woken_for`], because a requeue can move its place.
#[cfg(test)]
fn is_woken(address: usize, task: TaskId) -> bool {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .any(|waiter| waiter.address == address && waiter.task == task && waiter.woken)
    })
}

/// Whether *any* of this task's places has been marked woken.
///
/// A waiter that a `FUTEX_REQUEUE` moved holds its place under the
/// destination word, so a condition that names the source address would poll
/// a place that no longer exists and sleep forever - measured: the requeued
/// sleeper was never released by a wake on the destination. The wait belongs
/// to the task, not to one address, which is also the rule `FUTEX_WAITV`
/// needs for a word that was requeued out from under it.
fn is_woken_for(task: TaskId) -> bool {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .any(|waiter| waiter.task == task && waiter.woken)
    })
}

/// Give back every place this task holds, whatever addresses they now sit on.
fn unregister_task(task: TaskId) {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| waiter.task != task);
    });
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
        unregister_task(current);
        // The errno constants are already negative: negating one here would
        // hand the caller +11 and turn "look again" into a nonsense success.
        return crate::err::EAGAIN;
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
    let woken = super::block_until_nanos(|| is_woken_for(current), deadline.unwrap_or(u64::MAX));
    unregister_task(current);
    match woken {
        super::WaitEnd::Condition => 0,
        super::WaitEnd::Deadline => crate::err::ETIMEDOUT,
        super::WaitEnd::Interrupted => crate::err::EINTR,
    }
}

/// Waking up to `nr_wake` sleepers on `source` and moving up to `nr_requeue`
/// of the rest to `target`, without waking them - the two halves of
/// `futex_requeue(2)`, and the places it touched (woken first, then moved).
///
/// The table is one list, so "moving" is rewriting the address a place names,
/// and a later `FUTEX_WAKE` on `target` finds it: that is what makes this
/// different from a wake (which the caller could do with `futex_wake(2)`), and
/// the whole reason the condvar's `broadcast` can hand a waiter to another
/// word without a lost-wakeup window.
///
/// Measured on Linux 7.2.11: the first `nr_wake` matching sleepers are woken,
/// the next `nr_requeue` are moved, in wait order, and the return value counts
/// both. The destination word is **never read** - an unmapped destination is
/// accepted and the moved sleepers are unreachable forever, which is Linux's
/// answer too (a requeue to a word nobody will ever wake).
#[cfg(any(test, target_os = "none"))]
fn requeue_places(
    source: usize,
    target: usize,
    nr_wake: usize,
    nr_requeue: usize,
) -> (alloc::vec::Vec<TaskId>, usize) {
    let mut woken = alloc::vec::Vec::new();
    let mut moved = 0usize;
    critical_section::with(|cs| {
        let mut table = WAITERS.borrow(cs).borrow_mut();
        for waiter in table.iter_mut() {
            if waiter.address != source || waiter.woken {
                continue;
            }
            if woken.len() < nr_wake {
                waiter.woken = true;
                woken.push(waiter.task);
            } else if moved < nr_requeue {
                waiter.address = target;
                moved += 1;
            } else {
                break;
            }
        }
    });
    (woken, moved)
}

/// `futex_requeue(2)` under Linux's comparison: the source word must still hold
/// `expected`, or `EAGAIN` - the caller's own signal to look again rather than
/// to have its waiters moved under a word that changed. `EAGAIN` is answered
/// before any place is touched.
#[cfg(target_os = "none")]
pub fn requeue(
    source: usize,
    expected: u32,
    target: usize,
    nr_wake: usize,
    nr_requeue: usize,
) -> isize {
    if word_at(source) != expected {
        return crate::err::EAGAIN;
    }
    let (woken, moved) = requeue_places(source, target, nr_wake, nr_requeue);
    // Waking outside the critical section: `super::wake` schedules, and the
    // places are already marked, so the sleepers' own condition sees it.
    for task in &woken {
        super::wake(*task);
    }
    (woken.len() + moved) as isize
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

/// One word a `FUTEX_WAITV` wait names: where it is, and what it expected.
///
/// The pair is all the table needs - a multi-word wait registers the same
/// `(address, task)` place once per word, because the wake side has no notion
/// of a group. What makes the wait one sleep and one answer is the order
/// [`wait_multiple`] uses the places in, not the places themselves.
pub struct Wait {
    pub address: usize,
    pub expected: u32,
}

#[cfg(any(test, target_os = "none"))]
/// Take a place for `task` on every address the wait names.
fn register_all(waits: &[Wait], task: TaskId) {
    for wait in waits {
        register(wait.address, task, BITSET_ANY);
    }
}

#[cfg(any(test, target_os = "none"))]
/// Give every place back, woken or not.
///
/// Task-wide rather than word-by-word: a requeue can move any of the places
/// between the registration and the answer, and a cleanup that named the
/// original addresses would leave the moved ones behind - a place no wake can
/// ever be observed taking, and a word it would answer on for the rest of the
/// task's life.
fn unregister_all(_waits: &[Wait], task: TaskId) {
    unregister_task(task);
}

#[cfg(any(test, target_os = "none"))]
/// Which of the named words has this task been marked woken on, earliest
/// first. `FUTEX_WAITV` answers with an index into the caller's array, so the
/// order the caller wrote is the order the answer is spelled in.
fn first_woken(waits: &[Wait], task: TaskId) -> Option<usize> {
    critical_section::with(|cs| {
        let table = WAITERS.borrow(cs).borrow();
        waits.iter().position(|wait| {
            table
                .iter()
                .any(|waiter| waiter.task == task && waiter.asked == wait.address && waiter.woken)
        })
    })
}

/// `FUTEX_WAITV`: sleep while every named word still holds what the caller
/// expected, and wake on whichever one a waker names.
///
/// Returns the index of a woken word, `EAGAIN` when one of the words had
/// already moved (which is the caller's "look again", exactly as `FUTEX_WAIT`'s
/// answer is), `ETIMEDOUT` when the deadline passed first, and `EINTR` when a
/// pending signal ended the sleep.
///
/// The order of the two halves is the contract: every place is taken before
/// any word is re-read, so a wake landing between the read and the sleep is
/// not lost; and the value check comes before the deadline, so a wait whose
/// word moved answers `EAGAIN` even if its deadline has already passed (the
/// measured Linux order, not an accident of this implementation).
#[cfg(target_os = "none")]
pub fn wait_multiple(waits: &[Wait], deadline: Option<u64>) -> isize {
    let current = super::current();
    register_all(waits, current);
    for wait in waits {
        if word_at(wait.address) != wait.expected {
            unregister_all(waits, current);
            return crate::err::EAGAIN;
        }
    }
    let ended = super::block_until_nanos(
        || first_woken(waits, current).is_some(),
        deadline.unwrap_or(u64::MAX),
    );
    let fired = first_woken(waits, current);
    unregister_all(waits, current);
    match ended {
        super::WaitEnd::Condition => match fired {
            Some(index) => index as isize,
            // A wake marks the place and then nudges the task; nothing can
            // clear the mark between those two steps on one core, so this arm
            // is unreachable. Answering index 0 rather than an errno keeps a
            // lost mark from becoming an error the caller cannot act on.
            None => 0,
        },
        super::WaitEnd::Deadline => crate::err::ETIMEDOUT,
        super::WaitEnd::Interrupted => crate::err::EINTR,
    }
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

// ---------------------------------------------------------------------------
// Priority-inheritance futexes.
//
// A PI futex is the same thirty-two-bit word with a second owner: the kernel.
// The word carries the holder's task id in its low thirty bits and two bits
// above it, and the operations that use it are the ones a libc reaches for
// once a program asks a mutex for UNLOCK-time ownership tracking:
//
//   FUTEX_LOCK_PI     take the word, or wait to be handed it
//   FUTEX_TRYLOCK_PI  take it if it is free, else say who has it
//   FUTEX_UNLOCK_PI   give it up, handing it to the first waiter
//
// Measured on Linux 7.2.14 (`build/scratch/pifutex/probe.c`, gcc -O1) before
// any of this was written: a free word is taken and reads back the taker's
// id; the taker's own second LOCK_PI and TRYLOCK_PI are both EDEADLK; an
// UNLOCK_PI by anyone else is EPERM; TRYLOCK_PI under contention is EAGAIN
// *and sets FUTEX_WAITERS*; a word whose owner id names no live task is
// ESRCH; a misaligned address is EINVAL; and FUTEX_CLOCK_REALTIME on any of
// the three is ENOSYS, because the PI operations do not take a clock flag.
//
// The owner id is the kernel's own bookkeeping mirrored into the caller's
// word, which is why the table below tracks who owns what: the wake side
// needs the waiting tasks, and a task's exit needs its words.
//
// What this does **not** do is the name the operation carries: raise the
// owner's scheduling priority to the waiter's while it holds the lock. This
// kernel's ready queues are kernel-internal bands, and the user-visible nice
// does not order them, so there is no priority for a lock to inherit yet -
// the honest answer is recorded as a named gap in `docs/notes/` and in the
// roadmap rather than an imitation of the behaviour.

/// The owner task id lives in the low thirty bits of the word.
pub const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
/// Someone is waiting for the word, or was waiting and has not been handed
/// it yet.
pub const FUTEX_WAITERS: u32 = 1 << 31;
/// The previous owner died while holding the word; its guarded state may be
/// inconsistent, and the next taker is told with `EOWNERDEAD`.
pub const FUTEX_OWNER_DIED: u32 = 1 << 30;

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// A task queued for one PI word.
struct PiWaiter {
    address: usize,
    task: TaskId,
    /// Handed the word by an unmount... by an unlock or an exit; its wait is
    /// over and the answer is the one this flag spells.
    granted: bool,
    /// The handover carries `EOWNERDEAD` (the previous holder died).
    died: bool,
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
static PI_WAITERS: Mutex<RefCell<Vec<PiWaiter>>> = Mutex::new(RefCell::new(Vec::new()));

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// The PI words each task holds. The word in the caller's memory says who
/// owns it, but a task's exit cannot find those words by scanning memory: it
/// needs the list, the same way Linux's robust list works.
static PI_HELD: Mutex<RefCell<Vec<(usize, TaskId)>>> = Mutex::new(RefCell::new(Vec::new()));

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// Remember that `task` holds the word at `address`.
fn pi_held(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        let mut held = PI_HELD.borrow(cs).borrow_mut();
        if !held.iter().any(|(a, t)| *a == address && *t == task) {
            held.push((address, task));
        }
    });
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// The word is no longer `task`'s.
fn pi_released(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        PI_HELD
            .borrow(cs)
            .borrow_mut()
            .retain(|(a, t)| !(*a == address && *t == task));
    });
}

/// The owner id a word names.
pub fn pi_owner(word: u32) -> u32 {
    word & FUTEX_TID_MASK
}

/// The word a given state spells: the owner id, plus the bits the caller's
/// own operations set.
pub fn pi_word(tid: u32, more_waiters: bool, died: bool) -> u32 {
    let mut word = tid & FUTEX_TID_MASK;
    if more_waiters {
        word |= FUTEX_WAITERS;
    }
    if died {
        word |= FUTEX_OWNER_DIED;
    }
    word
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// The number a lock word carries for a task: the id its **reader's** pid
/// namespace gives it, which is what `gettid` would answer.
///
/// Linux writes the global tid there. A machine with pid namespaces cannot
/// hand a program a number it has no way to read, so the number is resolved
/// through the caller's namespace exactly as `gettid` resolves it - a lock
/// word a program compares against `gettid()` is then meaningful on both
/// sides of the comparison, and in the one namespace this machine has the two
/// spellings agree.
fn pi_tid_of(owner: TaskId) -> u32 {
    #[cfg(target_os = "none")]
    {
        super::with_table(|t| {
            t.task(t.current)
                .pid_ns
                .number_of(owner.0)
                .unwrap_or(owner.0 as u32)
        })
    }
    #[cfg(not(target_os = "none"))]
    {
        owner.0 as u32
    }
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// Queue `task` for `address`, once.
fn pi_register(address: usize, task: TaskId) {
    critical_section::with(|cs| {
        let mut table = PI_WAITERS.borrow(cs).borrow_mut();
        if table
            .iter()
            .any(|waiter| waiter.address == address && waiter.task == task)
        {
            return;
        }
        table.push(PiWaiter {
            address,
            task,
            granted: false,
            died: false,
        });
    });
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// Take `task` out of every PI queue - a timed-out or interrupted wait leaves
/// no place behind.
fn pi_unregister(task: TaskId) {
    critical_section::with(|cs| {
        PI_WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| waiter.task != task);
    });
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// Hand the word at `address` to its first waiter, saying whether that waiter
/// inherits a dead owner's state. Returns the granted task and whether there
/// are more waiters behind it.
fn pi_grant_next(address: usize, died: bool) -> Option<(TaskId, bool)> {
    critical_section::with(|cs| {
        let mut table = PI_WAITERS.borrow(cs).borrow_mut();
        let index = table
            .iter()
            .position(|waiter| waiter.address == address && !waiter.granted)?;
        let task = table[index].task;
        table[index].granted = true;
        table[index].died = died;
        let more = table
            .iter()
            .any(|waiter| waiter.address == address && !waiter.granted);
        Some((task, more))
    })
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
/// What a granted waiter was told, cleared as it is read.
fn pi_take_grant(task: TaskId) -> Option<bool> {
    critical_section::with(|cs| {
        let mut table = PI_WAITERS.borrow(cs).borrow_mut();
        let index = table
            .iter()
            .position(|waiter| waiter.task == task && waiter.granted)?;
        let died = table[index].died;
        table.remove(index);
        Some(died)
    })
}

/// Wake the task a PI handover names, so its `block_until` looks again.
#[cfg(target_os = "none")]
fn pi_wake(task: TaskId) {
    super::wake(task);
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
#[cfg(target_os = "none")]
fn set_word_at(address: usize, word: u32) -> bool {
    super::write_user(address, &word.to_ne_bytes())
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
#[cfg(not(target_os = "none"))]
fn set_word_at(address: usize, word: u32) -> bool {
    // The host tests call these helpers with a real `u32` of their own; the
    // pointer is the address they passed, so the write is the one the
    // machine's copy would make.
    unsafe { core::ptr::write_unaligned(address as *mut u32, word) };
    true
}

/// `FUTEX_LOCK_PI`: take the word, waiting for it if another task holds it.
///
/// Returns zero when the caller now owns it, `EOWNERDEAD` when it now owns a
/// word whose previous holder died under it, `EDEADLK` when the caller already
/// holds it, and `ETIMEDOUT`/`EINTR` when the wait ended without the word.
#[cfg(target_os = "none")]
pub fn lock_pi(address: usize, deadline: Option<u64>) -> isize {
    let me = super::current();
    loop {
        let word = word_at(address);
        let owner = pi_owner(word);
        if owner == pi_tid_of(me) {
            return crate::err::EDEADLK;
        }
        if owner == 0 {
            // Claiming and seeing the free word have to be one step: two
            // takers that each read "free" and then wrote would both believe
            // they own it. The claim runs with the futex table's own lock
            // held and re-reads the word inside it.
            let claimed = critical_section::with(|_| {
                if pi_owner(word_at(address)) != 0 {
                    return false;
                }
                set_word_at(
                    address,
                    pi_word(pi_tid_of(me), false, word & FUTEX_OWNER_DIED != 0),
                )
            });
            if claimed {
                pi_held(address, me);
                return if word & FUTEX_OWNER_DIED != 0 {
                    crate::err::EOWNERDEAD
                } else {
                    0
                };
            }
            continue;
        }

        // Contended: the waiter is in the table before the word says so, which
        // is the same order the plain wait uses - a handover that lands in
        // between finds the place instead of going to nobody.
        pi_register(address, me);
        set_word_at(address, word | FUTEX_WAITERS);
        let end =
            super::block_until_nanos(|| pi_peek_grant(me).is_some(), deadline.unwrap_or(u64::MAX));
        match end {
            super::WaitEnd::Condition => {
                // The grant is taken now that the wait is over; the flag it
                // carries says which answer this wait owes.
                if let Some(died) = pi_take_grant(me) {
                    pi_held(address, me);
                    return if died { crate::err::EOWNERDEAD } else { 0 };
                }
                continue;
            }
            super::WaitEnd::Deadline => {
                pi_unregister(me);
                return crate::err::ETIMEDOUT;
            }
            super::WaitEnd::Interrupted => {
                pi_unregister(me);
                return crate::err::EINTR;
            }
        }
    }
}

/// `FUTEX_TRYLOCK_PI`: take the word if it is free; under contention answer
/// `EAGAIN` and leave `FUTEX_WAITERS` set, which is what Linux does (measured:
/// the refused trylock is what tells the holder somebody is waiting).
#[cfg(target_os = "none")]
pub fn trylock_pi(address: usize) -> isize {
    let me = super::current();
    let word = word_at(address);
    let owner = pi_owner(word);
    if owner == pi_tid_of(me) {
        return crate::err::EDEADLK;
    }
    if owner != 0 {
        set_word_at(address, word | FUTEX_WAITERS);
        return crate::err::EAGAIN;
    }
    let died = word & FUTEX_OWNER_DIED != 0;
    if !set_word_at(address, pi_word(pi_tid_of(me), false, died)) {
        return crate::err::EFAULT;
    }
    pi_held(address, me);
    if died { crate::err::EOWNERDEAD } else { 0 }
}

/// `FUTEX_UNLOCK_PI`: give the word up. The first waiter is handed ownership
/// and woken; with nobody waiting the word goes back to zero.
#[cfg(target_os = "none")]
pub fn unlock_pi(address: usize) -> isize {
    let me = super::current();
    let word = word_at(address);
    if pi_owner(word) != pi_tid_of(me) {
        return crate::err::EPERM;
    }
    pi_released(address, me);
    match pi_grant_next(address, false) {
        None => {
            set_word_at(address, 0);
            0
        }
        Some((next, more)) => {
            set_word_at(address, pi_word(pi_tid_of(next), more, false));
            pi_wake(next);
            0
        }
    }
}

/// Release every PI word a dying task still holds: hand each to its first
/// waiter with `EOWNERDEAD`, or leave it marked `FUTEX_OWNER_DIED` when nobody
/// is waiting. The words themselves are in the dying task's memory, so the
/// caller writes them through `write` before the address space goes.
///
/// Returns the `(address, word)` pairs the caller must store, and the tasks it
/// must wake afterwards - the caller holds the task table, and waking is a
/// schedule.
#[cfg(target_os = "none")]
pub fn release_pi(
    task: TaskId,
    words: &mut alloc::vec::Vec<(usize, u32)>,
    wake: &mut alloc::vec::Vec<TaskId>,
) {
    // The registry is this kernel's own bookkeeping - the word in the dying
    // task's memory is a copy of it, and reading that copy would need the
    // task's address space, which the caller is about to take down.
    let held: alloc::vec::Vec<usize> = critical_section::with(|cs| {
        PI_HELD
            .borrow(cs)
            .borrow()
            .iter()
            .filter(|(_, holder)| *holder == task)
            .map(|(address, _)| *address)
            .collect()
    });
    for address in held {
        pi_released(address, task);
        match pi_grant_next(address, true) {
            Some((next, more)) => {
                words.push((address, pi_word(pi_tid_of(next), more, true)));
                wake.push(next);
            }
            None => words.push((address, FUTEX_OWNER_DIED)),
        }
    }
}

/// Peek at a grant without consuming it - the parking condition must not have
/// side effects, so the flag is read here and taken once the wait has ended.
#[cfg(target_os = "none")]
fn pi_peek_grant(task: TaskId) -> Option<bool> {
    critical_section::with(|cs| {
        PI_WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .find(|waiter| waiter.task == task && waiter.granted)
            .map(|waiter| waiter.died)
    })
}

#[cfg(test)]
mod tests {
    //! The waiters' map is one module global (`WAITERS` above), and the
    //! addresses below repeat on purpose - two of these claims are about the
    //! *same* two words - so every test here takes the module's test lock.
    //! Without it two claims about `0xa000`/`0xb000` see each other's waiters and
    //! the suite reddens about one run in three (`docs/notes/` §6.87). The rule
    //! is this repo's own: *a test that touches a module global takes that
    //! module's test lock* (`AGENTS.md` §3, concurrency).

    use super::*;

    /// A task id the tests can use without a scheduler.
    fn task(n: usize) -> TaskId {
        TaskId(0x100 + n)
    }

    #[test]
    fn a_wake_reaches_only_the_address_it_names_and_only_as_many_as_asked() {
        let _lock = crate::sched::test_lock::take();
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
        let _lock = crate::sched::test_lock::take();
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
    fn a_pi_word_names_its_owner_and_the_two_bits_above_it() {
        let _lock = crate::sched::test_lock::take();
        let id = task(0x1234).0 as u32;
        assert_eq!(pi_word(id, false, false), id);
        assert_eq!(pi_word(id, true, false), id | FUTEX_WAITERS);
        assert_eq!(pi_word(id, false, true), id | FUTEX_OWNER_DIED);
        assert_eq!(
            pi_word(id, true, true),
            id | FUTEX_WAITERS | FUTEX_OWNER_DIED
        );
        // The two bit positions the word's ABI fixes.
        assert_eq!(FUTEX_WAITERS, 0x8000_0000);
        assert_eq!(FUTEX_OWNER_DIED, 0x4000_0000);
        assert_eq!(pi_owner(0x8000_0000 | 0x4000_0000 | 77), 77);
        assert_eq!(pi_owner(0), 0);
    }

    #[test]
    fn a_pi_handover_grants_the_first_waiter_and_says_who_waits_behind() {
        let _lock = crate::sched::test_lock::take();
        let address = 0x9000usize;
        pi_register(address, task(1));
        pi_register(address, task(2));

        let (first, more) = pi_grant_next(address, false).expect("a waiter to grant");
        assert_eq!(first, task(1), "the queue hands over in wait order");
        assert!(more, "the second waiter is still there");
        assert_eq!(pi_take_grant(task(1)), Some(false));
        assert_eq!(pi_take_grant(task(1)), None, "a grant is taken once");

        let (second, more) = pi_grant_next(address, true).expect("the second waiter");
        assert_eq!(second, task(2));
        assert!(!more, "nobody is behind the second");
        assert_eq!(
            pi_take_grant(task(2)),
            Some(true),
            "it inherits a dead owner"
        );
        assert_eq!(pi_grant_next(address, false), None, "the queue is empty");

        pi_unregister(task(1));
        pi_unregister(task(2));
    }

    #[test]
    fn a_pi_registration_is_one_place_per_task_and_address() {
        let _lock = crate::sched::test_lock::take();
        let address = 0xa100usize;
        pi_register(address, task(3));
        pi_register(address, task(3));
        assert!(pi_grant_next(address, false).is_some());
        assert!(
            !pi_grant_next(address, false).is_some(),
            "one place, one grant"
        );
        pi_unregister(task(3));
    }

    #[test]
    fn a_wake_that_arrives_before_the_sleep_is_not_lost() {
        let _lock = crate::sched::test_lock::take();
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
        let _lock = crate::sched::test_lock::take();
        let address = 0x4000usize;
        assert_eq!(mark_woken(address, 1), 0);
        assert!(!is_woken(address, task(9)));
    }

    #[test]
    fn a_multi_wait_holds_a_place_on_every_word_and_names_the_one_that_fired() {
        let _lock = crate::sched::test_lock::take();
        let (a, b) = (0x8000usize, 0x9000usize);
        let waits = [
            Wait {
                address: a,
                expected: 1,
            },
            Wait {
                address: b,
                expected: 2,
            },
        ];
        let me = task(11);
        register_all(&waits, me);
        assert_eq!(waiting_on(a), 1, "one place on the first word");
        assert_eq!(waiting_on(b), 1, "and one on the second");
        assert_eq!(first_woken(&waits, me), None, "nothing has fired yet");

        assert_eq!(mark_woken(b, 1), 1);
        assert_eq!(
            first_woken(&waits, me),
            Some(1),
            "the answer is the index of the word the wake named"
        );

        unregister_all(&waits, me);
        assert_eq!(waiting_on(a), 0);
        assert_eq!(waiting_on(b), 0);
    }

    #[test]
    fn a_multi_wait_names_the_earliest_word_when_several_fire() {
        let _lock = crate::sched::test_lock::take();
        let (a, b) = (0xa000usize, 0xb000usize);
        let waits = [
            Wait {
                address: a,
                expected: 1,
            },
            Wait {
                address: b,
                expected: 2,
            },
        ];
        let me = task(12);
        register_all(&waits, me);
        // Both words fire before the sleeper looks; the caller's array order
        // decides which index is named, not the order the wakes landed in.
        assert_eq!(mark_woken(b, 1), 1);
        assert_eq!(mark_woken(a, 1), 1);
        assert_eq!(first_woken(&waits, me), Some(0));
        unregister_all(&waits, me);
    }

    #[test]
    fn a_requeue_wakes_the_first_places_and_moves_the_rest_to_its_other_word() {
        let _lock = crate::sched::test_lock::take();
        let (a, b) = (0x7000usize, 0x8000usize);
        register(a, task(1), BITSET_ANY);
        register(a, task(2), BITSET_ANY);
        register(a, task(3), BITSET_ANY);

        let (woken, moved) = requeue_places(a, b, 1, 2);
        assert_eq!(woken.len(), 1, "one sleeper is woken");
        assert_eq!(woken[0], task(1), "the one that waited first");
        assert_eq!(moved, 2, "the other two are moved");
        assert_eq!(waiting_on(b), 2, "a wake on the destination finds them");
        assert_eq!(
            mark_woken(b, 2),
            2,
            "and that wake is the only one they answer"
        );

        unregister(a, task(1));
        unregister(b, task(2));
        unregister(b, task(3));
        assert_eq!(waiting_on(a), 0);
        assert_eq!(waiting_on(b), 0);
    }

    #[test]
    fn a_requeue_with_no_count_on_one_side_leaves_that_half_alone() {
        let _lock = crate::sched::test_lock::take();
        let (a, b) = (0xa000usize, 0xb000usize);
        register(a, task(1), BITSET_ANY);
        register(a, task(2), BITSET_ANY);

        // Requeue nothing, wake both: the destination never hears of them.
        let (woken, moved) = requeue_places(a, b, 2, 0);
        assert_eq!(woken.len(), 2);
        assert_eq!(moved, 0);
        assert_eq!(waiting_on(b), 0);

        unregister(a, task(1));
        unregister(a, task(2));

        // Wake nothing, move both: they are asleep on the other word, and a
        // wake there is what ends them.
        register(a, task(3), BITSET_ANY);
        register(a, task(4), BITSET_ANY);
        let (woken, moved) = requeue_places(a, b, 0, 2);
        assert!(woken.is_empty(), "nobody is woken by a move");
        assert_eq!(moved, 2);
        assert_eq!(waiting_on(a), 0, "the source is empty");
        assert!(!is_woken(b, task(3)), "moved places are not woken");
        assert_eq!(mark_woken(b, usize::MAX), 2);

        unregister(b, task(3));
        unregister(b, task(4));
    }
}
