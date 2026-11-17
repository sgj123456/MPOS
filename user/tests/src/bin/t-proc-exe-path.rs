//! t-proc-exe-path - `/proc/<pid>/exe` reports an **absolute canonical** path.
//!
//! Found while chasing Chromium's `Failed to load /proc/self/chrome_100_percent.pak`:
//! a program that locates its resources with `dirname(readlink("/proc/self/exe"))`
//! looks in the wrong directory when the link is a *relative* name.
//!
//! Measured on Linux 7.3.4 (`build/scratch/rex.c`, invoked as `./rexprobe` from
//! `/tmp`): `readlink(/proc/self/exe)` answers `/tmp/rexprobe` - **absolute** -
//! and the same path when invoked absolutely. Measured in this kernel before the
//! fix (`build/probe/rel/serial.log`): exec'd as `./probe-exepath` from `/bin`,
//! `/proc/self/exe` answered `./probe-exepath`.
//!
//! **`argv[0]` is not the same question** and stays as the caller wrote it.
//!
//! This program re-execs itself by a **relative** name in a child, then reads
//! that child's link. The child is the subject; the parent arranges it and reads
//! the answer back through a pipe.

use mpos_user_tests::Report;

fn main() {
    println!("t-proc-exe-path");
    let mut r = Report::new();

    // Already the exec'd child: report the link and leave.
    if let Ok(fd) = std::env::var("T_PROC_EXE_PATH_CHILD") {
        let link = std::fs::read_link("/proc/self/exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let msg = alloc_msg(&link);
        // SAFETY: the descriptor number the parent wrote into the environment.
        unsafe {
            if let Ok(fd) = fd.parse::<i32>() {
                libc::write(fd, msg.as_ptr().cast::<libc::c_void>(), msg.len());
            }
            libc::_exit(0);
        }
    }

    let self_exe = std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    r.check(
        &format!("/proc/self/exe is absolute ({self_exe})"),
        self_exe.starts_with('/'),
    );
    if !self_exe.starts_with('/') {
        r.finish();
    }
    let (dir, base) = match self_exe.rsplit_once('/') {
        Some((d, b)) if !b.is_empty() => (
            if d.is_empty() {
                "/".to_string()
            } else {
                d.to_string()
            },
            b.to_string(),
        ),
        _ => {
            r.check("the executable path names a file", false);
            r.finish()
        }
    };
    r.check(
        &format!("and splits into a directory and a name ({dir} + {base})"),
        true,
    );

    let mut fds = [0i32; 2];
    // SAFETY: a pipe this test owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        r.check("a pipe is made", false);
        r.finish();
    }
    // SAFETY: fork in a test is the fixture.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: the child narrows to its own descriptors and execs itself by a
        // RELATIVE name - the shape under test.
        unsafe {
            libc::close(fds[0]);
            let d = to_cstring(&dir);
            libc::chdir(d.as_ptr());
            std::env::set_var("T_PROC_EXE_PATH_CHILD", fds[1].to_string());
            let rel = format!("./{base}");
            let name = to_cstring(&rel);
            let arg0 = to_cstring(&rel);
            let argv = [arg0.as_ptr(), core::ptr::null()];
            libc::execv(name.as_ptr(), argv.as_ptr());
            libc::_exit(1);
        }
    }
    // SAFETY: the parent's write end.
    unsafe { libc::close(fds[1]) };

    let mut buf = [0u8; 512];
    // SAFETY: the read end this test owns; the buffer is its own stack.
    let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    // SAFETY: the descriptor and the child this test made.
    unsafe {
        libc::close(fds[0]);
        let mut st = 0;
        libc::waitpid(child, &mut st, 0);
    }
    let seen = if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize])
            .trim()
            .to_string()
    } else {
        String::new()
    };

    // **The claim**: a relatively-exec'd program still reports an absolute link,
    // and it is the file it actually ran.
    r.check(
        &format!("a relatively-exec'd child reports an absolute exe ({seen})"),
        seen.starts_with('/'),
    );
    r.check(
        &format!("and it names the same file this test runs ({seen})"),
        !seen.is_empty() && seen == self_exe,
    );

    // ---- the same question through the magic link ----
    //
    // A crash handler respawns itself with `execve("/proc/self/exe")`, and a
    // zygote does the same; that is not an exotic door. The path such a program
    // reports afterwards must be the **real** one, because everything
    // downstream reads it: `comm`, and a resource loader's
    // `dirname(readlink("/proc/self/exe"))`.
    //
    // Measured on Linux 7.3.4 (`build/scratch/selfexec/t.c`): the re-exec'd
    // child reads its own absolute path back and its `comm` is the file's name.
    // Measured here before the fix (`probe-selfexec`): the child read
    // `/proc/self/exe` back **as its own path** - a self-reference that never
    // resolves - and its `comm` became `exe`, which is the shape Chromium showed
    // as `exe[780]` in its trap lines and as
    // `Failed to load /proc/self/chrome_100_percent.pak` in its stderr.
    r.check(
        &format!("this program's own path is known ({self_exe})"),
        self_exe.starts_with('/') && !self_exe.ends_with("/proc/self/exe"),
    );
    let seen_self = reexec_via_magic_link(&self_exe);
    r.check(
        &format!("a child exec'd through /proc/self/exe reports a real path ({seen_self})"),
        seen_self.starts_with('/') && !seen_self.is_empty(),
    );
    // The claim that separates "some path" from "the right one": the literal
    // spelling must not come back, and the answer must be this program.
    r.check(
        &format!(
            "and not the magic link it was started with ({} vs {self_exe})",
            seen_self
        ),
        seen_self != "/proc/self/exe" && seen_self == self_exe,
    );

    r.finish()
}

/// Run a child whose `execve` names `/proc/self/exe`, and report the path that
/// child reads back.
///
/// The child branches on an environment marker rather than on `argv`, because
/// the door under test replaces the image and only the environment survives it
/// in a way that does not change what is being tested.
fn reexec_via_magic_link(_expect: &str) -> String {
    const MARK: &str = "T_PROC_EXE_PATH_SELFEXEC_FD";
    if let Ok(fd) = std::env::var(MARK) {
        let link = std::fs::read_link("/proc/self/exe")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let msg = alloc_msg(&link);
        // SAFETY: the descriptor the parent wrote into the environment.
        unsafe {
            if let Ok(fd) = fd.parse::<i32>() {
                libc::write(fd, msg.as_ptr().cast::<libc::c_void>(), msg.len());
            }
            libc::_exit(0);
        }
    }
    let mut fds = [0i32; 2];
    // SAFETY: a pipe this test owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return String::new();
    }
    // SAFETY: fork is the fixture.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: the child narrows to its own write end and execs the link.
        unsafe {
            libc::close(fds[0]);
            std::env::set_var(MARK, fds[1].to_string());
            let path = to_cstring("/proc/self/exe");
            let arg0 = to_cstring("reexeced");
            let argv = [arg0.as_ptr(), core::ptr::null()];
            libc::execv(path.as_ptr(), argv.as_ptr());
            libc::_exit(1);
        }
    }
    // SAFETY: the parent's write end.
    unsafe { libc::close(fds[1]) };
    let mut buf = [0u8; 512];
    // SAFETY: the read end this test owns.
    let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    // SAFETY: the descriptor and the child.
    unsafe {
        libc::close(fds[0]);
        let mut st = 0;
        libc::waitpid(child, &mut st, 0);
    }
    if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize])
            .trim()
            .to_string()
    } else {
        String::new()
    }
}

/// A NUL-terminated copy for a libc call.
fn to_cstring(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

/// The message the child writes: the link plus a newline.
fn alloc_msg(link: &str) -> Vec<u8> {
    let mut v = link.as_bytes().to_vec();
    v.push(b'\n');
    v
}
