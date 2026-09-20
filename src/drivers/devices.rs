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
    fn read(&self, buf: &mut [u8]) -> usize {
        buf.fill(0);
        buf.len()
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
    dev.register("console", Arc::new(ConsoleDev));
    // The virtual consoles: the slave ends of the pty pairs minted at boot.
    // A getty respawns on `tty1`..`tty6` the way a stock inittab asks, and
    // the console multiplexer holds the masters - typed bytes go to the
    // active console's slave, what its shell prints comes back over the one
    // serial wire.
    for index in 1..=crate::vfs::pty::VC_COUNT {
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
