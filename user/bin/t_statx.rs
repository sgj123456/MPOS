//! t_statx - one test for statx, which uutils uses for its locale and
//! self-locating probes (`statx(AT_FDCWD, path, AT_STATX_SYNC_AS_STAT,
//! STATX_ALL, buf)` in the strace).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! flags, request mask, buffer - the statx layout, and consistency: the
//! mask must say TYPE and SIZE were filled, stx_size must equal what
//! reading the file yields, and stx_mode must say regular file. Timestamp
//! fields are deliberately not pinned.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static mut BUF: [u8; 4096] = [0; 4096];
static mut STX: core::mem::MaybeUninit<lin::Statx> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_statx\n");
    let mut failed = false;

    // Read the whole file first: stx_size has to agree with reality.
    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file to stat opens", fd >= 0);
    let mut total = 0usize;
    if fd >= 0 {
        for _ in 0..8 {
            let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4096) };
            let n = lin::read(fd as i32, buf);
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        let _ = lin::close(fd as i32);
    }

    let stx = &raw mut STX as *mut lin::Statx;
    let r = lin::statx(lin::AT_FDCWD, &MOTD, 0, lin::STATX_BASIC_STATS, stx);
    lin::check_eq(&mut failed, b"statx(/etc/motd) succeeds", r, 0);
    unsafe {
        lin::check_true(
            &mut failed,
            b"the mask reports type and size",
            (*stx).stx_mask & lin::STATX_TYPE != 0
                && (*stx).stx_mask & lin::STATX_SIZE != 0,
        );
        lin::check_true(
            &mut failed,
            b"stx_size matches the bytes read",
            (*stx).stx_size == total as u64,
        );
        lin::check_true(
            &mut failed,
            b"stx_mode says regular file",
            (*stx).stx_mode as u32 & lin::S_IFMT == lin::S_IFREG,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_statx: panicked\n")
}
