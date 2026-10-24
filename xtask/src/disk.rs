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

/// Creates `arch`'s image unless it is already there, and the two fixtures a
/// boot of it attaches: the raw pattern image the second machine's block check
/// verifies, and the SATA disk its AHCI controller serves.
///
/// The pattern belongs to the *boot*, not to the image: the check that reads it
/// is the second machine's device probe (`check_blk`, `NOTES.md` §9.1), which
/// halts the machine when no block window carries the known sector, and the
/// first machine has no such check. It is written here rather than by each
/// caller because it is one 1 MiB file of a known content that both a sweep and
/// an interactive boot attach.
///
/// The SATA disk is not gated on `arch` while the pattern is, and that is the
/// difference between them: the pattern is read by a check only one machine
/// has, whereas the wiring hands *both* machines an AHCI controller, so a boot
/// of either one that had no disk behind it would be a boot whose `t-ahci`
/// failed for a reason that is not the driver's.
pub fn ensure(build_dir: &Path, profile: &str, arch: Arch) -> Result<()> {
    if !image_path(build_dir, arch).exists() {
        build_disk(build_dir, profile, arch)?;
    }
    if arch == Arch::Aarch64 {
        ensure_pattern_image(build_dir, arch)?;
    }
    ensure_sata_image(build_dir)?;
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
/// AAVMF with the loader - which carries the kernel inside it - this machine's
/// initramfs and the command line on it.
const ESP_BYTES: u64 = 48 * 1024 * 1024;

/// The raw block image the block-driver check uses as its scratch device. Every
/// byte is `i as u8` and nothing reads it for that value any more - the check
/// asks the kernel whether a window carries a filesystem and writes a pattern it
/// generates itself (`docs/NOTES.md` §15.34) - so what this file has to be is
/// *raw*: no partition table, no superblock, a device nothing will miss a sector
/// of.
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

/// The disk the machine's AHCI controller serves, and its size.
///
/// The name the SATA backing file carries under `build/`.
pub const SATA_TEST_IMAGE: &str = "test-sata.img";
/// The second disk, on a real controller rather than on virtio.
///
/// One file for *both* machines, unlike the pattern image above, and for the
/// opposite reason: nothing compares this disk's bytes against a value, so
/// there is no per-machine content to keep apart - and what the driver has to
/// be proved against is one machine's controller answering, which is the same
/// question on either.
pub fn sata_image(build_dir: &Path) -> PathBuf {
    build_dir.join(SATA_TEST_IMAGE)
}

/// The size the SATA backing file is laid at, by whoever lays it first.
///
/// 4 MiB of zeros is what `t-ahci`'s one-sector write ever needed; §10.5's
/// instrument then needs more — `t-ext4-mount` copies an 80 MiB ext4 fixture
/// onto this disk, and a device whose capacity is under the copy's length
/// refuses every write past its end (`check_span`), which would read as a
/// broken driver rather than an undersized fixture. So the number lives in one
/// constant both ends honour.
pub const SATA_TEST_BYTES: u64 = 96 * 1024 * 1024;

/// Writes the SATA image unless one of the right size is already there.
pub fn ensure_sata_image(build_dir: &Path) -> Result<()> {
    let path = sata_image(build_dir);
    if path.metadata().map(|m| m.len()).unwrap_or(0) == SATA_TEST_BYTES {
        return Ok(());
    }
    lay_zeros(&path)?;
    Ok(())
}

/// Lay the SATA disk a **sweep** boots, fresh: zeros, the shape every claim
/// about it is stated against.
///
/// [`ensure_sata_image`] creates the file when it is missing and otherwise
/// leaves it alone, which is right for a session — `cargo xtask run` boots the
/// disk a person may have written to, and their bytes are theirs. It is wrong
/// for a gate, because the programs that run against this disk *write* to it:
/// `t-ahci` a sector and a 16 KiB run, and §10.5's pair a whole filesystem
/// through the block door. Every claim they make about what is *around* what
/// they wrote ("and is still zero", "the sector before it was not written by the
/// long request") is a claim about a disk in a known state — and the file
/// outlives the run, so a sweep that inherited the last one's filesystem would
/// fail its own pins on the second run and pass on the third. That is the one
/// shape a gate must not have.
///
/// So a sweep lays its own disk before every boot, the way it lays its ESP and
/// its medium. Sparse on the host: a run that touches a few sectors pays for a
/// few sectors.
pub fn reset_sata_image(build_dir: &Path) -> Result<()> {
    fs::create_dir_all(build_dir)?;
    lay_zeros(&sata_image(build_dir))
}

/// Truncate a file to [`SATA_TEST_BYTES`] of zeros, creating it if needed.
fn lay_zeros(path: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("Failed to lay {}", path.display()))?;
    file.set_len(SATA_TEST_BYTES)
        .with_context(|| format!("Failed to size {}", path.display()))?;
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

/// The test machine's ext4 fixture: one filesystem image with the files
/// `t-diskbench` reads and `t-ext4-mount` mounts. Built once into
/// `build/test-sata.ext4.img`, **shared by both machines' sweeps** — unlike the
/// pattern image, which is per-machine because a check compares its bytes, this
/// fixture reaches each guest through that machine's own payload channel (the
/// x86_64 test initramfs carries it as an archive member; the aarch64 sweep
/// medium lays it on its root partition), so the guest never reads the host
/// file and there is no per-machine content to keep apart. A fresh sparse image
/// is what made every read of an unwritten extent cost a device read (§10.5's
/// instrument needs a disk that answers like a disk).
///
/// The tree it lays from is built by hand — mkfs's `-d` takes a host directory,
/// and this content is three files, not a distribution. The two generated files
/// are `(block + byte) % 251` per byte rather than stored, so a reader can check
/// *content* and not only speed, and a slot handed back for the wrong block
/// fails as data. **Two sizes, because the instrument asks two questions**: the
/// cache's own working set is 8 MiB (2048 slots of 4 KiB), so one file is
/// bigger than it by eightfold — a sequential scan must miss every block, and
/// the count says so — and one fits in half of it, so a second pass over *that*
/// file must cost no device reads at all. The first draft asked the second
/// question of the first file, where the answer is arithmetically forced to be
/// "no" (measured: cold 16387, warm 16384). The sizes are constants
/// `t-diskbench` names; changing either without the other is the drift this
/// comment pins in one place.
pub fn build_ext4_image(build_dir: &Path) -> Result<PathBuf> {
    sata_ext4_image(build_dir).existing_or_build(EXT4_FIXTURE_BYTES, || {
        let tree = build_dir.join("test-sata-staging");
        let _ = fs::remove_dir_all(&tree);
        fs::create_dir_all(&tree)?;
        let mut bytes: Vec<u8> = Vec::with_capacity(PATTERN_BYTES as usize);
        for i in 0..PATTERN_BYTES as usize {
            bytes.push((((i / 4096) as u64 + (i % 4096) as u64) % 251) as u8)
        }
        fs::write(tree.join("pattern.bin"), &bytes)?;
        let mut cached: Vec<u8> = Vec::with_capacity(CACHED_BYTES as usize);
        for i in 0..CACHED_BYTES as usize {
            cached.push((((i / 4096) as u64 + (i % 4096) as u64) % 251) as u8)
        }
        fs::write(tree.join("cached.bin"), &cached)?;
        fs::write(tree.join("hello.txt"), b"mpos ext4 test filesystem\n")?;
        // The fixture is its own volume, not a sweep medium's `ext4_image`: it
        // is copied onto the guest's SATA disk by `t-ext4-mount`, and the file
        // itself is what lands there.
        lay_ext4(&sata_ext4_image(build_dir), &tree, EXT4_FIXTURE_BYTES)?;
        let _ = fs::remove_dir_all(&tree);
        Ok(())
    })
}

/// The fixture's pattern file: 64 MiB, past the cache's 8 MiB by eightfold, so a
/// sequential scan misses every block whatever the policy does.
const PATTERN_BYTES: u64 = 64 * 1024 * 1024;

/// The fixture's *fitting* file: 4 MiB, half of the cache's 2048 slots, so a
/// second sequential pass over it is served from memory. The other half of the
/// instrument's question, and the reason it is a second file.
const CACHED_BYTES: u64 = 4 * 1024 * 1024;

/// The fixture volume's size: the two generated files plus 16 MiB of filesystem
/// metadata and slack (an `mkfs` that runs out of blocks answers "no blocks",
/// which reads as a build failure).
const EXT4_FIXTURE_BYTES: u64 = PATTERN_BYTES + CACHED_BYTES + 16 * 1024 * 1024;

/// The fixture's path under `build/`.
fn sata_ext4_image(build_dir: &Path) -> PathBuf {
    build_dir.join("test-sata.ext4.img")
}

/// One cached builder over a derived file: reuse the artifact when it is already
/// the declared size, otherwise run the closure that lays it and hand back its
/// path. Split out from [`build_ext4_image`] so the rule has a name and can be
/// pinned directly (`fixture_is_laid_once_and_reused`).
///
/// The identity is the **size**, the same rule [`ensure_sata_image`] uses for the
/// SATA backing file, and for the same reason: a fixture whose content is defined
/// by a constant has to be rebuilt when that constant moves, or a tree still
/// holding the old image runs against a shape the programs no longer describe.
/// "Exists" was that identity until the fixture gained its second file — an
/// 80 MiB image from an earlier build would then have answered a `t-diskbench`
/// that expects the 4 MiB `cached.bin` inside it, and the failure would read as
/// the kernel's.
trait ExistingOrBuild {
    fn existing_or_build(self, bytes: u64, build: impl FnOnce() -> Result<()>) -> Result<PathBuf>;
}

impl ExistingOrBuild for PathBuf {
    fn existing_or_build(self, bytes: u64, build: impl FnOnce() -> Result<()>) -> Result<PathBuf> {
        if self.metadata().map(|m| m.len()).unwrap_or(0) != bytes {
            build()?;
        }
        Ok(self)
    }
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
    // A symlink standing where the payload wants its program is *replaced*, not
    // written through. The applet farm puts links at 309 names (`lay_applet_farm`
    // below), and one of them is the name a test's runner wants - `sbin/init` -
    // so `fs::write` would follow the link out of the tree entirely and fail on
    // whoever owns its target (measured: `Permission denied`, the host's
    // `/bin/busybox`). The payload lands last by contract; this is what makes
    // "lands" true for a name that is currently a link.
    if path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        fs::remove_file(&path)
            .with_context(|| format!("Failed to replace the link at {}", path.display()))?;
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
pub fn sweep_medium(build_dir: &Path, into: &Path, arch: Arch, boot: &Boot<'_>) -> Result<PathBuf> {
    let Boot {
        // Not laid on the ESP any more - it is inside `loader` now - but still
        // read here for nothing and by `medium_is_stale` for the kernel's own
        // change, which is the reason the field is still part of this input.
        kernel: _,
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
    // Everything named below lands in `into`, which is the machine's own
    // medium directory: the sweep's single medium lays it in `build/`, and a
    // lane lays its own beside its own serial log, so two lanes' disks and the
    // disks their programs write to (the SATA file, the pattern image) cannot
    // meet. The *inputs* stay shared in `build/` - the initramfs below is one
    // archive however many media stage it.
    ensure_pattern_image(into, arch)?;
    // The SATA disk, for exactly the same reason: `virt_machine_args` attaches
    // an AHCI controller to every boot of the second machine, and a sweep on a
    // fresh tree has no `disk`/`rootfs` run behind it either. Laid *fresh*
    // rather than merely ensured, because this sweep's own programs write to it
    // (`reset_sata_image` says why the two are not the same question).
    reset_sata_image(into)?;
    let disk = medium(into, arch).with_extension("img");
    // Hoisted above the check because it is an *input* to it: `ensure_path` is
    // itself cached, and when it does rebuild the archive the new mtime is
    // exactly what makes the disk stale.
    let initramfs = crate::initramfs::ensure_path(build_dir, arch)?;
    let stamp = medium_stamp(into, arch);
    if !medium_is_stale(arch, &disk, &stamp, &initramfs, boot)? {
        println!(
            "disk: {} is up to date (its inputs are unchanged)",
            disk.display()
        );
        return Ok(disk);
    }

    let esp = esp_dir(into, arch);
    if esp.exists() {
        fs::remove_dir_all(&esp).with_context(|| format!("Failed to clear {}", esp.display()))?;
    }
    let boot_dir = esp.join("EFI/BOOT");
    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("Failed to create {}", boot_dir.display()))?;

    fs::copy(loader, boot_dir.join(arch.uefi_boot_file()))
        .with_context(|| format!("Failed to install {}", loader.display()))?;
    // And no `mpos.elf` beside it: the kernel travels *inside* that loader
    // (`boot/build.rs` compresses it into the loader's own image), which is what
    // makes this volume carry one file where it used to carry two. The kernel is
    // still an input to this medium - a changed kernel rebuilds the loader, and
    // the loader's bytes are what this stage copies - but it is no longer a file
    // anything opens at boot.
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
    let staging = ext4_staging(into, arch);
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
    // And the *full* farm, the one an installed image carries: a disk root
    // answers the same names a product one does, so a test that execs an applet
    // the archive never needed (`t-busybox-farm`, and any program reaching for
    // `/sbin/...`) meets the same tree here as on the machine's own root. One
    // list, laid by one function (`docs/NOTES.md` §15.34); a name already
    // answered - the runner staged below, a package's own link - is left alone.
    let project = crate::utils::project_root()?;
    crate::rootfs::lay_applet_farm(&project, &staging)?;
    // The overlay pair first, the payload *after* it. Order is the contract: the
    // machine overlay carries the disk's files (the mountpoints a real init
    // expects, `etc/inittab`) while a test boot's payload names some of the same
    // paths with its own programs. The payload landing last is what lets a
    // runner stand beside the tree rather than under it.
    crate::rootfs::merge_overlays(&project, arch.root_overlay(), &staging)?;
    for (name, bytes) in entries {
        stage_program(&staging, name, bytes)?;
    }
    lay_ext4(&ext4_image(into, arch), &staging, disk_bytes)?;

    // The ESP half: `mkfs.fat` makes the empty volume and `fat::populate` fills
    // it, which is the root-free way to do what `mcopy` or a mount would.
    let esp_img = esp_image(into, arch);
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
        &ext4_image(into, arch),
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

/// A digest of a staged program's bytes — the shared
/// [`crate::utils::content_digest`], so the sweep medium and the rootfs stamp
/// answer with one hash function and one set of semantics.
fn digest(bytes: &[u8]) -> u64 {
    crate::utils::content_digest(bytes)
}

/// True when the medium in `build/` is not the one this sweep is asking for.
///
/// A missing image or a missing stamp is stale - there is nothing to reuse - and
/// so is a fingerprint that no longer matches. Then the mtimes: the loader and
/// the initramfs are copied onto the ESP whole, and the toml plus the two overlay
/// trees become the root filesystem. The kernel is asked about as well even
/// though it is no longer a file on that volume: it is inside the loader, so a
/// kernel that moved makes a loader that moved, and asking about the kernel
/// directly is what still holds if that chain ever breaks.
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
fn lay_ext4(image: &Path, tree: &Path, disk_bytes: u64) -> Result<()> {
    let file =
        fs::File::create(image).with_context(|| format!("Failed to create {}", image.display()))?;
    file.set_len(disk_bytes)?;
    drop(file);

    let mut cmd = Command::new("mkfs.ext4");
    cmd.args(["-F", "-q", "-d"])
        .arg(tree)
        .args(["-O", "^has_journal,^metadata_csum"])
        .args(["-b", "4096"])
        .arg(image);
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

    /// §10.5's fixture is laid once and reused: the cached builder runs its
    /// closure only when the file is absent, and hands the same path back
    /// every time after. The counterfactual is a sweep that re-laid 64 MiB of
    /// pattern on every run - not wrong, but the cost this whole pipeline
    /// exists to refuse; and an *un*cached builder would be worse, because the
    /// x86_64 archive tail and the aarch64 medium both read the file through
    /// this one function and must see one image, not two generations of it.
    #[test]
    fn fixture_is_laid_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("mpos-fixture-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.img");

        let mut builds = 0;
        for _ in 0..3 {
            let p = path
                .clone()
                .existing_or_build(4, || {
                    builds += 1;
                    fs::write(&path, b"laid").map_err(|e| anyhow::anyhow!(e))
                })
                .unwrap();
            assert_eq!(p, path);
        }
        assert_eq!(builds, 1, "the closure ran more than once for one artifact");
        assert_eq!(fs::read(&path).unwrap(), b"laid");

        // And the identity is the *size*, so an artifact laid by an older
        // constant is replaced rather than used: the counterfactual is the
        // fixture gaining a file (the 4 MiB `cached.bin`) while a tree still
        // holds the image built before it, which a caller has no way to notice
        // - `t-diskbench` would read a file the image does not carry.
        let p = path
            .clone()
            .existing_or_build(9, || {
                builds += 1;
                fs::write(&path, b"relaid!!").map_err(|e| anyhow::anyhow!(e))
            })
            .unwrap();
        assert_eq!(p, path);
        assert_eq!(builds, 2, "a wrong-size artifact was reused");
        assert_eq!(fs::read(&path).unwrap(), b"relaid!!");
        let _ = fs::remove_dir_all(&dir);
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
