//! t-uevent-doors - every receive door on a device-event socket answers the
//! event, not just `recvmsg(2)`.
//!
//! The shape this pins was measured on a NIC-less boot of the current tree
//! (`docs/notes/` §7.9r): on a `NETLINK_KOBJECT_UEVENT` socket that had just
//! been bound, `recvmsg(2)` answered the announcement and `recv(2)`/`read(2)`
//! answered **`EBADF`** on the same descriptor. The kernel had a branch for
//! these handles in its `recvmsg` door only, so the other two fell through to
//! the IPv4 registry - which has never heard of a uevent handle - and read the
//! process's own bound socket as a descriptor that names nothing.
//!
//! That is not a cosmetic gap. `read(2)` and `recv(2)` are the doors a
//! program uses when it does not care who sent the message, and udev's older
//! workers and many small initramfs listeners read the bus that way: which
//! door a listener happens to pick must not decide whether it hears anything
//! at all. Linux answers the family on all three (measured, root,
//! `build/scratch/uevdoor/`: 156 bytes from `read`, `recv` and `recvmsg`
//! alike, `EAGAIN` when the queue is empty and the caller asked not to wait,
//! and a blocking door woken by an event 300 ms later).
//!
//! **Host runs make no claims.** Announcing a device means writing a sysfs
//! `uevent` attribute, which an ordinary host process is not allowed to do -
//! and a claim that has no announcement behind it would be a claim about a
//! machine that never made one. The guest is where the announcement can be
//! made, so that is where these run.

use std::io::Write;

use mpos_user_tests::{Report, host_run};

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
/// `MSG_DONTWAIT`: look at the queue without parking on it.
const MSG_DONTWAIT: libc::c_int = 0x40;
/// The one group kernel-originated events go to.
const UEVENT_GROUP_KERNEL: u32 = 1;
/// How long a parked door is given to be woken.
const BUDGET_MS: u32 = 2000;

/// A `struct sockaddr_nl`: family, pad, port id, groups.
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
        family: libc::AF_NETLINK as u16,
        _pad: 0,
        pid,
        groups,
    }
}

fn nl_socket() -> libc::c_int {
    unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_KOBJECT_UEVENT) }
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

/// The errno `call` answered, or `None` when it answered something else.
fn failed_with(call: impl FnOnce() -> isize) -> Option<i32> {
    let n = call();
    if n < 0 {
        Some(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        None
    }
}

/// The card's **devpath** - the name its uevent announces it under - read out
/// of the index link `/sys/dev/char/226:0`. Asking rather than spelling it
/// keeps the machine's bus shape out of the test.
fn card_devpath() -> Option<String> {
    let link = std::fs::read_link("/sys/dev/char/226:0").ok()?;
    let devpath = link.to_string_lossy();
    let devpath = devpath.trim_start_matches("../");
    if devpath.is_empty() {
        return None;
    }
    Some(format!("/{devpath}"))
}

/// Announce the card the way `udevadm trigger` does: a write to its `uevent`
/// attribute, from a child, so that a read in this process has to wait for it.
fn announce_in_child(trigger: &str) -> libc::pid_t {
    let trigger = trigger.to_string();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let mut rem = nap;
        unsafe {
            libc::nanosleep(&nap, &mut rem);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
            let _ = f.write_all(b"add");
        }
        unsafe { libc::_exit(0) };
    }
    pid
}

fn main() {
    println!("t-uevent-doors");
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: announcing a device means writing a sysfs `uevent` attribute, which this host \
             will not let an ordinary process do - so no claim is made here (the reference \
             measurement is Linux's own, quoted in this program's header)"
        );
        r.finish();
    }
    let Some(devpath) = card_devpath() else {
        println!(
            "note: this machine has no /sys/dev/char/226:0, so there is no card to announce and \
             nothing is claimed"
        );
        r.finish();
    };
    let trigger = format!("/sys{devpath}/uevent");
    if std::fs::metadata(&trigger).is_err() {
        println!("note: {trigger} does not exist on this machine, so nothing is claimed");
        r.finish();
    }

    let fd = nl_socket();
    r.check("a kernel-uevent listener opens", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    r.check(
        "and binds to the kernel's group",
        nl_bind(fd, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );

    // Nothing has been announced since this bound, so the queue is empty and
    // the three non-blocking doors say so with `EAGAIN` - never `EBADF`, which
    // is a descriptor this process never asked for.
    let mut buf = [0u8; 512];
    r.check(
        "recv(MSG_DONTWAIT) on an empty queue answers EAGAIN, not EBADF",
        failed_with(|| unsafe {
            libc::recv(fd, buf.as_mut_ptr() as *mut _, buf.len(), MSG_DONTWAIT)
        }) == Some(libc::EAGAIN),
    );
    r.check(
        "recvmsg(MSG_DONTWAIT) on an empty queue answers EAGAIN",
        failed_with(|| {
            let mut iov = [libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            }];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = 1;
            unsafe { libc::recvmsg(fd, &mut msg, MSG_DONTWAIT) }
        }) == Some(libc::EAGAIN),
    );
    // `read(2)`'s non-blocking answer needs the descriptor's own flag.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    r.check(
        "read(2) on an empty queue answers EAGAIN",
        failed_with(|| unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) })
            == Some(libc::EAGAIN),
    );
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };

    // One announcement, then the same event through `read(2)`.
    let announcer = announce_in_child(&trigger);
    r.check("the announcing child started", announcer > 0);
    let mut got = [0u8; 512];
    let read_n = loop {
        let n = unsafe { libc::read(fd, got.as_mut_ptr() as *mut _, got.len()) };
        if n > 0 || !n_was_eagain() {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let text = String::from_utf8_lossy(&got[..read_n.max(0) as usize]).to_string();
    r.check(
        "read(2) answers the announcement a bound listener is owed",
        read_n > 0 && text.starts_with("add@"),
    );
    r.check(
        "and its bytes name the device that announced itself",
        text.contains(&devpath),
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // `recv(2)` is the third door, with the same event.
    let announcer = announce_in_child(&trigger);
    let mut got = [0u8; 512];
    let recv_n = loop {
        let n = unsafe { libc::recv(fd, got.as_mut_ptr() as *mut _, got.len(), MSG_DONTWAIT) };
        if n > 0 || !n_was_eagain() {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    let text = String::from_utf8_lossy(&got[..recv_n.max(0) as usize]).to_string();
    r.check(
        "recv(2) answers the announcement too",
        recv_n > 0 && text.starts_with("add@"),
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // The source slots a `recvfrom(2)` caller leaves room for: this event
    // comes from the kernel, which is port 0 on the kernel's group.
    let announcer = announce_in_child(&trigger);
    let mut from = SockAddrNl::default();
    let mut from_len = std::mem::size_of::<SockAddrNl>() as libc::socklen_t;
    let mut got = [0u8; 512];
    let mut n;
    let mut left = BUDGET_MS;
    loop {
        n = unsafe {
            libc::recvfrom(
                fd,
                got.as_mut_ptr() as *mut _,
                got.len(),
                MSG_DONTWAIT,
                &mut from as *mut SockAddrNl as *mut libc::sockaddr,
                &mut from_len,
            )
        };
        if n >= 0 || left == 0 {
            break;
        }
        left -= 2;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    r.check("recvfrom(2) answers the announcement", n > 0);
    r.check(
        "and the source it reports is the kernel's own port on its group",
        from.family == libc::AF_NETLINK as u16
            && from.pid == 0
            && from.groups == UEVENT_GROUP_KERNEL,
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    // A blocking door must *wait* for the event rather than spin or refuse: the
    // announcement is 300 ms away, so an answer before the queue is filled is
    // the bug a caller with no timeout cannot survive. The `poll` below bounds
    // the claim, so a door that never wakes fails the sweep instead of hanging
    // it.
    let announcer = {
        let trigger = trigger.clone();
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
                let _ = f.write_all(b"add");
            }
            unsafe { libc::_exit(0) };
        }
        pid
    };
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, BUDGET_MS as libc::c_int) };
    let mut got = [0u8; 512];
    let blocked = if ready > 0 {
        unsafe { libc::recv(fd, got.as_mut_ptr() as *mut _, got.len(), 0) }
    } else {
        -1
    };
    r.check(
        "a blocking recv(2) holds the event once one is announced",
        blocked > 0,
    );
    unsafe { libc::waitpid(announcer, std::ptr::null_mut(), 0) };

    r.finish();
}

/// `EAGAIN` in this call's errno, read the same way [`failed_with`] does.
fn n_was_eagain() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN)
}
