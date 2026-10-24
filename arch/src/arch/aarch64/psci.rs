//! PSCI, as the firmware's device tree states it: this machine's way out.
//!
//! The first machine ends a run by writing QEMU's debug-exit port - a wire
//! that exists because that machine boots with one. This machine has no such
//! port; what it has is the firmware's power-management interface, which
//! QEMU's `virt` machine publishes in its device tree as an `arm,psci-*` node
//! carrying the *conduit* - the instruction an EL1 caller raises the call
//! through (`hvc` or `smc`). `SYSTEM_OFF` through that conduit is what a
//! `poweroff -f` means here: the firmware turns the machine off, and QEMU -
//! which emulates the firmware - exits with it. Before this existed the
//! kernel's answer to "end the machine" was [`super::halt`], which parks the
//! core under a `wfi` with the clock still running: the emulator lived on,
//! a sweep that ended had to be killed by its harness, and an operator's
//! `poweroff` looked like a hang.
//!
//! The conduit is read from the tree rather than written down, for the same
//! reason every other device address here is: QEMU's choice (hvc when the
//! guest runs without EL2, smc when the firmware occupies it) is the
//! machine's, and a caller that guesses holds the wrong instruction for
//! whichever boot guesses wrong. A tree that names neither - or carries no
//! `psci` node at all - leaves the conduit at "none", and the machine's way
//! out falls back to the park, which is the honest answer a machine without
//! PSCI has.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use mpos_bootinfo::BootInfo;
use mpos_fdt as fdt;

/// PSCI `SYSTEM_OFF`: the firmware turns the machine off and the call does
/// not return. 0x8400_0008 is the 32-bit calling convention's function id,
/// which is what a 64-bit caller passes in `x0` for this call (PSCI 1.0's
/// own table).
const SYSTEM_OFF: u64 = 0x8400_0008;

/// The conduit the tree named: 0 none, 1 `hvc`, 2 `smc`.
static CONDUIT: AtomicU8 = AtomicU8::new(0);

/// Read the firmware's PSCI node and remember the conduit it names.
///
/// Called once, from the machine's own boot, beside the device-tree walk the
/// rest of the machine's discovery runs in: the tree is the loader's, still
/// mapped, and this node is as much a fact about the machine as the GIC's
/// address is. A tree that cannot be walked or names no method leaves the
/// conduit at "none" rather than guessing - [`system_off`] falls back to the
/// park, which is where the machine would have been anyway.
pub fn init(info: &BootInfo) {
    let Some(bytes) = info.dtb else {
        return;
    };
    let Ok(tree) = fdt::Fdt::new(bytes) else {
        return;
    };
    let Ok(Some(node)) = tree.find(|node| node.name() == "psci") else {
        return;
    };
    match node.string_property("method") {
        Some("hvc") => CONDUIT.store(1, Ordering::Relaxed),
        Some("smc") => CONDUIT.store(2, Ordering::Relaxed),
        _ => {}
    }
}

/// Whether the firmware named a conduit, for the boot's own report.
pub fn available() -> bool {
    CONDUIT.load(Ordering::Relaxed) != 0
}

/// End the machine: `SYSTEM_OFF` through the conduit the tree named.
///
/// The call does not return on a firmware that implements it - the machine is
/// off, and the emulator that emulates the firmware exits with it. A machine
/// whose tree named no conduit parks instead, which is [`super::halt`]'s
/// answer and the one an operator of such a machine already knew.
pub fn system_off() -> ! {
    match CONDUIT.load(Ordering::Relaxed) {
        1 => {
            // SAFETY: an HVC call at EL1 with the function id in `x0` is the
            // documented way to reach a PSCI whose conduit the tree names as
            // `hvc`; the firmware at the other end either acts on the call or
            // returns, and no guest state is disturbed either way.
            unsafe { asm!("hvc #0", in("x0") SYSTEM_OFF, in("x1") 0, options(nomem)) };
        }
        2 => {
            // SAFETY: as the `hvc` arm, through the SMC conduit - the call
            // traps to the firmware at EL2, which acts on it or returns.
            unsafe { asm!("smc #0", in("x0") SYSTEM_OFF, in("x1") 0, options(nomem)) };
        }
        _ => {}
    }
    // A firmware that returned from SYSTEM_OFF (it implements none of it, or
    // refused) has left the machine running: park, the way every other
    // dead-end here does.
    super::halt()
}
