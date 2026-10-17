//! t-ioctl - one test for ioctl, the terminal-query door musl and every line
//! editor walk through (`ioctl(0, TIOCGWINSZ, &ws)` in the strace), plus the
//! two device families the boot's own services ask: the entropy pool queries
//! seedrng sends to `/dev/urandom`, and the clock reads and writes hwclock
//! sends to `/dev/rtc0`. On both machines.
//!
//! Pinned here on fd 0, the console: TIOCGWINSZ writes the window the kernel
//! serves, zeroing the pixel fields too. The answer is the *screen's* grid in
//! cells whenever the machine has a screen - both test boots carry a
//! virtio-gpu, so that is what it answers - and the 25x80 fallback only when
//! there is no screen at all. Checking it against `FBIOGET_VSCREENINFO`/`8x16`
//! rather than against a number of its own is what keeps it that rule: a
//! full-screen program lays itself out in the window it was told, so a console
//! whose answer changed mid-run would be the corner-of-the-screen bug this
//! rule exists to prevent. The same rule is pinned on the host, where a screen
//! can be made (`vfs::tty::tests::a_console_reports_its_screens_window_and_
//! others_do_not`). TCGETS answers 0 and describes a console - a line at a
//! time, echoed, interruptible - and TCSETS changes what it describes, so a
//! shell that asks for raw mode gets one; a request the kernel has no handler
//! for answers ENOTTY. The bad-fd case is deliberately not exercised: this
//! kernel answers on the request alone, before any descriptor check, so
//! man-faithful EBADF ordering is not pinned here.
//!
//! # The doors: the record shape is why the raw call stays
//!
//! `rustix` binds the terminal half - `tcgetwinsize`, `tcgetattr`,
//! `tcsetattr` - and every one of them owns or rewrites the buffer: the
//! getter hands back a parsed `Winsize`/`Termios`, the setter takes one, and
//! the backend issues `TCGETS2`/`TCSETS2` first with a `TCGETS` fallback
//! (the §15.23 measurement). This program's claims are about the *wire*: the
//! pixel fields seeded to 0xFFFF and found zeroed, the `c_lflag` word read as
//! the kernel stored it, `c_cc[VEOF]` addressed by the uapi's index. So the
//! terminal trio rides `libc::syscall(SYS_ioctl, ..)` like every family here.
//! Clause (c) again, and the same reason `t-tty-query` gives for the raw
//! window claim beside rustix's typed one. The numbers do come from the
//! maintained tables where they exist: `libc::TCGETS`, `TCSETS`,
//! `TIOCGWINSZ`, `FIONREAD` (the arch-generic block, identical on both
//! machines), `libc::termios`'s own `c_lflag`/`c_cc` fields, `libc::winsize`,
//! and `libc::SYS_ioctl` per machine (16 / 29 - the number the no-std literal
//! could not carry). The `'R'`, `'K'`, `'t'` families stay spelled here:
//! libc binds none of them for linux (measured), exactly as `t-rtc` spells
//! its `'p'` set.
//!
//! One correction moves with the program: the old file's `RNDADDENTROPY`
//! label was on `0x4008_5202`, which is `RNDGETPOOL` - the dispatch-on-the-
//! low-byte bug that broke seedrng - and the credit's real word is
//! `0x4008_5203`. Both spellings are pinned below so the bug cannot come
//! back unnoticed.
//!
//! # The host branch
//!
//! A hosted run measures three things this sandbox's Linux does not give an
//! unprivileged caller: `/dev/rtc0` is `crw-rw---- root:clock` (EACCES at the
//! open, measured), the mutating `RND*` requests want `CAP_SYS_ADMIN` the
//! container does not grant (EPERM), and the capability dance's
//! `setresuid(-1, 65534, -1)` answers EINVAL on musl hosts (`t-posix`'s
//! measurement). Those sections print their skip verdict under
//! `MPOS_ON_HOST`; everything else - the console on the sweep's stdin, the
//! refusals, the count read - is claimed on its own answer. The pin remains
//! the two sweeps.

use std::ffi::c_void;

use mpos_user_tests::{Report, host_run};
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, SeekFrom, openat, seek, unlinkat};
use rustix::io::write;
use rustix::process::{geteuid, getuid};

// The request words. The terminal trio and FIONREAD come from libc's
// arch-generic table - identical numbers on both machines, which is the point
// of taking them from a maintained source rather than writing them down.
const TIOCGWINSZ: u32 = libc::TIOCGWINSZ as u32;
const TCGETS: u32 = libc::TCGETS as u32;
const TCSETS: u32 = libc::TCSETS as u32;
const FIONREAD: u32 = libc::FIONREAD as u32;

// The `'R'` family spelled out in full, because the dispatch is on the whole
// word and not on the command byte: the number this test used to call
// `RNDADDENTROPY` - 0x4008_5202 - is `RNDGETPOOL`, a request the kernel
// stopped answering, and the credit seedrng actually sends is 0x4008_5203.
// Both spellings are pinned below so a dispatch on the low byte alone cannot
// creep back in.
const RNDGETENTCNT: u32 = 0x8004_5200; // _IOR('R', 0x00, int)
const RNDADDTOENTCNT: u32 = 0x4004_5201; // _IOW('R', 0x01, int)
const RNDGETPOOL: u32 = 0x8008_5202; // _IOR('R', 0x02, int [2]): removed
const RNDADDENTROPY: u32 = 0x4008_5203; // _IOW('R', 0x03, int [2])
const RNDZAPENTCNT: u32 = 0x0000_5204; // _IO('R', 0x04): no arg, no direction
const RNDCLEARPOOL: u32 = 0x0000_5206; // _IO('R', 0x06)
const RNDRESEEDCRNG: u32 = 0x0000_5207; // _IO('R', 0x07)
/// `_IOW('R', 0x03, int)`: the credit's command with the header's `int [2]`
/// narrowed to an `int`. Not a credit, on either machine.
const CREDIT_WRONG_SIZE: u32 = 0x4004_5203;
/// `_IOWR('R', 0x03, int [2])`: the same word with both direction bits.
const CREDIT_WRONG_DIR: u32 = 0xC008_5203;
/// `_IO('R', 0x09)`: a number nobody has taken.
const RND_UNKNOWN: u32 = 0x0000_5209;
const RTC_RD_TIME: u32 = 0x8024_7409;
const RTC_SET_TIME: u32 = 0x4024_740A;
const FBIOGET_VSCREENINFO: u32 = 0x4600;
/// KDGKBTYPE: a `_IOR` over the console's 'K' family, one byte out - the
/// keyboard model the console reports. `0x4B33` as `/usr/include/linux/kd.h`
/// spells it (dir read, size 1, 'K' at bits 8..15, command 0x33).
const KDGKBTYPE: u32 = 0x4B33;
/// KDGETLED (0x31) is a READ of live LED state this kernel has no controller
/// to source, and KDSETLED (0x32) a WRITE to hardware that is not present.
/// Both used to fall through to `_ => 0`: the getter left the caller's int as
/// stack garbage while claiming success (the KDGKBTYPE-style lie), the setter
/// claimed an effect nothing performed. They are now EOPNOTSUPP - a real
/// console ioctl this device declines. A number outside the 'K' command block
/// (0x4B80, nr 0x80) is not a console ioctl at all and answers ENOTTY. All
/// counterfactual against the old blanket 0. Serving these per-device is the
/// Phase 11 B2 move.
const KDGETLED: u32 = 0x4B31;
const KDSETLED: u32 = 0x4B32;
const KD_UNKNOWN: u32 = 0x4B80;

/// The cell the kernel's console font draws: the framebuffer console *is*
/// `/dev/tty0`'s emulator and its grid *is* that terminal's window, so the
/// window in cells is the screen in pixels divided by this. `vfs/term` embeds
/// the 8x16 `vga8x16` font it draws with, and `TIOCGWINSZ` on `/dev/ttyN`
/// answers `Grid::rows/cols` - the same numbers, which is why they are checked
/// against the screen rather than against a number a headless machine picked.
const CELL_W: i32 = 8;
const CELL_H: i32 = 16;

/// The `c_lflag` bits and `c_cc` indices, from `linux/termios-struct.h`'s
/// values via the same arch-generic table the ioctls come from. Spelled here
/// because `libc` names `ICANON`/`ISIG`/`ECHO` but the *indices* VINTR/VEOF
/// are the ones the kernel's c_cc array uses on both machines.
const ICANON: libc::tcflag_t = 0x0000_0002;
const ISIG: libc::tcflag_t = 0x0000_0001;
const ECHO: libc::tcflag_t = 0x0000_0008;
const VINTR: usize = 0;
const VEOF: usize = 4;

/// One ioctl, answering `(rc, errno)` - `t-rtc`'s door for the same reason:
/// through `libc::syscall` rather than `libc::ioctl`, because `ioctl`'s
/// request parameter is `c_int` in the musl bindings and `c_ulong` in the gnu
/// ones, and a request is neither: it is an `unsigned long` word the kernel
/// decodes. The number comes from `libc::SYS_ioctl`, the maintained
/// per-machine table (16 here, 29 on the second).
fn ioctl(fd: libc::c_int, request: u32, arg: *mut c_void) -> (i64, i32) {
    // SAFETY: every caller passes a buffer it owns (or a scalar argument the
    // named request expects) and a request whose argument shape is the one
    // spelled above.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd as libc::c_long,
            request as libc::c_ulong as libc::c_long,
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

/// The same, on any descriptor-shaped thing.
fn ioctl_fd<F: AsFd>(fd: F, request: u32, arg: *mut c_void) -> (i64, i32) {
    ioctl(fd.as_fd().as_raw_fd(), request, arg)
}

/// `setresuid(r, e, s)` raw: `-1` keeps an id, and the typed wrapper takes
/// `uid_t` and cannot name it (`t-ns-uts`'s door). Answers the errno, or 0.
fn setresuid(r: i32, e: i32, s: i32) -> i32 {
    // SAFETY: three scalar arguments; -1 means "keep this one" by the ABI.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_setresuid,
            r as libc::c_long,
            e as libc::c_long,
            s as libc::c_long,
        )
    };
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

fn err_is(r: &mut Report, name: &str, got: i32, want: i32) {
    r.check(
        &format!("{name} (got errno {got}, want {want})"),
        got == want,
    );
}

fn main() {
    println!("t-ioctl");
    let mut r = Report::new();

    // ---- The console, on the descriptor the runner gave us ----
    //
    // The sweep's stdin *is* the console; a host run's is whatever launched the
    // program - this sandbox's shell answers ENOTTY for every terminal request
    // (measured), so the whole console section is named and not made there.
    // `t-sa-restart`'s pty rule: the payload mints no fixture for what the
    // runner already owns, and a host cannot pin a kernel it is not running.
    let console_is_tty = ioctl(0, TCGETS, core::ptr::null_mut()).1 != libc::ENOTTY;
    if !console_is_tty && host_run() {
        println!(
            "note: stdin is not a terminal here; the console section is named and not made on a host run"
        );
    }
    if console_is_tty {
        let mut ws = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0xFFFF,
            ws_ypixel: 0xFFFF,
        };
        let (rc, _) = ioctl(0, TIOCGWINSZ, &raw mut ws as *mut c_void);
        r.check("TIOCGWINSZ on the console succeeds", rc == 0);
        // The console's window, checked against the screen it draws on. A machine
        // with a display has a framebuffer console of that display's size whatever
        // its kernel log rides on; a machine with no display at all answers the
        // 25x80 fallback. What is *not* pinned is a boot-shaped answer: the same
        // machine has to give the same window all run long, which is what a
        // console whose size followed the DRM master got wrong.
        let fb = openat(CWD, "/dev/fb0", OFlags::RDONLY, Mode::empty());
        if let Ok(fb) = fb {
            let mut var = [0u8; 160];
            let (rc, _) = ioctl_fd(&fb, FBIOGET_VSCREENINFO, var.as_mut_ptr() as *mut c_void);
            r.check("FBIOGET_VSCREENINFO sizes the screen", rc == 0);
            let xres = u32::from_ne_bytes([var[0], var[1], var[2], var[3]]) as i32;
            let yres = u32::from_ne_bytes([var[4], var[5], var[6], var[7]]) as i32;
            // The grid is the screen counted in cells, which is exactly what the
            // render task computes: columns floor-divide (`Grid` caps rows, not
            // columns) and rows are capped at `vfs/term::MAX_ROWS`, 64 - a height no
            // test screen reaches.
            r.check(
                "the console's rows are the screen's",
                ws.ws_row as i32 == (yres / CELL_H).min(64),
            );
            r.check(
                "the console's columns are the screen's",
                ws.ws_col as i32 == xres / CELL_W,
            );
        } else {
            r.check(
                "with no screen the console reports 25 rows",
                ws.ws_row == 25,
            );
            r.check(
                "with no screen the console reports 80 columns",
                ws.ws_col == 80,
            );
        }
        r.check(
            "the pixel fields are zeroed, not left to the caller",
            ws.ws_xpixel == 0 && ws.ws_ypixel == 0,
        );

        let mut tio = libc::termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0xFF; libc::NCCS],
            #[cfg(target_env = "gnu")]
            c_ispeed: 0,
            #[cfg(target_env = "gnu")]
            c_ospeed: 0,
            #[cfg(not(target_env = "gnu"))]
            __c_ispeed: 0,
            #[cfg(not(target_env = "gnu"))]
            __c_ospeed: 0,
        };
        let (rc, _) = ioctl(0, TCGETS, &raw mut tio as *mut c_void);
        r.check("TCGETS on the console succeeds", rc == 0);
        // A console: a line at a time, and shown as it is typed.
        r.check(
            "the console is in canonical mode",
            tio.c_lflag & ICANON != 0,
        );
        r.check("the console echoes what is typed", tio.c_lflag & ECHO != 0);
        r.check(
            "the console turns the interrupt key into a signal",
            tio.c_lflag & ISIG != 0,
        );
        r.check("^C is the interrupt", tio.c_cc[VINTR] == 3);
        r.check("^D is end of input", tio.c_cc[VEOF] == 4);

        // TCSETS: what a shell does when it wants to do its own editing. Ask for
        // raw, read it back, and put the console back the way it was found - a
        // test that leaves the terminal in raw mode leaves the next line of the
        // script unreadable.
        let saved = tio;
        let mut raw = saved;
        raw.c_lflag &= !(ICANON | ECHO);
        let (rc, _) = ioctl(0, TCSETS, &raw mut raw as *mut c_void);
        r.check("TCSETS on the console succeeds", rc == 0);

        let (rc, _) = ioctl(0, TCGETS, &raw mut tio as *mut c_void);
        r.check("TCGETS after TCSETS succeeds", rc == 0);
        r.check(
            "the settings came back as they were set",
            tio.c_lflag == raw.c_lflag,
        );
        r.check("canonical mode is off, as asked", tio.c_lflag & ICANON == 0);

        let (rc, _) = ioctl(0, TCSETS, &raw const saved as *mut c_void);
        r.check("restoring the console succeeds", rc == 0);
        let (rc, _) = ioctl(0, TCGETS, &raw mut tio as *mut c_void);
        r.check("TCGETS after restoring succeeds", rc == 0);
        r.check(
            "the console is canonical again",
            tio.c_lflag == saved.c_lflag,
        );

        // A file is not a terminal, and says so. The release file belongs to a
        // package the test image does not install, so the fixture is the test's
        // own file under /tmp - batch 3's rule, and both payloads carry /tmp.
        let probe = "/tmp/t-ioctl-probe";
        let _ = rustix::fs::unlink(probe);
        let fd = openat(
            CWD,
            probe,
            OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        );
        match fd {
            Ok(fd) => {
                let (rc, errno) = ioctl_fd(&fd, TCGETS, &raw mut tio as *mut c_void);
                err_is(
                    &mut r,
                    "TCGETS on a file answers ENOTTY",
                    errno,
                    libc::ENOTTY,
                );
                let _ = rc;
                drop(fd);
                let _ = rustix::fs::unlink(probe);
            }
            Err(e) => r.check(&format!("the non-terminal probe opens ({e:?})"), false),
        }

        let (rc, errno) = ioctl(0, 0xDEAD_BEEF, core::ptr::null_mut());
        err_is(
            &mut r,
            "an unknown request answers ENOTTY",
            errno,
            libc::ENOTTY,
        );
        let _ = rc;

        let (rc, _) = ioctl(1, TIOCGWINSZ, &raw mut ws as *mut c_void);
        r.check("TIOCGWINSZ answers stdout the same console shape", rc == 0);

        // KDGKBTYPE on the console: a read that must *fill* the caller's byte, not
        // just answer 0. Linux reports the constant KB_101 whatever is plugged in
        // (kd.h: "this is what we always answer"). The buffer starts at 0xFF, so
        // this also pins the false-success that used to live here: the request fell
        // through to the console's setter accept, returning 0 with the byte left
        // uninitialised - a keyboard model the reader would then act on.
        let mut kbtype: u8 = 0xFF;
        let (rc, _) = ioctl(0, KDGKBTYPE, &raw mut kbtype as *mut c_void);
        r.check("KDGKBTYPE on the console succeeds", rc == 0);
        r.check(
            "KDGKBTYPE fills the byte with KB_101, not a stale stack value",
            kbtype == 2,
        );

        // ---- The rest of the console `K` family is refused, not faked ----
        let (_, errno) = ioctl(0, KDGETLED, &raw mut kbtype as *mut c_void);
        err_is(
            &mut r,
            "KDGETLED is refused EOPNOTSUPP, not 0 with a stale buffer",
            errno,
            libc::EOPNOTSUPP,
        );
        let (_, errno) = ioctl(0, KDSETLED, 3 as *mut c_void);
        err_is(
            &mut r,
            "KDSETLED is refused EOPNOTSUPP, not a fake success",
            errno,
            libc::EOPNOTSUPP,
        );
        let (_, errno) = ioctl(0, KD_UNKNOWN, core::ptr::null_mut());
        err_is(
            &mut r,
            "a 'K'-typed number outside the command block is ENOTTY",
            errno,
            libc::ENOTTY,
        );
    }

    // ---- The entropy pool, as seedrng asks of it ----
    let urandom = openat(CWD, "/dev/urandom", OFlags::RDONLY, Mode::empty());
    let urandom: OwnedFd = match urandom {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("/dev/urandom opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("/dev/urandom opens", true);

    let mut word: i32 = -1;
    let (rc, _) = ioctl_fd(&urandom, RNDGETENTCNT, &raw mut word as *mut c_void);
    r.check("RNDGETENTCNT succeeds", rc == 0);
    // The count is this kernel's design (a generator that always answers full,
    // §8.0) where a host Linux reports its own pool's own level; the *shape* - a read
    // that fills the int and answers 0 - is the claim on both.
    if host_run() {
        println!(
            "note: the host's pool count is the host's own ({word} bits); the 4096 pin is this kernel's"
        );
        r.check(
            "and the count reads as an int the kernel wrote",
            rc == 0 && word >= 0,
        );
    } else {
        r.check("the pool answers full (4096 bits)", word == 4096);
    }

    // FIONREAD on an entropy device is `EINVAL` on Linux - root and
    // unprivileged, `O_RDONLY` and `O_WRONLY`, and *before* the argument is
    // looked at (a NULL pointer is still `EINVAL`, not `EFAULT`). This kernel
    // used to answer 512 here, and the seedrng reason written down for it was
    // false: the binary in this image sends exactly one ioctl, the
    // `RNDADDENTROPY` credit below, and takes its seed length from
    // `/proc/sys/kernel/random/poolsize`. The old pin asserted the 512, so it
    // had encoded the divergence; it is corrected with the measurement, which
    // is rule 1's carve-out, not weakened to keep a passing shape.
    let (_, errno) = ioctl_fd(&urandom, FIONREAD, &raw mut word as *mut c_void);
    err_is(
        &mut r,
        "FIONREAD on the pool is EINVAL",
        errno,
        libc::EINVAL,
    );
    let (_, errno) = ioctl_fd(&urandom, FIONREAD, core::ptr::null_mut());
    err_is(
        &mut r,
        "FIONREAD on the pool is EINVAL even with a NULL argument",
        errno,
        libc::EINVAL,
    );

    // ---- FIONREAD on the descriptors that have a queue-length story ----
    //
    // Linux answers this request per file type, measured on the host first
    // (`.workbuddy/probe_fionread.c`, Linux 7.2.9) before any of it was made:
    // a regular file reports `i_size - f_pos` - 10 at the start of a ten-byte
    // file, 6 after a four-byte read, 0 at end of file, and **-10** at offset
    // 20, because the wide subtraction is truncated to `int` rather than
    // clamped - and a pipe reports its queued bytes on **both** ends. Both used
    // to be `ENOTTY` here, which the `docs/uapi/` §5.2 entry named as the
    // divergence this section closes. The same probe measured what stays named
    // rather than claimed: a socket (a `socketpair` end or a network socket)
    // reports its queued bytes and a tty (pty included) reports 0 on Linux,
    // neither of which this kernel answers yet; a directory reports `ENOTTY`
    // there too, which the last claim pins.
    let tmp = "/tmp/t-ioctl-fionread";
    match openat(
        CWD,
        tmp,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from(0o600),
    ) {
        Ok(f) => {
            write(&f, b"0123456789").expect("write the ten-byte fixture");
            let mut n: i32 = -1;
            seek(&f, SeekFrom::Start(0)).expect("rewind the fixture");
            let (rc, _) = ioctl_fd(&f, FIONREAD, &raw mut n as *mut c_void);
            r.check(
                "FIONREAD on a regular file at offset 0 reports 10",
                rc == 0 && n == 10,
            );
            let mut b = [0u8; 4];
            let _ = rustix::io::read(&f, &mut b);
            let (rc, _) = ioctl_fd(&f, FIONREAD, &raw mut n as *mut c_void);
            r.check(
                "after a four-byte read it reports the remaining 6",
                rc == 0 && n == 6,
            );
            seek(&f, SeekFrom::End(0)).expect("seek to end of file");
            let (rc, _) = ioctl_fd(&f, FIONREAD, &raw mut n as *mut c_void);
            r.check("at end of file it reports 0", rc == 0 && n == 0);
            // The one shape a saturating subtraction would get wrong: past the
            // end, Linux writes a negative count rather than stopping at zero.
            seek(&f, SeekFrom::Start(20)).expect("seek past the end");
            let (rc, _) = ioctl_fd(&f, FIONREAD, &raw mut n as *mut c_void);
            r.check(
                "past the end the subtraction is truncated, not clamped (-10)",
                rc == 0 && n == -10,
            );
            let (_, errno) = ioctl_fd(&f, FIONREAD, core::ptr::null_mut());
            err_is(
                &mut r,
                "a NULL destination on a regular file is EFAULT, not ENOTTY",
                errno,
                libc::EFAULT,
            );
            let _ = unlinkat(CWD, tmp, AtFlags::empty());
        }
        Err(e) => r.check(&format!("the FIONREAD fixture opens ({e:?})"), false),
    }

    // A pipe reports its queued bytes, and it is the *same* buffer both ends
    // count - the write end included.
    match rustix::pipe::pipe() {
        Ok((rd, wr)) => {
            let mut n: i32 = -1;
            let (rc, _) = ioctl_fd(&rd, FIONREAD, &raw mut n as *mut c_void);
            r.check("FIONREAD on an empty pipe reports 0", rc == 0 && n == 0);
            write(&wr, b"abcde").expect("write the five bytes");
            let (rc, _) = ioctl_fd(&rd, FIONREAD, &raw mut n as *mut c_void);
            r.check(
                "the pipe's read end reports the five queued bytes",
                rc == 0 && n == 5,
            );
            let (rc, _) = ioctl_fd(&wr, FIONREAD, &raw mut n as *mut c_void);
            r.check("and its write end reports them too", rc == 0 && n == 5);
        }
        Err(e) => r.check(&format!("a pipe is made ({e:?})"), false),
    }

    // A directory has no queue-length story on either kernel.
    if let Ok(dir) = openat(
        CWD,
        "/tmp",
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        let (_, errno) = ioctl_fd(&dir, FIONREAD, &raw mut word as *mut c_void);
        err_is(
            &mut r,
            "FIONREAD on a directory is ENOTTY",
            errno,
            libc::ENOTTY,
        );
    }

    // The mutating family and the capability dance need CAP_SYS_ADMIN or root;
    // a hosted unprivileged run meets EPERM at every door (measured), so the
    // section is named and not made there. On the payload the sweep runs as
    // root, which holds the cap, and all of it is pinned.
    let privileged = getuid().as_raw() == 0;
    if !privileged && host_run() {
        println!(
            "note: this host caller holds no CAP_SYS_ADMIN; the mutating RND family and the capability dance are named and not made here"
        );
    }
    if privileged {
        // ---- RNDADDENTROPY: the credit OpenRC's seedrng sends on every boot ----
        //
        // Every request in this block is gated on CAP_SYS_ADMIN by a current
        // kernel *before* any argument is read (measured on this host: each shape
        // answers EPERM for an unprivileged caller, even the ones whose argument
        // would be EINVAL), so a hosted run without the capability names the whole
        // section and claims nothing - `t-posix`'s rule. Under the sweep the
        // payload runs as root, which holds it.
        //
        // This is the request whose refusal was the boot's
        // "seedrng: Unable to seed: Not supported". The family used to be
        // dispatched on its command byte, and nr 3 is the credit while nr 4 is the
        // drain - so the credit was answered `EOPNOTSUPP`, and the word this test
        // called `RNDADDENTROPY` (0x4008_5202) was `RNDGETPOOL` all along. Every
        // shape below is the answer measured on real Linux 7.2.7 as root.
        //
        // A `{64 bits, 0 seed bytes}` header: the count is not negative, the
        // declared seed is empty, and the pool this generator has is already full,
        // so the credit is a no-op that the read above agrees with.
        let mut poolinfo = [64i32, 0i32];
        let (_, errno) = ioctl_fd(
            &urandom,
            RNDADDENTROPY,
            poolinfo.as_mut_ptr() as *mut c_void,
        );
        err_is(&mut r, "RNDADDENTROPY {64, 0} is accepted", errno, 0);

        // The shape seedrng sends: a header *and* the seed bytes it declares, so
        // the kernel has to prove the seed is reachable before it believes the
        // length. `rndpool` holds 4096 bits' worth of header plus eight seed bytes.
        let mut rndpool = [0i32; 16];
        rndpool[0] = 4096;
        rndpool[1] = 8;
        let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, rndpool.as_mut_ptr() as *mut c_void);
        err_is(
            &mut r,
            "RNDADDENTROPY with the seed bytes it declares is accepted",
            errno,
            0,
        );
        // An opening mode is not part of this family's gate: measured on Linux,
        // the credit is accepted on a descriptor open only for writing and the
        // count is read back on one open only for reading. That is worth a pin
        // here rather than an assumption, because this kernel *does* enforce
        // `O_RDONLY`/`O_WRONLY` on its read and write doors - an ioctl that
        // borrowed that predicate would refuse a caller Linux accepts, and the
        // pool fd above is itself `O_RDONLY`.
        let wonly = openat(CWD, "/dev/urandom", OFlags::WRONLY, Mode::empty());
        match wonly {
            Ok(wonly) => {
                r.check("/dev/urandom opens O_WRONLY", true);
                let (_, errno) =
                    ioctl_fd(&wonly, RNDADDENTROPY, poolinfo.as_mut_ptr() as *mut c_void);
                err_is(
                    &mut r,
                    "the credit does not care that the fd is write-only",
                    errno,
                    0,
                );
                let (rc, _) = ioctl_fd(&wonly, RNDGETENTCNT, &raw mut word as *mut c_void);
                r.check("and neither does the count read", rc == 0);
            }
            Err(e) => r.check(&format!("/dev/urandom opens O_WRONLY ({e:?})"), false),
        }

        // A negative count is EINVAL, and the count is looked at *before* the
        // length is believed: measured, `{-1, -1}` and `{-1, 65536}` are both
        // EINVAL although the second length is unusable too.
        rndpool[0] = -1;
        rndpool[1] = 65536;
        let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, rndpool.as_mut_ptr() as *mut c_void);
        err_is(
            &mut r,
            "a negative credit is EINVAL even beside a bad length",
            errno,
            libc::EINVAL,
        );

        // A length that is negative, or that reaches past any mapping, is the
        // `EFAULT` Linux answers where its copy faults. 16 MiB is the honest shape
        // of that pin: measured on Linux, a declared 64 KiB *succeeded*, because
        // the heap behind an 8-byte header is mapped and the kernel only reads what
        // the header says is there. Here the same length also crosses this
        // kernel's `MAX_IO` bound, which is the worst-case answer the §4 rules ask
        // for rather than a copy of the caller's choosing.
        rndpool[0] = 8;
        rndpool[1] = -1;
        let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, rndpool.as_mut_ptr() as *mut c_void);
        err_is(
            &mut r,
            "a negative seed length is EFAULT",
            errno,
            libc::EFAULT,
        );
        rndpool[1] = 16 * 1024 * 1024;
        let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, rndpool.as_mut_ptr() as *mut c_void);
        err_is(
            &mut r,
            "a seed length past any mapping is EFAULT",
            errno,
            libc::EFAULT,
        );
        let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, core::ptr::null_mut());
        err_is(
            &mut r,
            "RNDADDENTROPY with no header at all is EFAULT",
            errno,
            libc::EFAULT,
        );

        // ---- RNDADDTOENTCNT: the same gate, one int, no seed ----
        word = 8;
        let (_, errno) = ioctl_fd(&urandom, RNDADDTOENTCNT, &raw mut word as *mut c_void);
        err_is(&mut r, "RNDADDTOENTCNT 8 is accepted", errno, 0);
        word = -1;
        let (_, errno) = ioctl_fd(&urandom, RNDADDTOENTCNT, &raw mut word as *mut c_void);
        err_is(
            &mut r,
            "RNDADDTOENTCNT of a negative credit is EINVAL",
            errno,
            libc::EINVAL,
        );
        let (_, errno) = ioctl_fd(&urandom, RNDADDTOENTCNT, core::ptr::null_mut());
        err_is(
            &mut r,
            "RNDADDTOENTCNT with no int is EFAULT",
            errno,
            libc::EFAULT,
        );

        // ---- The words that are not the requests they look like ----
        //
        // Same command byte, wrong size or wrong direction; and two `'R'` numbers
        // the kernel does not answer - `RNDGETPOOL`, which the header still names,
        // and an untaken one. All four are `EINVAL` on Linux, for root and for
        // everyone else: the command is refused before the capability is asked, so
        // a dispatch keyed on `request & 0xff` - the shape that broke seedrng -
        // cannot come back unnoticed.
        for (request, label) in [
            (
                CREDIT_WRONG_SIZE,
                "the credit's command with the wrong size",
            ),
            (
                CREDIT_WRONG_DIR,
                "the credit's command with the wrong direction",
            ),
            (RNDGETPOOL, "RNDGETPOOL, which the kernel stopped answering"),
            (RND_UNKNOWN, "an untaken 'R' number"),
        ] {
            let (_, errno) = ioctl_fd(&urandom, request, rndpool.as_mut_ptr() as *mut c_void);
            err_is(&mut r, label, errno, libc::EINVAL);
        }

        // ---- The two drains this machine cannot perform, and the re-key it can ----
        //
        // `RNDZAPENTCNT`/`RNDCLEARPOOL` would drain a count this generator does not
        // hold, so they stay refused `EOPNOTSUPP` rather than answered with the `0`
        // Linux gives - a `0` would claim a change the read above contradicts one
        // moment later (§4.4: refuse what you cannot do, do not fake it).
        //
        // `RNDRESEEDCRNG` is not in that class any more. This generator has a fresh
        // hardware seed and the stream it has produced, and re-keying from both is
        // the same promise Linux's re-key makes; Linux answers `0` for root
        // (measured, both devices), and so does the kernel now. The counterfactual
        // is the old `EOPNOTSUPP` this line used to assert.
        for (request, label) in [
            (
                RNDZAPENTCNT,
                "RNDZAPENTCNT is refused EOPNOTSUPP, not a fake 0",
            ),
            (RNDCLEARPOOL, "RNDCLEARPOOL is refused EOPNOTSUPP too"),
        ] {
            let (_, errno) = ioctl_fd(&urandom, request, core::ptr::null_mut());
            err_is(&mut r, label, errno, libc::EOPNOTSUPP);
        }
        let (rc, _) = ioctl_fd(&urandom, RNDRESEEDCRNG, core::ptr::null_mut());
        r.check("RNDRESEEDCRNG re-keys the generator and answers 0", rc == 0);
        // A re-key is not a drain: the count the read reports does not move across
        // one, on Linux or here.
        let (rc, _) = ioctl_fd(&urandom, RNDGETENTCNT, &raw mut word as *mut c_void);
        r.check("the count still reads after a re-key", rc == 0);
        r.check(
            "and it is still 4096 bits - a re-key drains nothing",
            word == 4096,
        );

        // The capability gate, measured on Linux: every mutating RND ioctl is EPERM
        // without CAP_SYS_ADMIN, and EPERM comes *before* anything is read from the
        // caller's buffer. Lower the effective user only - ruid and the saved id
        // stay 0, so the privilege is reclaimable - and a header that answered
        // EINVAL for root a moment ago now answers EPERM. That is the counterfactual
        // for the order, as the old handler's `0` for everyone is the
        // counterfactual for the gate. On a host this dance is named and not made:
        // musl's answer to `setresuid(-1, 65534, -1)` is EINVAL (`t-posix`'s
        // measurement), and the container grants no CAP_SYS_ADMIN anyway, so the
        // gate keeps pinning and the host run prints the skip.
        let lowered = setresuid(-1, 65534, -1);
        if lowered != 0 && host_run() {
            println!(
                "note: the host refuses the effective-id drop (errno {lowered}); the capability section is named and not made here"
            );
        } else {
            err_is(
                &mut r,
                "setresuid lowers euid (ruid/saved stay 0)",
                lowered,
                0,
            );
            rndpool[0] = -1;
            rndpool[1] = -1;
            let (_, errno) = ioctl_fd(&urandom, RNDADDENTROPY, rndpool.as_mut_ptr() as *mut c_void);
            err_is(
                &mut r,
                "RNDADDENTROPY without CAP_SYS_ADMIN is EPERM, before its EINVAL",
                errno,
                libc::EPERM,
            );
            for (request, label) in [
                (
                    RNDADDTOENTCNT,
                    "RNDADDTOENTCNT without the capability is EPERM",
                ),
                (
                    RNDZAPENTCNT,
                    "and RNDZAPENTCNT without the capability is EPERM",
                ),
                (
                    RNDCLEARPOOL,
                    "and RNDCLEARPOOL without the capability is EPERM",
                ),
                (
                    RNDRESEEDCRNG,
                    "and RNDRESEEDCRNG without the capability is EPERM, not ENOTTY",
                ),
            ] {
                let (_, errno) = ioctl_fd(&urandom, request, core::ptr::null_mut());
                err_is(&mut r, label, errno, libc::EPERM);
            }
            // The command is refused before the capability is asked, so an
            // unrecognised `'R'` word is still `EINVAL` for this caller - it was
            // never a request the device could grant or withhold.
            for (request, label) in [
                (
                    RNDGETPOOL,
                    "RNDGETPOOL is still EINVAL without the capability",
                ),
                (RND_UNKNOWN, "an untaken 'R' number is still EINVAL too"),
            ] {
                let (_, errno) = ioctl_fd(&urandom, request, core::ptr::null_mut());
                err_is(&mut r, label, errno, libc::EINVAL);
            }
            // Reading the pool needs no privilege, whatever the write side costs.
            let (rc, _) = ioctl_fd(&urandom, RNDGETENTCNT, &raw mut word as *mut c_void);
            r.check("RNDGETENTCNT is not gated on the capability", rc == 0);
            err_is(&mut r, "euid is restored to 0", setresuid(-1, 0, -1), 0);
            r.check(
                "and the ids are root's again",
                geteuid().as_raw() == 0 && getuid().as_raw() == 0,
            );
        }
    }

    // A device that is not the pool refuses the pool's requests, and the
    // pool refuses the clock's - the families are keyed on the device, not
    // on whoever asks.
    let mut stray_rtc = [0i32; 9];
    let (_, errno) = ioctl_fd(&urandom, RTC_RD_TIME, stray_rtc.as_mut_ptr() as *mut c_void);
    if host_run() && errno != libc::ENOTTY {
        // Measured: a current kernel's random device reaches its own arm first
        // for a word whose command byte it has taken (nr 9 is `RNDGETPOOL`'s
        // neighbour), so the refusal is about the argument, not the family. The
        // cross-family ENOTTY is *this* kernel's order - magic first, device
        // second - and the gate keeps pinning it.
        println!(
            "note: the host answers the stray clock request errno {errno}; the family-keyed ENOTTY is this kernel's order"
        );
        r.check(
            "a stray clock request on the pool is refused either way",
            errno != 0,
        );
    } else {
        err_is(
            &mut r,
            "RTC_RD_TIME on urandom answers ENOTTY",
            errno,
            libc::ENOTTY,
        );
    }
    drop(urandom);

    // ---- The clock, as hwclock asks of it ----
    //
    // The host's rtc node is `crw-rw---- root:clock` (measured EACCES), so a
    // hosted run forgives the open and stops the section there - `t-dsp`'s
    // rule for a device node the host owns differently. Under a sweep the
    // node must open: the payload runs as root and the boot's own check reads
    // it the same way.
    let rtc = openat(CWD, "/dev/rtc0", OFlags::RDWR, Mode::empty());
    let rtc: OwnedFd = match rtc {
        Ok(fd) => fd,
        Err(e) => {
            r.check(
                &format!("/dev/rtc0 opens ({e:?})"),
                host_run() && matches!(e, rustix::io::Errno::PERM | rustix::io::Errno::ACCESS),
            );
            r.finish();
        }
    };
    r.check("/dev/rtc0 opens", true);

    let mut rtctime = [-1i32; 9];
    let (rc, _) = ioctl_fd(&rtc, RTC_RD_TIME, rtctime.as_mut_ptr() as *mut c_void);
    r.check("RTC_RD_TIME succeeds", rc == 0);
    let t = rtctime;
    r.check(
        "the clock's fields are in range",
        t[0] < 60 && t[1] < 60 && t[2] < 24 && (1..=31).contains(&t[3]) && (0..12).contains(&t[4]),
    );
    r.check(
        "the clock reads the post-2020 world it was booted from (tm_year since 1900)",
        t[5] >= 120,
    );
    r.check("tm_isdst is 0", t[8] == 0);

    // The same request spelled with the older RTC magic byte - `'p'`, which
    // is what Alpine's busybox hwclock actually sends, its `linux/rtc.h`
    // predating the kernel's move to `'t'`. Same struct, same command, the
    // same answer; and a `'p'` request that is *not* one of the two clock
    // commands stays ENOTTY, because `'p'` is PPP's family too.
    let mut rtctime2 = [-1i32; 9];
    let (rc, _) = ioctl_fd(&rtc, 0x8024_7009, rtctime2.as_mut_ptr() as *mut c_void);
    r.check("the 'p'-spelled RTC_RD_TIME succeeds", rc == 0);
    r.check(
        "both spellings read the same date",
        rtctime2[3] == t[3] && rtctime2[4] == t[4] && rtctime2[5] == t[5],
    );
    let (_, errno) = ioctl_fd(&rtc, 0x8024_7020, rtctime2.as_mut_ptr() as *mut c_void);
    err_is(
        &mut r,
        "a stray 'p'-typed request stays ENOTTY",
        errno,
        libc::ENOTTY,
    );

    // RTC_SET_TIME of what was just read back: the store round-trips, and a
    // second read agrees on the date. (The seconds may tick between the
    // calls, so the time-of-day fields are not compared.)
    let mut setbuf = t;
    let (rc, _) = ioctl_fd(&rtc, RTC_SET_TIME, setbuf.as_mut_ptr() as *mut c_void);
    r.check("RTC_SET_TIME succeeds", rc == 0);
    let mut rtctime3 = [-1i32; 9];
    let (rc, _) = ioctl_fd(&rtc, RTC_RD_TIME, rtctime3.as_mut_ptr() as *mut c_void);
    r.check("RTC_RD_TIME after the set succeeds", rc == 0);
    r.check(
        "the date reads back unchanged",
        t[3] == rtctime3[3] && t[4] == rtctime3[4] && t[5] == rtctime3[5],
    );

    // A nonsense date is refused rather than stored.
    let mut bad = [0i32; 9];
    bad[3] = 31;
    bad[4] = 12;
    bad[5] = 80;
    let (_, errno) = ioctl_fd(&rtc, RTC_SET_TIME, bad.as_mut_ptr() as *mut c_void);
    err_is(
        &mut r,
        "RTC_SET_TIME of month 13 answers EINVAL",
        errno,
        libc::EINVAL,
    );

    let mut word2 = 0i32;
    let (_, errno) = ioctl_fd(&rtc, RNDGETENTCNT, &raw mut word2 as *mut c_void);
    err_is(
        &mut r,
        "RNDGETENTCNT on rtc0 answers ENOTTY",
        errno,
        libc::ENOTTY,
    );
    // Measured on Linux, where `rtc_dev_ioctl` checks the magic byte first: a
    // pool request of any shape is not this device's, whichever arm it would
    // have reached.
    let mut credit_hdr = [64i32, 0i32];
    let (_, errno) = ioctl_fd(&rtc, RNDADDENTROPY, credit_hdr.as_mut_ptr() as *mut c_void);
    err_is(
        &mut r,
        "RNDADDENTROPY on rtc0 answers ENOTTY, not EPERM or EOPNOTSUPP",
        errno,
        libc::ENOTTY,
    );

    r.finish();
}
