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
const COW_BIT: u64 = 1 << 9; // Available bit on x86_64 (not used by hardware)
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
/// Map a device's registers into the kernel half, which every address space
/// shares.
///
/// The bootloader's direct map covers physical memory and nothing else, and a
/// PCI BAR is not memory - it is a range of addresses that answers to a
/// device. Reaching one takes a mapping of its own, and it has to go in the
/// kernel half because a device is not per-process: [`AddressSpace::map`]
/// refuses to touch that half, correctly, and this is the one path that does.
///
/// `virt` is chosen by the caller and must already be free; `phys` and `len`
/// come from the BAR. Device registers are mapped a page at a time rather than
/// with huge pages, because a BAR is not obliged to be aligned to one and
/// because a 2 MiB mapping of them would reach past the device into whatever
/// sits beside it.
pub fn map_mmio(virt: usize, phys: usize, len: usize) {
    assert!(
        virt >= USER_END && virt.is_multiple_of(crate::mm::pframe::PAGE_SIZE),
        "an MMIO window belongs above the user half, page-aligned"
    );
    assert!(len > 0, "mapping a device's registers of zero length");

    let root = kernel_root();
    let mut tables = unsafe {
        OffsetPageTable::new(
            &mut *(mm::phys_to_virt(root) as *mut PageTable),
            VirtAddr::new(mm::hhdm_offset() as u64),
        )
    };
    let mut pool = FramePool { frames: 0 };
    install_mapping(
        &mut tables,
        Mapping {
            virt,
            phys,
            length: len,
            // Deliberately without `USER`: a device's registers must not be
            // reachable from ring 3, whatever else a process manages to map.
            flags: MapFlags::READ | MapFlags::WRITE,
        },
        &mut pool,
    );
    // The tables are live the moment they are written, but the TLB is not, and
    // a stale entry for one of these addresses is the worst kind of stale: it
    // would send a device's read to whatever was there before.
    activate(root);
}

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

    /// Add one 4KiB user mapping, reporting a page that is already taken
    /// instead of panicking.
    ///
    /// [`map`] asserts because the kernel's own plan is built once and is
    /// disjoint by construction. A process's request is not: `MAP_FIXED` on an
    /// address that already holds something is an ordinary thing for a libc to
    /// do - musl's mallocng claims the guard page of its heap that way - so a
    /// syscall handler has to be able to *answer* such a request, not take the
    /// machine down with it.
    pub fn try_map(&mut self, mapping: Mapping) -> Result<(), PageTaken> {
        assert!(
            mapping.length == PAGE_SIZE
                && mapping.virt.is_multiple_of(PAGE_SIZE)
                && mapping.phys.is_multiple_of(PAGE_SIZE),
            "{mapping:?} is not a single whole page"
        );
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
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(mapping.virt as u64))
            .expect("aligned page");
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(mapping.phys as u64))
            .expect("aligned frame");
        match unsafe { tables.map_to(page, frame, page_flags(mapping.flags), &mut pool) } {
            Ok(flush) => flush.ignore(),
            Err(_) => return Err(PageTaken),
        }

        if self.root == active_root() {
            activate(self.root);
        }
        Ok(())
    }

    /// Take back every 4KiB mapping that covers `virt..virt + len`, handing the
    /// frames they pointed at back to the caller.
    ///
    /// Nothing is freed here, for the same reason [`Drop`] frees no frames: a
    /// frame may already be on a list its owner keeps for the reaper, and
    /// freeing it twice would hand one frame to two owners. Cleared entries
    /// read as unmapped to the CPU and to [`phys_for_virt`] alike.
    pub fn unmap(&mut self, virt: usize, len: usize) -> alloc::vec::Vec<usize> {
        let mut dropped = alloc::vec::Vec::new();
        if len == 0 {
            return dropped;
        }
        assert!(
            virt.checked_add(len).is_some_and(|end| end <= USER_END),
            "unmapping {len:#x} bytes at {virt:#x} reaches the kernel half"
        );

        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(self.root) as *mut PageTable),
                VirtAddr::new(mm::hhdm_offset() as u64),
            )
        };
        for page in pages_of(virt, len).step_by(PAGE_SIZE) {
            let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(page as u64))
                .expect("aligned page");
            if let Ok((frame, flush)) = tables.unmap(page) {
                // The whole hierarchy is refreshed by the CR3 write below when
                // this is the live address space, so a per-page flush is waste.
                flush.ignore();
                dropped.push(frame.start_address().as_u64() as usize);
            }
        }

        if self.root == active_root() {
            activate(self.root);
        }
        dropped
    }

    /// Rewrite the permissions of the mappings that already cover
    /// `virt..virt + len`, leaving their frames where they are.
    ///
    /// This is what makes `mprotect` mean something: `PROT_NONE` is mapped but
    /// unreachable from ring 3, not unmapped, so the bytes survive a round trip
    /// back to readable. Pages that are not mapped are skipped - there is
    /// nothing to protect, and Linux would answer `ENOMEM` for the whole range.
    pub fn protect(&mut self, virt: usize, len: usize, flags: MapFlags) {
        if len == 0 {
            return;
        }
        assert!(
            virt.checked_add(len).is_some_and(|end| end <= USER_END),
            "protecting {len:#x} bytes at {virt:#x} reaches the kernel half"
        );

        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(self.root) as *mut PageTable),
                VirtAddr::new(mm::hhdm_offset() as u64),
            )
        };
        for page in pages_of(virt, len).step_by(PAGE_SIZE) {
            let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(page as u64))
                .expect("aligned page");
            // An unmapped page is simply skipped: the walk fails and there is
            // nothing to re-flag.
            // Safety: the permissions of a live mapping may only change behind
            // the CPU's back if the TLB is dropped afterwards, which the CR3
            // write below does for the active address space. For any other
            // space nothing is translating through these tables at all.
            let _ = unsafe { tables.update_flags(page, page_flags(flags)) };
        }

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

    /// Map a page as copy-on-write: present and readable from ring 3, but
    /// not writable. The COW bit marks it for the page fault handler.
    ///
    /// Only a page that is *logically writable* gets the COW bit. A read-only
    /// page - a program's text, a `PROT_READ` mapping - is shared as it stands:
    /// neither process may write it, so there is nothing to copy, and marking
    /// it COW would quietly turn a write that should fault into a private copy.
    ///
    /// A page that came from a walk of an already-forked space carries `COW`
    /// instead of `WRITE`: it is writable, just not written yet. Sharing it
    /// without re-marking it would leave the new space with a page that is
    /// read-only and unresolvable.
    pub fn map_cow(&mut self, virt: usize, phys: usize, flags: MapFlags) {
        let writable = flags.contains(MapFlags::WRITE) || flags.contains(MapFlags::COW);
        // Remove WRITE; the COW fault handler will restore it on copy.
        let mut cow_flags = flags;
        cow_flags.remove(MapFlags::WRITE);

        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(self.root) as *mut PageTable),
                VirtAddr::new(mm::hhdm_offset() as u64),
            )
        };

        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virt as u64))
            .expect("aligned page for COW");
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(phys as u64))
            .expect("aligned frame for COW");

        let mut flags = page_flags(cow_flags);
        if writable {
            flags |= PageTableFlags::from_bits_truncate(COW_BIT);
        }

        match unsafe { tables.map_to(page, frame, flags, &mut FramePool { frames: 0 }) } {
            Ok(flush) => flush.ignore(),
            Err(_) => panic!("COW map failed at {virt:#x}"),
        }

        if self.root == active_root() {
            activate(self.root);
        }
    }

    /// Resolve a copy-on-write fault: allocate a fresh frame, copy the contents,
    /// update the mapping to writable (clearing the COW bit), and report both
    /// frames.
    ///
    /// The pair matters because the frame lists have to follow along: the caller
    /// now owns `new` instead of `old`, and `old` has one fewer owner. Returns
    /// `None` if the page is not COW.
    pub fn resolve_cow(&mut self, virt: usize) -> Option<(usize, usize)> {
        let aligned_virt = crate::mm::pframe::align_down(virt, crate::mm::PAGE_SIZE);

        // Read the current PTE to find the physical frame and check COW.
        let entry = self.read_pte(aligned_virt)?;
        if entry & COW_BIT == 0 {
            return None;
        }
        let old_phys = (entry & ADDR_MASK) as usize;

        // Allocate a fresh frame and copy.
        let new_phys = mm::alloc_frames(0)?;
        let src = unsafe {
            core::slice::from_raw_parts(
                mm::phys_to_virt(old_phys) as *const u8,
                crate::mm::PAGE_SIZE,
            )
        };
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                mm::phys_to_virt(new_phys) as *mut u8,
                crate::mm::PAGE_SIZE,
            )
        };
        dst.copy_from_slice(src);

        // Re-map with WRITE, without COW bit.
        let mut flags = MapFlags::READ | MapFlags::WRITE | MapFlags::USER;
        if entry & NO_EXECUTE != 0 {
            // Preserve NX: if the original had no-execute, keep it off the
            // user-visible flags (it maps to !EXEC).
        } else {
            flags |= MapFlags::EXEC;
        }
        // If the original was not user-accessible, don't make it so.
        if entry & USER == 0 {
            flags.remove(MapFlags::USER);
        }

        self.remap_page(aligned_virt, new_phys, flags);

        Some((old_phys, new_phys))
    }

    /// Read the raw PTE entry for a virtual address by walking the page tables.
    /// Returns the raw entry value, or `None` if any level is not present.
    fn read_pte(&self, virt: usize) -> Option<u64> {
        let mut table = self.root;
        for level in (0..4).rev() {
            let entry = table_entry(table, virt, level);
            if entry & PRESENT == 0 {
                return None;
            }
            if level == 0 || entry & HUGE != 0 {
                return Some(entry);
            }
            table = (entry & ADDR_MASK) as usize;
        }
        None
    }

    /// Re-map a single 4KiB page: remove the old mapping and install a new one
    /// with different flags/physical address. Used by `resolve_cow` and will be
    /// used by `exec_replace`.
    fn remap_page(&mut self, virt: usize, new_phys: usize, flags: MapFlags) {
        let mut tables = unsafe {
            OffsetPageTable::new(
                &mut *(mm::phys_to_virt(self.root) as *mut PageTable),
                VirtAddr::new(mm::hhdm_offset() as u64),
            )
        };

        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virt as u64))
            .expect("aligned page for remap");

        // Unmap the old entry (we don't care about the old frame here — the
        // caller manages refcounts).
        if let Ok((_, flush)) = tables.unmap(page) {
            flush.ignore();
        }

        // Map the new frame.
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(new_phys as u64))
            .expect("aligned frame for remap");
        match unsafe { tables.map_to(page, frame, page_flags(flags), &mut FramePool { frames: 0 }) }
        {
            Ok(flush) => flush.ignore(),
            Err(_) => panic!("remap_page failed at {virt:#x}"),
        }

        if self.root == active_root() {
            activate(self.root);
        }
    }

    /// Walk every user-space 4KiB page in the lower half and call `f` with
    /// `(virt, phys, flags)` for each one. Used by fork to duplicate the
    /// address space.
    pub fn walk_user_pages(&self, mut f: impl FnMut(usize, usize, MapFlags)) {
        self.walk_level(self.root, 3, 0, &mut f);
    }

    /// Set the COW bit on the PTE for `virt`. The page must already be mapped.
    /// Used by fork after `protect` strips WRITE: the PTE still has the old
    /// flags, and this adds the COW marker.
    pub fn set_cow_bit(&self, virt: usize) {
        let aligned_virt = crate::mm::pframe::align_down(virt, crate::mm::PAGE_SIZE);
        let mut table = self.root;
        for level in (0..4).rev() {
            let offset = (aligned_virt >> (12 + level * 9)) & 0x1ff;
            let entry_ptr = mm::phys_to_virt(table) + offset * 8;
            let entry = unsafe { (entry_ptr as *mut u64).read_volatile() };
            if entry & PRESENT == 0 {
                return;
            }
            if level == 0 || entry & HUGE != 0 {
                // Set the COW bit and clear WRITE.
                let new = (entry | COW_BIT) & !WRITABLE;
                unsafe { (entry_ptr as *mut u64).write_volatile(new) };
                if self.root == active_root() {
                    activate(self.root);
                }
                return;
            }
            table = (entry & ADDR_MASK) as usize;
        }
    }

    fn walk_level(
        &self,
        table: usize,
        level: u32,
        base: usize,
        f: &mut impl FnMut(usize, usize, MapFlags),
    ) {
        for index in 0..ENTRIES {
            let entry = entry_at(table, index);
            if entry & PRESENT == 0 {
                continue;
            }
            let addr = base | (index << (12 + level * 9));

            // Only walk the lower half (user space).
            if addr >= USER_END {
                continue;
            }

            if level == 0 || entry & HUGE != 0 {
                // Leaf: this is a mapped page.
                let phys = (entry & ADDR_MASK) as usize;
                let mut flags = MapFlags::empty();
                if entry & WRITABLE != 0 {
                    flags |= MapFlags::WRITE;
                }
                if entry & USER != 0 {
                    flags |= MapFlags::USER;
                }
                if entry & NO_EXECUTE == 0 {
                    flags |= MapFlags::EXEC;
                }
                // Report a copy-on-write page as writable-but-protected, not as
                // simply read-only. A caller that reads only `WRITE` still sees
                // the page as it stands; one that is duplicating the space - and
                // `fork` is the only one - needs the marker, or it would share
                // the frame a second time without protecting it again and the
                // next write would fault with nothing to resolve it.
                if entry & COW_BIT != 0 {
                    flags |= MapFlags::COW;
                }
                f(addr, phys, flags);
            } else {
                // Non-leaf: descend.
                let child = (entry & ADDR_MASK) as usize;
                self.walk_level(child, level - 1, addr, f);
            }
        }
    }

    /// Translate a user virtual address through *this* address space's page
    /// tables (not the active CR3) and return the physical frame it maps to.
    /// Used by the ELF loader to apply relocations through the direct map.
    pub fn phys_for_virt(&self, virt: usize) -> Option<usize> {
        let mut table = self.root;
        for level in (0..4).rev() {
            let entry = table_entry(table, virt, level);
            if entry & PRESENT == 0 {
                return None;
            }
            let span = 1 << (12 + level * 9);
            if level == 0 || entry & HUGE != 0 {
                return Some(((entry & ADDR_MASK) | (virt & (span - 1)) as u64) as usize);
            }
            table = (entry & ADDR_MASK) as usize;
        }
        None
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
    // The four permission bits only. `COW` is a state the fault handler cares
    // about, not something the CPU enforces, and starting from `all()` would
    // report every page as copy-on-write.
    let mut flags = MapFlags::PERM;

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

/// A page a syscall asked to map was already spoken for. The kernel's own plan
/// asserts on that instead (see [`AddressSpace::map`]): a collision there is a
/// bug in the plan, while one here is an ordinary request from a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageTaken;

/// The pages that cover `virt..virt + len`, lowest first and rounded
/// outwards: a range that starts mid-page takes that whole page with it, which
/// is what `munmap` and a `MAP_FIXED` replacement do to the pages they touch.
fn pages_of(virt: usize, len: usize) -> core::ops::Range<usize> {
    let first = align_down(virt, PAGE_SIZE);
    let end = virt
        .checked_add(len)
        .map_or(first, |end| crate::mm::pframe::align_up(end, PAGE_SIZE));
    first..end
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
        if !flags.contains(MapFlags::USER) {
            return false;
        }
        // A page the process is entitled to write counts as writable whether or
        // not its write bit is set. A copy-on-write page is mapped read-only on
        // purpose: the write it catches is answered by the fault handler, not
        // refused, so the permission is real and only the page's *state* says
        // otherwise. Reading the bit and stopping there would turn every syscall
        // that fills a user buffer - `read`, `uname`, `getrandom`, the status
        // word of a `wait` - into an `EFAULT` for any process that has forked,
        // which is every process a shell starts.
        if write && !flags.contains(MapFlags::WRITE) && !is_cow_page(page) {
            return false;
        }
        page += PAGE_SIZE;
    }
    true
}

/// Write one word to a user address, in the address space `root` names.
///
/// The scheduler gives an exited thread's memory back on its own stack, which
/// is to say while the CPU translates through somebody else's tables. A
/// `CLONE_CHILD_CLEARTID` address cannot be written straight from there: the
/// write lands in whichever process happens to be current - or, when that
/// process does not map the address at all, faults in ring 0 and takes the
/// machine down. Linux never meets this shape because its exit path runs in
/// the dying process's own context; this kernel has to load the space the
/// address belongs to before it can touch it.
///
/// A page the space will not let be written is left alone rather than forced:
/// a process that made its clear-tid word read-only loses the wake that word
/// would carry, not the machine.
pub fn write_user_u32(root: usize, at: usize, value: u32) -> bool {
    if at == 0 || at.checked_add(4).is_none_or(|end| end > USER_END) {
        return false;
    }
    // Switching CR3 mid-function is safe here and only here: both halves of
    // the kernel - its code, its stack, its heap - are mapped in every space,
    // so nothing this function runs on disappears when the switch happens.
    let saved = active_root();
    activate(root);
    let ok = user_accessible(at, 4, true);
    if ok {
        unsafe { core::ptr::write_volatile(at as *mut u32, value) };
    }
    activate(saved);
    ok
}

/// Whether the page holding `virt` carries the copy-on-write bit: present, and
/// marked as shared-until-written.
///
/// Only the leaf entry is consulted, because only the leaf is where the bit is
/// ever set: [`AddressSpace::map_cow`] and its sibling `set_cow_bit` both write
/// it there, and the fault handler reads it back from the same place.
pub fn is_cow_page(virt: usize) -> bool {
    let (root, _) = Cr3::read();
    let mut table = root.start_address().as_u64() as usize;
    for level in (0..4).rev() {
        let entry = table_entry(table, virt, level);
        if entry & PRESENT == 0 {
            return false;
        }
        if level == 0 || entry & HUGE != 0 {
            return entry & COW_BIT != 0;
        }
        table = (entry & ADDR_MASK) as usize;
    }
    false
}

/// The raw leaf entry the *active* address space holds for `virt`, if the
/// walk reaches one.
///
/// Diagnostic only: when a fault's answer depends on what a page's bits say,
/// the bits are worth more than a boolean about them.
pub fn leaf_pte(virt: usize) -> Option<u64> {
    let (root, _) = Cr3::read();
    let mut table = root.start_address().as_u64() as usize;
    for level in (0..4).rev() {
        let entry = table_entry(table, virt, level);
        if entry & PRESENT == 0 {
            return None;
        }
        if level == 0 || entry & HUGE != 0 {
            return Some(entry);
        }
        table = (entry & ADDR_MASK) as usize;
    }
    None
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
