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
//! # The number the node carries
//!
//! `st_rdev` is the field `mknod`'s `dev` argument fills, and this kernel ignored
//! the argument: every device node it made read `0` back out of it, whichever
//! number the caller named. What the field holds is not the argument's word but
//! the **(major, minor)** pair that word encodes, re-encoded the way Linux lays
//! it out - which is the same word for a pair whose minor fits in eight bits and
//! a *different* one for a pair whose minor does not, so the row worth claiming
//! is a wide minor. Measured before it was written
//! (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11, a tmpfs and an ext4 agreeing
//! on every row): `makedev(136, 300)` reports `0x0010882c` and decodes back to
//! `136:300`; a *block* node keeps its own number (`makedev(254, 70000)` →
//! `0x1110fe70`) **and** reports a block node, which this kernel did not - every
//! device node it made answered as a character device; a `dev` wider than 32
//! bits contributes nothing, only its low word being the number; and the
//! whiteout's `0` reports `0:0`. Each of those rows is claimed below.
//!
//! What is *not* claimed here is opening one, because that errno belongs to the
//! machine rather than to the rule: on a `nodev` mount Linux answers `EACCES`
//! for **every** device node before it consults the number (this host's `/tmp`
//! is one), and off one the number is what answers - the real `/dev/null` for
//! `makedev(1, 3)`, `ENXIO` for a number nobody owns. No driver here answers to
//! any number `mknod` can be given, so this kernel's own answer - `ENXIO`, from
//! the node rather than from a mount - is pinned where it is made, in
//! `vfs::ramfs`'s tests.
//!
//! # The one privilege question this call asks
//!
//! `mknod` asks for exactly one capability, and asks it for exactly one kind of
//! node: **`CAP_MKNOD` for a device node**. Measured on a current host through
//! `syscall(2)` as an **unprivileged** caller (`.workbuddy/probe_mknod_cap.c`,
//! Linux 7.2.9): `S_IFCHR` and `S_IFBLK` are `EPERM`, and every other creatable
//! type is served to a caller holding nothing - `S_IFREG`, `S_IFIFO` and
//! `S_IFSOCK` make their node. One device node is excepted and it is the
//! character node whose `dev` is **`0`**, which is how overlayfs spells a
//! *whiteout*: `makedev(0, 1)`, `makedev(1, 0)` and a *block* node with the same
//! `0` were all `EPERM` beside it. In the order above the question comes last: a
//! name that already exists is still `EEXIST`, a directory the caller may not
//! write is still `EACCES`, and a path that is not there is still `ENOENT`.
//!
//! Root holds the capability, so the refusal is only visible from *without* it,
//! and the window below is `t-caps`'s own: lower the **effective** id to an
//! ordinary user while the saved one stays root's, which clears the effective
//! set and leaves the permitted one full (the measured `cap_emulate_setxuid`
//! rule), make the claims, then raise the effective id back through the saved
//! one. A hand run on the host is *already* without the capability - it never
//! held one - so the same claims are made there as well, which is why this
//! section has no host-run hole in it.
//!
//! # Why the raw syscall
//!
//! Every `mknodat` here goes through `libc::syscall(libc::SYS_mknodat, …)`:
//! the **mode word is the subject** (a plain permission word and `0` are two of
//! the claims, and `rustix::fs::mknodat` takes a `FileType` - a plain enum with
//! no spelling for "no type at all"), and the errno is read the way a raw door
//! leaves it on this target (`-1` with the number beside it). The reads of what
//! was made are `rustix`'s, because those are not the subject.

use mpos_user_tests::{Report, in_user_namespace};
use rustix::fs::{AtFlags, CWD, FileType, Mode, statat, unlinkat};
use rustix::process::{geteuid, umask};

/// The name every claim starts from, and the two fixtures beside it.
const NODE: &str = "/tmp/t-mknod-node";
const PLAIN: &str = "/tmp/t-mknod-plain";
const NONE: &str = "/tmp/t-mknod-mode0";
const SOCK: &str = "/tmp/t-mknod-sock";
const CHR: &str = "/tmp/t-mknod-chr";
/// The number's shadow: the same node made with a minor above eight bits, and
/// one asked for from `S_IFBLK` - the two rows the encoding is not the identity
/// on, and the reason a device node that keeps the argument's *word* instead of
/// the pair names another device.
const CHR_WIDE: &str = "/tmp/t-mknod-chr-wide";
const BLK: &str = "/tmp/t-mknod-blk";
/// The node the capability section makes *after* it has the capability back.
const CHR_AGAIN: &str = "/tmp/t-mknod-chr-again";
const MISSING: &str = "/tmp/t-mknod-no-dir/node";

/// The capability section's writable directory (`0777`, so the lowered caller
/// may still make a name in it) and the one it may not write (`0555`) - the
/// pair whose answers separate a missing capability's `EPERM` from a
/// directory's own `EACCES`.
const CAP_DIR: &str = "/tmp/t-mknod-cap";
const RO_DIR: &str = "/tmp/t-mknod-ro";

/// A name inside [`CAP_DIR`], where the lowered caller's claims live.
fn cap_dir(name: &str) -> String {
    format!("{CAP_DIR}/{name}")
}

const AT_FDCWD: i32 = -100;
/// `S_IFMT`'s eight values, and the permission halves the claims use.
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFBLK: u32 = 0o060000;
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

/// `setresuid(r, e, s)` raw, answering the errno (or `0`).
///
/// Through `libc::syscall` rather than the typed wrapper, which takes a `uid_t`
/// and so cannot name `-1` ("keep this one") - the same reason `t-fsflags` uses
/// this door. Only the effective id moves: the real and saved ones stay root's,
/// which is what makes the drop reversible from inside the process.
fn setresuid(r: libc::c_long, e: libc::c_long, s: libc::c_long) -> i32 {
    let rc = unsafe { libc::syscall(libc::SYS_setresuid, r, e, s) };
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
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

/// What `st_rdev` reports for a name, or `None` when nothing answers to it.
fn rdev_of(path: &str) -> Option<u64> {
    statat(CWD, path, AtFlags::empty())
        .ok()
        .map(|st| st.st_rdev)
}

/// The word `st_rdev` holds for a pair - Linux's `new_encode_dev`: the minor's
/// low eight bits, twelve bits of major above them, and the minor's high twelve
/// bits at the top of the word.
///
/// This program hands `mknodat` the word rather than a pair because that is what
/// the uapi does (`dev` is `unsigned`), and reads the same word back - which is
/// the round trip the claims are about. The rows themselves are measured on a
/// host (`.workbuddy/probe_mknod_rdev.c`) rather than derived from this sum.
fn encode(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12)) as u64
}

/// Remove every fixture, so each claim starts from a free name whatever the
/// last run left behind. The capability section's directory has to be emptied
/// before it can go, which is why the names inside it are listed too.
fn clear() {
    for path in [NODE, PLAIN, NONE, SOCK, CHR, CHR_WIDE, BLK, CHR_AGAIN] {
        let _ = unlinkat(CWD, path, AtFlags::empty());
    }
    for name in [
        "chr", "blk", "whiteout", "blk0", "fifo", "reg", "sock", "exists", "ns-chr", "ns-reg",
    ] {
        let _ = unlinkat(CWD, cap_dir(name).as_str(), AtFlags::empty());
    }
    let _ = rustix::fs::rmdir(CAP_DIR);
    let _ = rustix::fs::rmdir(RO_DIR);
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

    // ---- a device node: what the caller holds decides -----------------------
    //
    // The last question the call asks, and the only one whose answer is the
    // *caller's* rather than the call's own shape. The whole measured contract
    // is in the header; what this section does is stage both sides of it - a
    // caller that holds `CAP_MKNOD` and a caller that does not - and then walk
    // the order, because the capability is asked *after* every refusal above.
    let was_root = geteuid().is_root();

    // The two directories the window needs, made while the caller is still
    // root: one the lowered caller may write in, one it may not. A `mkdir`
    // arrives masked by the umask, so the mode is set again explicitly - the
    // pair is the whole point of the fixture.
    let staged = rustix::fs::mkdir(CAP_DIR, Mode::from_bits_retain(0o777)).is_ok()
        && rustix::fs::chmod(CAP_DIR, Mode::from_bits_retain(0o777)).is_ok()
        && rustix::fs::mkdir(RO_DIR, Mode::from_bits_retain(0o555)).is_ok()
        && rustix::fs::chmod(RO_DIR, Mode::from_bits_retain(0o555)).is_ok();
    r.check(
        "the window's two directories are staged (one writable, one not)",
        staged,
    );

    // A name that is already there, for the `EEXIST` claim below: made now,
    // while the caller still may.
    let (made, e) = mknodat(S_IFREG | 0o600, 0, cap_dir("exists").as_str());
    r.check(
        &format!("and a name the window will find already there (got {made}, errno {e})"),
        made == 0,
    );

    if was_root {
        let (made, e) = mknodat(S_IFCHR | 0o666, 0x0103, CHR);
        r.check(
            &format!(
                "a character node is made by a caller that holds CAP_MKNOD (got {made}, errno {e})"
            ),
            made == 0 && kind_of(CHR) == Some(FileType::CharacterDevice),
        );
        // The number it was made with, which is the one answer this kernel had
        // nothing to say about until this pass: `st_rdev` read `0` for every
        // device node whatever number the caller named. Linux's own answer for
        // `1:3` is beside it in the header's measurement, and it is `/dev/null`'s
        // number - the row every spelling of the encoding agrees on, which makes
        // it the control rather than the interesting one.
        let number = rdev_of(CHR);
        r.check(
            &format!("the node carries the number it was made with (got rdev {number:?})"),
            number == Some(encode(1, 3)),
        );
    } else {
        println!(
            "     not root: the node a *capable* caller makes is named here, and Linux's `EPERM` \
             half is measured below as this ordinary user"
        );
    }

    // The window itself. Root lowers only its **effective** id, so the measured
    // `cap_emulate_setxuid` rule clears the effective set while the permitted
    // one survives (the saved id is still root's) - and an ordinary user, which
    // is what a hand run on the host is, never held the capability to begin
    // with. Either way the claims below are the same ones.
    let dropped = if was_root { setresuid(-1, 1000, -1) } else { 0 };
    let how = if was_root {
        format!("the effective id is lowered to 1000, root's saved one kept (got {dropped})")
    } else {
        String::from("the caller is an ordinary user, which never holds CAP_MKNOD")
    };
    r.check(&how, dropped == 0);

    if dropped == 0 {
        let (chr, ec) = mknodat(S_IFCHR | 0o600, 0x0103, cap_dir("chr").as_str());
        r.check(
            &format!("a character node is EPERM without CAP_MKNOD (got {chr}, errno {ec})"),
            chr == -1 && ec == libc::EPERM,
        );
        let (blk, eb) = mknodat(S_IFBLK | 0o600, 0x0700, cap_dir("blk").as_str());
        r.check(
            &format!("and a block node too (got {blk}, errno {eb})"),
            blk == -1 && eb == libc::EPERM,
        );
        // The control the two above need: the same caller, in the same
        // directory, making a node the gate does not cover - so their `EPERM`s
        // are the capability's and not a directory it may not write in.
        let (white, ew) = mknodat(S_IFCHR | 0o600, 0, cap_dir("whiteout").as_str());
        r.check(
            &format!(
                "the character node whose dev is 0 (a whiteout) needs no capability (got {white}, errno {ew})"
            ),
            white == 0 && kind_of(cap_dir("whiteout").as_str()) == Some(FileType::CharacterDevice),
        );
        let (b0, e0) = mknodat(S_IFBLK | 0o600, 0, cap_dir("blk0").as_str());
        r.check(
            &format!(
                "but the exception is the character type's: a block node with dev 0 is EPERM (got {b0}, errno {e0})"
            ),
            b0 == -1 && e0 == libc::EPERM,
        );
        let (fifo, ef) = mknodat(S_IFIFO | 0o600, 0, cap_dir("fifo").as_str());
        let (reg, er) = mknodat(S_IFREG | 0o600, 0, cap_dir("reg").as_str());
        let (sock, es) = mknodat(S_IFSOCK | 0o600, 0, cap_dir("sock").as_str());
        r.check(
            &format!(
                "and a name that is no device needs none: S_IFIFO, S_IFREG and S_IFSOCK are served \
                 (got {fifo}/{ef}, {reg}/{er}, {sock}/{es})"
            ),
            fifo == 0 && reg == 0 && sock == 0,
        );
        // The order, from the side that makes it visible: each of the refusals
        // above answers *before* the capability is asked.
        let (exists, ee) = mknodat(S_IFCHR | 0o600, 0x0103, cap_dir("exists").as_str());
        r.check(
            &format!("a name that is already there is EEXIST before the capability (got {exists}, errno {ee})"),
            exists == -1 && ee == libc::EEXIST,
        );
        let (ro, ero) = mknodat(S_IFCHR | 0o600, 0x0103, &format!("{RO_DIR}/chr"));
        r.check(
            &format!(
                "and a directory the caller may not write is its own EACCES (got {ro}, errno {ero})"
            ),
            ro == -1 && ero == libc::EACCES,
        );
        let (gone, eg) = mknodat(S_IFCHR | 0o600, 0x0103, "/tmp/t-mknod-no-dir/chr");
        r.check(
            &format!("and a path that is not there is ENOENT (got {gone}, errno {eg})"),
            gone == -1 && eg == libc::ENOENT,
        );
    }

    // ...and the way back, which is the other half of a gate: the same call
    // refused a moment ago is *served* once the capability is back, so the
    // `EPERM`s above were the capability's and not a missing feature.
    if was_root && dropped == 0 {
        let back = setresuid(-1, 0, -1);
        r.check(
            &format!("raising the effective id back through the saved one (got {back})"),
            back == 0,
        );
        let (again, ea) = mknodat(S_IFCHR | 0o666, 0x0103, CHR_AGAIN);
        r.check(
            &format!(
                "and the same character node is served once CAP_MKNOD is back (got {again}, errno {ea})"
            ),
            again == 0 && kind_of(CHR_AGAIN) == Some(FileType::CharacterDevice),
        );

        // ---- the number a device node carries, and the kind beside it -------
        //
        // Both while the capability is held, because a *device* node is the only
        // thing it gates. `1:3` above is the control - `/dev/null`'s own number,
        // and one whose minor fits in the eight bits every spelling of the
        // encoding agrees on. What settles the question is a pair that does not
        // fit, and a node asked for from the other device type: those two rows
        // are where keeping the argument's **word** instead of the pair names
        // another device, and where reporting one kind for both devices shows.
        // Measured on a current host first, on a tmpfs and an ext4 agreeing
        // (`.workbuddy/probe_mknod_rdev.c`, Linux 7.2.11).
        let (wide, ew2) = mknodat(S_IFCHR | 0o666, encode(136, 300), CHR_WIDE);
        let wide_number = rdev_of(CHR_WIDE);
        r.check(
            &format!(
                "a character node keeps a minor that does not fit in eight bits (got {wide}, errno {ew2}, rdev {wide_number:?})"
            ),
            wide == 0 && wide_number == Some(encode(136, 300)),
        );
        r.check(
            &format!("and its number is the word Linux reports: 136:300 is 0x0010882c (got {wide_number:?})"),
            wide_number == Some(0x0010_882c),
        );
        let (blk_node, eb2) = mknodat(S_IFBLK | 0o660, encode(254, 70_000), BLK);
        let blk_number = rdev_of(BLK);
        r.check(
            &format!(
                "a block node keeps a twenty-bit minor too (got {blk_node}, errno {eb2}, rdev {blk_number:?})"
            ),
            blk_node == 0 && blk_number == Some(encode(254, 70_000)),
        );
        r.check(
            &format!("254:70000 is 0x1110fe70 (got {blk_number:?})"),
            blk_number == Some(0x1110_fe70),
        );
        // The kind is the number's other half and it was wrong here too: every
        // device node `mknod` made reported a *character* device, so a block
        // node's own name answered `stat` about a device nobody asked for.
        r.check(
            "and stat calls the node made from S_IFBLK a block device",
            kind_of(BLK) == Some(FileType::BlockDevice),
        );
        // The number nobody asked for: the whiteout's `0`, made above without
        // the capability, which is `0:0` through this same field.
        r.check(
            "and the whiteout's zero is 0:0 through the same field",
            rdev_of(cap_dir("whiteout").as_str()) == Some(0),
        );
    }

    // ---- the namespace the capability belongs to ---------------------------
    //
    // `vfs_mknod` asks `capable(CAP_MKNOD)` - the **initial** user namespace -
    // and not `ns_capable(current_user_ns(), ...)`. The two answers differ for
    // a caller that has entered a user namespace of its own, which holds the
    // whole capability set *in it* (the rung's own measured grant, §6.3) and is
    // still refused a device node on a directory the parent namespace owns.
    // Measured on a current host first (`.workbuddy/probe_capable_scope.sh`),
    // and the ordinary file in the same directory is the control that keeps the
    // refusal on the capability rather than on the namespace's own ids: the
    // child reads uid 65534 (an empty map) and may still make a plain name in a
    // `0777` directory.
    let device = in_user_namespace(|| {
        let (rc, e) = mknodat(S_IFCHR | 0o666, 0x0103, cap_dir("ns-chr").as_str());
        if rc == 0 { 0 } else { e }
    });
    let ordinary = in_user_namespace(|| {
        let (rc, e) = mknodat(S_IFREG | 0o644, 0, cap_dir("ns-reg").as_str());
        if rc == 0 { 0 } else { e }
    });
    r.check(
        &format!("a device node inside a user namespace is EPERM (got {device:?})"),
        device == Some(libc::EPERM),
    );
    r.check(
        &format!("but an ordinary file in the same directory is served (got {ordinary:?})"),
        ordinary == Some(0),
    );

    clear();
    r.finish()
}
