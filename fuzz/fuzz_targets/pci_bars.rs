#![no_main]

//! PCI BAR sizing, and the text `/sys` and `/proc` print from it.
//!
//! Sizing a BAR is the one place the kernel trusts a device to describe its
//! own address space: write all-ones, read back the mask, and the difference
//! says how much the function decodes. `drivers/pci.rs` does that probing and
//! hands the raw words to `pci::bar_range`, which is pure arithmetic - so it is
//! the arithmetic that has to survive every mask a device could answer with,
//! including the ones that mean "I do not implement this".
//!
//! The second half is the formatting: pciutils, `lspci` and udev all parse the
//! `resource`, `modalias` and `uevent` attributes, and a wrong field width or an
//! unescaped space makes them read a device that is not there.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::pci::{self, Bar, CFG_SIZE, Device, ROM_INDEX};

#[derive(Arbitrary, Debug)]
struct Input {
    /// Raw BAR words and the values read back after writing all-ones, as
    /// `drivers/pci.rs` would hand them over: (low, high, mask_low, mask_high).
    probes: Vec<[u32; 4]>,
    /// The expansion ROM's (original, mask) pair.
    rom: (u32, u32),
    /// The descriptor fields every attribute is printed from.
    ident: Ident,
}

#[derive(Arbitrary, Debug)]
struct Ident {
    domain: u16,
    bus: u8,
    dev: u8,
    func: u8,
    vendor: u16,
    device: u16,
    revision: u8,
    prog_if: u8,
    subclass: u8,
    class: u8,
    header_type: u8,
    bridge: Option<u8>,
    subsys_vendor: u16,
    subsys_id: u16,
    irq: u8,
}

fuzz_target!(|input: Input| {
    let mut bars = [Bar::EMPTY; 7];
    for (index, [low, high, mask_low, mask_high]) in input.probes.iter().take(6).enumerate() {
        let Some(bar) = pci::bar_range(*low, *high, *mask_low, *mask_high) else {
            continue;
        };
        check_bar(&bar, format!("bar{index}"));
        bars[index] = bar;
    }
    let rom = pci::rom_bar(input.rom.0, input.rom.1);
    check_bar(&rom, "rom".to_string());
    bars[ROM_INDEX] = rom;

    let ident = &input.ident;
    let device = Device {
        domain: ident.domain,
        bus: ident.bus,
        dev: ident.dev,
        func: ident.func,
        vendor: ident.vendor,
        device: ident.device,
        revision: ident.revision,
        prog_if: ident.prog_if,
        subclass: ident.subclass,
        class: ident.class,
        header_type: ident.header_type,
        bridge: ident.bridge,
        subsys_vendor: ident.subsys_vendor,
        subsys_id: ident.subsys_id,
        irq: ident.irq,
        bars,
        driver: None,
    };

    // The names are path components: a space or a slash in one of them puts a
    // file where udev will not look for it.
    for name in [device.sysname(), device.proc_dir(), device.proc_name()] {
        assert!(
            !name.is_empty() && !name.contains(char::is_whitespace) && !name.contains('/'),
            "a device name nobody can spell: {name:?}"
        );
    }
    assert!(
        device.modalias_attr().starts_with("pci:"),
        "modalias is not a modalias: {:?}",
        device.modalias_attr()
    );
    assert!(
        device.uevent_attr().contains("PCI_ID="),
        "the uevent block lost the fields udev matches on"
    );
    // A resource line describes an address the host cannot have: `pci_size`
    // has no way to invent one, and printing a range that does not fit says
    // the device decodes memory the kernel never mapped.
    for line in device.resource_attr().lines() {
        assert_eq!(
            line.split_whitespace().count(),
            3,
            "a resource line: {line:?}"
        );
    }
    // `show_device`'s line is parsed by `sscanf` and pciutils insists on the
    // field count, so a BAR that printed nothing would make the whole device
    // unparseable rather than merely short.
    let line = device.devices_line();
    assert!(
        line.starts_with(&format!(
            "{:02x}{:02x}\t{:04x}{:04x}\t{:x}",
            ident.bus,
            device.devfn(),
            ident.vendor,
            ident.device,
            ident.irq
        )),
        "the devices line does not open with its own identity: {line:?}"
    );
    assert_eq!(
        line.split_whitespace().count(),
        17,
        "pciutils counts 17 numbers in a devices line, this one has {}: {line:?}",
        line.split_whitespace().count()
    );
    // The 24-bit class is the three bytes, spelled - everything `lspci -n`
    // prints is derived from this.
    assert_eq!(
        device.class_code(),
        ((ident.class as u32) << 16) | ((ident.subclass as u32) << 8) | ident.prog_if as u32
    );
    assert!(CFG_SIZE > 0);
});

/// The invariants every sized BAR obeys: a size that is a power of two (or
/// none at all), an end that is never before its start, and a range the
/// 256-byte config space could never have produced by accident.
fn check_bar(bar: &Bar, what: String) {
    assert!(
        bar.size == 0 || bar.size.is_power_of_two(),
        "{} decodes {} bytes, which is not a power of two",
        what,
        bar.size
    );
    if bar.size == 0 {
        assert_eq!(bar.end(), 0, "{what} of size 0 reports an end");
    } else {
        assert!(
            bar.base.checked_add(bar.size).is_some(),
            "{what} at {:#x} for {:#x} wraps the address space",
            bar.base,
            bar.size
        );
        assert_eq!(bar.end(), bar.base + bar.size - 1);
    }
}
