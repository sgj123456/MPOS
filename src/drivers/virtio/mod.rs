//! Virtio drivers.
//!
//! Both devices this kernel talks to - a block device and a network adapter -
//! are driven by the `virtio-drivers` crate, which implements VirtIO 1.0
//! itself: the queues, the feature negotiation, the configuration space. What
//! is left to a driver here is which device to open and what the rest of the
//! kernel calls the result.
//!
//! [`hal`] is the one thing the crate cannot provide, and the one thing this
//! module has to: how to allocate memory a device can reach, and how to turn
//! an address the device gives back into a pointer the kernel can use.

pub mod blk;
pub mod hal;
pub mod msix;
pub mod net;
