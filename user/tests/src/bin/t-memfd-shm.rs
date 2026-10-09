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
//!
//! **One host divergence.** `MFD_HUGETLB` is the only flag word the two
//! kernels answer differently: this kernel refuses it with `EOPNOTSUPP`
//! rather than answering with an ordinary memfd the caller believes is
//! huge-backed (it has no hugepages), while current Linux creates one
//! (measured: `probe_memfd.c` gets a descriptor). The claim names that
//! rather than pinning Linux's answer, which would be a claim about a
//! machine that has hugepages. Everything else - the closed flag vocabulary
//! (`0x1000` is `EINVAL` on both), the seal round-trip, both mappings of one
//! fd aliasing, `write(2)` reaching the mapping, the descriptor handover -
//! is the same answer on both (measured).

use mpos_user_tests::{Report, host_run};
use rustix::fs::{
    MemfdFlags, SealFlags, fcntl_add_seals, fcntl_get_seals, ftruncate, memfd_create,
};
use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, SendAncillaryBuffer, SendFlags, SocketType,
    socketpair,
};
use std::os::fd::AsFd;

const LEN: usize = 4096;
const MARKER: &[u8] = b"SHARED-MARKER";
const MARKER_AT: usize = 64;

/// `memfd_create` through the raw door, because two of the three claims here
/// are about *flag words*: `0x1000` has no name in any vocabulary, and
/// `MFD_HUGETLB` is being asked for the answer it gets rather than for a
/// pool. A typed flags argument cannot spell "a bit nobody named".
fn memfd_with(flags: u32) -> isize {
    let name = b"probe\0";
    let r = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr() as libc::c_long,
            flags as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn main() {
    println!("t-memfd-shm");
    let mut r = Report::new();

    let fd = memfd_create("wl_shm", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING);
    r.check("memfd_create succeeds", fd.is_ok());
    let fd = match fd {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };

    // memfd_create's flag vocabulary is closed: an unknown bit is EINVAL, and
    // `MFD_HUGETLB` is refused with `EOPNOTSUPP` rather than answered with an
    // ordinary memfd the caller believes is huge-backed. Both are
    // counterfactual - the old handler looked only at `MFD_CLOEXEC` and
    // created the fd for any flag at all.
    let unknown = memfd_with(0x1000);
    r.check(
        "memfd_create with an unknown flag (0x1000) is EINVAL",
        unknown == -(Errno::INVAL.raw_os_error() as isize),
    );
    const MFD_HUGETLB: u32 = 0o004;
    let huge = memfd_with(MFD_HUGETLB);
    r.check(
        "memfd_create with MFD_HUGETLB is EOPNOTSUPP, not a fake ordinary fd",
        huge == -(Errno::OPNOTSUPP.raw_os_error() as isize) || host_run() && huge >= 0,
    );
    // A recognised flag still creates the pool, so the gate is not blanket.
    let sealed_pool = memfd_create("m", MemfdFlags::ALLOW_SEALING);
    r.check(
        "memfd_create with MFD_ALLOW_SEALING alone still hands back a fd",
        sealed_pool.is_ok(),
    );
    r.check(
        "ftruncate sizes the pool",
        ftruncate(&fd, LEN as u64).is_ok(),
    );

    let prot = ProtFlags::READ | ProtFlags::WRITE;
    let a = unsafe { mmap(std::ptr::null_mut(), LEN, prot, MapFlags::SHARED, &fd, 0) };
    let b = unsafe { mmap(std::ptr::null_mut(), LEN, prot, MapFlags::SHARED, &fd, 0) };
    r.check("the first mapping lands", a.is_ok());
    r.check("the second mapping lands", b.is_ok());
    let (Ok(a), Ok(b)) = (a, b) else {
        r.finish();
    };

    // Two mappings of one fd alias the same pages: a write through one is a
    // read through the other.
    let pa = a as *mut u8;
    let pb = b as *mut u8;
    unsafe {
        std::ptr::copy_nonoverlapping(b"PIXELS".as_ptr(), pa, 6);
    }
    let seen = unsafe { std::slice::from_raw_parts(pb, 6) == b"PIXELS" };
    r.check("a write through one mapping shows through the other", seen);

    // A write(2) reaches the mapping, and the mapping reaches a read(2).
    let wrote = rustix::io::write(&fd, b"SYNC");
    r.check(
        "write(2) into the memfd succeeds",
        matches!(&wrote, Ok(n) if *n == 4),
    );
    let via_map = unsafe { std::slice::from_raw_parts(pa, 4) == b"SYNC" };
    r.check("the mapping shows the write(2)", via_map);

    // The seal round-trip an MFD_ALLOW_SEALING pool needs.
    r.check(
        "F_ADD_SEALS succeeds",
        fcntl_add_seals(&fd, SealFlags::SHRINK).is_ok(),
    );
    r.check(
        "F_GET_SEALS reports the shrink seal",
        matches!(fcntl_get_seals(&fd), Ok(seals) if seals.contains(SealFlags::SHRINK)),
    );

    // --- the compositor's case: hand the fd to another process ---
    let pair = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        rustix::net::SocketFlags::empty(),
        None,
    );
    r.check("socketpair for the handoff", pair.is_ok());
    let Ok((parent, child)) = pair else {
        r.finish();
    };

    // Draw a marker through the mapping so the child can only see it if the
    // pages are truly shared.
    unsafe {
        std::ptr::copy_nonoverlapping(MARKER.as_ptr(), pa.add(MARKER_AT), MARKER.len());
    }

    // Forked here rather than before the claims above, and the claim about the
    // fork comes *after* the child's branch: a check written between `fork`
    // and `if pid == 0` is printed by both processes, and the child's copy of
    // the report is not the one that is judged.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Child: receive the descriptor, map it, confirm the marker is there.
        // Its exit status is the only thing it says, because its copy of the
        // report is not the one that is judged.
        let mut cmsg = [std::mem::MaybeUninit::<u8>::uninit(); 64];
        let mut control = RecvAncillaryBuffer::new(&mut cmsg);
        let mut buf = [0u8; 1];
        let _ = rustix::net::recvmsg(
            &child,
            &mut [std::io::IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::empty(),
        );
        let mut mfd: Option<rustix::fd::OwnedFd> = None;
        for msg in control.drain() {
            if let rustix::net::RecvAncillaryMessage::ScmRights(fds) = msg {
                mfd = fds.into_iter().next();
            }
        }
        let ok = match mfd {
            Some(mfd) => {
                match unsafe { mmap(std::ptr::null_mut(), LEN, prot, MapFlags::SHARED, &mfd, 0) } {
                    Ok(map) => unsafe {
                        std::slice::from_raw_parts((map as *const u8).add(MARKER_AT), MARKER.len())
                            == MARKER
                    },
                    Err(_) => false,
                }
            }
            None => false,
        };
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }

    r.check("the handoff child started", pid > 0);

    // Parent: send the descriptor over the socket.
    let fds = [fd.as_fd()];
    let mut cmsg = [std::mem::MaybeUninit::<u8>::uninit(); 64];
    let mut control = SendAncillaryBuffer::new(&mut cmsg);
    let pushed = control.push(rustix::net::SendAncillaryMessage::ScmRights(&fds));
    r.check("the control message carries the memfd", pushed);
    let sent = rustix::net::sendmsg(
        &parent,
        &[std::io::IoSlice::new(b"s")],
        &mut control,
        SendFlags::empty(),
    );
    r.check(
        "SCM_RIGHTS send of the memfd",
        matches!(&sent, Ok(n) if *n == 1),
    );

    let mut status: libc::c_int = 0;
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    // WIFEXITED && WEXITSTATUS == 0 means the child saw the shared marker.
    r.check("the child exited normally", libc::WIFEXITED(status));
    r.check(
        "the child read the parent's shared mapping",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    unsafe {
        let _ = munmap(a, LEN);
        let _ = munmap(b, LEN);
    }
    r.finish();
}
