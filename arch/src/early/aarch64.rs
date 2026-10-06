//! The aarch64 machine as the loader sees it: the descriptor format, the PL011
//! wire, and the switch into the higher half.
//!
//! Everything around this file - the three ranges, where the frames come from,
//! the sizing of the pool - belongs to the loader (`mpos_boot::vm`). What is
//! here is only what is the machine's, and it is the same division
//! `crate::arch::aarch64` makes one level up.
//!
//! The page-table encoding is written to be the same encoding
//! `crate::arch::aarch64::paging` uses: the kernel's own tables decode what this
//! file writes (`paging::translate`, `flags_of`), and the two spellings drifting
//! apart would turn a working mapping into a `M3_TABLES_FAILED` that names a
//! permission nobody set.
//!
//! # One root, two base registers
//!
//! aarch64 splits the address space at bit 47 of the virtual address: `TTBR0`
//! takes the low half and `TTBR1` the high one, and there is no requirement the
//! two be different tables. Everything this loader maps in the low half is an
//! identity map and everything in the high half is the direct map or the
//! higher-half alias, so the two halves never want the same entry - which is
//! exactly the condition under which one root can serve both registers, and what
//! [`PageTables::root`] is handed to. The kernel's own `install` does the same,
//! so the arrangement is not a loader shortcut.
//!
//! # Two copies of one UART, on purpose
//!
//! The PL011 writer below is not `crate::arch::aarch64::pl011`, and now that
//! both live in one crate the reason is no longer "there is no crate between
//! them" - it is that they are different things at different times. This one is
//! a polled byte sink used before the loader's own tables exist, and it must
//! *not* reinitialize the UART: AAVMF has it up and is using it as its own
//! console, so writing the baud divisor and line control would take the wire
//! away from the firmware mid-sentence. The kernel's is a `hal::Console` with an
//! interrupt-driven transmitter, and it owns the device from the moment the
//! kernel runs. Folding them together would put "do not touch the firmware's
//! configuration" and "configure the UART" in one function with a parameter.

use super::{Frames, PAGE_SIZE, log};

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

const VALID: u64 = 1 << 0;
/// Set in a table descriptor and in a level-3 page descriptor, clear in a block
/// descriptor. Every leaf this file writes is a level-3 page, so this bit is set
/// at every level.
const TABLE: u64 = 1 << 1;
/// `AttrIndx`, bits 4:2: which `MAIR_EL1` field says how to cache this.
const ATTR_SHIFT: u64 = 2;
/// `AP`, bits 7:6: which exception levels may read and write.
const AP_SHIFT: u64 = 6;
/// Inner shareable, bits 9:8. The machine is uniprocessor, but shareability is
/// what makes a mapping usable through the core's caches at all.
const SH_INNER: u64 = 3 << 8;
/// `AF`, bit 10. Without it every access faults, which is the most common way a
/// hand-built table fails - and the failure looks like the memory being absent
/// rather than like a permission.
const AF: u64 = 1 << 10;
/// `PXN`, bit 53, and `UXN`, bit 54: execute never at EL1 and at EL0. They are
/// per-level and either clearing one makes a page executable by that level, so a
/// page meant to be non-executable sets both.
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

/// `AP[2]` is the read-only bit, `AP[1]` says whether EL0 may reach the page.
/// The loader maps nothing for EL0, so only the write bit is in play.
const AP_EL1_RO: u64 = 2 << AP_SHIFT;
const AP_EL1_RW: u64 = 0 << AP_SHIFT;

/// Output address of a table descriptor or a level-3 page: bits 47:12.
const ADDR_PAGE: u64 = 0x0000_ffff_ffff_f000;

/// `MAIR_EL1` fields, in the order `AttrIndx` indexes them. The kernel's
/// `paging.rs` numbers them the same way.
///
/// * 0 — Device-nGnRnE. What MMIO must be: a store to a device register may not
///   be reordered or merged, because for a PL011 that is bytes arriving out of
///   order.
/// * 1 — Normal, non-cacheable. The attribute a framebuffer wants; nothing maps
///   one yet, and the field exists so a later stage's value does not renumber
///   the two around it.
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

/// `TCR_EL1` for a 48-bit virtual address space on both halves, a 4 KiB granule,
/// inner-shareable write-back walks and `IPS` 40 bits (1 TiB of physical, which
/// covers this machine). `T0SZ`/`T1SZ` are 16, which is what makes both halves
/// 48 bits and puts the split at bit 47.
///
/// The same value the kernel's `paging.rs` writes, for the same reason: the
/// kernel stops writing it at stage 1e, so the loader's value is the machine's
/// from then on and the two must agree about what a virtual address is.
const TCR: u64 = 16
    | (16 << 16)
    | (3 << 8)
    | (3 << 10)
    | (3 << 12)
    | (3 << 24)
    | (3 << 26)
    | (3 << 28)
    | (2 << 30);

/// `SCTLR_EL1` bits the loader ORs in: `M` (the MMU), `A` (alignment checking),
/// `C` (data cache) and `I` (instruction cache).
///
/// Read-modify-written rather than assigned, because many of the register's bits
/// are RES1 and the value the firmware left there is the one being modified -
/// a constant that got one of them wrong is a machine that faults before it can
/// say why. `M` is already set (the firmware runs with the MMU on, which is why
/// this loader can write a page table at all); it is named anyway so that the
/// instruction that switches tables is not the only thing claiming translation
/// is on.
const SCTLR_ENABLE: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 12);

// ---------------------------------------------------------------------------
// The tables
// ---------------------------------------------------------------------------

/// A four-level hierarchy under construction.
pub struct PageTables<F: Frames> {
    root: u64,
    pool: F,
}

impl<F: Frames> PageTables<F> {
    /// # Safety
    ///
    /// `root` must be a zeroed, writable, identity-mapped frame, and `pool` must
    /// hand out frames that stay mapped at their physical addresses for as long
    /// as this hierarchy is in use - the firmware's own identity map covers both
    /// until the jump, and nothing here outlives it.
    pub unsafe fn new(root: u64, pool: F) -> PageTables<F> {
        PageTables { root, pool }
    }

    /// The frame both `TTBR0_EL1` and `TTBR1_EL1` are pointed at to use this
    /// hierarchy.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// The frame source, for the one caller that reports how many were taken.
    pub fn pool(&self) -> &F {
        &self.pool
    }

    /// Map `len` bytes of ordinary RAM at `virt`, a 4 KiB page at a time.
    ///
    /// Small pages rather than 2 MiB blocks, for the reason the x86_64 arm gives
    /// and one more of this machine's own: the identity map covers RAM region by
    /// region, and those boundaries are whatever the firmware chose. A block
    /// that overshot into a neighbouring region would describe memory as present
    /// that the caller did not ask for.
    pub fn map(&mut self, virt: u64, phys: u64, len: u64, writable: bool, executable: bool) {
        self.map_with(virt, phys, len, ram_attributes(writable, executable));
    }

    /// Map `len` bytes of device registers at `virt`, a 4 KiB page at a time.
    ///
    /// Device-nGnRnE rather than Normal, and never executable: the whole point
    /// of the attribute is that a store to a register reaches the device in
    /// order and exactly once. RAM accessors and MMIO accessors that differ
    /// only in a comment is how a driver ends up caching a status register.
    fn map_device(&mut self, virt: u64, phys: u64, len: u64) {
        self.map_with(virt, phys, len, device_attributes());
    }

    fn map_with(&mut self, virt: u64, phys: u64, len: u64, attrs: u64) {
        assert!(
            virt.is_multiple_of(PAGE_SIZE)
                && phys.is_multiple_of(PAGE_SIZE)
                && len.is_multiple_of(PAGE_SIZE)
                && len > 0,
            "map({virt:#x}, {phys:#x}, {len:#x}) is not whole pages"
        );

        let mut offset = 0;
        while offset < len {
            let v = virt + offset;
            let table = self.walk_to(v, 3);
            let entry = ((phys + offset) & ADDR_PAGE) | VALID | TABLE | attrs;
            unsafe {
                ((table + index_of(v, 3) as u64 * 8) as *mut u64).write_volatile(entry);
            }
            offset += PAGE_SIZE;
        }
    }

    /// The table `level` deep along `virt`'s walk, making the tables in between.
    fn walk_to(&mut self, virt: u64, level: usize) -> u64 {
        let mut table = self.root;
        for current in 0..level {
            table = self.next_table(table, index_of(virt, current));
        }
        table
    }

    /// The table an entry points at, creating a zeroed one when the entry is
    /// empty.
    fn next_table(&mut self, table: u64, index: usize) -> u64 {
        let slot = unsafe { &mut *((table + index as u64 * 8) as *mut u64) };
        if *slot & VALID != 0 {
            // A block where a table belongs means the layout asked for two
            // mappings that overlap at different granularities. Nothing here
            // writes blocks yet, so the assert is a statement about the ranges
            // this loader is handed rather than about this file - and silently
            // overwriting the entry would turn the bug into a mapping that
            // translates to the wrong frame instead.
            assert!(
                *slot & TABLE != 0,
                "a block descriptor at {table:#x}[{index}] is in the way of a table"
            );
            return *slot & ADDR_PAGE;
        }
        let frame = self.pool.zeroed_frame();
        *slot = frame | VALID | TABLE;
        frame
    }
}

/// The index of `virt` in the table at `level`, where level 0 is the root and
/// level 3 the pages.
fn index_of(virt: u64, level: usize) -> usize {
    ((virt >> (12 + 9 * (3 - level))) & 0x1ff) as usize
}

/// The descriptor bits an ordinary RAM page with these permissions wants.
fn ram_attributes(writable: bool, executable: bool) -> u64 {
    let ap = if writable { AP_EL1_RW } else { AP_EL1_RO };
    let mut descriptor = AF | SH_INNER | (ATTR_NORMAL << ATTR_SHIFT) | ap;
    if !executable {
        // Both levels at once. A page is executable if *either* "never" bit is
        // clear, so withholding only one would leave the kernel's rodata
        // executable at EL1 - which is exactly what W^X exists to prevent.
        descriptor |= PXN | UXN;
    }
    descriptor
}

/// [`ram_attributes`] with the memory attribute replaced: the same permissions
/// on a page that is device registers rather than RAM.
fn device_attributes() -> u64 {
    AF | SH_INNER | (ATTR_DEVICE << ATTR_SHIFT) | AP_EL1_RW | PXN | UXN
}

// ---------------------------------------------------------------------------
// The device window
// ---------------------------------------------------------------------------

/// The machine's device registers, as a fixed range.
///
/// x86_64's loader deliberately maps no device range: its kernel reaches MMIO
/// through `hal::Paging::map_mmio_window`, which walks a BAR and a plan, and the
/// loader has no business guessing where a device is. This machine has no such
/// path at the point the loader runs - the device tree is parsed by the kernel -
/// so the kernel reaches the PL011 and the GIC at the fixed addresses QEMU's
/// `virt` machine puts them at, and they have to be mapped as Device or the
/// first store to a register is a translation fault the moment the firmware's
/// identity map is replaced.
///
/// The range starts at 0x0800_0000 and not at zero: the low 128 MiB of `virt` is
/// flash and the machine's own vectors, and a null pointer that silently works
/// is the one thing x86_64's own `check_arch` asserts cannot happen.
const DEVICE_BASE: u64 = 0x0800_0000;
/// See [`DEVICE_BASE`]. 64 MiB, from the GIC's distributor to past the
/// virtio-MMIO frames: everything the kernel's bring-up and device probe touch.
const DEVICE_WINDOW: u64 = 0x0400_0000;

/// Where the *kernel* half reaches those registers.
///
/// The address x86_64's BAR window names, and for the same reason: a hole the
/// kernel half has room for, immediately below the image at `0xffff_ffff_8000_0000`.
/// Until stage 5b the window was mapped only at its physical address, in
/// `TTBR0` - the half a process will own - which is a collision waiting to
/// happen rather than an arrangement; the kernel has a device window in its own
/// half now, and the low half holds nothing at all.
///
/// The loader maps the window at **both** addresses, and that is not
/// redundancy: this loader's own console writes to the physical one (it runs on
/// the firmware's identity map before its own tables exist, and the firmware
/// brought that UART up there), while the kernel is compiled against the alias
/// from its very first instruction - which runs while the loader's tables are
/// still the machine's. A window only the kernel's `install` mapped would leave
/// the kernel's own banner unmapped.
const DEVICE_VIRT: u64 = 0xffff_ffff_4000_0000;

/// Twice the window, because [`map_extra`] maps it at both addresses.
///
/// A device window costs a page table per 2 MiB whether it is RAM or not, and
/// the loader sizes its pool before the first frame is taken - so the count has
/// to be here, before the mapping, or a pool sized for one address runs out of
/// frames partway through the second.
pub fn extra_mapped_bytes() -> u64 {
    2 * DEVICE_WINDOW
}

/// Map the machine's device window, Device and never executable: at the physical
/// address, which this loader's console is reached through, and at the
/// kernel-half alias, which is where the kernel reaches it.
pub fn map_extra<F: Frames>(tables: &mut PageTables<F>) {
    tables.map_device(DEVICE_BASE, DEVICE_BASE, DEVICE_WINDOW);
    tables.map_device(DEVICE_VIRT, DEVICE_BASE, DEVICE_WINDOW);
    log(&alloc::format!(
        "device window {:#x}..{:#x} mapped at its own address and at {:#x}, \
         Device and not executable",
        DEVICE_BASE,
        DEVICE_BASE + DEVICE_WINDOW,
        DEVICE_VIRT
    ));
}

/// Yes: AAVMF describes this machine with a device tree, and the loader copies
/// it out of the firmware's configuration table before boot services go.
pub const FIRMWARE_PUBLISHES_DEVICE_TREE: bool = true;

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

/// The PL011 on QEMU's `virt` machine, at the address the firmware's device tree
/// gives and `crate::arch::aarch64::pl011` also names.
const BASE: usize = 0x0900_0000;

/// Data register: what is written goes out.
const DR: usize = 0x00;
/// Flag register; bit 5 is `TXFF`, "the transmit FIFO is full".
const FR: usize = 0x18;
/// Control: the enable bits.
const CR: usize = 0x30;

const FR_TXFF: u32 = 1 << 5;
/// `UARTEN | TXE | RXE`.
const CR_ENABLE: u32 = 1 | (1 << 8) | (1 << 9);

fn read(offset: usize) -> u32 {
    // SAFETY: every offset above is a register of the PL011 at `BASE`, which the
    // firmware's identity map already covers - this runs before the loader's own
    // tables are installed.
    unsafe { core::ptr::read_volatile((BASE + offset) as *const u32) }
}

fn write_reg(offset: usize, value: u32) {
    // SAFETY: as `read`.
    unsafe { core::ptr::write_volatile((BASE + offset) as *mut u32, value) };
}

/// OR the enable bits in and touch nothing else.
///
/// Deliberately not a full initialization, unlike the x86_64 arm's: AAVMF has
/// already brought this UART up and is using it as its own console, so rewriting
/// the baud divisor and line control would take the wire away from the firmware
/// in the middle of its own output.
pub fn serial_init() {
    write_reg(CR, read(CR) | CR_ENABLE);
}

/// Send `text` to the wire, turning each newline into a CRLF pair.
///
/// The wire is a terminal emulator on the other end, and a bare newline there
/// is a line feed without a carriage return - which walks diagonally down the
/// screen.
pub fn serial_write(text: &str) {
    for byte in text.bytes() {
        if byte == b'\n' {
            put(b'\r');
        }
        put(byte);
    }
}

fn put(byte: u8) {
    while read(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    write_reg(DR, byte as u32);
}

// ---------------------------------------------------------------------------
// The last instruction
// ---------------------------------------------------------------------------

/// Hand the machine to the kernel: `msr ttbr0_el1` and `br`.
///
/// # Safety
///
/// `record` must point at the loader's filled-in handoff record, `entry` must be
/// mapped executable in the page tables rooted at `root`, `stack_top` must be
/// mapped writable in those same tables, and all three must be reachable from
/// the higher-half addresses they are given as. This is the last thing the
/// loader does and there is no way back from it. `root` is one frame, pointed at
/// by both base registers - see the module documentation for why that is sound.
///
/// # Why the order is the architecture's
///
/// * **Interrupts masked first.** The firmware may have left them on, and an
///   interrupt between the base-register writes and the branch would be taken on
///   the old tables and return to a different stack.
/// * **`dsb ish` before the base registers**, so the page tables this loader just
///   wrote are visible to the walker before anything points at them.
/// * **`MAIR`, `TCR`, then both `TTBR`s**, because a translation can only happen
///   once all four agree.
/// * **`tlbi vmalle1` and another `dsb ish`**, because the walker may have cached
///   the *failed* lookups it made while the firmware's tables were in use - the
///   higher half was not mapped a moment ago, so those entries exist and are
///   negative.
/// * **`isb` after the `SCTLR_EL1` write**, so the instruction after it is
///   fetched with the new tables. `M` is already set (the firmware runs with the
///   MMU on, which is the only reason this loader can write a page table), and
///   the register is read-modify-written rather than assigned for the reason the
///   descriptor section above gives.
///
/// The stack is handed over page-aligned rather than one word down the way the
/// x86_64 arm hands it: `SP` must be sixteen-byte aligned, and with `SCTLR_EL1.A`
/// set a misaligned one is a fault on the first `stp` of the kernel's own
/// prologue rather than a warning.
pub unsafe fn enter(record: *const u8, entry: u64, stack_top: u64, root: u64) -> ! {
    // Two blocks rather than one because `options(noreturn)` forbids an asm
    // output, and the `SCTLR_EL1` update needs a register to read the old value
    // into. The split is where it naturally falls: everything above the last
    // `isb` is the switch and arrives somewhere, and the instructions after it
    // do not.
    unsafe {
        core::arch::asm!(
            "msr daifset, #0xf",
            "dsb ish",
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
            root = in(reg) root,
            bits = in(reg) SCTLR_ENABLE,
            sctlr = lateout(reg) _,
        );
        core::arch::asm!(
            // The record is *placed* in `x0` by the compiler rather than moved
            // there by an instruction, and that is a correctness requirement
            // rather than a style: an instruction writing `x0` is invisible to
            // the register allocator, so `{entry}` may itself be allocated
            // `x0` - and then the move of the record overwrites the branch
            // target and `br` jumps into the handoff record. Measured, not
            // reasoned: the first boot with a slightly larger loader did
            // exactly that, and `Entering EL1` became a synchronous exception
            // at the record's own address.
            "mov sp, {stack}",
            "br {entry}",
            stack = in(reg) stack_top,
            entry = in(reg) entry,
            in("x0") record,
            options(noreturn)
        )
    }
}
