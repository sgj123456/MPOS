//! The `disk` command - the raw image file the rest of the build assembles
//! into - and the boot medium a *sweep* lays out of files.
//!
//! Two kinds of image live here, and the difference is where the content comes
//! from rather than which machine it is for:
//!
//! * the plain image `cargo xtask disk` creates, which `rootfs` then installs a
//!   userland into through the block layer (`qemu-nbd` + `mount`, which is why
//!   that half needs root). Both machines have one, and each has its own file
//!   (`Arch::disk_image`), because each holds its own root filesystem.
//! * [`sweep_medium`]: a complete GPT disk - ESP and ext4 root - assembled out
//!   of ordinary files, without a loop device, a mount or root. That is what a
//!   test sweep boots: the payload is a tree staged here, not an installed
//!   distribution, and it changes whenever a test program does. It used to be
//!   the second machine's private tool; both machines' *installs* no longer use
//!   it, so it is the sweep's alone.
//!
//! This is the `qemu-img create` half of `mkdisk.sh`, on its own: creating a file
//! needs no root, so the image starts life owned by the user and stays that way
//! through the rootfs build - formatting and mounting never change the owner of
//! the file being formatted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::arch::Arch;
use crate::config::RootfsConfig;
use crate::utils::{image_path, project_root};

pub fn build_disk(build_dir: &Path, _profile: &str, arch: Arch) -> Result<()> {
    let config = RootfsConfig::load(&project_root()?.join(crate::paths::ROOTFS_CONFIG), arch)?;
    let image = image_path(build_dir, arch);

    if image.exists() {
        println!(
            "disk: {} is already there ({})",
            image.display(),
            config.image_size
        );
        return Ok(());
    }

    println!("Creating the {} disk image", arch.name());
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

/// Creates `arch`'s image unless it is already there, and - because a boot of
/// the second machine reads it - the raw pattern image its block check verifies.
///
/// The pattern belongs to the *boot*, not to the image: the check that reads it
/// is the second machine's device probe (`check_blk`, `NOTES.md` §9.1), which
/// halts the machine when no block window carries the known sector, and the
/// first machine has no such check. It is written here rather than by each
/// caller because it is one 1 MiB file of a known content that both a sweep and
/// an interactive boot attach.
pub fn ensure(build_dir: &Path, profile: &str, arch: Arch) -> Result<()> {
    if !image_path(build_dir, arch).exists() {
        build_disk(build_dir, profile, arch)?;
    }
    if arch == Arch::Aarch64 {
        ensure_pattern_image(build_dir, arch)?;
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
/// `sudo` because it mounts what it made; a sweep's medium does not have to pay
/// for that, and this is the shape both machines' boot media take
/// (`NOTES.md` §15.18).
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
// A sweep's medium: one GPT disk, an ESP partition and an ext4 root, laid out
// of files.
//
// The files are the machine's own archive (`initramfs::unpack_into`), its
// overlays, its applet farm and the payload the sweep is running - no root
// needed anywhere, which is why a sweep is unprivileged on both machines.
// ---------------------------------------------------------------------------

/// The ESP partition's size, and why it is not smaller: FAT32 is defined to
/// need at least 65525 clusters, which at 512 bytes a sector is ~34 MiB, and a
/// smaller volume is not a FAT32 one. 48 MiB is the size measured to boot under
/// AAVMF with the loader, the kernel and this machine's initramfs on it.
const ESP_BYTES: u64 = 48 * 1024 * 1024;

/// The file a boot's filesystem check reads back through the VFS, and the bytes
/// it must find there.
///
/// Written into every root a sweep lays, and into the installed root of a
/// machine whose kernel reads it (`Arch::kernel_reads_root_probe`), so "the disk
/// mounted and served a file" is one line on the wire rather than the absence of
/// an error.
pub const PROBE_FILE: &str = "hello";
pub const PROBE_BYTES: &[u8] = b"mpos reads from ext4 over virtio-MMIO\n";

/// The raw block image the block-driver check reads a known pattern off. Every
/// byte is `i as u8`, so sector 0 reads back exactly as that check expects. It
/// is read, never mounted.
///
/// One file per machine, and one content for every boot of that machine: it is
/// a *fixture* the check compares bytes against, so two media sharing it is a
/// saving and not a collision.
pub fn blk_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-blk.img", arch.name()))
}

/// Writes the pattern image unless it is already there.
fn ensure_pattern_image(build_dir: &Path, arch: Arch) -> Result<()> {
    let path = blk_image(build_dir, arch);
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(build_dir)?;
    let pattern: Vec<u8> = (0..1024 * 1024u32).map(|i| i as u8).collect();
    fs::write(&path, &pattern).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// The name every file of `arch`'s sweep medium carries, under `build/`.
///
/// A medium of its own rather than the image `rootfs` installs into: the two are
/// *different disks* built from different content (an installed distribution
/// against this run's staged payload), and one file shared between them would
/// make every alternation between `cargo xtask run --arch aarch64` and `cargo
/// xtask test --arch aarch64` rebuild the disk the other command had just laid.
fn medium(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep", arch.name()))
}

/// The ext4 image the root partition is copied from.
fn ext4_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep-ext4.img", arch.name()))
}

/// The tree the ext4 half is laid from.
fn ext4_staging(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep-staging", arch.name()))
}

/// Where the medium's ESP volume is staged before it becomes a partition.
fn esp_dir(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep-esp", arch.name()))
}

/// The FAT image the ESP partition is copied from. A partition cannot be handed
/// over as a host path, so the volume QEMU used to synthesize out of a
/// directory has to be a real image once there is a table to put it in.
fn esp_image(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep-esp.fat", arch.name()))
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

/// One sweep boot's inputs: everything that decides *which* disk is built.
///
/// A struct rather than an eight-argument call because these travel together
/// from the caller that has them to the two functions that read them
/// ([`sweep_medium`], [`medium_is_stale`]), and a caller holding half of one
/// boot's inputs and half of another's is the bug this shape makes unspellable.
/// The fields mirror [`medium_fingerprint`]'s lines: `entries` and `disk_bytes`
/// and `cmdline` are exactly the parts of the image that have no file to stat,
/// and `kernel`/`loader` are the two inputs that do.
pub struct Boot<'a> {
    /// The kernel and its loader, as built for `arch`.
    pub kernel: &'a Path,
    pub loader: &'a Path,
    /// The payload: the programs the sweep's `/init` runs, laid on the root
    /// tree after the overlays so they win any name a distribution file also
    /// carries.
    pub entries: &'a [(&'a str, &'a [u8])],
    /// The root partition's size.
    pub disk_bytes: u64,
    /// The command line the kernel records and reads back.
    pub cmdline: &'a str,
}

/// The one disk a sweep boots.
///
/// The initramfs is not a caller's choice: every boot carries the real early
/// userspace the shared `initramfs` pipeline builds for this machine, because
/// the root switch lives in that archive's script on both machines, and a boot
/// without it would have nothing to do the switching.
///
/// This is *cached*: the filesystems behind the image are the expensive half of
/// preparing a boot and they change only when something that goes into them
/// does, so a boot whose inputs are unchanged boots the image already in
/// `build/`. The check fails *towards* rebuilding: anything it cannot prove is
/// unchanged is rebuilt.
pub fn sweep_medium(build_dir: &Path, arch: Arch, boot: &Boot<'_>) -> Result<PathBuf> {
    let Boot {
        kernel,
        loader,
        entries,
        disk_bytes,
        cmdline,
    } = *boot;
    // The block check's fixture, laid here rather than by the caller: the wiring
    // attaches it to *every* boot of this machine (`qemu::boot_args`), and a
    // sweep on a fresh tree has no `disk`/`rootfs` run behind it to have written
    // the file. It is a known 1 MiB pattern, so laying it once and reusing it is
    // the whole cost.
    ensure_pattern_image(build_dir, arch)?;
    let disk = medium(build_dir, arch).with_extension("img");
    // Hoisted above the check because it is an *input* to it: `ensure_path` is
    // itself cached, and when it does rebuild the archive the new mtime is
    // exactly what makes the disk stale.
    let initramfs = crate::initramfs::ensure_path(build_dir, arch)?;
    let stamp = medium_stamp(build_dir, arch);
    if !medium_is_stale(arch, &disk, &stamp, &initramfs, boot)? {
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
    let boot_dir = esp.join("EFI/BOOT");
    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("Failed to create {}", boot_dir.display()))?;

    fs::copy(loader, boot_dir.join(arch.uefi_boot_file()))
        .with_context(|| format!("Failed to install {}", loader.display()))?;
    fs::copy(kernel, esp.join("mpos.elf"))
        .with_context(|| format!("Failed to install {}", kernel.display()))?;
    // The command line the kernel records and reads back. A boot that carries a
    // real userland is not a test and says so by *leaving the word off*:
    // `mpos.selftest` is what lets the handoff inject a line of console input,
    // which an init is entitled to a console nobody has written to.
    fs::write(esp.join("mpos.cmdline"), cmdline)?;

    // The early userspace every boot carries: busybox, the applet farm and the
    // pivot script. The loader copies it into frames and records its address and
    // length in the handoff; the kernel unpacks it into a ramfs and runs its
    // `/init`. The sweep's payload *is* a tree under this root, so the archive
    // is unpacked onto the root partition below as well.
    fs::copy(&initramfs, esp.join("initramfs.tar"))
        .with_context(|| format!("Failed to install {}", initramfs.display()))?;
    println!("initramfs: {} {} -> ESP", arch.name(), initramfs.display());

    // The root filesystem a sweep boots: the archive unpacked, then the overlay
    // pair and the payload over it.
    let staging = ext4_staging(build_dir, arch);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .with_context(|| format!("Failed to create {}", staging.display()))?;
    // The archive, unpacked *first*, so everything below lands over it: this is
    // where `/bin/busybox`, the shell and the directories a mount needs come
    // from. A disk whose root is the same tree the ramfs half runs is the
    // one-flow shape rather than two trees agreeing by hand.
    crate::initramfs::unpack_into(build_dir, arch, &staging)?;
    // The farm on top of it: the archive carries the applets *its own scripts*
    // name, and a disk root is also what a test execs `/bin/true` and `/bin/ls`
    // from.
    crate::initramfs::link_farm(&staging, arch)?;
    fs::write(staging.join(PROBE_FILE), PROBE_BYTES)?;
    // The overlay pair first, the payload *after* it. Order is the contract: the
    // machine overlay carries the disk's `/init` - the post-switch pid-1 chain -
    // while a test boot's payload names some of the same paths with its own
    // programs. The payload landing last is what lets a runner stand beside the
    // script rather than under it; the overlay's remaining files
    // (`etc/inittab`) still arrive, because nothing in the payload names them.
    let project = crate::utils::project_root()?;
    crate::rootfs::merge_overlays(&project, arch.root_overlay(), &staging)?;
    for (name, bytes) in entries {
        stage_program(&staging, name, bytes)?;
    }
    lay_ext4(build_dir, arch, &staging, disk_bytes)?;

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
    fs::write(&stamp, medium_fingerprint(entries, disk_bytes, cmdline))?;

    Ok(disk)
}

/// The record beside a sweep's disk image that says what went into it.
///
/// A stamp file like the root filesystem install's, carrying more than an
/// existence proof. Most of what the image is made of is a file with an mtime,
/// and for those this file's own mtime does the work every other stamp in this
/// tree does. The rest - the command line, the image's size, and the programs
/// the caller stages as *bytes* rather than as paths - has no file to stat, so it
/// is written down here and compared as text.
///
/// That last group is why this is text at all. `entries` is what makes two boots
/// two different images: a sweep's disk carries every test program and a verdict
/// boot carries none, and both are laid at the one path. A stamp that only said
/// "a disk was built" would hand the second boot the first boot's payload.
fn medium_stamp(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-sweep.stamp", arch.name()))
}

/// The half of the medium's inputs that has no mtime, as text.
///
/// The staged bytes are digested rather than written out: a sweep stages a
/// hundred programs, and a stamp carrying them would be a second copy of the
/// image written in slow motion.
fn medium_fingerprint(entries: &[(&str, &[u8])], disk_bytes: u64, cmdline: &str) -> String {
    let mut out = format!("mpos {} sweep medium stamp v1\n", "boot");
    out.push_str(&format!("disk_bytes: {disk_bytes}\n"));
    out.push_str(&format!("cmdline: {cmdline:?}\n"));
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

/// True when the medium in `build/` is not the one this sweep is asking for.
///
/// A missing image or a missing stamp is stale - there is nothing to reuse - and
/// so is a fingerprint that no longer matches. Then the mtimes: the kernel, the
/// loader and the initramfs are copied onto the ESP whole, and the toml plus the
/// two overlay trees become the root filesystem.
///
/// This program is an input too, and it is the one entry that is not something
/// the image is made *of*: [`crate::fat`] writes the ESP's long filenames by
/// hand and learned three of their rules the hard way (`NOTES.md` §15.18), and a
/// change there changes the bytes on the disk while leaving every input above
/// untouched.
fn medium_is_stale(
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
        cmdline,
    } = *boot;
    if !disk.exists() || !stamp.exists() {
        return Ok(true);
    }
    if fs::read_to_string(stamp).unwrap_or_default()
        != medium_fingerprint(entries, disk_bytes, cmdline)
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
        project.join(crate::paths::ROOTFS_CONFIG),
        project.join(crate::rootfs::CORE_OVERLAY),
        project.join(arch.root_overlay()),
    ] {
        if crate::utils::newer(&input, stamp)? || crate::utils::dir_newer_than(&input, since)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Lay `tree` onto a bare ext4 image.
///
/// The features the in-tree driver cannot honour are switched off at creation,
/// not left to be discovered at mount: `has_journal` (nothing here replays a
/// journal, so a crash would leave the filesystem asking for a recovery this
/// kernel cannot do) and `metadata_csum` (every write would leave a checksum it
/// cannot recompute) - the same two the machines' installed roots turn off.
///
/// No `fakeroot` here, and that is a fact about who the tree's files are: a
/// sweep's payload is the build's own, written by this process as this user, and
/// nobody reads a permission bit off it beyond the executable ones
/// [`stage_program`] sets. An *installed* root - which is where ownership does
/// matter, because `login(1)` reads `/etc/shadow` - is not laid this way at all;
/// `rootfs` installs it through the block layer.
fn lay_ext4(build_dir: &Path, arch: Arch, tree: &Path, disk_bytes: u64) -> Result<()> {
    let ext4 = ext4_image(build_dir, arch);
    let file =
        fs::File::create(&ext4).with_context(|| format!("Failed to create {}", ext4.display()))?;
    file.set_len(disk_bytes)?;
    drop(file);

    let mut cmd = Command::new("mkfs.ext4");
    cmd.args(["-F", "-q", "-d"])
        .arg(tree)
        .args(["-O", "^has_journal,^metadata_csum"])
        .args(["-b", "4096"])
        .arg(&ext4);
    crate::utils::run_status(&mut cmd, "mkfs.ext4 the filesystem image")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sweep's medium and an installed image are two files, so alternating
    /// between `cargo xtask test --arch aarch64` and `cargo xtask run --arch
    /// aarch64` cannot make either one rebuild what the other laid.
    #[test]
    fn a_sweeps_medium_is_not_the_image_the_install_writes() {
        let build = Path::new("/tmp/mpos-build");
        assert_ne!(
            medium(build, Arch::Aarch64),
            image_path(build, Arch::Aarch64)
        );
        assert_ne!(
            medium_stamp(build, Arch::Aarch64),
            crate::utils::rootfs_stamp(build, Arch::Aarch64)
        );
        // And the two machines never share one: a sweep's medium carries the
        // payload built for its own machine's target.
        assert_ne!(medium(build, Arch::X86_64), medium(build, Arch::Aarch64));
    }

    /// A sweep's boot has its block check's fixture behind it.
    ///
    /// The counterfactual is a fresh tree: the wiring attaches the pattern image
    /// to every boot of the second machine (`qemu::boot_args`), and a sweep has
    /// no `disk`/`rootfs` run in front of it to have written the file - so a
    /// sweep that did not lay it itself would start a QEMU that refuses the
    /// `-drive` it was handed, which reads as a machine that hung.
    #[test]
    fn a_sweep_lays_the_pattern_image_its_boot_attaches() {
        let dir = std::env::temp_dir().join(format!("mpos-disk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = blk_image(&dir, Arch::Aarch64);
        assert!(!path.exists(), "the fixture starts absent");

        ensure_pattern_image(&dir, Arch::Aarch64).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 1024 * 1024);
        assert!(
            bytes.iter().enumerate().all(|(i, &b)| b == i as u8),
            "the pattern is what `check_blk` verifies sector 0 against"
        );
        // Idempotent: a second sweep must not rewrite a fixture the running
        // image was laid against.
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        ensure_pattern_image(&dir, Arch::Aarch64).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The fingerprint is what makes two *different* sweeps two different
    /// images: the payload has no mtime to compare, so a stamp that ignored the
    /// staged bytes would hand the second sweep the first one's programs.
    #[test]
    fn the_staged_bytes_are_part_of_what_the_stamp_records() {
        let a: &[u8] = b"one";
        let b: &[u8] = b"two";
        let one = medium_fingerprint(&[("bin/t-a", a)], 64, "console=x\n");
        assert_eq!(
            one,
            medium_fingerprint(&[("bin/t-a", a)], 64, "console=x\n")
        );
        assert_ne!(
            one,
            medium_fingerprint(&[("bin/t-a", b)], 64, "console=x\n")
        );
        assert_ne!(
            one,
            medium_fingerprint(&[("bin/t-b", a)], 64, "console=x\n")
        );
        assert_ne!(
            one,
            medium_fingerprint(&[("bin/t-a", a)], 128, "console=x\n")
        );
        assert_ne!(
            one,
            medium_fingerprint(&[("bin/t-a", a)], 64, "console=y\n")
        );
    }
}
