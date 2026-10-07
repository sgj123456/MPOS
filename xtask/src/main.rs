//! `cargo xtask`: the build front end for mpos.
//!
//! The commands mirror `scripts/mkdisk.sh` and `scripts/run.sh`, privilege
//! included: cargo runs as the user who invoked the command, root is taken only
//! for the steps that need it, and everything root writes is handed back to
//! that user afterwards.

mod aarch64;
mod arch;
mod config;
mod depgraph;
mod disk;
mod fat;
mod gates;
mod initramfs;
mod qemu;
mod rootfs;
mod selftest;
mod sweep;
mod utils;

use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::arch::Arch;

#[derive(Parser)]
#[command(name = "xtask", about = "Build and management tools for mpos")]
struct Cli {
    /// Build directory (default: $MPOS_BUILD_DIR, or build/)
    #[arg(long)]
    build_dir: Option<PathBuf>,

    /// Build profile (default: $PROFILE, or debug)
    #[arg(long)]
    profile: Option<String>,

    /// How to boot the machine (default: `qemu.mode` in `machine.toml`, or
    /// mirror): `terminal` (serial only, no window), `gui` (the whole machine in
    /// the window — a window with every device behind it and the text console
    /// shown there, the serial line copied to `build/serial.log` so the host
    /// terminal stays quiet), or `mirror` (the console in the window and a live
    /// copy on the host terminal). Global so `cargo xtask run --mode gui`
    /// accepts it after the subcommand, where a plain parent flag would be rejected.
    #[arg(long, value_name = "MODE", global = true)]
    mode: Option<String>,

    /// Which machine to build, boot and sweep: `x86_64` (default) or `aarch64`.
    /// Global so `cargo xtask run --arch aarch64` accepts it after the
    /// subcommand, where a plain parent flag would be rejected.
    ///
    /// It is the *whole* switch, not a hint to `run` and `test`: `build`,
    /// `rootfs` and `all` compile for the machine it names too, so the pair of
    /// binaries every command then picks up is the pair that machine boots
    /// (`AGENTS.md` §2). Parsed once, up front, into an `Arch`, so no command
    /// downstream compares this string again.
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
    /// Build the aarch64 kernel and its UEFI loader, lay out the same GPT boot
    /// disk the first machine boots off, and boot it under AAVMF, reading the
    /// verdict off the PL011 marker chain (`PLAN.md` §9.1)
    BootAarch64,
    /// Install a real Alpine aarch64 userland into the second machine's disk
    /// image and boot it headless, reporting how far it gets
    /// (`PLAN.md` §13.12). Unprivileged: the install runs under
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let build_dir = utils::resolve_build_dir(cli.build_dir.as_deref())?;
    let profile = utils::resolve_profile(cli.profile.as_deref());

    // The only machines this front end builds. An unrecognised `--arch` is an
    // error, not a silent fall-through to x86_64: a build that quietly compiles
    // one machine while reporting another is the one failure this must not
    // have. Parsing it here - before any command - is what makes it the switch
    // for all of them rather than a hint two of them read.
    let arch = Arch::parse(&cli.arch)?;

    // `disk`, `rootfs` and `all` assemble *the x86_64 disk image* - a GPT disk
    // with an Alpine root on it - and the second machine's disk is not that: it
    // is laid out by `boot-aarch64` / `alpine-aarch64` and by `run --arch
    // aarch64`. So `--arch aarch64` here would build an x86_64 root filesystem
    // under an aarch64 banner, which is the silent wrong-machine failure `AGENTS.md`
    // §3 refuses. Name the machine's own command in the refusal rather than doing
    // the other machine's work.
    //
    // `initramfs` is *not* in that set any more: both machines unpack an archive
    // and switch root from its script, the pipeline is one copy
    // (`xtask/src/initramfs.rs`), and the machine only picks which toml, overlay
    // and applet farm it uses - so it builds the machine it was asked for.
    if arch == Arch::Aarch64
        && matches!(
            cli.command,
            Commands::Disk | Commands::Rootfs | Commands::All
        )
    {
        anyhow::bail!(
            "--arch aarch64 with `{}`: the image commands build the x86_64 disk. \
             The second machine's disk is laid out by `cargo xtask boot-aarch64`, \
             `cargo xtask alpine-aarch64` and `cargo xtask run --arch aarch64`",
            cli.command.name()
        );
    }

    // `run --arch aarch64` is the second machine's own path: `run_interactive`
    // builds the aarch64 kernel and loader, lays out its AAVMF ESP with the real
    // Alpine root on the disk (§13.19), and reads the run mode through
    // `MPOS_XTASK_MODE` / `machine.toml` itself - the same sources the x86_64
    // half below consults - so it needs none of that disk / rootfs / sudo
    // machinery. Take it before any of it runs.
    if matches!(cli.command, Commands::Run) && arch == Arch::Aarch64 {
        // Validate and export the desired mode *before* taking the arm, so a
        // bad `--mode` word fails here exactly as it does for x86_64 (the
        // aarch64 session reads it back through `rootfs::effective_mode`, the
        // same single source both machines share).
        let mode = match cli.mode.as_deref() {
            Some(text) => Some(config::RunMode::parse_strict(text)?),
            None => Some(config::QemuConfig::load_default(arch)?.run_mode()?),
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
        utils::build_kernel(&profile, arch)?;
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
            Some(config::QemuConfig::load_default(arch)?.run_mode()?)
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
        // Root only where the pipeline needs to *exec* the tree it just laid:
        // the first machine's applet farm is `chroot <staging> /bin/busybox
        // --install -s`, and a chroot is root. The second machine cannot run its
        // cross-arch busybox at all, so it links its farm by hand and installs
        // its packages with apk's `--usermode` - a command that only writes
        // files, which is why the retired `aarch64-initramfs` needed no root
        // either.
        Commands::Initramfs => arch == Arch::X86_64,
        Commands::Rootfs | Commands::All => true,
        Commands::Run => run_needs_build,
    };

    if needs_root && !utils::is_root() {
        // Build with cargo before taking root: cargo writes into $HOME and
        // $CARGO_TARGET_DIR, and a `target/` cargo made as root is one the user
        // cannot write into again - the state a build that re-execs first ends
        // in.
        if !kernel_built && needs_kernel(cli.command, run_needs_build) {
            utils::build_kernel(&profile, arch)?;
        }
        if matches!(cli.command, Commands::Run) {
            // Build in a child, and wait: this process still has to boot the
            // result, and a process that replaced itself with the build never
            // could - which is why `run` used to end by asking to be run again.
            // The child is handed the same `--mode` so it recomputes the same
            // switch decision (and bakes it) rather than booting.
            build_under_sudo(cli.command, run_mode, &build_dir, &profile, arch)?;
            if qemu::needs_build(&build_dir, &profile)? {
                anyhow::bail!(
                    "The image is still out of date after building it; the output above says why."
                );
            }
            run_needs_build = false;
        } else {
            escalate(cli.command, run_mode, &build_dir, &profile, arch)?;
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
        Invocation {
            arch,
            build_dir: &build_dir,
            profile: &profile,
            artefacts_ready,
            run_needs_build,
            mode: boot_mode,
        },
        sweep::Selection {
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

/// What every command's dispatch needs to know about *this* invocation: the
/// machine, where the build lands, which profile it uses, and the facts the
/// pre-dispatch steps in [`main`] worked out (which artefacts are already
/// current, and which mode the emulator should boot with).
///
/// A struct rather than seven arguments because these travel together as one
/// thing - they are the resolved command line - and because it keeps the
/// `match` below readable: each arm names only the fields it actually reads.
struct Invocation<'a> {
    arch: Arch,
    build_dir: &'a Path,
    profile: &'a str,
    /// Set when the kernel and loader are known to be current, so a command
    /// that would otherwise rebuild them can skip it.
    artefacts_ready: bool,
    /// Set when `run` has to build before it boots.
    run_needs_build: bool,
    /// The mode the emulator should boot with, already resolved from
    /// `--mode`/`machine.toml`/the image's own marker.
    mode: config::RunMode,
}

fn dispatch(command: Commands, invocation: Invocation<'_>, tests: sweep::Selection) -> Result<()> {
    let Invocation {
        arch,
        build_dir,
        profile,
        artefacts_ready,
        run_needs_build,
        mode,
    } = invocation;
    match command {
        Commands::Build => utils::build_kernel(profile, arch),
        Commands::Initramfs => initramfs::build_initramfs(build_dir, arch),
        Commands::Disk => disk::build_disk(build_dir, profile),
        Commands::Rootfs => {
            if !artefacts_ready {
                utils::build_kernel(profile, arch)?;
            }
            // `build_rootfs` brings the initramfs up to date itself: it is one
            // of the inputs the image is assembled from.
            rootfs::build_rootfs(build_dir, profile)
        }
        Commands::All => {
            if !artefacts_ready {
                utils::build_kernel(profile, arch)?;
            }
            disk::ensure(build_dir, profile)?;
            rootfs::ensure(build_dir, profile)
        }
        Commands::Run => qemu::run_qemu(build_dir, profile, artefacts_ready, run_needs_build, mode),
        // Both machines run the same sweep now: a disk with the test payload
        // staged on its root, the shared `TEST_INIT` walking `/bin/t-*`, and
        // the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol read off the
        // serial line (`selftest::run_disk_sweep`). What still differs is the
        // *selection*: x86_64's programs are named by `tests.toml`'s groups,
        // and aarch64's group vocabulary does not exist yet, so this arm
        // refuses a selection it cannot honour rather than running a different
        // set than was asked for.
        Commands::Test => match arch {
            Arch::X86_64 => selftest::run_tests(build_dir, profile, tests),
            Arch::Aarch64 => {
                if !tests.groups.is_empty() || tests.all {
                    anyhow::bail!(
                        "--groups/--all-groups select among x86_64's programs today; \
                         the aarch64 sweep runs every aarch64 program it built \
                         (PLAN.md §13.5)"
                    );
                }
                selftest::run_disk_sweep(build_dir, profile, arch)
            }
        },
        Commands::Depgraph => depgraph::run(&utils::project_root()?),
        // The second machine's own entry points. Each builds this machine's
        // kernel and loader pair through `build_kernel(.., Arch::Aarch64)` like
        // every other command, then lays the disk that is its own payload's -
        // which is the half the shared build cannot know.
        Commands::BootAarch64 => aarch64::run(build_dir, profile),
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
    arch: Arch,
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
        // Carried across the boundary for the same reason `--build-dir` is:
        // the privileged half re-parses `--arch` from scratch, so omitting it
        // would hand the child the *default* machine however this one was
        // invoked - a switch that survives everything but sudo is not a switch.
        .arg("--arch")
        .arg(arch.name())
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
    arch: Arch,
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
        .arg("--arch")
        .arg(arch.name())
        .arg(command.name());
    if let Some(mode) = mode {
        sudo.arg("--mode").arg(mode.name());
    }

    let err = sudo.exec();
    anyhow::bail!("Failed to re-exec under sudo: {err}");
}

#[cfg(test)]
mod tests {
    use super::Arch;

    /// The counterfactual for the silent wrong-run: `--arch aarch64` used to
    /// reach `selftest::run_tests` unchanged and print an x86_64 verdict under
    /// an aarch64 banner - and to leave `build` compiling the x86_64 kernel
    /// whatever the switch said, because the string never reached cargo. Each
    /// machine now gets its own arm, a machine this front end does not build is
    /// refused by name, and the target the compiler is handed comes from the
    /// same value.
    #[test]
    fn a_machine_this_front_end_does_not_build_is_refused_by_name() {
        assert_eq!(Arch::parse("x86_64").unwrap(), Arch::X86_64);
        assert_eq!(Arch::parse("aarch64").unwrap(), Arch::Aarch64);
        let refusal = Arch::parse("riscv64")
            .expect_err("a machine this front end does not build must be refused")
            .to_string();
        assert!(
            refusal.contains("riscv64") && refusal.contains("x86_64"),
            "the refusal must name the machine asked for and the ones that are \
             built: {refusal}"
        );
    }
}
