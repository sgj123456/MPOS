//! t-epoll - the interest-list multiplexer, end to end.
//!
//! Pinned here, per epoll_create(2)/epoll_ctl(2)/epoll_wait(2): the legacy
//! create's size must be positive, create1 takes EPOLL_CLOEXEC alone, the
//! three operations answer EEXIST/ENOENT in the man page's cases, an open
//! number that is not an epoll is EINVAL (not EBADF), a wait with a
//! non-positive maxevents is EINVAL, and a pipe that receives bytes wakes
//! a wait carrying the caller's data word verbatim. The one-shot reports
//! once and needs a MOD to report again; closing a watched descriptor
//! ends its entry.
//!
//! # The migration
//!
//! `rustix::event::epoll` binds the calls but cannot state most of these
//! claims, all clause (c) shapes rather than missing bindings:
//!
//! * **the operation word.** rustix splits `epoll_ctl` into `add`, `modify`
//!   and `delete`, so "an unknown operation answers EINVAL before anything is
//!   looked at" - op 7 - has no spelling through any of them, and "DEL with a
//!   null event pointer" is unreachable because `delete` builds its own.
//! * **a target that is not open.** Every wrapper takes `impl AsFd`, which
//!   cannot name descriptor 90 or 99; `BorrowedFd::borrow_raw` refuses a
//!   closed number by contract, so the EBADF and ENOENT probes have nowhere
//!   to go inside the typed API.
//! * **the wait's maxevents.** rustix derives it from the buffer's length, so
//!   "maxevents 0 answers EINVAL" while handing over eight records is
//!   inexpressible - and that decoupling is exactly what the claim is about.
//! * **the legacy create.** `epoll_create(size)`'s positive-size rule is not
//!   a thing rustix exposes at all (its `create` is create1).
//!
//! So the program rides the raw door - `SYS_epoll_create1 / SYS_epoll_ctl` on
//! both machines, the wait as `SYS_epoll_wait` here where arm64 carries only
//! `SYS_epoll_pwait` (`nr_aarch64.rs`: `EPOLL_WAIT = 64029; // no arm64
//! number`, `EPOLL_PWAIT = 22`) and musl's own poll-side spelling is the pwait
//! form with a null mask, and the legacy create on the one machine that has a
//! number for it. arm64's only door for that one is musl's own size check,
//! which is measured rather than assumed (`create_legacy` below): the wrapper
//! is not a passthrough on either machine, so the claim states the answer a
//! caller gets, which is EINVAL on both. Every flag word (`EPOLLIN`,
//! `EPOLLONESHOT`, `EPOLLHUP`, `EPOLL_CLOEXEC`) comes from
//! `rustix::event::epoll`'s bitflags; the event record is the kernel's own
//! packed struct. The answers are negative errnos read at the call, because
//! musl's door collapses every refusal to -1 and leaves the number in errno -
//! and this program's claims are about *which* refusal.
//!
//! The hang-up half keeps its whole reason for existing below: udev's
//! `spawn_wait` parks on a missing EPOLLHUP, and the compositor loses its
//! keyboard for two minutes.

use mpos_user_tests::Report;
use rustix::event::epoll::{CreateFlags, EventFlags};
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::io::{read, write};
use rustix::pipe::pipe;

/// The *machine's* record, which the ctl hands over and the wait writes back.
///
/// Linux packs `struct epoll_event` on x86_64 only - twelve bytes with `data` at
/// byte 4 there, sixteen with `data` at byte 8 on every other machine
/// (`EPOLL_PACKED` is `#if defined(__x86_64__)` in the uapi, and musl's
/// `sys/epoll.h` copies that guard) - so a test that hard-codes *either* shape
/// agrees with a kernel that writes the wrong one. That is what this file did:
/// it asserted the x86_64 layout on both machines, so the aarch64 machine could
/// write it and no claim here could tell. The shape below is the machine's, and
/// the claims that use it - the caller's data word arriving verbatim - are what
/// fail if a kernel writes the other machine's offsets.
#[cfg(target_arch = "x86_64")]
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}

const ADD: i32 = 1;
const DEL: i32 = 2;
const MOD: i32 = 3;

/// The 64-bit data word the claims below register and read back.
///
/// Every bit of it matters, and that is the correction the aarch64 bug forced:
/// a word whose *high* half is zero survives a kernel that reads `data` four
/// bytes early and writes it four bytes early, because the two errors cancel -
/// so the small value this file used to register (`0x1234`) pinned the round
/// trip and not the layout, and the kernel's aarch64 half was wrong behind it.
const DATA: u64 = 0x0123_4567_89ab_cdef;

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Turn musl's `-1 + errno` door into the no-std harness's negated-errno
/// answer, read while the failing call is still the last one made.
fn fix(r: libc::c_long) -> isize {
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

/// `epoll_create1(flags)` through the raw door.
fn create1(flags: u32) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_epoll_create1, flags as libc::c_long) };
    fix(r)
}

/// `epoll_create(size)` - the legacy form, whose only promise is the
/// positive-size rule. The door is the machine's own, because neither
/// library spells this call as a passthrough: measured by disassembling the
/// crate's own static `libc.a`, musl's `epoll_create` is `size <= 0 ?
/// -EINVAL : epoll_create1(0)` on **both** machines (`cmp; b.le` to a
/// `-EINVAL` return, otherwise a tail call into `epoll_create1`), so the
/// wrapper never reaches a kernel's positive-size rule at all. On the first
/// machine that rule is still the subject, through the raw `SYS_epoll_create`
/// (213); on the second there is no number to reach (`EPOLL_CREATE = 64028;
/// // no arm64 number; unreachable on aarch64`), so musl's own check is the
/// only door a caller has. Both answer EINVAL, which is what the claim says.
#[cfg(target_arch = "x86_64")]
fn create_legacy(size: i32) -> i32 {
    let r = unsafe { libc::syscall(libc::SYS_epoll_create, size as libc::c_long) };
    fix(r) as i32
}

#[cfg(not(target_arch = "x86_64"))]
fn create_legacy(size: i32) -> i32 {
    let r = unsafe { libc::epoll_create(size) };
    if r < 0 { -last_errno() } else { r }
}

/// `epoll_ctl(epfd, op, fd, event)` with the caller's words, including the
/// ones no wrapper can spell: an unknown op, a closed target, a null event.
fn ctl(epfd: i32, op: i32, fd: i32, ev: Option<&Ev>) -> isize {
    let p = match ev {
        Some(e) => e as *const Ev as libc::c_long,
        None => 0,
    };
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_ctl,
            epfd as libc::c_long,
            op as libc::c_long,
            fd as libc::c_long,
            p,
        )
    };
    fix(r)
}

/// `epoll_wait(epfd, events, maxevents, timeout)` with the caller's
/// maxevents - decoupled from the buffer, which is the point of two claims.
/// On the second machine this is `epoll_pwait` with a null signal mask,
/// arm64's only spelling of the wait; the words that matter stay the caller's.
fn wait(epfd: i32, events: &mut [Ev], maxevents: i32, timeout: i32) -> isize {
    #[cfg(target_arch = "x86_64")]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_wait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            maxevents as libc::c_long,
            timeout as libc::c_long,
        )
    };
    #[cfg(not(target_arch = "x86_64"))]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_pwait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            maxevents as libc::c_long,
            timeout as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    fix(r)
}

/// The record a ctl hands over.
fn ev(flags: EventFlags, data: u64) -> Ev {
    Ev {
        events: flags.bits(),
        data,
    }
}

fn main() {
    println!("t-epoll");
    let mut r = Report::new();

    // The layout claim the record rests on: the caller's array is byte-for-byte
    // the *machine's* struct, which is twelve bytes with `data` at 4 on x86_64
    // and sixteen with `data` at 8 on aarch64. A kernel that writes the other
    // machine's offsets fails here, and then fails the data claims below - the
    // shape the aarch64 bug had before this claim existed.
    let (record_size, record_data) = if cfg!(target_arch = "x86_64") {
        (12, 4)
    } else {
        (16, 8)
    };
    r.check(
        "the event record is this machine's own epoll_event",
        core::mem::size_of::<Ev>() == record_size && core::mem::offset_of!(Ev, data) == record_data,
    );

    // The two creations: the legacy size argument is ignored beyond having to
    // be positive - on both machines' doors, see `create_legacy` - and create1
    // takes EPOLL_CLOEXEC alone.
    r.check(
        "epoll_create with a zero size answers EINVAL",
        create_legacy(0) == -libc::EINVAL,
    );
    r.check(
        "epoll_create1 with unknown flags answers EINVAL",
        create1(CreateFlags::from_bits_retain(0x80).bits()) == -libc::EINVAL as isize,
    );
    let epfd = create1(CreateFlags::empty().bits()) as i32;
    r.check("epoll_create1 hands back a descriptor", epfd >= 0);

    // A pipe to watch, and the entry for its read end.
    let pair = pipe();
    r.check("the pipe pair is made", pair.is_ok());
    if !pair.is_ok() {
        r.finish()
    }
    let (rin, wout) = pair.unwrap();
    let (rin, wout) = (rin.as_raw_fd(), wout.as_raw_fd());

    let e = ev(EventFlags::IN, 0);
    r.check(
        "ADD of the read end succeeds",
        ctl(epfd, ADD, rin, Some(&e)) == 0,
    );
    // The same pair again: EEXIST, the man page's own answer.
    r.check(
        "a second ADD of the pair answers EEXIST",
        ctl(epfd, ADD, rin, Some(&e)) == -libc::EEXIST as isize,
    );

    // MOD and DEL of numbers that never registered. The man page's answer is
    // ENOENT, and on this kernel it is; on a host the number is simply
    // *unbound* (nothing was ever open at 90), and Linux refuses an unbound
    // fd at the door before the interest list is consulted - EBADF, measured
    // here (errno 9). Both refusals state one fact, no entry exists for that
    // name, so the claim carries the pair with its measurement instead of
    // writing the host's table size as this kernel's rule.
    let m = ctl(epfd, MOD, 90, Some(&e));
    r.check(
        "MOD of an unregistered fd answers ENOENT, or EBADF where the number is unbound",
        m == -libc::ENOENT as isize || m == -libc::EBADF as isize,
    );
    let d = ctl(epfd, DEL, 90, None);
    r.check(
        "DEL of an unregistered fd answers the same pair",
        d == -libc::ENOENT as isize || d == -libc::EBADF as isize,
    );

    // A number that is not an epoll: EINVAL either way, the line the man
    // page draws between a bad number and the wrong kind of thing.
    r.check(
        "epoll_ctl on a pipe answers EINVAL",
        ctl(wout, ADD, rin, Some(&e)) == -libc::EINVAL as isize,
    );
    let mut events = [ev(EventFlags::empty(), 0); 8];
    r.check(
        "epoll_wait on a pipe answers EINVAL",
        wait(wout, &mut events, 8, 0) == -libc::EINVAL as isize,
    );

    // A target that is not open: EBADF, after the operation is known.
    r.check(
        "ADD of a closed number answers EBADF",
        ctl(epfd, ADD, 99, Some(&e)) == -libc::EBADF as isize,
    );
    // An operation outside the three: EINVAL, before anything is looked at.
    r.check(
        "an unknown operation answers EINVAL",
        ctl(epfd, 7, rin, Some(&e)) == -libc::EINVAL as isize,
    );

    // A wait with a non-positive maxevents is EINVAL; an empty interest
    // list with a zero timeout is the immediate zero.
    r.check(
        "epoll_wait with maxevents 0 answers EINVAL",
        wait(epfd, &mut events, 0, 0) == -libc::EINVAL as isize,
    );
    r.check(
        "a zero-timeout wait on an empty pipe reports nothing",
        wait(epfd, &mut events, 8, 0) == 0,
    );

    // Data arrives, and the wait wakes carrying the caller's data word.
    let wrote = write(unsafe { BorrowedFd::borrow_raw(wout) }, b"ping").is_ok_and(|n| n == 4);
    r.check("the pipe takes the bytes", wrote);
    // The data word is deliberately one whose *high* half is not zero, and that
    // is a correction this claim needed: a value below 2^32 round-trips through
    // either layout, because the kernel's read and its write are wrong in
    // opposite directions and cancel - 0x1234 came back as 0x1234 from a kernel
    // that read `data` at byte 4 and the caller read it at byte 8. A word with
    // all sixty-four bits set in it cannot cancel: udev's registered pointer did
    // not, which is how the aarch64 half of this layout was found at all.
    let e = ev(EventFlags::IN, DATA);
    r.check(
        "MOD setting the data word succeeds",
        ctl(epfd, MOD, rin, Some(&e)) == 0,
    );
    let n = wait(epfd, &mut events, 8, 200);
    r.check("the wait reports the readable pipe", n == 1);
    if n == 1 {
        r.check(
            "the report carries EPOLLIN",
            EventFlags::from_bits_retain(events[0].events).contains(EventFlags::IN),
        );
        r.check("the report carries the data word", events[0].data == DATA);
    }

    // Drained, and reported once already: a zero-timeout look sees nothing.
    let mut buf = [0u8; 4];
    let drained = read(unsafe { BorrowedFd::borrow_raw(rin) }, &mut buf).is_ok_and(|n| n == 4);
    r.check("the drain reads the 4 bytes", drained);
    r.check(
        "a drained pipe reports nothing to a zero-timeout wait",
        wait(epfd, &mut events, 8, 0) == 0,
    );

    // The one-shot: reported once, dark until a MOD re-arms it.
    let e = ev(EventFlags::IN | EventFlags::ONESHOT, 7);
    r.check(
        "MOD arming the one-shot succeeds",
        ctl(epfd, MOD, rin, Some(&e)) == 0,
    );
    r.check(
        "the pipe takes more bytes",
        write(unsafe { BorrowedFd::borrow_raw(wout) }, b"pong").is_ok_and(|n| n == 4),
    );
    r.check(
        "the one-shot reports its one event",
        wait(epfd, &mut events, 8, 200) == 1,
    );
    r.check(
        "the spent one-shot reports nothing while it is ready",
        wait(epfd, &mut events, 8, 0) == 0,
    );
    let e = ev(EventFlags::IN | EventFlags::ONESHOT, 7);
    r.check(
        "the MOD that re-arms succeeds",
        ctl(epfd, MOD, rin, Some(&e)) == 0,
    );
    r.check(
        "the re-armed one-shot reports again",
        wait(epfd, &mut events, 8, 200) == 1,
    );

    // The hang-up: reported whether or not the caller asked for it, and the
    // *whole* report once the pipe is drained. Both halves matter to the same
    // waiter - udev's `spawn_wait`, which watches its child's pipe with
    // epoll, takes a read of 0 as "look again" and ends the wait on EPOLLHUP
    // alone - so an end-of-stream reported as EPOLLIN alone left the device
    // manager parked on every `IMPORT{program}` rule until its event timeout
    // expired, and the `ID_INPUT` tag a compositor needs arrived two minutes
    // into the boot instead of with the device.
    let gone = pipe();
    r.check("a second pipe is made", gone.is_ok());
    if let Ok((g_r, g_w)) = &gone {
        let (g_r, g_w) = (g_r.as_raw_fd(), g_w.as_raw_fd());
        let e = ev(EventFlags::IN, DATA + 1);
        r.check("its read end is added", ctl(epfd, ADD, g_r, Some(&e)) == 0);
        r.check("the writer is closed", unsafe { libc::close(g_w) } == 0);
        let n = wait(epfd, &mut events, 8, 200);
        r.check("a hang-up ends the wait", n == 1);
        if n == 1 {
            r.check(
                "it carries EPOLLHUP though only EPOLLIN was asked for",
                EventFlags::from_bits_retain(events[0].events).contains(EventFlags::HUP),
            );
            r.check(
                "the caller's data word rides with it",
                events[0].data == DATA + 1,
            );
        }
        // A second look finds the hang-up still there: it is a state, not an
        // edge, and a wait that missed it once must be able to see it again.
        r.check(
            "the hang-up reports again",
            wait(epfd, &mut events, 8, 0) == 1,
        );
    }

    // Closing a watched descriptor ends its entry: the operations that
    // name it answer ENOENT, the shape Linux's close-time removal gives.
    r.check(
        "closing the watched read end succeeds",
        unsafe { libc::close(rin) } == 0,
    );
    // Measured on the host: the closed number answers EBADF (errno 9), not
    // ENOENT - Linux removes the entry when the watched *file* dies, and
    // until then a name no longer bound is refused at the door rather than
    // looked up in the list. The payload boot, whose fd table holds nothing
    // else on that inode at this point, answers ENOENT for the same call.
    // Both say the entry is gone, so the claim is stated as the pair with its
    // measurement instead of one number written as the whole rule.
    let e = ev(EventFlags::IN, 0);
    r.check(
        "MOD of the closed entry answers ENOENT or EBADF - either way the entry is gone",
        matches!(ctl(epfd, MOD, rin, Some(&e)), v if v == -libc::ENOENT as isize || v == -libc::EBADF as isize),
    );

    unsafe {
        libc::close(wout);
        libc::close(epfd);
    }
    r.finish()
}
