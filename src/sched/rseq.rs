//! `rseq(2)`: a thread's restartable-sequence registration, and the tick that
//! must not interrupt one.
//!
//! A restartable sequence is user-space code that reads and writes a shared
//! value in a span the kernel promises not to interrupt: either the sequence
//! runs to its commit unbroken, or the kernel rewinds the *return address* to
//! the sequence's abort handler before the task runs user code again. The
//! registration carries the block the kernel writes - the running CPU id, the
//! armed critical-section pointer, and in newer shapes the NUMA node and the
//! mm-wide concurrency id.
//!
//! # What this kernel serves, and what it measured
//!
//! Measured on Linux 7.2.11 (`build/scratch/rseq/`, every probe a fresh
//! `musl-gcc -static` task, because a glibc binary's own startup registers a
//! 33-byte area before `main` runs and every later call in that thread then
//! answers `EINVAL`):
//!
//! * `rseq(area, 32, 0, sig)` answers 0 for a 32-byte-aligned area, and writes
//!   `rseq_cs = 0`, `flags = 0`, `cpu_id_start = cpu_id = <the running cpu>`
//!   (registration itself leaves -1 and the exit path fills the id in),
//!   `node_id = 0`, `mm_cid = 0`. The legacy 32-byte shape leaves those three
//!   trailing words alone for `len == 32` on a v1 registration, and this kernel
//!   writes the post-return values directly, which on a uniprocessor is 0.
//! * `len` 0/8/31 is `EINVAL`; `len > 32` is the v2 shape (a 64-byte-aligned
//!   area and `flags`/`node_id`/`mm_cid`/`slice_ctrl` semantics this kernel does
//!   not carry) and is refused with `EOPNOTSUPP`.
//! * `flags` 0 and 2 (`RSEQ_FLAG_SLICE_EXT_DEFAULT_ON`, a v2 question) are
//!   accepted; 1, 3, `0x80000000` are `EINVAL`. The call's own `sig` is stored
//!   and checked on re-registration, not at registration.
//! * Re-registration: the same area and length then `EBUSY`; a different area or
//!   length `EINVAL`; a different `sig` `EPERM`.
//! * Unregistration is `rseq(area, len, RSEQ_FLAG_UNREGISTER, sig)` with the
//!   *registered* area, length and signature - not a null pointer. The block
//!   then reads `cpu_id_start = 0`, `cpu_id = -1`, `node_id = 0`, `mm_cid = 0`.
//!   A wrong signature is `EPERM`, a wrong area or length `EINVAL`, and
//!   unregistering when nothing is registered `EINVAL`.
//! * A `fork` child inherits the registration (the same area, length and
//!   signature: the child can unregister with the parent's triple); a *thread*
//!   (`CLONE_VM`) starts with none (measured), and `exec` clears it.
//! * Linux aborts an armed section that is preempted (measured: `rseq_cs` is
//!   cleared and the resume address becomes `abort_ip`), aborts it on a signal,
//!   and does *not* abort one interrupted by its own syscall (measured:
//!   `getpid`/`sched_yield`/`nanosleep` inside a section all commit), because
//!   the syscall-exit path clears the event flags instead.
//!
//! # What this kernel does instead of rewinding the address
//!
//! The promise a restartable sequence needs is atomicity, and this kernel keeps
//! it by never interrupting the sequence: the clock tick that finds the running
//! task inside its armed section **does not preempt it** - the tick is consumed
//! and the task keeps the core until the sequence commits or leaves its span.
//! That is a stronger promise than Linux's (a sequence here never aborts), and
//! it is the one this machine can keep honestly: on a uniprocessor there is no
//! other core to migrate away from, so the only thing that can break the span is
//! the scheduler, and the scheduler is exactly what is deferred.
//!
//! Rewinding instead would need the interrupted task's user register file at
//! the trap return, and that is the one thing this kernel's interrupt path does
//! not carry: the syscall gate owns a frame it can rewrite (`Gate::user_context`
//! and friends), while the clock's trap frame is the machine's own and is
//! `iret`'d back as it was saved. Deferring costs nothing but the tick, and it
//! is bounded: a sequence that is *still* inside its span on the following tick
//! has broken the contract a restartable sequence is defined by (a span is a
//! handful of instructions, never a millisecond), so the task is ended with
//! `SIGSEGV` rather than left holding the only core. Both the deferral and the
//! bound are named divergences in `docs/notes/` §6.35; `t-rseq` pins each.
//!
//! A section that the task leaves before the tick (the `rseq_cs` word still
//! armed, the address outside the span) is disarmed the way Linux does it on its
//! way past: the word is cleared to zero.

use alloc::collections::BTreeMap;
use core::cell::RefCell;
use critical_section::Mutex;

#[cfg(target_os = "none")]
use crate::err;
use crate::sched::TaskId;

/// The legacy v1 area: the only registration shape this kernel carries.
#[cfg(any(test, target_os = "none"))]
pub(crate) const AREA_LEN: usize = 32;

/// `RSEQ_FLAG_UNREGISTER`.
#[cfg(any(test, target_os = "none"))]
const FLAG_UNREGISTER: usize = 1 << 0;

/// `RSEQ_FLAG_SLICE_EXT_DEFAULT_ON` - accepted by Linux 7.2 and meaningless
/// without the v2 area it names, so here it is accepted and ignored.
#[cfg(any(test, target_os = "none"))]
const FLAG_SLICE_EXT_DEFAULT_ON: usize = 1 << 1;

/// `RSEQ_CPU_ID_UNINITIALIZED`.
#[cfg(any(test, target_os = "none"))]
const CPU_ID_UNINITIALIZED: i32 = -1;

/// The area's 32 bytes, as offsets: `struct rseq` is four `u32`s, a `u64` and
/// two more `u32`s, so nothing is naturally aligned past the start.
#[cfg(any(test, target_os = "none"))]
const OFF_CPU_ID_START: usize = 0;
#[cfg(any(test, target_os = "none"))]
const OFF_CPU_ID: usize = 4;
#[cfg(any(test, target_os = "none"))]
const OFF_CS: usize = 8;
#[cfg(any(test, target_os = "none"))]
const OFF_FLAGS: usize = 16;
#[cfg(any(test, target_os = "none"))]
const OFF_NODE_ID: usize = 20;
#[cfg(any(test, target_os = "none"))]
const OFF_MM_CID: usize = 24;

/// The armed section's descriptor, `struct rseq_cs`: version, flags, then the
/// three addresses the kernel needs to decide whether a resume point is inside
/// the span.
#[cfg(any(test, target_os = "none"))]
const CS_LEN: usize = 32;
#[cfg(any(test, target_os = "none"))]
const OFF_CS_START_IP: usize = 8;
#[cfg(any(test, target_os = "none"))]
const OFF_CS_POST_COMMIT: usize = 16;

/// One thread's registration: the area, and enough of the call to answer the
/// measured re-registration and unregistration errors.
///
/// Every field is read on the machine build (the tick and the unregister path)
/// and only carried on the host build, where there is no clock tick to ask the
/// question and no user address space to write into - so the fields a host build
/// never reads are not dead code there, only unread.
#[cfg_attr(
    not(target_os = "none"),
    allow(dead_code, reason = "read on the machine build only")
)]
#[derive(Clone, Copy)]
struct Registration {
    area: usize,
    len: usize,
    sig: u32,
    /// Ticks already spent deferring on this registration - the bound that keeps
    /// a broken span from holding the core. Reset whenever a tick finds the task
    /// outside its span or with nothing armed.
    deferrals: u8,
}

struct State {
    by_task: BTreeMap<usize, Registration>,
}

static REGISTRATIONS: Mutex<RefCell<State>> = Mutex::new(RefCell::new(State {
    by_task: BTreeMap::new(),
}));

fn with<R>(f: impl FnOnce(&mut BTreeMap<usize, Registration>) -> R) -> R {
    critical_section::with(|cs| f(&mut REGISTRATIONS.borrow(cs).borrow_mut().by_task))
}

fn registration(task: TaskId) -> Option<Registration> {
    with(|table| table.get(&task.0).copied())
}

/// What the clock tick found when it looked at the running task's registration.
#[cfg(any(test, target_os = "none"))]
pub(crate) enum Tick {
    /// Nothing armed, or the span does not cover the resume point: the tick is
    /// an ordinary preemption.
    Preempt,
    /// The task is inside an armed section: the tick is spent without a switch.
    Defer,
    /// A section this machine cannot keep, or a registration it cannot read: end
    /// the task rather than silently break atomicity (`SIGSEGV`, Linux's own
    /// answer to an unreadable section).
    Kill,
}

/// `rseq(2)`.
///
/// Everything a caller can steer is validated in the measured order: the
/// unregister flag first, then the flags word, then the length, then the area's
/// alignment, then the registration's own state, and only then the writes into
/// the caller's block.
#[cfg(target_os = "none")]
pub fn syscall(area: usize, len: usize, flags: usize, sig: usize) -> isize {
    let task = super::current();
    let sig = sig as u32;
    if flags & FLAG_UNREGISTER != 0 {
        // `RSEQ_FLAG_UNREGISTER` is the whole word, not a bit to add to
        // something: measured, `1 | 2` is `EINVAL`.
        if flags != FLAG_UNREGISTER {
            return err::EINVAL;
        }
        let Some(registered) = registration(task) else {
            return err::EINVAL;
        };
        if registered.area != area || registered.len != len {
            return err::EINVAL;
        }
        if registered.sig != sig {
            return err::EPERM;
        }
        // The block's ids go back to "not on any cpu", the state the next
        // registration fills in. A write that fails is a caller that unmapped its
        // own area: Linux raises `SIGSEGV` here; the call still reports the
        // failure, and a program that keeps its area mapped cannot see it.
        if !reset_ids(area) {
            return err::EFAULT;
        }
        with(|table| table.remove(&task.0));
        return 0;
    }
    if flags & !FLAG_SLICE_EXT_DEFAULT_ON != 0 {
        return err::EINVAL;
    }
    if len < AREA_LEN {
        return err::EINVAL;
    }
    if len > AREA_LEN {
        // The v2 area: `node_id`, `mm_cid`, `slice_ctrl` and the 64-byte
        // alignment. This kernel carries none of those facts, and answering 0
        // for a shape whose fields it would then leave stale is the success a
        // caller cannot recover from.
        return err::EOPNOTSUPP;
    }
    if !area.is_multiple_of(AREA_LEN) {
        return err::EINVAL;
    }
    if let Some(registered) = registration(task) {
        if registered.area != area || registered.len != len {
            return err::EINVAL;
        }
        if registered.sig != sig {
            return err::EPERM;
        }
        return err::EBUSY;
    }
    // A registration starts from the values a fresh thread's block shows: the
    // section disarmed, the ids on this cpu (0 - the only one), the node and the
    // concurrency id zero. Linux leaves the ids at -1 for the instant between
    // this write and the exit path that updates them; nothing a program can
    // observe between its own syscall's entry and return tells the two apart.
    let zero8 = [0u8; 8];
    if !crate::sched::write_user(area + OFF_CS, &zero8) {
        return err::EFAULT;
    }
    let zero4 = [0u8; 4];
    if !crate::sched::write_user(area + OFF_FLAGS, &zero4)
        || !crate::sched::write_user(area + OFF_NODE_ID, &zero4)
        || !crate::sched::write_user(area + OFF_MM_CID, &zero4)
    {
        return err::EFAULT;
    }
    if !write_ids(area, 0) {
        return err::EFAULT;
    }
    with(|table| {
        table.insert(
            task.0,
            Registration {
                area,
                len,
                sig,
                deferrals: 0,
            },
        )
    });
    0
}

/// Put a `u32` word into the caller's block.
#[cfg(target_os = "none")]
fn write_word(area: usize, off: usize, value: u32) -> bool {
    crate::sched::write_user(area + off, &value.to_ne_bytes())
}

/// The four id writes a registration performs, in the measured shape:
/// `cpu_id_start` and `cpu_id` both name the running cpu (0 - the only one this
/// machine has), and the node/concurrency words are 0.
#[cfg(target_os = "none")]
fn write_ids(area: usize, cpu: i32) -> bool {
    write_word(area, OFF_CPU_ID_START, cpu as u32)
        && write_word(area, OFF_CPU_ID, cpu as u32)
        && write_word(area, OFF_NODE_ID, 0)
        && write_word(area, OFF_MM_CID, 0)
}

/// The four writes an *unregistration* performs, which are not the
/// registration's with a different argument: measured, the block reads back
/// `cpu_id_start = 0` and `cpu_id = -1` (`RSEQ_CPU_ID_UNINITIALIZED`) - "not on
/// any cpu" is a different state from "on cpu 0".
#[cfg(target_os = "none")]
fn reset_ids(area: usize) -> bool {
    write_word(area, OFF_CPU_ID_START, 0)
        && write_word(area, OFF_CPU_ID, CPU_ID_UNINITIALIZED as u32)
        && write_word(area, OFF_NODE_ID, 0)
        && write_word(area, OFF_MM_CID, 0)
}

/// The clock tick's question: may this task be switched away from at `ip`?
///
/// Called from interrupt context with the interrupted task's own user address
/// space loaded, which is what makes the two reads below legal here.
#[cfg(target_os = "none")]
pub(crate) fn tick(ip: usize) -> Tick {
    let task = super::current();
    let Some(registered) = registration(task) else {
        return Tick::Preempt;
    };
    let mut bytes = [0u8; 8];
    if !crate::sched::read_user(registered.area + OFF_CS, &mut bytes) {
        // The registration's own block is gone: Linux answers that with
        // `SIGSEGV` the moment it next has to look at it, and so does this.
        return Tick::Kill;
    }
    let cs = u64::from_ne_bytes(bytes);
    if cs == 0 {
        with(|table| {
            if let Some(entry) = table.get_mut(&task.0) {
                entry.deferrals = 0;
            }
        });
        return Tick::Preempt;
    }
    let cs = cs as usize;
    let mut descriptor = [0u8; CS_LEN];
    if cs.is_multiple_of(CS_LEN) && crate::sched::read_user(cs, &mut descriptor) {
        let start = u64::from_ne_bytes(descriptor[OFF_CS_START_IP..][..8].try_into().unwrap());
        let span = u64::from_ne_bytes(descriptor[OFF_CS_POST_COMMIT..][..8].try_into().unwrap());
        let here = ip as u64;
        if here >= start && here - start < span {
            let deferred = with(|table| {
                let Some(entry) = table.get_mut(&task.0) else {
                    return true;
                };
                if entry.deferrals == 0 {
                    entry.deferrals = 1;
                    true
                } else {
                    false
                }
            });
            return if deferred { Tick::Defer } else { Tick::Kill };
        }
    }
    // Outside the span (or a descriptor that is not one): disarm it the way
    // Linux does on the way past it, then let the tick preempt normally.
    if !crate::sched::write_user(registered.area + OFF_CS, &[0u8; 8]) {
        return Tick::Kill;
    }
    with(|table| {
        if let Some(entry) = table.get_mut(&task.0) {
            entry.deferrals = 0;
        }
    });
    Tick::Preempt
}

/// The child of a `fork`/`vfork` inherits the registration; a thread does not
/// (measured: a `fork` child can unregister with the parent's triple, while a
/// fresh `CLONE_VM` thread registers its own area successfully).
pub fn clone_cleanup(parent: TaskId, child: TaskId, shares_group: bool) {
    if shares_group {
        return;
    }
    if let Some(parent) = registration(parent) {
        with(|table| {
            table.insert(
                child.0,
                Registration {
                    deferrals: 0,
                    ..parent
                },
            );
        });
    }
}

/// A thread's registration goes with it. The block it named is the caller's
/// memory and is not touched: another thread may still be reading it.
pub fn exit_cleanup(task: TaskId) {
    with(|table| {
        table.remove(&task.0);
    });
}

/// `exec` throws the address space away, and the registration names an address
/// in it.
pub fn exec_clear(task: TaskId) {
    exit_cleanup(task);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape words are pure functions of the arguments, so the map is
    /// pinned without a user address space: register/unregister need one, the
    /// answers to a malformed call do not.
    #[test]
    fn the_length_rules_are_the_measured_ones() {
        assert_eq!(AREA_LEN, 32);
        assert_eq!(CS_LEN, 32);
        assert_eq!(OFF_CS_START_IP, 8);
        assert_eq!(OFF_CS_POST_COMMIT, 16);
    }
}
