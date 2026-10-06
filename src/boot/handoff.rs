//! The structure `mpos-boot` leaves for the kernel, and its translation into
//! `boot::BootInfo`.
//!
//! This is the UEFI port's half of the seam `boot/mod.rs` describes: the
//! bootloader knows about firmware protocols, page tables and ELF relocations,
//! and the kernel knows only about the plain-data record below. Nothing in the
//! record is a pointer, so it survives the loader exiting boot services and
//! stops being meaningful the moment the kernel's own allocator starts handing
//! out the frames it names.
//!
//! The *record* is the `mpos-bootinfo` leaf now - `Handoff`, `AbiRegion`, the
//! `KIND_*` constants and `HANDOFF_MAGIC` are re-exported from here, so every
//! `crate::boot::handoff::` caller is unchanged. They moved because the machine
//! layer names `Handoff` too: the aarch64 entry is handed one, and a machine
//! crate cannot depend on the kernel (`PLAN.md` §14 S2a). What stays here is
//! [`collect`], the translation, which is the kernel's: it is the code that
//! decides what a region *means*.
//!
//! The record itself is the `mpos-bootinfo` leaf, and so is this module's
//! re-export of it: `Handoff`, `AbiRegion`, the `KIND_*` constants and
//! `HANDOFF_MAGIC` come from there, so every `crate::boot::handoff::` caller is
//! unchanged. Both sides read one definition - the loader (`mpos-boot`) depends
//! on the same leaf - so the record cannot drift. [`HANDOFF_MAGIC`] is still
//! checked before anything else is read, because a `mpos.elf` written by an
//! older build and left on a partition is a real thing to refuse. What stays
//! here is [`collect`], the translation, which is the kernel's: it is the code
//! that decides what a region *means*.

pub use mpos_bootinfo::{
    AbiRegion, BOOT_STACK_CANARY, HANDOFF_MAGIC, Handoff, KIND_ACPI_NVS, KIND_ACPI_RECLAIMABLE,
    KIND_BAD_MEMORY, KIND_BOOTLOADER_RECLAIMABLE, KIND_FRAMEBUFFER, KIND_KERNEL_AND_MODULES,
    KIND_RESERVED, KIND_USABLE,
};

use super::{BootInfo, MAX_MEM_REGIONS, MemKind, MemRegion};

/// Read the loader's record into a `BootInfo`.
///
/// # Safety
///
/// `at` must point at a `Handoff` the loader wrote, still mapped at the address
/// the loader put it, and every physical range it names must be reachable at
/// `hhdm_offset` for as long as the kernel runs. Both hold for the loader's own
/// record: it is inside the loaded image's data, and the ranges it names are
/// its allocations, which no allocator of the kernel's will ever reclaim.
pub unsafe fn collect(at: *const Handoff) -> BootInfo {
    let raw = unsafe { &*at };
    assert_eq!(
        raw.magic, HANDOFF_MAGIC,
        "mpos-boot handoff has magic {:#x}, not {:#x}",
        raw.magic, HANDOFF_MAGIC
    );
    assert!(
        raw.region_count as usize <= MAX_MEM_REGIONS,
        "mpos-boot handed over {} memory regions, more than the kernel has room for",
        raw.region_count
    );

    let hhdm_offset = raw.hhdm_offset as usize;
    let mut info = BootInfo {
        hhdm_offset,
        kernel_phys_base: raw.kernel_phys_base as usize,
        kernel_virt_base: raw.kernel_virt_base as usize,
        bootloader_name: "mpos-boot",
        bootloader_version: env!("CARGO_PKG_VERSION"),
        initramfs: unsafe { slice(raw.initramfs_phys, raw.initramfs_len, hhdm_offset) },
        regions: [MemRegion {
            base: 0,
            length: 0,
            kind: MemKind::Reserved,
        }; MAX_MEM_REGIONS],
        region_count: raw.region_count as usize,
        cmdline: "",
        boot_stack_base: raw.boot_stack_base as usize,
        dtb: unsafe { slice(raw.dtb_phys, raw.dtb_len, hhdm_offset) },
    };

    for (slot, region) in info.regions[..info.region_count]
        .iter_mut()
        .zip(raw.regions.iter())
    {
        *slot = MemRegion {
            base: region.base as usize,
            length: region.length as usize,
            kind: kind_of(region.kind),
        };
    }

    // The command line is a `&str`, so it has to be validated once here rather
    // than trusted: the bytes came from a file the build wrote, and a build that
    // wrote something else should fail at the assertion, not at every later use.
    if raw.cmdline_len != 0 {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (raw.cmdline_phys as usize + hhdm_offset) as *const u8,
                raw.cmdline_len as usize,
            )
        };
        info.cmdline = core::str::from_utf8(bytes).expect("the boot command line is not UTF-8");
    }

    // The kernel must never be offered the stack it is running on. The record's
    // allocatable regions *are* the frame allocator's zones, so a record that
    // listed the boot stack as allocatable would hand it straight back: the first
    // allocation of that order lands on the stack and overwrites whatever is on
    // it, and the stacks the kernel takes while it is still on that one - the
    // stage-2 context check's sixteen pages, a spawned task's - are exactly that
    // order.
    //
    // The property holds today, and the reason belongs here rather than being
    // left implicit in two other files: `mpos-boot` allocates the stack as UEFI
    // `LOADER_DATA`, and `MemKind::is_allocatable` admits only `Usable` -
    // `BootloaderReclaimable` is excluded *because* it holds the stack the kernel
    // is running on. Measured on the second machine (PLAN.md §13.17): the boot
    // stack at `0x44f07000` lies in a region of kind `BootloaderReclaimable`
    // whose `is_allocatable()` is false, and `mm::pframe` builds its five zones
    // from the record's five `Usable` regions.
    //
    // So this is not a check that the loader carves anything out - it does not,
    // and carving would not change what the allocator sees. It is the contract
    // the kernel is relying on, asked where the record is read: a loader that
    // described the stack as `Usable` stops here, by name, rather than as a
    // mystery inside a boot check later on.
    if info.boot_stack_base != 0 {
        let stack_phys = info.boot_stack_base - info.hhdm_offset;
        let offered = info
            .regions()
            .iter()
            .any(|r| r.kind.is_allocatable() && r.base <= stack_phys && stack_phys < r.end());
        assert!(
            !offered,
            "the handoff offers the boot stack at {stack_phys:#x} as allocatable memory: \
             the kernel would hand back the stack it is running on"
        );
    }

    info
}

/// A physical range as a slice reachable through the direct map, or `None` when
/// the loader loaded nothing.
unsafe fn slice(phys: u64, len: u64, hhdm: usize) -> Option<&'static [u8]> {
    if len == 0 {
        return None;
    }
    let start = (phys as usize).checked_add(hhdm)?;
    Some(unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) })
}

fn kind_of(kind: u32) -> MemKind {
    match kind {
        KIND_USABLE => MemKind::Usable,
        KIND_ACPI_RECLAIMABLE => MemKind::AcpiReclaimable,
        KIND_ACPI_NVS => MemKind::AcpiNvs,
        KIND_BAD_MEMORY => MemKind::BadMemory,
        KIND_BOOTLOADER_RECLAIMABLE => MemKind::BootloaderReclaimable,
        KIND_KERNEL_AND_MODULES => MemKind::KernelAndModules,
        KIND_FRAMEBUFFER => MemKind::Framebuffer,
        // Includes `KIND_RESERVED` and anything a newer loader might add.
        // Underselling a region - calling it reserved when it is really usable -
        // costs memory; overselling one hands the allocator a frame that belongs
        // to something else, so the pessimistic default is the safe one.
        _ => MemKind::Reserved,
    }
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::*;

    /// The loader's `Handoff` is `#[repr(C)]` and its twin lives in another
    /// crate, so the field offsets are the ABI: a field inserted in one copy
    /// and not the other is a machine that boots to a wrong address rather
    /// than a compile error. These are the offsets `boot/src/handoff.rs` has
    /// to agree with.
    #[test]
    fn the_record_lays_out_the_way_the_loader_writes_it() {
        assert_eq!(size_of::<AbiRegion>(), 24);
        assert_eq!(offset_of!(AbiRegion, base), 0);
        assert_eq!(offset_of!(AbiRegion, length), 8);
        assert_eq!(offset_of!(AbiRegion, kind), 16);

        assert_eq!(align_of::<Handoff>(), 8);
        assert_eq!(offset_of!(Handoff, magic), 0);
        assert_eq!(offset_of!(Handoff, hhdm_offset), 8);
        assert_eq!(offset_of!(Handoff, kernel_phys_base), 16);
        assert_eq!(offset_of!(Handoff, kernel_virt_base), 24);
        assert_eq!(offset_of!(Handoff, initramfs_phys), 32);
        assert_eq!(offset_of!(Handoff, initramfs_len), 40);
        assert_eq!(offset_of!(Handoff, cmdline_phys), 48);
        assert_eq!(offset_of!(Handoff, cmdline_len), 56);
        assert_eq!(offset_of!(Handoff, boot_stack_base), 64);
        assert_eq!(offset_of!(Handoff, region_count), 72);
        assert_eq!(offset_of!(Handoff, regions), 80);
        // The two device-tree fields are last on purpose: appending them leaves
        // every offset above this line where an older loader put it, so a record
        // from one is read correctly as far as it goes and reports "no tree"
        // rather than being misread. See the field's own note.
        assert_eq!(
            offset_of!(Handoff, dtb_phys),
            80 + MAX_MEM_REGIONS * size_of::<AbiRegion>()
        );
        assert_eq!(
            offset_of!(Handoff, dtb_len),
            88 + MAX_MEM_REGIONS * size_of::<AbiRegion>()
        );

        // The loader zero-fills one page and writes the record into it, so a
        // record that has grown past a page is a boot that cannot happen.
        assert!(
            size_of::<Handoff>() <= 4096,
            "the handoff record is {} bytes, more than the page it lives in",
            size_of::<Handoff>()
        );
    }

    #[test]
    fn the_magic_is_the_loaders_name_spelled_backwards_in_le_bytes() {
        // Spelled out so a change to the record is caught here rather than at
        // boot; a loader built from an older tree is refused by the kernel's
        // magic check instead of being misread.
        // `M` is the low byte: the value read as a little-endian u64.
        assert_eq!(HANDOFF_MAGIC, 0x544F_4F42_534F_504D);
        assert_eq!(HANDOFF_MAGIC.to_le_bytes(), *b"MPOSBOOT");
    }

    #[test]
    fn every_region_kind_the_loader_names_is_translated() {
        assert_eq!(kind_of(KIND_USABLE), MemKind::Usable);
        assert_eq!(kind_of(KIND_RESERVED), MemKind::Reserved);
        assert_eq!(kind_of(KIND_ACPI_RECLAIMABLE), MemKind::AcpiReclaimable);
        assert_eq!(kind_of(KIND_ACPI_NVS), MemKind::AcpiNvs);
        assert_eq!(kind_of(KIND_BAD_MEMORY), MemKind::BadMemory);
        assert_eq!(
            kind_of(KIND_BOOTLOADER_RECLAIMABLE),
            MemKind::BootloaderReclaimable
        );
        assert_eq!(kind_of(KIND_KERNEL_AND_MODULES), MemKind::KernelAndModules);
        assert_eq!(kind_of(KIND_FRAMEBUFFER), MemKind::Framebuffer);
        // A kind a newer loader invented is reserved, not usable: the cost of
        // being wrong that way is memory, not corruption.
        assert_eq!(kind_of(KIND_FRAMEBUFFER + 1), MemKind::Reserved);
        assert_eq!(kind_of(0xffff_ffff), MemKind::Reserved);
    }

    /// A record the loader never got to fill in - no initramfs, no command
    /// line, no regions - is what a diskless boot looks like, and it has to
    /// read as absent rather than as address zero.
    #[test]
    fn an_empty_record_reads_as_nothing_loaded() {
        let raw = Handoff {
            magic: HANDOFF_MAGIC,
            ..zeroed()
        };
        let info = unsafe { collect(&raw) };
        assert_eq!(info.initramfs, None);
        assert_eq!(info.cmdline, "");
        assert_eq!(info.region_count, 0);
        assert_eq!(info.boot_stack_base, 0);
    }

    /// The command line is the one field whose bytes the kernel reads, so it
    /// is read through the direct map the loader names - here a map of zero,
    /// which makes the physical address the address.
    #[test]
    fn the_command_line_and_the_regions_come_out_of_the_record() {
        static LINE: &[u8] = b"root=/dev/vda2 quiet";

        let mut raw = Handoff {
            magic: HANDOFF_MAGIC,
            hhdm_offset: 0,
            kernel_phys_base: 0x100_000,
            kernel_virt_base: 0xffff_ffff_8000_0000,
            boot_stack_base: 0x200_000,
            cmdline_phys: LINE.as_ptr() as u64,
            cmdline_len: LINE.len() as u64,
            region_count: 2,
            ..zeroed()
        };
        raw.regions[0] = AbiRegion {
            base: 0x100_000,
            length: 0x40_000,
            kind: KIND_KERNEL_AND_MODULES,
            reserved: 0,
        };
        raw.regions[1] = AbiRegion {
            base: 0x1_0000_0000,
            length: 0x1000,
            kind: KIND_FRAMEBUFFER,
            reserved: 0,
        };

        let info = unsafe { collect(&raw) };
        assert_eq!(info.cmdline, "root=/dev/vda2 quiet");
        assert_eq!(info.kernel_phys_base, 0x100_000);
        assert_eq!(info.kernel_virt_base, 0xffff_ffff_8000_0000);
        assert_eq!(info.boot_stack_base, 0x200_000);
        assert_eq!(info.region_count, 2);
        assert_eq!(info.regions[0].base, 0x100_000);
        assert_eq!(info.regions[0].length, 0x40_000);
        assert_eq!(info.regions[0].kind, MemKind::KernelAndModules);
        assert_eq!(info.regions[1].kind, MemKind::Framebuffer);
        // The tail of the array is left alone: only what `region_count` names
        // is a region the loader described.
        assert_eq!(info.regions[2].length, 0);
    }

    /// A loader that loaded no initramfs says so with a zero length rather
    /// than with a null pointer, and the translation has to answer "none"
    /// instead of slicing from wherever the pointer points.
    #[test]
    fn a_zero_length_range_is_absent_not_a_slice_at_zero() {
        let raw = Handoff {
            magic: HANDOFF_MAGIC,
            initramfs_phys: 0xdead_beef,
            initramfs_len: 0,
            ..zeroed()
        };
        assert_eq!(unsafe { collect(&raw) }.initramfs, None);
    }

    /// The device tree crosses the boundary as a physical address and a length,
    /// like the initramfs, and is read back through the direct map the loader
    /// names. A record that carries none - every x86_64 boot - reads as `None`
    /// rather than as an empty slice, because "there is no tree here" and
    /// "there is a tree of zero bytes" are different answers to the kernel.
    #[test]
    fn the_device_tree_comes_out_of_the_record_like_any_other_blob() {
        static TREE: [u8; 8] = [0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0, 0xa0];

        let raw = Handoff {
            magic: HANDOFF_MAGIC,
            dtb_phys: TREE.as_ptr() as u64,
            dtb_len: TREE.len() as u64,
            ..zeroed()
        };
        assert_eq!(unsafe { collect(&raw) }.dtb, Some(&TREE[..]));

        let raw = Handoff {
            magic: HANDOFF_MAGIC,
            dtb_phys: TREE.as_ptr() as u64,
            dtb_len: 0,
            ..zeroed()
        };
        assert_eq!(unsafe { collect(&raw) }.dtb, None);
    }

    /// The shape the loader actually produces: the boot stack inside a
    /// `BootloaderReclaimable` region, which the kernel will not allocate from -
    /// so the record is read and the stack stays where it is. This is the half of
    /// the pair below that says *why* the check is not vacuous rather than merely
    /// that it exists (`PLAN.md` §13.17).
    #[test]
    fn a_record_that_keeps_its_boot_stack_unallocatable_is_read() {
        let mut raw = Handoff {
            magic: HANDOFF_MAGIC,
            hhdm_offset: 0,
            boot_stack_base: 0x4200_0000,
            region_count: 1,
            ..zeroed()
        };
        raw.regions[0] = AbiRegion {
            base: 0x4000_0000,
            length: 0x1000_0000,
            kind: KIND_BOOTLOADER_RECLAIMABLE,
            reserved: 0,
        };

        let info = unsafe { collect(&raw) };
        assert!(!info.regions()[0].kind.is_allocatable());
        assert_eq!(info.boot_stack_base, 0x4200_0000);
    }

    /// A record that lists the boot stack as *allocatable* memory is refused.
    ///
    /// The loader is the only party that knows where its stack is and the kernel
    /// is the only party that knows which kinds it may allocate from, so a
    /// disagreement between them lands here - by name, before the frame allocator
    /// has a zone over the stack the kernel is running on.
    #[test]
    #[should_panic(expected = "offers the boot stack")]
    fn a_record_that_offers_the_boot_stack_is_refused() {
        let mut raw = Handoff {
            magic: HANDOFF_MAGIC,
            hhdm_offset: 0,
            boot_stack_base: 0x4200_0000,
            region_count: 1,
            ..zeroed()
        };
        raw.regions[0] = AbiRegion {
            base: 0x4000_0000,
            length: 0x1000_0000,
            kind: KIND_USABLE,
            reserved: 0,
        };
        let _ = unsafe { collect(&raw) };
    }

    /// The loader defaults the record to zeroes, so `Handoff` cannot carry
    /// padding the kernel would have to know about - and a test that spells
    /// out all 3 kB of it once is enough.
    fn zeroed() -> Handoff {
        unsafe { core::mem::zeroed() }
    }
}
