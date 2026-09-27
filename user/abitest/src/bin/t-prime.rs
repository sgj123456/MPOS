//! t-prime - a handle exported as a dma-buf still names the same pages.
//!
//! The GBM software path rests on exactly this: the card makes a dumb
//! buffer, exports it as a dma-buf descriptor, and every later user - the
//! renderer mapping the descriptor, the compositor importing it back as a
//! handle - must land on the *same pages*. Pinned the way the wl_shm story
//! is: the exporter's mapping and the dma-buf's mapping show each other's
//! writes; `fstat` sizes the buffer; `DMA_BUF_IOCTL_SYNC` is accepted; an
//! export asking for `DRM_CLOEXEC` gets a close-on-exec descriptor; and -
//! the compositor's case - a child that receives the descriptor over
//! `SCM_RIGHTS` imports it as a handle, maps, and reads the bytes its parent
//! drew before the send. Importing a descriptor the card itself exported
//! answers the handle it came from.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `DRM_IOCTL_BASE` is `'d'` (0x64); a request is the `_IOC` word, and this
/// kernel dispatches on the `nr` byte (bits 8..15).
const fn ioc_d(nr: usize, size: usize) -> usize {
    (3 << 30) | (size << 16) | ((b'd' as usize) << 8) | nr
}
const NR_CREATE_DUMB: usize = 0xb2;
const NR_MAP_DUMB: usize = 0xb3;
const NR_PRIME_HANDLE_TO_FD: usize = 0x2d;
const NR_PRIME_FD_TO_HANDLE: usize = 0x2e;

/// `DRM_CLOEXEC == O_CLOEXEC`: mark the exported descriptor close-on-exec.
const DRM_CLOEXEC: u32 = 0x80000;
/// `_IOW(DMA_BUF_BASE, 0, struct dma_buf_sync)` - the CPU access bracket,
/// an 8-byte flags word in, nothing back but success.
const DMA_BUF_IOCTL_SYNC: usize = 0x4008_6200;

const O_RDWR: usize = 2;
const F_GETFD: i32 = 1;
const FD_CLOEXEC: usize = 1;
/// 64x64 @ 32bpp: pitch 256 (64-aligned), 16384 bytes - four pages.
const SIZE: usize = 16384;

/// Little-endian words out of a byte buffer at `at` (the UAPI structs are
/// native-order, and the host is little-endian).
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(a)
}

/// An aligned control slot carrying one descriptor in a sendmsg, the same
/// shape t-memfd-shm uses for the wl_shm handoff.
#[repr(C, align(8))]
struct Cbuf {
    w: [usize; 4],
}
impl Cbuf {
    const fn new() -> Cbuf {
        Cbuf { w: [0; 4] }
    }
    unsafe fn one_fd(&mut self, fd: i32) -> usize {
        unsafe {
            let b = self.w.as_mut_ptr() as *mut u8;
            core::ptr::write_bytes(b, 0, 32);
            (b as *mut usize).write_unaligned(lin::cmsg_len(4));
            (b.add(8) as *mut i32).write_unaligned(lin::SOL_SOCKET as i32);
            (b.add(12) as *mut i32).write_unaligned(lin::SCM_RIGHTS);
            (b.add(16) as *mut i32).write_unaligned(fd);
            lin::cmsg_space(4)
        }
    }
    fn recv_fd(&self) -> i32 {
        unsafe { *((self.w.as_ptr() as *const u8).add(16) as *const i32) }
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-prime\n");
    let mut failed = false;

    let card = lin::openat(lin::AT_FDCWD, b"/dev/dri/card0\0", O_RDWR, 0) as i32;
    lin::check_true(&mut failed, b"/dev/dri/card0 opens", card >= 0);
    if card < 0 {
        lin::done(failed);
    }

    // A 64x64 dumb buffer, mapped: the pages every later name must alias.
    let mut dumb = [0u8; 32];
    dumb[0..4].copy_from_slice(&64u32.to_ne_bytes()); // height
    dumb[4..8].copy_from_slice(&64u32.to_ne_bytes()); // width
    dumb[8..12].copy_from_slice(&32u32.to_ne_bytes()); // bpp
    let r = lin::ioctl(card, ioc_d(NR_CREATE_DUMB, 32), dumb.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"CREATE_DUMB succeeds", r, 0);
    let handle = u32_at(&dumb, 16);
    lin::check_true(&mut failed, b"a dumb handle is returned", handle >= 1);
    if r != 0 || handle == 0 {
        lin::done(failed);
    }

    let mut map = [0u8; 16];
    map[0..4].copy_from_slice(&handle.to_ne_bytes());
    let r = lin::ioctl(card, ioc_d(NR_MAP_DUMB, 16), map.as_mut_ptr() as usize);
    lin::check_eq(&mut failed, b"MAP_DUMB succeeds", r, 0);
    let offset = u64_at(&map, 8) as usize;
    let prot = lin::PROT_READ | lin::PROT_WRITE;
    let drm_map = lin::mmap(0, SIZE, prot, lin::MAP_SHARED, card, offset);
    lin::check_true(&mut failed, b"the dumb buffer maps", drm_map > 0);
    if drm_map <= 0 {
        lin::done(failed);
    }
    unsafe {
        core::ptr::copy_nonoverlapping(b"DRM-PRIME".as_ptr(), drm_map as *mut u8, 9);
    }

    // PRIME_HANDLE_TO_FD: handle@0, flags@4 (DRM_CLOEXEC), fd@8 out.
    let mut prime = [0u8; 12];
    prime[0..4].copy_from_slice(&handle.to_ne_bytes());
    prime[4..8].copy_from_slice(&DRM_CLOEXEC.to_ne_bytes());
    let r = lin::ioctl(
        card,
        ioc_d(NR_PRIME_HANDLE_TO_FD, 12),
        prime.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"PRIME_HANDLE_TO_FD succeeds", r, 0);
    let dma_fd = unsafe { (prime.as_ptr().add(8) as *const i32).read_unaligned() };
    lin::check_true(&mut failed, b"a dma-buf fd is returned", dma_fd >= 0);
    if r != 0 || dma_fd < 0 {
        lin::done(failed);
    }

    // DRM_CLOEXEC reaches the descriptor, as O_CLOEXEC would.
    let flags = lin::fcntl(dma_fd, F_GETFD, 0);
    lin::check_true(
        &mut failed,
        b"DRM_CLOEXEC marks the descriptor close-on-exec",
        flags as usize & FD_CLOEXEC != 0,
    );

    // fstat: a regular file the size of the buffer - how an importer learns
    // what there is to map.
    let mut st = lin::Stat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 0,
        st_mode: 0,
        st_uid: 0,
        st_gid: 0,
        _pad: 0,
        st_rdev: 0,
        st_size: 0,
        st_blksize: 0,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        _unused: [0; 3],
    };
    let r = lin::fstat(dma_fd, &mut st);
    lin::check_eq(&mut failed, b"fstat on the dma-buf succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"the dma-buf sizes to pitch*height",
        st.st_size as isize,
        SIZE as isize,
    );
    lin::check_true(
        &mut failed,
        b"the dma-buf is a regular file",
        st.st_mode & lin::S_IFMT == lin::S_IFREG,
    );

    // The dma-buf's mapping aliases the handle's pages: the parent's marker
    // reads through it, and what it writes reads back through the handle.
    let buf_map = lin::mmap(0, SIZE, prot, lin::MAP_SHARED, dma_fd, 0);
    lin::check_true(&mut failed, b"the dma-buf maps", buf_map > 0);
    if buf_map > 0 {
        let seen = unsafe { core::slice::from_raw_parts(buf_map as *const u8, 9) == b"DRM-PRIME" };
        lin::check_true(
            &mut failed,
            b"the exporter's pixels show through the dma-buf",
            seen,
        );
        unsafe {
            core::ptr::copy_nonoverlapping(
                b"VIA-DMABUF".as_ptr(),
                (buf_map as *mut u8).add(100),
                10,
            );
        }
        let back = unsafe {
            core::slice::from_raw_parts((drm_map as *const u8).add(100), 10) == b"VIA-DMABUF"
        };
        lin::check_true(
            &mut failed,
            b"the dma-buf's pixels show through the handle",
            back,
        );
    }

    // The access bracket a coherent buffer asks for: accepted. The request
    // carries a *pointer* to `struct dma_buf_sync` (an 8-byte flags word)
    // the way the UAPI macro builds it, not the flags by value.
    let sync = [5u64]; // DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ
    let r = lin::ioctl(dma_fd, DMA_BUF_IOCTL_SYNC, sync.as_ptr() as usize);
    lin::check_eq(&mut failed, b"DMA_BUF_IOCTL_SYNC is accepted", r, 0);

    // --- the compositor's case: hand the descriptor to another process ---
    let mut sp = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"socketpair for the handoff",
        lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut sp),
        0,
    );
    // A marker only the child's mapping can reach, to prove the pages it
    // imported are the pages the parent drew.
    unsafe {
        core::ptr::copy_nonoverlapping(b"SEND-MARKER".as_ptr(), (drm_map as *mut u8).add(200), 11);
    }
    let pid = lin::fork();
    if pid == 0 {
        // Child: receive the descriptor, import it, map it, confirm the
        // parent's pixels are there and leave a marker of its own.
        let mut cbuf = Cbuf::new();
        let mut byte = [0u8; 1];
        let mut iov = [lin::IoVec {
            base: byte.as_mut_ptr() as usize,
            len: 1,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.w.as_mut_ptr() as usize;
        msg.msg_controllen = lin::cmsg_space(4);
        let _ = lin::recvmsg(sp[1], &mut msg, 0);
        let mfd = cbuf.recv_fd();
        let mut ok = mfd >= 0;
        if ok {
            let mut imp = [0u8; 12];
            imp[8..12].copy_from_slice(&(mfd as i32).to_ne_bytes());
            let r = lin::ioctl(
                card,
                ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
                imp.as_mut_ptr() as usize,
            );
            ok = r == 0;
            if ok {
                let h = u32_at(&imp, 0);
                ok = h >= 1;
                if h >= 1 {
                    let mut m2 = [0u8; 16];
                    m2[0..4].copy_from_slice(&h.to_ne_bytes());
                    let _ = lin::ioctl(card, ioc_d(NR_MAP_DUMB, 16), m2.as_mut_ptr() as usize);
                    let off = u64_at(&m2, 8) as usize;
                    let m = lin::mmap(0, SIZE, prot, lin::MAP_SHARED, card, off);
                    ok = m > 0
                        && unsafe {
                            core::slice::from_raw_parts((m as *const u8).add(200), 11)
                                == b"SEND-MARKER"
                        };
                    if ok {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                b"CHILD-WROTE".as_ptr(),
                                (m as *mut u8).add(300),
                                11,
                            );
                        }
                    }
                }
            }
        }
        lin::exit(if ok { 0 } else { 1 });
    }
    lin::check_true(&mut failed, b"the handoff child started", pid > 0);
    // Parent: send the descriptor over the socket.
    let mut cbuf = Cbuf::new();
    let clen = unsafe { cbuf.one_fd(dma_fd) };
    let mut data = [b'p'];
    let mut iov = [lin::IoVec {
        base: data.as_mut_ptr() as usize,
        len: 1,
    }];
    let mut msg = lin::MsgHdr::default();
    msg.msg_iov = iov.as_mut_ptr() as usize;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.w.as_mut_ptr() as usize;
    msg.msg_controllen = clen;
    let r = lin::sendmsg(sp[0], &mut msg, 0);
    lin::check_eq(&mut failed, b"SCM_RIGHTS send of the dma-buf", r, 1);

    let mut status = 0i32;
    lin::wait4(pid as usize, &mut status, 0);
    lin::check_true(
        &mut failed,
        b"the child exited normally",
        status & 0x7f == 0,
    );
    lin::check_eq(
        &mut failed,
        b"the child read the parent's pixels through its import",
        ((status >> 8) & 0xff) as isize,
        0,
    );
    // The child wrote through its own import; the parent's mapping sees it.
    let seen = unsafe {
        core::slice::from_raw_parts((drm_map as *const u8).add(300), 11) == b"CHILD-WROTE"
    };
    lin::check_true(
        &mut failed,
        b"the parent reads the child's pixels through its handle",
        seen,
    );

    // A descriptor this card exported answers the handle it came from, so
    // the two names cannot drift apart into separate aliases.
    let mut imp = [0u8; 12];
    imp[8..12].copy_from_slice(&dma_fd.to_ne_bytes());
    let r = lin::ioctl(
        card,
        ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
        imp.as_mut_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"PRIME_FD_TO_HANDLE succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"importing this card's own export answers its handle",
        u32_at(&imp, 0) as isize,
        handle as isize,
    );

    let _ = lin::munmap(drm_map as usize, SIZE);
    let _ = lin::munmap(buf_map as usize, SIZE);
    let _ = lin::close(dma_fd);
    let _ = lin::close(sp[0]);
    let _ = lin::close(sp[1]);
    let _ = lin::close(card);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-prime: panicked\n")
}
