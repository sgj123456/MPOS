//! Device drivers, separated from architecture-specific code.
//!
//! This module contains hardware drivers that were previously part of
//! `arch::x86_64` or `blk`. They are architecture-aware (using port I/O,
//! PCI enumeration) but are not part of the core architecture abstraction.

/// Every driver below `virtio` is x86_64's machine: port I/O, `uart_16550`,
/// PCI enumeration, AHCI, DRM. On aarch64 they are `#[cfg]`-ed out - a no-op
/// there, exactly as `virtio::pci` and the console `vcmux` already were
/// (`PLAN.md` §9.1's prep shape). `virtio` is the exception: its `Hal` impl is
/// machine-neutral (the MMIO-mapping seam took the last x86_64-only call out of
/// it), so it compiles on both, and the aarch64 MMIO transport is added there.
/// `devices` is the other exception, for the same reason and by the same method:
/// the `devfs` it builds is what `mount -t devtmpfs /dev` hands a real init, so
/// it must exist on aarch64 - and it does, because the x86_64-only device nodes
/// (the virtio-PCI `hwrng`/`hvc`/`dsp`/`fb0`, `/dev/input`, `uinput` and the DRM
/// card) are `#[cfg]`-ed out inside it, leaving the generic set (the ttys,
/// `console`, `null`/`zero`/`full`, the entropy pair, `kmsg`, the `/proc` links
/// and the disks) that every machine has.
#[cfg(target_arch = "x86_64")]
pub mod ahci;
pub mod devices;
#[cfg(target_arch = "x86_64")]
pub mod drm;
#[cfg(target_arch = "x86_64")]
pub mod pci;
#[cfg(target_arch = "x86_64")]
pub mod qemu;
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub mod rtc_dev;
#[cfg(target_arch = "x86_64")]
pub mod serial;
#[cfg(target_arch = "x86_64")]
pub mod uinput;
#[cfg(target_arch = "x86_64")]
pub mod video;
pub mod virtio;
