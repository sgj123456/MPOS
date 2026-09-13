//! t_ioctl - one test for ioctl, the terminal-query door musl and every
//! line editor walk through (`ioctl(0, TIOCGWINSZ, &ws)` in the strace).
//!
//! Pinned here on fd 0, the console: TIOCGWINSZ writes the 25x80 window
//! the kernel serves, zeroing the pixel fields too; TCGETS answers 0 and
//! zeroes the termios buffer it is handed; a request the kernel has no
//! handler for answers ENOTTY. The bad-fd case is deliberately not
//! exercised: this kernel answers on the request alone, before any
//! descriptor check, so man-faithful EBADF ordering is not pinned here.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut WS: lin::Winsize = lin::Winsize {
    ws_row: 0,
    ws_col: 0,
    ws_xpixel: 0xFFFF,
    ws_ypixel: 0xFFFF,
};
static mut TERMIOS: [u8; 64] = [0xFF; 64];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_ioctl\n");
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
    let termios = unsafe { core::slice::from_raw_parts(&raw mut TERMIOS as *const u8, 64) };
    let mut clean = true;
    for i in 0..8 {
        if termios[i] != 0 {
            clean = false;
        }
    }
    lin::check_true(
        &mut failed,
        b"TCGETS zeroes the termios buffer it fills",
        clean,
    );

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
    lin::die(b"t_ioctl: panicked\n")
}
