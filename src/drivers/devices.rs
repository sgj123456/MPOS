//! The devices userland is handed by name.
//!
//! Each of these is a thin adapter from the [`CharDevice`] stream contract to
//! something the kernel already has: the line discipline, the console, the
//! entropy pool. They live here rather than in the boot code because they are
//! drivers - and because `mount -t devfs` has to be able to build the same
//! set the boot code used to wire by hand, from wherever the request arrives.

use alloc::sync::Arc;

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

/// `/dev/hwrng`: the virtio entropy pipe, as a stream of raw bytes the host
/// generated. Reads ask the device and answer what came back - a refusal
/// reads as end of input, which is the honest shape for a machine whose
/// entropy source went quiet rather than a blocking wait for it. Writes
/// vanish, as they do on Linux.
pub struct HwrngDev(Arc<crate::drivers::virtio::rng::VirtioRng>);

impl CharDevice for HwrngDev {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        // The count is the device's own answer: how many bytes the host
        // actually generated, which for a healthy device is the whole
        // buffer and for a quiet one is a short read - the shape every
        // stream answers with, and the one a caller retrying a short read
        // expects.
        Ok(self.0.fill(buf))
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
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
    // character-device wrapper cannot answer any of them. `ttyS0` is the
    // serial port's name - the kernel log's wire, and the kernel's own
    // standard descriptors. `console` stays a bare device - writes go out,
    // nothing comes back. `tty` is the odd one: not a device at all but the
    // opening process's controlling terminal, which is what the name has
    // meant since Unix had job control. Every open of it resolves through the
    // caller's own `TIOCSCTTY` binding, and a process that has acquired none
    // is refused with `ENXIO` rather than handed someone else's terminal -
    // which is what keeps a shell's job-control probe honest about whether it
    // is in the foreground.
    let tty = crate::vfs::tty::global();
    dev.register_node("ttyS0", tty.clone());
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

    // The virtio devices the boot probed, each under the name Linux gives
    // its kind - registered only when the device answered, because a node
    // for a machine this kernel does not have is a lie an open would trip
    // over. The adapters wrap the driver's own handle: the device behind
    // /dev/hwrng is the entropy pipe, the one behind /dev/hvc0 is the
    // console port, /dev/dsp is the sound card's stream.
    if let Some(rng) = crate::drivers::virtio::rng::global() {
        dev.register("hwrng", Arc::new(HwrngDev(rng)));
    }
    // The CMOS clock, on its Linux misc-device number (10,135 - the number
    // `hwclock` and the `RTC_*` ioctls are keyed on). `rtc0` is the name the
    // class gives it; `rtc` is the alias every busybox `hwclock` tries when
    // the numbered one is missing, and registering both is cheaper than
    // trusting which list the shipped binary walks.
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    {
        dev.register_id("rtc0", Arc::new(crate::drivers::rtc_dev::RtcDev), 10, 135);
        dev.register_id("rtc", Arc::new(crate::drivers::rtc_dev::RtcDev), 10, 135);
    }
    if let Some(hvc) = crate::drivers::virtio::console::global() {
        let device = Arc::new(crate::drivers::virtio::console::HvcDev::new(hvc));
        // The name of a hypervisor console is `hvc0`; `hvc` is the alias
        // Linux keeps for "the first one" - a getty that asks for either
        // spelling finds the same port.
        dev.register("hvc0", device.clone());
        dev.register("hvc", device);
    }
    if let Some(snd) = crate::drivers::virtio::sound::global() {
        dev.register(
            "dsp",
            Arc::new(crate::drivers::virtio::sound::DspDev::new(snd)),
        );
    }
    // The framebuffer is a node with more than a stream face - `mmap` of
    // the pixels, the `FBIOGET_*` ioctls - so it registers as the inode
    // it is, through the door the tty uses for the same reason.
    if let Some(gpu) = crate::drivers::virtio::gpu::global() {
        dev.register_node(
            "fb0",
            Arc::new(crate::drivers::virtio::gpu::FbDev::new(gpu)),
        );
    }
    // The input devices, one node each under /dev/input, in bus order:
    // event0, event1, ... exactly how evdev names them. udev and libinput
    // find a device by the pair of its sysfs `dev` number and this node, so
    // each is also announced with `SUBSYSTEM=input` and DEVNAME pointing at
    // the /dev/input path it actually lives under.
    // `/dev/input` exists whether or not the bus answered with a device: it is
    // where `uinput` puts the devices userland makes, and a directory that only
    // appears when the machine happens to have a keyboard is a directory a
    // program cannot depend on.
    let input_dir = dev.register_dir("input");
    let evdevs = crate::drivers::virtio::input::nodes();
    {
        for (index, node) in evdevs.iter().enumerate() {
            let name = alloc::format!("event{index}");
            let major = crate::drivers::virtio::input::INPUT_MAJOR;
            let minor = crate::drivers::virtio::input::input_minor(index);
            input_dir.insert(&name, node.clone());
            crate::uevent::publish(&crate::uevent::Event {
                action: "add",
                // The event node's own path, not the input device it reports
                // through. Linux's uevent for `/dev/input/event0` is
                // `DEVPATH=.../input0/event0` beside `DEVNAME=input/event0`,
                // and udev keys its database entry by the syspath the event
                // names - so announcing the parent with the child's devnum
                // stores an entry that neither `udevadm info` on the event node
                // nor libinput can find. The device is then never tagged
                // `ID_INPUT`, libinput's seat enumeration passes every one of
                // them by, and weston comes up with a screen and "no input
                // devices found".
                devpath: alloc::format!("/devices/virtual/input/input{index}/{name}"),
                subsystem: "input",
                devname: alloc::format!("input/{name}"),
                major,
                minor,
                extra: &[],
            });
        }
    }

    // `/dev/uinput`: the interface a program makes an input device of its own
    // through. It hands the created devices' nodes into the same `/dev/input`,
    // and takes the next number after the ones the bus answered with, so a
    // synthetic device is `event2` on a machine with a keyboard and a tablet.
    dev.register_node(
        "uinput",
        crate::drivers::uinput::Uinput::new(input_dir, evdevs.len()),
    );
    crate::uevent::publish(&crate::uevent::Event {
        action: "add",
        devpath: alloc::string::String::from("/devices/virtual/misc/uinput"),
        subsystem: "misc",
        devname: alloc::string::String::from("uinput"),
        major: crate::uinput::MAJOR,
        minor: crate::uinput::MINOR,
        extra: &[],
    });

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
