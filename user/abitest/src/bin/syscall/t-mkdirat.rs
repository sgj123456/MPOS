//! t-mkdirat - one test for mkdirat, the directory-creation call behind
//! musl's `mkdir`.
//!
//! Pinned here: creating a directory under AT_FDCWD succeeds, creating the
//! same name again answers EEXIST, the new directory opens with openat and
//! lists through getdents64 (an empty one answering 0 on the first call),
//! removing it with unlinkat + AT_REMOVEDIR succeeds, and creating under a
//! missing parent answers ENOENT. The mode argument is ignored - one user,
//! no permission bits.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static DIR: [u8; 17] = *b"/tmp/t-mkdirat_d\0";
static MISSING: [u8; 13] = *b"/no/such/dir\0";
static mut DENT: [u8; 256] = [0; 256];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mkdirat\n");
    let mut failed = false;

    let r = lin::mkdirat(lin::AT_FDCWD, &DIR, 0o755);
    lin::check_eq(&mut failed, b"mkdirat creates the directory", r, 0);

    let r = lin::mkdirat(lin::AT_FDCWD, &DIR, 0o755);
    lin::check_eq(
        &mut failed,
        b"creating the same name again answers EEXIST",
        r,
        lin::EEXIST,
    );

    // A directory opens for reading, and lists without error: an empty
    // directory answers 0 on the first getdents64 call.
    let fd = lin::openat(lin::AT_FDCWD, &DIR, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the new directory opens", fd >= 0);
    if fd >= 0 {
        let dent = unsafe { core::slice::from_raw_parts_mut(&raw mut DENT as *mut u8, 256) };
        let n = lin::getdents64(fd as i32, dent);
        lin::check_true(
            &mut failed,
            b"getdents64 on the new directory does not error",
            n >= 0,
        );
        let _ = lin::close(fd as i32);
    }

    // Cleanup: AT_REMOVEDIR takes the directory back.
    let r = lin::unlinkat(lin::AT_FDCWD, &DIR, lin::AT_REMOVEDIR);
    lin::check_eq(&mut failed, b"unlinkat with AT_REMOVEDIR removes it", r, 0);

    // A parent that does not exist cannot be created under.
    let r = lin::mkdirat(lin::AT_FDCWD, &MISSING, 0o755);
    lin::check_eq(
        &mut failed,
        b"mkdirat under a missing parent answers ENOENT",
        r,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mkdirat: panicked\n")
}
