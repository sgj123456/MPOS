//! t-evdev - the capability picture libinput reads before it touches events.
//!
//! Pinned here, per input-event-codes and ioctl_if: opening an evdev node
//! and asking what it *is* - `EVIOCGVERSION`, the type bitmap of
//! `EVIOCGBIT(0)`, the per-type code bitmap of `EVIOCGBIT(EV_KEY)`, the
//! device `id`, and the `EVIOCGRAB` seat claim - must answer with the same
//! shape Linux's virtio-input does. `EVIOCGABS` on an axis the device lacks
//! is `EINVAL`, which is what lets a reader tell a keyboard from a tablet
//! without guessing. This is the surface that turns "every node reads as a
//! null device" (the old behaviour) into a device `libinput` will classify.
//!
//! The read rule is pinned here too: while one description holds the grab, a
//! reader that is not it is told nothing - neither the records nor a readable
//! answer - and two readers of one node each keep their own copy of what the
//! device has produced. Nothing presses the keyboard during a sweep, so the
//! events come from a device the test makes for itself through `/dev/uinput`,
//! which is the interface Linux gives a program that has to inject input.
//!
//! # What the move changed
//!
//! The claims, their order and their wording are the original's, except for the
//! fixture the second machine forced: **which node is the keyboard.** A boot
//! gives this machine two virtio input nodes - a keyboard and a tablet - and
//! which of `event0`/`event1` each lands at *differs by machine*: the first
//! machine enumerates the keyboard first, the second the tablet first (the
//! transport is virtio-PCI on one and virtio-MMIO on the other, and the order is
//! the bus's, not this kernel's). The original opened `event0` and named it the
//! keyboard, which is true on the first machine only. So the keyboard is now
//! found by *what it is* rather than by its number - the node that reports no
//! absolute axes - which is exactly the read the paragraph above describes, and
//! the same way the tablet and the synthetic device below are found (by asking,
//! not by index). Nothing about a claim moved beyond that one fixture, and the
//! door is the one the other batches took: this program's own `ioctl` through
//! `libc::syscall`, and `rustix` for the file and event calls (`openat`, `read`,
//! `write`, `fcntl_getfl`/`fcntl_setfl`, `readlink`, `event::poll`).
//!
//! # Why this one is not run on the host
//!
//! Every answer below is this kernel's evdev and uinput implementation's - the
//! read rule, the grab, `EVIOCREVOKE`, the sysfs picture udev walks. A host's
//! `/dev/input/event*` and `/dev/uinput` are different devices with a different
//! topology, so a hosted run would measure the host and fail for a reason that is
//! not a defect. It prints what it is and makes **no claim at all**, which is
//! `t-drm-lease`'s rule for the same reason (`NOTES.md` §15.34).

use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};

use mpos_user_tests::{Report, host_run};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};

const EV_SYN: usize = 0x00;
const EV_KEY: usize = 0x01;
const EV_ABS: usize = 0x03;

/// The refusals, from `libc` - the maintained per-machine table, not recalled.
const ENOENT: isize = -(libc::ENOENT as isize);
const EINVAL: isize = -(libc::EINVAL as isize);
const ENOSYS: isize = -(libc::ENOSYS as isize);
const EBUSY: isize = -(libc::EBUSY as isize);
const ENODEV: isize = -(libc::ENODEV as isize);
const EAGAIN: isize = -(libc::EAGAIN as isize);

/// `_IOC(dir, 'E', nr, size)`: the evdev request word, type byte `E`.
const fn ioc(dir: usize, nr: usize, size: usize) -> u64 {
    ((dir as u64) << 30) | ((size as u64) << 16) | ((b'E' as u64) << 8) | (nr as u64)
}
const IOC_READ: usize = 2;
const IOC_WRITE: usize = 1;

/// The same `_IOC` spelling for the `'U'` family `/dev/uinput` lives in: its
/// requests carry the type byte `U` where evdev's carry `E`, and everything
/// else about the word - direction, size, number - is spelled the same way.
const fn uio(dir: usize, nr: usize, size: usize) -> u64 {
    ((dir as u64) << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | (nr as u64)
}

/// `UI_DEV_CREATE` / `UI_DEV_DESTROY`: `_IO('U', 1)` and `_IO('U', 2)`.
const UI_DEV_CREATE: u64 = uio(0, 1, 0);
const UI_DEV_DESTROY: u64 = uio(0, 2, 0);
/// `UI_DEV_SETUP`: `_IOW('U', 3, struct uinput_setup)`, 92 bytes of it.
const UI_DEV_SETUP: u64 = uio(IOC_WRITE, 3, UINPUT_SETUP_SIZE);
/// `UI_GET_VERSION`: `_IOR('U', 45, unsigned int)`.
const UI_GET_VERSION: u64 = uio(IOC_READ, 45, 4);
/// The bit-setters, `_IOW('U', n, int)`.
const UI_SET_EVBIT: u64 = uio(IOC_WRITE, 100, 4);
const UI_SET_KEYBIT: u64 = uio(IOC_WRITE, 101, 4);

/// `struct uinput_setup`: `struct input_id` (four `u16`s), the name, and the
/// force-feedback effect count. The name field is the 80 bytes between them.
const UINPUT_SETUP_SIZE: usize = 92;

/// The code this test reports and the `SYN_REPORT` that frames it the way every
/// input device frames its events. `EV_SYN`/`EV_KEY` above are the types.
const SYN_REPORT: u16 = 0x00;
const KEY_A: u16 = 30;

/// One `struct input_event`: two `long`s of timestamp, then type, code and
/// value - the same 24 bytes a read of an evdev node returns.
const RECORD: usize = 24;

/// A `poll` that returns at once: this program asks "is it readable now?" and
/// never waits, so no claim here can hang on a device that stays quiet.
const NOW: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// Build the two records one keystroke is: the key going down, then the frame
/// it belongs to.
fn keystroke(value: i32) -> [u8; RECORD * 2] {
    let mut bytes = [0u8; RECORD * 2];
    let mut put = |at: usize, ty: u16, code: u16, value: i32| {
        bytes[at + 16..at + 18].copy_from_slice(&ty.to_ne_bytes());
        bytes[at + 18..at + 20].copy_from_slice(&code.to_ne_bytes());
        bytes[at + 20..at + 24].copy_from_slice(&value.to_ne_bytes());
    };
    put(0, EV_KEY as u16, KEY_A, value);
    put(RECORD, EV_SYN as u16, SYN_REPORT, 0);
    bytes
}

/// One `ioctl`, answering the negative errno a raw syscall hands back.
///
/// Through `libc::syscall` rather than `libc::ioctl`, for `t-drm-lease`'s
/// reason: the request is an `unsigned long` word and `ioctl`'s binding of it is
/// not the same width on both libcs. The number comes from `libc::SYS_ioctl`,
/// the maintained per-machine table.
///
/// # Safety
/// `arg` must be null, a value the request takes by value, or an address the
/// caller owns that is at least as long as the size the request word names.
unsafe fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd as libc::c_long, request, arg) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// One `read`, answering the negative errno a refusal hands back.
fn read(fd: &OwnedFd, buf: &mut [u8]) -> isize {
    match rustix::io::read(fd, buf) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw_os_error() as isize),
    }
}

/// The same, for a write.
fn write(fd: &OwnedFd, buf: &[u8]) -> isize {
    match rustix::io::write(fd, buf) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw_os_error() as isize),
    }
}

/// `EVIOCGBIT(evtype, len)`, whose command number is `0x20 + evtype`.
fn eviocgbit(fd: i32, evtype: usize, out: &mut [u8]) -> isize {
    let req = ioc(IOC_READ, 0x20 + evtype, out.len());
    unsafe { ioctl(fd, req, out.as_mut_ptr() as *mut c_void) }
}

/// Whether a `poll` says this descriptor is readable right now.
fn readable(fd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    poll(&mut fds, Some(&NOW)).is_ok_and(|n| n > 0 && fds[0].revents().contains(PollFlags::IN))
}

/// A claim that an ioctl answered a particular number. The name is printed
/// unchanged when it holds; the answer and the expectation are appended only on
/// failure, so a green run's claim list is byte-identical on both machines.
fn check_eq(r: &mut Report, name: &str, got: isize, want: isize) {
    if got == want {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got}, want {want})"), false);
    }
}

/// The same, for a predicate the caller computed.
fn check_true(r: &mut Report, name: &str, ok: bool) {
    r.check(name, ok);
}

fn main() {
    println!("t-evdev");
    let mut r = Report::new();

    // See the header: a host's evdev and uinput are different devices with a
    // different topology, so a hosted run makes no claim at all.
    if host_run() {
        println!(
            "note: not made on a host - these answers are this kernel's evdev and uinput \
             implementation's, and a host's /dev/input/event* and /dev/uinput are different \
             devices that would answer a different set of numbers"
        );
        r.finish();
    }

    // --- the fixture: which node is the keyboard ---
    //
    // A boot gives the machine one relative-only key device and one pointer with
    // absolute axes, and which of event0/event1 each lands at differs by machine
    // (see the header). The keyboard is therefore the node that reports no
    // absolute axes - the same read a reader uses to tell the two apart, and the
    // same "ask, do not index" the synthetic device below is found by.
    // The classifier holds no descriptor: it opens each candidate, asks, and
    // lets it close, so the descriptors the test keeps below get the same
    // numbers on both machines whatever order the bus enumerated the nodes in.
    let mut kbd_index: Option<usize> = None;
    let mut tablet_index: Option<usize> = None;
    for index in 0..8usize {
        let path = format!("/dev/input/event{index}");
        let Ok(candidate) = rustix::fs::open(&path, OFlags::RDONLY, Mode::empty()) else {
            continue;
        };
        let mut axes = [0u8; 32];
        let _ = eviocgbit(candidate.as_raw_fd(), EV_ABS, &mut axes);
        if axes.iter().any(|&b| b != 0) {
            if tablet_index.is_none() {
                tablet_index = Some(index);
            }
        } else if kbd_index.is_none() {
            kbd_index = Some(index);
        }
    }
    check_true(&mut r, "an evdev keyboard node opens", kbd_index.is_some());
    if kbd_index.is_none() {
        r.finish();
    }
    let kbd_path = format!("/dev/input/event{}", kbd_index.unwrap());
    let kbd = match rustix::fs::open(&kbd_path, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("the keyboard node reopens ({e:?})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    };
    let tablet = tablet_index.and_then(|index| {
        rustix::fs::open(
            format!("/dev/input/event{index}"),
            OFlags::RDONLY,
            Mode::empty(),
        )
        .ok()
    });
    let fd = kbd.as_raw_fd();

    // EVIOCGVERSION: the evdev ABI, always 1.
    let mut version: i32 = 0;
    let got = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x01, 4),
            &mut version as *mut i32 as *mut c_void,
        )
    };
    check_eq(&mut r, "EVIOCGVERSION succeeds", got, 0);
    check_eq(&mut r, "the evdev version is 1", version as isize, 1);

    // EVIOCGBIT(0): which event types exist. A keyboard sets EV_KEY (bit 1).
    let mut types = [0u8; 8];
    let got = eviocgbit(fd, EV_SYN, &mut types);
    check_eq(&mut r, "EVIOCGBIT(0) succeeds", got, 0);
    check_true(
        &mut r,
        "the type bitmap reports EV_KEY",
        types[0] & (1 << EV_KEY) != 0,
    );

    // EVIOCGBIT(EV_KEY): the set of keys - non-empty for a real keyboard.
    let mut keys = [0u8; 128];
    let got = eviocgbit(fd, EV_KEY, &mut keys);
    check_eq(&mut r, "EVIOCGBIT(EV_KEY) succeeds", got, 0);
    check_true(
        &mut r,
        "the key code bitmap is not empty",
        keys.iter().any(|&b| b != 0),
    );

    // EVIOCGID: a caller can read the 8-byte struct back without error.
    let mut id = [0u8; 8];
    let got = unsafe { ioctl(fd, ioc(IOC_READ, 0x02, 8), id.as_mut_ptr() as *mut c_void) };
    check_eq(&mut r, "EVIOCGID succeeds", got, 0);

    // EVIOCGPHYS and EVIOCGUNIQ: a virtio input device has neither string,
    // and the answer has to be ENOENT. It may not simply be "unsupported":
    // libevdev's `libevdev_set_fd` tolerates a failure here *only* for
    // ENOENT, exactly as Linux's `str_to_user` answers for a NULL attribute,
    // so an ENOTTY makes the whole read fatal - libinput gives up on every
    // node on the machine with "failed to create input device", and a
    // compositor comes up with a screen and no keyboard.
    let mut text = [0u8; 64];
    let got = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x07, text.len()),
            text.as_mut_ptr() as *mut c_void,
        )
    };
    check_eq(&mut r, "EVIOCGPHYS answers ENOENT, not ENOTTY", got, ENOENT);
    let got = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x08, text.len()),
            text.as_mut_ptr() as *mut c_void,
        )
    };
    check_eq(&mut r, "EVIOCGUNIQ answers ENOENT, not ENOTTY", got, ENOENT);

    // EVIOCGABS on a keyboard axis: EINVAL, the device has no absolute axis.
    let mut abs = [0u8; 24];
    let got = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x40, abs.len()),
            abs.as_mut_ptr() as *mut c_void,
        )
    };
    check_eq(&mut r, "EVIOCGABS on an absent axis is EINVAL", got, EINVAL);

    // EVIOCSFF / EVIOCRMFF / EVIOCSKEYCODE / EVIOCGKEYCODE - the per-device
    // capability refusals. Measured on real Linux: an evdev node with no
    // force-feedback answers `ENOSYS` to the FF requests (evdev keys them on
    // `test_bit(EV_FF)`), and a node with no per-device keycodes array answers
    // `EINVAL` to the keycode get and set. This machine's virtio nodes carry
    // neither, so those are the answers here. The counterfactual is the point:
    // the kernel used to return `0` to all four - success for an effect queued
    // and a key remapped that nothing actually stored, the §4.2 violation. The
    // FF refusal rests on the device's own `EV_FF` picture (the code bitmap,
    // empty below); EVIOCGKEYCODE and its `_V2` share command number 0x04 and
    // both take the same refusal, as do the two keymap-set spellings.
    let mut ffbit = [0u8; 8];
    let _ = eviocgbit(fd, 0x15 /* EV_FF */, &mut ffbit);
    check_true(
        &mut r,
        "the keyboard advertises no EV_FF codes (why FF is refused)",
        ffbit.iter().all(|&b| b == 0),
    );
    let mut ff_effect = [0u8; 48];
    check_eq(
        &mut r,
        "EVIOCSFF on a device with no force-feedback is ENOSYS",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_WRITE, 0x80, ff_effect.len()),
                ff_effect.as_mut_ptr() as *mut c_void,
            )
        },
        ENOSYS,
    );
    let mut effect_id = 0i32;
    check_eq(
        &mut r,
        "EVIOCRMFF is ENOSYS too",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_WRITE, 0x81, 4),
                &mut effect_id as *mut i32 as *mut c_void,
            )
        },
        ENOSYS,
    );
    let mut keypair = [0u32; 2];
    check_eq(
        &mut r,
        "EVIOCSKEYCODE on a node with no keycodes array is EINVAL",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_WRITE, 0x04, 8),
                keypair.as_mut_ptr() as *mut c_void,
            )
        },
        EINVAL,
    );
    check_eq(
        &mut r,
        "EVIOCGKEYCODE is EINVAL, not a zeroed success",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_READ, 0x04, 8),
                keypair.as_mut_ptr() as *mut c_void,
            )
        },
        EINVAL,
    );

    // EVIOCGREP / EVIOCSREP: the key repeat, now a stored per-device value
    // guarded on EV_REP (measured Linux: a device that advertises it returns
    // its real (delay, period), one that does not answers ENOSYS - never the
    // old zeroed-buffer success). The set-then-get round-trip is the
    // counterfactual: previously EVIOCSREP fell through to ENOTTY and
    // EVIOCGREP answered give(&[]) leaving the caller's two ints untouched,
    // so nothing here could store or report a value.
    const EV_REP_BIT: usize = 0x14; // 20, byte 2 bit 4 of the type bitmap
    let advertises_rep = types[EV_REP_BIT / 8] & (1 << (EV_REP_BIT % 8)) != 0;
    if advertises_rep {
        let mut rep = [0u32; 2];
        check_eq(
            &mut r,
            "EVIOCGREP on an EV_REP device succeeds",
            unsafe { ioctl(fd, ioc(IOC_READ, 0x03, 8), rep.as_mut_ptr() as *mut c_void) },
            0,
        );
        check_true(
            &mut r,
            "and reports a real (delay, period)",
            rep[0] > 0 && rep[1] > 0,
        );
        let set = [600u32, 50u32];
        check_eq(
            &mut r,
            "EVIOCSREP stores the repeat",
            unsafe { ioctl(fd, ioc(IOC_WRITE, 0x03, 8), set.as_ptr() as *mut c_void) },
            0,
        );
        let mut back = [0u32; 2];
        check_eq(
            &mut r,
            "and EVIOCGREP reads it back",
            unsafe { ioctl(fd, ioc(IOC_READ, 0x03, 8), back.as_mut_ptr() as *mut c_void) },
            0,
        );
        check_true(
            &mut r,
            "the round-trip carries what was set",
            back[0] == 600 && back[1] == 50,
        );
    } else {
        let mut rep = [0u32; 2];
        check_eq(
            &mut r,
            "EVIOCGREP on a device without EV_REP is ENOSYS",
            unsafe { ioctl(fd, ioc(IOC_READ, 0x03, 8), rep.as_mut_ptr() as *mut c_void) },
            ENOSYS,
        );
    }

    // EVIOCGKEY/LED/SND/SW: the pressed/switch-state getters. Measured on real
    // Linux: each copies `min(len, bitmap_bytes)` current-state bytes AND
    // returns that count (KEY bitmap = 96, LED/SND/SW = 8) - never a bare 0 with
    // nothing written. Nothing is held down during a sweep, so the picture is
    // zeros of that length. The old arm answered `give(&[])`: success (0) with
    // the caller's buffer left as-is. Filling the buffers with a sentinel and
    // requiring the count plus the overwrite is the counterfactual.
    let mut keystate = [0xAAu8; 128];
    let got = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x18, keystate.len()),
            keystate.as_mut_ptr() as *mut c_void,
        )
    };
    check_eq(
        &mut r,
        "EVIOCGKEY returns the key bitmap size (96), not a bare 0",
        got,
        96,
    );
    check_true(
        &mut r,
        "and the key state reads as all-released (zero), not the sentinel",
        keystate[..96].iter().all(|&b| b == 0),
    );
    // A buffer shorter than the bitmap is filled short - the returned count is
    // `min(len, bitmap)`, which is how a caller sizes its own read.
    let mut short = [0xAAu8; 8];
    let short_len = short.len();
    check_eq(
        &mut r,
        "a short EVIOCGKEY buffer is filled to its own length",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_READ, 0x18, short_len),
                short.as_mut_ptr() as *mut c_void,
            )
        },
        8,
    );
    let mut sw = [0xAAu8; 64];
    let sw_len = sw.len();
    check_eq(
        &mut r,
        "EVIOCGSW returns its 8-byte switch bitmap",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_READ, 0x1b, sw_len),
                sw.as_mut_ptr() as *mut c_void,
            )
        },
        8,
    );
    // EVIOCGMTSLOTS: measured EINVAL on a node without multi-touch, and no evdev
    // node here is a multitouch device. The old arm answered 0 (give(&[])).
    let mut mt = [0u8; 8];
    let mt_len = mt.len();
    check_eq(
        &mut r,
        "EVIOCGMTSLOTS on a non-multitouch node is EINVAL",
        unsafe {
            ioctl(
                fd,
                ioc(IOC_READ, 0x0a, mt_len),
                mt.as_mut_ptr() as *mut c_void,
            )
        },
        EINVAL,
    );

    // EVIOCGRAB - the seat claim, and who it belongs to. A grab is one
    // descriptor's claim on the device, so a second *independent* open of the
    // same node contends with it (`EBUSY`, the answer a compositor must be able
    // to tell its caller), while the claimant asking again is just holding it.
    // And the claim dies with the descriptor that made it, so a compositor that
    // was killed does not leave the keyboard unclaimable.
    let on: i32 = 1;
    let off: i32 = 0;
    let grab = |fd: i32, v: &i32| unsafe {
        ioctl(fd, ioc(IOC_WRITE, 0x90, 4), v as *const i32 as *mut c_void)
    };
    check_eq(&mut r, "EVIOCGRAB claims the device", grab(fd, &on), 0);
    let other = match rustix::fs::open(&kbd_path, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => {
            r.check("a second open() of the node succeeds", true);
            fd
        }
        Err(e) => {
            r.check(
                &format!("a second open() of the node succeeds ({e:?})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    check_eq(
        &mut r,
        "a second descriptor is refused the same device",
        grab(other.as_raw_fd(), &on),
        EBUSY,
    );
    check_eq(&mut r, "the holder may claim it again", grab(fd, &on), 0);
    check_eq(&mut r, "the holder releases", grab(fd, &off), 0);
    check_eq(
        &mut r,
        "and the other descriptor takes it",
        grab(other.as_raw_fd(), &on),
        0,
    );
    // Closing the claimant, with no release asked for, is a release.
    drop(other);
    check_eq(
        &mut r,
        "a grab dies with the descriptor that took it",
        grab(fd, &on),
        0,
    );
    let _ = grab(fd, &off);

    // EVIOCGNAME still returns the device's own name, and answers with its
    // length *without* the NUL a C string ends with - `strlcpy`'s contract, and
    // the count a caller sizes its own buffer from. A device whose name carries
    // a terminator of its own (the virtio config's) must not count that either:
    // the answer is where the string ends, which is the first NUL in the buffer.
    let mut name = [0u8; 64];
    let n = unsafe {
        ioctl(
            fd,
            ioc(IOC_READ, 0x06, name.len()),
            name.as_mut_ptr() as *mut c_void,
        )
    };
    check_true(&mut r, "EVIOCGNAME returns a non-empty name", n > 0);
    let ends = name.iter().position(|&b| b == 0).unwrap_or(name.len()) as isize;
    check_eq(
        &mut r,
        "EVIOCGNAME's answer is the name's length without the NUL",
        n,
        ends,
    );

    // --- the read a compositor's main loop actually makes ---
    //
    // `libinput` opens every evdev node with `O_NONBLOCK` and pumps it from
    // `epoll`, so an empty read has to answer `EAGAIN` and leave the caller
    // running. This is the fourth of the gaps that stood between udev and a
    // compositor's keyboard, and the one the other three hid: with the sysfs
    // paths, the `phys`/`uniq` answer and epoll's hang-up in place, weston
    // created `event0`, logged it as a keyboard, and then said nothing more -
    // not the pointer, not a failure - its event loop parked on a read Linux
    // would have answered `EAGAIN`, with nobody left to handle the input it was
    // waiting for. A node that answers the data case and stops at the empty one
    // is a node no event-driven reader can pump.
    //
    // Nothing presses this machine's keyboard during a sweep, so the queue is
    // empty by construction and the answer is the flag's, not the device's.
    // Both routes into the flag are pinned, because a node that honours only
    // one of them works for some callers and not others: the `open` that was
    // born with it, and `fcntl(F_SETFL)` afterwards - the two paths that meet in
    // the same `sync_nonblocking`.
    let mut spare = [0u8; RECORD];
    let nb = rustix::fs::open(&kbd_path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty());
    check_true(&mut r, "an evdev node opens O_NONBLOCK", nb.is_ok());
    if let Ok(nb) = nb {
        check_eq(
            &mut r,
            "an empty nonblocking evdev read answers EAGAIN, not a park",
            read(&nb, &mut spare),
            EAGAIN,
        );
    }
    let blocking = rustix::fs::open(&kbd_path, OFlags::RDONLY, Mode::empty());
    check_true(&mut r, "an evdev node opens blocking", blocking.is_ok());
    if let Ok(blocking) = blocking {
        let flags = rustix::fs::fcntl_getfl(&blocking);
        check_true(
            &mut r,
            "and reports no O_NONBLOCK of its own",
            flags.is_ok_and(|f| !f.contains(OFlags::NONBLOCK)),
        );
        if let Ok(flags) = flags {
            check_eq(
                &mut r,
                "F_SETFL sets the bit on an evdev descriptor",
                match rustix::fs::fcntl_setfl(&blocking, flags | OFlags::NONBLOCK) {
                    Ok(()) => 0,
                    Err(e) => -(e.raw_os_error() as isize),
                },
                0,
            );
            check_eq(
                &mut r,
                "and the empty read is EAGAIN through that route too",
                read(&blocking, &mut spare),
                EAGAIN,
            );
        }
    }

    drop(kbd);

    // --- device discovery (what udev/libinput actually enumerate through) ---
    // The node now lives under /dev/input, not the /dev root.
    let node = rustix::fs::open("/dev/input/event0", OFlags::RDONLY, Mode::empty());
    check_true(&mut r, "/dev/input/event0 exists", node.is_ok());
    drop(node);
    // /sys/class/input/event0/dev carries the major:minor the node answers to,
    // which is the pairing udev matches a uevent against.
    let mut numbers = [0u8; 16];
    let mut numbers_len = 0usize;
    let sdev = rustix::fs::open("/sys/class/input/event0/dev", OFlags::RDONLY, Mode::empty());
    check_true(
        &mut r,
        "/sys/class/input/event0/dev is readable",
        sdev.is_ok(),
    );
    if let Ok(sdev) = sdev {
        let mut content = [0u8; 16];
        let n = read(&sdev, &mut content);
        check_true(
            &mut r,
            "its dev number is the input major (13:...)",
            n >= 3 && &content[..3] == b"13:",
        );
        if n > 0 {
            // The attribute ends in a newline; the number is the part before
            // it, because that is the name the reverse index carries.
            let mut end = n as usize;
            while end > 0 && (content[end - 1] == b'\n' || content[end - 1] == b'\r') {
                end -= 1;
            }
            numbers[..end].copy_from_slice(&content[..end]);
            numbers_len = end;
        }
    }

    // `/sys/dev/char/M:m`: how a *number* becomes a device, and the check
    // libinput makes before it will take one. A compositor holds an open
    // descriptor and no path, so `evdev_device_have_same_syspath` re-derives
    // the device from that descriptor's devnum and refuses it unless the path
    // it lands on is, string for string, the one udev handed it. `libudev`
    // resolves exactly *one* hop of this link, so the target has to be the
    // device tree's own path (`devices/virtual/input/input0/event0`) and not
    // another link to it (`class/input/event0`): with the class-shaped target
    // the two answers disagreed, the keyboard was dropped without a word, and
    // weston reported "failed to create input device" for a node udev had
    // tagged perfectly. Pinned as the rule rather than as the spelling - the
    // number's link and the class link must resolve to the same string, and it
    // must live under `/devices` - so a tree that reorganises is still held to
    // the comparison libinput actually makes.
    if numbers_len > 0 {
        let devnum_path = format!(
            "/sys/dev/char/{}",
            core::str::from_utf8(&numbers[..numbers_len]).unwrap_or("")
        );
        let link = rustix::fs::readlink(&devnum_path, Vec::new()).unwrap_or_default();
        let class_link =
            rustix::fs::readlink("/sys/class/input/event0", Vec::new()).unwrap_or_default();
        let into_devices = !link.is_empty() && link.as_bytes().starts_with(b"../../devices/");
        check_true(
            &mut r,
            "the devnum link resolves into the device tree, not to a class entry",
            into_devices,
        );
        let same = !link.is_empty() && link == class_link;
        check_true(
            &mut r,
            "and to the same path udev knows the device by",
            same,
        );
        if !same {
            println!("  /sys/dev/char link: {}", link.to_string_lossy());
            println!("  /sys/class link:    {}", class_link.to_string_lossy());
        }
    }

    // A pointer device (the tablet) advertises EV_ABS and answers its axis
    // geometry - the path a relative-only keyboard never exercises, and the
    // one libinput needs to size a touchscreen or tablet.
    if let Some(tablet) = &tablet {
        // EVIOCGABS(ABS_X): min/max/... of axis 0, in struct input_absinfo.
        let mut axis = [0i32; 6];
        let got = unsafe {
            ioctl(
                tablet.as_raw_fd(),
                ioc(IOC_READ, 0x40, axis.len() * 4),
                axis.as_mut_ptr() as *mut c_void,
            )
        };
        check_eq(&mut r, "EVIOCGABS(ABS_X) succeeds", got, 0);
        check_true(
            &mut r,
            "the axis carries a real min..max range",
            axis[2] > axis[1],
        );
    }
    check_true(
        &mut r,
        "a pointer/tablet device advertises EV_ABS",
        tablet.is_some(),
    );

    // --- whose events are they? ---
    //
    // The machine has no hands: nothing presses the keyboard during a sweep, so
    // the device the rule is about is one this test makes for itself. `/dev/uinput`
    // is where that happens - the descriptor *is* a device - and it is the same
    // interface Linux gives a program that has to inject input, so what is pinned
    // here is the real thing rather than a test hook.
    let ui = rustix::fs::open("/dev/uinput", OFlags::RDWR, Mode::empty());
    check_true(&mut r, "/dev/uinput opens", ui.is_ok());
    if let Ok(ui) = ui {
        let uifd = ui.as_raw_fd();
        let mut version: u32 = 0;
        check_eq(
            &mut r,
            "UI_GET_VERSION succeeds",
            unsafe {
                ioctl(
                    uifd,
                    UI_GET_VERSION,
                    &mut version as *mut u32 as *mut c_void,
                )
            },
            0,
        );
        check_true(&mut r, "the interface has a version", version > 0);

        let bit = |fd: i32, req: u64, value: i32| unsafe {
            ioctl(fd, req, &value as *const i32 as *mut c_void)
        };
        check_eq(
            &mut r,
            "UI_SET_EVBIT declares EV_SYN",
            bit(uifd, UI_SET_EVBIT, EV_SYN as i32),
            0,
        );
        check_eq(
            &mut r,
            "UI_SET_EVBIT declares EV_KEY",
            bit(uifd, UI_SET_EVBIT, EV_KEY as i32),
            0,
        );
        check_eq(
            &mut r,
            "UI_SET_KEYBIT declares KEY_A",
            bit(uifd, UI_SET_KEYBIT, KEY_A as i32),
            0,
        );
        // A bit the request cannot carry is EINVAL, not silence.
        check_eq(
            &mut r,
            "a key beyond KEY_MAX is EINVAL",
            bit(uifd, UI_SET_KEYBIT, 0x300),
            EINVAL,
        );

        // The name and id the device will report, in `struct uinput_setup`.
        let mut setup = [0u8; UINPUT_SETUP_SIZE];
        for (i, word) in [3u16, 0x1234, 0x5678, 1u16].into_iter().enumerate() {
            setup[i * 2..i * 2 + 2].copy_from_slice(&word.to_ne_bytes());
        }
        let named = b"mpos-uitest";
        setup[8..8 + named.len()].copy_from_slice(named);
        check_eq(
            &mut r,
            "UI_DEV_SETUP succeeds",
            unsafe { ioctl(uifd, UI_DEV_SETUP, setup.as_ptr() as *mut c_void) },
            0,
        );
        // Creating before the interface knows what to call the device is
        // EINVAL, and the interface does know now.
        check_eq(
            &mut r,
            "UI_DEV_CREATE succeeds",
            unsafe { ioctl(uifd, UI_DEV_CREATE, core::ptr::null_mut()) },
            0,
        );

        // Where did it appear? The machine's own devices took event0 and
        // event1, so the new one is the next free number; the name is what
        // identifies it, which is how a program finds its own device without
        // being told.
        let mut synth: Option<(usize, OwnedFd)> = None;
        for index in 0..8usize {
            let path = format!("/dev/input/event{index}");
            let Ok(candidate) = rustix::fs::open(&path, OFlags::RDONLY, Mode::empty()) else {
                continue;
            };
            let mut name = [0u8; 64];
            let n = unsafe {
                ioctl(
                    candidate.as_raw_fd(),
                    ioc(IOC_READ, 0x06, name.len()),
                    name.as_mut_ptr() as *mut c_void,
                )
            };
            // The name is what identifies it: the number is whatever the
            // machine happened to leave free.
            if n as usize == named.len() && &name[..n as usize] == named {
                synth = Some((index, candidate));
                break;
            }
        }
        check_true(
            &mut r,
            "the device appears as /dev/input/eventN and reports its own name",
            synth.is_some(),
        );

        if let Some((which, synth)) = synth {
            let synth_path = format!("/dev/input/event{which}");
            // The second descriptor comes through the same path the search
            // reached first, so the test does not depend on which number the
            // machine happened to leave free.
            let second = match rustix::fs::open(&synth_path, OFlags::RDONLY, Mode::empty()) {
                Ok(fd) => {
                    r.check("a second reader can open it", true);
                    fd
                }
                Err(e) => {
                    r.check(&format!("a second reader can open it ({e:?})"), false);
                    println!("FAILED");
                    std::process::exit(1);
                }
            };

            // A keystroke, reported by the device the test made.
            let frame = keystroke(1);
            check_eq(
                &mut r,
                "a write of whole records is accepted",
                write(&ui, &frame),
                frame.len() as isize,
            );
            check_true(
                &mut r,
                "and a write that is not whole records is EINVAL",
                write(&ui, &frame[..frame.len() - 1]) == EINVAL,
            );

            let mut buf = [0u8; RECORD * 2];
            check_true(&mut r, "the reader sees the record", readable(&synth));
            let n = read(&synth, &mut buf);
            check_eq(
                &mut r,
                "a read returns the whole frame",
                n,
                (RECORD * 2) as isize,
            );
            check_eq(
                &mut r,
                "the record carries the event the test reported",
                u16::from_ne_bytes([buf[16], buf[17]]) as isize,
                EV_KEY as isize,
            );
            check_eq(
                &mut r,
                "its code and value are the ones written",
                (u16::from_ne_bytes([buf[18], buf[19]]) as isize) << 16
                    | i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]) as isize,
                ((KEY_A as isize) << 16) | 1,
            );

            // The bug this test exists for: a second reader of one node must
            // still have its own copy of what the first one read. Under one
            // shared queue the read above left it with nothing.
            check_true(
                &mut r,
                "the second reader still has its own copy",
                readable(&second),
            );
            check_true(
                &mut r,
                "while the first has run out of its own",
                !readable(&synth),
            );
            let n = read(&second, &mut buf);
            check_eq(
                &mut r,
                "and reads the same frame for itself",
                n,
                (RECORD * 2) as isize,
            );

            // The grab: the device's events are the grabber's, and a reader that
            // is not it is told nothing - not the records, and not a readable
            // answer either.
            check_eq(
                &mut r,
                "the reader claims the device",
                grab(synth.as_raw_fd(), &on),
                0,
            );
            let more = keystroke(1);
            check_eq(
                &mut r,
                "the owner writes through the grabbed device",
                write(&ui, &more),
                more.len() as isize,
            );
            check_true(&mut r, "the owner is told it is readable", readable(&synth));
            check_true(
                &mut r,
                "a non-owner of a grabbed device is not",
                !readable(&second),
            );
            let n = read(&synth, &mut buf);
            check_eq(
                &mut r,
                "the owner reads its own event",
                n,
                (RECORD * 2) as isize,
            );

            check_eq(
                &mut r,
                "the owner releases",
                grab(synth.as_raw_fd(), &off),
                0,
            );
            // What arrived under the grab was the grabber's alone, so the other
            // reader does not get it when the grab ends.
            check_true(
                &mut r,
                "a release does not hand the window over",
                !readable(&second),
            );
            let after = keystroke(0);
            let _ = write(&ui, &after);
            check_true(
                &mut r,
                "but the next record is the other reader's",
                readable(&second),
            );

            // --- EVIOCREVOKE: take the device back from a reader ---
            //
            // The seat mechanism. Per Linux's own `evdev.c` (`case EVIOCREVOKE:
            // if (p) return -EINVAL; else return evdev_revoke(...)`), the
            // argument must be NULL - any non-NULL pointer is `EINVAL` - and the
            // request revokes this description. It is one-way: there is no ioctl
            // to give the descriptor back (the caller closes and reopens), so a
            // revoked description calling `EVIOCREVOKE(NULL)` again is just the
            // idempotent revoke (`0`), never a restore. Once revoked, every read
            // and every other ioctl on the descriptor answers `ENODEV` - only
            // `EVIOCREVOKE` itself still gets through. A revoked read returns at
            // once (not a park), so this is safe on a blocking descriptor.
            //
            // The old pins here asserted the wrong shape - that arg `1` revoked
            // and arg `0` *restored* - which a correct `ioctl(fd, EVIOCREVOKE,
            // NULL)` (arg `0`) contradicts: on Linux that revokes, not restores.
            // The test changed with the source measurement, not the code (rule 1's
            // exception), because the pin had encoded a divergence from Linux.
            let revoke = |fd: i32, arg: usize| unsafe {
                ioctl(fd, ioc(IOC_WRITE, 0x91, 4), arg as *mut c_void)
            };
            // The revoke target is `second` (a reader of its own that the rest
            // of the sweep no longer needs un-revoked); the destroyed-device
            // end-of-input check below still runs on `synth`, which this leaves
            // readable. Revocation is one-way and per-client, exactly as Linux.
            check_eq(
                &mut r,
                "EVIOCREVOKE(NULL) revokes the descriptor",
                revoke(second.as_raw_fd(), 0),
                0,
            );
            let mut dead = [0u8; RECORD];
            check_eq(
                &mut r,
                "a read of a revoked descriptor is ENODEV",
                read(&second, &mut dead),
                ENODEV,
            );
            let mut v = 0i32;
            check_eq(
                &mut r,
                "and any other ioctl on it is ENODEV too",
                unsafe {
                    ioctl(
                        second.as_raw_fd(),
                        ioc(IOC_READ, 0x01, 4),
                        &mut v as *mut i32 as *mut c_void,
                    )
                },
                ENODEV,
            );
            // A non-NULL arg is refused - the `_IOW(..., int)` shape takes no
            // value, Linux demands NULL.
            check_eq(
                &mut r,
                "EVIOCREVOKE with a non-NULL argument is EINVAL",
                revoke(second.as_raw_fd(), 1),
                EINVAL,
            );
            // And it is one-way: the descriptor stays revoked (the idempotent
            // NULL revoke answers `0`; a later read is still `ENODEV`). There is
            // no restore - that is exactly the difference from the old model.
            check_eq(
                &mut r,
                "EVIOCREVOKE(NULL) again is an idempotent 0, not a restore",
                revoke(second.as_raw_fd(), 0),
                0,
            );
            let mut v2 = 0i32;
            check_eq(
                &mut r,
                "the descriptor is still revoked - there is no reverse ioctl",
                unsafe {
                    ioctl(
                        second.as_raw_fd(),
                        ioc(IOC_READ, 0x01, 4),
                        &mut v2 as *mut i32 as *mut c_void,
                    )
                },
                ENODEV,
            );

            // Destroying the device takes it away from the name and ends the
            // stream for everyone still holding a descriptor to it.
            check_eq(
                &mut r,
                "UI_DEV_DESTROY succeeds",
                unsafe { ioctl(uifd, UI_DEV_DESTROY, core::ptr::null_mut()) },
                0,
            );
            check_true(
                &mut r,
                "the node is gone from /dev/input",
                rustix::fs::open(&synth_path, OFlags::RDONLY, Mode::empty()).is_err(),
            );
            let mut tail = [0u8; RECORD * 2];
            check_eq(
                &mut r,
                "and a reader still holding one is at the end of input",
                read(&synth, &mut tail),
                0,
            );

            drop(synth);
            drop(second);
        }
    }

    r.finish();
}
