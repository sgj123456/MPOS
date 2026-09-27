#![no_main]

//! The ELF loader's decision about what it is willing to place.
//!
//! `mm::elf::parse` is the kernel's read of an untrusted file, and every
//! address it returns is one the loader then maps. The invariants asserted
//! here are the ones the module documents: an image is a lower-half thing
//! (`USER_LIMIT`), and a segment's memory run cannot be shorter than the file
//! bytes inside it. A violation is not a refusal to load - it is the loader
//! being handed an address it will trust.

use libfuzzer_sys::fuzz_target;
use mpos::mm::elf;

/// First address past the lower half, mirrored from the private constant in
/// `mm/elf.rs`: bit 47 is what separates a user address from a kernel one.
const USER_LIMIT: usize = 1 << 47;

fuzz_target!(|data: &[u8]| {
    let Some(image) = elf::parse(data) else {
        return;
    };

    assert!(
        image.entry < USER_LIMIT,
        "entry {:#x} is in the kernel half",
        image.entry
    );
    for segment in &image.segments {
        assert!(
            segment.vaddr < USER_LIMIT,
            "segment at {:#x} is in the kernel half",
            segment.vaddr
        );
        assert!(
            segment.memsz >= segment.data.len(),
            "segment at {:#x} maps {} bytes of file into {} of memory",
            segment.vaddr,
            segment.data.len(),
            segment.memsz
        );
        assert!(
            segment
                .vaddr
                .checked_add(segment.memsz)
                .is_some_and(|end| end <= USER_LIMIT),
            "segment at {:#x} of {} runs past the lower half",
            segment.vaddr,
            segment.memsz
        );
    }

    // Every relocation *the kernel* will apply has to land inside something it
    // mapped; a target outside the image is a write to an arbitrary address.
    // That is the static case only: an image with an interpreter is relocated
    // by that interpreter, and `runtime.rs` skips its own pass for it, so a
    // table that only the interpreter can make sense of is not a refusal here.
    if image.interp.is_none() {
        for &(offset, _) in &image.rela {
            assert!(
                offset < USER_LIMIT,
                "relocation target {:#x} is in the kernel half",
                offset
            );
            assert!(
                image.segments.iter().any(|s| {
                    s.vaddr <= offset && offset + core::mem::size_of::<u64>() <= s.vaddr + s.memsz
                }),
                "relocation target {:#x} is outside every loaded segment",
                offset
            );
        }
    }
});
