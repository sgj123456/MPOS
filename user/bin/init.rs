//! init: the first process, and the last one standing.
//!
//! It has one job - keep a shell available. The Linux shape is `fork` then
//! `execve` in the child and `wait4` in the parent. On mpos `fork(2)` answers
//! ENOSYS, and `execve` cannot swap a live task's image, so `execve` here is
//! the kernel's spawn shape: it starts /bin/sh as a *child* of init and
//! answers the child's pid. init then waits for that child and starts another
//! when it ends - which is what makes the machine keep a shell up.
//!
//! It is also where every orphan ends up: when a process's parent dies first,
//! the kernel hands its children to init, and init reaps them here.
//!
//! musl programs that statically link and use their own `__libc_start_main`
//! expect `AT_PHDR`, `AT_RANDOM`, `AT_EXECFN`, `AT_BASE`, and the rest of
//! the Linux auxv - mpos lays those out the way Linux does, which is what
//! makes execve from a musl-built shell into another musl-built program work.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Where the shell lives. There is no PATH yet, so this is the whole name.
/// NUL-terminated: `execve` on the other side walks until it finds a NUL, and
/// a Rust `&[u8]` slice carries no terminator of its own.
const SHELL: &[u8] = b"/bin/sh\0";

/// How long to wait before trying the shell again, and how far that wait may
/// grow. A shell that cannot be started at all would otherwise be retried in a
/// tight loop, burning the core to print the same failure; backing off keeps
/// the machine alive and readable while it says so.
const RETRY_MS: usize = 1000;
const RETRY_MAX_MS: usize = 8000;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    banner();

    // The argv the shell sees: argv[0] is its own name, what every libc's
    // program_invocation_short_name reads.
    let argv: [*const u8; 2] = [b"sh\0".as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];

    let mut backoff = RETRY_MS;
    loop {
        let pid = lin::execve(SHELL.as_ptr(), argv.as_ptr(), envp.as_ptr());
        if pid < 0 {
            let _ = lin::write_all(2, b"init: cannot start ");
            let _ = lin::write_all(2, SHELL);
            let _ = lin::write_all(2, b"\n");
            sleep_ms(backoff);
            backoff = (backoff * 2).min(RETRY_MAX_MS);
            continue;
        }

        // The shell is running: whatever went wrong before is over.
        backoff = RETRY_MS;

        // -1 is "any child": the shell, or one of the orphans the kernel has
        // handed over, so they are reaped rather than piling up as records.
        let _ = lin::wait4(usize::MAX, core::ptr::null_mut(), 0);
        let _ = lin::write_all(2, b"init: shell exited, starting another\n");
    }
}

/// Sleep for `ms` milliseconds through `nanosleep(2)`.
fn sleep_ms(ms: usize) {
    let ts = lin::TimeSpec {
        tv_sec: (ms / 1000) as i64,
        tv_nsec: ((ms % 1000) * 1_000_000) as i64,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let _ = lin::nanosleep(&ts, &mut rem);
}

/// The one line init prints for itself, so a boot log shows who pid 1 is.
fn banner() {
    let _ = lin::write_all(1, b"mpos init: pid ");
    let mut digits = [0u8; 8];
    let _ = lin::write_all(1, &decimal(lin::getpid() as isize, &mut digits));
    let _ = lin::write_all(1, b", starting ");
    // SHELL carries a NUL for execve's benefit; the banner wants the name.
    let _ = lin::write_all(1, &SHELL[..SHELL.len() - 1]);
    let _ = lin::write_all(1, b"\n");
}

/// `value` in decimal, written back to front into `buf`. The largest pid
/// the kernel can hand out has three digits, so eight bytes is plenty.
fn decimal(value: isize, buf: &mut [u8; 8]) -> &[u8] {
    let mut n = value.max(0) as usize;
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[at..]
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"init: panicked\n")
}
