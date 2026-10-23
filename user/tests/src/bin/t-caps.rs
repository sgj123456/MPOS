//! t-caps - the capability model, end to end: the gates exist and they ask
//! the caller's *effective set*, not its name.
//!
//! The credential model in `crates/cred` has stored the four sets since it
//! landed; what this program pins is the half a syscall test can see - that
//! the gates are wired. The shape is one identity dance: a root process
//! lowers its effective id to 1, which clears the effective set (the
//! measured `cap_emulate_setxuid` rule), and every gated call answers
//! `EPERM` while it is down - including `klogctl` with a type the kernel
//! would refuse anyway, which is the proof the gate runs *before* the type
//! validation, as Linux's does. Then the caller raises itself back through
//! the saved id, the effective set is restored from permitted, and the same
//! calls work again. Two of the namespace kinds are *served* now - the names
//! (`CLONE_NEWUTS`) and the mount table (`CLONE_NEWNS`, MNT at step 1 of
//! §6.3's ladder) - so both answer `EPERM` while the capability is down and
//! split exactly when it comes back, and a kind the kernel cannot hand over at
//! all (`CLONE_NEWPID` and the rest of the unserved ladder) is `EOPNOTSUPP`
//! *once the capability is back*: the privilege answer comes first, as Linux
//! decides it, so an unprivileged caller hears the same `EPERM` a Linux would
//! give it and only a capable one learns what this machine cannot do. That pair
//! is the difference between a gate and a refusal, and this program pins both
//! halves.
//!
//! The set words themselves are read through `capget` (the v3 wire shape,
//! two records): while lowered, effective is empty and permitted is still
//! full - some id was still root's, so the permitted set survives - and
//! after the raise-back both are full again. That is the round-trip
//! `t-posix` pins from the other side; here it is what the gates read.
//!
//! # The host run
//!
//! The host's kernel answers `EINVAL` to `setresuid(-1, 1, -1)` (measured,
//! `t-posix`'s probe), so the dance cannot be staged there and every claim
//! below is named and not made (`t-pty`'s mint rule). This program's
//! evidence lives in the guest, where the machine is root's to lower.

use std::os::unix::fs::PermissionsExt;

use mpos_user_tests::{Report, host_run, in_user_namespace};

/// `capget(header, data)` through the raw door - `libc` does not wrap it on
/// this target, and the v3 wire shape (two records) is the claim.
fn capget(header: &mut CapHeader, data: &mut [CapRecord; 2]) -> isize {
    // SAFETY: both pointers are this program's own memory, which outlives
    // the call; the kernel writes only the two records the version names.
    let r = unsafe {
        libc::syscall(
            libc::SYS_capget,
            header as *mut CapHeader as libc::c_long,
            data.as_mut_ptr() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// `capset(header, data)` through the same raw door, the setter of the v3
/// wire: what this program lowers and raises its own capabilities with.
fn capset(header: &mut CapHeader, data: &mut [CapRecord; 2]) -> isize {
    // SAFETY: both pointers are this program's own memory, which outlives the
    // call, and the records are the two the version names.
    let r = unsafe {
        libc::syscall(
            libc::SYS_capset,
            header as *mut CapHeader as libc::c_long,
            data.as_mut_ptr() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// The errno `open(2)` answers, or `0` - the DAC table's rows are "what did the
/// call say", so the number is the claim and not a boolean.
fn open_errno(path: &str, flags: libc::c_int) -> i32 {
    match std::ffi::CString::new(path) {
        Ok(p) => {
            let fd = unsafe { libc::open(p.as_ptr(), flags) };
            if fd < 0 {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            } else {
                unsafe { libc::close(fd) };
                0
            }
        }
        Err(_) => libc::EINVAL,
    }
}

/// The errno `access(2)` answers, or `0`.
fn access_errno(path: &str, mode: libc::c_int) -> i32 {
    match std::ffi::CString::new(path) {
        Ok(p) => {
            if unsafe { libc::access(p.as_ptr(), mode) } == 0 {
                0
            } else {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            }
        }
        Err(_) => libc::EINVAL,
    }
}

/// The v3 wire's bit for `cap`, in the low word the first 32 live in.
fn bit_of(cap: u32) -> u32 {
    1u32 << (cap % 32)
}

/// The errno `chmod(2)` answers, or `0` - the owner-shaped table's rows are the
/// same shape of claim as the DAC rows, so they read the number too.
fn chmod_errno(path: &str, mode: libc::mode_t) -> i32 {
    match std::ffi::CString::new(path) {
        Ok(p) => {
            if unsafe { libc::chmod(p.as_ptr(), mode) } == 0 {
                0
            } else {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            }
        }
        Err(_) => libc::EINVAL,
    }
}

/// The errno `chown(2)` answers, or `0`. `u32::MAX` is the family's "leave this
/// one alone", which is how a row asks about one id at a time.
fn chown_errno(path: &str, uid: libc::uid_t, gid: libc::gid_t) -> i32 {
    match std::ffi::CString::new(path) {
        Ok(p) => {
            if unsafe { libc::chown(p.as_ptr(), uid, gid) } == 0 {
                0
            } else {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            }
        }
        Err(_) => libc::EINVAL,
    }
}

/// The errno `utimensat(2)` answers, or `0`. A null `times` is the "both clocks
/// to now" form - the one Linux spells `ATTR_TOUCH` and the one write
/// permission buys - and an explicit pair is the form it does not.
fn utimensat_errno(path: &str, times: Option<&[libc::timespec; 2]>) -> i32 {
    let ptr = match times {
        Some(t) => t.as_ptr(),
        None => std::ptr::null(),
    };
    match std::ffi::CString::new(path) {
        Ok(p) => {
            if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), ptr, 0) } == 0 {
                0
            } else {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            }
        }
        Err(_) => libc::EINVAL,
    }
}

/// The permission bits a node carries *now* - what a row about a `chmod` that
/// was allowed to succeed has to read back, since the call's own return says
/// nothing about which bits survived.
fn mode_of(path: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.mode() & 0o7777)
        .unwrap_or(0)
}

/// Write one byte to `path` and read the mode back - the *write* door of the
/// set-id rule, which Linux's `file_remove_privs` runs off a write as it does
/// off a `chown`. Opened for writing **without** `O_TRUNC`, so what is being
/// measured is the bytes and not the resize (the resize is its own door, below).
fn write_one(path: &str) -> u32 {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.write_all(b"x");
    }
    mode_of(path)
}

/// Cut `path` back to nothing and read the mode back - the resize door, which
/// costs the file the same two bits (measured: `truncate` and `ftruncate` both
/// store `0755` where the file carried `4755`).
fn truncate_it(path: &str) -> u32 {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path);
    mode_of(path)
}

/// Lay out 8192 bytes in `path` and read the mode back - the preallocation
/// door, which is a write to the file's blocks without a byte going through
/// the caller's buffer. `fallocate` on a `4755` file stores `0755` unless
/// `CAP_FSETID` is held, and it does so whether or not the length moves
/// (measured: `.workbuddy/probe_fallocate_mode.c` A, B, D and E).
fn fallocate_it(path: &str) -> u32 {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is open for writing and the range is inside
        // what the filesystem can hold.
        let _ = unsafe {
            libc::syscall(
                libc::SYS_fallocate,
                f.as_raw_fd() as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                8192 as libc::c_long,
            )
        };
    }
    mode_of(path)
}

/// `klogctl(type, buf, len)` - `syslog(2)`'s raw door, the same one
/// `t-syslog` drives.
fn klogctl(kind: i32) -> isize {
    // SAFETY: a null buffer - only the counts and the refusals are asked.
    let r = unsafe { libc::syscall(libc::SYS_syslog, kind as libc::c_long, 0i64, 0i64) };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// A wrapped libc call whose errno a claim pins: musl answers a plain `-1`
/// with the errno beside it, so this restores the `-errno` spelling.
fn rc(rc: libc::c_int) -> libc::c_int {
    if rc == -1 {
        -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        rc
    }
}

fn vhangup() -> isize {
    rc(unsafe { libc::vhangup() }) as isize
}

fn chroot(path: &str) -> isize {
    match std::ffi::CString::new(path) {
        Ok(p) => rc(unsafe { libc::chroot(p.as_ptr()) }) as isize,
        Err(_) => -(libc::EINVAL as isize),
    }
}

fn unshare(flags: libc::c_int) -> isize {
    rc(unsafe { libc::unshare(flags) }) as isize
}

fn setresuid(r: u32, e: u32, s: u32) -> isize {
    rc(unsafe { libc::setresuid(r, e, s) }) as isize
}

/// The v3 wire shapes, as `<linux/capability.h>` spells them.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

/// The uapi's `CAP_LAST_CAP` (41 capabilities, 0..=40): bits 0..=31 in the
/// low word, the top nine in the high one.
const CAP_HIGH: u32 = (1u32 << (40 - 31)) - 1;

fn main() {
    println!("t-caps");
    let mut r = Report::new();

    // The dance the whole program turns on. On the host it cannot be staged
    // at all: the kernel refuses the lowering with `EINVAL`, so on a host
    // run the claims are named and not made rather than red.
    let lowered = setresuid(u32::MAX, 1, u32::MAX);
    r.check(
        "setresuid(-1, 1, -1) lowers the effective id for root",
        lowered == 0 || host_run() && lowered == -(libc::EINVAL as isize),
    );

    if lowered != 0 && host_run() {
        println!(
            "note: the host refuses the lowering (EINVAL, t-posix's probe); the gated claims are named and not made on a host run"
        );
        r.finish();
    }

    // ---- while lowered: the effective set is empty, and the gates hold ----
    //
    // The sets, read back through the v3 wire: effective empty (the identity
    // move cleared it), permitted still full (the saved id is still root's),
    // inheritable untouched.
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapRecord {
        effective: 0,
        permitted: 0,
        inheritable: 0xdead_beef,
    }; 2];
    r.check(
        "capget succeeds while lowered",
        capget(&mut header, &mut data) == 0,
    );
    r.check(
        "the effective set read back empty",
        data[0].effective == 0 && data[1].effective == 0,
    );
    r.check(
        "the permitted set is still full (some id is root's)",
        data[0].permitted == u32::MAX && data[1].permitted == CAP_HIGH,
    );
    r.check(
        "inheritable is still empty",
        data[0].inheritable == 0 && data[1].inheritable == 0,
    );

    // The gates. Each is a call this kernel serves to root, refusing now
    // only because the effective set is.
    let log = klogctl(9);
    r.check(
        "klogctl's counters answer EPERM while lowered",
        log == -(libc::EPERM as isize),
    );
    // The ordering claim, in one line: an unknown type would be EINVAL if
    // the type validation ran first, so EPERM here says the gate is in
    // front of it - Linux's order, measured on the host in batch 29.
    let unknown = klogctl(99);
    r.check(
        "klogctl answers EPERM before it validates the type",
        unknown == -(libc::EPERM as isize),
    );
    let hung = vhangup();
    r.check(
        "vhangup answers EPERM while lowered",
        hung == -(libc::EPERM as isize),
    );
    let rooted = chroot("/nonexistent-for-t-caps");
    r.check(
        "chroot answers EPERM before it looks at the path",
        rooted == -(libc::EPERM as isize),
    );
    let uts = unshare(0x0400_0000);
    r.check(
        "unshare CLONE_NEWUTS answers EPERM while lowered",
        uts == -(libc::EPERM as isize),
    );
    // The ordering the two refusals share: privilege is asked *first*, as
    // Linux's `check_unshare_flags` does before any kind exists to copy - so
    // CLONE_NEWNS, a served kind since MNT landed, answers the same EPERM as
    // CLONE_NEWUTS while the capability is down rather than a served-looking
    // success. What the *unserved* kinds answer is below, where the capability
    // is back and nothing else can be the reason.
    let mount = unshare(0x0002_0000);
    r.check(
        "unshare CLONE_NEWNS answers EPERM while lowered (served, so gated)",
        mount == -(libc::EPERM as isize),
    );
    let noop = unshare(0);
    r.check("unshare(0) is still a no-op", noop == 0);

    // Raw sockets are the other family this model gates. Linux asks for
    // `CAP_NET_RAW` *before* it looks at the protocol - measured on the host
    // (`.workbuddy/probe_rawip.c`, an unprivileged caller): ICMP, IPPROTO_RAW,
    // TCP and a protocol number nothing has are all EPERM - so the claim that
    // tells the two orders apart is the one with a protocol that is *not*
    // served, and it must answer EPERM rather than EPROTONOSUPPORT.
    let icmp = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP) };
    r.check(
        "AF_INET SOCK_RAW answers EPERM while lowered",
        icmp == -(libc::EPERM),
    );
    let bogus = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, 99) };
    r.check(
        "and EPERM before it judges the protocol (99 is not served)",
        bogus == -(libc::EPERM),
    );
    let packet = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM,
            libc::htons(libc::ETH_P_ALL as u16) as i32,
        )
    };
    r.check(
        "AF_PACKET answers EPERM while lowered (SOCK_DGRAM too)",
        packet == -(libc::EPERM),
    );

    // ---- the way back, through the saved id ----
    let raised = setresuid(u32::MAX, 0, u32::MAX);
    r.check(
        "setresuid(-1, 0, -1) raises the effective id back",
        raised == 0,
    );

    r.check(
        "capget succeeds after the raise",
        capget(&mut header, &mut data) == 0,
    );

    // ...and the raw doors open again for the same caller, which is the other
    // half of the gate: what `EPERM` refused a moment ago is now served, so the
    // refusals above were the capability's doing and not a missing feature.
    let icmp = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP) };
    r.check(
        "AF_INET SOCK_RAW opens once the capability is back",
        icmp >= 0,
    );
    if icmp >= 0 {
        unsafe { libc::close(icmp as i32) };
    }
    r.check(
        "the effective set is full again (restored from permitted)",
        data[0].effective == u32::MAX && data[1].effective == CAP_HIGH,
    );
    r.check(
        "and the permitted set never moved",
        data[0].permitted == u32::MAX && data[1].permitted == CAP_HIGH,
    );
    let log = klogctl(9);
    r.check("klogctl's counters answer a count again", log >= 0);
    let hung = vhangup();
    r.check(
        "vhangup is back to the no-terminal answer",
        hung == -(libc::ENOTTY as isize),
    );
    // The served namespace splits again now that the capability is back -
    // the positive answers in the set, and the difference between a gate and
    // a refusal.
    let uts = unshare(0x0400_0000);
    r.check("unshare CLONE_NEWUTS is served again", uts == 0);
    let mount = unshare(0x0002_0000);
    r.check("unshare CLONE_NEWNS is served again", mount == 0);
    // The third served rung is the numbers: with the capability back, the
    // caller is handed a namespace its *children* will be numbered in, and the
    // call is `0` - the pair *this* call cannot observe (the caller keeps its
    // own pid) is `t-ns-pid`'s subject.
    let pid = unshare(0x2000_0000);
    r.check(
        "unshare CLONE_NEWPID is served once nothing else can be the reason",
        pid == 0,
    );
    // And the kinds this machine has no way to hand over keep their honest
    // refusal *now* that no capability gate can be the reason: EOPNOTSUPP is
    // "present, cannot do that", which is what a capable caller learns, and
    // what an unprivileged one would have heard as EPERM above. Nothing is
    // unshared either way - the refusal is whole. This witness has moved once
    // per rung built (`CLONE_NEWPID`, then `CLONE_NEWNET`, then `CLONE_NEWIPC`)
    // and now names the last kind with nothing behind it: a hardcoded served set
    // drifts the moment it grows, which §6.3 records over and over.
    let cgroup = unshare(0x0200_0000);
    r.check(
        "unshare CLONE_NEWCGROUP is EOPNOTSUPP once nothing else can be the reason",
        cgroup == -(libc::EOPNOTSUPP as isize),
    );

    // ---- the DAC override is two capabilities, not the uid ----
    //
    // `may_as`'s override is Linux's `capable_wrt_inode_uidgid`, which is asked
    // of the **file's** filesystem's user namespace and is *two* capabilities
    // rather than one. Measured on a current host
    // (`.workbuddy/probe_capable_scope.sh dac`, root with each capability
    // dropped from the effective set in turn, against mode `0000` and `0444`
    // fixtures): `CAP_DAC_OVERRIDE` grants a read and a write on a file
    // **whatever its bits are** - the clause that required some triad to carry
    // the bit is gone from Linux - `CAP_DAC_READ_SEARCH` grants a read on its
    // own, and `X_OK` is the one kind that needs an execute bit *somewhere*,
    // which is why the same capabilities refuse `access(R_OK|W_OK|X_OK)` on a
    // `0000` file where they grant `access(R_OK)`. The fixtures are this
    // program's own files with their modes cleared, so the triads refuse them
    // to everyone and only a capability can answer.
    let zero = "/tmp/t-caps-zero";
    let four = "/tmp/t-caps-four";
    let sixfour = "/tmp/t-caps-0644";
    for path in [zero, four, sixfour] {
        let _ = std::fs::remove_file(path);
    }
    let made = std::fs::File::create(zero).is_ok()
        && std::fs::File::create(four).is_ok()
        && std::fs::File::create(sixfour).is_ok();
    let flagged = made
        && std::fs::set_permissions(zero, std::fs::Permissions::from_mode(0o000)).is_ok()
        && std::fs::set_permissions(four, std::fs::Permissions::from_mode(0o444)).is_ok()
        && std::fs::set_permissions(sixfour, std::fs::Permissions::from_mode(0o644)).is_ok();
    r.check("three fixtures with their modes set", flagged);

    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut whole = [CapRecord {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let read_back = capget(&mut header, &mut whole) == 0;
    r.check("the whole set read back for the table", read_back);

    // One row of the table: the effective set with `clear` lowered, and the
    // calls the measurement makes.
    let mut row = |r: &mut Report, label: &str, clear: &[u32], whole: &[CapRecord; 2]| {
        let mut data = *whole;
        for &cap in clear {
            data[0].effective &= !bit_of(cap);
        }
        let lowered = capset(&mut header, &mut data) == 0;
        r.check(&format!("{label}: the set is lowered"), lowered);
        let read = open_errno(zero, libc::O_RDONLY);
        let write = open_errno(four, libc::O_WRONLY);
        let exec = access_errno(sixfour, libc::X_OK);
        // Put the effective set back, so the next row starts from the whole one.
        let mut restore = *whole;
        capset(&mut header, &mut restore);
        (read, write, exec)
    };

    let (read, write, exec) = row(&mut r, "the whole set", &[], &whole);
    r.check(
        &format!("whole set: a 0000 file is readable (got {read})"),
        read == 0,
    );
    r.check(
        &format!("whole set: a 0444 file is writable (got {write})"),
        write == 0,
    );
    r.check(
        &format!("whole set: access(X_OK) on a 0644 file is EACCES (got {exec})"),
        exec == libc::EACCES,
    );
    let (read, write, _) = row(&mut r, "CAP_DAC_OVERRIDE dropped", &[1], &whole);
    r.check(
        &format!("OVERRIDE dropped: the read still answers, READ_SEARCH's (got {read})"),
        read == 0,
    );
    r.check(
        &format!("OVERRIDE dropped: the write is refused (got {write})"),
        write == libc::EACCES,
    );
    let (read, write, _) = row(&mut r, "CAP_DAC_READ_SEARCH dropped", &[2], &whole);
    r.check(
        &format!("READ_SEARCH dropped: the read still answers, OVERRIDE's (got {read})"),
        read == 0,
    );
    r.check(
        &format!("READ_SEARCH dropped: and the write answers too (got {write})"),
        write == 0,
    );
    let (read, write, _) = row(&mut r, "both DAC capabilities dropped", &[1, 2], &whole);
    r.check(
        &format!("both dropped: the read is refused (got {read})"),
        read == libc::EACCES,
    );
    r.check(
        &format!("both dropped: and so is the write (got {write})"),
        write == libc::EACCES,
    );

    // The sticky half of the same family - `check_sticky`'s last clause is
    // `capable_wrt_inode_uidgid(dir, CAP_FOWNER)`, and not "is the caller
    // root". The directory is `1777` and *another user's*, and the file inside
    // it is another user's too, so neither id arm answers and only the
    // capability can.
    let sticky = "/tmp/t-caps-sticky";
    let _ = std::fs::remove_dir_all(sticky);
    let mut fixture = std::fs::create_dir_all(sticky).is_ok();
    fixture &= std::fs::set_permissions(sticky, std::fs::Permissions::from_mode(0o1777)).is_ok();
    let inside = format!("{sticky}/f");
    fixture &= std::fs::File::create(&inside).is_ok();
    fixture &= unsafe {
        let dir = std::ffi::CString::new(sticky).unwrap();
        let file = std::ffi::CString::new(inside.as_str()).unwrap();
        libc::chown(dir.as_ptr(), 1000, 1000) == 0 && libc::chown(file.as_ptr(), 1000, 1000) == 0
    };
    r.check(
        "a 1777 directory and a file in it, both another user's",
        fixture,
    );
    let served = std::fs::remove_file(&inside).is_ok();
    r.check("the whole set removes it (CAP_FOWNER's answer)", served);
    // The recreated file is *this* caller's until it is handed over again: the
    // sticky rule's first arm is the file's own owner, so a fixture that stayed
    // root's would answer yes for the id and never reach the capability.
    let remade = std::fs::File::create(&inside).is_ok()
        && match std::ffi::CString::new(inside.as_str()) {
            Ok(path) => unsafe { libc::chown(path.as_ptr(), 1000, 1000) == 0 },
            Err(_) => false,
        };
    let mut without_fowner = whole;
    without_fowner[0].effective &= !bit_of(3);
    let lowered = capset(&mut header, &mut without_fowner) == 0;
    let refused = std::fs::remove_file(&inside).is_err();
    capset(&mut header, &mut whole);
    r.check(
        "with CAP_FOWNER dropped the same removal is refused",
        remade && lowered && refused,
    );

    // And the namespace half of the same question, which is what makes it a
    // capability and not an id. Inside a user namespace the caller holds the
    // whole set *in it* (the rung's measured grant, §6.3) - and the file below
    // is one whose triads refuse it, because it belongs to another uid, so the
    // *only* thing that could answer a read is a capability in the namespace
    // the file's filesystem belongs to. There is none: a capability is held in
    // a namespace and its descendants, and this file's filesystem is the
    // initial one's. Measured (`.workbuddy/probe_capable_scope.sh`, the
    // mapped-root row): `EACCES`. The file beside it, owned by the namespace's
    // own stored uid, is the control - the child does read what it owns.
    let foreign = "/tmp/t-caps-foreign-0600";
    let own = "/tmp/t-caps-own-0644";
    for path in [foreign, own] {
        let _ = std::fs::remove_file(path);
    }
    let made = std::fs::File::create(foreign).is_ok() && std::fs::File::create(own).is_ok();
    let owned = made
        && std::fs::set_permissions(foreign, std::fs::Permissions::from_mode(0o600)).is_ok()
        && std::fs::set_permissions(own, std::fs::Permissions::from_mode(0o644)).is_ok()
        && match std::ffi::CString::new(foreign) {
            Ok(path) => unsafe { libc::chown(path.as_ptr(), 1000, 1000) == 0 },
            Err(_) => false,
        };
    r.check("a 0600 file owned by another user, and a 0644 one", owned);
    // Two answers through the helper's one word: both are small errnos.
    let packed = in_user_namespace(|| {
        let other = open_errno(foreign, libc::O_RDONLY);
        let mine = open_errno(own, libc::O_RDONLY);
        (other << 16) | (mine & 0xffff)
    })
    .unwrap_or(-1);
    let (foreign_read, own_read) = (packed >> 16, packed & 0xffff);
    r.check(
        &format!(
            "inside a user namespace the other user's 0600 file is refused (got {foreign_read})"
        ),
        foreign_read == libc::EACCES,
    );
    r.check(
        &format!("and the file the namespace's own uid owns is served (got {own_read})"),
        own_read == 0,
    );
    for path in [foreign, own] {
        let _ = std::fs::remove_file(path);
    }

    // ---- the owner-shaped calls ask a capability, not the uid ----
    //
    // The same shape of question one noun over: `chmod`, `chown` and the
    // timestamps all turn on `inode_owner_or_capable` - the node's own uid, or
    // `CAP_FOWNER` - with `in_group_or_capable(CAP_FSETID)` deciding the
    // set-group-id bit and `CAP_CHOWN` an owner change. Measured on a current
    // host (`.workbuddy/probe_capable_scope.sh own`, one capability dropped at
    // a time from a root caller, against fixtures belonging to uid 1000): with
    // `CAP_FOWNER` gone, another user's file is refused a `chmod` while the
    // caller's own is served; with `CAP_FSETID` gone, `chmod 2755` on the
    // caller's own file stores `0755`; with `CAP_CHOWN` gone, a `chown` naming
    // a **uid** is refused while one naming the caller's own is served; and an
    // explicit pair of times is refused where the "now" form goes through the
    // DAC override.
    //
    // Every row keeps the caller's uid at `0` and moves only the effective
    // set, which is what makes each of these a capability question: the
    // id-shaped rule they replace answered `0` to all four.
    let other = "/tmp/t-caps-other";
    let mine = "/tmp/t-caps-mine";
    let minegid = "/tmp/t-caps-minegid";
    // The timestamps rows need a fixture of their own, and not the one the
    // `chown` rows use: a `chown` naming a uid is *served* to a caller holding
    // `CAP_CHOWN` even with `CAP_FOWNER` gone, so the moment a row has made it
    // the file is this caller's and the ownership question is no longer being
    // asked. Separate files are how the host probe keeps the two apart too
    // (`.workbuddy/probe_capable_scope.c`, whose `times_matrix` reads
    // `other0600`/`other0666` while its chown rows read `other`).
    let timesfile = "/tmp/t-caps-times";
    // Three more fixtures for the set-id bits' *other* door: changing a file's
    // bytes costs it the two bits, and `CAP_FSETID` is the only thing that
    // spares them (`.workbuddy/probe_suid_write.c`). One carries both bits with
    // group execute (the ordinary case), one a set-group-id with no group
    // execute (a locking mark, which nobody takes), and one is the resize door.
    let wbits = "/tmp/t-caps-wbits";
    let wmark = "/tmp/t-caps-wmark";
    let wtrunc = "/tmp/t-caps-wtrunc";
    // The fourth byte-door fixture: `fallocate` lays out blocks rather than
    // writing bytes, and it costs the file the same two bits - measured on a
    // current host (`.workbuddy/probe_fallocate_mode.c` A against H).
    let wfalloc = "/tmp/t-caps-wfalloc";
    // The four fixtures the set-id rows below turn on, each in a group this
    // caller is not in: a file carrying both bits, a file whose set-group-id
    // carries no group execute (a locking mark rather than an authority), a
    // file whose set-uid carries no execute at all, and a directory, where
    // neither bit means an authority. What a `chown` takes away is *not* a
    // capability's question, so these four shapes are how the clauses are told
    // apart (`.workbuddy/probe_chown_bits.c`).
    let bits = "/tmp/t-caps-bits";
    let bitsmark = "/tmp/t-caps-bitsmark";
    let bitssuid = "/tmp/t-caps-bitssuid";
    let bitsdir = "/tmp/t-caps-bitsdir";
    for path in [
        other, mine, minegid, timesfile, bits, bitsmark, bitssuid, wbits, wmark, wtrunc, wfalloc,
    ] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(bitsdir);
    let made = std::fs::File::create(other).is_ok()
        && std::fs::File::create(mine).is_ok()
        && std::fs::File::create(minegid).is_ok()
        && std::fs::File::create(timesfile).is_ok()
        && std::fs::File::create(wbits).is_ok()
        && std::fs::File::create(wmark).is_ok()
        && std::fs::File::create(wtrunc).is_ok()
        && std::fs::File::create(wfalloc).is_ok()
        && std::fs::File::create(bits).is_ok()
        && std::fs::File::create(bitsmark).is_ok()
        && std::fs::File::create(bitssuid).is_ok()
        && std::fs::create_dir_all(bitsdir).is_ok();
    // The owner is set first and the mode after, because a `chown` clears the
    // set-id bits - and `minegid` exists to have one to lose. `minegid` is this
    // caller's own file in a group it is *not* in, which is the only shape the
    // set-group-id row can turn on.
    let owned = made
        && std::fs::set_permissions(other, std::fs::Permissions::from_mode(0o644)).is_ok()
        && std::fs::set_permissions(mine, std::fs::Permissions::from_mode(0o644)).is_ok()
        && std::fs::set_permissions(minegid, std::fs::Permissions::from_mode(0o644)).is_ok()
        && chown_errno(other, 1000, 1000) == 0
        && chown_errno(timesfile, 1000, 1000) == 0
        && chown_errno(minegid, 0, 3000) == 0
        && chown_errno(bits, 0, 3000) == 0
        && chown_errno(bitsmark, 0, 3000) == 0
        && chown_errno(bitssuid, 0, 3000) == 0
        && chown_errno(bitsdir, 0, 3000) == 0
        && std::fs::set_permissions(bits, std::fs::Permissions::from_mode(0o6755)).is_ok()
        && std::fs::set_permissions(bitsmark, std::fs::Permissions::from_mode(0o6644)).is_ok()
        && std::fs::set_permissions(bitssuid, std::fs::Permissions::from_mode(0o4644)).is_ok()
        && std::fs::set_permissions(bitsdir, std::fs::Permissions::from_mode(0o6755)).is_ok()
        && std::fs::set_permissions(wbits, std::fs::Permissions::from_mode(0o4755)).is_ok()
        && std::fs::set_permissions(wmark, std::fs::Permissions::from_mode(0o2644)).is_ok()
        && std::fs::set_permissions(wtrunc, std::fs::Permissions::from_mode(0o4755)).is_ok()
        && std::fs::set_permissions(wfalloc, std::fs::Permissions::from_mode(0o4755)).is_ok();
    r.check("another user's file, and two of this caller's", owned);

    // What a row leaves behind, put back while the whole set is held: the rows
    // that are *served* change the fixtures, and a row measured against the
    // last one's residue would be measuring the wrong file.
    let restore = || {
        let _ = std::fs::set_permissions(other, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::set_permissions(mine, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::set_permissions(minegid, std::fs::Permissions::from_mode(0o644));
        let _ = chown_errno(other, 1000, 1000);
        let _ = chown_errno(timesfile, 1000, 1000);
        let _ = chown_errno(minegid, 0, 3000);
        // The set-id fixtures: the owner before the mode, for the same reason
        // they were made that way.
        let _ = chown_errno(bits, 0, 3000);
        let _ = chown_errno(bitsmark, 0, 3000);
        let _ = chown_errno(bitssuid, 0, 3000);
        let _ = chown_errno(bitsdir, 0, 3000);
        let _ = std::fs::set_permissions(bits, std::fs::Permissions::from_mode(0o6755));
        let _ = std::fs::set_permissions(bitsmark, std::fs::Permissions::from_mode(0o6644));
        let _ = std::fs::set_permissions(bitssuid, std::fs::Permissions::from_mode(0o4644));
        let _ = std::fs::set_permissions(bitsdir, std::fs::Permissions::from_mode(0o6755));
        let _ = std::fs::set_permissions(wbits, std::fs::Permissions::from_mode(0o4755));
        let _ = std::fs::set_permissions(wmark, std::fs::Permissions::from_mode(0o2644));
        let _ = std::fs::set_permissions(wtrunc, std::fs::Permissions::from_mode(0o4755));
        let _ = std::fs::set_permissions(wfalloc, std::fs::Permissions::from_mode(0o4755));
    };
    let explicit = [libc::timespec {
        tv_sec: 1_000_000_000,
        tv_nsec: 0,
    }; 2];

    struct OwnerAnswers {
        foreign_chmod: i32,
        own_chmod: i32,
        setgid_call: i32,
        setgid_stored: u32,
        named_uid: i32,
        own_uid: i32,
        explicit_times: i32,
        now_times: i32,
        strip_file: u32,
        strip_mark: u32,
        strip_suid: u32,
        strip_dir: u32,
        write_bits: u32,
        write_mark: u32,
        trunc_bits: u32,
        alloc_bits: u32,
    }

    // One row: the effective set with `clear` lowered, the calls the
    // measurement makes, and the whole set put back.
    let mut own_row = |r: &mut Report, label: &str, clear: &[u32], whole: &[CapRecord; 2]| {
        let mut data = *whole;
        for &cap in clear {
            data[0].effective &= !bit_of(cap);
        }
        let lowered = capset(&mut header, &mut data) == 0;
        r.check(&format!("{label}: the set is lowered"), lowered);
        let foreign_chmod = chmod_errno(other, 0o600);
        let own_chmod = chmod_errno(mine, 0o640);
        let setgid_call = chmod_errno(minegid, 0o2755);
        let setgid_stored = mode_of(minegid);
        let named_uid = chown_errno(other, 0, u32::MAX);
        let own_uid = chown_errno(mine, 0, u32::MAX);
        // The two timestamps forms, on the fixture no `chown` row has touched.
        let explicit_times = utimensat_errno(timesfile, Some(&explicit));
        let now_times = utimensat_errno(timesfile, None);
        // What a `chown` takes away. Naming nothing at all - `chown(path, -1,
        // -1)` - is the purest form of the question: no id is named and none is
        // moved, so whatever happens to the bits is the chown's own doing.
        let strip_file = {
            let _ = chown_errno(bits, u32::MAX, u32::MAX);
            mode_of(bits)
        };
        let strip_mark = {
            let _ = chown_errno(bitsmark, u32::MAX, u32::MAX);
            mode_of(bitsmark)
        };
        let strip_suid = {
            let _ = chown_errno(bitssuid, u32::MAX, u32::MAX);
            mode_of(bitssuid)
        };
        let strip_dir = {
            let _ = chown_errno(bitsdir, u32::MAX, u32::MAX);
            mode_of(bitsdir)
        };
        // The other door: the file's bytes. A write that writes a byte, a write
        // to the locking-mark shape, and a resize - each of which costs the file
        // the same two bits unless `CAP_FSETID` is held.
        let write_bits = write_one(wbits);
        let write_mark = write_one(wmark);
        let trunc_bits = truncate_it(wtrunc);
        // The third byte-door question: an allocation that changes the size
        // without writing a byte.
        let alloc_bits = fallocate_it(wfalloc);
        let mut back = *whole;
        capset(&mut header, &mut back);
        restore();
        OwnerAnswers {
            foreign_chmod,
            own_chmod,
            setgid_call,
            setgid_stored,
            named_uid,
            own_uid,
            explicit_times,
            now_times,
            strip_file,
            strip_mark,
            strip_suid,
            strip_dir,
            write_bits,
            write_mark,
            trunc_bits,
            alloc_bits,
        }
    };
    let whole_set = own_row(&mut r, "the whole set", &[], &whole);
    r.check(
        &format!(
            "whole set: another user's file is chmod'able (got {})",
            whole_set.foreign_chmod
        ),
        whole_set.foreign_chmod == 0,
    );
    r.check(
        &format!(
            "whole set: 2755 on the caller's own file is stored (got {} / {:04o})",
            whole_set.setgid_call, whole_set.setgid_stored
        ),
        whole_set.setgid_call == 0 && whole_set.setgid_stored == 0o2755,
    );
    r.check(
        &format!(
            "whole set: explicit times on another's file are served (got {})",
            whole_set.explicit_times
        ),
        whole_set.explicit_times == 0,
    );
    // A `chown` naming nothing at all still takes the two set-id bits away: not
    // one of the rows below is allowed to buy them back, and the four shapes
    // are the clauses - both bits on a file, a set-group-id with no group
    // execute (a locking mark, which stays), a set-uid with no execute at all
    // (which goes anyway), and a directory (where both stay).
    r.check(
        &format!(
            "whole set: a chown naming nothing takes both set-id bits (got {:04o})",
            whole_set.strip_file
        ),
        whole_set.strip_file == 0o0755,
    );
    r.check(
        &format!(
            "whole set: it leaves a set-group-id that carries no group execute (got {:04o})",
            whole_set.strip_mark
        ),
        whole_set.strip_mark == 0o2644,
    );
    r.check(
        &format!(
            "whole set: it takes the set-uid off a file with no execute at all (got {:04o})",
            whole_set.strip_suid
        ),
        whole_set.strip_suid == 0o0644,
    );
    r.check(
        &format!(
            "whole set: and it leaves a directory's bits alone (got {:04o})",
            whole_set.strip_dir
        ),
        whole_set.strip_dir == 0o6755,
    );
    // The same two bits, taken by the file's **bytes** instead of by a `chown`:
    // a write and a resize each cost them, and `CAP_FSETID` is the only thing
    // that spares them - a set-group-id with no group execute is a locking mark
    // and is nobody's to take either way.
    r.check(
        &format!(
            "whole set: a write leaves a 4755 file's bits alone (got {:04o})",
            whole_set.write_bits
        ),
        whole_set.write_bits == 0o4755,
    );
    r.check(
        &format!(
            "whole set: and a resize leaves them alone (got {:04o})",
            whole_set.trunc_bits
        ),
        whole_set.trunc_bits == 0o4755,
    );
    r.check(
        &format!(
            "whole set: and so does an allocation (got {:04o})",
            whole_set.alloc_bits
        ),
        whole_set.alloc_bits == 0o4755,
    );
    let no_fowner = own_row(&mut r, "CAP_FOWNER dropped", &[3], &whole);
    r.check(
        &format!(
            "FOWNER dropped: another user's file is refused the chmod (got {})",
            no_fowner.foreign_chmod
        ),
        no_fowner.foreign_chmod == libc::EPERM,
    );
    r.check(
        &format!(
            "FOWNER dropped: the caller's own file is still served (got {})",
            no_fowner.own_chmod
        ),
        no_fowner.own_chmod == 0,
    );
    r.check(
        &format!(
            "FOWNER dropped: an explicit pair of times is refused (got {})",
            no_fowner.explicit_times
        ),
        no_fowner.explicit_times == libc::EPERM,
    );
    r.check(
        &format!(
            "FOWNER dropped: the now form still answers, the DAC override's (got {})",
            no_fowner.now_times
        ),
        no_fowner.now_times == 0,
    );
    r.check(
        &format!(
            "FOWNER dropped: the chown still takes both set-id bits (got {:04o})",
            no_fowner.strip_file
        ),
        no_fowner.strip_file == 0o0755,
    );
    // The control for the door: this one is `CAP_FSETID`'s and not
    // `CAP_FOWNER`'s, so a caller that dropped the latter still keeps the bits
    // its write would otherwise have cost it.
    r.check(
        &format!(
            "FOWNER dropped: a write still keeps the set-id bits (got {:04o})",
            no_fowner.write_bits
        ),
        no_fowner.write_bits == 0o4755,
    );
    let no_fsetid = own_row(&mut r, "CAP_FSETID dropped", &[4], &whole);
    r.check(
        &format!(
            "FSETID dropped: 2755 comes back storing 0755 (got {} / {:04o})",
            no_fsetid.setgid_call, no_fsetid.setgid_stored
        ),
        no_fsetid.setgid_call == 0 && no_fsetid.setgid_stored == 0o755,
    );
    r.check(
        &format!(
            "FSETID dropped: the chown still takes both set-id bits (got {:04o})",
            no_fsetid.strip_file
        ),
        no_fsetid.strip_file == 0o0755,
    );
    r.check(
        &format!(
            "FSETID dropped: and still leaves the locking mark (got {:04o})",
            no_fsetid.strip_mark
        ),
        no_fsetid.strip_mark == 0o2644,
    );
    r.check(
        &format!(
            "FSETID dropped: a write costs a 4755 file both set-id bits (got {:04o})",
            no_fsetid.write_bits
        ),
        no_fsetid.write_bits == 0o0755,
    );
    r.check(
        &format!(
            "FSETID dropped: and so does a resize (got {:04o})",
            no_fsetid.trunc_bits
        ),
        no_fsetid.trunc_bits == 0o0755,
    );
    r.check(
        &format!(
            "FSETID dropped: an allocation costs the same two bits (got {:04o})",
            no_fsetid.alloc_bits
        ),
        no_fsetid.alloc_bits == 0o0755,
    );
    r.check(
        &format!(
            "FSETID dropped: the locking mark is nobody's to take (got {:04o})",
            no_fsetid.write_mark
        ),
        no_fsetid.write_mark == 0o2644,
    );
    let no_chown = own_row(&mut r, "CAP_CHOWN dropped", &[0], &whole);
    r.check(
        &format!(
            "CHOWN dropped: a chown naming a uid is refused (got {})",
            no_chown.named_uid
        ),
        no_chown.named_uid == libc::EPERM,
    );
    r.check(
        &format!(
            "CHOWN dropped: the owner's own uid is still served (got {})",
            no_chown.own_uid
        ),
        no_chown.own_uid == 0,
    );
    r.check(
        &format!(
            "CHOWN dropped: the chown still takes both set-id bits (got {:04o})",
            no_chown.strip_file
        ),
        no_chown.strip_file == 0o0755,
    );

    for path in [
        other, mine, minegid, timesfile, bits, bitsmark, bitssuid, wbits, wmark, wtrunc, wfalloc,
    ] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(bitsdir);
    for path in [zero, four, sixfour] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(sticky);

    r.finish()
}
