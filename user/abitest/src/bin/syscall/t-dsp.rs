// t-dsp: the OSS parameter face of /dev/dsp.
//
// The contract this pins is that the *writer* owns the numbers: rate,
// channels and format are asked about and changed with the `SNDCTL_DSP_*`
// ioctls, exactly as linux/soundcard.h spells them, and a parameter the
// stream will not take is refused with `EINVAL` rather than answered with a
// success that changed nothing. A device-less boot simply has no node to
// open, which is its own honest answer - the test fails loudly there,
// because the gate machine is configured with the card.
#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const DSP: &[u8] = b"/dev/dsp\0";
const INIT: &[u8] = b"/init\0";

/// The `_IOC` word, spelled as `linux/soundcard.h` computes it:
/// `dir << 30 | size << 16 | 'P' << 8 | nr`.
const fn ioc(dir: usize, size: usize, nr: usize) -> usize {
    (dir << 30) | (size << 16) | ((b'P' as usize) << 8) | nr
}
const SNDCTL_DSP_RESET: usize = ioc(0, 0, 0);
const SNDCTL_DSP_SYNC: usize = ioc(0, 0, 1);
const SNDCTL_DSP_SPEED: usize = ioc(3, 4, 2);
const SNDCTL_DSP_STEREO: usize = ioc(3, 4, 3);
const SNDCTL_DSP_GETBLKSIZE: usize = ioc(3, 4, 4);
const SNDCTL_DSP_SETFMT: usize = ioc(3, 4, 5);
const SNDCTL_DSP_CHANNELS: usize = ioc(3, 4, 6);
const SNDCTL_DSP_POST: usize = ioc(0, 0, 8);
const SNDCTL_DSP_SETFRAGMENT: usize = ioc(3, 4, 10);
const SNDCTL_DSP_GETFMTS: usize = ioc(2, 4, 11);
const SNDCTL_DSP_GETOSPACE: usize = ioc(2, 16, 12);
const SNDCTL_DSP_NONBLOCK: usize = ioc(0, 0, 14);

const AFMT_S16_LE: i32 = 0x0000_0010;
const AFMT_MU_LAW: i32 = 0x0000_0001;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dsp\n");
    let mut failed = false;

    let dsp = lin::openat(lin::AT_FDCWD, DSP, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/dsp opens", dsp >= 0);
    let dsp = dsp as i32;

    // --- speed: query, set, and the answer is what the device runs ---
    let mut rate: i32 = 0;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    lin::check_eq(&mut failed, b"SPEED(0) answers the running rate", r, 0);
    let running = rate;
    lin::check_true(&mut failed, b"and it is a rate", running > 0);

    rate = running;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    lin::check_eq(
        &mut failed,
        b"setting SPEED to the running rate succeeds",
        r,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and answers the same rate",
        rate as isize,
        running as isize,
    );

    // A rate the device does not list rounds onto the one it does, and the
    // ioctl reports the rounding rather than hiding it. 44101 is no device's
    // rate; the only promise is that the answer is some rate that exists now.
    rate = 44101;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    lin::check_eq(&mut failed, b"a near-miss rate is still accepted", r, 0);
    lin::check_true(
        &mut failed,
        b"and answered with the rate it settled on",
        rate > 0 && rate != 44101,
    );

    // --- format: S16LE is the stream, and the only format claimed ---
    let mut fmt: i32 = 0;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    lin::check_eq(&mut failed, b"SETFMT(0) answers the running format", r, 0);
    lin::check_eq(
        &mut failed,
        b"and it is AFMT_S16_LE",
        fmt as isize,
        AFMT_S16_LE as isize,
    );

    fmt = AFMT_S16_LE;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    lin::check_eq(&mut failed, b"SETFMT(S16_LE) succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"and answers the format",
        fmt as isize,
        AFMT_S16_LE as isize,
    );

    fmt = AFMT_MU_LAW;
    let r = lin::ioctl(dsp, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    lin::check_eq(
        &mut failed,
        b"a format the stream does not run is EINVAL, not a yes",
        r,
        lin::EINVAL,
    );

    let mut mask: i32 = 0;
    let r = lin::ioctl(dsp, SNDCTL_DSP_GETFMTS, &raw mut mask as usize);
    lin::check_eq(&mut failed, b"GETFMTS answers", r, 0);
    lin::check_eq(
        &mut failed,
        b"and the mask names S16_LE only",
        mask as isize,
        AFMT_S16_LE as isize,
    );

    // --- channels and the stereo spelling of the same number ---
    let mut ch: i32 = 0;
    let r = lin::ioctl(dsp, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    lin::check_eq(&mut failed, b"CHANNELS(0) answers the running count", r, 0);
    lin::check_true(&mut failed, b"and it is at least one", ch >= 1);

    ch = 1;
    let r = lin::ioctl(dsp, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    lin::check_eq(&mut failed, b"CHANNELS(1) is honoured", r, 0);
    lin::check_eq(&mut failed, b"and answers mono", ch as isize, 1);

    let mut stereo: i32 = 1;
    let r = lin::ioctl(dsp, SNDCTL_DSP_STEREO, &raw mut stereo as usize);
    lin::check_eq(&mut failed, b"STEREO(1) is honoured", r, 0);
    ch = 0;
    let _ = lin::ioctl(dsp, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    lin::check_eq(
        &mut failed,
        b"and the channel count now says two",
        ch as isize,
        2,
    );

    let mut bogus: i32 = 100;
    let r = lin::ioctl(dsp, SNDCTL_DSP_CHANNELS, &raw mut bogus as usize);
    lin::check_eq(
        &mut failed,
        b"a channel count outside the device is EINVAL",
        r,
        lin::EINVAL,
    );

    // --- the geometry the writers pace against ---
    let mut blk: i32 = 0;
    let r = lin::ioctl(dsp, SNDCTL_DSP_GETBLKSIZE, &raw mut blk as usize);
    lin::check_eq(&mut failed, b"GETBLKSIZE answers", r, 0);
    lin::check_true(&mut failed, b"and it is a fragment's worth", blk >= 4096);

    let mut info = [0i32; 4];
    let r = lin::ioctl(
        dsp,
        SNDCTL_DSP_GETOSPACE,
        &raw mut info as *mut i32 as usize,
    );
    lin::check_eq(&mut failed, b"GETOSPACE answers", r, 0);
    lin::check_true(
        &mut failed,
        b"and the geometry adds up",
        info[1] >= 1 && info[2] >= 1 && info[3] >= info[0] * info[2],
    );

    // --- the refusals ---
    let mut refused: i32 = (4096) | (8 << 16);
    let r = lin::ioctl(dsp, SNDCTL_DSP_SETFRAGMENT, &raw mut refused as usize);
    lin::check_eq(
        &mut failed,
        b"SETFRAGMENT is refused honestly",
        r,
        lin::EINVAL,
    );
    let r = lin::ioctl(dsp, SNDCTL_DSP_NONBLOCK, 0);
    lin::check_eq(&mut failed, b"NONBLOCK is refused honestly", r, lin::EINVAL);

    // --- the no-argument trio ---
    let r = lin::ioctl(dsp, SNDCTL_DSP_RESET, 0);
    lin::check_eq(&mut failed, b"RESET answers 0", r, 0);
    let r = lin::ioctl(dsp, SNDCTL_DSP_SYNC, 0);
    lin::check_eq(&mut failed, b"SYNC answers 0", r, 0);
    let r = lin::ioctl(dsp, SNDCTL_DSP_POST, 0);
    lin::check_eq(&mut failed, b"POST answers 0", r, 0);

    // --- and bytes still play ---
    let silence = [0u8; 512];
    let r = lin::write(dsp, &silence);
    lin::check_eq(&mut failed, b"a frame buffer writes through", r, 512);

    // A stream with no parameters is `/dev/null`, and the family is as
    // unknown there as any ioctl is on Linux's mixer-less null device.
    let null = lin::openat(lin::AT_FDCWD, b"/dev/null\0", lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/null opens", null >= 0);
    let mut v: i32 = 0;
    let r = lin::ioctl(null as i32, SNDCTL_DSP_SPEED, &raw mut v as usize);
    lin::check_eq(&mut failed, b"and SPEED on it is ENOTTY", r, lin::ENOTTY);
    let _ = lin::close(null as i32);

    // A regular file is the other half of the same refusal.
    let init = lin::openat(lin::AT_FDCWD, INIT, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/init opens", init >= 0);
    let r = lin::ioctl(init as i32, SNDCTL_DSP_SPEED, &raw mut v as usize);
    lin::check_eq(
        &mut failed,
        b"and SPEED on a regular file is ENOTTY",
        r,
        lin::ENOTTY,
    );
    let _ = lin::close(init as i32);

    // --- chmod on a device node really changes it ---
    //
    // The other half of the same promise: a `chmod(2)` that answers 0 while
    // the node keeps nothing is what left udev's `MODE=` rules as logs. The
    // number set here is the number `fstat` reports afterwards, and the
    // node is given its mode back before the test ends.
    let chmodded = lin::chmod(DSP, 0o600);
    lin::check_eq(&mut failed, b"chmod 0600 on /dev/dsp succeeds", chmodded, 0);
    let mut st = unsafe { core::mem::zeroed::<lin::Stat>() };
    let r = lin::fstat(dsp, &raw mut st);
    lin::check_eq(&mut failed, b"fstat answers", r, 0);
    lin::check_eq(
        &mut failed,
        b"and the mode it reports is the one that was set",
        (st.st_mode & 0o7777) as isize,
        0o600,
    );
    lin::check_true(
        &mut failed,
        b"and the node is still a character device",
        st.st_mode & 0o170_000 == 0o020_000,
    );
    let r = lin::chmod(DSP, 0o666);
    lin::check_eq(&mut failed, b"and the mode given back sticks too", r, 0);
    let r = lin::fstat(dsp, &raw mut st);
    lin::check_eq(&mut failed, b"fstat answers again", r, 0);
    lin::check_eq(
        &mut failed,
        b"with the number restored",
        (st.st_mode & 0o7777) as isize,
        0o666,
    );

    let _ = lin::close(dsp);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-dsp: panicked\n")
}
