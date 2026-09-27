//! t-statx - one test for statx, which uutils uses for its locale and
//! self-locating probes (`statx(AT_FDCWD, path, AT_STATX_SYNC_AS_STAT,
//! STATX_ALL, buf)` in the strace).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! flags, request mask, buffer - the statx layout, and consistency: the
//! mask must say TYPE and SIZE were filled, stx_size must equal what
//! reading the file yields, stx_mode must say regular file, stx_ino must
//! agree with newfstatat, an empty path stats the descriptor only under
//! AT_EMPTY_PATH, a null buffer answers EFAULT, and a directory reports
//! S_IFDIR. Timestamp fields are deliberately not pinned.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static BIN: [u8; 5] = *b"/bin\0";
static EMPTY: [u8; 1] = *b"\0";
static mut BUF: [u8; 4096] = [0; 4096];
static mut STX: core::mem::MaybeUninit<lin::Statx> = core::mem::MaybeUninit::uninit();
static mut ST: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-statx\n");
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
            (*stx).stx_mask & lin::STATX_TYPE != 0 && (*stx).stx_mask & lin::STATX_SIZE != 0,
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

    // statx and newfstatat must agree about the inode.
    let st = &raw mut ST as *mut lin::Stat;
    let r = lin::newfstatat(lin::AT_FDCWD, &MOTD, st, 0);
    lin::check_eq(&mut failed, b"newfstatat on the same file succeeds", r, 0);
    unsafe {
        lin::check_true(
            &mut failed,
            b"stx_ino equals st_ino from newfstatat",
            (*stx).stx_ino == (*st).st_ino,
        );
    }

    // A buffer the kernel cannot write answers EFAULT.
    let r = lin::statx(
        lin::AT_FDCWD,
        &MOTD,
        0,
        lin::STATX_BASIC_STATS,
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"statx with a null buffer answers EFAULT",
        r,
        lin::EFAULT,
    );

    // An empty path names the descriptor, but only under AT_EMPTY_PATH.
    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(
        &mut failed,
        b"the file opens again for the fd form",
        fd >= 0,
    );
    if fd >= 0 {
        let r = lin::statx(
            fd as i32,
            &EMPTY,
            lin::AT_EMPTY_PATH,
            lin::STATX_BASIC_STATS,
            stx,
        );
        lin::check_eq(
            &mut failed,
            b"statx by fd with AT_EMPTY_PATH succeeds",
            r,
            0,
        );
        unsafe {
            lin::check_true(
                &mut failed,
                b"the fd form reports the size the read yielded",
                (*stx).stx_size == total as u64,
            );
        }
        let r = lin::statx(fd as i32, &EMPTY, 0, lin::STATX_BASIC_STATS, stx);
        lin::check_eq(
            &mut failed,
            b"an empty path without AT_EMPTY_PATH answers ENOENT",
            r,
            lin::ENOENT,
        );
        let _ = lin::close(fd as i32);
    }

    // A directory reports S_IFDIR.
    let r = lin::statx(lin::AT_FDCWD, &BIN, 0, lin::STATX_BASIC_STATS, stx);
    lin::check_eq(&mut failed, b"statx(/bin) succeeds", r, 0);
    unsafe {
        lin::check_true(
            &mut failed,
            b"stx_mode says directory",
            (*stx).stx_mode as u32 & lin::S_IFMT == lin::S_IFDIR,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-statx: panicked\n")
}
