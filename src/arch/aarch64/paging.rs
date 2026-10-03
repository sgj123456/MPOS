//! The aarch64 page tables: the format, and the kernel's own tables.
//!
//! Four levels of 512 entries, a 4 KiB granule and two halves: `TTBR0` covers
//! the low virtual half and `TTBR1` the high one, which is the aarch64 division
//! of the address space. A *user* address space is one `TTBR0` and the kernel's
//! is one `TTBR1` — which is why this file owns both.
//!
//! The descriptor is where aarch64 and x86_64 differ, and it is the only place
//! they do: the mapping *plan* is `mm::vm`'s (`Mapping`, `MapFlags`,
//! `kernel_mappings`), and this file is the format that plan is written in.
//!
//! # What stage 1d does and does not have
//!
//! The bring-up path loads the image at a physical address and runs with the MMU
//! off, so this stage builds and installs tables *from a fixed layout* and turns
//! the MMU on. Two things are deliberately not here, and both are named in
//! `PLAN.md` §9.1 rather than implied:
//!
//! * **The layout is built from constants, not from `kernel_mappings`.** That
//!   function takes a `BootInfo`, and the `-kernel` path has none: QEMU hands
//!   over no memory map and no device tree (`x0` is zero — measured at stage
//!   1b). The frame allocator is not up for the same reason. Stage 1e's loader
//!   supplies the record; until then the layout below is honest for one machine
//!   and says so.
//! * **The kernel still runs at its physical address.** The image is *linked*
//!   low, so every absolute address in it is a physical one, and mapping those
//!   pages is what the low half of the identity map is for. The higher-half
//!   alias is built and checked here — it is what stage 1e's loader will jump to
//!   — but execution moves there only when the loader also relocates the image's
//!   absolute addresses, which is the loader's job and not something a
//!   trampoline can fake.
//!
//! # Why the tables are addressed by their physical address
//!
//! Every walk below reads and writes a table through a *physical* address, and
//! that is not an approximation: this stage runs with the low half
//! identity-mapped, so a table's physical address is also one the CPU can write.
//! Stage 1e's loader keeps the same rule for the same reason — it runs before
//! its tables exist.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::mm::vm::{KernelSections, MapFlags, Mapping};

/// 4 KiB granule, matching `mm`'s.
const PAGE_SIZE: usize = 4096;
/// A level-2 block: the granule at 2 MiB, which is what the large ranges use.
const BLOCK_SIZE: usize = 2 * 1024 * 1024;
const ENTRY_COUNT: usize = 512;

/// Where the `-kernel` path loads the image's first byte: the linker script's
/// `executable_start`, and the base every higher-half alias is computed from.
pub const IMAGE_LOAD_BASE: usize = 0x4008_0000;

/// Where the kernel image is mapped in the high half.
///
/// The same value as x86_64's `KERNEL_VIRT_BASE`, and for the same reason: the
/// top of the address space is the one place nothing else grows into. It is a
/// valid `TTBR1` address for a 48-bit virtual address space, whose high half
/// starts at `0xffff_0000_0000_0000`.
pub const KERNEL_VIRT_BASE: usize = 0xffff_ffff_8000_0000;

/// Add to a physical address to reach it through the direct map.
///
/// x86_64's value again, and again for the same reason: a mid-`TTBR1` address
/// the image and the device window cannot collide with. Stage 1e takes it from
/// the loader's record instead of from this constant.
pub const HHDM_OFFSET: usize = 0xffff_8000_0000_0000;

/// The distance between the address the image is loaded at and the address it
/// is mapped at in the high half. One constant, so the alias of any address is
/// an addition rather than a second layout.
const HIGH_OFFSET: usize = KERNEL_VIRT_BASE - IMAGE_LOAD_BASE;

/// The lowest physical address of this machine's RAM (QEMU virt).
pub const RAM_BASE: usize = 0x4000_0000;
/// How much of it this stage maps. One gigabyte, which is more than the 256 MiB
/// the bring-up command line gives QEMU and is therefore a *ceiling* rather than
/// a claim: mapping memory that is not there costs a page-table entry and
/// nothing else, and the code that would care — the frame allocator — arrives
/// with stage 1e's record.
const RAM_WINDOW: usize = 0x4000_0000;

/// The device window: 64 MiB from the GIC's distributor to past the virtio-mmio
/// frames, which is everything this stage and stage 1c touch.
///
/// It deliberately does not start at zero. The low 128 MiB is flash and the
/// machine's own vectors at address 0, and mapping address 0 would give the
/// kernel a null pointer that silently works — the one thing x86_64's own check
/// asserts cannot happen (`check_arch`: "the null page is unmapped").
const DEVICE_BASE: usize = 0x0800_0000;
const DEVICE_WINDOW: usize = 0x0400_0000;

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

const VALID: u64 = 1 << 0;
/// Set in a table descriptor and in a level-3 page descriptor, clear in a block
/// descriptor. This bit is what distinguishes them, which is why the block
/// builder does not set it.
const TABLE: u64 = 1 << 1;
/// `AttrIndx`, bits 4:2: which `MAIR_EL1` field says how to cache this.
const ATTR_SHIFT: u64 = 2;
/// `AP`, bits 7:6: which exception levels may read and write.
const AP_SHIFT: u64 = 6;
/// Inner shareable, bits 9:8. This machine is uniprocessor, but shareability is
/// what makes a mapping usable through the core's caches at all.
const SH_INNER: u64 = 3 << 8;
/// `AF`, bit 10. Without it every access faults, which is the most common way a
/// hand-built table fails.
const AF: u64 = 1 << 10;
/// `PXN`, bit 53: privileged execute never.
const PXN: u64 = 1 << 53;
/// `UXN`, bit 54: unprivileged execute never.
const UXN: u64 = 1 << 54;

/// `AP[2:1]`. `AP[2]` is the read-only bit; `AP[1]` says whether EL0 may reach
/// the page at all.
const AP_EL1_RO: u64 = 2 << AP_SHIFT;
const AP_EL1_RW: u64 = 0 << AP_SHIFT;
const AP_EL0_RO: u64 = 3 << AP_SHIFT;
const AP_EL0_RW: u64 = 1 << AP_SHIFT;
/// The read-only bit alone, for reading a descriptor back.
const AP_READ_ONLY: u64 = 2 << AP_SHIFT;
/// The EL0-accessible bit alone.
const AP_USER: u64 = 1 << AP_SHIFT;

/// Output address masks. A table descriptor and a level-3 page hold bits 47:12;
/// a level-2 block holds 47:21 and a level-1 block 47:30, which is what makes
/// their output addresses aligned to their own granularity.
const ADDR_PAGE: u64 = 0x0000_ffff_ffff_f000;
const ADDR_BLOCK2: u64 = 0x0000_ffff_ffe0_0000;
const ADDR_BLOCK1: u64 = 0x0000_ffff_c000_0000;

/// `MAIR_EL1`: three of its eight attribute fields, in the order `AttrIndx`
/// above indexes them.
///
/// * 0 — Device-nGnRnE. What MMIO must be: without it a store to a device
///   register can be reordered or merged, which for a PL011 is bytes arriving
///   out of order.
/// * 1 — Normal, non-cacheable. The attribute a framebuffer wants.
/// * 2 — Normal, inner and outer write-back, read/write allocate. RAM.
const MAIR_DEVICE_BYTE: u64 = 0x00;
const MAIR_NORMAL_NC_BYTE: u64 = 0x44;
const MAIR_NORMAL_BYTE: u64 = 0xff;
const ATTR_DEVICE: u64 = 0;
const ATTR_NORMAL_NC: u64 = 1;
const ATTR_NORMAL: u64 = 2;
/// The register itself: each field eight bits wide, at the index above. Written
/// from the same constants the descriptors index with, so the two cannot drift
/// apart into a page that asks for an attribute the register does not define.
const MAIR: u64 = (MAIR_DEVICE_BYTE << (8 * ATTR_DEVICE))
    | (MAIR_NORMAL_NC_BYTE << (8 * ATTR_NORMAL_NC))
    | (MAIR_NORMAL_BYTE << (8 * ATTR_NORMAL));

/// `TCR_EL1` for a 48-bit virtual address space with a 4 KiB granule on both
/// halves: `T0SZ`/`T1SZ` 16, both walks enabled, both inner-shareable
/// write-back, `IPS` 40 bits (1 TiB of physical, which covers this machine).
const TCR: u64 = 16
    | (16 << 16)
    | (3 << 8)
    | (3 << 10)
    | (3 << 12)
    | (3 << 24)
    | (3 << 26)
    | (3 << 28)
    | (2 << 30);

/// `SCTLR_EL1` bits to turn on: `M` (the MMU), `A` (alignment checking), `C`
/// (data cache) and `I` (instruction cache).
///
/// The other bits are read-modify-written rather than assigned: many are RES1,
/// and a constant that gets one wrong is a machine that faults before it can say
/// why. Alignment checking is on deliberately — an unaligned access is a bug
/// this machine can report, and leaving it off to avoid the report is how a port
/// ends up with a copy that only works by accident.
const SCTLR_ENABLE: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 12);

/// The root table's physical address once installed. Zero until [`install`].
static ROOT: AtomicUsize = AtomicUsize::new(0);

/// Page tables, taken from one static arena and never returned.
///
/// A bump allocator is the whole requirement, exactly as in the loader's
/// `Pool`: the tables are built once at boot and the address space they describe
/// is never torn down. There is no frame allocator to draw from yet (stage 1e),
/// so the arena is a `.bss` array; running out is a panic rather than a `None`,
/// because the size the layout needs is known before the first table is taken.
const ARENA_TABLES: usize = 32;
const ARENA_BYTES: usize = ARENA_TABLES * PAGE_SIZE;

#[repr(C, align(4096))]
struct Arena([u8; ARENA_BYTES]);

static ARENA: Arena = Arena([0; ARENA_BYTES]);

struct Tables {
    /// Byte offset into the arena of the next free table.
    next: usize,
    count: usize,
}

fn entry_slot(table: usize, index: usize) -> *mut u64 {
    (table + index * 8) as *mut u64
}

fn read_entry(table: usize, index: usize) -> u64 {
    // SAFETY: `table` is a table this module allocated from the arena and
    // `index` is below 512; the arena is identity-mapped, so its physical
    // address is one the CPU can read.
    unsafe { core::ptr::read_volatile(entry_slot(table, index)) }
}

fn write_entry(table: usize, index: usize, value: u64) {
    // SAFETY: as `read_entry`.
    unsafe { core::ptr::write_volatile(entry_slot(table, index), value) };
}

impl Tables {
    fn new() -> Tables {
        Tables { next: 0, count: 0 }
    }

    /// A fresh, zeroed table, by physical address.
    fn alloc(&mut self) -> usize {
        assert!(
            self.next + PAGE_SIZE <= ARENA_BYTES,
            "the page-table arena is full: {} tables of {ARENA_TABLES}",
            self.count
        );
        let table = &raw const ARENA as usize + self.next;
        self.next += PAGE_SIZE;
        self.count += 1;
        // The arena is `.bss` and so starts zeroed — but a table handed back a
        // second time would not be, and clearing here is what makes "a fresh
        // table is empty" true by construction rather than by luck.
        unsafe { core::ptr::write_bytes(table as *mut u8, 0, PAGE_SIZE) };
        table
    }

    /// The table below `parent`'s `index`, made if it is not there.
    fn next_table(&mut self, parent: usize, index: usize) -> usize {
        let entry = read_entry(parent, index);
        if entry & VALID != 0 {
            // A block where a table belongs means the layout asked for two
            // mappings that overlap at different granularities. That is a bug in
            // the layout, and silently overwriting the block would turn it into
            // a mapping that translates to the wrong frame instead.
            assert!(
                entry & TABLE != 0,
                "a block descriptor where a table was expected: \
                 the layout overlaps at two granularities"
            );
            return (entry & ADDR_PAGE) as usize;
        }
        let table = self.alloc();
        write_entry(parent, index, table as u64 | VALID | TABLE);
        table
    }

    fn walk_to(&mut self, virt: usize, level: usize) -> usize {
        let mut table = ROOT.load(Ordering::Relaxed);
        assert!(table != 0, "the tables are not installed");
        for current in 0..level {
            table = self.next_table(table, index_of(virt, current));
        }
        table
    }

    /// Map one 2 MiB block.
    fn map_block(&mut self, virt: usize, phys: usize, attrs: u64) {
        assert_eq!(virt % BLOCK_SIZE, 0, "a block mapping is not aligned");
        assert_eq!(phys % BLOCK_SIZE, 0, "a block mapping is not aligned");
        let table = self.walk_to(virt, 2);
        write_entry(
            table,
            index_of(virt, 2),
            (phys as u64 & ADDR_BLOCK2) | VALID | attrs,
        );
    }

    /// Map one 4 KiB page.
    fn map_page(&mut self, virt: usize, phys: usize, attrs: u64) {
        let table = self.walk_to(virt, 3);
        write_entry(
            table,
            index_of(virt, 3),
            (phys as u64 & ADDR_PAGE) | VALID | TABLE | attrs,
        );
    }

    /// Map `virt..virt + length` in 2 MiB blocks. A zero length maps nothing,
    /// which is what lets the runs around the image be empty.
    fn map_blocks(&mut self, virt: usize, phys: usize, length: usize, attrs: u64) {
        let mut offset = 0;
        while offset < length {
            self.map_block(virt + offset, phys + offset, attrs);
            offset += BLOCK_SIZE;
        }
    }

    /// Map `virt..virt + length` a page at a time, taking each page's
    /// attributes from `attrs_of` — its *physical* address, so that a run that
    /// is one section of the image and one stretch of ordinary RAM can be one
    /// call.
    fn map_run(
        &mut self,
        virt: usize,
        phys: usize,
        length: usize,
        attrs_of: &dyn Fn(usize) -> u64,
    ) {
        let mut offset = 0;
        while offset < length {
            self.map_page(virt + offset, phys + offset, attrs_of(phys + offset));
            offset += PAGE_SIZE;
        }
    }
}

/// The index of `virt` in the table at `level`, where level 0 is the root.
fn index_of(virt: usize, level: usize) -> usize {
    (virt >> (12 + 9 * (3 - level))) & (ENTRY_COUNT - 1)
}

fn align_down(value: usize, alignment: usize) -> usize {
    value & !(alignment - 1)
}

fn align_up(value: usize, alignment: usize) -> usize {
    align_down(value + alignment - 1, alignment)
}

/// The descriptor bits a `MapFlags` asks for.
///
/// The two "never" bits are what make W^X a fact rather than a convention: a
/// page is executable by exactly one of the two exception levels, and a page
/// that is not executable is not executable from either. A writable page is
/// therefore never executable, which is the whole rule.
fn attributes(flags: MapFlags) -> u64 {
    let user = flags.contains(MapFlags::USER);
    let exec = flags.contains(MapFlags::EXEC);
    let write = flags.contains(MapFlags::WRITE);
    let ap = match (user, write) {
        (false, false) => AP_EL1_RO,
        (false, true) => AP_EL1_RW,
        (true, false) => AP_EL0_RO,
        (true, true) => AP_EL0_RW,
    };
    let mut descriptor = AF | SH_INNER | (ATTR_NORMAL << ATTR_SHIFT) | ap;
    if !exec || user {
        descriptor |= PXN;
    }
    if !exec || !user {
        descriptor |= UXN;
    }
    descriptor
}

/// [`attributes`] with the memory attribute field replaced: the same permissions
/// on a page that is device registers rather than RAM.
fn device_attributes(flags: MapFlags) -> u64 {
    (attributes(flags) & !(0b111 << ATTR_SHIFT)) | (ATTR_DEVICE << ATTR_SHIFT)
}

/// The attributes of the page at physical address `phys`, given where the
/// linker put the image: its sections get their own permissions, and every other
/// page of RAM is writable memory.
///
/// Every section boundary is compared *page-aligned*, and the start of text is
/// why: the ELF headers sit in the same page as the first byte of code, so a
/// page-by-page comparison against the unaligned `__text_start` would call the
/// kernel's own first page ordinary writable RAM. x86_64's plan page-aligns the
/// same range for the same reason (`mm::vm::kernel_mappings`: "page-aligning
/// text pulls in the ELF headers, which share its first page").
fn image_page_attributes(phys: usize, sections: &KernelSections) -> u64 {
    let page = align_down(phys, PAGE_SIZE);
    if page >= align_down(sections.text.start, PAGE_SIZE) && page < sections.text.end {
        attributes(MapFlags::READ | MapFlags::EXEC)
    } else if page >= sections.rodata.start && page < sections.rodata.end {
        attributes(MapFlags::READ)
    } else {
        attributes(MapFlags::READ | MapFlags::WRITE)
    }
}

/// Build the kernel's tables and switch the machine onto them.
///
/// Answers how many tables the layout needed — the one number that says the
/// layout was built rather than assumed, and that a layout which suddenly needs
/// three times as many tables has changed shape.
///
/// # Safety
///
/// Runs once, at boot, on the bring-up path: the machine is executing at
/// `IMAGE_LOAD_BASE` with the MMU off, and after this returns it is executing on
/// the tables it just built.
pub unsafe fn install(sections: &KernelSections) -> usize {
    let mut tables = Tables::new();
    ROOT.store(tables.alloc(), Ordering::Relaxed);

    // The device window, identity-mapped and Device. It is what keeps the
    // console and the clock working across the switch: with the MMU off every
    // access is device-ordered, and a translation that made them Normal would
    // let the core cache a PL011's status register.
    tables.map_blocks(
        DEVICE_BASE,
        DEVICE_BASE,
        DEVICE_WINDOW,
        device_attributes(MapFlags::READ | MapFlags::WRITE),
    );

    // The low half of RAM, around the image. Two block runs with the image's own
    // 2 MiB window mapped a page at a time in between — the runs have to stop at
    // a block boundary, because a block descriptor and a table cannot both
    // describe the same 2 MiB.
    let image_start = align_down(sections.text.start, BLOCK_SIZE);
    let image_end = align_up(sections.data.end, BLOCK_SIZE);
    // The walk below is page by page, and both the alias and the direct map
    // reuse the same page count; the linker script aligns every section
    // boundary to a page, which is what makes that true. Asserted with the
    // plan's own predicate, so the two machines check the same invariant.
    let window = Mapping {
        virt: image_start,
        phys: image_start,
        length: image_end - image_start,
        flags: MapFlags::READ | MapFlags::WRITE,
    };
    assert!(
        window.is_page_granular(),
        "the image window is not page-granular: {window:?}"
    );
    tables.map_blocks(
        RAM_BASE,
        RAM_BASE,
        image_start - RAM_BASE,
        attributes(MapFlags::READ | MapFlags::WRITE),
    );
    tables.map_run(image_start, image_start, image_end - image_start, &|phys| {
        image_page_attributes(phys, sections)
    });
    let ram_end = RAM_BASE + RAM_WINDOW;
    tables.map_blocks(
        image_end,
        image_end,
        ram_end - image_end,
        attributes(MapFlags::READ | MapFlags::WRITE),
    );

    // The high half: the direct map the kernel allocates through, and the image
    // at the address the loader will jump to. Neither is used for execution yet
    // (the image's absolute addresses are still the physical ones), and both are
    // what stage 1e replaces these constants with.
    tables.map_blocks(
        HHDM_OFFSET + RAM_BASE,
        RAM_BASE,
        RAM_WINDOW,
        attributes(MapFlags::READ | MapFlags::WRITE),
    );
    tables.map_run(
        image_start + HIGH_OFFSET,
        image_start,
        image_end - image_start,
        &|phys| image_page_attributes(phys, sections),
    );

    let count = tables.count;
    // SAFETY: the caller's contract. The root is the table just built, and every
    // mapping the machine needs to keep executing is in it: the low half has the
    // devices and the image at the addresses the kernel is currently using, and
    // the high half has the direct map and the alias.
    unsafe { enable_mmu(ROOT.load(Ordering::Relaxed)) };
    count
}

/// Turn the MMU on, on `root`.
///
/// The order is the architecture's and every step is load-bearing: `MAIR` and
/// `TCR` before the base registers, because a translation can only happen once
/// all three agree; `dsb` so the table writes are visible to the walker; `tlbi
/// vmalle1` because the walker may have cached the *failed* lookups it made
/// before the tables existed; and `isb` after the `SCTLR_EL1` write so the
/// instruction after it is fetched with translation on.
unsafe fn enable_mmu(root: usize) {
    // SAFETY: the caller passes the root of a complete table set, and this is
    // the only place in the kernel that enables the MMU.
    //
    // `{sctlr}` is written by the `mrs` before either of the two instructions
    // that read it, which is why it is a `lateout` rather than an `inout` with a
    // value nothing uses: `SCTLR_EL1` has RES1 bits, and the value the
    // architecture put there is the one being modified.
    unsafe {
        core::arch::asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {root}",
            "msr ttbr1_el1, {root}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, {bits}",
            "msr sctlr_el1, {sctlr}",
            "isb",
            mair = in(reg) MAIR,
            tcr = in(reg) TCR,
            root = in(reg) root as u64,
            bits = in(reg) SCTLR_ENABLE,
            sctlr = lateout(reg) _,
        );
    }
}

/// Whether the MMU is on: `SCTLR_EL1.M`.
pub fn enabled() -> bool {
    let sctlr: u64;
    // SAFETY: `SCTLR_EL1` is readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
    }
    sctlr & 1 != 0
}

/// The frame mapped at `virt`, and the permissions it is mapped with, by walking
/// the tables the machine is using.
///
/// Both halves are walked, because both are this kernel's: `TTBR0`'s entries are
/// where the image and the devices are, `TTBR1`'s are the direct map and the
/// alias. Which half a `virt` is in is decided by the high bits, the same way
/// the hardware decides it.
pub fn translate(virt: usize) -> Option<(usize, MapFlags)> {
    let root = ROOT.load(Ordering::Relaxed);
    if root == 0 {
        return None;
    }
    let mut table = root;
    for level in 0..3 {
        let entry = read_entry(table, index_of(virt, level));
        if entry & VALID == 0 {
            return None;
        }
        if entry & TABLE == 0 {
            // A block. Only levels 1 and 2 have them, and the output address
            // begins where that level's granularity does.
            let (mask, span) = if level == 1 {
                (ADDR_BLOCK1, 1 << 30)
            } else {
                (ADDR_BLOCK2, BLOCK_SIZE)
            };
            let base = (entry & mask) as usize;
            return Some((base + (virt & (span - 1)), flags_of(entry)));
        }
        table = (entry & ADDR_PAGE) as usize;
    }
    let entry = read_entry(table, index_of(virt, 3));
    if entry & VALID == 0 || entry & TABLE == 0 {
        return None;
    }
    let offset = virt & (PAGE_SIZE - 1);
    Some(((entry & ADDR_PAGE) as usize + offset, flags_of(entry)))
}

/// The `MapFlags` a descriptor was built from, as far as the hardware is
/// concerned. `COW` is not among them — it is not a hardware permission (see
/// `mm::vm::MapFlags`), and a walk reports what the CPU enforces.
fn flags_of(descriptor: u64) -> MapFlags {
    let mut flags = MapFlags::READ;
    // Executable by *either* level: the two "never" bits are per-level, and a
    // page is executable if at least one of them is clear.
    if descriptor & (PXN | UXN) != PXN | UXN {
        flags |= MapFlags::EXEC;
    }
    if descriptor & AP_READ_ONLY == 0 {
        flags |= MapFlags::WRITE;
    }
    if descriptor & AP_USER != 0 {
        flags |= MapFlags::USER;
    }
    flags
}
