//! t-drm - a compositor's KMS bring-up, end to end on the card.
//!
//! Exercises the exact sequence `libdrm`/weston drives a DRM backend with, so
//! this pins the ABI rather than an internal-only notion: open `/dev/dri/card0`,
//! ask for `DRM_CAP_DUMB_BUFFER`, enumerate resources (one CRTC / connector /
//! encoder), make a dumb buffer, `MAP_DUMB` it and `mmap` the returned offset
//! (the buffer must alias real pages - a write through the mapping reads back),
//! `ADDFB` it to the CRTC, `SETCRTC`, then `PAGE_FLIP` with the event flag and
//! read the flip-completion `drmEvent` off the card fd. The request numbers and
//! struct offsets are the Linux UAPI's, so a green run means a real compositor
//! speaking libdrm would find the same shapes.
//!
//! Past the flip, the bracket a GL compositor adds around it: `GETFB` reading
//! the binding back, `ADDFB2` binding by fourcc, the connector's property list
//! (`OBJ_GETPROPERTIES`), the `SET_VERSION` handshake, the `GET_MAGIC`/
//! `AUTH_MAGIC` pair, `GEM_CLOSE` releasing a handle - and the render node
//! (`renderD128`) making and mapping a dumb buffer of its own, the door GBM
//! allocates through.
//!
//! Before any of that, the identity walk a Rust compositor makes with nothing
//! but the card's device number: `/sys/dev/char/226:0/device/drm` has to
//! resolve and has to name the two nodes of the one GPU, because a card that
//! cannot answer is refused as "the provided file descriptor does not refer to
//! a DRM node" - which is where niri stopped, not libdrm, whose test for the
//! same question is a different and weaker one. See `src/vfs/sysfs.rs`.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `DRM_IOCTL_BASE` is `'d'` (0x64). A request is the `_IOC` word; this
/// kernel's family dispatch accepts this spelling of it - the command number
/// at bits 8..15, the family byte below - and the Linux spelling a real
/// libdrm builds, with the two halves the other way around. The sizes only
/// matter for the direction bits, not for matching.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}
const NR_GET_CAP: usize = 0x0c;
const NR_GETRESOURCES: usize = 0xa0;
const NR_GETPLANERESOURCES: usize = 0xb5;
const NR_SETCRTC: usize = 0xa2;
const NR_GETPROPERTY: usize = 0xaa;
const NR_ADDFB: usize = 0xae;
const NR_PAGE_FLIP: usize = 0xb0;
const NR_CREATE_DUMB: usize = 0xb2;
const NR_MAP_DUMB: usize = 0xb3;
/// The core requests a GL compositor brackets the flip sequence with: the
/// framebuffer readback, the fourcc ADDFB, the connector's property list,
/// the version handshake, the auth pair, and the handle release.
const NR_GETFB: usize = 0xad;
const NR_GETFB2: usize = 0xce;
const NR_GETPLANE: usize = 0xb6;
const NR_MODE_CURSOR: usize = 0xa3;
const NR_MODE_CURSOR2: usize = 0xbb;
/// The card's second plane: the one that carries the pointer.
const CURSOR_PLANE_ID: u32 = 37;
const NR_ADDFB2: usize = 0xb8;
const NR_OBJ_GETPROPERTIES: usize = 0xb9;
const NR_VERSION: usize = 0x00;
const NR_SET_VERSION: usize = 0x07;
const NR_GET_MAGIC: usize = 0x02;
const NR_AUTH_MAGIC: usize = 0x11;
const NR_SET_MASTER: usize = 0x1e;
const NR_GEM_CLOSE: usize = 0x09;

const O_RDWR: usize = 2;
/// `DRM_EVENT_FLIP_COMPLETE`, as the UAPI spells it. libdrm dispatches on
/// this field, so its value is not the card's to choose - a test that took
/// the card's word for it agreed with a card whose flip events no compositor
/// would ever act on.
const EVENT_FLIP_COMPLETE: u32 = 0x02;
const CRTC_ID: u32 = 34;

/// Little-endian u32 out of a byte buffer at `at` (the UAPI structs are
/// native-order, and the host is little-endian).
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Little-endian u64 out of a byte buffer at `at` (the UAPI structs are
/// native-order, and the host is little-endian).
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(a)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-drm\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // GET_CAP(DUMB_BUFFER) -> value 1.
    let mut cap = [1u64, 0u64];
    let r = lin::ioctl(fd, ioc_d(NR_GET_CAP, 16), cap.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GET_CAP succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"DRM_CAP_DUMB_BUFFER is advertised",
        cap[1] == 1,
    );

    // GETRESOURCES reports one crtc, connector, encoder.
    // card_res: ptrs@0,8,16,24; count_fbs@32, crtcs@36, conns@40, enc@44.
    let mut res = [0u8; 64];
    let r = lin::ioctl(fd, ioc_d(NR_GETRESOURCES, 64), res.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETRESOURCES succeeds", r, 0);
    let word =
        |off: usize| u32::from_ne_bytes([res[off], res[off + 1], res[off + 2], res[off + 3]]);
    lin::check_true(&mut failed, b"one CRTC", word(36) == 1);
    lin::check_true(&mut failed, b"one connector", word(40) == 1);
    lin::check_true(&mut failed, b"one encoder", word(44) == 1);

    // GETPLANERESOURCES: the one KMS list whose pointer comes *before* its
    // count (`plane_id_ptr@0`, `count_planes@8`), which is exactly the order
    // a card can get wrong and still look plausible - libdrm sizes its array
    // from the count and hands the pointer back in the second call, so a
    // kernel that reads the count from offset 0 reads half of a pointer and
    // then writes through a zero-length allocation. Pinned here so the layout
    // cannot drift again.
    let mut pres = [0u8; 16];
    let r = lin::ioctl(
        fd,
        ioc_d(NR_GETPLANERESOURCES, 16),
        pres.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"GETPLANERESOURCES succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"two planes, counted at offset 8",
        u32_at(&pres, 8) as isize,
        2,
    );
    let mut plane_ids = [0u32; 2];
    pres[0..8].copy_from_slice(&(plane_ids.as_mut_ptr() as usize as u64).to_ne_bytes());
    let r = lin::ioctl(
        fd,
        ioc_d(NR_GETPLANERESOURCES, 16),
        pres.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"the plane list fills", r, 0);
    lin::check_eq(
        &mut failed,
        b"the first is the primary plane",
        plane_ids[0] as isize,
        36,
    );

    // CREATE_DUMB: 64x64 @ 32bpp -> pitch 256, size 16384, handle >= 1.
    let mut dumb = [0u8; 32];
    dumb[0..4].copy_from_slice(&64u32.to_ne_bytes()); // height
    dumb[4..8].copy_from_slice(&64u32.to_ne_bytes()); // width
    dumb[8..12].copy_from_slice(&32u32.to_ne_bytes()); // bpp
    let r = lin::ioctl(fd, ioc_d(NR_CREATE_DUMB, 32), dumb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"CREATE_DUMB succeeds", r, 0);
    let handle = u32::from_ne_bytes([dumb[16], dumb[17], dumb[18], dumb[19]]);
    let size = u64_at(&dumb, 24);
    lin::check_true(
        &mut failed,
        b"a dumb buffer handle is returned",
        handle >= 1,
    );
    lin::check_eq(
        &mut failed,
        b"the dumb size is pitch*height",
        size as isize,
        16384,
    );

    // MAP_DUMB -> a nonzero mmap offset.
    let mut map = [0u8; 16];
    map[0..4].copy_from_slice(&handle.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_MAP_DUMB, 16), map.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"MAP_DUMB succeeds", r, 0);
    let offset = u64_at(&map, 8);
    lin::check_true(&mut failed, b"MAP_DUMB returns an offset", offset != 0);

    // mmap the buffer and round-trip a value through the real pages.
    let m = lin::mmap(
        0,
        size as usize,
        lin::PROT_READ | lin::PROT_WRITE,
        lin::MAP_SHARED,
        fd,
        offset as usize,
    );
    lin::check_true(&mut failed, b"the dumb buffer maps", m > 0);
    if m > 0 {
        unsafe {
            (m as *mut u32).write_volatile(0xdead_beef);
        }
        let back = unsafe { (m as *const u32).read_volatile() };
        lin::check_true(
            &mut failed,
            b"the mapped buffer holds what was written",
            back == 0xdead_beef,
        );
    }

    // ADDFB the buffer: fb_cmd width@4,height@8,pitch@12,bpp@16,handle@24 -> fb_id@0.
    let mut fb = [0u8; 28];
    fb[4..8].copy_from_slice(&64u32.to_ne_bytes());
    fb[8..12].copy_from_slice(&64u32.to_ne_bytes());
    fb[12..16].copy_from_slice(&256u32.to_ne_bytes());
    fb[16..20].copy_from_slice(&32u32.to_ne_bytes());
    fb[20..24].copy_from_slice(&24u32.to_ne_bytes()); // depth
    fb[24..28].copy_from_slice(&handle.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_ADDFB, 28), fb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"ADDFB succeeds", r, 0);
    let fb_id = u32::from_ne_bytes([fb[0], fb[1], fb[2], fb[3]]);
    lin::check_true(&mut failed, b"a framebuffer id is returned", fb_id != 0);

    // SETCRTC crtc_id@12, fb_id@16.
    let mut crtc = [0u8; 104];
    crtc[12..16].copy_from_slice(&CRTC_ID.to_ne_bytes());
    crtc[16..20].copy_from_slice(&fb_id.to_ne_bytes());
    crtc[20..24].copy_from_slice(&0u32.to_ne_bytes()); // x
    crtc[24..28].copy_from_slice(&0u32.to_ne_bytes()); // y
    crtc[28..32].copy_from_slice(&64u32.to_ne_bytes()); // width
    crtc[32..36].copy_from_slice(&64u32.to_ne_bytes()); // height
    let r = lin::ioctl(fd, ioc_d(NR_SETCRTC, 104), crtc.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"SETCRTC succeeds", r, 0);

    // PAGE_FLIP with the event flag, then read the flip completion back.
    let mut flip = [0u8; 24];
    flip[0..4].copy_from_slice(&CRTC_ID.to_ne_bytes());
    flip[4..8].copy_from_slice(&fb_id.to_ne_bytes());
    flip[8..12].copy_from_slice(&1u32.to_ne_bytes()); // DRM_MODE_PAGE_FLIP_EVENT
    let r = lin::ioctl(fd, ioc_d(NR_PAGE_FLIP, 24), flip.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"PAGE_FLIP succeeds", r, 0);

    let mut ev = [0u8; 32];
    let n = lin::read(fd, &mut ev);
    lin::check_true(&mut failed, b"a drm event is readable", n >= 8);
    let etype = u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]);
    lin::check_eq(
        &mut failed,
        b"the event is a flip completion",
        etype as isize,
        EVENT_FLIP_COMPLETE as isize,
    );

    // --- the UAPI a GL compositor asks for beyond the flip itself ---

    // GETFB: the geometry and handle ADDFB bound, answered back.
    let mut getfb = [0u8; 28];
    getfb[0..4].copy_from_slice(&fb_id.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GETFB, 28), getfb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETFB succeeds", r, 0);
    if r == 0 {
        lin::check_eq(
            &mut failed,
            b"GETFB reports the width",
            u32_at(&getfb, 4) as isize,
            64,
        );
        lin::check_eq(
            &mut failed,
            b"GETFB reports the height",
            u32_at(&getfb, 8) as isize,
            64,
        );
        lin::check_eq(
            &mut failed,
            b"GETFB reports the pitch",
            u32_at(&getfb, 12) as isize,
            256,
        );
        lin::check_eq(
            &mut failed,
            b"GETFB reports the bpp",
            u32_at(&getfb, 16) as isize,
            32,
        );
        lin::check_eq(
            &mut failed,
            b"GETFB reports the handle",
            u32_at(&getfb, 24) as isize,
            handle as isize,
        );
    }

    // ADDFB2: the fourcc spelling of the same binding - XRGB8888, one
    // plane, no offsets or modifiers - answered with a second fb id.
    let mut fb2 = [0u8; 100];
    fb2[4..8].copy_from_slice(&64u32.to_ne_bytes()); // width
    fb2[8..12].copy_from_slice(&64u32.to_ne_bytes()); // height
    fb2[12..16].copy_from_slice(&0x3432_5258u32.to_ne_bytes()); // DRM_FORMAT_XRGB8888
    fb2[20..24].copy_from_slice(&handle.to_ne_bytes()); // handles[0]
    fb2[36..40].copy_from_slice(&256u32.to_ne_bytes()); // pitches[0]
    let r = lin::ioctl(fd, ioc_d(NR_ADDFB2, 100), fb2.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"ADDFB2 succeeds", r, 0);
    let fb2_id = u32::from_ne_bytes([fb2[0], fb2[1], fb2[2], fb2[3]]);
    lin::check_true(&mut failed, b"ADDFB2 returns a framebuffer id", fb2_id != 0);

    // GETFB2: the same binding read back *with its format*, which is the whole
    // reason libdrm has a second call. `struct drm_mode_fb_cmd2` is in and out
    // at once, so the request goes back with `fb_id` in the bytes the answer
    // comes back in.
    //
    // Every field below is one a caller that inherited this framebuffer needs,
    // and the three *unused* planes are checked as zeros: libdrm reads
    // `handles`/`pitches`/`offsets`/`modifier` as four-element arrays whose
    // length is "however many are non-zero", so a kernel that left them alone
    // would be describing a four-plane buffer over addresses the caller never
    // provided - the same class of mistake as the mode name weston's `strdup`
    // once walked off the end of.
    let mut g2 = [0u8; 104];
    g2[0..4].copy_from_slice(&fb2_id.to_ne_bytes()); // fb_id
    let r = lin::ioctl(fd, ioc_d(NR_GETFB2, 104), g2.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETFB2 succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"GETFB2 reports the width",
        u32_at(&g2, 4) as isize,
        64,
    );
    lin::check_eq(
        &mut failed,
        b"GETFB2 reports the height",
        u32_at(&g2, 8) as isize,
        64,
    );
    lin::check_eq(
        &mut failed,
        b"GETFB2 reports the fourcc ADDFB2 bound",
        u32_at(&g2, 12) as isize,
        0x3432_5258,
    );
    lin::check_eq(
        &mut failed,
        b"and no modifier flag, since none was carried",
        u32_at(&g2, 16) as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and the handle behind it",
        u32_at(&g2, 20) as isize,
        handle as isize,
    );
    lin::check_eq(&mut failed, b"and the pitch", u32_at(&g2, 36) as isize, 256);
    lin::check_eq(
        &mut failed,
        b"and a zero offset, since the buffer starts at its start",
        u32_at(&g2, 52) as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and a single plane, with the second handle zeroed",
        u32_at(&g2, 24) as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and the fourth pitch zeroed",
        u32_at(&g2, 48) as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and the second modifier zeroed",
        u64_at(&g2, 76) as isize,
        0,
    );

    // The legacy ADDFB names its layout as bpp plus depth, because its struct
    // predates fourcc codes - so GETFB2 on such a framebuffer is answering with
    // a format the card *derived*. Depth 24 over 32bpp is XRGB8888, the same
    // pair ADDFB2 spells directly, which is what makes the two doors agree.
    let mut leg = [0u8; 28];
    leg[4..8].copy_from_slice(&64u32.to_ne_bytes()); // width
    leg[8..12].copy_from_slice(&64u32.to_ne_bytes()); // height
    leg[12..16].copy_from_slice(&256u32.to_ne_bytes()); // pitch
    leg[16..20].copy_from_slice(&32u32.to_ne_bytes()); // bpp
    leg[20..24].copy_from_slice(&24u32.to_ne_bytes()); // depth
    leg[24..28].copy_from_slice(&handle.to_ne_bytes()); // handle
    let r = lin::ioctl(fd, ioc_d(NR_ADDFB, 28), leg.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"a legacy ADDFB succeeds too", r, 0);
    let leg_id = u32_at(&leg, 0);
    let mut g3 = [0u8; 104];
    g3[0..4].copy_from_slice(&leg_id.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GETFB2, 104), g3.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETFB2 reads a legacy framebuffer back", r, 0);
    lin::check_eq(
        &mut failed,
        b"and derives XRGB8888 from 32bpp depth 24, as Linux's legacy mapping does",
        u32_at(&g3, 12) as isize,
        0x3432_5258,
    );

    // A framebuffer id the card never made: `EINVAL`, the same answer the
    // legacy GETFB gives, so the two spellings of the question agree.
    let mut g4 = [0u8; 104];
    g4[0..4].copy_from_slice(&0xffffu32.to_ne_bytes());
    lin::check_eq(
        &mut failed,
        b"GETFB2 of an unknown framebuffer is EINVAL",
        lin::ioctl(fd, ioc_d(NR_GETFB2, 104), g4.as_mut_ptr() as usize),
        -22,
    );

    // --- the cursor plane ----------------------------------------------------
    // The plane list is two now, and the second one has to be *described* as the
    // cursor: a compositor finds its cursor plane by asking about each id here and
    // reading the `type` property back. Before this, `GETPLANE` ignored the id it
    // was given and answered with the primary plane for every question, which is
    // why the test asks about both ids and about one that is not there.
    let mut pl_res = [0u8; 16];
    let mut plane_ids = [0u32; 4];
    pl_res[0..8].copy_from_slice(&(plane_ids.as_mut_ptr() as usize as u64).to_ne_bytes());
    pl_res[8..12].copy_from_slice(&4u32.to_ne_bytes()); // count_planes: room for four
    let r = lin::ioctl(
        fd,
        ioc_d(NR_GETPLANERESOURCES, 16),
        pl_res.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"the plane list is read", r, 0);
    lin::check_eq(
        &mut failed,
        b"two planes, where there used to be one",
        u32_at(&pl_res, 8) as isize,
        2,
    );
    lin::check_eq(
        &mut failed,
        b"and the second is the cursor plane",
        plane_ids[1] as isize,
        CURSOR_PLANE_ID as isize,
    );

    // The cursor plane's own format list: `ARGB8888` alone, which is the layout a
    // cursor buffer is in and the format this card used to refuse.
    let mut cp = [0u8; 32];
    let mut cp_formats = [0u32; 4];
    cp[0..4].copy_from_slice(&CURSOR_PLANE_ID.to_ne_bytes());
    cp[24..32].copy_from_slice(&(cp_formats.as_mut_ptr() as usize as u64).to_ne_bytes());
    cp[20..24].copy_from_slice(&4u32.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GETPLANE, 32), cp.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"the cursor plane is described", r, 0);
    lin::check_eq(
        &mut failed,
        b"it reports the id it was asked about",
        u32_at(&cp, 0) as isize,
        CURSOR_PLANE_ID as isize,
    );
    lin::check_eq(
        &mut failed,
        b"one format, not the primary plane's two",
        u32_at(&cp, 20) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"and it is ARGB8888, the layout a cursor carries its alpha in",
        cp_formats[0] as isize,
        0x3432_5241,
    );

    // A plane id the card does not have: refused, rather than answered with
    // whichever plane happened to be looked at first.
    let mut bogus_plane = [0u8; 32];
    bogus_plane[0..4].copy_from_slice(&9999u32.to_ne_bytes());
    lin::check_eq(
        &mut failed,
        b"GETPLANE of a plane that is not there is ENOENT",
        lin::ioctl(
            fd,
            ioc_d(NR_GETPLANE, 32),
            bogus_plane.as_mut_ptr() as usize,
        ),
        -2,
    );

    // The cursor requests themselves. `flags` has to name one of the two things -
    // bind or move - and a request that says neither is EINVAL.
    let mut cur = [0u8; 36];
    cur[4..8].copy_from_slice(&34u32.to_ne_bytes()); // crtc_id
    lin::check_eq(
        &mut failed,
        b"MODE_CURSOR with no flags at all is EINVAL",
        lin::ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), cur.as_mut_ptr() as usize),
        -22,
    );
    // A bit outside the pair, which is a request this card cannot be asked.
    cur[0..4].copy_from_slice(&0x8u32.to_ne_bytes()); // flags: neither BO nor MOVE
    lin::check_eq(
        &mut failed,
        b"and a flag outside the pair is EINVAL too",
        lin::ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), cur.as_mut_ptr() as usize),
        -22,
    );
    // An image that is not a framebuffer: the `handle` is an fb id here, and the
    // UAPI calls that field "driver specific", so this card's choice *is* the ABI
    // - which is exactly why it needs pinning.
    cur[0..4].copy_from_slice(&0x1u32.to_ne_bytes()); // flags: BO
    cur[16..20].copy_from_slice(&64u32.to_ne_bytes()); // width
    cur[20..24].copy_from_slice(&64u32.to_ne_bytes()); // height
    cur[24..28].copy_from_slice(&0xffffu32.to_ne_bytes()); // handle: no such fb
    lin::check_eq(
        &mut failed,
        b"MODE_CURSOR binding a framebuffer that does not exist is EINVAL",
        lin::ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), cur.as_mut_ptr() as usize),
        -22,
    );

    // A real cursor image: ARGB8888 over the same dumb buffer, which is what a
    // compositor makes. It binds, and `GETPLANE` reports it bound.
    let mut cfb = [0u8; 104];
    cfb[4..8].copy_from_slice(&64u32.to_ne_bytes()); // width
    cfb[8..12].copy_from_slice(&64u32.to_ne_bytes()); // height
    cfb[12..16].copy_from_slice(&0x3432_5241u32.to_ne_bytes()); // DRM_FORMAT_ARGB8888
    cfb[20..24].copy_from_slice(&handle.to_ne_bytes()); // handles[0]
    cfb[36..40].copy_from_slice(&256u32.to_ne_bytes()); // pitches[0]
    let r = lin::ioctl(fd, ioc_d(NR_ADDFB2, 104), cfb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"an ARGB8888 framebuffer binds", r, 0);
    let cursor_fb = u32_at(&cfb, 0);
    cur[24..28].copy_from_slice(&cursor_fb.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), cur.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"and MODE_CURSOR binds it to the cursor plane",
        r,
        0,
    );
    let mut cp2 = [0u8; 32];
    cp2[0..4].copy_from_slice(&CURSOR_PLANE_ID.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GETPLANE, 32), cp2.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"the cursor plane reads back", r, 0);
    lin::check_eq(
        &mut failed,
        b"with that framebuffer bound to it",
        u32_at(&cp2, 8) as isize,
        cursor_fb as isize,
    );

    // `MODE_CURSOR2` moves it, hotspot and all. Where it landed is read back by
    // `t-drm-props`, which has the property-name lookup this file does not.
    cur[0..4].copy_from_slice(&0x2u32.to_ne_bytes()); // flags: MOVE
    cur[8..12].copy_from_slice(&10i32.to_ne_bytes()); // x
    cur[12..16].copy_from_slice(&20i32.to_ne_bytes()); // y
    cur[28..32].copy_from_slice(&1i32.to_ne_bytes()); // hot_x
    cur[32..36].copy_from_slice(&1i32.to_ne_bytes()); // hot_y
    let r = lin::ioctl(fd, ioc_d(NR_MODE_CURSOR2, 36), cur.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"MODE_CURSOR2 moves the pointer", r, 0);

    // And a zero-sized image turns it off, which is how libdrm spells it.
    cur[0..4].copy_from_slice(&0x1u32.to_ne_bytes()); // flags: BO
    cur[16..20].copy_from_slice(&0u32.to_ne_bytes()); // width
    cur[20..24].copy_from_slice(&0u32.to_ne_bytes()); // height
    let r = lin::ioctl(fd, ioc_d(NR_MODE_CURSOR, 28), cur.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"a zero-sized cursor turns it off", r, 0);
    let mut cp3 = [0u8; 32];
    cp3[0..4].copy_from_slice(&CURSOR_PLANE_ID.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GETPLANE, 32), cp3.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"the cursor plane reads back again", r, 0);
    lin::check_eq(
        &mut failed,
        b"with nothing bound, so the pointer is gone",
        u32_at(&cp3, 8) as isize,
        0,
    );

    // OBJ_GETPROPERTIES on the connector: the two-call list shape, and the
    // CRTC_ID/DPMS pair a compositor drives the connector by.
    let mut props = [0u8; 28];
    props[20..24].copy_from_slice(&35u32.to_ne_bytes()); // obj_id: the connector
    props[24..28].copy_from_slice(&0xc0c0_c0c0u32.to_ne_bytes()); // DRM_MODE_OBJECT_CONNECTOR
    let r = lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 28),
        props.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"OBJ_GETPROPERTIES succeeds", r, 0);
    let count = u32::from_ne_bytes([props[16], props[17], props[18], props[19]]);
    // The *set* is the card's business and its own test's: `t-drm-props` pins
    // the six a real connector carries and the shape of each. What this file
    // needs is that the two it reads are there at all, and that the list can be
    // walked - so it asks for the two by index and no more.
    lin::check_true(
        &mut failed,
        b"the connector carries at least the two properties this file reads",
        count >= 2,
    );
    let mut prop_ids = [0u32; 2];
    let mut prop_values = [0u64; 2];
    props[0..8].copy_from_slice(&(prop_ids.as_mut_ptr() as usize as u64).to_ne_bytes());
    props[8..16].copy_from_slice(&(prop_values.as_mut_ptr() as usize as u64).to_ne_bytes());
    let r = lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 28),
        props.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"OBJ_GETPROPERTIES fills the arrays", r, 0);
    lin::check_eq(
        &mut failed,
        b"the connector's first property is CRTC_ID",
        prop_ids[0] as isize,
        70,
    );
    lin::check_eq(
        &mut failed,
        b"the connector is bound to the one CRTC",
        prop_values[0] as isize,
        CRTC_ID as isize,
    );
    lin::check_eq(
        &mut failed,
        b"the connector's second property is DPMS",
        prop_ids[1] as isize,
        71,
    );
    lin::check_eq(
        &mut failed,
        b"the connector's DPMS mode is on",
        prop_values[1] as isize,
        0,
    );

    // OBJ_GETPROPERTIES on the plane: the `type` property that classifies it
    // - what weston picks its primary plane by - and the frame properties an
    // atomic commit writes (`FB_ID`, the two rectangles, the blend mode).
    // The list is sized from the count the first call answered, which is the
    // two-call contract every KMS list ioctl follows.
    let mut pprops = [0u8; 28];
    pprops[20..24].copy_from_slice(&36u32.to_ne_bytes()); // obj_id: the plane
    pprops[24..28].copy_from_slice(&0xeeee_eeeeu32.to_ne_bytes()); // DRM_MODE_OBJECT_PLANE
    let r = lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 28),
        pprops.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"the plane carries properties", r, 0);
    let plane_count = u32::from_ne_bytes([pprops[16], pprops[17], pprops[18], pprops[19]]) as usize;
    lin::check_true(
        &mut failed,
        b"the plane carries a frame's worth of properties",
        plane_count >= 5,
    );
    let mut pids = [0u32; 16];
    let mut pvals = [0u64; 16];
    pprops[0..8].copy_from_slice(&(pids.as_mut_ptr() as usize as u64).to_ne_bytes());
    pprops[8..16].copy_from_slice(&(pvals.as_mut_ptr() as usize as u64).to_ne_bytes());
    let r = lin::ioctl(
        fd,
        ioc_d(NR_OBJ_GETPROPERTIES, 28),
        pprops.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"the plane's properties fill", r, 0);
    lin::check_eq(
        &mut failed,
        b"the plane's property is its type",
        pids[0] as isize,
        73,
    );
    lin::check_eq(
        &mut failed,
        b"the plane is a primary surface",
        pvals[0] as isize,
        1,
    );
    lin::check_true(
        &mut failed,
        b"the plane carries FB_ID and the source rectangle",
        pids[..plane_count.min(16)].contains(&75) && pids[..plane_count.min(16)].contains(&79),
    );

    // GETPROPERTY: weston finds its properties by *name*, so the id the
    // connector's list advertised must answer under the name weston asks
    // for - and the name field is at 24 in the 64-byte struct.
    let mut gp = [0u8; 64];
    gp[16..20].copy_from_slice(&70u32.to_ne_bytes()); // prop_id: CRTC_ID
    let r = lin::ioctl(fd, ioc_d(NR_GETPROPERTY, 64), gp.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GETPROPERTY succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"GETPROPERTY names the property CRTC_ID",
        unsafe { core::slice::from_raw_parts(gp.as_ptr().add(24), 7) == b"CRTC_ID" },
    );

    // VERSION: the two-call shape libdrm's `drmGetVersion` drives the request
    // in, and the reason a card cannot answer it however it likes. The first
    // call brings a null pointer and a zero length and asks how long the name
    // is; the caller then allocates exactly that many bytes *plus one*, hands
    // the block back with the same length in the field - where it means "this
    // much room" - and after the second call writes its own terminator at
    // `name[name_len]`. So an answer that outgrows the first one, or a name
    // written into the pointer field instead of through the pointer, puts that
    // terminator outside the caller's block: weston died in libdrm, on a write
    // to a page nobody had mapped, over exactly this.
    let mut ver = [0u8; 64];
    let r = lin::ioctl(fd, ioc_d(NR_VERSION, 64), ver.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VERSION sizing call succeeds", r, 0);
    let name_len = u64_at(&ver, 16) as usize;
    lin::check_true(&mut failed, b"VERSION answers a name length", name_len > 0);
    lin::check_true(
        &mut failed,
        b"VERSION answers no date or description",
        u64_at(&ver, 32) == 0 && u64_at(&ver, 48) == 0,
    );

    let mut block = [0u8; 64];
    let block_ptr = block.as_mut_ptr() as usize;
    lin::check_true(
        &mut failed,
        b"the name fits the block the caller would allocate",
        name_len + 1 <= block.len(),
    );
    ver[24..32].copy_from_slice(&(block_ptr as u64).to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_VERSION, 64), ver.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VERSION fill call succeeds", r, 0);
    let name_len2 = u64_at(&ver, 16) as usize;
    lin::check_true(
        &mut failed,
        b"VERSION does not outgrow the room the caller brought",
        name_len2 <= name_len,
    );
    lin::check_true(
        &mut failed,
        b"VERSION wrote the name through the pointer, not over it",
        &block[..name_len2] == &b"mpos"[..] && u64_at(&ver, 24) == block_ptr as u64,
    );

    // SET_VERSION: request the 1.4 KMS interface, read the driver's 1.1.
    let mut ver = [0u8; 16];
    ver[0..4].copy_from_slice(&1i32.to_ne_bytes()); // drm_di_major
    ver[4..8].copy_from_slice(&4i32.to_ne_bytes()); // drm_di_minor
    let r = lin::ioctl(fd, ioc_d(NR_SET_VERSION, 16), ver.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"SET_VERSION succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"SET_VERSION confirms the interface",
        i32::from_ne_bytes([ver[0], ver[1], ver[2], ver[3]]) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"SET_VERSION reports the driver",
        i32::from_ne_bytes([ver[8], ver[9], ver[10], ver[11]]) as isize,
        1,
    );

    // The magic handshake, as a master authenticating itself. The master is
    // taken first because Linux's `AUTH_MAGIC` is *the master's* operation: a
    // card with no master refuses it `EACCES` rather than quietly letting a
    // random descriptor mint an authorisation for itself. `SET_MASTER` is `_IO`
    // - no argument, no size - so the request word carries a zero length and
    // the third ioctl argument is a plain 0, not a pointer.
    lin::check_eq(
        &mut failed,
        b"SET_MASTER succeeds",
        lin::ioctl(fd, ioc_d(NR_SET_MASTER, 0), 0),
        0,
    );
    let mut magic = [0u8; 4];
    let r = lin::ioctl(fd, ioc_d(NR_GET_MAGIC, 4), magic.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GET_MAGIC succeeds", r, 0);
    let magic = u32::from_ne_bytes([magic[0], magic[1], magic[2], magic[3]]);
    lin::check_true(&mut failed, b"GET_MAGIC hands out a token", magic != 0);
    let mut auth = [0u8; 4];
    auth.copy_from_slice(&magic.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_AUTH_MAGIC, 4), auth.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"AUTH_MAGIC accepts the token", r, 0);

    // GEM_CLOSE: a handle released once, then refused the second time.
    let mut dumb2 = [0u8; 32];
    dumb2[0..4].copy_from_slice(&48u32.to_ne_bytes()); // height
    dumb2[4..8].copy_from_slice(&48u32.to_ne_bytes()); // width
    dumb2[8..12].copy_from_slice(&32u32.to_ne_bytes()); // bpp
    let r = lin::ioctl(fd, ioc_d(NR_CREATE_DUMB, 32), dumb2.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"a second dumb buffer is made", r, 0);
    let handle2 = u32::from_ne_bytes([dumb2[16], dumb2[17], dumb2[18], dumb2[19]]);
    let mut close = [0u8; 8];
    close[0..4].copy_from_slice(&handle2.to_ne_bytes());
    let r = lin::ioctl(fd, ioc_d(NR_GEM_CLOSE, 8), close.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"GEM_CLOSE releases the handle", r, 0);
    let r = lin::ioctl(fd, ioc_d(NR_GEM_CLOSE, 8), close.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"GEM_CLOSE refuses a dead handle",
        r,
        lin::EINVAL,
    );

    // The identity walk a Rust compositor makes before it opens anything, and
    // the one no `/dev` node can answer. `DrmNode::from_path("/dev/dri/card0")`
    // is a `stat` of the node - which is where 226:0 comes from - and then two
    // questions only sysfs answers, either of which a card that fails them is
    // refused for: "the provided file descriptor does not refer to a DRM
    // node". First, whether `/sys/dev/char/226:0/device/drm` exists; then what
    // that directory lists, since its entries are the node *names* of the one
    // GPU, matched by prefix (`card`, `renderD`) to find the render node the
    // compositor goes on to open. niri stopped exactly here, with the node, its
    // `st_rdev` and the class entry all correct, so the chain is pinned as a
    // rule rather than as a spelling: it must resolve, it must be a directory,
    // and it must name the two nodes this machine really has.
    let mut st: lin::Stat = unsafe { core::mem::zeroed() };
    lin::check_eq(
        &mut failed,
        b"the card's number reaches its GPU's drm/",
        lin::stat(b"/sys/dev/char/226:0/device/drm\0", &mut st),
        0,
    );
    lin::check_true(
        &mut failed,
        b"and what it reaches is a directory",
        st.st_mode & lin::S_IFMT == lin::S_IFDIR,
    );

    let entries = lin::openat(
        lin::AT_FDCWD,
        b"/sys/dev/char/226:0/device/drm\0",
        lin::O_RDONLY | lin::O_DIRECTORY,
        0,
    ) as i32;
    lin::check_true(&mut failed, b"the GPU's drm/ lists", entries >= 0);
    if entries >= 0 {
        let mut saw_card = false;
        let mut saw_render = false;
        for _ in 0..4 {
            let mut buf = [0u8; 512];
            let n = lin::getdents64(entries, &mut buf);
            if n <= 0 {
                break;
            }
            let chunk = &buf[..n as usize];
            let mut off = 0usize;
            while off < chunk.len() {
                match lin::parse_dirent64(&chunk[off..]) {
                    Some((d, reclen)) => {
                        if d.name == b"card0" {
                            saw_card = true;
                        }
                        if d.name == b"renderD128" {
                            saw_render = true;
                        }
                        off += reclen;
                    }
                    None => off = chunk.len(),
                }
            }
        }
        // The names, not just the count: the crate takes the first entry with
        // the type's prefix and then requires `/dev/dri/<name>` to exist, so a
        // listing that named a node this machine does not have would send a
        // compositor after a render node it cannot open.
        lin::check_true(&mut failed, b"the listing names card0", saw_card);
        lin::check_true(&mut failed, b"and renderD128", saw_render);
        let _ = lin::close(entries);
    }
    // The render node's own number walks the same chain to the same place: a
    // compositor that opened the render node is handed 226:128 and asks again.
    // The two `device` links have to be the *same* directory - one card carries
    // both names - or the parent a compositor reads the sibling node's name
    // from is a different GPU than the one it opened.
    let mut rst: lin::Stat = unsafe { core::mem::zeroed() };
    lin::check_eq(
        &mut failed,
        b"the render node's number reaches drm/ too",
        lin::stat(b"/sys/dev/char/226:128/device/drm\0", &mut rst),
        0,
    );
    let mut a: lin::Stat = unsafe { core::mem::zeroed() };
    let mut b: lin::Stat = unsafe { core::mem::zeroed() };
    let ra = lin::stat(b"/sys/dev/char/226:0/device\0", &mut a);
    let rb = lin::stat(b"/sys/dev/char/226:128/device\0", &mut b);
    lin::check_true(
        &mut failed,
        b"both numbers hang from one GPU",
        ra == 0 && rb == 0 && a.st_ino == b.st_ino,
    );

    // The render node: the same card under GBM's allocation door, where a
    // client that is not the master makes its buffers.
    let render = lin::openat(lin::AT_FDCWD, b"/dev/dri/renderD128\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/renderD128 opens", render >= 0);
    if render >= 0 {
        let mut rdumb = [0u8; 32];
        rdumb[0..4].copy_from_slice(&32u32.to_ne_bytes()); // height
        rdumb[4..8].copy_from_slice(&32u32.to_ne_bytes()); // width
        rdumb[8..12].copy_from_slice(&32u32.to_ne_bytes()); // bpp
        let r = lin::ioctl(
            render,
            ioc_d(NR_CREATE_DUMB, 32),
            rdumb.as_mut_ptr() as usize,
        );
        lin::check_eq(&mut failed, b"the render node makes a dumb buffer", r, 0);
        let rhandle = u32::from_ne_bytes([rdumb[16], rdumb[17], rdumb[18], rdumb[19]]);
        let rsize = u64_at(&rdumb, 24) as usize;
        lin::check_true(
            &mut failed,
            b"the render node returns a handle",
            rhandle >= 1,
        );
        if r == 0 && rhandle >= 1 {
            let mut rmap = [0u8; 16];
            rmap[0..4].copy_from_slice(&rhandle.to_ne_bytes());
            let r = lin::ioctl(render, ioc_d(NR_MAP_DUMB, 16), rmap.as_mut_ptr() as usize);
            lin::check_eq(&mut failed, b"the render node maps the buffer", r, 0);
            let m = lin::mmap(
                0,
                rsize,
                lin::PROT_READ | lin::PROT_WRITE,
                lin::MAP_SHARED,
                render,
                u64_at(&rmap, 8) as usize,
            );
            lin::check_true(&mut failed, b"the render node's buffer maps", m > 0);
            if m > 0 {
                unsafe {
                    (m as *mut u32).write_volatile(0xfeed_face);
                }
                let back = unsafe { (m as *const u32).read_volatile() };
                lin::check_eq(
                    &mut failed,
                    b"the render node's mapping round-trips",
                    back as isize,
                    0xfeed_face as isize,
                );
                let _ = lin::munmap(m as usize, rsize);
            }
            let mut rclose = [0u8; 8];
            rclose[0..4].copy_from_slice(&rhandle.to_ne_bytes());
            let _ = lin::ioctl(render, ioc_d(NR_GEM_CLOSE, 8), rclose.as_mut_ptr() as usize);
        }
        let _ = lin::close(render);
    }

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-drm: panicked\n")
}
