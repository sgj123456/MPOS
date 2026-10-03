//! The aarch64 arm of the loader's page-table builder: four levels, a 4 KiB
//! granule, the aarch64 descriptor.
//!
//! Everything around this file - the three ranges, the pool, the sizing - is
//! `vm.rs`'s and is shared with x86_64. What is here is the encoding, and it is
//! written to be the same encoding `src/arch/aarch64/paging.rs` uses: the
//! kernel's own tables decode what this file writes (`paging::translate`,
//! `flags_of`), and the two spellings drifting apart would turn a working
//! mapping into a `M3_TABLES_FAILED` that names a permission nobody set.
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

use super::Pool;
use crate::elf::PAGE_SIZE;

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
pub const MAIR: u64 = (MAIR_DEVICE_BYTE << (8 * ATTR_DEVICE))
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
pub const TCR: u64 = 16
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
pub const SCTLR_ENABLE: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 12);

// ---------------------------------------------------------------------------
// The tables
// ---------------------------------------------------------------------------

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

    /// The frame both `TTBR0_EL1` and `TTBR1_EL1` are pointed at to use this
    /// hierarchy.
    pub fn root(&self) -> u64 {
        self.root
    }

    pub fn pool(&self) -> &Pool {
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
    pub fn map_device(&mut self, virt: u64, phys: u64, len: u64) {
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
        let frame = self.pool.frame();
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
