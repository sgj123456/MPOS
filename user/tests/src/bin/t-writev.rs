//! t-writev - one test for writev, the gather write behind every printf
//! that joins a prefix and a line without copying them together first.
//!
//! Pinned here: two iovecs to the console report exactly the sum of their
//! lengths - a short count on a live console fd is a bug - and a descriptor
//! that does not exist answers EBADF rather than a partial sum.
//!
//! `rustix::io::writev` binds the call with the caller's own `IoSlice`
//! array, which is the shape the ABI promises, so the whole program is
//! rustix; only the dead-descriptor probe leaves it, because
//! `BorrowedFd::borrow_raw` refuses a negative raw fd by contract and 99
//! stands in for the no-std version's `-1` (the same branch of the handler,
//! as batch 3 measured for `t-write`).

use mpos_user_tests::Report;
use rustix::io::{IoSlice, writev};

const HEAD: &[u8] = b"t-writev: ";
const TAIL: &[u8] = b"one line\n";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-writev");
    let mut r = Report::new();

    // The gather write onto this program's own stdout, which on the sweep
    // machine is the armed console. `stdout()` borrows fd 1 for exactly as
    // long as the call needs it.
    let iov = [IoSlice::new(HEAD), IoSlice::new(TAIL)];
    match writev(std::io::stdout(), &iov) {
        Ok(n) => r.check(
            "writev(1) reports exactly both lengths (10 + 9)",
            n == HEAD.len() + TAIL.len(),
        ),
        Err(_) => r.check("writev(1) reports exactly both lengths (10 + 9)", false),
    }

    let n = unsafe { libc::syscall(libc::SYS_writev, 99i32, &iov as *const _ as usize, 2usize) };
    r.check(
        "a descriptor that does not exist answers EBADF",
        refused(n as isize, libc::EBADF),
    );

    r.finish()
}
