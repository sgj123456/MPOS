//! t-fill-ramfs - filling a memory filesystem is an error, not a reboot.
//!
//! The kernel heap is what a ramfs write takes its bytes from, and until the
//! mount carried a budget a process could ask for as much of it as it liked:
//! writing hundreds of megabytes into a memory-backed directory reached an
//! allocator with nothing left, and the answer to *that* was a panic - a `dd`
//! becoming a reboot, which is not a trade any program expects to be offered.
//!
//! So what is pinned here is the shape of the refusal: write until the
//! filesystem says no, and require that it *says* something. `ENOSPC` is the
//! answer Linux's tmpfs gives, the machine has to still be running to report
//! it, and the file has to be left consistent at whatever size it stopped at.
//! Reaching the next line at all is most of the proof - a kernel that panicked
//! is not one that answered.
//!
//! It stops at the first refusal rather than pushing further, so a correct
//! kernel costs a few seconds and a wrong one costs a machine - which is the
//! only way to test this at all.
//!
//! **Not made on a host run.** Filling a filesystem is only safe to ask of a
//! filesystem whose budget is known to be small and whose full state is
//! disposable: this program's whole act is to consume everything the mount will
//! give it, and the host's `/tmp` is not a mount this run is allowed to drain -
//! it is the sandbox's own storage, and on a tmpfs it is the sandbox's memory.
//! The claim is about a budget the guest's ramfs was given, and on the host
//! there is nothing to measure but someone else's disk.

use mpos_user_tests::{Report, host_run};
use rustix::fs::SeekFrom;
use rustix::fs::{CWD, Mode, OFlags, fstat, ftruncate, openat, seek, unlinkat};
use rustix::io::{pread, write};

static PATH: &str = "/tmp/t-fill-ramfs.bin";
/// One megabyte at a time, so a refusal arrives in a handful of seconds
/// whether the budget is tens of megabytes or hundreds.
const CHUNK: usize = 1024 * 1024;
/// Refuse to believe a mount that stops before this: the test's own budget, not
/// the machine's, and the reason a `size=` far too small is caught rather than
/// praised.
const AT_LEAST: usize = 8;

fn main() {
    println!("t-fill-ramfs");
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: filling a filesystem consumes everything the mount will give; this host's /tmp is the sandbox's own storage and not a budget this run may drain, so the refusal is named and not made here"
        );
        r.finish();
    }

    let _ = unlinkat(CWD, PATH, rustix::fs::AtFlags::empty());
    // Opened read-write, not write-only: the test reads the head back with
    // `pread` further down, and on Linux a read on an `O_WRONLY` descriptor is
    // `EBADF` - a `write`-only fd cannot be read no matter what was written.
    let Ok(fd) = openat(
        CWD,
        PATH,
        OFlags::RDWR | OFlags::CREATE,
        Mode::from_bits_retain(0o644),
    ) else {
        r.check("the file to fill is created", false);
        r.finish();
    };
    r.check("the file to fill is created", true);

    let buf = [0u8; CHUNK];
    let mut written = 0usize;
    let mut refusal: Option<rustix::io::Errno> = None;
    // A short write is a stop too, but it is not the answer this claim is
    // about - it would be a filesystem that accepted part of the chunk and
    // then declined the rest, which tmpfs does not do. Kept apart from the
    // errno so neither is reported as the other.
    let mut short_write: Option<usize> = None;
    // One gigabyte is far past any limit worth having; the loop is expected to
    // end on the refusal, and the bound is only so a wrong answer cannot spin
    // here forever.
    for _ in 0..1024 {
        match write(&fd, &buf) {
            Ok(CHUNK) => written += CHUNK,
            Ok(n) => {
                short_write = Some(n);
                break;
            }
            Err(e) => {
                refusal = Some(e);
                break;
            }
        }
    }

    let Some(refused) = refusal else {
        r.check(
            "the write was refused rather than the kernel dying - reaching this line is most of the proof",
            false,
        );
        r.finish();
    };
    r.check(
        "the write was refused rather than the kernel dying - reaching this line is most of the proof",
        true,
    );
    r.check(
        "it was refused outright, not accepted in part",
        short_write.is_none(),
    );
    r.check(
        "and refused with ENOSPC, the answer tmpfs gives",
        refused == rustix::io::Errno::NOSPC,
    );
    r.check(
        "a mount that stops after a handful of megabytes is not being tested",
        written / CHUNK >= AT_LEAST,
    );
    println!(
        "note: refused after {written} bytes ({} MiB)",
        written / CHUNK
    );

    // The bytes written before the refusal are still there, and readable: a
    // failed extend must not have truncated or corrupted what it refused to grow.
    let Ok(st) = fstat(&fd) else {
        r.check("fstat still reports the size that was reached", false);
        r.finish();
    };
    r.check("fstat still reports the size that was reached", true);
    r.check(
        "the file is the length it was when the write refused",
        st.st_size as usize == written,
    );
    let mut head = [0u8; 8];
    r.check("and it still reads back", pread(&fd, &mut head, 0) == Ok(8));

    // Giving the room back has to work too, or a limit is a one-way ratchet.
    r.check("truncating to zero", ftruncate(&fd, 0).is_ok());
    // The seek is not a detail: truncating leaves the descriptor's offset where
    // it was, so writing there would ask for a ~512 MiB hole and be refused all
    // the way correctly. Back at the start, the space really is free again.
    r.check(
        "seeking back to the start of the truncated file",
        seek(&fd, SeekFrom::Start(0)) == Ok(0),
    );
    r.check(
        "and a full-size write fits again",
        write(&fd, &buf) == Ok(CHUNK),
    );

    drop(fd);
    let _ = unlinkat(CWD, PATH, rustix::fs::AtFlags::empty());
    r.finish()
}
