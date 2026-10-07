//! t-uname - one test for uname, the six-field identity card every build
//! system and motd printer asks for first.
//!
//! The man page pins: the buffer is a struct utsname of six NUL-terminated
//! character arrays, 65 bytes each - sysname, nodename, release, version,
//! machine, domainname - and the call answers 0. Pinned here: the sysname
//! is Linux and the machine names this architecture, the two fields programs
//! branch on; every field stays NUL-terminated within its 65 bytes, so a C
//! caller can printf it with %s and never run off the end; and the
//! domainname field is non-empty rather than garbage. (The kernel's own name
//! lives in `release` - `uname -r` reads "0.1.0" - which is where the
//! kernel's comment in the handler says it moved when sysname became the
//! Linux answer.)
//!
//! The migrated shape is the named `libc::uname`, whose `utsname` is libc's
//! *Linux* struct - six 65-byte fields on both machines (measured against
//! the table), which is what the ABI promises and what musl hands a caller.
//! The no-std version pinned the machine as `x86_64`; that claim was always
//! about *this machine's* answer, so the field now compares against the same
//! string `uname -m` prints here, per architecture.

use mpos_user_tests::Report;

fn field(uts: &libc::utsname, i: usize) -> &[u8] {
    let raw = match i {
        0 => uts.sysname.as_ptr(),
        1 => uts.nodename.as_ptr(),
        2 => uts.release.as_ptr(),
        3 => uts.version.as_ptr(),
        4 => uts.machine.as_ptr(),
        _ => uts.domainname.as_ptr(),
    };
    let bytes = unsafe { core::slice::from_raw_parts(raw as *const u8, 65) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(65);
    &bytes[..end]
}

fn main() {
    println!("t-uname");
    let mut r = Report::new();

    let mut uts = unsafe { core::mem::zeroed::<libc::utsname>() };
    let ret = unsafe { libc::uname(&mut uts) };
    r.check("uname succeeds", ret == 0);

    r.check("the sysname field is Linux", field(&uts, 0) == b"Linux");
    let machine = if cfg!(target_arch = "x86_64") {
        b"x86_64".as_slice()
    } else {
        b"aarch64".as_slice()
    };
    r.check(
        "the machine field names this architecture",
        field(&uts, 4) == machine,
    );

    // Each of the six 65-byte fields must end in a NUL inside its own slot:
    // byte i*65+64 is the last byte of field i, and it must be the
    // terminator, not payload.
    let bytes =
        unsafe { core::slice::from_raw_parts(&uts as *const libc::utsname as *const u8, 6 * 65) };
    r.check(
        "every field is NUL-terminated within its 65 bytes",
        (0..6).all(|i| bytes[i * 65 + 64] == 0),
    );

    r.check(
        "the domainname field is non-empty",
        !field(&uts, 5).is_empty(),
    );

    r.finish()
}
