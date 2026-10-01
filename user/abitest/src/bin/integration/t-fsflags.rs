//! t-fsflags - `FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS`: the inode attribute word
//! `lsattr` prints and `chattr` writes.
//!
//! Report that started this: `lsattr` on a file on the real rootfs printed
//! `lsattr: reading ./README: Not a tty` - musl's `strerror(ENOTTY)` - because
//! the ioctl had no handler at all and fell through the dispatch to the honest
//! `_ => ENOTTY`. So `GETFLAGS` working is the first thing pinned here.
//!
//! The second thing is the part that is easy to fake and worthless if faked:
//! *the flags do something*. A `SETFLAGS` that stored the word and enforced
//! nothing would pass every round-trip check in this file while `chattr +i`
//! protected nothing, so every flag this kernel accepts is pinned by the
//! behaviour it changes, and every flag it cannot honour is pinned by being
//! refused rather than stored:
//!
//!   * `IMMUTABLE` - no write, no open for write, no truncate, no unlink, no
//!     rename, no second link, no `chmod`; and reading it is still allowed.
//!   * `APPEND` - writes that reach the end go through, writes that would not
//!     are `EPERM`, and the file can be neither cut back nor removed.
//!   * `NODUMP`, `NOATIME`, `SYNC`, `DIRSYNC` - stored and read back. Three of
//!     the four are honoured by construction here (a write-through filesystem
//!     already does what `SYNC` promises, and `NODUMP` is a promise to
//!     userland that reads it back), and `NOATIME` is read where the access
//!     time would otherwise move.
//!   * everything else - `EOPNOTSUPP`, and *nothing stored*. `chattr +c`
//!     succeeding and compressing nothing is the failure this refuses.
//!
//! The file system under all of it is the gate's own root: a ramfs, which is
//! what makes these assertions about the kernel rather than about ext4. The
//! on-disk half - that `i_flags` really round-trips through an ext4 image - is
//! a host test (`vfs::ext4::tests::the_inode_flags_are_the_ones_on_the_disk`),
//! because the gate boots no disk.
//!
//! The last check is the capability: `CAP_LINUX_IMMUTABLE` is what `i` and `a`
//! are gated on, and a caller that lacks it is refused *those two bits* while
//! still being allowed the rest - which is why the check is run as a uid that
//! owns the file and has no capability, rather than as root, where every answer
//! would be the same.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `_IOR('f', 0x01, long)`: read the attribute word.
const FS_IOC_GETFLAGS: usize = 0x8008_6601;
/// `_IOW('f', 0x02, long)`: write it.
const FS_IOC_SETFLAGS: usize = 0x4008_6602;
/// `_IOR('f', 0x09, long)`: a command nobody has taken, spelled exactly like
/// `GETFLAGS` so that a dispatch on the low byte alone would answer it.
const FS_IOC_UNKNOWN_READ: usize = 0x8008_6609;
/// The same command as a write.
const FS_IOC_UNKNOWN_WRITE: usize = 0x4008_6609;

// The bits, from `<linux/fs.h>` - spelled out rather than shared with the
// kernel, because a number this test took from the kernel it is testing would
// agree with the kernel about a wrong answer.
const FS_SYNC_FL: u64 = 0x0000_0008;
const FS_IMMUTABLE_FL: u64 = 0x0000_0010;
const FS_APPEND_FL: u64 = 0x0000_0020;
const FS_NODUMP_FL: u64 = 0x0000_0040;
const FS_NOATIME_FL: u64 = 0x0000_0080;
const FS_DIRSYNC_FL: u64 = 0x0001_0000;
const FS_COMPR_FL: u64 = 0x0000_0004;
const FS_ENCRYPT_FL: u64 = 0x0000_0800;
/// A bit no filesystem here has ever heard of, to prove the check is on the
/// whole word rather than on a list of known-bad names. Bit 31 is the one
/// Linux's own `FS_*` vocabulary stops short of - the highest it names is
/// `FS_CASEFOLD_FL` at bit 30 - so this is a bit the UAPI leaves unassigned
/// rather than one this kernel merely has not implemented.
const FS_UNKNOWN_FL: u64 = 0x8000_0000;

/// The four flags a caller may set and this kernel keeps.
const KEPT: u64 = FS_NODUMP_FL | FS_NOATIME_FL | FS_SYNC_FL | FS_DIRSYNC_FL;

static FILE: &[u8] = b"/tmp/t-fsflags.bin\0";
static MOVED: &[u8] = b"/tmp/t-fsflags.moved\0";
static LINKED: &[u8] = b"/tmp/t-fsflags.link\0";
/// A second name in a directory a uid without `CAP_LINUX_IMMUTABLE` may write
/// to, which is what the capability check at the end needs.
static OWNED: &[u8] = b"/tmp/t-fsflags-owned.bin\0";

/// The word the ioctl writes into - a `long`, which is what every caller
/// allocates and what the kernel must therefore read and write in full.
static mut WORD: u64 = 0;

fn getflags(fd: i32) -> isize {
    unsafe {
        WORD = 0xdead_beef_dead_beef;
        lin::ioctl(fd, FS_IOC_GETFLAGS, &raw mut WORD as usize)
    }
}

fn setflags(fd: i32, flags: u64) -> isize {
    unsafe {
        WORD = flags;
        lin::ioctl(fd, FS_IOC_SETFLAGS, &raw mut WORD as usize)
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fsflags\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, FILE, 0);
    let fd = lin::openat(
        lin::AT_FDCWD,
        FILE,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o644,
    );
    lin::check_true(&mut failed, b"a file to flag", fd >= 0);
    let fd = fd as i32;
    lin::check_eq(
        &mut failed,
        b"and it takes bytes",
        lin::write(fd, b"hello\n"),
        6,
    );

    // GETFLAGS on a fresh file: zero, and the whole `long` written - the high
    // half is what a read of four bytes instead of eight would get wrong.
    lin::check_eq(&mut failed, b"GETFLAGS succeeds", getflags(fd), 0);
    lin::check_eq(
        &mut failed,
        b"and a fresh file has no flags at all",
        unsafe { WORD } as isize,
        0,
    );

    // Every flag this kernel keeps, set in one go and read back whole.
    lin::check_eq(
        &mut failed,
        b"SETFLAGS takes four flags",
        setflags(fd, KEPT),
        0,
    );
    lin::check_eq(&mut failed, b"and reads them back", getflags(fd), 0);
    lin::check_eq(
        &mut failed,
        b"all four, and nothing else",
        unsafe { WORD } as isize,
        KEPT as isize,
    );

    // A bit this kernel cannot honour is refused, and *nothing is stored*: the
    // refusal is the answer, and the word is left exactly as it was.
    lin::check_eq(
        &mut failed,
        b"a compression flag answers EOPNOTSUPP",
        setflags(fd, KEPT | FS_COMPR_FL),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(&mut failed, b"and nothing was stored", getflags(fd), 0);
    lin::check_eq(
        &mut failed,
        b"the word is unchanged",
        unsafe { WORD } as isize,
        KEPT as isize,
    );
    // Including a bit no vocabulary here names: the check is on the word, not
    // on a list of the flags this kernel happens to know.
    lin::check_eq(
        &mut failed,
        b"a bit nobody has heard of is refused too",
        setflags(fd, KEPT | FS_UNKNOWN_FL),
        lin::EOPNOTSUPP,
    );
    lin::check_eq(
        &mut failed,
        b"an encryption flag is refused with the rest",
        setflags(fd, FS_ENCRYPT_FL),
        lin::EOPNOTSUPP,
    );
    // And back to nothing, so the next section starts clean.
    lin::check_eq(&mut failed, b"clearing them works", setflags(fd, 0), 0);

    // A request in the family that nothing answers is ENOTTY, in both
    // directions - the family byte is not a permission slip.
    lin::check_eq(
        &mut failed,
        b"an unknown read request is ENOTTY",
        lin::ioctl(fd, FS_IOC_UNKNOWN_READ, &raw mut WORD as usize),
        lin::ENOTTY,
    );
    lin::check_eq(
        &mut failed,
        b"an unknown write request is ENOTTY",
        lin::ioctl(fd, FS_IOC_UNKNOWN_WRITE, &raw mut WORD as usize),
        lin::ENOTTY,
    );

    // A filesystem with no attribute word at all answers ENOTTY rather than
    // "no flags": a pipe has no inode to carry them, and Linux's answer there
    // is the same one it gives a filesystem with no `fileattr` operations.
    let mut fds = [0i32; 2];
    if lin::pipe2(&mut fds, 0) == 0 {
        lin::check_eq(
            &mut failed,
            b"a pipe has no attribute word",
            getflags(fds[0]),
            lin::ENOTTY,
        );
        lin::check_eq(
            &mut failed,
            b"and cannot be told it has one",
            setflags(fds[0], FS_NODUMP_FL),
            lin::ENOTTY,
        );
        lin::close(fds[0]);
        lin::close(fds[1]);
    } else {
        lin::check_true(&mut failed, b"a pipe to ask about", false);
    }

    // ---- IMMUTABLE: the flag that means "not this file" ----
    lin::check_eq(&mut failed, b"chattr +i", setflags(fd, FS_IMMUTABLE_FL), 0);
    lin::check_eq(&mut failed, b"read back", getflags(fd), 0);
    lin::check_eq(
        &mut failed,
        b"and it is the immutable bit",
        unsafe { WORD } as isize,
        FS_IMMUTABLE_FL as isize,
    );
    // The write on a descriptor opened before the flag was set: the flag is a
    // fact about the file, so the check cannot only live at the open.
    lin::check_eq(
        &mut failed,
        b"a write to an immutable file is EPERM",
        lin::write(fd, b"x"),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"ftruncate is EPERM",
        lin::ftruncate(fd, 0),
        lin::EPERM,
    );
    // `chmod` is metadata, and metadata is what the flag protects.
    lin::check_eq(
        &mut failed,
        b"chmod is EPERM",
        lin::chmod(FILE, 0o600),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"unlink is EPERM",
        lin::unlinkat(lin::AT_FDCWD, FILE, 0),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"rename is EPERM",
        lin::rename(FILE, MOVED),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"a second name is EPERM",
        lin::linkat(lin::AT_FDCWD, FILE, lin::AT_FDCWD, LINKED, 0),
        lin::EPERM,
    );
    // Reading it is *not* refused: an immutable file is one nobody may change,
    // not one nobody may open.
    let ro = lin::openat(lin::AT_FDCWD, FILE, lin::O_RDONLY, 0);
    lin::check_true(
        &mut failed,
        b"reading an immutable file is allowed",
        ro >= 0,
    );
    if ro >= 0 {
        lin::close(ro as i32);
    }
    // And the open for write is where a shell's `>` hears it.
    lin::check_eq(
        &mut failed,
        b"opening it for write is EPERM",
        lin::openat(lin::AT_FDCWD, FILE, lin::O_WRONLY, 0),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"O_TRUNC is refused too, so nothing was emptied",
        lin::openat(lin::AT_FDCWD, FILE, lin::O_RDWR | lin::O_TRUNC, 0),
        lin::EPERM,
    );
    // The way out is to clear the flag, which is what makes it a switch rather
    // than a one-way door.
    lin::check_eq(&mut failed, b"chattr -i", setflags(fd, 0), 0);
    lin::check_eq(
        &mut failed,
        b"and the write works again",
        lin::write(fd, b"y"),
        1,
    );

    // ---- APPEND: added to, never cut back ----
    lin::check_eq(&mut failed, b"chattr +a", setflags(fd, FS_APPEND_FL), 0);
    // `chattr(1)`: the file "can only be opened in append mode for writing".
    lin::check_eq(
        &mut failed,
        b"a write-open without O_APPEND is EPERM",
        lin::openat(lin::AT_FDCWD, FILE, lin::O_WRONLY, 0),
        lin::EPERM,
    );
    // The descriptor from before the flag was set still refuses: the write
    // path asks the same question, and for a flag set mid-life it is the only
    // one that can.
    lin::check_eq(
        &mut failed,
        b"and the old descriptor's write is EPERM",
        lin::write(fd, b"z"),
        lin::EPERM,
    );
    let app = lin::openat(lin::AT_FDCWD, FILE, lin::O_WRONLY | lin::O_APPEND, 0);
    lin::check_true(&mut failed, b"an append-mode open is allowed", app >= 0);
    if app >= 0 {
        let app = app as i32;
        lin::check_eq(
            &mut failed,
            b"and its write lands",
            lin::write(app, b"tail\n"),
            5,
        );
        lin::close(app);
    }
    // Read back, because "it landed at the end" is a claim about the *file*
    // and not about a descriptor's cursor: the seven bytes written before are
    // still there, and the five new ones follow them.
    let back = lin::openat(lin::AT_FDCWD, FILE, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file reads back", back >= 0);
    if back >= 0 {
        let back = back as i32;
        lin::check_eq(
            &mut failed,
            b"seven bytes before the append",
            lin::lseek(back, 7, lin::SEEK_SET),
            7,
        );
        let mut buf = [0u8; 5];
        lin::check_eq(
            &mut failed,
            b"and five at the end",
            lin::read(back, &mut buf),
            5,
        );
        lin::check_true(
            &mut failed,
            b"which are the appended ones",
            &buf == b"tail\n",
        );
        lin::close(back);
    }
    lin::check_eq(
        &mut failed,
        b"an append-only file cannot be cut back",
        lin::ftruncate(fd, 0),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"nor removed",
        lin::unlinkat(lin::AT_FDCWD, FILE, 0),
        lin::EPERM,
    );
    lin::check_eq(&mut failed, b"chattr -a", setflags(fd, 0), 0);
    lin::close(fd);
    lin::check_eq(
        &mut failed,
        b"and then it is an ordinary file again",
        lin::unlinkat(lin::AT_FDCWD, FILE, 0),
        0,
    );

    // ---- the capability, and only for the two bits it guards ----
    //
    // Run as a uid that owns its file and holds no capability, which is the
    // only configuration where the two answers differ: the rest of this
    // program runs as root, and root here holds `CAP_LINUX_IMMUTABLE`, so
    // every `chattr +i` above is one it is allowed to make.
    let uid = lin::getuid();
    lin::check_eq(&mut failed, b"the gate runs this test as root", uid, 0);
    if uid == 0 {
        // A failure to drop is a red check and not a skip: a capability rule
        // that this test quietly declined to exercise would be a rule nothing
        // pins.
        lin::check_eq(
            &mut failed,
            b"dropping to uid 1000, keeping root's saved id",
            lin::setresuid(-1, 1000, -1),
            0,
        );
        // The *effective* id, not the real one: `-1` kept the real id at 0 so
        // that `setresuid(0, 0, 0)` can put root back, which is the whole
        // reason this drops the one and not the other.
        lin::check_eq(&mut failed, b"now euid 1000", lin::geteuid(), 1000);
        lin::check_eq(
            &mut failed,
            b"with the real id still root's",
            lin::getuid(),
            0,
        );
        let mine = lin::openat(
            lin::AT_FDCWD,
            OWNED,
            lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
            0o644,
        );
        lin::check_true(&mut failed, b"a file of its own", mine >= 0);
        if mine >= 0 {
            let mine = mine as i32;
            // A flag that needs no capability, on a file it owns: allowed.
            lin::check_eq(
                &mut failed,
                b"an unprivileged owner may set NODUMP",
                setflags(mine, FS_NODUMP_FL),
                0,
            );
            // The privileged bit: refused, and *not* stored.
            lin::check_eq(
                &mut failed,
                b"but IMMUTABLE needs CAP_LINUX_IMMUTABLE",
                setflags(mine, FS_NODUMP_FL | FS_IMMUTABLE_FL),
                lin::EPERM,
            );
            lin::check_eq(&mut failed, b"and it was not set", getflags(mine), 0);
            lin::check_eq(
                &mut failed,
                b"only the NODUMP it may have",
                unsafe { WORD } as isize,
                FS_NODUMP_FL as isize,
            );
            lin::check_eq(
                &mut failed,
                b"clearing a bit it never set is not a change either",
                setflags(mine, FS_NODUMP_FL),
                0,
            );
            lin::close(mine);
        }
        lin::check_eq(
            &mut failed,
            b"and root comes back",
            lin::setresuid(0, 0, 0),
            0,
        );
        lin::check_eq(&mut failed, b"as euid 0", lin::geteuid(), 0);
        lin::unlinkat(lin::AT_FDCWD, OWNED, 0);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fsflags: panicked\n")
}
