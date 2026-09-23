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
