//! t-mknod - the `S_IFMT` field of `mknod(2)`/`mknodat(2)`, and the answers
//! around it.
//!
//! `mknodat` is the call an initramfs pre-seeds its `/dev` with (`[ -c /dev/null
//! ] || mknod /dev/null c 1 3`) and the call a package makes when it needs a
//! device node of its own. What it *makes* is decided by the type field of the
//! mode, and this kernel answered four of the field's eight values the wrong
//! way: `S_IFREG` and a plain permission word were `EINVAL` (Linux makes a
//! regular file, and with mode `0` a file with mode `0000`), `S_IFDIR` was
//! `EINVAL` (Linux is `EPERM` - it is the *privilege* word, not the argument
//! word), `S_IFSOCK` was `EINVAL` (Linux makes a socket node, unprivileged),
//! and `S_IFIFO` made a **regular file**. Measured on a current host through
//! `syscall(2)` (`.workbuddy/probe_mknod.c`, Linux 7.2.9) before any of it was
//! written, and the whole vocabulary is claimed here.
//!
//! Two answers *beside* the type field, both measured the same way:
//!
//!   * **a name that already exists is `EEXIST`** - over a node of the same
//!     type, over one of a different type, over a *dangling link* (the question
//!     is about the name, so it is asked without following), and for the legacy
//!     `mknod` spelling as well as `mknodat`. This kernel answered `0` for both
//!     spellings, on the reading that "the caller wanted the node, not the
//!     call" - which is a success where Linux refuses. The legacy spelling has
//!     no number on aarch64 (`nr_aarch64.rs`), so the claim here is the
//!     `mknodat` one, made on both machines; the sibling handler carries the
//!     same line.
//!   * **the order the call asks its questions in**, which the same probe
//!     measured from both sides: the *type*'s own refusals come **before the
//!     name** (`S_IFDIR` is `EPERM` and `S_IFLNK` `EINVAL` even with an *empty*
//!     name, which would be `ENOENT` had the name been asked first), then the
//!     name, then the name's existence (`EEXIST`), then the directory's own
//!     writability, and only then the kind - which is why `S_IFIFO` over an
//!     existing name is `EEXIST`: the type passes the *first* question now
//!     (it is creatable), so the name's existence is what answers, and why
//!     `t-at-empty-name` pins the other half of the order with this very type.
//!   * **the umask masks the permission half**: `mknodat(…|0666)` under a `027`
//!     mask leaves `0640`, so the mode a caller reads back is the mode it asked
//!     for minus the mask - `t-umask`'s subject, in this call's shape.
//!
//! # The FIFO, and where its rendezvous is pinned
//!
//! `S_IFIFO` makes a **FIFO** here now, as it does on Linux - the claim below
//! reads the node (`stat` reports `S_IFIFO`). Until this pass it made a regular
//! file, which read end-of-file at once: a *rendezvous* turned into a file, a
//! shape the caller cannot tell from an empty channel. The rest of the FIFO's
//! contract - the `open`-time park, `ENXIO`, `EAGAIN`, `EPIPE`, the buffer
//! discarded when both counts reach zero, and `S_IFIFO` at an *opened*
//! description - is the node's own subject and is pinned in `t-fifo`; the whole
//! measured shape is `docs/uapi/` §5.2 (`.workbuddy/probe_fifo.c`).
//!
//! # What this kernel does not write on the node, and what it does not check
//!
//! A device node made here carries **no device number**: `st_rdev` is the field
//! `mknod`'s `dev` argument fills, and this kernel ignores the argument. That is
//! named in `docs/uapi/` §5.2 rather than claimed, because what it can honestly
//! do with the number is nothing yet - it has no table from a device number to
//! a driver (its `/dev/null` *is* the null device; a node made here is a
//! placeholder the `devtmpfs` mount later covers), so writing it would make
//! `stat` true and the device still absent. The claims below therefore read the
//! *type*, and the section prints the number that came back.
//!
//! And it asks **no capability** for one: Linux refuses a device node to a
//! caller without `CAP_MKNOD` (`EPERM`, measured on the current host, and
//! measured in its place in the order - after the name, after `EEXIST`, after
//! the directory's own `EACCES`), where this kernel builds the node for any
//! caller. That is named in `docs/uapi/` §5.2 rather than claimed here, and the
//! reason it is not a one-line gate is recorded with it: a `setuid` here does
//! not re-derive the capability sets, so a dropped-privilege process would
//! still hold `CAP_MKNOD` and the gate would refuse nobody. The claim below
//! therefore reads the node and prints what Linux asks for beside it.
//!
//! # Why the raw syscall
//!
//! Every `mknodat` here goes through `libc::syscall(libc::SYS_mknodat, …)`:
//! the **mode word is the subject** (a plain permission word and `0` are two of
//! the claims, and `rustix::fs::mknodat` takes a `FileType` - a plain enum with
//! no spelling for "no type at all"), and the errno is read the way a raw door
//! leaves it on this target (`-1` with the number beside it). The reads of what
//! was made are `rustix`'s, because those are not the subject.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, FileType, Mode, statat, unlinkat};
use rustix::process::{geteuid, umask};

/// The name every claim starts from, and the two fixtures beside it.
const NODE: &str = "/tmp/t-mknod-node";
const PLAIN: &str = "/tmp/t-mknod-plain";
const NONE: &str = "/tmp/t-mknod-mode0";
const SOCK: &str = "/tmp/t-mknod-sock";
const CHR: &str = "/tmp/t-mknod-chr";
const MISSING: &str = "/tmp/t-mknod-no-dir/node";

const AT_FDCWD: i32 = -100;
/// `S_IFMT`'s eight values, and the permission halves the claims use.
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFSOCK: u32 = 0o140000;

/// One `mknodat` through the raw door, answering `(result, errno)` - the shape
/// this target's `syscall` leaves: `-1` with the refusal beside it.
fn mknodat(mode: u32, dev: u64, path: &str) -> (isize, i32) {
    let c = std::ffi::CString::new(path).unwrap();
    let r = unsafe { libc::syscall(libc::SYS_mknodat, AT_FDCWD, c.as_ptr(), mode, dev) } as isize;
    let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    (r, if r < 0 { e } else { 0 })
}

/// What a name is now, or `None` when nothing answers to it.
fn kind_of(path: &str) -> Option<FileType> {
    statat(CWD, path, AtFlags::empty())
        .ok()
        .map(|st| FileType::from_raw_mode(st.st_mode as _))
}

fn mode_of(path: &str) -> Option<u32> {
    statat(CWD, path, AtFlags::empty())
        .ok()
        .map(|st| st.st_mode as u32 & 0o7777)
}

/// Remove every fixture, so each claim starts from a free name whatever the
/// last run left behind.
fn clear() {
    for path in [NODE, PLAIN, NONE, SOCK, CHR] {
        let _ = unlinkat(CWD, path, AtFlags::empty());
    }
}

fn main() {
    println!("t-mknod");
    let mut r = Report::new();
    clear();

    // ---- the type field's own vocabulary -----------------------------------
    let (made, e) = mknodat(S_IFREG | 0o644, 0, NODE);
    r.check(
        &format!("mknodat makes a regular file from S_IFREG (got {made}, errno {e})"),
        made == 0,
    );
    r.check(
        "and stat calls it a regular file",
        kind_of(NODE) == Some(FileType::RegularFile),
    );
    r.check(
        "with the permission half it was made with",
        mode_of(NODE) == Some(0o644),
    );

    let (made, e) = mknodat(0o640, 0, PLAIN);
    r.check(
        &format!("a mode with no type at all is a regular file too (got {made}, errno {e})"),
        made == 0 && kind_of(PLAIN) == Some(FileType::RegularFile),
    );
    r.check(
        "and its permission half is what was asked",
        mode_of(PLAIN) == Some(0o640),
    );

    let (made, e) = mknodat(0, 0, NONE);
    r.check(
        &format!("mode 0 makes a file whose mode is 0000 (got {made}, errno {e})"),
        made == 0 && mode_of(NONE) == Some(0),
    );
    // The same reporting path from the other door that can reach it: a stored
    // `0000` is *zero*, not "nobody said". This claim is here rather than in a
    // chmod test because the two are one question - and because this program's
    // first run is what caught it: the kernel stored the mode and enforced it
    // (`may_as` asks `keeps_modes`), while the stat writers substituted the
    // kind's default for a stored zero, so the file `mknod` was asked to make
    // with mode 0 read back as `0644`.
    let zeroed = rustix::fs::chmod(NODE, Mode::empty()).is_ok();
    r.check(
        "and a chmod to 000 reads back as 0000 through the same path",
        zeroed && mode_of(NODE) == Some(0),
    );
    let _ = rustix::fs::chmod(NODE, Mode::from_bits_retain(0o644));

    // The mask applies to the permission half, which is `t-umask`'s subject
    // reached through another door.
    let was = umask(Mode::from_bits_retain(0o027));
    let (made, _) = mknodat(S_IFREG | 0o666, 0, "/tmp/t-mknod-masked");
    let masked = mode_of("/tmp/t-mknod-masked");
    umask(was);
    let _ = unlinkat(CWD, "/tmp/t-mknod-masked", AtFlags::empty());
    r.check(
        "the umask masks the permission half (0666 under 027 leaves 0640)",
        made == 0 && masked == Some(0o640),
    );

    let (made, e) = mknodat(S_IFSOCK | 0o644, 0, SOCK);
    r.check(
        &format!("mknodat makes a socket node from S_IFSOCK (got {made}, errno {e})"),
        made == 0 && kind_of(SOCK) == Some(FileType::Socket),
    );

    // ---- the two refusals Linux makes for a *type* -------------------------
    let (made, e) = mknodat(S_IFDIR | 0o755, 0, "/tmp/t-mknod-dir");
    r.check(
        &format!("S_IFDIR is EPERM, not EINVAL (got errno {e})"),
        made == -1 && e == libc::EPERM,
    );
    r.check(
        "and no directory was made",
        kind_of("/tmp/t-mknod-dir").is_none(),
    );
    let (made, e) = mknodat(S_IFLNK | 0o777, 0, "/tmp/t-mknod-link");
    r.check(
        &format!("S_IFLNK is EINVAL (got errno {e})"),
        made == -1 && e == libc::EINVAL,
    );

    // ---- the order the call asks its questions in --------------------------
    //
    // Both halves of it, from the side that makes it visible. An **empty**
    // name would be `ENOENT` had the name been asked first (that is
    // `t-at-empty-name`'s claim, made with `S_IFIFO` - a type this check
    // *passes*); and a name that is already there would be `EEXIST` had the
    // existence check come first.
    let (made, e) = mknodat(S_IFDIR | 0o755, 0, "");
    r.check(
        &format!("the type is asked before the name: an empty name is still EPERM (got errno {e})"),
        made == -1 && e == libc::EPERM,
    );
    let (made, e) = mknodat(S_IFDIR | 0o755, 0, NODE);
    r.check(
        &format!("and before the name's existence: over one that is there, still EPERM (got {e})"),
        made == -1 && e == libc::EPERM,
    );

    // ---- the name that is already there ------------------------------------
    let (made, e) = mknodat(S_IFREG | 0o600, 0, NODE);
    r.check(
        &format!("a name that exists is EEXIST with the same type (got errno {e})"),
        made == -1 && e == libc::EEXIST,
    );
    r.check(
        "and it does not replace what was there",
        mode_of(NODE) == Some(0o644),
    );
    let (made, e) = mknodat(S_IFIFO | 0o600, 0, NODE);
    r.check(
        &format!("as it is with a different type asked for (got errno {e})"),
        made == -1 && e == libc::EEXIST,
    );
    // A dangling link is a name that exists: the question is about the *name*.
    let _ = unlinkat(CWD, "/tmp/t-mknod-dangling", AtFlags::empty());
    let target = std::ffi::CString::new("/nowhere").unwrap();
    let link = std::ffi::CString::new("/tmp/t-mknod-dangling").unwrap();
    if unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) } == 0 {
        let (made, e) = mknodat(S_IFREG | 0o600, 0, "/tmp/t-mknod-dangling");
        r.check(
            &format!("and a dangling link is a name that exists too (got errno {e})"),
            made == -1 && e == libc::EEXIST,
        );
        let _ = unlinkat(CWD, "/tmp/t-mknod-dangling", AtFlags::empty());
    } else {
        r.check("and a dangling link is a name that exists too", false);
    }

    // ---- the FIFO: a node, on both machines now ----------------------------
    //
    // `mknodat(S_IFIFO)` makes a FIFO here and on the host: `stat` reports
    // `S_IFIFO` at the name, and `open(2)` on it runs a rendezvous. What the
    // *call* promises is only the node - the rendezvous is the node's own
    // subject, and `t-fifo` is where it is pinned (the whole measured contract
    // is `docs/uapi/` §5.2). The counterfactual is exact: before this pass the
    // call made a **regular file**, so the timing below read `S_IFREG` under
    // the name - a shape no caller can tell from an empty channel.
    let (made, e) = mknodat(S_IFIFO | 0o644, 0, "/tmp/t-mknod-fifo");
    r.check(
        &format!("S_IFIFO makes a FIFO node (got {made}, errno {e})"),
        made == 0 && kind_of("/tmp/t-mknod-fifo") == Some(FileType::Fifo),
    );
    r.check(
        "and the name's type is S_IFIFO to stat",
        statat(CWD, "/tmp/t-mknod-fifo", AtFlags::empty())
            .ok()
            .is_some_and(|st| st.st_mode as u32 & 0o170000 == S_IFIFO),
    );
    let _ = unlinkat(CWD, "/tmp/t-mknod-fifo", AtFlags::empty());
    r.check(
        "and no FIFO node answers to the name once it is cleaned up",
        kind_of("/tmp/t-mknod-fifo").is_none(),
    );

    // ---- a name whose directory is not there -------------------------------
    let (made, e) = mknodat(S_IFREG | 0o644, 0, MISSING);
    r.check(
        &format!("a directory that is not there is ENOENT (got errno {e})"),
        made == -1 && e == libc::ENOENT,
    );

    // ---- a device node, and the number it does not carry -------------------

    if geteuid().is_root() {
        let (made, e) = mknodat(S_IFCHR | 0o666, 0x0103, CHR);

        r.check(
            &format!("a character node is made (got {made}, errno {e})"),
            made == 0 && kind_of(CHR) == Some(FileType::CharacterDevice),
        );

        let number = statat(CWD, CHR, AtFlags::empty()).ok().map(|st| st.st_rdev);
        println!(
            "     the character node's st_rdev: {number:?} - Linux reports the dev argument \
             (1:3); this kernel does not write it, docs/uapi/ §5.2"
        );
        println!(
            "     note: Linux asks CAP_MKNOD for this node (EPERM unprivileged, measured); this \
             kernel checks no capability, and docs/uapi/ §5.2 names that too"
        );
    } else {
        println!(
            "     not root: the device-node claim is named, not made - Linux's answer for an \
             unprivileged caller is EPERM, which this kernel does not check yet (§5.2)"
        );
    }

    clear();
    r.finish()
}
