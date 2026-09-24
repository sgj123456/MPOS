//! The render task: the single owner of the GPU, and the only thing that
//! flushes it.
//!
//! One task holds the `VirtIOGpu` so there is exactly one caller of `flush()`.
//! That matters because the crate's flush busy-spins until the host has taken
//! the whole frame - a three-megabyte copy at 1024x768. Anywhere else in the
//! kernel a spin that long with interrupts masked would starve the clock and
//! the keyboard, which is the exact failure the serial console worked to fix.
//! So the design is: copy the changed screen's cells under the screens'
//! critical section (a fixed-size memcpy), leave the section, paint the pixels
//! from that copy with the clock live, and flush last. Nothing slow - neither
//! the million-pixel blit nor the DMA flush - runs with interrupts masked.
//!
//! Frames are coalesced: the task sleeps roughly a tick-locked beat and paints
//! only screens whose rows actually changed, so an idle console costs no DMA
//! and a flood costs one flush per frame, not one per byte. A `/dev/fb0` writer
//! that needs the write-through promise takes the same route by handing a
//! request to this task and parking until the frame left.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use virtio_drivers::device::gpu::VirtIOGpu;
use virtio_drivers::transport::pci::PciTransport;

use crate::drivers::virtio::gpu::VirtioGpuDev;
use crate::drivers::virtio::hal::HalImpl;
use crate::mm;
use crate::sched::policy::TaskId;
use crate::vfs::pty::VC_COUNT;
use crate::vfs::term::blit;
use crate::vfs::term::font::{CELL_H, CELL_W};
use crate::vfs::term::{self, Cell, DEFAULT_BG, DEFAULT_FG, MAX_ROWS};
use crate::vfs::vcmux;

/// The minimum ticks between repaints (~31 Hz at 250 Hz): the ceiling that
/// coalesces a flood, while a lone keystroke still repaints immediately.
const FRAME_TICKS: u64 = 8;

/// The device, parked here for the task to take at its start. After the take
/// nothing else can reach it, which is what keeps `flush` single-caller.
static DEVICE: Mutex<RefCell<Option<VirtIOGpu<HalImpl, PciTransport>>>> =
    Mutex::new(RefCell::new(None));
static VIEW: Mutex<RefCell<Option<Arc<VirtioGpuDev>>>> = Mutex::new(RefCell::new(None));
static RENDER_PID: AtomicU32 = AtomicU32::new(0);

/// The `/dev/fb0` write-through handshake. A writer bumps `REQ` and parks until
/// `DONE` catches up, which the task does right after a flush. Kept off the
/// screens' critical section: it is its own, and it never holds the device.
static REQ: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static WAITERS: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));

/// Whether the task only *presents* (a compositor owns the scanout) rather than
/// painting the console. Set once by [`spawn`]; the task never takes the console
/// over and never paints a text grid when this is true.
static PRESENT_ONLY: AtomicBool = AtomicBool::new(false);

/// A screen changed and needs repainting. Set by whoever feeds a grid, so the
/// render task wakes on the next keystroke instead of waiting out its beat -
/// and the task clears it when it repaints.
static DIRTY: AtomicBool = AtomicBool::new(false);

/// Nudge the render task to repaint soon. Called after any byte is painted into
/// a grid. Cheap and safe from any context (a keystroke's echo runs from the
/// interrupt handler): it only sets a flag and wakes the task.
pub fn notify() {
    DIRTY.store(true, Ordering::Relaxed);
    let pid = RENDER_PID.load(Ordering::Relaxed);
    if pid != 0 {
        crate::sched::wake(TaskId(pid as usize));
    }
}

/// Whether the framebuffer console is live. Cleared if the task was never
/// spawned (headless), so `console` and `/dev/fb0` know not to route bytes here.
pub fn running() -> bool {
    RENDER_PID.load(Ordering::Relaxed) != 0
}

/// Hand the device and its buffer view to a new render task. Called once, from
/// the boot, when a display was found and the machine is not headless.
/// `present_only` leaves the scanout to a userspace compositor: the task never
/// takes the console over and only flushes when `/dev/fb0` work asks.
pub fn spawn(
    device: VirtIOGpu<HalImpl, PciTransport>,
    view: Arc<VirtioGpuDev>,
    present_only: bool,
) {
    PRESENT_ONLY.store(present_only, Ordering::Relaxed);
    critical_section::with(|cs| {
        *DEVICE.borrow(cs).borrow_mut() = Some(device);
        *VIEW.borrow(cs).borrow_mut() = Some(view);
    });
    let id = crate::sched::spawn_permanent(task, crate::sched::Priority::NORMAL);
    RENDER_PID.store(id.0 as u32, Ordering::Relaxed);
    if present_only {
        // Nothing drives `notify` in this mode; wake the task once so it
        // settles into its present loop.
        notify();
    }
}

fn task() {
    let device = critical_section::with(|cs| DEVICE.borrow(cs).borrow_mut().take());
    let view = critical_section::with(|cs| VIEW.borrow(cs).borrow().clone());
    let (Some(mut device), Some(view)) = (device, view) else {
        return;
    };

    let (fb_w, fb_h) = {
        let (w, h) = view.dims();
        (w as usize, h as usize)
    };
    let fb_phys = view.fb_phys();
    let fb_len = view.fb_len();

    // Fill the whole scanout: a 1280x800 display at 8x16 is 160x50 cells, so
    // the text uses the window instead of a small corner of it.
    let rows = (fb_h / CELL_H).clamp(1, MAX_ROWS);
    let cols = (fb_w / CELL_W).max(1);

    // One screen per virtual console, so switching preserves each one's
    // contents - something the single shared wire could never do. Index `n`
    // is `/dev/ttyN`. Skipped for a compositor boot: the scanout is not the
    // console's, so no grid is made and the console is never taken over.
    let present_only = PRESENT_ONLY.load(Ordering::Relaxed);
    if !present_only {
        for vc in 1..=VC_COUNT as usize {
            term::make_screen(vc, rows, cols);
        }
        // The screens exist now: replay the boot log onto the console being shown
        // and move output from the wire to the framebuffer.
        crate::console::takeover();
    }

    // A scratch copy of one screen's cells, reused every frame, so the pixel
    // blit runs outside the critical section (see the module doc).
    let blank = Cell {
        ch: b' ',
        fg: DEFAULT_FG,
        bg: DEFAULT_BG,
    };
    let mut scratch = vec![blank; rows * cols];

    // Force a first paint of the replayed boot log even before any keystroke.
    DIRTY.store(true, Ordering::Relaxed);
    let mut last_shown = 0usize;
    let mut last_paint = 0u64;
    loop {
        // Wake the moment a screen changes - a keystroke should repaint now,
        // not on the next beat - or when a compositor writer asks for a
        // present. Never storming: sleep out the rest of the frame interval.
        crate::sched::block_until(|| {
            DIRTY.load(Ordering::Relaxed)
                || REQ.load(Ordering::Relaxed) > DONE.load(Ordering::Relaxed)
        });
        DIRTY.store(false, Ordering::Relaxed);
        let now = crate::arch::timer::ticks();
        if now.saturating_sub(last_paint) < FRAME_TICKS {
            crate::sched::sleep_ticks(FRAME_TICKS - (now - last_paint));
        }
        last_paint = crate::arch::timer::ticks();

        // Copy the changed screen's cells under the lock (a fixed memcpy), then
        // paint from the copy with the clock live. The flush is outside both.
        // In a compositor boot there is no console grid to paint - the pixels
        // are the compositor's already in the buffer - so this whole step is
        // skipped and only the `req > done` present below ever runs.
        let painted = if present_only {
            false
        } else {
            let shown = vcmux::active() as usize;
            let switched = shown != last_shown;
            last_shown = shown;
            if switched {
                term::with_screen(shown, |g| g.mark_all_dirty());
            }
            match term::snapshot(shown, &mut scratch) {
                Some(f) if f.dirty != 0 => {
                    let fb = unsafe {
                        core::slice::from_raw_parts_mut(
                            mm::phys_to_virt(fb_phys) as *mut u8,
                            fb_len,
                        )
                    };
                    blit::paint_cells(
                        &scratch,
                        f.rows,
                        f.cols,
                        f.cursor,
                        f.cursor_visible,
                        fb,
                        fb_w,
                        fb_h,
                        (0, 0),
                        f.dirty,
                    );
                    true
                }
                _ => false,
            }
        };

        let req = REQ.load(Ordering::Relaxed);
        let done = DONE.load(Ordering::Relaxed);
        if painted || req > done {
            let _ = device.flush();
            DONE.store(REQ.load(Ordering::Relaxed), Ordering::Relaxed);
            let waiters =
                critical_section::with(|cs| core::mem::take(&mut *WAITERS.borrow(cs).borrow_mut()));
            for id in waiters {
                crate::sched::wake(id);
            }
        }
    }
}

/// `write(2)` on `/dev/fb0`: block until the pixels this caller put in the
/// buffer have been flushed to the host, keeping the node's documented
/// write-through promise.
pub fn request_flush_blocking() {
    if !running() {
        return;
    }
    let me = crate::sched::current();
    let seq = REQ.fetch_add(1, Ordering::Relaxed) + 1;
    critical_section::with(|cs| {
        let mut waiters = WAITERS.borrow(cs).borrow_mut();
        if !waiters.contains(&me) {
            waiters.push(me);
        }
    });
    crate::sched::block_until(|| DONE.load(Ordering::Relaxed) >= seq);
    critical_section::with(|cs| {
        WAITERS.borrow(cs).borrow_mut().retain(|&w| w != me);
    });
}

/// The render task's id, so a producer that just changed a screen can nudge it
/// to repaint sooner than its next tick-locked lap. 0 when the task is not up.
pub fn render_pid() -> u32 {
    RENDER_PID.load(Ordering::Relaxed)
}
