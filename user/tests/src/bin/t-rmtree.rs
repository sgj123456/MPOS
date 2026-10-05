//! t-rmtree - taking a whole tree away, which is what `rm -r` and every build
//! script does.
//!
//! `std::fs::remove_dir_all` is not `rmdir` in a loop. It opens the directory,
//! reads it entry by entry, and takes each entry away *by descriptor* - a file
//! with `unlinkat(fd, name, 0)`, a directory by being opened and walked in turn
//! - and unlinks each directory only once it is empty. A tree that is not empty
//! is therefore a walk the other tests never make, and it is the walk a shell's
//! `rm -r` is built on.
//!
//! It is also an end-to-end check on something that looks unrelated, and the
//! reason the flat case below is not merely bookkeeping: `struct stat` is 128
//! bytes on this machine and the kernel was filling 144, so every stat wrote
//! sixteen bytes past the struct - and musl's `fdopendir`, which stats the
//! descriptor before it reads anything, keeps a `struct stat` inside the frame
//! it needs its return address in. A non-empty `remove_dir_all` is where that
//! was found: the second machine jumped to `pc 0` with `lr 0` inside the call,
//! and the flat case is the shape it died of (`PLAN.md` §13.10, and
//! `t-stat-buffer` for the length itself).
//!
//! The refusals are pinned too, because they are the difference between taking
//! a tree away and taking away something that is not a tree.

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

/// The scratch parent this test owns, emptied before use.
const ROOT: &str = "/tmp/t-rmtree";

/// Make `root/<name>` holding the files it is given, and answer the path.
fn tree(name: &str, files: &[&str]) -> String {
    let base = format!("{ROOT}/{name}");
    fs::create_dir_all(&base).expect("make the scratch tree");
    for file in files {
        let path = format!("{base}/{file}");
        fs::create_dir_all(Path::new(&path).parent().unwrap()).expect("make the parent");
        fs::write(path, b"x").expect("write the scratch file");
    }
    base
}

fn main() {
    println!("t-rmtree");
    let mut r = Report { failed: false };
    let _ = fs::remove_dir_all(ROOT);
    if fs::create_dir(ROOT).is_err() {
        r.check("the scratch parent can be made", false);
        std::process::exit(1);
    }

    // A directory holding two files: the shape a build script's output
    // directory has, and the one that died on the second machine.
    let flat = tree("flat", &["alpha.txt", "beta.txt"]);
    let removed = fs::remove_dir_all(&flat);
    r.check(
        &format!("a directory of two files comes away ({removed:?})"),
        removed.is_ok(),
    );
    r.check("and the directory is gone", !Path::new(&flat).exists());
    r.check(
        "and nothing else under the scratch parent went with it",
        Path::new(ROOT).is_dir(),
    );

    // One level deeper: the recursive arm, which opens a descriptor per level
    // and so makes one `fdopendir` per level.
    let nested = tree(
        "nested",
        &["top.txt", "sub/mid.txt", "sub/deeper/bottom.txt"],
    );
    let removed = fs::remove_dir_all(&nested);
    r.check(
        &format!("a nested tree comes away ({removed:?})"),
        removed.is_ok(),
    );
    r.check(
        "and every level of it is gone",
        !Path::new(&nested).exists(),
    );

    // An entry that is a directory but has nothing in it: the call that ends
    // each level, `unlinkat(fd, name, AT_REMOVEDIR)`, is the only one that
    // reaches it.
    let hollow = format!("{ROOT}/hollow");
    fs::create_dir_all(format!("{hollow}/empty")).expect("make the hollow tree");
    let removed = fs::remove_dir_all(&hollow);
    r.check(
        &format!("a tree whose only entry is an empty directory comes away ({removed:?})"),
        removed.is_ok() && !Path::new(&hollow).exists(),
    );

    // A tree that is a symlink is taken away as the *link*: the thing it names
    // is not this call's to delete, and deleting through it would be the
    // difference between `rm -r link` and `rm -r link/`.
    let target = tree("target", &["kept.txt"]);
    let link = format!("{ROOT}/link");
    let linked = std::os::unix::fs::symlink(&target, &link).is_ok();
    if linked {
        let removed = fs::remove_dir_all(&link);
        r.check(
            &format!("a symlink to a tree comes away ({removed:?})"),
            removed.is_ok(),
        );
        r.check(
            "and the tree it named is still there, with its file",
            Path::new(&format!("{target}/kept.txt")).is_file(),
        );
        let _ = fs::remove_dir_all(&target);
    }

    // The refusals: a name that is not there, and a name that is not a
    // directory at all.
    let absent = fs::remove_dir_all(format!("{ROOT}/never-existed"));
    r.check(
        &format!("a tree that is not there fails ({absent:?})"),
        absent.is_err(),
    );
    let file = format!("{ROOT}/plain.txt");
    fs::write(&file, b"x").expect("write the plain file");
    let not_a_tree = fs::remove_dir_all(&file);
    r.check(
        &format!("a plain file is refused ({not_a_tree:?})"),
        not_a_tree.is_err(),
    );
    r.check("and the file is untouched", Path::new(&file).is_file());

    // Leave the machine as it was found, with the calls that are not the
    // subject: `read_dir` to check the scratch parent is empty again, and
    // `remove_dir` to take it away.
    let _ = fs::remove_file(&file);
    let emptied = fs::read_dir(ROOT).map(|mut d| d.next().is_none());
    let empty_ok = matches!(&emptied, Ok(true));
    r.check(
        &format!("and the scratch parent is empty again ({emptied:?})"),
        empty_ok,
    );
    let _ = fs::remove_dir(ROOT);

    if r.failed {
        println!("t-rmtree: FAILED");
        std::process::exit(1);
    }
    println!("t-rmtree: all passed");
}
