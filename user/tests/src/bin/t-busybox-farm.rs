//! t-busybox-farm - the applet names this image carries are the ones its own
//! busybox would install.
//!
//! busybox ships one binary and a post-install script that makes the names
//! (`/sbin/init`, `/bin/mount`, ...) point at it. On a machine whose userland
//! the *host* cannot run, that script never runs - a cross-architecture `apk`
//! installs with `--no-scripts` - so the names are laid by the build instead,
//! from a recorded list (`assets/configs/busybox-applets.txt`, which this file
//! compiles in as its own copy of the *same* bytes `xtask` reads). That list is
//! the one thing in this arrangement that can go stale: a busybox upgrade that
//! gains, renames or moves an applet leaves a name no boot script can exec, and
//! nothing about the image would say so until a script died on `ENOENT`
//! mid-boot.
//!
//! So this asks the machine's own binary. `/bin/busybox --list-full` is what
//! busybox itself would install - each applet's name and the directory it
//! belongs in - and the claims below are that it names exactly the paths the
//! file records, and that every one of those paths is in the image and points at
//! `/bin/busybox`. A path that is a real program instead is not a failure: the
//! build's rule is to leave a name that already answers alone (`sbin/ifup` is
//! ifupdown's, `sbin/mkfs.vfat` is dosfstools'), and the file names those paths
//! because busybox has the applet.
//!
//! Regenerating the file after a red run is one command on the machine, and the
//! file's own header carries it: `busybox --list-full | LC_ALL=C sort`.

use mpos_user_tests::Report;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// The build's farm list, compiled in: the same file
/// `xtask/src/rootfs.rs::lay_applet_farm` links from, so the machine and the
/// build that made its image cannot disagree about which paths the farm is.
const FARM: &str = include_str!("../../../../assets/configs/busybox-applets.txt");

fn main() {
    println!("t-busybox-farm");
    let mut r = Report::new();

    let recorded: Vec<&str> = FARM
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    r.check(
        &format!("the recorded farm parses ({} paths)", recorded.len()),
        recorded.len() > 100,
    );

    // What the build did with the list: every recorded path is in the image, and
    // resolvable - which is what both outcomes look like. A path the build
    // linked into `/bin/busybox` resolves through it, and a name a *package*
    // already owns (`sbin/ifup` is ifupdown's own link, `sbin/openrc` a real
    // binary) resolves to that, because the build's rule is to leave an
    // answering name alone. A name that is missing or dangling is what this
    // catches: it is a boot script that dies on `ENOENT`.
    let mut missing: Vec<&str> = Vec::new();
    let mut dangling: Vec<&str> = Vec::new();
    let mut ours = 0usize;
    let mut theirs: Vec<&str> = Vec::new();
    for path in &recorded {
        let full = format!("/{path}");
        match std::fs::metadata(&full) {
            Err(_) => {
                missing.push(path);
                continue;
            }
            Ok(_) => {}
        }
        let linked = std::fs::read_link(&full).ok();
        if linked.as_deref() == Some(Path::new("/bin/busybox")) {
            ours += 1;
        } else {
            if linked.is_some() {
                // A package's own link: it has to answer too, or the name is
                // broken whichever side of the race made it.
                if std::fs::canonicalize(&full).is_err() {
                    dangling.push(path);
                }
            }
            theirs.push(path);
        }
    }
    r.check(
        &format!(
            "every recorded path is in the image ({} missing: {missing:?})",
            missing.len()
        ),
        missing.is_empty(),
    );
    r.check(
        &format!(
            "... and resolves - {} are the build's links into /bin/busybox, {} are a package's \
             own answer ({theirs:?}), {} dangling: {dangling:?}",
            ours,
            theirs.len(),
            dangling.len()
        ),
        dangling.is_empty(),
    );
    // The one name the whole arrangement exists for: without it the disk has no
    // pid-1 chain at all, which is what a cross-architecture install leaves
    // behind when nobody lays the farm. On an installed image this name is the
    // farm's link into busybox; on a *test* disk the payload's runner has taken
    // it (the payload lands last by contract), so what is claimed is what both
    // owe - a `/sbin/init` that is there and can be executed, which is what
    // `switch_root` asks for a moment later.
    r.check(
        "/sbin/init is present and executable",
        std::fs::metadata("/sbin/init").is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0),
    );

    // The oracle is the machine's own busybox. Its answer is the applet set of
    // *this* build, so a list that has drifted from it is a list that would lay
    // the wrong names - and the printed difference is the thing to regenerate
    // from.
    match Command::new("/bin/busybox").arg("--list-full").output() {
        Ok(out) if out.status.success() => {
            let theirs: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect();
            let ours: Vec<String> = recorded.iter().map(|path| path.to_string()).collect();
            let only_theirs: Vec<&String> =
                theirs.iter().filter(|path| !ours.contains(path)).collect();
            let only_ours: Vec<&String> =
                ours.iter().filter(|path| !theirs.contains(path)).collect();
            r.check(
                &format!(
                    "busybox names exactly the recorded paths ({} in busybox only: {only_theirs:?}; \
                     {} recorded only: {only_ours:?})",
                    only_theirs.len(),
                    only_ours.len()
                ),
                only_theirs.is_empty() && only_ours.is_empty(),
            );
        }
        other => r.check(
            &format!("/bin/busybox --list-full answers ({other:?})"),
            false,
        ),
    }

    r.finish()
}
