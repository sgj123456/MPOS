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
        .unwrap_or_else(|| "debug".to_string())
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

pub fn kernel_path(profile: &str) -> Result<PathBuf> {
    Ok(target_dir()?
        .join("x86_64-unknown-none")
        .join(profile)
        .join("mpos"))
}

pub fn loader_path(profile: &str) -> Result<PathBuf> {
    Ok(target_dir()?
        .join("x86_64-unknown-uefi")
        .join(profile)
        .join("mpos-boot.efi"))
}

pub fn initramfs_path(build_dir: &Path) -> PathBuf {
    build_dir.join("initramfs.tar")
}

pub fn image_path(build_dir: &Path) -> PathBuf {
    build_dir.join("mpos-uefi-x86_64.img")
}

/// Marks a disk image whose root filesystem has been installed. The image's
/// own mtime cannot say that - `disk` creating the file makes it newer than
/// the configuration, which would read as "the rootfs was built after the last
/// change" and skip the build entirely.
pub fn rootfs_stamp(build_dir: &Path) -> PathBuf {
    build_dir.join(".rootfs.stamp")
}

pub fn ovmf_vars_path(build_dir: &Path) -> PathBuf {
    build_dir.join("OVMF_VARS.4m.fd")
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

pub fn dir_newer_than(dir: &Path, after: SystemTime) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if dir_newer_than(&path, after)? {
                return Ok(true);
            }
        } else if file_mtime(&path)? > after {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn build_kernel(profile: &str) -> Result<()> {
    let root = project_root()?;
    println!("Building the kernel ({profile})");

    let mut kernel_build = Command::new("cargo");
    kernel_build.arg("build");
    if profile == "release" {
        kernel_build.arg("--release");
    }
    kernel_build.arg("-p").arg("mpos").current_dir(&root);
    let status = kernel_build
        .status()
        .context("Failed to run `cargo build` for the kernel")?;
    if !status.success() {
        anyhow::bail!("The kernel did not build");
    }

    println!("Building the EFI loader ({profile})");
    let mut loader_build = Command::new("cargo");
    loader_build.arg("build");
    if profile == "release" {
        loader_build.arg("--release");
    }
    loader_build
        .arg("-p")
        .arg("mpos-boot")
        .arg("--target")
        .arg("x86_64-unknown-uefi")
        .current_dir(&root);
    let status = loader_build
        .status()
        .context("Failed to run `cargo build` for the EFI loader")?;
    if !status.success() {
        anyhow::bail!("The EFI loader did not build");
    }

    let kernel = kernel_path(profile)?;
    let loader = loader_path(profile)?;
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

/// Copies a tree onto another, keeping symlinks as symlinks. `cp -a` does this
/// in the shell; `std::fs::copy` alone would turn a link into a second copy of
/// its target, which is wrong for every `/bin/*` in an Alpine tree.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
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
            copy_dir_recursive(&from, &to)?;
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
pub fn umount_recursively(mount_point: &Path) {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return;
    };
    let prefix = mount_point.to_string_lossy().to_string();
    let nested = format!("{prefix}/");
    let mut points: Vec<String> = mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|point| point.replace("\\040", " "))
        .filter(|point| point == &prefix || point.starts_with(&nested))
        .collect();
    points.sort_by_key(|point| std::cmp::Reverse(point.len()));
    for point in points {
        let _ = Command::new("umount").arg(&point).status();
    }
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

fn wget(url: &str, dest: &Path) -> Result<bool> {
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
}
