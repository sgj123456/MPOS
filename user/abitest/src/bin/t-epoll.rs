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

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut EVENTS: [lin::EpollEvent; 8] = [lin::EpollEvent {
    events: 0,
    data: 0,
}; 8];

/// The record a ctl hands over, by value: the struct is packed and its
/// `data` sits at byte 4, and a value copy is the safe way to touch it.
fn event(events: u32, data: u64) -> lin::EpollEvent {
    lin::EpollEvent { events, data }
}

/// Field `i` of the collected wait, copied out of the packed records.
fn events_of(index: usize) -> u32 {
    unsafe { EVENTS[index].events }
}

fn data_of(index: usize) -> u64 {
    unsafe { EVENTS[index].data }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-epoll\n");
    let mut failed = false;

    // The two creations: the legacy size argument is ignored but must be
    // positive, and create1 takes EPOLL_CLOEXEC alone.
    lin::check_eq(
        &mut failed,
        b"epoll_create with a zero size answers EINVAL",
        lin::epoll_create(0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"epoll_create1 with unknown flags answers EINVAL",
        lin::epoll_create1(0x80),
        lin::EINVAL,
    );
    let epfd = lin::epoll_create1(0) as i32;
    lin::check_true(&mut failed, b"epoll_create1 hands back a descriptor", epfd >= 0);

    // A pipe to watch, and the entry for its read end.
    let mut fds: [i32; 2] = [-1; 2];
    lin::check_eq(&mut failed, b"the pipe pair is made", lin::pipe2(&mut fds, 0), 0);

    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_ADD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"ADD of the read end succeeds", r, 0);
    // The same pair again: EEXIST, the man page's own answer.
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_ADD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"a second ADD of the pair answers EEXIST", r, lin::EEXIST);

    // MOD and DEL of numbers that never registered: ENOENT.
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_MOD, 90, unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"MOD of an unregistered fd answers ENOENT", r, lin::ENOENT);
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_DEL, 90, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"DEL of an unregistered fd answers ENOENT", r, lin::ENOENT);

    // A number that is not an epoll: EINVAL either way, the line the man
    // page draws between a bad number and the wrong kind of thing.
    let r = lin::epoll_ctl(fds[1], lin::EPOLL_CTL_ADD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"epoll_ctl on a pipe answers EINVAL", r, lin::EINVAL);
    let r = lin::epoll_wait(fds[1], unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 0);
    lin::check_eq(&mut failed, b"epoll_wait on a pipe answers EINVAL", r, lin::EINVAL);

    // A target that is not open: EBADF, after the operation is known.
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_ADD, 99, unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"ADD of a closed number answers EBADF", r, lin::EBADF);
    // An operation outside the three: EINVAL, before anything is looked at.
    let r = lin::epoll_ctl(epfd, 7, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"an unknown operation answers EINVAL", r, lin::EINVAL);

    // A wait with a non-positive maxevents is EINVAL; an empty interest
    // list with a zero timeout is the immediate zero.
    lin::check_eq(
        &mut failed,
        b"epoll_wait with maxevents 0 answers EINVAL",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 0, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a zero-timeout wait on an empty pipe reports nothing",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 0),
        0,
    );

    // Data arrives, and the wait wakes carrying the caller's data word.
    lin::check_eq(&mut failed, b"the pipe takes the bytes", lin::write(fds[1], b"ping"), 4);
    unsafe { EVENTS[0] = event(lin::EPOLLIN, 0x1234) };
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_MOD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"MOD setting the data word succeeds", r, 0);
    let n = lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 200);
    lin::check_eq(&mut failed, b"the wait reports the readable pipe", n, 1);
    lin::check_true(
        &mut failed,
        b"the report carries EPOLLIN",
        events_of(0) & lin::EPOLLIN != 0,
    );
    lin::check_eq(&mut failed, b"the report carries the data word", data_of(0) as isize, 0x1234);

    // Drained, and reported once already: a zero-timeout look sees nothing.
    static mut BUF: [u8; 4] = [0; 4];
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4) };
    lin::check_eq(&mut failed, b"the drain reads the 4 bytes", lin::read(fds[0], buf), 4);
    lin::check_eq(
        &mut failed,
        b"a drained pipe reports nothing to a zero-timeout wait",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 0),
        0,
    );

    // The one-shot: reported once, dark until a MOD re-arms it.
    unsafe { EVENTS[0] = event(lin::EPOLLIN | lin::EPOLLONESHOT, 7) };
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_MOD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"MOD arming the one-shot succeeds", r, 0);
    lin::check_eq(&mut failed, b"the pipe takes more bytes", lin::write(fds[1], b"pong"), 4);
    lin::check_eq(
        &mut failed,
        b"the one-shot reports its one event",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 200),
        1,
    );
    lin::check_eq(
        &mut failed,
        b"the spent one-shot reports nothing while it is ready",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 0),
        0,
    );
    unsafe { EVENTS[0] = event(lin::EPOLLIN | lin::EPOLLONESHOT, 7) };
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_MOD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"the MOD that re-arms succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"the re-armed one-shot reports again",
        lin::epoll_wait(epfd, unsafe { &raw mut EVENTS as *mut lin::EpollEvent }, 8, 200),
        1,
    );

    // Closing a watched descriptor ends its entry: the operations that
    // name it answer ENOENT, the shape Linux's close-time removal gives.
    lin::check_eq(&mut failed, b"closing the watched read end succeeds", lin::close(fds[0]), 0);
    let r = lin::epoll_ctl(epfd, lin::EPOLL_CTL_MOD, fds[0], unsafe { &raw mut EVENTS[0] as *mut lin::EpollEvent });
    lin::check_eq(&mut failed, b"MOD of the closed entry answers ENOENT", r, lin::ENOENT);

    let _ = lin::close(fds[1]);
    let _ = lin::close(epfd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-epoll: panicked\n")
}
