//! t-uname - one test for uname, the six-field identity card every build
//! system and motd printer asks for first.
//!
//! The man page pins: the buffer is a struct utsname of six NUL-terminated
//! character arrays, 65 bytes each - sysname, nodename, release, version,
//! machine, domainname - and the call answers 0. Pinned here: the sysname
//! is Linux and the machine is x86_64, the two fields programs branch on;
//! every field stays NUL-terminated within its 65 bytes, so a C caller can
//! printf it with %s and never run off the end; and the domainname field is
//! non-empty rather than garbage. (The kernel's own name lives in `release`
//! - `uname -r` reads "0.1.0" - which is where the kernel's comment in the
//! handler says it moved when sysname became the Linux answer.)

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-uname\n");
    let mut failed = false;

    let mut uts = lin::Utsname::new();
    let r = lin::uname(&mut uts);
    lin::check_eq(&mut failed, b"uname succeeds", r, 0);

    lin::check_true(
        &mut failed,
        b"the sysname field is Linux",
        uts.field(0) == b"Linux",
    );
    lin::check_true(
        &mut failed,
        b"the machine field is x86_64",
        uts.field(4) == b"x86_64",
    );

    // Each of the six 65-byte fields must end in a NUL inside its own slot:
    // byte i*65+64 is the last byte of field i, and it must be the
    // terminator, not payload.
    let mut nul_ok = true;
    for i in 0..6 {
        if uts.bytes[i * 65 + 64] != 0 {
            nul_ok = false;
        }
    }
    lin::check_true(
        &mut failed,
        b"every field is NUL-terminated within its 65 bytes",
        nul_ok,
    );

    lin::check_true(
        &mut failed,
        b"the domainname field is non-empty",
        !uts.field(5).is_empty(),
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-uname: panicked\n")
}
