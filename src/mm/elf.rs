//! The ELF64 program image, minus everything a loader does not need.
//!
//! Parsing is bytes in, image out, so it is unit-tested on the host; only the
//! loading of a parsed image touches the machine. What is accepted is
//! deliberately narrow: a statically linked `ET_EXEC` whose `PT_LOAD` segments
//! say exactly where each piece goes. That is what a freestanding user program
//! links to, and anything more - dynamic linking, PIE - waits until something
//! needs it.

use alloc::vec::Vec;

use crate::mm::vm::MapFlags;

/// A statically linked ELF64 executable the loader can place.
pub struct Image<'a> {
    /// Where control is transferred to, in the lower half.
    pub entry: usize,
    /// The loadable segments, in file order.
    pub segments: Vec<Segment<'a>>,
}

/// One `PT_LOAD` segment: file bytes to place at `vaddr`, zero-filled out to
/// `memsz`, with the permissions the program header asked for.
pub struct Segment<'a> {
    pub vaddr: usize,
    /// The bytes that come from the file, `p_filesz` of them.
    pub data: &'a [u8],
    /// How far the segment runs in memory (`p_memsz`), never shorter than
    /// [`Segment::data`]; the tail is what .bss is.
    pub memsz: usize,
    pub flags: MapFlags,
}

/// First address past the lower half. A canonical address's bit 47 decides
/// which half it is in, so a user image asking for anything at or above this
/// line is asking for kernel territory, and no loader should honor it.
const USER_LIMIT: usize = 1 << 47;

const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// The size the loader needs a program header to be: everything up to and
/// including `p_memsz`, the last field it reads.
const PHDR_NEEDED: usize = 48;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Parse `bytes` as an executable image, or `None` if it is not one this
/// kernel is willing to load.
pub fn parse(bytes: &[u8]) -> Option<Image<'_>> {
    // The fixed header is 64 bytes and carries everything read before the
    // program header table.
    if bytes.len() < 64 || bytes[..4] != ELFMAG {
        return None;
    }
    if bytes[4] != ELFCLASS64 || bytes[5] != ELFDATA2LSB || bytes[6] != EV_CURRENT {
        return None;
    }
    // Only fixed-address images: a `ET_DYN` would need a base chosen at load
    // time and relocations applied, and nothing produces one here yet.
    if u16_at(bytes, 16)? != ET_EXEC {
        return None;
    }
    let entry = u64_at(bytes, 24)?;
    let phoff = u64_at(bytes, 32)? as usize;
    let phentsize = u16_at(bytes, 54)? as usize;
    let phnum = u16_at(bytes, 56)? as usize;
    if phentsize < PHDR_NEEDED {
        return None;
    }

    let mut segments = Vec::new();
    for index in 0..phnum {
        let at = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let header = bytes.get(at..at.checked_add(phentsize)?)?;
        if u32_at(header, 0)? != PT_LOAD {
            continue;
        }
        let pflags = u32_at(header, 4)?;
        let offset = u64_at(header, 8)? as usize;
        let vaddr = u64_at(header, 16)? as usize;
        let filesz = u64_at(header, 32)? as usize;
        let memsz = u64_at(header, 40)? as usize;

        // The file image cannot be longer than the memory image: bytes past
        // `memsz` would have nowhere to live, and a header claiming so is
        // broken rather than merely unusual.
        if memsz < filesz {
            return None;
        }
        // Every bound the loader computes from a segment is safe arithmetic
        // only if the segment lies in the lower half to begin with.
        if vaddr.checked_add(memsz)? > USER_LIMIT {
            return None;
        }
        let data = bytes.get(offset..offset.checked_add(filesz)?)?;

        // READ needs no bit: a present page is readable, as on the wire.
        let mut flags = MapFlags::READ;
        if pflags & PF_W != 0 {
            flags |= MapFlags::WRITE;
        }
        if pflags & PF_X != 0 {
            flags |= MapFlags::EXEC;
        }
        if memsz > 0 {
            segments.push(Segment {
                vaddr,
                data,
                memsz,
                flags,
            });
        }
    }

    if entry as usize >= USER_LIMIT {
        return None;
    }
    Some(Image {
        entry: entry as usize,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::{MapFlags, parse};
    use alloc::vec::Vec;

    /// Assemble a minimal ELF64: one Ehdr, one Phdr per entry - each a
    /// `(p_type, p_flags, p_vaddr, p_memsz, file bytes)` tuple - then every
    /// segment's file bytes back to back.
    fn elf(entry: u64, phdrs: &[(u32, u32, u64, u64, &[u8])]) -> Vec<u8> {
        use alloc::vec;

        let phdr_size = 56;
        let mut out = vec![0u8; 64 + phdrs.len() * phdr_size];
        out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // ELFDATA2LSB
        out[6] = 1; // EV_CURRENT
        out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        out[24..32].copy_from_slice(&entry.to_le_bytes());
        out[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        out[54..56].copy_from_slice(&(phdr_size as u16).to_le_bytes());
        out[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());

        let mut data = Vec::new();
        for (index, (ptype, pflags, vaddr, memsz, bytes)) in phdrs.iter().enumerate() {
            let at = 64 + index * phdr_size;
            let offset = 64 + phdrs.len() * phdr_size + data.len();
            out[at..at + 4].copy_from_slice(&ptype.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&pflags.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&(offset as u64).to_le_bytes());
            out[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            out[at + 32..at + 40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            out[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
            data.extend_from_slice(bytes);
        }
        out.extend_from_slice(&data);
        out
    }

    const PT_LOAD: u32 = 1;
    const PT_NOTE: u32 = 4;

    #[test]
    fn a_static_executable_parses_into_its_load_segments() {
        // RW segment with more memory than file: the .bss tail.
        let bytes = elf(
            0x400_078,
            &[(PT_LOAD, 6, 0x400_000, 0x120, b"program bytes")],
        );

        let image = parse(&bytes).expect("a well-formed image parses");
        assert_eq!(image.entry, 0x400_078);
        assert_eq!(image.segments.len(), 1);
        let segment = &image.segments[0];
        assert_eq!(segment.vaddr, 0x400_000);
        assert_eq!(segment.data, b"program bytes");
        assert_eq!(segment.memsz, 0x120);
        assert_eq!(segment.flags, MapFlags::READ | MapFlags::WRITE);
    }

    #[test]
    fn the_executable_bit_and_the_write_bit_come_out_of_the_header() {
        let bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 5, 0x1000, 0x100, b"code"),
                (PT_LOAD, 6, 0x2000, 0x100, b"data"),
            ],
        );

        let image = parse(&bytes).expect("parses");
        assert_eq!(image.segments[0].flags, MapFlags::READ | MapFlags::EXEC);
        assert_eq!(image.segments[1].flags, MapFlags::READ | MapFlags::WRITE);
    }

    #[test]
    fn segments_that_do_not_load_are_skipped() {
        let bytes = elf(
            0x1000,
            &[
                (PT_NOTE, 4, 0x1000, 0x100, b"metadata"),
                (PT_LOAD, 5, 0x2000, 0x100, b"code"),
            ],
        );

        let image = parse(&bytes).expect("parses");
        assert_eq!(image.segments.len(), 1);
        assert_eq!(image.segments[0].vaddr, 0x2000);
    }

    #[test]
    fn refuse_what_the_loader_cannot_place() {
        let good = elf(0x1000, &[(PT_LOAD, 6, 0x1000, 0x100, b"x")]);

        let mut wrong_magic = good.clone();
        wrong_magic[0] = 0;
        assert!(parse(&wrong_magic).is_none(), "not ELF at all");

        let mut dynamic = good.clone();
        dynamic[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        assert!(parse(&dynamic).is_none(), "a PIE needs a load-time base");

        // File bytes longer than the memory image have nowhere to live.
        let mut oversized = good.clone();
        let at = 64 + 40; // p_memsz of the first program header
        oversized[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
        assert!(parse(&oversized).is_none(), "filesz past memsz");

        // Cut off partway through the program header table.
        assert!(parse(&good[..40]).is_none(), "truncated header");
    }

    #[test]
    fn a_segment_past_the_lower_half_is_refused() {
        let bytes = elf(0x1000, &[(PT_LOAD, 6, (1 << 47) + 0x1000, 0x100, b"x")]);
        assert!(parse(&bytes).is_none(), "the kernel half is not loadable");
    }
}
