//! The devices userland is handed by name.
//!
//! Each of these is a thin adapter from the [`CharDevice`] stream contract to
//! something the kernel already has: the line discipline, the console, the
//! entropy pool. They live here rather than in the boot code because they are
//! drivers - and because `mount -t devfs` has to be able to build the same
//! set the boot code used to wire by hand, from wherever the request arrives.

use alloc::sync::Arc;

use crate::vfs::devfs::{CharDevice, DevFs};

/// The console as the line discipline has it.
pub struct TtyDev;

impl CharDevice for TtyDev {
    fn read(&self, buf: &mut [u8]) -> usize {
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
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn write(&self, buf: &[u8]) -> usize {
        crate::console::write_bytes(buf);
        buf.len()
    }
}

/// Writes vanish; reads are end of input.
pub struct NullDev;

impl CharDevice for NullDev {
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
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
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn write(&self, buf: &[u8]) -> usize {
        crate::console::log_bytes(buf);
        buf.len()
    }
}

/// Reads are zeros forever, however much is asked; writes vanish.
pub struct ZeroDev;

impl CharDevice for ZeroDev {
    fn read(&self, buf: &mut [u8]) -> usize {
        buf.fill(0);
        buf.len()
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
    fn read(&self, buf: &mut [u8]) -> usize {
        crate::rand::fill(buf);
        buf.len()
    }

    fn write(&self, buf: &[u8]) -> usize {
        // Writing to `/dev/urandom` is how a caller *adds* entropy on Linux,
        // and Linux accepts and ignores a write when it has no room for it.
        // There is nowhere to put it here, and accepting it is the compatible
        // answer: refusing would turn a harmless call into a failure.
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
    // The tty names are all doors into the one line discipline, registered as
    // the line discipline's own inode: a getty's `TIOCSCTTY`, a shell's
    // `TCGETS` and every `isatty` ask the node itself, and a bare
    // character-device wrapper cannot answer any of them. `console` stays a
    // bare device - writes go out, nothing comes back. `ttyS0` is the serial
    // port's traditional name spelled as a second path to the same line, and
    // tty1-6 are the virtual consoles a stock /etc/inittab respawns gettys
    // on - without those names, busybox init's respawn entries fail six times
    // and back off for five minutes, and the serial port sits with no login
    // prompt on it at all.
    let tty = crate::vfs::tty::global();
    dev.register_node("tty", tty.clone());
    dev.register_node("ttyS0", tty.clone());
    for index in 1..=6 {
        dev.register_node(&alloc::format!("tty{index}"), tty.clone());
    }
    dev.register("console", Arc::new(ConsoleDev));
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
    // The pty master's traditional name. A real one hands out slave pairs
    // on /dev/pts; this kernel's devpts has no master to offer yet, but the
    // name exists as a character device so an init's `[ -c /dev/ptmx ]`
    // stops falling through to mknod.
    dev.register("ptmx", Arc::new(NullDev));
    dev.register("zero", Arc::new(ZeroDev));
    dev.register("random", Arc::new(RandomDev));
    dev.register("urandom", Arc::new(RandomDev));

    if let Some(raw) = crate::blk::raw_disk() {
        dev.register_block("vda", raw.clone());
        for (index, (start, sectors)) in crate::blk::part::partitions(raw.as_ref())
            .into_iter()
            .enumerate()
        {
            dev.register_block(
                &alloc::format!("vda{}", index + 1),
                Arc::new(crate::blk::part::Window::new(raw.clone(), start, sectors)),
            );
        }
    }
    dev
}
