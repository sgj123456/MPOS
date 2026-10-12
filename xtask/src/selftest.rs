//! The `test` command: the kernel's self-test sweep.
//!
//! Three stages, all of which have to pass for the command to succeed:
//!
//! 1. The host unit tests - the arch-independent core of the kernel (`cargo
//!    test --lib`) and xtask's own.
//! 2. The user test programs. `user/tests` is built, packed into an initramfs,
//!    and booted in QEMU without a disk: the test `/init` runs every `/bin/t-*`
//!    it finds, prints a
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

/// How long the guest may run before the harness gives up on it. TCG is slow;
/// KVM is not. `MPOS_TEST_TIMEOUT` overrides, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

pub fn run_tests(build_dir: &Path, profile: &str, selection: sweep::Selection) -> Result<()> {
    check_target_gates()?;
    check_boot_openings()?;
    run_unit_tests()?;
    println!();

    // The x86_64 sweep: its own initramfs on the QEMU wiring in `machine.toml`.
    // Named rather than left to the workspace default, so the machine this
    // sweep boots and the machine it builds are the same value.
    //
    // The *test* build of the kernel, not the ordinary one: this sweep is what
    // the boot's suites exist for, and they are only in the image a
    // `--features selftest` build produces (`utils::build_test_kernel` says why
    // the two cannot be the same artifact).
    // The image it returns is not staged here any more: the loader this call
    // built alongside the kernel *is* the kernel's container now
    // (`boot/build.rs`), so the ESP below carries the loader and nothing else
    // from this pair. The call is for its effect, and the effect is the order -
    // the self-test kernel first, the loader that embeds it directly after.
    utils::build_test_kernel(build_dir, profile, Arch::X86_64)?;
    println!();

    let built = build_user_tests()?;
    println!("user tests: {} programs built", built.len());

    // Which of them this run boots is the configuration's answer; the group
    // refusal, the selection and the two lines under it are `sweep`'s, because
    // they are questions about `tests.toml` rather than about this machine.
    let config = sweep::load_config()?;
    // The declared-name check, and *this* arm's scope: every name in the file
    // has to be answerable by a program this run built, or it is a stale entry.
    // The second machine's arm asks the narrower question of its own `gaps`
    // alone - both arms build the same std suite since §15.11's last batch
    // deleted the `no_std` one, so the two answers now coincide for every
    // `[default]` name; `sweep::select` records why the scopes are still two.
    let names: Vec<String> = built.iter().map(|test| test.name.clone()).collect();
    let unmatched = config.unmatched(&names);
    if !unmatched.is_empty() {
        bail!(
            "tests.toml lists {}, which no built test program answers to",
            unmatched.join(", ")
        );
    }
    let plan = sweep::select(crate::config::Machine::X86_64, &config, &built, &selection)?;

    // `tests.toml`'s own rule, one question answered from the file rather than
    // from a built-program list: would a *plain* sweep stage this name? Both
    // §10.5 instruments must answer yes on both machines - they are exact
    // counts, not timings, so they belong in the gate - and the answer is what
    // the notes' group-membership claim rests on. The staged sets themselves
    // are decided by `select`, which this check shares `runs` with (pinned in
    // `sweep::tests`). Same `config` the selection above read.
    for machine in [
        crate::config::Machine::X86_64,
        crate::config::Machine::Aarch64,
    ] {
        for instrument in ["t-ext4-mount", "t-diskbench", "t-netstats"] {
            if !sweep::in_default(machine, &config, instrument) {
                bail!(
                    "{instrument} is not in {machine:?}'s default sweep; §10.5's \
                     instrument only measures when its programs stage everywhere \
                     (see tests.toml's comment above the slow group)"
                );
            }
        }
    }

    // What is left is this arm's own: the throwaway initramfs, the QEMU wiring
    // in `machine.toml`, and a verdict that can quote an exit status. The
    // disk-counter word is decided here (§10.5): armed only when *this* run
    // stages the program that reads it back - the same question the staging
    // above asks of the same list, so the boot and its payload cannot disagree.
    let tar = build_test_initramfs(build_dir, &plan.staged)?;
    let diskstat = diskstat_for_staged(plan.staged.iter().map(|test| test.name.as_str()));
    let (status, log) = boot_tests(build_dir, profile, &tar, diskstat)?;

    report(&plan.staged, plan.built, &log, status, diskstat)
}

/// The structural check that belongs ahead of the compiler: a `#[cfg(test)]`
/// module whose file only a `target_os = "none"` build reaches has never run,
/// and nothing else in this pipeline would ever say so (`NOTES.md` §11.2 Step F,
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
         to a program that boots (NOTES.md §11.2 Step F)",
        dead.len()
    );
}

/// The other structural check, and the same kind of blindness: a test opening in
/// the boot's own code is a *word* - a command-line marker, a run-time feature
/// ask, an `M*` string - so nothing in the toolchain notices one coming back.
/// Seven files and a text match, which is cheap enough to run on every sweep
/// (`NOTES.md` §15.19 for the mechanism, §15.24 for the vocabulary).
fn check_boot_openings() -> Result<()> {
    println!("Target gates: the boot has no test opening");
    let openings = crate::gates::test_openings(&utils::project_root()?)?;
    if openings.is_empty() {
        return Ok(());
    }
    for opening in &openings {
        eprintln!(
            "  {}:{} {}: {}",
            opening.file, opening.line, opening.what, opening.code
        );
    }
    bail!(
        "{} test opening(s) in the boot's own code; whether a kernel carries tests \
         is the build's to say, not a word's or a run-time check's (NOTES.md §15.19)",
        openings.len()
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
    "mpos-acpi",
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

/// Builds the user test crate and returns what it produced.
///
/// It builds like any musl program, and it is the suite's only crate: the
/// `#![no_std]` conformance suite that used to sit beside it in `user/abitest`
/// was moved here one batch at a time, and the move is finished - so there is
/// one build, one target, and one `src/bin` for the sweep to walk.
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
    utils::run_status(&mut cmd, "the user tests")?;

    collect_user_tests(
        &root.join("user/tests/src/bin"),
        &target_dir()?.join("x86_64-unknown-linux-musl/release"),
        &mut tests,
    )?;
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tests)
}

/// Find every test program under `src_dir`, recursing into subdirectories.
///
/// The walk covers subdirectories because the suite used to be split by what a
/// test needs, and the binary keeps the file's name either way: that is what a
/// `/bin/t-*` run and the verdict line are keyed on.
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

    // The programs ride the archive's *tail*, not a repacked copy of the Alpine
    // tree: each staged binary is appended as its own ustar member after the
    // base archive's terminator. A second copy of the whole base would be paid
    // twice - once in host bytes, once in the guest's ramfs charge, whose limit
    // is a quarter of its RAM (§10.4) - and the x86_64 sweep's 2 GiB leaves
    // room for neither a doubled 70 MB nor the file `t-fill-ramfs` fills with.
    // The boot's tar reader recognizes a member by its `ustar` magic and stops
    // at anything else (`src/vfs/tar.rs`), so a tail written after the base's
    // data - with the base's zero-block terminator removed - is one stream to
    // it; the appended members carry the same `ustar` format and numeric-zero
    // owner/group the pack below always did. The runner
    // script still rides as its own member, written here rather than copied
    // from a staging tree.
    // §10.5's ext4 fixture rides only when this run stages the programs that
    // read it. It costs 80 MiB unpacked (the pattern file plus the filesystem's
    // own geometry), which the x86_64 sweep's ramfs budget - a quarter of its
    // 2 GiB (§10.4) - absorbs with room; a plain sweep pays it because both
    // instruments are in the default list, and a selection that left them out
    // (`--groups`, or a membership edit in tests.toml) leaves the tail alone.
    // The member is named absolutely because that is how the boot's tar reader
    // spells an absolute path; the disk arm's entry drops the slash for the
    // same guest answer from its own channel shape.
    let mut extra: Vec<(&str, PathBuf)> = Vec::new();
    if tests
        .iter()
        .any(|test| test.name == "t-ext4-mount" || test.name == "t-diskbench")
    {
        let image = crate::disk::build_ext4_image(build_dir)?;
        extra.push((DISKSTAT_FIXTURE_MEMBER, image));
    }
    let extra_refs: Vec<(&str, &Path)> = extra
        .iter()
        .map(|(name, path)| (*name, path.as_path()))
        .collect();
    let tar = build_dir.join("test-initramfs.tar");
    append_test_members(
        &crate::initramfs::archive_path(build_dir, Arch::X86_64),
        &tar,
        tests,
        &extra_refs,
    )?;

    let size = fs::metadata(&tar)?.len();
    println!(
        "test initramfs: {} ({:.1} MB)",
        tar.display(),
        size as f64 / 1048576.0
    );
    Ok(tar)
}

/// One ustar header block: the shape `src/vfs/tar.rs` parses, spelled here
/// because the writer and the reader must agree on it byte for byte.
fn ustar_header(name: &str, mode: u32, size: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    // The name fits - every test program is `/bin/t-<word>` - but refuse to
    // silently truncate it rather than mis-stage a payload.
    assert!(name.len() < 100, "member name too long for ustar: {name}");
    // A plain fn item borrows the buffer mutably on each call and gives the
    // borrow back; a closure captured one borrow for its whole life, which is
    // how the first draft built a header whose every field was zero.
    fn field<const N: usize>(h: &mut [u8; N], at: usize, text: &str) {
        let bytes = text.as_bytes();
        h[at..at + bytes.len()].copy_from_slice(bytes);
        // Fixed-width octal fields are zero-padded by their format below; text
        // fields get a NUL terminator and nothing else, which every reader
        // accepts.
        h[at + bytes.len()] = b'\0';
    }
    field(&mut h, 0, name);
    let mode_text = format!("{mode:07o}");
    field(&mut h, 100, &mode_text);
    field(&mut h, 108, "0000000"); // uid
    field(&mut h, 116, "0000000"); // gid
    let size_text = format!("{size:011o}");
    field(&mut h, 124, &size_text);
    field(&mut h, 136, "00000000000"); // mtime 0
    field(&mut h, 148, ""); // chksum placeholder: eight spaces below
    h[148..156].fill(b' ');
    // typeflag: regular file.
    h[156] = b'0';
    field(&mut h, 157, ""); // linkname
    field(&mut h, 257, "ustar");
    h[262] = b'\0';
    field(&mut h, 263, "00"); // version
    write_checksum(&mut h);
    h
}

/// Fill a header block's chksum field the way every tar reader verifies it:
/// the sum of the whole block *with the eight checksum bytes themselves read
/// as spaces*. Shared by the writer above and the rename below, because a
/// checksum computed over the old field is wrong in exactly one byte-group
/// and GNU tar calls the result "does not look like a tar archive".
fn write_checksum(block: &mut [u8]) {
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|&b| b as u32).sum();
    let digits = format!("{sum:07o}\0");
    block[148..156].copy_from_slice(digits.as_bytes());
}

/// One tar octal field: ASCII-octal digits, terminated by NUL or whitespace,
/// accepted left-padded or not. `src/vfs/tar.rs`'s rule, copied verbatim so the
/// writer here and the reader in the kernel agree on every byte they parse.
fn octal(field: &[u8]) -> Option<u64> {
    let text = core::str::from_utf8(field).ok()?;
    let text = text.trim_end_matches(|c: char| c == '\0' || c.is_ascii_whitespace());
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    u64::from_str_radix(text, 8).ok()
}

/// Walk `bytes` as a ustar stream, calling `at_header` with each header block.
///
/// The same walk `src/vfs/tar.rs` makes: a header block, then its data rounded
/// up to 512; two all-zero blocks end it; a block that is neither parses no
/// further. Shared by the rename below and by any future tail edit so the two
/// readers of this format cannot drift from each other.
fn for_each_header(bytes: &mut [u8], mut at_header: impl FnMut(&mut [u8]) -> bool) {
    let mut at = 0usize;
    while at + 512 <= bytes.len() {
        let header = &mut bytes[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            return; // the terminator
        }

        // A member whose size field cannot be parsed stops the walk rather
        // than letting it land mid-data and read a payload block as a header.
        let Some(size) = octal(&header[124..136]) else {
            return;
        };
        let done = at_header(header);
        if done {
            return;
        }
        // The reader's own rule (`src/vfs/tar.rs`): the data is `size` bytes,
        // rounded up to a block.
        at += 512 + (size as usize).div_ceil(512) * 512;
    }
}

/// Rewrite the base archive's `./init` header's name field to `./init.boot`.
///
/// A name-only edit: the checksum covers the header with its chksum field read
/// as spaces, so recomputing it here is the whole of the work. Anything that
/// parses the result - GNU `tar`, this kernel's reader - sees the same bytes
/// under a different name. Answers whether the rename happened.
fn rename_root_init(bytes: &mut [u8]) -> bool {
    let mut renamed = false;
    for_each_header(bytes, |header| {
        if header[156] != b'0' {
            return false; // keep walking; only regular files can collide
        }
        // The name is a C string: byte-for-byte the six characters and their
        // terminator, which a `starts_with` would answer for `./initial` too.
        if header[..7] == *b"./init\0" {
            // "./init.boot" is eleven bytes and the field holds a hundred, so
            // one copy does it - writing byte 11 by hand left byte 10's `t`
            // out of place and renamed nothing while claiming it did.
            header[0..11].copy_from_slice(b"./init.boot");
            header[11] = 0;
            write_checksum(header);
            renamed = true;
            return true; // one /init; stop the walk
        }
        false
    });
    renamed
}

/// Write `base` plus one ustar member per staged program into `out`.
///
/// The base archive is copied unchanged and its trailing zero blocks are kept:
/// `src/vfs/tar.rs` stops at two consecutive all-zero blocks, so the appended
/// members simply continue the walk. Two final zero blocks close the stream,
/// which is also what a GNU `tar -tvf` answers when asked to list the result.
/// `extra` rides after the programs as plain members — the ext4 fixture's path
/// and bytes — so one archive carries everything the test boot reads.
fn append_test_members(
    base: &Path,
    out: &Path,
    tests: &[&UserTest],
    extra: &[(&str, &Path)],
) -> Result<()> {
    use std::io::{BufWriter, Write};
    let mut w = BufWriter::new(
        fs::File::create(out).with_context(|| format!("Failed to create {}", out.display()))?,
    );
    let mut base_bytes =
        fs::read(base).with_context(|| format!("Failed to read {}", base.display()))?;
    if !rename_root_init(&mut base_bytes) {
        bail!(
            "the base initramfs carries no ./init to move aside; rebuild it \
             (`cargo xtask initramfs`) before staging programs"
        );
    }
    // The base's own end-of-archive pair is dropped: the tail continues the
    // same member stream, and one terminator closes the whole thing below.
    while base_bytes.ends_with(&[0u8; 512]) {
        base_bytes.truncate(base_bytes.len() - 512);
    }
    w.write_all(&base_bytes)?;
    // The base's own `/init` - the switch-root script the *session* boot runs -
    // is moved aside in the copy below, because `src/vfs/tar.rs` refuses a
    // second regular file of the same name outright (`Exists` stops the unpack,
    // and the boot treats a stopped unpack as a broken archive). Renaming it to
    // `init.boot` keeps it inspectable on the payload while leaving `/init` free
    // for the runner, which is the one member whose bytes changed since the old
    // staging tree overwrote it. Programs ride after both.
    let init = TEST_INIT.as_bytes();
    let header = ustar_header("./init", 0o755, init.len() as u64);
    w.write_all(&header)?;
    w.write_all(init)?;
    w.write_all(&vec![0u8; (512 - init.len() % 512) % 512])?;
    for test in tests {
        let bytes =
            fs::read(&test.binary).with_context(|| format!("Failed to stage {}", test.name))?;
        let header = ustar_header(&format!("./bin/{}", test.name), 0o755, bytes.len() as u64);
        w.write_all(&header)?;
        w.write_all(&bytes)?;
        let pad = (512 - bytes.len() % 512) % 512;
        w.write_all(&vec![0u8; pad])?;
    }
    for (name, path) in extra {
        let bytes = fs::read(path).with_context(|| format!("Failed to stage {name}"))?;
        let header = ustar_header(name, 0o644, bytes.len() as u64);
        w.write_all(&header)?;
        w.write_all(&bytes)?;
        let pad = (512 - bytes.len() % 512) % 512;
        w.write_all(&vec![0u8; pad])?;
    }
    w.write_all(&[0u8; 1024])?;
    w.flush()?;
    Ok(())
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

# The host share, mounted where the runner's payload reads it. The *kernel*
# does not mount it - a mount belongs to the owner of the tree it lands on, and
# on this machine that is this runner (the distribution's initramfs script
# mounts the same share on the root it promotes, which is why the mount here is
# allowed to fail: on that machine the share is already there, and a second
# mount over the same point is either the same device twice or an honest
# refusal this script has nothing to do about). An empty source is this
# kernel's spelling for "the share the bus exported", so no tag is copied here.
/bin/busybox mkdir -p /mnt/share
/bin/busybox mount -t 9p '' /mnt/share || echo "test init: no host share to mount"

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

/// The boot word that arms §10.5's disk-counter protocol, with its leading
/// space: empty when the run does not stage the instrument. The kernel reads
/// this exact word (`src/board/flow.rs`'s `DISKSTAT_WORD`), and the two files
/// cannot share one definition - xtask is a host binary and the kernel is the
/// thing booted - so [`tests::the_diskstat_word_matches_the_kernels_constant`]
/// pins them against each other instead.
const DISKSTAT_ON: &str = " mpos.diskstat";

/// Where §10.5's ext4 fixture lands inside either machine's payload - the same
/// path on both machines, which is what lets `t-ext4-mount` open one name here
/// regardless of whose sweep it is in. The x86_64 ramfs unpack turns the member
/// `/dev-sda-fs.img` into exactly this file (a leading slash is how its reader
/// names an absolute path); the aarch64 medium stages the bytes under the same
/// name because an ext4 tree is already rooted - the same guest answer from two
/// channel shapes.
///
/// **At the root, and deliberately not under `/dev`**, which is where the first
/// draft put it. Both boots mount a `devtmpfs` over `/dev` - the x86_64 runner's
/// script does it (and the second machine's initramfs chain before it) - so a
/// payload file unpacked into `/dev` is shadowed the moment that mount lands.
/// Measured on the pair's first gate run: the harness staged the fixture and both
/// programs reported it missing (`ENOENT`), because the bytes existed in the
/// payload and in nobody's `/dev`. The root is not a mountpoint on either
/// machine.
const DISKSTAT_FIXTURE_MEMBER: &str = "/dev-sda-fs.img";

/// The fixture's name as a *disk* entry: the member above without the absolute
/// slash, spelled out rather than derived so
/// [`tests::both_payload_channels_lay_the_fixture_at_one_path`] compares two
/// literals instead of a value against itself. Both arms push through this one
/// constant.
const DISKSTAT_FIXTURE_ENTRY: &str = "dev-sda-fs.img";

/// §10.5's disk-counter protocol: the boot word, armed when this run's staged
/// set actually contains `t-diskbench`.
///
/// The arming is a *boot word* (`mpos.diskstat` on the kernel command line), and
/// this is the only place that decides whether to put it there - asked of the
/// *staged list itself*, which both arms have by the time they write a boot
/// line, and which the fixture staging above reads the same question from. The
/// pairing is what makes it honest: a boot armed while nothing reads the
/// counters back is a machine whose `uname` answers a private shape for no
/// reason. A name moved out of the default group in `tests.toml` leaves the
/// staged set, un-arms the sweep with no other edit, and drops the 80 MiB
/// fixture with it: the instrument leaves no half-state behind.
///
/// The `staged` iterator yields whatever shape the arm keeps its names in.
fn diskstat_for_staged<'a>(mut staged: impl Iterator<Item = &'a str>) -> &'static str {
    if staged.any(|name| name == "t-diskbench") {
        DISKSTAT_ON
    } else {
        ""
    }
}

/// Boots the test initramfs in QEMU and returns the exit status with the path
/// of the captured serial log. The boot disk is QEMU's virtual FAT: the loader
/// and the test initramfs are served out of a plain directory, so no image
/// needs creating and no privilege is needed to write it.
///
/// No kernel file is staged here any more, and that is the loader's doing rather
/// than this function's: the loader carries the kernel *inside itself*
/// (`boot/build.rs`), so a file beside it would be a file nothing opens. What
/// decides which kernel this boot runs is therefore the loader - the one
/// `utils::build_test_kernel` built a moment ago, immediately after the
/// self-test kernel it had just made.
fn boot_tests(
    build_dir: &Path,
    profile: &str,
    tar: &Path,
    diskstat: &str,
) -> Result<(Option<i32>, String)> {
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
    fs::copy(tar, esp.join("initramfs.tar"))?;
    // `console=ttyS0` makes the serial wire the test boot's only console, so
    // every line the harness reads is the whole of what the machine said - there
    // is no second console to be missing it on.
    //
    // There used to be a second word here, `mpos.selftest`, and the kernel used
    // to parse it to decide whether to run its checks. Nothing reads it now:
    // whether a machine runs its suites is which *image* was built, so the test
    // boot is the build `utils::build_test_kernel` made and the word would be a
    // word nobody answers to. It is gone rather than left in as a marker, because
    // a marker the kernel ignores is exactly the shape that reads as a mechanism
    // and is not one.
    fs::write(
        esp.join("mpos.cmdline"),
        format!("console=ttyS0{diskstat}\n"),
    )?;

    // A SATA disk beside the ESP, so the test machine has *two* disks from two
    // drivers: the ESP reaches the guest through virtio-blk, and this one
    // through the AHCI controller the `Boot::Esp` arm attaches. It is what
    // `t-ahci` reads, writes and reads back, which is the only way a driver
    // whose every line is a volatile register access gets pinned at all
    // (`NOTES.md` §7.3). Sparse and small: the test writes one sector and reads
    // it back, and nothing else in the suite touches it.
    //
    // `disk`'s function and not a local one: both machines' wiring attaches this
    // disk now, so the file is one fixture with one name that either arm lays.
    // Laid *fresh* before every sweep: the suite writes to this disk (`t-ahci` a
    // sector, §10.5's pair a whole filesystem), and the claims about the bytes
    // around what was written are only claims if the state before them is known
    // — on a file that outlives the run, "ensured" would mean "whatever the last
    // run left" (`disk::reset_sata_image`).
    crate::disk::reset_sata_image(build_dir)?;

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
    // exported and never reachable (`NOTES.md` §13.29).
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
fn report(
    tests: &[&UserTest],
    built: usize,
    text: &str,
    status: Option<i32>,
    diskstat: &str,
) -> Result<()> {
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

    // The kernel's own suites, which ran before any program did. Both halves of
    // that sentence are asserted: the line has to be *there* (a test kernel that
    // reports nothing has not said its suites ran), and it has to say zero
    // failed.
    let kunit = verdict.kunit;

    println!();
    if let Some(summary) = summary {
        println!("self-test guest: {summary} ({} built)", tests.len());
    } else {
        println!("self-test guest: no summary line - the run did not get to the end");
    }
    match kunit {
        Some(kunit) => println!(
            "self-test guest: mpos-kunit {} passed, {} failed",
            kunit.passed, kunit.failed
        ),
        None => println!("self-test guest: the kernel printed no mpos-kunit line"),
    }
    let skipped = built.saturating_sub(tests.len());
    if skipped > 0 {
        println!("self-test guest: {skipped} of {built} user tests left out by tests.toml");
    }
    report_diskstat(text, diskstat)?;
    let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    report_netstat(text, &names)?;
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

    if !clean || kunit.map(|kunit| kunit.failed).unwrap_or(1) != 0 {
        println!();
        println!("self-test: the last of the serial log:");
        let tail: Vec<&str> = text.lines().collect();
        for line in tail.iter().rev().take(30).rev() {
            println!("  | {line}");
        }
    }

    println!();
    let suites_clean = kunit
        .map(|kunit| kunit.failed == 0 && kunit.passed > 0)
        .unwrap_or(false);
    if clean && missing.is_empty() && suites_clean {
        let kunit = kunit.expect("checked above");
        println!(
            "self-test: all {} user tests passed, unit tests passed, and the kernel's {} self-test cases passed{}",
            tests.len(),
            kunit.passed,
            if skipped > 0 {
                format!(" ({skipped} not run - see tests.toml)")
            } else {
                String::new()
            }
        );
        Ok(())
    } else {
        bail!(
            "self-test failed: {} of {} user tests did not pass{}, and the kernel's suites report {}",
            missing.len(),
            tests.len(),
            if clean {
                ""
            } else {
                " and the run did not end cleanly"
            },
            match kunit {
                Some(kunit) => format!("{} passed, {} failed", kunit.passed, kunit.failed),
                None => "nothing at all - no mpos-kunit line".to_string(),
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

/// The image size a disk sweep asks for. It is not a fixed number because the
/// payload moves: the std-side programs are ~57 MB today (measured: `mkfs.ext4`
/// answering `无法为 ext2 文件系统分配块` at 64 MiB once §15.11's migration put
/// the instruments on this machine too), and the tree also carries the archive's
/// own busybox/applet farm under `/bin`. This is its number plus room, rather
/// than one every boot pays for - a sweep that overflows its root reads as a
/// build failure, not as a full disk.
const SWEEP_DISK_BYTES: u64 = 96 * 1024 * 1024;

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
pub fn run_disk_sweep(
    build_dir: &Path,
    profile: &str,
    arch: Arch,
    selection: sweep::Selection,
) -> Result<()> {
    // The kernel this sweep boots has to be the current one, and it is this
    // arm's own build: the first machine's is done by `run_tests`, and `main`
    // does not build for `test` on either machine. It is the *test* build, so the
    // boot's suites are in the image at all (`utils::build_test_kernel`).
    let kernel = utils::build_test_kernel(build_dir, profile, arch)?;
    println!();
    let loader = utils::loader_path(profile, arch)?;
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
    // other arm asks, with the same answers. The reason-groups bind this machine
    // as they bind the first one: what `external` and `slow` name is outside the
    // machine under test, and slirp's DHCP server, the host's resolver and a
    // public address are outside this one too. `--groups` reaches this arm as
    // well now, which is what keeps those programs runnable here at all.
    let plan = sweep::select(machine, &config, &built, &selection)?;
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
    //
    // The runner lands at `/sbin/init` because that *is* the disk's pid-1 chain
    // now: the shared initramfs script execs `/sbin/init` after the switch on
    // both machines (`assets/initramfs/core/init`), the name a standard Alpine
    // disk answers to, and the payload lands last so the runner stands where the
    // installed image's own `/sbin/init` would. It used to be `/init`, which was
    // the second machine's own chain back when this machine's disk carried one.
    let mut entries: Vec<(&str, &[u8])> = vec![("sbin/init", TEST_INIT.as_bytes())];
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
    let rootfs = crate::config::RootfsConfig::load(
        &utils::project_root()?.join(crate::paths::ROOTFS_CONFIG),
        arch,
    )?;
    // §10.5's fixture and the boot word it arms, both keyed on this run's
    // staged set: the image rides the sweep medium as `dev/dev-sda-fs.img`,
    // which lands at the *same* guest path the first machine's archive member
    // unpacks to (`/dev/dev-sda-fs.img`), so one test program reads either
    // machine — and `mpos.diskstat` goes on the line only when `t-diskbench` is
    // in the set that will read the counters back.
    let diskstat = diskstat_for_staged(plan.staged.iter().map(|(name, _)| name.as_str()));
    // The fixture rides only when the programs that read it are staged: a
    // sweep without them does not pay 80 MiB of root partition for an image
    // nothing opens. With them, the medium grows to carry it - `mkfs.ext4`
    // answers "no blocks" rather than a warning when the tree overflows the
    // size, and a sweep that dies in the builder reads as a build failure.
    let mut disk_bytes = SWEEP_DISK_BYTES;
    let mut fixture: Option<Vec<u8>> = None;
    if plan
        .staged
        .iter()
        .any(|(name, _)| name == "t-ext4-mount" || name == "t-diskbench")
    {
        let image = crate::disk::build_ext4_image(build_dir)?;
        let bytes = fs::read(&image)
            .with_context(|| format!("Failed to read the ext4 fixture {}", image.display()))?;
        disk_bytes += bytes.len() as u64 + 16 * 1024 * 1024;
        fixture = Some(bytes);
    }
    if let Some(bytes) = &fixture {
        // The same path as the archive member, without the leading slash: a
        // disk entry is named from the root already.
        entries.push((DISKSTAT_FIXTURE_ENTRY, bytes.as_slice()));
    }
    let cmdline = format!(
        "console={} mpos.selftest root={} rootfstype={}{}\n",
        rootfs.serial_port, rootfs.root_device, rootfs.filesystem, diskstat
    );
    let disk = crate::disk::sweep_medium(
        build_dir,
        arch,
        &crate::disk::Boot {
            kernel: &kernel,
            loader: &loader,
            entries: &entries,
            disk_bytes,
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
    report_disk_sweep(build_dir, arch, &staged, plan.skipped.len(), &log, diskstat)
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

/// §10.5's instrument line, printed by both sweep arms when the boot was armed.
///
/// An armed run that leaves no reading behind is refused - the same rule the
/// kernel's own suites are held to ("a suite that silently never ran leaves its
/// cases out of the count"): an instrument whose numbers vanished would make a
/// green sweep read as a measured one. An un-armed run prints nothing and fails
/// nothing; the numbers themselves are the payload's, not a verdict.
fn report_diskstat(log: &str, diskstat: &str) -> Result<()> {
    if diskstat.is_empty() {
        return Ok(());
    }
    let readings = sweep::diskstat_readings(log);
    if readings.is_empty() {
        bail!(
            "the sweep booted armed for the disk-counter protocol but \
             `t-diskbench` left no DISKSTAT line in the log: the instrument \
             did not run"
        );
    }
    println!("self-test (diskstat): device reads counted through the ext4 mount");
    for (disk, cold, warm) in readings {
        println!("  {disk}: cold={cold} warm={warm}");
    }
    // §10.5's other half, where the machine could be asked it: the pass that read
    // the fixture *file* off the disk the root lives on. A machine whose root is
    // the boot archive leaves no such line, and `t-diskbench` says which case it
    // ran rather than passing silently.
    for (disk, calls, blocks) in sweep::bootdisk_readings(log) {
        println!("  {disk} (boot disk): bootcold={calls} of {blocks} blocks");
    }
    Ok(())
}

/// §10.4's instrument line, printed by both sweep arms when the run staged it.
///
/// Unlike the disk-counter reading, this door needs no arming - `/proc/net/dev`
/// answers on every boot - so the claim here is only about *visibility*: a run
/// that staged `t-netstats` and left no `NETSTAT` line behind refused itself in
/// the program's own verdict (a missing PASS fails the sweep), and what lands
/// here is the number, printed for the record. Rule 6's before/after figure,
/// automated rather than trusted to whoever ran the gate.
fn report_netstat(log: &str, staged: &[&str]) -> Result<()> {
    if !staged.contains(&"t-netstats") {
        return Ok(());
    }
    let readings = sweep::netstat_readings(log);
    let Some((tx, rate, hits, misses)) = readings.first() else {
        bail!(
            "the sweep staged t-netstats but its NETSTAT line never reached \
             the log: the instrument's number was lost between the guest and \
             the report"
        );
    };
    println!("self-test (netstat): frame counters through /proc/net/dev");
    println!("  tx={tx} datagrams_per_sec={rate} pool={hits}:{misses}");
    Ok(())
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
    diskstat: &str,
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

    // The kernel's own suites, which ran before any program did - the same
    // two-part assertion the first machine's arm makes: the line has to be
    // there, and it has to say zero failed.
    let kunit = verdict.kunit;
    let suites_clean = kunit
        .map(|kunit| kunit.failed == 0 && kunit.passed > 0)
        .unwrap_or(false);
    match kunit {
        Some(kunit) => println!(
            "self-test ({}): mpos-kunit {} passed, {} failed",
            arch.name(),
            kunit.passed,
            kunit.failed
        ),
        None => println!(
            "self-test ({}): the kernel printed no mpos-kunit line",
            arch.name()
        ),
    }
    if !suites_clean {
        bail!(
            "self-test ({}) failed: the kernel's own suites report {}",
            arch.name(),
            match kunit {
                Some(kunit) => format!("{} passed, {} failed", kunit.passed, kunit.failed),
                None => "nothing at all - no mpos-kunit line, so they did not run".to_string(),
            }
        );
    }

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
    report_diskstat(log, diskstat)?;
    report_netstat(log, staged)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        DISKSTAT_FIXTURE_ENTRY, DISKSTAT_FIXTURE_MEMBER, DISKSTAT_ON, LEAF_CRATES, UserTest,
        append_test_members, ustar_header,
    };

    /// The tail-staging writer's own format claims, against the host `tar`:
    /// the checksum rule both readers enforce ("sum with the chksum field read
    /// as spaces"), the rename that moves the base's `/init` aside so the
    /// runner can take the name back without the unpack's duplicate-file
    /// refusal, and the stream GNU tar lists as exactly the members the boot
    /// will unpack - the moved-aside script, the runner, the staged program,
    /// once each. Pinned through `append_test_members` itself rather than its
    /// parts, because the parts agreeing with each other is how the first
    /// draft shipped a header every byte of which was zero.
    #[test]
    fn the_tail_staging_writes_a_stream_both_readers_agree_on() {
        let dir = std::env::temp_dir().join("mpos-tail-staging-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // A base archive of one `/init` member plus its terminator - the shape
        // the pipeline's output has in miniature.
        let mut base = Vec::new();
        base.extend_from_slice(&ustar_header("./init", 0o755, 4));
        base.extend_from_slice(b"boot");
        base.resize(base.len().div_ceil(512) * 512, 0);
        base.extend_from_slice(&[0u8; 1024]);
        let base_path = dir.join("base.tar");
        fs::write(&base_path, &base).unwrap();

        let program = dir.join("t-one");
        fs::write(&program, b"ELF-bytes").unwrap();
        let staged = [UserTest {
            name: "t-one".to_string(),
            binary: program,
        }];
        let tests: Vec<&UserTest> = staged.iter().collect();

        let out = dir.join("tail.tar");
        append_test_members(&base_path, &out, &tests, &[]).unwrap();

        let listed = std::process::Command::new("tar")
            .args(["-tvf"])
            .arg(&out)
            .output()
            .expect("tar -tvf");
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
        let text = String::from_utf8(listed.stdout).unwrap();
        assert!(
            text.contains("./init.boot"),
            "the moved-aside init lists under its new name: {text}"
        );
        assert!(
            text.contains("./init"),
            "the runner takes the name back: {text}"
        );
        assert!(text.contains("./bin/t-one"), "the staged program: {text}");
        assert_eq!(
            text.matches("./init.boot").count(),
            1,
            "exactly one rename, not a loop over the stream: {text}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A gap whose program the payload does not build is a claim nothing in the
    /// sweep can check: there is no program to leave out, so the entry would sit
    /// in the file looking measured for ever. The rule is exercised on a list
    /// that *has* gaps, because a test must not need the file to be wrong to
    /// run - and on the repository's own file, whose list is one name long today
    /// (`t-arch-prctl`) and may be empty again the day the second machine grows
    /// the call.
    #[test]
    fn a_named_gap_has_to_be_a_program_the_payload_builds() {
        let gaps = [
            (crate::config::GAP_GROUP, "t-gone"),
            (crate::config::GAP_GROUP, "t-here"),
        ];
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
        let config =
            crate::config::TestConfig::load(&root.join(crate::paths::TESTS_CONFIG)).unwrap();
        let machine = crate::config::Machine::Aarch64;
        assert!(
            config
                .group_names(machine)
                .contains(&crate::config::GAP_GROUP),
            "the second machine's gap group has to be there, empty or not"
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
    /// thing that would notice (`NOTES.md` §13.5).
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

    /// The disk-counter boot word is one string with two readers: this file
    /// puts it on a sweep's command line, and `src/board/flow.rs` compares the
    /// command line against its own constant. A drift between them is an
    /// instrument that silently never arms - the sweep boots un-armed,
    /// `t-diskbench` finds no protocol, and its claims go red for a reason
    /// that reads like the kernel's fault. So the pair is pinned here, where
    /// both halves are just text.
    #[test]
    fn the_diskstat_word_matches_the_kernels_constant() {
        let flow = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent directory")
            .join("src/board/flow.rs");
        let text = fs::read_to_string(&flow).expect("the boot flow is readable");
        let declared = text
            .lines()
            .find_map(|line| line.strip_prefix("pub const DISKSTAT_WORD: &str = "))
            .and_then(|rest| rest.trim_end().strip_suffix(';'))
            .expect("flow.rs declares DISKSTAT_WORD as a plain literal");
        assert_eq!(
            declared.trim_matches('"'),
            DISKSTAT_ON.trim(),
            "the sweep's arming word and the boot's reading word diverged"
        );
    }

    /// The fixture's two payload spellings are one guest path, and the rule is
    /// the channels' own: an archive member names itself absolutely (the
    /// ramfs unpacker takes the leading slash as the path), a disk entry names
    /// itself from a root that already exists. Both must land at exactly
    /// `/dev-sda-fs.img` or `t-ext4-mount` opens a name that exists on one
    /// machine and not the other - the same program passing one sweep and
    /// failing its twin for a reason that reads like the kernel's fault.
    ///
    /// The path's own rule is asserted here too: *not* under `/dev`, because
    /// both boots mount a `devtmpfs` over that directory and a payload there is
    /// shadowed (measured - see the constant's comment). This is the cheap half
    /// of that mistake: both programs hard-code the same name, so a harness that
    /// moved the file would fail their opens rather than silently testing a
    /// different one.
    #[test]
    fn both_payload_channels_lay_the_fixture_at_one_path() {
        assert!(
            DISKSTAT_FIXTURE_MEMBER.starts_with("/dev-"),
            "the member is an absolute path at the root, and not under /dev"
        );
        assert!(
            !DISKSTAT_FIXTURE_ENTRY.starts_with('/'),
            "a disk entry must not carry the absolute slash"
        );
        assert_eq!(
            format!("/{DISKSTAT_FIXTURE_ENTRY}"),
            DISKSTAT_FIXTURE_MEMBER,
            "the two spellings drifted apart"
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
