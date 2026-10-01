//! t-ioctl - one test for ioctl, the terminal-query door musl and every
//! line editor walk through (`ioctl(0, TIOCGWINSZ, &ws)` in the strace),
//! plus the two device families the boot's own services ask: the entropy
//! pool queries seedrng sends to `/dev/urandom`, and the clock reads and
//! writes hwclock sends to `/dev/rtc0`.
//!
//! Pinned here on fd 0, the console: TIOCGWINSZ writes the window the
//! kernel serves, zeroing the pixel fields too. The answer is the *screen's*
//! grid in cells whenever the machine has a screen - the test boot's QEMU
//! carries a virtio-gpu, so that is what it answers - and the 25x80 fallback
//! only when there is no screen at all. Checking it against
//! `FBIOGET_VSCREENINFO`/`8x16` rather than against a number of its own is
//! what keeps it that rule: a full-screen program lays itself out in the
//! window it was told, so a console whose answer changed mid-run would be
//! the corner-of-the-screen bug this rule exists to prevent. The same rule is
//! pinned on the host, where a screen can be made
//! (`vfs::tty::tests::a_console_reports_its_screens_window_and_others_do_not`).
//! TCGETS answers 0 and describes a console - a line at a time, echoed,
//! interruptible - and TCSETS changes what it describes, so a shell that
//! asks for raw mode gets one; a request the kernel has no handler for
//! answers ENOTTY. The bad-fd case is deliberately not exercised: this
//! kernel answers on the request alone, before any descriptor check, so
//! man-faithful EBADF ordering is not pinned here.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use lin::{ECHO, ICANON, ISIG, Termios, VEOF, VINTR};

static mut WS: lin::Winsize = lin::Winsize {
    ws_row: 0,
    ws_col: 0,
    ws_xpixel: 0xFFFF,
    ws_ypixel: 0xFFFF,
};
static mut TERMIOS: [u8; 64] = [0xFF; 64];
/// The one-word answer slot for FIONREAD/RNDGETENTCNT, and the pool-info
/// pair RNDADDENTROPY takes, and the nine ints of `struct rtc_time`.
static mut WORD: i32 = -1;
/// KDGKBTYPE's one-byte answer, seeded to a value no keyboard model uses so a
/// read that answers 0 without filling it is caught rather than believed.
static mut KBTYPE: u8 = 0xFF;
static mut POOLINFO: [i32; 2] = [0; 2];
/// A `struct rand_pool_info` with room for the seed bytes that follow its two
/// ints: the credit `RNDADDENTROPY` carries is not expressible in the bare pair
/// `POOLINFO` is.
static mut RNDPOOL: [i32; 16] = [0; 16];
static mut RTCTIME: [i32; 9] = [-1; 9];

// The request words, spelled in their `_IOC` form: direction, size, the
// family byte at bits 8..15, the command in the low byte. FIONREAD is the
// legacy x86 number that predates the layout; the rest carry it exactly -
// `RND*` are 'R', `RTC_*` are 't', and the size field of the RTC pair is
// `struct rtc_time`'s 36 bytes, which is what pins the struct's shape here.
const FIONREAD: usize = 0x541B;
// The `'R'` family spelled out in full, because the dispatch is on the whole
// word and not on the command byte: the number this test used to call
// `RNDADDENTROPY` - 0x4008_5202 - is `RNDGETPOOL`, a request the kernel stopped
// answering, and the credit seedrng actually sends is 0x4008_5203. Both
// spellings are pinned below so a dispatch on the low byte alone cannot creep
// back in.
const RNDGETENTCNT: usize = 0x8004_5200; // _IOR('R', 0x00, int)
const RNDADDTOENTCNT: usize = 0x4004_5201; // _IOW('R', 0x01, int)
const RNDGETPOOL: usize = 0x8008_5202; // _IOR('R', 0x02, int [2]): removed
const RNDADDENTROPY: usize = 0x4008_5203; // _IOW('R', 0x03, int [2])
const RNDZAPENTCNT: usize = 0x0000_5204; // _IO('R', 0x04): no arg, no direction
const RNDCLEARPOOL: usize = 0x0000_5206; // _IO('R', 0x06)
const RNDRESEEDCRNG: usize = 0x0000_5207; // _IO('R', 0x07)
/// `_IOW('R', 0x03, int)`: the credit's command with the header's `int [2]`
/// narrowed to an `int`. Not a credit, on either machine.
const CREDIT_WRONG_SIZE: usize = 0x4004_5203;
/// `_IOWR('R', 0x03, int [2])`: the same word with both direction bits.
const CREDIT_WRONG_DIR: usize = 0xC008_5203;
/// `_IO('R', 0x09)`: a number nobody has taken.
const RND_UNKNOWN: usize = 0x0000_5209;
const RTC_RD_TIME: usize = 0x8024_7409;
const RTC_SET_TIME: usize = 0x4024_740A;
const FBIOGET_VSCREENINFO: usize = 0x4600;
/// KDGKBTYPE: a `_IOR` over the console's 'K' family, one byte out - the
/// keyboard model the console reports. `0x4B33` as `/usr/include/linux/kd.h`
/// spells it (dir read, size 1, 'K' at bits 8..15, command 0x33).
const KDGKBTYPE: usize = 0x4B33;
/// The cell the kernel's console font draws: the framebuffer console *is*
/// `/dev/tty0`'s emulator and its grid *is* that terminal's window, so the
/// window in cells is the screen in pixels divided by this. `vfs/term` embeds
/// the 8x16 `vga8x16` font it draws with, and `TIOCGWINSZ` on `/dev/ttyN`
/// answers `Grid::rows/cols` - the same numbers, which is why they are checked
/// against the screen rather than against a number a headless machine picked.
const CELL_W: isize = 8;
const CELL_H: isize = 16;

/// The `struct termios` glibc hands an ioctl, and the `c_lflag` bits and `c_cc`
/// indices this program reads: they live in `lin` beside the other ABI shapes,
/// because putting a terminal in raw mode is not one test's private business.
#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-ioctl\n");
    let mut failed = false;

    let r = lin::ioctl(0, lin::TIOCGWINSZ, &raw mut WS as usize);
    lin::check_eq(&mut failed, b"TIOCGWINSZ on the console succeeds", r, 0);
    let ws = unsafe { &*(&raw mut WS as *const lin::Winsize) };
    // The console's window, checked against the screen it draws on. A machine
    // with a display has a framebuffer console of that display's size whatever
    // its kernel log rides on; a machine with no display at all answers the
    // 25x80 fallback. What is *not* pinned is a boot-shaped answer: the same
    // machine has to give the same window all run long, which is what a
    // console whose size followed the DRM master got wrong.
    let fb = lin::openat(lin::AT_FDCWD, b"/dev/fb0\0", lin::O_RDONLY, 0) as i32;
    if fb >= 0 {
        let mut var = [0u8; 160];
        let r = lin::ioctl(fb, FBIOGET_VSCREENINFO, var.as_mut_ptr() as usize);
        lin::check_eq(&mut failed, b"FBIOGET_VSCREENINFO sizes the screen", r, 0);
        let xres = u32::from_ne_bytes([var[0], var[1], var[2], var[3]]) as isize;
        let yres = u32::from_ne_bytes([var[4], var[5], var[6], var[7]]) as isize;
        // The grid is the screen counted in cells, which is exactly what the
        // render task computes: columns floor-divide (`Grid` caps rows, not
        // columns) and rows are capped at `vfs/term::MAX_ROWS`, 64 - a height no
        // test screen reaches.
        lin::check_eq(
            &mut failed,
            b"the console's rows are the screen's",
            ws.ws_row as isize,
            (yres / CELL_H).min(64),
        );
        lin::check_eq(
            &mut failed,
            b"the console's columns are the screen's",
            ws.ws_col as isize,
            xres / CELL_W,
        );
        lin::close(fb);
    } else {
        lin::check_eq(
            &mut failed,
            b"with no screen the console reports 25 rows",
            ws.ws_row as isize,
            25,
        );
        lin::check_eq(
            &mut failed,
            b"with no screen the console reports 80 columns",
            ws.ws_col as isize,
            80,
        );
    }
    lin::check_eq(
        &mut failed,
        b"the pixel fields are zeroed, not left to the caller",
        (ws.ws_xpixel as isize) | (ws.ws_ypixel as isize),
        0,
    );

    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS on the console succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    // A console: a line at a time, and shown as it is typed.
    lin::check_true(
        &mut failed,
        b"the console is in canonical mode",
        tio.lflag & ICANON != 0,
    );
    lin::check_true(
        &mut failed,
        b"the console echoes what is typed",
        tio.lflag & ECHO != 0,
    );
    lin::check_true(
        &mut failed,
        b"the console turns the interrupt key into a signal",
        tio.lflag & ISIG != 0,
    );
    lin::check_eq(
        &mut failed,
        b"^C is the interrupt",
        tio.cc[VINTR] as isize,
        3,
    );
    lin::check_eq(&mut failed, b"^D is end of input", tio.cc[VEOF] as isize, 4);

    // TCSETS: what a shell does when it wants to do its own editing. Ask for
    // raw, read it back, and put the console back the way it was found - a
    // test that leaves the terminal in raw mode leaves the next line of the
    // script unreadable.
    let mut saved = *tio;
    let mut raw = saved;
    raw.lflag &= !(ICANON | ECHO);
    let r = lin::ioctl(0, lin::TCSETS, &raw mut raw as usize);
    lin::check_eq(&mut failed, b"TCSETS on the console succeeds", r, 0);

    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS after TCSETS succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    lin::check_eq(
        &mut failed,
        b"the settings came back as they were set",
        tio.lflag as isize,
        raw.lflag as isize,
    );
    lin::check_true(
        &mut failed,
        b"canonical mode is off, as asked",
        tio.lflag & ICANON == 0,
    );

    let r = lin::ioctl(0, lin::TCSETS, &raw mut saved as usize);
    lin::check_eq(&mut failed, b"restoring the console succeeds", r, 0);
    let r = lin::ioctl(0, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(&mut failed, b"TCGETS after restoring succeeds", r, 0);
    let tio = unsafe { &*(&raw mut TERMIOS as *const Termios) };
    lin::check_eq(
        &mut failed,
        b"the console is canonical again",
        tio.lflag as isize,
        saved.lflag as isize,
    );

    // A file is not a terminal, and says so. The path is a NUL-terminated
    // static, as in every other test: a bare `b"..."` literal carries no
    // terminator, and the kernel reads the path until the first zero byte -
    // which walks off the literal into whatever rodata happens to follow.
    // A file every initramfs and root filesystem here carries - the release
    // file belongs to a package the test image does not install, and a test
    // that needs a non-terminal descriptor should not depend on which.
    static HOSTNAME: [u8; 14] = *b"/etc/hostname\0";
    let fd = lin::openat(lin::AT_FDCWD, &HOSTNAME, lin::O_RDONLY, 0);
    let r = lin::ioctl(fd as i32, lin::TCGETS, &raw mut TERMIOS as usize);
    lin::check_eq(
        &mut failed,
        b"TCGETS on a file answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    lin::close(fd as i32);

    let r = lin::ioctl(0, 0xDEAD_BEEF, 0);
    lin::check_eq(
        &mut failed,
        b"an unknown request answers ENOTTY",
        r,
        lin::ENOTTY,
    );

    let r = lin::ioctl(1, lin::TIOCGWINSZ, &raw mut WS as usize);
    lin::check_eq(
        &mut failed,
        b"TIOCGWINSZ answers stdout the same console shape",
        r,
        0,
    );

    // KDGKBTYPE on the console: a read that must *fill* the caller's byte, not
    // just answer 0. Linux reports the constant KB_101 whatever is plugged in
    // (kd.h: "this is what we always answer"). The buffer starts at 0xFF, so
    // this also pins the false-success that used to live here: the request fell
    // through to the console's setter accept, returning 0 with the byte left
    // uninitialised - a keyboard model the reader would then act on.
    let r = lin::ioctl(0, KDGKBTYPE, &raw mut KBTYPE as usize);
    lin::check_eq(&mut failed, b"KDGKBTYPE on the console succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"KDGKBTYPE fills the byte with KB_101, not a stale stack value",
        unsafe { *(&raw mut KBTYPE as *const u8) } as isize,
        2,
    );

    // ---- The rest of the console `K` family is refused, not faked ----
    //
    // KDGETLED (0x31) is a READ of live LED state this kernel has no controller
    // to source, and KDSETLED (0x32) a WRITE to hardware that is not present.
    // Both used to fall through to `_ => 0`: the getter left the caller's int as
    // stack garbage while claiming success (the KDGKBTYPE-style lie), the setter
    // claimed an effect nothing performed. They are now EOPNOTSUPP - a real
    // console ioctl this device declines. A number outside the 'K' command block
    // (0x4B80, nr 0x80) is not a console ioctl at all and answers ENOTTY. All
    // counterfactual against the old blanket 0. Serving these per-device is the
    // Phase 11 B2 move.
    const KDGETLED: usize = 0x4B31;
    const KDSETLED: usize = 0x4B32;
    const KD_UNKNOWN: usize = 0x4B80;
    let r = lin::ioctl(0, KDGETLED, &raw mut KBTYPE as usize);
    lin::check_eq(
        &mut failed,
        b"KDGETLED is refused EOPNOTSUPP, not 0 with a stale buffer",
        r,
        lin::EOPNOTSUPP,
    );
    let r = lin::ioctl(0, KDSETLED, 3);
    lin::check_eq(
        &mut failed,
        b"KDSETLED is refused EOPNOTSUPP, not a fake success",
        r,
        lin::EOPNOTSUPP,
    );
    let r = lin::ioctl(0, KD_UNKNOWN, 0);
    lin::check_eq(
        &mut failed,
        b"a 'K'-typed number outside the command block is ENOTTY",
        r,
        lin::ENOTTY,
    );

    // ---- The entropy pool, as seedrng asks of it ----
    static URANDOM: [u8; 13] = *b"/dev/urandom\0";
    static RTC0: [u8; 10] = *b"/dev/rtc0\0";
    let urandom = lin::openat(lin::AT_FDCWD, &URANDOM, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/dev/urandom opens", urandom >= 0);

    let r = lin::ioctl(urandom as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"RNDGETENTCNT succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"the pool answers full (4096 bits)",
        unsafe { *(&raw mut WORD as *const i32) } as isize,
        4096,
    );

    // FIONREAD on an entropy device is `EINVAL` on Linux - root and
    // unprivileged, `O_RDONLY` and `O_WRONLY`, and *before* the argument is
    // looked at (a NULL pointer is still `EINVAL`, not `EFAULT`). This kernel
    // used to answer 512 here, and the seedrng reason written down for it was
    // false: the binary in this image sends exactly one ioctl, the
    // `RNDADDENTROPY` credit below, and takes its seed length from
    // `/proc/sys/kernel/random/poolsize`. The old pin asserted the 512, so it
    // had encoded the divergence; it is corrected with the measurement, which
    // is rule 1's carve-out, not weakened to keep a passing shape.
    let r = lin::ioctl(urandom as i32, FIONREAD, &raw mut WORD as usize);
    lin::check_eq(
        &mut failed,
        b"FIONREAD on the pool is EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::ioctl(urandom as i32, FIONREAD, 0);
    lin::check_eq(
        &mut failed,
        b"FIONREAD on the pool is EINVAL even with a NULL argument",
        r,
        lin::EINVAL,
    );

    // ---- RNDADDENTROPY: the credit OpenRC's seedrng sends on every boot ----
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
    unsafe {
        *(&raw mut POOLINFO as *mut [i32; 2]) = [64, 0];
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut POOLINFO as usize);
    lin::check_eq(&mut failed, b"RNDADDENTROPY {64, 0} is accepted", r, 0);

    // The shape seedrng sends: a header *and* the seed bytes it declares, so
    // the kernel has to prove the seed is reachable before it believes the
    // length. `RNDPOOL` holds 4096 bits' worth of header plus eight seed bytes.
    unsafe {
        let pool = &raw mut RNDPOOL as *mut i32;
        pool.write(4096);
        pool.add(1).write(8);
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut RNDPOOL as usize);
    lin::check_eq(
        &mut failed,
        b"RNDADDENTROPY with the seed bytes it declares is accepted",
        r,
        0,
    );
    // An opening mode is not part of this family's gate: measured on Linux,
    // the credit is accepted on a descriptor open only for writing and the
    // count is read back on one open only for reading. That is worth a pin
    // here rather than an assumption, because this kernel *does* enforce
    // `O_RDONLY`/`O_WRONLY` on its read and write doors - an ioctl that
    // borrowed that predicate would refuse a caller Linux accepts, and the
    // pool fd above is itself `O_RDONLY`.
    let wonly = lin::openat(lin::AT_FDCWD, &URANDOM, lin::O_WRONLY, 0);
    lin::check_true(&mut failed, b"/dev/urandom opens O_WRONLY", wonly >= 0);
    let r = lin::ioctl(wonly as i32, RNDADDENTROPY, &raw mut POOLINFO as usize);
    lin::check_eq(
        &mut failed,
        b"the credit does not care that the fd is write-only",
        r,
        0,
    );
    let r = lin::ioctl(wonly as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"and neither does the count read", r, 0);
    lin::close(wonly as i32);

    // A negative count is EINVAL, and the count is looked at *before* the
    // length is believed: measured, `{-1, -1}` and `{-1, 65536}` are both
    // EINVAL although the second length is unusable too.
    unsafe {
        let pool = &raw mut RNDPOOL as *mut i32;
        pool.write(-1);
        pool.add(1).write(65536);
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut RNDPOOL as usize);
    lin::check_eq(
        &mut failed,
        b"a negative credit is EINVAL even beside a bad length",
        r,
        lin::EINVAL,
    );

    // A length that is negative, or that reaches past any mapping, is the
    // `EFAULT` Linux answers where its copy faults. 16 MiB is the honest shape
    // of that pin: measured on Linux, a declared 64 KiB *succeeded*, because
    // the heap behind an 8-byte header is mapped and the kernel only reads what
    // the header says is there. Here the same length also crosses this
    // kernel's `MAX_IO` bound, which is the worst-case answer the §4 rules ask
    // for rather than a copy of the caller's choosing.
    unsafe {
        let pool = &raw mut RNDPOOL as *mut i32;
        pool.write(8);
        pool.add(1).write(-1);
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut RNDPOOL as usize);
    lin::check_eq(
        &mut failed,
        b"a negative seed length is EFAULT",
        r,
        lin::EFAULT,
    );
    unsafe {
        let pool = &raw mut RNDPOOL as *mut i32;
        pool.add(1).write(16 * 1024 * 1024);
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut RNDPOOL as usize);
    lin::check_eq(
        &mut failed,
        b"a seed length past any mapping is EFAULT",
        r,
        lin::EFAULT,
    );
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, 0);
    lin::check_eq(
        &mut failed,
        b"RNDADDENTROPY with no header at all is EFAULT",
        r,
        lin::EFAULT,
    );

    // ---- RNDADDTOENTCNT: the same gate, one int, no seed ----
    unsafe { WORD = 8 };
    let r = lin::ioctl(urandom as i32, RNDADDTOENTCNT, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"RNDADDTOENTCNT 8 is accepted", r, 0);
    unsafe { WORD = -1 };
    let r = lin::ioctl(urandom as i32, RNDADDTOENTCNT, &raw mut WORD as usize);
    lin::check_eq(
        &mut failed,
        b"RNDADDTOENTCNT of a negative credit is EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::ioctl(urandom as i32, RNDADDTOENTCNT, 0);
    lin::check_eq(
        &mut failed,
        b"RNDADDTOENTCNT with no int is EFAULT",
        r,
        lin::EFAULT,
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
        let r = lin::ioctl(urandom as i32, request, &raw mut RNDPOOL as usize);
        lin::check_eq(&mut failed, label.as_bytes(), r, lin::EINVAL);
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
        let r = lin::ioctl(urandom as i32, request, 0);
        lin::check_eq(&mut failed, label.as_bytes(), r, lin::EOPNOTSUPP);
    }
    let r = lin::ioctl(urandom as i32, RNDRESEEDCRNG, 0);
    lin::check_eq(
        &mut failed,
        b"RNDRESEEDCRNG re-keys the generator and answers 0",
        r,
        0,
    );
    // A re-key is not a drain: the count the read reports does not move across
    // one, on Linux or here.
    let r = lin::ioctl(urandom as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(&mut failed, b"the count still reads after a re-key", r, 0);
    lin::check_eq(
        &mut failed,
        b"and it is still 4096 bits - a re-key drains nothing",
        unsafe { *(&raw mut WORD as *const i32) } as isize,
        4096,
    );

    // The capability gate, measured on Linux: every mutating RND ioctl is EPERM
    // without CAP_SYS_ADMIN, and EPERM comes *before* anything is read from the
    // caller's buffer. Lower the effective user only - ruid and the saved id
    // stay 0, so the privilege is reclaimable - and a header that answered
    // EINVAL for root a moment ago now answers EPERM. That is the counterfactual
    // for the order, as the old handler's `0` for everyone is the counterfactual
    // for the gate.
    lin::check_eq(
        &mut failed,
        b"setresuid lowers euid (ruid/saved stay 0)",
        lin::setresuid(-1, 65534, -1),
        0,
    );
    unsafe {
        let pool = &raw mut RNDPOOL as *mut i32;
        pool.write(-1);
        pool.add(1).write(-1);
    }
    let r = lin::ioctl(urandom as i32, RNDADDENTROPY, &raw mut RNDPOOL as usize);
    lin::check_eq(
        &mut failed,
        b"RNDADDENTROPY without CAP_SYS_ADMIN is EPERM, before its EINVAL",
        r,
        lin::EPERM,
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
        let r = lin::ioctl(urandom as i32, request, 0);
        lin::check_eq(&mut failed, label.as_bytes(), r, lin::EPERM);
    }
    // The command is refused before the capability is asked, so an
    // unrecognised `'R'` word is still `EINVAL` for this caller - it was never
    // a request the device could grant or withhold.
    for (request, label) in [
        (
            RNDGETPOOL,
            "RNDGETPOOL is still EINVAL without the capability",
        ),
        (RND_UNKNOWN, "an untaken 'R' number is still EINVAL too"),
    ] {
        let r = lin::ioctl(urandom as i32, request, 0);
        lin::check_eq(&mut failed, label.as_bytes(), r, lin::EINVAL);
    }
    // Reading the pool needs no privilege, whatever the write side costs.
    let r = lin::ioctl(urandom as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(
        &mut failed,
        b"RNDGETENTCNT is not gated on the capability",
        r,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"euid is restored to 0",
        lin::setresuid(-1, 0, -1),
        0,
    );

    // A device that is not the pool refuses the pool's requests, and the
    // pool refuses the clock's - the families are keyed on the device, not
    // on whoever asks.
    let r = lin::ioctl(urandom as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"RTC_RD_TIME on urandom answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    lin::close(urandom as i32);

    // ---- The clock, as hwclock asks of it ----
    let rtc = lin::openat(lin::AT_FDCWD, &RTC0, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/rtc0 opens", rtc >= 0);

    let r = lin::ioctl(rtc as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_RD_TIME succeeds", r, 0);
    let t = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"the clock's fields are in range",
        t[0] < 60 && t[1] < 60 && t[2] < 24 && (1..=31).contains(&t[3]) && (0..12).contains(&t[4]),
    );
    lin::check_true(
        &mut failed,
        b"the clock reads the post-2020 world it was booted from (tm_year since 1900)",
        t[5] >= 120,
    );
    lin::check_eq(&mut failed, b"tm_isdst is 0", t[8] as isize, 0);

    // The same request spelled with the older RTC magic byte - `'p'`, which
    // is what Alpine's busybox hwclock actually sends, its `linux/rtc.h`
    // predating the kernel's move to `'t'`. Same struct, same command, the
    // same answer; and a `'p'` request that is *not* one of the two clock
    // commands stays ENOTTY, because `'p'` is PPP's family too.
    let r = lin::ioctl(rtc as i32, 0x8024_7009, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"the 'p'-spelled RTC_RD_TIME succeeds", r, 0);
    let tp = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"both spellings read the same date",
        tp[3] == t[3] && tp[4] == t[4] && tp[5] == t[5],
    );
    let r = lin::ioctl(rtc as i32, 0x8024_7020, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"a stray 'p'-typed request stays ENOTTY",
        r,
        lin::ENOTTY,
    );

    // RTC_SET_TIME of what was just read back: the store round-trips, and a
    // second read agrees on the date. (The seconds may tick between the
    // calls, so the time-of-day fields are not compared.)
    let r = lin::ioctl(rtc as i32, RTC_SET_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_SET_TIME succeeds", r, 0);
    let r = lin::ioctl(rtc as i32, RTC_RD_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(&mut failed, b"RTC_RD_TIME after the set succeeds", r, 0);
    let t2 = unsafe { *(&raw mut RTCTIME as *const [i32; 9]) };
    lin::check_true(
        &mut failed,
        b"the date reads back unchanged",
        t[3] == t2[3] && t[4] == t2[4] && t[5] == t2[5],
    );

    // A nonsense date is refused rather than stored.
    unsafe {
        *(&raw mut RTCTIME as *mut [i32; 9]) = [0, 0, 0, 31, 12, 80, 0, 0, 0];
    }
    let r = lin::ioctl(rtc as i32, RTC_SET_TIME, &raw mut RTCTIME as usize);
    lin::check_eq(
        &mut failed,
        b"RTC_SET_TIME of month 13 answers EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::ioctl(rtc as i32, RNDGETENTCNT, &raw mut WORD as usize);
    lin::check_eq(
        &mut failed,
        b"RNDGETENTCNT on rtc0 answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    // Measured on Linux, where `rtc_dev_ioctl` checks the magic byte first: a
    // pool request of any shape is not this device's, whichever arm it would
    // have reached.
    let r = lin::ioctl(rtc as i32, RNDADDENTROPY, &raw mut POOLINFO as usize);
    lin::check_eq(
        &mut failed,
        b"RNDADDENTROPY on rtc0 answers ENOTTY, not EPERM or EOPNOTSUPP",
        r,
        lin::ENOTTY,
    );
    lin::close(rtc as i32);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-ioctl: panicked\n")
}
