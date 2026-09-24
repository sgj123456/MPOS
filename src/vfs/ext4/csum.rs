//! The two checksums the ext4 on-disk format carries, and nothing else.
//!
//! ext4's metadata integrity rests on two CRCs, both defined by what they
//! stream over rather than by a textbook name:
//!
//! * `crc32c` - Castagnoli's polynomial, reflected (the `0x82F63B78` of iSCSI
//!   and SSE4.2), used *seeded and chained*: a caller passes the previous
//!   CRC in and reads the next one out, so a long structure is one fold of
//!   the running value. The final XOR that the RFC 3720 check value carries
//!   is the *framing* convention, not the primitive; here the primitive is
//!   what the format uses, and the one test pins both.
//! * `crc16` - the reflected `0xA001` of e2fsprogs' `crc16()`, which the
//!   older `uninit_bg`/`GDT_CSUM` spelling puts in each group descriptor.
//!
//! Both tables build themselves in a `const` block, so there is one copy of
//! the truth and no `static` initializer to get wrong.

/// Advance the reflected CRC-32C over `bytes`, starting from a caller's
/// running value. This is the raw streaming primitive the kernel's
/// `crc32c` and e2fsprogs' `ext2fs_crc32c` both mean: no initial ones, no
/// final complement - those belong to whoever frames a message.
pub(crate) fn crc32c(seed: u32, bytes: &[u8]) -> u32 {
    let mut crc = seed;
    for &b in bytes {
        crc = (crc >> 8) ^ CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

/// The RFC-3720 framing: complement in, complement out. A digest of one
/// buffer, where the streaming fold has to begin and end at a constant.
#[cfg(test)]
pub(crate) fn crc32c_of(bytes: &[u8]) -> u32 {
    !crc32c(!0, bytes)
}

const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

const fn build_crc32c_table() -> [u32; 256] {
    // The reflected form of the Castagnoli polynomial 0x1EDC6F41.
    const POLY: u32 = 0x82F6_3B78;
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// e2fsprogs' `crc16()`: the reflected `0xA001` (0x8005 un-reflected, the
/// polynomial of the old CRC-16-IBM), seeded by the caller's running value
/// and without final XOR. Group descriptors under `GDT_CSUM` fold the
/// superblock's UUID, the group number and the descriptor bytes through
/// exactly this.
pub(crate) fn crc16(seed: u16, bytes: &[u8]) -> u16 {
    let mut crc = seed;
    for &b in bytes {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// The metadata checksums, per the format's own definitions (e2fsprogs
// lib/ext2fs/csum.c and fs/ext4 - kept in agreement by the same e2fsck; the
// FileDev tests over a real `mke2fs -t ext4` image pin each against the
// bytes that program writes).
// ---------------------------------------------------------------------------

/// The checksum seed as both implementations compute it: the superblock's
/// written-down seed when `metadata_csum_seed` names its location, and the
/// raw streaming fold of `!0` through the UUID otherwise.
pub(crate) fn csum_seed(written: Option<u32>, uuid: &[u8; 16]) -> u32 {
    written.unwrap_or_else(|| crc32c(!0, uuid))
}

/// One group descriptor's `metadata_csum` checksum, stored in the 16 bits
/// at `bg_checksum` (offset 30 of the descriptor): the seed, the group
/// number as a little-endian word, then the descriptor itself with its own
/// checksum field zeroed.
pub(crate) fn gdt_csum(seed: u32, group: u32, desc: &mut [u8]) -> u16 {
    let old = (desc[30], desc[31]);
    desc[30] = 0;
    desc[31] = 0;
    let crc = crc32c(crc32c(seed, &group.to_le_bytes()), desc);
    desc[30] = old.0;
    desc[31] = old.1;
    (crc & 0xFFFF) as u16
}

/// The same descriptor under the older `GDT_CSUM`/`uninit_bg` spelling: a
/// CRC-16 over UUID, group number, and the first 32 bytes of the descriptor
/// with its checksum field (offset 30 of that layout) zeroed.
pub(crate) fn gdt_csum16(uuid: &[u8; 16], group: u32, desc: &mut [u8]) -> u16 {
    let old = (desc[30], desc[31]);
    desc[30] = 0;
    desc[31] = 0;
    let crc = crc16(
        crc16(crc16(!0, uuid), &group.to_le_bytes()),
        &desc[..desc.len().min(32)],
    );
    desc[30] = old.0;
    desc[31] = old.1;
    crc
}

/// A bitmap block's checksum, stored as the descriptor's 16+16 block/inode
/// bitmap halves: the seed and the bitmap bytes, nothing else - neither the
/// group number nor an inode.
pub(crate) fn bitmap_csum(seed: u32, bitmap: &[u8]) -> u32 {
    crc32c(seed, bitmap)
}

/// The same under the older `GDT_CSUM` spelling: a 16-bit fold of UUID,
/// group number, and the bitmap.
pub(crate) fn bitmap_csum16(uuid: &[u8; 16], group: u32, bitmap: &[u8]) -> u16 {
    crc16(crc16(crc16(!0, uuid), &group.to_le_bytes()), bitmap)
}

/// The superblock's own checksum: a crc32c over its first 1020 bytes from a
/// raw all-ones start - not the volume's seed - stored at 1020 of the
/// superblock's own area.
pub(crate) fn sb_csum(head: &[u8]) -> u32 {
    crc32c(!0, head)
}

/// The leaf directory block's trailer checksum: seed, the directory's inode
/// number and generation, then every octet up to the 12-byte fake entry
/// that hosts the result - the tail's own front (`det_reserved_zero1`) is
/// part of the covered region only as far as the tail begins; the checksum
/// word lives in the tail's last four octets.
pub(crate) fn dir_csum(seed: u32, dir_ino: u32, dir_gen: u32, block: &[u8]) -> u32 {
    let body = &block[..block.len().saturating_sub(12)];
    let s = crc32c(seed, &dir_ino.to_le_bytes());
    let s = crc32c(s, &dir_gen.to_le_bytes());
    crc32c(s, body)
}

/// An indexed directory block's checksum: the same folds, but the covered
/// bytes run only through the live index entries, then the four octets of
/// the tail that precede its checksum, then a zero word standing where the
/// checksum itself will be written.
pub(crate) fn dx_csum(
    seed: u32,
    dir_ino: u32,
    dir_gen: u32,
    head: &[u8],
    entries_end: usize,
    tail_first4: &[u8],
) -> u32 {
    let s = crc32c(seed, &dir_ino.to_le_bytes());
    let s = crc32c(s, &dir_gen.to_le_bytes());
    let s = crc32c(s, &head[..entries_end]);
    let s = crc32c(s, &tail_first4[..tail_first4.len().min(4)]);
    crc32c(s, &0u32.to_le_bytes())
}

/// An inode's checksum: seed, inode number, generation (offset 100), then
/// the whole on-disk record with its own checksum fields zeroed - the low
/// half's four-byte osd2 word at 124 and the high half at 130, the positions
/// fingerprinted against debugfs's own re-stamping. The answer splits: low
/// half at 124, high half at 130 once the extra area reaches it. An inode
/// with no extra space carries no checksum anywhere; both implementations
/// read that as nothing to verify, and a record whose stored fields are
/// zero alike.
pub(crate) fn inode_csum(seed: u32, ino: u32, gen_: u32, rec: &mut [u8]) -> (u16, u16) {
    let (lo0, lo1, lo2, lo3) = (rec[124], rec[125], rec[126], rec[127]);
    let (h0, h1) = (rec[130], rec[131]);
    rec[124..128].fill(0);
    rec[130] = 0;
    rec[131] = 0;
    let s = crc32c(seed, &ino.to_le_bytes());
    let s = crc32c(s, &gen_.to_le_bytes());
    let c = crc32c(s, rec);
    rec[124] = lo0;
    rec[125] = lo1;
    rec[126] = lo2;
    rec[127] = lo3;
    rec[130] = h0;
    rec[131] = h1;
    ((c & 0xFFFF) as u16, (c >> 16) as u16)
}

/// An extent block's tail checksum: the same inode folds, then the header
/// and live entries up to where the tail's checksum word goes. Where that
/// end falls (twelve header bytes plus twelve per entry) is the caller's
/// arithmetic; this folds what it hands over.
pub(crate) fn ext_csum(seed: u32, ino: u32, gen_: u32, covered: &[u8]) -> u32 {
    let s = crc32c(seed, &ino.to_le_bytes());
    let s = crc32c(s, &gen_.to_le_bytes());
    crc32c(s, covered)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{bitmap_csum, crc16, crc32c, crc32c_of, csum_seed, dir_csum, gdt_csum, inode_csum};

    /// The primitive is the streaming fold; the RFC's value is what its
    /// framing of that fold produces. Pinning both keeps a future "fix"
    /// that adds complements inside `crc32c` from silently changing every
    /// seeded call site.
    #[test]
    fn crc32c_matches_its_reference_value_under_the_rfc_framing() {
        assert_eq!(crc32c_of(b"123456789"), 0xE306_9283);
        // Streaming is associative across the fold, raw: two passes carry
        // the running value forward untouched - no complements in between.
        let a = crc32c(0, b"1234567");
        assert_eq!(crc32c(a, b"89"), crc32c(0, b"123456789"));
    }

    /// e2fsprogs' CRC-16 carries the standard `"123456789"` check value of
    /// 0xBB3D with a zero seed and no final XOR - the spelling the
    /// `GDT_CSUM` checksum was built from.
    #[test]
    fn crc16_matches_e2fsprogs_check_value() {
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    /// The seed: a written-down value is taken as-is, and the fallback is
    /// the all-ones fold of the UUID - the two spellings every
    /// `metadata_csum` structure chains from.
    #[test]
    fn the_seed_is_the_written_value_or_the_uuid_fold_from_all_ones() {
        let uuid = *b"0123456789abcdef";
        assert_eq!(csum_seed(Some(7), &uuid), 7);
        assert_eq!(csum_seed(None, &uuid), crc32c(!0, &uuid));
    }

    /// The shape of each chain - which octets fold in which order, and that
    /// a checksum call restores what it borrowed to zero the field. The
    /// byte-exact agreement with `mke2fs` is the FileDev tests' job; these
    /// fail when a call site reshuffles the fold.
    #[test]
    fn each_checksum_is_the_documented_fold_chain() {
        let seed = 0x9436_1cb2;
        let mut desc = [0u8; 64];
        desc[0] = 3;
        desc[30] = 0x77;
        let expect = {
            let mut d = desc;
            d[30] = 0;
            d[31] = 0;
            (crc32c(crc32c(seed, &0u32.to_le_bytes()), &d) & 0xFFFF) as u16
        };
        assert_eq!(gdt_csum(seed, 0, &mut desc), expect);
        assert_eq!(desc[30], 0x77, "the borrowed zeroing is put back");

        assert_eq!(bitmap_csum(seed, &[9u8; 128]), crc32c(seed, &[9u8; 128]));

        let block = [7u8; 1024];
        let folded = {
            let s = crc32c(seed, &2u32.to_le_bytes());
            let s = crc32c(s, &5u32.to_le_bytes());
            crc32c(s, &block[..1012])
        };
        assert_eq!(dir_csum(seed, 2, 5, &block), folded);

        let mut rec = [4u8; 256];
        rec[124] = 0xAA;
        rec[130] = 0xBB;
        let (lo, hi) = inode_csum(seed, 13, 9, &mut rec);
        let expect = {
            let mut r = rec;
            r[124..128].fill(0);
            r[130] = 0;
            r[131] = 0;
            let s = crc32c(seed, &13u32.to_le_bytes());
            let s = crc32c(s, &9u32.to_le_bytes());
            crc32c(s, &r)
        };
        assert_eq!((hi, lo), ((expect >> 16) as u16, (expect & 0xFFFF) as u16));
        assert_eq!(
            (rec[124], rec[130]),
            (0xAA, 0xBB),
            "and the record survives"
        );
    }

    /// The root inode of a `mkfs.ext4 -b 4096` volume with `metadata_csum`
    /// and `metadata_csum_seed`, kept byte for byte as the format's own
    /// tools wrote it, together with that volume's checksum seed. `debugfs`
    /// reads this inode's checksum off the record as 0xff9c1069; a fold that
    /// does not say the same number has the seed, a field offset, or the
    /// zeroed window wrong - and only one of those at a time would.
    #[test]
    fn an_inodes_checksum_matches_the_one_debugfs_reads_off_a_real_volume() {
        let hex = "ed4100000010000073edb36a73edb36a73edb36a00000000000003000800000000000800000000000af3010004000000000000000000000001000000251000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006910000020009cff00000000000000000000000073edb36a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        let rec_bytes = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect::<alloc::vec::Vec<u8>>();
        assert_eq!(rec_bytes.len(), 256);
        let mut rec = [0u8; 256];
        rec.copy_from_slice(&rec_bytes);
        let (lo, hi) = inode_csum(0x822f_1421, 2, 0, &mut rec);
        assert_eq!((lo, hi), (0x1069, 0xff9c));
    }
}
