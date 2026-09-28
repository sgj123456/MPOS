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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const EV_SYN: usize = 0x00;
const EV_KEY: usize = 0x01;

/// `poll(2)`'s readable bit, as `poll.h` spells it.
const POLLIN: i16 = 0x001;

/// `_IOC(dir, 'E', nr, size)`: the evdev request word, type byte `E`.
const fn ioc(dir: usize, nr: usize, size: usize) -> usize {
    (dir << 30) | (size << 16) | ((b'E' as usize) << 8) | nr
}
const IOC_READ: usize = 2;
const IOC_WRITE: usize = 1;

/// The same `_IOC` spelling for the `'U'` family `/dev/uinput` lives in: its
/// requests carry the type byte `U` where evdev's carry `E`, and everything
/// else about the word - direction, size, number - is spelled the same way.
const fn uio(dir: usize, nr: usize, size: usize) -> usize {
    (dir << 30) | (size << 16) | ((b'U' as usize) << 8) | nr
}

/// `UI_DEV_CREATE` / `UI_DEV_DESTROY`: `_IO('U', 1)` and `_IO('U', 2)`.
const UI_DEV_CREATE: usize = uio(0, 1, 0);
const UI_DEV_DESTROY: usize = uio(0, 2, 0);
/// `UI_DEV_SETUP`: `_IOW('U', 3, struct uinput_setup)`, 92 bytes of it.
const UI_DEV_SETUP: usize = uio(IOC_WRITE, 3, UINPUT_SETUP_SIZE);
/// `UI_GET_VERSION`: `_IOR('U', 45, unsigned int)`.
const UI_GET_VERSION: usize = uio(IOC_READ, 45, 4);
/// The bit-setters, `_IOW('U', n, int)`.
const UI_SET_EVBIT: usize = uio(IOC_WRITE, 100, 4);
const UI_SET_KEYBIT: usize = uio(IOC_WRITE, 101, 4);

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

fn eviocgbit(fd: i32, evtype: usize, out: &mut [u8]) -> isize {
    let req = ioc(IOC_READ, 0x20 + evtype, out.len());
    lin::ioctl(fd, req, out.as_mut_ptr() as usize)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-evdev\n");
    let mut failed = false;

    // The node the boot's virtio keyboard lives at. Stage 2 moves it under
    // /dev/input; accept either so this pins the ioctl surface, not the path.
    let mut fd = lin::openat(lin::AT_FDCWD, b"/dev/input/event0\0", lin::O_RDONLY, 0) as i32;
    if fd < 0 {
        fd = lin::openat(lin::AT_FDCWD, b"/dev/event0\0", lin::O_RDONLY, 0) as i32;
    }
    lin::check_true(&mut failed, b"an evdev node opens", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // EVIOCGVERSION: the evdev ABI, always 1.
    let mut version: i32 = 0;
    let r = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x01, 4),
        &mut version as *mut i32 as usize,
    );
    lin::check_eq(&mut failed, b"EVIOCGVERSION succeeds", r, 0);
    lin::check_eq(&mut failed, b"the evdev version is 1", version as isize, 1);

    // EVIOCGBIT(0): which event types exist. A keyboard sets EV_KEY (bit 1).
    let mut types = [0u8; 8];
    let r = eviocgbit(fd, EV_SYN, &mut types);
    lin::check_eq(&mut failed, b"EVIOCGBIT(0) succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the type bitmap reports EV_KEY",
        types[0] & (1 << EV_KEY) != 0,
    );

    // EVIOCGBIT(EV_KEY): the set of keys - non-empty for a real keyboard.
    let mut keys = [0u8; 128];
    let r = eviocgbit(fd, EV_KEY, &mut keys);
    lin::check_eq(&mut failed, b"EVIOCGBIT(EV_KEY) succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the key code bitmap is not empty",
        keys.iter().any(|&b| b != 0),
    );

    // EVIOCGID: a caller can read the 8-byte struct back without error.
    let mut id = [0u8; 8];
    let r = lin::ioctl(fd, ioc(IOC_READ, 0x02, 8), id.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"EVIOCGID succeeds", r, 0);

    // EVIOCGPHYS and EVIOCGUNIQ: a virtio input device has neither string,
    // and the answer has to be ENOENT. It may not simply be "unsupported":
    // libevdev's `libevdev_set_fd` tolerates a failure here *only* for
    // ENOENT, exactly as Linux's `str_to_user` answers for a NULL attribute,
    // so an ENOTTY makes the whole read fatal - libinput gives up on every
    // node on the machine with "failed to create input device", and a
    // compositor comes up with a screen and no keyboard.
    let mut text = [0u8; 64];
    let r = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x07, text.len()),
        text.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"EVIOCGPHYS answers ENOENT, not ENOTTY",
        r,
        lin::ENOENT,
    );
    let r = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x08, text.len()),
        text.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"EVIOCGUNIQ answers ENOENT, not ENOTTY",
        r,
        lin::ENOENT,
    );

    // EVIOCGABS on a keyboard axis: EINVAL, the device has no absolute axis.
    let mut abs = [0u8; 24];
    let r = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x40, abs.len()),
        abs.as_mut_ptr() as usize,
    );
    lin::check_eq(
        &mut failed,
        b"EVIOCGABS on an absent axis is EINVAL",
        r,
        lin::EINVAL,
    );

    // EVIOCGRAB - the seat claim, and who it belongs to. A grab is one
    // descriptor's claim on the device, so a second *independent* open of the
    // same node contends with it (`EBUSY`, the answer a compositor must be able
    // to tell its caller), while the claimant asking again is just holding it.
    // And the claim dies with the descriptor that made it, so a compositor that
    // was killed does not leave the keyboard unclaimable.
    let on: i32 = 1;
    let off: i32 = 0;
    let grab = |fd: i32, v: &i32| lin::ioctl(fd, ioc(IOC_WRITE, 0x90, 4), v as *const i32 as usize);
    lin::check_eq(
        &mut failed,
        b"EVIOCGRAB claims the device",
        grab(fd, &on),
        0,
    );
    let other = lin::openat(lin::AT_FDCWD, b"/dev/input/event0\0", lin::O_RDONLY, 0) as i32;
    lin::check_true(
        &mut failed,
        b"a second open() of the node succeeds",
        other >= 0,
    );
    lin::check_eq(
        &mut failed,
        b"a second descriptor is refused the same device",
        grab(other, &on),
        lin::EBUSY,
    );
    lin::check_eq(
        &mut failed,
        b"the holder may claim it again",
        grab(fd, &on),
        0,
    );
    lin::check_eq(&mut failed, b"the holder releases", grab(fd, &off), 0);
    lin::check_eq(
        &mut failed,
        b"and the other descriptor takes it",
        grab(other, &on),
        0,
    );
    // Closing the claimant, with no release asked for, is a release.
    let _ = lin::close(other);
    lin::check_eq(
        &mut failed,
        b"a grab dies with the descriptor that took it",
        grab(fd, &on),
        0,
    );
    let _ = lin::ioctl(fd, ioc(IOC_WRITE, 0x90, 4), &off as *const i32 as usize);

    // EVIOCGNAME still returns the device's own name, and answers with its
    // length *without* the NUL a C string ends with - `strlcpy`'s contract, and
    // the count a caller sizes its own buffer from. A device whose name carries
    // a terminator of its own (the virtio config's) must not count that either:
    // the answer is where the string ends, which is the first NUL in the buffer.
    let mut name = [0u8; 64];
    let n = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x06, name.len()),
        name.as_mut_ptr() as usize,
    );
    lin::check_true(&mut failed, b"EVIOCGNAME returns a non-empty name", n > 0);
    let ends = name.iter().position(|&b| b == 0).unwrap_or(name.len()) as isize;
    lin::check_eq(
        &mut failed,
        b"EVIOCGNAME's answer is the name's length without the NUL",
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
    let nb = lin::openat(
        lin::AT_FDCWD,
        b"/dev/input/event0\0",
        lin::O_RDONLY | lin::O_NONBLOCK,
        0,
    ) as i32;
    lin::check_true(&mut failed, b"an evdev node opens O_NONBLOCK", nb >= 0);
    if nb >= 0 {
        lin::check_eq(
            &mut failed,
            b"an empty nonblocking evdev read answers EAGAIN, not a park",
            lin::read(nb, &mut spare),
            lin::EAGAIN,
        );
        let _ = lin::close(nb);
    }
    let blocking = lin::openat(lin::AT_FDCWD, b"/dev/input/event0\0", lin::O_RDONLY, 0) as i32;
    lin::check_true(&mut failed, b"an evdev node opens blocking", blocking >= 0);
    if blocking >= 0 {
        let flags = lin::fcntl(blocking, lin::F_GETFL, 0);
        lin::check_true(
            &mut failed,
            b"and reports no O_NONBLOCK of its own",
            flags >= 0 && flags as usize & lin::O_NONBLOCK == 0,
        );
        if flags >= 0 {
            lin::check_eq(
                &mut failed,
                b"F_SETFL sets the bit on an evdev descriptor",
                lin::fcntl(blocking, lin::F_SETFL, flags as usize | lin::O_NONBLOCK),
                0,
            );
            lin::check_eq(
                &mut failed,
                b"and the empty read is EAGAIN through that route too",
                lin::read(blocking, &mut spare),
                lin::EAGAIN,
            );
        }
        let _ = lin::close(blocking);
    }

    let _ = lin::close(fd);

    // --- device discovery (what udev/libinput actually enumerate through) ---
    // The node now lives under /dev/input, not the /dev root.
    let node = lin::openat(lin::AT_FDCWD, b"/dev/input/event0\0", lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/dev/input/event0 exists", node >= 0);
    if node >= 0 {
        let _ = lin::close(node as i32);
    }
    // /sys/class/input/event0/dev carries the major:minor the node answers to,
    // which is the pairing udev matches a uevent against.
    let mut numbers = [0u8; 16];
    let mut numbers_len = 0usize;
    let sdev = lin::openat(
        lin::AT_FDCWD,
        b"/sys/class/input/event0/dev\0",
        lin::O_RDONLY,
        0,
    ) as i32;
    lin::check_true(
        &mut failed,
        b"/sys/class/input/event0/dev is readable",
        sdev >= 0,
    );
    if sdev >= 0 {
        let mut content = [0u8; 16];
        let n = lin::read(sdev, &mut content);
        let _ = lin::close(sdev);
        lin::check_true(
            &mut failed,
            b"its dev number is the input major (13:...)",
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
        let prefix = b"/sys/dev/char/";
        let mut path = [0u8; 64];
        path[..prefix.len()].copy_from_slice(prefix);
        path[prefix.len()..prefix.len() + numbers_len].copy_from_slice(&numbers[..numbers_len]);
        path[prefix.len() + numbers_len] = 0;
        let path = &path[..prefix.len() + numbers_len + 1];

        let mut link = [0u8; 128];
        let ln = lin::readlink(path, &mut link);
        let mut class_link = [0u8; 128];
        let cn = lin::readlink(b"/sys/class/input/event0\0", &mut class_link);
        let into_devices = ln > 0 && link[..ln as usize].starts_with(b"../../devices/");
        lin::check_true(
            &mut failed,
            b"the devnum link resolves into the device tree, not to a class entry",
            into_devices,
        );
        let same = ln > 0 && ln == cn && link[..ln as usize] == class_link[..cn as usize];
        lin::check_true(
            &mut failed,
            b"and to the same path udev knows the device by",
            same,
        );
        if !same {
            lin::puts(b"  /sys/dev/char link: ");
            lin::puts(&link[..ln.max(0) as usize]);
            lin::puts(b"\n  /sys/class link:    ");
            lin::puts(&class_link[..cn.max(0) as usize]);
            lin::puts(b"\n");
        }
    }

    // A pointer device (the tablet) advertises EV_ABS and answers its axis
    // geometry - the path a relative-only keyboard never exercises, and the
    // one libinput needs to size a touchscreen or tablet.
    let mut saw_abs = false;
    for path in ["/dev/input/event0\0", "/dev/input/event1\0"] {
        let p = lin::openat(lin::AT_FDCWD, path.as_bytes(), lin::O_RDONLY, 0) as i32;
        if p < 0 {
            continue;
        }
        let mut abs_bits = [0u8; 32];
        let _ = eviocgbit(p, 0x03 /* EV_ABS */, &mut abs_bits);
        if abs_bits.iter().any(|&b| b != 0) {
            saw_abs = true;
            // EVIOCGABS(ABS_X): min/max/... of axis 0, in struct input_absinfo.
            let mut axis = [0i32; 6];
            let r = lin::ioctl(
                p,
                ioc(IOC_READ, 0x40, axis.len() * 4),
                axis.as_mut_ptr() as usize,
            );
            lin::check_eq(&mut failed, b"EVIOCGABS(ABS_X) succeeds", r, 0);
            lin::check_true(
                &mut failed,
                b"the axis carries a real min..max range",
                axis[2] > axis[1],
            );
        }
        let _ = lin::close(p);
    }
    lin::check_true(
        &mut failed,
        b"a pointer/tablet device advertises EV_ABS",
        saw_abs,
    );

    // --- whose events are they? ---
    //
    // The machine has no hands: nothing presses the keyboard during a sweep, so
    // the device the rule is about is one this test makes for itself. `/dev/uinput`
    // is where that happens - the descriptor *is* a device - and it is the same
    // interface Linux gives a program that has to inject input, so what is pinned
    // here is the real thing rather than a test hook.
    let ui = lin::openat(lin::AT_FDCWD, b"/dev/uinput\0", lin::O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/uinput opens", ui >= 0);
    if ui >= 0 {
        let mut version: u32 = 0;
        lin::check_eq(
            &mut failed,
            b"UI_GET_VERSION succeeds",
            lin::ioctl(ui, UI_GET_VERSION, &mut version as *mut u32 as usize),
            0,
        );
        lin::check_true(&mut failed, b"the interface has a version", version > 0);

        let bit =
            |fd: i32, req: usize, value: i32| lin::ioctl(fd, req, &value as *const i32 as usize);
        lin::check_eq(
            &mut failed,
            b"UI_SET_EVBIT declares EV_SYN",
            bit(ui, UI_SET_EVBIT, EV_SYN as i32),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"UI_SET_EVBIT declares EV_KEY",
            bit(ui, UI_SET_EVBIT, EV_KEY as i32),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"UI_SET_KEYBIT declares KEY_A",
            bit(ui, UI_SET_KEYBIT, KEY_A as i32),
            0,
        );
        // A bit the request cannot carry is EINVAL, not silence.
        lin::check_eq(
            &mut failed,
            b"a key beyond KEY_MAX is EINVAL",
            bit(ui, UI_SET_KEYBIT, 0x300),
            lin::EINVAL,
        );

        // The name and id the device will report, in `struct uinput_setup`.
        let mut setup = [0u8; UINPUT_SETUP_SIZE];
        for (i, word) in [3u16, 0x1234, 0x5678, 1u16].into_iter().enumerate() {
            setup[i * 2..i * 2 + 2].copy_from_slice(&word.to_ne_bytes());
        }
        let named = b"mpos-uitest";
        setup[8..8 + named.len()].copy_from_slice(named);
        lin::check_eq(
            &mut failed,
            b"UI_DEV_SETUP succeeds",
            lin::ioctl(ui, UI_DEV_SETUP, setup.as_ptr() as usize),
            0,
        );
        // Creating before the interface knows what to call the device is
        // EINVAL, and the interface does know now.
        lin::check_eq(
            &mut failed,
            b"UI_DEV_CREATE succeeds",
            lin::ioctl(ui, UI_DEV_CREATE, 0),
            0,
        );

        // Where did it appear? The machine's own devices took event0 and
        // event1, so the new one is the next free number; the name is what
        // identifies it, which is how a program finds its own device without
        // being told.
        let paths: [&[u8]; 8] = [
            b"/dev/input/event0\0",
            b"/dev/input/event1\0",
            b"/dev/input/event2\0",
            b"/dev/input/event3\0",
            b"/dev/input/event4\0",
            b"/dev/input/event5\0",
            b"/dev/input/event6\0",
            b"/dev/input/event7\0",
        ];
        let mut synth = -1i32;
        let mut which = 0usize;
        for (index, path) in paths.iter().enumerate() {
            let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0) as i32;
            if fd < 0 {
                continue;
            }
            let mut name = [0u8; 64];
            let n = lin::ioctl(
                fd,
                ioc(IOC_READ, 0x06, name.len()),
                name.as_mut_ptr() as usize,
            );
            // The name is what identifies it: the number is whatever the
            // machine happened to leave free.
            if n as usize == named.len() && &name[..n as usize] == named {
                synth = fd;
                which = index;
                break;
            }
            let _ = lin::close(fd);
        }
        lin::check_true(
            &mut failed,
            b"the device appears as /dev/input/eventN and reports its own name",
            synth >= 0,
        );

        if synth >= 0 {
            // The second descriptor comes through the same path the search
            // reached first, so the test does not depend on which number the
            // machine happened to leave free.
            let second = lin::openat(lin::AT_FDCWD, paths[which], lin::O_RDONLY, 0) as i32;
            lin::check_true(&mut failed, b"a second reader can open it", second >= 0);

            let readable = |fd: i32| -> bool {
                let mut fds = [lin::PollFd {
                    fd,
                    events: POLLIN,
                    revents: 0,
                }];
                lin::poll(&mut fds, 0) > 0 && fds[0].revents & POLLIN != 0
            };

            // A keystroke, reported by the device the test made.
            let frame = keystroke(1);
            lin::check_eq(
                &mut failed,
                b"a write of whole records is accepted",
                lin::write(ui, &frame),
                frame.len() as isize,
            );
            lin::check_true(
                &mut failed,
                b"and a write that is not whole records is EINVAL",
                lin::write(ui, &frame[..frame.len() - 1]) == lin::EINVAL,
            );

            let mut buf = [0u8; RECORD * 2];
            lin::check_true(&mut failed, b"the reader sees the record", readable(synth));
            let n = lin::read(synth, &mut buf);
            lin::check_eq(
                &mut failed,
                b"a read returns the whole frame",
                n,
                (RECORD * 2) as isize,
            );
            lin::check_eq(
                &mut failed,
                b"the record carries the event the test reported",
                u16::from_ne_bytes([buf[16], buf[17]]) as isize,
                EV_KEY as isize,
            );
            lin::check_eq(
                &mut failed,
                b"its code and value are the ones written",
                (u16::from_ne_bytes([buf[18], buf[19]]) as isize) << 16
                    | i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]) as isize,
                ((KEY_A as isize) << 16) | 1,
            );

            // The bug this test exists for: a second reader of one node must
            // still have its own copy of what the first one read. Under one
            // shared queue the read above left it with nothing.
            lin::check_true(
                &mut failed,
                b"the second reader still has its own copy",
                readable(second),
            );
            lin::check_true(
                &mut failed,
                b"while the first has run out of its own",
                !readable(synth),
            );
            let n = lin::read(second, &mut buf);
            lin::check_eq(
                &mut failed,
                b"and reads the same frame for itself",
                n,
                (RECORD * 2) as isize,
            );

            // The grab: the device's events are the grabber's, and a reader that
            // is not it is told nothing - not the records, and not a readable
            // answer either.
            lin::check_eq(
                &mut failed,
                b"the reader claims the device",
                grab(synth, &on),
                0,
            );
            let more = keystroke(1);
            lin::check_eq(
                &mut failed,
                b"the owner writes through the grabbed device",
                lin::write(ui, &more),
                more.len() as isize,
            );
            lin::check_true(
                &mut failed,
                b"the owner is told it is readable",
                readable(synth),
            );
            lin::check_true(
                &mut failed,
                b"a non-owner of a grabbed device is not",
                !readable(second),
            );
            let n = lin::read(synth, &mut buf);
            lin::check_eq(
                &mut failed,
                b"the owner reads its own event",
                n,
                (RECORD * 2) as isize,
            );

            lin::check_eq(&mut failed, b"the owner releases", grab(synth, &off), 0);
            // What arrived under the grab was the grabber's alone, so the other
            // reader does not get it when the grab ends.
            lin::check_true(
                &mut failed,
                b"a release does not hand the window over",
                !readable(second),
            );
            let after = keystroke(0);
            lin::write(ui, &after);
            lin::check_true(
                &mut failed,
                b"but the next record is the other reader's",
                readable(second),
            );

            // Destroying the device takes it away from the name and ends the
            // stream for everyone still holding a descriptor to it.
            lin::check_eq(
                &mut failed,
                b"UI_DEV_DESTROY succeeds",
                lin::ioctl(ui, UI_DEV_DESTROY, 0),
                0,
            );
            lin::check_true(
                &mut failed,
                b"the node is gone from /dev/input",
                lin::openat(lin::AT_FDCWD, paths[which], lin::O_RDONLY, 0) < 0,
            );
            let mut tail = [0u8; RECORD * 2];
            lin::check_eq(
                &mut failed,
                b"and a reader still holding one is at the end of input",
                lin::read(synth, &mut tail),
                0,
            );

            let _ = lin::close(synth);
            let _ = lin::close(second);
        }
        let _ = lin::close(ui);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-evdev: panicked\n")
}
