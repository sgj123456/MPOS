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

use crate::arch::Arch;
use crate::config::{QemuConfig, RootfsConfig, RunMode, UserConfig};
use crate::utils;

/// The mode to bake into the image: an explicit `MPOS_XTASK_MODE`
/// (what `cargo xtask run --mode X` hands the privileged build) wins over
/// `qemu.mode` in `machine.toml`, which itself defaults to mirror.
///
/// Reading it from the machine's own file is what makes a mode change cheap:
/// the key is not in `rootfs.toml`, so it cannot mark the image stale, and the
/// only thing a new mode costs is the boot-file refresh below.
///
/// A word neither source recognises is an error rather than the default, in both
/// halves: this is the step that writes `mpos.cmdline`, so a silent fallback would
/// put consoles the user never asked for inside the image.
///
/// `pub(crate)` because the second machine's interactive boot asks the same
/// question of the same sources (`aarch64::alpine_cmdline`): a session is a
/// session on either machine, and `--mode` means one answer across both.
pub(crate) fn effective_mode() -> Result<RunMode> {
    if let Ok(text) = std::env::var("MPOS_XTASK_MODE") {
        return RunMode::parse_strict(&text);
    }
    // The standing mode is the file's plain key: `mode` is a word about *this*
    // image, whose command line this build writes. A machine's own override is
    // read where the machine is known (`QemuConfig::load_default(arch)`), and a
    // session on the second machine reads it back through `MPOS_XTASK_MODE`,
    // which `main` sets from its own view of the file before this is reached.
    QemuConfig::load_default(crate::arch::Arch::X86_64)?.run_mode()
}

pub fn build_rootfs(build_dir: &Path, profile: &str) -> Result<()> {
    let root = utils::project_root()?;
    let config_path = root.join("rootfs.toml");
    let config = RootfsConfig::load(&config_path, Arch::X86_64)?;

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
    crate::initramfs::ensure(build_dir, crate::arch::Arch::X86_64)?;

    // The mode to bake. It is a boot-file change, not a package change, so it
    // forces the (fast) boot-file refresh on its own even when nothing else
    // moved - that is how `cargo xtask run --mode X` re-points an existing image
    // without reinstalling Alpine.
    let mode = effective_mode()?;

    if !needs_rebuild(build_dir, profile)? && crate::qemu::baked_mode(build_dir) == Some(mode) {
        println!("rootfs: up to date");
        return Ok(());
    }

    let kernel = utils::kernel_path(profile, Arch::X86_64)?;
    let loader = utils::loader_path(profile, Arch::X86_64)?;
    let initramfs = utils::initramfs_path(build_dir);
    for (what, path, command) in [
        ("kernel", &kernel, "build"),
        ("EFI loader", &loader, "build"),
        ("initramfs", &initramfs, "initramfs"),
    ] {
        if !path.exists() {
            anyhow::bail!(
                "No {what} at {}; run `cargo xtask {command}` first",
                path.display()
            );
        }
    }

    // A kernel or an initramfs that changed is two files on the ESP and nothing
    // else: the Alpine tree inside the image is untouched. Reinstalling
    // thirty-odd packages to copy two files is how a one-line kernel change
    // turns into minutes, so the base install runs only when the thing that
    // decides what is installed - the configuration - has changed, or when
    // there is no image to update.
    let full_install = needs_full_install(build_dir)?;
    println!(
        "{}",
        if full_install {
            "Building the rootfs (base system)"
        } else {
            "Updating the image (boot files only)"
        }
    );

    // A run that was killed left the image attached and mounted, and the next
    // one would meet a device in use. Clear that before attaching again.
    clear_stale_state(build_dir);

    let mount_dir = build_dir.join("mnt");
    fs::create_dir_all(build_dir)?;
    fs::create_dir_all(&mount_dir)?;

    let disk_dev = attach_image(&image, build_dir)?;
    // From here on the image is attached and may be mounted: the guard detaches
    // it however this function leaves, errors included.
    let _guard = ImageGuard::new(
        disk_dev.clone(),
        mount_dir.clone(),
        build_dir.join(NBD_LEASE),
    );

    if full_install {
        create_gpt(&disk_dev, &config.esp_size)?;
    }
    let (esp_dev, root_dev) = partitions(&disk_dev)?;

    if full_install {
        // The ESP gets FAT32, and the root filesystem is whatever the config
        // says - ext4 today, mounted by the kernel's ext2/ext4 driver. Two
        // features are refused, because the driver cannot honour either:
        // `has_journal`, since a crash then leaves RECOVER set and nothing
        // here replays a journal - the image would come up read-only until a
        // Linux fsck ran - and `metadata_csum`, since every write would leave
        // behind a checksum this kernel cannot recompute. `64bit` stays on:
        // the driver reads the high halves now, and a 2 GiB image exercises
        // them for free. `-F` because the image still carries the filesystem
        // the last run put there, and `mke2fs` otherwise stops to ask about
        // overwriting it; `-E nodiscard` because these blocks are a sparse
        // file and discarding them is wasted work.
        utils::run_status(
            Command::new("mkfs.fat")
                .args(["-F32", "-n", "EFI"])
                .arg(&esp_dev),
            "mkfs.fat",
        )?;
        utils::run_status(
            Command::new(format!("mkfs.{}", config.filesystem))
                .args(["-F", "-L", "root"])
                .args(if config.mkfs_options.is_empty() {
                    Vec::new()
                } else {
                    vec!["-O", &config.mkfs_options]
                })
                .args(["-E", "nodiscard"])
                .arg(&root_dev),
            "mkfs the root filesystem",
        )?;
    }

    utils::run_status(
        Command::new("mount").arg(&root_dev).arg(&mount_dir),
        "mount the root filesystem",
    )?;
    fs::create_dir_all(mount_dir.join("boot"))?;
    utils::run_status(
        Command::new("mount")
            .args(["-t", "vfat"])
            .arg(&esp_dev)
            .arg(mount_dir.join("boot")),
        "mount the ESP",
    )?;

    if full_install {
        install_base(build_dir, &mount_dir, &config, &root_dev)?;
    }

    install_boot_files(&mount_dir, &config, &kernel, &loader, &initramfs, mode)?;

    // The machine's own configuration, then the overlay. Both run on every
    // build path - a change to the hostname or to a file under `rootfs/` should
    // take effect on the next boot, not on the next reinstall - and the overlay
    // goes last, so a file there is the authority over anything generated. The
    // resolver is the one file that may still change after it.
    write_system_config(&mount_dir, &config)?;
    // The runlevels and the module index are re-asserted on *every* build,
    // not only on a full install - they are the build's policy, and policy
    // that only lands on a reinstall quietly rots (a `rc-update add` inside
    // a running guest otherwise survives every later build). Both run
    // before the overlay, so `rootfs/` stays the authoritative last word on
    // any name either writes.
    enable_services(&mount_dir)?;
    write_modules(&mount_dir)?;
    // The overlay pair, and the inittab rendered with it: `rootfs/` stays the
    // authoritative last word on any name either writes, and the one file that
    // lands as a template is resolved before anything reads it.
    merge_overlays(&root, "rootfs", &mount_dir, &config, false)?;
    write_resolver(&mount_dir, &config)?;

    // Detached before the stamp, not after: the nbd server can still be
    // flushing the last writes when the last file is copied, and the image's
    // mtime bumps when it does. Letting go of the image first makes the file
    // quiet, so the stamp that goes down next is strictly after everything the
    // build wrote.
    drop(_guard);
    fs::write(
        utils::rootfs_stamp(build_dir),
        format!("image: {}\n", image.display()),
    )?;
    // Record the mode now baked into `mpos.cmdline`, so the next `run`
    // can tell cheaply whether the image already says what the user asked for.
    crate::qemu::set_baked_mode(build_dir, mode)?;
    println!("rootfs: completed");
    Ok(())
}

/// Builds the root filesystem unless the stamp says the image already holds the
/// current one.
pub fn ensure(build_dir: &Path, profile: &str) -> Result<()> {
    build_rootfs(build_dir, profile)
}

/// True when the image does not hold the current root filesystem: it is
/// missing, it was never assembled, or anything it was assembled from has
/// changed since the stamp was written.
///
/// The stamp is what makes this answerable at all. The image's own mtime
/// cannot: `disk` creating the file makes it newer than the configuration,
/// which reads as "built after the last change" and skips the install
/// entirely - which is how an image ends up with no root filesystem in it and
/// still looks newer than everything it was supposed to contain. An image that
/// is simply *missing* is answered above, so the gap the old wording was
/// guarding is closed without the image in the list.
///
/// It is also why the image must not be compared against the stamp at all:
/// the image is the output of this build, not one of its inputs, and a write
/// that lands after the stamp reads as "the image changed after the build" for
/// ever. `qemu-nbd` does exactly that - it flushes when it detaches, which is
/// after the last file is copied, and the image's mtime bumps tens of
/// milliseconds after the stamp. Every run then rebuilds, and `run` bails with
/// "still out of date" having just built successfully.
/// The files the stamp is a record of: everything the rootfs is built *from*,
/// and nothing it is built *into*.
///
/// The image is what [`needs_rebuild`] leaves out, on purpose.
fn stamp_inputs(root: &Path, build_dir: &Path, profile: &str) -> Result<Vec<PathBuf>> {
    Ok(vec![
        root.join("rootfs.toml"),
        utils::kernel_path(profile, Arch::X86_64)?,
        utils::loader_path(profile, Arch::X86_64)?,
        utils::initramfs_path(build_dir),
    ])
}

pub fn needs_rebuild(build_dir: &Path, profile: &str) -> Result<bool> {
    let root = utils::project_root()?;
    let image = utils::image_path(build_dir);
    let stamp = utils::rootfs_stamp(build_dir);
    if !image.exists() || !stamp.exists() {
        return Ok(true);
    }

    for input in stamp_inputs(&root, build_dir, profile)? {
        if utils::newer(&input, &stamp)? {
            return Ok(true);
        }
    }

    // The overlay is two trees rather than one - the shared core and this
    // machine's own - and a tree is not a file: a file added under either is an
    // input the stamp knows nothing about until it is written, so the trees'
    // own contents are what say they changed.
    let stamp_time = utils::file_mtime(&stamp)?;
    Ok(utils::dir_newer_than(&root.join(CORE_OVERLAY), stamp_time)?
        || utils::dir_newer_than(&root.join("rootfs"), stamp_time)?)
}

/// `/etc/fstab`: the root filesystem by UUID, which is the one name for it
/// that does not depend on how the kernel enumerated the disk.
///
/// The ESP gets no entry: the firmware reads from it and nothing else mounts
/// it, so an entry would only be a mount that can fail.
///
/// Written only on a full install, because what it says is decided by
/// `mke2fs`/`mkfs.fat` and the UUID they handed out - neither of which a
/// boot-files update touches.
fn write_fstab(mount_dir: &Path, config: &RootfsConfig, root_uuid: &str) -> Result<()> {
    let fstab = format!(
        "# <fs>\t\t<mountpoint>\t<type>\t<opts>\t\t<dump/pass>\nUUID={root_uuid}\t/\t\t{}\trelatime\t0 1\n",
        config.filesystem
    );
    fs::write(mount_dir.join("etc/fstab"), fstab)?;
    Ok(())
}

/// The machine's own configuration: the shared half (below), plus the two
/// steps that need the image's own userland to be *runnable*.
///
/// This runs on both build paths - the full install and the boot-files update -
/// because none of it needs `apk`, all of it is cheap, and a change to the
/// configuration should take effect on the next boot rather than on the next
/// reinstall. Everything whose content is a constant moved to the `rootfs/`
/// overlay instead (`inittab`, `interfaces`, `resolv.conf`, `doas.conf`), which
/// is applied after this and so wins over anything written here.
fn write_system_config(mount_dir: &Path, config: &RootfsConfig) -> Result<()> {
    write_shared_config(mount_dir, config)?;

    // Hashing a password is `crypt(3)`, and creating an account is busybox's
    // `adduser` - both run in the chroot, so both need an image this host can
    // *exec*. That is the x86_64 install's answer only: the second machine's
    // tree is installed cross-arch with `--no-scripts`, and its root cannot be
    // chrooted into from an x86_64 host. Everything above is pure file I/O and
    // therefore shared; everything here is not.
    if !config.root_password.is_empty() {
        set_password(mount_dir, "root", &config.root_password)?;
    }
    if !config.user.name.is_empty() {
        set_password(mount_dir, &config.user.name, &config.user.password)?;
    }
    Ok(())
}

/// The half of the machine's configuration that is pure file I/O, and so the
/// half **both** machines' installs run.
///
/// Extracted from [`write_system_config`] when the second machine's Alpine
/// install started consuming the same layer (`PLAN.md` §13.19): its name, its
/// clock, its locale, the network hook directories, and the two cleanups the
/// generated files need. A cross-arch install cannot chroot the target's
/// userland, so what this writes with `fs::write` alone is exactly the boundary
/// between the one shared layer and the x86-only executable half above.
pub(crate) fn write_shared_config(mount_dir: &Path, config: &RootfsConfig) -> Result<()> {
    // The name the prompt, `hostname` and the DHCP client report. Written
    // rather than left to alpine-baselayout's `localhost`, so that
    // `system.hostname` is the one place it is decided.
    fs::write(
        mount_dir.join("etc/hostname"),
        format!("{}\n", config.hostname),
    )?;
    // The hostname rides the loopback line, which is how Alpine's own
    // `setup-hostname` leaves it: a resolver asked for the machine's own name
    // answers from this file instead of going to the network for it.
    fs::write(
        mount_dir.join("etc/hosts"),
        format!(
            "127.0.0.1\tlocalhost localhost.localdomain {hostname}\n::1\t\tlocalhost localhost.localdomain\n",
            hostname = config.hostname
        ),
    )?;

    // musl reads the zone from `/etc/localtime` unless `TZ` says otherwise,
    // and the zone files come from the `tzdata` package. UTC needs neither -
    // it is what the RTC is already read as - so the default costs nothing and
    // a zone that is not installed is named rather than linked to a file that
    // is not there.
    if config.timezone != "UTC" {
        let zone = Path::new("/usr/share/zoneinfo").join(&config.timezone);
        let localtime = mount_dir.join("etc/localtime");
        if zone.exists() {
            let _ = fs::remove_file(&localtime);
            std::os::unix::fs::symlink(&zone, &localtime)?;
        } else {
            println!(
                "rootfs: no {} on this host; leaving the clock in UTC (add tzdata to extra_packages to set {})",
                zone.display(),
                config.timezone
            );
        }
    }

    // There is no locale table to generate: musl takes the locale from the
    // environment. A file that exports it is the whole implementation, and
    // `/etc/profile.d` is what a login shell sources. The directory is made
    // rather than assumed - the base system ships one, and a test tree does
    // not.
    fs::create_dir_all(mount_dir.join("etc/profile.d"))?;
    fs::write(
        mount_dir.join("etc/profile.d/locale.sh"),
        format!(
            "# Written from `system.locale` in rootfs.toml.\nexport LANG={locale}\nexport LC_ALL={locale}\n",
            locale = config.locale
        ),
    )?;

    // The console is a serial line with no keyboard worth typing a password on,
    // which is why the default for both accounts is none at all. Clearing the
    // shadow field is pure file I/O - the *setting* half of `set_password`
    // needs chroot and stays in [`write_system_config`].
    set_password(mount_dir, "root", "")?;
    if !config.user.name.is_empty() {
        set_password(mount_dir, &config.user.name, "")?;
    }

    // These directories exist because `interfaces` names them in its post-up
    // and post-down lines; a glob that matches nothing is fine, a directory
    // that is not there is a line that fails on every ifup.
    fs::create_dir_all(mount_dir.join("etc/network/if-post-up.d"))?;
    fs::create_dir_all(mount_dir.join("etc/network/if-post-down.d"))?;

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

    fs::create_dir_all(mount_dir.join("mnt"))?;
    Ok(())
}

/// The guest's resolver, when `system.dns` names a server. Written after
/// the overlay - the one generated file that beats a file in the tree, which
/// is what makes the key the answer it is.
///
/// The stock `rootfs/etc/resolv.conf` names `10.0.2.3`, the address QEMU's
/// user-mode network answers DNS at by forwarding to whatever the *host's*
/// own resolv.conf names - on a systemd-resolved host, the `127.0.0.53`
/// loopback stub. No QEMU option can redirect that forwarding: `dns=` on the
/// netdev renames the address slirp answers at rather than naming an
/// upstream, so passing it strands every query the guest sends (the `bad
/// address` a ping answers with) - which is why the build never passes it.
/// What a build *can* do is name a real server here: slirp carries the
/// guest's UDP to any address the host can reach, so the guest asks that
/// server directly.
pub(crate) fn write_resolver(mount_dir: &Path, config: &RootfsConfig) -> Result<()> {
    let Some(dns) = config
        .dns
        .as_deref()
        .map(str::trim)
        .filter(|dns| !dns.is_empty())
    else {
        return Ok(());
    };
    fs::write(
        mount_dir.join("etc/resolv.conf"),
        format!(
            "# Written from `system.dns` in rootfs.toml; the guest asks this\n# server directly, carried over QEMU's user-mode network.\nnameserver {dns}\n"
        ),
    )?;
    Ok(())
}

/// The repository's machine-neutral overlay: one copy of each file, merged into
/// **both** images before their per-machine trees. A file belongs here when both
/// machines want the same answer and nothing about the hardware says otherwise;
/// `rootfs-core/root/README` is the inventory and the rule.
pub(crate) const CORE_OVERLAY: &str = "rootfs-core";

/// Merge the shared core first, then the machine's own tree over it - so a
/// machine may still replace anything the core carries, and the pair is applied
/// by every install path on both machines. `src` names are resolved against the
/// repository root; a missing directory merges nothing (`merge_overlay`'s
/// documented answer), which keeps the second machine's small tree honest
/// without a special case here.
///
/// `/etc/inittab` is the one merged file that lands as a *template*: the
/// placeholder line is rendered for this machine immediately after the merge,
/// while the template is still what the tree holds. Every consumer therefore
/// sees one shape - a staged or mounted tree never carries an unrendered
/// placeholder - and no call site can forget its turn.
pub(crate) fn merge_overlays(
    root: &Path,
    machine_tree: &str,
    mount_dir: &Path,
    config: &RootfsConfig,
    serial_getty: bool,
) -> Result<()> {
    utils::merge_overlay(&root.join(CORE_OVERLAY), mount_dir)?;
    utils::merge_overlay(&root.join(machine_tree), mount_dir)?;
    render_inittab(mount_dir, config, serial_getty)
}

/// Render the shared `/etc/inittab` for one machine. Only called through
/// [`merge_overlays`] - see the note there about why rendering rides with the
/// merge rather than with the callers.
///
/// `@SERIAL_GETTY@` is the template's only per-machine line, and its two
/// answers are both facts about the *boot*, not the kernel: an x86_64 session
/// gets a window, so its consoles reach the screen through the framebuffer and
/// the placeholder becomes empty (the stock file's shape). The second
/// machine's interactive `run` attaches no window at all - its wire *is* the
/// console - so there the placeholder becomes a getty on the machine's own
/// serial node, named from `rootfs.serial_port` because the psABI name is
/// the machine's (`ttyAMA0` on the PL011, `ttyS0` on the 16550).
///
/// Everything else in the file is one kernel's answer on both machines since
/// §13.16 ported the vcmux: the virtual consoles carry their bytes to the
/// serial wire everywhere, so the getty lines need no branch.
fn render_inittab(mount_dir: &Path, config: &RootfsConfig, serial_getty: bool) -> Result<()> {
    // The placeholder's own blank line goes with an empty render, so the
    // stock answer keeps the file's spacing exactly as the old per-machine one
    // had it; a rendered getty keeps its surrounding blanks like any other entry.
    let line = if serial_getty {
        format!(
            "{port}::respawn:/sbin/getty -L 38400 {port} vt100",
            port = config.serial_port
        )
    } else {
        String::new()
    };
    // The serial port's name is this machine's psABI spelling (`ttyAMA0` on the
    // PL011, `ttyS0` on the 16550), and it is *read* from the machine's own
    // toml rather than shared with the boot wiring: `rootfs.serial_port` is
    // an install input on both machines, while the `console=` word stays the
    // launcher's business (x86_64 bakes it via `install_boot_files`, aarch64
    // via `aarch64::alpine_cmdline`). The two spellings of one node are pinned
    // to each other per machine - here by `rootfs.toml`'s `[aarch64]` value and
    // `aarch64::SERIAL_PORT`, in config.rs's tests.
    let path = mount_dir.join("etc/inittab");
    let text = fs::read_to_string(&path)?;
    if !text.lines().any(|l| l.trim() == "@SERIAL_GETTY@") {
        anyhow::bail!(
            "rootfs: the shared inittab has no @SERIAL_GETTY@ line to render - \
             the template drifted from what `render_inittab` reads"
        );
    }
    // A whole-line replacement, never a bare word substitution: the comment
    // block explains the rendering *with* the placeholder token in it, and a
    // word-level replace would render that prose too.
    let rendered: Vec<&str> = text
        .lines()
        .map(|l| {
            if l.trim() == "@SERIAL_GETTY@" {
                line.as_str()
            } else {
                l
            }
        })
        .collect();
    let mut out = if line.is_empty() {
        rendered.join("\n").replace("\n\n", "\n")
    } else {
        rendered.join("\n")
    };
    out.push('\n');
    fs::write(&path, out)?;
    Ok(())
}

/// Sets or clears one account's password.
///
/// Empty means "nothing to type": the shadow field is emptied, which is what
/// makes both the default root login and a passwordless account work. Anything
/// else goes through `chpasswd` in the chroot, because hashing a password is
/// `crypt(3)` and the image's own libc owns that function, not this build.
fn set_password(mount_dir: &Path, name: &str, password: &str) -> Result<()> {
    if password.is_empty() {
        let shadow_path = mount_dir.join("etc/shadow");
        if let Ok(shadow) = fs::read_to_string(&shadow_path) {
            let prefix = format!("{name}:");
            let cleared: Vec<String> = shadow
                .lines()
                .map(|line| match line.strip_prefix(&prefix) {
                    Some(rest) => match rest.split_once(':') {
                        Some((_, after)) => format!("{name}::{after}"),
                        None => line.to_string(),
                    },
                    None => line.to_string(),
                })
                .collect();
            fs::write(&shadow_path, cleared.join("\n") + "\n")?;
        }
        return Ok(());
    }

    use std::io::Write;

    let mut child = Command::new("chroot")
        .arg(mount_dir)
        .args(["/usr/sbin/chpasswd"])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to run chpasswd in the image")?;
    if let Some(stdin) = child.stdin.as_mut() {
        writeln!(stdin, "{name}:{password}").context("Failed to feed chpasswd")?;
    }
    let status = child.wait().context("Failed to wait for chpasswd")?;
    if !status.success() {
        anyhow::bail!("chpasswd could not set the password for {name}");
    }
    Ok(())
}

/// Creates the account the config names, using the image's own userland.
///
/// `adduser` is the one that keeps `/etc/passwd`, `/etc/shadow` and the group
/// files consistent with each other, and running it inside the chroot means the
/// busybox in the image decides what those files look like - the same busybox
/// that will read them.
///
/// The password is not set here: [`set_password`] does that on both build paths,
/// so changing it does not need the packages reinstalled. `-D` is what keeps
/// `adduser` from assigning one of its own.
fn create_user(mount_dir: &Path, user: &UserConfig) -> Result<()> {
    if user.name.is_empty() {
        return Ok(());
    }

    // Busybox's `adduser -G` names ONE group and does not split a comma list,
    // so the old `-G mpos,wheel` looked up a group literally called "mpos,wheel"
    // and died with "unknown group mpos,wheel". The supported shape is one
    // membership per call - but the account first: `adduser` makes a group
    // named after the user for its primary gid, and a group already sitting
    // under that name reads back as "group 'mpos' in use" and fails the user
    // add outright. So: create the account, then for each configured group
    // make it if missing and give the user the membership. Creating an
    // existing group and re-adding a membership that is there are both fine,
    // so `addgroup`'s failures say a warning rather than stopping the build;
    // the `-D` user add is the one step that must succeed.
    let status = Command::new("chroot")
        .arg(mount_dir)
        .args(["/usr/sbin/adduser", "-D", "-s", &user.shell])
        .arg(&user.name)
        .status()
        .context("Failed to run adduser in the image")?;
    if !status.success() {
        anyhow::bail!("adduser could not create {}", user.name);
    }

    for group in &user.groups {
        addgroup_in_image(mount_dir, &[group.as_str()]);
        addgroup_in_image(mount_dir, &[user.name.as_str(), group.as_str()]);
    }

    println!(
        "rootfs: created the account {} ({})",
        user.name,
        if user.groups.is_empty() {
            "no extra groups".to_string()
        } else {
            user.groups.join(", ")
        }
    );
    Ok(())
}

/// Run busybox `addgroup` inside the image with these arguments. One argument
/// creates a group; two (`USER GROUP`) add an existing user to an existing
/// group. Both are called only for their end state in `/etc/group`, so an
/// "already exists" or "already a member" is success here.
///
/// The applet is named by absolute path, like `adduser` is: this image keeps
/// the busybox user-management links in `/usr/sbin` alone, and the bare name
/// resolves through the PATH the chroot inherits from the host - which is what
/// made every call here "not found" while its own `adduser` succeeded.
fn addgroup_in_image(mount_dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("chroot");
    cmd.arg(mount_dir)
        .arg("/bin/sh")
        .arg("-c")
        .arg("/usr/sbin/addgroup \"$@\"")
        .arg("addgroup");
    for arg in args {
        cmd.arg(arg);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // The two ways busybox says "already there": `addgroup GROUP` on
            // a group that exists answers "group 'wheel' in use", and the
            // membership forms report an existing entry as one that exists.
            // Both are the end state these calls are after.
            if !text.contains("exists") && !text.contains("in use") {
                eprintln!("addgroup {:?} failed: {}", args, text.trim());
            }
        }
        Err(e) => eprintln!("addgroup {:?} could not run: {}", args, e),
    }
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
    /// Where this build wrote down the device it attached, so a run that is
    /// killed can be cleared by the next one.
    lease: PathBuf,
}

impl ImageGuard {
    fn new(disk_dev: String, mount_dir: PathBuf, lease: PathBuf) -> Self {
        Self {
            disk_dev,
            mount_dir,
            lease,
        }
    }
}

impl Drop for ImageGuard {
    fn drop(&mut self) {
        let still_mounted = utils::umount_recursively(&self.mount_dir);
        // The partition nodes first: `qemu-nbd --disconnect` removes the whole
        // device, and a partition node left pointing at a deleted parent is a
        // stale entry the next run counts as a device in use.
        let _ = Command::new("partx").args(["-d", &self.disk_dev]).status();
        let _ = Command::new("qemu-nbd")
            .arg("--disconnect")
            .arg(&self.disk_dev)
            .status();
        let _ = fs::remove_file(&self.lease);
        // Only remove the staging directory once nothing is mounted under it.
        // `remove_dir_all` recurses through any live bind mount and deletes the
        // mounted-through tree, so with `/dev` bound in here that would destroy
        // the host's own device nodes. A leftover empty-ish directory is a
        // far cheaper failure than that.
        if still_mounted == 0 {
            let _ = fs::remove_dir_all(&self.mount_dir);
        } else {
            eprintln!(
                "rootfs: leaving {} in place: {still_mounted} filesystem(s) still mounted under it",
                self.mount_dir.display()
            );
        }
    }
}

/// Attaches the image as an NBD block device and returns its path.
fn attach_image(image: &Path, build_dir: &Path) -> Result<String> {
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

    // The device is written down before anything is done with it: a `SIGINT`
    // between here and `ImageGuard::drop` ends the process without unwinding,
    // and the next run has to be able to name what it has to clear.
    fs::write(build_dir.join(NBD_LEASE), &disk_dev)
        .with_context(|| format!("Failed to record {NBD_LEASE} in {}", build_dir.display()))?;

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
///
/// The ESP's size is the config's, because it is the one number that depends on
/// what goes on it - the loader, the kernel, the initramfs and nothing else, so
/// the default is generous and a bigger kernel image does not have to mean a
/// different disk.
fn create_gpt(disk_dev: &str, esp_size: &str) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("sfdisk")
        .arg(disk_dev)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to run sfdisk")?;

    if let Some(mut stdin) = child.stdin.take() {
        let table =
            format!("label: gpt\nname=efi,type=U,size={esp_size},bootable\nname=system,type=L\n");
        stdin
            .write_all(table.as_bytes())
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

/// The file recording which nbd device this build attached. A run that is
/// killed cannot clean up after itself, so it leaves the name behind for the
/// next one.
const NBD_LEASE: &str = ".nbd-device";

/// True when the image needs the base system installed rather than just its
/// boot files refreshed.
///
/// The configuration is what decides what is installed, so a change to it -
/// or an image with no root filesystem in it - is what makes the install run.
/// Everything else the image contains is either already there or is a file on
/// the ESP.
pub fn needs_full_install(build_dir: &Path) -> Result<bool> {
    let root = utils::project_root()?;
    let stamp = utils::rootfs_stamp(build_dir);
    if !utils::image_path(build_dir).exists() || !stamp.exists() {
        return Ok(true);
    }
    utils::newer(&root.join("rootfs.toml"), &stamp)
}

/// Undoes what a run that was killed left behind.
///
/// [`ImageGuard`] detaches the image on every path this process can take - but
/// not on `SIGINT`, which ends it without unwinding. Ctrl-C during a build
/// therefore leaves the image attached and mounted, and the next run meets a
/// device in use: `sfdisk` warns about it, `mke2fs` stops to ask about
/// overwriting the filesystem, and both messages name a device this run never
/// made. The device is written down when it is attached, so the next run can be
/// the one that clears it.
fn clear_stale_state(build_dir: &Path) {
    let mount_dir = build_dir.join("mnt");
    let still_mounted = utils::umount_recursively(&mount_dir);

    let lease = build_dir.join(NBD_LEASE);
    if let Ok(device) = fs::read_to_string(&lease) {
        let device = device.trim();
        if !device.is_empty() {
            println!("rootfs: clearing {device}, left attached by an interrupted build");
            let _ = Command::new("partx").args(["-d", device]).status();
            let _ = Command::new("qemu-nbd")
                .arg("--disconnect")
                .arg(device)
                .status();
        }
        let _ = fs::remove_file(&lease);
    }
    // Same hazard as `ImageGuard::drop`: never `remove_dir_all` through a bind
    // mount that outlived the umount - it deletes the mounted-through tree.
    if still_mounted == 0 {
        let _ = fs::remove_dir_all(&mount_dir);
    } else {
        eprintln!(
            "rootfs: leaving {} in place: {still_mounted} filesystem(s) still mounted under it",
            mount_dir.display()
        );
    }
}

/// Asks the kernel for the partition nodes of an attached image.
///
/// Any the last run left are dropped first: `partx -a` on a table it has
/// already added fails, and the failure is a message the caller would have to
/// know to ignore.
fn partitions(disk_dev: &str) -> Result<(String, String)> {
    let esp_dev = format!("{disk_dev}p1");
    let root_dev = format!("{disk_dev}p2");

    let _ = Command::new("partx").args(["-d", disk_dev]).status();
    let _ = Command::new("partx").args(["-a", disk_dev]).status();
    for _ in 0..10 {
        if Path::new(&esp_dev).exists() && Path::new(&root_dev).exists() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        let _ = Command::new("partx").args(["-a", disk_dev]).status();
    }
    if !Path::new(&esp_dev).exists() || !Path::new(&root_dev).exists() {
        anyhow::bail!("The kernel did not publish {esp_dev} and {root_dev}");
    }
    Ok((esp_dev, root_dev))
}

/// Installs the Alpine base system into a freshly formatted root filesystem.
///
/// The expensive half, and the reason it is a step of its own: apk resolves and
/// unpacks thirty-odd packages, and none of that changes when the kernel does.
fn install_base(
    build_dir: &Path,
    mount_dir: &Path,
    config: &RootfsConfig,
    root_dev: &str,
) -> Result<()> {
    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;

    // Repositories and keys, then apk - the way `mkdisk.sh` does it.
    fs::create_dir_all(mount_dir.join("etc/apk/keys"))?;
    let repositories = utils::repositories_text(&config.mirror, &config.branch, &config.repos);
    fs::write(mount_dir.join("etc/apk/repositories"), repositories)?;
    utils::install_apk_keys(&mount_dir.join("etc/apk/keys"))?;

    let status_ok = apk_add(
        &[&apk],
        mount_dir,
        &["--arch", config.arch.as_str(), "--update-cache", "--initdb"],
        &config.base_packages,
    );
    status_ok.context("Failed to install the Alpine base system")?;

    prepare_chroot(mount_dir)?;

    write_fstab(mount_dir, config, &blk_uuid(root_dev)?)?;

    if !config.extra_packages.is_empty() {
        println!("Installing additional packages");
        apk_add(&[&apk], mount_dir, &[], &config.extra_packages)
            .context("Failed to install the additional packages")?;
    }

    // After the packages, so a shell the config names is one that is installed
    // by the time the account points at it.
    create_user(mount_dir, &config.user)?;

    Ok(())
}

/// The packages **both** machines install as their base system - the shared
/// core of the two userlands (§13.19). Each machine's `base_packages` is this
/// list plus whatever only that machine has hardware for (`e2fsprogs` and
/// `dosfstools` exist because the x86_64 image makes filesystems *in the
/// guest*; the second machine's root is laid by this build on the host), so a
/// package both use is one name in one place, and the drift §13.12 recorded -
/// `openrc` in the second machine's base but sitting in the first's extras -
/// cannot come back. A cross-arch install runs no post-install scripts, so a
/// package whose scripts do real work (busybox's applet farm) is covered by
/// the disk `/init` re-making them at first boot (`rootfs-aarch64/init`), and
/// nothing joins this list that needs more than that.
pub(crate) const SHARED_BASE_PACKAGES: &[&str] = &["alpine-base", "openrc"];

/// The services a fresh image boots with, in the runlevel each starts from.
///
/// This is what a stock Alpine turns on - `setup-alpine`, `setup-devd`'s
/// device manager, and `setup-wayland-base`'s session bus - with the entries
/// this kernel cannot honour left out, each for a reason the kernel itself
/// gives:
///
///   * `devfs`, `mdev`, `hwdrivers`, `udev`: the initramfs mounts `/dev` as
///     devtmpfs and the drivers populate it as they probe. There is no module
///     loader for `hwdrivers` to load a modalias with and no hotplug event
///     for a device manager to deliver.
///   * `networking`: the kernel brings `eth0` up with the address QEMU's
///     user-mode network hands out. The service would run a DHCP client over
///     a link that is already configured, and a client that fails leaves the
///     interface it flushed behind it.
///   * `cgroups` and `elogind`: `elogind` declares `need cgroups`, and this
///     kernel has no cgroup filesystem for either to stand on. Stock Alpine
///     does not enable `elogind` either - `setup-wayland-base` starts the
///     bus and leaves the login manager to whoever wants it. `seatd` is the
///     seat manager that runs here, and `weston` reaches it through libseat.
///   * `acpid`: it would open `/dev/input/event*`, and this kernel's evdev
///     nodes are a single queue - a second reader takes the compositor's
///     keystrokes away from it.
///
/// What is left is the log, the clock, the hostname, the session bus, the
/// seat manager, and the hook a local service script hangs off. `hwclock` is
/// the `clock` provider `syslog` needs (`need clock hostname localmount`);
/// the kernel answers it through `/dev/rtc0`, the CMOS clock the wall time
/// is anchored to. The `modules` service is the one modprobe reads the
/// built-in list below (`write_modules`) through.
const SERVICES: &[(&str, &[&str])] = &[
    (
        "sysinit",
        &[
            // The initramfs mounted /sys and /dev before it handed the
            // machine over, so these two have nothing left to mount - but
            // `udev` needs them by name (`need sysfs dev-mount`), and a
            // service whose dependency is not enabled is a service OpenRC
            // refuses to start. Both scripts are written to be no-ops when
            // the filesystem is already there.
            "sysfs",
            "devfs",
            // udev itself, and the cold-plug that gives it the devices it
            // never saw appear: the kernel emitted their uevents during the
            // boot, long before udevd existed, and `udevadm trigger` is what
            // makes it say them again. Without this pair libinput finds no
            // input devices at all - a machine with a keyboard and a pointer
            // that no compositor will ever read.
            "udev",
            "udev-trigger",
            "dmesg",
        ],
    ),
    (
        "boot",
        &[
            "hwclock", "modules", "sysctl", "hostname", "bootmisc", "syslog", "seedrng",
        ],
    ),
    ("default", &["dbus", "seatd", "crond", "local"]),
    ("shutdown", &["killprocs", "mount-ro", "savecache"]),
];

/// Turn on the services [`SERVICES`] names, the way `rc-update add` does it:
/// a symlink from the runlevel directory to the init script, which is the
/// whole of what a runlevel is. Done here rather than through a chroot round
/// trip so a rebuild reproduces the boot from the config alone, and so the
/// set is one place to read - and then read *exactly*: after the adds, the
/// function removes every other name from the directories SERVICES owns,
/// which is what keeps a guest's own `rc-update add` from surviving a
/// rebuild. (An old image is a disk the guest has written to; a rebuilt one
/// is the config talking again.) A name to keep is either added to SERVICES
/// or dropped into `rootfs/etc/runlevels/...`, which runs after this.
///
/// A service whose script is not installed is named and skipped: which
/// packages the image carries is the config's decision, and a runlevel entry
/// pointing at a script that is not there would be an OpenRC complaint on
/// every boot.
///
/// `pub(crate)` because the aarch64 image's install (`aarch64::install_alpine_root`)
/// turns the same runlevels on: the second machine boots the same Alpine
/// userland, so which services start is one decision and not two. A service
/// whose script is not installed is named and skipped there too, which is what
/// lets the two machines share one list while carrying different packages.
pub(crate) fn enable_services(mount_dir: &Path) -> Result<()> {
    let mut enabled = 0usize;
    for (runlevel, services) in SERVICES {
        let dir = mount_dir.join("etc/runlevels").join(runlevel);
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create the {runlevel} runlevel"))?;
        for service in *services {
            if !mount_dir.join("etc/init.d").join(service).exists() {
                println!("rootfs: {service} is not installed; not adding it to {runlevel}");
                continue;
            }
            let link = dir.join(service);
            if link.symlink_metadata().is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(format!("/etc/init.d/{service}"), &link)
                .with_context(|| format!("Failed to enable {service} in {runlevel}"))?;
            enabled += 1;
        }
    }
    // The removals, per runlevel directory SERVICES names. (A directory that
    // is not a runlevel this table speaks for is left alone entirely - this
    // function is SERVICES' voice, not a janitor's.)
    let mut removed = 0usize;
    for (runlevel, services) in SERVICES {
        let dir = mount_dir.join("etc/runlevels").join(runlevel);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if services.contains(&name.as_ref()) {
                continue;
            }
            // Only the symlinks are runlevel entries; anything else a guest
            // put in its own runlevel directory is not this build's to judge.
            if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                fs::remove_file(entry.path())
                    .with_context(|| format!("Failed to remove {runlevel}/{name}"))?;
                removed += 1;
            }
        }
    }
    println!("rootfs: enabled {enabled} boot services, removed {removed} others");
    Ok(())
}

/// Tell kmod's `modprobe` what this kernel already has.
///
/// `modprobe unix` - what a device service's init script asks, and what the
/// boot's `modules` service runs for every line of `/etc/modules` - starts
/// by looking for a tree under `/lib/modules/$(uname -r)`, and warns "can't
/// change directory" loudly when there is none. mpos has no loadable modules
/// at all; every name the boot reaches for (`unix`, `ipv6`, `af_packet`) is
/// built in the way Linux means it, and these two files are how modprobe
/// learns that without a kernel to ask. `modules.builtin` is the list - a hit
/// answers "already loaded", exit 0, no output - and `modules.dep` is the
/// empty resolution table, since nothing in it needs resolving. The directory
/// is named for the version the running kernel will report; see
/// [`utils::kernel_release`].
///
/// `pub(crate)` because the second machine's install (`aarch64::install_alpine_root`)
/// calls it too, and the reason is the same statement of fact: mpos has no loadable
/// modules on either machine, so `modprobe` on either machine has to be told that.
/// Without it the second machine's `modules` service - which reads `/etc/modules`
/// and asks for `af_packet` and `ipv6` - found no `/lib/modules` at all and printed
/// `modprobe: can't change directory to '/lib/modules'` twice on every boot
/// (`PLAN.md` §13.15). [`utils::kernel_release`] is the same string on both
/// machines (`uname -r` is one number, from the root `Cargo.toml`), so one
/// directory name serves both.
pub(crate) fn write_modules(mount_dir: &Path) -> Result<()> {
    let release = utils::kernel_release()?;
    let dir = mount_dir.join("lib/modules").join(&release);
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {dir:?}"))?;
    fs::write(dir.join("modules.dep"), b"")
        .with_context(|| format!("Failed to write {dir:?}/modules.dep"))?;
    // The paths are decoration - kmod matches on the basename - but they
    // are the paths the real kernel's modules would have, so the file reads
    // like everyone else's.
    fs::write(
        dir.join("modules.builtin"),
        "kernel/net/unix/unix.ko\nkernel/net/ipv6/ipv6.ko\nkernel/net/af_packet/af_packet.ko\n",
    )
    .with_context(|| format!("Failed to write {dir:?}/modules.builtin"))?;
    Ok(())
}

/// Attempts to make a transient `qemu-nbd` write hiccup recoverable: how many
/// times `apk add` is given before the build gives up. The device has been seen
/// to return `Input/output error` on the largest package payloads - the binutils
/// archives, `libgcc.a`, the gcc `cc1` - on a host with free space and no stale
/// mount, and only some runs, which is the signature of the NBD layer under
/// heavy write load rather than a real fault the retry can't clear.
const APK_ADD_ATTEMPTS: usize = 4;

/// `<wrapper...> <apk> --no-progress add --root <mount_dir> <preflags...>
/// <packages...>`, retried. A `qemu-nbd`-attached image intermittently fails
/// its biggest writes with an I/O error; `apk` records what it did manage to
/// install, so a plain re-run finishes the packages the failed attempt dropped.
/// `sync` before each retry flushes the queued writeback to the image before
/// the device is touched again.
///
/// The second machine's install shares this retry (`aarch64::install_alpine_root`):
/// its failure mode is not NBD I/O but the mirror fetch, and a re-run that
/// resumes from what `apk` already recorded is the same finish-the-job answer.
/// Its wrapper is `fakeroot --` (the tree must land root-owned without the
/// build taking root), which is why the command is a slice rather than one
/// path: `[]` runs apk directly, `[apk]` runs it bare, `[fakeroot, "--", apk]`
/// wraps it.
pub(crate) fn apk_add(
    cmd: &[&str],
    mount_dir: &Path,
    preflags: &[&str],
    packages: &[String],
) -> Result<()> {
    let (program, rest) = cmd.split_first().context("apk_add: an empty command")?;
    for attempt in 1..=APK_ADD_ATTEMPTS {
        let status = Command::new(program)
            .args(rest)
            .args(["--no-progress", "add"])
            .arg("--root")
            .arg(mount_dir)
            .args(preflags)
            .args(packages)
            .status()
            .context("Failed to run apk")?;
        if status.success() {
            return Ok(());
        }
        if attempt == APK_ADD_ATTEMPTS {
            anyhow::bail!("apk add failed after {attempt} attempts: {packages:?}");
        }
        eprintln!(
            "rootfs: apk add failed (attempt {attempt}/{APK_ADD_ATTEMPTS}); syncing and retrying"
        );
        let _ = Command::new("sync").status();
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// The three files Alpine's `linux-virt` and `mkinitfs` would put on the ESP,
/// and the command line the loader hands the kernel.
///
/// `BOOTX64.EFI` under `EFI/BOOT` is where a UEFI firmware falls back when
/// nothing has written a boot entry into its non-volatile memory, which is
/// exactly the state of a machine that has never booted this disk.
fn install_boot_files(
    mount_dir: &Path,
    config: &RootfsConfig,
    kernel: &Path,
    loader: &Path,
    initramfs: &Path,
    mode: crate::config::RunMode,
) -> Result<()> {
    fs::create_dir_all(mount_dir.join("boot/EFI/BOOT"))?;
    install_file(loader, &mount_dir.join("boot/EFI/BOOT/BOOTX64.EFI"))?;
    install_file(kernel, &mount_dir.join("boot/mpos.elf"))?;
    install_file(initramfs, &mount_dir.join("boot/initramfs.tar"))?;
    fs::write(
        mount_dir.join("boot/mpos.cmdline"),
        format!(
            // `rootfstype` is for the initramfs's mount, not for the kernel:
            // the loader hands the whole line over, and the kernel ignores
            // what it does not know. Naming it here keeps the init script
            // from guessing. The `console=` words are the consoles the run mode
            // asks for (see `RunMode`), and they are all the kernel is told
            // about how this image was meant to be shown.
            "root={} rootfstype={} rootflags=rw {}\n",
            config.root_device,
            config.filesystem,
            mode.cmdline_consoles(&config.serial_port)
        ),
    )?;
    Ok(())
}

/// Install a real Alpine userland for `arch` into a staging tree and return it.
///
/// This is the one place the two machines' *userlands* differ, and the reason is
/// a fact about each machine rather than a taste: x86_64 boots a desktop
/// (`rootfs.toml`: weston, mesa, DRM, seatd), while the second machine has no
/// GPU, no DRM and no PCI bus to hang the card on, so its Alpine tree is the
/// smallest one that boots to a login prompt - `alpine-base` and `openrc`, the
/// packages `rootfs.toml`'s `[aarch64]` half names. What is *not* duplicated is
/// the install: the repositories text, the keys, the mirror and the apk lookup
/// are this file's own helpers, and only the tree they are pointed at differs.
///
/// Two choices are load-bearing and both are measured:
///
///   * **`fakeroot`, not `--usermode`.** A cross-arch `apk` cannot run the
///     packages' post-install scripts - they are the *target's* binaries and the
///     host cannot exec them (`execve: Exec format error`, measured) - so
///     `--no-scripts` is required, and the applet farm such a script would build
///     is made at first boot instead (see that machine's overlay `init`). But
///     `--usermode` writes every file owned by whoever ran the build, and
///     `/etc/shadow` mode 0600 owned by a uid that is not root is a login that
///     cannot read its own password file. `fakeroot` gives the tree root
///     ownership without taking root, so this command stays unprivileged.
///   * **the overlay is core + the machine's own tree, not `rootfs/`.** The
///     shared half of both userlands lives in `rootfs-core/` (`inittab`, the
///     network, the resolver, doas); what stays the machine's own is the `/init`
///     that mounts the pseudo-filesystems a real init expects. The x86_64 side
///     does that from its initramfs script; the second machine's disk `/init` is
///     where that work lives. After the trees merge, the same generated layer
///     the first install runs - hostname, locale, the securetty cleanup, the
///     resolver - lands here too: it is pure file I/O, and §13.12's prompt said
///     `localhost` precisely because this path skipped it.
pub fn install_alpine_root(build_dir: &Path, arch: Arch) -> Result<PathBuf> {
    let root = utils::project_root()?;
    let config = RootfsConfig::load(&root.join("rootfs.toml"), arch)?;
    let staging = alpine_staging(build_dir, arch);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .with_context(|| format!("Failed to create {}", staging.display()))?;

    // Repositories and keys first, the way `install_base` does it: apk decides a
    // repository index is trusted by finding a key whose filename matches the
    // name in the index's signature, and without them every package answers
    // `UNTRUSTED signature` and then `no such package`.
    fs::create_dir_all(staging.join("etc/apk/keys"))?;
    fs::write(
        staging.join("etc/apk/repositories"),
        utils::repositories_text(&config.mirror, &config.branch, &config.repos),
    )?;
    utils::install_apk_keys(&staging.join("etc/apk/keys"))?;

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;
    let mut packages = config.base_packages.clone();
    packages.extend(config.extra_packages.iter().cloned());
    // The same retry the x86_64 install wraps its `apk add` in. Its reason was
    // NBD I/O errors under heavy write load; a cross-arch tree has no nbd
    // device, but the failure it guards is the mirror fetch mid-install, and
    // "apk records what it did manage to install, so a plain re-run finishes the
    // job" is the answer for either producer. `fakeroot` rides in through the
    // preflags, so the command shape is unchanged.
    apk_add(
        &["fakeroot", "--", apk.as_str()],
        &staging,
        &[
            "--arch",
            config.arch.as_str(),
            "--update-cache",
            "--initdb",
            "--no-scripts",
        ],
        &packages,
    )?;

    // The overlay pair, and the inittab rendered with it (`merge_overlays`'s
    // rule: a staged tree never carries an unrendered placeholder). The stock
    // answer for the installed tree; a session re-renders against its own mode
    // before laying the disk.
    merge_overlays(&root, arch.root_overlay(), &staging, &config, false)?;
    // The runlevels, from the same table the x86_64 image uses: a service whose
    // script a machine's package set does not carry is named and skipped, so one
    // list serves both machines. Without this the boot would hand pid 1 to a
    // busybox init whose `::sysinit:/sbin/openrc sysinit` lines run against empty
    // runlevel directories - an Alpine userland that starts no service at all.
    enable_services(&staging)?;
    // And the module index, for the same reason and from the same helper the
    // x86_64 install uses: the `modules` service this table turns on reads
    // `/etc/modules` - `af_packet` and `ipv6` here - and asks `modprobe` for each
    // name, so without `/lib/modules/<release>` the boot prints "can't change
    // directory to '/lib/modules'" for every line. `modules.builtin` is how a
    // machine with no loadable modules answers that honestly: a hit means "already
    // loaded", exit 0, nothing printed. The names are the same ones on both
    // machines because they are one kernel's code, and the directory is named for
    // the release both machines report (`uname -r` is one number).
    write_modules(&staging)?;
    // The generated system layer, now that the cross-arch install can consume
    // it: `system.hostname` becomes `/etc/hostname` (without it the prompt
    // says `localhost`, which §13.12 had to name rather than fix), the locale
    // export lands in `/etc/profile.d`, the passwordless shadow clearing makes
    // the login actually work, the securetty cleanup keeps `login` from calling
    // an empty password a wrong one, and the init.d chmod sweep is the one
    // thing OpenRC silently refuses without. All pure file I/O - none of it
    // needs the image's binaries to run, which is the whole boundary (§13.19).
    write_shared_config(&staging, &config)?;
    // The probe file the kernel's filesystem check reads (see
    // [`crate::disk::PROBE_FILE`]). It sits at the root of a real Alpine tree
    // because the check is about the *block driver and the filesystem*, which
    // the machine must prove on whichever root it boots - and the proof is the
    // file's known bytes, not a name it happens to have. Written here rather
    // than in a machine's overlay so the two roots' copies come from one
    // constant and cannot drift.
    fs::write(
        staging.join(crate::disk::PROBE_FILE),
        crate::disk::PROBE_BYTES,
    )
    .with_context(|| format!("Failed to lay the probe file on {}", staging.display()))?;
    // The resolver, after the trees and the generated layer - the one file
    // that beats a file in the tree, which is what makes `system.dns` the
    // answer it is (`write_resolver` carries the full story; it is one slirp
    // fact for both machines). Without this call the guest rides QEMU's
    // forwarder into the host's loopback stub and every `apk` fetch from the
    // *running* machine asks a question nothing answers.
    write_resolver(&staging, &config)?;
    println!(
        "alpine: {} packages installed into {} for {}",
        packages.len(),
        staging.display(),
        config.arch
    );
    Ok(staging)
}

/// Where `arch`'s Alpine staging tree lives, whether or not it is installed yet.
pub fn alpine_staging(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-alpine-root", arch.name()))
}

/// The stamp that records what `arch`'s Alpine tree was installed *from*.
fn alpine_stamp(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-alpine-root.stamp", arch.name()))
}

/// True when the staging tree predates something it is built from: the toml,
/// either overlay tree, or the kernel whose release name `/lib/modules/<release>`
/// carries (`write_modules`' directory outlives a version bump otherwise).
/// A missing tree is stale by definition - there is nothing to reuse.
fn alpine_root_is_stale(build_dir: &Path, arch: Arch) -> Result<bool> {
    let root = utils::project_root()?;
    let staging = alpine_staging(build_dir, arch);
    let stamp = alpine_stamp(build_dir, arch);
    if !staging.exists() || !stamp.exists() {
        return Ok(true);
    }
    let since = utils::file_mtime(&stamp)?;
    for input in [
        // The one rootfs.toml now holds both machines' halves, so this stamp is
        // invalidated by an edit to the *other* machine's section too. That is
        // the price of one file describing both, and the same price the first
        // machine's stamp pays.
        root.join("rootfs.toml"),
        root.join(CORE_OVERLAY),
        root.join(arch.root_overlay()),
    ] {
        if utils::dir_newer_than(&input, since)? || utils::newer(&input, &stamp)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Install `arch`'s Alpine tree unless the stamp says the staging directory
/// already holds the current one - then write the stamp. The re-lay of the ext4
/// image happens on *every* boot-media build regardless; this stamp only decides
/// whether the network-backed `apk` install runs. That is what makes a boot after
/// a kernel change cost seconds instead of a package fetch, and it is the same
/// split the first machine draws between its boot-file update and its full
/// install.
pub fn ensure_alpine_root(build_dir: &Path, arch: Arch) -> Result<()> {
    if alpine_root_is_stale(build_dir, arch)? {
        install_alpine_root(build_dir, arch)?;
    } else {
        println!(
            "alpine: {} is up to date (reinstall inputs unchanged)",
            alpine_staging(build_dir, arch).display()
        );
    }
    fs::write(alpine_stamp(build_dir, arch), "alpine: staged tree\n")?;
    Ok(())
}

/// The first openrc service failure in `log`, named by the line that explains
/// it - or `None` when every service came up.
///
/// openrc marks the result of each service with `[ ok ]` or `[ !! ]`, wrapped in
/// the terminal escapes that make the marker coloured, and it prints the
/// service's own message on the line *above* the marker. So the search is: strip
/// the escapes, find the marker, and walk back to the last line with anything on
/// it - for the failure that motivated this that is `mount: mounting proc on
/// /proc failed: Resource busy`, and for a failure that prints nothing it is the
/// marker's own line.
///
/// Kept as a function rather than inlined because it is the whole of the check
/// and the only way to test it is against a captured log; the case it was
/// written for is a unit test below.
pub fn openrc_failure(log: &str) -> Option<String> {
    let plain = without_ansi(log);
    let lines: Vec<&str> = plain.lines().collect();
    let at = lines.iter().position(|line| line.contains("!!"))?;
    lines[..at]
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .or_else(|| lines.get(at))
        .map(|line| line.trim().to_string())
}

/// The log with openrc's terminal escapes removed.
///
/// A failure marker arrives wrapped in colour and cursor codes -
/// `ESC[1mESC[31m !! ESC[1mESC[34m]` - so the service's own message, which is
/// what a person needs, is two lines above a marker no `grep` will show. This
/// strips `ESC [ ... letter` sequences and carriage returns so the line can be
/// quoted in an error message as it was written.
pub fn without_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Only CSI is used by openrc's output; anything else is left
                // in place rather than guessed at.
                if chars.next() == Some('[') {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-for-byte block the Alpine boot printed while this kernel's
    /// procfs had no `/proc/<pid>/environ`, copied out of a captured log.
    ///
    /// It is here rather than paraphrased because the escapes *are* the case:
    /// openrc writes the service's message, then moves the cursor back to the
    /// start of the line and overwrites it with the coloured marker, so a
    /// checker that searched the raw text for the mount error would find
    /// nothing on the marker's line and a checker that searched for `!!` in the
    /// raw text would still have to know it is the failure colour and not the
    /// success one.
    const FAILING_PROC_MOUNT: &str = " \x1b[1m\x1b[32m*\x1b[m Mounting /proc\x1b[K ...\r\r\n\
         mount: mounting proc on /proc failed: Resource busy\r\r\n\
         \x1b[A\x1b[153C \x1b[1m\x1b[34m[\x1b[1m\x1b[31m !! \x1b[1m\x1b[34m]\x1b[m\r\r\n";

    #[test]
    fn an_openrc_service_failure_is_named_by_its_own_message() {
        assert_eq!(
            openrc_failure(FAILING_PROC_MOUNT).as_deref(),
            Some("mount: mounting proc on /proc failed: Resource busy")
        );

        // A healthy runlevel is `None`, and the success marker shares the
        // marker's brackets - which is the whole reason the check looks for the
        // failure colour's text and not for a bracket.
        let healthy = " \x1b[1m\x1b[32m*\x1b[m Starting local\x1b[K ...\r\r\n\
             \x1b[A\x1b[153C \x1b[1m\x1b[34m[\x1b[1m\x1b[32m ok \x1b[1m\x1b[34m]\x1b[m\r\r\n\
             Welcome to Alpine Linux 3.25.0\r\r\nmpos login: ";
        assert_eq!(openrc_failure(healthy), None);

        // And the escapes really are stripped for the message a person reads,
        // rather than merely skipped over.
        assert_eq!(
            without_ansi("a\x1b[1mb\x1b[0m\r\nc"),
            "ab\nc",
            "CSI sequences and carriage returns go, everything else stays"
        );
    }

    #[test]
    fn the_image_is_not_an_input_to_its_own_stamp() {
        // The output of the build is not one of its inputs. Comparing the
        // image against the stamp meant a write landing after the stamp read
        // as "the image changed after the build": qemu-nbd flushes when it
        // detaches, which is after the last file is copied, and a real build
        // left the image 82 milliseconds newer than the stamp - every run
        // then rebuilt, and `run` bailed with "still out of date" having just
        // built successfully.
        let root = utils::project_root().expect("the project root is there");
        let build_dir = Path::new("/mpos-a-build-dir-that-does-not-exist");
        let inputs = stamp_inputs(&root, build_dir, "debug").expect("the paths resolve");

        assert!(
            inputs.contains(&root.join("rootfs.toml")),
            "the configuration is an input"
        );
        assert!(
            inputs.contains(&utils::initramfs_path(build_dir)),
            "the initramfs is an input"
        );
        assert!(
            inputs.contains(&utils::kernel_path("debug", Arch::X86_64).unwrap()),
            "the kernel is an input"
        );
        assert!(
            inputs.contains(&utils::loader_path("debug", Arch::X86_64).unwrap()),
            "the loader is an input"
        );
        assert_eq!(inputs.len(), 4, "and that is the whole list");
        assert!(
            !inputs.contains(&utils::image_path(build_dir)),
            "the image is built, not built from"
        );
    }

    /// A mount directory with the two files the generated configuration reads
    /// back: `/etc/shadow`, as the packages leave it, and `/etc/init.d`.
    fn staged_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpos-rootfs-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc/init.d")).unwrap();
        fs::create_dir_all(dir.join("etc/network")).unwrap();
        fs::write(
            dir.join("etc/shadow"),
            "root:$6$abc$def:19000:0:99999:7:::\nmpos:!:19000:0:99999:7:::\nother:$6$x$y:19000:0:99999:7:::\n",
        )
        .unwrap();
        fs::write(dir.join("etc/securetty"), "ttyS0\n").unwrap();
        dir
    }

    /// The configuration a test asks for, read back through the real loader -
    /// the keys and the defaults are then the ones the build gets, not a
    /// hand-built struct that can drift from them. `name` keys the file as
    /// well, because the tests run in parallel and share a temp directory.
    ///
    /// The file a test writes is sectioned like the shipped one, and read for
    /// the first machine: these tests are about what the *rootfs* half does, and
    /// a machine fact is what the machine's own section is for.
    fn load_config(name: &str, toml: &str) -> RootfsConfig {
        let path = std::env::temp_dir().join(format!("mpos-rootfs-test-{name}.toml"));
        fs::write(&path, toml).unwrap();
        RootfsConfig::load(&path, Arch::X86_64).unwrap()
    }

    #[test]
    fn a_passwordless_account_has_nothing_to_type_and_the_rest_is_untouched() {
        let dir = staged_dir("shadow");
        let config = load_config(
            "shadow",
            "[default]\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\nuser.password = \"\"\n",
        );
        write_system_config(&dir, &config).unwrap();

        let shadow = fs::read_to_string(dir.join("etc/shadow")).unwrap();
        let field = |name: &str| {
            shadow
                .lines()
                .find(|line| line.starts_with(&format!("{name}:")))
                .map(|line| line.split(':').nth(1).unwrap().to_string())
        };
        assert_eq!(field("root").as_deref(), Some(""), "root's hash is cleared");
        assert_eq!(
            field("mpos").as_deref(),
            Some(""),
            "and so is the account's"
        );
        assert_eq!(
            field("other").as_deref(),
            Some("$6$x$y"),
            "an account the config does not name keeps its own"
        );
        assert!(
            !dir.join("etc/securetty").exists(),
            "securetty lists tty names ttyname(2) cannot answer yet"
        );
    }

    #[test]
    fn the_shared_half_clears_the_login_without_the_chroot_half_running() {
        // The counterfactual for the extraction: the second machine's install
        // calls *only* this function, on a tree whose userland its host cannot
        // exec. So the whole passwordless shape - shadow cleared, securetty
        // gone, name/locale landed - must come out of `write_shared_config`
        // alone. If any of it ever moves back into the chroot half, this fails
        // and the cross-arch install silently loses the login again.
        let dir = staged_dir("shared-config");
        let config = load_config(
            "shared-config",
            "[default]\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\nuser.password = \"hunter2\"\n",
        );
        write_shared_config(&dir, &config).unwrap();

        let shadow = fs::read_to_string(dir.join("etc/shadow")).unwrap();
        let field = |name: &str| {
            shadow
                .lines()
                .find(|line| line.starts_with(&format!("{name}:")))
                .map(|line| line.split(':').nth(1).unwrap().to_string())
        };
        assert_eq!(field("root").as_deref(), Some(""), "root's hash is cleared");
        assert_eq!(
            field("mpos").as_deref(),
            Some(""),
            "and so is the account's, whatever its configured password says - \
             setting that one needs the image's own crypt(3)"
        );
        assert!(
            !dir.join("etc/securetty").exists(),
            "the securetty cleanup is in the shared half"
        );
        assert_eq!(
            fs::read_to_string(dir.join("etc/hostname")).unwrap(),
            "mpos\n",
            "so is the name - without it the prompt says localhost"
        );
        assert!(
            dir.join("etc/network/if-post-up.d").is_dir()
                && dir.join("etc/network/if-post-down.d").is_dir(),
            "the hook directories `interfaces` names exist before the overlay lands"
        );
    }

    #[test]
    fn the_name_and_the_locale_land_where_the_guest_reads_them() {
        let dir = staged_dir("system");
        let config = load_config(
            "system",
            "[default]\nsystem.hostname = \"probe\"\nsystem.locale = \"C.UTF-8\"\nuser.name = \"\"\n",
        );
        write_system_config(&dir, &config).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("etc/hostname")).unwrap(),
            "probe\n"
        );
        let hosts = fs::read_to_string(dir.join("etc/hosts")).unwrap();
        assert!(
            hosts.contains("127.0.0.1\tlocalhost localhost.localdomain probe"),
            "the name rides the loopback line: {hosts}"
        );
        let locale = fs::read_to_string(dir.join("etc/profile.d/locale.sh")).unwrap();
        assert!(locale.contains("export LANG=C.UTF-8"), "{locale}");
    }

    #[test]
    fn the_shared_inittab_renders_one_getty_line_per_machine() {
        // The two answers `render_inittab` may give, and the shape each must
        // leave behind. The x86_64 half is a regression pin: it asserts the
        // rendered file is *byte-identical* to what the repository's own tree
        // carried before the core split - the placeholder and its blank line
        // vanish together, so the stock image's inittab cannot drift just
        // because it gained a template. The aarch64 half pins the getty's
        // exact spelling, node name included, because that line is the whole
        // difference between the two machines' boots.
        //
        // Pure string work on purpose: every test here would otherwise share
        // one staging directory name across parallel harness threads, and the
        // renderer under test is exactly the function `merge_overlays` calls -
        // so this reads the repository's template once and renders it twice.
        let config = load_config(
            "inittab",
            "[default]\nrootfs.serial_port = \"ttyAMA0\"\nsystem.hostname = \"mpos\"\n",
        );
        let stock = fs::read_to_string(
            crate::utils::project_root()
                .unwrap()
                .join("rootfs-core/etc/inittab"),
        )
        .unwrap();

        let render = |text: &str, serial_getty: bool| -> String {
            let dir = staged_dir(&format!("render-{}", std::process::id()));
            fs::write(dir.join("etc/inittab"), text).unwrap();
            render_inittab(&dir, &config, serial_getty).unwrap();
            let out = fs::read_to_string(dir.join("etc/inittab")).unwrap();
            let _ = fs::remove_dir_all(&dir);
            out
        };

        for (name, serial_getty) in [("plain", false), ("serial", true)] {
            let text = render(&stock, serial_getty);
            assert!(
                !text.contains("@SERIAL_GETTY@"),
                "the placeholder never survives rendering ({name})"
            );
            for console in ["tty1", "tty2", "tty3", "tty4"] {
                assert!(
                    text.contains(&format!("{console}::respawn:/sbin/getty 38400 {console}")),
                    "{console}'s getty is on both machines"
                );
            }
            if serial_getty {
                assert!(
                    text.contains("ttyAMA0::respawn:/sbin/getty -L 38400 ttyAMA0 vt100\n"),
                    "the interactive second machine logs in on its own serial node: {text}"
                );
            } else {
                let config_lines: Vec<&str> =
                    text.lines().filter(|l| !l.starts_with('#')).collect();
                assert!(
                    !config_lines
                        .iter()
                        .any(|l| l.contains("ttyAMA0") || l.contains("ttyS0")),
                    "the default answer carries no serial getty at all: {text}"
                );
                // The *configuration* half of the file - every non-comment,
                // non-blank line, in order - must read exactly as the old
                // per-machine x86_64 file did. The comment block and the blank
                // spacing are allowed to differ (the prose now explains the
                // rendering); a lost or reordered entry may not.
                let body = |t: &str| -> Vec<String> {
                    t.lines()
                        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                        .map(str::to_string)
                        .collect()
                };
                // The original file's config lines, copied here so the pin does
                // not depend on git history: what `rootfs/etc/inittab` said
                // before §13.19 folded it into the shared template.
                let old = "::sysinit:/sbin/openrc sysinit\n::sysinit:/sbin/openrc boot\n::wait:/sbin/openrc default\ntty1::respawn:/sbin/getty 38400 tty1\ntty2::respawn:/sbin/getty 38400 tty2\ntty3::respawn:/sbin/getty 38400 tty3\ntty4::respawn:/sbin/getty 38400 tty4\n::ctrlaltdel:/sbin/reboot\n::shutdown:/sbin/openrc shutdown\n";
                assert_eq!(
                    body(&text),
                    body(old),
                    "the stock answer cannot drift from the file it replaced"
                );
            }
        }
    }

    #[test]
    fn the_core_tree_carries_the_files_both_machines_share() {
        // The inventory `rootfs-core/root/README` claims, checked against the
        // tree itself: these are the files the convergence moved out of
        // `rootfs/`, and a drift back (or a GPU-only file sneaking in) fails
        // here rather than silently re-splitting the userlands.
        let root = crate::utils::project_root().unwrap();
        for path in [
            "rootfs-core/etc/inittab",
            "rootfs-core/etc/network/interfaces",
            "rootfs-core/etc/resolv.conf",
            "rootfs-core/etc/doas.conf",
            "rootfs-core/root/README",
        ] {
            assert!(
                root.join(path).is_file(),
                "the shared overlay must carry {path}"
            );
        }
        // And the halves that stayed per-machine, because they are hardware
        // facts: the Wayland session's pieces live only in `rootfs/`.
        assert!(root.join("rootfs/etc/profile.d/xdg-runtime.sh").is_file());
        assert!(
            !root
                .join("rootfs-core/etc/profile.d/xdg-runtime.sh")
                .exists()
        );
        // One inittab now exists in the repository, not two.
        assert!(!root.join("rootfs/etc/inittab").exists());
        assert!(!root.join("rootfs-aarch64/etc/inittab").exists());
    }

    #[test]
    fn fstab_names_the_root_by_uuid_and_the_configured_filesystem() {
        let dir = staged_dir("fstab");
        let config = load_config("fstab", "[default]\nrootfs.filesystem = \"ext4\"\n");
        write_fstab(&dir, &config, "1234-abcd").unwrap();

        let fstab = fs::read_to_string(dir.join("etc/fstab")).unwrap();
        assert!(
            fstab.contains("UUID=1234-abcd\t/\t\text4\trelatime\t0 1"),
            "{fstab}"
        );
        assert!(
            !fstab.contains("boot"),
            "the ESP is the firmware's, and an entry for it would only be a mount that can fail"
        );
    }

    #[test]
    fn a_named_dns_becomes_the_guests_resolv_conf_and_an_absent_one_keeps_it() {
        // `10.0.2.3` is the address QEMU's user-mode network answers DNS at,
        // and what the tree's own resolv.conf names; an unset `dns` leaves it
        // in place, so the guest rides the host's resolver. A named server is
        // written over it, after the overlay, because slirp can carry a query
        // to any server the host reaches - the one way a build has of
        // choosing the guest's upstream.
        let dir = staged_dir("resolver-set");
        fs::write(dir.join("etc/resolv.conf"), "nameserver 10.0.2.3\n").unwrap();
        let config = load_config("resolver-set", "[default]\nsystem.dns = \"223.5.5.5\"\n");
        write_resolver(&dir, &config).unwrap();
        let text = fs::read_to_string(dir.join("etc/resolv.conf")).unwrap();
        assert!(text.contains("nameserver 223.5.5.5"), "{text}");

        let dir = staged_dir("resolver-unset");
        fs::write(dir.join("etc/resolv.conf"), "nameserver 10.0.2.3\n").unwrap();
        let config = load_config("resolver-unset", "[default]\n");
        write_resolver(&dir, &config).unwrap();
        let text = fs::read_to_string(dir.join("etc/resolv.conf")).unwrap();
        assert!(text.contains("nameserver 10.0.2.3"), "{text}");
    }

    #[test]
    fn the_repository_file_lists_the_components_in_the_order_they_are_configured() {
        let text = utils::repositories_text(
            "https://example.invalid/alpine",
            "edge",
            &["main".to_string(), "community".to_string()],
        );
        assert_eq!(
            text,
            "https://example.invalid/alpine/edge/main\nhttps://example.invalid/alpine/edge/community\n"
        );
    }
}
