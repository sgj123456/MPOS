//! t-drm-lease - a lease is a subset of the card, and nothing more.
//!
//! DRM's lease family (`MODE_CREATE_LEASE` and its three siblings) is how a
//! compositor hands *part* of a display to somebody else: the objects it names
//! become a card descriptor of their own, and everything outside the list is
//! refused on it. wlroots asks for a lease when it reopens a node after a
//! hand-over, which is why `PLAN.md` §7.4 counts these requests as the price of
//! a second compositor family - and why it matters that the answer here is a
//! real subset rather than a second name for the whole card.
//!
//! So the test is about boundaries. A lease made for one CRTC must be able to
//! flip that CRTC and must refuse the card's *other* objects; a lease made from
//! a lease cannot widen what it was given; only the master may mint one; and a
//! revoked lease refuses the object it used to cover, while still leaving the
//! descriptor itself readable.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `_IOWR('d', nr, size)` as libdrm builds it: the family byte at bits 8..15.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}
/// `_IO('d', nr)` - no argument at all, which is how the master requests are
/// spelled and why their third ioctl argument is a number rather than a pointer.
const fn io_d(nr: usize) -> usize {
    (nr) | ((b'd' as usize) << 8)
}

const NR_GETRESOURCES: usize = 0xa0;
const NR_CREATE_DUMB: usize = 0xb2;
const NR_ADDFB: usize = 0xae;
const NR_PAGE_FLIP: usize = 0xb0;
const NR_MODE_CREATE_LEASE: usize = 0xc6;
const NR_MODE_LIST_LESSES: usize = 0xc7;
const NR_MODE_REVOKE_LEASE: usize = 0xc9;
const NR_MODE_GET_LEASE: usize = 0xc8;
const NR_SET_MASTER: usize = 0x1e;
const NR_DROP_MASTER: usize = 0x1f;

/// `DRM_MODE_PAGE_FLIP_EVENT`.
const PAGE_FLIP_EVENT: u32 = 0x1;
/// `O_CLOEXEC`, which is also `DRM_CLOEXEC` for a leased descriptor.
const CLOEXEC: u32 = 0o2000000;

const EINVAL: isize = -22;
const EACCES: isize = -13;
const O_RDWR: usize = 2;
/// `struct drm_mode_resfcn`'s `fb_id` of 0 means "disable", so a flip that names
/// no buffer is the cheapest legal commit to ask for.
const FB_NONE: u32 = 0;

fn put32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn get32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn put64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}
fn get64(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

/// `KDSETMODE` / `KDGETMODE`'s family byte and numbers.
const KD: u8 = b'K';
const NR_KDSETMODE: usize = 0x3a;
const KD_GRAPHICS: usize = 1;
const KD_TEXT: usize = 0;

const fn io_k(nr: usize) -> usize {
    nr | ((KD as usize) << 8)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-drm-lease\n");
    let mut failed = false;

    // The four request numbers, against `<drm.h>`'s own, written out here as
    // literals rather than read from the constant beside them.
    //
    // This is the whole reason the family was dead on arrival. The driver
    // carried 0xd3..0xd6, every real caller sends 0xc6..0xc9, and nothing here
    // could tell: the test built its requests from the same four constants the
    // driver matched them with, so the two agreed about a number that no
    // userspace ever sends - `drmModeCreateLease` got `ENOTTY` from the
    // dispatch catch-all, while this program passed. A pin whose expectation
    // comes from the value it is checking agrees with any value at all, so the
    // expectation is the header's literal and the exercise below is an
    // exercise rather than the contract.
    lin::check_eq(
        &mut failed,
        b"MODE_CREATE_LEASE is drm.h's 0xc6",
        ioc_d(NR_MODE_CREATE_LEASE, 24) as isize,
        ioc_d(0xc6, 24) as isize,
    );
    lin::check_eq(
        &mut failed,
        b"MODE_LIST_LESSEES is drm.h's 0xc7",
        ioc_d(NR_MODE_LIST_LESSES, 16) as isize,
        ioc_d(0xc7, 16) as isize,
    );
    lin::check_eq(
        &mut failed,
        b"MODE_GET_LEASE is drm.h's 0xc8",
        ioc_d(NR_MODE_GET_LEASE, 24) as isize,
        ioc_d(0xc8, 24) as isize,
    );
    lin::check_eq(
        &mut failed,
        b"MODE_REVOKE_LEASE is drm.h's 0xc9",
        ioc_d(NR_MODE_REVOKE_LEASE, 8) as isize,
        ioc_d(0xc9, 8) as isize,
    );

    let card = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", card >= 0);
    if card < 0 {
        lin::done(failed);
    }

    // The card's own console, taken into graphics mode first. This is what
    // seatd does for weston, and it is what the flip gate below measures: a
    // master whose console is *not* the screen being drawn is refused, so a
    // test that wants its flips to land has to hold the screen honestly.
    lin::check_eq(
        &mut failed,
        b"KDSETMODE(KD_GRAPHICS) on the session's tty",
        lin::ioctl(0, io_k(NR_KDSETMODE), KD_GRAPHICS),
        0,
    );

    // The CRTC id, read rather than hard-coded: it is the object this card has
    // to lease, and a test that assumed the number would be checking its own
    // copy of the driver's constants. `drm_mode_card_res` puts the crtc pointer
    // at 8 and its count at 36.
    let mut crtcs = [0u32; 4];
    let mut res = [0u8; 64];
    put64(&mut res, 8, crtcs.as_mut_ptr() as u64);
    put32(&mut res, 36, 4);
    let r = lin::ioctl(card, ioc_d(NR_GETRESOURCES, 64), res.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETRESOURCES succeeds", r, 0);
    let crtc = crtcs[0];
    lin::check_true(&mut failed, b"a CRTC is named", crtc != 0);

    // A buffer to flip, because `PAGE_FLIP` names a real framebuffer.
    let mut dumb = [0u8; 32];
    put32(&mut dumb, 0, 64); // height
    put32(&mut dumb, 4, 64); // width
    put32(&mut dumb, 8, 32); // bpp
    let r = lin::ioctl(card, ioc_d(NR_CREATE_DUMB, 32), dumb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"CREATE_DUMB succeeds", r, 0);
    let handle = get32(&dumb, 16);
    let mut fb = [0u8; 28];
    put32(&mut fb, 4, 64); // width
    put32(&mut fb, 8, 64); // height
    put32(&mut fb, 12, 256); // pitch
    put32(&mut fb, 16, 32); // bpp
    put32(&mut fb, 20, 24); // depth
    put32(&mut fb, 24, handle);
    let r = lin::ioctl(card, ioc_d(NR_ADDFB, 28), fb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"ADDFB succeeds", r, 0);
    let fb_id = get32(&fb, 0);
    lin::check_true(&mut failed, b"a framebuffer id is returned", fb_id != 0);

    // A lease may only be minted by the master: it is a claim on the screen, and
    // an unauthorised descriptor handing one out would be a way to take the
    // display without ever asking for the card.
    let mut objects = [crtc, 0, 0, 0];
    let mut lease = [0u8; 24];
    put64(&mut lease, 0, objects.as_mut_ptr() as u64);
    put32(&mut lease, 8, 1); // object_count
    put32(&mut lease, 12, CLOEXEC); // flags
    lin::check_eq(
        &mut failed,
        b"a non-master cannot create a lease",
        lin::ioctl(
            card,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            lease.as_mut_ptr() as usize,
        ),
        EACCES,
    );
    lin::check_eq(
        &mut failed,
        b"SET_MASTER succeeds",
        lin::ioctl(card, io_d(NR_SET_MASTER), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the master creates a lease",
        lin::ioctl(
            card,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            lease.as_mut_ptr() as usize,
        ),
        0,
    );
    let fd = get32(&lease, 16) as i32;
    lin::check_true(&mut failed, b"the lease arrives as a descriptor", fd >= 0);

    // An object the card does not have is not leaseable, and neither is a
    // request that names no objects at all.
    let mut junk = [0x1234u32; 1];
    let mut bad = [0u8; 24];
    put64(&mut bad, 0, junk.as_mut_ptr() as u64);
    put32(&mut bad, 8, 1);
    lin::check_eq(
        &mut failed,
        b"an unknown object is refused",
        lin::ioctl(
            card,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            bad.as_mut_ptr() as usize,
        ),
        EINVAL,
    );
    let mut none = [0u8; 24];
    put64(&mut none, 0, junk.as_mut_ptr() as u64);
    put32(&mut none, 8, 0);
    lin::check_eq(
        &mut failed,
        b"an empty lease is refused",
        lin::ioctl(
            card,
            ioc_d(NR_MODE_CREATE_LEASE, 24),
            none.as_mut_ptr() as usize,
        ),
        EINVAL,
    );

    if fd >= 0 {
        // The lease reads back as the object it was made for.
        let mut ids = [0u32; 4];
        let mut get = [0u8; 24];
        put64(&mut get, 8, ids.as_mut_ptr() as u64);
        put32(&mut get, 16, fd as u32);
        let r = lin::ioctl(
            card,
            ioc_d(NR_MODE_GET_LEASE, 24),
            get.as_mut_ptr() as usize,
        );
        lin::check_eq(&mut failed, b"GET_LEASE succeeds", r, 0);
        lin::check_eq(
            &mut failed,
            b"GET_LEASE reports one object",
            get32(&get, 0) as isize,
            1,
        );
        lin::check_eq(&mut failed, b"and names it", ids[0] as isize, crtc as isize);

        // A flip through the lease is its whole purpose.
        let mut flip = [0u8; 24];
        put32(&mut flip, 0, crtc);
        put32(&mut flip, 4, fb_id);
        put32(&mut flip, 8, PAGE_FLIP_EVENT);
        lin::check_eq(
            &mut failed,
            b"the lease may flip its own CRTC",
            lin::ioctl(fd, ioc_d(NR_PAGE_FLIP, 24), flip.as_mut_ptr() as usize),
            0,
        );
        // ...and an object outside the lease is refused on the lease descriptor.
        put32(&mut flip, 0, crtc.wrapping_add(1));
        lin::check_eq(
            &mut failed,
            b"an object outside the lease is refused",
            lin::ioctl(fd, ioc_d(NR_PAGE_FLIP, 24), flip.as_mut_ptr() as usize),
            EINVAL,
        );
        put32(&mut flip, 0, crtc);

        // A lease cannot widen itself: the new lease's objects must be a subset
        // of what the caller holds, which is the family's safety property. (The
        // id named here is not even one the card has, so the refusal is doubled
        // - and a lease descriptor is not a master either.)
        let mut outer = [crtc + 1];
        let mut sub = [0u8; 24];
        put64(&mut sub, 0, outer.as_mut_ptr() as u64);
        put32(&mut sub, 8, 1);
        lin::check_eq(
            &mut failed,
            b"a lease cannot lease out what it was not given",
            lin::ioctl(
                fd,
                ioc_d(NR_MODE_CREATE_LEASE, 24),
                sub.as_mut_ptr() as usize,
            ),
            EACCES,
        );

        // Take it back. The descriptor survives - Linux does not close a revoked
        // lease out from under its holder - but the object no longer belongs to
        // anybody through it.
        let mut rev = [0u8; 8];
        put32(&mut rev, 0, fd as u32);
        lin::check_eq(
            &mut failed,
            b"the master revokes the lease",
            lin::ioctl(
                card,
                ioc_d(NR_MODE_REVOKE_LEASE, 8),
                rev.as_mut_ptr() as usize,
            ),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"a revoked lease flips nothing",
            lin::ioctl(fd, ioc_d(NR_PAGE_FLIP, 24), flip.as_mut_ptr() as usize),
            EINVAL,
        );
        let mut after = [0u8; 24];
        put32(&mut after, 16, fd as u32);
        let r = lin::ioctl(
            card,
            ioc_d(NR_MODE_GET_LEASE, 24),
            after.as_mut_ptr() as usize,
        );
        lin::check_eq(
            &mut failed,
            b"GET_LEASE still answers a revoked lease",
            r,
            0,
        );
        lin::check_eq(
            &mut failed,
            b"and reports it as empty",
            get32(&after, 0) as isize,
            0,
        );
    }

    // Hand the console back, the way every test that borrows the screen does.
    lin::check_eq(
        &mut failed,
        b"KDSETMODE(KD_TEXT) restores",
        lin::ioctl(0, io_k(NR_KDSETMODE), KD_TEXT),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"DROP_MASTER releases the card",
        lin::ioctl(card, io_d(NR_DROP_MASTER), 0),
        0,
    );

    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-drm-lease: panicked\n")
}
