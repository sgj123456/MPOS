//! A private mapping of an ordinary file is a statement about addresses.
//!
//! `docs/notes/` §10.15 measured the shape this program exists to keep out:
//! `mmap(2)` of a 189 MB shared object spent **3.4 s inside the call**, because
//! the mapping copied every page of the file at map time - and the first touch
//! afterwards was free. A loader mapping a compositor's Mesa stack paid that
//! per object, ~15 s before its first line of output.
//!
//! The claims here are about **residency**, so the counterfactual is a
//! behavioural one, not a stopwatch: with the eager copy every page of the
//! mapping is resident the instant `mmap` returns; with the fault path none is,
//! and a page becomes resident exactly when it is first touched. `mincore(2)`
//! asks the kernel that question directly, which is why this program uses it
//! rather than timing (a timing claim would also pass on a fast host).
//!
//! The file is one this program creates itself, so the test does not depend on
//! what an image happens to ship.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
/// 512 pages: large enough that an eager copy is unmistakable in `mincore`,
/// small enough to write in one pass on the slowest lane.
const PAGES: usize = 512;
const BYTES: usize = PAGES * PAGE;
const PATH: &str = "/t-mmap-file-lazy.bin";
const FILL: u8 = 0x5a;

/// A page's bytes say which page they are, so a wrong page is visible.
fn page_byte(i: usize) -> u8 {
    (i as u8).wrapping_mul(7).wrapping_add(3)
}

fn main() {
    println!("t-mmap-file-lazy");
    let mut r = Report::new();

    // --- the file, written and read back before anything is mapped ---
    let fd = unsafe {
        libc::open(
            c"/t-mmap-file-lazy.bin".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    r.check("a file to map is created", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    let mut block = [FILL; PAGE];
    let mut ok = true;
    for i in 0..PAGES {
        block.fill(page_byte(i));
        let got = unsafe { libc::write(fd, block.as_ptr() as *const _, PAGE) };
        if got != PAGE as isize {
            ok = false;
            break;
        }
    }
    r.check("the file is written in full", ok);
    // The pages this program just wrote are in the page cache, and Linux's
    // `mincore` counts a cached page as resident (`docs/notes/` §10.15: a
    // file already read once makes `mmap`+touch look free). Dropping them is
    // what makes the residency claim about the *mapping*, not about a warm
    // cache this kernel does not have either.
    unsafe { libc::fsync(fd) };
    let adv = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    r.check("the file's cache is dropped before the mapping", adv == 0);
    let st = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    r.check(
        "the file is as long as the mapping will be",
        st == BYTES as i64,
    );
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    // --- the mapping itself: private, readable, and empty of residency ---
    let map = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    r.check(
        "a private mapping of the file succeeds",
        map != libc::MAP_FAILED,
    );
    if map == libc::MAP_FAILED {
        r.finish();
    }
    let base = map as usize;

    // `mincore`'s vector comes from the heap: one byte per page.
    let vec = vec![0u8; PAGES];
    let mc = unsafe { libc::mincore(base as *mut _, BYTES, vec.as_ptr() as *mut u8) };
    r.check("mincore over the fresh file mapping answers 0", mc == 0);
    let resident = vec.iter().filter(|b| **b & 1 != 0).count();
    r.check(
        "no page of the file is resident when the call returns",
        resident == 0,
    );

    // --- one page read: that page, and only that page, becomes resident ---
    let first = unsafe { (base as *const u8).read() };
    r.check(
        "the first page reads the file's bytes",
        first == page_byte(0),
    );
    let mid = unsafe { ((base + (PAGES / 2) * PAGE) as *const u8).read() };
    r.check(
        "a middle page reads the file's bytes",
        mid == page_byte(PAGES / 2),
    );

    let vec2 = vec![0u8; PAGES];
    let mc = unsafe { libc::mincore(base as *mut _, BYTES, vec2.as_ptr() as *mut u8) };
    r.check("mincore after two touches answers 0", mc == 0);
    // Linux maps a *window* around a faulted page (fault-around: up to 16
    // pages of the same file), which `mincore` then reports too - measured on
    // 7.2.11 with this very program. So the claim is not "exactly these two":
    // the pages read are resident, and the mapping is nowhere near full.
    let touched = [0usize, PAGES / 2];
    let present = |i: usize| vec2[i] & 1 != 0;
    let all_read_present = touched.iter().all(|i| present(*i));
    r.check("the pages that were read are resident", all_read_present);
    let resident = vec2.iter().filter(|b| **b & 1 != 0).count();
    r.check(
        "two touches do not populate the whole mapping",
        resident > 0 && resident < PAGES / 2,
    );

    // --- a private write is a copy: the file keeps its byte ---
    let target = base + 3 * PAGE;
    unsafe { (target as *mut u8).write(0xff) };
    let neighbour = unsafe { ((target + 1) as *const u8).read() };
    r.check(
        "a write through the mapping keeps the page's other bytes",
        neighbour == page_byte(3),
    );
    let mut back = [0u8; 1];
    let got = unsafe { libc::pread(fd, back.as_mut_ptr() as *mut _, 1, (3 * PAGE) as i64) };
    r.check(
        "a write through a private mapping does not reach the file",
        got == 1 && back[0] == page_byte(3),
    );

    // --- past the end of the file: zeros, as a short read leaves them ---
    let tail = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            BYTES + 2 * PAGE,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    r.check(
        "a mapping longer than the file succeeds",
        tail != libc::MAP_FAILED,
    );
    if tail != libc::MAP_FAILED {
        let past = unsafe { ((tail as usize + BYTES) as *const u8).read() };
        r.check("the tail past the end of the file reads zero", past == 0);
        unsafe { libc::munmap(tail, BYTES + 2 * PAGE) };
    }

    unsafe { libc::munmap(map, BYTES) };
    unsafe { libc::close(fd) };
    unsafe { libc::unlink(c"/t-mmap-file-lazy.bin".as_ptr()) };
    let _ = PATH;
    r.finish();
}
