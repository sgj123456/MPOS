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
//! calls work again. `unshare(CLONE_NEWNS)` keeps its `EOPNOTSUPP` the whole
//! time - this kernel's refuse-rather-than-approximate answer, which no
//! capability can turn into a mount namespace - and `CLONE_NEWUTS`, the one
//! served kind, splits exactly when the capability comes back.
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

use mpos_user_tests::{Report, host_run};

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
    // The served-kind ordering: NEWUTS is served, so a capability-less
    // caller gets Linux's EPERM; NEWNS is not served at all, so the
    // refusal is EOPNOTSUPP whatever the caller holds - the
    // refuse-rather-than-approximate rule, which no capability buys.
    let mount = unshare(0x0002_0000);
    r.check(
        "unshare CLONE_NEWNS is EOPNOTSUPP even lowered (refused, not gated)",
        mount == -(libc::EOPNOTSUPP as isize),
    );
    let noop = unshare(0);
    r.check("unshare(0) is still a no-op", noop == 0);

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
    // the one positive answer in the set, and the difference between a gate
    // and a refusal.
    let uts = unshare(0x0400_0000);
    r.check("unshare CLONE_NEWUTS is served again", uts == 0);

    r.finish()
}
