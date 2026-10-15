pub(crate) static MASTER_OFD: AtomicU64 = AtomicU64::new(0);
/// The thread group that holds it, for the `KDSETMODE` check further down. Linux
/// does not need this because its tty carries a pointer to the DRM `master`;
/// this kernel has no object that spans the two, so the process is the closest
/// thing to one.
use super::abi::{
    CRTC_ID, DRM_CLOEXEC, EVENT_FLIP_COMPLETE, LEASE_IMMUTABLE, LEASE_SEAL_ALL, NR_MODE_ATOMIC,
    NR_MODE_CURSOR, NR_MODE_CURSOR2, NR_OBJ_SETPROPERTY, NR_PAGE_FLIP, NR_RMFB, NR_SETCRTC,
};
use super::card::DrmCard;
use super::fb::{BufferGone, drop_fb_hold, hide_host_cursor, tell_buffer_gone};
use super::ioctl::user_array;
use super::props::copy_to_scanout;
use super::state::{STATE, drm_event};

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use critical_section::Mutex;

static MASTER_PID: AtomicU64 = AtomicU64::new(0);
/// The console master was taken on lives in `crate::vfs::vcmux`, not here: it
/// is the console layer's fact - the same one `KDSETMODE` and `VT_ACTIVATE`
/// write - and reading it from two places would only let the two disagree.
/// Whether the master has been revoked by a console switch since it was held:
/// the one edge in this card's life a compositor can hear without polling,
/// because it turns the card fd's readability into `POLLHUP`.
static REVOKED: AtomicBool = AtomicBool::new(false);

/// A magic the master has been handed and not yet spent: the token, and the
/// description it names.
type PendingMagic = (u32, u64);

/// The tokens on the card, the descriptions they name, and the descriptions the
/// master has authenticated.
///
/// Linux keeps these as a `drm_magic_list` per `drm_master` plus an `auth` list
/// per file private, and because its masters are real objects the lists are
/// shared between the fds of one session. mpos has exactly one master at a
/// time, which collapses both lists to one table each - and the collapse is the
/// reason a token here is *global* rather than per-master: it names a
/// description, and only the master's right to spend it is checked at
/// `AUTH_MAGIC`.
static MAGICS: critical_section::Mutex<RefCell<Vec<PendingMagic>>> =
    critical_section::Mutex::new(RefCell::new(Vec::new()));
static AUTH_OFD: critical_section::Mutex<RefCell<Vec<u64>>> =
    critical_section::Mutex::new(RefCell::new(Vec::new()));
/// The next token to hand out. Non-zero, and never reused within a boot, so a
/// token is unambiguous even after its description has closed.
static NEXT_MAGIC: AtomicU32 = AtomicU32::new(1);

/// `DRM_IOCTL_SET_MASTER` / `_DROP_MASTER` / `_AUTH`, which carry no argument
/// and so are answered before the request's buffer is checked.
///
/// mpos has one card and one screen, and the whole point of these three is not
/// arbitration between competing masters (there is nobody to arbitrate
/// between) but the *handover*: a compositor that cannot get past `SET_MASTER`
/// never gets an fd it may set a mode on, and one that is never told it lost
/// the screen paints over the text console forever. So taking the master
/// records which console it was taken on, and losing that console revokes it.
///
/// The render node is exempt throughout: Mesa's GBM allocates buffers on
/// `renderD128` precisely because a GL client is not the display master, and a
/// card that asked it to prove otherwise would take the renderer down with it.
pub(crate) fn master_request(nr: u32, minor: u32, ofd: u64) -> isize {
    if minor != 0 {
        // Not a request a render node answers with anything but success; it has
        // no screen to own, and libdrm asks anyway on its way to an allocation.
        return 0;
    }
    match nr {
        super::abi::NR_SET_MASTER => {
            // Linux's `SET_MASTER` is *void*: it never fails, and it takes the
            // card from whoever held it. That is not sloppiness - it is the
            // recovery path. A compositor that regains its console calls
            // `SET_MASTER` again and must succeed unconditionally, because the
            // alternative is a session that lost a race with a `^A` and can
            // never come back. Taking the master therefore ends the previous
            // holder's claim, drops the authentications it granted, and leaves
            // any unspent tokens in place: a token names a *description*, not a
            // master, and Linux's `DROP_MASTER` keeps those lists too.
            let previous = MASTER_OFD.swap(ofd, Ordering::Relaxed);
            if previous != 0 && previous != ofd {
                critical_section::with(|cs| AUTH_OFD.borrow(cs).borrow_mut().clear());
            }
            let vc = current_console().unwrap_or_else(crate::term::vc::active);
            MASTER_PID.store(crate::sched::current_tgid().0 as u64, Ordering::Relaxed);
            // The master is always bound to a console. A holder with no
            // controlling terminal - seatd taking the card before it hands the
            // fd to weston, or a launch chain that never ran `TIOCSCTTY` - is
            // bound to the console on the screen at the moment master is taken,
            // which is the console the compositor is driving. An *unbound*
            // master could never be revoked from a `^A` switch: `master_ok`
            // passes it, `painted()` says the text console may draw, and the two
            // then race on one scanout - the mouse that repaints a text VT, and
            // the flicker of a shell and a compositor overwriting each other.
            // Binding here is what makes "losing the console revokes it" true.
            crate::vfs::vcmux::set_master_vc(vc);
            // A fresh master on a console that still holds the screen clears
            // the hang-up it was revoked under; a master on a console that is
            // not on the screen stays revoked, which is the honest state.
            if crate::vfs::vcmux::master_holds_screen() {
                REVOKED.store(false, Ordering::Relaxed);
            }
            0
        }
        super::abi::NR_DROP_MASTER => {
            // Also void, and also unconditional: releasing the master is always
            // allowed, including from a description that is not the master (the
            // call is a no-op for it rather than an error).
            if MASTER_OFD.load(Ordering::Relaxed) == ofd {
                MASTER_OFD.store(0, Ordering::Relaxed);
                MASTER_PID.store(0, Ordering::Relaxed);
                crate::vfs::vcmux::set_master_vc(0);
                REVOKED.store(false, Ordering::Relaxed);
            }
            0
        }
        // `DRM_IOCTL_AUTH` - the one request whose *failure* libdrm reads as
        // "you are the master". `drmIsMaster` opens with it, and weston's DRM
        // backend refuses to build a display device on a descriptor it cannot
        // prove owns the card, so a card that answered this the other way round
        // is a card no compositor will drive.
        _ => {
            if MASTER_OFD.load(Ordering::Relaxed) == ofd {
                -22 // EINVAL: the master cannot authenticate itself to itself
            } else {
                -13 // EACCES
            }
        }
    }
}

/// `DRM_IOCTL_GET_MAGIC`: mint a token naming the calling description, for the
/// master to spend on it.
///
/// One outstanding token per description, which is Linux's rule and is what the
/// list below enforces by removing an old entry before adding the new one: a
/// client that asks twice must not be authenticatable twice, and a master that
/// holds a stale token should hear nothing rather than something surprising.
pub(crate) fn take_magic(minor: u32, ofd: u64, arg: usize) -> isize {
    if arg == 0 {
        return -14; // EFAULT: `struct drm_auth` is the whole request
    }
    if minor != 0 {
        // The render node has no master to authenticate to; libdrm does not ask,
        // and an answer of "no token for you" would be more honest than a token
        // nothing can spend.
        return -13; // EACCES
    }
    let token = NEXT_MAGIC.fetch_add(1, Ordering::Relaxed);
    critical_section::with(|cs| {
        let mut m = MAGICS.borrow(cs).borrow_mut();
        m.retain(|&(_, owner)| owner != ofd);
        m.push((token, ofd));
    });
    // `struct drm_auth { __u32 magic; }`
    unsafe { (arg as *mut u32).write_unaligned(token) };
    0
}

/// `DRM_IOCTL_AUTH_MAGIC`: the master spends a token to let *another*
/// description drive the card. The caller must be the master (Linux's
/// `EACCES`), the token must still exist, and it is consumed either way.
pub(crate) fn auth_magic(caller: u64, token: u32) -> isize {
    if !is_master(caller) {
        return -13; // EACCES: only the master may let someone else in
    }
    let named = critical_section::with(|cs| {
        let mut m = MAGICS.borrow(cs).borrow_mut();
        m.iter()
            .position(|&(t, _)| t == token)
            .map(|at| m.remove(at).1)
    });
    match named {
        Some(owner) => {
            critical_section::with(|cs| {
                let mut a = AUTH_OFD.borrow(cs).borrow_mut();
                if !a.contains(&owner) {
                    a.push(owner);
                }
            });
            0
        }
        // An unknown or already-spent token. Linux answers EACCES here as well:
        // the caller proved nothing about the token it named.
        None => -13,
    }
}

/// The console the calling task is on, which is the console its session was
/// given when the seat manager opened it. `(4, 0)` and `(4, 64)` mean "the one
/// on the screen now", and `Tty::vc_number` already resolves both.
fn current_console() -> Option<u32> {
    let (major, minor) = crate::sched::controlling_tty_of_current()?;
    crate::vfs::tty::by_dev_id((major, minor))?.vc_number()
}

/// Whether the calling task's console is the one a DRM master holds. Linux lets
/// this answer stand in for a *terminal*'s foreground at `KDSETMODE`, because the
/// driver that owns the screen is allowed to say what is drawn on it; a seat
/// manager that has taken the card but not yet re-opened its own console tty is
/// the ordinary case it exists for.
///
/// The identity checked here is the task's, not the descriptor's, because
/// `KDSETMODE` arrives on a tty fd and cannot name the card fd it is talking
/// about - which is also why the answer is "somebody in this process holds the
/// card", not "this fd is authenticated".
pub fn card_grabbed_by_current() -> bool {
    if !master_held() {
        return false;
    }
    let tgid = crate::sched::current_tgid();
    MASTER_PID.load(Ordering::Relaxed) == tgid.0 as u64
}

/// Whether anybody holds the card at all. The console the holder named is a
/// separate question (`crate::vfs::vcmux::master_vc`), because a holder that
/// named none - a session with no controlling terminal, which is what a test
/// program has - has no console to lose and so must not be treated as one.
pub fn master_held() -> bool {
    MASTER_OFD.load(Ordering::Relaxed) != 0
}

/// Whether this description is the one that holds the master. Input uses it to
/// tell "the grab belongs to the session on the screen" from "somebody's grab
/// outlived their claim on the screen".
pub fn is_master(ofd: u64) -> bool {
    ofd != 0 && MASTER_OFD.load(Ordering::Relaxed) == ofd
}

/// Whether this description may drive the screen: the master itself, or one the
/// master has authenticated with a token. This is Linux's `drm_is_current_master`
/// answer to `DRM_AUTH`-gated requests, and the whole reason a client of a
/// compositor (a `weston-simple-dmabuf-egl`, an XWayland) can page-flip without
/// ever being a master of its own.
fn is_authorized(ofd: u64) -> bool {
    is_master(ofd) || critical_section::with(|cs| AUTH_OFD.borrow(cs).borrow().contains(&ofd))
}

/// Refuse a presentation request from a description that is neither the master
/// nor authenticated by it, or from a master whose console is no longer the one
/// on the screen.
///
/// The gate is asked before the request's struct is even read, and *not*
/// applied to the render node, so GBM keeps allocating and `mmap`ing while the
/// compositor that owns the screen is the only thing that can be locked out.
///
/// The second half of the condition is what the console work is about: a page
/// flip from a session that has lost its console is the mouse-move that paints a
/// compositor over a text VT, and the frame it commits is already in the scanout
/// by the time the answer comes back.
pub(crate) fn master_ok(nr: u32, minor: u32, ofd: u64) -> bool {
    if minor != 0 || !PRESENTATION.contains(&nr) {
        return true;
    }
    let Some(vc) = crate::vfs::vcmux::master_vc() else {
        // Nobody has claimed the card. (A *held* master is always bound now:
        // `SET_MASTER` falls back to the console on the screen, so `None` here
        // means "no master", not "a master that named no console".) A
        // compositor's first `SETCRTC` can arrive before its
        // seat manager's `SET_MASTER`, and a machine whose kernel refused it
        // there would be a machine that cannot show a mode at all - so an
        // unbound card serves its holder, and only a card bound to a console
        // can be revoked from one.
        return true;
    };
    if !crate::vfs::vcmux::holds_screen(vc) {
        revoke();
        return false;
    }
    is_authorized(ofd)
}

/// Queue a flip-completion event, *after* the frame it reports has been
/// flushed to the host.
///
/// The order is the point. A completion queued at commit time reports a frame
/// that has not left the machine yet, and a compositor that schedules against it
/// (which is precisely what `DRM_CAP_TIMESTAMP_MONOTONIC` promises it may do) is
/// then pacing itself against an event that arrives early, every frame, by
/// however long the present takes. `request_flush_blocking()` has returned by
/// now, so `since_boot_nanos` is the landing instant and the render task has
/// already counted the boundary (which is what `FBIO_WAITFORVSYNC` waits on).
///
/// This is the closest thing the machine has to a vertical blank, and the name
/// says so rather than claiming one: there is no display controller here to be
/// in blank, only a frame that has gone and one that has not.
pub(crate) fn queue_flip_event(sequence: u32, user_data: u64) {
    let now = crate::hal::since_boot_nanos();
    critical_section::with(|cs| {
        STATE
            .borrow(cs)
            .borrow_mut()
            .events
            .push_back(drm_event::Vblank {
                type_: EVENT_FLIP_COMPLETE,
                length: 32,
                user_data,
                time_sec: (now / 1_000_000_000) as u32,
                time_usec: ((now % 1_000_000_000) / 1000) as u32,
                sequence,
                crtc: CRTC_ID,
            });
    });
    // A reader parked in `poll`/`select` on the card fd was waiting for this
    // byte; say so, rather than leaving it to the next tick's re-check.
    wake_card_readers();
}

/// The tasks parked on the card fd's readability.
pub(crate) static CARD_WAITERS: critical_section::Mutex<
    RefCell<Vec<crate::sched::policy::TaskId>>,
> = critical_section::Mutex::new(RefCell::new(Vec::new()));

/// Wake whoever is polling the card. Called on every frame boundary as well as
/// on a queued event, because the two questions a card reader asks - "is there
/// an event?" and "has my flip landed?" - are answered at the same moment.
fn wake_card_readers() {
    let waiters = critical_section::with(|cs| CARD_WAITERS.borrow(cs).borrow().clone());
    for id in waiters {
        crate::sched::wake(id);
    }
}

/// The render task has presented a frame: tell anyone waiting on the card that
/// the boundary moved.
pub fn frame_landed() {
    wake_card_readers();
}

/// Whether a lease permits the object `id`. An unleased descriptor is the whole
/// card, so it permits whatever the card has.
pub(crate) fn leased(lease: Option<&LeaseSet>, id: u32) -> bool {
    lease.is_none_or(|l| l.holds(id))
}

/// The lessee ids `MODE_CREATE_LEASE` has handed out, and the lease each one
/// names.
///
/// Linux keeps a lease in the *master's* file private, which is where
/// `MODE_REVOKE_LEASE` finds it: that request arrives on the master carrying
/// nothing but the id, because the whole point of a lease is that its
/// descriptor has gone to another process. This kernel has no per-file lease
/// list to hang one on, so the id resolves here instead. An entry is dropped
/// when the lease is revoked; one whose descriptor is closed without a revoke
/// leaves its id behind, which is the same shape as a Linux `lessee_id` whose
/// file nobody can name any more.
static LESSEES: Mutex<RefCell<Vec<(u32, Arc<LeaseSet>)>>> = Mutex::new(RefCell::new(Vec::new()));
/// The next lessee id. Linux's is a per-file counter too, and nothing outside
/// this file compares them for anything but equality.
static NEXT_LESSEE: AtomicU32 = AtomicU32::new(1);

/// `DRM_IOCTL_MODE_CREATE_LEASE`: the master hands a subset of the card's
/// objects to somebody else, as a *file descriptor*, and names the handover
/// with a lessee id.
///
/// `struct drm_mode_create_lease { __u64 object_ids; __u32 object_count;
/// __u32 flags; __u32 lessee_id; __u32 fd; }` - five fields, 24 bytes, and no
/// trailing `pad` (this machine's `<drm_mode.h>` has none, as `cc` will say).
/// The objects are read
/// the way every other id list on this card is read (`user_array` first, then
/// element by element), and the fd is installed the way `PRIME_HANDLE_TO_FD`
/// installs its dma-buf: a new `File`, the lowest free slot, `O_CLOEXEC` if the
/// caller asked. The two returned words are in the header's order rather than
/// "whichever order this driver found convenient": a caller built against
/// `drm_mode.h` reads `fd` at 20, and a kernel that wrote it at 16 hands a real
/// `libdrm` a zero descriptor it thinks it can flip.
///
/// Only the master may lease. That is not a courtesy: a lease *is* a claim on
/// the screen, and letting an unauthorised descriptor mint one would be a way to
/// take the screen without ever calling `SET_MASTER`.
pub(crate) fn create_lease(card: &DrmCard, minor: u32, caller: u64, arg: usize) -> isize {
    if minor != 0 || !is_master(caller) {
        return -13; // EACCES
    }
    let objects_ptr = unsafe { (arg as *const u64).read_unaligned() } as usize;
    let count = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let flags = unsafe { ((arg + 12) as *const u32).read_unaligned() };
    if count == 0 || objects_ptr == 0 {
        return -22; // EINVAL
    }
    // The two flags this card knows. `IMMUTABLE` and `SEAL_ALL` are recorded on
    // the lease and read back; neither is enforced, which is spelled out on
    // [`LeaseSet`] rather than hidden here.
    if flags & !(LEASE_IMMUTABLE | LEASE_SEAL_ALL | DRM_CLOEXEC) != 0 {
        return -22; // EINVAL
    }
    // Linux bounds a lease at 16 objects; this card has four KMS objects in
    // total, so the bound is met by construction and asked only to catch a
    // caller whose count was garbage.
    if count > 16 {
        return -7; // E2BIG
    }
    let ids = unsafe { core::slice::from_raw_parts(objects_ptr as *const u32, count) };
    for id in ids {
        // Every id must be one this card actually has - and for a lease out of
        // a lease, one its own lease covers. A lease cannot widen what it was
        // given, which is the whole safety property of the family.
        if !known_object(*id) || !leased(card.lease(), *id) {
            return -22; // EINVAL
        }
    }
    let leased = card.lease_of(ids.to_vec(), flags);
    let Some(lease) = leased.lease_arc() else {
        return -22; // EINVAL: the descriptor we just made is somehow no lease
    };
    let fd = match crate::vfs::open_inode(leased) {
        Ok(fd) => fd,
        Err(_) => return -24, // EMFILE
    };
    if flags & DRM_CLOEXEC != 0 {
        // A failed `FD_SETFD` on a descriptor we just made is not this request's
        // failure to report; the lease is real and the flag is the caller's own
        // `exec` hygiene.
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    let lessee = NEXT_LESSEE.fetch_add(1, Ordering::Relaxed);
    critical_section::with(|cs| LESSEES.borrow(cs).borrow_mut().push((lessee, lease)));
    // The header's order: `lessee_id` at 16, `fd` at 20.
    unsafe { ((arg + 16) as *mut u32).write_unaligned(lessee) };
    unsafe { ((arg + 20) as *mut u32).write_unaligned(fd as u32) };
    0
}

/// `DRM_IOCTL_MODE_GET_LEASE`: which objects a lease covers.
///
/// The request is issued on the *lease* descriptor - `drmModeGetLease(fd)` is
/// given the fd `drmModeCreateLease` returned, not the master's - because on
/// Linux the lease is the file and the id list hangs off it. `struct
/// drm_mode_get_lease { __u32 count_objects; __u32 pad; __u64 objects_ptr; }`:
/// the count is always rewritten, the array is filled when the caller brought
/// room for it and somewhere to put it, and a descriptor that is not a lease
/// answers the `EINVAL` Linux gives a lease that does not exist. (An earlier
/// shape named a descriptor number in the struct instead, which is a request no
/// `libdrm` sends: it made this ioctl unaskable from the only program that has
/// the lease.)
pub(crate) fn get_lease(card: &DrmCard, arg: usize) -> isize {
    let Some(lease) = card.lease() else {
        return -22; // EINVAL: this descriptor is not a lease
    };
    let ids = lease.objects();
    // The caller's `count_objects` is both the question and the answer: it
    // arrives as the room the caller brought and leaves as the total there is.
    // So it is read *before* the answer is written - a handler that wrote first
    // and read back would see its own total and copy that many, which is a
    // caller whose array was shorter than the lease getting its neighbour
    // overwritten. Linux reads the capacity, writes the total and copies the
    // smaller of the two; the write below is therefore last of the three, and
    // the copy is bounded by what the caller actually offered.
    let room = unsafe { (arg as *const u32).read_unaligned() } as usize;
    unsafe { (arg as *mut u32).write_unaligned(ids.len() as u32) };
    let ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let take = core::cmp::min(room, ids.len());
    if take > 0 && ptr != 0 && user_array(ptr, take, 4, true) {
        let out = ptr as *mut u32;
        for (i, id) in ids.iter().take(take).enumerate() {
            unsafe { out.add(i).write_unaligned(*id) };
        }
    }
    0
}

/// The card's four fixed KMS object ids. Property ids are *not* leaseable:
/// Linux leases connectors, crtcs and planes, and nothing else.
fn known_object(id: u32) -> bool {
    id == super::abi::CRTC_ID
        || id == super::abi::ENCODER_ID
        || id == super::abi::CONNECTOR_ID
        || id == super::abi::PLANE_ID
}

/// `DRM_IOCTL_MODE_LIST_LESSES`: how many lessees this descriptor handed out,
/// and - given room - their lessee ids.
///
/// `struct drm_mode_list_lessees { __u32 count_lessees, __u32 pad, __u64
/// lessees_ptr }`. The count is always rewritten; the array is filled only when
/// the caller brought room for it, which is the same two-call shape
/// `GETRESOURCES` uses. It answers *lessees*, not objects - the objects are
/// `MODE_GET_LEASE`'s question - and a lease descriptor has handed nothing out,
/// which is also the answer a non-lessor gets on Linux.
pub(crate) fn list_lessees(card: &DrmCard, arg: usize) -> isize {
    if card.lease().is_some() {
        // A lease is a lessee, not a lessor: on Linux this is the lessor's file
        // private, and a leased descriptor has never leased anything from
        // itself.
        unsafe { (arg as *mut u32).write_unaligned(0) };
        return 0;
    }
    let ids: Vec<u32> = critical_section::with(|cs| {
        LESSEES
            .borrow(cs)
            .borrow()
            .iter()
            .map(|(lessee, _)| *lessee)
            .collect()
    });
    // The same shared field as `get_lease`'s, so the same order: read the room
    // the caller brought, write the total there is, copy the smaller. (This one
    // read its capacity back *after* writing the answer, which made the
    // comparison true by construction and copied every id the card had handed
    // out into whatever the caller's array happened to be.)
    let room = unsafe { (arg as *const u32).read_unaligned() } as usize;
    unsafe { (arg as *mut u32).write_unaligned(ids.len() as u32) };
    let ptr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let take = core::cmp::min(room, ids.len());
    if take > 0 && ptr != 0 && user_array(ptr, take, 4, true) {
        let out = ptr as *mut u32;
        for (i, id) in ids.iter().take(take).enumerate() {
            unsafe { out.add(i).write_unaligned(*id) };
        }
    }
    0
}

/// `DRM_IOCTL_MODE_REVOKE_LEASE`: the lessor takes a lease back by the lessee
/// id `MODE_CREATE_LEASE` returned.
///
/// `struct drm_mode_revoke_lease { __u32 lessee_id; }` - an id rather than a
/// descriptor, which is what lets a compositor revoke a lease whose fd it has
/// already handed to another process. (The id is the whole reason the number
/// exists: the lessor cannot name the fd, because the fd is the lessee's.)
///
/// Linux marks the lease's file private dead; here the lease's own object list
/// is emptied, which refuses every later request that names an object - the same
/// practical effect for the one card this kernel has.
pub(crate) fn revoke_lease(_card: &DrmCard, caller: u64, arg: usize) -> isize {
    // Only the master may take a lease back: a lease may only be revoked by
    // whoever holds the master that made it.
    if !is_master(caller) {
        return -13; // EACCES
    }
    let lessee = unsafe { (arg as *const u32).read_unaligned() };
    let Some(lease) = critical_section::with(|cs| {
        let mut all = LESSEES.borrow(cs).borrow_mut();
        let at = all.iter().position(|(id, _)| *id == lessee)?;
        Some(all.remove(at).1)
    }) else {
        return -22; // EINVAL: there is no such lessee to take back
    };
    lease.revoke();
    0
}

/// The requests that put pixels on the screen. Everything else the card serves
/// without asking who owns it: enumerating the topology, making and mapping
/// buffers, reading back properties. `RMFB` is here because a framebuffer the
/// CRTC is driving is a framebuffer that cannot be removed, and that is the
/// master's business rather than an allocator's.
const PRESENTATION: &[u32] = &[
    NR_SETCRTC,
    NR_PAGE_FLIP,
    NR_MODE_ATOMIC,
    NR_OBJ_SETPROPERTY,
    NR_RMFB,
    // The cursor is presentation too, and belongs here for the reason the list
    // exists: a master that has lost its console must stop putting pixels on the
    // screen, and a pointer that keeps moving on someone else's screen is pixels.
    // The server drawing the opposite way round - a cloned output whose cursor is
    // handled per-client - is a driver that would not use this list at all.
    NR_MODE_CURSOR,
    NR_MODE_CURSOR2,
];

/// The master has lost its console: stop it driving the scanout, and make the
/// card fd say so.
pub(crate) fn revoke() {
    if REVOKED.swap(true, Ordering::Relaxed) {
        return;
    }
    // Drain what is queued rather than holding it back. A compositor blocked in
    // `poll` on the card fd, waiting for the flip-completion event its own
    // commit promised, is not resumed by a `POLLHUP` it cannot see until the
    // events it already waited for are read: it would sit there with its frames
    // never landing and its session never ending. Empty the queue first and let
    // the hang-up close it.
    critical_section::with(|cs| STATE.borrow(cs).borrow_mut().events.clear());
}

/// The screen moved onto a console that is not the master's: take the
/// presentation away *now*, not on the master's next try.
///
/// `master_ok` refuses a lost master lazily, on the next `SETCRTC`/`PAGE_FLIP` -
/// enough to stop its pixels landing, not enough to tell the compositor it has
/// lost the screen. A compositor that is never told parks quietly in `poll`, and
/// when its console comes back it still believes it holds the card, so it re-draws
/// only on the next input event: the switch to the desktop shows a stale screen
/// until the mouse moves. Linux is not so quiet - the VT move takes the master,
/// the compositor reads the hang-up, releases and re-takes its session, and
/// repaints the moment the console returns. This is that call: revoke on the
/// switch, then wake the parked reader so the `POLLHUP` is seen. The master's
/// recovery - `SET_MASTER` after its `acqsig` - clears the hang-up again (see
/// [`revoked`]), which is what makes the switch *back* repaint without a mouse.
pub fn revoke_now() {
    if !master_held() {
        return;
    }
    // Only a master that has actually lost the screen. A master whose console is
    // still the one on the screen keeps presenting.
    if crate::vfs::vcmux::master_holds_screen() {
        return;
    }
    revoke();
    // The compositor is parked in `poll` on the card fd waiting for a frame that
    // will now never land; wake it so the hang-up closes its wait and it can
    // pause and, on the `acqsig`, come back.
    wake_card_readers();
}

/// The screen has moved back onto the DRM master's console: give it the card
/// again and put its picture back on the scanout, the way Linux's VT switch
/// re-grants the master and the compositor's re-commit re-shows it.
///
/// `revoke_now` stopped the master presenting while its console was away, and
/// the text render filled the screen in its place - so the desktop's pixels are
/// gone from the scanout even though the master's committed framebuffer is still
/// the card's `current_fb` (weston keeps rendering into its dumb buffer whether or
/// not it can present, and only a *mouse* event makes it flip a fresh one). Relying
/// on the compositor to re-commit is what left a stale screen until the mouse moved.
/// So the kernel does the re-present itself, exactly as Linux hands the master back
/// and forces the output to be re-applied on `VT_ACTIVATE`: clear the hang-up, then
/// ask the render task - the only flusher - to copy `current_fb` onto the scanout
/// and flush. The desktop is back on the very frame that switched to it.
pub fn grant_and_resent() {
    if !master_held() {
        return;
    }
    // The console comes back in graphics mode before anything asks whether it
    // is, and that is the whole fix for a card this one cannot drive away from
    // its desktop.
    //
    // The measurement: seatd opens the console, writes `VT_SETMODE` and closes
    // the descriptor - and it writes the mode with **`relsig` and `acqsig` both
    // zero**, which is what a seat manager that watches the console itself
    // rather than waiting to be told looks like. So the handover has no keeper
    // to signal, `release_console` takes Linux's own `reset_vc` road, and the
    // console lands back in `KD_TEXT`. That is Linux's answer too; what Linux
    // does not do is let the word matter afterwards, because there a master
    // drives the screen because it is the master. Here the *veto* the text
    // console paints under is `master_holds_screen()`, and that asks `KD_GRAPHICS`
    // as one of its three facts - so a handover nobody answered cost the
    // compositor its screen for good: the text grid repainted over the desktop
    // and every `MODE_ATOMIC` was answered `EACCES` (which is what a `^A` away
    // and back showed, with `switch to=1 master_vc=Some(1) active=1 kd=0` in
    // the log beside it).
    //
    // So the master's own console is a graphics console again the moment it is
    // the one on the screen and a live master still holds the card. Nobody
    // else's: a master that has let go (`master_held` above), one on another
    // console, and one whose console is not the one being shown all leave the
    // text console its screen, which is what `^A` away from a desktop is.
    if let Some(vc) = crate::vfs::vcmux::master_vc()
        && crate::term::vc::active() == vc
        && crate::vfs::vcmux::kd_mode(vc) != crate::term::vc::KD_GRAPHICS
    {
        crate::vfs::vcmux::set_kd_mode(vc, crate::term::vc::KD_GRAPHICS);
    }
    // Only when the master's console is genuinely back on the screen, in graphics
    // mode - otherwise there is nothing to hand back and the text console keeps it.
    if !crate::vfs::vcmux::master_holds_screen() {
        return;
    }
    // The hang-up was because it lost the console; the console is back.
    REVOKED.store(false, Ordering::Relaxed);
    if critical_section::with(|cs| STATE.borrow(cs).borrow().current_fb) != 0 {
        crate::drivers::video::render::request_re_present();
    }
    // A reader parked in `poll` on the card fd now sees the fd live again, not
    // hang-up, and its next flip will land.
    wake_card_readers();
}

/// Copy the master's current framebuffer onto the scanout. The render task's
/// re-present path (`drivers::video::render::request_re_present`) calls this as
/// the one flusher, then flushes. Unlike a compositor's own flip this is the
/// kernel's act, so it is not gated by `master_ok` - it is what hands the screen
/// back. Returns 0 on success, the `copy_to_scanout` error if there is no frame
/// to show.
pub fn re_present_now() -> isize {
    critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let fb = st.current_fb;
        if fb == 0 { 0 } else { copy_to_scanout(&st, fb) }
    })
}

/// A description closed its last reference: it holds nothing any more, and no
/// token or authentication may name it.
///
/// The removal is unconditional rather than "only if it was the master" because
/// a closed description's address is *reusable*: a token left behind for an
/// `Arc<File>` that has since been freed would one day name a different file,
/// and that is a use-after-free dressed up as an authorisation.
pub fn master_closed(ofd: u64) {
    if ofd == 0 {
        return;
    }
    critical_section::with(|cs| {
        MAGICS
            .borrow(cs)
            .borrow_mut()
            .retain(|&(_, owner)| owner != ofd);
        AUTH_OFD
            .borrow(cs)
            .borrow_mut()
            .retain(|&owner| owner != ofd);
    });
    if MASTER_OFD.load(Ordering::Relaxed) == ofd {
        MASTER_OFD.store(0, Ordering::Relaxed);
        MASTER_PID.store(0, Ordering::Relaxed);
        REVOKED.store(false, Ordering::Relaxed);
        crate::vfs::vcmux::set_master_vc(0);
    }
    // And what it holds by *description* rather than by any object: the sync
    // objects it made, and the `DRM_CLIENT_CAP_ATOMIC` it set. A handle is per
    // description, so a description that is gone has nobody left who could name
    // one - and the objects themselves are not dropped here: a
    // `SYNCOBJ_HANDLE_TO_FD` descriptor that outlived the client still holds the
    // one it names, which is the whole reason that descriptor exists. The cap is
    // the same shape, and forgetting it here is not a leak of pixels but a
    // descriptor number that a *later* file could be given: an `ofd` left in the
    // list would hand some other client the atomic properties it never asked
    // for, which is the use-after-free of `master_closed`'s own header, one
    // table over.
    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        st.syncobjs
            .retain(|rec| rec.as_ref().is_none_or(|rec| rec.owner != ofd));
        st.atomic_ofds.retain(|&owner| owner != ofd);
        st.hotspot_ofds.retain(|&owner| owner != ofd);
    });
    release_framebuffers(ofd);
}

/// The exit path's half of [`master_closed`].
///
/// [`master_closed`] is called from the `close(2)` hook, and a process that ends
/// without closing its card - every `exit_group`, which is what a Rust `main`
/// returning through `std::process::exit` does, and what a killed compositor
/// does - never runs that hook. Its per-description DRM state then outlives it:
/// the master and the console binding `SET_MASTER` took, the client-capability
/// set, the authorisations, the sync objects, the framebuffers. The next
/// description the allocator hands the *same address* - an `ofd` **is** that
/// address (`file.ofd()`), so a reused one is a real thing and not a
/// theoretical one - then inherits a claim it never made. That is exactly the
/// failure `master_closed`'s own header names ("a compositor that is killed ...
/// must not leave the machine holding a screen nobody can drive"), and it is
/// reachable from a program that merely returns: the sweep's `t-drm` sets
/// `DRM_CLIENT_CAP_ATOMIC`, and `t-drm-props` - which opens a *fresh, uncapped*
/// description and asserts it is not shown the atomic properties - read the
/// leaked cap and failed until this ran.
///
/// Registered with `sched::exit` (`src/board/steps.rs`) so the descriptor's
/// subsystem releases its own state rather than the scheduler naming `drivers`,
/// which is the whole point of `sched::exit`'s hook list. `for_each_last` is the
/// same "is this the final reference" rule `close(2)`'s hook applies by hand, so
/// a description a fork still holds is left alone.
pub fn close_at_exit(files: &[Arc<crate::vfs::File>], _owner: u32) {
    crate::vfs::file::for_each_last(files, |file, ofd| {
        if file.inode().as_any().downcast_ref::<DrmCard>().is_some() {
            master_closed(ofd);
        }
    });
}

/// Take a description's framebuffers away with it: Linux's `drm_fb_release`,
/// which `drm_file_free` runs for every framebuffer the closing `struct
/// drm_file` made.
///
/// This is what stops a dead session's ids being *enumerated*. `GETRESOURCES`
/// reports the framebuffers the card has, and a client that asks for one of
/// them gets an id whose owner is long gone - smithay's `DrmDevice` reads the
/// properties of every framebuffer it is handed on the way in, and this card
/// answers `EINVAL` for an object that is not a connector, CRTC or plane
/// (Linux's `!obj->properties`, which is what a framebuffer is). The whole
/// device then fails to open: "Error reading properties on device
/// `/dev/dri/card0` (Invalid argument)", and niri comes up with no output and
/// no way to say why. A framebuffer nobody made is the lie underneath it.
///
/// A framebuffer that was being scanned out is dropped from the state that
/// names it rather than from the pixels: the scanout keeps the last frame -
/// which is what a compositor's exit looks like on a real card too, and what
/// lets the text console take the screen over from here.
pub(crate) fn release_framebuffers(ofd: u64) {
    // What the device has to be told afterwards: the pointer's own resource, and
    // the buffers the going framebuffers held. Gathered under the lock and acted
    // on outside it, because every one of these is a device command and a command
    // parks - parking here would be parking in the close path holding `STATE`.
    let (cursor_died, buffers_gone) = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let leaving: Vec<(u32, u32)> = st
            .fbs
            .iter()
            .filter_map(|f| f.as_ref())
            .filter(|f| f.owner == ofd)
            .map(|f| (f.id, f.handle))
            .collect();
        if leaving.is_empty() {
            return (None, Vec::new());
        }
        st.fbs.retain(|f| f.as_ref().is_none_or(|f| f.owner != ofd));
        if leaving.iter().any(|(id, _)| *id == st.current_fb) {
            st.current_fb = 0;
        }
        // A handle this description released earlier was held alive by these
        // framebuffers alone, so their going is what frees it - the same give-back
        // `RMFB` does, on the other door out.
        let buffers_gone: Vec<BufferGone> = leaving
            .iter()
            .filter_map(|&(_, handle)| drop_fb_hold(&mut st, cs, handle))
            .collect();
        // The cursor framebuffer going means the host pointer is now drawn from a
        // resource whose backing this client no longer owns. Take the binding down
        // and hand back the resource id so the caller can hide + release it *outside*
        // the lock (a device command parks, and parking here would be parking in the
        // close path holding `STATE`).
        let cursor_died = if leaving.iter().any(|(id, _)| *id == st.cursor_fb) {
            st.cursor_fb = 0;
            st.cursor_resource.take()
        } else {
            None
        };
        (cursor_died, buffers_gone)
    });
    // The pointer first, then the buffers: the cursor's binding is the thing that
    // must not be left naming a resource the device has been told to drop.
    if let Some((id, _)) = cursor_died {
        hide_host_cursor(id);
    }
    for gone in buffers_gone {
        tell_buffer_gone(gone);
    }
}

/// Whether the card fd should report hang-up: true from the moment a master
/// loses its console until it either re-takes it (`SET_MASTER` after the
/// `acqsig`, which is weston's own recovery path) or lets go of the descriptor.
pub fn revoked() -> bool {
    REVOKED.load(Ordering::Relaxed)
}

/// The caller's argument for request `nr`: how many bytes the kernel touches,
/// and whether it writes into them. `None` means the request carries no
/// argument at all (or is unknown), so there is nothing to validate.
///
/// The lengths are `sizeof` of the struct `<drm.h>` and `<drm_mode.h>` name for
/// the request - not the count of fields this driver happens to read - because
/// the caller allocated the former, and because a check that only covered the
/// fields in use would let a request whose later fields are read by a future
/// version of this file go unvalidated again.

/// The object ids one lease descriptor may drive, and the flags it was made
/// with.
///
/// `sealed` is the flags the lessor passed, reported back by `GET_LEASE` and
/// `LIST_LESSES`-adjacent queries. It is *recorded, not enforced*: an
/// `IMMUTABLE` lease can still be revoked by its lessor here, and a sealed one
/// still lets its holder do everything the object ids allow. That is the honest
/// shape of the gap - Linux's sealing refuses *plane and mode* changes on the
/// leased object, which is a property set this virtual card does not have, and a
/// kernel that pretended to seal would be refusing requests it cannot explain.
pub struct LeaseSet {
    objects: critical_section::Mutex<RefCell<Vec<u32>>>,
    sealed: u32,
}

impl LeaseSet {
    pub(crate) fn new(objects: Vec<u32>, sealed: u32) -> LeaseSet {
        LeaseSet {
            objects: critical_section::Mutex::new(RefCell::new(objects)),
            sealed,
        }
    }

    /// Whether this lease permits the KMS object `id`.
    pub fn holds(&self, id: u32) -> bool {
        critical_section::with(|cs| self.objects.borrow(cs).borrow().contains(&id))
    }

    /// The object ids, as `MODE_GET_LEASE` and `MODE_LIST_LESSES` report them.
    pub fn objects(&self) -> Vec<u32> {
        critical_section::with(|cs| self.objects.borrow(cs).borrow().clone())
    }

    pub fn sealed(&self) -> u32 {
        self.sealed
    }

    /// `MODE_REVOKE_LEASE`: the lessor takes the lease back, which empties it.
    ///
    /// The descriptor is not torn down - Linux does not do that either, it marks
    /// the lease's file private dead - so a holder that keeps using it finds
    /// every object it names refused, which is the same practical effect on the
    /// one card this kernel has.
    pub(crate) fn revoke(&self) {
        critical_section::with(|cs| self.objects.borrow(cs).borrow_mut().clear());
    }
}
