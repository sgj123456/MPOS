//! The `run` command: boot the image the build just made. This is
//! `scripts/run.sh` as a command.
//!
//! QEMU runs as the user - the guest console is watched from this terminal, and
//! booting the machine as root buys nothing - and nothing here needs root at
//! all. The one exception is the first run on a tree with no image yet: there is
//! a build to do, and that is handled before this is reached.

use std::env;
use std::ffi::CString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::utils;

/// True when there is nothing to boot yet, or only the empty image file `disk`
/// made - the stamp is what says a root filesystem was installed into it.
pub fn needs_build(build_dir: &Path) -> Result<bool> {
    Ok(!utils::image_path(build_dir).exists() || !utils::rootfs_stamp(build_dir).exists())
}

pub fn run_qemu(
    build_dir: &Path,
    profile: &str,
    artefacts_ready: bool,
    needs_build: bool,
) -> Result<()> {
    if needs_build {
        println!("run: there is no complete image yet; building one");
        if !artefacts_ready {
            utils::build_kernel(profile)?;
        }
        crate::initramfs::ensure(build_dir, profile)?;
        crate::disk::ensure(build_dir, profile)?;
        crate::rootfs::ensure(build_dir, profile)?;
        println!();
        println!("run: the image is built; run this command again to boot it");
        return Ok(());
    }

    let image = utils::image_path(build_dir);

    // An image older than the kernel or the loader it was built from boots the
    // *previous* code and says nothing about it, which reads exactly like a fix
    // that changed nothing. `run.sh` warns and boots anyway; so does this.
    for artifact in [utils::kernel_path(profile)?, utils::loader_path(profile)?] {
        if utils::newer(&artifact, &image)? {
            eprintln!(
                "run: warning: {} predates {}",
                image.display(),
                artifact.display()
            );
            eprintln!("run:          it boots the code from before that build, which will");
            eprintln!("run:          look like a fix that changed nothing. Rebuild first:");
            eprintln!("run:            cargo xtask all");
            eprintln!();
        }
    }

    // The firmware two halves are separate files, and the variables half is
    // copied fresh on every run rather than written to in place: the firmware
    // writes to it, and this machine's variables are not worth keeping - a disk
    // that has never booted has no boot entry written down, which is exactly the
    // state the loader is built for. The 4 MiB build is the one that matters:
    // the smaller 2 MiB one cannot map the kernel and the initramfs at the
    // addresses the loader asks for.
    let ovmf_dir = find_ovmf()?;
    let vars = utils::ovmf_vars_path(build_dir);
    fs::create_dir_all(build_dir)?;
    let src = ovmf_dir.join("OVMF_VARS.4m.fd");
    fs::copy(&src, &vars).with_context(|| {
        format!(
            "cannot write {}; {} is not writable",
            vars.display(),
            build_dir.display()
        )
    })?;

    let accel = detect_accel();
    let mem = env::var("MEM").unwrap_or_else(|_| "1024".to_string());

    let args: Vec<String> = vec![
        "-machine".into(),
        "q35".into(),
        "-accel".into(),
        accel,
        "-m".into(),
        mem,
        "-drive".into(),
        format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            ovmf_dir.join("OVMF_CODE.4m.fd").display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=hd0,file.locking=off",
            image.display()
        ),
        "-device".into(),
        "virtio-blk-pci,drive=hd0".into(),
        "-netdev".into(),
        "user,id=n0,hostname=mpos".into(),
        "-device".into(),
        "virtio-net-pci,netdev=n0".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-serial".into(),
        "stdio".into(),
        "-no-reboot".into(),
    ];

    // Printed because a boot that goes wrong is usually read off this line.
    eprintln!("+ qemu-system-x86_64 {}", args.join(" "));
    let err = Command::new("qemu-system-x86_64").args(&args).exec();
    anyhow::bail!("Failed to start QEMU: {err}");
}

pub fn find_ovmf() -> Result<PathBuf> {
    let dirs = [
        "/usr/share/OVMF/x64",
        "/usr/share/edk2/x64",
        "/usr/share/OVMF",
        "/usr/share/edk2-ovmf/x64",
    ];

    for dir in &dirs {
        let path = Path::new(dir);
        if path.join("OVMF_CODE.4m.fd").exists() && path.join("OVMF_VARS.4m.fd").exists() {
            return Ok(path.to_path_buf());
        }
    }

    anyhow::bail!(
        "No OVMF firmware found; looked in /usr/share/OVMF/x64, /usr/share/edk2/x64, /usr/share/OVMF and /usr/share/edk2-ovmf/x64 for OVMF_CODE.4m.fd"
    )
}

/// The accelerator. `ACCEL` from the environment wins over the detection, the
/// way `run.sh` reads it: KVM makes this usable, but it needs `/dev/kvm`, which
/// a container usually does not have.
pub fn detect_accel() -> String {
    if let Ok(accel) = env::var("ACCEL")
        && !accel.is_empty()
    {
        return accel;
    }

    let kvm = CString::new("/dev/kvm").expect("the path has no interior NUL");
    if unsafe { libc::access(kvm.as_ptr(), libc::W_OK) } == 0 {
        return "kvm".to_string();
    }

    "tcg".to_string()
}
