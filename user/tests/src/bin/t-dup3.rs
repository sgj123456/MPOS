//! t-dup3 - one test for dup3, dup2 plus the descriptor-flag argument.
//!
//! Pinned here: flags = 0 behaves as dup2, oldfd == newfd answers EINVAL (the
//! one way dup3 differs from dup2's no-op), a flag word with bits beyond
//! O_CLOEXEC answers EINVAL, O_CLOEXEC alone is accepted, and the copy lands
//! exactly on newfd, replacing whatever named it before - the old description is
//! closed, which is what makes redirections safe.
//!
//! # Why this program calls `libc` and not `rustix`
//!
//! `rustix::io::dup3` does bind the syscall, but its *shape* cannot express the
//! two things this test is about most directly. It takes the target as a
//! `&mut OwnedFd`, which by construction cannot be the descriptor it is
//! duplicating *from* - so `oldfd == newfd`, the case where dup3 differs from
//! dup2, is unreachable through it. And it returns `()` rather than the new
//! number, so "dup3(stdout, 5, 0) returns 5" cannot be written against it. Those
//! two are what send this program to `libc::dup3`, which takes and returns the
//! raw numbers the syscall does; the third claim - a flag word with bits beyond
//! `O_CLOEXEC` - rides along with them rather than forcing the choice.
//!
//! That third one is worth being exact about, because the obvious reason for it
//! is wrong: `DupFlags` is a bitflags type, and rustix's flag types carry
//! bitflags' unnamed flag (`const _ = !0`), so `from_bits_retain` holds *any*
//! word - `DupFlags::from_bits_retain(!0)` is a legal call. An invalid flag word
//! is therefore expressible through `rustix`, and it is the return value and the
//! aliasing target that make `libc::dup3` the right call here. A rustix *plain
//! enum* has no such escape, and that is a real reason.
//! Either way the shape is the third one `user/tests/Cargo.toml`'s `libc` rule
//! names, beside a call `rustix` does not bind and one whose wrapper is not the
//! syscall the ABI promises.
//!
//! # What this pins on each machine
//!
//! Unlike `dup2` - which arm64 dropped, so musl emulates it over `dup3` - `dup3`
//! has a real number on both machines (`nr_aarch64.rs`: `DUP3 = 24`; `nr_x86_64.rs`:
//! `DUP3 = 292`), and both reach the same handler. So this program pins the same
//! syscall's answers on both, which is the point of migrating rather than
//! duplicating.
//!
//! The file is one this test writes itself under `/tmp`, not the no-std version's
//! `/etc/motd` (which the second machine's payload does not carry, `NOTES.md`
//! §15.11): the claim is about what `dup3` does to a descriptor, not about which
//! file that descriptor happens to name.

use mpos_user_tests::Report;

/// The file this test owns: opened at the lowest free descriptor, then replaced by
/// a duplicate of the console, which is what the last three claims are about.
const FILE: &str = "/tmp/t-dup3_file";
const CONTENT: &[u8] = b"mpos t-dup3 fixture\n";
const LINE: &[u8] = b"replaced\n";

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn main() {
    println!("t-dup3");
    let mut r = Report::new();

    // Which number the lowest free descriptor is depends on what this process
    // inherited, so measure it rather than assuming 3.
    let base = unsafe { libc::dup(1) };
    r.check("dup(stdout) hands out a free fd", base >= 3);
    unsafe { libc::close(base) };

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, CONTENT).is_err() {
        r.check("the file opens on the lowest free fd", false);
        r.finish()
    }
    let cpath = std::ffi::CString::new(FILE).expect("the path has no interior NUL");

    let d = unsafe { libc::dup3(1, 5, 0) };
    r.check("dup3(stdout, 5, 0) returns 5", d == 5);
    unsafe { libc::close(5) };

    let same = unsafe { libc::dup3(1, 1, 0) };
    r.check(
        "dup3 with oldfd == newfd answers EINVAL",
        same == -1 && last_errno() == libc::EINVAL,
    );

    // A whole flag word set: every bit beyond `O_CLOEXEC` is not a flag dup3 has.
    let bad = unsafe { libc::dup3(1, 6, -1) };
    r.check(
        "flag bits beyond O_CLOEXEC answer EINVAL",
        bad == -1 && last_errno() == libc::EINVAL,
    );

    let clo = unsafe { libc::dup3(1, 6, libc::O_CLOEXEC) };
    r.check("dup3 with O_CLOEXEC succeeds", clo == 6);
    unsafe { libc::close(6) };

    let a = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    r.check("the file opens on the lowest free fd", a == base);

    let onto = unsafe { libc::dup3(1, base, 0) };
    r.check("dup3 onto the open file fd replaces it", onto == base);
    let n = unsafe { libc::write(base, LINE.as_ptr().cast(), LINE.len()) };
    r.check(
        "the replaced fd now writes to the console, every byte",
        n == LINE.len() as isize,
    );
    r.check(
        "closing the replaced fd succeeds",
        unsafe { libc::close(base) } == 0,
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
