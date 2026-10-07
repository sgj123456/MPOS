//! t-stat-consistency - `stat(path)` and `fstat(fd)` must name the same object.
//!
//! The shape pinned here is the one a lock-file protocol depends on: open a
//! path with `O_CREAT|O_TRUNC|O_RDWR`, then compare what `fstat` says about the
//! descriptor with what `stat` says about the path. A caller uses the pair to
//! close a race - it locks the file, then confirms the name still points at the
//! inode it locked, and retries if it does not. If the two answers disagree
//! about a file nobody has touched, that retry never converges: it is a tight
//! loop, not a wait.
//!
//! That is what happened here. `wayland-backend`'s pure-Rust server - the
//! backend `niri` uses, and the reason it links no `libwayland-server` -
//! acquires `<XDG_RUNTIME_DIR>/wayland-N.lock` exactly this way, and the loop
//! spun forever on an unchanged file, in state R with zero voluntary context
//! switches. The counterfactual is that loop: with the two answers agreeing
//! this program passes and the compositor gets past its socket setup.
//!
//! Two paths, chosen for what each says about the filesystem: `/tmp` is the
//! ramfs the boot mounts itself, and `/run/user/0` is where the lock file
//! actually lives (made by the image's own `/etc/local.d`). A path that cannot
//! be opened is reported rather than passed over in silence.
//!
//! # The migration changed what "the struct" means
//!
//! The no-std version declared the 144-byte x86_64 `struct stat` (`lin::Stat`)
//! and read four fields off it. Here the struct is `rustix`'s machine-native
//! `Stat` - the same shape `t-fstat` and `t-newfstatat` already answer with -
//! so the claims stay *answers* (inode, device, link count, size) and the byte
//! layout stops being one of them; the layout pin lives in `t-stat-buffer`.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, Stat, fstat, mkdirat, openat, stat, unlinkat};

/// `O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC`: the flags the callers use.
fn create_trunc() -> OFlags {
    OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC
}

/// The ramfs the boot mounts itself.
const TMP: &str = "/tmp/t-stat-consistency.lock";
/// Where niri's lock actually lives.
const RUN: &str = "/run/user/0/t-stat-consistency.lock";

/// Print the numbers, so a red run carries its own evidence: a divergence is
/// easier to act on with both answers printed than with the verdict alone.
fn print_pair(tag: &str, st: &Stat) {
    println!(
        "     {}: dev={} ino={} nlink={} size={}",
        tag, st.st_dev, st.st_ino, st.st_nlink, st.st_size
    );
}

/// Make the lock file's own directory exist, without depending on the image's
/// userspace having made it: niri's lock lives in `$XDG_RUNTIME_DIR`, and the
/// point of the second path is to reach the same place it does. A refusal
/// because something is already there is the expected answer after the first
/// run, so neither result is checked - the open below is the check.
fn ensure_runtime_dir() {
    for path in ["/run", "/run/user", "/run/user/0"] {
        let _ = mkdirat(CWD, path, Mode::from_raw_mode(0o755));
    }
}

/// The pair against one path. Returns whether the path was reachable at all.
fn check(r: &mut Report, label: &str, path: &str) -> bool {
    println!("  -- {label} --");
    let fd = match openat(CWD, path, create_trunc(), Mode::from_raw_mode(0o660)) {
        Ok(fd) => fd,
        Err(_) => {
            println!("skip: the path could not be opened on this boot");
            return false;
        }
    };

    let by_fd = fstat(&fd);
    let by_path = stat(path);
    r.check("fstat succeeds", by_fd.is_ok());
    r.check("stat succeeds", by_path.is_ok());
    if let (Ok(a), Ok(b)) = (&by_fd, &by_path) {
        print_pair("fstat", a);
        print_pair("stat ", b);
        r.check("fstat and stat name the same inode", a.st_ino == b.st_ino);
        r.check("fstat and stat name the same device", a.st_dev == b.st_dev);
        r.check("the link count agrees", a.st_nlink == b.st_nlink);
        r.check("the size agrees", a.st_size == b.st_size);
    }

    // And in the order the lock protocol uses: stat the *path* first, then open
    // (truncating), then fstat the descriptor. The protocol compares the read
    // taken before the open with the one taken after it, so an inode that
    // changed in between - or two that simply disagree - spins the retry.
    let before = stat(path);
    r.check("the pre-open stat succeeds", before.is_ok());
    let fd2 = openat(CWD, path, create_trunc(), Mode::from_raw_mode(0o660));
    r.check("the second open succeeds", fd2.is_ok());
    if let (Ok(before), Ok(fd2)) = (before, fd2) {
        let after = fstat(&fd2);
        if let Ok(after) = &after {
            print_pair("pre-open stat ", &before);
            print_pair("post-open fstat", after);
            r.check(
                "the pre-open stat and the post-open fstat agree",
                before.st_ino == after.st_ino && before.st_dev == after.st_dev,
            );
        }
    }

    drop(fd);
    let _ = unlinkat(CWD, path, AtFlags::empty());
    true
}

fn main() {
    println!("t-stat-consistency");
    let mut r = Report::new();

    let mut reachable = 0;
    if check(&mut r, "ramfs (/tmp)", TMP) {
        reachable += 1;
    }
    ensure_runtime_dir();
    if check(&mut r, "the runtime directory (/run/user/0)", RUN) {
        reachable += 1;
    }
    // A run that reached neither path proved nothing, so it must not pass.
    r.check("at least one of the two paths was reachable", reachable > 0);

    r.finish()
}
