//! The console multiplexer: one serial wire, six terminals.
//!
//! The machine has one wire to the world - COM1, `ttyS0`, where the kernel's
//! `tracing` log goes. The shells do not run there; they run on the virtual
//! consoles `/dev/tty1`..`tty6`, the slave ends of pty pairs. Something has to
//! stand in between, and this is it: it carries what the active console's
//! shell prints out to the serial wire, and it carries what is typed on the
//! serial wire in to that shell. Six terminals, one wire, in turn.
//!
//! # Switching
//!
//! `^A` followed by a digit `1`..`6` makes that console the active one, the
//! convention GNU screen taught every terminal veteran. `^A` typed twice
//! arrives as one `^A` at the shell, because a shell that cannot receive the
//! byte its key sent is not usable; any other byte after a lone `^A` sends
//! the `^A` on first and the byte after it, so `^A`-then-command is never
//! eaten.
//!
//! # Why the mux is a task
//!
//! Output has to be pumped: the slave prints, the bytes pile up on the master,
//! and only a task can carry them out - an interrupt handler cannot block, and
//! there is nothing else running at handoff time. The task parks until a
//! master has something, and the masters wake it the same way they wake any
//! reader, so an idle console costs nothing.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::sched::policy::TaskId;

/// The virtual console currently on the wire, 1-based.
static ACTIVE: AtomicU32 = AtomicU32::new(1);

/// A `^A` has been seen and the next byte decides what it meant.
static PENDING_PREFIX: AtomicBool = AtomicBool::new(false);

/// The mux task's id, so an input-time switch can wake it - the task parks on
/// the old console's master, and the switch changes what it should park on.
static MUX_TASK: AtomicU32 = AtomicU32::new(0);

/// Whether the mux is up. Until it is, COM1's input belongs to the console
/// tty - the boot checks type at it directly, and the handoff happens long
/// before any shell exists.
pub fn ready() -> bool {
    MUX_TASK.load(Ordering::Relaxed) != 0
}

/// The virtual console currently on the wire, 1-based.
pub fn active() -> u32 {
    ACTIVE.load(Ordering::Relaxed)
}

/// Spawn the pump task. Called once, at handoff: before init runs there is no
/// shell to carry output for, and after it runs there is no one else to do it.
pub fn init() {
    let id = crate::sched::spawn_permanent(mux_task, crate::sched::Priority::NORMAL);
    MUX_TASK.store(id.0 as u32, Ordering::Relaxed);
    // The mapping is worth a line on the wire: six consoles whose slaves are
    // /dev/tty1..6 and whose masters only the kernel holds, and the key that
    // turns the one serial wire between them. A login that names its terminal
    // `tty1` and a host that sees one serial port are only reconcilable if
    // somebody says how.
    crate::console::write_bytes(
        b"console mux: shells on tty1..tty6 (pty-backed); ^A 1..6 switches\n",
    );
}

fn mux_task() {
    let id = crate::sched::current();
    loop {
        let was_active = active();
        // Join every pair's wake list, every lap. Membership is not cached
        // because it does not need to be: the lists are small, and a wake
        // that misses because a list was drained between laps stalls every
        // console for exactly as long as the mux stays asleep.
        for pty in crate::vfs::pty::pairs() {
            pty.subscribe_master(id);
        }
        crate::sched::block_until(|| {
            active() != was_active
                || crate::vfs::pty::lookup(was_active).is_some_and(|pty| pty.master_ready())
        });
        // A switch: nothing to pump yet, the next lap reads the new console.
        if active() != was_active {
            continue;
        }
        // Drain this console's queue to the framebuffer. The sink is memory
        // now, so take more than one 512-byte wire-sized gulp per lap - but
        // with a budget, so a flood here can never keep the mux from looking at
        // whether the active console changed (that is what makes `^A` snappy).
        if let Some(pty) = crate::vfs::pty::lookup(was_active) {
            let mut budget = 64 * 1024;
            while budget > 0 {
                let Some(bytes) = pty.take_master(4096.min(budget)) else {
                    break;
                };
                budget -= bytes.len();
                crate::console::vc_output(was_active, &bytes);
                if bytes.len() < 4096 {
                    break;
                }
            }
        }
    }
}

/// One byte from COM1, already drained from the UART. Before the mux is up
/// this belongs to the console tty; after, to the active virtual console,
/// with `^A` held back for one byte to see whether a switch was asked for.
pub fn input(byte: u8) {
    if PENDING_PREFIX.swap(false, Ordering::Relaxed) {
        match byte {
            b'1'..=b'6' => {
                switch((byte - b'0') as u32);
            }
            // `^A` twice is one `^A` at the shell, the way screen does it.
            0x01 => forward(0x01),
            // Any other follower: the `^A` was itself, and goes through too.
            other => {
                forward(0x01);
                forward(other);
            }
        }
        return;
    }
    if byte == 0x01 {
        PENDING_PREFIX.store(true, Ordering::Relaxed);
        return;
    }
    forward(byte);
}

fn forward(byte: u8) {
    let Some(pty) = crate::vfs::pty::lookup(active()) else {
        return;
    };
    // The machine's very first reader is not on a virtual console: init runs
    // with its standard descriptors on the console tty, and so does any
    // emergency shell the initramfs drops to before a getty exists. Until a
    // reader has claimed the active console - `Tty::read` records its group -
    // typed bytes still belong to that first reader, or a boot that stops
    // halfway would take the keyboard with it.
    if pty.slave().foreground().is_none() {
        crate::vfs::tty::global().receive_byte(byte);
        return;
    }
    pty.slave().receive_byte(byte);
}

fn switch(to: u32) {
    if to > crate::vfs::pty::VC_COUNT {
        return;
    }
    ACTIVE.store(to, Ordering::Relaxed);
    // The pump is parked on the old console's master; the switch changed what
    // it waits for, and only a wake can make it look.
    let mux = MUX_TASK.load(Ordering::Relaxed);
    if mux != 0 {
        crate::sched::wake(TaskId(mux as usize));
    }
    // On the shared serial wire a word in the kernel's own voice tells the eye
    // which console the next bytes belong to. On the framebuffer it is not
    // needed - each console is its own screen, and the switch itself is the
    // join - so say nothing and let the redraw speak.
    if !crate::console::gpu_mode() {
        crate::console::write_bytes(alloc::format!("\r\n[tty{to}]\r\n").as_bytes());
    }
}

/// Route COM1's input: the mux's when it is up, the console tty's before.
/// The serial interrupt handler calls this for every byte it drains.
pub fn route_input(byte: u8) {
    if ready() {
        input(byte);
    } else {
        crate::vfs::tty::global().receive_byte(byte);
    }
}
