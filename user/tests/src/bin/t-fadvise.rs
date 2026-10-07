//! t-fadvise - `posix_fadvise`'s advice vocabulary is closed, not a blanket
//! accept.
//!
//! `posix_fadvise` is a hint, so a *recognised* advice answered `0` is honest
//! even on a kernel that prefetches nothing. The failure this pins is the other
//! half: the call used to answer `0` for **any** advice on **any** open
//! descriptor - a number no one gave it, and a pipe, which has no byte range to
//! advise about. That is the §4.2 success-for-a-thing-not-done: the caller was
//! told the kernel had accepted a request it was never asked to keep.
//!
//! Measured on a current host (glibc `posix_fadvise` on x86_64, whose raw
//! syscall answers `0` or `-errno` directly):
//!
//!  * advice `NORMAL(0)`, `DONTNEED(4)`, `NOREUSE(5)` - the recognised set
//!    `0..=5` - answers `0`;
//!  * advice `6` and a past-the-set number like `99` answer `EINVAL`;
//!  * a descriptor that is not open answers `EBADF`;
//!  * a pipe answers `ESPIPE`, not the `0` a page-cacheable file would get.
//!
//! # Why two of the nine claims are `libc`
//!
//! The advice word is this program's subject, and seven of the nine claims pass
//! one that `rustix::fs::Advice` names. The other two pass `6` and `99`, which
//! it does not: `Advice` is a plain *enum* of the six `POSIX_FADV_*` values, so
//! an out-of-vocabulary word has nowhere to go in it at all - the `libc` rule's
//! clause (c), the same shape as `t-lseek`'s bogus whence. (A rustix bitflags
//! type is not like this: it carries bitflags' unnamed flag `const _ = !0`, so
//! it *can* hold an unnamed word through `from_bits_retain`, which is why
//! `t-fallocate` needs no `libc` for its unnamed mode bit.)
//!
//! Those two go through `libc::posix_fadvise` - the named wrapper, as `t-dup3`
//! uses `libc::dup3` and `t-getcwd` uses `libc::getcwd`, rather than a raw
//! `libc::syscall` and a number. One consequence is the comparison's shape:
//! POSIX's `posix_fadvise` *returns the error number* rather than `-1` with
//! `errno` set, so those claims compare against `libc::EINVAL` directly while
//! the `rustix` ones compare against `Err(Errno::INVAL)`.
//!
//! # What this pins on each machine
//!
//! `fadvise64` carries a real number on both (`nr_aarch64.rs`: `FADVISE64 =
//! 223`; `nr_x86_64.rs`: `FADVISE64 = 221`) and both reach the same
//! `handle_fadvise64`, so the per-claim output is the same on both machines.
//!
//! This program is the batch's cheapest on fixtures, and deliberately: the
//! regular file it advises is a `memfd`, not a path. A memfd is a regular file
//! with a name and no directory to be found in, so there is nothing to write and
//! nothing to clean up - and the pipe it refuses comes from `pipe_with`, not
//! from a payload path either.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::fs::{Advice, MemfdFlags, fadvise, memfd_create};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};

/// The first advice past the recognised set (`NOREUSE` is 5), and one far past
/// it. Written down because they are deliberately outside every table - the
/// claim is precisely that a word nothing names is refused.
const PAST_NOREUSE: i32 = 6;
const FAR_PAST_THE_SET: i32 = 99;

/// A descriptor nothing is open on. The number is not part of the claim - the
/// descriptor table having no such row is - and this is the one the no-std
/// version measured with.
const UNOPENED: i32 = 0x3FFF;

fn main() {
    println!("t-fadvise");
    let mut r = Report::new();

    // A memfd is a regular file, so a recognised advice is accepted as the hint
    // it is.
    let Ok(fd) = memfd_create("t-fadvise", MemfdFlags::empty()) else {
        r.check("memfd_create hands back a descriptor", false);
        r.finish()
    };
    r.check("memfd_create hands back a descriptor", true);

    r.check(
        "POSIX_FADV_NORMAL (0) on a regular file answers 0",
        fadvise(&fd, 0, None, Advice::Normal) == Ok(()),
    );
    r.check(
        "POSIX_FADV_DONTNEED (4) on a regular file answers 0",
        fadvise(&fd, 0, None, Advice::DontNeed) == Ok(()),
    );
    r.check(
        "POSIX_FADV_NOREUSE (5) - the last recognised advice - answers 0",
        fadvise(&fd, 0, None, Advice::NoReuse) == Ok(()),
    );

    // The vocabulary is closed: the first number past the recognised set, and a
    // number far past it, are EINVAL. Both were a silent 0 before this shipped.
    // POSIX's wrapper returns the error number itself, so these read as the
    // number rather than as -1 with an errno beside it.
    r.check(
        "advice 6 (past NOREUSE) answers EINVAL, not a silent 0",
        unsafe { libc::posix_fadvise(fd.as_raw_fd(), 0, 0, PAST_NOREUSE) } == libc::EINVAL,
    );
    r.check(
        "advice 99 answers EINVAL",
        unsafe { libc::posix_fadvise(fd.as_raw_fd(), 0, 0, FAR_PAST_THE_SET) } == libc::EINVAL,
    );

    // A descriptor that is not open is EBADF - and it is asked of the *first*
    // thing the handler does, so a valid advice does not rescue it.
    let unopened = unsafe { BorrowedFd::borrow_raw(UNOPENED) };
    r.check(
        "an unopened descriptor is EBADF even with a valid advice",
        fadvise(unopened, 0, None, Advice::Normal) == Err(Errno::BADF),
    );

    // A pipe has no byte range to advise about, so even a recognised advice is
    // ESPIPE rather than the 0 a regular file gets. This was a silent 0 before.
    match pipe_with(PipeFlags::empty()) {
        Ok((read_end, _write_end)) => {
            r.check("pipe2 succeeds", true);
            r.check(
                "a pipe is ESPIPE, not the 0 a page-cacheable file gets",
                fadvise(&read_end, 0, None, Advice::Normal) == Err(Errno::SPIPE),
            );
        }
        Err(_) => {
            r.check("pipe2 succeeds", false);
            r.check(
                "a pipe is ESPIPE, not the 0 a page-cacheable file gets",
                false,
            );
        }
    }

    r.finish()
}
