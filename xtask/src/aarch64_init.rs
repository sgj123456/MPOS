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
//! - [`exec_launcher_init_elf`] (the headless verdict's `/init`) echoes the queued
//!   line - writes back what it read prefixed with `R:` (the tty's own echo of the
//!   same line carries no `R:`), so `R:echo-test` on the wire is unambiguous proof a
//!   scheduler-run program read console input and echoed it through
//!   `syscall::dispatch` - then its last act is `execve("/t", ...)`. If aarch64
//!   `execve` works the image is replaced and the launcher's own `exit_group`
//!   fallback (which `boot-aarch64` can stop polling on) is never reached; `/t` is
//!   [`marker_target_elf`].
//!
//! - [`marker_target_elf`] (the verdict's `/t`) writes `MPOS_EXEC_OK`, then forks
//!   via `clone` and `wait4`: the child writes `MPOS_FORK_OK` and the parent waits,
//!   so the child's line lands before the halt. Nothing else prints either string,
//!   so their appearance proves `execve` transferred to a *second* image that ran at
//!   EL0 and that `clone` started a *third* process (stage 6D).
//!
//! - [`looping_init_elf`] (the interactive `run --arch aarch64`'s) is the echo body
//!   with the exit replaced by a branch back to the top, so it is a read/echo
//!   *forever* - a REPL stand-in a person can type into. It never exits, which is
//!   exactly why it must not be the headless verdict's image: with nobody to type,
//!   its second `read(0)` parks (the tty wakes it only when a line lands,
//!   `read_wait`'s canonical `UntilInput`) and `INIT_EXITED` never comes. A real
//!   shell (busybox `ash`) is this loop plus `execve` of the shell binary; the
//!   `execve`/`clone`/`wait4` paths and an unmodified static-musl ELF running at EL0
//!   are all built and pinned by the verdict's `MPOS_MUSL_OK` (stages 6D + the
//!   demand-paging handler).

use std::os::unix::fs::PermissionsExt;
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

/// `CBNZ X0, label` - branch to `label` if `x0` is non-zero, given the branch's
/// own word index and the label's. `imm19` is the signed distance in instructions;
/// `CBNZ` is opcode `0xB5000000` with `Rt`=0, so `0xB5000000 | (d << 5)`. The fork
/// target uses it to split parent (`x0` = child pid) from child (`x0` = 0).
fn cbnz_x0(from_word: usize, to_word: usize) -> u32 {
    let d = (to_word - from_word) as i32;
    let imm19 = (d as u32) & 0x0007_ffff;
    0xb500_0000u32 | (imm19 << 5)
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

/// Assemble a hand-written instruction list plus a data blob into the loadable
/// `ET_EXEC` `EM_AARCH64` image: header, one RWX `PT_LOAD` spanning the whole
/// file from offset 0, the code at `CODE_OFF`, then `data` immediately after the
/// code. The data addresses are derived from the code length, so a caller that
/// changes the word count cannot desync the pointers its code loads into the blob.
fn assemble_with(code: &[u32], data: &[u8]) -> Vec<u8> {
    let file_len = CODE_OFF + code.len() * 4 + data.len();

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
    let data_off = CODE_OFF + code.len() * 4;
    out[data_off..].copy_from_slice(data);
    out
}

/// The echo body's data blob: a zero `READ_LEN` scratch buffer then `PREFIX`.
fn assemble(code: &[u32]) -> Vec<u8> {
    let mut data = vec![0u8; READ_LEN as usize];
    data.extend_from_slice(PREFIX);
    assemble_with(code, &data)
}

/// The addresses the echo body loads for a block of `n_code` instructions: the
/// scratch buffer and the prefix, laid out after the code by `assemble`.
fn data_addrs(n_code: usize) -> (u64, u64) {
    let buf_off = CODE_OFF + n_code * 4;
    let prefix_off = buf_off + READ_LEN as usize;
    (VADDR + buf_off as u64, VADDR + prefix_off as u64)
}

/// The interactive `/init` `run --arch aarch64` runs: echo in a loop, forever.
/// Same body as the exec launcher's echo, with `exit_group`/`b .` replaced by a
/// branch back to the top, so a typed line is read and echoed, then the next is
/// waited for. See the module header.
pub fn looping_init_elf() -> Vec<u8> {
    let body_len = echo_body(0, 0).len();
    let n = body_len + 1; // the body plus the single backward branch
    let (buf_addr, prefix_addr) = data_addrs(n);
    let mut code = echo_body(buf_addr, prefix_addr);
    code.push(branch_to_start(n));
    debug_assert_eq!(code.len(), n);
    assemble(&code)
}

/// `aarch64` asm-generic syscall numbers used below (Linux's, from `nr.rs`'s
/// aarch64 table): `execve`, `clone`, `wait4`, and `exit_group`. `read`/`write`
/// (63/64) are in [`echo_body`]. `SIGCHLD` is the `clone` exit signal so the
/// parent's `wait4` reaps the child.
const NR_EXECVE: u32 = 221;
const NR_CLONE: u32 = 220;
const NR_WAIT4: u32 = 260;
const NR_EXIT_GROUP: u32 = 94;
const SIGCHLD: u32 = 17;

/// The string the exec target writes when it runs - proof aarch64 `execve`
/// replaced the image and the new program ran at EL0. Nothing else prints it.
const EXEC_MARKER: &[u8] = b"MPOS_EXEC_OK\n";
/// The string the launcher writes only after its deliberately-far `str` to a
/// never-mapped stack page survives - i.e. only if the kernel's aarch64 EL0
/// fault handler demand-paged it. Without the handler that store is a fatal data
/// abort and this line never appears. Nothing else prints it.
const FAULT_MARKER: &[u8] = b"MPOS_FAULT_OK\n";
/// `SUB SP, SP, #0x1000` and `STR X19, [SP]` - the two words that push the stack
/// a page past what the loader mapped and touch it, to take a demand-paging fault.
/// Verified against `llvm-objdump`'s decoding of these exact bytes.
const SUB_SP_PAGE: u32 = 0xd140_07ff;
const STR_X19_SP: u32 = 0xf900_03f3;
/// The string the fork target's *child* writes - proof aarch64 `clone` (the
/// arch's real `fork`) started a second running program. Nothing else prints it.
const FORK_MARKER: &[u8] = b"MPOS_FORK_OK\n";
/// The path the launcher `execve`s, and the ramfs name the target is packed as.
const EXEC_PATH: &[u8] = b"/t\0";

/// The exec target: writes `MPOS_EXEC_OK` (proving aarch64 `execve` transferred
/// control here and ran at EL0), then forks. The child writes `MPOS_FORK_OK` and
/// exits; the parent `wait4`s for it. The parent then `execve`s `/musl` - an
/// *unmodified* statically-linked musl/Rust binary that prints `MPOS_MUSL_OK` - so
/// the wire proves `execve`, `clone`/`wait4`, AND running a real musl binary at EL0
/// (the demand-paging handler services its larger stack/anon pages). The parent's
/// `exit_group` is only a fallback if that `execve` fails. Word layout (entry = 0):
///   0..5   write(1, "MPOS_EXEC_OK\n", 13)
///   6..12  clone(SIGCHLD, 0,0,0,0) -> x0 = child pid (parent) or 0 (child)
///   13     cbnz x0, 24   (parent jumps; child falls through)
///   14..23 child: write(1, "MPOS_FORK_OK\n", 13); exit_group(0); b .
///   24..30 parent: wait4(0, &status, 0, NULL)
///   31..38 parent: execve("/musl", ["/musl", NULL], [NULL])
///   39..42 parent: exit_group(0); b .  (fallback only)
pub fn marker_target_elf() -> Vec<u8> {
    let n = 43usize;
    let data_base = VADDR + (CODE_OFF + n * 4) as u64;
    let exec_addr = data_base;
    let fork_addr = data_base + 13;
    let musl_path_addr = data_base + 26;
    let status_addr = data_base + 32;
    let argv_addr = data_base + 40;
    let envp_addr = data_base + 56;
    let load = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32),
            movk(rd, ((addr >> 16) & 0xffff) as u32),
        ]
    };

    let mut code: Vec<u32> = Vec::new();
    // write(1, exec_marker, 13) - the exec confirmation, run once before forking.
    code.extend([movz(8, 64), movz(0, 1)]);
    code.extend(load(1, exec_addr));
    code.extend([movz(2, EXEC_MARKER.len() as u32), svc()]); // 0..5
    // clone(SIGCHLD, child_stack=0, ptid=0, ctid=0, tls=0): the aarch64 `fork`.
    code.extend([
        movz(8, NR_CLONE),
        movz(0, SIGCHLD),
        movz(1, 0),
        movz(2, 0),
        movz(3, 0),
        movz(4, 0),
        svc(),
    ]); // 6..12
    code.push(cbnz_x0(13, 24)); // 13 - parent (x0 != 0) jumps to 24
    // child: write(1, fork_marker, 13); exit_group(0); b .
    code.extend([movz(8, 64), movz(0, 1)]);
    code.extend(load(1, fork_addr));
    code.extend([movz(2, FORK_MARKER.len() as u32), svc()]); // 14..19
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]); // 20..23
    // parent: wait4(pid=0, &status, options=0, rusage=NULL).
    code.extend([movz(8, NR_WAIT4), movz(0, 0)]); // 24..25
    code.extend(load(1, status_addr)); // 26..27
    code.extend([movz(2, 0), movz(3, 0), svc()]); // 28..30
    // parent: execve("/musl", argv=["/musl",NULL], envp=[NULL]) - the real binary.
    code.extend([movz(8, NR_EXECVE)]); // 31
    code.extend(load(0, musl_path_addr)); // 32..33
    code.extend(load(1, argv_addr)); // 34..35
    code.extend(load(2, envp_addr)); // 36..37
    code.push(svc()); // 38
    // Fallback if that execve failed: exit_group(0); b .
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]); // 39..42
    debug_assert_eq!(code.len(), n, "fork target word count must match");

    // Data blob: the two markers, the "/musl" path, the wait4 status word, then the
    // argv/envp pointer arrays.
    let mut data = vec![0u8; 64];
    data[0..EXEC_MARKER.len()].copy_from_slice(EXEC_MARKER);
    data[13..13 + FORK_MARKER.len()].copy_from_slice(FORK_MARKER);
    data[26..32].copy_from_slice(b"/musl\0");
    data[40..48].copy_from_slice(&musl_path_addr.to_le_bytes()); // argv[0] = &"/musl"
    // argv[1], envp[0], status stay zero.
    assemble_with(&code, &data)
}

/// The headless verdict's `/init`: first it proves aarch64 EL0 demand paging by
/// pushing `sp` a page below what the loader mapped and storing there - a fault
/// that is fatal unless the kernel's fault handler takes the page - then writes
/// `MPOS_FAULT_OK` only if that store survived. It then echoes the queued line (so
/// `boot-aarch64` still shows `R:echo-test`) and `execve("/t", ...)`. If `execve`
/// works the image becomes the target and `MPOS_EXEC_OK`/`MPOS_FORK_OK` reach the
/// wire; the launcher's `exit_group` fallback runs only if `execve` fails - honest,
/// so a broken `execve` is a missing marker, not a hang.
pub fn exec_launcher_init_elf() -> Vec<u8> {
    let load = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32),
            movk(rd, ((addr >> 16) & 0xffff) as u32),
        ]
    };
    // 8 demand-paging words (sub/str/write/svc) + 19 echo + 8 execve + 4 fallback = 39.
    let body_len = echo_body(0, 0).len();
    let n = 8 + body_len + 8 + 4;
    let data_base = VADDR + (CODE_OFF + n * 4) as u64;
    // Blob offsets from `data_base`:
    //   0..64   read scratch (buf)
    //   64..66  "R:" (prefix)
    //   66..69  "/t\0" (exec path)
    //   69..72  padding to 8
    //   72..88  argv = [&"/t", NULL]
    //   88..96  envp = [NULL]
    //   96..    "MPOS_FAULT_OK\n"
    let buf_addr = data_base;
    let prefix_addr = data_base + 64;
    let path_addr = data_base + 66;
    let argv_addr = data_base + 72;
    let envp_addr = data_base + 88;
    let fault_addr = data_base + 96;

    // Demand-paging probe: `sp -= 0x1000` lands one page below the mapped stack;
    // `str x19,[sp]` faults there. If `el0_sync`'s handler demand-pages it, the
    // store succeeds and we fall through to the write; otherwise it is SIGSEGV and
    // `MPOS_FAULT_OK` never appears.
    let mut code = vec![SUB_SP_PAGE, STR_X19_SP];
    code.extend([movz(8, 64), movz(0, 1)]);
    code.extend(load(1, fault_addr));
    code.extend([movz(2, FAULT_MARKER.len() as u32), svc()]); // 0..7
    code.extend(echo_body(buf_addr, prefix_addr));
    // execve(path, argv, envp): x8=221, x0=&"/t", x1=&argv, x2=&envp, svc.
    code.extend([movz(8, NR_EXECVE)]);
    code.extend(load(0, path_addr));
    code.extend(load(1, argv_addr));
    code.extend(load(2, envp_addr));
    code.push(svc());
    // Fallback if execve returns (i.e. failed): exit_group(0) so the verdict
    // still terminates, and `b .`.
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]);
    debug_assert_eq!(code.len(), n);

    // Build the data blob with the pointers written as LE u64 into it.
    let mut data = vec![0u8; 96 + FAULT_MARKER.len()];
    data[64..66].copy_from_slice(PREFIX);
    data[66..66 + EXEC_PATH.len()].copy_from_slice(EXEC_PATH);
    data[72..80].copy_from_slice(&path_addr.to_le_bytes()); // argv[0] = &"/t"
    // argv[1] = NULL (0) and envp[0] = NULL (0): the zero-init already covers them.
    data[96..].copy_from_slice(FAULT_MARKER);
    assemble_with(&code, &data)
}

/// Pack a set of `(name, bytes)` files into a ustar `initramfs.tar` at `out`, the
/// way `xtask/src/initramfs.rs` does for the x86 image. The kernel unpacks it with
/// `vfs::tar::unpack_into` and execs `/init` from the ramfs root it fills; the
/// exec-pin packs `/init` and `/t` together so the launcher can reach the target.
pub fn write_initramfs_entries(
    entries: &[(&str, &[u8])],
    staging_parent: &Path,
    out: &Path,
) -> Result<()> {
    let stage = staging_parent.join("aarch64-init-stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .with_context(|| format!("Failed to create {}", stage.display()))?;
    let mut names = Vec::new();
    for (name, bytes) in entries {
        let path = stage.join(name);
        std::fs::write(&path, bytes)
            .with_context(|| format!("Failed to write {}/{}", stage.display(), name))?;
        // Mark it executable. The kernel's ramfs keeps the mode the archive
        // records, and `execve` (and the loader's read of `/init`) reach it: even
        // root is denied execute when no x bit is set, so a 0644 `/t` makes the
        // launcher's `execve` fail with `EACCES` rather than transfer.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to chmod +x {}", path.display()))?;
        names.push(name.to_string());
    }

    let mut cmd = Command::new("tar");
    cmd.arg("--format=ustar")
        .args(["-C", &stage.display().to_string()])
        .args(["-cf", &out.display().to_string()]);
    for name in &names {
        cmd.arg(name);
    }
    let status = cmd
        .status()
        .context("Failed to start tar to build the aarch64 initramfs")?;
    if !status.success() {
        anyhow::bail!("tar failed building the aarch64 initramfs: {status}");
    }
    let _ = std::fs::remove_dir_all(&stage);
    Ok(())
}

/// The aarch64 sweep's `/init`: an ordinary `std` program that runs every `/t-*`
/// the payload staged and prints the same verdict protocol the x86 test `/init`
/// does, so the harness parses both machines with one reader.
///
/// It is a *program* rather than the x86 side's `#!/bin/sh` script because the
/// second machine has no shell to run one - the smallest honest runner is a musl
/// binary, and one already exists in the toolchain. Each test runs through
/// `std::process::Command`, i.e. musl's `posix_spawn`
/// (`clone(CLONE_VM|CLONE_VFORK|SIGCHLD)` + `execve`), a flag set the kernel's own
/// clone validation names and accepts (`syscall/linux/abi.rs`), with
/// `clone`/`execve`/`wait4` already proven on this machine by the hand-encoded
/// launcher (`MPOS_FORK_OK`, `MPOS_EXEC_OK`).
///
/// The runner does not end the machine the way the x86 one does: there is no
/// `isa-debug-exit` on this wiring, so `=== END` is the terminator the harness
/// polls for, exactly as `INIT_EXITED` is for `boot-aarch64`.
///
/// The tests land on an ext4 the harness builds with `mkfs.ext4 -d`, which keeps
/// the mode the staging step wrote, so the runner makes each program executable
/// before it runs it rather than depending on that step to.
pub fn build_sweep_init(build_dir: &Path) -> Result<Vec<u8>> {
    const SRC: &str = r#"use std::os::unix::fs::PermissionsExt;

fn main() {
    let dir = "/";
    let mut names: Vec<String> = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("t-") {
                    names.push(name);
                }
            }
        }
        Err(e) => println!("=== FAIL read_dir({dir}) {e}"),
    }
    names.sort();
    let mut pass = 0u32;
    let mut fail = 0u32;
    for name in &names {
        let path = format!("{dir}{name}");
        if let Ok(md) = std::fs::metadata(&path) {
            let mut perms = md.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&path, perms);
        }
        println!("=== RUN {name}");
        match std::process::Command::new(&path).status() {
            Ok(status) if status.success() => {
                println!("=== PASS {name}");
                pass += 1;
            }
            Ok(status) => {
                println!("=== FAIL {name} exit={}", status.code().unwrap_or(-1));
                fail += 1;
            }
            Err(e) => {
                println!("=== FAIL {name} spawn={e}");
                fail += 1;
            }
        }
    }
    println!("=== SUMMARY pass={pass} fail={fail}");
    println!("=== END");
}
"#;
    let src = build_dir.join("aarch64-sweep-init.rs");
    let out = build_dir.join("aarch64-sweep-init");
    std::fs::write(&src, SRC).with_context(|| format!("Failed to write {}", src.display()))?;

    let status = Command::new("rustc")
        .arg("--target")
        .arg("aarch64-unknown-linux-musl")
        .args(["-C", "target-feature=+crt-static"])
        .args(["-C", "linker=rust-lld"])
        .args(["-C", "strip=symbols"])
        .arg("-O")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .context("Failed to run rustc for the aarch64 sweep init")?;
    if !status.success() {
        anyhow::bail!(
            "rustc failed building the aarch64 sweep init ({status}); the \
             aarch64-unknown-linux-musl target must be installed: \
             `rustup target add aarch64-unknown-linux-musl`"
        );
    }
    let bytes = std::fs::read(&out).with_context(|| format!("Failed to read {}", out.display()))?;
    if bytes.len() < 20
        || &bytes[0..4] != b"\x7fELF"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != EM_AARCH64
    {
        anyhow::bail!("the aarch64 sweep init is not an EM_AARCH64 ELF");
    }
    Ok(bytes)
}

/// An *unmodified* Rust `std` program compiled to a statically-linked
/// `aarch64-unknown-linux-musl` ELF. Deliberately a normal `main`, not
/// hand-encoded: it proves the second machine runs the real musl/Rust runtime end
/// to end. It allocates and fills a 256 KiB heap buffer (musl `malloc` -> `mmap`
/// and the demand-paging handler for the untouched pages), sums it (musl `memcpy`,
/// which uses 8-byte-aligned `ldp`/`stp` on arbitrary pointers - the case that
/// pinned the `SCTLR_EL1.A` alignment-check fix), and prints the checksum. A wrong
/// number or a fault means the real-binary path is broken, so the `MPOS_MUSL_OK`
/// pin is a claim the run can back, not a fixed string that prints no matter what.
///
/// The `aarch64-unknown-linux-musl` target and `rust-lld` are prerequisites (the
/// x86 test path already requires the `x86_64` musl target); if the target is absent
/// `rustc` fails and the message names the install command rather than skipping - a
/// silent skip would be the one failure mode this must not have.
pub fn build_static_musl_helper(build_dir: &Path) -> Result<Vec<u8>> {
    const SRC: &str = concat!(
        "fn main() {\n",
        "    let n = 256 * 1024;\n",
        "    let mut buf = vec![0u8; n];\n",
        "    for (i, b) in buf.iter_mut().enumerate() { *b = (i & 0xff) as u8; }\n",
        "    let sum: u64 = buf.iter().map(|&b| b as u64).sum();\n",
        "    println!(\"MPOS_MUSL_OK sum={}\", sum);\n",
        "    {\n",
        "        use std::io::Read;\n",
        "        match std::fs::File::open(\"/dev/urandom\") {\n",
        "            Ok(mut f) => {\n",
        "                let mut e = [0u8; 8];\n",
        "                if f.read_exact(&mut e).is_ok() { println!(\"MPOS_DEV_URANDOM_OK\"); }\n",
        "                else { println!(\"MPOS_DEV_URANDOM_FAILED\"); }\n",
        "            }\n",
        "            Err(_) => println!(\"MPOS_DEV_URANDOM_FAILED\"),\n",
        "        }\n",
        "    }\n",
        "    {\n",
        "        use std::io::Write;\n",
        "        match std::fs::OpenOptions::new().write(true).open(\"/dev/console\") {\n",
        "            Ok(mut c) => {\n",
        "                if c.write_all(b\"MPOS_DEV_CONSOLE_OK\\n\").is_ok() { let _ = c.flush(); }\n",
        "                else { println!(\"MPOS_DEV_CONSOLE_FAILED\"); }\n",
        "            }\n",
        "            Err(_) => println!(\"MPOS_DEV_CONSOLE_FAILED\"),\n",
        "        }\n",
        "    }\n",
        "    match std::fs::read_dir(\"/dev\") {\n",
        "        Ok(rd) => {\n",
        "            let mut k = 0usize;\n",
        "            for e in rd { if e.is_ok() { k += 1; } }\n",
        "            if k > 0 { println!(\"MPOS_DEV_GETDENTS_OK k={}\", k); }\n",
        "            else { println!(\"MPOS_DEV_GETDENTS_FAILED\"); }\n",
        "        }\n",
        "        Err(_) => println!(\"MPOS_DEV_GETDENTS_FAILED\"),\n",
        "    }\n",
        "    match std::fs::metadata(\"/dev/urandom\") {\n",
        "        Ok(_) => println!(\"MPOS_DEV_STAT_OK\"),\n",
        "        Err(_) => println!(\"MPOS_DEV_STAT_FAILED\"),\n",
        "    }\n",
        "    // A real file lifecycle on the mounted root ramfs - create, write,\n",
        "    // read back, stat, remove - the operations a shell and any init do\n",
        "    // against the filesystem proper, not just device nodes.\n",
        "    let probe = \"/mprobe\";\n",
        "    let msg: &[u8] = b\"mpos-userspace-file-lifecycle\";\n",
        "    let mut ok = std::fs::write(probe, msg).is_ok();\n",
        "    if ok { ok = std::fs::read(probe).map(|g| g == msg).unwrap_or(false); }\n",
        "    if ok { ok = std::fs::metadata(probe).map(|m| m.len() == msg.len() as u64).unwrap_or(false); }\n",
        "    if ok { ok = std::fs::remove_file(probe).is_ok(); }\n",
        "    if ok { ok = std::fs::metadata(probe).is_err(); }\n",
        "    if ok { println!(\"MPOS_FILE_OPS_OK\"); } else { println!(\"MPOS_FILE_OPS_FAILED\"); }\n",
        "    // The shell's startup + `cd`: getcwd and chdir. Move into /dev, confirm\n",
        "    // current_dir reports it, move back - the round trip a `cd`/`pwd` makes.\n",
        "    let mut cwdok = std::env::current_dir().is_ok();\n",
        "    if cwdok && std::env::set_current_dir(\"/dev\").is_ok() {\n",
        "        cwdok = std::env::current_dir().map(|p| p == std::path::PathBuf::from(\"/dev\")).unwrap_or(false);\n",
        "        let _ = std::env::set_current_dir(\"/\");\n",
        "    } else {\n",
        "        cwdok = false;\n",
        "    }\n",
        "    if cwdok { println!(\"MPOS_CWD_OK\"); } else { println!(\"MPOS_CWD_FAILED\"); }\n",
        "    // symlink + readlink: create a link to /dev, read the target back,\n",
        "    // remove it - SYMLINKAT/READLINK/unlink, the pair `ln -s` and the\n",
        "    // resolution step a shell or loader uses.\n",
        "    let link = \"/plink\";\n",
        "    let mut sok = std::os::unix::fs::symlink(\"/dev\", link).is_ok();\n",
        "    if sok { sok = std::fs::read_link(link).map(|t| t == std::path::PathBuf::from(\"/dev\")).unwrap_or(false); }\n",
        "    if sok { sok = std::fs::remove_file(link).is_ok(); }\n",
        "    if sok { println!(\"MPOS_SYMLINK_OK\"); } else { println!(\"MPOS_SYMLINK_FAILED\"); }\n",
        "    // mkdir + a file inside the new directory + rmdir: MKDIRAT creates a\n",
        "    // traversable directory, the nested write/read reaches it by name,\n",
        "    // remove_file empties it, and remove_dir only succeeds because rmdir\n",
        "    // accepts an empty directory (the same call refuses a full one with\n",
        "    // ENOTEMPTY and unlink refuses a directory with EISDIR). This is the\n",
        "    // directory half of the filesystem an init uses to set up /tmp, /var.\n",
        "    let mut dok = std::fs::create_dir(\"/mdir\").is_ok();\n",
        "    if dok { dok = std::fs::write(\"/mdir/f\", msg).is_ok(); }\n",
        "    if dok { dok = std::fs::read(\"/mdir/f\").map(|g| g == msg).unwrap_or(false); }\n",
        "    if dok { dok = std::fs::remove_file(\"/mdir/f\").is_ok(); }\n",
        "    if dok { dok = std::fs::remove_dir(\"/mdir\").is_ok(); }\n",
        "    if dok { dok = std::fs::metadata(\"/mdir\").is_err(); }\n",
        "    if dok { println!(\"MPOS_MKDIR_OK\"); } else { println!(\"MPOS_MKDIR_FAILED\"); }\n",
        "    // rename + hard link: the two remaining name-edge operations a shell\n",
        "    // and every atomic-update pattern needs - `mv` (RENAMEAT) and `ln`\n",
        "    // (LINKAT). rename moves the name and the body but makes a new node;\n",
        "    // hard link gives the SAME node a second name, which is exactly the\n",
        "    // difference the last step pins: after removing /mrn_b the bytes must\n",
        "    // still read back through /mrn_c, which only holds if linkat shared the\n",
        "    // node rather than copying it.\n",
        "    let mut rok = std::fs::write(\"/mrn_a\", msg).is_ok();\n",
        "    if rok { rok = std::fs::rename(\"/mrn_a\", \"/mrn_b\").is_ok(); }\n",
        "    if rok { rok = std::fs::metadata(\"/mrn_a\").is_err(); }\n",
        "    if rok { rok = std::fs::read(\"/mrn_b\").map(|g| g == msg).unwrap_or(false); }\n",
        "    if rok { rok = std::fs::hard_link(\"/mrn_b\", \"/mrn_c\").is_ok(); }\n",
        "    if rok { rok = std::fs::read(\"/mrn_c\").map(|g| g == msg).unwrap_or(false); }\n",
        "    if rok { rok = std::fs::remove_file(\"/mrn_b\").is_ok(); }\n",
        "    if rok { rok = std::fs::read(\"/mrn_c\").map(|g| g == msg).unwrap_or(false); }\n",
        "    if rok { rok = std::fs::remove_file(\"/mrn_c\").is_ok(); }\n",
        "    if rok { println!(\"MPOS_RENAME_LINK_OK\"); } else { println!(\"MPOS_RENAME_LINK_FAILED\"); }\n",
        "    // ftruncate: change a file's length through its descriptor - the\n",
        "    // `: >file` truncate and `truncate -s N` idioms a shell uses, and the\n",
        "    // last core length handler not yet proven from a live process. Shrink\n",
        "    // only: the 10-byte write is trimmed to 4 then to 0, each confirmed by\n",
        "    // the stat size, because a truncate-to-zero that did not actually drop\n",
        "    // the bytes would leave a stale length and the file's content.\n",
        "    use std::io::Write;\n",
        "    let mut tok = false;\n",
        "    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).open(\"/mtrunc\") {\n",
        "        tok = f.write_all(b\"0123456789\").is_ok() && f.flush().is_ok();\n",
        "        tok = tok && f.set_len(4).is_ok()\n",
        "            && std::fs::metadata(\"/mtrunc\").map(|m| m.len() == 4).unwrap_or(false);\n",
        "        tok = tok && f.set_len(0).is_ok()\n",
        "            && std::fs::metadata(\"/mtrunc\").map(|m| m.len() == 0).unwrap_or(false);\n",
        "    }\n",
        "    let _ = std::fs::remove_file(\"/mtrunc\");\n",
        "    if tok { println!(\"MPOS_TRUNCATE_OK\"); } else { println!(\"MPOS_TRUNCATE_FAILED\"); }\n",
        "    // memory-handler proof: a 1 MiB heap buffer. Past musl's malloc\n",
        "    // threshold this is served by `mmap`, so writing one byte per page and\n",
        "    // reading the pattern back exercises the anonymous/mmap demand-paging\n",
        "    // path a running process depends on - the same region family the\n",
        "    // posix_spawn child stack faults in, so this pins (or locates) it.\n",
        "    let mut b = vec![0u8; 1024 * 1024];\n",
        "    let mut i = 0usize;\n",
        "    while i < b.len() { b[i] = (i / 4096) as u8; i += 4096; }\n",
        "    let memok = b[0] == 0 && b[409600] == 100 && b[1044480] == 255;\n",
        "    if memok { println!(\"MPOS_MEMORY_OK\"); } else { println!(\"MPOS_MEMORY_FAILED\"); }\n",
        "    // fd duplication: the redirection/pipe primitive a shell relies on.\n",
        "    // `File::try_clone` is `fcntl(F_DUPFD_CLOEXEC)`, and reading the\n",
        "    // duped descriptor must see the same file's bytes - the shared\n",
        "    // file-description semantics that make `cmd > file` and `cmd | cmd`\n",
        "    // possible. The read-back is the counterfactual: a dup that handed\n",
        "    // back an invalid or independent fd fails here, not silently.\n",
        "    use std::io::Read;\n",
        "    let mut dupok = std::fs::write(\"/mdup\", b\"0123456789\").is_ok();\n",
        "    if dupok {\n",
        "        if let Ok(mut f) = std::fs::File::open(\"/mdup\") {\n",
        "            if let Ok(mut c) = f.try_clone() {\n",
        "                let mut buf = Vec::new();\n",
        "                dupok = c.read_to_end(&mut buf).is_ok() && buf == b\"0123456789\";\n",
        "            } else { dupok = false; }\n",
        "        } else { dupok = false; }\n",
        "    }\n",
        "    let _ = std::fs::remove_file(\"/mdup\");\n",
        "    if dupok { println!(\"MPOS_DUP_OK\"); } else { println!(\"MPOS_DUP_FAILED\"); }\n",
        "    // stat(2) ABI check: st_mode type+perms, file-type, and size must all\n",
        "    // read back at the aarch64 struct-stat offsets. chmod 0o640 then the\n",
        "    // mode round-trips; the regular file reports is_file; a directory\n",
        "    // reports is_dir; and len() matches the bytes written. All four at once\n",
        "    // is the counterfactual for the aarch64 struct-stat field layout.\n",
        "    use std::os::unix::fs::PermissionsExt;\n",
        "    let mut sok = std::fs::write(\"/mstat\", b\"12345\").is_ok();\n",
        "    if sok {\n",
        "        sok = std::fs::metadata(\"/mstat\").map(|m| {\n",
        "            let mut p = m.permissions(); p.set_mode(0o640);\n",
        "            std::fs::set_permissions(\"/mstat\", p)\n",
        "        }).and_then(|r| r).is_ok();\n",
        "    }\n",
        "    if sok {\n",
        "        use std::os::linux::fs::MetadataExt;\n",
        "        sok = std::fs::metadata(\"/mstat\")\n",
        "            .map(|m| m.permissions().mode() & 0o777 == 0o640 && m.is_file()\n",
        "                && m.len() == 5 && m.st_nlink() == 1 && m.st_ino() != 0\n",
        "                && m.st_blocks() >= 1 && m.st_size() == 5)\n",
        "            .unwrap_or(false);\n",
        "    }\n",
        "    if sok { sok = std::fs::metadata(\"/dev\").map(|m| m.is_dir()).unwrap_or(false); }\n",
        "    if sok {\n",
        "        let _ = std::os::unix::fs::symlink(\"/dev\", \"/mlnk\");\n",
        "        sok = std::fs::symlink_metadata(\"/mlnk\").map(|m| m.is_symlink()).unwrap_or(false);\n",
        "        let _ = std::fs::remove_file(\"/mlnk\");\n",
        "    }\n",
        "    let _ = std::fs::remove_file(\"/mstat\");\n",
        "    if sok { println!(\"MPOS_STAT_OK\"); } else { println!(\"MPOS_STAT_FAILED\"); }\n",
        "}\n",
    );
    let src = build_dir.join("aarch64-musl-helper.rs");
    let out = build_dir.join("aarch64-musl-helper");
    std::fs::write(&src, SRC).with_context(|| format!("Failed to write {}", src.display()))?;

    let status = Command::new("rustc")
        .arg("--target")
        .arg("aarch64-unknown-linux-musl")
        .args(["-C", "target-feature=+crt-static"])
        .args(["-C", "linker=rust-lld"])
        .args(["-C", "strip=symbols"])
        .arg("-O")
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .context("Failed to run rustc for the aarch64 musl helper")?;
    if !status.success() {
        anyhow::bail!(
            "rustc failed building the aarch64 musl helper ({status}); the \
             aarch64-unknown-linux-musl target must be installed: \
             `rustup target add aarch64-unknown-linux-musl`"
        );
    }
    let bytes = std::fs::read(&out).with_context(|| format!("Failed to read {}", out.display()))?;
    // Sanity: it really is a static EM_AARCH64 ELF the loader will accept.
    if bytes.len() < 20
        || &bytes[0..4] != b"\x7fELF"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != EM_AARCH64
    {
        anyhow::bail!("the aarch64 musl helper is not an EM_AARCH64 ELF");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned instruction counts the tests assert the built code has, which
    /// also pin that the loop's backward branch spans exactly the body.
    const LOOP_WORDS: usize = 20;
    const LAUNCHER_WORDS: usize = 39;
    const TARGET_WORDS: usize = 43;

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

    /// The exec launcher is a loadable image that first takes a demand-paging
    /// fault (`sub sp` + `str`), writes `MPOS_FAULT_OK` if it survives, echoes,
    /// then calls `execve`. Words: [0]=`sub sp`, [1]=`str x19,[sp]`, [2]=`mov
    /// x8,#64` (the fault write), [8]=`mov x8,#63` (read), [14]=stash,
    /// [27]=`mov x8,#221` (execve), [35]=`mov x8,#94` (fallback exit_group), last
    /// `b .`. Its data blob carries the `/t\0` path, `argv[0]` = that path's
    /// address, NULL-terminated `argv`/`envp`, then `MPOS_FAULT_OK`.
    #[test]
    fn the_launcher_pages_then_echoes_then_execs() {
        let e = exec_launcher_init_elf();
        assert_loadable(&e);
        let words = words_of(&e, LAUNCHER_WORDS);
        assert_eq!(words.len(), LAUNCHER_WORDS);
        assert_eq!(words[0], 0xd14007ff, "sub sp,sp,#0x1000");
        assert_eq!(
            words[1], 0xf90003f3,
            "str x19,[sp] (the demand-paging fault)"
        );
        assert_eq!(words[2], 0xd2800808, "mov x8,#64 (fault-marker write)");
        assert_eq!(words[8], 0xd28007e8, "mov x8,#63 (read)");
        assert_eq!(words[14], 0xaa0003e9, "mov x9,x0 (stash n)");
        assert_eq!(words[27], 0xd2801ba8, "mov x8,#221 (execve)");
        assert_eq!(words[34], 0xd4000001, "svc (the execve)");
        assert_eq!(words[35], 0xd2800bc8, "mov x8,#94 (exit_group fallback)");
        assert_eq!(words[LAUNCHER_WORDS - 1], 0x14000000, "b . (park)");

        let data_file_off = CODE_OFF + LAUNCHER_WORDS * 4;
        assert_eq!(
            &e[data_file_off + 66..data_file_off + 69],
            b"/t\0",
            "the exec path"
        );
        let path_addr = VADDR + (data_file_off + 66) as u64;
        assert_eq!(
            &e[data_file_off + 72..data_file_off + 80],
            path_addr.to_le_bytes(),
            "argv[0] points at the /t path"
        );
        assert_eq!(
            &e[data_file_off + 80..data_file_off + 88],
            &[0u8; 8],
            "argv is NULL-terminated"
        );
        assert_eq!(
            &e[data_file_off + 88..data_file_off + 96],
            &[0u8; 8],
            "envp is a lone NULL"
        );
        assert_eq!(
            &e[data_file_off + 96..],
            FAULT_MARKER,
            "the fault marker the demand-paged store writes"
        );
    }

    /// The target writes the exec marker, forks (`clone`), and its parent `execve`s
    /// `/musl`: word 0 is the write, word 6 is `mov x8,#220` (clone), word 13 is the
    /// `cbnz` to the parent at word 24 (`imm19 = 11`), word 31 is `mov x8,#221`
    /// (execve), the last is `b .`. Its data carries `MPOS_EXEC_OK`, `MPOS_FORK_OK`,
    /// and the `/musl` path with `argv[0]` pointing at it.
    #[test]
    fn the_target_writes_then_forks() {
        let e = marker_target_elf();
        assert_loadable(&e);
        let words = words_of(&e, TARGET_WORDS);
        assert_eq!(words.len(), TARGET_WORDS);
        assert_eq!(words[0], 0xd2800808, "mov x8,#64 (write exec marker)");
        assert_eq!(words[6], 0xd2801b88, "mov x8,#220 (clone)");
        assert_eq!(words[13], 0xb5000160, "cbnz x0 -> word 24 (parent)");
        assert_eq!(words[31], 0xd2801ba8, "mov x8,#221 (execve /musl)");
        assert_eq!(words[TARGET_WORDS - 1], 0x14000000, "b . (parent park)");

        let data_file_off = CODE_OFF + TARGET_WORDS * 4;
        assert_eq!(
            &e[data_file_off..data_file_off + EXEC_MARKER.len()],
            EXEC_MARKER,
            "the exec marker at the first write's x1"
        );
        assert_eq!(
            &e[data_file_off + 13..data_file_off + 13 + FORK_MARKER.len()],
            FORK_MARKER,
            "the fork marker at the child write's x1"
        );
        assert_eq!(
            &e[data_file_off + 26..data_file_off + 32],
            b"/musl\0",
            "the execve path in the parent's tail"
        );
        let musl_path_addr = VADDR + (data_file_off + 26) as u64;
        assert_eq!(
            &e[data_file_off + 40..data_file_off + 48],
            musl_path_addr.to_le_bytes(),
            "argv[0] points at the /musl path"
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
