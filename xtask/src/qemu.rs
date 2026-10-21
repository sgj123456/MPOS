//! The `run` command: boot the image the build just made. This is
//! `scripts/run.sh` as a command.
//!
//! QEMU runs as the user - the guest console is watched from this terminal, and
//! booting the machine as root buys nothing - and nothing here needs root at
//! all. The one exception is the first run on a tree with no image yet: there is
//! a build to do, and that is handled before this is reached.

use std::env;
use std::ffi::CString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::arch::Arch;
use crate::config::{QemuConfig, RunMode};
use crate::utils;

/// What the machine boots from, which is the one thing an image run and a user
/// test run do differently.
pub enum Boot {
    /// The assembled image, as a virtio disk the guest can write.
    Image(PathBuf),
    /// A directory exported over QEMU's virtual FAT, read-only - what
    /// `xtask test` boots when there is no image and no root filesystem, just
    /// an ESP full of test programs.
    Esp(PathBuf),
}

/// Where the guest's serial line and framebuffer go.
pub enum Console {
    /// An interactive `run`, laid out per [`RunMode`]: which of the window, the
    /// serial terminal and the serial log file actually shows the console.
    Run(RunMode),
    /// A file, which is what a boot whose verdict is read out of the transcript
    /// uses. Always headless: no window, the whole console on the wire to the
    /// log.
    File(String),
}

/// Where this crate records which [`RunMode`] is baked into the image's
/// `mpos.cmdline`. The guest learns its console policy from that file, which
/// lives inside the disk image; the marker lets `run` decide, without mounting
/// anything as root, whether the image already says what the user asked for.
///
/// One marker per machine, with the first machine keeping the historic
/// `console.mode` name: it is a cache, not an interface, and a new name would
/// read as "nothing baked" and make every existing image look as if it needed a
/// root rebuild to say what it already does (`Arch::mode_marker`).
fn mode_marker_path(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(arch.mode_marker())
}

/// The mode last baked into the image, or `None` for an image that carries no
/// marker the current xtask recognises - one built before the run modes existed,
/// or one baked when `weston` was a mode word and is not one any more.
///
/// A missing marker is deliberately not read as [`RunMode::Mirror`]: such an
/// image's command line says `console=ttyS0` and nothing else, and the kernel
/// now reads that the way Linux does - the wire is the *only* console. A mirror
/// image has to name the framebuffer console too, so the two are different
/// machines and the caller gets to decide what to do about it, rather than being
/// told a mode the image does not carry. A retired word reads the same way, and
/// costs the same one boot-file refresh - which now genuinely rewrites
/// `mpos.cmdline`: `weston` baked `console=ttyS0`, the line that later `gui`
/// absorbed and that today's `gui` (`console=tty0`) replaces, so the refresh
/// moves the guest's console onto the screen rather than only renaming the
/// marker.
pub fn baked_mode(build_dir: &Path, arch: Arch) -> Option<RunMode> {
    fs::read_to_string(mode_marker_path(build_dir, arch))
        .ok()
        .and_then(|text| RunMode::parse(&text))
}

pub fn set_baked_mode(build_dir: &Path, arch: Arch, mode: RunMode) -> Result<()> {
    fs::write(mode_marker_path(build_dir, arch), mode.name())
        .with_context(|| format!("Failed to record the baked mode in {}", build_dir.display()))
}

/// Whether the mode baked into the image differs from `desired`, so a `run` must
/// take root to rewrite `mpos.cmdline` before booting. No image means nothing to
/// switch (the build that makes it will bake `desired` anyway), and an image with
/// no marker at all differs from every mode - its line predates the console list.
pub fn needs_mode_switch(build_dir: &Path, arch: Arch, desired: RunMode) -> bool {
    utils::image_path(build_dir, arch).exists() && baked_mode(build_dir, arch) != Some(desired)
}

/// Where one boot's console goes on the host: the `-display` backend and the
/// serial line's sink.
///
/// One decision for both machines, because it answers one question - is the
/// window showing the console - and QEMU spells the answer the same way for
/// either. What is per-machine is which `-device` list the answer is attached
/// to, and that is the builders' business.
struct Wiring {
    /// The `-display` argument's value.
    display: String,
    /// Where the wire is read.
    serial: SerialSink,
}

/// Where a boot's serial line is read from.
enum SerialSink {
    /// The launcher's terminal, through a chardev with `mux=off` so the guest
    /// owns `Ctrl-A` (the kernel's own console switching) rather than QEMU's
    /// escape, and `signal=off` so `Ctrl-C` reaches the guest's line discipline.
    Stdio,
    /// A file, which is what a boot whose verdict is read out of the transcript
    /// uses.
    File(PathBuf),
}

/// The wiring `console` asks for, given the machine's configured display.
///
/// `gui` moves the wire into a log file because the window *is* the console in
/// that mode - and that is exactly why the move is conditional on a window
/// actually being drawn: a machine whose `qemu.display` is `none` has no window
/// to be the console, so its wire stays on the launcher's terminal in every mode
/// (`machine.toml`'s `[aarch64]` names no window backend at all, and the PL011
/// session is what a person watches).
fn wiring(console: &Console, config: &QemuConfig, build_dir: &Path) -> Wiring {
    // A missing `display` is `sdl` for a mode that wants a window - the
    // interactive default - and `none` for a boot that renders nowhere.
    let configured = config.display.clone().unwrap_or_else(|| "sdl".to_string());
    match console {
        Console::Run(mode) => {
            // A mode with a window takes the configured backend; one without
            // renders nowhere, and then the *card* decides what QEMU needs - a GL
            // card refuses `none` outright, so a `terminal` boot of one has to
            // name the headless GL backend instead.
            let display = if mode.has_window() {
                gl_for_card(&configured, &config.gpu)
            } else {
                windowless_display(&config.gpu)
            };
            let window_shows_the_console = *mode == RunMode::Gui && display != "none";
            let serial = if window_shows_the_console {
                // The wire still goes somewhere, and nowhere visible would be the
                // old `gui` that dropped the kernel log with the port: the UEFI
                // loader prints to the port unconditionally, the kernel's banner
                // precedes the command-line parse, and a panic's `force_serial`
                // report reaches no screen. The log lives beside the share, not
                // inside it - the share is exported over 9p, and a host-side
                // transcript has no business appearing in the guest's
                // `/mnt/share`.
                SerialSink::File(build_dir.join("serial.log"))
            } else {
                SerialSink::Stdio
            };
            Wiring { display, serial }
        }
        Console::File(log) => Wiring {
            display: windowless_display(&config.gpu),
            serial: SerialSink::File(PathBuf::from(log)),
        },
    }
}

/// The display argument for a boot that renders nowhere - a `terminal`/`mirror`
/// run whose console is the wire, or a `test`/sweep boot, which has no window at
/// all. `none` was the whole of that answer, and a card that needs a GL context
/// refuses it, so the headless GL backend is derived from the card rather than
/// written down at each of the two places that need it.
fn windowless_display(gpu: &str) -> String {
    if needs_gl_context(gpu) {
        "egl-headless,gl=on".to_string()
    } else {
        "none".to_string()
    }
}

/// The display backend for a boot that *does* draw a window, given the card.
///
/// A GL card refuses a backend without OpenGL (`needs_gl_context`), and QEMU's
/// own error names the fix: `-display BACKEND,gl=on`. The windowed path used to
/// pass the configured backend verbatim, so a machine with `qemu.gpu =
/// "virtio-gpu-gl-pci"` and no `qemu.display` - which defaults to `sdl` - was
/// refused at QEMU's hand before the guest started, and the same refusal met a
/// `mirror`/`gui` run of that machine. So the context is derived from the card
/// here too, exactly as `windowless_display` derives it for a boot that renders
/// nowhere: the key stays about *which* window, and the card answers whether it
/// must carry a GL context. A backend that already names `gl=` is left alone, so
/// an explicit `display = "sdl,gl=core"` says what it means.
fn gl_for_card(display: &str, gpu: &str) -> String {
    if needs_gl_context(gpu) && !display.contains("gl=") {
        format!("{display},gl=on")
    } else {
        display.to_string()
    }
}

/// Whether a card needs a GL context from the display backend.
///
/// QEMU spells the GL variant of every virtio GPU with `-gl` in its device name
/// (`virtio-gpu-gl-pci`, `virtio-vga-gl`, the MMIO `virtio-gpu-gl-device`), and
/// QEMU refuses one beside a backend without OpenGL - `none` included, which is
/// the answer for a boot that renders nowhere. So that answer has to come off the
/// card: the only question left is whether the card it was wired with needs the
/// context.
fn needs_gl_context(gpu: &str) -> bool {
    gpu.contains("-gl")
}

/// The QEMU command line both `run` and `test` boot with.
///
/// One builder, because the two differed only in what they put on the disk and
/// where the serial line went - and the rest (the firmware, the machine type,
/// the network) drifting apart between them is a difference nobody asked for.
/// The sizes and the name come from `qemu.*` in `machine.toml`, and the name
/// again from `system.hostname` in `rootfs.toml` (which is where the guest is
/// told it).
///
/// `firmware_code` and `vars` are the two flash banks: the read-only code the
/// machine boots from ([`find_firmware`] answered where it is) and the writable
/// variable store, which the caller has already copied into the build directory.
///
/// `share_dir` is where the 9p share is exported from and where the console
/// port's host socket lives - both under the build directory, both made
/// before QEMU starts, so a device that needs a host-side file never finds
/// it missing.
pub fn machine_args(
    config: &QemuConfig,
    firmware_code: &Path,
    vars: &Path,
    boot: Boot,
    console: &Console,
    share_dir: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-machine".into(),
        "q35".into(),
        "-accel".into(),
        detect_accel(),
        "-m".into(),
        config.memory.clone(),
    ];

    // A second processor is worth saying only when there is one: `-smp 1` is
    // QEMU's default and the setting the kernel's bring-up code reads as
    // "one processor, nothing to start".
    if config.cpus > 1 {
        args.push("-smp".into());
        args.push(config.cpus.to_string());
    }

    args.extend([
        "-drive".into(),
        format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            firmware_code.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
    ]);

    match &boot {
        Boot::Image(image) => args.extend([
            "-drive".into(),
            format!(
                "file={},format=raw,if=none,id=hd0,file.locking=off",
                image.display()
            ),
            "-device".into(),
            "virtio-blk-pci,drive=hd0".into(),
        ]),
        // `readonly=on` is not decoration: QEMU's virtual FAT cannot be
        // written, and a virtio-blk device refuses a backend it was not told
        // is read-only (`Block node is read-only`) - the machine never
        // starts, and every test reports as failed. Say so up front and the
        // disk is a perfectly good ESP: the loader only ever reads it.
        Boot::Esp(esp) => args.extend([
            "-drive".into(),
            format!(
                "file=fat:{},format=raw,if=none,id=hd0,readonly=on",
                esp.display()
            ),
            "-device".into(),
            "virtio-blk-pci,drive=hd0".into(),
        ]),
    }

    // And a *second* disk on a real controller, which the self-test machine
    // always gets: the ESP above arrives through virtio, and this one arrives
    // through an AHCI controller, so the machine the `t-ahci` pins run on has
    // two disks from two drivers (`NOTES.md` §7.3). The image lives beside the
    // ESP, because `Boot::Esp` carries the ESP's own path and nothing else -
    // its directory is the one fact both ends already agree on.
    //
    // Attached unconditionally rather than "if the file exists": a test machine
    // that is supposed to have a SATA disk and quietly does not would report
    // `t-ahci` failing for a reason that is not the driver's.
    if let Boot::Esp(esp) = &boot {
        let disk = crate::disk::sata_image(esp.parent().expect("the ESP path has a directory"));
        args.extend([
            "-drive".into(),
            format!("file={},format=raw,if=none,id=sd0", disk.display()),
            "-device".into(),
            "ich9-ahci,id=ahci".into(),
            "-device".into(),
            "ide-hd,drive=sd0,bus=ahci.0".into(),
        ]);
    }

    // The name the guest's DHCP client reports, and the network the socket
    // tests are written against: `/init` brings eth0 up with the address slirp
    // expects, and `t-socket`, `t-tcp` and `t-udp` then talk to the host
    // through it. No `dns=` ever goes here. QEMU's option of that name is the
    // address slirp *answers* DNS at - the 10.0.2.3 the guest's resolv.conf
    // and `t-dns` both name - not an upstream to forward to: the forwarding
    // behind it always follows the host's own resolver, and moving the
    // address strands every query the guest sends. Naming a different server
    // is `system.dns` in rootfs.toml, which the rootfs build writes into
    // the guest's resolv.conf instead.
    let netdev = format!("user,id=n0,hostname={}", config.hostname);
    args.extend([
        "-netdev".into(),
        netdev,
        "-device".into(),
        "virtio-net-pci,netdev=n0".into(),
    ]);

    // The rest of the virtio devices the kernel drives, each one probed and
    // reported by the boot: the entropy source, the GPU, a keyboard, and a
    // console port. None is required by the guest - a missing one boots
    // without it - but the ones a boot verifies are the ones attached.
    //
    // The GPU is the machine's choice rather than this function's: the 2D
    // `virtio-gpu-pci` is the default and needs nothing of the host, while the
    // GL card named in `machine.toml` is what puts a 3D context within reach
    // (`NOTES.md` §7.4). Which one it is changes only this word, so the rest of
    // the machine - and the kernel's own device probe - is the same either way.
    args.extend([
        "-device".into(),
        "virtio-rng-pci".into(),
        "-device".into(),
        config.gpu.clone(),
        "-device".into(),
        "virtio-keyboard-pci".into(),
        // A pointer, so a compositor has somewhere to route the mouse. The
        // tablet is absolute (a host cursor position maps straight onto it),
        // which is what a windowed display feeds, so it is the form that
        // actually moves a compositor's pointer rather than a relative mouse the
        // SDL window would have to grab and release.
        "-device".into(),
        "virtio-tablet-pci".into(),
        // The console port rides a `virtio-serial` card, and its far end is
        // a host socket a test - or a curious operator - can hold. Until
        // something connects, the port is exactly what it says it is: a
        // console with no reader, whose writes go nowhere and whose reads
        // answer nothing. `wait=off` keeps the machine from blocking on a
        // listener that may never come.
        "-chardev".into(),
        format!(
            "socket,id=console0,path={},server=on,wait=off",
            share_dir.join("console.sock").display()
        ),
        "-device".into(),
        "virtio-serial-pci".into(),
        "-device".into(),
        "virtconsole,chardev=console0".into(),
        // The host share, under the tag the guest's `mount -t 9p hostshare`
        // names. The directory is made before QEMU starts; an empty one is a
        // share that lists empty, not a failed boot. `security_model=none`
        // is the model a teaching kernel wants: the host's own permissions
        // decide, and QEMU invents neither a chroot nor a mangled-uid
        // mapping between the two sides.
        "-virtfs".into(),
        format!(
            "local,path={},mount_tag=hostshare,security_model=none",
            share_dir.display()
        ),
    ]);

    // The two devices that depend on the host having something to give.
    // Both are opt-in through `qemu.vsock`/`qemu.audio` because a host without
    // what they need refuses to start QEMU outright - and a machine that never
    // boots is worse than a machine with no vsock and no sound.
    if config.vsock {
        // `guest-cid=3` is the guest's address on the host's vsock bus; the
        // host itself answers on CID 2. The device's QEMU name is
        // `vhost-vsock-pci` - the vhost backend is the device, which is why
        // the name is not the virtio one the other devices carry.
        args.extend(["-device".into(), "vhost-vsock-pci,guest-cid=3".into()]);
    }
    if config.audio {
        // The `none` backend consumes the stream and plays silence - the
        // driver is exercised end to end without asking the host for
        // speakers.
        args.extend([
            "-audiodev".into(),
            "none,id=snd0".into(),
            "-device".into(),
            "virtio-sound-pci,audiodev=snd0".into(),
        ]);
    }

    // One PCIe function for the extended configuration space to be asked
    // about, on the test machine only. Every device q35 attaches by itself is
    // conventional PCI - measured on the gate's own machine, 2026-10-19: none
    // of its fourteen functions carries capability id 0x10 - so without this
    // the 4096-byte path has no device to answer for it and `t-pci` could pin
    // only the 256 side.
    //
    // A bare `pcie-root-port` is not enough, measured the same day: QEMU
    // answers its 0x100 with the standard header repeated, which is the
    // aliasing case Linux refuses (`pci_ext_cfg_is_aliased`) and this kernel
    // rightly keeps at 256. An NVMe controller is the device QEMU gives a real
    // extended space to - AER at 0x100 - and with no `drive=` it has no
    // namespace, so no block device appears and no driver here claims it. It
    // is added last so the addresses of the devices above stay where the
    // surrounding notes and probes expect them, and an installed disk does not
    // get it because this is the bus the pins describe.
    if let Boot::Esp(_) = &boot {
        args.extend(["-device".into(), "nvme,serial=mpos-pci-ext".into()]);
    }

    // The display and the wire, from the one decision both machines share
    // (`wiring`). A boot that renders nowhere still gets a display argument: a
    // `virtio-gpu-gl-pci` device refuses `-display none` outright - QEMU answers
    // "OpenGL is not supported by display backend 'none'" and never starts - so
    // hardcoding `none` here would make a GL machine untestable, and the GL half
    // of `DRM_VIRTGPU_*` is exactly the half the default machine cannot reach.
    // The console stays on the wire either way.
    let wiring = wiring(
        console,
        config,
        share_dir
            .parent()
            .expect("the share lives under the build directory"),
    );
    args.extend(["-display".into(), wiring.display]);
    match wiring.serial {
        SerialSink::Stdio => args.extend([
            "-chardev".into(),
            "stdio,id=stdio0,mux=off,signal=off".into(),
            "-serial".into(),
            "chardev:stdio0".into(),
        ]),
        SerialSink::File(log) => args.extend(["-serial".into(), format!("file:{}", log.display())]),
    }

    // The guest's own way out. `reboot(2)` with a stop command ends at
    // `drivers::qemu::exit`, which writes port 0xf4; with no device listening,
    // that write goes nowhere and the kernel halts with QEMU still up - which
    // is why `run` had no way to stop except killing the emulator. The test
    // boot reads the same port for its verdict, so it belongs to every machine
    // this builds, not to the harness alone.
    //
    // Beside it, a **control channel**, and the console socket above is its
    // precedent: a host-side socket nothing has to be listening on
    // (`wait=off`), which a test - or an operator - may hold. This one is QMP
    // rather than HMP because the one measurement neither a `t-` program nor
    // the guest itself can make is *reading a compositor's pointer back off the
    // host's side of the card*: the cursor queue is drawn into the scanout the
    // device composites, not into guest memory, and the two commands that reach
    // it - `screendump` and `input-send-event` - exist only in QMP
    // (`docs/ROADMAP.md`'s pointer item, whose last step was an eye's until
    // this). `-monitor none` stays: it is the *other* chardev, and a machine
    // that named only the QMP socket would still get QEMU's default monitor
    // wherever stdio already carries the serial line. 9p shares files and not
    // sockets, so the path sitting in the exported directory is not a door the
    // guest can reach.
    args.extend([
        "-monitor".into(),
        "none".into(),
        "-qmp".into(),
        format!(
            "unix:{},server=on,wait=off",
            share_dir.join("qmp.sock").display()
        ),
        "-no-reboot".into(),
    ]);
    args.extend([
        "-device".into(),
        "isa-debug-exit,iobase=0xf4,iosize=0x04".into(),
    ]);
    args.extend(config.extra_args.iter().cloned());
    args
}

/// True when the image does not reflect the sources: missing, never assembled,
/// or assembled from a kernel, a loader, an initramfs or an overlay that has
/// changed since.
///
/// `run` asks this before it decides anything. A kernel rebuilt after the last
/// image has to trigger the rebuild - otherwise the boot is of code that is no
/// longer on disk, and it reads exactly like a fix that changed nothing.
pub fn needs_build(build_dir: &Path, profile: &str, arch: Arch) -> Result<bool> {
    utils::image_stale(build_dir, profile, arch)
}

/// Boot `arch`'s image: build it first when it is stale, then hand the machine
/// to the emulator with its serial line on this terminal.
///
/// One body for both machines, because a session is the same thing on either:
/// the disk `disk`/`rootfs` installed, the firmware that machine boots under,
/// and the mode the caller asked for deciding both the window and the consoles
/// the guest is told about. What is per-machine is answered by `arch` (`Arch`'s
/// name, firmware and console) and by [`boot_args`]'s two arms - the transports
/// the two kernels can find.
pub fn run_qemu(
    build_dir: &Path,
    profile: &str,
    arch: Arch,
    artefacts_ready: bool,
    needs_build: bool,
    mode: RunMode,
) -> Result<()> {
    if needs_build {
        println!(
            "run: the {} image is missing or out of date; building it",
            arch.name()
        );
        if !artefacts_ready {
            // The kernel this build boots, named rather than left to the
            // workspace default - `--arch` is the switch for the whole command,
            // and the images below are assembled *from* these two binaries.
            utils::build_kernel(profile, arch)?;
        }
        crate::disk::ensure(build_dir, profile, arch)?;
        // The rootfs step brings the initramfs up to date itself, so there is
        // no separate call here to report it twice.
        crate::rootfs::ensure(build_dir, profile, arch)?;
        // When `artefacts_ready` is set this is the sudo half, and its parent
        // is waiting to boot what was just built. Only a `run` that is the
        // whole command has to be told to come back.
        if !artefacts_ready {
            println!();
            println!(
                "run: the image is built; run this command again to boot it \
                 (`cargo xtask run --arch {}` for the interactive boot)",
                arch.name()
            );
        }
        return Ok(());
    }

    let image = utils::image_path(build_dir, arch);

    // An image older than the kernel or the loader it was built from boots the
    // *previous* code and says nothing about it, which reads exactly like a fix
    // that changed nothing. `run.sh` warns and boots anyway; so does this.
    for artifact in [
        utils::kernel_path(profile, arch)?,
        utils::loader_path(profile, arch)?,
    ] {
        if utils::newer(&artifact, &image)? {
            eprintln!(
                "run: warning: {} predates {}",
                image.display(),
                artifact.display()
            );
            eprintln!("run:          it boots the code from before that build, which will");
            eprintln!("run:          look like a fix that changed nothing. Rebuild first:");
            eprintln!("run:            cargo xtask all --arch {}", arch.name());
            eprintln!();
        }
    }

    // The firmware's variables half is copied fresh on every run rather than
    // written to in place: the firmware writes to it, and a disk that has never
    // booted has no boot entry written down, which is exactly the state the
    // loader is built for.
    let (firmware, vars) = write_firmware_vars(build_dir, arch)?;

    // The share the 9p device exports and the console port's socket both
    // live under the build directory, and the devices they belong to start
    // only if their host-side files do - so they exist before QEMU is
    // asked for them.
    let share = utils::lay_p9_share(&build_dir.join(format!("{}-9p-share", arch.name())))?;

    // The machine's own settings: memory, processors, the name the guest
    // reports, and whatever `extra_args` adds. They come from `machine.toml`
    // rather than the image's config, so tuning the machine never reinstalls
    // the image.
    let config = QemuConfig::load_default(arch)?;
    // The host wiring follows the mode the caller asked for, so the window, the
    // serial line and the guest's own console policy always agree.
    let args = boot_args(
        arch,
        &config,
        &firmware,
        &vars,
        &image,
        &Console::Run(mode),
        &share,
        false,
    )?;

    // Printed because a boot that goes wrong is usually read off this line.
    eprintln!("+ {} {}", arch.qemu_binary(), args.join(" "));
    let err = Command::new(arch.qemu_binary()).args(&args).exec();
    anyhow::bail!("Failed to start QEMU: {err}");
}

/// The firmware pair a machine boots under.
///
/// Two files, because that is how EDK2 is deployed: a read-only code bank at the
/// address the machine boots from, and a writable variable store the firmware
/// writes to as it goes. Callers copy the second into the build directory rather
/// than pointing QEMU at the system's copy - a machine whose variables are the
/// host's own would keep whatever a previous boot wrote there, and a disk that
/// has never booted has no boot entry written down, which is exactly the state
/// the loader is built for.
pub struct Firmware {
    pub code: PathBuf,
    pub vars: PathBuf,
}

/// Find `arch`'s firmware, or refuse naming every place that was looked in.
///
/// One function for the two machines rather than a copy each, because the
/// question is the same - "where does this host keep the firmware for this
/// machine" - and so is the failure the reader needs: the list of directories.
/// What differs is only the names, and they differ because the ports do: the
/// first machine's is EDK2's OVMF (`OVMF_CODE.4m.fd` / `OVMF_VARS.4m.fd`, and the
/// 4 MiB build is the one that matters - the 2 MiB one cannot map the kernel and
/// the initramfs at the addresses the loader asks for), the second's is AAVMF,
/// which distributions also ship as edk2's own aarch64 build with different file
/// names.
///
/// The second machine has no accelerator list to answer: it runs under TCG on
/// every host, because `detect_accel` answers with the *host's* accelerator and
/// a host that is not aarch64 cannot run this machine on it.
pub fn find_firmware(arch: Arch) -> Result<Firmware> {
    let mut looked: Vec<String> = Vec::new();

    for dir in [
        "/usr/share/OVMF/x64",
        "/usr/share/edk2/x64",
        "/usr/share/OVMF",
        "/usr/share/edk2-ovmf/x64",
    ] {
        if arch != Arch::X86_64 {
            break;
        }
        let (code, vars) = (
            Path::new(dir).join("OVMF_CODE.4m.fd"),
            Path::new(dir).join("OVMF_VARS.4m.fd"),
        );
        if code.exists() && vars.exists() {
            return Ok(Firmware { code, vars });
        }
        looked.push(dir.to_string());
    }

    for (code, vars) in [
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
    ] {
        if arch != Arch::Aarch64 {
            break;
        }
        let (code, vars) = (Path::new(code), Path::new(vars));
        if code.exists() && vars.exists() {
            return Ok(Firmware {
                code: code.to_path_buf(),
                vars: vars.to_path_buf(),
            });
        }
        looked.push(format!("{} + {}", code.display(), vars.display()));
    }

    anyhow::bail!(
        "No UEFI firmware for {} found; looked for {}",
        arch.name(),
        looked.join(", ")
    )
}

/// The `qemu-system-aarch64` command line, as data.
///
/// Here rather than beside the commands that boot it, for the same reason
/// [`machine_args`] is here:
/// the two machines' wiring is one question - how this build boots a machine -
/// and the values both read come from one `machine.toml`. What does *not* come
/// from that file is the device list below, and that is deliberate: the virtio
/// transports here are the MMIO form because this machine's virtio probe walks
/// its own device tree's `virtio,mmio` windows. The PCI bus is a *separate*
/// discovery path (`drivers::pci`'s `discover_ecam_dt` reads the tree's
/// `pci-host-ecam-generic` window), which is why the SATA controller below and
/// the test machine's NVMe ride it; a `virtio-*-pci` device would be one the
/// virtio probe does not reach.
///
/// `test_machine` is the sweep's boot rather than a session's. It carries the
/// devices the pins describe and an installed machine does not: today the one
/// PCIe function whose extended configuration space `t-pci` asks about. It is a
/// parameter rather than a `machine.toml` key because it is not a property of
/// the machine - the same `virt` boots both - and the x86_64 arm makes the same
/// distinction with [`Boot::Esp`].
///
/// Taken from the boot that works, flag by flag, so what this function returns is
/// the machine the port was measured on. The reasons worth keeping are on the
/// lines below.
pub fn virt_machine_args(
    config: &QemuConfig,
    firmware: &Firmware,
    vars: &Path,
    image: &Path,
    blk: &Path,
    console: &Console,
    share: &Path,
    test_machine: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        // `acpi=off` is what makes this firmware describe the machine the way
        // this port reads it, and it is a measured difference rather than a
        // preference: with ACPI left on - the default - AAVMF advertises ACPI
        // 2.0 in its configuration table and publishes **no device tree at
        // all**, which is `NOTES.md` §9.1's assumption corrected (it said `virt`
        // + AAVMF passes a tree). A tree and a set of ACPI tables are two ways
        // to say the same things; this port reads the first, because that is
        // the one an embedded machine has, and the second is named as a gap
        // rather than half-read.
        "-machine".into(),
        "virt,gic-version=2,acpi=off".into(),
        // A named CPU rather than `max`, because the kernel's interrupt
        // controller is a GICv2 and its clock is the EL1 physical timer: both
        // are things a `max` CPU is free to configure differently, and a
        // difference there reads as a broken driver.
        "-cpu".into(),
        "cortex-a57".into(),
        // TCG and never KVM, on any host - see `find_firmware`.
        "-accel".into(),
        "tcg".into(),
        "-m".into(),
        config.memory.clone(),
        "-monitor".into(),
        "none".into(),
        // The control channel the first machine's arm carries too, at this
        // machine's own path: `-monitor none` is the *other* chardev, so both
        // are named. See that arm for what it is for (the pointer a guest
        // cannot read back) and why a socket in the exported directory is not a
        // door the guest can reach.
        "-qmp".into(),
        format!(
            "unix:{},server=on,wait=off",
            share.join("qmp.sock").display()
        ),
        "-no-reboot".into(),
        // Two flash banks, the way EDK2 expects to find itself: the code bank

        // read-only at the address the machine boots from, and a writable
        // variable store the firmware updates as it goes.
        "-drive".into(),
        format!(
            "if=pflash,format=raw,unit=0,readonly=on,file={}",
            firmware.code.display()
        ),
        "-drive".into(),
        format!("if=pflash,format=raw,unit=1,file={}", vars.display()),
        // The one disk: a GPT whose first partition is the ESP and whose second
        // is the root filesystem, attached on a virtio-**MMIO** transport.
        //
        // MMIO, and not the `-pci` this used to be: the same disk has to serve
        // two readers that do not share a discovery path. The firmware reads
        // partition 1 to find `BOOTAA64.EFI`; the kernel's `check_fs` finds its
        // root by walking the device tree's `virtio,mmio` windows, and a PCI
        // disk is not in that tree - it would be invisible there and the boot
        // would fail in `check_fs`. MMIO is the one transport both can use,
        // and AAVMF reads it: measured, it loads the loader from
        // `VenHw(837DCA9E-…)`, the virtio-mmio device path (`NOTES.md` §15.18).
        //
        // `file.locking=off`, for the reason the images below ask for it too.
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=hd0,file.locking=off",
            image.display()
        ),
        "-device".into(),
        "virtio-blk-device,drive=hd0".into(),
        // One device on a virtio-**MMIO** transport, which is what stage 5b's
        // check reads back. `virt` publishes its thirty-two `virtio,mmio` nodes
        // whether or not anything is behind them (measured: the decompiled tree
        // with and without this flag differs only in the firmware's own random
        // seeds), so without a device every window answers `DeviceID` 0 and the
        // check could only say "these look like transports". With one attached,
        // it reads a `DeviceID` the machine did not have to be told and names it.
        //
        // The MMIO variant on purpose, and not `-pci`: a PCI disk would be a
        // *different* discovery path (configuration space, not a fixed window),
        // and a check that a PCI device answered would not say anything about
        // the window the tree describes.
        "-device".into(),
        "virtio-rng-device".into(),
        // The second block device: a raw image carrying a known sector pattern,
        // which `check_blk` opens and verifies by content rather than by the slot
        // QEMU gave it. Two `virtio-blk-device` instances on purpose - the port
        // picks the right one by what the bytes are, which is the whole point of
        // the check.
        //
        // It stays a second disk and does not become a third partition: what it
        // proves is that a window the *tree* describes carries a block device the
        // machine was not told about, and a check that found it inside the boot
        // disk's own partition table would be a different claim - about
        // partitioning, which `check_fs` already makes.
        //
        // `file.locking=off`, which is what the first machine's wiring already
        // asks for and for a measured reason: with the default locking, a
        // `qemu-system-aarch64` that outlived the command that started it (a
        // boot killed at its deadline, an interrupted run) holds a write lock on
        // these files, and the *next* boot dies at once with "Failed to get
        // write lock" and an empty serial log - a machine that never started,
        // read as a machine that hung. Nothing else opens these files, so there
        // is no lock to keep.
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=blk0,file.locking=off",
            blk.display()
        ),
        "-device".into(),
        "virtio-blk-device,drive=blk0".into(),
        // The network adapter, on a user-mode (SLIRP) backend, which `check_net`
        // opens and reads the MAC of - the net stage's transport proof on this
        // machine (`NOTES.md` §9.1). No socket leaves the machine.
        "-netdev".into(),
        format!("user,id=n0,hostname={}", config.hostname),
        "-device".into(),
        "virtio-net-device,netdev=n0".into(),
        // A virtio-input device - the fourth class the MMIO virtio path has to
        // reach (`check_input_registry`). The keyboard backend is self-contained: it
        // presents its event queue and config with no external chardev, so the
        // boot opens and negotiates the transport headless.
        "-device".into(),
        "virtio-keyboard-device".into(),
        // The tablet, for the same reason the first machine attaches one: the
        // keyboard above is a *relative* device in the sense that matters here -
        // it carries key events and no position - and an absolute pointer is a
        // second input class a compositor is written against. It is the second
        // node in `/sys/class/input` on the first machine and was simply absent
        // here, which is a gap in what this machine offered rather than in the
        // driver: the MMIO input path reaches either device the same way.
        "-device".into(),
        "virtio-tablet-device".into(),
        // A host-driven console port - the fifth class (`check_console_dev`). Its
        // far end is a host socket nothing has to be listening on (`wait=off`),
        // so the port opens and a write lands in the socket instead of blocking
        // a machine that has nobody reading it yet.
        "-chardev".into(),
        format!(
            "socket,id=console0,path={},server=on,wait=off",
            share.join("console.sock").display()
        ),
        "-device".into(),
        "virtio-serial-device".into(),
        "-device".into(),
        "virtconsole,chardev=console0".into(),
        // The display adapter - the sixth class (`check_gpu_dev`). The MMIO form of
        // the card the first machine attaches as `virtio-gpu-pci`, so the
        // driver, the render task and the DRM layer above them are one copy on
        // both machines.
        //
        // Not `config.gpu`: which transport carries the card is how this machine
        // finds it, not a choice. A boot has no window, so the host reports no
        // monitor and the driver builds its headless fallback shape; the scanout
        // is set either way, which is what `/dev/fb0` reads.
        "-device".into(),
        "virtio-gpu-device".into(),
    ];

    // The second disk, on a *PCI* controller - the one device on this machine
    // that is not on an MMIO transport, and deliberately so: it is the second
    // machine's answer to the first's `ich9-ahci`, and the reason it can be is
    // that this kernel now enumerates a PCI bus here at all (`discover_ecam_dt`
    // reads the `pci-host-ecam-generic` window this machine's tree publishes,
    // and `mmio::window` gives the controller's BAR a virtual address). Before
    // either of those existed there was nowhere to put a BAR on this machine,
    // which is why `t-ahci` was a test the first machine alone could run.
    //
    // Attached unconditionally rather than "if the file exists", for the reason
    // the first machine's arm gives: a test machine that is supposed to have a
    // SATA disk and quietly does not would report `t-ahci` failing for a reason
    // that is not the driver's. The image is laid beside the boot disk
    // (`disk::ensure_sata_image`), so its directory is the one fact both ends
    // already agree on.
    let sata = crate::disk::sata_image(image.parent().expect("the image has a directory"));
    args.extend([
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=sd0,file.locking=off",
            sata.display()
        ),
        "-device".into(),
        "ich9-ahci,id=ahci".into(),
        "-device".into(),
        "ide-hd,drive=sd0,bus=ahci.0".into(),
    ]);

    // One PCIe function for the extended configuration space to be asked
    // about, on the test machine only - the same device and the same reason as
    // the first machine's `Boot::Esp` arm. The `ich9-ahci` above is
    // conventional PCI and publishes 256 bytes, and the host bridge, although
    // the rule asks it, has no extended capability to answer with; neither can
    // pin "a PCIe function publishes the 4096-byte extended space". An NVMe
    // controller is the device QEMU gives a real extended space to (AER at
    // 0x100); with no `drive=` it has no namespace, so no block device appears
    // and no driver here claims it. Added after the SATA devices so the
    // addresses the notes and probes above expect stay where they are.
    if test_machine {
        args.extend(["-device".into(), "nvme,serial=mpos-pci-ext".into()]);
    }

    // The display and the wire, from the one decision both machines share
    // (`wiring`). This machine names its window backend in its own half of
    // `machine.toml` (`none`: the session is the PL011, and the kernel's
    // framebuffer console is painted on a scanout nobody is looking at), so a
    // mode that asks for a window still draws none - and the wire therefore
    // stays on the launcher's terminal in every mode, which is what keeps the
    // session watchable.
    let wiring = wiring(
        console,
        config,
        share
            .parent()
            .expect("the share lives under the build directory"),
    );
    args.extend(["-display".into(), wiring.display]);
    match wiring.serial {
        SerialSink::Stdio => args.extend([
            "-chardev".into(),
            "stdio,id=stdio0,mux=off,signal=off".into(),
            "-serial".into(),
            "chardev:stdio0".into(),
        ]),
        SerialSink::File(log) => args.extend(["-serial".into(), format!("file:{}", log.display())]),
    }

    if config.audio {
        // The seventh class (`check_sound_dev`), the same shape the first machine
        // attaches as `virtio-sound-pci`. The backend is `none`: it consumes the
        // stream and plays silence, so the driver is exercised end to end
        // without asking the host for speakers.
        args.extend([
            "-audiodev".into(),
            "none,id=snd0".into(),
            "-device".into(),
            "virtio-sound-device,audiodev=snd0".into(),
        ]);
    }

    // The eighth class (`check_vsock`) - but only where the host can back it.
    // vsock is the one virtio device QEMU does not emulate in userspace:
    // `vhost-vsock-device` is served by the host's vhost kernel module, and a
    // machine naming it without `/dev/vhost-vsock` does not start at all.
    // Attaching it only then keeps this machine's boot honest on a bare host -
    // the kernel prints the absence and answers `EAFNOSUPPORT` - while a host
    // that has it runs the class for real (`NOTES.md` §13.28). The check is here
    // as well as in the file because the file cannot see the host; the first
    // machine's arm has no such check, which is why its `vsock` key is
    // documented as "off unless the host has the module".
    if config.vsock && Path::new("/dev/vhost-vsock").exists() {
        args.extend(["-device".into(), "vhost-vsock-device,guest-cid=4".into()]);
    }

    // The ninth class (9p): the host share, the same arrangement the
    // first machine's `-virtfs` makes. `-fsdev` is the backend - the host
    // directory QEMU's 9p server exports, with `security_model=none` so the
    // host's own permissions decide - and `virtio-9p-device` is the transport
    // the guest opens over MMIO. `mount_tag=hostshare` is the name
    // `mount -t 9p hostshare /mnt` is pointed at.
    args.extend([
        "-fsdev".into(),
        format!(
            "local,id=fsdev0,path={},security_model=none",
            share.display()
        ),
        "-device".into(),
        "virtio-9p-device,fsdev=fsdev0,mount_tag=hostshare".into(),
    ]);

    // And the transports have to be the *modern* ones, which on this machine is
    // a choice somebody has to make: measured, `virt`'s `virtio-mmio` transports
    // answer `Version` 1 - the legacy register layout - unless this says
    // otherwise, and version 2 is the only layout this kernel's driver crate
    // reads. The alternative to selecting it is a kernel that either drives the
    // legacy layout (a second register map, a second feature-negotiation order)
    // or refuses devices on its own machine.
    args.extend(["-global".into(), "virtio-mmio.force-legacy=false".into()]);

    // Whatever the machine's own `extra_args` adds, last, exactly as the first
    // machine's wiring puts them.
    args.extend(config.extra_args.iter().cloned());
    args
}

// ---------------------------------------------------------------------------
// Booting the machine the build just made.
//
// Both machines' boots are one sequence - copy the firmware's variables, start
// the emulator on the disk `crate::disk` laid, read the serial line until it
// says it is done - so both call these. What is per-machine is answered by
// `Arch` or by the one arm below, and nothing here asks which machine it is
// twice.
// ---------------------------------------------------------------------------

/// The `qemu-system-<machine>` command line for a boot off one disk.
///
/// The two arms are two machines, not two styles: the first is a `q35` whose
/// devices hang off a PCI bus, the second a `virt` whose devices the kernel
/// finds by walking its own device tree, so their transports are named
/// differently and neither can borrow the other's. What is shared is the
/// question - this disk, this firmware, this serial line - and the answer's
/// shape, which is why they are two arms of one function rather than two
/// functions a caller has to choose between.
///
/// `test_machine` is the sweep's boot rather than a session's; the second
/// machine's arm passes it through to the device list (`virt_machine_args`),
/// and the first machine's arm answers the same question from `Boot` at its
/// own call site, where the ESP names that boot.
pub fn boot_args(
    arch: Arch,
    config: &QemuConfig,
    firmware: &Firmware,
    vars: &Path,
    image: &Path,
    console: &Console,
    share: &Path,
    test_machine: bool,
) -> Result<Vec<String>> {
    match arch {
        Arch::X86_64 => Ok(machine_args(
            config,
            &firmware.code,
            vars,
            Boot::Image(image.to_path_buf()),
            console,
            share,
        )),
        Arch::Aarch64 => Ok(virt_machine_args(
            config,
            firmware,
            vars,
            image,
            // The raw pattern the block check reads by content. It lives beside
            // the image and is the same fixture for every boot of this machine
            // (`disk::blk_image`), so a sweep and a session share it.
            &crate::disk::blk_image(image.parent().expect("the image has a directory"), arch),
            console,
            share,
            test_machine,
        )),
    }
}

/// Where a machine's serial transcript is written.
///
/// The PL011's output, captured to a file for the same reason the first
/// machine's test boot captures its own: a verdict has to be readable after
/// the machine has stopped, and a machine booted to a real userland never
/// exits on its own.
pub fn serial_log(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(format!("{}-serial.log", arch.name()))
}

/// The writable copy of the firmware's variable store, under `build/`.
pub fn firmware_vars(build_dir: &Path, arch: Arch) -> PathBuf {
    build_dir.join(arch.firmware_vars())
}

/// The firmware pair and this machine's copy of the variable store.
///
/// The variables half is copied fresh on every boot rather than written to in
/// place: the firmware writes to it, and a disk that has never booted has no
/// boot entry written down, which is the state the loader is built for.
pub fn write_firmware_vars(build_dir: &Path, arch: Arch) -> Result<(Firmware, PathBuf)> {
    let firmware = find_firmware(arch)?;
    let vars = firmware_vars(build_dir, arch);
    fs::create_dir_all(build_dir)?;
    fs::copy(&firmware.vars, &vars).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            firmware.vars.display(),
            vars.display()
        )
    })?;
    Ok((firmware, vars))
}

/// Boot `arch`'s machine off `image` and return what its serial line said.
///
/// This is the headless boot an evidence-producing command uses: the wire goes
/// to a file and the poll stops on the first of `terminators` - a test sweep's
/// runner prints the harness protocol's `=== END`, so what ends the read is the
/// caller's to say. A session a person types at is [`run_qemu`], which hands the
/// wire to the terminal instead.
pub fn boot(
    build_dir: &Path,
    arch: Arch,
    image: &Path,
    terminators: &[&str],
    deadline: Duration,
) -> Result<String> {
    let (firmware, vars) = write_firmware_vars(build_dir, arch)?;
    let log = serial_log(build_dir, arch);
    let _ = fs::remove_file(&log);
    let share = utils::lay_p9_share(&build_dir.join(format!("{}-9p-share", arch.name())))?;
    let config = QemuConfig::load_default(arch)?;
    let args = boot_args(
        arch,
        &config,
        &firmware,
        &vars,
        image,
        &Console::File(log.display().to_string()),
        &share,
        true,
    )?;
    eprintln!("+ {} {}", arch.qemu_binary(), args.join(" "));
    let mut qemu = Command::new(arch.qemu_binary());
    qemu.args(&args);
    // QEMU's own stderr goes to this process's, not to a pipe nobody reads. A
    // QEMU that refuses to start - a missing firmware, an image it cannot open -
    // says so there and nowhere else, and the serial log it never wrote is
    // indistinguishable from a machine that hung; that is exactly the failure
    // this used to hide.
    qemu.stdout(Stdio::null()).stderr(Stdio::inherit());
    let captured = capture(qemu, &log, terminators, deadline)?;
    if captured.timed_out {
        // Not an error here, and that is deliberate: the caller reads the log
        // back and its own report says a run that never reached its last marker
        // did not finish. What is worth saying on the way past is that the
        // deadline, not the machine, ended this.
        eprintln!(
            "{}: the machine did not reach {} within {}s; reading what it said",
            arch.name(),
            terminators.join(" / "),
            deadline.as_secs()
        );
    }
    Ok(captured.log)
}

/// Runs a machine whose serial line goes to `log`, and returns when it stops
/// talking.
///
/// The two machines' boots used to poll on different clocks with different exit
/// rules and different failure text, and a machine that never started read as one
/// that hung in two different ways. The question is one question - run QEMU, read
/// what it says, stop when it is done saying it - so this is one loop.
///
/// Three things end it, and each is a different answer rather than a timeout:
///
/// * a `terminators` marker appeared in the log. A machine whose boot is its own
///   verdict prints markers, and waiting for the process to exit after the last
///   one is waiting for a boot to stop doing what it already proved;
/// * the process exited - which is how the first machine's boot ends, on the
///   debug exit its kernel writes;
/// * the deadline passed. The child is killed and `timed_out` says so, because a
///   boot that had to be killed is not a boot that finished.
///
/// A machine that never started is deliberately not special-cased here: QEMU's
/// own stderr is inherited, so it says why, and the log it never wrote is empty.
pub struct Captured {
    /// The process's exit code, or `None` if it was killed at the deadline.
    pub status: Option<i32>,
    /// The whole serial line, read after the machine stopped.
    pub log: String,
    pub timed_out: bool,
    /// Every line that began with one of the caller's marks, with the
    /// millisecond the poll first saw it. Empty for [`capture`], which does not
    /// ask for any: a single machine's caller reads the log it already has.
    ///
    /// This is where a sweep's per-program costs come from (`docs/notes/`
    /// §15.94): the harness's protocol prints a line when a program starts and
    /// another when it ends, and the harness is already polling the log, so the
    /// wall time between the two is free to read. It is a *hint* - the poll's
    /// own beat is 20 ms - and nothing but the next run's lane balance depends
    /// on it.
    pub progress: Vec<(u64, String)>,
}

/// Whether any of `markers` has reached the log at `path`.
///
/// Read as **bytes**, and that is the whole point of the function: the second
/// machine's serial line carries the ANSI escapes openrc writes, one of which is
/// not valid UTF-8, so `read_to_string` refuses the entire file (measured: the
/// host's `file(1)` reports "data", and the poll never matched). The marker
/// search therefore never fired and every aarch64 boot waited out its whole
/// deadline before reading the log it already had - a boot that took 60 s to
/// report something it said in ten. A marker is ASCII, so a byte search is the
/// honest comparison.
fn log_contains(path: &Path, markers: &[&str]) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    markers.iter().any(|marker| {
        bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    })
}

/// [`capture`]'s loop over several machines at once: the sweep's lanes.
///
/// A lane is not a second protocol - it is a second guest running the same one -
/// so the only thing this function adds is that a guest which finished does not
/// keep the others waiting. Each job is spawned before the first one is polled,
/// which is what makes the boots overlap rather than queue.
///
/// The deadline is sweep-wide, not per-lane: two lanes of a sweep share one
/// `MPOS_TEST_TIMEOUT` budget, because a sweep that hangs anywhere is a sweep
/// that hangs, and a per-lane deadline would make a four-lane run wait four
/// times as long before admitting it. As in [`capture`], a lane with
/// terminators that reached one is *done* - the caller reads the log either way
/// - and only a lane that did neither is reported as timed out.
///
/// `marks` are the line prefixes whose arrival time the caller wants: the poll
/// already has the log open, so the moment each marked line appeared is free to
/// keep (see [`Captured::progress`]). A partial line waits for its newline, and
/// a lane's last bytes are drained once more after the loop, so the closing
/// line of a guest that exited is not lost.
///
/// Answers in the order the jobs were given, so the caller keeps its own lane
/// numbering rather than inferring one from the order things finished in.
pub fn capture_many(
    jobs: Vec<(Command, PathBuf)>,
    terminators: &[&str],
    marks: &[&str],
    deadline: Duration,
) -> Result<Vec<Captured>> {
    struct Lane {
        child: std::process::Child,
        log: PathBuf,
        status: Option<std::process::ExitStatus>,
        reached_a_terminator: bool,
        /// How much of the log has been turned into lines already, and the
        /// bytes of a line still arriving.
        read: u64,
        partial: String,
        progress: Vec<(u64, String)>,
    }

    /// Take whatever has landed in `lane`'s log since the last look, and keep
    /// the marked lines with the millisecond they were first seen.
    fn drain(lane: &mut Lane, marks: &[&str], elapsed_ms: u64) {
        let Ok(bytes) = fs::read(&lane.log) else {
            return;
        };
        if bytes.len() as u64 <= lane.read {
            return;
        }
        let chunk = String::from_utf8_lossy(&bytes[lane.read as usize..]).into_owned();
        lane.read = bytes.len() as u64;
        lane.partial.push_str(&chunk);
        while let Some(index) = lane.partial.find('\n') {
            let line: String = lane.partial.drain(..=index).collect();
            let line = line.trim_end();
            if marks.iter().any(|mark| line.starts_with(mark)) {
                lane.progress.push((elapsed_ms, line.to_string()));
            }
        }
    }

    let started = Instant::now();
    let mut lanes: Vec<Lane> = Vec::with_capacity(jobs.len());
    for (mut command, log) in jobs {
        let child = command
            .spawn()
            .with_context(|| format!("Failed to start QEMU for {}", log.display()))?;
        lanes.push(Lane {
            child,
            log,
            status: None,
            reached_a_terminator: false,
            read: 0,
            partial: String::new(),
            progress: Vec::new(),
        });
    }

    loop {
        let mut running = false;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        for lane in &mut lanes {
            // The bytes are taken even from a lane that has finished: the line
            // that says a machine is done often arrives in the same poll as its
            // exit, and a cost table missing the last program of every lane is
            // a table that keeps that program on the wrong lane for ever.
            drain(lane, marks, elapsed_ms);
            if lane.status.is_some() || lane.reached_a_terminator {
                continue;
            }
            // A lane that said its last word is finished even though its
            // process is not; the same rule `capture` applies to one machine.
            if !terminators.is_empty() && log_contains(&lane.log, terminators) {
                lane.reached_a_terminator = true;
                continue;
            }
            match lane.child.try_wait()? {
                Some(status) => lane.status = Some(status),
                None => running = true,
            }
        }
        if !running {
            break;
        }
        if started.elapsed() >= deadline {
            break;
        }
        // Finer than `capture`'s hundred milliseconds: a lane's *whole* run is
        // the sweep, so a tenth of a second of join latency per lane would be a
        // measured part of the gate rather than the rounding of a one-minute
        // boot.
        std::thread::sleep(Duration::from_millis(20));
    }

    // One last look for every lane, with the clock where the loop left it: a
    // guest's closing lines can land between the last poll and its exit.
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let mut captured = Vec::with_capacity(lanes.len());
    for mut lane in lanes {
        drain(&mut lane, marks, elapsed_ms);
        // A lane still running is one that neither left nor said its last word:
        // it is stopped here, so the sweep cannot leave a guest behind.
        if lane.status.is_none() {
            let _ = lane.child.kill();
            let _ = lane.child.wait();
        }
        let timed_out = lane.status.is_none() && !lane.reached_a_terminator;
        let text = String::from_utf8_lossy(&fs::read(&lane.log).unwrap_or_default()).into_owned();
        captured.push(Captured {
            status: lane.status.and_then(|status| status.code()),
            log: text,
            timed_out,
            progress: lane.progress,
        });
    }
    Ok(captured)
}

pub fn capture(
    mut command: Command,
    log: &Path,
    terminators: &[&str],
    deadline: Duration,
) -> Result<Captured> {
    let started = Instant::now();
    let mut child = command.spawn().context("Failed to start QEMU")?;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut reached_a_terminator = false;

    loop {
        if !terminators.is_empty() && log_contains(log, terminators) {
            reached_a_terminator = true;
            break;
        }
        match child.try_wait()? {
            Some(finished) => {
                status = Some(finished);
                break;
            }
            None => {
                if started.elapsed() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    // The child is stopped unless it stopped itself. A machine that reached its
    // last marker is still running - it has no reason to exit, and the caller
    // reads the log either way.
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Three reasons to have stopped, and only one of them is a failure to
    // finish: the machine said its last word, the machine left, or the clock ran
    // out. Reading the first as the third is what made a healthy seven-second
    // boot report that it had exhausted its deadline.
    let timed_out = status.is_none() && !reached_a_terminator;

    // Read back as text for the caller's own verdict: the escapes are the
    // caller's business, and every report that reads this log strips them or
    // searches for the ASCII around them.
    let text = String::from_utf8_lossy(&fs::read(log).unwrap_or_default()).into_owned();
    Ok(Captured {
        status: status.and_then(|status| status.code()),
        log: text,
        timed_out,
        progress: Vec::new(),
    })
}

/// The accelerator. `ACCEL` from the environment wins over the detection, the
/// way `run.sh` reads it: KVM makes this usable, but it needs `/dev/kvm`, which
/// a container usually does not have.
pub fn detect_accel() -> String {
    if let Ok(accel) = env::var("ACCEL")
        && !accel.is_empty()
    {
        return accel;
    }

    let kvm = CString::new("/dev/kvm").expect("the path has no interior NUL");
    if unsafe { libc::access(kvm.as_ptr(), libc::W_OK) } == 0 {
        return "kvm".to_string();
    }

    "tcg".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QemuConfig;

    fn config(memory: &str, cpus: u32, extra: &[&str]) -> QemuConfig {
        QemuConfig {
            memory: memory.to_string(),
            cpus,
            hostname: "probe".to_string(),
            extra_args: extra.iter().map(|arg| arg.to_string()).collect(),
            vsock: false,
            audio: false,
            display: None,
            gpu: "virtio-gpu-pci".to_string(),
            mode_text: None,
        }
    }

    fn args(config: &QemuConfig, boot: Boot) -> Vec<String> {
        machine_args(
            config,
            Path::new("/ovmf/OVMF_CODE.4m.fd"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            boot,
            &Console::Run(RunMode::Mirror),
            Path::new("/build/9p-share"),
        )
    }

    /// The argument after `flag`, which is how every assertion here reads the
    /// line - the order of unrelated options is QEMU's business, not this
    /// crate's.
    fn after(args: &[String], flag: &str) -> Option<String> {
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).cloned()
    }

    fn firmware() -> Firmware {
        Firmware {
            code: PathBuf::from("/aavmf/AAVMF_CODE.fd"),
            vars: PathBuf::from("/build/AAVMF_VARS.fd"),
        }
    }

    /// The second machine's line, as a *headless* boot builds it: the wire to a
    /// file, which is the shape every evidence-producing boot has.
    fn aarch64_args(config: &QemuConfig) -> Vec<String> {
        aarch64_args_for(config, false)
    }

    /// The same line for a sweep's boot (`test_machine`), whose device list
    /// carries what the pins read.
    fn aarch64_args_for(config: &QemuConfig, test_machine: bool) -> Vec<String> {
        virt_machine_args(
            config,
            &firmware(),
            Path::new("/build/AAVMF_VARS.fd"),
            Path::new("/build/aarch64-disk.img"),
            Path::new("/build/aarch64-blk.img"),
            &Console::File("/build/aarch64-serial.log".to_string()),
            Path::new("/build/aarch64-9p-share"),
            test_machine,
        )
    }

    /// A booted machine carries a QMP control socket, and it rides *beside*
    /// `-monitor none` rather than replacing it: the two are separate chardevs,
    /// so a machine that named only the QMP socket would still get QEMU's
    /// default monitor wherever stdio already carries the serial line. The
    /// socket is what `screendump` and `input-send-event` are reached through -
    /// neither exists in HMP - and therefore the reason a compositor's pointer
    /// can be read back off the host's side of the card at all.
    #[test]
    fn a_booted_machine_carries_a_control_socket() {
        let config = config("256", 1, &[]);

        let first = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(after(&first, "-monitor").as_deref(), Some("none"));
        assert_eq!(
            after(&first, "-qmp").as_deref(),
            Some("unix:/build/9p-share/qmp.sock,server=on,wait=off"),
            "the first machine's control socket"
        );

        let second = aarch64_args(&config);
        assert_eq!(after(&second, "-monitor").as_deref(), Some("none"));
        assert_eq!(
            after(&second, "-qmp").as_deref(),
            Some("unix:/build/aarch64-9p-share/qmp.sock,server=on,wait=off"),
            "and the second machine's, at its own path: one arch's boot must not \
             open the other's socket"
        );
    }

    /// The marker search reads bytes, because the second machine's log is not
    /// text: openrc's escapes include a byte sequence that is not valid UTF-8,
    /// and a `read_to_string` over that file fails outright - which is how every
    /// aarch64 boot came to wait out its whole 60 s deadline while the marker it
    /// was waiting for sat in the file.
    #[test]
    fn a_marker_is_found_in_a_log_that_is_not_valid_utf8() {
        let dir = std::env::temp_dir().join(format!("mpos-capture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("serial.log");
        // `\x1b[1m` is openrc's bold; the lone `\xff` is the byte that makes the
        // file not decode.
        let mut bytes = b"Starting local ...\n\xff\x1b[1m[ ok ]".to_vec();
        bytes.extend_from_slice(b"\nMPOS_SWITCH_ROOT_OK: / is /dev/vda\nINIT_EXITED\n");
        std::fs::write(&log, &bytes).unwrap();

        assert!(log_contains(&log, &["INIT_EXITED"]));
        assert!(log_contains(&log, &["KERNEL PANIC", "INIT_EXITED"]));
        assert!(!log_contains(&log, &["M8_P9_OK"]));
        assert!(
            std::fs::read_to_string(&log).is_err(),
            "the fixture must be a log `read_to_string` refuses, or it pins nothing"
        );
    }

    /// The three ways a capture ends are told apart. Reading "stopped on a
    /// marker" as "ran out of deadline" is not cosmetic: it made a healthy
    /// seven-second boot print that it had exhausted its sixty.
    #[test]
    fn a_capture_that_reached_its_marker_is_not_a_timeout() {
        let dir = std::env::temp_dir().join(format!("mpos-capture-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("serial.log");

        // A process that stays up while the marker is already in the log: the
        // capture stops on the marker, stops the child, and does not call it a
        // timeout.
        std::fs::write(&log, "MPOS_SWITCH_ROOT_OK\nINIT_EXITED\n").unwrap();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("30");
        let started = Instant::now();
        let captured = capture(sleeper, &log, &["INIT_EXITED"], Duration::from_secs(10)).unwrap();
        assert!(captured.log.contains("INIT_EXITED"));
        assert!(
            !captured.timed_out,
            "the marker ended this run, not the clock"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it must stop on the marker rather than wait the deadline out"
        );

        // No marker, but the process leaves: finished, not timed out.
        std::fs::write(&log, "=== END\n").unwrap();
        let captured = capture(
            Command::new("true"),
            &log,
            &["NEVER_PRINTED"],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(!captured.timed_out);
        assert_eq!(captured.status, Some(0));

        // No marker and no exit: this one *is* the clock running out.
        std::fs::write(&log, "still booting\n").unwrap();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("30");
        let captured =
            capture(sleeper, &log, &["INIT_EXITED"], Duration::from_millis(300)).unwrap();
        assert!(captured.timed_out);
    }

    #[test]
    fn the_second_machines_line_is_virt_acpi_off_and_reads_its_own_values() {
        let mut config = config("256", 1, &[]);
        config.display = Some("none".to_string());
        let args = aarch64_args(&config);

        assert_eq!(
            after(&args, "-machine").as_deref(),
            Some("virt,gic-version=2,acpi=off"),
            "ACPI off is what makes AAVMF publish a device tree at all"
        );
        assert_eq!(after(&args, "-cpu").as_deref(), Some("cortex-a57"));
        assert_eq!(
            after(&args, "-accel").as_deref(),
            Some("tcg"),
            "a host that is not aarch64 cannot run this machine on its own accelerator"
        );
        assert_eq!(after(&args, "-m").as_deref(), Some("256"));
        assert_eq!(after(&args, "-display").as_deref(), Some("none"));
        assert_eq!(
            after(&args, "-serial").as_deref(),
            Some("file:/build/aarch64-serial.log")
        );
        assert!(
            args.iter().any(|a| a == "virtio-mmio.force-legacy=false"),
            "without this the transports answer the legacy layout the crate cannot read: {args:?}"
        );
    }

    /// Every *virtio* device the second machine drives arrives on the **MMIO**
    /// transport, because that is how its virtio probe finds them - by walking
    /// its own device tree. A `virtio-*-pci` device here would be one the probe
    /// does not reach, which is why the transports are the machine's rather
    /// than a `machine.toml` key. The PCI bus is still enumerated for the SATA
    /// controller below and the test machine's NVMe; it is the *virtio* probe
    /// that reads the tree instead.
    #[test]
    fn the_second_machines_devices_are_the_mmio_forms() {
        let mut config = config("256", 1, &[]);
        config.audio = true;
        config.extra_args = vec!["-vga".to_string(), "none".to_string()];
        let args = aarch64_args(&config);

        for device in [
            "virtio-rng-device",
            // Since §15.18 the machine carries *one* disk - a GPT whose first
            // partition is the ESP and whose second is the root - plus the raw
            // pattern image the block check reads sector 0 off. The ext4 used to
            // be a second block device of its own; it is a partition now, so
            // there is no `drive=blk1` to name.
            "virtio-blk-device,drive=hd0",
            "virtio-blk-device,drive=blk0",
            "virtio-net-device,netdev=n0",
            "virtio-keyboard-device",
            "virtio-gpu-device",
            "virtio-sound-device,audiodev=snd0",
            "virtio-serial-device",
        ] {
            assert!(
                args.iter().any(|arg| arg == device),
                "{device} is missing: {args:?}"
            );
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg.ends_with("-pci") && arg.starts_with("virtio")),
            "a virtio-PCI device on this machine is one the kernel cannot find: {args:?}"
        );
        // The machine's own `extra_args` come last, the way the first machine's
        // wiring puts them.
        assert_eq!(args[args.len() - 2..], ["-vga", "none"]);
    }

    /// The test machine carries one PCIe function whose extended configuration
    /// space `t-pci` asks about, and a session's machine does not - the same
    /// rule as the first machine's `Boot::Esp` arm. A bare controller with no
    /// `drive=` has no namespace, so no block device appears and no driver here
    /// claims it.
    #[test]
    fn the_second_machines_test_boot_carries_one_pcie_function() {
        let config = config("256", 1, &[]);
        assert!(
            !aarch64_args(&config)
                .iter()
                .any(|arg| arg.starts_with("nvme")),
            "a session boots the installed machine's device list"
        );
        let test = aarch64_args_for(&config, true);
        assert!(
            test.iter().any(|arg| arg == "nvme,serial=mpos-pci-ext"),
            "the sweep has no PCIe function to pin the extended space with: {test:?}"
        );
        assert!(
            !test
                .iter()
                .any(|arg| arg.starts_with("nvme") && arg.contains("drive=")),
            "an NVMe with a drive would become a block device nobody asked for: {test:?}"
        );
    }

    /// The vsock device needs the host's vhost module, and a machine that names
    /// it without one refuses to start - so the key alone is not enough here.
    /// The first machine's arm has no such check; that machine's file documents
    /// the key as opt-in for the same reason.
    #[test]
    fn the_second_machines_vsock_device_needs_the_host_to_have_one() {
        let mut config = config("256", 1, &[]);

        config.vsock = false;
        assert!(
            !aarch64_args(&config)
                .iter()
                .any(|arg| arg.starts_with("vhost-vsock-device")),
            "the device was attached with the key off"
        );

        config.vsock = true;
        let attached = aarch64_args(&config)
            .iter()
            .any(|arg| arg == "vhost-vsock-device,guest-cid=4");
        assert_eq!(
            attached,
            Path::new("/dev/vhost-vsock").exists(),
            "the key asks for the device; only this host can say whether it may have one"
        );
        assert!(
            !aarch64_args(&config)
                .iter()
                .any(|arg| arg.contains("guest-cid=3")),
            "cid 3 is the first machine's; a second guest claiming a live cid aborts QEMU"
        );
    }

    #[test]
    fn the_machine_line_carries_the_configured_memory_and_processors() {
        let config = config("2048", 4, &[]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(after(&args, "-m").as_deref(), Some("2048"));
        assert_eq!(after(&args, "-smp").as_deref(), Some("4"));
        assert_eq!(after(&args, "-machine").as_deref(), Some("q35"));
    }

    #[test]
    fn one_processor_is_left_to_qemu() {
        // `-smp 1` is the default, and the kernel's own bring-up reads the
        // absence of a second processor out of the same place either way.
        let config = config("1024", 1, &[]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert!(!args.iter().any(|arg| arg == "-smp"), "{args:?}");
    }

    #[test]
    fn an_image_boot_is_a_writable_disk_and_an_esp_boot_is_read_only() {
        let config = config("1024", 1, &[]);

        let image = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));
        let disk = image
            .iter()
            .find(|arg| arg.contains("mpos.img"))
            .expect("the image is on the command line");
        assert!(disk.contains("file.locking=off"), "{disk}");
        assert!(!disk.contains("readonly"), "the guest writes to it: {disk}");

        let esp = args(&config, Boot::Esp(PathBuf::from("/build/test-esp")));
        let disk = esp
            .iter()
            .find(|arg| arg.contains("test-esp"))
            .expect("the ESP is on the command line");
        assert!(
            disk.contains("readonly=on"),
            "virtual FAT cannot be written, and a device that was not told so never starts: {disk}"
        );
    }

    #[test]
    fn the_guest_is_named_and_extra_arguments_come_last() {
        let config = config("1024", 1, &["-enable-kvm"]);
        let args = args(&config, Boot::Image(PathBuf::from("/build/mpos.img")));

        let netdev = args
            .iter()
            .find(|arg| arg.starts_with("user,id=n0"))
            .expect("the network is on the command line");
        assert!(netdev.contains("hostname=probe"), "{netdev}");
        // No `dns=` on the line, ever - not because the guest's nameserver is
        // not configurable, but because QEMU's `dns=` does something else: it
        // renames the address slirp answers DNS at (the 10.0.2.3 the guest's
        // resolv.conf and `t-dns` name) rather than choosing an upstream,
        // which strands every query the guest sends. Choosing a server is
        // `system.dns` in rootfs.toml, which the rootfs build writes into
        // the guest's own resolv.conf.
        assert!(!netdev.contains("dns="), "{netdev}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("-enable-kvm"),
            "the escape hatch is appended after everything computed: {args:?}"
        );
    }

    #[test]
    fn an_interactive_run_opens_a_window_for_the_framebuffer() {
        // The shells now render to the framebuffer, so `run` must give QEMU a
        // real display backend (SDL by default) - not `none`, which would show
        // nothing once the console leaves the wire. Overridable; a headless
        // file-log boot (the selftest) still asks for `none`.
        let cfg = config("1024", 1, &[]);
        let stdio = args(&cfg, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(after(&stdio, "-display").as_deref(), Some("sdl"));

        let mut custom = config("1024", 1, &[]);
        custom.display = Some("gtk".to_string());
        let gtk = args(&custom, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(after(&gtk, "-display").as_deref(), Some("gtk"));

        let file = machine_args(
            &cfg,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            Boot::Image(PathBuf::from("/build/mpos.img")),
            &Console::File("/build/test-serial.log".to_string()),
            Path::new("/build/9p-share"),
        );
        assert_eq!(after(&file, "-display").as_deref(), Some("none"));

        // And a boot that renders nowhere ignores the interactive window key
        // entirely: it has no window to place, and the one thing it must get
        // right is that a `virtio-gpu-gl-pci` refuses `-display none` - so the
        // headless GL backend comes off the *card*, not off a key that describes
        // something else. Without this the GL machine would be untestable, or
        // every gate run would open a window because a key for the interactive
        // window travelled into a boot that has none.
        let mut gl = config("1024", 1, &[]);
        gl.gpu = "virtio-gpu-gl-pci".to_string();
        gl.display = Some("gtk".to_string());
        let gl_file = machine_args(
            &gl,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            Boot::Image(PathBuf::from("/build/mpos.img")),
            &Console::File("/build/test-serial.log".to_string()),
            Path::new("/build/9p-share"),
        );
        assert_eq!(
            after(&gl_file, "-display").as_deref(),
            Some("egl-headless,gl=on")
        );
        // The same rule reaches a *run* that draws nowhere: a `terminal` boot of
        // a GL card cannot name `none` either, so it takes the headless GL
        // backend - and the wire still reaches the launcher's terminal, because
        // that mode's console is the wire and nothing is drawn.
        let gl_terminal = machine_args(
            &gl,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            Boot::Image(PathBuf::from("/build/mpos.img")),
            &Console::Run(RunMode::Terminal),
            Path::new("/build/9p-share"),
        );
        assert_eq!(
            after(&gl_terminal, "-display").as_deref(),
            Some("egl-headless,gl=on")
        );
        assert_eq!(
            after(&gl_terminal, "-serial").as_deref(),
            Some("chardev:stdio0")
        );
        // The console is still the wire, whatever the display is: the test
        // image's own command line names `ttyS0`, and the transcript is still a
        // file rather than a window.
        assert_eq!(
            after(&gl_file, "-serial").as_deref(),
            Some("file:/build/test-serial.log")
        );
    }

    #[test]
    fn a_gl_card_beside_a_window_gets_its_context_from_the_card() {
        // The refusal that started this: QEMU answers `-device virtio-gpu-gl-pci`
        // against a bare `-display sdl` with "The display backend does not have
        // OpenGL support enabled", and the run dies before the guest boots. So a
        // windowed boot of a GL machine - `mirror` or `gui`, whether the backend
        // is defaulted or configured - must carry `,gl=on`, while a 2D card leaves
        // the window exactly as it was named. Without the derivation the default
        // `sdl` is refused outright; without the 2D case staying untouched the fix
        // would append a context no plain card asked for.
        let mut gl = config("1024", 1, &[]);
        gl.gpu = "virtio-gpu-gl-pci".to_string();
        let mirror = run_args(&gl, RunMode::Mirror);
        assert_eq!(
            after(&mirror, "-display").as_deref(),
            Some("sdl,gl=on"),
            "a mirror run of a GL card is refused without a context"
        );
        let gui = run_args(&gl, RunMode::Gui);
        assert_eq!(after(&gui, "-display").as_deref(), Some("sdl,gl=on"));

        // A configured backend is carried with its own suboptions intact, and one
        // that already names `gl=` is left alone rather than doubled.
        let mut gtk_gl = config("1024", 1, &[]);
        gtk_gl.gpu = "virtio-gpu-gl-pci".to_string();
        gtk_gl.display = Some("gtk".to_string());
        assert_eq!(
            after(&run_args(&gtk_gl, RunMode::Mirror), "-display").as_deref(),
            Some("gtk,gl=on")
        );
        let mut explicit = config("1024", 1, &[]);
        explicit.gpu = "virtio-gpu-gl-pci".to_string();
        explicit.display = Some("sdl,gl=core".to_string());
        assert_eq!(
            after(&run_args(&explicit, RunMode::Mirror), "-display").as_deref(),
            Some("sdl,gl=core")
        );

        // The counterfactual: a 2D card's window is untouched by all this.
        let plain = config("1024", 1, &[]);
        assert_eq!(
            after(&run_args(&plain, RunMode::Mirror), "-display").as_deref(),
            Some("sdl")
        );
    }

    /// The GPU the machine was wired with, read off the `-device` list the way
    /// QEMU reads it: the card that rides beside the entropy source.
    fn gpu_of(args: &[String]) -> String {
        let at = args
            .iter()
            .position(|arg| arg == "virtio-rng-pci")
            .expect("the rng card is attached");
        args.get(at + 2)
            .cloned()
            .expect("the gpu is the -device after the rng card")
    }

    #[test]
    fn the_gpu_is_the_machines_choice_and_nothing_elses() {
        // The default machine is the 2D card every boot before this key had.
        let plain = config("1024", 1, &[]);
        let plain_args = args(&plain, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(gpu_of(&plain_args), "virtio-gpu-pci");

        // Naming the GL card moves two words, not one: the `-device`, and the
        // window's `-display`, which must carry `,gl=on` or QEMU refuses the card
        // outright ("The display backend does not have OpenGL support enabled").
        // Pinning the exact pair makes a boot failure attributable - a GL machine
        // that will not start differs from a working 2D one by the card and the
        // context it needs, and by nothing else.
        let mut gl = config("1024", 1, &[]);
        gl.gpu = "virtio-gpu-gl-pci".to_string();
        let gl_args = args(&gl, Boot::Image(PathBuf::from("/build/mpos.img")));
        assert_eq!(gpu_of(&gl_args), "virtio-gpu-gl-pci");
        assert_eq!(
            plain_args.len(),
            gl_args.len(),
            "a card changed the argument count, so something else moved too"
        );
        let differing: Vec<(String, String)> = plain_args
            .iter()
            .zip(&gl_args)
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        assert_eq!(
            differing,
            vec![
                (
                    "virtio-gpu-pci".to_string(),
                    "virtio-gpu-gl-pci".to_string()
                ),
                ("sdl".to_string(), "sdl,gl=on".to_string()),
            ],
            "the GPU choice moved something besides the card and its GL context"
        );

        // A card is not a mode: every mode boots the machine's own GPU, so
        // whether a window is wanted and which card the guest is given stay
        // separate questions.
        for mode in [RunMode::Terminal, RunMode::Mirror, RunMode::Gui] {
            assert_eq!(
                gpu_of(&run_args(&gl, mode)),
                "virtio-gpu-gl-pci",
                "{} booted a different GPU",
                mode.name()
            );
        }
    }

    fn run_args(config: &QemuConfig, mode: RunMode) -> Vec<String> {
        machine_args(
            config,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            Boot::Image(PathBuf::from("/build/mpos.img")),
            &Console::Run(mode),
            Path::new("/build/9p-share"),
        )
    }

    #[test]
    fn a_mode_switches_the_window_and_where_the_wire_is_read() {
        // Two facts separate the three modes on the host side: whether there is
        // a window, and whether the serial line is read off stdio or off a log
        // file. `terminal` and `mirror` differ by exactly one word - the window -
        // and pinning that equality here is what keeps a mode word from quietly
        // acquiring a third wiring. `gui` shares mirror's window and moves the
        // wire off the terminal, because in that mode the window *is* the
        // console; everything else - every device - stays attached in all three.
        let cfg = config("1024", 1, &[]);
        for mode in [RunMode::Terminal, RunMode::Mirror] {
            let args = run_args(&cfg, mode);
            assert_eq!(
                after(&args, "-serial").as_deref(),
                Some("chardev:stdio0"),
                "{} should watch the wire on the terminal",
                mode.name()
            );
            assert!(
                !args.iter().any(|a| a.contains("/dev/null")),
                "{} throws the serial line away",
                mode.name()
            );
        }

        let terminal = run_args(&cfg, RunMode::Terminal);
        assert_eq!(after(&terminal, "-display").as_deref(), Some("none"));
        let mirror = run_args(&cfg, RunMode::Mirror);
        assert_eq!(after(&mirror, "-display").as_deref(), Some("sdl"));
        let mut only_the_window_differs = terminal;
        let at = only_the_window_differs
            .iter()
            .position(|arg| arg == "-display")
            .unwrap()
            + 1;
        only_the_window_differs[at] = "sdl".to_string();
        assert_eq!(
            only_the_window_differs, mirror,
            "a mode switched something in QEMU besides the window"
        );

        // gui: the window shows the console, and the launcher's terminal does
        // not - the wire rides a file under the build directory instead, so the
        // loader's lines, the pre-cmdline banner and a panic report keep a
        // witness while the terminal stays quiet.
        let gui = run_args(&cfg, RunMode::Gui);
        assert_eq!(after(&gui, "-display").as_deref(), Some("sdl"));
        assert_eq!(
            after(&gui, "-serial").as_deref(),
            Some("file:/build/serial.log"),
            "gui should tap the wire into the build directory, beside the share \
             and not inside it"
        );
        assert!(
            !gui.iter().any(|a| a.contains("stdio")),
            "gui puts the wire back on the terminal: {gui:?}"
        );
        assert!(
            !gui.iter().any(|a| a.contains("/dev/null")),
            "gui throws the serial line away again",
        );
    }
}
