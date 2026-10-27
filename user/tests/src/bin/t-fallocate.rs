//! t-fallocate - the storage a range is promised.
//!
//! Pinned here, per fallocate(2): the default operation grows the file to the
//! end of the range (and the new bytes read as zeros), a bad range is EINVAL
//! before a thing is touched, an unknown mode bit is EOPNOTSUPP the way Linux
//! answers a mode word from a newer kernel, a punch without KEEP_SIZE is EINVAL
//! (and the punch itself is the unsupported interior operation this kernel
//! answers EOPNOTSUPP for), a descriptor that is not a file is refused, that
//! KEEP_SIZE inside an existing file changes nothing - the flag's promise is
//! about the writes that come, and on this kernel they cannot fail for lack of
//! room anyway - and that the call is a *write* even when it writes no byte:
//! a successful allocation stamps the modification clock whether or not the
//! length moved, and a refused one does not.
//!
//! The set-id half of that last sentence is not here: it turns on
//! `CAP_FSETID`, which the sweep holds, so the fixture can only lose its bits
//! in a run that has dropped the capability - `t-caps`' own row, one file
//! over. The non-file pair is a pipe's two ends, and the man page's
//! `ENODEV` is not what a pipe answers on either machine: the read end is
//! read-only and gets `EBADF`, and the write end is a FIFO, which is the one
//! non-file Linux answers `ESPIPE` to - both measured through `syscall(2)`
//! (`.workbuddy/probe_fsize.c`), and both this kernel's answer now.
//!
//! # Why this one is all `rustix`
//!
//! The mode word is a subject here - `0`, `KEEP_SIZE`, a punch with and without
//! it, a collapse, and a bit nothing names - and that last one is the shape the
//! `libc` rule is written to catch. It is not a reason here, and the difference
//! is worth stating because it looks like one: `FallocateFlags` is a bitflags
//! type carrying bitflags' unnamed flag (`const _ = !0`), and every bitflags
//! type generates `from_bits_retain` for exactly this purpose - carrying a word
//! from a kernel newer than the one that wrote the flag list. So
//! `FallocateFlags::from_bits_retain(0x8000)` says "a bit this list does not
//! name" in the ABI's own vocabulary. (`SeekFrom` and `Advice` in `t-lseek` and
//! `t-fadvise` are plain enums, with no such escape - a different thing
//! entirely, and the one that does force `libc`; `user/tests/Cargo.toml`'s rule
//! spells the distinction out.)
//!
//! # What this pins on each machine
//!
//! `fallocate` has a real number on both (`nr_aarch64.rs`: `FALLOCATE = 47`;
//! `nr_x86_64.rs`: `FALLOCATE = 285`), both reach `handle_fallocate`, and the
//! per-claim output is the same on both machines.
//!
//! The size is read with `rustix::fs::fstat` and not with the no-std suite's
//! `lin::Stat`: that struct is the x86_64 `struct stat`, and the second
//! machine's is a different shape (measured: `t-stat-buffer` pins x86_64's at
//! 144 bytes). The claim is about the *answer*, so it is read through a binding
//! that knows which machine it is on. The file is the test's own under `/tmp`,
//! which both runners back with a tmpfs.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{
    AtFlags, CWD, FallocateFlags, Mode, OFlags, Timespec, Timestamps, fallocate, fstat, openat,
    unlinkat, utimensat,
};
use rustix::io::{Errno, read};
use rustix::pipe::{PipeFlags, pipe_with};

/// The file this test owns. `O_TRUNC` in the open below makes the create's mode
/// and its size the test's rather than a leftover's.
const PATH: &str = "/tmp/t-fallocate";

/// The mode word's unnamed bit: nothing in `FALLOC_FL_*` sets it, which is the
/// whole claim. Written as a number only because no table names it - the type
/// it travels in is `rustix`'s, and `from_bits_retain` is how it gets there.
const NOT_A_MODE_BIT: u32 = 0x8000;

/// The size the first allocation promises, and what the file must report after
/// it - and still report after a `KEEP_SIZE` that touches the interior.
const ALLOCATED: i64 = 4096;

fn main() {
    println!("t-fallocate");
    let mut r = Report::new();

    let _ = std::fs::remove_file(PATH);
    let Ok(fd) = openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from(0o644),
    ) else {
        r.check("the file is created", false);
        r.finish()
    };
    r.check("the file is created", true);

    // The default operation: the range becomes the file's new size, and the
    // bytes inside it read as zeros.
    r.check(
        "allocating 4096 bytes succeeds",
        fallocate(&fd, FallocateFlags::empty(), 0, ALLOCATED as u64).is_ok(),
    );
    match fstat(&fd) {
        Ok(st) => {
            r.check("fstat succeeds", true);
            r.check("the file grew to the range's end", st.st_size == ALLOCATED);
        }
        Err(_) => {
            r.check("fstat succeeds", false);
            r.check("the file grew to the range's end", false);
        }
    }
    // Filled with a byte that is not a zero before the read, so "reads as
    // zeros" is a claim about what the file hands back rather than about what
    // the buffer started as.
    let mut buf = [0xFFu8; 16];
    r.check("16 bytes read back", matches!(read(&fd, &mut buf), Ok(16)));
    r.check(
        "the allocated range reads as zeros",
        buf.iter().all(|&b| b == 0),
    );

    // The range's own validation, before any storage is touched.
    r.check(
        "a zero length answers EINVAL",
        fallocate(&fd, FallocateFlags::empty(), 0, 0) == Err(Errno::INVAL),
    );
    r.check(
        "a past-the-end offset answers EINVAL",
        fallocate(&fd, FallocateFlags::empty(), 1u64 << 63, 1) == Err(Errno::INVAL),
    );

    // The mode vocabulary: unknown bits are EOPNOTSUPP, and the interior
    // operations this kernel cannot keep are answered the same way, after the
    // punch's own EINVAL when KEEP_SIZE is missing beside it.
    r.check(
        "unknown mode bits answer EOPNOTSUPP",
        fallocate(&fd, FallocateFlags::from_bits_retain(NOT_A_MODE_BIT), 0, 1)
            == Err(Errno::OPNOTSUPP),
    );
    // Linux refuses the missing KEEP_SIZE, but a filesystem that cannot punch
    // at all may answer EOPNOTSUPP first: `/tmp` is tmpfs on the host, measured
    // Linux 7.2.11 as EOPNOTSUPP. On mpos `/tmp` is the ramfs and the answer is
    // the man page's EINVAL.
    let punch = fallocate(&fd, FallocateFlags::PUNCH_HOLE, 0, 1);
    r.check(
        "a punch without KEEP_SIZE answers EINVAL",
        punch == Err(Errno::INVAL)
            || (mpos_user_tests::host_run() && punch == Err(Errno::OPNOTSUPP)),
    );
    let punched = fallocate(
        &fd,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        0,
        1,
    );
    r.check(
        "the punch is unsupported here",
        punched == Err(Errno::OPNOTSUPP) || (mpos_user_tests::host_run() && punched.is_ok()),
    );
    r.check(
        "collapsing a range is unsupported here",
        fallocate(&fd, FallocateFlags::COLLAPSE_RANGE, 0, 1) == Err(Errno::OPNOTSUPP),
    );

    // KEEP_SIZE inside an existing file: accepted, and the size is the flag's
    // whole promise - unchanged.
    r.check(
        "KEEP_SIZE inside the file succeeds",
        fallocate(&fd, FallocateFlags::KEEP_SIZE, 0, 100).is_ok(),
    );
    match fstat(&fd) {
        Ok(st) => {
            r.check("fstat still succeeds", true);
            r.check("KEEP_SIZE changed nothing", st.st_size == ALLOCATED);
        }
        Err(_) => {
            r.check("fstat still succeeds", false);
            r.check("KEEP_SIZE changed nothing", false);
        }
    }

    // ---- the call is a write, and the file's clocks say so ----
    //
    // Laying out blocks writes no byte through the caller's buffer and still
    // changes the file. Linux's `vfs_fallocate` ends every successful path in
    // `file_modified`, which stamps the modification clock - measured on a
    // current host with the clock pinned to a known second first
    // (`.workbuddy/probe_fallocate_mode.c`, Linux 7.2.9, ext4 and tmpfs
    // agreeing): the pinned `1000000000` becomes the current second on all six
    // successful shapes, *including* the `KEEP_SIZE` that allocates without
    // moving the length and the range that already sits inside the file (B, D,
    // E), while a refused call leaves it where it was (J). So the claim is not
    // "a resize stamps it" - a call that resizes nothing does too.
    const PINNED: i64 = 1_000_000_000;
    let stamp = Timestamps {
        last_access: Timespec {
            tv_sec: PINNED,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: PINNED,
            tv_nsec: 0,
        },
    };
    r.check(
        "the file's times can be pinned to a known second",
        utimensat(CWD, PATH, &stamp, AtFlags::empty()).is_ok(),
    );
    // A range already inside the file, so nothing about the length can be what
    // moves the clock.
    r.check(
        "an allocation that cannot change the length succeeds",
        fallocate(&fd, FallocateFlags::KEEP_SIZE, 0, 8).is_ok(),
    );
    match fstat(&fd) {
        Ok(st) => {
            r.check("fstat after the allocation succeeds", true);
            r.check(
                "an allocation moves mtime off the pinned second",
                st.st_mtime != PINNED,
            );
            r.check(
                "and leaves the length alone, which is the flag's promise",
                st.st_size == ALLOCATED,
            );
        }
        Err(_) => {
            r.check("fstat after the allocation succeeds", false);
            r.check("an allocation moves mtime off the pinned second", false);
            r.check(
                "and leaves the length alone, which is the flag's promise",
                false,
            );
        }
    }
    // The other half of the same measurement: a *refused* call is not a write,
    // so it must leave the clock alone. Re-pinned first, because the call above
    // is what moved it.
    r.check(
        "the times can be pinned again",
        utimensat(CWD, PATH, &stamp, AtFlags::empty()).is_ok(),
    );
    r.check(
        "a refused allocation is still EINVAL",
        fallocate(&fd, FallocateFlags::empty(), 0, 0) == Err(Errno::INVAL),
    );
    match fstat(&fd) {
        Ok(st) => {
            r.check("fstat after the refusal succeeds", true);
            r.check(
                "a refused allocation leaves mtime pinned",
                st.st_mtime == PINNED,
            );
        }
        Err(_) => {
            r.check("fstat after the refusal succeeds", false);
            r.check("a refused allocation leaves mtime pinned", false);
        }
    }

    // A descriptor that is not a file, asked about *both* ends of a pipe, and
    // the measurement corrected this pair twice over. The read end is
    // read-only, so the mode refuses it first with Linux's own `EBADF` (that
    // kernel's `!(f_mode & FMODE_WRITE)` answers `EBADF` for `fallocate` the
    // way it answers `EINVAL` for `ftruncate`). The write end is writable and
    // reaches the inode, where a FIFO is the one non-file Linux answers
    // `ESPIPE` to rather than `ENODEV` (measured through `syscall(2)` on this
    // host, `.workbuddy/probe_fsize.c`) - so the man page's `ENODEV` this test
    // used to pin for *both* ends was the man page's word rather than either
    // machine's answer.
    match pipe_with(PipeFlags::empty()) {
        Ok((read_end, write_end)) => {
            r.check("the pipe pair is made", true);
            r.check(
                "a pipe's read end answers EBADF, as Linux does",
                fallocate(&read_end, FallocateFlags::empty(), 0, 1) == Err(Errno::BADF),
            );
            r.check(
                "a pipe's write end answers ESPIPE, as Linux does",
                fallocate(&write_end, FallocateFlags::empty(), 0, 1) == Err(Errno::SPIPE),
            );
        }
        Err(_) => {
            r.check("the pipe pair is made", false);
            r.check("a pipe's read end answers EBADF, as Linux does", false);
            r.check("a pipe's write end answers ESPIPE, as Linux does", false);
        }
    }
    // The number is not part of the claim, the descriptor table's empty row is;
    // 99 is the same one `t-read`/`t-write`/`t-openat` probe.
    let closed = unsafe { BorrowedFd::borrow_raw(99) };
    r.check(
        "a closed number answers EBADF",
        fallocate(closed, FallocateFlags::empty(), 0, 1) == Err(Errno::BADF),
    );

    // A regular file opened read-only: fallocate is a write, so the descriptor
    // is refused EBADF and nothing is grown (measured on the host). The old
    // handler resized a read-only descriptor's file - a §4.2 write done on a
    // descriptor that must not write.
    match openat(CWD, PATH, OFlags::RDONLY, Mode::empty()) {
        Ok(ro) => {
            r.check("the file reopens read-only", true);
            r.check(
                "fallocate on an O_RDONLY fd answers EBADF",
                fallocate(&ro, FallocateFlags::empty(), 0, ALLOCATED as u64) == Err(Errno::BADF),
            );
        }
        Err(_) => {
            r.check("the file reopens read-only", false);
            r.check("fallocate on an O_RDONLY fd answers EBADF", false);
        }
    }

    drop(fd);
    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}
