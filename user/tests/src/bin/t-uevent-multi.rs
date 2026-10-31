//! t-uevent-multi - one announcement reaches **every** listener on the bus.
//!
//! The shape this pins came out of a desktop boot (`docs/notes/` §7.9e): with
//! `udevd` running, `udevadm trigger --action=add` moved the kernel's counter
//! (`/sys/kernel/uevent_seqnum`, 24 → 37) but a `udevadm monitor --kernel`
//! bound *before* the trigger printed none of the thirteen events, and
//! `udevadm settle` then waited its full timeout with `/run/udev/data` empty -
//! the daemon never saw a device. Without `udevd` running the same trigger
//! delivered 137 lines to the same monitor. So the question this program asks
//! is not "does an event arrive" (that is `t-uevent`'s) but "does it arrive at
//! a *second* listener": two sockets bind, one announcement is made, and both
//! must hold it.
//!
//! A socket per listener, not two in one process: the bus's real listeners are
//! separate processes, and the netlink layer's wake list is the network
//! stack's, which both share. The two are bound in the same process here
//! because that is the cheapest shape in which the kernel still has to keep
//! two *subscribers* apart - the delivery loop in `src/uevent.rs`'s `publish`
//! runs over every bound subscriber, and a regression that lets the first one
//! take the event is exactly what a desktop boot shows and this claim would
//! catch. (The cross-process half is the boot's own shape; §7.9e records it,
//! and `t-uevent` already pins that a blocking wait on one listener is woken.)
//!
//! The announcement is a write to the card's `uevent` attribute, the file
//! `udevadm trigger` writes - so the test asks the machine which device it
//! has rather than naming one (`card_devpath`, shared with `t-uevent`), and a
//! machine with no card claims nothing and says so.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use mpos_user_tests::{Report, host_run};

const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
/// `MSG_DONTWAIT`: look at the queue without parking on it.
const MSG_DONTWAIT: libc::c_int = 0x40;
/// The one group kernel-originated events go to: `1 << KOBJ_UEVENT_KERNEL_GROUP`.
const UEVENT_GROUP_KERNEL: u32 = 1;
/// How long a listener is given to hold the announcement: a desktop boot's
/// measured settle timeout is four seconds and the event is queued before the
/// trigger's write returns, so this is only about scheduling noise.
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

/// `recvmsg` with the caller's flags, answering the payload length. The
/// `msg_name`/control buffers are offered because a kernel event carries an
/// address and a credential; this program only reads the payload length.
fn receive(fd: libc::c_int, buf: &mut [u8], flags: libc::c_int) -> isize {
    let mut iov = [libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    }];
    let mut ctl = [0u8; 128];
    let mut name = SockAddrNl::default();
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = &mut name as *mut SockAddrNl as *mut libc::c_void;
    msg.msg_namelen = std::mem::size_of::<SockAddrNl>() as u32;
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = ctl.len() as _;
    unsafe { libc::recvmsg(fd, &mut msg, flags) }
}

/// The card's **devpath** - the name its uevent announces it under - read out
/// of the index link `/sys/dev/char/226:0`, exactly as `t-uevent` reads it.
/// Asking rather than spelling it keeps the machine's bus shape out of the
/// test, and both uses (the file to write, the prefix the block opens with)
/// come from the one reading.
fn card_devpath() -> Option<String> {
    let link = std::fs::read_link("/sys/dev/char/226:0").ok()?;
    let devpath = link.to_string_lossy();
    let devpath = devpath.trim_start_matches("../");
    if devpath.is_empty() {
        return None;
    }
    Some(format!("/{devpath}"))
}

fn main() {
    println!("t-uevent-multi");
    let mut r = Report::new();

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
    // On the host the attribute is `0200 root` - a listener can be opened, but
    // nothing this program can run is allowed to announce, which is the one
    // precondition the claims rest on. Saying that is better than failing
    // claims about a machine where the announcement cannot be made.
    if host_run()
        && std::fs::metadata(&trigger)
            .map(|m| m.permissions().mode() & 0o777 != 0o200)
            .unwrap_or(true)
    {
        println!(
            "note: {trigger} is not writable here (host sysfs is `0200 root`), so no announcement \
             can be made and nothing is claimed"
        );
        r.finish();
    }

    let first = nl_socket();
    let second = nl_socket();
    r.check(
        "two kernel-uevent listeners open",
        first >= 0 && second >= 0,
    );
    if first < 0 || second < 0 {
        r.finish();
    }
    r.check(
        "and both bind to the kernel's group",
        nl_bind(first, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0
            && nl_bind(second, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );

    // Nothing has been announced since these bound, so neither holds anything.
    let mut buf = [0u8; 512];
    r.check(
        "and neither holds an event before one is announced",
        receive(first, &mut buf, MSG_DONTWAIT) < 0 && receive(second, &mut buf, MSG_DONTWAIT) < 0,
    );

    // The announcement runs in a child, so that the two reads below are what
    // wait for it - the same shape `t-uevent` uses, and for the same reason:
    // a write from this process would queue the event before a read could
    // prove it had to wait for it.
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
    r.check("the announcing child started", pid > 0);

    // Both listeners ask with a budget: a claim that hangs takes the sweep with
    // it, so neither read is allowed to park without a deadline.
    let wait = |fd: libc::c_int, buf: &mut [u8]| -> bool {
        let mut left = BUDGET_MS;
        loop {
            if receive(fd, buf, MSG_DONTWAIT) > 0 {
                return true;
            }
            if left == 0 {
                return false;
            }
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 2_000_000,
            };
            let mut rem = nap;
            unsafe {
                libc::nanosleep(&nap, &mut rem);
            }
            left -= 2;
        }
    };
    let first_got = wait(first, &mut buf);
    let mut buf2 = [0u8; 512];
    let second_got = wait(second, &mut buf2);

    r.check("the first listener is handed the announcement", first_got);
    // The claim this program exists for: one announcement, every listener.
    r.check("and so is the second, in the same process", second_got);
    if first_got && second_got {
        let opener = format!("add@{devpath}");
        let opener = opener.as_bytes();
        r.check(
            "both blocks name the same device the trigger named",
            buf.starts_with(opener) && buf2.starts_with(opener),
        );
    }
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };

    // ---- the shape a boot has: two listeners in two processes ---------------
    //
    // `udevd` and `udevadm monitor` are separate processes, and each holds its
    // own socket; this is the arrangement the desktop boot's silence came from.
    // The child binds its listener and says so down a pipe, so the announcement
    // cannot overtake the bind; it then waits for the event with the same
    // budget and reports through its exit status.
    let mut pipe_fds = [0 as libc::c_int; 2];
    r.check(
        "a pipe carries the child's readiness",
        unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } == 0,
    );
    let mine = nl_socket();
    r.check("the parent's listener opens", mine >= 0);
    r.check(
        "and binds",
        nl_bind(mine, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::close(pipe_fds[0]) };
        let fd = nl_socket();
        let bound = fd >= 0 && nl_bind(fd, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0;
        let word = [if bound { 1u8 } else { 0u8 }];
        unsafe {
            libc::write(pipe_fds[1], word.as_ptr() as *const libc::c_void, 1);
        }
        if !bound {
            unsafe { libc::_exit(2) };
        }
        let mut b = [0u8; 512];
        let got = wait(fd, &mut b);
        unsafe { libc::_exit(if got { 0 } else { 1 }) };
    }
    unsafe { libc::close(pipe_fds[1]) };
    let mut word = [0u8; 1];
    let read_back = unsafe { libc::read(pipe_fds[0], word.as_mut_ptr() as *mut libc::c_void, 1) };
    r.check(
        "the other process's listener is bound before the announcement",
        read_back == 1 && word[0] == 1,
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&trigger) {
        let _ = f.write_all(b"add");
    }
    let mut b = [0u8; 512];
    r.check(
        "the parent's listener is handed the announcement",
        wait(mine, &mut b),
    );
    let mut status = 0 as libc::c_int;
    unsafe { libc::waitpid(child, &mut status, 0) };
    r.check(
        "and the listener in the other process is handed it too",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    // ---- and a listener that *parks* on the socket is woken ----------------
    //
    // The listeners a boot actually has - `udevd` and `udevadm monitor` - do
    // not spin on `MSG_DONTWAIT`; they park in `poll(2)`/`epoll_wait(2)` and
    // wait to be told. A queue that fills without the waiter being woken is a
    // device nobody sees, which is what the boot's silence looked like, so the
    // same announcement is made once more against a parked waiter - once
    // through `poll(2)`, once through `epoll_wait(2)`, the two spellings the
    // daemons and the monitor use.
    let mut poll_fds = [0 as libc::c_int; 2];
    r.check(
        "a pipe carries the parked listener's readiness",
        unsafe { libc::pipe(poll_fds.as_mut_ptr()) } == 0,
    );
    let polled = nl_socket();
    r.check("the polled listener opens", polled >= 0);
    r.check(
        "and binds",
        nl_bind(polled, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
    );
    let announced = unsafe { libc::fork() };
    if announced == 0 {
        unsafe { libc::close(poll_fds[0]) };
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
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
    unsafe { libc::close(poll_fds[1]) };
    let mut pfd = libc::pollfd {
        fd: polled,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, BUDGET_MS as libc::c_int) };
    r.check(
        "poll(2) parks until the announcement and reports it readable",
        ready == 1 && pfd.revents & libc::POLLIN != 0,
    );
    unsafe { libc::waitpid(announced, std::ptr::null_mut(), 0) };

    // The epoll spelling, on a second announcement: one more listener parks in
    // `epoll_wait(2)` (the shape `udevd`'s workers and `udevadm monitor` use),
    // and the event has to end that wait.
    let mut epipe = [0 as libc::c_int; 2];
    r.check(
        "a pipe carries the epoll waiter's readiness",
        unsafe { libc::pipe(epipe.as_mut_ptr()) } == 0,
    );
    let watched = nl_socket();
    let ep = unsafe { libc::epoll_create1(0) };
    r.check("the watched listener opens", watched >= 0 && ep >= 0);
    if watched >= 0 && ep >= 0 {
        r.check(
            "and binds",
            nl_bind(watched, &nl_addr(0, UEVENT_GROUP_KERNEL)) == 0,
        );
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: watched as u64,
        };
        r.check(
            "and joins an epoll set for readability",
            unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, watched, &mut ev) } == 0,
        );
        let again = unsafe { libc::fork() };
        if again == 0 {
            unsafe { libc::close(epipe[0]) };
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
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
        unsafe { libc::close(epipe[1]) };
        let mut out = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let n = unsafe { libc::epoll_wait(ep, out.as_mut_ptr(), 4, BUDGET_MS as libc::c_int) };
        r.check(
            "epoll_wait(2) parks until the announcement and reports it readable",
            n >= 1 && out[0].events & libc::EPOLLIN as u32 != 0,
        );
        unsafe { libc::waitpid(again, std::ptr::null_mut(), 0) };
    }

    r.finish();
}
