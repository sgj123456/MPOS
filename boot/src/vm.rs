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

use alloc::vec::Vec;

use crate::elf::{PAGE_SIZE, Range};
use crate::handoff::Region;

/// Add to a physical address to reach it through the direct map.
///
/// PML4 index 256, which is where the architectural split puts the kernel half:
/// a canonical address has bits 48..=63 equal to bit 47, so bit 47 alone decides
/// which half an address is in. The kernel uses the same constant for its own
/// `USER_END` check.
pub const HHDM_OFFSET: u64 = 0xffff_8000_0000_0000;

/// Where the kernel image is mapped.
///
/// PML4 index 511, above the window the kernel reserves for device registers
/// (`mm::MMIO_BASE`, a little under three quarters of a gigabyte below this) and
/// far above the direct map, which grows up from `HHDM_OFFSET` and would need
/// 64 TiB of RAM to reach here.
pub const KERNEL_VIRT_BASE: u64 = 0xffff_ffff_8000_0000;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12..=51 hold the address of the next table or of the page itself.
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

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

/// A four-level hierarchy under construction.
pub struct PageTables {
    root: u64,
    pool: Pool,
}

impl PageTables {
    /// # Safety
    ///
    /// `root` must be a zeroed, writable, identity-mapped frame, and `pool` must
    /// satisfy [`Pool::new`]'s contract.
    pub unsafe fn new(root: u64, pool: Pool) -> PageTables {
        PageTables { root, pool }
    }

    /// The frame the CPU is pointed at to use this hierarchy.
    pub fn root(&self) -> u64 {
        self.root
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Map `len` bytes of physical memory at `virt`, a 4 KiB page at a time.
    ///
    /// Small pages rather than 2 MiB ones because none of the three ranges
    /// starts life aligned to two megabytes: the kernel image's sections are
    /// page-aligned, and the memory map's regions are whatever the firmware
    /// chose. A 2 MiB mapping that overshot into a neighbouring region would
    /// describe memory as present that the caller did not ask for, which for
    /// MMIO is the difference between a driver working and a machine rebooting.
    ///
    /// The cost is page tables, and it is bounded: mapping all of a 512 MiB
    /// machine twice over takes about 2 MiB of them.
    pub fn map(&mut self, virt: u64, phys: u64, len: u64, writable: bool, executable: bool) {
        assert!(
            virt.is_multiple_of(PAGE_SIZE)
                && phys.is_multiple_of(PAGE_SIZE)
                && len.is_multiple_of(PAGE_SIZE)
                && len > 0,
            "map({virt:#x}, {phys:#x}, {len:#x}) is not whole pages"
        );

        let leaf = PRESENT
            | if writable { WRITABLE } else { 0 }
            | if executable { 0 } else { NO_EXECUTE };

        let mut offset = 0;
        while offset < len {
            let v = virt + offset;
            let pml4 = self.root;
            let pdpt = self.child(pml4, index(v, 3));
            let pd = self.child(pdpt, index(v, 2));
            let pt = self.child(pd, index(v, 1));
            unsafe {
                ((pt + index(v, 0) as u64 * 8) as *mut u64).write_volatile(leaf | (phys + offset));
            }
            offset += PAGE_SIZE;
        }
    }

    /// The table an entry points at, creating a zeroed one when the entry is
    /// empty.
    ///
    /// Every level above a leaf is made writable so that a read-only leaf can be
    /// reached through it: on x86_64 a write is refused if *any* level withholds
    /// `WRITABLE`, and the kernel writes its own read-only pages' tables through
    /// the direct map long before it writes the pages themselves.
    fn child(&mut self, table: u64, index: usize) -> u64 {
        let slot = unsafe { &mut *((table + index as u64 * 8) as *mut u64) };
        if *slot & PRESENT != 0 {
            assert!(
                *slot & HUGE == 0,
                "a huge page at {table:#x}[{index}] is in the way of a small one"
            );
            return *slot & ADDRESS_MASK;
        }
        let frame = self.pool.frame();
        *slot = frame | PRESENT | WRITABLE;
        frame
    }
}

/// Which entry of the `level`-th table from the root covers `virt`. Level 3 is
/// the root itself.
fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1ff) as usize
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
