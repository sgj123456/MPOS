//! `mlock`/`munlock`/`mlock2`/`mlockall`/`munlockall` - the family whose whole
//! shape is a limit and a set of refusals, measured on a current host.
//!
//! This kernel used to answer `0` to every well-formed request and lock
//! nothing. That is the one thing `AGENTS.md` forbids hardest - returning
//! success for something not implemented - and it was invisible to every
//! reader: `VmLck` printed the literal `0 kB`, `smaps`'s `Locked:` too, and no
//! caller could tell a pin from a shrug. Linux's answers, measured on 7.2.14
//! with `RLIMIT_MEMLOCK` 8 MiB (`build/scratch/mlk/*.c`):
//!
//! * `RLIMIT_MEMLOCK` is enforced, and the pages already pinned **inside the
//!   range** come off the count first, so re-locking a range does not charge it
//!   twice (`acct.c`: 8 pages locked twice read `VmLck 32 kB` both times);
//! * a range with **no mapping anywhere** under it is `ENOMEM`, exactly like a
//!   range with a hole in the middle - the answer is about coverage, not about
//!   "the start was below something" (`holes.c`, which had to read
//!   `/proc/self/maps` first: a library mapping had been landing in the range
//!   the earlier probe called empty);
//! * `RLIMIT_MEMLOCK` of 0 without `CAP_IPC_LOCK` is `EPERM`, and that check
//!   comes *first*: `mlock(unmapped, 1)` with the limit at 0 is `EPERM`
//!   (`ov.c`), while `munlock` asks for no capability at all;
//! * an **unaligned** `addr` is not refused - it is rounded out to whole pages
//!   (`zero.c`: `mlock(mapped + 1, 1)` is `0` and charges 4 kB);
//! * `mlock2`'s one flag is `MLOCK_ONFAULT` = **1**, not `0x10`
//!   (`raw.c`: `mlock2(..., 1)` is `0`, `mlock2(..., 0x10)` is `EINVAL`);
//! * `mlockall` refuses an empty word, a bit outside
//!   `MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT`, and `MCL_ONFAULT` on its own
//!   (`bits.c`).
//!
//! Every claim below is a *movement* or a refusal, never a fixed number: the
//! baseline `VmLck` belongs to the test binary and is whatever the loader left
//! behind. The one absolute figure is `RLIMIT_MEMLOCK`'s own row, which is the
//! kernel's to state.

use mpos_user_tests::Report;

/// The task's pinned kilobytes, read the way a caller reads them.
fn vmlck() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("VmLck:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// `RLIMIT_MEMLOCK` as the kernel reports it.
fn memlock_limit() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `&mut limit` is a live `struct rlimit`.
    let answered = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) };
    (answered == 0).then_some(limit.rlim_cur)
}

/// One page-aligned anonymous run of `pages` pages, touched so its frames are
/// there - a page that no fault has reached is still mapped, but a pin on
/// untouched memory is a different measurement.
fn anon(pages: usize) -> Option<*mut u8> {
    // SAFETY: an anonymous private mapping of a length the caller chose, and
    // the returned pointer is checked against `MAP_FAILED` below.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * 4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the mapping is `pages * 4096` bytes and is ours.
    unsafe { std::ptr::write_bytes(base as *mut u8, 1, pages * 4096) };
    Some(base as *mut u8)
}

/// One `mlock`, answering the raw return value.
fn mlock(addr: *mut u8, len: usize) -> i64 {
    // SAFETY: the caller passes a live mapping from [`anon`].
    unsafe { libc::syscall(libc::SYS_mlock, addr, len) as i64 }
}

/// One `munlock`, answering the raw return value.
fn munlock(addr: *mut u8, len: usize) -> i64 {
    // SAFETY: as above.
    unsafe { libc::syscall(libc::SYS_munlock, addr, len) as i64 }
}

/// `mlock2`, whose `flags` word is the point of several claims.
fn mlock2(addr: *mut u8, len: usize, flags: usize) -> i64 {
    // SAFETY: as above.
    unsafe { libc::syscall(libc::SYS_mlock2, addr, len, flags) as i64 }
}

/// `mlockall`'s three bits, as the kernel's own header words.
const MCL_CURRENT: usize = 1;
const MCL_FUTURE: usize = 2;
const MCL_ONFAULT: usize = 4;

fn mlockall(flags: usize) -> i64 {
    // SAFETY: `mlockall` takes a flag word and no pointers.
    unsafe { libc::syscall(libc::SYS_mlockall, flags) as i64 }
}

/// `MAP_LOCKED`, `/usr/include/asm-generic/mman-common.h`'s `0x2000`.
const MAP_LOCKED: libc::c_int = 0x2000;

fn munlockall() -> i64 {
    // SAFETY: `munlockall` takes no arguments.
    unsafe { libc::syscall(libc::SYS_munlockall) as i64 }
}

fn main() {
    let mut report = Report::new();

    let limit = memlock_limit();
    report.check(
        &format!("RLIMIT_MEMLOCK is Linux's 8 MiB row ({limit:?})"),
        limit == Some(8 * 1024 * 1024),
    );

    let Some(base) = anon(8) else {
        report.check("a test page could be mapped", false);
        report.finish();
    };
    let eight = std::hint::black_box(base);

    let before = vmlck();
    let pinned = mlock(eight, 8 * 4096);
    let after = vmlck();
    report.check(
        &format!("mlock of eight pages answers 0 ({pinned})"),
        pinned == 0,
    );
    report.check(
        &format!("and VmLck grows by those eight pages ({before:?} -> {after:?} kB)"),
        matches!((before, after), (Some(b), Some(a)) if a >= b + 32),
    );

    // Locking the same range again charges nothing: `do_mlock` subtracts the
    // pages already pinned inside the range before it compares the limit.
    let again = mlock(eight, 8 * 4096);
    let unchanged = vmlck();
    report.check(
        &format!("locking the same range again answers 0 ({again})"),
        again == 0,
    );
    report.check(
        &format!("and the count does not move ({after:?} -> {unchanged:?} kB)"),
        unchanged == after,
    );

    // An unaligned address is rounded out, not refused.
    let unaligned = mlock(unsafe { eight.add(1) }, 4096);
    report.check(
        &format!("an unaligned address is rounded out, not refused ({unaligned})"),
        unaligned == 0,
    );

    // `munlock` of a sub-range takes off exactly that sub-range's pages.
    let before_part = vmlck();
    let partial = munlock(unsafe { eight.add(2 * 4096) }, 3 * 4096);
    let after_part = vmlck();
    report.check(
        &format!("munlock of a sub-range answers 0 ({partial})"),
        partial == 0,
    );
    report.check(
        &format!("and takes off those three pages ({before_part:?} -> {after_part:?} kB)"),
        matches!((before_part, after_part), (Some(b), Some(a)) if a + 12 == b),
    );

    // A range with nothing under it is `ENOMEM` - the same answer a hole in
    // the middle of a mapping gets, because the question is coverage.
    let nothing = 0x7000_0000_0000usize as *mut u8;
    let empty = mlock(nothing, 4096);
    report.check(
        &format!(
            "a range with no mapping under it is ENOMEM ({empty}, errno {})",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ),
        empty == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM),
    );

    // `mlock2`'s one flag is 1, and `0x10` is a different number entirely.
    let onfault = mlock2(std::ptr::null_mut(), 0, 1);
    report.check(
        &format!("mlock2 accepts MLOCK_ONFAULT, which is 1 ({onfault})"),
        onfault == 0,
    );
    let wrong = mlock2(std::ptr::null_mut(), 0, 0x10);
    report.check(
        &format!(
            "mlock2 refuses 0x10, which is not a flag ({wrong}, errno {})",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ),
        wrong == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
    );

    // `mlockall`'s flag word: empty is a refusal, a stray bit is a refusal, and
    // `MCL_ONFAULT` alone is a refusal because it says *how* and not *what*.
    let empty_word = mlockall(0);
    report.check(
        &format!("mlockall refuses an empty flags word ({empty_word})"),
        empty_word == -1,
    );
    let stray = mlockall(0x40);
    report.check(
        &format!("mlockall refuses a bit outside its three ({stray})"),
        stray == -1,
    );
    let onfault_alone = mlockall(MCL_ONFAULT);
    report.check(
        &format!("mlockall refuses MCL_ONFAULT on its own ({onfault_alone})"),
        onfault_alone == -1,
    );
    let current = mlockall(MCL_CURRENT);
    report.check(
        &format!("mlockall(MCL_CURRENT) is accepted ({current})"),
        current == 0,
    );
    let after_all = vmlck();
    report.check(
        &format!("and VmLck now covers the mapping it pinned ({after_all:?} kB)"),
        after_all.is_some_and(|kb| kb > 0),
    );

    // `munlockall` drops the whole count.
    let dropped = munlockall();
    let after_drop = vmlck();
    report.check(&format!("munlockall answers 0 ({dropped})"), dropped == 0);
    report.check(
        &format!("and VmLck is back to nothing ({after_drop:?} kB)"),
        after_drop == Some(0),
    );

    // `MAP_LOCKED` is `mlock`'s `mmap(2)` spelling: the mapping arrives pinned
    // and `VmLck` grows by its size (`build/scratch/mlk/mapl2.c`).
    let before_map = vmlck();
    // SAFETY: an anonymous private mapping of four pages; the result is
    // checked below.
    let locked_map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4 * 4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_LOCKED,
            -1,
            0,
        )
    };
    let after_map = vmlck();
    report.check(
        &format!("MAP_LOCKED maps without a separate mlock ({locked_map:?})"),
        locked_map != libc::MAP_FAILED,
    );
    report.check(
        &format!("and the mapping arrives pinned ({before_map:?} -> {after_map:?} kB)"),
        matches!((before_map, after_map), (Some(b), Some(a)) if a >= b + 16),
    );

    // A pin is not the advice's to take: `MADV_DONTNEED` on a locked range is
    // `EINVAL` and the contents stay (`build/scratch/mlk/dont.c`).
    if locked_map != libc::MAP_FAILED {
        // SAFETY: the mapping is four pages and is ours.
        unsafe { std::ptr::write_bytes(locked_map as *mut u8, 7, 4096) };
        // SAFETY: as above; `MADV_DONTNEED` takes no pointer.
        let advised = unsafe { libc::madvise(locked_map, 4096, libc::MADV_DONTNEED) };
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        // SAFETY: the first byte of a mapping we own.
        let survived = unsafe { *(locked_map as *const u8) } == 7;
        report.check(
            &format!("MADV_DONTNEED on a locked page is EINVAL ({advised}, errno {errno})"),
            advised == -1 && errno == libc::EINVAL,
        );
        report.check(
            &format!("and the page was not dropped (first byte {survived})"),
            survived,
        );
        // SAFETY: the mapping is ours and no longer needed.
        unsafe { libc::munmap(locked_map, 4 * 4096) };
    }

    let _ = munlockall();

    // `mlockall(MCL_FUTURE)` pins what is mapped *next*, and a forked child
    // inherits neither the pins nor the promise: Linux's `dup_mmap` clears
    // `VMA_LOCKED_MASK` and `mm_init` starts `locked_vm` and `def_vma_flags`
    // fresh (`build/scratch/mlk/futfork.c`).
    let future = mlockall(MCL_FUTURE);
    let before_future = vmlck();
    let Some(future_page) = anon(4) else {
        report.check("a page could be mapped under MCL_FUTURE", false);
        report.finish();
    };
    let after_future = vmlck();
    report.check(
        &format!("mlockall(MCL_FUTURE) is accepted ({future})"),
        future == 0,
    );
    report.check(
        &format!(
            "and a mapping made afterwards is pinned as it is made ({before_future:?} -> {after_future:?} kB)"
        ),
        matches!((before_future, after_future), (Some(b), Some(a)) if a >= b + 16),
    );

    // The child reads none of it - and can unpin a range it never pinned.
    let pinned_now = vmlck();
    // SAFETY: `fork` in a test that immediately reaps the child.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let inherited = vmlck();
        let unpinned = munlock(future_page, 4 * 4096);
        let child_ok = inherited == Some(0) && unpinned == 0;
        // SAFETY: leaving the child, which owns nothing that must be flushed.
        unsafe { libc::_exit(i32::from(!child_ok)) };
    }
    let mut status = 0;
    // SAFETY: `child` is a live child of this process and `&mut status` is a
    // live `c_int`.
    unsafe { libc::waitpid(child, &mut status, 0) };
    report.check(
        &format!(
            "a forked child inherits the pins not at all and unpins freely ({} kB here, wait status {status})",
            pinned_now.unwrap_or(0)
        ),
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    let _ = munlockall();
    // SAFETY: both mappings are ours and no longer needed.
    unsafe {
        libc::munmap(future_page as *mut libc::c_void, 4 * 4096);
        libc::munmap(eight as *mut libc::c_void, 8 * 4096);
    }

    // The limit's other half: at 0, with no `CAP_IPC_LOCK`, the pin is
    // `EPERM` - and that comes before any address is looked at, so an
    // unmapped range answers `EPERM` too. A root caller holds
    // `CAP_IPC_LOCK`, and there is no other answer for it to give - the
    // dropped-identity fixture that reaches it is `t-affinity-rules`'s, and
    // this program states the boundary rather than claiming it green.
    report.finish();
}
