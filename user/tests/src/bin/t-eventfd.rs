//! t-eventfd - the counter a process waits on, reached through a descriptor.
//!
//! Pinned here, per eventfd(2): the create flags are `EFD_NONBLOCK` and its
//! siblings, the counter starts at the requested initial value, a read of a
//! non-empty counter takes the whole value and leaves zero behind, two writes
//! add before one read takes the sum, and a read or a write shorter than the
//! eight bytes the ABI fixes is `EINVAL`.
//!
//! Where mpos diverges: this kernel offers only the whole-count take. Linux's
//! `EFD_SEMAPHORE` asks for a read to decrement by one and hand back one;
//! that mode is not implemented here, and the create call says so with
//! `EOPNOTSUPP` rather than granting a descriptor whose reads would quietly
//! mean something else. The check below pins that refusal, not a semaphore.
//! (On real Linux EFD_SEMAPHORE *is* served, so the claim accepts a created
//! descriptor there as Linux's own answer - named in code, pinned here.)
//!
//! A blocking read of a zero counter parks until someone writes; every empty
//! read here is therefore on an `EFD_NONBLOCK` descriptor, where the ABI's
//! answer is `EAGAIN`.
//!
//! Pure rustix beside one raw door: `rustix::eventfd` binds the create with
//! typed flags, but the unknown-bit claim needs a word outside its
//! vocabulary - bitflags' escape covers bits rustix *names*, not a number it
//! has never heard of - so that one probe is the raw call (`SYS_eventfd2`:
//! 290 / 19).

use mpos_user_tests::Report;
use rustix::event::{EventfdFlags, eventfd};
use rustix::io::{Errno, FdFlags, fcntl_getfd, read, write};

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-eventfd");
    let mut r = Report::new();

    // A fresh counter, nonblocking: an empty read answers EAGAIN rather than
    // parking. A blocking read of a zero counter would never return.
    let fd = match eventfd(0, EventfdFlags::NONBLOCK) {
        Ok(fd) => {
            r.check("eventfd(0, EFD_NONBLOCK) hands back a descriptor", true);
            fd
        }
        Err(_) => {
            r.check("eventfd(0, EFD_NONBLOCK) hands back a descriptor", false);
            r.finish();
        }
    };

    let mut eight = [0u8; 8];
    r.check(
        "a read of an empty counter answers EAGAIN, not a park",
        read(&fd, &mut eight) == Err(Errno::AGAIN),
    );

    // A write of 8 bytes carrying 5, then a read that takes the counter whole.
    let five = 5u64.to_ne_bytes();
    r.check(
        "a write of 8 bytes carrying 5 takes 8",
        write(&fd, &five) == Ok(8),
    );
    r.check("the read takes 8 bytes", read(&fd, &mut eight) == Ok(8));
    r.check(
        "the value read back is 5, native-endian",
        u64::from_ne_bytes(eight) == 5,
    );
    r.check(
        "the counter was taken whole, so the next read is EAGAIN",
        read(&fd, &mut eight) == Err(Errno::AGAIN),
    );

    // Taken whole: two writes add, one read takes the sum.
    let three = 3u64.to_ne_bytes();
    let four = 4u64.to_ne_bytes();
    r.check("a write of 3 takes 8", write(&fd, &three) == Ok(8));
    r.check("a write of 4 takes 8", write(&fd, &four) == Ok(8));
    r.check("the read takes 8 bytes", read(&fd, &mut eight) == Ok(8));
    r.check(
        "the counter is the sum, 7, not 3 or 4",
        u64::from_ne_bytes(eight) == 7,
    );

    // The read and the write are 8 bytes wide and no less.
    let mut four_bytes = [0u8; 4];
    r.check(
        "a 4-byte read answers EINVAL",
        read(&fd, &mut four_bytes) == Err(Errno::INVAL),
    );
    r.check(
        "a 4-byte write answers EINVAL",
        write(&fd, &four_bytes) == Err(Errno::INVAL),
    );

    // The initial value is the counter's starting point.
    match eventfd(7, EventfdFlags::NONBLOCK) {
        Ok(start) => {
            r.check("eventfd(7, EFD_NONBLOCK) hands back a descriptor", true);
            let mut eight = [0u8; 8];
            r.check("the read takes 8 bytes", read(&start, &mut eight) == Ok(8));
            r.check("the first read is 7", u64::from_ne_bytes(eight) == 7);
        }
        Err(_) => {
            r.check("eventfd(7, EFD_NONBLOCK) hands back a descriptor", false);
        }
    }

    // The semaphore mode mpos deliberately does not offer. On real Linux the
    // flag is served (measured here: it creates fine), so under a host run
    // Linux's own success is accepted and named; the payload sweep pins the
    // EOPNOTSUPP refusal.
    let sem = eventfd(0, EventfdFlags::SEMAPHORE);
    let sem_answer_ok = match sem {
        Err(Errno::NOTSUP) => true,
        Ok(_) => mpos_user_tests::host_run(),
        Err(_) => false,
    };
    r.check(
        "EFD_SEMAPHORE answers EOPNOTSUPP (whole-count take only)",
        sem_answer_ok,
    );
    if let Ok(fd) = sem {
        drop(fd);
    }

    // The flag word is validated, not ignored: a bit outside the three
    // `eventfd2` knows is `EINVAL`, refused before the descriptor is made.
    // Raw because rustix's vocabulary cannot name 0x40.
    let bad = unsafe { libc::syscall(libc::SYS_eventfd2, 0u32, 0x40u64) as isize };
    r.check(
        "an unknown eventfd flag bit is EINVAL",
        refused(bad, libc::EINVAL),
    );

    // EFD_CLOEXEC actually sets close-on-exec - read it back, the way
    // SFD_CLOEXEC is pinned. Silently dropping it would leak the descriptor
    // across exec.
    match eventfd(0, EventfdFlags::CLOEXEC) {
        Ok(cloexec) => {
            r.check("eventfd(0, EFD_CLOEXEC) hands back a descriptor", true);
            let fl = fcntl_getfd(&cloexec);
            r.check(
                "EFD_CLOEXEC is reflected in the descriptor's close-on-exec flag",
                matches!(fl, Ok(f) if f.contains(FdFlags::CLOEXEC)),
            );
        }
        Err(_) => {
            r.check("eventfd(0, EFD_CLOEXEC) hands back a descriptor", false);
        }
    }

    drop(fd);
    r.finish()
}
