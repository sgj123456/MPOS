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
/// `M6_TLS_OK` is not a terminator and `M6_TLS_FAILED` is.
const TERMINATORS: [&str; 14] = [
    "M6_EL0_OK",
    "M6_EL0_FAILED",
    "M6_AS_FAILED",
    "M6_PAGING_FAILED",
    "M6_WALL_FAILED",
    "M6_CPU_FAILED",
    "M6_TLS_FAILED",
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

    let esp = build_esp(build_dir, &kernel, &loader)?;
    println!("esp:    {}", esp.display());

    let log = boot(build_dir, &esp)?;
    print!("{log}");

    // Every marker the kernel prints, in the order it prints them:
    // `M4_CONSOLE_OK` says the boot wiring reached the wire through
    // `crate::console` (stage 4a/4b - the console service, not `arch`'s own
    // writer), `M4_DEVICES_OK` says the firmware's device tree reads back as the machine
    // this kernel was built for, `M3_TABLES_OK` says the kernel's own page
    // tables say what the plan says - and that the low half is empty, which is
    // where the device window used to be - `M5_VIRTIO_OK` says those tables
    // reach the machine's virtio transports and that the transports answer as
    // transports, `M1_TIMER_OK` says the clock still reaches the vector table
    // through them (a statement about the device window too, because the GIC is
    // reached through it), `M2_CONTEXT_OK` says two tasks can suspend and
    // resume each other, `M6_AS_OK` says a process's address space can be built,
    // walked, protected, shared, copied on write and taken apart again,
    // `M6_PAGING_OK` says the kernel can ask that space the questions a running
    // process needs answered - which root is active, which is the kernel's, is a
    // range touchable, is a page copy-on-write, write a word through a root -
    // `M6_WALL_OK` says the machine has a persistent clock this kernel can reach
    // through the device window, `M6_CPU_OK` says user SIMD is turned on and a
    // line can be masked and an interrupt waited on, `M6_TLS_OK` says the
    // scheduler's thread-pointer write reaches `TPIDR_EL0` and reads back through
    // the `hal::Cpu` capability, and `M6_EL0_OK` says the kernel ran a program at
    // EL0 and answered the call it made.
    let markers = [
        ("M4_CONSOLE_OK", "console service"),
        ("M4_DEVICES_OK", "devices"),
        ("M3_TABLES_OK", "page tables"),
        ("M5_VIRTIO_OK", "virtio transports"),
        ("M1_TIMER_OK", "timer"),
        ("M2_CONTEXT_OK", "contexts"),
        ("M6_AS_OK", "user address space"),
        ("M6_PAGING_OK", "running-process paging"),
        ("M6_WALL_OK", "wall clock"),
        ("M6_CPU_OK", "cpu and interrupt primitives"),
        ("M6_TLS_OK", "thread pointer"),
        ("M6_EL0_OK", "EL0"),
    ];
    let missing: Vec<&str> = markers
        .iter()
        .filter(|(marker, _)| !log.contains(marker))
        .map(|(_, what)| *what)
        .collect();
    if missing.is_empty() {
        println!();
        println!(
            "aarch64 boot: M4_CONSOLE_OK, M4_DEVICES_OK, M3_TABLES_OK, M5_VIRTIO_OK, \
             M1_TIMER_OK, M2_CONTEXT_OK, M6_AS_OK, M6_PAGING_OK, M6_WALL_OK, M6_CPU_OK, \
             M6_TLS_OK and M6_EL0_OK, from the ESP under AAVMF"
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
fn build_esp(build_dir: &Path, kernel: &Path, loader: &Path) -> Result<PathBuf> {
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

    // The third file is a real aarch64 `/init`, not the x86_64 image's archive.
    // `aarch64_init` hand-encodes a static `EM_AARCH64` `ET_EXEC` whose whole
    // program is `exit_group(0)`, and packs it ustar. The loader copies it into
    // frames and records its address and length in the handoff, so this now
    // proves a *second* file crosses the firmware's file protocols and lands in
    // the direct map - the same thing the old optional x86 archive copy was only
    // standing in for. The kernel does not read it yet: it still halts after
    // `check_el0`, and the init handoff that mounts a ramfs and spawns this
    // `/init` is the next cut (PLAN.md §9.1 stage 6E). It is laid down now so the
    // image and the boot wiring can be landed and pinned independently.
    let initramfs = build_dir.join("aarch64-initramfs.tar");
    crate::aarch64_init::write_initramfs(build_dir, &initramfs)?;
    fs::copy(&initramfs, esp.join("initramfs.tar"))
        .with_context(|| format!("Failed to install {}", initramfs.display()))?;
    println!("initramfs: aarch64 /init -> {}", initramfs.display());
    Ok(esp)
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

/// Boot the ESP under AAVMF and return what the serial line said.
fn boot(build_dir: &Path, esp: &Path) -> Result<String> {
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
        .args(["-serial", &format!("file:{}", log.display())])
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
        // And the transports have to be the *modern* ones, which on this machine
        // is a choice somebody has to make: measured, `virt`'s `virtio-mmio`
        // transports answer `Version` 1 - the legacy register layout - unless
        // this says otherwise, and version 2 is the only layout this kernel's
        // driver crate reads. The alternative to selecting it is a kernel that
        // either drives the legacy layout (a second register map, a second
        // feature-negotiation order) or refuses devices on its own machine, and
        // neither is what stage 5 is for.
        .args(["-global", "virtio-mmio.force-legacy=false"]);

    let mut child = qemu
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start qemu-system-aarch64")?;

    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(&log)
            && TERMINATORS.iter().any(|marker| text.contains(marker))
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
