//! t-execveat - `execveat(2)`: `execve` with a directory descriptor, and the one
//! door that can run **a descriptor's own file**.
//!
//! It was not implemented at all: the number was never declared, so the dispatch
//! answered `ENOSYS`. That is the honest answer to an unknown number and the
//! wrong one for a call musl's `fexecve` is built from - `posix_spawn`'s
//! file-action path reaches it, and a program that execs an already-verified
//! descriptor has no fallback.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/exat.c`) before any claim:
//!
//! | what | Linux |
//! |---|---|
//! | a relative name plus a dirfd | served |
//! | `("", AT_EMPTY_PATH)` on a file descriptor | served, runs that file |
//! | `(-1, "", AT_EMPTY_PATH)` | **`EBADF`** — the descriptor before the name |
//! | `("", 0)` — empty name, no flag | `ENOENT` |
//! | a name that is not there | `ENOENT` |
//! | an unknown flag bit | `EINVAL` |
//! | `AT_SYMLINK_NOFOLLOW` | accepted |
//!
//! Every claim is a **forked child**, because a successful exec does not return:
//! the parent watches the child's exit and asks whether the new image ran. The
//! child writes a marker byte through a pipe before it forks, so "the exec
//! worked" is a read the parent can make rather than an exit code it has to
//! interpret - the shape `t-exec-concurrent` uses for the same reason.
//!
//! Numbers from libc's table, because `rustix` binds `execveat` only as
//! `execveat(dirfd, path, args, env, flags)` over a `CStr` - and the empty-name
//! shape *is* the claim, so the raw door is the one that can express it.

use mpos_user_tests::Report;

const DIR: &str = "/tmp/t-execveat";
/// What the re-executed image prints. A shell is used because it is the one
/// program every image here has (`/bin/sh`), and the *marker* is what the
/// parent checks, not the program.
const MARKER: &str = "T-EXECVEAT-RAN";

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

/// Run `execveat(dirfd, path, argv, envp, flags)` in a child; answer what the
/// child did. `Ok(())` means the exec happened (the child is a different image
/// and printed the marker), `Err(errno)` means the call refused.
///
/// The child reports the refusal through a pipe *before* `_exit`, so the parent
/// sees an errno rather than having to decode an exit status.
fn execveat_in_child(dirfd: i32, path: &str, flags: i32) -> Result<(), i32> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(-1);
    }
    // SAFETY: fork in a test process; the child only touches its own pipe end.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let p = cstr(path);
        // `sh -c 'echo MARKER'` - the marker is the evidence that a *different*
        // image ran, which an exit code alone cannot give: a child that never
        // execs can still exit 0.
        let argv = [
            cstr("sh").into_raw(),
            cstr("-c").into_raw(),
            cstr(&format!("echo {MARKER}")).into_raw(),
            std::ptr::null_mut(),
        ];
        let envp: [*mut libc::c_char; 1] = [std::ptr::null_mut()];
        let rc = unsafe {
            libc::syscall(
                libc::SYS_execveat,
                dirfd,
                p.as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
                flags,
            )
        };
        // Only reached when the exec *failed*, and the errno is what the parent
        // needs. Writing it is the last thing this image does.
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let word = errno.to_ne_bytes();
        unsafe {
            libc::syscall(libc::SYS_write, fds[1], word.as_ptr(), word.len());
            libc::_exit(if rc == -1 { 99 } else { 0 });
        }
    }
    unsafe { libc::close(fds[1]) };
    // A successful exec inherits the write end and the shell closes it, so a
    // zero-length read means "nothing was written": the exec worked.
    let mut buf = [0u8; 4];
    let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast::<libc::c_void>(), 4) };
    unsafe { libc::close(fds[0]) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if n == 4 {
        return Err(i32::from_ne_bytes(buf));
    }
    // No errno written: the child became another image, so the question is
    // whether that image marked itself. It reports through the exit status: the
    // shell's `echo` succeeds, so 0.
    let code = (status >> 8) & 0xff;
    if code == 0 { Ok(()) } else { Err(code) }
}

fn main() {
    println!("t-execveat");
    let mut r = Report::new();

    let _ = std::fs::remove_dir_all(DIR);
    let made = std::fs::create_dir_all(DIR);
    r.check(
        &format!("the fixture directory is made ({made:?})"),
        made.is_ok(),
    );
    if made.is_err() {
        r.finish();
    }
    // A copy of the shell, so the fixture is a *file the descriptor holds*
    // rather than a link to one.
    let shell = format!("{DIR}/sh");
    let copied = std::fs::copy("/bin/sh", &shell);
    r.check(
        &format!("a copy of the shell is made ({copied:?})"),
        copied.is_ok(),
    );
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(DIR);
        r.finish();
    }

    let dfd = unsafe {
        libc::open(
            cstr(DIR).as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    r.check(
        &format!("a directory descriptor is opened ({dfd})"),
        dfd >= 0,
    );

    // === the two shapes that are served ====================================
    let plain = execveat_in_child(dfd, "sh", 0);
    r.check(
        &format!("execveat(dirfd, name) runs it ({plain:?})"),
        plain.is_ok(),
    );
    let ffd = unsafe { libc::open(cstr(&shell).as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    r.check(
        &format!("a file descriptor for the image is opened ({ffd})"),
        ffd >= 0,
    );
    // The claim this whole program exists for: an **empty name with
    // AT_EMPTY_PATH** runs the descriptor's own file.
    let by_fd = execveat_in_child(ffd, "", libc::AT_EMPTY_PATH);
    r.check(
        &format!("execveat(fd, empty, AT_EMPTY_PATH) runs the descriptor's file ({by_fd:?})"),
        by_fd.is_ok(),
    );

    // === the refusals ======================================================
    let ebadf = |res: Result<(), i32>| res == Err(libc::EBADF);
    let badfd = execveat_in_child(-1, "", libc::AT_EMPTY_PATH);
    r.check(
        &format!("execveat(-1, empty, AT_EMPTY_PATH) is EBADF ({badfd:?})"),
        ebadf(badfd),
    );
    // The descriptor is judged **before** the empty name: this is the ordering
    // claim, and a kernel that answered ENOENT here would pass the one above
    // by accident only if it also refused a live descriptor.
    let unflagged = execveat_in_child(dfd, "", 0);
    r.check(
        &format!("an empty name without AT_EMPTY_PATH is ENOENT ({unflagged:?})"),
        unflagged == Err(libc::ENOENT),
    );
    let missing = execveat_in_child(dfd, "nope", 0);
    r.check(
        &format!("a name that is not there is ENOENT ({missing:?})"),
        missing == Err(libc::ENOENT),
    );
    let badflag = execveat_in_child(dfd, "sh", 0x8);
    r.check(
        &format!("an unknown flag bit is EINVAL ({badflag:?})"),
        badflag == Err(libc::EINVAL),
    );
    // `AT_SYMLINK_NOFOLLOW` is accepted, so the flag word is a vocabulary and
    // not a single member.
    let nofollow = execveat_in_child(dfd, "sh", libc::AT_SYMLINK_NOFOLLOW);
    r.check(
        &format!("AT_SYMLINK_NOFOLLOW is accepted ({nofollow:?})"),
        nofollow.is_ok(),
    );

    if dfd >= 0 {
        unsafe { libc::close(dfd) };
    }
    if ffd >= 0 {
        unsafe { libc::close(ffd) };
    }
    let _ = std::fs::remove_dir_all(DIR);
    r.finish()
}
