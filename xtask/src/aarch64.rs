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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::arch::Arch;
use crate::utils;

/// The command line the loader copies into the handoff record, and the kernel
/// prints back.
///
/// Two readers now, and both are real. The kernel records it for
/// `/proc/cmdline` (an init reads `root=` through that file), and the handoff
/// reads `mpos.selftest` back out of it to decide whether this boot is a test -
/// the word that lets it inject the console line its read/echo pin needs, and
/// the word a real userland boot must not carry (see [`ALPINE_CMDLINE`]).
/// `ttyAMA0` is what this machine's console is called everywhere else, and the
/// day the shared `boot::console_list` is parsed here, the name is already right.
const CMDLINE: &str = "console=ttyAMA0 mpos.selftest root=/dev/vda rootfstype=ext4\n";

/// The command line the *Alpine* boot carries: the machine's consoles, and
/// nothing else.
///
/// The difference from [`CMDLINE`] is one word of policy and one of shape.
/// `mpos.selftest` is what the kernel's aarch64 handoff reads to decide whether
/// it may inject a line of console input before starting `/init` - an instrument
/// the verdict and sweep boots need and a real userland must never see. An
/// Alpine boot's getty reads the same tty; a boot that injected `echo-test`
/// there would hand it to the login prompt as a username. So this boot keeps
/// the console untouched by the test half.
///
/// The console *list* is the x86_64 path's own answer, read from the same
/// sources (`machine.toml`'s `[qemu] mode`, or `MPOS_XTASK_MODE` when
/// `run --mode X` hands the desired mode down): `RunMode::cmdline_consoles`
/// spells `console=ttyAMA0 [console=tty0]` for terminal/mirror/gui, and since
/// §13.16 the vcmux carries each virtual console's bytes over the wire on this
/// machine too, so the list means exactly what it means on x86_64. The headless
/// verdicts have no mode to honour - they are not sessions - and pass
/// [`ModeChoice::Verdicts`], which keeps the single serial word they always had.
const ALPINE_CMDLINE: &str = "console=ttyAMA0 root=/dev/vda rootfstype=ext4\n";

/// Which consoles an Alpine-layout boot should carry: the session's own mode,
/// or the verdict's fixed serial line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeChoice {
    /// `alpine-aarch64`: the scripted gate. No window exists to prefer, and its
    /// `login:` verdict must not depend on what `machine.toml` happens to say.
    Verdicts,
    /// `run --arch aarch64`: the interactive session, booted with the same
    /// console list `cargo xtask run` bakes for the first machine.
    Session,
}

/// The command line an Alpine boot carries, given the caller's mode choice.
/// Always exactly one trailing newline: `mpos.cmdline` is read whole by the
/// loader and recorded verbatim by the kernel.
///
/// `root=` rides on both answers since §15.6: this machine's root switch moved
/// from the kernel into its initramfs script, and a script with no `root=` sits
/// in the ramfs instead ("staying here"), so the disk it booted would be the
/// disk the boot never reaches. The x86_64 image spells the same key through
/// `rootfstype=` and a partition number; here the ext4 is the whole
/// (unpartitioned) virtio disk, which the block layer names `/dev/vda`.
fn alpine_cmdline(choice: ModeChoice) -> Result<String> {
    let mode = match choice {
        ModeChoice::Verdicts => return Ok(ALPINE_CMDLINE.to_string()),
        ModeChoice::Session => crate::rootfs::effective_mode()?,
    };
    Ok(format!(
        "{} root=/dev/vda rootfstype=ext4\n",
        mode.cmdline_consoles(SERIAL_PORT)
    ))
}

/// This machine's serial console, spelled the way its psABI names the PL011.
const SERIAL_PORT: &str = "ttyAMA0";

/// Whether an interactive aarch64 session puts a getty on the serial node.
///
/// One rule, and it is the x86_64 image's own: the login answers on the
/// framebuffer when there *is* one (`gui`, `mirror` - their consoles reach the
/// screen, and the vcmux still carries the virtual consoles to the wire), and
/// on the serial node when the session has no window at all (`terminal`).
/// The headless verdicts render nothing: their gate reads `login:` off the
/// wire, which the mux delivers from tty1 either way.
pub(crate) fn serial_getty_for(mode: crate::config::RunMode) -> bool {
    !mode.has_window()
}

/// The file every aarch64 disk root carries, and the bytes it carries.
///
/// The kernel's `check_fs` mounts the root's ext4 over the MMIO virtio-blk device
/// and reads *this* file, comparing what it got against these bytes. The name and
/// the content are the two halves of that contract (the kernel side spells the
/// same string in `arch::aarch64::check_fs`), and the point of checking content
/// rather than existence is that a filesystem which merely *mounts* cannot pass.
/// Both roots carry it - the test staging writes it directly, and the Alpine
/// install writes it beside the tree - so the check is the same check on every
/// aarch64 boot.
const PROBE_FILE: &str = "hello";
const PROBE_BYTES: &[u8] = b"mpos reads from ext4 over virtio-MMIO\n";

/// How long the boot is given before it is called stuck. Generous: AAVMF's own
/// startup is most of it, and the kernel's checks are milliseconds.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long the Alpine boot is given before it is called stuck. Much longer than
/// [`DEADLINE`], and for a reason the boot itself explains: this one runs a real
/// userland - busybox init forks openrc, whose `sysinit`/`boot`/`default`
/// runlevels start a dozen services - where the marker boot's checks are
/// milliseconds. Measured on an unwarmed TCG machine, the prompt is well inside
/// this; the slack is so that a *slow* boot is not reported as a stuck one.
const ALPINE_DEADLINE: Duration = Duration::from_secs(240);

/// What ends the Alpine boot's log read: a login prompt, or a stop that says the
/// machine will not reach one.
///
/// Unlike [`TERMINATORS`] this list is not the machine's marker chain - this boot
/// prints no `M*_OK` for it to key on (those are the kernel's own checks, run
/// before the handoff on every aarch64 boot) - but the *userland's* own words:
/// `login:` from getty, and the kernel's two stops.
const ALPINE_TERMINATORS: [&str; 3] = ["login:", "KERNEL PANIC", "EXCEPTION:"];

/// The ext4 image `build_esp` lays the aarch64 disk from. `boot-aarch64`'s
/// payload is three small images plus the shared overlay trees; the test sweep
/// also carries the musl test programs (~5.6 MB), so it asks for its own size
/// rather than making every boot allocate room for the sweep.
const DISK_BYTES: u64 = 8 * 1024 * 1024;
const SWEEP_DISK_BYTES: u64 = 64 * 1024 * 1024;

/// The Alpine root image's size. A cross-arch `alpine-base` + `openrc` tree is
/// ~15 MiB on disk (measured), and `mkfs.ext4 -d` lays a filesystem whose used
/// blocks are the tree's; the slack is for the `/var` a running openrc writes as
/// it boots and for the log files a session will grow. It is the one disk here
/// whose size is a *userland's* choice rather than a payload's.
const ALPINE_DISK_BYTES: u64 = 256 * 1024 * 1024;

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
/// `M8_NET_OK` is not a terminator and `M8_NET_FAILED` is. The input class's
/// success marker is printed later still - the registry it pins is read back
/// through a `/dev` that only exists after `check_dev` has mounted it - so
/// `M8_INPUT_OK` is not a terminator either, while `M8_INPUT_FAILED` is, at both
/// the point the window opens and the point the registry is read back (§15.2).
/// The console class is the same shape one device later: its marker is printed
/// after `check_dev` too (the `/dev/hvc0` it names is resolved through the
/// mounted `/dev`), so `M8_CONSOLE_OK` is not a terminator and
/// `M8_CONSOLE_FAILED` is (§13.25). The GPU class is the same again, one device
/// later still: `M8_GPU_OK` is read back through the mounted `/dev` as well, so
/// it is not a terminator and `M8_GPU_FAILED` is (§13.26). The sound class is
/// the same again - `M8_SOUND_OK` is printed after `check_dev` too - so it is
/// not a terminator and `M8_SOUND_FAILED` is (§13.27). The vsock class is the one
/// whose success marker is *conditional*: its device is attached only on a host
/// that carries `/dev/vhost-vsock` (vsock is the one virtio device QEMU does not
/// emulate in userspace), so `M8_VSOCK_OK` is required only there - and the skip
/// is printed, never silent - while `M8_VSOCK_FAILED`, a window that exists and
/// will not open, is the terminator (§13.28). The 9p class is the sound class's
/// shape one device later: its marker is read back through the mounted share
/// after the root switch, so `M8_P9_OK` is not a terminator and `M8_P9_FAILED`
/// is (§13.29).
const TERMINATORS: [&str; 28] = [
    "INIT_EXITED",
    "MPOS_SWITCH_ROOT_FAILED",
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
    "M7_DEV_FAILED",
    "M8_NET_FAILED",
    "M8_INPUT_FAILED",
    "M8_CONSOLE_FAILED",
    "M8_GPU_FAILED",
    "M8_SOUND_FAILED",
    "M8_VSOCK_FAILED",
    "M8_P9_FAILED",
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

/// Lay one payload file on a staging tree, creating any directory its name
/// carries and marking it executable.
///
/// The name may hold a directory: the payload's own programs sit flat at the
/// disk root, while the tools `t-stdspawn` and `t-socketpair` run live in
/// `/bin`. And the mode is set *here* because `mkfs.ext4 -d` copies what it
/// finds: a file written mode 0644 lands on the disk non-executable, and
/// `/bin/true` would then answer `EACCES` at `execve` - a failure that reads as
/// a broken program rather than a staging step that forgot.
fn stage_program(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("Failed to lay {name} on the aarch64 disk"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to chmod +x {name} on the aarch64 disk"))?;
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

    // The headless verdict's disk payload: `/init` is the launcher that stands
    // as pid 1 on this disk, `/t` the fixed-marker target it execs, `/musl` the
    // unmodified static-musl binary. Since §15.6 the chain that reaches them
    // starts in the boot's own early userspace: the initramfs script mounts
    // `/dev/vda`, promotes it into `/` (`MPOS_SWITCH_ROOT_OK`) and `switch_root`s
    // into the disk's `/init`, which `execve`s `/t`, whose parent then `execve`s
    // `/musl`. A green verdict shows that switch line, the target's
    // `MPOS_EXEC_OK`/`MPOS_FORK_OK`, the musl run's markers, and `INIT_EXITED` -
    // proving aarch64 `execve` replaced the running image with a second one that
    // then ran at EL0 (PLAN.md §9.1 stage 6D) *through the same pivot flow the
    // first machine's initramfs script runs*.
    //
    // `/init` is a program rather than a script because this disk has no
    // interpreter to serve one: it is the build's own payload, so it carries no
    // busybox, and the overlay's `rootfs-aarch64/init` - which *is* the Alpine
    // disk's pid 1 - names a `/bin/sh` that is not here (see
    // `aarch64_init::exec_launcher_init_elf` for the measured failure).
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
        None,
    )?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp, &TERMINATORS, DEADLINE)?;
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
    // EL0 and answered the call it made, `M6_OK`/`M7_OK`/`M8_IPC_OK` are the
    // shared check families this machine runs alongside the first one (the
    // syscall gate, ramfs root, mounts and device files; multi-task file I/O
    // through the fd table; pipes, the tty line discipline and signals), and
    // `M6_INIT_OK` says the scheduler
    // started, the loader's aarch64 `/init` was loaded and run to completion, and
    // its `exit_group` was routed through the shared syscall table and reaped - the
    // first real userspace program on this machine.
    //
    // One of them is conditional: `M8_VSOCK_OK` is required only when the host
    // could back the device (`vsock_available`), because a host without
    // `/dev/vhost-vsock` cannot start a machine carrying one at all. The skip is
    // printed below rather than taken silently.
    let mut markers: Vec<(&str, &str)> = vec![
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
            "MPOS_SWITCH_ROOT_OK",
            "root switch: the initramfs script mounted /dev/vda and promoted it \
             into / - the same flow the first machine's script runs (§15.6). It is \
             printed by *userspace*, which is what makes the marker the proof that \
             a program ran on the switched tree, not merely that the kernel could \
             pivot one",
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
            "the machine's virtio-input is in the shared evdev registry and \
             /dev/input/event0 resolves through the mounted /dev at evdev's 13:64",
        ),
        (
            "M8_CONSOLE_OK",
            "the machine's virtio-console port is installed over MMIO and \
             /dev/hvc0 resolves through the mounted /dev",
        ),
        (
            "M8_GPU_OK",
            "the machine's virtio-gpu card is installed over MMIO, /dev/fb0 \
             resolves through the mounted /dev and the render task owns the device",
        ),
        (
            "M8_SOUND_OK",
            "the machine's virtio-sound card is installed over MMIO, /dev/dsp \
             resolves through the mounted /dev and the stream took a silent burst",
        ),
        (
            "M8_P9_OK",
            "the machine's 9p share is installed over MMIO, mounted at /mnt/share \
             and a file the host laid there reads back through the VFS",
        ),
    ];
    if vsock_available() {
        markers.push((
            "M8_VSOCK_OK",
            "the machine's virtio-vsock device is the AF_VSOCK family's wire over \
             MMIO (the family answers the device's own guest CID)",
        ));
    } else {
        println!(
            "vsock:   the host has no /dev/vhost-vsock, so this boot attaches no vsock \
             device and M8_VSOCK_OK is not required"
        );
    }
    markers.extend([
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
        // The two shared families this machine now runs as well, after the
        // kernel's own interrupt stub was taught to keep the resume state of the
        // context it preempted (`PLAN.md` §13.22): with either wired and that
        // missing, the boot took an instruction abort at EL1 on the next
        // preemption, so these three markers are the assertion that the gate
        // stayed in place.
        (
            "M6_OK",
            "syscall gate + fd table, ramfs root, a mounted tree, device files",
        ),
        ("M7_OK", "multi-task file I/O through the fd table"),
        (
            "M8_IPC_OK",
            "pipes, the tty line discipline, signal delivery",
        ),
        ("M6_INIT_OK", "init handed to pid 1"),
        ("INIT_EXITED", "init ran and exited via the scheduler"),
    ]);
    let missing: Vec<&str> = markers
        .iter()
        .filter(|(marker, _)| !log.contains(marker))
        .map(|(_, what)| *what)
        .collect();
    // The disk's root tree is the shared overlay pair this machine installs
    // from, and since §15.6 it is *read by userspace*: the staged `/t` execs
    // `/etc/inittab` through libc (`open` + `read`) after its fork round trip,
    // and prints what came back as a byte count. That number is the length of
    // the file *as staged* - core + machine merged, placeholder rendered - so a
    // copy that dropped, truncated or replaced it cannot print the line below,
    // which is what makes the marker a check rather than a claim (and after
    // §13.19 folded the two inittabs into one template, reading the repository
    // file raw would pin a byte count no image ever carries). It is also the
    // disk root's own proof: the read only succeeds on the switched tree, after
    // the script's pivot moved the ext4 into `/`.
    let staging = build_dir.join("aarch64-ext4-staging");
    let laid = fs::read(staging.join("etc/inittab")).with_context(|| {
        format!(
            "the boot verdict cannot pin the disk tree: {} was not staged - \
             `build_esp_with_cmdline` lays it before every test boot",
            staging.join("etc/inittab").display()
        )
    })?;
    let want = format!("MPOS_INITTAB_OK bytes={:04}", laid.len());
    if !log.contains(&want) {
        bail!(
            "the aarch64 disk root did not serve the rootfs tree: the log has no \
             \"{want}\" from /t's read of /etc/inittab, and the staged file is {} bytes",
            laid.len()
        );
    }
    // A real console line reaches a userspace program through the whole chain:
    // the kernel queued "echo-test\n" on the tty at handoff, the initramfs
    // script's `cat /dev/console` read it back (its canonical echo shows the
    // same bytes without the prefix), and the prefixed form on the wire is the
    // *program's* output - proof that a scheduler-run EL0 process read console
    // input and wrote the bytes it read, through `syscall::dispatch`, on the
    // ramfs root, before the pivot.
    if !log.contains("R:echo-test") {
        bail!(
            "the aarch64 init never echoed the input it read: the R:echo-test \
             line is absent (the initramfs script's read/echo round trip on the \
             console did not complete)"
        );
    }
    // The target's very first act pushes `sp` a page below the mapped stack and
    // stores there; that store only survives if the kernel's aarch64 EL0 fault
    // handler demand-pages it, and `MPOS_FAULT_OK` is written immediately after.
    // Its presence proves user page faults are serviced (the prerequisite for any
    // real, larger program - including a static-musl binary - to run at EL0).
    if !log.contains("MPOS_FAULT_OK") {
        bail!(
            "the aarch64 exec target's demand-paging store did not survive: the \
             MPOS_FAULT_OK line is absent (el0_sync did not take the stack page)"
        );
    }
    // The disk's pid 1 exec'd `/t`: the marker prints before anything else `/t`
    // does, so its presence proves the *switched* root served a pid-1 program
    // and that `execve` on the disk replaced it (`INIT_EXITED` at the end of it
    // all is `/musl`'s own `exit_group`, reaped from pid 1's subtree - execve
    // keeps the pid).
    if !log.contains("MPOS_EXEC_OK") {
        bail!(
            "the aarch64 init chain never reached its exec target: the \
             MPOS_EXEC_OK line is absent (the disk's /init did not exec /t)"
        );
    }
    // `/t` then forks (aarch64 has no `fork` syscall; `clone(SIGCHLD)` is the
    // real path), reads `/etc/inittab` through libc, and its child writes
    // `MPOS_FORK_OK`. The parent `wait4`s before moving on, so the child's line
    // is on the wire. Their presence proves aarch64 `clone` started a distinct
    // running process that reached EL0 (stage 6D) *and* that the switched root
    // answers a userspace `open`/`read` of a repository file (the inittab pin
    // above reads the byte count back).
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
        // The summary must not claim the vsock class on a host that could not back
        // it: its marker is required only when the device was attached, so the
        // parts of the sentence describing it are built from the same decision.
        let vsock_marker = if vsock_available() {
            "M8_VSOCK_OK, "
        } else {
            ""
        };
        let device_count = if vsock_available() { "nine" } else { "eight" };
        let vsock_device = if vsock_available() {
            ", and a virtio-vsock device the AF_VSOCK family is brought up over - \
             answering the same guest CID the first machine's own probe answers"
        } else {
            ""
        };
        println!();
        println!(
            "aarch64 boot: M4_CONSOLE_OK, M4_DEVICES_OK, M3_TABLES_OK, M5_VIRTIO_OK, \
             M7_RNG_OK, M7_BLK_OK, M7_FS_OK, M7_DEV_OK, M8_NET_OK, M8_NET_STACK_OK, M8_INPUT_OK, \
             M8_CONSOLE_OK, M8_GPU_OK, M8_SOUND_OK, {vsock_marker}M8_P9_OK, M1_TIMER_OK, M2_CONTEXT_OK, \
             M6_AS_OK, \
             M6_PAGING_OK, M6_WALL_OK, M6_CPU_OK, \
             M6_SERIAL_RX_OK, M6_TLS_OK, M6_EL0_OK, M6_CONSOLE_RX_OK, M6_INIT_OK, the \
             initramfs script's echoed input (R:echo-test) and its root switch \
             (MPOS_SWITCH_ROOT_OK), the demand-paged stack store (MPOS_FAULT_OK), \
             the exec target (MPOS_EXEC_OK), its fork child (MPOS_FORK_OK) and its \
             read of the switched root's inittab (MPOS_INITTAB_OK), \
             the unmodified musl binary (MPOS_MUSL_OK), and INIT_EXITED - the machine \
             drives {device_count} devices over the memory-mapped virtio transport (rng, blk with a \
             verified sector read and a written sector round-tripped, net whose TX virtqueue accepted a frame over MMIO \
             and which the smoltcp stack is then initialized over, the same \
             net::init + spawn_service pair the first machine boots with, input, which it hands to the \
             shared evdev registry the first machine has had since §13.4 - so /dev/input/event0 is this \
             machine's own keyboard, named by the registry and resolving at evdev's 13:64, a \
             virtio-console port, installed through the same shared driver the first machine opens on its PCI \
             bus and resolving as /dev/hvc0, a virtio-gpu card whose framebuffer is /dev/fb0, \
             owned by the shared render task, a virtio-sound card driving /dev/dsp{vsock_device}, \
             and the host's 9p share mounted at /mnt/share with a file it laid there read back \
             through the VFS) \
             and mounts an ext4 filesystem \
             over the block (the disk root), and a boot whose init chain - the \
             script that echoed and switched root, then the disk's own `/init` \
             exec'ing a program that demand-paged, cloned, waited, read \
             /etc/inittab and ran a \
             real static-musl ELF at EL0 \
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
    // The machine's `/bin`: the programs `t-stdspawn` and `t-socketpair` start.
    // One multi-call image, staged under each name it answers to (`aarch64_bin`
    // says why it is built in-tree rather than installed from a distribution).
    let tools = crate::aarch64_bin::build(build_dir)?;
    let tool_paths: Vec<String> = crate::aarch64_bin::APPLETS
        .iter()
        .map(|applet| format!("bin/{applet}"))
        .collect();
    let built = build_user_tests_for_aarch64()?;
    println!(
        "user tests: {} aarch64 programs built, plus /bin/{{{}}} for the tests that start other programs",
        built.len(),
        crate::aarch64_bin::APPLETS.join(",")
    );

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

    // The runner is `/init` on the disk: the initramfs script switches into the
    // disk, the disk's own chain (`/sbin/init` from the applet farm) execs it,
    // and it runs every `/t-*` staged beside it - the same handoff an Alpine boot
    // gets, with a test payload instead of a distribution under it. The programs
    // sit flat at the root and the `/bin` tools under their own directory; the
    // initramfs carries no copy of them any more (§15.6 unified the flow onto the
    // script pivot, so the ramfs holds busybox and the script alone).
    //
    // The overlay pair lands *before* this payload inside `build_esp_with_cmdline`,
    // so the disk's own post-switch `/init` (the pid-1 chain the Alpine image
    // ships) is overwritten by the runner here - the two name one path and the
    // payload wins, which is the order that function documents.
    let mut entries: Vec<(&str, &[u8])> = vec![("init", init.as_slice())];
    for path in &tool_paths {
        entries.push((path.as_str(), tools.as_slice()));
    }
    for (name, bytes) in &tests {
        entries.push((name.as_str(), bytes.as_slice()));
    }
    let esp = build_esp(
        build_dir,
        &kernel,
        &loader,
        &entries,
        SWEEP_DISK_BYTES,
        None,
    )?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp, &["=== END"], DEADLINE)?;
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

/// Install the second machine's real Alpine userland into a staging tree and
/// return it.
///
/// This is the one place the two machines' *userlands* differ, and the reason is
/// a fact about each machine rather than a taste: x86_64 boots a desktop
/// (`rootfs.toml`: weston, mesa, DRM, seatd), while this machine has no GPU, no
/// DRM and no PCI bus to hang the card on, so its Alpine tree is the smallest one
/// that boots to a login prompt - `alpine-base` and `openrc`, the packages
/// `rootfs-aarch64.toml` names. What is *not* duplicated is the install: the
/// repositories text, the keys, the mirror and the apk lookup are the x86_64
/// path's own helpers (`utils::repositories_text`, `utils::install_apk_keys`,
/// `utils::find_apk_tool`), and only the tree they are pointed at differs.
///
/// Two choices are load-bearing and both are measured:
///
///   * **`fakeroot`, not `--usermode`.** A cross-arch `apk` cannot run the
///     packages' post-install scripts - they are the *target's* binaries and the
///     host cannot exec them (`execve: Exec format error`, measured) - so
///     `--no-scripts` is required, and the applet farm such a script would build
///     is made at first boot instead (see `rootfs-aarch64/init`). But `--usermode`
///     writes every file owned by whoever ran the build, and `/etc/shadow` mode
///     0600 owned by a uid that is not root is a login that cannot read its own
///     password file. `fakeroot` gives the tree root ownership without taking
///     root, so this command - like `boot-aarch64` and the sweep - stays
///     unprivileged.
///   * **the overlay is core + `rootfs-aarch64/`, not `rootfs/`.** The shared
///     half of both userlands lives in `rootfs-core/` (`inittab`, the network,
///     the resolver, doas); what stays machine's own is the `/init` that mounts
///     the pseudo-filesystems a real init expects. The x86_64 side does that
///     from its initramfs script; this machine pivots in the *kernel*, so its
///     disk `/init` is where that work lives. After the trees merge, the same
///     generated layer the first install runs - hostname, locale, the securetty
///     cleanup, the resolver - lands here too: it is pure file I/O, and §13.12's
///     prompt said `localhost` precisely because this path skipped it.
fn install_alpine_root(build_dir: &Path) -> Result<PathBuf> {
    let root = utils::project_root()?;
    let config = crate::config::RootfsConfig::load(&root.join("rootfs-aarch64.toml"))?;
    let staging = build_dir.join("aarch64-alpine-root");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .with_context(|| format!("Failed to create {}", staging.display()))?;

    // Repositories and keys first, the way `install_base` does it: apk decides a
    // repository index is trusted by finding a key whose filename matches the
    // name in the index's signature, and without them every package answers
    // `UNTRUSTED signature` and then `no such package`.
    fs::create_dir_all(staging.join("etc/apk/keys"))?;
    fs::write(
        staging.join("etc/apk/repositories"),
        utils::repositories_text(&config.mirror, &config.branch, &config.repos),
    )?;
    utils::install_apk_keys(&staging.join("etc/apk/keys"))?;

    let apk = utils::find_apk_tool(build_dir, &config.mirror, &config.branch)?;
    let mut packages = config.base_packages.clone();
    packages.extend(config.extra_packages.iter().cloned());
    // The same retry the x86_64 install wraps its `apk add` in. Its reason was
    // NBD I/O errors under heavy write load; this tree has no nbd device, but
    // the failure it guards is the mirror fetch mid-install, and "apk records
    // what it did manage to install, so a plain re-run finishes the job" is the
    // answer for either producer. `fakeroot` rides in through the preflags, so
    // the command shape is unchanged.
    crate::rootfs::apk_add(
        &["fakeroot", "--", apk.as_str()],
        &staging,
        &[
            "--arch",
            config.arch.as_str(),
            "--update-cache",
            "--initdb",
            "--no-scripts",
        ],
        &packages,
    )?;

    // The overlay pair, and the inittab rendered with it (`merge_overlays`'s
    // rule: a staged tree never carries an unrendered placeholder). The stock
    // answer for the installed tree; a session re-renders against its own mode
    // before laying the disk.
    crate::rootfs::merge_overlays(&root, "rootfs-aarch64", &staging, &config, false)?;
    // The runlevels, from the same table the x86_64 image uses: a service whose
    // script this machine's package set does not carry is named and skipped, so
    // one list serves both machines. Without this the boot would hand pid 1 to a
    // busybox init whose `::sysinit:/sbin/openrc sysinit` lines run against empty
    // runlevel directories - an Alpine userland that starts no service at all.
    crate::rootfs::enable_services(&staging)?;
    // And the module index, for the same reason and from the same helper the
    // x86_64 install uses: the `modules` service this table turns on reads
    // `/etc/modules` - `af_packet` and `ipv6` here - and asks `modprobe` for each
    // name, so without `/lib/modules/<release>` the boot prints "can't change
    // directory to '/lib/modules'" for every line. `modules.builtin` is how a
    // machine with no loadable modules answers that honestly: a hit means "already
    // loaded", exit 0, nothing printed. The names are the same ones on both
    // machines because they are one kernel's code, and the directory is named for
    // the release both machines report (`uname -r` is one number).
    crate::rootfs::write_modules(&staging)?;
    // The generated system layer, now that the cross-arch install can consume
    // it: `[system] hostname` becomes `/etc/hostname` (without it the prompt
    // says `localhost`, which §13.12 had to name rather than fix), the locale
    // export lands in `/etc/profile.d`, the passwordless shadow clearing makes
    // the login actually work, the securetty cleanup keeps `login` from calling
    // an empty password a wrong one, and the init.d chmod sweep is the one
    // thing OpenRC silently refuses without. All pure file I/O - none of it
    // needs the image's binaries to run, which is the whole boundary (§13.19).
    crate::rootfs::write_shared_config(&staging, &config)?;
    // The probe file the kernel's `check_fs` reads (see [`PROBE_FILE`]). It sits
    // at the root of a real Alpine tree because the check is about the *block
    // driver and the filesystem*, which the machine must prove on whichever root
    // it boots - and the proof is the file's known bytes, not a name it happens to
    // have. Written here rather than in `rootfs-aarch64/` so the two roots' copies
    // come from one constant and cannot drift.
    fs::write(staging.join(PROBE_FILE), PROBE_BYTES)
        .with_context(|| format!("Failed to lay the probe file on {}", staging.display()))?;
    // The resolver, after the trees and the generated layer - the one file
    // that beats a file in the tree, which is what makes `[system] dns` the
    // answer it is (rootfs.rs's `write_resolver` carries the full story; it is
    // one slirp fact for both machines). Without this call the guest rides
    // QEMU's forwarder into the host's loopback stub and every `apk` fetch
    // from the *running* machine asks a question nothing answers.
    crate::rootfs::write_resolver(&staging, &config)?;
    println!(
        "alpine: {} packages installed into {} for {}",
        packages.len(),
        staging.display(),
        config.arch
    );
    Ok(staging)
}

/// The Alpine root image's ESP: the loader, the kernel, an *empty* initramfs and
/// the real Alpine tree on the disk.
///
/// The initramfs is empty on purpose. The kernel unpacks it into the ramfs it
/// boots on and then switches `/` onto the disk, so the archive's only job here
/// is to exist - the machine's init is the disk's own `/init`, which is what the
/// root switch is for. The `entries` a test boot fills it with are the payload
/// those boots have and this one does not.
///
/// `mode` decides the command line ([`alpine_cmdline`]) and, for a session,
/// whether the installed inittab carries the serial getty. The install itself
/// is stamped ([`alpine_root_is_current`]): a fresh kernel re-lays the disk
/// from the existing staging tree without touching the network, exactly the
/// way x86_64's `run` updates boot files without reinstalling Alpine.
fn build_alpine_esp(
    build_dir: &Path,
    kernel: &Path,
    loader: &Path,
    mode: ModeChoice,
) -> Result<PathBuf> {
    ensure_alpine_root(build_dir)?;
    let alpine_root = alpine_staging(build_dir);
    let esp = build_esp_with_cmdline(
        build_dir,
        kernel,
        loader,
        &[],
        ALPINE_DISK_BYTES,
        Some(&alpine_root),
        &alpine_cmdline(mode)?,
    )?;
    // A session's getty choice is re-asserted on the staging tree *after* the
    // install stamp has had its way with it: `run --mode terminal` must find
    // the login on the wire even when the tree was installed under a windowed
    // default, and the refresh is one file rather than a reinstall. The
    // verdicts keep the installed answer - their gate reads `login:` through
    // the mux either way.
    if mode == ModeChoice::Session {
        let config =
            crate::config::RootfsConfig::load(&utils::project_root()?.join("rootfs-aarch64.toml"))?;
        // The template is idempotent: re-merging the pair lands the same files,
        // and the render inside `merge_overlays` answers this session's mode.
        crate::rootfs::merge_overlays(
            &utils::project_root()?,
            "rootfs-aarch64",
            &alpine_root,
            &config,
            serial_getty_for(crate::rootfs::effective_mode()?),
        )?;
    }
    Ok(esp)
}

/// Where the Alpine staging tree lives, whether or not it is installed yet.
fn alpine_staging(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-alpine-root")
}

/// The stamp that records what the Alpine tree was installed *from*.
fn alpine_stamp(build_dir: &Path) -> PathBuf {
    build_dir.join("aarch64-alpine-root.stamp")
}

/// True when the staging tree predates something it is built from: the toml,
/// either overlay tree, or the kernel whose release name `/lib/modules/<re>`
/// carries (`write_modules`' directory outlives a version bump otherwise).
/// A missing tree is stale by definition - there is nothing to reuse.
fn alpine_root_is_stale(build_dir: &Path) -> Result<bool> {
    let root = utils::project_root()?;
    let staging = alpine_staging(build_dir);
    let stamp = alpine_stamp(build_dir);
    if !staging.exists() || !stamp.exists() {
        return Ok(true);
    }
    let since = utils::file_mtime(&stamp)?;
    for input in [
        root.join("rootfs-aarch64.toml"),
        root.join(crate::rootfs::CORE_OVERLAY),
        root.join("rootfs-aarch64"),
    ] {
        if utils::dir_newer_than(&input, since)? || utils::newer(&input, &stamp)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Install the Alpine tree unless the stamp says the staging directory already
/// holds the current one - then write the stamp. The re-lay of the ext4 image
/// happens on *every* ESP build regardless; this stamp only decides whether the
/// network-backed `apk` install runs. That is what makes `run --arch aarch64`
/// after a kernel change cost seconds instead of a package fetch, and it is the
/// same split x86_64 draws between its boot-file update and its full install.
pub(crate) fn ensure_alpine_root(build_dir: &Path) -> Result<()> {
    if alpine_root_is_stale(build_dir)? {
        install_alpine_root(build_dir)?;
    } else {
        println!(
            "alpine: {} is up to date (reinstall inputs unchanged)",
            alpine_staging(build_dir).display()
        );
    }
    fs::write(alpine_stamp(build_dir), "alpine: staged tree\n")?;
    Ok(())
}

/// Boot the second machine into its real Alpine userland: busybox init reading
/// `/etc/inittab`, openrc's runlevels, and a getty on a virtual console.
///
/// This is the boot the machine was being built for. It is headless and bounded -
/// the PL011 goes to a log file and the run ends when the log shows the login
/// prompt or a panic, or when the deadline passes - because the point of this
/// command is the *verdict*: how far a real, unmodified aarch64 Alpine tree gets
/// on this kernel. The interactive session a person types at is `run --arch
/// aarch64` once this boot is green.
pub fn run_alpine(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile)?;
    println!("kernel: {} ({:.1} KB)", kernel.display(), kib(&kernel)?);
    println!("loader: {} ({:.1} KB)", loader.display(), kib(&loader)?);
    let esp = build_alpine_esp(build_dir, &kernel, &loader, ModeChoice::Verdicts)?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp, &ALPINE_TERMINATORS, ALPINE_DEADLINE)?;
    print!("{log}");

    // openrc's own verdict, which is a stricter answer than `login:`. A service
    // that fails still prints `[ !! ]` and the runlevel carries on, so the
    // prompt is reached either way - and that is exactly how the fourth such
    // failure hid: `mount: mounting proc on /proc failed: Resource busy`, from
    // a procfs missing `/proc/<pid>/environ`, sat in the log while this command
    // reported success. The three §13.12/§13.14/§13.15 chased down (`hwclock`,
    // `modprobe`, `syslog`) all read the same way, so the marker is named here
    // rather than left to whoever reads the log next.
    if let Some(detail) = openrc_failure(&log) {
        bail!(
            "an openrc service failed on the Alpine boot: {detail} \
             (the `[ !! ]` marker is in the log above)"
        );
    }

    // The verdict. `login:` is what busybox `getty` has printed once it has run
    // `login(1)` on a console - the whole chain of this kernel serving a real
    // Alpine userland, in one word. Its absence is not a failure to *report* but a
    // result to *read*: the log above says how far the machine got, and naming
    // that is the point of the command. So a run that does not reach the prompt
    // returns the log and an honest non-zero, never a fabricated success.
    if log.contains("login:") {
        println!("alpine: the aarch64 machine reached a login prompt");
        return Ok(());
    }
    bail!(
        "the aarch64 Alpine boot did not reach a login prompt within {ALPINE_DEADLINE:?}; \
         the serial log above says how far it got"
    )
}

/// The first openrc service failure in `log`, named by the line that explains
/// it - or `None` when every service came up.
///
/// openrc marks the result of each service with `[ ok ]` or `[ !! ]`, wrapped in
/// the terminal escapes that make the marker coloured, and it prints the
/// service's own message on the line *above* the marker. So the search is: strip
/// the escapes, find the marker, and walk back to the last line with anything on
/// it - for the failure that motivated this that is `mount: mounting proc on
/// /proc failed: Resource busy`, and for a failure that prints nothing it is the
/// marker's own line.
///
/// Kept as a function rather than inlined because it is the whole of the check
/// and the only way to test it is against a captured log; the case it was
/// written for is a unit test below.
fn openrc_failure(log: &str) -> Option<String> {
    let plain = without_ansi(log);
    let lines: Vec<&str> = plain.lines().collect();
    let at = lines.iter().position(|line| line.contains("!!"))?;
    lines[..at]
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .or_else(|| lines.get(at))
        .map(|line| line.trim().to_string())
}

/// The log with openrc's terminal escapes removed.
///
/// A failure marker arrives wrapped in colour and cursor codes -
/// `ESC[1mESC[31m !! ESC[1mESC[34m]` - so the service's own message, which is
/// what a person needs, is two lines above a marker no `grep` will show. This
/// strips `ESC [ ... letter` sequences and carriage returns so the line can be
/// quoted in an error message as it was written.
fn without_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Only CSI is used by openrc's output; anything else is left
                // in place rather than guessed at.
                if chars.next() == Some('[') {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Boot the kernel and its loader for this machine.
///
/// The build itself is `build_kernel` with [`Arch::Aarch64`] - the same call
/// every command makes, so the pair of binaries and the targets they were
/// compiled for cannot drift apart between `cargo xtask build --arch aarch64`
/// and this command. What stays here is what is genuinely this machine's: the
/// paths those two binaries land on.
fn build(profile: &str) -> Result<(PathBuf, PathBuf)> {
    utils::build_kernel(profile, Arch::Aarch64)?;
    Ok((
        utils::kernel_path(profile, Arch::Aarch64)?,
        utils::loader_path(profile, Arch::Aarch64)?,
    ))
}

/// The volume a UEFI firmware reads.
///
/// `entries` are the *disk* payload - the programs a test boot's `/init` chain
/// execs (`/t`, `/musl`, the sweep runner and its `/bin` tools) - staged onto
/// the root tree after the overlay pair, so they win any name the distribution
/// files also carry. The initramfs is no longer one of the caller's choices:
/// since §15.6 every aarch64 boot carries the same real early userspace (the
/// busybox archive the shared `initramfs` pipeline builds for this machine),
/// because the root switch that
/// used to live in the kernel for a test boot and in the disk script for an
/// Alpine one now lives in the *script* on both - which is what makes this
/// machine's boot the first machine's flow rather than its shape-alike.
fn build_esp(
    build_dir: &Path,
    kernel: &Path,
    loader: &Path,
    entries: &[(&str, &[u8])],
    disk_bytes: u64,
    alpine_root: Option<&Path>,
) -> Result<PathBuf> {
    // A test boot carries the fixed selftest line; an Alpine boot's caller
    // says which consoles it wants (`alpine_cmdline`), because a session and a
    // scripted verdict are different machines' worth of console.
    let cmdline = CMDLINE.to_string();
    build_esp_with_cmdline(
        build_dir,
        kernel,
        loader,
        entries,
        disk_bytes,
        alpine_root,
        &cmdline,
    )
}

/// The volume a UEFI firmware reads, with the command line written by the
/// caller rather than chosen here. See [`build_esp`] for everything else.
fn build_esp_with_cmdline(
    build_dir: &Path,
    kernel: &Path,
    loader: &Path,
    entries: &[(&str, &[u8])],
    disk_bytes: u64,
    alpine_root: Option<&Path>,
    cmdline: &str,
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
    // The command line the kernel records and reads back. A boot that carries an
    // Alpine root is not a test and says so by *leaving the word off* - see
    // `ALPINE_CMDLINE` for why that one word is the difference between a console
    // an init may use and one a test has written to.
    fs::write(esp.join("mpos.cmdline"), cmdline)?;

    // The third file is the real aarch64 initramfs - busybox, the applet farm
    // and the pivot script - installed on *every* boot's volume, the Alpine one
    // included: the archive exists so the machine has a userland to run before
    // the disk is reached, and reaching the disk *is* running that userland's
    // `/init`. The loader copies the archive into frames and records its address
    // and length in the handoff; the kernel unpacks it into a ramfs and runs its
    // `/init` (PLAN.md §9.1 stages 6D/6E, §15.6 for the flow's one shape).
    let initramfs = crate::initramfs::ensure_path(build_dir, crate::arch::Arch::Aarch64)?;
    fs::copy(&initramfs, esp.join("initramfs.tar"))
        .with_context(|| format!("Failed to install {}", initramfs.display()))?;
    println!("initramfs: aarch64 {} -> ESP", initramfs.display());

    // A raw block image, attached below as a virtio-blk **MMIO** transport so
    // `check_blk` can read sector 0 and *verify* it against a known pattern - the
    // drivers stage's second driven device (`PLAN.md` §9.1). Every byte is
    // `i as u8`, so the first 512 bytes read back exactly as the check expects.
    // It is read, never mounted - the filesystem-over-block step comes later.
    let blk_img = build_dir.join("aarch64-blk.img");
    let pattern: Vec<u8> = (0..1024 * 1024u32).map(|i| i as u8).collect();
    fs::write(&blk_img, &pattern)
        .with_context(|| format!("Failed to write {}", blk_img.display()))?;

    // A second block image: the machine's root filesystem, which is a bare ext4
    // attached as a virtio-MMIO block and mounted by the kernel's `check_fs`.
    //
    // Two trees become it. The *test* roots are the in-tree payload - `hello`,
    // the programs a test `/init` execs (staged after the overlay, below), and
    // the repository's shared overlay pair (`rootfs-core/` + `rootfs-aarch64/`,
    // exactly what the Alpine install merges, so the file `check_fs` reads
    // through the VFS is one file for both machines' paths). The *Alpine* root
    // is a whole tree `apk` installed (see [`install_alpine_root`]) - the second
    // machine's real userland. Whichever it is, `mkfs.ext4 -d` lays a filesystem
    // down from the directory without a mount, so this command needs no privilege
    // to write a root only the guest will read.
    let staging;
    let tree: &Path = match alpine_root {
        Some(root) => root,
        None => {
            staging = build_dir.join("aarch64-ext4-staging");
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging)
                .with_context(|| format!("Failed to create {}", staging.display()))?;
            fs::write(staging.join(PROBE_FILE), PROBE_BYTES)?;
            // The overlay pair first, the payload *after* it. Order is the
            // contract: `rootfs-aarch64/` carries the disk's `/init` - the
            // post-switch pid-1 chain (applet farm, mounts, `exec /sbin/init`) -
            // while a test boot's payload names some of the same paths with its
            // own programs. The payload landing last is what lets `/t` stand
            // beside the script rather than under it; the overlay's remaining
            // files (`etc/inittab`, which the boot's own read pins) still
            // arrive, because nothing in the payload names them.
            crate::rootfs::merge_overlays(
                &crate::utils::project_root()?,
                "rootfs-aarch64",
                &staging,
                &crate::config::RootfsConfig::load(
                    &crate::utils::project_root()?.join("rootfs-aarch64.toml"),
                )?,
                false,
            )?;
            for (name, bytes) in entries {
                stage_program(&staging, name, bytes)?;
            }
            &staging
        }
    };
    lay_ext4(build_dir, tree, disk_bytes, alpine_root.is_some())?;

    Ok(esp)
}

/// Lay `tree` onto a bare ext4 image at `build/aarch64-ext4.img`.
///
/// The features the in-tree driver cannot honour are switched off at creation,
/// not left to be discovered at mount: `has_journal` (nothing here replays a
/// journal, so a crash would leave the filesystem asking for a recovery this
/// kernel cannot do) and `metadata_csum` (every write would leave a checksum it
/// cannot recompute) - the same two the x86_64 root turns off.
///
/// `fakeroot` is for the Alpine tree, and it is not optional there: `mke2fs -d`
/// records each file's owner by *stat`ing it, and the tree was installed under
/// `fakeroot` - so its files are really owned by whoever ran the build and only
/// *appear* root-owned inside a fakeroot session. Running `mke2fs` outside one
/// therefore wrote a root filesystem owned by a uid that is not root, which is
/// wrong in a way that shows up as "login cannot read /etc/shadow": measured,
/// the first Alpine boot's `/lib/librc.so.1` was `mp mp`. Inside a fakeroot
/// session every `stat` answers `0:0` (measured), so the image comes out the way
/// `apk` intended. The test payloads need none of this - their files are the
/// build's own and nobody reads a permission bit off them.
fn lay_ext4(build_dir: &Path, tree: &Path, disk_bytes: u64, fakeroot: bool) -> Result<()> {
    let ext4 = build_dir.join("aarch64-ext4.img");
    let file =
        fs::File::create(&ext4).with_context(|| format!("Failed to create {}", ext4.display()))?;
    file.set_len(disk_bytes)?;
    drop(file);

    // The same `mkfs.ext4` either way; under `fakeroot` it is that program's
    // child rather than this process's, which is the whole of the difference.
    let mkfs = |cmd: &mut Command| {
        cmd.args(["-F", "-q", "-d"])
            .arg(tree)
            .args(["-O", "^has_journal,^metadata_csum"])
            .args(["-b", "4096"])
            .arg(&ext4);
    };
    let mut cmd = if fakeroot {
        let mut c = Command::new("fakeroot");
        c.arg("--").arg("mkfs.ext4");
        c
    } else {
        Command::new("mkfs.ext4")
    };
    mkfs(&mut cmd);
    utils::run_status(&mut cmd, "mkfs.ext4 the aarch64 filesystem image")
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

/// Whether this host can back a virtio-vsock device.
///
/// vsock is the one class the aarch64 boot drives that QEMU does not emulate in
/// userspace: `vhost-vsock-*` is a host kernel backend, and QEMU *refuses to
/// start* a machine that names the device when `/dev/vhost-vsock` is absent. So
/// the device is attached only where the host can carry it, and the boot verdict
/// prints which case it took instead of quietly running one check fewer
/// (`PLAN.md` §13.28). The device's own CID is `4` rather than the first
/// machine's `3`, because the two machines' boots can be up at once and a second
/// guest claiming a live CID aborts QEMU before the guest starts.
fn vsock_available() -> bool {
    Path::new("/dev/vhost-vsock").exists()
}

/// The full `qemu-system-aarch64` command for this machine, with the serial line
/// sent wherever `serial` says (`file:…` for the marker-polling boot, `mon:stdio`
/// for an interactive `run`). Every flag here is a measured machine choice, so
/// boot-aarch64 and `run --arch aarch64` share one definition rather than two
/// lists that could drift into booting different machines.
fn aarch64_qemu(esp: &Path, code: &Path, vars: &Path, serial: &str, share: &Path) -> Command {
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
                "file={},format=raw,if=none,id=blk0,file.locking=off",
                esp.parent().unwrap().join("aarch64-blk.img").display()
            ),
        ])
        .args(["-device", "virtio-blk-device,drive=blk0"])
        // The second block device: the ext4 image `build_esp` laid down. `check_fs`
        // opens the block windows, tries each as an ext4, and finds this one - so a
        // filesystem mounts over the memory-mapped transport (PLAN.md §9.1's disk
        // root). Two virtio-blk-device instances, so the port picks the right one by
        // content rather than by the slot QEMU happened to give it.
        //
        // `file.locking=off` on both raw images, which is what the x86_64 wiring
        // already asks for and for a measured reason: with the default locking, a
        // `qemu-system-aarch64` that outlived the command that started it (a boot
        // killed at its deadline, an interrupted run) holds a write lock on these
        // files, and the *next* boot dies at once with "Failed to get write lock"
        // and an empty serial log - a machine that never started, read as a machine
        // that hung. Nothing else opens these files, so there is no lock to keep.
        .args([
            "-drive",
            &format!(
                "file={},format=raw,if=none,id=blk1,file.locking=off",
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
        // A host-driven console port - the fifth class (`check_console`,
        // `M8_CONSOLE_OK`): a `virtio-serial-device` carrying one `virtconsole`,
        // the same shape the first machine attaches on its PCI bus. Its far end
        // is a host socket nothing has to be listening on (`wait=off`), so the
        // port opens and a write lands in the socket instead of blocking a
        // machine that has nobody reading it yet.
        .args([
            "-chardev",
            &format!(
                "socket,id=console0,path={},server=on,wait=off",
                esp.parent().unwrap().join("aarch64-console.sock").display()
            ),
        ])
        .args(["-device", "virtio-serial-device"])
        .args(["-device", "virtconsole,chardev=console0"])
        // And the display adapter - the sixth class (`check_gpu`, `M8_GPU_OK`).
        // `virtio-gpu-device` is the MMIO form of the card the first machine
        // attaches as `virtio-gpu-pci`, so the driver, the render task and the
        // DRM layer above them are one copy on both machines. A test boot has no
        // `-display`, so the host reports no monitor and the driver builds its
        // headless fallback shape; the scanout is set either way, which is what
        // `/dev/fb0` reads and the framebuffer console paints on.
        .args(["-device", "virtio-gpu-device"])
        // A sound card - the seventh class (`check_sound`, `M8_SOUND_OK`), the
        // same shape the first machine attaches as `virtio-sound-pci`. The
        // backend is `none`: it consumes the stream and plays silence, so the
        // driver is exercised end to end without asking the host for speakers -
        // and the boot's own probe writes silence through it, which is what
        // `M8_SOUND_OK` asserts.
        .args(["-audiodev", "none,id=snd0"])
        .args(["-device", "virtio-sound-device,audiodev=snd0"]);
    // And the eighth class (`check_vsock`, `M8_VSOCK_OK`) - but only on a host
    // that can back it. vsock is the one virtio device QEMU does not emulate in
    // userspace: `vhost-vsock-device` is served by the host's vhost kernel
    // module, and a machine naming it without `/dev/vhost-vsock` does not start
    // at all. Attaching it only when the host has one keeps this machine's boot
    // honest on a bare host - the kernel prints the absence and answers
    // `EAFNOSUPPORT` - while a host that has it runs the class for real
    // (`PLAN.md` §13.28).
    if vsock_available() {
        qemu.args(["-device", "vhost-vsock-device,guest-cid=4"]);
    }
    // The ninth class (`check_p9`, `M8_P9_OK`): the host share, the same
    // arrangement the first machine's `-virtfs` makes. `-fsdev` is the backend -
    // the host directory QEMU's 9p server exports, with `security_model=none` so
    // the host's own permissions decide - and `virtio-9p-device` is the transport
    // the guest opens over MMIO. `mount_tag=hostshare` is the name
    // `mount -t 9p hostshare /mnt` is pointed at, and the probe file `lay_p9_share`
    // laid in `share` is what a boot reads back to prove the share serves.
    qemu.args([
        "-fsdev",
        &format!(
            "local,id=fsdev0,path={},security_model=none",
            share.display()
        ),
    ]);
    qemu.args([
        "-device",
        "virtio-9p-device,fsdev=fsdev0,mount_tag=hostshare",
    ]);
    // And the transports have to be the *modern* ones, which on this machine
    // is a choice somebody has to make: measured, `virt`'s `virtio-mmio`
    // transports answer `Version` 1 - the legacy register layout - unless
    // this says otherwise, and version 2 is the only layout this kernel's
    // driver crate reads. The alternative to selecting it is a kernel that
    // either drives the legacy layout (a second register map, a second
    // feature-negotiation order) or refuses devices on its own machine, and
    // neither is what stage 5 is for.
    qemu.args(["-global", "virtio-mmio.force-legacy=false"]);
    qemu
}

/// Boot the aarch64 machine and hand its serial line to the terminal, staying
/// attached until the guest exits or the user detaches. This is the interactive
/// twin of `boot` (which captures the PL011 to a file and stops at a marker):
/// same ESP, same firmware, same `qemu-system-aarch64`, only the serial goes to
/// stdio and no kill follows.
///
/// What it boots into is the machine's *distribution*: the real Alpine tree
/// `install_alpine_root` lays - pid 1 the disk's `/init`, busybox init reading
/// `/etc/inittab`, openrc's runlevels, getties respawning on the virtual
/// consoles. That is what makes this symmetric with `cargo xtask run` on the
/// first machine: both sessions are the same userland shape, and §13.12–13.16
/// built the second one specifically so this could stop being a stand-in. The
/// in-tree shell `/init` (`aarch64_init::build_shell_init`) that ran here
/// before is deleted rather than left as an escape hatch: a respawning init is
/// a getty's job, and the machine has the userland for one now.
///
/// Two things change with it, both said by the boot itself. `exit` at a prompt
/// logs the session out - getty respawns it; Ctrl-A x is how the machine
/// stops. And the console list follows the run mode exactly as the first
/// machine's does (`alpine_cmdline`): `--mode terminal` renders the login onto
/// the serial node itself, `mirror`/`gui` keep it on the virtual consoles the
/// vcmux carries to the wire.
pub fn run_interactive(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile)?;
    let esp = build_alpine_esp(build_dir, &kernel, &loader, ModeChoice::Session)?;
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
        "booting aarch64 interactively: the Alpine root, pid 1 = /init -> openrc -> \
         getty (log in as root, no password; Ctrl-A x exits QEMU; `--mode terminal` \
         puts the login on the serial line itself)"
    );
    let share = utils::lay_p9_share(&build_dir.join("aarch64-9p-share"))?;
    let mut qemu = aarch64_qemu(&esp, &code, &vars, "mon:stdio", &share);
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
fn boot(build_dir: &Path, esp: &Path, terminators: &[&str], deadline: Duration) -> Result<String> {
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

    let share = utils::lay_p9_share(&build_dir.join("aarch64-9p-share"))?;
    let mut qemu = aarch64_qemu(
        esp,
        &code,
        &vars,
        &format!("file:{}", log.display()),
        &share,
    );
    // QEMU's own stderr goes to this process's, not to a pipe nobody reads. A
    // QEMU that refuses to start - a missing firmware, an image it cannot open -
    // says so there and nowhere else, and the serial log it never wrote is
    // indistinguishable from a machine that hung; that is exactly the failure
    // this used to hide.
    qemu.stdout(Stdio::null()).stderr(Stdio::inherit());
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
        if started.elapsed() > deadline {
            eprintln!("qemu-system-aarch64 was still running after {deadline:?}; stopping it");
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
    use crate::config::{Machine, RunMode, TestConfig};

    /// The byte-for-byte block the Alpine boot printed while this kernel's
    /// procfs had no `/proc/<pid>/environ`, copied out of a captured log.
    ///
    /// It is here rather than paraphrased because the escapes *are* the case:
    /// openrc writes the service's message, then moves the cursor back to the
    /// start of the line and overwrites it with the coloured marker, so a
    /// checker that searched the raw text for the mount error would find
    /// nothing on the marker's line and a checker that searched for `!!` in the
    /// raw text would still have to know it is the failure colour and not the
    /// success one.
    const FAILING_PROC_MOUNT: &str = " \x1b[1m\x1b[32m*\x1b[m Mounting /proc\x1b[K ...\r\r\n\
         mount: mounting proc on /proc failed: Resource busy\r\r\n\
         \x1b[A\x1b[153C \x1b[1m\x1b[34m[\x1b[1m\x1b[31m !! \x1b[1m\x1b[34m]\x1b[m\r\r\n";

    #[test]
    fn an_openrc_service_failure_is_named_by_its_own_message() {
        assert_eq!(
            openrc_failure(FAILING_PROC_MOUNT).as_deref(),
            Some("mount: mounting proc on /proc failed: Resource busy")
        );

        // A healthy runlevel is `None`, and the success marker shares the
        // marker's brackets - which is the whole reason the check looks for the
        // failure colour's text and not for a bracket.
        let healthy = " \x1b[1m\x1b[32m*\x1b[m Starting local\x1b[K ...\r\r\n\
             \x1b[A\x1b[153C \x1b[1m\x1b[34m[\x1b[1m\x1b[32m ok \x1b[1m\x1b[34m]\x1b[m\r\r\n\
             Welcome to Alpine Linux 3.25.0\r\r\nmpos login: ";
        assert_eq!(openrc_failure(healthy), None);

        // And the escapes really are stripped for the message a person reads,
        // rather than merely skipped over.
        assert_eq!(
            without_ansi("a\x1b[1mb\x1b[0m\r\nc"),
            "ab\nc",
            "CSI sequences and carriage returns go, everything else stays"
        );
    }

    #[test]
    fn alpine_boot_consoles_follow_the_run_mode() {
        // The session's console list is the x86_64 path's own answer - one
        // `RunMode`, three lists (`config.rs`'s `cmdline_consoles`) - spelled
        // with this machine's serial name. The two spellings of that name are
        // pinned to each other here: `[rootfs] serial_port` in
        // `rootfs-aarch64.toml` (which `render_inittab` writes into the getty
        // line) and `SERIAL_PORT` (which the boot bakes into `console=`), plus
        // the kernel node at `src/drivers/devices.rs`. If they ever disagree,
        // the login answers on a node the wire never names.
        for (mode, want) in [
            (RunMode::Terminal, "console=ttyAMA0"),
            (RunMode::Mirror, "console=ttyAMA0 console=tty0"),
            (RunMode::Gui, "console=tty0"),
        ] {
            unsafe { std::env::set_var("MPOS_XTASK_MODE", mode.name()) };
            // The written file always ends in exactly one newline: the loader
            // reads `mpos.cmdline` whole and the kernel records it verbatim.
            // And every session line carries the root the script pivots to -
            // without `root=` the initramfs stays on the ramfs and the disk is
            // never reached (§15.6), so the consoles are only half the answer.
            assert_eq!(
                alpine_cmdline(ModeChoice::Session).unwrap(),
                format!("{want} root=/dev/vda rootfstype=ext4\n")
            );
        }
        unsafe { std::env::remove_var("MPOS_XTASK_MODE") };

        let config = crate::config::RootfsConfig::load(
            &utils::project_root().unwrap().join("rootfs-aarch64.toml"),
        )
        .unwrap();
        assert_eq!(
            config.serial_port, SERIAL_PORT,
            "the toml's serial port and the boot wiring must name one node"
        );

        // The verdict's fixed line carries no selftest word and no framebuffer:
        // its gate reads `login:` off the wire alone.
        assert_eq!(
            alpine_cmdline(ModeChoice::Verdicts).unwrap(),
            ALPINE_CMDLINE
        );

        // And the getty rule is the window rule: only a session with no screen
        // to log into needs the login on the serial node.
        assert!(serial_getty_for(RunMode::Terminal));
        assert!(!serial_getty_for(RunMode::Mirror));
        assert!(!serial_getty_for(RunMode::Gui));
    }

    #[test]
    fn the_alpine_install_stamp_keys_on_the_trees_it_installs_from() {
        // The counterfactual for `run --arch aarch64` after every kernel edit:
        // a fresh build re-lays the disk from the staged tree without touching
        // the network, so the stamp may *not* list the kernel or the loader.
        // What it must list is everything `install_alpine_root` reads: the
        // toml and both overlay trees.
        let inputs = [
            "rootfs-aarch64.toml",
            crate::rootfs::CORE_OVERLAY,
            "rootfs-aarch64",
        ];
        for name in inputs {
            assert!(
                utils::project_root().unwrap().join(name).exists(),
                "the install reads {name}, so the stamp must too"
            );
        }
        // A file touched inside the core tree makes the installed tree stale -
        // the check `alpine_root_is_stale` runs against a real stamp.
        let dir = std::env::temp_dir().join("mpos-aarch64-stamp-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("build")).unwrap();
        fs::create_dir_all(dir.join("root/core")).unwrap();
        fs::write(dir.join("root/core/f"), "x").unwrap();
        fs::write(dir.join("root/toml"), "x").unwrap();
        fs::create_dir_all(dir.join("build/aarch64-alpine-root")).unwrap();
        fs::write(dir.join("build/aarch64-alpine-root.stamp"), "old").unwrap();
        // Backdate the stamp, then touch a core file: stale.
        let older = std::time::SystemTime::now() - Duration::from_secs(60);
        let f = fs::File::options()
            .write(true)
            .open(dir.join("build/aarch64-alpine-root.stamp"))
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(older))
            .unwrap();
        drop(f);
        filetime_touch(&dir.join("root/core/f")).unwrap();
        assert!(
            newer_check(
                &dir.join("root/core"),
                &dir.join("build/aarch64-alpine-root.stamp")
            ),
            "a file added under the core tree is an input the stamp knows"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `utils::dir_newer_than` through a stamp, the way `alpine_root_is_stale`
    /// uses it - factored out so the test above checks the same predicate the
    /// build runs, not a copy of it.
    fn newer_check(tree: &Path, stamp: &Path) -> bool {
        utils::dir_newer_than(tree, utils::file_mtime(stamp).unwrap()).unwrap()
    }

    fn filetime_touch(path: &Path) -> Result<()> {
        let now = std::time::SystemTime::now();
        let f = fs::File::options()
            .write(true)
            .open(path)
            .context("touch")?;
        f.set_times(std::fs::FileTimes::new().set_modified(now))?;
        Ok(())
    }

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
        assert_eq!(missing_programs(&gaps, &built), vec!["t-gone".to_string()]);
        assert!(missing_programs(&gaps, &["t-here".to_string(), "t-gone".to_string()]).is_empty());

        // And the repository's own file: the group exists and every gap it names
        // is a program the payload carries. The payload is what `cargo` builds,
        // which a unit test does not run; what it can check is that the arm's
        // validation is consistent with the file it reads, and empty - the
        // healthy answer - is a state this asserts as a property, not a count.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let config = TestConfig::load(&root.join("tests.toml")).unwrap();
        let machine = Machine::Aarch64;
        assert!(
            config.group_names(machine).contains(&"aarch64"),
            "the second machine's gap group is gone rather than empty"
        );
        let gaps: Vec<String> = config
            .machine_tests(machine)
            .into_iter()
            .map(|(_, test)| test.to_string())
            .collect();
        assert!(gaps_without_programs(&config, machine, &gaps).is_empty());
    }
}
