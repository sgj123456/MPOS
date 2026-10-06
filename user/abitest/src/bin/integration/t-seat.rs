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

#[path = "../../../lib/lin.rs"]
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
/// `_IOW` - the caller's struct is read by the kernel.
const fn iow(magic: u8, nr: usize, size: usize) -> usize {
    ioc(1, magic, nr, size)
}

/// `<linux/vt.h>`, whose requests are `_IO(VT, n)` and so start at zero:
/// OPENQRY 0x00, GETMODE 0x01, SETMODE 0x02, GETSTATE 0x03, SENDSIG 0x04,
/// RELDISP 0x05, ACTIVATE 0x06, WAITACTIVE 0x07.
const NR_VT_OPENQRY: usize = 0x00;
const NR_VT_GETMODE: usize = 0x01;
const NR_VT_GETSTATE: usize = 0x03;
const NR_VT_RELDISP: usize = 0x05;
const NR_VT_ACTIVATE: usize = 0x06;
const NR_VT_WAITACTIVE: usize = 0x07;
/// `VT_WAITUNTIL` - Linux's `vt_waitrelease`: the same park, with a
/// `0` that means "nothing to wait for" instead of a range error.
const NR_VT_WAITUNTIL: usize = 0x0c;
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
/// `<drm.h>`'s authentication pair, both `_IOW('d', n, struct drm_auth)` where
/// `struct drm_auth` is one `__u32`: the token a client mints for itself, and
/// the number the master spends to authorise that client's descriptor.
const NR_DRM_GET_MAGIC: usize = 0x02;
const NR_DRM_AUTH_MAGIC: usize = 0x11;

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
/// `VT_ACTIVATE`/`VT_WAITACTIVE` on a console number the machine cannot have is
/// Linux's `ENXIO` (`vt_ioctl.c`: `arg == 0 || arg > MAX_NR_CONSOLES`), not the
/// `EINVAL` an unusable *mode* gets.
const ENXIO: isize = -6;
const EACCES: isize = -13;

/// `struct vt_mode`'s `mode` field.
const VT_AUTO: u8 = 1;
const VT_PROCESS: u8 = 0;
/// The seat manager's `relsig`/`acqsig`: `SIGUSR1` and `SIGUSR2`.
const SIGUSR1: u8 = 10;
const SIGUSR2: u8 = 12;
/// What seatd names `frsig` (`common/terminal.h`'s `FRSIG` on Linux) - the field
/// Linux ignores, and so the one the kernel must not read back.
const SIGIO: u8 = 29;
/// `VT_SETMODE` is `_IOW('V', 0x02, struct vt_mode)`, so unlike its neighbours
/// it does carry a size, and a kernel that decoded it as `_IO` would read zero
/// bytes of a caller that wrote eight.
const NR_VT_SETMODE: usize = 0x02;

/// `struct vt_mode` in the shape the ioctl layer expects: eight bytes, the
/// padding the caller's struct has.
fn vt_mode_bytes(mode: u8, waitv: u8, rel: u8, acq: u8, fr: u8) -> [u8; 8] {
    [mode, waitv, rel, acq, fr, 0, 0, 0]
}

/// `struct vt_stat`'s `v_active`, `v_signal`, `v_state` - three shorts, indexed
/// as *fields*: the argument is the field number, so the byte offset is twice it.
///
/// Measured: an earlier version read `buf[at]`/`buf[at + 1]`, which for the
/// second field straddled `v_active`'s high byte and `v_signal`'s low one, and
/// for the third read `v_signal` and called it `v_state`. A kernel that wrote
/// exactly what Linux writes came back as two failures - `v_state` without its
/// always-open bit and a `v_signal` that had "changed" - which is a test
/// reporting its own bug as the kernel's.
fn stat_shorts(buf: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([buf[at * 2], buf[at * 2 + 1]])
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

    // VT_GETSTATE -> struct vt_stat { v_active, v_signal, v_state }. Linux writes
    // exactly two of the three shorts: `v_active` is the console on the screen
    // (1-based), `v_state` is the bit mask of the consoles in use with bit 0
    // always set - its own comment is "/dev/tty0 is always open" - and `v_signal`
    // is *left alone*. The buffer starts at a sentinel rather than zeroed, because
    // that is the only way "never written" is a fact: zeroed, a kernel that wrote
    // 0 there would pass.
    //
    // Which console a test was started on is the harness's business, so the number
    // is read rather than asserted.
    let mut st = [0xAAu8; 6];
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETSTATE succeeds", r, 0);
    let active = stat_shorts(&st, 0);
    lin::check_true(
        &mut failed,
        b"VT_GETSTATE names the active console",
        active >= 1,
    );
    lin::check_eq(
        &mut failed,
        b"v_state always carries /dev/tty0's bit",
        (stat_shorts(&st, 2) & 1) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"VT_GETSTATE leaves v_signal untouched (Linux never writes it)",
        stat_shorts(&st, 1) as isize,
        0xAAAA,
    );

    // And the console's *mode*, through the door that answers it: `struct vt_stat`
    // has no mode field on Linux, so a console's text mode is `KDGETMODE`'s.
    let mut kdmode = [0u8; 4];
    let r = lin::ioctl(tty, io(KD, NR_KDGETMODE), kdmode.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"KDGETMODE succeeds on a fresh console", r, 0);
    lin::check_eq(
        &mut failed,
        b"a console starts in text mode",
        i32::from_ne_bytes(kdmode) as isize,
        KD_TEXT as isize,
    );

    // VT_GETMODE -> struct vt_mode: a console nobody has claimed is automatic,
    // which is what tells a manager it can put its session here without asking
    // permission first.
    let mut vm = [0u8; 8];
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"a free console reports VT_AUTO",
        vm[0] as isize,
        VT_AUTO as isize,
    );

    // VT_SETMODE is where a handover stops being decoration. A session that
    // wants the screen asks for `VT_PROCESS`, names the two signals the kernel
    // will use, and *must* find both back: a mode the kernel accepted and
    // forgot is a mode that cannot be relied on, and the whole `relsig` dance
    // depends on the console on the screen knowing it was asked to be asked.
    let want = vt_mode_bytes(VT_PROCESS, 0, SIGUSR1, SIGUSR2, 0);
    lin::check_eq(
        &mut failed,
        b"VT_SETMODE(VT_PROCESS) succeeds",
        lin::ioctl(tty, iow(VT, NR_VT_SETMODE, 8), want.as_ptr() as usize),
        0,
    );
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE after SETMODE succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"VT_GETMODE reads back the mode and both signals",
        vm == want,
    );

    // `frsig` is the one field of `struct vt_mode` Linux does not keep:
    // `vt_setmode` copies the caller's struct and then writes zero over that byte
    // ("the frsig is ignored, so we set it to 0"), and no refusal signal is ever
    // sent - the refused session learns it from its own `VT_ACTIVATE` return or a
    // `VT_GETSTATE`. seatd names `SIGIO` there, so a kernel that stored it would be
    // reading back a number that owes the caller nothing.
    let with_fr = vt_mode_bytes(VT_PROCESS, 0, SIGUSR1, SIGUSR2, SIGIO);
    lin::check_eq(
        &mut failed,
        b"VT_SETMODE with frsig named succeeds",
        lin::ioctl(tty, iow(VT, NR_VT_SETMODE, 8), with_fr.as_ptr() as usize),
        0,
    );
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(
        &mut failed,
        b"VT_GETMODE after the frsig call succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"frsig is ignored and reads back zero, the rest survives",
        vm[4] == 0 && vm[0] == VT_PROCESS && vm[2] == SIGUSR1 && vm[3] == SIGUSR2,
    );

    // The `EINVAL` set is `<linux/vt.h>`'s and each reason is its own: an
    // unknown mode, the same signal for giving the screen up and taking it.
    let junk = vt_mode_bytes(77, 0, SIGUSR1, SIGUSR2, 0);
    lin::check_eq(
        &mut failed,
        b"VT_SETMODE with an unknown mode is EINVAL",
        lin::ioctl(tty, iow(VT, NR_VT_SETMODE, 8), junk.as_ptr() as usize),
        EINVAL,
    );
    let same = vt_mode_bytes(VT_PROCESS, 0, SIGUSR1, SIGUSR1, 0);
    lin::check_eq(
        &mut failed,
        b"VT_SETMODE with relsig == acqsig is EINVAL",
        lin::ioctl(tty, iow(VT, NR_VT_SETMODE, 8), same.as_ptr() as usize),
        EINVAL,
    );
    // The mode the two refusals above did *not* take is still the one set,
    // because a refused SETMODE that half-wrote its struct is worse than one
    // that was never answered at all.
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE after the refusals succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"a refused VT_SETMODE changed nothing",
        vm == want,
    );

    // VT_RELDISP is an answer to a question, and nobody has asked this console
    // anything yet: a session that could complete a switch it was never offered
    // would be taking the screen rather than handing it over.
    lin::check_eq(
        &mut failed,
        b"VT_RELDISP with no release outstanding is EINVAL",
        lin::ioctl(tty, io(VT, NR_VT_RELDISP), 1),
        EINVAL,
    );

    // VT_SENDSIG (0x04) was removed from the vt driver years ago; measured on a
    // current console the request falls through the kernel's switch and answers
    // ENOTTY - not the 0 this arm used to return for a command that does nothing
    // anywhere. Pinning the refusal is the counterfactual against that 0.
    lin::check_eq(
        &mut failed,
        b"VT_SENDSIG answers ENOTTY (the command is gone from Linux)",
        lin::ioctl(tty, io(VT, 0x04), 0),
        ENOTTY,
    );

    // VT_OPENQRY: a console number, and not the one already in hand - a second
    // session has somewhere else to go.
    let mut qry = [0u8; 4];
    let r = lin::ioctl(tty, io(VT, NR_VT_OPENQRY), qry.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_OPENQRY succeeds", r, 0);

    // Taking the console that is already active is what `VT_ACTIVATE` means for
    // a session opened on its own console. The call does not wait for anything:
    // Linux's `VT_ACTIVATE` is `set_console()`, which records the request, wakes
    // the console callback that sends any `relsig`, and returns 0 - the switch
    // completes when the console on the screen answers `VT_RELDISP`, which is why
    // a seat manager can stand on a `VT_PROCESS` console and ask for another one
    // without blocking itself out of the answer.
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE on the current console succeeds",
        lin::ioctl(tty, io(VT, NR_VT_ACTIVATE), active as usize),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"VT_WAITACTIVE on the current console returns",
        lin::ioctl(tty, io(VT, NR_VT_WAITACTIVE), active as usize),
        0,
    );
    // A console number the machine does not have is refused, not swallowed:
    // a seat manager reads a success here as "the screen is mine". Linux answers
    // `ENXIO` for exactly this argument, reserving `EINVAL` for a bad *mode*.
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE on a console that does not exist is ENXIO",
        lin::ioctl(tty, io(VT, NR_VT_ACTIVATE), 600),
        ENXIO,
    );
    lin::check_eq(
        &mut failed,
        b"VT_ACTIVATE 0 is ENXIO",
        lin::ioctl(tty, io(VT, NR_VT_ACTIVATE), 0),
        ENXIO,
    );
    // VT_WAITUNTIL: `arg == 0` is Linux's "nothing to wait for" - a plain
    // success without touching the screen - and a console the machine does
    // not have is the same `ENXIO` `vt_waitactive` gives. The console that
    // is already active ends the wait on the spot.
    lin::check_eq(
        &mut failed,
        b"VT_WAITUNTIL 0 answers 0 at once",
        lin::ioctl(tty, io(VT, NR_VT_WAITUNTIL), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"VT_WAITUNTIL on a console that does not exist is ENXIO",
        lin::ioctl(tty, io(VT, NR_VT_WAITUNTIL), 600),
        ENXIO,
    );
    lin::check_eq(
        &mut failed,
        b"VT_WAITUNTIL on the active console returns",
        lin::ioctl(tty, io(VT, NR_VT_WAITUNTIL), active as usize),
        0,
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
    // `VT_GETSTATE` is *not* where a mode is read - on Linux it never was; that is
    // the `KDGETMODE` answer above. What it says here is the console on the screen,
    // unchanged by the mode and keyboard dance, and a `v_state` that agrees with the
    // other door: `VT_OPENQRY` names the lowest console nobody has claimed, so every
    // console below it is in use (bit set) and it itself is free (bit clear). Two
    // answers, one predicate, asked back to back so nothing can move between them -
    // which is what makes this a pin of the mask's numbering rather than of itself.
    let mut free_q = [0u8; 4];
    let r = lin::ioctl(tty, io(VT, NR_VT_OPENQRY), free_q.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_OPENQRY succeeds again", r, 0);
    let free = i32::from_ne_bytes(free_q) as u16;
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETSTATE, 6), st.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETSTATE succeeds again", r, 0);
    lin::check_eq(
        &mut failed,
        b"VT_GETSTATE still names the same console",
        stat_shorts(&st, 0) as isize,
        active as isize,
    );
    let below = (1u16 << free) - 1;
    lin::check_eq(
        &mut failed,
        b"v_state marks every console below the free one as in use",
        (stat_shorts(&st, 2) & below) as isize,
        below as isize,
    );
    lin::check_eq(
        &mut failed,
        b"and leaves the free console's own bit clear",
        ((stat_shorts(&st, 2) >> free) & 1) as isize,
        0,
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
    // Back to automatic before the program ends, for the same reason the
    // keyboard mode is restored: a console left in `VT_PROCESS` makes every
    // later `^A` wait for an answer from a session that has already exited, and
    // the machine is left switching consoles on a timer.
    let auto = vt_mode_bytes(VT_AUTO, 0, 0, 0, 0);
    lin::check_eq(
        &mut failed,
        b"VT_SETMODE(VT_AUTO) restores",
        lin::ioctl(tty, iow(VT, NR_VT_SETMODE, 8), auto.as_ptr() as usize),
        0,
    );
    let r = lin::ioctl(tty, ior(VT, NR_VT_GETMODE, 8), vm.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"VT_GETMODE after the restore succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the console is back in automatic mode",
        vm[0] == VT_AUTO,
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
        // The card is unclaimed on a test boot, so this session can take it -
        // and having taken it, it is the only one that may set a mode.
        lin::check_eq(
            &mut failed,
            b"DRM SET_MASTER succeeds (no arg)",
            lin::ioctl(card, io(DRM, NR_DRM_SET_MASTER), 0),
            0,
        );
        // `drmIsMaster` asks with AUTH, and reads its *failure* as "I am the
        // master": `EACCES` for a non-master, `EINVAL` for the one that is.
        // A card that answered 0 here would be a card weston refuses to build a
        // display device on, and a card that answered EACCES to its master would
        // be a card no compositor can ever start on.
        lin::check_eq(
            &mut failed,
            b"DRM AUTH as the master is EINVAL",
            lin::ioctl(card, io(DRM, NR_DRM_AUTH), 0),
            EINVAL,
        );
        let card2 = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
        lin::check_true(
            &mut failed,
            b"/dev/dri/card0 opens a second time",
            card2 >= 0,
        );
        if card2 >= 0 {
            // A second open is a second *file description*, and a description is
            // what holds the master - but taking it away from the first is not
            // an error. Linux's `SET_MASTER` is `void`, because it is the
            // recovery path: a compositor that lost its console to a `^A` calls
            // it again when it wins the console back, and a `SET_MASTER` that
            // could answer "no, somebody else has it" is a compositor that can
            // never come back. What the steal does is end the first
            // description's claim, which its next probe reports.
            lin::check_eq(
                &mut failed,
                b"a second descriptor takes the master (SET_MASTER is void)",
                lin::ioctl(card2, io(DRM, NR_DRM_SET_MASTER), 0),
                0,
            );
            lin::check_eq(
                &mut failed,
                b"and the first descriptor is no longer the master",
                lin::ioctl(card, io(DRM, NR_DRM_AUTH), 0),
                EACCES,
            );
            // Authentication is the master's to grant, and the grant names a
            // *description*: `GET_MAGIC` by the client, `AUTH_MAGIC` spent by
            // the master, and the client may then drive the screen without ever
            // having been a master itself. That is how a compositor lets a
            // wayland client page-flip at all.
            let mut tok = [0u8; 4];
            lin::check_eq(
                &mut failed,
                b"GET_MAGIC on the client succeeds",
                lin::ioctl(
                    card2,
                    iow(DRM, NR_DRM_GET_MAGIC, 4),
                    tok.as_mut_ptr() as usize,
                ),
                0,
            );
            lin::check_true(
                &mut failed,
                b"GET_MAGIC hands out a nonzero token",
                u32::from_ne_bytes([tok[0], tok[1], tok[2], tok[3]]) != 0,
            );
            let mut spend = tok;
            // `card` takes the master back, and the token survives the swap: it
            // names its own description, not the master that was in place when
            // it was minted.
            lin::check_eq(
                &mut failed,
                b"the first descriptor takes the master back",
                lin::ioctl(card, io(DRM, NR_DRM_SET_MASTER), 0),
                0,
            );
            // Spending is the master's operation, and `card2` is not the master
            // any more - so the very number it was given is now worthless in its
            // own hands.
            lin::check_eq(
                &mut failed,
                b"a non-master cannot spend a token",
                lin::ioctl(
                    card2,
                    iow(DRM, NR_DRM_AUTH_MAGIC, 4),
                    spend.as_mut_ptr() as usize,
                ),
                EACCES,
            );
            lin::check_eq(
                &mut failed,
                b"the master spends the token on the client",
                lin::ioctl(
                    card,
                    iow(DRM, NR_DRM_AUTH_MAGIC, 4),
                    spend.as_mut_ptr() as usize,
                ),
                0,
            );
            // The authenticated descriptor still is not the master - an
            // authorisation buys the right to drive the screen, not the right to
            // authorise anybody else. `AUTH` is the master probe, and it says no.
            lin::check_eq(
                &mut failed,
                b"an authenticated descriptor is not thereby the master",
                lin::ioctl(card2, io(DRM, NR_DRM_AUTH), 0),
                EACCES,
            );
            // A spent token is gone: authorising is not something a master can
            // do twice with the same number.
            lin::check_eq(
                &mut failed,
                b"a spent token is refused",
                lin::ioctl(
                    card,
                    iow(DRM, NR_DRM_AUTH_MAGIC, 4),
                    spend.as_mut_ptr() as usize,
                ),
                EACCES,
            );
            // `DROP_MASTER` is void too, including from a description that is
            // not the master - there it is simply a no-op rather than an error.
            lin::check_eq(
                &mut failed,
                b"DROP_MASTER from a non-master is allowed",
                lin::ioctl(card2, io(DRM, NR_DRM_DROP_MASTER), 0),
                0,
            );
        }
        lin::check_eq(
            &mut failed,
            b"DRM DROP_MASTER succeeds (no arg)",
            lin::ioctl(card, io(DRM, NR_DRM_DROP_MASTER), 0),
            0,
        );
        // And having dropped it, the same descriptor is no longer the master, so
        // the probe's answer flips the other way.
        lin::check_eq(
            &mut failed,
            b"DRM AUTH after DROP_MASTER is EACCES",
            lin::ioctl(card, io(DRM, NR_DRM_AUTH), 0),
            EACCES,
        );
    }

    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-seat: panicked\n")
}
