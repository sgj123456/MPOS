#![no_main]

//! Mounting, and then reading through, a disk image nobody formatted.
//!
//! ext2's on-disk structures are the largest untrusted-input surface the
//! kernel has after the network: the superblock decides the block size and the
//! group layout, an inode decides whether its data is a block map or an
//! extent tree, and every block number in both is trusted once the mount
//! succeeded. This target gets a mount to the furthest point the bytes allow
//! and then asks the resulting filesystem for inodes, names and bytes it never
//! saw coming - which is the same sequence a boot follows on a disk an
//! attacker did control.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::vfs::ext4::Ext4Fs;

use support::MemDev;

/// Names to ask a directory for. The point is not that they exist: a hashed
/// directory tree computes an index from the name before it touches the disk,
/// so a name that is not a plausible filename still walks that code.
const NAMES: [&str; 12] = [
    "",
    ".",
    "..",
    "/",
    "\0",
    "\u{1}",
    "\u{7f}",
    "é",
    "a name longer than any ext2 directory entry can hold by a wide margin",
    "\u{10ffff}",
    " ",
    "x",
];

/// Offsets to read at, including two a sane file would not be asked for.
const OFFSETS: [u64; 5] = [0, 1, 511, 4096, 1 << 20];

fuzz_target!(|data: &[u8]| {
    let disk = MemDev::from_bytes(data);
    let Ok(fs) = Ext4Fs::mount(MemDev::as_dev(disk)) else {
        return;
    };

    // The root, always: this is what the boot does first.
    read_inodes(&fs, &[2], &[]);

    // Then inode numbers straight out of the image. `inode_of` does not check
    // the number against `s_inodes_count`, so this walks the same path a
    // directory entry with a corrupt inode number would.
    let mut inodes = [0u32; 4];
    for (i, slot) in inodes.iter_mut().enumerate() {
        *slot = support::u32_at(data, i * 4) % 8192;
    }
    read_inodes(
        &fs,
        &inodes,
        &[support::text(data.get(4..).unwrap_or_default(), 60)],
    );
});

fn read_inodes(fs: &Ext4Fs, inodes: &[u32], asked: &[String]) {
    let mut buf = [0u8; 512];
    for &ino in inodes {
        let inode = fs.inode_of(ino);
        let meta = inode.metadata();
        assert_eq!(meta.ino, ino as u64, "inode {ino} answered as another");
        let _ = inode.mode();
        let _ = inode.readlink();
        for offset in OFFSETS {
            if let Ok(n) = inode.read_at(offset, &mut buf) {
                assert!(n <= buf.len(), "read_at returned {n} of {}", buf.len());
            }
        }
        for name in NAMES
            .iter()
            .map(|n| n.to_string())
            .chain(asked.iter().cloned())
        {
            if let Ok(child) = inode.lookup(&name) {
                let _ = child.metadata();
            }
        }
    }
}
