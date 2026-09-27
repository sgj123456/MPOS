//! t-umask - the creation mask, and the mode a create is actually given.
//!
//! Pinned here, per umask(2): the call answers what the mask *was* while
//! storing what it is given, so a program can save and restore it; the mask
//! is the bits a create must clear out of the mode it was asked for, so a
//! create of `0666` under `0077` arrives `0600`; and only the low twelve
//! bits survive - those are all a mode has, and `umask(017777)` leaves
//! `07777` behind on the next read. The mask is the process's own, which is
//! the reason the test restores it on the way out.
//!
//! mpos applies the mask where Linux does, in the create path
//! (`masked_mode` in `src/syscall/linux/fs.rs`, which reads
//! `sched::current_umask`), so no check here records a divergence.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-umask.f\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-umask\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);

    // The first call answers the inherited mask, whatever it is; the point of
    // the next one is that it answers what the first *set*.
    let m0 = lin::umask(0o022);

    // The round trip: set 0077, be told the mask is 0022.
    lin::check_eq(
        &mut failed,
        b"umask answers the mask it is replacing",
        lin::umask(0o077),
        0o022,
    );

    // What the mask is *for*: a create's mode has the mask's bits cleared.
    // 0666 & ~0077 is 0600, and that is what the file must carry.
    let fd = lin::openat(
        lin::AT_FDCWD,
        PATH,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o666,
    ) as i32;
    lin::check_true(&mut failed, b"the file is created", fd >= 0);
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
    lin::check_eq(
        &mut failed,
        b"fstat succeeds",
        lin::fstat(fd, &raw mut st),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a create of 0666 under umask 0077 is stored as 0600",
        (st.st_mode & 0o777) as isize,
        0o600,
    );
    let _ = lin::close(fd);

    // Only the twelve bits a mode has are kept: 017777 stores 07777.
    lin::check_eq(
        &mut failed,
        b"setting a mask with bits above the twelve answers the old mask",
        lin::umask(0o17777),
        0o077,
    );
    lin::check_eq(
        &mut failed,
        b"the mask kept only its low twelve bits",
        lin::umask(0),
        0o7777,
    );

    // The mask is process state, not machine state: put back what was found.
    // The first call answers the zero the check above left behind; the second
    // reads the restored mask back and must answer what was found.
    let _ = lin::umask(m0 as usize);
    lin::check_eq(
        &mut failed,
        b"the mask is restored to what the test found",
        lin::umask(m0 as usize),
        m0,
    );

    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-umask: panicked\n")
}
