//! The address space the loader hands the kernel: an identity map to keep
//! running in, a direct map for the kernel to allocate out of, and the kernel
//! image aliased into the higher half.
//!
//! Three ranges, three reasons:
//!
//! * **Identity.** The loader is executing at the address the firmware loaded
//!   it at, and so is every page table it is writing. Nothing it does can work
//!   without `virt == phys` for at least the loader's own image, its pool and
//!   its stack, and mapping all of RAM costs a few hundred page tables. The
//!   kernel never sees this range again: the loader jumps to the higher-half
//!   alias, and its own `paging::install` builds a hierarchy with no identity
//!   map in it at all. That is what keeps the kernel image out of the half a
//!   process may map.
//!
//! * **Direct.** `HHDM_OFFSET + phys` for all of RAM, so `mm::init` can write
//!   the frame allocator's metadata through it before any page tables of the
//!   kernel's own exist. Limine handed the kernel a mapping like this; here the
//!   loader has to build it, because the firmware's map covers physical
//!   addresses and nothing else.
//!
//! * **The image, in the higher half.** The kernel is linked as a PIE and
//!   relocated against `KERNEL_VIRT_BASE`, so every absolute address in its data
//!   already points there. The loader maps the same frames at both the address
//!   they were loaded at and the address they were linked for, then jumps to the
//!   second, which is what makes dropping the identity map safe.
//!
//! Everything here runs with the firmware's identity map still active, so a page
//! table is addressed by its physical address and written directly.
//!
//! # Two machines, one plan, one format each
//!
//! The ranges above are the *plan*, and it does not differ between x86_64 and
//! aarch64 - both machines want a direct map, an identity window and a
//! higher-half alias, at the same two addresses. What differs is the descriptor
//! each machine writes to say so, and that is the whole of the split: [`Pool`],
//! [`map_physical_memory`], [`map_kernel_image`], [`is_ram`], [`merge`] and
//! [`pool_bytes_for`] are shared, while [`PageTables`] has one body per machine
//! under `vm/`. The kernel's own `arch/<arch>/paging.rs` is divided the same way
//! and for the same reason.

use alloc::vec::Vec;

use crate::elf::{PAGE_SIZE, Range};
use crate::handoff::Region;

#[cfg(target_arch = "aarch64")]
#[path = "vm/aarch64.rs"]
mod arch;
#[cfg(target_arch = "x86_64")]
#[path = "vm/x86_64.rs"]
mod arch;

pub use arch::PageTables;

/// The registers [`enter`] programs before it branches into the higher half.
///
/// They live in the aarch64 arm because they are that machine's format - one
/// `MAIR_EL1` field per `AttrIndx`, one `TCR_EL1` for both address halves - and
/// they are re-exported here because `main.rs`'s `enter` is the one caller and
/// it should not have to know which module wrote them.
///
/// [`enter`]: crate::enter
#[cfg(target_arch = "aarch64")]
pub use arch::{MAIR, SCTLR_ENABLE, TCR};

/// Add to a physical address to reach it through the direct map.
///
/// Both machines put the kernel half's split at the same place, so one constant
/// serves both: on x86_64 a canonical address has bits 48..=63 equal to bit 47,
/// and on aarch64 `TTBR1` takes exactly the half whose bit 47 is set for a
/// 48-bit virtual address space. The kernel uses the same value for its own
/// `USER_END` check.
pub const HHDM_OFFSET: u64 = 0xffff_8000_0000_0000;

/// Where the kernel image is mapped.
///
/// The top of the address space on both machines - on x86_64 PML4 index 511, on
/// aarch64 just below the end of `TTBR1`'s range - which is the one place
/// nothing else grows into. It sits far above the direct map, which grows up
/// from `HHDM_OFFSET` and would need 64 TiB of RAM to reach here.
pub const KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8000_0000;

/// Frames for page tables, taken from one contiguous allocation and never
/// returned.
///
/// A bump allocator is the whole requirement: tables are created once, during
/// boot, and the address space they describe is never torn down. Running out is
/// a panic and not a `None`, because the size is computed from the size of RAM
/// before the first frame is taken.
pub struct Pool {
    base: u64,
    next: u64,
    end: u64,
}

impl Pool {
    /// # Safety
    ///
    /// `base` must be the start of `size` bytes of writable memory that the
    /// caller has reserved for nothing else and that stays mapped at that
    /// address for as long as this pool is used.
    pub unsafe fn new(base: u64, size: u64) -> Pool {
        Pool {
            base,
            next: base,
            end: base + size,
        }
    }

    /// A zeroed page.
    ///
    /// Zeroed because an empty page-table entry is an absent one: a frame
    /// straight from the firmware still holds whatever the last user left in it,
    /// and any of those bytes with bit 0 set would read as a mapping.
    fn frame(&mut self) -> u64 {
        assert!(
            self.next + PAGE_SIZE <= self.end,
            "the page-table pool is exhausted"
        );
        let frame = self.next;
        self.next += PAGE_SIZE;
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        frame
    }

    /// How many pages have been taken, for the boot log.
    pub fn pages_taken(&self) -> u64 {
        (self.next - self.base) / PAGE_SIZE
    }
}

/// Whether the firmware calls this kind of memory RAM.
///
/// Only these are mapped. Everything else in a firmware map is either a device's
/// registers or a hole, and the two differ in a way that matters: a hole is
/// absent memory and mapping it is merely useless, while a device's registers
/// are a range that answers to hardware and mapping it into the kernel's direct
/// map would put a driver's writes wherever the firmware's own driver had put
/// them. The kernel reaches devices through `mm::map_mmio` and a BAR, which is
/// the only place that knows what it is talking to.
pub fn is_ram(kind: u32) -> bool {
    use crate::handoff::*;
    matches!(
        kind,
        KIND_USABLE
            | KIND_ACPI_RECLAIMABLE
            | KIND_ACPI_NVS
            | KIND_BOOTLOADER_RECLAIMABLE
            | KIND_KERNEL_AND_MODULES
    )
}

/// Map every RAM region at its own address and again through the direct map.
pub fn map_physical_memory(tables: &mut PageTables, regions: &[Region]) -> u64 {
    let mut bytes = 0;
    for region in regions.iter().filter(|r| is_ram(r.kind)) {
        let start = region.base;
        let end = (region.base + region.length).next_multiple_of(PAGE_SIZE);
        let length = end - start;
        bytes += length * 2;
        // The identity map is what the loader is running on.
        tables.map(start, start, length, true, true);
        // The direct map is what the kernel will allocate through.
        tables.map(HHDM_OFFSET + start, start, length, true, true);
    }
    bytes
}

/// Map the loaded image at `KERNEL_VIRT_BASE`, section by section, with the
/// permissions the kernel's own plan gives them.
///
/// Text is executable and read-only, rodata is read-only, and everything from
/// `.data` to the end of `.bss` is writable and not executable. The identity copy
/// the loader copies the file into is, by contrast, mapped writable and
/// executable: the loader has no symbols for the sections it is writing, and the
/// window in which that mapping exists ends at the jump.
pub fn map_kernel_image(
    tables: &mut PageTables,
    phys_base: u64,
    text: Range,
    rodata: Range,
    data: Range,
) {
    let pages = |range: Range| (align_down(range.start), align_up(range.end));
    let (text_start, text_end) = pages(text);
    let (rodata_start, rodata_end) = pages(rodata);
    let (data_start, data_end) = pages(data);

    let mut map = |range: (u64, u64), writable: bool, executable: bool| {
        if range.1 > range.0 {
            tables.map(
                KERNEL_VIRT_BASE + range.0,
                phys_base + range.0,
                range.1 - range.0,
                writable,
                executable,
            );
        }
    };

    map((text_start, text_end), false, true);
    map((rodata_start, rodata_end), false, false);
    map((data_start, data_end), true, false);
}

/// The device window the aarch64 loader maps on top of RAM, or `None` on a
/// machine that maps none.
///
/// x86_64's loader deliberately maps no device range: its kernel reaches MMIO
/// through `mm::map_mmio`, which walks a BAR and a plan, and the loader has no
/// business guessing where a device is. aarch64 has no such path yet - stage 5's
/// device tree is what finds the PL011 and the GIC - so the kernel reaches them
/// at the fixed addresses QEMU's `virt` machine puts them at, and they have to
/// be mapped as Device or the first store to a register is a translation fault
/// the moment the firmware's identity map is replaced.
///
/// The range starts at 0x0800_0000 and not at zero: the low 128 MiB of `virt` is
/// flash and the machine's own vectors, and a null pointer that silently works
/// is the one thing x86_64's own `check_arch` asserts cannot happen.
#[cfg(target_arch = "aarch64")]
pub const DEVICE_BASE: u64 = 0x0800_0000;
/// See [`DEVICE_BASE`]. 64 MiB, from the GIC's distributor to past the
/// virtio-MMIO frames: everything this stage and stage 1c touch.
#[cfg(target_arch = "aarch64")]
pub const DEVICE_WINDOW: u64 = 0x0400_0000;

/// Map the machine's device window, Device and never executable.
#[cfg(target_arch = "aarch64")]
pub fn map_device_window(tables: &mut PageTables) {
    tables.map_device(DEVICE_BASE, DEVICE_BASE, DEVICE_WINDOW);
}

/// Bytes this machine's loader maps beyond RAM, so the page-table pool is sized
/// for everything that will be mapped rather than for RAM alone.
///
/// A device window costs a page table per 2 MiB whether it is RAM or not, and
/// the pool is allocated before the first one is taken.
pub fn extra_mapped_bytes() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        DEVICE_WINDOW
    }
    #[cfg(target_arch = "x86_64")]
    {
        0
    }
}

fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

fn align_up(value: u64) -> u64 {
    value.next_multiple_of(PAGE_SIZE)
}

/// Bytes of page tables needed to map `bytes` of memory.
///
/// Counted, not guessed: a 4 KiB page table holds 512 leaves, a page directory
/// 512 tables, a PDPT 512 directories, and physical memory is mapped twice -
/// once at its own address and once through the direct map. The difference
/// between this and the answer the machine actually uses is the slack at the
/// end, which is there because a region boundary that straddles a table costs
/// a whole extra table and no arithmetic over the total can see where those
/// boundaries fall. Running out is a panic during boot, so the slack is cheap
/// insurance: a few hundred pages against half a gigabyte of RAM.
pub fn pool_bytes_for(bytes: u64) -> u64 {
    let leaves = (bytes / PAGE_SIZE).div_ceil(512);
    let tables = leaves.div_ceil(512) + 1;
    let upper = tables.div_ceil(512) + 1;
    ((leaves + tables + upper) * 2 + 512) * PAGE_SIZE
}

/// Regions a kernel can be handed, sorted and with neighbours of the same kind
/// folded into one.
///
/// The firmware describes RAM in many more pieces than a bootloader does, and
/// `MAX_MEM_REGIONS` is a ceiling on a fixed-size array the kernel keeps. Folding
/// is not a compression trick with a loss: two adjacent usable regions describe
/// exactly the same memory as one that spans both, and folding keeps the direct
/// map from mapping the same pages twice.
pub fn merge(mut regions: Vec<Region>) -> Vec<Region> {
    regions.retain(|r| is_ram(r.kind));
    regions.sort_unstable_by_key(|r| r.base);

    let mut out: Vec<Region> = Vec::with_capacity(regions.len());
    for region in regions {
        match out.last_mut() {
            Some(last) if last.kind == region.kind && last.base + last.length == region.base => {
                last.length += region.length;
            }
            _ => out.push(region),
        }
    }
    out
}
