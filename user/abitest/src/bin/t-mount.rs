//! t-mount - one test for mount(2)'s MS_BIND and umount2(2)'s side of it:
//! the second name on an already-mounted tree.
//!
//! This is the call OpenRC's boot cleanup makes (`mount --bind / /tmp/tmp.X`,
//! to test whether `/run` sits on the root), and the one whose absence was a
//! "No such device" on every boot. Pinned here: binding a directory makes its
//! contents visible under the target name, the same files are *the same*
//! storage (a write through one name reads back through the other, and both
//! stat with one `st_dev` - a bind shares the superblock, it does not mint a
//! new one), a second bind on a taken mountpoint answers EBUSY, binding a
//! file answers the EINVAL the kernel's refusal spells rather than succeeding,
//! and the umount peels the bind off the target: after it, the target's own
//! (empty) directory shows through again and the source is untouched.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const MS_BIND: usize = 0x1000;

static SRC: [u8; 15] = *b"/tmp/t-mount_a\0";
static SRC_MARKER: [u8; 22] = *b"/tmp/t-mount_a/marker\0";
static DST: [u8; 15] = *b"/tmp/t-mount_b\0";
static DST_MARKER: [u8; 22] = *b"/tmp/t-mount_b/marker\0";
static DST_THROUGH: [u8; 22] = *b"/tmp/t-mount_b/second\0";
static SRC_THROUGH: [u8; 22] = *b"/tmp/t-mount_a/second\0";
static NONE: [u8; 5] = *b"none\0";
static mut ST_A: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();
static mut ST_B: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

/// Create `path` with `contents` and close it, failing the test if either
/// step does not answer 0 or the buffer's length. `0o1000` is `O_TRUNC`,
/// which `lin` carries no name for.
fn touch(failed: &mut bool, what: &[u8], path: &[u8], contents: &[u8]) {
    let fd = lin::openat(
        lin::AT_FDCWD,
        path,
        lin::O_WRONLY | lin::O_CREAT | 0o1000,
        0o644,
    );
    lin::check_true(failed, what, fd >= 0);
    if fd >= 0 {
        let n = lin::write(fd as i32, contents);
        lin::check_eq(
            failed,
            b"the write lands in full",
            n,
            contents.len() as isize,
        );
        lin::close(fd as i32);
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mount\n");
    let mut failed = false;

    lin::check_eq(
        &mut failed,
        b"the source directory is made",
        lin::mkdirat(lin::AT_FDCWD, &SRC, 0o755),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the target directory is made",
        lin::mkdirat(lin::AT_FDCWD, &DST, 0o755),
        0,
    );
    touch(
        &mut failed,
        b"the marker file is created",
        &SRC_MARKER,
        b"shared",
    );

    // The bind itself. busybox spells the fstype `none` and the kernel
    // ignores the word - MS_BIND never consults the type table.
    let r = lin::mount(&SRC, &DST, &NONE, MS_BIND);
    lin::check_eq(&mut failed, b"mount --bind succeeds", r, 0);

    // The source's contents are visible under the target name.
    let fd = lin::openat(lin::AT_FDCWD, &DST_MARKER, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the marker reads through the bind", fd >= 0);
    if fd >= 0 {
        let mut buf = [0u8; 8];
        let n = lin::read(fd as i32, &mut buf);
        lin::check_eq(&mut failed, b"with the bytes it carries", n, 6);
        lin::check_true(
            &mut failed,
            b"the marker says \"shared\"",
            &buf[..6] == b"shared",
        );
        lin::close(fd as i32);
    }

    // One storage, two names: a file created through the bind is the
    // source's, and both names answer the same st_dev - the bind shares
    // the filesystem, it does not mint a new instance.
    touch(
        &mut failed,
        b"a file created through the bind opens",
        &DST_THROUGH,
        b"x",
    );
    let fd = lin::openat(lin::AT_FDCWD, &SRC_THROUGH, lin::O_RDONLY, 0);
    lin::check_true(
        &mut failed,
        b"and is visible under the source name",
        fd >= 0,
    );
    if fd >= 0 {
        lin::close(fd as i32);
    }
    let ra = lin::newfstatat(lin::AT_FDCWD, &SRC, &raw mut ST_A as *mut lin::Stat, 0);
    let rb = lin::newfstatat(lin::AT_FDCWD, &DST, &raw mut ST_B as *mut lin::Stat, 0);
    lin::check_true(&mut failed, b"both names stat", ra == 0 && rb == 0);
    lin::check_eq(
        &mut failed,
        b"both names answer one st_dev (a bind shares the superblock)",
        unsafe {
            (*(&raw mut ST_A as *const lin::Stat)).st_dev
                == (*(&raw mut ST_B as *const lin::Stat)).st_dev
        } as isize,
        1,
    );

    // A second bind on the taken mountpoint: the table carries one mount
    // per point, and the answer is the same EBUSY a second mount is.
    let r = lin::mount(&SRC, &DST, &NONE, MS_BIND);
    lin::check_eq(
        &mut failed,
        b"a second bind on one point answers EBUSY",
        r,
        lin::EBUSY,
    );

    // Binding a file is refused with EINVAL, not quietly accepted: the
    // resolver's second name is for directories.
    let r = lin::mount(&SRC_MARKER, &DST, &NONE, MS_BIND);
    lin::check_eq(
        &mut failed,
        b"binding a file answers EINVAL",
        r,
        lin::EINVAL,
    );

    // The peel. After the umount the target's own - now empty - directory
    // shows through, and the source keeps its files.
    let r = lin::umount2(&DST, 0);
    lin::check_eq(&mut failed, b"umount2 of the bind succeeds", r, 0);
    let fd = lin::openat(lin::AT_FDCWD, &DST_MARKER, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"the marker is gone from the target after the umount",
        fd,
        lin::ENOENT,
    );
    let fd = lin::openat(lin::AT_FDCWD, &SRC_THROUGH, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"and the source kept its files", fd >= 0);
    if fd >= 0 {
        lin::close(fd as i32);
    }

    // Cleanup: the test leaves the scratch tree as it found it.
    let _ = lin::unlinkat(lin::AT_FDCWD, &SRC_THROUGH, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, &SRC_MARKER, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, &SRC, lin::AT_REMOVEDIR);
    let _ = lin::unlinkat(lin::AT_FDCWD, &DST, lin::AT_REMOVEDIR);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mount: panicked\n")
}
