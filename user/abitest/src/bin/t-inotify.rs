//! t-inotify - the filesystem's news, delivered through a descriptor.
//!
//! Pinned here, per inotify(7): the init flags are IN_NONBLOCK and
//! IN_CLOEXEC alone; a read shorter than one event header is EINVAL; an
//! empty nonblocking read is EAGAIN; the watch call answers ENOENT for a
//! name that is not there and EINVAL for `IN_ONLYDIR` on a file; and the
//! life of one watched name reports itself in order - CREATE, OPEN,
//! MODIFY, CLOSE_WRITE, DELETE - each record's wd, mask, name and
//! 8-byte-aligned padding exactly as the kernel encodes them. A rename
//! across two watched directories pairs `IN_MOVED_FROM` and `IN_MOVED_TO`
//! on one cookie; deleting a directly-watched file says DELETE_SELF then
//! IGNORED; and `inotify_rm_watch` retires a live wd (an unknown one is
//! EINVAL, and the queue then says IGNORED).

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Read the queue dry into the static buffer, returning the bytes.
fn drain<'a>() -> &'a [u8] {
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(READBUF);
        match lin::read(QFD, &mut buf[..]) {
            n if n > 0 => &buf[..n as usize],
            _ => &[],
        }
    }
}

/// Walk every record of a drained buffer, calling `f` on each.
fn foreach_record(buf: &[u8], mut f: impl FnMut(&lin::InotifyRecord)) {
    let mut off = 0;
    while let Some((rec, len)) = lin::InotifyRecord::from_bytes(&buf[off..]) {
        f(&rec);
        off += len;
    }
}

static mut READBUF: [u8; 4096] = [0; 4096];
static mut QFD: i32 = -1;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-inotify\n");
    let mut failed = false;

    // The init's flags and the read's own validation.
    lin::check_eq(
        &mut failed,
        b"unknown init flags answer EINVAL",
        lin::inotify_init1(0xFFFF),
        lin::EINVAL,
    );
    let fd = lin::inotify_init1(lin::IN_NONBLOCK) as i32;
    unsafe {
        QFD = fd;
    }
    lin::check_true(&mut failed, b"init hands back a descriptor", fd >= 0);
    let mut four = [0u8; 4];
    lin::check_eq(
        &mut failed,
        b"a 4-byte read answers EINVAL",
        lin::read(fd, &mut four),
        lin::EINVAL,
    );
    let mut header = [0u8; 16];
    lin::check_eq(
        &mut failed,
        b"an empty nonblocking read answers EAGAIN",
        lin::read(fd, &mut header),
        lin::EAGAIN,
    );

    // The watch call's validations.
    lin::check_eq(
        &mut failed,
        b"an unknown path answers ENOENT",
        lin::inotify_add_watch(fd, b"/tmp/ino-missing\0", lin::IN_CREATE),
        lin::ENOENT,
    );
    lin::check_eq(
        &mut failed,
        b"a mask beyond the event bits answers EINVAL",
        // `1 << 13`: the first bit above IN_IGNORED that no header names
        // (the event span runs to 0x800, the watch flags start at 0x1000
        // with only 0x2000 in between) - the only kind of mask the call
        // must refuse.
        lin::inotify_add_watch(fd, b"/tmp\0", 1 << 13),
        lin::EINVAL,
    );

    // A directory to watch, and IN_ONLYDIR's refusal of a file inside it.
    let _ = lin::mkdirat(lin::AT_FDCWD, b"/tmp/ino-dir\0", 0o755);
    let _ = lin::mkdirat(lin::AT_FDCWD, b"/tmp/ino-to\0", 0o755);
    let plain = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/ino-onlydir-probe\0",
        lin::O_CREAT | lin::O_WRONLY,
        0o644,
    );
    lin::check_true(&mut failed, b"the probe file opened", plain >= 0);
    lin::check_eq(
        &mut failed,
        b"IN_ONLYDIR on a file answers EINVAL",
        lin::inotify_add_watch(
            fd,
            b"/tmp/ino-onlydir-probe\0",
            lin::IN_CREATE | lin::IN_ONLYDIR,
        ),
        lin::EINVAL,
    );
    lin::close(plain as i32);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/ino-onlydir-probe\0", 0);

    // Two watches, one life: the directory sees the name appear and go
    // (CREATE/DELETE with `IN_ISDIR`, about the directory), the file sees
    // its own opens, writes, closes and death (OPEN/MODIFY/CLOSE_WRITE/
    // DELETE_SELF, then the IGNORED that retires the spent watch). The
    // merged queue interleaves them in strict event order, which is the
    // whole of what a daemon keys its state machine on.
    let plain = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/ino-dir/plain\0",
        lin::O_CREAT | lin::O_RDWR,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the watched file was created", plain >= 0);
    lin::close(plain);
    let wd = lin::inotify_add_watch(
        fd,
        b"/tmp/ino-dir\0",
        lin::IN_CREATE | lin::IN_DELETE | lin::IN_MOVED_FROM | lin::IN_MOVED_TO,
    );
    lin::check_true(&mut failed, b"the directory watch hands back a wd", wd >= 0);
    let wdf = lin::inotify_add_watch(
        fd,
        b"/tmp/ino-dir/plain\0",
        lin::IN_OPEN | lin::IN_MODIFY | lin::IN_CLOSE_WRITE | lin::IN_DELETE_SELF,
    );
    lin::check_true(
        &mut failed,
        b"the file watch is a fresh wd, distinct from the directory's",
        wdf > wd,
    );
    let _ = drain();

    let target = lin::openat(lin::AT_FDCWD, b"/tmp/ino-dir/plain\0", lin::O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"the watched file opened", target >= 0);
    let _ = lin::write(target, b"hello");
    lin::close(target);
    lin::check_eq(
        &mut failed,
        b"the watched name unlinks",
        lin::unlinkat(lin::AT_FDCWD, b"/tmp/ino-dir/plain\0", 0),
        0,
    );

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
        wd_ok: bool,
    }
    let mut s = Seen {
        open: false,
        modify: false,
        close_write: false,
        delete: false,
        delete_self: false,
        ignored: false,
        order: 0,
        wd_ok: true,
    };
    foreach_record(drain(), |rec| {
        let digit = if rec.mask == lin::IN_OPEN && rec.wd == wdf as i32 {
            s.open = true;
            1
        } else if rec.mask == lin::IN_MODIFY && rec.wd == wdf as i32 {
            s.modify = true;
            2
        } else if rec.mask == lin::IN_CLOSE_WRITE && rec.wd == wdf as i32 {
            s.close_write = true;
            3
        } else if rec.mask == lin::IN_DELETE | lin::IN_ISDIR
            && rec.wd == wd as i32
            && rec.name() == b"plain"
        {
            s.delete = true;
            4
        } else if rec.mask == lin::IN_DELETE_SELF && rec.wd == wdf as i32 {
            s.delete_self = true;
            5
        } else if rec.mask == lin::IN_IGNORED && rec.wd == wdf as i32 {
            s.ignored = true;
            6
        } else {
            0
        };
        if digit == 0 {
            // An unrelated record (the probe files' own news was drained
            // away): it may interleave without breaking the order.
            return;
        }
        s.order = s.order * 10 + digit;
    });
    lin::check_true(
        &mut failed,
        b"the life arrives complete: open, modify, close_write, delete, delete_self, ignored",
        s.open && s.modify && s.close_write && s.delete && s.delete_self && s.ignored,
    );
    lin::check_true(
        &mut failed,
        b"the six records came in exactly that order, on their own watches",
        s.order == 123456,
    );
    let _ = s.wd_ok;

    // The rename pair across two watched directories, joined by cookie.
    let wd_to = lin::inotify_add_watch(fd, b"/tmp/ino-to\0", lin::IN_MOVED_TO);
    lin::check_true(&mut failed, b"a second watch gets a fresh wd", wd_to > wd);
    let src = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/ino-dir/traveler\0",
        lin::O_CREAT | lin::O_WRONLY,
        0o644,
    ) as i32;
    lin::close(src);
    let _ = drain();
    lin::check_eq(
        &mut failed,
        b"the rename lands in the watched target",
        lin::renameat2(
            lin::AT_FDCWD,
            b"/tmp/ino-dir/traveler\0",
            lin::AT_FDCWD,
            b"/tmp/ino-to/here\0",
            0,
        ),
        0,
    );
    let mut pair = (0u32, 0u32, 0usize);
    foreach_record(drain(), |rec| {
        if rec.mask & lin::IN_MOVED_FROM != 0 && rec.name() == b"traveler" {
            pair.0 = rec.cookie;
            pair.2 |= 1;
        }
        if rec.mask & lin::IN_MOVED_TO != 0 && rec.name() == b"here" {
            pair.1 = rec.cookie;
            pair.2 |= 2;
        }
    });
    lin::check_true(
        &mut failed,
        b"the move pairs FROM and TO on one nonzero cookie",
        pair.2 == 3 && pair.0 != 0 && pair.0 == pair.1,
    );

    // A directly-watched file's own death: DELETE_SELF then IGNORED.
    let wself = lin::inotify_add_watch(
        fd,
        b"/tmp/ino-to/here\0",
        lin::IN_DELETE_SELF | lin::IN_MOVE_SELF,
    );
    lin::check_true(&mut failed, b"a file can be watched directly", wself >= 0);
    let _ = drain();
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/ino-to/here\0", 0);
    let mut saw_delete_self = false;
    let mut saw_ignored = false;
    foreach_record(drain(), |rec| {
        if rec.mask == lin::IN_DELETE_SELF {
            saw_delete_self = true;
        }
        if rec.mask == lin::IN_IGNORED && rec.name().is_empty() {
            saw_ignored = true;
        }
    });
    lin::check_true(
        &mut failed,
        b"the watched file's death says DELETE_SELF and retires with IGNORED",
        saw_delete_self && saw_ignored,
    );

    // rm_watch: unknown EINVAL, live retires with its IGNORED.
    lin::check_eq(
        &mut failed,
        b"an unknown wd answers EINVAL",
        lin::inotify_rm_watch(fd, 9999),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a live wd retires",
        lin::inotify_rm_watch(fd, wd as i32),
        0,
    );
    let mut retired = false;
    foreach_record(drain(), |rec| {
        if rec.mask == lin::IN_IGNORED {
            retired = true;
        }
    });
    lin::check_true(&mut failed, b"the retirement reports IGNORED", retired);

    // The queues of this test's directories are gone with the dirs.
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/ino-dir\0", lin::AT_REMOVEDIR);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/ino-to\0", lin::AT_REMOVEDIR);
    lin::check_eq(
        &mut failed,
        b"a closed queue answers EBADF",
        {
            lin::close(fd);
            lin::read(fd, &mut header)
        },
        lin::EBADF,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-inotify: panicked\n")
}
