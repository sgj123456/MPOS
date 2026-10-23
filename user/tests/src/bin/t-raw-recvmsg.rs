//! t-raw-recvmsg - the raw socket's receive door, on the door's own terms.
//!
//! `docs/uapi/` §5.1 item 73 left this family's two receive shapes named rather
//! than claimed, because the host that contract was measured on holds no
//! `CAP_NET_RAW`: a **zero-length** data room answered `EINVAL`, and
//! `MSG_PEEK` answered `EOPNOTSUPP`. Both were measured here first
//! (`.workbuddy/probe_raw_recvmsg.c`, a host that opens the door inside a user
//! namespace of its own, ICMP echo replies on the loopback), and the answers
//! are a datagram family's - the same ones `t-recvmsg-iov` pinned for UDP:
//!
//! * a zero-length room takes the packet and copies nothing - 0 back,
//!   `MSG_TRUNC` in `msg_flags`, the queue empty afterwards;
//! * `MSG_TRUNC` is set whether or not the caller asked, and only the caller
//!   that asked gets the packet's own length back instead of the copy's;
//! * `MSG_PEEK` copies without taking, through a full segment and through a
//!   zero-length one alike;
//! * a chain whose later segment cannot be written answers `EFAULT` with the
//!   packet already gone;
//! * `msg_iovlen` past `UIO_MAXIOV` is `EMSGSIZE` before anything is consumed;
//! * `IP_RECVTTL` attaches even when the buffer cut the IP header short, and
//!   the octet it carries is the packet's, not the copy's.
//!
//! # The two fabrics, and why this file has two
//!
//! The shapes above are the door's own, so the same claims hold on either
//! fabric. A guest's raw socket lives on `eth0` and never on the loopback
//! interface (`src/net/mod.rs`'s two-fabrics note), so it asks a real address
//! beyond slirp - the same one `t-rawicmp` pins. A host run holds no
//! `CAP_NET_RAW` at all, so it makes one: `unshare(CLONE_NEWUSER|CLONE_NEWNET)`
//! hands the caller the whole capability set inside a user namespace whose own
//! network namespace that set answers for (the probe's own measurement), brings
//! `lo` up, and sends itself ICMP echo *replies* - a type the kernel's ICMP
//! input does not answer, so one send is exactly one packet on the queue. The
//! reply's own length differs between the fabrics and no claim hardcodes it:
//! the run's first receive measures it, and every later claim is written
//! against that number.
//!
//! The calls go through the raw `recvmsg` for the reason `t-rawicmp`'s do: the
//! subject is what the kernel writes back into the msghdr's own words, and the
//! shapes include an iovec array of 1025 entries and a segment pointing into a
//! `PROT_NONE` page, which no wrapper builds.

use std::net::Ipv4Addr;

use mpos_user_tests::{Report, host_run};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::net::{AddressFamily, SocketType, ipproto, socket};

/// 110.242.74.102 - one of baidu's front addresses, the same peer
/// `t-rawicmp` pins: the guest's raw fabric is slirp's, and this address
/// answers an echo from behind it.
const PEER: Ipv4Addr = Ipv4Addr::new(110, 242, 74, 102);

const ECHO_ID: u16 = 0xBEEF;

const MSG_TRUNC: i32 = 0x20;
const MSG_PEEK: i32 = 0x2;
const MSG_DONTWAIT: i32 = 0x40;

/// The msghdr's field offsets, in the layout `src/net/socket.rs`'s reader
/// takes: name@0, namelen@8, iov@16, iovlen@24, control@32, controllen@40,
/// flags@48 - the same words `t-rawicmp` spells out, for the same reason: the
/// controllen word is pointer-sized on the wire and the claims below are about
/// what the kernel writes back into it.
const MSG_NAME: usize = 0;
const MSG_NAMELEN: usize = 8;
const MSG_IOV: usize = 16;
const MSG_IOVLEN: usize = 24;
const MSG_CONTROL: usize = 32;
const MSG_CONTROLLEN: usize = 40;
const MSG_FLAGS: usize = 48;

fn msg_word(msg: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(msg[at..at + 8].try_into().unwrap())
}

fn put_msg_word(msg: &mut [u8], at: usize, value: u64) {
    msg[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// `struct sockaddr_in` as the kernel reads it, sixteen bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WireAddr {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

/// `struct iovec` with the kernel's pointer-sized length word.
#[repr(C)]
#[derive(Clone, Copy)]
struct WireIovec {
    base: usize,
    len: u64,
}

impl WireIovec {
    fn of(buf: &mut [u8]) -> Self {
        WireIovec {
            base: buf.as_mut_ptr() as usize,
            len: buf.len() as u64,
        }
    }
    /// A segment that points at real memory and offers none of it - the
    /// zero-length data room, and the filler of the 1025-segment array.
    fn none_of(buf: &mut [u8]) -> Self {
        WireIovec {
            base: buf.as_mut_ptr() as usize,
            len: 0,
        }
    }
}

/// What one `recvmsg` left behind: the count, the errno, and the three in-out
/// words the kernel writes back (`msg_namelen`, `msg_controllen`, `msg_flags`).
struct Answer {
    n: isize,
    errno: i32,
    namelen: u32,
    controllen: u64,
    flags: i32,
}

fn errno_of(rc: isize) -> i32 {
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

/// The ICMP checksum: the standard 16-bit one's-complement fold over the
/// message. The walk is **two bytes at a time** - a one-byte step sums
/// overlapping words (`[0,1]`, `[1,2]`, `[2,3]`, ...) and answers a number no
/// peer accepts, which measured as a ping that left the machine with no reply
/// ever coming back: the real peer beyond slirp validates the word and drops
/// the request, while a loopback fabric does not re-validate one, so the host
/// run stayed green with the broken sum in place.
fn icmp_cksum(buf: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < buf.len() {
        sum += u16::from_be_bytes([buf[i], buf[i + 1]]) as u32;
        i += 2;
    }
    if i < buf.len() {
        sum += (buf[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Which fabric this run asks.
enum Fabric {
    /// The guest: a real address beyond slirp, answered with a reply.
    Slirp,
    /// The host: the loopback of a network namespace of its own, answered with
    /// the very packet the send put on the wire.
    Loopback,
}

/// One door and its fabric.
struct Door {
    fd: OwnedFd,
    fabric: Fabric,
    /// Seconds every blocking receive is armed for. A reply that never comes
    /// must answer `EINTR` and fail its claim, not park the sweep for ever -
    /// the one claim that *wants* the interrupt sets this to one.
    guard: u32,
}

impl Door {
    fn recvmsg(
        &self,
        iov: &mut [WireIovec],
        flags: i32,
        name: Option<&mut WireAddr>,
        ctl: &mut [u8],
    ) -> Answer {
        let mut msg = [0u8; 64];
        let (name_ptr, namelen) = match name {
            Some(n) => (
                n as *mut WireAddr as u64,
                std::mem::size_of::<WireAddr>() as u64,
            ),
            None => (0, 0),
        };
        put_msg_word(&mut msg, MSG_NAME, name_ptr);
        put_msg_word(&mut msg, MSG_NAMELEN, namelen);
        put_msg_word(&mut msg, MSG_IOV, iov.as_mut_ptr() as u64);
        put_msg_word(&mut msg, MSG_IOVLEN, iov.len() as u64);
        put_msg_word(&mut msg, MSG_CONTROL, ctl.as_mut_ptr() as u64);
        put_msg_word(&mut msg, MSG_CONTROLLEN, ctl.len() as u64);
        // SAFETY: the header names the caller's own buffers, sized above; the
        // kernel reads the first three words and writes words 1, 6 and 7 back.
        let rc = unsafe {
            libc::alarm(self.guard);
            let rc = libc::syscall(
                libc::SYS_recvmsg,
                self.fd.as_raw_fd() as libc::c_long,
                msg.as_mut_ptr() as libc::c_ulong as libc::c_long,
                flags as libc::c_long,
            );
            libc::alarm(0);
            rc
        };
        Answer {
            n: rc as isize,
            errno: errno_of(rc as isize),
            namelen: msg_word(&msg, MSG_NAMELEN) as u32,
            controllen: msg_word(&msg, MSG_CONTROLLEN),
            flags: i32::from_ne_bytes(msg[MSG_FLAGS..MSG_FLAGS + 4].try_into().unwrap()),
        }
    }

    /// A full-size receive into one segment.
    fn recv_full(&self, buf: &mut [u8], flags: i32) -> Answer {
        let mut iov = [WireIovec::of(buf)];
        let mut name = WireAddr::default();
        let mut ctl = [0u8; 64];
        self.recvmsg(&mut iov, flags, Some(&mut name), &mut ctl)
    }

    /// One echo, in this fabric's own way: an echo *reply* on the loopback
    /// (type 0) is the type the kernel's ICMP input does not answer, while an
    /// echo *request* to the peer beyond slirp is answered by the peer. One
    /// packet either way - but only if it arrives, which is the send's
    /// caller's problem, not this function's.
    fn send_echo(&self, seq: u16) -> bool {
        let mut pkt = [0u8; 28];
        pkt[0] = match self.fabric {
            Fabric::Slirp => 8,
            Fabric::Loopback => 0,
        };
        pkt[4] = (ECHO_ID >> 8) as u8;
        pkt[5] = ECHO_ID as u8;
        pkt[6] = (seq >> 8) as u8;
        pkt[7] = seq as u8;
        for (i, byte) in pkt[8..28].iter_mut().enumerate() {
            *byte = b'0' + (i as u8 % 10);
        }
        let cs = icmp_cksum(&pkt[..28]);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;
        let target = match self.fabric {
            Fabric::Slirp => PEER,
            Fabric::Loopback => Ipv4Addr::new(127, 0, 0, 1),
        };
        let addr = WireAddr {
            family: 2,
            port: 0,
            addr: target.octets(),
            zero: [0; 8],
        };
        // SAFETY: `pkt` and `addr` are caller-owned and live for the call.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_sendto,
                self.fd.as_raw_fd() as libc::c_long,
                pkt.as_ptr() as libc::c_ulong as libc::c_long,
                pkt.len() as libc::c_ulong as libc::c_long,
                0 as libc::c_long,
                &addr as *const WireAddr as libc::c_ulong as libc::c_long,
                std::mem::size_of::<WireAddr>() as libc::c_long,
            )
        };
        rc as isize == pkt.len() as isize
    }

    /// Take everything the queue holds. A retry's first act: a reply that
    /// arrives late belongs to the attempt that asked for it, and a claim
    /// written against *one* queued packet must not inherit a second one
    /// from an attempt that already gave up.
    fn drain(&self) {
        let mut buf = [0u8; 96];
        while !self.queue_is_empty(&mut buf) {}
    }

    /// Queue one packet, and confirm it is *on the queue* before returning.
    ///
    /// The peer beyond slirp drops a request every so often, and one gate run
    /// lost the third echo: the receive that should have answered 0 parked
    /// instead, the guard expired, and the run answered `EINTR` - a red claim
    /// about the door's own answer, caused by the internet's mood. Every
    /// claim below is written against "one queued packet", so this is where
    /// that promise is kept: each attempt drains, sends, and then confirms
    /// with a **peek** - a look leaves the packet where the claim expects to
    /// find it, and a peek that finds nothing is an `EINTR` (the guard
    /// expiring) or an `EAGAIN`, either of which is a retry. Three attempts
    /// is the bound; past it the claim that asked fails loudly rather than
    /// quietly measuring a packet that never came.
    fn queue_one(&self, seq: u16) -> bool {
        for _ in 0..3 {
            self.drain();
            if !self.send_echo(seq) {
                return false;
            }
            let mut buf = [0u8; 96];
            let a = self.recv_full(&mut buf, MSG_PEEK);
            if a.n > 0 {
                return true;
            }
        }
        false
    }

    /// Whether the queue is empty, asked with a receive that takes nothing
    /// when there is nothing: `MSG_DONTWAIT` answers `EAGAIN` and only
    /// `EAGAIN`.
    fn queue_is_empty(&self, buf: &mut [u8]) -> bool {
        let a = self.recv_full(buf, MSG_DONTWAIT);
        a.n < 0 && a.errno == libc::EAGAIN
    }
}

/// Bring `lo` up inside a network namespace of our own - a fresh one is born
/// down, and a raw packet has no route to the loopback until it is up.
fn lo_up() {
    // SAFETY: an ordinary datagram socket, closed below.
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if s < 0 {
        return;
    }
    // `struct ifreq` as bytes: the name at zero, the flags word at sixteen.
    let mut req = [0u8; 40];
    req[..2].copy_from_slice(b"lo");
    let flags: i16 = (libc::IFF_UP | libc::IFF_RUNNING) as i16;
    req[16..18].copy_from_slice(&flags.to_ne_bytes());
    // `libc::Ioctl` because the request word's type is the binding's, not the
    // ABI's: musl's `ioctl` binding takes a `c_int` and glibc's a `c_ulong`,
    // so a constant typed by hand fits exactly one of the two targets -
    // measured when the host sweep first built this crate for
    // `x86_64-unknown-linux-gnu`. The number is SIOCSIFFLAGS either way.
    const SIOCSIFFLAGS: libc::Ioctl = 0x8914;
    // SAFETY: `req` is a 40-byte buffer holding an ifreq; the ioctl reads the
    // name and the flags word out of it.
    unsafe {
        libc::ioctl(s, SIOCSIFFLAGS, req.as_mut_ptr());
        libc::close(s);
    }
}

/// The alarm handler. It exists so the signal is deliverable, which is what
/// makes an armed receive answer `EINTR` instead of sleeping through it; the
/// flag is not read, the interrupt is the answer.
extern "C" fn on_alrm(_sig: libc::c_int) {}

fn main() {
    println!("t-raw-recvmsg");
    let mut r = Report::new();

    // The handler, installed once, with no SA_RESTART: every armed receive
    // below must be *ended* by the signal, not resumed after it.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_alrm as *const () as usize;
    sa.sa_flags = 0;
    // SAFETY: installing a handler for SIGALRM for the length of the run.
    unsafe { libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut()) };

    // A host run makes its own door: a user namespace hands the caller the
    // whole capability set inside it, and a network namespace of its own is
    // one that set answers for - the probe's measurement, and the only way a
    // host without `CAP_NET_RAW` asks the family at all. A host that cannot
    // unshare names the fabric rather than claiming anything about it,
    // `t-rawicmp`'s rule.
    let fabric = if host_run() {
        // SAFETY: a plain `unshare` with two namespace bits.
        let rc =
            unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
        if rc != 0 {
            println!(
                "note: a host run without CAP_NET_RAW needs a user namespace of its own to open this door, and unshare was refused - named and not made"
            );
            r.finish();
        }
        lo_up();
        Fabric::Loopback
    } else {
        Fabric::Slirp
    };

    let fd = match socket(AddressFamily::INET, SocketType::RAW, Some(ipproto::ICMP)) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("raw ICMP socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("raw ICMP socket opens", true);
    let mut door = Door {
        fd,
        fabric,
        guard: 5,
    };

    let one: libc::c_int = 1;
    // SAFETY: SOL_IP/IP_RECVTTL with an int argument of the declared size.
    let accepted = unsafe {
        libc::setsockopt(
            door.fd.as_raw_fd(),
            libc::SOL_IP,
            libc::IP_RECVTTL,
            std::ptr::from_ref(&one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    r.check("setsockopt(IP_RECVTTL) is accepted", accepted == 0);

    let mut buf = [0u8; 96];
    let mut scratch = [0u8; 96];
    let mut name = WireAddr::default();
    let mut ctl = [0u8; 64];

    // ---- the control: the door delivers a whole packet --------------------
    r.check("an echo is queued", door.queue_one(1));
    let a = door.recv_full(&mut buf, 0);
    let pkt_len = if a.n > 0 { a.n as usize } else { 0 };
    r.check(
        "the reply arrives as a whole IPv4 echo packet",
        pkt_len >= 28 && buf[0] >> 4 == 4 && buf[9] == 1 && buf[24] == (ECHO_ID >> 8) as u8,
    );
    r.check(
        "the first delivery reports no truncation and names the sender",
        a.flags == 0 && a.namelen == 16,
    );

    // ---- a zero-length data room, a packet queued -------------------------
    r.check("an echo is queued", door.queue_one(2));
    let mut iov = [WireIovec::none_of(&mut scratch)];
    let a = door.recvmsg(&mut iov, 0, Some(&mut name), &mut ctl);
    r.check(
        "a zero-length data room answers 0",
        a.n == 0 && a.errno == 0,
    );
    r.check(
        "and it names the cut it made with MSG_TRUNC",
        a.flags & MSG_TRUNC != 0,
    );
    r.check(
        "and the packet is gone: the next MSG_DONTWAIT receive is EAGAIN",
        door.queue_is_empty(&mut buf),
    );

    // ---- a zero-length data room, the queue empty -------------------------
    let a = door.recvmsg(&mut iov, MSG_DONTWAIT, Some(&mut name), &mut ctl);
    r.check(
        "a zero-length data room with nothing queued answers EAGAIN",
        a.n < 0 && a.errno == libc::EAGAIN,
    );

    // ---- the same, blocking: a signal ends the park -----------------------
    door.guard = 1;
    let a = door.recvmsg(&mut iov, 0, Some(&mut name), &mut ctl);
    door.guard = 5;
    r.check(
        "a zero-length data room on an empty queue parks, and the signal ends it with EINTR",
        a.n < 0 && a.errno == libc::EINTR,
    );

    // ---- no iovec at all ---------------------------------------------------
    r.check("an echo is queued", door.queue_one(3));
    let mut none: [WireIovec; 0] = [];
    let a = door.recvmsg(&mut none, 0, Some(&mut name), &mut ctl);
    r.check("no iovec at all answers 0", a.n == 0 && a.errno == 0);
    r.check(
        "and the packet is gone: the next MSG_DONTWAIT receive is EAGAIN",
        door.queue_is_empty(&mut buf),
    );

    // ---- a short segment: the copy is what fitted --------------------------
    r.check("an echo is queued", door.queue_one(4));
    let mut four = [0u8; 4];
    let mut iov = [WireIovec::of(&mut four)];
    let a = door.recvmsg(&mut iov, 0, Some(&mut name), &mut ctl);
    r.check(
        "a 4-byte segment copies the packet's first four octets",
        a.n == 4 && a.errno == 0 && four[0] == 0x45 && four[1] == 0x00,
    );
    r.check(
        "and msg_flags names the cut with MSG_TRUNC",
        a.flags & MSG_TRUNC != 0,
    );

    // ---- the same segment, with MSG_TRUNC asked for ------------------------
    r.check("an echo is queued", door.queue_one(5));
    let mut four = [0u8; 4];
    let mut iov = [WireIovec::of(&mut four)];
    let a = door.recvmsg(&mut iov, MSG_TRUNC, Some(&mut name), &mut ctl);
    r.check(
        "a 4-byte segment with MSG_TRUNC answers the packet's own length",
        a.n as usize == pkt_len && a.errno == 0,
    );

    // ---- MSG_PEEK through a full segment -----------------------------------
    r.check("an echo is queued", door.queue_one(6));
    let a = door.recv_full(&mut buf, MSG_PEEK);
    r.check(
        "MSG_PEEK delivers the whole packet",
        a.n as usize == pkt_len && a.errno == 0 && a.flags == 0,
    );
    let b = door.recv_full(&mut buf, MSG_PEEK);
    r.check(
        "and again: the packet is still there to peek at",
        b.n as usize == pkt_len && b.errno == 0,
    );
    let c = door.recv_full(&mut buf, 0);
    r.check(
        "an ordinary receive then takes it",
        c.n as usize == pkt_len && c.errno == 0,
    );
    r.check(
        "and the queue is empty afterwards",
        door.queue_is_empty(&mut buf),
    );

    // ---- MSG_PEEK through a zero-length room -------------------------------
    r.check("an echo is queued", door.queue_one(7));
    // The zero-length room again: the two short-segment shapes above rebound
    // this name to their own four bytes, and the claim is about a room with
    // none.
    let mut iov = [WireIovec::none_of(&mut scratch)];
    let a = door.recvmsg(&mut iov, MSG_PEEK, Some(&mut name), &mut ctl);
    r.check(
        "MSG_PEEK through a zero-length room answers 0",
        a.n == 0 && a.errno == 0,
    );
    r.check(
        "with MSG_TRUNC naming the packet it looked at",
        a.flags & MSG_TRUNC != 0,
    );
    let b = door.recv_full(&mut buf, MSG_DONTWAIT);
    r.check(
        "and the packet is still queued for the ordinary receive",
        b.n as usize == pkt_len && b.errno == 0,
    );
    r.check(
        "and the queue is empty afterwards",
        door.queue_is_empty(&mut buf),
    );

    // ---- a zero-length room with MSG_TRUNC asked for -----------------------
    r.check("an echo is queued", door.queue_one(8));
    let a = door.recvmsg(&mut iov, MSG_TRUNC, Some(&mut name), &mut ctl);
    r.check(
        "a zero-length room with MSG_TRUNC answers the packet's own length",
        a.n as usize == pkt_len && a.errno == 0,
    );
    r.check(
        "and the packet is gone: the next MSG_DONTWAIT receive is EAGAIN",
        door.queue_is_empty(&mut buf),
    );

    // ---- MSG_PEEK with MSG_TRUNC and a zero-length room --------------------
    r.check("an echo is queued", door.queue_one(9));
    let a = door.recvmsg(&mut iov, MSG_PEEK | MSG_TRUNC, Some(&mut name), &mut ctl);
    r.check(
        "MSG_PEEK with MSG_TRUNC through a zero-length room answers the packet's own length",
        a.n as usize == pkt_len && a.errno == 0,
    );
    let b = door.recv_full(&mut buf, MSG_DONTWAIT);
    r.check(
        "and leaves the packet queued",
        b.n as usize == pkt_len && b.errno == 0,
    );
    r.check(
        "and the queue is empty afterwards",
        door.queue_is_empty(&mut buf),
    );

    // ---- a chain -----------------------------------------------------------
    r.check("an echo is queued", door.queue_one(10));
    let mut s0 = [0u8; 4];
    let mut s1 = [0u8; 4];
    let mut s2 = [0u8; 96];
    let mut chain = [
        WireIovec::of(&mut s0),
        WireIovec::of(&mut s1),
        WireIovec::of(&mut s2),
    ];
    let a = door.recvmsg(&mut chain, 0, Some(&mut name), &mut ctl);
    r.check(
        "a chain 4+4+rest takes the whole packet",
        a.n as usize == pkt_len && a.errno == 0,
    );
    r.check(
        "and the packet is scattered across it segment by segment",
        s0[0] == 0x45
            && s0[1] == 0x00
            && (1..=255).contains(&s2[0]) // the ttl octet at packet offset 8
            && s2[1] == 1 // the protocol at 9
            && s2[16] == (ECHO_ID >> 8) as u8 // the echo id at 24
            && s2[20] == b'0', // the first echoed octet at 28
    );

    // ---- a chain whose later segment cannot be written ---------------------
    r.check("an echo is queued", door.queue_one(11));
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    // SAFETY: a fresh anonymous page, written to by nobody before it is made
    // unwritable below.
    let guard = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    let mapped = guard != libc::MAP_FAILED;
    if mapped {
        // SAFETY: the page is this process's own and holds nothing yet.
        unsafe { libc::mprotect(guard, page, libc::PROT_NONE) };
    }
    let mut head = [0u8; 4];
    let mut bad = if mapped {
        [
            WireIovec::of(&mut head),
            WireIovec {
                base: guard as usize,
                len: 4,
            },
        ]
    } else {
        [WireIovec::of(&mut head), WireIovec::of(&mut head)]
    };
    let a = door.recvmsg(&mut bad, 0, Some(&mut name), &mut ctl);
    r.check(
        "a chain with an unwritable later segment answers EFAULT",
        a.n < 0 && a.errno == libc::EFAULT,
    );
    r.check(
        "and the packet is gone with it: the next MSG_DONTWAIT receive is EAGAIN",
        door.queue_is_empty(&mut buf),
    );
    if mapped {
        // SAFETY: the page is this process's own and empty.
        unsafe { libc::munmap(guard, page) };
    }

    // ---- an iovec array past UIO_MAXIOV ------------------------------------
    r.check("an echo is queued", door.queue_one(12));
    let mut many: Vec<WireIovec> = (0..1025)
        .map(|_| WireIovec::none_of(&mut scratch))
        .collect();
    let a = door.recvmsg(&mut many, 0, Some(&mut name), &mut ctl);
    r.check(
        "1025 segments answer EMSGSIZE",
        a.n < 0 && a.errno == libc::EMSGSIZE,
    );
    // The reply takes a round trip to arrive, so this waits for it rather than
    // asking once: on the loopback fabric the packet is queued by the time the
    // send returns, while beyond slirp it lands some tens of milliseconds
    // later - a `MSG_DONTWAIT` probe here would answer `EAGAIN` on the guest
    // for a packet that is on its way, which says nothing about the refusal.
    // A blocking receive answers the packet if the refusal left it queued, and
    // `EINTR` (the guard) if the refusal took it.
    let b = door.recv_full(&mut buf, 0);
    r.check(
        "and the packet survives the refusal, still queued",
        b.n as usize == pkt_len && b.errno == 0,
    );
    r.check(
        "and the queue is empty afterwards",
        door.queue_is_empty(&mut buf),
    );

    // ---- IP_RECVTTL through a buffer that cut the header -------------------
    r.check("an echo is queued", door.queue_one(13));
    let mut four = [0u8; 4];
    let mut iov = [WireIovec::of(&mut four)];
    let a = door.recvmsg(&mut iov, 0, Some(&mut name), &mut ctl);
    r.check(
        "a 4-byte segment still copies the header's first four octets",
        a.n == 4 && a.errno == 0 && four[0] == 0x45,
    );
    r.check(
        "and the hop count still rides along as one IP_RECVTTL record",
        a.controllen == 24
            // `cmsg_len` is the record's own unpadded length (`CMSG_LEN(4)`
            // = 20), while `msg_controllen` is the space it occupies
            // (`CMSG_SPACE(4)` = 24) - the two words are not the same number,
            // and the host measurement is what says so.
            && i32::from_ne_bytes(ctl[0..4].try_into().unwrap()) == 20
            && i32::from_ne_bytes(ctl[8..12].try_into().unwrap()) == libc::SOL_IP
            // The option is IP_RECVTTL; the record it answers with is named
            // IP_TTL, which is Linux's own word for it.
            && i32::from_ne_bytes(ctl[12..16].try_into().unwrap()) == libc::IP_TTL,
    );
    let ttl = i32::from_ne_bytes(ctl[16..20].try_into().unwrap());
    r.check(
        "and the octet it carries is a real hop count, not the copy's absent one",
        (1..=255).contains(&ttl),
    );

    r.finish()
}
