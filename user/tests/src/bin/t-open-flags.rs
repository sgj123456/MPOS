//! t-open-flags - the *numbers* of `open(2)`'s flags are the machine's.
//!
//! Three of the flags userspace spells have different bit values on the two
//! machines this kernel runs on, because arm64's `asm/fcntl.h` overrides them
//! before including the generic header:
//!
//! | flag | x86_64 | arm64 |
//! |---|---|---|
//! | `O_DIRECTORY` | `0x10000` | `0x4000` |
//! | `O_NOFOLLOW`  | `0x20000` | `0x8000` |
//! | `O_LARGEFILE` | `0` | `0x20000` |
//!
//! A kernel that carries one table misreads the other machine's flags, and the
//! overlap is not hypothetical: on arm64 `O_LARGEFILE` *is* x86_64's
//! `O_NOFOLLOW` bit, and musl's `open()` adds `O_LARGEFILE` to every open. So a
//! kernel with the x86_64 table answers `ELOOP` to any `open()` of a symbolic
//! link on aarch64 - which is how this was found, with `openrc` failing to start
//! its syslog service because the dynamic loader could not open the versioned
//! symlink `libcap.so.2 -> libcap.so.2.78` ("Symbolic link loop").
//!
//! The test is written against `libc`'s constants rather than numbers written
//! down here, so that what is compared is *this machine's* `libc` and this
//! kernel - the two things that have to agree. The expectations are spelled per
//! architecture so a wrong value is a failing line and not a tautology.
//!
//! It runs on **both** machines: on x86_64 it is a regression pin for a table
//! that was already right, and on aarch64 it is the pin for the one that was
//! not.

use std::ffi::CString;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// The scratch tree this test owns.
const DIR: &str = "/tmp/t-open-flags";

fn cstr(path: &str) -> CString {
    CString::new(path.as_bytes()).expect("the path has no interior NUL")
}

fn openat_raw(path: &str, flags: libc::c_int, mode: libc::mode_t) -> (libc::c_int, i32) {
    // SAFETY: `path` is a valid NUL-terminated C string this call owns for the
    // duration, and the flags are `libc`'s own. A failure answers a negative fd
    // and `errno` is read immediately, before anything else can set it.
    let fd = unsafe { libc::open(cstr(path).as_ptr(), flags, mode) };
    if fd < 0 {
        (
            fd,
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        )
    } else {
        (fd, 0)
    }
}

fn close(fd: libc::c_int) {
    if fd >= 0 {
        // SAFETY: the descriptor came from `open` above and is closed once.
        unsafe { libc::close(fd) };
    }
}

fn main() {
    println!("t-open-flags");
    let mut r = Report { failed: false };

    let _ = std::fs::remove_dir_all(DIR);
    if let Err(err) = std::fs::create_dir(DIR) {
        r.check(
            &format!("the scratch directory can be made (got {err})"),
            false,
        );
        std::process::exit(1);
    }
    let real = format!("{DIR}/real.txt");
    let link = format!("{DIR}/link");
    if let Err(err) = std::fs::write(&real, b"opened-through-a-link\n") {
        r.check(&format!("the target file can be made (got {err})"), false);
        std::process::exit(1);
    }
    // A *relative* target, which is the shape a distribution's versioned
    // libraries have (`libfoo.so.2 -> libfoo.so.2.78`).
    if let Err(err) = std::os::unix::fs::symlink("real.txt", &link) {
        r.check(&format!("the symlink can be made (got {err})"), false);
        std::process::exit(1);
    }

    // The table itself, named in the log so a failure says which bit moved.
    println!(
        "     O_DIRECTORY={:#x} O_NOFOLLOW={:#x} O_LARGEFILE={:#x} O_TMPFILE={:#x}",
        libc::O_DIRECTORY,
        libc::O_NOFOLLOW,
        libc::O_LARGEFILE,
        libc::O_TMPFILE
    );
    #[cfg(target_arch = "aarch64")]
    let (want_directory, want_nofollow) = (0x4000usize, 0x8000usize);
    #[cfg(not(target_arch = "aarch64"))]
    let (want_directory, want_nofollow) = (0x1_0000usize, 0x2_0000usize);
    r.check(
        "O_DIRECTORY is this machine's bit",
        libc::O_DIRECTORY as usize == want_directory,
    );
    r.check(
        "O_NOFOLLOW is this machine's bit",
        libc::O_NOFOLLOW as usize == want_nofollow,
    );

    // The measured failure, in its original clothing: `libc`'s own `open` is
    // musl's wrapper, which adds `O_LARGEFILE` to the flags it passes on. A
    // kernel that reads arm64's `O_LARGEFILE` as `O_NOFOLLOW` answers `ELOOP`
    // here, and the file is not a loop under any reading.
    let (fd, err) = openat_raw(&link, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    r.check(
        &format!("open(symlink, O_RDONLY|O_CLOEXEC) follows the link (got errno {err})"),
        fd >= 0,
    );
    close(fd);
    // And the same through `std`, because that is the surface a program sees.
    match std::fs::read_to_string(&link) {
        Ok(text) => r.check(
            "std::fs::read_to_string follows the link too",
            text == "opened-through-a-link\n",
        ),
        Err(err) => r.check(
            &format!("std::fs::read_to_string follows the link too (got {err})"),
            false,
        ),
    }

    // `O_NOFOLLOW` really means what it says: the *same* open refused.
    let (fd, err) = openat_raw(&link, libc::O_RDONLY | libc::O_NOFOLLOW, 0);
    r.check(
        &format!("open(symlink, O_NOFOLLOW) is ELOOP (got errno {err})"),
        fd < 0 && err == libc::ELOOP,
    );
    close(fd);

    // `O_DIRECTORY` is a demand, not a hint: a file is ENOTDIR, a directory
    // opens. Before the flags were the machine's, aarch64 read the bit as
    // nothing at all and handed back a descriptor for a regular file.
    let (fd, err) = openat_raw(&real, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    r.check(
        &format!("open(file, O_DIRECTORY) is ENOTDIR (got errno {err})"),
        fd < 0 && err == libc::ENOTDIR,
    );
    close(fd);
    let (fd, err) = openat_raw(DIR, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    r.check(
        &format!("open(dir, O_DIRECTORY) succeeds (got errno {err})"),
        fd >= 0,
    );
    close(fd);

    // `O_TMPFILE` is `O_DIRECTORY` with the unnamed-inode bit over it, so it is
    // the flag whose *composite* is machine-specific, and the kernel refuses the
    // bit without the flag with `EINVAL` - which is the reading that moved. What
    // this pins is the decoding: the pair must be recognised. Whether the
    // *filesystem* can make an unnamed inode is a different question, and one
    // this test does not pretend to answer - the trait's default refuses with
    // `EOPNOTSUPP` (measured: the answer on both machines here, from `RamFs`),
    // which is the honest answer a caller with a named-temporary-and-rename
    // fallback listens for.
    let (fd, err) = openat_raw(DIR, libc::O_RDWR | libc::O_TMPFILE, 0o600);
    r.check(
        &format!("open(dir, O_TMPFILE) reads the pair, not EINVAL (got errno {err})"),
        fd >= 0 || err != libc::EINVAL,
    );
    close(fd);
    // And the half on its own is the `EINVAL` Linux promises for it, which is
    // what says the kernel knows which bit is which.
    let tmpfile_bit = libc::O_TMPFILE & !libc::O_DIRECTORY;
    let (fd, err) = openat_raw(DIR, libc::O_RDWR | tmpfile_bit, 0o600);
    r.check(
        &format!("open(dir, __O_TMPFILE without O_DIRECTORY) is EINVAL (got errno {err})"),
        fd < 0 && err == libc::EINVAL,
    );
    close(fd);

    let _ = std::fs::remove_dir_all(DIR);
    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
