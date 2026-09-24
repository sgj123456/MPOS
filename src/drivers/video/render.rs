//! The render task: the single owner of the GPU, and the only thing that
//! flushes it.
//!
//! One task holds the `VirtIOGpu` so there is exactly one caller of `flush()`.
//! That matters because the crate's flush busy-spins until the host has taken
//! the whole frame - a three-megabyte copy at 1024x768. Anywhere else in the
//! kernel a spin that long with interrupts masked would starve the clock and
//! the keyboard, which is the exact failure the serial console worked to fix.
//! So the design is: paint under the screens' critical section (fast, memory
//! only), leave the section, *then* flush with the clock live.
//!
//! Frames are coalesced: the task sleeps roughly a tick-locked beat and paints
//! only screens whose rows actually changed, so an idle console costs no DMA
//! and a flood costs one flush per frame, not one per byte. A `/dev/fb0` writer
//! that needs the write-through promise takes the same route by handing a
//! request to this task and parking until the frame left.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use critical_section::Mutex;
use virtio_drivers::device::gpu::VirtIOGpu;
use virtio_drivers::transport::pci::PciTransport;

use crate::drivers::virtio::gpu::VirtioGpuDev;
use crate::drivers::virtio::hal::HalImpl;
use crate::mm;
use crate::sched::policy::TaskId;
use crate::vfs::term::font::{CELL_H, CELL_W};
use crate::vfs::term::{self, KERNEL_SCREEN, MAX_ROWS};

/// Rows and columns the console screen is sized to: the same 80x25 the line
/// discipline's cursor model uses, so the two never disagree about a position.
const SCREEN_ROWS: usize = 25;
const SCREEN_COLS: usize = 80;

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

/// Whether the framebuffer console is live. Cleared if the task was never
/// spawned (headless), so `console` and `/dev/fb0` know not to route bytes here.
pub fn running() -> bool {
    RENDER_PID.load(Ordering::Relaxed) != 0
}

/// Hand the device and its buffer view to a new render task. Called once, from
/// the boot, when a display was found and the machine is not headless.
pub fn spawn(device: VirtIOGpu<HalImpl, PciTransport>, view: Arc<VirtioGpuDev>) {
    critical_section::with(|cs| {
        *DEVICE.borrow(cs).borrow_mut() = Some(device);
        *VIEW.borrow(cs).borrow_mut() = Some(view);
    });
    let id = crate::sched::spawn_permanent(task, crate::sched::Priority::NORMAL);
    RENDER_PID.store(id.0 as u32, Ordering::Relaxed);
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

    // A screen can never show more rows than the pixels allow, nor more than
    // the dirty mask tracks.
    let rows = (fb_h / CELL_H).min(SCREEN_ROWS).clamp(1, MAX_ROWS);
    let cols = (fb_w / CELL_W).clamp(1, SCREEN_COLS);
    term::make_screen(KERNEL_SCREEN, rows, cols);
    // A first thing to see, so the very act of bringing the console up is
    // visible on the display: the marker below is proof the render path works
    // even before any program has written a byte.
    term::with_screen(KERNEL_SCREEN, |g| {
        g.feed(b"mpos: framebuffer console online\r\n")
    });

    loop {
        crate::sched::sleep_ticks(8);

        // Paint under a short-lived borrow of the buffer, then release it
        // before the flush so the DMA spin holds no lock and masks no clock.
        let painted = {
            let fb = unsafe {
                core::slice::from_raw_parts_mut(mm::phys_to_virt(fb_phys) as *mut u8, fb_len)
            };
            term::render_screen(KERNEL_SCREEN, fb, fb_w, fb_h, (0, 0))
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
