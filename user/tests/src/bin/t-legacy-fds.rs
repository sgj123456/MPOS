//! t-legacy-fds - the **legacy** `eventfd(284)` and `inotify_init(253)`, the two
//! descriptors a libc asks for by their flagless numbers.
//!
//! Both were undeclared, so the dispatch answered `ENOSYS` - and both are served
//! by their modern siblings (`eventfd2` 290, `inotify_init1` 294), so nothing
//! visibly broke until a binary built against an older glibc asked for them.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{legacy,legacy2}.c`):
//!
//! | call | Linux |
//! |---|---|
//! | `eventfd(count)` | the counter is honoured; `F_GETFL` is `O_RDWR`, `FD_CLOEXEC` clear |
//! | `eventfd(count, ANY_WORD)` | **served for every word**, `0x7ffff` included - the second argument is not part of the call |
//! | `eventfd2(count, 0x99)` | `EINVAL` - the modern door validates |
//! | `inotify_init()` | `F_GETFL` 0 (`O_RDONLY`), `FD_CLOEXEC` clear |
//! | `inotify_init1(0x99)` | `EINVAL` |
//!
//! **The asymmetry is the point of the program.** A kernel that routed both
//! spellings through one handler would either refuse the legacy caller's ignored
//! second argument (`EINVAL` for a call Linux serves) or accept an unknown bit on
//! the modern one (a descriptor whose `F_GETFL` disagrees with what was asked
//! for). Both doors are pinned against the other's answer.
//!
//! **Both numbers are x86_64-only** - the asm-generic table aarch64 uses never
//! had the legacy spellings, so on that machine these numbers are values no
//! caller can pass and the program is skipped there rather than claiming a call
//! the machine cannot make.
//!
//! Numbers from libc's table: `rustix` binds `eventfd` as `eventfd2` and
//! `inotify_init` not at all, so the flagless spellings only exist at the raw
//! door.

use mpos_user_tests::Report;

/// The two legacy numbers, x86_64's own.
#[cfg(target_arch = "x86_64")]
const EVENTFD_LEGACY: i64 = 284;
#[cfg(target_arch = "x86_64")]
const INOTIFY_INIT_LEGACY: i64 = 253;
// **The modern numbers differ per machine**: x86_64's `eventfd2` is 290 and
// `inotify_init1` 294, while the asm-generic table aarch64 uses numbers them 19
// and 26. A first draft hardcoded the x86_64 pair, so on aarch64 the program
// called numbers those machines give other calls entirely and the two modern
// doors looked broken - a claim about this *program*, not about the kernel.
#[cfg(target_arch = "x86_64")]
const EVENTFD2: i64 = 290;
#[cfg(target_arch = "x86_64")]
const INOTIFY_INIT1: i64 = 294;
#[cfg(not(target_arch = "x86_64"))]
const EVENTFD2: i64 = 19;
#[cfg(not(target_arch = "x86_64"))]
const INOTIFY_INIT1: i64 = 26;

fn call(nr: i64, a: usize, b: usize) -> isize {
    unsafe { libc::syscall(nr, a, b) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

fn main() {
    println!("t-legacy-fds");
    let mut r = Report::new();

    // The modern doors are claimed on **both** machines, so the asymmetry has a
    // reference point on each.
    r.check(
        "eventfd2(0, 0x99) is EINVAL - the modern door validates the word",
        refused(call(EVENTFD2, 0, 0x99), libc::EINVAL),
    );
    r.check(
        "inotify_init1(0x99) is EINVAL",
        refused(call(INOTIFY_INIT1, 0x99, 0), libc::EINVAL),
    );

    // === the legacy doors, on the machine that has them =====================
    #[cfg(target_arch = "x86_64")]
    {
        // The counter is honoured, and the descriptor is a plain `O_RDWR` one.
        let fd = call(EVENTFD_LEGACY, 7, 0);
        r.check(&format!("eventfd(count) is served ({fd})"), fd >= 0);
        if fd >= 0 {
            let mut v = 0u64;
            // SAFETY: `v` is 8 bytes of this test's own stack, and the descriptor
            // is the eventfd just made.
            let n = unsafe { libc::read(fd as i32, (&mut v as *mut u64).cast(), 8) };
            r.check(
                &format!("and the initial counter is what was asked ({v}, read {n})"),
                n == 8 && v == 7,
            );
            // SAFETY: the live descriptor.
            let fl = unsafe { libc::fcntl(fd as i32, libc::F_GETFL) };
            r.check(
                &format!("and F_GETFL is O_RDWR ({fl:#x})"),
                fl & libc::O_ACCMODE == libc::O_RDWR,
            );
            // SAFETY: as above.
            let fdf = unsafe { libc::fcntl(fd as i32, libc::F_GETFD) };
            r.check(
                &format!("and FD_CLOEXEC is clear ({fdf})"),
                fdf & libc::FD_CLOEXEC == 0,
            );
            // SAFETY: as above.
            unsafe { libc::close(fd as i32) };
        }

        // **The claim that separates the two doors**: the legacy one ignores its
        // second argument entirely, so a word `eventfd2` refuses is served here
        // with no effect.
        for (label, flags) in [
            ("0", 0usize),
            ("EFD_NONBLOCK", 0x800),
            ("EFD_CLOEXEC", 0x80000),
            ("0x99 (refused by eventfd2)", 0x99),
            ("0x7ffff", 0x7ffff),
        ] {
            let fd = call(EVENTFD_LEGACY, 0, flags);
            let ok = fd >= 0 && {
                // SAFETY: the descriptor just made.
                let fdf = unsafe { libc::fcntl(fd as i32, libc::F_GETFD) };
                // SAFETY: as above.
                let fl = unsafe { libc::fcntl(fd as i32, libc::F_GETFL) };
                // SAFETY: as above.
                unsafe { libc::close(fd as i32) };
                // The word had **no** effect: no `CLOEXEC`, no `NONBLOCK`.
                fdf & libc::FD_CLOEXEC == 0 && fl & libc::O_NONBLOCK == 0
            };
            r.check(
                &format!("eventfd(0, {label}) is served and the word took no effect ({fd})"),
                ok,
            );
        }

        // `inotify_init` is the flagless form: `F_GETFL` 0 and no `CLOEXEC`.
        let ifd = call(INOTIFY_INIT_LEGACY, 0, 0);
        r.check(&format!("inotify_init() is served ({ifd})"), ifd >= 0);
        if ifd >= 0 {
            // SAFETY: the live descriptor.
            let fl = unsafe { libc::fcntl(ifd as i32, libc::F_GETFL) };
            // SAFETY: as above.
            let fdf = unsafe { libc::fcntl(ifd as i32, libc::F_GETFD) };
            r.check(
                &format!("and F_GETFL is O_RDONLY ({fl:#x}) with FD_CLOEXEC clear ({fdf})"),
                fl & libc::O_ACCMODE == libc::O_RDONLY && fdf & libc::FD_CLOEXEC == 0,
            );
            // SAFETY: as above.
            unsafe { libc::close(ifd as i32) };
        }
        // The reference the legacy pair is compared against: `inotify_init1` with
        // `IN_CLOEXEC` **does** set the flag, so the cleanness above is the
        // legacy door's doing and not the kernel's default.
        let c = call(INOTIFY_INIT1, 0x80000, 0);
        if c >= 0 {
            // SAFETY: the live descriptor.
            let fdf = unsafe { libc::fcntl(c as i32, libc::F_GETFD) };
            r.check(
                &format!("and inotify_init1(IN_CLOEXEC) DOES set it ({fdf})"),
                fdf & libc::FD_CLOEXEC != 0,
            );
            // SAFETY: as above.
            unsafe { libc::close(c as i32) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // The asm-generic table never had these spellings, so the claim would be
        // about a number no aarch64 caller can pass. Stated rather than skipped
        // silently: a program that printed nothing here would look like a pass.
        r.check(
            "the legacy eventfd/inotify_init numbers are x86_64-only, so aarch64 has nothing to claim",
            true,
        );
    }

    r.finish()
}
