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
use std::process::Command;
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
    /// A file, which is what a test run reads its verdict out of. Always
    /// headless: no window, the whole console on the wire to the log.
    File(String),
}

/// Where this crate records which [`RunMode`] is baked into the image's
/// `mpos.cmdline`. The guest learns its console policy from that file, which
/// lives inside the disk image; the marker lets `run` decide, without mounting
/// anything as root, whether the image already says what the user asked for.
///
/// The file is still named `console.mode` from before the option was `--console`.
/// It is a cache, not an interface: renaming it would read as "nothing baked" and
/// make every existing image look as if it needed a root rebuild to say what it
/// already does.
fn mode_marker_path(build_dir: &Path) -> PathBuf {
    build_dir.join("console.mode")
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
pub fn baked_mode(build_dir: &Path) -> Option<RunMode> {
    fs::read_to_string(mode_marker_path(build_dir))
        .ok()
        .and_then(|text| RunMode::parse(&text))
}

pub fn set_baked_mode(build_dir: &Path, mode: RunMode) -> Result<()> {
    fs::write(mode_marker_path(build_dir), mode.name())
        .with_context(|| format!("Failed to record the baked mode in {}", build_dir.display()))
}

/// Whether the mode baked into the image differs from `desired`, so a `run` must
/// take root to rewrite `mpos.cmdline` before booting. No image means nothing to
/// switch (the build that makes it will bake `desired` anyway), and an image with
/// no marker at all differs from every mode - its line predates the console list.
pub fn needs_mode_switch(build_dir: &Path, desired: RunMode) -> bool {
    utils::image_path(build_dir).exists() && baked_mode(build_dir) != Some(desired)
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
    console: Console,
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
    // two disks from two drivers (`PLAN.md` §7.3). The image lives beside the
    // ESP, because `Boot::Esp` carries the ESP's own path and nothing else -
    // its directory is the one fact both ends already agree on.
    //
    // Attached unconditionally rather than "if the file exists": a test machine
    // that is supposed to have a SATA disk and quietly does not would report
    // `t-ahci` failing for a reason that is not the driver's.
    if let Boot::Esp(esp) = &boot {
        let disk = esp
            .parent()
            .expect("the ESP path has a directory")
            .join(crate::selftest::SATA_TEST_IMAGE);
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
    // (`PLAN.md` §7.4). Which one it is changes only this word, so the rest of
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

    match console {
        // `mux=off` is what the guest's own console keys are for. QEMU's
        // default for a serial line shared with stdio is to claim `Ctrl-A` as
        // its own escape between the monitor and the port, and the byte never
        // reaches the machine - so `^A 1..6`, the virtual-console switch the
        // kernel's mux and `/etc/inittab` both advertise, would sit unheard
        // while every other keystroke went through. `signal=off` keeps
        // `Ctrl-C` an ordinary character for the guest's line discipline
        // rather than a request to quit the emulator.
        Console::Run(mode) => {
            // The window only when the mode asks for one. A terminal boot
            // renders nowhere: `-display none`, the whole console on the wire.
            let display = if mode.has_window() {
                config.display.clone().unwrap_or_else(|| "sdl".to_string())
            } else {
                "none".to_string()
            };
            args.extend(["-display".into(), display]);
            if mode == RunMode::Gui {
                // The window is the console here, so the launcher's terminal
                // goes quiet: the wire is read from a file, not stdio. The
                // guest's own list omits ttyS0 and the kernel never writes to
                // the port — but the UEFI loader prints to COM1 unconditionally,
                // the kernel's banner precedes the command-line parse, and a
                // panic's `force_serial` report reaches no screen, so the port
                // stays tapped. Tapping it into a file keeps the witness while
                // the terminal shows nothing; discarding it was the old `gui`,
                // and the modes collapsed away from that precisely because a
                // mode that dropped the wire dropped the kernel log with it.
                //
                // The log lives beside the share, not inside it: `share_dir`
                // is exported over 9p, and a host-side transcript has no
                // business appearing in the guest's `/mnt/hostshare`.
                let log = share_dir
                    .parent()
                    .expect("the share lives under the build directory")
                    .join("serial.log");
                args.extend(["-serial".into(), format!("file:{}", log.display())]);
            } else {
                args.extend([
                    "-chardev".into(),
                    "stdio,id=stdio0,mux=off,signal=off".into(),
                    "-serial".into(),
                    "chardev:stdio0".into(),
                ]);
            }
        }
        Console::File(log) => {
            // A test boot renders nowhere *unless the machine names a display*,
            // and the exception is not decoration: a `virtio-gpu-gl-pci` device
            // refuses `-display none` outright - QEMU answers "OpenGL is not
            // supported by display backend 'none'" and never starts - so
            // hardcoding it here would make a GL machine untestable, and the GL
            // half of `DRM_VIRTGPU_*` is exactly the half the default machine
            // cannot reach. The console stays on the wire either way: the test
            // image's command line names `ttyS0`, so the transcript is still the
            // file below, and the display is only somewhere for the device to
            // draw.
            let display = config.display.clone().unwrap_or_else(|| "none".to_string());
            args.extend([
                "-display".into(),
                display,
                "-serial".into(),
                format!("file:{log}"),
            ]);
        }
    }

    // The guest's own way out. `reboot(2)` with a stop command ends at
    // `drivers::qemu::exit`, which writes port 0xf4; with no device listening,
    // that write goes nowhere and the kernel halts with QEMU still up - which
    // is why `run` had no way to stop except killing the emulator. The test
    // boot reads the same port for its verdict, so it belongs to every machine
    // this builds, not to the harness alone.
    args.extend(["-monitor".into(), "none".into(), "-no-reboot".into()]);
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
pub fn needs_build(build_dir: &Path, profile: &str) -> Result<bool> {
    utils::image_stale(build_dir, profile)
}

pub fn run_qemu(
    build_dir: &Path,
    profile: &str,
    artefacts_ready: bool,
    needs_build: bool,
    mode: RunMode,
) -> Result<()> {
    if needs_build {
        println!("run: the image is missing or out of date; building it");
        if !artefacts_ready {
            // `run_qemu` is reached only for the x86_64 machine: `run --arch
            // aarch64` takes its own arm before the image machinery runs. So
            // the kernel it builds here is that machine's, named rather than
            // left to the workspace default.
            utils::build_kernel(profile, Arch::X86_64)?;
        }
        crate::disk::ensure(build_dir, profile)?;
        // The rootfs step brings the initramfs up to date itself, so there is
        // no separate call here to report it twice.
        crate::rootfs::ensure(build_dir, profile)?;
        // When `artefacts_ready` is set this is the sudo half, and its parent
        // is waiting to boot what was just built. Only a `run` that is the
        // whole command has to be told to come back.
        if !artefacts_ready {
            println!();
            println!("run: the image is built; run this command again to boot it");
        }
        return Ok(());
    }

    let image = utils::image_path(build_dir);

    // An image older than the kernel or the loader it was built from boots the
    // *previous* code and says nothing about it, which reads exactly like a fix
    // that changed nothing. `run.sh` warns and boots anyway; so does this.
    for artifact in [
        utils::kernel_path(profile, Arch::X86_64)?,
        utils::loader_path(profile, Arch::X86_64)?,
    ] {
        if utils::newer(&artifact, &image)? {
            eprintln!(
                "run: warning: {} predates {}",
                image.display(),
                artifact.display()
            );
            eprintln!("run:          it boots the code from before that build, which will");
            eprintln!("run:          look like a fix that changed nothing. Rebuild first:");
            eprintln!("run:            cargo xtask all");
            eprintln!();
        }
    }

    // The firmware two halves are separate files, and the variables half is
    // copied fresh on every run rather than written to in place: the firmware
    // writes to it, and this machine's variables are not worth keeping - a disk
    // that has never booted has no boot entry written down, which is exactly the
    // state the loader is built for. The 4 MiB build is the one that matters:
    // the smaller 2 MiB one cannot map the kernel and the initramfs at the
    // addresses the loader asks for.
    let firmware = find_firmware(Arch::X86_64)?;
    let vars = utils::ovmf_vars_path(build_dir);
    fs::create_dir_all(build_dir)?;
    let src = firmware.vars.clone();
    fs::copy(&src, &vars).with_context(|| {
        format!(
            "cannot write {}; {} is not writable",
            vars.display(),
            build_dir.display()
        )
    })?;

    // The share the 9p device exports and the console port's socket both
    // live under the build directory, and the devices they belong to start
    // only if their host-side files do - so they exist before QEMU is
    // asked for them.
    let share = build_dir.join("9p-share");
    fs::create_dir_all(&share)?;

    // The machine's own settings: memory, processors, the name the guest
    // reports, and whatever `extra_args` adds. They come from `machine.toml`
    // rather than the image's config, so tuning the machine never reinstalls
    // the image.
    let config = QemuConfig::load_default(Arch::X86_64)?;
    // The host wiring follows the mode the caller asked for, so the window, the
    // serial line and the guest's own console policy always agree.
    let args = machine_args(
        &config,
        &firmware.code,
        &vars,
        Boot::Image(image.clone()),
        Console::Run(mode),
        &share,
    );

    // Printed because a boot that goes wrong is usually read off this line.
    eprintln!("+ qemu-system-x86_64 {}", args.join(" "));
    let err = Command::new("qemu-system-x86_64").args(&args).exec();
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
/// Here rather than in `aarch64.rs` for the same reason [`machine_args`] is here:
/// the two machines' wiring is one question - how this build boots a machine -
/// and the values both read come from one `machine.toml`. What does *not* come
/// from that file is the device list below, and that is deliberate: every
/// transport here is the MMIO form because this machine discovers its devices by
/// walking its own device tree, so a `-pci` device would be one the kernel cannot
/// reach. That is the machine's fact rather than a setting, and the file says so
/// in its own note.
///
/// Taken from the boot that works, flag by flag, so what this function returns is
/// the machine the port was measured on. The reasons worth keeping are on the
/// lines below.
pub fn aarch64_machine_args(
    config: &QemuConfig,
    firmware: &Firmware,
    vars: &Path,
    esp: &Path,
    serial: &str,
    share: &Path,
) -> Vec<String> {
    let work = esp.parent().expect("the ESP path has a directory");
    let mut args: Vec<String> = vec![
        // `acpi=off` is what makes this firmware describe the machine the way
        // this port reads it, and it is a measured difference rather than a
        // preference: with ACPI left on - the default - AAVMF advertises ACPI
        // 2.0 in its configuration table and publishes **no device tree at
        // all**, which is `PLAN.md` §9.1's assumption corrected (it said `virt`
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
        "-display".into(),
        config.display.clone().unwrap_or_else(|| "none".to_string()),
        "-monitor".into(),
        "none".into(),
        "-no-reboot".into(),
        "-serial".into(),
        serial.to_string(),
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
        // The ESP itself, as a bare FAT volume on a virtio disk rather than a
        // partitioned image: EDK2's partition driver hands a volume with no
        // partition table to the FAT driver as a whole, which is how a USB
        // stick with no MBR boots. Read-only, because the loader only reads.
        "-drive".into(),
        format!(
            "file=fat:{},format=raw,if=none,id=hd0,readonly=on",
            esp.display()
        ),
        "-device".into(),
        "virtio-blk-pci,drive=hd0".into(),
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
        "-device".into(),
        "virtio-rng-device".into(),
        // The first block device: a raw image `build_esp` lays down, which
        // `check_blk` opens by `DeviceID` and verifies sector 0 of against the
        // pattern the image carries.
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=blk0,file.locking=off",
            work.join("aarch64-blk.img").display()
        ),
        "-device".into(),
        "virtio-blk-device,drive=blk0".into(),
        // The second block device: the ext4 image `build_esp` laid down, which
        // `check_fs` finds by mounting each block window until one answers. Two
        // `virtio-blk-device` instances on purpose, so the port picks the right
        // one by content rather than by the slot QEMU happened to give it.
        //
        // `file.locking=off` on both, which is what the first machine's wiring
        // already asks for and for a measured reason: with the default locking,
        // a `qemu-system-aarch64` that outlived the command that started it (a
        // boot killed at its deadline, an interrupted run) holds a write lock on
        // these files, and the *next* boot dies at once with "Failed to get
        // write lock" and an empty serial log - a machine that never started,
        // read as a machine that hung. Nothing else opens these files, so there
        // is no lock to keep.
        "-drive".into(),
        format!(
            "file={},format=raw,if=none,id=blk1,file.locking=off",
            work.join("aarch64-ext4.img").display()
        ),
        "-device".into(),
        "virtio-blk-device,drive=blk1".into(),
        // The network adapter, on a user-mode (SLIRP) backend, which `check_net`
        // opens and reads the MAC of - the net stage's transport proof on this
        // machine (`PLAN.md` §9.1). No socket leaves the machine.
        "-netdev".into(),
        format!("user,id=n0,hostname={}", config.hostname),
        "-device".into(),
        "virtio-net-device,netdev=n0".into(),
        // A virtio-input device - the fourth class the MMIO virtio path has to
        // reach (`M8_INPUT_OK`). The keyboard backend is self-contained: it
        // presents its event queue and config with no external chardev, so the
        // boot opens and negotiates the transport headless.
        "-device".into(),
        "virtio-keyboard-device".into(),
        // A host-driven console port - the fifth class (`M8_CONSOLE_OK`). Its
        // far end is a host socket nothing has to be listening on (`wait=off`),
        // so the port opens and a write lands in the socket instead of blocking
        // a machine that has nobody reading it yet.
        "-chardev".into(),
        format!(
            "socket,id=console0,path={},server=on,wait=off",
            work.join("aarch64-console.sock").display()
        ),
        "-device".into(),
        "virtio-serial-device".into(),
        "-device".into(),
        "virtconsole,chardev=console0".into(),
        // The display adapter - the sixth class (`M8_GPU_OK`). The MMIO form of
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

    if config.audio {
        // The seventh class (`M8_SOUND_OK`), the same shape the first machine
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

    // The eighth class (`M8_VSOCK_OK`) - but only where the host can back it.
    // vsock is the one virtio device QEMU does not emulate in userspace:
    // `vhost-vsock-device` is served by the host's vhost kernel module, and a
    // machine naming it without `/dev/vhost-vsock` does not start at all.
    // Attaching it only then keeps this machine's boot honest on a bare host -
    // the kernel prints the absence and answers `EAFNOSUPPORT` - while a host
    // that has it runs the class for real (`PLAN.md` §13.28). The check is here
    // as well as in the file because the file cannot see the host; the first
    // machine's arm has no such check, which is why its `vsock` key is
    // documented as "off unless the host has the module".
    if config.vsock && Path::new("/dev/vhost-vsock").exists() {
        args.extend(["-device".into(), "vhost-vsock-device,guest-cid=4".into()]);
    }

    // The ninth class (`M8_P9_OK`): the host share, the same arrangement the
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
            Console::Run(RunMode::Mirror),
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

    fn aarch64_args(config: &QemuConfig) -> Vec<String> {
        aarch64_machine_args(
            config,
            &firmware(),
            Path::new("/build/AAVMF_VARS.fd"),
            Path::new("/build/aarch64-esp"),
            "file:/build/aarch64-serial.log",
            Path::new("/build/aarch64-9p-share"),
        )
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

    /// Every device the second machine drives arrives on the **MMIO** transport,
    /// because that is how it finds its devices - by walking its own device
    /// tree. A `-pci` device here would be one the kernel cannot reach, which is
    /// why the transports are the machine's rather than a `machine.toml` key.
    #[test]
    fn the_second_machines_devices_are_the_mmio_forms() {
        let mut config = config("256", 1, &[]);
        config.audio = true;
        config.extra_args = vec!["-vga".to_string(), "none".to_string()];
        let args = aarch64_args(&config);

        for device in [
            "virtio-rng-device",
            "virtio-blk-device,drive=blk0",
            "virtio-blk-device,drive=blk1",
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
            Console::File("/build/test-serial.log".to_string()),
            Path::new("/build/9p-share"),
        );
        assert_eq!(after(&file, "-display").as_deref(), Some("none"));

        // And a machine that names a display gets it on a *test* boot too, which
        // is the one thing the file-log boot used not to allow. A
        // `virtio-gpu-gl-pci` refuses `-display none`, so without this the GL
        // machine is untestable and the GL half of `DRM_VIRTGPU_*` is reachable
        // only by hand.
        let gl_file = machine_args(
            &custom,
            Path::new("/ovmf"),
            Path::new("/build/OVMF_VARS.4m.fd"),
            Boot::Image(PathBuf::from("/build/mpos.img")),
            Console::File("/build/test-serial.log".to_string()),
            Path::new("/build/9p-share"),
        );
        assert_eq!(after(&gl_file, "-display").as_deref(), Some("gtk"));
        // The console is still the wire, whatever the display is: the test
        // image's own command line names `ttyS0`, and the transcript is still a
        // file rather than a window.
        assert_eq!(
            after(&gl_file, "-serial").as_deref(),
            Some("file:/build/test-serial.log")
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

        // Naming the GL card is the whole of the difference: exactly one word
        // of one `-device`. Pinning that makes a boot failure attributable -
        // a GL machine that will not start differs from a working 2D one by
        // the card and by nothing else.
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
            vec![(
                "virtio-gpu-pci".to_string(),
                "virtio-gpu-gl-pci".to_string()
            )],
            "the GPU choice moved something besides the card"
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
            Console::Run(mode),
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
