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

/// The `(pid, uid, gid)` of the one `SCM_CREDENTIALS` at the head of a control
/// buffer, or `None` when the record is not there or is not that. Parsed by
/// hand because the length the kernel wrote back is part of the claim:
/// `cmsg_len` is a `size_t` on Linux, so the two words after it are `level`
/// and `type`.
fn credentials(ctl: &[u8], controllen: usize) -> Option<(i32, u32, u32)> {
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
    let gid = u32::from_ne_bytes(ctl[at + 8..at + 12].try_into().ok()?);
    Some((pid, uid, gid))
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
            .map(|(cred_pid, uid, _gid)| cred_pid == unsafe { libc::getpid() } && uid == 0)
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

    // ---------------------------------------------------------------------
    // The ids are *stored*, and a read renders them through the reader's own
    // user namespace - the rule every id this kernel shows follows, and
    // Linux's for this family too. A reader in a namespace that maps the
    // stored `0` to `7` is told `7` for a peer's message and for the kernel's
    // own zero record alike, so a listener that recognises kernel origin by
    // `uid 0` is in the machine namespace, exactly as on Linux
    // (`.workbuddy/probe-nlcred.c`, `.workbuddy/probe-nluevent.c`; §6.3).
    //
    // The reader is a child, because `unshare(CLONE_NEWUSER)` moves the
    // *caller* and it is the machine-namespace root that announces the device.
    // ---------------------------------------------------------------------
    let ns_manager = nl_socket();
    let ns_worker = nl_socket();
    let ns_kernel = nl_socket();
    r.check(
        "a manager, a worker and a kernel listener open for the namespace section",
        ns_manager >= 0 && ns_worker >= 0 && ns_kernel >= 0,
    );
    if ns_manager < 0 || ns_worker < 0 || ns_kernel < 0 {
        r.finish();
    }
    r.check(
        "and bind, the listener to the kernel group",
        nl_bind(ns_manager, &unspecific)
            + nl_bind(ns_worker, &unspecific)
            + nl_bind(ns_kernel, &nl_addr(0, UEVENT_GROUP_KERNEL))
            == 0,
    );
    for fd in [ns_manager, ns_worker, ns_kernel] {
        let one: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let mut ns_worker_addr = SockAddrNl::default();
    let _ = sockname(ns_worker, &mut ns_worker_addr);

    let mut ready = [0 as libc::c_int; 2];
    let mut said = [0 as libc::c_int; 2];
    let pipes_ok = unsafe { libc::pipe(ready.as_mut_ptr()) } == 0
        && unsafe { libc::pipe(said.as_mut_ptr()) } == 0;
    r.check("the two handshake pipes are made", pipes_ok);

    let reader = if pipes_ok {
        unsafe { libc::fork() }
    } else {
        -1
    };
    if reader == 0 {
        // The reader, in a user namespace that maps the stored uid/gid 0 to 7.
        unsafe {
            libc::close(ready[0]);
            libc::close(said[0]);
        }
        let moved = unsafe { libc::unshare(libc::CLONE_NEWUSER) } == 0;
        let uid_ok = std::fs::write("/proc/self/uid_map", "7 0 1\n").is_ok();
        let _ = std::fs::write("/proc/self/setgroups", "deny\n");
        let gid_ok = std::fs::write("/proc/self/gid_map", "7 0 1\n").is_ok();
        let hello = [moved as u8, uid_ok as u8, gid_ok as u8];
        let _ = unsafe { libc::write(ready[1], hello.as_ptr().cast(), hello.len()) };
        let mut nbuf = [0u8; 512];
        let mut nctl = [0u8; 128];
        let mut nname = SockAddrNl::default();
        let _ = receive_from(ns_worker, &mut nbuf, 0, &mut nctl, &mut nname);
        let peer = credentials(&nctl, nctl.len());
        let _ = receive_from(ns_kernel, &mut nbuf, 0, &mut nctl, &mut nname);
        let kern = credentials(&nctl, nctl.len());
        let line = format!(
            "{} {} {} {}\n",
            peer.map_or(-1, |c| c.1 as i64),
            peer.map_or(-1, |c| c.2 as i64),
            kern.map_or(-1, |c| c.1 as i64),
            kern.map_or(-1, |c| c.2 as i64),
        );
        let _ = unsafe { libc::write(said[1], line.as_ptr().cast(), line.len()) };
        unsafe { libc::_exit(0) };
    }
    unsafe {
        libc::close(ready[1]);
        libc::close(said[1]);
    }
    r.check("the reader child started", reader > 0);
    if reader > 0 {
        let mut hello = [0u8; 3];
        let hn = unsafe { libc::read(ready[0], hello.as_mut_ptr().cast(), 3) };
        r.check(
            "the reader is inside a user namespace that maps 0 to 7",
            hn == 3 && hello[0] == 1 && hello[1] == 1 && hello[2] == 1,
        );
        let payload2 = b"namespace\0";
        r.check(
            "the manager sends a peer's message to the namespace's worker",
            send_to(ns_manager, payload2, &ns_worker_addr, 12) == payload2.len() as isize,
        );
        let announced = std::fs::OpenOptions::new()
            .write(true)
            .open(&trigger)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(b"add")
            })
            .is_ok();
        r.check(
            "and a device is announced for the namespace's listener",
            announced,
        );
        let mut line = [0u8; 64];
        let n = unsafe { libc::read(said[0], line.as_mut_ptr().cast(), line.len()) };
        let words: Vec<i64> = String::from_utf8_lossy(&line[..n.max(0) as usize])
            .split_whitespace()
            .filter_map(|w| w.parse().ok())
            .collect();
        r.check("the reader reports four ids", words.len() == 4);
        if words.len() == 4 {
            r.check(
                "a peer's uid and gid are rendered through the reader's namespace (0 -> 7)",
                words[0] == 7 && words[1] == 7,
            );
            r.check(
                "and the kernel's own zero record is rendered too (0 -> 7)",
                words[2] == 7 && words[3] == 7,
            );
        }
        let mut status: libc::c_int = 0;
        unsafe {
            libc::waitpid(reader, &mut status, 0);
            libc::close(ns_manager);
            libc::close(ns_worker);
            libc::close(ns_kernel);
        }
    }

    // ---------------------------------------------------------------------
    // The pid, the other half of the same record, is the *sender's* number in
    // the sender's own namespace - resolved once at send (Linux's
    // `scm_set_cred`'s `pid_vnr`), and the port id a zero-`nl_pid` bind is
    // given comes from the same number (`netlink_autobind`'s
    // `task_tgid_vnr`). A sender inside a pid namespace is named there by the
    // number it knows itself by, and the port id its own bind is given is that
    // number too (`.workbuddy/probe-nlpid.c`; §6.3). The *reader's* namespace
    // does not move it: a peer in an ancestor namespace reads the sender's own
    // number and not `0` (`.workbuddy/probe-nlcred.c`).
    //
    // A pid namespace renumbers children, so the sender has to be the
    // namespace's init: the test moves its children in, and the init - pid 1 -
    // owns both sockets and does the send and the read itself.
    // ---------------------------------------------------------------------
    let moved_pid = unsafe { libc::unshare(libc::CLONE_NEWPID) } == 0;
    r.check(
        "the test moves its children into a pid namespace",
        moved_pid,
    );
    if moved_pid {
        let mut told = [0 as libc::c_int; 2];
        let told_ok = unsafe { libc::pipe(told.as_mut_ptr()) } == 0;
        let init = if told_ok { unsafe { libc::fork() } } else { -1 };
        if init == 0 {
            unsafe { libc::close(told[0]) };
            let sender = nl_socket();
            let receiver = nl_socket();
            let mut sender_addr = SockAddrNl::default();
            let mut receiver_addr = SockAddrNl::default();
            let _ = nl_bind(sender, &unspecific);
            let _ = nl_bind(receiver, &unspecific);
            let _ = sockname(sender, &mut sender_addr);
            let _ = sockname(receiver, &mut receiver_addr);
            let one: libc::c_int = 1;
            unsafe {
                libc::setsockopt(
                    receiver,
                    libc::SOL_SOCKET,
                    libc::SO_PASSCRED,
                    &one as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            let _ = send_to(sender, b"pidns\0", &nl_addr(receiver_addr.pid, 0), 12);
            let mut nbuf = [0u8; 512];
            let mut nctl = [0u8; 128];
            let mut nname = SockAddrNl::default();
            let _ = receive_from(receiver, &mut nbuf, 0, &mut nctl, &mut nname);
            let named = credentials(&nctl, nctl.len()).map_or(-1, |c| c.0 as i64);
            let line = format!(
                "{} {} {}\n",
                unsafe { libc::getpid() } as i64,
                sender_addr.pid as i64,
                named
            );
            let _ = unsafe { libc::write(told[1], line.as_ptr().cast(), line.len()) };
            unsafe { libc::_exit(0) };
        }
        unsafe { libc::close(told[1]) };
        r.check("the namespace's init started", init > 0);
        if init > 0 {
            let mut line = [0u8; 64];
            let n = unsafe { libc::read(told[0], line.as_mut_ptr().cast(), line.len()) };
            let words: Vec<i64> = String::from_utf8_lossy(&line[..n.max(0) as usize])
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            r.check("the init reports three numbers", words.len() == 3);
            if words.len() == 3 {
                r.check(
                    "a sender in a pid namespace is its own number 1",
                    words[0] == 1,
                );
                r.check(
                    "and the port id a zero-`nl_pid` bind gives it is that number",
                    words[1] == 1,
                );
                r.check("and a peer's ucred names it by that number", words[2] == 1);
            }
            let mut status: libc::c_int = 0;
            unsafe { libc::waitpid(init, &mut status, 0) };
        }
    }

    unsafe {
        libc::close(member);
        libc::close(manager);
        libc::close(worker);
    }
    r.finish();
}
