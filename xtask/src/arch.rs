//! The machines this front end builds for, and the facts that differ between
//! them.
//!
//! `--arch` is one switch, so which machine a command is for is parsed once, up
//! front, into an [`Arch`] - and everything downstream names the machine by
//! that value rather than by comparing a string again. That is the whole point:
//! the switch used to reach only `run` and `test`, while `build` silently
//! compiled the x86_64 kernel no matter what `--arch` said, because the string
//! never travelled as far as the `cargo` invocation. A target is a property of
//! the machine, so it is answered here and nowhere else - `boot-aarch64` used
//! to carry its own copy of both target triples for exactly that reason.
//!
//! What is *not* here is what each machine's boot needs: the ESP layout, the
//! firmware, the disk. Those live in `disk.rs` and `qemu.rs`, both of which
//! take an [`Arch`] and answer per-machine from it - which is the whole point
//! of this module: the two machines' boot paths are *one* body parameterised by
//! this type, not two bodies (`PLAN.md` §15.18).

use anyhow::Result;

/// A machine this front end builds and boots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The word `--arch` accepts for this machine.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The word's [`Arch`], or a refusal naming it.
    ///
    /// An unrecognised machine is an error rather than a fall-through: a build
    /// that quietly compiles one machine while reporting another is the one
    /// failure this must not have, and that is exactly what `--arch aarch64`
    /// used to do (`AGENTS.md` §3).
    pub fn parse(text: &str) -> Result<Arch> {
        match text {
            "x86_64" => Ok(Arch::X86_64),
            "aarch64" => Ok(Arch::Aarch64),
            other => anyhow::bail!(
                "--arch {other}: this front end builds and boots x86_64 and aarch64 only, \
                 and a verdict for a machine nothing booted is the one failure it must not \
                 have (PLAN.md §13.5)"
            ),
        }
    }

    /// The target triple the *kernel* is compiled for.
    ///
    /// `x86_64-unknown-none` is also the workspace default (`.cargo/config.toml`
    /// pins `[build] target`), so naming it here costs nothing and keeps the two
    /// machines' arms spelling the same thing.
    pub fn kernel_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-none",
            Arch::Aarch64 => "aarch64-unknown-none",
        }
    }

    /// The target triple the *user programs* a sweep runs are compiled for.
    ///
    /// Static musl on both machines: the loader execs these with no dynamic
    /// linker to find, and the same crate builds them - so a sweep's programs
    /// are one `cargo build` whose target is this machine's answer.
    pub const fn musl_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-linux-musl",
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }

    /// The target triple the UEFI *loader* is compiled for.
    ///
    /// Always passed to cargo rather than left to `boot/.cargo/config.toml`,
    /// whose `[build] target` names the x86_64 one: a config file cannot know
    /// which machine this invocation is for, and guessing there would break the
    /// plain `cargo build` inside `boot/`.
    pub fn loader_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-uefi",
            Arch::Aarch64 => "aarch64-unknown-uefi",
        }
    }

    /// The QEMU system emulator that runs this machine.
    ///
    /// The one name a boot cannot derive from the target triple: it is the
    /// emulator's own, and `qemu-system-aarch64` on an aarch64 *host* would be
    /// a different machine than the one this front end builds.
    pub const fn qemu_binary(self) -> &'static str {
        match self {
            Arch::X86_64 => "qemu-system-x86_64",
            Arch::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// The UEFI variable store's file name under `build/`, per machine.
    ///
    /// Each machine's firmware ships its own - OVMF's is the 4 MiB build,
    /// AAVMF's is not - so the copy a boot writes has to be the one that
    /// firmware can read, and a shared name would hand the second machine the
    /// first one's variables.
    pub const fn firmware_vars(self) -> &'static str {
        match self {
            Arch::X86_64 => "OVMF_VARS.4m.fd",
            Arch::Aarch64 => "AAVMF_VARS.fd",
        }
    }

    /// The name this machine's console has on the kernel command line and in
    /// `/dev`.
    ///
    /// `ttyS0` is the 16550 the PC machine carries; `ttyAMA0` is the PL011 the
    /// `virt` machine's device tree describes. Every place that writes a
    /// `console=` or an `/etc/inittab` line has to agree with the driver that
    /// answers, and asking here is what keeps them from disagreeing.
    pub const fn console(self) -> &'static str {
        match self {
            Arch::X86_64 => "ttyS0",
            Arch::Aarch64 => "ttyAMA0",
        }
    }

    /// The file name the firmware looks for in `EFI/BOOT/` on the ESP.
    ///
    /// Fixed by the UEFI specification per machine, not chosen: a loader
    /// installed under any other name is not bootable, and the failure is a
    /// firmware message about no bootable device rather than anything naming
    /// the file.
    pub fn uefi_boot_file(self) -> &'static str {
        match self {
            Arch::X86_64 => "BOOTX64.EFI",
            Arch::Aarch64 => "BOOTAA64.EFI",
        }
    }

    /// The one disk image this machine boots, under `build/`.
    ///
    /// Both are the same thing now - a GPT carrying an ESP and an ext4 root -
    /// and the names are historic rather than meaningful, so they are kept:
    /// the first machine's is what `rootfs` and `disk` have always written,
    /// and renaming it would invalidate every stamp on disk.
    pub fn disk_image(self) -> &'static str {
        match self {
            Arch::X86_64 => "mpos-uefi-x86_64.img",
            Arch::Aarch64 => "aarch64-disk.img",
        }
    }

    /// The overlay tree that carries this machine's half of a root filesystem:
    /// its `/init`, its `etc/`, whatever only this machine needs.
    ///
    /// `rootfs-core/` is shared and is merged before it by
    /// `rootfs::merge_overlays`; this is the per-machine tree merged after.
    pub fn root_overlay(self) -> &'static str {
        match self {
            Arch::X86_64 => "rootfs",
            Arch::Aarch64 => "rootfs-aarch64",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Arch;

    /// The counterfactual for the silent wrong-run: `--arch aarch64` used to
    /// reach `selftest::run_tests` unchanged and print an x86_64 verdict under
    /// an aarch64 banner. A machine this front end does not build is now
    /// refused by name rather than run as another.
    #[test]
    fn a_machine_this_front_end_does_not_build_is_refused_by_name() {
        assert_eq!(Arch::parse("x86_64").unwrap(), Arch::X86_64);
        assert_eq!(Arch::parse("aarch64").unwrap(), Arch::Aarch64);
        let refusal = Arch::parse("riscv64")
            .expect_err("a machine this front end does not build must be refused")
            .to_string();
        assert!(
            refusal.contains("riscv64") && refusal.contains("x86_64"),
            "the refusal must name the machine asked for and the ones that are \
             built: {refusal}"
        );
    }

    /// The one switch has to reach the compiler: a machine whose kernel target
    /// is not the workspace default is the whole reason this exists.
    #[test]
    fn each_machine_names_its_own_kernel_and_loader_targets() {
        assert_eq!(Arch::X86_64.kernel_target(), "x86_64-unknown-none");
        assert_eq!(Arch::Aarch64.kernel_target(), "aarch64-unknown-none");
        assert_eq!(Arch::X86_64.loader_target(), "x86_64-unknown-uefi");
        assert_eq!(Arch::Aarch64.loader_target(), "aarch64-unknown-uefi");
    }
}
