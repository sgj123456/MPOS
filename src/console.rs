//! Kernel console — architecture-agnostic formatting over `arch::serial`.
//!
//! This module is a **kernel-wide service**, not a driver or a VFS component.
//! It wraps the raw serial driver with interrupt-safe output and provides the
//! `println!` macro. Callers include the kernel itself (`println!`), the TTY
//! line discipline (echo output), and the syscall `write` handler.

use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use critical_section::Mutex;

pub struct Console;

/// Where the console's bytes land once the GPU framebuffer takes over.
///
/// `Serial` is the boot answer and the permanent headless answer; `Gpu` is
/// flipped once, when the render task owns the scanout. The switch is read from
/// the serial interrupt path, so it is a plain atomic, never a lock - for the
/// same reason `drivers::serial` keeps no lock (see its module doc).
const BACKEND_SERIAL: u8 = 0;
const BACKEND_GPU: u8 = 1;
static BACKEND: AtomicU8 = AtomicU8::new(BACKEND_SERIAL);

/// Mirror kernel-console bytes onto the serial wire as well as drawing them.
///
/// On while there is no GPU (and on a panic, so the report reaches the host);
/// on in headless mode, which is how `cargo xtask test` keeps reading its log;
/// off for an interactive GPU boot, because a mirror would send the `bat` flood
/// back down the one wire and undo the whole feature.
static MIRROR_SERIAL: AtomicBool = AtomicBool::new(true);

/// `mpos.headless` on the command line. A headless QEMU still has a virtio-gpu
/// (it answers with a fallback mode), so head-lessness is a command-line fact,
/// not a device-presence one; when it is set the GPU is never engaged at all.
static HEADLESS: AtomicBool = AtomicBool::new(false);

/// Record the `mpos.headless` boot flag. Called once, from the kernel entry.
pub fn set_headless(on: bool) {
    HEADLESS.store(on, Ordering::Relaxed);
}

/// Whether the boot mode *asks* for the serial mirror at all, before the console
/// reaches the framebuffer. Set from the command line (`mpos.nomirror` clears
/// it) and read once by [`takeover`] to decide `MIRROR_SERIAL`. Default on, so a
/// boot that says nothing keeps mirroring the console to the wire as it did
/// before the gui/terminal run modes existed.
static MIRROR_WANTED: AtomicBool = AtomicBool::new(true);

/// Record whether this boot wants the console mirrored to the serial wire once
/// the framebuffer takes over. Called once, from the kernel entry; off only for a
/// `mpos.nomirror` (gui-mode) boot.
pub fn set_mirror_wanted(on: bool) {
    MIRROR_WANTED.store(on, Ordering::Relaxed);
}

/// Whether the boot mode asked for the serial mirror. Read by [`takeover`].
pub fn mirror_wanted() -> bool {
    MIRROR_WANTED.load(Ordering::Relaxed)
}

/// Whether this machine was booted with no display (selftest, headless CI).
pub fn headless() -> bool {
    HEADLESS.load(Ordering::Relaxed)
}

/// Whether the framebuffer is drawing the console.
pub fn gpu_mode() -> bool {
    BACKEND.load(Ordering::Relaxed) == BACKEND_GPU
}

/// Hand the console to the GPU. Ignored while headless, which is the point of
/// the headless flag: no scanout, no DMA, the wire stays the whole display.
pub fn set_gpu() {
    if !headless() {
        BACKEND.store(BACKEND_GPU, Ordering::Relaxed);
    }
}

/// Pull the console back to the serial wire and mirror onto it. The panic path
/// uses this so a report is always on the host's serial, GPU or not.
pub fn force_serial() {
    BACKEND.store(BACKEND_SERIAL, Ordering::Relaxed);
    MIRROR_SERIAL.store(true, Ordering::Relaxed);
}

/// Turn the serial mirror on or off, as the boot mode and takeover require.
pub fn set_mirror_serial(on: bool) {
    MIRROR_SERIAL.store(on, Ordering::Relaxed);
}

/// Whether kernel-console bytes are also going out the serial wire.
pub fn mirror_serial() -> bool {
    MIRROR_SERIAL.load(Ordering::Relaxed)
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::drivers::serial::write_bytes(s.as_bytes());
        Ok(())
    }
}

/// The line being formatted, gathered before any of it reaches the wire.
///
/// Only used once the heap exists and the console queues into the transmit
/// ring - see [`print_args`]. Gathering is what lets a line be handed over as
/// one submission instead of being streamed under a held critical section.
struct Sink(Vec<u8>);

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

pub fn init() {
    crate::drivers::serial::init();
}

/// The kernel log ring: what `/dev/kmsg` writers file. The serial console
/// carries the kernel's own voice; this carries userland's filed notes,
/// which Linux keeps *off* the console - an init that echoes its banner
/// into `/dev/kmsg` and prints it to stdout would otherwise be on the
/// screen twice, and every boot after this one has that init.
static DMESG: Mutex<RefCell<alloc::collections::VecDeque<u8>>> =
    Mutex::new(RefCell::new(alloc::collections::VecDeque::new()));
const DMESG_CAP: usize = 16 * 1024;

/// File bytes into the kernel log ring, oldest entries dropped first.
pub fn log_bytes(bytes: &[u8]) {
    critical_section::with(|cs| {
        let mut log = DMESG.borrow(cs).borrow_mut();
        for &b in bytes {
            if log.len() == DMESG_CAP {
                log.pop_front();
            }
            log.push_back(b);
        }
    });
}

/// Snapshot the kernel log ring, as a `dmesg` reader would see it.
pub fn dmesg_snapshot() -> alloc::vec::Vec<u8> {
    critical_section::with(|cs| DMESG.borrow(cs).borrow().iter().copied().collect())
}

/// Take up to `len` bytes off the front of the kernel log ring - the
/// destructive read `syslog(2)`'s `SYSLOG_ACTION_READ` serves, and the one
/// a `klogd` parks in. What is taken is gone: reading the log is news, and
/// news is news once.
pub fn dmesg_take(len: usize) -> alloc::vec::Vec<u8> {
    critical_section::with(|cs| {
        let mut log = DMESG.borrow(cs).borrow_mut();
        let n = log.len().min(len);
        (0..n)
            .map(|_| log.pop_front().expect("length checked"))
            .collect()
    })
}

/// Empty the kernel log ring - `syslog(2)`'s `SYSLOG_ACTION_CLEAR`, and the
/// tail of its read-and-clear. The bytes are dropped rather than archived:
/// a `dmesg` after a clear answers as nothing having been said.
pub fn dmesg_clear() {
    critical_section::with(|cs| DMESG.borrow(cs).borrow_mut().clear());
}

/// How many bytes the kernel log ring holds - `syslog(2)`'s
/// `SYSLOG_ACTION_SIZE_BUFFER` answer, and the number a `dmesg` sizes its
/// own buffer by before it asks for the log.
pub fn dmesg_capacity() -> usize {
    DMESG_CAP
}

pub fn write_bytes(bytes: &[u8]) {
    // Keep a copy of what the console has said, so the framebuffer can be
    // brought up to a screen that already shows the boot log rather than born
    // mid-story. Fixed-size, never allocates, safe from the interrupt path.
    record(bytes);
    if gpu_mode() {
        // Kernel console text lands on the virtual console currently on screen,
        // exactly as Linux prints onto the foreground VT. Once its grid exists
        // (after the render task made it) this paints; before, the ring above
        // still catches it for the replay.
        crate::vfs::term::with_screen(active_grid(), |g| g.feed(bytes));
        crate::drivers::video::render::notify();
        if mirror_serial() {
            serial_mirror_now(bytes);
        }
        return;
    }
    serial_bytes(bytes);
}

/// A virtual console's own printed bytes - what the shell on `/dev/ttyN` wrote.
///
/// Distinct from [`write_bytes`] (the kernel console): these belong to one
/// screen, whichever produced them, so a flood on a background console updates
/// that console's grid and is simply not on screen until you switch to it. On
/// the framebuffer they paint that grid; and when the serial mirror is on they
/// also go out the wire, so the same console shows in the terminal window as
/// well. On a headless boot (no framebuffer) the wire is the only sink.
pub fn vc_output(vc: u32, bytes: &[u8]) {
    if gpu_mode() {
        crate::vfs::term::with_screen(vc as usize, |g| g.feed(bytes));
        crate::drivers::video::render::notify();
        if mirror_serial() {
            serial_mirror_now(bytes);
        }
    } else {
        serial_bytes(bytes);
    }
}

/// The grid index of the console currently shown, by its `/dev/ttyN` number.
fn active_grid() -> usize {
    crate::vfs::vcmux::active() as usize
}

/// How far back the replayed boot log reaches. A screen is 25 rows; a few
/// kilobytes covers the banner, the memory map and the milestone lines.
const LOG_CAP: usize = 8 * 1024;
static LOG: Mutex<RefCell<alloc::collections::VecDeque<u8>>> =
    Mutex::new(RefCell::new(alloc::collections::VecDeque::new()));

/// File a byte-run into the replay ring, oldest bytes dropped first.
fn record(bytes: &[u8]) {
    critical_section::with(|cs| {
        let mut log = LOG.borrow(cs).borrow_mut();
        for &b in bytes {
            if log.len() == LOG_CAP {
                log.pop_front();
            }
            log.push_back(b);
        }
    });
}

/// Hand the console to the framebuffer: replay the boot log onto the freshly
/// made screen, then switch. Called by the render task once its screen exists,
/// so there is no window between "GPU is the backend" and "there is a screen to
/// paint." Whether the serial wire *also* mirrors the console is the boot mode's
/// choice ([`mirror_wanted`]): mirror mode does, gui mode stays quiet. When it
/// mirrors it does so best-effort - [`serial_mirror_now`] hands the bytes to the
/// transmit ring and returns, never parking the caller - so a host terminal that
/// cannot keep up drops serial bytes instead of stalling the framebuffer and the
/// console mux behind it.
pub fn takeover() {
    let snapshot: Vec<u8> = critical_section::with(|cs| {
        let mut out = Vec::with_capacity(LOG.borrow(cs).borrow().len());
        out.extend(LOG.borrow(cs).borrow().iter().copied());
        out
    });
    crate::vfs::term::with_screen(active_grid(), |g| {
        g.reset_to_boot(&snapshot);
    });
    set_gpu();
    set_mirror_serial(mirror_wanted());
}

/// Mirror console bytes onto the serial wire without ever blocking the producer.
///
/// This is the framebuffer console's serial mirror - the copy of the screen's
/// text that also goes out the wire so a plain terminal can watch the same
/// console. Unlike [`serial_bytes`] (the *authoritative* write, which parks
/// until the hardware takes everything so nothing is lost), the mirror is
/// best-effort: the bytes are handed to the transmit ring and we return, and
/// whatever the ring has no room for is dropped. That is the whole point - a
/// slow host terminal must never throttle the framebuffer, and the caller here
/// is often the console mux or a `println!` whose progress the machine cannot
/// afford to stall on the wire's speed.
///
/// Before the transmitter interrupts - early boot, or an interrupt context that
/// cannot reach the ring - there is nothing to hand over to, so this falls back
/// to the bounded polled [`serial_bytes`], which never parks either.
fn serial_mirror_now(bytes: &[u8]) {
    if crate::drivers::serial::interrupt_driven() {
        // `tx_submit` copies what fits and returns that count; the tail is
        // dropped by design - the TX interrupt drains the rest of the ring at
        // wire speed in the background.
        let _ = crate::drivers::serial::tx_submit(bytes);
        return;
    }
    serial_bytes(bytes);
}

/// Write raw bytes on the serial console - the wire, through the transmit ring.
///
/// This is what [`write_bytes`] dispatches to when the console is on the wire
/// (and, for now, even when it is meant for the framebuffer). Everything below
/// is the ring-or-polled choice the boot depends on.
///
/// Once the transmitter interrupts - [`crate::drivers::serial::enable_tx`],
/// where the IDT and the PIC exist - the bytes are queued and a full ring
/// *waits*: the task sleeps until the hardware takes enough for the rest.
///
/// Before that it is the polled write, in chunks with the clock live between
/// them. The 16550 spins on the transmitter-empty bit per byte, so a few
/// kilobytes of output written inside one critical section blanked interrupts
/// long enough to starve the receiver and lose ticks - which is not merely
/// slow, it is the machine looking dead: no clock means no scheduling, and no
/// serial interrupt means the keyboard is not even read. The bug that shape
/// took was a `bat` over a minified stylesheet, whose colour sequences reach
/// the console in tens of thousands of writes.
///
/// The polled form is still what a caller that cannot park gets: an interrupt
/// handler, or the panic path on the way out. It is bounded by the chunk, not
/// by the wait, so a host that stops draining still costs only one line's
/// worth of masked interrupts rather than the machine.
fn serial_bytes(bytes: &[u8]) {
    if !crate::drivers::serial::interrupt_driven() || !may_park() {
        const CHUNK: usize = 64;
        for piece in bytes.chunks(CHUNK) {
            critical_section::with(|_| crate::drivers::serial::write_bytes(piece));
        }
        return;
    }
    let mut at = 0;
    while at < bytes.len() {
        at += crate::drivers::serial::tx_submit(&bytes[at..]);
        if at == bytes.len() {
            break;
        }
        // Re-armed before *every* park, not once above. `pump_tx` hands the
        // room out by taking the WHOLE wait list and waking it, so a wake also
        // removes this task from the list. A write larger than one drain burst
        // - a `bat` flood over the ring - therefore parks again, and unless it
        // rejoined, that second sleep would be announced to nobody and the
        // console would hang mid-write. Being on the list before the check
        // still closes the first gap: room that appears between the failed
        // submit and the sleep is seen, because `block_until` re-checks under
        // the same critical section that would take us off it.
        crate::drivers::serial::tx_wait_now();
        crate::sched::block_until(|| crate::drivers::serial::tx_space() > 0);
    }
    crate::drivers::serial::tx_stop_waiting();
}

/// Whether this context may sleep: the scheduler is running, and interrupts
/// are enabled to let the wake through. An interrupt handler fails the second
/// test, and the boot code before `sched::init` fails the first.
fn may_park() -> bool {
    crate::sched::started() && crate::arch::interrupts_enabled()
}

#[doc(hidden)]
pub fn print_args(args: fmt::Arguments<'_>) {
    // A whole line at a time. Tasks preempt each other anywhere, including
    // halfway through a format argument, and two of them writing to one UART
    // produce output neither meant.
    //
    // Before the console queues into the ring - which is armed at the end of
    // the handoff, after the heap - there is nothing to park on and, more to
    // the point, nothing to allocate *from*: the first lines this prints are
    // the memory map and the frame allocator's own tally, and a `Vec` here
    // would ask for a heap that does not exist yet. So that half of the boot
    // keeps the old shape, streaming the line under the critical section.
    if !crate::drivers::serial::interrupt_driven() {
        critical_section::with(|_| {
            let _ = Console.write_fmt(args);
        });
        return;
    }
    // Gathered, then handed over once: the line stays contiguous without the
    // section spanning a wait for the hardware, which is what used to blank
    // the clock for as long as the host took to drain the line.
    let mut sink = Sink(Vec::new());
    let _ = sink.write_fmt(args);
    write_bytes(&sink.0);
}
