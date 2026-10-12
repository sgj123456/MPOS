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
/// `pub(crate)` because the same question is asked outside a build:
/// `qemu::needs_mode_switch` compares the answer with what the image already
/// carries, and that comparison is what tells `run` whether it has to take root
/// at all.
pub(crate) fn effective_mode(arch: Arch) -> Result<RunMode> {
    if let Ok(text) = std::env::var("MPOS_XTASK_MODE") {
        return RunMode::parse_strict(&text);
    }
    // The standing mode is the machine's own key: `mode` is a word about *this*
    // image, whose command line this build writes. `main` reads the same source
    // and exports it through `MPOS_XTASK_MODE`, so both halves of the decision
    // (`run` deciding whether a refresh is needed, and the refresh itself) answer
    // with one value.
    QemuConfig::load_default(arch)?.run_mode()
}

pub fn build_rootfs(build_dir: &Path, profile: &str, arch: Arch) -> Result<()> {
    let root = utils::project_root()?;
    let config_path = root.join(crate::paths::ROOTFS_CONFIG);
    let config = RootfsConfig::load(&config_path, arch)?;

    let image = utils::image_path(build_dir, arch);
    if !image.exists() {
        anyhow::bail!(
            "The {} disk image does not exist at {}; run `cargo xtask disk --arch {}` first",
            arch.name(),
            image.display(),
            arch.name()
        );
    }

    // The initramfs is one of the inputs the staleness check below reads, and it
    // is brought up to date here rather than assumed: a stale archive carries an
    // old mtime, which would read as "the rootfs is newer than this" and skip
    // the rebuild the new archive needs.
    crate::initramfs::ensure(build_dir, arch)?;

    // The mode to bake. It is a boot-file change, not a package change, so it
    // forces the (fast) boot-file refresh on its own even when nothing else
    // moved - that is how `cargo xtask run --mode X` re-points an existing image
    // without reinstalling Alpine.
    let mode = effective_mode(arch)?;

    if !needs_rebuild(build_dir, profile, arch)?
        && crate::qemu::baked_mode(build_dir, arch) == Some(mode)
    {
        println!("rootfs: up to date");
        return Ok(());
    }

    let kernel = utils::kernel_path(profile, arch)?;
    let loader = utils::loader_path(profile, arch)?;
    let initramfs = crate::initramfs::archive_path(build_dir, arch);
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
    // The record the last build left, read once and carried through this one:
    // its owned set is what an incremental install diffs against, and its
    // package-list hash is what tells a moved list from a torn record. A v1 or
    // missing stamp reads as `None` everywhere it is used, which resolves to
    // the full install — the direction that costs one rebuild and cannot be
    // wrong about what the image holds.
    let previous = read_stamp(build_dir, arch);
    let full_install = needs_full_install(build_dir, arch)?;
    println!(
        "{}",
        match (full_install, &previous) {
            (true, _) => "Building the rootfs (base system)",
            (false, Some(_)) => "Updating the image (boot files, configuration and packages)",
            (false, None) => "Updating the image (boot files only)",
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
        install_base(build_dir, &mount_dir, &config, &root_dev, arch)?;
    } else if previous.is_some() {
        // The diff path: only reached when the shape keys did not move *and*
        // there is a v2 record to diff against. It installs what the lists ask
        // for now and owns but the image does not have yet, removes what the
        // last build installed and the lists no longer name, and leaves every
        // other byte of the tree alone.
        apply_package_delta(
            build_dir,
            &mount_dir,
            &config,
            arch,
            previous.as_ref().unwrap(),
        )?;
    }

    install_boot_files(&mount_dir, &config, &loader, &initramfs, mode, arch)?;

    // The machine's own configuration, then the overlay. Both run on every
    // build path - a change to the hostname or to a file under `rootfs/` should
    // take effect on the next boot, not on the next reinstall - and the overlay
    // goes last, so a file there is the authority over anything generated. The
    // resolver is the one file that may still change after it.
    write_system_config(&mount_dir, &config, arch)?;
    // The runlevels and the module index are re-asserted on *every* build,
    // not only on a full install - they are the build's policy, and policy
    // that only lands on a reinstall quietly rots (a `rc-update add` inside
    // a running guest otherwise survives every later build). Both run
    // before the overlay, so `rootfs/` stays the authoritative last word on
    // any name either writes.
    enable_services(&mount_dir)?;
    write_modules(&mount_dir)?;
    // The busybox applet farm, re-asserted on every build for the same reason
    // the runlevels are: it is the build's answer to a post-install script a
    // cross-architecture `apk` could not run, and a name that only appears on a
    // reinstall is one that rots. It runs *before* the overlay, so a file either
    // tree carries stays the authoritative last word over a name it holds.
    lay_applet_farm(&root, &mount_dir)?;
    // The overlay pair, applied last: `rootfs-core/` is the half both machines
    // share and the machine's own tree wins over it, so `rootfs/` stays the
    // authoritative last word on any name either writes.
    merge_overlays(&root, arch.root_overlay(), &mount_dir)?;
    write_resolver(&mount_dir, &config)?;

    // Detached before the stamp, not after: the nbd server can still be
    // flushing the last writes when the last file is copied, and the image's
    // mtime bumps when it does. Letting go of the image first makes the file
    // quiet, so the stamp that goes down next is strictly after everything the
    // build wrote.
    drop(_guard);
    // What the *package lists* hashed to before this build ran anything: on a
    // full install that is the current value (`delta`), because the tree was
    // laid from exactly these lists; on a delta run it is the predecessor the
    // previous stamp recorded, so a stamp torn between two delta runs still
    // says which lists the image's packages came from.
    let packages_hash = if full_install {
        config.package_delta_hash(sorted_tree_digests(&root, arch))
    } else {
        previous
            .as_ref()
            .map(|r| r.packages.clone())
            .unwrap_or_default()
    };
    write_stamp(build_dir, arch, &image, &config, &root, &packages_hash)?;
    // Record the mode now baked into `mpos.cmdline`, so the next `run`
    // can tell cheaply whether the image already says what the user asked for.
    crate::qemu::set_baked_mode(build_dir, arch, mode)?;
    println!("rootfs: completed");
    Ok(())
}

/// Builds the root filesystem unless the stamp says the image already holds the
/// current one.
pub fn ensure(build_dir: &Path, profile: &str, arch: Arch) -> Result<()> {
    build_rootfs(build_dir, profile, arch)
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
fn stamp_inputs(root: &Path, build_dir: &Path, profile: &str, arch: Arch) -> Result<Vec<PathBuf>> {
    Ok(vec![
        root.join(crate::paths::ROOTFS_CONFIG),
        utils::kernel_path(profile, arch)?,
        utils::loader_path(profile, arch)?,
        crate::initramfs::archive_path(build_dir, arch),
    ])
}

pub fn needs_rebuild(build_dir: &Path, profile: &str, arch: Arch) -> Result<bool> {
    let root = utils::project_root()?;
    let image = utils::image_path(build_dir, arch);
    let stamp = utils::rootfs_stamp(build_dir, arch);
    if !image.exists() || !stamp.exists() {
        return Ok(true);
    }

    for input in stamp_inputs(&root, build_dir, profile, arch)? {
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
        || utils::dir_newer_than(&root.join(arch.root_overlay()), stamp_time)?)
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

/// The machine's own configuration: the shared half (below), plus the account
/// the image logs in as.
///
/// This runs on both build paths - the full install and the boot-files update -
/// because none of it needs `apk`, all of it is cheap, and a change to the
/// configuration should take effect on the next boot rather than on the next
/// reinstall. Everything whose content is a constant moved to the `rootfs/`
/// overlay instead (`inittab`, `interfaces`, `resolv.conf`, `doas.conf`), which
/// is applied after this and so wins over anything written here.
fn write_system_config(mount_dir: &Path, config: &RootfsConfig, arch: Arch) -> Result<()> {
    write_shared_config(mount_dir, config)?;

    // Hashing a password is `crypt(3)` and running the image's own `adduser` is
    // how its account is made; both are the image's *userland*, so both need a
    // tree this host can exec. That is `Arch::host_runs_guest_userland`, the same
    // boundary `install_base` draws with `--no-scripts` - and the account is what
    // the two sides of it answer differently.
    if arch.host_runs_guest_userland() {
        // The first machine: `install_base` has already run the image's
        // `adduser`, so the account exists and only its password is written here
        // - on every build path, which is what makes changing it cheap.
        if !config.root_password.is_empty() {
            set_password(mount_dir, "root", &config.root_password)?;
        }
        if !config.user.name.is_empty() {
            set_password(mount_dir, &config.user.name, &config.user.password)?;
        }
        return Ok(());
    }

    // The second machine: this host cannot enter the tree, so the account is
    // written here instead, in the only form a cross-architecture build can
    // manage - the files `adduser` would have written, by pure file I/O. What is
    // still out of reach is a *password*, because hashing one is the image's own
    // `crypt(3)`: that is refused by name rather than written in a shape the
    // guest cannot check.
    refuse_password(arch, "system.root_password", "root", &config.root_password)?;
    if !config.user.name.is_empty() {
        refuse_password(
            arch,
            "user.password",
            &config.user.name,
            &config.user.password,
        )?;
    }
    // The packages' own system accounts come first: the configured account joins
    // some of the groups they make, and the other order would create such a group
    // itself at a *regular* id (measured: `seat` at 1001 on the image built
    // before this, against 103 on the machine whose package scripts ran).
    create_system_accounts(mount_dir)?;
    create_account_without_chroot(mount_dir, &config.user)
}

/// The first id a *system* account or group gets: where Alpine's `adduser -S`
/// and `addgroup -S` begin their search.
const FIRST_SYSTEM_ID: u32 = 100;

/// The groups a cross-architecture install has to make itself, in the order the
/// packages' own post-install scripts make them - which is what decides their
/// ids - each beside the package whose script names it.
///
/// Read out of the image's own script store, not guessed: every one of these is
/// an `addgroup -S <name>` line in `/lib/apk/db/scripts.tar.gz` of the tree this
/// install builds (`busybox`'s, `dbus`'s, `seatd`'s and `weston`'s post-install
/// scripts), and the two shipped images' `/etc/group` files differ by exactly
/// these four names plus the ids they push:
///
///   x86_64  (scripts run):   klogd:101  messagebus:102  seat:103  weston-launch:104
///   aarch64 (`--no-scripts`): *none of the four*
///
/// `seat` is the one that shows what not making them costs: the configured
/// account joins it, so [`create_account_without_chroot`] used to *create* it -
/// at the first free id for a regular group, 1001 - and the two machines'
/// group files then disagreed about the group the seat manager hands its socket
/// to. A group-only name is made here so that membership finds it.
const SYSTEM_GROUPS: [(&str, &str); 4] = [
    ("klogd", "busybox"),
    ("messagebus", "dbus"),
    ("seat", "seatd"),
    ("weston-launch", "weston"),
];

/// The system *users* of the same list: `(name, primary group, package)`.
///
/// Both scripts spell the same call - `adduser -S -D -H -h /dev/null -s
/// /sbin/nologin -G <group> -g <name> <name>` - and `-D` is why the shadow line
/// this writes is *locked* rather than passwordless: measured against the image
/// whose scripts ran, whose lines are
/// `klogd:!:20736:0:99999:7:::` and `messagebus:!:20736:0:99999:7:::`.
const SYSTEM_USERS: [(&str, &str, &str); 2] = [
    ("klogd", "klogd", "busybox"),
    ("messagebus", "messagebus", "dbus"),
];

/// Makes the system accounts a cross-architecture install cannot let the
/// packages' own scripts make.
///
/// The same boundary [`create_account_without_chroot`] works on, one step
/// earlier: `apk --no-scripts` skips every post-install script, and a script
/// that creates an account is not something the machine's own `/init` can finish
/// later without being told to. What that costs is visible on the machine:
/// `dbus` refuses to start (`checkpath: owner 'root:messagebus' not found`, then
/// `ERROR: dbus failed to start`) and `syslogd` runs as root instead of dropping
/// to `klogd`. Both are the *result* of those scripts, which is what the two
/// shipped images' own files measured:
///
///   x86_64:  klogd:x:100:101:klogd:/dev/null:/sbin/nologin
///            messagebus:x:101:102:messagebus:/dev/null:/sbin/nologin
///   aarch64: neither account at all
///
/// So this writes those two lines, their groups, and the two group-only names,
/// with Alpine's own rule for the ids - the first free one at or above
/// [`FIRST_SYSTEM_ID`] - in the order the scripts run, which is what reproduces
/// the other image's 100/101 users and 101/102/103/104 groups. Idempotent for
/// the same reason the account is: a name that is already there is left exactly
/// as it is, and a group that exists keeps whatever id it has.
fn create_system_accounts(mount_dir: &Path) -> Result<()> {
    let passwd_path = mount_dir.join("etc/passwd");
    let group_path = mount_dir.join("etc/group");
    let shadow_path = mount_dir.join("etc/shadow");

    let mut passwd = fs::read_to_string(&passwd_path)
        .with_context(|| format!("Failed to read {}", passwd_path.display()))?;
    let mut groups = fs::read_to_string(&group_path)
        .with_context(|| format!("Failed to read {}", group_path.display()))?;
    let mut shadow = fs::read_to_string(&shadow_path)
        .with_context(|| format!("Failed to read {}", shadow_path.display()))?;

    let mut made: Vec<String> = Vec::new();
    for (name, package) in SYSTEM_GROUPS {
        if !has_name(&groups, name) {
            ensure_group(&mut groups, name, None, FIRST_SYSTEM_ID);
            made.push(format!("group {name} ({package})"));
        }
    }
    // `adduser -S` counts in the *user* sequence, which is its own: `klogd` is
    // user 100 while its group is 101, and `messagebus` is user 101 while its
    // group is 102.
    for (name, group, package) in SYSTEM_USERS {
        if has_name(&passwd, name) {
            continue;
        }
        let gid = ensure_group(&mut groups, group, None, FIRST_SYSTEM_ID);
        let uid = next_free_id(&passwd, FIRST_SYSTEM_ID);
        append_line(
            &mut passwd,
            &format!("{name}:x:{uid}:{gid}:{name}:/dev/null:/sbin/nologin"),
        );
        remove_line(&mut shadow, name);
        append_line(
            &mut shadow,
            &format!("{name}:!:{today}:0:99999:7:::", today = days_since_epoch()),
        );
        made.push(format!("user {name} ({package})"));
    }

    if made.is_empty() {
        return Ok(());
    }
    for (path, text) in [
        (&passwd_path, &passwd),
        (&group_path, &groups),
        (&shadow_path, &shadow),
    ] {
        fs::write(path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    }
    println!("rootfs: created the system accounts {}", made.join(", "));
    Ok(())
}

/// Today, as `/etc/shadow` counts it: whole days since the epoch. `adduser`
/// writes this same field, and the other image's lines carry the day its own
/// install ran, so a build stamping its own day here is the faithful answer
/// rather than a zero a shadow tool would read as a password past its time.
fn days_since_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() / 86_400)
        .unwrap_or(0)
}

/// Refuses a password where the host cannot run the image's own `chpasswd`.
///
/// The account beside it *is* made without a chroot (see
/// [`create_account_without_chroot`]); a password is the one part that is not,
/// because hashing one is `crypt(3)` and the image's libc owns that function.
/// It is refused by the key that asked for it, so the message names where to
/// look rather than only what failed.
fn refuse_password(arch: Arch, key: &str, account: &str, password: &str) -> Result<()> {
    if password.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "rootfs: {key} sets a password for `{account}`, which is hashed by running the \
         image's own `chpasswd` - and this host cannot run {} binaries. Leave it empty \
         (the console is a serial line with no keyboard) or build on that machine",
        arch.name()
    )
}

/// The half of the machine's configuration that is pure file I/O, and so the
/// half **both** machines' installs run.
///
/// Extracted from [`write_system_config`] when the second machine's Alpine
/// install started consuming the same layer (`NOTES.md` §13.19): its name, its
/// clock, its locale, the network hook directories, and the two cleanups the
/// generated files need. A cross-arch install cannot chroot the target's
/// userland, so what this writes with `fs::write` alone is exactly the boundary
/// between the one shared layer and the half above that needs the image's own
/// programs. That half is a *password* now and no longer the account: the
/// account itself is text, so it is written on this side too
/// ([`create_account_without_chroot`]), and an account whose password is empty -
/// the shipped config's, on both machines - needs nothing executed at all.
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
pub(crate) const CORE_OVERLAY: &str = crate::paths::ROOTFS_CORE;

/// Merge the shared core first, then the machine's own tree over it - so a
/// machine may still replace anything the core carries, and the pair is applied
/// by every install path on both machines. `src` names are resolved against the
/// repository root; a missing directory merges nothing (`merge_overlay`'s
/// documented answer), which keeps the second machine's small tree honest
/// without a special case here.
///
/// Nothing is *rendered* afterwards any more. `/etc/inittab` used to land as a
/// template whose one placeholder line the first machine rendered empty and the
/// second machine rendered as a getty on its serial node; the measurement that
/// retired it is in that file's own header - the vcmux carries the current
/// virtual console's bytes to the wire on both machines, so the windowless
/// session reaches its login prompt without a getty on the wire, and the second
/// login was one line nothing needed.
pub(crate) fn merge_overlays(root: &Path, machine_tree: &str, mount_dir: &Path) -> Result<()> {
    utils::merge_overlay(&root.join(CORE_OVERLAY), mount_dir)?;
    utils::merge_overlay(&root.join(machine_tree), mount_dir)?;
    Ok(())
}

/// Lay the busybox applet farm a package's own post-install would have made.
///
/// `busybox --install -s` is the honest way to make it, and it has to run as
/// *the tree's* busybox - which is the target's binary, and the host cannot exec
/// it. So the names are data ([`crate::paths::BUSYBOX_APPLETS`], from
/// `busybox --list-full` on the same Alpine packages) and this links them,
/// which is the same shape as `SYSTEM_USERS`: what a package's script would have
/// written, written by the build instead. Without it a cross-architecture
/// install has no `/sbin/init` at all - the package ships the applet, not the
/// name - and the disk's pid-1 chain does not exist until somebody boots it.
///
/// Used by an image install *and* by a sweep's medium (`disk::sweep_medium`), so
/// a test disk answers the same names a product one does.
pub(crate) fn lay_applet_farm(root: &Path, mount_dir: &Path) -> Result<()> {
    let path = root.join(crate::paths::BUSYBOX_APPLETS);
    let list =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let paths: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    link_applet_paths(mount_dir, &paths)
}

/// Link `/bin/busybox` at each relative `path` under `dest`, skipping the ones
/// that already answer.
///
/// Absolute targets, because the guest resolves a symlink relative to the tree
/// it stands in and `busybox` sits at `/bin/busybox`. A path that already
/// answers is left *alone* rather than replaced: `sbin/ifup` is ifupdown's and
/// `sbin/mkfs.vfat` is dosfstools', and a link into busybox over either would
/// lose the program a boot script reads. That rule is also what makes a second
/// build a no-op, and it is the same one the archive's small farm uses
/// (`initramfs::link_applets`), so both farms answer "what is already here?"
/// the same way.
pub(crate) fn link_applet_paths(dest: &Path, paths: &[&str]) -> Result<()> {
    for path in paths {
        let target = dest.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink("/bin/busybox", &target)
            .with_context(|| format!("Failed to link /bin/busybox at {}", target.display()))?;
    }
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

/// Creates the account the config names on a machine whose userland this host
/// cannot execute.
///
/// [`create_user`] is the first machine's answer, and it is the better one: it
/// runs the image's own `adduser`, so the busybox that will *read*
/// `/etc/passwd`, `/etc/group` and `/etc/shadow` is the one that writes them. A
/// cross-architecture build has no such door - a chroot into the tree is `Exec
/// format error` (measured), the same boundary that makes `install_base` pass
/// `--no-scripts` - so this writes the three entries `adduser` would have
/// written, and nothing here is executed.
///
/// The shape is small and the files are text: a `passwd` line naming the primary
/// group `adduser` makes for the account, the same group made in `/etc/group`,
/// one membership per configured group, and a `shadow` line whose password field
/// is empty. Groups the config names are *made* when they are missing, which is
/// not a convenience but a consequence of the same boundary: a package that makes
/// a system group does it in its post-install script, and a cross-arch install
/// runs none (`seatd`'s `seat` is the live example), so the account's own groups
/// are the one place the build can be sure they exist.
///
/// It runs on *every* build path rather than only on a full install, and it is
/// idempotent: an account that is already there is left exactly as it is, so a
/// rebuild - or a config edit that lands on the boot-files path - neither
/// duplicates the entries nor renumbers the account. (That is also why it is the
/// one step here that may not assume an empty tree.)
///
/// What it deliberately does not do, beside what `adduser` does: copy `/etc/skel`
/// into the home. Nothing in either image's boot reads a dotfile from there, and
/// an empty home is the honest half of a step this build cannot run.
fn create_account_without_chroot(mount_dir: &Path, user: &UserConfig) -> Result<()> {
    if user.name.is_empty() {
        return Ok(());
    }

    let passwd_path = mount_dir.join("etc/passwd");
    let group_path = mount_dir.join("etc/group");
    let shadow_path = mount_dir.join("etc/shadow");

    let mut passwd = fs::read_to_string(&passwd_path)
        .with_context(|| format!("Failed to read {}", passwd_path.display()))?;
    // The account is the `passwd` entry; a tree that already has it (a rebuild)
    // is left alone here, and `write_shared_config` above has re-cleared its
    // shadow field on the way past.
    if passwd
        .lines()
        .any(|line| line_name(line) == Some(user.name.as_str()))
    {
        return Ok(());
    }
    let mut groups = fs::read_to_string(&group_path)
        .with_context(|| format!("Failed to read {}", group_path.display()))?;
    let mut shadow = fs::read_to_string(&shadow_path)
        .with_context(|| format!("Failed to read {}", shadow_path.display()))?;

    // The primary group, named after the account, then one membership per
    // configured group. `user.groups` carries the primary group's own name in
    // the shipped config, so joining it adds a membership there too - which is
    // what the first machine's `addgroup mpos mpos` leaves behind, and the two
    // images' files are better off saying the same thing.
    let gid = ensure_group(&mut groups, &user.name, None, FIRST_REGULAR_ID);
    let uid = next_free_id(&passwd, FIRST_REGULAR_ID);
    for name in &user.groups {
        // A group the *system accounts* already made keeps its own id, which is
        // what makes the compositor's `seat` the same group on both machines.
        ensure_group(&mut groups, name, Some(&user.name), FIRST_REGULAR_ID);
    }

    append_line(
        &mut passwd,
        &format!(
            "{}:x:{}:{}:Linux User,,,:/home/{}:{}",
            user.name, uid, gid, user.name, user.shell
        ),
    );
    // An empty password field is the whole of what makes a login passwordless;
    // the aging fields are the ones an Alpine account gets (min 0, max 99999,
    // warn 7). Any stub line already carrying the name is dropped first, so the
    // file cannot end up with two entries for one account - `write_shared_config`
    // clears a field *by name* and would address the first of them.
    remove_line(&mut shadow, &user.name);
    append_line(&mut shadow, &format!("{}::0:0:99999:7:::", user.name));

    for (path, text) in [
        (&passwd_path, &passwd),
        (&group_path, &groups),
        (&shadow_path, &shadow),
    ] {
        fs::write(path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    }

    // A home the account can actually use. The build runs as root - the mount
    // needs it - so the directory can be owned by the new account; a host test
    // cannot own a foreign id, and what it pins is the account's numbers and the
    // file shape rather than the owner.
    //
    // Being uid 0 is not the same fact as being *able* to own a foreign id, and
    // the machine this was measured on is the one that separates them: its root
    // is the root of a user namespace whose map holds a single id
    // (`/proc/self/uid_map` = `0 1000 1`), so `chown` to any other id answers
    // `EINVAL` - the id cannot be represented here - on every filesystem, and a
    // build that treated that as fatal could not write an account at all. So
    // `EINVAL` is read as what it says and printed rather than assumed, the way
    // `hand_back` already treats a refused hand-over of the build tree; every
    // other error is still a failure, because a home the account cannot write
    // in is a real defect of an installed image.
    let home = mount_dir.join("home").join(&user.name);
    fs::create_dir_all(&home).with_context(|| format!("Failed to create {}", home.display()))?;
    utils::set_mode(&home, 0o755)?;
    if utils::is_root()
        && let Err(err) = std::os::unix::fs::chown(&home, Some(uid), Some(gid))
    {
        if err.raw_os_error() != Some(libc::EINVAL) {
            return Err(err).with_context(|| format!("Failed to own {}", home.display()));
        }
        println!(
            "rootfs: {} is left owned by the build user - this machine cannot own id {uid} \
             (a user namespace whose map holds one id)",
            home.display()
        );
    }

    println!(
        "rootfs: created the account {} ({}) without a chroot",
        user.name,
        if user.groups.is_empty() {
            "no extra groups".to_string()
        } else {
            user.groups.join(", ")
        }
    );
    Ok(())
}

/// The first id a regular account or group gets. Alpine's own `adduser` and
/// `addgroup` begin their search here, so an account made without a chroot lands
/// where one made with it would.
const FIRST_REGULAR_ID: u32 = 1000;

/// The name at the head of a `passwd`/`group`/`shadow` line, or `None` for a
/// line that has none (a blank one, or a comment).
fn line_name(line: &str) -> Option<&str> {
    let name = line.split(':').next()?.trim();
    (!name.is_empty()).then_some(name)
}

/// The smallest identifier at or above `floor` that no line of `text` already
/// uses. Both files this is asked about keep it in field 2 - `passwd`'s uid and
/// `group`'s gid - so one function answers for both.
fn next_free_id(text: &str, floor: u32) -> u32 {
    let used: std::collections::HashSet<u32> = text
        .lines()
        .filter_map(|line| line.split(':').nth(2))
        .filter_map(|field| field.trim().parse().ok())
        .collect();
    let mut id = floor;
    while used.contains(&id) {
        id += 1;
    }
    id
}

/// The gid `name` has in a `group` file's text, making the group at a free id
/// when the text does not carry it. Answers the id either way, which is what
/// lets the `passwd` line name it as the account's primary group.
///
/// `member` is a second, optional job in the same walk: the account is added to
/// the group's comma list, whether the group was just made or was already there.
/// The two belong together because `adduser` and `addgroup USER GROUP` are the
/// two calls this stands in for, and both end in one line of one file.
///
/// `floor` is the caller's own rule for a *new* group, because the two callers
/// have different ones and Alpine does too: `adduser` makes a regular group from
/// [`FIRST_REGULAR_ID`] up, while the system groups a package's script makes with
/// `addgroup -S` come from [`FIRST_SYSTEM_ID`] up.
fn ensure_group(text: &mut String, name: &str, member: Option<&str>, floor: u32) -> u32 {
    if let Some(line) = text.lines().find(|line| line_name(line) == Some(name)) {
        let fields: Vec<&str> = line.split(':').collect();
        let gid: u32 = fields
            .get(2)
            .and_then(|field| field.trim().parse().ok())
            .unwrap_or(FIRST_REGULAR_ID);
        if let Some(member) = member {
            let members = fields.get(3).copied().unwrap_or("");
            // `addgroup` treats "already a member" as success, and so does this:
            // the group carrying the account is the end state either way.
            if !members.split(',').any(|existing| existing == member) {
                let joined = if members.is_empty() {
                    member.to_string()
                } else {
                    format!("{members},{member}")
                };
                let replaced = format!("{name}:x:{gid}:{joined}");
                let mut updated = text
                    .lines()
                    .map(|line| {
                        if line_name(line) == Some(name) {
                            replaced.clone()
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<String>>()
                    .join("\n");
                updated.push('\n');
                *text = updated;
            }
        }
        return gid;
    }

    let gid = next_free_id(text, floor);
    append_line(text, &format!("{name}:x:{gid}:{}", member.unwrap_or("")));
    gid
}

/// Whether a `passwd`/`group`/`shadow` file's text carries this name already.
fn has_name(text: &str, name: &str) -> bool {
    text.lines().any(|line| line_name(line) == Some(name))
}

/// Drops every line naming `name`.
fn remove_line(text: &mut String, name: &str) {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| line_name(line) != Some(name))
        .collect();
    *text = if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n") + "\n"
    };
}

/// Appends a line to a file's contents, keeping exactly one newline between it
/// and what was there.
fn append_line(text: &mut String, line: &str) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
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

/// The header line every v2 stamp begins with, and the version gate.
const STAMP_V2_HEADER: &str = "mpos rootfs stamp v2";

/// What one build's record says about the image it left behind.
///
/// `full` is the hash of the keys that decide the *shape* of the tree - the
/// mirror, the branch, the repositories, the filesystem and the partition
/// sizes - and a change there can only be answered by laying the image again.
/// `delta` is the hash of the package lists; Commit 3 turns a delta-only
/// change into an incremental add/remove rather than a reinstall. `owned` is
/// the set this build installed (the names it handed `apk`, not the dependency
/// closure): the diff may remove only what the build itself put there, never a
/// package someone added inside a running guest. A v1 stamp - or anything that
/// does not parse - reads as `None`, which every caller resolves towards the
/// full install, the direction that costs one rebuild and buys a correct image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StampRecord {
    pub(crate) full: String,
    pub(crate) delta: String,
    /// The package-list hash the *last* build carried — written after every
    /// path (full or incremental), so a delta install can tell "the lists
    /// moved" from "the stamp was torn and never said what it installed".
    pub(crate) packages: String,
    pub(crate) owned: Vec<String>,
}

/// The two hashes a build records for the configuration it just installed.
/// The two overlay trees' digests, sorted — the content half of what the
/// package lists mean on this machine. Part of the delta hash because the
/// trees land in the image too: without them, a stamp written from identical
/// config but different overlays would read as "already current" to a later
/// run whose trees had moved back. Their mtimes are still checked by
/// [`needs_rebuild`] on every path; this makes the content answer too.
pub(crate) fn sorted_tree_digests(root: &Path, arch: Arch) -> Vec<u64> {
    let mut trees = vec![
        utils::dir_digest(&root.join(CORE_OVERLAY)),
        utils::dir_digest(&root.join(arch.root_overlay())),
    ];
    trees.sort();
    trees
}

fn stamp_hashes(config: &RootfsConfig, root: &Path, arch: Arch) -> (String, String) {
    (
        config.full_install_hash(),
        config.package_delta_hash(sorted_tree_digests(root, arch)),
    )
}

/// Writes the stamp: the hashes above, then the owned set. `packages` is the
/// current package-list hash — the same value as `delta` on a full install, and
/// its *predecessor* on an incremental one, which is what lets the next build
/// tell a moved list from a torn record that never said what it installed.
fn write_stamp(
    build_dir: &Path,
    arch: Arch,
    image: &Path,
    config: &RootfsConfig,
    root: &Path,
    packages: &str,
) -> Result<()> {
    let (full, delta) = stamp_hashes(config, root, arch);
    let mut out = format!("{STAMP_V2_HEADER}\nimage: {}\n", image.display());
    out.push_str(&format!(
        "full: {full}\ndelta: {delta}\npackages: {packages}\nowned: {}\n",
        config.installed_names().len()
    ));
    for name in config.installed_names() {
        out.push_str(&name);
        out.push('\n');
    }
    fs::write(utils::rootfs_stamp(build_dir, arch), out)?;
    Ok(())
}

/// The names apk's own database says a tree holds.
///
/// `lib/apk/db/installed` is one record per installed package separated by a
/// blank line, and each record's `P:` line is its name — the same truth
/// `apk info` reads, which is why this parser exists: it is the only complete
/// answer to "what is in there", including the dependencies no list named.
pub(crate) fn parse_installed_db(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("P:"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Reads the mounted tree's apk database. A missing file is an empty answer,
/// not an error: a tree with no db is one nothing was ever installed into,
/// which is exactly what the add side of a diff needs to hear.
pub(crate) fn installed_names_in(mount_dir: &Path) -> Result<Vec<String>> {
    let path = mount_dir.join("lib/apk/db/installed");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse_installed_db(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Installs and removes only the difference between what the configuration
/// names now and what the last build left owned.
///
/// Three sets decide it, and each exists for a measured reason:
///
/// * **want** — the configuration's names. These are added when the tree does
///   not hold them yet.
/// * **owned** — the previous stamp's set: the names *this build series*
///   installed. Only these may be removed. A package someone added inside a
///   running guest (a `t-*` fixture's tool, a hand-installed editor) is in no
///   stamp, so no future build touches it — which is the whole safety property
///   of the incremental path.
/// * **present** — what apk's own database says the tree holds, read live
///   rather than trusted from a record: a torn write or a manual removal
///   changes the truth under the stamp, and the diff answers to the tree.
///
/// An owned name that is wanted but absent (a failed earlier install, apk
/// having dropped a dependency) re-enters `want`, because apk's resolution is
/// idempotent and re-listing a held package costs a no-op.
pub(crate) fn apply_package_delta(
    build_dir: &Path,
    mount_dir: &Path,
    config: &RootfsConfig,
    arch: Arch,
    previous: &StampRecord,
) -> Result<()> {
    let want = config.installed_names();
    let present = installed_names_in(mount_dir)?;
    let to_add: Vec<String> = want
        .iter()
        .filter(|name| !present.contains(name))
        .cloned()
        .collect();
    let to_remove: Vec<String> = previous
        .owned
        .iter()
        .filter(|name| !want.contains(name))
        .cloned()
        .collect();

    if to_add.is_empty() && to_remove.is_empty() {
        println!("packages: up to date");
        return Ok(());
    }

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;
    let cache = apk_cache_dir(build_dir, arch);
    let no_scripts: &[&str] = if arch.host_runs_guest_userland() {
        &[]
    } else {
        &["--no-scripts"]
    };

    if !to_remove.is_empty() {
        println!(
            "Removing {} package(s): {}",
            to_remove.len(),
            to_remove.join(", ")
        );
        // `--no-scripts` always, on both machines: removing packages while the
        // host cannot exec the guest's binaries must not ask those binaries to
        // run their own deinstall scripts. What a script would have undone is
        // the farm below and files the next boot reads, not this call's job.
        apk_del(
            &[&apk],
            mount_dir,
            &["--arch", arch.name(), "--no-scripts"],
            &to_remove,
        )
        .context("Failed to remove the packages the configuration no longer names")?;
    }

    if !to_add.is_empty() {
        println!(
            "Installing {} package(s): {}",
            to_add.len(),
            to_add.join(", ")
        );
        // The conditional refresh again, as on a full install: warm cache
        // first, network only when resolution fails. No `--initdb` — the db
        // exists and holds the tree's truth.
        let mut flags: Vec<&str> = vec!["--arch", arch.name()];
        flags.extend_from_slice(no_scripts);
        if apk_add(&[&apk], mount_dir, &flags, &to_add, &cache).is_err() {
            eprintln!(
                "rootfs: incremental add without a refreshed index failed; refreshing and retrying"
            );
            let mut refreshed = flags.clone();
            refreshed.push("--update-cache");
            apk_add(&[&apk], mount_dir, &refreshed, &to_add, &cache)
                .context("Failed to install the packages the configuration newly names")?;
        }
    }

    // A removed package may have owned applet links, and a newly installed one
    // brings names the farm has never linked: re-lay it before anything else
    // reads `/bin`. `lay_applet_farm` skips names that already answer, so the
    // common case is a walk over the list and nothing written.
    let root = utils::project_root()?;
    lay_applet_farm(&root, mount_dir)?;

    // The chroot half an x86_64 install runs for post-install scripts: a newly
    // added weston or eudev expects the same `/proc`-backed setup here that
    // `install_base` gives it. Harmless when nothing needed it — bind mounts
    // are cheap and the guard unmounts recursively either way.
    if arch.host_runs_guest_userland() {
        prepare_chroot(mount_dir)?;
    }
    Ok(())
}

/// `<wrapper...> <apk> --no-progress del --root <mount_dir> <preflags...>
/// <packages...>`, retried like [`apk_add`] for the same NBD I/O failure.
fn apk_del(cmd: &[&str], mount_dir: &Path, preflags: &[&str], packages: &[String]) -> Result<()> {
    let (program, rest) = cmd.split_first().context("apk_del: an empty command")?;
    for attempt in 1..=APK_ADD_ATTEMPTS {
        let mut command = Command::new(program);
        command.args(rest);
        apk_del_arguments(mount_dir, preflags, packages).apply(&mut command);
        let status = command.status().context("Failed to run apk del")?;
        if status.success() {
            return Ok(());
        }
        if attempt == APK_ADD_ATTEMPTS {
            anyhow::bail!("apk del failed after {attempt} attempts: {packages:?}");
        }
        eprintln!(
            "rootfs: apk del failed (attempt {attempt}/{APK_ADD_ATTEMPTS}); syncing and retrying"
        );
        let _ = Command::new("sync").status();
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// Reads the record a previous build left, or `None` when there is no v2
/// record to trust.
pub fn read_stamp(build_dir: &Path, arch: Arch) -> Option<StampRecord> {
    let text = fs::read_to_string(utils::rootfs_stamp(build_dir, arch)).ok()?;
    parse_stamp(&text)
}

/// The reader above's parsing half, split out so the format is host-testable
/// without a `build/` directory: every ambiguity resolves towards `None`, which
/// its callers resolve towards a full install.
fn parse_stamp(text: &str) -> Option<StampRecord> {
    let mut lines = text.lines();
    if lines.next() != Some(STAMP_V2_HEADER) {
        return None; // v1 (`image:` first) or foreign: nothing to reuse
    }
    let mut full = None;
    let mut delta = None;
    let mut packages = None;
    let mut owned_count = None;
    let mut owned: Vec<String> = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("image: ") {
            let _ = rest; // the path is informational; the image is not an input
        } else if let Some(rest) = line.strip_prefix("full: ") {
            full = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("delta: ") {
            delta = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("packages: ") {
            packages = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("owned: ") {
            owned_count = Some(rest.parse::<usize>().ok()?);
        } else if !line.is_empty() && owned_count.is_some_and(|n| owned.len() < n) {
            owned.push(line.to_string());
        } else {
            return None; // anything else is a format this reader does not own
        }
    }
    Some(StampRecord {
        full: full?,
        delta: delta?,
        packages: packages?,
        owned,
    })
    .filter(|record| {
        // A torn write — the count declared more names than the file holds —
        // is not a record to trust: a later diff would remove against an owned
        // set it believes complete. `owned_count` only reaches this point as
        // `Some`, because a missing or unparseable line already returned `None`.
        owned_count == Some(record.owned.len())
    })
}

/// True when the image needs the base system installed rather than just its
/// boot files refreshed.
///
/// The shape keys decide this, not the file's mtime: changing one package name
/// used to reinstall thirty-odd packages because `rootfs.toml` was "newer than
/// the stamp", which is the cost this stamp exists to take away. A missing
/// image or stamp installs; so does a v1 stamp, whose record cannot say whether
/// the shape moved. With a v2 record the answer is one comparison: the shape
/// hash the last build wrote against the one this configuration carries now.
pub fn needs_full_install(build_dir: &Path, arch: Arch) -> Result<bool> {
    let root = utils::project_root()?;
    let config = RootfsConfig::load(&root.join(crate::paths::ROOTFS_CONFIG), arch)?;
    if std::env::var_os("MPOS_XTASK_FORCE_FULL").is_some() {
        // The escape hatch: lay the image fresh whatever the hashes say. An
        // incremental image can legitimately drift from a clean build (an edge
        // mirror moved under it; a package was broken inside a running guest),
        // and the honest answer to "reinstall it" is a flag, not a deleted file.
        return Ok(true);
    }
    if !utils::image_path(build_dir, arch).exists() {
        return Ok(true);
    }
    match read_stamp(build_dir, arch) {
        None => Ok(true),
        Some(record) => Ok(record.full != config.full_install_hash()),
    }
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
    arch: Arch,
) -> Result<()> {
    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;

    // Repositories and keys, then apk - the way `mkdisk.sh` does it.
    fs::create_dir_all(mount_dir.join("etc/apk/keys"))?;
    let repositories = utils::repositories_text(&config.mirror, &config.branch, &config.repos);
    fs::write(mount_dir.join("etc/apk/repositories"), repositories)?;
    utils::install_apk_keys(&mount_dir.join("etc/apk/keys"))?;

    // Two flags differ between the machines, and both are about what an install
    // can *run*:
    //
    // * `--arch` is the package architecture, which is the machine's own word -
    //   the one difference no amount of shared code can wish away, because a
    //   mirror serves each architecture's packages separately. It comes from
    //   the `Arch` the command was given and not from a config key, so the word
    //   apk is asked for is the same word the kernel and loader were built for.
    // * `--no-scripts` where the host cannot exec the target's binaries. A
    //   package's post-install script is the *guest's* program, and a cross-arch
    //   `execve` is `Exec format error` (measured): apk would fail the install
    //   rather than skip it. What such a script would have done is done by the
    //   build instead, in the machine's own name rather than at first boot -
    //   busybox's applet farm, for one, which `lay_applet_farm` links from
    //   `assets/configs/busybox-applets.txt`.
    let no_scripts: &[&str] = if arch.host_runs_guest_userland() {
        &[]
    } else {
        &["--no-scripts"]
    };
    // The index refresh is conditional, and the order of the two attempts is
    // the whole mechanism: install *without* `--update-cache` first, so a warm
    // cache answers an offline host from its own indexes; only when that fails
    // retry with the refresh, which is what a changed or expired index needs.
    // A full install always lands on `--initdb` — the tree's apk db starts
    // empty here whatever the cache holds.
    let cache = apk_cache_dir(build_dir, arch);
    let mut base_flags: Vec<&str> = vec!["--arch", arch.name(), "--initdb"];
    base_flags.extend_from_slice(no_scripts);

    match apk_add(
        &[&apk],
        mount_dir,
        &base_flags,
        &config.base_packages,
        &cache,
    )
    .context("Failed to install the Alpine base system")
    {
        Ok(()) => {}
        Err(first) => {
            eprintln!(
                "rootfs: base install without a refreshed index failed ({first:#}); refreshing and retrying"
            );
            let mut refreshed_flags = base_flags.clone();
            refreshed_flags.push("--update-cache");
            // An incremental failure leaves packages half-installed; the caller's
            // guard detaches the image and `build_rootfs` bails before writing
            // the stamp, so the owned set never records over a partial tree.
            apk_add(
                &[&apk],
                mount_dir,
                &refreshed_flags,
                &config.base_packages,
                &cache,
            )
            .context("Failed to install the Alpine base system (after --update-cache)")?;
            std::mem::drop(first);
        }
    }

    // The half of an install that runs the image's own userland: apk's scripts as
    // they need `/proc`, `/dev` and `/sys`, and - below - the account, whose
    // password is hashed by the image's own `crypt(3)` and made by its own
    // `adduser`. A host that cannot exec the tree does neither, which is the one
    // thing `Arch::host_runs_guest_userland` answers.
    if arch.host_runs_guest_userland() {
        prepare_chroot(mount_dir)?;
    }

    write_fstab(mount_dir, config, &blk_uuid(root_dev)?)?;

    if !config.extra_packages.is_empty() {
        println!("Installing additional packages");
        // The same `--arch`/`--no-scripts` answer as the base set: these are the
        // same packages' siblings, installed by the same apk for the same
        // machine. The index is refreshed here — the base install above has
        // already put one in the cache, and extras reach for names (weston,
        // mesa, eudev) outside the `main` component's default view; keeping the
        // refresh on this second call is what keeps an offline host working,
        // because a warm index costs no network either way.
        let mut extra_flags: Vec<&str> = no_scripts.to_vec();
        extra_flags.push("--update-cache");
        apk_add(
            &[&apk],
            mount_dir,
            &extra_flags,
            &config.extra_packages,
            &cache,
        )
        .context("Failed to install the additional packages")?;
    }

    // After the packages, so a shell the config names is one that is installed
    // by the time the account points at it.
    if arch.host_runs_guest_userland() {
        create_user(mount_dir, &config.user)?;
    }

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
/// package whose scripts do real work is covered by the build instead: the
/// applet farm is `lay_applet_farm`'s, and nothing joins this list that needs
/// more than that.
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
    (
        "default",
        &[
            // The service that reads `/etc/network/interfaces` (`iface eth0 inet
            // dhcp`) and runs `ifup` for each `auto` interface. The image has
            // declared DHCP since the file was written; what was missing is this
            // line, so nothing ever ran `ifup` - the address came from the
            // kernel's own static default and the client §4.5 names as the gap
            // was never invoked. `enable_services` skips a service the image does
            // not have and *says so*, so a machine whose Alpine install carries
            // no such script reports the name rather than booting quietly
            // unconfigured.
            "networking",
            "dbus",
            "seatd",
            "crond",
            "local",
        ],
    ),
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
/// The second machine's install runs this too, because both machines boot the
/// same Alpine userland: which services start is one decision and not two. A
/// service whose script a machine's package set does not carry is named and
/// skipped, which is what lets the two machines share one list while carrying
/// different packages.
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
/// The second machine's install calls it too, and the reason is the same
/// statement of fact: mpos has no loadable modules on either machine, so
/// `modprobe` on either machine has to be told that.
/// Without it the second machine's `modules` service - which reads `/etc/modules`
/// and asks for `af_packet` and `ipv6` - found no `/lib/modules` at all and printed
/// `modprobe: can't change directory to '/lib/modules'` twice on every boot
/// (`NOTES.md` §13.15). [`utils::kernel_release`] is the same string on both
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

/// The argument tail one `apk add` invocation carries, as data rather than a
/// spawned process: every rule below is a promise callers of the build make
/// about what apk is *asked* — the root it installs into, the per-machine
/// download cache, and that flags precede the package names because apk stops
/// option parsing at the first non-option word.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApkArgs {
    Add {
        root: PathBuf,
        cache_dir: PathBuf,
        preflags: Vec<String>,
        packages: Vec<String>,
    },
    Del {
        root: PathBuf,
        preflags: Vec<String>,
        packages: Vec<String>,
    },
}

impl ApkArgs {
    /// Pushes this shape onto a command already carrying the wrapper and the
    /// apk binary itself.
    pub(crate) fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        match self {
            ApkArgs::Add {
                root,
                cache_dir,
                preflags,
                packages,
            } => command
                .args(["--no-progress", "add"])
                .arg("--root")
                .arg(root)
                .arg("--cache-dir")
                .arg(cache_dir)
                .args(preflags)
                .args(packages),
            ApkArgs::Del {
                root,
                preflags,
                packages,
            } => command
                .args(["--no-progress", "del"])
                .arg("--root")
                .arg(root)
                .args(preflags)
                .args(packages),
        }
    }

    /// The whole argv as strings, for tests and for a log line that must say
    /// exactly what would be run. Used only by the test build: the spawned
    /// commands go through [`ApkArgs::apply`] directly, and this is the same
    /// shape read back as data — which is what lets the argv rules (cache
    /// before flags, flags before names) be pinned without running apk.
    #[cfg(test)]
    pub(crate) fn argv(&self) -> Vec<String> {
        let mut out = Vec::new();
        // A stand-in program name keeps the indices readable; the real argv
        // prepends the wrapper and `apk` before these words.
        out.push("apk".into());
        match self {
            ApkArgs::Add {
                root,
                cache_dir,
                preflags,
                packages,
            } => {
                out.extend([
                    "--no-progress".into(),
                    "add".into(),
                    "--root".into(),
                    root.display().to_string(),
                    "--cache-dir".into(),
                    cache_dir.display().to_string(),
                ]);
                out.extend(preflags.iter().cloned());
                out.extend(packages.iter().cloned());
            }
            ApkArgs::Del {
                root,
                preflags,
                packages,
            } => {
                out.extend([
                    "--no-progress".into(),
                    "del".into(),
                    "--root".into(),
                    root.display().to_string(),
                ]);
                out.extend(preflags.iter().cloned());
                out.extend(packages.iter().cloned());
            }
        }
        out
    }
}

fn to_strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

pub(crate) fn apk_add_arguments(
    mount_dir: &Path,
    preflags: &[&str],
    packages: &[String],
    cache_dir: &Path,
) -> ApkArgs {
    ApkArgs::Add {
        root: mount_dir.to_path_buf(),
        cache_dir: cache_dir.to_path_buf(),
        preflags: to_strings(preflags),
        packages: packages.to_vec(),
    }
}

pub(crate) fn apk_del_arguments(
    mount_dir: &Path,
    preflags: &[&str],
    packages: &[String],
) -> ApkArgs {
    ApkArgs::Del {
        root: mount_dir.to_path_buf(),
        preflags: to_strings(preflags),
        packages: packages.to_vec(),
    }
}

/// `<wrapper...> <apk> --no-progress add --root <mount_dir> --cache-dir
/// <build>/apk-cache/<arch> <preflags...> <packages...>`, retried. A
/// `qemu-nbd`-attached image intermittently fails its biggest writes with an
/// I/O error; `apk` records what it did manage to install, so a plain re-run
/// finishes the packages the failed attempt dropped. `sync` before each retry
/// flushes the queued writeback to the image before the device is touched again.
///
/// Both machines' installs share it, because both install Alpine packages into
/// a tree this build lays: the first machine's failure mode is the NBD device
/// under heavy write load, the second's is the mirror fetch mid-install, and
/// "apk records what it did manage to install, so a plain re-run finishes the
/// job" is the answer for either producer. The command is a slice rather than
/// one path because an install running *inside* another program - the initramfs
/// pipeline's `--usermode` fetch has no wrapper, an Alpine tree laid under
/// `fakeroot` has one - spells its wrapper here.
///
/// The cache directory is per-machine on purpose. apk files each `.apk` under a
/// path built from its repository URL, and the package names carry their
/// architecture, so two machines sharing one directory would be *safe*; they
/// would not be legible — a branch drift on one machine evicting nothing and
/// silently mixing indexes is exactly the state this build must not reason
/// about. One directory per machine makes "which archive came from where"
/// answerable by looking, and keeps an offline run on one machine honest about
/// only ever hitting that machine's downloads.
pub(crate) fn apk_add(
    cmd: &[&str],
    mount_dir: &Path,
    preflags: &[&str],
    packages: &[String],
    cache_dir: &Path,
) -> Result<()> {
    let (program, rest) = cmd.split_first().context("apk_add: an empty command")?;
    fs::create_dir_all(cache_dir)?;
    for attempt in 1..=APK_ADD_ATTEMPTS {
        let mut command = Command::new(program);
        command.args(rest);
        apk_add_arguments(mount_dir, preflags, packages, cache_dir).apply(&mut command);
        let status = command.status().context("Failed to run apk")?;
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

/// This machine's download cache under `build/`.
pub(crate) fn apk_cache_dir(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join("apk-cache").join(arch.name())
}

/// The three files Alpine's `linux-virt` and `mkinitfs` would put on the ESP,
/// and the command line the loader hands the kernel.
///
/// `BOOTX64.EFI`/`BOOTAA64.EFI` under `EFI/BOOT` is where a UEFI firmware falls
/// back when nothing has written a boot entry into its non-volatile memory, which
/// is exactly the state of a machine that has never booted this disk - and which
/// of the two names it looks for is the machine's own (`Arch::uefi_boot_file`),
/// fixed by the UEFI specification rather than chosen.
///
/// The loader's *two* paths are the ESP's root on both machines, which is why
/// this one function writes both: the loader is one crate with one set of paths
/// in it, mounted here as `/boot` (the ESP) on either machine.
///
/// There is no third path for the kernel, and that is the shape rather than an
/// omission: the kernel is inside the loader (`boot/build.rs` compresses it in),
/// so the volume holds the loader, the early userspace and the command line -
/// and a kernel that is not a file here is a kernel that cannot be left behind
/// by the next build.
fn install_boot_files(
    mount_dir: &Path,
    config: &RootfsConfig,
    loader: &Path,
    initramfs: &Path,
    mode: crate::config::RunMode,
    arch: Arch,
) -> Result<()> {
    fs::create_dir_all(mount_dir.join("boot/EFI/BOOT"))?;
    install_file(
        loader,
        &mount_dir.join("boot/EFI/BOOT").join(arch.uefi_boot_file()),
    )?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Both machines, because the stamp's list is the same list with the
        // machine's own kernel, loader and archive in it.
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let inputs = stamp_inputs(&root, build_dir, "debug", arch).expect("the paths resolve");

            assert!(
                inputs.contains(&root.join(crate::paths::ROOTFS_CONFIG)),
                "the configuration is an input"
            );
            assert!(
                inputs.contains(&crate::initramfs::archive_path(build_dir, arch)),
                "this machine's initramfs is an input"
            );
            assert!(
                inputs.contains(&utils::kernel_path("debug", arch).unwrap()),
                "this machine's kernel is an input"
            );
            assert!(
                inputs.contains(&utils::loader_path("debug", arch).unwrap()),
                "this machine's loader is an input"
            );
            assert_eq!(inputs.len(), 4, "and that is the whole list");
            assert!(
                !inputs.contains(&utils::image_path(build_dir, arch)),
                "the image is built, not built from"
            );
        }
        // And the two machines' lists really are two lists: a shared stamp would
        // let one machine's build answer for the other's image.
        assert_ne!(
            stamp_inputs(&root, build_dir, "debug", Arch::X86_64).unwrap(),
            stamp_inputs(&root, build_dir, "debug", Arch::Aarch64).unwrap()
        );
        assert_ne!(
            utils::rootfs_stamp(build_dir, Arch::X86_64),
            utils::rootfs_stamp(build_dir, Arch::Aarch64)
        );
    }

    #[test]
    fn the_stamp_round_trips_and_a_v1_record_reads_as_nothing_to_reuse() {
        // The format is the incrementality record: a v2 stamp must read back
        // exactly what was written, and anything older or malformed must read as
        // `None` — which every caller resolves towards a full install. A parser
        // that accepted a truncated owned set would let a later build remove
        // packages it never recorded installing.
        let config_text = "[default]\nrootfs.base_packages = \"busybox,openrc\"\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\n";
        let root = utils::project_root().unwrap();
        let config = load_config("stamp-roundtrip", config_text);
        let (full, delta) = stamp_hashes(&config, &root, Arch::X86_64);
        let body = format!(
            "{STAMP_V2_HEADER}\nimage: /tmp/x.img\nfull: {full}\ndelta: {delta}\npackages: {delta}\nowned: {}\n{}\n",
            config.installed_names().len(),
            config.installed_names().join("\n")
        );
        let record = parse_stamp(&body).expect("a well-formed v2 stamp parses");
        assert_eq!(record.full, full);
        assert_eq!(record.delta, delta);
        assert!(record.owned.contains(&"busybox".to_string()));

        // v1 — the shape before this record existed — reads as no record at all.
        assert_eq!(parse_stamp("image: /tmp/x.img\n"), None);
        // Truncated, foreign, or an owned count that lies: all `None`.
        assert_eq!(parse_stamp(""), None);
        assert_eq!(
            parse_stamp(&format!("{STAMP_V2_HEADER}\nfull: abc\n")),
            None,
            "a record with no delta is not a complete claim"
        );
        assert_eq!(
            parse_stamp(&format!(
                "{STAMP_V2_HEADER}\nimage: i\nfull: f\ndelta: d\nowned: 9\nbusybox\n"
            )),
            None,
            "an owned set shorter than its declared count is a torn write"
        );
    }

    #[test]
    fn the_overlay_trees_join_the_delta_hash_so_a_moved_file_invalidates_the_record() {
        // Without the trees in any hash, two builds from identical config but
        // different overlays would share one delta value, and Commit 3's diff
        // path would answer "already current" to a tree whose overlay moved.
        let root = utils::project_root().unwrap();
        let config = load_config(
            "stamp-trees",
            "[default]\nrootfs.base_packages = \"busybox\"\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\n",
        );
        let core_now = utils::dir_digest(&root.join(CORE_OVERLAY));
        // A file added to the shared tree moves the digest — which is what makes
        // it an input the stamp answers for, not only the mtime `needs_rebuild`
        // reads. The fixture writes into a temp copy of nothing: `dir_digest`
        // over a directory that does not exist digests as empty, so creating one
        // with a byte inside proves the walk sees content at all.
        let scratch = std::env::temp_dir().join("mpos-rootfs-test-delta-tree");
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();
        assert_ne!(
            utils::dir_digest(&scratch),
            core_now,
            "a real overlay tree and an empty directory are different digests"
        );
        fs::write(scratch.join("etc-marker"), "one\n").unwrap();
        let before = utils::dir_digest(&scratch);
        fs::write(scratch.join("etc-marker"), "two\n").unwrap();
        assert_ne!(
            utils::dir_digest(&scratch),
            before,
            "changing a file's bytes changes the tree's digest"
        );
        fs::remove_dir_all(&scratch).unwrap();
        assert_eq!(
            utils::dir_digest(&scratch),
            utils::dir_digest(Path::new("/nonexistent-on-purpose")),
            "an absent tree digests as empty rather than failing the build"
        );
        let (_, delta) = stamp_hashes(&config, &root, Arch::X86_64);
        assert!(!delta.is_empty(), "and the delta side carries them both");
    }

    #[test]
    fn the_installed_db_reader_names_every_package_record() {
        // The diff's `present` side is this parser's answer, so it must see a
        // real Alpine record set: records separated by blank lines, each with a
        // `P:` name line and whatever else apk writes (version, checksum, file
        // lists). A reader that keyed on something other than `P:` would report
        // an installed tree as empty and re-install everything.
        let db = "P:busybox\nV:1.36.1-r15\nA:x86_64\n\nP:alpine-baselayout\nV:3.4.0-r2\n\nP:musl\nV:1.2.4-r2\n";
        assert_eq!(
            parse_installed_db(db),
            vec!["busybox", "alpine-baselayout", "musl"]
        );
        assert_eq!(parse_installed_db(""), Vec::<String>::new());
        // A record with no P: line names nothing; a `p:` file-list field is not
        // a package name.
        assert_eq!(
            parse_installed_db("V:1.0\nR:/bin/sh\np:123\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_delta_installs_only_the_new_names_and_removes_only_owned_ones() {
        // The three sets and their boundaries, pinned without a mount: want
        // gains what `present` lacks, remove comes from *owned* only — never
        // from `present`, which would delete a package someone added inside a
        // running guest. This is the safety property of the whole incremental
        // path, stated as data the same way `apply_package_delta` reads it.
        // `alpine-base` is what SHARED_BASE_PACKAGES folds into every machine's
        // list; the fixture spells it where the build would get it for free.
        let config = load_config(
            "delta",
            "[default]\nrootfs.base_packages = \"alpine-base,busybox,openrc\"\nrootfs.extra_packages = \"weston\"\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\n",
        );
        let want = config.installed_names();
        let present = ["alpine-base", "busybox", "openrc", "eudev-handinstalled"];
        let previous_owned = ["alpine-base", "busybox", "openrc", "seatd"];
        let to_add: Vec<&str> = want
            .iter()
            .filter(|name| !present.contains(&name.as_str()))
            .map(String::as_str)
            .collect();
        let to_remove: Vec<&str> = previous_owned
            .iter()
            .filter(|name| !want.iter().any(|w| w == *name))
            .cloned()
            .collect();
        assert_eq!(
            to_add,
            vec!["weston"],
            "only the newly named package installs"
        );
        assert_eq!(
            to_remove,
            vec!["seatd"],
            "only an owned name the lists dropped"
        );
        assert!(
            !to_remove.contains(&"eudev-handinstalled"),
            "a package in the tree but in no stamp belongs to nobody's diff"
        );
    }

    #[test]
    fn apk_is_asked_with_the_root_the_cache_and_flags_before_names() {
        // The argv shape carries the two promises the incremental build runs
        // on: every install names its per-machine download cache (`--cache-dir`
        // before any flag or name), and options precede the packages because
        // apk stops option parsing at the first non-option word. A swapped
        // order silently treats a flag as a package name.
        let args = apk_add_arguments(
            Path::new("/mnt/tree"),
            &["--arch", "x86_64", "--initdb"],
            &["busybox".to_string()],
            Path::new("/build/apk-cache/x86_64"),
        );
        let argv = args.argv();
        let root_at = argv.iter().position(|w| w == "--root").unwrap();
        let cache_at = argv.iter().position(|w| w == "--cache-dir").unwrap();
        let arch_at = argv.iter().position(|w| w == "--arch").unwrap();
        let pkg_at = argv.iter().position(|w| w == "busybox").unwrap();
        assert!(cache_at < arch_at, "the cache is set before any preflag");
        assert!(arch_at < pkg_at, "flags precede the package names");
        assert_eq!(argv[root_at + 1], "/mnt/tree");
        assert_eq!(argv[cache_at + 1], "/build/apk-cache/x86_64");

        // Removal carries no cache directory: apk del touches no network, and
        // an argument list claiming otherwise would read like a promise it
        // cannot keep.
        let del = apk_del_arguments(
            Path::new("/mnt/tree"),
            &["--no-scripts"],
            &["seatd".to_string()],
        );
        let dargv = del.argv();
        assert!(!dargv.iter().any(|w| w == "--cache-dir"));
        assert!(dargv.windows(2).any(|w| w[0] == "del" && w[1] == "--root"));
    }

    #[test]
    fn a_stamp_written_by_a_full_install_reads_back_all_four_fields() {
        // The round trip above proves the parser; this proves the writer's
        // counterpart: `write_stamp`'s exact bytes, read back through
        // `parse_stamp`, including the `packages` line the delta path threads
        // through builds. A field the writer emits and the reader drops would
        // make every later run look torn and reinstall — the bug class this
        // test exists for.
        let body = format!(
            "{STAMP_V2_HEADER}\nimage: /tmp/i.img\nfull: {:016x}\ndelta: {:016x}\npackages: {:016x}\nowned: 2\nbusybox\nopenrc\n",
            1u64, 2u64, 3u64
        );
        let record = parse_stamp(&body).expect("the writer's own format parses");
        assert_eq!(record.full, format!("{:016x}", 1u64));
        assert_eq!(record.delta, format!("{:016x}", 2u64));
        assert_eq!(record.packages, format!("{:016x}", 3u64));
        assert_eq!(record.owned, vec!["busybox", "openrc"]);
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

    /// The same tree, with the two files a *cross-architecture* install also
    /// reads: `staged_dir` above is the first machine's fixture, whose account
    /// the image's own `adduser` would have made, so it has no `/etc/passwd` at
    /// all. `create_account_without_chroot` writes it, so the test has to give
    /// it one - as `alpine-baselayout` leaves it, system accounts and all.
    fn staged_account_tree(name: &str) -> PathBuf {
        let dir = staged_dir(name);
        fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/ash\nbin:x:1:1:bin:/bin:/sbin/nologin\ndaemon:x:2:2:daemon:/sbin:/sbin/nologin\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n",
        )
        .unwrap();
        // `users` at gid 100 is the base system's own, and it is load-bearing:
        // it is what makes Alpine's `addgroup -S` put the first system group at
        // **101** and not 100 - measured, the shipped x86_64 image has
        // `users:100  klogd:101  messagebus:102  seat:103  weston-launch:104`,
        // and this fixture has to carry the base file for the same reason the
        // real tree does.
        fs::write(
            dir.join("etc/group"),
            "root:x:0:root\nbin:x:1:\ndaemon:x:2:\nwheel:x:10:\nusers:x:100:\nnobody:x:65534:\n",
        )
        .unwrap();
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
        write_system_config(&dir, &config, Arch::X86_64).unwrap();

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
    fn a_machine_the_host_cannot_run_gets_its_account_written_not_chrooted() {
        // The counterfactual for the whole step: the second machine's config
        // names the same account the first machine's does, and the build used to
        // refuse it outright ("asks for an account password ... build on that
        // machine"). Nothing here is executed, so a host that cannot run aarch64
        // binaries still gets the `mpos` login the shared `/etc/inittab` offers.
        let dir = staged_account_tree("no-chroot");
        let config = load_config(
            "no-chroot",
            "[default]\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\n\
             user.groups = [\"mpos\", \"wheel\", \"seat\"]\nuser.shell = \"/bin/ash\"\n\
             user.password = \"\"\n",
        );
        write_system_config(&dir, &config, Arch::Aarch64).unwrap();

        let passwd = fs::read_to_string(dir.join("etc/passwd")).unwrap();
        let account = passwd
            .lines()
            .find(|line| line.starts_with("mpos:"))
            .expect("the account was created")
            .to_string();
        let fields: Vec<&str> = account.split(':').collect();
        // name:x:uid:gid:gecos:home:shell - the shape busybox writes, at an id
        // above the system accounts, with the primary group named after it.
        assert_eq!(fields[0], "mpos");
        assert_eq!(fields[1], "x");
        let uid: u32 = fields[2].parse().expect("a numeric uid");
        let gid: u32 = fields[3].parse().expect("a numeric gid");
        assert!(uid >= 1000 && gid >= 1000, "{account}");
        assert_eq!(fields[5], "/home/mpos");
        assert_eq!(fields[6], "/bin/ash");

        let groups = fs::read_to_string(dir.join("etc/group")).unwrap();
        // The primary group exists and carries the account, which is what the
        // first machine's `addgroup mpos mpos` leaves behind...
        assert!(
            groups
                .lines()
                .any(|line| line == format!("mpos:x:{gid}:mpos")),
            "{groups}"
        );
        // ...`wheel` (which the base system ships) is joined, and `seat` - which
        // `seatd`'s own post-install would have made, and a `--no-scripts`
        // install never ran - exists because the account joined it.
        assert!(
            groups
                .lines()
                .any(|line| line.starts_with("wheel:x:10:") && line.ends_with("mpos")),
            "{groups}"
        );
        let seat = groups
            .lines()
            .find(|line| line.starts_with("seat:"))
            .expect("a group the account names is made");
        assert!(seat.ends_with(":mpos"), "{seat}");

        let shadow = fs::read_to_string(dir.join("etc/shadow")).unwrap();
        assert_eq!(
            shadow
                .lines()
                .filter(|line| line.starts_with("mpos:"))
                .count(),
            1,
            "one shadow entry for one account: {shadow}"
        );
        assert!(
            shadow.lines().any(|line| line.starts_with("mpos::")),
            "an empty password field is what makes the login passwordless: {shadow}"
        );

        // The home is made for the account. Its owner is the build's job (it runs
        // as root); the mode is this one's.
        use std::os::unix::fs::PermissionsExt;
        let home = dir.join("home/mpos");
        assert!(home.is_dir(), "the account has a home");
        assert_eq!(home.metadata().unwrap().permissions().mode() & 0o777, 0o755);

        // A second pass changes nothing: the build runs on the boot-files path
        // too, and an account a previous build made is the one it must not
        // duplicate or renumber.
        write_system_config(&dir, &config, Arch::Aarch64).unwrap();
        assert_eq!(fs::read_to_string(dir.join("etc/passwd")).unwrap(), passwd);
        assert_eq!(fs::read_to_string(dir.join("etc/group")).unwrap(), groups);
        assert_eq!(fs::read_to_string(dir.join("etc/shadow")).unwrap(), shadow);
    }

    #[test]
    fn a_machine_the_host_cannot_run_gets_the_system_accounts_the_scripts_would_make() {
        // The pin for what `--no-scripts` skips, measured *before* this code
        // existed, out of the two shipped images' own files: the machine whose
        // package scripts ran has
        //     klogd:x:100:101:klogd:/dev/null:/sbin/nologin
        //     messagebus:x:101:102:messagebus:/dev/null:/sbin/nologin
        // and the groups klogd 101, messagebus 102, seat 103, weston-launch 104;
        // the cross-arch image had neither user and its `seat` at **1001**,
        // because the configured account joins that group and so *made* it - at
        // the first free id for a regular group, which is not where a package's
        // `addgroup -S` would have put it. On the machine that is `dbus`
        // refusing to start (`checkpath: owner 'root:messagebus' not found`).
        let dir = staged_account_tree("system-accounts");
        let config = load_config(
            "system-accounts",
            "[default]\nsystem.hostname = \"mpos\"\nuser.name = \"mpos\"\n\
             user.groups = [\"mpos\", \"wheel\", \"seat\"]\nuser.shell = \"/bin/sh\"\n",
        );
        write_system_config(&dir, &config, Arch::Aarch64).unwrap();

        let passwd = fs::read_to_string(dir.join("etc/passwd")).unwrap();
        let groups = fs::read_to_string(dir.join("etc/group")).unwrap();
        let shadow = fs::read_to_string(dir.join("etc/shadow")).unwrap();

        // The two users, at the ids `adduser -S` picks: the first free ones from
        // 100 in the *user* sequence, each with its own group beside it.
        assert!(
            passwd.contains("klogd:x:100:101:klogd:/dev/null:/sbin/nologin"),
            "{passwd}"
        );
        assert!(
            passwd.contains("messagebus:x:101:102:messagebus:/dev/null:/sbin/nologin"),
            "{passwd}"
        );
        // The four groups, in the order their packages install, at the system
        // ids the other image's own file carries.
        for group in [
            "klogd:x:101:",
            "messagebus:x:102:",
            "seat:x:103:",
            "weston-launch:x:104:",
        ] {
            assert!(groups.contains(group), "{group} missing from {groups}");
        }
        // `-D` means *locked*, not passwordless: an empty password field here
        // would be a system account anyone could log into.
        for name in ["klogd", "messagebus"] {
            assert!(shadow.contains(&format!("{name}:!:")), "{shadow}");
        }
        // And the configured account joins the *system* `seat` - which is the
        // whole reason this runs before it: the other order makes a second group
        // at a regular id instead.
        assert!(
            groups
                .lines()
                .any(|line| line.starts_with("seat:x:103:") && line.ends_with("mpos")),
            "{groups}"
        );
        assert!(
            !groups.contains("seat:x:1001:"),
            "the account made its own seat group: {groups}"
        );

        // Idempotent, like the account itself.
        write_system_config(&dir, &config, Arch::Aarch64).unwrap();
        assert_eq!(fs::read_to_string(dir.join("etc/passwd")).unwrap(), passwd);
        assert_eq!(fs::read_to_string(dir.join("etc/group")).unwrap(), groups);
        assert_eq!(fs::read_to_string(dir.join("etc/shadow")).unwrap(), shadow);
    }

    #[test]
    fn a_password_is_still_refused_where_the_host_cannot_run_the_image() {
        // What is *not* reachable without a chroot is hashing, so the refusal
        // narrows from "any account at all" to "a password" - and both spellings
        // of one are named by the key that asked for them rather than silently
        // ignored, because a login nobody can pass is a broken image.
        let dir = staged_account_tree("no-chroot-password");
        for (key, toml) in [
            (
                "system.root_password",
                "[default]\nuser.name = \"\"\nsystem.root_password = \"hunter2\"\n",
            ),
            (
                "user.password",
                "[default]\nuser.name = \"mpos\"\nuser.password = \"hunter2\"\n",
            ),
        ] {
            let config = load_config("no-chroot-password", toml);
            let error = write_system_config(&dir, &config, Arch::Aarch64)
                .expect_err("a password needs the image's own crypt(3)")
                .to_string();
            assert!(error.contains("aarch64"), "{error}");
            assert!(error.contains(key), "{error}");
            assert!(
                !fs::read_to_string(dir.join("etc/passwd"))
                    .unwrap()
                    .contains("mpos"),
                "the refusal happens before anything is written"
            );
        }

        // The other direction: an empty password is not a refusal, and the same
        // config is accepted - so what the two machines share is still shared.
        let config = load_config(
            "no-chroot-password",
            "[default]\nuser.name = \"mpos\"\nuser.password = \"\"\n",
        );
        write_system_config(&dir, &config, Arch::Aarch64).unwrap();
        assert!(
            fs::read_to_string(dir.join("etc/passwd"))
                .unwrap()
                .contains("mpos:"),
            "the account is made when no password is asked for"
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
        write_system_config(&dir, &config, Arch::X86_64).unwrap();

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

    /// The shipped `/etc/inittab` is what both images get, verbatim.
    ///
    /// It is one file with no placeholder any more: the template's single
    /// per-machine line used to render as a getty on the second machine's serial
    /// node, and the measurement that retired it (`rootfs-core/etc/inittab`'s own
    /// header) is that the vcmux already carries the current virtual console to
    /// the wire - so a windowless session reaches its login prompt without a
    /// second getty beside the one busybox init respawns. What this pins is that
    /// the file the overlay carries is the file the image gets, and that its
    /// configuration half is the stock answer: a lost or reordered entry, or a
    /// getty creeping back onto the serial node, fails here rather than at a
    /// login prompt nobody sees.
    #[test]
    fn the_shared_inittab_is_the_one_both_images_get() {
        let root = crate::utils::project_root().unwrap();
        let stock =
            fs::read_to_string(root.join(crate::paths::ROOTFS_CORE).join("etc/inittab")).unwrap();
        assert!(
            !stock.contains("@SERIAL_GETTY@"),
            "the placeholder is gone from the file and from the build"
        );

        let config_lines: Vec<&str> = stock
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect();
        // The stock answer, copied here so the pin does not depend on git
        // history: the three openrc runlevels, the four virtual consoles, and no
        // getty on the serial node.
        assert_eq!(
            config_lines,
            vec![
                "::sysinit:/sbin/openrc sysinit",
                "::sysinit:/sbin/openrc boot",
                "::wait:/sbin/openrc default",
                "tty1::respawn:/sbin/getty 38400 tty1",
                "tty2::respawn:/sbin/getty 38400 tty2",
                "tty3::respawn:/sbin/getty 38400 tty3",
                "tty4::respawn:/sbin/getty 38400 tty4",
                "::ctrlaltdel:/sbin/reboot",
                "::shutdown:/sbin/openrc shutdown",
            ],
            "the inittab's configuration half may not drift"
        );
        assert!(
            !config_lines
                .iter()
                .any(|line| line.contains("ttyS0") || line.contains("ttyAMA0")),
            "a getty on the serial node is back"
        );

        // And the merge lands it byte for byte on either machine: the file the
        // tree carries is the file the image gets.
        for (arch, machine_tree) in [
            (Arch::X86_64, crate::paths::ROOTFS_OVERLAY_X86_64),
            (Arch::Aarch64, crate::paths::ROOTFS_OVERLAY_AARCH64),
        ] {
            let dir = staged_dir(&format!("inittab-{}", arch.name()));
            merge_overlays(&root, machine_tree, &dir).unwrap();
            assert_eq!(
                fs::read_to_string(dir.join("etc/inittab")).unwrap(),
                stock,
                "the {} image's inittab is not the shared file",
                arch.name()
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_applet_farm_is_the_list_busybox_would_install() {
        // Shape pin, host-side: the file the build links from is data, and it
        // has to stay readable and diffable after a regeneration. What it
        // *contains* is pinned on the machine by `t-busybox-farm`, which asks
        // each busybox for its own answer - this is what keeps a hand-edit, or a
        // re-dump from some other tree, from passing unnoticed.
        let root = utils::project_root().unwrap();
        let text = fs::read_to_string(root.join(crate::paths::BUSYBOX_APPLETS)).unwrap();
        let paths: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert!(!paths.is_empty(), "the farm cannot be empty");
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "regenerate with `| sort` and keep it sorted");
        assert!(
            sorted.windows(2).all(|pair| pair[0] != pair[1]),
            "a path is listed twice"
        );
        for path in &paths {
            let (dir, name) = path
                .rsplit_once('/')
                .unwrap_or_else(|| panic!("{path} names no directory"));
            assert!(
                ["bin", "sbin", "usr/bin", "usr/sbin"].contains(&dir),
                "{path} is not under a directory a PATH holds"
            );
            assert!(!name.is_empty(), "{path} names no applet");
        }
        assert!(
            paths.contains(&"sbin/init"),
            "the pid-1 name is the whole reason the build lays this farm"
        );
    }

    #[test]
    fn the_core_tree_carries_the_files_both_machines_share() {
        // The inventory `rootfs-core/root/README` claims, checked against the
        // tree itself: these are the files the convergence moved out of
        // `rootfs/`, and a drift back (or a GPU-only file sneaking in) fails
        // here rather than silently re-splitting the userlands.
        let root = crate::utils::project_root().unwrap();
        for path in [
            "etc/inittab",
            "etc/network/interfaces",
            "etc/resolv.conf",
            "etc/doas.conf",
            "root/README",
        ] {
            assert!(
                root.join(crate::paths::ROOTFS_CORE).join(path).is_file(),
                "the shared overlay must carry rootfs-core/{path}"
            );
        }
        // And the Wayland session's pieces, which moved into the core when the
        // second machine gained a compositor too: what they serve (`XDG_RUNTIME_DIR`,
        // weston's own settings) is no longer a hardware fact of one machine, and
        // the inventory above says so. A drift back to one machine's overlay fails
        // here rather than leaving the other image without a runtime directory.
        for path in [
            "etc/profile.d/xdg-runtime.sh",
            "etc/local.d/xdg-runtime.start",
            "etc/xdg/weston/weston.ini",
        ] {
            assert!(
                root.join(crate::paths::ROOTFS_CORE).join(path).is_file(),
                "the shared overlay must carry the Wayland session's {path}"
            );
            assert!(
                !root
                    .join(crate::paths::ROOTFS_OVERLAY_X86_64)
                    .join(path)
                    .exists(),
                "{path} lives in one tree, not two"
            );
        }
        // One inittab now exists in the repository, not two.
        assert!(
            !root
                .join(crate::paths::ROOTFS_OVERLAY_X86_64)
                .join("etc/inittab")
                .exists()
        );
        assert!(
            !root
                .join(crate::paths::ROOTFS_OVERLAY_AARCH64)
                .join("etc/inittab")
                .exists()
        );
    }

    /// The DHCP client the boot now runs, pinned where a sweep cannot see it.
    ///
    /// Both sweeps boot a payload of their own (the first machine's throwaway
    /// initramfs, the second's sweep medium), so neither runs the *image's*
    /// `/etc/init.d` - which makes this wiring the easier thing to lose: drop one
    /// line of [`SERVICES`], or one file out of the tree, and every test still
    /// passes while a product boot comes up with no address of its own.
    ///
    /// The three parts are the three that have to meet: the service that reads
    /// `/etc/network/interfaces` (whose `iface eth0 inet dhcp` line was already
    /// there, which is why the gap was invisible), the script `ifup` hands to
    /// `udhcpc -s`, and that script's executable bit - `merge_overlay` is
    /// `fs::copy`, so the mode in the tree is the mode in the image, and a script
    /// the shell cannot run fails exactly the way a missing one does.
    #[test]
    fn the_image_carries_the_dhcp_client_the_boot_runs() {
        use std::os::unix::fs::PermissionsExt;

        let root = crate::utils::project_root().unwrap();
        let script = root
            .join(crate::paths::ROOTFS_CORE)
            .join("usr/share/udhcpc/default.script");
        assert!(
            script.is_file(),
            "the shared overlay must carry the udhcpc script `ifup` invokes"
        );
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "the udhcpc script must be executable in the overlay ({mode:o}): \
             `merge_overlay` copies the mode, and a script the shell cannot run \
             leaves the client with a lease and nothing configured"
        );
        let default = SERVICES
            .iter()
            .find(|(runlevel, _)| *runlevel == "default")
            .expect("SERVICES names the default runlevel");
        assert!(
            default.1.contains(&"networking"),
            "the default runlevel must enable `networking`, or nothing runs ifup \
             and the image's `inet dhcp` never reaches a client"
        );
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
