//! t-prime - a handle exported as a dma-buf still names the same pages.
//!
//! The GBM software path rests on exactly this: the card makes a dumb buffer,
//! exports it as a dma-buf descriptor, and every later user - the renderer
//! mapping the descriptor, the compositor importing it back as a handle - must
//! land on the *same pages*. Pinned the way the wl_shm story is: the exporter's
//! mapping and the dma-buf's mapping show each other's writes; `fstat` sizes the
//! buffer; `DMA_BUF_IOCTL_SYNC` is accepted; an export asking for `DRM_CLOEXEC`
//! gets a close-on-exec descriptor; and - the compositor's case - a child that
//! receives the descriptor over `SCM_RIGHTS` imports it as a handle, maps, and
//! reads the bytes its parent drew before the send. Importing a descriptor the
//! card itself exported answers the handle it came from.
//!
//! What the move to the std side changes is where the *structs* come from. The
//! no-std original spelled every request as a byte buffer with hand-counted
//! offsets (`handle@16` of the dumb create, `offset@8` of the map, `fd@8` of the
//! prime pair); here each record is spelled as `drm.h` lays it out with the
//! offsets as compile-time asserts, `t-drm-args`'s rule and for the same reason -
//! neither library binds any of them, so a drifting field would otherwise be a
//! silently misread word. `drm_prime_handle` is the one worth pointing at: the
//! same 12 bytes are the argument to *both* halves of the pair, with `handle` and
//! `fd` swapping which one is read and which is written, so a struct whose two
//! halves were spelled separately could disagree with itself.
//!
//! The other thing the move changes is the handoff. `fork` is one of the calls
//! `rustix` does not bind (`t-fork`'s note: `__NR_fork` has no arm64 number, and
//! musl's `fork` is `clone` there), so the child is `libc::fork` and everything
//! it does is raw `libc` - no allocation, no printing, and no `Report`, because
//! the child's copy of one would report against the parent's name and flush the
//! parent's own buffered lines a second time (`t-fork`'s reason). The child
//! answers with its exit code and the parent does the judging.
//!
//! Not made on a host, for `t-drm-args`'s reason: dumb buffers, the prime pair
//! and this dma-buf are this kernel's own objects, and a host's real card answers
//! its own. The one claim here a host could in principle share - that an
//! `SCM_RIGHTS` descriptor maps to the same pages - is pinned properly by
//! `t-unix-scm` and `t-mmap-shared-fork` against real Linux, so this file does
//! not restate it under a card it is not describing.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};

use mpos_user_tests::{Report, host_run};
use rustix::fd::FromRawFd;
use rustix::fs::OFlags;
use rustix::mm::{MapFlags, ProtFlags};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

/// `_IOWR('d', nr, size)`, as `t-drm-args` spells it and for the same reason.
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}
const NR_CREATE_DUMB: u32 = 0xb2;
const NR_MAP_DUMB: u32 = 0xb3;
const NR_PRIME_HANDLE_TO_FD: u32 = 0x2d;
const NR_PRIME_FD_TO_HANDLE: u32 = 0x2e;
const NR_GEM_CLOSE: u32 = 0x09;
const NR_RESOURCE_INFO: u32 = 0x45;

/// `DRM_CLOEXEC == O_CLOEXEC`: mark the exported descriptor close-on-exec.
const DRM_CLOEXEC: u32 = 0x80000;
/// `_IOW(DMA_BUF_BASE, 0, struct dma_buf_sync)` - the CPU access bracket, an
/// 8-byte flags word in, nothing back but success. `DMA_BUF_BASE` is `'b'`.
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
/// `DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ`.
const DMA_BUF_SYNC: u64 = 5;

/// 64x64 @ 32bpp: pitch 256 (64-aligned), 16384 bytes - four pages.
const SIZE: usize = 16384;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const BPP: u32 = 32;

// --- the records ------------------------------------------------------------

/// `struct drm_mode_create_dumb`: the geometry in, then the handle, the pitch and
/// the byte length out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_mode_map_dumb`: a handle in, the fake offset `mmap` wants out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MapDumb {
    handle: u32,
    _pad: u32,
    offset: u64,
}

/// `struct drm_prime_handle` - the argument to *both* halves of the prime pair.
/// Export reads `handle` and writes `fd`; import reads `fd` and writes `handle`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

const _: () = assert!(core::mem::size_of::<CreateDumb>() == 32);
const _: () = assert!(core::mem::offset_of!(CreateDumb, height) == 0);
const _: () = assert!(core::mem::offset_of!(CreateDumb, width) == 4);
const _: () = assert!(core::mem::offset_of!(CreateDumb, bpp) == 8);
const _: () = assert!(core::mem::offset_of!(CreateDumb, handle) == 16);
const _: () = assert!(core::mem::offset_of!(CreateDumb, pitch) == 20);
const _: () = assert!(core::mem::offset_of!(CreateDumb, size) == 24);
const _: () = assert!(core::mem::size_of::<MapDumb>() == 16);
const _: () = assert!(core::mem::offset_of!(MapDumb, handle) == 0);
const _: () = assert!(core::mem::offset_of!(MapDumb, offset) == 8);
const _: () = assert!(core::mem::size_of::<PrimeHandle>() == 12);
const _: () = assert!(core::mem::offset_of!(PrimeHandle, handle) == 0);
const _: () = assert!(core::mem::offset_of!(PrimeHandle, flags) == 4);
const _: () = assert!(core::mem::offset_of!(PrimeHandle, fd) == 8);

/// `struct drm_gem_close`: the handle a file is giving back.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GemClose {
    handle: u32,
    _pad: u32,
}

/// `struct drm_virtgpu_resource_info`: a handle in, the device resource and the
/// size it names out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ResourceInfo {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}

const _: () = assert!(core::mem::size_of::<GemClose>() == 8);
const _: () = assert!(core::mem::offset_of!(GemClose, handle) == 0);
const _: () = assert!(core::mem::size_of::<ResourceInfo>() == 16);
const _: () = assert!(core::mem::offset_of!(ResourceInfo, res_handle) == 4);
const _: () = assert!(core::mem::offset_of!(ResourceInfo, size) == 8);

// --- the control-message door -----------------------------------------------
//
// `t-unix-scm`'s shape: the raw `libc::sendmsg`/`libc::recvmsg` pair, because
// `rustix` does not carry a descriptor through a message. The area is
// 8-byte-aligned because that is what the kernel's `CMSG_ALIGN` walks on.

const CMSG_HDRLEN: usize = 16;

const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}
const fn cmsg_len(n: usize) -> usize {
    CMSG_HDRLEN + n
}
const fn cmsg_space(n: usize) -> usize {
    cmsg_align(CMSG_HDRLEN + n)
}

/// An 8-byte-aligned control area: a `cmsghdr` then one descriptor int.
#[repr(C, align(8))]
struct Ctrl {
    w: [usize; 4],
}

impl Ctrl {
    const fn new() -> Ctrl {
        Ctrl { w: [0; 4] }
    }

    fn ptr(&mut self) -> *mut u8 {
        self.w.as_mut_ptr().cast::<u8>()
    }

    /// Lay down one `SCM_RIGHTS` chain carrying `fd`, and answer the
    /// `msg_controllen` that covers it.
    fn one_fd(&mut self, fd: i32) -> usize {
        let b = self.ptr();
        // SAFETY: `w` is 32 bytes and every write below is inside it.
        unsafe {
            core::ptr::write_bytes(b, 0, 32);
            b.cast::<usize>().write_unaligned(cmsg_len(4));
            b.add(8).cast::<i32>().write_unaligned(libc::SOL_SOCKET);
            b.add(12).cast::<i32>().write_unaligned(libc::SCM_RIGHTS);
            b.add(16).cast::<i32>().write_unaligned(fd);
        }
        cmsg_space(4)
    }

    /// The descriptor the kernel handed over - the chain's first data word.
    fn first_int(&self) -> i32 {
        // SAFETY: the data of a `cmsghdr` starts at `CMSG_HDRLEN`, and the
        // kernel wrote one there (or the caller is about to be told it failed).
        unsafe {
            (self.w.as_ptr().cast::<u8>().add(CMSG_HDRLEN))
                .cast::<i32>()
                .read_unaligned()
        }
    }
}

/// One `ioctl`, answering the negative errno a raw syscall hands back: through
/// `libc::syscall` because the request is an `unsigned long` word and `ioctl`'s
/// binding of it is not the same width on both libcs (`t-drm-args`'s note).
///
/// # Safety
/// `arg` must be an address the caller owns, at least as long as the size the
/// request word names.
unsafe fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd as libc::c_long, request, arg) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// One claim that a request succeeded.
fn check_ok(r: &mut Report, name: &str, got: isize) {
    if got >= 0 {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got})"), false);
    }
}

/// A fixture the rest of the program is built on: if it did not hold there is
/// nothing to test, so the program says which one and stops.
fn take<T>(r: &mut Report, name: &str, got: Result<T, rustix::io::Errno>) -> T {
    match got {
        Ok(value) => {
            r.check(name, true);
            value
        }
        Err(err) => {
            r.check(&format!("{name} (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    }
}

/// Write `bytes` at `off` inside a mapping, and answer whether they read back.
///
/// # Safety
/// `base` must be a mapping at least `off + bytes.len()` long.
unsafe fn stamp(base: *mut u8, off: usize, bytes: &[u8]) {
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(off), bytes.len());
    }
}

/// Whether `bytes` is what sits at `off` inside a mapping.
///
/// # Safety
/// `base` must be a mapping at least `off + bytes.len()` long.
unsafe fn reads(base: *mut u8, off: usize, bytes: &[u8]) -> bool {
    unsafe { core::slice::from_raw_parts(base.add(off), bytes.len()) == bytes }
}

fn main() {
    println!("t-prime");
    let mut r = Report::new();

    // A hosted run measures a different card's objects, so it names that and
    // stops - `t-drm-args`'s reason, and `t-pty`'s rule for a fixture it cannot
    // build.
    if host_run() {
        println!(
            "note: not made on a host - dumb buffers, the prime pair and this dma-buf are \
             this kernel's objects, and a host's own /dev/dri/card0 answers its own"
        );
        r.finish();
    }

    let card = match rustix::fs::open("/dev/dri/card0", OFlags::RDWR, rustix::fs::Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("/dev/dri/card0 opens ({e:?})"), false);
            r.finish();
        }
    };
    let card_fd = card.as_raw_fd();
    r.check("/dev/dri/card0 opens", true);

    // --- a dumb buffer, mapped: the pages every later name must alias ---
    let mut dumb = CreateDumb {
        height: HEIGHT,
        width: WIDTH,
        bpp: BPP,
        ..CreateDumb::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_CREATE_DUMB, 32),
            (&raw mut dumb) as *mut c_void,
        )
    };
    check_ok(&mut r, "CREATE_DUMB succeeds", got);
    let handle = dumb.handle;
    r.check(
        &format!("a dumb handle is returned (got {handle})"),
        handle >= 1,
    );
    if handle == 0 {
        r.finish();
    }
    r.check(
        &format!("and the buffer is the size it said (got {})", dumb.size),
        dumb.size == SIZE as u64,
    );

    let mut map = MapDumb {
        handle,
        ..MapDumb::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_MAP_DUMB, 16),
            (&raw mut map) as *mut c_void,
        )
    };
    check_ok(&mut r, "MAP_DUMB succeeds", got);
    let drm_map = take(&mut r, "the dumb buffer maps", unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            SIZE,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &card,
            map.offset,
        )
    });
    unsafe { stamp(drm_map.cast::<u8>(), 0, b"DRM-PRIME") };

    // --- PRIME_HANDLE_TO_FD: handle in, dma-buf descriptor out ---
    let mut prime = PrimeHandle {
        handle,
        flags: DRM_CLOEXEC,
        ..PrimeHandle::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_PRIME_HANDLE_TO_FD, 12),
            (&raw mut prime) as *mut c_void,
        )
    };
    check_ok(&mut r, "PRIME_HANDLE_TO_FD succeeds", got);
    let dma_fd = prime.fd;
    r.check(
        &format!("a dma-buf fd is returned (got {dma_fd})"),
        dma_fd >= 0,
    );
    if dma_fd < 0 {
        r.finish();
    }
    // Owned from here on, so an early exit closes it rather than leaking it into
    // the next test in the sweep.
    let dma = unsafe { OwnedFd::from_raw_fd(dma_fd) };

    // DRM_CLOEXEC reaches the descriptor, as O_CLOEXEC would.
    match rustix::io::fcntl_getfd(&dma) {
        Ok(flags) => r.check(
            "DRM_CLOEXEC marks the descriptor close-on-exec",
            flags.contains(rustix::io::FdFlags::CLOEXEC),
        ),
        Err(e) => r.check(&format!("F_GETFD on the dma-buf answers ({e:?})"), false),
    }

    // fstat: a regular file the size of the buffer - how an importer learns what
    // there is to map.
    match rustix::fs::fstat(&dma) {
        Ok(st) => {
            r.check(
                &format!("the dma-buf sizes to pitch*height (got {})", st.st_size),
                st.st_size == SIZE as i64,
            );
            r.check(
                "the dma-buf is a regular file",
                rustix::fs::FileType::from_raw_mode(st.st_mode)
                    == rustix::fs::FileType::RegularFile,
            );
        }
        Err(e) => r.check(&format!("fstat on the dma-buf succeeds ({e:?})",), false),
    }

    // The dma-buf's mapping aliases the handle's pages, both directions.
    let buf_map = take(&mut r, "the dma-buf maps", unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            SIZE,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &dma,
            0,
        )
    });
    r.check("the exporter's pixels show through the dma-buf", unsafe {
        reads(buf_map.cast::<u8>(), 0, b"DRM-PRIME")
    });
    unsafe { stamp(buf_map.cast::<u8>(), 100, b"VIA-DMABUF") };
    r.check("the dma-buf's pixels show through the handle", unsafe {
        reads(drm_map.cast::<u8>(), 100, b"VIA-DMABUF")
    });

    // The access bracket a coherent buffer asks for: accepted. The request
    // carries a *pointer* to `struct dma_buf_sync` the way the UAPI macro builds
    // it, not the flags by value.
    let mut sync: u64 = DMA_BUF_SYNC;
    let got = unsafe {
        ioctl(
            dma.as_raw_fd(),
            DMA_BUF_IOCTL_SYNC,
            (&raw mut sync) as *mut c_void,
        )
    };
    check_ok(&mut r, "DMA_BUF_IOCTL_SYNC is accepted", got);

    // --- the compositor's case: hand the descriptor to another process ---
    let (sp0, sp1) = take(
        &mut r,
        "socketpair for the handoff",
        socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        ),
    );
    // A marker only the child's mapping can reach, to prove the pages it
    // imported are the pages the parent drew.
    unsafe { stamp(drm_map.cast::<u8>(), 200, b"SEND-MARKER") };

    // SAFETY: `fork` is the only way to get a second process whose descriptors
    // are these ones, and the child below touches nothing but raw calls.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check("fork succeeds", false);
        r.finish();
    }
    if pid == 0 {
        child(sp1.as_raw_fd(), card_fd);
    }
    r.check("the handoff child started", pid > 0);

    // Parent: send the descriptor over the socket.
    let mut cbuf = Ctrl::new();
    let clen = cbuf.one_fd(dma.as_raw_fd());
    let sent = send_fd(sp0.as_raw_fd(), &mut cbuf, clen);
    r.check(
        &format!("SCM_RIGHTS send of the dma-buf (got {sent})"),
        sent == 1,
    );

    let reaped = rustix::process::waitpid(
        rustix::process::Pid::from_raw(pid),
        rustix::process::WaitOptions::empty(),
    );
    match reaped {
        Ok(Some((_, status))) => {
            r.check("the child exited normally", status.exited());
            r.check(
                &format!(
                    "the child read the parent's pixels through its import (code {:?})",
                    status.exit_status()
                ),
                status.exit_status() == Some(0),
            );
        }
        other => {
            r.check(&format!("the child was reaped ({other:?})"), false);
            r.check(
                "the child read the parent's pixels through its import",
                false,
            );
        }
    }
    // The child wrote through its own import; the parent's mapping sees it.
    r.check(
        "the parent reads the child's pixels through its handle",
        unsafe { reads(drm_map.cast::<u8>(), 300, b"CHILD-WROTE") },
    );

    // A descriptor this card exported answers the handle it came from, so the
    // two names cannot drift apart into separate aliases.
    let mut imp = PrimeHandle {
        fd: dma.as_raw_fd(),
        ..PrimeHandle::default()
    };
    let got = unsafe {
        ioctl(
            card_fd,
            ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
            (&raw mut imp) as *mut c_void,
        )
    };
    check_ok(&mut r, "PRIME_FD_TO_HANDLE succeeds", got);
    r.check(
        &format!(
            "importing this card's own export answers its handle (got {}, want {handle})",
            imp.handle
        ),
        imp.handle == handle,
    );

    // A handle is a **name the file holds**, not a property of the buffer.
    // Linux's handle table is per `drm_file`, so a *second* file that imported
    // this descriptor and then gave back the name it was handed says nothing
    // about this file's name for the same buffer. Measured as the compositor
    // coming up black: the render node closed the handle it had imported, and
    // every batch the compositor's own file submitted naming its own handle came
    // back `ENOENT` - which Mesa prints as `got error from kernel - expect bad
    // rendering 2` and answers by dropping all rendering. This card kept one
    // global flag, so one file's release took the other file's name away with
    // it, and the claim below is the one that holds that flag to account.
    match rustix::fs::open(
        "/dev/dri/renderD128",
        OFlags::RDWR,
        rustix::fs::Mode::empty(),
    ) {
        Ok(second) => {
            let sfd = second.as_raw_fd();
            let mut imp2 = PrimeHandle {
                fd: dma.as_raw_fd(),
                ..PrimeHandle::default()
            };
            let got = unsafe {
                ioctl(
                    sfd,
                    ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
                    (&raw mut imp2) as *mut c_void,
                )
            };
            check_ok(&mut r, "a second file imports the same descriptor", got);
            let mut close2 = GemClose {
                handle: imp2.handle,
                ..GemClose::default()
            };
            let got = unsafe {
                ioctl(
                    sfd,
                    ioc_d(NR_GEM_CLOSE, 8),
                    (&raw mut close2) as *mut c_void,
                )
            };
            check_ok(&mut r, "and gives back the name it was given", got);
            // The claim. This file's name is this file's: a card that keyed the
            // name on the *buffer* answered `ENOENT` here, which is the shape
            // that stopped the compositor drawing.
            let mut info = ResourceInfo {
                bo_handle: handle,
                ..ResourceInfo::default()
            };
            let got = unsafe {
                ioctl(
                    card_fd,
                    ioc_d(NR_RESOURCE_INFO, 16),
                    (&raw mut info) as *mut c_void,
                )
            };
            r.check(
                &format!(
                    "while this file's own name still stands (got {got}, res {}, want {handle})",
                    info.res_handle
                ),
                got == 0 && info.res_handle == handle,
            );
        }
        Err(e) => r.check(&format!("/dev/dri/renderD128 opens ({e:?})"), false),
    }

    let _ = unsafe { rustix::mm::munmap(drm_map, SIZE) };
    let _ = unsafe { rustix::mm::munmap(buf_map, SIZE) };
    r.finish();
}

/// The child: receive the descriptor, import it, map it, confirm the parent's
/// pixels are there and leave a marker of its own. Never returns.
///
/// Every call in here is raw `libc` and none of them allocate, because this is
/// the far side of a `fork` in a program with a live allocator and a buffered
/// stdout: the child reports with its exit code and prints nothing, `t-fork`'s
/// reason exactly.
fn child(sock: i32, card: i32) -> ! {
    // SAFETY: the descriptors are the parent's, inherited across `fork`, and
    // every buffer below outlives the call it is given to.
    let code = unsafe {
        let mut ctrl = Ctrl::new();
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.ptr().cast::<libc::c_void>();
        msg.msg_controllen = cmsg_space(4) as _;
        let got = libc::recvmsg(sock, &mut msg, 0);
        let mut ok = got == 1;
        let mfd = ctrl.first_int();
        ok = ok && mfd >= 0;

        if ok {
            // Import the descriptor as a handle on the same card.
            let mut imp = PrimeHandle {
                fd: mfd,
                ..PrimeHandle::default()
            };
            ok = libc::syscall(
                libc::SYS_ioctl,
                card as libc::c_long,
                ioc_d(NR_PRIME_FD_TO_HANDLE, 12),
                &raw mut imp,
            ) == 0;
            ok = ok && imp.handle >= 1;

            if ok {
                let mut m2 = MapDumb {
                    handle: imp.handle,
                    ..MapDumb::default()
                };
                let _ = libc::syscall(
                    libc::SYS_ioctl,
                    card as libc::c_long,
                    ioc_d(NR_MAP_DUMB, 16),
                    &raw mut m2,
                );
                let addr = libc::mmap(
                    core::ptr::null_mut(),
                    SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    card,
                    m2.offset as libc::off_t,
                );
                ok = addr != libc::MAP_FAILED;
                if ok {
                    let base = addr.cast::<u8>();
                    ok = reads(base, 200, b"SEND-MARKER");
                    if ok {
                        stamp(base, 300, b"CHILD-WROTE");
                    }
                }
            }
        }
        if ok { 0 } else { 1 }
    };
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code);
    }
}

/// `sendmsg(fd, &one byte, &[control])`, answering the byte count.
fn send_fd(fd: i32, ctrl: &mut Ctrl, clen: usize) -> isize {
    let data = *b"p";
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.ptr().cast::<libc::c_void>();
    msg.msg_controllen = clen as _;
    // SAFETY: the iovec and the control area both outlive the call.
    let rc = unsafe { libc::sendmsg(fd, &msg, 0) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}
