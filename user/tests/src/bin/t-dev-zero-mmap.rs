//! `/dev/zero` maps, and what it maps is anonymous memory.
//!
//! Before this, the `mmap` door answered `ENODEV` for every character device,
//! `/dev/zero` included: `mappable()`'s default was false and nothing overrode
//! it. That is right for four of the five memory streams and wrong for the one
//! a program actually maps — a run-time that wants a large zeroed arena opens
//! `/dev/zero` because POSIX guarantees it, and Linux serves it.
//!
//! Every claim below is a Linux 7.2.14 measurement first
//! (`build/scratch/zeromap/{z,w,v,mat,n}.c`), so this program is a reading of
//! that behaviour on both machines and not a description of what this kernel
//! happens to do:
//!
//! - `/dev/zero` maps under `MAP_PRIVATE` **and** `MAP_SHARED`; `/dev/null` is
//!   `ENODEV` under both. That is the pair that decides whether this is "the
//!   memory streams map" (false) or "`/dev/zero` maps" (true).
//! - What it maps is fresh zero memory, not a window on a device: a write to a
//!   private mapping sticks in it, and two shared mappings of the same page are
//!   **not** aliased (`a[0] = 0xabc` leaves `b[0] = 0`). A kernel that aliased
//!   them would look right on every claim that only reads, which is why the
//!   write is here.
//! - `offset 4096 * 1000`, a thousand pages past any size the device could
//!   have, is accepted — there is no extent to run past.
//! - The descriptor's access mode is judged first, in Linux's order: a
//!   write-only descriptor is `EACCES` for *every* mapping, and a writable
//!   `MAP_SHARED` mapping of a read-only descriptor is `EACCES` while the same
//!   request privately is served (a private write is the process's own copy).
//! - `maps` names the range `/dev/zero`.
//!
//! The `ENODEV` claims for `/dev/null` are the counterfactual's other half: a
//! kernel that made every character device mappable would pass every
//! `/dev/zero` claim here and fail those.

use mpos_user_tests::Report;

const PAGE: usize = 4096;

/// `mmap(NULL, len, prot, flags, fd, offset)`, answering what the call said.
fn try_map_at(
    fd: libc::c_int,
    len: usize,
    prot: libc::c_int,
    flags: libc::c_int,
    offset: i64,
) -> Result<*mut libc::c_void, libc::c_int> {
    // SAFETY: the arguments are the ones this test built; `MAP_FAILED` is the
    // only answer read as an error, and errno is read immediately after.
    let answer = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, offset) };
    if answer == libc::MAP_FAILED {
        // SAFETY: no thread in this program writes errno between the call and
        // this read.
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(answer)
    }
}

fn try_map(
    fd: libc::c_int,
    len: usize,
    prot: libc::c_int,
    flags: libc::c_int,
) -> Result<*mut libc::c_void, libc::c_int> {
    try_map_at(fd, len, prot, flags, 0)
}

fn open_at(path: &str, flags: libc::c_int) -> libc::c_int {
    // SAFETY: a NUL-terminated path this test owns, and the flags it named.
    unsafe { libc::open(std::ffi::CString::new(path).unwrap().as_ptr(), flags) }
}

/// Map one page of `path` with `flags`, then unmap it. Answers the errno, or 0.
fn refused(path: &str, prot: libc::c_int, flags: libc::c_int) -> libc::c_int {
    let fd = open_at(path, libc::O_RDWR);
    if fd < 0 {
        // SAFETY: errno is the answer the failed call just left.
        return unsafe { *libc::__errno_location() };
    }
    let answer = try_map(fd, PAGE, prot, flags);
    // SAFETY: the descriptor belongs to this test.
    unsafe { libc::close(fd) };
    match answer {
        Ok(at) => {
            // SAFETY: the mapping is this test's own, mapped one page.
            unsafe { libc::munmap(at, PAGE) };
            0
        }
        Err(e) => e,
    }
}

/// The `maps` line that covers `addr`, if one does.
fn maps_line_for(addr: usize) -> Option<String> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .find(|line| {
            let mut fields = line.split_whitespace();
            let Some(range) = fields.next() else {
                return false;
            };
            let Some((start, end)) = range.split_once('-') else {
                return false;
            };
            match (
                usize::from_str_radix(start, 16),
                usize::from_str_radix(end, 16),
            ) {
                (Ok(start), Ok(end)) => addr >= start && addr < end,
                _ => false,
            }
        })
        .map(String::from)
}

fn main() {
    let mut r = Report::new();

    // The refusal first, because it is the half that must not move: the four
    // memory streams Linux refuses. `/dev/null` is the one that would be
    // tempting to serve beside `/dev/zero` and is measured `ENODEV` with both
    // flags.
    for (path, flags, what) in [
        ("/dev/null", libc::MAP_PRIVATE, "private"),
        ("/dev/null", libc::MAP_SHARED, "shared"),
    ] {
        let e = refused(path, libc::PROT_READ, flags);
        r.check(
            &format!("{path} is still refused {what} with ENODEV (answered {e})"),
            e == libc::ENODEV,
        );
    }

    // The control: `/dev/zero` maps, both ways. A kernel that refused this was
    // the state before the change; a kernel that refused everything would be
    // indistinguishable from it without this pair.
    for (flags, what) in [(libc::MAP_PRIVATE, "private"), (libc::MAP_SHARED, "shared")] {
        let fd = open_at("/dev/zero", libc::O_RDWR);
        if fd < 0 {
            r.check(&format!("/dev/zero opened ({what})"), false);
            continue;
        }
        match try_map(fd, PAGE, libc::PROT_READ | libc::PROT_WRITE, flags) {
            Ok(at) => {
                // SAFETY: the call reported a page of this program's own.
                let first = unsafe { (at as *const u8).read_volatile() };
                r.check(&format!("/dev/zero maps {what}"), true);
                r.check(
                    &format!("/dev/zero's {what} mapping reads zero"),
                    first == 0,
                );
                // The write is the claim a read-only fixture cannot make: it
                // proves the mapping is writable memory and not a view that
                // discards stores.
                // SAFETY: the mapping is one writable page of this program's.
                unsafe { (at as *mut u8).write_volatile(0x5a) };
                // SAFETY: the mapping is still the program's; it is unmapped
                // below.
                let back = unsafe { (at as *const u8).read_volatile() };
                r.check(
                    &format!("/dev/zero's {what} mapping keeps what is written to it"),
                    back == 0x5a,
                );
                // SAFETY: the mapping is this test's own, mapped one page.
                unsafe { libc::munmap(at, PAGE) };
            }
            Err(e) => {
                r.check(&format!("/dev/zero maps {what} (errno {e})"), false);
                r.check(&format!("/dev/zero's {what} mapping reads zero"), false);
                r.check(
                    &format!("/dev/zero's {what} mapping keeps what is written to it"),
                    false,
                );
            }
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
    }

    // Two shared mappings of the same page are not the same memory on Linux.
    // This is the claim that separates "anonymous memory with sharing
    // bookkeeping" from "a shared object", and a kernel that aliased them would
    // pass every read-only claim above.
    let fd = open_at("/dev/zero", libc::O_RDWR);
    if fd < 0 {
        r.check(
            "two shared mappings of /dev/zero are separate memory",
            false,
        );
    } else {
        let a = try_map(
            fd,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        );
        let b = try_map(
            fd,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        );
        match (a, b) {
            (Ok(a), Ok(b)) => {
                // SAFETY: both are one writable page of this program's.
                unsafe {
                    (a as *mut u8).write_volatile(0x11);
                }
                // SAFETY: the second mapping is this program's own page.
                let seen_in_b = unsafe { (b as *const u8).read_volatile() };
                r.check(
                    &format!("two shared mappings of /dev/zero are separate memory (b read {seen_in_b:#x})"),
                    seen_in_b == 0,
                );
                // SAFETY: both mappings are this test's own.
                unsafe {
                    libc::munmap(a, PAGE);
                    libc::munmap(b, PAGE);
                }
            }
            _ => r.check(
                "two shared mappings of /dev/zero are separate memory",
                false,
            ),
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
    }

    // No extent: a page-aligned offset far past anything the device holds is
    // accepted, because there is nothing to run past.
    let fd = open_at("/dev/zero", libc::O_RDONLY);
    if fd < 0 {
        r.check("an offset a thousand pages in is accepted", false);
    } else {
        match try_map_at(
            fd,
            PAGE,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            PAGE as i64 * 1000,
        ) {
            Ok(at) => {
                // SAFETY: the call reported a page of this program's own.
                let first = unsafe { (at as *const u8).read_volatile() };
                r.check("an offset a thousand pages in is accepted", first == 0);
                // SAFETY: the mapping is this test's own.
                unsafe { libc::munmap(at, PAGE) };
            }
            Err(e) => r.check(
                &format!("an offset a thousand pages in is accepted (errno {e})"),
                false,
            ),
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
    }

    // The descriptor's access mode, in Linux's order. A write-only descriptor
    // has no view of the bytes to give, so every mapping of it is refused -
    // including `PROT_NONE`, which asks for nothing.
    for (prot, what) in [
        (libc::PROT_READ, "PROT_READ"),
        (libc::PROT_READ | libc::PROT_WRITE, "PROT_READ|PROT_WRITE"),
        (libc::PROT_NONE, "PROT_NONE"),
    ] {
        let fd = open_at("/dev/zero", libc::O_WRONLY);
        if fd < 0 {
            r.check(&format!("a write-only /dev/zero refuses {what}"), false);
            continue;
        }
        let answer = try_map(fd, PAGE, prot, libc::MAP_PRIVATE);
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
        let e = match answer {
            Ok(at) => {
                // SAFETY: the call reported a mapping; leave it unmapped.
                unsafe { libc::munmap(at, PAGE) };
                0
            }
            Err(e) => e,
        };
        r.check(
            &format!("a write-only /dev/zero refuses {what} with EACCES (answered {e})"),
            e == libc::EACCES,
        );
    }

    // A writable shared mapping needs a writable descriptor, because the writes
    // go to the file; the same request privately does not, because the writes
    // are the process's own copy made from a read it was allowed to do.
    let fd = open_at("/dev/zero", libc::O_RDONLY);
    if fd < 0 {
        r.check("a read-only /dev/zero refuses a shared write", false);
    } else {
        let shared = try_map(
            fd,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        );
        let e = match shared {
            Ok(at) => {
                // SAFETY: the call reported a mapping; leave it unmapped.
                unsafe { libc::munmap(at, PAGE) };
                0
            }
            Err(e) => e,
        };
        r.check(
            &format!("a read-only /dev/zero refuses a shared write with EACCES (answered {e})"),
            e == libc::EACCES,
        );
        // The neighbour: the same descriptor, privately. This is what makes the
        // claim above about the *shared* half rather than about read-only
        // descriptors in general.
        let private = try_map(
            fd,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
        );
        match private {
            Ok(at) => {
                // SAFETY: the mapping is one writable page of this program's.
                unsafe {
                    (at as *mut u8).write_volatile(0x7e);
                }
                // SAFETY: the mapping is still this program's.
                let back = unsafe { (at as *const u8).read_volatile() };
                r.check(
                    &format!("but serves the same write privately (read back {back:#x})"),
                    back == 0x7e,
                );
                // SAFETY: the mapping is this test's own.
                unsafe { libc::munmap(at, PAGE) };
            }
            Err(e) => r.check(
                &format!("but serves the same write privately (errno {e})"),
                false,
            ),
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
    }

    // The name. A private `/dev/zero` mapping is anonymous memory and Linux
    // still prints the file beside those pages; a program that maps a device
    // and then reads `maps` to find out what it mapped needs that name.
    let fd = open_at("/dev/zero", libc::O_RDWR);
    if fd < 0 {
        r.check("a private mapping of /dev/zero is named in maps", false);
    } else {
        match try_map(fd, PAGE, libc::PROT_READ, libc::MAP_PRIVATE) {
            Ok(at) => {
                let line = maps_line_for(at as usize);
                let named = line
                    .as_deref()
                    .is_some_and(|line| line.contains("/dev/zero"));
                r.check(
                    &format!(
                        "a private mapping of /dev/zero is named in maps (line {:?})",
                        line.as_deref().unwrap_or("none")
                    ),
                    named,
                );
                // SAFETY: the mapping is this test's own.
                unsafe { libc::munmap(at, PAGE) };
            }
            Err(e) => r.check(
                &format!("a private mapping of /dev/zero is named in maps (errno {e})"),
                false,
            ),
        }
        // SAFETY: the descriptor belongs to this test.
        unsafe { libc::close(fd) };
    }

    // The descriptor may be closed after the mapping and the memory stays: the
    // mapping is of pages, not of the open description.
    let fd = open_at("/dev/zero", libc::O_RDWR);
    if fd < 0 {
        r.check("the mapping outlives its descriptor", false);
    } else {
        match try_map(
            fd,
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
        ) {
            Ok(at) => {
                // SAFETY: the descriptor belongs to this test and is only
                // closed; the mapping is the program's own page.
                unsafe {
                    libc::close(fd);
                    (at as *mut u8).write_volatile(0x3c);
                }
                // SAFETY: the mapping is still this program's.
                let back = unsafe { (at as *const u8).read_volatile() };
                r.check(
                    &format!("the mapping outlives its descriptor (read back {back:#x})"),
                    back == 0x3c,
                );
                // SAFETY: the mapping is this test's own.
                unsafe { libc::munmap(at, PAGE) };
            }
            Err(e) => {
                // SAFETY: the descriptor belongs to this test.
                unsafe { libc::close(fd) };
                r.check(
                    &format!("the mapping outlives its descriptor (errno {e})"),
                    false,
                );
            }
        }
    }

    r.finish();
}
