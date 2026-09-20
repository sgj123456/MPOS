//! t-sh - diagnostic: an interactive busybox ash, exec'd straight onto the
//! console terminal (/dev/ttyS0), with the test as its parent ghost.
//!
//! What this pins down is where an interactive shell stops before its first
//! prompt. The child's standard descriptors are the console tty, so the
//! prompt it prints - or the warning it prints instead - lands on the serial
//! wire unfiltered. The parent then reads the child's /proc stat and prints
//! it: the state letter says whether the shell is stopped (a signal it never
//! handled), sleeping (a syscall that never ended), running, or gone.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static TTY: [u8; 11] = *b"/dev/ttyS0\0";
static BUSYBOX: [u8; 13] = *b"/bin/busybox\0";
static ARGV0: [u8; 8] = *b"busybox\0";
static ARGV1: [u8; 3] = *b"sh\0";
static PATH_ENV: [u8; 10] = *b"PATH=/bin\0";
static TERM_ENV: [u8; 11] = *b"TERM=vt100\0";

static mut ENVP: [*const u8; 3] = [core::ptr::null(); 3];
static mut ARGV: [*const u8; 3] = [core::ptr::null(); 3];

/// `/proc/<pid>/stat` spelled into a static buffer.
static mut STAT_PATH: [u8; 24] = [0; 24];

fn stat_path(pid: usize) -> &'static [u8] {
    const PREFIX: [u8; 6] = *b"/proc/";
    unsafe {
        STAT_PATH[..6].copy_from_slice(&PREFIX);
        let mut n = pid;
        let mut digits = [0u8; 10];
        let mut len = 0;
        while n > 0 {
            digits[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
        for (i, &d) in digits[..len].iter().rev().enumerate() {
            STAT_PATH[6 + i] = d;
        }
        let tail = *b"/stat\0";
        STAT_PATH[6 + len..6 + len + 6].copy_from_slice(&tail);
        core::slice::from_raw_parts(core::ptr::addr_of!(STAT_PATH).cast(), 6 + len + 6)
    }
}

/// The third whitespace-separated field of a `/proc/<pid>/stat` line: Linux's
/// state letter. `T` is stopped, `S` sleeping, `R` running.
fn state_letter(bytes: &[u8]) -> u8 {
    let mut field = 0;
    let mut in_field = false;
    for &b in bytes {
        if b == b' ' || b == b'\n' {
            if in_field {
                in_field = false;
                field += 1;
            }
        } else {
            if !in_field {
                in_field = true;
                if field == 2 {
                    return b;
                }
            }
            if field == 2 {
                return b;
            }
        }
    }
    b'?'
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-sh: spawning an interactive ash on /dev/ttyS0\n");
    let mut failed = false;

    let tty = lin::openat(lin::AT_FDCWD, &TTY, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"the console tty opens", tty >= 0);

    let pid = lin::fork();
    if pid == 0 {
        // The child's own process group first, and this is the load-bearing
        // part of the whole test. An interactive shell that finds itself in
        // the background of its controlling terminal raises `SIGTTIN` to stop
        // the group it is in - which until now is the group it *inherited*,
        // and `t-sh` is in it too. The stop then takes the test down with the
        // shell and the run ends with nothing printed at all. This is no more
        // than what every shell does before it `exec`s a job.
        let got = lin::setpgid(0, 0);
        let _ = lin::write(1, b"t-sh: child setpgid = ");
        let _ = lin::puts_isize(got);
        let _ = lin::write(1, b"\n");

        // The child: stdio onto the console terminal, then the shell.
        let _ = lin::dup2(tty as i32, 0);
        let _ = lin::dup2(tty as i32, 1);
        let _ = lin::dup2(tty as i32, 2);
        unsafe {
            ENVP[0] = PATH_ENV.as_ptr();
            ENVP[1] = TERM_ENV.as_ptr();
            ARGV[0] = ARGV0.as_ptr();
            ARGV[1] = ARGV1.as_ptr();
            lin::execve(
                BUSYBOX.as_ptr(),
                core::ptr::addr_of!(ARGV) as *const *const u8,
                core::ptr::addr_of!(ENVP) as *const *const u8,
            );
        }
        lin::die(b"t-sh: the shell did not exec\n");
    }
    lin::check_true(&mut failed, b"the shell was forked", pid > 0);
    // The parent says it too, so the job's group does not depend on which of
    // the two reached the call first. Either may find the other already there.
    let _ = lin::setpgid(pid as i32, pid as i32);

    // Give the shell a moment, then report its /proc state.
    lin::puts(b"t-sh: parent probing\n");
    for _ in 0..50 {
        lin::sched_yield();
    }
    lin::puts(b"t-sh: yields done\n");
    let fd = lin::openat(lin::AT_FDCWD, stat_path(pid as usize), lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the child's /proc entry opens", fd >= 0);
    if fd >= 0 {
        let mut buf = [0u8; 192];
        let got = lin::read(fd as i32, &mut buf);
        if got > 0 {
            lin::puts(b"t-sh: stat = ");
            lin::puts(&buf[..got as usize]);
            let state = state_letter(&buf[..got as usize]);
            lin::puts(b"t-sh: state = ");
            lin::puts(&[state, b'\n']);
        } else {
            lin::puts(b"t-sh: no stat to read\n");
        }
        let _ = lin::close(fd as i32);
    }

    // The shell has no stdin worth waiting on and nobody to type to it: left
    // alive it holds the console and outlives the test that spawned it. SIGCONT
    // first, because a stopped task does not run its own death on SIGKILL.
    let _ = lin::kill(pid as i32, lin::SIGCONT);
    let _ = lin::kill(pid as i32, lin::SIGKILL);
    let mut status = 0i32;
    let reaped = lin::wait4(pid as usize, &mut status, 0);
    lin::check_true(&mut failed, b"the shell was reaped", reaped > 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-sh: panicked\n")
}
