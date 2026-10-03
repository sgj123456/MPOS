//! The aarch64 page tables: reading the ones the machine is already running on.
//!
//! Four levels of 512 entries, a 4 KiB granule and two halves: `TTBR0` covers
//! the low virtual half and `TTBR1` the high one, which is the aarch64 division
//! of the address space. Which base register a walk starts from is decided by
//! the address's own top bit, the way the hardware decides it.
//!
//! # This file does not build tables any more, and that is stage 1e's change
//!
//! Until stage 1e this module *built* the kernel's tables: a fixed layout from
//! constants, out of a `.bss` arena, installed by writing `TTBR0_EL1`,
//! `TTBR1_EL1`, `TCR_EL1`, `MAIR_EL1` and `SCTLR_EL1`, because the `-kernel`
//! bring-up entered the image with the MMU off and had to turn it on itself.
//! Stage 1e deletes that path: `mpos-boot` builds the tables from the firmware's
//! own memory map, maps the image at `KERNEL_VIRT_BASE`, relocates the image's
//! absolute addresses and branches into the higher half with the MMU already on.
//! The loader can do that because it runs before the kernel and with firmware
//! protocols still available; the kernel could not, because it has no record to
//! build from until the loader hands it one.
//!
//! So what is left here is the *format*: the descriptor decode, and a walk that
//! reads the tables the machine is using. The builder comes back when the kernel
//! installs its own tables from `mm::vm::kernel_mappings` and a frame allocator
//! (`PLAN.md` §9.1's named follow-on), and it will be built from that plan rather
//! than from the constants this file used to carry.
//!
//! The encoding is shared with the loader's `boot/src/vm/aarch64.rs`, which
//! writes these descriptors. The two are separate compilations with no crate
//! between them, so the pairing is kept honest by the checks in
//! `arch/aarch64/mod.rs` reading back what the loader wrote rather than by a
//! shared constant - the same arrangement as the handoff record.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::mm::vm::MapFlags;

/// 4 KiB granule, matching the loader's and `mm`'s.
const PAGE_SIZE: usize = 4096;
/// A level-2 block: the granule at 2 MiB. The loader never writes one, and the
/// decode below still understands it - the tables the kernel installs from
/// `kernel_mappings` will, and a walk that quietly mis-decoded one would report
/// a wrong frame rather than a missing mapping.
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
/// `PXN`, bit 53, and `UXN`, bit 54: execute never at EL1 and at EL0.
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

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

/// The `AttrIndx` values the loader's `MAIR_EL1` defines, as far as a walk can
/// tell them apart: field 0 is Device-nGnRnE and everything else in use is
/// ordinary Normal memory.
///
/// Worth checking rather than ignoring, because the two are not interchangeable
/// in either direction - RAM mapped Device is uncached and thousands of times
/// slower, and a device register mapped Normal lets the core merge or reorder
/// stores to it, which for a UART is bytes arriving out of order.
const ATTR_DEVICE: u64 = 0;

/// How the page at a translated address is cached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Memory {
    Normal,
    Device,
}

/// The root table `TTBR0_EL1` names, once the machine is running on tables.
static TTBR0: AtomicUsize = AtomicUsize::new(0);
/// The root table `TTBR1_EL1` names.
static TTBR1: AtomicUsize = AtomicUsize::new(0);

/// Read both base registers into the statics the walk uses.
///
/// Read once, at the point the kernel knows the machine is running on its own
/// tables, so that a walk cannot observe a half-changed pair if anything ever
/// switches them again.
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
    // and the walk wants the frame: bits 47:12, and on this machine the tables
    // are in the low half so nothing above bit 47 is set.
    TTBR0.store(ttbr0 & ADDR_PAGE as usize, Ordering::Relaxed);
    TTBR1.store(ttbr1 & ADDR_PAGE as usize, Ordering::Relaxed);
}

/// The frame each base register names, for the boot log.
pub fn roots() -> (usize, usize) {
    (TTBR0.load(Ordering::Relaxed), TTBR1.load(Ordering::Relaxed))
}

fn entry_slot(table: usize, index: usize) -> *mut u64 {
    (table + index * 8) as *mut u64
}

fn read_entry(table: usize, index: usize) -> u64 {
    // SAFETY: `table` came out of a descriptor chain that starts at a base
    // register and `index` is below 512, so this is inside a page-table frame
    // the machine is translating through. Those frames are in RAM and the
    // direct map covers RAM, so the physical address is one the CPU can read.
    unsafe { core::ptr::read_volatile(entry_slot(table, index)) }
}

/// The index of `virt` in the table at `level`, where level 0 is the root.
fn index_of(virt: usize, level: usize) -> usize {
    (virt >> (12 + 9 * (3 - level))) & (ENTRY_COUNT - 1)
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

/// Whether the MMU is on: `SCTLR_EL1.M`.
///
/// The loader turned it on and the kernel never turned it off; the check reads
/// rather than assumes, because everything below it is a statement about
/// translation and a machine with the MMU off would translate nothing.
pub fn enabled() -> bool {
    let sctlr: u64;
    // SAFETY: `SCTLR_EL1` is readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
    }
    sctlr & 1 != 0
}
