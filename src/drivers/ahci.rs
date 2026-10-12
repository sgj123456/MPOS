//! AHCI/SATA: a real disk controller, beside the paravirtual one.
//!
//! Every block device this kernel has driven so far hangs off virtio, so the
//! storage path has only ever been exercised on a machine that already has the
//! paravirtualisation the rest of the kernel needs. This is the other half of
//! that sentence: a device with a register file of its own, DMA descriptors of
//! its own and failure modes of its own, on hardware QEMU presents with a single
//! `-device ich9-ahci`.
//!
//! What it is *not* is on any compositor's critical path - `NOTES.md` §7.3 is
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

use crate::ahci::{PortStatus, signature_is_atapi};
use crate::blk::BlockDev;
use crate::mm::{PAGE_SIZE, alloc_frames, free_frames, phys_to_virt};

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
// `PxTFD`'s two error bits and `PxIS`'s task-file-error latch live in
// [`crate::ahci`] with the decision they feed, because the decision is what was
// wrong here and only a host build can test it.

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
/// A sector, which is the only amount `BlockDev` moves and so the only thing a
/// request's length has to be a multiple of.
const SECTOR: usize = 512;

/// How many pages one command describes, which is how many the port holds: the
/// bounce is a *list* of pages rather than one big allocation because a PRDT is a
/// scatter-gather table - each entry points wherever it likes - and asking the
/// buddy allocator for 32 contiguous pages would make this driver's I/O fail on
/// a machine that has the memory but not the run.
///
/// The size is a bound on one command, not on a request: a longer request is more
/// commands (see [`crate::ahci::chunks`]), so what a caller can steer is the
/// number of commands it causes and not the memory this driver holds, which is
/// fixed here and allocated once per port. That is the worst-case rule's shape
/// for a device driver: the buffer is the driver's, and a user's read of any size
/// reaches the disk through it.
const BOUNCE_PAGES: usize = 32;
/// One command's transfer, and so the plan's piece size.
const MAX_TRANSFER: usize = BOUNCE_PAGES * PAGE_SIZE;

// A table longer than the command page can hold would be read past its end by
// the device, and `DBC` counts 22 bits, so a page-sized run is always codable.
const _: () = assert!(BOUNCE_PAGES <= crate::ahci::prdt_capacity(PAGE_SIZE, CMD_TABLE_PRDT));

/// How long a bring-up step may take, in reads of the register it waits on.
///
/// A count rather than a clock, and not for the reason a first reading guesses:
/// the same wait runs while a mounted filesystem is reading through
/// [`BlockDev`], which holds the port inside `critical_section::with`, and an
/// MMIO read inside a critical section happens with interrupts masked - so the
/// 250 Hz tick counter does not advance, and a deadline in ticks would never
/// expire. Reads advance whatever else does.
///
/// What a count *does* depend on is the cost of one read, and that is measured
/// rather than assumed: 5,000,000 reads of a dead port on this machine's
/// emulated controller is **28.3 s** of the boot (7,067 ticks at 250 Hz, so
/// ~5.7 µs per exit). The budget stays generous because a disk that is
/// legitimately spinning up is what it is there for - a healthy command retires
/// at try 0 to 2, measured on a `q35` with a real device - but the number is no
/// longer the whole of the wait's bound: [`crate::ahci::PortStatus::waiting`]
/// stops the same wait as soon as the port reports the command failed, which is
/// what a controller with nothing behind a "present" port actually does.
const WAIT_TRIES: usize = 5_000_000;

/// Read one device register. Volatile because the device changes its own
/// registers underneath the compiler, which must not fold two reads together.
fn rd(base: usize, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

fn wr(base: usize, offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

/// Spin until `done` holds, at most [`WAIT_TRIES`] reads. `false` means it never
/// did: the budget ran out, which is a device that is not answering rather than
/// one that has answered no - the caller still reads the status afterwards, so a
/// refusal says which of the two it was.
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
    /// The received-FIS area. Programmed into `PxFB` and kept here only so
    /// [`release`] can hand it back with the rest.
    fb: usize,
    /// The command table the one header points at.
    ctba: usize,
    /// The data pages, which are both the PRDT's targets and the caller's
    /// bounce: a command fills as many of them, from the front, as its transfer
    /// needs, and the copy to or from the caller's buffer happens here, under the
    /// port's lock, so no second command can be building a transfer over pages
    /// the first one still owns.
    bounce: [usize; BOUNCE_PAGES],
}

impl AhciPort {
    /// Build and issue one command, and answer whether the *device* said it
    /// worked.
    ///
    /// `transfer` is `Some((bytes, write))` for a command that moves sectors and
    /// `None` for one that does not. `bytes` is at most [`MAX_TRANSFER`], and it
    /// is described by **one PRDT entry per bounce page**: the device fills the
    /// runs in order and `PRDTL` says how many there are, so the request's length
    /// decides the table's length. That is what a PRDT is for - the runs need not
    /// be contiguous, and this driver's are not, because they are individually
    /// allocated pages.
    ///
    /// The two fields a wrong table breaks on are both here rather than inline:
    /// `DBC` per run (see [`crate::ahci::prdt_flags`], which says out loud that
    /// only the *last* run carries the completion bit) and `PRDBC`, the total,
    /// which is what the device checks its own transfer against.
    fn issue(&self, command: u8, lba: u64, sectors: u16, transfer: Option<(usize, bool)>) -> bool {
        let header = phys_to_virt(self.clb) as *mut u32;
        let table = phys_to_virt(self.ctba);
        let (bytes, write) = transfer.unwrap_or((0, false));
        let entries = bytes.div_ceil(PAGE_SIZE);

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

        // Loud rather than a shorter table: with a debug_assert only, a release
        // build would write `PRDTL` for a table it had not filled, and the device
        // would DMA against descriptors that are someone else's memory. The
        // callers chunk every request to this bound, so arriving here means one
        // of them stopped doing that.
        assert!(
            bytes <= MAX_TRANSFER,
            "ahci: a {bytes}-byte command is longer than the port's {}-byte table",
            MAX_TRANSFER
        );
        for (index, frame) in self.bounce.iter().enumerate().take(entries) {
            let run = crate::ahci::run_bytes(bytes, index, PAGE_SIZE);
            let flags = match crate::ahci::prdt_flags(run, index + 1 == entries) {
                Some(flags) => flags,
                // Unreachable while `MAX_TRANSFER` is the bound on `bytes`, and a
                // panic rather than a skipped entry if that ever stops being
                // true: a run whose length the table cannot say is a transfer
                // that stops short while the command reports success.
                None => panic!("ahci: a {run}-byte PRDT run has no encoding"),
            };
            let entry = (table + CMD_TABLE_PRDT) as *mut u32;
            unsafe {
                let entry = entry.wrapping_add(index * 4);
                entry.write_volatile(*frame as u32);
                entry.add(1).write_volatile((*frame >> 32) as u32);
                entry.add(2).write_volatile(0);
                entry.add(3).write_volatile(flags);
            }
        }

        // The command header, in slot 0: `CFL` in the low five bits, the write
        // bit next to it, `PRDTL` in the high half; the next word is `PRDBC`, the
        // total. The write bit is the one field that decides whether a wrong
        // buffer becomes a wrong *disk*.
        let dw0 = 5 | ((write as u32) << 6) | ((entries as u32) << 16);
        unsafe {
            header.write_volatile(dw0);
            header.add(1).write_volatile(bytes as u32);
            header.add(2).write_volatile(self.ctba as u32);
            header.add(3).write_volatile((self.ctba >> 32) as u32);
        }

        // Issue, and wait for the port to answer. Slot 0 is the only bit this
        // driver ever sets.
        let slot = 1u32;
        wr(self.regs, PX_CI, slot);
        // "Answered" is two registers wide: the slot's retirement is the
        // command finishing, and the task-file error latch is the device
        // refusing it. The wait below used to watch the first alone, so a port
        // that latches the error and never clears the slot - which is exactly
        // what a controller's "device present" port with nothing behind it does
        // on this machine - was polled five million times, 28.3 s of the boot,
        // before the error that had been there from the first read was read.
        let answered = wait_until(|| !self.status(slot).waiting());
        let status = self.status(slot);
        // And the task file is the actual answer either way: an ATA error is
        // reported *after* the command bit retires, so a driver that stopped at
        // the wait above would call a failed transfer a successful one.
        if !answered || status.failed() {
            // Writing the latched bits back is how `PxIS` clears them; ask for
            // the answer again on the next command rather than reading this
            // one's failure a second time.
            wr(self.regs, PX_IS, status.is);
            return false;
        }
        true
    }

    /// The port's three status words, read as the one question [`issue`] asks.
    fn status(&self, slot: u32) -> PortStatus {
        PortStatus {
            ci: rd(self.regs, PX_CI),
            is: rd(self.regs, PX_IS),
            tfd: rd(self.regs, PX_TFD),
            slot,
        }
    }

    /// Move whole sectors from the device into `buf`.
    ///
    /// The request is planned into commands of at most [`MAX_TRANSFER`] by
    /// [`crate::ahci::chunks`], and each command fills the bounce and is copied
    /// out before the next one starts - the pages are the driver's, fixed at
    /// bring-up, so the number of commands a caller causes is the only thing a
    /// caller steers (the worst-case rule).
    fn read(&self, sector: u64, buf: &mut [u8]) {
        let mut done = 0usize;
        for take in crate::ahci::chunks(buf.len(), MAX_TRANSFER) {
            if !self.issue(
                CMD_READ_DMA_EXT,
                sector + (done / SECTOR) as u64,
                (take / SECTOR) as u16,
                Some((take, false)),
            ) {
                self.refused("READ DMA EXT", sector + (done / SECTOR) as u64, take);
            }
            self.copy_from_bounce(done, take, buf);
            done += take;
        }
    }

    /// Move whole sectors from `buf` to the device. The mirror of
    /// [`Self::read`], with the bytes staged into the bounce pages first, under
    /// the caller's lock, so no second command can be building a transfer over
    /// pages the first one still owns.
    fn write(&self, sector: u64, buf: &[u8]) {
        let mut done = 0usize;
        for take in crate::ahci::chunks(buf.len(), MAX_TRANSFER) {
            self.copy_to_bounce(done, take, buf);
            if !self.issue(
                CMD_WRITE_DMA_EXT,
                sector + (done / SECTOR) as u64,
                (take / SECTOR) as u16,
                Some((take, true)),
            ) {
                self.refused("WRITE DMA EXT", sector + (done / SECTOR) as u64, take);
            }
            done += take;
        }
    }

    /// The block layer's rule for a device error: a panic with something in it
    /// worth reading, naming the command, the sector it was at and the length it
    /// had left.
    #[cold]
    fn refused(&self, command: &str, sector: u64, bytes: usize) -> ! {
        panic!(
            "ahci: {command} at sector {sector} ({}/512 bytes) failed: PxTFD {:#010x}",
            bytes,
            rd(self.regs, PX_TFD)
        )
    }

    /// Copy the first `bytes` of the bounce into `buf[at..]`, run by run: the
    /// pages are individually allocated and so are not contiguous, which is the
    /// whole reason the table exists.
    fn copy_from_bounce(&self, at: usize, bytes: usize, buf: &mut [u8]) {
        for index in 0..bytes.div_ceil(PAGE_SIZE) {
            let run = crate::ahci::run_bytes(bytes, index, PAGE_SIZE);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(self.bounce[index]) as *const u8,
                    buf[at + index * PAGE_SIZE..].as_mut_ptr(),
                    run,
                );
            }
        }
    }

    /// The other direction of [`Self::copy_from_bounce`].
    fn copy_to_bounce(&self, at: usize, bytes: usize, buf: &[u8]) {
        for index in 0..bytes.div_ceil(PAGE_SIZE) {
            let run = crate::ahci::run_bytes(bytes, index, PAGE_SIZE);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf[at + index * PAGE_SIZE..].as_ptr(),
                    phys_to_virt(self.bounce[index]) as *mut u8,
                    run,
                );
            }
        }
    }

    /// `FLUSH CACHE EXT`.
    fn flush(&self) -> bool {
        self.issue(CMD_FLUSH_CACHE_EXT, 0, 0, None)
    }

    /// The answer `IDENTIFY` wrote into the first bounce page.
    fn identify_words(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(phys_to_virt(self.bounce[0]) as *const u8, SECTOR) }
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
        check_span(offset, buf.len());
        critical_section::with(|cs| {
            self.port
                .borrow(cs)
                .borrow()
                .read(offset / SECTOR as u64, buf)
        });
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        check_span(offset, buf.len());
        critical_section::with(|cs| {
            self.port
                .borrow(cs)
                .borrow()
                .write(offset / SECTOR as u64, buf)
        });
    }

    /// One command's transfer, which is what a caller may ask for in one call.
    ///
    /// [`MAX_TRANSFER`] is the bounce's own bound and not a separate figure: the
    /// port holds [`BOUNCE_PAGES`] pages, `issue` describes one command with one
    /// PRDT entry per page, and a longer request is `chunks`' business of being
    /// more commands - so a request this wide is exactly one command, which is
    /// what the trait's promise is about. The block cache asks it to decide how
    /// wide a run to load in one call, and with this method absent the disk
    /// answers the page default: `t-diskbench`'s cold pass over a 64 MiB file
    /// then costs **16387** device calls (one per block, measured on the committed
    /// tree at `3be89aa`) instead of 1027, and its three run claims fail. The
    /// method was named - and answered - by the change that added the run, whose
    /// commit lost this hunk; the red run above is where its absence was found.
    fn max_transfer(&self) -> usize {
        MAX_TRANSFER
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

/// What a request must be to reach this device at all: whole sectors from a
/// sector boundary, in either direction.
///
/// That is the whole of it now. The length is not a limit - a request the device
/// can hold is a longer PRDT and, past [`MAX_TRANSFER`], more commands - and the
/// offset is checked here rather than trusted from the caller because `LBA` and
/// `count` are the only address a DMA command carries: an offset inside a sector
/// has no encoding, and the failure would be a transfer of the wrong length
/// rather than a refusal. virtio-blk asserts the same pair, and the two drivers
/// answering the same question the same way is the point.
fn check_span(offset: u64, len: usize) {
    assert!(
        offset.is_multiple_of(SECTOR as u64) && len.is_multiple_of(SECTOR),
        "ahci: {len} bytes at {offset:#x} is not a whole number of sectors from a sector boundary"
    );
}

/// Give back what [`bring_up`] took, and only after the port has said it stopped.
///
/// `false` means the pages stay allocated and unreferenced, which is the point:
/// a controller whose engine did not come down still holds `PxCLB` and `PxFB`,
/// and the frame behind those is the command list and the FIS area *it is
/// writing to*. Handing that to the allocator does not fail there - it fails
/// whenever the buddy system gives the page to someone else, which is a corrupt
/// page cache entry or a process's data, three subsystems and a minute away from
/// this function. A leaked page is a smaller wrong than that, and a bounded one:
/// one port's worth per boot, on the machine that has a "present" port with
/// nothing behind it - which the `q35` chipset's own SATA controller does, and
/// whose stop the measurement says *does* come down (`PxCMD` quiet, `PxCI` empty),
/// so the leak is the branch and not the rule.
fn release(port: &AhciPort) {
    if stop(port.regs) {
        for frame in [port.clb, port.fb, port.ctba]
            .into_iter()
            .chain(port.bounce)
        {
            free_frames(frame, 0);
        }
    }
}

/// Clear `ST` and `FRE` and wait for the command engine and the FIS receiver to
/// say they stopped. `false` is the same answer as everywhere else in this file:
/// the port was asked and did not comply, and the caller has nothing truer to do
/// than move on.
///
/// The command engine first and the FIS engine after it, in both directions: the
/// other order leaves the controller reading a list that is about to move.
fn stop(regs: usize) -> bool {
    let cmd = rd(regs, PX_CMD);
    if cmd & PX_CMD_ST != 0 {
        wr(regs, PX_CMD, cmd & !PX_CMD_ST);
        if !wait_until(|| rd(regs, PX_CMD) & PX_CMD_CR == 0) {
            return false;
        }
    }
    let cmd = rd(regs, PX_CMD);
    if cmd & PX_CMD_FRE != 0 {
        wr(regs, PX_CMD, cmd & !PX_CMD_FRE);
        return wait_until(|| rd(regs, PX_CMD) & PX_CMD_FR == 0);
    }
    true
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
    // The BAR's registers, reached through the seam: which machine answers this
    // and how is not this driver's question (`NOTES.md` §14.3).
    let base = crate::hal::map_mmio_window(bar.base as usize, bar.size as usize)?;

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
        if signature_is_atapi(rd(regs, PX_SIG)) {
            continue;
        }
        let Some(port) = bring_up(regs) else {
            continue;
        };
        let Some(sectors) = identify(&port) else {
            // A port that claims a device and will not identify one - which is
            // what this machine's built-in controller's port 2 does, and used to
            // cost 28 s of the boot before `crate::ahci` started reading the
            // error latch. Quiet it and take the memory back before the next
            // port, so trying every candidate costs nothing per failure.
            release(&port);
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
    if !stop(regs) {
        return None;
    }

    // The DMA memory: a command list, a received-FIS area, one command table and
    // the bounce - `BOUNCE_PAGES` pages, which are the runs one command's table
    // can describe. Every one of them is a single frame, and that is the whole
    // of the alignment story: the command list wants 1 KiB, the FIS area 256
    // bytes and the command table 128, and a page clears all three without
    // asking the allocator for a contiguous run it may not have. The bounce pages
    // do not need to be contiguous with each other either, because `PRDT` is a
    // list - see [`AhciPort::issue`].
    let mut frames = [0usize; 3 + BOUNCE_PAGES];
    let mut taken = 0usize;
    for slot in frames.iter_mut() {
        match alloc_frames(0) {
            Some(frame) => {
                *slot = frame;
                taken += 1;
            }
            None => {
                // Hand back what this port got before giving up on it. A machine
                // whose controller reports a present port with nothing behind it
                // walks this path on every boot, and a bring-up that leaves its
                // memory behind when it fails is a boot that loses a little more
                // each time.
                for frame in frames[..taken].iter() {
                    free_frames(*frame, 0);
                }
                return None;
            }
        }
    }
    let (clb, fb, ctba) = (frames[0], frames[1], frames[2]);
    let mut bounce = [0usize; BOUNCE_PAGES];
    bounce.copy_from_slice(&frames[3..]);
    for frame in frames {
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
        fb,
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
    // 512 bytes: the identity page, which is one run of the table.
    if !port.issue(CMD_IDENTIFY, 0, 1, Some((SECTOR, false))) {
        return None;
    }
    let words = port.identify_words();
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
