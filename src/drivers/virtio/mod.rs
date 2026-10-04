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

//! Every driver here except `hal` reaches its device over PCI today, so the
//! PCI-only ones are x86_64's. `hal` is the machine-neutral `Hal` impl the
//! aarch64 MMIO transport builds on (`PLAN.md` §9.1); the aarch64 `rng`/`blk`/
//! `net` over MMIO come as the drivers stage proceeds, so `rng` is gated to
//! x86_64 until its MMIO path exists.

#[cfg(target_arch = "x86_64")]
pub mod blk;
#[cfg(target_arch = "x86_64")]
pub mod console;
#[cfg(target_arch = "x86_64")]
pub mod fs9p;
#[cfg(target_arch = "x86_64")]
pub mod gpu;
#[cfg(target_arch = "x86_64")]
pub mod gpu_ctrl;
pub mod hal;
#[cfg(target_arch = "x86_64")]
pub mod input;
#[cfg(target_arch = "x86_64")]
pub mod msix;
#[cfg(target_arch = "x86_64")]
pub mod net;
#[cfg(target_arch = "x86_64")]
pub mod rng;
#[cfg(target_arch = "x86_64")]
pub mod sound;
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]
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
            // read here and the screen is not rebuilt here: the render task
            // owns the device, re-reads the size and advertises it as a hotplug
            // uevent, and the scanout only follows a compositor's commit.
            if crate::drivers::video::render::running() && gpu::take_display_event() {
                crate::drivers::video::render::request_display_probe();
            }
            crate::sched::sleep_ticks(1);
        },
        crate::sched::Priority::NORMAL,
    );
}
