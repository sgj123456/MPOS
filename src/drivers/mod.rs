//! Device drivers, separated from architecture-specific code.
//!
//! This module contains hardware drivers that were previously part of
//! `arch::x86_64` or `blk`. They are architecture-aware (using port I/O,
//! PCI enumeration) but are not part of the core architecture abstraction.

pub mod pci;
pub mod qemu;
pub mod serial;
pub mod virtio;
