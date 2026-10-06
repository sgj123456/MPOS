//! The x86_64 machine as the loader sees it: the descriptor format, the COM1
//! wire, and the switch into the higher half.
//!
//! Everything around this file - the three ranges, where the frames come from,
//! the sizing of the pool - belongs to the loader (`mpos_boot::vm`). What is
//! here is only what is the machine's, and it is the same division
//! `crate::arch::x86_64` makes one level up.
//!
//! The descriptor format is written to match `crate::arch::x86_64::paging` bit
//! for bit, because the kernel's own tables decode what this file writes and the
//! two spellings drifting apart would turn a working mapping into a check that
//! names a permission nobody set. They are the same bits; what differs is when
//! they run - see the module above.

use super::{Frames, PAGE_SIZE};

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12..=51 hold the address of the next table or of the page itself.
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

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

    /// The frame the CPU is pointed at to use this hierarchy.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// The frame source, for the one caller that reports how many were taken.
    pub fn pool(&self) -> &F {
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
        let frame = self.pool.zeroed_frame();
        *slot = frame | PRESENT | WRITABLE;
        frame
    }
}

/// Which entry of the `level`-th table from the root covers `virt`. Level 3 is
/// the root itself.
fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1ff) as usize
}

// ---------------------------------------------------------------------------
// What this machine reaches beyond RAM
// ---------------------------------------------------------------------------

/// Nothing. This machine's loader maps no device range: the kernel reaches MMIO
/// by walking a BAR through `hal::Paging::map_mmio_window`, and the loader has
/// no business guessing where a device is.
pub fn extra_mapped_bytes() -> u64 {
    0
}

/// The other half of [`extra_mapped_bytes`]: there is nothing to map, so
/// nothing is mapped and nothing is logged.
pub fn map_extra<F: Frames>(_tables: &mut PageTables<F>) {}

/// No device tree.
///
/// This machine's firmware describes it with ACPI, and this port reads none of
/// it, so the loader looks for no tree and the handoff record says "absent"
/// rather than the kernel being handed an empty one it would have to
/// special-case.
pub const FIRMWARE_PUBLISHES_DEVICE_TREE: bool = false;

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

const COM1: u16 = 0x3f8;

fn out(port: u16, value: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack))
    }
}

fn inp(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack))
    };
    value
}

/// Bring the 16550 up to 115200 8N1, FIFO on.
///
/// A UART is a port and an address, and it works before, during and after boot
/// services - which is why the loader's progress lines go here and not to the
/// firmware's console.
pub fn serial_init() {
    out(COM1 + 1, 0x00); // no interrupts
    out(COM1 + 3, 0x80); // divisor latch
    out(COM1, 0x01); // 115200 baud
    out(COM1 + 1, 0x00);
    out(COM1 + 3, 0x03); // 8 bits, no parity, one stop
    out(COM1 + 2, 0xc7); // FIFO on, cleared
    out(COM1 + 4, 0x03); // ready to transmit
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
    while inp(COM1 + 5) & 0x20 == 0 {
        core::hint::spin_loop();
    }
    out(COM1, byte);
}

// ---------------------------------------------------------------------------
// The last instruction
// ---------------------------------------------------------------------------

/// Hand the machine to the kernel: `mov cr3` and `jmp`.
///
/// # Safety
///
/// `record` must point at the loader's filled-in handoff record, `entry` must be
/// mapped executable in the page tables rooted at `root`, `stack_top` must be
/// mapped writable in those same tables, and all three must be reachable from
/// the higher-half addresses they are given as. This is the last thing the
/// loader does and there is no way back from it.
///
/// # Why the order is the architecture's
///
/// Interrupts go off first: the firmware may have left them on, and a timer
/// between the two instructions below would be delivered on a stack that has
/// already moved. The stack is set one word *down* from `stack_top`, because
/// x86_64's first instruction is a `push` of the return address its own calling
/// convention expects.
pub unsafe fn enter(record: *const u8, entry: u64, stack_top: u64, root: u64) -> ! {
    unsafe {
        core::arch::asm!(
            // Interrupts off for the switch: the firmware may have left them on,
            // and a timer between the two instructions below would be delivered
            // on a stack that has already moved.
            "cli",
            "mov cr3, {cr3}",
            // The kernel half was not mapped a moment ago, so this cannot be
            // hoisted above the CR3 write.
            "mov rsp, {stack}",
            "xor rbp, rbp",
            "jmp {entry}",
            cr3 = in(reg) root,
            stack = in(reg) stack_top - 8,
            entry = in(reg) entry,
            in("rdi") record,
            options(noreturn)
        )
    }
}
