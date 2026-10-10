//! Shared helpers for every xtask command: where the build lives, who owns
//! what it produces, and the filesystem and process utilities the commands
//! need. The behaviour mirrors `scripts/mkdisk.sh` and `scripts/run.sh`; those
//! two scripts are what these commands are a Rust front end for.

use std::env;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::arch::Arch;

/// The user a build's outputs belong to: whoever ran `cargo xtask`, even when
/// the privileged half of the build runs under sudo.
///
/// `mkdisk.sh` builds the kernel as the user and takes root only for the parts
/// that create device nodes and set ownership, and the files that half writes
/// have to be handed back afterwards: a `build/` or `target/` owned by root is
/// a tree the user cannot write to again, and the next build fails in a way
/// that says nothing about what actually went wrong.
#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    pub fn from_path(path: &Path) -> Result<Self> {
        let meta =
            fs::metadata(path).with_context(|| format!("Failed to stat {}", path.display()))?;
        Ok(Self {
            uid: meta.uid(),
            gid: meta.gid(),
        })
    }

    pub fn is_root(self) -> bool {
        self.uid == 0 && self.gid == 0
    }
}

pub fn is_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

/// The identity the build outputs should end up with. The sudo half carries it
/// in the environment, because sudo knows it and the elevated process would
/// otherwise have to guess. A build started as root with no sudo in the middle
/// falls back to the checkout's own owner, which is who a build of a tree
/// belongs to.
pub fn build_owner() -> Option<Owner> {
    if let (Ok(uid), Ok(gid)) = (env::var("MPOS_XTASK_UID"), env::var("MPOS_XTASK_GID"))
        && let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>())
    {
        return Some(Owner { uid, gid });
    }

    project_root()
        .ok()
        .and_then(|root| Owner::from_path(&root).ok())
}

pub fn project_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .context("The xtask crate has no parent directory")
}

/// The kernel's `uname -r` answer, read from the workspace root package.
///
/// The kernel embeds its own version at build time - `env!("CARGO_PKG_VERSION")`
/// in `proc.rs` (uname) and `procfs.rs` (`/proc/sys/kernel/osrelease`) - and
/// xtask, being a different package, cannot read *that* at its own compile
/// time. But the `/lib/modules/<release>` tree it writes into the image has to
/// be named exactly what `uname -r` will answer inside the guest, or modprobe
/// looks in a directory the kernel does not name. One number, and this parse
/// is what keeps the three readers agreeing without a shared crate.
pub fn kernel_release() -> Result<String> {
    let manifest = project_root()?.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .with_context(|| format!("Failed to read the workspace manifest {manifest:?}"))?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((_, value)) = line.strip_prefix("version").and_then(|r| r.split_once('=')) {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() && !value.starts_with('{') {
                return Ok(value.to_string());
            }
        }
    }
    anyhow::bail!("The workspace root Cargo.toml names no [package] version")
}

/// Resolves the build directory: an absolute path is taken as written, a
/// relative one is relative to the tree, so the commands name the same image
/// wherever they are invoked from.
pub fn resolve_build_dir(cli: Option<&Path>) -> Result<PathBuf> {
    let root = project_root()?;
    let raw = match cli {
        Some(path) => path.to_path_buf(),
        None => env::var_os("MPOS_BUILD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("build")),
    };
    Ok(if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    })
}

pub fn resolve_profile(cli: Option<&str>) -> String {
    cli.map(str::to_string)
        .or_else(|| env::var("PROFILE").ok())
        .unwrap_or_else(|| "release".to_string())
}

pub fn target_dir() -> Result<PathBuf> {
    let root = project_root()?;
    match env::var_os("CARGO_TARGET_DIR") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            Ok(if path.is_absolute() {
                path
            } else {
                root.join(path)
            })
        }
        _ => Ok(root.join("target")),
    }
}

pub fn kernel_path(profile: &str, arch: Arch) -> Result<PathBuf> {
    Ok(target_dir()?
        .join(arch.kernel_target())
        .join(profile)
        .join("mpos"))
}

pub fn loader_path(profile: &str, arch: Arch) -> Result<PathBuf> {
    Ok(target_dir()?
        .join(arch.loader_target())
        .join(profile)
        .join("mpos-boot.efi"))
}

/// The disk image `arch`'s machine boots, under the build directory.
///
/// The name is the machine's (`Arch::disk_image`) and the two are different
/// files: one machine's image holds its own root filesystem and its own boot
/// files, so an image built for one is not the other's - which is also what
/// keeps the two rootfs stamps meaningful.
pub fn image_path(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(arch.disk_image())
}

/// Marks a disk image whose root filesystem has been installed. The image's
/// own mtime cannot say that - `disk` creating the file makes it newer than
/// the configuration, which would read as "the rootfs was built after the last
/// change" and skip the build entirely.
///
/// One stamp per machine (`Arch::rootfs_stamp`): the two images are built from
/// the same configuration and the same overlay trees, so a shared name would let
/// the machine that built last answer for the other one.
pub fn rootfs_stamp(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(arch.rootfs_stamp())
}

pub fn file_mtime(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .with_context(|| format!("Failed to get metadata for {}", path.display()))?
        .modified()
        .with_context(|| format!("Failed to get modification time for {}", path.display()))
}

/// True when both paths exist and `candidate` is the newer of the two.
pub fn newer(candidate: &Path, reference: &Path) -> Result<bool> {
    if !candidate.exists() || !reference.exists() {
        return Ok(false);
    }
    let candidate_time = file_mtime(candidate)?;
    let reference_time = file_mtime(reference)?;
    Ok(candidate_time > reference_time)
}

/// True when any file under `dir` was modified after `after`.
///
/// Read through `symlink_metadata`, so a link is *the entry* the walk compares
/// rather than whatever it points at. That is not a detail: an installed Alpine
/// tree carries absolute links (`/etc/init.d/functions.sh ->
/// /usr/libexec/rc/sh/functions.sh`), which dangle on the host that installed it,
/// and a walk that followed them failed outright - "Failed to get metadata" -
/// which took the whole command down with it rather than answering the question
/// it was asked. A dangling link is an entry like any other here; its own mtime
/// is what says whether the tree moved.
pub fn dir_newer_than(dir: &Path, after: SystemTime) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            if dir_newer_than(&path, after)? {
                return Ok(true);
            }
        } else if meta
            .modified()
            .with_context(|| format!("Failed to get modification time for {}", path.display()))?
            > after
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The line `lay_p9_share` writes into every exported share, and the line a boot
/// reads back to prove the share *serves* rather than merely mounted.
///
/// Written literally in the kernel's own probe too (`drivers/virtio/probe.rs`
/// registers the tag the host exported; the mount is the early userland's).
/// A fixture both sides have to agree on has two copies, and the check's job is
/// to notice if they drift; the disk image's `i as u8` pattern sector is the
/// same arrangement (`NOTES.md` §13.29).
pub const P9_PROBE: &[u8] = b"mpos reads from the host share over 9p\n";

/// The file name that line is written under, in every exported share.
pub const P9_PROBE_NAME: &str = "mpos-9p-probe";

/// Create `dir`, lay the 9p probe file in it, and hand the directory back.
///
/// Every boot that exports a host share lays it through here, so a machine can
/// prove the share reads and not just that a device answered: the kernel's own
/// mount + read and the `t-9p` program both look for exactly this file. The
/// directory is created even when it exists, because an *empty* share is a
/// share that lists empty rather than a failed boot.
pub fn lay_p9_share(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to make the 9p share at {}", dir.display()))?;
    let probe = dir.join(P9_PROBE_NAME);
    fs::write(&probe, P9_PROBE)
        .with_context(|| format!("Failed to write the 9p probe at {}", probe.display()))?;
    Ok(dir.to_path_buf())
}

/// True when `arch`'s image on disk does not reflect the sources: it is missing,
/// it was never assembled, or anything it was assembled from has changed since.
///
/// The image is the last of three stages - the kernel and its loader, the
/// initramfs archive, the root filesystem - and it depends on all of them, so
/// any one moving makes it stale. Each stage is asked the question it asks
/// itself rather than this function keeping a second copy of the rules: a
/// kernel rebuilt after the last image has to trigger the rebuild, or `run`
/// boots code that is no longer on disk and calls it a fix that did nothing.
///
/// The three stages are `arch`'s, so both machines ask this one question. What
/// the answer costs differs by how far the staleness reaches: a kernel change is
/// boot files, and a configuration change is the packages
/// (`rootfs::needs_full_install`).
pub fn image_stale(build_dir: &Path, profile: &str, arch: Arch) -> Result<bool> {
    if !image_path(build_dir, arch).exists() {
        return Ok(true);
    }
    if crate::initramfs::needs_rebuild(build_dir, arch)? {
        return Ok(true);
    }
    crate::rootfs::needs_rebuild(build_dir, profile, arch)
}

/// Build the kernel and its UEFI loader *for `arch`*.
///
/// Both targets are named rather than left to the configuration: the kernel's
/// is the workspace default (`x86_64-unknown-none`, pinned in
/// `.cargo/config.toml`) and the loader's is `boot/.cargo/config.toml`'s, so a
/// build that named neither compiled the first machine's pair whatever `--arch`
/// said. That was the switch's one real gap - `cargo xtask build --arch
/// aarch64` produced an x86_64 kernel and reported success - and passing both
/// targets here is what closes it.
pub fn build_kernel(profile: &str, arch: Arch) -> Result<()> {
    let root = project_root()?;
    println!("Building the {arch} kernel ({profile})", arch = arch.name());

    let mut kernel_build = Command::new("cargo");
    kernel_build.arg("build");
    if profile == "release" {
        kernel_build.arg("--release");
    }
    kernel_build
        .arg("-p")
        .arg("mpos")
        .arg("--target")
        .arg(arch.kernel_target())
        .current_dir(&root);
    let status = kernel_build
        .status()
        .context("Failed to run `cargo build` for the kernel")?;
    if !status.success() {
        anyhow::bail!("The kernel did not build");
    }

    build_loader(profile, arch)?;

    let kernel = kernel_path(profile, arch)?;
    let loader = loader_path(profile, arch)?;
    if !kernel.exists() {
        anyhow::bail!("Expected the kernel at {}", kernel.display());
    }
    if !loader.exists() {
        anyhow::bail!("Expected the EFI loader at {}", loader.display());
    }

    println!("kernel: {} ({:.1} KB)", kernel.display(), kib(&kernel)?);
    println!("loader: {} ({:.1} KB)", loader.display(), kib(&loader)?);
    Ok(())
}

/// Build the kernel the way a **test** build is built, and answer where its image
/// landed.
///
/// `--features selftest` is what puts the boot's check suites and their harness
/// into the kernel. Without it the wiring layer compiles neither (`src/board/
/// mod.rs` says where they went, and the kernel's own manifest why the switch is
/// a cargo feature rather than a `--cfg …`), so a kernel built by
/// [`build_kernel`] carries no test code at all and no way to run any.
///
/// The image is **copied** out of the target directory rather than left there.
/// Cargo holds one `mpos` per target and profile: a production build after a test
/// build - or the reverse, which is what `cargo xtask test` then `cargo xtask run`
/// does - replaces the other's binary, and the ESP assembled a moment later would
/// stage whichever one happened to be there. The copy is what makes "the test
/// machine booted the test kernel" a fact rather than an assumption, and it is
/// also why this returns a path instead of nothing.
///
/// What it costs: the shared target directory means alternating the two builds
/// recompiles the kernel crate, because the feature set changed. Measured at a
/// few seconds either way, against a second target directory's order of a full
/// rebuild and a second copy of every dependency.
pub fn build_test_kernel(build_dir: &Path, profile: &str, arch: Arch) -> Result<PathBuf> {
    let root = project_root()?;
    println!(
        "Building the {arch} kernel for the self-test ({profile})",
        arch = arch.name()
    );

    let mut kernel_build = Command::new("cargo");
    kernel_build.arg("build");
    if profile == "release" {
        kernel_build.arg("--release");
    }
    kernel_build
        .arg("-p")
        .arg("mpos")
        .arg("--features")
        .arg("selftest")
        .arg("--target")
        .arg(arch.kernel_target())
        .current_dir(&root);
    let status = kernel_build
        .status()
        .context("Failed to run `cargo build` for the test kernel")?;
    if !status.success() {
        anyhow::bail!("The test kernel did not build");
    }

    // The loader is the same program in both builds - it loads an ELF and does
    // not know what is in it - so this is the ordinary build of it.
    build_loader(profile, arch)?;

    let source = kernel_path(profile, arch)?;
    let dir = build_dir.join(format!("{}-test-kernel", arch.name()));
    fs::create_dir_all(&dir)?;
    let image = dir.join("mpos");
    fs::copy(&source, &image)
        .with_context(|| format!("Failed to copy {} to {}", source.display(), image.display()))?;

    println!(
        "self-test kernel: {} ({:.1} KB)",
        image.display(),
        kib(&image)?
    );
    Ok(image)
}

/// Build the EFI loader, which is the same program for both kernel builds.
fn build_loader(profile: &str, arch: Arch) -> Result<()> {
    let root = project_root()?;
    println!(
        "Building the {arch} EFI loader ({profile})",
        arch = arch.name()
    );
    let mut loader_build = Command::new("cargo");
    loader_build.arg("build");
    if profile == "release" {
        loader_build.arg("--release");
    }
    loader_build
        .arg("-p")
        .arg("mpos-boot")
        .arg("--target")
        .arg(arch.loader_target())
        .current_dir(&root);
    let status = loader_build
        .status()
        .context("Failed to run `cargo build` for the EFI loader")?;
    if !status.success() {
        anyhow::bail!("The EFI loader did not build");
    }
    Ok(())
}

fn kib(path: &Path) -> Result<f64> {
    Ok(fs::metadata(path)?.len() as f64 / 1024.0)
}

pub fn clean(build_dir: &Path) -> Result<()> {
    let target = target_dir()?;
    for dir in [build_dir.to_path_buf(), target] {
        if dir.exists() {
            println!("Cleaning {}", dir.display());
            fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove {}", dir.display()))?;
        }
    }
    println!("Clean completed");
    Ok(())
}

/// True when `clean` needs root because an earlier build left a root-owned
/// directory behind. Builds hand their output back to the user now, so this is
/// only about trees that predate that.
pub fn clean_needs_root(build_dir: &Path) -> bool {
    if is_root() {
        return false;
    }
    let target = target_dir().unwrap_or_default();
    [build_dir.to_path_buf(), target]
        .iter()
        .any(|dir| dir.exists() && !owned_by_current_user(dir))
}

fn owned_by_current_user(path: &Path) -> bool {
    owned_by(path, current_uid())
}

/// True when `path` exists and belongs to `uid`. A path that cannot be stat'ed
/// counts as owned, so a missing directory never triggers a repair.
pub fn owned_by(path: &Path, uid: u32) -> bool {
    fs::metadata(path)
        .map(|meta| meta.uid() == uid)
        .unwrap_or(true)
}

/// Repairs a tree an earlier build left owned by root - the state a build that
/// re-execs into sudo *before* running cargo ends in, which is exactly what
/// this crate used to do. Cargo cannot write into a root-owned `target/`, and
/// the build would fail before anything could hand it back, so it is repaired
/// up front: one `chown -R` over the two directories, and only when they are
/// not already the user's.
pub fn repair_ownership(build_dir: &Path) -> Result<()> {
    if is_root() {
        return Ok(());
    }

    let uid = current_uid();
    let mut roots = vec![build_dir.to_path_buf()];
    if let Ok(target) = target_dir() {
        roots.push(target);
    }

    let strays: Vec<PathBuf> = roots
        .into_iter()
        .filter(|path| path.exists() && !owned_by(path, uid))
        .collect();
    if strays.is_empty() {
        return Ok(());
    }

    println!("Handing root-owned build directories back to the user");
    let mut cmd = Command::new("sudo");
    cmd.arg("-E")
        .arg("chown")
        .arg("-R")
        .arg(format!("{uid}:{}", current_gid()));
    for path in &strays {
        cmd.arg(path);
    }
    run_status(&mut cmd, "chown of root-owned build directories")
}

/// Gives every entry under `tree` (and `tree` itself) to `owner`, without
/// following symlinks: a staged root filesystem holds links like
/// `/bin/sh -> /bin/busybox`, and a chown that followed them would look for
/// that path on the host.
pub fn chown_tree(tree: &Path, owner: Owner) -> Result<()> {
    if !tree.exists() {
        return Ok(());
    }
    chown_children(tree, owner)?;
    chown_one(tree, owner)
}

fn chown_children(dir: &Path, owner: Owner) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        chown_one(&path, owner)?;
        if meta.is_dir() {
            chown_children(&path, owner)?;
        }
    }
    Ok(())
}

fn chown_one(path: &Path, owner: Owner) -> Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains an interior NUL: {}", path.display()))?;
    let rc = unsafe { libc::lchown(name.as_ptr(), owner.uid, owner.gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to chown {}", path.display()));
    }
    Ok(())
}

pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("Failed to set mode {mode:o} on {}", path.display()))
}

/// Merges an overlay directory into a tree: the *contents* of `src` land in
/// `dst`, so `rootfs/etc/abc` becomes `/etc/abc` and replaces whatever was
/// already there. Directories are merged rather than replaced; files and
/// symlinks overwrite.
///
/// `cp -a src/. dst/` is the shell equivalent, and `std::fs::copy` alone would
/// not do: it turns a link into a second copy of its target, which is wrong
/// for every `/bin/*` in an Alpine tree.
pub fn merge_overlay(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&from)?;

        if meta.file_type().is_symlink() {
            if to.symlink_metadata().is_ok() {
                fs::remove_file(&to)?;
            }
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else if meta.is_dir() {
            // A directory in the overlay replaces a file of the same name: the
            // overlay is the authority on what the tree looks like, and
            // `create_dir_all` over a file would otherwise be the build's
            // failure for a conflict the caller already resolved by writing
            // the overlay.
            let replaced_file = to
                .symlink_metadata()
                .is_ok_and(|existing| !existing.is_dir());
            if replaced_file {
                fs::remove_file(&to)?;
            }
            merge_overlay(&from, &to)?;
        } else {
            if to.symlink_metadata().is_ok() {
                fs::remove_file(&to)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub fn run_status(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run {what}"))?;
    if !status.success() {
        anyhow::bail!("{what} failed");
    }
    Ok(())
}

/// Unmounts everything mounted at or under `mount_point`, deepest first. Read
/// from `/proc/mounts` rather than kept in a list, because a run that failed
/// half way through can leave mounts this process never recorded.
pub fn umount_recursively(mount_point: &Path) -> usize {
    let prefix = mount_point.to_string_lossy().to_string();
    // Deepest first, so a child mount is released before the directory under
    // which its parent sits. A plain `umount` is the polite pass; whatever still
    // holds the table after it is forced loose with a lazy detach, which
    // unlinks the mount from the namespace at once even while a process remains
    // inside it.
    for point in mount_points_under(&prefix) {
        let _ = Command::new("umount").arg(&point).status();
    }
    for point in mount_points_under(&prefix) {
        let _ = Command::new("umount").args(["-l", &point]).status();
    }
    // The caller must know if anything is still mounted: deleting a directory
    // that still has a live bind mount under it recurses THROUGH that mount and
    // removes the mounted tree's own files. The staged root bind-mounts the
    // host `/dev` and `/sys`, so a `/dev` bind that outlived the umount and was
    // then `rm -rf`'d would take the real `/dev/null` down with it.
    mount_points_under(&prefix).len()
}

/// Every mount point at or below `prefix`, deepest first. Reads the live table
/// each call so a caller can re-check after issuing unmounts.
fn mount_points_under(prefix: &str) -> Vec<String> {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let nested = format!("{prefix}/");
    let mut points: Vec<String> = mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|point| point.replace("\\040", " "))
        .filter(|point| point == prefix || point.starts_with(&nested))
        .collect();
    points.sort_by_key(|point| std::cmp::Reverse(point.len()));
    points
}

/// The version of `apk-tools-static` fetched when the host has no `apk`. It is
/// only a preference: the branch moves, `apk-tools` is rebuilt with it, and the
/// version pinned here stops being on the mirror - so a fetch that fails falls
/// back to whichever version the mirror is serving.
const APK_TOOLS_VERSION: &str = "3.0.8";

/// Finds the static `apk` the image's packages are installed with. The host's
/// own `apk` is used when it has one; otherwise the static build is fetched from
/// the same mirror every package comes from.
pub fn find_apk_tool(build_dir: &Path, mirror: &str, branch: &str) -> Result<String> {
    if let Some(apk) = command_v("apk") {
        return Ok(apk);
    }

    println!("No apk on this machine; fetching apk.static");
    let temp = build_dir.join("apktools");
    fs::create_dir_all(&temp)?;
    let apk_pkg = temp.join("apk-tools-static.apk");
    let apk_static = temp.join("sbin/apk.static");

    if !apk_static.exists() {
        let pkg_dir = format!("{mirror}/{branch}/main/x86_64");
        let pinned = format!("{pkg_dir}/apk-tools-static-{APK_TOOLS_VERSION}-r0.apk");
        if !wget(&pinned, &apk_pkg)? {
            // The pin is stale. Ask the mirror what it serves and take the newest.
            let listing = shell(&format!(
                "wget -T 60 --no-verbose -O - '{pkg_dir}/' 2>/dev/null"
            ))?;
            let newest = listing
                .lines()
                .flat_map(|line| line.split('"'))
                .filter(|name| name.starts_with("apk-tools-static-") && name.ends_with(".apk"))
                .max_by(|a, b| version_cmp(a, b));
            let Some(newest) = newest else {
                anyhow::bail!("No apk-tools-static at {pkg_dir}, and no apk on PATH");
            };
            if !wget(&format!("{pkg_dir}/{newest}"), &apk_pkg)? {
                anyhow::bail!("Could not fetch apk-tools-static; put apk on PATH and try again");
            }
        }

        fs::create_dir_all(temp.join("sbin"))?;
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&apk_pkg)
            .arg("-C")
            .arg(&temp)
            .args(["--warning=no-unknown-keyword", "sbin/apk.static"])
            .status()
            .context("Failed to extract apk-tools-static")?;
        if !status.success() {
            anyhow::bail!("The apk-tools-static package did not extract");
        }
    }

    set_mode(&apk_static, 0o755)?;
    Ok(apk_static.to_string_lossy().to_string())
}

pub(crate) fn wget(url: &str, dest: &Path) -> Result<bool> {
    let status = Command::new("wget")
        .args(["-T", "60", "--no-verbose", "-O"])
        .arg(dest)
        .arg(url)
        .status()
        .context("Failed to run wget")?;
    Ok(status.success())
}

fn shell(command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .context("Failed to run a shell command")?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn command_v(name: &str) -> Option<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Orders two `apk-tools-static-<version>-r<n>.apk` names by version, the way
/// `sort -V` would.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let numbers = |name: &str| -> Vec<u64> {
        name.trim_start_matches("apk-tools-static-")
            .trim_end_matches(".apk")
            .split(['.', '-', '_'])
            .filter_map(|part| {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect()
    };
    numbers(a).cmp(&numbers(b))
}

/// The `/etc/apk/repositories` file, one line per repository the config names.
///
/// The order is the config's and it matters: apk takes a package from the first
/// repository that has it, so `main` before `community` is what keeps a package
/// that exists in both coming from the one it belongs to.
pub fn repositories_text(mirror: &str, branch: &str, repos: &[String]) -> String {
    repos
        .iter()
        .map(|repo| format!("{mirror}/{branch}/{repo}\n"))
        .collect()
}

/// Writes the host's APK keys into a tree, or the ones embedded here when the
/// host has none. The file *name* is load-bearing: apk decides whether a
/// repository index is trusted by looking for a key whose filename matches the
/// name in the index's signature, and every Alpine index is signed as
/// `alpine-devel@lists.alpinelinux.org-<key id>.rsa.pub`.
pub fn install_apk_keys(dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)?;
    let host = Path::new("/etc/apk/keys");
    if host.is_dir() {
        for entry in fs::read_dir(host)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "pub") {
                fs::copy(&path, dest_dir.join(entry.file_name()))?;
            }
        }
    } else {
        dump_alpine_keys(dest_dir)?;
    }
    Ok(())
}

fn dump_alpine_keys(dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)?;
    for (id, content) in ALPINE_KEYS {
        let name = format!("alpine-devel@lists.alpinelinux.org-{id}.rsa.pub");
        let body = format!("-----BEGIN PUBLIC KEY-----\n{content}\n-----END PUBLIC KEY-----\n");
        fs::write(dest_dir.join(name), body)?;
    }
    Ok(())
}

/// Alpine APK keys for x86_64, the same ones `scripts/mkdisk.sh` embeds. A key
/// written under any other name is present but never consulted - apk reports
/// the index as `UNTRUSTED signature` and then answers `no such package` for
/// every name in the world file.
const ALPINE_KEYS: &[(&str, &str)] = &[
    (
        "4a6a0840",
        "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA1yHJxQgsHQREclQu4Ohe\nqxTxd1tHcNnvnQTu/UrTky8wWvgXT+jpveroeWWnzmsYlDI93eLI2ORakxb3gA2O\nQ0Ry4ws8vhaxLQGC74uQR5+/yYrLuTKydFzuPaS1dK19qJPXB8GMdmFOijnXX4SA\njixuHLe1WW7kZVtjL7nufvpXkWBGjsfrvskdNA/5MfxAeBbqPgaq0QMEfxMAn6/R\nL5kNepi/Vr4S39Xvf2DzWkTLEK8pcnjNkt9/aafhWqFVW7m3HCAII6h/qlQNQKSo\nGuH34Q8GsFG30izUENV9avY7hSLq7nggsvknlNBZtFUcmGoQrtx3FmyYsIC8/R+B\nywIDAQAB",
    ),
    (
        "5261cecb",
        "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAwlzMkl7b5PBdfMzGdCT0\ncGloRr5xGgVmsdq5EtJvFkFAiN8Ac9MCFy/vAFmS8/7ZaGOXoCDWbYVLTLOO2qtX\nyHRl+7fJVh2N6qrDDFPmdgCi8NaE+3rITWXGrrQ1spJ0B6HIzTDNEjRKnD4xyg4j\ng01FMcJTU6E+V2JBY45CKN9dWr1JDM/nei/Pf0byBJlMp/mSSfjodykmz4Oe13xB\nCa1WTwgFykKYthoLGYrmo+LKIGpMoeEbY1kuUe04UiDe47l6Oggwnl+8XD1MeRWY\nsWgj8sF4dTcSfCMavK4zHRFFQbGp/YFJ/Ww6U9lA3Vq0wyEI6MCMQnoSMFwrbgZw\nwwIDAQAB",
    ),
    (
        "6165ee59",
        "MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAutQkua2CAig4VFSJ7v54\nALyu/J1WB3oni7qwCZD3veURw7HxpNAj9hR+S5N/pNeZgubQvJWyaPuQDm7PTs1+\ntFGiYNfAsiibX6Rv0wci3M+z2XEVAeR9Vzg6v4qoofDyoTbovn2LztaNEjTkB+oK\ntlvpNhg1zhou0jDVYFniEXvzjckxswHVb8cT0OMTKHALyLPrPOJzVtM9C1ew2Nnc\n3848xLiApMu3NBk0JqfcS3Bo5Y2b1FRVBvdt+2gFoKZix1MnZdAEZ8xQzL/a0YS5\nHd0wj5+EEKHfOd3A75uPa/WQmA+o0cBFfrzm69QDcSJSwGpzWrD1ScH3AK8nWvoj\nv7e9gukK/9yl1b4fQQ00vttwJPSgm9EnfPHLAtgXkRloI27H6/PuLoNvSAMQwuCD\nhQRlyGLPBETKkHeodfLoULjhDi1K2gKJTMhtbnUcAA7nEphkMhPWkBpgFdrH+5z4\nLxy+3ek0cqcI7K68EtrffU8jtUj9LFTUC8dERaIBs7NgQ/LfDbDfGh9g6qVj1hZl\nk9aaIPTm/xsi8v3u+0qaq7KzIBc9s59JOoA8TlpOaYdVgSQhHHLBaahOuAigH+VI\nisbC9vmqsThF2QdDtQt37keuqoda2E6sL7PUvIyVXDRfwX7uMDjlzTxHTymvq2Ck\nhtBqojBnThmjJQFgZXocHG8CAwEAAQ==",
    ),
    (
        "58199dcc",
        "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA3v8/ye/V/t5xf4JiXLXa\nhWFRozsnmn3hobON20GdmkrzKzO/eUqPOKTpg2GtvBhK30fu5oY5uN2ORiv2Y2ht\neLiZ9HVz3XP8Fm9frha60B7KNu66FO5P2o3i+E+DWTPqqPcCG6t4Znk2BypILcit\nwiPKTsgbBQR2qo/cO01eLLdt6oOzAaF94NH0656kvRewdo6HG4urbO46tCAizvCR\nCA7KGFMyad8WdKkTjxh8YLDLoOCtoZmXmQAiwfRe9pKXRH/XXGop8SYptLqyVVQ+\ntegOD9wRs2tOlgcLx4F/uMzHN7uoho6okBPiifRX+Pf38Vx+ozXh056tjmdZkCaV\naQIDAQAB",
    ),
    (
        "616ae350",
        "MIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEAyduVzi1mWm+lYo2Tqt/0\nXkCIWrDNP1QBMVPrE0/ZlU2bCGSoo2Z9FHQKz/mTyMRlhNqTfhJ5qU3U9XlyGOPJ\npiM+b91g26pnpXJ2Q2kOypSgOMOPA4cQ42PkHBEqhuzssfj9t7x47ppS94bboh46\nxLSDRff/NAbtwTpvhStV3URYkxFG++cKGGa5MPXBrxIp+iZf9GnuxVdST5PGiVGP\nODL/b69sPJQNbJHVquqUTOh5Ry8uuD2WZuXfKf7/C0jC/ie9m2+0CttNu9tMciGM\nEyKG1/Xhk5iIWO43m4SrrT2WkFlcZ1z2JSf9Pjm4C2+HovYpihwwdM/OdP8Xmsnr\nDzVB4YvQiW+IHBjStHVuyiZWc+JsgEPJzisNY0Wyc/kNyNtqVKpX6dRhMLanLmy+\nf53cCSI05KPQAcGj6tdL+D60uKDkt+FsDa0BTAobZ31OsFVid0vCXtsbplNhW1IF\nHwsGXBTVcfXg44RLyL8Lk/2dQxDHNHzAUslJXzPxaHBLmt++2COa2EI1iWlvtznk\nOk9WP8SOAIj+xdqoiHcC4j72BOVVgiITIJNHrbppZCq6qPR+fgXmXa+sDcGh30m6\n9Wpbr28kLMSHiENCWTdsFij+NQTd5S47H7XTROHnalYDuF1RpS+DpQidT5tUimaT\nJZDr++FjKrnnijbyNF8b98UCAwEAAQ==",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_apk_static_versions_numerically() {
        let older = "apk-tools-static-3.0.8-r0.apk";
        let newer = "apk-tools-static-3.0.10-r0.apk";
        assert_eq!(version_cmp(older, newer), std::cmp::Ordering::Less);
        assert_eq!(version_cmp(newer, older), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp(older, older), std::cmp::Ordering::Equal);
    }

    /// A scratch directory of this test's own, removed first so a previous
    /// run's leftovers cannot make the test pass.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpos-xtask-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_overlay_lands_at_the_target_root_and_overwrites() {
        let dir = scratch("overlay");
        let src = dir.join(crate::paths::ROOTFS_OVERLAY_X86_64);
        let dst = dir.join("target");
        fs::create_dir_all(src.join("etc/init.d")).unwrap();
        fs::write(src.join("etc/abc"), "from the overlay").unwrap();
        fs::create_dir_all(dst.join("etc")).unwrap();
        fs::write(dst.join("etc/abc"), "from the base system").unwrap();

        merge_overlay(&src, &dst).unwrap();

        // `rootfs/etc/abc` is `/etc/abc`: the *contents* of the overlay map to
        // the target's root, not to a `/rootfs` directory inside it.
        assert_eq!(
            fs::read_to_string(dst.join("etc/abc")).unwrap(),
            "from the overlay",
            "the overlay has to replace what the base system put there"
        );
        assert!(
            !dst.join(crate::paths::ROOTFS_OVERLAY_X86_64).exists(),
            "the overlay is not a directory in the target"
        );
        assert!(dst.join("etc/init.d").is_dir(), "directories are merged");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_in_the_overlay_lands_as_a_symlink() {
        // The counterfactual for the second machine's install moving from its
        // local `copy_tree` (which dereferences: `fs::copy` follows the link)
        // onto this one merge path. A core file that is a symlink - a timezone
        // link, an applet alias - must stay a link on both images, or the two
        // machines' trees would mean different things by the same name.
        let dir = scratch("overlay-symlink");
        let src = dir.join(crate::paths::ROOTFS_CORE);
        let dst = dir.join("target");
        fs::create_dir_all(src.join("usr/share/zoneinfo")).unwrap();
        fs::write(src.join("usr/share/zoneinfo/UTC"), "tz data").unwrap();
        fs::create_dir_all(src.join("etc")).unwrap();
        std::os::unix::fs::symlink("../usr/share/zoneinfo/UTC", src.join("etc/localtime")).unwrap();

        merge_overlay(&src, &dst).unwrap();

        let link = fs::symlink_metadata(dst.join("etc/localtime")).unwrap();
        assert!(link.file_type().is_symlink(), "landed as: {link:?}");
        assert_eq!(
            fs::read_link(dst.join("etc/localtime")).unwrap(),
            PathBuf::from("../usr/share/zoneinfo/UTC")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_in_the_overlay_replaces_a_file_of_the_same_name() {
        let dir = scratch("overlay-dir");
        let src = dir.join(crate::paths::ROOTFS_OVERLAY_X86_64);
        let dst = dir.join("target");
        fs::create_dir_all(src.join("etc/abc")).unwrap();
        fs::write(src.join("etc/abc/inner"), "x").unwrap();
        fs::create_dir_all(dst.join("etc")).unwrap();
        fs::write(
            dst.join("etc/abc"),
            "a file where the overlay has a directory",
        )
        .unwrap();

        merge_overlay(&src, &dst).unwrap();

        assert!(dst.join("etc/abc/inner").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The walk answers the question it is asked even when the tree carries a
    /// link it cannot follow.
    ///
    /// The counterfactual is a real one: an installed Alpine tree's links are
    /// absolute (`/etc/init.d/functions.sh -> /usr/libexec/rc/sh/functions.sh`)
    /// and dangle on the host that installed it, and a walk that followed them
    /// answered `Failed to get metadata ...: No such file or directory` instead of
    /// `false` - which is what took a whole aarch64 boot down with it, since the
    /// staleness check is asked before every boot.
    #[test]
    fn a_dangling_symlink_in_a_tree_does_not_fail_the_walk() {
        let dir = scratch("dir-newer-dangling");
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("etc")).unwrap();
        fs::write(tree.join("etc/real"), "x").unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", tree.join("etc/dangling")).unwrap();

        let after = SystemTime::now() + std::time::Duration::from_secs(60);
        assert!(
            !dir_newer_than(&tree, after).unwrap(),
            "nothing in the tree is newer than a time in the future"
        );

        // And the link is still an entry the walk compares: touching it *is* a
        // change to the tree, which is what keeps a link the tree carries from
        // being invisible to the stamp that reads it.
        let old = SystemTime::now() - std::time::Duration::from_secs(600);
        std::os::unix::fs::symlink("/nowhere/at/all", dir.join("fresh")).unwrap();
        fs::remove_file(tree.join("etc/dangling")).unwrap();
        fs::rename(dir.join("fresh"), tree.join("etc/dangling")).unwrap();
        assert!(dir_newer_than(&tree, old).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rebuilt_kernel_makes_the_image_stale() {
        let dir = scratch("stale");
        let build = dir.join("build");
        let target = dir.join("target");
        fs::create_dir_all(&build).unwrap();
        fs::create_dir_all(target.join("x86_64-unknown-none/debug")).unwrap();
        fs::create_dir_all(target.join("x86_64-unknown-uefi/debug")).unwrap();

        let kernel = target.join("x86_64-unknown-none/debug/mpos");
        let loader = target.join("x86_64-unknown-uefi/debug/mpos-boot.efi");
        let tar = crate::initramfs::archive_path(&build, Arch::X86_64);
        let image = image_path(&build, Arch::X86_64);
        let stamp = rootfs_stamp(&build, Arch::X86_64);

        // The tree as it is just after a build: every input older than the
        // stamp, and the stamp the newest thing in it.
        fs::write(&kernel, "").unwrap();
        fs::write(&loader, "").unwrap();
        fs::write(&tar, "").unwrap();
        fs::write(&image, "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&stamp, "").unwrap();

        // The kernel path is resolved through `CARGO_TARGET_DIR`, so the
        // environment is pointed at this test's tree for the length of it.
        // Nothing else in this crate reads that variable, which is what makes
        // changing it here safe.
        let previous = env::var_os("CARGO_TARGET_DIR");
        unsafe { env::set_var("CARGO_TARGET_DIR", &target) };

        assert!(
            !image_stale(&build, "debug", Arch::X86_64).unwrap(),
            "an image newer than everything it was built from is not stale"
        );

        // The image is the kernel, the loader, the initramfs and the root
        // filesystem; rebuilding any one of them has to make it stale, or
        // `run` boots code that is no longer on disk.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&kernel, "rebuilt").unwrap();
        assert!(
            image_stale(&build, "debug", Arch::X86_64).unwrap(),
            "a kernel rebuilt after the image has to trigger the rebuild"
        );

        // And the two machines' images are two images: the second machine's is
        // missing at this point, which is stale for a reason that has nothing to
        // do with the kernel above.
        assert!(
            image_stale(&build, "debug", Arch::Aarch64).unwrap(),
            "one machine's image is not the other's"
        );

        match previous {
            Some(value) => unsafe { env::set_var("CARGO_TARGET_DIR", value) },
            None => unsafe { env::remove_var("CARGO_TARGET_DIR") },
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
