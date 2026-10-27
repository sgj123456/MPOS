//! `test-host`: run every `t-*` program against the **host** kernel.
//!
//! The suite is written as Linux conformance - each `t-*` makes a claim about
//! what a Linux call answers - so the same programs can also run on a real
//! Linux machine, which is what this command is for. It builds
//! `mpos-user-tests` for the host target and executes every program, one at a
//! time, reporting each answer and keeping its output beside the build.
//!
//! It is **not** the gate. `cargo xtask test` boots the kernel the claims are
//! about; this command measures the host instead, and a program that expects
//! the guest's devices (DRM, evdev, the mpos-specific `/proc` rows) or root will
//! say so here. Those failures are the honest answer, not a regression.
//!
//! Safety: the programs are the guest suite and some exercise privileged calls.
//! Run as root, `t-reboot-libc` would reboot the machine the command runs on,
//! so this refuses `euid == 0` outright. As a normal user the same calls answer
//! `EPERM`, which is what the failing lines report. Each program runs in its own
//! process group, from the guest's `/` (its `/init` starts there), so a
//! `kill(0, …)` or a relative file cannot reach the runner or the repository.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Machine;
use crate::utils;

/// The host target the suite is built for. Not the workspace default:
/// `.cargo/config.toml` pins bare-metal, and `--target` is what makes this a
/// program the host can run.
const HOST_TARGET: &str = "x86_64-unknown-linux-gnu";

/// One program's budget, from `tests.toml`'s `timeout`/`timeouts` keys for this
/// machine. A real Linux kernel finishes the suite in the same seconds the guest
/// gate does, so the file's answer is the right one here too - and it is one
/// place to change rather than a second constant that drifts.
///
/// `MPOS_HOST_TEST_TIMEOUT_SECS` still overrides every program at once: it is a
/// local escape hatch for a host whose disk is slow, not a second configuration
/// file.
fn env_timeout_override() -> Option<Duration> {
    std::env::var("MPOS_HOST_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map(Duration::from_secs)
}

/// Run the whole suite on the host.
pub fn run(build_dir: &Path) -> Result<()> {
    if running_as_root() {
        bail!(
            "test-host refuses to run as root: the suite includes privileged calls \
             (t-reboot-libc would reboot this machine). Re-run as a normal user; \
             the calls that need privilege will report EPERM instead."
        );
    }

    let root = utils::project_root()?;
    println!("test-host: building the user suite for {HOST_TARGET}");
    let mut build = Command::new("cargo");
    build
        .args([
            "build",
            "--release",
            "-p",
            "mpos-user-tests",
            "--target",
            HOST_TARGET,
        ])
        .current_dir(&root);
    // Captured rather than inherited: a successful build emits a few dozen
    // warnings from guest-only programs (`t-ns-*`, `t-cgroupfs`), and a runner's
    // header should not be buried in them. A failed build still prints all of it.
    let output = build
        .output()
        .context("Failed to run the host user-test build")?;
    if !output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        bail!("the host user tests did not build:\n{text}");
    }

    let out = utils::target_dir()?.join(HOST_TARGET).join("release");
    let mut tests = Vec::new();
    crate::selftest::collect_user_tests(&root.join("user/tests/src/bin"), &out, &mut tests)?;
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    // A debugging escape hatch, not a selection mechanism: the command's
    // contract is "every t-* program", and the filter only narrows a local run
    // (the guest gate's `tests.toml` groups are the real selection).
    if let Ok(filter) = std::env::var("MPOS_HOST_TEST_FILTER") {
        tests.retain(|test| test.name.contains(&filter));
    }
    if tests.is_empty() {
        bail!(
            "no host test program was built under {} - a green zero is the one \
             answer this command must not give",
            out.display()
        );
    }

    let scratch = build_dir.join("test-host");
    fs::create_dir_all(&scratch)?;
    println!(
        "test-host: {} programs, run against the host kernel ({})",
        tests.len(),
        release_of()
    );
    println!(
        "test-host: this measures the host, not mpos - a program that expects the \
         guest's root, devices or mpos-only `/proc` rows is expected to fail here"
    );

    let started = Instant::now();
    let config = crate::sweep::load_config()?;
    let forced = env_timeout_override();
    let mut passed = 0usize;
    let mut failed: Vec<(String, Outcome)> = Vec::new();
    for test in &tests {
        let log = scratch.join(format!("{}.log", test.name));
        let timeout = forced.unwrap_or_else(|| {
            Duration::from_secs(config.timeout_secs(Machine::X86_64, &test.name))
        });
        let outcome = run_one(&test.binary, &test.name, &log, timeout)?;
        match outcome {
            Outcome::Pass => passed += 1,
            ref other => failed.push((test.name.clone(), other.clone())),
        }
        println!("  {:<8} {}", outcome.label(), test.name);
        if !matches!(outcome, Outcome::Pass) {
            print_tail(&log);
        }
    }

    println!(
        "\ntest-host: {} of {} passed in {:.1}s (logs under {})",
        passed,
        tests.len(),
        started.elapsed().as_secs_f64(),
        scratch.display()
    );
    if failed.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = failed.iter().map(|(name, _)| name.as_str()).collect();
    bail!(
        "test-host: {} program(s) did not pass: {}",
        failed.len(),
        names.join(", ")
    )
}

/// The host kernel's release, for the header - the command is measuring this
/// machine, and naming it is what keeps a green run from being read as mpos's.
fn release_of() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown kernel".to_string())
}

#[derive(Clone)]
enum Outcome {
    Pass,
    /// The program exited with this code (a `t-*` program's `FAIL` is 1).
    Fail(i32),
    /// Killed by a signal - `Report::finish` never runs, so this is not a pass.
    Signal(i32),
    Timeout(u64),
    /// The program could not be started at all.
    Spawn(String),
}

impl Outcome {
    fn label(&self) -> String {
        match self {
            Outcome::Pass => "ok".to_string(),
            Outcome::Fail(code) => format!("FAIL({code})"),
            Outcome::Signal(sig) => format!("SIG({sig})"),
            Outcome::Timeout(secs) => format!("TIMEOUT({secs}s)"),
            Outcome::Spawn(err) => format!("ERROR({err})"),
        }
    }
}

/// Run one program with a fresh process group and output file, from the
/// directory the guest runner uses.
///
/// `stdout`/`stderr` go to `log` rather than a pipe: a program that fills a pipe
/// nobody is draining would block until the timeout and read as a hang. The
/// process group is fresh so the suite's own `kill(0, …)`/`kill(-pgid, …)`
/// claims cannot reach this runner or its terminal.
fn run_one(binary: &Path, name: &str, log: &Path, timeout: Duration) -> Result<Outcome> {
    let file =
        fs::File::create(log).with_context(|| format!("Failed to create {}", log.display()))?;
    let out = file
        .try_clone()
        .with_context(|| format!("Failed to clone {}", log.display()))?;
    let mut command = Command::new(binary);
    // The guest's `/init` runs every program from `/`; `t-stdargs` is the one
    // exception the guest runner makes (`cd /bin && PATH=/bin t-stdargs one two
    // three`), and it pins that argv exactly. Reproduce both here, so the host
    // run measures the suite rather than this runner's own conventions.
    if name == "t-stdargs" {
        command
            .current_dir("/bin")
            .arg0("t-stdargs")
            .args(["one", "two", "three"])
            .env("PATH", "/bin");
    } else {
        command.current_dir("/");
    }
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(file))
        .process_group(0)
        // The suite's own host branch: a program that knows it is not on mpos
        // prints a note or accepts the host's answer instead of pinning the
        // guest's. Without this the host run measures the suite's guest
        // assumptions, not the host.
        .env("MPOS_ON_HOST", "1")
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return Ok(Outcome::Spawn(err.to_string())),
    };
    let pid = child.id();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(match (status.code(), status.success()) {
                (_, true) => Outcome::Pass,
                (Some(code), false) => Outcome::Fail(code),
                // No code: the kernel ended it. On a host that is a real
                // answer (a signal was delivered), not a timeout.
                (None, _) => Outcome::Signal(signal_of(status)),
            });
        }
        if started.elapsed() >= timeout {
            // Kill the whole group: the suite forks, and a timed-out parent's
            // waiters would otherwise outlive the run.
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Timeout(timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(-1)
}

/// Print the last lines of a non-passing program's output, indented, so the
/// failing `FAIL:` line is visible without opening the log.
fn print_tail(log: &Path) {
    let Ok(mut file) = fs::File::open(log) else {
        return;
    };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let tail: Vec<&str> = text.lines().rev().take(12).collect();
    for line in tail.into_iter().rev() {
        // A failing program may print one very long line (the environment, a
        // buffer dump); the point is the failing claim, not a wall of text.
        let mut short: String = line.chars().take(240).collect();
        if line.chars().count() > 240 {
            short.push('…');
        }
        println!("           {short}");
    }
}

/// True when this process is root, asked through `id` rather than a new
/// dependency for one check.
fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
}
