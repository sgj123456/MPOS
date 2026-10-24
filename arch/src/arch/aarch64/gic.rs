//! GICv2, as QEMU's `virt` machine wires it.
//!
//! Measured rather than assumed: the machine's own device tree (QEMU
//! 11.1.1, `-M virt`, `-machine dumpdtb`) says `arm,cortex-a15-gic` with
//! `reg = <0 0x0800_0000 0 0x1_0000 0 0x0801_0000 0 0x1_0000>` - the
//! distributor and the CPU interface, 64 KiB each. That is why this file is
//! GICv2 and not GICv3: the two are different *mechanisms* (GICv3's CPU
//! interface is system registers, not a second MMIO frame), so "which one" is
//! the first thing to establish. Stage 5a reads this node out of the tree and
//! holds it against the constants below, so the fixed pair is now a checked
//! claim about one machine rather than an assumption about it.
//!
//! The two interrupt numbers this kernel uses are measured the same way: the
//! PL011's node carries `interrupts = <0 1 4>` (SPI 1, so INTID 33) and the
//! `arm,armv7-timer` node's second entry is PPI 14 - the non-secure EL1
//! physical timer - so INTID 30.

use super::paging;

/// The distributor's physical address: what the firmware's device tree names,
/// and what `check_devices` compares the tree against.
pub const DIST_PHYS: usize = 0x0800_0000;
/// The CPU interface's physical address, likewise.
pub const CPU_PHYS: usize = 0x0801_0000;

/// The distributor as this kernel reaches it: through the kernel half's device
/// window. [`paging::DEVICE_VIRT`] says why the registers are not reached at the
/// address they physically are; both are spelled as the window's base plus the
/// frame's offset into it, so a window that moves takes them with it.
pub const DIST: usize = paging::DEVICE_VIRT + (DIST_PHYS - paging::DEVICE_PHYS);
/// The CPU interface, likewise: the second `reg` window of the same node.
pub const CPU: usize = paging::DEVICE_VIRT + (CPU_PHYS - paging::DEVICE_PHYS);

/// Distributor control: bit 0 enables group 0 (a FIQ), bit 1 group 1 (an IRQ).
const GICD_CTLR: usize = 0x000;
/// Interrupt group: one bit per INTID. Group 1 is what reaches an IRQ.
const GICD_IGROUPR: usize = 0x080;
/// Set-enable: one bit per INTID, write-1-to-set.
const GICD_ISENABLER: usize = 0x100;
/// Clear-enable: one bit per INTID, write-1-to-clear. [`GICD_ISENABLER`]'s
/// inverse, and the reason [`disable`] is a plain store rather than a
/// read-modify-write like [`enable`]'s `set_bit`: writing the bit clears it, and
/// the other bits in the word are unaffected.
const GICD_ICENABLER: usize = 0x180;
/// Priority: one byte per INTID. Lower is more urgent.
const GICD_IPRIORITYR: usize = 0x400;

/// CPU interface control: bit 0 enables group 0, bit 1 group 1, and bit 2 is
/// `AckCtl` - see [`GICC_CTLR_ACK_GRP1`].
const GICC_CTLR: usize = 0x0000;
/// Priority mask: only interrupts with a priority *lower* than this are
/// delivered, so `0xff` lets everything through.
const GICC_PMR: usize = 0x0004;
/// Acknowledge: reading it returns the INTID being taken.
const GICC_IAR: usize = 0x000c;
/// End of interrupt: writing the INTID retires it.
const GICC_EOIR: usize = 0x0010;

/// What a read of [`GICC_IAR`] reports when there is nothing to acknowledge.
/// It must **not** be written to `EOIR`.
pub const SPURIOUS: u32 = 1023;

/// The non-secure EL1 physical timer's PPI (14), as INTID 30.
pub const TIMER_INTID: u32 = 30;

/// The non-secure EL1 **virtual** timer's PPI (11), as INTID 27 - the one-shot
/// a sub-tick wake is armed with (`timer::arm_wake_ns`). The architecture
/// counts its PPIs from 16, so PPI 11 lands here; the device tree's `timer`
/// node names the same pair (PPI 14 the physical clock, PPI 11 the virtual
/// one), and `check_devices` reads both against this file.
pub const VTIMER_INTID: u32 = 27;

/// The PL011's SPI 1, as INTID 33 - the tree's `interrupts = <0 1 4>`. It is
/// wired but unmasked nothing yet (the console is polled until stage 4); it is
/// named so the `Line::Serial` a driver will unmask maps to a real line here.
pub const SERIAL_INTID: u32 = 33;

fn read(base: usize, offset: usize) -> u32 {
    // SAFETY: every offset above names a register of one of the two frames, and
    // both frames are device memory the machine maps from reset.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

fn write(base: usize, offset: usize, value: u32) {
    // SAFETY: as `read`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) };
}

fn set_bit(base: usize, offset: usize, intid: u32) {
    let reg = offset + 4 * (intid / 32) as usize;
    let bit = 1 << (intid % 32);
    write(base, reg, read(base, reg) | bit);
}

/// The byte-wide priority of one interrupt, read-modify-write: four interrupts
/// share a word, so a plain store would clear its three neighbours.
fn set_priority(intid: u32, priority: u8) {
    let word = GICD_IPRIORITYR + (intid as usize & !3);
    let shift = (intid % 4) * 8;
    let old = read(DIST, word);
    let new = (old & !(0xff << shift)) | ((priority as u32) << shift);
    write(DIST, word, new);
}

/// Let `intid` be delivered.
pub fn enable(intid: u32) {
    set_bit(DIST, GICD_ISENABLER, intid);
}

/// Stop `intid` being delivered. [`enable`]'s inverse; a device that is about to
/// be torn down masks its own line first so no interrupt lands on a driver that
/// has let go of its rings.
pub fn disable(intid: u32) {
    let reg = GICD_ICENABLER + 4 * (intid / 32) as usize;
    write(DIST, reg, 1 << (intid % 32));
}

/// Whether `intid` is currently enabled, as the distributor says. [`enable`] and
/// [`disable`] set the bit; this reads it back, which is what makes a mask that
/// silently did nothing a failure rather than a line that quietly never fires.
pub fn is_enabled(intid: u32) -> bool {
    let reg = GICD_ISENABLER + 4 * (intid / 32) as usize;
    read(DIST, reg) & (1 << (intid % 32)) != 0
}

/// Bring the controller up with the timer's line enabled.
///
/// The order matters and is the TRM's: the distributor is configured with it
/// disabled, so no interrupt can be signalled against a half-written
/// configuration - which for a *level*-triggered line like the timer means an
/// interrupt arriving with a priority or group that is not set yet.
///
/// Both group-enable bits are set on each interface, and that is not tidiness.
/// An IRQ is a *group 1* interrupt, and a group-1 interrupt is delivered only
/// when the corresponding `EnableGrp1` bit is set: writing bit 0 alone leaves
/// the interrupt pending in the distributor forever, which is exactly what this
/// machine did before the bits were both written - silently, since a pending
/// interrupt that is never delivered raises nothing. Bit 0 (group 0) is written
/// too because a GIC built without a security extension, which is what `-M virt`
/// gives by default, has only the one bank and reads both bits from it.
///
/// `AckCtl` is the third bit and it was measured rather than reasoned out. With
/// it clear, this machine's timer interrupt was acknowledged as INTID **1022**,
/// not 30 - the GICv2 specification's answer when the highest-priority pending
/// interrupt is group 1 and the acknowledging access is *secure*
/// (`GICC_IAR`: `if !NS_access && AckCtl == '0' then pendID = 1022`). The
/// interrupt stayed pending, `nIRQ` stayed asserted, and the vector was
/// re-entered every few hundred nanoseconds: a storm of unacknowledgeable
/// interrupts, which reads from a distance as "the clock is very fast". Linux
/// programs the same `0x7` (`GICC_CTLR_ENABLE_BITS`), which is what made the
/// register value - rather than the group choice - the thing to suspect.
pub fn init() {
    // Both lines this kernel knows about are *configured* here and enabled by
    // the driver that wants one - the timer immediately, because nothing in this
    // machine runs without a clock, and the serial only later: its handler hands
    // bytes to the console's tty, so a line the boot opened before that layer
    // existed would interrupt into a panic. Group and priority are set now
    // anyway, because changing a line's group while it could already be pending
    // is exactly the ordering the disable-then-write above exists to protect.
    write(DIST, GICD_CTLR, 0);
    set_bit(DIST, GICD_IGROUPR, TIMER_INTID);
    set_priority(TIMER_INTID, 0x80);
    // The virtual timer's line is configured here too, for the same reason the
    // physical one is - a one-shot armed before the driver that wants it exists
    // must land on a line the controller will deliver, and the scheduler arms
    // its first wake on the first turn after the clock starts. The same
    // priority as the clock: a sub-tick wake is a scheduling event, no more and
    // no less urgent than the tick itself.
    set_bit(DIST, GICD_IGROUPR, VTIMER_INTID);
    set_priority(VTIMER_INTID, 0x80);
    // ... and *enabled*, the step the first cut of this line forgot: group and
    // priority are configuration, delivery is the enable bit, and a line with
    // the first two and not the third holds a fired interrupt in the
    // distributor's pending set for ever - measured (the bring-up check's own
    // register dump: ISTATUS set, count past the compare, ISENABLER0 with bit
    // 27 clear) before this sentence was.
    enable(VTIMER_INTID);
    set_bit(DIST, GICD_IGROUPR, SERIAL_INTID);
    // The same priority as the clock: the console input a person typed is not
    // worth less than a tick, and not worth more either - neither line can
    // starve the other, which is what two equal priorities buy.
    set_priority(SERIAL_INTID, 0x80);
    enable(TIMER_INTID);
    write(
        DIST,
        GICD_CTLR,
        GICD_CTLR_ENABLE_GRP0 | GICD_CTLR_ENABLE_GRP1,
    );

    // `0xff` accepts every priority; a lower mask would silently swallow the
    // timer's 0x80 as well as anything else.
    write(CPU, GICC_PMR, 0xff);
    write(
        CPU,
        GICC_CTLR,
        GICC_CTLR_ENABLE_GRP0 | GICC_CTLR_ENABLE_GRP1 | GICC_CTLR_ACK_GRP1,
    );
}

/// Group 0's enable bit, on both interfaces.
const GICD_CTLR_ENABLE_GRP0: u32 = 1 << 0;
/// Group 1's enable bit - the one an IRQ needs.
const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1;
/// The CPU interface's copy of the same pair.
const GICC_CTLR_ENABLE_GRP0: u32 = 1 << 0;
/// See [`GICD_CTLR_ENABLE_GRP1`].
const GICC_CTLR_ENABLE_GRP1: u32 = 1 << 1;
/// `AckCtl`: let an access in the secure state acknowledge a group 1 interrupt.
/// Without it the same interrupt is answered `1022` for as long as it is
/// asserted - see [`init`].
const GICC_CTLR_ACK_GRP1: u32 = 1 << 2;

/// Which interrupt was taken, or [`SPURIOUS`].
pub fn acknowledge() -> u32 {
    // The register also carries the acknowledging CPU in bits 10-12, which is
    // always this one on a uniprocessor.
    read(CPU, GICC_IAR) & 0x3ff
}

/// Retire an acknowledged interrupt so the controller may deliver the next.
///
/// Called after the handler has done its work, which for the timer means after
/// it has re-armed: a level-triggered line that is still asserted when this
/// runs is delivered again immediately.
pub fn end_of_interrupt(intid: u32) {
    write(CPU, GICC_EOIR, intid);
}

/// The registers a failed bring-up needs to see.
///
/// A timer that never fires has three quite different causes - the line is not
/// enabled, the controller is not delivering to this CPU, or the CPU has the
/// interrupt masked - and they are indistinguishable from the outside. These
/// five registers tell them apart: `dist_control` clears the first,
/// `highest_pending` the second (1023 means the controller has nothing for this
/// CPU while an INTID there means it has something the CPU is not taking), and
/// `DAIF` the third. Read fresh rather than cached, so they are the machine's
/// answer and not this module's intent.
pub struct State {
    pub dist_control: u32,
    pub enabled0: u32,
    pub cpu_control: u32,
    pub priority_mask: u32,
    pub highest_pending: u32,
}

/// See [`State`].
pub fn state() -> State {
    State {
        dist_control: read(DIST, GICD_CTLR),
        enabled0: read(DIST, GICD_ISENABLER),
        cpu_control: read(CPU, GICC_CTLR),
        priority_mask: read(CPU, GICC_PMR),
        // `GICC_HPPIR` sits between the ack and the end-of-interrupt registers.
        highest_pending: read(CPU, 0x0018) & 0x3ff,
    }
}
