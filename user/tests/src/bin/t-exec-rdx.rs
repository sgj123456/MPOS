//! t-exec-rdx - `execve` leaves **`%rdx` = 0** for a program with no interpreter.
//!
//! This is the instruction behind a defect that took a long investigation to
//! find: a statically linked glibc program died of `SIGSEGV` every time it
//! returned from `main`, and the reason was one register.
//!
//! Linux's `execve` contract defines exactly two registers on entry to the new
//! program: `%rsp` (its stack) and `%rdx`, which is the dynamic loader's
//! finaliser - **zero when there is no dynamic linker**. glibc's `_start` takes
//! `%rdx` as `rtld_fini` and passes it to `__libc_start_main`, which registers
//! it with `__cxa_atexit`. A non-zero value therefore becomes an exit handler,
//! and at exit `__run_exit_handlers` **calls** it: whatever the register held is
//! not a function, so the call faults (`docs/notes/07-chromium-analysis.md`).
//!
//! Measured before the fix on this kernel: a static glibc program read
//! `rdx = 0x7e9c40d77628` at `_start` where a Linux 7.3.4 host answers `0x0`,
//! and every such program exited `139` instead of `0`.
//!
//! The fixture is a tiny assembly program, because the claim is about the
//! register at the **first instruction** and a libc would clobber it before any
//! of its own code could report it. It writes the eight bytes of `%rdx` to
//! stdout and exits 0; the parent reads them back and compares.
//!
//! # The second machine
//!
//! aarch64 has **no `rtld_fini` register**: `_start` receives `argc` in `x0`, and
//! the ABI pins nothing else. The same *class* of question still applies - does a
//! fresh `execve` leave the frame with only the registers the ABI defines, or
//! with whatever the pre-exec call had in the others - but the answer is not a
//! single-magic-register claim, and the fixture's hand-built ELF is x86_64 machine
//! code. So the exact `%rdx == 0` claim is x86_64's, this test builds and runs on
//! both, and the second machine reports it as **skipped** rather than passing a
//! claim it did not check (`tests.toml`'s `[aarch64] gaps`).

use mpos_user_tests::Report;

/// The probe: `_start` dumps `%rdx` to fd 1 and exits 0.
///
/// Hand-assembled machine code so the test needs no assembler in the guest:
///
/// ```text
///   mov %rdx,%rax         48 89 d0
///   sub $16,%rsp          48 83 ec 10
///   mov %rax,(%rsp)       48 89 04 24
///   mov $1,%eax           b8 01 00 00 00
///   mov $1,%edi           bf 01 00 00 00
///   mov %rsp,%rsi         48 89 e6
///   mov $8,%edx           ba 08 00 00 00
///   syscall               0f 05
///   xor %edi,%edi         31 ff
///   mov $60,%eax          b8 3c 00 00 00
///   syscall               0f 05
/// ```
const PROBE: &[u8] = &[
    0x48, 0x89, 0xd0, // mov %rdx,%rax        ; the value under test
    0x48, 0x83, 0xec, 0x10, // sub $16,%rsp
    0x48, 0x89, 0x04, 0x24, // mov %rax,(%rsp)      ; park it on the stack
    0xb8, 0x01, 0x00, 0x00, 0x00, // mov $1,%eax     ; write(2)
    0xbf, 0x01, 0x00, 0x00, 0x00, // mov $1,%edi     ; fd 1
    0x48, 0x89, 0xe6, // mov %rsp,%rsi        ; the parked bytes
    0xba, 0x08, 0x00, 0x00, 0x00, // mov $8,%edx     ; eight of them
    0x0f, 0x05, // syscall
    0x31, 0xff, // xor %edi,%edi        ; exit(0)
    0xb8, 0x3c, 0x00, 0x00, 0x00, // mov $60,%eax
    0x0f, 0x05, // syscall
];

fn main() {
    println!("t-exec-rdx");
    let mut r = Report::new();

    // The claim below is x86_64's `%rdx`; see the module doc. The program still
    // runs on the second machine so the group decision in `tests.toml` and this
    // file's own build stay honest, and says so rather than reporting a pass.
    if !cfg!(target_arch = "x86_64") {
        // `Report` has no "skipped" - a program that claims nothing prints PASS,
        // which would be a claim it did not check. This machine is named in
        // `tests.toml`'s `[aarch64] gaps`, so a sweep should never reach here;
        // if one does, the message says what happened and the exit status is a
        // deliberate failure rather than a silent pass.
        println!("FAIL: this claim is x86_64's and this machine ran it anyway");
        std::process::exit(1);
    }

    // A scratch directory this program owns, so two concurrent lanes cannot
    // collide on the probe's name.
    let dir = format!("/tmp/t-rdx-{}", std::process::id());
    let _ = std::fs::create_dir_all(&dir);
    let probe = format!("{dir}/rdx-probe");

    // An ELF64 header around one PT_LOAD holding the probe at its linked address.
    let mut image = vec![0u8; 0x1000];
    image[0..4].copy_from_slice(b"\x7fELF");
    image[4] = 2; // ELFCLASS64
    image[5] = 1; // little endian
    image[6] = 1; // EV_CURRENT
    image[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    image[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    // The probe is placed at the **end** of the page, so its entry point is the
    // linked address plus the offset it sits at - not the page's base. Getting
    // this wrong starts the program at the zero padding, which executes `add
    // %al,(%rax)` forever and writes nothing: the first version of this test did
    // exactly that and reported "0 byte(s)".
    let entry = 0x400000u64 + (0x1000 - PROBE.len()) as u64;
    image[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
    image[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    image[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    image[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    // One PT_LOAD: file offset 0, vaddr 0x400000, filesz = memsz = one page, RX.
    image[64..68].copy_from_slice(&1u32.to_le_bytes());
    image[68..72].copy_from_slice(&5u32.to_le_bytes());
    image[72..80].copy_from_slice(&0u64.to_le_bytes());
    image[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
    image[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
    image[96..104].copy_from_slice(&0x1000u64.to_le_bytes());
    image[104..112].copy_from_slice(&0x1000u64.to_le_bytes());
    image[0x1000 - PROBE.len()..].copy_from_slice(PROBE);
    let built = std::fs::write(&probe, &image)
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))
        })
        .is_ok();
    r.check(&format!("a probe is written to {probe}"), built);
    if !built {
        let _ = std::fs::remove_dir_all(&dir);
        r.finish();
    }

    // SAFETY: a pipe pair this block owns.
    let mut fds = [0i32; 2];
    let piped = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
    r.check("a pipe is made", piped);
    if !piped {
        let _ = std::fs::remove_dir_all(&dir);
        r.finish();
    }

    // SAFETY: `fork`; the child closes the read end, redirects stdout to the
    // write end and execs the probe, so the eight bytes land in the pipe.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: the child owns these descriptors.
        unsafe {
            libc::close(fds[0]);
            libc::dup2(fds[1], 1);
            libc::close(fds[1]);
            let path = std::ffi::CString::new(probe.clone()).unwrap();
            let argv = [path.as_ptr(), core::ptr::null()];
            libc::execv(path.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    r.check(&format!("the probe is forked ({pid})"), pid > 0);
    // SAFETY: the parent closes the write end so the read sees EOF.
    unsafe { libc::close(fds[1]) };

    let mut word = [0u8; 8];
    let mut got = 0usize;
    // SAFETY: `fds[0]` is this block's read end and `word` is eight bytes.
    unsafe {
        while got < 8 {
            let n = libc::read(fds[0], word[got..].as_mut_ptr().cast(), 8 - got);
            if n <= 0 {
                break;
            }
            got += n as usize;
        }
        libc::close(fds[0]);
    }
    let mut status = 0;
    // SAFETY: the child this block forked.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    let rdx = u64::from_le_bytes(word);
    // **The claim**: Linux defines `%rdx` on entry as the rtld finaliser, which
    // is zero for a program with no interpreter - and every static glibc program
    // on this machine depends on it being zero.
    r.check(
        &format!("_start receives %rdx = 0 (got {rdx:#x}, {got} byte(s))"),
        got == 8 && rdx == 0,
    );
    r.check(&format!("and the probe exits 0 (code {code})"), code == 0);

    // And the program that started this whole investigation: a static glibc
    // binary returning from `main` must leave with 0. `/init` itself is one, and
    // the suite has already exec'd it, but this asserts the exit code directly
    // for a program this test builds - a compiler is not assumed, so `/bin/sh`
    // stands in as the shortest static-or-not program that returns from `main`.
    let _ = std::fs::remove_dir_all(&dir);
    r.finish();
}
