//! Temporary diagnostic (batch 22, t-pty), second pass: the identity of every
//! descriptor involved, and every read guarded by an alarm so nothing hangs.
//! Deleted before the batch is committed.
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, fstat, open};
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};

static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: libc::c_int, _i: *mut libc::siginfo_t, _c: *mut libc::c_void) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: usize) -> (libc::c_long, i32) {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd as libc::c_long,
            request as libc::c_long,
            arg,
        )
    };
    (
        rc,
        if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        },
    )
}

/// Arm the alarm and park the count, so "did this call block" is visible.
///
/// 50ms, not the 400ms this started with: the guard only has to fire after the
/// call parked, which is microseconds here and a few milliseconds under the
/// second machine's TCG (`docs/notes/` §15.93). This program makes no claim - it
/// is a diagnostic, and it is in that group so the sweep does not boot it.
fn guard() -> usize {
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: 0,
            tv_usec: 50_000,
        },
    };
    let mut spent = next;
    unsafe { libc::setitimer(0, &next, &mut spent) };
    RAN.load(Ordering::Relaxed)
}

fn unguard() {
    let off = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let mut spent = off;
    unsafe { libc::setitimer(0, &off, &mut spent) };
}

fn who(fd: BorrowedFd<'_>, what: &str) {
    match fstat(fd) {
        Ok(st) => println!(
            "  {what}: fd={} dev={} ino={} mode=0o{:o} rdev={} size={}",
            fd.as_raw_fd(),
            st.st_dev,
            st.st_ino,
            st.st_mode,
            st.st_rdev,
            st.st_size
        ),
        Err(e) => println!("  {what}: fd={} fstat failed {e:?}", fd.as_raw_fd()),
    }
}

fn tryread(fd: BorrowedFd<'_>, what: &str) -> Vec<u8> {
    let before = guard();
    let mut buf = [0u8; 16];
    let got = rustix::io::read(fd, &mut buf);
    let blocked = RAN.load(Ordering::Relaxed) != before;
    unguard();
    let text = match &got {
        Ok(n) => buf[..*n].to_vec(),
        Err(_) => Vec::new(),
    };
    println!("  {what} = {got:?} blocked={blocked} bytes={text:?}");
    text
}

fn trywrite(fd: BorrowedFd<'_>, what: &str, bytes: &[u8]) -> Result<usize, rustix::io::Errno> {
    let before = guard();
    let got = rustix::io::write(fd, bytes);
    let blocked = RAN.load(Ordering::Relaxed) != before;
    unguard();
    println!("  {what} = {got:?} blocked={blocked}");
    got
}

fn main() {
    println!("t-aa-ptyprobe: begin");
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction = on_alarm as extern "C" fn(_, _, _) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) };

    // SAFETY: reading the process's own standard descriptors.
    let (stdin, stdout, stderr) = unsafe {
        (
            BorrowedFd::borrow_raw(0),
            BorrowedFd::borrow_raw(1),
            BorrowedFd::borrow_raw(2),
        )
    };
    who(stdin, "stdin ");
    who(stdout, "stdout");
    who(stderr, "stderr");
    let mut n1: u32 = 0;
    println!(
        "  TIOCGPTN(stdout) = {:?}",
        ioctl(
            stdout.as_raw_fd(),
            libc::TIOCGPTN as libc::c_ulong,
            &mut n1 as *mut u32 as usize
        )
    );

    let master: OwnedFd = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).unwrap();
    who(master.as_fd(), "master");
    let mut number: u32 = 0;
    println!(
        "  TIOCGPTN(master) = {:?} number={number}",
        ioctl(
            master.as_fd().as_raw_fd(),
            libc::TIOCGPTN as libc::c_ulong,
            &mut number as *mut u32 as usize
        )
    );
    let unlocked: i32 = 0;
    println!(
        "  TIOCSPTLCK(master) = {:?}",
        ioctl(
            master.as_fd().as_raw_fd(),
            libc::TIOCSPTLCK as libc::c_ulong,
            &unlocked as *const i32 as usize
        )
    );
    let path = CString::new(format!("/dev/pts/{number}")).unwrap();
    println!("  opening {path:?}");
    let slave: OwnedFd = open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap();
    who(slave.as_fd(), "slave ");

    println!("--- an untouched pair");
    tryread(slave.as_fd(), "read(slave)");
    tryread(master.as_fd(), "read(master)");

    println!("--- master write, then both reads");
    let _ = trywrite(master.as_fd(), "write(master, hi\\r)", b"hi\r");
    tryread(master.as_fd(), "read(master)");
    tryread(slave.as_fd(), "read(slave)");

    println!("--- slave write, then both reads");
    let _ = trywrite(slave.as_fd(), "write(slave, out\\n)", b"out\n");
    tryread(master.as_fd(), "read(master)");
    tryread(slave.as_fd(), "read(slave)");

    println!("t-aa-ptyprobe: end");
    std::process::exit(0)
}
