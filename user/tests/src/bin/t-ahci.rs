//! t-ahci - a real SATA disk, read and written through the AHCI driver, on both
//! machines.
//!
//! Every other block device this kernel drives arrives through virtio, so the
//! storage path only runs on a machine that already has the paravirtualisation
//! the rest of the kernel needs. The AHCI driver (`src/drivers/ahci.rs`) is the
//! other kind: a register file reached through a memory BAR, a command list and
//! a PRDT the controller reads by physical address, and a task file that reports
//! failure *after* the command has retired. Every line of it is a volatile
//! access to hardware, so no host test can reach any of it - this program is the
//! pin, which is what `NOTES.md` §7.3 says it would be.
//!
//! What it checks is the part a wrong driver cannot fake. A driver with a wrong
//! command FIS reads the wrong sector; one with a wrong PRDT moves the wrong
//! number of bytes; one that mistook "the command bit cleared" for success
//! would hand back whatever the bounce page held. So: the disk is *named*
//! (`/dev/sda`, major 8, and not `vda`), its capacity is what the reads can
//! actually reach, and a sector written with a pattern reads back with that
//! pattern - which is the one thing that cannot happen by accident.
//!
//! # The migration
//!
//! The no-std original could only run on the first machine, because the second
//! had no PCI enumeration at all and nowhere to put a controller's BAR - the
//! three pieces that closed that (§15.11's batch-40 entry: ECAM read from the
//! device tree, a dynamic MMIO window, and the port pair becoming the x86_64
//! arm of `pci`). Both wiring arms attach a controller now, unconditionally and
//! from the same image file (`disk::ensure_sata_image`), so the std side carries
//! the program and both sweeps run it. Nothing about a claim moved.
//!
//! The doors: `rustix::fs::openat` for the disk, `pread`/`pwrite` from
//! `rustix::io` for every transfer, and `/proc/partitions` through plain
//! `std::fs`. The buffers are heap vectors rather than the no-std file's two
//! statics: the sizes were load-bearing there only because a
//! `no_std` program has no allocator story of its own, and here 16 KiB and 1 MiB
//! are just allocations - the claims are about what the *device* answers at each
//! length, not about where the caller's bytes live.
//!
//! **Not run on the host**, and the reason is batch 37's again: a hosted run
//! would read and write a real SATA disk on the machine hosting this sandbox,
//! which is not what the pin is about. `t-fbdev`'s rule at the open: only a
//! hosted run forgives the node that is missing or refuses it, and under a
//! payload sweep - where both wiring arms attach the disk unconditionally from
//! one image file (`disk::ensure_sata_image`) - the open failing is the check
//! itself. Its validation is the two sweeps.

use std::fs;

use mpos_user_tests::{Report, host_run};
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{pread, pwrite};

/// The test machine's SATA disk is this many bytes (`xtask/src/disk.rs`'s
/// `SATA_TEST_BYTES`), so this many sectors. Written down here on purpose: a
/// capacity that is *reported* and a capacity that is *reachable* are two
/// different claims, and the whole point of checking both is that they can
/// disagree.
///
/// 96 MiB, not the 4 MiB this file first named: §10.5's disk-backed instrument
/// needed a disk a filesystem fits on, so the backing file grew and this pin
/// moved with it. The *scratch* region below (sectors 4000 and 6000) sits
/// inside what that filesystem occupies — harmless because the programs that
/// read it lay their own copy first, through the block door, and the fence
/// between the two is the sweep's: the disk is laid fresh before every run
/// (`disk::reset_sata_image`), so nothing a run writes can be read by the next.
const SATA_BYTES: u64 = 96 * 1024 * 1024;
const SATA_SECTORS: u64 = SATA_BYTES / 512;

/// A 16 KiB pattern and a 1 MiB answer - the widths the request-shape claims
/// are made of, see above for why they are heap here and statics there.
const PATTERN_BYTES: usize = 16 * 1024;
const BIG_BYTES: usize = 1024 * 1024;

/// The fingerprint of the wide write: a function of the byte's index rather
/// than a repeated block, because the failure this pins is a table that says
/// "done" at the end of the first run - a uniform pattern would read back
/// perfect with three quarters of the buffer unwritten.
fn fingerprint(index: usize) -> u8 {
    let mixed = (index as u32).wrapping_mul(0x9E37_79B9) ^ (index >> 3) as u32;
    (mixed >> 11) as u8
}

/// Whether `hay` contains `needle`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|window| window == needle)
}

fn main() {
    println!("t-ahci");
    let mut r = Report::new();

    // --- the disk has a name, and it is its own ---
    //
    // Not `/dev/vda`: a SATA disk is `sda` under major 8, and a kernel that
    // called it `vda` would be claiming it came from a driver it did not.
    let disk = openat(CWD, "/dev/sda", OFlags::RDWR, Mode::empty());
    let disk = match disk {
        Err(e) => {
            // Under a payload sweep the disk missing is a failed check, not a
            // skip: both wiring arms attach it unconditionally from one image
            // file (`disk::ensure_sata_image`). On the host there may be no
            // SATA disk at all, so only a hosted run forgives the open -
            // `t-fbdev`'s rule for a device node the host lacks.
            r.check(&format!("/dev/sda opens ({e:?})"), host_run());
            r.finish();
        }
        Ok(disk) => {
            r.check("/dev/sda opens", true);
            disk
        }
    };

    // The second reader is `/proc/partitions`, which is where a partitioner
    // reads the numbers before it touches a sector.
    let listing = fs::read("/proc/partitions");
    r.check("/proc/partitions opens", listing.is_ok());
    let listing = listing.unwrap_or_default();
    r.check("and names the SATA disk", contains(&listing, b"sda"));
    r.check(
        "under major 8, which is how userland tells it from the virtio disk",
        contains(&listing, b"   8    0 "),
    );
    r.check(
        "and gives its size, in the column a partitioner reads",
        contains(&listing, b" 98304 sda"),
    );

    // --- the capacity IDENTIFY reported is the capacity that is readable ---
    let mut sector = [0u8; 512];
    r.check(
        "the first sector reads",
        pread(&disk, &mut sector, 0) == Ok(512),
    );
    r.check(
        "and is zero, because the test image is made of zeros",
        sector.iter().all(|&byte| byte == 0),
    );
    r.check(
        "the last sector IDENTIFY promised is there",
        pread(&disk, &mut sector, (SATA_SECTORS - 1) * 512) == Ok(512),
    );
    // And one past it is *not*: a driver that reported a capacity the device
    // cannot reach would answer here instead of stopping, which is the failure
    // a wrong `PxCLB`/PRDT setup really has.
    r.check(
        "and one past the end is an empty read rather than a lie",
        pread(&disk, &mut sector, SATA_SECTORS * 512) == Ok(0),
    );

    // --- a write lands where it was put, and only there ---
    //
    // This is the check nothing can pass by accident. The pattern is not
    // something the image builder wrote and not something a previous run left:
    // it is written here and read back through a second command, so a driver
    // that built the command FIS with the LBA split in the wrong places would
    // read a zero sector instead.
    let target_sector = 4000u64;
    let mut written = [0u8; 512];
    for (index, byte) in written.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(0x5a);
    }
    r.check(
        "a sector is written",
        pwrite(&disk, &written, target_sector * 512) == Ok(512),
    );
    let mut read_back = [0u8; 512];
    r.check(
        "and reads back",
        pread(&disk, &mut read_back, target_sector * 512) == Ok(512),
    );
    r.check(
        "with the bytes that were written to it",
        read_back == written,
    );
    // The neighbour is untouched, which is what says the LBA in the command FIS
    // was the one that was asked for rather than one block either side of it.
    r.check(
        "while the sector beside it is left alone",
        pread(&disk, &mut read_back, (target_sector - 1) * 512) == Ok(512),
    );
    r.check("and is still zero", read_back.iter().all(|&byte| byte == 0));

    // --- a request longer than one page is one request, not a panic ---
    //
    // This is the shape that ended the machine: the block node reads serve the
    // caller's whole buffer to the driver in one call (`vfs/devfs.rs` sizes a
    // sector window around the request), and the SATA driver used to assert that
    // nothing wider than its own single bounce page could reach it. `udevd`
    // asking a new disk what is on it - a 262144-byte read, measured - was
    // therefore enough. A PRDT is a *list*, so the driver now describes a long
    // request with one entry per page and, past what one command can carry, with
    // more commands: `BlockDev`'s contract is whole sectors, not one page, and
    // virtio-blk has never accepted anything less than that.
    let pattern: Vec<u8> = (0..PATTERN_BYTES).map(fingerprint).collect();
    // Sector 6000: past the single-sector test above, and far enough inside the
    // disk that the 16 KiB write and the 1 MiB read below still fit.
    let wide = 6000u64 * 512;
    r.check(
        "a 16 KiB write is one request",
        pwrite(&disk, &pattern, wide) == Ok(PATTERN_BYTES),
    );
    let mut back = vec![0u8; PATTERN_BYTES];
    r.check(
        "and a 16 KiB read is one request",
        pread(&disk, &mut back, wide) == Ok(PATTERN_BYTES),
    );
    r.check(
        "every page of the 16 KiB came back, not just the first",
        back == pattern,
    );

    // And longer still: 1 MiB is `MAX_IO`, the widest read this kernel's ABI
    // carries at all, which is eight commands' worth of the driver's own bound.
    // The pattern sits 512 bytes into it, so the check is that a request spanning
    // commands both finds its bytes and puts them where the *offset* says - a
    // chunking loop that forgot to advance the LBA would read the same first
    // 128 KiB eight times and still return a full buffer.
    let mut big = vec![0u8; BIG_BYTES];
    r.check(
        "a 1 MiB read returns 1 MiB",
        pread(&disk, &mut big, wide - 512) == Ok(BIG_BYTES),
    );
    r.check(
        "and the pattern is inside it at the offset it was written at",
        big[512..512 + PATTERN_BYTES]
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == fingerprint(index)),
    );
    // The neighbour check again at the other scale: the sector before the wide
    // write is still the untouched one the single-sector test left behind it.
    r.check(
        "and the sector before it was not written by the long request",
        big[..512].iter().all(|&byte| byte == 0),
    );

    drop(disk);
    r.finish();
}
