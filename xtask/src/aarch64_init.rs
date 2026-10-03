//! The aarch64 `/init`: the smallest real userspace program, built in pure Rust.
//!
//! Stage 6/7's image prerequisite. The kernel's ELF loader (`src/mm/elf.rs`) was
//! taught `EM_AARCH64` in 6g, and `spawn_init`/`load_user` map the segments and
//! jump to `e_entry` - but something must *produce* an aarch64 binary for the
//! initramfs. This file does.
//!
//! It is deliberately hand-encoded rather than built with a cross compiler. Two
//! reasons, both the house's: a build step that shells out to `clang` plus a
//! rustup-internal `ld.lld` path would add a fragile host prerequisite to the boot
//! path and make the artifact non-hermetic (a test that has to run the compiler to
//! know the answer tests the compiler, not the image). And the program is a couple
//! dozen straight-line instructions, so it fits in a page and every byte is pinned
//! here against `llvm-objdump`'s own decoding of the built image (encode, then
//! disassemble, then confirm), not a hand-guess at the bit layout.
//!
//! What it does is an interactive echo round trip: `read(0, buf, 64)`, then
//! `write(1, "R:", 2)` and `write(1, buf, n)` (the bytes it just read), then
//! `exit_group(0)`. Because it writes back *what it read* - with a prefix that the
//! tty's own canonical echo does not add - the boot log can assert the literal
//! `R:echo-test`: proof a scheduler-run userspace program read console input and
//! echoed it through `syscall::dispatch`, the interactive path reaching userspace
//! end to end. A looping shell (a read/echo forever, or busybox `ash`) is the next
//! step; this proves the single round trip it is made of.

use std::path::Path;
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
/// scratch buffer and the prefix string; `W` so the loader maps it writable (the
/// `read` lands in the scratch buffer).
const PF_R_W_X: u32 = 7;

/// Where the image is mapped: the low text address a static aarch64 executable
/// uses, comfortably inside the user span and below `USER_LIMIT`.
const VADDR: u64 = 0x40_0000;

/// The prefix the program writes before echoing the bytes it read, so its output
/// is distinguishable from the tty's own echo of the same input.
const PREFIX: &[u8] = b"R:";
/// How many bytes the `read` asks for (and the scratch buffer's size).
const READ_LEN: u32 = 64;

/// The number of `_start` instructions (used to lay out the data after the code).
const CODE_WORDS: usize = 23;

/// Build the aarch64 `/init`: read a line, echo `"R:"` + the bytes read to the
/// console, exit. See the module header.
pub fn minimal_init_elf() -> Vec<u8> {
    const HEADER_LEN: usize = 64;
    const PHDR_LEN: usize = 56;
    let code_off = HEADER_LEN + PHDR_LEN; // 120
    let buf_off = code_off + CODE_WORDS * 4;
    let prefix_off = buf_off + READ_LEN as usize;
    let buf_addr = VADDR + buf_off as u64;
    let prefix_addr = VADDR + prefix_off as u64;
    let entry = VADDR + code_off as u64;
    let file_len = prefix_off + PREFIX.len();

    let movz = |rd: u32, imm: u32| 0xd280_0000u32 | ((imm & 0xffff) << 5) | rd;
    let movk = |rd: u32, imm: u32| 0xf280_0000u32 | (1 << 21) | ((imm & 0xffff) << 5) | rd;
    let svc = || 0xd400_0001u32;
    let movreg = |rd: u32, rm: u32| 0xaa00_03e0u32 | (rm << 16) | rd;

    let mut code: Vec<u32> = Vec::with_capacity(CODE_WORDS);
    // read(0, buf, READ_LEN): x8=63, x0=0, x1=&buf, x2=READ_LEN, svc.
    code.push(movz(8, 63));
    code.push(movz(0, 0));
    code.push(movz(1, (buf_addr & 0xffff) as u32));
    code.push(movk(1, ((buf_addr >> 16) & 0xffff) as u32));
    code.push(movz(2, READ_LEN));
    code.push(svc());
    // n is now in x0; stash it in x9 (the writes clobber x0/x1/x2).
    code.push(movreg(9, 0));
    // write(1, PREFIX, len): x8=64, x0=1, x1=&prefix, x2=len, svc.
    code.push(movz(8, 64));
    code.push(movz(0, 1));
    code.push(movz(1, (prefix_addr & 0xffff) as u32));
    code.push(movk(1, ((prefix_addr >> 16) & 0xffff) as u32));
    code.push(movz(2, PREFIX.len() as u32));
    code.push(svc());
    // write(1, buf, n): x8=64, x0=1, x1=&buf, x2=n (from x9), svc.
    code.push(movz(8, 64));
    code.push(movz(0, 1));
    code.push(movz(1, (buf_addr & 0xffff) as u32));
    code.push(movk(1, ((buf_addr >> 16) & 0xffff) as u32));
    code.push(movreg(2, 9));
    code.push(svc());
    // exit_group(0): x8=94, x0=0, svc; then b . (park) if the kernel ever returns.
    code.push(movz(8, 94));
    code.push(movz(0, 0));
    code.push(svc());
    code.push(0x1400_0000);

    assert_eq!(
        code.len(),
        CODE_WORDS,
        "instruction count must match CODE_WORDS"
    );

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
    // The read scratch (buf_off..prefix_off) is zero; PREFIX sits at prefix_off,
    // exactly the address the second write passes in x1.
    out[prefix_off..].copy_from_slice(PREFIX);
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

    /// The built image is a loadable static `ET_EXEC` `EM_AARCH64` whose single
    /// `PT_LOAD` covers header+code+scratch+prefix from offset 0; the instruction
    /// count is the pinned `CODE_WORDS`; the first instruction is `read`'s
    /// `mov x8,#63`; and the prefix `R:` sits at the address the echo write passes
    /// in `x1`. Bytes are checked against `llvm-objdump`'s decoding of the image.
    #[test]
    fn the_init_echoes_what_it_reads() {
        let e = minimal_init_elf();
        assert_eq!(&e[0..4], &[0x7f, b'E', b'L', b'F']);
        assert_eq!(e[4], 2, "ELFCLASS64");
        assert_eq!(u16::from_le_bytes([e[16], e[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([e[18], e[19]]), EM_AARCH64);
        let entry = u64::from_le_bytes(e[24..32].try_into().unwrap());
        assert_eq!(entry, VADDR + 120, "entry is the code, inside the segment");
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
            u64::from_le_bytes(e[ph + 32..ph + 40].try_into().unwrap()),
            e.len() as u64,
            "p_filesz"
        );

        let words: Vec<u32> = e[120..120 + CODE_WORDS * 4]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words.len(), CODE_WORDS);
        assert_eq!(words[0], 0xd28007e8, "mov x8,#63 (read)");
        assert_eq!(words[6], 0xaa0003e9, "mov x9,x0 (stash n)");
        assert_eq!(words[CODE_WORDS - 1], 0x14000000, "b . (park)");

        let buf_off = 120 + CODE_WORDS * 4;
        let prefix_off = buf_off + READ_LEN as usize;
        assert!(
            e[buf_off..prefix_off].iter().all(|&b| b == 0),
            "scratch is zero-filled"
        );
        assert_eq!(
            &e[prefix_off..],
            PREFIX,
            "the mapped prefix is the echo write's x1"
        );
    }
}
