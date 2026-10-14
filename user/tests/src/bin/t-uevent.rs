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
//! `lib/kobject_uevent.c` builds the skb that way, and wrapping the block in a
//! header was tried here once: the listeners that read it directly stopped
//! finding their root device.
//!
//! **Nothing here is measurable on a host run, and that is said rather than
//! worked around.** Two measurements (`probe_uevent.c`): the sysfs `uevent`
//! file this test writes to announce a device - the way `udevadm trigger` does
//! it, so no device has to appear - is not a path this host has at all
//! (`/sys/devices/virtual/drm/card0/uevent` is ENOENT there), and a unicast
//! `sendmsg` between two monitors answers `EPERM`, so the udev handoff half
//! cannot be made either. Without an announcement the blocking `recvmsg` this
//! test exists for would wait forever, and a claim that hangs is worse than one
//! that is not made: a host run prints the measurement and finishes with no
//! claims.
//!
//! **The door is `libc`'s, not `rustix`'s**, for the reason `t-unix-scm` gives
//! for the control-message half: `rustix`'s ancillary buffers compute the
//! lengths a caller would have to lie about, and the address a monitor unicasts
//! to is a `sockaddr_nl` this test builds by hand.

use mpos_user_tests::{Report, host_run};

const AF_NETLINK: libc::c_int = 16;
/// `SOCK_RAW`, the type `libudev` opens the uevent socket as.
const SOCK_RAW: libc::c_int = 3;
const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
const MSG_DONTWAIT: libc::c_int = 0x40;
/// The one group kernel-originated events go to: `1 << KOBJ_UEVENT_KERNEL_GROUP`.
const UEVENT_GROUP_KERNEL: u32 = 1;
/// The one udev's own monitors use.
const UDEV_GROUP: u32 = 2;

/// The card's **devpath** - the name a uevent announces it under - read out of
/// the index entry `/sys/dev/char/226:0`, whose link target *is* that devpath
/// (see `src/vfs/sysfs.rs`'s `the_cards_devpath_runs_through_its_function`).
///
/// Asking rather than spelling it is what keeps this test from carrying the
/// tree's shape. The card hangs off the PCI function its driver declared when
/// the bus named one (`/devices/pci0000:00/0000:00:04.0/drm/card0`), and off the
/// `virtual/virtio-gpu` stand-in when it did not; either is the honest name for
/// the machine that has it, and both of this file's two uses - the file to write
/// and the string the event must open with - come from this one reading.
fn card_devpath() -> Option<String> {
    let link = std::fs::read_link("/sys/dev/char/226:0").ok()?;
    // The target is relative to `/sys/dev/char`, so one walk up is `/sys`.
    let devpath = link.to_string_lossy();
    let devpath = devpath.trim_start_matches("../");
    if devpath.is_empty() {
        return None;
    }
    Some(format!("/{devpath}"))
}

/// A `struct sockaddr_nl`: family, pad, port id, groups. Twelve bytes is what a
/// monitor passes as `msg_namelen`; eight is the short address one claim pins
/// as `EINVAL`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrNl {
    family: u16,
    _pad: u16,
    pid: u32,
    groups: u32,
}

fn nl_addr(pid: u32, groups: u32) -> SockAddrNl {
    SockAddrNl {
        family: AF_NETLINK as u16,
        _pad: 0,
        pid,
        groups,
    }
}

/// Turn this target's error protocol into the negated-errno answer these claims
/// compare against. musl's wrappers answer a plain `-1` with the errno beside
/// it, not glibc's negative errno - so `ECONNREFUSED` read straight off
/// `sendmsg` is `-1` and every refusal claim compares against the wrong
/// number. (`t-posix` measures the same thing; this is the third program it
/// has bitten.)
fn fix(rc: libc::c_long) -> isize {
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

fn nl_socket() -> libc::c_int {
    unsafe { libc::socket(AF_NETLINK, SOCK_RAW, NETLINK_KOBJECT_UEVENT) }
}

fn nl_bind(fd: libc::c_int, addr: &SockAddrNl) -> libc::c_int {
    unsafe {
        libc::bind(
            fd,
            addr as *const SockAddrNl as *const libc::sockaddr,
            std::mem::size_of::<SockAddrNl>() as libc::socklen_t,
        )
    }
}

fn sockname(fd: libc::c_int, out: &mut SockAddrNl) -> libc::c_int {
    let mut len = std::mem::size_of::<SockAddrNl>() as libc::socklen_t;
    unsafe { libc::getsockname(fd, out as *mut SockAddrNl as *mut libc::sockaddr, &mut len) }
}

/// `recvmsg` with the caller's flags, answering the payload length and the
/// `msg_namelen` and `msg_controllen` the kernel wrote back. A control buffer
/// sized for credentials is offered whether or not the kernel attaches any -
/// which is exactly what the one claim about them looks for.
fn receive_from(
    fd: libc::c_int,
    buf: &mut [u8],
    flags: libc::c_int,
    ctl: &mut [u8],
    name: &mut SockAddrNl,
) -> (isize, u32, usize) {
    let mut iov = [libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = name as *mut SockAddrNl as *mut libc::c_void;
    msg.msg_namelen = std::mem::size_of::<SockAddrNl>() as u32;
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = ctl.len() as u32;
    let got = fix(unsafe { libc::recvmsg(fd, &mut msg, flags) } as libc::c_long);
    (got, msg.msg_namelen, msg.msg_controllen as usize)
}

/// `sendmsg` of one payload to one address, with `namelen` the caller's - the
/// short-address and wrong-family claims are about that word.
fn send_to(fd: libc::c_int, payload: &[u8], dest: &SockAddrNl, namelen: u32) -> isize {
    let mut iov = [libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = dest as *const SockAddrNl as *mut libc::c_void;
    msg.msg_namelen = namelen;
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = 1;
    fix(unsafe { libc::sendmsg(fd, &msg, 0) } as libc::c_long)
}

/// The `(pid, uid)` of the one `SCM_CREDENTIALS` at the head of a control
/// buffer, or `None` when the record is not there or is not that. Parsed by
/// hand because the length the kernel wrote back is part of the claim:
/// `cmsg_len` is a `size_t` on Linux, so the two words after it are `level`
/// and `type`.
fn credentials(ctl: &[u8], controllen: usize) -> Option<(i32, u32)> {
    const HDR: usize = std::mem::size_of::<usize>();
    let ucred = std::mem::size_of::<libc::ucred>();
    if controllen < HDR + 8 + ucred {
        return None;
    }
    let len = usize::from_ne_bytes(ctl[..HDR].try_into().ok()?);
    let level = i32::from_ne_bytes(ctl[HDR..HDR + 4].try_into().ok()?);
    let ctype = i32::from_ne_bytes(ctl[HDR + 4..HDR + 8].try_into().ok()?);
    if level != libc::SOL_SOCKET || ctype != libc::SCM_CREDENTIALS {
        return None;
    }
    let at = HDR + 8;
    if len < at + ucred {
        return None;
    }
    let pid = i32::from_ne_bytes(ctl[at..at + 4].try_into().ok()?);
    let uid = u32::from_ne_bytes(ctl[at + 4..at + 8].try_into().ok()?);
    Some((pid, uid))
}

fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn main() {
    println!("t-uevent");
    let mut r = Report::new();

    // The announcement needs a device whose uevent file can be written. Saying
    // so is better than waiting: without one, the blocking `recvmsg` below
    // parks for a machine that is never going to answer it, and a claim that
    // hangs takes the whole sweep with it - it hung the C probe on a host for
    // exactly that reason. So the precondition is a claim of its own, and the
    // wait is not attempted when it does not hold.
    let Some(devpath) = card_devpath() else {
        println!(
            "note: this machine has no /sys/dev/char/226:0, so no card's uevent file can be \
             named - nothing can be announced, so nothing is claimed"
        );
        if host_run() {
            r.finish();
        }
        r.check(
            "the sysfs uevent file this test announces on is there",
            false,
        );
        println!("note: no announcement is possible without it, so the blocking wait is skipped");
        r.finish();
    };
    let trigger = format!("/sys{devpath}/uevent");
    if std::fs::metadata(&trigger).is_err() {
        if host_run() {
            println!(
                "note: {trigger} is not a path this host has, and a unicast sendmsg between two \
                 monitors answers EPERM here (probe_uevent.c) - nothing can be announced, so \
                 nothing is claimed"
            );
            r.finish();
        }
        r.check(
            "the sysfs uevent file this test announces on is there",
            false,
        );
        println!("note: no announcement is possible without it, so the blocking wait is skipped");
        r.finish();
    }

    let fd = nl_socket();
    r.check("a kernel uevent listener opens", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    r.check(
        "and binds",
        nl_bind(fd, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );

    // Nothing has been announced since this socket bound, so the only honest
    // answer to a *non-blocking* read is that there is none yet.
    let mut buf = [0u8; 512];
    let mut ctl = [0u8; 128];
    let mut name = SockAddrNl::default();
    let (got, _, _) = receive_from(fd, &mut buf, MSG_DONTWAIT, &mut ctl, &mut name);
    r.check(
        "an empty queue answers MSG_DONTWAIT with EAGAIN",
        got == -(libc::EAGAIN as isize),
    );

    // Now the call under test. The child announces a device a moment from now;
    // the parent asks for it *without* MSG_DONTWAIT and must therefore be made
    // to wait for it - and what it wakes holding is the legacy block.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 300_000_000,
        };
        let mut rem = nap;
        unsafe {
            libc::nanosleep(&nap, &mut rem);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
            use std::io::Write;
            let _ = f.write_all(b"add");
        }
        unsafe { libc::_exit(0) };
    }
    r.check("the announcing child started", pid > 0);

    let (got, _, _) = receive_from(fd, &mut buf, 0, &mut ctl, &mut name);
    r.check(
        "a blocking recvmsg waits for the event and returns it",
        got > 0,
    );
    if got > 0 {
        let block = &buf[..(got as usize).min(buf.len())];
        // The event names *this* machine's card, so what it opens with is the
        // same devpath the trigger was built from - see `card_devpath`.
        let opener = format!("add@{devpath}");
        r.check(
            "the block opens with the action and the device path",
            has(block, opener.as_bytes()),
        );
        // The legacy shape has no header: the first byte of the first key is
        // the first byte of `add@...`, so a kernel that wrapped the block in a
        // `struct nlmsghdr` would not match the check above.
        r.check(
            "and carries the keys a listener reads",
            has(block, b"ACTION=add") && has(block, b"SUBSYSTEM=") && has(block, b"SEQNUM="),
        );
    }

    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
        libc::close(fd);
    }

    // ---------------------------------------------------------------------
    // The other half of udev: userland *sending*. `udev_monitor_send_device`
    // hands a device to one worker with a unicast to that worker's own monitor
    // socket, and the worker drops the message unless the sender's credentials
    // came with it. Both monitors bind with `nl_pid == 0` and read their
    // address back with `getsockname`; that address is what the manager
    // addresses the handoff to.
    // ---------------------------------------------------------------------
    let manager = nl_socket();
    let worker = nl_socket();
    r.check(
        "a manager and a worker socket open",
        manager >= 0 && worker >= 0,
    );
    if manager < 0 || worker < 0 {
        r.finish();
    }

    let unspecific = nl_addr(0, 0);
    r.check(
        "both bind asking for a port id",
        nl_bind(manager, &unspecific) + nl_bind(worker, &unspecific) == 0,
    );
    let passcred = unsafe {
        let one: libc::c_int = 1;
        libc::setsockopt(
            worker,
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    r.check(
        "SO_PASSCRED is accepted on the worker's monitor",
        passcred == 0,
    );

    let mut mine = SockAddrNl::default();
    let mut theirs = SockAddrNl::default();
    let got_mine = sockname(manager, &mut mine);
    let got_theirs = sockname(worker, &mut theirs);
    r.check(
        "getsockname hands back the port id the kernel assigned",
        got_mine == 0
            && got_theirs == 0
            && mine.pid != 0
            && theirs.pid != 0
            && mine.pid != theirs.pid,
    );

    // The handoff itself: one message, addressed to the worker's port id.
    let payload = b"libudev\0\xfe\xed\xca\xfeACTION=add\0DEVNAME=event0\0";
    let sent = send_to(manager, payload, &nl_addr(theirs.pid, 0), 12);
    r.check(
        "the manager's sendmsg to a port id is accepted",
        sent == payload.len() as isize,
    );

    let mut small = [0u8; 256];
    let (got, namelen, controllen) =
        receive_from(worker, &mut small, MSG_DONTWAIT, &mut ctl, &mut name);
    r.check(
        "the worker receives the handoff whole",
        got == payload.len() as isize,
    );
    r.check(
        "and the payload is the bytes the manager sent",
        got == payload.len() as isize && &small[..payload.len()] == payload,
    );
    r.check(
        "with the sender's port id as the source address, on no group",
        namelen == 12 && name.pid == mine.pid && name.groups == 0,
    );
    r.check(
        "and the sender's credentials, which is what a monitor demands",
        credentials(&ctl, controllen)
            .map(|(cred_pid, uid)| cred_pid == unsafe { libc::getpid() } && uid == 0)
            .unwrap_or(false),
    );

    // The refusals udev's own error handling is written against: nobody at
    // that port id, an address too short to be a `sockaddr_nl`, and a family
    // this is not.
    let gone = nl_socket();
    let _ = nl_bind(gone, &unspecific);
    let mut gone_addr = SockAddrNl::default();
    let _ = sockname(gone, &mut gone_addr);
    unsafe {
        libc::close(gone);
    }
    r.check(
        "a unicast to a port id nobody holds is ECONNREFUSED",
        send_to(manager, payload, &nl_addr(gone_addr.pid, 0), 12) == -(libc::ECONNREFUSED as isize),
    );
    r.check(
        "an address shorter than a sockaddr_nl is EINVAL",
        send_to(manager, payload, &nl_addr(theirs.pid, 0), 8) == -(libc::EINVAL as isize),
    );
    let mut wrong_family = nl_addr(theirs.pid, 0);
    wrong_family.family = libc::AF_INET as u16;
    r.check(
        "and a family that is not AF_NETLINK is EINVAL too",
        send_to(manager, payload, &wrong_family, 12) == -(libc::EINVAL as isize),
    );

    // A broadcast: the group a listener joined, and not the sender.
    let member = nl_socket();
    r.check(
        "a listener joins a group by binding to it",
        nl_bind(member, &nl_addr(0, UDEV_GROUP)) == 0,
    );
    r.check(
        "a broadcast is accepted",
        send_to(manager, payload, &nl_addr(0, UDEV_GROUP), 12) == payload.len() as isize,
    );
    let (got, _, _) = receive_from(member, &mut small, MSG_DONTWAIT, &mut ctl, &mut name);
    r.check(
        "the group's member is told, on the group it was sent to",
        got == payload.len() as isize && name.groups == UDEV_GROUP,
    );
    let (nothing, _, _) = receive_from(manager, &mut small, MSG_DONTWAIT, &mut ctl, &mut name);
    r.check(
        "and the sender is not told about its own broadcast",
        nothing == -(libc::EAGAIN as isize),
    );

    unsafe {
        libc::close(member);
        libc::close(manager);
        libc::close(worker);
    }
    r.finish();
}
