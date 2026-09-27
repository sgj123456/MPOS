//! t-enosys - the calls this kernel has no dispatch arm for.
//!
//! Pinned here: keyctl(250), fanotify_init(300), fanotify_mark(301),
//! userfaultfd(323), io_uring_setup(425), io_uring_enter(426) and
//! io_uring_register(427) all answer ENOSYS. That is the answer a libc
//! reads as "this optional call is not available"; the harness passes the
//! Linux x86_64 numbers from `lin`, and the kernel's table has no constant
//! or handler for any of them, so each one falls through the default arm.
//! A call the kernel *does* implement - `getpid` - is the sanity that the
//! probe is really reaching the table and not reading a stuck register.
//!
//! The point of walking these is the kernel's own note, not the return
//! value. The first time a number falls through, mpos prints
//! `mpos: syscall <nr> -> ENOSYS` on the console, once per number for the
//! life of the boot. `keyctl` is therefore asked twice below: both calls
//! answer ENOSYS, but only the first prints a line. That once-per-number
//! rule is what keeps a startup loop's dozens of probes from becoming
//! dozens of console lines while still telling a reader what is missing.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-enosys\n");
    let mut failed = false;

    lin::check_eq(
        &mut failed,
        b"keyctl (250) answers ENOSYS",
        lin::raw_sys(lin::SYS_KEYCTL, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"fanotify_init (300) answers ENOSYS",
        lin::raw_sys(lin::SYS_FANOTIFY_INIT, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"fanotify_mark (301) answers ENOSYS",
        lin::raw_sys(lin::SYS_FANOTIFY_MARK, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"userfaultfd (323) answers ENOSYS",
        lin::raw_sys(lin::SYS_USERFAULTFD, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"io_uring_setup (425) answers ENOSYS",
        lin::raw_sys(lin::SYS_IO_URING_SETUP, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"io_uring_enter (426) answers ENOSYS",
        lin::raw_sys6(lin::SYS_IO_URING_ENTER, 0, 0, 0, 0, 0, 0),
        lin::ENOSYS,
    );
    lin::check_eq(
        &mut failed,
        b"io_uring_register (427) answers ENOSYS",
        lin::raw_sys(lin::SYS_IO_URING_REGISTER, 0, 0, 0),
        lin::ENOSYS,
    );

    // Asked a second time: still ENOSYS, but the console note for number
    // 250 was printed by the first call and not again.
    lin::check_eq(
        &mut failed,
        b"keyctl asked a second time still answers ENOSYS",
        lin::raw_sys(lin::SYS_KEYCTL, 0, 0, 0),
        lin::ENOSYS,
    );

    // The control: an implemented call does not answer ENOSYS, so the
    // probe above is measuring the table and not a broken register.
    let pid = lin::getpid();
    lin::check_true(
        &mut failed,
        b"getpid is implemented, so it does not answer ENOSYS",
        pid > 0 && pid != lin::ENOSYS,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-enosys: panicked\n")
}
