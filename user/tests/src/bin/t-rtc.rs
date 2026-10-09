//! t-rtc - the hardware clock as a device node, on both machines.
//!
//! `/dev/rtc0` is the door `hwclock(8)` reaches the real-time clock through, and
//! OpenRC's `hwclock` service is the reason it matters: its `start` walks
//! `/dev/rtc` and `/dev/rtc[0-9]*`, so a machine with neither cannot set its system
//! clock and says `[ !! ]` on every boot. That was the second machine, whose node
//! was gated `target_arch = "x86_64"` while the thing behind the node - the wall
//! clock - was already `crate::hal`'s, implemented by both (`NOTES.md` §13.15).
//!
//! What is pinned here is the node's *contract*, not a machine's register:
//!
//! * both names exist, because the shipped `hwclock` tries both;
//! * `RTC_RD_TIME` answers the kernel's own wall clock - one clock, so `date` and
//!   `hwclock -r` cannot disagree - which is checked by converting the fields back
//!   to a second with `timegm` and comparing against `clock_gettime`, and by
//!   comparing the fields themselves against `gmtime_r`'s for that same second;
//! * `RTC_SET_TIME` is served, and is exercised with the value the device just
//!   reported, so the check cannot move the machine's clock;
//! * the family is keyed on the *device number*, so a request aimed at some other
//!   descriptor is `ENOTTY` - the property that makes serving it on a new machine a
//!   change to one node and not to every `ioctl` a program can make;
//! * an unimplemented member of the family is `ENOTTY`, which is what Linux answers
//!   for an RTC with no alarm.
//!
//! The ioctl numbers are spelled here rather than taken from `libc`, which binds
//! neither these nor `struct rtc_time` on any platform. They are the numbers the
//! macros make, measured from a real `linux/rtc.h` rather than recalled:
//! `RTC_RD_TIME` = `_IOR('p', 0x09, struct rtc_time)` = `0x80247009`,
//! `RTC_SET_TIME` = `_IOW('p', 0x0a, struct rtc_time)` = `0x4024700a`, with the
//! struct thirty-six bytes - and the `'p'` magic is what that header uses (24
//! spellings, none with `'t'`), which is also the byte Alpine's `hwclock` sends.
//!
//! **Not run on the host**, and the reason is worth recording: this host's
//! `/dev/rtc0` is `crw-rw---- root:clock`, so the first check cannot even open it
//! unprivileged, and a privileged run would be writing the machine's real clock
//! rather than a guest's. The pin's validation is the measured constants above plus
//! the two sweeps, where the device is this kernel's own node.

use std::ffi::c_void;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// `struct rtc_time` is nine `int`s, and their order and meaning are the first nine
/// fields of `struct tm` - which is why the two convert to each other field for
/// field with no table in between.
const RTC_TIME_INTS: usize = 9;

/// `_IOR('p', 0x09, struct rtc_time)`.
const RTC_RD_TIME: libc::c_ulong = 0x8024_7009;
/// `_IOW('p', 0x0a, struct rtc_time)`.
const RTC_SET_TIME: libc::c_ulong = 0x4024_700A;
/// `_IO('t', 0x7f)`: the modern magic byte with a sub-command nothing implements, so
/// it reaches the RTC arm and is refused *by* it rather than by the default arm.
const RTC_UNKNOWN: libc::c_ulong = 0x747f;

/// One ioctl, with `errno` read immediately on failure.
///
/// Through `libc::syscall` rather than `libc::ioctl`, because `ioctl`'s request
/// parameter is `c_int` in the musl bindings and `c_ulong` in the gnu ones, and a
/// request is neither: it is an `unsigned long` word the kernel decodes. The
/// syscall route takes the number from `libc::SYS_ioctl`, which is the maintained
/// per-architecture table - the same reason the other tests reach for `libc` at
/// all (`user/tests/Cargo.toml`).
fn rtc_ioctl(fd: libc::c_int, request: libc::c_ulong, arg: *mut c_void) -> (libc::c_long, i32) {
    // SAFETY: every caller passes a buffer it owns and a request whose argument
    // shape is the one named above; the pointer travels unchanged.
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

fn open_ro(path: &str) -> Result<libc::c_int, std::io::Error> {
    let path = std::ffi::CString::new(path).expect("the path has no interior NUL");
    // SAFETY: `path` is a valid NUL-terminated string for the call's duration.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn close(fd: libc::c_int) {
    // SAFETY: the descriptor came from `open` above and is closed once.
    unsafe { libc::close(fd) };
}

fn main() {
    println!("t-rtc");
    let mut r = Report { failed: false };

    // Both names, because a `hwclock` walks a list rather than asking the kernel.
    let fd = match open_ro("/dev/rtc0") {
        Ok(fd) => {
            r.check("/dev/rtc0 opens", true);
            fd
        }
        Err(err) => {
            r.check(&format!("/dev/rtc0 opens (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    };
    match open_ro("/dev/rtc") {
        Ok(alias) => {
            r.check("/dev/rtc opens (the alias hwclock falls back to)", true);
            close(alias);
        }
        Err(err) => r.check(&format!("/dev/rtc opens (got {err})"), false),
    }

    // The read: nine ints, which are a calendar.
    let mut fields = [0i32; RTC_TIME_INTS];
    let (rc, err) = rtc_ioctl(fd, RTC_RD_TIME, fields.as_mut_ptr().cast::<c_void>());
    r.check(
        &format!("RTC_RD_TIME succeeds (got rc {rc}, errno {err})"),
        rc == 0,
    );
    if rc != 0 {
        close(fd);
        println!("FAILED");
        std::process::exit(1);
    }
    println!(
        "     RTC_RD_TIME -> {:04}-{:02}-{:02} {:02}:{:02}:{:02} wday={} yday={} isdst={}",
        fields[5] + 1900,
        fields[4] + 1,
        fields[3],
        fields[2],
        fields[1],
        fields[0],
        fields[6],
        fields[7],
        fields[8],
    );

    // The fields as a second. `timegm` is `gmtime`'s inverse, and the RTC's fields
    // are UTC in exactly that convention (`tm_mon` 0-based, `tm_year` since 1900),
    // so this is the device's own answer converted back with the libc's own code.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_sec = fields[0];
    tm.tm_min = fields[1];
    tm.tm_hour = fields[2];
    tm.tm_mday = fields[3];
    tm.tm_mon = fields[4];
    tm.tm_year = fields[5];
    tm.tm_wday = fields[6];
    tm.tm_yday = fields[7];
    tm.tm_isdst = fields[8];
    // SAFETY: `tm` is a correctly sized `struct tm` the call may rewrite.
    let rtc_epoch = unsafe { libc::timegm(&mut tm) };
    r.check(
        &format!("the fields convert to a second (timegm gave {rtc_epoch})"),
        rtc_epoch > 1_000_000_000,
    );

    // The claim the node exists for: one clock. `date` and `hwclock -r` read the
    // same seconds because both come from the kernel's wall clock, so the device's
    // answer and `clock_gettime`'s differ only by the time the two calls take.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `ts` is a correctly sized `struct timespec` the call owns.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let delta = ts.tv_sec - rtc_epoch;
    println!(
        "     RTC_RD_TIME vs clock_gettime(CLOCK_REALTIME): {rtc_epoch} vs {} (delta {delta})",
        ts.tv_sec
    );
    r.check("clock_gettime(CLOCK_REALTIME) succeeds", rc == 0);
    r.check(
        &format!("the device and the kernel agree to the second (delta {delta})"),
        delta.abs() <= 1,
    );

    // The fields against the libc's own breakdown of the *same* second, so a wrong
    // `wday` or `yday` is a failing line rather than something only a program that
    // prints a day name would notice. Derived from `rtc_epoch` rather than from the
    // live clock on purpose: no second can roll between the two answers.
    let mut want: libc::tm = unsafe { std::mem::zeroed() };
    let mut secs = rtc_epoch;
    // SAFETY: `want` is a correctly sized `struct tm` the call owns, and `secs`
    // outlives the call.
    let got = unsafe { libc::gmtime_r(&mut secs, &mut want) };
    r.check("gmtime_r returns the buffer it was given", !got.is_null());
    for (name, have, expected) in [
        ("tm_sec", fields[0], want.tm_sec),
        ("tm_min", fields[1], want.tm_min),
        ("tm_hour", fields[2], want.tm_hour),
        ("tm_mday", fields[3], want.tm_mday),
        ("tm_mon", fields[4], want.tm_mon),
        ("tm_year", fields[5], want.tm_year),
        ("tm_wday", fields[6], want.tm_wday),
        ("tm_yday", fields[7], want.tm_yday),
    ] {
        r.check(
            &format!("{name} matches gmtime_r's ({have} vs {expected})"),
            have == expected,
        );
    }

    // The write, with the value the device just reported: this exercises
    // `RTC_SET_TIME`'s whole path - the argument check, the field conversion, the
    // hardware write - without moving the machine's clock, which a sweep must not do
    // to the tests that follow it.
    let (rc, err) = rtc_ioctl(fd, RTC_SET_TIME, fields.as_mut_ptr().cast::<c_void>());
    r.check(
        &format!("RTC_SET_TIME with the value just read succeeds (got rc {rc}, errno {err})"),
        rc == 0,
    );

    // A calendar no month carries is the `EINVAL` Linux's own driver returns, and
    // nothing is written.
    let mut bad = fields;
    bad[4] = 12;
    let (rc, err) = rtc_ioctl(fd, RTC_SET_TIME, bad.as_mut_ptr().cast::<c_void>());
    r.check(
        &format!("RTC_SET_TIME with month 12 is EINVAL (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::EINVAL,
    );

    // The family's unimplemented half - this clock has no alarm.
    let (rc, err) = rtc_ioctl(fd, RTC_UNKNOWN, fields.as_mut_ptr().cast::<c_void>());
    r.check(
        &format!("an unimplemented 't' command is ENOTTY (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::ENOTTY,
    );

    // The family is keyed on the device number, not on the request: the same
    // request aimed at another descriptor is ENOTTY. This is the property that makes
    // "serve the RTC on a new machine" one node's business rather than every
    // `ioctl`'s.
    if let Ok(other) = open_ro("/dev/null") {
        let (rc, err) = rtc_ioctl(other, RTC_RD_TIME, fields.as_mut_ptr().cast::<c_void>());
        r.check(
            &format!("RTC_RD_TIME on /dev/null is ENOTTY (got rc {rc}, errno {err})"),
            rc != 0 && err == libc::ENOTTY,
        );
        close(other);
    } else {
        r.check("/dev/null opens, to try the request elsewhere", false);
    }

    close(fd);
    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
