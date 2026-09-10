//! The kernel's own page tables, and the address spaces built alongside them.
//!
//! Limine hands us a working higher-half mapping, but it is Limine's: the tables
//! live in bootloader-reclaimable memory, the permissions are whatever the
//! protocol chose, and there is only ever one address space. So the kernel builds
//! a fresh four-level hierarchy from the plan in [`crate::mm::vm`] and switches
//! to it. What to map is decided there, once, for every port; this file is only
//! the x86_64 encoding of it.
//!
//! On top of that sit [`AddressSpace`]s: one per process, each with its own lower
//! half and a copy of the kernel's upper half. Sharing the upper half is what
//! lets a syscall or an interrupt land in the kernel without a switch, and what
//! lets the mapper below reach these very tables through the direct map.
//!
//! Everything after [`install`] runs on the kernel's own tables, which is what
//! makes W^X, address-space isolation and demand paging possible at all.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size1GiB, Size2MiB, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::boot::BootInfo;
use crate::mm::pframe::align_down;
use crate::mm::vm::{self, MapFlags, Mapping};
use crate::mm::{self, PAGE_SIZE};

/// Bits of a page table entry that matter here. The crate has named constants
/// for these, but the walker below reads raw entries and should not have to
/// round-trip them through a bitflags type that rejects reserved combinations.
const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12..=51 hold the address of the next table or of the page itself.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const ENTRIES: usize = 512;

/// First PML4 index the kernel owns, and the exclusive upper bound of what a
/// process may map. This is not a convention we chose: a canonical address has
/// bits 48..=63 equal to bit 47, so bit 47 alone decides which half an address is
/// in, and bit 47 is index bit 8. The split at 256 is forced by the architecture.
const KERNEL_PML4_START: usize = 256;

/// One past the last address a process may map.
const USER_END: usize = 1 << 47;

/// Physical frame of the kernel's root page table, set by [`install`]. Every
/// address space copies the kernel half out of it.
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Build the kernel's page tables from the plan in [`crate::mm::vm`] and switch
/// the CPU over to them. Returns how many frames the new hierarchy occupies.
///
/// The old tables are not freed. They belong to the bootloader, which is still
/// the only thing that knows how big they were, and the memory they sit in is
/// excluded from the frame allocator anyway.
///
/// # Safety
///
/// `info` must describe live memory, all of it reachable at `info.hhdm_offset`,
/// and `sections` must describe the image that is currently executing. The caller
/// must also be running on a stack the plan maps, because after the switch there
/// is nothing left to fall back on.
pub unsafe fn install(info: &BootInfo, sections: &vm::KernelSections) -> usize {
    assert!(
        nx_supported(),
        "this CPU has no execute-disable bit, so it cannot enforce W^X"
    );

    // Interrupts off for the whole build: a clock interrupt halfway through would
    // run our handlers on tables that describe only part of memory.
    critical_section::with(|_| {
        let root = mm::alloc_frames(0).expect("out of frames for the root page table");
        // The mapper zeroes every table it links in itself, but not the root,
        // which we hand it directly.
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(root) as *mut u8, 0, PAGE_SIZE) };

        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(root) as *mut PageTable),
                VirtAddr::new(info.hhdm_offset as u64),
            )
        };
        let mut pool = FramePool { frames: 0 };
        for mapping in vm::kernel_mappings(info, sections) {
            install_mapping(&mut tables, mapping, &mut pool);
        }

        unsafe {
            // Execute-disable must be on before an entry with bit 63 set is ever
            // loaded; until then that bit is reserved and faults.
            Efer::write(Efer::read() | EferFlags::NO_EXECUTE_ENABLE);
            // Write protection is what makes a read-only page read-only for ring 0
            // too. Without it the kernel could scribble over its own text and
            // rodata and never find out.
            Cr0::update(|flags| flags.insert(Cr0Flags::WRITE_PROTECT));
        }
        KERNEL_ROOT.store(root, Ordering::Relaxed);
        activate(root);

        pool.frames + 1
    })
}

/// Point the CPU at the page table rooted at `root`.
///
/// Writing CR3 flushes every non-global TLB entry, so no stale translation from
/// whatever was loaded before survives this. No entry sets `GLOBAL`, so nothing
/// is left behind either.
fn activate(root: usize) {
    unsafe {
        Cr3::write(
            PhysFrame::from_start_address(PhysAddr::new(root as u64))
                .expect("the frame allocator returned an unaligned root"),
            Cr3Flags::empty(),
        );
    }
}

/// The root frame of the address space the CPU is translating through.
pub fn active_root() -> usize {
    Cr3::read().0.start_address().as_u64() as usize
}

/// The root frame of the kernel's own address space.
pub fn kernel_root() -> usize {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// Leave whatever address space is loaded and return to the kernel's own. Kernel
/// threads run here; only a process has one of its own.
pub fn switch_to_kernel() {
    switch_to_root(kernel_root());
}

/// Point the CPU at the page table rooted at `root` - unless it already is.
///
/// The skip is the point, not an optimization to reconsider: a CR3 write
/// flushes the whole TLB, and the scheduler dispatches task after task, most
/// of them through address spaces that share the kernel half they are about
/// to run in. Flushing on every switch would buy nothing and cost every
/// translation.
pub fn switch_to_root(root: usize) {
    if active_root() != root {
        activate(root);
    }
}

/// One process's view of memory: its own lower half, sharing the kernel's upper
/// half with every other address space.
///
/// Dropping one frees the page tables it owns and nothing else. In particular the
/// frames its mappings point *at* belong to whoever allocated them, so a process's
/// memory outlives its address space and has to be released separately.
pub struct AddressSpace {
    root: usize,
}

impl AddressSpace {
    /// An address space with nothing in it but the kernel half.
    pub fn new() -> AddressSpace {
        let kernel = KERNEL_ROOT.load(Ordering::Relaxed);
        assert_ne!(
            kernel, 0,
            "install() has not run, so there is no kernel half to share"
        );

        let root = mm::alloc_frames(0).expect("out of frames for an address space");
        // A frame straight from the allocator still holds its previous owner's
        // bytes, and any of them with bit 0 set would read as a mapping.
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(root) as *mut u8, 0, PAGE_SIZE) };

        // Share the kernel half by copying the PML4 entries, not by pointing at
        // the same subtables: an entry copied in brings its whole subtree with it,
        // and the subtree is never modified through this address space.
        for index in KERNEL_PML4_START..ENTRIES {
            let entry = entry_at(kernel, index);
            let slot = mm::phys_to_virt(root) + index * 8;
            unsafe { (slot as *mut u64).write_volatile(entry) };
        }

        AddressSpace { root }
    }

    /// The root frame, for callers that have to reason about which address space
    /// is which.
    pub fn root(&self) -> usize {
        self.root
    }

    /// Add one mapping to this address space. It need not be the active one: the
    /// direct map is part of the shared kernel half, so the tables are reachable
    /// either way.
    pub fn map(&mut self, mapping: Mapping) {
        assert!(
            mapping.end() <= USER_END,
            "{mapping:?} reaches into the kernel half, which every address space shares"
        );

        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(self.root) as *mut PageTable),
                VirtAddr::new(mm::hhdm_offset() as u64),
            )
        };
        let mut pool = FramePool { frames: 0 };
        install_mapping(&mut tables, mapping, &mut pool);

        // install_mapping skips the per-page invalidation because it is normally
        // building tables nothing is running on yet. If this one is live, reload
        // CR3 instead and drop the whole TLB in one go.
        if self.root == active_root() {
            activate(self.root);
        }
    }

    /// Make this the address space the CPU translates through.
    ///
    /// Safe because the kernel half is always shared and this code, its stack and
    /// its heap all live there: nothing the caller is running on disappears when
    /// the switch happens.
    pub fn switch(&self) {
        activate(self.root);
    }
}

impl Default for AddressSpace {
    fn default() -> AddressSpace {
        AddressSpace::new()
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Never tear down the hierarchy the CPU is walking. Leave first, then free
        // - the same order a process exit takes.
        if active_root() == self.root {
            switch_to_kernel();
        }

        // Only the lower half is ours. The upper half was copied in and is shared
        // with every other address space, so freeing it would take the kernel down
        // with this one.
        for index in 0..KERNEL_PML4_START {
            let entry = entry_at(self.root, index);
            if entry & PRESENT == 0 {
                continue;
            }
            let child = (entry & ADDR_MASK) as usize;
            free_below(child, 2);
            mm::free_frames(child, 0);
        }
        mm::free_frames(self.root, 0);
    }
}

/// Free every page table hanging off `table`, which sits at `level`.
///
/// Leaf entries are skipped rather than followed: the frame a leaf points at is
/// mapped memory, not a table, and belongs to whoever mapped it.
fn free_below(table: usize, level: u32) {
    if level == 0 {
        return;
    }
    for index in 0..ENTRIES {
        let entry = entry_at(table, index);
        if entry & PRESENT == 0 || entry & HUGE != 0 {
            continue;
        }
        let child = (entry & ADDR_MASK) as usize;
        free_below(child, level - 1);
        mm::free_frames(child, 0);
    }
}

/// What the *active* page tables do with `virt`: the physical address it lands
/// on, and the permissions every level walked agreed to. `None` if unmapped.
pub fn translate(virt: usize) -> Option<(usize, MapFlags)> {
    let (root, _) = Cr3::read();
    let mut table = root.start_address().as_u64() as usize;
    let mut flags = MapFlags::all();

    for level in (0..4).rev() {
        let entry = table_entry(table, virt, level);
        if entry & PRESENT == 0 {
            return None;
        }
        // A page is only as permissive as the strictest level above it.
        if entry & WRITABLE == 0 {
            flags.remove(MapFlags::WRITE);
        }
        if entry & USER == 0 {
            flags.remove(MapFlags::USER);
        }
        if entry & NO_EXECUTE != 0 {
            flags.remove(MapFlags::EXEC);
        }

        let span = 1 << (12 + level * 9);
        if level == 0 || entry & HUGE != 0 {
            let phys = ((entry & ADDR_MASK) | (virt & (span - 1)) as u64) as usize;
            return Some((phys, flags));
        }
        table = (entry & ADDR_MASK) as usize;
    }

    unreachable!("a four-level walk ends in a leaf or in an absent entry")
}

/// Whether every byte of `start..start + len` is mapped for ring 3 in the
/// *active* address space, and writable too when `write` is set.
///
/// This is what makes a user pointer trustworthy: the kernel reaches user
/// memory through the same tables the user does, so a syscall that takes an
/// address from user mode walks every page it touches and refuses the ones
/// that are unmapped or kernel-half. It runs inside the syscall gate, where
/// the active tables are still the caller's.
pub fn user_accessible(start: usize, len: usize, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let mut page = align_down(start, PAGE_SIZE);
    while page < end {
        let Some((_, flags)) = translate(page) else {
            return false;
        };
        if !flags.contains(MapFlags::USER) || (write && !flags.contains(MapFlags::WRITE)) {
            return false;
        }
        page += PAGE_SIZE;
    }
    true
}

/// One entry of a page table, read through the direct map.
fn entry_at(table: usize, index: usize) -> u64 {
    let entry = mm::phys_to_virt(table) + index * 8;
    // Volatile: this is hardware-visible state another actor could change, and the
    // read must not be hoisted out of the walk or cached against a later one.
    unsafe { (entry as *const u64).read_volatile() }
}

/// The entry of the table at `table` that `virt` passes through at `level`.
fn table_entry(table: usize, virt: usize, level: u32) -> u64 {
    entry_at(table, (virt >> (12 + level * 9)) & 0x1ff)
}

/// Whether the CPU has the execute-disable bit: leaf 0x80000001, EDX bit 20.
fn nx_supported() -> bool {
    // Extended leaves always exist on a CPU that got us into long mode.
    core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 20) != 0
}

/// Install one entry of the plan, using the largest page size that fits.
///
/// Bigger pages are not just smaller tables: one TLB entry then covers a whole
/// gigabyte of the direct map instead of a single 4 KiB slice of it.
fn install_mapping(tables: &mut OffsetPageTable<'_>, mapping: Mapping, pool: &mut FramePool) {
    // Not redundant with the plan's own tests: a length that is not a whole number
    // of pages leaves a remainder no arm below can match, and the loop would spin
    // forever instead of failing.
    assert!(
        mapping.is_page_granular(),
        "{mapping:?} cannot be split into whole pages"
    );

    let flags = page_flags(mapping.flags);
    let mut virt = mapping.virt;
    let mut phys = mapping.phys;
    let mut left = mapping.length;

    while left > 0 {
        // Largest first; the 4 KiB arm always matches, so this cannot stall.
        for size in [Size1GiB::SIZE, Size2MiB::SIZE, Size4KiB::SIZE] {
            let size = size as usize;
            if left >= size && virt.is_multiple_of(size) && phys.is_multiple_of(size) {
                map_chunk(tables, virt, phys, flags, size, pool);
                virt += size;
                phys += size;
                left -= size;
                break;
            }
        }
    }
}

fn map_chunk(
    tables: &mut OffsetPageTable<'_>,
    virt: usize,
    phys: usize,
    flags: PageTableFlags,
    size: usize,
    pool: &mut FramePool,
) {
    match size as u64 {
        Size1GiB::SIZE => map_one::<Size1GiB, _>(tables, virt, phys, flags, pool),
        Size2MiB::SIZE => map_one::<Size2MiB, _>(tables, virt, phys, flags, pool),
        _ => map_one::<Size4KiB, _>(tables, virt, phys, flags, pool),
    }
}

fn map_one<S, M>(
    tables: &mut M,
    virt: usize,
    phys: usize,
    flags: PageTableFlags,
    pool: &mut FramePool,
) where
    S: PageSize + fmt::Debug,
    M: Mapper<S>,
{
    let page = Page::<S>::from_start_address(VirtAddr::new(virt as u64)).expect("aligned page");
    let frame =
        PhysFrame::<S>::from_start_address(PhysAddr::new(phys as u64)).expect("aligned frame");

    let result = unsafe { tables.map_to(page, frame, flags, pool) };
    match result {
        // These tables are not active yet, so there is nothing in the TLB to
        // invalidate; writing CR3 drops all of it at once.
        Ok(flush) => flush.ignore(),
        // The plan is disjoint, so any of these means the plan or the walker that
        // built it is wrong, and continuing would install a lie.
        Err(error) => panic!(
            "cannot map {virt:#x} -> {phys:#x} as {}: {error:?}",
            S::DEBUG_STR
        ),
    }
}

/// The x86_64 encoding of a [`MapFlags`].
///
/// `READ` has no bit of its own: a present entry is always readable. `GLOBAL` is
/// deliberately left off - it would let kernel pages survive a CR3 write, which
/// is worth having once address spaces switch often, but only once nothing needs
/// flushing on the way out.
fn page_flags(flags: MapFlags) -> PageTableFlags {
    let mut out = PageTableFlags::PRESENT;
    if flags.contains(MapFlags::WRITE) {
        out |= PageTableFlags::WRITABLE;
    }
    if flags.contains(MapFlags::USER) {
        out |= PageTableFlags::USER_ACCESSIBLE;
    }
    if !flags.contains(MapFlags::EXEC) {
        out |= PageTableFlags::NO_EXECUTE;
    }
    out
}

/// Hands the mapper frames from the kernel's allocator, counting what it takes so
/// the caller can report the cost of the hierarchy.
struct FramePool {
    frames: usize,
}

unsafe impl FrameAllocator<Size4KiB> for FramePool {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let phys = mm::alloc_frames(0)?;
        self.frames += 1;
        Some(
            PhysFrame::from_start_address(PhysAddr::new(phys as u64))
                .expect("the frame allocator returned an unaligned frame"),
        )
    }
}
