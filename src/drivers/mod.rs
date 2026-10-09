//! Device drivers, separated from architecture-specific code.
//!
//! This module contains hardware drivers that were previously part of
//! `arch::x86_64` or `blk`. They are architecture-aware (using port I/O,
//! PCI enumeration) but are not part of the core architecture abstraction.

/// Every driver below `virtio` is x86_64's machine: port I/O, `uart_16550`,
/// PCI enumeration, AHCI, DRM. On aarch64 they are `#[cfg]`-ed out - a no-op
/// there, exactly as `virtio::pci` and the console `vcmux` already were
/// (`NOTES.md` §9.1's prep shape). `virtio` is the exception: its `Hal` impl is
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
#[cfg(target_arch = "x86_64")]
pub mod ahci;
pub mod devices;
pub mod drm;
#[cfg(target_arch = "x86_64")]
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
