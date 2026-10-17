//! t-fsflags - `FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS`: the inode attribute word
//! `lsattr` prints and `chattr` writes, on both machines.
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
//!     userland that reads it back); `NOATIME`'s reader lives in ext4's read
//!     path, which the gate boots no disk for - see below.
//!   * everything else - `EOPNOTSUPP`, and *nothing stored*. `chattr +c`
//!     succeeding and compressing nothing is the failure this refuses.
//!
//! The file system under all of it is the payload's own root: a ramfs, which
//! is what makes these assertions about the kernel rather than about ext4. The
//! on-disk half - that `i_flags` really round-trips through an ext4 image, and
//! that `NOATIME` is read where the access time would otherwise move
//! (`src/vfs/ext4/mod.rs`'s read path) - is host-tested
//! (`vfs::ext4::tests::the_inode_flags_are_the_ones_on_the_disk`), because the
//! gate boots no disk and the sweep payloads mount no ext4 either; the flag's
//! *storage* is claimed here on ramfs and its *honouring* stays with the
//! filesystem that reads it. That is why the migrated program says so instead
//! of claiming an atime that never moves.
//!
//! The last section is the capability: `CAP_LINUX_IMMUTABLE` is what `i` and
//! `a` are gated on, and a caller that lacks it is refused *those two bits*
//! while still being allowed the rest - which is why the check runs as a uid
//! that owns the file and has no capability, rather than as root, where every
//! answer would be the same.
//!
//! # The doors, and one new door this batch opened
//!
//! The ioctls ride `libc::syscall(SYS_ioctl, ..)` - `t-rtc`'s shape: the
//! request is an `unsigned long` word the kernel decodes, which `libc::ioctl`
//! types as `c_int` on musl, and rustix binds neither `FS_IOC_GETFLAGS` nor
//! `SETFLAGS` (clause (a), measured against its `all-apis` netlink/ioctl
//! surface). The flag words come from `linux/fs.h`'s numbers spelled here,
//! *not* from this kernel's table - the §15.11 rule a claim's number and its
//! claim have different sources, and `lin.rs` carried exactly those literals.
//!
//! The capability section needs a caller that owns its file and holds no
//! capability, and the payload has no non-root user to become: `/tmp` is
//! root's, and a process that lowers only its effective id cannot even create
//! its fixture. So the file is made while root, handed over with
//! `fchown(1000, 1000)` - the raw door, because this target's `libc` spells
//! `SYS_fchown` per machine (93 / 55, the numbers the no-std file could not
//! carry) - and the drop itself is `setresuid(-1, 1000, -1)` through
//! `libc::syscall`, keeping root's saved id so root comes back. The host
//! branch is `t-posix`'s measurement: a host Linux answers that same
//! `setresuid(-1, 1000, -1)` `EINVAL` (raw and glibc alike), so there the
//! whole dance is named and not made, while the gate keeps pinning all of it.
//!
//! Not run on the host beyond that: a host's `/tmp` is not a ramfs, its
//! filesystem answers these flags differently (ext4 stores them, tmpfs refuses
//! `IMMUTABLE` outright), and `lsattr`/`chattr` themselves are the host-side
//! instrument NOTES §4.3 carries. The pin is the two sweeps. A hosted run gets
//! through the word round-trip - tmpfs carries an attribute word and refuses
//! every bit outside it, which is the same refusal shape - and stops at the
//! first `chattr +i`, because setting `i`/`a` on a current kernel wants
//! `CAP_LINUX_IMMUTABLE` and this sandbox's unprivileged user has none
//! (measured EPERM): the immutable and append sections are named and not made
//! there, `t-posix`'s rule for a host gate rather than a host answer.

use std::ffi::c_void;

use mpos_user_tests::{Report, host_run, in_user_namespace};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{CWD, FileType, Mode, OFlags, SeekFrom, openat, seek, stat};
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe_with;
use rustix::process::{geteuid, getuid};

/// The namespace section's own file: owned by this process, opened `O_RDWR`
/// before any `fork`, and never left behind.
const NS_FILE: &str = "/tmp/t-fsflags-ns";

/// `_IOR('f', 0x01, long)`: read the attribute word.
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
/// `_IOW('f', 0x02, long)`: write it.
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
/// `_IOR('f', 0x09, long)`: a command nobody has taken, spelled exactly like
/// `GETFLAGS` so that a dispatch on the low byte alone would answer it.
const FS_IOC_UNKNOWN_READ: libc::c_ulong = 0x8008_6609;
/// The same command as a write.
const FS_IOC_UNKNOWN_WRITE: libc::c_ulong = 0x4008_6609;

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

const FILE: &str = "/tmp/t-fsflags.bin";
const MOVED: &str = "/tmp/t-fsflags.moved";
const LINKED: &str = "/tmp/t-fsflags.link";
/// A second name in a directory a uid without `CAP_LINUX_IMMUTABLE` may write
/// to, which is what the capability check at the end needs.
const OWNED: &str = "/tmp/t-fsflags-owned.bin";

/// One ioctl on the attribute word, answering `(rc, errno)` - `t-rtc`'s door
/// for the same reason: the request is an `unsigned long` word, and the answer
/// is the number the syscall itself gives.
fn fs_flags(fd: impl AsFd, request: libc::c_ulong, arg: *mut c_void) -> (i64, i32) {
    // SAFETY: every caller passes a word it owns and a request whose argument
    // shape is the `long` the family declares; the descriptor came from `open`.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd.as_fd().as_raw_fd() as libc::c_long,
            request as libc::c_long,
            arg,
        )
    };
    (
        rc,
        if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        },
    )
}

/// `GETFLAGS`: the word, or the errno the door answered with. Seeded with
/// `0xdead_beef_dead_beef` first - the high half is what a read of four bytes
/// instead of eight would get wrong, and the seed is what makes "the kernel
/// wrote the word" observable rather than assumed (`t-times`'s lesson).
fn getflags(fd: impl AsFd) -> Result<u64, i32> {
    let mut word: u64 = 0xdead_beef_dead_beef;
    let (rc, errno) = fs_flags(&fd, FS_IOC_GETFLAGS, &raw mut word as *mut c_void);
    if rc == 0 { Ok(word) } else { Err(errno) }
}

/// `SETFLAGS` with `flags`, answering the errno (or `0`).
fn setflags(fd: impl AsFd, flags: u64) -> i32 {
    let mut word = flags;
    let (_rc, errno) = fs_flags(&fd, FS_IOC_SETFLAGS, &raw mut word as *mut c_void);
    errno
}

/// `setresuid(r, e, s)` raw, answering the errno (or 0). Through
/// `libc::syscall` rather than `libc::setresuid` because the typed wrapper
/// takes `uid_t` and cannot name `-1` (`t-ns-uts`'s reason for the same door),
/// and the answer a claim pins is the syscall's.
fn setresuid(r: libc::c_long, e: libc::c_long, s: libc::c_long) -> i32 {
    // SAFETY: three scalar arguments; `-1` means "keep this one" by the ABI.
    let rc = unsafe { libc::syscall(libc::SYS_setresuid, r, e, s) };
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

/// The drop the capability section runs on: lower only the effective id, keep
/// root's real and saved ids so root can come back.
fn setresuid_keep_effective(uid: u32) -> i32 {
    setresuid(-1, uid as libc::c_long, -1)
}

/// The raw `fchown(fd, uid, gid)` - the owner change the capability section's
/// fixture needs before any drop, while the process is still root. Raw because
/// the syscall number is per-machine (93 / 55) and `libc::SYS_fchown` is the
/// maintained table that knows both.
fn fchown(fd: impl AsFd, uid: u32, gid: u32) -> i32 {
    // SAFETY: scalars; the descriptor is the caller's owned one.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_fchown,
            fd.as_fd().as_raw_fd() as libc::c_long,
            uid as libc::c_long,
            gid as libc::c_long,
        )
    };
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

fn err_is(r: &mut Report, name: &str, got: i32, want: i32) {
    r.check(
        &format!("{name} (got errno {got}, want {want})"),
        got == want,
    );
}

fn main() {
    println!("t-fsflags");
    let mut r = Report::new();

    let _ = rustix::fs::unlink(FILE);
    let fd: OwnedFd = match openat(
        CWD,
        FILE,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("a file to flag ({e:?})"), false);
            r.finish();
        }
    };
    r.check("a file to flag", true);
    r.check("and it takes bytes", write(&fd, b"hello\n") == Ok(6));

    // GETFLAGS on a fresh file: zero, and the whole `long` written - the high
    // half is what a read of four bytes instead of eight would get wrong.
    match getflags(&fd) {
        Ok(word) => {
            r.check("GETFLAGS succeeds", true);
            // The seed claim is mpos's answer (the handler zeroes the record
            // and writes the word); a host filesystem may leave the padding
            // past the low 32 bits untouched, so the hosted run reads only the
            // flag word itself.
            if host_run() {
                r.check(
                    &format!("and a fresh file has no flags at all (host word {word:#x})"),
                    word & 0xffff_ffff == 0,
                );
            } else {
                r.check("and a fresh file has no flags at all", word == 0);
            }
        }
        Err(e) => {
            r.check(&format!("GETFLAGS succeeds (errno {e})"), false);
            r.finish();
        }
    }

    // Every flag this kernel keeps, set in one go and read back whole. On a
    // host tmpfs none of the four are storable (`chattr +a` there answers
    // EPERM), so the hosted run refuses them exactly as it refuses the ones
    // nobody knows - the shape is the claim, the vocabulary is the machine's.
    let kept_rc = setflags(&fd, KEPT);
    if host_run() && kept_rc != 0 {
        println!(
            "note: the host's tmpfs stores none of NODUMP/NOATIME/SYNC/DIRSYNC (errno {kept_rc}); the storage claims below take the refusal branch"
        );
    } else {
        err_is(&mut r, "SETFLAGS takes four flags", kept_rc, 0);
        match getflags(&fd) {
            Ok(word) => {
                r.check("and reads them back", true);
                r.check("all four, and nothing else", word == KEPT);
            }
            Err(e) => r.check(&format!("and reads them back (errno {e})"), false),
        }
    }

    // A bit this kernel cannot honour is refused, and *nothing is stored*: the
    // refusal is the answer, and the word is left exactly as it was. On a host
    // tmpfs every bit is refused (the four above included), so "unchanged"
    // there means unchanged-from-empty; on mpos it means the KEPT word the
    // previous section stored survived the refusal untouched.
    err_is(
        &mut r,
        "a compression flag answers EOPNOTSUPP",
        setflags(&fd, KEPT | FS_COMPR_FL),
        libc::EOPNOTSUPP,
    );
    match getflags(&fd) {
        Ok(word) => {
            r.check("and nothing was stored", true);
            // The low 32 bits are the flag word on both machines; a host tmpfs
            // leaves the padding past them whatever it likes (measured: its
            // GETFLAGS answers `0xdeadbeef_00000000` against our seed, so the
            // high half is nobody's promise there). On mpos the whole long is
            // written and KEPT is what must still be there.
            let unchanged = if host_run() {
                word & 0xffff_ffff == 0
            } else {
                word == KEPT
            };
            r.check("the word is unchanged", unchanged);
        }
        Err(e) => r.check(&format!("and nothing was stored (errno {e})"), false),
    }
    // Including a bit no vocabulary here names: the check is on the word, not
    // on a list of the flags this kernel happens to know.
    err_is(
        &mut r,
        "a bit nobody has heard of is refused too",
        setflags(&fd, KEPT | FS_UNKNOWN_FL),
        libc::EOPNOTSUPP,
    );
    err_is(
        &mut r,
        "an encryption flag is refused with the rest",
        setflags(&fd, FS_ENCRYPT_FL),
        libc::EOPNOTSUPP,
    );
    // And back to nothing, so the next section starts clean.
    err_is(&mut r, "clearing them works", setflags(&fd, 0), 0);

    // NOATIME's *reader* is named, not claimed: the flag is stored above and
    // ext4's read path honours it (host test `the_inode_flags_are_the_ones_on_
    // the_disk` plus its own offset coverage); ramfs keeps one clock and the
    // sweep mounts no ext4, so an atime claim here would pin a filesystem that
    // is not the one that reads the flag.
    r.check(
        "NOATIME is stored here and read by ext4's read path (the atime claim lives with the host test)",
        true,
    );

    // A request in the family that nothing answers is ENOTTY, in both
    // directions - the family byte is not a permission slip.
    err_is(
        &mut r,
        "an unknown read request is ENOTTY",
        fs_flags(&fd, FS_IOC_UNKNOWN_READ, core::ptr::null_mut()).1,
        libc::ENOTTY,
    );
    err_is(
        &mut r,
        "an unknown write request is ENOTTY",
        fs_flags(&fd, FS_IOC_UNKNOWN_WRITE, core::ptr::null_mut()).1,
        libc::ENOTTY,
    );

    // A filesystem with no attribute word at all answers ENOTTY rather than
    // "no flags": a pipe has no inode to carry them, and Linux's answer there
    // is the same one it gives a filesystem with no `fileattr` operations.
    match pipe_with(rustix::pipe::PipeFlags::empty()) {
        Ok((reader, writer)) => {
            let _ = writer;
            err_is(
                &mut r,
                "a pipe has no attribute word",
                getflags(&reader).err().unwrap_or(0),
                libc::ENOTTY,
            );
            err_is(
                &mut r,
                "and cannot be told it has one",
                setflags(&reader, FS_NODUMP_FL),
                libc::ENOTTY,
            );
        }
        Err(e) => r.check(&format!("a pipe to ask about ({e:?})"), false),
    }

    // ---- IMMUTABLE: the flag that means "not this file" ----
    // The host branch is Linux's *privilege*, not its semantics: a current
    // kernel sets `i`/`a` only for a caller holding `CAP_LINUX_IMMUTABLE`, and
    // this sandbox's unprivileged user has none (measured: `chattr +i` answers
    // EPERM on tmpfs), so every claim behind a successful set would measure
    // the host's capabilities rather than either kernel's rules. The hosted
    // run names the refusal it got and stops; the gate - where the sweep runs
    // as root, which holds the cap - pins all of it. `t-posix`'s rule for the
    // same class of host gate. On a host that *is* privileged (a root run with
    // `MPOS_ON_HOST=1`) the answer comes from the filesystem below instead -
    // tmpfs refuses IMMUTABLE outright, which is its own honest shape - so the
    // dance proceeds only when the flag actually landed.
    let rc = setflags(&fd, FS_IMMUTABLE_FL);
    if rc != 0 && host_run() {
        println!(
            "note: the host refuses chattr +i (errno {rc}); the immutable and append sections are named and not made here"
        );
        drop(fd);
        let _ = rustix::fs::unlink(FILE);
        r.finish();
    }
    err_is(&mut r, "chattr +i", rc, 0);
    match getflags(&fd) {
        Ok(word) => {
            r.check("read back", true);
            r.check("and it is the immutable bit", word == FS_IMMUTABLE_FL);
        }
        Err(e) => r.check(&format!("read back (errno {e})"), false),
    }
    // The write on a descriptor opened before the flag was set: the flag is a
    // fact about the file, so the check cannot only live at the open.
    match write(&fd, b"x") {
        Err(Errno::PERM) => r.check("a write to an immutable file is EPERM", true),
        other => r.check(
            &format!("a write to an immutable file is EPERM (got {other:?})"),
            false,
        ),
    }
    err_is(
        &mut r,
        "ftruncate is EPERM",
        rustix::fs::ftruncate(&fd, 0)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    // `chmod` is metadata, and metadata is what the flag protects.
    err_is(
        &mut r,
        "chmod is EPERM",
        rustix::fs::chmod(FILE, Mode::from_raw_mode(0o600))
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(
        &mut r,
        "unlink is EPERM",
        rustix::fs::unlink(FILE)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(
        &mut r,
        "rename is EPERM",
        rustix::fs::rename(FILE, MOVED)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(
        &mut r,
        "a second name is EPERM",
        rustix::fs::linkat(CWD, FILE, CWD, LINKED, rustix::fs::AtFlags::empty())
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    // Reading it is *not* refused: an immutable file is one nobody may change,
    // not one nobody may open.
    let ro = openat(CWD, FILE, OFlags::RDONLY, Mode::empty());
    r.check("reading an immutable file is allowed", ro.is_ok());
    drop(ro);
    // And the open for write is where a shell's `>` hears it.
    err_is(
        &mut r,
        "opening it for write is EPERM",
        openat(CWD, FILE, OFlags::WRONLY, Mode::empty())
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(
        &mut r,
        "O_TRUNC is refused too, so nothing was emptied",
        openat(CWD, FILE, OFlags::RDWR | OFlags::TRUNC, Mode::empty())
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    // The way out is to clear the flag, which is what makes it a switch rather
    // than a one-way door.
    err_is(&mut r, "chattr -i", setflags(&fd, 0), 0);
    r.check("and the write works again", write(&fd, b"y") == Ok(1));

    // ---- APPEND: added to, never cut back ----
    err_is(&mut r, "chattr +a", setflags(&fd, FS_APPEND_FL), 0);
    // `chattr(1)`: the file "can only be opened in append mode for writing".
    err_is(
        &mut r,
        "a write-open without O_APPEND is EPERM",
        openat(CWD, FILE, OFlags::WRONLY, Mode::empty())
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    // The descriptor from before the flag was set still refuses: the write
    // path asks the same question, and for a flag set mid-life it is the only
    // one that can.
    match write(&fd, b"z") {
        Err(Errno::PERM) => r.check("and the old descriptor's write is EPERM", true),
        other => r.check(
            &format!("and the old descriptor's write is EPERM (got {other:?})"),
            false,
        ),
    }
    let app = openat(CWD, FILE, OFlags::WRONLY | OFlags::APPEND, Mode::empty());
    r.check("an append-mode open is allowed", app.is_ok());
    if let Ok(app) = app {
        r.check("and its write lands", write(&app, b"tail\n") == Ok(5));
    }
    // Read back, because "it landed at the end" is a claim about the *file*
    // and not about a descriptor's cursor. The no-std version counted seven
    // bytes before the append - `hello\n` plus the one byte the IMMUTABLE
    // section wrote when it came back; the refused writes add nothing, so the
    // ledger here is the same six plus one. The five new ones follow them.
    let back = openat(CWD, FILE, OFlags::RDONLY, Mode::empty());
    r.check("the file reads back", back.is_ok());
    if let Ok(back) = back {
        r.check(
            "seven bytes before the append",
            seek(&back, SeekFrom::Start(7)) == Ok(7),
        );
        let mut buf = [0u8; 5];
        match read(&back, &mut buf) {
            Ok(5) => r.check("and five at the end", true),
            n => r.check(&format!("and five at the end (got {n:?})"), false),
        }
        r.check("which are the appended ones", &buf == b"tail\n");
    }
    err_is(
        &mut r,
        "an append-only file cannot be cut back",
        rustix::fs::ftruncate(&fd, 0)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(
        &mut r,
        "nor removed",
        rustix::fs::unlink(FILE)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        libc::EPERM,
    );
    err_is(&mut r, "chattr -a", setflags(&fd, 0), 0);
    drop(fd);
    err_is(
        &mut r,
        "and then it is an ordinary file again",
        rustix::fs::unlink(FILE)
            .err()
            .map_or(0, |e| e.raw_os_error()),
        0,
    );

    // ---- the capability, and only for the two bits it guards ----
    //
    // Run as a uid that owns its file and holds no capability, which is the
    // only configuration where the two answers differ: the rest of this
    // program runs as root, and root here holds `CAP_LINUX_IMMUTABLE`, so
    // every `chattr +i` above is one it is allowed to make.
    r.check("the gate runs this test as root", getuid().as_raw() == 0);
    if getuid().as_raw() == 0 {
        // The fixture is made while root and handed to uid 1000 before the
        // drop: `/tmp` is root's, so a lowered caller could not create it -
        // the step the no-std version did not need because its `/tmp` story
        // was the same and it dropped after creating.
        let mine: OwnedFd = match openat(
            CWD,
            OWNED,
            OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                r.check(&format!("a file of its own ({e:?})"), false);
                r.finish();
            }
        };
        err_is(
            &mut r,
            "and root hands it to uid 1000",
            fchown(&mine, 1000, 1000),
            0,
        );
        let st = stat(OWNED).ok();
        r.check(
            "so its owner is the uid that will hold it",
            st.map(|st| st.st_uid == 1000 && FileType::from_raw_mode(st.st_mode).is_file())
                .unwrap_or(false),
        );
        // A failure to drop is a red check and not a skip on the payload: a
        // capability rule that this test quietly declined to exercise would
        // be a rule nothing pins. On the host the dance is named and not
        // made - `t-posix` measured that a host answers `setresuid(-1, 1, -1)`
        // `EINVAL` (raw and glibc alike), so the gate keeps pinning and the
        // host run says so.
        let dropped = setresuid_keep_effective(1000);
        if host_run() && dropped != 0 {
            println!(
                "note: the host refuses the effective-id drop (errno {dropped}); the sudo dance is named and not made here"
            );
        } else {
            err_is(
                &mut r,
                "dropping to uid 1000, keeping root's saved id",
                dropped,
                0,
            );
            // The *effective* id, not the real one: `-1` kept the real id at 0 so
            // that `setresuid(0, 0, 0)` can put root back, which is the whole
            // reason this drops the one and not the other.
            r.check("now euid 1000", geteuid().as_raw() == 1000);
            r.check("with the real id still root's", getuid().as_raw() == 0);
            // A flag that needs no capability, on a file it owns: allowed.
            err_is(
                &mut r,
                "an unprivileged owner may set NODUMP",
                setflags(&mine, FS_NODUMP_FL),
                0,
            );
            // The privileged bit: refused, and *not* stored.
            err_is(
                &mut r,
                "but IMMUTABLE needs CAP_LINUX_IMMUTABLE",
                setflags(&mine, FS_NODUMP_FL | FS_IMMUTABLE_FL),
                libc::EPERM,
            );
            match getflags(&mine) {
                Ok(word) => {
                    r.check("and it was not set", true);
                    r.check("only the NODUMP it may have", word == FS_NODUMP_FL);
                }
                Err(e) => r.check(&format!("and it was not set (errno {e})"), false),
            }
            err_is(
                &mut r,
                "clearing a bit it never set is not a change either",
                setflags(&mine, FS_NODUMP_FL),
                0,
            );
            drop(mine);
            err_is(&mut r, "and root comes back", setresuid(0, 0, 0), 0);
            r.check("as euid 0", geteuid().as_raw() == 0);
        }
        let _ = rustix::fs::unlink(OWNED);
    }

    // ---- the namespace the capability belongs to ---------------------------
    //
    // `fileattr_set` asks `capable(CAP_LINUX_IMMUTABLE)` - the **initial** user
    // namespace - so a caller that has entered one of its own is refused the
    // same way the unprivileged uid above was, even though entering it handed
    // that caller the whole capability set *in it* (the rung's own measured
    // grant, §6.3). Measured on a current host first
    // (`.workbuddy/probe_capable_scope.sh`). The descriptor is this process's
    // own `O_RDWR` one, inherited across the `fork`: the ioctl needs a writable
    // descriptor, and the namespace's ids are unmapped, so a file it could open
    // by name is one nobody wrote for this test. `NODUMP` beside `IMMUTABLE` is
    // the control - the ioctl itself is served inside the namespace, and only
    // the privileged bit is refused.
    let _ = rustix::fs::unlink(NS_FILE);
    let ns_fd = match openat(
        CWD,
        NS_FILE,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("a file for the namespace section ({e:?})"), false);
            r.finish();
        }
    };
    let raw = ns_fd.as_raw_fd();
    let refused = in_user_namespace(move || {
        // SAFETY: `raw` is this process's own descriptor, open across the call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        setflags(borrowed, FS_IMMUTABLE_FL)
    });
    let served = in_user_namespace(move || {
        // SAFETY: as above; the second child inherits the same descriptor.
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        setflags(borrowed, FS_NODUMP_FL)
    });
    r.check(
        &format!("IMMUTABLE inside a user namespace is EPERM (got {refused:?})"),
        refused == Some(libc::EPERM),
    );
    r.check(
        &format!("but a bit needing no capability is served there (got {served:?})"),
        served == Some(0),
    );
    drop(ns_fd);
    let _ = rustix::fs::unlink(NS_FILE);

    r.finish();
}
