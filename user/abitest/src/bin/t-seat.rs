//! t-seat - the ioctls a seat manager (seatd) drives before weston's DRM
//! backend can take the console and the card.
//!
//! weston's DRM backend never touches the hardware directly: it asks `libseat`
//! to open a session, and the seat manager behind it owns the console through
//! the `'V'`(VT_*) and `'K'`(KD_*) ioctls and the DRM card through
//! `SET_MASTER`/`DROP_MASTER`. This pins those ABI shapes - the request words
//! exactly as `<linux/vt.h>` and `<linux/kd.h>` build them - because a seat
//! that cannot get past them is a fatal "failed to create compositor backend"
//! no matter how complete the rest of the DRM path is, and because the whole
//! point of the dance is that the kernel *remembers* what it was told:
//! `KDGETMODE` has to read back the mode `KDSETMODE` set, or a compositor's
//! screen is painted over by the text console one keystroke later.
//!
//! The requests are `_IO` (no argument at all), `_IOR` (a struct out) and
//! `_IO`-with-a-value (the mode itself, in the third ioctl argument), so the
//! null buffer the master and mode setters carry must never be checked as if it
//! were a buffer.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Linux's `_IOC(dir, type, nr, size)`.
const fn ioc(dir: usize, magic: u8, nr: usize, size: usize) -> usize {
    (dir << 30) | (size << 16) | ((magic as usize) << 8) | nr
}
/// `_IO` - no size, no direction: an argument passed as a value, or none.
const fn io(magic: u8, nr: usize) -> usize {
    ioc(0, magic, nr, 0)
}
/// `_IOR` - the kernel writes `size` bytes out to the caller.
const fn ior(magic: u8, nr: usize, size: usize) -> usize {
    ioc(2, magic, nr, size)
}

const VT: u8 = b'V';
const KD: u8 = b'K';
const DRM: u8 = b'd';

/// `<linux/vt.h>`, whose requests are `_IO(VT, n)` and so start at zero:
/// OPENQRY 0x00, GETMODE 0x01, SETMODE 0x02, GETSTATE 0x03, SENDSIG 0x04,
/// RELDISP 0x05, ACTIVATE 0x06, WAITACTIVE 0x07.
const NR_VT_OPENQRY: usize = 0x00;
const NR_VT_GETMODE: usize = 0x01;
const NR_VT_GETSTATE: usize = 0x03;
const NR_VT_ACTIVATE: usize = 0x06;
const NR_VT_WAITACTIVE: usize = 0x07;
/// `<linux/kd.h>`: KDSETMODE 0x3a, KDGETMODE 0x3b, KDGKBMODE 0x44,
/// KDSKBMODE 0x45.
const NR_KDSETMODE: usize = 0x3a;
const NR_KDGETMODE: usize = 0x3b;
const NR_KDGKBMODE: usize = 0x44;
const NR_KDSKBMODE: usize = 0x45;
/// `<drm.h>`: the legacy core requests, all `_IO`.
const NR_DRM_AUTH: usize = 0x1c;
const NR_DRM_SET_MASTER: usize = 0x1e;
const NR_DRM_DROP_MASTER: usize = 0x1f;

/// `KD_TEXT` / `KD_GRAPHICS`.
const KD_TEXT: usize = 0x00;
const KD_GRAPHICS: usize = 0x01;
/// The keyboard modes this kernel answers: `K_XLATE` is a console typing into a
/// shell, `K_OFF` is a session that has taken the keys for itself.
const K_XLATE: usize = 0x01;
const K_OFF: usize = 0x04;

const O_RDWR: usize = 2;
const ENOTTY: isize = -25;
const EINVAL: isize = -22;

/// `struct vt_stat`'s `v_mode`, `v_num`, `v_state` - three shorts.
fn stat_shorts(buf: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([buf[at], buf[at + 1]])
}

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

    // VT_GETSTATE -> struct vt_stat: the console it is on, in text mode. The
    // number is the console's own, so it is read rather than asserted: which
    // console a test was started on is the harness's business.
    let mut st = [0u8; 6];
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETSTATE succeeds", r, 0);
    let first = stat_shorts(&st, 2);
    lin::check_true(&mut failed, b"VT_GETSTATE names a console", first >= 1);
    lin::check_eq(
        &mut failed,
        b"a console starts in text mode",
        stat_shorts(&st, 0) as isize,
        KD_TEXT as isize,
    );

    // VT_GETMODE -> struct vt_mode: automatic switching, so a manager leaves
    // the console in the mode it is already in.
    let mut vm = [0u8; 8];
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE succeeds", r, 0);
    lin::check_true(&mut failed, b"VT_GETMODE reports VT_AUTO", vm[0] == 1);

    // VT_OPENQRY: a console number, and not the one already in hand - a second
    // session has somewhere else to go.
    let mut qry = [0u8; 4];
    let r = lin::ioctl(tty, io(VT, NR_VT_OPENQRY), qry.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_OPENQRY succeeds", r, 0);

    // Taking the console that is already active is what `VT_ACTIVATE` means for
    // a session opened on its own console, and waiting on it returns.
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE on the current console succeeds",
        lin::ioctl(tty, io(VT, NR_VT_ACTIVATE), first as usize),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"VT_WAITACTIVE on the current console returns",
        lin::ioctl(tty, io(VT, NR_VT_WAITACTIVE), first as usize),
        0,
    );
    // A console number the machine does not have is refused, not swallowed:
    // a seat manager reads a success here as "the screen is mine".
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE on a console that does not exist is EINVAL",
        lin::ioctl(tty, io(VT, NR_VT_ACTIVATE), 600),
        EINVAL,
    );

    // KDSETMODE(KD_GRAPHICS) -> KDGETMODE reads it back. This pair is the
    // handover: the text console stops drawing on the screen the session has
    // taken, and a kernel that accepted the word and forgot it would keep
    // painting a login prompt over the compositor.
    lin::check_eq(
        &mut failed,
        b"KDSETMODE(KD_GRAPHICS) succeeds",
        lin::ioctl(tty, io(KD, NR_KDSETMODE), KD_GRAPHICS),
        0,
    );
    let mut kd = [0u8; 4];
    let r = lin::ioctl(tty, io(KD, NR_KDGETMODE), kd.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGETMODE succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"KDGETMODE reads back KD_GRAPHICS",
        i32::from_ne_bytes([kd[0], kd[1], kd[2], kd[3]]) as isize,
        KD_GRAPHICS as isize,
    );
    // `VT_GETSTATE`'s `v_mode` is the same fact in the other door's shape.
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETSTATE succeeds again", r, 0);
    lin::check_eq(
        &mut failed,
        b"VT_GETSTATE reports the graphics mode",
        stat_shorts(&st, 0) as isize,
        KD_GRAPHICS as isize,
    );

    // A mode that is neither is Linux's EINVAL, not a success that did nothing.
    lin::check_eq(
        &mut failed,
        b"KDSETMODE with a mode that does not exist is EINVAL",
        lin::ioctl(tty, io(KD, NR_KDSETMODE), 77),
        EINVAL,
    );

    // KDSKBMODE(K_OFF) is the other half of taking the keys: the console stops
    // eating them, and KDGKBMODE says so.
    let mut kb = [0u8; 4];
    let r = lin::ioctl(tty, io(KD, NR_KDGKBMODE), kb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGKBMODE succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"a console's keyboard starts in K_XLATE",
        i32::from_ne_bytes([kb[0], kb[1], kb[2], kb[3]]) as isize,
        K_XLATE as isize,
    );
    lin::check_eq(
        &mut failed,
        b"KDSKBMODE(K_OFF) succeeds",
        lin::ioctl(tty, io(KD, NR_KDSKBMODE), K_OFF),
        0,
    );
    let r = lin::ioctl(tty, io(KD, NR_KDGKBMODE), kb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGKBMODE succeeds again", r, 0);
    lin::check_eq(
        &mut failed,
        b"KDGKBMODE reads back K_OFF",
        i32::from_ne_bytes([kb[0], kb[1], kb[2], kb[3]]) as isize,
        K_OFF as isize,
    );
    lin::check_eq(
        &mut failed,
        b"KDSKBMODE with a mode that does not exist is EINVAL",
        lin::ioctl(tty, io(KD, NR_KDSKBMODE), 44),
        EINVAL,
    );

    // Hand it all back before anything else is asserted, and check that the
    // way out works too: a session that exits without restoring leaves a
    // machine with no text console, so the restore is not optional.
    lin::check_eq(
        &mut failed,
        b"KDSKBMODE(K_XLATE) restores",
        lin::ioctl(tty, io(KD, NR_KDSKBMODE), K_XLATE),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"KDSETMODE(KD_TEXT) restores",
        lin::ioctl(tty, io(KD, NR_KDSETMODE), KD_TEXT),
        0,
    );
    let r = lin::ioctl(tty, io(KD, NR_KDGETMODE), kd.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGETMODE after the restore", r, 0);
    lin::check_eq(
        &mut failed,
        b"the console is back in text mode",
        i32::from_ne_bytes([kd[0], kd[1], kd[2], kd[3]]) as isize,
        KD_TEXT as isize,
    );

    // The VT/KD requests name a console, so a non-tty must refuse them rather
    // than answer for a device it is not.
    let null = lin::openat(lin::AT_FDCWD, b"/dev/null\0", O_RDWR, 0) as i32;
    if null >= 0 {
        lin::check_eq(
            &mut failed,
            b"VT_GETSTATE on a non-tty is ENOTTY",
            lin::ioctl(null, ior(VT, NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize),
            ENOTTY,
        );
        lin::check_eq(
            &mut failed,
            b"KDGETMODE on a non-tty is ENOTTY",
            lin::ioctl(null, io(KD, NR_KDGETMODE), kd.as_mut_ptr() as usize),
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
            lin::ioctl(card, io(DRM, NR_DRM_SET_MASTER), 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"DRM DROP_MASTER succeeds (no arg)",
            lin::ioctl(card, io(DRM, NR_DRM_DROP_MASTER), 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"DRM AUTH succeeds (no arg)",
            lin::ioctl(card, io(DRM, NR_DRM_AUTH), 0),
            0,
        );
    }

    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-seat: panicked\n")
}
