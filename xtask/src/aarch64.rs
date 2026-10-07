//! `cargo xtask boot-aarch64` / `alpine-aarch64` / `run --arch aarch64`: what
//! the *second machine's* three entry points still own.
//!
//! They own as little as possible. Once this machine booted off the same GPT
//! disk, through the same loader, into the same initramfs-script-then-disk-root
//! chain as x86_64 (§15.18), almost everything these commands do became the
//! first machine's own steps, so those steps live in the shared modules -
//! `disk` lays the media, `qemu` starts the emulator, `selftest` runs a sweep,
//! `rootfs` installs Alpine, `arch` answers what the machine is called - and
//! what is left here is only what a second machine genuinely has of its own:
//!
//! - its console word and its deadline ([`CMDLINE`], [`ALPINE_CMDLINE`],
//!   [`DEADLINE`]), which no other machine can supply for it;
//! - the marker list a boot has to reach ([`TERMINATORS`]) and the verdict read
//!   off it ([`run`], [`run_alpine`]) - the markers are the machine's own
//!   because they are what its bring-up stages print;
//! - the install step ([`build_alpine_esp`]'s `ensure_alpine_root`), the one
//!   place where the architecture really does change what happens: Alpine
//!   packages are per-`$ARCH`, so this is the one command that cannot be
//!   shared.
//!
//! The three commands are thin on purpose: each builds its payload, calls
//! [`boot_and_capture`] and reads the log. A shared command would have to
//! parameterise the payload, and the payload *is* the difference between a
//! verdict, a sweep and a session.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

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
///
/// `root=/dev/vda2`: the root is the GPT disk's second partition, named the way
/// the block layer names every disk's partitions - `/dev/vda` is the whole disk
/// and `/dev/vdaN` is one partition - which is the same naming the first
/// machine's image has always spelled (`PLAN.md` §15.18).
const CMDLINE: &str = "console=ttyAMA0 mpos.selftest root=/dev/vda2 rootfstype=ext4\n";

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
/// sources (`machine.toml`'s `qemu.mode`, or `MPOS_XTASK_MODE` when
/// `run --mode X` hands the desired mode down): `RunMode::cmdline_consoles`
/// spells `console=ttyAMA0 [console=tty0]` for terminal/mirror/gui, and since
/// §13.16 the vcmux carries each virtual console's bytes over the wire on this
/// machine too, so the list means exactly what it means on x86_64. The headless
/// verdicts have no mode to honour - they are not sessions - and pass
/// [`ModeChoice::Verdicts`], which keeps the single serial word they always had.
const ALPINE_CMDLINE: &str = "console=ttyAMA0 root=/dev/vda2 rootfstype=ext4\n";

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
/// disk the boot never reaches. The x86_64 image spells the same key the same
/// way: `/dev/vda2` is the GPT disk's root partition on either machine.
fn alpine_cmdline(choice: ModeChoice) -> Result<String> {
    let mode = match choice {
        ModeChoice::Verdicts => return Ok(ALPINE_CMDLINE.to_string()),
        ModeChoice::Session => crate::rootfs::effective_mode()?,
    };
    Ok(format!(
        "{} root=/dev/vda2 rootfstype=ext4\n",
        mode.cmdline_consoles(SERIAL_PORT)
    ))
}

/// This machine's serial console, spelled the way its psABI names the PL011.
///
/// Answered by [`Arch::console`] rather than written here: the same node is
/// named by the kernel's driver, `rootfs.toml`'s `serial_port` and the
/// `console=` word below, and one answer for all three is what lets the two
/// machines' boot wiring be one body.
const SERIAL_PORT: &str = Arch::Aarch64.console();

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
/// confirmation) is the proof that a scheduled program reached the console, and
/// the **login prompt** the disk's own pid-1 chain gets to is the terminator.
///
/// `login:` and not `INIT_EXITED`: while this machine's disk carried a payload
/// of its own - a hand-assembled `/t` and a static-musl helper whose
/// `exit_group` the scheduler reaped - the init *exited* and the kernel printed
/// that. Both machines now boot a real root instead, whose pid 1 is an init that
/// stays: the last thing this machine says is the prompt, which is the same
/// verdict `alpine-aarch64` reads and the same place a boot ends on the first
/// machine. `M6_INIT_OK` prints before the wait, so it is not the terminator. Stage 7's virtio-rng
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
    "login:",
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

/// What a boot leaves behind.
///
/// Just the log, because that is the whole of what a boot's evidence is:
/// [`boot_and_capture`] also prints the log as it goes and prints the media it
/// laid, so a caller that wants only the verdict reads one field and nothing
/// else has to be threaded back.
pub(crate) struct Boot {
    pub log: String,
}

/// Boot `arch` and return the log it produced.
///
/// Every command here starts the same way - build the pair, say how big they
/// are, lay the boot media, start the machine - so that is one function, and
/// what a caller adds is what its own boot is *for*. The `entries` and the
/// command line are the caller's: a verdict boot carries no payload and the
/// fixed selftest word, a sweep carries its programs, an Alpine boot carries the
/// installed tree's own command line. Those inputs travel as one
/// [`crate::disk::Boot`] because they are one boot's, and a caller that spelled
/// two of them from two different boots would get a disk mixing both; the two
/// that are the *run's* rather than the disk's - the terminators a log is read
/// to, and how long it is given - stay arguments.
pub(crate) fn boot_and_capture(
    build_dir: &Path,
    profile: &str,
    arch: Arch,
    boot: &crate::disk::Boot<'_>,
    terminators: &[&str],
    deadline: Duration,
) -> Result<Boot> {
    let (kernel, loader) = build(profile, arch)?;
    println!("kernel: {} ({:.1} KB)", kernel.display(), kib(&kernel)?);
    println!("loader: {} ({:.1} KB)", loader.display(), kib(&loader)?);
    let esp = crate::disk::boot_media(
        build_dir,
        arch,
        &crate::disk::Boot {
            kernel: &kernel,
            loader: &loader,
            ..*boot
        },
    )?;
    println!("esp:    {}", esp.display());
    let log = crate::qemu::boot(build_dir, arch, &esp, terminators, deadline)?;
    print!("{log}");
    Ok(Boot { log })
}

pub fn run(build_dir: &Path, profile: &str) -> Result<()> {
    let Boot { log } = boot_and_capture(
        build_dir,
        profile,
        Arch::Aarch64,
        &crate::disk::Boot {
            kernel: Path::new(""),
            loader: Path::new(""),
            entries: &[],
            disk_bytes: DISK_BYTES,
            root: None,
            cmdline: CMDLINE,
        },
        &TERMINATORS,
        DEADLINE,
    )?;

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
            "root switch: the initramfs script mounted /dev/vda2 - the disk's root \
             partition, the same name the first machine's script mounts - and promoted it \
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
    if crate::qemu::vsock_available() {
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
        (
            "login:",
            "the disk root's pid-1 chain reached a login prompt",
        ),
    ]);
    let missing: Vec<&str> = markers
        .iter()
        .filter(|(marker, _)| !log.contains(marker))
        .map(|(_, what)| *what)
        .collect();
    // The disk root used to be pinned by a *payload*: a hand-assembled `/t` read
    // `/etc/inittab` through libc and printed its byte count, and that number was
    // the check. Those payloads are gone - this machine's boot no longer carries
    // a program the first machine's does not - so what proves a userspace program
    // read this root is the sweep, where a real musl program actually runs
    // (`cargo xtask test --arch aarch64`). What stays here is the machine's own
    // chain: the kernel's markers, the script's switch, and a login prompt.
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
    if missing.is_empty() {
        // The summary must not claim the vsock class on a host that could not back
        // it: its marker is required only when the device was attached, so the
        // parts of the sentence describing it are built from the same decision.
        let vsock_marker = if crate::qemu::vsock_available() {
            "M8_VSOCK_OK, "
        } else {
            ""
        };
        let device_count = if crate::qemu::vsock_available() {
            "nine"
        } else {
            "eight"
        };
        let vsock_device = if crate::qemu::vsock_available() {
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
             (MPOS_SWITCH_ROOT_OK), and the login prompt its pid 1 got to - the machine \
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
             over the block (the disk root), and a boot whose init chain is the \
             one both machines run - the script that echoed and switched root, \
             then the disk's own `/init` - \
             from the ESP under AAVMF"
        );
        return Ok(());
    }
    bail!(
        "the aarch64 boot did not reach every marker; nothing printed for: {}; \
         the log above and {} say where it stopped",
        missing.join(", "),
        crate::qemu::serial_log(build_dir, Arch::Aarch64).display()
    )
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
/// is stamped (`rootfs::ensure_alpine_root`): a fresh kernel re-lays the disk
/// from the existing staging tree without touching the network, exactly the
/// way x86_64's `run` updates boot files without reinstalling Alpine.
fn build_alpine_esp(
    build_dir: &Path,
    kernel: &Path,
    loader: &Path,
    mode: ModeChoice,
) -> Result<PathBuf> {
    crate::rootfs::ensure_alpine_root(build_dir, Arch::Aarch64)?;
    let alpine_root = crate::rootfs::alpine_staging(build_dir, Arch::Aarch64);
    let esp = crate::disk::boot_media(
        build_dir,
        Arch::Aarch64,
        &crate::disk::Boot {
            kernel,
            loader,
            entries: &[],
            disk_bytes: ALPINE_DISK_BYTES,
            root: Some(&alpine_root),
            cmdline: &alpine_cmdline(mode)?,
        },
    )?;
    // A session's getty choice is re-asserted on the staging tree *after* the
    // install stamp has had its way with it: `run --mode terminal` must find
    // the login on the wire even when the tree was installed under a windowed
    // default, and the refresh is one file rather than a reinstall. The
    // verdicts keep the installed answer - their gate reads `login:` through
    // the mux either way.
    if mode == ModeChoice::Session {
        let config = crate::config::RootfsConfig::load(
            &utils::project_root()?.join("rootfs.toml"),
            crate::arch::Arch::Aarch64,
        )?;
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
    let Boot { log } = boot_and_capture_alpine(build_dir, profile, ModeChoice::Verdicts)?;

    // openrc's own verdict, which is a stricter answer than `login:`. A service
    // that fails still prints `[ !! ]` and the runlevel carries on, so the
    // prompt is reached either way - and that is exactly how the fourth such
    // failure hid: `mount: mounting proc on /proc failed: Resource busy`, from
    // a procfs missing `/proc/<pid>/environ`, sat in the log while this command
    // reported success. The three §13.12/§13.14/§13.15 chased down (`hwclock`,
    // `modprobe`, `syslog`) all read the same way, so the marker is named here
    // rather than left to whoever reads the log next.
    if let Some(detail) = crate::rootfs::openrc_failure(&log) {
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

/// The Alpine boot's own shape of [`boot_and_capture`]: the installed tree as
/// the root, its `mode`'s command line, and the emulator's stderr left on this
/// process's (an Alpine boot's console list is the caller's, not a fixed one).
fn boot_and_capture_alpine(build_dir: &Path, profile: &str, mode: ModeChoice) -> Result<Boot> {
    let (kernel, loader) = build(profile, Arch::Aarch64)?;
    println!("kernel: {} ({:.1} KB)", kernel.display(), kib(&kernel)?);
    println!("loader: {} ({:.1} KB)", loader.display(), kib(&loader)?);
    let esp = build_alpine_esp(build_dir, &kernel, &loader, mode)?;
    println!("esp:    {}", esp.display());
    let log = crate::qemu::boot(
        build_dir,
        Arch::Aarch64,
        &esp,
        &ALPINE_TERMINATORS,
        ALPINE_DEADLINE,
    )?;
    print!("{log}");
    Ok(Boot { log })
}

/// Boot the kernel and its loader for `arch`.
///
/// The build itself is `build_kernel` with `arch` - the same call every command
/// makes, so the pair of binaries and the targets they were compiled for cannot
/// drift apart between `cargo xtask build --arch aarch64` and this command.
/// What stays here is the pair of paths those two binaries land on.
fn build(profile: &str, arch: Arch) -> Result<(PathBuf, PathBuf)> {
    utils::build_kernel(profile, arch)?;
    Ok((
        utils::kernel_path(profile, arch)?,
        utils::loader_path(profile, arch)?,
    ))
}

/// Boot the machine into its Alpine userland and hand the serial line to the
/// terminal, staying attached until the guest exits or the user detaches. This
/// is the interactive twin of [`run_alpine`] (which captures the log and stops
/// at a marker): same disk, same firmware, same emulator, only the wire goes to
/// stdio and no kill follows.
///
/// What it boots into is the machine's *distribution*: the real Alpine tree
/// [`install_alpine_root`] lays - pid 1 the disk's `/init`, busybox init
/// reading `/etc/inittab`, openrc's runlevels, getties respawning on the
/// virtual consoles. That is what makes this symmetric with `cargo xtask run`
/// on the first machine: both sessions are the same userland shape, and
/// §13.12-13.16 built the second one specifically so this could stop being a
/// stand-in.
///
/// The console list follows the run mode exactly as the first machine's does
/// ([`alpine_cmdline`]): `--mode terminal` renders the login onto the serial
/// node itself, `mirror`/`gui` keep it on the virtual consoles the vcmux
/// carries to the wire. Ctrl-A x is how the machine stops.
pub fn run_interactive(build_dir: &Path, profile: &str) -> Result<()> {
    let (kernel, loader) = build(profile, Arch::Aarch64)?;
    let esp = build_alpine_esp(build_dir, &kernel, &loader, ModeChoice::Session)?;
    println!(
        "booting aarch64 interactively: the Alpine root, pid 1 = /init -> openrc -> \
         getty (log in as root, no password; Ctrl-A x exits QEMU; `--mode terminal` \
         puts the login on the serial line itself)"
    );
    crate::qemu::run_interactive(build_dir, Arch::Aarch64, &esp)
}

/// The size of a built binary, in kibibytes, for the line a boot prints.
fn kib(path: &Path) -> Result<f64> {
    Ok(fs::metadata(path)?.len() as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RunMode;
    use anyhow::Context;

    #[test]
    fn alpine_boot_consoles_follow_the_run_mode() {
        // The session's console list is the x86_64 path's own answer - one
        // `RunMode`, three lists (`config.rs`'s `cmdline_consoles`) - spelled
        // with this machine's serial name. The two spellings of that name are
        // pinned to each other here: `rootfs.serial_port` in
        // `rootfs.toml`'s `[aarch64]` half (which `render_inittab` writes into the getty
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
                format!("{want} root=/dev/vda2 rootfstype=ext4\n")
            );
        }
        unsafe { std::env::remove_var("MPOS_XTASK_MODE") };

        let config = crate::config::RootfsConfig::load(
            &utils::project_root().unwrap().join("rootfs.toml"),
            crate::arch::Arch::Aarch64,
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
        let inputs = ["rootfs.toml", crate::rootfs::CORE_OVERLAY, "rootfs-aarch64"];
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
}
