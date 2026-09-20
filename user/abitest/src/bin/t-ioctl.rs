//! t-ioctl - one test for ioctl, the terminal-query door musl and every
//! line editor walk through (`ioctl(0, TIOCGWINSZ, &ws)` in the strace).
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
    lin::check_eq(&mut failed, b"the console reports 25 rows", ws.ws_row as isize, 25);
    lin::check_eq(&mut failed, b"the console reports 80 columns", ws.ws_col as isize, 80);
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
    lin::check_eq(&mut failed, b"^C is the interrupt", tio.cc[VINTR] as isize, 3);
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

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-ioctl: panicked\n")
}
