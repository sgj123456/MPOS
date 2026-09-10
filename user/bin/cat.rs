//! cat: every file named on the command line, in order, to fd 1. With no
//! arguments it copies stdin, the way the manual says.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = sys::Args { argc, argv };
    let mut failed = false;

    if argc <= 1 {
        failed = copy(0) < 0;
    } else {
        for i in 1..argc {
            let name = args.get(i);
            let fd = sys::open(name);
            if fd < 0 {
                let _ = sys::write_all(2, b"cat: ");
                let _ = sys::write_all(2, name);
                let _ = sys::write_all(2, b": cannot open\n");
                failed = true;
                continue;
            }
            if copy(fd) < 0 {
                failed = true;
            }
            let _ = sys::close(fd as i32);
        }
    }

    sys::exit(if failed { 1 } else { 0 });
}

/// stdin to stdout for one descriptor, until end of input or error.
fn copy(fd: isize) -> isize {
    let mut buf = [0u8; 256];
    loop {
        let n = sys::read(fd as i32, &mut buf);
        if n <= 0 {
            return n;
        }
        if sys::write_all(1, &buf[..n as usize]) < 0 {
            return -1;
        }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
