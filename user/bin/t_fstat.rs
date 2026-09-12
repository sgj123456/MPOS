//! t_fstat - one test for fstat, which the strace calls on every file the
//! loader opens (`fstat(3, {st_mode=S_IFREG|0644, st_size=158831, ...})`).
//!
//! Pinned here: the Linux argument shape - fd then buffer, no length, no
//! path - the 144-byte x86_64 struct stat layout, and consistency: st_size
//! must equal what reading the file actually yields, and st_mode must say
//! regular file. The kernel currently reads fstat's (fd, buf) as if it
//! were a path pair; this test marks that mismatch.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static mut BUF: [u8; 4096] = [0; 4096];
static mut ST: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_fstat\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file to stat opens", fd >= 0);

    if fd >= 0 {
        // Read the whole file first: st_size has to agree with reality.
        let mut total = 0usize;
        for _ in 0..8 {
            let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4096) };
            let n = lin::read(fd as i32, buf);
            if n <= 0 {
                break;
            }
            total += n as usize;
        }

        let st = &raw mut ST as *mut lin::Stat;
        let r = lin::fstat(fd as i32, st);
        lin::check_eq(&mut failed, b"fstat(fd) succeeds", r, 0);
        unsafe {
            lin::check_true(
                &mut failed,
                b"st_size matches the bytes read",
                (*st).st_size == total as i64,
            );
            lin::check_true(
                &mut failed,
                b"st_mode says regular file",
                (*st).st_mode & lin::S_IFMT == lin::S_IFREG,
            );
        }
        let _ = lin::close(fd as i32);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_fstat: panicked\n")
}
