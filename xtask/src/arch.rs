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
//! firmware, the disk. Those live in `aarch64.rs` and `qemu.rs`, which is where
//! the two machines' paths genuinely diverge.

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
