//! What the kernel maps, independent of how any architecture maps it.
//!
//! The page-table *format* belongs to `arch`. The *plan* - which virtual ranges
//! point at which physical ranges, with which permissions - is the same idea on
//! every architecture, so it lives here and is unit-tested on the host. A port
//! consumes a list of [`Mapping`]s and does not get to invent policy.

use alloc::vec::Vec;
use bitflags::bitflags;
use core::fmt;
use core::ops::Range;

use crate::boot::BootInfo;
use crate::mm::pframe::{PAGE_SIZE, align_down, align_up};

bitflags! {
    /// Page permissions. A bit that is absent is a restriction: no `WRITE` means
    /// read-only, no `EXEC` means not executable, no `USER` means kernel-only.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct MapFlags: u8 {
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC  = 1 << 2;
        const USER  = 1 << 3;
    }
}

impl fmt::Debug for MapFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (flag, letter) in [
            (MapFlags::READ, 'r'),
            (MapFlags::WRITE, 'w'),
            (MapFlags::EXEC, 'x'),
        ] {
            write!(f, "{}", if self.contains(flag) { letter } else { '-' })?;
        }
        if self.contains(MapFlags::USER) {
            write!(f, "u")?;
        }
        Ok(())
    }
}

/// One page-granular region of the virtual address space and what backs it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub virt: usize,
    pub phys: usize,
    pub length: usize,
    pub flags: MapFlags,
}

impl Mapping {
    pub fn end(&self) -> usize {
        self.virt + self.length
    }

    pub fn covers(&self, virt: usize) -> bool {
        self.virt <= virt && virt < self.end()
    }

    /// Physical address this mapping gives `virt`, which it must cover.
    pub fn phys_of(&self, virt: usize) -> usize {
        self.phys + (virt - self.virt)
    }

    pub(crate) fn is_page_granular(&self) -> bool {
        self.virt.is_multiple_of(PAGE_SIZE)
            && self.phys.is_multiple_of(PAGE_SIZE)
            && self.length.is_multiple_of(PAGE_SIZE)
    }
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{:016x}..{:016x}) -> {:016x} {:?}",
            self.virt,
            self.end(),
            self.phys,
            self.flags
        )
    }
}

/// Where the linker put the kernel image, as virtual addresses. The architecture
/// port fills this in from its linker symbols.
#[derive(Clone, Debug)]
pub struct KernelSections {
    /// Executable code.
    pub text: Range<usize>,
    /// Constants, including the Limine request structures.
    pub rodata: Range<usize>,
    /// Writable data and BSS, up to the end of the image.
    pub data: Range<usize>,
}

/// Every mapping a kernel address space needs, in installation order.
///
/// The plan is a set of disjoint page-granular ranges: an architecture port can
/// install it entry by entry without ever meeting a page it already mapped.
pub fn kernel_mappings(info: &BootInfo, sections: &KernelSections) -> Vec<Mapping> {
    // The direct map covers *all* physical memory, not just the allocatable
    // parts: the bootloader's stack, its page tables and the framebuffer all have
    // to stay reachable while we are still running on them.
    let direct = MapFlags::READ | MapFlags::WRITE;
    let mut mappings = Vec::new();
    for region in info.regions() {
        let start = align_down(region.base, PAGE_SIZE);
        let end = align_up(region.end(), PAGE_SIZE);
        if end > start {
            mappings.push(Mapping {
                virt: info.phys_to_virt(start),
                phys: start,
                length: end - start,
                flags: direct,
            });
        }
    }

    // Rounding outward makes neighbouring regions collide on any page they share,
    // so fold each entry into its predecessor once the list is ordered. They
    // cannot disagree: every direct-map entry has the same flags, and the virtual
    // address is a fixed offset from the physical one, so a merged run still
    // translates one page to exactly one frame.
    mappings.sort_unstable_by_key(|m| m.virt);
    let mut runs: Vec<Mapping> = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        match runs.last_mut() {
            Some(last) if mapping.virt <= last.end() => {
                last.length = last.end().max(mapping.end()) - last.virt;
            }
            _ => runs.push(mapping),
        }
    }

    // Then the kernel image at its own virtual base, one mapping per section so
    // that nothing ends up both writable and executable. Page-aligning text pulls
    // in the ELF headers, which share its first page.
    let image = [
        (&sections.text, MapFlags::READ | MapFlags::EXEC),
        (&sections.rodata, MapFlags::READ),
        (&sections.data, MapFlags::READ | MapFlags::WRITE),
    ];
    for (range, flags) in image {
        let start = align_down(range.start, PAGE_SIZE);
        let end = align_up(range.end, PAGE_SIZE);
        if end > start {
            runs.push(Mapping {
                virt: start,
                phys: image_phys(info, start),
                length: end - start,
                flags,
            });
        }
    }

    runs
}

/// Physical address of a kernel-image virtual address. The image is loaded
/// contiguously, so the two bases differ by a constant.
fn image_phys(info: &BootInfo, virt: usize) -> usize {
    virt - info.kernel_virt_base + info.kernel_phys_base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::{MAX_MEM_REGIONS, MemKind, MemRegion};

    const HHDM: usize = 0xffff_8000_0000_0000;
    const IMAGE_VIRT: usize = 0xffff_ffff_8000_0000;
    const IMAGE_PHYS: usize = 0x0000_0000_0100_0000;

    /// A memory map shaped like the one QEMU produces, including two regions that
    /// share a page so the direct map has to cope with rounding them outward.
    fn boot_info() -> BootInfo {
        let mut info = BootInfo {
            hhdm_offset: HHDM,
            kernel_phys_base: IMAGE_PHYS,
            kernel_virt_base: IMAGE_VIRT,
            bootloader_name: "host-test",
            bootloader_version: "0",
            initramfs: None,
            regions: [MemRegion {
                base: 0,
                length: 0,
                kind: MemKind::Reserved,
            }; MAX_MEM_REGIONS],
            region_count: 0,
        };

        let regions = [
            (0x1000, 0x51000, MemKind::BootloaderReclaimable),
            (0x52000, 0x9f800, MemKind::Usable),
            (0x9f800, 0xa0000, MemKind::Reserved),
            (IMAGE_PHYS, IMAGE_PHYS + 0x29000, MemKind::KernelAndModules),
        ];
        for (base, end, kind) in regions {
            info.regions[info.region_count] = MemRegion {
                base,
                length: end - base,
                kind,
            };
            info.region_count += 1;
        }
        info
    }

    /// Sections laid out the way the linker script lays them out: page-aligned
    /// boundaries, text starting one header's worth of bytes into the image.
    fn sections() -> KernelSections {
        KernelSections {
            text: IMAGE_VIRT + 0x190..IMAGE_VIRT + 0x4000,
            rodata: IMAGE_VIRT + 0x4000..IMAGE_VIRT + 0x6000,
            data: IMAGE_VIRT + 0x6000..IMAGE_VIRT + 0x29000,
        }
    }

    #[test]
    fn every_mapping_is_page_granular() {
        for mapping in kernel_mappings(&boot_info(), &sections()) {
            assert!(
                mapping.is_page_granular(),
                "{mapping:?} is not page-aligned on all three fields"
            );
            assert!(mapping.length > 0, "{mapping:?} is empty");
        }
    }

    #[test]
    fn nothing_is_both_writable_and_executable() {
        for mapping in kernel_mappings(&boot_info(), &sections()) {
            assert!(
                !mapping.flags.contains(MapFlags::WRITE | MapFlags::EXEC),
                "{mapping:?} violates W^X"
            );
            assert!(
                !mapping.flags.contains(MapFlags::USER),
                "{mapping:?} is reachable from user mode"
            );
        }
    }

    #[test]
    fn no_two_entries_cover_the_same_page() {
        let info = boot_info();
        let mappings = kernel_mappings(&info, &sections());

        // Two regions share a page, so the direct map really did have to merge
        // something; without this the loop below could pass by never overlapping.
        let rounded: Vec<(usize, usize)> = info
            .regions()
            .iter()
            .map(|r| (align_down(r.base, PAGE_SIZE), align_up(r.end(), PAGE_SIZE)))
            .collect();
        let shared = rounded.iter().enumerate().any(|(i, &(start, end))| {
            rounded[i + 1..]
                .iter()
                .any(|&(other, other_end)| start < other_end && other < end)
        });
        assert!(shared, "the memory map no longer shares a page");

        for (i, a) in mappings.iter().enumerate() {
            for b in &mappings[i + 1..] {
                assert!(
                    a.end() <= b.virt || b.end() <= a.virt,
                    "{a:?} and {b:?} overlap"
                );
            }
        }
    }

    #[test]
    fn the_kernel_image_is_mapped_with_per_section_permissions() {
        let mappings = kernel_mappings(&boot_info(), &sections());
        let sections = sections();

        let at = |virt: usize| {
            *mappings
                .iter()
                .find(|m| m.covers(virt))
                .unwrap_or_else(|| panic!("nothing maps {virt:#x}"))
        };

        // Text shares its first page with the ELF headers, so the mapping starts
        // at the image base rather than at __text_start.
        let text = at(sections.text.start);
        assert_eq!(text.virt, IMAGE_VIRT);
        assert_eq!(text.phys, IMAGE_PHYS);
        assert_eq!(text.flags, MapFlags::READ | MapFlags::EXEC);

        assert_eq!(at(sections.rodata.start).flags, MapFlags::READ);

        let data = at(sections.data.start);
        assert_eq!(data.flags, MapFlags::READ | MapFlags::WRITE);
        assert_eq!(data.end(), IMAGE_VIRT + 0x29000);

        // The image is loaded contiguously, so every mapping keeps that offset.
        for mapping in mappings.iter().filter(|m| m.virt >= IMAGE_VIRT) {
            assert_eq!(mapping.virt - IMAGE_VIRT, mapping.phys - IMAGE_PHYS);
        }
    }

    #[test]
    fn the_direct_map_covers_all_physical_memory() {
        let info = boot_info();
        let mappings = kernel_mappings(&info, &sections());

        // Reserved and bootloader memory must be reachable too: the bootloader's
        // stack lives there and we are still running on it.
        for region in info.regions() {
            let page = align_down(region.base, PAGE_SIZE);
            let mapping = mappings
                .iter()
                .find(|m| m.covers(info.phys_to_virt(page)))
                .unwrap_or_else(|| panic!("nothing maps region {region:?}"));
            assert_eq!(mapping.flags, MapFlags::READ | MapFlags::WRITE);
            assert_eq!(mapping.phys_of(info.phys_to_virt(page)), page);
        }
    }
}
