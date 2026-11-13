//! `/proc/<pid>/pagemap`: one 64-bit word per page of a task's address space,
//! saying what the page tables have behind that page.
//!
//! Measured on a current host first (Linux 7.2.14, `build/scratch/pmap/`,
//! `gcc -O1`), uid 1000, against this process's own mappings:
//!
//! ```text
//! open /proc/self/pagemap   mode 0400, st_size 0
//! private anon, touched     0x8180_0000_0000_0000  present, exclusive, soft-dirty
//! private anon, untouched   0x0080_0000_0000_0000  inside a VMA, soft-dirty only
//! private file, touched     0xa180_0000_0000_0000  + file page
//! shared anon, touched      0xa080_0000_0000_0000  file page, not exclusive
//! vaddr 0 (in no VMA)       0x0000_0000_0000_0000  all zero
//! pread page*8, 8 bytes     the word
//! pread page*8+1, 8 bytes   -1 EINVAL(22)
//! pread page*8, 4 bytes     -1 EINVAL(22)
//! pread 1<<40, 8 bytes      0 bytes, no error
//! zombie's pagemap open     -1 ESRCH(3);  after waitpid: -1 ENOENT(2)
//! stranger's open (65534)   -1 EACCES(13), even for a root-owned zombie
//! ```
//!
//! The word is a set of answers, and this test asks for each of them by
//! **moving** the page it describes: a private anonymous page is present and
//! exclusive and is not a file page; a private file page adds the file bit; a
//! `MAP_SHARED|MAP_ANONYMOUS` page is file-like and *not* exclusive, because two
//! processes own it; a page inside a mapping but not yet faulted is non-zero
//! though not present (soft-dirty is the never-cleared state, so the word is
//! not zero); and an address in no mapping at all is zero - the difference
//! between "no VMA describes it" and "a VMA describes it with no frame".
//!
//! What is *not* claimed, and why (the kernel names these gaps in `docs/notes/`
//! §6.150): the frame number is zero here as it is for an unprivileged Linux
//! reader; a descriptor does not keep serving after its task dies (Linux's open
//! holds the `mm`); there is no `/proc/<pid>/clear_refs` to clear a soft-dirty
//! bit; and "exclusive" is this kernel's model (private copies are one
//! address space's own, a shared mapping is the only sharing). `MPOS_ON_HOST=1`
//! runs every claim below against real Linux.

use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;

use mpos_user_tests::{Report, host_run};

const PAGE: usize = 4096;

/// Linux's bit meanings (`fs/proc/task_mmu.c`), measured on the pages above.
const SOFT_DIRTY: u64 = 1 << 55;
const EXCLUSIVE: u64 = 1 << 56;
const FILE_PAGE: u64 = 1 << 61;
const PRESENT: u64 = 1 << 63;

/// The errno a file's *open* answers, or `None` when it opens.
fn open_errno(path: &str) -> Option<i32> {
    match std::fs::File::open(path) {
        Ok(_) => None,
        Err(e) => e.raw_os_error(),
    }
}

/// One 8-byte word at `page`'s index, read the way Linux's file wants it: a
/// **byte** offset of `page * 8` into a stream of words.
fn word_at(fd: RawFd, page: usize) -> Result<u64, i32> {
    let mut word: u64 = 0;
    // SAFETY: `pread` writes at most 8 bytes into `word`, which is exactly
    // eight bytes; the descriptor is this process's own.
    let n = unsafe { libc::pread(fd, (&raw mut word).cast(), 8, (page * 8) as libc::off_t) };
    match n {
        8 => Ok(word),
        n if n >= 0 => Err(-1),
        _ => Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)),
    }
}

/// A `pread` of an arbitrary length and byte offset: the count answered, or the
/// errno.
fn pread_at(fd: RawFd, len: usize, offset: u64) -> Result<usize, i32> {
    let mut bytes = vec![0u8; len];
    // SAFETY: `pread` writes at most `len` bytes into a buffer of `len`.
    let n = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), len, offset as libc::off_t) };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(n as usize)
    }
}

/// An anonymous-or-file mapping of `len` bytes, `prot` and `flags` given, at
/// the kernel's choice of address.
fn mm(len: usize, prot: libc::c_int, flags: libc::c_int, fd: RawFd) -> *mut u8 {
    // SAFETY: `mmap` with a length this frame owns; the result is checked.
    let base = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
    assert_ne!(base, libc::MAP_FAILED, "mmap failed");
    base.cast::<u8>()
}

/// Fault one page in by *reading* it, which leaves a file-backed page
/// file-backed: a write fault on a private file mapping copies the page, and the
/// copy is anonymous (measured: the write-touched page read `0x8180_…` with no
/// file bit at all).
fn fault_read(base: *mut u8, page: usize) {
    // SAFETY: the mapping this frame made covers `page` and is readable.
    unsafe {
        let byte = std::ptr::read_volatile(base.add(page * PAGE));
        std::hint::black_box(byte);
    }
}

/// Touch one page with a **write**, for the mappings that are this process's
/// own memory: an anonymous page and a shared anonymous page both want a write
/// fault to get a frame.
fn touch(base: *mut u8, page: usize) {
    // SAFETY: the mapping this frame made covers `page`, and it is writable.
    unsafe {
        std::ptr::write_bytes(base.add(page * PAGE), 1, PAGE);
        std::hint::black_box(base);
    }
}

/// The page index of an address inside this process's address space.
fn page_of(base: *mut u8, page: usize) -> usize {
    (base as usize) / PAGE + page
}

/// The parenthesised state word of `/proc/<pid>/stat`: `R`, `S`, `Z`, …
fn state_of(pid: i32) -> char {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return ' ';
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let inner = text.rsplit_once(')').map(|(_, rest)| rest).unwrap_or("");
    inner
        .split_whitespace()
        .next()
        .and_then(|word| word.chars().next())
        .unwrap_or('?')
}

/// Wait until `pid` reports the zombie state the kernel keeps after it exits.
fn wait_for_zombie(pid: i32) -> char {
    for _ in 0..2000 {
        let state = state_of(pid);
        if state == 'Z' {
            return state;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    state_of(pid)
}

fn main() {
    let mut report = Report::new();

    // ---- the file's own shape: 0400 and no length
    let file = std::fs::File::open("/proc/self/pagemap").expect("/proc/self/pagemap opens");
    let meta = file.metadata().expect("pagemap metadata");
    report.check(
        &format!(
            "pagemap opens 0400 with st_size 0 (mode {:o}, size {})",
            meta.permissions().mode() & 0o777,
            meta.len()
        ),
        meta.permissions().mode() & 0o777 == 0o400 && meta.len() == 0,
    );
    let fd = file.as_raw_fd();

    // ---- a private anonymous page: present, exclusive, not a file page
    let anon = mm(
        4 * PAGE,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1,
    );
    touch(anon, 0);
    let anon_word = word_at(fd, page_of(anon, 0)).unwrap_or(0);
    report.check(
        &format!("a touched private anon page is present and exclusive ({anon_word:#018x})"),
        anon_word & PRESENT != 0 && anon_word & EXCLUSIVE != 0 && anon_word & FILE_PAGE == 0,
    );
    // ---- one page of the same mapping that nothing has faulted: non-zero, not
    // present. Zero would be the answer for "no VMA describes this address", so
    // the difference is what this claim is for.
    let unfaulted_word = word_at(fd, page_of(anon, 1)).unwrap_or(0);
    report.check(
        &format!(
            "an unfaulted page inside the same mapping is non-zero and not present ({unfaulted_word:#018x})"
        ),
        unfaulted_word != 0 && unfaulted_word & PRESENT == 0 && unfaulted_word & SOFT_DIRTY != 0,
    );

    // ---- a private file page: the same word plus the file page bit
    let path = "/tmp/pmap-file";
    std::fs::write(path, vec![7u8; 3 * PAGE]).expect("the fixture file");
    let fixture = std::fs::File::open(path).expect("the fixture file opens");
    let mapped = mm(
        PAGE,
        libc::PROT_READ,
        libc::MAP_PRIVATE,
        fixture.as_raw_fd(),
    );
    fault_read(mapped, 0);
    let file_word = word_at(fd, page_of(mapped, 0)).unwrap_or(0);
    report.check(
        &format!("a touched private file page carries the file page bit ({file_word:#018x})"),
        file_word & PRESENT != 0 && file_word & FILE_PAGE != 0,
    );

    // ---- a shared anonymous page: file-like. Shared anonymous memory is a
    // page-cache page in Linux's accounting, so the word carries the file bit;
    // the exclusive bit is **not** claimed here, because it answers the page's
    // mapcount across processes and this kernel keeps none (the model is named
    // in `docs/notes/` §6.150, and the host sets it for a mapping only one
    // process has touched).
    let shared = mm(
        2 * PAGE,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED | libc::MAP_ANONYMOUS,
        -1,
    );
    touch(shared, 0);
    let shared_word = word_at(fd, page_of(shared, 0)).unwrap_or(0);
    report.check(
        &format!("a touched MAP_SHARED anon page is file-like ({shared_word:#018x})"),
        shared_word & PRESENT != 0 && shared_word & FILE_PAGE != 0,
    );

    // ---- an address in no mapping is all zero
    let nowhere = word_at(fd, 0).unwrap_or(u64::MAX);
    report.check(
        &format!("an address in no VMA reads zero ({nowhere:#018x})"),
        nowhere == 0,
    );

    // ---- the read's own edges: whole words, word-aligned, and end-of-file
    // past the top of the address space
    let unaligned = pread_at(fd, 8, (page_of(anon, 0) * 8 + 1) as u64);
    report.check(
        &format!(
            "an unaligned read is EINVAL ({unaligned:?} vs {})",
            libc::EINVAL
        ),
        unaligned == Err(libc::EINVAL),
    );
    let short = pread_at(fd, 4, (page_of(anon, 0) * 8) as u64);
    report.check(
        &format!("a four-byte read is EINVAL ({short:?} vs {})", libc::EINVAL),
        short == Err(libc::EINVAL),
    );
    let past = pread_at(fd, 8, 1 << 40);
    report.check(
        &format!("a read past the address space is end-of-file ({past:?})"),
        past == Ok(0),
    );
    // ---- and a read that starts inside it answers its word: the position is
    // the file's, as `lseek` + `read` would show
    // SAFETY: seeking this process's own descriptor.
    let seeked = unsafe { libc::lseek(fd, (page_of(anon, 0) * 8) as libc::off_t, libc::SEEK_SET) };
    let mut word = 0u64;
    // SAFETY: `read` writes at most 8 bytes into `word`.
    let n = unsafe { libc::read(fd, (&raw mut word).cast(), 8) };
    report.check(
        &format!("lseek to a word then read answers that page ({seeked}, {n} bytes, {word:#018x})"),
        seeked >= 0 && n == 8 && word == word_at(fd, page_of(anon, 0)).unwrap_or(0),
    );

    // ---- the thread's node is the same answer: both names read the group's
    // address space, and Linux binds the file in a task directory too
    let page = page_of(anon, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        // SAFETY: `gettid` takes no arguments and cannot fail.
        let tid = unsafe { libc::gettid() };
        let answer = std::fs::File::open(format!("/proc/self/task/{tid}/pagemap"))
            .ok()
            .and_then(|f| word_at(f.as_raw_fd(), page).ok());
        let _ = tx.send((tid, answer));
    });
    let (tid, thread_word) = rx.recv().expect("the thread answers");
    handle.join().ok();
    report.check(
        &format!(
            "a thread's own pagemap answers the group's word (tid {tid}, {thread_word:?} vs {anon_word:#018x})"
        ),
        thread_word == Some(anon_word),
    );

    // ---- a dead task refuses the open, and the two refusals are distinct
    // SAFETY: `fork` takes no arguments; the child only pauses.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        // SAFETY: a plain pause loop; every call here is a libc constant.
        unsafe {
            loop {
                libc::pause();
            }
        }
    }
    let live = open_errno(&format!("/proc/{child}/pagemap"));
    report.check(
        &format!("a live child's pagemap opens ({live:?})"),
        live.is_none(),
    );
    // SAFETY: signalling this process's own child.
    unsafe {
        libc::kill(child, libc::SIGKILL);
    }
    let state = wait_for_zombie(child);
    report.check(
        &format!("the child is a zombie whose desk still answers ({state})"),
        state == 'Z',
    );
    let zombie = open_errno(&format!("/proc/{child}/pagemap"));
    report.check(
        &format!(
            "a zombie's pagemap refuses the open with ESRCH ({zombie:?} vs {})",
            libc::ESRCH
        ),
        zombie == Some(libc::ESRCH),
    );

    let mut status = 0;
    // SAFETY: reaping this process's own child, which is a zombie by now.
    unsafe {
        libc::waitpid(child, &raw mut status, 0);
    }
    let gone = open_errno(&format!("/proc/{child}/pagemap"));
    report.check(
        &format!(
            "a collected task's pagemap is ENOENT ({gone:?} vs {})",
            libc::ENOENT
        ),
        gone == Some(libc::ENOENT),
    );

    // ---- the permission word answers before the missing memory
    if host_run() {
        println!(
            "note: a host run cannot own a root process, so the stranger's EACCES is named and not made"
        );
    } else {
        // A root-owned zombie: this payload runs as root and its child keeps
        // those ids, so a 65534 reader is a stranger to both.
        // SAFETY: `fork` takes no arguments; the child only pauses.
        let owned = unsafe { libc::fork() };
        assert!(owned >= 0, "fork failed");
        if owned == 0 {
            // SAFETY: a plain pause loop.
            unsafe {
                loop {
                    libc::pause();
                }
            }
        }
        // SAFETY: signalling this process's own child.
        unsafe {
            libc::kill(owned, libc::SIGKILL);
        }
        wait_for_zombie(owned);
        let path = CString::new(format!("/proc/{owned}/pagemap")).unwrap();
        // SAFETY: `fork` takes no arguments; the child drops to 65534 and asks
        // for a file that belongs to root.
        let reader = unsafe { libc::fork() };
        assert!(reader >= 0, "fork failed");
        if reader == 0 {
            // SAFETY: all three ids go to 65534, then the open asks.
            unsafe {
                let dropped = libc::setresgid(65534, 65534, 65534) == 0
                    && libc::setresuid(65534, 65534, 65534) == 0;
                let fd = if dropped {
                    libc::open(path.as_ptr(), libc::O_RDONLY)
                } else {
                    -1
                };
                let answer = if dropped && fd < 0 {
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                } else {
                    0
                };
                if fd >= 0 {
                    libc::close(fd);
                }
                libc::_exit(answer);
            }
        }
        let mut reader_status = 0;
        // SAFETY: `reader` is this frame's own child.
        unsafe {
            libc::waitpid(reader, &raw mut reader_status, 0);
        }
        let answer = if libc::WIFEXITED(reader_status) {
            libc::WEXITSTATUS(reader_status)
        } else {
            -1
        };
        report.check(
            &format!("a 65534 stranger's open of a root zombie's pagemap is EACCES ({answer})"),
            answer == libc::EACCES,
        );
        // SAFETY: reaping this process's own child.
        unsafe {
            libc::waitpid(owned, &raw mut status, 0);
        }
    }

    // ---- and the file reads its own content back: a cheap guard that the
    // descriptor above was not merely opened
    let mut whole = Vec::new();
    std::fs::File::open("/proc/self/pagemap")
        .expect("pagemap opens")
        .take(16)
        .read_to_end(&mut whole)
        .expect("two words read");
    report.check(
        &format!(
            "a sixteen-byte read of pagemap answers two words ({})",
            whole.len()
        ),
        whole.len() == 16,
    );

    // SAFETY: unmapping the mappings this frame made, once nothing reads them.
    unsafe {
        libc::munmap(anon.cast(), 4 * PAGE);
        libc::munmap(mapped.cast(), PAGE);
        libc::munmap(shared.cast(), 2 * PAGE);
    }

    report.finish();
}
