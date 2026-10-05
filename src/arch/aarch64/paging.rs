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
//! # Five stages of the same file
//!
//! What a page-table module is for changed five times on this machine, and the
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
//! * **Stage 6b takes one apart again.** Building a space turns out to be half of
//!   the job: a process that has run for a while needs mappings taken back
//!   (`unmap`), permissions changed in place (`protect`), a walk that names what
//!   is there (`walk_user_pages`), and the copy-on-write half that lets two
//!   processes share a frame until one of them writes it - plus, at the end,
//!   `Drop`, which is `unmap` applied to whole tables. The encoding does not
//!   change at all; what changes is that this file now writes *and erases*.
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
use crate::mm::vm::{self, KernelSections, MapFlags, Mapping, PageTaken};

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

/// A software bit: this page is copy-on-write.
///
/// Bits 58:55 of a stage-1 descriptor are reserved for software use on a 4 KiB
/// granule - the walker ignores them, so nothing here translates into a
/// permission - and this is the one bit this kernel spends there. The x86_64
/// twin keeps the same marker in bit 9 of its PTE, which is "available" there
/// for the same reason: copy-on-write is not something the hardware enforces,
/// the kernel does. What the CPU *does* enforce for such a page is read-only,
/// which is `AP[2]`, and `set_cow_bit_quietly` therefore sets both.
const COW_BIT: u64 = 1 << 55;

/// One past the last address `TTBR0_EL1` can name: the low half of a 48-bit
/// virtual address space, which is 2^48 with this kernel's `T0SZ` of 16.
///
/// Twice the x86_64 twin's `1 << 47`, and the difference is the machine's
/// rather than a choice: x86_64 splits its canonical address space in the
/// middle of the *address* - bit 47 - while aarch64 splits it between two
/// registers, so everything below 2^48 is the half a process owns and the top
/// of the space is the kernel's. `TTBR0_EL1` cannot name an address past this,
/// which is what makes it the bound of what a user space may map.
const USER_END: usize = 1 << 48;

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

/// `SCTLR_EL1` bits the kernel ORs in: `M` (the MMU), `C` (data cache) and `I`
/// (instruction cache) - deliberately **not** `A` (alignment checking).
///
/// `A=1` makes an unaligned load/store an Alignment fault, and it applies to EL0
/// too. Linux boots with `A=0`, so an unaligned memory access simply works; real
/// programs depend on that - glibc/musl's `memcpy` uses `ldp`/`stp`, which need
/// 8-byte alignment, on whatever pointers callers hand it, so with `A=1` an
/// unmodified binary faults in `memcpy` copying an odd-offset string. Measured: a
/// print-then-`memcpy` static-musl helper took a Data Abort (DFSC `0x21`, alignment)
/// at an unmis-aligned-but-mapped page until `A` was dropped. Clearing it matches
/// Linux and lets real binaries' unaligned accesses through; the kernel's own data
/// is aligned and needs the check not at all.
///
/// Read-modify-written rather than assigned, because many of the register's bits
/// are RES1 and the value the loader left there is the one being modified.
const SCTLR_ENABLE: u64 = (1 << 0) | (1 << 2) | (1 << 12);

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

/// The root the hardware walks `virt` through: `TTBR1` for the high half and
/// `TTBR0` for the low, decided by the top bit of the address exactly as
/// [`translate`] does. Shared by the operations that ask about the *active*
/// space, so a kernel address is never answered from a process's tables.
fn root_for(virt: usize) -> usize {
    if virt >> (usize::BITS - 1) != 0 {
        TTBR1.load(Ordering::Relaxed)
    } else {
        TTBR0.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// The questions asked of an address space that is not the only one - the half
// of `hal::Paging` that needs a running process. Stage 6b built the operations
// a `UserSpace` performs on itself; these read the machine's state and walk a
// root the caller names, which is what the scheduler (`active_root`,
// `switch_to_root`) and the uaccess/signal path (`user_accessible`,
// `write_user_u32`, `phys_for_virt_of`) will call once they un-gate here.
// ---------------------------------------------------------------------------

/// The root of the address space currently active on this CPU (`TTBR0_EL1`).
pub fn active_root() -> usize {
    TTBR0.load(Ordering::Relaxed)
}

/// The root of the kernel's own address space (`TTBR1_EL1`), which is also what
/// `TTBR0_EL1` names when no process is installed.
pub fn kernel_root() -> usize {
    TTBR1.load(Ordering::Relaxed)
}

/// Make `root` the active low half. The kernel's high half (`TTBR1_EL1`) is not
/// touched, so the switch is safe while this code runs from it.
///
/// # Safety
///
/// `root` must name a complete low half - the barriers `load_root0` runs flush
/// the previous space's cached translations, so a half-way table would fault the
/// very next instruction fetch. The scheduler installs a space only after it has
/// mapped it.
pub unsafe fn switch_to_root(root: usize) {
    // SAFETY: the caller's contract, passed to `load_root0` unchanged.
    unsafe { load_root0(root) };
    TTBR0.store(root, Ordering::Relaxed);
}

/// Switch back to the kernel's own space by pointing `TTBR0_EL1` at its root.
///
/// The kernel half is empty below `USER_END`, so this is the "no process" state:
/// a low address stops resolving entirely rather than resolving into whichever
/// process was last installed.
pub fn switch_to_kernel() {
    let root = TTBR1.load(Ordering::Relaxed);
    // SAFETY: the kernel's own root is the one value always safe to install - its
    // low half is empty and everything the kernel runs from is in `TTBR1_EL1`.
    unsafe { load_root0(root) };
    TTBR0.store(root, Ordering::Relaxed);
}

/// The frame backing `virt` in the space rooted at `root`, if any.
///
/// A process asking `mincore` about its own mapping is answered from its page
/// tables whichever of them is installed, so this walks the root the caller
/// names, not the active one.
pub fn phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
    walk_from(root, virt).map(|(phys, _, _)| phys)
}

/// [`phys_for_virt_of`], but only for a page the user may touch - the walk
/// requires the `USER` bit, which is what makes `PAGE_MAP` in `/proc` refuse to
/// leak a kernel mapping a caller named by address.
pub fn user_phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
    walk_from(root, virt)
        .and_then(|(phys, flags, _)| flags.contains(MapFlags::USER).then_some(phys))
}

/// Whether `start..start + len` is mapped in the *active* space as a user range
/// the caller's access allows: every page it touches must carry `USER` and `READ`
/// (and `WRITE` when `write` is set). A range that runs off the top of the low
/// half into the kernel's is rejected whole, because a user pointer is a `TTBR0`
/// address by definition.
///
/// The policy here is `arch/x86_64/paging.rs`'s, and deliberately a second copy
/// rather than a shared one: it needs two things no lower layer may name - the
/// machine's own table walk and `sched`'s VMA table - so a single home for it
/// would be an edge the module graph forbids (`hal` may depend on nothing, `mm`
/// on `boot`). What is shared is the *answer*; a port that writes this again
/// writes these three rules with it, and each is a measurement rather than a
/// reading of the architecture:
///
/// 1. **An absent page is not an unmapped one.** A mapping the process has made
///    and not touched has no table entry yet, so the live VMA table decides. This
///    is not a nicety: a `BufReader`'s heap buffer is allocated and unread when
///    the first `read` asks for its whole capacity, so refusing the absent half
///    answers `EFAULT` to a perfectly valid buffer - measured on the second
///    machine, where a shell's very first `read(0)` died with it (`PLAN.md`
///    §13.9). Only anonymous ranges qualify; see `sched::kernel_lazy_accessible`
///    for why a frame-backed one must be present first.
/// 2. **A copy-on-write page is writable.** It is mapped read-only on purpose:
///    the write it catches is answered by the fault handler, not refused. Reading
///    the bit and stopping there turns every syscall that fills a user buffer
///    into an `EFAULT` for any process that has forked.
/// 3. **A zero-length range is accessible.** `read(fd, buf, 0)` is a valid call
///    whatever `buf` holds - Linux answers 0 - and the length bound all three
///    callers share is the real gate.
pub fn user_accessible(start: usize, len: usize, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return false,
    };
    // Every touched page, from the page `start` lies in to the page the last byte
    // lies in. `USER` is required throughout: a kernel mapping is never a valid
    // uaccess target, and its permissions would answer `true` on their own.
    let mut page = start & !(PAGE_SIZE - 1);
    loop {
        if page >> (usize::BITS - 1) != 0 {
            // Walked up into `TTBR1`'s half - not a user range.
            return false;
        }
        match translate(page) {
            Some((_, flags, _)) => {
                if !flags.contains(MapFlags::USER) || !flags.contains(MapFlags::READ) {
                    return false;
                }
                if write && !flags.contains(MapFlags::WRITE) && !is_cow_page(page) {
                    return false;
                }
            }
            // Rule 1: a mapping with no table entry yet is the VMA table's to
            // answer, and a page with no VMA behind it is still refused - that is
            // a pointer the process never mapped. `write` is passed through
            // because wanting to write a read-only VMA is a refusal, not a fault
            // a handler would resolve.
            None => {
                if !crate::sched::kernel_lazy_accessible(page, write) {
                    return false;
                }
            }
        }
        if page + PAGE_SIZE >= end {
            return true;
        }
        page += PAGE_SIZE;
    }
}

/// Whether the leaf mapping at `virt` in the *active* space carries the
/// copy-on-write marker. The hardware does not enforce that bit, so `translate`
/// never reports it; this asks the descriptor directly. It is the first question
/// a data-abort handler asks to tell a COW fault from a real one.
pub fn is_cow_page(virt: usize) -> bool {
    leaf_of(root_for(virt), virt).is_some_and(|(_, _, entry)| entry & COW_BIT != 0)
}

/// Write `value` to the user address `at` through the space rooted at `root`,
/// answering whether the address was mapped. Used to place a signal trampoline
/// and the signal frame into a task the kernel is not currently running, so it
/// must reach through *that* task's tables and the direct map, not a live switch.
pub fn write_user_u32(root: usize, at: usize, value: u32) -> bool {
    match leaf_of(root, at) {
        Some((_, _, entry)) => {
            let phys = (entry & ADDR_PAGE) as usize + (at & (PAGE_SIZE - 1));
            // SAFETY: `root` names a mapping the caller asked us to write, and the
            // direct map - active in `TTBR1` whichever low half is installed -
            // reaches the frame `phys` is in.
            unsafe { core::ptr::write(mm::phys_to_virt(phys) as *mut u32, value) };
            true
        }
        None => false,
    }
}

/// Map a device's register window into the kernel half.
///
/// There is nothing to add on this machine: the whole 64 MiB window the tree
/// describes is mapped at boot (stage 5b), and [`device_virt`] is the only door to
/// it. So `map_mmio` confirms the window already covers `[phys, phys + len)` at
/// the address the caller named and does nothing - which is correct for a machine
/// whose devices are a firmware-described range - and panics on a window outside
/// it. A PCIe aarch64 server that hands out BARs to map dynamically is a named
/// gap in `PLAN.md` §9.1, not something to half-implement here.
pub fn map_mmio(virt: usize, phys: usize, len: usize) {
    if device_virt(phys, len) != Some(virt) {
        panic!(
            "aarch64 map_mmio: [{phys:#x}, {:#x}) is outside the fixed device window; \
             this machine maps its MMIO statically and has no dynamic window yet",
            phys + len
        );
    }
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
            // Explicitly clear `A` (bit 1, alignment-check enable): the `orr` can
            // only set bits, and the firmware may have left `A` already set, which
            // would fault real binaries' unaligned `memcpy`. Linux boots `A=0`;
            // match it. (See `SCTLR_ENABLE`'s note.)
            "bic {sctlr}, {sctlr}, #2",
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

/// [`flags_of`] plus the one marker the hardware does not enforce.
///
/// [`translate`] deliberately reports only what the CPU enforces, and
/// [`UserSpace::walk_user_pages`] deliberately does not: its caller is a `fork`
/// duplicating the space, and a fork that could not see the marker would share an
/// already copy-on-write frame a second time without protecting it again - the
/// next write to either copy would then fault with nothing to resolve it to. The
/// x86_64 twin's walk makes the same exception for the same reason.
fn walk_flags(descriptor: u64) -> MapFlags {
    let mut flags = flags_of(descriptor);
    if descriptor & COW_BIT != 0 {
        flags |= MapFlags::COW;
    }
    flags
}

/// The level-3 leaf for `virt` under `root`, as `(table, index, entry)`.
///
/// `None` when nothing is mapped there, and `None` for a block descriptor
/// part-way down as well. Nothing here writes blocks, so a caller that treated
/// one as a page - clearing it, re-flagging it - would touch one frame of a 2 MiB
/// span and leave the other five hundred mappings in place.
fn leaf_of(root: usize, virt: usize) -> Option<(usize, usize, u64)> {
    if root == 0 {
        return None;
    }
    let mut table = root;
    for level in 0..3 {
        let entry = read_entry(table, index_of(virt, level));
        if entry & VALID == 0 || entry & TABLE == 0 {
            return None;
        }
        table = (entry & ADDR_PAGE) as usize;
    }
    let index = index_of(virt, 3);
    let entry = read_entry(table, index);
    if entry & VALID == 0 || entry & TABLE == 0 {
        return None;
    }
    Some((table, index, entry))
}

/// Point `TTBR0_EL1` at `root`, with the barriers the architecture requires.
///
/// `dsb ish` so anything written into the tables is visible to the walker
/// before the register names them, `tlbi vmalle1` because the walker has cached
/// the *previous* root's translations and its failed lookups - neither of which
/// describes this address space - and `isb` so the next instruction is fetched
/// with the new register in effect.
///
/// # Safety
///
/// `root` must be a complete low half for whatever the caller is about to do
/// with it. The kernel's own root is the one value always safe to install: its
/// low half is empty, and every address the kernel executes from, reads or
/// writes is in `TTBR1_EL1`, which this does not touch.
unsafe fn load_root0(root: usize) {
    // SAFETY: the caller's contract.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {root}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root as u64,
        );
    }
}

/// Drop this core's cached translations.
///
/// `vmalle1` rather than an address-tagged invalidation for the same reason
/// [`load_root0`] uses it: this machine does not use ASIDs yet, so there is no
/// tag to name. Over-broad and correct is the trade this port makes until there
/// is more than one space to tell apart.
///
/// # Safety
///
/// The caller must know that dropping these cannot make the address it is
/// executing from unreachable - true for everything in `TTBR1_EL1`, which is
/// where the kernel's own code, stack, heap and direct map live.
unsafe fn flush_tlb() {
    // SAFETY: the caller's contract.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nomem, nostack),
        );
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
/// What is here is what stage 6a runs and stage 6b completes: a fresh root, the
/// mappings a process is allowed to reach, the switch that installs it, the walk
/// that asks what an address in it maps to, and the operations a `fork` and a
/// demand-paged `mmap` are written out of - taking a mapping back, changing one's
/// permissions, and the copy-on-write half that lets two processes share a frame
/// until one of them writes it.
///
/// The methods below are deliberately in the *shapes*
/// [`crate::hal::AddressSpaceOps`] declares - `map` takes a [`Mapping`], `unmap`
/// answers frames, `try_map` answers [`PageTaken`] - even though that trait
/// cannot be implemented for this type yet: `crate::hal` is x86_64-only until
/// all six of its concern traits exist, so `crate::hal::AddressSpaceOps` does not
/// name anything on this machine. Writing them to the trait's shapes now is what
/// makes the impl, when `hal` reaches here, delegation and nothing else.
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

    /// Add one mapping, a page at a time. It need not be the active space.
    ///
    /// Asserts rather than refusing a mapping that reaches into the kernel half:
    /// `TTBR0_EL1` cannot name one, so a caller that asked for it has made a
    /// mistake the hardware would turn into a fault somewhere else entirely.
    ///
    /// It deliberately does *not* require `USER`. `PROT_NONE` is a mapping with
    /// no permission at all - one the process must not touch - and on this
    /// machine that is a page EL1 may reach and EL0 may not, which is a shape a
    /// user space has to be able to hold. The x86_64 twin's `map` makes the same
    /// exception, and its comment records what breaks without it: musl's
    /// `mallocng` claims the guard page of its heap with `MAP_FIXED` and
    /// `PROT_NONE`, and a kernel that refused it would answer a request it is
    /// supposed to serve.
    pub fn map(&mut self, mapping: Mapping) {
        assert!(
            mapping.is_page_granular() && mapping.length > 0,
            "map({mapping:?}) is not whole pages"
        );
        assert!(
            mapping.end() <= USER_END,
            "{mapping:?} reaches the kernel half, which TTBR0_EL1 cannot name"
        );
        let attrs = attributes(mapping.flags);
        let mut tables = Tables::rooted(self.root);
        let mut offset = 0;
        while offset < mapping.length {
            tables.map_page(mapping.virt + offset, mapping.phys + offset, attrs);
            offset += PAGE_SIZE;
        }
        self.flush_if_active();
    }

    /// Add one whole page, answering [`PageTaken`] where [`map`](Self::map)
    /// asserts.
    ///
    /// The kernel's own plan is built once and is disjoint by construction, so a
    /// collision there is a bug and [`map`](Self::map) treats it as one. A
    /// process's request is not the plan: `MAP_FIXED` on an address that already
    /// holds something is an ordinary thing for a libc to do, so a syscall
    /// handler has to be able to *answer* such a request instead of taking the
    /// machine down with it.
    pub fn try_map(&mut self, mapping: Mapping) -> Result<(), PageTaken> {
        assert!(
            mapping.length == PAGE_SIZE
                && mapping.virt.is_multiple_of(PAGE_SIZE)
                && mapping.phys.is_multiple_of(PAGE_SIZE),
            "{mapping:?} is not a single whole page"
        );
        assert!(
            mapping.end() <= USER_END,
            "{mapping:?} reaches the kernel half, which TTBR0_EL1 cannot name"
        );
        if leaf_of(self.root, mapping.virt).is_some() {
            return Err(PageTaken);
        }
        let mut tables = Tables::rooted(self.root);
        tables.map_page(mapping.virt, mapping.phys, attributes(mapping.flags));
        self.flush_if_active();
        Ok(())
    }

    /// Take back every mapping that covers `virt..virt + len`, answering the
    /// frames that were mapped there, lowest first.
    ///
    /// Nothing is freed here, for the twin's reason: a frame may already be on a
    /// list its owner keeps for the reaper, and freeing it twice would hand one
    /// frame to two owners. What the caller does with the frames it gets back is
    /// the caller's business - `munmap` releases them, a `fork`'s failed child
    /// keeps them - and a cleared entry reads as unmapped to the CPU, to
    /// [`translate`](Self::translate) and to `phys_for_virt` alike.
    ///
    /// The tables left empty on the way down are not freed: the descent is the
    /// expensive part, and a process unmapping one page of a long range would pay
    /// for it again at the next `map`. [`Drop`] is where a space's tables go back.
    pub fn unmap(&mut self, virt: usize, len: usize) -> alloc::vec::Vec<usize> {
        let mut dropped = alloc::vec::Vec::new();
        if len == 0 {
            return dropped;
        }
        assert!(
            virt.checked_add(len).is_some_and(|end| end <= USER_END),
            "unmapping {len:#x} bytes at {virt:#x} reaches the kernel half"
        );
        let (first, last) = (virt & !(PAGE_SIZE - 1), virt + len);
        let mut page = first;
        while page < last {
            if let Some((table, index, entry)) = leaf_of(self.root, page) {
                write_entry(table, index, 0);
                dropped.push((entry & ADDR_PAGE) as usize);
            }
            page += PAGE_SIZE;
        }
        self.flush_if_active();
        dropped
    }

    /// Rewrite the permissions of the mappings that already cover
    /// `virt..virt + len`, leaving their frames where they are.
    ///
    /// This is what makes `mprotect` mean something: `PROT_NONE` is mapped but
    /// unreachable, not unmapped, so the bytes survive a round trip back to
    /// readable. A page that is not mapped is skipped - there is nothing to
    /// re-flag, and the range as a whole is what the caller hears about.
    ///
    /// The whole attribute field of the descriptor is replaced, so every flag the
    /// caller did not ask for goes, `COW` included: it is a flag *of the
    /// mapping*, and the trait's wording makes the caller's set the new one.
    /// What the hardware never had - `AF` and the shareability - is re-asserted
    /// by [`attributes`] rather than inherited from the descriptor, because a
    /// descriptor written without `AF` faults on its first access.
    pub fn protect(&mut self, virt: usize, len: usize, flags: MapFlags) {
        if len == 0 {
            return;
        }
        assert!(
            virt.checked_add(len).is_some_and(|end| end <= USER_END),
            "protecting {len:#x} bytes at {virt:#x} reaches the kernel half"
        );
        let (first, last) = (virt & !(PAGE_SIZE - 1), virt + len);
        let mut page = first;
        while page < last {
            if let Some((table, index, entry)) = leaf_of(self.root, page) {
                write_entry(
                    table,
                    index,
                    (entry & ADDR_PAGE) | VALID | TABLE | attributes(flags),
                );
            }
            page += PAGE_SIZE;
        }
        self.flush_if_active();
    }

    /// Map one page copy-on-write: the frame is shared, and the page is read-only
    /// until somebody writes it.
    ///
    /// Only a page that is *logically writable* is marked. A read-only page - a
    /// program's text, a `PROT_READ` mapping - is shared as it stands: neither
    /// process may write it, so there is nothing to copy, and marking it would
    /// turn a write that should fault into a private copy nobody asked for.
    ///
    /// A page that came from a walk of an already-forked space carries `COW`
    /// instead of `WRITE` - it is writable, just not written yet - which is why
    /// the two count the same here. Sharing it without re-marking it would leave
    /// the new space with a page that is read-only and nothing to resolve.
    pub fn map_cow(&mut self, virt: usize, phys: usize, flags: MapFlags) {
        assert!(
            virt.is_multiple_of(PAGE_SIZE) && phys.is_multiple_of(PAGE_SIZE),
            "map_cow({virt:#x}, {phys:#x}) is not whole pages"
        );
        assert!(
            virt.checked_add(PAGE_SIZE)
                .is_some_and(|end| end <= USER_END),
            "map_cow({virt:#x}) reaches the kernel half"
        );
        let writable = flags.contains(MapFlags::WRITE) || flags.contains(MapFlags::COW);
        let mut shared = flags;
        shared.remove(MapFlags::WRITE);
        let mut attrs = attributes(shared);
        if writable {
            attrs |= COW_BIT;
        }
        let mut tables = Tables::rooted(self.root);
        tables.map_page(virt, phys, attrs);
        self.flush_if_active();
    }

    /// Map a page as a genuinely *shared* frame: the same physical page in more
    /// than one space, writable in both, with no copy-on-write marker.
    ///
    /// The difference from [`map_cow`](Self::map_cow) is the whole of what makes a
    /// frame-backed mapping shared across a `fork` - a `memfd`, the unnamed
    /// object behind `MAP_SHARED | MAP_ANONYMOUS`, a dma-buf, the framebuffer.
    /// Such a mapping exists so that two mappings watch each other's writes, and
    /// marking its pages copy-on-write would turn the first store on either side
    /// into a private copy: the sharing would be broken by the very mechanism
    /// meant to preserve it. Both processes map the frame writable and both hold a
    /// reference, so it is released when the last of them lets go.
    pub fn map_shared(&mut self, virt: usize, phys: usize, flags: MapFlags) {
        let mut flags = flags;
        // The marker is not a hardware permission, and it must not travel into a
        // page that is meant to be written in place.
        flags.remove(MapFlags::COW);
        self.map(Mapping {
            virt,
            phys,
            length: PAGE_SIZE,
            flags,
        });
    }

    /// Resolve a write to a copy-on-write page: take a frame of its own, copy the
    /// bytes, make the mapping writable, and answer `(old_phys, new_phys)`.
    ///
    /// The pair matters because the frame lists have to follow along: the caller
    /// now owns `new` instead of `old`, and `old` has one fewer owner.
    /// `None` when there is no mapping at `virt` or it is not copy-on-write, which
    /// is what a fault handler asks first.
    pub fn resolve_cow(&mut self, virt: usize) -> Option<(usize, usize)> {
        let virt = virt & !(PAGE_SIZE - 1);
        let (table, index, entry) = leaf_of(self.root, virt)?;
        if entry & COW_BIT == 0 {
            return None;
        }
        let old = (entry & ADDR_PAGE) as usize;
        let new = mm::alloc_frames(0)?;
        // SAFETY: both are frames of RAM the direct map reaches. `old` is the
        // frame the descriptor names, `new` came from the allocator a moment ago
        // and is nobody else's, and the two cannot overlap because the allocator
        // never hands out a frame it has already given away.
        unsafe {
            core::ptr::copy_nonoverlapping(
                mm::phys_to_virt(old) as *const u8,
                mm::phys_to_virt(new) as *mut u8,
                PAGE_SIZE,
            );
        }
        // The permissions the descriptor already had, plus the write it was
        // downgraded for - and without the marker, because a descriptor built by
        // `attributes` cannot carry one. Deriving them from the descriptor rather
        // than from a constant is what keeps a non-executable page
        // non-executable, and a page the user may not reach out of the user's
        // hands, through the copy.
        let flags = flags_of(entry) | MapFlags::WRITE;
        write_entry(
            table,
            index,
            (new as u64 & ADDR_PAGE) | VALID | TABLE | attributes(flags),
        );
        self.flush_if_active();
        Some((old, new))
    }

    /// Walk every mapped page of this space, lowest address first, handing
    /// `(virt, phys, flags)` to `f`.
    ///
    /// Used by `fork` to duplicate the space, which is why the flags it reports
    /// include `COW` - see [`walk_flags`].
    pub fn walk_user_pages(&self, mut f: impl FnMut(usize, usize, MapFlags)) {
        if self.root != 0 {
            walk_level(self.root, 0, 0, &mut f);
        }
    }

    /// Mark the page at `virt` copy-on-write in this space's own tables, and make
    /// it read-only.
    ///
    /// In *this space's* tables and not the active ones, because the space being
    /// marked is usually not the one running: a `fork` marks the parent's pages,
    /// and the parent is the task making the call.
    pub fn set_cow_bit(&self, virt: usize) {
        self.set_cow_bit_quietly(virt);
        self.flush_if_active();
    }

    /// [`set_cow_bit`](Self::set_cow_bit) without the flush, for a caller marking
    /// a whole space and flushing once at the end.
    ///
    /// The flush is the expensive half: it drops every translation the process
    /// owns, so a `fork` that marked ten thousand pages would ask the CPU to walk
    /// them again once per page - for pages the process is about to touch anyway.
    /// Deferring it makes the walk that follows the only one that costs anything,
    /// which is what [`flush_if_active`](Self::flush_if_active) is for.
    ///
    /// The safety is a caller's contract rather than a type's: until the flush the
    /// CPU may be holding writable translations for pages this has just made
    /// read-only, so nothing may run on this space in between. That is true for
    /// `fork`, which is inside a syscall and marks the space of the task making
    /// it.
    pub fn set_cow_bit_quietly(&self, virt: usize) {
        let virt = virt & !(PAGE_SIZE - 1);
        if let Some((table, index, entry)) = leaf_of(self.root, virt) {
            // Both bits: the marker the kernel reads back, and the read-only bit
            // the CPU enforces. A page marked copy-on-write but left writable
            // would never fault, so the copy would never happen and the two
            // spaces would share one frame after all.
            write_entry(table, index, entry | COW_BIT | AP_READ_ONLY);
        }
    }

    /// Drop this space's translations if it is the one the CPU is using.
    ///
    /// The one flush a batch of [`set_cow_bit_quietly`](Self::set_cow_bit_quietly)
    /// calls owes. Nothing happens when the space is not the active one, which is
    /// the common case: no CPU is translating through those tables, so there is
    /// nothing cached to drop.
    pub fn flush_if_active(&self) {
        if self.is_active() {
            // SAFETY: `TTBR0_EL1` already names this root, so the invalidation
            // cannot make the address this is executing from unreachable - the
            // kernel's own mappings are all in `TTBR1_EL1`.
            unsafe { flush_tlb() };
        }
    }

    /// The frame mapped at `virt` in this space, if any.
    pub fn phys_for_virt(&self, virt: usize) -> Option<usize> {
        walk_from(self.root, virt).map(|(phys, _, _)| phys)
    }

    /// Whether `TTBR0_EL1` is naming this space right now.
    ///
    /// Asked by comparing the static the switch maintains, not by reading the
    /// register: the two agree because [`switch`](Self::switch) is the only thing
    /// that writes either, and a `mrs` in a hot path to learn something the
    /// static already says would be a second source of truth.
    fn is_active(&self) -> bool {
        TTBR0.load(Ordering::Relaxed) == self.root
    }

    /// Leave for the kernel's own root if this space is the installed one.
    ///
    /// `TTBR1_EL1` is the value installed: it names the root the machine ran on
    /// before any user space existed, whose low half is empty, so after this the
    /// CPU resolves nothing below 2^48 - which is exactly the state a process exit
    /// leaves the machine in.
    fn leave_if_active(&self) {
        if !self.is_active() {
            return;
        }
        let kernel = TTBR1.load(Ordering::Relaxed);
        // SAFETY: `kernel` is the root the machine booted on; every address this
        // code, its stack, the heap and the direct map live at is in `TTBR1_EL1`,
        // which this does not touch.
        unsafe { load_root0(kernel) };
        TTBR0.store(kernel, Ordering::Relaxed);
    }

    /// Make this space the one `TTBR0_EL1` names, and this core's translations
    /// for the low half follow it.
    pub fn switch(&self) {
        // SAFETY: `self.root` is a hierarchy this type built, and every kernel
        // address the machine is executing from or reading is in `TTBR1`, which
        // this does not touch.
        unsafe { load_root0(self.root) };
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

/// `crate::hal::AddressSpaceOps`, which stage 6b wrote the methods *for* - they
/// take a [`Mapping`], answer the frames a range held, mark the marker in bit 55
/// beside the read-only bit the CPU enforces. Now that the trait compiles on this
/// machine the impl is delegation and nothing else: each method forwards to the
/// inherent one of the same name, which Rust resolves to the inherent method (the
/// trait method it sits in never shadows it), so this block is signatures over the
/// code stage 6b already tested. `new` builds the root directly rather than
/// forwarding, so the allocator's "cannot be recovered from" is written once.
impl crate::hal::AddressSpaceOps for UserSpace {
    fn new() -> Self {
        let root = mm::alloc_frames(0)
            .expect("out of frames for a user address space, which cannot be recovered from");
        // SAFETY: a fresh frame the allocator handed over and nothing else holds,
        // reached through the direct map; an empty entry must be an absent one.
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(root) as *mut u8, 0, PAGE_SIZE) };
        UserSpace { root }
    }

    fn root(&self) -> usize {
        self.root
    }

    fn map(&mut self, mapping: Mapping) {
        self.map(mapping)
    }

    fn try_map(&mut self, mapping: Mapping) -> Result<(), PageTaken> {
        self.try_map(mapping)
    }

    fn unmap(&mut self, virt: usize, len: usize) -> alloc::vec::Vec<usize> {
        self.unmap(virt, len)
    }

    fn protect(&mut self, virt: usize, len: usize, flags: MapFlags) {
        self.protect(virt, len, flags)
    }

    fn switch(&self) {
        self.switch()
    }

    fn map_cow(&mut self, virt: usize, phys: usize, flags: MapFlags) {
        self.map_cow(virt, phys, flags)
    }

    fn map_shared(&mut self, virt: usize, phys: usize, flags: MapFlags) {
        self.map_shared(virt, phys, flags)
    }

    fn resolve_cow(&mut self, virt: usize) -> Option<(usize, usize)> {
        self.resolve_cow(virt)
    }

    fn walk_user_pages(&self, f: impl FnMut(usize, usize, MapFlags)) {
        self.walk_user_pages(f)
    }

    fn set_cow_bit(&self, virt: usize) {
        self.set_cow_bit(virt)
    }

    fn set_cow_bit_quietly(&self, virt: usize) {
        self.set_cow_bit_quietly(virt)
    }

    fn flush_if_active(&self) {
        self.flush_if_active()
    }

    fn phys_for_virt(&self, virt: usize) -> Option<usize> {
        self.phys_for_virt(virt)
    }
}

/// Leaving, and giving the tables back.
///
/// Dropping a space frees the page tables it owns and nothing else. In
/// particular the frames its leaves point *at* belong to whoever allocated them,
/// so a process's memory outlives its address space and is released separately -
/// and freeing a leaf's frame here would hand a live frame to two owners, which
/// is the one mistake a reaper cannot survive.
impl Drop for UserSpace {
    fn drop(&mut self) {
        // Never tear down the hierarchy the CPU is walking: leave first, then
        // free. The same order a process exit takes, and the reason this is not
        // simply a walk that frees as it goes.
        self.leave_if_active();
        free_subtables(self.root, 0);
        mm::free_frames(self.root, 0);
    }
}

/// Free every page table hanging off `table`, which sits at `level`.
///
/// The tables *between* the root and the leaves, which is what a space owns: the
/// leaves themselves are mapped memory rather than tables, and are skipped rather
/// than followed. A block descriptor is skipped too, for [`leaf_of`]'s reason -
/// its child address is a mapping, not a table.
fn free_subtables(table: usize, level: usize) {
    if level == 3 {
        return;
    }
    for index in 0..ENTRY_COUNT {
        let entry = read_entry(table, index);
        if entry & VALID == 0 || entry & TABLE == 0 {
            continue;
        }
        let child = (entry & ADDR_PAGE) as usize;
        free_subtables(child, level + 1);
        mm::free_frames(child, 0);
    }
}

/// [`UserSpace::walk_user_pages`]'s descent: the leaves under `table`, which sits
/// at `level` and covers everything from `base` upwards.
fn walk_level(table: usize, level: usize, base: usize, f: &mut impl FnMut(usize, usize, MapFlags)) {
    for index in 0..ENTRY_COUNT {
        let entry = read_entry(table, index);
        if entry & VALID == 0 {
            continue;
        }
        let virt = base | (index << (12 + 9 * (3 - level)));
        // Nothing below `USER_END` is skipped on this machine - `TTBR0_EL1`'s
        // whole range *is* the low half - and the bound is here because it is the
        // x86_64 twin's shape and because the kernel's own root is one register
        // away: a walk pointed at that one would otherwise report the direct map
        // as a process's memory.
        if virt >= USER_END {
            continue;
        }
        if level == 3 {
            f(virt, (entry & ADDR_PAGE) as usize, walk_flags(entry));
            continue;
        }
        // A block below level 3 has no place in a space this type built:
        // `map_page` writes pages and nothing else. Refusing it rather than
        // reporting its first frame is what keeps a `fork` from sharing a
        // sixteenth of a mapping and believing it copied the whole thing -
        // `Tables::next_table` refuses a block in its way for the same reason.
        assert!(
            entry & TABLE != 0,
            "a block descriptor at {table:#x}[{index}] is in a user space, which maps pages"
        );
        walk_level((entry & ADDR_PAGE) as usize, level + 1, virt, f);
    }
}
