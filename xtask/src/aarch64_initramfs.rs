//! The second machine's initramfs: the early userspace its kernel unpacks as
//! its first root, and the script that switches the real one in.
//!
//! Until §15.6 this machine had no copy of this tree at all. Its test boots
//! carried a hand-encoded launcher as `/init` and pivoted to the disk *inside
//! the kernel*, and its Alpine boot carried **no** archive - the disk was the
//! whole userland and the kernel switched to it. Both were the same duplication
//! `PLAN.md` Phase 15 names: the root switch is one flow, and the first machine
//! has always run it from an initramfs script, because mounting `/proc`, `/dev`
//! and the real root is userspace's job (`AGENTS.md` §4 rule 1). This module is
//! where the second machine stops being the exception.
//!
//! The build is the x86_64 side's shape with two measured differences
//! (`xtask/src/initramfs.rs`):
//!
//! * **no chroot.** The x86 side installs busybox and then runs
//!   `chroot /bin/busybox --install -s` to lay the applet links - which works
//!   only because its host can exec its own arch. An aarch64 busybox cannot be
//!   run here at all (the cross-arch `execve: Exec format error` the Alpine
//!   install records), so the farm is laid by hand: every applet name the boot
//!   scripts use becomes a symlink to `/bin/busybox`. The guest makes the rest
//!   of the farm itself at first boot (`rootfs-aarch64/init` runs
//!   `busybox --install -s` on the machine that can).
//! * **a pinned package set**, not the toml's: the archive needs exactly a
//!   shell, `mount`, `switch_root` and `pivot_root`, and every name added here
//!   costs the boot a file it does not need.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::utils;

/// The packages the archive carries. `alpine-baselayout` is there for the
/// directories a mount wants to land on (`/dev`, `/proc`, `/sys` exist before
/// the script mounts over them); busybox is everything else.
const PACKAGES: [&str; 2] = ["busybox", "alpine-baselayout"];

/// The applets the initramfs's own scripts exec, besides the shell. `switch_root`
/// is the pivot; `pivot_root` is what the *disk* `/init` chain may reach for;
/// `mkdir`/`cat`/`echo` are the script's file vocabulary. A name missing from
/// the farm is a boot that dies on `ENOENT` mid-script, so this list is part of
/// the contract with `initramfs-aarch64/init` and `rootfs-aarch64/init`.
pub(crate) const APPLETS: [&str; 8] = [
    "sh",
    "mount",
    "umount",
    "switch_root",
    "pivot_root",
    "mkdir",
    "cat",
    "echo",
];

/// Where the staging tree lives under `build/`.
fn staging_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-initramfs")
}

/// The packed archive - the file the ESP step copies onto the volume as
/// `initramfs.tar`, named apart from the x86_64 side's so neither boot can
/// read the other's.
pub fn tar_path(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-initramfs.tar")
}

/// Build the archive unless the one on disk is newer than what it is built from:
/// the config and the overlay tree (whose `/init` is the script). The package
/// set is pinned in code, so a change to it is a change to this file - and a
/// rebuilt `xtask` has never been an input to an archive whose contents did not
/// change (`xtask/src/initramfs.rs`'s lesson, kept).
pub fn ensure(build_dir: &Path) -> Result<()> {
    let tar = tar_path(build_dir);
    if !needs_rebuild(build_dir, &tar)? {
        println!("initramfs: aarch64 up to date");
        return Ok(());
    }
    build(build_dir)
}

/// The packed archive, built if needed - the path every aarch64 boot installs
/// onto its ESP as `initramfs.tar`.
pub fn ensure_path(build_dir: &Path) -> Result<PathBuf> {
    ensure(build_dir)?;
    Ok(tar_path(build_dir))
}

/// True when the archive is missing or older than any of its inputs: the toml
/// and the overlay tree (whose `/init` is the script). The stamp beside the tar
/// records *when* it was laid, so an overlay edit that lands after a rebuild
/// still asks for another one.
fn needs_rebuild(build_dir: &Path, tar: &Path) -> Result<bool> {
    let root = utils::project_root()?;
    if !tar.exists() {
        return Ok(true);
    }
    if utils::newer(&root.join("initramfs-aarch64.toml"), tar)? {
        return Ok(true);
    }
    let stamp = build_dir.join("aarch64-initramfs.stamp");
    if !stamp.exists() {
        return Ok(true);
    }
    let since = utils::file_mtime(&stamp)?;
    Ok(utils::dir_newer_than(
        &root.join("initramfs-aarch64"),
        since,
    )?)
}

/// Lay the tree and pack it: apk-installed packages, the hand-made applet farm,
/// the overlay's `/init`, ustar.
fn build(build_dir: &Path) -> Result<()> {
    let root = utils::project_root()?;
    let config = crate::config::InitramfsConfig::load(&root.join("initramfs-aarch64.toml"))?;
    let staging = staging_dir(build_dir);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // Repositories and keys before anything asks apk to resolve a name: the
    // trust decision is a filename match against `etc/apk/keys`, and without
    // the keys every package answers `UNTRUSTED signature` (the story the
    // Alpine install tells for the same reason).
    fs::create_dir_all(staging.join("etc/apk/keys"))?;
    fs::write(
        staging.join("etc/apk/repositories"),
        utils::repositories_text(&config.mirror, &config.branch, &config.repos),
    )?;
    utils::install_apk_keys(&staging.join("etc/apk/keys"))?;

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;
    let mut packages: Vec<String> = PACKAGES.iter().map(|p| p.to_string()).collect();
    packages.extend(config.extra_packages.clone());
    // `--no-scripts` for the same measured reason as the Alpine install: the
    // post-install scripts are the *target's* binaries and this host cannot
    // exec them. What those scripts would have made - the applet farm - is laid
    // below by hand, because the chroot trick the x86 side uses cannot run an
    // aarch64 busybox either. `--usermode` is what lets a non-root build write
    // the database at all (measured: without it apk answers "Use --usermode to
    // allow creating database as non-root"), and it costs nothing here - the
    // archive is packed with `--owner=0`, so the guest sees root ownership no
    // matter who ran the build. The command shape is otherwise the x86_64
    // initramfs's own.
    //
    // Except under root, where apk refuses the flag outright (`ERROR:
    // --usermode not allowed as root`) - a root build needs no such permission
    // and used to die here on any re-pack, which is how a one-line change to a
    // boot script became unverifiable.
    let mut flags = vec![
        "--arch",
        config.arch.as_str(),
        "--initdb",
        "--no-scripts",
        "--update-cache",
    ];
    if !utils::is_root() {
        flags.push("--usermode");
    }
    crate::rootfs::apk_add(&[apk.as_str()], &staging, &flags, &packages)?;

    // The farm. Absolute targets, because the guest resolves a symlink relative
    // to the tree it stands in and `busybox` sits at `/bin/busybox`; and only
    // where nothing already answered - baselayout ships some of these names,
    // and replacing a real file with a link back to busybox would lose content
    // the script reads.
    fs::create_dir_all(staging.join("bin"))?;
    for applet in APPLETS {
        let path = staging.join(format!("bin/{applet}"));
        if path.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink("/bin/busybox", &path)
            .with_context(|| format!("Failed to link /bin/{applet} into the initramfs"))?;
    }

    // The overlay last: `/init` is a file in the tree, so editing the boot's
    // script is editing a file and not this crate (the x86 side's lesson).
    utils::merge_overlay(&root.join("initramfs-aarch64"), &staging)?;
    let init = staging.join("init");
    fs::set_permissions(&init, fs::Permissions::from_mode(0o755))
        .context("The aarch64 initramfs carries no /init")?;

    // Packed the way the x86 side packs: ustar, owned by root inside the guest
    // whatever ran the build, entries relative to the tree root. The kernel's
    // `vfs::tar` reads the mode the archive records, and `execve` refuses a
    // non-executable `/init` even for root - hence the chmod above surviving
    // into the tar through `--numeric-owner`.
    let tar = tar_path(build_dir);
    let status = Command::new("tar")
        .args([
            "--format=ustar",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
        ])
        .arg(&tar)
        .arg("-C")
        .arg(&staging)
        .arg(".")
        .status()
        .context("Failed to pack the aarch64 initramfs")?;
    if !status.success() {
        anyhow::bail!("The aarch64 initramfs did not pack");
    }
    fs::write(
        build_dir.join("aarch64-initramfs.stamp"),
        format!("{}\n", packages.join("\n")),
    )?;
    let size = fs::metadata(&tar)?.len();
    println!(
        "initramfs: aarch64 {} ({:.1} KB)",
        tar.display(),
        size as f64 / 1024.0
    );
    fs::remove_dir_all(&staging)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The farm's list is a contract with the script: every verb declared in
    /// `initramfs-aarch64/init`'s `@vocabulary:` line must be an applet busybox
    /// answers - shipped by the package or laid by the farm above. A name that
    /// is neither is a boot that dies on `ENOENT` mid-pivot, and a headless
    /// verdict reads that as a hang; this pin fails at build time instead. The
    /// declaration is read rather than scanned because a shell-script scanner is
    /// a heuristic, and a heuristic that passes is the failure mode here.
    #[test]
    fn the_farm_covers_the_scripts_vocabulary() {
        let root = utils::project_root().unwrap();
        let text = std::fs::read_to_string(root.join("initramfs-aarch64/init")).unwrap();
        let line = text
            .lines()
            .find(|l| l.starts_with("@vocabulary:"))
            .expect("the initramfs script declares its @vocabulary:");
        for verb in line.trim_start_matches("@vocabulary:").split_whitespace() {
            assert!(
                APPLETS.contains(&verb),
                "`{verb}` is execed by the initramfs script but is not in the farm"
            );
        }
    }
}
