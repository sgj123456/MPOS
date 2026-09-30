//! DRM sync objects: the state machine every `SYNCOBJ_*` request is decided by.
//!
//! `src/drivers/drm.rs` has no `cfg` gates and is never compiled for the host, so
//! anything written there is pinned only by booting a machine. Most of the card's
//! work there is bookkeeping - id tables, property values, two-call list shapes -
//! and booting is the right instrument for it. The sync objects are not that: a
//! request's *answer* here is a decision about three fields, and the decisions
//! are exactly where the authorities disagree. The upstream sources treat a sync
//! object with no fence as already signaled; the real card refuses it. `PLAN.md`
//! §7.4 records which of the two was measured, and the rules encoded below are
//! that measurement, line for line.
//!
//! So the state and the questions asked of it live here, following `drm_blob.rs`,
//! `drm_cursor.rs`, `evdev.rs` and `uinput.rs`: pure logic in an ungated leaf
//! module with `cargo test` beside it, called from the driver that cannot be
//! tested that way.
//!
//! **What a sync object on this machine can be is narrow, and the narrowness is a
//! fact rather than a simplification.** A fence here is present or absent, and a
//! fence that is present has retired: there is no engine on this card that could
//! leave one in flight, which is the same absence `WAIT_VBLANK` and the CRTC
//! sequence pair are refused on. So the question the family actually branches on
//! is *"is there a fence yet"* - and `WAIT_FOR_SUBMIT` is the sleep that waits for
//! one to arrive from another task, which is a real wait this kernel can really
//! end, not a placeholder for one.
//!
//! The four rules that are this card's rather than upstream's:
//!
//! 1. **An object with no fence is refused, not treated as ready.** `WAIT` says
//!    `EINVAL` where the sources would say "signaled", and `EXPORT_SYNC_FILE` has
//!    nothing to export.
//! 2. **A fence's presence satisfies `WAIT_FOR_SUBMIT` whatever point was asked
//!    for.** The measured card answers `ok` to a `TIMELINE_WAIT` on a point it has
//!    never reached, as soon as some fence is behind the object.
//! 3. **A timeline is not a kind chosen at creation.** `CREATE` has no flag for
//!    it; an object becomes a timeline when a point is submitted to it, which is
//!    why `TIMELINE_SIGNAL` on a plain object is `ok` and why `TIMELINE_WAIT` on
//!    one that never had a point is `EINVAL`.
//! 4. **The point only moves forward, and only `RESET` takes it back** - to zero,
//!    along with the fence. A point that could go backwards would let a fresh
//!    wait on a point already passed fail again, which is the property a timeline
//!    exists to deny.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

/// One `dma_fence`, as far as this card can have one.
///
/// A fence is a thing that has either retired or not, and a program can wait on
/// it. Everything this machine makes retires before anyone can look at it, so
/// [`Fence::retired`] is the constructor nearly every caller wants and the
/// `armed`/`retire` pair exists because the *shape* is what a `sync_file`
/// descriptor is a handle on: the commit path builds one armed and retires it
/// when the frame has gone, so that the transition is real code rather than a
/// constant that happens to be true.
pub struct Fence {
    retired: AtomicBool,
}

impl Fence {
    /// A fence that is already through - what every fence a client can obtain
    /// from this card is, because there is no engine to be in flight.
    pub fn retired() -> Arc<Fence> {
        Arc::new(Fence {
            retired: AtomicBool::new(true),
        })
    }

    /// A fence that has not retired yet. The only caller is the out-fence a
    /// commit hands back: it is armed before the flush and retired after it.
    pub fn armed() -> Arc<Fence> {
        Arc::new(Fence {
            retired: AtomicBool::new(false),
        })
    }

    /// The fence is through: whoever waits on it now waits no longer.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    /// Whether this fence is through - what a `sync_file` descriptor's
    /// readability is, and the condition a wait on one is parked against.
    pub fn has_retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }
}

/// What a sync object holds. Three fields, and the whole family is decided by
/// them; `fence` being `None` is the state every refusal is about.
#[derive(Default)]
struct Inner {
    /// The fence behind this object, if there is one. The upstream sources read
    /// `None` as "nothing to wait for, so signaled"; this card reads it as
    /// "nothing has been submitted", which is a refusal rather than an answer.
    fence: Option<Arc<Fence>>,
    /// Whether a point has ever been submitted to this object. Set by the first
    /// `TIMELINE_SIGNAL`, which is what `TIMELINE_WAIT` and `QUERY` are asking
    /// about; `CREATE` has no flag that could set it.
    timeline: bool,
    /// The highest point submitted. Zero for an object that never had one, which
    /// is also what a timeline `RESET` goes back to.
    point: u64,
}

/// One sync object: the card's side of a `drm_syncobj` handle.
pub struct SyncObj {
    inner: Mutex<RefCell<Inner>>,
    /// Tasks parked in a `WAIT_FOR_SUBMIT`. A sync object's readiness changes
    /// when *another task submits a fence to it*, so a waiter has to be named and
    /// woken - the same shape `vfs::eventfd`'s counter has, for the same reason.
    waiters: Mutex<RefCell<Vec<crate::sched::TaskId>>>,
}

impl SyncObj {
    /// A fresh object. `signaled` is `DRM_SYNCOBJ_CREATE_SIGNALED`: a client that
    /// asked for it gets a fence that has already retired, and one that did not
    /// gets an object every request about a fence will refuse.
    pub fn new(signaled: bool) -> Arc<SyncObj> {
        Arc::new(SyncObj {
            inner: Mutex::new(RefCell::new(Inner {
                fence: signaled.then(Fence::retired),
                timeline: false,
                point: 0,
            })),
            waiters: Mutex::new(RefCell::new(Vec::new())),
        })
    }

    /// Whether a fence is behind this object - the question every request with
    /// nothing else to go on is really asking, and the one the measured card
    /// refuses on.
    pub fn has_fence(&self) -> bool {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().fence.is_some())
    }

    /// Whether a point has ever been submitted, which is what makes this object a
    /// timeline.
    pub fn is_timeline(&self) -> bool {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().timeline)
    }

    /// The highest point submitted - what `QUERY` reports, and 0 for an object
    /// that never had one.
    pub fn point(&self) -> u64 {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().point)
    }

    /// The fence itself, for the `sync_file` descriptor an export builds over it.
    pub fn fence(&self) -> Option<Arc<Fence>> {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().fence.clone())
    }

    /// Whether this object would satisfy a wait, asked in the shape the request
    /// asked it.
    ///
    /// `point` is `None` for a `WAIT` on a plain object and `Some(p)` for a
    /// `TIMELINE_WAIT`; `for_submit` is `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT`.
    /// The order of the two tests is the measured one: a fence's *presence*
    /// settles a `WAIT_FOR_SUBMIT` before the point is looked at at all, which is
    /// why the card answers `ok` to a point it has never reached as soon as
    /// something is behind the object.
    pub fn ready(&self, point: Option<u64>, for_submit: bool) -> bool {
        critical_section::with(|cs| {
            let inner = self.inner.borrow(cs).borrow();
            if inner.fence.is_none() {
                return false;
            }
            if for_submit {
                return true;
            }
            match point {
                None => true,
                Some(p) => inner.timeline && p <= inner.point,
            }
        })
    }

    /// Submit a fence to this object, and wake whoever was waiting for one.
    ///
    /// This is the one write `SIGNAL`, `TIMELINE_SIGNAL`, `FD_TO_HANDLE` with
    /// `IMPORT_SYNC_FILE` and `TRANSFER` all end in, which is why they cannot
    /// disagree about what "submitting a fence" means. `point` is `Some` for the
    /// two that carry one; it makes the object a timeline and moves the
    /// high-water mark forward, never back.
    pub fn submit(&self, fence: Arc<Fence>, point: Option<u64>) {
        critical_section::with(|cs| {
            self.inner.borrow(cs).borrow_mut().fence = Some(fence);
        });
        match point {
            Some(p) => self.submit_point(p),
            None => self.wake_waiters(),
        }
    }

    /// Move the point forward without touching the fence.
    ///
    /// One caller: a `TRANSFER` whose source holds nothing to carry. The
    /// destination keeps its own fence - which is the fence that made the request
    /// legal in the first place - and learns the point it was told about.
    pub fn submit_point(&self, point: u64) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            inner.timeline = true;
            if point > inner.point {
                inner.point = point;
            }
        });
        self.wake_waiters();
    }

    /// `SYNCOBJ_RESET`: nothing is behind this object any more.
    ///
    /// The point goes back to zero with the fence, and `timeline` is deliberately
    /// *not* cleared: a client that reset its timeline still has a timeline, and
    /// one that found `TIMELINE_WAIT` refused afterwards would be a caller stuck
    /// on a wrong shape rather than one told to try again - the unrecoverable
    /// direction `AGENTS.md` §4.2 is about.
    pub fn reset(&self) {
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow(cs).borrow_mut();
            inner.fence = None;
            inner.point = 0;
        });
    }

    /// Register a task to be woken when a fence is submitted here.
    pub fn subscribe(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&id) {
                waiters.push(id);
            }
        });
    }

    pub fn unsubscribe(&self, id: crate::sched::TaskId) {
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
    }

    /// Wake every parked waiter. On the host there is no scheduler and nothing
    /// asleep, so the list is empty and the loop is not there at all.
    fn wake_waiters(&self) {
        #[cfg(target_os = "none")]
        {
            let waiters = critical_section::with(|cs| self.waiters.borrow(cs).borrow().clone());
            for id in waiters {
                crate::sched::wake(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_object_with_no_fence_is_refused_rather_than_ready() {
        // The rule the whole family turns on, and the one the upstream sources
        // disagree about: `CREATE` without `SIGNALED` leaves nothing behind, and
        // every question about a fence then has no answer to give.
        let obj = SyncObj::new(false);
        assert!(!obj.has_fence());
        assert!(!obj.ready(None, false), "a plain WAIT");
        assert!(!obj.ready(Some(0), false), "a TIMELINE_WAIT at point 0");
        assert!(
            !obj.ready(None, true),
            "even WAIT_FOR_SUBMIT, until one arrives"
        );
        assert!(
            obj.fence().is_none(),
            "and EXPORT_SYNC_FILE has nothing to export"
        );
    }

    #[test]
    fn a_created_signaled_object_is_ready_for_both_kinds_of_wait() {
        let obj = SyncObj::new(true);
        assert!(obj.has_fence());
        assert!(obj.ready(None, false));
        // Point 0 is covered by a fence that exists, on an object that is not yet
        // a timeline: a plain object answers a `WAIT` and not a point.
        assert!(
            !obj.ready(Some(0), false),
            "not a timeline, so no point is covered"
        );
        assert!(obj.ready(None, true));
    }

    #[test]
    fn a_point_is_a_high_water_mark() {
        let obj = SyncObj::new(false);
        obj.submit(Fence::retired(), Some(4));
        assert!(obj.is_timeline());
        assert_eq!(obj.point(), 4);
        assert!(obj.ready(Some(4), false));
        assert!(
            obj.ready(Some(1), false),
            "anything below the mark is covered"
        );
        assert!(!obj.ready(Some(5), false), "and anything above it is not");

        // Backwards and equal are accepted and move nothing: a point that went
        // back would let a wait on a point already passed fail again.
        obj.submit(Fence::retired(), Some(2));
        assert_eq!(obj.point(), 4);
        obj.submit(Fence::retired(), Some(4));
        assert_eq!(obj.point(), 4);
        obj.submit(Fence::retired(), Some(9));
        assert_eq!(obj.point(), 9);
    }

    #[test]
    fn a_fence_settles_a_wait_for_submit_whatever_point_was_asked_for() {
        // Measured on the real card: a `TIMELINE_WAIT` for a point it has never
        // reached answers `ok` once something is behind the object.
        let obj = SyncObj::new(true);
        obj.submit(Fence::retired(), Some(2));
        assert!(!obj.ready(Some(9), false), "the point is not covered");
        assert!(obj.ready(Some(9), true), "but the fence is there");
    }

    #[test]
    fn reset_takes_the_fence_and_the_point_away_and_leaves_a_timeline() {
        let obj = SyncObj::new(false);
        obj.submit(Fence::retired(), Some(7));
        assert!(obj.ready(Some(7), false));

        obj.reset();
        assert!(!obj.has_fence());
        assert_eq!(obj.point(), 0);
        assert!(!obj.ready(None, false), "nothing is behind it");
        assert!(!obj.ready(Some(0), false));
        assert!(obj.is_timeline(), "but it is still a timeline");

        // And a second fence re-arms it, which is the property the point going
        // backwards would have destroyed.
        obj.submit(Fence::retired(), Some(7));
        assert!(obj.ready(Some(7), false));
    }

    #[test]
    fn submitting_a_plain_fence_does_not_make_an_object_a_timeline() {
        // `SIGNAL` leaves a fence behind and no point: the measured card reads
        // its point back as 0 afterwards.
        let obj = SyncObj::new(false);
        obj.submit(Fence::retired(), None);
        assert!(obj.has_fence());
        assert!(!obj.is_timeline());
        assert_eq!(obj.point(), 0);
        assert!(obj.ready(None, false), "a plain WAIT is satisfied");
        assert!(
            !obj.ready(Some(0), false),
            "and a point is not covered by it"
        );
    }

    #[test]
    fn a_transfer_carries_the_fence_and_moves_the_destinations_point() {
        let src = SyncObj::new(false);
        src.submit(Fence::retired(), Some(6));
        let dst = SyncObj::new(false);

        // What `TRANSFER` does at the destination: the fence, and the point it
        // was told - which is what makes a plain object a timeline.
        dst.submit(src.fence().expect("the source holds one"), Some(3));
        assert!(dst.has_fence());
        assert!(dst.is_timeline());
        assert_eq!(dst.point(), 3);
        assert!(dst.ready(Some(3), false));
        assert!(!dst.ready(Some(4), false));
    }

    #[test]
    fn a_fence_is_through_once_it_has_retired() {
        // What a `sync_file` descriptor's readability reads, and the one piece of
        // the commit path that is a transition rather than a constant.
        let fence = Fence::armed();
        assert!(!fence.has_retired());
        fence.retire();
        assert!(fence.has_retired());
        fence.retire();
        assert!(fence.has_retired(), "retiring twice is retiring once");

        assert!(Fence::retired().has_retired());
    }

    #[test]
    fn a_waiter_is_registered_once_and_can_leave() {
        let obj = SyncObj::new(false);
        let me = crate::sched::TaskId(7);
        obj.subscribe(me);
        obj.subscribe(me);
        let count = critical_section::with(|cs| obj.waiters.borrow(cs).borrow().len());
        assert_eq!(count, 1, "a task that waits twice is one waiter");
        obj.unsubscribe(me);
        let count = critical_section::with(|cs| obj.waiters.borrow(cs).borrow().len());
        assert_eq!(count, 0);
        // Submitting with nobody waiting is the ordinary case, not a special one.
        obj.submit(Fence::retired(), None);
        assert!(obj.ready(None, false));
    }
}
