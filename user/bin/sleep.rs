//! sleep: do nothing for a whole number of seconds.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = lin::Args { argc, argv };
    if argc < 2 {
        let _ = lin::write_all(2, b"sleep: how long?\n");
        lin::exit_group(1);
    }

    let mut secs: usize = 0;
    for &b in args.get(1) {
        if !b.is_ascii_digit() || secs > 100_000 {
            let _ = lin::write_all(2, b"sleep: not a number\n");
            lin::exit_group(1);
        }
        secs = secs * 10 + (b - b'0') as usize;
    }
    let ts = lin::TimeSpec {
        tv_sec: secs as i64,
        tv_nsec: 0,
    };
    let mut rem = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    let _ = lin::nanosleep(&ts, &mut rem);
    lin::exit_group(0);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"sleep: panicked\n")
}