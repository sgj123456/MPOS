#![no_std]

//! The PCI devices this machine has, and the way Linux spells them.
//!
//! Two things live here, and they are here rather than in the driver because
//! both are *pure*: what a device is, and how a kernel is expected to describe
//! one. Finding the devices needs port I/O and lives in
//! [`crate::drivers::pci`]; describing them is string formatting, so it is
//! compiled on the host and argued with by tests.
//!
//! # Why the spelling matters
//!
//! `lspci` does not read a kernel's data structures - it reads files, and it
//! parses them with `sscanf` formats that were written against one kernel's
//! exact output. A byte in the wrong place is not a cosmetic difference: a
//! `0x` prefix missing from `vendor` makes pciutils read `1af4` as decimal 1
//! and every vendor name in the output becomes "unknown". So the formats below
//! are not this kernel's invention; each one is a citation of the kernel
//! function that prints it, and the tests pin the bytes.
//!
//! # What the two trees need
//!
//! pciutils probes access methods in the order *sysfs, proc, direct port I/O*
//! (`lib/init.c`), and the first one that answers wins:
//!
//!   `/sys/bus/pci/devices/<bdf>/{vendor,device,class,revision,resource,config,…}`
//!   `/proc/bus/pci/devices` and `/proc/bus/pci/<bus>/<dev>.<func>`
//!
//! The direct method needs `ioperm`, which x86_64 Linux does not implement -
//! so on a 64-bit kernel the first two are the only ones that can work, and
//! both are built from the devices in this module.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

/// Linux's `IORESOURCE_IO`: the resource decodes the port address space.
pub const IORESOURCE_IO: u32 = 0x0000_0100;
/// Linux's `IORESOURCE_MEM`: the resource decodes memory.
pub const IORESOURCE_MEM: u32 = 0x0000_0200;
/// Linux's `IORESOURCE_PREFETCH`: reads have no side effects.
pub const IORESOURCE_PREFETCH: u32 = 0x0000_2000;
/// Linux's `IORESOURCE_READONLY`: a ROM, which is read but not written.
pub const IORESOURCE_READONLY: u32 = 0x0000_4000;
/// Linux's `IORESOURCE_SIZEALIGN`: the resource is naturally aligned. Set on
/// every BAR by `__pci_read_base`, which is why a BAR's flags are never just
/// the type bits.
pub const IORESOURCE_SIZEALIGN: u32 = 0x0004_0000;
/// Linux's `IORESOURCE_MEM_64`: a 64-bit memory BAR.
pub const IORESOURCE_MEM_64: u32 = 0x0010_0000;
/// Linux's `IORESOURCE_ROM_ENABLE`: the ROM BAR is currently decoding.
pub const IORESOURCE_ROM_ENABLE: u32 = 0x0000_0001;

/// The mask `/proc/bus/pci/devices` keeps out of the flags when it prints an
/// address: Linux's `PCI_REGION_FLAG_MASK`. The BAR's own low type bits (which
/// `decode_bar` folds into the flags) survive into the printed address, and
/// pciutils masks them off again for display - but a caller that does not is
/// reading the same number Linux would have given it.
pub const PCI_REGION_FLAG_MASK: u32 = 0x0f;

/// How many bytes of configuration space a device has.
///
/// 256, always: this kernel reaches configuration space through mechanism 1
/// (ports 0xcf8/0xcfc), whose address register has eight bits of offset. The
/// 4096-byte extended space PCIe added is not reachable that way at all, so a
/// device here honestly has 256 bytes and the PCIe extended capabilities are
/// simply not visible. Reporting 4096 and reading zeros would be a lie that
/// makes pciutils print capability headers that are not there.
pub const CFG_SIZE: u64 = 256;

/// One base address register (or the expansion ROM), as Linux models it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bar {
    /// The address the BAR decodes, or 0 when nothing is assigned.
    pub base: u64,
    /// How much address space it decodes, or 0 when it decodes none.
    pub size: u64,
    /// Linux's `IORESOURCE_*` bits, including the BAR's own low type bits -
    /// see [`PCI_REGION_FLAG_MASK`].
    pub flags: u32,
}

impl Bar {
    /// A BAR that decodes nothing, which is what an unimplemented BAR and a
    /// bridge's non-BAR words both read as.
    pub const EMPTY: Bar = Bar {
        base: 0,
        size: 0,
        flags: 0,
    };

    /// The last address the BAR decodes, as the `resource` attribute prints it.
    ///
    /// A resource of size zero has no last address, and Linux prints 0 rather
    /// than one *before* its start - the underflow a `base + size - 1` would
    /// produce, which pciutils reads as an enormous size rather than as none.
    pub fn end(&self) -> u64 {
        if self.size == 0 {
            0
        } else {
            self.base + self.size - 1
        }
    }

    /// The address field `/proc/bus/pci/devices` prints: Linux's
    /// `pci_resource_to_user` start, with `PCI_REGION_FLAG_MASK` kept.
    pub fn proc_base(&self) -> u64 {
        self.base | (self.flags as u64 & PCI_REGION_FLAG_MASK as u64)
    }
}

/// `PCI_BASE_ADDRESS_IO_MASK`: the port address a BAR decodes, and the bits of
/// the read-back mask that mean "decodes this much port space".
pub const PCI_BASE_ADDRESS_IO_MASK: u32 = 0xffff_fffc;
/// `PCI_BASE_ADDRESS_MEM_MASK`: the same for a memory BAR.
pub const PCI_BASE_ADDRESS_MEM_MASK: u32 = 0xffff_fff0;
/// `PCI_BASE_ADDRESS_MEM_TYPE_MASK`: the two bits that say how wide a memory
/// BAR is.
pub const PCI_BASE_ADDRESS_MEM_TYPE_MASK: u32 = 0x0000_0006;
/// `PCI_BASE_ADDRESS_MEM_TYPE_64`: a memory BAR whose upper half is the next
/// register along.
pub const PCI_BASE_ADDRESS_MEM_TYPE_64: u32 = 0x0000_0004;
/// `PCI_BASE_ADDRESS_MEM_PREFETCH`: reads of this BAR have no side effects.
pub const PCI_BASE_ADDRESS_MEM_PREFETCH: u32 = 0x0000_0008;
/// `PCI_ROM_ADDRESS_MASK`: the address bits of the expansion ROM BAR. Bit 0 is
/// its enable, and the bits between are reserved.
pub const PCI_ROM_ADDRESS_MASK: u32 = 0xffff_f800;
/// `IO_SPACE_LIMIT` on x86: the port address space is 64 KiB, so an I/O BAR can
/// decode no more than that however many bits it claims.
pub const IO_SPACE_LIMIT: u32 = 0x0000_ffff;
/// `PCI_ERROR_RESPONSE`: what a configuration read answers when nothing is
/// there, and what Linux's `PCI_POSSIBLE_ERROR` tests for.
pub const PCI_ERROR_RESPONSE: u32 = 0xffff_ffff;

/// The flags a BAR's low dword carries, and whether its upper half is the next
/// register along - Linux's `decode_bar`, which folds the BAR's own low type
/// bits into the resource flags.
///
/// The folding is not cosmetic. `/proc/bus/pci/devices` prints the address with
/// those bits still in it, and a memory BAR's bit 2 is what says "64-bit": a
/// caller that drops them describes a different device than Linux does.
///
/// `IORESOURCE_SIZEALIGN` is *not* here: `__pci_read_base` adds it to every
/// BAR, but not to a bridge's window registers, and the two are described by
/// different functions in Linux. [`bar_range`] is the one that describes a BAR
/// and so is the one that adds it.
pub fn bar_flags(low: u32) -> (u32, bool) {
    if low & 1 == 1 {
        // An I/O BAR: the low two bits are the space indicator and the
        // reserved bit, and both survive into the flags.
        (low & !PCI_BASE_ADDRESS_IO_MASK | IORESOURCE_IO, false)
    } else {
        let mut flags = low & !PCI_BASE_ADDRESS_MEM_MASK | IORESOURCE_MEM;
        if flags & PCI_BASE_ADDRESS_MEM_PREFETCH != 0 {
            flags |= IORESOURCE_PREFETCH;
        }
        let wide = low & PCI_BASE_ADDRESS_MEM_TYPE_MASK == PCI_BASE_ADDRESS_MEM_TYPE_64;
        if wide {
            flags |= IORESOURCE_MEM_64;
        }
        (flags, wide)
    }
}

/// Linux's `pci_size`: how much address space a BAR decodes, from the value it
/// held and the value it read back after being asked to decode everything.
///
/// The device answers with its own address bits inverted, so the size is the
/// lowest bit the mask sets - and the two extra checks are Linux's, for the
/// devices that answer nonsense: a mask of zero means nothing to size, and a
/// base that equals its own mask with an incomplete one means the BAR is
/// already programmed with all ones, which no size can be read from.
pub fn pci_size(base: u64, maxbase: u64, mask: u64) -> u64 {
    let size = mask & maxbase;
    if size == 0 {
        return 0;
    }
    // The lowest bit the device answered with is the one it decodes down to.
    let size = 1u64 << size.trailing_zeros();
    if base == maxbase && ((base | (size - 1)) & mask) != mask {
        return 0;
    }
    size
}

/// Whether a BAR of `size` at `base` can be described at all.
///
/// A resource's last address is `base + size - 1`, so a range that runs off the
/// end of the address space has no last address to print - and `resource` and
/// `/proc/bus/pci/devices` would report one that wrapped, which pciutils reads
/// as a device decoding several exabytes. Linux never meets this: it sizes a
/// BAR and then *reallocates* it if the fit is bad, so the address it prints is
/// one it chose. Here the address is the one the device already holds, which is
/// exactly the value an unconfigured or misprogrammed function can leave at the
/// top of a 64-bit space.
fn range_fits(base: u64, size: u64) -> bool {
    base.checked_add(size).is_some()
}

/// The address a BAR decodes and how much of it, from the value it held and the
/// mask it answered - Linux's `__pci_read_base`, minus the port I/O.
///
/// `high` and `mask_high` are the next register's dwords, read only when the
/// low half says the BAR is 64-bit wide; for a 32-bit BAR they are ignored, so
/// a caller can pass zeros. `None` means the BAR decodes nothing at all - the
/// device does not implement it, or answers a mask no size can be read from.
pub fn bar_range(low: u32, high: u32, mask_low: u32, mask_high: u32) -> Option<Bar> {
    // `PCI_POSSIBLE_ERROR`: a read that answers all ones is a read that never
    // reached a device.
    let low = if low == PCI_ERROR_RESPONSE { 0 } else { low };
    let mask_low = if mask_low == PCI_ERROR_RESPONSE {
        0
    } else {
        mask_low
    };
    let (flags, wide) = bar_flags(low);
    let (base, mask, space) = if flags & IORESOURCE_IO != 0 {
        (
            (low & PCI_BASE_ADDRESS_IO_MASK) as u64,
            (mask_low & PCI_BASE_ADDRESS_IO_MASK) as u64,
            (PCI_BASE_ADDRESS_IO_MASK & IO_SPACE_LIMIT) as u64,
        )
    } else if wide {
        (
            (low & PCI_BASE_ADDRESS_MEM_MASK) as u64 | ((high as u64) << 32),
            (mask_low & PCI_BASE_ADDRESS_MEM_MASK) as u64 | ((mask_high as u64) << 32),
            PCI_BASE_ADDRESS_MEM_MASK as u64 | 0xffff_ffff_0000_0000,
        )
    } else {
        (
            (low & PCI_BASE_ADDRESS_MEM_MASK) as u64,
            (mask_low & PCI_BASE_ADDRESS_MEM_MASK) as u64,
            PCI_BASE_ADDRESS_MEM_MASK as u64,
        )
    };
    if mask == 0 {
        return None;
    }
    let size = pci_size(base, mask, space);
    if size == 0 || !range_fits(base, size) {
        return None;
    }
    Some(Bar {
        base,
        size,
        flags: flags | IORESOURCE_SIZEALIGN,
    })
}

/// The expansion ROM BAR, from its value and its mask - Linux's
/// `__pci_read_base` with `pci_bar_mem32` and the flags `pci_read_bases` gives
/// a ROM: memory, prefetchable, read-only, and never sized by anything but its
/// own address bits.
///
/// A ROM is not a BAR with a different name. Its mask register is not the whole
/// dword - bit 0 is the enable and the bits below the address are reserved - so
/// writing all ones would set an enable the caller never asked for and size the
/// ROM against bits that are not part of its address.
pub fn rom_bar(original: u32, mask: u32) -> Bar {
    let mut flags =
        IORESOURCE_MEM | IORESOURCE_PREFETCH | IORESOURCE_READONLY | IORESOURCE_SIZEALIGN;
    let original = if original == PCI_ERROR_RESPONSE {
        0
    } else {
        original
    };
    let mask = if mask == PCI_ERROR_RESPONSE { 0 } else { mask };
    if original & IORESOURCE_ROM_ENABLE != 0 {
        flags |= IORESOURCE_ROM_ENABLE;
    }
    let base = original & PCI_ROM_ADDRESS_MASK;
    let answered = mask & PCI_ROM_ADDRESS_MASK;
    if answered == 0 {
        return Bar::EMPTY;
    }
    let size = pci_size(base as u64, answered as u64, PCI_ROM_ADDRESS_MASK as u64);
    if size == 0 {
        return Bar::EMPTY;
    }
    Bar {
        base: base as u64,
        size,
        flags,
    }
}

/// One PCI function: what it is, where it is, and what it decodes.
///
/// The numbers are the ones configuration space holds, read once at boot and
/// not re-read - except through the `config` attribute, which goes back to the
/// device. That split is deliberate and matches Linux: identity and BARs are
/// fixed for a boot, while the command register and a capability's enable bit
/// are live state a program is entitled to see change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    /// The PCI domain. Always 0 here: this kernel drives one host bridge.
    pub domain: u16,
    pub bus: u8,
    /// Device (slot) number, 0..=31.
    pub dev: u8,
    /// Function number, 0..=7.
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    /// Byte 0x08: the revision.
    pub revision: u8,
    /// Byte 0x09: the programming interface.
    pub prog_if: u8,
    /// Byte 0x0a: the subclass.
    pub subclass: u8,
    /// Byte 0x0b: the base class.
    pub class: u8,
    /// Byte 0x0e, with the multifunction bit masked off - Linux's
    /// `dev->hdr_type`.
    pub header_type: u8,
    /// For a PCI-to-PCI bridge (header type 1), the bus it leads to: byte 0x19.
    /// `None` for everything else.
    ///
    /// A bridge is not just a device that happens to sit on a bus - it *is*
    /// the bus's edge, and the tree `/sys` publishes hangs the devices behind
    /// it underneath it. Nothing else about a device says which bus it is on,
    /// so the number has to be carried rather than derived.
    pub bridge: Option<u8>,
    pub subsys_vendor: u16,
    pub subsys_id: u16,
    /// The legacy interrupt line, and *only* when an interrupt pin is
    /// implemented - see [`Device::irq`]'s setter for why that matters.
    pub irq: u8,
    /// BAR0..BAR5 and the expansion ROM, in that order. A BAR the device does
    /// not implement, and a bridge's bridge registers, are [`Bar::EMPTY`].
    pub bars: [Bar; 7],
    /// The driver that bound this function, for `lspci -k`. `None` until
    /// something claims it - see [`claim`].
    pub driver: Option<&'static str>,
    /// Whether the driver that bound this function has declared it to be the
    /// home of a DRM/KMS card. `false` until [`mark_drm_card`] says so.
    ///
    /// This is the fact a display driver owns about *its own* device, and the
    /// reason it is a field rather than an inference: `sysfs` hangs the card's
    /// nodes off the function that carries the card, and the machine can put a
    /// second display-class function - the firmware's own VGA - beside it.
    /// Choosing by class code alone hung the card off the VGA (`NOTES.md` §7.4).
    /// The driver names its function instead; `sysfs` reads this and stops
    /// guessing.
    pub drm_card: bool,
}

/// Index of the expansion ROM in [`Device::bars`]: Linux's `PCI_ROM_RESOURCE`.
pub const ROM_INDEX: usize = 6;

impl Device {
    /// The 24-bit class code, as `class` and `modalias` print it:
    /// base class in the high byte, then subclass, then programming interface.
    pub fn class_code(&self) -> u32 {
        ((self.class as u32) << 16) | ((self.subclass as u32) << 8) | self.prog_if as u32
    }

    /// `0000:00:03.0` - the name every path in `/sys` and the `PCI_SLOT_NAME`
    /// key use. Linux's `dev_set_name` for a PCI device, `%04x:%02x:%02x.%d`.
    pub fn sysname(&self) -> String {
        alloc::format!(
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain,
            self.bus,
            self.dev,
            self.func
        )
    }

    /// The device and function as one number, the way configuration space's
    /// address register wants them: `dev << 3 | func`.
    pub fn devfn(&self) -> u8 {
        (self.dev << 3) | self.func
    }

    /// The `/proc/bus/pci` directory this device's configuration file lives
    /// in: the bus, two hex digits.
    pub fn proc_dir(&self) -> String {
        alloc::format!("{:02x}", self.bus)
    }

    /// The `/proc/bus/pci/<bus>/` entry name: `03.0`.
    pub fn proc_name(&self) -> String {
        alloc::format!("{:02x}.{:x}", self.dev, self.func)
    }

    /// `vendor` - Linux's `pci_config_attr(vendor, "0x%04x\n")`.
    ///
    /// The `0x` is load-bearing: pciutils parses this with `strtol(…, 0)`,
    /// which reads a bare `1af4` as *decimal* and stops at the `a`.
    pub fn vendor_attr(&self) -> String {
        alloc::format!("0x{:04x}\n", self.vendor)
    }

    /// `device` - `pci_config_attr(device, "0x%04x\n")`.
    pub fn device_attr(&self) -> String {
        alloc::format!("0x{:04x}\n", self.device)
    }

    /// `subsystem_vendor` - `pci_config_attr(subsystem_vendor, "0x%04x\n")`.
    pub fn subsystem_vendor_attr(&self) -> String {
        alloc::format!("0x{:04x}\n", self.subsys_vendor)
    }

    /// `subsystem_device` - `pci_config_attr(subsystem_device, "0x%04x\n")`.
    pub fn subsystem_device_attr(&self) -> String {
        alloc::format!("0x{:04x}\n", self.subsys_id)
    }

    /// `revision` - `pci_config_attr(revision, "0x%02x\n")`.
    pub fn revision_attr(&self) -> String {
        alloc::format!("0x{:02x}\n", self.revision)
    }

    /// `class` - `pci_config_attr(class, "0x%06x\n")`, the full 24-bit code.
    pub fn class_attr(&self) -> String {
        alloc::format!("0x{:06x}\n", self.class_code())
    }

    /// `irq` - `sysfs_emit(buf, "%u\n", pdev->irq)`, decimal.
    pub fn irq_attr(&self) -> String {
        alloc::format!("{}\n", self.irq)
    }

    /// `resource` - one line per resource, Linux's
    /// `sysfs_emit_at(buf, len, "0x%016llx 0x%016llx 0x%016llx\n", start, end, flags)`.
    ///
    /// Seven lines: the six BARs and the ROM. Linux prints more - the bridge
    /// windows and the SR-IOV resources follow - but it prints *those* from
    /// registers this kernel does not configure, and pciutils reads a file
    /// shorter than those lines as "this device has no bridge windows", which
    /// is the truth here: mpos assigns no window and nothing sits behind one.
    /// It then falls back to reading the bridge's window registers from
    /// configuration space, which is what `lspci -v` on a bridge actually
    /// prints.
    ///
    /// The flags are printed *raw*, not masked - pciutils needs
    /// `IORESOURCE_IO`/`MEM`/`MEM_64` to tell a port BAR from a memory one.
    pub fn resource_attr(&self) -> String {
        let mut out = String::new();
        for bar in &self.bars {
            out.push_str(&alloc::format!(
                "0x{:016x} 0x{:016x} 0x{:016x}\n",
                bar.base,
                bar.end(),
                bar.flags
            ));
        }
        out
    }

    /// `modalias` - Linux's `modalias_show`, the string udev matches a module
    /// against: `pci:v00001AF4d00001001sv00001AF4sd00000001bc01sc00i00`.
    pub fn modalias_attr(&self) -> String {
        alloc::format!(
            "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
            self.vendor,
            self.device,
            self.subsys_vendor,
            self.subsys_id,
            self.class,
            self.subclass,
            self.prog_if
        )
    }

    /// The keys in the device's `uevent` attribute - Linux's `pci_uevent`.
    ///
    /// Not the `ACTION=`-led block a netlink event carries: this attribute is
    /// the *device's* own description, and udev reads it when it wants to know
    /// what a device is without opening its configuration space. The
    /// `MODALIAS` line is what `udevadm trigger`/`mdev` match a driver on.
    pub fn uevent_attr(&self) -> String {
        alloc::format!(
            "PCI_CLASS={:04X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\n\
             PCI_SLOT_NAME={}\nMODALIAS={}\n",
            self.class_code(),
            self.vendor,
            self.device,
            self.subsys_vendor,
            self.subsys_id,
            self.sysname(),
            self.modalias_attr().trim_end(),
        )
    }

    /// One line of `/proc/bus/pci/devices` - Linux's `show_device`.
    ///
    /// The format is `%02x%02x\t%04x%04x\t%x`, then seven 16-wide hex fields
    /// of addresses, seven of sizes, a tab, and the driver's name. pciutils
    /// parses it with `sscanf("%x %x %x" + 14 × "%016llx" + "%n")` and insists
    /// on exactly 17 converted numbers, so the field *count* is part of the
    /// contract: a missing size makes the line unparseable rather than merely
    /// short. It also means the addresses must fit in eight hex digits, which
    /// is why pciutils prefers sysfs and keeps this as its fallback.
    pub fn devices_line(&self) -> String {
        let mut out = alloc::format!(
            "{:02x}{:02x}\t{:04x}{:04x}\t{:x}",
            self.bus,
            self.devfn(),
            self.vendor,
            self.device,
            self.irq
        );
        for bar in &self.bars {
            out.push_str(&alloc::format!("\t{:16x}", bar.proc_base()));
        }
        for bar in &self.bars {
            out.push_str(&alloc::format!("\t{:16x}", bar.size));
        }
        out.push('\t');
        if let Some(driver) = self.driver {
            out.push_str(driver);
        }
        out.push('\n');
        out
    }
}

/// A function with nothing assigned to its BARs, for the tests of the trees
/// that describe devices.
///
/// The two filesystems that publish a device - `/sys` and `/proc` - need the
/// same shape to hang their attributes on, and a copy of it in each would be
/// three chances to describe three different devices.
#[cfg(any(test, feature = "test-helpers"))]
impl Device {
    pub fn fixture(bus: u8, dev: u8, func: u8) -> Device {
        Device {
            domain: 0,
            bus,
            dev,
            func,
            vendor: 0x1af4,
            device: 0x1001,
            revision: 0x00,
            prog_if: 0x00,
            subclass: 0x00,
            class: 0x01,
            header_type: 0x00,
            bridge: None,
            subsys_vendor: 0x1af4,
            subsys_id: 0x0002,
            irq: 0x0b,
            bars: [Bar::EMPTY; 7],
            driver: None,
            drm_card: false,
        }
    }
}

/// Every PCI function the boot found, in bus order.
static DEVICES: Mutex<RefCell<Vec<Device>>> = Mutex::new(RefCell::new(Vec::new()));

/// Replace the device list. Called once, by the boot's enumeration.
///
/// A whole list rather than an `add`, because the list is a *snapshot*: it is
/// taken in one pass while the bus is quiescent, and a caller that could add
/// to it one device at a time could also publish half a machine.
pub fn publish(devices: Vec<Device>) {
    critical_section::with(|cs| {
        *DEVICES.borrow(cs).borrow_mut() = devices;
    });
}

/// The devices, as a copy. The tree builders walk this once at mount time.
pub fn devices() -> Vec<Device> {
    critical_section::with(|cs| DEVICES.borrow(cs).borrow().clone())
}

/// Record that `driver` bound the function at `(bus, dev, func)`.
///
/// Kept separate from [`publish`] because the two happen at different times:
/// the snapshot is taken before any driver runs - so the BAR sizes are probed
/// on a bus nobody is driving - while a claim can only be made once a driver
/// has probed and succeeded. `lspci -k` reads the result, so a claim that is
/// never made shows as a device with no driver, which is the truth for one
/// nothing bound.
pub fn claim(bus: u8, dev: u8, func: u8, driver: &'static str) {
    critical_section::with(|cs| {
        let mut devices = DEVICES.borrow(cs).borrow_mut();
        if let Some(entry) = devices
            .iter_mut()
            .find(|d| d.bus == bus && d.dev == dev && d.func == func)
        {
            entry.driver = Some(driver);
        }
    });
}

/// Declare that the function at `(bus, dev, func)` is the home of the DRM/KMS
/// card, so the tree builder hangs the card's `card0`/`renderD128` nodes off it.
///
/// The honest half of `NOTES.md` §7.4, and the reason it lives here rather than
/// as an inference in `sysfs`: a card belongs to the function whose driver made
/// it, which is a fact only that driver owns. It reaches the tree builder
/// through this [`Device`] record - the driver writes `pci`, `vfs::sysfs` reads
/// `pci` - so the two never reach into each other. The dependency stays the
/// `drivers -> pci` and `vfs -> pci` edges the architecture graph already
/// allows; a direct `drivers -> vfs` registration would be a new back edge the
/// graph forbids.
pub fn mark_drm_card(bus: u8, dev: u8, func: u8) {
    critical_section::with(|cs| {
        let mut devices = DEVICES.borrow(cs).borrow_mut();
        if let Some(entry) = devices
            .iter_mut()
            .find(|d| d.bus == bus && d.dev == dev && d.func == func)
        {
            entry.drm_card = true;
        }
    });
}

/// Taking turns on the one global device list, for tests. The crate has no
/// `std` and the host build has no `spin`, so this is a lock made of one
/// atomic - all a test needs to keep another test's devices out of its tree.
#[cfg(any(test, feature = "test-helpers"))]
pub mod test_lock {
    use core::sync::atomic::{AtomicBool, Ordering};

    static LOCK: AtomicBool = AtomicBool::new(false);

    pub struct ListLock;

    pub fn take() -> ListLock {
        while LOCK.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        ListLock
    }

    impl Drop for ListLock {
        fn drop(&mut self) {
            LOCK.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A virtio-blk function as QEMU's q35 puts one on the bus: BAR0 is a
    /// 16 KiB memory BAR, the ROM is disabled, and there is an interrupt line.
    fn virtio_blk() -> Device {
        Device {
            domain: 0,
            bus: 0,
            dev: 3,
            func: 0,
            vendor: 0x1af4,
            device: 0x1001,
            revision: 0x00,
            prog_if: 0x00,
            subclass: 0x00,
            class: 0x01,
            header_type: 0x00,
            bridge: None,
            subsys_vendor: 0x1af4,
            subsys_id: 0x0002,
            irq: 0x0b,
            bars: [
                Bar {
                    base: 0xc000,
                    size: 0x4000,
                    flags: IORESOURCE_MEM | IORESOURCE_SIZEALIGN,
                },
                Bar::EMPTY,
                Bar::EMPTY,
                Bar::EMPTY,
                Bar::EMPTY,
                Bar::EMPTY,
                Bar::EMPTY,
            ],
            driver: None,
            drm_card: false,
        }
    }

    /// A PCI-to-PCI bridge: header type 1, so only BAR0 and BAR1 are BARs and
    /// the words at 0x18/0x1c/0x20/0x24 are its bus numbers and windows.
    fn bridge() -> Device {
        Device {
            domain: 0,
            bus: 0,
            dev: 1,
            func: 2,
            vendor: 0x1b36,
            device: 0x000c,
            revision: 0x00,
            prog_if: 0x00,
            subclass: 0x04,
            class: 0x06,
            header_type: 0x01,
            bridge: Some(0x01),
            subsys_vendor: 0x0000,
            subsys_id: 0x0000,
            irq: 0x00,
            bars: [Bar::EMPTY; 7],
            driver: None,
            drm_card: false,
        }
    }

    #[test]
    fn the_attributes_carry_the_prefix_pciutils_needs() {
        // `strtol(…, 0)` reads "1af4" as decimal 1 and "0x1af4" as 0x1af4, so
        // the prefix is what makes the vendor name resolve at all.
        let d = virtio_blk();
        assert_eq!(d.vendor_attr(), "0x1af4\n");
        assert_eq!(d.device_attr(), "0x1001\n");
        assert_eq!(d.subsystem_vendor_attr(), "0x1af4\n");
        assert_eq!(d.subsystem_device_attr(), "0x0002\n");
        assert_eq!(d.revision_attr(), "0x00\n");
        assert_eq!(d.class_attr(), "0x010000\n");
    }

    #[test]
    fn the_irq_attribute_is_decimal_and_the_class_is_the_full_code() {
        let mut d = virtio_blk();
        d.irq = 25;
        // `%u`, not `%x`: pciutils reads this one with base 0 too, but Linux
        // prints it in decimal and 25 must not read as 0x25.
        assert_eq!(d.irq_attr(), "25\n");
        d.class = 0x0c;
        d.subclass = 0x05;
        d.prog_if = 0x00;
        assert_eq!(d.class_attr(), "0x0c0500\n");
        assert_eq!(d.class_code(), 0x0c0500);
    }

    #[test]
    fn the_resource_attribute_lists_seven_lines_with_the_raw_flags() {
        let d = virtio_blk();
        let text = d.resource_attr();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 7, "six BARs and the ROM");
        // start, end, flags - and the flags are the raw IORESOURCE_* word, not
        // the masked one: pciutils tells a port BAR from a memory one by them.
        assert_eq!(
            lines[0],
            "0x000000000000c000 0x000000000000ffff 0x0000000000040200"
        );
        assert_eq!(
            lines[1],
            "0x0000000000000000 0x0000000000000000 0x0000000000000000"
        );
        assert_eq!(
            lines[6],
            "0x0000000000000000 0x0000000000000000 0x0000000000000000"
        );
    }

    #[test]
    fn an_empty_resource_prints_no_end_before_its_start() {
        // A size of zero has no last address; printing `base + size - 1` would
        // underflow to 0xffffffffffffffff and pciutils would read that as an
        // enormous region rather than as no region at all.
        let bar = Bar {
            base: 0xfebf_0000,
            size: 0,
            flags: IORESOURCE_MEM,
        };
        assert_eq!(bar.end(), 0);
        // A live one ends at its last byte, not one past it.
        let bar = Bar {
            base: 0xfebf_0000,
            size: 0x1000,
            flags: IORESOURCE_MEM,
        };
        assert_eq!(bar.end(), 0xfebf_0fff);
    }

    #[test]
    fn a_rom_resource_keeps_the_enable_bit_in_the_printed_address() {
        // `decode_bar` folds the BAR's own low bits into the flags, and
        // `/proc/bus/pci/devices` prints the address with `PCI_REGION_FLAG_MASK`
        // kept - a live ROM BAR reads back with bit 0 set.
        let bar = Bar {
            base: 0xc0000,
            size: 0x10000,
            flags: IORESOURCE_MEM
                | IORESOURCE_PREFETCH
                | IORESOURCE_READONLY
                | IORESOURCE_SIZEALIGN
                | IORESOURCE_ROM_ENABLE,
        };
        assert_eq!(bar.proc_base(), 0xc0001);
        let bar = Bar {
            base: 0xc0000,
            size: 0x10000,
            flags: IORESOURCE_MEM,
        };
        assert_eq!(bar.proc_base(), 0xc0000);
    }

    #[test]
    fn the_modalias_matches_linuxs_spelling() {
        let d = virtio_blk();
        // `pci:v%08Xd%08Xsv%08Xsd%08Xbc%02Xsc%02Xi%02X` - uppercase hex, and
        // the class bytes split out of the 24-bit code.
        assert_eq!(
            d.modalias_attr(),
            "pci:v00001AF4d00001001sv00001AF4sd00000002bc01sc00i00\n"
        );
    }

    #[test]
    fn the_uevent_attribute_carries_the_five_keys_udev_reads() {
        let d = virtio_blk();
        // `%04X` is a *minimum* width, so the 24-bit class prints as five
        // digits and a leading zero is not invented - which is exactly what
        // Linux prints, and what udev reads.
        assert_eq!(
            d.uevent_attr(),
            "PCI_CLASS=10000\n\
             PCI_ID=1AF4:1001\n\
             PCI_SUBSYS_ID=1AF4:0002\n\
             PCI_SLOT_NAME=0000:00:03.0\n\
             MODALIAS=pci:v00001AF4d00001001sv00001AF4sd00000002bc01sc00i00\n"
        );
        // A class whose high byte is not a single digit keeps its width.
        let mut d = virtio_blk();
        d.class = 0x06;
        d.subclass = 0x00;
        d.prog_if = 0x00;
        assert!(d.uevent_attr().starts_with("PCI_CLASS=60000\n"));
    }

    #[test]
    fn the_names_are_the_ones_the_two_trees_are_built_from() {
        let d = virtio_blk();
        assert_eq!(d.sysname(), "0000:00:03.0");
        assert_eq!(d.devfn(), 0x18);
        assert_eq!(d.proc_dir(), "00");
        assert_eq!(d.proc_name(), "03.0");
        let d = bridge();
        assert_eq!(d.sysname(), "0000:00:01.2");
        assert_eq!(d.devfn(), 0x0a, "device 1, function 2");
        assert_eq!(d.proc_name(), "01.2");
    }

    /// pciutils' own `sscanf` format, transcribed: three hex numbers, then
    /// fourteen 16-wide hex ones, then the offset the driver name starts at.
    /// Returning the field count is the point - the parser insists on 17.
    fn pciutils_parse(line: &str) -> (Vec<u64>, Option<&str>) {
        let mut fields = Vec::new();
        let mut rest = line;
        // `%02x%02x` and `%04x%04x` are single numbers to the parser: `%x`
        // stops at the tab.
        for _ in 0..3 {
            let end = rest
                .find(|c: char| !c.is_ascii_hexdigit())
                .expect("a separator after every number");
            fields.push(u64::from_str_radix(&rest[..end], 16).expect("a hex number"));
            rest = rest[end..].trim_start_matches([' ', '\t']);
        }
        for _ in 0..14 {
            // `%016llx`: whitespace is skipped, then up to sixteen hex digits.
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .take(16)
                .collect();
            assert!(!digits.is_empty(), "a sixteenth-of-a-field must be there");
            fields.push(u64::from_str_radix(&digits, 16).expect("a hex number"));
            rest = rest[digits.len()..].trim_start_matches([' ', '\t']);
        }
        let driver = rest.trim();
        (
            fields,
            if driver.is_empty() {
                None
            } else {
                Some(driver)
            },
        )
    }

    #[test]
    fn the_devices_line_is_the_seventeen_numbers_pciutils_insists_on() {
        let d = virtio_blk();
        let line = d.devices_line();
        let (fields, driver) = pciutils_parse(&line);
        assert_eq!(fields.len(), 17, "pciutils' `cnt == 17`");
        // The first field is `bus << 8 | devfn`, one number - pciutils takes
        // it apart as `d->bus = dfn >> 8`, `PCI_SLOT(dfn & 0xff)`,
        // `PCI_FUNC(dfn & 0xff)`. Device 3 function 0 is devfn 0x18.
        assert_eq!(fields[0], 0x18);
        assert_eq!((fields[0] >> 8) as u8, 0, "the bus");
        assert_eq!(((fields[0] & 0xff) >> 3) as u8, 3, "the slot");
        assert_eq!((fields[0] & 7) as u8, 0, "the function");
        assert_eq!(fields[1], 0x1af4_1001, "vendor and device, packed");
        assert_eq!(fields[2], 0x0b, "the interrupt line");
        // The seven addresses come first, then the seven sizes.
        assert_eq!(fields[3], 0xc000);
        assert_eq!(fields[10], 0x4000, "BAR0's size, the seventh field on");
        assert_eq!(driver, None, "nothing claimed this one");
        // A tab closes the line even with no driver: the parser needs the
        // separator to have something to skip past.
        assert!(line.ends_with("\t\n"), "the driver field is still there");
    }

    #[test]
    fn a_claimed_device_names_its_driver_in_the_devices_line() {
        let mut d = bridge();
        d.driver = Some("pcieport");
        d.bars[0] = Bar {
            base: 0x1000,
            size: 0x1000,
            flags: IORESOURCE_IO | 0x1 | IORESOURCE_SIZEALIGN,
        };
        d.bars[1] = Bar {
            base: 0xfe000000,
            size: 0x100000,
            flags: IORESOURCE_MEM
                | IORESOURCE_PREFETCH
                | IORESOURCE_MEM_64
                | IORESOURCE_SIZEALIGN
                | 0x4,
        };
        let line = d.devices_line();
        let (fields, driver) = pciutils_parse(&line);
        assert_eq!(fields.len(), 17);
        assert_eq!(fields[0], 0x0a, "bus 0, device 1, function 2");
        assert_eq!(fields[1], 0x1b36_000c);
        // The address keeps the BAR's low type bits, as Linux's
        // `PCI_REGION_FLAG_MASK` keeps them; the size does not.
        assert_eq!(fields[3], 0x1001);
        assert_eq!(fields[4], 0xfe000004);
        assert_eq!(fields[10], 0x1000);
        assert_eq!(fields[11], 0x100000);
        assert_eq!(driver, Some("pcieport"));
    }

    #[test]
    fn the_line_is_the_sixteen_wide_hex_linux_prints() {
        // Not cosmetic: pciutils' format is `%08lx` for a 64-bit address, so a
        // field with more than eight significant digits is read as two and the
        // whole line shifts. A short field is still correct - the value must
        // simply never need more than eight.
        let d = virtio_blk();
        let line = d.devices_line();
        let addresses: Vec<&str> = line.split('\t').skip(3).take(14).collect();
        assert_eq!(addresses.len(), 14);
        for field in addresses {
            assert_eq!(field.len(), 16, "{field:?} is padded to sixteen");
            assert!(
                field.trim().len() <= 8,
                "{field:?} must fit pciutils' eight-digit field"
            );
        }
    }

    #[test]
    fn a_memory_bar_decodes_its_address_and_size() {
        // A 1 MiB 32-bit memory BAR at 0xfe000000: asked to decode everything
        // it answers with the bits it cannot decode cleared, and the size is
        // the lowest of them.
        assert_eq!(bar_flags(0xfe00_0000), (IORESOURCE_MEM, false));
        let bar = bar_range(0xfe00_0000, 0, 0xfff0_0000, 0).expect("a BAR that sizes");
        assert_eq!(bar.base, 0xfe00_0000);
        assert_eq!(bar.size, 0x0010_0000);
        assert_eq!(bar.flags, IORESOURCE_MEM | IORESOURCE_SIZEALIGN);
        // A prefetchable one carries the extra bit, which is what `lspci`
        // prints as "prefetchable" - and the BAR's own bit 3 stays in the
        // flags beside it, as Linux's `decode_bar` leaves it.
        assert_eq!(
            bar_flags(0xfe00_0008),
            (0x0000_0008 | IORESOURCE_MEM | IORESOURCE_PREFETCH, false)
        );
    }

    #[test]
    fn a_real_64_bit_bar_decodes_to_the_flags_linux_prints() {
        // An NVMe drive's BAR0 on a real machine: 16 KiB at 0xdcc00000, and
        // `/sys/.../resource` prints `0x00000000dcc00000 0x00000000dcc03fff
        // 0x0000000000140204`. The flags are the BAR's own type bit 4, the
        // memory bit, the alignment bit, and the 64-bit bit - so a reader that
        // drops any of them describes a different device.
        let (flags, wide) = bar_flags(0xdcc0_0004);
        assert!(wide);
        assert_eq!(flags, 0x0004 | IORESOURCE_MEM | IORESOURCE_MEM_64);
        let bar = bar_range(0xdcc0_0004, 0, 0xffff_c004, 0xffff_ffff).expect("a BAR that sizes");
        assert_eq!(bar.base, 0xdcc0_0000);
        assert_eq!(bar.size, 0x4000);
        assert_eq!(bar.flags, 0x0014_0204);
        // An I/O BAR keeps its space indicator and reserved bit instead.
        let (flags, wide) = bar_flags(0x0000_c101);
        assert!(!wide);
        assert_eq!(flags, 0x0000_0101);
    }

    #[test]
    fn a_64_bit_memory_bar_takes_its_upper_half_from_the_next_register() {
        // A 16 MiB BAR above 4 GiB, as QEMU's q35 puts a 64-bit MMIO BAR: the
        // low half holds the type bits and the high half the address, and the
        // size is only visible when both halves are read.
        let (flags, wide) = bar_flags(0x0000_0004);
        assert!(wide);
        assert_eq!(flags & IORESOURCE_MEM_64, IORESOURCE_MEM_64);
        let bar = bar_range(0x0000_0004, 0x0000_0038, 0xff00_0000, 0xffff_ffff)
            .expect("a BAR that sizes");
        assert_eq!(bar.base, 0x0000_0038_0000_0000);
        assert_eq!(bar.size, 0x0100_0000);
        // The upper half is ignored for a 32-bit BAR, so a caller that passes
        // a bridge's bus-number register there is not silently reading a
        // second BAR.
        let bar = bar_range(0xfe00_0000, 0x0000_0019, 0xfff0_0000, 0xffff_ffff)
            .expect("a BAR that sizes");
        assert_eq!(bar.base, 0xfe00_0000);
        assert_eq!(bar.size, 0x0010_0000);
    }

    #[test]
    fn an_io_bar_sizes_within_the_port_space() {
        // A 64-port I/O BAR at 0xc000. A 16-bit port BAR answers with its
        // upper half zero, so the read-back is 0xffc1, not 0xffff_ffc1.
        assert_eq!(bar_flags(0x0000_c001), (0x0000_0101, false));
        let bar = bar_range(0x0000_c001, 0, 0x0000_ffc1, 0).expect("a BAR that sizes");
        assert_eq!(bar.base, 0x0000_c000);
        assert_eq!(bar.size, 0x0000_0040);
        assert_eq!(bar.flags, 0x0004_0101);
        // The port space is 64 KiB, so an I/O BAR that decodes half of it
        // still sizes correctly.
        let bar = bar_range(0x0000_8001, 0, 0x0000_8001, 0).expect("a BAR that sizes");
        assert_eq!(bar.base, 0x0000_8000);
        assert_eq!(bar.size, 0x0000_8000);
    }

    #[test]
    fn a_bar_nothing_answers_for_decodes_nothing() {
        // An unimplemented BAR reads back as zero after being asked to decode
        // everything - there is no size to be read.
        assert_eq!(bar_range(0x0000_0000, 0, 0x0000_0000, 0), None);
        // A read that answers all ones never reached a device at all.
        assert_eq!(
            bar_range(PCI_ERROR_RESPONSE, 0, PCI_ERROR_RESPONSE, 0),
            None
        );
        // And a mask with no address bits in it sizes nothing even though the
        // device answered.
        assert_eq!(bar_range(0x0000_0000, 0, 0x0000_000f, 0), None);
    }

    #[test]
    fn the_rom_bar_keeps_its_enable_and_never_sizes_the_reserved_bits() {
        // A 64 KiB ROM at 0xc0000, currently decoding.
        let rom = rom_bar(0x000c_0001, 0xffff_0000);
        assert_eq!(rom.base, 0x000c_0000);
        assert_eq!(rom.size, 0x0001_0000);
        assert_eq!(
            rom.flags,
            IORESOURCE_MEM
                | IORESOURCE_PREFETCH
                | IORESOURCE_READONLY
                | IORESOURCE_SIZEALIGN
                | IORESOURCE_ROM_ENABLE
        );
        // Not decoding: the same ROM, without the enable bit in the address
        // `/proc/bus/pci/devices` prints.
        let rom = rom_bar(0x000c_0000, 0xffff_0000);
        assert_eq!(rom.flags & IORESOURCE_ROM_ENABLE, 0);
        assert_eq!(rom.proc_base(), 0x000c_0000);
        assert_eq!(
            rom_bar(0x000c_0001, 0xffff_0000).proc_base(),
            0x000c_0001,
            "the enable bit survives into the printed address"
        );
        // A device with no ROM: the mask reads back zero.
        assert_eq!(rom_bar(0x0000_0000, 0x0000_0000), Bar::EMPTY);
    }

    #[test]
    fn a_device_is_found_by_the_numbers_its_path_is_made_of() {
        let d = virtio_blk();
        assert_eq!(
            (d.domain, d.bus, d.dev, d.func),
            (0, 0, 3, 0),
            "the four numbers the sysfs path and the scan both use"
        );
    }

    #[test]
    fn publishing_replaces_the_list_and_a_claim_lands_on_its_device() {
        let _lock = test_lock::take();
        publish(vec![virtio_blk(), bridge()]);
        assert_eq!(devices().len(), 2);
        assert_eq!(devices()[0].driver, None);

        claim(0, 3, 0, "virtio-pci");
        let after = devices();
        assert_eq!(after[0].driver, Some("virtio-pci"));
        assert_eq!(after[1].driver, None, "an unclaimed device names nobody");

        // A claim on a function that is not there is dropped rather than
        // inventing a device: a driver that probes a device it did not find
        // has nothing to name.
        claim(0, 31, 7, "ghost");
        assert_eq!(devices().len(), 2);

        // A second publish is a new snapshot, and the old claims go with it.
        publish(vec![bridge()]);
        assert_eq!(devices().len(), 1);
        assert_eq!(
            devices()[0].driver,
            None,
            "the claim went with the snapshot"
        );
        publish(Vec::new());
    }
}
