//! The `disk` command: the raw image file the rest of the build assembles into.
//!
//! This is the `qemu-img create` half of `mkdisk.sh`, on its own: nothing here
//! needs root, because creating a file does not, so the image starts life owned
//! by the user and stays that way through the rootfs build - formatting and
//! mounting never change the owner of the file being formatted.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::RootfsConfig;
use crate::utils::{image_path, project_root};

pub fn build_disk(build_dir: &Path, _profile: &str) -> Result<()> {
    let config = RootfsConfig::load(&project_root()?.join("rootfs.toml"))?;
    let image = image_path(build_dir);

    if image.exists() {
        println!(
            "disk: {} is already there ({})",
            image.display(),
            config.image_size
        );
        return Ok(());
    }

    println!("Creating the disk image");
    fs::create_dir_all(build_dir)
        .with_context(|| format!("Failed to create {}", build_dir.display()))?;

    let status = Command::new("qemu-img")
        .args(["create", "-f", "raw"])
        .arg(&image)
        .arg(&config.image_size)
        .status()
        .context("Failed to run qemu-img")?;

    if !status.success() {
        anyhow::bail!("Failed to create the disk image");
    }

    println!("disk: {} created ({})", image.display(), config.image_size);
    Ok(())
}

/// Creates the image unless it is already there.
pub fn ensure(build_dir: &Path, profile: &str) -> Result<()> {
    if !image_path(build_dir).exists() {
        build_disk(build_dir, profile)?;
    }
    Ok(())
}
