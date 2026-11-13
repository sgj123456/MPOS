//! Executing a file larger than this kernel's largest allocation.
//!
//! `execve` used to read the whole file into one `Vec<u8>` before it knew what
//! the file was, and the frame allocator's largest block is 64 MiB, so a
//! 296 MB binary was refused with the *disk's* errno (`ENOSPC`). Measured on
//! the guest (`build/probe/oneshot`): `/usr/bin/chromium`, a 296242984-byte
//! launcher target, answered
//!
//! ```text
//! /usr/bin/chromium: exec: line 36: /usr/lib/chromium/chromium: No space left on device
//! ```
//!
//! on a filesystem with 549 MB free. Linux reads a `BINPRM_BUF_SIZE` window
//! (256 bytes) to decide whether the file is a script, and hands the image to
//! the loader, which maps what the headers ask for and never holds the file.
//!
//! The claims here are about size, not about chromium: a copy of a real
//! program padded past the ceiling, a script past it, and a file past it that
//! is no program at all. Everything is a movement or a refusal code, so the
//! test says the same thing on the host (`MPOS_ON_HOST=1`, Linux 7.2.14) and on
//! both machines.

use mpos_user_tests::Report;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

/// Comfortably past `2^14 × 4096` (the allocator's largest block, 64 MiB), so
/// the whole-file read this pins could not have succeeded.
const BIG: usize = 96 << 20;
const PAD: usize = BIG;
/// One mebibyte at a time: the test itself must not need a huge buffer.
const CHUNK: usize = 1 << 20;

/// Make `path` executable. A file `execve` is asked about must have the bit, or
/// the answer is `EACCES` and every claim here would be about permissions.
fn make_executable(path: &std::path::Path) {
    let mut permissions = std::fs::metadata(path).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

fn write_zeros(file: &mut std::fs::File, mut len: usize) {
    let zeros = vec![0u8; CHUNK];
    while len > 0 {
        let take = len.min(CHUNK);
        file.write_all(&zeros[..take]).expect("write");
        len -= take;
    }
}

/// A path only this test uses, so a leftover from a previous run cannot decide
/// a claim.
fn fixture(name: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(format!("/tmp/t-exec-big-{name}"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Run `path` with `args`, and answer either its exit code or the errno
/// `execve` answered when the program never ran.
///
/// The channel is a `CLOEXEC` pipe rather than the exit code: an executable may
/// exit with any number, and "the child exited 8" has to be told apart from
/// "execve answered ENOEXEC". A successful exec closes the pipe (so the parent
/// reads EOF), and a failed one writes the errno down it.
fn exec_child(path: &std::ffi::CStr, args: &[&std::ffi::CStr]) -> Result<i32, i32> {
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut fds = [0i32; 2];
    // SAFETY: plain libc calls on values that outlive them; the child only calls
    // async-signal-safe functions between `fork` and `execve`.
    unsafe {
        assert!(libc::pipe(fds.as_mut_ptr()) == 0, "pipe");
        let pid = libc::fork();
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            libc::close(fds[0]);
            // The exec'd program must not inherit the write end, or the parent
            // would wait for a program that may run for ever.
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::execve(
                path.as_ptr(),
                argv.as_ptr(),
                std::ptr::null::<*const libc::c_char>(),
            );
            let errno = *libc::__errno_location();
            libc::write(fds[1], &errno as *const i32 as *const libc::c_void, 4);
            libc::_exit(127);
        }
        libc::close(fds[1]);
        let mut answer: i32 = 0;
        let read = libc::read(fds[0], &mut answer as *mut i32 as *mut libc::c_void, 4);
        libc::close(fds[0]);
        let mut status = 0;
        assert!(libc::waitpid(pid, &mut status, 0) == pid, "waitpid");
        if read == 4 {
            return Err(answer);
        }
        if libc::WIFEXITED(status) {
            return Ok(libc::WEXITSTATUS(status));
        }
        Err(0)
    }
}

fn main() {
    let mut report = Report::new();
    // A child that exits with a code tells us the exec *ran*; only a child whose
    // exit code is a raw errno tells us it did not. Those overlap (an executable
    // may exit 8), so every errno claim also asserts the code is the errno and
    // not the program's own answer - which for the fixtures here means "exit
    // 5/7/9", never 8 or 28.
    let sh = std::ffi::CString::new("/bin/sh").unwrap();
    let exit_of = |code: &str| std::ffi::CString::new(code).unwrap();

    // The control: a real program of ordinary size, so a pass here is not a
    // statement about the fixtures alone.
    let control = exec_child(
        &sh,
        &[
            &sh,
            &std::ffi::CString::new("-c").unwrap(),
            &exit_of("exit 9"),
        ],
    );
    report.check(
        "a program of ordinary size still runs (exit 9)",
        control == Ok(9),
    );

    // A real ELF padded past the ceiling. The loader maps what the program
    // headers ask for and ignores the tail - Linux's own behaviour - so the only
    // thing that can refuse this is reading the file whole.
    let padded = fixture("elf");
    {
        let program = std::fs::read("/bin/sh").expect("read /bin/sh");
        let mut file = std::fs::File::create(&padded).expect("create");
        file.write_all(&program).expect("write program");
        write_zeros(&mut file, PAD);
    }
    make_executable(&padded);
    let padded_c = std::ffi::CString::new(padded.to_str().unwrap()).unwrap();
    // `argv[0]` is `sh`, not the fixture's name: busybox picks its applet from
    // `argv[0]`, so a padded copy of busybox called `t-exec-big-elf` answers
    // "applet not found" - correct for busybox, and not what this claim is
    // about.
    let sh_name = std::ffi::CString::new("sh").unwrap();
    let ran = exec_child(
        &padded_c,
        &[
            &sh_name,
            &std::ffi::CString::new("-c").unwrap(),
            &exit_of("exit 5"),
        ],
    );
    eprintln!("padded ELF: {ran:?}");
    report.check(
        "a program padded past the largest allocation still loads (exit 5)",
        ran == Ok(5),
    );

    // The same size, but not a program: the file's first bytes say nothing that
    // names an executable. Linux answers ENOEXEC, and so must a kernel that read
    // only the window a decision needs.
    let noise = fixture("not-a-program");
    {
        let mut file = std::fs::File::create(&noise).expect("create");
        write_zeros(&mut file, BIG);
    }
    make_executable(&noise);
    let noise_c = std::ffi::CString::new(noise.to_str().unwrap()).unwrap();
    let answer = exec_child(&noise_c, &[&noise_c]);
    eprintln!("not a program: {answer:?}");
    report.check(
        "a file past the ceiling that is no program is ENOEXEC",
        answer == Err(libc::ENOEXEC),
    );
    report.check(
        "and the disk is not blamed for a file it never filled",
        answer != Err(libc::ENOSPC),
    );

    // A script past the ceiling. `execve` reads only the shebang window and the
    // interpreter reads the script itself, so size is not a property of a
    // script the kernel has any business caring about.
    let script = fixture("script");
    {
        let mut file = std::fs::File::create(&script).expect("create");
        file.write_all(b"#!/bin/sh\nexit 7\n")
            .expect("write header");
        write_zeros(&mut file, PAD - 16);
    }
    make_executable(&script);
    let script_c = std::ffi::CString::new(script.to_str().unwrap()).unwrap();
    let scripted = exec_child(&script_c, &[&script_c]);
    eprintln!("big script: {scripted:?}");
    report.check(
        "a script past the ceiling still runs (exit 7)",
        scripted == Ok(7),
    );

    let _ = std::fs::remove_file(&padded);
    let _ = std::fs::remove_file(&noise);
    let _ = std::fs::remove_file(&script);
    report.finish();
}
