//! The render task: the single owner of the GPU, and the only thing that
//! flushes it.
//!
//! One task holds the `Gpu` so there is exactly one caller of `flush()`.
//! That matters because the flush busy-spins until the host has taken
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

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::drivers::virtio::gpu_ctrl::Gpu;
use critical_section::Mutex;

use crate::drivers::virtio::gpu::VirtioGpuDev;
use crate::mm;
use crate::sched::policy::TaskId;
use crate::term::blit;
use crate::term::font::{CELL_H, CELL_W};
use crate::term::vc::VC_COUNT;
use crate::term::{self, Cell, DEFAULT_BG, DEFAULT_FG, MAX_ROWS};

/// The minimum ticks between repaints (~31 Hz at 250 Hz): the ceiling that
/// coalesces a flood, while a lone keystroke still repaints immediately.
const FRAME_TICKS: u64 = 8;

/// The device, parked here for the task to take at its start. After the take
/// nothing else can reach it, which is what keeps `flush` single-caller.
static DEVICE: Mutex<RefCell<Option<Gpu>>> = Mutex::new(RefCell::new(None));
static VIEW: Mutex<RefCell<Option<Arc<VirtioGpuDev>>>> = Mutex::new(RefCell::new(None));
static RENDER_PID: AtomicU32 = AtomicU32::new(0);

/// The `/dev/fb0` write-through handshake. A writer bumps `REQ` and parks until
/// `DONE` catches up, which the task does right after a flush. Kept off the
/// screens' critical section: it is its own, and it never holds the device.
static REQ: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static WAITERS: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));

/// A screen changed and needs repainting. Set by whoever feeds a grid, so the
/// render task wakes on the next keystroke instead of waiting out its beat -
/// and the task clears it when it repaints.
static DIRTY: AtomicBool = AtomicBool::new(false);

/// How many frames have been flushed to the host. `FBIO_WAITFORVSYNC` and the
/// DRM flip events read this as "the frame boundary moved"; it is a count and
/// not a clock, because the only boundary this machine has is a frame it
/// presented. Calling it a vblank counter would be a lie the ABI cannot tell.
static LANDINGS: AtomicU64 = AtomicU64::new(0);

/// The frame boundary counter: how many frames have gone to the host since boot.
pub fn landings() -> u64 {
    LANDINGS.load(Ordering::Relaxed)
}

/// The host's display changed shape. Set by the virtio service task, which is
/// what polls the GPU's device configuration for the host's event (see
/// `virtio::gpu::take_display_event`), and taken by the render task, which is
/// the only context that may touch the device.
///
/// It requests a *probe*, not a resize: the task re-reads `GET_DISPLAY_INFO`,
/// records the new size as the connector's advertised mode and fires a hotplug
/// uevent. It does not touch the scanout - the screen follows only when a KMS
/// master commits a mode of that size, which arrives through [`resize_blocking`].
/// This is the Linux virtio-gpu split: the driver learns and announces, the
/// compositor drives.
static PROBE: AtomicBool = AtomicBool::new(false);

/// A scanout rebuild a modeset has asked for, as `width << 32 | height`; zero
/// when none is pending. The render task owns the device and its control queue,
/// so a DRM commit cannot resize on its own: it publishes the target here, bumps
/// `RESIZE_SEQ`, and parks until `RESIZE_ACK` catches up. One request in flight
/// at a time, because only the card's single master may commit a mode and its
/// commits are serial.
static RESIZE_TARGET: AtomicUsize = AtomicUsize::new(0);
static RESIZE_SEQ: AtomicU32 = AtomicU32::new(0);
static RESIZE_ACK: AtomicU32 = AtomicU32::new(0);
static RESIZE_OK: AtomicBool = AtomicBool::new(false);
static RESIZE_WAITERS: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));

/// Re-present the DRM master's current framebuffer onto the scanout. Set by
/// `drm::grant_and_resent` when the screen moves back onto the master's console:
/// the text render overwrote the desktop's pixels, and the compositor will not
/// re-commit them on its own until a mouse event, so the kernel shows the master's
/// last frame here instead - the switch back is then visible on the same frame, not
/// on the next input. Taken and cleared by the render task, the only flusher.
static RE_PRESENT: AtomicBool = AtomicBool::new(false);

/// Ask the render task to re-present the compositor's framebuffer and flush.
/// Called from the mux's VT-switch path; wakes the task so it happens now.
pub fn request_re_present() {
    RE_PRESENT.store(true, Ordering::Relaxed);
    wake_task();
}

/// Nudge the render task to repaint soon. Called after any byte is painted into
/// a grid. Cheap and safe from any context (a keystroke's echo runs from the
/// interrupt handler): it only sets a flag and wakes the task.
pub fn notify() {
    DIRTY.store(true, Ordering::Relaxed);
    wake_task();
}

/// A device command the DRM layer needs performed.
///
/// The DRM family's resource requests are the caller: `RESOURCE_CREATE` has to
/// tell the *device* to make a resource and attach the caller's pages to it, and
/// the device's control queue belongs to this task. So the ioctl does not reach
/// the device - it describes the work here and parks.
///
/// Nothing in this enum reads an answer out of the device, and that is why
/// [`job`] answers an errno and nothing else: a resource id is the DRM layer's
/// to choose (the device does not allocate them), a size and a handle are the
/// layer's own bookkeeping, and the one thing that *is* an answer - a cap set -
/// is fetched once at bring-up rather than per request, which
/// `crate::gpu::virtgpu::Capset` says more about.
pub enum Job {
    /// Create the 2D resource `id` and attach `entries` as its backing.
    Create2d {
        id: u32,
        width: u32,
        height: u32,
        entries: Vec<(u64, u32)>,
    },
    /// Release the resource `id`.
    Unref { id: u32 },
    /// Copy `id`'s `width` x `height` pixels from guest memory into the device.
    TransferToHost { id: u32, width: u32, height: u32 },
    /// Create the 3D resource `id`, attach `entries` as its backing, and let the
    /// context `ctx` name it.
    ///
    /// Three commands and one job, for the reason the 2D pair is one: a resource
    /// created and never backed is one the device reads from nowhere, and a 3D
    /// resource no context has been told about is one the stream that wants it
    /// cannot name. The attach is what carries the context - Linux's
    /// `RESOURCE_CREATE_3D` leaves the header's `ctx_id` zero - so all three go
    /// in that order and all three have to land.
    Create3d {
        ctx: u32,
        id: u32,
        request: crate::gpu::abi::ResourceCreate3d,
        entries: Vec<(u64, u32)>,
    },
    /// Copy a box of the resource between guest memory and the device.
    TransferHost3d {
        to_host: bool,
        ctx: u32,
        request: crate::gpu::abi::TransferHost3d,
    },
    /// Make the context `id`, naming the cap set it runs.
    ContextCreate {
        id: u32,
        context_init: u32,
        name: Vec<u8>,
    },
    /// Submit `stream` to context `ctx`, under the fence id `fence`.
    ///
    /// The one job whose payload is a caller's bytes rather than a shape this
    /// kernel chose, and they are forwarded untouched: `virtgpu_abi::CmdSubmit`
    /// says why, and why that is not a parser and wants no fuzz target.
    Submit3d {
        ctx: u32,
        ring_idx: u8,
        fence: u64,
        stream: Vec<u8>,
    },
}

/// One queued job and how its requester learns the answer.
///
/// A `Vec` of waiters would do as well as the count, and the shape here is the
/// same one `resize_blocking` uses: the requester's task id goes into a list
/// *before* the wake, so a task that runs immediately still finds someone to
/// wake, and the answer is posted before `done` is set, so a task that sees
/// `done` sees the result. Both orderings matter and neither is decoration.
struct Pending {
    kind: Job,
    done: AtomicBool,
    result: AtomicIsize,
}

/// The jobs waiting for the render task, in the order they were posted.
static JOBS: Mutex<RefCell<VecDeque<Arc<Pending>>>> = Mutex::new(RefCell::new(VecDeque::new()));
static JOB_WAITERS: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));

/// Ask the render task to perform a device command, and park until it has.
///
/// Answers `0` or a negative errno, and the errno is `EIO` for every failure the
/// device reports: `gpu_ctrl` collapses the device's `RESP_ERR_*` codes into one
/// error because nothing in the DRM layer branches on which one came back. A
/// caller that needed the code would want it routed out of `gpu_ctrl` first,
/// with the request it answered - and there is no such caller.
///
/// Without a render task there is no device to command, and that is `ENOSYS`
/// rather than a failure to allocate: a machine with no GPU has not run out of
/// anything.
pub fn job(kind: Job) -> isize {
    if !running() {
        return crate::err::ENOSYS;
    }
    let pending = Arc::new(Pending {
        kind,
        done: AtomicBool::new(false),
        result: AtomicIsize::new(crate::err::EIO),
    });
    critical_section::with(|cs| {
        JOBS.borrow(cs).borrow_mut().push_back(pending.clone());
    });
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut waiters = JOB_WAITERS.borrow(cs).borrow_mut();
        if !waiters.contains(&me) {
            waiters.push(me);
        }
    });
    wake_task();
    crate::sched::block_until(|| pending.done.load(Ordering::Relaxed));
    critical_section::with(|cs| {
        JOB_WAITERS.borrow(cs).borrow_mut().retain(|&w| w != me);
    });
    pending.result.load(Ordering::Relaxed)
}

/// Whether anything is waiting for the task. Part of the loop's wake condition:
/// a requester that posted a job and woke this task needs the task to look, and
/// nothing else in the loop would make it.
fn jobs_pending() -> bool {
    critical_section::with(|cs| !JOBS.borrow(cs).borrow().is_empty())
}

/// Perform every queued job, in order, and wake whoever asked.
fn drain_jobs(device: &mut Gpu) {
    loop {
        let Some(pending) = critical_section::with(|cs| JOBS.borrow(cs).borrow_mut().pop_front())
        else {
            return;
        };
        let result = perform(device, &pending.kind);
        // The result before the flag: a requester that reads `done` true must
        // already be able to read the answer, and this is the pair of stores the
        // release/acquire ordering behind `block_until` is about.
        pending.result.store(result, Ordering::Relaxed);
        pending.done.store(true, Ordering::Relaxed);
        let waiters =
            critical_section::with(|cs| core::mem::take(&mut *JOB_WAITERS.borrow(cs).borrow_mut()));
        for id in waiters {
            crate::sched::wake(id);
        }
    }
}

/// Run one job on the device, answering an errno.
///
/// A `CREATE` is two commands and both must land, which is why they are one job
/// rather than two: a resource created and never backed is a resource the device
/// will read from nowhere the moment anything is transferred to it.
fn perform(device: &mut Gpu, kind: &Job) -> isize {
    let done = match kind {
        Job::Create2d {
            id,
            width,
            height,
            entries,
        } => device
            .create_2d(*id, *width, *height)
            .and_then(|()| device.attach_backing(*id, entries)),
        Job::Unref { id } => device.unref(*id),
        Job::TransferToHost { id, width, height } => {
            device.transfer_to_host_2d(*id, *width, *height)
        }
        Job::Create3d {
            ctx,
            id,
            request,
            entries,
        } => device
            .create_3d(request)
            .and_then(|()| device.attach_backing(*id, entries))
            .and_then(|()| device.ctx_attach_resource(*ctx, *id)),
        Job::TransferHost3d {
            to_host,
            ctx,
            request,
        } => device.transfer_host_3d(*to_host, *ctx, request),
        Job::ContextCreate {
            id,
            context_init,
            name,
        } => device.ctx_create(*id, *context_init, name),
        Job::Submit3d {
            ctx,
            ring_idx,
            fence,
            stream,
        } => device.submit_3d(*ctx, *ring_idx, *fence, stream),
    };
    match done {
        Ok(()) => 0,
        Err(_) => crate::err::EIO,
    }
}

/// Hand the DRM layer what the device negotiated, and cache the cap sets it
/// offers.
///
/// Called once, at the top of the task, which is the only place the device is
/// reachable from: the DRM layer's ioctl path cannot touch it, and none of these
/// values changes after negotiation, so they are published rather than asked for
/// per request. `drivers/drm.rs`'s `Published` is the other end.
///
/// The walk is bounded by the device's own `num_capsets` and fetches each set at
/// the *version* `GET_CAPSET_INFO` reported - which is the version a caller asks
/// for later, so the cache holds what will be asked for. A set whose fetch fails
/// is skipped rather than aborting the walk: a device that offers a set it cannot
/// then produce has still told the truth about the others, and because
/// `GETPARAM(7)`'s mask is derived from the table that survives, the mask stays
/// honest about what is really there.
///
/// This runs before anything paints, so the two control-queue round trips per set
/// are paid once at boot rather than on a `GET_CAPS` a compositor makes on every
/// startup.
fn publish_card(device: &mut Gpu) {
    use crate::gpu::virtgpu::{Capset, Card};

    let features = device.features();
    let num_capsets = device.num_capsets();
    let mut capsets = Vec::new();
    for index in 0..num_capsets {
        let Ok(info) = device.get_capset_info(index) else {
            continue;
        };
        let mut bytes = Vec::new();
        bytes.resize(info.capset_max_size as usize, 0);
        let Ok(len) = device.get_capset(info.capset_id, info.capset_max_version, &mut bytes) else {
            continue;
        };
        bytes.truncate(len);
        capsets.push(Capset {
            id: info.capset_id,
            max_version: info.capset_max_version,
            max_size: info.capset_max_size,
            bytes,
        });
    }
    let card = Card {
        virgl: features.contains(crate::gpu::abi::Features::VIRGL),
        context_init: features.contains(crate::gpu::abi::Features::CONTEXT_INIT),
        capset_ids: crate::gpu::virtgpu::table_mask(&capsets),
    };
    // Said out loud, and the `cached=` count is the half worth having: a cap set
    // whose fetch failed is skipped silently (`continue` above), so without this
    // line a device that offered two sets and produced none looks exactly like a
    // device that had none - and `GETPARAM(7)` answering `0` would be the only
    // trace, indistinguishable from the honest no-3D answer.
    crate::println!(
        "virtio-gpu: virgl={} context_init={} capsets={num_capsets} cached={} mask={:#x}",
        card.virgl as u8,
        card.context_init as u8,
        capsets.len(),
        card.capset_ids,
    );
    crate::drivers::drm::publish_card(card, num_capsets, &capsets);
}

/// Tell the render task the host's display changed size.
///
/// Called by the virtio service task, which notices the host's event while
/// polling; *acting* on it is the render task's because it owns the device and is
/// the only caller of `flush`. The action is a probe, not a resize, so a burst of
/// events - a window drag sends one per motion event - costs one re-query per
/// frame rather than a rebuild per event.
pub fn request_display_probe() {
    PROBE.store(true, Ordering::Relaxed);
    wake_task();
}

/// Rebuild the scanout at the size a modeset committed, and answer whether the
/// host accepted it.
///
/// The DRM ioctl that carries a compositor's mode cannot resize the virtio-gpu
/// itself - the render task owns the device - so it hands the target to that task
/// and parks here until the rebuild is done, exactly the request/park discipline
/// `/dev/fb0`'s write-through uses. The return is the host's answer: `true` means
/// the scanout now drives `width`x`height`, `false` that it refused and the old
/// screen is back - the caller turns that into `EINVAL`, the answer Linux gives a
/// modeset the panel will not do.
pub fn resize_blocking(width: u32, height: u32) -> bool {
    if !running() {
        return false;
    }
    let me = crate::sched::current();
    RESIZE_TARGET.store(
        ((width as usize) << 32) | height as usize,
        Ordering::Relaxed,
    );
    let seq = RESIZE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    critical_section::with(|cs| {
        let mut waiters = RESIZE_WAITERS.borrow(cs).borrow_mut();
        if !waiters.contains(&me) {
            waiters.push(me);
        }
    });
    // Registered before the wake, so a task that rebuilds immediately still finds
    // this caller to wake. The target was stored before the sequence, so a task
    // that sees `RESIZE_SEQ` advanced has a target to build.
    wake_task();
    crate::sched::block_until(|| RESIZE_ACK.load(Ordering::Relaxed) >= seq);
    critical_section::with(|cs| {
        RESIZE_WAITERS.borrow(cs).borrow_mut().retain(|&w| w != me);
    });
    RESIZE_OK.load(Ordering::Relaxed)
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
/// spawned (no display), so `console` and `/dev/fb0` know not to route bytes
/// here. Note this is not "the kernel console is on the screen": a boot that
/// keeps its log on the wire still has a render task, because `/dev/fb0` is a
/// device a program may open and the scanout may be handed to a text console.
pub fn running() -> bool {
    RENDER_PID.load(Ordering::Relaxed) != 0
}

/// Hand the device and its buffer view to a new render task. Called once, from
/// the boot, when a display was found.
///
/// What the task does with the screen follows the console list, and this function
/// does not decide any of it: the framebuffer console's being one of the
/// consoles is what moves the kernel's log onto it, and a session claiming the
/// card is what brings the grids up for a screen somebody else is driving.
pub fn spawn(device: Gpu, view: Arc<VirtioGpuDev>) {
    critical_section::with(|cs| {
        *DEVICE.borrow(cs).borrow_mut() = Some(device);
        *VIEW.borrow(cs).borrow_mut() = Some(view);
    });
    let id = crate::sched::spawn_permanent(task, crate::sched::Priority::NORMAL);
    RENDER_PID.store(id.0 as u32, Ordering::Relaxed);
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
    // Before anything paints: what the device negotiated, and the cap sets it
    // offers. A compositor's first `GETPARAM` is what this answers.
    publish_card(&mut device);

    let mut fb_w = view.dims().0 as usize;
    let mut fb_h = view.dims().1 as usize;
    let mut fb_phys = view.fb_phys();
    let mut fb_len = view.fb_len();

    // Fill the whole scanout: a 1280x800 display at 8x16 is 160x50 cells, so
    // the text uses the window instead of a small corner of it.
    let mut rows = (fb_h / CELL_H).clamp(1, MAX_ROWS);
    let mut cols = (fb_w / CELL_W).max(1);

    // One screen per virtual console, so switching preserves each one's
    // contents - something the single shared wire could never do. Index `n` is
    // `/dev/ttyN`.
    //
    // Made whenever there is a display, whatever the console list says, and for
    // one reason with two faces: a virtual console a compositor releases has to
    // be drawable by somebody, and the console's *window* - what `TIOCGWINSZ`
    // answers and what a full-screen program lays itself out in - is this grid,
    // so that number has to be the same in every mode rather than one that moves
    // the moment a session claims the card.
    //
    // The rejected alternative was to make them on the first `SET_MASTER`: six
    // grids cheaper on a boot that never draws, but the console's geometry then
    // depends on who holds the card - which is neither Linux's answer (fbcon
    // binds to the display whatever the DRM master is doing) nor a stable one.
    // `t-ioctl` is what caught it: it read 160x50 where the same boot had
    // answered 80x25 earlier, because a `SET_MASTER` had happened in between.
    for vc in 1..=VC_COUNT as usize {
        term::make_screen(vc, rows, cols);
    }
    if crate::console::vt_is_sink() {
        // The framebuffer console is one of this boot's consoles: replay the boot
        // log onto the console being shown, and move output from the wire to it.
        crate::console::takeover();
    } else {
        // The kernel's log stays on the wire, but the screens exist: a text
        // console the compositor releases is drawn from here on, and the shells
        // fill in as they speak.
        crate::console::set_grids_live();
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
            DIRTY.load(Ordering::Relaxed)
                || pending()
                || PROBE.load(Ordering::Relaxed)
                || RE_PRESENT.load(Ordering::Relaxed)
                || jobs_pending()
                || RESIZE_TARGET.load(Ordering::Relaxed) != 0
        });
        DIRTY.store(false, Ordering::Relaxed);

        // The beat coalesces *repaints*, not *requests*. A repaint is work
        // nobody asked for, and deferring it by a frame is invisible; a request
        // has a writer parked in `write(2)` until its frame leaves, so pacing it
        // at the beat would make a 4 KiB write cost a frame and a screenful
        // cost a minute - which is what a `/dev/fb0` writer would have paid.
        let now = crate::arch::timer::ticks();
        if !pending() && !jobs_pending() && now.saturating_sub(last_paint) < FRAME_TICKS {
            crate::sched::sleep_ticks(FRAME_TICKS - (now - last_paint));
        }
        last_paint = crate::arch::timer::ticks();

        // Whatever the DRM layer asked the device to do, before anything that
        // paints: a caller parked in `job` is holding a system call open, and the
        // beat above coalesces repaints - work nobody asked for - rather than
        // requests. The jobs are resource commands and touch no scanout, so their
        // place relative to the resize and the probe is a latency question only.
        drain_jobs(&mut device);

        // The host's display changed shape, taken after the beat so a drag's
        // stream of events costs one re-query per frame. This is a *probe*: it
        // learns the new size and announces it, and leaves the scanout alone -
        // nothing here rebuilds the screen. The screen follows only when a
        // compositor, having read the new advertised mode, commits it below.
        if PROBE.swap(false, Ordering::Relaxed) {
            probe_display(&mut device, &view);
        }

        // A modeset committed a size the scanout does not yet drive and parked in
        // `resize_blocking`. Taken before the paint so this frame already uses the
        // new geometry. Rebuild, answer the parked caller, and - if the host
        // accepted - retarget the paint to the new block.
        let target = RESIZE_TARGET.swap(0, Ordering::Relaxed);
        if target != 0 {
            let width = (target >> 32) as u32;
            let height = target as u32;
            let ok = resize_to(&mut device, &view, width, height);
            RESIZE_OK.store(ok, Ordering::Relaxed);
            RESIZE_ACK.store(RESIZE_SEQ.load(Ordering::Relaxed), Ordering::Relaxed);
            let waiters = critical_section::with(|cs| {
                core::mem::take(&mut *RESIZE_WAITERS.borrow(cs).borrow_mut())
            });
            for id in waiters {
                crate::sched::wake(id);
            }
            if ok {
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
        }

        // The screen moved back onto the DRM master's console. Re-show the
        // compositor's committed frame now: the text render filled the scanout
        // while the master was away, and the master re-commits only on its own
        // input, so without this the desktop stays hidden until a mouse move.
        // `re_present_now` copies `current_fb` over the scanout (the kernel's own
        // path, not gated as a compositor's flip would be), and the flush below
        // puts it on the window - the very frame the switch landed on.
        let resent = if RE_PRESENT.swap(false, Ordering::Relaxed) {
            crate::drivers::drm::re_present_now() == 0
        } else {
            false
        };

        // Copy the changed screen's cells under the lock (a fixed memcpy), then
        // paint from the copy with the clock live. The flush is outside both.
        // Two moments have no console to show: a session that holds the card
        // and has its console on the screen in graphics mode owns it, and a
        // compositor boot before anybody has claimed the card at all - the
        // window is promised to the compositor there, so the kernel's log stays
        // on the wire until the seat hands the card over and the first flip
        // lands, and the console takes the screen back the moment its own
        // `^A` wins the negotiation.
        let claimed = crate::drivers::drm::master_held();
        let painted = if !crate::console::vt_is_sink() && !claimed || !crate::vfs::vcmux::painted()
        {
            // Forget which console was last shown: when the screen comes back,
            // whatever is on it belongs to something else, and the whole grid
            // needs redrawing rather than the changed rows.
            last_shown = 0;
            false
        } else {
            let shown = crate::term::vc::active() as usize;
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
                    // A switch shows a different console than the one whose
                    // picture is in the scanout. Paint alone would leave the
                    // region the new grid does not cover - the rows past MAX_ROWS
                    // on a tall screen, the margins where the pixels do not divide
                    // into cells, and above all the band a compositor left behind
                    // - as a stale strip of the old screen. Clear the whole
                    // scanout to the console's paper first, then paint the grid
                    // onto it, so the switch shows the console and not a band.
                    if switched {
                        blit::fill_background(fb, fb_w, fb_h, DEFAULT_BG);
                    }
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
        if painted || resent || req > done {
            let _ = device.flush();
            // The frame has left. This is the moment a page-flip completion is
            // *about*, and it is the only moment in this kernel that honestly
            // means "the pixels are where they were asked to be" - so it is what
            // `drivers::drm` timestamps its flip events with, what a card reader
            // is woken by, and what `FBIO_WAITFORVSYNC` waits for.
            LANDINGS.fetch_add(1, Ordering::Relaxed);
            DONE.store(req, Ordering::Relaxed);
            let waiters =
                critical_section::with(|cs| core::mem::take(&mut *WAITERS.borrow(cs).borrow_mut()));
            for id in waiters {
                crate::sched::wake(id);
            }
            crate::drivers::drm::frame_landed();
        }
    }
}

/// Learn the size the host now wants, and - if it is new - advertise it as the
/// connector's available mode and fire a hotplug uevent.
///
/// This is the entire kernel response to a QEMU window resize. It reads
/// `GET_DISPLAY_INFO` and records the answer as the size the connector offers;
/// it does *not* touch the scanout. A compositor that owns the card sees the
/// uevent, re-probes, finds the new preferred mode and commits it, which is the
/// only thing that rebuilds the screen (see [`resize_to`]). A text console with
/// nobody to commit leaves the scanout at the size it was driven at - Linux's
/// fbcon behaves the same way.
///
/// A zero size means no display is attached (a headless QEMU answers every query
/// with nothing, which is not a request for an empty screen) and is ignored; so
/// is a QEMU window *move*, which sends the event with an unchanged size.
fn probe_display(device: &mut Gpu, view: &Arc<VirtioGpuDev>) {
    let (width, height) = match device.resolution() {
        Ok((width, height)) if width > 0 && height > 0 => (width, height),
        _ => return,
    };
    // `set_advertised` answers whether the size actually moved, so a burst that
    // re-reports the same shape - a drag, a window move - fires no uevent.
    if view.set_advertised(width, height) {
        crate::drivers::drm::display_changed();
        let (cur_w, cur_h) = view.dims();
        crate::println!(
            "virtio-gpu: host wants {width}x{height}; still driving {cur_w}x{cur_h} until a compositor commits it"
        );
    }
}

/// Rebuild the scanout at the size a compositor committed, and answer whether the
/// host took it.
///
/// This is the piece the kernel no longer does on its own: it runs only when a
/// KMS master's modeset asks for a size the scanout does not yet drive, and the
/// target is that committed size rather than something read from the host. The
/// old screen is preserved into the new block (a resize that blanked the window
/// until the compositor repainted would be worse than the corner it is fixing),
/// the console grids follow the new geometry, and a host refusal puts the old
/// size back and reports `false` so the ioctl can answer `EINVAL`.
///
/// Returns `true` when the scanout now drives the requested size (including the
/// no-op case where it already did), `false` when the host refused and the
/// screen was restored.
fn resize_to(device: &mut Gpu, view: &Arc<VirtioGpuDev>, width: u32, height: u32) -> bool {
    let (old_w, old_h) = view.dims();
    if (width, height) == (old_w, old_h) {
        return true;
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
            // The host refused the committed size - and asking tore the old
            // scanout down, so the machine has no screen at all until one is
            // built. Put the size it had back, which at least leaves the machine
            // as it was: the block the old screen lived in is left pinned and
            // unused rather than handed to the allocator while it may still be
            // readable, so the price of a refused resize is one leaked screen.
            // The refused size is taken out of the advertised mode too, so a
            // re-probe can offer what the host will actually do next time.
            let Ok(slice) = device.change_resolution(old_w, old_h) else {
                return false;
            };
            let (fb_len, fb_phys) = adopt(slice, old_w, old_h, old_phys, old_w, old_h, old_len);
            view.resized(old_w, old_h, fb_phys, fb_len);
            view.set_advertised(old_w, old_h);
            crate::println!("virtio-gpu: {width}x{height} refused; staying at {old_w}x{old_h}");
            return false;
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
    // The scanout now drives what was advertised, so the two agree again.
    view.set_advertised(width, height);

    // The console's grid *is* the screen's size, so the grids are rebuilt - at
    // the *new* cell count, computed here rather than passed in, because that is
    // the point - and the boot log replayed onto the one being shown. A resize
    // costs the scrollback, which is the honest price: the other virtual
    // consoles' contents are not kept anywhere but in their own grids. On a
    // compositor boot the grids are resized the same way and the kernel's log is
    // left on the wire, but whoever owns the scanout has just committed this size
    // itself, so it needs no further telling.
    let rows = (height as usize / CELL_H).clamp(1, MAX_ROWS);
    let cols = (width as usize / CELL_W).max(1);
    for vc in 1..=VC_COUNT as usize {
        term::make_screen(vc, rows, cols);
    }
    if crate::console::vt_is_sink() {
        crate::console::takeover();
    }

    // Said out loud, like the boot's own `virtio-gpu: WxH at /dev/fb0` line and
    // for the same reason: the size of the screen is the first thing anyone
    // debugging a display wants to know, and it just changed under them. After
    // the grids are rebuilt, so the line lands on the screen that exists now.
    crate::println!(
        "virtio-gpu: display {old_w}x{old_h} -> {width}x{height}, {} KiB at /dev/fb0",
        fb_len / 1024
    );
    true
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
