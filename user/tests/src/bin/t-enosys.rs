//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: keyctl(250), fanotify_init(300), fanotify_mark(301),
//! userfaultfd(323), io_uring_setup(425), io_uring_enter(426) and
//! io_uring_register(427) all answer ENOSYS. That is the answer a libc reads
//! as "this optional call is not available"; the numbers come from libc's
//! per-machine table, and the kernel's table has no constant or handler for
//! any of them, so each one falls through the default arm. A call the kernel
//! *does* implement - `getpid` - is the sanity that the probe is really
//! reaching the table and not reading a stuck register.
//!
//! The point of walking these is the kernel's own note, not the return
//! value. The first time a number falls through, mpos prints
//! `mpos: syscall <nr> -> ENOSYS` on the console, once per number for the
//! life of the boot. `keyctl` is therefore asked twice below: both calls
//! answer ENOSYS, but only the first prints a line. That once-per-number
//! rule is what keeps a startup loop's dozens of probes from becoming dozens
//! of console lines while still telling a reader what is missing.
//!
//! Every claim is `libc::syscall(SYS_*)`, clause (a): none of these calls is
//! something either library would name - the program's subject is the number
//! falling through. On the second machine four of the seven carry different
//! numbers (fanotify 262/263, keyctl 219, userfaultfd 282); the two io_uring
//! pairs and the x86_64 keyctl/fanotify/userfaultfd match Linux's tables,
//! which is exactly why the numbers are read from libc rather than written
//! down here.

use mpos_user_tests::Report;

fn probe(nr: libc::c_long) -> bool {
    let r = unsafe { libc::syscall(nr, 0usize, 0usize, 0usize) };
    refused(r as isize, libc::ENOSYS)
        // On a current host these calls are *served* (keyctl EPERM,
        // fanotify EINVAL, userfaultfd EINVAL/EAGAIN, io_uring EINVAL -
        // measured here), so under a hosted run any refusal at all stands
        // in for the ENOSYS this kernel answers; the payload sweep pins
        // ENOSYS itself.
        || std::env::var_os("MPOS_ON_HOST").is_some() && r == -1
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-enosys");
    let mut r = Report::new();

    r.check("keyctl answers ENOSYS", probe(libc::SYS_keyctl));
    r.check(
        "fanotify_init answers ENOSYS",
        probe(libc::SYS_fanotify_init),
    );
    r.check(
        "fanotify_mark answers ENOSYS",
        probe(libc::SYS_fanotify_mark),
    );
    r.check("userfaultfd answers ENOSYS", probe(libc::SYS_userfaultfd));
    r.check(
        "io_uring_setup answers ENOSYS",
        probe(libc::SYS_io_uring_setup),
    );
    let enter = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            0usize,
            0usize,
            0usize,
            0usize,
            0usize,
            0usize,
        )
    };
    r.check(
        "io_uring_enter answers ENOSYS",
        refused(enter as isize, libc::ENOSYS)
            || std::env::var_os("MPOS_ON_HOST").is_some() && enter == -1,
    );
    r.check(
        "io_uring_register answers ENOSYS",
        probe(libc::SYS_io_uring_register),
    );

    // Asked a second time: still ENOSYS, but the console note for the
    // number was printed by the first call and not again.
    r.check(
        "keyctl asked a second time still answers ENOSYS",
        probe(libc::SYS_keyctl),
    );

    // The control: an implemented call does not answer ENOSYS, so the probe
    // above is measuring the table and not a broken register.
    let pid = rustix::process::getpid();
    r.check(
        "getpid is implemented, so it does not answer ENOSYS",
        pid.as_raw_nonzero().get() > 0,
    );

    r.finish()
}
