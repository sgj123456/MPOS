//! The aarch64 verdict's payload: the smallest real userspace programs, built
//! in pure Rust.
//!
//! Stage 6/7's image prerequisite. The kernel's ELF loader (`src/mm/elf.rs`) was
//! taught `EM_AARCH64` in 6g, and `spawn_init`/`load_user` map the segments and
//! jump to `e_entry` - but something must *produce* an aarch64 binary for the
//! payload. This file does, in the two ways the payloads need.
//!
//! The *hand-encoded* image is the verdict's exec target, hand-encoded on
//! purpose: a build step that shells out to `clang` plus a rustup-internal
//! `ld.lld` path would add a fragile host prerequisite to the boot path and make
//! the artifact non-hermetic (a test that has to run the compiler to know the
//! answer tests the compiler, not the image). It is a few dozen straight-line
//! instructions, so it fits in a page and every byte is pinned here against
//! `llvm-objdump`'s own decoding of the built image (encode, then disassemble,
//! then confirm), not a hand-guess at the bit layout:
//!
//! - [`marker_target_elf`] (the verdict's `/t`, exec'd by the payload's own
//!   pid 1 - [`exec_launcher_init_elf`] - which the initramfs script promotes
//!   into `/` before handing over)
//!   demand-pages its stack (`MPOS_FAULT_OK` only if the store survived - the
//!   pin any real, larger program needs), writes `MPOS_EXEC_OK`, forks via
//!   `clone` and `wait4`s, reads `/etc/inittab` through raw syscalls and prints
//!   its length (the switched root serving a userspace open/read/close whose
//!   byte count the command pins), and its parent then `execve`s `/musl` - an
//!   *unmodified* statically-linked musl/Rust binary that prints `MPOS_MUSL_OK` -
//!   so the wire proves `execve`, `clone`/`wait4`, demand paging, a real file
//!   read on the disk root, AND running a real musl binary at EL0 (the
//!   demand-paging handler services its larger stack/anon pages).
//!
//! The *rustc-built* programs are the ones that do real work, where hand-encoding
//! would be a toy of the thing rather than a smaller version of it: the sweep's
//! runner ([`build_sweep_init`]) and the unmodified-musl probe
//! ([`build_static_musl_helper`]). Both are static
//! `aarch64-unknown-linux-musl` binaries and share [`build_static_musl`],
//! because the flags that make such a binary - `+crt-static`, `rust-lld`,
//! stripped, `-O` - are one answer and not three.
//!
//! Two stand-ins have been retired from this module rather than left dead. The
//! interactive `/init` was once a third hand-encoded image (`looping_init_elf`:
//! an echo loop with no exit), then a musl program that made `/tmp` and execed
//! the payload's `sh` (`build_shell_init`); both existed while the machine had
//! no userland to boot, and since §13.19 `run --arch aarch64` boots the real
//! Alpine tree instead. The archive packer (`write_initramfs_entries`) went the
//! same way - the initramfs is a real busybox tree now (the shared pipeline in
//! `xtask/src/initramfs.rs`), not a tar of hand-encoded members.
//!
//! The verdict's launcher ([`exec_launcher_init_elf`]) is *not* one of those,
//! though §15.6 briefly made it look like one. That change moved the boot onto
//! the initramfs script's pivot and with it the `R:` echo the launcher had been
//! doing - correctly, since the script is the process that reads the console
//! first. What it also assumed was that the disk's pid 1 could then be a script.
//! It cannot: the test disk has no interpreter to name, and the boot died at
//! `switch_root: can't execute '/init'`. The launcher is back as what it always
//! was - the payload's own pid 1 - and the echo stays with the script.

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
/// scratch buffer and the strings; `W` so the loader maps it writable (the
/// `read` lands in the scratch buffer).
const PF_R_W_X: u32 = 7;

/// Where the image is mapped: the low text address a static aarch64 executable
/// uses, comfortably inside the user span and below `USER_LIMIT`.
const VADDR: u64 = 0x40_0000;

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
/// `MOVN Xd, #imm16` - the complement form, for the negated immediates the
/// syscall ABI carries (`AT_FDCWD` is `-100`, which no `MOVZ` can name because
/// `MOVZ` zero-extends its immediate).
fn movn(rd: u32, imm: u32) -> u32 {
    0x9280_0000u32 | ((imm & 0xffff) << 5) | rd
}
/// `SVC #0`.
fn svc() -> u32 {
    0xd400_0001
}
/// `MOV Xd, Xm` (the `ORR Xd, XZR, Xm` alias).
fn movreg(rd: u32, rm: u32) -> u32 {
    0xaa00_03e0u32 | (rm << 16) | rd
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

/// `SUBS XZR, Xn, #imm` - compare against an immediate without writing a
/// destination: `0xF100001F | (imm12<<10) | (Rn<<5)`.
///
/// `SUBS`, not `ADDS`: `0xB100001F` has the same N-flag behaviour for a zero
/// immediate (which is the only immediate used here) but decodes as `cmn`, and a
/// helper named `cmp_imm` that assembles `cmn` is a trap set for the next
/// reader. Measured with `llvm-mc -show-encoding`.
fn cmp_imm(rn: u32, imm: u32) -> u32 {
    0xf100_001fu32 | ((imm & 0xfff) << 10) | (rn << 5)
}

/// `B.LT label` - branch when the N-flag says "less than":
/// `0x54000000 | (imm19 << 5) | 0b1011`. The target uses it to walk out of the
/// inittab probe when a syscall answered a negative errno.
fn b_lt(from_word: usize, to_word: usize) -> u32 {
    let d = ((to_word - from_word) as i32) & 0x7ffff;
    0x5400_0000u32 | ((d as u32) << 5) | 0b1011
}

/// `ADD Xd, Xm, #imm12` (unsigned offset, no shift).
fn add_imm(rd: u32, rm: u32, imm: u32) -> u32 {
    0x9100_0000u32 | ((imm & 0xfff) << 10) | (rm << 5) | rd
}

/// `STRB Wt, [Xn]` - byte store at an un-offset register address. The unsigned
/// immediate form with `imm12 = 0` (`0x39000000`), not the `0x3E000000` that
/// reads like "size 00, byte" and is in fact a SIMD store against a register
/// offset - which is what an earlier version of this helper assembled, storing
/// `b2` at `[x21 + x0]` and faulting instead of printing the digit.
fn strb_at(rt: u32, rn: u32) -> u32 {
    0x3900_0000u32 | (rn << 5) | rt
}

/// `UDIV Xd, Xn, Xm` - unsigned divide. The odd-looking `0x800` is the
/// instruction's own opcode field (bits 15..10 = `0b000010`); without it the
/// word is not a divide at all, and the CPU took an unknown-reason exception in
/// the middle of the digit print - measured, and the reason every helper here
/// now has a pinned encoding in the tests below.
fn udiv(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9ac0_0800u32 | (rm << 16) | (rn << 5) | rd
}

/// `MSUB Xd, Xn, Xm, Xa` - `Xd = Xa - Xn*Xm`, the remainder half of a
/// divide-and-mod pair. `0x9b00_8000`, not `0x9b00_0000`: bit 15 is MSUB's
/// `o0 = 1`, and leaving it clear assembles `MADD` - the *reverse* of the
/// operation wanted, which would have printed `bytes=` followed by a quotient.
fn msub(rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
    0x9b00_8000u32 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}
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
/// `aarch64` asm-generic syscall numbers used below (Linux's, from `nr.rs`'s
/// aarch64 table): `open`, `close`, `read`, `write`, `execve`, `clone`, `wait4`
/// and `exit_group`. `SIGCHLD` is the `clone` exit signal so the parent's
/// `wait4` reaps the child.
///
/// `NR_OPENAT`, not `open`: aarch64's asm-generic table has no `open` at all -
/// the number `nr_aarch64.rs` records for it (`64000`) is a sentinel that is
/// deliberately unreachable - and 56 is `openat`. Measured: the inittab probe
/// below asked for 56 with the *path* in `x0`, which is `openat`'s `dirfd`, so
/// the call answered a negative errno, the probe skipped its print, and
/// `boot-aarch64` failed on `MPOS_INITTAB_OK` never appearing.
const NR_OPENAT: u32 = 56;
const NR_CLOSE: u32 = 57;
const NR_READ: u32 = 63;
const NR_WRITE: u32 = 64;
const NR_EXECVE: u32 = 221;
const NR_CLONE: u32 = 220;
const NR_WAIT4: u32 = 260;
const NR_EXIT_GROUP: u32 = 94;
const SIGCHLD: u32 = 17;
/// `AT_FDCWD` (`-100`), which `openat` takes in `x0` to mean "resolve the path
/// from the caller's cwd". A negative immediate, so it is materialised with
/// `movn` - and `movn` writes the *complement* of its immediate, so what is
/// stored here is that complement and not the value. Written as the expression
/// rather than as a bare `99` so a reader can connect it back to the name; the
/// alternative is a magic number in the code stream.
const AT_FDCWD: u32 = (!(-100i32)) as u32 & 0xffff;

/// The string the exec target writes when it runs - proof aarch64 `execve`
/// replaced the image and the new program ran at EL0. Nothing else prints it.
const EXEC_MARKER: &[u8] = b"MPOS_EXEC_OK\n";
/// The string the target writes only after its deliberately-far `str` to a
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
/// The path the target reads through raw syscalls after its fork round trip -
/// the switched root's own file, staged by the overlay pair. Its byte count is
/// what `boot-aarch64` pins against the staging tree, so the read proves the
/// disk serves the repository's tree to a userspace `open`/`read`, not merely
/// that a filesystem mounted (§15.6 moved this pin from the kernel's `/mnt`
/// read here, where the same claim is made about the *switched* root).
const INITTAB_PATH: &[u8] = b"/etc/inittab\0";
/// The prefix of the line the target prints with the length it read back.
const INITTAB_MARKER: &[u8] = b"MPOS_INITTAB_OK bytes=";
/// How many bytes the inittab read asks for. The staged file is a few hundred,
/// so one ask either returns the whole thing or says the tree is different from
/// the one the pin expects; a negative errno skips the print entirely, which is
/// the honest failure (a missing marker, not a wrong number).
const INITTAB_LEN: u32 = 4096;
/// The exec target: demand-pages its stack (`sub sp` + `str`, then
/// `MPOS_FAULT_OK` only if the store survived), writes `MPOS_EXEC_OK` (proving
/// aarch64 `execve` transferred control here and ran at EL0), then forks. The
/// child writes `MPOS_FORK_OK` and exits; the parent `wait4`s for it,
/// `openat`s `/etc/inittab`, reads it, closes it and prints
/// `MPOS_INITTAB_OK bytes=<n>` - the switched root serving a userspace file
/// read whose length the command pins - and finally `execve`s `/musl`, an
/// *unmodified* statically-linked musl/Rust binary that prints `MPOS_MUSL_OK`.
/// The wire therefore proves `execve`, `clone`/`wait4`, demand paging, a real
/// `openat`/read/close on the disk root, AND running a real musl binary at EL0.
/// The parent's `exit_group` is only a fallback if that `execve` fails. Word
/// layout (entry = 0):
///   0..7   demand-paging store, then write(1, "MPOS_FAULT_OK\n", 14)
///   8..13  write(1, "MPOS_EXEC_OK\n", 13)
///   14..20 clone(SIGCHLD, 0,0,0,0) -> x0 = child pid (parent) or 0 (child)
///   21     cbnz x0, 32   (parent jumps; child falls through)
///   22..31 child: write(1, "MPOS_FORK_OK\n", 13); exit_group(0); b .
///   32..38 parent: wait4(0, &status, 0, NULL)
///   39..45 parent: openat(AT_FDCWD, "/etc/inittab", O_RDONLY=0, 0)
///   46..47 cmp x0,#0 ; b.lt 95   (a failed openat skips the probe, honestly)
///   48     mov x19, x0 (fd)
///   49..54 read(fd, buf, 4096) -> n in x0
///   55..56 cmp x0,#0 ; b.lt 95   (a failed read likewise)
///   57     mov x20, x0 (n)
///   58..60 close(fd)
///   61..66 write(1, "MPOS_INITTAB_OK bytes=", 22)
///   67..68 x21 = digit_base
///   69..73 place 1000: UDIV / MSUB / ADD 48 / STRB
///   74..79 place 100 (advance x21 first)
///   80..85 place 10
///   86..88 place 1: the remainder stored directly
///   89..94 write(1, digit_base, 4)
///   95..100 write(1, "\n", 1)
///   101..108 execve("/musl", ["/musl", NULL], [NULL])
///   109..112 exit_group(0); b .  (fallback only)
pub fn marker_target_elf() -> Vec<u8> {
    let load = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32),
            movk(rd, ((addr >> 16) & 0xffff) as u32),
        ]
    };

    // The code length is fixed by the block below; the blob addresses depend on
    // it, and the debug_assert at the end is what keeps them honest.
    let n = 113usize;
    let data_base = VADDR + (CODE_OFF + n * 4) as u64;
    // Blob offsets from `data_base`:
    //   0..14   "MPOS_FAULT_OK\n"
    //   14..27  "MPOS_EXEC_OK\n"
    //   27..40  "MPOS_FORK_OK\n"
    //   40..46  "/musl\0"
    //   46..56  pad
    //   56..64  argv[0] = &"/musl"
    //   64..72  envp = [NULL]
    //   72..80  wait4 status word
    //   80..94  "/etc/inittab\0" (+pad)
    //   94..116 "MPOS_INITTAB_OK bytes="
    //   116..117 "\n"
    //   117..128 pad
    //   128..   the read scratch buffer (INITTAB_LEN bytes); the four digit
    //           bytes land at its very end (offsets LEN-4 .. LEN).
    let fault_addr = data_base;
    let exec_addr = data_base + 14;
    let fork_addr = data_base + 27;
    let musl_path_addr = data_base + 40;
    let argv_addr = data_base + 56;
    let envp_addr = data_base + 64;
    let status_addr = data_base + 72;
    let inittab_addr = data_base + 80;
    let ittab_marker_addr = data_base + 94;
    let newline_addr = data_base + 116;
    let buf_addr = data_base + 128;
    // The digits go at the tail of the scratch buffer: the read fills at most
    // `n` bytes from the front, and every inittab this repository stages is far
    // shorter than 4 KiB minus 4, so the two uses cannot collide. A >4092-byte
    // inittab would make the *pin* fail first (the read caps at 4096 and the
    // expected byte count stops matching), which is the loud direction.
    let digit_base = buf_addr + (INITTAB_LEN as u64 - 4);

    let mut code: Vec<u32> = Vec::new();
    // Demand-paging probe first: `sp -= 0x1000` lands one page below the mapped
    // stack; `str x19,[sp]` faults there. If `el0_sync`'s handler demand-pages
    // it, the store succeeds and we fall through to the write; otherwise it is
    // SIGSEGV and `MPOS_FAULT_OK` never appears.
    code.extend([SUB_SP_PAGE, STR_X19_SP]);
    code.extend([movz(8, NR_WRITE), movz(0, 1)]);
    code.extend(load(1, fault_addr));
    code.extend([movz(2, FAULT_MARKER.len() as u32), svc()]); // 0..7
    // write(1, exec_marker, 13) - the exec confirmation, run once before forking.
    code.extend([movz(8, NR_WRITE), movz(0, 1)]);
    code.extend(load(1, exec_addr));
    code.extend([movz(2, EXEC_MARKER.len() as u32), svc()]); // 8..13
    // clone(SIGCHLD, child_stack=0, ptid=0, ctid=0, tls=0): the aarch64 `fork`.
    code.extend([
        movz(8, NR_CLONE),
        movz(0, SIGCHLD),
        movz(1, 0),
        movz(2, 0),
        movz(3, 0),
        movz(4, 0),
        svc(),
    ]); // 14..20
    code.push(cbnz_x0(21, 32)); // 21 - parent (x0 != 0) jumps to 32
    // child: write(1, fork_marker, 13); exit_group(0); b .
    code.extend([movz(8, NR_WRITE), movz(0, 1)]);
    code.extend(load(1, fork_addr));
    code.extend([movz(2, FORK_MARKER.len() as u32), svc()]); // 22..27
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]); // 28..31
    // parent: wait4(pid=0, &status, options=0, rusage=NULL).
    code.extend([movz(8, NR_WAIT4), movz(0, 0)]); // 32..33
    code.extend(load(1, status_addr)); // 34..35
    code.extend([movz(2, 0), movz(3, 0), svc()]); // 36..38
    // parent: open("/etc/inittab", O_RDONLY=0, mode=0). A negative fd (ENOENT,
    // EACCES, ...) must not reach `read`, and must not print a fake length:
    // branch out to the newline write when the sign says errno, so the marker
    // line never appears rather than appearing with a wrong number.
    // `openat(AT_FDCWD, "/etc/inittab", O_RDONLY, 0)`: x8=nr, x0=dirfd, x1=path,
    // x2=flags, x3=mode. `openat` and not `open` because aarch64 has no `open`
    // (see `NR_OPENAT`), and `AT_FDCWD` and not the path in `x0` for the same
    // reason - the measurement that put these words here is the same one.
    code.extend([movz(8, NR_OPENAT)]); // 39
    code.extend([movn(0, AT_FDCWD)]); // 40  mov x0, #-100
    code.extend(load(1, inittab_addr)); // 41..42
    code.extend([movz(2, 0), movz(3, 0), svc()]); // 43..45
    code.extend([cmp_imm(0, 0), b_lt(46, 95)]); // 46..47
    code.push(movreg(19, 0)); // 48  mov x19, x0 (fd)
    // read(fd, buf, INITTAB_LEN): x0=fd, x1=&buf, x2=len.
    code.extend([movz(8, NR_READ)]); // 49
    code.push(movreg(0, 19)); // 50
    code.extend(load(1, buf_addr)); // 51..52
    code.extend([movz(2, INITTAB_LEN), svc()]); // 53..54
    code.extend([cmp_imm(0, 0), b_lt(55, 95)]); // 55..56
    code.push(movreg(20, 0)); // 57  mov x20, x0 (n)
    // close(fd).
    code.extend([movz(8, NR_CLOSE)]); // 58
    code.push(movreg(0, 19)); // 59
    code.push(svc()); // 60
    // write(1, "MPOS_INITTAB_OK bytes=", 22).
    code.extend([movz(8, NR_WRITE), movz(0, 1)]); // 61..62
    code.extend(load(1, ittab_marker_addr)); // 63..64
    code.extend([movz(2, INITTAB_MARKER.len() as u32), svc()]); // 65..66
    // Decimal of n (<= 4096) as exactly four ASCII digits, thousands first,
    // written at the scratch tail. Each place divides the running value by its
    // weight (`UDIV`), multiplies back and subtracts (`MSUB`) to keep the
    // remainder, turns the quotient into a character (`ADD #48`) and stores it
    // (`STRB`). The last place stores the remainder itself - nothing left to
    // divide. Leading zeros are printed (`bytes=0042`): the pin compares the
    // same fixed width the command formats, so no parsing rule is invented here.
    code.extend(load(21, digit_base)); // 67..68
    // place 1000:
    code.extend([
        movz(1, 1000),
        udiv(2, 20, 1),
        msub(20, 2, 1, 20),
        add_imm(2, 2, 48),
        strb_at(2, 21),
    ]); // 69..73
    // place 100:
    code.extend([
        add_imm(21, 21, 1),
        movz(1, 100),
        udiv(2, 20, 1),
        msub(20, 2, 1, 20),
        add_imm(2, 2, 48),
        strb_at(2, 21),
    ]); // 74..79
    // place 10:
    code.extend([
        add_imm(21, 21, 1),
        movz(1, 10),
        udiv(2, 20, 1),
        msub(20, 2, 1, 20),
        add_imm(2, 2, 48),
        strb_at(2, 21),
    ]); // 80..85
    // place 1: the remainder is the last digit.
    code.extend([add_imm(21, 21, 1), add_imm(2, 20, 48), strb_at(2, 21)]); // 86..88
    // write(1, digit_base, 4).
    code.extend([movz(8, NR_WRITE), movz(0, 1)]); // 89..90
    code.extend(load(1, digit_base)); // 91..92
    code.extend([movz(2, 4), svc()]); // 93..94
    // write(1, "\n", 1) - also where a failed openat/read jumped to: printing
    // the newline alone leaves the line prefix absent, which is what the pin
    // reads as "no answer", never as a zero-length file.
    code.extend([movz(8, NR_WRITE), movz(0, 1)]); // 95..96
    code.extend(load(1, newline_addr)); // 97..98
    code.extend([movz(2, 1), svc()]); // 99..100
    // execve("/musl", argv=["/musl",NULL], envp=[NULL]) - the real binary.
    code.extend([movz(8, NR_EXECVE)]); // 101
    code.extend(load(0, musl_path_addr)); // 102..103
    code.extend(load(1, argv_addr)); // 104..105
    code.extend(load(2, envp_addr)); // 106..107
    code.push(svc()); // 108
    // Fallback if that execve failed: exit_group(0); b .
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]); // 109..112
    debug_assert_eq!(code.len(), n, "fork target word count must match");

    // Data blob: the markers, the paths, the wait4 status word, the pointer
    // arrays, and the read scratch. The digit tail stays zero until the run
    // fills it.
    let mut data = vec![0u8; 128 + INITTAB_LEN as usize];
    data[0..FAULT_MARKER.len()].copy_from_slice(FAULT_MARKER);
    data[14..14 + EXEC_MARKER.len()].copy_from_slice(EXEC_MARKER);
    data[27..27 + FORK_MARKER.len()].copy_from_slice(FORK_MARKER);
    data[40..46].copy_from_slice(b"/musl\0");
    data[56..64].copy_from_slice(&musl_path_addr.to_le_bytes()); // argv[0] = &"/musl"
    data[80..80 + INITTAB_PATH.len()].copy_from_slice(INITTAB_PATH);
    data[94..94 + INITTAB_MARKER.len()].copy_from_slice(INITTAB_MARKER);
    data[116] = b'\n';
    assemble_with(&code, &data)
}

/// The verdict's disk `/init`: the smallest program that can stand as pid 1 on
/// the *test* disk, `execve`-ing [`marker_target_elf`] as `/t` so the target
/// runs as the image that replaced it.
///
/// A program, not the `#!/bin/sh` script the Alpine disk carries, and for a
/// reason that is about the disk rather than about style: the test disk is the
/// build's own payload, so it has no package manager and therefore no busybox -
/// an interpreter is a file that disk simply does not have, and pid 1 cannot be
/// a script whose interpreter is absent. Measured: leaving the overlay's
/// `rootfs-aarch64/init` (the Alpine chain: applet farm, mounts, `exec
/// /sbin/init`) to stand as the test disk's `/init` made the pivot die at
/// `switch_root: can't execute '/init': No such file or directory` - the
/// `ENOENT` of a missing `/bin/sh`, reported against the script's own name -
/// after which `/t` never ran and every marker the verdict polls was absent.
/// The sweep's runner ([`build_sweep_init`]) stages a program as `/init` for
/// the same reason; this is that shape reduced to the one thing the verdict's
/// chain needs, an `execve`.
///
/// Nothing else lives here. The console echo the verdict pins is the initramfs
/// script's now (`initramfs-aarch64/init`), which runs on the ramfs *before*
/// this program is reached; a second reader here would race it for the one line
/// the handoff queued.
///
/// Word layout (entry = 0), data blob at `data_base`:
///   0..3   "/t\0"
///   4..8   padding, so the pointer table is 8-aligned
///   8..24  argv = [&"/t", NULL]
///   24..32 envp = [NULL]
pub fn exec_launcher_init_elf() -> Vec<u8> {
    let load = |rd: u32, addr: u64| {
        [
            movz(rd, (addr & 0xffff) as u32),
            movk(rd, ((addr >> 16) & 0xffff) as u32),
        ]
    };
    // 1 + 2 + 2 + 2 + 1 + 4 = 12 words: the execve, its three operands, the
    // `svc`, and the exit_group fallback for the case where it returned.
    let n = 12usize;
    let data_base = VADDR + (CODE_OFF + n * 4) as u64;
    let path_addr = data_base;
    let argv_addr = data_base + 8;
    let envp_addr = data_base + 24;

    let mut code: Vec<u32> = vec![movz(8, NR_EXECVE)];
    code.extend(load(0, path_addr));
    code.extend(load(1, argv_addr));
    code.extend(load(2, envp_addr));
    code.push(svc());
    // Fallback if that `execve` failed: leave through `exit_group`, so the
    // verdict still terminates on `INIT_EXITED` instead of spinning.
    code.extend([movz(8, NR_EXIT_GROUP), movz(0, 0), svc(), 0x1400_0000]);
    debug_assert_eq!(code.len(), n, "launcher word count must match");

    let mut data = vec![0u8; 32];
    data[0..3].copy_from_slice(b"/t\0");
    data[8..16].copy_from_slice(&path_addr.to_le_bytes()); // argv[0] = &"/t"
    // argv[1] and envp[0] are the zero-init above: NULL terminates both arrays.
    assemble_with(&code, &data)
}

fn build_static_musl(build_dir: &Path, stem: &str, src: &str, what: &str) -> Result<Vec<u8>> {
    let path = build_dir.join(format!("{stem}.rs"));
    let out = build_dir.join(stem);
    std::fs::write(&path, src).with_context(|| format!("Failed to write {}", path.display()))?;

    let status = Command::new("rustc")
        .arg("--target")
        .arg("aarch64-unknown-linux-musl")
        .args(["-C", "target-feature=+crt-static"])
        .args(["-C", "linker=rust-lld"])
        .args(["-C", "strip=symbols"])
        .arg("-O")
        .arg(&path)
        .arg("-o")
        .arg(&out)
        .status()
        .with_context(|| format!("Failed to run rustc for {what}"))?;
    if !status.success() {
        anyhow::bail!(
            "rustc failed building {what} ({status}); the \
             aarch64-unknown-linux-musl target must be installed: \
             `rustup target add aarch64-unknown-linux-musl`"
        );
    }
    let bytes = std::fs::read(&out).with_context(|| format!("Failed to read {}", out.display()))?;
    // Sanity: it really is a static EM_AARCH64 ELF the loader will accept. The
    // loader matches on the machine, so a host build that leaked through here
    // would be a payload the machine refuses rather than a program that fails.
    if bytes.len() < 20
        || &bytes[0..4] != b"\x7fELF"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != EM_AARCH64
    {
        anyhow::bail!("{what} is not an EM_AARCH64 ELF");
    }
    Ok(bytes)
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
/// the mode the staging step wrote. That step marks every file it lays down
/// executable (`aarch64::stage_program`) - it has to, or `/bin`'s tools would land
/// mode 0644 and answer `EACCES` at `execve` - and the runner re-marks each
/// `/t-*` it starts anyway, so a program that will not run fails as itself
/// rather than as a mode bit some other step forgot.
///
/// It also does the part of the x86 `/init` that is not about running tests at
/// all: the machine setup. `/tmp` is made, because two of the programs build a
/// scratch tree there, and the runner's `set_up_machine` says why a directory is
/// the honest version of the tmpfs the x86 harness mounts. A `=== SETUP_FAIL`
/// line is the one thing the harness refuses to read past.
pub fn build_sweep_init(build_dir: &Path) -> Result<Vec<u8>> {
    const SRC: &str = r#"use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

/// `mount(2)`, whose only caller is [`set_up_machine`]. Declared rather than
/// imported because this runner is compiled by plain `rustc` against musl's
/// headers' absence: the signature is Linux's, and the flags word is an
/// `unsigned long`.
unsafe extern "C" {
    fn mount(
        source: *const core::ffi::c_char,
        target: *const core::ffi::c_char,
        fstype: *const core::ffi::c_char,
        flags: usize,
        data: *const core::ffi::c_void,
    ) -> i32;
}

/// The machine the x86_64 sweep's `/init` sets up with busybox, done again here,
/// where there is no shell to do it.
///
/// `/tmp` is the one thing this payload needs that the machine does not already
/// have, and it is a *tmpfs* here for the same reason the first machine mounts
/// one: the filesystem a name is created in decides what the name can be. This
/// machine's root is ext4, and an ext4 directory cannot hold a socket node - so
/// `t-unix-rustix`, whose subject is exactly an `AF_UNIX` name, answered
/// EOPNOTSUPP from `create(name, SOCK_FILE)` while its `/tmp` was a directory on
/// the disk. Backed by the same tmpfs the x86 harness uses, the name is a kind
/// the filesystem can hold, and the test is about the socket rather than about
/// ext4. `t-rename` and `t-rmdir` need the directory too, which is why it exists
/// at all.
///
/// A setup failure prints `=== SETUP_FAIL` rather than being left to surface as
/// the tests' own failures: a machine that was not set up is not evidence about
/// the kernel, and a verdict that read those ENOENTs as the tests' would be
/// blaming them for the harness. The reader refuses to report anything else when
/// the line is there. Nothing **here** mounts anything else and no interface is
/// brought up here - but this file is not the whole payload, and this comment
/// used to say the machine "reaches neither /proc, /sys nor the network", which
/// is wrong about `/proc` and `/sys` and was measured wrong in S6 batch 11: the
/// disk's own chain runs first (`rootfs-aarch64/init` mounts `/proc`, `/sys`,
/// `/tmp`, `/run` and `devpts`), so a test may find a `/proc` this file never
/// made - `t-fd-list` and `t-tmpfile-link` both report "the payload's own
/// /proc" on this machine. The tests that want a live wire are the socket ones -
/// which this sweep stages, because the stack `check_net` publishes and `main`
/// initializes is what serves them.
fn set_up_machine() {
    // The mountpoint has to exist before anything hangs on it - Linux requires
    // it and so does this kernel's `mount` (`ENOTDIR` otherwise).
    match std::fs::create_dir("/tmp") {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => println!("=== SETUP_FAIL /tmp: {e}"),
    }
    let (source, target, fstype) = (cstr("tmpfs"), cstr("/tmp"), cstr("tmpfs"));
    // SAFETY: three NUL-terminated strings that outlive the call, no data.
    let answer = unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            core::ptr::null(),
        )
    };
    if answer != 0 {
        println!(
            "=== SETUP_FAIL mount tmpfs /tmp: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// A `&str` as the C string a syscall wants.
fn cstr(text: &str) -> std::ffi::CString {
    std::ffi::CString::new(text).expect("a literal with no interior NUL")
}

/// The command line the x86_64 sweep's shell hands a program.
///
/// The harness has exactly one per-test special case and it is copied here,
/// because argv *is* what that test is about: t-stdargs pins that std reads back
/// `t-stdargs one two three`, and the first word is load-bearing - a path there
/// ("/t-stdargs", which is what a plain `Command::new("/t-stdargs")` hands over)
/// is a different argv[0]. The x86 harness gets the bare word from a PATH
/// lookup, so it runs `(cd /bin && PATH=/bin t-stdargs one two three)`; there is
/// no shell here to look anything up, so the runner says the same thing
/// directly. Every other program takes no arguments - the x86 harness runs them
/// as they are.
fn command(path: &str, name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(path);
    if name == "t-stdargs" {
        cmd.arg0("t-stdargs");
        cmd.args(["one", "two", "three"]);
    }
    cmd
}

fn main() {
    let dir = "/";
    set_up_machine();
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
        match command(&path, name).status() {
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
    build_static_musl(
        build_dir,
        "aarch64-sweep-init",
        SRC,
        "the aarch64 sweep init",
    )
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
    build_static_musl(
        build_dir,
        "aarch64-musl-helper",
        SRC,
        "the aarch64 musl helper",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned instruction count the test asserts the built code has - the
    /// same number `marker_target_elf`'s own `debug_assert_eq!` checks, written
    /// down twice on purpose: the blob's addresses are derived from it, so a
    /// drift between the function and its pin is exactly the failure this file
    /// exists to catch (a hand-encoded image whose pointers moved under it).
    const TARGET_WORDS: usize = 113;

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

    /// Every hand-encoded instruction, pinned to the encoding the *assembler*
    /// produces - so a helper whose bit pattern is wrong fails here rather than
    /// three minutes later on the wire, where it reads as "the program died".
    ///
    /// The expected words were taken from `llvm-mc --triple=aarch64
    /// -show-encoding` over the same mnemonics. Each one below is a bug that
    /// this file shipped and this test would have caught:
    ///
    /// * `udiv` without bits 15..10 = `0b000010` is not a divide at all, and the
    ///   CPU answered an unknown-reason exception mid-print;
    /// * `msub` without bit 15 assembles `madd` - the reverse operation;
    /// * `strb` built on `0x3E000000` is a *SIMD* store at a register offset, so
    ///   it wrote `b2` to `[x21 + x0]` instead of the digit to `[x21]`;
    /// * `cmp` built on `0xB100001F` decodes as `cmn`.
    #[test]
    fn every_hand_encoded_instruction_is_what_the_assembler_emits() {
        assert_eq!(udiv(2, 20, 1), 0x9ac1_0a82, "udiv x2, x20, x1");
        assert_eq!(msub(20, 2, 1, 20), 0x9b01_d054, "msub x20, x2, x1, x20");
        assert_eq!(strb_at(2, 21), 0x3900_02a2, "strb w2, [x21]");
        assert_eq!(cmp_imm(0, 0), 0xf100_001f, "cmp x0, #0");
        assert_eq!(add_imm(2, 20, 48), 0x9100_c282, "add x2, x20, #48");
        assert_eq!(movz(8, 64), 0xd280_0808, "movz x8, #64");
        assert_eq!(movk(1, 64), 0xf2a0_0801, "movk x1, #64, lsl #16");
        assert_eq!(movn(0, 99), 0x9280_0c60, "movn x0, #99");
        assert_eq!(
            movn(0, AT_FDCWD),
            0x9280_0c60,
            "the AT_FDCWD materialisation is `mov x0, #-100`"
        );
        assert_eq!(movreg(19, 0), 0xaa00_03f3, "mov x19, x0");
        assert_eq!(svc(), 0xd400_0001, "svc #0");
        assert_eq!(SUB_SP_PAGE, 0xd140_07ff, "sub sp, sp, #0x1000");
        assert_eq!(STR_X19_SP, 0xf900_03f3, "str x19, [sp]");
    }

    /// The target writes the fault marker after its demand-paging store, the
    /// exec marker, forks (`clone`), and its parent reads `/etc/inittab`, prints
    /// the byte count, then execve's `/musl`. Words: [0]=`sub sp`, [1]=`str
    /// x19,[sp]`, [2]=`mov x8,#64` (the fault write), [8+6]=`mov x8,#220`
    /// (clone), the `cbnz` splits parent from child, the parent's probe starts
    /// with `mov x8,#56` (`openat`, with `AT_FDCWD` in `x0`), and the last word
    /// is `b .`. Its data carries the three markers, the `/musl` path with
    /// `argv[0]` pointing at it, the `/etc/inittab` path and the inittab line's
    /// prefix.
    #[test]
    fn the_target_pages_then_writes_then_forks_then_reads() {
        let e = marker_target_elf();
        assert_loadable(&e);
        let words = words_of(&e, TARGET_WORDS);
        assert_eq!(words.len(), TARGET_WORDS);
        assert_eq!(words[0], 0xd14007ff, "sub sp,sp,#0x1000");
        assert_eq!(
            words[1], 0xf90003f3,
            "str x19,[sp] (the demand-paging fault)"
        );
        assert_eq!(words[2], 0xd2800808, "mov x8,#64 (fault-marker write)");
        // clone at word 14: `mov x8,#220`.
        assert_eq!(words[14], movz(8, NR_CLONE), "mov x8,#220 (clone)");
        // The parent/child split at word 21 jumps to the wait4 at 32.
        assert_eq!(words[21], cbnz_x0(21, 32), "cbnz x0 -> word 32 (parent)");
        // The probe's open: `mov x8,#56` is `openat` on aarch64, and the dirfd
        // it is handed is `AT_FDCWD` - not the path, which is what a reader
        // written against x86_64's `open` would put there (the bug this pins).
        assert_eq!(
            words[39],
            movz(8, NR_OPENAT),
            "mov x8,#56 (openat, not open)"
        );
        assert_eq!(words[40], movn(0, AT_FDCWD), "mov x0,#-100 (AT_FDCWD)");
        assert_eq!(
            words[41],
            movz(
                1,
                (VADDR + (CODE_OFF + TARGET_WORDS * 4) as u64 + 80) as u32 & 0xffff
            ),
            "movz x1, #lo(&/etc/inittab) - the path is the second argument"
        );
        // The openat guard: `cmp x0,#0` then `b.lt` to the newline write at 95.
        assert_eq!(words[46], cmp_imm(0, 0), "cmp x0,#0 (the fd sign test)");
        assert_eq!(words[47], b_lt(46, 95), "b.lt -> word 95 (skip the probe)");
        // The read guard branches to the same place.
        assert_eq!(words[55], cmp_imm(0, 0), "cmp x0,#0 (the length sign test)");
        assert_eq!(words[56], b_lt(55, 95), "b.lt -> word 95");
        // A digit place: word 69 loads the weight 1000 into x1.
        assert_eq!(words[69], movz(1, 1000), "movz x1,#1000 (thousands)");
        // The last word is the fallback park.
        assert_eq!(words[TARGET_WORDS - 1], 0x1400_0000, "b . (parent park)");

        let data_file_off = CODE_OFF + TARGET_WORDS * 4;
        assert_eq!(
            &e[data_file_off..data_file_off + FAULT_MARKER.len()],
            FAULT_MARKER,
            "the fault marker at the first write's x1"
        );
        assert_eq!(
            &e[data_file_off + 14..data_file_off + 14 + EXEC_MARKER.len()],
            EXEC_MARKER,
            "the exec marker at the second write's x1"
        );
        assert_eq!(
            &e[data_file_off + 27..data_file_off + 27 + FORK_MARKER.len()],
            FORK_MARKER,
            "the fork marker at the child write's x1"
        );
        assert_eq!(
            &e[data_file_off + 40..data_file_off + 46],
            b"/musl\0",
            "the execve path in the parent's tail"
        );
        let musl_path_addr = VADDR + (data_file_off + 40) as u64;
        assert_eq!(
            &e[data_file_off + 56..data_file_off + 64],
            musl_path_addr.to_le_bytes(),
            "argv[0] points at the /musl path"
        );
        assert_eq!(
            &e[data_file_off + 80..data_file_off + 80 + INITTAB_PATH.len()],
            INITTAB_PATH,
            "the inittab path the probe opens"
        );
        assert_eq!(
            &e[data_file_off + 94..data_file_off + 94 + INITTAB_MARKER.len()],
            INITTAB_MARKER,
            "the inittab line's prefix"
        );
        assert_eq!(e[data_file_off + 116], b'\n', "the line terminator");
        // The scratch buffer reaches to the end of the blob, and the digits land
        // at its last four bytes - inside the mapped segment, not past it.
        assert_eq!(
            e.len() - data_file_off,
            128 + INITTAB_LEN as usize,
            "the blob is the pad plus the full scratch"
        );
    }
}
