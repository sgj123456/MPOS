//! t-rmdir - the name-shaped half of the filesystem.
//!
//! Every call here is about a *name* rather than a file: making one, taking one
//! away, and asking about one that has not been made. Three shapes in it are
//! the ones that were wrong, and each was found by a real program rather than
//! by a test:
//!
//! * **a bare name**. `mkdir a` has no slash in it, which is not the same as
//!   being unnameable - it means "beside me", which is what `.` spells. The
//!   kernel answered `EINVAL` for every such call, so a shell could not create
//!   anything without spelling the whole path;
//! * **`.` asked about directly**. `ls -l` starts by looking up its own
//!   argument, and a path splitter that treats `.` as an unnameable last
//!   component makes the first thing every listing tool does an error;
//! * **`..` from a cwd**. A directory does not know its parent - it is a name
//!   edge in something else - so `..` from a relative path used to leave the
//!   process where it was, silently. `cd ..` did nothing.
//!
//! The refusals are pinned too, because they are the whole difference between
//! `rmdir` and a recursive delete wearing the wrong name.
//!
//! Written with `rustix`, so every errno compared below is the one Linux
//! defines rather than one this test remembered.

use rustix::fs::{AtFlags, Mode, mkdir, mkdirat, rmdir, unlinkat};
use rustix::io::Errno;
use std::env;
use std::path::Path;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: Errno, want: Errno) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }
}

/// The scratch tree this test owns, emptied before use.
const DIR: &str = "/tmp/t-rmdir";

fn main() {
    println!("t-rmdir");
    let mut r = Report { failed: false };

    let _ = std::fs::remove_dir_all(DIR);
    if let Err(err) = std::fs::create_dir(DIR) {
        r.check(
            &format!("the scratch directory can be made (got {err})"),
            false,
        );
        std::process::exit(1);
    }

    // Everything below happens *inside* a directory, by its bare name, so the
    // whole reason this test exists is in play: no absolute path anywhere.
    if let Err(err) = env::set_current_dir(DIR) {
        r.check(
            &format!("the scratch directory can be entered (got {err})"),
            false,
        );
        std::process::exit(1);
    }

    // A bare name is a name beside the caller.
    r.check(
        "a bare name can be created",
        mkdir("bare", Mode::RWXU).is_ok(),
    );
    r.check("and it is a directory", Path::new("bare").is_dir());
    r.check("and its bare name removes it again", rmdir("bare").is_ok());
    r.check("and it is gone", !Path::new("bare").exists());

    // A name that is already taken answers EEXIST - *including* the names that
    // are always taken. This is not a corner: `mkdir -p` creates a path by
    // walking up it and stopping at the first ancestor that is already there,
    // so an `EINVAL` where an `EEXIST` belonged sends it to `mkdir("/")` and
    // fails the whole command.
    for taken in [".", "..", "/"] {
        match mkdir(taken, Mode::RWXU) {
            Ok(()) => r.check(
                &format!("mkdir of the existing name {taken:?} fails"),
                false,
            ),
            Err(err) => r.check_errno(
                &format!("mkdir of the existing name {taken:?} is EEXIST"),
                err,
                Errno::EXIST,
            ),
        }
    }
    match mkdir(DIR, Mode::RWXU) {
        Ok(()) => r.check("mkdir of a directory that exists fails", false),
        Err(err) => r.check_errno(
            "mkdir of a directory that exists is EEXIST",
            err,
            Errno::EXIST,
        ),
    }
    // And the nested shape `-p` is built on.
    r.check(
        "a path can be created one component at a time",
        mkdir("sub", Mode::RWXU).is_ok(),
    );
    r.check(
        "and the component inside it",
        mkdir("sub/deeper", Mode::RWXU).is_ok(),
    );
    r.check("and it is there", Path::new("sub/deeper").is_dir());
    r.check("the inner directory goes", rmdir("sub/deeper").is_ok());
    r.check("and so does the outer one", rmdir("sub").is_ok());

    // Asking about the current directory and its parent directly - the first
    // thing a listing tool does, and the case a parent/name split cannot say.
    r.check(
        "the current directory can be stat'ed as `.`",
        rustix::fs::lstat(".").is_ok(),
    );
    r.check("and its parent as `..`", rustix::fs::lstat("..").is_ok());

    // `..` has to go up. This is the one an inode cannot do for itself.
    r.check(
        "a child directory can be made",
        std::fs::create_dir("inner").is_ok(),
    );
    r.check("and entered", env::set_current_dir("inner").is_ok());
    let inside = env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n == "inner"))
        .unwrap_or(false);
    r.check("the shell is standing in it", inside);
    r.check("and `..` leaves it", env::set_current_dir("..").is_ok());
    let back = env::current_dir()
        .ok()
        .map(|p| p.file_name().map(|n| n == "t-rmdir").unwrap_or(false))
        .unwrap_or(false);
    r.check("the shell is back where it started", back);

    // The refusals: a directory that still holds something, and a name that is
    // not a directory at all.
    r.check(
        "a directory to fill can be made",
        mkdir("full", Mode::RWXU).is_ok(),
    );
    r.check(
        "with something inside it",
        std::fs::write("full/inside.txt", b"x").is_ok(),
    );
    match rmdir("full") {
        Ok(()) => r.check("rmdir of a directory with something in it fails", false),
        Err(err) => r.check_errno(
            "rmdir of a directory with something in it is ENOTEMPTY",
            err,
            Errno::NOTEMPTY,
        ),
    }
    r.check(
        "and what was inside is still there",
        Path::new("full/inside.txt").exists(),
    );

    r.check(
        "a plain file can be made",
        std::fs::write("plain.txt", b"x").is_ok(),
    );
    match rmdir("plain.txt") {
        Ok(()) => r.check("rmdir of a file fails", false),
        Err(err) => r.check_errno("rmdir of a file is ENOTDIR", err, Errno::NOTDIR),
    }
    r.check("and the file is untouched", Path::new("plain.txt").exists());

    match rmdir("never-existed") {
        Ok(()) => r.check("rmdir of a name that is not there fails", false),
        Err(err) => r.check_errno(
            "rmdir of a name that is not there is ENOENT",
            err,
            Errno::NOENT,
        ),
    }

    // The same operation reached by descriptor, which is how a tool that
    // already holds a directory does it. `unlinkat` says which of the two it
    // means with a flag, and getting it wrong either removes a directory that
    // still holds something or refuses a plain file.
    match rustix::fs::open(
        DIR,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(dirfd) => {
            r.check(
                "a directory can be unlinked by descriptor with the REMOVEDIR flag",
                unlinkat(&dirfd, "full/inside.txt", AtFlags::empty()).is_ok(),
            );
            r.check(
                "and with the flag the empty directory goes too",
                unlinkat(&dirfd, "full", AtFlags::REMOVEDIR).is_ok(),
            );
            r.check("and it is gone", !Path::new("full").exists());
            r.check(
                "without the flag a directory is refused",
                unlinkat(&dirfd, "inner", AtFlags::empty()) == Err(Errno::ISDIR),
            );
            r.check(
                "and with the flag a file is refused",
                unlinkat(&dirfd, "plain.txt", AtFlags::REMOVEDIR) == Err(Errno::NOTDIR),
            );
            // A descriptor-based mkdir, the same shape as the bare name above.
            r.check(
                "mkdirat with a descriptor makes a directory beside it",
                mkdirat(&dirfd, "made", Mode::RWXU).is_ok(),
            );
            r.check("and it is there", Path::new("made").is_dir());
            let _ = unlinkat(&dirfd, "made", AtFlags::REMOVEDIR);
        }
        Err(err) => r.check(&format!("the scratch directory opens (got {err})"), false),
    }

    // Leave the tree as it was found, and stand somewhere the shell can find
    // the next test from.
    let _ = std::fs::remove_file("plain.txt");
    let _ = std::fs::remove_dir("inner");
    if let Err(err) = env::set_current_dir("/") {
        r.check(&format!("the root can be entered again (got {err})"), false);
    }

    if r.failed {
        println!("t-rmdir: FAILED");
        std::process::exit(1);
    }
    println!("t-rmdir: all passed");
}
