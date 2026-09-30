//! AHCI/SATA: a real disk controller, beside the paravirtual one.
//!
//! Every block device this kernel has driven so far hangs off virtio, so the
//! storage path has only ever been exercised on a machine that already has the
//! paravirtualisation the rest of the kernel needs. This is the other half of
//! that sentence: a device with a register file of its own, DMA descriptors of
//! its own and failure modes of its own, on hardware QEMU presents with a single
//! `-device ich9-ahci`.
//!
//! What it is *not* is on any compositor's critical path - `PLAN.md` §7.3 is
//! explicit that this is the stretch item - and the scope says so: one
//! controller, one port, `IDENTIFY`, `READ DMA EXT`, `WRITE DMA EXT` and
//! `FLUSH CACHE EXT`. NCQ, ATAPI, port multipliers, hotplug and 28-bit LBA are
//! all absent, because nothing in this kernel asks for them.
//!
//! Three things about the shape are worth knowing before reading it.
//!
//! **The registers are not in the direct map.** BAR5 is a memory BAR, which is a
//! window onto the device rather than memory, so it is reached through
//! [`crate::mm::map_mmio`] - the same call `virtio`'s `Hal` makes for its own
//! BAR. Everything below reads and writes it with `read_volatile`/`write_volatile`,
//! because a device's registers are the one place the compiler may not assume
//! that two reads of the same address see the same thing.
//!
//! **One command at a time, through slot 0.** `PxCI` has a bit per command slot
//! and this driver ever sets one of them. A uniprocessor kernel with a
//! synchronous `BlockDev` behind it has nothing to overlap, and one slot means
//! one command-list entry to build rather than a free list to maintain.
//!
//! **Success is not "the command bit cleared".** The device retires that bit and
//! reports an ATA failure in `PxTFD` afterwards, so a driver that read the bit as
//! the answer would call a failed read a successful one and hand a filesystem
//! whatever the bounce page happened to hold - the silent success `AGENTS.md`
//! §4.2 puts above every other rule. [`AhciPort::issue`] checks both, and its
//! callers turn a failure into a panic carrying the command and the sector,
//! which is the block layer's own rule for a device error.
//!
//! The decisions in here - which class code is a SATA controller, where a 48-bit
//! LBA is split across the command FIS, which task-file bits mean failure - are
//! pinned in the booted machine by `t-ahci` rather than by a host test: every
//! function below is a volatile access to a register file that only exists on a
//! machine, which is the same trade `drivers/drm.rs` makes and §5's rule about
//! where a pin goes.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::blk::BlockDev;
use crate::mm::{PAGE_SIZE, alloc_frames, map_mmio, phys_to_virt};

/// The driver name `lspci -k` prints, and what a device claim records.
pub const PCI_DRIVER: &str = "ahci";

/// The class code of a SATA controller with an AHCI interface: mass storage
/// (01), SATA (06), AHCI 1.0 (01).
///
/// Matched as a class rather than as a vendor and device pair, because that is
/// what a storage controller has. `crate::pci::find` matches ids, and there is
/// no id worth naming that would also match the next chipset's.
const CLASS_SATA_AHCI: u32 = 0x01_06_01;

/// Generic host control, at the top of the register file.
const GHC: usize = 0x04;
/// `GHC.AE`: AHCI enable. Without it these registers are not the AHCI ones.
const GHC_AE: u32 = 1 << 31;
/// `GHC.HR`: the controller is resetting. Software sets it and waits for the
/// controller to clear it.
const GHC_HR: u32 = 1 << 0;
/// `CAP.PI`: the ports that are implemented, one bit each.
const CAP_PI: usize = 0x0c;

/// The first port's register block; port `n` is at `0x100 + n * 0x80`.
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;

// One port's registers, as offsets from its block.
const PX_CLB: usize = 0x00;
const PX_CLBU: usize = 0x04;
const PX_FB: usize = 0x08;
const PX_FBU: usize = 0x0c;
const PX_IS: usize = 0x10;
const PX_IE: usize = 0x14;
const PX_CMD: usize = 0x18;
const PX_TFD: usize = 0x20;
const PX_SIG: usize = 0x24;
const PX_SSTS: usize = 0x28;
const PX_CI: usize = 0x38;

/// `PxCMD.ST`/`.CR`: the command engine is running, and stopped, respectively.
const PX_CMD_ST: u32 = 1 << 0;
const PX_CMD_CR: u32 = 1 << 15;
/// `PxCMD.FRE`/`.FR`: the FIS receive engine, which has to be running before
/// the command engine.
const PX_CMD_FRE: u32 = 1 << 4;
const PX_CMD_FR: u32 = 1 << 14;

/// `PxSSTS.DET`: 3 is "device present and PHY communication established".
const SSTS_DET_MASK: u32 = 0x0f;
const SSTS_DET_PRESENT: u32 = 3;
/// `PxSIG` for an ATAPI device, which speaks a different command set than this
/// driver does.
///
/// The *only* signature this driver refuses, and it is deliberately not a test
/// for the ATA one: the controller this was measured against reports
/// `0xffffffff` on a port with a disk on it until the port has been brought up,
/// so a driver that waited for `0x00000101` would never bring up the port that
/// has the disk. Whether the device is an ATA disk is what `IDENTIFY` answers,
/// and that is where the discovery belongs; `PxSSTS.DET` is the presence
/// question, which is a different one.
const SIG_ATAPI: u32 = 0xeb14_0101;

/// `PxTFD`'s two error bits: `ERR` is an ATA error, `DF` a device fault.
const TFD_ERR: u32 = 1 << 0;
const TFD_DF: u32 = 1 << 5;
/// `PxIS.TFES`: the task-file error status bit that goes with them.
const PX_IS_TFES: u32 = 1 << 30;

/// The ATA commands this driver issues.
const CMD_IDENTIFY: u8 = 0xec;
const CMD_READ_DMA_EXT: u8 = 0x25;
const CMD_WRITE_DMA_EXT: u8 = 0x35;
const CMD_FLUSH_CACHE_EXT: u8 = 0xea;

/// The command list: 32 entries of 32 bytes, at the 1 KiB alignment the
/// controller requires.
const CMD_LIST_SIZE: usize = 1024;
/// The command table: a 64-byte command FIS, then the ATAPI command, then a
/// reserved gap, then the PRDT entries.
const CMD_TABLE_PRDT: usize = 0x80;
/// The received-FIS area, which wants 256-byte alignment.
const FIS_SIZE: usize = 256;

/// How long a bring-up step may take, in reads of the register it waits on.
///
/// A count of tries rather than a clock, because this runs while a device is
/// being brought up and a controller that never answers has to end in `None`
/// rather than in a boot that stops. The loop body is a volatile read of an MMIO
/// register, so this is tens of milliseconds of a healthy device and a bounded
/// wait on a dead one.
const WAIT_TRIES: usize = 5_000_000;

/// Read one device register. Volatile because the device changes its own
/// registers underneath the compiler, which must not fold two reads together.
fn rd(base: usize, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

fn wr(base: usize, offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

/// Spin until `done` holds, at most [`WAIT_TRIES`] times. `false` means it
/// never did.
fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..WAIT_TRIES {
        if done() {
            return true;
        }
    }
    false
}

/// One brought-up port, with the DMA structures behind it.
///
/// Every address here is a *physical* one: the device reads the command list and
/// the command table and moves the data, so what goes into `PxCLB` and into a
/// PRDT entry is what the controller's own bus sees. The kernel reaches the same
/// memory through the direct map.
struct AhciPort {
    /// The port's register block.
    regs: usize,
    /// The command list, 32 headers of 32 bytes.
    clb: usize,
    /// The command table the one header points at.
    ctba: usize,
    /// The data page, which is both the PRDT's target and the caller's bounce.
    bounce: usize,
}

impl AhciPort {
    /// Build and issue one command, and answer whether the *device* said it
    /// worked.
    ///
    /// `data` is `Some((physical, bytes, write))` for a command that moves
    /// sectors and `None` for one that does not. `bytes` is at most one page,
    /// which is what [`BlockDev`] promises per call and what one PRDT entry
    /// points at.
    fn issue(
        &self,
        command: u8,
        lba: u64,
        sectors: u16,
        data: Option<(usize, usize, bool)>,
    ) -> bool {
        let header = phys_to_virt(self.clb) as *mut u32;
        let table = phys_to_virt(self.ctba);

        // The command FIS: a 20-byte register host-to-device FIS, so `CFL` is
        // five dwords.
        //
        // The register layout for a 48-bit command is the one place this file is
        // easy to get subtly wrong: the address is split into *two* groups with
        // the device register between them - low three bytes, then the
        // device/head byte, then the high three - because that is where a 28-bit
        // command kept them and the 48-bit form kept the split.
        let fis = unsafe { core::slice::from_raw_parts_mut(table as *mut u8, 20) };
        fis.fill(0);
        fis[0] = 0x27; // register host-to-device
        fis[1] = 0x80; // C: this is a command, not a control word
        fis[2] = command;
        fis[4] = (lba & 0xff) as u8;
        fis[5] = ((lba >> 8) & 0xff) as u8;
        fis[6] = ((lba >> 16) & 0xff) as u8;
        // The device register: LBA mode, which the two commands that address
        // sectors need. `IDENTIFY` and `FLUSH` are sent with it clear, as the
        // ATA command set specifies.
        fis[7] = if matches!(command, CMD_READ_DMA_EXT | CMD_WRITE_DMA_EXT) {
            0x40
        } else {
            0
        };
        fis[8] = ((lba >> 24) & 0xff) as u8;
        fis[9] = ((lba >> 32) & 0xff) as u8;
        fis[10] = ((lba >> 40) & 0xff) as u8;
        fis[12] = (sectors & 0xff) as u8;
        fis[13] = ((sectors >> 8) & 0xff) as u8;

        let prdt_len = match data {
            Some((phys, bytes, _)) => {
                let entry = (table + CMD_TABLE_PRDT) as *mut u32;
                unsafe {
                    entry.write_volatile(phys as u32);
                    entry.add(1).write_volatile((phys >> 32) as u32);
                    entry.add(2).write_volatile(0);
                    // `DBC` is the byte count *minus one*, in 22 bits, with the
                    // interrupt bit above it. The device moves inside this run
                    // and no further, so a wrong count is a transfer that stops
                    // short rather than one that runs away.
                    entry
                        .add(3)
                        .write_volatile(((bytes - 1) as u32 & 0x003f_ffff) | (1 << 31));
                }
                1u32
            }
            None => 0,
        };

        // The command header, in slot 0: `CFL` in the low five bits, the write
        // bit next to it, `PRDTL` in the high half. The write bit is the one
        // field that decides whether a wrong buffer becomes a wrong *disk*.
        let write = data.is_some_and(|(_, _, write)| write);
        let dw0 = 5 | ((write as u32) << 6) | (prdt_len << 16);
        unsafe {
            header.write_volatile(dw0);
            header
                .add(1)
                .write_volatile(data.map_or(0, |(_, bytes, _)| bytes as u32));
            header.add(2).write_volatile(self.ctba as u32);
            header.add(3).write_volatile((self.ctba >> 32) as u32);
        }

        // Issue, and wait for the device to retire it. Slot 0 is the only bit
        // this driver ever sets.
        let slot = 1u32;
        wr(self.regs, PX_CI, slot);
        if !wait_until(|| rd(self.regs, PX_CI) & slot == 0) {
            return false;
        }
        // And then ask the task file, which is the actual answer: an ATA error
        // is reported *after* the command bit retires, so a driver that stopped
        // at the wait above would call a failed transfer a successful one.
        let tfd = rd(self.regs, PX_TFD);
        if tfd & (TFD_ERR | TFD_DF) != 0 || rd(self.regs, PX_IS) & PX_IS_TFES != 0 {
            // Clear the status, so the next command is not answered by this
            // one's error, and report the failure.
            wr(self.regs, PX_IS, PX_IS_TFES);
            return false;
        }
        true
    }

    /// Move whole sectors from the device into `buf`.
    fn read(&self, sector: u64, buf: &mut [u8]) {
        let sectors = (buf.len() / 512) as u16;
        if !self.issue(
            CMD_READ_DMA_EXT,
            sector,
            sectors,
            Some((self.bounce, buf.len(), false)),
        ) {
            panic!(
                "ahci: READ DMA EXT at sector {sector} ({sectors} sectors) failed: PxTFD {:#010x}",
                rd(self.regs, PX_TFD)
            );
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(self.bounce) as *const u8,
                buf.as_mut_ptr(),
                buf.len(),
            );
        }
    }

    /// Move whole sectors from `buf` to the device.
    fn write(&self, sector: u64, buf: &[u8]) {
        // The bytes go into the bounce page, which is what the PRDT points at.
        // The copy happens here, under the caller's lock, so no second command
        // can be building a transfer over the same page.
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                phys_to_virt(self.bounce) as *mut u8,
                buf.len(),
            );
        }
        let sectors = (buf.len() / 512) as u16;
        if !self.issue(
            CMD_WRITE_DMA_EXT,
            sector,
            sectors,
            Some((self.bounce, buf.len(), true)),
        ) {
            panic!(
                "ahci: WRITE DMA EXT at sector {sector} ({sectors} sectors) failed: PxTFD {:#010x}",
                rd(self.regs, PX_TFD)
            );
        }
    }

    /// `FLUSH CACHE EXT`.
    fn flush(&self) -> bool {
        self.issue(CMD_FLUSH_CACHE_EXT, 0, 0, None)
    }
}

/// One SATA disk, as the block layer sees it.
pub struct AhciDisk {
    port: Mutex<RefCell<AhciPort>>,
    sectors: u64,
}

impl BlockDev for AhciDisk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        check_span(buf.len());
        critical_section::with(|cs| self.port.borrow(cs).borrow().read(offset / 512, buf));
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        check_span(buf.len());
        critical_section::with(|cs| self.port.borrow(cs).borrow().write(offset / 512, buf));
    }

    /// `FLUSH CACHE EXT`, and `true` only when the device said it worked.
    ///
    /// A failure here is a panic rather than a `false`, and the distinction is
    /// `BlockDev::flush`'s own: `false` means "this device has no such concept,
    /// so keep the ordering you already had", which is not what a device that
    /// was asked and refused has told the caller.
    fn flush(&self) -> bool {
        critical_section::with(|cs| {
            let port = self.port.borrow(cs);
            let port = port.borrow();
            if !port.flush() {
                panic!(
                    "ahci: FLUSH CACHE EXT was refused: PxTFD {:#010x}",
                    rd(port.regs, PX_TFD)
                );
            }
            true
        })
    }
}

/// `BlockDev` promises at most one page per call, which is what one PRDT entry
/// and one bounce page can describe. More than that is the caller's contract
/// breaking, and the block layer's rule for a device error is a panic with
/// something in it worth reading rather than a quiet shortfall.
fn check_span(len: usize) {
    assert!(
        len <= PAGE_SIZE && len.is_multiple_of(512),
        "ahci: {len} bytes is not a whole number of sectors within one page"
    );
}

/// Probe the PCI bus for a SATA controller and bring up its first disk.
///
/// `None` is the ordinary answer on a machine with no such controller *and* the
/// answer for a controller with nothing on it, which the boot path treats the
/// same way: a machine that cannot read this disk boots without it.
pub fn init() -> Option<Arc<dyn BlockDev>> {
    // Every controller of this class, not just the first: a chipset's own SATA
    // controller and an added card are two devices with the same class code, and
    // which of them has a disk on it is not something the class can say. This
    // machine is exactly that - a `q35` chipset's AHCI at `00:1f.2` and the
    // `ich9-ahci` that carries the test disk at `00:03.0`.
    //
    // The first controller that answers with a disk wins, so a machine with one
    // disk on its second controller is a machine this driver reads.
    for device in crate::pci::devices()
        .into_iter()
        .filter(|device| device.class_code() == CLASS_SATA_AHCI)
    {
        if let Some(disk) = bring_up_controller(&device) {
            // The device is this driver's from here, which is what `lspci -k`
            // reads back. Recorded after the port and the identify both came up,
            // so a claim means a driver really bound rather than merely matched.
            crate::pci::claim(device.bus, device.dev, device.func, PCI_DRIVER);
            return Some(disk);
        }
    }
    None
}

/// Bring up one controller's first ATA disk, if it has one.
fn bring_up_controller(device: &crate::pci::Device) -> Option<Arc<dyn BlockDev>> {
    // Decode memory and let the controller reach main memory. Memory decoding
    // is not optional the way it is for a port-based driver: the register file
    // and every DMA structure are behind it.
    let func = crate::drivers::pci::Func::at(device.bus, device.dev, device.func);
    func.command_on(
        crate::drivers::pci::COMMAND_MEMORY_SPACE
            | crate::drivers::pci::COMMAND_BUS_MASTER
            | crate::drivers::pci::COMMAND_INTX_DISABLE,
    );

    // BAR5 is the ABAR. A controller whose BAR decodes nothing is one the
    // firmware did not assign, and this driver has no way to drive that.
    let bar = device.bars[5];
    if bar.base == 0 || bar.size == 0 {
        return None;
    }
    let base = map_mmio(bar.base as usize, bar.size as usize);

    // Into AHCI mode, and reset, so every port starts from one known state
    // whatever the firmware left behind.
    wr(base, GHC, rd(base, GHC) | GHC_AE);
    wr(base, GHC, rd(base, GHC) | GHC_HR);
    if !wait_until(|| rd(base, GHC) & GHC_HR == 0) {
        return None;
    }
    // A controller that reset itself clears `AE` along with everything else on
    // some silicon; setting it again is free, and the registers below are the
    // AHCI ones only while it is set.
    wr(base, GHC, rd(base, GHC) | GHC_AE);

    let implemented = rd(base, CAP_PI);
    for index in 0..32u32 {
        if implemented & (1 << index) == 0 {
            continue;
        }
        let regs = base + PORT_BASE + index as usize * PORT_STRIDE;
        // Presence first, and this is the whole of that question: `DET == 3`
        // is "device present and PHY communication established".
        if rd(regs, PX_SSTS) & SSTS_DET_MASK != SSTS_DET_PRESENT {
            continue;
        }
        // An ATAPI device is the one this driver cannot drive. What a *disk*
        // is, `IDENTIFY` says.
        if rd(regs, PX_SIG) == SIG_ATAPI {
            continue;
        }
        let Some(port) = bring_up(regs) else {
            continue;
        };
        let Some(sectors) = identify(&port) else {
            continue;
        };
        return Some(Arc::new(AhciDisk {
            port: Mutex::new(RefCell::new(port)),
            sectors,
        }));
    }
    None
}

/// Stop the port, point it at this driver's own memory, and start it again.
fn bring_up(regs: usize) -> Option<AhciPort> {
    // The command engine first, then the FIS engine: the other order leaves the
    // controller reading a list that is about to move.
    if rd(regs, PX_CMD) & PX_CMD_ST != 0 {
        wr(regs, PX_CMD, rd(regs, PX_CMD) & !PX_CMD_ST);
        if !wait_until(|| rd(regs, PX_CMD) & PX_CMD_CR == 0) {
            return None;
        }
    }
    if rd(regs, PX_CMD) & PX_CMD_FRE != 0 {
        wr(regs, PX_CMD, rd(regs, PX_CMD) & !PX_CMD_FRE);
        if !wait_until(|| rd(regs, PX_CMD) & PX_CMD_FR == 0) {
            return None;
        }
    }

    // Four frames, each one page and so aligned past every alignment the
    // controller asks for: the command list wants 1 KiB, the FIS area 256 bytes
    // and the command table 128. The fourth is the data page, which is also what
    // `BlockDev` promises a caller: one page, physically contiguous.
    let clb = alloc_frames(0)?;
    let fb = alloc_frames(0)?;
    let ctba = alloc_frames(0)?;
    let bounce = alloc_frames(0)?;
    for frame in [clb, fb, ctba, bounce] {
        unsafe {
            core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE);
        }
    }
    debug_assert!(clb % CMD_LIST_SIZE == 0 && fb % FIS_SIZE == 0);

    wr(regs, PX_CLB, clb as u32);
    wr(regs, PX_CLBU, (clb >> 32) as u32);
    wr(regs, PX_FB, fb as u32);
    wr(regs, PX_FBU, (fb >> 32) as u32);
    // A stale interrupt status from the firmware's own use would otherwise be
    // read as *this* driver's first failure. Nothing is enabled: this driver
    // polls, so `PxIE` stays clear and no line is ever asked for.
    wr(regs, PX_IS, 0xffff_ffff);
    wr(regs, PX_IE, 0);
    wr(regs, PX_CMD, rd(regs, PX_CMD) | PX_CMD_FRE);
    wr(regs, PX_CMD, rd(regs, PX_CMD) | PX_CMD_ST);

    Some(AhciPort {
        regs,
        clb,
        ctba,
        bounce,
    })
}

/// `IDENTIFY DEVICE`, from which the capacity comes.
///
/// This is the discovery as well as the check: a device that will not identify
/// itself is one this driver has nothing true to say about, so the answer is
/// `None` rather than a disk of unknown size.
fn identify(port: &AhciPort) -> Option<u64> {
    if !port.issue(CMD_IDENTIFY, 0, 1, Some((port.bounce, 512, false))) {
        return None;
    }
    let words = unsafe { core::slice::from_raw_parts(phys_to_virt(port.bounce) as *const u8, 512) };
    let word = |index: usize| u16::from_le_bytes([words[index * 2], words[index * 2 + 1]]);

    // Words 100..103 are the 48-bit capacity in sectors, little-endian by word;
    // words 60..61 hold the 28-bit answer for a device from before 48-bit
    // addressing. Zero in both is a device that has told us nothing usable.
    let mut lba48 = 0u64;
    for index in (100..104).rev() {
        lba48 = (lba48 << 16) | word(index) as u64;
    }
    if lba48 > 0 {
        return Some(lba48);
    }
    let lba28 = ((word(61) as u64) << 16) | word(60) as u64;
    if lba28 > 0 { Some(lba28) } else { None }
}
