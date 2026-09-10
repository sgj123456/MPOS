//! 8259 PIC.
//!
//! The legacy PIC is what QEMU's q35 machine wires the PIT and keyboard to. It
//! gets replaced by the IOAPIC once we care about real hardware performance,
//! but for a uniprocessor kernel the remapped PIC is entirely sufficient and
//! much simpler.

use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;

const ICW1_INIT: u8 = 0x11;
const ICW4_8086: u8 = 0x01;

/// Vectors the two PICs are remapped onto. IRQ0..7 -> 32..39, IRQ8..15 -> 40..47.
pub const IRQ0_VECTOR: u8 = 32;
pub const IRQ8_VECTOR: u8 = 40;

pub fn remap() {
    unsafe {
        let mut cmd1: Port<u8> = Port::new(PIC1_CMD);
        let mut cmd2: Port<u8> = Port::new(PIC2_CMD);
        let mut data1: Port<u8> = Port::new(PIC1_DATA);
        let mut data2: Port<u8> = Port::new(PIC2_DATA);

        // Save masks so we can restore them after the init sequence clobbers them.
        let mask1 = data1.read();
        let mask2 = data2.read();

        cmd1.write(ICW1_INIT);
        cmd2.write(ICW1_INIT);
        data1.write(IRQ0_VECTOR);
        data2.write(IRQ8_VECTOR);
        data1.write(4); // PIC1 has slave on IRQ2
        data2.write(2); // slave identity
        data1.write(ICW4_8086);
        data2.write(ICW4_8086);

        data1.write(mask1);
        data2.write(mask2);
    }
}

/// Mask (disable) or unmask (enable) one IRQ line.
pub fn set_masked(irq: u8, masked: bool) {
    let (port, bit) = if irq < 8 {
        (PIC1_DATA, irq)
    } else {
        (PIC2_DATA, irq - 8)
    };
    unsafe {
        let mut p: Port<u8> = Port::new(port);
        let value = p.read();
        let next = if masked {
            value | (1 << bit)
        } else {
            value & !(1 << bit)
        };
        p.write(next);
    }
}

/// The master PIC's current mask, for boot-time checks that an IRQ really is
/// unmasked. Nothing in the kernel's own paths needs to read it back.
pub fn irq_mask() -> u8 {
    let mut port: Port<u8> = Port::new(PIC1_DATA);
    unsafe { port.read() }
}

/// Non-specific EOI. Fine for a uniprocessor kernel without priority nesting.
pub fn end_of_interrupt(irq: u8) {
    unsafe {
        if irq >= 8 {
            let mut slave: Port<u8> = Port::new(PIC2_CMD);
            slave.write(0x20);
        }
        let mut master: Port<u8> = Port::new(PIC1_CMD);
        master.write(0x20);
    }
}
