//! t-renameat2 - one test for renameat2, the call musl's `rename` and
//! `mv` land in.
//!
//! Pinned here: a plain rename (flags 0) moves the content to the new name
//! and the old name answers ENOENT afterwards, RENAME_NOREPLACE refuses with
//! EEXIST when the target name is taken and leaves the source alone, a plain
//! rename over an existing target replaces it, a flags word with unknown bits
//! answers EINVAL, and a missing source answers ENOENT.
//!
//! rustix binds `renameat` but not `renameat2`'s flag word (clause (a)), so
//! every claim is the raw door with libc's number (`SYS_renameat2`: 316 /
//! 276). The fixtures are this program's own under `/tmp`.

use mpos_user_tests::Report;

const A: &str = "/tmp/t-renameat2_a";
const B: &str = "/tmp/t-renameat2_b";
const GONE: &str = "/tmp/t-renameat2_gone";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn renameat2(old: &str, new: &str, flags: u64) -> isize {
    let c_old = std::ffi::CString::new(old).unwrap();
    let c_new = std::ffi::CString::new(new).unwrap();
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            c_old.as_ptr() as usize,
            libc::AT_FDCWD,
            c_new.as_ptr() as usize,
            flags,
        ) as isize
    }
}

fn open_errno(path: &str) -> Option<i32> {
    let c = std::ffi::CString::new(path).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd >= 0 {
        unsafe { libc::close(fd) };
        Some(0)
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
}

fn main() {
    println!("t-renameat2");
    let mut r = Report::new();

    let _ = std::fs::remove_file(A);
    let _ = std::fs::remove_file(B);
    let _ = std::fs::remove_file(GONE);

    // Create a, write two bytes, close, then rename it onto b.
    let wrote = std::fs::write(A, b"R1").is_ok();
    r.check("creating a opens it", wrote);
    r.check("plain rename a to b succeeds", renameat2(A, B, 0) == 0);

    // The content lives under b now; a is gone.
    match std::fs::read(B) {
        Ok(bytes) => {
            r.check("b holds the two bytes written", bytes.len() == 2);
            r.check("the bytes are the ones a carried", &bytes == b"R1");
        }
        Err(_) => {
            r.check("b holds the two bytes written", false);
            r.check("the bytes are the ones a carried", false);
        }
    }
    r.check(
        "reopening the old name answers ENOENT",
        open_errno(A) == Some(libc::ENOENT),
    );

    // RENAME_NOREPLACE refuses to take a name that is already taken.
    let _ = std::fs::write(A, b"");
    r.check(
        "NOREPLACE over an existing target answers EEXIST",
        refused(renameat2(A, B, 1 /* RENAME_NOREPLACE */), libc::EEXIST),
    );
    r.check(
        "NOREPLACE left the source in place",
        open_errno(A) == Some(0),
    );

    // Without NOREPLACE the rename replaces the target.
    r.check(
        "a plain rename over an existing target succeeds",
        renameat2(A, B, 0) == 0,
    );

    // A flags word with unknown bits is refused outright.
    r.check(
        "unknown flags answer EINVAL",
        refused(renameat2(A, B, 0x8), libc::EINVAL),
    );

    // A source that is not there cannot be renamed.
    r.check(
        "renaming a missing source answers ENOENT",
        refused(renameat2(GONE, B, 0), libc::ENOENT),
    );

    let _ = std::fs::remove_file(A);
    let _ = std::fs::remove_file(B);

    r.finish()
}
