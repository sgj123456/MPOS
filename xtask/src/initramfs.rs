//! The `initramfs` command: the early userspace the kernel unpacks as its first
//! root filesystem. This is `make_initramfs` from `scripts/mkdisk.sh`.
//!
//! The kernel does not mount the disk itself. It unpacks this archive over a
//! ramfs, runs `/init` out of it, and that script mounts the real root
//! filesystem and pivots into it. So the tree needs exactly three things: a
//! shell, the tools to mount with, and the script.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::InitramfsConfig;
use crate::utils;

pub fn build_initramfs(build_dir: &Path, _profile: &str) -> Result<()> {
    let root = utils::project_root()?;
    let config_path = root.join("initramfs.toml");
    let config = InitramfsConfig::load(&config_path)?;

    let staging = build_dir.join("initramfs");
    let tar = utils::initramfs_path(build_dir);

    if !needs_rebuild(build_dir)? {
        println!("initramfs: up to date");
        return Ok(());
    }

    println!("Building the initramfs");
    fs::create_dir_all(build_dir)?;

    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for dir in ["etc/apk/keys", "proc", "sys", "dev", "sysroot"] {
        fs::create_dir_all(staging.join(dir))?;
    }

    let repositories = utils::repositories_text(&config.mirror, &config.branch, &config.repos);
    fs::write(staging.join("etc/apk/repositories"), repositories)?;

    utils::install_apk_keys(&staging.join("etc/apk/keys"))?;

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;

    let mut packages = config.base_packages.clone();
    packages.extend(config.extra_packages.clone());

    // `--no-scripts` because the package scripts are written for a chroot and
    // this tree is not one yet. What they also do is not optional: busybox's
    // post-install creates the applet links, so the install is repeated below
    // by hand, running busybox against the tree it was just unpacked into.
    let status = Command::new(&apk)
        .args(["--no-progress", "add"])
        .arg("--root")
        .arg(&staging)
        .args([
            "--arch",
            &config.arch,
            "--initdb",
            "--no-scripts",
            "--update-cache",
        ])
        .args(&packages)
        .status()
        .context("Failed to install the initramfs packages")?;
    if !status.success() {
        anyhow::bail!("The initramfs packages did not install");
    }

    // busybox --install lays down every applet as a symlink in one pass, and it
    // has to run as the binary in the tree so the links point where this
    // initramfs will look for them rather than at the host's busybox.
    let status = Command::new("chroot")
        .arg(&staging)
        .args(["/bin/busybox", "--install", "-s"])
        .status()
        .context("Failed to run busybox --install in the initramfs tree")?;
    if !status.success() {
        anyhow::bail!("Could not install the busybox applet links");
    }

    // `/init` is a file of its own now - `initramfs/init`, applied with the
    // rest of the overlay below - so nothing here writes it. It used to be a
    // constant in this binary, which is why a rebuilt `xtask` had to count as a
    // reason to rebuild the archive; a file in the tree has an mtime, and the
    // staleness check reads that instead.
    let sh = staging.join("bin/sh");
    if sh.symlink_metadata().is_ok() {
        fs::remove_file(&sh)?;
    }
    std::os::unix::fs::symlink("/bin/busybox", &sh)?;

    utils::merge_overlay(&root.join("initramfs"), &staging)?;

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
        .context("Failed to pack the initramfs")?;
    if !status.success() {
        anyhow::bail!("The initramfs did not pack");
    }

    let size = fs::metadata(&tar)?.len();
    println!(
        "initramfs: {} ({:.1} KB)",
        tar.display(),
        size as f64 / 1024.0
    );

    fs::remove_dir_all(&staging)?;
    Ok(())
}

/// Builds the archive unless the one on disk is already newer than its inputs.
pub fn ensure(build_dir: &Path, profile: &str) -> Result<()> {
    build_initramfs(build_dir, profile)
}

/// True when the archive is missing or older than what it is built from: the
/// configuration and the `initramfs/` overlay, which is where the init script
/// and everything else in the tree lives.
///
/// The binary is not an input. It was one while `/init` was a constant inside
/// it - a rebuilt `xtask` meant a new init script, and nothing else said so.
/// With the script a file in the overlay, the tree's own mtimes answer the
/// question, and editing this crate no longer rebuilds an archive whose
/// contents did not change.
///
/// `run` asks this same question through `utils::image_stale`, which is what
/// makes a stale image rebuild instead of booting code that is no longer on
/// disk.
pub fn needs_rebuild(build_dir: &Path) -> Result<bool> {
    let root = utils::project_root()?;
    let tar = utils::initramfs_path(build_dir);
    if !tar.exists() {
        return Ok(true);
    }

    let tar_time = utils::file_mtime(&tar)?;

    Ok(utils::newer(&root.join("initramfs.toml"), &tar)?
        || utils::dir_newer_than(&root.join("initramfs"), tar_time)?)
}
