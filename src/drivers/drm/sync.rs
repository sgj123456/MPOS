pub(crate) struct SyncObjRec {
    pub(crate) owner: u64,
    handle: u32,
    obj: Arc<crate::gpu::sync::SyncObj>,
    /// One entry per `SYNCOBJ_EVENTFD` call. Each fires at most once - that is
    /// what was measured, and it is what makes "register, signal, read one"
    /// composable.
    events: Vec<SyncEvent>,
}

/// One `SYNCOBJ_EVENTFD` registration: the counter to bump, the point it was
/// asked about, and whether it has already been handed its one increment.
use super::abi::{
    DRM_CLOEXEC, SYNCOBJ_CREATE_SIGNALED, SYNCOBJ_EXPORT_SYNC_FILE, SYNCOBJ_FLAGS_TIMELINE,
    SYNCOBJ_IMPORT_SYNC_FILE, SYNCOBJ_QUERY_LAST_SUBMITTED, SYNCOBJ_WAIT_ALL,
    SYNCOBJ_WAIT_AVAILABLE, SYNCOBJ_WAIT_DEADLINE, SYNCOBJ_WAIT_FOR_SUBMIT,
};
use super::ioctl::{libc_einval, user_array};
use super::state::{DrmState, STATE};

use crate::vfs::{Inode, Metadata, NodeKind, Ready};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

struct SyncEvent {
    /// The eventfd node itself, held as a node rather than as a number: a
    /// registration outlives the request that made it, and the increment it owes
    /// later has to reach a counter that is still there. This is the reference
    /// Linux's `eventfd_ctx_fdget` takes for the same reason.
    counter: Arc<dyn Inode>,
    /// The point asked about. Zero is not a point: it is the request saying
    /// "whatever this object is", which is how a plain binary object is
    /// registered, and the measured card fires that one on the fence alone.
    point: u64,
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`: fire when *a fence exists* rather
    /// than when the point is covered.
    available: bool,
    fired: bool,
}

/// Cap on the sync objects one card tracks. Linux numbers them from an idr with
/// no ceiling of its own; this card has to bound a table a client can grow
/// (`AGENTS.md` §4.4), and no compositor is within two orders of magnitude of
/// this one.
const MAX_SYNCOBJS: usize = 256;
/// Cap on the handles one `SYNCOBJ_WAIT` may name. The real card accepted 300;
/// this is the point past which the count is a corrupted struct rather than a
/// request.
const MAX_SYNC_WAIT: usize = 4096;

/// The sync objects' state machine, in the one place a host test can reach it.
/// What it means is documented there and measured in `NOTES.md` §7.4.
///
/// The descriptor `SYNCOBJ_HANDLE_TO_FD` with `flags == 0` answers: an anonymous
/// file that *is* the sync object, so a second description that takes it in
/// (`SCM_RIGHTS`, or simply a `dup`) can `SYNCOBJ_FD_TO_HANDLE` the same object
/// into its own handle table. That sharing is the whole point of the protocol it
/// exists for - `wp_linux_drm_syncobj_v1` passes one between the client that
/// rendered and the compositor that will wait.
pub struct SyncObjFd {
    ino: u64,
    obj: Arc<crate::gpu::sync::SyncObj>,
}

impl SyncObjFd {
    fn new(obj: Arc<crate::gpu::sync::SyncObj>) -> Arc<SyncObjFd> {
        static NEXT: AtomicU64 = AtomicU64::new(0x9200);
        Arc::new(SyncObjFd {
            ino: NEXT.fetch_add(1, Ordering::Relaxed),
            obj,
        })
    }

    /// The object behind the descriptor, for the import that installs a handle
    /// for it in another table.
    pub fn object(&self) -> Arc<crate::gpu::sync::SyncObj> {
        self.obj.clone()
    }
}

impl Inode for SyncObjFd {
    fn metadata(&self) -> Metadata {
        // Anonymous, like every other descriptor that is not a file: the number
        // only has to be one nothing else claims, and `fstat` is the only
        // reporter.
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }
}

/// The descriptor `SYNCOBJ_HANDLE_TO_FD` with `EXPORT_SYNC_FILE` answers, and
/// the one a commit's out-fence leaves in the caller's table: `struct sync_file`,
/// a fence behind a file descriptor.
///
/// It is a *readable* descriptor - `poll` reports `POLLIN` once the fence has
/// retired - and that readiness is the only thing it has to say, which is why
/// there is no `read_at` here: a caller waits on it or hands it to someone who
/// will.
pub struct SyncFileFd {
    ino: u64,
    fence: Arc<crate::gpu::sync::Fence>,
    /// The timeline point the fence was made at, snapshotted when this
    /// descriptor was made - 0 for a fence that has no point, which is every
    /// fence this card makes for a *frame*.
    ///
    /// It is a snapshot rather than a reference to the object it came from
    /// because that is what the measurement says it is: exporting a timeline
    /// object's `sync_file` and then importing it lands the destination on the
    /// point the object had *when it was exported*, which is the same fact
    /// Linux keeps in the exported fence's own sequence number. A later signal
    /// on the source does not move a descriptor already handed out.
    point: u64,
}

impl SyncFileFd {
    /// A `sync_file` over a fence with no timeline point - the shape every fence
    /// this card makes for a frame is, because there is no engine here to have a
    /// sequence number.
    pub(crate) fn new(fence: Arc<crate::gpu::sync::Fence>) -> Arc<SyncFileFd> {
        SyncFileFd::at(fence, 0)
    }

    /// A `sync_file` over a fence *and* the point it was made at.
    pub(crate) fn at(fence: Arc<crate::gpu::sync::Fence>, point: u64) -> Arc<SyncFileFd> {
        static NEXT: AtomicU64 = AtomicU64::new(0x9300);
        Arc::new(SyncFileFd {
            ino: NEXT.fetch_add(1, Ordering::Relaxed),
            fence,
            point,
        })
    }

    /// The fence behind the descriptor, for `FD_TO_HANDLE` with
    /// `IMPORT_SYNC_FILE`.
    pub fn fence(&self) -> Arc<crate::gpu::sync::Fence> {
        self.fence.clone()
    }

    /// The point that fence was made at - what an import *without* the
    /// `TIMELINE` flag lands its destination on.
    pub fn point(&self) -> u64 {
        self.point
    }
}

impl Inode for SyncFileFd {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// A `sync_file` is readable exactly when its fence has retired - Linux's
    /// answer, and the one a compositor's event loop is built on.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.fence.has_retired(),
            write: false,
        }
    }
}

/// The object a `SyncObjFd` holds, behind one more `Arc` so that the descriptor
/// type can live here while the object's own type lives in the host-tested leaf
/// module.
///
/// The extra indirection is deliberate rather than accidental: `crate::gpu::sync`
/// knows nothing about descriptors, and a `pub fn` on it that returned an `Arc`
/// of its own type would be the same thing without the wrapper. This is the
/// wrapper.
///
/// (The alias's fields are `pub(crate)` in spirit; see `drm_sync::SyncObjectAlias`.)
fn syncobj_lookup(
    st: &DrmState,
    owner: u64,
    handle: u32,
) -> Option<Arc<crate::gpu::sync::SyncObj>> {
    st.syncobjs
        .iter()
        .flatten()
        .find(|rec| rec.owner == owner && rec.handle == handle)
        .map(|rec| rec.obj.clone())
}

/// Resolve every handle of a `WAIT`-shaped request before any condition is
/// looked at: a request that names an object the caller does not have is refused
/// as a whole rather than partly answered, which is the same property the
/// commit path keeps with its plan.
fn syncobj_resolve(
    st: &DrmState,
    owner: u64,
    handles: &[u32],
) -> Result<Vec<Arc<crate::gpu::sync::SyncObj>>, isize> {
    let mut out = Vec::with_capacity(handles.len());
    for &handle in handles {
        match syncobj_lookup(st, owner, handle) {
            Some(obj) => out.push(obj),
            None => return Err(-2), // ENOENT
        }
    }
    Ok(out)
}

/// The handle array of a `SYNCOBJ_WAIT`/`_RESET`/`_SIGNAL`-shaped request.
///
/// `count == 0` is a successful no-op on the measured card and not an error, so
/// it is answered before the pointer is looked at - and a null pointer with a
/// non-zero count is `EFAULT`, which is what the kernel's own `copy_from_user`
/// of the array would give.
fn syncobj_handles(ptr: usize, count: usize) -> Result<Vec<u32>, isize> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_SYNC_WAIT {
        return Err(libc_einval());
    }
    if ptr == 0 || !user_array(ptr, count, 4, false) {
        return Err(-14); // EFAULT
    }
    Ok(unsafe { core::slice::from_raw_parts(ptr as *const u32, count) }.to_vec())
}

/// A DRM absolute `CLOCK_MONOTONIC` deadline in nanoseconds, as the tick
/// deadline `block_until_deadline` keeps.
///
/// Zero is "do not wait" and not "wait for one tick": the measured card answers
/// `ETIME` at once when a `WAIT_FOR_SUBMIT` cannot be satisfied, so the deadline
/// has to be *now* - which is what `block_until_deadline` reports on its first
/// pass. A negative timeout is the header's "wait for ever".
fn wait_deadline(nsec: i64) -> u64 {
    if nsec < 0 {
        return u64::MAX;
    }
    let per_tick = 1_000_000_000u64 / crate::hal::TICK_HZ as u64;
    let left = (nsec as u64).saturating_sub(crate::hal::since_boot_nanos());
    crate::hal::ticks().saturating_add(left / per_tick.max(1))
}

/// Park until `satisfied` holds, the deadline passes, or a signal arrives, and
/// answer the errno the request owes. Returns 0 when the wait ended because the
/// condition held.
///
/// The objects are the ones a waiter has to be registered on: a sync object's
/// readiness changes when *another task submits a fence to it*, and that task has
/// no way to find a sleeper it was not told about - the same lock-poll-try-unlock-
/// park shape `vfs::eventfd` uses for its counter.
///
/// The waking half of that is the kernel's rather than the state machine's: a
/// parked waiter here is a scheduler task, and `gpu::sync` deliberately cannot
/// name one, so the *how* arrives as this implementation of the crate's `Waker`.
struct SchedWaker;

impl crate::gpu::sync::Waker for SchedWaker {
    fn wake(&mut self, waiter: crate::gpu::sync::WaiterId) {
        crate::sched::wake(crate::sched::TaskId(waiter));
    }
}

fn syncobj_park(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    nsec: i64,
    satisfied: impl Fn() -> bool,
) -> isize {
    let me = crate::sched::current().0;
    for obj in objs {
        obj.subscribe(me);
    }
    let end = crate::sched::block_until_deadline(&satisfied, wait_deadline(nsec));
    for obj in objs {
        obj.unsubscribe(me);
    }
    match end {
        crate::sched::WaitEnd::Condition => 0,
        crate::sched::WaitEnd::Deadline => -62,   // ETIME
        crate::sched::WaitEnd::Interrupted => -4, // EINTR
    }
}

/// Whether one handle of a wait array is satisfied. `point == 0` is a plain
/// object's question, which is "is a fence behind it" - the measured card's
/// reading, where a point that was never asked about is not asked about.
fn syncobj_one_ready(obj: &crate::gpu::sync::SyncObj, point: u64, for_submit: bool) -> bool {
    obj.ready((point != 0).then_some(point), for_submit)
}

/// The readiness of a whole `WAIT` array: `all` is
/// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL`, and otherwise *any* satisfied handle ends
/// the wait.
fn syncobj_any_ready(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    points: &[u64],
    for_submit: bool,
    all: bool,
) -> bool {
    if all {
        objs.iter()
            .zip(points.iter())
            .all(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
    } else {
        objs.iter()
            .zip(points.iter())
            .any(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
    }
}

/// Which handle of the array the answer's `first_signaled` names: the first one
/// that is satisfied. `WAIT_ALL` has no such index to give and the measured card
/// leaves the field alone for it, so this is only meaningful for the `any` shape.
fn syncobj_first_ready(
    objs: &[Arc<crate::gpu::sync::SyncObj>],
    points: &[u64],
    for_submit: bool,
) -> u32 {
    objs.iter()
        .zip(points.iter())
        .position(|(obj, &point)| syncobj_one_ready(obj, point, for_submit))
        .unwrap_or(0) as u32
}

/// Hand the eventfd registrations on a record whatever they have come due - the
/// increment each was promised, once.
fn fire_sync_events(rec: &mut SyncObjRec) {
    for event in rec.events.iter_mut() {
        if event.fired {
            continue;
        }
        let due = if event.available {
            // `WAIT_AVAILABLE`: a fence existing is the whole question.
            rec.obj.has_fence()
        } else {
            // Point 0 is not a point: it is the request saying "whatever this
            // object is", which is how a plain binary syncobj is registered.
            let point = (event.point != 0).then_some(event.point);
            rec.obj.ready(point, false)
        };
        if due {
            event.fired = true;
            // One increment, saturating, which is what wakes whoever is in
            // `poll` on the eventfd - the reader an event loop already has. The
            // write is the counter node's own, so it carries the same wake the
            // eventfd's `write` syscall does rather than a second one beside it.
            let _ = Inode::write_at(&*event.counter, 0, &1u64.to_ne_bytes());
        }
    }
}

/// `SYNCOBJ_CREATE`: a new object, and the handle that names it.
///
/// `DRM_SYNCOBJ_CREATE_SIGNALED` is the difference between an object every
/// request about a fence will refuse and one with a fence already behind it -
/// measured, and the opposite reading from the upstream sources, which treat
/// *both* as signaled. `NOTES.md` §7.4 has the measurement.
pub(crate) fn syncobj_create(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_create: handle@0, flags@4.
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if flags & !SYNCOBJ_CREATE_SIGNALED != 0 {
        return libc_einval();
    }
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        if st.syncobjs.len() >= MAX_SYNCOBJS {
            return 0;
        }
        // Lowest free number *for this description*, which is what Linux's idr
        // gives and what a test that creates and destroys in a loop sees.
        let mut handle = 1u32;
        while syncobj_lookup(st, owner, handle).is_some() {
            handle += 1;
        }
        st.syncobjs.push(Some(SyncObjRec {
            owner,
            handle,
            obj: crate::gpu::sync::SyncObj::new(flags & SYNCOBJ_CREATE_SIGNALED != 0),
            events: Vec::new(),
        }));
        handle
    });
    if handle == 0 {
        return libc_einval(); // the table is full
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

/// `SYNCOBJ_DESTROY`: the handle stops naming anything.
///
/// The object itself lives on while a `SYNCOBJ_HANDLE_TO_FD` descriptor names it,
/// which is exactly the sharing the descriptor exists for. A registration that
/// has not fired never will, which is what the measured card does: destroying the
/// object leaves the eventfd at zero rather than handing it an increment for a
/// point nobody will reach.
pub(crate) fn syncobj_destroy(owner: u64, arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // A handle that names nothing is `EINVAL` here and `ENOENT` from the
        // requests that *operate* on an object. Both were measured that way.
        let Some(slot) = st.syncobjs.iter_mut().find(|rec| {
            rec.as_ref()
                .is_some_and(|rec| rec.owner == owner && rec.handle == handle)
        }) else {
            return libc_einval();
        };
        *slot = None;
        st.syncobjs.retain(Option::is_some);
        0
    })
}

/// `SYNCOBJ_HANDLE_TO_FD`: the object behind a descriptor.
///
/// Two descriptors come out of this, and which one is asked for is
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`. Without it the descriptor
/// *is* the object and another description can name the same timeline; with it
/// the descriptor is a `sync_file` over the fence the object holds, so a caller
/// that only wants to wait on one gets nothing that could be handed on. The
/// export refuses an object with no fence, which the measured card does and the
/// sources do not - there is nothing to export.
pub(crate) fn syncobj_handle_to_fd(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_handle: handle@0, flags@4, fd@8, pad@12, point@16.
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    if flags & !(SYNCOBJ_EXPORT_SYNC_FILE | SYNCOBJ_FLAGS_TIMELINE | DRM_CLOEXEC) != 0 {
        return libc_einval();
    }
    // What `point` means, measured on the real card for every combination of
    // flags and points (§7.4) rather than read off the sources, which say
    // nothing about it:
    //
    //   * **A non-zero point needs `EXPORT_SYNC_FILE`.** That is the only form
    //     whose descriptor can carry one: `flags = 0` and `TIMELINE` alone with
    //     point 5 are both `EINVAL`, while `EXPORT_SYNC_FILE` with the same
    //     point on the same object succeeds.
    //   * **With `TIMELINE` the point is a *timeline* point**, so it must be one
    //     the object has reached: a binary object - which has no timeline and so
    //     no point at all - refuses any non-zero point, and an object at 5
    //     answers 0, 1, 3 and 5 and refuses 6 and 100. That last pair is the
    //     part worth having measured: the rule is `point <= the object's`, not
    //     merely "the object is a timeline".
    //   * **Without `TIMELINE` the point is not read at all.** `EXPORT_SYNC_FILE`
    //     with point 5 on a *binary* object succeeds, which is what says the flag
    //     is what turns the number into a timeline point rather than the number
    //     by itself.
    if point != 0 && flags & SYNCOBJ_EXPORT_SYNC_FILE == 0 {
        return libc_einval();
    }
    let obj = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_lookup(&st, owner, handle)
    });
    let Some(obj) = obj else {
        return -2; // ENOENT
    };
    if point != 0
        && flags & SYNCOBJ_FLAGS_TIMELINE != 0
        && !(obj.is_timeline() && point <= obj.point())
    {
        return libc_einval();
    }
    let want_file = flags & SYNCOBJ_EXPORT_SYNC_FILE != 0;
    let inode: Arc<dyn Inode> = if want_file {
        match obj.fence() {
            // The descriptor carries the point the object has *now*, and the
            // measurement is what says so rather than the request's `point`: an
            // export asked for at point 5 from an object at 9 lands a later
            // import on **9** (§7.4). It is also what makes the point a snapshot
            // instead of a reference - the object may move on, and the fence a
            // caller is holding does not.
            Some(fence) => SyncFileFd::at(fence, obj.point()),
            None => return libc_einval(),
        }
    } else {
        SyncObjFd::new(obj)
    };
    let file = Arc::new(crate::vfs::File::new(inode));
    let fd = match crate::sched::current_fds().install(file) {
        Ok(fd) => fd,
        Err(crate::vfs::VfsError::TooManyDescriptors) => return -24, // EMFILE
        Err(_) => return libc_einval(),
    };
    if flags & DRM_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    unsafe { ((arg + 8) as *mut i32).write_unaligned(fd) };
    0
}

/// `SYNCOBJ_FD_TO_HANDLE`: a descriptor's object, under a handle in the caller's
/// own table.
///
/// The two directions take the two descriptors `HANDLE_TO_FD` makes, and they do
/// not take each other's: without the flag the descriptor must *be* the object
/// (and `handle` must be 0, because a new name is the only thing this can
/// produce), while with `IMPORT_SYNC_FILE` it must be a `sync_file` and `handle`
/// must name an object that already exists, whose fence is then replaced. Asking
/// for a new handle with the import flag is `ENOENT` on the measured card - and
/// that is the whole difference between the two, so it is pinned rather than
/// smoothed over.
pub(crate) fn syncobj_fd_to_handle(owner: u64, arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let fd = unsafe { ((arg + 8) as *const i32).read_unaligned() };
    let point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    if flags & !(SYNCOBJ_IMPORT_SYNC_FILE | SYNCOBJ_FLAGS_TIMELINE) != 0 {
        return libc_einval();
    }
    // The other half of the export side's first rule, and measured the same way:
    // a non-zero point is the *import* form's to name, and with `flags = 0` or
    // `TIMELINE` alone it is refused - importing a plain object with point 5 is
    // `EINVAL` where point 0 succeeds. What the import form does with the point
    // is the gap named below.
    if point != 0 && flags & SYNCOBJ_IMPORT_SYNC_FILE == 0 {
        return libc_einval();
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(_) => return -9, // EBADF
    };
    let inode = file.inode();
    if flags & SYNCOBJ_IMPORT_SYNC_FILE != 0 {
        // The object is named first, so that a caller which has neither hears
        // the same answer the measured card gives rather than the fd's.
        let Some(target) = critical_section::with(|cs| {
            let st = STATE.borrow(cs).borrow();
            syncobj_lookup(&st, owner, handle)
        }) else {
            return -2; // ENOENT
        };
        let Some(sync_file) = inode.as_any().downcast_ref::<SyncFileFd>() else {
            return libc_einval();
        };
        // Which point lands, and it is the flag that decides - measured both
        // ways on the real card (§7.4), because the two are not variants of one
        // rule but two rules:
        //
        //   * **With `TIMELINE` the request names it**, and the bookkeeping is
        //     every other point writer's: it *raises*, so a destination at 4
        //     asked for 2 stays at 4 and one asked for 6 goes to 6.
        //   * **Without it the fence's own point lands, exactly.** Importing the
        //     `sync_file` of a timeline at 9 into an object at 4 leaves that
        //     object at **9**; importing a plain object's leaves it at **0**,
        //     which is a point going *backwards* - the one door here that may do
        //     that. Both shapes leave the destination a timeline, which is
        //     `replace`'s other half.
        if flags & SYNCOBJ_FLAGS_TIMELINE != 0 {
            target.submit(sync_file.fence(), Some(point), &mut SchedWaker);
        } else {
            target.replace(sync_file.fence(), sync_file.point(), &mut SchedWaker);
        }
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            sync_events_left(&mut st, owner, handle);
        });
        return 0;
    }
    if handle != 0 {
        // Without the import flag there is nothing to replace.
        return libc_einval();
    }
    let Some(syncobj_fd) = inode.as_any().downcast_ref::<SyncObjFd>() else {
        return libc_einval();
    };
    let object = syncobj_fd.object();
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        if st.syncobjs.len() >= MAX_SYNCOBJS {
            return 0;
        }
        let mut handle = 1u32;
        while syncobj_lookup(st, owner, handle).is_some() {
            handle += 1;
        }
        st.syncobjs.push(Some(SyncObjRec {
            owner,
            handle,
            obj: object,
            events: Vec::new(),
        }));
        handle
    });
    if handle == 0 {
        return libc_einval();
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

/// `SYNCOBJ_WAIT`: wait for the fences behind a list of objects.
///
/// The measured rule that makes this unlike the sources: **an object with no
/// fence is refused**, not treated as already signaled, unless the caller said
/// `WAIT_FOR_SUBMIT` - in which case there is something to wait for after all and
/// the wait parks until another task submits a fence. Without that flag a wait on
/// an empty object could never end, which is why the card answers `EINVAL` rather
/// than blocking.
pub(crate) fn syncobj_wait(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_wait: handles@0, timeout_nsec@8, count_handles@16,
    // flags@20, first_signaled@24, pad@28, deadline_nsec@32.
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let timeout = unsafe { ((arg + 8) as *const i64).read_unaligned() };
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    const KNOWN: u32 =
        SYNCOBJ_WAIT_ALL | SYNCOBJ_WAIT_FOR_SUBMIT | SYNCOBJ_WAIT_AVAILABLE | SYNCOBJ_WAIT_DEADLINE;
    if flags & !KNOWN != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if handles.is_empty() {
        return 0;
    }
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    let all = flags & SYNCOBJ_WAIT_ALL != 0;
    let for_submit = flags & SYNCOBJ_WAIT_FOR_SUBMIT != 0;
    // No point is named: a plain object's whole question is whether a fence is
    // behind it.
    let points = alloc::vec![0u64; objs.len()];
    let satisfied = || syncobj_any_ready(&objs, &points, for_submit, all);
    if !for_submit && !satisfied() {
        // An object with no fence, and no promise that one is coming.
        return libc_einval();
    }
    if !satisfied() {
        let rc = syncobj_park(&objs, timeout, satisfied);
        if rc != 0 {
            return rc;
        }
    }
    if !all {
        let first = syncobj_first_ready(&objs, &points, for_submit);
        unsafe { ((arg + 24) as *mut u32).write_unaligned(first) };
    }
    0
}

/// `SYNCOBJ_TIMELINE_WAIT`: the same, for a point on each object.
///
/// Two rules are this request's own. An object that has never had a point
/// submitted is **not a timeline**, and asking it about a point is `EINVAL`
/// rather than a wait that could never end. And `WAIT_FOR_SUBMIT` again settles
/// on the fence's presence: the measured card answers `ok` to a point it has
/// never reached, as soon as something is behind the object.
pub(crate) fn syncobj_timeline_wait(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_timeline_wait: handles@0, points@8, timeout_nsec@16,
    // count_handles@24, flags@28, first_signaled@32, pad@36, deadline_nsec@40.
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let timeout = unsafe { ((arg + 16) as *const i64).read_unaligned() };
    let count = unsafe { ((arg + 24) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 28) as *const u32).read_unaligned() };
    const KNOWN: u32 =
        SYNCOBJ_WAIT_ALL | SYNCOBJ_WAIT_FOR_SUBMIT | SYNCOBJ_WAIT_AVAILABLE | SYNCOBJ_WAIT_DEADLINE;
    if flags & !KNOWN != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if handles.is_empty() {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, false) {
        return -14; // EFAULT
    }
    let points = unsafe { core::slice::from_raw_parts(points_ptr as *const u64, count) }.to_vec();
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    if objs.iter().any(|obj| !obj.is_timeline()) {
        return libc_einval(); // a point was never submitted, so there is none to wait on
    }
    let all = flags & SYNCOBJ_WAIT_ALL != 0;
    let for_submit = flags & SYNCOBJ_WAIT_FOR_SUBMIT != 0;
    let satisfied = || syncobj_any_ready(&objs, &points, for_submit, all);
    if !for_submit && !satisfied() {
        return libc_einval();
    }
    if !satisfied() {
        let rc = syncobj_park(&objs, timeout, satisfied);
        if rc != 0 {
            return rc;
        }
    }
    if !all {
        let first = syncobj_first_ready(&objs, &points, for_submit);
        unsafe { ((arg + 32) as *mut u32).write_unaligned(first) };
    }
    0
}

/// `SYNCOBJ_RESET`: the objects stop holding anything.
///
/// A handle that names nothing is `ENOENT` here, unlike `DESTROY`'s `EINVAL` -
/// both measured, and the difference is the one Linux draws: destroying a name
/// that is not there is an argument error, while operating on a name that is not
/// there is a missing object.
pub(crate) fn syncobj_reset(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_array: handles@0, count_handles@8.
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for obj in &objs {
        obj.reset();
    }
    0
}

/// `SYNCOBJ_SIGNAL`: a fence is submitted to each object, retired already.
///
/// On this card that is all a signal can be - there is no engine to hand the
/// work to, so the fence it leaves is one that has retired by the time the
/// request returns. `TIMELINE_SIGNAL` is the same write with a point attached.
pub(crate) fn syncobj_signal(owner: u64, arg: usize) -> isize {
    let ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let handles = match syncobj_handles(ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for obj in &objs {
        obj.submit(crate::gpu::sync::Fence::retired(), None, &mut SchedWaker);
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        for &handle in &handles {
            sync_events_left(&mut st, owner, handle);
        }
    });
    0
}

/// `SYNCOBJ_TIMELINE_SIGNAL`: submit a fence at a point.
///
/// This is the request that *makes* an object a timeline, which is why there is
/// no gate here for one that is not: the measured card accepts it on a plain
/// object and answers its `QUERY` with the point afterwards. Points are accepted
/// below the object's own too, and move nothing - a point that could go backwards
/// would let a wait on a point already passed fail again.
pub(crate) fn syncobj_timeline_signal(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_timeline_array: handles@0, points@8, count@16, flags@20.
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    if flags != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if count == 0 {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, false) {
        return -14; // EFAULT
    }
    let points = unsafe { core::slice::from_raw_parts(points_ptr as *const u64, count) }.to_vec();
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    for (obj, &point) in objs.iter().zip(points.iter()) {
        obj.submit(
            crate::gpu::sync::Fence::retired(),
            Some(point),
            &mut SchedWaker,
        );
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        for &handle in &handles {
            sync_events_left(&mut st, owner, handle);
        }
    });
    0
}

/// `SYNCOBJ_QUERY`: how far each timeline has got.
///
/// An object that has never had a point reports 0, and so does one that was
/// reset, which is the same statement: `RESET` takes the point back to zero.
/// This is the one request in the family that does not need a fence, because
/// "how far has this got" has an answer even when the answer is nothing.
pub(crate) fn syncobj_query(owner: u64, arg: usize) -> isize {
    let handles_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let points_ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 16) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    if flags & !SYNCOBJ_QUERY_LAST_SUBMITTED != 0 {
        return libc_einval();
    }
    let handles = match syncobj_handles(handles_ptr, count) {
        Ok(handles) => handles,
        Err(e) => return e,
    };
    if count == 0 {
        return 0;
    }
    if points_ptr == 0 || !user_array(points_ptr, count, 8, true) {
        return -14; // EFAULT
    }
    let objs = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        syncobj_resolve(&st, owner, &handles)
    });
    let objs = match objs {
        Ok(objs) => objs,
        Err(e) => return e,
    };
    // Every point is written, including the zeros: the array is the caller's
    // output and a point left alone would be a number the card never said.
    unsafe {
        for (i, obj) in objs.iter().enumerate() {
            (points_ptr as *mut u64).add(i).write_unaligned(obj.point());
        }
    }
    0
}

/// `SYNCOBJ_TRANSFER`: move a fence from one object to another.
///
/// Measured on the real card: the request needs *one* fence between its two ends
/// and refuses it with `EINVAL` when neither has one, which is also the honest
/// answer - a transfer that carries nothing and reports success is the silent
/// no-op `AGENTS.md` §4.2 forbids. The destination's point moves to `dst_point`
/// when one was named, with no check that it went up.
pub(crate) fn syncobj_transfer(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_transfer: src@0, dst@4, src_point@8, dst_point@16,
    // flags@24.
    let src = unsafe { (arg as *const u32).read_unaligned() };
    let dst = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let dst_point = unsafe { ((arg + 16) as *const u64).read_unaligned() };
    let flags = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    if flags != 0 {
        return libc_einval();
    }
    let (src_obj, dst_obj) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        (
            syncobj_lookup(&st, owner, src),
            syncobj_lookup(&st, owner, dst),
        )
    });
    let (Some(src_obj), Some(dst_obj)) = (src_obj, dst_obj) else {
        return -2; // ENOENT
    };
    let some_fence = src_obj.has_fence() || dst_obj.has_fence();
    if !some_fence {
        return libc_einval();
    }
    match src_obj.fence() {
        Some(fence) => dst_obj.submit(
            fence,
            (dst_point != 0).then_some(dst_point),
            &mut SchedWaker,
        ),
        // The source carries nothing, so only the destination's own point moves;
        // its fence (the one that made this request legal) is left where it is.
        None => {
            if dst_point != 0 {
                dst_obj.submit_point(dst_point, &mut SchedWaker);
            }
        }
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        sync_events_left(&mut st, owner, dst);
    });
    0
}

/// `SYNCOBJ_EVENTFD`: a sync object's arrival, as an increment on a counter.
///
/// This is the request an event loop uses, and the one Smithay probes before it
/// will expose `wp_linux_drm_syncobj_v1` at all - a compositor that has to wait
/// for a client's timeline cannot afford a thread per wait. Each call registers
/// *one* increment: two registrations and one signal read back as 2, and a
/// registration whose point is already covered fires before the request returns.
pub(crate) fn syncobj_eventfd(owner: u64, arg: usize) -> isize {
    // struct drm_syncobj_eventfd: handle@0, flags@4, point@8, fd@16, pad@20.
    let handle = unsafe { (arg as *const u32).read_unaligned() };
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let point = unsafe { ((arg + 8) as *const u64).read_unaligned() };
    let fd = unsafe { ((arg + 16) as *const i32).read_unaligned() };
    // `WAIT_AVAILABLE` is meaningful here; the rest of the wait flags are not,
    // and the measured card refuses anything else with `EINVAL`.
    if flags & !SYNCOBJ_WAIT_AVAILABLE != 0 {
        return libc_einval();
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(_) => return -9, // EBADF
    };
    // The node, not the number: the increment this registration may owe later
    // has to reach a counter that is still there, which is the reference
    // Linux's `eventfd_ctx_fdget` takes for the same reason.
    let counter = file.inode();
    if counter.as_eventfd().is_none() {
        return libc_einval(); // not a counter anything could increment
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        let Some(rec) = st
            .syncobjs
            .iter_mut()
            .flatten()
            .find(|rec| rec.owner == owner && rec.handle == handle)
        else {
            return -2; // ENOENT
        };
        rec.events.push(SyncEvent {
            counter,
            point,
            available: flags & SYNCOBJ_WAIT_AVAILABLE != 0,
            fired: false,
        });
        // A point that is already covered, or a fence already behind it, owes
        // its increment now rather than at the next signal.
        fire_sync_events(rec);
        0
    })
}

/// Fire the registrations a request may have made due, by handle.
fn sync_events_left(st: &mut DrmState, owner: u64, handle: u32) {
    for rec in st.syncobjs.iter_mut().flatten() {
        if rec.owner == owner && rec.handle == handle {
            fire_sync_events(rec);
        }
    }
}
