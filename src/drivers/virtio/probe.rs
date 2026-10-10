//! Device discovery: what this machine has, and the one pass that finds it.
//!
//! Linux's shape, and deliberately so. A kernel's job at bring-up is to find the
//! devices the machine describes, open the ones this kernel has a driver for,
//! register each with the subsystem that owns it, and say what it found. It
//! mounts nothing: the pseudo-filesystems, the real root and the host share are
//! the early userland's, because only their owner can unmount them again
//! (`assets/initramfs/core/init`, `docs/NOTES.md` §15.6). Nothing here halts the
//! machine either: a device that did not answer is a line in the log and an
//! absent node, which is what lets a distribution's own init run on the machine.
//! What did get registered is asserted by the test build's `devices` suite
//! (`src/board/suites.rs`) - a build (`--features selftest`), not a word on the
//! command line, so an ordinary boot carries none of it (§15.19).
//!
//! # Two transports, one pass
//!
//! Which bus carries a device is a fact about the *machine*; which device the
//! kernel wants is a fact about the *class*. So every class here asks both: the
//! PCI front-end first (a machine whose devices hang off a bus answers with them,
//! and a machine with no bus answers with nothing - `drivers::pci` returns an
//! empty bus), then the memory-mapped windows the record's device tree describes
//! ([`mmio::windows`], which is empty on a machine that describes itself with
//! ACPI). What comes back is registered through the class's own contract
//! (`NetDevice`, `P9Device`, `ConsolePort`, ...), so nothing above this module -
//! and nothing in the boot flow - learns which transport answered. That is what
//! makes the boot have *one* device step rather than one per machine.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use mpos_bootinfo::BootInfo;

use super::gpu::VirtioGpuDev;
use super::gpu_ctrl::GpuDevice;
use super::{blk, console, fs9p, gpu, input, mmio, net, rng, sound, vsock};

// The virtio device IDs this kernel looks for, as the specification fixes them.
// They are the *class* a device belongs to, which is the question a probe asks of
// a window whose contents nothing else describes.
const VIRTIO_ID_NET: u32 = 1;
const VIRTIO_ID_BLOCK: u32 = 2;
const VIRTIO_ID_CONSOLE: u32 = 3;
const VIRTIO_ID_ENTROPY: u32 = 4;
const VIRTIO_ID_9P: u32 = 9;
const VIRTIO_ID_GPU: u32 = 16;
const VIRTIO_ID_INPUT: u32 = 18;
const VIRTIO_ID_VSOCK: u32 = 19;
const VIRTIO_ID_SOUND: u32 = 25;

/// Open and register every device class this machine has, except the display.
///
/// The display is not here on purpose: it is the one class whose *owner* is a
/// task, so the boot opens it where it can hand it straight over ([`gpu_card`]),
/// once there is a scheduler to start that task on.
pub fn all(info: &BootInfo) {
    entropy(info);
    storage(info);
    network(info);
    #[cfg(mpos_input_registry)]
    keyboard(info);
    console_port(info);
    sound_card(info);
    sockets(info);
    shares(info);
}

/// This machine's display card, if it has one: the device the render task owns
/// and the buffer view `/dev/fb0` reads through.
///
/// The device arrives erased ([`GpuDevice`]) because the two transports open
/// different concrete types and nothing above the transport differs - a
/// `Gpu<PciTransport>` on one machine and a `Gpu<MmioTransport>` on the other.
/// Erasing it here is what lets the boot have one call site for "a display was
/// found" instead of one per machine.
pub fn gpu_card(info: &BootInfo) -> Option<(Box<dyn GpuDevice>, Arc<VirtioGpuDev>)> {
    if let Some((view, device)) = gpu::init_global() {
        return Some((Box::new(device), view));
    }
    for (virt, size) in mmio::windows(info, VIRTIO_ID_GPU) {
        if let Some((view, device)) = mmio::gpu_device(virt, size) {
            return Some((Box::new(device), view));
        }
    }
    None
}

/// The entropy source, if the machine has one.
///
/// Both front-ends install what they open (`rng::install_probed`), so there is
/// nothing to hand on here - and what is installed is read back by both the seed
/// and `/dev/hwrng`, which is why the source is a global rather than a value the
/// boot carries.
fn entropy(info: &BootInfo) {
    let installed = rng::init_global().is_some()
        || mmio::windows(info, VIRTIO_ID_ENTROPY)
            .into_iter()
            .any(|(virt, size)| mmio::rng_device(virt, size).is_some());
    if installed {
        crate::println!("virtio-rng: host entropy mixed into the kernel seed");
    } else {
        crate::println!("virtio-rng: no device; the seed stands on the CPU's own source alone");
    }
}

/// The boot disk, registered as `vda`.
///
/// More than one device can answer as a block device - the test wiring attaches a
/// second, deliberately raw one - and *which* of them is the root is the kernel's
/// own question rather than a launcher's: [`mpos::blk::part::find_filesystem`]
/// reads the protective MBR, the GPT and the ext4 superblock, so the disk it
/// answers for is the one a filesystem is on rather than the one at a remembered
/// slot. A machine whose disks carry no filesystem at all still gets its first one
/// registered, because a disk is a disk and a filesystem-less one is what a
/// payload writes to.
fn storage(info: &BootInfo) {
    let mut candidates: Vec<Arc<dyn crate::blk::BlockDev>> = Vec::new();
    if let Some(dev) = blk::VirtioBlk::init() {
        candidates.push(dev);
    }
    for (virt, size) in mmio::windows(info, VIRTIO_ID_BLOCK) {
        if let Some(dev) = mmio::blockdev(virt, size) {
            candidates.push(dev);
        }
    }
    if candidates.is_empty() {
        crate::println!("virtio-blk: no device; there is no disk to mount a root from");
        return;
    }
    let root = candidates
        .iter()
        .position(|dev| crate::blk::part::find_filesystem(dev.clone()).is_some())
        .unwrap_or(0);
    let dev = candidates.swap_remove(root);
    let sectors = dev.capacity_sectors();
    crate::println!(
        "virtio-blk: vda, {} sectors ({} MiB)",
        sectors,
        sectors * 512 / (1024 * 1024)
    );
    super::register_block_disk(dev);
}

/// The network adapter, published in the one slot the stack's bring-up reads.
///
/// Published rather than handed straight to `net::init` because the two halves
/// cannot be the same step: the device is found as soon as the kernel can reach
/// device memory, while the stack over it starts with a service task, which needs
/// a scheduler. A machine with no adapter leaves the slot empty and the socket
/// calls answer `ENETDOWN`.
fn network(info: &BootInfo) {
    if let Some(dev) = net::VirtioNet::init() {
        crate::println!("virtio-net: adapter on the bus");
        net::install(dev);
        return;
    }
    for (virt, size) in mmio::windows(info, VIRTIO_ID_NET) {
        if let Some(dev) = mmio::net(virt, size) {
            crate::println!("virtio-net: adapter at {virt:#x}");
            net::install(dev);
            return;
        }
    }
    crate::println!("virtio-net: no adapter; sockets answer ENETDOWN");
}

/// The input devices, into the evdev registry `/dev/input` is published from.
///
/// Every one the machine has, not the first: a keyboard and a tablet are two
/// nodes, and a compositor is written against the second as much as the first.
#[cfg(mpos_input_registry)]
fn keyboard(info: &BootInfo) {
    let nodes = input::init();
    if !nodes.is_empty() {
        for (index, (_, name)) in nodes.iter().enumerate() {
            crate::println!("virtio-input: event{index}: {name}");
        }
        return;
    }
    let mut opened = Vec::new();
    for (virt, size) in mmio::windows(info, VIRTIO_ID_INPUT) {
        if let Some(dev) = mmio::input_device(virt, size) {
            opened.push(dev);
        }
    }
    if opened.is_empty() {
        crate::println!("virtio-input: no device on the bus; /dev/input stays empty");
        return;
    }
    for (index, (_, name)) in input::register(opened).iter().enumerate() {
        crate::println!("virtio-input: event{index}: {name}");
    }
}

/// The host-driven console port (`/dev/hvc0`), a second serial line whose far end
/// is whatever the host wired it to.
fn console_port(info: &BootInfo) {
    let port = console::init_global().or_else(|| {
        mmio::windows(info, VIRTIO_ID_CONSOLE)
            .into_iter()
            .find_map(|(virt, size)| mmio::console_device(virt, size).map(console::install))
    });
    match port {
        Some(port) => match port.size() {
            Some((columns, rows)) => {
                crate::println!("virtio-console: {columns}x{rows} at /dev/hvc0")
            }
            None => crate::println!("virtio-console: up at /dev/hvc0"),
        },
        None => crate::println!("virtio-console: no port on this machine; /dev/hvc0 is absent"),
    }
}

/// The sound card (`/dev/dsp`).
fn sound_card(info: &BootInfo) {
    let card = sound::init_global().or_else(|| {
        mmio::windows(info, VIRTIO_ID_SOUND)
            .into_iter()
            .find_map(|(virt, size)| mmio::sound_device(virt, size))
    });
    match card {
        Some(card) => {
            let (rate, channels) = card.format();
            crate::println!("virtio-sound: {rate} Hz, {channels} channels at /dev/dsp");
        }
        None => crate::println!("virtio-sound: no card on this machine; /dev/dsp is absent"),
    }
}

/// The `AF_VSOCK` device, handed to the family that serves the sockets over it.
///
/// The device is the transport and the family is the protocol, so this is the one
/// class whose probe ends in another subsystem's door: `net::vsock::init` is what
/// makes `socket(AF_VSOCK)` answer, and it is a device fact that a machine without
/// one refuses with `EAFNOSUPPORT`.
fn sockets(info: &BootInfo) {
    let dev = vsock::init_global().or_else(|| {
        mmio::windows(info, VIRTIO_ID_VSOCK)
            .into_iter()
            .find_map(|(virt, size)| mmio::vsock_device(virt, size))
    });
    match dev {
        Some(dev) => {
            crate::println!(
                "virtio-vsock: guest cid {} answers on AF_VSOCK",
                dev.guest_cid()
            );
            crate::net::vsock::init(dev);
        }
        None => crate::println!("virtio-vsock: no device; AF_VSOCK answers EAFNOSUPPORT"),
    }
}

/// The host's shares, into the registry `mount -t 9p` reads.
///
/// The *mount* is not this function's and never was: a share is mounted by
/// whoever owns the tree it lands on - the initramfs's own `/init` on the root it
/// is about to promote (`NOTES.md` §15.6) - and what the kernel owes that mount is
/// the device, registered under the tag the host exported it with.
fn shares(info: &BootInfo) {
    let found = fs9p::init_global();
    if !found.is_empty() {
        for dev in &found {
            crate::println!("virtio-9p: \"{}\" on the bus", dev.mount_tag());
        }
        return;
    }
    let opened: Vec<_> = mmio::windows(info, VIRTIO_ID_9P)
        .into_iter()
        .filter_map(|(virt, size)| mmio::p9_device(virt, size))
        .collect();
    if opened.is_empty() {
        crate::println!("virtio-9p: no share on the bus");
        return;
    }
    for dev in fs9p::install(opened) {
        crate::println!("virtio-9p: \"{}\" on the bus", dev.mount_tag());
    }
}
