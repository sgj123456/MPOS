//! Advisory file locks - `fcntl` `F_SETLK`/`F_GETLK`/`F_SETLKW` and `flock`.
//!
//! Two families share one table, and the whole point of the `class` field is
//! that they never see each other: an `fcntl` *record* lock covers a byte range
//! and a `flock` *whole-file* lock covers the file, but on Linux a lock in one
//! family never blocks a request in the other. Within a family the rules are the
//! ones a `Vec` of entries can carry and a host test can pin without a
//! filesystem: shared (read) locks coexist, exclusive (write) locks stand alone,
//! range resolution from `l_whence`/`l_start`/`l_len`, the split a re-lock or
//! unlock performs on the holder's own locks, and the "closing a descriptor
//! drops the process's locks on that file" behaviour.
//!
//! The store is one global table under a `critical_section` mutex, every entry
//! keyed by `(dev, ino)` so two files sharing an inode number on different
//! mounts cannot collide, and owned by a thread-group id - the thing that
//! survives `dup` and threads. (Linux keys `flock` by the open file description
//! rather than the process; the per-process model here is a simplification the
//! single-locker workloads do not distinguish, and is noted where it matters.)

use alloc::vec::Vec;
#[cfg(target_os = "none")]
use core::cell::RefCell;
#[cfg(target_os = "none")]
use critical_section::Mutex;

/// `l_type` / `F_*LCK` values, in the `fcntl.h` numbering.
pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

/// `l_whence`, the origin `l_start` measures from.
const WHENCE_SET: i16 = 0;
const WHENCE_CUR: i16 = 1;
const WHENCE_END: i16 = 2;

/// Whether a lock is shared or exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    Read,
    Write,
}

/// Which family a lock belongs to. Requests only ever conflict within a class,
/// so an `flock` never sees an `fcntl` lock and the reverse - the independence
/// Linux documents for the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockClass {
    /// `fcntl` record locking (`F_SETLK` &c.).
    Record,
    /// `flock(2)` whole-file locking.
    Whole,
}

/// One held lock: a range `[start, end)` of one file, owned by one process, in
/// one class. `end` is [`INF`] for an open-ended (whole-file, or a `l_len` of
/// zero) lock.
#[derive(Clone, Copy, Debug)]
pub struct Lock {
    pub dev: u64,
    pub ino: u64,
    pub owner: u32,
    pub class: LockClass,
    pub kind: LockKind,
    pub start: i64,
    pub end: i64,
}

/// The stand-in for "no upper bound". Larger than any real offset, so range
/// arithmetic with `INF` as an end just works.
pub const INF: i64 = i64::MAX;

/// Resolve a caller's `(whence, start, len)` against a file of `size` bytes and
/// its current position `cur` into a half-open `[lo, hi)` range.
///
/// `len` of zero means "everything from `start` onward"; a negative `len` names
/// the range that *ends* at `start`. Returns `None` when the resolved start is
/// negative, which Linux refuses with `EINVAL`.
pub fn resolve_range(whence: i16, start: i64, len: i64, size: i64, cur: i64) -> Option<(i64, i64)> {
    let base = match whence {
        WHENCE_SET => start,
        WHENCE_CUR => cur.checked_add(start)?,
        WHENCE_END => size.checked_add(start)?,
        _ => return None,
    };
    if base < 0 {
        return None;
    }
    let (lo, hi) = if len == 0 {
        (base, INF)
    } else if len > 0 {
        (base, base.saturating_add(len))
    } else {
        (base.saturating_add(len), base)
    };
    if lo < 0 {
        return None;
    }
    Some((lo, hi))
}

/// Do the half-open ranges `[a_start, a_end)` and `[b_start, b_end)` overlap?
fn overlaps(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> bool {
    a_start < b_end && b_start < a_end
}

/// Does `existing` conflict with a request by `owner` in `class` for `kind`
/// over `[start, end)`? Only another process's lock conflicts, only a lock of
/// the same family conflicts, and read locks never block read locks - an
/// exclusive request fights any overlap, a shared one only an exclusive holder.
pub fn conflicts(
    existing: &Lock,
    owner: u32,
    class: LockClass,
    kind: LockKind,
    start: i64,
    end: i64,
) -> bool {
    if existing.owner == owner || existing.class != class {
        return false;
    }
    if !overlaps(existing.start, existing.end, start, end) {
        return false;
    }
    matches!(kind, LockKind::Write) || matches!(existing.kind, LockKind::Write)
}

/// Split-remove `owner`'s locks in `class` on `(dev, ino)` that overlap
/// `[lo, hi)`, keeping the non-overlapping remainder of each as its own lock.
pub fn remove_range(
    locks: &mut Vec<Lock>,
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    lo: i64,
    hi: i64,
) {
    let mut i = 0;
    while i < locks.len() {
        let l = locks[i];
        let same = l.dev == dev && l.ino == ino && l.owner == owner && l.class == class;
        if same && overlaps(l.start, l.end, lo, hi) {
            let mut pieces: Vec<Lock> = Vec::new();
            if l.start < lo {
                pieces.push(Lock { end: lo, ..l });
            }
            if hi < l.end {
                pieces.push(Lock { start: hi, ..l });
            }
            locks.remove(i);
            for (n, p) in pieces.iter().enumerate() {
                locks.insert(i + n, *p);
            }
            i += pieces.len();
        } else {
            i += 1;
        }
    }
}

/// The index of the first lock in `locks` that conflicts with the request - what
/// `F_GETLK` reports and `F_SETLK` refuses on.
pub fn find_conflict(
    locks: &[Lock],
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    kind: LockKind,
    lo: i64,
    hi: i64,
) -> Option<usize> {
    locks
        .iter()
        .position(|l| l.dev == dev && l.ino == ino && conflicts(l, owner, class, kind, lo, hi))
}

/// Apply a set/unlock: drop the owner's overlapping locks in the class, then
/// record the new one unless `kind` is `None` (an unlock).
pub fn apply_set(
    locks: &mut Vec<Lock>,
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) {
    remove_range(locks, dev, ino, owner, class, lo, hi);
    if let Some(k) = kind {
        locks.push(Lock {
            dev,
            ino,
            owner,
            class,
            kind: k,
            start: lo,
            end: hi,
        });
    }
}

/// The one global table. The wrappers reach into the task table (to prune a
/// holder that died holding a lock) and the scheduler (to wake a blocked
/// caller), so they exist only on the kernel - the pure logic above is what the
/// host tests exercise.
#[cfg(target_os = "none")]
static FILE_LOCKS: Mutex<RefCell<Vec<Lock>>> = Mutex::new(RefCell::new(Vec::new()));

/// Drop every lock whose `owner` no longer names a live task.
#[cfg(target_os = "none")]
fn prune_dead(locks: &mut Vec<Lock>) {
    locks.retain(|l| crate::sched::snapshot(l.owner as usize).is_some());
}

/// `F_GETLK`: the first conflicting lock in the class, or `None`.
#[cfg(target_os = "none")]
pub fn query(
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    kind: LockKind,
    lo: i64,
    hi: i64,
) -> Option<Lock> {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        prune_dead(&mut table);
        find_conflict(&table, dev, ino, owner, class, kind, lo, hi).map(|i| table[i])
    })
}

/// `F_SETLK` / `flock` (non-blocking): take or drop a lock in `class`, refusing
/// with the conflicting owner's pid. `None` `kind` is an unlock.
#[cfg(target_os = "none")]
pub fn set(
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) -> Result<(), u32> {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        prune_dead(&mut table);
        if let Some(k) = kind {
            if let Some(i) = find_conflict(&table, dev, ino, owner, class, k, lo, hi) {
                return Err(table[i].owner);
            }
        }
        apply_set(&mut table, dev, ino, owner, class, kind, lo, hi);
        if kind.is_none() {
            wake_waiters(dev, ino);
        }
        Ok(())
    })
}

/// Release the process's locks of *both* classes on `(dev, ino)` - what closing
/// a descriptor does.
#[cfg(target_os = "none")]
pub fn release(dev: u64, ino: u64, owner: u32) {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        let before = table.len();
        table.retain(|l| !(l.dev == dev && l.ino == ino && l.owner == owner));
        if table.len() != before {
            wake_waiters(dev, ino);
        }
    });
}

/// A task parked in a blocking acquire, waiting for one file to free up. Keyed
/// by `(dev, ino)` only - the wake is a nudge to re-run the acquisition.
#[cfg(target_os = "none")]
struct Waiter {
    tid: usize,
    dev: u64,
    ino: u64,
}

#[cfg(target_os = "none")]
static LOCK_WAITERS: Mutex<RefCell<Vec<Waiter>>> = Mutex::new(RefCell::new(Vec::new()));

/// Is the caller's request currently free (non-mutating)? The sleep condition.
#[cfg(target_os = "none")]
pub fn is_free(
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) -> bool {
    critical_section::with(|cs| {
        let table = FILE_LOCKS.borrow(cs).borrow();
        match kind {
            None => true,
            Some(k) => find_conflict(&table, dev, ino, owner, class, k, lo, hi).is_none(),
        }
    })
}

/// Take the lock, or - if another process holds it in this class - register
/// `tid` as a waiter so a later release wakes it. `true` means the caller now
/// holds it (or the unlock was applied); `false` means it must block. The check
/// and the arming share one lock so a release racing the sleep is never missed.
#[cfg(target_os = "none")]
pub fn set_or_arm(
    dev: u64,
    ino: u64,
    owner: u32,
    class: LockClass,
    tid: usize,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) -> bool {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        prune_dead(&mut table);
        if let Some(k) = kind {
            if find_conflict(&table, dev, ino, owner, class, k, lo, hi).is_some() {
                let mut waiters = LOCK_WAITERS.borrow(cs).borrow_mut();
                if !waiters
                    .iter()
                    .any(|w| w.tid == tid && w.dev == dev && w.ino == ino)
                {
                    waiters.push(Waiter { tid, dev, ino });
                }
                return false;
            }
        }
        apply_set(&mut table, dev, ino, owner, class, kind, lo, hi);
        if kind.is_none() {
            wake_waiters(dev, ino);
        }
        true
    })
}

/// Stop waiting for a file (a blocked task that abandoned on a signal).
#[cfg(target_os = "none")]
pub fn disarm(tid: usize) {
    critical_section::with(|cs| {
        LOCK_WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|w| w.tid != tid);
    });
}

/// Wake every task waiting on this file, outside the `FILE_LOCKS` borrow. The
/// wake touches only the scheduler table, and no wait/wake path grabs
/// `FILE_LOCKS`, so the two never invert.
#[cfg(target_os = "none")]
fn wake_waiters(dev: u64, ino: u64) {
    let tids: Vec<usize> = critical_section::with(|cs| {
        let mut waiters = LOCK_WAITERS.borrow(cs).borrow_mut();
        let hit: Vec<usize> = waiters
            .iter()
            .filter(|w| w.dev == dev && w.ino == ino)
            .map(|w| w.tid)
            .collect();
        waiters.retain(|w| !(w.dev == dev && w.ino == ino));
        hit
    });
    for tid in tids {
        crate::sched::wake(crate::sched::TaskId(tid));
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::vec;

    fn lock(owner: u32, class: LockClass, kind: LockKind, start: i64, end: i64) -> Lock {
        Lock {
            dev: 1,
            ino: 1,
            owner,
            class,
            kind,
            start,
            end,
        }
    }

    #[test]
    fn ranges_resolve_by_whence_and_sign() {
        assert_eq!(resolve_range(WHENCE_SET, 10, 20, 100, 0), Some((10, 30)));
        assert_eq!(resolve_range(WHENCE_SET, 10, 0, 100, 0), Some((10, INF)));
        assert_eq!(resolve_range(WHENCE_CUR, 5, 5, 100, 40), Some((45, 50)));
        assert_eq!(resolve_range(WHENCE_END, -10, 4, 100, 0), Some((90, 94)));
        assert_eq!(resolve_range(WHENCE_SET, 30, -10, 100, 0), Some((20, 30)));
        assert_eq!(resolve_range(WHENCE_SET, -1, 5, 100, 0), None);
        assert_eq!(resolve_range(WHENCE_CUR, -50, 5, 100, 20), None);
    }

    #[test]
    fn shared_locks_coexist_and_exclusive_fights_everything() {
        let held = lock(7, LockClass::Record, LockKind::Read, 0, 100);
        assert!(!conflicts(
            &held,
            8,
            LockClass::Record,
            LockKind::Read,
            0,
            50
        ));
        assert!(conflicts(
            &held,
            8,
            LockClass::Record,
            LockKind::Write,
            50,
            60
        ));
        let w = lock(7, LockClass::Record, LockKind::Write, 0, 100);
        assert!(conflicts(&w, 8, LockClass::Record, LockKind::Read, 50, 60));
        assert!(!conflicts(
            &w,
            7,
            LockClass::Record,
            LockKind::Write,
            0,
            100
        ));
        assert!(!conflicts(
            &w,
            8,
            LockClass::Record,
            LockKind::Write,
            100,
            200
        ));
    }

    /// The two families are independent: a `flock` never blocks an `fcntl`
    /// request over the same bytes, nor the reverse.
    #[test]
    fn record_and_whole_locks_never_conflict() {
        let rec = lock(7, LockClass::Record, LockKind::Write, 0, 100);
        // A whole-file exclusive request is unbothered by the record lock...
        assert!(!conflicts(
            &rec,
            8,
            LockClass::Whole,
            LockKind::Write,
            0,
            100
        ));
        // ...and vice versa.
        let whl = lock(8, LockClass::Whole, LockKind::Write, 0, INF);
        assert!(!conflicts(
            &whl,
            7,
            LockClass::Record,
            LockKind::Write,
            50,
            60
        ));
        // But same-class requests still do.
        assert!(conflicts(
            &rec,
            8,
            LockClass::Record,
            LockKind::Write,
            0,
            100
        ));
        assert!(conflicts(&whl, 7, LockClass::Whole, LockKind::Read, 0, 5));
    }

    /// A `flock` whole-file lock covers the file for its exclusive holder and
    /// lets a second shared holder in - the shared/exclusive rule at `INF` end.
    #[test]
    fn whole_file_locks_arbitrate_like_the_rest() {
        let mut t = Vec::new();
        assert_eq!(
            set_on(&mut t, 7, LockClass::Whole, LockKind::Write, 0, INF),
            Ok(())
        );
        // Another writer of the same class is refused across the whole file.
        assert_eq!(
            set_on(&mut t, 8, LockClass::Whole, LockKind::Write, 0, INF),
            Err(7)
        );
        // An `fcntl` writer of the other class is not affected.
        assert_eq!(
            set_on(&mut t, 8, LockClass::Record, LockKind::Write, 0, 100),
            Ok(())
        );
    }

    #[test]
    fn set_records_and_refuses_on_conflict() {
        let mut t = Vec::new();
        assert_eq!(
            set_on(&mut t, 7, LockClass::Record, LockKind::Write, 0, 100),
            Ok(())
        );
        assert_eq!(
            set_on(&mut t, 8, LockClass::Record, LockKind::Write, 50, 60),
            Err(7)
        );
        assert_eq!(
            find_conflict(&t, 1, 1, 8, LockClass::Record, LockKind::Write, 50, 60)
                .map(|i| t[i].owner),
            Some(7)
        );
        assert_eq!(
            set_on(&mut t, 8, LockClass::Record, LockKind::Read, 0, 50),
            Err(7)
        );
        let mut both = Vec::new();
        set_on(&mut both, 7, LockClass::Record, LockKind::Read, 0, 50).unwrap();
        assert_eq!(
            set_on(&mut both, 8, LockClass::Record, LockKind::Read, 10, 40),
            Ok(())
        );
    }

    #[test]
    fn unlock_splits_a_wide_lock_into_its_remainders() {
        let mut t = Vec::new();
        set_on(&mut t, 7, LockClass::Record, LockKind::Write, 0, 100).unwrap();
        apply_set(&mut t, 1, 1, 7, LockClass::Record, None, 40, 60);
        let mine: Vec<(i64, i64)> = t
            .iter()
            .filter(|l| l.owner == 7)
            .map(|l| (l.start, l.end))
            .collect();
        assert_eq!(mine, vec![(0, 40), (60, 100)]);
    }

    #[test]
    fn relock_by_the_holder_replaces_its_own_range() {
        let mut t = Vec::new();
        set_on(&mut t, 7, LockClass::Record, LockKind::Write, 0, 100).unwrap();
        set_on(&mut t, 7, LockClass::Record, LockKind::Write, 80, 100).unwrap();
        assert_eq!(t.len(), 2);
        let mut ranges: Vec<(i64, i64)> = t.iter().map(|l| (l.start, l.end)).collect();
        ranges.sort();
        assert_eq!(ranges, vec![(0, 80), (80, 100)]);
    }

    // A table-local `set` for the pure tests (the real one takes the global).
    fn set_on(
        t: &mut Vec<Lock>,
        owner: u32,
        class: LockClass,
        kind: LockKind,
        lo: i64,
        hi: i64,
    ) -> Result<(), u32> {
        if let Some(i) = find_conflict(t, 1, 1, owner, class, kind, lo, hi) {
            return Err(t[i].owner);
        }
        apply_set(t, 1, 1, owner, class, Some(kind), lo, hi);
        Ok(())
    }
}
