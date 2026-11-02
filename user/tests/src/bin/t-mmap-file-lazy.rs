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

use mpos_user_tests::{Report, host_run};

const PAGE: usize = 4096;
/// 512 pages: large enough that an eager copy is unmistakable in `mincore`,
/// small enough to write in one pass on the slowest lane.
const PAGES: usize = 512;
const BYTES: usize = PAGES * PAGE;
/// The guest's root is writable; the host reference run has no `/`, so it
/// uses `/tmp`. Both spell the same claims.
fn path() -> std::ffi::CString {
    if host_run() {
        let dir = std::env::var("MPOS_TEST_DIR").unwrap_or_else(|_| "/tmp".into());
        std::ffi::CString::new(format!("{dir}/t-mmap-file-lazy.bin")).unwrap()
    } else {
        c"/t-mmap-file-lazy.bin".into()
    }
}

/// The file the syscall-buffer claim writes into.
fn out_path() -> std::ffi::CString {
    if host_run() {
        let dir = std::env::var("MPOS_TEST_DIR").unwrap_or_else(|_| "/tmp".into());
        std::ffi::CString::new(format!("{dir}/t-mmap-file-lazy.out")).unwrap()
    } else {
        c"/t-mmap-file-lazy.out".into()
    }
}
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
            path().as_ptr(),
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

    // --- a syscall reading an untouched page is served, not refused ---
    //
    // The page is one this process has never touched (`mincore` says so), and
    // it is handed to `write(2)` as the *source* buffer. Linux's `copy_from_user`
    // faults such a page in exactly as a ring-3 load does, so the call returns
    // the byte count; a kernel that only admits pages that are already present
    // answers `EFAULT` for memory the caller legally mapped
    // (`docs/notes/` §10.15a, item 188 - measured on 7.2.11 with this program).
    let last = PAGES - 1;
    let probe = base + last * PAGE;
    let vec3 = vec![0u8; PAGES];
    let mc = unsafe { libc::mincore(base as *mut _, BYTES, vec3.as_ptr() as *mut u8) };
    r.check("mincore before the syscall answers 0", mc == 0);
    r.check(
        "the page handed to the syscall is not resident yet",
        vec3[last] & 1 == 0,
    );
    let out = unsafe { libc::open(out_path().as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600) };
    r.check("a second file to write into is open", out >= 0);
    if out >= 0 {
        let n = unsafe { libc::pwrite(out, probe as *const _, 4, 0) };
        r.check(
            "a syscall may read a page this process has not touched",
            n == 4,
        );
        let mut vec4 = vec![0u8; PAGES];
        let mc = unsafe { libc::mincore(base as *mut _, BYTES, vec4.as_mut_ptr() as *mut u8) };
        r.check(
            "and the kernel faulted that page in to do it",
            mc == 0 && vec4[last] & 1 != 0,
        );
        unsafe { libc::close(out) };
        unsafe { libc::unlink(out_path().as_ptr()) };
    }

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
        // A page lying *wholly* past the end of the file is `SIGBUS` on Linux,
        // not zeros - measured on 7.2.11 (`build/scratch/frk/tail.c`): a read
        // of a page wholly beyond EOF kills the reader with `Bus error`, while
        // a byte inside the file's last partial page past its data reads zero.
        // The claim is the second one, because it is a value this kernel can
        // answer; a `SIGBUS` claim would belong to a fault the machine has to
        // raise, and that is its own item.
        let grown = unsafe { libc::ftruncate(fd, (BYTES + 200) as i64) };
        r.check("the file can grow into the mapping's last page", grown == 0);
        if grown == 0 {
            let past = unsafe { ((tail as usize + BYTES + 200) as *const u8).read() };
            r.check(
                "a byte past the file's data in its last page reads zero",
                past == 0,
            );
        }
        unsafe { libc::munmap(tail, BYTES + 2 * PAGE) };
    }

    // --- a private mapping is private across a fork ---
    //
    // The point of `MAP_PRIVATE` is that the child's writes are its own. This
    // kernel got it wrong for a while: `fork` collected every `VmaKind::Frames`
    // range as a *shared object*, and after a private file mapping became
    // frame-backed the child mapped its parent's pages writable and marked
    // nothing - so the parent watched the child's stores appear in its own
    // memory. Linux 7.2.11 answers the other way, and a compositor's heap is
    // exactly such a mapping: a child writing over its parent's metadata is a
    // NULL where a pointer belongs, which is what udevd's crash looked like.
    let priv_map = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    if priv_map == libc::MAP_FAILED {
        r.check("a writable private mapping of the file succeeds", false);
        unsafe { libc::munmap(map, BYTES) };
        unsafe { libc::close(fd) };
        unsafe { libc::unlink(path().as_ptr()) };
        r.finish();
    }
    r.check("a writable private mapping of the file succeeds", true);
    unsafe { core::ptr::write_bytes(priv_map as *mut u8, b'A', BYTES) };

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The child overwrites every page and leaves; nothing it wrote may be
        // visible to the parent.
        unsafe {
            core::ptr::write_bytes(priv_map as *mut u8, b'C', BYTES);
            libc::_exit(0);
        }
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let first = unsafe { (priv_map as *const u8).read() };
    let middle = unsafe { ((priv_map as usize + BYTES / 2) as *const u8).read() };
    r.check(
        "a child's writes to a private file mapping do not reach the parent",
        first == b'A' && middle == b'A',
    );
    unsafe { libc::munmap(priv_map, BYTES) };

    unsafe { libc::munmap(map, BYTES) };
    unsafe { libc::close(fd) };
    unsafe { libc::unlink(path().as_ptr()) };
    r.finish();
}
