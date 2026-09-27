/// The largest hash an index entry can name; `<< 1` is where the clamped
/// ones go.
pub(crate) const HTREE_EOF_32BIT: u32 = 0x7fff_ffff;

// ---- the directory hash (htree) ----
//
// An indexed directory sorts its entries by a hash of the name. The hash is
// not a local invention: a Linux that mounts this image later has to agree
// with every value this driver ever wrote into an index, byte for byte, or
// its lookups land in the wrong leaf and names vanish. The transforms below
// are line-for-line the ones in Linux's `fs/ext4/hash.c` - the legacy ext2
// hash, a cut-down MD4 and a TEA - fed by the same seed from the superblock.

/// One round of the cut-down MD4: `a += f(b, c, d) + x`, rotated.
pub(crate) fn md4_round(
    f: impl Fn(u32, u32, u32) -> u32,
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    x: u32,
    s: u32,
) -> u32 {
    a.wrapping_add(f(b, c, d)).wrapping_add(x).rotate_left(s)
}

/// Half of MD4 - rounds 1..3 over eight words, the whole of what the htree
/// needs of the real thing.
pub(crate) fn half_md4_transform(buf: &mut [u32; 4], input: &[u32; 8]) {
    // The round constants of MD4's middle and last rounds.
    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);

    a = md4_round(f, a, b, c, d, input[0], 3);
    d = md4_round(f, d, a, b, c, input[1], 7);
    c = md4_round(f, c, d, a, b, input[2], 11);
    b = md4_round(f, b, c, d, a, input[3], 19);
    a = md4_round(f, a, b, c, d, input[4], 3);
    d = md4_round(f, d, a, b, c, input[5], 7);
    c = md4_round(f, c, d, a, b, input[6], 11);
    b = md4_round(f, b, c, d, a, input[7], 19);

    a = md4_round(g, a, b, c, d, input[1].wrapping_add(K2), 3);
    d = md4_round(g, d, a, b, c, input[3].wrapping_add(K2), 5);
    c = md4_round(g, c, d, a, b, input[5].wrapping_add(K2), 9);
    b = md4_round(g, b, c, d, a, input[7].wrapping_add(K2), 13);
    a = md4_round(g, a, b, c, d, input[0].wrapping_add(K2), 3);
    d = md4_round(g, d, a, b, c, input[2].wrapping_add(K2), 5);
    c = md4_round(g, c, d, a, b, input[4].wrapping_add(K2), 9);
    b = md4_round(g, b, c, d, a, input[6].wrapping_add(K2), 13);

    a = md4_round(h, a, b, c, d, input[3].wrapping_add(K3), 3);
    d = md4_round(h, d, a, b, c, input[7].wrapping_add(K3), 9);
    c = md4_round(h, c, d, a, b, input[2].wrapping_add(K3), 11);
    b = md4_round(h, b, c, d, a, input[6].wrapping_add(K3), 15);
    a = md4_round(h, a, b, c, d, input[1].wrapping_add(K3), 3);
    d = md4_round(h, d, a, b, c, input[5].wrapping_add(K3), 9);
    c = md4_round(h, c, d, a, b, input[0].wrapping_add(K3), 11);
    b = md4_round(h, b, c, d, a, input[4].wrapping_add(K3), 15);

    for (word, sum) in buf.iter_mut().zip([a, b, c, d]) {
        *word = word.wrapping_add(sum);
    }
}

/// The TEA round the htree uses as a second mixing function.
pub(crate) fn tea_transform(buf: &mut [u32; 4], input: &[u32; 4]) {
    const DELTA: u32 = 0x9e37_79b9;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (input[0], input[1], input[2], input[3]);
    let mut sum = 0u32;
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(
            ((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)),
        );
        b1 = b1.wrapping_add(
            ((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)),
        );
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// The original ext2 hash, before there was a tree to sort with.
pub(crate) fn dx_hack_hash(name: &[u8], unsigned: bool) -> u32 {
    let (mut hash0, mut hash1) = (0x12a3_fe2d_u32, 0x37ab_e8f9_u32);
    for &byte in name {
        let c = if unsigned {
            byte as u32
        } else {
            byte as i8 as i32 as u32
        };
        let mut hash = hash1.wrapping_add(hash0 ^ c.wrapping_mul(7152373));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// Pack name bytes into `out.len()` words the way the transforms consume
/// them. `unsigned` decides whether a byte above 127 enters the word
/// sign-extended or zero-filled - the one place the two hash flavors differ.
pub(crate) fn str2hashbuf(msg: &[u8], out: &mut [u32], unsigned: bool) {
    let len = msg.len().min(out.len() * 4);
    let mut pad = len as u32 | (len as u32) << 8;
    pad |= pad << 16;
    let mut word = 0;
    let mut at = 0;
    while len - at >= 4 {
        out[word] = (byte_of(msg[at], unsigned) << 24)
            .wrapping_add((byte_of(msg[at + 1], unsigned)) << 16)
            .wrapping_add((byte_of(msg[at + 2], unsigned)) << 8)
            .wrapping_add(byte_of(msg[at + 3], unsigned));
        word += 1;
        at += 4;
    }
    let mut val = pad;
    for b in &msg[at..len] {
        val = byte_of(*b, unsigned).wrapping_add(val << 8);
    }
    if word < out.len() {
        out[word] = val;
        word += 1;
    }
    out[word..].fill(pad);
}

pub(crate) fn byte_of(b: u8, unsigned: bool) -> u32 {
    if unsigned {
        b as u32
    } else {
        b as i8 as i32 as u32
    }
}

/// The hash an index sorts a name by: the algorithm the directory's root
/// block names, salted with the superblock's seed. `None` for a version this
/// driver does not implement (Siphash, the casefolding flavor).
///
/// Stored versions 0..=2 are the signed variants; the superblock's unsigned
/// flag renumbers them onto the unsigned ones (+3). Every hash is even - the
/// low bit is reserved to mark a leaf that continues the previous one.
pub(crate) fn dx_hash_name(
    name: &[u8],
    version: u8,
    seed: [u32; 4],
    unsigned: bool,
) -> Option<u32> {
    let version = match version {
        0..=2 if unsigned => version + 3,
        v => v,
    };
    // MD4's initialization vector: what the transforms start from when the
    // superblock never recorded a seed.
    let mut buf = if seed == [0; 4] {
        [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476]
    } else {
        seed
    };
    let hash = match version {
        0 | 3 => dx_hack_hash(name, version == 3),
        1 | 4 => {
            let mut input = [0u32; 8];
            let mut rest = name;
            while !rest.is_empty() {
                str2hashbuf(rest, &mut input, version == 4);
                half_md4_transform(&mut buf, &input);
                rest = &rest[rest.len().min(32)..];
            }
            buf[1]
        }
        2 | 5 => {
            let mut input = [0u32; 4];
            let mut rest = name;
            while !rest.is_empty() {
                str2hashbuf(rest, &mut input, version == 5);
                tea_transform(&mut buf, &input);
                rest = &rest[rest.len().min(16)..];
            }
            buf[0]
        }
        _ => return None,
    };
    let mut hash = hash & !1;
    if hash == HTREE_EOF_32BIT << 1 {
        hash = (HTREE_EOF_32BIT - 1) << 1;
    }
    Some(hash)
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    const IV: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    /// A byte enters a hash word sign-extended (the legacy flavor) or
    /// zero-filled (the unsigned one) - the single place the two differ.
    #[test]
    fn a_byte_enters_the_word_signed_or_zero_filled() {
        assert_eq!(byte_of(0x7f, false), 0x7f);
        assert_eq!(byte_of(0x7f, true), 0x7f);
        assert_eq!(byte_of(0x80, false), 0xffff_ff80);
        assert_eq!(byte_of(0x80, true), 0x80);
        assert_eq!(byte_of(0xff, false), 0xffff_ffff);
        assert_eq!(byte_of(0xff, true), 0xff);
    }

    /// Four bytes pack big-endian into one word, and the rest of the buffer is
    /// filled with the length repeated - MD4's padding, computed before the
    /// bytes are read.
    #[test]
    fn four_bytes_pack_big_endian_and_the_rest_is_padding() {
        let mut out = [0u32; 8];
        str2hashbuf(b"abcd", &mut out, true);
        assert_eq!(out[0], 0x6162_6364);
        for word in &out[1..] {
            assert_eq!(*word, 0x0404_0404, "the length, four times over");
        }
    }

    /// A short name packs into one word and the padding still fills the rest:
    /// two bytes are never dropped, they are shifted up over the padding.
    #[test]
    fn a_short_name_packs_into_one_word_and_pads_the_rest() {
        let mut out = [0u32; 8];
        str2hashbuf(b"ab", &mut out, true);
        assert_eq!(out[0], 0x0202_6162); // 'a' then 'b' over the two-byte padding
        for word in &out[1..] {
            assert_eq!(*word, 0x0202_0202);
        }
    }

    /// A byte above 127 is where the two flavors part company, and only there:
    /// for an ASCII name the signed and unsigned hashes are the same value.
    #[test]
    fn the_two_flavors_differ_only_above_ascii() {
        let mut signed = [0u32; 8];
        let mut unsigned = [0u32; 8];
        str2hashbuf(b"hello", &mut signed, false);
        str2hashbuf(b"hello", &mut unsigned, true);
        assert_eq!(signed, unsigned, "an ASCII name hashes the same either way");

        let mut signed = [0u32; 8];
        let mut unsigned = [0u32; 8];
        str2hashbuf(&[0xff], &mut signed, false);
        str2hashbuf(&[0xff], &mut unsigned, true);
        assert_ne!(signed[0], unsigned[0], "a high byte is where they differ");
    }

    /// One MD4 round: `a + f(b, c, d) + x`, rotated left by `s`.
    #[test]
    fn an_md4_round_is_a_sum_and_a_rotation() {
        // f = 0 leaves the sum and the rotation, written out.
        let zero = |_x: u32, _y: u32, _z: u32| 0;
        assert_eq!(md4_round(zero, 0, 9, 9, 9, 1, 0), 1);
        assert_eq!(md4_round(zero, 0, 9, 9, 9, 1, 4), 0x10);
        assert_eq!(md4_round(zero, 0, 9, 9, 9, 0x8000_0001, 1), 3);
        // f = z ^ (x & (y ^ z)) is MD4's own first round; here it only has to
        // be the function it is handed.
        let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
        assert_eq!(
            md4_round(f, 0, 0, 0, 0xffff_ffff, 0, 0),
            0xffff_ffff,
            "f(0, 0, ~0) is ~0"
        );
    }

    /// The legacy hash of the empty name is the initial word shifted up, which
    /// is the whole of the hash with no bytes to fold in.
    #[test]
    fn the_legacy_hash_of_nothing_is_its_initial_word() {
        assert_eq!(dx_hack_hash(b"", false), 0x12a3_fe2d << 1);
        assert_eq!(dx_hack_hash(b"", true), 0x12a3_fe2d << 1);
    }

    /// The hash an index sorts by: the versions are the ones it implements,
    /// and the unsigned flag renumbers the signed ones onto them.
    #[test]
    fn a_name_hashes_only_for_the_versions_this_driver_writes() {
        let seed = [0x1234_5678, 0x9abc_def0, 0x0f1e_2d3c, 0x4b5a_6978];
        for version in 0..=5u8 {
            assert!(
                dx_hash_name(b"name", version, seed, false).is_some(),
                "version {version}"
            );
        }
        assert!(dx_hash_name(b"name", 6, seed, false).is_none());
        assert!(dx_hash_name(b"name", 0xff, seed, false).is_none());
    }

    /// The unsigned flag is not a second algorithm: it renumbers the signed
    /// version onto the unsigned one, so asking for one and asking for the
    /// other by its number are the same question.
    #[test]
    fn the_unsigned_flag_renumbers_the_version_it_names() {
        let seed = [1, 2, 3, 4];
        for version in 0..=2u8 {
            assert_eq!(
                dx_hash_name(b"some-name", version, seed, true),
                dx_hash_name(b"some-name", version + 3, seed, false),
                "version {version} with the unsigned flag"
            );
        }
    }

    /// Every hash is even - the low bit is reserved to mark a leaf that
    /// continues the previous one - and the seed is part of the answer.
    #[test]
    fn a_hash_is_even_and_the_seed_is_part_of_it() {
        let seed = [1, 2, 3, 4];
        for version in 0..=5u8 {
            let hash = dx_hash_name(b"anything", version, seed, false).unwrap();
            assert_eq!(hash & 1, 0, "version {version} produced an odd hash");
        }
        assert_ne!(
            dx_hash_name(b"name", 1, [1, 2, 3, 4], false),
            dx_hash_name(b"name", 1, [4, 3, 2, 1], false),
            "the seed is a salt, not decoration"
        );
        // An empty name hashes too: the transforms simply never run.
        assert!(dx_hash_name(b"", 1, [0; 4], false).is_some());
    }

    /// The two transforms are the published ones (Linux's `fs/ext4/hash.c`,
    /// line for line), and this is the value they produce for a fixed input -
    /// a regression pin rather than a derivation: every index this driver
    /// ever wrote is sorted by these numbers, so a change here is a change to
    /// what a Linux that later mounts the image will find.
    #[test]
    fn the_transforms_are_locked_to_their_published_vectors() {
        let mut buf = IV;
        half_md4_transform(&mut buf, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            buf,
            [0x8970_0c05, 0xf254_f422, 0xba86_39e5, 0x4e2d_6a7c],
            "the half-MD4 of the IV over eight words"
        );

        // The TEA round mixes only the first two words; the last two come back
        // as the IV's, which is itself part of the shape worth pinning.
        let mut tea = IV;
        tea_transform(&mut tea, &[0, 1, 2, 3]);
        assert_eq!(tea, [0x4189_3e50, 0x763b_7485, IV[2], IV[3]]);
    }
}
