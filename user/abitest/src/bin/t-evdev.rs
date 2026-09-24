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

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const EV_SYN: usize = 0x00;
const EV_KEY: usize = 0x01;

/// `_IOC(dir, 'E', nr, size)`: the evdev request word, type byte `E`.
const fn ioc(dir: usize, nr: usize, size: usize) -> usize {
    (dir << 30) | (size << 16) | (nr << 8) | (b'E' as usize)
}
const IOC_READ: usize = 2;
const IOC_WRITE: usize = 1;

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

    // EVIOCGRAB: the seat claim is accepted.
    let on: i32 = 1;
    let r = lin::ioctl(fd, ioc(IOC_WRITE, 0x90, 4), &on as *const i32 as usize);
    lin::check_eq(&mut failed, b"EVIOCGRAB is accepted", r, 0);

    // EVIOCGNAME still returns the device's own name.
    let mut name = [0u8; 64];
    let n = lin::ioctl(
        fd,
        ioc(IOC_READ, 0x06, name.len()),
        name.as_mut_ptr() as usize,
    );
    lin::check_true(&mut failed, b"EVIOCGNAME returns a non-empty name", n > 0);

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
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-evdev: panicked\n")
}
