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

use crate::config::QemuConfig;
use crate::utils;

/// What the machine boots from, which is the one thing an image run and a user
/// test run do differently.
pub enum Boot {
    /// The assembled image, as a virtio disk the guest can write.
    Image(PathBuf),
    /// A directory exported over QEMU's virtual FAT, read-only - what
    /// `xtask test` boots when there is no image and no root filesystem, just
    /// an ESP full of test programs.
    Esp(PathBuf),
}

/// Where the guest's serial line goes.
pub enum Console {
    /// The terminal this command was run from.
    Stdio,
    /// A file, which is what a test run reads its verdict out of.
    File(String),
}

/// The QEMU command line both `run` and `test` boot with.
///
/// One builder, because the two differed only in what they put on the disk and
/// where the serial line went - and the rest (the firmware, the machine type,
/// the network) drifting apart between them is a difference nobody asked for.
/// The sizes and the name come from `[qemu]` in `rootfs.toml`.
pub fn machine_args(
    config: &QemuConfig,
    ovmf_dir: &Path,
    vars: &Path,
    boot: Boot,
    console: Console,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-machine".into(),
        "q35".into(),
        "-accel".into(),
        detect_accel(),
        "-m".into(),
        config.memory.clone(),
    ];

    // A second processor is worth saying only when there is one: `-smp 1` is
    // QEMU's default and the setting the kernel's bring-up code reads as
    // "one processor, nothing to start".
    if config.cpus > 1 {
        args.push("-smp".into());
        args.push(config.cpus.to_string());
    }

    args.extend([
        "-drive".into(),
        format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            ovmf_dir.join("OVMF_CODE.4m.fd").display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
    ]);

    match boot {
        Boot::Image(image) => args.extend([
            "-drive".into(),
            format!(
                "file={},format=raw,if=none,id=hd0,file.locking=off",
                image.display()
            ),
            "-device".into(),
            "virtio-blk-pci,drive=hd0".into(),
        ]),
        // `readonly=on` is not decoration: QEMU's virtual FAT cannot be
        // written, and a virtio-blk device refuses a backend it was not told
        // is read-only (`Block node is read-only`) - the machine never
        // starts, and every test reports as failed. Say so up front and the
        // disk is a perfectly good ESP: the loader only ever reads it.
        Boot::Esp(esp) => args.extend([
            "-drive".into(),
            format!(
                "file=fat:{},format=raw,if=none,id=hd0,readonly=on",
                esp.display()
            ),
            "-device".into(),
            "virtio-blk-pci,drive=hd0".into(),
        ]),
    }

    // The name the guest's DHCP client reports, and the network the socket
    // tests are written against: `/init` brings eth0 up with the address slirp
    // expects, and `t-socket`, `t-tcp` and `t-udp` then talk to the host
    // through it. A configured `dns` names the server behind `10.0.2.3`;
    // without it slirp follows the host's own resolver file.
    let mut netdev = format!("user,id=n0,hostname={}", config.hostname);
    if let Some(dns) = config.dns.as_deref() {
        netdev.push_str(&format!(",dns={dns}"));
    }
    args.extend([
        "-netdev".into(),
        netdev,
        "-device".into(),
        "virtio-net-pci,netdev=n0".into(),
    ]);

    match console {
        Console::Stdio => args.extend([
            "-display".into(),
            "none".into(),
            "-serial".into(),
            "mon:stdio".into(),
        ]),
        Console::File(log) => args.extend([
            "-display".into(),
            "none".into(),
            "-serial".into(),
            format!("file:{log}"),
        ]),
    }

    args.extend(["-monitor".into(), "none".into(), "-no-reboot".into()]);
    args.extend(config.extra_args.iter().cloned());
    args
}

/// True when the image does not reflect the sources: missing, never assembled,
/// or assembled from a kernel, a loader, an initramfs or an overlay that has
/// changed since.
///
/// `run` asks this before it decides anything. A kernel rebuilt after the last
/// image has to trigger the rebuild - otherwise the boot is of code that is no
/// longer on disk, and it reads exactly like a fix that changed nothing.
pub fn needs_build(build_dir: &Path, profile: &str) -> Result<bool> {
    utils::image_stale(build_dir, profile)
}

pub fn run_qemu(
    build_dir: &Path,
    profile: &str,
    artefacts_ready: bool,
    needs_build: bool,
) -> Result<()> {
    if needs_build {
        println!("run: the image is missing or out of date; building it");
        if !artefacts_ready {
            utils::build_kernel(profile)?;
        }
        crate::disk::ensure(build_dir, profile)?;
        // The rootfs step brings the initramfs up to date itself, so there is
        // no separate call here to report it twice.
        crate::rootfs::ensure(build_dir, profile)?;
        // When `artefacts_ready` is set this is the sudo half, and its parent
        // is waiting to boot what was just built. Only a `run` that is the
        // whole command has to be told to come back.
        if !artefacts_ready {
            println!();
            println!("run: the image is built; run this command again to boot it");
        }
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

    // The machine's own settings: memory, processors, the name the guest
    // reports, and whatever `extra_args` adds.
    let config = QemuConfig::load(&utils::project_root()?.join("rootfs.toml"))?;
    let args = machine_args(
        &config,
        &ovmf_dir,
        &vars,
        Boot::Image(image.clone()),
        Console::Stdio,
    );

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QemuConfig;

    fn config(memory: &str, cpus: u32, extra: &[&str]) -> QemuConfig {
        QemuConfig {
            memory: memory.to_string(),
            cpus,
            hostname: "probe".to_string(),
            dns: None,
            extra_args: extra.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    fn args(config: &QemuConfig, boot: Boot) -> Vec<String> {
        machine_args(
            config,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            boot,
            Console::Stdio,
        )
    }

    /// The argument after `flag`, which is how every assertion here reads the
    /// line - the order of unrelated options is QEMU's business, not this
    /// crate's.
    fn after(args: &[String], flag: &str) -> Option<String> {
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).cloned()
    }

    #[test]
    fn the_machine_line_carries_the_configured_memory_and_processors() {
        let config = config("2048", 4, &[]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(after(&args, "-m").as_deref(), Some("2048"));
        assert_eq!(after(&args, "-smp").as_deref(), Some("4"));
        assert_eq!(after(&args, "-machine").as_deref(), Some("q35"));
    }

    #[test]
    fn one_processor_is_left_to_qemu() {
        // `-smp 1` is the default, and the kernel's own bring-up reads the
        // absence of a second processor out of the same place either way.
        let config = config("1024", 1, &[]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert!(!args.iter().any(|arg| arg == "-smp"), "{args:?}");
    }

    #[test]
    fn an_image_boot_is_a_writable_disk_and_an_esp_boot_is_read_only() {
        let config = config("1024", 1, &[]);

        let image = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        let disk = image
            .iter()
            .find(|arg| arg.contains("mpos.img"))
            .expect("the image is on the command line");
        assert!(disk.contains("file.locking=off"), "{disk}");
        assert!(!disk.contains("readonly"), "the guest writes to it: {disk}");

        let esp = args(&config, Boot::Esp(PathBuf::from("/build/test-esp")));
        let disk = esp
            .iter()
            .find(|arg| arg.contains("test-esp"))
            .expect("the ESP is on the command line");
        assert!(
            disk.contains("readonly=on"),
            "virtual FAT cannot be written, and a device that was not told so never starts: {disk}"
        );
    }

    #[test]
    fn the_guest_is_named_and_extra_arguments_come_last() {
        let config = config("1024", 1, &["-enable-kvm"]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));

        let netdev = args
            .iter()
            .find(|arg| arg.starts_with("user,id=n0"))
            .expect("the network is on the command line");
        assert!(netdev.contains("hostname=probe"), "{netdev}");
        // No `dns` configured, no `dns=` on the line: slirp follows the host.
        assert!(!netdev.contains("dns="), "{netdev}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("-enable-kvm"),
            "the escape hatch is appended after everything computed: {args:?}"
        );
    }

    #[test]
    fn a_configured_dns_names_the_nameserver_behind_10_0_2_3() {
        let mut config = config("1024", 1, &[]);
        config.dns = Some("172.16.0.1".to_string());
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        let netdev = args
            .iter()
            .find(|arg| arg.starts_with("user,id=n0"))
            .expect("the network is on the command line");
        assert!(netdev.contains("dns=172.16.0.1"), "{netdev}");
    }
}
