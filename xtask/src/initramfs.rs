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

    if tar.exists() {
        let tar_time = utils::file_mtime(&tar)?;
        if !utils::newer(&config_path, &tar)?
            && !utils::dir_newer_than(&root.join("initramfs"), tar_time)?
        {
            println!("initramfs: up to date");
            return Ok(());
        }
    }

    println!("Building the initramfs");
    fs::create_dir_all(build_dir)?;

    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for dir in ["etc/apk/keys", "proc", "sys", "dev", "sysroot"] {
        fs::create_dir_all(staging.join(dir))?;
    }

    let repositories = format!(
        "{}/{}/main\n{}/{}/community\n",
        config.mirror, config.branch, config.mirror, config.branch
    );
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

    fs::write(staging.join("init"), INIT)?;
    utils::set_mode(&staging.join("init"), 0o755)?;

    let sh = staging.join("bin/sh");
    if sh.symlink_metadata().is_ok() {
        fs::remove_file(&sh)?;
    }
    std::os::unix::fs::symlink("/bin/busybox", &sh)?;

    utils::copy_dir_recursive(&root.join("initramfs"), &staging)?;

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

const INIT: &str = r#"#!/bin/sh
# Mount the pseudo-filesystems on the temporary root, mount the root
# filesystem the kernel command line names, and hand the machine over.
#
# This runs as pid 1 on a ramfs, with the archive the kernel unpacked as its
# whole world. Everything it does before `switch_root` is gone with that
# ramfs, and everything it mounts *onto the new root* is not.

export PATH=/sbin:/usr/sbin:/bin:/usr/bin

# Before the command line is read, because /proc/cmdline is where it is read
# from, and before anything else because every tool below wants one of these
# three.
mount -t proc     proc     /proc
mount -t sysfs    sysfs    /sys
mount -t devtmpfs devtmpfs /dev

root=''
rootflags='rw'
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        root=*)      root="${arg#root=}" ;;
        rootflags=*) rootflags="${arg#rootflags=}" ;;
    esac
done

if [ -z "$root" ]; then
    echo "initramfs: no root= on the command line; staying here"
    exec /bin/sh
fi

mkdir -p /sysroot
if ! mount -t ext2 -o "$rootflags" "$root" /sysroot; then
    echo "initramfs: cannot mount $root; staying here"
    exec /bin/sh
fi

# The directories the real init expects to find already mounted. What is
# mounted here survives the pivot; what is created here does not, so each one
# is made on the new root, not on the ramfs.
for dir in proc sys dev tmp run; do
    mkdir -p "/sysroot/$dir"
done

mount -t proc     proc     /sysroot/proc
mount -t sysfs    sysfs    /sysroot/sys
mount -t tmpfs    tmpfs    /sysroot/tmp
mount -t tmpfs    tmpfs    /sysroot/run
mount --move /dev /sysroot/dev

exec switch_root /sysroot /sbin/init
"#;
