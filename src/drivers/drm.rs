//! DRM/KMS: a `/dev/dri/card0` a compositor can drive.
//!
//! weston's modern backend - and any DRM/KMS compositor - opens
//! `/dev/dri/card0`, enumerates a single CRTC/encoder/connector/plane, makes
//! CPU-writable *dumb buffers* (GEM), attaches them with `ADDFB`, and shows
//! them with `SETCRTC`/`PAGE_FLIP`, waiting on vblank events read back off
//! the card fd. There is no 3D here; GBM and its GL clients reach these same
//! dumb buffers from the other side, as PRIME dma-bufs (`drm.rs` exports a
//! handle to a descriptor, Mesa maps the descriptor, renders into it with
//! llvmpipe, and the compositor imports the fb back to scan out).
//!
//! The whole card is virtual and fixed: exactly one of each KMS object, the
//! dumb buffers are page-backed like a memfd (so `MAP_DUMB` + `mmap` aliases
//! the real frames, with the same per-mapping reference discipline so a
//! unmapping process cannot free a buffer another still owns). A page flip
//! is applied by *copying the framebuffer into the GPU's scanout buffer* -
//! row by row, because the dumb buffer's pitch is 64-byte aligned while the
//! scanout's is `width * 4` - and then asking the render task to flush, so
//! what the compositor drew actually reaches the host window. The ioctl
//! numbers and struct layouts follow the Linux UAPI exactly (`drm.h`,
//! `drm_mode.h`), which is what libdrm compiles against.
//!
//! `/dev/dri/renderD128` is the same card state under the render node's
//! name: GBM opens the render node to allocate (a client is not the master,
//! so libdrm's convention is to keep allocation off `card0`), and a teaching
//! kernel has no permission boundary to draw between the two. Both nodes
//! ioctl one shared `DrmState`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::mm::{PAGE_SIZE, clone_frame_ref, free_frames, phys_to_virt};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};

/// Fixed KMS object ids - one of each, never changing. libdrm only cares that
/// `GETRESOURCES` and the `GET*` ioctls agree on them.
const CRTC_ID: u32 = 34;
const ENCODER_ID: u32 = 33;
const CONNECTOR_ID: u32 = 35;
const PLANE_ID: u32 = 36;
/// Fixed property ids, allocated the same way: `OBJ_GETPROPERTIES` and
/// `GETPROPERTY` have to agree on them, and nothing else cares what they are.
/// `CRTC_ID` and `DPMS` live on the connector, `ACTIVE` on the CRTC, and
/// `type` on the plane - the set weston's DRM backend looks every one of
/// them up by *name* for.
const PROP_CRTC_ID: u32 = 70;
const PROP_DPMS: u32 = 71;
const PROP_ACTIVE: u32 = 72;
const PROP_TYPE: u32 = 73;
/// `DRM_MODE_CONNECTOR_VIRTUAL` / `DRM_MODE_ENCODER_VIRTUAL`.
const CONNECTOR_TYPE: u32 = 15;
const ENCODER_TYPE: u32 = 5;
/// `DRM_MODE_CONNECTED`.
const CONNECTED: u32 = 1;
/// `DRM_EVENT_FLIP_COMPLETE`.
const EVENT_FLIP_COMPLETE: u32 = 0x8000_0002;

/// The ioctl `nr` byte (bits 8..15) for the KMS + core requests we serve.
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
const NR_GETPLANERESOURCES: u32 = 0xb5;
const NR_GETPLANE: u32 = 0xb6;
/// `DRM_IOCTL_MODE_ADDFB2` / `..._OBJ_GETPROPERTIES` / `..._OBJ_SETPROPERTY`:
/// the fourcc form of ADDFB and the object-typed property interface.
const NR_ADDFB2: u32 = 0xb8;
const NR_OBJ_GETPROPERTIES: u32 = 0xb9;
const NR_OBJ_SETPROPERTY: u32 = 0xba;
/// The core (non-KMS) requests: version, auth, and the PRIME pair that hands
/// a GEM handle to a dma-buf descriptor and back. `SET_VERSION` and the
/// magic/auth pair are what libdrm's setup probes; PRIME is what GBM builds
/// its buffers through.
const NR_VERSION: u32 = 0x00;
const NR_GET_MAGIC: u32 = 0x02;
const NR_SET_VERSION: u32 = 0x07;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_AUTH_MAGIC: u32 = 0x11;
const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;
/// Session-ownership requests - `DRM_IOCTL_AUTH` / `_SET_MASTER` /
/// `_DROP_MASTER`. They are `_IO` (no argument), so `arg` is null for them
/// and they must be answered before the read-buffer check in `drm_ioctl`. A
/// seat manager reaches them through libdrm when it hands the card to a
/// compositor. mpos has one card and whoever opened it is its master, so
/// all three succeed by doing nothing - refusing `SET_MASTER` is what stops
/// weston from ever getting a master fd it can set a mode on.
const NR_AUTH: u32 = 0x1c;
const NR_SET_MASTER: u32 = 0x1e;
const NR_DROP_MASTER: u32 = 0x1f;

/// `DRM_CAP_DUMB_BUFFER`; `GET_CAP` answers 1 for it and 0 elsewhere.
const CAP_DUMB_BUFFER: u64 = 0x1;
/// `DRM_CAP_PRIME`: both directions of PRIME work here (the dma-buf of
/// `prime_handle_to_fd`), which is what tells Mesa's GBM that the render
/// node can hand buffers to the card.
const CAP_PRIME: u64 = 0x5;
/// `DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT`.
const PRIME_CAPS: u64 = 0x3;
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES`: the one client capability this card
/// actually has. The refusal of the others is load-bearing - `ATOMIC`
/// especially: a cap accepted blindly would tell weston to speak a
/// mode-setting protocol this card does not implement.
const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
/// `DRM_CLOEXEC == O_CLOEXEC` on Linux: mark the handed-out descriptor the
/// way `open(O_CLOEXEC)` would.
const DRM_CLOEXEC: u32 = 0x80000;
/// The magic `GET_MAGIC` hands out. Any value is self-consistent on a card
/// whose master is whoever opened it; what matters is that the pair of calls
/// exists for libdrm's setup to make.
const MAGIC: u32 = u32::from_le_bytes(*b"MPOS");

/// `DRM_MODE_OBJECT_CRTC` / `_CONNECTOR` / `_PLANE`: the object-type tags of
/// `OBJ_GETPROPERTIES`/`OBJ_SETPROPERTY`.
const OBJECT_CRTC: u32 = 0xcccc_cccc;
const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const OBJECT_PLANE: u32 = 0xeeee_eeee;
/// `DRM_MODE_PROP_RANGE` / `_ENUM` / `_OBJECT` (the last is `TYPE(1)`,
/// i.e. bit 6), and `_IMMUTABLE` for the one property userspace only reads.
const PROP_RANGE: u32 = 1 << 1;
const PROP_ENUM: u32 = 1 << 3;
const PROP_OBJECT: u32 = 1 << 6;
const PROP_IMMUTABLE: u32 = 1 << 2;
/// `DRM_PLANE_TYPE_PRIMARY`: the one plane is the CRTC's primary surface.
const PLANE_TYPE_PRIMARY: u64 = 1;

/// The 32-bit fourccs `ADDFB2` accepts - every BGRA-order spelling of a
/// 32-bit pixel this scanout can show. `fourcc_code('X','R','2','4')` and
/// friends, little-endian.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_ARGB8888: u32 = 0x3432_4152;
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4142;

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
                        core::ptr::write_bytes(phys_to_virt(p) as *mut u8, 0, PAGE_SIZE);
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
    /// Copy `len` bytes of the buffer at byte `src_off` into `dst`. The pages
    /// are read through the direct map; a page the buffer never grew reads as
    /// zeros, which is what an untouched dumb page shows on Linux.
    fn copy_span(&self, src_off: usize, dst: *mut u8, len: usize) {
        let mut done = 0;
        while done < len {
            let pos = src_off + done;
            let off = pos % PAGE_SIZE;
            let chunk = (PAGE_SIZE - off).min(len - done);
            match self.frames.get(pos / PAGE_SIZE) {
                Some(&frame) if frame != 0 => unsafe {
                    core::ptr::copy_nonoverlapping(
                        (phys_to_virt(frame) + off) as *const u8,
                        dst.add(done),
                        chunk,
                    );
                },
                _ => unsafe {
                    core::ptr::write_bytes(dst.add(done), 0, chunk);
                },
            }
            done += chunk;
        }
    }
}

/// A framebuffer: `ADDFB`/`ADDFB2` bound a dumb buffer to an id a CRTC can
/// display. The pitch is the caller's (what `GETFB` reports back); the
/// presentation copy reads the dumb buffer's own geometry.
struct Framebuffer {
    id: u32,
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
}

/// The identity record of one dma-buf this card has been involved with: the
/// buffer's ino, the GEM handle the relationship runs through, and - for a
/// buffer this card *exported* - the object itself, so a second export of
/// the same handle answers the same dma-buf rather than a second alias. A
/// record dies with its handle (`GEM_CLOSE`/`DESTROY_DUMB` prune it), which
/// is what bounds the table by the handle table; the import half keeps only
/// the (ino, handle) pair, because the descriptor's own lifetime owns the
/// buffer there.
struct PrimeRecord {
    ino: u64,
    handle: u32,
    buf: Option<Arc<crate::vfs::dmabuf::DmaBuf>>,
}

/// The card's mutable state, all under one lock. There is a single device, so
/// a process opening `card0` shares it with every other opener - which is
/// fine, because exactly one compositor owns the seat.
struct DrmState {
    dumb: Vec<Option<Dumb>>,
    fbs: Vec<Option<Framebuffer>>,
    /// The framebuffer currently driven to the scanout by the CRTC.
    current_fb: u32,
    /// The connector's `CRTC_ID` property value and its `DPMS` mode, and the
    /// CRTC's `ACTIVE` - recorded by `SETPROPERTY`, reported by
    /// `GETCONNECTOR`/`OBJ_GETPROPERTIES`. Presentation itself follows
    /// `current_fb`, not these words: they are the state a compositor reads
    /// back, not a second display path.
    conn_crtc: u32,
    dpms: u32,
    crtc_active: bool,
    /// The PRIME identity table (see [`PrimeRecord`]).
    prime: Vec<PrimeRecord>,
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

static STATE: Mutex<RefCell<DrmState>> = Mutex::new(RefCell::new(DrmState {
    dumb: Vec::new(),
    fbs: Vec::new(),
    current_fb: 0,
    conn_crtc: CRTC_ID,
    dpms: 0,
    crtc_active: true,
    prime: Vec::new(),
    seq: 0,
    events: VecDeque::new(),
}));

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
            return libc_EINVAL();
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

/// Serve a DRM ioctl on the card. `nr` selects the request - the syscall
/// layer has already decoded it out of the request word, whichever half of
/// it carried the command number; `arg` is the user pointer to the request's
/// struct. Every struct is copied in whole, filled per the fixed topology,
/// and (for the in-out KMS structs) copied back - matching how libdrm passes
/// them.
pub fn drm_ioctl(fd: i32, nr: u32, arg: usize) -> isize {
    // Most requests need a readable file that is the card.
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(_) => return -9, // EBADF
    };
    if file.inode().as_drm().is_none() {
        return -25; // ENOTTY
    }
    // Master/auth requests carry no argument, so answer them before the
    // `arg == 0` fault check below would reject a legitimate no-buffer call.
    if matches!(nr, NR_AUTH | NR_SET_MASTER | NR_DROP_MASTER) {
        return 0;
    }
    if arg == 0 {
        return -14; // EFAULT
    }
    match nr {
        NR_VERSION => fill_version(arg),
        NR_GET_CAP => get_cap(arg),
        NR_SET_CLIENT_CAP => set_client_cap(arg),
        NR_GET_MAGIC => {
            // struct drm_auth { drm_magic_t magic; } - the token `AUTH_MAGIC`
            // accepts below. One master, so the token names nothing.
            unsafe { (arg as *mut u32).write_unaligned(MAGIC) };
            0
        }
        NR_AUTH_MAGIC => 0, // the only master authenticates its own magic
        NR_SET_VERSION => set_version(arg),
        NR_GEM_CLOSE | NR_DESTROY_DUMB => {
            // struct drm_gem_close / drm_mode_destroy_dumb: handle@0. The
            // two spell one release - a GEM handle and a dumb buffer are the
            // same object here, and both names may close it.
            let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
            destroy_handle(handle)
        }
        NR_PRIME_HANDLE_TO_FD => prime_handle_to_fd(arg),
        NR_PRIME_FD_TO_HANDLE => prime_fd_to_handle(arg),
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
        NR_GETPLANE => get_plane(arg),
        NR_GETPROPERTY => get_property(arg),
        NR_SETPROPERTY => set_property(arg),
        NR_OBJ_GETPROPERTIES => obj_get_properties(arg),
        NR_OBJ_SETPROPERTY => obj_set_property(arg),
        // No blobs exist to read (no EDID): the caller's length stays 0,
        // which libdrm reports as "property absent".
        NR_GETPROPBLOB => 0,
        NR_GETFB => get_fb(arg),
        NR_ADDFB => add_fb(arg),
        NR_ADDFB2 => add_fb2(arg),
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
        _ => -25, // ENOTTY
    }
}

/// `GET_CAP`: the two caps this card has, and 0 elsewhere.
fn get_cap(arg: usize) -> isize {
    // struct drm_get_cap { u64 capability; u64 value; }
    let cap = unsafe { (arg as *const u64).read_unaligned() };
    let value = match cap {
        CAP_DUMB_BUFFER => 1,
        CAP_PRIME => PRIME_CAPS,
        _ => 0,
    };
    unsafe { ((arg + 8) as *mut u64).write_unaligned(value) };
    0
}

/// `SET_CLIENT_CAP`: accept the one cap the card speaks, refuse the rest
/// with `EOPNOTSUPP` the way Linux answers a cap the driver lacks. The
/// refusal - not the acceptance - is what keeps weston on the legacy
/// mode-setting path it can actually drive (an `ATOMIC` accepted blindly
/// would send it into ioctls this card does not have).
fn set_client_cap(arg: usize) -> isize {
    // struct drm_set_client_cap { u64 capability; u64 value; }
    let capability = unsafe { (arg as *const u64).read_unaligned() };
    let value = unsafe { ((arg + 8) as *const u64).read_unaligned() };
    if capability == CLIENT_CAP_UNIVERSAL_PLANES && value == 1 {
        0
    } else {
        -95 // EOPNOTSUPP
    }
}

/// `SET_VERSION`: the caller names the DRM *ioctl interface* it wants; the
/// reply reports the interface in force and this driver's own version,
/// 1.1.0 - the same version `fill_version` answers by name.
fn set_version(arg: usize) -> isize {
    // struct drm_set_version { int drm_di_major@0; int drm_di_minor@4;
    // int drm_dd_major@8; int drm_dd_minor@12; }
    unsafe {
        let di_major = (arg as *const i32).read_unaligned();
        let di_minor = ((arg + 4) as *const i32).read_unaligned();
        // A 0/0 (or negative) request asks for the canonical version, and
        // the KMS interface is where this card stops; a request above 1.4 is
        // refused the way newer userland is on an older kernel.
        let (major, minor) = if di_major >= 0 && (di_major > 0 || di_minor > 0) {
            if di_major > 1 || di_minor > 4 {
                return libc_EINVAL();
            }
            (di_major, di_minor)
        } else {
            (1, 4)
        };
        (arg as *mut i32).write_unaligned(major); // drm_di_major
        ((arg + 4) as *mut i32).write_unaligned(minor); // drm_di_minor
        ((arg + 8) as *mut i32).write_unaligned(1); // drm_dd_major
        ((arg + 12) as *mut i32).write_unaligned(1); // drm_dd_minor
    }
    0
}

fn get_resources(arg: usize) -> isize {
    // struct drm_mode_card_res: fb@0,crtc@8,conn@16,enc@24 (u64 ptrs);
    // count_fbs@32, crtcs@36, conns@40, encoders@44, widths/heights@48..
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
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

/// Copy `fb_id`'s pixels into the GPU's scanout buffer - the step that makes
/// a `SETCRTC`/`PAGE_FLIP` a presentation rather than bookkeeping. The rows
/// go one by one because the two pitches differ: the dumb buffer's is
/// 64-byte aligned, the scanout's is `width * 4`, and a whole-buffer copy
/// would smear every row after the first.
///
/// Returns 0, or -EINVAL when the framebuffer is unknown, not 32bpp, or
/// larger than the scanout's mode - Linux refuses a framebuffer that does
/// not fit the CRTC with the same errno.
fn copy_to_scanout(st: &DrmState, fb_id: u32) -> isize {
    let Some(fb) = st
        .fbs
        .iter()
        .filter_map(|f| f.as_ref())
        .find(|f| f.id == fb_id)
    else {
        return libc_EINVAL();
    };
    let Some(dumb) = st.dumb.get(fb.handle as usize - 1).and_then(|s| s.as_ref()) else {
        return libc_EINVAL();
    };
    let (screen_w, screen_h) = crtc_mode_dims();
    if fb.bpp != 32 || fb.width > screen_w || fb.height > screen_h {
        return libc_EINVAL();
    }
    // A headless machine has no scanout to draw to; the bookkeeping is still
    // right, and the flush after is the no-op it already is there.
    let Some(gpu) = crate::drivers::virtio::gpu::global() else {
        return 0;
    };
    let scanout = phys_to_virt(gpu.fb_phys()) as *mut u8;
    let scan_pitch = screen_w as usize * 4;
    let row = (fb.width as usize * 4).min(scan_pitch);
    let rows = (fb.height as usize).min(screen_h as usize);
    for y in 0..rows {
        unsafe {
            dumb.copy_span(y * dumb.pitch as usize, scanout.add(y * scan_pitch), row);
        }
    }
    0
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
        }
    });
    0
}

fn set_crtc(arg: usize) -> isize {
    // struct drm_mode_crtc: crtc_id@12, fb_id@16, x@20,y@24, ...
    let crtc_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return libc_EINVAL();
    }
    // fb_id 0 is "disable the CRTC": accepted, and nothing is drawn. Any
    // other id must be a framebuffer the card made, whose pixels are put
    // on the scanout before the call returns.
    let rc = if fb_id == 0 {
        critical_section::with(|cs| STATE.borrow(cs).borrow_mut().current_fb = 0);
        0
    } else {
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            match copy_to_scanout(&st, fb_id) {
                0 => {
                    st.current_fb = fb_id;
                    0
                }
                e => e,
            }
        })
    };
    // Present: flush the scanout if a render engine is running.
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

fn get_encoder(arg: usize) -> isize {
    // struct drm_mode_get_encoder: encoder_id@0, encoder_type@4, crtc_id@8,
    // possible_crtcs@12, possible_clones@16.
    unsafe {
        let p = arg as *mut u32;
        let want = p.read_unaligned();
        if want != 0 && want != ENCODER_ID {
            return libc_EINVAL();
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
        return libc_EINVAL();
    }
    let (w, h) = crtc_mode_dims();
    // The connector's two properties and their current values - what
    // `OBJ_GETPROPERTIES` also answers, on the path libdrm fills in through
    // `drmModeGetConnector`.
    let (crtc_value, dpms_value) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        (st.conn_crtc as u64, st.dpms as u64)
    });
    unsafe {
        let p = arg as *mut u8;
        // Fill a single mode into modes_ptr if the caller asked for it.
        let count_modes = ((p.add(32) as *const u32).read_unaligned());
        let modes_ptr = ((p.add(8) as *const u64).read_unaligned()) as *mut u8;
        if count_modes >= 1 && !modes_ptr.is_null() {
            write_mode(modes_ptr, (w * h * 60) / 1000, w, h);
        }
        (p.add(32) as *mut u32).write_unaligned(1); // count_modes
        // The property ids and values, when the caller has room for them.
        let count_props = (p.add(36) as *const u32).read_unaligned();
        (p.add(36) as *mut u32).write_unaligned(2); // count_props
        let props_ptr = ((p.add(16) as *const u64).read_unaligned()) as *mut u32;
        if count_props >= 2 && !props_ptr.is_null() {
            props_ptr.write_unaligned(PROP_CRTC_ID);
            props_ptr.add(1).write_unaligned(PROP_DPMS);
        }
        let values_ptr = ((p.add(24) as *const u64).read_unaligned()) as *mut u64;
        if count_props >= 2 && !values_ptr.is_null() {
            values_ptr.write_unaligned(crtc_value);
            values_ptr.add(1).write_unaligned(dpms_value);
        }
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

/// The one plane: the CRTC's primary surface, reported the way a compositor
/// matches an output format against it.
fn get_plane(arg: usize) -> isize {
    // struct drm_mode_get_plane: plane_id@0, crtc_id@4, fb_id@8,
    // possible_crtcs@12, gamma_size@16, count_format_types@20,
    // format_type_ptr@24. The format list is the two-call shape the KMS
    // list ioctls all use: count first, the array once there is room.
    let fb_id = critical_section::with(|cs| STATE.borrow(cs).borrow().current_fb);
    unsafe {
        let p = arg as *mut u32;
        p.write_unaligned(PLANE_ID); // plane_id
        p.add(1).write_unaligned(CRTC_ID); // crtc_id: the CRTC it serves
        p.add(2).write_unaligned(fb_id); // fb_id
        p.add(3).write_unaligned(1); // possible_crtcs: bit 0
        p.add(4).write_unaligned(0); // gamma_size
        // The format list, when the caller has room for it.
        let want = p.add(5).read_unaligned();
        let formats = ((arg + 24) as *const u64).read_unaligned() as *mut u32;
        if want >= 1 && !formats.is_null() {
            formats.write_unaligned(DRM_FORMAT_XRGB8888);
        }
        p.add(5).write_unaligned(1); // count_format_types
    }
    0
}

/// `GETPROPERTY`: one of the three standard properties by id - the shape
/// every KMS property query takes (counts first, arrays once the caller has
/// sized them).
fn get_property(arg: usize) -> isize {
    // struct drm_mode_get_property: values_ptr@0, enum_blob_ptr@8,
    // prop_id@16, flags@20, name[32]@24, count_values@56, count_enum_blobs@60.
    let prop_id = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    // (name, flags, value list, enum entries as (name, value))
    let (name, flags, values, enums): (&str, u32, &[u64], &[(&str, u64)]) = match prop_id {
        // An object-typed property: the value is the id of a CRTC (0 for
        // none). The *current* binding comes from the connector's property
        // list, not from here - this reports the property's shape.
        PROP_CRTC_ID => ("CRTC_ID", PROP_OBJECT, &[0], &[]),
        PROP_DPMS => (
            "DPMS",
            PROP_ENUM,
            &[0, 1, 2, 3],
            &[("On", 0), ("Standby", 1), ("Suspend", 2), ("Off", 3)],
        ),
        PROP_ACTIVE => ("ACTIVE", PROP_RANGE, &[0, 1], &[]),
        // The plane's class, and the one value it carries here: this is the
        // primary surface, the plane a compositor looks for by name.
        PROP_TYPE => (
            "type",
            PROP_ENUM | PROP_IMMUTABLE,
            &[0, 1, 2],
            &[("Overlay", 0), ("Primary", 1), ("Cursor", 2)],
        ),
        _ => return libc_EINVAL(),
    };
    unsafe {
        // The name, NUL-padded into its 32-byte field.
        let field = (arg + 24) as *mut u8;
        core::ptr::write_bytes(field, 0, 32);
        core::ptr::copy_nonoverlapping(name.as_ptr(), field, name.len());
        ((arg + 20) as *mut u32).write_unaligned(flags);
        // The value list, when the caller has room for it.
        let want_values = ((arg + 56) as *const u32).read_unaligned();
        let values_ptr = (arg as *const u64).read_unaligned() as *mut u64;
        if want_values >= values.len() as u32 && !values_ptr.is_null() {
            for (i, v) in values.iter().enumerate() {
                values_ptr.add(i).write_unaligned(*v);
            }
        }
        ((arg + 56) as *mut u32).write_unaligned(values.len() as u32);
        // The enum entries (struct drm_mode_property_enum: u64 value,
        // name[32]), same shape.
        let want_enums = ((arg + 60) as *const u32).read_unaligned();
        let enums_ptr = ((arg + 8) as *const u64).read_unaligned() as *mut u8;
        if want_enums >= enums.len() as u32 && !enums_ptr.is_null() {
            for (i, (ename, evalue)) in enums.iter().enumerate() {
                let entry = enums_ptr.add(i * 40);
                (entry as *mut u64).write_unaligned(*evalue);
                core::ptr::write_bytes(entry.add(8), 0, 32);
                core::ptr::copy_nonoverlapping(ename.as_ptr(), entry.add(8), ename.len());
            }
        }
        ((arg + 60) as *mut u32).write_unaligned(enums.len() as u32);
    }
    0
}

/// Record a property write against the connector. Only the two properties it
/// carries are settable; anything else is the refusal Linux makes of a
/// property the object does not have.
fn set_connector_prop(prop_id: u32, value: u64) -> isize {
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        match prop_id {
            PROP_CRTC_ID => st.conn_crtc = value as u32,
            PROP_DPMS => st.dpms = value as u32,
            _ => return libc_EINVAL(),
        };
        0
    })
}

/// `SETPROPERTY`: the connector's property, by the legacy connector-typed
/// spelling.
fn set_property(arg: usize) -> isize {
    // struct drm_mode_connector_set_property: value@0(u64), prop_id@8,
    // connector_id@12.
    let value = unsafe { (arg as *const u64).read_unaligned() };
    let prop_id = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let connector_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    if connector_id != CONNECTOR_ID {
        return libc_EINVAL();
    }
    set_connector_prop(prop_id, value)
}

/// `OBJ_SETPROPERTY`: the object-typed spelling of the same write.
fn obj_set_property(arg: usize) -> isize {
    // struct drm_mode_obj_set_property: value@0(u64), prop_id@8, obj_id@12,
    // obj_type@16.
    let value = unsafe { (arg as *const u64).read_unaligned() };
    let prop_id = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let obj_id = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    match (obj_type, obj_id) {
        (OBJECT_CONNECTOR, CONNECTOR_ID) => set_connector_prop(prop_id, value),
        (OBJECT_CRTC, CRTC_ID) if prop_id == PROP_ACTIVE => {
            critical_section::with(|cs| {
                STATE.borrow(cs).borrow_mut().crtc_active = value != 0;
            });
            0
        }
        _ => libc_EINVAL(),
    }
}

/// `OBJ_GETPROPERTIES`: the property ids and values one KMS object carries -
/// the two-call list shape again, over the same facts `GETCONNECTOR`
/// reports for the connector.
fn obj_get_properties(arg: usize) -> isize {
    // struct drm_mode_obj_get_properties: props_ptr@0, prop_values_ptr@8,
    // count_props@16, obj_id@20, obj_type@24.
    let obj_id = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let obj_type = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    // (ids, values, count) - the arrays are fixed-size so the lock only
    // guards two word reads.
    let Some((ids, values, count)) = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        match (obj_type, obj_id) {
            (OBJECT_CONNECTOR, CONNECTOR_ID) => Some((
                [PROP_CRTC_ID, PROP_DPMS],
                [st.conn_crtc as u64, st.dpms as u64],
                2usize,
            )),
            (OBJECT_CRTC, CRTC_ID) => Some(([PROP_ACTIVE, 0], [st.crtc_active as u64, 0], 1usize)),
            // The plane's type is what weston picks its primary plane by.
            (OBJECT_PLANE, PLANE_ID) => Some(([PROP_TYPE, 0], [PLANE_TYPE_PRIMARY, 0], 1usize)),
            _ => None,
        }
    }) else {
        return libc_EINVAL();
    };
    unsafe {
        let want = ((arg + 16) as *const u32).read_unaligned();
        if want >= count as u32 {
            let ids_ptr = (arg as *const u64).read_unaligned() as *mut u32;
            let values_ptr = ((arg + 8) as *const u64).read_unaligned() as *mut u64;
            if !ids_ptr.is_null() {
                for (i, id) in ids.iter().enumerate().take(count) {
                    ids_ptr.add(i).write_unaligned(*id);
                }
            }
            if !values_ptr.is_null() {
                for (i, value) in values.iter().enumerate().take(count) {
                    values_ptr.add(i).write_unaligned(*value);
                }
            }
        }
        ((arg + 16) as *mut u32).write_unaligned(count as u32);
    }
    0
}

/// Bind a dumb buffer to a fresh framebuffer id. The handle must be one the
/// card made or imported, and the geometry must be a shape the scanout can
/// show: 32bpp, nonzero, with a pitch wide enough for a row. `ADDFB` and
/// `ADDFB2` differ only in how they spell those facts.
fn insert_fb(
    st: &mut DrmState,
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
) -> Option<u32> {
    let slot = handle as usize;
    // The handle's buffer must exist - and the geometry it comes with has to
    // be a shape the scanout can show: the handle's own bpp (the scanout is
    // a 32-bit BGRA surface, whatever the ADDFB claims), nonzero, with a
    // pitch wide enough for a row.
    let dumb = st.dumb.get(slot.checked_sub(1)?).and_then(|d| d.as_ref())?;
    if dumb.bpp != 32 || bpp != 32 || width == 0 || height == 0 || pitch < width * 4 {
        return None;
    }
    let id = st.fbs.len() as u32 + 100;
    st.fbs.push(Some(Framebuffer {
        id,
        handle,
        width,
        height,
        pitch,
        bpp,
        depth,
    }));
    Some(id)
}

fn add_fb(arg: usize) -> isize {
    // struct drm_mode_fb_cmd: fb_id@0,width@4,height@8,pitch@12,bpp@16,
    // depth@20,handle@24.
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let pitch = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 16) as *const u32).read_unaligned() };
    let depth = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 24) as *const u32).read_unaligned() };
    let id = critical_section::with(|cs| {
        insert_fb(
            &mut STATE.borrow(cs).borrow_mut(),
            handle,
            width,
            height,
            pitch,
            bpp,
            depth,
        )
    });
    match id {
        Some(id) => {
            unsafe { (arg as *mut u32).write_unaligned(id) };
            0
        }
        None => libc_EINVAL(),
    }
}

fn add_fb2(arg: usize) -> isize {
    // struct drm_mode_fb_cmd2: fb_id@0,width@4,height@8,pixel_format@12,
    // flags@16, handles[4]@20, pitches[4]@36, offsets[4]@52, modifier[4]@68.
    // The fourcc names the layout; every 32-bit spelling this scanout can
    // show lands on the ADDFB path, and the rest is refused. A nonzero
    // plane offset or format modifier is too: there is exactly one linear
    // plane, and pretending otherwise would lie about what gets scanned.
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let height = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    let format = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    let handle = unsafe { ((arg + 20) as *const u32).read_unaligned() };
    let pitch = unsafe { ((arg + 36) as *const u32).read_unaligned() };
    let offset = unsafe { ((arg + 52) as *const u32).read_unaligned() };
    let modifier = unsafe { ((arg + 68) as *const u64).read_unaligned() };
    if offset != 0 || modifier != 0 {
        return libc_EINVAL();
    }
    let (bpp, depth) = match format {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_XBGR8888 => (32, 24),
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_ABGR8888 => (32, 32),
        _ => return libc_EINVAL(),
    };
    let id = critical_section::with(|cs| {
        insert_fb(
            &mut STATE.borrow(cs).borrow_mut(),
            handle,
            width,
            height,
            pitch,
            bpp,
            depth,
        )
    });
    match id {
        Some(id) => {
            unsafe { (arg as *mut u32).write_unaligned(id) };
            0
        }
        None => libc_EINVAL(),
    }
}

/// `GETFB`, in the direction ADDFB came from: the framebuffer's geometry
/// and the handle behind it.
fn get_fb(arg: usize) -> isize {
    // struct drm_mode_fb_cmd again: fb_id@0 selects, the rest comes back.
    let id = unsafe { (arg as *const u32).read_unaligned() };
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let Some(fb) = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .find(|f| f.id == id)
        else {
            return libc_EINVAL();
        };
        unsafe {
            ((arg + 4) as *mut u32).write_unaligned(fb.width);
            ((arg + 8) as *mut u32).write_unaligned(fb.height);
            ((arg + 12) as *mut u32).write_unaligned(fb.pitch);
            ((arg + 16) as *mut u32).write_unaligned(fb.bpp);
            ((arg + 20) as *mut u32).write_unaligned(fb.depth);
            ((arg + 24) as *mut u32).write_unaligned(fb.handle);
        }
        0
    })
}

/// Release a GEM handle - the one path `GEM_CLOSE` and `DESTROY_DUMB` both
/// spell. The dumb buffer gives its frames back, and the PRIME records that
/// name the handle go too: the *dma-bufs* the handle exported keep their own
/// references and their own lifetime, but a later import must not be
/// answered with a handle that no longer exists. A handle already released
/// is the refusal Linux makes too.
fn destroy_handle(handle: usize) -> isize {
    if handle == 0 {
        return libc_EINVAL();
    }
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.prime.retain(|record| record.handle as usize != handle);
        match st.dumb.get_mut(handle - 1) {
            Some(slot) if slot.is_some() => {
                if let Some(d) = slot.as_mut() {
                    d.release();
                }
                *slot = None;
                0
            }
            _ => libc_EINVAL(),
        }
    })
}

/// `PRIME_HANDLE_TO_FD`: export a handle as a dma-buf descriptor - the door
/// GBM and the GL stack walk buffers through. Every page of the buffer
/// becomes real first, so a mapping of the descriptor past what the handle
/// had grown still aliases the handle's own pages; then the descriptor is
/// the dma-buf over those pages with one reference of its own per page, and
/// the identity record lets a second export of the same handle answer the
/// same object.
fn prime_handle_to_fd(arg: usize) -> isize {
    // struct drm_prime_handle: handle@0, flags@4, fd@8.
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let buf = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // A handle out of range, or one the card never made, answers nothing.
        if handle == 0 || handle > MAX_HANDLES {
            return None;
        }
        let dumb = st.dumb.get_mut(handle - 1).and_then(|slot| slot.as_mut())?;
        // A handle already exported: the same object again, so both
        // descriptors map the same pages.
        if let Some(buf) = st
            .prime
            .iter()
            .find(|record| record.handle as usize == handle)
            .and_then(|record| record.buf.clone())
        {
            return Some(buf);
        }
        // Grow the whole buffer, then clone one reference per page for the
        // dma-buf; its `Drop` gives these back when the last descriptor
        // closes, so the buffer dies exactly then and never while a
        // mapping or the exporting handle still needs it.
        let pages = (dumb.size() as usize).div_ceil(PAGE_SIZE);
        for idx in 0..pages {
            if dumb.ensure_page(idx) == 0 {
                return None; // the cap or the frame allocator said no
            }
        }
        let frames = dumb.frames.clone();
        for &frame in &frames {
            clone_frame_ref(frame);
        }
        let buf = Arc::new(crate::vfs::dmabuf::DmaBuf::new(
            dumb.width,
            dumb.height,
            dumb.pitch,
            frames,
        ));
        st.prime.push(PrimeRecord {
            ino: buf.ino(),
            handle: handle as u32,
            buf: Some(buf.clone()),
        });
        Some(buf)
    });
    let Some(buf) = buf else {
        return libc_EINVAL();
    };
    // The descriptor, in the caller's own table, under the caller's own
    // descriptor ceiling.
    let file = Arc::new(crate::vfs::File::new(buf));
    let fd = match crate::sched::current_fds().install(file) {
        Ok(fd) => fd,
        Err(crate::vfs::VfsError::TooManyDescriptors) => return -24, // EMFILE
        Err(_) => return libc_EINVAL(),
    };
    if flags & DRM_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    unsafe { ((arg + 8) as *mut i32).write_unaligned(fd) };
    0
}

/// `PRIME_FD_TO_HANDLE`: import a dma-buf descriptor as a handle - the
/// compositor's half of the GBM path. A descriptor this card exported
/// answers the handle it came from; any other dma-buf gets a fresh dumb
/// buffer over its pages, each page with one reference the handle owns (the
/// descriptor keeps its own), so closing either side leaves the other
/// standing.
fn prime_fd_to_handle(arg: usize) -> isize {
    // struct drm_prime_handle, the other way: fd@8 names, handle@0 receives.
    let fd = unsafe { ((arg + 8) as *const i32).read_unaligned() };
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(_) => return -9, // EBADF
    };
    // The inode is bound first: `as_dmabuf` borrows from it, and the borrow
    // must outlive the statement that asks for it.
    let inode = file.inode();
    let Some(buf) = inode.as_dmabuf() else {
        return libc_EINVAL(); // not a dma-buf descriptor
    };
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let st = &mut *st;
        // The same object the card already knows: its handle, so repeated
        // imports never stack up aliases of one buffer.
        if let Some(record) = st.prime.iter().find(|record| record.ino == buf.ino()) {
            return record.handle;
        }
        if st.dumb.len() >= MAX_HANDLES {
            return 0;
        }
        let frames = buf.clone_frames();
        st.dumb.push(Some(Dumb {
            width: buf.width(),
            height: buf.height(),
            pitch: buf.pitch(),
            bpp: 32,
            frames,
        }));
        let handle = st.dumb.len() as u32;
        st.prime.push(PrimeRecord {
            ino: buf.ino(),
            handle,
            buf: None,
        });
        handle
    });
    if handle == 0 {
        return libc_EINVAL(); // the handle table is full
    }
    unsafe { (arg as *mut u32).write_unaligned(handle) };
    0
}

fn page_flip(arg: usize) -> isize {
    // struct drm_mode_crtc_page_flip: crtc_id@0, fb_id@4, flags@8, reserved@12,
    // user_data@16.
    let crtc_id = unsafe { (arg as *const u32).read_unaligned() };
    let fb_id = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    if crtc_id != CRTC_ID {
        return libc_EINVAL();
    }
    let rc = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let rc = copy_to_scanout(&st, fb_id);
        if rc == 0 {
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
        }
        rc
    });
    if rc == 0 {
        crate::drivers::video::render::request_flush_blocking();
    }
    rc
}

fn create_dumb(arg: usize) -> isize {
    // struct drm_mode_create_dumb: height@0,width@4,bpp@8,flags@12,
    // handle@16,pitch@20,size@24(u64).
    let height = unsafe { (arg as *const u32).read_unaligned() };
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    if bpp == 0 || width == 0 || height == 0 {
        return libc_EINVAL();
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
        return libc_EINVAL();
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
