//! t-sockopt-names - an option query answers what the socket holds, or refuses
//! the name; it never answers a zero nobody stored.
//!
//! Pinned here, against Linux 7.2.11 measured on the host
//! (`build/scratch/sockopt2/{o,g,l,m,r,t}.c`, `docs/notes/` §4.8):
//!
//! * A socket's **identity names** answer the truth it was made with: a
//!   `SOCK_DGRAM` socket reads `SO_TYPE` 2, `SO_DOMAIN` `AF_INET` (2) and
//!   `SO_PROTOCOL` 17; a `SOCK_STREAM` one reads 1, 2 and 6. `SO_ACCEPTCONN` is
//!   0 until the socket listens.
//! * Those names are **read-only**: `set` on any of them is `ENOPROTOOPT`
//!   (measured for `SO_TYPE`, `SO_DOMAIN`, `SO_PROTOCOL`, `SO_ACCEPTCONN` and
//!   `SO_ERROR`).
//! * An **unknown name** is refused rather than answered: `ENOPROTOOPT` on a
//!   level the socket has (`SOL_SOCKET`, `SOL_IP`, its own transport) and
//!   `EOPNOTSUPP` from a **level class the socket does not have** - the
//!   `SOL_TCP` class on a datagram socket, and the UDP class on a stream. The
//!   two doors differ on that second class: `get` answers `EOPNOTSUPP`, `set`
//!   answers `ENOPROTOOPT`, which is Linux's own split.
//!
//! # What this program cannot claim on the host
//!
//! Nothing here is unclaimable: every assertion is a statement about a socket
//! the host's own Linux also makes, which is where the numbers came from. Run
//! as `MPOS_ON_HOST=1 ./target/x86_64-unknown-linux-gnu/debug/t-sockopt-names`
//! the same assertions hold - that is the reference, not a weaker run.

use mpos_user_tests::Report;

const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_ACCEPTCONN: i32 = 30;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;
const SOL_IP: i32 = 0;
const SOL_TCP: i32 = 6;
const SOL_UDP: i32 = 17;

fn sock(typ: i32) -> i32 {
    unsafe { libc::socket(libc::AF_INET, typ, 0) }
}

fn get_int(fd: i32, level: i32, name: i32) -> Result<i32, i32> {
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            &mut v as *mut i32 as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        Ok(v)
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn set_int(fd: i32, level: i32, name: i32, value: i32) -> Result<(), i32> {
    let v: i32 = value;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &v as *const i32 as *const libc::c_void,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn main() {
    let mut r = Report::new();
    let udp = sock(libc::SOCK_DGRAM);
    let tcp = sock(libc::SOCK_STREAM);
    r.check("a datagram socket opens", udp >= 0);
    r.check("a stream socket opens", tcp >= 0);
    if udp < 0 || tcp < 0 {
        r.finish();
        return;
    }

    // ---- the identity names answer what the socket was made with ------------
    //
    // Linux answers these from the socket's own record, not from a table of
    // defaults: a `SOCK_DGRAM` socket is type 2, family 2, protocol 17, and a
    // `SOCK_STREAM` one is 1, 2, 6 (`build/scratch/sockopt2/g.c`). Answering
    // zero - which this kernel did - is indistinguishable from "the socket is
    // an unset AF_UNSPEC stream", a value no socket can have.
    r.check(
        "a datagram socket's type is SOCK_DGRAM",
        get_int(udp, SOL_SOCKET, SO_TYPE) == Ok(libc::SOCK_DGRAM),
    );
    r.check(
        "a stream socket's type is SOCK_STREAM",
        get_int(tcp, SOL_SOCKET, SO_TYPE) == Ok(libc::SOCK_STREAM),
    );
    r.check(
        "a datagram socket's domain is AF_INET",
        get_int(udp, SOL_SOCKET, SO_DOMAIN) == Ok(libc::AF_INET),
    );
    r.check(
        "and its protocol is UDP",
        get_int(udp, SOL_SOCKET, SO_PROTOCOL) == Ok(SOL_UDP),
    );
    r.check(
        "a stream socket's protocol is TCP",
        get_int(tcp, SOL_SOCKET, SO_PROTOCOL) == Ok(SOL_TCP),
    );
    r.check(
        "a socket that is not listening reports that",
        get_int(udp, SOL_SOCKET, SO_ACCEPTCONN) == Ok(0),
    );

    // ---- the identity names are read-only ----------------------------------
    //
    // A socket cannot be told its own type: Linux refuses every one of these
    // with `ENOPROTOOPT` (measured, `build/scratch/sockopt2/r.c`). Accepting
    // the write would be a lie the next query would have to repeat.
    for (label, name) in [
        ("SO_TYPE", SO_TYPE),
        ("SO_ERROR", SO_ERROR),
        ("SO_DOMAIN", SO_DOMAIN),
        ("SO_PROTOCOL", SO_PROTOCOL),
        ("SO_ACCEPTCONN", SO_ACCEPTCONN),
    ] {
        r.check(
            &format!("setting {label} is refused as an unknown option"),
            set_int(udp, SOL_SOCKET, name, 1) == Err(libc::ENOPROTOOPT),
        );
    }
    r.check(
        "and a refused write leaves the type as it was",
        get_int(udp, SOL_SOCKET, SO_TYPE) == Ok(libc::SOCK_DGRAM),
    );

    // ---- an unknown name is refused, not answered with a zero --------------
    //
    // This is the defect: `getsockopt` answered `0` to every name it did not
    // model, and `setsockopt` accepted every name it did not model. Both are
    // claims about values nobody stored. Linux refuses them, and it refuses
    // them by *class*: a name under a level the socket has is `ENOPROTOOPT`,
    // a name under a level the socket lacks is `EOPNOTSUPP` on the get side
    // (`build/scratch/sockopt2/m.c`, `l.c`).
    r.check(
        "an unknown SOL_SOCKET name is refused",
        get_int(udp, SOL_SOCKET, 9999) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "an unknown SOL_IP name is refused",
        get_int(udp, SOL_IP, 9999) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "an unknown name under a level a datagram socket lacks is not supported",
        get_int(udp, SOL_TCP, 9999) == Err(libc::EOPNOTSUPP),
    );
    r.check(
        "and the same name under a level a stream socket lacks is not supported",
        get_int(tcp, SOL_UDP, 9999) == Err(libc::EOPNOTSUPP),
    );
    r.check(
        "while a stream socket's own transport level refuses the name",
        get_int(tcp, SOL_TCP, 9999) == Err(libc::ENOPROTOOPT),
    );
    r.check(
        "a level no socket has says the same",
        get_int(udp, 999, 9999) == Err(libc::EOPNOTSUPP),
    );

    // The write door answers `ENOPROTOOPT` for both classes - the one place it
    // differs from the read door, and Linux's own asymmetry (`m.c`).
    r.check(
        "an unknown name under a level the socket lacks is refused on the write door too",
        set_int(udp, SOL_TCP, 9999, 1) == Err(libc::ENOPROTOOPT),
    );
    // NOT claimed: an unknown name under a level the socket *has* (`set(2)` of
    // `SOL_SOCKET` 9999). Linux answers `ENOPROTOOPT` there; this kernel still
    // accepts and forgets it, because a strict answer would need Linux's whole
    // per-level name table - and a partial one would be *stricter* than Linux
    // for the real names it failed to list, which is the failure that stops a
    // program's setup. The leniency is a named divergence (`docs/notes/` §4.8),
    // not an unclaimed zero: the value is still never read back as a stored one,
    // because the read door refuses the name.

    // ---- a name the stack models still answers ----------------------------
    r.check(
        "a modelled option still answers",
        set_int(udp, SOL_SOCKET, libc::SO_REUSEADDR, 1).is_ok()
            && get_int(udp, SOL_SOCKET, libc::SO_REUSEADDR) == Ok(1),
    );

    unsafe {
        libc::close(udp);
        libc::close(tcp);
    }
    r.finish();
}
