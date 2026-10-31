//! t-uevent-opts - the options a device-event socket answers, in Linux's table.
//!
//! A `NETLINK_KOBJECT_UEVENT` socket is not in this kernel's network registry,
//! so `getsockopt` used to answer **zero** for every option name it was not
//! told about - a claim that the socket had been set to a value it never
//! stored. Linux's own answers were measured first
//! (`build/scratch/opt/{wide,set,set2,tv,trunc,pc}.c`, 7.2.11, as root and
//! unprivileged - the two agree), and the three that mattered are visible to
//! any listener:
//!
//! - `SO_TYPE` is **3** (`SOCK_RAW`), and so are `SO_PROTOCOL` (15) and
//!   `SO_DOMAIN` (16). libudev asks for none of them, but `sd_device`-based
//!   readers and every socket-sanitising shim do.
//! - `SO_RCVBUF`/`SO_SNDBUF` start at the datagram default and, once set, read
//!   back **doubled** (`set(4096)` answers `8192`) - the same arithmetic the
//!   IP family already used.
//! - A name the family does not model is refused with Linux's own errno rather
//!   than answered: `ENOPROTOOPT` for an unknown name, a foreign level or any
//!   `SOL_NETLINK` option; `EPERM` for `SO_PRIORITY`/`SO_MARK`; `EOPNOTSUPP`
//!   for `SO_REUSEPORT`.
//!
//! **Host runs make no claims.** The whole table is the *kernel's* answer, and
//! Linux refuses to be the reference for two rows of it: a host process may
//! `setsockopt(SO_REUSEPORT)` on a netlink socket successfully, and as root
//! `SO_PRIORITY`/`SO_MARK` too, so a claim written here would fail against the
//! very answers it was measured from. The reference is quoted in this header
//! instead, and the claims run where the kernel under test is.

use mpos_user_tests::{Report, host_run};

const SOL_SOCKET: libc::c_int = 1;
const SO_SNDBUF: libc::c_int = 7;
const SO_RCVBUF: libc::c_int = 8;
const SO_TYPE: libc::c_int = 3;
const SO_PRIORITY: libc::c_int = 12;
const SO_REUSEPORT: libc::c_int = 15;
const SO_MARK: libc::c_int = 36;
const SO_PROTOCOL: libc::c_int = 38;
const SO_DOMAIN: libc::c_int = 39;
const SO_LINGER: libc::c_int = 13;
const SO_RCVLOWAT: libc::c_int = 18;
const SO_INCOMING_CPU: libc::c_int = 49;
const SOL_NETLINK: libc::c_int = 270;
const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrNl {
    family: u16,
    _pad: u16,
    pid: u32,
    groups: u32,
}

fn get_int(fd: libc::c_int, level: libc::c_int, name: libc::c_int) -> Result<i32, libc::c_int> {
    let mut val: i32 = -7;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let rc = unsafe { libc::getsockopt(fd, level, name, &mut val as *mut i32 as *mut _, &mut len) };
    if rc == 0 {
        Ok(val)
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn set_int(fd: libc::c_int, level: libc::c_int, name: libc::c_int, v: i32) -> libc::c_int {
    unsafe { libc::setsockopt(fd, level, name, &v as *const i32 as *const _, 4) }
}

fn main() {
    println!("t-uevent-opts");
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: the rows this pin asserts are this kernel's answers, and Linux refuses two \
             of them by permission on a host (SO_REUSEPORT sets, SO_PRIORITY/SO_MARK are EPERM \
             only unprivileged) - so the reference is the header's measurement, not a claim here"
        );
        r.finish();
    }

    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_KOBJECT_UEVENT) };
    r.check("a device-event socket opens", fd >= 0);
    if fd < 0 {
        r.finish();
    }
    let addr = SockAddrNl {
        family: libc::AF_NETLINK as u16,
        _pad: 0,
        pid: 0,
        groups: 1,
    };
    let bound = unsafe {
        libc::bind(
            fd,
            &addr as *const SockAddrNl as *const _,
            std::mem::size_of::<SockAddrNl>() as libc::socklen_t,
        )
    };
    r.check("and binds to the kernel's group", bound == 0);

    // The three identity answers - Linux's, and formerly a zero here.
    r.check(
        "SO_TYPE is SOCK_RAW",
        get_int(fd, SOL_SOCKET, SO_TYPE) == Ok(3),
    );
    r.check(
        "SO_PROTOCOL is NETLINK_KOBJECT_UEVENT",
        get_int(fd, SOL_SOCKET, SO_PROTOCOL) == Ok(NETLINK_KOBJECT_UEVENT),
    );
    r.check(
        "SO_DOMAIN is AF_NETLINK",
        get_int(fd, SOL_SOCKET, SO_DOMAIN) == Ok(libc::AF_NETLINK),
    );

    // The buffer pair: the datagram default, then Linux's doubling on a set.
    let rcv_default = get_int(fd, SOL_SOCKET, SO_RCVBUF);
    r.check(
        "SO_RCVBUF starts at the datagram default",
        matches!(rcv_default, Ok(v) if v > 0),
    );
    r.check(
        "and SO_SNDBUF starts there too",
        matches!(get_int(fd, SOL_SOCKET, SO_SNDBUF), Ok(v) if v > 0),
    );
    r.check(
        "setting SO_RCVBUF answers 0",
        set_int(fd, SOL_SOCKET, SO_RCVBUF, 4096) == 0,
    );
    r.check(
        "and it reads back doubled, as Linux stores it",
        get_int(fd, SOL_SOCKET, SO_RCVBUF) == Ok(8192),
    );
    r.check(
        "setting SO_SNDBUF doubles the same way",
        set_int(fd, SOL_SOCKET, SO_SNDBUF, 4096) == 0
            && get_int(fd, SOL_SOCKET, SO_SNDBUF) == Ok(8192),
    );

    // An option that is stored and read back rather than computed.
    r.check(
        "SO_RCVLOWAT reads back 1 before any set",
        get_int(fd, SOL_SOCKET, SO_RCVLOWAT) == Ok(1),
    );
    r.check(
        "and reads back what a set was told",
        set_int(fd, SOL_SOCKET, SO_RCVLOWAT, 5) == 0
            && get_int(fd, SOL_SOCKET, SO_RCVLOWAT) == Ok(5),
    );
    r.check(
        "SO_INCOMING_CPU starts at Linux's -1",
        get_int(fd, SOL_SOCKET, SO_INCOMING_CPU) == Ok(-1),
    );

    // The refusals: a name the family does not model is never answered with a
    // value it did not store.
    r.check(
        "an unknown SOL_SOCKET name is refused",
        get_int(fd, SOL_SOCKET, 9999) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "a foreign level is refused",
        get_int(fd, 999, SO_RCVBUF) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "any SOL_NETLINK option is refused",
        get_int(fd, SOL_NETLINK, 1) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "SO_PRIORITY is a permission refusal, not an answer",
        set_int(fd, SOL_SOCKET, SO_PRIORITY, 3) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    );
    r.check(
        "SO_MARK likewise",
        set_int(fd, SOL_SOCKET, SO_MARK, 9) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    );
    r.check(
        "SO_REUSEPORT is unsupported rather than ignored",
        set_int(fd, SOL_SOCKET, SO_REUSEPORT, 1) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EOPNOTSUPP),
    );
    r.check(
        "and an identity name cannot be set",
        set_int(fd, SOL_SOCKET, SO_TYPE, 2) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOPROTOOPT)
            && get_int(fd, SOL_SOCKET, SO_TYPE) == Ok(3),
    );

    // A short buffer is truncated to what fits and answered with that length -
    // Linux's rule, not an `EINVAL`.
    let mut val: i32 = -7;
    let mut len = 2u32;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVBUF,
            &mut val as *mut i32 as *mut _,
            &mut len as *mut u32 as *mut _,
        )
    };
    r.check(
        "a 2-byte buffer is truncated, not refused",
        rc == 0 && len == 2,
    );

    // A pair-valued option needs the whole pair on a set and answers its width.
    let mut two = [-7i32; 2];
    let mut len = 8u32;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_SOCKET,
            SO_LINGER,
            two.as_mut_ptr() as *mut _,
            &mut len as *mut u32 as *mut _,
        )
    };
    r.check(
        "SO_LINGER answers both words and a length of 8",
        rc == 0 && len == 8 && two == [0, 0],
    );
    r.check(
        "and a 4-byte set of it is refused",
        unsafe { libc::setsockopt(fd, SOL_SOCKET, SO_LINGER, two.as_ptr() as *const _, 4) } == -1,
    );

    unsafe { libc::close(fd) };
    r.finish();
}
