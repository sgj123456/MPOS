//! t-getcwd - one test for getcwd, which musl calls on every `pwd` and every
//! relative path that needs its absolute form.
//!
//! Pinned here: the answer is the buffer address itself and the buffer holds the
//! cwd NUL-terminated ("/" at process start), chdir updates what getcwd reports
//! ("/etc", then a relative ".." back to "/"), a buffer smaller than two bytes
//! answers EINVAL, and a buffer too small for the path plus its NUL answers
//! ERANGE.
//!
//! # Why this program calls `libc::getcwd` and not `rustix::process::getcwd`
//!
//! `rustix` binds this call, but not this contract. Its wrapper takes a
//! `Vec<u8>` and returns a `CString`, growing the vector on ERANGE until the path
//! fits - so the two things this test is about, *the size the caller chose* (and
//! the EINVAL and ERANGE answers to it) and *the address the syscall returns* (the
//! buffer itself, not a copy), are both outside it. That is `user/tests/
//! Cargo.toml`'s third clause: a wrapper that binds the right syscall but whose
//! shape cannot express the claim. `libc::getcwd` is the call with the shape the
//! ABI promises.
//!
//! # What this pins on each machine
//!
//! `getcwd` has a real number on both (`nr_aarch64.rs`/`nr_x86_64.rs`) and both
//! reach `handle_getcwd`, so the per-claim output is the same on both. The
//! "initial cwd is `/`" claim is about the *runner*, not the machine, and both are
//! the same shape: each initramfs script ends in busybox `switch_root`, which
//! chdirs to the new root before it execs the payload.

use mpos_user_tests::Report;
use rustix::process::chdir;

const ETC: &str = "/etc";

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// One `getcwd` into `buf`, answering the raw result the syscall gave: the buffer
/// address, or null. The length of `buf` is the size passed with it, which is the
/// contract this test is about.
fn getcwd(buf: &mut [u8]) -> *mut libc::c_char {
    // SAFETY: `buf` is a live, writable slice and its length is the size that goes
    // with it.
    unsafe { libc::getcwd(buf.as_mut_ptr().cast(), buf.len()) }
}

fn main() {
    println!("t-getcwd");
    let mut r = Report::new();

    let mut cwd = [0u8; 256];
    let answer = getcwd(&mut cwd);
    r.check(
        "getcwd returns the buffer address",
        answer == cwd.as_mut_ptr().cast(),
    );
    r.check("the initial cwd reads /", cwd[0] == b'/' && cwd[1] == 0);

    r.check("chdir(/etc) succeeds", chdir(ETC).is_ok());
    let answer = getcwd(&mut cwd);
    r.check(
        "getcwd returns the buffer again",
        answer == cwd.as_mut_ptr().cast(),
    );
    r.check(
        "the cwd now reads /etc",
        cwd[0] == b'/' && cwd[1] == b'e' && cwd[2] == b't' && cwd[3] == b'c' && cwd[4] == 0,
    );

    r.check("chdir(..) succeeds", chdir("..").is_ok());
    let answer = getcwd(&mut cwd);
    r.check(
        "getcwd returns the buffer a third time",
        answer == cwd.as_mut_ptr().cast(),
    );
    r.check("the cwd reads / again", cwd[0] == b'/' && cwd[1] == 0);

    // A buffer smaller than two bytes cannot even hold "/\0": EINVAL.
    let mut tiny = [0u8; 1];
    let answer = getcwd(&mut tiny);
    r.check(
        "a one-byte buffer answers EINVAL",
        answer.is_null() && last_errno() == libc::EINVAL,
    );

    // A buffer too small for the path plus its NUL answers ERANGE.
    r.check("chdir(/etc) succeeds again", chdir(ETC).is_ok());
    let mut two = [0u8; 2];
    let answer = getcwd(&mut two);
    r.check(
        "a two-byte buffer for /etc answers ERANGE",
        answer.is_null() && last_errno() == libc::ERANGE,
    );

    r.finish()
}
