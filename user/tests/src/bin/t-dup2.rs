//! t-dup2 - one test for dup2, the syscall every redirection is built from:
//! `dup2(1, 5)` and friends in the shell's plumbing.
//!
//! Pinned here: the copy lands exactly on `newfd` (returned, even one far above
//! the current table), `oldfd == newfd` on an open descriptor is the POSIX **no-op
//! that returns `newfd`**, writes through the duplicate reach the same console and
//! report every byte, and an `oldfd` that is not open answers `EBADF`.
//!
//! # Why this program calls `libc` and not `rustix`
//!
//! `rustix::io::dup2` is not a binding of `dup2` on every machine: its own source
//! substitutes `dup3(fd, new, DupFlags::empty())` under
//! `#[cfg(any(target_arch = "aarch64", ...))]`, and its comment explains why that
//! is right *for its contract* - it takes `&mut OwnedFd` for `new`, so `oldfd ==
//! newfd` cannot be expressed and the one case where `dup2` and `dup3` differ is
//! unreachable through it.
//!
//! That one case is the whole middle of this test. On aarch64 the wrapper would
//! issue `dup3(fd, fd, 0)`, which Linux answers `EINVAL`, while the ABI promises
//! the descriptor back - so a migration written against the wrapper would pin the
//! wrong call on one machine and pass on the other. `libc::dup2` is a maintained
//! per-machine binding of the syscall this test is about, which is what the
//! crate's rule asks for; it is not a second interface for a call `rustix`
//! already names (`user/tests/Cargo.toml`).

//! # What this pins on each machine
//!
//! `dup2` is one of the calls arm64 dropped: `nr_aarch64.rs` records
//! `DUP2 = 64006; // no arm64 number; unreachable on aarch64`, and musl
//! implements `dup2` over `dup3(old, new, 0)`, emulating the `old == new` no-op
//! itself. So on the first machine this program reaches the `dup2` syscall and
//! pins *it*; on the second it pins the answer a program gets from `dup2(3)`,
//! which is the ABI every caller actually speaks - and the per-claim output is
//! the same on both, which is the point of migrating rather than duplicating.
//!
//! It still uses `libc::dup2` rather than `rustix::io::dup2` on both machines:
//! the wrapper hands back `&mut OwnedFd` for `new`, which cannot express
//! `oldfd == newfd`, so it substitutes `dup3` *everywhere* and the case this
//! test is about would go untested even on the machine that has the syscall.

use mpos_user_tests::Report;

/// The file this test owns: a real descriptor at the lowest free number, which is
/// what makes the `oldfd == newfd` claim about an *open* descriptor.
const FILE: &str = "/tmp/t-dup2_file";
const CONTENT: &[u8] = b"mpos t-dup2 fixture\n";
const LINE: &[u8] = b"via fd40\n";

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn main() {
    println!("t-dup2");
    let mut r = Report::new();

    // Which number the lowest free descriptor is depends on what this process
    // inherited, so measure it rather than assuming 3.
    let base = unsafe { libc::dup(1) };
    r.check("dup(stdout) hands out a free fd", base >= 3);
    unsafe { libc::close(base) };

    if std::fs::write(FILE, CONTENT).is_err() {
        r.check("the file opens on the lowest free fd", false);
        r.finish()
    }

    let cpath = std::ffi::CString::new(FILE).expect("the path has no interior NUL");
    let a = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    r.check("the file opens on the lowest free fd", a == base);

    let dup_to_5 = unsafe { libc::dup2(1, 5) };
    r.check("dup2(stdout, 5) returns 5", dup_to_5 == 5);

    let same = unsafe { libc::dup2(a, a) };
    r.check(
        "dup2(fd, fd) on an open fd is the no-op that returns it",
        same == a,
    );

    let high = unsafe { libc::dup2(1, 40) };
    r.check("dup2 onto a high number grows the table to 40", high == 40);
    let n = unsafe { libc::write(40, LINE.as_ptr().cast(), LINE.len()) };
    r.check(
        "the duplicate writes to the console, every byte",
        n == LINE.len() as isize,
    );
    r.check("closing fd 40 succeeds", unsafe { libc::close(40) } == 0);

    unsafe { libc::close(a) };
    let gone = unsafe { libc::dup2(base, base + 3) };
    r.check(
        "an oldfd that is not open answers EBADF",
        gone == -1 && last_errno() == libc::EBADF,
    );

    unsafe { libc::close(5) };
    let _ = std::fs::remove_file(FILE);
    r.finish()
}
