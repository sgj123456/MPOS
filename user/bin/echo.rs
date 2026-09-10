//! echo: its arguments, one space apart, one newline at the end.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = sys::Args { argc, argv };

    // The kernel caps the whole argv at 512 bytes; without the terminators
    // the joined line always fits one fixed buffer with room to spare.
    let mut out = [0u8; 512];
    let mut n = 0usize;
    for i in 1..argc {
        if i > 1 {
            out[n] = b' ';
            n += 1;
        }
        let arg = args.get(i);
        out[n..n + arg.len()].copy_from_slice(arg);
        n += arg.len();
    }
    out[n] = b'\n';
    n += 1;

    if sys::write_all(1, &out[..n]) < 0 {
        sys::exit(1);
    }
    sys::exit(0);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
