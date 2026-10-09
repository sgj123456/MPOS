//! t-vt - the virtual consoles and the console mux's switch, on both machines.
//!
//! `/dev/tty1`..`/dev/tty6` are the slave ends of the pty pairs the devfs mints at
//! boot, and the console multiplexer holds their masters: it carries the active
//! console's output to the one serial wire and routes the wire's typed bytes back
//! to it, with `^A 1`..`6` (or a session's `VT_ACTIVATE`) choosing which console
//! that is. The nodes have always existed on both machines; the mux was x86_64's
//! until `NOTES.md` §13.16, so on the second machine a getty on `tty1` printed into
//! a pty nobody drained and the `'V'`/`'K'` ioctls answered `ENOTTY`.
//!
//! What this pins is that the family is *served* on both machines and that its
//! state is the mux's, read back through the other door:
//!
//! * `KDGETMODE`/`KDSETMODE` and `KDGKBMODE`/`KDSKBMODE` round-trip per console -
//!   a mode a seat manager sets and the kernel forgets is a text console painted
//!   over a compositor's screen one keystroke later;
//! * `VT_OPENQRY` names a console, `VT_ACTIVATE` is accepted, `VT_WAITACTIVE`
//!   returns - and the *switch itself* is visible on the wire, because a
//!   machine with no framebuffer narrates it (`[ttyN]`, from
//!   `vcmux::switch_to`). The mark this test writes to the newly active console
//!   arriving on the wire at all is the mux's own proof: that device is a pty
//!   slave, and the only thing that reads its master is the pump;
//! * the refusals Linux gives: a console the machine does not have is `ENXIO`, a
//!   mode that is not a mode is `EINVAL`, an unimplemented member of the family is
//!   `ENOTTY`.
//!
//! It leaves the console where it found it - a boot starts on console 1, which is
//! `term::vc::active`'s initial value, and the test switches back to it rather
//! than leaving the machine on a console nobody asked for.
//!
//! The ioctl words are the ones a real `linux/vt.h` and `linux/kd.h` spell - the
//! VT family as bare literals (`VT_GETSTATE` is `0x5603`, with no size bits at
//! all, which is why dispatch in both kernels keys on the magic byte and number).

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

// Measured from the shipped headers on this host, not recalled:
const VT_OPENQRY: libc::c_ulong = 0x5600;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;
const KDGETMODE: libc::c_ulong = 0x4b3b;
const KDSETMODE: libc::c_ulong = 0x4b3a;
const KDGKBMODE: libc::c_ulong = 0x4b44;
const KDSKBMODE: libc::c_ulong = 0x4b45;

/// The two mode words this test *drives*. `KD_TEXT` (0) and `K_XLATE` (1) are the
/// other half of each pair, and the test reaches them by reading back whatever mode
/// it found rather than by naming them - a console that starts in text mode is an
/// observation about a boot, while the round trip is the contract.
const KD_GRAPHICS: u32 = 0x01;
const K_OFF: u32 = 0x04;

/// How many virtual consoles the machine has (`term::vc::VC_COUNT`); a number
/// past it is the `ENXIO` Linux answers.
const VC_COUNT: u32 = 6;

/// One ioctl, with `errno` read immediately on failure.
///
/// Through `libc::syscall` for the reason given in `t-rtc`: `libc::ioctl`'s
/// request parameter is `c_int` in the musl bindings and `c_ulong` in the gnu
/// ones, and a request is neither.
fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: usize) -> (libc::c_long, i32) {
    // SAFETY: the pointer-shaped argument is whatever the caller passed, and every
    // caller here passes either a buffer it owns or a plain value the request
    // carries (the KD modes and the VT console numbers are their own argument).
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

fn open_rw(path: &str) -> Result<libc::c_int, std::io::Error> {
    let path = std::ffi::CString::new(path).expect("the path has no interior NUL");
    // SAFETY: `path` is a valid NUL-terminated string for the call's duration.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
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
    println!("t-vt");
    let mut r = Report { failed: false };

    // The KD family is keyed to a console's number, and the console tty the
    // harness handed this program on fd 0 is not one of the virtual consoles - so
    // the mode half runs against `/dev/tty1` itself.
    let tty1 = match open_rw("/dev/tty1") {
        Ok(fd) => {
            r.check("/dev/tty1 opens", true);
            fd
        }
        Err(err) => {
            r.check(&format!("/dev/tty1 opens (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    };

    // The VT family is a *console's*: Linux routes `VT_*` through the console
    // device, and a serial tty answers `ENOTTY` to it - which is what fd 0 is in
    // this harness (the console tty the runner gave the program, not a VT).
    // `/dev/tty0` is the door every system uses for "the console now on the
    // screen", and both kernels resolve it to the active virtual console.
    let console = match open_rw("/dev/tty0") {
        Ok(fd) => {
            r.check("/dev/tty0 opens", true);
            fd
        }
        Err(err) => {
            r.check(&format!("/dev/tty0 opens (got {err})"), false);
            -1
        }
    };

    // The video mode, as a round trip rather than a claim about what a boot
    // leaves behind: the *contract* is that the mode a session sets is the mode
    // the other door reads back, and the console is put back to whatever it was
    // found in (a machine whose console a compositor already owns is in graphics
    // mode, and this test must not report that as a failure of this kernel).
    let mut mode: i32 = -1;
    let (rc, err) = ioctl(tty1, KDGETMODE, &mut mode as *mut i32 as usize);
    r.check(
        &format!("KDGETMODE on /dev/tty1 succeeds (got rc {rc}, errno {err}, mode {mode})"),
        rc == 0,
    );
    println!("     the console was found in mode {mode}");
    let (rc, err) = ioctl(tty1, KDSETMODE, KD_GRAPHICS as usize);
    let mut readback: i32 = -1;
    let (rc2, _) = ioctl(tty1, KDGETMODE, &mut readback as *mut i32 as usize);
    r.check(
        &format!(
            "KDSETMODE(KD_GRAPHICS) is read back by KDGETMODE (got rc {rc} then {rc2}, \
             mode {readback}, errno {err})"
        ),
        rc == 0 && rc2 == 0 && readback == KD_GRAPHICS as i32,
    );
    let (rc, _) = ioctl(tty1, KDSETMODE, mode as usize);
    let mut restored: i32 = -1;
    let (rc2, _) = ioctl(tty1, KDGETMODE, &mut restored as *mut i32 as usize);
    r.check(
        &format!(
            "the mode the console was found in is put back (got rc {rc} then {rc2}, mode {restored})"
        ),
        rc == 0 && rc2 == 0 && restored == mode,
    );
    let (rc, err) = ioctl(tty1, KDSETMODE, 9);
    r.check(
        &format!("KDSETMODE(9) is EINVAL (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::EINVAL,
    );

    // The keyboard mode is the other half of the same handover, and the same
    // round trip: `K_OFF` is the one that matters to a session handing the console
    // over, because a keystroke that reaches the terminal underneath a compositor
    // is a command run twice.
    let mut kb: i32 = -1;
    let (rc, err) = ioctl(tty1, KDGKBMODE, &mut kb as *mut i32 as usize);
    r.check(
        &format!("KDGKBMODE succeeds (got rc {rc}, errno {err}, mode {kb})"),
        rc == 0,
    );
    let (rc, err) = ioctl(tty1, KDSKBMODE, K_OFF as usize);
    let mut kb_read: i32 = -1;
    let (rc2, _) = ioctl(tty1, KDGKBMODE, &mut kb_read as *mut i32 as usize);
    r.check(
        &format!(
            "KDSKBMODE(K_OFF) is read back by KDGKBMODE (got rc {rc} then {rc2}, mode {kb_read}, \
             errno {err})"
        ),
        rc == 0 && rc2 == 0 && kb_read == K_OFF as i32,
    );
    let (rc, _) = ioctl(tty1, KDSKBMODE, kb as usize);
    let mut kb_restored: i32 = -1;
    let (rc2, _) = ioctl(tty1, KDGKBMODE, &mut kb_restored as *mut i32 as usize);
    r.check(
        &format!(
            "the keyboard mode it was found with is put back (got rc {rc} then {rc2}, mode {kb_restored})"
        ),
        rc == 0 && rc2 == 0 && kb_restored == kb,
    );

    // The switch. `VT_OPENQRY` first: a console number, which is what a session
    // asks for when it wants one of its own.
    let mut free: i32 = -1;
    let (rc, err) = ioctl(console, VT_OPENQRY, &mut free as *mut i32 as usize);
    r.check(
        &format!("VT_OPENQRY names a console (got rc {rc}, errno {err}, console {free})"),
        rc == 0 && (1..=VC_COUNT as i32).contains(&free),
    );

    // VT_GETSTATE's `struct vt_stat { v_active, v_signal, v_state }`, and Linux
    // writes exactly two of the three shorts (`NOTES.md` §13.18). `v_active` is the
    // console on the screen (1-based); `v_state` is the bit mask of the consoles in
    // use, with bit 0 always set because "/dev/tty0 is always open"; and
    // `v_signal` - the SIGIO number a console promises to send when it loses the
    // screen - is *never written*. The buffer starts at a sentinel, because that is
    // the only way "never written" is observable: zeroed, a kernel that wrote 0
    // there would pass.
    //
    // This block used to print the three shorts and assert only that the call
    // succeeded, because the kernel's answer was not Linux's. It is now, so the
    // shape is asserted rather than described.
    // every short, not every byte: `0xAAu16` is `0x00AA`, so an array of them is
    // three shorts of 170 - and the assertion below would then be comparing the
    // kernel's answer against a sentinel it never wrote. `t-seat` spells the same
    // fill as `[0xAAu8; 6]`, which is the shape that actually reads back 0xAAAA.
    let mut st = [0xAAAAu16; 3];
    let (rc, err) = ioctl(console, VT_GETSTATE, st.as_mut_ptr() as usize);
    r.check(
        &format!("VT_GETSTATE succeeds (got rc {rc}, errno {err})"),
        rc == 0,
    );
    r.check(
        &format!("v_active names a console (got {}, of {VC_COUNT})", st[0]),
        (1..=VC_COUNT as u16).contains(&st[0]),
    );
    r.check(
        &format!(
            "v_state carries /dev/tty0's always-open bit (got {:#06x})",
            st[2]
        ),
        st[2] & 1 == 1,
    );
    r.check(
        &format!(
            "v_signal is left untouched, as Linux leaves it (got {:#06x}, expected 0xaaaa)",
            st[1]
        ),
        st[1] == 0xAAAA,
    );
    println!(
        "     VT_GETSTATE -> v_active {}, v_signal {:#06x}, v_state {:#06x}",
        st[0], st[1], st[2]
    );

    // The switch away: accepted, then waited for - and the mux narrates it on the
    // wire, which is where a reader can see the active console actually move.
    let target: u32 = 2;
    let (rc, err) = ioctl(console, VT_ACTIVATE, target as usize);
    r.check(
        &format!("VT_ACTIVATE({target}) is accepted (got rc {rc}, errno {err})"),
        rc == 0,
    );
    let (rc, err) = ioctl(console, VT_WAITACTIVE, target as usize);
    r.check(
        &format!("VT_WAITACTIVE({target}) returns (got rc {rc}, errno {err})"),
        rc == 0,
    );

    // The console that is now on the screen is the one a *write* reaches the wire
    // from: `/dev/tty2` is a pty slave whose master only the mux reads, so this
    // line arriving on the wire at all is the pump running (the mark is
    // deliberately distinctive - nothing else in this program prints it).
    let tty2 = match open_rw("/dev/tty2") {
        Ok(fd) => fd,
        Err(err) => {
            r.check(&format!("/dev/tty2 opens (got {err})"), false);
            -1
        }
    };
    if tty2 >= 0 {
        let mark = b"VT-PUMP-MARK\n";
        // SAFETY: `mark` is a live slice for the call's duration.
        let wrote = unsafe { libc::write(tty2, mark.as_ptr().cast::<c_void>(), mark.len()) };
        r.check(
            &format!(
                "writing to the active console is accepted (wrote {wrote} of {})",
                mark.len()
            ),
            wrote == mark.len() as isize,
        );
        close(tty2);
    }

    // The refusals.
    let (rc, err) = ioctl(console, VT_ACTIVATE, 0);
    r.check(
        &format!("VT_ACTIVATE(0) is ENXIO (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::ENXIO,
    );
    let (rc, err) = ioctl(console, VT_ACTIVATE, 600);
    r.check(
        &format!("VT_ACTIVATE(600) is ENXIO (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::ENXIO,
    );
    let (rc, err) = ioctl(console, 0x567f, 0);
    r.check(
        &format!("an unimplemented 'V' request is ENOTTY (got rc {rc}, errno {err})"),
        rc != 0 && err == libc::ENOTTY,
    );

    // And back to the console a boot starts on, so the test leaves the machine as
    // it found it.
    let (rc, err) = ioctl(console, VT_ACTIVATE, 1);
    r.check(
        &format!("VT_ACTIVATE(1) restores the boot console (got rc {rc}, errno {err})"),
        rc == 0,
    );
    let (rc, err) = ioctl(console, VT_WAITACTIVE, 1);
    r.check(
        &format!("VT_WAITACTIVE(1) returns (got rc {rc}, errno {err})"),
        rc == 0,
    );

    close(tty1);
    if console >= 0 {
        close(console);
    }
    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
