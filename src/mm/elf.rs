//! The ELF64 program image, minus everything a loader does not need.
//!
//! The ELF format itself - headers, program headers, dynamic entries,
//! relocation records - is parsed by the `elf` crate, which is pure safe Rust
//! and fuzz-tested, so this module keeps only what a loader decides: which
//! files it is willing to place, where each piece lands, and what the caller
//! must place. Parsing is bytes in, image out, so it is unit-tested on the
//! host; only the loading of a parsed image touches the machine. What is
//! accepted is deliberately narrow: a statically linked `ET_EXEC` or `ET_DYN`
//! whose `PT_LOAD` segments say where each piece goes relative to a base. The
//! first kind's base is the one its header implies and the loader must honour;
//! the second kind is placed wherever the loader likes, which is what makes
//! address randomisation possible. Dynamic linking - following `PT_INTERP` and
//! letting the interpreter finish the job - is the third thing this accepts, and
//! the kernel does no part of *that* relocation itself.

use alloc::vec::Vec;
use elf::abi::{
    DT_RELA, DT_RELAENT, DT_RELASZ, EM_AARCH64, EM_X86_64, ET_DYN, ET_EXEC, PF_W, PF_X, PT_DYNAMIC,
    PT_INTERP, PT_LOAD, R_AARCH64_RELATIVE, R_X86_64_RELATIVE,
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
    pub segments: Vec<Segment>,
    /// Virtual address of the program header table (for AT_PHDR auxv).
    pub phdr_addr: usize,
    /// Number of program headers (for AT_PHNUM auxv).
    pub phnum: usize,
    /// Size of each program header entry (for AT_PHENT auxv).
    pub phent: usize,
    /// Whether the image's addresses are positions rather than destinations.
    ///
    /// This is `ET_DYN`, and it is the loader's whole basis for moving a
    /// program: an image that says `ET_EXEC` names the address each piece must
    /// be at - a binary that was linked to sit at `0x400000` has jumps to
    /// `0x400000`-plus-offset baked into it - while a position-independent one
    /// carries `R_X86_64_RELATIVE` records precisely so that it can be placed
    /// anywhere. Linux honours the same split, and honours it the same way: a
    /// `ET_DYN` load address is randomised per `exec`, an `ET_EXEC` one is not
    /// randomised at all.
    pub pie: bool,
    /// The machine the file's `e_machine` names (`EM_X86_64` or `EM_AARCH64` -
    /// nothing else parses here). The parser accepts both because it is pure
    /// bytes-in, image-out and must be testable on the host, but the kernel runs
    /// one machine, so this is reported for the caller (`binfmt`/`execve`) to
    /// reject an image built for the wrong ISA rather than the loader deciding.
    pub machine: u16,
    /// `R_*_RELATIVE` relocations for `machine`: (virtual address, addend).
    /// The loader must write `base + addend` at `offset` after mapping segments -
    /// the one relative-relocation kind, whose number differs per psABI but whose
    /// meaning does not.
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
    /// Where the segment bytes live. The image describes the program; this is
    /// how the loader gets the bytes for a page without holding the file.
    pub source: &'a dyn Source,
}

/// Where an image's bytes come from.
///
/// An `exec` must not read a program into one allocation: the kernel heap's
/// largest block is 64 MiB (`mm::pframe::MAX_ORDER`), so a 296 MB binary - a
/// browser - could never be loaded that way, and the failure surfaced as
/// `ENOSPC` from a `Vec::try_reserve_exact` (`docs/notes/` §"the image is read
/// in pieces"). A *file* is a source: the loader asks for the one page it is
/// about to place, so the allocation is the page, not the program.
///
/// `&[u8]` is a source too, which is how every host test and the kernel's own
/// in-memory spawns parse without a filesystem behind them.
pub trait Source {
    /// Bytes the source can answer for.
    fn len(&self) -> usize;
    /// Read `buf.len()` bytes at `offset`. `Some(n)` is a short read only at
    /// end of file; `None` is a source that cannot answer at all.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize>;

    /// Nothing to read. A default rather than a required answer: it is asked
    /// once, to refuse a file too short to hold a header, and every source says
    /// the same thing.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The file this source's bytes live in, when they live in one: its device
    /// and inode, which `/proc/<pid>/maps` reports beside each mapped segment.
    ///
    /// Default `None`, and that is the honest answer for a source that *is* the
    /// bytes (`&[u8]`, a script's buffer): there is no file to name, and a
    /// mapping of it has no offset to report. A file-backed source answers the
    /// pair, and the loader puts it on each segment's range so a reader can
    /// attribute an address to a file and an offset - the question every crash
    /// reporter and symboliser asks of `maps`.
    fn identity(&self) -> Option<((u32, u32), u64)> {
        None
    }
}

impl Source for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let available = self.len().checked_sub(offset)?;
        let count = available.min(buf.len());
        buf[..count].copy_from_slice(&self[offset..offset + count]);
        Some(count)
    }
}

impl Source for &[u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let available = self.len().checked_sub(offset)?;
        let count = available.min(buf.len());
        buf[..count].copy_from_slice(&self[offset..offset + count]);
        Some(count)
    }
}

/// A borrowed slice as a [`Source`]: the in-memory images the kernel's own
/// spawns and every host test parse.
pub struct Bytes<'a>(pub &'a [u8]);

impl Source for Bytes<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        self.0.read_at(offset, buf)
    }
}

impl Source for Vec<u8> {
    fn len(&self) -> usize {
        <[u8]>::len(self.as_slice())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        self.as_slice().read_at(offset, buf)
    }
}

/// One `PT_LOAD` segment: `filesz` file bytes from `offset` placed at `vaddr`,
/// zero-filled out to `memsz`, with the permissions the program header asked
/// for.
///
/// The bytes are *not* held here. They are in the [`Image`]'s source at
/// `offset`, and the loader reads the one page it is placing. Holding them was
/// the whole-file allocation a large program cannot survive.
pub struct Segment {
    pub vaddr: usize,
    /// File offset of the segment's first byte (`p_offset`).
    pub offset: usize,
    /// File bytes the segment carries (`p_filesz`) - the part of the segment
    /// that is not .bss.
    pub filesz: usize,
    /// How far the segment runs in memory (`p_memsz`), never shorter than
    /// [`Segment::filesz`]; the tail is what .bss is.
    pub memsz: usize,
    pub flags: MapFlags,
}

/// First address past the user half: the bound every image placement and every
/// `mmap`/`mremap` address is validated against. An address at or above it is
/// kernel territory, and no loader or mapping should honour it.
///
/// It is the *machine's* span, and the two machines disagree - which is why it
/// is declared here the same way [`MMAP_BASE`] is, one declaration per machine
/// with the reason written down, rather than as one shared `1 << 47`:
///
/// - x86_64: bit 47 decides the half, so the canonical lower half ends at
///   `1 << 47`.
/// - aarch64: the user span is `TTBR0_EL1`'s own size, `1 << 48` (the
///   machine's `paging::USER_END`), and a shared `1 << 47` is not conservative
///   here - it is *wrong*, and measurably so: `MMAP_BASE` on this machine is
///   `0xff00_0000_0000`, **above** `1 << 47`, so the `mmap` cursor hands out
///   addresses this constant called kernel territory. `handle_mmap` then
///   answered `EINVAL` to every `MAP_FIXED` mapping at such an address - which
///   is exactly the mapping a dynamic loader makes for a shared library's
///   segments, so the first `dlopen` on the second machine failed with "Error
///   loading shared library librc.so.1: Invalid argument" while the statically
///   linked programs, whose mappings are never fixed, ran fine. (`NOTES.md` §9.1
///   named this follow-on when it wrote down that the value was still x86_64's.)
/// - host: the x86_64 value, so the host-tested placement math matches x86.
///
/// `mm` declares it rather than reading it through `hal`: `mm`'s only edge is
/// down to `boot`, and a new `mm -> hal` or `mm -> arch` edge would be a design
/// change the modules do not otherwise need. The fact is one the mapping plan
/// owns - [`MMAP_BASE`] sits beside it for the same reason - and `paging::USER_END`
/// is the page tables' own copy of the same number.
#[cfg(any(not(target_arch = "aarch64"), not(target_os = "none")))]
pub const USER_LIMIT: usize = 1 << 47;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub const USER_LIMIT: usize = 1 << 48;

/// The ceiling of the `mmap`/image ASLR band: `sched`'s `new_layout` stacks the
/// `mmap` region, the interpreter and the program *below* this, and leaves the
/// region from here to the top of the address space to the stack. It is a
/// per-architecture address-space layout fact - which is why it lives in `mm`
/// (which owns the mapping plan and `USER_LIMIT` above) and not in `syscall`,
/// whose dispatch it has nothing to do with; naming `crate::syscall::MMAP_BASE`
/// from `sched` was a recorded `sched -> syscall` edge blocking the aarch64
/// un-gate (`NOTES.md` §9.1, §11.2).
///
/// - x86_64: `0x7f00_0000_0000`, the classical Linux mmap base, below the
///   `1 << 47` canonical line with a terabyte of stack headroom above it.
/// - aarch64: the same ratio taken against `paging::USER_END` (`1 << 48`): a
///   base near the top with the headroom above it left to the stack. This is
///   *this* kernel's layout choice, chosen so a static-pie image places sanely;
///   the exact base Linux aarch64 uses (top-down, `TASK_SIZE`-relative) is a
///   stage-7 measurement against a running aarch64 process, not claimed here.
/// - host: the x86_64 value, so the host-tested placement math matches x86.
#[cfg(any(not(target_arch = "aarch64"), not(target_os = "none")))]
pub const MMAP_BASE: usize = 0x0000_7f00_0000_0000;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub const MMAP_BASE: usize = 0xff00_0000_0000;

/// The size of an `Elf64_Rela` record. The `elf` crate's table view reads
/// exactly this stride, so a `DT_RELAENT` claiming anything else is a file
/// this loader does not understand.
const RELA64_RECORD: usize = 24;

/// An ELF64 program header is a fixed 56-byte record.
const PHDR64_SIZE: usize = 56;

/// The largest piece of a file a parse pulls into memory at once: the header
/// and its program header table, the interpreter path, the dynamic entries, and
/// the RELA table. The program's own bytes are never among them - the loader
/// reads those one page at a time through [`Source`], which is what lets a 296
/// MB binary load on a heap whose largest block is 64 MiB.
const META_MAX: usize = 1 << 20;
const INTERP_MAX: usize = 4096;
const DYNAMIC_MAX: usize = 64 << 10;
const RELA_MAX: usize = 4 << 20;

/// Read exactly `buf.len()` bytes, or nothing. A short read is a file that ends
/// where its own header said it did not.
fn read_exact(source: &dyn Source, offset: usize, buf: &mut [u8]) -> Option<()> {
    match source.read_at(offset, buf) {
        Some(count) if count == buf.len() => Some(()),
        _ => None,
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|f| u16::from_le_bytes([f[0], f[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|f| u32::from_le_bytes([f[0], f[1], f[2], f[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 8)
        .map(|f| u64::from_le_bytes(f.try_into().expect("eight bytes")))
}

/// Pull one bounded file range into memory, or `None` when the file cannot
/// answer for it inside `cap`.
fn read_range(source: &dyn Source, offset: u64, len: u64, cap: usize) -> Option<Vec<u8>> {
    let (offset, len) = (usize::try_from(offset).ok()?, usize::try_from(len).ok()?);
    if len > cap || offset.checked_add(len)? > source.len() {
        return None;
    }
    let mut bytes = alloc::vec![0u8; len];
    read_exact(source, offset, &mut bytes)?;
    Some(bytes)
}

/// Parse `source` as an executable image, or `None` if it is not one this
/// kernel is willing to load.
///
/// The header, the program header table and the two small tables a relocation
/// pass needs are the only bytes this function holds. Everything the crate
/// used to give us by parsing the *whole* file is read here, in bounded pieces
/// - the shape Linux's loader has, and the one a large program requires.
pub fn parse(source: &dyn Source) -> Option<Image<'_>> {
    let mut header = [0u8; 64];
    if source.len() < header.len() {
        return None;
    }
    read_exact(source, 0, &mut header)?;
    if header[0..4] != [0x7f, b'E', b'L', b'F'] {
        return None;
    }
    // ELFCLASS64, little endian, and the version this module understands.
    if header[4] != 2 || header[5] != 1 || header[6] != 1 {
        return None;
    }
    let e_type = u16_at(&header, 16)?;
    let machine = u16_at(&header, 18)?;
    // Which machine the file is built for decides which relative-relocation
    // kind this loader applies: `R_X86_64_RELATIVE` (8) and
    // `R_AARCH64_RELATIVE` (1027) mean the same thing but are numbered per
    // psABI. An unvalidated loader would accept an image for a foreign ISA and
    // read none of its relocations, placing a program that faults on its first
    // `.got` access. The parser accepts both because it is bytes-in,
    // image-out and host-testable for both; which machine the kernel may run is
    // the caller's check.
    let relative = match machine {
        EM_X86_64 => R_X86_64_RELATIVE,
        EM_AARCH64 => R_AARCH64_RELATIVE,
        _ => return None,
    };
    // Accept ET_EXEC (fixed-address) and ET_DYN (position independent).
    if e_type != ET_EXEC && e_type != ET_DYN {
        return None;
    }
    let entry = u64_at(&header, 24)?;
    let phoff = u64_at(&header, 32)?;
    let phentsize = u16_at(&header, 54)? as usize;
    let phnum = u16_at(&header, 56)? as usize;
    // Program headers are a fixed 56-byte record in ELF64; anything else is a
    // table this loader does not know how to walk.
    if phentsize != PHDR64_SIZE {
        return None;
    }
    let table_len = phnum.checked_mul(PHDR64_SIZE)?;
    let table_end = usize::try_from(phoff).ok()?.checked_add(table_len)?;
    if table_end > source.len() || table_len > META_MAX {
        return None;
    }
    let mut table = alloc::vec![0u8; table_len];
    read_exact(source, usize::try_from(phoff).ok()?, &mut table)?;

    let mut segments = Vec::new();
    let mut dyn_range: Option<(u64, u64)> = None;
    let mut interp_range: Option<(u64, u64)> = None;
    for index in 0..phnum {
        let phdr = &table[index * PHDR64_SIZE..(index + 1) * PHDR64_SIZE];
        let p_type = u32_at(phdr, 0)?;
        let p_flags = u32_at(phdr, 4)?;
        let p_offset = u64_at(phdr, 8)?;
        let p_vaddr = u64_at(phdr, 16)?;
        let p_filesz = u64_at(phdr, 32)?;
        let p_memsz = u64_at(phdr, 40)?;
        if p_type == PT_DYNAMIC {
            // The dynamic entries the relocation pass reads below come from
            // the PT_DYNAMIC segment, the loader's authoritative source.
            dyn_range = Some((p_offset, p_filesz));
        }
        if p_type == PT_INTERP {
            // A NUL-terminated path to the interpreter. More than one such
            // segment is malformed, and the first is as good an answer as any.
            interp_range = interp_range.or(Some((p_offset, p_filesz)));
        }
        if p_type != PT_LOAD {
            continue;
        }
        let vaddr = usize::try_from(p_vaddr).ok()?;
        let filesz = usize::try_from(p_filesz).ok()?;
        let memsz = usize::try_from(p_memsz).ok()?;
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
        // The file range has to be inside the file. A segment claiming bytes
        // past its end is a file that cannot be loaded, not a short read to
        // paper over later.
        let offset = usize::try_from(p_offset).ok()?;
        if offset.checked_add(filesz)? > source.len() {
            return None;
        }
        // READ needs no bit: a present page is readable, as on the wire.
        let mut segment_flags = MapFlags::READ;
        if p_flags & PF_W != 0 {
            segment_flags |= MapFlags::WRITE;
        }
        if p_flags & PF_X != 0 {
            segment_flags |= MapFlags::EXEC;
        }
        if memsz > 0 {
            segments.push(Segment {
                vaddr,
                offset,
                filesz,
                memsz,
                flags: segment_flags,
            });
        }
    }

    if usize::try_from(entry).ok()? >= USER_LIMIT {
        return None;
    }

    // For static-pie ET_DYN binaries the dynamic section lists RELA entries
    // that the runtime linker would process. We only handle the one
    // relative-relocation kind this machine's psABI names (write `base+addend`
    // at `offset`). A table the file names but cannot answer for refuses the
    // whole image: placing it without its relocations is a program that runs
    // and then writes into the layout it was linked for.
    let rela = match dyn_range {
        Some((offset, len)) => match read_range(source, offset, len, DYNAMIC_MAX) {
            Some(dyn_data) => extract_rela(source, &segments, &dyn_data, relative)?,
            // A dynamic segment larger than the cap is a file whose own
            // metadata is unreasonable; refuse rather than silently drop the
            // relocations it names.
            None if len > DYNAMIC_MAX as u64 => return None,
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    // Compute the virtual address of the program header table. For both
    // ET_EXEC and ET_DYN the table lives inside the first PT_LOAD segment
    // (file offset 0), so the virtual address is base + e_phoff.
    let phdr_addr = segments.first().map_or(0, |segment| {
        segment.vaddr + usize::try_from(phoff).unwrap_or(0)
    });
    // The path loses its terminator here and keeps nothing else: the caller
    // hands it to the filesystem, which wants the name, not the padding.
    let interp = interp_range
        .and_then(|(offset, len)| read_range(source, offset, len, INTERP_MAX))
        .and_then(|bytes| {
            let end = bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(bytes.len());
            (!bytes[..end].is_empty()).then(|| Vec::from(&bytes[..end]))
        });

    // Every relocation *the kernel* will apply has to land inside something this
    // image actually mapped. `DT_RELA` is the one table read without checking it
    // against the segments, so an entry outside them hands `apply_rela` an
    // address the file chose and the kernel never placed - a write beside the
    // program rather than into it.
    //
    // Asked of `interp` rather than of `PT_INTERP` itself, because that is the
    // question the loader asks: the relocation pass runs when there is no
    // interpreter image to hand the job to. Measured against `memsz`, not
    // `filesz`: a `R_*_RELATIVE` most often lands in `.got` or `.data.rel.ro`,
    // and the tail of the last segment is `.bss`, which has no file bytes.
    if interp.is_none()
        && rela.iter().any(|&(offset, _)| {
            !segments.iter().any(|segment| {
                segment.vaddr <= offset
                    && offset
                        .checked_add(core::mem::size_of::<u64>())
                        .is_some_and(|end| end <= segment.vaddr + segment.memsz)
            })
        })
    {
        return None;
    }

    Some(Image {
        entry: usize::try_from(entry).ok()?,
        segments,
        phdr_addr,
        // Only a position-independent image may be placed elsewhere: the
        // addresses an `ET_EXEC` names are the ones its own code refers to.
        pie: e_type == ET_DYN,
        machine,
        phnum,
        phent: phentsize,
        rela,
        interp,
        source,
    })
}

/// Convert a virtual address to a file offset by finding the `PT_LOAD` segment
/// that contains it. `None` if the address is in no segment, or in one's
/// zero-filled tail (which has no file bytes behind it).
fn vaddr_to_offset(segments: &[Segment], vaddr: usize) -> Option<usize> {
    for segment in segments {
        if vaddr >= segment.vaddr && vaddr < segment.vaddr + segment.memsz {
            let within = vaddr - segment.vaddr;
            if within >= segment.filesz {
                return None;
            }
            return Some(segment.offset + within);
        }
    }
    None
}

/// Read the DYNAMIC section and extract this machine's relative relocations.
///
/// `relative` is the `R_*_RELATIVE` number for the file's `e_machine`, chosen in
/// `parse`. `None` is a table the file names but cannot answer for - the caller
/// refuses the image, because an image placed without its relocations runs and
/// then writes into the layout it was linked for.
fn extract_rela(
    source: &dyn Source,
    segments: &[Segment],
    dyn_data: &[u8],
    relative: u32,
) -> Option<Vec<(usize, i64)>> {
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
        return Some(Vec::new());
    };
    if rela_size == 0 {
        return Some(Vec::new());
    }
    if let Some(ent) = rela_ent
        && ent as usize != RELA64_RECORD
    {
        return Some(Vec::new());
    }

    // Relocate the RELA table's virtual address into the file view, then pull
    // that one range: it is the last thing a parse holds, and it is bounded.
    let Some(file_offset) = vaddr_to_offset(segments, rela_vaddr as usize) else {
        return Some(Vec::new());
    };
    let rela_bytes = read_range(source, file_offset as u64, rela_size, RELA_MAX)?;

    let table = ParsingTable::<LittleEndian, Rela>::new(LittleEndian, Class::ELF64, &rela_bytes);
    Some(
        table
            .iter()
            .filter(|rela| rela.r_type == relative)
            .map(|rela| (rela.r_offset as usize, rela.r_addend))
            .collect(),
    )
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
        // e_machine: EM_X86_64. The loader now refuses an unknown machine, so the
        // helper's default image declares x86_64; the aarch64 and foreign-machine
        // tests overwrite these two bytes.
        out[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
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
        let mut carried = alloc::vec![0u8; segment.filesz];
        image
            .source
            .read_at(segment.offset, &mut carried)
            .expect("the image's source answers for its own bytes");
        assert_eq!(carried, b"program bytes");
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
        assert!(parse(&good[..40].to_vec()).is_none(), "truncated header");
    }

    #[test]
    fn a_segment_past_the_lower_half_is_refused() {
        let bytes = elf(0x1000, &[(PT_LOAD, 6, (1 << 47) + 0x1000, 0x100, b"x")]);
        assert!(parse(&bytes).is_none(), "the kernel half is not loadable");
    }

    /// The one bit of the header that decides where the loader may put the
    /// image. `ET_EXEC` names addresses its own instructions refer to, so it is
    /// placed where it says; `ET_DYN` carries relocations precisely so that it
    /// can be placed anywhere, which is what makes per-process randomisation
    /// possible at all - and applying it to the first kind would produce a
    /// program that cannot run rather than one that is hard to attack.
    #[test]
    fn only_a_position_independent_image_moved_from_its_named_addresses() {
        let bytes = elf(0x400_000, &[(PT_LOAD, 5, 0x400_000, 0x100, b"code")]);
        assert!(!parse(&bytes).expect("parses").pie, "ET_EXEC");

        let mut dyn_bytes = bytes.clone();
        dyn_bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        assert!(parse(&dyn_bytes).expect("parses").pie, "ET_DYN");
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
        // it starts at the segment. The addresses it relocates are in a second
        // one - which is where a real static-pie puts them, in `.data` rather
        // than beside the table that describes them.
        let mut table = Vec::new();
        table.extend_from_slice(&[0u8; 16]);
        table.extend_from_slice(&rela(0x3010, 8, 0x1234)); // R_X86_64_RELATIVE
        table.extend_from_slice(&rela(0x9000, 1, 0x99)); // some other kind
        table.extend_from_slice(&rela(0x3020, 8, -8)); // relative again

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
                (PT_LOAD, 6, 0x3000, 0x100, b"data"),
            ],
        );
        let image = parse(&bytes).expect("parses");
        assert_eq!(image.rela, vec![(0x3010, 0x1234), (0x3020, -8)]);
    }

    /// A `R_X86_64_RELATIVE` whose target is inside no loaded segment is a
    /// write to an address the image named and the kernel never placed, so the
    /// loader would apply it beside the program - into whatever else is mapped
    /// there. Nothing in an ELF link map can produce this, which is the point
    /// of refusing the file rather than skipping the entry: a table that
    /// claims it is a table this image is not.
    #[test]
    fn a_relocation_target_outside_every_segment_refuses_the_image() {
        let mut table = Vec::new();
        table.extend_from_slice(&rela(0x9000, 8, 0x1234));

        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&dyn_entry(7, 0x1000)); // DT_RELA
        dynamic.extend_from_slice(&dyn_entry(8, 24)); // DT_RELASZ
        dynamic.extend_from_slice(&dyn_entry(9, 24)); // DT_RELAENT
        dynamic.extend_from_slice(&dyn_entry(0, 0));

        let bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x100, &table),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
            ],
        );
        assert!(parse(&bytes).is_none(), "0x9000 is in no segment");

        // The entry has to fit its segment rather than merely start in it: a
        // target four bytes from the end would put half of the `u64` outside,
        // and the tail of the last segment is the one place that happens.
        let mut edge = Vec::new();
        edge.extend_from_slice(&rela(0x1000 + 0x100 - 4, 8, 0x1234));
        let dynamic_at_edge = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x100, &edge),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
            ],
        );
        assert!(
            parse(&dynamic_at_edge).is_none(),
            "a relocation that runs past the end of its segment"
        );
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

    /// The machine the file names reports itself, for the caller's arch check.
    #[test]
    fn an_x86_64_image_reports_its_machine() {
        let bytes = elf(0x1000, &[(PT_LOAD, 6, 0x1000, 0x100, b"x")]);
        assert_eq!(parse(&bytes).expect("parses").machine, 62);
    }

    /// Aarch64's relative relocations are read, not dropped. The whole point: a
    /// filter still pinned to x86_64's type 8 would return an empty `rela` for a
    /// file whose relocations are type 1027, and a loader that never checked
    /// `e_machine` would load that aarch64 image as if it were x86_64 - a broken
    /// `.got`, not an error. This test is the one that fails on either mistake.
    #[test]
    fn an_aarch64_image_gets_its_relative_relocations_read() {
        const EM_AARCH64: u16 = 183;
        const R_AARCH64_RELATIVE: u32 = 1027;

        let mut table = Vec::new();
        table.extend_from_slice(&[0u8; 16]);
        table.extend_from_slice(&rela(0x3010, R_AARCH64_RELATIVE, 0x1234));
        table.extend_from_slice(&rela(0x9000, 1, 0x99)); // some other kind
        table.extend_from_slice(&rela(0x3020, R_AARCH64_RELATIVE, -8));

        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&dyn_entry(7, 0x1000 + 16)); // DT_RELA
        dynamic.extend_from_slice(&dyn_entry(8, 72)); // DT_RELASZ
        dynamic.extend_from_slice(&dyn_entry(9, 24)); // DT_RELAENT
        dynamic.extend_from_slice(&dyn_entry(0, 0)); // DT_NULL

        let mut bytes = elf(
            0x1000,
            &[
                (PT_LOAD, 6, 0x1000, 0x200, &table),
                (PT_DYNAMIC, 6, 0x2000, 0x100, &dynamic),
                (PT_LOAD, 6, 0x3000, 0x100, b"data"),
            ],
        );
        bytes[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());

        let image = parse(&bytes).expect("an aarch64 image parses");
        assert_eq!(image.machine, EM_AARCH64);
        assert_eq!(image.rela, vec![(0x3010, 0x1234), (0x3020, -8)]);
    }

    /// A file for a machine this kernel knows nothing about is refused, not
    /// loaded and relocated by the wrong psABI's rules.
    #[test]
    fn an_image_for_an_unknown_machine_is_refused() {
        const EM_RISCV: u16 = 243;
        let mut bytes = elf(0x1000, &[(PT_LOAD, 6, 0x1000, 0x100, b"x")]);
        bytes[18..20].copy_from_slice(&EM_RISCV.to_le_bytes());
        assert!(parse(&bytes).is_none(), "an unknown machine is refused");
    }

    /// `e_machine` 0 is "no machine named", which the old accept-anything loader
    /// would have taken. It is a malformed image for the purpose of loading.
    #[test]
    fn a_file_that_names_no_machine_is_refused() {
        let mut bytes = elf(0x1000, &[(PT_LOAD, 6, 0x1000, 0x100, b"x")]);
        bytes[18..20].copy_from_slice(&0u16.to_le_bytes());
        assert!(
            parse(&bytes).is_none(),
            "e_machine 0 is not a known machine"
        );
    }
}
