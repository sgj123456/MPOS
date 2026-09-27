//! t-uevent - the device-event socket, and what a *blocking* listener is owed.
//!
//! Everything in this machine that finds a device without being told its name
//! twice - udev's daemon and its workers, `udevadm monitor`, and the initramfs's
//! `nlplug-findfs`, which is the one that decides where the root filesystem is -
//! listens on `NETLINK_KOBJECT_UEVENT` and blocks in `recvmsg` between events.
//! That second part is the whole test: a listener that is told `EAGAIN` instead
//! of being made to wait does not go back to sleep, it *spins*, and a udevd that
//! spins never settles into the quiet state where it will answer a signal and
//! exit. Linux parks a blocking netlink read and reserves `EAGAIN` for the
//! caller that asked not to wait, with `MSG_DONTWAIT` or `O_NONBLOCK`.
//!
//! The bytes are pinned too, because their shape is not negotiable: the kernel
//! sends the legacy block - `add@/devices/...` first, then the `KEY=value`
//! entries, each NUL-terminated - and *no* `struct nlmsghdr` in front of it.
//! `lib/kobject_uevent.c` builds the skb that way (`sprintf(scratch, "%s@%s",
//! action, devpath)`), and wrapping the block in a header was tried here once:
//! the listeners that read it directly stopped finding their root device.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const AF_NETLINK: usize = 16;
/// `SOCK_RAW`, the type `libudev` opens the uevent socket as.
const SOCK_RAW: usize = 3;
const NETLINK_KOBJECT_UEVENT: usize = 15;
const MSG_DONTWAIT: usize = 0x40;
const O_WRONLY: usize = 1;
/// The one group kernel-originated events go to: `1 << KOBJ_UEVENT_KERNEL_GROUP`,
/// i.e. group bit 0.
const UEVENT_GROUP_KERNEL: u32 = 1;

/// A `struct sockaddr_nl` wearing the only 16-byte address the harness can
/// pass: family at 0, `nl_pid` at 4, `nl_groups` at 8.
fn nl_addr(groups: u32) -> lin::SockAddrIn {
    let pid = (lin::getpid() as u32).to_le_bytes();
    let grp = groups.to_le_bytes();
    lin::SockAddrIn {
        family: AF_NETLINK as u16,
        port: 0,
        addr: pid,
        zero: [grp[0], grp[1], grp[2], grp[3], 0, 0, 0, 0],
    }
}

/// Open and bind a listener for kernel device events.
fn listen() -> isize {
    let fd = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT);
    if fd < 0 {
        return fd;
    }
    let addr = nl_addr(UEVENT_GROUP_KERNEL);
    if lin::bind(fd as i32, &addr) < 0 {
        return -1;
    }
    fd
}

/// One `recvmsg` into `buf`, with a control buffer big enough for credentials
/// - which is what a real listener offers, and what a kernel that attaches none
/// is detected by.
fn receive(fd: i32, buf: &mut [u8], flags: usize, ctl: &mut [usize; 4]) -> (isize, usize) {
    let mut iov = [lin::IoVec {
        base: buf.as_mut_ptr() as usize,
        len: buf.len(),
    }];
    let mut msg = lin::MsgHdr::default();
    msg.msg_iov = iov.as_mut_ptr() as usize;
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr() as usize;
    msg.msg_controllen = core::mem::size_of_val(ctl);
    let got = lin::recvmsg(fd, &mut msg, flags);
    (got, msg.msg_controllen)
}

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    let _ = lin::write(1, b"t-uevent\n");
    let mut failed = false;

    let fd = listen();
    lin::check_true(
        &mut failed,
        b"a kernel uevent listener opens and binds",
        fd > 0,
    );
    if fd <= 0 {
        lin::done(failed);
    }

    // Nothing has been announced since this socket bound, so the only honest
    // answer to a *non-blocking* read is that there is none yet.
    let mut buf = [0u8; 512];
    let mut ctl = [0usize; 4];
    let (got, _used) = receive(fd as i32, &mut buf, MSG_DONTWAIT, &mut ctl);
    lin::check_eq(
        &mut failed,
        b"an empty queue answers MSG_DONTWAIT with EAGAIN",
        got,
        lin::EAGAIN,
    );

    // Now the call under test. The child announces a device a moment from now;
    // the parent asks for it *without* MSG_DONTWAIT and must therefore be made
    // to wait for it - and what it wakes holding is the legacy block.
    //
    // The announcement is a write to a sysfs `uevent` file, which is how a
    // cold-plug re-sends an event for a device that is already there, so this
    // needs no device to appear: `udevadm trigger` does exactly this, to every
    // device, at boot.
    let pid = lin::fork();
    if pid == 0 {
        let ts = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 300_000_000,
        };
        let mut rem = ts;
        let _ = lin::nanosleep(&ts, &mut rem);
        let path = b"/sys/devices/virtual/drm/card0/uevent\0";
        let f = lin::openat(lin::AT_FDCWD, path, O_WRONLY, 0);
        if f >= 0 {
            let _ = lin::write(f as i32, b"add");
            let _ = lin::close(f as i32);
        }
        lin::exit(0);
    }
    lin::check_true(&mut failed, b"the announcing child started", pid > 0);

    let (got, _used) = receive(fd as i32, &mut buf, 0, &mut ctl);
    lin::check_true(
        &mut failed,
        b"a blocking recvmsg waits for the event and returns it",
        got > 0,
    );
    if got > 0 {
        let n = (got as usize).min(buf.len());
        let block = &buf[..n];
        lin::check_true(
            &mut failed,
            b"the block opens with the action and the device path",
            block
                .windows(b"add@/devices/virtual/drm/card0".len())
                .any(|w| w == b"add@/devices/virtual/drm/card0"),
        );
        lin::check_true(
            &mut failed,
            b"and carries the keys a listener reads",
            block
                .windows(b"ACTION=add".len())
                .any(|w| w == b"ACTION=add")
                && block
                    .windows(b"SUBSYSTEM=".len())
                    .any(|w| w == b"SUBSYSTEM=")
                && block.windows(b"SEQNUM=".len()).any(|w| w == b"SEQNUM="),
        );
        // The legacy shape has no header: the first byte of the first key is
        // the first byte of `add@...`, so a kernel that wrapped the block in a
        // `struct nlmsghdr` would not match the check above.
    }

    let mut status = 0i32;
    let _ = lin::waitpid(pid as usize, &mut status, 0);
    let _ = lin::close(fd as i32);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-uevent: panicked\n")
}
