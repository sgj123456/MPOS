//! PCI configuration space: the machine's half of it, reached through the
//! memory-mapped window ACPI names when the firmware names one and through
//! mechanism 1's port pair 0xcf8/0xcfc when it does not.
//!
//! Two jobs live here, and they are the two halves of what a PCI bus is to a
//! kernel. The first is *discovery*: find the function with a given vendor and
//! device id, which is all a driver needs and all this module used to do. The
//! second is the *snapshot*: walk every bus, learn what each function is and
//! what address space it decodes, and hand that to [`crate::pci`] so `/sys` and
//! `/proc` can describe the machine the way `lspci` reads it.
//!
//! Only the bus cycles are here. Every decision made *about* what was read -
//! which bits are a BAR's type, how much space a mask means, how Linux spells
//! the result - is in [`crate::pci`], where a host build can test it without a
//! machine to read.
//!
//! # Two mechanisms, and why the choice is made at boot
//!
//! Mechanism 1 reaches 256 bytes of configuration space per function: its
//! address register has eight bits of offset. A PCIe machine may instead decode
//! configuration space as *memory* - ECAM, or MMCONFIG: one 4 KiB page per
//! function, one 1 MiB window per bus, at an address its ACPI MCFG table names -
//! and Linux prefers that, because the window reaches the whole 4 KiB.
//! `docs/NOTES.md` §7.6 reads the firmware's tables; this module is what uses
//! what they said.
//!
//! A function's *published* size is the device's answer rather than the
//! mechanism's: [`cfg_size`] asks a PCIe function, a host bridge or a PCI-X mode
//! 2 bridge for the dword at 0x100 and keeps 4096 only when that answers
//! something real and the extended space is not the standard header repeated.
//! Linux's `pci_cfg_space_size` is the rule, step for step; on a machine with
//! only the port pair the probe reads all ones and every function stays 256,
//! which is Linux's answer there too.
//!
//! The window is *mapped a bus at a time*, on the first access to that bus
//! ([`slice`]): this machine's MCFG covers 256 buses - 256 MiB of address space -
//! and a walk that touches two or three of them has no business mapping all of
//! it. The port pair is not discarded. It is what a machine with no MCFG reads
//! the bus with, and it is also the *reference* the window is checked against
//! before it is trusted ([`init`]): a window that disagrees with the ports on this
//! machine about any function is not one this kernel reads the bus through.

use alloc::vec::Vec;
use core::cell::RefCell;
use critical_section::Mutex;

use crate::acpi::EcamRange;
use crate::boot::BootInfo;
use crate::pci::{
    Bar, Device, PCI_CAP_ID_PCIE, PCI_CAP_ID_PCIX, PCI_ERROR_RESPONSE, PCI_ROM_ADDRESS_MASK,
    PCI_X_STATUS, PCI_X_STATUS_266MHZ, PCI_X_STATUS_533MHZ, ROM_INDEX, bar_flags, bar_range,
    rom_bar,
};

/// Mechanism 1's address and data ports: only an x86_64 machine has an I/O
/// address space to put them in, and the constant is gate-kept with the module
/// below so that the two facts cannot drift apart.
#[cfg(target_arch = "x86_64")]
const CONFIG_ADDRESS: u16 = 0xcf8;
#[cfg(target_arch = "x86_64")]
const CONFIG_DATA: u16 = 0xcfc;

/// Command register bits.
pub const COMMAND_IO_SPACE: u16 = 1 << 0;
pub const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
/// PCI 2.3 "Interrupt Disable": the device must not assert its INTx line. A
/// polling driver wants this, or every completed request raises an unhandled
/// interrupt.
pub const COMMAND_INTX_DISABLE: u16 = 1 << 10;

// ---------------------------------------------------------------------------
// The window ACPI names, when the firmware names one
// ---------------------------------------------------------------------------

/// The memory-mapped configuration window in use, and the slices of it that have
/// been mapped.
///
/// `None` means the port pair is the mechanism: either because the firmware named
/// no window, or because the one it named did not survive [`init`]'s check.
struct Window {
    /// What MCFG said, kept whole because the log line and the per-bus physical
    /// address both come off it.
    range: EcamRange,
    /// The kernel-half address of each bus's mebibyte of configuration space,
    /// indexed by bus number.
    ///
    /// Zero means *not yet mapped*, and that is unambiguous rather than a
    /// convention to remember: no mapping this machine hands out is ever at
    /// address zero, because the MMIO window sits just below the kernel image.
    /// Indexed rather than searched because the index *is* the bus number - 256
    /// entries of `usize` say exactly as much as a list of the buses mapped so far
    /// would, and cost no walk of that list on every access.
    slices: [usize; 256],
}

static WINDOW: Mutex<RefCell<Option<Window>>> = Mutex::new(RefCell::new(None));

/// The kernel-half address of `bus`'s mebibyte of the window, mapping it if this
/// is the first access to that bus.
///
/// One mebibyte per bus rather than the whole window at once: this machine's MCFG
/// covers buses 0..=255, and a walk that touches two or three of them would be
/// mapping 256 MiB of address space to read a few kilobytes of it. A slice is
/// never handed back - the bus is walked for the machine's life, and a driver
/// probing a device an hour later asks for the same one.
///
/// `None` means the port pair is this access's mechanism: no window is in use,
/// the window does not cover `bus`, or the machine has no room left for a mapping
/// - the honest refusal rather than an address nothing answers for.
fn slice(bus: u8) -> Option<usize> {
    // Three steps rather than one guard, because the middle one allocates frames
    // and writes page tables and that is not work to do with interrupts off. The
    // two steps cannot disagree: this is the only writer of a window's `slices`,
    // `use_window` and `use_ports` run at most once each and only at boot, and a
    // task preempted here could only find the entry still zero and map the bus a
    // second time - a mapping nothing hands back, of the same bytes either way.
    let (mapped, range) = critical_section::with(|cs| {
        let window = WINDOW.borrow(cs).borrow();
        let window = window.as_ref()?;
        // A bus outside the window has no slice, and reading the table anyway
        // would hand back the *wrong* bus's memory.
        window.range.offset(bus, 0, 0)?;
        Some((window.slices[bus as usize], window.range))
    })?;
    if mapped != 0 {
        return Some(mapped);
    }

    let phys = range.base + (bus - range.start_bus) as u64 * crate::acpi::ECAM_BUS_BYTES;
    let virt = crate::hal::map_mmio_window(phys as usize, crate::acpi::ECAM_BUS_BYTES as usize)?;
    critical_section::with(|cs| {
        if let Some(window) = WINDOW.borrow(cs).borrow_mut().as_mut() {
            window.slices[bus as usize] = virt;
        }
    });
    Some(virt)
}

/// The kernel-half address of one configuration register inside the window, or
/// `None` when the port pair is this access's mechanism.
///
/// `off` must already be aligned to the access size the caller makes; the callers
/// below mask it the same way the port mechanism's address register does, so
/// "which mechanism read this" cannot change which bytes were touched.
///
/// The layout is [`crate::acpi::function_offset`]'s - written down beside the
/// reader that validated the table it came from, and not a second time here.
fn window_byte(bus: u8, dev: u8, func: u8, off: u16) -> Option<usize> {
    let slice = slice(bus)?;
    Some(slice + crate::acpi::function_offset(dev, func)? as usize + off as usize)
}

/// Read configuration space through this window from here on.
fn use_window(range: EcamRange) {
    critical_section::with(|cs| {
        *WINDOW.borrow(cs).borrow_mut() = Some(Window {
            range,
            slices: [0; 256],
        });
    });
}

/// Read configuration space through the port pair from here on.
///
/// The slices a window mapped are left where they are rather than unmapped: this
/// machine never hands a mapping back, and a page nothing reads costs nothing.
fn use_ports() {
    critical_section::with(|cs| *WINDOW.borrow(cs).borrow_mut() = None);
}

// ---------------------------------------------------------------------------
// Mechanism 1: the port pair, and what a machine with no ports answers
// ---------------------------------------------------------------------------

/// The machine's own fallback for a configuration access the window did not
/// serve, as one name on both machines.
///
/// The two arms are the two kinds of machine this kernel boots. An x86_64 one
/// reaches configuration space through mechanism 1's address and data ports,
/// which exist whether or not the firmware named a window; an aarch64 one has
/// no I/O address space at all - no instruction to reach it and nothing
/// decoded behind one - so on that machine the window is the *only* door, and
/// an access that finds no window is answered the way a PCI bus answers a
/// function that is not there: all ones, the width of the access.
///
/// That is what lets the rest of this module be one body on both machines. The
/// fallback is not "this machine cannot do PCI" - which would make the walk, the
/// snapshot and every driver's `find` a second shape to keep - but "this access
/// found nothing", which is a bus's own answer for an absent function and is
/// already what [`enumerate`] skips.
#[cfg(target_arch = "x86_64")]
mod ports {
    use x86_64::instructions::port::Port;

    use super::{CONFIG_ADDRESS, CONFIG_DATA};

    fn port_address(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
        0x8000_0000
            | (bus as u32) << 16
            | (slot as u32) << 11
            | (func as u32) << 8
            | (off as u32 & 0xfc)
    }

    pub(super) fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA).read()
        }
    }

    pub(super) fn write32(bus: u8, slot: u8, func: u8, off: u8, value: u32) {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA).write(value);
        }
    }

    /// Read one byte of configuration space over the port pair.
    ///
    /// A *byte* access, not a dword with three bytes ignored. Mechanism 1's data
    /// port is four bytes wide and a narrower access reaches only the bytes it
    /// names - which is what makes a byte read see what a byte write wrote.
    pub(super) fn read8(bus: u8, slot: u8, func: u8, off: u8) -> u8 {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA + (off as u16 & 3)).read()
        }
    }

    /// Write one byte of configuration space over the port pair.
    ///
    /// The access size is load-bearing rather than pedantic. A byte write reaches
    /// one byte of the addressed dword; a read-modify-write of the whole dword
    /// would write back the bytes beside it, and the register beside the command
    /// word is the *status* word, whose error bits are cleared by writing ones.
    /// A read-modify-write there would silently throw away every error flag the
    /// device had raised.
    pub(super) fn write8(bus: u8, slot: u8, func: u8, off: u8, value: u8) {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA + (off as u16 & 3)).write(value)
        }
    }

    /// Read a 16-bit configuration register, `off` even, over the port pair.
    pub(super) fn read16(bus: u8, slot: u8, func: u8, off: u8) -> u16 {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA + (off as u16 & 2)).read()
        }
    }

    /// Write a 16-bit configuration register, `off` even, over the port pair.
    pub(super) fn write16(bus: u8, slot: u8, func: u8, off: u8, value: u16) {
        unsafe {
            Port::new(CONFIG_ADDRESS).write(port_address(bus, slot, func, off));
            Port::new(CONFIG_DATA + (off as u16 & 2)).write(value)
        }
    }
}

/// The same six calls on a machine with no port I/O: an absent function's
/// answer, at the width the caller asked for.
///
/// A write is *dropped* rather than panicked on, and that is the PCI answer
/// rather than a shortcut: a write to a function that is not there is discarded
/// by the bus, and a driver that writes to one gets no error from the machine.
/// What must not happen is a *read* inventing a device, which is why the reads
/// are all ones - the value [`enumerate`] tests for.
#[cfg(not(target_arch = "x86_64"))]
mod ports {
    pub(super) fn read32(_bus: u8, _slot: u8, _func: u8, _off: u8) -> u32 {
        super::PCI_ERROR_RESPONSE
    }
    pub(super) fn write32(_bus: u8, _slot: u8, _func: u8, _off: u8, _value: u32) {}
    pub(super) fn read8(_bus: u8, _slot: u8, _func: u8, _off: u8) -> u8 {
        0xff
    }
    pub(super) fn write8(_bus: u8, _slot: u8, _func: u8, _off: u8, _value: u8) {}
    pub(super) fn read16(_bus: u8, _slot: u8, _func: u8, _off: u8) -> u16 {
        0xffff
    }
    pub(super) fn write16(_bus: u8, _slot: u8, _func: u8, _off: u8, _value: u16) {}
}

// ---------------------------------------------------------------------------
// The two mechanisms behind one address
// ---------------------------------------------------------------------------

/// Read a configuration dword.
///
/// Through the window this is one aligned load of device memory - the whole
/// reason ECAM exists is that a 4 KiB page per function can be addressed as
/// memory - and the mask clears the same two address bits mechanism 1's
/// register does. The mask is twelve bits wide because an extended offset is:
/// `0xfc` would clear bits 8..15 and fold 0x100 back onto 0x000, which is what
/// an aliasing bridge looks like.
pub fn read32(bus: u8, slot: u8, func: u8, off: u16) -> u32 {
    match window_byte(bus, slot, func, off & !0x3) {
        // SAFETY: the window is device memory the firmware named and this module
        // mapped, and the address is four-byte aligned inside its page.
        Some(address) => unsafe { core::ptr::read_volatile(address as *const u32) },
        // Beyond the port pair's reach there is no third mechanism: the
        // extended space is a window's alone, and an access with no window
        // answers what an absent function answers rather than reading the
        // low 256 bytes twice.
        None if off > 0xff => PCI_ERROR_RESPONSE,
        None => ports::read32(bus, slot, func, off as u8),
    }
}

/// Write a configuration dword. See [`read32`] for the two mechanisms.
pub fn write32(bus: u8, slot: u8, func: u8, off: u16, value: u32) {
    match window_byte(bus, slot, func, off & !0x3) {
        // SAFETY: as `read32`, and the register was read through the same address
        // arithmetic before anything wrote to it.
        Some(address) => unsafe { core::ptr::write_volatile(address as *mut u32, value) },
        None if off > 0xff => {}
        None => ports::write32(bus, slot, func, off as u8, value),
    }
}

/// Read one byte of configuration space.
///
/// A *byte* access under either mechanism, which is what makes a byte read see
/// what a byte write wrote: the port pair's data port reaches only the bytes a
/// narrower access names, and a memory window is byte-addressable outright.
pub fn read8(bus: u8, slot: u8, func: u8, off: u16) -> u8 {
    match window_byte(bus, slot, func, off) {
        // SAFETY: device memory this module mapped; a byte load has no alignment
        // requirement.
        Some(address) => unsafe { core::ptr::read_volatile(address as *const u8) },
        None if off > 0xff => 0xff,
        None => ports::read8(bus, slot, func, off as u8),
    }
}

/// Write one byte of configuration space.
///
/// The access size is load-bearing rather than pedantic. A byte write reaches
/// one byte of the addressed dword; a read-modify-write of the whole dword
/// would write back the bytes beside it, and the register beside the command
/// word is the *status* word, whose error bits are cleared by writing ones.
/// A read-modify-write there would silently throw away every error flag the
/// device had raised.
pub fn write8(bus: u8, slot: u8, func: u8, off: u16, value: u8) {
    match window_byte(bus, slot, func, off) {
        // SAFETY: as `read8`.
        Some(address) => unsafe { core::ptr::write_volatile(address as *mut u8, value) },
        None if off > 0xff => {}
        None => ports::write8(bus, slot, func, off as u8, value),
    }
}

/// Read a 16-bit configuration register. `off` must be even.
pub fn read16(bus: u8, slot: u8, func: u8, off: u16) -> u16 {
    match window_byte(bus, slot, func, off & !0x1) {
        // SAFETY: device memory this module mapped, at an even address inside its
        // page.
        Some(address) => unsafe { core::ptr::read_volatile(address as *const u16) },
        None if off > 0xff => 0xffff,
        None => ports::read16(bus, slot, func, off as u8),
    }
}

/// Write a 16-bit configuration register. `off` must be even.
pub fn write16(bus: u8, slot: u8, func: u8, off: u16, value: u16) {
    match window_byte(bus, slot, func, off & !0x1) {
        // SAFETY: as `read16`.
        Some(address) => unsafe { core::ptr::write_volatile(address as *mut u16, value) },
        None if off > 0xff => {}
        None => ports::write16(bus, slot, func, off as u8, value),
    }
}

/// Serve `buf` bytes of configuration space starting at `offset`.
///
/// The access sizes are Linux's `pci_read_config`, step for step: a byte if
/// the offset is odd, a word to reach a dword boundary, whole dwords through
/// the middle, then a trailing word and byte. The result is the same bytes a
/// simpler dword walk would produce, but the *reads* are the sizes the kernel
/// makes, and a caller that has to match a kernel's behaviour byte for byte
/// should match its accesses too.
///
/// Returns how many bytes landed in `buf`, which is short at the end of the
/// configuration space and zero past it - a caller reads the end of the file
/// the same way it reads the end of any other.
///
/// `size` is the function's own published length, [`Device::cfg_size`]: 256 for
/// a conventional function, 4096 for one whose extended space the device
/// answered for. Linux's `pci_read_config` clamps to `dev->cfg_size` the same
/// way.
pub fn config_read(bus: u8, dev: u8, func: u8, size: u64, offset: u64, buf: &mut [u8]) -> usize {
    if offset >= size || buf.is_empty() {
        return 0;
    }
    let count = buf.len().min((size - offset) as usize);
    let mut at = offset as u32;
    let mut done = 0usize;

    if at & 1 == 1 && done < count {
        buf[done] = read8(bus, dev, func, at as u16);
        at += 1;
        done += 1;
    }
    if at & 3 != 0 && count - done > 2 {
        let value = read16(bus, dev, func, at as u16);
        buf[done] = value as u8;
        buf[done + 1] = (value >> 8) as u8;
        at += 2;
        done += 2;
    }
    while count - done > 3 {
        let value = read32(bus, dev, func, at as u16);
        buf[done..done + 4].copy_from_slice(&value.to_le_bytes());
        at += 4;
        done += 4;
    }
    if count - done >= 2 {
        let value = read16(bus, dev, func, at as u16);
        buf[done] = value as u8;
        buf[done + 1] = (value >> 8) as u8;
        at += 2;
        done += 2;
    }
    if done < count {
        buf[done] = read8(bus, dev, func, at as u16);
        done += 1;
    }
    done
}

/// Store `buf` into configuration space starting at `offset`, in Linux's
/// access sizes - see [`config_read`] for why the sizes are part of the
/// contract rather than an implementation detail.
///
/// A write is where the sizes *matter*: a read-modify-write would disturb the
/// registers beside the one named, and some of them - a status word, an
/// interrupt line - are not the caller's to change. Returns how many bytes
/// were stored.
pub fn config_write(bus: u8, dev: u8, func: u8, size: u64, offset: u64, buf: &[u8]) -> usize {
    if offset >= size || buf.is_empty() {
        return 0;
    }
    let count = buf.len().min((size - offset) as usize);
    let mut at = offset as u32;
    let mut done = 0usize;

    if at & 1 == 1 && done < count {
        write8(bus, dev, func, at as u16, buf[done]);
        at += 1;
        done += 1;
    }
    if at & 3 != 0 && count - done > 2 {
        let value = u16::from_le_bytes([buf[done], buf[done + 1]]);
        write16(bus, dev, func, at as u16, value);
        at += 2;
        done += 2;
    }
    while count - done > 3 {
        let value = u32::from_le_bytes([buf[done], buf[done + 1], buf[done + 2], buf[done + 3]]);
        write32(bus, dev, func, at as u16, value);
        at += 4;
        done += 4;
    }
    if count - done >= 2 {
        let value = u16::from_le_bytes([buf[done], buf[done + 1]]);
        write16(bus, dev, func, at as u16, value);
        at += 2;
        done += 2;
    }
    if done < count {
        write8(bus, dev, func, at as u16, buf[done]);
        done += 1;
    }
    done
}

/// One PCI function.
#[derive(Clone, Copy)]
pub struct Func {
    bus: u8,
    slot: u8,
    func: u8,
}

impl Func {
    /// The function at one bus/device/function address.
    ///
    /// For a driver that found its device by *class* rather than by id, which is
    /// what a storage controller has and what `crate::pci::devices()` enumerates:
    /// the three numbers are what such a driver has, and the command register is
    /// what it needs next. `find` cannot answer that question - there is no
    /// vendor/device pair worth naming for "any SATA controller".
    pub fn at(bus: u8, slot: u8, func: u8) -> Func {
        Func { bus, slot, func }
    }

    fn vendor(&self) -> u16 {
        read16(self.bus, self.slot, self.func, 0x00)
    }

    fn device(&self) -> u16 {
        read16(self.bus, self.slot, self.func, 0x02)
    }

    /// The bus, device and function this entry names.
    ///
    /// For a caller that has to hand the address to someone else: the
    /// `virtio-drivers` transport walks the configuration space itself, and
    /// asks for the three numbers rather than for anything this module
    /// decided about the device.
    pub fn bdf(&self) -> (u8, u8, u8) {
        (self.bus, self.slot, self.func)
    }

    /// Base I/O port of BAR0, which must be a port-mapped BAR. `None` means
    /// the BAR is memory-mapped and this driver cannot drive the device.
    pub fn io_bar0(&self) -> Option<u16> {
        let bar = read32(self.bus, self.slot, self.func, 0x10);
        if bar & 1 == 1 {
            Some((bar & 0xfffc) as u16)
        } else {
            None
        }
    }

    /// Set bits in the command register without disturbing the rest.
    pub fn command_on(&self, bits: u16) {
        let current = read16(self.bus, self.slot, self.func, 0x04);
        write16(self.bus, self.slot, self.func, 0x04, current | bits);
    }
}

/// Find the first bus-0 function with this vendor and device ID.
///
/// `0xffff` reads as the vendor of an empty slot, which is how absent devices
/// are signalled.
pub fn find(vendor: u16, device: u16) -> Option<Func> {
    find_any(vendor, &[device]).into_iter().next()
}

/// Every bus-0 function with this vendor and any of these device IDs, in
/// slot order.
///
/// One device ID is not always one device: QEMU names every virtio input
/// device - its keyboard, its tablet - with the same PCI id, and a driver
/// that can only find "the first" finds only the keyboard. The multifunction
/// header bit is not consulted: a read of a function that is not there
/// answers the empty-slot vendor, which is a check in itself.
pub fn find_any(vendor: u16, devices: &[u16]) -> Vec<Func> {
    let mut found = Vec::new();
    for slot in 0..32 {
        for function in 0..8 {
            let func = Func {
                bus: 0,
                slot,
                func: function,
            };
            if func.vendor() == vendor && devices.contains(&func.device()) {
                found.push(func);
            }
        }
    }
    found
}

/// The snapshot of the bus: every function the boot can reach, in bus order.
///
/// Taken once, before any driver has probed. That ordering is what makes the
/// BAR sizes trustworthy: sizing a BAR means writing all ones into it and
/// reading back what it claims to decode, and doing that on a bus a driver is
/// already using would have the device answering for an address nobody
/// assigned it for as long as the probe takes. With the machine otherwise
/// idle, the only registers disturbed are ones nothing has touched yet, and
/// every one of them is written back.
pub fn enumerate() -> Vec<Device> {
    let mut found = Vec::new();
    let mut seen = Vec::new();
    scan_bus(0, &mut found, &mut seen);
    found
}

/// Scan one bus: its devices, then the buses behind its bridges.
fn scan_bus(bus: u8, found: &mut Vec<Device>, seen: &mut Vec<u8>) {
    // A bridge whose secondary bus number points back at a bus already walked
    // is a loop, and following it would walk forever.
    if seen.contains(&bus) {
        return;
    }
    seen.push(bus);

    let mut children = Vec::new();
    for slot in 0..32u8 {
        let Some(first) = read_function(bus, slot, 0) else {
            // Function 0 is required: a slot with no function 0 has nothing
            // in it, and nothing to look further in.
            continue;
        };
        // All seven functions once function 0 says the device is
        // multifunction - not stopping at the first gap, because a device may
        // implement 0 and 2 and leave 1 absent.
        let multifunction = read16(bus, slot, 0, 0x0e) & 0x80 != 0;
        let mut here = Vec::new();
        here.push(first);
        if multifunction {
            for function in 1..8u8 {
                if let Some(device) = read_function(bus, slot, function) {
                    here.push(device);
                }
            }
        }
        for device in here {
            if let Some(secondary) = secondary_bus(&device, bus) {
                children.push(secondary);
            }
            found.push(device);
        }
    }
    // The bridges' buses after the whole bus they are on: a bus's own devices
    // come before the ones behind it, which is the order Linux's own scan
    // produces and the order `/proc/bus/pci/devices` lists them in.
    for child in children {
        scan_bus(child, found, seen);
    }
}

/// The bus behind a bridge, if this function is one.
fn secondary_bus(device: &Device, parent: u8) -> Option<u8> {
    if device.header_type != 0x01 {
        return None;
    }
    let secondary = read8(device.bus, device.dev, device.func, 0x19);
    // A bridge that was never given a bus number, or that points at the bus
    // it is already on, leads nowhere new.
    if secondary == 0 || secondary == parent {
        return None;
    }
    Some(secondary)
}

/// Linux's `pci_cfg_space_size`: how many bytes of configuration space this
/// function publishes.
///
/// Only a function that may have extended space is asked: a PCIe function, a
/// host bridge (whose root complex can publish it without a PCIe capability)
/// and a PCI-X mode 2 bridge. The question is the device's, and on a machine
/// with no memory-mapped window the read at 0x100 answers all ones, so every
/// function stays 256 - Linux's answer over the port pair alone, too.
fn cfg_size(bus: u8, dev: u8, func: u8, class: u32) -> u64 {
    let status = read16(bus, dev, func, 0x06);
    let header_type = (read16(bus, dev, func, 0x0e) & 0x7f) as u8;
    let find = |id| {
        crate::pci::find_capability(status, header_type, id, |off| {
            read8(bus, dev, func, off as u16)
        })
    };
    let pcie = find(PCI_CAP_ID_PCIE).is_some();
    let pcix_mode_2 = find(PCI_CAP_ID_PCIX)
        .map(|pos| {
            read32(bus, dev, func, pos as u16 + PCI_X_STATUS as u16)
                & (PCI_X_STATUS_266MHZ | PCI_X_STATUS_533MHZ)
                != 0
        })
        .unwrap_or(false);
    crate::pci::cfg_space_size(class, pcie, pcix_mode_2, |off| read32(bus, dev, func, off))
}

/// Read one function's identity, class, interrupt line and BARs.
fn read_function(bus: u8, dev: u8, func: u8) -> Option<Device> {
    let identity = read32(bus, dev, func, 0x00);
    let vendor = identity as u16;
    let device = (identity >> 16) as u16;
    // What an empty slot answers. `0xffff_ffff` is a read that never landed;
    // the other shapes are what broken boards return in its place, and Linux
    // refuses all four.
    if vendor == 0xffff || vendor == 0x0000 {
        return None;
    }

    let class_revision = read32(bus, dev, func, 0x08);
    let header_type = (read16(bus, dev, func, 0x0e) & 0x7f) as u8;
    // A bridge's secondary bus number, which is the edge the tree in `/sys`
    // hangs the devices behind it from.
    let bridge = if header_type == 0x01 {
        Some(read8(bus, dev, func, 0x19))
    } else {
        None
    };

    // A CardBus bridge keeps its subsystem ids at 0x40 rather than 0x2c,
    // because the bytes a normal header uses for them are its bus numbers.
    let (subsys_vendor, subsys_id) = if header_type == 0x02 {
        (read16(bus, dev, func, 0x40), read16(bus, dev, func, 0x42))
    } else {
        let ids = read32(bus, dev, func, 0x2c);
        (ids as u16, (ids >> 16) as u16)
    };

    // Linux's `pci_read_irq`: the line register means nothing unless the
    // device implements an interrupt pin, and one that does not leaves
    // whatever the firmware wrote there.
    let irq = if read8(bus, dev, func, 0x3d) != 0 {
        read8(bus, dev, func, 0x3c)
    } else {
        0
    };

    Some(Device {
        domain: 0,
        bus,
        dev,
        func,
        vendor,
        device,
        revision: class_revision as u8,
        prog_if: (class_revision >> 8) as u8,
        subclass: (class_revision >> 16) as u8,
        class: (class_revision >> 24) as u8,
        header_type,
        bridge,
        subsys_vendor,
        subsys_id,
        irq,
        bars: read_bars(bus, dev, func, header_type),
        cfg_size: cfg_size(bus, dev, func, class_revision >> 8),
        driver: None,
        drm_card: false,
    })
}

/// The configuration register holding BAR `index`.
fn bar_register(index: usize) -> u16 {
    0x10 + (index as u16) * 4
}

/// The BARs a device implements, and how much address space each decodes.
///
/// Which registers *are* BARs depends on the header: a bridge's words at 0x18
/// and beyond are its bus numbers and its windows, and reading them as BARs
/// would invent resources out of a bridge's own configuration - worse, sizing
/// one would write all ones into the bus number of the bus behind it.
fn read_bars(bus: u8, dev: u8, func: u8, header_type: u8) -> [Bar; 7] {
    let mut bars = [Bar::EMPTY; 7];
    let (count, rom) = match header_type {
        0x00 => (6usize, 0x30u16),
        0x01 => (2, 0x38),
        0x02 => (1, 0),
        // An unknown header type is a device this kernel cannot describe -
        // Linux ignores it too, rather than guessing where its BARs are.
        _ => return bars,
    };

    // The values first, so the 64-bit decision is made before anything is
    // written. The register beside a 64-bit BAR is its upper half, but the
    // register beside a bridge's BAR1 is the bus it leads to, and only the
    // BAR's own type bits say which of the two this is.
    let mut values = [0u32; 7];
    for (index, value) in values.iter_mut().enumerate().take(count) {
        *value = read32(bus, dev, func, bar_register(index));
    }
    // One register past the last BAR is sized as well, when the last BAR is
    // the wide one: its upper half lives there. For a normal header that
    // register is the CardBus CIS pointer, which is read-only and takes the
    // write harmlessly; for the common case - no 64-bit BAR in the last slot
    // - it is not touched at all.
    let sized = if count == 6 && bar_flags(values[5]).1 {
        7
    } else {
        count
    };
    if sized > count {
        values[count] = read32(bus, dev, func, bar_register(count));
    }

    // Sizing means asking the device to decode everything, so decoding is off
    // while it answers - otherwise the device claims an address nobody
    // assigned it for as long as the probe takes.
    let command = read16(bus, dev, func, 0x04);
    let decode = command & (COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE);
    if decode != 0 {
        write16(bus, dev, func, 0x04, command & !decode);
    }

    let mut masks = [0u32; 7];
    for index in 0..sized {
        let register = bar_register(index);
        write32(bus, dev, func, register, PCI_ERROR_RESPONSE);
        masks[index] = read32(bus, dev, func, register);
        write32(bus, dev, func, register, values[index]);
    }
    let (rom_value, rom_mask) = if rom != 0 {
        let value = read32(bus, dev, func, rom);
        // A ROM's mask register is not the whole dword: bit 0 is its enable
        // and the bits below its address are reserved, so writing all ones
        // would set an enable nobody asked for and size the ROM against bits
        // that are not part of its address.
        write32(bus, dev, func, rom, PCI_ROM_ADDRESS_MASK);
        let mask = read32(bus, dev, func, rom);
        write32(bus, dev, func, rom, value);
        (value, mask)
    } else {
        (0, 0)
    };

    if decode != 0 {
        write16(bus, dev, func, 0x04, command);
    }

    let mut index = 0;
    while index < count {
        let low = values[index];
        let (_, wide) = bar_flags(low);
        if wide && index + 1 >= sized {
            // The upper half was never sized, so this BAR's size cannot be
            // read: reporting the low half alone would name a smaller region
            // than the device decodes. Nothing is a better answer than
            // something wrong.
            index += 2;
            continue;
        }
        let high = if wide { values[index + 1] } else { 0 };
        let high_mask = if wide { masks[index + 1] } else { 0 };
        if let Some(bar) = bar_range(low, high, masks[index], high_mask) {
            bars[index] = bar;
        }
        // A 64-bit BAR *is* two registers, and the second is not a BAR of its
        // own - describing it as one would double every wide BAR and read a
        // bridge's window registers as if they decoded address space.
        index += if wide { 2 } else { 1 };
    }
    if rom != 0 {
        bars[ROM_INDEX] = rom_bar(rom_value, rom_mask);
    }
    bars
}

/// Record that `driver` bound `func`, so `lspci -k` can name it.
pub fn claim(func: &Func, driver: &'static str) {
    let (bus, dev, function) = func.bdf();
    crate::pci::claim(bus, dev, function, driver);
}

/// Declare `func` to be the home of the DRM/KMS card, so `vfs::sysfs` hangs the
/// card's nodes off it (`crate::pci::mark_drm_card`, `NOTES.md` §7.4).
pub fn mark_drm_card(func: &Func) {
    let (bus, dev, function) = func.bdf();
    crate::pci::mark_drm_card(bus, dev, function);
}

/// The largest root table, and the largest single table, this walk will read.
///
/// A length is a number the firmware wrote, so it is checked before anything is
/// walked with it: these are the caps that keep a lie from becoming a read past
/// the end of the direct map, and both are far above what a real firmware
/// publishes - an XSDT is kilobytes and an MCFG is under a hundred bytes.
const MAX_ROOT_TABLE: usize = 64 * 1024;
const MAX_TABLE: usize = 64 * 1024;

/// Read one ACPI table through the direct map: its declared length first, then
/// the whole of it, and only then validate it.
///
/// The order is forced by what is knowable when. A table's length lives in its
/// header, and the header's own validation - the checksum over the table, the
/// length against the buffer that arrived - needs the table. So the first
/// [`crate::acpi::SDT_HEADER_LEN`] bytes are read to learn how much there is, the
/// bytes are read, and [`crate::acpi::parse_header`] then decides whether any of
/// it may be used. `cap` bounds how much a single table can claim, because the
/// number came from the firmware.
fn read_table(info: &BootInfo, phys: u64, cap: usize) -> Option<&'static [u8]> {
    // SAFETY: as `discover_ecam` states - the tables belong to the firmware and
    // the frame allocator never hands their memory out, so they stay readable.
    let head = unsafe {
        core::slice::from_raw_parts(
            info.phys_to_virt(phys as usize) as *const u8,
            crate::acpi::SDT_HEADER_LEN,
        )
    };
    let length = (crate::acpi::declared_length(head)? as usize).min(cap);
    if length < crate::acpi::SDT_HEADER_LEN {
        return None;
    }
    // SAFETY: as above, and `length` is bounded by `cap` rather than trusted.
    let table = unsafe {
        core::slice::from_raw_parts(info.phys_to_virt(phys as usize) as *const u8, length)
    };
    crate::acpi::parse_header(table)?;
    Some(table)
}

/// Ask the firmware's ACPI tables for a memory-mapped configuration window.
///
/// The root pointer's *address* came from the loader (`BootInfo::rsdp_phys`),
/// because the firmware's configuration table is the one place it is written
/// down and the kernel has no way to ask for it. Everything below the pointer is
/// read here, through the direct map: the memory map reports the tables as
/// ACPI-reclaimable, which the frame allocator never hands out, so they are still
/// where the firmware left them.
///
/// `None` is an honest absence - no root pointer, no MCFG among the tables, a
/// checksum that does not hold, or a length that runs past what this walk reads.
/// The caller falls back to the port mechanism, which is what a machine with no
/// window uses - and, on a machine with no ports either, to a bus with nothing
/// on it (see [`discover_ecam_dt`] for the window a device tree names instead).
///
/// What comes back is the range itself ([`EcamRange`]) rather than a kernel-side
/// copy of it: every question this module asks of a window - does it cover this
/// bus, where is that bus's slice, how wide is it - is a question `crates/acpi`
/// already answers, host-tested and fuzzed. What is *not* decided here is whether
/// the window is ever read; that is [`init`]'s check.
fn discover_ecam(info: &BootInfo) -> Option<EcamRange> {
    if let Some(range) = discover_ecam_acpi(info) {
        return Some(range);
    }
    // A machine with no ACPI tables is not a machine with no PCI: the same
    // firmware that declined to publish MCFG may still have described the host
    // bridge in its device tree, which is the whole of what this module needs -
    // a base, and the buses behind it.
    discover_ecam_dt(info)
}

/// The window the firmware's ACPI tables name, if it named one.
fn discover_ecam_acpi(info: &BootInfo) -> Option<EcamRange> {
    if info.rsdp_phys == 0 {
        crate::println!("pci: the boot record carries no ACPI root pointer");
        return None;
    }
    // SAFETY: the loader named this address as the firmware's root pointer, and
    // the memory it and everything it names lives in belongs to the firmware
    // (ACPI-reclaimable, never handed out by the frame allocator), so it stays
    // readable for as long as the machine runs. Every length is checked against
    // the caps above before it is used.
    let bytes = |phys: u64, len: usize| unsafe {
        core::slice::from_raw_parts(info.phys_to_virt(phys as usize) as *const u8, len)
    };

    let Some(root) =
        crate::acpi::parse_rsdp(bytes(info.rsdp_phys as u64, crate::acpi::RSDP_V2_LEN))
    else {
        crate::println!(
            "pci: the ACPI root pointer at {:#x} did not validate",
            info.rsdp_phys
        );
        return None;
    };
    let (root_phys, wide) = if root.has_xsdt() {
        (root.xsdt, true)
    } else {
        (root.rsdt as u64, false)
    };
    if root_phys == 0 {
        return None;
    }
    let Some(root_table) = read_table(info, root_phys, MAX_ROOT_TABLE) else {
        crate::println!("pci: the ACPI root table at {root_phys:#x} did not validate");
        return None;
    };
    // What the firmware *did* publish, for the same reason the loader lists the
    // configuration table it walked: a machine whose description is under
    // another name is one line away from this log and an afternoon away from the
    // words "no window" on their own.
    let mut published: Vec<[u8; 4]> = Vec::new();
    for address in crate::acpi::table_entries(&root_table[crate::acpi::SDT_HEADER_LEN..], wide) {
        if address == 0 {
            continue;
        }
        let Some(table) = read_table(info, address, MAX_TABLE) else {
            continue;
        };
        let signature = [table[0], table[1], table[2], table[3]];
        published.push(signature);
        if signature != *crate::acpi::MCFG_SIGNATURE {
            continue;
        }
        let ranges = crate::acpi::parse_mcfg(table);
        if ranges.is_empty() {
            crate::println!("pci: MCFG at {address:#x} names no window");
        }
        for range in ranges {
            // Segment zero is the one the enumeration is written for: the
            // snapshot's `Device` carries a domain, but every driver that reads
            // the bus names bus 0 of segment 0.
            if range.segment == 0 && !range.is_empty() {
                return Some(range);
            }
        }
    }
    if published.is_empty() {
        crate::println!("pci: the ACPI root table names no tables");
    } else {
        crate::print!("pci: tables:");
        for signature in &published {
            crate::print!(
                " {}{}{}{}",
                signature[0] as char,
                signature[1] as char,
                signature[2] as char,
                signature[3] as char
            );
        }
        crate::println!();
    }
    None
}

/// Whether two walks describe the same machine, with the one field that is the
/// *mechanism's* answer left out.
///
/// `cfg_size` is the device's length as reachable through whoever asked: the
/// port pair cannot address 0x100, so its walk reads every function as 256
/// where the window may read 4096. That is not a disagreement about the
/// machine, and comparing it would make every PCIe machine refuse its own
/// window.
fn same_hardware(a: &Device, b: &Device) -> bool {
    let mut a = *a;
    let mut b = *b;
    a.cfg_size = 0;
    b.cfg_size = 0;
    a == b
}

/// The same comparison over a whole snapshot.
fn same_machine(a: &[Device], b: &[Device]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_hardware(a, b))
}

/// Where two snapshots of the same bus first disagree, as a phrase for [`init`]'s
/// log line.
///
/// Called only when they *do* differ, so the answer is never empty: either one
/// function's identity reads differently, or the two walks did not even find the
/// same number of them.
fn first_difference(over_ports: &[Device], over_mmio: &[Device]) -> alloc::string::String {
    for (ports, mmio) in over_ports.iter().zip(over_mmio) {
        if !same_hardware(ports, mmio) {
            return alloc::format!(
                " at bus {} dev {} func {}: the ports read {:04x}:{:04x}, the window {:04x}:{:04x}",
                ports.bus,
                ports.dev,
                ports.func,
                ports.vendor,
                ports.device,
                mmio.vendor,
                mmio.device
            );
        }
    }
    alloc::format!(
        " about how many functions there are: {} over the ports, {} over the window",
        over_ports.len(),
        over_mmio.len()
    )
}

/// Hand a snapshot to [`crate::pci`] and answer how many functions were in it, for
/// the boot's own log line.
fn hand_over(devices: Vec<Device>) -> usize {
    let count = devices.len();
    crate::pci::publish(devices);
    count
}

/// The window a *device tree* names, for a machine whose firmware published no
/// ACPI tables at all.
///
/// QEMU's `virt` is that machine, and it is booted with `acpi=off` on purpose -
/// this port reads the tree, and AAVMF publishes either a tree or a set of ACPI
/// tables and not both (`NOTES.md` §9.1). So MCFG is absent, and for a long time
/// that read as "this machine has no PCI": the module was x86_64's, one of the
/// two drivers behind it was reachable only through it, and `t-ahci` stayed
/// un-migratable because the second machine had no `/dev/sda` (`NOTES.md` §15).
/// The tree does describe the bus, in the node the ECAM binding reserves for it:
///
/// ```text
/// pcie@10000000 {
///     compatible = "pci-host-ecam-generic";
///     device_type = "pci";
///     reg = <0x40 0x10000000 0x00 0x10000000>;
///     bus-range = <0x00 0xff>;
///     dma-coherent;
/// };
/// ```
///
/// measured by `qemu-system-aarch64 -machine virt,gic-version=2,acpi=off -machine
/// dumpdtb=` on QEMU 11.1.2 and read back with `dtc`. `reg` is two address cells
/// and two size cells - the *parent's*, which is what `fdt`'s `Reg` is written to
/// use - so the four words are one 64-bit base of `0x40_1000_0000` and a length of
/// `0x1000_0000`: QEMU's high ECAM, 256 MiB, one bus per mebibyte. (This is why
/// the address cannot be written down: `virt` puts the low ECAM at `0x3f00_0000`
/// on a machine with no high memory and the high one here when there is, and the
/// tree is the only place the machine says which.)
///
/// `dma-coherent` is in that node and it is worth saying why it matters: it is
/// the firmware's promise that a device's writes land in memory the CPU can
/// read without an explicit invalidate, which is what lets this kernel's AHCI
/// bounce pages work unmodified on this machine. A machine whose tree omits it
/// would need a cache flush around every transfer, and nothing here claims to
/// have one.
fn discover_ecam_dt(info: &BootInfo) -> Option<EcamRange> {
    let Some(bytes) = info.dtb else {
        crate::println!("pci: the boot record carries no device tree either");
        return None;
    };
    let tree = match crate::fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(_) => {
            crate::println!("pci: the device tree will not walk");
            return None;
        }
    };
    let node = match tree.find_compatible("pci-host-ecam-generic") {
        Ok(Some(node)) => node,
        Ok(None) => {
            crate::println!("pci: the tree names no pci-host-ecam-generic bridge");
            return None;
        }
        Err(_) => {
            crate::println!("pci: the device tree will not walk");
            return None;
        }
    };
    let Some((base, size)) = node.reg().next() else {
        crate::println!("pci: the bridge at {} names no window", node.name());
        return None;
    };
    // A bus is a mebibyte of the window, so the window itself bounds the bus
    // numbers rather than the other way round: a tree that promised more buses
    // than its `reg` has room for would send the walk past the end of the
    // mapping into whatever the firmware put there.
    let buses = size / crate::acpi::ECAM_BUS_BYTES;
    if buses == 0 {
        crate::println!(
            "pci: the bridge at {} names a {}-byte window, which holds no bus",
            node.name(),
            size
        );
        return None;
    }
    let mut range = node.cells("bus-range");
    let start_bus = range.next().unwrap_or(0) as u8;
    // The last bus the window has room for, which is what a `bus-range` wider
    // than the window is clamped to.
    let last = (start_bus as u32 + buses as u32 - 1).min(0xff);
    let end_bus = range.next().unwrap_or(last).min(last) as u8;
    crate::println!(
        "pci: the tree names ECAM at {:#x}, buses {}..={}, {} MiB",
        base,
        start_bus,
        end_bus,
        size / (1024 * 1024)
    );
    Some(EcamRange {
        base,
        // One segment, which is what the tree's `linux,pci-domain` says and what
        // every device's `Device` carries a domain for.
        segment: 0,
        start_bus,
        end_bus,
    })
}

/// Whether this machine has a port pair to fall back on, which is the question
/// [`init`]'s order is built around.
///
/// Stated once rather than as `cfg!` at each of the four places that ask it, so
/// that "the ports are the known-good mechanism" and "there may be no ports" are
/// one fact with one spelling.
const fn has_ports() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Take the bus snapshot, once, and publish it for `/sys` and `/proc`.
///
/// Returns how many functions were found, for the boot's own log line. The
/// mechanism is chosen here, and the order is the point: on a machine that has a
/// port pair, the ports walk the bus first, because they are the mechanism that
/// asks nothing of the firmware and are therefore the one known to work; if the
/// firmware named a window, the walk is done again through it and the two
/// snapshots are compared function by function. A window that agrees is kept -
/// from here on, discovery and the `config` files under `/sys` and `/proc` read
/// memory instead of issuing port cycles - and a window that disagrees about
/// anything is refused, with the port pair left in place.
///
/// **A machine with no port pair cannot be checked that way**, and it is not
/// checked that way: there is no second mechanism to compare against, so the
/// window is used as it was found and the absence is said in the log rather than
/// dressed up as a comparison. That is not a weaker claim than the first
/// machine's - it is a different one. The check on x86_64 is "these two agree",
/// which needs two; what remains on aarch64 is "this window is the only door and
/// here is what the walk read through it", and a walk that read *nothing* is
/// printed as such rather than published as an empty machine.
///
/// The check is what makes "MMIO by preference" safe to say. Everything above this
/// module - every driver's `find`, `docs/NOTES.md` §7.4's identity chain - rests
/// on the snapshot being right, and a second mechanism no machine had ever
/// compared against the first is exactly the kind of thing that is right until it
/// is not.
pub fn init(info: &BootInfo) -> usize {
    let over_ports = if has_ports() { Some(enumerate()) } else { None };
    let Some(ecam) = discover_ecam(info) else {
        if has_ports() {
            crate::println!("pci: no ECAM window; configuration space is over ports 0xcf8/0xcfc");
            // `Some` on every machine that has ports, which is what makes the
            // unwrap below the *same* statement as the arm that follows.
            return hand_over(over_ports.unwrap_or_default());
        }
        crate::println!(
            "pci: no ECAM window and no port pair - this machine's configuration space is \
             unreachable"
        );
        return hand_over(Vec::new());
    };
    crate::println!(
        "pci: ECAM at {:#x}, segment {}, buses {}..={}, {} MiB",
        ecam.base,
        ecam.segment,
        ecam.start_bus,
        ecam.end_bus,
        ecam.len() / (1024 * 1024)
    );
    if ecam.offset(0, 0, 0).is_none() {
        // `enumerate` walks from bus 0 - the host bridge, and the root of every bus
        // behind it. A window that starts above bus 0 cannot read the bus the walk
        // exists to describe, and it would make the check below *vacuous*: every
        // access on bus 0 would fall back to the ports, the two snapshots would
        // agree by construction, and the boot would claim a window it never read a
        // byte through. The ports stay, which is what a machine with no window has.
        crate::println!(
            "pci: the window starts at bus {} and the walk starts at bus 0; ports kept",
            ecam.start_bus
        );
        return hand_over(over_ports.unwrap_or_default());
    }

    use_window(ecam);
    let over_mmio = enumerate();
    if let Some(over_ports) = over_ports {
        if !same_machine(&over_mmio, &over_ports) {
            crate::println!(
                "pci: ECAM disagrees with the port pair{}; ports kept",
                first_difference(&over_ports, &over_mmio)
            );
            use_ports();
            return hand_over(over_ports);
        }
        if over_mmio.is_empty() {
            // Both mechanisms found nothing, which is a machine with no PCI bus
            // rather than a window that failed: say which, because the two read
            // the same in every other line of this log.
            crate::println!("pci: the port pair and the window agree there are no functions");
        }
        crate::println!(
            "pci: ECAM and the port pair agree on all {} functions; configuration space is over \
             MMIO",
            over_mmio.len()
        );
        return hand_over(over_mmio);
    }
    if over_mmio.is_empty() {
        crate::println!("pci: the window named no function at all; the bus is empty or unreadable");
    } else {
        crate::println!(
            "pci: ECAM is this machine's only mechanism and the walk read {} functions through it",
            over_mmio.len()
        );
    }
    hand_over(over_mmio)
}
