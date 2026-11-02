//! Exec from a worker thread: the group's other threads end, and the process
//! keeps the number its leader had.
//!
//! Linux's `de_thread` runs before the image is replaced, and what it leaves
//! behind is measured on 7.2.11 (`build/scratch/execthr/`): a worker's `execv`
//! leaves `Threads: 1`, a sibling parked in a futex never runs again, and
//! `Tgid`/`Pid` are still the numbers the **group leader** was born with - not
//! the execing worker's tid.
//!
//! The shape that made this a defect here is not "the sibling survives": it is
//! that the old image's pages went back to the allocator while a sibling was
//! still executing on them. Measured on the machine
//! (`build/scratch/execthr/s.log`): the sibling died with `t-s[17]: segfault at
//! 40715c ip 000000000040715c error 14` - error 14 is an instruction fetch, in
//! the *old* image's `.text`, which is a page handed to a new owner.
//!
//! So the pin asks three questions, and the third is the one that fails without
//! the fix: does the exec'd image run, does the group have one thread and its
//! leader's number, and did the sibling it ended leave quietly - no fault filed
//! against it, and no corruption noticed in the page it was writing.

use mpos_user_tests::{Report, host_run};
use std::ffi::CString;
use std::io::Write;

const PAGES: usize = 4096;
const PATTERN: u8 = 0x5a;

fn out_path() -> CString {
    // `/tmp` is tmpfs: fine for the guest, and on the host the reference run
    // points this at a file-backed directory nothing else writes.
    let dir = std::env::var("MPOS_TEST_DIR").unwrap_or_else(|_| "/tmp".into());
    CString::new(format!("{dir}/t-exec-threads.out")).unwrap()
}

fn sib_path() -> CString {
    let dir = std::env::var("MPOS_TEST_DIR").unwrap_or_else(|_| "/tmp".into());
    CString::new(format!("{dir}/t-exec-threads.sib")).unwrap()
}

fn read_file(path: &CString) -> String {
    let path = path.to_str().unwrap();
    std::fs::read_to_string(path).unwrap_or_default()
}

/// One `Key: value` line of `/proc/self/status`, as a number.
fn status_number(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest.trim_start_matches(':').trim().parse().ok();
        }
    }
    None
}

/// The `comm` the kernel files a fault under, which is this program's own name.
fn comm() -> String {
    std::fs::read_to_string("/proc/self/comm")
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default()
}

/// The kernel log, or `None` when this machine will not show it to us.
///
/// The fault report is a `syslog(2)` entry (`docs/notes/` §6.73); on a host run
/// the sandbox answers `EPERM` for every type, so the claim that needs it is
/// reported as unmeasurable rather than made up.
fn kernel_log() -> Option<String> {
    const SYSLOG_ACTION_READ_ALL: libc::c_int = 3;
    let mut buf = vec![0u8; 1 << 16];
    let n = unsafe {
        libc::syscall(
            libc::SYS_syslog,
            SYSLOG_ACTION_READ_ALL,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn main() {
    // ---- phase 2: the image the exec selected ----
    if std::env::var_os("MPOS_ET_PHASE").is_some() {
        let mut out = std::fs::File::create(out_path().to_str().unwrap()).unwrap();
        let tgid = status_number("Tgid").unwrap_or(0);
        let pid = status_number("Pid").unwrap_or(0);
        let threads = status_number("Threads").unwrap_or(0);
        writeln!(
            out,
            "phase2 tgid={tgid} pid={pid} threads={threads} getpid={}",
            unsafe { libc::getpid() }
        )
        .unwrap();
        std::process::exit(0);
    }

    let mut r = Report::new();
    let _ = std::fs::remove_file(out_path().to_str().unwrap());
    let _ = std::fs::remove_file(sib_path().to_str().unwrap());

    // The describing run has to be a fresh fork: exec replaces this process.
    let child = unsafe { libc::fork() };
    if child < 0 {
        r.check("a child process could be forked to exec in", false);
        r.finish();
    }

    if child == 0 {
        // ---- the child: a spinner sibling, a worker that execs, and a leader
        // parked so it never finishes first ----
        //
        // The sibling's file is opened without `CLOEXEC` and therefore stays
        // open across the exec (the execer shares the descriptor table): if its
        // page is recycled under it, it can still say so.
        let sib_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(sib_path().to_str().unwrap())
            .unwrap();
        let fd = std::os::fd::IntoRawFd::into_raw_fd(sib_file);
        unsafe {
            SIB_FD = fd;
        }

        static mut SIB_FD: libc::c_int = -1;
        static mut PAGE: *mut u8 = std::ptr::null_mut();
        static mut START: i32 = 0;
        unsafe {
            PAGE = libc::mmap(
                std::ptr::null_mut(),
                PAGES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            ) as *mut u8;
            for i in 0..PAGES {
                *PAGE.add(i) = PATTERN;
            }
        }

        unsafe fn report(bytes: &[u8]) {
            let fd = SIB_FD;
            if fd >= 0 {
                libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
            }
        }

        extern "C" fn spinner(_: *mut libc::c_void) -> *mut libc::c_void {
            unsafe {
                START = 1;
                loop {
                    // A beat, so "the exec ended this thread" is a measurement
                    // and not an inference: the file stops growing only if
                    // nothing is running here any more.
                    report(b".");
                    let mut bad = false;
                    for i in 0..PAGES {
                        if *PAGE.add(i) != PATTERN {
                            bad = true;
                            break;
                        }
                    }
                    if bad {
                        report(b"CORRUPT\n");
                        return std::ptr::null_mut();
                    }
                    libc::usleep(1000);
                }
            }
        }

        extern "C" fn execer(_: *mut libc::c_void) -> *mut libc::c_void {
            unsafe {
                while START == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                // Long enough that the sibling's beat is a steady rhythm by
                // the time the parent starts measuring it: the exec has to
                // land *while* something is running, or the pin is away.
                std::thread::sleep(std::time::Duration::from_millis(500));
                let exe = CString::new("/proc/self/exe").unwrap();
                let arg0 = exe.clone();
                let argv = [arg0.as_ptr(), std::ptr::null()];
                // The environment is inherited and the phase added to it: an
                // exec that hands over only the one word it needs would also
                // drop `MPOS_TEST_DIR`, which is how the host reference run
                // finds its file - measured by this pin's own first run.
                let mut env: Vec<CString> = std::env::vars_os()
                    .filter(|(k, _)| k != "MPOS_ET_PHASE")
                    .filter_map(|(k, v)| {
                        CString::new(format!("{}={}", k.to_str()?, v.to_str()?)).ok()
                    })
                    .collect();
                env.push(CString::new("MPOS_ET_PHASE=2").unwrap());
                let mut envp: Vec<*const libc::c_char> = env.iter().map(|s| s.as_ptr()).collect();
                envp.push(std::ptr::null());
                libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(70);
            }
        }

        unsafe {
            let mut t: libc::pthread_t = std::mem::zeroed();
            libc::pthread_create(&mut t, std::ptr::null(), spinner, std::ptr::null_mut());
            let mut t2: libc::pthread_t = std::mem::zeroed();
            libc::pthread_create(&mut t2, std::ptr::null(), execer, std::ptr::null_mut());
            // The leader parks: on Linux the exec ends it too, and this kernel
            // must not let it outlive the image either.
            loop {
                libc::pause();
            }
        }
    }

    // ---- the parent ----
    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(child, &mut status, 0);
    }
    let exited = unsafe { libc::WIFEXITED(status) } && unsafe { libc::WEXITSTATUS(status) } == 0;

    let out = read_file(&out_path());
    r.check(
        "a worker thread's exec ran the new image",
        exited && out.starts_with("phase2"),
    );
    let field = |key: &str| -> Option<u64> {
        out.split_whitespace()
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.parse().ok()))
    };
    r.check(
        "and the group has one thread afterwards, as Linux's de_thread leaves it",
        field("threads=") == Some(1),
    );
    r.check(
        "and the group kept the number its leader had, not the execing thread's",
        field("tgid=") == Some(child as u64)
            && field("pid=") == Some(child as u64)
            && field("getpid=") == Some(child as u64),
    );

    let sib_size = || {
        std::fs::metadata(sib_path().to_str().unwrap())
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let first = sib_size();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let second = sib_size();
    let sib = read_file(&sib_path());
    r.check("the sibling was running before the exec", first > 0);
    r.check(
        "and the exec ended it, as Linux's de_thread does",
        first == second,
    );
    r.check(
        "and it was not left running on a recycled page",
        !sib.contains("CORRUPT"),
    );

    match kernel_log() {
        Some(log) => {
            let me = comm();
            let filed = log
                .lines()
                .filter(|line| line.contains("segfault") || line.contains("traps:"))
                .filter(|line| line.trim_end().starts_with(&format!("{me}[")))
                .count();
            r.check(
                "and no fault was filed against the sibling the exec ended",
                filed == 0,
            );
        }
        None => {
            println!(
                "note: this machine does not show its kernel log to us, so the \
                 'no fault was filed' claim is not measured here"
            );
            if !host_run() {
                r.check(
                    "and a payload sweep can read the kernel log the fault report goes to",
                    false,
                );
            }
        }
    }

    r.finish();
}
