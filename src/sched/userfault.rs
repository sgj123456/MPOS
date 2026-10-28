//! `userfaultfd`'s machinery: the registered ranges, the queue of pagefaults
//! waiting to be answered, and the parking the fault path does when a page
//! belongs to one of them.
//!
//! It lives in `sched` and not in `vfs` because the fault path does
//! (`docs/ARCHITECTURE.md` §3: a module calls downward only, and
//! [`crate::sched::runtime::child_mm::populate_in`] is where a lazily mapped
//! page is created). The descriptor's `Inode` face is
//! [`crate::vfs::userfaultfd::UserfaultFd`], which owns one of these and
//! forwards to it - the same split `sysv_msg` takes between its queue and its
//! syscall layer.
//!
//! The shape, the served subset and every refusal are measured on Linux 7.2.11
//! as real root; the record is `docs/notes/06-process-management.md` §6.33 and
//! the numbers are in [`crate::vfs::userfaultfd_abi`].

use super::policy::TaskId;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};
use critical_section::Mutex;

/// The most pagefaults one descriptor may have outstanding. A record is an
/// allocation a user steers and the global allocator still panics on failure
/// (`docs/notes/` §8.0), so the count carries a bound.
pub const MAX_PENDING: usize = 4096;
/// The most live descriptors the machine may hold.
pub const MAX_DESCRIPTORS: usize = 64;
/// The most ranges one descriptor may have registered.
pub const MAX_RANGES: usize = 256;

/// One registered span of pages.
#[derive(Clone, Copy)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

impl Range {
    fn covers(&self, page: usize) -> bool {
        page >= self.start && page < self.end
    }
}

/// One pagefault waiting to be answered.
#[derive(Clone, Copy)]
pub struct Pending {
    pub page: usize,
    pub write: bool,
    /// The task parked on it. One record per page: a second thread faulting the
    /// same page joins the same wait (measured - one record, one `UFFDIO_COPY`
    /// releases both).
    pub task: TaskId,
}

struct State {
    /// The `UFFDIO_API` handshake happened. Linux answers a second one with
    /// `EINVAL`, and a fault is not delivered before it.
    api_done: bool,
    ranges: Vec<Range>,
    pending: Vec<Pending>,
    /// Tasks parked **on a page**, waiting for its answer. This is deliberately
    /// not `pending`: `read(2)` removes a record from the queue when it hands it
    /// to the program, and the faulting task is still waiting afterwards - the
    /// queue is the program's mail, this list is who to wake when the page
    /// arrives. Losing that distinction parks the faulting thread forever.
    parked: Vec<Pending>,
    /// Tasks parked in `read(2)` - the list a delivered record wakes.
    readers: Vec<TaskId>,
}

/// One `userfaultfd` descriptor's state.
pub struct UserfaultObject {
    id: u64,
    nonblocking: bool,
    state: Mutex<RefCell<State>>,
}

/// Live descriptors, so the fault path can ask "is this page registered
/// anywhere" without a descriptor in hand.
static LIVE: Mutex<RefCell<Vec<Arc<UserfaultObject>>>> = Mutex::new(RefCell::new(Vec::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl UserfaultObject {
    /// A fresh descriptor, or `None` when the machine holds
    /// [`MAX_DESCRIPTORS`] already.
    pub fn create(nonblocking: bool) -> Option<Arc<UserfaultObject>> {
        let mut made = None;
        critical_section::with(|cs| {
            let mut live = LIVE.borrow(cs).borrow_mut();
            if live.len() >= MAX_DESCRIPTORS {
                return;
            }
            let object = Arc::new(UserfaultObject {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                nonblocking,
                state: Mutex::new(RefCell::new(State {
                    api_done: false,
                    ranges: Vec::new(),
                    pending: Vec::new(),
                    parked: Vec::new(),
                    readers: Vec::new(),
                })),
            });
            live.push(Arc::clone(&object));
            made = Some(object);
        });
        made
    }

    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }

    /// The descriptor's identity, for the `Inode` face's `ino`.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Record the handshake, answering `false` for a second one - the `EINVAL`
    /// the caller answers. The shape checks themselves live in the ABI half.
    pub fn mark_api_done(&self) -> bool {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.api_done {
                return false;
            }
            st.api_done = true;
            true
        })
    }

    pub fn api_done(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().api_done)
    }

    /// Add a registered span. A repeat of the same span is a no-op (measured:
    /// registering the same range twice answers 0).
    pub fn add_range(&self, start: usize, end: usize) -> bool {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.ranges.iter().any(|r| r.start == start && r.end == end) {
                return true;
            }
            if st.ranges.len() >= MAX_RANGES {
                return false;
            }
            st.ranges.push(Range { start, end });
            true
        })
    }

    /// Drop a registered span. Linux answers 0 even for a span it does not know
    /// (measured).
    pub fn remove_range(&self, start: usize, end: usize) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .ranges
                .retain(|r| !(r.start == start && r.end == end));
        })
    }

    /// Whether `start..end` lies inside one registered span - what `UFFDIO_COPY`
    /// and `UFFDIO_ZEROPAGE` check before touching a page (`ENOENT` otherwise,
    /// measured).
    pub fn covers_span(&self, start: usize, end: usize) -> bool {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .ranges
                .iter()
                .any(|r| r.start <= start && end <= r.end)
        })
    }

    /// Whether a pagefault on `page` is this descriptor's to deliver.
    pub fn covers_page(&self, page: usize) -> bool {
        critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            st.api_done && st.ranges.iter().any(|r| r.covers(page))
        })
    }

    /// Whether a record is waiting to be read.
    pub fn has_pending(&self) -> bool {
        critical_section::with(|cs| !self.state.borrow(cs).borrow().pending.is_empty())
    }

    /// The pagefaults outstanding, oldest first.
    pub fn pending(&self) -> Vec<Pending> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().pending.clone())
    }

    /// Take the oldest record's `(page, write)`, clearing its place in the
    /// queue. `None` when the queue is empty.
    pub fn take_pending(&self) -> Option<Pending> {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if st.pending.is_empty() {
                return None;
            }
            Some(st.pending.remove(0))
        })
    }

    /// Queue a fault on `page` for the parked `task`. Answers `false` when the
    /// page is not this descriptor's business (or the queue is at its bound),
    /// which is the fault path's cue to resolve the page the ordinary way.
    ///
    /// The caller must not hold the task table - [`deliver`] is the shape that
    /// does this from the fault path.
    fn queue(&self, page: usize, write: bool, task: TaskId) -> bool {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.api_done || !st.ranges.iter().any(|r| r.covers(page)) {
                return false;
            }
            // One record per page until it is answered: a second thread on the
            // same page joins the wait rather than queueing a second record for
            // a page the program has already been told about.
            if st.pending.iter().any(|p| p.page == page) || st.parked.iter().any(|p| p.page == page)
            {
                return true;
            }
            if st.pending.len() >= MAX_PENDING {
                return false;
            }
            st.pending.push(Pending { page, write, task });
            true
        })
    }

    /// Record that `task` is waiting for `page`'s answer. Kept apart from the
    /// readable queue on purpose (see `State::parked`).
    pub fn park(&self, page: usize, task: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.parked.iter().any(|p| p.page == page && p.task == task) {
                st.parked.push(Pending {
                    page,
                    write: false,
                    task,
                });
            }
        });
    }

    /// Forget a waiting task - it woke up, or its wait was interrupted.
    pub fn unpark(&self, task: TaskId) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .parked
                .retain(|p| p.task != task);
        });
    }

    /// The tasks waiting on `page`'s answer.
    pub fn waiting_on(&self, page: usize) -> Vec<TaskId> {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .parked
                .iter()
                .filter(|p| p.page == page)
                .map(|p| p.task)
                .collect()
        })
    }

    /// The tasks parked in `read(2)`.
    pub fn readers(&self) -> Vec<TaskId> {
        critical_section::with(|cs| self.state.borrow(cs).borrow().readers.clone())
    }

    pub fn subscribe_reader(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut st = self.state.borrow(cs).borrow_mut();
            if !st.readers.contains(&id) {
                st.readers.push(id);
            }
        });
    }

    pub fn unsubscribe_reader(&self, id: TaskId) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .readers
                .retain(|&w| w != id);
        });
    }

    /// Wake the tasks parked on the pages in this span - `UFFDIO_WAKE`. A span
    /// with nothing parked on it is 0, as measured.
    pub fn wake_span(&self, start: usize, end: usize) {
        let tasks = critical_section::with(|cs| {
            let st = self.state.borrow(cs).borrow();
            st.parked
                .iter()
                .filter(|p| p.page >= start && p.page < end)
                .map(|p| p.task)
                .collect::<Vec<TaskId>>()
        });
        for task in tasks {
            super::wake(task);
        }
    }
}

impl Drop for UserfaultObject {
    fn drop(&mut self) {
        critical_section::with(|cs| {
            LIVE.borrow(cs).borrow_mut().retain(|o| o.id != self.id);
        });
    }
}

/// Whether any live descriptor has `page` registered - asked by the fault path
/// before it allocates.
pub fn registered_page(page: usize) -> bool {
    critical_section::with(|cs| LIVE.borrow(cs).borrow().iter().any(|o| o.covers_page(page)))
}

/// Deliver a pagefault on `page` to whichever descriptor registered it, and park
/// the current task until the answer arrives.
///
/// This is the fault path's whole involvement: `true` means "the fault is
/// somebody else's to answer, do not allocate a frame"; `false` means no
/// descriptor has this page, so the caller resolves it the ordinary way.
///
/// **Must not be called with the task table borrowed** - it parks, and parking
/// switches tasks. [`crate::sched::runtime::child_mm::populate_in`] takes the
/// table for its own work, so the delivery is done by its caller
/// ([`crate::sched::runtime::child_mm::resolve_anon_fault`]) on the way in.
pub fn deliver(page: usize, write: bool) -> bool {
    let me = super::current();
    // Which descriptor, if any, owns this page. Cloned out of the registry so
    // the borrow is not held across the park.
    let owner = critical_section::with(|cs| {
        LIVE.borrow(cs)
            .borrow()
            .iter()
            .find(|o| o.covers_page(page))
            .cloned()
    });
    let Some(object) = owner else {
        return false;
    };
    if !object.queue(page, write, me) {
        return false;
    }
    // The record is queued; wake whoever is in `read(2)` and wait for the
    // answer. The answer is a page appearing where the fault was - the
    // answering thread's `UFFDIO_COPY`/`UFFDIO_ZEROPAGE` fills it in - so the
    // wait's condition is the page being present.
    object.park(page, me);
    for task in object.readers() {
        super::wake(task);
    }
    let root = crate::hal::active_root();
    let _ = super::block_until_interruptible(|| {
        crate::hal::translate(page).is_some() || super::page_present_in(root, page)
    });
    object.unpark(me);
    // A signal that interrupted the wait leaves the record in the queue; the
    // faulting instruction retries and faults again, which is the honest
    // answer - the program asked to be told about this page and has not
    // answered yet.
    true
}

/// Put `bytes` at `dst` in the current task's address space, waking whoever was
/// parked on the page - `UFFDIO_COPY`'s work.
///
/// `EEXIST` when the page is already present (measured: copying over a present
/// page is `EEXIST`, not an overwrite). With `dontwake` the page is installed
/// and the parked task stays parked, which is what `MODE_DONTWAKE` asks for.
pub fn fill_page(dst: usize, bytes: &[u8], dontwake: bool) -> Result<(), isize> {
    let page = crate::mm::pframe::align_down(dst, crate::mm::PAGE_SIZE);
    // A page that is already there is EEXIST. The question is the address
    // space's, asked through the walk the fault path uses.
    if super::user_frame_at(page).is_some() {
        return Err(crate::err::EEXIST);
    }
    // A frame of the process's own, filled from the caller's buffer.
    let Some(frame) = crate::mm::alloc_frames(0) else {
        return Err(crate::err::ENOMEM);
    };
    unsafe {
        core::ptr::write_bytes(
            crate::mm::phys_to_virt(frame) as *mut u8,
            0,
            crate::mm::PAGE_SIZE,
        );
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            crate::mm::phys_to_virt(frame) as *mut u8,
            bytes.len().min(crate::mm::PAGE_SIZE),
        );
    }
    // The VMA's own permissions, which is what the page must come up with.
    let Some(prot) = super::prot_of_page(page) else {
        crate::mm::free_frames(frame, 0);
        return Err(crate::err::EINVAL);
    };
    let mapped = super::current_map_user(page, frame, prot);
    if mapped < 0 {
        crate::mm::free_frames(frame, 0);
        return Err(mapped as isize);
    }
    if !dontwake {
        wake_page(page);
    }
    Ok(())
}

/// Put a fresh zero page at `dst` - `UFFDIO_ZEROPAGE`'s work. `EEXIST` when the
/// page is already there (measured).
pub fn fill_zero_page(dst: usize, len: usize) -> Result<(), isize> {
    let page = crate::mm::pframe::align_down(dst, crate::mm::PAGE_SIZE);
    let _ = len;
    if super::user_frame_at(page).is_some() {
        return Err(crate::err::EEXIST);
    }
    let Some(frame) = crate::mm::alloc_frames(0) else {
        return Err(crate::err::ENOMEM);
    };
    unsafe {
        core::ptr::write_bytes(
            crate::mm::phys_to_virt(frame) as *mut u8,
            0,
            crate::mm::PAGE_SIZE,
        );
    }
    let Some(prot) = super::prot_of_page(page) else {
        crate::mm::free_frames(frame, 0);
        return Err(crate::err::EINVAL);
    };
    let mapped = super::current_map_user(page, frame, prot);
    if mapped < 0 {
        crate::mm::free_frames(frame, 0);
        return Err(mapped as isize);
    }
    wake_page(page);
    Ok(())
}

/// Wake every task parked on `page` - the answer arriving is what they wait for.
pub fn wake_page(page: usize) {
    let tasks = critical_section::with(|cs| {
        LIVE.borrow(cs)
            .borrow()
            .iter()
            .flat_map(|o| o.waiting_on(page))
            .collect::<Vec<TaskId>>()
    });
    for task in tasks {
        super::wake(task);
    }
}
