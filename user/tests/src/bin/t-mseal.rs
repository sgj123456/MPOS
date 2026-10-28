//! t-mseal - `mseal(2)`: a span sealed against unmapping, resizing and
//! protection changes.
//!
//! Pinned here is the shape measured on Linux 7.2.11
//! (`build/scratch/mseal/probe4.c`, `probe5.c`, `probe6.c`, `probe7.c`), which
//! is not the shape a reader guesses from the man page:
//!
//! - `flags` must be zero and the start must be page-aligned (`EINVAL`); a
//!   zero length is a success that seals nothing; sealing a sealed span again
//!   is also a success;
//! - the span must be **fully mapped** - a hole inside it is `ENOMEM`, and so
//!   is an address nothing owns;
//! - after a seal, `munmap` and `mprotect` over a sealed page are `EPERM`, and
//!   so is `mremap` of it whether it would move or grow in place;
//! - a `munmap` that only *abuts* the sealed page still works: sealing page 0
//!   of a four-page mapping leaves pages 1..4 free to be given back;
//! - a destructive `madvise` is refused (`EPERM`) only when the sealed pages
//!   are anonymous **and** not writable: a sealed writable anonymous page and
//!   a sealed page of a file-backed mapping both answer `0`, because the pages
//!   there can be put back from where they came from;
//! - seals survive `fork` but not `exec` (exec replaces the address space).
//!
//! On a host run this is the reference direction: `mseal` is unprivileged, so
//! the same binary runs against Linux and every claim must hold there - which
//! is what says the expectations are Linux's rather than this kernel's.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_SHARED: i32 = 0x01;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;
const MREMAP_MAYMOVE: i32 = 1;
const MADV_DONTNEED: i32 = 4;
const MADV_FREE: i32 = 8;

const EINVAL: i32 = -22;
const EPERM: i32 = -1;
const ENOMEM: i32 = -12;

/// The errno of a failed call, in Linux's sign, or `Ok(())`.
fn errno(rc: libc::c_long) -> Result<(), i32> {
    if rc < 0 {
        return Err(-std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

fn mseal(addr: *mut u8, len: usize, flags: u64) -> Result<(), i32> {
    errno(unsafe { libc::syscall(libc::SYS_mseal, addr, len, flags) })
}

fn munmap(addr: *mut u8, len: usize) -> Result<(), i32> {
    errno(unsafe { libc::munmap(addr.cast(), len) } as libc::c_long)
}

fn mprotect(addr: *mut u8, len: usize, prot: i32) -> Result<(), i32> {
    errno(unsafe { libc::mprotect(addr.cast(), len, prot) } as libc::c_long)
}

fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32) -> Result<*mut u8, i32> {
    let rc = unsafe { libc::mmap(addr.cast(), len, prot, flags | MAP_ANONYMOUS, -1, 0) };
    if rc == libc::MAP_FAILED {
        return Err(-std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(rc.cast())
}

/// A fresh anonymous mapping of `pages` pages, writable and touched, so that
/// the pages are really there.
fn anon(pages: usize) -> *mut u8 {
    let map = mmap(
        std::ptr::null_mut(),
        pages * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE,
    )
    .expect("an anonymous mapping");
    unsafe { std::ptr::write_bytes(map, 0x5a, pages * PAGE) };
    map
}

fn main() {
    let mut r = Report::new();

    // ---- the argument shapes -------------------------------------------
    {
        let map = anon(4);
        r.check(
            "a fresh span seals and answers 0",
            mseal(map, PAGE, 0).is_ok(),
        );
        r.check(
            "sealing the sealed span again still answers 0",
            mseal(map, PAGE, 0).is_ok(),
        );
        r.check("flags is EINVAL", mseal(map, PAGE, 1) == Err(EINVAL));
        r.check(
            "an unaligned start is EINVAL",
            mseal(unsafe { map.add(1) }, PAGE, 0) == Err(EINVAL),
        );
        r.check(
            "a zero length is 0 and seals nothing",
            mseal(unsafe { map.add(0x10_0000) }, 0, 0).is_ok(),
        );
    }

    // ---- the map must be there ----------------------------------------
    {
        // A span with a hole in it: four pages, the middle two given back.
        let map = anon(4);
        assert!(munmap(unsafe { map.add(PAGE) }, 2 * PAGE).is_ok());
        r.check(
            "a span with a hole inside it is ENOMEM",
            mseal(map, 4 * PAGE, 0) == Err(ENOMEM),
        );
        r.check(
            "the mapped page before the hole seals fine",
            mseal(map, PAGE, 0).is_ok(),
        );

        // An address nothing owns.
        let stray = 0x1000 as *mut u8;
        r.check(
            "an address nothing owns is ENOMEM",
            mseal(stray, PAGE, 0) == Err(ENOMEM),
        );
        r.check(
            "a NULL start is ENOMEM",
            mseal(std::ptr::null_mut(), PAGE, 0) == Err(ENOMEM),
        );
    }

    // ---- what a seal refuses afterwards -------------------------------
    {
        let map = anon(4);
        assert!(mseal(map, PAGE, 0).is_ok());
        r.check(
            "munmap of the sealed page is EPERM",
            munmap(map, PAGE) == Err(EPERM),
        );
        r.check(
            "mprotect of the sealed page is EPERM",
            mprotect(map, PAGE, PROT_READ) == Err(EPERM),
        );
        r.check(
            "a munmap that covers the seal and its neighbours is EPERM too",
            munmap(map, 4 * PAGE) == Err(EPERM),
        );
        // The unsealed tail, given back only by itself, is still free.
        r.check(
            "a munmap that only abuts the seal answers 0",
            munmap(unsafe { map.add(PAGE) }, 3 * PAGE).is_ok(),
        );
    }

    // ---- resizing a sealed range --------------------------------------
    {
        let map = anon(4);
        assert!(mseal(map, PAGE, 0).is_ok());
        let in_place = unsafe { libc::mremap(map.cast(), 2 * PAGE, 2 * PAGE, 0) };
        r.check(
            "mremap in place over a sealed page is EPERM",
            in_place == libc::MAP_FAILED
                && -std::io::Error::last_os_error().raw_os_error().unwrap_or(0) == EPERM,
        );
        let moved = unsafe { libc::mremap(map.cast(), PAGE, PAGE, MREMAP_MAYMOVE as libc::c_int) };
        r.check(
            "mremap(MAYMOVE) of a sealed page is EPERM",
            moved == libc::MAP_FAILED
                && -std::io::Error::last_os_error().raw_os_error().unwrap_or(0) == EPERM,
        );

        // A fixed mapping over the sealed page is refused, over the tail is not.
        let over_sealed = mmap(map, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED);
        r.check(
            "MAP_FIXED over the sealed page is EPERM",
            over_sealed == Err(EPERM),
        );
        let over_tail = mmap(
            unsafe { map.add(PAGE) },
            PAGE,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_FIXED,
        );
        r.check(
            "MAP_FIXED over the unsealed tail answers 0",
            matches!(over_tail, Ok(_)),
        );
    }

    // ---- madvise's page-destroying advice -----------------------------
    {
        // Writable anonymous: the pages can come back, so the advice is fine.
        let map = anon(2);
        assert!(mseal(map, 2 * PAGE, 0).is_ok());
        r.check(
            "MADV_DONTNEED on sealed writable anonymous pages is 0",
            errno(unsafe { libc::madvise(map.cast(), 2 * PAGE, MADV_DONTNEED) } as libc::c_long)
                .is_ok(),
        );
        r.check(
            "MADV_FREE on sealed writable anonymous pages is 0",
            errno(unsafe { libc::madvise(map.cast(), 2 * PAGE, MADV_FREE) } as libc::c_long)
                .is_ok(),
        );

        // Read-only anonymous: dropping the pages would lose the contents the
        // program can no longer write back, so the promise keeps them.
        let map = anon(2);
        assert!(mprotect(map, 2 * PAGE, PROT_READ).is_ok());
        assert!(mseal(map, 2 * PAGE, 0).is_ok());
        r.check(
            "MADV_DONTNEED on sealed read-only anonymous pages is EPERM",
            errno(unsafe { libc::madvise(map.cast(), 2 * PAGE, MADV_DONTNEED) } as libc::c_long)
                == Err(EPERM),
        );
        r.check(
            "MADV_FREE on sealed read-only anonymous pages is EPERM",
            errno(unsafe { libc::madvise(map.cast(), 2 * PAGE, MADV_FREE) } as libc::c_long)
                == Err(EPERM),
        );

        // A file still holds what the pages came from, so a file-backed range
        // is not the madvise refusal's shape.
        let path = std::ffi::CString::new("/tmp/t-mseal-file").unwrap();
        let file = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o644) };
        assert!(file >= 0, "the fixture file opens");
        let data = [0x11u8; 2 * PAGE];
        unsafe {
            assert_eq!(
                libc::write(file, data.as_ptr().cast(), data.len()),
                data.len() as isize
            );
        }
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * PAGE,
                PROT_READ,
                MAP_PRIVATE,
                file,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "the file mapping is created");
        r.check(
            "a seal over a file-backed private mapping answers 0",
            mseal(map.cast(), 2 * PAGE, 0).is_ok(),
        );
        r.check(
            "MADV_DONTNEED on the sealed file-backed range is 0",
            errno(unsafe { libc::madvise(map, 2 * PAGE, MADV_DONTNEED) } as libc::c_long).is_ok(),
        );
        r.check(
            "mprotect of a sealed file-backed range is EPERM",
            errno(unsafe { libc::mprotect(map, 2 * PAGE, PROT_READ | PROT_WRITE) } as libc::c_long)
                == Err(EPERM),
        );
        unsafe {
            libc::close(file);
            libc::unlink(path.as_ptr());
        }
    }

    // ---- a seal travels with a fork -----------------------------------
    {
        let map = anon(1);
        assert!(mseal(map, PAGE, 0).is_ok());
        let child = unsafe { libc::fork() };
        if child == 0 {
            // The child inherits the seal: the same page cannot be unmapped.
            let refused = munmap(map, PAGE) == Err(EPERM);
            unsafe { libc::_exit(if refused { 0 } else { 1 }) };
        }
        let mut status = 0;
        let waited = unsafe { libc::waitpid(child, &mut status, 0) };
        let child_exit = (status >> 8) & 0xff;
        r.check(
            "a forked child inherits the seal",
            waited == child && libc::WIFEXITED(status) && child_exit == 0,
        );
    }

    // ---- a shared anonymous mapping is not an anonymous private one ----
    {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * PAGE,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        r.check(
            "a seal over a shared anonymous mapping answers 0",
            mseal(map.cast(), 2 * PAGE, 0).is_ok(),
        );
        r.check(
            "munmap of the sealed shared range is EPERM",
            errno(unsafe { libc::munmap(map, 2 * PAGE) } as libc::c_long) == Err(EPERM),
        );
    }

    r.finish();
}
