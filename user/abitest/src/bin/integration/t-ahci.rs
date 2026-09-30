//! t-ahci - a real SATA disk, read and written through the AHCI driver.
//!
//! Every other block device this kernel drives arrives through virtio, so the
//! storage path only runs on a machine that already has the paravirtualisation
//! the rest of the kernel needs. The AHCI driver (`src/drivers/ahci.rs`) is the
//! other kind: a register file reached through a memory BAR, a command list and
//! a PRDT the controller reads by physical address, and a task file that reports
//! failure *after* the command has retired. Every line of it is a volatile
//! access to hardware, so no host test can reach any of it - this program is the
//! pin, which is what `PLAN.md` §7.3 says it would be.
//!
//! What it checks is the part a wrong driver cannot fake. A driver with a wrong
//! command FIS reads the wrong sector; one with a wrong PRDT moves the wrong
//! number of bytes; one that mistook "the command bit cleared" for success
//! would hand back whatever the bounce page held. So: the disk is *named*
//! (`/dev/sda`, major 8, and not `vda`), its capacity is what the reads can
//! actually reach, and a sector written with a pattern reads back with that
//! pattern - which is the one thing that cannot happen by accident.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The test machine's SATA disk is this many bytes (`xtask/src/selftest.rs`),
/// so this many sectors. Written down here on purpose: a capacity that is
/// *reported* and a capacity that is *reachable* are two different claims, and
/// the whole point of checking both is that they can disagree.
const SATA_BYTES: u64 = 4 * 1024 * 1024;
const SATA_SECTORS: u64 = SATA_BYTES / 512;

const O_RDWR: usize = 2;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-ahci\n");
    let mut failed = false;

    // --- the disk has a name, and it is its own ---
    //
    // Not `/dev/vda`: a SATA disk is `sda` under major 8, and a kernel that
    // called it `vda` would be claiming it came from a driver it did not.
    let fd = lin::openat(lin::AT_FDCWD, b"/dev/sda\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/sda opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // The second reader is `/proc/partitions`, which is where a partitioner
    // reads the numbers before it touches a sector.
    let listing_fd = lin::openat(lin::AT_FDCWD, b"/proc/partitions\0", 0, 0) as i32;
    lin::check_true(&mut failed, b"/proc/partitions opens", listing_fd >= 0);
    let mut listing = [0u8; 4096];
    let n = if listing_fd >= 0 {
        let n = lin::read(listing_fd, &mut listing).max(0) as usize;
        lin::close(listing_fd);
        n
    } else {
        0
    };
    let listing = &listing[..n];
    lin::check_true(
        &mut failed,
        b"and names the SATA disk",
        contains(listing, b"sda"),
    );
    lin::check_true(
        &mut failed,
        b"under major 8, which is how userland tells it from the virtio disk",
        contains(listing, b"   8    0 "),
    );
    lin::check_true(
        &mut failed,
        b"and gives its size, in the column a partitioner reads",
        contains(listing, b" 4096 sda"),
    );

    // --- the capacity IDENTIFY reported is the capacity that is readable ---
    let mut sector = [0u8; 512];
    lin::check_eq(
        &mut failed,
        b"the first sector reads",
        lin::pread(fd, &mut sector, 0),
        512,
    );
    lin::check_true(
        &mut failed,
        b"and is zero, because the test image is made of zeros",
        sector.iter().all(|&byte| byte == 0),
    );
    lin::check_eq(
        &mut failed,
        b"the last sector IDENTIFY promised is there",
        lin::pread(fd, &mut sector, ((SATA_SECTORS - 1) * 512) as i64),
        512,
    );
    // And one past it is *not*: a driver that reported a capacity the device
    // cannot reach would answer here instead of stopping, which is the failure
    // a wrong `PxCLB`/PRDT setup really has.
    lin::check_eq(
        &mut failed,
        b"and one past the end is an empty read rather than a lie",
        lin::pread(fd, &mut sector, (SATA_SECTORS * 512) as i64),
        0,
    );

    // --- a write lands where it was put, and only there ---
    //
    // This is the check nothing can pass by accident. The pattern is not
    // something the image builder wrote and not something a previous run left:
    // it is written here and read back through a second command, so a driver
    // that built the command FIS with the LBA split in the wrong places would
    // read a zero sector instead.
    let target = 4000u64;
    let mut written = [0u8; 512];
    for (index, byte) in written.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(0x5a);
    }
    lin::check_eq(
        &mut failed,
        b"a sector is written",
        lin::pwrite(fd, &written, target as i64 * 512),
        512,
    );
    let mut read_back = [0u8; 512];
    lin::check_eq(
        &mut failed,
        b"and reads back",
        lin::pread(fd, &mut read_back, target as i64 * 512),
        512,
    );
    lin::check_true(
        &mut failed,
        b"with the bytes that were written to it",
        read_back == written,
    );
    // The neighbour is untouched, which is what says the LBA in the command FIS
    // was the one that was asked for rather than one block either side of it.
    lin::check_eq(
        &mut failed,
        b"while the sector beside it is left alone",
        lin::pread(fd, &mut read_back, ((target - 1) * 512) as i64),
        512,
    );
    lin::check_true(
        &mut failed,
        b"and is still zero",
        read_back.iter().all(|&byte| byte == 0),
    );

    // --- and the sectors that fit in one command are not a limit on the file ---
    //
    // `BlockDev` moves at most one page per call and the driver bounces through
    // it; a read that spans several calls is the filesystem layer's own path,
    // so it is worth knowing that two sectors in one call arrive whole and in
    // order.
    let mut pair = [0u8; 1024];
    lin::check_eq(
        &mut failed,
        b"two sectors in one read arrive whole",
        lin::pread(fd, &mut pair, target as i64 * 512),
        1024,
    );
    lin::check_true(
        &mut failed,
        b"the first of them the pattern and the second the neighbour",
        pair[..512] == written && pair[512..].iter().all(|&byte| byte == 0),
    );

    lin::close(fd);
    lin::done(failed);
}

/// Whether `hay` contains `needle`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|window| window == needle)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    lin::exit(101)
}
