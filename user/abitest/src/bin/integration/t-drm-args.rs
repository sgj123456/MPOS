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
/// The queries this card *refuses*: the legacy gamma pair, which it has no LUT
/// for, and the CRTC-sequence pair, which it has no vblank for.
const NR_MODE_GETGAMMA: usize = 0xa4;
const NR_MODE_SETGAMMA: usize = 0xa5;
const NR_CRTC_GET_SEQUENCE: usize = 0x3b;
const NR_CRTC_QUEUE_SEQUENCE: usize = 0x3c;
/// The CRTC's own state readback, whose `gamma_size` is what tells a caller the
/// gamma pair is not worth asking about.
const NR_GETCRTC: usize = 0xa1;

/// `DRM_CAP_DUMB_BUFFER`, which this card answers 1 for.
const CAP_DUMB_BUFFER: u64 = 1;

const EFAULT: isize = -14;
/// The answer for a request this card's hardware cannot do: no gamma table
/// behind the legacy gamma pair, no vblank behind the sequence pair.
const EINVAL: isize = -22;
const ENOTTY: isize = -25;
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
fn get32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
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

    // The *other* half of that rule - a zero count beside the dangling non-null
    // pointer an empty `Vec` carries, which is not a fault - cannot be pinned
    // from this file: it needs a card with no framebuffer, and by the time this
    // program runs, `t-drm` has already created one. It is pinned at that point
    // instead, in `t-drm`'s own enumeration.

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

    // The requests §7.4's list names that this card *refuses*, and the argument
    // check in front of each. These are not "unimplemented": each one is in the
    // dispatch table with a reason, and the reason is that this card has no
    // hardware behind it - no gamma table for the legacy pair, no vertical blank
    // for the sequence pair. Before they were named they fell through to the
    // catch-all, which meant the same errno for a different reason and, worse, no
    // argument check at all: Linux's ioctl layer copies every DRM struct in
    // before a handler can refuse it, so a bad pointer must hear EFAULT.
    //
    // The legacy gamma pair: `GETCRTC` reports `gamma_size` 0, and a CRTC with
    // no LUT answers EINVAL - both to a read and to a write, because an OK from
    // the write would be a compositor believing it had applied a colour
    // transformation no pixel will show.
    buf = [0u8; 128];
    put32(&mut buf, 0, 34); // crtc_id: the card's one CRTC
    put32(&mut buf, 4, 0); // gamma_size: the caller asks for none
    lin::check_eq(
        &mut failed,
        b"GETGAMMA on a CRTC with no gamma table is EINVAL",
        lin::ioctl(card, ioc_d(NR_MODE_GETGAMMA, 32), ask(&mut buf)),
        EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"and SETGAMMA is refused the same way",
        lin::ioctl(card, ioc_d(NR_MODE_SETGAMMA, 32), ask(&mut buf)),
        EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a poisoned GETGAMMA argument is EFAULT before the refusal",
        lin::ioctl(card, ioc_d(NR_MODE_GETGAMMA, 32), POISON as usize),
        EFAULT,
    );

    // The CRTC-sequence pair: the question is about a vblank clock, and this
    // card's only boundary is a frame it presented - ENOTTY, the answer §7.1
    // recorded for the other vblank door. What changes here is the argument
    // check, which is what the poisoned case pins.
    buf = [0u8; 128];
    put32(&mut buf, 0, 34); // crtc_id
    lin::check_eq(
        &mut failed,
        b"CRTC_GET_SEQUENCE on a card with no vblank is ENOTTY",
        lin::ioctl(card, ioc_d(NR_CRTC_GET_SEQUENCE, 24), ask(&mut buf)),
        ENOTTY,
    );
    lin::check_eq(
        &mut failed,
        b"and CRTC_QUEUE_SEQUENCE with it",
        lin::ioctl(card, ioc_d(NR_CRTC_QUEUE_SEQUENCE, 24), ask(&mut buf)),
        ENOTTY,
    );
    lin::check_eq(
        &mut failed,
        b"a poisoned CRTC_GET_SEQUENCE argument is EFAULT, not ENOTTY",
        lin::ioctl(card, ioc_d(NR_CRTC_GET_SEQUENCE, 24), POISON as usize),
        EFAULT,
    );

    // `GETCRTC` writes `gamma_size` rather than leaving it, which matters for
    // exactly the reason the mode's name does: the caller allocated the struct
    // with `malloc`, so a field the kernel skips is a number the caller reads as
    // garbage. Filled with a poison first, so a skipped field cannot pass by
    // being zero.
    buf = [0u8; 128];
    put32(&mut buf, 0, 34); // crtc_id
    put32(&mut buf, 28, 0xffff_ffff); // gamma_size, to be overwritten
    let r = lin::ioctl(card, ioc_d(NR_GETCRTC, 104), ask(&mut buf));
    lin::check_eq(&mut failed, b"GETCRTC succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"and reports a gamma_size of 0 rather than leaving the caller's bytes",
        get32(&buf, 28) as isize,
        0,
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
