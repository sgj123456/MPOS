use super::abi::{DRM_MAJOR, MAX_HANDLES, PAGES_PER_BUFFER};
use super::ioctl::libc_einval;
use super::master::{CARD_WAITERS, LeaseSet, revoked};
use super::state::{Dumb, STATE, drm_event};

use crate::mm::{PAGE_SIZE, clone_frame_ref};
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsResult};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub(crate) fn create_dumb(arg: usize, ofd: u64) -> isize {
    // struct drm_mode_create_dumb: height@0,width@4,bpp@8,flags@12,
    // handle@16,pitch@20,size@24(u64).
    let height = unsafe { (arg as *const u32).read_unaligned() };
    let width = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let bpp = unsafe { ((arg + 8) as *const u32).read_unaligned() };
    if bpp == 0 || width == 0 || height == 0 {
        return libc_einval();
    }
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let idx = st.dumb.len();
        if idx >= MAX_HANDLES {
            return 0usize;
        }
        st.dumb
            .push(Some(Dumb::new(width, height, bpp).named_by(ofd)));
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
    crate::println!(
        "mpos-dbg: CREATE_DUMB -> handle {handle} ({width}x{height} {bpp}bpp) ofd={ofd}"
    );
    0
}

pub(crate) fn map_dumb(arg: usize) -> isize {
    // struct drm_mode_map_dumb: handle@0, pad@4, offset@8(u64).
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    if handle == 0 {
        return libc_einval();
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
    /// The DRM minor this name is: 0 for `card0`, 128 for `renderD128`. DRM's
    /// major is 226 by convention (fixed since XFree86), and the render node's
    /// minor is the card index plus 128. A lease gets one of its own (see
    /// [`DrmCard::lease`]), because `fstat` on a lease fd must not be
    /// indistinguishable from `fstat` on the card it came from.
    minor: u32,
    /// The objects this descriptor may drive, or `None` for the whole card.
    ///
    /// A lease is *only* this: a subset of the card's object ids, plus the fact
    /// that its holder is treated as a master of them. Everything else - the
    /// dumb buffers, the framebuffers, the event queue - is the one shared card
    /// state, exactly as on Linux, where a lease is a `drm_file` pointing at the
    /// same `drm_device` with a list of allowed object ids attached.
    lease: Option<Arc<LeaseSet>>,
    /// The node's mode and owner - `chmod`/`chown`/udev's rules on `card0`
    /// stick here and are what `stat` and the permission checks read
    /// (see `crate::vfs::devfs::NodePerms`).
    perms: crate::vfs::devfs::NodePerms,
}
impl DrmCard {
    pub fn new(ino: u64, minor: u32) -> Arc<DrmCard> {
        Arc::new(DrmCard {
            ino,
            minor,
            lease: None,
            // devtmpfs births both the card and the render node `0600
            // root:root` (measured on Linux 7.2.11: `card0` and `renderD129`
            // are strict at creation, and the rules then hand the card to
            // group `video` and widen the render node to `0666`).
            perms: crate::vfs::devfs::NodePerms::new(crate::vfs::devfs::BIRTH_RESTRICTED, 0, 0),
        })
    }

    /// A lease of this card: a fresh inode number, the next lease minor, and the
    /// object ids it may drive.
    pub(crate) fn lease_of(&self, objects: Vec<u32>, sealed: u32) -> Arc<DrmCard> {
        static LEASE_INO: core::sync::atomic::AtomicU64 =
            core::sync::atomic::AtomicU64::new(0x9110);
        static LEASE_MINOR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(64);
        Arc::new(DrmCard {
            ino: LEASE_INO.fetch_add(1, Ordering::Relaxed),
            minor: LEASE_MINOR.fetch_add(1, Ordering::Relaxed),
            lease: Some(Arc::new(LeaseSet::new(objects, sealed))),
            // A lease is an fd-only inode - it has no `/dev` name, so this
            // number is not a birth anyone stats; it starts strict like the
            // card it leases.
            perms: crate::vfs::devfs::NodePerms::new(crate::vfs::devfs::BIRTH_RESTRICTED, 0, 0),
        })
    }

    /// The object ids this descriptor may drive, for the requests that name an
    /// object by id. `None` is the whole card.
    pub fn lease(&self) -> Option<&LeaseSet> {
        self.lease.as_deref()
    }

    /// The same lease as the shared handle the lessee registry keeps, so a lease
    /// that has been handed out can still be reached by its lessee id after its
    /// descriptor has gone to another process.
    pub(crate) fn lease_arc(&self) -> Option<Arc<LeaseSet>> {
        self.lease.clone()
    }

    /// Which of the card's names this node is: 0 = `card0`, the master's screen;
    /// 128 = `renderD128`, an allocation handle that is never asked who owns the
    /// display.
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// Whether a `read` would return an event now.
    pub(crate) fn has_event() -> bool {
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

    fn keeps_modes(&self) -> bool {
        self.perms.keeps()
    }

    fn mode(&self) -> u32 {
        self.perms.mode()
    }

    fn set_mode(&self, mode: u32) {
        self.perms.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        self.perms.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        self.perms.set_owner(uid, gid);
    }

    fn atime(&self) -> u64 {
        self.perms.atime()
    }

    fn mtime(&self) -> u64 {
        self.perms.mtime()
    }

    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        self.perms.set_times(atime, mtime);
        Ok(())
    }

    /// The device number `stat` reports: DRM's major 226 and this name's
    /// minor. Mesa's loader identifies a DRM device through exactly this
    /// pair - it looks the number up under `/sys/dev/char/226:0` to find
    /// which driver owns the fd - so a card that reports `0,0` is a card
    /// whose driver cannot be named ("MESA-LOADER: failed to retrieve device
    /// information", which is what the log said before this existed).
    fn rdev(&self) -> Option<(u32, u32)> {
        Some((DRM_MAJOR, self.minor))
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

    /// The card reports hang-up once its master has lost the console.
    ///
    /// This is the only notice a compositor gets that does not depend on it
    /// asking: weston parks in `poll` on the card fd inside `drmHandleEvent`,
    /// and a master that has been revoked has nothing left to wait *for* - its
    /// flips are refused, so no completion event will ever arrive to wake it.
    /// Without the bit the session is not merely frozen, it is frozen with the
    /// keyboard and the pointer still pointed at it.
    fn poll_hup(&self) -> bool {
        revoked()
    }

    /// A card reader parks on this list and is woken by the frame that carries
    /// its event, which is what takes this node off the "readiness nobody can
    /// announce" list `poll_select`'s comment describes.
    fn poll_subscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            let mut v = CARD_WAITERS.borrow(cs).borrow_mut();
            if !v.contains(&id) {
                v.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            CARD_WAITERS.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
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

    /// A dumb buffer is real memory the card scans out; its mapping must be
    /// those pages, so the mapping is frame-backed.
    fn frame_backed(&self) -> bool {
        true
    }
}
