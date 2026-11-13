//! `/proc/<pid>`'s memory files for a task that has died: which ones refuse the
//! **open**, which read nothing, and which keep serving what their open saw.
//!
//! Measured on a current host first (Linux 7.2.14, `build/scratch/zombiemm/`,
//! `gcc -O1`), uid 1000, against this process's own child:
//!
//! ```text
//! child live:    auxv 368 bytes | environ 512 | cmdline 59 | maps 512
//!                smaps 512 | statm 20 | status 512
//! child a zombie: auxv open -> errno 3 (ESRCH) | environ open -> errno 3
//!                cmdline open ok, read 0 | maps 0 | smaps 0
//!                statm 14 bytes ("0 0 0 0 0 0 0\n") | status 512
//! after reaping: auxv open -> errno 2 (ENOENT) | environ 2 | cmdline 2 | statm 2
//! held auxv fd:  16 bytes read while live, 352 more after death and reap
//! ```
//!
//! Three facts follow from that table and each has a claim below. **The
//! refusal is the open's**, not a read's: a task whose `mm` is gone - a zombie,
//! whose `exit_mm` released it - answers `ESRCH` from `mm_access` in the file's
//! own `open`, while a number that names nothing is `ENOENT` from path
//! resolution. **The files that only name a process still answer** (`statm`
//! reads its seven zeros, `status` its fields), which is the over-refusal this
//! test guards against. And **the descriptor keeps the address space its open
//! grabbed**: Linux's `proc_open` holds (`mmgrab`) it, so bytes still stream
//! after the task is gone - so a kernel that re-renders on every read would
//! answer `ENOENT` where Linux keeps serving.
//!
//! `maps`/`smaps`/`cmdline` are the other half of the same question: their
//! opens succeed and they read **nothing**, because the memory they describe was
//! released. `MPOS_ON_HOST=1` runs every claim of that shape against real Linux.
//!
//! What is *not* claimed: the last `stat` of a task that is gone is `ENOENT`
//! here on both machines, so the difference between "exited" and "reaped" is
//! measured by opening while it is a zombie and again after `waitpid`.

use std::ffi::CString;
use std::io::Read;

use mpos_user_tests::{Report, host_run};

/// The bytes a file holds, or the errno its open answered.
fn read_file(path: &str) -> Result<Vec<u8>, i32> {
    let mut file = std::fs::File::open(path).map_err(|e| e.raw_os_error().unwrap_or(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| e.raw_os_error().unwrap_or(0))?;
    Ok(bytes)
}

/// The errno a file's *open* answers, or `None` when it opens.
fn open_errno(path: &str) -> Option<i32> {
    match std::fs::File::open(path) {
        Ok(_) => None,
        Err(e) => e.raw_os_error(),
    }
}

/// The parenthesised state word of `/proc/<pid>/stat`: `R`, `S`, `Z`, …
fn state_of(pid: i32) -> char {
    match read_file(&format!("/proc/{pid}/stat")) {
        Err(_) => ' ',
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let inner = text.rsplit_once(')').map(|(_, rest)| rest).unwrap_or("");
            inner
                .split_whitespace()
                .next()
                .and_then(|word| word.chars().next())
                .unwrap_or('?')
        }
    }
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

    // ---- a live task's memory files answer with bytes
    // SAFETY: `fork` takes no arguments; the child only pauses, and the parent
    // below kills and reaps it.
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
    let live_auxv = read_file(&format!("/proc/{child}/auxv")).unwrap_or_default();
    report.check(
        &format!(
            "a live task's auxv opens and answers {} bytes (a whole number of pairs)",
            live_auxv.len()
        ),
        !live_auxv.is_empty() && live_auxv.len() % 16 == 0,
    );
    let own_env = read_file("/proc/self/environ").unwrap_or_default();
    let child_env = read_file(&format!("/proc/{child}/environ")).unwrap_or_default();
    report.check(
        &format!(
            "a fork child's environ is the parent's block ({} vs {} bytes)",
            child_env.len(),
            own_env.len()
        ),
        !child_env.is_empty() && child_env == own_env,
    );

    // ---- the descriptor keeps what its open captured, so it is opened first,
    // before any read: a kernel that renders on the read has nothing to serve
    // once the task is gone, while Linux's open grabbed the address space.
    let mut held =
        std::fs::File::open(format!("/proc/{child}/auxv")).expect("a live task's auxv opens");

    // SAFETY: signalling this process's own child.
    unsafe {
        libc::kill(child, libc::SIGKILL);
    }
    let state = wait_for_zombie(child);
    report.check(
        &format!("the child is a zombie whose desk still answers ({state})"),
        state == 'Z',
    );

    // ---- a zombie's memory files refuse the open with ESRCH. `smaps_rollup`
    // is the third: it is a *sum* of the mappings, so a header whose numbers are
    // all zero would describe the process as still there with no memory, where
    // Linux refuses outright (`build/scratch/zombiemm/t.c`).
    for file in ["auxv", "environ", "smaps_rollup"] {
        let path = format!("/proc/{child}/{file}");
        let answer = open_errno(&path);
        report.check(
            &format!(
                "a zombie's {file} refuses the open with ESRCH ({answer:?} vs {})",
                libc::ESRCH
            ),
            answer == Some(libc::ESRCH),
        );
    }

    // ---- and a zombie's read-nothing files are empty, while its named facts
    // are not: the guard against fixing the first two by refusing everything
    for file in ["cmdline", "maps", "smaps", "numa_maps"] {
        let path = format!("/proc/{child}/{file}");
        match read_file(&path) {
            Err(e) => report.check(
                &format!("a zombie's {file} opens and reads nothing (errno {e})"),
                false,
            ),
            Ok(bytes) => report.check(
                &format!(
                    "a zombie's {file} opens and reads nothing ({} bytes)",
                    bytes.len()
                ),
                bytes.is_empty(),
            ),
        }
    }
    let zombie_statm = read_file(&format!("/proc/{child}/statm")).unwrap_or_default();
    report.check(
        &format!(
            "a zombie's statm still answers seven zeros ({:?})",
            String::from_utf8_lossy(&zombie_statm)
        ),
        zombie_statm == b"0 0 0 0 0 0 0\n",
    );
    let zombie_status = read_file(&format!("/proc/{child}/status")).unwrap_or_default();
    report.check(
        &format!(
            "and its status still names it ({} bytes)",
            zombie_status.len()
        ),
        !zombie_status.is_empty(),
    );

    // ---- the open that captured the bytes keeps serving them, though not one
    // byte of the file was read while the task lived
    let mut held_bytes = Vec::new();
    held.read_to_end(&mut held_bytes).unwrap_or(0);
    report.check(
        &format!(
            "an fd opened while the task lived still serves its {} bytes after the death (got {})",
            live_auxv.len(),
            held_bytes.len()
        ),
        !held_bytes.is_empty() && held_bytes == live_auxv,
    );

    let mut status = 0;
    // SAFETY: reaping this process's own child, which is a zombie by now.
    unsafe {
        libc::waitpid(child, &raw mut status, 0);
    }
    // ---- once collected, the number names nothing at all: ENOENT
    for file in ["auxv", "environ", "cmdline"] {
        let path = format!("/proc/{child}/{file}");
        let answer = open_errno(&path);
        report.check(
            &format!(
                "a collected task's {file} is ENOENT ({answer:?} vs {})",
                libc::ENOENT
            ),
            answer == Some(libc::ENOENT),
        );
    }

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
        let path = CString::new(format!("/proc/{owned}/auxv")).unwrap();
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
            &format!("a 65534 stranger's open of a root zombie's auxv is EACCES ({answer})"),
            answer == libc::EACCES,
        );
        // SAFETY: reaping this process's own child.
        unsafe {
            libc::waitpid(owned, &raw mut status, 0);
        }
    }

    report.finish();
}
