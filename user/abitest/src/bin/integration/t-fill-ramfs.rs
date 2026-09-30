//! t-fill-ramfs - filling a memory filesystem is an error, not a reboot.
//!
//! The kernel heap is what a ramfs write takes its bytes from, and until the
//! mount carried a budget a process could ask for as much of it as it liked:
//! writing hundreds of megabytes into a memory-backed directory reached an
//! allocator with nothing left, and the answer to *that* was a panic - a `dd`
//! becoming a reboot, which is not a trade any program expects to be offered.
//!
//! So this asserts the shape of the refusal: write until the filesystem says no,
//! and require that it *says* something. `ENOSPC` is the answer Linux's tmpfs
//! gives, the machine has to still be running to report it, and the file has to
//! be left consistent at whatever size it stopped at.
//!
//! It stops at the first refusal rather than pushing further, so a correct kernel
//! costs a few seconds and a wrong one costs a machine - which is the only way to
//! test this at all.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-fill-ramfs.bin\0";
const O_CREAT: usize = 0o100;
const CHUNK: usize = 1024 * 1024;
/// Refuse to believe a mount that stops before this: the test's own budget, not
/// the machine's, and the reason a `size=` far too small is caught rather than
/// praised.
const AT_LEAST: usize = 8;

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fill-ramfs\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    // Opened read-write, not write-only: the test reads the head back with
    // `pread` further down, and on Linux a read on an `O_WRONLY` descriptor is
    // `EBADF` - a `write`-only fd cannot be read no matter what was written.
    // The access mode is now enforced (Phase 12 §12.2 B1), so this fd must ask
    // for both directions the way any program that reads back its own write must.
    let fd = lin::openat(lin::AT_FDCWD, PATH, lin::O_RDWR | O_CREAT, 0o644);
    if fd < 0 {
        lin::die(b"t-fill-ramfs: cannot create the file to fill\n");
    }

    let buf = [0u8; CHUNK];
    let mut written = 0usize;
    let mut refusal = 0isize;
    // One gigabyte is far past any limit worth having; the loop is expected to
    // end on the refusal, and the bound is only so a wrong answer cannot spin
    // here forever.
    for _ in 0..1024 {
        let r = lin::write(fd as i32, &buf);
        if r != CHUNK as isize {
            refusal = r;
            break;
        }
        written += CHUNK;
    }

    lin::check_true(
        &mut failed,
        b"the write was refused rather than the kernel dying - reaching this line is most of the proof",
        refusal < 0,
    );
    lin::check_eq(
        &mut failed,
        b"and refused with ENOSPC, the answer tmpfs gives",
        refusal,
        lin::ENOSPC,
    );
    lin::check_true(
        &mut failed,
        b"a mount that stops after a handful of megabytes is not being tested",
        written / CHUNK >= AT_LEAST,
    );

    // The bytes written before the refusal are still there, and readable: a
    // failed extend must not have truncated or corrupted what it refused to grow.
    let mut st: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::zeroed();
    lin::check_eq(
        &mut failed,
        b"fstat still reports the size that was reached",
        lin::fstat(fd as i32, &raw mut st as *mut lin::Stat),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the file is the length it was when the write refused",
        unsafe { st.assume_init_ref() }.st_size as usize == written,
    );
    let mut head = [0u8; 8];
    lin::check_eq(
        &mut failed,
        b"and it still reads back",
        lin::pread(fd as i32, &mut head, 0),
        8,
    );

    // Giving the room back has to work too, or a limit is a one-way ratchet.
    lin::check_eq(
        &mut failed,
        b"truncating to zero",
        lin::ftruncate(fd as i32, 0),
        0,
    );
    // The seek is not a detail: truncating leaves the descriptor's offset where
    // it was, so writing there would ask for a ~512 MiB hole and be refused all
    // the way correctly. Back at the start, the space really is free again.
    lin::check_eq(
        &mut failed,
        b"seeking back to the start of the truncated file",
        lin::lseek(fd as i32, 0, 0 /* SEEK_SET */),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and a full-size write fits again",
        lin::write(fd as i32, &buf),
        CHUNK as isize,
    );

    let _ = lin::close(fd as i32);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fill-ramfs: panicked\n")
}
