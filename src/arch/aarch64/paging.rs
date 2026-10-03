//! The aarch64 page tables: the descriptor format, the kernel's own tables, and
//! a walk of whatever the machine is using.
//!
//! Four levels of 512 entries, a 4 KiB granule and two halves: `TTBR0` covers
//! the low virtual half and `TTBR1` the high one, which is the aarch64 division
//! of the address space. Until stage 6a both were pointed at the *same* root,
//! which is the arrangement the loader used and which is sound only while no
//! entry is wanted by both - and since stage 5b that condition is met trivially:
//! the direct map, the image *and* the device window are all in the high half,
//! so the low half was empty and `TTBR0` resolved nothing. Stage 6a is where the
//! two registers stop agreeing, which is what the low half exists for: a process
//! gets its own root ([`UserSpace`]) and the kernel keeps the one `TTBR1` names.
//!
//! # Four stages of the same file
//!
//! What a page-table module is for changed four times on this machine, and the
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
//! * **Stage 6a builds one for somebody else.** The same [`Tables`] builder, on
//!   a root that is not the kernel's and with leaves that carry `AP`'s EL0 bits:
//!   a user space is the same four levels and the same descriptors, and the only
//!   thing new is who the root belongs to.
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

/// Where this machine's device registers are, physically, as the firmware's
/// device tree describes them.
///
/// QEMU `virt` puts PL011 at `0x0900_0000` and the GIC at
/// `0x0800_0000`/`0x0801_0000`; the device tree is what a real port reads, and
/// stage 5a is where this one does.
///
/// It deliberately does not start at zero: the low 128 MiB of `virt` is flash
/// and the machine's own vectors, and a null pointer that silently works is the
/// one thing x86_64's own `check_arch` asserts cannot happen.
pub const DEVICE_PHYS: usize = 0x0800_0000;
/// 64 MiB, from the GIC's distributor to past the virtio-MMIO frames.
pub const DEVICE_WINDOW: usize = 0x0400_0000;

/// Where the *kernel* half reaches those same registers.
///
/// The address x86_64's `mm::MMIO_BASE` uses, and for the same reason: a hole
/// the kernel half has room for, immediately below the image at
/// `0xffff_ffff_8000_0000`. Stage 5b moved the window here from the physical
/// address it used to be mapped at, which lived in `TTBR0` - the half a process
/// will own, so a process's own mapping could have landed on the console's
/// registers. The low half holds nothing at all now, which `check_paging` says
/// in as many words.
///
/// `pl011` and `gic` are compiled against addresses in this window rather than
/// against the physical ones, and that is why the *loader* maps the alias too
/// (`boot/src/vm.rs`) rather than only `install` below: the kernel's first
/// instruction - the banner, and the device check that runs before the frame
/// allocator - executes while the loader's tables are still the machine's.
pub const DEVICE_VIRT: usize = 0xffff_ffff_4000_0000;

/// The kernel-half address a device's registers are reached through, or `None`
/// when the device is not wholly inside the window the kernel mapped.
///
/// The whole 64 MiB is mapped a page at a time by [`Tables::map_devices`], so
/// reaching a device is arithmetic rather than a page-table walk - which is what
/// this machine's `mm::map_mmio` becomes when `hal` reaches it at stage 6. A
/// device outside the window is `None` rather than a wrapped address: the caller
/// is a boot check reading what the firmware described, and a tree that named a
/// device this kernel did not map is a failure it has to be able to report.
pub fn device_virt(phys: usize, len: usize) -> Option<usize> {
    let end = phys.checked_add(len)?;
    if phys < DEVICE_PHYS || end > DEVICE_PHYS + DEVICE_WINDOW {
        return None;
    }
    Some(DEVICE_VIRT + (phys - DEVICE_PHYS))
}

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
///
/// Zero until [`install`] has run. While there is no user space this is the
/// *same* frame as [`TTBR1`], which is why the kernel could run with one root
/// for both registers; [`UserSpace::switch`] is what makes them differ, and
/// after it this static follows `TTBR0_EL1` rather than the kernel.
static TTBR0: AtomicUsize = AtomicUsize::new(0);
/// The root table `TTBR1_EL1` names - the kernel's own, where the image, the
/// direct map and the device window live. Nothing moves it after `install`.
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

    /// A hierarchy rooted at a frame that already exists - a process's, rather
    /// than the kernel's.
    ///
    /// The same builder, on purpose: a user space's tables are the same four
    /// levels of the same descriptors, and the only difference is which root
    /// they hang from and which `AttrIndx`/`AP` bits the leaves carry. Two
    /// builders would be two places for the encoding to drift.
    fn rooted(root: usize) -> Tables {
        Tables { root, frames: 0 }
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

    /// Map the machine's device registers, Device and never executable, into the
    /// kernel half.
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
            self.map_page(DEVICE_VIRT + offset, DEVICE_PHYS + offset, attrs);
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
    walk_from(root, virt)
}

/// [`translate`]'s walk, against a root the caller names.
///
/// Split out because a space that is *not* the active one has to be readable
/// too - a signal trampoline goes into the address space of a task the kernel is
/// not currently running, and a process asking `mincore` about its own mapping
/// is answered from its page tables whichever of them is installed. The active
/// walk chooses its root by the address's own top bit, the way the hardware
/// does; this one is told.
fn walk_from(root: usize, virt: usize) -> Option<(usize, MapFlags, Memory)> {
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

// ---------------------------------------------------------------------------
// A process's address space
// ---------------------------------------------------------------------------

/// A user address space: its own root for `TTBR0_EL1`, and the mappings a
/// process is allowed to reach.
///
/// The split between the two base registers is the architecture's and this type
/// is the first thing on this machine to use it. Until stage 6a both pointed at
/// the *kernel's* root, which worked only because the low half was empty (5b
/// moved the last thing that lived there into the kernel half) - so "an address
/// space" is not a new arrangement imposed on the kernel, it is the state the
/// kernel was already in being given to somebody else.
///
/// What is here is what stage 6a runs: a fresh root, a page mapped into it with
/// user permissions, the switch that installs it, and the walk that asks what an
/// address in it maps to. What is *not* here is the rest of
/// [`crate::hal::AddressSpaceOps`] - `unmap`, `protect`, `walk_user_pages`, and
/// the copy-on-write half - which is what a `fork` and a demand-paged `mmap`
/// need, and which lands with the trait impl rather than before it. The
/// page-table machinery those need is [`Tables`], which is already built; what
/// is missing is the operations, not the encoding.
///
/// The frames a space is made of are leaked rather than freed, which is honest
/// for a machine with one user space that lives as long as the boot check does
/// and is not honest for a running process. `Drop` lands with the trait impl
/// too, beside `unmap`, which is the operation it is built out of.
pub struct UserSpace {
    root: usize,
}

impl UserSpace {
    /// An address space with nothing in it.
    pub fn new() -> UserSpace {
        let root = mm::alloc_frames(0)
            .expect("out of frames for a user address space, which cannot be recovered from");
        // Zeroed for the reason `Tables::frame` zeroes: an empty page-table
        // entry is an absent one, and a frame straight from the allocator holds
        // whatever the last user left in it.
        // SAFETY: the allocator handed over `PAGE_SIZE` bytes of RAM that
        // nothing else holds, and the direct map reaches them.
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(root) as *mut u8, 0, PAGE_SIZE) };
        UserSpace { root }
    }

    /// The frame `TTBR0_EL1` must name for this space to be the active one.
    pub fn root(&self) -> usize {
        self.root
    }

    /// Map `len` bytes of `phys` at `virt`, for EL0.
    ///
    /// Asserts rather than refusing a mapping without `USER`: a page the user
    /// cannot reach is not a user mapping, and the caller that wants one has the
    /// kernel's own [`Tables`] through `install`. It also asserts a whole number
    /// of pages, because `Mapping`'s page-granular predicate is what the rest of
    /// the kernel is written against and a space that quietly rounded would be a
    /// second rule.
    pub fn map(&mut self, virt: usize, phys: usize, len: usize, flags: MapFlags) {
        assert!(
            flags.contains(MapFlags::USER),
            "a user space maps user pages: {flags:?} at {virt:#x} would be unreachable from EL0"
        );
        assert!(
            virt.is_multiple_of(PAGE_SIZE)
                && phys.is_multiple_of(PAGE_SIZE)
                && len.is_multiple_of(PAGE_SIZE)
                && len > 0,
            "map({virt:#x}, {phys:#x}, {len:#x}) is not whole pages"
        );
        let attrs = attributes(flags);
        let mut tables = Tables::rooted(self.root);
        let mut offset = 0;
        while offset < len {
            tables.map_page(virt + offset, phys + offset, attrs);
            offset += PAGE_SIZE;
        }
    }

    /// Make this space the one `TTBR0_EL1` names, and this core's translations
    /// for the low half follow it.
    ///
    /// The barriers are the architecture's and every one is load-bearing:
    /// `dsb ish` so the table writes are visible to the walker, `tlbi vmalle1`
    /// because the walker has cached the *previous* space's translations and its
    /// failed lookups, and `isb` so the next fetch happens with the new
    /// register. `vmalle1` rather than an ASID-tagged invalidation because this
    /// machine does not use ASIDs yet - one space, and no tag to give it.
    pub fn switch(&self) {
        // SAFETY: `root` is a hierarchy this type built, and every kernel address
        // the machine is executing from or reading is in `TTBR1`, which this
        // does not touch.
        unsafe {
            core::arch::asm!(
                "msr ttbr0_el1, {root}",
                "dsb ish",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                root = in(reg) self.root as u64,
            );
        }
        TTBR0.store(self.root, Ordering::Relaxed);
    }

    /// What `virt` maps to in this space, with the permissions the walk reports
    /// and whether the page is cached as memory - the same three things
    /// [`translate`] answers for the active space.
    ///
    /// The pair with `phys_for_virt` is the point: one asks *where* a page is and
    /// one asks *what it is*, and a caller checking its own work wants both
    /// without the space having to be the active one.
    pub fn translate(&self, virt: usize) -> Option<(usize, MapFlags, Memory)> {
        walk_from(self.root, virt)
    }
}
