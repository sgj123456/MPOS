//! t-rename - the name a rename actually lands on.
//!
//! `rename` is one call in the C library and two in the kernel: `renameat`,
//! which takes a directory descriptor *per name*, and `renameat2`, which is
//! the same call with flags. Both are asked for the same thing, and the thing
//! that is easy to get wrong is the second one - a bare name means "beside
//! this descriptor", not "in the root".
//!
//! That mistake is not cosmetic. `apk` fetches an index to one name and
//! renames it to another, both relative to the same descriptor:
//!
//! ```text
//! renameat(dfd, "APKINDEX.part", dfd, "APKINDEX")
//! ```
//!
//! Resolve the second name against `/` and the index is stored in the root
//! directory instead of the cache. `update` then reports success while the
//! next run reports `opening from cache ...: No such file or directory` -
//! downloading work that is thrown away, every time. So the case worth
//! pinning is exactly this one: a real descriptor, a bare name, and the
//! assertion that neither name touched the root.
//!
//! Written with `rustix`, so both the call and the errors are Linux's.

use rustix::fs::{AtFlags, Mode, OFlags, open, renameat, renameat_with, unlinkat};
use std::fs;
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
}

/// A directory the test owns outright, emptied before use.
fn scratch() -> &'static Path {
    let dir = Path::new("/tmp/t-rename_dir");
    let _ = fs::remove_dir_all(dir);
    fs::create_dir(dir).expect("create the scratch directory");
    dir
}

fn main() {
    println!("t-rename");
    let mut r = Report { failed: false };
    let dir = scratch();
    let inside = |name: &str| dir.join(name);

    // The regression this test exists for: both names given relative to a
    // directory descriptor.
    fs::write(inside("a.part"), b"index bytes").expect("write the source");
    match open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()) {
        Ok(dirfd) => {
            let moved = renameat(&dirfd, "a.part", &dirfd, "a.done");
            r.check("a bare name on both sides is renamed", moved.is_ok());

            match fs::read(inside("a.done")) {
                Ok(bytes) => r.check(
                    &format!(
                        "and the new name is inside the directory, not at the root ({:?})",
                        String::from_utf8_lossy(&bytes)
                    ),
                    bytes == b"index bytes",
                ),
                Err(err) => r.check(
                    &format!("and the new name is inside the directory (got {err})"),
                    false,
                ),
            }
            // The half that was actually broken. A root-level `a.done` means
            // the second name was resolved against `/` instead of the
            // descriptor, which is the shape that broke apk's cache.
            r.check(
                "nothing appeared in the root directory",
                !Path::new("/a.done").exists() && !Path::new("/a.part").exists(),
            );
            r.check("and the old name is gone", !inside("a.part").exists());
        }
        Err(err) => r.check(&format!("the directory can be opened (got {err})"), false),
    }

    // `renameat2` is the same call with flags, and takes the same two
    // descriptors. `RENAME_NOREPLACE` is the flag that makes it worth having:
    // it refuses to clobber, which a plain `rename` never does.
    fs::write(inside("b.part"), b"second").expect("write the second source");
    fs::write(inside("b.done"), b"already here").expect("write the target");
    match open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()) {
        Ok(dirfd) => {
            let refused = renameat_with(
                &dirfd,
                "b.part",
                &dirfd,
                "b.done",
                rustix::fs::RenameFlags::NOREPLACE,
            );
            r.check(
                &format!("NOREPLACE refuses to clobber an existing name ({refused:?})"),
                refused.is_err(),
            );
            // The target must be untouched, and the source must survive: a
            // refused rename moves nothing.
            r.check(
                "the existing name still holds what it held",
                fs::read(inside("b.done")).ok().as_deref() == Some(&b"already here"[..]),
            );
            r.check("and the source is still there", inside("b.part").exists());

            // Without the flag it replaces, which is what `mv` relies on.
            let replaced = renameat_with(
                &dirfd,
                "b.part",
                &dirfd,
                "b.done",
                rustix::fs::RenameFlags::empty(),
            );
            r.check("without the flag it replaces", replaced.is_ok());
            r.check(
                "and the target now holds the source's bytes",
                fs::read(inside("b.done")).ok().as_deref() == Some(&b"second"[..]),
            );

            // A source that is not there is ENOENT, not a silent success -
            // the other half of "a rename reports what it did".
            let missing = renameat(&dirfd, "never-existed", &dirfd, "target");
            r.check(
                &format!("a source that is not there fails ({missing:?})"),
                missing == Err(rustix::io::Errno::NOENT),
            );

            // And the tidy-up, so a rerun starts from nothing.
            let _ = unlinkat(&dirfd, "a.done", AtFlags::empty());
            let _ = unlinkat(&dirfd, "b.done", AtFlags::empty());
        }
        Err(err) => r.check(&format!("the directory can be opened (got {err})"), false),
    }

    fs::write(inside("c.part"), b"by path").expect("write the third source");
    let by_path = fs::rename(inside("c.part"), inside("c.done"));
    r.check("a rename by full path works too", by_path.is_ok());
    r.check(
        "and lands where the path said",
        fs::read(inside("c.done")).ok().as_deref() == Some(&b"by path"[..]),
    );

    // === a directory rename moves the *subtree* ============================
    //
    // `renameat2` had no directory arm at all (`_ => return err::EISDIR`),
    // under a comment saying a directory rename would have to move the whole
    // subtree's parent back-pointer. What the comment had wrong is that this
    // tree keeps no parent pointers to move: `..` is answered by a *search*
    // (`MountTable::parent_of`), so a directory's location follows from which
    // directory holds its name. The move is therefore a name move, and the
    // filesystem's own `rename_dir_into` does the accounting - two map edits on
    // ramfs, an add/`..`-rewrite/remove on ext4 (`docs/notes/` §6.186).
    //
    // Measured on Linux 7.3.4 (`build/scratch/opathmmap/rename_dir2.c`): a
    // populated subtree moves whole, the moved directory keeps its mode and
    // contents, an empty target directory is **replaced**, a non-empty one is
    // `ENOTEMPTY` with nothing moved, a directory onto a file is `ENOTDIR`, a
    // file onto a directory is `EISDIR`, and into its own subtree is `EINVAL`.
    let rd = Path::new("/tmp/t-rename_dirs");
    let _ = fs::remove_dir_all(rd);
    let d = |p: &str| rd.join(p);
    if fs::create_dir_all(d("a/b/c")).is_ok()
        && fs::write(d("a/b/c/f"), b"deep").is_ok()
        && fs::write(d("a/g"), b"top").is_ok()
    {
        let moved = fs::rename(d("a"), d("z"));
        r.check("a populated subtree renames", moved.is_ok());
        r.check(
            "a file three levels down survived it",
            fs::read(d("z/b/c/f")).ok().as_deref() == Some(&b"deep"[..]),
        );
        r.check(
            "a file one level down survived it",
            fs::read(d("z/g")).is_ok(),
        );
        r.check("the old name is gone", fs::metadata(d("a")).is_err());
        // The claim that would pass on a half-move: `..` must reach the new
        // parent, so `z/b/c/../..` is `z`.
        r.check(
            "and `..` inside the moved subtree reaches the new parent",
            fs::metadata(d("z/b/c/../..")).is_ok() && fs::read_dir(d("z/b/c/../..")).is_ok(),
        );

        // The moved directory's own mode and owner travel with it - a rename
        // that rebuilt the node would reset them.
        let _ = fs::create_dir_all(d("m/sub"));
        let _ = fs::write(d("m/sub/k"), b"k");
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(d("m"), fs::Permissions::from_mode(0o700));
        let kept = fs::rename(d("m"), d("n"));
        r.check("a directory with a mode renames", kept.is_ok());
        r.check(
            "and keeps its mode",
            fs::metadata(d("n"))
                .map(|m| m.permissions().mode() & 0o777 == 0o700)
                .unwrap_or(false),
        );
        r.check("and its contents", fs::read(d("n/sub/k")).is_ok());

        // The four refusals, each with the errno Linux gives.
        let _ = fs::create_dir_all(d("e1"));
        let _ = fs::create_dir_all(d("e2"));
        r.check(
            "a directory onto an existing EMPTY directory is served",
            fs::rename(d("e1"), d("e2")).is_ok(),
        );
        r.check(
            "and the empty target's name now reaches the source's inode",
            fs::metadata(d("e2")).is_ok() && fs::metadata(d("e1")).is_err(),
        );
        let _ = fs::create_dir_all(d("ne/inner"));
        let errno = |e: std::io::Error| e.raw_os_error();
        r.check(
            "a directory onto a NON-empty one is ENOTEMPTY",
            errno(fs::rename(d("e2"), d("ne")).unwrap_err()) == Some(libc::ENOTEMPTY),
        );
        r.check(
            "and nothing moved: both names are still there",
            fs::metadata(d("e2")).is_ok() && fs::metadata(d("ne/inner")).is_ok(),
        );
        let _ = fs::write(d("file"), b"x");
        r.check(
            "a directory onto a file is ENOTDIR",
            errno(fs::rename(d("e2"), d("file")).unwrap_err()) == Some(libc::ENOTDIR),
        );
        r.check(
            "a file onto a directory is EISDIR",
            errno(fs::rename(d("file"), d("ne")).unwrap_err()) == Some(libc::EISDIR),
        );
        let _ = fs::create_dir_all(d("sub/inner2"));
        r.check(
            "a directory into its own subtree is EINVAL",
            errno(fs::rename(d("sub"), d("sub/inner2/x")).unwrap_err()) == Some(libc::EINVAL),
        );
        r.check(
            "a directory onto itself is the no-op",
            fs::rename(d("sub"), d("sub")).is_ok(),
        );
    } else {
        r.check("the directory-rename fixture is made", false);
    }
    let _ = fs::remove_dir_all(rd);

    if r.failed {
        println!("t-rename: FAILED");
        std::process::exit(1);
    }
    println!("t-rename: all passed");
}
