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

#[path = "../../../lib/lin.rs"]
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

/// A `struct sockaddr_nl` aimed at somebody else: family, pad, port id, groups.
/// The harness passes addresses as the 16-byte `SockAddrIn`, of which the first
/// twelve bytes are the netlink address.
fn nl_addr_to(pid: u32, groups: u32) -> lin::SockAddrIn {
    let pid = pid.to_le_bytes();
    let grp = groups.to_le_bytes();
    lin::SockAddrIn {
        family: AF_NETLINK as u16,
        port: 0,
        addr: pid,
        zero: [grp[0], grp[1], grp[2], grp[3], 0, 0, 0, 0],
    }
}

/// The `nl_pid` and `nl_groups` out of a written `sockaddr_nl`.
fn nl_address(addr: &lin::SockAddrIn) -> (u32, u32) {
    (
        u32::from_le_bytes(addr.addr),
        u32::from_le_bytes([addr.zero[0], addr.zero[1], addr.zero[2], addr.zero[3]]),
    )
}

/// One `recvmsg` that also asks for the sender's address, and reports what came
/// back: the payload length, `msg_namelen`, the control length and the address.
fn receive_from(
    fd: i32,
    buf: &mut [u8],
    flags: usize,
    ctl: &mut [usize; 4],
    name: &mut [u8; 16],
) -> (isize, u32, usize) {
    let mut iov = [lin::IoVec {
        base: buf.as_mut_ptr() as usize,
        len: buf.len(),
    }];
    let mut msg = lin::MsgHdr::default();
    msg.msg_name = name.as_mut_ptr() as usize;
    msg.msg_namelen = 16;
    msg.msg_iov = iov.as_mut_ptr() as usize;
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr() as usize;
    msg.msg_controllen = core::mem::size_of_val(ctl);
    let got = lin::recvmsg(fd, &mut msg, flags);
    (got, msg.msg_namelen, msg.msg_controllen)
}

/// The `(pid, uid, gid)` of the one `SCM_CREDENTIALS` at the head of a control
/// buffer, or `None` when the record is not there or is not that.
fn credentials(ctl: &[usize; 4], controllen: usize) -> Option<(i32, u32, u32)> {
    if controllen < 28 || ctl[0] != 28 {
        return None;
    }
    let level = ctl[1] as u32;
    let ctype = (ctl[1] >> 32) as u32;
    if level != lin::SOL_SOCKET as u32 || ctype != 2 {
        return None;
    }
    Some((ctl[2] as u32 as i32, (ctl[2] >> 32) as u32, ctl[3] as u32))
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

    // ---------------------------------------------------------------------
    // The other half of udev: userland *sending*. `udev_monitor_send_device`
    // hands a device to one worker with a unicast to that worker's own monitor
    // socket - `sendmsg` with a 12-byte `sockaddr_nl` and the device's
    // properties in the iovec - and the worker drops the message unless the
    // sender's credentials came with it. Both monitors bind with `nl_pid == 0`
    // and read their address back with `getsockname`; that address is what the
    // manager addresses the handoff to.
    // ---------------------------------------------------------------------
    let manager = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT) as i32;
    let worker = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT) as i32;
    lin::check_true(
        &mut failed,
        b"a manager and a worker socket open",
        manager > 0 && worker > 0,
    );
    if manager > 0 && worker > 0 {
        let unspecific = nl_addr_to(0, 0);
        lin::check_eq(
            &mut failed,
            b"both bind asking for a port id",
            lin::bind(manager, &unspecific) + lin::bind(worker, &unspecific),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"SO_PASSCRED is accepted on the worker's monitor",
            lin::setsockopt_int(worker, lin::SOL_SOCKET, lin::SO_PASSCRED, 1),
            0,
        );

        let mut mine = nl_addr_to(0, 0);
        let mut theirs = nl_addr_to(0, 0);
        let got_name = lin::getsockname(manager, &mut mine);
        let got_theirs = lin::getsockname(worker, &mut theirs);
        let (manager_pid, _) = nl_address(&mine);
        let (worker_pid, _) = nl_address(&theirs);
        lin::check_true(
            &mut failed,
            b"getsockname hands back the port id the kernel assigned",
            got_name == 0
                && got_theirs == 0
                && manager_pid != 0
                && worker_pid != 0
                && manager_pid != worker_pid,
        );

        // The handoff itself: one message, addressed to the worker's port id.
        let payload = b"libudev\0\xfe\xed\xca\xfeACTION=add\0DEVNAME=event0\0";
        let dest = nl_addr_to(worker_pid, 0);
        let mut iov = [lin::IoVec {
            base: payload.as_ptr() as usize,
            len: payload.len(),
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_name = &dest as *const lin::SockAddrIn as usize;
        msg.msg_namelen = 12;
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        lin::check_eq(
            &mut failed,
            b"the manager's sendmsg to a port id is accepted",
            lin::sendmsg(manager, &mut msg, 0),
            payload.len() as isize,
        );

        let mut buf = [0u8; 256];
        let mut ctl = [0usize; 4];
        let mut name = [0u8; 16];
        let (got, namelen, controllen) = receive_from(worker, &mut buf, 0, &mut ctl, &mut name);
        lin::check_eq(
            &mut failed,
            b"the worker receives the handoff whole",
            got,
            payload.len() as isize,
        );
        lin::check_true(
            &mut failed,
            b"and the payload is the bytes the manager sent",
            got == payload.len() as isize && &buf[..payload.len()] == payload,
        );
        let source_pid = u32::from_le_bytes([name[4], name[5], name[6], name[7]]);
        let source_groups = u32::from_le_bytes([name[8], name[9], name[10], name[11]]);
        lin::check_true(
            &mut failed,
            b"with the sender's port id as the source address, on no group",
            namelen == 12 && source_pid == manager_pid && source_groups == 0,
        );
        lin::check_true(
            &mut failed,
            b"and the sender's credentials, which is what a monitor demands",
            credentials(&ctl, controllen)
                .map(|(pid, uid, _)| pid == lin::getpid() as i32 && uid == 0)
                .unwrap_or(false),
        );

        // The refusals udev's own error handling is written against: nobody at
        // that port id, an address too short to be a `sockaddr_nl`, and a
        // family this is not.
        let gone = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT) as i32;
        let mut gone_addr = nl_addr_to(0, 0);
        let _ = lin::bind(gone, &unspecific);
        let _ = lin::getsockname(gone, &mut gone_addr);
        let (gone_pid, _) = nl_address(&gone_addr);
        let _ = lin::close(gone);
        let gone_dest = nl_addr_to(gone_pid, 0);
        msg.msg_name = &gone_dest as *const lin::SockAddrIn as usize;
        msg.msg_namelen = 12;
        lin::check_eq(
            &mut failed,
            b"a unicast to a port id nobody holds is ECONNREFUSED",
            lin::sendmsg(manager, &mut msg, 0),
            lin::ECONNREFUSED,
        );

        let short_dest = nl_addr_to(worker_pid, 0);
        msg.msg_name = &short_dest as *const lin::SockAddrIn as usize;
        msg.msg_namelen = 8;
        lin::check_eq(
            &mut failed,
            b"an address shorter than a sockaddr_nl is EINVAL",
            lin::sendmsg(manager, &mut msg, 0),
            lin::EINVAL,
        );

        let mut wrong_family = nl_addr_to(worker_pid, 0);
        wrong_family.family = 2; /* AF_INET */
        msg.msg_name = &wrong_family as *const lin::SockAddrIn as usize;
        msg.msg_namelen = 12;
        lin::check_eq(
            &mut failed,
            b"and a family that is not AF_NETLINK is EINVAL too",
            lin::sendmsg(manager, &mut msg, 0),
            lin::EINVAL,
        );

        // A broadcast: the group a listener joined, and not the sender.
        let udev_group: u32 = 2;
        let member = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT) as i32;
        lin::check_eq(
            &mut failed,
            b"a listener joins a group by binding to it",
            lin::bind(member, &nl_addr_to(0, udev_group)),
            0,
        );
        let group_dest = nl_addr_to(0, udev_group);
        msg.msg_name = &group_dest as *const lin::SockAddrIn as usize;
        msg.msg_namelen = 12;
        lin::check_eq(
            &mut failed,
            b"a broadcast is accepted",
            lin::sendmsg(manager, &mut msg, 0),
            payload.len() as isize,
        );
        let (got, _, _) = receive_from(member, &mut buf, 0, &mut ctl, &mut name);
        let source_groups = u32::from_le_bytes([name[8], name[9], name[10], name[11]]);
        lin::check_true(
            &mut failed,
            b"the group's member is told, on the group it was sent to",
            got == payload.len() as isize && source_groups == udev_group,
        );
        let (nothing, _, _) = receive_from(manager, &mut buf, MSG_DONTWAIT, &mut ctl, &mut name);
        lin::check_eq(
            &mut failed,
            b"and the sender is not told about its own broadcast",
            nothing,
            lin::EAGAIN,
        );
        let _ = lin::close(member);
        let _ = lin::close(manager);
        let _ = lin::close(worker);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-uevent: panicked\n")
}
