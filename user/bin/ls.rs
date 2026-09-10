//! ls: the names in one directory, one per line, dots skipped.
//!
//! The listing crosses the gate as one blob - kind byte, name, NUL, per
//! entry - sized by a first call with an empty buffer, then read with a
//! buffer of exactly that size.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

const BUF_MAX: usize = 4096;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = sys::Args { argc, argv };
    let target = if argc > 1 { args.get(1) } else { b"." as &[u8] };

    let mut buf = [0u8; BUF_MAX];
    let need = sys::listdir(target, &mut []);
    if need < 0 {
        sys::exit(cannot_list(target));
    }
    let need = need as usize;
    if need > BUF_MAX {
        let _ = sys::write_all(2, b"ls: listing too large\n");
        sys::exit(1);
    }
    let n = sys::listdir(target, &mut buf[..need]);
    if n < 0 {
        sys::exit(cannot_list(target));
    }

    // Walk the stream: kind byte, name, NUL, over and over.
    let n = n as usize;
    let mut i = 0usize;
    while i < n {
        let _kind = buf[i];
        i += 1;
        let end = buf[i..n].iter().position(|&b| b == 0).expect("a NUL ends every name") + i;
        let name = &buf[i..end];
        i = end + 1;
        // POSIX readdir reports them; ls does not print them.
        if name == b"." || name == b".." {
            continue;
        }
        if sys::write_all(1, name) < 0 || sys::write_all(1, b"\n") < 0 {
            sys::exit(1);
        }
    }

    sys::exit(0);
}

fn cannot_list(target: &[u8]) -> i32 {
    let _ = sys::write_all(2, b"ls: ");
    let _ = sys::write_all(2, target);
    let _ = sys::write_all(2, b": cannot list\n");
    1
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
