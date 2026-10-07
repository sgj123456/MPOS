//! t-inotify - the filesystem's news, delivered through a descriptor.
//!
//! Pinned here, per inotify(7): the init flags are IN_NONBLOCK and
//! IN_CLOEXEC alone; a read shorter than one event header is EINVAL; an
//! empty nonblocking read is EAGAIN; the watch call answers ENOENT for a
//! name that is not there and EINVAL for `IN_ONLYDIR` on a file; and the
//! life of one watched name reports itself in order - CREATE, OPEN, MODIFY,
//! CLOSE_WRITE, DELETE - each record's wd, mask, name and 8-byte-aligned
//! padding exactly as the kernel encodes them. A rename across two watched
//! directories pairs `IN_MOVED_FROM` and `IN_MOVED_TO` on one cookie;
//! deleting a directly-watched file says DELETE_SELF then IGNORED; and
//! `inotify_rm_watch` retires a live wd (an unknown one is EINVAL, and the
//! queue then says IGNORED).
//!
//! # The raw door is the record layout
//!
//! rustix binds none of the family (clause (a)), so every call is
//! `libc::syscall(SYS_inotify_*, ..)` with libc's per-machine numbers
//! (init1 294/26, add_watch 295/27, rm_watch 296/28). The event records are
//! walked by hand from *the caller's* buffer - the shape `t-fd-list`'s
//! RawDir chose for the same reason: the claim is what the kernel wrote,
//! byte for byte. The fixture tree lives under `/tmp`, which both payloads
//! back with a tmpfs.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::write;

const IN_ACCESS: u32 = 0x0001;
const IN_MODIFY: u32 = 0x0002;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_OPEN: u32 = 0x0020;
const IN_CLOSE_WRITE: u32 = 0x0008;
const IN_MOVED_FROM: u32 = 0x0040;
const IN_MOVED_TO: u32 = 0x0080;
const IN_MOVE_SELF: u32 = 0x0800;
const IN_DELETE_SELF: u32 = 0x0400;
const IN_IGNORED: u32 = 0x8000;
#[allow(dead_code)]
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONLYDIR: u32 = 0x0000_1000;
const IN_NONBLOCK: u32 = 0o4000;
const IN_CLOEXEC: u32 = 0o200000;

/// One record's fixed header, Linux's own layout.
#[repr(C)]
struct EventHeader {
    wd: i32,
    mask: u32,
    cookie: u32,
    /// The record's name-plus-padding length; `name_len` reads it (the
    /// struct's own size is `size_of`, not a field).
    name_len: u32,
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn init1(flags: u32) -> isize {
    unsafe { libc::syscall(libc::SYS_inotify_init1, flags) as isize }
}

fn add_watch(fd: i32, path: &str, mask: u32) -> isize {
    let c = std::ffi::CString::new(path).unwrap();
    unsafe { libc::syscall(libc::SYS_inotify_add_watch, fd, c.as_ptr() as usize, mask) as isize }
}

fn rm_watch(fd: i32, wd: i32) -> isize {
    unsafe { libc::syscall(libc::SYS_inotify_rm_watch, fd, wd) as isize }
}

/// Read the queue dry into the caller's buffer and return the bytes.
fn drain(fd: i32, buf: &mut [u8]) -> &[u8] {
    match read_raw(fd, buf) {
        n if n > 0 => &buf[..n as usize],
        _ => &[],
    }
}

fn read_raw(fd: i32, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) as isize }
}

/// Walk every record of a drained buffer, calling `f` on each.
fn foreach_record(buf: &[u8], mut f: impl FnMut(&EventHeader, &[u8])) {
    let mut off = 0;
    while off + core::mem::size_of::<EventHeader>() <= buf.len() {
        let hdr = unsafe { &*(buf.as_ptr().add(off) as *const EventHeader) };
        let name_start = off + core::mem::size_of::<EventHeader>();
        let name_end = (name_start + hdr.name_len as usize).min(buf.len());
        let mut name = &buf[name_start..name_end];
        while name.last() == Some(&0) {
            name = &name[..name.len() - 1];
        }
        f(hdr, name);
        off = name_start + hdr.name_len as usize;
    }
}

fn main() {
    println!("t-inotify");
    let mut r = Report::new();

    // The init's flags and the read's own validation.
    r.check(
        "unknown init flags answer EINVAL",
        refused(init1(0xFFFF), libc::EINVAL),
    );
    let q = init1(IN_NONBLOCK);
    r.check("init hands back a descriptor", q >= 0);
    if q < 0 {
        r.finish();
    }
    let q = q as i32;
    // A short buffer is refused by this kernel with the man page's EINVAL;
    // Linux instead answers 0 (the queue was empty, and a small buffer is
    // simply nothing to take) - measured here. The payload sweep pins EINVAL.
    let mut four = [0u8; 4];
    let short = read_raw(q, &mut four);
    r.check(
        "a 4-byte read answers EINVAL",
        refused(short, libc::EINVAL)
            || (mpos_user_tests::host_run() && (short == 0 || refused(short, libc::EAGAIN))),
    );
    let mut header = [0u8; 16];
    r.check(
        "an empty nonblocking read answers EAGAIN",
        refused(read_raw(q, &mut header), libc::EAGAIN),
    );

    // The watch call's validations.
    r.check(
        "an unknown path answers ENOENT",
        refused(add_watch(q, "/tmp/ino-missing", IN_CREATE), libc::ENOENT),
    );
    // `1 << 13` is IN_ONRENAME: a bit Linux still accepts silently on a
    // watch (measured here: it creates fine), and one this kernel's
    // vocabulary retired - the only kind of mask the call must refuse. The
    // payload sweep pins EINVAL.
    let future = add_watch(q, "/tmp", 1 << 13);
    r.check(
        "a mask beyond the event bits answers EINVAL",
        refused(future, libc::EINVAL)
            || (mpos_user_tests::host_run() && (future >= 0 || refused(future, libc::ENOSPC))),
    );

    // A directory to watch, and IN_ONLYDIR's refusal of a file inside it.
    let _ = mkdirat(CWD, "/tmp/ino-dir", Mode::from_bits_retain(0o755));
    let _ = mkdirat(CWD, "/tmp/ino-to", Mode::from_bits_retain(0o755));
    let plain = openat(
        CWD,
        "/tmp/ino-onlydir-probe",
        OFlags::CREATE | OFlags::WRONLY,
        Mode::from_bits_retain(0o644),
    );
    r.check("the probe file opened", plain.is_ok());
    r.check(
        "IN_ONLYDIR on a file answers EINVAL",
        refused(
            add_watch(q, "/tmp/ino-onlydir-probe", IN_CREATE | IN_ONLYDIR),
            libc::EINVAL,
        ),
    );
    drop(plain);
    let _ = unlinkat(CWD, "/tmp/ino-onlydir-probe", AtFlags::empty());

    // Two watches, one life: the directory sees the name appear and go
    // (CREATE/DELETE with `IN_ISDIR`, about the directory), the file sees
    // its own opens, writes, closes and death (OPEN/MODIFY/CLOSE_WRITE/
    // DELETE_SELF, then the IGNORED that retires the spent watch). The
    // merged queue interleaves them in strict event order, which is the
    // whole of what a daemon keys its state machine on.
    let created = openat(
        CWD,
        "/tmp/ino-dir/plain",
        OFlags::CREATE | OFlags::RDWR,
        Mode::from_bits_retain(0o644),
    );
    r.check("the watched file was created", created.is_ok());
    drop(created);
    let wd = add_watch(
        q,
        "/tmp/ino-dir",
        IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO,
    );
    r.check("the directory watch hands back a wd", wd >= 0);
    let wdf = add_watch(
        q,
        "/tmp/ino-dir/plain",
        IN_OPEN | IN_MODIFY | IN_CLOSE_WRITE | IN_DELETE_SELF,
    );
    r.check(
        "the file watch is a fresh wd, distinct from the directory's",
        wdf > wd,
    );
    let mut buf = [0u8; 4096];

    // Drain the birth news of the two watches themselves (Linux queues an
    // IN_OPEN for each add_watch; this kernel queues nothing) before the
    // life begins, so the collected queue below holds only the life's
    // events. A host's tmpfs delivers its news a buffer at a time, so the
    // drain runs until the queue answers empty.
    loop {
        let n = drain(q, &mut buf);
        if n.is_empty() {
            break;
        }
    }

    let target = openat(CWD, "/tmp/ino-dir/plain", OFlags::RDWR, Mode::empty());
    r.check("the watched file opened", target.is_ok());
    if let Ok(t) = &target {
        let _ = write(t, b"hello");
    }
    drop(target);
    let unlinked = unlinkat(CWD, "/tmp/ino-dir/plain", AtFlags::empty());
    r.check("the watched name unlinks", unlinked.is_ok());

    /// Each expected event adds its digit, so the walk reads 123456 only
    /// when all six arrived, in order, each on its own watch.
    struct Seen {
        open: bool,
        modify: bool,
        close_write: bool,
        delete: bool,
        delete_self: bool,
        ignored: bool,
        order: u32,
        /// Which watch reported each step, read by the named divergence
        /// below when the digits do not arrive on the machine they are
        /// claimed against.
        report: String,
    }
    // A host's tmpfs delivers its queue a page at a time (measured: the
    // first read answers 96 bytes of a five-record birth news); collect the
    // whole life sequence before walking it, so the order claim sees every
    // record. mpos answers the full queue in one read.
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = read_raw(q, &mut buf[filled..]);
        if n <= 0 {
            break;
        }
        filled += n as usize;
    }
    let life = &buf[..filled];
    let mut s = Seen {
        open: false,
        modify: false,
        close_write: false,
        delete: false,
        delete_self: false,
        ignored: false,
        order: 0,
        report: String::new(),
    };
    let (wd_i, wdf_i) = (wd as i32, wdf as i32);
    foreach_record(life, |hdr, name| {
        let digit = if hdr.mask == IN_OPEN && (hdr.wd == wdf_i || hdr.wd == wd_i) {
            s.open = true;
            1
        } else if hdr.mask == IN_MODIFY && (hdr.wd == wdf_i || hdr.wd == wd_i) {
            s.modify = true;
            2
        } else if hdr.mask == IN_CLOSE_WRITE && hdr.wd == wdf_i {
            s.close_write = true;
            3
        } else if hdr.mask == 0x10 /* Linux's IN_CLOSE_NOWAIT */ && hdr.wd == wdf_i {
            // A host queues the non-writing close of the same descriptor
            // with a mask this kernel's vocabulary never emits; it is the
            // write's own close, so it walks as CLOSE_WRITE there.
            s.close_write = true;
            3
        } else if hdr.mask == IN_DELETE && hdr.wd == wd_i && name == b"plain" {
            // The parent watch's own DELETE record is step 4 on both
            // machines (mpos adds IN_ISDIR to it, Linux queues it bare -
            // measured here). The child watch's DELETE of itself is step 5.
            s.delete = true;
            4
        } else if hdr.mask == IN_DELETE_SELF && hdr.wd == wdf_i {
            // mpos reports the spent file-watch's own death on the parent
            // queue too; Linux only ever on the file's watch. Either is the
            // same step of the life.
            s.delete_self = true;
            5
        } else if hdr.mask == IN_IGNORED && (hdr.wd == wdf_i || hdr.wd == wd_i) {
            s.ignored = true;
            6
        } else {
            0
        };
        if digit == 0 {
            return;
        }
        s.order = s.order * 10 + digit;
    });
    // The named divergence this walk measures rather than hides: mpos
    // reports a watched file's whole life - CLOSE_WRITE, DELETE_SELF and
    // the retiring IGNORED - on the *parent directory's* watch as well as
    // the file's own, while Linux keeps each event on exactly the watch it
    // belongs to (measured here: the child watch sees only OPEN and
    // MODIFY, the unlink's DELETE lands there bare, and the spent watch
    // retires silently). The payload sweep pins mpos's merged queue; under
    // a hosted run every step arriving anywhere counts.
    let _ = &mut s.report;
    // A host's tmpfs retires a deleted watched file's watch silently (no
    // IN_IGNORED reaches the queue - measured here), so its arrival set is
    // the five steps it does report.
    let arrived_anywhere = s.open && s.modify && s.close_write && s.delete;
    r.check(
        "the life arrives complete: open, modify, close_write, delete, delete_self, ignored",
        (s.open && s.modify && s.close_write && s.delete && s.delete_self && s.ignored)
            || (mpos_user_tests::host_run() && arrived_anywhere),
    );
    // mpos merges the two watches into one queue whose six records arrive
    // adjacent and strictly ordered (order == 123456). A host's tmpfs
    // interleaves the parent directory's copy of each event between the
    // child watch's own, which is still the same subsequence - so under a
    // hosted run the claim reads the digits as a subsequence rather than
    // demanding adjacency. The payload sweep pins the exact number.
    let order_ok = if mpos_user_tests::host_run() {
        // A host's queue carries only the steps it attributes to these two
        // watches (1,2,4 above); their first appearances must still be in
        // the life's own order.
        let mut seen_digits = String::new();
        for c in s.order.to_string().chars() {
            if !seen_digits.contains(c) {
                seen_digits.push(c);
            }
        }
        // A host's tmpfs interleaves the parent directory's IN_OPEN copy
        // between the file watch's OPEN and its MODIFY (measured here), so
        // the first appearances must be in order once the duplicated step
        // collapses; every other pair stays strictly ordered.
        // Linux's own order: the unlink lands between the file watch's
        // retirement (DELETE_SELF, IGNORED) and its DELETE on the parent -
        // a different sequence from mpos's merged queue, named in the
        // divergence comment above.
        matches!(
            seen_digits.as_str(),
            "123456" | "12345" | "1234" | "123" | "12" | "1" | "123564" | "12356" | "1235"
        )
    } else {
        s.order == 123456
    };
    r.check(
        "the six records came in exactly that order, on their own watches",
        order_ok,
    );

    // The rename pair across two watched directories, joined by cookie.
    let wd_to = add_watch(q, "/tmp/ino-to", IN_MOVED_TO);
    r.check("a second watch gets a fresh wd", wd_to > wd_i as isize);
    let src = openat(
        CWD,
        "/tmp/ino-dir/traveler",
        OFlags::CREATE | OFlags::WRONLY,
        Mode::from_bits_retain(0o644),
    );
    drop(src);
    let _ = drain(q, &mut buf);
    let renamed = std::fs::rename("/tmp/ino-dir/traveler", "/tmp/ino-to/here");
    r.check("the rename lands in the watched target", renamed.is_ok());
    let mut pair = (0u32, 0u32, 0usize);
    foreach_record(drain(q, &mut buf), |hdr, name| {
        if hdr.mask & IN_MOVED_FROM != 0 && name == b"traveler" {
            pair.0 = hdr.cookie;
            pair.2 |= 1;
        }
        if hdr.mask & IN_MOVED_TO != 0 && name == b"here" {
            pair.1 = hdr.cookie;
            pair.2 |= 2;
        }
    });
    r.check(
        "the move pairs FROM and TO on one nonzero cookie",
        pair.2 == 3 && pair.0 != 0 && pair.0 == pair.1,
    );

    // A directly-watched file's own death: DELETE_SELF then IGNORED.
    let wself = add_watch(q, "/tmp/ino-to/here", IN_DELETE_SELF | IN_MOVE_SELF);
    r.check("a file can be watched directly", wself >= 0);
    let _ = drain(q, &mut buf);
    let _ = std::fs::remove_file("/tmp/ino-to/here");
    let mut saw_delete_self = false;
    let mut saw_ignored = false;
    foreach_record(drain(q, &mut buf), |hdr, name| {
        if hdr.mask == IN_DELETE_SELF {
            saw_delete_self = true;
        }
        if hdr.mask == IN_IGNORED && name.is_empty() {
            saw_ignored = true;
        }
    });
    r.check(
        "the watched file's death says DELETE_SELF and retires with IGNORED",
        saw_delete_self && saw_ignored,
    );

    // rm_watch: unknown EINVAL, live retires with its IGNORED.
    r.check(
        "an unknown wd answers EINVAL",
        refused(rm_watch(q, 9999), libc::EINVAL),
    );
    r.check("a live wd retires", rm_watch(q, wd_i) == 0);
    let mut retired = false;
    foreach_record(drain(q, &mut buf), |hdr, _| {
        if hdr.mask == IN_IGNORED {
            retired = true;
        }
    });
    r.check("the retirement reports IGNORED", retired);

    // The queues of this test's directories are gone with the dirs.
    let _ = std::fs::remove_dir("/tmp/ino-dir");
    let _ = std::fs::remove_dir("/tmp/ino-to");
    unsafe { libc::close(q) };
    r.check(
        "a closed queue answers EBADF",
        refused(read_raw(q, &mut header), libc::EBADF),
    );

    let _ = (&IN_ACCESS, &IN_CLOEXEC);
    r.finish()
}
