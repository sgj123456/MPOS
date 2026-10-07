//! t-readv - one test for readv, the scatter read glibc's stdio drains
//! sockets and files with.
//!
//! Pinned here: the iovecs fill in order from the file's cursor, the return
//! is the total across all of them (bounded by what the iovecs can hold),
//! the bytes land where the caller aimed them - iov1 gets the file's first
//! bytes and iov2 the ones right after, checked against a plain re-read of
//! the same file - and a descriptor that does not exist answers EBADF.
//!
//! Pure `rustix::io::readv`: it takes the caller's own `IoSliceMut` array -
//! the shape the ABI promises - and hands back the `Result` whose count and
//! errno are the claims. The fixture moved off `/etc/motd` for batch 3's
//! reason (the second machine's payload has no motd): the file is this
//! program's own under `/tmp`.

use mpos_user_tests::Report;
use rustix::fs::seek;
use rustix::fs::{OFlags, SeekFrom};
use rustix::io::{IoSliceMut, read, readv, write};

const PATH: &str = "/tmp/t-readv.f";
const BODY: &[u8] = b"abcdefghij";

fn main() {
    println!("t-readv");
    let mut r = Report::new();

    let _ = std::fs::remove_file(PATH);
    let fd = match rustix::fs::open(
        PATH,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        rustix::fs::Mode::from_bits_retain(0o644),
    ) {
        Ok(fd) => fd,
        Err(_) => {
            r.check("the file opens on a fresh descriptor", false);
            r.finish();
        }
    };
    r.check(
        "the file takes its ten bytes",
        write(&fd, BODY) == Ok(BODY.len()),
    );
    let _ = seek(&fd, SeekFrom::Start(0));

    let mut iov1 = [0u8; 4];
    let mut iov2 = [0u8; 8];
    {
        let mut bufs = [IoSliceMut::new(&mut iov1), IoSliceMut::new(&mut iov2)];
        match readv(&fd, &mut bufs) {
            Ok(total) => {
                r.check("readv hands back bytes", total > 0);
                r.check("the total is bounded by the iovecs (4 + 8)", total <= 12);
                let n1 = total.min(4);
                let n2 = total.saturating_sub(n1).min(8);
                let mut whole = [0u8; 64];
                let _ = seek(&fd, SeekFrom::Start(0));
                let n = read(&fd, &mut whole).unwrap_or(0);
                r.check(
                    "the plain re-read covers everything readv took",
                    n >= total && n > 0,
                );
                let in_order = whole[..n1] == iov1[..n1] && whole[n1..n1 + n2] == iov2[..n2];
                r.check("the iovecs hold the file's bytes in order", in_order);
            }
            Err(_) => {
                r.check("readv hands back bytes", false);
                r.check("the total is bounded by the iovecs (4 + 8)", false);
                r.check("the plain re-read covers everything readv took", false);
                r.check("the iovecs hold the file's bytes in order", false);
            }
        }
    }

    // A descriptor that does not exist answers EBADF - raw, because
    // rustix's AsFd cannot name 99 (batch 3's rule, as in t-write).
    let mut bufs = [IoSliceMut::new(&mut iov1)];
    let ebadf = unsafe {
        libc::syscall(libc::SYS_readv, 99i32, &mut bufs as *mut _ as usize, 1usize) as isize
    };
    r.check(
        "a descriptor that does not exist answers EBADF",
        ebadf == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
    );

    drop(fd);
    let _ = rustix::fs::unlinkat(rustix::fs::CWD, PATH, rustix::fs::AtFlags::empty());
    r.finish()
}
