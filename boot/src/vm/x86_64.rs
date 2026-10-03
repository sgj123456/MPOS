//! The x86_64 arm of the loader's page-table builder: four levels, 4 KiB pages,
//! the x86_64 descriptor.
//!
//! Everything around this file - the three ranges, the pool, the sizing - is
//! `vm.rs`'s and is shared with aarch64. What is here is only the encoding: which
//! bits mean present, writable, "not executable" and where the output address
//! starts.

use super::Pool;
use crate::elf::PAGE_SIZE;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12..=51 hold the address of the next table or of the page itself.
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

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

        let leaf =
            PRESENT | if writable { WRITABLE } else { 0 } | if executable { 0 } else { NO_EXECUTE };

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
