//! sleep: do nothing for a whole number of seconds.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = sys::Args { argc, argv };
    if argc < 2 {
        let _ = sys::write_all(2, b"sleep: how long?\n");
        sys::exit(1);
    }

    let mut secs: usize = 0;
    for &b in args.get(1) {
        if !b.is_ascii_digit() || secs > 100_000 {
            let _ = sys::write_all(2, b"sleep: not a number\n");
            sys::exit(1);
        }
        secs = secs * 10 + (b - b'0') as usize;
    }
    sys::sleep_ms(secs * 1000);
    sys::exit(0);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
