//! Device drivers, separated from architecture-specific code.
//!
//! This module contains hardware drivers that were previously part of
//! `arch::x86_64` or `blk`. They are architecture-aware (using port I/O,
//! PCI enumeration) but are not part of the core architecture abstraction.

/// Two of the drivers below are one machine's and two are both machines'.
/// `ahci` and `pci` used to be x86_64's alone, and the reason was not the
/// driver: it was that this machine had no way to reach a BAR. Closing that
/// (`arch::aarch64::mmio`, a bump-allocated window above the fixed one, and
/// ECAM read from the device tree when the firmware publishes no MCFG) made
/// both of them ordinary drivers again, so the `#[cfg]` came off rather than
/// being replaced with a second copy - `NOTES.md` §9.1's "a PCIe aarch64
/// server that hands out BARs to map dynamically is a named gap" is closed,
/// and `t-ahci` is migratable. `virtio` is the exception of a different kind:
/// its `Hal` impl is
/// machine-neutral (the MMIO-mapping seam took the last x86_64-only call out of
/// it), so it compiles on both, and the aarch64 MMIO transport is added there.
/// `devices` is the other exception, for the same reason and by the same method:
/// the `devfs` it builds is what `mount -t devtmpfs /dev` hands a real init, so
/// it must exist on aarch64 - and it does, because the nodes that are still one
/// machine's alone (the virtio-PCI `dsp`/`fb0` and the DRM card) are `#[cfg]`-ed
/// out inside it, on top of the generic set (the ttys, `console`,
/// `null`/`zero`/`full`, the entropy pair, `kmsg`, the `/proc` links and the
/// disks) that every machine has. `/dev/hwrng`, `/dev/hvc0` and the input nodes
/// are *not* gated any more: each is published by whichever machine opened the
/// device, through the driver module the two machines share (`NOTES.md`
/// §13.24/§13.25).
pub mod ahci;
pub mod devices;
pub mod drm;
/// PCI configuration space, and the snapshot of the bus behind it. On both
/// machines now: the window is the device tree's ECAM when the firmware
/// published no MCFG, and the port pair it falls back to is x86_64's alone
/// (`ports`, inside the module) - a machine with no I/O address space has an
/// absent-function answer there instead.
pub mod pci;
/// The real-time clock's device node - `/dev/rtc0`, and the `RTC_*` ioctls'
/// conversion between a Unix second and the nine fields `struct rtc_time` spells.
/// Machine-neutral, and the only thing that was not was the `target_arch` gate
/// here: what the node reads and writes is `crate::hal::WallClock`, which both
/// machines implement (the CMOS and the PL031), so it is compiled wherever there
/// is a machine (`NOTES.md` §13.15).
#[cfg(target_os = "none")]
pub mod rtc_dev;
/// The userspace input-device interface: a program that opens `/dev/uinput`
/// creates virtual devices, and they land in the evdev registry beside the real
/// ones. Gated on the *capability* rather than the architecture, like the
/// registry it feeds (`drivers::virtio::input`, which this module names for
/// `EvdevNode`): a machine with no registry has nowhere for a created device to
/// go, so the two are one switch - and uinput's exit hook is the same switch
/// (`board::steps::register_exit_hooks`).
#[cfg(mpos_input_registry)]
pub mod uinput;
pub mod video;
pub mod virtio;
