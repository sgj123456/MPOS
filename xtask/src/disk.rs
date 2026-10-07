//! The `disk` command: the raw image file the rest of the build assembles into.
//!
//! This is the `qemu-img create` half of `mkdisk.sh`, on its own: nothing here
//! needs root, because creating a file does not, so the image starts life owned
//! by the user and stays that way through the rootfs build - formatting and
//! mounting never change the owner of the file being formatted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::arch::Arch;
use crate::config::RootfsConfig;
use crate::utils::{image_path, project_root};

pub fn build_disk(build_dir: &Path, _profile: &str) -> Result<()> {
    let config = RootfsConfig::load(&project_root()?.join("rootfs.toml"), Arch::X86_64)?;
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

/// One 512-byte sector.
const SECTOR: u64 = 512;

/// Where the first partition starts.
///
/// 2048 sectors - one mebibyte - which is the alignment every modern tool
/// expects and the one `sfdisk` would choose on its own. Naming it rather than
/// reading it back out of the table is what lets the caller know the byte offset
/// it has to copy a filesystem to without parsing `sfdisk`'s output.
const FIRST_PARTITION: u64 = 2048;

/// Room left after the last partition for the GPT's backup copy, which lives at
/// the very end of the disk rather than beside the primary one.
const BACKUP_ROOM: u64 = 2048;

/// Assemble a GPT disk out of whole filesystem images: the ESP in partition 1
/// and the root in partition 2.
///
/// Everything here is a write to a plain file - `sfdisk` lays the table and each
/// filesystem is copied to its partition's byte offset - so this needs no loop
/// device, no mount and therefore no root. That is the difference from
/// `rootfs.rs`'s `create_gpt`, which is handed a block device and runs under
/// `sudo` because it mounts what it made; a machine's boot image does not have
/// to pay for that, and this is the shape both machines' boot media take
/// (`PLAN.md` §15.18).
///
/// The partition *types* are `U` (EFI System) and `L` (Linux), the two `sfdisk`
/// names, and the ESP is marked bootable - which is what makes a firmware that
/// finds no boot variable fall back to `EFI/BOOT/BOOT<ARCH>.EFI` on it.
pub fn lay_gpt(
    image: &Path,
    esp: &Path,
    esp_bytes: u64,
    root: &Path,
    root_bytes: u64,
) -> Result<()> {
    let esp_sectors = esp_bytes / SECTOR;
    let root_sectors = root_bytes / SECTOR;
    let root_start = FIRST_PARTITION + esp_sectors;
    let total = root_start + root_sectors + BACKUP_ROOM;

    let file =
        fs::File::create(image).with_context(|| format!("Failed to create {}", image.display()))?;
    file.set_len(total * SECTOR)
        .with_context(|| format!("Failed to size {}", image.display()))?;
    drop(file);

    // Explicit `start=` on both entries, so the offsets the copying below uses
    // are the ones this file wrote rather than whatever alignment a future
    // `sfdisk` would have picked.
    let table = format!(
        "label: gpt\n\
         start={FIRST_PARTITION}, name=efi, type=U, size={esp_sectors}, bootable\n\
         start={root_start}, name=system, type=L, size={root_sectors}\n"
    );
    let mut child = Command::new("sfdisk")
        .arg(image)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("Failed to run sfdisk")?;
    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .context("sfdisk has no standard input to write the table to")?;
        stdin
            .write_all(table.as_bytes())
            .context("Failed to write the partition table to sfdisk")?;
    }
    let status = child.wait().context("Failed to wait for sfdisk")?;
    if !status.success() {
        anyhow::bail!(
            "sfdisk could not write the partition table on {}",
            image.display()
        );
    }

    copy_into_partition(esp, image, FIRST_PARTITION, "the ESP")?;
    copy_into_partition(root, image, root_start, "the root filesystem")
}

/// Copy a whole filesystem image to one partition's offset, without disturbing
/// the table in front of it or the partition after it.
fn copy_into_partition(src: &Path, image: &Path, start: u64, what: &str) -> Result<()> {
    let status = Command::new("dd")
        .arg(format!("if={}", src.display()))
        .arg(format!("of={}", image.display()))
        .arg("bs=512")
        .arg(format!("seek={start}"))
        .args(["conv=notrunc", "status=none"])
        .status()
        .with_context(|| format!("Failed to run dd for {what}"))?;
    if !status.success() {
        anyhow::bail!("dd could not copy {what} into {}", image.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The boot media itself: one GPT disk, an ESP partition and an ext4 root.
//
// Both machines' boots are this shape now, so both are built here. What stays
// outside is the one step that genuinely differs between them - installing a
// userland (`rootfs`'s, which is where `apk` and its privileges are answered).
// ---------------------------------------------------------------------------

/// The ESP partition's size, and why it is not smaller: FAT32 is defined to
/// need at least 65525 clusters, which at 512 bytes a sector is ~34 MiB, and a
/// smaller volume is not a FAT32 one. 48 MiB is the size measured to boot under
/// AAVMF with the loader, the kernel and this machine's initramfs on it.
const ESP_BYTES: u64 = 48 * 1024 * 1024;

/// The file a boot's filesystem check reads back through the VFS, and the bytes
/// it must find there.
///
/// Written into every root this module lays, whatever else the tree carries, so
/// "the disk mounted and served a file" is one line on the wire rather than the
/// absence of an error.
pub const PROBE_FILE: &str = "hello";
pub const PROBE_BYTES: &[u8] = b"mpos reads from ext4 over virtio-MMIO\n";

/// Where a boot's ESP volume is staged before it becomes a partition.
pub fn esp_dir(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-esp", arch.name()))
}

/// The FAT image the ESP partition is copied from. A partition cannot be handed
/// over as a host path, so the volume QEMU used to synthesize out of a
/// directory has to be a real image once there is a table to put it in.
pub fn esp_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-esp.fat", arch.name()))
}

/// The ext4 image the root partition is copied from.
pub fn ext4_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-ext4.img", arch.name()))
}

/// The raw block image the block-driver check reads a known pattern off. Every
/// byte is `i as u8`, so sector 0 reads back exactly as that check expects. It
/// is read, never mounted.
pub fn blk_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-blk.img", arch.name()))
}

/// The tree the ext4 half is laid from, when the root is not an installed
/// userland.
fn ext4_staging(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-ext4-staging", arch.name()))
}

/// Lay one payload file on a staging tree, creating any directory its name
/// carries and marking it executable.
///
/// The name may hold a directory: a test boot's programs live in `/bin`, where
/// the shared runner script walks for them. And the mode is set *here* because
/// `mkfs.ext4 -d` copies what it finds: a file written mode 0644 lands on the
/// disk non-executable, and `/bin/true` would then answer `EACCES` at
/// `execve` - a failure that reads as a broken program rather than a staging
/// step that forgot.
pub fn stage_program(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("Failed to lay {name} on the disk"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to chmod +x {name} on the disk"))?;
    Ok(())
}

/// One boot's inputs: everything that decides *which* disk is built.
///
/// A struct rather than an eight-argument call because these travel together
/// from the caller that has them to the two functions that read them
/// ([`boot_media`], [`disk_is_stale`]), and a caller holding half of one boot's
/// inputs and half of another's is the bug this shape makes unspellable. The
/// fields mirror [`disk_fingerprint`]'s lines: `entries` and `disk_bytes` and
/// `cmdline` and `root` are exactly the parts of the image that have no file to
/// stat, and `kernel`/`loader` are the two inputs that do.
pub struct Boot<'a> {
    /// The kernel and its loader, as built for `arch`.
    pub kernel: &'a Path,
    pub loader: &'a Path,
    /// The payload: the programs a test boot's `/init` runs, laid on the root
    /// tree after the overlays so they win any name a distribution file also
    /// carries.
    pub entries: &'a [(&'a str, &'a [u8])],
    /// The root partition's size.
    pub disk_bytes: u64,
    /// An already-installed userland to use as the root instead of the in-tree
    /// tree; `None` gets the shared overlay pair plus the payload.
    pub root: Option<&'a Path>,
    /// The command line the kernel records and reads back.
    pub cmdline: &'a str,
}

/// The one disk a machine boots.
///
/// The initramfs is no longer a caller's choice: every boot carries the real
/// early userspace the shared `initramfs` pipeline builds for this machine,
/// because the root switch lives in that archive's script on both machines now,
/// and a boot without it would have nothing to do the switching.
///
/// This is *cached*: the filesystems behind the image are the expensive half of
/// preparing a boot and they change only when something that goes into them
/// does, so a boot whose inputs are unchanged boots the image already in
/// `build/` (`PLAN.md` §15.18). The check fails *towards* rebuilding: anything
/// it cannot prove is unchanged is rebuilt.
pub fn boot_media(build_dir: &Path, arch: Arch, boot: &Boot<'_>) -> Result<PathBuf> {
    let Boot {
        kernel,
        loader,
        entries,
        disk_bytes,
        root,
        cmdline,
    } = *boot;
    let disk = build_dir.join(arch.disk_image());
    // Hoisted above the check because it is an *input* to it: `ensure_path` is
    // itself cached, and when it does rebuild the archive the new mtime is
    // exactly what makes the disk stale.
    let initramfs = crate::initramfs::ensure_path(build_dir, arch)?;
    let stamp = disk_stamp(build_dir, arch);
    if !disk_is_stale(arch, &disk, &stamp, &initramfs, boot)? {
        println!(
            "disk: {} is up to date (its inputs are unchanged)",
            disk.display()
        );
        return Ok(disk);
    }

    let esp = esp_dir(build_dir, arch);
    if esp.exists() {
        fs::remove_dir_all(&esp).with_context(|| format!("Failed to clear {}", esp.display()))?;
    }
    let boot = esp.join("EFI/BOOT");
    fs::create_dir_all(&boot).with_context(|| format!("Failed to create {}", boot.display()))?;

    fs::copy(loader, boot.join(arch.uefi_boot_file()))
        .with_context(|| format!("Failed to install {}", loader.display()))?;
    fs::copy(kernel, esp.join("mpos.elf"))
        .with_context(|| format!("Failed to install {}", kernel.display()))?;
    // The command line the kernel records and reads back. A boot that carries a
    // real userland is not a test and says so by *leaving the word off*:
    // `mpos.selftest` is what lets the handoff inject a line of console input,
    // which an init is entitled to a console nobody has written to.
    fs::write(esp.join("mpos.cmdline"), cmdline)?;

    // The early userspace every boot carries: busybox, the applet farm and the
    // pivot script. Installed on the Alpine boot's volume too, because the
    // archive is what has a userland to run *before* the disk is reached, and
    // reaching the disk is running that userland's `/init`. The loader copies
    // it into frames and records its address and length in the handoff; the
    // kernel unpacks it into a ramfs and runs its `/init`.
    fs::copy(&initramfs, esp.join("initramfs.tar"))
        .with_context(|| format!("Failed to install {}", initramfs.display()))?;
    println!("initramfs: {} {} -> ESP", arch.name(), initramfs.display());

    // The raw block image the block-driver check reads its pattern off.
    let pattern: Vec<u8> = (0..1024 * 1024u32).map(|i| i as u8).collect();
    fs::write(blk_image(build_dir, arch), &pattern)
        .with_context(|| format!("Failed to write {}", blk_image(build_dir, arch).display()))?;

    // The root filesystem: either the installed userland, or a tree laid here
    // from the archive, the shared overlay pair and the payload.
    let staging;
    let tree: &Path = match root {
        Some(root) => root,
        None => {
            staging = ext4_staging(build_dir, arch);
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging)
                .with_context(|| format!("Failed to create {}", staging.display()))?;
            // The archive, unpacked *first*, so everything below lands over it:
            // this is where `/bin/busybox`, the shell and the directories a
            // mount needs come from. A disk whose root is the same tree the
            // ramfs half runs is the one-flow shape rather than two trees
            // agreeing by hand.
            crate::initramfs::unpack_into(build_dir, arch, &staging)?;
            // The farm on top of it: the archive carries the applets *its own
            // scripts* name, and a disk root is also what a test execs
            // `/bin/true` and `/bin/ls` from.
            crate::initramfs::link_farm(&staging, arch)?;
            fs::write(staging.join(PROBE_FILE), PROBE_BYTES)?;
            // The overlay pair first, the payload *after* it. Order is the
            // contract: the machine overlay carries the disk's `/init` - the
            // post-switch pid-1 chain - while a test boot's payload names some
            // of the same paths with its own programs. The payload landing last
            // is what lets a runner stand beside the script rather than under
            // it; the overlay's remaining files (`etc/inittab`) still arrive,
            // because nothing in the payload names them.
            let project = crate::utils::project_root()?;
            crate::rootfs::merge_overlays(
                &project,
                arch.root_overlay(),
                &staging,
                &RootfsConfig::load(&project.join("rootfs.toml"), arch)?,
                false,
            )?;
            for (name, bytes) in entries {
                stage_program(&staging, name, bytes)?;
            }
            &staging
        }
    };
    lay_ext4(build_dir, arch, tree, disk_bytes, root.is_some())?;

    // The ESP half: `mkfs.fat` makes the empty volume and `fat::populate` fills
    // it, which is the root-free way to do what `mcopy` or a mount would.
    let esp_img = esp_image(build_dir, arch);
    if esp_img.exists() {
        fs::remove_file(&esp_img)
            .with_context(|| format!("Failed to clear {}", esp_img.display()))?;
    }
    let file = fs::File::create(&esp_img)
        .with_context(|| format!("Failed to create {}", esp_img.display()))?;
    file.set_len(ESP_BYTES)?;
    drop(file);
    crate::utils::run_status(
        Command::new("mkfs.fat")
            .args(["-F32", "-n", "MPSESP"])
            .arg(&esp_img),
        "mkfs.fat the ESP",
    )?;
    crate::fat::populate(&esp_img, &esp)?;

    lay_gpt(
        &disk,
        &esp_img,
        ESP_BYTES,
        &ext4_image(build_dir, arch),
        disk_bytes,
    )?;

    // Written last, so the mtime the next run reads as its "since" is strictly
    // after everything this build laid down - the image, the ext4 half and the
    // ESP half all finish above. A stamp written first would measure its own
    // build as already done.
    fs::write(&stamp, disk_fingerprint(entries, disk_bytes, cmdline, root))?;

    Ok(disk)
}

/// The record beside the disk image that says what went into it.
///
/// A stamp file like the Alpine install's, carrying more than an existence
/// proof. Most of what the image is made of is a file with an mtime, and for
/// those this file's own mtime does the work every other stamp in this tree
/// does. The rest - the command line, the image's size, and the programs the
/// caller stages as *bytes* rather than as paths - has no file to stat, so it
/// is written down here and compared as text.
///
/// That last group is why this is text at all. `entries` is what makes two
/// boots two different images: the sweep's disk carries every test program and
/// the verdict's carries none, and both are laid at the one path
/// [`Arch::disk_image`]. A stamp that only said "a disk was built" would hand
/// the second boot the first boot's payload.
fn disk_stamp(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-disk.stamp", arch.name()))
}

/// The half of the disk's inputs that has no mtime, as text.
///
/// The staged bytes are digested rather than written out: the sweep stages a
/// hundred programs, and a stamp carrying them would be a second copy of the
/// image written in slow motion.
fn disk_fingerprint(
    entries: &[(&str, &[u8])],
    disk_bytes: u64,
    cmdline: &str,
    root: Option<&Path>,
) -> String {
    let mut out = format!("mpos {} disk stamp v1\n", "boot");
    out.push_str(&format!("disk_bytes: {disk_bytes}\n"));
    out.push_str(&format!("cmdline: {cmdline:?}\n"));
    // Which *kind* of root, not which tree: the tree's own mtimes are what say
    // whether it moved, and its path under `build/` would say nothing.
    out.push_str(&format!("installed root: {}\n", root.is_some()));
    out.push_str(&format!("entries: {}\n", entries.len()));
    for (name, bytes) in entries {
        out.push_str(&format!(
            "  {name} {} {:016x}\n",
            bytes.len(),
            digest(bytes)
        ));
    }
    out
}

/// A digest of a staged program's bytes.
///
/// `DefaultHasher` is not a checksum and is not promised to hash the same way
/// across compiler versions. It does not need to be: a digest that changes when
/// it should not costs one rebuild, and one rebuild is the direction every
/// ambiguity here is resolved towards.
fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(bytes, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// True when the disk in `build/` is not the one this boot is asking for.
///
/// A missing image or a missing stamp is stale - there is nothing to reuse -
/// and so is a fingerprint that no longer matches. Then the mtimes: the kernel,
/// the loader and the initramfs are copied onto the ESP whole, and the toml
/// plus the two overlay trees become the root filesystem. An installed root is
/// read *as a tree* and not through its own stamp: that one is rewritten on
/// every run whether or not `apk` ran, so it records when a build last looked
/// at the tree and says nothing about whether the tree moved.
///
/// This program is an input too, and it is the one entry that is not something
/// the image is made *of*: [`crate::fat`] writes the ESP's long filenames by
/// hand and learned three of their rules the hard way (`PLAN.md` §15.18), and a
/// change there changes the bytes on the disk while leaving every input above
/// untouched.
fn disk_is_stale(
    arch: Arch,
    disk: &Path,
    stamp: &Path,
    initramfs: &Path,
    boot: &Boot<'_>,
) -> Result<bool> {
    let Boot {
        kernel,
        loader,
        entries,
        disk_bytes,
        root,
        cmdline,
    } = *boot;
    if !disk.exists() || !stamp.exists() {
        return Ok(true);
    }
    if fs::read_to_string(stamp).unwrap_or_default()
        != disk_fingerprint(entries, disk_bytes, cmdline, root)
    {
        return Ok(true);
    }
    let since = crate::utils::file_mtime(stamp)?;
    for input in [kernel, loader, initramfs] {
        if !input.exists() || crate::utils::file_mtime(input)? > since {
            return Ok(true);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && exe.exists()
        && crate::utils::file_mtime(&exe)? > since
    {
        return Ok(true);
    }
    let project = crate::utils::project_root()?;
    for input in [
        project.join("rootfs.toml"),
        project.join(crate::rootfs::CORE_OVERLAY),
        project.join(arch.root_overlay()),
    ] {
        if crate::utils::newer(&input, stamp)? || crate::utils::dir_newer_than(&input, since)? {
            return Ok(true);
        }
    }
    if let Some(tree) = root
        && crate::utils::dir_newer_than(tree, since)?
    {
        return Ok(true);
    }
    Ok(false)
}

/// Lay `tree` onto a bare ext4 image.
///
/// The features the in-tree driver cannot honour are switched off at creation,
/// not left to be discovered at mount: `has_journal` (nothing here replays a
/// journal, so a crash would leave the filesystem asking for a recovery this
/// kernel cannot do) and `metadata_csum` (every write would leave a checksum it
/// cannot recompute) - the same two the first machine's root turns off.
///
/// `fakeroot` is for an installed userland, and it is not optional there:
/// `mke2fs -d` records each file's owner by *stat`ing it, and such a tree was
/// installed under `fakeroot` - so its files are really owned by whoever ran
/// the build and only *appear* root-owned inside a fakeroot session. Running
/// `mke2fs` outside one therefore wrote a root filesystem owned by a uid that
/// is not root, which is wrong in a way that shows up as "login cannot read
/// /etc/shadow". Inside a fakeroot session every `stat` answers `0:0`, so the
/// image comes out the way `apk` intended. The in-tree payloads need none of
/// this - their files are the build's own and nobody reads a permission bit off
/// them.
fn lay_ext4(
    build_dir: &Path,
    arch: Arch,
    tree: &Path,
    disk_bytes: u64,
    fakeroot: bool,
) -> Result<()> {
    let ext4 = ext4_image(build_dir, arch);
    let file =
        fs::File::create(&ext4).with_context(|| format!("Failed to create {}", ext4.display()))?;
    file.set_len(disk_bytes)?;
    drop(file);

    // The same `mkfs.ext4` either way; under `fakeroot` it is that program's
    // child rather than this process's, which is the whole of the difference.
    let mkfs = |cmd: &mut Command| {
        cmd.args(["-F", "-q", "-d"])
            .arg(tree)
            .args(["-O", "^has_journal,^metadata_csum"])
            .args(["-b", "4096"])
            .arg(&ext4);
    };
    let mut cmd = if fakeroot {
        let mut c = Command::new("fakeroot");
        c.arg("--").arg("mkfs.ext4");
        c
    } else {
        Command::new("mkfs.ext4")
    };
    mkfs(&mut cmd);
    crate::utils::run_status(&mut cmd, "mkfs.ext4 the filesystem image")
}
