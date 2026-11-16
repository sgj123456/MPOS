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
    // Every fixture this section makes is cleared first, and **by kind**: an
    // exchange swaps a name between a file and a directory, so a previous run
    // leaves `xa` a *directory* - and `write` on one is `EISDIR`, which made
    // the guard below false and skipped all sixteen claims silently. A test
    // whose fixtures depend on how the last run ended is a test that reports
    // nothing.
    for p in [xa, xb, xc, xmissing] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
    for p in [
        "/tmp/t-renameat2_xdir",
        "/tmp/t-renameat2_xdir2",
        "/tmp/t-renameat2_swp",
    ] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir_all(p);
    }
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

    // === RENAME_WHITEOUT: the name moves and a whiteout takes its place =====
    //
    // A whiteout is a **character device with device number 0**, which is how
    // overlayfs records "this name was deleted from the lower layer". Measured
    // on Linux 7.3.4 (`build/scratch/opathmmap/wout2.c`, `wflags.c`): the node
    // reads `mode 0000`, `S_ISCHR`, `rdev 0`; the move is an ordinary rename
    // otherwise (a taken target is replaced, a missing one created); a
    // **directory** may be whited out and its tree travels; and
    // **`WHITEOUT|NOREPLACE` is legal** (it answers `EEXIST` on a taken target)
    // while `EXCHANGE` with either of the others is `EINVAL`.
    //
    // `RENAME_WHITEOUT` is the one flag whose answer depends on the
    // *filesystem*: the man page says it "requires support from the underlying
    // filesystem", and here ramfs keeps `i_rdev` and can make the node while
    // ext4 has nowhere to put one. The sweep machine's root is ramfs and the
    // aarch64 root is ext4, so this program's claims are made where the answer
    // is the served one - a filesystem without support answers `EOPNOTSUPP`,
    // which is a truth about that medium rather than a divergence.
    const WHITEOUT: u64 = 0x4;
    let wo = "/tmp/t-renameat2_wo";
    let wo2 = "/tmp/t-renameat2_wo2";
    let wotaken = "/tmp/t-renameat2_wotaken";
    let womissing = "/tmp/t-renameat2_womissing";
    // Cleanup must cope with what a **previous run** left: a whiteout is a
    // character device, and `remove_file` on one is fine - but the *directories*
    // and the `wodir2` target survive a run that failed partway, so they are
    // cleared here rather than at the end where an early return cannot reach.
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_wodir2");
    for p in [wo, wo2, wotaken, womissing] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_wodir");
    let _ = std::fs::write(wo, b"PAYLOAD");
    if std::fs::read(wo).is_ok() {
        let answered = renameat2(wo, wo2, WHITEOUT);
        // Either the filesystem can make the node or it says so; both are
        // honest, and which one this machine gives is a property of its root.
        let supported = answered == 0;
        r.check(
            &format!(
                "RENAME_WHITEOUT is served or answers EOPNOTSUPP, never EINVAL (got {answered})"
            ),
            supported || refused(answered, libc::EOPNOTSUPP),
        );
        if supported {
            r.check(
                "the name moved and the node is intact",
                std::fs::read(wo2).ok().as_deref() == Some(&b"PAYLOAD"[..]),
            );
            // The whiteout itself: a char device with rdev 0 and mode 0000.
            let node = std::fs::symlink_metadata(wo);
            r.check(
                "a character device is left behind",
                node.as_ref()
                    .ok()
                    .map(|m| {
                        use std::os::unix::fs::FileTypeExt;
                        m.file_type().is_char_device()
                    })
                    .unwrap_or(false),
            );
            r.check(
                "with device number 0",
                node.as_ref().ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    m.rdev()
                }) == Some(0),
            );
            // A directory may be whited out, and its tree travels.
            let wodir = "/tmp/t-renameat2_wodir";
            if std::fs::create_dir_all(format!("{wodir}/in")).is_ok()
                && std::fs::write(format!("{wodir}/in/f"), b"Q").is_ok()
            {
                r.check(
                    "a directory may be whited out",
                    renameat2(wodir, "/tmp/t-renameat2_wodir2", WHITEOUT) == 0,
                );
                r.check(
                    "and its tree travelled",
                    std::fs::read("/tmp/t-renameat2_wodir2/in/f")
                        .ok()
                        .as_deref()
                        == Some(&b"Q"[..]),
                );
                r.check(
                    "and a whiteout took the directory's name",
                    std::fs::symlink_metadata(wodir)
                        .map(|m| {
                            use std::os::unix::fs::FileTypeExt;
                            m.file_type().is_char_device()
                        })
                        .unwrap_or(false),
                );
                let _ = std::fs::remove_file(wodir);
                let _ = std::fs::remove_dir_all("/tmp/t-renameat2_wodir2");
            }
            // The name `wo` now holds the whiteout node, so it has to go before
            // the name can be a file again - and the first draft of this pin did
            // not, which made **six** claims silently never run: `write` on a
            // char device answers `ENXIO`, the `if` was false, and the harness
            // printed nothing. A claim that cannot run must fail rather than skip.
            let _ = std::fs::remove_file(wo);
            // A taken target is replaced (with no NOREPLACE).
            if std::fs::write(wo, b"A").is_ok() && std::fs::write(wotaken, b"B").is_ok() {
                r.check(
                    "a taken target is replaced",
                    renameat2(wo, wotaken, WHITEOUT) == 0,
                );
                r.check(
                    "and the target holds the source's bytes",
                    std::fs::read(wotaken).ok().as_deref() == Some(&b"A"[..]),
                );
                let _ = std::fs::remove_file(wo);
            }
            // **`WHITEOUT|NOREPLACE` is legal**, and refuses a taken target.
            let _ = std::fs::remove_file(wo);
            if std::fs::write(wo, b"A").is_ok() && std::fs::write(wotaken, b"B").is_ok() {
                r.check(
                    "WHITEOUT|NOREPLACE on a taken target is EEXIST",
                    refused(renameat2(wo, wotaken, WHITEOUT | NOREPLACE), libc::EEXIST),
                );
                r.check(
                    "and nothing moved",
                    std::fs::read(wo).ok().as_deref() == Some(&b"A"[..]),
                );
            }
            // A missing target is created, as a plain rename creates one.
            if std::fs::write(wo, b"X").is_ok() {
                r.check(
                    "a missing target is created",
                    renameat2(wo, womissing, WHITEOUT) == 0,
                );
                r.check(
                    "and holds the source's bytes",
                    std::fs::read(womissing).ok().as_deref() == Some(&b"X"[..]),
                );
                let _ = std::fs::remove_file(wo);
            }
        }
        // The pair rule that holds on every filesystem: EXCHANGE with WHITEOUT.
        r.check(
            "WHITEOUT|EXCHANGE is EINVAL",
            refused(renameat2(wo2, wo2, WHITEOUT | EXCHANGE), libc::EINVAL),
        );
    }
    for p in [wo, wo2, wotaken, womissing] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir_all("/tmp/t-renameat2_wodir2");

    let _ = std::fs::remove_file(A);
    let _ = std::fs::remove_file(B);

    r.finish()
}
