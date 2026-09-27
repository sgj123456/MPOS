//! t-memfd-shm - a memfd whose mapping is shared, not copied.
//!
//! The whole `wl_shm` story rests on one property: a client draws into its
//! mapping of a memfd, sends the descriptor to the compositor, and the
//! compositor maps the *same* descriptor and sees those pixels. That only
//! holds if a mapping of a memfd aliases the buffer's real pages rather than
//! copying them per-mapping. Pinned here three ways: two mappings of one fd
//! in one process show each other's writes; a `write(2)` shows through a
//! mapping and vice-versa; and - the compositor's actual case - a child
//! process that receives the fd over `SCM_RIGHTS` maps it and reads the byte
//! its parent drew before sending. Plus the seal round-trip.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const LEN: usize = 4096;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-memfd-shm\n");
    let mut failed = false;

    let fd = lin::memfd_create(b"wl_shm\0", lin::MFD_CLOEXEC | lin::MFD_ALLOW_SEALING) as i32;
    lin::check_true(&mut failed, b"memfd_create succeeds", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }
    lin::check_eq(
        &mut failed,
        b"ftruncate sizes the pool",
        lin::ftruncate(fd, LEN),
        0,
    );

    let prot = lin::PROT_READ | lin::PROT_WRITE;
    let a = lin::mmap(0, LEN, prot, lin::MAP_SHARED, fd, 0);
    let b = lin::mmap(0, LEN, prot, lin::MAP_SHARED, fd, 0);
    lin::check_true(&mut failed, b"the first mapping lands", a > 0);
    lin::check_true(&mut failed, b"the second mapping lands", b > 0);
    if a <= 0 || b <= 0 {
        lin::done(failed);
    }

    // Two mappings of one fd alias the same pages: a write through one is a
    // read through the other.
    let pa = a as *mut u8;
    let pb = b as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(b"PIXELS".as_ptr(), pa, 6);
    }
    let seen = unsafe { core::slice::from_raw_parts(pb, 6) == b"PIXELS" };
    lin::check_true(
        &mut failed,
        b"a write through one mapping shows through the other",
        seen,
    );

    // A write(2) reaches the mapping, and the mapping reaches a read(2).
    lin::check_eq(
        &mut failed,
        b"write(2) into the memfd succeeds",
        lin::write(fd, b"SYNC"),
        4,
    );
    let via_map = unsafe { core::slice::from_raw_parts(pa, 4) == b"SYNC" };
    lin::check_true(&mut failed, b"the mapping shows the write(2)", via_map);

    // The seal round-trip an MFD_ALLOW_SEALING pool needs.
    let r = lin::fcntl(fd, lin::F_ADD_SEALS as i32, lin::F_SEAL_SHRINK);
    lin::check_eq(&mut failed, b"F_ADD_SEALS succeeds", r, 0);
    let got = lin::fcntl(fd, lin::F_GET_SEALS as i32, 0);
    let seals = got as usize;
    lin::check_true(
        &mut failed,
        b"F_GET_SEALS reports the shrink seal",
        seals & lin::F_SEAL_SHRINK != 0,
    );

    // --- the compositor's case: hand the fd to another process ---
    let mut sp = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"socketpair for the handoff",
        lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut sp),
        0,
    );
    // Draw a marker through the mapping so the child can only see it if the
    // pages are truly shared.
    unsafe {
        core::ptr::copy_nonoverlapping(b"SHARED-MARKER".as_ptr(), pa.add(64), 13);
    }
    let pid = lin::fork();
    if pid == 0 {
        // Child: receive the descriptor, map it, confirm the marker is there.
        let mut cbuf = lin::ScmRights::new();
        let mut buf = [0u8; 1];
        let mut iov = [lin::IoVec {
            base: buf.as_mut_ptr() as usize,
            len: 1,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.control_ptr();
        msg.msg_controllen = lin::cmsg_space(4);
        let _ = lin::recvmsg(sp[1], &mut msg, 0);
        let mfd = cbuf.recv_fd();
        let ok = mfd >= 0 && {
            let m = lin::mmap(0, LEN, prot, lin::MAP_SHARED, mfd, 0);
            m > 0
                && unsafe {
                    core::slice::from_raw_parts((m as *const u8).add(64), 13) == b"SHARED-MARKER"
                }
        };
        lin::exit(if ok { 0 } else { 1 });
    }
    lin::check_true(&mut failed, b"the handoff child started", pid > 0);
    // Parent: send the descriptor over the socket.
    let mut cbuf = lin::ScmRights::new();
    let clen = unsafe { cbuf.one_fd(fd) };
    let mut data = [b's'];
    let mut iov = [lin::IoVec {
        base: data.as_mut_ptr() as usize,
        len: 1,
    }];
    let mut msg = lin::MsgHdr::default();
    msg.msg_iov = iov.as_mut_ptr() as usize;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.control_ptr();
    msg.msg_controllen = clen;
    let r = lin::sendmsg(sp[0], &mut msg, 0);
    lin::check_eq(&mut failed, b"SCM_RIGHTS send of the memfd", r, 1);

    let mut status = 0i32;
    lin::wait4(pid as usize, &mut status, 0);
    // WIFEXITED && WEXITSTATUS == 0 means the child saw the shared marker.
    let exited_clean = status & 0x7f == 0;
    let child_code = (status >> 8) & 0xff;
    lin::check_true(&mut failed, b"the child exited normally", exited_clean);
    lin::check_eq(
        &mut failed,
        b"the child read the parent's shared mapping",
        child_code as isize,
        0,
    );

    let _ = lin::munmap(a as usize, LEN);
    let _ = lin::munmap(b as usize, LEN);
    let _ = lin::close(fd);
    let _ = lin::close(sp[0]);
    let _ = lin::close(sp[1]);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-memfd-shm: panicked\n")
}
