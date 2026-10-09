//! t-read - one test for read, which a trace of a musl program uses to drain
//! `/etc/ld.so.cache` and `/proc/self/maps` three pages at a time.
//!
//! Pinned here on a real file: the first read hands back 1..n bytes, more reads
//! drain the rest, and the read at end-of-file answers 0 - the EOF marker every
//! musl reader loops on. Short reads are legal, so the count is only bounded,
//! never pinned. Also pinned: a zero-length read answers 0, a descriptor that
//! does not exist answers `EBADF`, and a buffer with no readable pages behind it
//! answers `EFAULT`.
//!
//! The file is one this test writes itself under `/tmp`, not `/etc/motd` as the
//! no-std version used: the claim is about what `read` does, not about which
//! file it is doing it to, and the second machine's test payload carries `/tmp`
//! and not a motd (`NOTES.md` §15.11). The unreadable buffer is a `PROT_NONE`
//! mapping at an address the kernel chooses, and it is probed with a descriptor
//! this test *knows* is open, so the answer cannot be confused with `EBADF`.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{Errno, read};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};

/// The file this test owns. Under `/tmp` because that is the one scratch
/// directory both machines' test payloads carry.
const FILE: &str = "/tmp/t-read_file";
/// Longer than one read needs, and short enough to fit in one page.
const CONTENT: &[u8] = b"mpos t-read fixture: enough bytes to drain in a few reads.\n";

fn main() {
    println!("t-read");
    let mut r = Report::new();

    if std::fs::write(FILE, CONTENT).is_err() {
        r.check("the file to read is made", false);
        r.finish()
    }

    let Ok(file) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("the file to read opens", false);
        r.finish()
    };
    r.check("the file to read opens", true);

    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    let mut eof = false;
    for _ in 0..8 {
        match read(&file, &mut buf) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    r.check("the first read hands back bytes", total > 0);
    r.check("reading past the end answers 0", eof);

    // Spelled on the binding: an empty literal slice is a `MaybeUninit` slice as
    // far as inference is concerned, and `read` takes both.
    let empty: &mut [u8] = &mut [];
    r.check("a zero-length read answers 0", read(&file, empty) == Ok(0));

    let missing = unsafe { BorrowedFd::borrow_raw(999) };
    r.check(
        "a descriptor that does not exist answers EBADF",
        read(missing, &mut buf) == Err(Errno::BADF),
    );

    // SAFETY: `PROT_NONE` at an address the kernel picked - the page exists and
    // cannot be read. Nothing here dereferences it.
    let guard = unsafe {
        mmap_anonymous(
            core::ptr::null_mut(),
            4096,
            ProtFlags::empty(),
            MapFlags::PRIVATE,
        )
    };
    match guard {
        Ok(guard) => {
            // SAFETY: the slice is built over the mapping and never read by
            // Rust - `read` receives its pointer and length and the kernel
            // refuses the call with EFAULT. The descriptor is the one opened
            // above, so the answer cannot be EBADF.
            let dst = unsafe { core::slice::from_raw_parts_mut(guard as *mut u8, 16) };
            r.check(
                "a read into unreadable memory answers EFAULT",
                read(&file, dst) == Err(Errno::FAULT),
            );
        }
        Err(_) => r.check("a read into unreadable memory answers EFAULT", false),
    }

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
