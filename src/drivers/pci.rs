//! PCI configuration space, accessed with mechanism 1 (ports 0xcf8/0xcfc).
//!
//! Two jobs live here, and they are the two halves of what a PCI bus is to a
//! kernel. The first is *discovery*: find the function with a given vendor and
//! device id, which is all a driver needs and all this module used to do. The
//! second is the *snapshot*: walk every bus, learn what each function is and
//! what address space it decodes, and hand that to [`crate::pci`] so `/sys` and
//! `/proc` can describe the machine the way `lspci` reads it.
//!
//! Only the port I/O is here. Every decision made *about* what was read - which
//! bits are a BAR's type, how much space a mask means, how Linux spells the
//! result - is in [`crate::pci`], where a host build can test it without a
//! machine to read.
//!
//! Mechanism 1 reaches 256 bytes of configuration space per function: its
//! address register has eight bits of offset. That is why a device here has
//! [`crate::pci::CFG_SIZE`] bytes and no PCIe extended capabilities.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

use crate::pci::{
    Bar, CFG_SIZE, Device, PCI_ERROR_RESPONSE, PCI_ROM_ADDRESS_MASK, ROM_INDEX, bar_flags,
    bar_range, rom_bar,
};

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

/// Command register bits.
pub const COMMAND_IO_SPACE: u16 = 1 << 0;
pub const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
/// PCI 2.3 "Interrupt Disable": the device must not assert its INTx line. A
/// polling driver wants this, or every completed request raises an unhandled
/// interrupt.
pub const COMMAND_INTX_DISABLE: u16 = 1 << 10;

fn address(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    0x8000_0000
        | (bus as u32) << 16
        | (slot as u32) << 11
        | (func as u32) << 8
        | (off as u32 & 0xfc)
}

pub fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA).read()
    }
}

pub fn write32(bus: u8, slot: u8, func: u8, off: u8, value: u32) {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA).write(value);
    }
}

/// Read one byte of configuration space.
///
/// A *byte* access, not a dword with three bytes ignored. Mechanism 1's data
/// port is four bytes wide and a narrower access reaches only the bytes it
/// names - which is what makes a byte read see what a byte write wrote.
pub fn read8(bus: u8, slot: u8, func: u8, off: u8) -> u8 {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA + (off as u16 & 3)).read()
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
pub fn write8(bus: u8, slot: u8, func: u8, off: u8, value: u8) {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA + (off as u16 & 3)).write(value)
    }
}

/// Read a 16-bit configuration register. `off` must be even.
pub fn read16(bus: u8, slot: u8, func: u8, off: u8) -> u16 {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA + (off as u16 & 2)).read()
    }
}

/// Write a 16-bit configuration register. `off` must be even.
pub fn write16(bus: u8, slot: u8, func: u8, off: u8, value: u16) {
    unsafe {
        Port::new(CONFIG_ADDRESS).write(address(bus, slot, func, off));
        Port::new(CONFIG_DATA + (off as u16 & 2)).write(value)
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
pub fn config_read(bus: u8, dev: u8, func: u8, offset: u64, buf: &mut [u8]) -> usize {
    if offset >= CFG_SIZE || buf.is_empty() {
        return 0;
    }
    let count = buf.len().min((CFG_SIZE - offset) as usize);
    let mut at = offset as u32;
    let mut done = 0usize;

    if at & 1 == 1 && done < count {
        buf[done] = read8(bus, dev, func, at as u8);
        at += 1;
        done += 1;
    }
    if at & 3 != 0 && count - done > 2 {
        let value = read16(bus, dev, func, at as u8);
        buf[done] = value as u8;
        buf[done + 1] = (value >> 8) as u8;
        at += 2;
        done += 2;
    }
    while count - done > 3 {
        let value = read32(bus, dev, func, at as u8);
        buf[done..done + 4].copy_from_slice(&value.to_le_bytes());
        at += 4;
        done += 4;
    }
    if count - done >= 2 {
        let value = read16(bus, dev, func, at as u8);
        buf[done] = value as u8;
        buf[done + 1] = (value >> 8) as u8;
        at += 2;
        done += 2;
    }
    if done < count {
        buf[done] = read8(bus, dev, func, at as u8);
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
pub fn config_write(bus: u8, dev: u8, func: u8, offset: u64, buf: &[u8]) -> usize {
    if offset >= CFG_SIZE || buf.is_empty() {
        return 0;
    }
    let count = buf.len().min((CFG_SIZE - offset) as usize);
    let mut at = offset as u32;
    let mut done = 0usize;

    if at & 1 == 1 && done < count {
        write8(bus, dev, func, at as u8, buf[done]);
        at += 1;
        done += 1;
    }
    if at & 3 != 0 && count - done > 2 {
        let value = u16::from_le_bytes([buf[done], buf[done + 1]]);
        write16(bus, dev, func, at as u8, value);
        at += 2;
        done += 2;
    }
    while count - done > 3 {
        let value = u32::from_le_bytes([buf[done], buf[done + 1], buf[done + 2], buf[done + 3]]);
        write32(bus, dev, func, at as u8, value);
        at += 4;
        done += 4;
    }
    if count - done >= 2 {
        let value = u16::from_le_bytes([buf[done], buf[done + 1]]);
        write16(bus, dev, func, at as u8, value);
        at += 2;
        done += 2;
    }
    if done < count {
        write8(bus, dev, func, at as u8, buf[done]);
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
        driver: None,
        drm_card: false,
    })
}

/// The configuration register holding BAR `index`.
fn bar_register(index: usize) -> u8 {
    0x10 + (index as u8) * 4
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
        0x00 => (6usize, 0x30u8),
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
/// card's nodes off it (`crate::pci::mark_drm_card`, `PLAN.md` §7.4).
pub fn mark_drm_card(func: &Func) {
    let (bus, dev, function) = func.bdf();
    crate::pci::mark_drm_card(bus, dev, function);
}

/// Take the bus snapshot, once, and publish it for `/sys` and `/proc`.
///
/// Returns how many functions were found, for the boot's own log line.
pub fn init() -> usize {
    let devices = enumerate();
    let count = devices.len();
    crate::pci::publish(devices);
    count
}
