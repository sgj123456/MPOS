//! The `test` command: the kernel's self-test sweep.
//!
//! Three stages, all of which have to pass for the command to succeed:
//!
//! 1. The host unit tests - the arch-independent core of the kernel (`cargo
//!    test --lib`) and xtask's own.
//! 2. The user test programs. The ABI conformance suite (`user/abitest`, split
//!    into `src/bin/syscall/` and `src/bin/integration/`) and the std-side tests
//!    (`user/tests`) are built, packed into an initramfs, and booted in QEMU
//!    without a disk: the test `/init` runs every `/bin/t-*` it finds, prints a
//!    `=== PASS`/`=== FAIL` verdict per program, and ends the machine with
//!    `poweroff -f`, which the kernel turns into QEMU's debug exit. The serial
//!    console is captured to a file and parsed here.
//! 3. Nothing from stage 2 lands in the image `cargo xtask run` boots: the
//!    test initramfs is a separate archive under the build directory, the boot
//!    disk is a virtual FAT QEMU synthesizes from a directory (`fat:`), and no
//!    root privileges are needed anywhere.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::arch::Arch;
use crate::qemu;
use crate::sweep;
use crate::utils;

/// The SATA disk the test machine is given, and how big it is. Named here and
/// used by `qemu`'s `Boot::Esp` arm through the same build directory, so the
/// file that is made and the file that is attached are one name.
pub const SATA_TEST_IMAGE: &str = "test-sata.img";
pub const SATA_TEST_BYTES: u64 = 4 * 1024 * 1024;

/// How long the guest may run before the harness gives up on it. TCG is slow;
/// KVM is not. `MPOS_TEST_TIMEOUT` overrides, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

pub fn run_tests(build_dir: &Path, profile: &str, selection: sweep::Selection) -> Result<()> {
    check_target_gates()?;
    run_unit_tests()?;
    println!();

    // The x86_64 sweep: its own initramfs on the QEMU wiring in `machine.toml`.
    // Named rather than left to the workspace default, so the machine this
    // sweep boots and the machine it builds are the same value.
    utils::build_kernel(profile, Arch::X86_64)?;
    println!();

    let built = build_user_tests()?;
    println!("user tests: {} programs built", built.len());

    // Which of them this run boots is the configuration's answer; the group
    // refusal, the selection and the two lines under it are `sweep`'s, because
    // they are questions about `tests.toml` rather than about this machine.
    let config = sweep::load_config()?;
    // The declared-name check, and *this* arm's scope: it builds both suites
    // (169 programs, the bare-metal conformance suite among them), so a name in
    // the file that no program answers to is a stale entry. The second machine
    // builds only the std suite and asks the narrower question; `sweep::select`
    // records why the two cannot share one scope.
    let names: Vec<String> = built.iter().map(|test| test.name.clone()).collect();
    let unmatched = config.unmatched(&names);
    if !unmatched.is_empty() {
        bail!(
            "tests.toml lists {}, which no built test program answers to",
            unmatched.join(", ")
        );
    }
    let plan = sweep::select(crate::config::Machine::X86_64, &config, &built, &selection)?;

    // What is left is this arm's own: the throwaway initramfs, the QEMU wiring
    // in `machine.toml`, and a verdict that can quote an exit status.
    let tar = build_test_initramfs(build_dir, &plan.staged)?;
    let (status, log) = boot_tests(build_dir, profile, &tar)?;

    report(&plan.staged, plan.built, &log, status)
}

/// The structural check that belongs ahead of the compiler: a `#[cfg(test)]`
/// module whose file only a `target_os = "none"` build reaches has never run,
/// and nothing else in this pipeline would ever say so (`PLAN.md` §11.2 Step F,
/// the V-11 class). Cheap enough to run on every sweep - it reads `src/*.rs`
/// and matches attributes - and loud when it fires, because the alternative is
/// a test count that looks like coverage.
fn check_target_gates() -> Result<()> {
    println!("Target gates: every test module is one the host compiles");
    let dead = crate::gates::dead_host_tests(&utils::project_root()?)?;
    if dead.is_empty() {
        return Ok(());
    }
    for test in &dead {
        eprintln!(
            "  {}:{} is inside `{}`, which no host build compiles",
            test.file.display(),
            test.line,
            test.gated_by
        );
    }
    bail!(
        "{} test module(s) can never run; host-compile the file or move the pins \
         to a program that boots (PLAN.md §11.2 Step F)",
        dead.len()
    );
}

/// The arch-independent kernel, tested on the host. `--target` is explicit
/// because the root `.cargo/config.toml` defaults every cargo command to the
/// bare-metal target, and the host tests are the opposite of that.
/// Every leaf crate under `crates/`, by package name.
///
/// The host unit tests are run one `-p` at a time because the root
/// `.cargo/config.toml` defaults every cargo command to a bare-metal target, and
/// `--workspace` would try to build the kernel for the host too. That makes this
/// list a place a test can disappear into: a leaf crate added without a line
/// here is compiled, but its tests never run, and the gate keeps reporting a
/// green number that silently means less than it did. `mpos-fdt` was in exactly
/// that state - eight tests, never run. [`tests::every_leaf_crate_is_tested`]
/// pins the list against the directory, so this is the last time.
const LEAF_CRATES: &[&str] = &[
    "mpos-err",
    "mpos-bootinfo",
    "mpos-vm",
    "mpos-abi",
    "mpos-date",
    "mpos-binfmt",
    "mpos-cred",
    "mpos-uts",
    "mpos-seccomp",
    "mpos-blk",
    "mpos-pci",
    "mpos-term",
    "mpos-ahci",
    "mpos-gpu",
    "mpos-evdev",
    "mpos-uinput",
    "mpos-fdt",
];

fn run_unit_tests() -> Result<()> {
    let root = utils::project_root()?;

    println!("Unit tests: the kernel and its leaf crates (host)");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "--lib",
        "--target",
        "x86_64-unknown-linux-gnu",
        "-p",
        "mpos",
    ]);
    for crate_name in LEAF_CRATES {
        cmd.arg("-p").arg(crate_name);
    }
    cmd.current_dir(&root);
    utils::run_status(&mut cmd, "the kernel's host unit tests")?;

    println!("Unit tests: xtask");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "--target",
        "x86_64-unknown-linux-gnu",
        "-p",
        "mpos-xtask",
    ])
    .current_dir(&root);
    utils::run_status(&mut cmd, "xtask's unit tests")?;

    Ok(())
}

/// One user test program: its name (what appears on a verdict line) and the
/// built binary.
#[derive(Clone)]
struct UserTest {
    name: String,
    binary: PathBuf,
}

/// This machine stages a *path*: the binary is copied into the test initramfs
/// from where cargo left it. The name is the only part `sweep` needs, which is
/// what lets the choice of programs be one implementation.
impl sweep::TestProgram for UserTest {
    fn name(&self) -> &str {
        &self.name
    }
}

/// Builds both user test crates and returns what they produced.
///
/// The std-side crate builds like any musl program. The ABI suite must not
/// inherit the root `.cargo/config.toml`'s `x86_64-unknown-none` flags - they
/// point at the kernel's linker script - so it is built with `RUSTFLAGS`,
/// which replaces the configuration's flags for this invocation rather than
/// merging with them.
fn build_user_tests() -> Result<Vec<UserTest>> {
    let root = utils::project_root()?;
    let mut tests = Vec::new();

    println!("User tests: the std-side suite (mpos-user-tests)");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--release",
        "-p",
        "mpos-user-tests",
        "--target",
        "x86_64-unknown-linux-musl",
    ])
    .current_dir(&root);
    utils::run_status(&mut cmd, "the std-side user tests")?;

    println!("User tests: the ABI conformance suite (mpos-user-abi)");
    let mut cmd = Command::new("cargo");
    cmd.env("RUSTFLAGS", "-C relocation-model=static");
    cmd.args([
        "build",
        "--release",
        "-p",
        "mpos-user-abi",
        "--target",
        "x86_64-unknown-none",
    ])
    .current_dir(root.join("user/abitest"));
    utils::run_status(&mut cmd, "the ABI conformance suite")?;

    let sources = [
        (
            root.join("user/abitest/src/bin"),
            target_dir()?.join("x86_64-unknown-none/release"),
        ),
        (
            root.join("user/tests/src/bin"),
            target_dir()?.join("x86_64-unknown-linux-musl/release"),
        ),
    ];
    for (src_dir, out_dir) in sources {
        collect_user_tests(&src_dir, &out_dir, &mut tests)?;
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tests)
}

/// Find every test program under `src_dir`, recursing into subdirectories.
///
/// The ABI suite is split by what a test needs - `syscall/` for a self-contained
/// syscall family, `integration/` for a device, the network, or a subsystem run
/// end to end - so its programs no longer sit directly in `src/bin`; the std-side
/// suite is still flat, which the same walk covers. The binary keeps the file's
/// name, which is what a `/bin/t-*` run and the verdict line are keyed on.
fn collect_user_tests(src_dir: &Path, out_dir: &Path, tests: &mut Vec<UserTest>) -> Result<()> {
    for entry in
        fs::read_dir(src_dir).with_context(|| format!("Failed to read {}", src_dir.display()))?
    {
        let source = entry?.path();
        if source.is_dir() {
            collect_user_tests(&source, out_dir, tests)?;
            continue;
        }
        if source.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = source.file_stem().unwrap().to_string_lossy().to_string();
        let binary = out_dir.join(&name);
        if !binary.exists() {
            bail!("The build produced no binary for {name}");
        }
        tests.push(UserTest { name, binary });
    }
    Ok(())
}

fn target_dir() -> Result<PathBuf> {
    utils::target_dir()
}

/// Packs the test initramfs: the ordinary one (busybox and the Alpine
/// baselayout the tests read files from), with every test binary in `/bin` and
/// a `/init` that runs them instead of pivoting into a root filesystem.
///
/// The ordinary initramfs is built through the `initramfs` command - as a
/// subprocess of this executable, because that command knows how to take root
/// for its chroot step and hand the tree back; from here the archive is
/// unpacked as an ordinary user.
fn build_test_initramfs(build_dir: &Path, tests: &[&UserTest]) -> Result<PathBuf> {
    println!("User tests: packing the test initramfs");

    let self_exe = std::env::current_exe().context("Failed to find the xtask executable")?;
    // The base initramfs only needs the privileged `initramfs` command when it
    // is actually out of date - that command takes root for its `chroot` step,
    // and a fresh archive needs no such thing. Building it unconditionally
    // would demand a password even when there is nothing to rebuild.
    if crate::initramfs::needs_rebuild(build_dir, Arch::X86_64)? {
        let status = Command::new(&self_exe)
            .arg("initramfs")
            .status()
            .context("Failed to run the initramfs command")?;
        if !status.success() {
            bail!("The initramfs did not build");
        }
    } else {
        println!("User tests: the base initramfs is up to date; not rebuilding");
    }

    let staging = build_dir.join("test-initramfs.d");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    let unpacked = Command::new("tar")
        .arg("-xf")
        .arg(crate::initramfs::archive_path(build_dir, Arch::X86_64))
        .arg("-C")
        .arg(&staging)
        .arg("--no-same-owner")
        .status()
        .context("Failed to unpack the initramfs")?;
    if !unpacked.success() {
        bail!("The initramfs did not unpack");
    }

    let bin = staging.join("bin");
    fs::create_dir_all(&bin)?;
    for test in tests {
        let dst = bin.join(&test.name);
        fs::copy(&test.binary, &dst).with_context(|| format!("Failed to stage {}", test.name))?;
        utils::set_mode(&dst, 0o755)?;
    }

    fs::write(staging.join("init"), TEST_INIT)?;
    utils::set_mode(&staging.join("init"), 0o755)?;

    let tar = build_dir.join("test-initramfs.tar");
    let packed = Command::new("tar")
        .args([
            "--format=ustar",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
        ])
        .arg(&tar)
        .arg("-C")
        .arg(&staging)
        .arg(".")
        .status()
        .context("Failed to pack the test initramfs")?;
    if !packed.success() {
        bail!("The test initramfs did not pack");
    }
    fs::remove_dir_all(&staging)?;

    let size = fs::metadata(&tar)?.len();
    println!(
        "test initramfs: {} ({:.1} MB)",
        tar.display(),
        size as f64 / 1048576.0
    );
    Ok(tar)
}

/// ... of the test initramfs. busybox is driven by absolute path
/// because the archive's applet links are the only ones it has; every program
/// matching `/bin/t-*` is a test, and `poweroff -f` is what ends the run -
/// the kernel answers `reboot(RB_POWER_OFF)` with QEMU's debug exit.
///
/// Which programs are in `/bin` at all *is* the test selection: the guest has
/// no opinion about which tests it runs, it runs what it was given, so
/// `tests.toml` decides the sweep simply by deciding what is staged here.
///
/// Both machines run this one script. The second machine stages the same
/// programs onto its *disk* rather than into the archive (its boot pivots onto
/// a root filesystem, so that is where a payload has to sit), but the runner,
/// the protocol and the selection are one implementation - which is why this
/// constant is readable from `disk` rather than private to this file.
pub(crate) const TEST_INIT: &str = r#"#!/bin/sh
# mpos self-test: run every test program in /bin, then end the machine.

/bin/busybox mount -t proc     proc     /proc
/bin/busybox mount -t sysfs    sysfs    /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mount -t tmpfs    tmpfs    /tmp

# /dev/pts is a filesystem of its own, not a directory devtmpfs hands out:
# an unmounted one leaves /dev/pts/<n> absent, so a pty test opens its slave
# as EBADF and then blocks for ever on a master nobody writes to. Linux's
# init mounts it here, and so does this one.
/bin/busybox mount -t devpts devpts /dev/pts

# The socket tests talk to QEMU's user-network stack, which only answers at
# its fixed addresses once the interface is up and carries the address slirp
# expects of the guest. The kernel brings eth0 up with that address and route
# already; the commands run for the boot that does not have them, and
# `|| true` keeps one that does from failing on the "File exists" the
# netlink write side now honestly answers.
/bin/busybox ip link set eth0 up || true
/bin/busybox ip addr add 10.0.2.15/24 dev eth0 || true
/bin/busybox ip route add default via 10.0.2.2 || true

# musl's resolver asks 127.0.0.1 when /etc/resolv.conf is absent - a socket
# nothing in this machine answers, so every lookup ends `Try again` and
# t-stdresolve fails 0/20. The address QEMU's user-mode network *does*
# answer is 10.0.2.3, which forwards to the host's own resolver; the test
# boot has no rootfs to carry the file, so it is written here.
echo "nameserver 10.0.2.3" > /etc/resolv.conf

pass=0
fail=0
# t-stdargs is the one test that pins the argument vector itself: it wants
# argv to be exactly `t-stdargs one two three`, and the bare word is
# load-bearing - a path there ("/bin/t-stdargs") is a different argv[0]
# than the PATH lookup a shell hands it. Everything else runs as-is.
run_one() {
    if [ "${1##*/}" = "t-stdargs" ]; then
        (cd /bin && PATH=/bin t-stdargs one two three)
    else
        "$1"
    fi
}
for t in /bin/t-*; do
    echo "=== RUN ${t##*/}"
    if run_one "$t"; then
        echo "=== PASS ${t##*/}"
        pass=$((pass + 1))
    else
        echo "=== FAIL ${t##*/} exit=$?"
        fail=$((fail + 1))
    fi
done
echo "=== SUMMARY pass=$pass fail=$fail"
echo "=== END"
/bin/busybox poweroff -f
"#;

/// Boots the test initramfs in QEMU and returns the exit status with the path
/// of the captured serial log. The boot disk is QEMU's virtual FAT: the
/// loader, kernel and test initramfs are served out of a plain directory, so
/// no image needs creating and no privilege is needed to write it.
fn boot_tests(build_dir: &Path, profile: &str, tar: &Path) -> Result<(Option<i32>, String)> {
    println!("User tests: booting the test machine");

    let esp = build_dir.join("test-esp");
    if esp.exists() {
        fs::remove_dir_all(&esp)?;
    }
    fs::create_dir_all(esp.join("EFI/BOOT"))?;
    fs::copy(
        utils::loader_path(profile, Arch::X86_64)?,
        esp.join("EFI/BOOT/BOOTX64.EFI"),
    )?;
    fs::copy(
        utils::kernel_path(profile, Arch::X86_64)?,
        esp.join("mpos.elf"),
    )?;
    fs::copy(tar, esp.join("initramfs.tar"))?;
    // Two words, both load-bearing. `console=ttyS0` makes the serial wire the
    // test boot's only console, so every line the harness reads is the whole of
    // what the machine said - there is no second console to be missing it on.
    // `mpos.selftest` is the switch the kernel reads to run its milestone checks
    // (the half-second clock wait and the scheduler sleeps); a normal boot leaves
    // it off and goes straight to init, and the test boot wants every check run.
    fs::write(esp.join("mpos.cmdline"), "console=ttyS0 mpos.selftest\n")?;

    // A SATA disk beside the ESP, so the test machine has *two* disks from two
    // drivers: the ESP reaches the guest through virtio-blk, and this one
    // through the AHCI controller the `Boot::Esp` arm attaches. It is what
    // `t-ahci` reads, writes and reads back, which is the only way a driver
    // whose every line is a volatile register access gets pinned at all
    // (`PLAN.md` §7.3). Sparse and small: the test writes one sector and reads
    // it back, and nothing else in the suite touches it.
    let sata = build_dir.join(SATA_TEST_IMAGE);
    if sata.metadata().map(|m| m.len()).unwrap_or(0) != SATA_TEST_BYTES {
        let file = fs::File::create(&sata)?;
        file.set_len(SATA_TEST_BYTES)?;
    }

    let (firmware, vars) = qemu::write_firmware_vars(build_dir, Arch::X86_64)?;

    let log = build_dir.join("test-serial.log");
    let esp = esp.canonicalize()?;
    let log_path = log.display().to_string();
    // The same command line `run` boots with, from `qemu.*` in machine.toml -
    // except that the disk is this run's ESP and the serial line goes to a
    // file, because a test reads its verdict back out of it. The share and
    // the console socket live under the build directory the same way, made
    // here so the devices that want them never start without.
    // The share and console socket get their own directory, not `run`'s:
    // `machine_args` binds `console.sock` inside this dir and exports it over
    // 9p, and a second QEMU (a live `cargo xtask run`) already holds those -
    // the bind would fail before the guest boots.
    // The share carries the 9p probe file too, so a test can mount it and read
    // the bytes back - the pin for the first machine's share, which used to be
    // exported and never reachable (`PLAN.md` §13.29).
    let share = utils::lay_p9_share(&build_dir.join("test-9p-share"))?;
    let mut config = crate::config::QemuConfig::load_default(Arch::X86_64)?;
    // A test boot never needs the host vsock bus, and claiming `guest-cid=3`
    // collides with any VM already running on this host (a concurrent
    // `cargo xtask run`, say) - which would abort QEMU before the guest boots
    // and report every test as failed. Leave it off here; `run` keeps it.
    config.vsock = false;
    let args = qemu::machine_args(
        &config,
        &firmware.code,
        &vars,
        qemu::Boot::Esp(esp.clone()),
        &qemu::Console::File(log_path.clone()),
        &share,
    );
    // `isa-debug-exit` - the port the kernel writes its verdict to - comes with
    // every machine `machine_args` builds.

    eprintln!("+ qemu-system-x86_64 {}", args.join(" "));
    let mut command = Command::new("qemu-system-x86_64");
    command.args(&args);

    // The loop is `qemu::capture`'s - one loop for both machines. What this call
    // does not pass is terminators: this machine's boot ends when its kernel
    // writes the debug exit, so the process leaving *is* the end of the run, and
    // there is no marker to stop on earlier.
    let timeout = Duration::from_secs(test_timeout_secs());
    let captured = qemu::capture(command, &log, &[], timeout)?;
    if captured.timed_out {
        bail!(
            "The test machine did not finish within {}s; its serial output is at {}",
            timeout.as_secs(),
            log.display()
        );
    }
    if captured.status != Some(33) {
        eprintln!(
            "test: note: QEMU exited with {} (33 is the clean debug exit)",
            captured.status.unwrap_or(-1)
        );
    }
    Ok((captured.status, captured.log))
}

fn test_timeout_secs() -> u64 {
    std::env::var("MPOS_TEST_TIMEOUT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Reads the run's verdicts off the serial log and answers for every test that
/// was **staged**: a test that never printed `=== PASS` failed, whether it
/// crashed, hung or never ran. `built` is what the crate produced, so the line
/// can say how much of it the configuration left out - a sweep that quietly
/// runs less than it used to is the failure this number is here to catch.
fn report(tests: &[&UserTest], built: usize, text: &str, status: Option<i32>) -> Result<()> {
    // The protocol is `sweep`'s - one parser for both runners - and what this
    // arm does with it is its own, because its claim is: every staged program
    // printed `=== PASS`, and the machine ended on the debug exit.
    let verdict = sweep::verdict(text);
    let missing: Vec<&str> = tests
        .iter()
        .filter(|test| !verdict.passed.contains(&test.name.as_str()))
        .map(|test| test.name.as_str())
        .collect();
    let summary = verdict.summary;
    let clean = status == Some(33) && summary.is_some();

    println!();
    if let Some(summary) = summary {
        println!("self-test guest: {summary} ({} built)", tests.len());
    } else {
        println!("self-test guest: no summary line - the run did not get to the end");
    }
    let skipped = built.saturating_sub(tests.len());
    if skipped > 0 {
        println!("self-test guest: {skipped} of {built} user tests left out by tests.toml");
    }
    println!(
        "self-test guest: exit {} (33 is the clean debug exit)",
        status.unwrap_or(-1)
    );

    if !missing.is_empty() {
        println!();
        println!("self-test: {} test(s) did not pass:", missing.len());
        for name in &missing {
            println!("  FAIL {name}");
        }
    }

    if !clean {
        println!();
        println!("self-test: the last of the serial log:");
        let tail: Vec<&str> = text.lines().collect();
        for line in tail.iter().rev().take(30).rev() {
            println!("  | {line}");
        }
    }

    println!();
    if clean && missing.is_empty() {
        println!(
            "self-test: all {} user tests passed, unit tests passed{}",
            tests.len(),
            if skipped > 0 {
                format!(" ({skipped} not run - see tests.toml)")
            } else {
                String::new()
            }
        );
        Ok(())
    } else {
        bail!(
            "self-test failed: {} of {} user tests did not pass{}",
            missing.len(),
            tests.len(),
            if clean {
                ""
            } else {
                " and the run did not end cleanly"
            }
        )
    }
}

// ---------------------------------------------------------------------------
// The sweep whose payload rides on the disk.
//
// The first machine's boot runs its payload out of the boot archive (there is
// no disk to pivot onto), so its programs are packed into the initramfs above.
// A machine that switches root - which is what `steps.rs`'s shared five steps
// do - has to have them on the tree it switches *to*: the runner is `/init` on
// the disk root, and the programs sit beside it under the same `/bin/t-*` names
// the shared runner script walks. Everything else - the selection, the
// protocol, the report - is the implementation above.
// ---------------------------------------------------------------------------

/// The image size a disk sweep asks for. The payload is every std-side program
/// built for the machine (~5.6 MB), so this is its number rather than one every
/// boot pays for.
const SWEEP_DISK_BYTES: u64 = 64 * 1024 * 1024;

/// Run the sweep for a machine whose boot pivots onto a disk root.
///
/// `arch` is the machine, and it is the only thing this needs to know: the musl
/// target its programs are compiled for, the node its console is called, and the
/// boot media `crate::disk` lays for it. The verdict is the shared
/// `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol, read by [`crate::sweep`].
///
/// `=== END` is what ends the poll: unlike the first machine's sweep there is no
/// debug exit to wait for - the runner ends the machine itself, so the harness's
/// own last line is the terminator.
pub fn run_disk_sweep(build_dir: &Path, profile: &str, arch: Arch) -> Result<()> {
    // The kernel this sweep boots has to be the current one, and it is this
    // arm's own build: the first machine's is done by `run_tests`, and `main`
    // does not build for `test` on either machine.
    utils::build_kernel(profile, arch)?;
    println!();
    let (kernel, loader) = (
        utils::kernel_path(profile, arch)?,
        utils::loader_path(profile, arch)?,
    );
    let built = build_std_tests(arch)?;
    println!("user tests: {} {} programs built", arch.name(), built.len());

    // The check below is what keeps the naming honest, and it is this arm's own
    // rather than `sweep`'s: it is about *this machine's* group specifically, and
    // what it refuses is a gap declared here that this payload does not build. A
    // gap has to be a program this machine was asked about and could not serve; a
    // name in its group that this payload never builds is a claim about the ABI
    // that nothing in the sweep can check - the other arm stages the name (it
    // builds every program), and this one has nothing to leave out - so it would
    // sit in the file looking like a measured gap for ever.
    let config = sweep::load_config()?;
    let machine = crate::config::Machine::Aarch64;
    let built_names: Vec<String> = built.iter().map(|(name, _)| name.clone()).collect();
    let unbuilt = gaps_without_programs(&config, machine, &built_names);
    if !unbuilt.is_empty() {
        bail!(
            "tests.toml names {}, as gaps for this machine, which its payload \
             does not build: a gap nobody can check is not a gap",
            unbuilt.join(", ")
        );
    }

    // The selection is `sweep`'s - the same questions about `tests.toml` the
    // other arm asks - and the flags are empty because `main` refuses
    // `--groups` on this arm before it gets here (this payload's group
    // vocabulary is the first machine's suites' membership).
    let plan = sweep::select(machine, &config, &built, &sweep::Selection::default())?;
    let tests = &plan.staged;

    // The runner is the *shared* one: the same `#!/bin/sh` script the first
    // machine's test boot is handed, because what a sweep is does not change
    // with the machine - a program is exec'd, its exit status becomes a
    // `=== PASS`/`=== FAIL` line, and the run ends with `poweroff -f`. What is
    // this machine's own is only *where the payload sits*.
    //
    // No in-tree applet farm: the disk's root is the archive the shared
    // `initramfs` pipeline builds, so `/bin/busybox`, the shell and the
    // directories a mount lands on are the ones that pipeline installs for this
    // machine - the same ones the *ramfs* half runs - instead of a second,
    // hand-written multi-call binary maintained beside them.
    let mut entries: Vec<(&str, &[u8])> = vec![("init", TEST_INIT.as_bytes())];
    let paths: Vec<String> = plan
        .staged
        .iter()
        .map(|(name, _)| format!("bin/{name}"))
        .collect();
    for (path, (_, bytes)) in paths.iter().zip(plan.staged.iter()) {
        entries.push((path.as_str(), bytes.as_slice()));
    }
    // The root the kernel is told to switch to, and the consoles it is told it
    // has: both come from the machine's own configuration (`rootfs.root_device`,
    // `rootfs.serial_port`), because the disk the sweep boots is the same GPT
    // shape the install writes and its partition carries the same name.
    let rootfs =
        crate::config::RootfsConfig::load(&utils::project_root()?.join("rootfs.toml"), arch)?;
    let cmdline = format!(
        "console={} mpos.selftest root={} rootfstype={}\n",
        rootfs.serial_port, rootfs.root_device, rootfs.filesystem
    );
    let disk = crate::disk::sweep_medium(
        build_dir,
        arch,
        &crate::disk::Boot {
            kernel: &kernel,
            loader: &loader,
            entries: &entries,
            disk_bytes: SWEEP_DISK_BYTES,
            cmdline: &cmdline,
        },
    )?;
    println!("disk:   {}", disk.display());

    // How long the guest may run before the harness gives up on it: the shared
    // `MPOS_TEST_TIMEOUT` answer, because a sweep that is slower on this machine
    // than on the first one is a fact about TCG and not about the deadline.
    let log = qemu::boot(
        build_dir,
        arch,
        &disk,
        &["=== END"],
        Duration::from_secs(test_timeout_secs()),
    )?;
    print!("{log}");
    let staged: Vec<&str> = tests.iter().map(|(name, _)| name.as_str()).collect();
    report_disk_sweep(build_dir, arch, &staged, plan.skipped.len(), &log)
}

/// The names in `machine`'s own `tests.toml` groups that `built` does not carry.
///
/// Empty is the healthy answer, and the caller refuses the run when it is not:
/// see the comment at its call site for why a gap has to be a program.
fn gaps_without_programs(
    config: &crate::config::TestConfig,
    machine: crate::config::Machine,
    built: &[String],
) -> Vec<String> {
    missing_programs(&config.machine_tests(machine), built)
}

/// The gap names among `gaps` that `built` does not carry.
///
/// Split from the `TestConfig` lookup so the rule can be tested without a file:
/// the shipped file's own group is empty today, and a rule that can only be
/// exercised by a non-empty file would have no test on the day the file is right.
fn missing_programs(gaps: &[(&str, &str)], built: &[String]) -> Vec<String> {
    gaps.iter()
        .filter(|(_, test)| !built.iter().any(|name| name == test))
        .map(|(_, test)| test.to_string())
        .collect()
}

/// The std-side user test programs for `arch`, built by cargo for its musl
/// target and read back as bytes.
///
/// The files cross as *data* rather than as paths: this arm's payload is a disk
/// laid out by name, not a tar the guest unpacks.
///
/// Zero programs is an error, not an empty green run - a sweep that quietly runs
/// fewer tests than asked is the one failure this must not have.
fn build_std_tests(arch: Arch) -> Result<Vec<(String, Vec<u8>)>> {
    let root = utils::project_root()?;
    println!(
        "User tests: the std-side suite for {} (mpos-user-tests)",
        arch.name()
    );
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "mpos-user-tests",
            "--target",
            arch.musl_target(),
        ])
        .current_dir(&root)
        .status()
        .context("Failed to run cargo for the user tests")?;
    if !status.success() {
        bail!("the {} std-side user tests did not build", arch.name());
    }

    let dir = utils::target_dir()?.join(format!("{}/release", arch.musl_target()));
    let mut tests = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("Failed to read {}", dir.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.starts_with("t-") || !path.is_file() {
            continue;
        }
        let bytes =
            fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        // Cargo writes a `.d` dep-info file beside every binary, and its name
        // starts with `t-` too. Staging one makes the runner exec a text file
        // and report `exit=127`, which reads like a broken program rather than a
        // broken payload - so the payload is the ELF magic, not the name.
        if !bytes.starts_with(b"\x7fELF") {
            continue;
        }
        tests.push((name, bytes));
    }
    tests.sort_by(|a, b| a.0.cmp(&b.0));
    if tests.is_empty() {
        bail!(
            "no {} test program was built under {} - the sweep would have \
             reported a green zero",
            arch.name(),
            dir.display()
        );
    }
    Ok(tests)
}

/// This arm stages *bytes*: the binary is read here and laid on the ext4 image
/// by name, because the payload is a disk the guest does not unpack. The name is
/// the only part `sweep` needs.
impl sweep::TestProgram for (String, Vec<u8>) {
    fn name(&self) -> &str {
        &self.0
    }
}

/// Read a disk sweep's verdict out of the log.
///
/// Three facts have to agree, and each is a different way for the run to be
/// less than it looks: every program that was staged has to appear (a log that
/// stops early still contains the passes it got through), no program may have
/// failed, and the runner's own summary line has to be there - the same shape
/// [`report`] checks, because both are claims about a machine someone has to be
/// able to quote.
///
/// `staged` is the programs this run asked the machine about and `left_out` how
/// many `tests.toml` named instead - the latter counted, never hidden: a green
/// here means "every program this machine was supposed to be able to run
/// passed", and the names and reasons of the ones it was not are in that file,
/// printed by the caller above.
fn report_disk_sweep(
    build_dir: &Path,
    arch: Arch,
    staged: &[&str],
    left_out: usize,
    log: &str,
) -> Result<()> {
    // The protocol is `sweep`'s - one parser for both runners. What this arm
    // does with it is its own, because its claim differs from the first
    // machine's: there is no debug exit to read here, so instead of quoting a
    // status it asserts an exact count - every staged program must have left a
    // verdict, and "some of them passed" is not a partial pass.
    let verdict = sweep::verdict(log);
    // The runner sets the machine up before it runs anything (it makes `/tmp`),
    // and a setup that failed makes every "failure" below a statement about the
    // harness rather than the kernel. So this is checked first and refuses the
    // run outright - the one line here that is not about the programs.
    if let Some(line) = verdict.setup_failed {
        bail!(
            "the {} sweep's runner could not set the machine up: {line}; \
             nothing the programs did is evidence about the kernel until that \
             works",
            arch.name()
        );
    }

    println!();
    let Some(summary) = verdict.summary else {
        bail!(
            "the {} sweep never printed a summary: the run did not reach the \
             end of the {} staged program(s); {} says how far it got",
            arch.name(),
            staged.len(),
            qemu::serial_log(build_dir, arch).display()
        );
    };
    println!("self-test ({}): {summary}", arch.name());

    if !verdict.failed.is_empty() {
        let mut names: Vec<&str> = verdict
            .failed
            .iter()
            .map(|name| name.split_whitespace().next().unwrap_or(name))
            .collect();
        names.sort_unstable();
        for name in &names {
            println!("  FAILED {name}");
        }
        bail!(
            "self-test ({}) failed: {} of {} programs did not pass: {}",
            arch.name(),
            names.len(),
            staged.len(),
            names.join(", ")
        );
    }
    if verdict.passed.len() != staged.len() {
        bail!(
            "the {} sweep staged {} program(s) but the log carries {} verdict(s): \
             the payload and the run disagree, which is not a partial pass",
            arch.name(),
            staged.len(),
            verdict.passed.len()
        );
    }
    println!(
        "self-test ({}): all {} staged user tests passed ({} left out by \
         tests.toml - the groups above name every one of them, with the reason \
         in that file)",
        arch.name(),
        staged.len(),
        left_out
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::LEAF_CRATES;

    /// A gap whose program the payload does not build is a claim nothing in the
    /// sweep can check: there is no program to leave out, so the entry would sit
    /// in the file looking measured for ever. The rule is exercised on a list
    /// that *has* gaps, because the shipped file's own list is empty - the state
    /// the harness cut left it in - and a test must not need the file to be
    /// wrong to run.
    #[test]
    fn a_named_gap_has_to_be_a_program_the_payload_builds() {
        let gaps = [("aarch64", "t-gone"), ("aarch64", "t-here")];
        let built = ["t-here".to_string(), "t-openat".to_string()];
        assert_eq!(
            super::missing_programs(&gaps, &built),
            vec!["t-gone".to_string()]
        );
        assert!(
            super::missing_programs(&gaps, &["t-here".to_string(), "t-gone".to_string()])
                .is_empty()
        );

        // And the repository's own file: the group exists and every gap it names
        // is a program the payload carries. The payload is what `cargo` builds,
        // which a unit test does not run; what it can check is that the arm's
        // validation is consistent with the file it reads, and empty - the
        // healthy answer - is a state this asserts as a property, not a count.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let config = crate::config::TestConfig::load(&root.join("tests.toml")).unwrap();
        let machine = crate::config::Machine::Aarch64;
        assert!(
            config.group_names(machine).contains(&"aarch64"),
            "the second machine's gap group is gone rather than empty"
        );
        let gaps: Vec<String> = config
            .machine_tests(machine)
            .into_iter()
            .map(|(_, test)| test.to_string())
            .collect();
        assert!(super::gaps_without_programs(&config, machine, &gaps).is_empty());
    }

    /// The aarch64 user programs are built by cargo, so the two facts the loader
    /// insists on have to be in the root `.cargo/config.toml`
    /// for that target, and neither shows up as a compile error when it is
    /// missing - a static flag dropped links a program the loader refuses, and a
    /// linker dropped fails at the link step with an option the host `ld` does not
    /// know. The x86_64 block carries the same shape for the same reason; this
    /// pins the aarch64 one until the sweep that builds these programs is the
    /// thing that would notice (`PLAN.md` §13.5).
    #[test]
    fn the_aarch64_user_programs_are_declared_static_and_linked_by_rust_lld() {
        const HEADER: &str = "[target.aarch64-unknown-linux-musl]";
        let config = std::fs::read_to_string(
            crate::utils::project_root()
                .unwrap()
                .join(".cargo/config.toml"),
        )
        .expect("the root .cargo/config.toml must be readable");
        // The target's own table: from its header to the next table. Taking the
        // slice rather than searching the whole file is what keeps the x86_64
        // block's identical flags from answering for the aarch64 one.
        let start = config
            .find(HEADER)
            .expect("the aarch64 musl target must be declared")
            + HEADER.len();
        let rest = &config[start..];
        let block = rest.split("\n[").next().unwrap();

        assert!(
            block.contains("+crt-static"),
            "the aarch64 user programs must be static: the loader has no \
             PT_INTERP support.\n{block}"
        );
        assert!(
            block.contains("linker=rust-lld"),
            "the aarch64 link must name rust-lld: the host `ld` rejects the \
             `--fix-cortex-a53-843419` rustc passes for this target.\n{block}"
        );
    }

    /// A leaf crate whose tests nothing runs.
    ///
    /// The counterfactual is not hypothetical: `mpos-fdt` sat with eight tests
    /// that no gate had ever executed, because `run_unit_tests` names every
    /// crate by hand and a new one is a line nobody remembers to add. The
    /// failure mode is the one this front end is built to refuse - a run that
    /// reports a green number while sweeping less than it says - so the list is
    /// pinned against the directory it is supposed to mirror.
    #[test]
    fn every_leaf_crate_is_tested() {
        // `CARGO_MANIFEST_DIR` is this crate's directory at compile time, so the
        // walk needs no working-directory assumption the way `utils` does.
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent directory")
            .join("crates");

        let mut on_disk: Vec<String> = fs::read_dir(&crates)
            .expect("the crates directory must be readable")
            .map(|entry| entry.expect("a crates entry must be readable").path())
            .map(|dir| fs::read_to_string(dir.join("Cargo.toml")).expect("a leaf Cargo.toml"))
            .map(|toml| {
                toml.lines()
                    .find_map(|line| line.strip_prefix("name = "))
                    .expect("a leaf Cargo.toml names its package")
                    .trim_matches('"')
                    .to_string()
            })
            .collect();
        on_disk.sort();

        let mut listed: Vec<&str> = LEAF_CRATES.to_vec();
        listed.sort();

        assert_eq!(
            listed,
            on_disk,
            "the host unit tests name {} crates but {} exist under crates/: a \
             leaf crate left out of `LEAF_CRATES` is compiled and never tested",
            listed.len(),
            on_disk.len()
        );
    }
}
