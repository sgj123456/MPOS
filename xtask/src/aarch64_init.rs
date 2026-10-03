//! The aarch64 `/init`: the smallest real userspace program, built in pure Rust.
//!
//! This is stage 6's image prerequisite. The kernel's ELF loader (`src/mm/elf.rs`)
//! was taught `EM_AARCH64` in 6g, and `spawn_init`/`load_user` map the segments
//! and jump to `e_entry` - but nothing had yet *produced* an aarch64 binary for
//! the initramfs the loader hands to the kernel. This file does.
//!
//! It is deliberately hand-encoded rather than built with a cross compiler. Two
//! reasons, both the house's: a build step that shells out to `clang` plus a
//! rustup-internal `ld.lld` path would add a fragile host prerequisite to the
//! boot path and make the artifact non-hermetic (a test that has to run the
//! compiler to know the answer tests the compiler, not the image). And the
//! program is four instructions, so the whole thing fits in a page and every byte
//! is pinned by hand against the assembler's own output.
//!
//! What it does is exactly one thing: `exit_group(0)`. That is enough to prove the
//! round trip the port is missing - `spawn_init` loads it, the scheduler enters it
//! at EL0, its `svc` traps to the gate, the gate routes it into the shared
//! `syscall::dispatch` table (because the scheduler has a current task by then),
//! and `proc::handle_exit_group` reaps it - without needing a console write first,
//! which would require the serial-RX / `/dev` / tty plumbing that is the drivers
//! stage (`PLAN.md` §9.1 stage 7).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// `EM_AARCH64` - the value the kernel's loader matches on.
const EM_AARCH64: u16 = 183;
/// `ET_EXEC`: a fixed-address image, not a `PIE`. The loader places it exactly at
/// the addresses its own segments name; `e_entry` is where execution begins.
const ET_EXEC: u16 = 2;
/// `PT_LOAD`.
const PT_LOAD: u32 = 1;
/// `PF_R | PF_X` - the one mapped segment holds the headers and the code.
const PF_R_X: u32 = 5;

/// Where the image is mapped: the low text address a static aarch64 executable
/// uses, comfortably inside the user span and below `USER_LIMIT`.
const VADDR: u64 = 0x40_0000;

/// The `_start` instructions, each word verified against `llvm-objdump` from the
/// same source an assembler produced, so these bytes are not a guess at the
/// encoding:
///   `mov x8, #94`  = exit_group (the aarch64 asm-generic number)
///   `mov x0, #0`   = status 0
///   `svc #0`       = trap to the kernel
///   `b .`          = if the kernel ever returned, park rather than fall through
const CODE: [u8; 16] = [
    0xc8, 0x0b, 0x80, 0xd2, // 0xd2800bc8
    0x00, 0x00, 0x80, 0xd2, // 0xd2800000
    0x01, 0x00, 0x00, 0xd4, // 0xd4000001
    0x00, 0x00, 0x00, 0x14, // 0x14000000
];

/// The one `PT_LOAD` program header and the ELF64 header around it, assembled the
/// way the kernel's `elf::parse` expects: a statically linked `ET_EXEC` whose
/// first segment covers the whole file (`p_offset == 0`), so header and code live
/// in one page and `p_offset` is congruent to `p_vaddr` modulo the page. The code
/// begins at file offset `HEADER_LEN` and so at `VADDR + HEADER_LEN`.
pub fn minimal_init_elf() -> Vec<u8> {
    const HEADER_LEN: usize = 64;
    const PHDR_LEN: usize = 56;
    let code_off = HEADER_LEN + PHDR_LEN; // 120
    let entry = VADDR + code_off as u64;
    let file_len = code_off + CODE.len(); // 136

    let mut out = vec![0u8; file_len];
    // e_ident: magic, class 64, little-endian, current version.
    out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    // e_type / e_machine.
    out[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    out[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
    // e_version, e_flags default 0.
    out[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
    out[32..40].copy_from_slice(&(HEADER_LEN as u64).to_le_bytes()); // e_phoff
    // e_shoff, e_shentsize, e_shnum, e_shstrndx all 0 (no section headers).
    out[52..54].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes()); // e_phentsize
    out[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    // The single program header, covering the whole file.
    let ph = HEADER_LEN;
    out[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes()); // p_type
    out[ph + 4..ph + 8].copy_from_slice(&PF_R_X.to_le_bytes()); // p_flags
    out[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    out[ph + 16..ph + 24].copy_from_slice(&VADDR.to_le_bytes()); // p_vaddr
    out[ph + 24..ph + 32].copy_from_slice(&VADDR.to_le_bytes()); // p_paddr
    out[ph + 32..ph + 40].copy_from_slice(&(file_len as u64).to_le_bytes()); // p_filesz
    out[ph + 40..ph + 48].copy_from_slice(&(file_len as u64).to_le_bytes()); // p_memsz
    out[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    out[code_off..].copy_from_slice(&CODE);
    out
}

/// Pack the `/init` into a ustar archive named `initramfs.tar` at `out`, the way
/// `xtask/src/initramfs.rs` does for the x86 image: stage the file in a directory
/// and let the system `tar --format=ustar` write it. The kernel unpacks it with
/// `vfs::tar::unpack_into` and execs `/init` from the ramfs root it fills.
pub fn write_initramfs(staging_parent: &Path, out: &Path) -> Result<()> {
    let stage = staging_parent.join("aarch64-init-stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .with_context(|| format!("Failed to create {}", stage.display()))?;
    std::fs::write(stage.join("init"), minimal_init_elf())
        .with_context(|| format!("Failed to write {}/init", stage.display()))?;

    let status = Command::new("tar")
        .arg("--format=ustar")
        .args(["-C", &stage.display().to_string()])
        .args(["-cf", &out.display().to_string()])
        .arg("init")
        .status()
        .context("Failed to start tar to build the aarch64 initramfs")?;
    if !status.success() {
        anyhow::bail!("tar failed building the aarch64 initramfs: {status}");
    }
    let _ = std::fs::remove_dir_all(&stage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes are a static `ET_EXEC` `EM_AARCH64` image whose single `PT_LOAD`
    /// the kernel's loader reads: header sanity, the entry inside the mapped
    /// segment, and the code words are exactly the `exit_group(0)` sequence. This
    /// is the pin that fails if the encoding or layout drifts, checked against the
    /// assembler's own words rather than a re-derivation.
    #[test]
    fn the_minimal_init_is_a_loadable_aarch64_exit_group() {
        let e = minimal_init_elf();
        assert_eq!(&e[0..4], &[0x7f, b'E', b'L', b'F']);
        assert_eq!(e[4], 2, "ELFCLASS64");
        assert_eq!(e[5], 1, "ELFDATA2LSB");
        assert_eq!(u16::from_le_bytes([e[16], e[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([e[18], e[19]]), EM_AARCH64);
        assert_eq!(u32::from_le_bytes([e[54], e[55], 0, 0]), 56, "e_phentsize");
        assert_eq!(u16::from_le_bytes([e[56], e[57]]), 1, "e_phnum");
        let entry = u64::from_le_bytes(e[24..32].try_into().unwrap());
        assert_eq!(entry, VADDR + 120, "entry is the code, inside the segment");

        // The single program header covers the whole file from offset 0.
        let ph = 64;
        assert_eq!(
            u32::from_le_bytes([e[ph], e[ph + 1], e[ph + 2], e[ph + 3]]),
            PT_LOAD
        );
        assert_eq!(
            u64::from_le_bytes(e[ph + 8..ph + 16].try_into().unwrap()),
            0,
            "p_offset"
        );
        assert_eq!(
            u64::from_le_bytes(e[ph + 16..ph + 24].try_into().unwrap()),
            VADDR,
            "p_vaddr"
        );
        assert_eq!(
            u64::from_le_bytes(e[ph + 32..ph + 40].try_into().unwrap()),
            e.len() as u64,
            "p_filesz"
        );

        // The four instructions, as 32-bit little-endian words, are the verified
        // exit_group(0) encoding - not a hand-guessed bit layout.
        let words: Vec<u32> = e[120..136]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            vec![0xd2800bc8, 0xd2800000, 0xd4000001, 0x14000000],
            "mov x8,#94 / mov x0,#0 / svc #0 / b ."
        );
    }
}
