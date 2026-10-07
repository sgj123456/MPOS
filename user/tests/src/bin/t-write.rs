//! t-write - one test for write, the one syscall that produces a musl binary's
//! output: `write(1, "Hello, world!\n", 14) = 14`.
//!
//! The test writes that exact payload to fd 1, a line to fd 2, and asks for the
//! exact byte counts back - a short or negative answer on a live console fd is a
//! bug. Then it writes to a descriptor that does not exist and expects `EBADF`,
//! the error shape every musl wrapper keys on. Also pinned: a zero-length write
//! answers 0, and a source buffer with no readable pages behind it answers
//! `EFAULT`.
//!
//! The unreadable buffer is a `PROT_NONE` mapping at an address the kernel
//! chooses, rather than the no-std suite's fixed `0x5000_0000`: the claim is
//! "this page cannot be read", and which page it is - or where the machine's
//! address space happens to have room - is not part of it. The reference is
//! never dereferenced by Rust; only its pointer and length reach the kernel,
//! which refuses the whole call with `EFAULT`.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::io::{Errno, write};
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};

const HELLO: &[u8] = b"Hello, world!\n";
const STDERR_LINE: &[u8] = b"t-write: stderr line\n";

fn main() {
    println!("t-write");
    let mut r = Report::new();

    // The standard descriptors are open by the runner's own contract - it
    // spawns this program with 0, 1 and 2 connected.
    let (stdout, stderr) = unsafe { (BorrowedFd::borrow_raw(1), BorrowedFd::borrow_raw(2)) };

    r.check(
        "write(1, ...) reports every byte",
        write(stdout, HELLO) == Ok(HELLO.len()),
    );
    r.check(
        "write(2, ...) reports every byte",
        write(stderr, STDERR_LINE) == Ok(STDERR_LINE.len()),
    );

    // A number nothing is open on. The no-std version used `-1`, which `rustix`
    // deliberately will not spell: `BorrowedFd::borrow_raw` refuses a negative
    // raw fd outright (`assert!(fd != -1)`), because claiming a negative number
    // is a descriptor is the kind of borrow its contract exists to prevent. The
    // kernel's answer is the same branch either way - the descriptor table has
    // no such row - and 999 is the number the `t-read` migration uses too.
    let missing = unsafe { BorrowedFd::borrow_raw(999) };
    r.check(
        "a descriptor that does not exist answers EBADF",
        write(missing, HELLO) == Err(Errno::BADF),
    );

    r.check("a zero-length write answers 0", write(stdout, &[]) == Ok(0));

    // SAFETY: `PROT_NONE` at an address the kernel picked, which is the point -
    // the page exists and cannot be read. Nothing here dereferences it.
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
            // Rust - `write` receives its pointer and length and the kernel
            // refuses the call with EFAULT.
            let src = unsafe { core::slice::from_raw_parts(guard as *const u8, 16) };
            r.check(
                "a write from unreadable memory answers EFAULT",
                write(stdout, src) == Err(Errno::FAULT),
            );
        }
        Err(_) => r.check("a write from unreadable memory answers EFAULT", false),
    }

    r.finish()
}
