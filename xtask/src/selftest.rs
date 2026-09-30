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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::qemu;
use crate::utils;

/// The verdict lines the test `/init` prints around every program, and the
/// summary line the parse below counts on. The markers are deliberately
/// shaped so a test's own output cannot forge a verdict: no test prints
/// `=== PASS` itself.
const SUMMARY_MARKER: &str = "=== SUMMARY";

/// How long the guest may run before the harness gives up on it. TCG is slow;
/// KVM is not. `MPOS_TEST_TIMEOUT` overrides, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

pub fn run_tests(build_dir: &Path, profile: &str) -> Result<()> {
    run_unit_tests()?;
    println!();

    utils::build_kernel(profile)?;
    println!();

    let tests = build_user_tests()?;
    println!("user tests: {} programs built", tests.len());
    println!();

    let tar = build_test_initramfs(build_dir, &tests)?;
    let (status, log) = boot_tests(build_dir, profile, &tar)?;

    report(&tests, &log, status)
}

/// The arch-independent kernel, tested on the host. `--target` is explicit
/// because the root `.cargo/config.toml` defaults every cargo command to the
/// bare-metal target, and the host tests are the opposite of that.
fn run_unit_tests() -> Result<()> {
    let root = utils::project_root()?;

    println!("Unit tests: the kernel (host)");
    let mut cmd = Command::new("cargo");
    cmd.args(["test", "--lib", "--target", "x86_64-unknown-linux-gnu"])
        .current_dir(&root);
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
struct UserTest {
    name: String,
    binary: PathBuf,
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
fn build_test_initramfs(build_dir: &Path, tests: &[UserTest]) -> Result<PathBuf> {
    println!("User tests: packing the test initramfs");

    let self_exe = std::env::current_exe().context("Failed to find the xtask executable")?;
    // The base initramfs only needs the privileged `initramfs` command when it
    // is actually out of date - that command takes root for its `chroot` step,
    // and a fresh archive needs no such thing. Building it unconditionally
    // would demand a password even when there is nothing to rebuild.
    if crate::initramfs::needs_rebuild(build_dir)? {
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
        .arg(utils::initramfs_path(build_dir))
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

/// The `/init` of the test initramfs. busybox is driven by absolute path
/// because the archive's applet links are the only ones it has; every program
/// matching `/bin/t-*` is a test, and `poweroff -f` is what ends the run -
/// the kernel answers `reboot(RB_POWER_OFF)` with QEMU's debug exit.
const TEST_INIT: &str = r#"#!/bin/sh
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
fn boot_tests(build_dir: &Path, profile: &str, tar: &Path) -> Result<(Option<i32>, PathBuf)> {
    println!("User tests: booting the test machine");

    let esp = build_dir.join("test-esp");
    if esp.exists() {
        fs::remove_dir_all(&esp)?;
    }
    fs::create_dir_all(esp.join("EFI/BOOT"))?;
    fs::copy(
        utils::loader_path(profile)?,
        esp.join("EFI/BOOT/BOOTX64.EFI"),
    )?;
    fs::copy(utils::kernel_path(profile)?, esp.join("mpos.elf"))?;
    fs::copy(tar, esp.join("initramfs.tar"))?;
    // Two words, both load-bearing. `console=ttyS0` makes the serial wire the
    // test boot's only console, so every line the harness reads is the whole of
    // what the machine said - there is no second console to be missing it on.
    // `mpos.selftest` is the switch the kernel reads to run its milestone checks
    // (the half-second clock wait and the scheduler sleeps); a normal boot leaves
    // it off and goes straight to init, and the test boot wants every check run.
    fs::write(esp.join("mpos.cmdline"), "console=ttyS0 mpos.selftest\n")?;

    let ovmf_dir = qemu::find_ovmf()?;
    let vars = utils::ovmf_vars_path(build_dir);
    fs::create_dir_all(build_dir)?;
    fs::copy(ovmf_dir.join("OVMF_VARS.4m.fd"), &vars)?;

    let log = build_dir.join("test-serial.log");
    let esp = esp.canonicalize()?;
    let log_path = log.display().to_string();
    // The same command line `run` boots with, from `[qemu]` in rootfs.toml -
    // except that the disk is this run's ESP and the serial line goes to a
    // file, because a test reads its verdict back out of it. The share and
    // the console socket live under the build directory the same way, made
    // here so the devices that want them never start without.
    // The share and console socket get their own directory, not `run`'s:
    // `machine_args` binds `console.sock` inside this dir and exports it over
    // 9p, and a second QEMU (a live `cargo xtask run`) already holds those -
    // the bind would fail before the guest boots.
    let share = build_dir.join("test-9p-share");
    fs::create_dir_all(&share)?;
    let mut config = crate::config::QemuConfig::load_default()?;
    // A test boot never needs the host vsock bus, and claiming `guest-cid=3`
    // collides with any VM already running on this host (a concurrent
    // `cargo xtask run`, say) - which would abort QEMU before the guest boots
    // and report every test as failed. Leave it off here; `run` keeps it.
    config.vsock = false;
    let args = qemu::machine_args(
        &config,
        &ovmf_dir,
        &vars,
        qemu::Boot::Esp(esp.clone()),
        qemu::Console::File(log_path.clone()),
        &share,
    );
    // `isa-debug-exit` - the port the kernel writes its verdict to - comes with
    // every machine `machine_args` builds.

    eprintln!("+ qemu-system-x86_64 {}", args.join(" "));
    let mut child = Command::new("qemu-system-x86_64")
        .args(&args)
        .spawn()
        .context("Failed to start QEMU")?;

    let timeout = Duration::from_secs(test_timeout_secs());
    let started = Instant::now();
    let mut status: Option<std::process::ExitStatus> = None;
    while started.elapsed() < timeout {
        match child.try_wait()? {
            Some(finished) => {
                status = Some(finished);
                break;
            }
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "The test machine did not finish within {}s; its serial output is at {}",
            timeout.as_secs(),
            log.display()
        );
    }
    let status = status.unwrap();
    if !status.success() && status.code() != Some(33) {
        eprintln!(
            "test: note: QEMU exited with {} (33 is the clean debug exit)",
            status.code().unwrap_or(-1)
        );
    }
    Ok((status.code(), log))
}

fn test_timeout_secs() -> u64 {
    std::env::var("MPOS_TEST_TIMEOUT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Reads the run's verdicts off the serial log and answers for every test that
/// was built: a test that never printed `=== PASS` failed, whether it
/// crashed, hung or never ran.
fn report(tests: &[UserTest], log: &Path, status: Option<i32>) -> Result<()> {
    let text = fs::read_to_string(log)
        .with_context(|| format!("Failed to read the serial log at {}", log.display()))?;

    let mut passed: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("=== PASS ") {
            passed.push(name.trim());
        } else if let Some(rest) = line.strip_prefix("=== FAIL ") {
            failed.push(rest.split_whitespace().next().unwrap_or(""));
        }
    }

    let summary = text
        .lines()
        .rev()
        .find(|line| line.starts_with(SUMMARY_MARKER));
    let clean = status == Some(33) && summary.is_some();

    let missing: Vec<&str> = tests
        .iter()
        .filter(|test| !passed.contains(&test.name.as_str()))
        .map(|test| test.name.as_str())
        .collect();

    println!();
    if let Some(summary) = summary {
        println!("self-test guest: {summary} ({} built)", tests.len());
    } else {
        println!("self-test guest: no summary line - the run did not get to the end");
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
            "self-test: all {} user tests passed, unit tests passed",
            tests.len()
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
