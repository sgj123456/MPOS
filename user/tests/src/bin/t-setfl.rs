//! t-setfl - `fcntl(F_SETFL)` can set and clear `O_NOATIME`, and the bit has
//! teeth: a read through a description whose *status* carries it leaves the
//! file's access clock alone.
//!
//! Measured on a current host first (`build/scratch/setfl/{p,q,r,t,u}.c`, Linux
//! 7.2.14, `gcc -O1`):
//!
//! * a plain description's `F_GETFL` has no `O_NOATIME` (`0x8002`), and
//!   `F_SETFL(O_NOATIME)` makes it appear (`0x48002`); `F_SETFL(0)` takes it
//!   back out (`0x8002`). The same holds for a description *opened* with the
//!   flag: `F_SETFL(0)` clears it, which is why what governs a read is the
//!   effective word rather than the flags the open carried;
//! * with `atime` forced to `1000000000` by `utimensat`, a plain read moves it
//!   to the read's own moment, a read through `F_SETFL(O_NOATIME)` leaves it at
//!   `1000000000`, and clearing the bit makes the next read move it again -
//!   the flag is not merely reported, it is obeyed;
//! * `O_NOATIME|O_NONBLOCK` in one word sets both, so the mask is a mask;
//! * a caller that neither owns the file nor holds `CAP_FOWNER` is refused a
//!   *fresh* set with `EPERM` and stores none of the word, while re-affirming an
//!   already-set bit and clearing it are both served.
//!
//! # What this does not claim
//!
//! `O_ASYNC` and `O_DIRECT` are Linux's other two `SETFL_MASK` members and this
//! kernel's mask has neither: `O_ASYNC` needs `fasync`/SIGIO plumbing that does
//! not exist here (Linux itself reports it from `F_GETFL` only for an inode with
//! an `fasync` method - `F_SETFL(O_ASYNC)` on a regular file returns 0 and the
//! bit never appears), and `O_DIRECT` has no direct-I/O path here and a
//! machine-specific number. Both are named in `docs/uapi/` §5.2. The claim
//! below about `O_ASYNC` is the *narrow* one both kernels agree on: it is
//! reported when an open asked for it, and it is not a status bit here.
//!
//! # Why the stranger claim is guest-only
//!
//! It needs to stop being root (or the file's owner), and a host run under
//! `MPOS_ON_HOST=1` is uid 1000 with no way to drop further - the same shape
//! `t-atime`'s own stranger claim has, and it prints the same `note:` and skips.

use mpos_user_tests::{Report, host_run};

/// The file this program owns. Its own name, so a leftover from an interrupted
/// run is removed before the fixture is made.
const FILE: &str = "/tmp/t-setfl_file";

/// An access clock no clock in this system will ever read as "now": year 2001.
const OLD_ATIME: i64 = 1_000_000_000;

fn cpath(path: &str) -> std::ffi::CString {
    std::ffi::CString::new(path).unwrap()
}

/// The node's three clocks, or `None` if `stat` refused.
fn stat_times(path: &str) -> Option<libc::stat> {
    let c = cpath(path);
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: both pointers are this frame's, and `stat` writes only `st`.
    let rc = unsafe { libc::stat(c.as_ptr(), &mut st) };
    (rc == 0).then_some(st)
}

/// Force the access and modify clocks to `OLD_ATIME`, so a read that touches
/// atime is visible as a movement rather than as two readings of "now".
fn age_file(path: &str) -> bool {
    let c = cpath(path);
    let times = [
        libc::timespec {
            tv_sec: OLD_ATIME,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: OLD_ATIME,
            tv_nsec: 0,
        },
    ];
    // SAFETY: the path and the two-word array are this frame's.
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) == 0 }
}

/// Open `path` read-only, answering the raw errno on a refusal.
fn open_ro(path: &str, extra: libc::c_int) -> Result<libc::c_int, i32> {
    let c = cpath(path);
    // SAFETY: the path is this frame's and the flags are numbers.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | extra) };
    if fd < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(fd)
    }
}

/// `fcntl(fd, F_SETFL, arg)`, answering the raw errno on a refusal.
fn setfl(fd: libc::c_int, arg: libc::c_int) -> Result<(), i32> {
    // SAFETY: `fd` is a descriptor this frame opened; the third argument is
    // variadic and musl reads it as an `unsigned long`.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, arg as libc::c_ulong) };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(())
    }
}

/// The `F_GETFL` word, or `None` if the call was refused.
fn getfl(fd: libc::c_int) -> Option<libc::c_int> {
    // SAFETY: `fd` is a descriptor this frame opened.
    let rc = unsafe { libc::fcntl(fd, libc::F_GETFL, 0 as libc::c_ulong) };
    (rc >= 0).then_some(rc as libc::c_int)
}

/// Read one byte from the *start* through `fd`, answering whether any byte came
/// back. The seek is part of the helper because these claims read the same
/// description more than once, and a second read at the advanced cursor would
/// find EOF and move nothing - a claim about the access clock would then pass or
/// fail for a reason that is not the flag.
fn read_one(fd: libc::c_int) -> bool {
    // SAFETY: `fd` is open; seeking to the start is what the fixture wants.
    let at_start = unsafe { libc::lseek(fd, 0, libc::SEEK_SET) == 0 };
    let mut byte = 0u8;
    // SAFETY: `fd` is open and the buffer is one byte this frame owns.
    let n = unsafe { libc::read(fd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    at_start && n > 0
}

fn main() {
    println!("t-setfl");
    let mut r = Report::new();

    // A fresh fixture: one byte, so a read has something to move.
    let c = cpath(FILE);
    // SAFETY: the path and the mode are literals this frame owns.
    let made = unsafe {
        libc::unlink(c.as_ptr());
        let fd = libc::open(
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        );
        if fd >= 0 {
            libc::write(fd, b"x".as_ptr() as *const libc::c_void, 1);
            libc::close(fd);
            true
        } else {
            false
        }
    };
    if !made {
        for label in [
            "a plain description's F_GETFL has no O_NOATIME",
            "F_SETFL sets O_NOATIME",
            "F_SETFL clears it again",
            "and clears one an open had asked for",
            "the set bit leaves the access clock alone",
            "clearing it restores the touch",
            "O_NOATIME|O_NONBLOCK sets both",
            "an O_ASYNC the open asked for is reported and F_SETFL cannot clear it",
        ] {
            r.check(label, false);
        }
        r.finish();
    }

    // ---- the word: set, clear, and clear again from an open's own flag
    let fd = open_ro(FILE, 0).expect("the fixture opens");
    let base = getfl(fd);
    r.check(
        "a plain description's F_GETFL has no O_NOATIME",
        base.is_some_and(|f| f & libc::O_NOATIME == 0),
    );

    let set = setfl(fd, libc::O_NOATIME);
    let after_set = getfl(fd);
    r.check(
        "F_SETFL sets O_NOATIME",
        set.is_ok() && after_set.is_some_and(|f| f & libc::O_NOATIME != 0),
    );

    let cleared = setfl(fd, 0);
    let after_clear = getfl(fd);
    r.check(
        "F_SETFL clears it again",
        cleared.is_ok() && after_clear.is_some_and(|f| f & libc::O_NOATIME == 0),
    );
    // SAFETY: `fd` is the descriptor this frame opened.
    unsafe {
        libc::close(fd);
    }

    // The other direction, and the one the old kernel could not answer: a
    // description *opened* with O_NOATIME loses the bit to a later F_SETFL.
    let born_with_it = open_ro(FILE, libc::O_NOATIME).expect("the owner may open with O_NOATIME");
    let born_word = getfl(born_with_it);
    let cleared_born = setfl(born_with_it, 0);
    let after_born_clear = getfl(born_with_it);
    r.check(
        "and clears one an open had asked for",
        born_word.is_some_and(|f| f & libc::O_NOATIME != 0)
            && cleared_born.is_ok()
            && after_born_clear.is_some_and(|f| f & libc::O_NOATIME == 0),
    );
    // SAFETY: `born_with_it` is this frame's descriptor.
    unsafe {
        libc::close(born_with_it);
    }

    // ---- the teeth: the status bit governs the access clock
    let fd = open_ro(FILE, 0).expect("the fixture opens");
    let aged = age_file(FILE);
    let before = stat_times(FILE);
    let set = setfl(fd, libc::O_NOATIME);
    let read_ok = read_one(fd);
    let after_noatime = stat_times(FILE);
    r.check(
        "the set bit leaves the access clock alone",
        aged && set.is_ok()
            && read_ok
            && before.is_some_and(|b| b.st_atime == OLD_ATIME)
            && after_noatime.is_some_and(|a| a.st_atime == OLD_ATIME),
    );

    // Clearing it puts the teeth back: the same description now moves atime.
    let aged = age_file(FILE);
    let cleared = setfl(fd, 0);
    let read_ok = read_one(fd);
    let after_clear = stat_times(FILE);
    r.check(
        "clearing it restores the touch",
        aged && cleared.is_ok() && read_ok && after_clear.is_some_and(|a| a.st_atime != OLD_ATIME),
    );
    // SAFETY: `fd` is this frame's descriptor.
    unsafe {
        libc::close(fd);
    }

    // ---- the mask is a mask: two members in one word both land
    let fd = open_ro(FILE, 0).expect("the fixture opens");
    let set = setfl(fd, libc::O_NOATIME | libc::O_NONBLOCK);
    let word = getfl(fd);
    r.check(
        "O_NOATIME|O_NONBLOCK sets both",
        set.is_ok() && word.is_some_and(|f| f & libc::O_NOATIME != 0 && f & libc::O_NONBLOCK != 0),
    );
    // SAFETY: `fd` is this frame's descriptor.
    unsafe {
        libc::close(fd);
    }

    // ---- the bit this kernel does not carry, claimed narrowly. Linux reports
    // ---- an O_ASYNC the open asked for and *cannot clear it* through F_SETFL:
    // ---- measured (`build/scratch/setfl/q.c`), `F_GETFL=0xa002` before and
    // ---- after `F_SETFL(0)`. It is the open's own word, not a status bit - the
    // ---- same split as O_NOFOLLOW. What this kernel does not do is *set* it
    // ---- from F_SETFL, which needs fasync/SIGIO plumbing; that is §5.2's named
    // ---- residual and no claim here asks for it.
    let fd = open_ro(FILE, libc::O_ASYNC).expect("the fixture opens with O_ASYNC");
    let born = getfl(fd);
    let set = setfl(fd, 0);
    let after = getfl(fd);
    r.check(
        "an O_ASYNC the open asked for is reported and F_SETFL cannot clear it",
        born.is_some_and(|f| f & libc::O_ASYNC != 0)
            && set.is_ok()
            && after.is_some_and(|f| f & libc::O_ASYNC != 0),
    );
    // SAFETY: `fd` is this frame's descriptor.
    unsafe {
        libc::close(fd);
    }

    // ---- a stranger's fresh set is EPERM, and stores none of the word
    if host_run() {
        println!(
            "note: a host run cannot drop below uid 1000; the stranger's refusal is named and not made"
        );
    } else {
        // The parent opens the file first (it is the owner), then the child
        // drops to 65534 and asks for the flag on its own descriptor.
        let fd = open_ro(FILE, 0).expect("the fixture opens");
        // SAFETY: `fork` takes no arguments; the child only calls libc and
        // writes one number down a pipe.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            // SAFETY: all three ids go to 65534, then the child asks for a
            // promise it is not entitled to on a descriptor it opened itself.
            unsafe {
                let dropped = libc::setresgid(65534, 65534, 65534) == 0
                    && libc::setresuid(65534, 65534, 65534) == 0;
                let own = if dropped {
                    open_ro(FILE, 0).unwrap_or(-1)
                } else {
                    -1
                };
                let answer = if own >= 0 {
                    match setfl(own, libc::O_NOATIME) {
                        Err(e) => e,
                        Ok(()) => 0,
                    }
                } else {
                    -1
                };
                let stored = if own >= 0 {
                    getfl(own).unwrap_or(-1)
                } else {
                    -1
                };
                if own >= 0 {
                    libc::close(own);
                }
                // Two numbers: the errno, then whether the bit was stored.
                libc::_exit(if answer == libc::EPERM && stored & libc::O_NOATIME == 0 {
                    libc::EPERM
                } else {
                    1
                });
            }
        }
        let mut status = 0;
        // SAFETY: `child` is this frame's own child.
        unsafe {
            libc::waitpid(child, &mut status, 0);
        }
        let answer = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        // SAFETY: `fd` is this frame's descriptor.
        unsafe {
            libc::close(fd);
        }
        r.check(
            "a stranger's fresh O_NOATIME set is EPERM and stores nothing",
            answer == libc::EPERM,
        );
    }

    // SAFETY: the name is this frame's and the file is its own.
    unsafe {
        libc::unlink(c.as_ptr());
    }
    r.finish();
}
