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

    // === RENAME_EXCHANGE: the two names swap what they reach ===============
    //
    // Measured on Linux 7.3.4 (`build/scratch/opathmmap/{rex,rex3,rex5}.c`):
    // both names **must exist** (`ENOENT` onto a missing one, where a plain
    // rename would create it); the two may be **different kinds** (a file and
    // a directory swap, each subtree following its new parent's name); the two
    // names may be in **different directories**, and each exchanged directory
    // keeps the `..` it had because neither name moves; `EXCHANGE|NOREPLACE`
    // is `EINVAL`; and a name exchanged with itself is 0.
    const EXCHANGE: u64 = 0x2;
    const NOREPLACE: u64 = 0x1;
    let xa = "/tmp/t-renameat2_xa";
    let xb = "/tmp/t-renameat2_xb";
    let xc = "/tmp/t-renameat2_xc";
    let xmissing = "/tmp/t-renameat2_xmissing";
    for p in [xa, xb, xc, xmissing] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_xdir");
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_xdir2");
    let _ = std::fs::write(xa, b"AAA");
    let _ = std::fs::write(xb, b"BBB");
    if std::fs::read(xa).is_ok() && std::fs::read(xb).is_ok() {
        r.check(
            "RENAME_EXCHANGE of two files is served",
            renameat2(xa, xb, EXCHANGE) == 0,
        );
        r.check(
            "and the first name now holds what the second did",
            std::fs::read(xa).ok().as_deref() == Some(&b"BBB"[..]),
        );
        r.check(
            "and the second holds what the first did",
            std::fs::read(xb).ok().as_deref() == Some(&b"AAA"[..]),
        );
        // Both names must exist - the rule that separates this from a rename.
        r.check(
            "an exchange onto a missing name is ENOENT",
            refused(renameat2(xa, xmissing, EXCHANGE), libc::ENOENT),
        );
        r.check(
            "and nothing moved: the source is still there",
            std::fs::read(xa).is_ok() && std::fs::metadata(xmissing).is_err(),
        );
        // The flag combinations Linux refuses as a pair.
        r.check(
            "EXCHANGE|NOREPLACE is EINVAL",
            refused(renameat2(xa, xb, EXCHANGE | NOREPLACE), libc::EINVAL),
        );
        // A name exchanged with itself is the no-op.
        r.check(
            "a name exchanged with itself answers 0",
            renameat2(xa, xa, EXCHANGE) == 0,
        );
        r.check(
            "and still holds its bytes",
            std::fs::read(xa).ok().as_deref() == Some(&b"BBB"[..]),
        );
        // A **file and a directory** may swap, and the subtree follows the name
        // it now sits behind.
        let xdir = "/tmp/t-renameat2_xdir";
        if std::fs::create_dir_all(xdir).is_ok()
            && std::fs::write(format!("{xdir}/inner"), b"in").is_ok()
        {
            r.check(
                "a file and a directory exchange",
                renameat2(xa, xdir, EXCHANGE) == 0,
            );
            r.check(
                "and the file name is a directory now",
                std::fs::metadata(xa).map(|m| m.is_dir()).unwrap_or(false),
            );
            r.check(
                "and the directory name is a file now",
                std::fs::metadata(xdir)
                    .map(|m| m.is_file())
                    .unwrap_or(false),
            );
            r.check(
                "and the subtree travelled with its name",
                std::fs::read(format!("{xa}/inner")).ok().as_deref() == Some(&b"in"[..]),
            );
        }
        // **Across two directories**, which Linux serves - and the reason it
        // does is worth a claim: neither name moves, so no `..` changes.
        let xdir2 = "/tmp/t-renameat2_xdir2";
        if std::fs::create_dir_all(xdir2).is_ok()
            && std::fs::write(format!("{xdir2}/g"), b"G").is_ok()
            && std::fs::write(xc, b"C").is_ok()
        {
            r.check(
                "an exchange between two directories is served",
                renameat2(xc, &format!("{xdir2}/g"), EXCHANGE) == 0,
            );
            r.check(
                "and the outside name holds the inner one's bytes",
                std::fs::read(xc).ok().as_deref() == Some(&b"G"[..]),
            );
            r.check(
                "and the inner name holds the outside one's",
                std::fs::read(format!("{xdir2}/g")).ok().as_deref() == Some(&b"C"[..]),
            );
        }
        // A directory exchanged across directories keeps the `..` it had: the
        // claim that would catch a "swap inodes and reparent" implementation.
        let d1 = "/tmp/t-renameat2_swap_d1";
        let d2dir = "/tmp/t-renameat2_swap_d2";
        let _ = std::fs::remove_dir_all("/tmp/t-renameat2_swp");
        let _ =
            std::fs::create_dir_all(format!("/tmp/t-renameat2_swp/p/{d1_tail}", d1_tail = "one"));
        let _ =
            std::fs::create_dir_all(format!("/tmp/t-renameat2_swp/q/{d2_tail}", d2_tail = "two"));
        let _ = std::fs::write("/tmp/t-renameat2_swp/p/one/in1", b"1");
        let _ = std::fs::write("/tmp/t-renameat2_swp/q/two/in2", b"2");
        r.check(
            "two directories exchange across two parents",
            renameat2(
                "/tmp/t-renameat2_swp/p/one",
                "/tmp/t-renameat2_swp/q/two",
                EXCHANGE,
            ) == 0,
        );
        // `p/one` now holds what `q/two` did, and its `..` is still `p` - so
        // walking through it reaches `p`'s other children.
        r.check(
            "and each exchanged directory keeps the `..` it had",
            std::fs::metadata("/tmp/t-renameat2_swp/p/one/../one/in2").is_ok()
                && std::fs::metadata("/tmp/t-renameat2_swp/q/two/../two/in1").is_ok(),
        );
        let _ = d1;
        let _ = d2dir;
        let _ = std::fs::remove_dir_all("/tmp/t-renameat2_swp");
    } else {
        r.check("the exchange fixtures are made", false);
    }
    for p in [xa, xb, xc, xmissing] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_xdir");
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_xdir2");

    let _ = std::fs::remove_file(A);
    let _ = std::fs::remove_file(B);

    r.finish()
}
