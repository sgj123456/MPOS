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
    // unshared either way - the refusal is whole. This word has moved once per
    // rung built (`CLONE_NEWPID`, then `CLONE_NEWNET`): a hardcoded served set
    // drifts the moment it grows, which §6.3 records over and over.
    let ipc = unshare(0x0800_0000);
    r.check(
        "unshare CLONE_NEWIPC is EOPNOTSUPP once nothing else can be the reason",
        ipc == -(libc::EOPNOTSUPP as isize),
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

    for path in [zero, four, sixfour] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(sticky);

    r.finish()
}
