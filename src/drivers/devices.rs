//! The devices userland is handed by name.
//!
//! Each of these is a thin adapter from the [`CharDevice`] stream contract to
//! something the kernel already has: the line discipline, the console, the
//! entropy pool. They live here rather than in the boot code because they are
//! drivers - and because `mount -t devfs` has to be able to build the same
//! set the boot code used to wire by hand, from wherever the request arrives.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::vfs::VfsResult;
use crate::vfs::devfs::{CharDevice, DevFs};

/// The console as the line discipline has it.
pub struct TtyDev;

impl CharDevice for TtyDev {
    /// The line discipline's own answer, including its `EINTR`: a program
    /// blocked on the console is waiting on the same signal rule as a program
    /// blocked on any other terminal.
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        crate::vfs::tty::global().read(buf)
    }

    fn write(&self, buf: &[u8]) -> usize {
        crate::console::write_bytes(buf);
        buf.len()
    }
}

/// Writes go out to the console. Nothing arrives, so reads are end of input.
pub struct ConsoleDev;

impl CharDevice for ConsoleDev {
    fn read(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> usize {
        crate::console::write_bytes(buf);
        buf.len()
    }
}

/// Writes vanish; reads are end of input.
pub struct NullDev;

impl CharDevice for NullDev {
    fn read(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
}

/// `/dev/kmsg`. Writes land in the kernel log ring (`console::log_bytes`),
/// which is where Linux keeps them too: off the console. An init that files
/// its banner through this device and prints it to stdout would otherwise be
/// on the screen twice - once from its own stdout, once from where it filed
/// it. Reads answer end of file: the ring has no reader yet, and there is
/// nothing to replay.
pub struct KmsgDev;

impl CharDevice for KmsgDev {
    fn read(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> usize {
        crate::console::log_bytes(buf);
        buf.len()
    }
}

/// Reads are zeros forever; every write is refused with ENOSPC.
///
/// This is the device a program is pointed at to make it *fail*: `cp file
/// /dev/full` is how a test proves a program reports a full disk instead of
/// silently losing data. A write that answered "zero bytes written" would be
/// a successful short write, which is the opposite of what is being tested -
/// so the refusal is carried by [`CharDevice::write_error`] rather than by the
/// count, and reaches the caller as ENOSPC.
///
/// Reads give zeros, as `/dev/zero` does: the pair differs only in whether
/// writes are accepted, and a reader asked for bytes gets them either way.
pub struct FullDev;

impl CharDevice for FullDev {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write(&self, _buf: &[u8]) -> usize {
        0
    }

    fn write_error(&self) -> Option<crate::vfs::VfsError> {
        Some(crate::vfs::VfsError::NoSpace)
    }
}

/// Reads are zeros forever, however much is asked; writes vanish.
pub struct ZeroDev;

impl CharDevice for ZeroDev {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
}

/// Reads are random bytes, however much is asked; writes vanish.
///
/// This is the device a TLS stack's entropy comes from, and for busybox it is
/// the *only* one: its HTTPS implementation opens `/dev/urandom` and never
/// calls `getrandom(2)`. A kernel without this node cannot run an HTTPS client
/// at all, which is why it is here beside `/dev/zero` rather than later.
///
/// Reads never block. Linux blocks `/dev/random` until the pool is confident
/// and answers `/dev/urandom` immediately; this kernel has one stream and
/// behaves like the latter for both names, so a program that reaches for
/// either one makes progress.
pub struct RandomDev;

impl CharDevice for RandomDev {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        crate::rand::fill(buf);
        Ok(buf.len())
    }

    fn write(&self, buf: &[u8]) -> usize {
        // Writing to `/dev/urandom` is how a caller *adds* entropy on Linux,
        // and Linux accepts and ignores a write when it has no room for it.
        // There is nowhere to put it here, and accepting it is the compatible
        // answer: refusing would turn a harmless call into a failure.
        buf.len()
    }
}

/// A driver's contribution to the device set userland is handed.
///
/// The set below is the *interface* - `/dev/hwrng` is the entropy pipe,
/// `/dev/fb0` the scanout, whichever bus carried one - but whether a node exists
/// at all, and what sits behind it, is the *driver's* answer. Written centrally,
/// every probed node meant naming its driver module from here
/// (`virtio::rng::global()`, `virtio::gpu::global()`, ...), which is the
/// registration side reaching into the driver side and one file to edit for every
/// new device. Inverted, a driver publishes its own nodes the moment it comes up,
/// and this module names no driver at all: the probe decides, where the probe
/// happens.
///
/// What is deliberately *not* published this way is the core set (the console,
/// the ttys, the entropy pair, the null/zero sinks and the `/proc` links): those
/// are the device interface every machine has, not the result of a probe. The
/// same distinction keeps `rtc`, `drm` and the disks in their own blocks below,
/// each for the reason the comment there gives.
#[derive(Clone, Copy)]
pub struct Publisher {
    /// The driver that owns these nodes. Publishing again under a name already
    /// present replaces the entry, so a driver that comes up twice does not
    /// register its nodes twice.
    owner: &'static str,
    /// Fill `dev` with the owner's nodes. Called once per [`standard_devfs`].
    nodes: fn(&DevFs),
}

/// The publishers drivers registered, in the order the drivers came up.
static PUBLISHERS: Mutex<RefCell<Vec<Publisher>>> = Mutex::new(RefCell::new(Vec::new()));

/// Publish a driver's nodes to every device set built from now on.
///
/// Called by a driver when it has something to publish - after its probe
/// succeeded - and never by the builder. That direction is the whole point:
/// `standard_devfs` drains this list without knowing a single device name.
/// Idempotent per `owner`, because a driver that retries, or comes up on two
/// buses, must not produce two nodes under one name.
pub fn publish(owner: &'static str, nodes: fn(&DevFs)) {
    critical_section::with(|cs| {
        let mut list = PUBLISHERS.borrow(cs).borrow_mut();
        match list.iter_mut().find(|p| p.owner == owner) {
            Some(slot) => slot.nodes = nodes,
            None => list.push(Publisher { owner, nodes }),
        }
    });
}

/// The device set every boot needs, plus the disk and its partitions.
///
/// This is what `mount -t devfs /dev` builds, and the same set the boot code
/// used to wire up by hand: the tty and the console for standard descriptors,
/// the null and zero sinks, and the entropy devices a TLS stack reaches for.
/// The block devices are named after the partition layout the kernel found -
/// `vda` for the whole disk, `vda1`, `vda2`, ... for its partitions in table
/// order - because mounting the real root is the first thing an initramfs has
/// to do, and it can only do that if the disk has a name.
///
/// A caller wanting more - a second disk, a new driver - registers it itself,
/// because naming a device is the driver's decision, not a caller's.
pub fn standard_devfs() -> Arc<DevFs> {
    let dev = DevFs::new();
    // The console names are doors into the console tty: a getty's `TIOCSCTTY`,
    // a shell's `TCGETS` and every `isatty` ask the node itself, and a bare
    // character-device wrapper cannot answer any of them. `console` stays a bare
    // device - writes go out, nothing comes back. `tty` is the odd one: not a
    // device at all but the opening process's controlling terminal, which is what
    // the name has meant since Unix had job control. Every open of it resolves
    // through the caller's own `TIOCSCTTY` binding, and a process that has
    // acquired none is refused with `ENXIO` rather than handed someone else's
    // terminal - which is what keeps a shell's job-control probe honest about
    // whether it is in the foreground.
    //
    // The console tty's name is the *machine's*, and the two differ: the psABI
    // calls the first serial port `ttyS0` on x86_64 and `ttyAMA0` on aarch64 -
    // the same name `boot::console_list` reads out of a `console=` word. It is
    // one node either way (the console tty, where the kernel's log goes and which
    // the kernel's own standard descriptors are), so publishing it under this
    // machine's name is what lets an `/etc/inittab` name the port Linux names:
    // a getty on `ttyAMA0` on a machine whose serial port is a PL011, and on
    // `ttyS0` where it is a 16550. One name, not two - a second spelling for the
    // same node is the private dialect these devices avoid.
    let tty = crate::vfs::tty::global();
    #[cfg(target_arch = "x86_64")]
    dev.register_node("ttyS0", tty.clone());
    #[cfg(target_arch = "aarch64")]
    dev.register_node("ttyAMA0", tty.clone());
    dev.register_node("tty", Arc::new(crate::vfs::tty::ControllingTty));
    // `/dev/tty0` - "the current VT", the name a seat manager opens to ask
    // the console as a whole about its VT state and switch it. It carries
    // Linux's own `(4, 0)` and every open resolves to the console actually on
    // the screen, which is what makes a *foreground* rule expressible: a
    // descriptor for the shared console object has no foreground group of its
    // own to check, and a descriptor for `tty3` does.
    dev.register_node("tty0", Arc::new(crate::vfs::tty::Tty0));
    dev.register("console", Arc::new(ConsoleDev));
    // The virtual consoles: the slave ends of the pty pairs minted at boot.
    // A getty respawns on `tty1`..`tty6` the way a stock inittab asks, and
    // the console multiplexer holds the masters - typed bytes go to the
    // active console's slave, what its shell prints comes back over the one
    // serial wire.
    for index in 1..=crate::term::vc::VC_COUNT {
        dev.register_node(
            &alloc::format!("tty{index}"),
            crate::vfs::pty::vc(index).slave().clone(),
        );
    }
    // The mountpoints an init expects `/dev` to carry: devpts goes on
    // /dev/pts, a tmpfs on /dev/shm, and both names have to be there to be
    // mounted on.
    dev.register_dir("pts");
    dev.register_dir("shm");

    // The kernel's ring buffer, as far as this kernel has one: a sink. The
    // initramfs's init writes its progress there, and a write that fails
    // would turn every line of its output into an error message instead.
    dev.register("kmsg", Arc::new(KmsgDev));
    dev.register("null", Arc::new(NullDev));
    dev.register("full", Arc::new(FullDev));
    // The three names a script writes to when it means "wherever standard
    // output goes", and the directory a program lists when it wants to know
    // what it has open. They are links into /proc rather than devices, so
    // they follow the reader's own descriptors: a shell's `> /dev/stdout`
    // lands on the pipe it was given, not on a second console.
    dev.register_symlink("fd", "/proc/self/fd");
    dev.register_symlink("stdin", "/proc/self/fd/0");
    dev.register_symlink("stdout", "/proc/self/fd/1");
    dev.register_symlink("stderr", "/proc/self/fd/2");
    // The pty master's traditional name. Every open mints a pty pair and
    // hands back the master, so a login's `posix_openpt`/`grantpt`/
    // `unlockpt`/`open("/dev/pts/N")` sequence works by the book.
    dev.register_node("ptmx", Arc::new(crate::vfs::pty::Ptmx));
    dev.register("zero", Arc::new(ZeroDev));
    // The entropy pair carries its real Linux device numbers (mem 1, minors
    // 8 and 9) because an ioctl on them is recognized by that number rather
    // than by a downcast: one stream, two names, and the numbers are what
    // make the pair the pair.
    dev.register_id("random", Arc::new(RandomDev), 1, 8);
    dev.register_id("urandom", Arc::new(RandomDev), 1, 9);

    // Everything a *driver* published, in the order the drivers came up. Each
    // entry is the driver's own registration code, so this loop knows no device
    // by name and a new driver does not edit this file (`publish` above). The
    // *name* is the interface - `/dev/hwrng` is the entropy pipe on either bus -
    // and a driver that never came up is simply not in the list, so a node for a
    // device this machine does not have is never created.
    let publishers =
        critical_section::with(|cs| -> Vec<Publisher> { PUBLISHERS.borrow(cs).borrow().to_vec() });
    for publisher in publishers {
        (publisher.nodes)(&dev);
    }
    // The real-time clock, on its Linux misc-device number (10,135 - the number
    // `hwclock` and the `RTC_*` ioctls are keyed on). `rtc0` is the name the
    // class gives it; `rtc` is the alias every busybox `hwclock` tries when
    // the numbered one is missing, and registering both is cheaper than
    // trusting which list the shipped binary walks.
    //
    // Both machines and not one: what the node reads is `crate::hal`, not a
    // register, so the machine behind it is the CMOS on x86_64 and the PL031
    // here, and the only x86-shaped thing about this was the gate. The node was
    // registered on x86_64 alone, which meant the second machine's `hwclock`
    // service found no `/dev/rtc` on any of the three names it walks, could not
    // set the system clock, and reported `[ !! ]` on every boot (`NOTES.md`
    // §13.15). The `modprobe rtc-cmos`/`rtc`/`genrtc` loop that noise came from
    // is guarded by the same `rtc_exists` test, so the node silences it too.
    #[cfg(target_os = "none")]
    {
        dev.register_id("rtc0", Arc::new(crate::drivers::rtc_dev::RtcDev), 10, 135);
        dev.register_id("rtc", Arc::new(crate::drivers::rtc_dev::RtcDev), 10, 135);
    }
    // The DRM/KMS card. A compositor that drives the display through KMS (not
    // the raw framebuffer) opens `/dev/dri/card0`, makes dumb buffers, and
    // page-flips them onto the scanout. One card, fixed ids, software
    // presentation only.
    //
    // Beside it the render node: GBM opens renderD128 to make its buffers
    // (a GL client is not the display master, so libdrm's convention is to
    // allocate on the render node and import onto the card), and mpos has one
    // card with no permission boundary between the two names - so the node
    // is the same card state under a fresh inode, not a second driver.
    // Both nodes ioctl the same single `DrmState`, and a modesetting ioctl
    // sent to the render node is accepted the way it would be on card0
    // (a teaching kernel's honesty; the split is a naming convention here,
    // not a boundary).
    //
    // Both machines' cards are driven through virtio-GPU - the first over PCI,
    // the second over MMIO - and both publish the one card the render task
    // found into `drivers::drm`, so the pair of nodes is not a machine's
    // property: it is the display's. The gate this block used to carry was
    // `target_arch = "x86_64"`, written when the second machine had no GPU at
    // all; the port brought its card up (and `render::spawn` has published it
    // since) without the node following, which left a machine with a card in
    // the kernel and no `/dev/dri` to reach it by.
    {
        let dri = dev.register_dir("dri");
        dri.insert("card0", crate::drivers::drm::DrmCard::new(0x9100, 0));
        dri.insert("renderD128", crate::drivers::drm::DrmCard::new(0x9200, 128));
    }

    // Every disk the kernel brought up, each under the name its own driver
    // gave it, with its partitions beside it. The disk itself is a window over
    // the whole device and each partition is a window over its own slice, so
    // what userland opens is the same storage either way.
    for disk in crate::blk::disks() {
        dev.register_block(&disk.name, disk.dev.clone());
        for (index, (start, sectors)) in crate::blk::part::partitions(disk.dev.as_ref())
            .into_iter()
            .enumerate()
        {
            dev.register_block(
                &alloc::format!("{}{}", disk.name, index + 1),
                Arc::new(crate::blk::part::Window::new(
                    disk.dev.clone(),
                    start,
                    sectors,
                )),
            );
        }
    }
    dev
}
