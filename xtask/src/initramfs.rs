//! The `initramfs` command: the early userspace the kernel unpacks as its first
//! root filesystem. This is `make_initramfs` from `scripts/mkdisk.sh`.
//!
//! The kernel does not mount the disk itself. It unpacks this archive over a
//! ramfs, runs `/init` out of it, and that script mounts the real root
//! filesystem and pivots into it (`NOTES.md` §15.6). So the tree needs exactly
//! three things: a shell, the tools to mount with, and the script.
//!
//! # One pipeline, two machines
//!
//! Both machines build this archive the same way - repositories, apk, the
//! busybox applet farm, the overlays, ustar - and this module is that one copy.
//! What differs is [`Machine`]: which toml describes it, which overlay tree is
//! its own half, where the staging tree and the archive live, whether the
//! package set comes from the toml or is pinned, how the applet farm is laid,
//! and whether a stamp records when the tree was laid. What does *not* differ is
//! the `/init` script: it is the shared core's, one file for both machines
//! ([`CORE_OVERLAY`]).
//!
//! The module was two files until this became one: `aarch64_initramfs.rs` was
//! the second machine's copy of this pipeline, and the two shapes are the same
//! one written twice (`TEST-AND-BOOT-REFACTOR.md` §2's third target). The single
//! exception to "the machine is data" is the applet farm, and it is a real
//! machine fact rather than a preference: see [`Farm`].

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::arch::Arch;
use crate::config::InitramfsConfig;
use crate::utils;

/// How the busybox applet farm gets laid in the staging tree.
///
/// This is the one thing in the pipeline that cannot be data, and the reason is
/// measured rather than stylistic. `busybox --install -s` is the honest way to
/// make the farm - one pass, every applet, the binary's own list - and it has to
/// run as *the tree's* busybox so the links point where the guest will look for
/// them. Running it means executing it, and the x86_64 build can (its host is
/// its own architecture) while the aarch64 build cannot: a cross-arch `execve`
/// is the `Exec format error` the Alpine install records. So the second machine
/// declares the applets its own boot scripts exec and links them by hand.
///
/// This is the *archive*'s farm, and it is deliberately short: the archive's
/// `/init` (the shared core script) needs a shell, a `mount` and a
/// `switch_root`, and everything else a boot execs comes from the *disk*, whose
/// full farm the build lays from `busybox --list-full`
/// (`rootfs::lay_applet_farm`).
enum Farm {
    /// `chroot <staging> /bin/busybox --install -s` - the host can exec the tree.
    Chroot,
    /// Link each named applet to `/bin/busybox`, skipping names that already
    /// answer: `alpine-baselayout` ships some of them, and replacing a real file
    /// with a link back to busybox would lose content the script reads.
    ByHand(&'static [&'static str]),
}

/// How apk is asked for the packages.
///
/// Two shapes because the second machine's install needed two things the first
/// machine's did not, and both are measurements rather than conveniences:
///
/// * `--usermode`, because a non-root build cannot write apk's database without
///   it ("Use --usermode to allow creating database as non-root"). It is refused
///   under root ("ERROR: --usermode not allowed as root"), so it is added only
///   when the build is not root.
/// * a retry around the fetch, because the failure mode here is the mirror and a
///   re-run resumes from what apk already recorded.
///
/// The first machine keeps its single bare invocation: adopting the retry there
/// would be a behaviour change to a path this module was not asked to move.
enum Apk {
    /// One bare `apk add`, no retry - the x86_64 build's shape.
    Direct,
    /// `--usermode` when not root, wrapped in `rootfs::apk_add`'s retry.
    UsermodeRetry,
}

/// What differs between the two machines' archives.
struct Machine {
    arch: Arch,
    /// The toml this archive is described by: mirror, branch, repositories and
    /// (unless pinned) the package set.
    config: &'static str,
    /// The machine's own overlay tree, applied last - after the shared core, so
    /// a machine may still replace anything the core carries. Empty today, and
    /// that is a value rather than a case: the `/init` script is the core's, and
    /// a directory that does not exist merges to nothing (`merge_overlay`'s
    /// documented answer).
    overlay: &'static str,
    /// The staging tree under `build/`.
    staging: &'static str,
    /// Directories made in the staging tree before apk runs, for the mounts the
    /// init script lands on. The first machine's toml carries no filesystem, so
    /// its tree has to declare them; the second machine's `alpine-baselayout`
    /// ships them, and declaring them again would only add empty members to an
    /// archive whose whole point is being small.
    dirs: &'static [&'static str],
    /// The packed archive under `build/`. Named apart per machine so neither
    /// boot can read the other's.
    tar: &'static str,
    /// Written after a successful pack so an overlay edit landing *after* a
    /// rebuild still asks for another one. `None` leaves the question to the
    /// archive's own mtime against the config and the tree.
    stamp: Option<&'static str>,
    /// The package set, or `None` for the toml's `base_packages` +
    /// `extra_packages`. A pinned set is what the second machine declares: its
    /// archive needs exactly a shell, `mount`, `switch_root` and `pivot_root`,
    /// and every name added costs the boot a file it does not need.
    packages: Option<&'static [&'static str]>,
    farm: Farm,
    apk: Apk,
}

const X86_64: Machine = Machine {
    arch: Arch::X86_64,
    config: crate::paths::INITRAMFS_CONFIG,
    overlay: crate::paths::INITRAMFS_OVERLAY_X86_64,
    staging: "initramfs",
    dirs: &["etc/apk/keys", "proc", "sys", "dev", "sysroot"],
    tar: "initramfs.tar",
    stamp: None,
    packages: None,
    farm: Farm::Chroot,
    apk: Apk::Direct,
};

const AARCH64: Machine = Machine {
    arch: Arch::Aarch64,
    // One file for both machines now: which half is read is `[default]` plus
    // `[aarch64]`, resolved by `config::parse_sectioned_toml`.
    config: crate::paths::INITRAMFS_CONFIG,
    overlay: crate::paths::INITRAMFS_OVERLAY_AARCH64,
    staging: "aarch64-initramfs",
    dirs: &["etc/apk/keys"],
    tar: "aarch64-initramfs.tar",
    stamp: Some("aarch64-initramfs.stamp"),
    // `alpine-baselayout` is there for the directories a mount wants to land on
    // (`/dev`, `/proc`, `/sys` exist before the script mounts over them);
    // busybox is everything else.
    packages: Some(&["busybox", "alpine-baselayout"]),
    farm: Farm::ByHand(AARCH64_APPLETS),
    apk: Apk::UsermodeRetry,
};

/// The applets the second machine's initramfs scripts exec, besides the shell.
/// `switch_root` is the pivot; `pivot_root` is what a *disk* script may reach
/// for; `mkdir`/`cat`/`echo` are the script's file vocabulary. A name missing
/// from the farm is a boot that dies on `ENOENT` mid-script, so this list is
/// part of the contract with the shared `/init`
/// (`assets/initramfs/core/init`) - [`tests::the_farm_covers_the_scripts_vocabulary`]
/// is what holds it there.
///
/// `true`, `false` and `ls` are here for a *test*, not a script: `t-stdspawn`
/// runs `/bin/{true,false,echo,cat,sh,ls}` to prove a program can start other
/// programs, and this farm is the only `/bin` a disk root gets - the archive is
/// what the disk is unpacked from. They used to come from a hand-written
/// multi-call binary staged beside the payload (`xtask/src/aarch64_bin.rs`);
/// one applet list for both machines' roots is why that file is gone.
const AARCH64_APPLETS: &[&str] = &[
    "sh",
    "mount",
    "umount",
    "switch_root",
    "pivot_root",
    "mkdir",
    "cat",
    "echo",
    "ls",
    "true",
    "false",
];

/// The machine's archive description.
fn machine(arch: Arch) -> &'static Machine {
    match arch {
        Arch::X86_64 => &X86_64,
        Arch::Aarch64 => &AARCH64,
    }
}

impl Machine {
    fn tar_path(&self, build_dir: &Path) -> PathBuf {
        build_dir.join(self.tar)
    }
}

/// The repository's machine-neutral early-userspace overlay: one copy of each
/// file, merged into **both** machines' archives before their per-machine trees.
/// A file belongs here when both machines want the same answer and nothing about
/// the hardware says otherwise; `initramfs/core/root/README` is the inventory
/// and the rule. The `/init` both machines run is the reason it exists: it was
/// two files, one per machine, whose bodies had drifted while the second one's
/// header claimed they were the same script.
pub(crate) const CORE_OVERLAY: &str = crate::paths::INITRAMFS_CORE;

/// Merge the shared core first, then the machine's own tree over it - so a
/// machine may still replace anything the core carries, exactly as
/// `rootfs::merge_overlays` does for the disk. `src` names are resolved against
/// the repository root; a missing directory merges nothing, which keeps an empty
/// machine half honest without a special case here.
fn merge_overlays(root: &Path, machine: &Machine, staging: &Path) -> Result<()> {
    utils::merge_overlay(&root.join(CORE_OVERLAY), staging)?;
    utils::merge_overlay(&root.join(machine.overlay), staging)?;
    Ok(())
}

/// Build the archive for `arch`, unless the one on disk is already newer than
/// what it is built from.
pub fn build_initramfs(build_dir: &Path, arch: Arch) -> Result<()> {
    let machine = machine(arch);
    let root = utils::project_root()?;
    let config_path = root.join(machine.config);
    let config = InitramfsConfig::load(&config_path, arch)?;

    let staging = build_dir.join(machine.staging);
    let tar = machine.tar_path(build_dir);

    if !needs_rebuild(build_dir, arch)? {
        println!("{}", up_to_date_line(arch));
        return Ok(());
    }

    println!("Building the initramfs");
    fs::create_dir_all(build_dir)?;

    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    // A mount wants a directory to land on, and which ones the tree already has
    // is the machine's answer rather than a list written twice.
    for dir in machine.dirs {
        fs::create_dir_all(staging.join(dir))?;
    }

    // Repositories and keys before anything asks apk to resolve a name: the
    // trust decision is a filename match against `etc/apk/keys`, and without the
    // keys every package answers `UNTRUSTED signature`.
    let repositories = utils::repositories_text(&config.mirror, &config.branch, &config.repos);
    fs::write(staging.join("etc/apk/repositories"), repositories)?;

    utils::install_apk_keys(&staging.join("etc/apk/keys"))?;

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;

    // A pinned set is the machine's to declare; otherwise the toml's base list
    // and its `extra_packages`, in that order.
    let mut packages: Vec<String> = match machine.packages {
        Some(pinned) => pinned.iter().map(|p| p.to_string()).collect(),
        None => config.base_packages.clone(),
    };
    packages.extend(config.extra_packages.clone());

    install_packages(
        machine,
        &apk,
        &staging,
        &packages,
        &crate::rootfs::apk_cache_dir(build_dir, machine.arch),
    )?;
    lay_farm(machine, &staging)?;

    // `/init` is a file of the overlay trees and not a constant in this binary,
    // which is why a rebuilt `xtask` stopped counting as a reason to rebuild the
    // archive: a file in the tree has an mtime, and the staleness check reads
    // that instead.
    //
    // The first machine's `/bin/sh` is a link this function has to make by hand,
    // because the chrooted `busybox --install -s` above cannot know the tree's
    // shell name; the second machine's farm is laid by hand and includes `sh`.
    if matches!(machine.arch, Arch::X86_64) {
        let sh = staging.join("bin/sh");
        if sh.symlink_metadata().is_ok() {
            fs::remove_file(&sh)?;
        }
        std::os::unix::fs::symlink("/bin/busybox", &sh)?;
    }

    // The overlay pair, applied last: the shared core, then this machine's own
    // half over it.
    merge_overlays(&root, machine, &staging)?;
    // The script comes from a git checkout, where the executable bit is not
    // guaranteed, so it is set here - `execve` refuses a non-executable `/init`
    // even for root.
    let init = staging.join("init");
    fs::set_permissions(&init, fs::Permissions::from_mode(0o755))
        .context("The initramfs carries no /init")?;

    pack(machine, &staging, &tar)?;

    if let Some(stamp) = machine.stamp {
        fs::write(build_dir.join(stamp), format!("{}\n", packages.join("\n")))?;
    }

    let size = fs::metadata(&tar)?.len();
    println!("{}", packed_line(arch, &tar, size));

    fs::remove_dir_all(&staging)?;
    Ok(())
}

/// Ask apk for the packages, in the shape this machine's build needs.
///
/// The archive's config is not a parameter: what it decides - which packages, in
/// which order - is resolved by the caller, and the one thing apk still needs
/// from it is the machine's own package architecture, which is `machine.arch`'s
/// word rather than a key's (`xtask/src/arch.rs`).
fn install_packages(
    machine: &Machine,
    apk: &str,
    staging: &Path,
    packages: &[String],
    cache_dir: &Path,
) -> Result<()> {
    // `--no-scripts` because the package scripts are written for a chroot and
    // this tree is not one yet. What they also do is not optional on the first
    // machine: busybox's post-install creates the applet links, so the farm is
    // laid below by hand there instead.
    //
    // Both shapes share the rootfs pipeline's download cache (`--cache-dir`),
    // which is where the archive bytes come from when the mirror need not be
    // asked again; the index refresh stays unconditional here because this
    // tree is rebuilt whole (see [`build_initramfs`]) and its stamp records no
    // owned set — an initramfs install is always a fresh one.
    let arch = machine.arch.name();
    std::fs::create_dir_all(cache_dir)?;
    match machine.apk {
        Apk::Direct => {
            let status = Command::new(apk)
                .args(["--no-progress", "add"])
                .arg("--root")
                .arg(staging)
                .arg("--cache-dir")
                .arg(cache_dir)
                .args(["--arch", arch, "--initdb", "--no-scripts", "--update-cache"])
                .args(packages)
                .status()
                .context("Failed to install the initramfs packages")?;
            if !status.success() {
                anyhow::bail!("The initramfs packages did not install");
            }
            Ok(())
        }
        Apk::UsermodeRetry => {
            let mut flags = vec!["--arch", arch, "--initdb", "--no-scripts", "--update-cache"];
            if !utils::is_root() {
                flags.push("--usermode");
            }
            crate::rootfs::apk_add(&[apk], staging, &flags, packages, cache_dir)
        }
    }
}

/// Lay the busybox applet farm, in the shape this machine can.
fn lay_farm(machine: &Machine, staging: &Path) -> Result<()> {
    match machine.farm {
        Farm::Chroot => {
            // `--install` lays every applet as a symlink in one pass, and it has
            // to run as the binary *in the tree* so the links point where this
            // initramfs will look for them rather than at the host's busybox.
            let status = Command::new("chroot")
                .arg(staging)
                .args(["/bin/busybox", "--install", "-s"])
                .status()
                .context("Failed to run busybox --install in the initramfs tree")?;
            if !status.success() {
                anyhow::bail!("Could not install the busybox applet links");
            }
            Ok(())
        }
        Farm::ByHand(applets) => link_applets(staging, applets),
    }
}

/// Link the *archive's* applet vocabulary into a tree of the caller's own.
///
/// A tree laid from this archive - a boot disk's root - is a tree this machine's
/// scripts will exec in, so it answers the names those scripts name. It is laid
/// *before* the full farm (`rootfs::lay_applet_farm`), which is what a disk
/// carries on top: 309 names from the machine's own `busybox --list-full`,
/// covering this short list and everything else a distribution's scripts reach
/// for. Keeping the two separate is what makes this list a statement about *this
/// script* rather than a subset of a bigger one, and the second pass costs a
/// no-op - a name already answered is skipped (`link_applet_paths`).
pub fn link_farm(staging: &Path, arch: Arch) -> Result<()> {
    match machine(arch).farm {
        Farm::ByHand(applets) => link_applets(staging, applets),
        Farm::Chroot => Ok(()),
    }
}

fn link_applets(staging: &Path, applets: &[&str]) -> Result<()> {
    // The archive's farm is all in `/bin` (the applets its own scripts exec),
    // and the link itself is `rootfs::link_applet_paths` - one implementation of
    // "link `/bin/busybox` here unless something already answers", shared with
    // the disk's full farm.
    let paths: Vec<String> = applets
        .iter()
        .map(|applet| format!("bin/{applet}"))
        .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    crate::rootfs::link_applet_paths(staging, &paths)
}

/// Pack the staging tree as the archive both kernels' `vfs::tar` reads.
///
/// ustar, owned by root inside the guest whatever ran the build, entries
/// relative to the tree root. The kernel reads the mode the archive records, and
/// `execve` refuses a non-executable `/init` even for root - hence the chmod
/// above surviving into the tar through `--numeric-owner`.
fn pack(machine: &Machine, staging: &Path, tar: &Path) -> Result<()> {
    let status = Command::new("tar")
        .args([
            "--format=ustar",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
        ])
        .arg(tar)
        .arg("-C")
        .arg(staging)
        .arg(".")
        .status()
        .with_context(|| format!("Failed to pack the {} initramfs", machine.arch.name()))?;
    if !status.success() {
        anyhow::bail!("The {} initramfs did not pack", machine.arch.name());
    }
    Ok(())
}

/// Builds the archive unless the one on disk is already newer than its inputs.
pub fn ensure(build_dir: &Path, arch: Arch) -> Result<()> {
    build_initramfs(build_dir, arch)
}

/// Where `arch`'s packed archive lives, whether or not it is built yet.
///
/// The name is the machine's (`Machine::tar`), so the two archives cannot be
/// mistaken for each other: they are read by two kernels with two different
/// userlands in them.
pub fn archive_path(build_dir: &Path, arch: Arch) -> PathBuf {
    machine(arch).tar_path(build_dir)
}

/// The packed archive, built if needed - the path a boot installs onto its ESP
/// as `initramfs.tar`.
pub fn ensure_path(build_dir: &Path, arch: Arch) -> Result<PathBuf> {
    ensure(build_dir, arch)?;
    Ok(archive_path(build_dir, arch))
}

/// The archive, unpacked into `dest`.
///
/// A boot disk's root and an early userspace are the same tree: the one that
/// pivots and the one it pivots *onto* both want a shell, the applet farm and
/// the directories a mount lands on, and those are exactly what the archive
/// carries. So the second machine's disk is laid from this archive with its
/// overlays and payload merged over the top, which is the same arrangement the
/// first machine's test boot builds by hand (`selftest`'s unpack-then-stage
/// step) rather than a second tree maintained alongside this one.
pub fn unpack_into(build_dir: &Path, arch: Arch, dest: &Path) -> Result<()> {
    let tar = ensure_path(build_dir, arch)?;
    fs::create_dir_all(dest)?;
    let unpacked = Command::new("tar")
        .arg("-xf")
        .arg(&tar)
        .arg("-C")
        .arg(dest)
        // The archive is packed `--owner=0`; unpacking as an ordinary user
        // must not try to *become* root to write it.
        .arg("--no-same-owner")
        .status()
        .with_context(|| format!("Failed to unpack {}", tar.display()))?;
    if !unpacked.success() {
        anyhow::bail!("The initramfs did not unpack into {}", dest.display());
    }
    Ok(())
}

/// True when the archive is missing or older than what it is built from: the
/// configuration and the overlay tree, which is where the init script and
/// everything else in the tree lives.
///
/// The binary is not an input. It was one while `/init` was a constant inside
/// it - a rebuilt `xtask` meant a new init script, and nothing else said so.
/// With the script a file in the overlay, the tree's own mtimes answer the
/// question, and editing this crate no longer rebuilds an archive whose contents
/// did not change.
///
/// The second machine also carries a stamp, because its package set is pinned in
/// code rather than in its toml: the stamp records *when* the tree was laid, so
/// an overlay edit that lands after a rebuild still asks for another one even
/// though the toml's mtime did not move.
///
/// `run` asks this same question through `utils::image_stale`, which is what
/// makes a stale image rebuild instead of booting code that is no longer on
/// disk.
pub fn needs_rebuild(build_dir: &Path, arch: Arch) -> Result<bool> {
    let machine = machine(arch);
    let root = utils::project_root()?;
    let tar = machine.tar_path(build_dir);
    if !tar.exists() {
        return Ok(true);
    }

    if utils::newer(&root.join(machine.config), &tar)? {
        return Ok(true);
    }

    // Without a stamp the archive's own mtime is the answer.
    let since = match machine.stamp {
        Some(stamp) => {
            let stamp = build_dir.join(stamp);
            if !stamp.exists() {
                return Ok(true);
            }
            utils::file_mtime(&stamp)?
        }
        None => utils::file_mtime(&tar)?,
    };

    // Both halves, because either can hold a file a boot will exec: the core
    // carries the `/init` script, and a machine's own tree carries anything that
    // replaced it.
    if utils::dir_newer_than(&root.join(CORE_OVERLAY), since)? {
        return Ok(true);
    }
    utils::dir_newer_than(&root.join(machine.overlay), since)
}

/// The line `build_initramfs` prints when there is nothing to do.
fn up_to_date_line(arch: Arch) -> String {
    match arch {
        Arch::X86_64 => "initramfs: up to date".to_string(),
        Arch::Aarch64 => "initramfs: aarch64 up to date".to_string(),
    }
}

/// The line `build_initramfs` prints after a pack.
fn packed_line(arch: Arch, tar: &Path, size: u64) -> String {
    let kb = size as f64 / 1024.0;
    match arch {
        Arch::X86_64 => format!("initramfs: {} ({kb:.1} KB)", tar.display()),
        Arch::Aarch64 => format!("initramfs: aarch64 {} ({kb:.1} KB)", tar.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The farm's list is a contract with the script: every verb declared in the
    /// shared `/init`'s `@vocabulary:` line must be an applet busybox answers -
    /// shipped by the package or laid by the farm above. A name that is neither
    /// is a boot that dies on `ENOENT` mid-pivot, and a headless verdict reads
    /// that as a hang; this pin fails at build time instead. The declaration is
    /// read rather than scanned because a shell-script scanner is a heuristic,
    /// and a heuristic that passes is the failure mode here.
    ///
    /// One script to read, because both machines run the core's: this used to
    /// ask the second machine's own copy, which is also how the two bodies were
    /// able to drift apart unnoticed.
    #[test]
    fn the_farm_covers_the_scripts_vocabulary() {
        let root = utils::project_root().unwrap();
        let text = std::fs::read_to_string(root.join(CORE_OVERLAY).join("init")).unwrap();
        let line = text
            .lines()
            .find(|l| l.starts_with("@vocabulary:"))
            .expect("the initramfs script declares its @vocabulary:");
        for verb in line.trim_start_matches("@vocabulary:").split_whitespace() {
            assert!(
                AARCH64_APPLETS.contains(&verb),
                "`{verb}` is execed by the initramfs script but is not in the farm"
            );
        }
    }

    /// The two archives are read by two kernels and neither may find the other's
    /// bytes: the staging trees and the archive names are distinct, and so is
    /// everything a boot derives from them.
    ///
    /// The *config* is the one thing they now share. One `initramfs.toml` holds
    /// both machines' halves, and which half is read is the machine's own section
    /// (`config::parse_sectioned_toml`) - so what keeps the two archives apart is
    /// asserted where it now lives, and `config.rs`'s
    /// `the_shipped_files_answer_each_machine_its_own_values` is the pin that the
    /// two sections really do answer different values.
    #[test]
    fn the_two_machines_archives_cannot_be_mistaken_for_each_other() {
        assert_ne!(X86_64.tar, AARCH64.tar);
        assert_ne!(X86_64.staging, AARCH64.staging);
        assert_ne!(X86_64.overlay, AARCH64.overlay);
        assert_eq!(
            X86_64.config, AARCH64.config,
            "one file holds both machines' halves now; the section is what differs"
        );
        assert_eq!(X86_64.config, crate::paths::INITRAMFS_CONFIG);
    }

    /// The `/init` both machines run is **one** file, and it is the core's: a
    /// copy in either machine's tree is the drift this split exists to end. The
    /// two files it replaced *were* the same script - the second one's header
    /// said so - while their bodies differed by the selftest console probe, the
    /// switch-root verdict line and the name of the disk's own pid-1 chain, and
    /// nothing failed over any of it.
    ///
    /// The pin is the script and not the directories: a machine tree is allowed
    /// to be empty (both are, today) and to grow a file of its own; what it may
    /// not do silently is shadow the shared script.
    #[test]
    fn the_boot_script_is_one_file_in_the_shared_core() {
        let root = utils::project_root().unwrap();
        assert!(
            root.join(CORE_OVERLAY).join("init").is_file(),
            "the shared core must carry the /init both machines run"
        );
        // The inventory `initramfs/core/root/README` claims.
        assert!(
            root.join(CORE_OVERLAY).join("root/README").is_file(),
            "the core's README is the inventory of this tree"
        );
        for overlay in [X86_64.overlay, AARCH64.overlay] {
            assert!(
                !root.join(overlay).join("init").exists(),
                "{overlay}/init shadows the shared script - two copies is how \
                 the two machines drifted apart"
            );
        }
    }

    /// The two machines' archives are two files, and each machine's boot reads
    /// its own: one archive carries one userland, and a boot that read the
    /// other's would be a machine running the wrong machine's `/init`.
    #[test]
    fn each_machines_archive_is_a_file_of_its_own() {
        let build = Path::new("/tmp/mpos-build");
        assert_eq!(archive_path(build, Arch::X86_64), X86_64.tar_path(build));
        assert_eq!(archive_path(build, Arch::Aarch64), AARCH64.tar_path(build));
        assert_ne!(
            archive_path(build, Arch::X86_64),
            archive_path(build, Arch::Aarch64)
        );
    }
}
