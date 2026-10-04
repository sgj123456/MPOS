//! The aarch64 `/init`: the smallest real userspace programs, built in pure Rust.
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
//! know the answer tests the compiler, not the image). And the programs are a couple
//! dozen straight-line instructions, so each fits in a page and every byte is pinned
//! here against `llvm-objdump`'s own decoding of the built image (encode, then
//! disassemble, then confirm), not a hand-guess at the bit layout.
//!
//! Two images, one assembler:
//!
//! - [`minimal_init_elf`] (the headless verdict's) reads one line and exits, so
//!   `boot-aarch64` can stop polling on its terminal `INIT_EXITED`. It writes back
//!   what it read prefixed with `R:` (the tty's own echo of the same line carries no
//!   `R:`), so `R:echo-test` on the wire is unambiguous proof a scheduler-run
//!   program read console input and echoed it through `syscall::dispatch`.
//!
//! - [`looping_init_elf`] (the interactive `run --arch aarch64`'s) is the same read/
//!   echo body with the exit replaced by a branch back to the top, so it is a
//!   read/echo *forever* - a REPL stand-in a person can type into. It never exits,
//!   which is exactly why it must not be the headless verdict's image: with nobody to
//!   type, its second `read(0)` parks (the tty wakes it only when a line lands,
//!   `read_wait`'s canonical `UntilInput`) and `INIT_EXITED` never comes. A real
//!   shell (busybox `ash`) is the same shape plus the aarch64 `execve`/`clone`
//!   return-frame path (the named stage-6D gap).

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

const HEADER_LEN: usize = 64;
const PHDR_LEN: usize = 56;
/// Where the code sits: right after the header and the one program header.
const CODE_OFF: usize = HEADER_LEN + PHDR_LEN;

/// `MOVZ Xd, #imm16` (hardware shift 0).
fn movz(rd: u32, imm: u32) -> u32 {
    0xd280_0000u32 | ((imm & 0xffff) << 5) | rd
}
/// `MOVK Xd, #imm16, LSL #16` - the upper half of a two-word address load.
fn movk(rd: u32, imm: u32) -> u32 {
    0xf280_0000u32 | (1 << 21) | ((imm & 0xffff) << 5) | rd
}
/// `SVC #0`.
fn svc() -> u32 {
    0xd400_0001
}
/// `MOV Xd, Xm` (the `ORR Xd, XZR, Xm` alias).
fn movreg(rd: u32, rm: u32) -> u32 {
    0xaa00_03e0u32 | (rm << 16) | rd
}

/// An unconditional branch back to the first instruction of a block `n_words`
/// long, from the last word in it. `B` encodes a signed offset in instruction
/// units; from word `n_words-1` the top (word 0) is `-(n_words-1)` away, stored
/// as the `imm26` two's complement. Self-derived from the length so the loop and
/// its layout cannot disagree.
fn branch_to_start(n_words: usize) -> u32 {
    let back = (n_words - 1) as u32;
    let imm26 = ((1u32 << 26) - back) & 0x03ff_ffff;
    0x1400_0000u32 | imm26
}

/// The shared body: `read(0, buf, 64)`, stash the count in `x9`, `write(1, "R:",
/// 2)`, `write(1, buf, n)`. The caller decides what follows - `exit_group` (the
/// one-shot) or a branch back to the top (the loop). `buf_addr` and `prefix_addr`
/// are the addresses the data lands at, which depend on how many words the body
/// is, so the caller passes them in.
fn echo_body(buf_addr: u64, prefix_addr: u64) -> Vec<u32> {
    let load = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32),
            movk(rd, ((addr >> 16) & 0xffff) as u32),
        ]
    };
    let mut code = Vec::new();
    // read(0, buf, READ_LEN): x8=63, x0=0, x1=&buf, x2=READ_LEN, svc.
    code.extend([movz(8, 63), movz(0, 0)]);
    code.extend(load(1, buf_addr));
    code.extend([movz(2, READ_LEN), svc()]);
    // n is now in x0; keep it across the writes, which clobber x0/x1/x2.
    code.push(movreg(9, 0));
    // write(1, PREFIX, len): x8=64, x0=1, x1=&prefix, x2=len, svc.
    code.extend([movz(8, 64), movz(0, 1)]);
    code.extend(load(1, prefix_addr));
    code.extend([movz(2, PREFIX.len() as u32), svc()]);
    // write(1, buf, n): x8=64, x0=1, x1=&buf, x2=n (from x9), svc.
    code.extend([movz(8, 64), movz(0, 1)]);
    code.extend(load(1, buf_addr));
    code.push(movreg(2, 9));
    code.push(svc());
    code
}

/// Assemble a hand-written instruction list into the loadable `ET_EXEC`
/// `EM_AARCH64` image: header, one RWX `PT_LOAD` spanning the whole file from
/// offset 0, then the code at `CODE_OFF`, then a zero `READ_LEN` scratch buffer,
/// then `PREFIX`. The data addresses are derived from the code length, so a
/// caller that changes the word count cannot desync the pointers the code loads.
fn assemble(code: &[u32]) -> Vec<u8> {
    let buf_off = CODE_OFF + code.len() * 4;
    let prefix_off = buf_off + READ_LEN as usize;
    let file_len = prefix_off + PREFIX.len();

    let mut out = vec![0u8; file_len];
    out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
    out[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
    out[24..32].copy_from_slice(&(VADDR + CODE_OFF as u64).to_le_bytes()); // e_entry
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
        out[CODE_OFF + i * 4..CODE_OFF + i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out[prefix_off..].copy_from_slice(PREFIX);
    out
}

/// The addresses the echo body loads for a block of `n_code` instructions: the
/// scratch buffer and the prefix, laid out after the code by `assemble`.
fn data_addrs(n_code: usize) -> (u64, u64) {
    let buf_off = CODE_OFF + n_code * 4;
    let prefix_off = buf_off + READ_LEN as usize;
    (VADDR + buf_off as u64, VADDR + prefix_off as u64)
}

/// The one-shot `/init` the headless verdict runs: echo one line, then
/// `exit_group(0)`. See the module header.
pub fn minimal_init_elf() -> Vec<u8> {
    // Exit + park trail the body: exit_group(0) is x8=94, x0=0, svc; `b .` parks
    // in the impossible case the kernel returns from exit_group.
    let body_len = echo_body(0, 0).len();
    let n = body_len + 4;
    let (buf_addr, prefix_addr) = data_addrs(n);
    let mut code = echo_body(buf_addr, prefix_addr);
    code.extend([movz(8, 94), movz(0, 0), svc(), 0x1400_0000]);
    debug_assert_eq!(code.len(), n);
    assemble(&code)
}

/// The interactive `/init` `run --arch aarch64` runs: echo in a loop, forever.
/// Same body as the one-shot, with `exit_group`/`b .` replaced by a branch back to
/// the top, so a typed line is read and echoed, then the next is waited for. See
/// the module header.
pub fn looping_init_elf() -> Vec<u8> {
    let body_len = echo_body(0, 0).len();
    let n = body_len + 1; // the body plus the single backward branch
    let (buf_addr, prefix_addr) = data_addrs(n);
    let mut code = echo_body(buf_addr, prefix_addr);
    code.push(branch_to_start(n));
    debug_assert_eq!(code.len(), n);
    assemble(&code)
}

/// Pack a given `/init` image into a ustar archive named `initramfs.tar` at `out`,
/// the way `xtask/src/initramfs.rs` does for the x86 image: stage the file in a
/// directory and let the system `tar --format=ustar` write it. The kernel unpacks
/// it with `vfs::tar::unpack_into` and execs `/init` from the ramfs root it fills.
pub fn write_initramfs_with(elf: &[u8], staging_parent: &Path, out: &Path) -> Result<()> {
    let stage = staging_parent.join("aarch64-init-stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .with_context(|| format!("Failed to create {}", stage.display()))?;
    std::fs::write(stage.join("init"), elf)
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

    /// The pinned instruction counts the tests assert the built code has, which
    /// also pin that the loop's backward branch spans exactly the body.
    const ONE_SHOT_WORDS: usize = 23;
    const LOOP_WORDS: usize = 20;

    /// Shared structural checks for any assembled image: the ELF identity, the
    /// single RWX `PT_LOAD` covering the whole file from offset 0, and the entry
    /// pointing at the code.
    fn assert_loadable(e: &[u8]) {
        assert_eq!(&e[0..4], &[0x7f, b'E', b'L', b'F']);
        assert_eq!(e[4], 2, "ELFCLASS64");
        assert_eq!(u16::from_le_bytes([e[16], e[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([e[18], e[19]]), EM_AARCH64);
        let entry = u64::from_le_bytes(e[24..32].try_into().unwrap());
        assert_eq!(entry, VADDR + CODE_OFF as u64, "entry is the code");
        let ph = HEADER_LEN;
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
            "p_filesz covers the whole file"
        );
    }

    fn words_of(e: &[u8], n: usize) -> Vec<u32> {
        e[CODE_OFF..CODE_OFF + n * 4]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// The one-shot is a loadable image whose code starts `mov x8,#63` (read),
    /// stashes `n` in `x9`, and ends with `exit_group`/`b .`; its `R:` prefix sits
    /// at the address the echo write loads in `x1`.
    #[test]
    fn the_one_shot_reads_echoes_then_exits() {
        let e = minimal_init_elf();
        assert_loadable(&e);
        let words = words_of(&e, ONE_SHOT_WORDS);
        assert_eq!(words.len(), ONE_SHOT_WORDS);
        assert_eq!(words[0], 0xd28007e8, "mov x8,#63 (read)");
        assert_eq!(words[6], 0xaa0003e9, "mov x9,x0 (stash n)");
        assert_eq!(
            words[ONE_SHOT_WORDS - 4],
            0xd2800bc8,
            "mov x8,#94 (exit_group)"
        );
        assert_eq!(words[ONE_SHOT_WORDS - 1], 0x14000000, "b . (park)");

        // The data the code loads: the scratch buffer the `read` lands in, then the
        // `R:` prefix the echo write sends. `assemble` lays both out from the code
        // length, so this asserts where they sit for the pinned word count.
        let (buf_addr, prefix_addr) = data_addrs(ONE_SHOT_WORDS);
        let buf_off = CODE_OFF + ONE_SHOT_WORDS * 4;
        let prefix_off = buf_off + READ_LEN as usize;
        assert_eq!(VADDR + buf_off as u64, buf_addr);
        assert_eq!(VADDR + prefix_off as u64, prefix_addr);
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

    /// The loop image is the same body but ends in a branch back to the top, not
    /// an exit: word 0 is still `mov x8,#63`, the last word is `B` with a signed
    /// offset that lands on word 0 (`-(LOOP_WORDS-1)` instructions), and there is
    /// no `exit_group` in it.
    #[test]
    fn the_loop_echoes_forever() {
        let e = looping_init_elf();
        assert_loadable(&e);
        let words = words_of(&e, LOOP_WORDS);
        assert_eq!(words.len(), LOOP_WORDS);
        assert_eq!(words[0], 0xd28007e8, "mov x8,#63 (read)");
        assert_eq!(words[6], 0xaa0003e9, "mov x9,x0 (stash n)");
        assert!(
            !words.contains(&0xd2800bc8),
            "the loop has no exit_group - it never leaves"
        );

        // The final word decodes as an unconditional branch whose offset, from the
        // last instruction, points back at the first.
        let last = words[LOOP_WORDS - 1];
        assert_eq!(last >> 26, 0x05, "opcode 0x14000000 is B");
        let imm26 = last & 0x03ff_ffff;
        // Reinterpret the 26-bit field as signed; it must equal -(n_words-1).
        let signed = if imm26 & (1 << 25) != 0 {
            (imm26 as i64) - (1i64 << 26)
        } else {
            imm26 as i64
        };
        assert_eq!(signed, -(LOOP_WORDS as i64) + 1, "branch targets word 0");
    }
}
