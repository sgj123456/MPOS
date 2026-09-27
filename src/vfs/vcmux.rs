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

/// Each console's video mode (`KD_TEXT`/`KD_GRAPHICS`) and keyboard mode
/// (`K_XLATE`, `K_OFF`, ...), indexed 1-based like the consoles themselves.
///
/// Two arrays rather than a small struct per console because each is read on a
/// path that already exists and must not take a lock to find out: the render
/// task asking whether this console should be painted at all, and the mux
/// asking whether a keystroke belongs to a shell. `KDSETMODE` and `KDSKBMODE`
/// write them, which is what makes a seat's handover mean something instead of
/// being a request that was accepted and forgotten.
static KD_MODE: [AtomicU32; crate::vfs::pty::VC_COUNT as usize + 1] =
    [const { AtomicU32::new(crate::vfs::tty::KD_TEXT) }; crate::vfs::pty::VC_COUNT as usize + 1];
static KB_MODE: [AtomicU32; crate::vfs::pty::VC_COUNT as usize + 1] =
    [const { AtomicU32::new(crate::vfs::tty::K_XLATE) }; crate::vfs::pty::VC_COUNT as usize + 1];

/// The slot a console's mode lives in, or `None` for a number that is not one
/// of the consoles this machine has.
fn slot(vc: u32) -> Option<usize> {
    if vc == 0 || vc > crate::vfs::pty::VC_COUNT {
        None
    } else {
        Some(vc as usize)
    }
}

/// `KDSETMODE`: put a console in text or graphics mode. Refused for a console
/// that does not exist.
pub fn set_kd_mode(vc: u32, mode: u32) -> bool {
    match slot(vc) {
        Some(at) => {
            KD_MODE[at].store(mode, Ordering::Relaxed);
            // Taking the screen back is a change the render task has to notice
            // even though no console was switched: the framebuffer holds
            // whatever the compositor last drew, so the whole grid goes stale.
            crate::drivers::video::render::notify();
            true
        }
        None => false,
    }
}

/// `KDGETMODE`, and the answer for a console that is not one of ours is text -
/// there is nothing there to have taken the screen.
pub fn kd_mode(vc: u32) -> u32 {
    match slot(vc) {
        Some(at) => KD_MODE[at].load(Ordering::Relaxed),
        None => crate::vfs::tty::KD_TEXT,
    }
}

/// `KDSKBMODE`: which translation this console's keyboard is in.
pub fn set_kb_mode(vc: u32, mode: u32) -> bool {
    match slot(vc) {
        Some(at) => {
            KB_MODE[at].store(mode, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// `KDGKBMODE`, defaulting to `K_XLATE` as a console Linux never heard of would.
pub fn kb_mode(vc: u32) -> u32 {
    match slot(vc) {
        Some(at) => KB_MODE[at].load(Ordering::Relaxed),
        None => crate::vfs::tty::K_XLATE,
    }
}

/// Whether the console on the screen is one the kernel should be drawing.
///
/// A session that has put its console in `KD_GRAPHICS` owns that screen: the
/// text grid still exists and still scrolls, but painting it would overwrite
/// the compositor's output a line at a time - which is exactly what a login
/// shell echoing one keystroke did to weston, because every key the machine
/// sees is also fed to the active console.
pub fn painted() -> bool {
    kd_mode(active()) != crate::vfs::tty::KD_GRAPHICS
}

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

/// `VT_ACTIVATE`'s way in: make console `to` the one on the screen.
///
/// `false` says the number is not one of this machine's consoles, which is what
/// Linux answers `EINVAL` for. The `^A`-digit path uses the same door, so a
/// session's activation and an operator's keystroke cannot disagree about what
/// switching means.
pub fn activate(to: u32) -> bool {
    if slot(to).is_none() {
        return false;
    }
    switch(to);
    true
}

/// `VT_OPENQRY`: the lowest console that no one has taken over as their
/// terminal, or `None` when all of them are spoken for - which is what Linux
/// answers `ENXIO` for.
///
/// "Taken over" is asked of the console's own reader: a getty that has opened
/// `/dev/tty3` and is waiting on it has claimed that console for a session, and
/// one nobody has read from is still free for a seat manager to point a second
/// session at.
pub fn free_console() -> Option<u32> {
    (1..=crate::vfs::pty::VC_COUNT).find(|vc| {
        crate::vfs::pty::lookup(*vc).is_none_or(|pty| pty.slave().foreground().is_none())
    })
}

/// `VT_SETMODE`'s `waitv`: park until console `want` is the active one. Nothing
/// else can move the console while this sleeps, so the condition is checked
/// again on every wake and the wait is honest rather than a returned promise.
pub fn wait_active(want: u32) {
    crate::sched::block_until(|| active() == want);
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
    // Note what does *not* happen here: a `KDSKBMODE(K_OFF)` the session on a
    // console wrote does not stop these bytes. On Linux that mode belongs to
    // the AT keyboard, and the machine's other input path - the serial line a
    // person is watching and typing at - carries on regardless. Turning the
    // keyboard off there is what a compositor does to the *text* console, and
    // the text console's half of that is the screen, not the wire: see
    // [`painted`], which is where the compositor's ownership is honoured. A
    // drop here instead left the machine with no way to reach its own shell
    // after weston exited, because nothing restores a mode the session set on
    // the way out.
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
    // On the framebuffer, tell the renderer to show the new console now.
    if crate::console::gpu_mode() {
        crate::drivers::video::render::notify();
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
