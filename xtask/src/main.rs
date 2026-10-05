//! `cargo xtask`: the build front end for mpos.
//!
//! The commands mirror `scripts/mkdisk.sh` and `scripts/run.sh`, privilege
//! included: cargo runs as the user who invoked the command, root is taken only
//! for the steps that need it, and everything root writes is handed back to
//! that user afterwards.

mod aarch64;
mod aarch64_bin;
mod aarch64_init;
mod config;
mod depgraph;
mod disk;
mod gates;
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
    /// mirror): `terminal` (serial only, no window), `gui` (the whole machine in
    /// the window — a window with every device behind it and the text console
    /// shown there, the serial line copied to `build/serial.log` so the host
    /// terminal stays quiet), or `mirror` (the console in the window and a live
    /// copy on the host terminal). Global so `cargo xtask run --mode gui`
    /// accepts it after the subcommand, where a plain parent flag would be rejected.
    #[arg(long, value_name = "MODE", global = true)]
    mode: Option<String>,

    /// `run`: which machine to boot. `x86_64` (default) boots the disk image on
    /// the QEMU wiring in `machine.toml`; `aarch64` builds the second machine's
    /// kernel and UEFI loader, lays out its ESP, and boots it under AAVMF with
    /// the serial line on the terminal. Global for the same reason `--mode` is.
    #[arg(long, value_name = "ARCH", global = true, default_value = "x86_64")]
    arch: String,

    /// `test`: groups of user tests to run in addition to the ones that are in
    /// no group (default: the `default` list in `tests.toml`, which is empty).
    /// A named group *replaces* that list rather than adding to it. Global for
    /// the same reason `--mode` is - the flag is written after the subcommand.
    #[arg(long, value_name = "GROUPS", global = true, value_delimiter = ',')]
    groups: Vec<String>,

    /// `test`: run every group, including the ones `tests.toml` leaves out of
    /// the default sweep (`external`, `slow`, `diagnostic`).
    #[arg(long, global = true)]
    all_groups: bool,

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
    /// Run the self-tests: host unit tests plus the user test programs
    /// `tests.toml` selects, booted in QEMU from a test initramfs
    Test,
    /// Measure the kernel's module graph and check it against the ratchet
    /// (PLAN.md §11.2 Step G), so the cycle count cannot grow back
    Depgraph,
    /// Build the aarch64 kernel and its UEFI loader, lay out an ESP and boot it
    /// under AAVMF. The second machine's own boot path, and stage 1e's
    /// instrument: it reads the verdict off the PL011 (`PLAN.md` §9.1)
    BootAarch64,
    /// Install a real Alpine aarch64 userland into the second machine's disk
    /// image and boot it headless, reporting how far it gets
    /// (`PLAN.md` §9.1 stage 7). Unprivileged: the install runs under
    /// `fakeroot`, so the tree keeps root ownership without taking root
    AlpineAarch64,
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
            Commands::Depgraph => "depgraph",
            Commands::BootAarch64 => "boot-aarch64",
            Commands::AlpineAarch64 => "alpine-aarch64",
            Commands::Clean => "clean",
        }
    }
}

/// The machine a `test` run sweeps, named by `--arch`.
#[derive(Debug)]
enum Sweep {
    X86_64,
    Aarch64,
}

/// Which machine's sweep `test` runs.
///
/// The two are separate paths rather than one parameterised sweep: x86_64 boots
/// a test initramfs on the QEMU wiring in `machine.toml` and the guest ends the
/// machine with a debug exit, while aarch64 packs a musl runner and the test
/// programs onto its own ESP and disk and ends on `=== END`. What they share is
/// the verdict: the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol, read by the
/// same shape of check. Anything that is neither is refused here rather than
/// falling through to one of them - a sweep that prints a verdict for a machine
/// it never booted is the one failure this must not have (`AGENTS.md` §3).
fn test_sweep_machine(arch: &str) -> Result<Sweep> {
    match arch {
        "x86_64" => Ok(Sweep::X86_64),
        "aarch64" => Ok(Sweep::Aarch64),
        other => anyhow::bail!(
            "test --arch {other}: this front end sweeps x86_64 and aarch64 only, \
             and a verdict for a machine nothing booted is the one failure it \
             must not have (PLAN.md §13.5)"
        ),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let build_dir = utils::resolve_build_dir(cli.build_dir.as_deref())?;
    let profile = utils::resolve_profile(cli.profile.as_deref());

    // The only machines this front end boots. An unrecognised `--arch` is an
    // error, not a silent fall-through to x86_64: a sweep that quietly boots the
    // wrong machine is the one failure this must not have.
    if cli.arch != "x86_64" && cli.arch != "aarch64" {
        anyhow::bail!(
            "--arch {}: this xtask builds and boots x86_64 and aarch64 only",
            cli.arch
        );
    }

    // `run --arch aarch64` is the second machine's own path: `run_interactive`
    // builds the aarch64 kernel and loader, lays out its AAVMF ESP with the real
    // Alpine root on the disk (§13.19), and reads the run mode through
    // `MPOS_XTASK_MODE` / `machine.toml` itself - the same sources the x86_64
    // half below consults - so it needs none of that disk / rootfs / sudo
    // machinery. Take it before any of it runs.
    if matches!(cli.command, Commands::Run) && cli.arch == "aarch64" {
        // Validate and export the desired mode *before* taking the arm, so a
        // bad `--mode` word fails here exactly as it does for x86_64 (the
        // aarch64 session reads it back through `rootfs::effective_mode`, the
        // same single source both machines share).
        let mode = match cli.mode.as_deref() {
            Some(text) => Some(config::RunMode::parse_strict(text)?),
            None => Some(config::QemuConfig::load_default()?.run_mode()?),
        };
        if let Some(mode) = mode {
            // SAFETY: single-threaded here (cargo has not spawned any of our
            // threads), and no other code reads the environment concurrently
            // in this process.
            unsafe { env::set_var("MPOS_XTASK_MODE", mode.name()) };
        }
        return aarch64::run_interactive(&build_dir, &profile);
    }

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
        Some(text) => Some(config::RunMode::parse_strict(text)?),
        // With no `--mode`, `run` boots the mode `machine.toml` names rather
        // than whatever the image happens to have been baked with. Reading it
        // here is what makes editing that key cheap: `needs_mode_switch` below
        // then sees the difference and refreshes the image's *boot files*, the
        // same path `--mode` takes - where before, the key lived in
        // `rootfs.toml` and changing it marked the whole image stale and
        // reinstalled the Alpine tree for one line of `mpos.cmdline`.
        //
        // A key naming a word this crate no longer has - `weston` was one - is
        // reported here rather than defaulted: the consoles baked differ between
        // the modes, so booting the default instead would be a machine nobody asked
        // for. `test` never reaches this and so never fails on it.
        None if matches!(cli.command, Commands::Run) => {
            Some(config::QemuConfig::load_default()?.run_mode()?)
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
        Commands::Build
        | Commands::Disk
        | Commands::Test
        | Commands::Depgraph
        | Commands::BootAarch64
        | Commands::AlpineAarch64 => false,
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
        &cli.arch,
        &build_dir,
        &profile,
        artefacts_ready,
        run_needs_build,
        boot_mode,
        selftest::Selection {
            groups: cli.groups.clone(),
            all: cli.all_groups,
        },
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
    arch: &str,
    build_dir: &Path,
    profile: &str,
    artefacts_ready: bool,
    run_needs_build: bool,
    mode: config::RunMode,
    tests: selftest::Selection,
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
        Commands::Test => match test_sweep_machine(arch)? {
            Sweep::X86_64 => selftest::run_tests(build_dir, profile, tests),
            Sweep::Aarch64 => {
                // The aarch64 payload is every aarch64 program that builds; the
                // group vocabulary in `tests.toml` names the x86_64 suites'
                // membership (and the bare-metal conformance suite has no
                // aarch64 port yet), so honouring `--groups` here would be a
                // selection this arm cannot make. Refuse rather than run a
                // different set than asked for.
                if !tests.groups.is_empty() || tests.all {
                    anyhow::bail!(
                        "--groups/--all-groups select among x86_64's programs today; \
                         the aarch64 sweep runs every aarch64 program it built \
                         (PLAN.md §13.5)"
                    );
                }
                aarch64::run_sweep(build_dir, profile)
            }
        },
        Commands::Depgraph => depgraph::run(&utils::project_root()?),
        // Builds both binaries itself rather than through `build_kernel`: that
        // one is the x86_64 kernel and the x86_64 loader, and this machine's
        // pair is a different target for each.
        Commands::BootAarch64 => aarch64::run(build_dir, profile),
        // Also builds both binaries itself, and lays out its own ESP and disk.
        // Unprivileged by construction: the Alpine install runs under
        // `fakeroot`, so this command needs no sudo and is *not* in the
        // `needs_root` list below.
        Commands::AlpineAarch64 => aarch64::run_alpine(build_dir, profile),
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

#[cfg(test)]
mod tests {
    use super::{Sweep, test_sweep_machine};

    /// The counterfactual for the silent wrong-run: `--arch aarch64` used to
    /// reach `selftest::run_tests` unchanged and print an x86_64 verdict under an
    /// aarch64 banner. Each machine now gets its own arm, and a machine this
    /// front end does not sweep is refused by name rather than run as another.
    #[test]
    fn the_sweep_never_runs_a_machine_it_was_not_asked_for() {
        assert!(matches!(test_sweep_machine("x86_64"), Ok(Sweep::X86_64)));
        assert!(matches!(test_sweep_machine("aarch64"), Ok(Sweep::Aarch64)));
        let refusal = test_sweep_machine("riscv64")
            .expect_err("a machine this front end does not sweep must be refused")
            .to_string();
        assert!(
            refusal.contains("riscv64") && refusal.contains("x86_64"),
            "the refusal must name the machine asked for and the ones that are \
             swept: {refusal}"
        );
    }
}
