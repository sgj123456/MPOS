//! Kernel console — architecture-agnostic formatting over `drivers::serial`.
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

use crate::boot::console_list::ConsoleList;

pub struct Console;

/// Where the console's bytes land once the GPU framebuffer takes over.
///
/// `Serial` is the boot answer and the answer on a boot whose console list names
/// the wire alone; `Gpu` is flipped once, when the render task owns the scanout
/// and the framebuffer console is one of the consoles. The switch is read from
/// the serial interrupt path, so it is a plain atomic, never a lock - for the
/// same reason `drivers::serial` keeps no lock (see its module doc).
const BACKEND_SERIAL: u8 = 0;
const BACKEND_GPU: u8 = 1;
static BACKEND: AtomicU8 = AtomicU8::new(BACKEND_SERIAL);

/// The consoles this boot writes its log to, from `console=` on the command
/// line (see [`crate::boot::console_list`]).
///
/// Three booleans rather than one value because these are the three questions
/// the write path actually asks, and that path is reached from the serial
/// interrupt, where an atomic load is the only safe read - the same reason
/// `drivers::serial` keeps no lock. They begin as the wire alone, which is where
/// a boot's first lines go; [`set_console_list`] replaces them once the command
/// line has been read.
static SERIAL_SINK: AtomicBool = AtomicBool::new(true);
static VT_SINK: AtomicBool = AtomicBool::new(false);
static SERIAL_PREFERRED: AtomicBool = AtomicBool::new(true);

/// Take the console list the command line named. Called once, from the entry.
///
/// What is *not* here is as load-bearing as what is: there is no "mirror" flag
/// and no "compositor" flag, because both are consequences of this list. A wire
/// that is one of the consoles is written; a wire that is the preferred console
/// is written *losslessly*; and a framebuffer console that is not one of the
/// consoles is simply never taken over. That is what lets a boot describe itself
/// with the parameter Linux uses instead of a word for how it was launched.
pub fn set_console_list(list: ConsoleList) {
    SERIAL_SINK.store(list.has_serial(), Ordering::Relaxed);
    VT_SINK.store(list.has_vt(), Ordering::Relaxed);
    SERIAL_PREFERRED.store(list.prefers_serial(), Ordering::Relaxed);
}

/// Whether the serial wire is one of this boot's consoles.
pub fn serial_is_sink() -> bool {
    SERIAL_SINK.load(Ordering::Relaxed)
}

/// Whether the framebuffer's virtual console is one of this boot's consoles.
pub fn vt_is_sink() -> bool {
    VT_SINK.load(Ordering::Relaxed)
}

/// Whether the console's bytes are being *drawn* - that one screen exists and
/// the render task can paint it - as opposed to whether the framebuffer console
/// is the console the kernel is writing to, which is what [`gpu_mode`] answers.
///
/// The two are the same question on every boot but a compositor's, and that is
/// the whole reason for splitting them: on a `gui` boot the kernel log stays on
/// the serial wire (the scanout belongs to the compositor), and yet the virtual
/// consoles still have to be *somewhere*, because a text VT the compositor has
/// released is a VT somebody has to be able to read. Feeding a grid that does
/// not exist yet is a no-op the terminal layer already handles, so a compositor
/// boot turns this on with its other grids at the moment they are made and the
/// console keeps its wire until then and afterwards.
static GRIDS_LIVE: AtomicBool = AtomicBool::new(false);

pub fn grids_live() -> bool {
    gpu_mode() || GRIDS_LIVE.load(Ordering::Relaxed)
}

/// Flip [`grids_live`] on. Called by the render task as it makes the grids, and
/// only for a boot whose virtual console is *not* one of the consoles - on any
/// other boot [`takeover`] reaches the same state by way of [`set_gpu`].
pub fn set_grids_live() {
    GRIDS_LIVE.store(true, Ordering::Relaxed);
}

/// Whether a text console is live anywhere: the framebuffer console is one of
/// this boot's consoles, or the grids exist because the screen can be handed to
/// one (a compositor boot).
///
/// This is what "the console somebody is looking at" means to a subsystem that
/// has to decide whether an event belongs to a shell - the keyboard is the one
/// caller today - and it is deliberately not `vt_is_sink()`: on a compositor
/// boot keystrokes still belong to a text console, because the screen can be
/// switched to one.
pub fn vt_live() -> bool {
    vt_is_sink() || grids_live()
}

/// Whether the framebuffer is drawing the console.
pub fn gpu_mode() -> bool {
    BACKEND.load(Ordering::Relaxed) == BACKEND_GPU
}

/// Hand the console to the GPU - the framebuffer console becomes the console the
/// kernel writes to. Ignored when it is not one of this boot's consoles, which is
/// the old headless veto stated in the terms the command line actually uses: a
/// console the boot never asked for is never taken.
pub fn set_gpu() {
    if vt_is_sink() {
        BACKEND.store(BACKEND_GPU, Ordering::Relaxed);
    }
}

/// Pull the console back to the serial wire. The panic path uses this so a report
/// always reaches the host, GPU or not - and it makes the wire one of the
/// consoles even on a boot whose command line did not name it, because a report
/// that reached nobody is not a report.
pub fn force_serial() {
    BACKEND.store(BACKEND_SERIAL, Ordering::Relaxed);
    SERIAL_SINK.store(true, Ordering::Relaxed);
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

/// Hand console bytes to the serial wire, as one of this boot's consoles.
///
/// A sink that is not the preferred one is written best-effort: the bytes go
/// into the transmit ring and the caller returns, so a host terminal that cannot
/// keep up drops them instead of throttling the console somebody is reading. The
/// preferred console writes through the parking path and loses nothing.
///
/// Which of the two the wire is follows the command line, and that is what
/// replaced the old mirror/nomirror flag pair: `console=ttyS0` alone makes the
/// wire preferred, so every line is lossless; in a `console=ttyS0 console=tty0`
/// boot the wire stays authoritative until the framebuffer console has actually
/// taken over, which is what keeps a boot's early lines - the ones printed
/// before any screen exists - lossless on the wire.
fn wire_bytes(bytes: &[u8]) {
    if !serial_is_sink() {
        return;
    }
    if gpu_mode() && !SERIAL_PREFERRED.load(Ordering::Relaxed) {
        serial_mirror_now(bytes);
    } else {
        serial_bytes(bytes);
    }
}

pub fn write_bytes(bytes: &[u8]) {
    // Keep a copy of what the console has said, so the framebuffer can be
    // brought up to a screen that already shows the boot log rather than born
    // mid-story. Fixed-size, never allocates, safe from the interrupt path.
    record(bytes);
    if grids_live() {
        // Kernel console text lands on the virtual console currently on screen,
        // exactly as Linux prints onto the foreground VT. Once its grid exists
        // (after the render task made it) this paints; before, the ring above
        // still catches it for the replay.
        crate::term::with_screen(active_grid(), |g| g.feed(bytes));
        crate::drivers::video::render::notify();
    }
    wire_bytes(bytes);
}

/// A virtual console's own printed bytes - what the shell on `/dev/ttyN` wrote.
///
/// Distinct from [`write_bytes`] (the kernel console): these belong to one
/// screen, whichever produced them, so a flood on a background console updates
/// that console's grid and is simply not on screen until you switch to it. On
/// the framebuffer they paint that grid; and when the serial wire is one of the
/// consoles they also go out the wire, so the same console shows in the terminal
/// window as well. On a boot whose only console is the wire, the wire is the
/// only sink.
pub fn vc_output(vc: u32, bytes: &[u8]) {
    if !grids_live() {
        wire_bytes(bytes);
        return;
    }
    crate::term::with_screen(vc as usize, |g| g.feed(bytes));
    crate::drivers::video::render::notify();
    wire_bytes(bytes);
}

/// The grid index of the console currently shown, by its `/dev/ttyN` number.
fn active_grid() -> usize {
    crate::term::vc::active() as usize
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
/// so there is no window between "GPU is the console" and "there is a screen to
/// paint." Whether the serial wire *also* carries the console is not decided
/// here - it was never a property of this switch, only of the console list: a
/// wire the boot named is written by [`wire_bytes`], best-effort once the
/// framebuffer has taken over, so a host terminal that cannot keep up drops
/// bytes instead of stalling the framebuffer and the console mux behind it.
pub fn takeover() {
    let snapshot: Vec<u8> = critical_section::with(|cs| {
        let mut out = Vec::with_capacity(LOG.borrow(cs).borrow().len());
        out.extend(LOG.borrow(cs).borrow().iter().copied());
        out
    });
    crate::term::with_screen(active_grid(), |g| {
        g.reset_to_boot(&snapshot);
    });
    set_gpu();
}

/// Mirror console bytes onto the serial wire without ever blocking the producer.
///
/// This is the peer-sink write: the copy that follows the console out the wire
/// when the wire is one of the consoles but not the preferred one. Unlike
/// [`serial_bytes`] (the preferred console's write, which parks until the
/// hardware takes everything so nothing is lost), the mirror is best-effort: the
/// bytes are handed to the transmit ring and we return, and whatever the ring
/// has no room for is dropped. That is the whole point - a slow host terminal
/// must never throttle the console, and the caller here is often the console mux
/// or a `println!` whose progress the machine cannot afford to stall on the
/// wire's speed.
///
/// Before the transmitter interrupts - early boot, or an interrupt context that
/// cannot reach the ring - there is nothing to hand over to, so this falls back
/// to the bounded polled [`serial_bytes`], which never parks either.
///
/// What is dropped when the ring is full is decided by
/// [`crate::term::wire::cut`]: the tail of a write may go, but never a
/// sequence's *middle*. Dropped bytes were always the deal here; a cut that
/// lands inside an escape sequence is not a lost repaint but a terminal left
/// reading one, which swallows the screenful after it and drops the state
/// changes in between - `ESC [ ? 1049h` and `ESC [ ? 25l` among them, so the
/// host is left on an alternate screen with no cursor after the program that
/// asked for both has exited. A cut between sequences loses content and nothing
/// else: the emulator stays in step with the bytes it did get.
fn serial_mirror_now(bytes: &[u8]) {
    if crate::drivers::serial::interrupt_driven() {
        // `tx_submit` copies what fits and returns that count, so the room is
        // asked for first and the copy is of a prefix that fits it exactly.
        let room = crate::drivers::serial::tx_space();
        let take = if bytes.len() <= room {
            bytes.len()
        } else {
            crate::term::wire::cut(bytes, room)
        };
        // Zero is a sequence that does not fit at all: sending its head would
        // be the one thing this must never do, so the chunk goes unsent.
        if take > 0 {
            let _ = crate::drivers::serial::tx_submit(&bytes[..take]);
        }
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
    crate::sched::started() && crate::hal::interrupts_enabled()
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
