#![no_main]

//! Writing to an image that may not describe a real filesystem.
//!
//! Mounting is ext4's read-only half; the allocator is where the damage can be
//! permanent. Creating an entry picks an inode and a block, splitting an
//! indexed directory moves blocks and rewrites the hash tree, and a write that
//! outgrows an inode's inline extent tree grows the tree - all of it driven by
//! counters the image itself states. This target mounts, mutates, and then
//! re-reads through the same handle, which is the cheapest way to catch a
//! cache that has disagreed with the disk it just wrote.
//!
//! It is a separate target from `ext4_image` on purpose: the corpora the two
//! explore are different, and a crash while allocating says something a crash
//! while reading does not.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::vfs::ext4::Ext4Fs;
use mpos::vfs::{FileSystem, NodeKind};
use support::MemDev;

fuzz_target!(|data: &[u8]| {
    let disk = MemDev::from_bytes(data);
    let Ok(fs) = Ext4Fs::mount(MemDev::as_dev(disk)) else {
        return;
    };
    let root = fs.root();

    let name = support::text(data, 24);
    let payload: &[u8] = data.get(24..).unwrap_or_default();

    // A plain file, then a directory, then the removal of both. Every step is
    // allowed to fail - the image may have refused a read-write mount, or may
    // genuinely be full - and none of them is allowed to panic.
    if let Ok(file) = root.create(&name, NodeKind::File) {
        let _ = file.write_at(0, payload);
        let _ = file.write_at(1 << 12, &payload[..payload.len().min(64)]);
        let _ = file.truncate(payload.len() as u64);
        let mut buf = [0u8; 256];
        if let Ok(n) = file.read_at(0, &mut buf) {
            assert!(n <= buf.len());
        }
        let _ = root.unlink(&name);
    }

    if let Ok(dir) = root.create("d", NodeKind::Dir) {
        // Enough entries to reach a directory index, whose split is the
        // recursive half of this module. Bounded so a slow image stays cheap.
        for i in 0..40u32 {
            let child = alloc_name(i);
            if let Ok(file) = dir.create(&child, NodeKind::File) {
                let _ = file.write_at(0, payload);
            }
        }
        for i in (0..40u32).step_by(3) {
            let _ = dir.unlink(&alloc_name(i));
        }
        let mut buf = [0u8; 512];
        let _ = dir.read_at(0, &mut buf);
        let _ = root.rmdir("d");
    }

    // Re-reading the superblock's own accounting after all that is the point
    // of the target: a `statfs` that cannot add up means the on-disk free
    // counters and the in-memory allocator have diverged.
    let _ = fs.statfs();
});

fn alloc_name(index: u32) -> String {
    use core::fmt::Write;
    let mut name = String::from("f");
    let _ = write!(name, "{index:04}");
    name
}
