//! `cargo xtask boot-aarch64`: the second machine's boot path, end to end.
//!
//! This is stage 1e's instrument and it is deliberately narrow. The x86_64
//! commands build a root filesystem, an initramfs, a partitioned disk image and
//! a test ESP; the aarch64 machine cannot use any of that yet - nothing above
//! `arch` is ported, so the kernel halts after its own machine checks and never
//! mounts a filesystem (`PLAN.md` §9.1's stage table). What this command does is
//! the smallest thing that proves the *boot path*: build the two binaries, lay
//! out the ESP a UEFI firmware expects, and boot AAVMF with it.
//!
//! What it must not be is a shortcut. The stages 1b-1d bring-up loaded the
//! kernel ELF with QEMU's `-kernel`, which places the image at a fixed address
//! and jumps in with the MMU off - a fine instrument for proving a toolchain and
//! nothing like a boot. This boots the same way x86_64 does: firmware, ESP, a
//! loader that reads the kernel off the volume, builds the page tables and
//! branches into the higher half.
//!
//! The verdict is read out of the serial log, the way `cargo xtask test` reads
//! its own: the guest prints `M4_DEVICES_OK`, `M3_TABLES_OK`, `M5_VIRTIO_OK`,
//! `M1_TIMER_OK`, `M2_CONTEXT_OK` and the `M6_*_OK` family (address space, paging,
//! wall clock, cpu, thread pointer, EL0) when it is happy, and the run stops as
//! soon as one of those - or a failure line - appears. Nothing here claims a green
//! the log does not contain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::utils;

/// The machines this command builds for. The kernel target is the same one the
/// `-kernel` bring-up used; the loader's is stage 1e's addition, and it is the
/// aarch64 twin of `x86_64-unknown-uefi` - a PE32+ EFI application, which is a
/// different object format from the kernel's ELF.
const KERNEL_TARGET: &str = "aarch64-unknown-none";
const LOADER_TARGET: &str = "aarch64-unknown-uefi";

/// The command line the loader copies into the handoff record, and the kernel
/// prints back.
///
/// Nothing parses it at this stage - `boot::console_list` is shared and has no
/// aarch64 caller yet - and it is here for two reasons that are worth the file:
/// the loader reads three files off the volume, and a record whose command line
/// came back empty would not tell a reader whether the file was missing or the
/// read was broken. `ttyAMA0` is what this machine's console is called
/// everywhere else, so the day something does parse it, the name is already
/// right.
const CMDLINE: &str = "console=ttyAMA0\n";

/// How long the boot is given before it is called stuck. Generous: AAVMF's own
/// startup is most of it, and the kernel's checks are milliseconds.
const DEADLINE: Duration = Duration::from_secs(60);

/// The ext4 image `build_esp` lays the aarch64 disk from. `boot-aarch64`'s
/// payload is three small images plus the `rootfs/` tree; the test sweep also
/// carries the musl test programs (~5.6 MB), so it asks for its own size rather
/// than making every boot allocate room for the sweep.
const DISK_BYTES: u64 = 8 * 1024 * 1024;
const SWEEP_DISK_BYTES: u64 = 64 * 1024 * 1024;

/// Lines that mean the machine has finished saying something. The poll below
/// stops on the first of them, which is what keeps a run from waiting out the
/// whole deadline on a machine that has already reported.
///
/// The rule for this list, because getting it wrong is silent and reads as a
/// broken machine: it holds the success marker of the **last** check and the
/// failure marker of **every** check - never the success marker of a check that
/// another check comes after. A terminator stops the run, so a success marker
/// listed too early kills the machine mid-boot and the log simply ends, which is
/// indistinguishable from a hang. Both directions of this mistake have now been
/// made here: the list once ended at `M1_TIMER_OK`, which stage 2's checks come
/// after, and stage 5a briefly added `M4_DEVICES_OK`, which the timer and
/// context checks come after - measured as a boot that took 5.7 s instead of the
/// deadline and stopped three lines into a working machine.
///
/// A failure line belongs here for the same reason a success line does not:
/// `EXCEPTION:` is the kernel's own fault dump, and waiting out the deadline for
/// a marker that will never come is time spent pretending a stopped machine
/// might still be working.
///
/// Stage 5b's check runs *before* the timer and the contexts, so its success
/// marker is not a terminator and its failure marker is - and stage 6a's runs
/// after all of them, so it is the one that now ends the list. Applying the rule
/// is what moved `M2_CONTEXT_OK` out: it is no longer the last line the kernel
/// prints, and leaving it in would kill a machine that was still working.
///
/// Stage 6b's check is in the same position as 5b's - the EL0 check that stage 6a
/// ended the list with still runs after it - so `M6_AS_OK` is deliberately *not*
/// here and `M6_AS_FAILED` is. Stage 6c sits between 6b and 6a's EL0 check for the
/// same reason: `M6_PAGING_OK` is not a terminator and `M6_PAGING_FAILED` is, and so
/// are stages 6d and 6e (`M6_WALL_FAILED`, `M6_CPU_FAILED`). Stage 6h's thread-
/// pointer check lands between 6e's `check_cpu` and 6a's `check_el0`, so
/// `M6_TLS_OK` is not a terminator and `M6_TLS_FAILED` is. Stage 7's serial-receive
/// check lands between 6e's `check_cpu` and 6h's `check_tls`, so `M6_SERIAL_RX_OK` is
/// not a terminator and `M6_SERIAL_RX_FAILED` is. Stage 6E's init handoff
/// runs *after* `check_el0` prints `M6_EL0_OK`, so `M6_INIT_OK` is now the last
/// marker and `M6_EL0_OK` came **out** of the list - leaving it in would stop the
/// poll on a machine that still has init to run. Stage 7c's fix made the handoff
/// actually schedule `/init` (waiting on `is_running`, not `has_live_child`), so the
/// init now runs: its `read(0)`/`write(1)` echo round trip (the `R:echo-test`
/// confirmation) and the `INIT_EXITED` its `exit_group` reaches
/// via the scheduler are the terminal proof, and `INIT_EXITED` (not `M6_INIT_OK`,
/// which now prints before the wait) is the last terminator. Stage 7's virtio-rng
/// check runs in the boot sequence *after* `check_virtio` prints `M5_VIRTIO_OK` but
/// *before* the timer and every later check, so `M7_RNG_OK` is not a terminator and
/// `M7_RNG_FAILED` is - the same rule as the other pre-EL0 checks. Stage 7's block
/// check runs right after the rng one, so `M7_BLK_OK` is not a terminator and
/// `M7_BLK_FAILED` is. Stage 8's net check runs right after the block one, so
/// `M8_NET_OK` is not a terminator and `M8_NET_FAILED` is.
const TERMINATORS: [&str; 24] = [
    "INIT_EXITED",
    "M6_EL0_FAILED",
    "M6_AS_FAILED",
    "M6_PAGING_FAILED",
    "M6_WALL_FAILED",
    "M6_CPU_FAILED",
    "M6_SERIAL_RX_FAILED",
    "M6_CONSOLE_RX_FAILED",
    "M6_TLS_FAILED",
    "M7_RNG_FAILED",
    "M7_BLK_FAILED",
    "M7_FS_FAILED",
    "M7_DISK_FAILED",
    "M7_ROOT_FAILED",
    "M7_DEV_FAILED",
    "M8_NET_FAILED",
    "M8_INPUT_FAILED",
    "M2_CONTEXT_FAILED",
    "M1_TIMER_FAILED",
    "M3_TABLES_FAILED",
    "M4_DEVICES_FAILED",
    "M5_VIRTIO_FAILED",
    "EXCEPTION:",
    "KERNEL PANIC",
];

/// Where the ESP this command assembles lives. Under `build/`, so it is thrown
/// away by `cargo xtask clean` and never confused with the x86_64 image.
fn esp_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-esp")
}

/// Copy `from`'s tree into `to`, making directories as it goes.
///
/// Deliberately small: the one caller is the aarch64 disk's staging directory,
/// which takes the repository's `rootfs/` overlay - a handful of files. A
/// symlink is copied as the file it names (`fs::copy` follows it), which is what
/// laying an overlay onto a filesystem means.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("Failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("Failed to read {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("Failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// The PL011's output, captured to a file for the same reason the x86_64 test
/// machine captures its own: a verdict has to be readable after the machine has
/// stopped, and this one never exits on its own.
fn log_path(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-serial.log")
}

/// The writable copy of the firmware's variable store. AAVMF writes to the
/// second flash bank as it boots, and the system's copy belongs to the package
/// manager.
fn vars_path(build_dir: &Path) -> PathBuf {
    build_dir.join("AAVMF_VARS.fd")
}

pub fn run(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile)?;
    println!("kernel: {} ({:.1} KB)", kernel.display(), kib(&kernel)?);
    println!("loader: {} ({:.1} KB)", loader.display(), kib(&loader)?);

    // The headless verdict's initramfs: `/init` is the exec launcher (echo, then
    // `execve("/t")`), `/t` the fixed-marker target. A green verdict then shows
    // the launcher's `R:echo-test`, the target's `MPOS_EXEC_OK`, and `INIT_EXITED`
    // - proving aarch64 `execve` replaced the running image with a second one that
    // then ran at EL0 (PLAN.md §9.1 stage 6D).
    let launcher = crate::aarch64_init::exec_launcher_init_elf();
    let target = crate::aarch64_init::marker_target_elf();
    // The exec target's parent execve's `/musl`, an *unmodified* statically-linked
    // musl/Rust binary - the project's central promise that such a binary runs. If
    // the aarch64-musl target is not installed this errors with the install command,
    // never a silent skip.
    let musl = crate::aarch64_init::build_static_musl_helper(build_dir)?;
    let esp = build_esp(
        build_dir,
        &kernel,
        &loader,
        &[
            ("init", launcher.as_slice()),
            ("t", target.as_slice()),
            ("musl", musl.as_slice()),
        ],
        DISK_BYTES,
    )?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp, &TERMINATORS)?;
    print!("{log}");

    // Every marker the kernel prints, in the order it prints them:
    // `M4_CONSOLE_OK` says the boot wiring reached the wire through
    // `crate::console` (stage 4a/4b - the console service, not `arch`'s own
    // writer), `M4_DEVICES_OK` says the firmware's device tree reads back as the machine
    // this kernel was built for, `M3_TABLES_OK` says the kernel's own page
    // tables say what the plan says - and that the low half is empty, which is
    // where the device window used to be - `M5_VIRTIO_OK` says those tables
    // reach the machine's virtio transports and that the transports answer as
    // transports, `M7_RNG_OK` says the machine drives its first MMIO device - the
    // tree's virtio-rng window, opened over the memory-mapped transport, returns
    // 32 host-generated bytes, `M7_BLK_OK` says it drives a second one - the
    // tree's virtio-blk window opened as an `MmioTransport` returns sector 0 that
    // matches the pattern the raw image carries and round-trips a written sector, `M8_NET_OK` says it opens a third
    // (the net adapter), reads its non-zero MAC, and hands a frame to its TX
    // virtqueue that the device accepts, `M1_TIMER_OK` says the clock still reaches the
    // vector table through them (a statement about the device window too, because
    // the GIC is reached through it), `M2_CONTEXT_OK` says two tasks can suspend
    // and resume each other, `M6_AS_OK` says a process's address space can be
    // built,
    // walked, protected, shared, copied on write and taken apart again,
    // `M6_PAGING_OK` says the kernel can ask that space the questions a running
    // process needs answered - which root is active, which is the kernel's, is a
    // range touchable, is a page copy-on-write, write a word through a root -
    // `M6_WALL_OK` says the machine has a persistent clock this kernel can reach
    // through the device window, `M6_CPU_OK` says user SIMD is turned on and a
    // line can be masked and an interrupt waited on, `M6_SERIAL_RX_OK` says the
    // console's receive half actually reads (a loopback probe byte echoed through
    // `FR.RXFE`/`DR`), `M6_TLS_OK` says the
    // scheduler's thread-pointer write reaches `TPIDR_EL0` and reads back through
    // the `hal::Cpu` capability, `M6_EL0_OK` says the kernel ran a program at
    // EL0 and answered the call it made, and `M6_INIT_OK` says the scheduler
    // started, the loader's aarch64 `/init` was loaded and run to completion, and
    // its `exit_group` was routed through the shared syscall table and reaped - the
    // first real userspace program on this machine.
    let markers = [
        ("M4_CONSOLE_OK", "console service"),
        ("M4_DEVICES_OK", "devices"),
        ("M3_TABLES_OK", "page tables"),
        ("M5_VIRTIO_OK", "virtio transports"),
        ("M7_RNG_OK", "virtio-rng over MMIO"),
        (
            "M7_BLK_OK",
            "virtio-blk over MMIO (read + write round trip)",
        ),
        ("M7_FS_OK", "ext4 mounted over the MMIO block"),
        (
            "M7_DISK_OK",
            "disk root: /dev/vda mounted at /mnt, a rootfs/ tree file read through the VFS",
        ),
        (
            "M7_ROOT_OK",
            "root switch: / is the disk rootfs, /dev re-mounted on it",
        ),
        ("M7_DEV_OK", "/dev device set builds and reads entropy"),
        (
            "M8_NET_OK",
            "virtio-net over MMIO (TX queue accepted a frame)",
        ),
        (
            "M8_NET_STACK_OK",
            "the smoltcp stack initialized over that same adapter (the boot's net wiring, \
             which `net::init` + `spawn_service` reached)",
        ),
        (
            "M8_INPUT_OK",
            "virtio-input over MMIO negotiated + read device identity",
        ),
        ("M1_TIMER_OK", "timer"),
        ("M2_CONTEXT_OK", "contexts"),
        ("M6_AS_OK", "user address space"),
        ("M6_PAGING_OK", "running-process paging"),
        ("M6_WALL_OK", "wall clock"),
        ("M6_CPU_OK", "cpu and interrupt primitives"),
        ("M6_SERIAL_RX_OK", "serial receive"),
        ("M6_TLS_OK", "thread pointer"),
        ("M6_EL0_OK", "EL0"),
        ("M6_CONSOLE_RX_OK", "console receive path"),
        ("M6_INIT_OK", "init handed to pid 1"),
        ("INIT_EXITED", "init ran and exited via the scheduler"),
    ];
    let missing: Vec<&str> = markers
        .iter()
        .filter(|(marker, _)| !log.contains(marker))
        .map(|(_, what)| *what)
        .collect();
    // The disk's root tree is the repository's `rootfs/` overlay, so the length
    // the kernel reports for `/mnt/etc/inittab` is the length of the file in the
    // tree. A copy that dropped, truncated or replaced that file cannot print the
    // number below, which is what makes the marker a check rather than a claim.
    let inittab = fs::read(crate::utils::project_root()?.join("rootfs/etc/inittab"))?;
    let want = format!("{} bytes", inittab.len());
    if !log.contains(&want) {
        bail!(
            "the aarch64 disk root did not serve the rootfs tree: the log has no \
             \"{want}\" for /mnt/etc/inittab, and the file is {} bytes in rootfs/",
            inittab.len()
        );
    }
    // The init reads a line from `fd 0` (the handoff queued "echo-test\n" on the
    // tty) and echoes it back prefixed with "R:" - so `R:echo-test` on the wire is
    // the init's OWN output (the tty's canonical echo of the same line carries no
    // "R:"). Its presence proves a scheduler-run userspace program read console
    // input and wrote the bytes it read, through `syscall::dispatch`.
    if !log.contains("R:echo-test") {
        bail!(
            "the aarch64 init never echoed the input it read: the R:echo-test \
             line is absent (read(0)/write(1) round trip did not complete)"
        );
    }
    // The launcher's very first act pushes `sp` a page below the mapped stack and
    // stores there; that store only survives if the kernel's aarch64 EL0 fault
    // handler demand-pages it, and `MPOS_FAULT_OK` is written immediately after.
    // Its presence proves user page faults are serviced (the prerequisite for any
    // real, larger program - including a static-musl binary - to run at EL0).
    if !log.contains("MPOS_FAULT_OK") {
        bail!(
            "the aarch64 launcher's demand-paging store did not survive: the \
             MPOS_FAULT_OK line is absent (el0_sync did not take the stack page)"
        );
    }
    // The launcher's last act is `execve("/t", ...)`. `/t` writes `MPOS_EXEC_OK`,
    // which nothing else prints - so its presence proves aarch64 `execve` replaced
    // the running image with a *second* program and that program then ran at EL0
    // through the same trap/dispatch path. `INIT_EXITED` after it is `/t`'s own
    // `exit_group`, reaped from pid 1 (execve keeps the pid).
    if !log.contains("MPOS_EXEC_OK") {
        bail!(
            "the aarch64 launcher never reached its exec target: the \
             MPOS_EXEC_OK line is absent (execve did not transfer to /t)"
        );
    }
    // `/t` then forks (aarch64 has no `fork` syscall; `clone(SIGCHLD)` is the
    // real path). Its child writes `MPOS_FORK_OK` and the parent `wait4`s before
    // exiting, so the child's line is on the wire. Its presence proves aarch64
    // `clone` started a distinct running process that reached EL0 (stage 6D).
    if !log.contains("MPOS_FORK_OK") {
        bail!(
            "the aarch64 exec target never forked a running child: the \
             MPOS_FORK_OK line is absent (clone did not start the child, or \
             the parent did not wait for it)"
        );
    }
    // The target's parent finally execve's `/musl`, an unmodified statically-linked
    // musl/Rust binary whose `println!` writes `MPOS_MUSL_OK`. That it prints proves
    // the kernel runs a REAL binary at EL0 - multi-segment load, the libc/Rust prologue,
    // and the demand-paging of its pages - the central promise, on aarch64.
    if !log.contains("MPOS_MUSL_OK") {
        bail!(
            "the aarch64 target never ran the unmodified musl binary: the \
             MPOS_MUSL_OK line is absent (execve of /musl did not run)"
        );
    }
    // The musl binary then opens the mounted `/dev` through real libc file I/O:
    // `MPOS_DEV_URANDOM_OK` means `open("/dev/urandom")` + `read_exact` returned
    // entropy to a userspace program, and `MPOS_DEV_CONSOLE_OK` means
    // `open("/dev/console")` + `write` reached the console. Together they prove a
    // running userspace program uses the device set the kernel mounts - the same
    // open/read/write a shell does - not just that `/dev` builds or resolves
    // kernel-side. A missing line means the userspace device path is broken.
    if !log.contains("MPOS_DEV_URANDOM_OK") {
        bail!(
            "the aarch64 musl binary could not read /dev/urandom: the \
             MPOS_DEV_URANDOM_OK line is absent (userspace open/read of the mounted /dev failed)"
        );
    }
    if !log.contains("MPOS_DEV_CONSOLE_OK") {
        bail!(
            "the aarch64 musl binary could not write /dev/console: the \
             MPOS_DEV_CONSOLE_OK line is absent (userspace open/write of the mounted /dev failed)"
        );
    }
    // Directory traversal and metadata from userspace over the mounted /dev:
    // `readdir` (getdents64) lists the device nodes and `metadata` (fstat) reads a
    // node's stat - the operations `ls /dev` and `test -e /dev/...` issue. Both
    // must round-trip through libc to the mount table; a missing line means the
    // userspace fs-metadata path is broken.
    if !log.contains("MPOS_DEV_GETDENTS_OK") {
        bail!(
            "the aarch64 musl binary could not list /dev: the \
             MPOS_DEV_GETDENTS_OK line is absent (userspace getdents64 failed)"
        );
    }
    if !log.contains("MPOS_DEV_STAT_OK") {
        bail!(
            "the aarch64 musl binary could not stat /dev/urandom: the \
             MPOS_DEV_STAT_OK line is absent (userspace fstat failed)"
        );
    }
    // A real file lifecycle on the mounted root (create/write/read/stat/unlink) -
    // the fs operations a shell and init issue against the filesystem proper, not
    // just device nodes. getdents64/fstat covered traversal; this covers the
    // write/create/remove path end to end from userspace.
    if !log.contains("MPOS_FILE_OPS_OK") {
        bail!(
            "the aarch64 musl binary could not complete the file lifecycle on the \
             root: MPOS_FILE_OPS_OK is absent (create/write/read/stat/unlink failed)"
        );
    }
    // getcwd + chdir - the syscalls behind `pwd` and `cd`, which a shell needs at
    // startup and on every directory change. The program chdir's into /dev, reads
    // the cwd back, and returns; MPOS_CWD_OK asserts the round trip held.
    if !log.contains("MPOS_CWD_OK") {
        bail!(
            "the aarch64 musl binary could not round-trip getcwd/chdir: the \
             MPOS_CWD_OK line is absent (chdir into /dev and back, or getcwd, failed)"
        );
    }
    // symlink + readlink + unlink of a symlink - SYMLINKAT/READLINK, the pair
    // `ln -s` and the resolution a loader or shell tab-completion performs.
    if !log.contains("MPOS_SYMLINK_OK") {
        bail!(
            "the aarch64 musl binary could not round-trip symlink/readlink: the \
             MPOS_SYMLINK_OK line is absent (symlinkat/readlink/unlink failed)"
        );
    }
    // mkdir + a file inside the new directory + rmdir - MKDIRAT/RMDIR, the
    // directory half of the filesystem an init uses to set up /tmp, /var. The
    // nested write/read proves the directory is traversable by name, and the
    // rmdir only succeeding after the file is gone proves the empty-directory
    // gate (ENOTEMPTY on a full one) held on this machine.
    if !log.contains("MPOS_MKDIR_OK") {
        bail!(
            "the aarch64 musl binary could not round-trip mkdir/nested file/rmdir: \
             the MPOS_MKDIR_OK line is absent (mkdirat, the nested write/read, \
             unlink or rmdir failed)"
        );
    }
    // rename + hard link - RENAMEAT/LINKAT, the `mv`/`ln` pair. The marker only
    // prints if rename moved the body to the new name (and dropped the old) and
    // a hard link kept the bytes reachable through the second name after the
    // first was removed - so it pins that linkat shares a node, not that it
    // copies one.
    if !log.contains("MPOS_RENAME_LINK_OK") {
        bail!(
            "the aarch64 musl binary could not round-trip rename/hard link: the \
             MPOS_RENAME_LINK_OK line is absent (renameat, linkat or unlink failed)"
        );
    }
    // ftruncate - the `: >file` / `truncate` length handler. The marker prints
    // only if a 10-byte file trimmed to 4 then 0 reports those sizes back, so a
    // truncate that left stale bytes or a stale length fails.
    if !log.contains("MPOS_TRUNCATE_OK") {
        bail!(
            "the aarch64 musl binary could not round-trip ftruncate: the \
             MPOS_TRUNCATE_OK line is absent (set_len / the stat length it implies failed)"
        );
    }
    // 1 MiB heap buffer - past musl's malloc threshold this is an `mmap`, so the
    // marker proves the anonymous/mmap demand-paging path works from a live
    // process (a memory handler, not only fs). It is also the counter-evidence
    // that the spawn child-stack fault is *not* a general mmap/VMA breakage.
    if !log.contains("MPOS_MEMORY_OK") {
        bail!(
            "the aarch64 musl binary could not exercise a 1 MiB mmap-backed heap \
             buffer: the MPOS_MEMORY_OK line is absent (the mmap/anon demand-paging \
             path a large allocation reaches failed)"
        );
    }
    // fd duplication via fcntl(F_DUPFD_CLOEXEC) - `File::try_clone`, the
    // redirection/pipe primitive a shell uses to wire stdin/stdout. The duped
    // descriptor must read back the file's bytes; an invalid or independent dup
    // fails here.
    if !log.contains("MPOS_DUP_OK") {
        bail!(
            "the aarch64 musl binary could not duplicate a file descriptor: the \
             MPOS_DUP_OK line is absent (fcntl F_DUPFD_CLOEXEC / the read through \
             the duped fd failed)"
        );
    }
    // aarch64 struct-stat ABI: chmod's mode reads back through st_mode, a regular
    // file reports is_file/len, a directory reports is_dir, a symlink reports
    // is_symlink (S_IFLNK via lstat), and st_nlink/st_ino/st_blocks/st_size read
    // at the aarch64 offsets. This pins that the kernel lays out `struct stat` at
    // the aarch64 (asm-generic) field offsets, not x86_64's - the earlier mismatch
    // made st_mode return st_nlink's value (1), so every stat-based type/permission
    // read was wrong.
    if !log.contains("MPOS_STAT_OK") {
        bail!(
            "the aarch64 musl binary could not read stat fields back correctly: the \
             MPOS_STAT_OK line is absent (st_mode/is_file/len/is_dir at the aarch64 \
             struct-stat offsets failed)"
        );
    }
    if missing.is_empty() {
        println!();
        println!(
            "aarch64 boot: M4_CONSOLE_OK, M4_DEVICES_OK, M3_TABLES_OK, M5_VIRTIO_OK, \
             M7_RNG_OK, M7_BLK_OK, M7_FS_OK, M7_DEV_OK, M8_NET_OK, M8_NET_STACK_OK, M8_INPUT_OK, M1_TIMER_OK, M2_CONTEXT_OK, \
             M6_AS_OK, \
             M6_PAGING_OK, M6_WALL_OK, M6_CPU_OK, \
             M6_SERIAL_RX_OK, M6_TLS_OK, M6_EL0_OK, M6_CONSOLE_RX_OK, M6_INIT_OK, the \
             demand-paged stack store (MPOS_FAULT_OK), the init's echoed input \
             (R:echo-test), the exec target (MPOS_EXEC_OK), its fork child (MPOS_FORK_OK), \
             the unmodified musl binary (MPOS_MUSL_OK), and INIT_EXITED - the machine \
             drives three devices over the memory-mapped virtio transport (rng, blk with a \
             verified sector read and a written sector round-tripped, and net whose TX virtqueue accepted a frame over MMIO \
             and which the smoltcp stack is then initialized over, the same \
             net::init + spawn_service pair the first machine boots with) and mounts an ext4 filesystem \
             over the block (the disk root), and a scheduler-run program that demand-paged, \
             read, echoed, execve'd, cloned, waited, then ran a real static-musl ELF at EL0 \
             which opened the mounted /dev through libc file I/O (MPOS_DEV_URANDOM_OK, \
             MPOS_DEV_CONSOLE_OK), traversed it (MPOS_DEV_GETDENTS_OK, MPOS_DEV_STAT_OK), \
             and ran a full file lifecycle on the root (MPOS_FILE_OPS_OK) and \
             round-tripped getcwd/chdir (MPOS_CWD_OK) and symlink/readlink \
             (MPOS_SYMLINK_OK) and mkdir/a nested file/rmdir (MPOS_MKDIR_OK) and \
             rename/hard link (MPOS_RENAME_LINK_OK) and ftruncate (MPOS_TRUNCATE_OK) \
             and a 1 MiB mmap-backed heap buffer (MPOS_MEMORY_OK) and fd \
             duplication via fcntl (MPOS_DUP_OK) and the aarch64 struct-stat layout \
             (MPOS_STAT_OK) - \
             from the ESP under AAVMF"
        );
        return Ok(());
    }
    bail!(
        "the aarch64 boot did not reach every marker; nothing printed for: {}; \
         the log above and {} say where it stopped",
        missing.join(", "),
        log_path(build_dir).display()
    )
}

/// The aarch64 arm of `cargo xtask test`: boot the second machine with the musl
/// user test programs on its disk and a runner as `/init`, and read the same
/// `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol the x86 sweep reads.
///
/// The machine's own checks still run first - this is a normal aarch64 boot, so
/// `M7_DISK_OK`/`M7_ROOT_OK` and the rest print before `/init` does - but the
/// verdict here is the user programs', not the marker chain's: those are
/// `boot-aarch64`'s own instrument. `=== END` is what ends the poll.
///
/// Which programs *are* the payload is `tests.toml`'s answer, read for this
/// machine (`Machine::Aarch64`): the file's `aarch64` group is this machine's own
/// list of what it cannot serve yet, and it is the only thing that can leave a
/// program out - an ungrouped test, or one left out for a machine-independent
/// reason (`external`), is staged and has to pass.
pub fn run_sweep(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile)?;
    let init = crate::aarch64_init::build_sweep_init(build_dir)?;
    let built = build_user_tests_for_aarch64()?;
    println!("user tests: {} aarch64 programs built", built.len());

    // The check below is what keeps the naming honest. A gap has to be a program
    // this machine was asked about and could not serve; a name in its group that
    // this payload never builds is a claim about the ABI that nothing in the
    // sweep can check - the x86_64 arm stages the name (it builds every
    // program), and this one has nothing to leave out - so it would sit in the
    // file looking like a measured gap for ever.
    let config = crate::config::TestConfig::load(&utils::project_root()?.join("tests.toml"))?;
    let machine = crate::config::Machine::Aarch64;
    let built_names: Vec<String> = built.iter().map(|(name, _)| name.clone()).collect();
    let unbuilt = gaps_without_programs(&config, machine, &built_names);
    if !unbuilt.is_empty() {
        anyhow::bail!(
            "tests.toml names {}, as gaps for this machine, which its payload \
             does not build: a gap nobody can check is not a gap",
            unbuilt.join(", ")
        );
    }

    let selected = config.selection(&[], false);
    let tests: Vec<(String, Vec<u8>)> = built
        .iter()
        .filter(|(name, _)| config.runs(name, &selected, machine))
        .cloned()
        .collect();
    let skipped: Vec<&(String, Vec<u8>)> = built
        .iter()
        .filter(|(name, _)| !config.runs(name, &selected, machine))
        .collect();
    println!(
        "user tests: {} staged, {} left out by tests.toml",
        tests.len(),
        skipped.len()
    );
    // Group by group, like the x86_64 arm's report and for the same reason: the
    // group is the reason, and a reader has to be able to see at a glance that
    // what this machine left out are the things it is supposed to be missing.
    for group in config.group_names(machine) {
        let left: Vec<&str> = skipped
            .iter()
            .filter(|(name, _)| config.groups_of(name, machine).contains(&group))
            .map(|(name, _)| name.as_str())
            .collect();
        if !left.is_empty() {
            println!("  {group:11} {}", left.join(", "));
        }
    }
    println!();

    // The runner is `/init` on the disk (which is what the boot reads and spawns
    // as pid 1) *and* in the initramfs, and the programs sit beside it. One
    // payload, both places: the ramfs is where they are before the root switch
    // and the ext4 is where they are after it, and the same list is a correct
    // answer on either side of that.
    let mut entries: Vec<(&str, &[u8])> = vec![("init", init.as_slice())];
    for (name, bytes) in &tests {
        entries.push((name.as_str(), bytes.as_slice()));
    }
    let esp = build_esp(build_dir, &kernel, &loader, &entries, SWEEP_DISK_BYTES)?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp, &["=== END"])?;
    print!("{log}");
    let staged: Vec<&str> = tests.iter().map(|(name, _)| name.as_str()).collect();
    report_sweep(build_dir, &staged, skipped.len(), &log)
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
    config
        .machine_tests(machine)
        .into_iter()
        .filter(|(_, test)| !built.iter().any(|name| name == test))
        .map(|(_, test)| test.to_string())
        .collect()
}

/// The second machine's user test programs, built by cargo for
/// `aarch64-unknown-linux-musl` and read back as bytes: this payload is an
/// initramfs and a disk laid out by name, not a tar the guest unpacks, so the
/// files cross as data rather than as paths.
///
/// Zero programs is an error, not an empty green run - a sweep that quietly runs
/// fewer tests than asked is the one failure this must not have.
fn build_user_tests_for_aarch64() -> Result<Vec<(String, Vec<u8>)>> {
    let root = utils::project_root()?;
    println!("User tests: the std-side suite for aarch64 (mpos-user-tests)");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--release",
        "-p",
        "mpos-user-tests",
        "--target",
        "aarch64-unknown-linux-musl",
    ])
    .current_dir(&root);
    utils::run_status(&mut cmd, "the aarch64 std-side user tests")?;

    let dir = utils::target_dir()?.join("aarch64-unknown-linux-musl/release");
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
        anyhow::bail!(
            "no aarch64 test program was built under {} - the sweep would have \
             reported a green zero",
            dir.display()
        );
    }
    Ok(tests)
}

/// Read the sweep's verdict out of the log.
///
/// Three facts have to agree, and each is a different way for the run to be
/// less than it looks: every program that was staged has to appear (a log that
/// stops early still contains the passes it got through), no program may have
/// failed, and the runner's own summary line has to be there - the same shape
/// the x86 report checks, because both are claims about a machine someone has to
/// be able to quote.
///
/// `staged` is the programs this run asked the machine about and `left_out` how
/// many `tests.toml` named instead - the latter counted, never hidden: a green
/// here means "every program this machine was supposed to be able to run passed",
/// and the names and reasons of the ones it was not are in that file, printed by
/// the caller above.
fn report_sweep(build_dir: &Path, staged: &[&str], left_out: usize, log: &str) -> Result<()> {
    // The runner sets the machine up before it runs anything (it makes `/tmp`),
    // and a setup that failed makes every "failure" below a statement about the
    // harness rather than the kernel. So this is checked first and refuses the
    // run outright - the one line here that is not about the programs.
    if let Some(line) = log.lines().find(|line| line.starts_with("=== SETUP_FAIL")) {
        anyhow::bail!(
            "the aarch64 sweep's runner could not set the machine up: {line}; \
             nothing the programs did is evidence about the kernel until that \
             works"
        );
    }
    let mut passed: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for line in log.lines() {
        let line = line.trim_end();
        if let Some(name) = line.strip_prefix("=== PASS ") {
            passed.push(name);
        } else if let Some(name) = line.strip_prefix("=== FAIL ") {
            failed.push(name);
        }
    }
    let summary = log.lines().find(|line| line.starts_with("=== SUMMARY"));

    println!();
    let Some(summary) = summary else {
        anyhow::bail!(
            "the aarch64 sweep never printed a summary: the run did not reach the \
             end of the {} staged program(s); {} says how far it got",
            staged.len(),
            log_path(build_dir).display()
        );
    };
    println!("self-test (aarch64): {summary}");

    if !failed.is_empty() {
        let mut names: Vec<&str> = failed
            .iter()
            .map(|name| name.split_whitespace().next().unwrap_or(name))
            .collect();
        names.sort_unstable();
        for name in &names {
            println!("  FAILED {name}");
        }
        anyhow::bail!(
            "self-test (aarch64) failed: {} of {} programs did not pass: {}",
            names.len(),
            staged.len(),
            names.join(", ")
        );
    }
    if passed.len() != staged.len() {
        anyhow::bail!(
            "the aarch64 sweep staged {} program(s) but the log carries {} verdict(s): \
             the payload and the run disagree, which is not a partial pass",
            staged.len(),
            passed.len()
        );
    }
    println!(
        "self-test (aarch64): all {} staged user tests passed ({} left out by \
         tests.toml - the groups above name every one of them, with the reason \
         in that file)",
        staged.len(),
        left_out
    );
    Ok(())
}

/// Build the kernel and its loader for this machine.
fn build(profile: &str) -> Result<(PathBuf, PathBuf)> {
    let root = utils::project_root()?;
    let release = profile == "release";

    let mut kernel = Command::new("cargo");
    kernel
        .arg("build")
        .args(["-p", "mpos"])
        .args(["--target", KERNEL_TARGET]);
    if release {
        kernel.arg("--release");
    }
    run_cargo(kernel, &root, "the aarch64 kernel")?;

    // The loader's target is passed here rather than left to
    // `boot/.cargo/config.toml`, whose `[build] target` names the x86_64 one:
    // a config file cannot know which machine this invocation is for, and
    // guessing there would break the plain `cargo build` inside `boot/`.
    let mut loader = Command::new("cargo");
    loader
        .arg("build")
        .args(["-p", "mpos-boot"])
        .args(["--target", LOADER_TARGET]);
    if release {
        loader.arg("--release");
    }
    run_cargo(loader, &root, "the aarch64 loader")?;

    let target = utils::target_dir()?;
    Ok((
        target.join(KERNEL_TARGET).join(profile).join("mpos"),
        target
            .join(LOADER_TARGET)
            .join(profile)
            .join("mpos-boot.efi"),
    ))
}

fn run_cargo(mut command: Command, root: &Path, what: &str) -> Result<()> {
    let status = command
        .current_dir(root)
        .status()
        .with_context(|| format!("Failed to run cargo for {what}"))?;
    if !status.success() {
        bail!("{what} did not build");
    }
    Ok(())
}

/// The volume a UEFI firmware reads: the loader where the firmware looks for it
/// by default, and the three files the loader expects beside it.
///
/// `\\EFI\\BOOT\\BOOTAA64.EFI` is the aarch64 name in the removable-media
/// fallback path, the twin of the `BOOTX64.EFI` the x86_64 image carries. The
/// three root files are the loader's `KERNEL_PATH`, `INITRAMFS_PATH` and
/// `CMDLINE_PATH`, and they are at the volume's root rather than under `EFI/`
/// because that is where `boot/src/main.rs` reads them.
fn build_esp(
    build_dir: &Path,
    kernel: &Path,
    loader: &Path,
    entries: &[(&str, &[u8])],
    disk_bytes: u64,
) -> Result<PathBuf> {
    let esp = esp_dir(build_dir);
    if esp.exists() {
        fs::remove_dir_all(&esp).with_context(|| format!("Failed to clear {}", esp.display()))?;
    }
    let boot = esp.join("EFI/BOOT");
    fs::create_dir_all(&boot).with_context(|| format!("Failed to create {}", boot.display()))?;

    fs::copy(loader, boot.join("BOOTAA64.EFI"))
        .with_context(|| format!("Failed to install {}", loader.display()))?;
    fs::copy(kernel, esp.join("mpos.elf"))
        .with_context(|| format!("Failed to install {}", kernel.display()))?;
    fs::write(esp.join("mpos.cmdline"), CMDLINE)?;

    // The third file is a real aarch64 initramfs - which programs it holds is the
    // caller's choice, passed as `entries` (the headless verdict packs the exec
    // launcher as `init` plus its target as `t`, so the run reaches a *second*
    // image via `execve`; the interactive `run` packs the looping REPL as `init`
    // alone). `aarch64_init` hand-encodes each static `EM_AARCH64` `ET_EXEC` and
    // tar writes them ustar. The loader copies the archive into frames and records
    // its address and length in the handoff, so this proves a file crosses the
    // firmware's protocols and lands in the direct map. The kernel unpacks it into
    // a ramfs and runs `/init` (PLAN.md §9.1 stages 6D/6E/7d/7e).
    let initramfs = build_dir.join("aarch64-initramfs.tar");
    crate::aarch64_init::write_initramfs_entries(entries, build_dir, &initramfs)?;
    fs::copy(&initramfs, esp.join("initramfs.tar"))
        .with_context(|| format!("Failed to install {}", initramfs.display()))?;
    println!(
        "initramfs: aarch64 {} -> {}",
        names_of(entries),
        initramfs.display()
    );

    // A raw block image, attached below as a virtio-blk **MMIO** transport so
    // `check_blk` can read sector 0 and *verify* it against a known pattern - the
    // drivers stage's second driven device (`PLAN.md` §9.1). Every byte is
    // `i as u8`, so the first 512 bytes read back exactly as the check expects.
    // It is read, never mounted - the filesystem-over-block step comes later.
    let blk_img = build_dir.join("aarch64-blk.img");
    let pattern: Vec<u8> = (0..1024 * 1024u32).map(|i| i as u8).collect();
    fs::write(&blk_img, &pattern)
        .with_context(|| format!("Failed to write {}", blk_img.display()))?;

    // A second block image, an ext4 filesystem carrying one file, built WITHOUT a
    // mount: `mkfs.ext4 -d <dir>` lays the filesystem down from a directory at
    // create time, so `boot-aarch64` still runs unprivileged (e2fsprogs is now a
    // prereq of this command). The in-tree ext4 driver refuses `has_journal` (no
    // journal replay lives here) and `metadata_csum` (writes would leave a checksum
    // it cannot recompute) - the same two features the x86_64 root switches off - so
    // `check_fs` can mount this over the memory-mapped block and prove the
    // block-driver -> filesystem path on the second machine (`PLAN.md` §9.1).
    let staging = build_dir.join("aarch64-ext4-staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .with_context(|| format!("Failed to create {}", staging.display()))?;
    fs::write(
        staging.join("hello"),
        b"mpos reads from ext4 over virtio-MMIO\n",
    )?;
    // The disk carries a *root tree*, not one test file. Two things go in beside
    // `hello`: the three member images the initramfs also carries (so the disk
    // has an init to hand pid 1 when the root switch lands), and the in-tree
    // `rootfs/` overlay - the files `rootfs.toml`'s packages are layered under
    // (AGENTS.md §5). Copying the tree rather than naming files keeps the image a
    // function of the repository: the kernel's `check_disk_root` reads
    // `/mnt/etc/inittab` out of it and reports the length, which this command
    // checks against the file in the tree.
    for &(name, bytes) in entries {
        fs::write(staging.join(name), bytes)
            .with_context(|| format!("Failed to lay {name} on the aarch64 disk"))?;
    }
    copy_tree(&crate::utils::project_root()?.join("rootfs"), &staging)?;
    let ext4 = build_dir.join("aarch64-ext4.img");
    let file =
        fs::File::create(&ext4).with_context(|| format!("Failed to create {}", ext4.display()))?;
    file.set_len(disk_bytes)?;
    drop(file);
    utils::run_status(
        Command::new("mkfs.ext4")
            .args(["-F", "-q", "-d"])
            .arg(&staging)
            .args(["-O", "^has_journal,^metadata_csum"])
            .args(["-b", "4096"])
            .arg(&ext4),
        "mkfs.ext4 the aarch64 filesystem image",
    )?;

    Ok(esp)
}

/// The ramfs names in `entries`, for the log line (so a reader sees which files
/// the archive holds, not just that it was built).
fn names_of(entries: &[(&str, &[u8])]) -> String {
    let parts: Vec<String> = entries.iter().map(|(name, _)| format!("/{name}")).collect();
    parts.join(" + ")
}

/// The firmware's two flash images, from whichever package the host has.
///
/// AAVMF is EDK2 built for `virt`. Distributions put it in one of a handful of
/// places and name the variable store differently from the code, so both are
/// looked up together rather than guessed from one path.
fn firmware() -> Result<(PathBuf, PathBuf)> {
    const PLACES: [(&str, &str); 3] = [
        (
            "/usr/share/AAVMF/AAVMF_CODE.fd",
            "/usr/share/AAVMF/AAVMF_VARS.fd",
        ),
        (
            "/usr/share/edk2/aarch64/QEMU_EFI.fd",
            "/usr/share/edk2/aarch64/QEMU_VARS.fd",
        ),
        (
            "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
            "/usr/share/qemu-efi-aarch64/QEMU_VARS.fd",
        ),
    ];
    for (code, vars) in PLACES {
        let (code, vars) = (Path::new(code), Path::new(vars));
        if code.exists() && vars.exists() {
            return Ok((code.to_path_buf(), vars.to_path_buf()));
        }
    }
    bail!(
        "No aarch64 UEFI firmware found; looked for AAVMF_CODE.fd + AAVMF_VARS.fd in \
         /usr/share/AAVMF, /usr/share/edk2/aarch64 and /usr/share/qemu-efi-aarch64"
    )
}

/// The full `qemu-system-aarch64` command for this machine, with the serial line
/// sent wherever `serial` says (`file:…` for the marker-polling boot, `mon:stdio`
/// for an interactive `run`). Every flag here is a measured machine choice, so
/// boot-aarch64 and `run --arch aarch64` share one definition rather than two
/// lists that could drift into booting different machines.
fn aarch64_qemu(esp: &Path, code: &Path, vars: &Path, serial: &str) -> Command {
    let mut qemu = Command::new("qemu-system-aarch64");
    // `acpi=off` is what makes this firmware describe the machine the way this
    // port reads it, and it is a measured difference rather than a preference:
    // with ACPI left on - the default - AAVMF advertises ACPI 2.0 in its
    // configuration table and publishes **no device tree at all**, which is
    // `PLAN.md` §9.1's assumption corrected (it said `virt` + AAVMF passes a
    // tree). A tree and a set of ACPI tables are two ways to say the same
    // things; this port reads the first, because that is the one an embedded
    // machine has, and the second is named as a gap rather than half-read.
    qemu.args(["-machine", "virt,gic-version=2,acpi=off"])
        // A named CPU rather than `max`, because the kernel's interrupt
        // controller is a GICv2 and its clock is the EL1 physical timer: both
        // are things a `max` CPU is free to configure differently, and a
        // difference there reads as a broken driver.
        .args(["-cpu", "cortex-a57"])
        // TCG and never KVM, on any host: `detect_accel` answers with the
        // *host's* accelerator, and a host that is not aarch64 cannot run this
        // machine on it. The x86_64 commands are welcome to it.
        .args(["-accel", "tcg"])
        .args(["-m", "256"])
        .args(["-display", "none"])
        .args(["-monitor", "none"])
        .args(["-no-reboot"])
        .args(["-serial", serial])
        // Two flash banks, the way EDK2 expects to find itself: the code bank
        // read-only at the address the machine boots from, and a writable
        // variable store the firmware updates as it goes.
        .args([
            "-drive",
            &format!(
                "if=pflash,format=raw,unit=0,readonly=on,file={}",
                code.display()
            ),
        ])
        .args([
            "-drive",
            &format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
        ])
        // The ESP itself, as a bare FAT volume on a virtio disk rather than a
        // partitioned image: EDK2's partition driver hands a volume with no
        // partition table to the FAT driver as a whole, which is how a USB
        // stick with no MBR boots. Read-only, because the loader only reads.
        .args([
            "-drive",
            &format!(
                "file=fat:{},format=raw,if=none,id=hd0,readonly=on",
                esp.display()
            ),
        ])
        .args(["-device", "virtio-blk-pci,drive=hd0"])
        // One device on a virtio-**MMIO** transport, which is what stage 5b's
        // check reads back. `virt` publishes its thirty-two `virtio,mmio` nodes
        // whether or not anything is behind them (measured: the decompiled tree
        // with and without this flag differs only in the firmware's own random
        // seeds), so without a device every window answers `DeviceID` 0 and the
        // check could only say "these look like transports". With one attached,
        // it reads a `DeviceID` the machine did not have to be told and names it.
        //
        // The MMIO variant on purpose, and not `-pci`: the ESP's own disk is a
        // `virtio-blk-pci` behind the PCIe bridge, which is a *different*
        // discovery path (configuration space, not a fixed window), and a check
        // that a PCI device answered would not say anything about the window the
        // tree describes.
        .args(["-device", "virtio-rng-device"])
        // A second MMIO device, a block one, on a raw image the ESP step lays
        // down. This is what `check_blk` drives: it opens the window whose
        // `DeviceID` is 2 and reads sector 0 back against the pattern the image
        // carries, so the marker proves a verified MMIO block read - the drivers
        // stage's second device, not a mounted root yet. It sits on the same
        // fixed-window MMIO path the rng does, and the `-global` below makes it
        // the modern (version 2) layout the crate reads.
        .args([
            "-drive",
            &format!(
                "file={},format=raw,if=none,id=blk0",
                esp.parent().unwrap().join("aarch64-blk.img").display()
            ),
        ])
        .args(["-device", "virtio-blk-device,drive=blk0"])
        // The second block device: the ext4 image `build_esp` laid down. `check_fs`
        // opens the block windows, tries each as an ext4, and finds this one - so a
        // filesystem mounts over the memory-mapped transport (PLAN.md §9.1's disk
        // root). Two virtio-blk-device instances, so the port picks the right one by
        // content rather than by the slot QEMU happened to give it.
        .args([
            "-drive",
            &format!(
                "file={},format=raw,if=none,id=blk1",
                esp.parent().unwrap().join("aarch64-ext4.img").display()
            ),
        ])
        .args(["-device", "virtio-blk-device,drive=blk1"])
        // A third MMIO device, a network adapter on a user-mode (SLIRP) backend,
        // so `check_net` can open it the same way and read its MAC - the net
        // stage's transport proof on this machine (`PLAN.md` §9.1). No socket
        // leaves the machine; the check reads config, it does not send traffic.
        .args(["-netdev", "user,id=n0"])
        .args(["-device", "virtio-net-device,netdev=n0"])
        // A virtio-input device too - the fourth class the aarch64 MMIO virtio path
        // has to reach (`check_input`, `M8_INPUT_OK`). The keyboard backend is chosen
        // because it is a self-contained virtio-input device: it presents its event
        // queue and config with no external chardev/socket, so the boot can open and
        // negotiate the transport headless (the milestone is the transport reaching
        // the class, not any event arriving).
        .args(["-device", "virtio-keyboard-device"])
        // And the transports have to be the *modern* ones, which on this machine
        // is a choice somebody has to make: measured, `virt`'s `virtio-mmio`
        // transports answer `Version` 1 - the legacy register layout - unless
        // this says otherwise, and version 2 is the only layout this kernel's
        // driver crate reads. The alternative to selecting it is a kernel that
        // either drives the legacy layout (a second register map, a second
        // feature-negotiation order) or refuses devices on its own machine, and
        // neither is what stage 5 is for.
        .args(["-global", "virtio-mmio.force-legacy=false"]);
    qemu
}

/// Boot the aarch64 machine and hand its serial line to the terminal, staying
/// attached until the guest exits or the user detaches. This is the interactive
/// twin of `boot` (which captures the PL011 to a file and stops at a marker):
/// same ESP, same firmware, same `qemu-system-aarch64`, only the serial goes to
/// stdio and no kill follows. The machine currently boots to a one-shot `/init`
/// that halts after `exit_group`; a persistent interactive shell needs the
/// drivers stage (serial RX + the idle/park path, `PLAN.md` §9.1 stage 7).
pub fn run_interactive(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile)?;
    let loop_init = crate::aarch64_init::looping_init_elf();
    let esp = build_esp(
        build_dir,
        &kernel,
        &loader,
        &[("init", loop_init.as_slice())],
        DISK_BYTES,
    )?;
    let (code, vars_src) = firmware()?;
    let vars = vars_path(build_dir);
    fs::copy(&vars_src, &vars).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            vars_src.display(),
            vars.display()
        )
    })?;

    println!(
        "booting aarch64 interactively: a read/echo loop on the serial console \
         (type a line, it echoes back prefixed R:; Ctrl-A x to exit QEMU)"
    );
    let mut qemu = aarch64_qemu(&esp, &code, &vars, "mon:stdio");
    let status = qemu
        .status()
        .context("Failed to start qemu-system-aarch64")?;
    if !status.success() {
        eprintln!("qemu-system-aarch64 exited {status}");
    }
    Ok(())
}

/// Boot the ESP under AAVMF and return what the serial line said.
///
/// `terminators` is what ends the poll: each of `boot-aarch64`'s `M*_OK` checks
/// prints a marker the kernel's own handoff produces, while the test sweep's
/// runner prints the harness protocol's `=== END` instead - the two boots end on
/// different lines, so which one stops the read is the caller's to say.
fn boot(build_dir: &Path, esp: &Path, terminators: &[&str]) -> Result<String> {
    let (code, vars_src) = firmware()?;
    let vars = vars_path(build_dir);
    fs::copy(&vars_src, &vars).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            vars_src.display(),
            vars.display()
        )
    })?;

    let log = log_path(build_dir);
    let _ = fs::remove_file(&log);

    let mut qemu = aarch64_qemu(esp, &code, &vars, &format!("file:{}", log.display()));
    qemu.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = qemu
        .spawn()
        .context("Failed to start qemu-system-aarch64")?;

    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(&log)
            && terminators.iter().any(|marker| text.contains(marker))
        {
            break;
        }
        if let Some(status) = child.try_wait()? {
            eprintln!("qemu-system-aarch64 exited on its own: {status}");
            break;
        }
        if started.elapsed() > DEADLINE {
            eprintln!("qemu-system-aarch64 was still running after {DEADLINE:?}; stopping it");
            break;
        }
        sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    fs::read_to_string(&log).with_context(|| format!("Failed to read {}", log.display()))
}

fn kib(path: &Path) -> Result<f64> {
    Ok(fs::metadata(path)?.len() as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Machine, TestConfig};

    /// The gap check, against the repository's own file.
    ///
    /// The group is the second machine's list of what it cannot serve, and the
    /// one way it rots quietly is a name the payload never builds: nothing in
    /// the sweep would notice, because there is no program to leave out, and the
    /// entry would read as a measured gap for ever. So a real gap passes and a
    /// gap whose program is missing is named.
    #[test]
    fn a_named_gap_has_to_be_a_program_the_payload_builds() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let config = TestConfig::load(&root.join("tests.toml")).unwrap();
        let machine = Machine::Aarch64;

        let gaps: Vec<String> = config
            .machine_tests(machine)
            .into_iter()
            .map(|(_, test)| test.to_string())
            .collect();
        assert!(!gaps.is_empty(), "the aarch64 group names nothing");
        // The payload the arm really builds is exactly these plus the ones that
        // run, so every name is present and the check is silent.
        assert!(gaps_without_programs(&config, machine, &gaps).is_empty());

        // And the shape a stale entry takes: the file still names one, the
        // payload no longer carries it. The member is taken from the file rather
        // than spelled here, so this pin does not need editing when the group
        // shrinks - which is the direction it is supposed to move.
        let last = gaps.len() - 1;
        let missing_one: Vec<String> = gaps[..last].to_vec();
        assert_eq!(
            gaps_without_programs(&config, machine, &missing_one),
            vec![gaps[last].clone()]
        );
    }
}
