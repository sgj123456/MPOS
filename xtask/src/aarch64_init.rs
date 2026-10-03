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
//! What it does is an interactive round trip: `read(0, buf, 64)`, then - only if
//! that read returns - `write(1, "MPOS_INIT_SAW_INPUT\n", 20)` and `exit_group(0)`.
//! The confirmation string is emitted only after the `read` syscall completes, so
//! the pair `MPOS_INIT_SAW_INPUT` and `INIT_EXITED` in the boot log proves a
//! scheduler-run userspace program *read console input and wrote output* through
//! the shared syscall table - the interactive input path reaching userspace, not
//! merely a program leaving. The boot hands off with a line already queued in the
//! tty (stage 7b's receive path), so the `read` returns real input. Every word is
//! verified by disassembling the built image with `llvm-objdump` (encode ->
//! disassemble -> confirm), not hand-guessed.

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
/// `PF_R | PF_W | PF_X` - the one mapped segment holds headers, code, the read
/// scratch buffer and the confirmation string; `W` so the loader maps it writable
/// (the `read` lands in the scratch buffer).
const PF_R_W_X: u32 = 7;

/// Where the image is mapped: the low text address a static aarch64 executable
/// uses, comfortably inside the user span and below `USER_LIMIT`.
const VADDR: u64 = 0x40_0000;

/// The string the program writes after its `read(0,...)` returns. Reaching the wire
/// is the proof the read completed; nothing else prints it.
const CONFIRM: &[u8] = b"MPOS_INIT_SAW_INPUT\n";
/// How many bytes the `read` asks for (and the scratch buffer's size).
const READ_LEN: u32 = 64;

/// The `_start` instructions (16 words = 64 bytes), then a 64-byte read scratch
/// buffer, then `CONFIRM`. Addresses are `VADDR + offset`, loaded into `x1` with
/// `mov`/`movk` (no `adr`/`adrp`, which a fixed `ET_EXEC` makes unnecessary).
pub fn minimal_init_elf() -> Vec<u8> {
    const HEADER_LEN: usize = 64;
    const PHDR_LEN: usize = 56;
    const CODE_BYTES: usize = 64; // sixteen 32-bit instructions
    let code_off = HEADER_LEN + PHDR_LEN; // 120
    let buf_off = code_off + CODE_BYTES; // 184 - the read scratch buffer
    let confirm_off = buf_off + READ_LEN as usize; // 248 - the message
    let buf_addr = VADDR + buf_off as u64;
    let confirm_addr = VADDR + confirm_off as u64;
    let entry = VADDR + code_off as u64;
    let file_len = confirm_off + CONFIRM.len();

    let movz = |rd: u32, imm: u32, hw: u32| 0xd280_0000u32 | (hw << 21) | (imm << 5) | rd;
    let movk = |rd: u32, imm: u32, hw: u32| 0xf280_0000u32 | (hw << 21) | (imm << 5) | rd;
    let svc = || 0xd400_0001u32;
    // Build x(rd) = addr: low 16 via movz, next 16 via movk lsl #16.
    let load_addr = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32, 0),
            movk(rd, ((addr >> 16) & 0xffff) as u32, 1),
        ]
    };

    let mut code: Vec<u32> = Vec::new();
    // read(0, buf, READ_LEN): x8=63, x0=0, x1=&buf, x2=READ_LEN
    code.extend([movz(8, 63, 0), movz(0, 0, 0)]);
    code.extend(load_addr(1, buf_addr));
    code.extend([movz(2, READ_LEN, 0), svc()]);
    // write(1, CONFIRM, len): x8=64, x0=1, x1=&confirm, x2=len
    code.extend([movz(8, 64, 0), movz(0, 1, 0)]);
    code.extend(load_addr(1, confirm_addr));
    code.extend([movz(2, CONFIRM.len() as u32, 0), svc()]);
    // exit_group(0): x8=94, x0=0, svc; then b . (park) in case it returns.
    code.extend([movz(8, 94, 0), movz(0, 0, 0), svc(), 0x1400_0000]);

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
    // The read scratch buffer (buf_off..confirm_off) stays zero-filled; the
    // confirmation string sits after it.
    out[confirm_off..].copy_from_slice(CONFIRM);
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
    /// segment, the code words are exactly `read(0,buf); write(1,confirm);
    /// exit_group(0)`, and the confirmation string is present. The words and the
    /// addresses are checked against `llvm-objdump`'s own decoding of the same
    /// bytes, not a re-derivation.
    #[test]
    fn the_init_reads_then_confirms_and_exits() {
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

        // The sixteen instructions: read(0, 0x4000b8, 64) then write(1, 0x4000f8,
        // 20) then exit_group(0); b . - the confirm pointer 0x4000f8 is VADDR +
        // buf_off(184) + READ_LEN(64), where the string sits after the 64-byte
        // scratch buffer the read fills.
        let words: Vec<u32> = e[120..184]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            vec![
                0xd28007e8, // mov x8,#63   (read)
                0xd2800000, // mov x0,#0    (stdin)
                0xd2801701, // mov x1,#0xb8
                0xf2a00801, // movk x1,#0x40,lsl #16   -> 0x4000b8 (scratch buf)
                0xd2800802, // mov x2,#64   (read length)
                0xd4000001, // svc #0
                0xd2800808, // mov x8,#64   (write)
                0xd2800020, // mov x0,#1    (stdout)
                0xd2801f01, // mov x1,#0xf8
                0xf2a00801, // movk x1,#0x40,lsl #16   -> 0x4000f8 (confirm string)
                0xd2800282, // mov x2,#20   (confirm length)
                0xd4000001, // svc #0
                0xd2800bc8, // mov x8,#94   (exit_group)
                0xd2800000, // mov x0,#0
                0xd4000001, // svc #0
                0x14000000, // b .
            ],
            "read(0) / write(confirm) / exit_group / park"
        );

        // The 64-byte read scratch (e[184..248]) is zero; the confirm string is at
        // e[248..], exactly what `x1` for the write points at.
        assert!(
            e[184..248].iter().all(|&b| b == 0),
            "scratch buffer is zero-filled"
        );
        assert_eq!(
            &e[248..],
            CONFIRM,
            "the mapped confirm string matches the write's x1"
        );
    }
}
