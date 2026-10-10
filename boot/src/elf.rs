//! Just enough ELF64 to load the kernel: its program headers, its dynamic
//! relocations and four section boundaries.
//!
//! Hand-written rather than pulled from the `elf` crate because the loader
//! parses exactly one image, that image is this project's own kernel, and the
//! failure mode that matters is a clear panic - "segment 3 reaches past the end
//! of the image" - rather than a typed error nobody can act on. Every read goes
//! through [`Reader`], which bounds-checks against the file and panics with the
//! offset it refused.

use alloc::vec::Vec;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

const SHT_NOBITS: u32 = 8;

const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;

/// The one relocation kind a static PIE against itself needs, per machine: the
/// image's own absolute addresses, adjusted by the load base. The other types
/// all name a symbol, and a kernel with no shared libraries has none to name.
///
/// The numbers are `ELF`'s, not this project's: `R_X86_64_RELATIVE` is 8 in the
/// x86_64 psABI and `R_AARCH64_RELATIVE` is 1027 in the aarch64 one, and the two
/// machines disagree because the numbers were assigned by different processor
/// supplements. Which one is expected comes from the file's own `e_machine`, so
/// a loader built for one machine still refuses the other's image by name.
const R_X86_64_RELATIVE: u32 = 8;
const R_AARCH64_RELATIVE: u32 = 1027;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

/// One `PT_LOAD` segment, as the loader has to place it.
#[derive(Clone, Copy)]
pub struct Segment {
    pub file_offset: u64,
    pub vaddr: u64,
    pub file_size: u64,
    pub mem_size: u64,
}

/// One dynamic relocation, resolved.
#[derive(Clone, Copy)]
pub struct Relocation {
    pub vaddr: u64,
    pub addend: u64,
}

/// A virtual address range, as the kernel's own linker symbols describe it.
#[derive(Clone, Copy)]
pub struct Range {
    pub start: u64,
    pub end: u64,
}

/// A parsed kernel image. Borrows the bytes it was parsed from - the loader's
/// decompressed copy of the payload it carries - so those bytes have to outlive
/// it, which they do, being an allocation of the loader's that is never freed.
pub struct Kernel<'a> {
    bytes: &'a [u8],
    /// The relocation kind this image's machine uses for a self-relative fixup,
    /// taken from `e_machine`. Held rather than looked up per entry so that the
    /// check in [`Kernel::read_relocations`] names the machine it expected.
    relative: u32,
    pub entry: u64,
    /// Lowest virtual address any segment uses. Zero for a normal link, since
    /// the linker script starts the first loadable segment at file offset 0.
    pub base: u64,
    /// Bytes the image occupies, from `base`, rounded up to whole pages.
    pub span: u64,
    pub segments: Vec<Segment>,
    pub relocations: Vec<Relocation>,
    /// Executable code, as the linker script laid it out.
    pub text: Range,
    /// Constants.
    pub rodata: Range,
    /// Everything writable, from the start of `.data` to the end of `.bss`.
    pub data: Range,
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn slice(&self, at: u64, len: u64) -> &'a [u8] {
        let start = usize::try_from(at).expect("an offset beyond the address space");
        let len = usize::try_from(len).expect("a length beyond the address space");
        let end = start
            .checked_add(len)
            .expect("a range that wraps the address space");
        self.bytes.get(start..end).unwrap_or_else(|| {
            panic!(
                "the kernel image ends at {} bytes, so {}..{} is past its end",
                self.bytes.len(),
                start,
                end
            )
        })
    }

    fn u8(&self, at: u64) -> u8 {
        self.slice(at, 1)[0]
    }

    fn u16(&self, at: u64) -> u16 {
        u16::from_le_bytes(self.slice(at, 2).try_into().unwrap())
    }

    fn u32(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.slice(at, 4).try_into().unwrap())
    }

    fn u64(&self, at: u64) -> u64 {
        u64::from_le_bytes(self.slice(at, 8).try_into().unwrap())
    }

    fn i64(&self, at: u64) -> i64 {
        i64::from_le_bytes(self.slice(at, 8).try_into().unwrap())
    }
}

impl<'a> Kernel<'a> {
    pub fn parse(bytes: &'a [u8]) -> Kernel<'a> {
        let r = Reader { bytes };

        assert_eq!(
            r.slice(0, 4),
            ELF_MAGIC,
            "the kernel image does not begin with an ELF magic number"
        );
        assert_eq!(r.u8(4), ELFCLASS64, "the kernel image is not a 64-bit ELF file");
        // Both machines this loader serves, and no other: the two are the ones
        // a UEFI firmware can hand it, and an image built for a third would be
        // one whose page-table format the loader below does not speak.
        let machine = r.u16(18);
        assert!(
            machine == EM_X86_64 || machine == EM_AARCH64,
            "the kernel image was built for machine {machine}, not x86_64 ({EM_X86_64}) \
             or aarch64 ({EM_AARCH64})"
        );
        let relative = if machine == EM_AARCH64 {
            R_AARCH64_RELATIVE
        } else {
            R_X86_64_RELATIVE
        };

        let entry = r.u64(24);
        let phoff = r.u64(32);
        let shoff = r.u64(40);
        let phentsize = r.u16(54) as u64;
        let phnum = r.u16(56) as u64;
        let shentsize = r.u16(58) as u64;
        let shnum = r.u16(60) as u64;
        let shstrndx = r.u16(62) as u64;

        // Program headers: the loadable segments, and the dynamic array that
        // names the relocations.
        let mut segments = Vec::new();
        let mut dynamic = None;
        let mut lowest = u64::MAX;
        let mut highest = 0u64;
        for index in 0..phnum {
            let at = phoff + index * phentsize;
            let kind = r.u32(at);
            match kind {
                PT_LOAD => {
                    let segment = Segment {
                        file_offset: r.u64(at + 8),
                        vaddr: r.u64(at + 16),
                        file_size: r.u64(at + 32),
                        mem_size: r.u64(at + 40),
                    };
                    // The copy below reads file_size bytes out of the file, so
                    // the segment has to have them.
                    r.slice(segment.file_offset, segment.file_size);
                    lowest = lowest.min(segment.vaddr);
                    highest = highest.max(segment.vaddr + segment.mem_size);
                    segments.push(segment);
                }
                PT_DYNAMIC => dynamic = Some((r.u64(at + 8), r.u64(at + 32))),
                _ => {}
            }
        }
        assert!(!segments.is_empty(), "the kernel image has no loadable segments");
        assert!(
            lowest <= entry && entry < highest,
            "the kernel image's entry point is not inside it"
        );

        let mut kernel = Kernel {
            bytes,
            relative,
            entry: entry - lowest,
            base: lowest,
            span: (highest - lowest).next_multiple_of(PAGE_SIZE),
            segments,
            relocations: Vec::new(),
            text: Range { start: 0, end: 0 },
            rodata: Range { start: 0, end: 0 },
            data: Range { start: 0, end: 0 },
        };

        kernel.relocations = kernel.read_relocations(dynamic);
        kernel.read_sections(&r, shoff, shentsize, shnum, shstrndx);
        kernel
    }

    /// Walk the dynamic array for `DT_RELA` and its size, then turn every entry
    /// into an addend to apply.
    ///
    /// The dynamic array is used rather than the `.rela.dyn` section header
    /// because it is what the kernel's own `PT_DYNAMIC` segment points at: it
    /// cannot drift if the linker script ever stops naming the section.
    fn read_relocations(&self, dynamic: Option<(u64, u64)>) -> Vec<Relocation> {
        let Some((at, len)) = dynamic else {
            return Vec::new();
        };
        let r = Reader { bytes: self.bytes };
        let at = self.to_file_offset(at);

        let mut rela = None;
        let mut rela_size = 0u64;
        let mut rela_ent = 24u64;
        let mut cursor = at;
        let end = at + len;
        while cursor + 16 <= end {
            let tag = r.i64(cursor);
            let value = r.u64(cursor + 8);
            cursor += 16;
            match tag {
                DT_NULL => break,
                DT_RELA => rela = Some(self.to_file_offset(value)),
                DT_RELASZ => rela_size = value,
                DT_RELAENT => rela_ent = value,
                _ => {}
            }
        }

        let Some(rela) = rela else {
            return Vec::new();
        };
        assert!(
            rela_ent >= 24,
            "the kernel image uses {rela_ent}-byte relocation entries, which are not the 24-byte form"
        );

        let mut out = Vec::new();
        for at in (rela..rela + rela_size).step_by(rela_ent as usize) {
            let offset = r.u64(at);
            let info = r.u64(at + 8);
            let addend = r.u64(at + 16);
            let kind = info as u32;
            let symbol = info >> 32;
            assert_eq!(
                kind, self.relative,
                "the kernel image carries a relocation of type {kind} against symbol {symbol}; \
                 only its machine's RELATIVE fixup ({}) is expected from a static PIE",
                self.relative,
            );
            out.push(Relocation {
                vaddr: offset,
                addend,
            });
        }
        out
    }

    /// The four boundaries the kernel maps its own image by, taken from the
    /// section headers so that the loader's own tables give the same W^X the
    /// kernel will install a moment later.
    fn read_sections(
        &mut self,
        r: &Reader<'a>,
        shoff: u64,
        shentsize: u64,
        shnum: u64,
        shstrndx: u64,
    ) {
        let names = {
            let at = shoff + shstrndx * shentsize;
            (r.u64(at + 24), r.u64(at + 32))
        };
        let names = r.slice(names.0, names.1);

        let find = |want: &str| -> Option<(u64, u64, u32)> {
            for index in 0..shnum {
                let at = shoff + index * shentsize;
                let name_at = r.u32(at) as u64;
                if name_at as usize >= names.len() {
                    continue;
                }
                let name = &names[name_at as usize..];
                let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                if name == want.as_bytes() {
                    return Some((r.u64(at + 16), r.u64(at + 32), r.u32(at + 4)));
                }
            }
            None
        };

        let text = find(".text").expect("the kernel image has no .text section");
        let rodata = find(".rodata").expect("the kernel image has no .rodata section");
        let data = find(".data").expect("the kernel image has no .data section");
        let bss = find(".bss");
        assert_ne!(
            rodata.2, SHT_NOBITS,
            ".rodata is uninitialised, which cannot be right"
        );

        self.text = Range {
            start: text.0,
            end: text.0 + text.1,
        };
        self.rodata = Range {
            start: rodata.0,
            end: rodata.0 + rodata.1,
        };
        // `.data`, `.dynamic` and `.bss` are consecutive, so the writable range
        // runs from the first to the end of the last. `.bss` is `SHT_NOBITS`:
        // it has an address and a size, and no bytes in the file.
        let end = bss.map_or(data.0 + data.1, |b| b.0 + b.1);
        self.data = Range { start: data.0, end };
    }

    /// Where in the file a virtual address's bytes are.
    fn to_file_offset(&self, vaddr: u64) -> u64 {
        for segment in &self.segments {
            if vaddr >= segment.vaddr && vaddr < segment.vaddr + segment.file_size {
                return segment.file_offset + (vaddr - segment.vaddr);
            }
        }
        panic!("the kernel image addresses {vaddr:#x} with no segment holding its bytes");
    }

    /// Copy the image into `dest` and apply the relocations for a load base of
    /// `virt_base`.
    ///
    /// # Safety
    ///
    /// `dest` must point at `span` bytes of writable memory that is not used for
    /// anything else, and it must stay mapped at the plain physical address the
    /// caller computed it at for as long as the copy takes - the loader does
    /// this while the firmware's identity map is still active.
    pub unsafe fn load(&self, dest: *mut u8, virt_base: u64) {
        for segment in &self.segments {
            let target = unsafe { dest.add((segment.vaddr - self.base) as usize) };
            let source = &self.bytes[segment.file_offset as usize..][..segment.file_size as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(source.as_ptr(), target, source.len());
                // What the file does not carry - `.bss` - is zero, which is
                // what the segment's own memory size promises.
                core::ptr::write_bytes(
                    target.add(source.len()),
                    0,
                    (segment.mem_size - segment.file_size) as usize,
                );
            }
        }

        for relocation in &self.relocations {
            let slot = unsafe { dest.add((relocation.vaddr - self.base) as usize) } as *mut u64;
            let value = virt_base.wrapping_add(relocation.addend);
            unsafe { core::ptr::write_unaligned(slot, value) };
        }
    }
}

/// The loader's own page size, and the kernel's: 4 KiB on both machines, which
/// is the granularity the memory map, the segments and the kernel's own
/// `paging::install` are all aligned to.
pub const PAGE_SIZE: u64 = 4096;
