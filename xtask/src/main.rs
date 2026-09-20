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

    // `run` needs root only when there is nothing to boot yet, because then it
    // has to build. The boot itself belongs to the user, not to root.
    let run_needs_build = match cli.command {
        Commands::Run => qemu::needs_build(&build_dir)?,
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
        if needs_kernel(cli.command, run_needs_build) {
            utils::build_kernel(&profile)?;
        }
        escalate(cli.command, &build_dir, &profile)?;
    }

    let artefacts_ready = env::var_os("MPOS_XTASK_ARTEFACTS_READY").is_some();
    let result = dispatch(
        cli.command,
        &build_dir,
        &profile,
        artefacts_ready,
        run_needs_build,
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
) -> Result<()> {
    match command {
        Commands::Build => utils::build_kernel(profile),
        Commands::Initramfs => initramfs::build_initramfs(build_dir, profile),
        Commands::Disk => disk::build_disk(build_dir, profile),
        Commands::Rootfs => {
            if !artefacts_ready {
                utils::build_kernel(profile)?;
            }
            initramfs::ensure(build_dir, profile)?;
            rootfs::build_rootfs(build_dir, profile)
        }
        Commands::All => {
            if !artefacts_ready {
                utils::build_kernel(profile)?;
            }
            initramfs::ensure(build_dir, profile)?;
            disk::ensure(build_dir, profile)?;
            rootfs::ensure(build_dir, profile)
        }
        Commands::Run => qemu::run_qemu(build_dir, profile, artefacts_ready, run_needs_build),
        Commands::Test => selftest::run_tests(build_dir, profile),
        Commands::Clean => utils::clean(build_dir),
    }
}

/// Re-runs this command as root, carrying the invoking user's identity along so
/// the output can be given back when the privileged half is done. `exec` rather
/// than `spawn` keeps one process and one exit status for the whole command.
fn escalate(command: Commands, build_dir: &Path, profile: &str) -> Result<()> {
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
        .arg(format!("MPOS_XTASK_GID={}", utils::current_gid()))
        .arg(exe)
        .arg("--build-dir")
        .arg(build_dir)
        .arg("--profile")
        .arg(profile)
        .arg(command.name());

    let err = sudo.exec();
    anyhow::bail!("Failed to re-exec under sudo: {err}");
}
