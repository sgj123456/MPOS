//! t-dsp - the OSS parameter face of /dev/dsp.
//!
//! The contract this pins is that the *writer* owns the numbers: rate,
//! channels and format are asked about and changed with the `SNDCTL_DSP_*`
//! ioctls, exactly as linux/soundcard.h spells them, and a parameter the
//! stream will not take is refused with `EINVAL` rather than answered with a
//! success that changed nothing. A device-less boot simply has no node to
//! open, which is its own honest answer - the test fails loudly there,
//! because the gate machine is configured with the card.
//!
//! # The ioctl words are the writer's own arithmetic
//!
//! Neither library binds the OSS family (clause (a): libc has no
//! `SNDCTL_DSP_*` constants on Linux at all), so the `_IOC` words are
//! spelled here exactly as `linux/soundcard.h` computes them - they are the
//! ABI, not a remembered number. This kernel serves `/dev/dsp` on both
//! machines (the same in-tree driver behind both consoles' audio), so the
//! program runs in both sweeps; a host without an OSS card answers ENOENT
//! for the node and the hosted run accepts that as the honest absence.
//!
//! The mode check moved off the x86_64 `lin::Stat`: `rustix::fs::fstat`
//! reports the machine's own `st_mode`, batch 12's pattern.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, chmod, fstat, openat};
use rustix::io::write;

const DSP: &str = "/dev/dsp";
const INIT: &str = "/init";

/// The `_IOC` word, spelled as `linux/soundcard.h` computes it:
/// `dir << 30 | size << 16 | 'P' << 8 | nr`.
const fn ioc(dir: usize, size: usize, nr: usize) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | ((b'P' as usize) << 8) | nr) as libc::c_ulong
}
const SNDCTL_DSP_RESET: libc::c_ulong = ioc(0, 0, 0);
const SNDCTL_DSP_SYNC: libc::c_ulong = ioc(0, 0, 1);
const SNDCTL_DSP_SPEED: libc::c_ulong = ioc(3, 4, 2);
const SNDCTL_DSP_STEREO: libc::c_ulong = ioc(3, 4, 3);
const SNDCTL_DSP_GETBLKSIZE: libc::c_ulong = ioc(3, 4, 4);
const SNDCTL_DSP_SETFMT: libc::c_ulong = ioc(3, 4, 5);
const SNDCTL_DSP_CHANNELS: libc::c_ulong = ioc(3, 4, 6);
const SNDCTL_DSP_POST: libc::c_ulong = ioc(0, 0, 8);
const SNDCTL_DSP_SETFRAGMENT: libc::c_ulong = ioc(3, 4, 10);
const SNDCTL_DSP_GETFMTS: libc::c_ulong = ioc(2, 4, 11);
const SNDCTL_DSP_GETOSPACE: libc::c_ulong = ioc(2, 16, 12);
const SNDCTL_DSP_NONBLOCK: libc::c_ulong = ioc(0, 0, 14);

const AFMT_S16_LE: i32 = 0x0000_0010;
const AFMT_MU_LAW: i32 = 0x0000_0001;

/// One `ioctl`, and the answer the *caller* sees.
///
/// Through `libc::ioctl`, which is a `syscall`-based wrapper in musl: it
/// reports a refusal the way C does, as `-1` with the errno left behind, and
/// **not** as the raw `-errno` the kernel returned. The claims below that
/// expect a refusal therefore compare through [`refused`] rather than against
/// `-(libc::EINVAL as isize)` - a raw comparison can never be true through
/// this door, and it read as six device refusals failing when the device was
/// refusing correctly all along (`t-rtc`/`t-vt` reach ioctl through
/// `libc::syscall` and read errno on `rc < 0` for the same reason).
fn io(fd: i32, req: libc::c_ulong, arg: usize) -> isize {
    unsafe { libc::ioctl(fd, req as _, arg) as isize }
}

/// Whether a call came back refused with exactly `want`.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-dsp");
    let mut r = Report::new();

    let dsp = match openat(CWD, DSP, OFlags::RDWR, Mode::empty()) {
        Ok(fd) => {
            r.check("/dev/dsp opens", true);
            fd
        }
        Err(_) => {
            // On a host with no OSS card the node is simply absent; under a
            // payload sweep it must exist (the gate machine is configured
            // with the card), so only the hosted run forgives it.
            r.check("/dev/dsp opens", mpos_user_tests::host_run());
            r.finish();
        }
    };
    let raw = dsp.as_raw_fd();

    // --- speed: query, set, and the answer is what the device runs ---
    let mut rate: i32 = 0;
    let rr = io(raw, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    r.check("SPEED(0) answers the running rate", rr == 0);
    let running = rate;
    r.check("and it is a rate", running > 0);

    let mut rate = running;
    let rr = io(raw, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    r.check("setting SPEED to the running rate succeeds", rr == 0);
    r.check("and answers the same rate", rate == running);

    // A rate the device does not list rounds onto the one it does, and the
    // ioctl reports the rounding rather than hiding it. 44101 is no device's
    // rate; the only promise is that the answer is some rate that exists now.
    let mut rate = 44101;
    let rr = io(raw, SNDCTL_DSP_SPEED, &raw mut rate as usize);
    r.check("a near-miss rate is still accepted", rr == 0);
    r.check(
        "and answered with the rate it settled on",
        rate > 0 && rate != 44101,
    );

    // --- format: S16LE is the stream, and the only format claimed ---
    let mut fmt: i32 = 0;
    let rr = io(raw, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    r.check("SETFMT(0) answers the running format", rr == 0);
    r.check("and it is AFMT_S16_LE", fmt == AFMT_S16_LE);

    let mut fmt = AFMT_S16_LE;
    let rr = io(raw, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    r.check("SETFMT(S16_LE) succeeds", rr == 0);
    r.check("and answers the format", fmt == AFMT_S16_LE);

    let mut fmt = AFMT_MU_LAW;
    let rr = io(raw, SNDCTL_DSP_SETFMT, &raw mut fmt as usize);
    r.check(
        "a format the stream does not run is EINVAL, not a yes",
        refused(rr, libc::EINVAL),
    );

    let mut mask: i32 = 0;
    let rr = io(raw, SNDCTL_DSP_GETFMTS, &raw mut mask as usize);
    r.check("GETFMTS answers", rr == 0);
    r.check("and the mask names S16_LE only", mask == AFMT_S16_LE);

    // --- channels and the stereo spelling of the same number ---
    let mut ch: i32 = 0;
    let rr = io(raw, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    r.check("CHANNELS(0) answers the running count", rr == 0);
    r.check("and it is at least one", ch >= 1);

    let mut ch = 1;
    let rr = io(raw, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    r.check("CHANNELS(1) is honoured", rr == 0);
    r.check("and answers mono", ch == 1);

    let mut stereo = 1;
    let rr = io(raw, SNDCTL_DSP_STEREO, &raw mut stereo as usize);
    r.check("STEREO(1) is honoured", rr == 0);
    let mut ch: i32 = 0;
    let _ = io(raw, SNDCTL_DSP_CHANNELS, &raw mut ch as usize);
    r.check("and the channel count now says two", ch == 2);

    let mut bogus: i32 = 100;
    let rr = io(raw, SNDCTL_DSP_CHANNELS, &raw mut bogus as usize);
    r.check(
        "a channel count outside the device is EINVAL",
        refused(rr, libc::EINVAL),
    );

    // --- the geometry the writers pace against ---
    let mut blk: i32 = 0;
    let rr = io(raw, SNDCTL_DSP_GETBLKSIZE, &raw mut blk as usize);
    r.check("GETBLKSIZE answers", rr == 0);
    r.check("and it is a fragment's worth", blk >= 4096);

    let mut info = [0i32; 4];
    let rr = io(raw, SNDCTL_DSP_GETOSPACE, &raw mut info as usize);
    r.check("GETOSPACE answers", rr == 0);
    r.check(
        "and the geometry adds up",
        info[1] >= 1 && info[2] >= 1 && info[3] >= info[0] * info[2],
    );

    // --- the refusals ---
    // The fragment word is `size | count << 16`; the driver takes the request
    // and refuses the change, which is what the claim is about.
    let mut frag: i32 = (4096) | (8 << 16);
    let rr = io(raw, SNDCTL_DSP_SETFRAGMENT, &raw mut frag as usize);
    r.check("SETFRAGMENT is refused honestly", refused(rr, libc::EINVAL));
    let rr = io(raw, SNDCTL_DSP_NONBLOCK, 0);
    r.check("NONBLOCK is refused honestly", refused(rr, libc::EINVAL));

    // --- the no-argument trio ---
    r.check("RESET answers 0", io(raw, SNDCTL_DSP_RESET, 0) == 0);
    r.check("SYNC answers 0", io(raw, SNDCTL_DSP_SYNC, 0) == 0);
    r.check("POST answers 0", io(raw, SNDCTL_DSP_POST, 0) == 0);

    // --- and bytes still play ---
    let silence = [0u8; 512];
    r.check(
        "a frame buffer writes through",
        write(&dsp, &silence) == Ok(512),
    );

    // A stream with no parameters is `/dev/null`, and the family is as
    // unknown there as any ioctl is on Linux's mixer-less null device.
    let null = openat(CWD, "/dev/null", OFlags::RDWR, Mode::empty());
    r.check("/dev/null opens", null.is_ok());
    if let Ok(n) = &null {
        let mut v: i32 = 0;
        let rr = io(n.as_raw_fd(), SNDCTL_DSP_SPEED, &raw mut v as usize);
        r.check("and SPEED on it is ENOTTY", refused(rr, libc::ENOTTY));
    } else {
        r.check("and SPEED on it is ENOTTY", false);
    }
    drop(null);

    // A regular file is the other half of the same refusal.
    let init = openat(CWD, INIT, OFlags::RDONLY, Mode::empty());
    r.check("/init opens", init.is_ok());
    if let Ok(i) = &init {
        let mut v: i32 = 0;
        let rr = io(i.as_raw_fd(), SNDCTL_DSP_SPEED, &raw mut v as usize);
        r.check(
            "and SPEED on a regular file is ENOTTY",
            refused(rr, libc::ENOTTY),
        );
    } else {
        r.check("and SPEED on a regular file is ENOTTY", false);
    }
    drop(init);

    // --- chmod on a device node really changes it ---
    //
    // The other half of the same promise: a `chmod(2)` that answers 0 while
    // the node keeps nothing is what left udev's `MODE=` rules as logs. The
    // number set here is the number `fstat` reports afterwards, and the
    // node is given its mode back before the test ends.
    let chmodded = chmod(DSP, Mode::from_bits_retain(0o600));
    r.check(
        "chmod 0600 on /dev/dsp succeeds",
        chmodded.is_ok() || mpos_user_tests::host_run(),
    );
    let st = fstat(&dsp);
    r.check("fstat answers", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "and the mode it reports is the one that was set",
            Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o600 || mpos_user_tests::host_run(),
        );
        r.check(
            "and the node is still a character device",
            st.st_mode & 0o170_000 == 0o020_000,
        );
    } else {
        r.check("and the mode it reports is the one that was set", false);
        r.check("and the node is still a character device", false);
    }
    let back = chmod(DSP, Mode::from_bits_retain(0o666));
    r.check(
        "and the mode given back sticks too",
        back.is_ok() || mpos_user_tests::host_run(),
    );
    let st = fstat(&dsp);
    r.check("fstat answers again", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "with the number restored",
            Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o666 || mpos_user_tests::host_run(),
        );
    } else {
        r.check("with the number restored", false);
    }

    drop(dsp);
    r.finish()
}
