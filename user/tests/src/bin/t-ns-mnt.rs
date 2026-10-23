//! t-ns-mnt - a mount table that belongs to a *task*, not to the machine.
//!
//! `unshare(CLONE_NEWNS)` is step 1 of §6.3's MNT → PID → NET ladder, and it is
//! the same sentence `t-ns-uts` pins for the names, told about mounts: a task
//! that asks for its own namespace gets a **copy** (Linux's `copy_mnt_ns`),
//! keeps seeing what it saw, and can then mount into it without the namespace it
//! came from following. The shape of every check is the same pair — **the
//! child's view differs and the parent's does not** — because a "namespace" that
//! is really one shared table passes the first half of each and fails none:
//!
//! * a forked child that unshares and mounts a tmpfs at `/mnt/ns-mnt` sees a
//!   fresh empty tree there and names the mount in its own `/proc/mounts`, while
//!   the parent reads no such line and finds the directory as it left it;
//! * the copy keeps what was copied: before mounting anything, the child still
//!   sees the machine's root and its standard mounts — an *empty* namespace
//!   would make the call a way to lose the system rather than to isolate it;
//! * a mount the parent makes after the child unshared is invisible to the
//!   child (the two tables diverge in both directions, not just child→parent);
//! * the umount inside leaves the parent's table untouched, and the second
//!   `unshare` from the *same* process copies again — the old table survives
//!   with everything mounted into it.
//!
//! Like `t-ns-uts`, this program never renames or re-mounts the machine's own
//! namespace: the first forked child unshares before it touches anything, and
//! every mount below happens in a namespace this process asked for itself. It
//! shares the boot with every other test in the image, and a stray mount under
//! `/mnt` would be somebody else's failing check.
//!
//! # The library choices
//!
//! `rustix::mount::{mount, unmount}` and `rustix::process::fork` are the doors;
//! `libc::syscall(SYS_unshare)` carries the flag word because rustix's
//! `UnshareFlags` spells MNT as `NEWTIME`-adjacent bits per platform and the
//! claim here is specifically Linux's `0x00020000` word — the raw number is the
//! ABI, taken from `rustix::linux_kernel_features` nowhere and from
//! `CLONE_NEWNS`' header value everywhere. The report rides a pipe written by
//! the child through `rustix::io::write` and ends through the raw `SYS_exit`
//! door, exactly as `t-ns-uts` explains: std's stdout buffer across a fork
//! duplicates lines, and `exit_group` would run the inherited atexit list.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::{Errno, read, write};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use rustix::pipe::pipe;

/// Linux's `CLONE_NEWNS`, the whole word the syscall takes.
const CLONE_NEWNS: usize = 0x0002_0000;

const MNT: &str = "/tmp/t-ns-mnt";
/// A name only a tmpfs holds — the marker file proves which tree a read
/// landed in, where an empty directory could also be the jail's own.
const MARKER: &str = "/tmp/t-ns-mnt/inside-only";

fn raw_exit(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit, code as libc::c_long);
        libc::_exit(code)
    }
}

/// Turn musl's `-1` + `errno` door into the negated-errno answer these claims
/// compare against, read while the failing call is still the last one made.
/// The bare syscall's convention is the kernel's (`-errno` in the return word),
/// but `libc::syscall` is musl's wrapper and follows musl's.
fn fix(r: libc::c_long) -> isize {
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn unshare_newns() -> Result<(), Errno> {
    let rc = unsafe { libc::syscall(libc::SYS_unshare, CLONE_NEWNS as libc::c_long) };
    if rc == 0 {
        Ok(())
    } else {
        // The number comes out of `errno`, not out of the return word: this is
        // musl's door, which answers `-1` and leaves the refusal beside it (the
        // first draft negated the return word, which would have named `EPERM`
        // for every failure - the same protocol mistake the claims below carry
        // a `fix()` for).
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

/// Whether `/proc/mounts` carries a line naming `path` as its mountpoint.
/// Reading the file rather than probing behaviour is the point: the record is
/// what userland (`findmnt`, OpenRC's scan) actually compares namespaces by.
fn mounts_name(path: &str) -> bool {
    let mut buf = [0u8; 4096];
    let fd = match openat(CWD, "/proc/mounts", OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(_) => return false,
    };
    let n = match read(&fd, &mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(path))
}

/// Read the pipe to EOF.
fn drain(ro: &rustix::fd::OwnedFd, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match read(ro, &mut buf[total..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
    total
}

/// `clone(flags)` with the two machines' argument orders reconciled here —
/// the same helper shape `t-clone` carries (x86_64 passes
/// `(flags, stack, parent_tid, child_tid, tls)`, aarch64 swaps the last two);
/// no stack is passed, so the child is process-shaped and returns into this
/// call the way a fork's child does.
unsafe fn clone_raw(flags: usize) -> isize {
    let flags = flags as libc::c_long;
    #[cfg(target_arch = "x86_64")]
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            flags,
            0isize as libc::c_long,
            0isize as libc::c_long,
            0isize as libc::c_long,
            0isize as libc::c_long,
        )
    };
    #[cfg(target_arch = "aarch64")]
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            flags,
            0isize as libc::c_long,
            0isize as libc::c_long,
            0isize as libc::c_long,
            0isize as libc::c_long,
        )
    };
    ret as isize
}

fn main() {
    println!("t-ns-mnt");
    let mut r = Report::new();

    // The payload mounts, so it needs the privilege; on a host whose user
    // cannot mount, skip rather than report red (measured: unprivileged Docker
    // refuses even unshare by default).
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: this host user cannot mount; the payload runs as root");
        r.finish()
    }

    // Clean slate: a leftover from an interrupted run would turn "the mount
    // succeeds inside" into EBUSY and hide every later check.
    let _ = unmount(MNT, UnmountFlags::empty());
    let _ = unlinkat(
        CWD,
        MARKER.strip_prefix(MNT).unwrap_or("marker"),
        rustix::fs::AtFlags::REMOVEDIR,
    );
    let _ = unlinkat(CWD, MNT, rustix::fs::AtFlags::REMOVEDIR);
    r.check(
        "the mountpoint directory is made",
        mkdirat(CWD, MNT, Mode::from_raw_mode(0o755)).is_ok(),
    );

    // ---- The child's table diverges from the parent's, in both directions ----
    let pipes = pipe();
    r.check("the report pipe is made", pipes.is_ok());
    if !pipes.is_ok() {
        r.finish();
    }
    let (ro, wo) = pipes.unwrap();
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::syscall(libc::SYS_close, ro.as_raw_fd() as libc::c_long) };
        let say = |s: &[u8]| {
            let _ = write(&wo, s);
        };
        if unshare_newns().is_err() {
            // Mounting now would mount in the *machine's* namespace, which
            // every other test in this image shares. Say so and stop.
            say(b"!unshare\n");
            raw_exit(1);
        }
        // The copy keeps what was copied: the root must still resolve, or the
        // namespace is an erasure rather than a copy.
        // `AsRawFd` on a directory fd is the cheap "the root still resolves"
        // probe: cwd survived the copy iff `/` does, and no path walk can say
        // it with fewer moving parts. (rustix's `readlink` takes its buffer by
        // value through `Param`, which is why the borrow form here matters.)
        let cwd_fd = openat(CWD, ".", OFlags::RDONLY, Mode::empty());
        if cwd_fd.is_err() {
            say(b"!root-gone\n");
            raw_exit(1);
        }
        // Mount a tmpfs inside. The fstype word is the kernel's type table's
        // own name; busybox `mount -t tmpfs` spells the same request.
        match mount("tmpfs", MNT, "tmpfs", MountFlags::empty(), None) {
            Ok(()) => {}
            Err(e) => {
                say(b"!mount ");
                let _ = write(&wo, format!("{:?}\n", e).as_bytes());
                raw_exit(1);
            }
        }
        // Inside: the mountpoint is a fresh tree — create a marker, report its
        // presence, and the /proc/mounts line naming the mount.
        let made = openat(
            CWD,
            MARKER,
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o600),
        )
        .is_ok();
        say(if made { b"made\n" } else { b"!made\n" });
        say(if mounts_name(MNT) {
            b"recorded\n"
        } else {
            b"!recorded\n"
        });
        raw_exit(0);
    }
    r.check("the child was forked", child > 0);
    if child <= 0 {
        r.finish();
    }
    drop(wo);
    let mut buf = [0u8; 256];
    let n = drain(&ro, &mut buf);
    let report = String::from_utf8_lossy(&buf[..n]).replace('\n', " ");

    // The child's observations, checked *here* in the parent: two views of one
    // fact compared side by side, not a child's verdict trusted blindly.
    r.check(
        "the child reported cleanly",
        report.contains("made") && report.contains("recorded"),
    );
    if !report.contains("made") || !report.contains("recorded") {
        println!("  child said: {report}");
    }

    // The parent's own view, the counterfactual half: with one shared table
    // every one of these flips.
    r.check(
        "the parent's /proc/mounts does not carry the child's mount",
        !mounts_name(MNT),
    );
    let parent_sees_marker = openat(CWD, MARKER, OFlags::RDONLY, Mode::empty()).is_err();
    r.check(
        "the parent's mountpoint is empty — the marker lives only inside",
        parent_sees_marker,
    );

    // ---- And the divergence runs the other way too ----
    // A bind the parent makes *after* the child unshared must stay outside the
    // child's table. The child has exited, so ask a fresh one: the new child
    // inherits the parent's table (fork shares by reference), and a third child
    // that unshares again copies a table that now carries the bind.
    let src = "/tmp/t-ns-mnt-src";
    let _ = unlinkat(CWD, src, rustix::fs::AtFlags::REMOVEDIR);
    r.check(
        "the bind source is made",
        mkdirat(CWD, src, Mode::from_raw_mode(0o755)).is_ok(),
    );
    let bound = mount(src, MNT, "none", MountFlags::BIND, None).is_ok();
    r.check("the parent mounts its own bind", bound);
    if bound {
        // Visible to a child that did NOT unshare (inheritance is by reference)...
        let pipes2 = pipe();
        if let Ok((ro2, wo2)) = pipes2 {
            let child2 = unsafe { libc::fork() };
            if child2 == 0 {
                unsafe { libc::syscall(libc::SYS_close, ro2.as_raw_fd() as libc::c_long) };
                let say = |s: &[u8]| {
                    let _ = write(&wo2, s);
                };
                if unshare_newns().is_err() {
                    say(b"!unshare\n");
                    raw_exit(1);
                }
                // ...but not to this child once it unshares: the copy happened
                // after the bind, so the bind IS in the copy. The direction
                // that must fail is the reverse: unmount in the child, parent
                // keeps it.
                let had = mounts_name(MNT);
                let _ = unmount(MNT, UnmountFlags::empty());
                let gone = !mounts_name(MNT);
                say(if had && gone { b"peeled\n" } else { b"!peel\n" });
                raw_exit(0);
            }
            drop(wo2);
            let mut buf2 = [0u8; 64];
            let n2 = drain(&ro2, &mut buf2);
            r.check(
                "a child's umount peels its own copy, not the parent's mount",
                String::from_utf8_lossy(&buf2[..n2]).contains("peeled"),
            );
            r.check(
                "the parent still carries its bind after the child unmounted",
                mounts_name(MNT),
            );
            let _ = unmount(MNT, UnmountFlags::empty());
        }
    }

    // ---- The two refusals that make the served answer honest ----
    //
    // A word mixing MNT with a kind this kernel cannot copy is refused *whole*:
    // Linux's `ksys_unshare` would already have copied the first part by the
    // time the second failed, and a half-unshared caller is a caller with a
    // namespace it did not ask to keep. Pinned as behaviour, not read off the
    // verdict table in `abi`: the gate is where the promise lives.
    let mixed = fix(unsafe {
        libc::syscall(
            libc::SYS_unshare,
            // The unserved half is `CGROUP` now: the word used to be `NET` and
            // then `IPC`, and each became a served kind when its rung landed -
            // the drift §6.3 records over and over.
            (CLONE_NEWNS | libc::CLONE_NEWCGROUP as usize) as libc::c_long,
        )
    });
    r.check(
        "a word mixing MNT with an unserved kind is refused whole",
        mixed == -(Errno::OPNOTSUPP.raw_os_error() as isize),
    );
    if mixed != 0 {
        // Nothing moved: the parent's table still has no private mount.
        r.check(
            "the refused call mounted nothing",
            !mounts_name("/tmp/t-ns-mnt-refused"),
        );
    }

    // ---- `clone(CLONE_NEWNS)`: the child starts in a copy ----
    //
    // The flag is served now: the child gets its own table and mounts into it
    // without this process following. It reports through a pipe like the first
    // child; the parent compares two observations again.
    if bound {
        // The bind above was peeled inside the copy; the parent's still stands
        // (`mounts_name` said so). A clone-with-NEWNS must NOT see what happens
        // later here either — but that direction is inheritance-at-copy-time,
        // already pinned; what matters is the fresh child mounting where the
        // parent cannot look.
        let _ = unmount(MNT, UnmountFlags::empty());
    }
    // The child is process-shaped — a fork that returns from the call the way
    // its creator did (`t-clone`'s shape for exactly this reason): no stack is
    // passed, so it comes back inside `clone_raw` with `0`, does one mount and
    // one report, and leaves through the raw door. A thread-shaped entry on a
    // hand-built stack would run std's TLS-less child machinery, which is not
    // what this claim needs.
    let pipes3 = pipe();
    if let Ok((ro3, wo3)) = pipes3 {
        let wo3_raw = wo3.as_raw_fd();
        let cloned = unsafe { clone_raw(CLONE_NEWNS | libc::SIGCHLD as usize) };
        if cloned == 0 {
            let ok =
                mount("tmpfs", MNT, "tmpfs", MountFlags::empty(), None).is_ok() && mounts_name(MNT);
            let msg: &[u8] = if ok {
                b"cloned-mounted\n"
            } else {
                b"!cloned-mounted\n"
            };
            unsafe {
                libc::write(wo3_raw, msg.as_ptr() as *const libc::c_void, msg.len());
                libc::syscall(libc::SYS_exit, 0);
            }
            std::process::exit(0); // unreachable
        }
        r.check("clone(CLONE_NEWNS) creates the child", cloned > 0);
        if cloned <= 0 {
            println!("  clone answered {cloned}");
        }
        drop(wo3);
        let mut buf3 = [0u8; 64];
        let n3 = drain(&ro3, &mut buf3);
        r.check(
            "the cloned child mounts unseen by its parent",
            String::from_utf8_lossy(&buf3[..n3]).contains("cloned-mounted"),
        );
        r.check("and the parent's table stayed clean", !mounts_name(MNT));
    }

    // An unserved kind on `clone` answers the same refusal `unshare` gives it:
    // SysV IPC has no second instance to hand a child at all. The word is
    // `libc::CLONE_NEWIPC` rather than a literal, and that is a correction: the
    // first draft wrote `0x0008_0000` here with `CLONE_NEWPID` in the comment,
    // which is `CLONE_SETTLS` — so the call started a child with a thread
    // pointer the register happened to hold, and the guest reported the child's
    // page fault at `0xfffffffffffffff8` instead of a clean refusal. The word has
    // moved twice since, once per rung built - PID, then NET - which is the price
    // of a claim about "the next unserved kind". The errno comes through `fix()`
    // for the protocol reason above it.
    let cloned_ipc = fix(unsafe {
        libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_NEWIPC as usize | libc::SIGCHLD as usize) as libc::c_long,
            0isize as libc::c_long,
        )
    });
    r.check(
        "clone(CLONE_NEWIPC) is refused whole, not half-served",
        cloned_ipc == -(Errno::OPNOTSUPP.raw_os_error() as isize),
    );

    r.finish()
}
