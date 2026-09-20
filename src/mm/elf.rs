//! The ELF64 program image, minus everything a loader does not need.
//!
//! The ELF format itself - headers, program headers, dynamic entries,
//! relocation records - is parsed by the `elf` crate, which is pure safe Rust
//! and fuzz-tested, so this module keeps only what a loader decides: which
//! files it is willing to place, where each piece lands, and what the caller
//! must place. Parsing is bytes in, image out, so it is unit-tested on the
//! host; only the loading of a parsed image touches the machine. What is
//! accepted is deliberately narrow: a statically linked `ET_EXEC` or `ET_DYN`
//! whose `PT_LOAD` segments say exactly where each piece goes. That is what a
//! freestanding user program links to, and anything more - dynamic linking,
//! PIE with a base - waits until something needs it.

use alloc::vec::Vec;
use elf::ElfBytes;
use elf::abi::{
    DT_RELA, DT_RELAENT, DT_RELASZ, ET_DYN, ET_EXEC, PF_W, PF_X, PT_DYNAMIC, PT_INTERP, PT_LOAD,
    R_X86_64_RELATIVE,
};
use elf::dynamic::Dyn;
use elf::endian::LittleEndian;
use elf::file::Class;
use elf::parse::ParsingTable;
use elf::relocation::Rela;

use crate::mm::vm::MapFlags;

/// A statically linked ELF64 executable the loader can place.
pub struct Image<'a> {
    /// Where control is transferred to, in the lower half.
    pub entry: usize,
    /// The loadable segments, in file order.
    pub segments: Vec<Segment<'a>>,
    /// Virtual address of the program header table (for AT_PHDR auxv).
    pub phdr_addr: usize,
    /// Number of program headers (for AT_PHNUM auxv).
    pub phnum: usize,
    /// Size of each program header entry (for AT_PHENT auxv).
    pub phent: usize,
    /// R_X86_64_RELATIVE relocations: (virtual address, addend).
    /// The loader must write `addend` at `offset` after mapping segments.
    pub rela: Vec<(usize, i64)>,
    /// The program interpreter this image asks to be started through, as a
    /// path: `/lib/ld-musl-x86_64.so.1` for anything linked against musl
    /// dynamically. Its presence is what makes an image dynamically linked;
    /// `None` means the image is complete on its own and is entered directly.
    ///
    /// The loader does not act on this - it is a path into the filesystem, and
    /// this module has never heard of one. It reports it so that the caller,
    /// which does have a filesystem, can load what the image points at.
    pub interp: Option<Vec<u8>>,
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

/// The size of an `Elf64_Rela` record. The `elf` crate's table view reads
/// exactly this stride, so a `DT_RELAENT` claiming anything else is a file
/// this loader does not understand.
const RELA64_RECORD: usize = 24;

/// Parse `bytes` as an executable image, or `None` if it is not one this
/// kernel is willing to load.
pub fn parse(bytes: &[u8]) -> Option<Image<'_>> {
    // The crate verifies the magic, the ELF version, the endianness (a
    // big-endian file fails the ident parse against `LittleEndian`) and that
    // every table it hands out lies inside `bytes`. ELF32 is refused here:
    // the loader's placement and relocation math assume the 64-bit shapes.
    let file = ElfBytes::<LittleEndian>::minimal_parse(bytes).ok()?;
    if file.ehdr.class != Class::ELF64 {
        return None;
    }
    // Accept ET_EXEC (fixed-address) and ET_DYN (static-pie). Both are
    // statically linked and need no relocation processing beyond the
    // R_X86_64_RELATIVE entries: ET_DYN segments have vaddrs that are
    // absolute (base 0 for static-pie), so the loader can map them directly.
    if file.ehdr.e_type != ET_EXEC && file.ehdr.e_type != ET_DYN {
        return None;
    }
    let entry = file.ehdr.e_entry;
    let phoff = file.ehdr.e_phoff;

    let mut segments = Vec::new();
    let mut dyn_data: Option<&[u8]> = None;
    let mut interp_data: Option<&[u8]> = None;
    for phdr in file.segments()?.iter() {
        if phdr.p_type == PT_DYNAMIC {
            // The dynamic entries the relocation pass reads below come from
            // the PT_DYNAMIC segment, the loader's authoritative source.
            dyn_data = file.segment_data(&phdr).ok();
        }
        if phdr.p_type == PT_INTERP {
            // A NUL-terminated path to the interpreter. More than one such
            // segment is malformed, and the first is as good an answer as any.
            interp_data = interp_data.or_else(|| file.segment_data(&phdr).ok());
        }
        if phdr.p_type != PT_LOAD {
            continue;
        }
        let vaddr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;
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
        // Zero-copy: the slice borrows straight out of `bytes`, with the
        // crate having checked that the file range is inside it.
        let data = file.segment_data(&phdr).ok()?;
        // READ needs no bit: a present page is readable, as on the wire.
        let mut flags = MapFlags::READ;
        if phdr.p_flags & PF_W != 0 {
            flags |= MapFlags::WRITE;
        }
        if phdr.p_flags & PF_X != 0 {
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

    // For static-pie ET_DYN binaries the dynamic section lists RELA entries
    // that the runtime linker would process. We only handle the simple
    // R_X86_64_RELATIVE kind (write `addend` at `offset`).
    let rela = extract_rela(bytes, &file, dyn_data);

    // Compute the virtual address of the program header table.  For both
    // ET_EXEC and ET_DYN the table lives inside the first PT_LOAD segment
    // (file offset 0), so the virtual address is base + e_phoff.
    let phdr_addr = segments.first().map_or(0, |s| s.vaddr + phoff as usize);
    // The path loses its terminator here and keeps nothing else: the caller
    // hands it to the filesystem, which wants the name, not the padding.
    let interp = interp_data.and_then(|bytes| {
        let end = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        (!bytes[..end].is_empty()).then(|| Vec::from(&bytes[..end]))
    });
    Some(Image {
        entry: entry as usize,
        segments,
        phdr_addr,
        phnum: file.ehdr.e_phnum as usize,
        phent: file.ehdr.e_phentsize as usize,
        rela,
        interp,
    })
}

/// Convert a virtual address to a file offset by finding the PT_LOAD segment
/// that contains it.  Returns `None` if no segment covers `vaddr`.
fn vaddr_to_offset(file: &ElfBytes<'_, LittleEndian>, vaddr: usize) -> Option<usize> {
    for phdr in file.segments()?.iter() {
        if phdr.p_type != PT_LOAD {
            continue;
        }
        let base = phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        if vaddr >= base && vaddr < base + memsz {
            return Some(phdr.p_offset as usize + (vaddr - base));
        }
    }
    None
}

/// Read the DYNAMIC section and extract R_X86_64_RELATIVE relocations.
fn extract_rela(
    bytes: &[u8],
    file: &ElfBytes<'_, LittleEndian>,
    dyn_data: Option<&[u8]>,
) -> Vec<(usize, i64)> {
    let Some(dyn_data) = dyn_data else {
        return Vec::new();
    };

    // The crate parses each `(d_tag, d_val)` record; the table ends at the
    // first DT_NULL the iterator stops on.
    let mut rela_vaddr: Option<u64> = None;
    let mut rela_size: Option<u64> = None;
    let mut rela_ent: Option<u64> = None;
    for entry in ParsingTable::<LittleEndian, Dyn>::new(LittleEndian, Class::ELF64, dyn_data) {
        match entry.d_tag {
            DT_RELA => rela_vaddr = Some(entry.d_val()),
            DT_RELASZ => rela_size = Some(entry.d_val()),
            DT_RELAENT => rela_ent = Some(entry.d_val()),
            _ => {}
        }
    }

    let (Some(rela_vaddr), Some(rela_size)) = (rela_vaddr, rela_size) else {
        return Vec::new();
    };
    if rela_size == 0 {
        return Vec::new();
    }
    if let Some(ent) = rela_ent
        && ent as usize != RELA64_RECORD
    {
        return Vec::new();
    }

    // Relocate the RELA table's virtual address into the file view.
    let Some(file_offset) = vaddr_to_offset(file, rela_vaddr as usize) else {
        return Vec::new();
    };
    let Some(end) = file_offset.checked_add(rela_size as usize) else {
        return Vec::new();
    };
    let Some(rela_bytes) = bytes.get(file_offset..end) else {
        return Vec::new();
    };

    // The records themselves are parsed by the crate: iterate them as
    // `Rela`s and keep the one kind the loader applies.
    let table = ParsingTable::<LittleEndian, Rela>::new(LittleEndian, Class::ELF64, rela_bytes);
    table
        .iter()
        .filter(|rela| rela.r_type == R_X86_64_RELATIVE)
        .map(|rela| (rela.r_offset as usize, rela.r_addend))
        .collect()
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{MapFlags, parse};
    use alloc::vec;
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
    const PT_DYNAMIC: u32 = 2;
    const PT_INTERP: u32 = 3;
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
        assert!(parse(&dynamic).is_some(), "a static-pie binary is accepted");

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

    #[test]
    fn a_static_image_names_no_interpreter() {
        let bytes = elf(0x400_078, &[(PT_LOAD, 6, 0x400_000, 0x120, b"program")]);
        assert_eq!(parse(&bytes).expect("parses").interp, None);
    }

    #[test]
    fn a_dynamic_image_reports_the_interpreter_it_asks_for() {
        // The path is stored NUL-terminated and is reported without the
        // terminator, since the caller hands it to the filesystem.
        let path = b"/lib/ld-musl-x86_64.so.1\0";
        let bytes = elf(
            0x1000,
            &[
                (PT_INTERP, 4, 0x200, path.len() as u64, path),
                (PT_LOAD, 5, 0x1000, 0x100, b"code"),
            ],
        );

        let image = parse(&bytes).expect("parses");
        assert_eq!(
            image.interp.as_deref(),
            Some(&b"/lib/ld-musl-x86_64.so.1"[..])
        );
        // The interpreter is a note to the loader, not something to map: it
        // has no vaddr of its own and must not turn into a segment.
        assert_eq!(image.segments.len(), 1);
        assert_eq!(image.segments[0].vaddr, 0x1000);
    }

    /// One `Elf64_Rela` record: where to write, what kind it is (the low half
    /// of `r_info`, with the symbol index above it), and the addend.
    fn rela(r_offset: u64, r_type: u32, r_addend: i64) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..8].copy_from_slice(&r_offset.to_le_bytes());
        out[8..16].copy_from_slice(&(r_type as u64).to_le_bytes());
        out[16..24].copy_from_slice(&r_addend.to_le_bytes());
        out
    }

    /// One `(d_tag, d_val)` pair of a `PT_DYNAMIC` table.
    fn dyn_entry(tag: u64, val: u64) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&tag.to_le_bytes());
        out[8..16].copy_from_slice(&val.to_le_bytes());
        out
    }

    /// A static-pie binary lists its `R_X86_64_RELATIVE` entries in `.rela.dyn`
    /// and the runtime linker would apply them. The loader applies only that
    /// one kind - it is the only one that needs nothing but a base address -
    /// so the table is read out of the dynamic segment and filtered.
    #[test]
    fn the_relative_relocations_are_the_ones_the_loader_reads() {
        // The table sits 16 bytes into the load segment, so reading it means
        // translating a virtual address to a file offset rather than assuming
        // it starts at the segment.
        let mut table = Vec::new();
        table.extend_from_slice(&[0u8; 16]);
        table.extend_from_slice(&rela(0x2000, 8, 0x1234)); // R_X86_64_RELATIVE
        table.extend_from_slice(&rela(0x3000, 1, 0x99)); // some other kind
        table.extend_from_slice(&rela(0x4000, 8, -8)); // relative again

        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&dyn_entry(7, 0x1000 + 16)); // DT_RELA
        dynamic.extend_from_slice(&dyn_entry(8, 72)); // DT_RELASZ
        dynamic.extend_from_slice(&dyn_entry(9, 24)); // DT_RELAENT
        dynamic.extend_from_slice(&dyn_entry(0, 0)); // DT_NULL

        let bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x200, &table),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
            ],
        );
        let image = parse(&bytes).expect("parses");
        assert_eq!(image.rela, vec![(0x2000, 0x1234), (0x4000, -8)]);
    }

    /// A record size the loader does not use is a file it must not guess at:
    /// the stride is what turns a byte range into records, and reading the
    /// wrong one would produce addresses out of the middle of a field.
    #[test]
    fn a_rela_table_of_an_unknown_record_size_is_left_alone() {
        let mut table = Vec::new();
        table.extend_from_slice(&rela(0x2000, 8, 0x1234));

        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&dyn_entry(7, 0x1000)); // DT_RELA
        dynamic.extend_from_slice(&dyn_entry(8, 24)); // DT_RELASZ
        dynamic.extend_from_slice(&dyn_entry(9, 16)); // DT_RELAENT, not 24
        dynamic.extend_from_slice(&dyn_entry(0, 0));

        let bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x100, &table),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
            ],
        );
        assert!(parse(&bytes).expect("parses").rela.is_empty());
    }

    /// `DT_RELA` is a virtual address, and one no segment covers names bytes
    /// that are not in the file - which is a malformed image, not a table to
    /// read at whatever offset the arithmetic happens to produce.
    #[test]
    fn a_rela_table_outside_every_segment_is_left_alone() {
        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&dyn_entry(7, 0x900_000)); // nowhere
        dynamic.extend_from_slice(&dyn_entry(8, 24));
        dynamic.extend_from_slice(&dyn_entry(9, 24));
        dynamic.extend_from_slice(&dyn_entry(0, 0));

        let bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x100, b"code"),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
            ],
        );
        assert!(parse(&bytes).expect("parses").rela.is_empty());
    }

    #[test]
    fn an_empty_interpreter_path_is_not_an_interpreter() {
        let bytes = elf(
            0x1000,
            &[
                (PT_INTERP, 4, 0x200, 1, b"\0"),
                (PT_LOAD, 5, 0x1000, 0x100, b"code"),
            ],
        );
        assert_eq!(parse(&bytes).expect("parses").interp, None);
    }
}
