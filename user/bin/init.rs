//! init: the first process, and the last one standing.
//!
//! It has one job - keep a shell available. It starts /bin/sh, waits for it to
//! end, and starts another, forever. That is what makes the machine usable the
//! way a real one is: the shell can crash, or be ended with Ctrl-D, and the
//! next prompt appears instead of the system stopping.
//!
//! It is also where every orphan ends up: when a process's parent dies first,
//! the kernel hands its children to init, and init reaps them here. That is
//! why the wait below asks for *any* child rather than for the shell by pid.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

/// Where the shell lives. There is no PATH yet, so this is the whole name.
const SHELL: &[u8] = b"/bin/sh";

/// How long to wait before trying the shell again, and how far that wait may
/// grow. A shell that cannot be started at all would otherwise be retried in a
/// tight loop, burning the core to print the same failure; backing off keeps
/// the machine alive and readable while it says so.
const RETRY_MS: usize = 1000;
const RETRY_MAX_MS: usize = 8000;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    banner();

    let mut backoff = RETRY_MS;
    loop {
        let pid = sys::spawn(SHELL, &[b"sh"]);
        if pid < 0 {
            let _ = sys::write_all(2, b"init: cannot start ");
            let _ = sys::write_all(2, SHELL);
            let _ = sys::write_all(2, b"\n");
            sys::sleep_ms(backoff);
            backoff = (backoff * 2).min(RETRY_MAX_MS);
            continue;
        }

        // The shell is running: whatever went wrong before is over.
        backoff = RETRY_MS;

        // -1 is "any child": the shell, or one of the orphans the kernel has
        // handed over, so they are reaped rather than piling up as records.
        let _ = sys::wait(-1);
        let _ = sys::write_all(2, b"init: shell exited, starting another\n");
    }
}

/// The one line init prints for itself, so a boot log shows who pid 1 is.
fn banner() {
    let _ = sys::write_all(1, b"mpos init: pid ");
    let mut digits = [0u8; 8];
    let _ = sys::write_all(1, &decimal(sys::getpid(), &mut digits));
    let _ = sys::write_all(1, b", starting ");
    let _ = sys::write_all(1, SHELL);
    let _ = sys::write_all(1, b"\n");
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
    sys::abort()
}
