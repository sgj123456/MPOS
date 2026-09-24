//! DRM/KMS: a `/dev/dri/card0` a compositor can drive.
//!
//! weston's modern backend - and any DRM/KMS compositor - opens
//! `/dev/dri/card0`, enumerates a single CRTC/encoder/connector/plane, makes
//! CPU-writable *dumb buffers* (GEM), attaches them with `ADDFB`, and shows
//! them with `SETCRTC`/`PAGE_FLIP`, waiting on vblank events read back off
//! the card fd. There is no 3D here and no `GBM`/`EGL`; this is the software
//! (pixman) presentation path, so a compositor that can't find a GL driver
//! still has a scanout it owns rather than sharing `/dev/fb0` with the
//! console.
//!
//! The whole card is virtual and fixed: exactly one of each KMS object, the
//! dumb buffers are page-backed like a memfd (so `MAP_DUMB` + `mmap` aliases
//! the real frames, with the same per-mapping reference discipline so a
//! unmapping process cannot free a buffer another still owns), and a page
//! flip is applied by pointing the framebuffer at the new buffer and queueing
//! one vblank event. The ioctl numbers and struct layouts follow the Linux
//! UAPI exactly (`drm.h`, `drm_mode.h`), which is what libdrm compiles
//! against.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, clone_frame_ref, free_frames};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

/// Fixed KMS object ids - one of each, never changing. libdrm only cares that
/// `GETRESOURCES` and the `GET*` ioctls agree on them.
const CRTC_ID: u32 = 34;
const ENCODER_ID: u32 = 33;
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// `DRM_MODE_CONNECTOR_VIRTUAL` / `DRM_MODE_ENCODER_VIRTUAL`.
const CONNECTOR_TYPE: u32 = 15;
const ENCODER_TYPE: u32 = 5;
/// `DRM_MODE_CONNECTED`.
const CONNECTED: u32 = 1;
/// `DRM_EVENT_FLIP_COMPLETE`.
const EVENT_FLIP_COMPLETE: u32 = 0x8000_0002;

/// The ioctl `nr` byte (bits 8..15) for the KMS + core requests we serve.
const NR_VERSION: u32 = 0x00;
const NR_GET_CAP: u32 = 0x0c;
const NR_SET_CLIENT_CAP: u32 = 0x0d;
const NR_GETRESOURCES: u32 = 0xa0;
const NR_GETCRTC: u32 = 0xa1;
const NR_SETCRTC: u32 = 0xa2;
const NR_GETENCODER: u32 = 0xa6;
const NR_GETCONNECTOR: u32 = 0xa7;
const NR_GETPROPERTY: u32 = 0xaa;
const NR_SETPROPERTY: u32 = 0xab;
const NR_GETPROPBLOB: u32 = 0xac;
const NR_GETFB: u32 = 0xad;
const NR_ADDFB: u32 = 0xae;
const NR_RMFB: u32 = 0xaf;
const NR_PAGE_FLIP: u32 = 0xb0;
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_MAP_DUMB: u32 = 0xb3;
const NR_DESTROY_DUMB: u32 = 0xb4;
const NR_GETPLANE: u32 = 0xb6;
const NR_GETPLANERESOURCES: u32 = 0xb5;

/// `DRM_CAP_DUMB_BUFFER`; `GET_CAP` answers 1 for it and 0 elsewhere.
const CAP_DUMB_BUFFER: u64 = 0x1;

/// A dumb buffer's pages are found in the mmap address space at
/// `handle * PAGES_PER_BUFFER * PAGE`, so `mmap_frame` can decode the buffer
/// back out of an absolute page index. 65536 pages (256 MiB) per slot is
/// comfortably above any single frame.
const PAGES_PER_BUFFER: usize = 1 << 16;
/// Cap on buffers and framebuffers the card will track.
const MAX_HANDLES: usize = 64;

/// One dumb buffer: its geometry and the physical pages behind it.
struct Dumb {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    /// One physical frame per page of the buffer, grown on the first write or
    /// map that touches it (the inode holds a reference on each; every process
    /// that maps it takes its own via `clone_frame_ref`).
    frames: Vec<usize>,
    // The card holds one reference per frame until DESTROY_DUMB; frames are
    // released then, and only actually recycled once no live mapping still
    // references them (each mapping took its own via clone_frame_ref).
}

impl Dumb {
    fn new(width: u32, height: u32, bpp: u32) -> Dumb {
        let pitch = (width * bpp / 8).next_multiple_of(64);
        Dumb {
            width,
            height,
            pitch,
            bpp,
            frames: Vec::new(),
        }
    }
    fn size(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
    /// Grow to cover page `idx`, returning its frame (0 if the cap or memory
    /// is hit). Allocates outside any borrow of the global card state? no -
    /// callers hold it; allocation itself takes no card lock.
    fn ensure_page(&mut self, idx: usize) -> usize {
        if idx >= PAGES_PER_BUFFER {
            return 0;
        }
        while self.frames.len() <= idx {
            match crate::mm::alloc_frames(0) {
                Some(p) => {
                    unsafe {
                        core::ptr::write_bytes(crate::mm::phys_to_virt(p) as *mut u8, 0, PAGE_SIZE);
                    }
                    self.frames.push(p);
                }
                None => return 0,
            }
        }
        self.frames[idx]
    }
    fn release(&mut self) {
        for p in self.frames.drain(..) {
            if p != 0 {
                free_frames(p, 0);
            }
        }
    }
}

/// A framebuffer: `ADDFB`/`ADDFB2` bound a dumb buffer to an id a CRTC can
/// display.
struct Framebuffer {
    id: u32,
    handle: u32,
    width: u32,
    height: u32,
}

/// The card's mutable state, all under one lock. There is a single device, so
/// a process opening `card0` shares it with every other opener - which is
/// fine, because exactly one compositor owns the seat.
struct DrmState {
    dumb: Vec<Option<Dumb>>,
    fbs: Vec<Option<Framebuffer>>,
    /// The framebuffer currently driven to the scanout by the CRTC.
    current_fb: u32,
    /// Sequence number for flip/vblank events, echoed back to the reader.
    seq: u32,
    /// Queued `drmEvent` objects awaiting `read` on the card fd: (type,
    /// payload-u64-words). Only flip-completion is produced.
    events: VecDeque<drm_event::Vblank>,
}

mod drm_event {
    /// `struct drm_event { u32 type; u32 length; }` then a `drmVblank`
    /// payload: sequence, reserved, time (u64), vblank, plus the crtc id for
    /// `DRM_CAP_CRTC_IN_VBLANK_EVENT`. We carry sequence + time.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Vblank {
        pub type_: u32,
        pub length: u32,
        pub sequence: u32,
        pub reserved: u32,
        pub time_sec: u32,
        pub time_usec: u32,
        pub vblank: u32,
        pub crtc: u32,
    }
}

use alloc::collections::VecDeque;

static STATE: Mutex<RefCell<DrmState>> = Mutex::new(RefCell::new(DrmState {
    dumb: Vec::new(),
    fbs: Vec::new(),
    current_fb: 0,
    seq: 0,
    events: VecDeque::new(),
}));

/// A fresh handle for a dumb buffer or framebuffer id, from the table sizes.
fn next_handle(slot: &mut Vec<Option<()>>) -> usize {
    if slot.len() < MAX_HANDLES {
        let id = slot.len() + 1;
        slot.push(None);
        id
    } else {
        0
    }
}

/// The `drm_version` answer: this is not a real driver, so report the KMS ABI
/// we speak (4.0) and the name "mpos".
fn fill_version(buf: usize) -> isize {
    // struct drm_version: 3 ints, then name_len/ptr, date_len/ptr, desc_len/ptr.
    unsafe {
        let w = |off: usize, v: i32| ((buf + off) as *mut i32).write_unaligned(v);
        w(0, 4); // major
        w(4, 0); // minor
        w(8, 0); // patchlevel
        // name_len at 16 (after align to 8), ptr at 24; date/desc follow.
        let name = b"mpos";
        let sz = (buf + 16) as *mut usize;
        if sz.read_unaligned() >= name.len() {
            let dst = (buf + 24) as *mut u8; // name pointer
            if !dst.is_null() {
                core::ptr::copy_nonoverlapping(name.as_ptr(), dst, name.len());
            }
            sz.write_unaligned(name.len());
        } else {
            sz.write_unaligned(name.len());
            return -libc_EINVAL();
        }
        // date_len at 32 = 0, desc_len at 48 = 0 (no date/desc strings).
        ((buf + 32) as *mut usize).write_unaligned(0);
        ((buf + 48) as *mut usize).write_unaligned(0);
    }
    0
}

fn libc_EINVAL() -> isize {
    -22
}

/// Serve a DRM ioctl on the card. `request`'s `nr` byte selects the request;
/// `arg` is the user pointer to the request's struct. Every struct is copied
/// in whole, filled per the fixed topology, and (for the in-out KMS structs)
/// copied back - matching how libdrm passes them.
pub fn drm_ioctl(fd: i32, request: u32, arg: usize) -> isize {
    let nr = (request >> 8) & 0xff;
    // Most requests need a readable file that is the card.
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(_) => return -9, // EBADF
    };
    if file.inode().as_drm().is_none() {
        return -25; // ENOTTY
    }
    if arg == 0 {
        return -14; // EFAULT
    }
    match nr {
        NR_VERSION => fill_version(arg),
        NR_GET_CAP => {
            // struct drm_get_cap { u64 capability; u64 value; }
            let cap = unsafe { (arg as *const u64).read_unaligned() };
            let value = if cap == CAP_DUMB_BUFFER { 1 } else { 0 };
            unsafe { ((arg + 8) as *mut u64).write_unaligned(value) };
            0
        }
        NR_SET_CLIENT_CAP => 0, // we advertise the base feature set only
        NR_GETRESOURCES => get_resources(arg),
        NR_GETCRTC => get_crtc(arg),
        NR_SETCRTC => set_crtc(arg),
        NR_GETENCODER => get_encoder(arg),
        NR_GETCONNECTOR => get_connector(arg),
        NR_GETPLANERESOURCES => {
            // struct drm_mode_get_plane_res { u32 count_planes; u32 *plane_ids; }
            unsafe {
                let cnt = (arg as *mut u32).read_unaligned();
                let ids = ((arg + 8) as *const u32).read_unaligned() as *mut u32;
                if cnt >= 1 && !ids.is_null() {
                    ids.write_unaligned(PLANE_ID);
                }
                (arg as *mut u32).write_unaligned(1);
            }
            0
        }
        NR_GETPLANE => {
            // Minimal: report the single plane bound to the one CRTC.
            unsafe {
                let p = arg as *mut u32;
                p.write_unaligned(PLANE_ID);
                p.add(1).write_unaligned(CRTC_ID);
            }
            0
        }
        NR_GETPROPERTY => {
            // No properties to enumerate: an empty name, zero values.
            unsafe {
                let p = arg as *mut u8;
                // prop_id at 16, flags at 20, name[32] at 24, counts at 56/60.
                ((p as *mut u32).add(14)).write_unaligned(0); // count_values
                ((p as *mut u32).add(15)).write_unaligned(0); // count_enum_blobs
            }
            0
        }
        NR_SETPROPERTY | NR_GETPROPBLOB | NR_GETFB => 0,
        NR_ADDFB => add_fb(arg),
        NR_RMFB => {
            let id = unsafe { (arg as *const u32).read_unaligned() };
            critical_section::with(|cs| {
                for fb in STATE.borrow(cs).borrow_mut().fbs.iter_mut() {
                    if fb.as_ref().is_some_and(|f| f.id == id) {
                        *fb = None;
                    }
                }
            });
            0
        }
        NR_PAGE_FLIP => page_flip(arg),
        NR_CREATE_DUMB => create_dumb(arg),
        NR_MAP_DUMB => map_dumb(arg),
        NR_DESTROY_DUMB => {
            let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
            if handle >= 1 {
                critical_section::with(|cs| {
                    let mut st = STATE.borrow(cs).borrow_mut();
                    if let Some(slot) = st.dumb.get_mut(handle - 1) {
                        if let Some(d) = slot.as_mut() {
                            d.release();
                        }
                        *slot = None;
                    }
                });
            }
            0
        }
        _ => -25, // ENOTTY
    }
}

fn get_resources(arg: usize) -> isize {
    // struct drm_mode_card_res: fb@0,crtc@8,conn@16,enc@24 (u64 ptrs);
    // count_fbs@32, crtcs@36, conns@40, encoders@44, widths/heights@48..
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let n_fbs = st.fbs.iter().filter(|f| f.is_some()).count() as u32;
        unsafe {
            let put_ids = |ptr_off: usize, count_off: usize, ids: &[u32]| {
                let ptr = ((arg + ptr_off) as *const u64).read_unaligned() as *mut u32;
                let want = ((arg + count_off) as *const u32).read_unaligned();
                if want >= ids.len() as u32 && !ptr.is_null() {
                    for (i, id) in ids.iter().enumerate() {
                        ptr.add(i).write_unaligned(*id);
                    }
                }
                ((arg + count_off) as *mut u32).write_unaligned(ids.len() as u32);
            };
            // count_fbs is set by the caller to its buffer size; the kernel
            // reports how many there are and fills fb ids when space allows.
            let fbs: Vec<u32> = st
                .fbs
                .iter()
                .filter_map(|f| f.as_ref().map(|f| f.id))
                .collect();
            let c = ((arg + 32) as *const u32).read_unaligned();
            if c >= fbs.len() as u32 {
                let p = ((arg) as *const u64).read_unaligned() as *mut u32;
                if !p.is_null() {
                    for (i, id) in fbs.iter().enumerate() {
                        p.add(i).write_unaligned(*id);
                    }
                }
            }
            ((arg + 32) as *mut u32).write_unaligned(fbs.len() as u32);
            put_ids(8, 36, &[CRTC_ID]);
            put_ids(16, 40, &[CONNECTOR_ID]);
            put_ids(24, 44, &[ENCODER_ID]);
            // min/max size: the modes we offer.
            ((arg + 48) as *mut u32).write_unaligned(0); // min_width
            ((arg + 52) as *mut u32).write_unaligned(4096);
            ((arg + 56) as *mut u32).write_unaligned(0);
            ((arg + 60) as *mut u32).write_unaligned(4096);
        }
    });
    0
}

/// Write a `drm_mode_modeinfo` describing the current scanout at `mode_off`
/// within the crtc struct (mode is at offset 36 in `drm_mode_crtc`).
fn write_mode(buf: *mut u8, clock_khz: u32, w: u32, h: u32) {
    unsafe {
        let w32 = |off: usize, v: u32| ((buf.add(off)) as *mut u32).write_unaligned(v);
        let w16 = |off: usize, v: u16| ((buf.add(off)) as *mut u16).write_unaligned(v);
        w32(0, clock_khz); // clock
        w16(4, w as u16); // hdisplay
        w16(6, w as u16); // hsync_start
        w16(8, w as u16); // hsync_end
        w16(10, w as u16); // htotal
        w16(12, 0); // hskew
        w16(14, h as u16); // vdisplay
        w16(16, h as u16); // vsync_start
        w16(18, h as u16); // vsync_end
        w16(20, h as u16); // vtotal
        w16(22, 0); // vscan
        w32(24, 60); // vrefresh
        w32(28, 0); // flags
        w32(32, 0x08 | 0x40); // type: PREFERRED|DRIVER
    }
}

fn crtc_mode_dims() -> (u32, u32) {
    // The scanout geometry, if the GPU is present; else a fallback.
    match crate::drivers::virtio::gpu::global() {
        Some(gpu) => gpu.dims(),
        None => (1024, 768),
    }
}

fn get_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: set_connectors_ptr@0,count_connectors@8,
    // crtc_id@12, fb_id@16, x@20, y@24, gamma_size@28, mode_valid@32, mode@36.
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let (w, h) = crtc_mode_dims();
        unsafe {
            let p = arg as *mut u8;
            (p.add(12) as *mut u32).write_unaligned(CRTC_ID);
            (p.add(16) as *mut u32).write_unaligned(st.current_fb);
            (p.add(32) as *mut u32).write_unaligned(1); // mode_valid
            write_mode(p.add(36), (w * h * 60) / 1000, w, h);
            // one preferred mode
            ((p.add(36) as *mut u32).add(8)).write_unaligned(1); // count_modes not in this struct; ignore
        }
    });
    0
}

fn set_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: crtc_id@12, fb_id@16, x@20,y@24, ...
    let crtc_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return -22; // EINVAL
    }
    critical_section::with(|cs| {
        STATE.borrow(cs).borrow_mut().current_fb = fb_id;
    });
    // Present: flush the scanout if a render engine is running.
    crate::drivers::video::render::request_flush_blocking();
    0
}

fn get_encoder(arg: usize) -> isize {
    // struct drm_mode_get_encoder: encoder_id@0, encoder_type@4, crtc_id@8,
    // possible_crtcs@12, possible_clones@16.
    unsafe {
        let p = arg as *mut u32;
        let want = p.read_unaligned();
        if want != 0 && want != ENCODER_ID {
            return -22;
        }
        p.write_unaligned(ENCODER_ID);
        p.add(1).write_unaligned(ENCODER_TYPE);
        p.add(2).write_unaligned(CRTC_ID);
        p.add(3).write_unaligned(1); // possible_crtcs
        p.add(4).write_unaligned(0);
    }
    0
}

fn get_connector(arg: usize) -> isize {
    // struct drm_mode_get_connector:
    // encoders_ptr@0, modes_ptr@8, props_ptr@16, prop_values_ptr@24 (u64);
    // count_modes@32, count_props@36, count_encoders@40, encoder_id@44,
    // connector_id@48, connector_type@52, connector_type_id@56, connection@60,
    // mm_width@64, mm_height@68, subpixel@72, pad@76.
    let want = unsafe { ((arg + 48) as *const u32).read_unaligned() };
    if want != 0 && want != CONNECTOR_ID {
        return -22;
    }
    let (w, h) = crtc_mode_dims();
    unsafe {
        let p = arg as *mut u8;
        // Fill a single mode into modes_ptr if the caller asked for it.
        let count_modes = ((p.add(32) as *const u32).read_unaligned());
        let modes_ptr = ((p.add(8) as *const u64).read_unaligned()) as *mut u8;
        if count_modes >= 1 && !modes_ptr.is_null() {
            write_mode(modes_ptr, (w * h * 60) / 1000, w, h);
        }
        (p.add(32) as *mut u32).write_unaligned(1); // count_modes
        (p.add(36) as *mut u32).write_unaligned(0); // count_props
        (p.add(40) as *mut u32).write_unaligned(1); // count_encoders
        let enc_ptr = ((p as *const u64).read_unaligned()) as *mut u32;
        if !enc_ptr.is_null() {
            enc_ptr.write_unaligned(ENCODER_ID);
        }
        (p.add(44) as *mut u32).write_unaligned(ENCODER_ID); // encoder_id
        (p.add(48) as *mut u32).write_unaligned(CONNECTOR_ID);
        (p.add(52) as *mut u32).write_unaligned(CONNECTOR_TYPE);
        (p.add(56) as *mut u32).write_unaligned(1); // connector_type_id
        (p.add(60) as *mut u32).write_unaligned(CONNECTED); // connection
        (p.add(64) as *mut u32).write_unaligned(0); // mm_width
        (p.add(68) as *mut u32).write_unaligned(0);
    }
    0
}

fn add_fb(arg: usize) -> isize {
    // struct drm_mode_fb_cmd: fb_id@0,width@4,height@8,pitch@12,bpp@16,
    // depth@20,handle@24.
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    let id = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let fb_id = st.fbs.len() as u32 + 100;
        st.fbs.push(Some(Framebuffer {
            id: fb_id,
            handle,
            width,
            height,
        }));
        fb_id
    });
    unsafe { (arg as *mut u32).write_unaligned(id) };
    0
}

fn page_flip(arg: usize) -> isize {
    // struct drm_mode_crtc_page_flip: crtc_id@0, fb_id@4, flags@8, reserved@12,
    // user_data@16.
    let crtc_id = unsafe { (arg as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return -22;
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.current_fb = fb_id;
        st.seq = st.seq.wrapping_add(1);
        let seq = st.seq;
        // DRM_MODE_PAGE_FLIP_EVENT (0x1) asks for the completion event.
        let flags = unsafe { ((arg + 8) as *const u32).read_unaligned() };
        if flags & 0x1 != 0 {
            st.events.push_back(drm_event::Vblank {
                type_: EVENT_FLIP_COMPLETE,
                length: 32,
                sequence: seq,
                reserved: 0,
                time_sec: crate::arch::rtc::boot_epoch() as u32,
                time_usec: 0,
                vblank: 1,
                crtc: CRTC_ID,
            });
        }
    });
    crate::drivers::video::render::request_flush_blocking();
    0
}

fn create_dumb(arg: usize) -> isize {
    // struct drm_mode_create_dumb: height@0,width@4,bpp@8,flags@12,
    // handle@16,pitch@20,size@24(u64).
    let height = unsafe { (arg as *const u32).read_unaligned() };
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    if bpp == 0 || width == 0 || height == 0 {
        return -22;
    }
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let idx = st.dumb.len();
        if idx >= MAX_HANDLES {
            return 0usize;
        }
        st.dumb.push(Some(Dumb::new(width, height, bpp)));
        idx + 1
    });
    if handle == 0 {
        return -28; // ENOSPC
    }
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let d = st.dumb[handle - 1].as_ref().unwrap();
        unsafe {
            ((arg + 16) as *mut u32).write_unaligned(handle as u32);
            ((arg + 20) as *mut u32).write_unaligned(d.pitch);
            ((arg + 24) as *mut u64).write_unaligned(d.size());
        }
    });
    0
}

fn map_dumb(arg: usize) -> isize {
    // struct drm_mode_map_dumb: handle@0, pad@4, offset@8(u64).
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    if handle == 0 {
        return -22;
    }
    // Encode the buffer's slot into the mmap offset so `mmap_frame` can find
    // it from the absolute page index the kernel's mmap hands it.
    let offset = (handle as u64) * (PAGES_PER_BUFFER as u64) * (PAGE_SIZE as u64);
    unsafe { ((arg + 8) as *mut u64).write_unaligned(offset) };
    0
}

/// The `/dev/dri/card0` node. Bytes read out of it are DRM events (flip
/// completions); the buffer memory itself is reached by `mmap`, not `read`.
pub struct DrmCard {
    ino: u64,
}

impl DrmCard {
    pub fn new(ino: u64) -> Arc<DrmCard> {
        Arc::new(DrmCard { ino })
    }

    /// Whether a `read` would return an event now.
    fn has_event() -> bool {
        critical_section::with(|cs| !STATE.borrow(cs).borrow().events.is_empty())
    }
}

impl Inode for DrmCard {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// Serve the next queued DRM event, as `drmGetEventsReady`/a reader of the
    /// card fd expects: a `drm_event` header then the vblank payload. A
    /// blocking read is left to the caller's poll loop (this node's readiness
    /// is polled at tick granularity like other event nodes).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let ev = critical_section::with(|cs| STATE.borrow(cs).borrow_mut().events.pop_front());
        match ev {
            Some(ev) => {
                let bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &ev as *const _ as *const u8,
                        core::mem::size_of::<drm_event::Vblank>(),
                    )
                };
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            None => Ok(0),
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        // Writes to the card are ioctls' job; the file face is read-only.
        Ok(0)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: Self::has_event(),
            write: true,
        }
    }

    /// Map a dumb buffer's page. The buffer is chosen from the absolute page
    /// index (which carries the `MAP_DUMB` offset), and the caller is given
    /// its own reference on the frame so its later teardown cannot free a
    /// buffer another process still maps - the same discipline as the memfd.
    fn mmap_frame(&self, index: u64) -> Option<usize> {
        let index = index as usize;
        let handle = index / PAGES_PER_BUFFER;
        let local = index % PAGES_PER_BUFFER;
        if handle == 0 {
            return None;
        }
        let frame = critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            match st.dumb.get_mut(handle - 1).and_then(|slot| slot.as_mut()) {
                Some(d) => d.ensure_page(local),
                None => 0,
            }
        });
        if frame == 0 {
            return None;
        }
        clone_frame_ref(frame);
        Some(frame)
    }

    fn as_drm(&self) -> Option<&DrmCard> {
        Some(self)
    }
}
