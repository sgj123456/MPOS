//! t-localtime - the wall clock through `localtime(3)` and `asctime(3)`.
//!
//! Every log line a daemon writes starts with a timestamp, and on musl that
//! timestamp is `localtime_r` filling a `struct tm` which `asctime_r` (or the
//! program's own `snprintf`) then formats. Nothing in either suite had ever
//! called one: the clock was pinned (`t-*`'s `clock_gettime`, and the machine
//! layer's own wall-clock check on aarch64), but nothing had turned the number
//! into a calendar date.
//!
//! That is where this test comes from - a real Alpine boot in which busybox
//! `syslogd` died before it could log anything, at EL0, on
//! `strb wzr, [x0]` with `x0 = 0` inside `ld-musl-aarch64.so.1`'s `asctime_r`.
//! The instruction is musl's `a_crash()`, and the branch that reaches it is
//! the one taken when the formatted timestamp is longer than the 26 bytes
//! `asctime_r` promises - i.e. when a field of the caller's `struct tm` holds
//! something a date cannot express (`%3d` of a five-digit day, or `%d` of a
//! five-digit year, makes the string longer than 25 characters).
//!
//! So the test measures the two halves that have to agree. First it prints the
//! kernel's `struct timespec` and every field `localtime_r` derived from it,
//! because a wrong field is the whole of the failure and the log has to show
//! it. Then it ranges each field against what a calendar can hold. Only then
//! does it make the real `asctime_r` call, whose failure mode is a crash rather
//! than an errno - which is why the checks above it run first.
//!
//! It runs on both machines: the same `struct tm`, the same musl, and a wall
//! clock the kernel anchors to the machine's own RTC.

use std::ffi::CStr;

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

fn main() {
    println!("t-localtime");
    let mut r = Report { failed: false };

    // The number the kernel hands back, read directly so a wrong `timespec` is
    // visible before anything derives a calendar from it.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `ts` is a correctly sized `struct timespec` this call owns, and
    // the clock id is `libc`'s own constant.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    println!(
        "     clock_gettime(CLOCK_REALTIME) = {rc}, tv_sec={} tv_nsec={}",
        ts.tv_sec, ts.tv_nsec
    );
    r.check("clock_gettime(CLOCK_REALTIME) succeeds", rc == 0);
    r.check(
        &format!("tv_nsec is inside a second (got {})", ts.tv_nsec),
        (0..1_000_000_000).contains(&ts.tv_nsec),
    );

    // The wall anchor itself, isolated from the elapsed time: the kernel answers
    // `REALTIME` with `boot_epoch + uptime` and the monotonic clocks with
    // `uptime` alone, so the difference is the epoch the boot anchored to. On
    // Linux the same subtraction lands on the same number (the anchor there is
    // the RTC read at boot, and the two clocks differ by it plus any later
    // `settimeofday`) - so this is a claim both machines can make.
    let mut mono: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `mono` is a correctly sized `struct timespec` this call owns.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut mono) };
    let epoch = ts.tv_sec - mono.tv_sec;
    println!(
        "     CLOCK_REALTIME - CLOCK_MONOTONIC = {epoch} (the boot anchor; {} vs {})",
        ts.tv_sec, mono.tv_sec
    );
    r.check("CLOCK_MONOTONIC succeeds", rc == 0);
    r.check(
        &format!("the wall anchor is a live epoch (got {epoch})"),
        (1_000_000_000..4_300_000_000).contains(&epoch),
    );

    // `localtime_r` - the call every timestamp goes through.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a correctly sized `struct tm` this call owns, and `ts`'s
    // seconds field outlives the call.
    let p = unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) };
    r.check("localtime_r returns the buffer it was given", !p.is_null());
    if p.is_null() {
        println!("FAILED");
        std::process::exit(1);
    }
    let year = 1900i64 + tm.tm_year as i64;
    println!(
        "     localtime_r -> {year:04}-{:02}-{:02} {:02}:{:02}:{:02} wday={} yday={} isdst={} gmtoff={}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tm.tm_wday,
        tm.tm_yday,
        tm.tm_isdst,
        tm.tm_gmtoff,
    );

    // Each field against what a calendar can hold. These are the checks whose
    // failure is the crash below rather than a bad `ok:` line: `asctime_r`
    // cannot format what they describe, and musl's answer to that is to die.
    r.check(
        &format!("tm_mon is 0..11 (got {})", tm.tm_mon),
        (0..=11).contains(&tm.tm_mon),
    );
    r.check(
        &format!("tm_mday is 1..31 (got {})", tm.tm_mday),
        (1..=31).contains(&tm.tm_mday),
    );
    r.check(
        &format!("tm_hour is 0..23 (got {})", tm.tm_hour),
        (0..=23).contains(&tm.tm_hour),
    );
    r.check(
        &format!("tm_min is 0..59 (got {})", tm.tm_min),
        (0..=59).contains(&tm.tm_min),
    );
    r.check(
        &format!("tm_sec is 0..60 (got {})", tm.tm_sec),
        (0..=60).contains(&tm.tm_sec),
    );
    r.check(
        &format!("tm_wday is 0..6 (got {})", tm.tm_wday),
        (0..=6).contains(&tm.tm_wday),
    );
    r.check(
        &format!("tm_yday is 0..365 (got {})", tm.tm_yday),
        (0..=365).contains(&tm.tm_yday),
    );
    // The kernel's wall clock is anchored to the machine's own RTC, so the year
    // is a live one rather than a constant; the band is wide enough for any
    // machine that boots this kernel and narrow enough that a garbage field
    // cannot land inside it.
    r.check(
        &format!("the year is a live one (got {year})"),
        (2020..=2100).contains(&year),
    );

    // The length `asctime_r` has to fit, measured through the same format it
    // uses. `snprintf` answers the length that *would* have been written, which
    // is exactly the number musl compares against 25 - so a bad field is shown
    // here as a number before it is shown as a crash.
    let names = (
        ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ],
    );
    let (wday, mon) = (tm.tm_wday, tm.tm_mon);
    if (0..=6).contains(&wday) && (0..=11).contains(&mon) {
        // `%d` takes a `c_int`, so the year goes through the same narrowing
        // `asctime_r` does - `1900 + tm_year` in an `int`.
        let year_arg: libc::c_int = tm.tm_year.saturating_add(1900);
        // `c_char` is signed on x86_64 and unsigned on aarch64, so the buffer
        // is typed by the platform's own `c_char` rather than by `i8`.
        let mut probe = [0 as libc::c_char; 64];
        // SAFETY: the buffer is 64 bytes and the format writes at most the
        // fields above, none of which can approach it while they are in range.
        let n = unsafe {
            libc::snprintf(
                probe.as_mut_ptr(),
                probe.len(),
                c"%.3s %.3s%3d %.2d:%.2d:%.2d %d\n".as_ptr(),
                names.0[wday as usize].as_ptr() as *const libc::c_char,
                names.1[mon as usize].as_ptr() as *const libc::c_char,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                year_arg,
            )
        };
        println!("     asctime-shaped snprintf reports {n} characters (must be <= 25)");
        r.check(
            &format!("the timestamp fits asctime's 26-byte buffer (got {n})"),
            n <= 25,
        );
    } else {
        r.check(
            &format!("wday/mon are in range to format (wday={wday} mon={mon})"),
            false,
        );
    }

    // And the real call. On a kernel that fills the fields correctly this is a
    // 25-byte string; on one that does not, musl's overflow branch reaches
    // `a_crash()` and the process dies here, which the harness reports as a
    // failure of this program.
    let mut buf = [0 as libc::c_char; 32];
    // SAFETY: `tm` was filled by `localtime_r` above and `buf` is the 26 bytes
    // musl's `asctime_r` contract requires (plus slack).
    let p = unsafe { libc::asctime_r(&tm, buf.as_mut_ptr()) };
    r.check(
        "asctime_r returns the buffer (musl crashes rather than refusing)",
        !p.is_null(),
    );
    if !p.is_null() {
        // SAFETY: `asctime_r` NUL-terminates what it wrote into `buf`.
        let text = unsafe { CStr::from_ptr(buf.as_ptr()) };
        println!("     asctime_r -> {:?}", text);
        r.check(
            &format!(
                "the timestamp is the 25 characters asctime promises (got {})",
                text.to_bytes().len()
            ),
            text.to_bytes().len() == 25,
        );
    }

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
