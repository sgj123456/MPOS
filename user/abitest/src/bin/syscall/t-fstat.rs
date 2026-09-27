//! t-fstat - one test for fstat, which the strace calls on every file the
//! loader opens (`fstat(3, {st_mode=S_IFREG|0644, st_size=158831, ...})`).
//!
//! Pinned here: the Linux argument shape - fd then buffer, no length, no
//! path - the 144-byte x86_64 struct stat layout, and consistency: st_size
//! must equal what reading the file actually yields, st_mode must say
//! regular file, directories report nlink 2, two stats of one fd agree,
//! a bad descriptor answers EBADF, and an unwritable buffer EFAULT.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static BIN: [u8; 5] = *b"/bin\0";
static mut BUF: [u8; 4096] = [0; 4096];
static mut ST: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fstat\n");
    let mut failed = false;
    let st = &raw mut ST as *mut lin::Stat;

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

        // A second stat of the same descriptor must agree with the first.
        let (size1, ino1) = unsafe { ((*st).st_size, (*st).st_ino) };
        let r = lin::fstat(fd as i32, st);
        lin::check_eq(&mut failed, b"the second fstat succeeds", r, 0);
        unsafe {
            lin::check_true(
                &mut failed,
                b"st_size is stable between two stats",
                (*st).st_size == size1,
            );
            lin::check_true(
                &mut failed,
                b"st_ino is stable between two stats",
                (*st).st_ino == ino1,
            );
        }

        // A buffer the kernel cannot write answers EFAULT, not corruption.
        let r = lin::fstat(fd as i32, core::ptr::null_mut());
        lin::check_eq(
            &mut failed,
            b"fstat with a null buffer answers EFAULT",
            r,
            lin::EFAULT,
        );
        let _ = lin::close(fd as i32);
    }

    // A descriptor that is not open answers EBADF.
    let r = lin::fstat(77, st);
    lin::check_eq(
        &mut failed,
        b"fstat on a bad fd answers EBADF",
        r,
        lin::EBADF,
    );

    // A directory: S_IFDIR in st_mode, nlink 2 like the man page's example.
    let binfd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/bin opens", binfd >= 0);
    if binfd >= 0 {
        let r = lin::fstat(binfd as i32, st);
        lin::check_eq(&mut failed, b"fstat(/bin) succeeds", r, 0);
        unsafe {
            lin::check_true(
                &mut failed,
                b"st_mode says directory",
                (*st).st_mode & lin::S_IFMT == lin::S_IFDIR,
            );
            lin::check_eq(
                &mut failed,
                b"a directory reports st_nlink 2",
                (*st).st_nlink as isize,
                2,
            );
        }
        let _ = lin::close(binfd as i32);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fstat: panicked\n")
}
