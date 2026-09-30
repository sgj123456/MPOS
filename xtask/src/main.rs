//! `cargo xtask`: the build front end for mpos.
//!
//! The commands mirror `scripts/mkdisk.sh` and `scripts/run.sh`, privilege
//! included: cargo runs as the user who invoked the command, root is taken only
//! for the steps that need it, and everything root writes is handed back to
//! that user afterwards.

mod config;
mod disk;
mod initramfs;
mod qemu;
mod rootfs;
mod selftest;
mod utils;

use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Build and management tools for mpos")]
struct Cli {
    /// Build directory (default: $MPOS_BUILD_DIR, or build/)
    #[arg(long)]
    build_dir: Option<PathBuf>,

    /// Build profile (default: $PROFILE, or debug)
    #[arg(long)]
    profile: Option<String>,

    /// How to boot the machine (default: `[qemu] mode` in `machine.toml`, or
    /// mirror): `terminal` (serial only, no window), `gui` (window only, the
    /// console on the framebuffer), `mirror` (both at once), or `weston` (the
    /// window is the compositor's, and the console stays off the framebuffer).
    /// Global so `cargo xtask run --mode weston` accepts it after the
    /// subcommand, where a plain parent flag would be rejected.
    #[arg(long, value_name = "MODE", global = true)]
    mode: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Subcommand)]
enum Commands {
    /// Build the kernel and the EFI loader
    Build,
    /// Build the initramfs
    Initramfs,
    /// Install the root filesystem into the disk image
    Rootfs,
    /// Create the (empty) disk image
    Disk,
    /// Boot the disk image in QEMU
    Run,
    /// Build everything: kernel, initramfs, disk image, root filesystem
    All,
    /// Run the self-tests: host unit tests plus every user test program,
    /// booted in QEMU from a test initramfs
    Test,
    /// Remove the build and target directories
    Clean,
}

impl Commands {
    fn name(self) -> &'static str {
        match self {
            Commands::Build => "build",
            Commands::Initramfs => "initramfs",
            Commands::Rootfs => "rootfs",
            Commands::Disk => "disk",
            Commands::Run => "run",
            Commands::All => "all",
            Commands::Test => "test",
            Commands::Clean => "clean",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let build_dir = utils::resolve_build_dir(cli.build_dir.as_deref())?;
    let profile = utils::resolve_profile(cli.profile.as_deref());

    // A build from before this command knew about ownership can leave a
    // root-owned `build/` or `target/` behind, and cargo cannot write into a
    // root-owned tree. Repair it before anything tries to.
    utils::repair_ownership(&build_dir)?;

    // Whether the kernel is already built for this command. It is true in the
    // sudo half of a build, which is the one process that must never run
    // cargo: `target/` would come back owned by root.
    let artefacts_ready = env::var_os("MPOS_XTASK_ARTEFACTS_READY").is_some();

    // `run` boots a kernel, so the kernel has to be the current one. Cargo is
    // run first - and is a no-op when nothing has changed - because an image
    // built from a kernel that has since been rebuilt is exactly the stale
    // image this is supposed to catch: editing kernel sources and running
    // `run` must rebuild the image, not boot the old code.
    let mut kernel_built = artefacts_ready;
    if matches!(cli.command, Commands::Run) && !kernel_built {
        utils::build_kernel(&profile)?;
        kernel_built = true;
    }

    // `run --mode X` re-points the image at a different boot mode. That is
    // a change to one file inside the image (`mpos.cmdline`), so it takes root
    // only to refresh the boot files - not to reinstall the root filesystem -
    // which `build_rootfs` does when the mode it is handed differs from the one
    // already baked. The desired mode travels to the privileged half through
    // `MPOS_XTASK_MODE`.
    let run_mode = match cli.mode.as_deref() {
        Some(text) => Some(config::RunMode::parse(text).ok_or_else(|| {
            anyhow::anyhow!("unknown mode {text:?}; expected terminal, gui, mirror or weston")
        })?),
        // With no `--mode`, `run` boots the mode `machine.toml` names rather
        // than whatever the image happens to have been baked with. Reading it
        // here is what makes editing that key cheap: `needs_mode_switch` below
        // then sees the difference and refreshes the image's *boot files*, the
        // same path `--mode` takes - where before, the key lived in
        // `rootfs.toml` and changing it marked the whole image stale and
        // reinstalled the Alpine tree for one line of `mpos.cmdline`.
        None if matches!(cli.command, Commands::Run) => {
            Some(config::QemuConfig::load_default()?.mode)
        }
        None => None,
    };
    if matches!(cli.command, Commands::Run)
        && let Some(mode) = run_mode
    {
        // SAFETY: single-threaded here (cargo has not spawned any of our
        // threads), and no other code reads the environment concurrently in
        // this process before we `exec`/`spawn` the privileged half.
        unsafe { env::set_var("MPOS_XTASK_MODE", mode.name()) };
    }

    // `run` needs root only when the image has to be rebuilt, and it has to be
    // rebuilt whenever the kernel, the loader, the initramfs or the root
    // filesystem has changed since it was assembled - or when `--mode` asks
    // for a mode other than the one the image currently carries. The boot itself
    // belongs to the user, not to root.
    let mut run_needs_build = match cli.command {
        Commands::Run => {
            qemu::needs_build(&build_dir, &profile)?
                || run_mode.is_some_and(|mode| qemu::needs_mode_switch(&build_dir, mode))
        }
        _ => false,
    };

    let needs_root = match cli.command {
        Commands::Build | Commands::Disk | Commands::Test => false,
        Commands::Clean => utils::clean_needs_root(&build_dir),
        Commands::Initramfs | Commands::Rootfs | Commands::All => true,
        Commands::Run => run_needs_build,
    };

    if needs_root && !utils::is_root() {
        // Build with cargo before taking root: cargo writes into $HOME and
        // $CARGO_TARGET_DIR, and a `target/` cargo made as root is one the user
        // cannot write into again - the state a build that re-execs first ends
        // in.
        if !kernel_built && needs_kernel(cli.command, run_needs_build) {
            utils::build_kernel(&profile)?;
        }
        if matches!(cli.command, Commands::Run) {
            // Build in a child, and wait: this process still has to boot the
            // result, and a process that replaced itself with the build never
            // could - which is why `run` used to end by asking to be run again.
            // The child is handed the same `--mode` so it recomputes the same
            // switch decision (and bakes it) rather than booting.
            build_under_sudo(cli.command, run_mode, &build_dir, &profile)?;
            if qemu::needs_build(&build_dir, &profile)? {
                anyhow::bail!(
                    "The image is still out of date after building it; the output above says why."
                );
            }
            run_needs_build = false;
        } else {
            escalate(cli.command, run_mode, &build_dir, &profile)?;
        }
    }

    // The host wiring follows the mode the user asked for (or, with no
    // `--mode`, the mode the image is already baked in), so the window and
    // the serial line always match the consoles the guest was told to use. An
    // image with no marker has just been refreshed above (`needs_mode_switch`
    // is true for one whatever mode was asked for), so the fallback is only
    // read on a build that is about to happen anyway.
    let boot_mode = run_mode
        .or_else(|| qemu::baked_mode(&build_dir))
        .unwrap_or(config::RunMode::Mirror);

    let result = dispatch(
        cli.command,
        &build_dir,
        &profile,
        artefacts_ready,
        run_needs_build,
        boot_mode,
    );

    // Root wrote into the build directory. Hand the tree back to the user who
    // asked for the build.
    if utils::is_root()
        && let Some(owner) = utils::build_owner()
    {
        hand_back(&build_dir, owner);
    }

    result
}

/// Gives the build directory - and, when it is not already the user's, the
/// target directory cargo writes to - back to `owner`, warning rather than
/// failing if something cannot be handed over: the build succeeding is the
/// point, and a root-owned file is a cleanliness problem, not a broken build.
fn hand_back(build_dir: &Path, owner: utils::Owner) {
    if owner.is_root() {
        return;
    }

    // The privileged half writes into `build/` every time it runs, so that tree
    // always has something to hand back.
    if let Err(err) = utils::chown_tree(build_dir, owner) {
        warn_hand_back(build_dir, owner, &err);
    }

    // `target/` belongs to cargo, and cargo runs as the user now: only a tree
    // left over from a build that ran as root needs handing back, and the top
    // of the tree is enough to tell.
    if let Ok(target) = utils::target_dir()
        && target.exists()
        && !utils::owned_by(&target, owner.uid)
        && let Err(err) = utils::chown_tree(&target, owner)
    {
        warn_hand_back(&target, owner, &err);
    }
}

fn warn_hand_back(path: &Path, owner: utils::Owner, err: &anyhow::Error) {
    eprintln!(
        "xtask: warning: could not give {} back to {}:{}: {err}",
        path.display(),
        owner.uid,
        owner.gid
    );
}

/// The commands whose steps install the kernel build into the image, and so
/// need it built before root is taken.
fn needs_kernel(command: Commands, run_needs_build: bool) -> bool {
    match command {
        Commands::Rootfs | Commands::All => true,
        Commands::Run => run_needs_build,
        _ => false,
    }
}

fn dispatch(
    command: Commands,
    build_dir: &Path,
    profile: &str,
    artefacts_ready: bool,
    run_needs_build: bool,
    mode: config::RunMode,
) -> Result<()> {
    match command {
        Commands::Build => utils::build_kernel(profile),
        Commands::Initramfs => initramfs::build_initramfs(build_dir, profile),
        Commands::Disk => disk::build_disk(build_dir, profile),
        Commands::Rootfs => {
            if !artefacts_ready {
                utils::build_kernel(profile)?;
            }
            // `build_rootfs` brings the initramfs up to date itself: it is one
            // of the inputs the image is assembled from.
            rootfs::build_rootfs(build_dir, profile)
        }
        Commands::All => {
            if !artefacts_ready {
                utils::build_kernel(profile)?;
            }
            disk::ensure(build_dir, profile)?;
            rootfs::ensure(build_dir, profile)
        }
        Commands::Run => qemu::run_qemu(build_dir, profile, artefacts_ready, run_needs_build, mode),
        Commands::Test => selftest::run_tests(build_dir, profile),
        Commands::Clean => utils::clean(build_dir),
    }
}

/// Runs the privileged half in a child sudo process instead of replacing this
/// one, and waits for it.
///
/// `escalate` execs, which is right for a command whose work ends when the
/// privileged half does - but a `run` that needs to build still has to boot
/// afterwards, and a process that replaced itself with the build never could.
/// The child is told to skip the cargo build: this process already did that as
/// the user, and doing it again as root is what used to make root the owner of
/// `target/`.
fn build_under_sudo(
    command: Commands,
    mode: Option<config::RunMode>,
    build_dir: &Path,
    profile: &str,
) -> Result<()> {
    let exe = env::current_exe().context("Failed to find the xtask executable")?;
    let home = env::var("HOME").unwrap_or_default();
    let path = env::var("PATH").unwrap_or_default();

    let mut sudo = Command::new("sudo");
    sudo.arg("-E")
        .arg("env")
        .arg(format!("HOME={home}"))
        .arg(format!("PATH={path}"))
        .arg("MPOS_XTASK_SUDO=1")
        .arg("MPOS_XTASK_ARTEFACTS_READY=1")
        .arg(format!("MPOS_XTASK_UID={}", utils::current_uid()))
        .arg(format!("MPOS_XTASK_GID={}", utils::current_gid()));
    // Carry a requested mode across the sudo boundary - as an `env`
    // assignment before the program name (so the privileged refresh bakes it)
    // and as `--mode` after the subcommand (so the child recomputes the same
    // switch decision and bakes, rather than booting the old mode).
    if let Some(mode) = env::var_os("MPOS_XTASK_MODE") {
        sudo.arg(format!("MPOS_XTASK_MODE={}", mode.to_string_lossy()));
    }
    sudo.arg(exe)
        .arg("--build-dir")
        .arg(build_dir)
        .arg("--profile")
        .arg(profile)
        .arg(command.name());
    if let Some(mode) = mode {
        sudo.arg("--mode").arg(mode.name());
    }

    let status = sudo.status().context("Failed to run sudo")?;
    if !status.success() {
        anyhow::bail!("The privileged part of the build failed");
    }
    Ok(())
}

/// Re-runs this command as root, carrying the invoking user's identity along so
/// the output can be given back when the privileged half is done. `exec` rather
/// than `spawn` keeps one process and one exit status for the whole command.
fn escalate(
    command: Commands,
    mode: Option<config::RunMode>,
    build_dir: &Path,
    profile: &str,
) -> Result<()> {
    let exe = env::current_exe().context("Failed to find the xtask executable")?;
    let home = env::var("HOME").unwrap_or_default();
    let path = env::var("PATH").unwrap_or_default();

    let mut sudo = Command::new("sudo");
    sudo.arg("-E")
        .arg("env")
        .arg(format!("HOME={home}"))
        .arg(format!("PATH={path}"))
        .arg("MPOS_XTASK_SUDO=1")
        .arg("MPOS_XTASK_ARTEFACTS_READY=1")
        .arg(format!("MPOS_XTASK_UID={}", utils::current_uid()))
        .arg(format!("MPOS_XTASK_GID={}", utils::current_gid()));
    if let Some(mode) = env::var_os("MPOS_XTASK_MODE") {
        sudo.arg(format!("MPOS_XTASK_MODE={}", mode.to_string_lossy()));
    }
    sudo.arg(exe)
        .arg("--build-dir")
        .arg(build_dir)
        .arg("--profile")
        .arg(profile)
        .arg(command.name());
    if let Some(mode) = mode {
        sudo.arg("--mode").arg(mode.name());
    }

    let err = sudo.exec();
    anyhow::bail!("Failed to re-exec under sudo: {err}");
}
