//! t-drm-lease - a lease is a subset of the card, and nothing more.
//!
//! DRM's lease family (`MODE_CREATE_LEASE` and its three siblings) is how a
//! compositor hands *part* of a display to somebody else: the objects it names
//! become a card descriptor of their own, and everything outside the list is
//! refused on it. wlroots asks for a lease when it reopens a node after a
//! hand-over, which is why `NOTES.md` §7.4 counts these requests as the price of
//! a second compositor family - and why it matters that the answer here is a
//! real subset rather than a second name for the whole card.
//!
//! So the test is about boundaries. A lease made for one CRTC must be able to
//! flip that CRTC and must refuse the card's *other* objects; a lease made from
//! a lease cannot widen what it was given; only the master may mint one; and a
//! revoked lease refuses the object it used to cover, while still leaving the
//! descriptor itself readable.
//!
//! **This program is also where the family's four request words are held to
//! `drm.h`'s literals and its four structs to `drm_mode.h`'s layout - and both
//! halves were wrong before the move, in the same two ways.**
//!
//! The numbers came first and were the worse of the two: the driver carried
//! 0xd3..0xd6, every real caller sends 0xc6..0xc9, and nothing could tell,
//! because the no-std test built its requests from the same four constants the
//! driver matched them with. The two agreed about a number no userspace ever
//! sends. A pin whose expectation comes from the value it is checking agrees
//! with any value at all, so the expectation here is the header's literal,
//! written as the word `drm.h` spells.
//!
//! The layout was the same mistake one layer down, and the move found it twice.
//! The sizes were what this driver's handlers happened to touch (24, 16, 24 and
//! 8) rather than what `drm_mode.h` declares (24, 16, 16 and 4); the create
//! struct's two returned words were in the driver's order - `fd` at 16 - where
//! the header puts `lessee_id` at 16 and `fd` at 20; `MODE_GET_LEASE` named a
//! descriptor number in the struct instead of being issued on the lease
//! descriptor (`drmModeGetLease(fd)` is given the fd `drmModeCreateLease`
//! returned); and `MODE_REVOKE_LEASE` took a descriptor where the header takes
//! the lessee id - which is the whole point of the id, since the lessor cannot
//! name the fd the lessee is holding.
//!
//! **The size then went wrong a second time on the way to being fixed**, and
//! the second attempt is worth more than the first because it looked careful.
//! It wrote 28 bytes for `create_lease`, from a field list that ended in a
//! `__u32 pad` - a field this machine's `<drm_mode.h>` does not declare: the
//! struct has five members, so `sizeof` is 24 and
//! `DRM_IOWR(0xC6, struct drm_mode_create_lease)` is `0xc01864c6`. 28 is not a
//! size that struct can have at all, whatever the kernel version: with a `__u64`
//! first member the alignment is 8, so six members ending in `pad` would round
//! up to 32 and the word would be `0xc02064c6`. A number no alignment can
//! produce is a number that was invented from a remembered field list rather
//! than measured, which is why the row below is what `cc` prints for the header
//! installed on this machine - and why the four words here are literals rather
//! than the same `sizeof` applied twice.
//!
//! Then the leaves: **both list-shaped requests in the family copied without
//! looking at the room the caller brought.** `count_objects` and
//! `count_lessees` are one field doing two jobs - it arrives as the array's
//! length and leaves as the total there is - and both handlers wrote the answer
//! first and read the capacity back afterwards, so the comparison was true by
//! construction and the copy ran at the total's length. Linux reads the
//! capacity, writes the total, and copies the smaller. It is the family's own
//! two-call shape that makes this reachable: the sizing call brings room zero,
//! so a caller that sized then asked with a short array gets its neighbour
//! overwritten. The claims below bring a one-element array to a two-object
//! lease and to a two-lessee card, and name what must *not* have been written.
//!
//! One fixture moves because the second machine made it visible: `KDSETMODE`
//! went to `/dev/tty1` rather than to fd 0. The console the harness hands a
//! program on fd 0 is the serial wire, which is not one of the virtual consoles
//! the KD family is keyed to (`t-vt` measures this), so the request is issued on
//! the console it is about - the one a boot leaves on the screen - and handed
//! back at the end.
//!
//! Not made on a host, for `t-drm-args`'s reason: this card's leases, its master
//! and its one CRTC are this kernel's objects, and a host's own card answers
//! `MODE_CREATE_LEASE` with its own topology.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use mpos_user_tests::{Report, host_run};
use rustix::fs::OFlags;
use rustix::io::Errno;

/// `_IOWR('d', nr, size)` as libdrm builds it.
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}
/// `_IO('d', nr)` - no argument at all, which is how the master requests are
/// spelled and why their third ioctl argument is a number rather than a pointer.
const fn io_d(nr: u32) -> u64 {
    (nr as u64) | ((b'd' as u64) << 8)
}
/// `_IO('K', nr)`: the console's own family, whose requests carry their value in
/// the argument slot rather than behind a pointer.
const fn io_k(nr: u32) -> u64 {
    (nr as u64) | ((b'K' as u64) << 8)
}

const NR_GETRESOURCES: u32 = 0xa0;
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_ADDFB: u32 = 0xae;
const NR_PAGE_FLIP: u32 = 0xb0;
const NR_SET_MASTER: u32 = 0x1e;
const NR_DROP_MASTER: u32 = 0x1f;
const NR_MODE_CREATE_LEASE: u32 = 0xc6;
const NR_MODE_LIST_LESSES: u32 = 0xc7;
const NR_MODE_GET_LEASE: u32 = 0xc8;
const NR_MODE_REVOKE_LEASE: u32 = 0xc9;
const NR_KDSETMODE: u32 = 0x3a;

// The four lease request words, as `<drm.h>` spells them - `sizeof` and all.
// Literals rather than `ioc_d` applied to the constants above: the point of the
// claim is that the two agree, and a computed expectation agrees with anything.
// (They are `DRM_IOWR(0xC6, struct drm_mode_create_lease)` and its three
// siblings, as `cc` prints them for the headers on this machine.)
const CREATE_LEASE_WORD: u64 = 0xc018_64c6;
const LIST_LESSES_WORD: u64 = 0xc010_64c7;
const GET_LEASE_WORD: u64 = 0xc010_64c8;
const REVOKE_LEASE_WORD: u64 = 0xc004_64c9;

/// `DRM_MODE_PAGE_FLIP_EVENT`.
const PAGE_FLIP_EVENT: u32 = 0x1;
/// `O_CLOEXEC`, which is also `DRM_CLOEXEC` for a leased descriptor.
const CLOEXEC: u32 = 0o2000000;
/// `KD_GRAPHICS` and `KD_TEXT`: the screen is the compositor's, or the console's.
const KD_GRAPHICS: u64 = 1;
const KD_TEXT: u64 = 0;
/// Filled into every array slot the kernel is not supposed to touch, so an
/// overrun is a read of this value rather than a zero that looks like success.
const POISON: u32 = 0xdead_beef;

// --- the records, spelled as `drm_mode.h` lays them out ----------------------

/// `struct drm_mode_card_res`, so the object ids are asked for rather than
/// assumed - a lease names ids, and an id written down here would be the same
/// agreement-with-itself the request numbers were.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// `struct drm_mode_create_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_mode_fb_cmd`: the framebuffer a flip names.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

/// `struct drm_mode_create_lease` - **24 bytes, five members, no trailing
/// `pad`**, with the two returned words in the header's order (`lessee_id` at
/// 16, `fd` at 20).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateLease {
    object_ids: u64,
    object_count: u32,
    flags: u32,
    lessee_id: u32,
    fd: u32,
}

/// `struct drm_mode_get_lease`, asked on the *lease* descriptor.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetLease {
    count_objects: u32,
    pad: u32,
    objects_ptr: u64,
}

/// `struct drm_mode_list_lessees`, asked on the master.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ListLessees {
    count_lessees: u32,
    pad: u32,
    lessees_ptr: u64,
}

/// `struct drm_mode_revoke_lease`: one id, which is the point of it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RevokeLease {
    lessee_id: u32,
}

/// `struct drm_mode_crtc_page_flip`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

const _: () = assert!(core::mem::size_of::<CardRes>() == 64);
const _: () = assert!(core::mem::offset_of!(CardRes, crtc_id_ptr) == 8);
const _: () = assert!(core::mem::offset_of!(CardRes, connector_id_ptr) == 16);
const _: () = assert!(core::mem::offset_of!(CardRes, count_crtcs) == 36);
const _: () = assert!(core::mem::offset_of!(CardRes, count_connectors) == 40);
const _: () = assert!(core::mem::size_of::<CreateDumb>() == 32);
const _: () = assert!(core::mem::offset_of!(CreateDumb, handle) == 16);
const _: () = assert!(core::mem::size_of::<FbCmd>() == 28);
const _: () = assert!(core::mem::offset_of!(FbCmd, fb_id) == 0);
const _: () = assert!(core::mem::offset_of!(FbCmd, handle) == 24);
const _: () = assert!(core::mem::size_of::<CreateLease>() == 24);
const _: () = assert!(core::mem::offset_of!(CreateLease, object_ids) == 0);
const _: () = assert!(core::mem::offset_of!(CreateLease, object_count) == 8);
const _: () = assert!(core::mem::offset_of!(CreateLease, flags) == 12);
const _: () = assert!(core::mem::offset_of!(CreateLease, lessee_id) == 16);
const _: () = assert!(core::mem::offset_of!(CreateLease, fd) == 20);
const _: () = assert!(core::mem::size_of::<GetLease>() == 16);
const _: () = assert!(core::mem::offset_of!(GetLease, count_objects) == 0);
const _: () = assert!(core::mem::offset_of!(GetLease, objects_ptr) == 8);
const _: () = assert!(core::mem::size_of::<ListLessees>() == 16);
const _: () = assert!(core::mem::offset_of!(ListLessees, count_lessees) == 0);
const _: () = assert!(core::mem::offset_of!(ListLessees, lessees_ptr) == 8);
const _: () = assert!(core::mem::size_of::<RevokeLease>() == 4);
const _: () = assert!(core::mem::size_of::<PageFlip>() == 24);
const _: () = assert!(core::mem::offset_of!(PageFlip, crtc_id) == 0);
const _: () = assert!(core::mem::offset_of!(PageFlip, fb_id) == 4);
const _: () = assert!(core::mem::offset_of!(PageFlip, flags) == 8);

/// One `ioctl`, answering the negative errno a raw syscall hands back.
///
/// Through `libc::syscall` rather than `libc::ioctl`, for `t-drm-args`'s reason:
/// the request is an `unsigned long` word and `ioctl`'s binding of it is not the
/// same width on both libcs. The number comes from `libc::SYS_ioctl`, the
/// maintained per-machine table.
///
/// # Safety
/// `arg` must be null, a value the request takes by value (the console family's
/// two), or an address the caller owns that is at least as long as the size the
/// request word names.
unsafe fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd as libc::c_long, request, arg) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// The errno a refusal answers, as the negative value the door hands back.
fn refused(err: Errno) -> isize {
    -(err.raw_os_error() as isize)
}

/// A claim that a request was refused with a particular errno. Failure is named
/// rather than silent, so a run that goes red says which door and with what.
fn check_refused(r: &mut Report, name: &str, got: isize, want: Errno) {
    if got == refused(want) {
        r.check(name, true);
    } else {
        r.check(
            &format!("{name} (got {got}, want {})", refused(want)),
            false,
        );
    }
}

/// A claim that a request succeeded.
fn check_ok(r: &mut Report, name: &str, got: isize) {
    if got >= 0 {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got})"), false);
    }
}

/// A fixture the rest of the program is built on: if it did not hold there is
/// nothing to lease, so the program says which one and stops.
fn need(r: &mut Report, name: &str, got: isize) {
    if got < 0 {
        r.check(&format!("{name} (got {got})"), false);
        println!("FAILED");
        std::process::exit(1);
    }
}

/// The same, for a value the kernel handed back that the rest of the program
/// would be meaningless without.
fn take<T>(r: &mut Report, name: &str, got: Result<T, Errno>) -> T {
    match got {
        Ok(value) => {
            r.check(name, true);
            value
        }
        Err(err) => {
            r.check(&format!("{name} (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    }
}

fn main() {
    println!("t-drm-lease");
    let mut r = Report::new();

    // A hosted run measures a different card's leases: see the header.
    if host_run() {
        println!(
            "note: not made on a host - this card's master, its one CRTC and the leases off it \
             are this kernel's objects, and a host's own /dev/dri/card0 answers \
             MODE_CREATE_LEASE with its own topology"
        );
        r.finish();
    }

    // --- the four request words, against `drm.h`'s literals ---
    //
    // The family was dead on arrival the first time: the driver matched numbers
    // no caller sends, and the test agreed with the driver. So the expectation
    // is the header's word, written out, and the builder is what is checked.
    for (name, got, want) in [
        (
            "MODE_CREATE_LEASE is drm.h's 0xc6 with a 24-byte argument",
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            CREATE_LEASE_WORD,
        ),
        (
            "MODE_LIST_LESSES is 0xc7 with a 16-byte argument",
            ioc_d(NR_MODE_LIST_LESSES, 16),
            LIST_LESSES_WORD,
        ),
        (
            "MODE_GET_LEASE is 0xc8 with a 16-byte argument",
            ioc_d(NR_MODE_GET_LEASE, 16),
            GET_LEASE_WORD,
        ),
        (
            "MODE_REVOKE_LEASE is 0xc9 with a 4-byte argument",
            ioc_d(NR_MODE_REVOKE_LEASE, 4),
            REVOKE_LEASE_WORD,
        ),
    ] {
        r.check(&format!("{name} (got {got:#x})"), got == want);
    }

    let card = take(
        &mut r,
        "/dev/dri/card0 opens",
        rustix::fs::open("/dev/dri/card0", OFlags::RDWR, rustix::fs::Mode::empty()),
    );
    let card_fd = card.as_raw_fd();

    // --- the card's own console, taken into graphics mode first ---
    //
    // What seatd does for weston. `/dev/tty1` rather than fd 0: the console the
    // harness hands a program is the serial wire, and the KD family is keyed to
    // a virtual console's number (`t-vt` measures this) - so the request goes to
    // the console it is about, the one a boot leaves on the screen. It is handed
    // back at the end, the way every program that borrows the screen does.
    let tty = take(
        &mut r,
        "/dev/tty1 opens",
        rustix::fs::open("/dev/tty1", OFlags::RDWR, rustix::fs::Mode::empty()),
    );
    let got = unsafe {
        ioctl(
            tty.as_raw_fd(),
            io_k(NR_KDSETMODE),
            KD_GRAPHICS as *mut c_void,
        )
    };
    need(
        &mut r,
        "KDSETMODE(KD_GRAPHICS) on the session's console",
        got,
    );
    r.check("KDSETMODE(KD_GRAPHICS) on the session's console", true);

    // --- the objects, read rather than hard-coded ---
    let mut crtcs = [0u32; 4];
    let mut conns = [0u32; 4];
    let mut res = CardRes {
        crtc_id_ptr: crtcs.as_mut_ptr() as u64,
        connector_id_ptr: conns.as_mut_ptr() as u64,
        count_crtcs: crtcs.len() as u32,
        count_connectors: conns.len() as u32,
        ..CardRes::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut res) as *mut c_void,
        )
    };
    need(&mut r, "GETRESOURCES succeeds", got);
    r.check("GETRESOURCES succeeds", true);
    let crtc = crtcs[0];
    let connector = conns[0];
    r.check(&format!("a CRTC is named (got {crtc})"), crtc != 0);
    r.check(
        &format!("and a connector is named (got {connector})"),
        connector != 0 && connector != crtc,
    );
    if crtc == 0 || connector == 0 {
        let _ = unsafe { ioctl(tty.as_raw_fd(), io_k(NR_KDSETMODE), KD_TEXT as *mut c_void) };
        r.finish();
    }

    // --- a buffer to flip, because PAGE_FLIP names a real framebuffer ---
    let mut dumb = CreateDumb {
        height: 64,
        width: 64,
        bpp: 32,
        ..CreateDumb::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_CREATE_DUMB, 32),
            (&raw mut dumb) as *mut c_void,
        )
    };
    need(&mut r, "CREATE_DUMB succeeds", got);
    r.check("CREATE_DUMB succeeds", true);
    let mut fb = FbCmd {
        width: 64,
        height: 64,
        pitch: 256,
        bpp: 32,
        depth: 24,
        handle: dumb.handle,
        ..FbCmd::default()
    };
    let got = unsafe { ioctl(card_fd, ioc_d(NR_ADDFB, 28), (&raw mut fb) as *mut c_void) };
    need(&mut r, "ADDFB succeeds", got);
    r.check("ADDFB succeeds", true);
    let fb_id = fb.fb_id;
    r.check(
        &format!("a framebuffer id is returned (got {fb_id})"),
        fb_id != 0,
    );
    if fb_id == 0 {
        let _ = unsafe { ioctl(tty.as_raw_fd(), io_k(NR_KDSETMODE), KD_TEXT as *mut c_void) };
        r.finish();
    }

    // --- only the master may mint one ---
    //
    // Not a courtesy: a lease *is* a claim on the screen, and an unauthorised
    // descriptor handing one out would be a way to take the display without ever
    // asking for the card.
    let mut objects = [crtc, connector, 0, 0];
    let mut lease = CreateLease {
        object_ids: objects.as_mut_ptr() as u64,
        object_count: 2,
        flags: CLOEXEC,
        ..CreateLease::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            (&raw mut lease) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "a non-master cannot create a lease",
        got,
        Errno::ACCESS,
    );
    let got = unsafe { ioctl(card_fd, io_d(NR_SET_MASTER), core::ptr::null_mut()) };
    need(&mut r, "SET_MASTER succeeds", got);
    r.check("SET_MASTER succeeds", true);
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            (&raw mut lease) as *mut c_void,
        )
    };
    check_ok(&mut r, "the master creates a lease", got);
    let lessee = lease.lessee_id;
    let lease_fd = lease.fd as i32;
    r.check(
        &format!("the lease arrives as a descriptor (got {lease_fd})"),
        lease_fd >= 0,
    );
    r.check(
        &format!("and is named by a lessee id (got {lessee})"),
        lessee != 0,
    );
    // Owned from here, so an early exit closes it rather than leaking a held
    // lease into the next test in the sweep.
    let lease1 = if lease_fd >= 0 {
        Some(unsafe { OwnedFd::from_raw_fd(lease_fd) })
    } else {
        None
    };
    // DRM_CLOEXEC reached the descriptor, as O_CLOEXEC would.
    if let Some(leased) = &lease1 {
        match rustix::io::fcntl_getfd(leased) {
            Ok(flags) => r.check(
                "DRM_CLOEXEC marked the leased descriptor close-on-exec",
                flags.contains(rustix::io::FdFlags::CLOEXEC),
            ),
            Err(e) => r.check(&format!("F_GETFD on the lease answers ({e:?})"), false),
        }
    }

    // --- an object the card does not have is not leaseable ---
    let mut junk = [0x1234u32; 1];
    let mut bad = CreateLease {
        object_ids: junk.as_mut_ptr() as u64,
        object_count: 1,
        ..CreateLease::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            (&raw mut bad) as *mut c_void,
        )
    };
    check_refused(&mut r, "an unknown object is refused", got, Errno::INVAL);
    let mut none = CreateLease {
        object_ids: junk.as_mut_ptr() as u64,
        object_count: 0,
        ..CreateLease::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            (&raw mut none) as *mut c_void,
        )
    };
    check_refused(&mut r, "an empty lease is refused", got, Errno::INVAL);

    // A second lease, over the connector alone. It is here for two reasons: it
    // makes the lessee list two long, which is what lets the clamp below be
    // asked at all, and it is the lease whose CRTC is *outside* it - the shape
    // a lease of a second head has in a compositor that still owns the first.
    let mut conn_only = [connector];
    let mut second = CreateLease {
        object_ids: conn_only.as_mut_ptr() as u64,
        object_count: 1,
        ..CreateLease::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            (&raw mut second) as *mut c_void,
        )
    };
    check_ok(&mut r, "a second lease over the connector alone", got);
    let lessee2 = second.lessee_id;
    let lease2 = if second.fd as i32 >= 0 {
        Some(unsafe { OwnedFd::from_raw_fd(second.fd as i32) })
    } else {
        None
    };
    r.check(
        &format!("and is a different lessee (got {lessee} and {lessee2})"),
        lessee2 != 0 && lessee2 != lessee,
    );

    if let (Some(lease1), Some(lease2)) = (&lease1, &lease2) {
        // --- LIST_LESSES: the master names who it leased to ---
        //
        // Two calls, the shape every KMS list uses: the sizing call brings no
        // room and hears the count, the second brings an array to fill.
        let mut list = ListLessees::default();
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_LIST_LESSES, 16),
                (&raw mut list) as *mut c_void,
            )
        };
        check_ok(&mut r, "LIST_LESSES answers the sizing call", got);
        let count = list.count_lessees;
        r.check(
            &format!("and reports both lessees (got {count})"),
            count == 2,
        );
        // Now the clamp: one element of room, two lessees to name. The count is
        // still the total - that is the field's other job - and the slot the
        // caller never offered must be exactly as it was left.
        let mut lessees = [POISON; 8];
        let mut list = ListLessees {
            count_lessees: 1,
            lessees_ptr: lessees.as_mut_ptr() as u64,
            ..ListLessees::default()
        };
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_LIST_LESSES, 16),
                (&raw mut list) as *mut c_void,
            )
        };
        check_ok(&mut r, "LIST_LESSES fills a one-element array", got);
        r.check(
            &format!(
                "LIST_LESSES still reports the total (got {}, want 2)",
                list.count_lessees
            ),
            list.count_lessees == 2,
        );
        r.check(
            &format!("and names one of them (got {:#x})", lessees[0]),
            (lessees[0] == lessee || lessees[0] == lessee2) && lessees[0] != POISON,
        );
        r.check(
            &format!(
                "without writing past the room it was given (got {:#x})",
                lessees[1]
            ),
            lessees[1] == POISON,
        );
        // A lease is a lessee, not a lessor: it has handed nothing out.
        let mut none_here = ListLessees::default();
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_LIST_LESSES, 16),
                (&raw mut none_here) as *mut c_void,
            )
        };
        check_ok(&mut r, "LIST_LESSES answers a lease descriptor", got);
        r.check(
            &format!(
                "and reports that a lease has handed nothing out (got {})",
                none_here.count_lessees
            ),
            none_here.count_lessees == 0,
        );

        // --- GET_LEASE, asked on the lease descriptor ---
        //
        // The request `drmModeGetLease` makes: it is given the fd
        // `drmModeCreateLease` returned, and it answers that lease's objects. It
        // is also the request a *lessor's* own descriptor has no answer to,
        // which is the claim underneath this one.
        let mut get = GetLease::default();
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut get) as *mut c_void,
            )
        };
        check_ok(&mut r, "GET_LEASE answers the sizing call", got);
        r.check(
            &format!("and reports both objects (got {})", get.count_objects),
            get.count_objects == 2,
        );
        // The same clamp, on the other list: one element of room, two objects.
        let mut ids = [POISON; 4];
        let mut get = GetLease {
            count_objects: 1,
            objects_ptr: ids.as_mut_ptr() as u64,
            ..GetLease::default()
        };
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut get) as *mut c_void,
            )
        };
        check_ok(&mut r, "GET_LEASE fills a one-element array", got);
        r.check(
            &format!(
                "GET_LEASE still reports the total (got {}, want 2)",
                get.count_objects
            ),
            get.count_objects == 2,
        );
        r.check(
            &format!("and names an object of the lease (got {:#x})", ids[0]),
            (ids[0] == crtc || ids[0] == connector) && ids[0] != POISON,
        );
        r.check(
            &format!(
                "without writing past the room it was given (got {:#x})",
                ids[1]
            ),
            ids[1] == POISON,
        );
        // With room for both, both arrive.
        let mut both = [POISON; 4];
        let mut get = GetLease {
            count_objects: both.len() as u32,
            objects_ptr: both.as_mut_ptr() as u64,
            ..GetLease::default()
        };
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut get) as *mut c_void,
            )
        };
        check_ok(&mut r, "GET_LEASE fills a two-element array", got);
        r.check(
            &format!("with the CRTC it was made for (got {:?})", &both[..2]),
            both[..2].contains(&crtc) && both[..2].contains(&connector),
        );
        // ...and the master's own descriptor is not a lease at all.
        let mut master_get = GetLease::default();
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut master_get) as *mut c_void,
            )
        };
        check_refused(
            &mut r,
            "GET_LEASE on the lessor's own descriptor is EINVAL",
            got,
            Errno::INVAL,
        );

        // --- a flip through the lease is its whole purpose ---
        let mut flip = PageFlip {
            crtc_id: crtc,
            fb_id,
            flags: PAGE_FLIP_EVENT,
            ..PageFlip::default()
        };
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_PAGE_FLIP, 24),
                (&raw mut flip) as *mut c_void,
            )
        };
        check_ok(&mut r, "the lease may flip its own CRTC", got);
        // ...and the object *outside* the second lease is refused on it, which
        // is the boundary the whole family exists to draw.
        let got = unsafe {
            ioctl(
                lease2.as_raw_fd(),
                ioc_d(NR_PAGE_FLIP, 24),
                (&raw mut flip) as *mut c_void,
            )
        };
        check_refused(
            &mut r,
            "an object outside the lease is refused",
            got,
            Errno::INVAL,
        );

        // A lease cannot widen itself: the new lease's objects must be a subset
        // of what the caller holds, which is the family's safety property. (A
        // lease descriptor is not a master either, so the refusal is doubled.)
        let mut outer = [crtc];
        let mut sub = CreateLease {
            object_ids: outer.as_mut_ptr() as u64,
            object_count: 1,
            ..CreateLease::default()
        };
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_CREATE_LEASE, 24),
                (&raw mut sub) as *mut c_void,
            )
        };
        check_refused(
            &mut r,
            "a lease cannot lease out what it was not given",
            got,
            Errno::ACCESS,
        );

        // Take it back, by the lessee id rather than by a descriptor - which is
        // what lets a compositor revoke a lease whose fd it has handed away.
        let mut rev = RevokeLease { lessee_id: lessee };
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_REVOKE_LEASE, 4),
                (&raw mut rev) as *mut c_void,
            )
        };
        check_ok(&mut r, "the master revokes the lease", got);
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_PAGE_FLIP, 24),
                (&raw mut flip) as *mut c_void,
            )
        };
        check_refused(&mut r, "a revoked lease flips nothing", got, Errno::INVAL);
        // The descriptor survives - Linux does not close a revoked lease out from
        // under its holder - but the object no longer belongs to anybody through
        // it.
        let mut after = GetLease::default();
        let got = unsafe {
            ioctl(
                lease1.as_raw_fd(),
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut after) as *mut c_void,
            )
        };
        check_ok(&mut r, "GET_LEASE still answers a revoked lease", got);
        r.check(
            &format!("and reports it as empty (got {})", after.count_objects),
            after.count_objects == 0,
        );
        // The id does not name a lease any more either.
        let mut again = RevokeLease { lessee_id: lessee };
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_REVOKE_LEASE, 4),
                (&raw mut again) as *mut c_void,
            )
        };
        check_refused(&mut r, "and revoking it twice is EINVAL", got, Errno::INVAL);
        r.check(
            &format!(
                "the lessee list is one shorter (got {})",
                list_after_revoke(card_fd)
            ),
            list_after_revoke(card_fd) == 1,
        );
        // The second lease is still the second lease: a revoke is of one id, and
        // its neighbour's objects are untouched.
        let mut still = GetLease::default();
        let got = unsafe {
            ioctl(
                lease2.as_raw_fd(),
                ioc_d(NR_MODE_GET_LEASE, 16),
                (&raw mut still) as *mut c_void,
            )
        };
        check_ok(&mut r, "GET_LEASE answers the other lease", got);
        r.check(
            &format!(
                "and still names its connector (got {})",
                still.count_objects
            ),
            still.count_objects == 1,
        );

        // Take the second one back too. Not a claim about the family so much as
        // about the machine the next test in the sweep inherits: a lease this
        // program leaves behind is a lessee the card is still holding, and
        // `LIST_LESSES` is a *global* count on this card - so a run that exits
        // holding one would move the number under whatever asks next.
        let mut rev2 = RevokeLease { lessee_id: lessee2 };
        let got = unsafe {
            ioctl(
                card_fd,
                ioc_d(NR_MODE_REVOKE_LEASE, 4),
                (&raw mut rev2) as *mut c_void,
            )
        };
        check_ok(&mut r, "the master revokes the second lease", got);
        r.check(
            &format!(
                "and the card is holding nothing out any more (got {})",
                list_after_revoke(card_fd)
            ),
            list_after_revoke(card_fd) == 0,
        );
    }

    // Hand the console back, the way every program that borrows the screen does.
    let got = unsafe { ioctl(tty.as_raw_fd(), io_k(NR_KDSETMODE), KD_TEXT as *mut c_void) };
    check_ok(&mut r, "KDSETMODE(KD_TEXT) restores", got);
    let got = unsafe { ioctl(card_fd, io_d(NR_DROP_MASTER), core::ptr::null_mut()) };
    check_ok(&mut r, "DROP_MASTER releases the card", got);

    r.finish();
}

/// How many lessees the master is holding now, as `LIST_LESSES`'s sizing call
/// counts them. A revoked lease is one the registry has dropped, and the count
/// is the only place outside `REVOKE_LEASE` itself that says so.
fn list_after_revoke(card_fd: i32) -> u32 {
    let mut list = ListLessees::default();
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MODE_LIST_LESSES, 16),
            (&raw mut list) as *mut c_void,
        )
    };
    if got < 0 {
        u32::MAX
    } else {
        list.count_lessees
    }
}
