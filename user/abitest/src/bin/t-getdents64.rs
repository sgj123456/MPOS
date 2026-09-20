//! t-getdents64 - one test for getdents64, the raw directory stream ls and
//! every glob walk record by record.
//!
//! Pinned here: the loop terminates - after the last record a further call
//! answers 0, which is what the descriptor's entry cursor owes every
//! readdir loop; the records parse in the Linux shape (d_reclen strides
//! the buffer, d_type labels each name); "sh" and "cat" turn up in /bin as
//! the entry kinds the root really has (DT_LNK for the busybox links);
//! seeking back to 0 restarts the stream; /etc lists at least one
//! entry; the root lists a DT_DIR entry; and a buffer too small for the
//! first record answers EINVAL.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static BIN: [u8; 5] = *b"/bin\0";
static ETC: [u8; 5] = *b"/etc\0";
static ROOT: [u8; 2] = *b"/\0";
static mut DBUF: [u8; 4096] = [0; 4096];

/// One full pass over the directory `fd` names: counts records, remembers
/// the d_type of any name in `want1`/`want2`, and says whether the stream
/// ended with the terminating 0.
fn scan(fd: i32, want1: &[u8], want2: &[u8], types: &mut [u8; 2]) -> (usize, bool) {
    let mut count = 0usize;
    let mut eof = false;
    for _ in 0..64 {
        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut DBUF as *mut u8, 4096) };
        let n = lin::getdents64(fd, buf);
        if n < 0 {
            break;
        }
        if n == 0 {
            eof = true;
            break;
        }
        let chunk = &buf[..n as usize];
        let mut off = 0usize;
        while off < chunk.len() {
            if let Some((d, reclen)) = lin::parse_dirent64(&chunk[off..]) {
                if d.name == want1 {
                    types[0] = d.d_type;
                }
                if d.name == want2 {
                    types[1] = d.d_type;
                }
                count += 1;
                off += reclen;
            } else {
                off = chunk.len();
            }
        }
    }
    (count, eof)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-getdents64\n");
    let mut failed = false;
    // Which number the lowest free descriptor is depends on what this process
    // inherited, so measure it instead of assuming 3.
    let base = lin::free_fd();

    let fd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"the /bin directory opens on the lowest free fd", fd, base);

    let mut types = [0u8; 2];
    let (count, eof) = scan(fd as i32, b"sh", b"cat", &mut types);
    lin::check_true(
        &mut failed,
        b"the readdir loop terminates with a final 0",
        eof,
    );
    lin::check_true(&mut failed, b"/bin lists records", count >= 2);
    lin::check_eq(
        &mut failed,
        b"\"sh\" turns up as a DT_LNK entry - it is a link to busybox",
        types[0] as isize,
        lin::DT_LNK as isize,
    );
    lin::check_eq(
        &mut failed,
        b"\"cat\" turns up as a DT_LNK entry - it is a link to busybox",
        types[1] as isize,
        lin::DT_LNK as isize,
    );

    let r = lin::lseek(fd as i32, 0, lin::SEEK_SET);
    lin::check_eq(&mut failed, b"seeking the directory back to 0 succeeds", r, 0);
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut DBUF as *mut u8, 4096) };
    let n = lin::getdents64(fd as i32, buf);
    lin::check_true(
        &mut failed,
        b"the rewind restarts the stream, entries come again",
        n > 0,
    );
    let _ = lin::close(fd as i32);

    let fd = lin::openat(lin::AT_FDCWD, &ETC, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"the /etc directory opens on the lowest free fd", fd, base);
    let mut types = [0u8; 2];
    let (count, eof) = scan(fd as i32, b"motd", b"", &mut types);
    lin::check_true(&mut failed, b"the /etc pass terminates", eof);
    lin::check_true(&mut failed, b"/etc lists at least one entry", count >= 1);
    let _ = lin::close(fd as i32);

    let fd = lin::openat(lin::AT_FDCWD, &ROOT, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"the root directory opens on the lowest free fd", fd, base);
    let mut types = [0u8; 2];
    let (_count, eof) = scan(fd as i32, b"etc", b"bin", &mut types);
    lin::check_true(&mut failed, b"the root pass terminates", eof);
    lin::check_eq(&mut failed, b"\"etc\" turns up at the root as a DT_DIR entry", types[0] as isize, lin::DT_DIR as isize);
    lin::check_eq(&mut failed, b"\"bin\" turns up at the root as a DT_DIR entry", types[1] as isize, lin::DT_DIR as isize);
    let _ = lin::close(fd as i32);

    let fd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"/bin opens again on the lowest free fd", fd, base);
    let tiny = unsafe { core::slice::from_raw_parts_mut(&raw mut DBUF as *mut u8, 8) };
    let n = lin::getdents64(fd as i32, tiny);
    lin::check_eq(
        &mut failed,
        b"a buffer too small for one record answers EINVAL",
        n,
        lin::EINVAL,
    );
    let _ = lin::close(fd as i32);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-getdents64: panicked\n")
}
