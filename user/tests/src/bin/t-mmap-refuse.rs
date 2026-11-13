//! A node with no pages to map is refused at the `mmap` call, with Linux's
//! number.
//!
//! The defect this pins: `mmap` of a **directory** was accepted as a private
//! file mapping. Nothing could fill the page behind it - a directory's
//! `read_at` answers `EISDIR` - so the first touch of the mapping was refused
//! by the fault path, the task died of `SIGSEGV`, and the program had already
//! been told the call succeeded. That is how `mkfontscale` died in the guest:
//! it walks the entries of whatever directory it is given and maps the ones
//! that open, and the mapped `/root/.local` killed it on the first read.
//!
//! Measured on Linux 7.2.14 (`build/scratch/mmapdir/{p,q,r}.c`): `mmap` of a
//! directory, a FIFO, a socket, a character device with no mmap operation and
//! every `procfs`/`sysfs` file answers `-1 ENODEV`; a regular file's bytes map
//! and read back. The refusals come in a measured order too: a zero length or
//! an offset that is not a whole page is `EINVAL` **before** the node is asked,
//! and a bad descriptor is `EBADF`.
//!
//! `MPOS_ON_HOST=1` runs the same claims against the host's Linux.

use mpos_user_tests::{Report, host_run};

const PAGE: usize = 4096;

/// `mmap(NULL, len, prot, flags, fd, 0)`, answering what the call said.
fn try_map(
    fd: libc::c_int,
    len: usize,
    prot: libc::c_int,
    flags: libc::c_int,
) -> Result<*mut libc::c_void, libc::c_int> {
    // SAFETY: the arguments are the constants this test names; MAP_FAILED is
    // the only answer read as an error, and errno is read immediately after.
    let answer = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
    if answer == libc::MAP_FAILED {
        // SAFETY: no thread in this program writes errno between the call and
        // this read.
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(answer)
    }
}

fn open_at(path: &str, flags: libc::c_int) -> libc::c_int {
    // SAFETY: a NUL-terminated path this test owns, and the flags it named.
    unsafe { libc::open(std::ffi::CString::new(path).unwrap().as_ptr(), flags) }
}

fn read_one(path: &str) -> Result<Vec<u8>, libc::c_int> {
    let fd = open_at(path, libc::O_RDONLY);
    if fd < 0 {
        // SAFETY: errno is the answer the failed call just left.
        return Err(unsafe { *libc::__errno_location() });
    }
    // SAFETY: a one-page mapping of a descriptor this test just opened; it is
    // unmapped before the descriptor is closed.
    let mapped = try_map(fd, PAGE, libc::PROT_READ, libc::MAP_PRIVATE);
    // SAFETY: the descriptor belongs to this test and is used once.
    unsafe { libc::close(fd) };
    match mapped {
        Ok(at) => {
            // SAFETY: the mapping is a page, and this reads its first byte.
            let byte = unsafe { (at as *const u8).read_volatile() };
            // SAFETY: the mapping is this test's own, mapped one page.
            unsafe { libc::munmap(at, PAGE) };
            Ok(vec![byte])
        }
        Err(e) => Err(e),
    }
}

fn main() {
    let mut report = Report::new();

    // The control: a file whose bytes are storage still maps, and the byte the
    // mapping reports is the file's first byte. Every refusal below is only
    // meaningful beside this one - a kernel that refused everything would pass
    // all of them.
    let fixture = "/tmp/mmap-refuse-fixture";
    // The mode is not decoration: `O_CREAT` without one leaves the new file at
    // whatever the third register held, and a mode of zero is a file this very
    // program cannot open again - measured here as `EACCES` on the control
    // claim, which is exactly what a fixture that is not a file looks like.
    // SAFETY: a NUL-terminated path this test owns, and the flags and mode it
    // names.
    let fd = unsafe {
        libc::open(
            c"/tmp/mmap-refuse-fixture".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        report.check("the fixture file could be created", false);
        report.finish();
    }
    let body = b"first byte is f";
    // SAFETY: the buffer is this program's own and the descriptor is open.
    let wrote = unsafe { libc::write(fd, body.as_ptr().cast(), body.len()) };
    // SAFETY: the descriptor belongs to this test.
    unsafe { libc::close(fd) };
    report.check("the fixture file was written", wrote == body.len() as isize);

    match read_one(fixture) {
        Ok(bytes) => report.check(
            "a regular file's bytes map, and the first byte is the file's",
            bytes.first() == Some(&b'f'),
        ),
        Err(e) => report.check(
            &format!("a regular file's bytes map, and the first byte is the file's (errno {e})"),
            false,
        ),
    }

    // A directory: no pages, and Linux refuses the call rather than the first
    // touch.
    match read_one("/tmp") {
        Err(libc::ENODEV) => report.check("a directory is refused with ENODEV", true),
        other => report.check(
            &format!("a directory is refused with ENODEV (answered {other:?})"),
            false,
        ),
    }

    // The same node asked for *shared*: still no pages, still ENODEV.
    let dir_fd = open_at("/tmp", libc::O_RDONLY);
    report.check("the directory opened", dir_fd >= 0);
    // SAFETY: the descriptor was just opened and is mapped-or-refused once.
    let shared = try_map(dir_fd, PAGE, libc::PROT_READ, libc::MAP_SHARED);
    // SAFETY: the descriptor belongs to this test.
    unsafe { libc::close(dir_fd) };
    match shared {
        Err(libc::ENODEV) => report.check("a shared mapping of a directory is ENODEV too", true),
        Ok(at) => {
            // SAFETY: the call reported a mapping; leave it unmapped.
            unsafe { libc::munmap(at, PAGE) };
            report.check("a shared mapping of a directory is ENODEV too", false);
        }
        Err(e) => report.check(
            &format!("a shared mapping of a directory is ENODEV too (errno {e})"),
            false,
        ),
    }

    // The measured order: the length and the offset are judged before the node
    // is asked what it has.
    let dir_fd = open_at("/tmp", libc::O_RDONLY);
    // SAFETY: a zero-length request is the thing under test; the pointer is
    // never dereferenced because the call refuses it.
    let zero_len = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            0,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            dir_fd,
            0,
        )
    };
    // SAFETY: errno is the failed call's own answer.
    let zero_errno = unsafe { *libc::__errno_location() };
    report.check(
        "a zero length is EINVAL before the node is asked",
        zero_len == libc::MAP_FAILED && zero_errno == libc::EINVAL,
    );
    // SAFETY: an offset that is not a whole page is the thing under test.
    let bad_offset = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            dir_fd,
            1,
        )
    };
    // SAFETY: errno is the failed call's own answer.
    let offset_errno = unsafe { *libc::__errno_location() };
    report.check(
        "an offset that is not a whole page is EINVAL too",
        bad_offset == libc::MAP_FAILED && offset_errno == libc::EINVAL,
    );
    // SAFETY: the descriptor belongs to this test.
    unsafe { libc::close(dir_fd) };

    // A descriptor that names nothing: EBADF, and the node question is never
    // reached.
    // SAFETY: it maps `-1` on purpose.
    let bad_fd = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    // SAFETY: errno is the failed call's own answer.
    let bad_fd_errno = unsafe { *libc::__errno_location() };
    report.check(
        "a descriptor that names nothing is EBADF",
        bad_fd == libc::MAP_FAILED && bad_fd_errno == libc::EBADF,
    );

    // A generated file: its bytes are produced per read, so there is nothing
    // to map - Linux's own answer for `procfs`.
    match read_one("/proc/self/stat") {
        Err(libc::ENODEV) => report.check("a procfs file is refused with ENODEV", true),
        other => report.check(
            &format!("a procfs file is refused with ENODEV (answered {other:?})"),
            false,
        ),
    }

    // A character device with no mmap operation.
    match read_one("/dev/null") {
        Err(libc::ENODEV) => {
            report.check("a device node with no mmap is refused with ENODEV", true)
        }
        other => report.check(
            &format!("a device node with no mmap is refused with ENODEV (answered {other:?})"),
            false,
        ),
    }

    // A pipe end: a channel, not storage.
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: the array is this test's, and pipe(2) fills both entries on 0.
    let piped = unsafe { libc::pipe(fds.as_mut_ptr()) };
    report.check("the pipe was made", piped == 0);
    if piped == 0 {
        match try_map(fds[0], PAGE, libc::PROT_READ, libc::MAP_PRIVATE) {
            Err(libc::ENODEV) => report.check("a pipe end is refused with ENODEV", true),
            Ok(at) => {
                // SAFETY: the call reported a mapping; leave it unmapped.
                unsafe { libc::munmap(at, PAGE) };
                report.check("a pipe end is refused with ENODEV", false);
            }
            Err(e) => report.check(
                &format!("a pipe end is refused with ENODEV (errno {e})"),
                false,
            ),
        }
        // SAFETY: both descriptors belong to this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    // The arm the fix must not have broken: a memfd owns pages, so a mapping
    // of one aliases them - the frame-backed answer, asked before the node
    // question.
    // SAFETY: `memfd_create` with a literal name and no flags.
    let mem = unsafe { libc::syscall(libc::SYS_memfd_create, c"m".as_ptr(), 0 as libc::c_uint) }
        as libc::c_int;
    if mem < 0 {
        report.check("the memfd was created", false);
    } else {
        // SAFETY: the memfd is open and this writes into it.
        let wrote = unsafe { libc::write(mem, body.as_ptr().cast(), body.len()) };
        report.check("the memfd was written", wrote == body.len() as isize);
        match try_map(mem, PAGE, libc::PROT_READ, libc::MAP_PRIVATE) {
            Ok(at) => {
                // SAFETY: the mapping is a page of a memfd this test filled.
                let byte = unsafe { (at as *const u8).read_volatile() };
                // SAFETY: the mapping is this test's own.
                unsafe { libc::munmap(at, PAGE) };
                report.check("a memfd's pages still map and alias", byte == b'f');
            }
            Err(e) => report.check(
                &format!("a memfd's pages still map and alias (errno {e})"),
                false,
            ),
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(mem) };
    }

    // SAFETY: the fixture is this test's own path.
    unsafe { libc::unlink(c"/tmp/mmap-refuse-fixture".as_ptr()) };

    if host_run() {
        println!("note: the claims above were run against the host, not mpos");
    }
    report.finish();
}
