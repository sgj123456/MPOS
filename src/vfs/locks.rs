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

/// One held lock: a range `[start, end)` of one file, in one class, held by
/// `owner`. `end` is [`INF`] for an open-ended (whole-file, or a `l_len` of
/// zero) lock.
///
/// `owner` is *whose lock this counts as being*, which is a different question
/// per class because Linux draws the line in two places:
///
/// * a **record** (`fcntl`) lock belongs to the **thread group**, so the same
///   process locking a range twice replaces its own lock rather than fighting
///   itself, and any descriptor it closes drops the lot;
/// * a **whole-file** (`flock`) lock belongs to the **open file description**,
///   so two independent `open()`s of one file in *one* process conflict, while
///   `dup`ing an fd keeps sharing the one lock.
///
/// `pid` is the process that holds it either way - what `F_GETLK` reports as
/// `l_pid`, and what says whether the holder is still alive.
#[derive(Clone, Copy, Debug)]
pub struct Lock {
    pub dev: u64,
    pub ino: u64,
    pub owner: u64,
    pub pid: u32,
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
/// over `[start, end)`? `owner` is the *class-appropriate* identity, so this is
/// where Linux's two lines are drawn: same owner means the same process (a
/// record lock) or the same open file description (a `flock`), and such a
/// request replaces the lock it meets rather than fighting it. Only a lock of
/// the same family conflicts, and read locks never block read locks - an
/// exclusive request fights any overlap, a shared one only an exclusive holder.
pub fn conflicts(
    existing: &Lock,
    owner: u64,
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
    owner: u64,
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
///
/// The argument list is a lock request spelled out - device, inode, owner,
/// class, kind and the byte range - which every caller here already holds as
/// separate values, and which `set` and `set_or_arm` take in the same order.
/// A struct would move the same seven fields to each call site and give the
/// bundle a name nothing else needs.
#[allow(clippy::too_many_arguments)]
pub fn find_conflict(
    locks: &[Lock],
    dev: u64,
    ino: u64,
    owner: u64,
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
/// record the new one unless `kind` is `None` (an unlock). `pid` is recorded
/// with it for `F_GETLK` and for the liveness check, whatever `owner` was
/// keyed on.
#[allow(clippy::too_many_arguments)] // the same lock request, plus its `pid`
pub fn apply_set(
    locks: &mut Vec<Lock>,
    dev: u64,
    ino: u64,
    owner: u64,
    pid: u32,
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
            pid,
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
    locks.retain(|l| crate::sched::snapshot(l.pid as usize).is_some());
}

/// `F_GETLK`: the first conflicting lock in the class, or `None`.
#[cfg(target_os = "none")]
pub fn query(
    dev: u64,
    ino: u64,
    owner: u64,
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
#[allow(clippy::too_many_arguments)] // the same lock request, plus its `pid`
#[cfg(target_os = "none")]
pub fn set(
    dev: u64,
    ino: u64,
    owner: u64,
    pid: u32,
    class: LockClass,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) -> Result<(), u32> {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        prune_dead(&mut table);
        if let Some(k) = kind
            && let Some(i) = find_conflict(&table, dev, ino, owner, class, k, lo, hi)
        {
            return Err(table[i].pid);
        }
        apply_set(&mut table, dev, ino, owner, pid, class, kind, lo, hi);
        if kind.is_none() {
            wake_waiters(dev, ino);
        }
        Ok(())
    })
}

/// Release the locks that go away when one descriptor closes.
///
/// The two classes drop differently, because they are owned differently:
///
/// * every **record** lock the *process* holds on this file goes - POSIX's rule
///   and the one that surprises people, since a lock taken through one fd is
///   lost by closing an unrelated fd to the same file;
/// * the **whole-file** lock goes only if it was this open file description's,
///   so a second `open()` of the same file keeps its `flock` when the first is
///   closed, and `dup`ed descriptors drop it only with the last of them.
#[cfg(target_os = "none")]
pub fn release_on_close(dev: u64, ino: u64, pid: u32, ofd: u64) {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        let before = table.len();
        table.retain(|l| {
            !(l.dev == dev
                && l.ino == ino
                && (match l.class {
                    LockClass::Record => l.pid == pid,
                    LockClass::Whole => l.owner == ofd,
                }))
        });
        if table.len() != before {
            wake_waiters(dev, ino);
        }
    });
}

/// Release every lock a *process* held on `(dev, ino)`, in either class - what
/// process exit does, since its descriptions die with it.
#[cfg(target_os = "none")]
pub fn release_process(dev: u64, ino: u64, pid: u32) {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        let before = table.len();
        table.retain(|l| !(l.dev == dev && l.ino == ino && l.pid == pid));
        if table.len() != before {
            wake_waiters(dev, ino);
        }
    });
}

/// The whole of a dying process's locks, as the scheduler's exit hook sees it.
///
/// Where [`release_process`] is the rule for one inode, this walks the batch the
/// teardown hands over - which is why it lives here rather than in `sched`: the
/// teardown owns *tasks*, and "which of these descriptors names an inode this
/// process still holds a lock on" is a locking question (`PLAN.md` §11.1.3).
/// Registered by the boot sequence, so the order it runs in among the other
/// hooks is the registration order (`sched::exit`).
#[cfg(target_os = "none")]
pub fn release_on_process_exit(files: &[alloc::sync::Arc<crate::vfs::file::File>], owner: u32) {
    for file in files {
        release_process(
            crate::vfs::mount::fs_id_of_inode(&file.inode()),
            file.metadata().ino,
            owner,
        );
    }
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
    owner: u64,
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
#[allow(clippy::too_many_arguments)] // the same lock request, plus pid and waiter
#[cfg(target_os = "none")]
pub fn set_or_arm(
    dev: u64,
    ino: u64,
    owner: u64,
    pid: u32,
    class: LockClass,
    tid: usize,
    kind: Option<LockKind>,
    lo: i64,
    hi: i64,
) -> bool {
    critical_section::with(|cs| {
        let mut table = FILE_LOCKS.borrow(cs).borrow_mut();
        prune_dead(&mut table);
        if let Some(k) = kind
            && find_conflict(&table, dev, ino, owner, class, k, lo, hi).is_some()
        {
            let mut waiters = LOCK_WAITERS.borrow(cs).borrow_mut();
            if !waiters
                .iter()
                .any(|w| w.tid == tid && w.dev == dev && w.ino == ino)
            {
                waiters.push(Waiter { tid, dev, ino });
            }
            return false;
        }
        apply_set(&mut table, dev, ino, owner, pid, class, kind, lo, hi);
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

    fn lock(owner: u64, class: LockClass, kind: LockKind, start: i64, end: i64) -> Lock {
        Lock {
            dev: 1,
            ino: 1,
            owner,
            pid: owner as u32,
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
            set_on(&mut t, 7, 7, LockClass::Whole, LockKind::Write, 0, INF),
            Ok(())
        );
        // Another writer of the same class is refused across the whole file.
        assert_eq!(
            set_on(&mut t, 8, 8, LockClass::Whole, LockKind::Write, 0, INF),
            Err(7)
        );
        // An `fcntl` writer of the other class is not affected.
        assert_eq!(
            set_on(&mut t, 8, 8, LockClass::Record, LockKind::Write, 0, 100),
            Ok(())
        );
    }

    #[test]
    fn set_records_and_refuses_on_conflict() {
        let mut t = Vec::new();
        assert_eq!(
            set_on(&mut t, 7, 7, LockClass::Record, LockKind::Write, 0, 100),
            Ok(())
        );
        assert_eq!(
            set_on(&mut t, 8, 8, LockClass::Record, LockKind::Write, 50, 60),
            Err(7)
        );
        assert_eq!(
            find_conflict(&t, 1, 1, 8, LockClass::Record, LockKind::Write, 50, 60)
                .map(|i| t[i].owner),
            Some(7)
        );
        assert_eq!(
            set_on(&mut t, 8, 8, LockClass::Record, LockKind::Read, 0, 50),
            Err(7)
        );
        let mut both = Vec::new();
        set_on(&mut both, 7, 7, LockClass::Record, LockKind::Read, 0, 50).unwrap();
        assert_eq!(
            set_on(&mut both, 8, 8, LockClass::Record, LockKind::Read, 10, 40),
            Ok(())
        );
    }

    #[test]
    fn unlock_splits_a_wide_lock_into_its_remainders() {
        let mut t = Vec::new();
        set_on(&mut t, 7, 7, LockClass::Record, LockKind::Write, 0, 100).unwrap();
        apply_set(&mut t, 1, 1, 7, 7, LockClass::Record, None, 40, 60);
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
        set_on(&mut t, 7, 7, LockClass::Record, LockKind::Write, 0, 100).unwrap();
        set_on(&mut t, 7, 7, LockClass::Record, LockKind::Write, 80, 100).unwrap();
        assert_eq!(t.len(), 2);
        let mut ranges: Vec<(i64, i64)> = t.iter().map(|l| (l.start, l.end)).collect();
        ranges.sort();
        assert_eq!(ranges, vec![(0, 80), (80, 100)]);
    }

    // A table-local `set` for the pure tests (the real one takes the global).
    fn set_on(
        t: &mut Vec<Lock>,
        owner: u64,
        pid: u32,
        class: LockClass,
        kind: LockKind,
        lo: i64,
        hi: i64,
    ) -> Result<(), u32> {
        if let Some(i) = find_conflict(t, 1, 1, owner, class, kind, lo, hi) {
            return Err(t[i].pid);
        }
        apply_set(t, 1, 1, owner, pid, class, Some(kind), lo, hi);
        Ok(())
    }

    /// `flock` is owned by the open file description and `fcntl` by the
    /// process, so the same two facts must come out differently in the two
    /// classes - which is exactly what the single `owner` field is for.
    #[test]
    fn a_whole_file_lock_divides_processes_by_description_not_by_pid() {
        let mut t = Vec::new();
        // One process (pid 7), two independent `open()`s: descriptions 101
        // and 102. Linux refuses the second `LOCK_EX` with EWOULDBLOCK.
        assert!(set_on(&mut t, 101, 7, LockClass::Whole, LockKind::Write, 0, INF).is_ok());
        assert_eq!(
            set_on(&mut t, 102, 7, LockClass::Whole, LockKind::Write, 0, INF),
            Err(7),
            "a second description of the same process still contends"
        );
        // A `dup` shares the description, so it shares the lock - and a shared
        // request from the same description coexists with its own exclusive one
        // rather than deadlocking the caller against itself.
        assert!(set_on(&mut t, 101, 7, LockClass::Whole, LockKind::Read, 0, INF).is_ok());
        // The record class reads the same pair the other way: one process never
        // conflicts with itself, whichever descriptor asked.
        let mut r = Vec::new();
        assert!(set_on(&mut r, 7, 7, LockClass::Record, LockKind::Write, 0, 10).is_ok());
        assert!(set_on(&mut r, 7, 7, LockClass::Record, LockKind::Write, 0, 10).is_ok(),);
        assert_eq!(
            r.len(),
            1,
            "the second replaced the first rather than joining it"
        );
    }
}
