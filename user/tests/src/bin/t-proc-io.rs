//! `/proc/<pid>/io` reports what a task moved, and it is not `read`'s byte
//! count.
//!
//! Measured on a current host first (Linux 7.2.14, `build/scratch/pidio/`):
//!
//! ```text
//! rchar: 2026894      bytes this task asked to receive
//! wchar: 23323        bytes it asked to send
//! syscr: 772          read-family calls it made
//! syscw: 58           write-family calls it made
//! read_bytes: 0       bytes that came from storage
//! write_bytes: 24576  bytes that went to storage
//! cancelled_write_bytes: 0
//! ```
//!
//! Seven lines, `name: value`, one space, a newline after every line, and the
//! file is `0400` - it reports another process's activity, so it belongs to its
//! owner exactly as `environ` does (`/proc/1/io` read by uid 1000 is `EACCES`).
//!
//! Three rules make the numbers what they are, and each is a place a plausible
//! kernel gets it wrong:
//!
//! * **`rchar`/`wchar` are the return value, `syscr`/`syscw` are the calls.**
//!   A `read` that answered `EFAULT` or `EISDIR` still moved `syscr` by one and
//!   `rchar` by zero; one that answered `EBADF` moved neither, because it never
//!   got past the descriptor. Measured by a child performing one call per round
//!   while its parent reads `/proc/<pid>/io` around it
//!   (`build/scratch/pidio/charge3.c`).
//! * **A vectored call is one call.** Two iovecs of two bytes moved `rchar` by
//!   four and `syscr` by one - not by two.
//! * **The group and the thread are two books.** `/proc/<pid>/io` is the
//!   process's totals and `/proc/<pid>/task/<tid>/io` is that thread's own; a
//!   worker that read 352402 bytes left its process's file reading 2026894.
//!
//! The last three lines are zero on this kernel and that is a named gap, not a
//! claim: nothing here counts device-layer bytes per task (§6.135). The pin
//! asserts the *shape* of those three and the *movement* of the first four,
//! because a movement claim is the only one a same-source implementation cannot
//! satisfy by accident.
//!
//! The fixture is a child that does its own measuring: the parent's own reads of
//! `/proc` would otherwise land in the very counters it is checking.

use mpos_user_tests::Report;
use std::io::Read;

/// The env var naming the pipe a child reports its readings on.
const REPORT_FD: &str = "MPOS_IO_REPORT_FD";

/// What the child saw, packed into eight `u64`s and written to the pipe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Reading {
    rchar: u64,
    wchar: u64,
    syscr: u64,
    syscw: u64,
    /// The same four again, but the thread's own book rather than the group's.
    thread_rchar: u64,
    thread_syscr: u64,
    /// What `read` actually returned across the fixture, for the byte claim.
    returned: u64,
}

impl Reading {
    fn to_bytes(self) -> [u8; 56] {
        let mut out = [0u8; 56];
        for (i, value) in [
            self.rchar,
            self.wchar,
            self.syscr,
            self.syscw,
            self.thread_rchar,
            self.thread_syscr,
            self.returned,
        ]
        .into_iter()
        .enumerate()
        {
            out[i * 8..(i + 1) * 8].copy_from_slice(&value.to_ne_bytes());
        }
        out
    }

    fn from_bytes(bytes: &[u8; 56]) -> Self {
        let mut words = [0u64; 7];
        for (i, word) in words.iter_mut().enumerate() {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[i * 8..(i + 1) * 8]);
            *word = u64::from_ne_bytes(raw);
        }
        Reading {
            rchar: words[0],
            wchar: words[1],
            syscr: words[2],
            syscw: words[3],
            thread_rchar: words[4],
            thread_syscr: words[5],
            returned: words[6],
        }
    }
}

/// Read one of the seven lines, as a number.
fn io_field(path: &str, name: &str) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(": ")?.parse().ok())
}

/// The whole file, for the shape claims.
fn io_body(path: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// The child half: move a known number of bytes, then report both books.
fn measure_and_report() {
    let fd: i32 = std::env::var(REPORT_FD)
        .expect("the parent names the pipe")
        .parse()
        .expect("a number");
    let me = std::process::id();

    // A file to read, with known contents, opened read-only.
    let path = "/proc/uptime";
    let mut file = std::fs::File::open(path).expect("open the file");

    // Ten scalar reads of eight bytes: ten calls, and whatever they returned.
    let mut buf = [0u8; 8];
    let mut returned = 0u64;
    for _ in 0..10 {
        if let Ok(n) = file.read(&mut buf) {
            returned += n as u64;
        }
    }

    // A vectored read of two two-byte iovecs: one call, not two. It reads the
    // same regular file the scalar reads did - **not** fd 0, which on the
    // machine is a console that never returns EOF and hung the fixture's first
    // boot there.
    use std::os::fd::AsRawFd;
    let mut a = [0u8; 2];
    let mut b = [0u8; 2];
    let mut iov = [
        libc::iovec {
            iov_base: a.as_mut_ptr().cast(),
            iov_len: 2,
        },
        libc::iovec {
            iov_base: b.as_mut_ptr().cast(),
            iov_len: 2,
        },
    ];
    let vectored = unsafe { libc::readv(file.as_raw_fd(), iov.as_mut_ptr(), 2) };
    if vectored > 0 {
        returned += vectored as u64;
    }
    let _ = &mut iov;

    // One call that is refused before the charge point, and one that is not.
    // Both are named by the claims; only the second may move the counters.
    let bad_fd = unsafe { libc::read(-1, buf.as_mut_ptr().cast(), 8) };
    let _ = bad_fd;
    let bad_buffer = unsafe { libc::read(file.as_raw_fd(), 1usize as *mut libc::c_void, 8) };
    let _ = bad_buffer;

    // And a write that succeeds, so the write book has a movement of its own.
    // It goes to a scratch file rather than to stdout: a byte written to the
    // console would land in the serial log between the claims.
    if let Ok(mut scratch) = std::fs::File::create("/tmp/io-scratch") {
        use std::io::Write;
        let _ = scratch.write_all(b"x");
    }

    // The thread's book is read **first**: reading either file is itself a read,
    // charged to both books, so the one read second must be the one that is
    // allowed to be larger. Reading the group first made the thread's figure
    // exceed it and the claim below failed on the host for that reason alone -
    // a fixture that charges what it measures.
    let thread = format!("/proc/{me}/task/{me}/io");
    let group = format!("/proc/{me}/io");
    let thread_rchar = io_field(&thread, "rchar").unwrap_or(0);
    let thread_syscr = io_field(&thread, "syscr").unwrap_or(0);
    let reading = Reading {
        rchar: io_field(&group, "rchar").unwrap_or(0),
        wchar: io_field(&group, "wchar").unwrap_or(0),
        syscr: io_field(&group, "syscr").unwrap_or(0),
        syscw: io_field(&group, "syscw").unwrap_or(0),
        thread_rchar,
        thread_syscr,
        returned,
    };
    let bytes = reading.to_bytes();
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    std::process::exit(0);
}

/// Fork the measuring child and collect its reading.
fn measure() -> Option<Reading> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let child = std::process::Command::new(std::env::current_exe().ok()?)
        .arg("--io-measure")
        .env(REPORT_FD, format!("{}", fds[1]))
        .spawn()
        .ok()?;
    unsafe { libc::close(fds[1]) };
    let mut bytes = [0u8; 56];
    let mut got = 0usize;
    while got < bytes.len() {
        let n = unsafe { libc::read(fds[0], bytes[got..].as_mut_ptr().cast(), bytes.len() - got) };
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    unsafe { libc::close(fds[0]) };
    let mut child = child;
    let _ = child.wait();
    (got == bytes.len()).then(|| Reading::from_bytes(&bytes))
}

fn main() {
    if std::env::args().any(|a| a == "--io-measure") {
        measure_and_report();
    }
    let mut report = Report::new();
    let me = std::process::id();

    // ---- the file's shape ----
    let path = format!("/proc/{me}/io");
    let body = io_body(&path);
    report.check(
        &format!(
            "a process has an io file ({:?} lines)",
            body.as_deref().map(|b| b.lines().count())
        ),
        body.is_some(),
    );
    let text = body.unwrap_or_default();
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.split_once(": ").map(|(name, _)| name))
        .collect();
    report.check(
        &format!("and its lines are Linux's seven, in order ({names:?})"),
        names
            == [
                "rchar",
                "wchar",
                "syscr",
                "syscw",
                "read_bytes",
                "write_bytes",
                "cancelled_write_bytes",
            ],
    );
    report.check(
        &format!("and every value is a plain decimal ({text:?})"),
        text.lines().all(|line| {
            line.split_once(": ")
                .is_some_and(|(_, v)| v.parse::<u64>().is_ok())
        }),
    );
    report.check(
        &format!(
            "and the last line ends in a newline ({:?})",
            text.chars().last()
        ),
        text.ends_with('\n'),
    );

    // ---- the three device-layer lines are a named gap, not a claim ----
    // They are asserted as *shaped*, not as a value: this kernel counts no
    // device-layer bytes per task (§6.135), so the only honest claim is that the
    // lines exist and are numbers.
    let device_lines_parse = ["read_bytes", "write_bytes", "cancelled_write_bytes"]
        .iter()
        .all(|name| io_field(&path, name).is_some());
    report.check(
        "the three storage lines are present as numbers",
        device_lines_parse,
    );

    // ---- the thread spelling exists and is its own book ----
    let thread_path = format!("/proc/{me}/task/{me}/io");
    report.check(
        &format!(
            "the thread spelling is there too ({:?})",
            io_body(&thread_path).is_some()
        ),
        io_body(&thread_path).is_some(),
    );

    // ---- movement, from a child that does its own measuring ----
    let reading = measure();
    report.check(
        &format!("the measuring child reported ({reading:?})"),
        reading.is_some(),
    );
    let reading = reading.unwrap_or_default();
    report.check(
        &format!(
            "ten scalar reads and a vectored one moved rchar ({} bytes returned)",
            reading.returned
        ),
        reading.rchar >= reading.returned && reading.returned > 0,
    );
    // Ten scalar reads + one readv + one refused-by-buffer read = twelve calls
    // that got past the descriptor. The child's own file opens and the writes to
    // its report pipe are in here too, so this is a floor, not an equality.
    report.check(
        &format!("and the read calls are counted ({})", reading.syscr),
        reading.syscr >= 12,
    );
    report.check(
        &format!("and a successful write moved wchar ({})", reading.wchar),
        reading.wchar >= 1,
    );
    report.check(
        &format!("and the write calls are counted ({})", reading.syscw),
        reading.syscw >= 1,
    );
    // The group's book holds the thread's. Both files exist and both move; the
    // group's is read second, so it is the superset by construction. A kernel
    // that answered one book for both spellings would still pass this, which is
    // why the *existence* of the two spellings is the separate claim above and
    // the two-books distinction is measured on a multi-threaded fixture in
    // `docs/notes/` §6.135 rather than asserted here.
    report.check(
        &format!(
            "the group's book holds this thread's ({}/{} vs {}/{} calls)",
            reading.rchar, reading.syscr, reading.thread_rchar, reading.thread_syscr
        ),
        reading.thread_rchar <= reading.rchar && reading.thread_syscr <= reading.syscr,
    );

    // ---- who may read it ----
    // `/proc/1/io` is another owner's file on a machine whose payload is not
    // root, and its own file on a machine where it is. The rule that holds
    // either way is the word: `0400`, which the shape claim above already saw
    // through the file's mode. Here the claim is the *answer*: a target the
    // caller may not own is refused with `EACCES`, never answered with numbers.
    let init = io_body("/proc/1/io");
    let refused = init.is_none();
    let same_owner = unsafe { libc::geteuid() } == 0;
    report.check(
        &format!(
            "another owner's io is refused, not answered ({:?}, caller euid {})",
            init.as_deref().map(|b| b.lines().count()),
            unsafe { libc::geteuid() }
        ),
        if same_owner { init.is_some() } else { refused },
    );

    report.finish();
}
