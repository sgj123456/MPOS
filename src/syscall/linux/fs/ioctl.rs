//! The device-`ioctl` entry point: `handle_ioctl` and its per-family routers.
//!
//! Split out of the old `fs.rs` (PLAN.md §11.2 Step E). This is the one job the
//! VFS-traversing half of `fs` did not share: a `request` word decoded by its
//! `_IOC_TYPE` magic (`family_nr`) and dispatched to the console/DRM/evdev/uinput/
//! fb/rtc/netlink/ioctl families. The path-carrying handlers live in `super::file`;
//! the single helper both need (`sync_nonblocking`) is defined in `super::mod` and
//! imported below.

use super::sync_nonblocking;

use crate::syscall::linux::abi;
use crate::syscall::{ARG_COUNT, buffer_ok, err, err_of};
// The concrete node types these handlers drive: the devices, and the socket
// families whose syscalls live here. Naming one is the *syscall* layer's job -
// it is the layer that knows which request belongs to which device - and it is
// exactly what the VFS stopped doing: `Inode` carries no method per device type
// any more, so a caller asks the node for its concrete type through
// `dyn Inode::as_any` instead (`PLAN.md` §11.2 Step B1, V-1). Gated with the
// modules they come from, which the host build does not compile.
#[cfg(all(target_os = "none", not(target_arch = "aarch64")))]
use crate::drivers::uinput::Uinput;
#[cfg(all(target_os = "none", not(target_arch = "aarch64")))]
use crate::drivers::virtio::gpu::FbDev;
#[cfg(all(target_os = "none", not(target_arch = "aarch64")))]
use crate::drivers::virtio::input::EvdevNode;

/// `ioctl(fd, request, arg)` — terminal settings and queries, stub the rest.
///
/// `TCGETS` and `TCSETS` are the ones that matter: a shell asks what the
/// terminal is, switches off line editing and echo so it can do its own, and
/// puts them back when it is finished. Answering `TCGETS` with sixty zeroes
/// while carrying on editing and echoing regardless is what makes a shell
/// show every line twice and read every arrow key as punctuation - so the
/// settings are the tty's, and the tty obeys them.
pub fn handle_ioctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, request) = (args[0] as i32, args[1] as u32);
    let buf = args[2];
    match request {
        0x5421 => {
            // FIONBIO: set or clear `O_NONBLOCK` - `fcntl(F_SETFL)`'s bit in
            // an ioctl's clothes, and the one request here that is about any
            // descriptor rather than a terminal. It is not optional: Rust's
            // std puts the pipes it is about to `poll` into non-blocking mode
            // exactly this way, so answering ENOTTY makes every
            // `Command::output()` panic with "Not a tty" - which is how
            // `rustup-init` dies on its first command. The other status bits
            // are left alone, because this request carries only the one.
            if !buffer_ok(buf, 4, false, from_user) {
                return err::EFAULT;
            }
            let on = unsafe { (buf as *const i32).read_unaligned() } != 0;
            match crate::sched::current_fds().clone_at(fd) {
                Ok(file) => {
                    let mut status = file.status();
                    if on {
                        status |= crate::vfs::file::O_NONBLOCK;
                    } else {
                        status &= !crate::vfs::file::O_NONBLOCK;
                    }
                    file.set_status(status);
                    sync_nonblocking(&file, status);
                    0
                }
                Err(e) => err_of(e),
            }
        }
        0x5413 => {
            // TIOCGWINSZ: the shape of the terminal the descriptor names.
            // `isatty` is a call to this on musl, so it has to work on a tty and
            // fail everywhere else - and the "everywhere else" is load-bearing:
            // a program that finds its stdin a terminal when it is a pipe draws
            // the wrong conclusion about where its input came from. fish's
            // `source`, piped a script on stdin, asked exactly that question and
            // gave up.
            //
            // The size is the terminal's own, not a constant: a virtual console
            // is painted by the framebuffer console, so its window is the grid
            // that console draws - and answering 80x25 there is what drew a
            // `vim` into the corner of a screen instead of over it (80 of its
            // 128 columns and 25 of its 48 rows on a 1024x768 display). See
            // `Tty::window_size`.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            // The inode is bound first: `as_tty` borrows from it, and a
            // temporary `Arc` borrowed through would be gone before the
            // answer it lent was read.
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            let (rows, cols) = tty.window_size();
            if buffer_ok(buf, 8, true, from_user) {
                unsafe {
                    (buf as *mut u16).write_unaligned(rows);
                    (buf as *mut u16).add(1).write_unaligned(cols);
                    (buf as *mut u16).add(2).write_unaligned(0);
                    (buf as *mut u16).add(3).write_unaligned(0);
                }
                0
            } else {
                err::EFAULT
            }
        }
        0x5414 => {
            // TIOCSWINSZ: the other half of the pair - a terminal emulator
            // naming the window its programs should draw in.
            //
            // The size lands on the *terminal*, where both ends of a pair read
            // the same one: an emulator sets it on the master, the program
            // inside reads it on the slave, and that is the whole of how a pty
            // knows how big it is. A *change* is also the one thing a
            // full-screen program cannot notice by itself, so the foreground
            // group is told - `SIGWINCH`, by the same route a `Ctrl-C` takes.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            // The inode is bound first, as above: `as_tty` borrows from it.
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, 8, false, from_user) {
                return err::EFAULT;
            }
            let (rows, cols) = unsafe {
                (
                    (buf as *const u16).read_unaligned(),
                    (buf as *const u16).add(1).read_unaligned(),
                )
            };
            if tty.set_window_size(rows, cols) {
                // A virtual console stores it and keeps answering its screen
                // (see `Tty::set_window_size`), so this is a redraw of the same
                // size there - harmless, and it is what the ABI promises.
                tty.signal_window_change();
            }
            0
        }
        0x540E => {
            // TIOCSCTTY: make the descriptor's tty the controlling terminal.
            // The call binds the terminal to the caller - `/dev/tty` is that
            // terminal's name for exactly this process from here on - and,
            // the Linux `__proc_set_tty` behaviour, aims the terminal's
            // foreground at the caller's process group: the getty that just
            // became a session leader owns the line before anyone reads or
            // writes it, which is the state a login shell's job-control probe
            // expects to find when it asks `tcgetpgrp`.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            if inode.as_tty().is_none() {
                return err::ENOTTY;
            }
            // The device id is what the binding and every later `/dev/tty`
            // open resolve through; a tty inode always carries one.
            match inode.rdev() {
                Some(dev_id) => crate::sched::acquire_controlling_tty(dev_id),
                None => return err::ENOTTY,
            }
            0
        }
        0x540F | 0x5410 => {
            // TIOCGPGRP / TIOCSPGRP: the terminal's foreground process group.
            //
            // Job control is built on exactly this pair. A shell asks who owns
            // the terminal, claims it for each job it starts, and takes it back
            // when the job finishes - and a kernel that cannot answer makes
            // every shell print `can't access tty; job control turned off` and
            // then run as though it were talking to a file.
            //
            // TIOCSPGRP is 0x5410 on x86_64 - 0x541E is TIOCSETD, and routing
            // the pair there was why every `tcsetpgrp` read back ENOTTY.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };

            if request == 0x540F {
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                // Zero is what a terminal with no foreground job reports, and
                // it is a valid answer rather than an error: a shell is
                // allowed to find out that nobody owns the terminal yet.
                let pgid = tty.foreground().unwrap_or(crate::sched::TaskId(0));
                unsafe { (buf as *mut i32).write_unaligned(pgid.0 as i32) };
                return 0;
            }

            if !buffer_ok(buf, 4, false, from_user) {
                return err::EFAULT;
            }
            let pgid = unsafe { (buf as *const i32).read_unaligned() };
            if pgid <= 0 || pgid as usize >= crate::sched::MAX_TASKS {
                return err::EINVAL;
            }
            tty.set_foreground(Some(crate::sched::TaskId(pgid as usize)));
            // POSIX's `tcsetpgrp` is not only a record: it sends `SIGCONT` to
            // the group it names. That is what resumes a job that stopped
            // itself waiting to be made foreground - a shell's `fg`, and a
            // supervisor's handshake - and without it the wait is one-way: the
            // group is named the foreground and the process that was going to
            // use that answer is still stopped, for the machine's life.
            crate::sched::send_signal_group(
                crate::sched::TaskId(pgid as usize),
                crate::sched::signal::SIGCONT,
            );
            0
        }
        // TIOCGSID: the session the terminal belongs to - `tcgetsid`, which a
        // job-control shell asks before it decides whether this terminal is
        // *its* terminal to claim. Only the session leader that owns the line
        // may take it, and a shell that cannot find out has to assume it may
        // not: `su`'s shell then leaves the terminal to the process that
        // acquired it and stops itself waiting to be made foreground. A
        // terminal no session holds has no answer, which is ENOTTY.
        0x5429 => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            if inode.as_tty().is_none() {
                return err::ENOTTY;
            }
            // A pair is one terminal, so the master asks about the slave's
            // session - the id its binding was recorded under.
            let dev_id = inode
                .as_pty()
                .map_or_else(|| inode.rdev(), |pty| Some(pty.slave_dev_id()));
            let Some(sid) = dev_id.and_then(crate::sched::session_of_tty) else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, 4, true, from_user) {
                return err::EFAULT;
            }
            unsafe { (buf as *mut i32).write_unaligned(sid as i32) };
            0
        }
        0x80045430 | 0x40045431 => {
            // TIOCGPTN / TIOCSPTLCK, the pty master's pair. `TIOCGPTN` names
            // the number the slave will appear under in `/dev/pts` - the
            // middle step of the `posix_openpt` / `grantpt` / `unlockpt` /
            // `open` sequence, which has no other way to learn it.
            // `TIOCSPTLCK` is `grantpt` and `unlockpt` themselves: a zero
            // unlocks the pair so a slave may be opened, and a locked pair
            // refuses its slave with `EIO`.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(pty) = inode.as_pty() else {
                return err::ENOTTY;
            };
            if request == 0x80045430 {
                if !buffer_ok(buf, 4, true, from_user) {
                    return err::EFAULT;
                }
                unsafe { (buf as *mut u32).write_unaligned(pty.number()) };
                return 0;
            }
            if !buffer_ok(buf, 4, false, from_user) {
                return err::EFAULT;
            }
            let lock = unsafe { (buf as *const i32).read_unaligned() };
            pty.set_locked(lock != 0);
            0
        }
        0x5401 => {
            // TCGETS
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, crate::vfs::termios::SIZE, true, from_user) {
                return err::EFAULT;
            }
            unsafe { tty.termios().store_to(buf as *mut u8) };
            0
        }
        // TCSETS, TCSETSW, TCSETSF: the difference between them is when they
        // take effect, which is about draining an output queue this tty does
        // not have - a byte written is already gone.
        0x5402..=0x5404 => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(tty) = inode.as_tty() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, crate::vfs::termios::SIZE, false, from_user) {
                return err::EFAULT;
            }
            let tio = unsafe { crate::vfs::termios::Termios::from_ptr(buf as *const u8) };
            tty.set_termios(tio);
            0
        }
        // The block-device facts libblkid asks for before it probes a device:
        // how big it is, in bytes and in sectors, and what one sector is.
        // Without them a probe reads the device blind, sizing it by EIO.
        0x80081272 | 0x1260 | 0x1268 => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let Some(dev) = file.inode().as_block() else {
                return err::ENOTTY;
            };
            let sectors = dev.capacity_sectors();
            match request {
                // BLKGETSIZE64: u64, the size in bytes.
                //
                // All three of these write into the caller's buffer, so all
                // three validate it as a destination. They used to validate it
                // `write = false`, which accepted a read-only page and filled
                // it: measured on a real device, a `PROT_READ` mapping is
                // `EFAULT` for every one of them, because Linux's `put_user`
                // faults it. No real caller (libblkid, `blockdev`, which pass a
                // writable variable) ever saw the difference, which is why it
                // went unnoticed; the direction is the contract regardless.
                0x80081272 => {
                    if !buffer_ok(buf, 8, true, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u64).write_unaligned(sectors * 512) };
                }
                // BLKGETSIZE: u32, the size in 512-byte sectors.
                0x1260 => {
                    if !buffer_ok(buf, 4, true, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u32).write_unaligned(sectors as u32) };
                }
                // BLKSSZGET: u32, the sector size itself.
                _ => {
                    if !buffer_ok(buf, 4, true, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut u32).write_unaligned(512) };
                }
            }
            0
        }
        0x8913 => {
            // SIOCGIFFLAGS: the administrative flags of the named link, as
            // busybox's `ip link set` reads them before writing new ones.
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf).and_then(ifreq_flags) {
                Some(flags) => {
                    unsafe {
                        ((buf + 16) as *mut u16).write_unaligned(flags);
                    }
                    0
                }
                None => err::ENODEV,
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x8914 => {
            // SIOCSIFFLAGS: the new flags. Only `IFF_UP` carries meaning -
            // the capability bits are the link's to keep, not the setter's.
            if !buffer_ok(buf, 40, false, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf) {
                Some(index) => {
                    let flags = unsafe { ((buf + 16) as *const u16).read_unaligned() };
                    match crate::net::set_link_up(index, flags & 1 != 0) {
                        Ok(()) => 0,
                        Err(e) => e,
                    }
                }
                None => err::ENODEV,
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x8921 => {
            // SIOCGIFMTU: the link's current MTU - the loopback's is the
            // 64 KiB an IPv4 datagram tops out at, the NIC's is what
            // `RTM_SETLINK` last set.
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf) {
                Some(1) => {
                    unsafe {
                        ((buf + 16) as *mut i32).write_unaligned(65536);
                    }
                    0
                }
                Some(_) => match crate::net::link_facts() {
                    Some(facts) => {
                        unsafe {
                            ((buf + 16) as *mut i32).write_unaligned(facts.mtu as i32);
                        }
                        0
                    }
                    None => err::ENODEV,
                },
                None => err::ENODEV,
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x8922 => {
            // SIOCSIFMTU: the new MTU, which `ip link set eth0 mtu N`
            // writes. The stack clamps it to the bounds a frame can carry.
            if !buffer_ok(buf, 40, false, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf) {
                Some(2) => {
                    let mtu = unsafe { ((buf + 16) as *const i32).read_unaligned() };
                    match crate::net::set_mtu(mtu as u32) {
                        Ok(()) => 0,
                        Err(e) => e,
                    }
                }
                Some(_) => err::EOPNOTSUPP,
                None => err::ENODEV,
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x8923 => {
            // SIOCSIFNAME: rename. The new name rides the union half of the
            // request - the same place the flags and the MTU do.
            if !buffer_ok(buf, 40, false, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf) {
                Some(2) => {
                    let mut name = [0u8; 16];
                    unsafe {
                        for (i, slot) in name.iter_mut().enumerate() {
                            *slot = ((buf + 16 + i) as *const u8).read_unaligned();
                            if *slot == 0 {
                                break;
                            }
                        }
                    }
                    let len = name.iter().position(|b| *b == 0).unwrap_or(16);
                    match core::str::from_utf8(&name[..len]) {
                        Ok(name) => match crate::net::set_eth_name(name) {
                            Ok(()) => 0,
                            Err(e) => e,
                        },
                        Err(_) => err::EINVAL,
                    }
                }
                Some(_) => err::EOPNOTSUPP,
                None => err::ENODEV,
            }
        }
        0x8933 => {
            // SIOCGIFINDEX: the index of the named link. musl's
            // `if_nametoindex` asks this, and busybox's `ip ... dev eth0`
            // resolves the device through it - without this arm every
            // device-named command dies with "can't find device".
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            match ifreq_index(buf) {
                Some(index) => {
                    unsafe {
                        ((buf + 16) as *mut i32).write_unaligned(index as i32);
                    }
                    0
                }
                None => err::ENODEV,
            }
        }
        0x8942 => {
            // SIOCGIFTXQLEN: the interface's transmit queue length, which
            // busybox `ip` asks for by name right after the netlink dump -
            // the one field it never reads from the attributes. The name is
            // ignored: one NIC, one answer, and both it and `lo` carry the
            // 1000 that is Linux's own default.
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                ((buf + 16) as *mut i32).write_unaligned(1000);
            }
            0
        }
        0x8927 => {
            // SIOCGIFHWADDR: the hardware address of the named interface.
            // `ip` falls back to this for the MAC a netlink dump did not
            // spell out. One interface exists; its address is the one the
            // stack's static configuration assumes.
            if !buffer_ok(buf, 40, true, from_user) {
                return err::EFAULT;
            }
            unsafe {
                let at = |offset: usize| (buf + offset) as *mut u8;
                // ifr_hwaddr: sockaddr with family ARPHRD_ETHER, then the MAC.
                (at(16) as *mut u16).write_unaligned(1);
                for (i, byte) in [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56]
                    .into_iter()
                    .enumerate()
                {
                    at(18 + i).write_unaligned(byte);
                }
            }
            0
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x4600 => {
            // FBIOGET_VSCREENLINFO: the framebuffer's own description, in
            // `struct fb_var_screeninfo` - the resolution a program sizes
            // its surface from, and the bitfields that tell it where in one
            // 32-bit pixel each channel lives. The device draws BGRA, which
            // as a little-endian word is 0xAARRGGBB - red at bit 16, green
            // at 8, blue at 0 - and that mapping is what the fields carry.
            // Everything a headless display cannot promise - timings, sync
            // polarity, rotation - stays zero, which is the ABI's own "not
            // applicable".
            const FB_VAR_SIZE: usize = 160;
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            // The inode is bound first: the downcast borrows from it, and a
            // temporary `Arc` borrowed through would be gone before the
            // answer it lent was read.
            let inode = file.inode();
            let Some(fb) = inode.as_any().downcast_ref::<FbDev>() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, FB_VAR_SIZE, true, from_user) {
                return err::EFAULT;
            }
            let (width, height) = fb.dims();
            unsafe {
                core::ptr::write_bytes(buf as *mut u8, 0, FB_VAR_SIZE);
                let word = |offset: usize, value: u32| {
                    ((buf + offset) as *mut u32).write_unaligned(value);
                };
                word(0, width);
                word(4, height);
                word(8, width);
                word(12, height);
                word(24, 32); // bits_per_pixel
                // The channel bitfields: offset, length, msb_right each.
                word(32, 16); // red.offset
                word(36, 8);
                word(44, 8); // green.offset
                word(48, 8);
                word(56, 0); // blue.offset
                word(60, 8);
                word(68, 24); // transp.offset: alpha
                word(72, 8);
            }
            0
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x4601 => {
            // FBIOPUT_VSCREENLINFO: a request to reshape the framebuffer.
            // Refused rather than faked: changing the resolution tears down
            // the buffer every `mmap` holds a mapping of, and a node whose
            // mappings change length under their holders is a node that
            // corrupts whoever trusted it. EOPNOTSUPP, the errno of a
            // request the device type exists to carry but this one
            // declines.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().as_any().downcast_ref::<FbDev>().is_none() {
                return err::ENOTTY;
            }
            err::EOPNOTSUPP
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x4602 => {
            // FBIOGET_FSCREENINFO: the fixed half of the pair - where the
            // pixel buffer physically is and how long a row is, the numbers
            // an fbdev program needs before its first draw. `FB_TYPE_PACKED
            // _PIXELS` (0) and `FB_VISUAL_TRUECOLOR` (2) are what a plain
            // 32-bit linear buffer answers on Linux.
            const FB_FIX_SIZE: usize = 80;
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            // The inode is bound first, for the same lifetime reason as the
            // arm above it.
            let inode = file.inode();
            let Some(fb) = inode.as_any().downcast_ref::<FbDev>() else {
                return err::ENOTTY;
            };
            if !buffer_ok(buf, FB_FIX_SIZE, true, from_user) {
                return err::EFAULT;
            }
            // One snapshot of the scanout, so the address and the length it
            // answers with are the same buffer's: a resize between the two
            // would hand a program a mapping length that belongs to another
            // screen.
            let scanout = fb.scanout();
            let width = scanout.width;
            unsafe {
                core::ptr::write_bytes(buf as *mut u8, 0, FB_FIX_SIZE);
                let bytes = b"virtio-gpu";
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
                ((buf + 16) as *mut u64).write_unaligned(scanout.fb_phys as u64); // smem_start
                ((buf + 24) as *mut u32).write_unaligned(scanout.fb_len as u32); // smem_len
                ((buf + 28) as *mut u32).write_unaligned(0); // type: packed pixels
                ((buf + 36) as *mut u32).write_unaligned(2); // visual: truecolor
                ((buf + 48) as *mut u32).write_unaligned(width * 4); // line_length
            }
            0
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x4603 => {
            // FBIOPAN_DISPLAY: accept a page-flip request. This framebuffer
            // is a single scanout (`yres_virtual == yres`), so a compositor
            // does not pan between buffer halves here - it draws through its
            // `mmap` instead. Answering success rather than `ENOTTY` is what
            // lets weston's fbdev backend finish its setup against a display
            // whose type is expected to speak this ioctl.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().as_any().downcast_ref::<FbDev>().is_none() {
                return err::ENOTTY;
            }
            // Present the pixels the caller has drawn: ask the render engine to
            // flush and park until they have left. On a headless boot no
            // engine runs, so this returns at once and the ioctl is just the
            // accepted no-op a setup probe expects.
            crate::drivers::video::render::request_flush_blocking();
            0
        }
        #[cfg(not(target_arch = "aarch64"))]
        0x4622 => {
            // FBIO_WAITFORVSYNC: a frame-pacing wait, which this now genuinely
            // is - it sleeps until the next frame the render task *presents*.
            // The honest name for that boundary is a landing, not a vertical
            // blank: there is no display controller here to be blanking, only a
            // frame that has gone to the host and one that has not, and pacing
            // against the former is what a client of this device means.
            //
            // A bounded wait rather than an unbounded one, because the thing it
            // waits for is a frame, and a machine that is presenting no frames -
            // a headless boot, a window the host has stopped taking input for -
            // must still answer a client that asked to be paced. The timeout is
            // the pace itself: 40 ms is a frame at a third of the nominal rate,
            // so a caller that loops on this gets roughly frame-shaped
            // behaviour, and never gets a hang.
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().as_any().downcast_ref::<FbDev>().is_none() {
                return err::ENOTTY;
            }
            let from = crate::drivers::video::render::landings();
            // 10 ticks at this kernel's 250 Hz clock: 40 ms, a frame and a half
            // at the nominal 60 Hz the connector's mode says.
            let deadline = crate::hal::ticks() + 10;
            crate::sched::block_until_deadline(
                || crate::drivers::video::render::landings() != from,
                deadline,
            );
            0
        }
        0x541B => {
            // FIONREAD: "how many bytes can I read before I would block".
            // The two entropy devices are the case this arm is reached for,
            // and Linux refuses them: measured on 7.2.7 (2026-10-01), both
            // `/dev/random` and `/dev/urandom` answer `EINVAL` - root and
            // unprivileged, `O_RDONLY` and `O_WRONLY`, and *before* the
            // argument is looked at (a NULL or unmapped pointer is still
            // `EINVAL`, not `EFAULT`). The 512 this arm used to write was a
            // divergence, and the reason it was written down for was false:
            // OpenRC's `seedrng` never asks `FIONREAD` - the binary in this
            // image sends one ioctl, the `RNDADDENTROPY` credit below, and
            // takes its seed length from `/proc/sys/kernel/random/poolsize`.
            // Other descriptors keep their own queue-length story for a later
            // day; here a pipe or socket asking is answered the honest
            // `ENOTTY` rather than a borrowed zero. (A regular file and a pipe
            // *would* answer 0 with their remaining/queued bytes on Linux;
            // that is a named divergence in `UAPI.md` §5.2, not this arm's.)
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            match file.inode().rdev() {
                Some((1, 8)) | Some((1, 9)) => err::EINVAL,
                _ => err::ENOTTY,
            }
        }
        // The entropy-pool ioctls (`'R'`-typed), answered on the two device
        // numbers the pool's names carry. A `'R'` word on any other descriptor
        // is `ENOTTY`, as it is on Linux (measured on `/dev/rtc0`, `/dev/tty1`
        // and a regular file), and that gate comes first: the device answers
        // before the command does.
        //
        // There is no pool here - one always-ready ChaCha20 stream - so the
        // read reports it full (4096 bits, the same figure
        // `/proc/sys/kernel/random/poolsize` gives) and a privileged credit
        // succeeds by doing nothing, which is honest: the state a credit would
        // move, "entropy available", is already at the ceiling the read
        // reports, so the caller that checks one after the other sees the same
        // answer Linux gives it. What this kernel cannot do it refuses in the
        // only language a caller reads - see the two refusals below.
        //
        // The family is dispatched on the *whole command word*, as Linux's
        // `random_fops_ioctl` switch does, and the words are `abi`'s. Keying on
        // the command byte alone is what broke the boot: OpenRC's seedrng sends
        // `RNDADDENTROPY` = `_IOW('R', 0x03, int [2])` = 0x4008_5203 (its own
        // only ioctl immediate, read off the binary in this image), which
        // landed in the arm that answers for `RNDZAPENTCNT` - nr 3, not nr 4 -
        // and got `EOPNOTSUPP` back: "seedrng: Unable to seed: Not supported".
        // Measured on Linux 7.2.7, a wrong size or direction on a right command
        // is `EINVAL` rather than the command, so matching the whole word is not
        // a refinement of the old shape, it is what makes the old shape wrong.
        request if family_nr(request, b'R').is_some() => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            match file.inode().rdev() {
                Some((1, 8)) | Some((1, 9)) => {}
                _ => return err::ENOTTY,
            }
            // The mutating requests are gated on `CAP_SYS_ADMIN`, and the gate
            // is asked *before* anything is read from the caller's buffer
            // (measured: every argument shape is `EPERM` for an unprivileged
            // caller, including one that would otherwise be `EINVAL`).
            let privileged = crate::sched::current_identity().euid == 0;
            match request {
                abi::RNDGETENTCNT => {
                    // Entropy available, in bits.
                    if !buffer_ok(buf, 4, true, from_user) {
                        return err::EFAULT;
                    }
                    unsafe { (buf as *mut i32).write_unaligned(4096) };
                    0
                }
                abi::RNDADDTOENTCNT => {
                    if !privileged {
                        return err::EPERM;
                    }
                    if !buffer_ok(buf, 4, false, from_user) {
                        return err::EFAULT;
                    }
                    // A negative credit is the `EINVAL` Linux answers; a
                    // positive one has no count here to receive it, which the
                    // read above already says.
                    if unsafe { (buf as *const i32).read_unaligned() } < 0 {
                        return err::EINVAL;
                    }
                    0
                }
                abi::RNDADDENTROPY => {
                    if !privileged {
                        return err::EPERM;
                    }
                    if !buffer_ok(buf, abi::RND_POOL_INFO_SIZE, false, from_user) {
                        return err::EFAULT;
                    }
                    let (count, size) = unsafe {
                        let header = buf as *const i32;
                        (header.read_unaligned(), header.add(1).read_unaligned())
                    };
                    // `abi`'s order, which is Linux's: the count is refused
                    // before the size is believed.
                    let seed_len = match abi::rnd_pool_seed_len(count, size) {
                        Ok(len) => len,
                        Err(errno) => return errno,
                    };
                    // The seed bytes must be where the header says they are -
                    // measured `EFAULT` for a declared 65536 with nothing
                    // behind the header - and the stretch is *checked*, never
                    // copied: this pool has no accumulator to receive them, so
                    // the worst case a caller can steer is one page walk and
                    // `buffer_ok`'s own `MAX_IO` bound (the worst-case rule),
                    // not an allocation of its choosing.
                    if seed_len != 0 {
                        let seed = match buf.checked_add(abi::RND_POOL_INFO_SIZE) {
                            Some(seed) => seed,
                            None => return err::EFAULT,
                        };
                        if !buffer_ok(seed, seed_len, false, from_user) {
                            return err::EFAULT;
                        }
                    }
                    0
                }
                abi::RNDZAPENTCNT | abi::RNDCLEARPOOL => {
                    // Gated on the same capability (measured `EPERM`
                    // unprivileged), asked first. A privileged drain, though,
                    // has nothing to drain: this generator holds no entropy
                    // count that a `zap` could zero, so answering the `0` Linux
                    // gives here claimed a clearing that `RNDGETENTCNT` - still
                    // full - contradicted one moment later. §4.4: an action this
                    // machine cannot perform is refused, not quietly reported
                    // done. It would take a count to say otherwise, and that is
                    // the named gap in `PLAN.md`.
                    if !privileged {
                        return err::EPERM;
                    }
                    err::EOPNOTSUPP
                }
                abi::RNDRESEEDCRNG => {
                    // Linux asks the capability and then re-keys the CRNG from
                    // the input pool, answering `0`; measured on 7.2.7, an
                    // unprivileged caller gets `EPERM` and root gets `0` on
                    // both devices. `rand::reseed` is the re-key this generator
                    // *can* do - it has no input pool, but it has a fresh
                    // hardware seed and the stream it has already produced, and
                    // mixing those is the honest shape of the same promise. The
                    // capability is asked first, before any state moves.
                    if !privileged {
                        return err::EPERM;
                    }
                    crate::rand::reseed();
                    0
                }
                // An unrecognised `'R'` word - a stale `RNDGETPOOL`, a wrong
                // size or direction, a number nobody has taken - is the
                // `EINVAL` Linux answers, and the capability is not even asked
                // (measured, unprivileged: `RNDGETPOOL` → `EINVAL`).
                _ => err::EINVAL,
            }
        }
        // The RTC ioctls `hwclock(8)` speaks, answered on the clock's device
        // number. Both directions read and write the kernel's wall clock -
        // one clock, so `date` and `hwclock -r` cannot disagree - and
        // `RTC_SET_TIME` additionally stores it in the CMOS, where it waits
        // for the next boot's anchor. The alarm/interrupt requests are the
        // family's own ENOTTY, as they are for hardware without them.
        //
        // The family is accepted under two magic bytes. The kernel's own
        // header spells the RTC ioctls `'t'`, but the `linux/rtc.h` busybox
        // was built against carries the older `'p'` - the byte the mc146818
        // driver used before the subsystem moved - and Alpine's hwclock
        // therefore asks with `0x80247009`/`0x4024700a`. Only those two
        // commands cross over: `'p'` is also PPP's family, and a stray PPP
        // request must stay as unknown here as it would be on any machine.
        #[cfg(all(target_os = "none", target_arch = "x86_64"))]
        request
            if family_nr(request, b't').is_some()
                || matches!(request, 0x8024_7009 | 0x4024_700A) =>
        {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().rdev() != Some((10, 135)) {
                return err::ENOTTY;
            }
            let nr = family_nr(request, b't').or(match request {
                0x8024_7009 => Some(0x09),
                0x4024_700A => Some(0x0a),
                _ => None,
            });
            // `struct rtc_time` is nine ints in both directions.
            const RTC_TIME_SIZE: usize = 9 * 4;
            match nr {
                Some(0x09) => {
                    // RTC_RD_TIME.
                    if !buffer_ok(buf, RTC_TIME_SIZE, true, from_user) {
                        return err::EFAULT;
                    }
                    let fields =
                        crate::drivers::rtc_dev::time_fields(crate::drivers::rtc_dev::now_epoch());
                    unsafe {
                        for (i, value) in fields.iter().enumerate() {
                            (buf as *mut i32).add(i).write_unaligned(*value);
                        }
                    }
                    0
                }
                Some(0x0a) => {
                    // RTC_SET_TIME: a field set no calendar carries is the
                    // EINVAL the real driver returns, and nothing is written.
                    if !buffer_ok(buf, RTC_TIME_SIZE, false, from_user) {
                        return err::EFAULT;
                    }
                    let mut fields = [0i32; 9];
                    unsafe {
                        for (i, value) in fields.iter_mut().enumerate() {
                            *value = (buf as *const i32).add(i).read_unaligned();
                        }
                    }
                    match crate::drivers::rtc_dev::fields_to_epoch(&fields) {
                        Ok(epoch) => {
                            if crate::hal::write_epoch(epoch) {
                                0
                            } else {
                                err::EINVAL
                            }
                        }
                        Err(e) => err_of(e),
                    }
                }
                _ => err::ENOTTY,
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        request if family_nr(request, b'E').is_some() => evdev_ioctl(fd, request, buf, from_user),
        // uinput (`'U'`-typed): how a program makes an input device of its own.
        // The descriptor *is* the device, so every request here is about the one
        // this open file description owns.
        #[cfg(target_os = "none")]
        #[cfg(not(target_arch = "aarch64"))]
        request if family_nr(request, b'U').is_some() => uinput_ioctl(fd, request, buf, from_user),
        // Console-ownership (`'V'`-typed VT_*) and keyboard/video-mode
        // (`'K'`-typed KD*) ioctls, which a seat manager drives the line
        // through when it takes a terminal away from the text console and
        // hands it to a compositor.
        #[cfg(not(target_arch = "aarch64"))]
        request if family_nr(request, b'V').is_some() => vt_ioctl(fd, request, buf, from_user),
        #[cfg(not(target_arch = "aarch64"))]
        request if family_nr(request, b'K').is_some() => kd_ioctl(fd, request, buf, from_user),
        // The inode attribute word: `FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS`,
        // the two requests `lsattr` and `chattr` are made of. The `'f'` family,
        // which is Linux's own for exactly these.
        request if family_nr(request, b'f').is_some() => {
            fs_flags_ioctl(fd, request, buf, from_user)
        }
        // dma-buf ioctls (`'b'`-typed), on the descriptor a PRIME export
        // handed out. A dma-buf here is coherent guest RAM - the same pages
        // the exporter and every mapper share - so the CPU access bracket
        // `DMA_BUF_IOCTL_SYNC` is accepted as the no-op it documents itself
        // as for coherent memory, and `DMA_BUF_SET_NAME` is a debug label
        // nothing reads. Anything else on one of these descriptors is as
        // unknown as it is on Linux.
        #[cfg(target_os = "none")]
        request if family_nr(request, b'b').is_some() => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            if file.inode().as_dmabuf().is_none() {
                return err::ENOTTY;
            }
            if !buffer_ok(buf, 8, false, from_user) {
                return err::EFAULT;
            }
            match family_nr(request, b'b') {
                Some(0) | Some(1) => 0, // SYNC / SET_NAME
                _ => err::ENOTTY,
            }
        }
        // DRM/KMS ioctls (`'d'`-typed), routed to the card handler with the
        // command number already decoded out of the request word. Deferred off
        // aarch64 with the DRM driver itself: there is no card to reach.
        #[cfg(all(target_os = "none", not(target_arch = "aarch64")))]
        request if family_nr(request, b'd').is_some() => match family_nr(request, b'd') {
            Some(nr) => crate::drivers::drm::drm_ioctl(fd, nr, buf),
            None => err::ENOTTY,
        },
        // The OSS dsp family (`'P'`-typed), on whichever character stream the
        // descriptor opened: the stream's rate, channels and format belong to
        // whoever writes its bytes, which is the whole point of the family
        // and the thing this kernel used to decide at boot instead. The
        // dispatcher validates the caller's buffer against the `_IOC` word's
        // own direction and size; what the device answers is the answer.
        request if family_nr(request, b'P').is_some() => {
            let size = ((request >> 16) & 0x3fff) as usize;
            let dir = (request >> 30) & 3;
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(f) => f,
                Err(e) => return err_of(e),
            };
            let inode = file.inode();
            let Some(device) = inode.as_chardev() else {
                return err::ENOTTY;
            };
            if size > 0 {
                // `_IOC_WRITE` is the kernel reading the caller's buffer,
                // `_IOC_READ` the kernel writing it - and `_IOWR` promises
                // both, which is why the two bits are checked separately.
                if dir & 1 != 0 && !buffer_ok(buf, size, false, from_user) {
                    return err::EFAULT;
                }
                if dir & 2 != 0 && !buffer_ok(buf, size, true, from_user) {
                    return err::EFAULT;
                }
            }
            device.ioctl(request, buf).unwrap_or(err::ENOTTY)
        }
        _ => err::ENOTTY,
    }
}

/// The command number of a request word, for a family whose magic byte is
/// `magic` - or `None` when the request does not belong to the family.
///
/// Linux lays an `_IOC` word out with the family byte at bits 8..15 and the
/// command number in the low byte, and filters a family's requests by
/// `_IOC_TYPE(request) == magic` - nothing else. That filter is the whole
/// function: a request whose *number* happens to be another family's magic byte
/// must not be read as that other family's, which is exactly what a fallback
/// that also matched on the low byte did. `KDSKBMODE` is `_IO('K', 0x45)`, and
/// evdev's family byte is `'E'` = 0x45, so the seat manager's "take the keyboard
/// off the console" call was answered as an evdev request on a tty and refused
/// with `ENOTTY` - a real `KDSKBMODE` failing for looking like someone else.
/// The two spellings are not symmetrical either: every number a family uses
/// collides with some other family's byte, so which one wins depends on the
/// order the arms are written in, and that is not a protocol.
fn family_nr(request: u32, magic: u8) -> Option<u32> {
    if (request >> 8) & 0xff == magic as u32 {
        Some(request & 0xff)
    } else {
        None
    }
}

/// The virtual-terminal (`'V'`-typed) ioctls a seat manager uses to own the
/// console a compositor runs on. The machine has six of them (`/dev/tty1`..6,
/// pty-backed screens the console multiplexer shows one at a time), so the
/// queries name the one actually on the screen and the switches move it; the
/// obsolete or do-nothing ones - `VT_SENDSIG`, `VT_DISALLOCATE`, the two resize
/// forms, whose answer `TIOCGWINSZ` carries - succeed without doing anything,
/// which is what they mean. A `seatd` that cannot get past this dance never
/// hands the card to weston, which is why the reads answer from live state
/// rather than a flat success.
///
/// `VT_SETMODE` and `VT_RELDISP` are the two that cannot be flat successes, and
/// they are the reason the console's mode is recorded at all: a session that
/// asks to be consulted before its screen is taken away (`VT_PROCESS`) has to
/// find, on the other side of a `relsig`, that the kernel really did wait for
/// its answer - and has to be able to *refuse* it. The policy lives in
/// `crate::vfs::vtmode`; this is the argument checking and the errno mapping.
///
/// Every request that *names* a console - as opposed to asking what consoles
/// exist - also asks who is calling, and the answer Linux gives is the same for
/// all of them: `EPERM` unless the descriptor is the terminal's own foreground
/// job or the caller is superuser. That gate is what makes a hand-over a
/// conversation between the session that holds the screen and the one that
/// wants it, rather than anything holding a `/dev/tty` fd being able to silence
/// a compositor's `relsig`, or to answer it.
#[cfg(not(target_arch = "aarch64"))]
fn vt_ioctl(fd: i32, request: u32, buf: usize, from_user: bool) -> isize {
    if (request >> 8) & 0xff != b'V' as u32 {
        return err::ENOTTY;
    }
    // These name a console, so only a tty answers them.
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(tty) = inode.as_tty() else {
        return err::ENOTTY;
    };
    // Who is asking is the one part of this family that lives above the VFS:
    // the foreground question, the root question and the thread group a
    // `VT_SETMODE` promise is recorded against are all answers the syscall
    // layer holds (it may name `sched`), so they are handed down rather than
    // reached up for. Every accept/reject decision for the `'V'` family is the
    // console's own, in `Tty::vt_ioctl` (V-2, the same move as the `'K'` and
    // `'E'` families).
    let caller = crate::vfs::tty::VtCaller {
        task: crate::sched::current(),
        root: crate::sched::current_is_root(),
        tgid: crate::sched::current_tgid().0 as u32,
    };
    tty.vt_ioctl(caller, request, buf, from_user)
}

/// The keyboard/video-mode (`'K'`-typed) ioctls that bracket a VT handover: put
/// the keyboard in a mode where the console stops eating the keys, and put the
/// console in graphics mode so the framebuffer is not painted over by the text
/// driver.
///
/// Both *are* recorded, per console (`crate::vfs::vcmux`), because a seat
/// manager's whole handover is these two words: `weston` through `seatd` sets its
/// console graphics and its keyboard off, and the kernel that accepts both and
/// remembers nothing keeps drawing the login prompt over the compositor's screen
/// - one keystroke at a time, because the shell still echoes what it was handed.
///
/// The command numbers are `<linux/kd.h>`'s: `KDSETMODE 0x4B3A`,
/// `KDGETMODE 0x4B3B`, `KDGKBMODE 0x4B44`, `KDSKBMODE 0x4B45`, so the low byte of
/// the request word after the `'K'` magic is `0x3a`, `0x3b`, `0x44` and `0x45`.
/// `FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS`: the inode attribute word, which
/// `lsattr` prints as its one-line `--------------e-------` and `chattr` writes.
///
/// Two requests, and the family byte is `'f'` - Linux's, from `fs.h`. Anything
/// else in the family is `ENOTTY`, the answer every family this kernel does not
/// speak gets: there is no third request here to refuse more precisely.
///
/// The argument is a `long`, so the word is read and written as eight bytes with
/// only the low four used - which is what Linux passes and what every caller
/// allocates. Reading four would work on a little-endian machine and be wrong
/// everywhere else, and "works on the machine I tested" is not a reason.
fn fs_flags_ioctl(fd: i32, request: u32, buf: usize, from_user: bool) -> isize {
    const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
    const FS_IOC_SETFLAGS: u32 = 0x4008_6602;

    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    match request {
        FS_IOC_GETFLAGS => {
            // Whether this filesystem has an attribute word at all, which is a
            // different question from which flags it holds. `ENOTTY` is Linux's
            // answer for a filesystem whose inode operations have no
            // `fileattr` half - procfs, sysfs, devfs, a pipe, a socket - and it
            // is what `lsattr` reports as "not supported" rather than as a file
            // with no attributes.
            let flags = match inode.flags() {
                Ok(flags) => flags,
                Err(_) => return err::ENOTTY,
            };
            if !buffer_ok(buf, 8, true, from_user) {
                return err::EFAULT;
            }
            unsafe { (buf as *mut u64).write_unaligned(flags as u64) };
            0
        }
        FS_IOC_SETFLAGS => {
            // The same question as above, asked through the door that has no
            // other way to fail, because the *next* refusal has to mean
            // something else. A filesystem with no attribute word is `ENOTTY`
            // here too, on both requests; once it is known to have one, a
            // refusal is about the flags and is `EOPNOTSUPP` - "this filesystem
            // cannot do that" - which is the answer `chattr` reports as
            // "Operation not supported" and the one that sends a caller to its
            // fallback rather than to a retry.
            if inode.flags().is_err() {
                return err::ENOTTY;
            }
            if !buffer_ok(buf, 8, false, from_user) {
                return err::EFAULT;
            }
            let flags = (unsafe { (buf as *const u64).read_unaligned() }) as u32;
            // The two bits Linux gates on a capability, and *only* those: a
            // caller without `CAP_LINUX_IMMUTABLE` may still set `nodump` or
            // `noatime`, and is refused the one change it cannot make. The test
            // is on the change rather than on the word, so writing back the
            // flags a file already has is always allowed - which is what every
            // `chattr` run does, since it reads the word, edits one bit and
            // writes the whole thing back.
            let old = crate::vfs::flags_of(&inode);
            let privileged = crate::vfs::FS_IMMUTABLE_FL | crate::vfs::FS_APPEND_FL;
            if (flags ^ old) & privileged != 0
                && !crate::sched::current_credentials()
                    .has_effective(crate::cred::CAP_LINUX_IMMUTABLE)
            {
                return err::EPERM;
            }
            match inode.set_flags(flags) {
                Ok(()) => {
                    // The flags are metadata, so the watcher that asked to hear
                    // about `chmod` hears about this too.
                    crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
                    0
                }
                Err(e) => err_of(e),
            }
        }
        _ => err::ENOTTY,
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn kd_ioctl(fd: i32, request: u32, buf: usize, from_user: bool) -> isize {
    if (request >> 8) & 0xff != b'K' as u32 {
        return err::ENOTTY;
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    // Which of the six consoles this descriptor is the terminal of. A tty that
    // is not one of them - a `/dev/pts` slave, a master - has no screen and no
    // keyboard mode to set, and Linux's answer for it is the same one as for a
    // tty that is not a console at all.
    let tty_inode = file.inode();
    let Some(tty) = tty_inode.as_tty() else {
        return err::ENOTTY;
    };
    let Some(vc) = tty.vc_number() else {
        return err::ENOTTY;
    };
    // Whether the caller holds the DRM card is the one question the console
    // cannot answer for itself: the card driver sits above the VFS and the
    // console device must not reach up into it, so the syscall layer - which may
    // name `drivers` - asks it here and hands the answer down. Every other
    // decision in the `K` family is the console's own to make, in `Tty::kd_ioctl`
    // - the ioctl-to-device move (V-2), with the DRM edge kept on this side.
    let owns_card = crate::drivers::drm::card_grabbed_by_current();
    tty.kd_ioctl(vc, request, buf, from_user, owns_card)
}

/// The evdev (`'E'`-typed) ioctl set libinput reads to classify a device
/// before it consumes any event. `request` carries the direction in bits
/// 30-31, the number in whichever half the family byte did not take (see
/// [`family_nr`]) and the caller's buffer size in 16-29; the
/// number selects which question this is. Reads answer from the node's
/// frozen capability picture (`drivers/virtio/input.rs`); the keymap and
/// force-feedback writes are accepted rather than refused, since a compositor
/// that only wanted them to take effect quietly should not be stopped by a
/// repeat rate this kernel does not have. `EVIOCGRAB` is the one write with a
/// real answer to give: it belongs to the calling descriptor, and a device
/// already claimed answers `EBUSY`.
/// The `'U'`-typed requests: `/dev/uinput`, where userland makes input devices.
///
/// The descriptor *is* the device, so every request here is about the one this
/// open file description owns. Resolving it stays at this boundary - a bad fd is
/// `EBADF` and a node that is not a uinput device is `ENOTTY`, which are the
/// syscall layer's answers to give - and the request itself is the device's to
/// judge (`Uinput::ioctl`, the same split the `'E'` and `'K'` families took:
/// `PLAN.md` §11.2 Step B2).
#[cfg(not(target_arch = "aarch64"))]
fn uinput_ioctl(fd: i32, request: u32, buf: usize, from_user: bool) -> isize {
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(dev) = inode.as_any().downcast_ref::<Uinput>() else {
        return err::ENOTTY;
    };
    dev.ioctl(file.ofd(), request, buf, from_user)
}

#[cfg(not(target_arch = "aarch64"))]
fn evdev_ioctl(fd: i32, request: u32, buf: usize, from_user: bool) -> isize {
    // Resolve the descriptor to its evdev node here, at the user boundary - the
    // bad-fd `EBADF` and the not-an-evdev-node `ENOTTY` are the syscall layer's
    // to give - then hand the decoded request to the device. Every accept/reject
    // decision for the `'E'` family now lives in `EvdevNode::ioctl` (the input
    // driver), not in this switchboard: Linux's `evdev_unlocked_ioctl` is the
    // device's, and the node answers each request from its own state.
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(ev) = inode.as_any().downcast_ref::<EvdevNode>() else {
        return err::ENOTTY;
    };
    ev.ioctl(file.ofd(), request, buf, from_user)
}
/// whatever name it currently carries - is 2. `None` names no device, which
/// the interface ioctls answer with `ENODEV`.
fn ifreq_index(buf: usize) -> Option<u32> {
    let mut name = [0u8; 16];
    unsafe {
        for (i, slot) in name.iter_mut().enumerate() {
            *slot = ((buf + i) as *const u8).read_unaligned();
            if *slot == 0 {
                break;
            }
        }
    }
    let len = name.iter().position(|b| *b == 0)?;
    let name = core::str::from_utf8(&name[..len]).ok()?;
    ifreq_index_named(name)
}

/// Whether a parsed interface name resolves to a link index. The loopback is
/// the always-present index 1; the ethernet is 2, but only once the net stack
/// knows that name. Off aarch64 the lookup reaches into `crate::net`; on it the
/// stack is deferred (its driver stage follows), so no name resolves and the
/// interface ioctls answer `ENODEV` - the honest answer for "there is no such
/// link", not a fabricated index.
#[cfg(not(target_arch = "aarch64"))]
fn ifreq_index_named(name: &str) -> Option<u32> {
    if name == "lo" {
        return Some(1);
    }
    crate::net::link_facts()
        .filter(|facts| facts.eth_name == name)
        .map(|_| 2)
}
#[cfg(target_arch = "aarch64")]
fn ifreq_index_named(_name: &str) -> Option<u32> {
    None
}

/// The flags an interface ioctl reports - the same bits the netlink link
/// dump carries, as the `ifr_flags` halfword: the link's kind, and the
/// up-and-running pair that rides the administrative bit.
#[cfg(not(target_arch = "aarch64"))]
fn ifreq_flags(index: u32) -> Option<u16> {
    let facts = crate::net::link_facts()?;
    let (up, loopback) = match index {
        1 => (facts.lo_up, true),
        2 => (facts.eth_up, false),
        _ => return None,
    };
    let mut flags = if loopback { 0x8 } else { 0x2 | 0x1000 };
    if up {
        flags |= 0x1 | 0x40;
    }
    Some(flags)
}
#[cfg(target_arch = "aarch64")]
fn ifreq_flags(_index: u32) -> Option<u16> {
    // No net stack to report link facts on this machine yet; the interface
    // ioctls fall to `ENODEV` rather than invent a link's state.
    None
}
