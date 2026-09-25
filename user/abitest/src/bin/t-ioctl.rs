//! t-ioctl - one test for ioctl, the terminal-query door musl and every
//! line editor walk through (`ioctl(0, TIOCGWINSZ, &ws)` in the strace),
//! plus the two device families the boot's own services ask: the entropy
//! pool queries seedrng sends to `/dev/urandom`, and the clock reads and
//! writes hwclock sends to `/dev/rtc0`.
//!
//! Pinned here on fd 0, the console: TIOCGWINSZ writes the 25x80 window
//! the kernel serves, zeroing the pixel fields too; TCGETS answers 0 and
//! describes a console - a line at a time, echoed, interruptible - and
//! TCSETS changes what it describes, so a shell that asks for raw mode gets
//! one; a request the kernel has no handler for answers ENOTTY. The bad-fd
//! case is deliberately not exercised: this kernel answers on the request
//! alone, before any descriptor check, so man-faithful EBADF ordering is
//! not pinned here.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut WS: lin::Winsize = lin::Winsize {
    ws_row: 0,
    ws_col: 0,
    ws_xpixel: 0xFFFF,
    ws_ypixel: 0xFFFF,
};
static mut TERMIOS: [u8; 64] = [0xFF; 64];
/// The one-word answer slot for FIONREAD/RNDGETENTCNT, and the pool-info
/// pair RNDADDENTROPY takes, and the nine ints of `struct rtc_time`.
static mut WORD: i32 = -1;
static mut POOLINFO: [i32; 2] = [0; 2];
static mut RTCTIME: [i32; 9] = [-1; 9];

// The request words, spelled in their `_IOC` form: direction, size, the
// family byte at bits 8..15, the command in the low byte. FIONREAD is the
// legacy x86 number that predates the layout; the rest carry it exactly -
// `RND*` are 'R', `RTC_*` are 't', and the size field of the RTC pair is
// `struct rtc_time`'s 36 bytes, which is what pins the struct's shape here.
const FIONREAD: usize = 0x541B;
const RNDGETENTCNT: usize = 0x8004_5200;
const RNDADDENTROPY: usize = 0x4008_5202;
const RTC_RD_TIME: usize = 0x8024_7409;
const RTC_SET_TIME: usize = 0x4024_740A;

/// The `struct termios` glibc hands an ioctl: four flag words, a line
/// discipline, 32 control characters, then two speeds.
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; 32],
    ispeed: u32,
    ospeed: u32,
}

const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const VINTR: usize = 0;
const VEOF: usize = 4;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-ioctl\n");
    let mut failed = false;

    let r = lin::ioctl(0, lin::TIOCGWINSZ, &raw mut WS as usize);
    lin::check_eq(&mut failed, b"TIOCGWINSZ on the console succeeds", r, 0);
    let ws = unsafe { &*(&raw mut WS as *const lin::Winsize) };
    lin::check_eq(
        &mut failed,
        b"the console reports 25 rows",
        ws.ws_row as isize,
        25,
    );
    lin::check_eq(
        &mut failed,
        b"the console reports 80 columns",
        ws.ws_col as isize,
        80,
    );
    lin::check_eq(
        &mut failed,
        b"the pixel fields are zeroed, not left to the caller",
        (ws.ws_xpixel as isize) | (ws.ws_ypixel as isize),
        0,
    );

    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS on the console succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    // A console: a line at a time, and shown as it is typed.
    lin::check_true(
        &mut failed,
        b"the console is in canonical mode",
        tio.lflag & ICANON != 0,
    );
    lin::check_true(
        &mut failed,
        b"the console echoes what is typed",
        tio.lflag & ECHO != 0,
    );
    lin::check_true(
        &mut failed,
        b"the console turns the interrupt key into a signal",
        tio.lflag & ISIG != 0,
    );
    lin::check_eq(
        &mut failed,
        b"^C is the interrupt",
        tio.cc[VINTR] as isize,
        3,
    );
    lin::check_eq(&mut failed, b"^D is end of input", tio.cc[VEOF] as isize, 4);

    // TCSETS: what a shell does when it wants to do its own editing. Ask for
    // raw, read it back, and put the console back the way it was found - a
    // test that leaves the terminal in raw mode leaves the next line of the
    // script unreadable.
    let mut saved = *tio;
    let mut raw = saved;
    raw.lflag &= !(ICANON | ECHO);
    let r = lin::ioctl(0, lin::TCSETS, &raw mut raw as usize);
    lin::check_eq(&mut failed, b"TCSETS on the console succeeds", r, 0);

    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS after TCSETS succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    lin::check_eq(
        &mut failed,
        b"the settings came back as they were set",
        tio.lflag as isize,
        raw.lflag as isize,
    );
    lin::check_true(
        &mut failed,
        b"canonical mode is off, as asked",
        tio.lflag & ICANON == 0,
    );

    let r = lin::ioctl(0, lin::TCSETS, &raw mut saved as usize);
    lin::check_eq(&mut failed, b"restoring the console succeeds", r, 0);
    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS after restoring succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    lin::check_eq(
        &mut failed,
        b"the console is canonical again",
        tio.lflag as isize,
        saved.lflag as isize,
    );

    // A file is not a terminal, and says so. The path is a NUL-terminated
    // static, as in every other test: a bare `b"..."` literal carries no
    // terminator, and the kernel reads the path until the first zero byte -
    // which walks off the literal into whatever rodata happens to follow.
    // A file every initramfs and root filesystem here carries - the release
    // file belongs to a package the test image does not install, and a test
    // that needs a non-terminal descriptor should not depend on which.
    static HOSTNAME: [u8; 14] = *b"/etc/hostname\0";
    let fd = lin::openat(lin::AT_FDCWD, &HOSTNAME, lin::O_RDONLY, 0);
    let r = lin::ioctl(fd as i32, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(
        &mut failed,
        b"TCGETS on a file answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    lin::close(fd as i32);

    let r = lin::ioctl(0, 0xDEAD_BEEF, 0);
    lin::check_eq(
        &mut failed,
        b"an unknown request answers ENOTTY",
        r,
        lin::ENOTTY,
    );

    let r = lin::ioctl(1, lin::TIOCGWINSZ, &raw mut WS as usize);
    lin::check_eq(
        &mut failed,
        b"TIOCGWINSZ answers stdout the same console shape",
        r,
        0,
    );

    // ---- The entropy pool, as seedrng asks of it ----
    static URANDOM: [u8; 13] = *b"/dev/urandom\0";
    static RTC0: [u8; 10] = *b"/dev/rtc0\0";
    let urandom = lin::openat(lin::AT_FDCWD, &URANDOM, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/dev/urandom opens", urandom >= 0);

    let r = lin::ioctl(urandom as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"RNDGETENTCNT succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"the pool answers full (4096 bits)",
        unsafe { *(&raw mut WORD as *const i32) } as isize,
        4096,
    );

    let r = lin::ioctl(urandom as i32, FIONREAD, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"FIONREAD on the pool succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"FIONREAD reports the pool's bytes (4096 bits = 512)",
        unsafe { *(&raw mut WORD as *const i32) } as isize,
        512,
    );

    // RNDADDENTROPY credits a `struct poolinfo { int entropy; int size; }`.
    // This kernel has no accounting to receive the credit; what is pinned is
    // that the request is accepted rather than bounced as ENOTTY - which is
    // what made seedrng's boot line an error.
    unsafe {
        *(&raw mut POOLINFO as *mut [i32; 2]) = [64, 0];
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut POOLINFO as usize);
    lin::check_eq(&mut failed, b"RNDADDENTROPY is accepted", r, 0);

    // A device that is not the pool refuses the pool's requests, and the
    // pool refuses the clock's - the families are keyed on the device, not
    // on whoever asks.
    let r = lin::ioctl(urandom as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"RTC_RD_TIME on urandom answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    lin::close(urandom as i32);

    // ---- The clock, as hwclock asks of it ----
    let rtc = lin::openat(lin::AT_FDCWD, &RTC0, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/rtc0 opens", rtc >= 0);

    let r = lin::ioctl(rtc as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_RD_TIME succeeds", r, 0);
    let t = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"the clock's fields are in range",
        t[0] < 60 && t[1] < 60 && t[2] < 24 && (1..=31).contains(&t[3]) && (0..12).contains(&t[4]),
    );
    lin::check_true(
        &mut failed,
        b"the clock reads the post-2020 world it was booted from (tm_year since 1900)",
        t[5] >= 120,
    );
    lin::check_eq(&mut failed, b"tm_isdst is 0", t[8] as isize, 0);

    // The same request spelled with the older RTC magic byte - `'p'`, which
    // is what Alpine's busybox hwclock actually sends, its `linux/rtc.h`
    // predating the kernel's move to `'t'`. Same struct, same command, the
    // same answer; and a `'p'` request that is *not* one of the two clock
    // commands stays ENOTTY, because `'p'` is PPP's family too.
    let r = lin::ioctl(rtc as i32, 0x8024_7009, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"the 'p'-spelled RTC_RD_TIME succeeds", r, 0);
    let tp = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"both spellings read the same date",
        tp[3] == t[3] && tp[4] == t[4] && tp[5] == t[5],
    );
    let r = lin::ioctl(rtc as i32, 0x8024_7020, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"a stray 'p'-typed request stays ENOTTY",
        r,
        lin::ENOTTY,
    );

    // RTC_SET_TIME of what was just read back: the store round-trips, and a
    // second read agrees on the date. (The seconds may tick between the
    // calls, so the time-of-day fields are not compared.)
    let r = lin::ioctl(rtc as i32, RTC_SET_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_SET_TIME succeeds", r, 0);
    let r = lin::ioctl(rtc as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_RD_TIME after the set succeeds", r, 0);
    let t2 = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"the date reads back unchanged",
        t[3] == t2[3] && t[4] == t2[4] && t[5] == t2[5],
    );

    // A nonsense date is refused rather than stored.
    unsafe {
        *(&raw mut RTCTIME as *mut [i32; 9]) = [0, 0, 0, 31, 12, 80, 0, 0, 0];
    }
    let r = lin::ioctl(rtc as i32, RTC_SET_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"RTC_SET_TIME of month 13 answers EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::ioctl(rtc as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(
        &mut failed,
        b"RNDGETENTCNT on rtc0 answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    lin::close(rtc as i32);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-ioctl: panicked\n")
}
