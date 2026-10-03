//! The aarch64 page tables: the descriptor format, the kernel's own tables, and
//! a walk of whatever the machine is using.
//!
//! Four levels of 512 entries, a 4 KiB granule and two halves: `TTBR0` covers
//! the low virtual half and `TTBR1` the high one, which is the aarch64 division
//! of the address space. Both are pointed at the *same* root here, and that is
//! the same arrangement the loader used: everything the low half holds is the
//! device window and everything the high half holds is the direct map or the
//! image, so no entry is wanted by both registers.
//!
//! # Three stages of the same file
//!
//! What a page-table module is for changed three times on this machine, and the
//! history is worth keeping because each version is a different answer to "who
//! knows the layout":
//!
//! * **Stage 1d built tables from constants.** `-kernel` entered the image with
//!   the MMU off and there was no boot record at all, so `IMAGE_LOAD_BASE`,
//!   `RAM_BASE` and a 1 GiB window were the machine's description of itself, and
//!   the tables came out of a 128 KiB `.bss` arena.
//! * **Stage 1e read the loader's.** `mpos-boot` built them from the firmware's
//!   own memory map, so this file lost its layout and gained a walk: `adopt`
//!   reads `TTBR0_EL1`/`TTBR1_EL1` and `translate` reports what those tables
//!   say. The kernel was a *guest* of the loader's address space.
//! * **Stage 1f builds them from the plan.** `mm::vm::kernel_mappings(info,
//!   sections)` is the shared, host-tested description of what the kernel maps,
//!   and frames come from the frame allocator; the walk stayed, because the
//!   kernel checking its own work is the point.
//!
//! # Two rules that are easy to get wrong
//!
//! * **Every table is addressed through the direct map, never by its physical
//!   address.** While the loader's tables are active both work, which is exactly
//!   why this matters: the moment `install` switches to the kernel's own tables
//!   the identity map is gone, and a physical address read by this file would be
//!   a translation fault in the middle of a page-table walk.
//! * **`memory_of` reports `AttrIndx`, and the loader's and this file's `MAIR_EL1`
//!   must agree about what the indices mean.** They do, field for field, and the
//!   check below reads a device mapping back rather than assuming it.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::boot::BootInfo;
use crate::mm;
use crate::mm::vm::{self, KernelSections, MapFlags, Mapping};

/// 4 KiB granule, matching the loader's and `mm`'s.
const PAGE_SIZE: usize = 4096;
/// A level-2 block: the granule at 2 MiB. Nothing here *writes* one - every
/// mapping goes in a page at a time - and the walk still understands them,
/// because a decoder that quietly mis-read one would report a wrong frame rather
/// than a missing mapping.
const BLOCK_SIZE: usize = 2 * 1024 * 1024;
const ENTRY_COUNT: usize = 512;

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

const VALID: u64 = 1 << 0;
/// Set in a table descriptor and in a level-3 page descriptor, clear in a block
/// descriptor. This bit is what distinguishes them.
const TABLE: u64 = 1 << 1;
/// `AttrIndx`, bits 4:2: which `MAIR_EL1` field says how to cache this.
const ATTR_SHIFT: u64 = 2;
/// `AP`, bits 7:6: which exception levels may read and write.
const AP_SHIFT: u64 = 6;
/// Inner shareable, bits 9:8. The machine is uniprocessor, but shareability is
/// what makes a mapping usable through the core's caches at all.
const SH_INNER: u64 = 3 << 8;
/// `AF`, bit 10. Without it every access faults, which is the most common way a
/// hand-built table fails.
const AF: u64 = 1 << 10;
/// `PXN`, bit 53, and `UXN`, bit 54: execute never at EL1 and at EL0.
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

/// `AP[2]` is the read-only bit; `AP[1]` says whether EL0 may reach the page.
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
/// indexes them. Same indices, same values, as the loader's `vm/aarch64.rs` -
/// the two are separate compilations, so the agreement is a fact to be kept
/// rather than a type to be shared.
///
/// * 0 — Device-nGnRnE. What MMIO must be: a store to a device register may not
///   be reordered or merged, because for a PL011 that is bytes arriving out of
///   order.
/// * 1 — Normal, non-cacheable. The attribute a framebuffer wants; nothing maps
///   one yet.
/// * 2 — Normal, inner and outer write-back, read/write allocate. RAM.
const MAIR_DEVICE_BYTE: u64 = 0x00;
const MAIR_NORMAL_NC_BYTE: u64 = 0x44;
const MAIR_NORMAL_BYTE: u64 = 0xff;
const ATTR_DEVICE: u64 = 0;
const ATTR_NORMAL_NC: u64 = 1;
const ATTR_NORMAL: u64 = 2;

/// The register itself: each field eight bits wide, at the index above.
const MAIR: u64 = (MAIR_DEVICE_BYTE << (8 * ATTR_DEVICE))
    | (MAIR_NORMAL_NC_BYTE << (8 * ATTR_NORMAL_NC))
    | (MAIR_NORMAL_BYTE << (8 * ATTR_NORMAL));

/// `TCR_EL1` for a 48-bit virtual address space on both halves, a 4 KiB granule,
/// inner-shareable write-back walks and `IPS` 40 bits (1 TiB of physical).
/// `T0SZ`/`T1SZ` are 16, which is what puts the split at bit 47.
const TCR: u64 = 16
    | (16 << 16)
    | (3 << 8)
    | (3 << 10)
    | (3 << 12)
    | (3 << 24)
    | (3 << 26)
    | (3 << 28)
    | (2 << 30);

/// `SCTLR_EL1` bits the kernel ORs in: `M` (the MMU), `A` (alignment checking),
/// `C` (data cache) and `I` (instruction cache).
///
/// Read-modify-written rather than assigned, because many of the register's bits
/// are RES1 and the value the loader left there is the one being modified.
const SCTLR_ENABLE: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 12);

// ---------------------------------------------------------------------------
// The device window
// ---------------------------------------------------------------------------

/// Where this machine's device registers are, as a physical range the kernel
/// maps at the *same* virtual address in the low half.
///
/// QEMU `virt` puts PL011 at `0x0900_0000` and the GIC at
/// `0x0800_0000`/`0x0801_0000`; the device tree is what a real port reads, and
/// stage 5 is where it does (`PLAN.md` §9.1).
///
/// It is mapped in the low half - `TTBR0`, the half a process will own - and
/// that is a named gap rather than a choice: x86_64 puts its device window in
/// the *kernel* half (`mm::MMIO_BASE`, just below the image) precisely so a
/// process cannot collide with it, and aarch64 gets the same arrangement when
/// `mm::map_mmio` grows an arm here in stage 5. Until then the kernel's console
/// and interrupt controller are at fixed physical addresses, and a kernel-half
/// alias would have to be spelled in `pl011`/`gic` anyway - so the honest thing
/// is to map where they already are and name the collision that a user address
/// space will eventually have.
///
/// It deliberately does not start at zero: the low 128 MiB of `virt` is flash
/// and the machine's own vectors, and a null pointer that silently works is the
/// one thing x86_64's own `check_arch` asserts cannot happen.
const DEVICE_BASE: usize = 0x0800_0000;
/// 64 MiB, from the GIC's distributor to past the virtio-MMIO frames.
const DEVICE_WINDOW: usize = 0x0400_0000;

/// How the page at a translated address is cached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Memory {
    Normal,
    Device,
}

// ---------------------------------------------------------------------------
// The kernel's tables
// ---------------------------------------------------------------------------

/// The root table `TTBR0_EL1` names, once the machine is running on tables.
static TTBR0: AtomicUsize = AtomicUsize::new(0);
/// The root table `TTBR1_EL1` names.
static TTBR1: AtomicUsize = AtomicUsize::new(0);

/// Build the kernel's page tables from the boot record and the shared plan, and
/// switch the machine onto them.
///
/// Answers how many frames the build took - the one number that says the layout
/// was built rather than assumed, and that a layout which suddenly needs three
/// times as many frames has changed shape.
///
/// # Safety
///
/// Runs once, at boot, after the frame allocator is up. The tables it builds
/// become the machine's, and every address the kernel is executing from, reading
/// or writing at that moment must be in them: the image at `KERNEL_VIRT_BASE`
/// (from `sections` and `info`) and the direct map over `info`'s regions (which
/// is where the stack and the frame allocator's own metadata live).
pub unsafe fn install(info: &BootInfo, sections: &KernelSections) -> usize {
    assert!(
        mm::hhdm_offset() != 0,
        "the tables are built out of frames, so the frame allocator has to be up first"
    );

    // Interrupts off for the whole build: a clock interrupt halfway through
    // would run a handler on tables that describe only part of memory. The
    // loader left `DAIF.I` set and the caller has not opened it yet, so this is
    // a statement of the contract rather than a change of state.
    critical_section::with(|_| {
        let mut tables = Tables::new();
        for mapping in vm::kernel_mappings(info, sections) {
            tables.map(&mapping);
        }
        tables.map_devices();

        let root = tables.root;
        let frames = tables.frames;
        // SAFETY: the caller's contract. Every mapping the machine needs to keep
        // executing, reading and writing is in these tables by construction -
        // they are `mm::vm::kernel_mappings` over `info`'s regions plus the
        // image and the device window - and nothing else is running on this core.
        unsafe { enable_mmu(root) };
        frames
    })
}

/// Read both base registers into the statics the walk uses.
///
/// Read rather than remembered, and that is the difference between "the kernel
/// built tables" and "the machine is translating through them": everything else
/// in this file reports what the *hardware* says. Called once, after `install`.
///
/// # Safety
///
/// The machine must be running on the tables it points at: the walk below reads
/// these frames as tables, and a register that names something else is a read of
/// arbitrary memory.
pub unsafe fn adopt() {
    let ttbr0: usize;
    let ttbr1: usize;
    // SAFETY: both are readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0, options(nomem, nostack));
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1, options(nomem, nostack));
    }
    // The low bits of a `TTBR` are `CnP`/`BADDR` attributes rather than address,
    // and the walk wants the frame.
    TTBR0.store(ttbr0 & ADDR_PAGE as usize, Ordering::Relaxed);
    TTBR1.store(ttbr1 & ADDR_PAGE as usize, Ordering::Relaxed);
}

/// The frame each base register names, for the boot log.
pub fn roots() -> (usize, usize) {
    (TTBR0.load(Ordering::Relaxed), TTBR1.load(Ordering::Relaxed))
}

/// A hierarchy under construction, with its frames from the frame allocator.
///
/// No arena and no pool: stage 1d had one because there was no allocator to draw
/// from, and the whole reason this stage exists is that there is one now.
struct Tables {
    root: usize,
    frames: usize,
}

impl Tables {
    fn new() -> Tables {
        let mut tables = Tables { root: 0, frames: 0 };
        tables.root = tables.frame();
        tables
    }

    /// A fresh, zeroed frame, by physical address.
    ///
    /// Zeroed because an empty page-table entry is an absent one: a frame
    /// straight from the allocator still holds whatever the last user left in
    /// it, and any of those bytes with bit 0 set would read as a mapping.
    fn frame(&mut self) -> usize {
        let frame = mm::alloc_frames(0)
            .expect("out of frames for the kernel's page tables, which cannot be recovered from");
        self.frames += 1;
        // SAFETY: the allocator handed over `PAGE_SIZE` bytes of RAM that nothing
        // else holds, and the direct map - which is what `phys_to_virt` is, and
        // which is active both before and after the switch below - reaches them.
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
        frame
    }

    /// Map one entry of the plan, a page at a time.
    ///
    /// `Mapping` is the shared plan's own type, so the assert is the plan's own
    /// predicate: a mapping that is not page-granular cannot be described by a
    /// loop that walks pages.
    fn map(&mut self, mapping: &Mapping) {
        assert!(
            mapping.is_page_granular(),
            "a mapping that is not page-granular: {mapping:?}"
        );
        let attrs = attributes(mapping.flags);
        let mut offset = 0;
        while offset < mapping.length {
            self.map_page(mapping.virt + offset, mapping.phys + offset, attrs);
            offset += PAGE_SIZE;
        }
    }

    /// Map the machine's device registers, Device and never executable.
    ///
    /// A page at a time, like everything else here. The window is a constant and
    /// could be a handful of 2 MiB blocks, but a block descriptor and a page
    /// descriptor cannot describe the same 2 MiB - so mixing them turns "the
    /// ranges do not overlap" into something a reader has to check, for the sake
    /// of a few hundred bytes of table.
    fn map_devices(&mut self) {
        let attrs = device_attributes(MapFlags::READ | MapFlags::WRITE);
        let mut offset = 0;
        while offset < DEVICE_WINDOW {
            self.map_page(DEVICE_BASE + offset, DEVICE_BASE + offset, attrs);
            offset += PAGE_SIZE;
        }
    }

    fn map_page(&mut self, virt: usize, phys: usize, attrs: u64) {
        let table = self.walk_to(virt, 3);
        write_entry(
            table,
            index_of(virt, 3),
            (phys as u64 & ADDR_PAGE) | VALID | TABLE | attrs,
        );
    }

    /// The table `level` deep along `virt`'s walk, making the tables in between.
    fn walk_to(&mut self, virt: usize, level: usize) -> usize {
        let mut table = self.root;
        for current in 0..level {
            table = self.next_table(table, index_of(virt, current));
        }
        table
    }

    fn next_table(&mut self, table: usize, index: usize) -> usize {
        let entry = read_entry(table, index);
        if entry & VALID != 0 {
            // A block where a table belongs means two mappings overlap at
            // different granularities. Nothing here writes blocks, so this can
            // only be a range that arrived from somewhere else - and silently
            // overwriting the entry would turn it into a mapping that translates
            // to the wrong frame instead.
            assert!(
                entry & TABLE != 0,
                "a block descriptor at {table:#x}[{index}] is in the way of a table"
            );
            return (entry & ADDR_PAGE) as usize;
        }
        let frame = self.frame();
        write_entry(table, index, frame as u64 | VALID | TABLE);
        frame
    }
}

/// The entry at `index` of the table at physical address `table`.
///
/// Through the direct map, never by the physical address itself - see the module
/// header: after `install` there is no identity map, and a raw physical address
/// here would fault inside a walk.
fn entry_slot(table: usize, index: usize) -> *mut u64 {
    (mm::phys_to_virt(table) + index * 8) as *mut u64
}

fn read_entry(table: usize, index: usize) -> u64 {
    // SAFETY: `table` came out of a descriptor chain that starts at a base
    // register and `index` is below 512, so this is inside a page-table frame
    // the machine is translating through. Those frames are RAM, which the direct
    // map covers.
    unsafe { core::ptr::read_volatile(entry_slot(table, index)) }
}

fn write_entry(table: usize, index: usize, value: u64) {
    // SAFETY: as `read_entry`, and the frame is one this build allocated.
    unsafe { core::ptr::write_volatile(entry_slot(table, index), value) };
}

/// The index of `virt` in the table at `level`, where level 0 is the root.
fn index_of(virt: usize, level: usize) -> usize {
    (virt >> (12 + 9 * (3 - level))) & (ENTRY_COUNT - 1)
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

/// Point the machine at `root`, on both halves.
///
/// The order is the architecture's and every step is load-bearing: `MAIR` and
/// `TCR` before the base registers, because a translation can only happen once
/// all three agree; `dsb` so the table writes are visible to the walker; `tlbi
/// vmalle1` because the walker has cached the loader's translations and its
/// *failed* lookups, neither of which describes this address space; and `isb`
/// after the `SCTLR_EL1` write so the instruction after it is fetched with
/// translation on.
///
/// # Safety
///
/// `root` must be a complete hierarchy for everything the kernel is executing
/// from, reading and writing at this moment. The call does not return to a
/// machine that can be recovered if it is not.
unsafe fn enable_mmu(root: usize) {
    // SAFETY: the caller's contract.
    //
    // `{sctlr}` is written by the `mrs` before either of the two instructions
    // that read it, which is why it is a `lateout` rather than an `inout` with a
    // value nothing uses: `SCTLR_EL1` has RES1 bits, and the value the loader put
    // there is the one being modified.
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

/// The frame mapped at `virt`, the permissions it is mapped with, and whether it
/// is cached as memory or as device, by walking the tables the machine is using.
///
/// Which half a `virt` is in is decided the way the hardware decides it - by the
/// top bit of the address, `TTBR1` for the high half and `TTBR0` for the low -
/// rather than by asking which root has an entry, because a walk that guessed
/// would report a missing mapping as absent instead of as the wrong half.
pub fn translate(virt: usize) -> Option<(usize, MapFlags, Memory)> {
    let root = if virt >> (usize::BITS - 1) != 0 {
        TTBR1.load(Ordering::Relaxed)
    } else {
        TTBR0.load(Ordering::Relaxed)
    };
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
            return Some((
                base + (virt & (span - 1)),
                flags_of(entry),
                memory_of(entry),
            ));
        }
        table = (entry & ADDR_PAGE) as usize;
    }
    let entry = read_entry(table, index_of(virt, 3));
    if entry & VALID == 0 || entry & TABLE == 0 {
        return None;
    }
    let offset = virt & (PAGE_SIZE - 1);
    Some((
        (entry & ADDR_PAGE) as usize + offset,
        flags_of(entry),
        memory_of(entry),
    ))
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

/// Whether a descriptor's page is cached as device registers or as memory.
fn memory_of(descriptor: u64) -> Memory {
    if (descriptor >> ATTR_SHIFT) & 0b111 == ATTR_DEVICE {
        Memory::Device
    } else {
        Memory::Normal
    }
}
