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

    if r.failed {
        println!("t-rename: FAILED");
        std::process::exit(1);
    }
    println!("t-rename: all passed");
}
