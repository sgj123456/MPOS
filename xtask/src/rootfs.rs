//! The `rootfs` command: install an Alpine root filesystem into the disk image
//! `disk` created, alongside the kernel, the loader and the initramfs this
//! repository just built. This is the assembly half of `scripts/mkdisk.sh`.
//!
//! It needs the disk image to exist, not to be empty: everything in it is
//! written here, so re-running after a change is how the image is updated.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::RootfsConfig;
use crate::utils;

pub fn build_rootfs(build_dir: &Path, profile: &str) -> Result<()> {
    let root = utils::project_root()?;
    let config_path = root.join("rootfs.toml");
    let config = RootfsConfig::load(&config_path)?;

    let image = utils::image_path(build_dir);
    if !image.exists() {
        anyhow::bail!(
            "The disk image does not exist at {}; run `cargo xtask disk` first",
            image.display()
        );
    }

    // The initramfs is one of the inputs the staleness check below reads, and it
    // is brought up to date here rather than assumed: a stale archive carries an
    // old mtime, which would read as "the rootfs is newer than this" and skip
    // the rebuild the new archive needs.
    crate::initramfs::ensure(build_dir, profile)?;

    // The image's own mtime cannot say whether a root filesystem was installed
    // into it: `disk` creating the file makes it newer than the configuration,
    // which would read as "built after the last change" and skip the install.
    // The stamp is written only by a run that got all the way through.
    let stamp = utils::rootfs_stamp(build_dir);
    if stamp.exists() {
        let built = utils::file_mtime(&stamp)?;
        let mut changed = false;
        for path in [
            config_path.clone(),
            utils::kernel_path(profile)?,
            utils::loader_path(profile)?,
            utils::initramfs_path(build_dir),
            image.clone(),
        ] {
            if utils::newer(&path, &stamp)? {
                changed = true;
            }
        }
        if !changed && !utils::dir_newer_than(&root.join("rootfs"), built)? {
            println!("rootfs: up to date");
            return Ok(());
        }
    }

    println!("Building the rootfs");

    let kernel = utils::kernel_path(profile)?;
    let loader = utils::loader_path(profile)?;
    let initramfs = utils::initramfs_path(build_dir);
    if !kernel.exists() {
        anyhow::bail!(
            "No kernel at {}; run `cargo xtask build` first",
            kernel.display()
        );
    }
    if !loader.exists() {
        anyhow::bail!(
            "No EFI loader at {}; run `cargo xtask build` first",
            loader.display()
        );
    }
    if !initramfs.exists() {
        anyhow::bail!(
            "No initramfs at {}; run `cargo xtask initramfs` first",
            initramfs.display()
        );
    }

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;

    let mount_dir = build_dir.join("mnt");
    fs::create_dir_all(build_dir)?;
    if mount_dir.exists() {
        utils::umount_recursively(&mount_dir);
        let _ = fs::remove_dir_all(&mount_dir);
    }
    fs::create_dir_all(&mount_dir)?;

    let disk_dev = attach_image(&image)?;
    // From here on the image is attached and may be mounted: the guard detaches
    // it however this function leaves, errors included.
    let _guard = ImageGuard::new(disk_dev.clone(), mount_dir.clone());

    create_gpt(&disk_dev)?;

    let esp_dev = format!("{disk_dev}p1");
    let root_dev = format!("{disk_dev}p2");

    // The partition nodes are asked for rather than waited for: the nbd node
    // may have been made by hand a moment ago, and a node udev never saw appear
    // is one it has no event to make partitions for.
    let _ = Command::new("partx").args(["-a", &disk_dev]).status();
    for _ in 0..10 {
        if Path::new(&esp_dev).exists() && Path::new(&root_dev).exists() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        let _ = Command::new("partx").args(["-a", &disk_dev]).status();
    }
    if !Path::new(&esp_dev).exists() || !Path::new(&root_dev).exists() {
        anyhow::bail!("The kernel did not publish {esp_dev} and {root_dev}");
    }

    // The ESP gets FAT32, and the root filesystem classic ext2: this kernel's
    // driver reads that layout, and `-E nodiscard` because these blocks are a
    // sparse file and discarding them is wasted work.
    utils::run_status(
        Command::new("mkfs.fat")
            .args(["-F32", "-n", "EFI"])
            .arg(&esp_dev),
        "mkfs.fat",
    )?;
    utils::run_status(
        Command::new("mkfs.ext2")
            .args(["-L", "root", "-O", "^64bit", "-E", "nodiscard"])
            .arg(&root_dev),
        "mkfs.ext2",
    )?;

    let root_uuid = blk_uuid(&root_dev)?;

    utils::run_status(
        Command::new("mount").arg(&root_dev).arg(&mount_dir),
        "mount the root filesystem",
    )?;
    fs::create_dir_all(mount_dir.join("boot"))?;
    utils::set_mode(&mount_dir.join("boot"), 0o000)?;
    utils::run_status(
        Command::new("mount")
            .args(["-t", "vfat"])
            .arg(&esp_dev)
            .arg(mount_dir.join("boot")),
        "mount the ESP",
    )?;

    // The base system, installed into the mounted root the way `mkdisk.sh`
    // does: repositories, keys, then apk.
    fs::create_dir_all(mount_dir.join("etc/apk/keys"))?;
    let repositories = format!(
        "{}/{}/main\n{}/{}/community\n",
        config.mirror, config.branch, config.mirror, config.branch
    );
    fs::write(mount_dir.join("etc/apk/repositories"), repositories)?;
    utils::install_apk_keys(&mount_dir.join("etc/apk/keys"))?;

    let status = Command::new(&apk)
        .args(["--no-progress", "add"])
        .arg("--root")
        .arg(&mount_dir)
        .args(["--arch", &config.arch, "--update-cache", "--initdb"])
        .args(&config.base_packages)
        .status()
        .context("Failed to install the Alpine base system")?;
    if !status.success() {
        anyhow::bail!("Failed to install the Alpine base system");
    }

    prepare_chroot(&mount_dir)?;

    // The three files Alpine's `linux-virt` and `mkinitfs` would put on the ESP,
    // all of them built out of this repository a moment ago. `BOOTX64.EFI` under
    // EFI/BOOT is where a UEFI firmware falls back when nothing has written a
    // boot entry into its non-volatile memory.
    fs::create_dir_all(mount_dir.join("boot/EFI/BOOT"))?;
    install_file(&loader, &mount_dir.join("boot/EFI/BOOT/BOOTX64.EFI"))?;
    install_file(&kernel, &mount_dir.join("boot/mpos.elf"))?;
    install_file(&initramfs, &mount_dir.join("boot/initramfs.tar"))?;
    fs::write(
        mount_dir.join("boot/mpos.cmdline"),
        format!(
            "root={} rootflags=rw console={}\n",
            config.root_device, config.serial_port
        ),
    )?;

    write_system_config(&mount_dir, &config, &root_uuid)?;

    if !config.extra_packages.is_empty() {
        println!("Installing additional packages");
        let status = Command::new(&apk)
            .args(["--no-progress", "add"])
            .arg("--root")
            .arg(&mount_dir)
            .args(&config.extra_packages)
            .status()
            .context("Failed to install the additional packages")?;
        if !status.success() {
            anyhow::bail!("Failed to install the additional packages");
        }
    }

    utils::copy_dir_recursive(&root.join("rootfs"), &mount_dir)?;

    fs::write(&stamp, format!("image: {}\n", image.display()))?;
    println!("rootfs: completed");
    Ok(())
}

/// Builds the root filesystem unless the stamp says the image already holds the
/// current one.
pub fn ensure(build_dir: &Path, profile: &str) -> Result<()> {
    build_rootfs(build_dir, profile)
}

/// The files that make the guest usable from a serial console: fstab, the
/// network, an inittab with a getty on the serial port, an unlocked root, and
/// the resolver QEMU's user network answers on.
fn write_system_config(mount_dir: &Path, config: &RootfsConfig, root_uuid: &str) -> Result<()> {
    // The ESP is where the firmware reads from and nothing else mounts it, so it
    // gets no fstab entry: that would only be a mount that can fail.
    let fstab = format!(
        "# <fs>\t\t<mountpoint>\t<type>\t<opts>\t\t<dump/pass>\nUUID={root_uuid}\t/\t\t{}\trelatime\t0 1\n",
        config.filesystem
    );
    fs::write(mount_dir.join("etc/fstab"), fstab)?;

    let interfaces = "auto eth0\niface eth0 inet dhcp\n\npost-up /etc/network/if-post-up.d/*\npost-down /etc/network/if-post-down.d/*\n";
    fs::write(mount_dir.join("etc/network/interfaces"), interfaces)?;
    fs::create_dir_all(mount_dir.join("etc/network/if-post-up.d"))?;
    fs::create_dir_all(mount_dir.join("etc/network/if-post-down.d"))?;

    // The respawn entries name the virtual consoles `tty1`..`tty4`, which are
    // real pty-backed terminals here - the kernel's console multiplexer
    // carries their traffic over `ttyS0`, and `ttyS0` itself carries only the
    // kernel's log. Root ships password-locked; one line with an empty
    // password field is what lets root log in with nothing to type.
    let gettys = ["tty1", "tty2", "tty3", "tty4"]
        .map(|tty| format!("{tty}::respawn:/sbin/getty 38400 {tty}\n"))
        .concat();
    let inittab = format!(
        "# /etc/inittab - mpos: shells on the virtual consoles, kernel log on ttyS0\n\n::sysinit:/sbin/openrc sysinit\n::sysinit:/sbin/openrc boot\n::wait:/sbin/openrc default\n\n# A getty on each virtual console; ^A 1..4 switches the serial mux between them\n{gettys}\n# Stuff to do for the 3-finger salute\n::ctrlaltdel:/sbin/reboot\n\n# Stuff to do before rebooting\n::shutdown:/sbin/openrc shutdown\n"
    );
    fs::write(mount_dir.join("etc/inittab"), inittab)?;

    let shadow_path = mount_dir.join("etc/shadow");
    if let Ok(shadow) = fs::read_to_string(&shadow_path) {
        let unlocked: Vec<String> = shadow
            .lines()
            .map(|line| match line.strip_prefix("root:") {
                Some(rest) => match rest.split_once(':') {
                    Some((_, after)) => format!("root::{after}"),
                    None => line.to_string(),
                },
                None => line.to_string(),
            })
            .collect();
        fs::write(&shadow_path, unlocked.join("\n") + "\n")?;
    }
    // /etc/securetty lists tty names this kernel's ttyname(2) cannot answer
    // yet, and login would refuse root while calling it a wrong password.
    let _ = fs::remove_file(mount_dir.join("etc/securetty"));

    // OpenRC refuses to start a service whose script is not executable and
    // answers 0, so the failure is silent: the `root` service never runs and
    // every write on the root filesystem answers EROFS.
    if let Ok(entries) = fs::read_dir(mount_dir.join("etc/init.d")) {
        for entry in entries.flatten() {
            let _ = utils::set_mode(&entry.path(), 0o755);
        }
    }

    let resolv_conf = "# QEMU user networking's built-in forwarder.\nnameserver 10.0.2.3\n";
    fs::write(mount_dir.join("etc/resolv.conf"), resolv_conf)?;

    fs::create_dir_all(mount_dir.join("mnt"))?;
    Ok(())
}

fn install_file(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst)
        .with_context(|| format!("Failed to install {} to {}", src.display(), dst.display()))?;
    utils::set_mode(dst, 0o644)
}

/// Detaches the image however the assembly ends. A failed build that leaves the
/// image attached and mounted is one the next run trips over, and the failure
/// that caused it is already the interesting one.
struct ImageGuard {
    disk_dev: String,
    mount_dir: PathBuf,
}

impl ImageGuard {
    fn new(disk_dev: String, mount_dir: PathBuf) -> Self {
        Self {
            disk_dev,
            mount_dir,
        }
    }
}

impl Drop for ImageGuard {
    fn drop(&mut self) {
        utils::umount_recursively(&self.mount_dir);
        // The partition nodes first: `qemu-nbd --disconnect` removes the whole
        // device, and a partition node left pointing at a deleted parent is a
        // stale entry the next run counts as a device in use.
        let _ = Command::new("partx").args(["-d", &self.disk_dev]).status();
        let _ = Command::new("qemu-nbd")
            .arg("--disconnect")
            .arg(&self.disk_dev)
            .status();
        let _ = fs::remove_dir_all(&self.mount_dir);
    }
}

/// Attaches the image as an NBD block device and returns its path.
fn attach_image(image: &Path) -> Result<String> {
    let disk_dev = match get_available_nbd() {
        Ok(dev) => dev,
        Err(_) => {
            // The nodes may be missing because the module was never loaded.
            let _ = Command::new("modprobe")
                .args(["nbd", "max_part=16"])
                .status();
            std::thread::sleep(Duration::from_secs(1));
            get_available_nbd()?
        }
    };

    // `--format=raw` rather than letting qemu-nbd probe: without it it refuses
    // to write the first 512 bytes of a raw image, and the first 512 bytes are
    // where sfdisk puts the GPT header.
    let status = Command::new("qemu-nbd")
        .arg(format!("--connect={disk_dev}"))
        .args(["--cache=writeback", "--format=raw"])
        .arg(image)
        .status()
        .context("Failed to run qemu-nbd")?;
    if !status.success() {
        anyhow::bail!("Could not attach the image");
    }

    std::thread::sleep(Duration::from_secs(1));
    Ok(disk_dev)
}

/// Returns an unconnected `/dev/nbd*`, creating the node if the module did not
/// create it - `modprobe nbd` only registers the block driver and populates no
/// `/dev` entry on a machine without `nbd-client`.
fn get_available_nbd() -> Result<String> {
    let devices = fs::read_to_string("/proc/devices")?;
    let major = devices
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let number = fields.next()?;
            let name = fields.next()?;
            (name == "nbd").then(|| number.parse::<u32>().ok())?
        })
        .context("nbd is not registered in /proc/devices")?;

    for minor in 0..16u32 {
        let dev = format!("/dev/nbd{minor}");

        if !Path::new(&dev).exists() {
            let _ = Command::new("mknod")
                .args([&dev, "b", &major.to_string(), &minor.to_string()])
                .status();
            if !Path::new(&dev).exists() {
                continue;
            }
        }

        // An unconnected device reports size 0; a connected one does not.
        let size = Command::new("blockdev")
            .args(["--getsize64", &dev])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0);

        if size == 0 {
            return Ok(dev);
        }
    }

    anyhow::bail!("No available nbd device found")
}

/// Creates the GPT partition table: an EFI System Partition and a system one.
fn create_gpt(disk_dev: &str) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("sfdisk")
        .arg(disk_dev)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to run sfdisk")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(b"label: gpt\nname=efi,type=U,size=128M,bootable\nname=system,type=L\n")
            .context("Failed to write the partition table to sfdisk")?;
    }

    let status = child.wait().context("Failed to wait for sfdisk")?;
    if !status.success() {
        anyhow::bail!("sfdisk could not write the partition table");
    }
    Ok(())
}

/// The UUID of the filesystem on a block device, which is what fstab names the
/// root by: `/dev/vda2` is how the guest sees it, and the UUID does not depend
/// on that.
fn blk_uuid(dev: &str) -> Result<String> {
    let output = Command::new("blkid")
        .arg(dev)
        .output()
        .context("Failed to run blkid")?;
    let text = String::from_utf8_lossy(&output.stdout);

    text.split_whitespace()
        .find_map(|token| {
            token
                .strip_prefix("UUID=\"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .map(str::to_string)
        .context("Failed to read the root UUID from blkid")
}

/// Mounts /proc, /dev and /sys into a tree so apk's scripts and any chroot see
/// a machine rather than a directory.
fn prepare_chroot(dest: &Path) -> Result<()> {
    let proc_dir = dest.join("proc");
    fs::create_dir_all(&proc_dir)?;
    utils::run_status(
        Command::new("mount")
            .args(["-t", "proc", "none"])
            .arg(&proc_dir),
        "mount proc in the staged root",
    )?;
    mount_bind(Path::new("/dev"), &dest.join("dev"))?;
    mount_bind(Path::new("/sys"), &dest.join("sys"))?;

    let resolv_conf = Path::new("/etc/resolv.conf");
    if resolv_conf.exists() {
        fs::create_dir_all(dest.join("etc"))?;
        fs::copy(resolv_conf, dest.join("etc/resolv.conf"))?;
    }
    Ok(())
}

fn mount_bind(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    utils::run_status(
        Command::new("mount").arg("--bind").arg(src).arg(dst),
        "bind mount",
    )?;
    utils::run_status(
        Command::new("mount").arg("--make-private").arg(dst),
        "make the bind mount private",
    )?;
    Ok(())
}
