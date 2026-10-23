//! System V semaphores: the set table, the operations, and the undo list.
//!
//! A semaphore set is an array of counters any process holding its number may
//! adjust, and the adjustment a caller can ask for is the one that *waits*: a
//! negative operation that cannot be satisfied sleeps until another task raises
//! the counter, a zero operation sleeps until it *is* zero, and a set removed
//! under a sleeper wakes it with `EIDRM`. That is why this module lives in
//! `sched` rather than beside the segment table: what a set holds beyond its
//! counters is **waiting**, which is this module's business, and what it costs
//! on a process's death - the `SEM_UNDO` adjustments - is the per-process
//! cleanup `sched` already owns (ARCHITECTURE §3's V-3).
//!
//! The contract is Linux's, measured before any of it was written (Linux
//! 7.2.11, `.workbuddy/probe_sysv_sem.c`, `probe_sem2.c`, `probe_sem3.c`,
//! `probe_sem_unshare.c`; the record is `docs/notes/` §6.14). The facts that
//! shaped this file:
//!
//! * `semtimedop`'s timeout is **relative** (a 120 ms request returned after
//!   120.5 ms) and an expired one answers `EAGAIN`, not `ETIMEDOUT`;
//! * an operation array is **atomic**: if one entry cannot proceed, nothing is
//!   applied (`semop([+1 s0, -1 s1(0)], IPC_NOWAIT)` left `s0` alone);
//! * the values are capped at `SEMVMX` (32767) both ways: `semop(+1)` on 32767
//!   and `SETVAL(32768)` and `SETVAL(-1)` are all `ERANGE`;
//! * `sem_num >= nsems` is `EFBIG` (not `EINVAL`), more than `SEMOPM` (500)
//!   operations is `E2BIG`, and an empty array is `EINVAL`;
//! * `sem_flg` bits the kernel does not know are **ignored** (0x2000 accepted);
//! * the creator (or `CAP_SYS_ADMIN`) may `IPC_SET`/`IPC_RMID` (`EPERM`
//!   otherwise), while `semop` and every `GET*`/`SET*` go through the triads and
//!   answer `EACCES` - a set with mode `0000` refuses even its owner;
//! * a blocked `semop` is woken by another process's raise (50.6 ms later), by a
//!   **handler-bearing** signal (`EINTR`), or by `IPC_RMID` (`EIDRM`);
//! * `SEM_UNDO` records are per process, **not inherited by `fork`** (a child's
//!   exit left the parent's adjustment alone) and applied at exit; a child's own
//!   record is applied by its own exit; `unshare(CLONE_SYSVSEM)` keeps them
//!   (measured: the value went back to 0).
//!
//! **The table is per IPC namespace**, one slot space each, for the reason
//! `sysv_msg`'s header gives: the number a set is known by restarts at 0 in a
//! fresh namespace (measured, `.workbuddy/probe_ipcns.c`), so a slot index only
//! means anything inside its own space. Two consequences ride with it, both
//! handled here rather than hidden: waiters and `SEM_UNDO` records are keyed by
//! the set object (`Set::handle`) and not by its number, because two namespaces
//! can hold sets numbered alike; and an undo record names the object, so a task
//! that leaves its namespace does not have its adjustment land on a stranger
//! (§6.16).
//!
//! **What is not here**: the IPC namespace. As with the segment table, this is
//! one table for the machine until `CLONE_NEWIPC` exists.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use super::TaskId;
use crate::mm::ipc_id;

/// `IPC_PRIVATE`: the key that always makes a fresh set.
pub const IPC_PRIVATE: i32 = 0;

/// The triads of an `IPC_SET`-able mode.
pub const MODE_MASK: u32 = 0o777;

/// How many sets may exist at once - Linux's `SEMMNI` is 32000; this kernel's
/// bound is smaller for the same reason `SHMMNI` is (`docs/notes/` §6.14): a
/// table whose allocation a caller can steer needs one.
pub const SEMMNI: usize = 4_096;

/// The most semaphores one set may hold.
pub const SEMMSL: usize = 1_024;

/// The most semaphores all the sets together may hold, which is what keeps the
/// `i32` array behind a set from growing without bound.
pub const SEMMNS: usize = 65_536;

/// The most operations one `semop` may name (`SEMOPM`).
pub const SEMOPM: usize = 500;

/// The highest value a semaphore may reach, and the largest single adjustment
/// `SEM_UNDO` may carry (`SEMVMX`/`SEMAEM`).
pub const SEMVMX: i32 = 32_767;
pub const SEMAEM: i32 = 32_767;

/// `SEM_UNDO` and `IPC_NOWAIT`, the two bits a `sembuf`'s `sem_flg` carries.
pub const SEM_UNDO: i16 = 0x1000;
pub const IPC_NOWAIT: i16 = 0x4000;

/// One `struct sembuf`: a slot, an operation, and its flags. The ABI's six-byte
/// record, read by the syscall layer and handed here as values.
#[derive(Clone, Copy)]
pub struct SemBuf {
    pub num: usize,
    pub op: i32,
    pub flags: i16,
}

/// Why a set operation was refused. The syscall layer maps these onto errnos.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SemError {
    /// The key names nothing and `IPC_CREAT` was not asked for.
    NoEntry,
    /// The key names something and `IPC_CREAT|IPC_EXCL` was asked for.
    Exists,
    /// The request would make the set bigger than the one the key names.
    TooSmall,
    /// `nsems` is zero or past `SEMMSL`.
    BadCount,
    /// The table, or its semaphore allowance, is full.
    Full,
    /// `sem_num` is past the set's end.
    BadSem,
    /// The value would leave `0..=SEMVMX`.
    Range,
    /// The operation cannot proceed now: the caller blocks, or reads `EAGAIN`.
    WouldBlock,
}

/// A set's identity and permissions: everything `IPC_STAT` reports and
/// `IPC_SET` may change.
#[derive(Clone, Copy)]
pub struct Perm {
    pub key: i32,
    pub id: u64,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u32,
    pub seq: u16,
    pub nsems: usize,
}

/// What a set is beyond its permissions: the counters and the three clocks.
struct Inner {
    perm: Perm,
    /// The machine-wide identity waiters and undo records are keyed by, from
    /// `mm::ipc_id::next_handle` - never the namespace-numbered `perm.id`.
    handle: u64,
    values: Vec<i32>,
    /// `sem_otime`: the last `semop` that changed something.
    otime: i64,
    /// `sem_ctime`: creation, `IPC_SET`, `SETVAL`/`SETALL`.
    ctime: i64,
    /// The last task to `semop` on the set (`GETPID`, `sem_lpid`).
    lpid: u32,
}

/// One semaphore set.
pub struct Set {
    inner: Mutex<RefCell<Inner>>,
    /// `IPC_RMID` has been asked for. The set leaves the table at once - a
    /// number that named it is `EINVAL` from then on - but a waiter already
    /// asleep holds this `Arc` and reads this flag to answer `EIDRM`.
    removed: AtomicBool,
}

impl Set {
    pub fn perm(&self) -> Perm {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().perm)
    }

    pub fn id(&self) -> u64 {
        self.perm().id
    }

    /// The handle waiters and undo records are keyed by: unique across every
    /// namespace, where [`Self::id`] is only unique inside one.
    pub fn handle(&self) -> u64 {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().handle)
    }

    pub fn nsems(&self) -> usize {
        self.perm().nsems
    }

    pub fn removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    /// `sem_otime` and `sem_ctime`, for `IPC_STAT`.
    pub fn times(&self) -> (i64, i64) {
        critical_section::with(|cs| {
            let inner = self.inner.borrow(cs).borrow();
            (inner.otime, inner.ctime)
        })
    }

    /// The task that last operated on the set.
    pub fn lpid(&self) -> u32 {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().lpid)
    }

    /// One counter, or `None` past the set's end.
    pub fn value(&self, sem: usize) -> Option<i32> {
        critical_section::with(|cs| {
            let inner = self.inner.borrow(cs).borrow();
            inner.values.get(sem).copied()
        })
    }

    /// Every counter, for `GETALL`. Bounded by `SEMMSL`, so the copy is bounded
    /// too.
    pub fn values(&self) -> Vec<i32> {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().values.clone())
    }

    /// `SETVAL`: one counter, which may not leave `0..=SEMVMX`.
    pub fn set_value(&self, sem: usize, value: i32, now: i64) -> Result<(), SemError> {
        if !(0..=SEMVMX).contains(&value) {
            return Err(SemError::Range);
        }
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            let Some(slot) = inner.values.get_mut(sem) else {
                return Err(SemError::BadSem);
            };
            *slot = value;
            inner.ctime = now;
            Ok(())
        })
    }

    /// `SETALL`: every counter at once. The array must be exactly `nsems` long;
    /// a value outside the range refuses the whole call (nothing is written).
    pub fn set_all(&self, values: &[i32], now: i64) -> Result<(), SemError> {
        if values.iter().any(|v| !(0..=SEMVMX).contains(v)) {
            return Err(SemError::Range);
        }
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            if values.len() != inner.values.len() {
                return Err(SemError::BadSem);
            }
            inner.values.copy_from_slice(values);
            inner.ctime = now;
            Ok(())
        })
    }

    /// Rewrite the owner, group and triads: `IPC_SET`'s half, which leaves the
    /// creator and every bit above the triads alone.
    pub fn set_perm(&self, uid: u32, gid: u32, mode: u32, now: i64) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            inner.perm.uid = uid;
            inner.perm.gid = gid;
            inner.perm.mode = mode & MODE_MASK;
            inner.ctime = now;
        });
    }

    /// How many tasks are asleep wanting a negative operation on `sem`, and how
    /// many are waiting for it to read zero: `GETNCNT` and `GETZCNT`.
    pub fn waiting(&self, sem: usize) -> (usize, usize) {
        let handle = self.handle();
        critical_section::with(|cs| {
            let waiters = WAITERS.borrow(cs).borrow();
            let mut negative = 0;
            let mut zero = 0;
            for waiter in waiters.iter() {
                if waiter.set != handle || waiter.sem != sem {
                    continue;
                }
                match waiter.kind {
                    WaitKind::Negative => negative += 1,
                    WaitKind::Zero => zero += 1,
                }
            }
            (negative, zero)
        })
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// One namespace's slot space, on the same slot-and-generation numbers the
/// segment table uses (`mm::ipc_id`).
///
/// `sems` is the semaphore allowance, and it is per space because Linux's
/// `SEMMNS` is: a namespace is held to the limit on its own semaphores, not to a
/// machine-wide pool shared with every other namespace.
struct Space {
    ns: Option<u64>,
    sets: Vec<Option<Arc<Set>>>,
    seqs: Vec<u16>,
    /// Semaphores claimed by this space's live sets, against [`SEMMNS`].
    sems: usize,
}

impl Space {
    const fn new(ns: Option<u64>) -> Space {
        Space {
            ns,
            sets: Vec::new(),
            seqs: Vec::new(),
            sems: 0,
        }
    }

    fn slot(&self, index: u64) -> Option<&Arc<Set>> {
        self.sets.get(index as usize)?.as_ref()
    }

    fn find_key(&self, key: i32) -> Option<Arc<Set>> {
        self.sets
            .iter()
            .flatten()
            .find(|set| set.perm().key == key)
            .cloned()
    }

    fn free_slot(&mut self) -> Option<u64> {
        if let Some(index) = self.sets.iter().position(|slot| slot.is_none()) {
            return Some(index as u64);
        }
        if self.sets.len() >= SEMMNI {
            return None;
        }
        self.sets.push(None);
        self.seqs.push(0);
        Some((self.sets.len() - 1) as u64)
    }
}

/// Every namespace's slot space, the machine's own made on first use.
///
/// A namespace that dies leaves its space behind: nothing here is told when a
/// namespace loses its last task, so the sets stay until the machine ends -
/// bounded by `SEMMNI` per space and by how many namespaces were ever made
/// (`docs/notes/` §6.16).
struct Table {
    spaces: Vec<Space>,
}

impl Table {
    const fn empty() -> Table {
        Table { spaces: Vec::new() }
    }

    fn space(&self, ns: Option<u64>) -> Option<&Space> {
        self.spaces.iter().find(|space| space.ns == ns)
    }

    fn space_mut(&mut self, ns: Option<u64>) -> &mut Space {
        if let Some(index) = self.spaces.iter().position(|space| space.ns == ns) {
            return &mut self.spaces[index];
        }
        self.spaces.push(Space::new(ns));
        self.spaces.last_mut().expect("just pushed")
    }
}

static TABLE: Mutex<RefCell<Table>> = Mutex::new(RefCell::new(Table::empty()));

/// Find a live set by the number that names it in namespace `ns`, checking the
/// generation.
pub fn lookup(ns: Option<u64>, id: i64) -> Option<Arc<Set>> {
    let (slot, seq) = ipc_id::decode(id)?;
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        let space = table.space(ns)?;
        if space.seqs.get(slot as usize).copied()? != seq {
            return None;
        }
        space.slot(slot).cloned()
    })
}

/// The set in namespace `ns`'s table slot `index`, for `SEM_STAT`/`SEM_STAT_ANY`.
pub fn by_slot(ns: Option<u64>, index: u64) -> Option<Arc<Set>> {
    critical_section::with(|cs| TABLE.borrow(cs).borrow().space(ns)?.slot(index).cloned())
}

/// Every live set of namespace `ns`, in slot order: what `/proc/sysvipc/sem`
/// walks.
pub fn all(ns: Option<u64>) -> Vec<Arc<Set>> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow()
            .space(ns)
            .map(|space| space.sets.iter().flatten().cloned().collect())
            .unwrap_or_default()
    })
}

/// The highest slot a live set of `ns` occupies - the `IPC_INFO`/`SEM_INFO`
/// return value `ipcs` walks.
pub fn highest_slot(ns: Option<u64>) -> u64 {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        match table
            .space(ns)
            .and_then(|space| space.sets.iter().rposition(|slot| slot.is_some()))
        {
            Some(index) => index as u64,
            None => 0,
        }
    })
}

/// How many sets are live in `ns` and how many semaphores they hold, for
/// `SEM_INFO`.
pub fn totals(ns: Option<u64>) -> (usize, usize) {
    critical_section::with(|cs| {
        let table = TABLE.borrow(cs).borrow();
        let Some(space) = table.space(ns) else {
            return (0, 0);
        };
        (space.sets.iter().flatten().count(), space.sems)
    })
}

/// The facts a new set is made from.
pub struct New {
    pub key: i32,
    pub nsems: usize,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub now: i64,
}

/// Make a set in `ns`, or answer the live one `key` already names **there**.
///
/// The size question is one-sided, measured: a request for **more** semaphores
/// than the set holds is `EINVAL`, while one for fewer is answered with the set
/// (`semget(key, 1, 0)` on a two-slot set returned its id). `IPC_CREAT|IPC_EXCL`
/// is `EEXIST` before either. A key is namespace-scoped, so the same key inside a
/// fresh namespace makes a new set numbered from 0 (measured, §6.16).
pub fn get_or_create(
    ns: Option<u64>,
    new: New,
    create: bool,
    exclusive: bool,
) -> Result<(Arc<Set>, bool), SemError> {
    if new.key != IPC_PRIVATE {
        let existing = critical_section::with(|cs| {
            TABLE
                .borrow(cs)
                .borrow()
                .space(ns)
                .and_then(|space| space.find_key(new.key))
        });
        if let Some(existing) = existing {
            if create && exclusive {
                return Err(SemError::Exists);
            }
            if new.nsems > existing.nsems() {
                return Err(SemError::BadCount);
            }
            return Ok((existing, false));
        }
        if !create {
            return Err(SemError::NoEntry);
        }
    }
    create_set(ns, new).map(|set| (set, true))
}

fn create_set(ns: Option<u64>, new: New) -> Result<Arc<Set>, SemError> {
    if new.nsems == 0 || new.nsems > SEMMSL {
        return Err(SemError::BadCount);
    }
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let space = table.space_mut(ns);
        if space.sems + new.nsems > SEMMNS {
            return Err(SemError::Full);
        }
        let slot = space.free_slot().ok_or(SemError::Full)?;
        let seq = space.seqs[slot as usize];
        let id = ipc_id::build(slot, seq);
        let set = Arc::new(Set {
            inner: Mutex::new(RefCell::new(Inner {
                perm: Perm {
                    key: new.key,
                    id,
                    uid: new.uid,
                    gid: new.gid,
                    cuid: new.uid,
                    cgid: new.gid,
                    mode: new.mode & MODE_MASK,
                    seq,
                    nsems: new.nsems,
                },
                handle: ipc_id::next_handle(),
                values: alloc::vec![0; new.nsems],
                otime: 0,
                ctime: new.now,
                lpid: 0,
            })),
            removed: AtomicBool::new(false),
        });
        space.sets[slot as usize] = Some(Arc::clone(&set));
        Ok(set)
    })
}

/// Take a set out of `ns`'s table and wake everyone asleep on it with `EIDRM`.
///
/// `IPC_RMID` on a semaphore set is a *removal*, not the mark the segment table
/// keeps: there is nothing to keep the object alive by (a set has no
/// attachments), and Linux's own answer to the next call naming the number is
/// `EINVAL`. A waiter already asleep is the one case that still needs the
/// object, and it holds the `Arc` to read [`Set::removed`].
pub fn remove(ns: Option<u64>, id: i64, wake: impl FnMut(TaskId)) -> Option<Arc<Set>> {
    let set = critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let (slot, seq) = ipc_id::decode(id)?;
        let space = table.space_mut(ns);
        if space.seqs.get(slot as usize).copied()? != seq {
            return None;
        }
        let taken = space.sets.get_mut(slot as usize)?.take()?;
        space.sems = space.sems.saturating_sub(taken.nsems());
        if let Some(next) = space.seqs.get_mut(slot as usize) {
            *next = ipc_id::next_generation(*next);
        }
        Some(taken)
    })?;
    set.removed.store(true, Ordering::Relaxed);
    wake_waiters(set.handle(), wake);
    Some(set)
}

/// Drop the slot space a **dead** IPC namespace kept, for the reason
/// [`super::sysv_msg::forget`] gives.
///
/// A set an undo record still names stays alive through the record's own `Arc`,
/// and it is marked removed first so the dying task's exit pass skips it instead
/// of adding to a value nobody can read any more.
pub fn forget(ns: Option<u64>) {
    let gone = critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let at = table.spaces.iter().position(|space| space.ns == ns)?;
        Some(table.spaces.remove(at))
    });
    if let Some(space) = gone {
        for set in space.sets.iter().flatten() {
            set.removed.store(true, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// The waiters
// ---------------------------------------------------------------------------

/// Which of the two sleeps a waiter is in: a negative operation that could not
/// be satisfied, or a zero operation on a non-zero counter. The two are counted
/// separately by `GETNCNT`/`GETZCNT`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    Negative,
    Zero,
}

struct Waiter {
    /// The set's **handle** (`Set::handle`), not its number: two namespaces can
    /// hold sets numbered alike, and a wake meant for one must not reach the
    /// other.
    set: u64,
    sem: usize,
    kind: WaitKind,
    task: TaskId,
    /// Set by a wake. The task polls this from inside `block_until_deadline`,
    /// so a waker never has to find it anywhere else.
    woken: bool,
}

static WAITERS: Mutex<RefCell<Vec<Waiter>>> = Mutex::new(RefCell::new(Vec::new()));

/// Register the running task as asleep on `sem`, *before* the condition is
/// re-checked: the other order has a window in which a raise goes to nobody and
/// the sleeper never hears it.
fn register(set: u64, sem: usize, kind: WaitKind, task: TaskId) {
    critical_section::with(|cs| {
        let mut waiters = WAITERS.borrow(cs).borrow_mut();
        // One place per task: a retry replaces the entry rather than piling up.
        waiters.retain(|waiter| waiter.task != task);
        waiters.push(Waiter {
            set,
            sem,
            kind,
            task,
            woken: false,
        });
    });
}

fn unregister(task: TaskId) {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow_mut()
            .retain(|waiter| waiter.task != task);
    });
}

fn is_woken(task: TaskId) -> bool {
    critical_section::with(|cs| {
        WAITERS
            .borrow(cs)
            .borrow()
            .iter()
            .any(|waiter| waiter.task == task && waiter.woken)
    })
}

/// Mark every task asleep on the set with this handle woken, and wake each one.
///
/// The wake callback rather than `crate::sched::wake` because two callers exist:
/// a `semop` that raised a counter (which may take the task table itself) and
/// the exit path, which already holds it mutably and must hand its own wake
/// through (`exit_group` does the same).
pub fn wake_waiters(set: u64, mut wake: impl FnMut(TaskId)) {
    let woken: Vec<TaskId> = critical_section::with(|cs| {
        let mut waiters = WAITERS.borrow(cs).borrow_mut();
        let mut woken = Vec::new();
        for waiter in waiters.iter_mut() {
            if waiter.set == set && !waiter.woken {
                waiter.woken = true;
                woken.push(waiter.task);
            }
        }
        woken
    });
    for task in woken {
        wake(task);
    }
}

// ---------------------------------------------------------------------------
// The operations
// ---------------------------------------------------------------------------

/// What one `semop` attempt came to.
pub enum Attempt {
    /// Applied, and the set's counters moved.
    Done,
    /// Cannot proceed now. The running task has been registered as a waiter
    /// when `park` was given, inside the same critical section that found it
    /// blocked - so no raise can slip between the check and the sleep.
    Blocked,
    /// Refused: nothing was applied and no waiter was registered.
    Refused(SemError),
}

/// The value of `sem` as the operations so far in this call have left it.
///
/// Two entries in one array may name the same semaphore, and the second must see
/// the first's effect (Linux checks the array sequentially) - while the stored
/// values may not be written until the whole array is known to be satisfiable.
fn working_value(values: &[i32], changes: &[(usize, i32)], sem: usize) -> i32 {
    for (num, value) in changes.iter().rev() {
        if *num == sem {
            return *value;
        }
    }
    values[sem]
}

/// Try to apply `ops` atomically.
///
/// `park` is the task to register as a waiter if the array cannot proceed, or
/// `None` for a caller that will not block (`IPC_NOWAIT`, or a `SEM_UNDO` list
/// being applied at exit). Registration happens while the set's own lock is
/// held, which is the ordering the "no lost wake" argument depends on: a raiser
/// must take that same lock to change a counter.
pub fn semop_attempt(
    set: &Arc<Set>,
    ops: &[SemBuf],
    undo: Option<&Arc<UndoList>>,
    now: i64,
    pid: u32,
    park: Option<TaskId>,
) -> Attempt {
    let nsems = set.nsems();
    // Read out before the borrow below: `Set::handle` goes through the same
    // `RefCell` this closure holds mutably - the first draft asked for the *id*
    // inside and the machine panicked with "already mutably borrowed" on the
    // first operation that had to block.
    let handle = set.handle();
    critical_section::with(|cs| {
        let mut inner = set.inner.borrow(cs).borrow_mut();
        let mut changes: Vec<(usize, i32)> = Vec::with_capacity(ops.len());
        let mut adjust: Vec<(usize, i32)> = Vec::new();
        let mut blocked: Option<(usize, WaitKind, i16)> = None;
        for op in ops {
            if op.num >= nsems {
                return Attempt::Refused(SemError::BadSem);
            }
            let current = working_value(&inner.values, &changes, op.num);
            let next = match op.op {
                0 => {
                    if current != 0 {
                        blocked = Some((op.num, WaitKind::Zero, op.flags));
                        break;
                    }
                    current
                }
                delta if delta > 0 => {
                    match current.checked_add(delta) {
                        Some(next) if next <= SEMVMX => next,
                        // Past the maximum is a refusal, not a clamp: Linux
                        // answers `ERANGE` (measured on a current host).
                        _ => return Attempt::Refused(SemError::Range),
                    }
                }
                delta => match current.checked_add(delta) {
                    Some(next) if next >= 0 => next,
                    // Not enough value to take: this is the wait, not an error.
                    _ => {
                        blocked = Some((op.num, WaitKind::Negative, op.flags));
                        break;
                    }
                },
            };
            changes.push((op.num, next));
            if op.flags & SEM_UNDO != 0 && op.op != 0 {
                // The adjustment is the operation **negated**: it is what the
                // exit pass adds back, so a `+2` recorded here is a `-2` there
                // (Linux's `semadj -= sem_op`). Measured the only way that
                // settles it: `+1` with `SEM_UNDO`, then exit, leaves the value
                // at 0 rather than 2.
                adjust.push((op.num, -op.op));
            }
        }
        if let Some((sem, kind, flags)) = blocked {
            // `IPC_NOWAIT` is the *operation's* flag, not the call's: the array
            // that cannot proceed answers `EAGAIN` when the entry that blocked
            // carries it, and sleeps otherwise. Measured both ways, and the
            // first draft asked the caller instead - so a `semop(-1,
            // IPC_NOWAIT)` at zero slept for ever and hung the sweep.
            if flags & IPC_NOWAIT != 0 || park.is_none() {
                return Attempt::Refused(SemError::WouldBlock);
            }
            register(handle, sem, kind, park.expect("checked above"));
            return Attempt::Blocked;
        }
        for (sem, value) in &changes {
            inner.values[*sem] = *value;
        }
        inner.otime = now;
        inner.lpid = pid;
        if let Some(undo) = undo {
            for (sem, delta) in adjust {
                // The record names the **object**, not its number: a task that
                // leaves its IPC namespace must not have the adjustment land on
                // whatever set holds the same number in the namespace it
                // entered (§6.16).
                undo.record(set, sem, delta);
            }
        }
        Attempt::Done
    })
}

/// Sleep until a wake marks this task, the deadline passes, or a signal arrives.
///
/// `deadline` is **nanoseconds** since boot, the clock `semtimedop`'s relative
/// timeout is added to - and the reason this is `block_until_nanos` and not
/// `block_until_deadline`: the latter takes a *tick* count and multiplies it by
/// the machine's tick, so a nanosecond value passed to it becomes a wait
/// hundreds of times longer (the first draft did exactly that, and a 120 ms
/// `semtimedop` hung the sweep instead of answering `EAGAIN`).
///
/// The caller has already registered itself (inside the attempt that found the
/// array blocked), so this only waits and then leaves the queue.
#[cfg(target_os = "none")]
pub fn park(task: TaskId, deadline: Option<u64>) -> super::WaitEnd {
    let end = super::block_until_nanos(|| is_woken(task), deadline.unwrap_or(u64::MAX));
    unregister(task);
    end
}

// ---------------------------------------------------------------------------
// The undo list
// ---------------------------------------------------------------------------

/// One process's `SEM_UNDO` adjustments: what to add back to a counter when the
/// process exits.
///
/// Per *process*, which is why it is held by `Arc` and shared by the tasks a
/// `CLONE_SYSVSEM` clone creates (that is the flag's whole meaning, and
/// `pthread_create` passes it). A plain `fork` gives the child **no** list - not
/// a copy - which is measured: a child that exits does not apply its parent's
/// adjustments. The last holder's exit applies the list, which is Linux's
/// `exit_sem` and its `refcount_dec_and_test`.
pub struct UndoList {
    records: Mutex<RefCell<Vec<Undo>>>,
}

/// One adjustment: the set **object**, the slot in it, and what to add back at
/// exit - the operation *negated*, which is Linux's `semadj`.
///
/// The object rather than its number, which used to be enough: numbers are
/// per-namespace now, so a record keyed by one could name a stranger's set after
/// an `unshare(CLONE_NEWIPC)` (§6.16). Holding the `Arc` is also what lets the
/// exit pass see that the set was removed - `Set::removed` is the flag the
/// record reads before it touches anything.
struct Undo {
    set: Arc<Set>,
    sem: usize,
    adjustment: i32,
}

impl Default for UndoList {
    fn default() -> UndoList {
        UndoList {
            records: Mutex::new(RefCell::new(Vec::new())),
        }
    }
}

impl UndoList {
    pub fn new() -> UndoList {
        UndoList::default()
    }

    /// Add `delta` to this set-and-slot's adjustment, folding into a record that
    /// is already there: Linux's `sem_undo` keeps one `semadj` per set and slot,
    /// which is also what makes a `+1` then a `-1` cancel out to nothing.
    fn record(&self, set: &Arc<Set>, sem: usize, delta: i32) {
        critical_section::with(|cs| {
            let mut records = self.records.borrow(cs).borrow_mut();
            if let Some(record) = records
                .iter_mut()
                .find(|record| Arc::ptr_eq(&record.set, set) && record.sem == sem)
            {
                record.adjustment = record
                    .adjustment
                    .saturating_add(delta)
                    .clamp(-SEMAEM, SEMAEM);
                return;
            }
            records.push(Undo {
                set: Arc::clone(set),
                sem,
                adjustment: delta.clamp(-SEMAEM, SEMAEM),
            });
        });
    }

    /// How many records it holds, for the kernel's own tests and the exit pass's
    /// "nothing to do" case.
    pub fn len(&self) -> usize {
        critical_section::with(|cs| self.records.borrow(cs).borrow().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take every record out, leaving the list empty: the caller is applying
    /// them and must not do it twice.
    fn take(&self) -> Vec<Undo> {
        critical_section::with(|cs| core::mem::take(&mut *self.records.borrow(cs).borrow_mut()))
    }
}

/// Apply a dying task's undo list, then wake everyone a change could satisfy.
///
/// The value is *clamped* to `0..=SEMVMX` rather than refused - an adjustment
/// from the past meets counters that have moved since, and Linux's `sem_undo`
/// clamps for the same reason. A set that was removed in the meantime is
/// skipped: its records are about an object that no longer exists.
pub fn apply_undo(list: &UndoList, mut wake: impl FnMut(TaskId)) {
    let records = list.take();
    let mut touched: Vec<u64> = Vec::new();
    for record in records {
        // A set that was removed in the meantime is skipped: the record is about
        // an object that no longer exists, and its number may already name a
        // different one in this namespace.
        if record.set.removed() {
            continue;
        }
        let set = &record.set;
        critical_section::with(|cs| {
            let mut inner = set.inner.borrow(cs).borrow_mut();
            let Some(value) = inner.values.get_mut(record.sem) else {
                return;
            };
            *value = value.saturating_add(record.adjustment).clamp(0, SEMVMX);
        });
        touched.push(set.handle());
    }
    touched.sort_unstable();
    touched.dedup();
    for set in touched {
        wake_waiters(set, &mut wake);
    }
}

/// `unshare(CLONE_SYSVSEM)`'s half: give the caller a list of its own, carrying
/// the records it already had.
///
/// Measured on a current host (`.workbuddy/probe_sem_unshare.c`): a process that
/// records `+1` with `SEM_UNDO`, calls `unshare(CLONE_SYSVSEM)` and exits has
/// its adjustment applied - so the records are *kept*, not dropped. Moving them
/// out of the shared list is what keeps that true for a process that unshares
/// on its own; the thread-sharing case is named in `docs/notes/` §6.14.
#[cfg(target_os = "none")]
pub fn unshare_undo() {
    super::with_table(|t| {
        let me = t.current;
        let Some(shared) = t.task(me).sem_undo.clone() else {
            return;
        };
        let moved = shared.take();
        let fresh = UndoList::new();
        for record in moved {
            fresh.record(&record.set, record.sem, record.adjustment);
        }
        let mut fresh = Some(Arc::new(fresh));
        if fresh.as_ref().is_some_and(|list| list.is_empty()) {
            fresh = None;
        }
        t.task_mut(me).sem_undo = fresh;
    });
}

/// How many undo records, and how many lists holding them, the machine's tasks
/// have: `SEM_INFO`'s `semusz`/`semaem`, which Linux re-purposes as usage
/// statistics there.
#[cfg(target_os = "none")]
pub fn undo_stats() -> (usize, usize) {
    super::with_table(|t| {
        let mut records = 0;
        let mut lists = 0;
        for task in t.tasks.iter().flatten() {
            if let Some(list) = &task.sem_undo {
                records += list.len();
                lists += 1;
            }
        }
        (records, lists)
    })
}

#[cfg(not(target_os = "none"))]
pub fn undo_stats() -> (usize, usize) {
    (0, 0)
}

/// The running task's undo list, making one if it has none.
#[cfg(target_os = "none")]
pub fn undo_list() -> Arc<UndoList> {
    super::with_table(|t| {
        let me = t.current;
        if let Some(list) = &t.task(me).sem_undo {
            return Arc::clone(list);
        }
        let list = Arc::new(UndoList::new());
        t.task_mut(me).sem_undo = Some(Arc::clone(&list));
        list
    })
}

/// Apply the dying task's undo list if it is the last holder of it.
///
/// Called from the exit path, inside the task table's own borrow - hence the
/// wake callback, which is the caller's `Table::wake` (`crate::sched::wake`
/// would take the table a second time).
#[cfg(target_os = "none")]
pub fn exit_undo(dying: TaskId, group_dead: bool, wake: impl FnMut(TaskId)) {
    super::with_table(|t| {
        let taken = t.task_mut(dying).sem_undo.take();
        let Some(list) = taken else {
            return;
        };
        // The *process's* exit is the one Linux applies on, and the list is
        // applied by whoever holds it last: a thread that is not its group's
        // last leaves the records to its siblings, and a process sharing the
        // list with a `CLONE_SYSVSEM` child leaves them to that child.
        if group_dead && Arc::strong_count(&list) == 1 {
            apply_undo(&list, wake);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limits are what `IPC_INFO` reports and what the table enforces; the
    /// arithmetic tests below do not touch the static table (a host test shares
    /// it with nothing, but a kernel run does).
    #[test]
    fn the_limits_are_reported_and_sane() {
        assert_eq!(SEMMNI, 4_096);
        assert_eq!(SEMMSL, 1_024);
        assert_eq!(SEMMNS, 65_536);
        assert_eq!(SEMOPM, 500);
        assert_eq!(SEMVMX, 32_767);
        assert_eq!(SEMAEM, 32_767);
        // The allowance has to hold at least one full set, or no set could be
        // made at the largest size the other limit allows.
        assert!(SEMMNS >= SEMMSL);
    }

    /// The undo fold: repeated adjustments of one slot are one record, which is
    /// what makes `+1` then `-1` cancel rather than apply twice.
    #[test]
    fn an_undo_record_folds_and_saturates() {
        let set = fresh();
        let list = UndoList::new();
        list.record(&set, 0, -3);
        list.record(&set, 0, 2);
        list.record(&set, 1, 5);
        assert_eq!(list.len(), 2);
        let records = list.take();
        assert!(list.is_empty());
        assert_eq!(records.len(), 2);
        let first = records.iter().find(|r| r.sem == 0).expect("slot 0");
        assert_eq!(first.adjustment, -1);
        let second = records.iter().find(|r| r.sem == 1).expect("slot 1");
        assert_eq!(second.adjustment, 5);
    }

    /// A set made for a test: `IPC_PRIVATE` so no other test can collide with
    /// it, one semaphore, mode 0600.
    /// The machine's own namespace, the key every host test's table reads use.
    const MACHINE: Option<u64> = None;

    fn fresh() -> alloc::sync::Arc<Set> {
        get_or_create(
            MACHINE,
            New {
                key: IPC_PRIVATE,
                nsems: 1,
                mode: 0o600,
                uid: 0,
                gid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a private set")
        .0
    }

    fn buf(num: usize, op: i32, flags: i16) -> SemBuf {
        SemBuf { num, op, flags }
    }

    /// A raise applies; a `-1` at zero with `IPC_NOWAIT` refuses with the
    /// *caller's* signal to try again, and leaves no waiter behind - the bug
    /// the first draft had was registering one anyway, so a non-blocking call
    /// slept for ever.
    #[test]
    fn a_nowait_refusal_registers_no_waiter() {
        let set = fresh();
        assert!(matches!(
            semop_attempt(&set, &[buf(0, 1, 0)], None, 1, 1, None),
            Attempt::Done
        ));
        assert_eq!(set.value(0), Some(1));
        // `-2` against a value of 1: the take cannot be satisfied, which is the
        // refusal a `-1` at zero would be in a freshly made set.
        match semop_attempt(&set, &[buf(0, -2, IPC_NOWAIT)], None, 1, 1, Some(TaskId(1))) {
            Attempt::Refused(SemError::WouldBlock) => {}
            _ => panic!("a NOWAIT take it cannot satisfy must refuse"),
        }
        assert_eq!(set.waiting(0), (0, 0));
        assert_eq!(set.value(0), Some(1));
    }

    /// A blocking `-1` registers its waiter *and* changes nothing; a later
    /// raise wakes it, and the retry then applies.
    #[test]
    fn a_blocked_operation_registers_and_nothing_is_applied() {
        let set = fresh();
        let me = TaskId(7);
        match semop_attempt(&set, &[buf(0, -1, 0)], None, 1, 1, Some(me)) {
            Attempt::Blocked => {}
            _ => panic!("a `-1` at zero must block"),
        }
        assert_eq!(set.waiting(0), (1, 0));
        assert_eq!(set.value(0), Some(0));
        // The raise that a waker would make, then the retry the woken task
        // runs: the value is consumed by the second attempt.
        assert!(matches!(
            semop_attempt(&set, &[buf(0, 1, 0)], None, 2, 2, None),
            Attempt::Done
        ));
        assert!(!is_woken(me));
        wake_waiters(set.handle(), |_| {});
        assert!(is_woken(me));
        assert!(matches!(
            semop_attempt(&set, &[buf(0, -1, 0)], None, 3, 2, Some(me)),
            Attempt::Done
        ));
        assert_eq!(set.value(0), Some(0));
        unregister(me);
    }

    /// The array is all-or-nothing: a second entry that cannot proceed leaves
    /// the first entry's slot alone.
    #[test]
    fn an_array_that_cannot_proceed_changes_nothing() {
        let set = get_or_create(
            MACHINE,
            New {
                key: IPC_PRIVATE,
                nsems: 2,
                mode: 0o600,
                uid: 0,
                gid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a private set")
        .0;
        assert!(matches!(
            semop_attempt(
                &set,
                &[buf(0, 1, 0), buf(1, -1, IPC_NOWAIT)],
                None,
                1,
                1,
                Some(TaskId(1))
            ),
            Attempt::Refused(SemError::WouldBlock)
        ));
        assert_eq!(set.value(0), Some(0));
        assert_eq!(set.value(1), Some(0));
    }

    /// Both ends of the range refuse: a raise past `SEMVMX` and a take below
    /// zero when the caller asked not to wait.
    #[test]
    fn the_range_refuses_at_both_ends() {
        let set = fresh();
        assert!(matches!(
            semop_attempt(&set, &[buf(0, SEMVMX, 0)], None, 1, 1, None),
            Attempt::Done
        ));
        assert!(matches!(
            semop_attempt(&set, &[buf(0, 1, IPC_NOWAIT)], None, 1, 1, Some(TaskId(1))),
            Attempt::Refused(SemError::Range)
        ));
        assert!(matches!(
            set.set_value(0, SEMVMX + 1, 1),
            Err(SemError::Range)
        ));
    }

    /// `SEM_UNDO` records the *requested* delta, and applying the list moves the
    /// value back.
    #[test]
    fn an_undo_record_is_applied_to_the_set() {
        let set = fresh();
        let list = alloc::sync::Arc::new(UndoList::new());
        assert!(matches!(
            semop_attempt(&set, &[buf(0, 2, SEM_UNDO)], Some(&list), 1, 1, None),
            Attempt::Done
        ));
        assert_eq!(set.value(0), Some(2));
        assert_eq!(list.len(), 1);
        apply_undo(&list, |_| {});
        assert_eq!(set.value(0), Some(0));
        assert!(list.is_empty());
    }

    /// `working_value` is the sequential reading an operation array needs: a
    /// second entry naming the same slot sees the first's effect.
    #[test]
    fn a_second_operation_on_one_slot_sees_the_first() {
        let values = [10, 20];
        let changes = [(0usize, 11i32)];
        assert_eq!(working_value(&values, &changes, 0), 11);
        assert_eq!(working_value(&values, &changes, 1), 20);
        let changes = [(0, 11), (0, 12)];
        assert_eq!(working_value(&values, &changes, 0), 12);
    }
}
