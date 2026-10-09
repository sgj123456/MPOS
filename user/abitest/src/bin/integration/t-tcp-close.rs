//! t-tcp-close - does a `close(2)` tell the peer, or is it silent?
//!
//! mpos's TCP is outbound-only (IPv4 `listen` is refused, by `t-tcp`), so the
//! only peer available is one the host is running: slirp forwards a guest
//! connect to `10.0.2.2:PORT` to the host's loopback on the same port. That
//! makes the *host* the instrument, and this program the thing it measures -
//! it connects, says a word, and closes. An orderly close leaves a `FIN` on the
//! wire and the host's `recv` returns end-of-stream; a close that tears the
//! smoltcp socket out of its set before the stack has polled removes any
//! chance of that, and the host sits in silence until it gives up.
//!
//! With nothing listening - which is the normal state of a test run - the
//! connect fails and every check is skipped. The test is a probe, not a gate:
//! the host side decides what happened, and `NOTES.md` §4.3 says how to run it.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// slirp's host, and the port the measuring script listens on.
const PORT: u16 = 4599;

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    lin::puts(b"t-tcp-close\n");
    let mut failed = false;

    let fd = lin::socket(lin::AF_INET as usize, lin::SOCK_STREAM, 0);
    lin::check_true(&mut failed, b"a TCP socket opens", fd >= 0);
    let addr = lin::SockAddrIn::new([10, 0, 2, 2], PORT);
    match lin::connect(fd as i32, &addr) {
        0 => {}
        // Nothing listening on the host: nothing to measure. Say so and pass.
        -111 | -110 | -113 | -115 => {
            lin::puts(b"skip: no host listener on 10.0.2.2:4599 (refused)\n");
            let _ = lin::close(fd as i32);
            lin::done(false);
        }
        other => {
            lin::puts(b"skip: the connection did not come up\n");
            let _ = other;
            let _ = lin::close(fd as i32);
            lin::done(false);
        }
    }

    lin::check_eq(
        &mut failed,
        b"the greeting is sent",
        lin::write(fd as i32, b"hi"),
        2,
    );
    // The act under measurement, from the guest's side only: the close must
    // succeed, and must not be the last thing the stack does quietly.
    lin::check_eq(&mut failed, b"close succeeds", lin::close(fd as i32), 0);

    // And the option that is supposed to change this answer. `SO_LINGER{l_onoff
    // = 1, l_linger = 0}` asks for a reset rather than an orderly close; this
    // stack cannot send one (smoltcp 0.13's `abort()` transmits nothing, and its
    // resets are reply-driven), so the request is recorded, reported back, and
    // the close stays orderly - which is what the host instrument sees for both
    // connections. Pinned here so the gap is measured, not assumed: if the
    // option ever stops round-tripping, that is a new bug, not the known one.
    let second = lin::socket(lin::AF_INET as usize, lin::SOCK_STREAM, 0);
    if lin::connect(second as i32, &addr) == 0 {
        let linger = [1i32, 0i32];
        let r = lin::raw_sys5(
            54, // setsockopt
            second as usize,
            1,  // SOL_SOCKET
            13, // SO_LINGER
            &raw const linger as usize,
            8,
        );
        lin::check_eq(&mut failed, b"SO_LINGER{on, 0} is accepted", r, 0);
        let mut back = [0i32; 2];
        let mut len: usize = 8;
        let g = lin::raw_sys5(
            55, // getsockopt
            second as usize,
            1,
            13,
            &raw mut back as usize,
            &raw mut len as usize,
        );
        lin::check_eq(&mut failed, b"SO_LINGER round-trips", g, 0);
        lin::check_true(
            &mut failed,
            b"and reports the l_onoff/l_linger pair it was given",
            back == [1, 0],
        );
        let _ = lin::write(second as i32, b"bye");
        lin::check_eq(
            &mut failed,
            b"the lingering close succeeds",
            lin::close(second as i32),
            0,
        );
    } else {
        lin::puts(b"skip: no host listener for the reset case\n");
        let _ = lin::close(second as i32);
    }
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-tcp-close: panicked\n")
}
