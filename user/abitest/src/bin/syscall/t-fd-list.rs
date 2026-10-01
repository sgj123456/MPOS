//! t-fd-list - `/proc/<pid>/fd` must name every open descriptor.
//!
//! Pinned here: a process opens a known set of descriptors, then reads its own
//! `/proc/self/fd` with `getdents64` and has to find **every one** of them -
//! the three standard descriptors plus the ones it opened, whichever numbers
//! they landed on. The listing is what `ls -l /proc/<pid>/fd`, `udevadm` and a
//! debugger read to learn what a process holds; a listing that silently drops
//! entries is worse than an empty one, because the reader concludes the
//! descriptors are gone.
//!
//! This exists because a diagnosis of `niri` reported the listing going
//! `0, 1, 17, 18` - as if `2..=16` had been dropped. That is also exactly what
//! a process that opened and closed those numbers looks like, so the question
//! had to be settled by construction rather than by reading: this program opens
//! a contiguous run of descriptors and then checks the listing against what it
//! knows it holds. If the two disagree the kernel is dropping entries; if they
//! agree, the compositor's fd numbers were simply not contiguous.
//!
//! The names are compared as numbers, not strings, so fd 10 is checked as ten
//! and not as the byte `'1'` followed by `'0'`.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// How many descriptors this program deliberately holds, beyond the three
/// standard ones. Twelve is enough to cross the one-digit/two-digit boundary
/// and to cover the range that was reported missing.
const EXTRA: usize = 12;
/// One past the largest descriptor number the listing is scanned for.
const MAX_FD: usize = 64;

/// `/tmp/t-fd-list-<c>` is sixteen bytes, plus three NULs of slack.
static NAMES: [[u8; 19]; EXTRA] = [
    *b"/tmp/t-fd-list-0\0\0\0",
    *b"/tmp/t-fd-list-1\0\0\0",
    *b"/tmp/t-fd-list-2\0\0\0",
    *b"/tmp/t-fd-list-3\0\0\0",
    *b"/tmp/t-fd-list-4\0\0\0",
    *b"/tmp/t-fd-list-5\0\0\0",
    *b"/tmp/t-fd-list-6\0\0\0",
    *b"/tmp/t-fd-list-7\0\0\0",
    *b"/tmp/t-fd-list-8\0\0\0",
    *b"/tmp/t-fd-list-9\0\0\0",
    *b"/tmp/t-fd-list-a\0\0\0",
    *b"/tmp/t-fd-list-b\0\0\0",
];

/// The decimal name of a dirent, or `None` when it is not a number (`.` and
/// `..` are the two that matter).
fn parse_number(name: &[u8]) -> Option<usize> {
    let mut value = 0usize;
    let mut digits = 0;
    for &byte in name {
        if byte == 0 {
            break;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as usize;
        digits += 1;
        if digits > 6 {
            return None;
        }
    }
    (digits > 0).then_some(value)
}

/// Read `/proc/self/fd` whole, into a presence table and a count.
fn listing() -> ([bool; MAX_FD], usize) {
    static PATH: [u8; 15] = *b"/proc/self/fd\0\0";
    let mut present = [false; MAX_FD];
    let mut count = 0;
    let fd = lin::openat(lin::AT_FDCWD, &PATH, lin::O_RDONLY | lin::O_DIRECTORY, 0);
    if fd < 0 {
        return (present, count);
    }
    let fd = fd as i32;
    let mut buf = [0u8; 1024];
    loop {
        let n = lin::getdents64(fd, &mut buf);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        let mut off = 0usize;
        while off + 19 <= n {
            // `struct linux_dirent64`: d_ino(8) d_off(8) d_reclen(2)
            // d_type(1) d_name[] - the length at 16, the name at 19.
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            if reclen < 20 || off + reclen > n {
                break;
            }
            if let Some(number) = parse_number(&buf[off + 19..off + reclen]) {
                count += 1;
                if number < MAX_FD {
                    present[number] = true;
                }
            }
            off += reclen;
        }
    }
    lin::close(fd);
    (present, count)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fd-list\n");
    let mut failed = false;

    // A contiguous run of descriptors, opened the ordinary way: they take the
    // lowest free numbers, which is what makes the expectation unambiguous.
    let mut held = [0i32; EXTRA];
    let mut opened = 0usize;
    for (i, name) in NAMES.iter().enumerate() {
        let fd = lin::openat(
            lin::AT_FDCWD,
            name,
            lin::O_RDWR | lin::O_CREAT | lin::O_TRUNC,
            0o600,
        );
        if fd < 0 {
            lin::puts(b"FAIL: could not open the fixture file\n");
            failed = true;
            break;
        }
        held[i] = fd as i32;
        opened += 1;
    }

    let (present, count) = listing();
    lin::check_true(&mut failed, b"the listing could be read", count > 0);
    // The three standard descriptors are the floor: a process cannot be
    // without them, and a listing that omitted one would be lying about the
    // most fundamental thing in the table.
    for fd in 0..3 {
        lin::check_true(
            &mut failed,
            b"the listing names a standard descriptor",
            present[fd],
        );
    }
    // And every descriptor this program opened, at the number the open
    // actually returned - so a number the kernel reused is still checked.
    let mut missing = 0usize;
    for &fd in held.iter().take(opened) {
        if fd >= 0 && (fd as usize) < MAX_FD && !present[fd as usize] {
            missing += 1;
            lin::puts(b"FAIL: the listing is missing the descriptor\n");
        }
    }
    lin::check_eq(
        &mut failed,
        b"no opened descriptor is missing",
        missing as isize,
        0,
    );
    // The count has to be at least what is held: three standard plus the run.
    lin::check_true(
        &mut failed,
        b"the listing has at least one entry per held descriptor",
        count >= 3 + opened,
    );

    for &fd in held.iter().take(opened) {
        if fd >= 0 {
            lin::close(fd);
        }
    }
    for name in NAMES.iter() {
        lin::unlinkat(lin::AT_FDCWD, name, 0);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fd-list: panicked\n")
}
