//! t-ifreq - the `ifreq` ioctls a program configures a link with.
//!
//! `ip link set eth0 mtu 1400` reaches the stack through `SIOCSIFMTU`, and
//! `ip link show` reads the answer back through `SIOCGIFMTU`; the flags pair
//! (`SIOCGIFFLAGS`/`SIOCSIFFLAGS`) is what `ip link set up|down` and every
//! `IFF_UP` test are made of, and `SIOCSIFNAME` is the rename. They are not
//! netlink's less-interesting cousins: a program written against the older
//! `ioctl` interface - busybox's `ip` falls back to it, and
//! `if_nametoindex(3)` is `SIOCGIFINDEX` - uses exactly these.
//!
//! Half of this family was already served on the second machine while the
//! other half answered `ENOTTY`, because the arms that *change* a link were
//! gated off when the stack was the first machine's alone (`NOTES.md` §13.4).
//! A test that names both halves is what keeps that from happening again: the
//! read arms and the write arms have to agree about which machine they are on,
//! and the only way to see that they do is to drive one from the other.
//!
//! Two rules the test keeps, both about running on a machine other tests share:
//!
//! * It never takes a link *down*. Only `up` is ever set, which leaves the
//!   machine's state where the boot put it - a test that stopped eth0 would
//!   sabotage every socket test that follows it.
//! * It restores what it changes. The MTU round trip reads the original value
//!   rather than assuming 1500, sets and reads back, then puts the original
//!   back; the rename is undone the same way and the undo is itself checked.
//!
//! Written with `rustix` for the socket, because the descriptor these requests
//! are issued on is a socket's; the ioctl itself is declared, because `std` has
//! none and the alternative - a binding crate - would be a dependency for one
//! call.

use std::os::fd::AsRawFd;

use rustix::net::{AddressFamily, SocketType, socket};

unsafe extern "C" {
    /// `ioctl(2)`, declared rather than imported: `std` exposes no ioctl, and
    /// this is the same declaration style the aarch64 harness uses for `mount`.
    /// Linux's signature is `int ioctl(int fd, unsigned long request, ...)`;
    /// the variadic argument is always the `ifreq` block's address here, so it
    /// is written out rather than left open.
    fn ioctl(fd: i32, request: u64, arg: *mut u8) -> i32;

    /// musl's `errno` slot. Read immediately after a failing call - before
    /// anything that could make a syscall of its own - so the number is the
    /// failing request's and not a later one's.
    fn __errno_location() -> *mut i32;
}

// The `SIOC*` requests, as Linux's `linux/sockios.h` spells them.
const SIOCGIFFLAGS: u64 = 0x8913;
const SIOCSIFFLAGS: u64 = 0x8914;
const SIOCGIFMTU: u64 = 0x8921;
const SIOCSIFMTU: u64 = 0x8922;
const SIOCSIFNAME: u64 = 0x8923;
const SIOCGIFINDEX: u64 = 0x8933;

// The two errnos an answer here distinguishes: no such link, and a value the
// link cannot take.
const ENODEV: i32 = 19;
const EINVAL: i32 = 22;

// The interface flag bits, as `net/if.h` names them.
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_LOOPBACK: u16 = 0x8;

/// `struct ifreq`: a 16-byte name and a 24-byte union. The union's layout is
/// the request's, which is why this is a byte block with accessors rather than
/// a field per request - `ifr_mtu`, `ifr_flags`, `ifr_ifindex` and the rename's
/// new name all start at offset 16 and are read as different types.
#[repr(C)]
struct Ifreq {
    name: [u8; 16],
    data: [u8; 24],
}

impl Ifreq {
    /// A request naming `name` (at most 15 bytes, NUL-terminated by the zeroed
    /// field).
    fn named(name: &str) -> Self {
        let mut req = Ifreq {
            name: [0; 16],
            data: [0; 24],
        };
        req.name[..name.len()].copy_from_slice(name.as_bytes());
        req
    }

    /// The `ifr_mtu` half.
    fn mtu(&self) -> i32 {
        i32::from_ne_bytes(self.data[..4].try_into().unwrap())
    }

    /// The `ifr_flags` half.
    fn flags(&self) -> u16 {
        u16::from_ne_bytes(self.data[..2].try_into().unwrap())
    }

    /// The `ifr_ifindex` half.
    fn ifindex(&self) -> i32 {
        i32::from_ne_bytes(self.data[..4].try_into().unwrap())
    }

    fn set_mtu(&mut self, mtu: i32) {
        self.data[..4].copy_from_slice(&mtu.to_ne_bytes());
    }

    fn set_flags(&mut self, flags: u16) {
        self.data[..2].copy_from_slice(&flags.to_ne_bytes());
    }

    /// The rename request, which carries the *new* name in the union half.
    fn set_new_name(&mut self, name: &str) {
        self.data[..name.len()].copy_from_slice(name.as_bytes());
    }
}

/// The report shape the std-side tests share: a name, whether it held, and a
/// failure flag the exit status is built from.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: i32, want: i32) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }
}

/// Issue the request and hand back the raw return, so the caller can read
/// `errno` before anything else runs.
fn request(fd: i32, request: u64, req: &mut Ifreq) -> i32 {
    unsafe { ioctl(fd, request, req as *mut Ifreq as *mut u8) }
}

/// `errno` as of the call that just failed.
fn errno() -> i32 {
    unsafe { *__errno_location() }
}

fn main() {
    println!("t-ifreq");
    let mut r = Report { failed: false };

    // These requests are issued on a socket's descriptor, which is what `ip`
    // and `if_nametoindex` do; the kernel serves them from the link table, so
    // the socket never has to be bound or connected.
    let sock = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(sock) => sock,
        Err(e) => {
            println!("FAIL: no socket to issue the requests on: {e}");
            std::process::exit(1);
        }
    };
    let fd = sock.as_raw_fd();

    // The loopback is index 1 and its MTU is the 64 KiB an IPv4 datagram tops
    // out at - the one number here that is a constant rather than the stack's.
    let mut req = Ifreq::named("lo");
    let rc = request(fd, SIOCGIFMTU, &mut req);
    r.check("SIOCGIFMTU on lo is served", rc == 0);
    r.check(
        &format!("and lo's MTU is 65536 (got {})", req.mtu()),
        rc == 0 && req.mtu() == 65536,
    );

    // eth0: read, change, read back, restore. The original is read rather than
    // assumed, so the test says the request round-tripped rather than that the
    // stack's default is the number this file happens to name.
    let mut req = Ifreq::named("eth0");
    let rc = request(fd, SIOCGIFMTU, &mut req);
    let original = req.mtu();
    r.check("SIOCGIFMTU on eth0 is served", rc == 0);
    r.check(
        &format!("and its MTU is one a frame can carry (got {original})"),
        rc == 0 && (68..=65536).contains(&original),
    );

    let mut req = Ifreq::named("eth0");
    req.set_mtu(1400);
    let rc = request(fd, SIOCSIFMTU, &mut req);
    r.check("SIOCSIFMTU is served", rc == 0);

    let mut req = Ifreq::named("eth0");
    let rc = request(fd, SIOCGIFMTU, &mut req);
    r.check(
        &format!("and SIOCGIFMTU reads back the new MTU (got {})", req.mtu()),
        rc == 0 && req.mtu() == 1400,
    );

    // Below the floor is refused, not silently clamped - the value the caller
    // asked for is the value it gets or the call failed, which is the
    // distinction that keeps a program from believing it set something it did
    // not.
    let mut req = Ifreq::named("eth0");
    req.set_mtu(4);
    let rc = request(fd, SIOCSIFMTU, &mut req);
    let got = errno();
    r.check("a refused request reports failure", rc != 0);
    r.check_errno("an MTU below the floor is EINVAL", got, EINVAL);

    // Put the original back, and check that it took: a test that left the
    // machine's MTU changed would be changing the machine for the tests after
    // it, however green it looked.
    let mut req = Ifreq::named("eth0");
    req.set_mtu(original);
    let rc = request(fd, SIOCSIFMTU, &mut req);
    let mut req = Ifreq::named("eth0");
    let back = request(fd, SIOCGIFMTU, &mut req);
    r.check(
        &format!("the original MTU is put back (got {})", req.mtu()),
        rc == 0 && back == 0 && req.mtu() == original,
    );

    // The flags pair. `lo` answers IFF_LOOPBACK, `eth0` IFF_BROADCAST, and both
    // are up as the boot left them - so the read arm is checked against facts
    // the boot established rather than against a value this test wrote.
    let mut req = Ifreq::named("lo");
    let rc = request(fd, SIOCGIFFLAGS, &mut req);
    let flags = req.flags();
    r.check(
        &format!("lo is a loopback that is up (flags {flags:#06x})"),
        rc == 0 && flags & IFF_LOOPBACK != 0 && flags & IFF_UP != 0,
    );

    let mut req = Ifreq::named("eth0");
    let rc = request(fd, SIOCGIFFLAGS, &mut req);
    let flags = req.flags();
    r.check(
        &format!("eth0 is a broadcast link that is up (flags {flags:#06x})"),
        rc == 0 && flags & IFF_BROADCAST != 0 && flags & IFF_UP != 0,
    );

    // The write arm: set the link up, which it already is. Deliberately never
    // the other direction - a link this test brought down would take the socket
    // tests after it with it, and what is pinned here is that the request is
    // *served* rather than answered ENOTTY.
    let mut req = Ifreq::named("eth0");
    req.set_flags(IFF_UP);
    let rc = request(fd, SIOCSIFFLAGS, &mut req);
    r.check("SIOCSIFFLAGS is served", rc == 0);

    let mut req = Ifreq::named("eth0");
    let _ = request(fd, SIOCGIFFLAGS, &mut req);
    r.check("and the link still reports up", req.flags() & IFF_UP != 0);

    // The rename, undone at the end and the undo checked with SIOCGIFINDEX -
    // the read that says the *name* moved, not merely that the write returned
    // success.
    let mut req = Ifreq::named("eth0");
    req.set_new_name("eth9");
    let rc = request(fd, SIOCSIFNAME, &mut req);
    r.check("SIOCSIFNAME is served", rc == 0);

    let mut req = Ifreq::named("eth9");
    let rc = request(fd, SIOCGIFINDEX, &mut req);
    r.check(
        &format!("the new name resolves to index 2 (got {})", req.ifindex()),
        rc == 0 && req.ifindex() == 2,
    );

    let mut req = Ifreq::named("eth0");
    let rc = request(fd, SIOCGIFINDEX, &mut req);
    let got = errno();
    r.check("the name it left behind no longer resolves", rc != 0);
    r.check_errno("and that answer is ENODEV", got, ENODEV);

    let mut req = Ifreq::named("eth9");
    req.set_new_name("eth0");
    let renamed = request(fd, SIOCSIFNAME, &mut req) == 0;
    let mut req = Ifreq::named("eth0");
    let rc = request(fd, SIOCGIFINDEX, &mut req);
    r.check(
        "and the original name is put back",
        renamed && rc == 0 && req.ifindex() == 2,
    );

    // A name that is no link is ENODEV, which is what Linux answers a machine
    // with no such interface.
    let mut req = Ifreq::named("nosuch0");
    let rc = request(fd, SIOCGIFMTU, &mut req);
    let got = errno();
    r.check("a name that is no link is refused", rc != 0);
    r.check_errno("a name that is no link is ENODEV", got, ENODEV);

    if r.failed {
        println!("t-ifreq: FAILED");
        std::process::exit(1);
    }
    println!("t-ifreq: all passed");
}
