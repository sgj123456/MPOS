//! `/proc/<pid>/syscall`: the call a task is inside, or the word `running`.
//!
//! This is the file `strace -p` reads and the only door for a task's *current*
//! syscall that needs neither a tracer nor a stop. The claims are **relations**
//! to calls the program makes itself, so they hold on both machines where the
//! numbers differ:
//!
//! * a task parked in `read(2)` reports `read`'s number and that call's file
//!   descriptor as its first argument;
//! * a task in ring 3 reports the word `running`;
//! * a zombie reports Linux's three-field form `-1 0x0 0x0`.
//!
//! Measured on Linux 7.2.14 first (`build/scratch/procsyscall/p.c`).

use mpos_user_tests::Report;

/// The `read(2)` number on this machine, from libc's own table.
const READ_NR: i64 = libc::SYS_read as i64;

/// `read(2)` into `buf` on `fd`, answering the byte count or `-errno`.
fn read_fd(fd: i32, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) }
}

/// Split one `/proc/<pid>/syscall` body into its whitespace-separated fields.
fn fields(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Parse a `0x`-prefixed hex field.
fn hex(field: &str) -> Option<u64> {
    u64::from_str_radix(field.strip_prefix("0x")?, 16).ok()
}

fn main() {
    let mut r = Report::new();
    let me = std::process::id();

    // ---- the caller's own call, observed from inside it -------------------
    //
    // The file is read by a `read(2)` this program issues, and the record is
    // taken at dispatch *entry* - so what comes back describes the very call
    // that is fetching it. That makes the two strongest claims possible: the
    // number is `read`'s, and the first argument is the descriptor we passed.
    let path = format!("/proc/{me}/syscall\0");
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY) };
    r.check(
        &format!("the caller's own syscall file opens (fd {fd})"),
        fd >= 0,
    );
    let mut buf = [0u8; 512];
    let n = read_fd(fd, &mut buf);
    let body = String::from_utf8_lossy(&buf[..n.max(0) as usize]).to_string();
    let f = fields(&body);
    r.check(
        &format!("a task inside a call reports nine fields (got {f:?})"),
        f.len() == 9,
    );
    r.check(
        &format!(
            "the number is read(2)'s ({READ_NR}), decimal (got {:?})",
            f.first()
        ),
        f.first().and_then(|v| v.parse::<i64>().ok()) == Some(READ_NR),
    );
    r.check(
        &format!(
            "the first argument is the descriptor this read was given ({fd}, got {:?})",
            f.get(1)
        ),
        f.get(1).and_then(|v| hex(v)).map(|v| v as i32) == Some(fd),
    );
    // The six arguments are all `0x`-prefixed hex, and so are the two registers.
    r.check(
        "the eight fields after the number are 0x-prefixed hex",
        f.len() == 9 && f[1..].iter().all(|field| hex(field).is_some()),
    );
    // The stack pointer and instruction pointer are addresses in the program's
    // own space: nonzero, and not this kernel's idea of a task slot.
    let (sp, pc) = (
        f.get(7).and_then(|v| hex(v)).unwrap_or(0),
        f.get(8).and_then(|v| hex(v)).unwrap_or(0),
    );
    r.check(
        &format!("sp and pc are real addresses (sp {sp:#x}, pc {pc:#x})"),
        sp > 0x1000 && pc > 0x1000,
    );
    unsafe { libc::close(fd) };

    // ---- a task in ring 3 says `running` ---------------------------------
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Spin in user mode: no syscall, so nothing should be in flight.
        loop {
            std::hint::spin_loop();
        }
    }
    r.check("a ring-3 child exists", child > 0);
    if child > 0 {
        // Wait until the child is genuinely running in user mode, then read.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let text = std::fs::read_to_string(format!("/proc/{child}/syscall")).ok();
        r.check(
            &format!("a task in ring 3 answers the word `running` (got {text:?})"),
            text.as_deref() == Some("running\n"),
        );
        unsafe {
            libc::kill(child, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
        }
    }

    // ---- a task parked *inside* a call -----------------------------------
    let mut pp = [0i32; 2];
    r.check("a pipe exists", unsafe { libc::pipe(pp.as_mut_ptr()) } == 0);
    let parked = unsafe { libc::fork() };
    if parked == 0 {
        // Block in read(2) on the pipe's read end: the classic parked task.
        unsafe { libc::close(pp[1]) };
        let mut one = [0u8; 1];
        read_fd(pp[0], &mut one);
        std::process::exit(0);
    }
    r.check("a parked child exists", parked > 0);
    if parked > 0 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let text = std::fs::read_to_string(format!("/proc/{parked}/syscall")).ok();
        let g = fields(text.as_deref().unwrap_or(""));
        r.check(
            &format!("a task parked in read(2) reports read and its descriptor (got {g:?})"),
            g.first().and_then(|v| v.parse::<i64>().ok()) == Some(READ_NR)
                && g.get(1).and_then(|v| hex(v)).map(|v| v as i32) == Some(pp[0]),
        );
        unsafe {
            libc::kill(parked, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(parked, &mut status, 0);
            libc::close(pp[0]);
            libc::close(pp[1]);
        }
    }

    // ---- a zombie, and a pid that is not there ---------------------------
    let zombie = unsafe { libc::fork() };
    if zombie == 0 {
        std::process::exit(7);
    }
    if zombie > 0 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let text = std::fs::read_to_string(format!("/proc/{zombie}/syscall")).ok();
        // `-1` is the number no syscall has, and a task with no saved frame has
        // zeros where the two registers go - Linux's three-field form, not a
        // nine-field line of zeros and not `running`.
        r.check(
            &format!("a zombie answers Linux's three-field form (got {text:?})"),
            text.as_deref() == Some("-1 0x0 0x0\n"),
        );
        let mut status = 0;
        unsafe { libc::waitpid(zombie, &mut status, 0) };
    }

    let absent = std::fs::read_to_string("/proc/999999/syscall").ok();
    r.check(
        "a pid that does not exist has no syscall file",
        absent.is_none(),
    );

    // The mode is the word's own.
    use std::os::unix::fs::MetadataExt;
    let mode = std::fs::metadata(format!("/proc/{me}/syscall"))
        .ok()
        .map(|meta| meta.mode() & 0o777);
    r.check(
        &format!("the file is 0400 (got {mode:?})"),
        mode == Some(0o400),
    );

    r.finish();
}
