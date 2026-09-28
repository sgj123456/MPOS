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

/// The host asked for a different display size. Set by the virtio service task,
/// which is what polls the GPU's device configuration for the host's event (see
/// `virtio::gpu::take_display_event`), and taken by the render task, which is
/// the only context that may touch the device.
static RESIZE: AtomicBool = AtomicBool::new(false);

/// Nudge the render task to repaint soon. Called after any byte is painted into
/// a grid. Cheap and safe from any context (a keystroke's echo runs from the
/// interrupt handler): it only sets a flag and wakes the task.
pub fn notify() {
    DIRTY.store(true, Ordering::Relaxed);
    wake_task();
}

/// Tell the render task the host wants the display at a different size.
///
/// Called by the virtio service task, which notices the host's event while
/// polling; *doing* something about it is the render task's because it owns the
/// device and is the only caller of `flush`. The task re-reads the size itself,
/// so a burst of events - a window drag sends one per motion event - costs one
/// rebuild per frame rather than one per event.
pub fn request_resize() {
    RESIZE.store(true, Ordering::Relaxed);
    wake_task();
}

/// Wake the render task so that it looks at its condition again.
///
/// Every flag the task waits on needs this, and the reason is the kind of sleep
/// it takes: `block_until` parks with no deadline - "until someone wakes me" -
/// so a condition that becomes true without a wake is a condition the task will
/// not read until something *else* happens to wake it. On an idle machine
/// nothing else does, which is what turned a `/dev/fb0` write into two tasks
/// waiting for each other: the request counter said there was work, and nothing
/// said "look".
fn wake_task() {
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

/// Whether a writer is waiting for a frame it has already put in the buffer.
///
/// The task's other reason to wake, alongside a dirty console - and the only
/// one on a compositor boot, where there is no console grid to repaint.
fn pending() -> bool {
    REQ.load(Ordering::Relaxed) > DONE.load(Ordering::Relaxed)
}

fn task() {
    let device = critical_section::with(|cs| DEVICE.borrow(cs).borrow_mut().take());
    let view = critical_section::with(|cs| VIEW.borrow(cs).borrow().clone());
    let (Some(mut device), Some(view)) = (device, view) else {
        return;
    };

    let mut fb_w = view.dims().0 as usize;
    let mut fb_h = view.dims().1 as usize;
    let mut fb_phys = view.fb_phys();
    let mut fb_len = view.fb_len();

    // Fill the whole scanout: a 1280x800 display at 8x16 is 160x50 cells, so
    // the text uses the window instead of a small corner of it.
    let mut rows = (fb_h / CELL_H).clamp(1, MAX_ROWS);
    let mut cols = (fb_w / CELL_W).max(1);

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
        // not on the next beat - or when a writer asks for a frame it has
        // already put in the buffer. An idle machine does neither, and this
        // wait has no deadline, so an idle console really does cost nothing.
        crate::sched::block_until(|| {
            DIRTY.load(Ordering::Relaxed) || pending() || RESIZE.load(Ordering::Relaxed)
        });
        DIRTY.store(false, Ordering::Relaxed);

        // The beat coalesces *repaints*, not *requests*. A repaint is work
        // nobody asked for, and deferring it by a frame is invisible; a request
        // has a writer parked in `write(2)` until its frame leaves, so pacing it
        // at the beat would make a 4 KiB write cost a frame and a screenful
        // cost a minute - which is what a `/dev/fb0` writer would have paid.
        let now = crate::arch::timer::ticks();
        if !pending() && now.saturating_sub(last_paint) < FRAME_TICKS {
            crate::sched::sleep_ticks(FRAME_TICKS - (now - last_paint));
        }
        last_paint = crate::arch::timer::ticks();

        // The host asking for a different display, taken before the paint so
        // that this frame already uses the new geometry - and after the beat,
        // because the beat is what turns a drag's stream of events into one
        // rebuild per frame (the task re-reads the size, so every event after
        // the first of a frame finds nothing left to do).
        if RESIZE.swap(false, Ordering::Relaxed)
            && let Some((width, height)) = resize(&mut device, &view, present_only)
        {
            fb_w = width as usize;
            fb_h = height as usize;
            fb_phys = view.fb_phys();
            fb_len = view.fb_len();
            rows = (fb_h / CELL_H).clamp(1, MAX_ROWS);
            cols = (fb_w / CELL_W).max(1);
            scratch = vec![blank; rows * cols];
            // Whatever was on the screen belongs to the old one: paint the
            // whole grid rather than the rows that changed since.
            last_shown = 0;
        }

        // Copy the changed screen's cells under the lock (a fixed memcpy), then
        // paint from the copy with the clock live. The flush is outside both.
        // Two boots have no console grid to paint: a compositor boot, where the
        // pixels are already in the buffer, and any moment the console on the
        // screen has been put in graphics mode by the session that took it - the
        // grid still scrolls, and painting it would overwrite weston's output
        // with a login prompt one keystroke at a time.
        let painted = if present_only || !crate::vfs::vcmux::painted() {
            // Forget which console was last shown: when the screen comes back,
            // whatever is on it belongs to something else, and the whole grid
            // needs redrawing rather than the changed rows.
            last_shown = 0;
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

        // The sequence this frame answers for, read *before* the flush: a
        // request that arrives after this load is not in the frame, so marking
        // it done would let its writer return with its pixels still only in the
        // buffer - the one thing the write-through promise says cannot happen.
        let req = REQ.load(Ordering::Relaxed);
        let done = DONE.load(Ordering::Relaxed);
        if painted || req > done {
            let _ = device.flush();
            DONE.store(req, Ordering::Relaxed);
            let waiters =
                critical_section::with(|cs| core::mem::take(&mut *WAITERS.borrow(cs).borrow_mut()));
            for id in waiters {
                crate::sched::wake(id);
            }
        }
    }
}

/// Rebuild the scanout at the size the host now wants, and tell the rest of
/// the machine that the display changed.
///
/// This is where a QEMU window resize turns into a screen of the same size. The
/// host keeps the size it wants in the GPU's own device configuration, and asks
/// for it with an event the service task polls; what it actually *is* comes from
/// `GET_DISPLAY_INFO`, and it can differ from what the machine is driving in
/// either direction (a fullscreen toggle asks for a *smaller* size, too).
///
/// `None` means the geometry did not change: the host reports a zero-pixel
/// display when it has none attached (a headless QEMU answers every query with
/// nothing, which is not a request for an empty screen), QEMU sends the event
/// for a window *move* as well as a resize, and a size the host refuses leaves
/// the screen it had. The first two are no-ops, and the third rebuilds what was
/// there and says so - in every case the next event may try again.
fn resize(
    device: &mut VirtIOGpu<HalImpl, PciTransport>,
    view: &Arc<VirtioGpuDev>,
    present_only: bool,
) -> Option<(u32, u32)> {
    let (width, height) = match device.resolution() {
        Ok((width, height)) if width > 0 && height > 0 => (width, height),
        _ => return None,
    };
    let (old_w, old_h) = view.dims();
    if (width, height) == (old_w, old_h) {
        return None;
    }
    let old_phys = view.fb_phys();
    let old_len = view.fb_len();

    // The old block is *still pinned* as the crate tears the resource down, and
    // that is on purpose: `dma_dealloc` refuses a pinned range, so the frames the
    // old screen was drawn from are still ours to read below. What was on the
    // screen goes back on it in the same call - a resize that blanked the window
    // until the compositor happened to repaint would be worse than the corner it
    // is fixing.
    let slice = match device.change_resolution(width, height) {
        Ok(slice) => slice,
        Err(_) => {
            // The host refused the new size - and asking tore the old scanout
            // down, so the machine has no screen at all until one is built. Put
            // the size it had back, which at least leaves the machine as it was:
            // the block the old screen lived in is left pinned and unused rather
            // than handed to the allocator while it may still be readable, so
            // the price of a refused resize is one leaked screen.
            let Ok(slice) = device.change_resolution(old_w, old_h) else {
                return None;
            };
            let (fb_len, fb_phys) = adopt(slice, old_w, old_h, old_phys, old_w, old_h, old_len);
            view.resized(old_w, old_h, fb_phys, fb_len);
            crate::println!("virtio-gpu: {width}x{height} refused; staying at {old_w}x{old_h}");
            return None;
        }
    };
    let (fb_len, fb_phys) = adopt(slice, width, height, old_phys, old_w, old_h, old_len);

    // The old block is the allocator's again - unless userland has mapped the
    // pixels, because then a mapping exists that would go on writing into frames
    // the allocator is free to hand to someone else.
    if !view.mapped() {
        mm::unpin_frames(old_phys, old_len);
        mm::free_frames(old_phys, order_of(old_len));
    }

    view.resized(width, height, fb_phys, fb_len);

    if !present_only {
        // The console owns the screen and its grid *is* the screen's size, so
        // the grids are rebuilt - at the *new* cell count, computed here rather
        // than passed in, because that is the point - and the boot log replayed
        // onto the one being shown. A resize costs the scrollback, which is the
        // honest price: the other virtual consoles' contents are not kept
        // anywhere but in their own grids.
        let rows = (height as usize / CELL_H).clamp(1, MAX_ROWS);
        let cols = (width as usize / CELL_W).max(1);
        for vc in 1..=VC_COUNT as usize {
            term::make_screen(vc, rows, cols);
        }
        crate::console::takeover();
    }

    // Whoever owns the scanout has to be *told*: a compositor takes its mode
    // from the connector, and a mode change it is not told about is a mode it
    // never sets - which is exactly the bug this whole path exists for. Linux
    // announces it as a DRM hotplug uevent, and that is what this publishes.
    crate::drivers::drm::display_changed();

    // Said out loud, like the boot's own `virtio-gpu: WxH at /dev/fb0` line and
    // for the same reason: the size of the screen is the first thing anyone
    // debugging a display wants to know, and it just changed under them. After
    // the grids are rebuilt, so the line lands on the screen that exists now.
    crate::println!(
        "virtio-gpu: display {old_w}x{old_h} -> {width}x{height}, {} KiB at /dev/fb0",
        fb_len / 1024
    );
    Some((width, height))
}

/// Take a freshly created scanout into service: pin its frames, put what was on
/// the old screen onto it, and hand back the block's length and address for
/// [`VirtioGpuDev::resized`].
///
/// Both the resize and its failure path do exactly these three things, differing
/// only in the sizes they name, which is why they are one function.
fn adopt(
    slice: &mut [u8],
    width: u32,
    height: u32,
    old_phys: usize,
    old_w: u32,
    old_h: u32,
    old_len: usize,
) -> (usize, usize) {
    let fb_len = slice.len();
    let fb_phys = mm::virt_to_phys(slice.as_ptr() as usize);
    mm::pin_frames(fb_phys, fb_len.div_ceil(mm::PAGE_SIZE) * mm::PAGE_SIZE);
    restore(
        slice,
        width as usize,
        height as usize,
        old_phys,
        old_w as usize,
        old_h as usize,
        old_len,
    );
    (fb_len, fb_phys)
}

/// Put what was on the old scanout back on the new one: the rectangle the two
/// share, row by row.
///
/// Row by row because the two buffers have different pitches - each is its own
/// width times four bytes - so a whole-buffer copy would smear every row after
/// the first. `old` is the block the crate has just released and has not yet
/// handed back (see the caller): the frames are still readable, and the range
/// is exactly `old_phys .. old_phys + old_len`.
fn restore(
    new: &mut [u8],
    new_w: usize,
    new_h: usize,
    old_phys: usize,
    old_w: usize,
    old_h: usize,
    old_len: usize,
) {
    if new_w == 0 || old_w == 0 {
        return;
    }
    // SAFETY: `old_phys` is the framebuffer the device was driving until this
    // call, pinned for the machine's life and not yet returned to the allocator,
    // so the direct map reaches all of `old_len` bytes of it.
    let old =
        unsafe { core::slice::from_raw_parts(mm::phys_to_virt(old_phys) as *const u8, old_len) };
    let rows = old_h.min(new_h);
    let bytes = old_w.min(new_w) * 4;
    for y in 0..rows {
        let (src, dst) = (y * old_w * 4, y * new_w * 4);
        new[dst..dst + bytes].copy_from_slice(&old[src..src + bytes]);
    }
}

/// The buddy order `HalImpl::dma_alloc` gave a block of `len` bytes: its page
/// count rounded up to a power of two.
///
/// The HAL's arithmetic, spelled here because a driver that has to free one of
/// those blocks *by hand* - the crate frees its own `Dma`, but only after this
/// path has copied what was on the screen out of it - has to name the same
/// order the allocator was asked for.
fn order_of(len: usize) -> usize {
    len.div_ceil(mm::PAGE_SIZE)
        .next_power_of_two()
        .trailing_zeros() as usize
}

/// `write(2)` on `/dev/fb0`: block until the pixels this caller put in the
/// buffer have been flushed to the host, keeping the node's documented
/// write-through promise.
///
/// The request is a *notification* as much as a counter. The task sleeps
/// without a deadline, so bumping `REQ` and not waking it leaves the work
/// unread - and the writer below waiting for a flush that nothing is going to
/// make. Every caller of this is a producer talking to a sleeper, which is why
/// the wake belongs here rather than at each of them.
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
    // Registered before the wake, so a task that flushes immediately still
    // finds this writer to wake; and woken after registering, so the request
    // is read at all.
    wake_task();
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
