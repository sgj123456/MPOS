#![no_main]

//! The partition table of a disk, and the four questions the boot asks of it.
//!
//! `blk::part` reads an MBR or a GPT that a physical disk handed over, and the
//! answer becomes the byte range every later read is confined to. A window
//! that runs past the device, or that overlaps its own header, is how a
//! filesystem ends up reading memory the driver never intended to reach.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::blk::{BlockDev, part};
use support::MemDev;

fuzz_target!(|data: &[u8]| {
    let disk = MemDev::from_bytes(data);
    let capacity = disk.capacity_sectors();

    let found = part::partitions(&*disk);
    assert!(
        found.len() <= 128,
        "a table of {} entries is more than any GPT header allows",
        found.len()
    );
    // `partitions` answers straight from the table and never consults the
    // device, which is what makes devfs name a partition. Every other reader
    // here - `find_filesystem` and friends - does check, so the two halves can
    // disagree about whether a partition exists at all.
    //
    // Skipped for an input the harness truncated at `MAX_DISK`, where the
    // off-the-end partition is the shortened test rig rather than the table.
    if data.len() <= support::MAX_DISK {
        for &(start, sectors) in &found {
            let end = start.checked_add(sectors);
            assert!(
                end.is_some_and(|end| end <= capacity),
                "partition at {start} for {sectors} runs past a {capacity}-sector disk"
            );
        }
    }

    // The four named partitions. Each returns a window over this same device,
    // and a window is only correct if the range it wraps is inside it.
    for window in [
        part::find_filesystem(disk.clone()),
        part::find_rootfs(disk.clone()),
        part::find_boot(disk.clone()),
        part::find_data(disk.clone()),
    ] {
        let Some(window) = window else { continue };
        let inner = window.capacity_sectors();
        assert!(
            inner <= capacity,
            "a window of {inner} sectors was cut from a {capacity}-sector disk"
        );
        // Reading the window's own last sector is the boundary a filesystem
        // will hit first when the arithmetic is off by one.
        let mut buf = [0u8; 512];
        if inner > 0 {
            window.read_at(inner * 512 - 512, &mut buf);
        }
        window.read_at(0, &mut buf);
    }
});
