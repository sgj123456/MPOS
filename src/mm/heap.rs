//! Kernel heap: power-of-two size classes over the physical frame allocator.
//!
//! A slab per class, and each slab is just an intrusive free list - the first
//! word of a free slot points at the next one, so a class needs exactly one
//! `usize` of bookkeeping. When a class runs dry it opens a new run: a block of
//! frames from `mm::alloc_pages`, carved into slots and pushed onto the list.
//! The heap therefore has no fixed size; it grows to fit the machine.
//!
//! Classes are powers of two, which is what makes alignment free: a run starts
//! on a page boundary and every slot is a power-of-two multiple of the class
//! size away from it, so a slot is always aligned to at least its own size.
//! That covers every request whose size *and* alignment fit in a class. Anything
//! bigger - or aligned more strictly - goes straight to the frame allocator and
//! is remembered in a side table, so it can be given back later.
//!
//! Runs are never returned to the frame allocator. Doing that would mean
//! tracking which run each slot came from and whether it is completely empty,
//! and a kernel that has freed a whole run of dentry slots is about to need it
//! again. Waste is bounded by the high-water mark of each class.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use critical_section::Mutex;

use super::pframe::{MAX_ORDER, PAGE_SHIFT, PAGE_SIZE, align_up};
use super::{alloc_pages, free_pages};
use core::cell::RefCell;

/// 16 bytes is the smallest class: it has to hold the free-list link.
const MIN_CLASS: usize = 4;
/// 2 KiB is the largest. Beyond this the frame allocator is cheaper than a slab,
/// since a slot that size wastes half a page on average anyway.
const MAX_CLASS: usize = 11;
const CLASSES: usize = MAX_CLASS - MIN_CLASS + 1;

/// A new run holds at least this many slots, and at least one page.
const MIN_RUN_SLOTS: usize = 32;

/// Over-sized allocations we can remember at once. Bounded because the table is
/// a `static`; 128 concurrent multi-page allocations is far past what the kernel
/// does before M9, when userspace memory stops going through here.
const MAX_LARGE: usize = 128;

fn class_size(class: usize) -> usize {
    1 << (class + MIN_CLASS)
}

/// The class that can satisfy `layout`, or `None` if it must go to the frame
/// allocator. Alignment is folded into the size, so a small but over-aligned
/// request gets a class big enough to guarantee the alignment.
fn class_of(layout: Layout) -> Option<usize> {
    let size = layout.size().max(layout.align()).next_power_of_two();
    let class = size.trailing_zeros() as usize;
    if class > MAX_CLASS {
        return None;
    }
    Some(class.max(MIN_CLASS) - MIN_CLASS)
}

/// Frames needed for `layout`, rounded up to a buddy order.
///
/// The extra `align - 1` pages are the slack that lets the returned pointer be
/// aligned *within* the block: the frame allocator only guarantees alignment
/// relative to its zone, not to an arbitrary power of two.
fn large_order(layout: Layout) -> usize {
    let align_pages = layout.align().div_ceil(PAGE_SIZE).max(1);
    let frames = layout.size().div_ceil(PAGE_SIZE) + align_pages - 1;
    frames.next_power_of_two().trailing_zeros() as usize
}

#[derive(Clone, Copy)]
struct Large {
    /// Address handed to the caller, which may sit inside `block`.
    ptr: usize,
    /// Address the frame allocator actually gave us, and gets back on free.
    block: usize,
    order: usize,
}

impl Large {
    const EMPTY: Large = Large {
        ptr: 0,
        block: 0,
        order: 0,
    };
}

#[derive(Clone, Copy)]
pub struct HeapStats {
    /// Bytes currently handed out to callers.
    pub live: usize,
    /// Bytes sitting in slab free lists, ready without touching the frame
    /// allocator.
    pub cached: usize,
    /// Bytes this heap holds in total, all of it taken from the frame allocator.
    pub mapped: usize,
}

struct Slab {
    /// Free-list head per class; 0 means the class has no free slot.
    heads: [usize; CLASSES],
    /// Free slots per class, so `cached` does not need walking the lists.
    counts: [usize; CLASSES],
    large: [Large; MAX_LARGE],
    large_count: usize,
    live: usize,
    cached: usize,
    mapped: usize,
}

impl Slab {
    const fn empty() -> Slab {
        Slab {
            heads: [0; CLASSES],
            counts: [0; CLASSES],
            large: [Large::EMPTY; MAX_LARGE],
            large_count: 0,
            live: 0,
            cached: 0,
            mapped: 0,
        }
    }

    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        match class_of(layout) {
            Some(class) => self.alloc_slot(class),
            None => self.alloc_large(layout),
        }
    }

    fn alloc_slot(&mut self, class: usize) -> *mut u8 {
        if self.heads[class] == 0 && !self.grow(class) {
            return ptr::null_mut();
        }

        let slot = self.heads[class];
        self.heads[class] = next_of(slot);
        self.counts[class] -= 1;

        let size = class_size(class);
        self.live += size;
        self.cached -= size;
        slot as *mut u8
    }

    /// Take a new run from the frame allocator and thread it into `class`.
    fn grow(&mut self, class: usize) -> bool {
        let size = class_size(class);
        let bytes = (size * MIN_RUN_SLOTS).max(PAGE_SIZE).next_power_of_two();
        let order = (bytes >> PAGE_SHIFT).trailing_zeros() as usize;
        if order > MAX_ORDER {
            return false;
        }
        let Some(base) = alloc_pages(order) else {
            return false;
        };

        // Thread high-to-low so the list hands out ascending addresses.
        let mut next = 0;
        for index in (0..bytes / size).rev() {
            let slot = base + index * size;
            set_next(slot, next);
            next = slot;
        }

        self.heads[class] = next;
        self.counts[class] = bytes / size;
        self.mapped += bytes;
        self.cached += bytes;
        true
    }

    fn alloc_large(&mut self, layout: Layout) -> *mut u8 {
        if self.large_count == MAX_LARGE {
            return ptr::null_mut();
        }
        let order = large_order(layout);
        if order > MAX_ORDER {
            return ptr::null_mut();
        }
        let Some(block) = alloc_pages(order) else {
            return ptr::null_mut();
        };

        let bytes = PAGE_SIZE << order;
        let aligned = align_up(block, layout.align());
        debug_assert!(
            aligned + layout.size() <= block + bytes,
            "aligned pointer escaped its block"
        );

        self.large[self.large_count] = Large {
            ptr: aligned,
            block,
            order,
        };
        self.large_count += 1;
        self.mapped += bytes;
        self.live += bytes;
        aligned as *mut u8
    }

    fn dealloc(&mut self, ptr: usize, layout: Layout) {
        if let Some(class) = class_of(layout) {
            set_next(ptr, self.heads[class]);
            self.heads[class] = ptr;
            self.counts[class] += 1;

            let size = class_size(class);
            self.live -= size;
            self.cached += size;
            return;
        }

        let index = self.large[..self.large_count]
            .iter()
            .position(|entry| entry.ptr == ptr)
            .unwrap_or_else(|| panic!("dealloc of {ptr:#x}: not a live large allocation"));

        // Swap-remove: the table is unordered and searched linearly anyway.
        let entry = self.large[index];
        self.large[index] = self.large[self.large_count - 1];
        self.large_count -= 1;

        let bytes = PAGE_SIZE << entry.order;
        self.mapped -= bytes;
        self.live -= bytes;
        free_pages(entry.block, entry.order);
    }

    fn stats(&self) -> HeapStats {
        HeapStats {
            live: self.live,
            cached: self.cached,
            mapped: self.mapped,
        }
    }
}

fn next_of(slot: usize) -> usize {
    unsafe { (slot as *const usize).read() }
}

fn set_next(slot: usize, next: usize) {
    unsafe { (slot as *mut usize).write(next) };
}

pub struct KernelHeap(Mutex<RefCell<Slab>>);

impl KernelHeap {
    pub const fn empty() -> Self {
        Self(Mutex::new(RefCell::new(Slab::empty())))
    }

    pub fn stats(&self) -> HeapStats {
        critical_section::with(|cs| self.0.borrow(cs).borrow().stats())
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        critical_section::with(|cs| self.0.borrow(cs).borrow_mut().alloc(layout))
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        critical_section::with(|cs| self.0.borrow(cs).borrow_mut().dealloc(ptr as usize, layout))
    }
}
