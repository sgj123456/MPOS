use super::PERMANENT_EXTRA;

#[cfg(feature = "selftest")]
use super::PERMANENT_TASKS;
use super::load::spawn;
use super::task::with_table;
use super::{USER_STACK_MAX, USER_STACK_PAGES};
use crate::mm::vma::{ImageBounds, MappedRange, Vma, VmaKind, VmaTable, merge_ranges};
use core::cell::RefCell;
use critical_section::Mutex;

use super::task::{ChildKind, STACK_GROWTH_SLACK, Table, WaitFor, current};
use crate::hal;
use crate::mm::vm::{MapFlags, Mapping};
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{Priority, TaskId};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

/// Map a physical frame at `virt` in the current task's address space with
/// the given `flags`.  Returns the virtual address on success, or a negative
/// errno.
///
/// A page that is already mapped answers `-EEXIST` rather than taking the
/// kernel down: the caller is a syscall, and a libc asking for an address it
/// already holds is a request to answer, not a reason to panic.
pub fn current_map_user(virt: usize, frame: usize, flags: crate::mm::vm::MapFlags) -> isize {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        let mapped = match task.space.as_mut() {
            Some(space) => space.try_map(crate::mm::vm::Mapping {
                virt,
                phys: frame,
                length: crate::mm::pframe::PAGE_SIZE,
                flags,
            }),
            None => return -crate::err::EINVAL,
        };
        match mapped {
            Ok(()) => {
                // The frame now belongs to the process: the reaper returns it
                // once the last thread is gone, not before.
                task.mm.own(frame);
                virt as isize
            }
            Err(crate::hal::PageTaken) => -crate::err::EEXIST,
        }
    })
}

/// Put a frame the task *already owns* at `virt`, adding nothing to the list
/// of frames the reaper must return. This is how `mremap` moves a mapping: the
/// pages change address, their ownership does not change hands.
pub fn move_user_page(virt: usize, frame: usize, flags: crate::mm::vm::MapFlags) -> bool {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        match task.space.as_mut() {
            Some(space) => space
                .try_map(crate::mm::vm::Mapping {
                    virt,
                    phys: frame,
                    length: crate::mm::pframe::PAGE_SIZE,
                    flags,
                })
                .is_ok(),
            None => false,
        }
    })
}

/// The frame behind `virt` in the current task's address space, if it has one.
pub fn user_frame_at(virt: usize) -> Option<usize> {
    with_table(|t| {
        // The *group's* space, as every other mapping question asks: a thread
        // owns no address space of its own, so a thread's slot holds `None` and
        // asking it answers "nothing is mapped" for every address the thread
        // names - which is the wrong answer for all four callers. The sharpest
        // of them is `futex`: it reads the word it is about to sleep on, so
        // every thread's `FUTEX_WAIT` would see a word that is always zero and
        // answer `EAGAIN`, and musl's `__timedwait_cp` retries on `EAGAIN` in a
        // loop. A condvar wait inside a thread became a spin.
        let owner = t.space_owner(current());
        t.task(owner).space.as_ref()?.phys_for_virt(virt)
    })
}

/// How many pages of `start..end` have a frame behind them in the address space
/// of the task `pid` names - the per-mapping `Rss` `/proc/<pid>/smaps` reports.
///
/// The same page-table question [`user_frame_at`] asks, asked of a *named* task
/// rather than of the caller. It is asked one page at a time for the reason
/// `/proc/<pid>/smaps` cannot ask it any other way: this kernel's resident set
/// is the page tables (`Mm.frames` holds the frames, not the addresses they are
/// mapped at), so there is no structure to walk, and the answer is
/// `phys_for_virt` per page - which is exactly what `mincore` does for one
/// range. The table's critical section is therefore *not* held across the walk:
/// a process with a gigabyte mapped makes this 262144 pages, and interrupts
/// must not be off for that (AGENTS.md §3's rule 5, the bound on work a user
/// can steer).
///
/// The walk starts at the page `start` falls in, which is the page the range's
/// own first byte would fault: `mmap` returns page-aligned addresses, so the
/// rounding can only ever matter for a range that is not.
pub fn resident_pages(pid: usize, start: usize, end: usize) -> usize {
    let mut count = 0;
    let mut page = start & !(PAGE_SIZE - 1);
    while page < end && page >= start {
        if user_frame_at_task(pid, page).is_some() {
            count += 1;
        }
        // The wraparound guard is the loop's, not a caller's: a range ending at
        // the top of the address space would otherwise step past it.
        match page.checked_add(PAGE_SIZE) {
            Some(next) => page = next,
            None => break,
        }
    }
    count
}

/// [`user_frame_at`] for a task named by its id rather than by being current,
/// with the group's space resolved the same way: a thread owns no address space
/// of its own, so asking the thread's own slot would answer "nothing is mapped"
/// for every page of a process that has one.
///
/// The slot is checked before the leader is asked for its space, because the
/// reader is a *file*: `/proc/<pid>/smaps` of a process that exits between two
/// of its pages is a reading that has to stop answering rather than a reason to
/// panic (`Table::task` panics on a slot nothing holds, which is right for a
/// caller that holds the same lock it named the task under).
fn user_frame_at_task(pid: usize, virt: usize) -> Option<usize> {
    with_table(|t| {
        t.tasks.get(pid)?.as_ref()?;
        let owner = t.space_owner(TaskId(pid));
        t.task(owner).space.as_ref()?.phys_for_virt(virt)
    })
}

/// Whether no VMA describes any part of `virt..virt + len`.
///
/// This asks the question a range claim needs: that nothing belongs to the
/// process inside it - not even a lazily-mapped hole whose pages have never
/// arrived. Linux's `get_unmapped_area` checks the descriptions rather than
/// the frames, and both `mmap`'s plain hint and `brk`'s grow are gated on
/// this (measured, `NOTES.md` §15.18).
///
/// The answer comes from [`Mm::vma_range_free`], which is the same predicate
/// over a table the caller already holds - the `brk` growth loop asks it that
/// way, because `current_mm()` from inside a `with_table` closure would take
/// the table twice.
pub fn vma_range_free(virt: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    current_mm().vma_range_free(virt, virt.saturating_add(len))
}

/// Whether every page of `virt..virt + len` has no *frame* behind it yet.
///
/// The frame-wise counterpart of [`vma_range_free`], and deliberately not the
/// question any *placement* decision asks: a lazily-mapped VMA with no pages
/// yet is still somebody's address, and placing a second description over it
/// is the shape `NOTES.md` §15.18 refused. It is kept as the mm seam's
/// page-level question - a range whose description exists and whose frames do
/// not is exactly what "lazily mapped" means, and it is what a reader that
/// wants to know whether a hint is *busy* in the old frame-backed sense asks.
/// Nothing in the kernel gates a placement on it any more: measured against
/// real Linux, a hint over a lazy span and a hint over a busy one get the same
/// answer (relocated), so the distinction has no caller to serve.
pub fn frames_range_free(virt: usize, len: usize) -> bool {
    (0..len)
        .step_by(crate::mm::pframe::PAGE_SIZE)
        .all(|offset| user_frame_at(virt + offset).is_none())
}

/// Take the mappings covering `virt..virt + len` away from the current task,
/// handing back the frames they pointed at.
pub fn unmap_user(virt: usize, len: usize) -> alloc::vec::Vec<usize> {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        match task.space.as_mut() {
            Some(space) => space.unmap(virt, len),
            None => alloc::vec::Vec::new(),
        }
    })
}

/// Change the permission bits of the pages the current task already has mapped
/// in `virt..virt + len`, leaving their frames alone. Pages that are not mapped
/// are skipped, so this cannot fail the way Linux's `mprotect` fails on a hole.
pub fn protect_user(virt: usize, len: usize, flags: crate::mm::vm::MapFlags) {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        if let Some(space) = task.space.as_mut() {
            space.protect(virt, len, flags);
        }
    })
}

/// Give back frames an unmapping dropped. Nothing can reach them any more, so
/// the allocator can have them now - but only after they are off the task's own
/// lists, or its exit would free them a second time.
pub fn release_user_frames(frames: alloc::vec::Vec<usize>) {
    if frames.is_empty() {
        return;
    }
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        for frame in frames {
            task.mm.disown(frame);
            crate::mm::free_frames(frame, 0);
        }
    });
}

// --- Virtual memory areas, and the demand paging they make possible ---------
//
// A mapping is an *address space*: `mmap` records a VMA and stops, and the page
// behind an address appears the first time somebody touches it. Both directions
// that can touch it go through [`populate_in`] - a real `#PF` from ring 3, and
// a kernel copy that walks the page tables and finds nothing - because the two
// must give the same answer, and a page that exists for one of them and not the
// other is a bug with two faces.

/// The process whose address space is rooted at `root`, if any.
///
/// A walk of the task table rather than a lookup, and it is deliberate: the
/// caller has a CR3 value, not a task id, because the two places that need this
/// (the fault handler, and the clear-tid store the scheduler makes on a dying
/// thread's behalf) run in contexts where "the current task" is exactly the
/// thing that is not known. A thread owns no space of its own, so the answer is
/// the group leader.
fn owner_of_root(t: &Table, root: usize) -> Option<TaskId> {
    t.tasks.iter().enumerate().find_map(|(slot, task)| {
        let space = task.as_ref()?.space.as_ref()?;
        (space.root() == root).then_some(TaskId(slot))
    })
}

/// Bring in the page `virt` names, in the address space `root` names.
///
/// This is the one place a lazily-mapped page is created. It looks the VMA up,
/// checks the access against what the range permits, takes a frame - fresh and
/// zeroed for anonymous memory, the backing's own page for a frame-backed
/// mapping - maps it with the range's permissions, and records it as a frame
/// the group owns so the reaper returns it.
///
/// `false` means the address is not the process's to touch: no VMA covers it, or
/// the range does not permit the access. Every caller turns that into the
/// refusal it already had - `SIGSEGV` from the fault handler, `EFAULT` from a
/// kernel copy - so a page that is merely *absent* is distinguishable from one
/// that is not there at all, and only the former is ever created.
///
/// The third answer is the machine's, not the address's: an anonymous page this
/// process may touch and no frame left to bring it in with. Linux's measured
/// answer to that is the OOM killer, `SIGKILL` to the process that asked (§8.0),
/// and it is a *different* answer from the refusal for the reason the fault
/// handler needs it to be: a process that used the machine up must not look like
/// one that dereferenced a wild pointer. The frame it asks for comes from
/// [`mm::alloc_user_frame`], which keeps the kernel's own reserve back so the
/// death it triggers can run.
pub(crate) fn populate_in(
    t: &mut Table,
    root: usize,
    virt: usize,
    write: bool,
) -> crate::hal::kernel::FaultAnswer {
    use crate::hal::kernel::FaultAnswer;
    let page = pframe::align_down(virt, PAGE_SIZE);
    let Some(owner) = owner_of_root(t, root) else {
        return FaultAnswer::Refused;
    };
    // Normalised to the group: a thread's slot holds no space, and the frames
    // belong to the group either way.
    let owner = t.space_owner(owner);
    let Some(task) = t.tasks[owner.0].as_mut() else {
        return FaultAnswer::Refused;
    };
    let mm = Arc::clone(&task.mm);
    let Some(space) = task.space.as_mut() else {
        return FaultAnswer::Refused;
    };
    let Some(vma) = mm.vma_at(page) else {
        return FaultAnswer::Refused;
    };
    if !vma.permits(write) {
        return FaultAnswer::Refused;
    }
    let frame = match &vma.kind {
        VmaKind::Anon => {
            let Some(frame) = mm::alloc_user_frame() else {
                return FaultAnswer::OutOfMemory;
            };
            // A page nobody has written reads as zeros - it is a hole in the
            // program's memory, not whatever the frame's last owner left there.
            unsafe { core::ptr::write_bytes(mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
            frame
        }
        VmaKind::Frames { object, offset, .. } => {
            // The page of the *object* this address corresponds to. The offset
            // travels with the address through every split (see `mm::vma`), so
            // this is the same page the eager loop used to map.
            let index = ((offset + (page - vma.start)) / PAGE_SIZE) as u64;
            match object.frame_at(index) {
                Some(frame) => frame,
                None => return FaultAnswer::Refused,
            }
        }
    };
    let mapped = space
        .try_map(Mapping {
            virt: page,
            phys: frame,
            length: PAGE_SIZE,
            flags: vma.prot,
        })
        .is_ok();
    if !mapped {
        // Somebody is already there. On one core that cannot happen between the
        // miss and here, but a `PROT_NONE` reserve that was never populated can
        // still be met, and the frame must go back rather than leak.
        //
        // The one other way to land here is the page tables' own frame
        // allocation, which comes out of [`mm::alloc_user_frame`]'s reserve and
        // so cannot have failed: the check and the map are one fault wide, and
        // nothing else in it allocates a frame (`docs/notes/` §8.0's named
        // residual keeps the interrupt-context window).
        crate::mm::free_frames(frame, 0);
        return FaultAnswer::Refused;
    }
    mm.own(frame);
    FaultAnswer::Resolved
}

/// The `#PF` entry: resolve a fault on a lazily-mapped page, the way
/// [`resolve_cow_fault`] resolves one on a page a `fork` still shares.
///
/// The address space is taken from CR3 rather than from the current task,
/// because the two can differ - the scheduler writes a dying thread's clear-tid
/// word after switching the CPU to that thread's tables - and the tables the
/// fault came through are the ones that decide what the address means.
pub fn resolve_anon_fault(virt: usize, write: bool) -> crate::hal::kernel::FaultAnswer {
    let root = hal::active_root();
    // A page a `userfaultfd` registered is not this kernel's to resolve: the
    // fault becomes a record on that descriptor and the faulting task parks
    // until the process answers with the page. This runs **before** the table
    // is borrowed because the park switches tasks, and `with_table`'s own rule
    // is that its closure must not (`docs/ARCHITECTURE.md` §7). `deliver`
    // answers `true` only when a descriptor really owns the page.
    if super::super::userfault::deliver(pframe::align_down(virt, PAGE_SIZE), write) {
        return crate::hal::kernel::FaultAnswer::Resolved;
    }
    with_table(|t| populate_in(t, root, virt, write))
}

/// [`resolve_anon_fault`] for an address space named by its root rather than by
/// whatever CR3 holds: the cross-process half, what `process_vm_readv` and a
/// tracer's `PTRACE_POKEDATA` reach a lazily-mapped page through.
///
/// The third answer is folded into the refusal here on purpose: a kernel copy
/// into another process's address space has no process to end over it, and Linux
/// answers a copy that could not get a page with `EFAULT` too - the OOM kill is
/// the *fault handler's* answer, for a process faulting on its own memory
/// (`docs/notes/` §8.0).
pub fn populate_user_page_at(root: usize, virt: usize, write: bool) -> bool {
    with_table(|t| populate_in(t, root, virt, write)) == crate::hal::kernel::FaultAnswer::Resolved
}

/// Whether the kernel may treat the *absent* page holding `virt` as one it can
/// touch, because a VMA backs it and the access is permitted.
///
/// `buffer_ok` asks this before a syscall does a raw copy, so a range the
/// process has legally mapped but not touched is not refused as `EFAULT`.
///
/// Three answers, and the difference between the last two is where the page is
/// brought in rather than whether:
///
/// - **anonymous**: admitted, and nothing is done here. Populating it costs a
///   frame and touches no filesystem, so the copy's own fault can answer it.
/// - **a frame-backed object that says `lazy_populatable`** (a private mapping
///   of a file, and nothing else): the page is brought in **now**, before the
///   caller takes any lock. This is the load-bearing case. The copy that follows
///   runs *inside* the filesystem's own borrows - `ext4`'s `write_at` holds its
///   cache's `RefCell` across the transfer - so a fault answered lazily under
///   that borrow re-enters `ext4` and dies of `RefCell already borrowed`
///   (measured on aarch64: `src/vfs/ext4/mod.rs:558`, `t-mmap-file-lazy`).
///   Before this rule the kernel answered `EFAULT` instead of dying, which is
///   the gentler half of the same defect (`docs/notes/` §10.15a).
/// - **anything else**: refused. Populating those calls into the node that owns
///   the object, and the DRM ioctl path validates a caller's array while
///   holding the card's lock, so a page that needed the card to answer a fault
///   would be a lock taken twice.
pub fn kernel_lazy_accessible(virt: usize, write: bool) -> bool {
    /// What the VMA table says about this page, decided under the borrow the
    /// populate below must not be inside.
    enum Admission {
        No,
        Already,
        Populate,
    }
    let root = hal::active_root();
    let page = pframe::align_down(virt, PAGE_SIZE);
    let admission = with_table(|t| {
        let Some(owner) = owner_of_root(t, root) else {
            return Admission::No;
        };
        let owner = t.space_owner(owner);
        match t.task(owner).mm.vma_at(page) {
            Some(vma) if !vma.permits(write) => Admission::No,
            Some(vma) => match &vma.kind {
                VmaKind::Anon => Admission::Already,
                VmaKind::Frames { object, .. } if object.lazy_populatable() => Admission::Populate,
                VmaKind::Frames { .. } => Admission::No,
            },
            None => Admission::No,
        }
    });
    match admission {
        Admission::No => false,
        Admission::Already => true,
        // The borrow above is dead before this runs, which is the whole point:
        // `populate_user_page_at` takes the table again and reads the file.
        Admission::Populate => populate_user_page_at(root, page, write),
    }
}

/// Whether `page` is present in the address space `root` names.
///
/// The question a `userfaultfd` answer turns on: the parked task waits for the
/// page to appear, and the answering thread's `UFFDIO_COPY` is what makes it
/// appear. Asked through the same walk the fault handler uses.
pub fn page_present_in(root: usize, page: usize) -> bool {
    with_table(|t| {
        let Some(owner) = owner_of_root(t, root) else {
            return false;
        };
        let owner = t.space_owner(owner);
        t.task(owner)
            .space
            .as_ref()
            .and_then(|space| space.phys_for_virt(page))
            .is_some()
    })
}

/// Whether every page of `start..end` lies in a VMA - the question
/// `UFFDIO_REGISTER` asks before it accepts a range (measured: a range with no
/// mapping behind it is `EINVAL`).
pub fn range_is_mapped(start: usize, end: usize) -> bool {
    if end <= start {
        return false;
    }
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let mm = Arc::clone(&t.task(owner).mm);
        let mut page = pframe::align_down(start, PAGE_SIZE);
        while page < end {
            if !mm.vma_covers(page) {
                return false;
            }
            page += PAGE_SIZE;
        }
        true
    })
}

/// The permissions the VMA covering `page` grants, or `None` when no VMA does.
/// `UFFDIO_COPY` installs the page with these, so a page filled into a
/// `PROT_READ` range does not arrive writable.
pub(crate) fn prot_of_page(page: usize) -> Option<MapFlags> {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        t.task(owner).mm.vma_at(page).map(|vma| vma.prot)
    })
}

/// The frame list and VMA table of the current task's group.
pub(crate) fn current_mm() -> Arc<Mm> {
    with_table(|t| Arc::clone(&t.task(t.space_owner(t.current)).mm))
}

/// Record a private anonymous mapping of `virt..virt + len`.
///
/// Nothing is allocated: the address space is the whole of the mapping, and the
/// frames arrive as the program touches them. `flags` is what each page will be
/// mapped with, and what an access is checked against.
pub fn map_anon(virt: usize, len: usize, flags: MapFlags) {
    if len == 0 {
        return;
    }
    current_mm().vma_insert(Vma::new(virt, virt + len, flags, VmaKind::Anon));
}

/// Record a mapping of `virt..virt + len` backed by `inode`'s own frames,
/// starting at page `offset` of that object. This is what `memfd`, the
/// framebuffer and a dma-buf get, and what a shared anonymous mapping is made
/// of: the pages are the object's, shared by every mapping of it.
///
/// `shared` is the `MAP_SHARED` bit the caller asked for, which the mapping
/// keeps because `/proc/<pid>/maps` reports it - a `memfd` mapped
/// `MAP_PRIVATE` is the same object and still not a shared mapping. `name` is
/// what that file prints for this line, resolved from the descriptor *now*
/// because resolving it later means walking the filesystem tree (see
/// [`crate::vfs::File::display_name`]).
/// A **private** mapping of an ordinary file, populated a page at a time.
///
/// The eager copy this replaces read the whole range inside `mmap(2)` - one
/// frame and one 4 KiB `read_at` per page - so mapping a 189 MB shared object
/// cost 3.4 s of the caller's system call (`docs/notes/` §10.15, item 176),
/// and a compositor's loader spent ~15 s inside `mmap` before it printed its
/// first line. Here the fault path calls [`MappedObject::frame_at`] for the
/// page it needs and this object reads that page from the file then: one
/// private copy per page, and `MAP_PRIVATE`'s promise (a write never reaches
/// the file) holds because every page is this process's own frame.
///
/// `MAP_SHARED` of a plain file keeps the eager copy: sharing without
/// writeback would alias a file this kernel cannot write back, and a write
/// that silently vanished into a frame is worse than a copy.
pub struct FilePages {
    inode: Arc<dyn crate::vfs::Inode>,
}

impl FilePages {
    /// Wrap the inode a private mapping reads from.
    pub fn new(inode: Arc<dyn crate::vfs::Inode>) -> Arc<dyn crate::mm::vma::MappedObject> {
        Arc::new(FilePages { inode })
    }
}

impl crate::mm::vma::MappedObject for FilePages {
    fn frame_at(&self, index: u64) -> Option<usize> {
        let frame = mm::alloc_user_frame()?;
        // A page past the end of the file keeps the zeros a short read leaves,
        // which is POSIX's promise for the tail of a mapping.
        let dest = unsafe {
            core::slice::from_raw_parts_mut(
                mm::phys_to_virt(frame) as *mut u8,
                crate::mm::pframe::PAGE_SIZE,
            )
        };
        dest.fill(0);
        let start = (index as usize).checked_mul(PAGE_SIZE)?;
        let mut done = 0;
        while done < dest.len() {
            match self.inode.read_at((start + done) as u64, &mut dest[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                // A read that fails is Linux's `SIGBUS`, not a zero page: the
                // fault refuses, and the refusal is the honest answer.
                Err(_) => {
                    crate::mm::free_frames(frame, 0);
                    return None;
                }
            }
        }
        Some(frame)
    }

    fn identity(&self) -> ((u32, u32), u64) {
        (
            self.inode.rdev().unwrap_or((0, 0)),
            self.inode.metadata().ino,
        )
    }

    fn name_is_gone(&self) -> bool {
        self.inode.name_is_gone()
    }

    /// A private file mapping's pages come from the filesystem, so a syscall
    /// validating a buffer may bring one in - which is what keeps `write(2)`
    /// on a page this process has not touched yet from answering `EFAULT`.
    fn lazy_populatable(&self) -> bool {
        true
    }
}

pub fn map_frames(
    virt: usize,
    len: usize,
    flags: MapFlags,
    object: Arc<dyn crate::mm::vma::MappedObject>,
    offset: usize,
    shared: bool,
    name: Option<Arc<str>>,
) {
    if len == 0 {
        return;
    }
    current_mm().vma_insert(
        Vma::new(
            virt,
            virt + len,
            flags,
            VmaKind::Frames {
                object,
                offset,
                shared,
            },
        )
        .named(name),
    );
}

/// `munmap`'s two halves at once: forget the VMAs covering `virt..virt + len`
/// and hand back the frames the present pages held.
///
/// The order matters. The table goes first, so that a page released here is not
/// also reachable through a stale VMA - which is what `release_user_frames`
/// relies on when it asks the group to forget the frame.
pub fn munmap_user(virt: usize, len: usize) -> Result<(), isize> {
    if len == 0 {
        return Ok(());
    }
    let end = virt.saturating_add(len);
    // `mseal(2)`'s whole point: a sealed span may not be unmapped. The refusal
    // is for the call as a whole, not the sealed slice of it - measured: a
    // `munmap` covering one sealed page and three unsealed ones is `EPERM` in
    // full, and nothing is unmapped.
    if current_mm().sealed_within(virt, end) {
        return Err(crate::err::EPERM);
    }
    current_mm().vma_remove_range(virt, end);
    current_mm().forget_ranges(virt, end);
    release_user_frames(unmap_user(virt, len));
    Ok(())
}

/// Whether any part of `virt..virt + len` is sealed. The guard every resize,
/// re-protect and unmap asks; `mremap` asks it once for the source span, while
/// `munmap` and `mprotect` ask it through their own entry points.
pub fn user_range_sealed(virt: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    current_mm().sealed_within(virt, virt.saturating_add(len))
}

/// `mseal(start, len)`: seal the span, or `ENOMEM` when any byte of it is not
/// mapped (measured: a hole inside the span, an address nothing owns, and a
/// NULL start all answer `ENOMEM`; the length is rounded up to a page and a
/// zero length is `0`, which the handler has already checked).
pub fn seal_user_range(virt: usize, len: usize) -> Result<(), isize> {
    if len == 0 {
        return Ok(());
    }
    let end = virt.checked_add(len).ok_or(crate::err::ENOMEM)?;
    let mm = current_mm();
    // Both descriptions count as "mapped": a span over the program image is
    // sealable because the image is an eager range even though no VMA names it.
    if !mm.vma_gaps(virt, end).is_empty() {
        return Err(crate::err::ENOMEM);
    }
    critical_section::with(|cs| mm.vmas.borrow(cs).borrow_mut().seal_present(virt, end));
    mm.seal_ranges(virt, end);
    Ok(())
}

/// `mprotect`'s two halves: the VMAs get the new permissions so that a page
/// faulted in later comes up with them, and the pages already present are
/// re-flagged where they lie.
pub fn protect_user_range(virt: usize, len: usize, flags: MapFlags) -> Result<(), isize> {
    if len == 0 {
        return Ok(());
    }
    let end = virt.saturating_add(len);
    // A sealed range's permissions are a promise no one may rewrite - measured:
    // `mprotect` on a sealed mapping is `EPERM`, whether the mapping is
    // anonymous or a file's.
    if current_mm().sealed_within(virt, end) {
        return Err(crate::err::EPERM);
    }
    current_mm().vma_set_prot(virt, end, flags);
    current_mm().reprot_ranges(virt, end, flags);
    protect_user(virt, len, flags);
    Ok(())
}

/// `PR_SET_VMA_ANON_NAME`: name (or unname) the anonymous mappings in
/// `virt..virt + len`. The table's own rule is
/// [`VmaTable::set_anon_name`]'s; this is the facade the `prctl` handler asks,
/// because the table is the address space's.
pub fn name_user_range(
    virt: usize,
    len: usize,
    name: Option<alloc::sync::Arc<str>>,
) -> Result<(), isize> {
    let end = virt.saturating_add(len);
    // A range whose pages came from a **file** is `EBADF` (measured), and this
    // kernel holds two descriptions that can say so: the VMA table for an
    // object-backed mapping, and the recorded ranges for the eager ones a
    // `MAP_PRIVATE` file mapping and the loader leave behind - which have no VMA
    // at all, so the table alone would answer the *unmapped* errno for them
    // (`t-maps` caught exactly that: `ENOMEM` where Linux says `EBADF`). The
    // recorded list is asked first for that reason.
    if current_mm()
        .eager_ranges()
        .iter()
        .any(|range| range.start < end && virt < range.end)
    {
        return Err(crate::err::EBADF);
    }
    critical_section::with(|cs| {
        current_mm()
            .vmas
            .borrow(cs)
            .borrow_mut()
            .set_anon_name(virt, end, name)
    })
}

/// Whether a destructive `madvise` over `virt..end` hits a sealed page that is
/// anonymous and not writable - the one shape Linux refuses (measured; see
/// [`madvise_dontneed`]).
///
/// A page can be described by a VMA or by an eager range, so both are asked. A
/// bracketed name (`[stack]`, `[heap]`) is this kernel's spelling of an
/// anonymous range in that second list; anything else there is a file's.
pub fn destructive_advice_refused(virt: usize, end: usize) -> bool {
    let mm = current_mm();
    let mut page = virt;
    while page < end {
        if let Some(vma) = mm.vma_at(page) {
            if vma.sealed
                && matches!(vma.kind, VmaKind::Anon)
                && !vma.prot.contains(MapFlags::WRITE)
            {
                return true;
            }
        } else {
            for range in mm.eager_ranges() {
                if page >= range.start && page < range.end {
                    let anon = range
                        .name
                        .as_deref()
                        .is_some_and(|name| name.starts_with('['));
                    if range.sealed && anon && !range.prot.contains(MapFlags::WRITE) {
                        return true;
                    }
                    break;
                }
            }
        }
        page += PAGE_SIZE;
    }
    false
}

/// Whether `virt..end` holds a page `mlock` has pinned.
///
/// `MADV_DONTNEED` and `MADV_FREE` both refuse a locked range with `EINVAL`:
/// Linux's `madvise_dontneed_free_valid_vma` (`mm/madvise.c:886`) puts
/// `VM_LOCKED` in the forbidden set unless the advice is
/// `MADV_DONTNEED_LOCKED`, and takes the same `-EINVAL` path for both
/// behaviours. Measured on 7.2.14 (`build/scratch/mlk/dont.c`): a 32-page
/// locked range answers `-1 EINVAL` to `MADV_DONTNEED`, keeps its contents, and
/// answers `-1 EINVAL` to `MADV_FREE` too - while `mprotect(PROT_NONE)` over the
/// same pages is `0` and leaves `VmLck` alone.
pub fn locked_advice_refused(virt: usize, end: usize) -> bool {
    let mm = current_mm();
    let mut page = virt;
    while page < end {
        if let Some(vma) = mm.vma_at(page) {
            if vma.locked {
                return true;
            }
        } else if mm
            .eager_ranges()
            .iter()
            .any(|range| range.locked && page >= range.start && page < range.end)
        {
            return true;
        }
        page += PAGE_SIZE;
    }
    false
}

/// `madvise(MADV_DONTNEED)` over `virt..virt + len`: give the range's pages
/// back, leaving the mapping itself in place.
///
/// The mapping stays, so the next access is an ordinary fault and the page
/// comes back from whatever backs the range - and *that* is the whole of the
/// advice's contract, which is why it took demand paging to answer. A private
/// anonymous page comes back freshly zeroed (its frame is gone, so the fault
/// allocates a new one); a page of a frame-backed range - a memfd, a
/// framebuffer, or the unnamed object a shared anonymous mapping is made of -
/// comes back from the object, which still holds it. Linux says the same thing
/// in the same words: "subsequent accesses ... will result in either
/// repopulating the memory contents from ... the underlying mapped file ... or
/// zero-fill-on-demand pages for anonymous private mappings".
///
/// Only pages a VMA covers are touched. That is not a nicety: the program
/// image and the stack are mapped but have no VMA, and dropping one of those
/// would leave an address that no rule says how to refill.
pub fn madvise_dontneed(virt: usize, len: usize) -> Result<(), isize> {
    if len == 0 {
        return Ok(());
    }
    let end = virt.saturating_add(len);
    // `mseal(2)`'s madvise half, measured on Linux 7.2.11
    // (`build/scratch/mseal/probe4.c`, `probe6.c`): a destructive advice on a
    // sealed mapping is refused **only** when the pages are anonymous and not
    // writable - `EPERM` for a sealed read-only anonymous page, `0` for a
    // sealed writable one and for any file-backed mapping (shared or private,
    // because a file still holds the answer the pages came from). The rule is
    // the same sentence the man page writes, and the two permissive halves are
    // what a compositor's sealed-but-mapped framebuffer needs.
    if destructive_advice_refused(virt, end) {
        return Err(crate::err::EPERM);
    }
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return;
        };
        let mm = Arc::clone(&task.mm);
        let Some(space) = task.space.as_mut() else {
            return;
        };
        let mut dropped = Vec::new();
        let mut page = pframe::align_down(virt, PAGE_SIZE);
        while page < end {
            if !mm.vma_covers(page) {
                page += PAGE_SIZE;
                continue;
            }
            // The whole run of covered pages in one unmapping, so a range that
            // is all one VMA - the common case - is one page-table walk and one
            // TLB flush rather than one of each per page.
            let run = page;
            while page < end && mm.vma_covers(page) {
                page += PAGE_SIZE;
            }
            dropped.extend(space.unmap(run, page - run));
        }
        // The frames are the mapping's own references. For an `Anon` page this
        // is the last one, so the frame goes back to the allocator and the
        // "zero-filled page" the caller is promised really is a new one; for a
        // frame-backed page it is one of several, and the object's copy is what
        // the next fault finds.
        for frame in dropped {
            mm.disown(frame);
            crate::mm::free_frames(frame, 0);
        }
    });
    Ok(())
}

/// Whether an address is the current process's: a page is there, or a VMA says
/// one belongs there.
///
/// The distinction from [`user_frame_at`] is the whole point of demand paging:
/// a mapping is *claimed* at `mmap` time and *filled* on touch, so "is this
/// address mapped?" and "is there a frame behind it?" are different questions.
pub fn user_mapped(virt: usize) -> bool {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let task = t.task(owner);
        task.space
            .as_ref()
            .and_then(|space| space.phys_for_virt(virt))
            .is_some()
            || task.mm.vma_covers(virt)
    })
}

/// `mlock`/`munlock` over the current process's address space: mark or unmark
/// every description inside `virt..virt + len` as pinned.
pub fn set_locked(start: usize, end: usize, locked: bool) -> usize {
    current_mm().set_locked(start, end, locked)
}

/// How many pages the current process has pinned - `VmLck`'s numerator.
pub fn locked_pages() -> usize {
    current_mm().locked_pages()
}

/// How many pinned pages of the current process lie inside `start..end`.
pub fn locked_within(start: usize, end: usize) -> usize {
    current_mm().locked_within(start, end)
}

/// `mlockall(MCL_CURRENT)`: pin every mapping the current process has.
pub fn lock_current() {
    current_mm().lock_everything();
}

/// `munlockall()`: unpin everything and stop pinning what is mapped next.
pub fn unlock_all() {
    current_mm().unlock_everything();
}

/// Whether `mlockall(MCL_FUTURE)` is in force for the current process.
pub fn lock_future() -> bool {
    current_mm().lock_future()
}

/// Set or clear `mlockall`'s `MCL_FUTURE` promise for the current process.
pub fn set_lock_future(on: bool) {
    current_mm().set_lock_future(on)
}

/// The current process's mapped pages - `mm->total_vm`, which is what
/// `mlockall(MCL_CURRENT)` measures against `RLIMIT_MEMLOCK`.
pub fn mapped_pages() -> usize {
    current_mm().mapped_pages()
}

/// The permissions the current process has on `virt`, if a VMA covers it.
///
/// Used where the *range* decides what a later fault will be allowed to do -
/// `MADV_DONTNEED` dropping pages that must come back writable, `mremap`
/// carrying a mapping's permissions with it.
/// The permissions the process's own mapping gives `virt`, or `None` when no
/// mapping describes that address.
///
/// A fault report's question rather than a resolver's: the resolvers have all
/// answered no by the time anything is printed, and the one fact that decides
/// between the two possible stories is missing - whether the page was writable
/// to begin with. A writable mapping whose page this kernel left read-only is a
/// defect here; a read-only one is the process writing where it may not, which
/// is Linux's answer too.
pub fn vma_prot_at(virt: usize) -> Option<MapFlags> {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        t.task(owner)
            .mm
            .vma_at(pframe::align_down(virt, PAGE_SIZE))
            .map(|vma| vma.prot)
    })
}

/// What kind of mapping describes `virt`, in a word a fault report can print.
///
/// [`vma_prot_at`]'s companion: the permissions say whether the process was
/// allowed to write, and this says *what* it was writing to - an anonymous
/// arena, a private object (a `memfd` or a file mapped `MAP_PRIVATE`) or one
/// shared with somebody else.
pub fn vma_kind_at(virt: usize) -> Option<&'static str> {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        t.task(owner)
            .mm
            .vma_at(pframe::align_down(virt, PAGE_SIZE))
            .map(|vma| match &vma.kind {
                VmaKind::Anon => "anon",
                VmaKind::Frames { shared: true, .. } => "object(shared)",
                VmaKind::Frames { shared: false, .. } => "object(private)",
            })
    })
}

/// What the mapping containing `addr` is *called*, for a fault report: a file
/// mapping's name, a `[stack]`, the program image's own segment name, or a
/// library the interpreter mapped.
///
/// The two sources are the two `/proc/<pid>/maps` reads as well: the VMA table
/// (which names a device, a `memfd`, a file) and the loader's *eager* ranges
/// (the image's segments and every shared object the interpreter mapped, each
/// recorded with the path it was opened at). A faulting `rip` is almost always
/// inside one of those, so this is what turns "some library wrote 2 MiB below
/// its stack pointer" into "*this* library did".
pub fn mapping_name_at(addr: usize) -> Option<alloc::string::String> {
    let page = pframe::align_down(addr, PAGE_SIZE);
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let mm = &t.task(owner).mm;
        // The *range* travels with the name, and that is not decoration: an
        // unnamed VMA over a page 2 MiB below `sp` says only "some anonymous
        // region", while `0x…-0x…` beside it says whether the region is the
        // stack the loader made (64 KiB, `[stack]`), one the stack grew, or
        // something else entirely. `rw-u` is the permissions the *mapping*
        // asks for, which is what makes a read-only *page* inside it an
        // inconsistency rather than a refusal.
        let vma_descr = mm.vma_at(page).map(|vma| {
            alloc::format!(
                "vma[{}] {:#x}..{:#x} {}{}{}{} kind={}",
                vma.name.as_deref().unwrap_or("unnamed"),
                vma.start,
                vma.end,
                if vma.prot.contains(MapFlags::READ) {
                    "r"
                } else {
                    "-"
                },
                if vma.prot.contains(MapFlags::WRITE) {
                    "w"
                } else {
                    "-"
                },
                if vma.prot.contains(MapFlags::EXEC) {
                    "x"
                } else {
                    "-"
                },
                if vma.prot.contains(MapFlags::USER) {
                    "u"
                } else {
                    "-"
                },
                match &vma.kind {
                    VmaKind::Anon => "anon",
                    VmaKind::Frames { shared: true, .. } => "object(shared)",
                    VmaKind::Frames { shared: false, .. } => "object(private)",
                }
            )
        });
        // Both sources, when both cover the page. `handle_mmap`'s eager path
        // records a *range* and no VMA ("every page is present, so this mapping
        // has no VMA and no fault will ever describe it") - so a page that a
        // VMA *and* a range describe is a page two mappings share, which is the
        // shape a dynamic linker's read-only pass and its writable segment make
        // of one 4 KiB page. Printing only the first source would hide half of
        // it, and the half it hides is the one that says *which file*.
        let mut eager = None;
        for range in mm.eager_ranges() {
            if page >= range.start && page < range.end && eager.is_none() {
                eager = Some(alloc::format!(
                    "eager[{}] {:#x}..{:#x}",
                    range.name.as_deref().unwrap_or("unnamed"),
                    range.start,
                    range.end
                ));
            }
        }
        if let Some(eager) = eager {
            return Some(match vma_descr {
                Some(vma) => alloc::format!("{vma} + {eager}"),
                None => eager,
            });
        }
        vma_descr
    })
}

/// The `(name, start, end)` of the mapping holding `addr`, for Linux's
/// `in <module>[<offset>,<base>+<len>]` clause of a fault report.
///
/// Deliberately narrower than [`mapping_name_at`], which is a diagnostic dump:
/// this answers only when the range has a *name* - an anonymous page names
/// nothing, and Linux prints no clause for one rather than inventing a name for
/// "somewhere in the heap". The bounds matter for the same reason they do in
/// the dump: `<offset>` and `<len>` are what let a reader map the faulting
/// instruction back to a source file, and a length taken from the wrong
/// mapping would misreport every fault inside the right one.
///
/// The ip is looked up, not the faulting address: Linux's clause names the
/// module the *instruction* came from. A wild pointer read by code in `libc`
/// is a fault `in libc`, and asking about the address instead would answer
/// "no name" for exactly the faults most worth reporting.
pub fn mapping_span_at(addr: usize) -> Option<(alloc::string::String, usize, usize)> {
    let page = pframe::align_down(addr, PAGE_SIZE);
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let mm = &t.task(owner).mm;
        // Eager ranges first: the program image and every library the
        // interpreter mapped are eager, and those are the ranges a faulting
        // instruction actually comes from. A VMA over the same page is the
        // anonymous wrapper `mmap` put around a segment (`mmap` of a
        // `PROT_EXEC` file is the loader's own pattern), so the named one is
        // the answer.
        for range in mm.eager_ranges() {
            if page >= range.start && page < range.end {
                if let Some(name) = range.name.as_deref() {
                    return Some((alloc::string::String::from(name), range.start, range.end));
                }
            }
        }
        mm.vma_at(page).and_then(|vma| {
            vma.name
                .as_deref()
                .map(|name| (alloc::string::String::from(name), vma.start, vma.end))
        })
    })
}

/// The VMAs covering `virt..virt + len` in the current process, clipped to the
/// range, with the table left empty there - and their pages *left alone*.
///
/// `mremap` takes a mapping's backing out of one place and puts it in another,
/// and the VMAs are what remember which backing that is. The frames travel
/// with the mapping and the old ones come away explicitly afterwards, so this
/// must not free them on the way out: lifting a `PROT_NONE` description through
/// the page-table half used to hand its untouched pages to the allocator while
/// the PTE stayed live (`NOTES.md` §15.18).
pub fn take_vmas(virt: usize, len: usize) -> Vec<Vma> {
    if len == 0 {
        return Vec::new();
    }
    current_mm().vma_lift_range(virt, virt.saturating_add(len))
}

/// Whether this process already holds as many descriptions as Linux's own
/// default `max_map_count` allows - the question `mmap` asks before it adds
/// one (`docs/notes/` §10.11).
pub fn vma_count_reached() -> bool {
    current_mm().vma_count() >= crate::mm::vma::max_map_count()
}

/// The parts of `virt..virt + len` the current process has not already claimed.
pub fn vma_gaps(virt: usize, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    current_mm().vma_gaps(virt, virt.saturating_add(len))
}

/// The System V shared-memory segment attached at `virt`, as
/// `(start, end, shmid)`.
///
/// `shmdt`'s whole validation: Linux answers `EINVAL` unless the address is
/// exactly the start of a mapping of a segment (a `shmat` returns that address,
/// and a caller that adds an offset to it has not named an attachment). The
/// mapping is identified by its object - only a segment answers
/// [`MappedObject::shm_id`] - rather than by anything the VMA's own range says,
/// because the same address may have been attached twice in a process's life.
pub fn shm_attachment_at(virt: usize) -> Option<(usize, usize, u64)> {
    let vma = current_mm().vma_at(virt)?;
    match &vma.kind {
        VmaKind::Frames { object, .. } => object.shm_id().map(|id| (vma.start, vma.end, id)),
        VmaKind::Anon => None,
    }
}

/// How many attachments of the segment `id` any address space holds - the
/// `shm_nattch` a segment reports.
///
/// Folded out of the VMA tables rather than counted on the segment because a
/// `fork` copies a mapping and Linux counts the child as an attachment
/// (measured: a freshly forked child reads `nattch` one higher, and the count
/// falls when it exits). The fold counts **runs** rather than entries, because
/// splitting a mapping in two (`mprotect` half of it, a `MAP_FIXED` in its
/// middle) makes two VMAs of one attachment, and Linux's count does not change
/// on a split - its `vm_ops->open` is what counts, and a split does not call it.
/// Two entries continue one run exactly when the second starts where the first
/// ends at the offset the first would have there, which is the same arithmetic
/// [`Vma::abuts`] merges on.
///
/// The walk is one pass over the tasks and their tables, and it runs only for a
/// `shmctl(IPC_STAT)`, a reap sweep or a `/proc` read - never on a fault.
pub fn shm_attachment_count(id: u64) -> usize {
    with_table(|t| {
        let mut count = 0;
        let mut seen: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        for slot in t.tasks.iter() {
            let Some(task) = slot else { continue };
            // A thread shares its group's address space, so counting one table
            // per thread would multiply every attachment by the thread count.
            let mm = Arc::as_ptr(&task.mm) as usize;
            if seen.contains(&mm) {
                continue;
            }
            seen.push(mm);
            // `(segment, first offset, first start, end)` of the run being
            // counted. The first offset and start stay put as pieces are
            // folded in: what makes a piece a continuation is the offset the
            // *run* would give it, not the offset of the piece before it.
            let mut run: Option<(u64, usize, usize, usize)> = None;
            for vma in task.mm.vma_snapshot().iter() {
                let here = match &vma.kind {
                    VmaKind::Frames { object, offset, .. } => {
                        object.shm_id().map(|sid| (sid, *offset))
                    }
                    VmaKind::Anon => None,
                };
                let Some((sid, offset)) = here else {
                    run = None;
                    continue;
                };
                let continues = run.is_some_and(|(rid, roffset, rstart, rend)| {
                    rid == sid && rend == vma.start && offset == roffset + (rend - rstart)
                });
                if continues {
                    let (_, roffset, rstart, _) = run.expect("checked above");
                    run = Some((sid, roffset, rstart, vma.end));
                } else {
                    if sid == id {
                        count += 1;
                    }
                    run = Some((sid, offset, vma.start, vma.end));
                }
            }
        }
        count
    })
}

/// Put a VMA back, backing and all - how `mremap` completes a move.
///
/// The caller has just re-placed every page of the range at its destination,
/// so this is [`VmaTable::insert_present`]'s case: the description may fold
/// into an abutting neighbour, but the merge must not go through the path
/// that frees pages beside it (`NOTES.md` §15.18).
pub fn insert_vma(virt: usize, end: usize, flags: MapFlags, kind: VmaKind) {
    if virt >= end {
        return;
    }
    current_mm().vma_insert_present(Vma::new(virt, end, flags, kind));
}

/// Claim `virt..virt + len` for anonymous memory without disturbing what is
/// already there: the shape a heap growth wants, where a guard page a libc
/// mapped inside the range must survive. The claim is append-only
/// ([`VmaTable::insert_present`]) for the same reason `brk`'s own tail is.
pub fn claim_anon_gaps(virt: usize, len: usize, flags: MapFlags) {
    let mm = current_mm();
    for (start, end) in mm.vma_gaps(virt, virt.saturating_add(len)) {
        mm.vma_insert_present(Vma::new(start, end, flags, VmaKind::Anon));
    }
}

/// `brk(addr)` — per-task program break (heap end).
///
/// `addr` of 0 asks where the break is. Otherwise the break moves, and the
/// pages between the old and the new position are mapped or given back as the
/// move requires - which is what makes this a *break* rather than a note that
/// the number changed.
///
/// Growing skips a page that is already mapped instead of insisting it is
/// free. That is not a nicety: a libc may put a guard page inside the range the
/// break covers - musl's mallocng maps `PROT_NONE` over the first page of a
/// freshly grown heap - and the break still has to move past it.
pub fn brk(addr: usize) -> isize {
    use crate::mm::pframe::PAGE_SIZE;
    use crate::mm::vm::MapFlags;

    let id = current();
    with_table(|t| {
        let task = t.task_mut(id);
        // Kernel tasks have no brk.
        if task.space.is_none() {
            return 0isize;
        }

        if task.brk_base == 0 {
            return 0isize;
        }

        if addr == 0 {
            return (task.brk_base + task.brk_pages * PAGE_SIZE) as isize;
        }

        let needed_pages = if addr > task.brk_base {
            (addr - task.brk_base).div_ceil(PAGE_SIZE)
        } else {
            0
        };

        // A break that shrinks gives its frames back; one that grows takes
        // more. Either way the pages are this process's from here on.
        let frontier = task.brk_base + needed_pages * PAGE_SIZE;
        // The heap may not walk into the mmap region: between the two lies the
        // hole that gives the address space its Linux shape. A break that far
        // up is a runaway allocation, not a request to honor.
        if frontier >= crate::mm::elf::MMAP_BASE {
            return crate::err::ENOMEM;
        }
        // Growing over a range a VMA already describes is refused the way
        // Linux refuses it, with `EINVAL` and the break left where it was
        // (`brk(mm_brk)` after a `PROT_NONE` mapping inside the gap, measured
        // on real Linux - `NOTES.md` §15.18). The page-wise question would
        // answer "free" for such a range, because a lazily-mapped VMA has no
        // frames in it yet, and claiming it anyway would split somebody
        // else's description. A libc that hits this reads `EINVAL` as "my
        // own mapping is in the way", backs off, and falls back to `mmap`,
        // which is exactly what glibc does before it ever asks again.
        //
        // Only a range some description covers is refused: an ordinary
        // growth walks into empty address space, which is one hole from the
        // old break to the new one, so asking whether the growth range is
        // *free* is the question - an inverted predicate that refused every
        // range with a hole in it turned `t-brk` and `t-maps` red on both
        // machines.
        let grown = task.brk_base + task.brk_pages * PAGE_SIZE;
        if needed_pages > task.brk_pages && !task.mm.vma_range_free(grown, frontier) {
            return crate::err::EINVAL;
        }
        let grown_to = grown;
        if frontier < grown_to {
            // The description goes with the pages: a heap that shrank is no
            // longer the process's past the new break, so `munmap`/`madvise`
            // and a later `fork` must not see it there. The range is the tail
            // above the break, which is where a `brk` shrink always cuts.
            task.mm.vma_remove_range(frontier, grown_to);
            let dropped = match task.space.as_mut() {
                Some(space) => space.unmap(frontier, grown_to - frontier),
                None => alloc::vec::Vec::new(),
            };
            for frame in dropped {
                task.mm.disown(frame);
                crate::mm::free_frames(frame, 0);
            }
        }

        for i in task.brk_pages..needed_pages {
            let vaddr = task.brk_base + i * PAGE_SIZE;
            // Already mapped: someone else's guard page, or a page the break
            // has already been past. Either way there is nothing to add.
            if task
                .space
                .as_ref()
                .and_then(|space| space.phys_for_virt(vaddr))
                .is_some()
            {
                continue;
            }
            let frame = match crate::mm::alloc_frames(0) {
                Some(f) => f,
                None => return crate::err::ENOMEM,
            };
            unsafe {
                let ptr = crate::mm::phys_to_virt(frame) as *mut u8;
                core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
            }
            let mapped = match task.space.as_mut() {
                Some(space) => space
                    .try_map(crate::mm::vm::Mapping {
                        virt: vaddr,
                        phys: frame,
                        length: PAGE_SIZE,
                        flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                    })
                    .is_ok(),
                None => false,
            };
            if !mapped {
                crate::mm::free_frames(frame, 0);
                return crate::err::ENOMEM;
            }
            task.mm.own(frame);
        }

        // The pages the break grew over become a VMA too, so that `munmap`,
        // `madvise` and `fork` see one table rather than two - the heap used to
        // be the one region whose mapping only the page tables knew about.
        //
        // Only the *newly* added range is claimed, and only where nothing else
        // already is: a libc is free to have mapped inside the space the break
        // is about to grow into - musl's mallocng puts a `PROT_NONE` guard over
        // the first page of a fresh heap - and that mapping is its own, not the
        // break's. A gap here is a hole in the *descriptions*, which every
        // frame inside it the growth loop just placed; claiming it with
        // [`VmaTable::insert_present`] folds it into whatever abuts it without
        // re-inserting that neighbour, whose untouched lazy pages must stay
        // somebody's address even though they are nobody's frame yet
        // (`NOTES.md` §15.18).
        if frontier > grown_to {
            for (start, end) in task.mm.vma_gaps(grown_to, frontier) {
                task.mm.vma_insert_present(Vma::new(
                    start,
                    end,
                    MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                    VmaKind::Anon,
                ));
            }
        }

        task.brk_pages = needed_pages;
        addr as isize
    })
}

/// Set the initial brk base for the current task (called at ELF load time).
pub fn set_brk_base(base: usize) {
    with_table(|t| {
        let task = t.task_mut(current());
        task.brk_base = base;
    })
}

/// Get the mmap cursor for the current task.
pub fn get_mmap_cursor() -> usize {
    with_table(|t| {
        let task = t.task(current());
        task.mmap_cursor
    })
}

/// The `MEMBARRIER_CMD_REGISTER_*` commands the running task's process has
/// declared it will use, as a bitmask of the *barrier* commands they enable.
///
/// The process, not the task: a registration made by one thread is what lets
/// every thread of the group use the barrier, which is the only sense in which
/// a barrier over a process's threads can be asked for at all.
pub fn membarrier_registrations() -> usize {
    with_table(|t| t.task(t.current).mm.membarrier_registrations())
}

/// Record a `membarrier` registration for the running task's process.
pub fn register_membarrier(barrier: usize) {
    with_table(|t| t.task(t.current).mm.register_membarrier(barrier));
}

pub fn task_count() -> usize {
    with_table(|t| t.tasks.iter().filter(|slot| slot.is_some()).count())
}

/// Whether any child of `parent` is alive right now - what init's first shell
/// looks like from the boot task's side of the hand-over.
pub fn has_live_child(parent: TaskId) -> bool {
    with_table(|t| child_alive(t, parent))
}

/// Whether the task in `id`'s slot is still running - present and not exited.
///
/// The bring-up handoff needs to wait for the *init task itself* to leave, not
/// for children under it: `spawn_init` gives the first process no parent (it is
/// pid 1, the root of the tree), so `has_live_child(INIT_PID)` - which asks
/// whether the init has live children - is false from the start and would let the
/// boot task finish without ever yielding the CPU to the init. This is the
/// question that handoff actually means: is the program I started still up.
pub fn is_running(id: TaskId) -> bool {
    with_table(|t| t.tasks[id.0].as_ref().is_some_and(|task| !task.exited))
}

/// [`has_live_child`] against a table the caller already holds.
pub(crate) fn child_alive(t: &Table, parent: TaskId) -> bool {
    child_alive_for(t, parent, WaitFor::Any, ChildKind::Either)
}

/// Whether one of `parent`'s children that `sel` names and `kind` admits is
/// still running. The selector half of [`child_alive`], and the live half of
/// `has_matching_children`: a group selector asks every child's own `pgid`, and
/// the kind asks the signal it was born to report - a live "clone" child a
/// default wait cannot be answered about is not a reason to park, it is the
/// `ECHILD` Linux answers (measured, `.workbuddy/probe_wclone.c` G1).
pub(crate) fn child_alive_for(t: &Table, parent: TaskId, sel: WaitFor, kind: ChildKind) -> bool {
    // The caller's **group**, not its slot: a child forked on a worker thread is
    // still the leader's to wait for (`do_wait` walks every thread of the
    // caller's group), and a thread is not a child at all. Both halves live in
    // `trace::waitable`, and this is the live half of the same question.
    let Some(group) = t
        .tasks
        .get(parent.0)
        .and_then(Option::as_ref)
        .map(|task| task.tgid)
    else {
        return false;
    };
    t.tasks.iter().enumerate().any(|(slot, task)| {
        let id = TaskId(slot);
        task.as_ref().is_some_and(|task| {
            // An exited leader whose group still runs is a child a wait can
            // still park on: the process's death is its **last** thread's, and
            // until then Linux answers "nothing yet" rather than `ECHILD`
            // (`.workbuddy/probe_exit_group_thread.c`, C/C2). Once the group is
            // dead the record exists instead, so there is no gap between the
            // two: the last thread's exit writes the record in the same
            // critical section that clears this clause.
            (!task.exited || super::trace::group_has_live_task(t, task.tgid))
                && task.tgid == id
                && super::trace::forked_in_group(t, task, group)
                && id != parent
                && sel.names_task(id, task)
                && kind.sees(task.exit_signal)
        })
    })
}

/// How many tasks are alive forever, and so are not something a `drain` should
/// wait for: the boot task, the scheduler, and everything started as permanent.
///
/// The count has to move, not be a constant: the network service is a task that
/// never exits and is started at a time of the boot code's choosing, and a
/// `drain` that kept the old figure would wait on it forever.
#[cfg(feature = "selftest")]
pub fn permanent_tasks() -> usize {
    PERMANENT_TASKS + PERMANENT_EXTRA.load(Ordering::Relaxed)
}

/// Start a task that is meant to run for the machine's whole life, and count it
/// as one. [`permanent_tasks`] is what tells the boot code's `drain` which
/// tasks are never going to finish.
pub fn spawn_permanent(f: impl FnOnce() + Send + 'static, priority: Priority) -> TaskId {
    let id = spawn(f, priority);
    PERMANENT_EXTRA.fetch_add(1, Ordering::Relaxed);
    id
}

/// Resolve a write fault on a copy-on-write page: the first write one process
/// makes to a page another still shares. The writer gets a private frame, the
/// old one keeps its other owners. The frame lists have to follow, or the new
/// frame leaks and the old one is freed too many times - so the old frame is
/// swapped for the new in this task's list, and the old frame's owner count is
/// dropped by one.
///
/// Returns `false` if the fault cannot be resolved as COW, leaving the caller to
/// deliver SIGSEGV.
pub fn resolve_cow_fault(virt: usize) -> bool {
    with_table(|t| {
        let id = t.current;
        // The space the running task *translates through*, which for a thread
        // is its group leader's, not its own - a thread has no space of its own
        // at all, and asking its slot would answer "no space" and hand every
        // write fault it ever takes to SIGSEGV. A musl thread's first push is a
        // write to the group's memory, so this is the difference between a
        // thread that runs and one that dies on its first instruction.
        let owner = t.space_owner(id);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return false;
        };
        let mm = task.mm.clone();
        let Some(space) = task.space.as_mut() else {
            return false;
        };
        split_cow(space, &mm, virt)
    })
}

/// Give one copy-on-write page of `space` its own frame, and keep the group's
/// books on the swap. `false` if the page was not copy-on-write, in which case
/// nothing changes.
pub(crate) fn split_cow(space: &mut crate::hal::AddressSpace, mm: &Mm, virt: usize) -> bool {
    let Some((old_phys, new_phys)) = space.resolve_cow(virt) else {
        return false;
    };

    // The page the task now owns is `new_phys`; `old_phys` is one claim
    // lighter and may be free.
    if !mm.replace(old_phys, new_phys) {
        // A page outside the tracked list (the ELF loader's mappings are all
        // tracked, so this is unreachable in practice); record the new frame
        // so at least one of the pair is ever given back.
        mm.own(new_phys);
    }
    crate::mm::free_frames(old_phys, 0);
    true
}

/// Grow the main stack to cover `addr` - the fault Linux answers by mapping one
/// more page of stack.
///
/// The loader maps [`USER_STACK_PAGES`] and stops. That is not the size of a
/// stack, it is where one begins: a program's stack is a region that extends as
/// the program descends into it, which is why a function may use more of it
/// than any fixed default. Without this a process is limited to whatever the
/// loader reserved - 64 KiB - and a program with a deep call or a large local
/// frame dies of a segmentation fault it did nothing to deserve. (`apk add`,
/// whose dependency walk is exactly that shape, was the one that found it.)
///
/// Two bounds keep this from becoming a way to claim memory by accident. The
/// fault must lie below the stack top the process was laid out with, and within
/// [`STACK_GROWTH_SLACK`] of the stack's *own* current floor - not within that
/// distance of `sp` above it. That distinction is Linux's: `expand_downwards`
/// measures a stack fault from the `[stack]` vma's `vm_start` plus
/// `stack_guard_gap`, and never from the stack pointer. An aarch64 prologue
/// reserves a whole frame with a single `sub sp, sp, #N` and then saves
/// `x29`/`x30` at the frame's *top*, so the first store into a fresh page lands
/// *above* `sp`; a bound read from `sp` refused exactly that and ended the task
/// (`fastfetch`, `ESR 0x92000047`). A wild pointer lands far below the floor and
/// is still refused.
///
/// The floor itself is the process's own, not the kernel's: it is where the
/// stack has grown so far, which every program draws afresh at `exec`. A growth
/// check against the old constant would hand a process pages above the stack it
/// was given, in a region its own page tables say nothing about.
pub fn resolve_stack_growth(addr: usize) -> bool {
    let page = addr & !(PAGE_SIZE - 1);
    with_table(|t| {
        let id = t.current;
        let stack_top = t.task(id).stack_top;
        if addr >= stack_top {
            return false;
        }
        // RLIMIT_STACK is the floor, read from the faulting task rather than a
        // compile-time constant: a soft limit below the reserved region stops
        // growth early (SIGSEGV beyond it), and a raised or infinite one is
        // capped at the region actually reserved for the stack.
        let stack_max = t
            .task(id)
            .rlimits
            .get(3)
            .map_or(USER_STACK_MAX as u64, |r| r.cur)
            .min(USER_STACK_MAX as u64) as usize;
        if page < stack_top - stack_max {
            return false;
        }
        // The stack's own low-water mark: where growth has reached so far,
        // which `/proc/<pid>/maps` reports as the `[stack]` line's start. A
        // fault within SLACK below it is another push; one further down is a
        // wild pointer and stays the process's own fault to bear.
        let floor = t
            .task(id)
            .mm
            .stack_floor()
            .unwrap_or(stack_top - USER_STACK_PAGES * PAGE_SIZE);
        if page < floor.saturating_sub(STACK_GROWTH_SLACK) {
            return false;
        }
        // A sealed stack does not grow: `mseal` promises the range cannot be
        // resized, and growing it downwards is exactly that. Linux answers the
        // fault with `SIGSEGV`. (`false` here is the same answer - the fault
        // has no resolver and the program is killed.)
        if t.task(id).mm.eager_sealed_within(page, stack_top) {
            return false;
        }
        // The group's space, for the same reason a copy-on-write fault looks
        // there: a thread's stack is a mapping the group shares, and only the
        // leader's slot holds the tables that describe it.
        let owner = t.space_owner(id);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return false;
        };
        let Some(space) = task.space.as_mut() else {
            return false;
        };
        let Some(frame) = mm::alloc_frames(0) else {
            return false;
        };
        // A stack page nobody has written reads as zeros. It is a hole in the
        // program's memory, not whatever the frame's last owner left there -
        // and a program that reads uninitialised stack expects zeros.
        let dest = unsafe {
            core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);
        let mapped = space.try_map(Mapping {
            virt: page,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        if mapped.is_err() {
            // Something is already mapped there, so this was not a stack
            // growth fault after all: leave the page as it was.
            mm::free_frames(frame, 0);
            return false;
        }
        task.mm.own(frame);
        // The stack's reported range follows it down, so a program deep in a
        // recursion does not read a `[stack]` line that stops 64 KiB below
        // where it is actually standing. Linux's `[stack]` line covers the
        // VMA, which grows the same way.
        task.mm.grow_stack_range(page);
        true
    })
}

pub struct Mm {
    /// How many threads are running on this group. The last one out frees the
    /// frames; a group of one is a process that never made a thread.
    pub(crate) live: core::sync::atomic::AtomicUsize,
    /// The paging root of the group's address space, as CR3 is loaded with it.
    /// A thread has no `AddressSpace` of its own and switches to this one.
    pub(crate) root_value: usize,
    /// Physical frames behind the group's user mappings - program pages,
    /// stack and heap. Dropping the address space frees its tables but never
    /// these, so the reaper returns them by hand.
    pub(crate) frames: Mutex<RefCell<Vec<usize>>>,
    /// The largest number of frames this group has ever held at once - Linux's
    /// `VmHWM`, the high-water resident set. It is recorded where the frames
    /// table grows ([`Self::own`]) and nowhere derived: a process that mapped 4
    /// MiB, touched it and freed it reads `VmHWM: 4096 kB` and `VmRSS: 0 kB` on
    /// Linux (`build/scratch/vmfields/p.c`), which is a fact about the past that
    /// no current count can answer. `ru_maxrss` is the same number in KB, so
    /// this is also the group's own half of that field (§6.113).
    pub(crate) hwm_frames: core::sync::atomic::AtomicUsize,
    /// The largest `ru_maxrss` (in KB) among the children this group has
    /// **reaped** - Linux's `RUSAGE_CHILDREN`. It is a MAX, not a sum: a fresh
    /// worker that reaps an 8 MiB child reads `8204`, then reaps a 0-byte child
    /// and still reads `8204`, then reaps a 1 MiB child with an 8 MiB
    /// grandchild and still reads `8204` (`build/scratch/maxrss/u.c`, Linux
    /// 7.2.14). Recorded on wait as `fetch_max`, off the child's record, so the
    /// parent's own peak (`hwm_frames`) and its children's peak stay two
    /// separate books - exactly the split §6.105 gave the CPU and switch pairs.
    pub(crate) cmaxrss: core::sync::atomic::AtomicU64,
    /// The largest number of pages this address space has ever claimed -
    /// Linux's `VmPeak`, the high-water virtual size. Recorded where a mapping
    /// is added ([`Self::record_peak`]); `unmap` must not lower it.
    pub(crate) peak_pages: core::sync::atomic::AtomicUsize,
    /// The group's virtual memory areas: which ranges exist, with what
    /// permissions, and what backs each one.
    ///
    /// This is Linux's `mm->mmap`, and it is what makes a mapping an *address
    /// space* rather than a set of pre-allocated frames: a range with no page
    /// behind it yet is still mapped, and a fault inside it is answered from
    /// the [`crate::mm::vma`] entry instead of by SIGSEGV. The per-group
    /// placement is deliberate - threads share one address space, so they
    /// share one table, and `fork` copies it.
    pub(crate) vmas: Mutex<RefCell<VmaTable>>,
    /// The ranges mapped *eagerly* - every page filled as it was mapped, so no
    /// address in one ever faults. Here rather than in `vmas` for the reason
    /// [`MappedRange`] gives. Only `/proc/<pid>/maps` reads this.
    pub(crate) eager: Mutex<RefCell<Vec<MappedRange>>>,
    /// The thread that started the process: its id is the group's pid, and
    /// `getpid` answers it from any of the threads.
    pub(crate) leader: TaskId,
    /// Where a leaving thread writes zero and wakes whoever waits there -
    /// `CLONE_CHILD_CLEARTID`, which is how a libc's `pthread_join` learns a
    /// thread is gone without polling.
    pub(crate) clear_tid: Mutex<RefCell<Option<usize>>>,
    /// The `MEMBARRIER_CMD_REGISTER_*` commands this process has declared it
    /// will use, as a bitmask.
    ///
    /// Per *process*, not per thread, and that is not a shortcut: the
    /// registration is a statement about threads sharing one address space, and
    /// the sharing is what makes the barrier mean anything - which is why Linux
    /// keeps it on the `mm` too. A thread's `Mm` *is* the group's, so the `Arc`
    /// gives these semantics for free.
    pub(crate) membarrier: core::sync::atomic::AtomicUsize,
    /// The robust-futex list head this process registered with
    /// `set_robust_list`, kept so `get_robust_list` can answer it. Like the
    /// membarrier registrations it lives on the group, not the task: a libc
    /// registers one list per thread, threads are not implemented, so the group
    /// has the one. Walking the list to release held futexes at thread exit is a
    /// separate, named gap - storing the head is what makes `get` honest.
    pub(crate) robust_head: core::sync::atomic::AtomicUsize,
    /// The `comm` a process set with `PR_SET_NAME`, as a 16-byte NUL-terminated
    /// name; all-zero means none was set, and `comm_of` then falls back to the
    /// image's basename. Lives on the group like the other per-process facts
    /// above; `/proc/<pid>/comm` and `PR_GET_NAME` both report it, which is the
    /// whole reason `PR_SET_NAME` is honest rather than accepted-and-dropped.
    pub(crate) comm_name: [core::sync::atomic::AtomicU8; 16],
    /// The group's credentials: the four capability sets, the dumpable flag
    /// and `KEEPCAPS`. On the group rather than the task because every consumer
    /// here - `capget`/`capset`, the `prctl` options, the `execve` derivation
    /// and `ptrace_may_access` - is about the process, and this kernel's
    /// threads are one process; Linux keeps them per-thread and its libc keeps
    /// the threads in step, so a group-wide copy is the same state without the
    /// bookkeeping. The policy itself is [`crate::cred`], which is pure and
    /// host-tested; this is only where it is stored.
    pub(crate) credentials: Mutex<RefCell<crate::cred::Credentials>>,
    /// CPU time the group has spent with the core executing *its* tasks, in
    /// internal clock ticks (the 250 Hz tick), split user / system by the ring
    /// the timer interrupt took the core out of. On the group, not the task,
    /// because `times()` and `getrusage(RUSAGE_SELF)` are process-wide in Linux
    /// and the `Arc`-shared `Mm` is what a process is here: every thread's ticks
    /// land in the same two counters. Aggregating *reaped children* into
    /// `cutime`/`cstime` (and `RUSAGE_CHILDREN`) is a separate named gap -
    /// `NOTES.md` §12.2 B6.
    pub(crate) cpu_utime: core::sync::atomic::AtomicU64,
    pub(crate) cpu_stime: core::sync::atomic::AtomicU64,
    /// The CPU of this process's *reaped* children, accumulated by
    /// [`Mm::absorb_child`] at the reap: `Linux` folds a waited child's
    /// `utime + cutime` into the parent's `cutime`. Zero until a child is
    /// collected. What `tms_cutime`/`tms_cstime` and `getrusage(RUSAGE_CHILDREN)`
    /// report.
    pub(crate) cpu_cutime: core::sync::atomic::AtomicU64,
    pub(crate) cpu_cstime: core::sync::atomic::AtomicU64,
    /// Context switches this group's tasks have made, `(voluntary,
    /// involuntary)` - what `getrusage`'s `ru_nvcsw`/`ru_nivcsw` report.
    ///
    /// Linux counts a switch as **voluntary** when the task left the CPU
    /// because it had nothing to run (a sleep, a wait, a blocking read) and
    /// **involuntary** when the CPU was taken from it (a preemption by the
    /// clock, or a `sched_yield` that found somebody else). Measured on a
    /// current host (`build/scratch/rusage/switch.c`, Linux 7.2.14): 200
    /// `nanosleep(1 ms)` calls moved `ru_nvcsw` by exactly 200 and `ru_nivcsw`
    /// by 0; 200 `sched_yield` calls moved `ru_nvcsw` by 0 and `ru_nivcsw` by 1.
    ///
    /// On the group rather than the task for the reason the CPU counters are:
    /// `RUSAGE_SELF` is the process's, and a thread's switches are the
    /// process's - measured in the same run (`build/scratch/rusage/threadsw.c`),
    /// a worker that slept 100 times moved the leader's `RUSAGE_SELF` by 101
    /// while the worker's own `RUSAGE_THREAD` read 100.
    pub(crate) nvcsw: core::sync::atomic::AtomicU64,
    pub(crate) nivcsw: core::sync::atomic::AtomicU64,
    /// These fields for this process's *reaped* children, accumulated by
    /// [`Mm::absorb_child_switches`] at the reap - Linux's `cnvcsw`/`cnivcsw`,
    /// which is what `getrusage(RUSAGE_CHILDREN)` reports.
    pub(crate) cnvcsw: core::sync::atomic::AtomicU64,
    pub(crate) cnivcsw: core::sync::atomic::AtomicU64,
    /// Where the program image sits in this address space, and where its stack
    /// pointer started: the three absolute addresses `/proc/<pid>/stat` fields
    /// 26-28 report as `startcode`, `endcode` and `startstack`.
    ///
    /// Recorded at load rather than derived, because none of the three is
    /// recoverable from the mapping tables. `endcode` is `p_filesz`'s end, not
    /// the page the text *occupies* - a segment with a 1 MiB `.bss` behind it
    /// still ends at `start_code + p_filesz` (measured:
    /// `build/scratch/statcode/x.c`, a single `RWE` load with `FileSiz 0x640`
    /// and `MemSiz 0x100648` reads `endcode - startcode = 0x640`). And
    /// `startstack` is the `rsp` the process was *entered* with, which no
    /// current register and no `[stack]` VMA boundary answers: measured
    /// `build/scratch/statcode/start.S`, a program that saves `%rsp` in its
    /// first instruction reads exactly that value back out of `/proc/self/stat`
    /// while its own `main` is 5024 bytes deeper.
    ///
    /// `fork` copies all three - a child's address space is its parent's, and
    /// Linux's `copy_mm` takes `start_code`/`end_code`/`start_stack` with it
    /// (`build/scratch/statcode/inherit2.c`) - while `execve` replaces them
    /// with the new image's. A task with no user address space keeps zeros.
    pub(crate) start_code: core::sync::atomic::AtomicUsize,
    pub(crate) end_code: core::sync::atomic::AtomicUsize,
    pub(crate) start_stack: core::sync::atomic::AtomicUsize,
    /// Whether `mlockall(MCL_FUTURE)` is in force: for this group every mapping
    /// made from now on is locked as it is made, which is Linux's
    /// `mm->def_vma_flags`' `VM_LOCKED` bit.
    ///
    /// Measured (`build/scratch/mlk/acct2.c`): `mlockall(MCL_FUTURE)` alone
    /// leaves `VmLck` at 0, then a 4-page `mmap` takes it to `16 kB`. Cleared by
    /// `munlockall`, after which a new `mmap` leaves `VmLck` untouched.
    pub(crate) lock_future: core::sync::atomic::AtomicBool,
}

/// A thread group's memory: the frames behind its mappings, how many threads
/// are still running on it, and where a leaving thread says so.
///
/// A process is one thread group. `fork` makes a new one; `clone(CLONE_VM)`
/// adds a thread to an existing one, and the *last* thread out frees what the
/// group owns - which is why the frames live here and not on the task that
/// happened to allocate them. Linux calls this the `mm`; the page tables
/// themselves stay on the task that owns them, because threading is the only
/// case where two tasks reach the same ones.
impl Mm {
    /// The paging root of the group's address space.
    pub(crate) fn root(&self) -> usize {
        self.root_value
    }

    pub(crate) fn new(leader: TaskId, root: usize, frames: Vec<usize>) -> Arc<Mm> {
        Mm::with_eager(leader, root, frames, Vec::new())
    }

    /// A group whose address space already has eagerly-mapped ranges in it:
    /// what `load_user` laid out, and what a private file `mmap` copied. `fork`
    /// and `clone` are the other callers - a child's address space is its
    /// parent's, so its report is too.
    pub(crate) fn with_eager(
        leader: TaskId,
        root: usize,
        frames: Vec<usize>,
        eager: Vec<MappedRange>,
    ) -> Arc<Mm> {
        Mm::with_image(leader, root, frames, eager, ImageBounds::default())
    }

    /// The same, for a group that is being given a freshly loaded program
    /// image: what `execve` and the two spawn paths make, and the only callers
    /// that have bounds to hand over. A `fork` child inherits its parent's
    /// through [`Mm::set_image_bounds`] instead.
    pub(crate) fn with_image(
        leader: TaskId,
        root: usize,
        frames: Vec<usize>,
        eager: Vec<MappedRange>,
        image: ImageBounds,
    ) -> Arc<Mm> {
        // The high-water marks start at what the loader already placed: the
        // eager ranges it recorded and the frames it filled are this address
        // space's first peak, not something to discover on the first read.
        let page = crate::mm::pframe::PAGE_SIZE;
        let eager_pages: usize = eager
            .iter()
            .map(|range| (range.end.saturating_sub(range.start) + page - 1) / page)
            .sum();
        let held_frames = frames.len();
        Arc::new(Mm {
            live: core::sync::atomic::AtomicUsize::new(1),
            root_value: root,
            frames: Mutex::new(RefCell::new(frames)),
            hwm_frames: core::sync::atomic::AtomicUsize::new(held_frames),
            peak_pages: core::sync::atomic::AtomicUsize::new(eager_pages),
            cmaxrss: core::sync::atomic::AtomicU64::new(0),
            vmas: Mutex::new(RefCell::new(VmaTable::new())),
            eager: Mutex::new(RefCell::new(eager)),
            leader,
            clear_tid: Mutex::new(RefCell::new(None)),
            membarrier: core::sync::atomic::AtomicUsize::new(0),
            robust_head: core::sync::atomic::AtomicUsize::new(0),
            comm_name: [const { core::sync::atomic::AtomicU8::new(0) }; 16],
            credentials: Mutex::new(RefCell::new(crate::cred::Credentials::initial())),
            cpu_utime: core::sync::atomic::AtomicU64::new(0),
            cpu_stime: core::sync::atomic::AtomicU64::new(0),
            cpu_cutime: core::sync::atomic::AtomicU64::new(0),
            cpu_cstime: core::sync::atomic::AtomicU64::new(0),
            nvcsw: core::sync::atomic::AtomicU64::new(0),
            nivcsw: core::sync::atomic::AtomicU64::new(0),
            cnvcsw: core::sync::atomic::AtomicU64::new(0),
            cnivcsw: core::sync::atomic::AtomicU64::new(0),
            start_code: core::sync::atomic::AtomicUsize::new(image.start_code),
            end_code: core::sync::atomic::AtomicUsize::new(image.end_code),
            start_stack: core::sync::atomic::AtomicUsize::new(image.start_stack),
            lock_future: core::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Take another address space's image bounds: what `fork` and
    /// `clone(CLONE_VM)` do, whose child is running the same program from the
    /// same stack. Linux's `copy_mm` copies `start_code`, `end_code` and
    /// `start_stack` with the rest of the `mm`, so a child's report is its
    /// parent's down to these three addresses
    /// (`build/scratch/statcode/inherit2.c`).
    pub(crate) fn set_image_bounds(&self, bounds: ImageBounds) {
        let ordering = Ordering::Relaxed;
        self.start_code.store(bounds.start_code, ordering);
        self.end_code.store(bounds.end_code, ordering);
        self.start_stack.store(bounds.start_stack, ordering);
    }

    /// This address space's image bounds, for a child about to inherit them.
    pub(crate) fn image_bounds(&self) -> ImageBounds {
        let ordering = Ordering::Relaxed;
        ImageBounds {
            start_code: self.start_code.load(ordering),
            end_code: self.end_code.load(ordering),
            start_stack: self.start_stack.load(ordering),
        }
    }

    /// Put a whole VMA table in place: what `fork` hands its child, whose
    /// memory is its parent's - address space included.
    pub(crate) fn set_vmas(&self, vmas: VmaTable) {
        critical_section::with(|cs| *self.vmas.borrow(cs).borrow_mut() = vmas);
        // A child inherits the parent's address space, so its `VmPeak` starts at
        // what the parent has mapped now - not at the loader's eager ranges the
        // constructor saw. Linux's `copy_mm` takes the peak with the table.
        self.record_peak();
    }

    /// A copy of the table, for a child about to be given its own.
    pub(crate) fn vma_snapshot(&self) -> VmaTable {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().clone())
    }

    /// The eagerly-mapped ranges of this address space, folded into the runs a
    /// maps line describes. The folding happens here rather than on the way in
    /// because a mapping can be recorded in pieces (a segment list, a page loop)
    /// and only the reader cares that they are one line.
    pub(crate) fn eager_ranges(&self) -> Vec<MappedRange> {
        merge_ranges(critical_section::with(|cs| {
            self.eager.borrow(cs).borrow().clone()
        }))
    }

    /// Add one eagerly-mapped range, for `/proc/<pid>/maps` to report. Called by
    /// the loader and by a private file `mmap`; see [`MappedRange`] for why
    /// neither put a VMA in the table instead.
    pub(crate) fn record_range(&self, range: MappedRange) {
        critical_section::with(|cs| self.eager.borrow(cs).borrow_mut().push(range));
        self.record_peak();
    }

    /// Drop the parts of eagerly-mapped ranges inside `start..end`, clipped.
    ///
    /// `munmap` takes the description with the pages, and this list is a
    /// description now in more than the reading sense: the occupancy question
    /// reads it too, so a range left behind after its pages were unmapped would
    /// hold an address no program can map again - and `/proc/<pid>/maps` would
    /// keep printing a mapping that is not there.
    pub(crate) fn forget_ranges(&self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        critical_section::with(|cs| {
            let mut ranges = self.eager.borrow(cs).borrow_mut();
            let mut kept: Vec<MappedRange> = Vec::with_capacity(ranges.len() + 1);
            for range in ranges.drain(..) {
                if range.end <= start || range.start >= end {
                    kept.push(range);
                    continue;
                }
                if range.start < start {
                    kept.push(MappedRange {
                        start: range.start,
                        end: start,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
                if range.end > end {
                    kept.push(MappedRange {
                        start: end,
                        end: range.end,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
            }
            *ranges = kept;
        });
    }

    /// `mseal(2)` over the ranges that have no VMA: the program image, the
    /// loader's shared objects, the stack, and a `MAP_PRIVATE` copy of an
    /// ordinary file. The pieces inside `start..end` are split out and marked,
    /// so sealing the text of an image leaves its data neighbour unsealed and
    /// `/proc/<pid>/maps` still prints the runs it printed before - the seal is
    /// a fact about what may happen to a range, not a line of its own.
    pub(crate) fn seal_ranges(&self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        critical_section::with(|cs| {
            let mut ranges = self.eager.borrow(cs).borrow_mut();
            let mut rebuilt: Vec<MappedRange> = Vec::with_capacity(ranges.len() + 2);
            for range in ranges.drain(..) {
                if range.end <= start || range.start >= end {
                    rebuilt.push(range);
                    continue;
                }
                // Left piece, untouched.
                if range.start < start {
                    rebuilt.push(MappedRange {
                        start: range.start,
                        end: start,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
                // The piece inside the seal.
                let lo = range.start.max(start);
                let hi = range.end.min(end);
                rebuilt.push(MappedRange {
                    start: lo,
                    end: hi,
                    prot: range.prot,
                    name: range.name.clone(),
                    sealed: true,
                    locked: range.locked,
                });
                // Right piece, untouched.
                if range.end > end {
                    rebuilt.push(MappedRange {
                        start: end,
                        end: range.end,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
            }
            *ranges = rebuilt;
        });
    }

    /// Whether any eagerly-mapped range inside `start..end` is sealed.
    pub(crate) fn eager_sealed_within(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return false;
        }
        self.eager_ranges()
            .iter()
            .any(|range| range.sealed && range.end > start && range.start < end)
    }

    /// Whether the range `start..end` is sealed by either description: a VMA or
    /// an eager range. The two descriptions are what a mapping can be, so a
    /// seal lives on whichever one describes it and every guard asks both.
    pub(crate) fn sealed_within(&self, start: usize, end: usize) -> bool {
        if self.eager_sealed_within(start, end) {
            return true;
        }
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().sealed_within(start, end))
    }

    /// Give the eagerly-mapped ranges inside `start..end` these permissions.
    ///
    /// `mprotect` rewrites both halves of every other mapping - the VMA and the
    /// pages - and for an eager one the description *is* this list, so a
    /// permission change that skipped it would leave `/proc/<pid>/maps` reading
    /// the old ones for ever. That is not hypothetical: the loader maps a shared
    /// object read-only and then `mprotect`s the part of it that is writable,
    /// which is the case this list was introduced for.
    pub(crate) fn reprot_ranges(&self, start: usize, end: usize, prot: MapFlags) {
        if start >= end {
            return;
        }
        critical_section::with(|cs| {
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                if range.end > start && range.start < end {
                    range.prot = prot;
                }
            }
        });
    }

    /// Carry the eagerly-mapped ranges inside `start..start + len` to
    /// `start + delta`. `mremap` moves the pages; the description has to travel
    /// with them for the reason [`Mm::forget_ranges`] gives - the occupancy
    /// question reads it - and a description left at the old address would both
    /// hold an address nobody owns and leave the new one placeable.
    pub(crate) fn move_ranges(&self, start: usize, end: usize, delta: isize) {
        if start >= end {
            return;
        }
        critical_section::with(|cs| {
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                if range.start >= start && range.end <= end {
                    range.start = range.start.wrapping_add_signed(delta);
                    range.end = range.end.wrapping_add_signed(delta);
                }
            }
        });
    }

    /// Stretch the reported `[stack]` range down to `page`.
    ///
    /// A stack is a region a program descends into rather than a fixed
    /// allocation, so the line `/proc/<pid>/maps` prints has to follow it -
    /// otherwise a program reading its own maps from deep in a call sees a
    /// stack that stopped 64 KiB above where it is standing.
    pub(crate) fn grow_stack_range(&self, page: usize) {
        critical_section::with(|cs| {
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                // The loader adds exactly one range with this name, and nothing
                // else in this list carries a bracketed one.
                if range.name.as_deref() == Some("[stack]") && page < range.start {
                    range.start = page;
                }
            }
        });
    }

    /// Pin every description inside `start..end` (or unpin it), and answer how
    /// many *newly* pinned pages that added.
    ///
    /// The pages are marked on whichever description carries them - a VMA in
    /// the table or an eager range - because those are the two things a
    /// mapping can be here, exactly as for `mseal`. Nothing is faulted in:
    /// this kernel never pages a resident frame out, so a "locked" page and an
    /// ordinary one differ only in what three readers are told
    /// (`/proc/<pid>/status`'s `VmLck`, `smaps`'s `Locked:`, and the refusal a
    /// destructive `madvise` gets).
    ///
    /// The answer is the *difference* in pinned pages, not the range's own
    /// page count: re-locking an already-locked range must report 0 so that a
    /// caller past its `RLIMIT_MEMLOCK` can still repeat itself, which is what
    /// a current host does (`build/scratch/mlk/acct.c`: locking 8 pages twice
    /// reads `VmLck 32 kB` both times, and `ackt2.c`'s sub-range `munlock`
    /// takes off exactly its own 3 pages).
    pub(crate) fn set_locked(&self, start: usize, end: usize, locked: bool) -> usize {
        if start >= end {
            return 0;
        }
        let before = self.locked_pages();
        critical_section::with(|cs| {
            self.vmas
                .borrow(cs)
                .borrow_mut()
                .set_locked(start, end, locked);
            let mut eager = self.eager.borrow(cs).borrow_mut();
            let mut rebuilt: Vec<MappedRange> = Vec::with_capacity(eager.len() + 2);
            for range in eager.drain(..) {
                if range.end <= start || range.start >= end {
                    rebuilt.push(range);
                    continue;
                }
                // The pieces outside the range keep their own bit; only the
                // overlap changes, so a lock of one page of four leaves three
                // alone - the same split `mseal` makes.
                if range.start < start {
                    rebuilt.push(MappedRange {
                        start: range.start,
                        end: start,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
                rebuilt.push(MappedRange {
                    start: range.start.max(start),
                    end: range.end.min(end),
                    prot: range.prot,
                    name: range.name.clone(),
                    sealed: range.sealed,
                    locked,
                });
                if range.end > end {
                    rebuilt.push(MappedRange {
                        start: end,
                        end: range.end,
                        prot: range.prot,
                        name: range.name.clone(),
                        sealed: range.sealed,
                        locked: range.locked,
                    });
                }
            }
            *eager = rebuilt;
        });
        let after = self.locked_pages();
        after.saturating_sub(before)
    }

    /// How many pinned pages lie inside `start..end` in *both* descriptions -
    /// the `count_mm_mlocked_page_nr` half of `do_mlock`'s limit test.
    pub(crate) fn locked_within(&self, start: usize, end: usize) -> usize {
        if start >= end {
            return 0;
        }
        critical_section::with(|cs| {
            let vma_pages = self
                .vmas
                .borrow(cs)
                .borrow()
                .locked_pages_within(start, end);
            let eager_pages: usize = self
                .eager
                .borrow(cs)
                .borrow()
                .iter()
                .filter(|range| range.locked && range.end > start && range.start < end)
                .map(|range| {
                    range
                        .end
                        .min(end)
                        .saturating_sub(range.start.max(start))
                        .div_ceil(pframe::PAGE_SIZE)
                })
                .sum();
            vma_pages + eager_pages
        })
    }

    /// `mlockall(MCL_CURRENT)`: pin every mapping that exists now.
    pub(crate) fn lock_everything(&self) {
        critical_section::with(|cs| {
            self.vmas.borrow(cs).borrow_mut().lock_all();
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                range.locked = true;
            }
        });
    }

    /// `fork`'s half of the pinning rule: a child inherits the *mappings* but
    /// none of the pinning.
    ///
    /// Linux's `dup_mmap` clears `VMA_LOCKED_MASK` on every copied VMA
    /// (`mm/memory.c`, and `mm_init` starts the new `mm` with `locked_vm = 0`
    /// and a fresh `def_vma_flags`), which is why a child of a process that
    /// `mlock`ed 4 pages reads `VmLck 0` and can `munlock` pages it never
    /// pinned without a word - measured (`build/scratch/mlk/fork4.c`): parent
    /// `VmLck 16 kB`, child after `fork` `0`, child's `munlock` `0`, and the
    /// parent still `16`. The same applies to `mlockall(MCL_FUTURE)`
    /// (`build/scratch/mlk/futfork.c`): the child's own `mmap` is not pinned.
    pub(crate) fn clear_locks(&self) {
        self.lock_future.store(false, Ordering::Relaxed);
        critical_section::with(|cs| {
            self.vmas.borrow(cs).borrow_mut().clear_locks();
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                range.locked = false;
            }
        });
    }

    /// `munlockall`: unpin everything, and stop locking whatever is mapped next.
    pub(crate) fn unlock_everything(&self) {
        self.set_lock_future(false);
        critical_section::with(|cs| {
            self.vmas
                .borrow(cs)
                .borrow_mut()
                .set_locked(0, usize::MAX, false);
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                range.locked = false;
            }
        });
    }

    /// How many pages this address space has pinned - Linux's `mm->locked_vm`,
    /// the number `RLIMIT_MEMLOCK` bounds and `VmLck` prints.
    pub(crate) fn locked_pages(&self) -> usize {
        critical_section::with(|cs| {
            let vma_pages = self.vmas.borrow(cs).borrow().locked_pages();
            let eager_pages: usize = self
                .eager
                .borrow(cs)
                .borrow()
                .iter()
                .filter(|range| range.locked)
                .map(|range| (range.end - range.start).div_ceil(pframe::PAGE_SIZE))
                .sum();
            vma_pages + eager_pages
        })
    }

    /// Whether `mlockall(MCL_FUTURE)` is in force for this group.
    pub(crate) fn lock_future(&self) -> bool {
        self.lock_future.load(Ordering::Relaxed)
    }

    /// Set or clear `mlockall`'s `MCL_FUTURE` bit - for this group every
    /// mapping made from now on is locked as it is made.
    pub(crate) fn set_lock_future(&self, on: bool) {
        self.lock_future.store(on, Ordering::Relaxed);
    }

    /// The lowest page the stack currently covers: where growth has reached,
    /// which is the start of the `[stack]` line `/proc/<pid>/maps` prints and
    /// the bound [`resolve_stack_growth`] measures a fault against - Linux's
    /// `vma->vm_start` for the stack, rather than the faulting `sp`. `None`
    /// before a stack has been laid out, which the caller answers with the
    /// region the loader reserved.
    pub(crate) fn stack_floor(&self) -> Option<usize> {
        critical_section::with(|cs| {
            self.eager
                .borrow(cs)
                .borrow()
                .iter()
                .find(|range| range.name.as_deref() == Some("[stack]"))
                .map(|range| range.start)
        })
    }

    /// The VMA covering `virt`, if the group has one.
    pub(crate) fn vma_at(&self, virt: usize) -> Option<Vma> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().lookup(virt).cloned())
    }

    /// Whether a VMA covers `virt`.
    pub(crate) fn vma_covers(&self, virt: usize) -> bool {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().covers(virt))
    }

    /// How many descriptions this process holds - what
    /// [`crate::mm::vma::max_map_count`] bounds.
    pub(crate) fn vma_count(&self) -> usize {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().len())
    }

    pub(crate) fn vma_insert(&self, vma: Vma) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().insert(vma));
        self.record_peak();
    }

    /// Claim a run whose pages the caller has just placed, beside descriptions
    /// it must not rewrite. See [`VmaTable::insert_present`].
    pub(crate) fn vma_insert_present(&self, vma: Vma) {
        critical_section::with(|cs| {
            self.vmas.borrow(cs).borrow_mut().insert_present(vma);
        });
        self.record_peak();
    }

    pub(crate) fn vma_remove_range(&self, start: usize, end: usize) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().remove_range(start, end));
    }

    fn vma_take_range(&self, start: usize, end: usize) -> Vec<Vma> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().take_range(start, end))
    }

    /// Lift the descriptions out without touching their pages. See
    /// [`VmaTable::lift_range`].
    pub(crate) fn vma_lift_range(&self, start: usize, end: usize) -> Vec<Vma> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().lift_range(start, end))
    }

    pub(crate) fn vma_set_prot(&self, start: usize, end: usize, prot: MapFlags) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().set_prot(start, end, prot));
    }

    pub(crate) fn vma_gaps(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut gaps = critical_section::with(|cs| self.vmas.borrow(cs).borrow().gaps(start, end));
        if gaps.is_empty() {
            return gaps;
        }
        // Both sources, because both are mappings. The eager ones - the program
        // image, the loader's shared objects, and a `MAP_PRIVATE` mapping of an
        // ordinary file - carry no VMA by design (nothing may drop their pages:
        // there is no file behind them to refill from), which is exactly why
        // they were easy to forget here. Forgetting them is what let a second
        // mapping be placed *over* one: the hint was checked against the table
        // alone, the address looked free, and a new writable mapping's VMA went
        // over pages another mapping had already filled - read-only, because the
        // first one was. The fault that followed had no resolver, and a program
        // that had only asked for a mapping died instead of it being placed
        // somewhere else, which is what Linux does with an occupied hint.
        for range in self.eager_ranges() {
            let (lo, hi) = (range.start.max(start), range.end.min(end));
            if lo >= hi {
                continue;
            }
            let mut left = Vec::with_capacity(gaps.len() + 1);
            for (gs, ge) in gaps {
                if ge <= lo || gs >= hi {
                    left.push((gs, ge));
                    continue;
                }
                if gs < lo {
                    left.push((gs, lo));
                }
                if ge > hi {
                    left.push((hi, ge));
                }
            }
            gaps = left;
            if gaps.is_empty() {
                break;
            }
        }
        gaps
    }

    /// Whether no VMA describes any part of `start..end`.
    ///
    /// `gaps` answers the whole question in one call, which is the reason this
    /// is a predicate over descriptions rather than a loop over pages: a range
    /// nothing describes is *exactly one* hole, and that hole is the whole of
    /// the range. A range a lazily-mapped VMA covers is not free even though it
    /// has no frames in it - it is still somebody's address, and a second
    /// description over it is what `NOTES.md` §15.18 measured.
    pub(crate) fn vma_range_free(&self, start: usize, end: usize) -> bool {
        let gaps = self.vma_gaps(start, end);
        gaps.len() == 1 && gaps[0] == (start, end)
    }

    /// The `membarrier` registrations this group has made. See
    /// [`Mm::membarrier`].
    pub(crate) fn membarrier_registrations(&self) -> usize {
        self.membarrier.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Record that this process may use `barrier`, leaving any other
    /// registration as it was: a process may register for both kinds, and the
    /// second registration is not a claim that the first never happened.
    pub(crate) fn register_membarrier(&self, barrier: usize) {
        self.membarrier
            .fetch_or(barrier, core::sync::atomic::Ordering::Relaxed);
    }

    /// The robust-futex list head this process registered, `0` for none.
    pub(crate) fn robust_head(&self) -> usize {
        self.robust_head.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Record the robust-futex list head from `set_robust_list`. The last
    /// registration wins, as on Linux: a thread re-registering replaces its
    /// list rather than stacking another.
    pub(crate) fn set_robust_head(&self, head: usize) {
        self.robust_head
            .store(head, core::sync::atomic::Ordering::Relaxed);
    }

    /// The 16-byte name set by `PR_SET_NAME`, all-zero when none was. `comm_of`
    /// falls back to the image basename when the first byte is zero.
    pub(crate) fn comm_name_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        for (slot, out) in self.comm_name.iter().zip(b.iter_mut()) {
            *out = slot.load(core::sync::atomic::Ordering::Relaxed);
        }
        b
    }

    /// Store the `PR_SET_NAME` name: copy the caller's bytes up to the first
    /// NUL or the 16th, zero-filling the tail, so the stored array is always a
    /// NUL-terminated name within `TASK_COMM_LEN`.
    pub(crate) fn set_comm_name(&self, name: &[u8]) {
        for i in 0..16 {
            let byte = match name.get(i).copied() {
                Some(0) | None => 0,
                Some(b) => b,
            };
            self.comm_name[i].store(byte, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The group's credentials, as a snapshot the caller can hold.
    pub(crate) fn credentials(&self) -> crate::cred::Credentials {
        critical_section::with(|cs| *self.credentials.borrow(cs).borrow())
    }

    /// Replace the group's credential state. `capget`/`capset`, the `prctl`
    /// options and the identity rule all funnel through here.
    pub(crate) fn set_credentials(&self, creds: crate::cred::Credentials) {
        critical_section::with(|cs| *self.credentials.borrow(cs).borrow_mut() = creds);
    }

    /// Change one field of the group's credentials in place, so a caller that
    /// only moves a bit still gets the table's lock for the read-modify-write.
    pub(crate) fn with_credentials<R>(
        &self,
        f: impl FnOnce(&mut crate::cred::Credentials) -> R,
    ) -> R {
        critical_section::with(|cs| f(&mut self.credentials.borrow(cs).borrow_mut()))
    }

    /// Add one clock tick of CPU time to the group, split by the ring the tick
    /// interrupted: the core was in this process's user code, or in the kernel
    /// on its behalf. Two atomic increments - safe from the timer interrupt and
    /// allocation-free, as an interrupt handler must be.
    pub(crate) fn charge_tick(&self, in_user: bool) {
        let counter = if in_user {
            &self.cpu_utime
        } else {
            &self.cpu_stime
        };
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// The group's CPU time in internal clock ticks, as `(user, system,
    /// How many pages this group's address space has **mapped**, present or
    /// not - Linux's `mm->total_vm`, which `/proc/<pid>/stat`'s field 23
    /// (`vsize`) prints times the page size and `/proc/<pid>/statm`'s first
    /// field prints plain.
    ///
    /// Two sources, because the space is described by two (see [`MapFacts`]):
    /// the VMA table holds what `mmap`, `mprotect`, `brk` and the stack put
    /// there, and the eagerly-filled ranges hold what the loader laid out with
    /// no VMA behind it. A page counted by neither is a page that no fault can
    /// reach, so the sum is the answer to "how much address space does this
    /// process hold".
    ///
    /// Measured on a current host (`build/scratch/vsize/p.c`, Linux 7.2.14):
    /// mapping 4 MiB with `MAP_ANONYMOUS|MAP_PRIVATE` and touching none of it
    /// moved `/proc/self/statm`'s total from `63` to `1087` pages (and `vsize`
    /// with it) while `resident` stayed `24` - mapping is the claim, presence
    /// is the substance, and the two files report both. This kernel answered
    /// the same probe with `total`/`vsize` equal to the *resident* count, so a
    /// mapping nobody had touched did not exist to any reader of these files
    /// (`docs/notes/` §6.103).
    pub(crate) fn mapped_pages(&self) -> usize {
        let page = pframe::PAGE_SIZE;
        let round = |len: usize| -> usize { (len + page - 1) / page };
        let mut pages = 0;
        // One critical section for both tables: an interrupt between them must
        // not be able to see half a sum, and neither table's borrow is held
        // across the walk's arithmetic (there is none but additions).
        critical_section::with(|cs| {
            for vma in self.vmas.borrow(cs).borrow().iter() {
                pages += round(vma.end.saturating_sub(vma.start));
            }
            for range in self.eager.borrow(cs).borrow().iter() {
                pages += round(range.end.saturating_sub(range.start));
            }
        });
        pages
    }

    /// The pages in each of `status`'s three buckets - Linux's `exec_vm`,
    /// `stack_vm` and `data_vm`, as `(exec, stack, data)`.
    ///
    /// Counted here rather than kept as counters because this kernel has *two*
    /// tables describing one address space ([`Vma`] for what can fault,
    /// [`MappedRange`] for what was filled eagerly) and a counter would need
    /// six mutation sites kept in step with both. Linux can afford a counter
    /// (`vm_stat_account`) because every one of its mappings is one `vm_area`;
    /// here a recount costs what [`Self::mapped_pages`] already costs on the
    /// same two tables, and cannot drift from them.
    ///
    /// The classifier is [`crate::mm::vma::vm_book`], which is where Linux's
    /// three predicates and the measurements behind them live. What a
    /// [`MappedRange`] cannot say is whether it is shared - the field is not on
    /// it - so those ranges are counted as private, which is what both of their
    /// makers produce: the loader's segments and stack, and a `MAP_PRIVATE`
    /// file copy.
    pub(crate) fn vm_books(&self) -> (usize, usize, usize) {
        use crate::mm::vma::VmBook;
        let page = pframe::PAGE_SIZE;
        let round = |len: usize| -> usize { (len + page - 1) / page };
        let mut books = (0, 0, 0);
        critical_section::with(|cs| {
            for vma in self.vmas.borrow(cs).borrow().iter() {
                let shared = matches!(vma.kind, VmaKind::Frames { shared: true, .. });
                let book = crate::mm::vma::vm_book(vma.prot, shared, vma.name.as_deref());
                let pages = round(vma.end.saturating_sub(vma.start));
                match book {
                    VmBook::Exec => books.0 += pages,
                    VmBook::Stack => books.1 += pages,
                    VmBook::Data => books.2 += pages,
                    VmBook::Other => {}
                }
            }
            for range in self.eager.borrow(cs).borrow().iter() {
                let book = crate::mm::vma::vm_book(range.prot, false, range.name.as_deref());
                let pages = round(range.end.saturating_sub(range.start));
                match book {
                    VmBook::Exec => books.0 += pages,
                    VmBook::Stack => books.1 += pages,
                    VmBook::Data => books.2 += pages,
                    VmBook::Other => {}
                }
            }
        });
        books
    }

    /// The high-water marks `/proc/<pid>/status` prints as `VmPeak` and
    /// `VmHWM`: pages, like [`Self::mapped_pages`] and the frames table.
    ///
    /// Both only rise. A read is not a place they can be caught up from either:
    /// a process that maps 4 MiB and unmaps it again between two reads has a
    /// peak the second read still has to report, which is why they are recorded
    /// on the mutation and not computed here.
    pub(crate) fn peak_pages(&self) -> usize {
        self.peak_pages.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn hwm_frames(&self) -> usize {
        self.hwm_frames.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// This group's own resident-set high water in KB - `ru_maxrss`'s own
    /// half. A frame is `PAGE_SIZE` bytes and Linux's field is KB, so the
    /// conversion is a single multiply off the recorded peak, not a second
    /// counter: a process that touched 8 MiB and unmapped it still reads
    /// `8204` (`build/scratch/maxrss/q.c`, Linux 7.2.14), while `/proc`'s
    /// `VmHWM` may dip on the same `munmap` (`build/scratch/maxrss/t.c`).
    pub(crate) fn maxrss_kb(&self) -> u64 {
        let page = crate::mm::pframe::PAGE_SIZE as u64;
        self.hwm_frames.load(core::sync::atomic::Ordering::Relaxed) as u64 * page / 1024
    }

    /// The two books `getrusage` asks about, `(own, reaped-children)`. Linux's
    /// `RUSAGE_SELF`/`RUSAGE_THREAD` read the first (the peak lives on the
    /// process's shared signal struct, so a thread has no separate one -
    /// `build/scratch/maxrss/q.c` reads them equal); `RUSAGE_CHILDREN` reads
    /// the second, the MAX of every child's peak this process has waited for.
    pub(crate) fn maxrss_all(&self) -> (u64, u64) {
        (
            self.maxrss_kb(),
            self.cmaxrss.load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// What a `wait4`/`waitid` `struct rusage` reports for this group: the
    /// greater of its own peak and the peaks of the children it reaped -
    /// Linux's `RUSAGE_BOTH`. Measured (`build/scratch/maxrss/r.c`): a child
    /// whose own peak was ~2060 KB but which reaped an 8 MiB grandchild is
    /// reported by its parent's `wait4` as `8204`, and `u.c`'s reaper with a
    /// 1 MiB own peak and an 8 MiB grandchild is reported `8204` too.
    pub(crate) fn maxrss_both(&self) -> u64 {
        let (own, children) = self.maxrss_all();
        crate::abi::rusage_both(own, children)
    }

    /// Fold one reaped child's `RUSAGE_BOTH` peak into this process's
    /// `RUSAGE_CHILDREN`. It is a MAX, not the CPU/switch pairs' sum: Linux
    /// reports the largest child, and a later smaller child does not lower it
    /// (`build/scratch/maxrss/u.c`: `8204` stays `8204` after a `0` child).
    pub(crate) fn absorb_child_maxrss(&self, child_kb: u64) {
        // `fetch_max` is the atomic form of the leaf's `fold_child_maxrss`:
        // MAX, not a sum, and a later smaller child cannot lower it.
        self.cmaxrss
            .fetch_max(child_kb, core::sync::atomic::Ordering::Relaxed);
    }

    /// Note the address space's size after a mapping was added, for `VmPeak`.
    /// Called from the three insertion points rather than from the reader: the
    /// value has to survive whatever happens between two reads.
    fn record_peak(&self) {
        let pages = self.mapped_pages();
        self.peak_pages
            .fetch_max(pages, core::sync::atomic::Ordering::Relaxed);
    }

    /// child-user, child-system)` - the four `times()` reports and the pair
    /// behind each `getrusage` direction.
    pub(crate) fn cpu_all(&self) -> (u64, u64, u64, u64) {
        (
            self.cpu_utime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_stime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_cutime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_cstime.load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The CPU a `wait`'s `struct rusage` reports for this group, as
    /// `(utime, stime)` in internal clock ticks: its own totals **plus the CPU
    /// its own reaped children folded in**.
    ///
    /// That sum is Linux's `RUSAGE_BOTH` - the direction `getrusage(p,
    /// RUSAGE_BOTH, ...)` is asked for when `wait4` or `waitid` reports a
    /// child - and the reason a grandchild's CPU reaches the child's parent
    /// through the child that reaped it (measured,
    /// `.workbuddy/probe_wait4_rusage.c` scenario D, Linux 7.2.9: 200 ms burnt
    /// by a grandchild reads as 200 ms in the reaping child's `ru_utime`). It is
    /// deliberately *not* the pair a `SIGCHLD` record carries: Linux's
    /// `do_notify_parent` fills `si_utime`/`si_stime` from the group's own
    /// totals with no child term, and the two doors were measured apart.
    pub(crate) fn cpu_both(&self) -> (u64, u64) {
        let (utime, stime, cutime, cstime) = self.cpu_all();
        (utime.saturating_add(cutime), stime.saturating_add(cstime))
    }

    /// Fold a reaped child's CPU into this process's child totals - Linux's
    /// accounting on `wait`: the parent's `cutime` grows by the child's own
    /// `utime` plus the child's own `cutime` (its children, already folded in),
    /// the same for system. Saturating, so no exit can wrap the counter.
    /// Charge one context switch this group's task made - see
    /// [`Self::nvcsw`] for what makes one voluntary.
    ///
    /// Called from the scheduler's one switch point, in interrupt-free context,
    /// so the two `fetch_add`s are the whole of it: no allocation and no lock.
    pub(crate) fn charge_switch(&self, voluntary: bool) {
        let counter = if voluntary { &self.nvcsw } else { &self.nivcsw };
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// The group's switches, `(voluntary, involuntary, children's voluntary,
    /// children's involuntary)` - `getrusage`'s four numbers.
    pub(crate) fn switches_all(&self) -> (u64, u64, u64, u64) {
        (
            self.nvcsw.load(core::sync::atomic::Ordering::Relaxed),
            self.nivcsw.load(core::sync::atomic::Ordering::Relaxed),
            self.cnvcsw.load(core::sync::atomic::Ordering::Relaxed),
            self.cnivcsw.load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The group's own switches *plus its reaped children's*, which is what a
    /// `wait`'s `struct rusage` reports for the child it just collected - the
    /// same shape [`Self::cpu_both`] has for CPU, and for the same reason: a
    /// grandchild's switches reach the parent through the child that reaped it.
    pub(crate) fn switches_both(&self) -> (u64, u64) {
        let (voluntary, involuntary, children_v, children_iv) = self.switches_all();
        (
            voluntary.saturating_add(children_v),
            involuntary.saturating_add(children_iv),
        )
    }

    /// Fold a reaped child's switches into this process's child totals, the way
    /// [`Self::absorb_child`] folds its CPU: the parent's `cnvcsw` grows by the
    /// child's own count plus the child's own child count, so a grandchild
    /// reaches the grandparent through the child that collected it.
    pub(crate) fn absorb_child_switches(&self, child: (u64, u64, u64, u64)) {
        let (voluntary, involuntary, children_v, children_iv) = child;
        self.cnvcsw.fetch_add(
            voluntary.saturating_add(children_v),
            core::sync::atomic::Ordering::Relaxed,
        );
        self.cnivcsw.fetch_add(
            involuntary.saturating_add(children_iv),
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Whether this address space's group has a reaped child's switches to fold
    /// into - the guard the reap path asks before taking the group's books: a
    /// group that never lost a child never pays for the four loads.
    pub(crate) fn has_child_switches(&self) -> bool {
        let (_, _, children_v, children_iv) = self.switches_all();
        children_v != 0 || children_iv != 0
    }

    pub(crate) fn absorb_child(&self, child: (u64, u64, u64, u64)) {
        let (cu, su, ccu, csu) = child;
        self.cpu_cutime.fetch_add(
            cu.saturating_add(ccu),
            core::sync::atomic::Ordering::Relaxed,
        );
        self.cpu_cstime.fetch_add(
            su.saturating_add(csu),
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Swap one frame for another, in place: the copy-on-write path replaces
    /// the page it copied and keeps the group's books straight.
    pub(crate) fn replace(&self, old: usize, new: usize) -> bool {
        critical_section::with(|cs| {
            let mut frames = self.frames.borrow(cs).borrow_mut();
            match frames.iter_mut().find(|held| **held == old) {
                Some(slot) => {
                    *slot = new;
                    true
                }
                None => false,
            }
        })
    }

    /// A frame this group now owns.
    pub(crate) fn own(&self, frame: usize) {
        critical_section::with(|cs| {
            let mut frames = self.frames.borrow(cs).borrow_mut();
            frames.push(frame);
            self.hwm_frames
                .fetch_max(frames.len(), core::sync::atomic::Ordering::Relaxed);
        });
    }

    /// Forget a frame without giving it back: whoever took it owns it now.
    pub(crate) fn disown(&self, frame: usize) {
        critical_section::with(|cs| {
            self.frames
                .borrow(cs)
                .borrow_mut()
                .retain(|&held| held != frame)
        });
    }

    /// Take every frame out, for the last thread's exit to release.
    pub(crate) fn take_frames(&self) -> Vec<usize> {
        critical_section::with(|cs| core::mem::take(&mut *self.frames.borrow(cs).borrow_mut()))
    }

    /// A thread joins the group: one more alive, and it will free nothing.
    pub(crate) fn join(&self) {
        self.live
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// A thread leaves. The answer says whether it was the last, which is the
    /// one that frees what the group owns.
    pub(crate) fn leave(&self) -> bool {
        self.live.fetch_sub(1, core::sync::atomic::Ordering::AcqRel) == 1
    }

    pub(crate) fn leader(&self) -> TaskId {
        self.leader
    }

    pub(crate) fn set_clear_tid(&self, address: usize) {
        critical_section::with(|cs| *self.clear_tid.borrow(cs).borrow_mut() = Some(address));
    }

    pub(crate) fn clear_tid(&self) -> Option<usize> {
        critical_section::with(|cs| *self.clear_tid.borrow(cs).borrow())
    }
}
