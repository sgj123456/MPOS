//! A FAT32 writer, for the one place the build needs to put files in a FAT
//! filesystem without mounting it.
//!
//! `mkfs.fat` makes an empty filesystem and `mcopy` would fill it, but `mtools`
//! is not a dependency this build can assume, and the alternative - mounting the
//! image - is what makes the x86_64 rootfs build need root. This is the third
//! way: read the image as a file, write the clusters and the directory entries
//! a FAT32 volume is made of, write the file back. No privilege, no mount point,
//! no new dependency, and it works on a plain file *and* on a file whose bytes
//! will be `dd`'d into a GPT partition later.
//!
//! The part that has to be right, and is the reason this is not a toy: the ESP
//! carries names that do not fit in 8.3. `initramfs.tar` has a nine-character
//! stem and `mpos.cmdline` a seven-character extension, and a firmware matching
//! on 8.3 alone sees `INITRAM.TAR` and no `mpos.cmdline` at all - the loader
//! reads the command line it was given and finds an empty initramfs, which is a
//! boot that never reaches the kernel's own device probes at all. So every name
//! that is not 8.3 is written as a long-name entry chain over a generated short
//! name, and that is the whole of the difficulty here (`NOTES.md` §15.18).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// One directory entry, whether it holds a name or a long-name fragment.
const ENTRY: usize = 32;

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
/// The attribute a long-name entry carries, which is what marks it as one: a
/// entry no filesystem would otherwise hand out, because it is read-only,
/// hidden, system and volume-id at once.
const ATTR_LONG_NAME: u8 = 0x0f;

/// FAT32's end-of-chain marker, in the 28 bits an entry keeps.
const END_OF_CHAIN: u32 = 0x0fff_ffff;
/// The lowest cluster number that names data; 0 and 1 are reserved by FAT.
const FIRST_CLUSTER: u32 = 2;

/// The 13 UCS-2 characters one long-name entry holds.
const LONG_NAME_CHARS: usize = 13;

/// A FAT32 volume, held entirely in memory and written back once.
///
/// The whole image is resident because the volume is small (tens of MiB) and
/// every operation here is a random access into it: a cluster chain is read
/// while it is being built, and the FAT is patched while the data is written.
struct Fat32 {
    image: Vec<u8>,
    bytes_per_sector: u32,
    reserved: u32,
    fat_count: u32,
    fat_sectors: u32,
    root_cluster: u32,
    /// Where the data region starts, as a byte offset.
    data_start: usize,
    /// Bytes in one cluster.
    cluster_bytes: usize,
    /// How many clusters the data region has, from `FIRST_CLUSTER` up.
    cluster_count: u32,
}

impl Fat32 {
    /// Read a volume `mkfs.fat` has already made, and check it is a shape this
    /// writer understands.
    fn open(path: &Path) -> Result<Self> {
        let image = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        Self::from_bytes(image)
    }

    /// Parse a volume out of bytes, whether they came from a file or - in a
    /// test - from nothing.
    fn from_bytes(image: Vec<u8>) -> Result<Self> {
        if image.len() < 512 {
            bail!("the image is smaller than one boot sector");
        }
        if image[510..512] != [0x55, 0xaa] {
            bail!("the image has no boot sector signature");
        }
        let bytes_per_sector = u16::from_le_bytes([image[11], image[12]]) as u32;
        let sectors_per_cluster = image[13] as u32;
        let reserved = u16::from_le_bytes([image[14], image[15]]) as u32;
        let fat_count = image[16] as u32;
        let fat_sectors = u32::from_le_bytes([image[36], image[37], image[38], image[39]]);
        let root_cluster = u32::from_le_bytes([image[44], image[45], image[46], image[47]]);

        // FAT32 only, and the check is on the fact that decides it rather than
        // on a byte: FAT12/16 keep the root directory in a fixed area after the
        // FATs and have a 16-bit entry, so every address below would be wrong.
        // A 32-bit FAT is the one `mkfs.fat -F32` writes - the same call the
        // x86_64 rootfs build already makes - and a caller that hands this a
        // FAT16 image gets a named error rather than a corrupt volume.
        if root_cluster < FIRST_CLUSTER {
            bail!("the image is not FAT32: its root directory is not in a cluster chain");
        }
        if bytes_per_sector != 512 {
            bail!("the image has {bytes_per_sector}-byte sectors; only 512 is handled");
        }
        if sectors_per_cluster == 0 {
            bail!("the image reports zero sectors per cluster");
        }

        let cluster_bytes = (bytes_per_sector * sectors_per_cluster) as usize;
        let data_start = ((reserved + fat_count * fat_sectors) * bytes_per_sector) as usize;
        if data_start >= image.len() {
            bail!("the image has no data region");
        }
        let cluster_count = ((image.len() - data_start) / cluster_bytes) as u32;

        Ok(Self {
            image,
            bytes_per_sector,
            reserved,
            fat_count,
            fat_sectors,
            root_cluster,
            data_start,
            cluster_bytes,
            cluster_count,
        })
    }

    /// The byte offset of a cluster's data.
    fn cluster_offset(&self, cluster: u32) -> usize {
        self.data_start + ((cluster - FIRST_CLUSTER) as usize) * self.cluster_bytes
    }

    /// One FAT entry, masked to the 28 bits FAT32 keeps.
    fn fat_entry(&self, cluster: u32) -> u32 {
        let at = (self.reserved * self.bytes_per_sector) as usize + cluster as usize * 4;
        u32::from_le_bytes([
            self.image[at],
            self.image[at + 1],
            self.image[at + 2],
            self.image[at + 3],
        ]) & END_OF_CHAIN
    }

    /// Write one FAT entry, into every copy.
    ///
    /// Every copy, because a filesystem with two FATs whose second copy still
    /// says "free" is a volume a repairing driver disagrees with itself about;
    /// the mirrors are the redundancy, not a cache.
    fn set_fat_entry(&mut self, cluster: u32, value: u32) {
        let bytes = (value & END_OF_CHAIN).to_le_bytes();
        for fat in 0..self.fat_count {
            let at = ((self.reserved + fat * self.fat_sectors) * self.bytes_per_sector) as usize
                + cluster as usize * 4;
            self.image[at..at + 4].copy_from_slice(&bytes);
        }
    }

    /// `count` free clusters, linked into one chain.
    ///
    /// Free means the FAT says zero, which is the only question that is safe:
    /// the volume already owns a root directory chain, and a writer that
    /// numbered clusters from 2 upward would hand the root's own cluster to the
    /// first file and write the root out of existence.
    fn alloc(&mut self, count: usize) -> Result<Vec<u32>> {
        let mut chain = Vec::with_capacity(count);
        let mut cluster = FIRST_CLUSTER;
        while chain.len() < count {
            if cluster >= FIRST_CLUSTER + self.cluster_count {
                bail!(
                    "the FAT32 volume ran out of clusters: {} needed, {} available",
                    count,
                    self.cluster_count
                );
            }
            if self.fat_entry(cluster) == 0 {
                chain.push(cluster);
            }
            cluster += 1;
        }
        for (index, &cluster) in chain.iter().enumerate() {
            let next = chain.get(index + 1).copied().unwrap_or(END_OF_CHAIN);
            self.set_fat_entry(cluster, next);
        }
        Ok(chain)
    }

    /// Write `entries` into the directory whose first cluster is `dir`, growing
    /// the directory's chain when it has no room left.
    fn append_entries(&mut self, dir: u32, entries: &[[u8; ENTRY]]) -> Result<()> {
        let mut cluster = dir;
        loop {
            let start = self.cluster_offset(cluster);
            for slot in 0..self.cluster_bytes / ENTRY {
                let at = start + slot * ENTRY;
                let free = self.image[at] == 0x00 || self.image[at] == 0xe5;
                if !free {
                    continue;
                }
                // A long-name chain and the short entry under it must not be
                // split across a cluster boundary - some drivers read the
                // chain by walking backwards from the short entry, and a
                // fragment on the far side of a link is not part of the walk.
                let end = at + entries.len() * ENTRY;
                if end > start + self.cluster_bytes {
                    continue;
                }
                self.image[at..end].fill(0);
                for (index, entry) in entries.iter().enumerate() {
                    self.image[at + index * ENTRY..at + (index + 1) * ENTRY].copy_from_slice(entry);
                }
                return Ok(());
            }
            // No room: follow the chain, extending it if this is its end.
            let next = self.fat_entry(cluster);
            if next >= 0x0fff_fff8 {
                let grown = self.alloc(1)?[0];
                self.set_fat_entry(cluster, grown);
                self.set_fat_entry(grown, END_OF_CHAIN);
                let at = self.cluster_offset(grown);
                self.image[at..at + self.cluster_bytes].fill(0);
                cluster = grown;
            } else if next == 0 {
                bail!("directory cluster {cluster} is not linked to a next cluster");
            } else {
                cluster = next;
            }
        }
    }

    /// Build the entries that name `file`: the long-name chain if the name needs
    /// one, then the 8.3 entry that carries the cluster and the size.
    fn name_entries(&self, name: &str, attr: u8, first: u32, size: u32) -> Vec<[u8; ENTRY]> {
        let (short, long) = split_name(name);
        let mut out = Vec::new();
        if let Some(chars) = long {
            out.extend(long_name_entries(&chars, &short));
        }
        out.push(short_entry(&short, attr, first, size));
        out
    }

    /// Create a subdirectory under `parent` and return its first cluster.
    fn add_dir(&mut self, parent: u32, name: &str) -> Result<u32> {
        let cluster = self.alloc(1)?[0];
        self.set_fat_entry(cluster, END_OF_CHAIN);
        let at = self.cluster_offset(cluster);
        self.image[at..at + self.cluster_bytes].fill(0);

        // The two entries every directory starts with. `..` points at the
        // parent, and at 0 for a child of the root - which is what "has no
        // parent" means in FAT, and is why a walk that climbs stops there.
        self.append_entries(cluster, &self.name_entries(".", ATTR_DIRECTORY, cluster, 0))?;
        let dot_dot = if parent == self.root_cluster {
            0
        } else {
            parent
        };
        self.append_entries(
            cluster,
            &self.name_entries("..", ATTR_DIRECTORY, dot_dot, 0),
        )?;
        self.append_entries(parent, &self.name_entries(name, ATTR_DIRECTORY, cluster, 0))?;
        Ok(cluster)
    }

    /// Write `data` as a file called `name` in the directory `dir`.
    fn add_file(&mut self, dir: u32, name: &str, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            // A zero-length file has no chain at all: first cluster 0, which is
            // what FAT uses for "allocated but empty".
            let entries = self.name_entries(name, ATTR_ARCHIVE, 0, 0);
            return self.append_entries(dir, &entries);
        }
        let clusters = data.len().div_ceil(self.cluster_bytes);
        let chain = self.alloc(clusters)?;
        for (index, &cluster) in chain.iter().enumerate() {
            let start = index * self.cluster_bytes;
            let end = usize::min(start + self.cluster_bytes, data.len());
            let at = self.cluster_offset(cluster);
            // The tail of the last cluster is left as it was found rather than
            // zeroed: nothing reads past the size in the entry, and a cluster
            // is the unit the filesystem owns.
            self.image[at..at + (end - start)].copy_from_slice(&data[start..end]);
        }
        let entries = self.name_entries(name, ATTR_ARCHIVE, chain[0], data.len() as u32);
        self.append_entries(dir, &entries)
    }

    fn write_back(&self, path: &Path) -> Result<()> {
        fs::write(path, &self.image).with_context(|| format!("Failed to write {}", path.display()))
    }
}

/// Fill the FAT32 volume at `image` with everything under `dir`.
///
/// The ESP as a tree: `EFI/BOOT/BOOTAA64.EFI`, the kernel, the command line and
/// the initramfs, each at the path the firmware or the loader looks for. Sorted,
/// so two builds of the same tree lay down the same bytes.
pub fn populate(image: &Path, dir: &Path) -> Result<()> {
    let mut volume = Fat32::open(image)?;

    fn walk(volume: &mut Fat32, host: &Path, dir: u32, depth: usize) -> Result<()> {
        if depth > 8 {
            bail!("too deeply nested to be an ESP: {}", host.display());
        }
        let mut names: Vec<_> = fs::read_dir(host)
            .with_context(|| format!("Failed to list {}", host.display()))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| entry.file_name())
            .collect();
        names.sort();

        for name in names {
            let path = host.join(&name);
            let name = name.to_string_lossy();
            if path.is_dir() {
                let child = volume.add_dir(dir, &name)?;
                walk(volume, &path, child, depth + 1)?;
            } else {
                let data = fs::read(&path)
                    .with_context(|| format!("Failed to read {}", path.display()))?;
                volume.add_file(dir, &name, &data)?;
            }
        }
        Ok(())
    }

    let root = volume.root_cluster;
    walk(&mut volume, dir, root, 0)?;
    volume.write_back(image)
}

/// A name split into the 8.3 entry that goes on disk and, when the name does
/// not fit in one, the characters a long-name chain has to carry.
fn split_name(name: &str) -> ([u8; ENTRY - 21], Option<Vec<u16>>) {
    // `.` and `..` are a dot (or two) padded with spaces, which is not what a
    // stem-and-extension split produces: `".".rsplit_once('.')` is two *empty*
    // halves, and an entry of eleven spaces is not the current directory - a
    // filesystem scanning for it does not find it and reports the directory as
    // damaged. So they are named directly, before the general case.
    if name == "." || name == ".." {
        let mut short = [b' '; 11];
        short[..name.len()].copy_from_slice(name.as_bytes());
        return (short, None);
    }
    // 8.3 is a stem of up to eight and an extension of up to three, both from a
    // reduced character set. Anything longer, or with a character outside it,
    // needs the long-name entries beside it.
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, Some(extension)),
        None => (name, None),
    };
    let upper_stem: String = stem.chars().filter(|c| short_name_char(*c)).collect();
    let upper_extension: String = extension
        .unwrap_or("")
        .chars()
        .filter(|c| short_name_char(*c))
        .collect();

    let fits = upper_stem.len() <= 8
        && upper_stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && upper_extension.len() <= 3
        && upper_extension
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');

    if fits {
        let mut short = [b' '; 11];
        for (index, byte) in upper_stem.to_uppercase().bytes().enumerate() {
            short[index] = byte;
        }
        for (index, byte) in upper_extension.to_uppercase().bytes().enumerate() {
            short[8 + index] = byte;
        }
        return (short, None);
    }

    // A generated 8.3 name, which is the price of the long one: the short entry
    // is still what carries the cluster and the size, and it must be unique and
    // legal even though nothing reads it. `~1` is the conventional tail, and the
    // stem is cut to make room for it rather than to eight, so the tail cannot
    // push the name over.
    let mut short = [b' '; 11];
    let cut: String = upper_stem.chars().take(6).collect();
    let stem8 = format!("{}~1", cut.to_uppercase());
    for (index, byte) in stem8.bytes().take(8).enumerate() {
        short[index] = byte;
    }
    for (index, byte) in upper_extension
        .to_uppercase()
        .chars()
        .take(3)
        .collect::<String>()
        .bytes()
        .enumerate()
    {
        short[8 + index] = byte;
    }
    (short, Some(name.encode_utf16().collect()))
}

/// Whether a character may appear at all in an 8.3 name.
///
/// The set FAT32 allows is small enough that the honest test is a list: letters,
/// digits and a handful of punctuation. Everything else - a space, a `+`, a
/// comma - is legal in a long name and illegal in a short one, which is the
/// commonest reason a name that looks short still needs the long entries.
fn short_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '-' | '~' | '$' | '%' | '`' | '{' | '}' | '!' | '(' | ')' | '&'
        )
}

/// The 8.3 directory entry itself: the name, the attribute, the first cluster
/// and the size.
fn short_entry(short: &[u8; 11], attr: u8, first: u32, size: u32) -> [u8; ENTRY] {
    let mut entry = [0u8; ENTRY];
    entry[..11].copy_from_slice(short);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&((first & 0xffff) as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

/// The long-name entries that spell `chars`, in the order they go on disk.
///
/// Thirteen UCS-2 characters per entry, and the entries are stored *backwards*:
/// the one holding the last characters comes first and carries the 0x40 bit that
/// marks the end. A reader walks forward collecting fragments until it meets
/// that bit, which is why a chain split across a cluster cannot be read - and
/// why `append_entries` refuses to split one.
fn long_name_entries(chars: &[u16], short: &[u8; 11]) -> Vec<[u8; ENTRY]> {
    let checksum = short_name_checksum(short);
    let chunks: Vec<&[u16]> = chars.chunks(LONG_NAME_CHARS).collect();
    let total = chunks.len();

    chunks
        .iter()
        .enumerate()
        .rev()
        .map(|(index, chunk)| {
            let mut entry = [0xffu8; ENTRY];
            // Sequence number, with 0x40 on the first entry on disk - the one
            // holding the *last* characters - because that is the direction a
            // reader walks from.
            let sequence = index + 1;
            entry[0] = if index + 1 == total {
                sequence as u8 | 0x40
            } else {
                sequence as u8
            };
            entry[11] = ATTR_LONG_NAME;
            // The type byte, which must be zero: the slot was filled with 0xff
            // above so that unused *characters* read as padding, and a driver
            // that checks this field - Linux's, and the one in the firmware -
            // rejects the entry without it, which loses the name and nothing
            // else: the file is still there under its generated 8.3 alias.
            entry[12] = 0;
            entry[13] = checksum;
            // The alias must claim no cluster, or a driver that reads the long
            // entry as a file header will invent a chain for it.
            entry[26..28].copy_from_slice(&0u16.to_le_bytes());

            // Thirteen characters in three runs of five, six and two, which is
            // the layout the 32 bytes have room for around the fields that must
            // stay where a short entry keeps them.
            //
            // A name shorter than its last entry is NUL-terminated and *then*
            // padded with 0xffff, which is what Linux's own writer does and the
            // difference that matters: the padding is what marks the slots
            // unused, but a reader that stops at the terminator rather than
            // trimming padding is common, and without the 0x0000 the name it
            // builds is the real one with a `U+FFFF` welded to the end - which
            // matches nothing. `mpos.cmdline` is twelve characters, one short of
            // filling its entry, and was the file the loader could not find.
            let mut slots = [0xffffu16; LONG_NAME_CHARS];
            for (index, &c) in chunk.iter().enumerate() {
                slots[index] = c;
            }
            if chunk.len() < LONG_NAME_CHARS {
                slots[chunk.len()] = 0x0000;
            }

            let mut at = 1;
            for slot in &slots[0..5] {
                entry[at..at + 2].copy_from_slice(&slot.to_le_bytes());
                at += 2;
            }
            at = 14;
            for slot in &slots[5..11] {
                entry[at..at + 2].copy_from_slice(&slot.to_le_bytes());
                at += 2;
            }
            at = 28;
            for slot in &slots[11..13] {
                entry[at..at + 2].copy_from_slice(&slot.to_le_bytes());
                at += 2;
            }
            entry
        })
        .collect()
}

/// The checksum a long-name entry carries, over the short name it belongs to.
///
/// Rotate-and-add, one byte at a time, and it exists so a long chain detached
/// from its short entry - by a copy, a rename, or a driver that only half moved
/// it - is visibly not the name for that entry rather than silently attached to
/// the wrong file.
fn short_name_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = (((sum & 1) << 7).wrapping_add(sum >> 1)).wrapping_add(byte);
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A FAT32 volume with the geometry `mkfs.fat -F32` gives a small image:
    /// one sector per cluster, two FATs, root directory at cluster 2. Built in
    /// memory, so a test needs no image file and no `mkfs.fat`.
    fn synthetic() -> Fat32 {
        let reserved = 32u32;
        let fat_sectors = 256u32;
        let fat_count = 2u32;
        let size = ((reserved + fat_count * fat_sectors) * 512) as usize + 64 * 512;
        let mut image = vec![0u8; size];
        image[510] = 0x55;
        image[511] = 0xaa;
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        image[13] = 1;
        image[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        image[16] = fat_count as u8;
        image[36..40].copy_from_slice(&fat_sectors.to_le_bytes());
        image[44..48].copy_from_slice(&2u32.to_le_bytes());
        Fat32::from_bytes(image).expect("the synthetic volume is FAT32")
    }

    /// A name that fits in 8.3 is written as one entry and no long-name chain.
    #[test]
    fn an_eight_dot_three_name_needs_no_long_entries() {
        let (short, long) = split_name("mpos.elf");
        assert!(long.is_none(), "mpos.elf fits in 8.3");
        assert_eq!(&short[..4], b"MPOS");
        assert_eq!(&short[8..11], b"ELF");
    }

    /// The two names the ESP actually carries that do not, and which are the
    /// whole reason this module exists.
    #[test]
    fn the_esp_names_that_forced_this_module_get_long_entries() {
        let (short, long) = split_name("initramfs.tar");
        assert!(long.is_some(), "a nine-character stem does not fit in 8.3");
        assert_eq!(&short[..8], b"INITRA~1");
        assert_eq!(&short[8..11], b"TAR");

        let (short, long) = split_name("mpos.cmdline");
        assert!(long.is_some(), "a seven-character extension does not fit");
        assert_eq!(&short[..6], b"MPOS~1");
        assert_eq!(&short[8..11], b"CMD");
    }

    /// A name that does not fill its last entry is NUL-terminated *and then*
    /// padded.
    ///
    /// This is the bug that cost a boot: `mpos.cmdline` is twelve characters in
    /// a thirteen-character entry, and with padding alone a reader that stops at
    /// a terminator rather than trimming 0xffff builds `mpos.cmdline` with a
    /// `U+FFFF` on the end, which is not the name the loader asks for.
    #[test]
    fn a_name_that_does_not_fill_its_entry_is_terminated_then_padded() {
        let (short, long) = split_name("mpos.cmdline");
        let entries = long_name_entries(&long.expect("long"), &short);
        assert_eq!(entries.len(), 1);
        // The last character slot is the terminator, not padding.
        let last = u16::from_le_bytes([entries[0][30], entries[0][31]]);
        assert_eq!(last, 0x0000, "slot 13 must be a 0x0000 terminator");
        // And the name itself is intact in the slots before it.
        let first = u16::from_le_bytes([entries[0][1], entries[0][2]]);
        assert_eq!(first, b'm' as u16);
        let twelfth = u16::from_le_bytes([entries[0][28], entries[0][29]]);
        assert_eq!(twelfth, b'e' as u16);
    }

    /// A name shorter than one entry: the terminator follows the name and the
    /// rest is padding.
    #[test]
    fn a_short_name_leaves_padding_after_the_terminator() {
        let (short, long) = split_name("initramfs.tar");
        let entries = long_name_entries(&long.expect("long"), &short);
        assert_eq!(
            u16::from_le_bytes([entries[0][30], entries[0][31]]),
            b'r' as u16,
            "thirteen characters fill the entry, so the last slot is the name's"
        );
    }

    /// The chain is stored last-fragment-first, and the first entry on disk is
    /// the one marked 0x40.
    #[test]
    fn a_long_name_chain_is_written_backwards_and_marked() {
        let (short, long) = split_name("initramfs.tar");
        let chars = long.expect("long");
        let entries = long_name_entries(&chars, &short);
        assert_eq!(entries.len(), 1, "thirteen characters is one entry");
        assert_eq!(entries[0][0], 0x40 | 1, "a single entry is also the last");
        assert_eq!(entries[0][11], ATTR_LONG_NAME);
        assert_eq!(entries[0][13], short_name_checksum(&short));
        // The characters land in three runs of five, six and two, and the first
        // one on disk is the name's first character.
        assert_eq!(
            u16::from_le_bytes([entries[0][1], entries[0][2]]),
            b'i' as u16
        );
        assert_eq!(
            u16::from_le_bytes([entries[0][3], entries[0][4]]),
            b'n' as u16
        );
        assert_eq!(
            u16::from_le_bytes([entries[0][14], entries[0][15]]),
            b'a' as u16,
            "the sixth character starts the second run"
        );
        assert_eq!(
            u16::from_le_bytes([entries[0][28], entries[0][29]]),
            b'a' as u16,
            "the twelfth character starts the third run"
        );
        assert_eq!(
            u16::from_le_bytes([entries[0][30], entries[0][31]]),
            b'r' as u16,
            "and the thirteenth ends the name"
        );
    }

    /// A name longer than one entry: the fragments are numbered from the end,
    /// and only the first on disk carries the mark.
    #[test]
    fn a_chain_of_two_is_numbered_from_the_end() {
        let (short, long) = split_name("initramfs-aarch64.tar");
        let entries = long_name_entries(&long.expect("long"), &short);
        assert_eq!(entries.len(), 2, "21 characters needs two entries");
        assert_eq!(entries[0][0], 0x40 | 2, "the first on disk is the last");
        assert_eq!(entries[1][0], 1);
        // The first entry holds the tail of the name, so its first character is
        // the fourteenth - the start of the second fragment.
        assert_eq!(
            u16::from_le_bytes([entries[0][1], entries[0][2]]),
            b'c' as u16
        );
        assert_eq!(
            u16::from_le_bytes([entries[1][1], entries[1][2]]),
            b'i' as u16
        );
    }

    /// Allocation skips the clusters the volume already owns. Handing the root
    /// directory's own cluster to a file is the bug that wrote the root out of
    /// existence, so it is pinned rather than left to a boot to find.
    #[test]
    fn allocation_never_hands_out_a_cluster_that_is_taken() {
        let mut volume = synthetic();
        // The root directory occupies cluster 2 and is marked end-of-chain.
        volume.set_fat_entry(2, END_OF_CHAIN);
        let chain = volume.alloc(3).expect("three clusters are free");
        assert_eq!(chain, vec![3, 4, 5], "cluster 2 belongs to the root");
        assert_eq!(volume.fat_entry(3), 4);
        assert_eq!(volume.fat_entry(4), 5);
        assert_eq!(volume.fat_entry(5), END_OF_CHAIN);
    }

    /// The mirrors agree, because a second FAT that still says "free" is a
    /// volume that disagrees with itself.
    #[test]
    fn every_fat_copy_is_written() {
        let mut volume = synthetic();
        volume.set_fat_entry(7, 9);
        for fat in 0..volume.fat_count {
            let at = ((volume.reserved + fat * volume.fat_sectors) * volume.bytes_per_sector)
                as usize
                + 7 * 4;
            assert_eq!(
                u32::from_le_bytes([
                    volume.image[at],
                    volume.image[at + 1],
                    volume.image[at + 2],
                    volume.image[at + 3]
                ]),
                9,
                "copy {fat} disagrees"
            );
        }
    }

    /// A file's bytes land in the cluster its entry names.
    #[test]
    fn a_files_bytes_are_readable_through_its_entry() {
        let mut volume = synthetic();
        volume.set_fat_entry(2, END_OF_CHAIN);
        let data = b"mpos reads from a FAT32 volume";
        volume
            .add_file(2, "hello.txt", data)
            .expect("the file fits");

        let entry_at = volume.cluster_offset(2);
        let first = u32::from_le_bytes([
            volume.image[entry_at + 26],
            volume.image[entry_at + 27],
            0,
            0,
        ]);
        let size = u32::from_le_bytes([
            volume.image[entry_at + 28],
            volume.image[entry_at + 29],
            volume.image[entry_at + 30],
            volume.image[entry_at + 31],
        ]);
        assert_eq!(size as usize, data.len());
        let at = volume.cluster_offset(first);
        assert_eq!(&volume.image[at..at + data.len()], data);
    }
}
