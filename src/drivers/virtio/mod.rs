//! Virtio drivers.
//!
//! Every device the `virtio-drivers` crate can drive for this kernel is
//! driven from here: the block device, the network adapter, and the six
//! that came with them - the entropy source, the console port, the GPU,
//! the input devices, the sound card, the 9p share and the vsock device.
//! The crate implements VirtIO 1.0 itself: the queues, the feature
//! negotiation, the configuration space. What is left to a driver here is
//! which device to open and what the rest of the kernel calls the result.
//!
//! [`hal`] is the one thing the crate cannot provide, and the one thing
//! this module has to: how to allocate memory a device can reach, and how
//! to turn an address the device gives back into a pointer the kernel can
//! use. [`hal::transport`] is the other shared piece - the probe-and-open
//! step every driver starts with, so the only part that differs between
//! them is the ID the device answers.

pub mod blk;
pub mod console;
pub mod fs9p;
pub mod gpu;
pub mod hal;
pub mod input;
pub mod msix;
pub mod net;
pub mod rng;
pub mod sound;
pub mod vsock;

/// The name a virtio function's driver has in `/sys`, and the one `lspci -k`
/// prints as "Kernel driver in use".
///
/// One name for all of them, because that is what Linux has: `virtio-pci` is
/// the PCI driver, and the block device, the network adapter and the rest are
/// *children* of it. What a device's `driver` link names is the thing that
/// claimed the PCI function, not the thing that claimed the disk.
pub const PCI_DRIVER: &str = "virtio-pci";

/// The task that keeps the polled devices moving between userland calls.
///
/// Two families need it: the input devices, whose events arrive whether
/// or not anyone is reading, and the vsock family, whose handshakes and
/// buffers advance while nobody is parked on them. One task polls both
/// once a tick - the same arrangement the network stack's service task
/// keeps with its interface - because a reader blocked on either is
/// waiting on a device nobody else is going to ask.
pub fn spawn_service() {
    // Permanent: a listener parked in `accept` and a reader waiting on an
    // input event both depend on this task for as long as the machine runs.
    // The boot code's `drain` waits for every task a check started to be
    // reaped; this one never will be, which is what `spawn_permanent` is
    // for.
    crate::sched::spawn_permanent(
        || loop {
            input::service();
            crate::net::vsock::service();
            // The host's own events for the display. A window resize or a
            // fullscreen toggle arrives as `VIRTIO_GPU_EVENT_DISPLAY` in the
            // GPU's device configuration, and one word read a tick is what
            // notices it - the same cheap poll as the two above, and the
            // reason a display resize needs no interrupt. The *size* is not
            // read here: the render task owns the device and does the rebuild.
            if crate::drivers::video::render::running() && gpu::take_display_event() {
                crate::drivers::video::render::request_resize();
            }
            crate::sched::sleep_ticks(1);
        },
        crate::sched::Priority::NORMAL,
    );
}
