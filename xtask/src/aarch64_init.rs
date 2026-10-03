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
//! What it does is one round trip that reaches the wire: `write(1, "MPOS_INIT_ON\n", 13)`
//! then `exit_group(0)`. That is the smallest program that proves a userspace image
//! on this machine performs a *real syscall that produces output* - the write traps
//! to the gate, is routed into `syscall::dispatch`, and the byte stream reaches the
//! console - not merely that a program can leave. It is the first line a real shell
//! would print. The write path needs no serial-RX / `/dev` / tty plumbing (that is
//! stage 7's input side); a console *write* rides the tty output the boot already
//! wired at stage 4b.

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
/// `PF_R | PF_W | PF_X` - the one mapped segment holds headers, code and the string
/// the program writes; `W` so the loader maps it as a plain data page.
const PF_R_W_X: u32 = 7;

/// Where the image is mapped: the low text address a static aarch64 executable
/// uses, comfortably inside the user span and below `USER_LIMIT`.
const VADDR: u64 = 0x40_0000;

/// The bytes the program writes to `fd 1`; the length the `write` passes is
/// `BANNER.len()`. A trailing newline so it prints on its own line.
const BANNER: &[u8] = b"MPOS_INIT_ON\n";

/// The `_start` instructions, each word verified by disassembling the built image
/// with `llvm-objdump` (encode -> disassemble -> confirm), so these bytes are not a
/// guess at the encoding:
///   `mov x8, #64`  = write   (aarch64 asm-generic number)
///   `mov x0, #1`   = fd stdout
///   `mov x1, #lo16(BANNER_ADDR)` ; `movk x1, #hi16, lsl 16` = pointer to the banner
///   `mov x2, #13`  = length
///   `svc #0`       = trap to the kernel -> routed into `syscall::dispatch`
///   `mov x8, #94` / `mov x0, #0` / `svc #0` = exit_group(0)
///   `b .`          = if the kernel ever returned, park rather than fall through
pub fn minimal_init_elf() -> Vec<u8> {
    const HEADER_LEN: usize = 64;
    const PHDR_LEN: usize = 56;
    const CODE_BYTES: usize = 40; // ten 32-bit instructions
    let code_off = HEADER_LEN + PHDR_LEN; // 120
    let banner_off = code_off + CODE_BYTES; // 160
    let banner_addr = VADDR + banner_off as u64;
    let lo = (banner_addr & 0xffff) as u32;
    let hi = ((banner_addr >> 16) & 0xffff) as u32;
    let entry = VADDR + code_off as u64;
    let file_len = banner_off + BANNER.len();

    let movz = |rd: u32, imm: u32, hw: u32| 0xd280_0000u32 | (hw << 21) | (imm << 5) | rd;
    let movk = |rd: u32, imm: u32, hw: u32| 0xf280_0000u32 | (hw << 21) | (imm << 5) | rd;
    let svc = || 0xd400_0001u32;
    let code: [u32; 10] = [
        movz(8, 64, 0),                  // mov x8, #64        (write)
        movz(0, 1, 0),                   // mov x0, #1         (stdout)
        movz(1, lo, 0),                  // mov x1, #lo16(banner)
        movk(1, hi, 1),                  // movk x1, #hi16, lsl #16
        movz(2, BANNER.len() as u32, 0), // mov x2, #len
        svc(),                           // svc #0
        movz(8, 94, 0),                  // mov x8, #94        (exit_group)
        movz(0, 0, 0),                   // mov x0, #0
        svc(),                           // svc #0
        0x1400_0000,                     // b .                (park)
    ];

    let mut out = vec![0u8; file_len];
    out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    out[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
    out[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
    out[32..40].copy_from_slice(&(HEADER_LEN as u64).to_le_bytes()); // e_phoff
    out[52..54].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes()); // e_phentsize
    out[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    let ph = HEADER_LEN;
    out[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes()); // p_type
    out[ph + 4..ph + 8].copy_from_slice(&PF_R_W_X.to_le_bytes()); // p_flags
    out[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    out[ph + 16..ph + 24].copy_from_slice(&VADDR.to_le_bytes()); // p_vaddr
    out[ph + 24..ph + 32].copy_from_slice(&VADDR.to_le_bytes()); // p_paddr
    out[ph + 32..ph + 40].copy_from_slice(&(file_len as u64).to_le_bytes()); // p_filesz
    out[ph + 40..ph + 48].copy_from_slice(&(file_len as u64).to_le_bytes()); // p_memsz
    out[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    for (i, word) in code.iter().enumerate() {
        out[code_off + i * 4..code_off + i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out[banner_off..].copy_from_slice(BANNER);
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
    /// segment, the code words are exactly the `write(1,banner); exit_group(0)`
    /// sequence, `x1` points at the banner that follows the code, and the banner
    /// bytes are present. The words and the address are checked against
    /// `llvm-objdump`'s own decoding of the same bytes, not a re-derivation.
    #[test]
    fn the_init_writes_a_banner_then_exits() {
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

        // The single program header covers the whole file from offset 0, RWX.
        let ph = 64;
        assert_eq!(
            u32::from_le_bytes([e[ph], e[ph + 1], e[ph + 2], e[ph + 3]]),
            PT_LOAD
        );
        assert_eq!(
            u32::from_le_bytes([e[ph + 4], e[ph + 5], e[ph + 6], e[ph + 7]]),
            PF_R_W_X,
            "p_flags RWX"
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

        // The ten instructions. `mov x1,#0xa0` + `movk x1,#0x40,lsl#16` build
        // 0x4000a0 = VADDR + banner_off; `mov x2,#13` is its length; 64 and 94 are
        // write / exit_group.
        let words: Vec<u32> = e[120..160]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            vec![
                0xd2800808, // mov x8,#64   (write)
                0xd2800020, // mov x0,#1    (stdout)
                0xd2801401, // mov x1,#0xa0
                0xf2a00801, // movk x1,#0x40,lsl #16
                0xd28001a2, // mov x2,#13
                0xd4000001, // svc #0
                0xd2800bc8, // mov x8,#94   (exit_group)
                0xd2800000, // mov x0,#0
                0xd4000001, // svc #0
                0x14000000, // b .
            ],
            "write(banner) / exit_group / park"
        );

        assert_eq!(&e[160..], BANNER, "the mapped banner matches x1");
    }
}
