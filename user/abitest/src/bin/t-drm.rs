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

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `DRM_IOCTL_BASE` is `'d'` (0x64). A request is the `_IOC` word; this kernel
/// dispatches on the `nr` byte (bits 8..15), so the sizes below only need to
/// be right for the direction, not for matching.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | (nr << 8) | (b'd' as usize)
}
const NR_GET_CAP: usize = 0x0c;
const NR_GETRESOURCES: usize = 0xa0;
const NR_SETCRTC: usize = 0xa2;
const NR_ADDFB: usize = 0xae;
const NR_PAGE_FLIP: usize = 0xb0;
const NR_CREATE_DUMB: usize = 0xb2;
const NR_MAP_DUMB: usize = 0xb3;

const O_RDWR: usize = 2;
const EVENT_FLIP_COMPLETE: u32 = 0x8000_0002;
const CRTC_ID: u32 = 34;

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

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-drm: panicked\n")
}
