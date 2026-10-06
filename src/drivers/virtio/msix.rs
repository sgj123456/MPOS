//! MSI-X for a virtio PCI function: program the vector table by hand, pin the
//! queues to vector 0, and let the device interrupt the kernel directly.
//!
//! The `virtio-drivers` crate is written for polling - it never touches the
//! MSI-X capability, and its queues stay bound to `VIRTIO_MSI_NO_VECTOR`,
//! which means "never interrupt". What a polled kernel saves in machinery it
//! pays back in latency: a frame that arrives while nobody is polling waits in
//! the ring until the next poll tick. The two steps below are everything
//! needed to close that gap:
//!
//!   * [`enable`] walks the PCI capability list, maps the MSI-X table out of
//!     its BAR, and points entry 0 at one IDT vector - a memory write the
//!     local APIC turns into that interrupt on this CPU, bypassing the 8259
//!     and the IOAPIC entirely.
//!   * [`set_queue_vectors`] walks the same list for the virtio common
//!     configuration and binds every queue to that vector, which is the
//!     register the crate leaves at "no vector" and the device reads to know
//!     whom to tell.
//!
//! Both run once, at driver bring-up; failure of either is answered with
//! `false` and the caller carries on polling, which is what the kernel did
//! before any of this existed.

use crate::drivers::pci;

/// The PCI capability that carries the MSI-X table.
const PCI_CAP_ID_MSIX: u8 = 0x11;
/// The PCI capability virtio structures live in (vendor-defined).
const PCI_CAP_ID_VNDR: u8 = 0x09;
/// The virtio vendor capability's type byte for the common configuration.
const VIRTIO_CAP_COMMON_CFG: u8 = 0x01;
/// ...and for the device's own configuration, where a device keeps the state
/// that is not a queue: `virtio-gpu`'s `events_read` is the one this kernel
/// reads through it.
const VIRTIO_CAP_DEVICE_CFG: u8 = 0x04;

/// `MSI-X Message Control`, bit 15: the function starts delivering its
/// interrupts as memory writes.
const MSIX_ENABLE: u16 = 1 << 15;
/// `MSI-X Message Control`, bit 14: a mask over every entry at once, cleared
/// so the entries' own masks are the only ones in force.
const MSIX_FUNCTION_MASK: u16 = 1 << 14;

/// The address an MSI writes to reach the local APIC of the boot CPU: physical
/// destination mode, destination 0, edge-triggered - the plainest shape a
/// message-signalled interrupt can take.
const MSI_ADDRESS: u64 = 0xFEE0_0000;

/// Where the registers sit inside `virtio_pci_common_cfg` (virtio 1.0,
/// 4.1.4.3) - only the ones this module writes.
const CFG_MSIX_CONFIG: usize = 16;
const CFG_NUM_QUEUES: usize = 18;
const CFG_QUEUE_SELECT: usize = 22;
const CFG_QUEUE_MSIX_VECTOR: usize = 26;

/// `VIRTIO_MSI_NO_VECTOR`: the value that tells the device to tell nobody.
const NO_VECTOR: u16 = 0xFFFF;

/// Walk the capability list and hand back the offset of capability `id`.
fn find_cap(func: &pci::Func, id: u8) -> Option<u8> {
    let (bus, slot, function) = func.bdf();
    // Status register, bit 4: the capability list exists at all.
    let status = pci::read16(bus, slot, function, 0x06);
    if status & (1 << 4) == 0 {
        return None;
    }
    let mut next = (pci::read32(bus, slot, function, 0x34) & 0xff) as u8;
    while next != 0 {
        let this = next;
        // The header reads as one dword: id in the low byte, next pointer
        // behind it. Both lists are dword-aligned by construction.
        let header = pci::read32(bus, slot, function, this);
        next = (header >> 8) as u8;
        if (header & 0xff) as u8 == id {
            return Some(this);
        }
    }
    None
}

/// The capability after `cap`, for a walk that is already underway.
fn find_next(func: &pci::Func, cap: u8) -> Option<u8> {
    let (bus, slot, function) = func.bdf();
    let next = (pci::read32(bus, slot, function, cap) >> 8) as u8;
    (next != 0).then_some(next)
}

/// The memory address behind BAR `bir`, or `None` if it is not a memory BAR.
///
/// A 64-bit BAR keeps its upper half in the following register: the firmware
/// is free to place such a BAR above 4 GiB, where reading only the low dword
/// answers with a low physical address that happens to be ordinary RAM - and
/// every register "write" to it round-trips through memory while the device
/// never hears a word. The type lives in bits 2:1 of the BAR: `10` is the
/// 64-bit spelling, and it is the one this must not miss.
fn bar_address(func: &pci::Func, bir: u8) -> Option<usize> {
    let (bus, slot, function) = func.bdf();
    let bar = pci::read32(bus, slot, function, 0x10 + 4 * bir);
    // Bit 0 chooses I/O over memory; the low four bits of a memory BAR are
    // flags and the address is the rest.
    if bar & 1 != 0 {
        return None;
    }
    let mut base = u64::from(bar & !0xf);
    if bar & 0x6 == 0x4 {
        // 64-bit: the next register holds the high half, and there is no
        // next register after the last BAR.
        if bir >= 5 {
            return None;
        }
        base |= u64::from(pci::read32(bus, slot, function, 0x14 + 4 * bir)) << 32;
    }
    Some(base as usize)
}

/// Map the page `at` lives in and hand back the address of `at` itself. An
/// MMIO window is page-aligned; a structure inside a BAR is not obliged to
/// be, so the page is what gets mapped and the offset within it is added
/// back. `span` covers the case of a structure that starts near the end of
/// its page.
fn map_structure(at: usize, span: usize) -> usize {
    let page = at & !(crate::mm::PAGE_SIZE - 1);
    crate::hal::map_mmio_window(page, span).expect("out of room for device registers")
        + at % crate::mm::PAGE_SIZE
}

/// The MMIO window of the vendor capability whose `cfg_type` is `want`, or
/// `None` when the function has no such structure.
///
/// A virtio 1.0 function exposes its structures - common configuration,
/// notification area, ISR, device configuration - as vendor capabilities that
/// name a BAR and an offset. Walking them is the same walk for every caller,
/// so it lives here once.
fn vendor_cap_window(func: &pci::Func, want: u8) -> Option<usize> {
    let mut cap = find_cap(func, PCI_CAP_ID_VNDR);
    while let Some(this) = cap {
        let (bus, slot, function) = func.bdf();
        // The vendor capability's private half: cap_len, cfg_type, then the
        // BAR and offset of the structure (virtio 1.0, 4.1.4.3).
        let header = pci::read32(bus, slot, function, this);
        let cfg_type = (header >> 24) as u8;
        let bar = pci::read32(bus, slot, function, this + 4) as u8 & 0x7;
        let offset = pci::read32(bus, slot, function, this + 8);
        cap = find_next(func, this);
        if cfg_type == want {
            let addr = bar_address(func, bar)?;
            return Some(map_structure(addr + offset as usize, 0x1000));
        }
    }
    None
}

/// The virtio *device configuration* window of `func`: the space holding the
/// state a driver polls for the device's own events.
///
/// The crate reads this space once, at bring-up, and guards the read with a
/// generation counter - a value read while the host is writing it is not worth
/// trusting. A driver that has to look *again*, because the host can change
/// something under it while the machine runs, needs the address rather than a
/// snapshot, and this is it.
pub fn device_config_of(func: &pci::Func) -> Option<usize> {
    vendor_cap_window(func, VIRTIO_CAP_DEVICE_CFG)
}

/// Enable MSI-X on `func` and arm entry 0 at `vector`.
///
/// The entry is written masked first and unmasked last, so the device can
/// never raise the vector against a half-programmed entry.
pub fn enable(func: &pci::Func, vector: u8) -> bool {
    let Some(cap) = find_cap(func, PCI_CAP_ID_MSIX) else {
        return false;
    };
    let (bus, slot, function) = func.bdf();
    let control = pci::read16(bus, slot, function, cap + 2);
    // The table's whereabouts: which BAR, and how far into it.
    let table = pci::read32(bus, slot, function, cap + 4);
    let bir = (table & 0x7) as u8;
    let offset = (table & !0x7) as usize;
    let Some(bar) = bar_address(func, bir) else {
        return false;
    };

    // Two pages, so an entry at the very end of the first is still inside
    // the window. This kernel arms a single vector, so no more of the table
    // is ever touched.
    let table = map_structure(bar + offset, 0x2000);
    // Entry 0 is four words: message address, upper address, message data,
    // and the control word whose bit 0 is the entry's own mask.
    let write = |at: usize, value: u32| unsafe {
        ((table + at) as *mut u32).write_volatile(value);
    };
    write(12, 1); // mask the entry while it is being written
    write(0, MSI_ADDRESS as u32);
    write(4, 0); // no x2APIC: the upper half of the address is zero
    write(8, vector as u32); // fixed delivery, edge, assert - the zero fields
    write(12, 0); // unmask: the vector is armed

    // Enable, with the function-wide mask clear: the entry's own mask, off,
    // is the only one in force.
    pci::write16(
        bus,
        slot,
        function,
        cap + 2,
        (control | MSIX_ENABLE) & !MSIX_FUNCTION_MASK,
    );
    true
}

/// Bind every virtqueue of `func` to MSI-X vector 0, and the config-change
/// interrupt to none.
///
/// The common configuration is where `queue_msix_vector` still reads its
/// reset value, `VIRTIO_MSI_NO_VECTOR` - the register that tells the device
/// never to interrupt for that queue. Writing 0 for each queue - select it,
/// write the vector, read it back to confirm the device accepted - is the
/// register side of "interrupt me". The config-change vector stays `NO_VECTOR`
/// on purpose: nothing in this kernel watches for link changes, and a config
/// write it cannot explain would land on the same handler as a frame.
pub fn set_queue_vectors(func: &pci::Func) -> bool {
    // The virtio structures are all in the vendor capability; the one this
    // wants is the common configuration, which may not be the first of them.
    let Some(common) = vendor_cap_window(func, VIRTIO_CAP_COMMON_CFG) else {
        return false;
    };

    // The common configuration is MMIO: every field is a volatile word at
    // its struct offset, and the selects are stateful - queue_select chooses
    // the queue the queue_* registers speak for.
    let read16 = |at: usize| unsafe { ((common + at) as *const u16).read_volatile() };
    let write16 = |at: usize, v: u16| unsafe { ((common + at) as *mut u16).write_volatile(v) };

    // Config changes have no audience here: no vector for them.
    write16(CFG_MSIX_CONFIG, NO_VECTOR);

    let queues = read16(CFG_NUM_QUEUES);
    for queue in 0..queues {
        write16(CFG_QUEUE_SELECT, queue);
        write16(CFG_QUEUE_MSIX_VECTOR, 0);
        if read16(CFG_QUEUE_MSIX_VECTOR) != 0 {
            // The device refused the vector: it would have stayed silent, so
            // say the wiring failed and let the caller keep polling.
            return false;
        }
    }
    // Leave the registers pointing at queue 0, where a reader would expect
    // them; the crate only consults them at bring-up, which has happened.
    write16(CFG_QUEUE_SELECT, 0);
    true
}
