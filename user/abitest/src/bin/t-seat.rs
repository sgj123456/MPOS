//! t-seat - the ioctls a seat manager (seatd) drives before weston's DRM
//! backend can take the console and the card.
//!
//! weston's DRM backend never touches the hardware directly: it asks `libseat`
//! to open a session, and the seat manager behind it owns the console through
//! the `'V'`(VT_*) and `'K'`(KD_*) ioctls and the DRM card through
//! `SET_MASTER`/`DROP_MASTER`. This pins those ABI shapes, because a seat that
//! cannot get past them is a fatal "failed to create compositor backend" no
//! matter how complete the rest of the DRM path is. The console queries report
//! the single-console reality mpos has, and the master requests - which carry
//! no argument at all - must succeed rather than be rejected for a null buffer.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `_IOC` for a `'V'`-typed request (virtual terminal). This kernel dispatches
/// on the `nr` byte, so the size only has to be the caller's struct size.
const fn ioc_v(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | (nr << 8) | (b'V' as usize)
}
/// `_IO` for a `'K'`-typed request (keyboard / video mode).
const fn ioc_k(nr: usize) -> usize {
    (nr << 8) | (b'K' as usize)
}
/// `_IO` for a `'d'`-typed DRM core request (auth / master).
const fn ioc_d(nr: usize) -> usize {
    (nr << 8) | (b'd' as usize)
}

const NR_VT_GETMODE: usize = 0x01;
const NR_VT_GETSTATE: usize = 0x03;
const NR_VT_ACTIVATE: usize = 0x06;
const NR_VT_WAITACTIVE: usize = 0x07;
const NR_KDSETMODE: usize = 0x0a;
const NR_KDGETMODE: usize = 0x0b;
const NR_DRM_AUTH: usize = 0x1c;
const NR_DRM_SET_MASTER: usize = 0x1e;
const NR_DRM_DROP_MASTER: usize = 0x1f;

const O_RDWR: usize = 2;
const ENOTTY: isize = -25;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-seat\n");
    let mut failed = false;

    // The session's console. A test program inherits the console as its
    // standard input, and it is that tty a seat manager runs the VT/KD dance
    // on. (`/dev/tty` is unusable here: it names the *controlling* terminal,
    // which this process has not been given one of - the requests belong to a
    // tty, and fd 0 is exactly that tty.)
    let tty = 0;

    // VT_GETSTATE -> struct vt_stat { v_mode, v_num, v_state } (three shorts):
    // the console reports itself console 1 and active.
    let mut st = [0u8; 6];
    let r = lin::ioctl(tty, ioc_v(NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETSTATE succeeds", r, 0);
    lin::check_true(&mut failed, b"VT_GETSTATE reports console 1", st[2] == 1);

    // VT_GETMODE -> struct vt_mode: automatic switching, so a manager leaves
    // the console in the mode it is already in.
    let mut vm = [0u8; 8];
    let r = lin::ioctl(tty, ioc_v(NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"VT_GETMODE reports VT_AUTO",
        vm[0] == 1,
    );

    // The activation calls: taking and waiting on the (only) console succeed.
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE succeeds",
        lin::ioctl(tty, ioc_v(NR_VT_ACTIVATE, 0), 1),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"VT_WAITACTIVE succeeds",
        lin::ioctl(tty, ioc_v(NR_VT_WAITACTIVE, 0), 1),
        0,
    );

    // KDSETMODE / KDGETMODE: the graphics/text switch around the handover.
    lin::check_eq(
        &mut failed,
        b"KDSETMODE succeeds",
        lin::ioctl(tty, ioc_k(NR_KDSETMODE), 0x02 /* KD_GRAPHICS */),
        0,
    );
    let mut kd = [0u8; 4];
    let r = lin::ioctl(tty, ioc_k(NR_KDGETMODE), kd.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGETMODE succeeds", r, 0);

    // The VT/KD requests name a console, so a non-tty must refuse them rather
    // than answer for a device it is not.
    let null = lin::openat(lin::AT_FDCWD, b"/dev/null\0", O_RDWR, 0) as i32;
    if null >= 0 {
        lin::check_eq(
            &mut failed,
            b"VT_GETSTATE on a non-tty is ENOTTY",
            lin::ioctl(null, ioc_v(NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize),
            ENOTTY,
        );
    }

    // DRM master/auth carry no argument (they are `_IO`), so they must be
    // answered even with a null buffer - the read-buffer check must not fire.
    let card = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", card >= 0);
    if card >= 0 {
        lin::check_eq(
            &mut failed,
            b"DRM SET_MASTER succeeds (no arg)",
            lin::ioctl(card, ioc_d(NR_DRM_SET_MASTER), 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"DRM DROP_MASTER succeeds (no arg)",
            lin::ioctl(card, ioc_d(NR_DRM_DROP_MASTER), 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"DRM AUTH succeeds (no arg)",
            lin::ioctl(card, ioc_d(NR_DRM_AUTH), 0),
            0,
        );
    }

    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-seat: panicked\n")
}
