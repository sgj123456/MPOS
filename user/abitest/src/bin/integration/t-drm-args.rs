//! t-drm-args - a card that survives a caller whose pointer is not its own.
//!
//! Every DRM request passes a struct, and this kernel reads and writes that
//! struct straight from kernel mode. An address that is not the caller's mapped
//! memory is therefore not a failed system call but a paging fault *in the
//! kernel*, and the machine cannot report one: it resets. That is how a
//! compositor died with nothing on the console - a freed request struct filled
//! with a poison value (`0xdead1111`) coming back as `objs_ptr` on the next
//! commit, sixty times a second.
//!
//! So the test is the crash, asked politely. Each case hands the card a pointer
//! it does not own - null, a poison value, an unmapped low address, a valid
//! struct with an array pointer inside it that is not the caller's - and the
//! answer must be `EFAULT`, the one Linux's `copy_*_user` gives for the same
//! request. And afterwards the card has to still work, because the bug was
//! never the wrong answer; it was the absence of any machine left to answer.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `_IOWR('d', nr, size)` as libdrm builds it: the family byte at bits 8..15.
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}

const NR_VERSION: usize = 0x00;
const NR_GET_CAP: usize = 0x0c;
const NR_GETRESOURCES: usize = 0xa0;
const NR_MODE_ATOMIC: usize = 0xbc;
const NR_CREATE_BLOB: usize = 0xbd;

/// `DRM_CAP_DUMB_BUFFER`, which this card answers 1 for.
const CAP_DUMB_BUFFER: u64 = 1;

const EFAULT: isize = -14;
const O_RDWR: usize = 2;
/// The poison musl's allocator leaves in a freed block, and the value that
/// came back as a pointer the first time this killed a machine.
const POISON: u64 = 0xdead_1111;

fn put32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-drm-args\n");
    let mut failed = false;

    let card = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", card >= 0);
    if card < 0 {
        lin::done(failed);
    }

    let mut buf = [0u8; 128];
    let ask = |buf: &mut [u8]| buf.as_mut_ptr() as usize;

    // The good path, so a green run below means "refused that one request" and
    // not "the card answers nothing at all".
    put64(&mut buf, 0, CAP_DUMB_BUFFER);
    let r = lin::ioctl(card, ioc_d(NR_GET_CAP, 16), ask(&mut buf));
    lin::check_eq(&mut failed, b"a valid GET_CAP succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"and answers the cap",
        get64(&buf, 8) as isize,
        1,
    );

    // No argument at all.
    lin::check_eq(
        &mut failed,
        b"a null GET_CAP argument is EFAULT",
        lin::ioctl(card, ioc_d(NR_GET_CAP, 16), 0),
        EFAULT,
    );
    // A poison pointer: freed memory whose contents came back as an address.
    lin::check_eq(
        &mut failed,
        b"a poisoned GET_CAP argument is EFAULT",
        lin::ioctl(card, ioc_d(NR_GET_CAP, 16), POISON as usize),
        EFAULT,
    );
    // Mapped-looking but not the caller's: a low address no process holds.
    lin::check_eq(
        &mut failed,
        b"an unmapped GETRESOURCES argument is EFAULT",
        lin::ioctl(card, ioc_d(NR_GETRESOURCES, 64), 0x1000),
        EFAULT,
    );

    // A valid struct whose *nested* array pointer is not the caller's: the
    // two-call enumeration shape, asked for one crtc id at an address that is
    // not memory. The struct itself is fine, so only the array can be refused.
    buf = [0u8; 128];
    put32(&mut buf, 36, 1); // count_crtcs: one wanted
    put64(&mut buf, 8, POISON); // crtc_ptr: nowhere
    lin::check_eq(
        &mut failed,
        b"GETRESOURCES with a poisoned crtc_ptr is EFAULT",
        lin::ioctl(card, ioc_d(NR_GETRESOURCES, 64), ask(&mut buf)),
        EFAULT,
    );

    // `CREATE_BLOB`: the bytes are read *from* the caller, so an address that
    // is not the caller's fails the same way - and a kernel that reads it
    // anyway walks off into a page that is not there.
    buf = [0u8; 128];
    put64(&mut buf, 0, POISON); // data_ptr
    put32(&mut buf, 8, 68); // length: a mode
    lin::check_eq(
        &mut failed,
        b"CREATE_BLOB from a poisoned address is EFAULT",
        lin::ioctl(card, ioc_d(NR_CREATE_BLOB, 16), ask(&mut buf)),
        EFAULT,
    );

    // `MODE_ATOMIC`, the request a compositor makes sixty times a second: the
    // object array is read before anything is applied, and this is the exact
    // shape that killed the machine - a stale `objs_ptr` in a reused struct.
    buf = [0u8; 128];
    put32(&mut buf, 4, 1); // count_objs
    put64(&mut buf, 8, POISON); // objs_ptr
    put64(&mut buf, 16, POISON + 8); // count_props_ptr
    put64(&mut buf, 24, POISON + 16); // props_ptr
    put64(&mut buf, 32, POISON + 24); // prop_values_ptr
    lin::check_eq(
        &mut failed,
        b"MODE_ATOMIC with a poisoned objs_ptr is EFAULT",
        lin::ioctl(card, ioc_d(NR_MODE_ATOMIC, 56), ask(&mut buf)),
        EFAULT,
    );

    // `VERSION` is the two-call shape with three string buffers, and the one
    // every GL stack starts with: the sizing call brings a length and no room,
    // and a call that brings room that is not its own must not be copied into.
    buf = [0u8; 128];
    put64(&mut buf, 16, 4); // name_len
    put64(&mut buf, 24, POISON); // name_ptr
    lin::check_eq(
        &mut failed,
        b"VERSION writing a name to a poisoned address is EFAULT",
        lin::ioctl(card, ioc_d(NR_VERSION, 64), ask(&mut buf)),
        EFAULT,
    );

    // The whole point: the card is still there after being fed all of that, and
    // answers a real request as it did before it.
    buf = [0u8; 128];
    put64(&mut buf, 0, CAP_DUMB_BUFFER);
    let r = lin::ioctl(card, ioc_d(NR_GET_CAP, 16), ask(&mut buf));
    lin::check_eq(&mut failed, b"the card still answers GET_CAP", r, 0);
    lin::check_eq(
        &mut failed,
        b"and still with the same value",
        get64(&buf, 8) as isize,
        1,
    );

    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-drm-args: panicked\n")
}
