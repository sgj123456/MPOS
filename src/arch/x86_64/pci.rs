//! PCI configuration space, accessed with mechanism 1 (ports 0xcf8/0xcfc).
//!
//! Only what device discovery needs: bus 0, one function per slot. A q35 or
//! i440fx machine puts every device we care about on that bus, and the legacy
//! config mechanism reaches them all without walking the ECAM.

use x86_64::instructions::port::Port;

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

pub fn read16(bus: u8, slot: u8, func: u8, off: u8) -> u16 {
    (read32(bus, slot, func, off) >> ((off & 2) * 8)) as u16
}

pub fn write16(bus: u8, slot: u8, func: u8, off: u8, value: u16) {
    let word = read32(bus, slot, func, off);
    let shift = (off & 2) * 8;
    let masked = (word & !(0xffff << shift)) | ((value as u32) << shift);
    write32(bus, slot, func, off, masked);
}

/// One PCI function on bus 0.
#[derive(Clone, Copy)]
pub struct Func {
    bus: u8,
    slot: u8,
    func: u8,
}

impl Func {
    fn vendor(&self) -> u16 {
        read16(self.bus, self.slot, self.func, 0x00)
    }

    fn device(&self) -> u16 {
        read16(self.bus, self.slot, self.func, 0x02)
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
    for slot in 0..32 {
        let func = Func {
            bus: 0,
            slot,
            func: 0,
        };
        if func.vendor() == vendor && func.device() == device {
            return Some(func);
        }
    }
    None
}
