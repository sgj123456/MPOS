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
//! The layout is duplicated, field for field, in `boot/src/handoff.rs`. The two
//! crates cannot share the definition - the loader is built for
//! `x86_64-unknown-uefi` and this crate for `x86_64-unknown-none`, and neither
//! can be a dependency of the other - so the pair is kept honest by
//! [`HANDOFF_MAGIC`], which the loader writes and the kernel insists on before
//! it reads anything else. A record written by an older loader fails that check
//! loudly instead of being misread.

use super::{BootInfo, MAX_MEM_REGIONS, MemKind, MemRegion};

/// `"MPOSBOOT"` as little-endian bytes. Spelled out rather than written as a
/// number so the two copies can be compared by eye.
pub const HANDOFF_MAGIC: u64 = u64::from_le_bytes(*b"MPOSBOOT");

/// A memory region as it crosses the boundary. `kind` holds one of the
/// `KIND_*` constants below rather than a Rust enum, because the enum's
/// discriminant is a compiler decision the loader must not have to share.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiRegion {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub reserved: u32,
}

pub const KIND_USABLE: u32 = 0;
pub const KIND_RESERVED: u32 = 1;
pub const KIND_ACPI_RECLAIMABLE: u32 = 2;
pub const KIND_ACPI_NVS: u32 = 3;
pub const KIND_BAD_MEMORY: u32 = 4;
pub const KIND_BOOTLOADER_RECLAIMABLE: u32 = 5;
pub const KIND_KERNEL_AND_MODULES: u32 = 6;
pub const KIND_FRAMEBUFFER: u32 = 7;

/// Everything the loader knows that the kernel needs, in one page.
///
/// Defaulted to all zeroes by the loader before it fills in what it has, so a
/// field it does not set reads as "absent" rather than as a stale address.
#[repr(C)]
pub struct Handoff {
    pub magic: u64,
    /// Add to a physical address to reach it through the kernel's direct map.
    /// The loader chose this and mapped it; the kernel never invents one.
    pub hhdm_offset: u64,
    /// Physical address of the first byte of the loaded kernel image.
    pub kernel_phys_base: u64,
    /// Virtual address the image is mapped at, in the kernel half.
    pub kernel_virt_base: u64,
    /// The initramfs archive, in the direct map. Zero length when none was
    /// loaded, which is the diskless boot.
    pub initramfs_phys: u64,
    pub initramfs_len: u64,
    /// The command line, in the direct map, without a terminator.
    pub cmdline_phys: u64,
    pub cmdline_len: u64,
    /// Direct-map address of the lowest word of the boot stack - the one the
    /// loader wrote its canary into. Zero when the loader planted none, which is
    /// how a handoff from an older loader reads.
    ///
    /// The kernel needs this to check that canary, and it cannot work the
    /// address out for itself: the stack is an allocation of the loader's, and
    /// nothing else records where it began. Its top is in the kernel's initial
    /// `rsp`, but the bottom - the end an overflow reaches last, and the only
    /// place a mark survives being walked over - is only ever known here.
    pub boot_stack_base: u64,
    pub region_count: u64,
    pub regions: [AbiRegion; MAX_MEM_REGIONS],
}

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
        // Compared by eye against `boot/src/handoff.rs`; a loader built from
        // an older copy fails the kernel's check rather than being misread.
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

    /// The loader defaults the record to zeroes, so `Handoff` cannot carry
    /// padding the kernel would have to know about - and a test that spells
    /// out all 3 kB of it once is enough.
    fn zeroed() -> Handoff {
        unsafe { core::mem::zeroed() }
    }
}
