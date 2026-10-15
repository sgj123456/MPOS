//! t-ns-uts - a name that belongs to a *task*, not to the machine.
//!
//! `uname(2)`'s `nodename` and `domainname` are the first namespace this kernel
//! serves (`NOTES.md` §6.3, step 1 of UTS → MNT → PID → NET). What makes a
//! namespace a namespace is not that a call sets a name - `sethostname(2)`
//! always did that - but that the name is *somebody's*: a task that asks for its
//! own gets a copy, keeps reporting what it reported, and can then change it
//! without the task it came from following.
//!
//! Every check here is a line of that sentence, and the shape of each pair of
//! assertions is the same - **the child's view differs and the other view does
//! not** - because a "namespace" that is really a shared global passes the first
//! half of every one of them:
//!
//! * a forked child that unshares reports `inside` through `uname`, through
//!   `/proc/sys/kernel/hostname`, and through its domain's pair, while the
//!   parent - which has *not* unshared - still reports the names it started
//!   with;
//! * a task that unshares keeps the names it had (Linux's `copy_utsname`), so
//!   the call is an isolation and not a reset;
//! * a word mixing the served namespace with an unserved one is refused
//!   `EOPNOTSUPP` **and unshares nothing**, which is a pinned property and not
//!   an argument: a child forked before the call goes on sharing the parent's
//!   namespace, so a rename the parent makes afterwards is a rename the child
//!   sees;
//! * the capability gate and its order: `unshare(CLONE_NEWUTS)`,
//!   `sethostname(2)`, `setdomainname(2)` and a write to the sysctl file are all
//!   refused once the effective uid is lowered, and the *name does not move*.
//!
//! The machine's own namespace is never renamed. The first forked child
//! unshares before it names anything, and everything after that happens in a
//! namespace this process asked for itself - which matters, because this
//! program shares the boot's namespace with every other test in the image and a
//! stray rename would be somebody else's failing check.
//!
//! # The migration
//!
//! `rustix::thread::{unshare_unsafe, UnshareFlags}`,
//! `rustix::system::{sethostname, setdomainname}` and `rustix::process::fork`
//! carry the calls. Two things cannot come from a library wrapper at all, and
//! both are clause-(c) shapes rather than missing bindings:
//!
//! * **the report.** The no-std version wrote raw bytes down a pipe from a
//!   forked child and compared them in the parent. std owns stdout's buffer
//!   across a fork (a flushed child line would duplicate the parent's), so the
//!   child writes through `rustix::io::write` into a `rustix::pipe::pipe` and
//!   leaves through the raw `SYS_exit` door - not `exit_group`, which would run
//!   the inherited atexit list.
//! * **`uname`'s fields.** `rustix::system::uname` answers a `Utsname` whose
//!   fields are `&CStr`; reading them is fine, but the no-std harness compared
//!   *bytes*, and so does this - the same strings, taken through the CStr.
//!
//! The gate section lowers the effective uid with `libc::setresuid(-1, 65534,
//! -1)`: rustix binds `seteuid`, which changes the real id too on Linux's
//! semantics of that call, and the claim is specifically *effective-only* -
//! the reclaimable shape the entropy-ioctl checks use.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe;
use rustix::process::WaitOptions;
use rustix::system::{setdomainname, sethostname, uname};
use rustix::thread::{UnshareFlags, unshare_unsafe};

/// Long enough for the longest name either `/proc/sys` file may hold (64) plus
/// the newline every sysctl ends in.
const NAME_MAX: usize = 96;

const HOSTNAME_PATH: &str = "/proc/sys/kernel/hostname";
const DOMAIN_PATH: &str = "/proc/sys/kernel/domainname";

/// What every report pipe in this test carries: the three doors of the
/// reporting namespace, one newline-terminated line each.
const INSIDE_REPORT: &[u8] = b"inside\ninside.test\ninside\n";

fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Open `path` and read it whole into `buf`; the answer is the byte count, or
/// the negative errno of whichever call failed.
fn read_small(path: &str, buf: &mut [u8]) -> isize {
    let fd = match openat(CWD, path, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return -e.raw_os_error() as isize,
    };
    match read(&fd, buf) {
        Ok(n) => n as isize,
        Err(e) => -e.raw_os_error() as isize,
    }
}

/// Write `bytes` to `path`, answering the byte count or the negative errno -
/// from the open *or* from the write. They are the two doors Linux has for this
/// too, and a node whose bits refuse the open and a node whose policy refuses
/// the write both answer `EACCES`, so the caller cannot tell them apart and
/// this test does not have to.
fn write_small(path: &str, bytes: &[u8]) -> isize {
    let fd = match openat(CWD, path, OFlags::WRONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return -e.raw_os_error() as isize,
    };
    match write(&fd, bytes) {
        Ok(n) => n as isize,
        Err(e) => -e.raw_os_error() as isize,
    }
}

/// Read a pipe to end-of-file, answering the byte count.
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

/// The newline a sysctl's read ends in, taken off. The files are the
/// `sysctl(8)` format - value, newline - and the *name* is what stands before
/// it, which is also how the write side reads them.
fn without_newline(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == b'\n') {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

/// The uts field as bytes: the CStr's contents without its terminator.
fn cstr_field(s: &std::ffi::CStr) -> Vec<u8> {
    s.to_bytes().to_vec()
}

/// The three doors onto one namespace, read at one moment: `uname(2)`'s
/// `nodename` and `domainname`, and the two `/proc/sys/kernel` files.
///
/// They are read together because the property is that they agree - a namespace
/// is not the hostname, it is everything that reports the hostname - and an
/// assertion spread over four separate reads can pass while the four disagree.
struct Doors {
    node: Vec<u8>,
    domain: Vec<u8>,
    host_file: Vec<u8>,
    domain_file: Vec<u8>,
}

impl Doors {
    /// Read all four, from whatever namespace the caller is in now.
    fn read(failed: &mut Report) -> Doors {
        let u = uname();
        let mut host_file = vec![0u8; NAME_MAX];
        let n = read_small(HOSTNAME_PATH, &mut host_file);
        failed.check("/proc/sys/kernel/hostname reads", n > 0);
        host_file.truncate(n.max(0) as usize);
        let mut domain_file = vec![0u8; NAME_MAX];
        let n = read_small(DOMAIN_PATH, &mut domain_file);
        failed.check("/proc/sys/kernel/domainname reads", n > 0);
        domain_file.truncate(n.max(0) as usize);
        Doors {
            node: cstr_field(u.nodename()),
            domain: cstr_field(u.domainname()),
            host_file,
            domain_file,
        }
    }

    fn host_file_name(&self) -> &[u8] {
        without_newline(&self.host_file)
    }

    fn domain_file_name(&self) -> &[u8] {
        without_newline(&self.domain_file)
    }

    /// Whether the three doors tell one story, which is what they have to do
    /// whether or not anybody has unshared.
    fn agree(&self) -> bool {
        eq_bytes(&self.node, self.host_file_name())
            && eq_bytes(&self.domain, self.domain_file_name())
    }
}

/// `unshare(CLONE_*)` through rustix. The unsafe is rustix's own honesty about
/// `CLONE_FILES` invalidating other threads' descriptors; this test never
/// unshares a file table, so the precondition holds by construction.
fn unshare(flags: UnshareFlags) -> Result<(), Errno> {
    unsafe { unshare_unsafe(flags) }
}

/// Leave the child without running any atexit handler (see the header).
fn raw_exit(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit, code as libc::c_long);
        // The door does not return; `libc::_exit` is the maintained fallback
        // spelling of the same word.
        libc::_exit(code)
    }
}

fn main() {
    println!("t-ns-uts");
    let mut r = Report::new();

    // ---- The three doors agree in the namespace this process inherited ----
    let machine = Doors::read(&mut r);

    // A host that cannot grant the namespace cannot run any of what follows,
    // and the refusal is the *host's* policy, not this kernel's answer
    // (measured: unprivileged Docker namespaces are EPERM by default). The
    // payload runs as root; skipping says so instead of reporting red.
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: this host user cannot unshare; the payload runs as root");
        r.finish()
    }

    // ---- A child that unshares reports a different name, and this one does
    //      not ----
    //
    // The child writes its three doors down the pipe rather than checking them
    // itself, so the parent compares two *observations* instead of trusting a
    // child's verdict about its own view. The three lines are what a namespace
    // that never diverged cannot produce and a shared global cannot either.
    let pipes = pipe();
    r.check("the report pipe is made", pipes.is_ok());
    if !pipes.is_ok() {
        r.finish()
    }
    let (ro, wo) = pipes.unwrap();
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child: its own namespace, or nothing else below is safe.
        unsafe { libc::syscall(libc::SYS_close, ro.as_raw_fd() as libc::c_long) };
        let say = |s: &[u8]| {
            let _ = write(&wo, s);
        };
        if unshare(UnshareFlags::NEWUTS).is_err() {
            // Renaming now would rename the *machine's* namespace, which every
            // other process in this image shares. Say so and stop instead.
            say(b"!unshare\n");
            raw_exit(1);
        }
        let _ = sethostname(b"inside");
        let _ = setdomainname(b"inside.test");
        let mine = uname();
        say(&cstr_field(mine.nodename()));
        say(b"\n");
        say(&cstr_field(mine.domainname()));
        say(b"\n");
        let mut buf = [0u8; NAME_MAX];
        let n = read_small(HOSTNAME_PATH, &mut buf);
        if n > 0 {
            say(&buf[..n as usize]);
        }
        unsafe { libc::syscall(libc::SYS_close, wo.as_raw_fd() as libc::c_long) };
        raw_exit(0);
    }
    drop(wo);
    r.check("the child that unshares was forked", child > 0);
    if child > 0 {
        let mut report = [0u8; NAME_MAX];
        let n = drain(&ro, &mut report);
        drop(ro);
        r.check(
            "the unshared child reports its own name through all three doors",
            eq_bytes(&report[..n], INSIDE_REPORT),
        );
        let status =
            rustix::process::waitpid(rustix::process::Pid::from_raw(child), WaitOptions::empty());
        r.check(
            "the child exits cleanly",
            matches!(&status, Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)),
        );
        // ...and the namespace it came from did not follow.
        let after = Doors::read(&mut r);
        r.check(
            "the parent that did not unshare still reports the old name",
            eq_bytes(&after.node, &machine.node) && eq_bytes(&after.domain, &machine.domain),
        );
        r.check(
            "and its two sysctls still agree with its uname",
            after.agree(),
        );
    }

    // ---- Unsharing in place keeps the names, and then names only this task ----
    //
    // Everything below writes to a namespace this process owns. That is what
    // makes it safe to run in a test at all: the boot's namespace is shared with
    // every other program in the image.
    let unshared = unshare(UnshareFlags::NEWUTS);
    r.check("unshare(CLONE_NEWUTS) succeeds for root", unshared.is_ok());
    if unshared.is_err() {
        println!("t-ns-uts: skipping the in-place checks; the call they need failed");
        r.finish()
    }
    let copied = Doors::read(&mut r);
    r.check(
        "the new namespace starts from the names it was copied from",
        eq_bytes(&copied.node, &machine.node) && eq_bytes(&copied.domain, &machine.domain),
    );

    r.check(
        "sethostname names the new namespace",
        sethostname(b"ns-outer").is_ok(),
    );
    r.check(
        "setdomainname names its domain",
        setdomainname(b"ns-outer.test").is_ok(),
    );
    let named = Doors::read(&mut r);
    r.check(
        "all three doors report the new name",
        eq_bytes(&named.node, b"ns-outer") && eq_bytes(&named.domain, b"ns-outer.test"),
    );
    r.check("and they agree with each other", named.agree());

    // ---- A word that mixes a served namespace with an unserved one unshares
    //      nothing ----
    //
    // The discriminator is a task that was in *this* namespace before the call.
    // Had the served bit been applied before the unserved one was refused - which
    // is what Linux's `ksys_unshare` does, and this kernel deliberately does not
    // - the rename below would land in a copy the child does not hold, and the
    // child would report the old name. It reports the new one, so parent and
    // child are still one namespace.
    let go = pipe();
    let witness_report = pipe();
    r.check(
        "the witness pipes are made",
        go.is_ok() && witness_report.is_ok(),
    );
    if let (Ok((go_r, go_w)), Ok((wr_r, wr_w))) = (go, witness_report) {
        let witness = unsafe { libc::fork() };
        if witness == 0 {
            for fd in [go_w.as_raw_fd(), wr_r.as_raw_fd()] {
                unsafe { libc::syscall(libc::SYS_close, fd as libc::c_long) };
            }
            let mut byte = [0u8; 1];
            // Park until the parent has made its calls: what this child reports
            // is the name *after* them, which is the whole point.
            let _ = read(&go_r, &mut byte);
            let mine = uname();
            let mut line = cstr_field(mine.nodename());
            line.push(b'\n');
            let _ = write(&wr_w, &line);
            unsafe { libc::syscall(libc::SYS_close, wr_w.as_raw_fd() as libc::c_long) };
            raw_exit(0);
        }
        drop(go_r);
        drop(wr_w);
        r.check(
            "the witness that holds the old namespace was forked",
            witness > 0,
        );
        if witness > 0 {
            // The mixed word names one *served* kind and one this kernel has no
            // copy to hand over, which is the combination the refusal is about -
            // what this file's header has said all along ("the served namespace
            // with an unserved one"). The unserved half was `NEWNS` while MNT
            // was still unbuilt; MNT's landing made that pair legal, so the pair
            // moved to a kind still refused whole. Measured: with `NEWUTS |
            // NEWNS` the call started answering 0, the rename below landed in a
            // copy the witness does not hold, and the two claims that follow
            // were the ones that went red.
            r.check(
                // The unserved half is `IPC` because the word used to be `NET`,
                // which became a served kind when the fourth rung landed - the
                // drift §6.3 records over and over.
                "a word mixing CLONE_NEWUTS with CLONE_NEWIPC is EOPNOTSUPP",
                unshare(UnshareFlags::NEWUTS | UnshareFlags::NEWIPC) == Err(Errno::OPNOTSUPP),
            );
            // Only now, in a namespace that must still be the one the witness
            // holds.
            r.check(
                "and naming this namespace still works",
                sethostname(b"ns-after-refusal").is_ok(),
            );
            let _ = write(&go_w, b"g");
            drop(go_w);
            let mut report = [0u8; NAME_MAX];
            let n = drain(&wr_r, &mut report);
            drop(wr_r);
            r.check(
                "so the witness, forked before the refusal, sees the rename - nothing was unshared",
                eq_bytes(&report[..n], b"ns-after-refusal\n"),
            );
            let _ = rustix::process::waitpid(
                rustix::process::Pid::from_raw(witness),
                WaitOptions::empty(),
            );
        }
    }

    // ---- The capability gate, and its order ----
    //
    // `setresuid(-1, 65534, -1)` lowers only the effective id, so the privilege
    // is reclaimable - the same shape the entropy-ioctl checks use, and the same
    // capability they are gated on. Every one of these then answers EPERM, and
    // the name does not move: the callers that were refused did not half-apply.
    r.check(
        "setresuid lowers euid and leaves the saved id at 0",
        unsafe {
            libc::syscall(
                libc::SYS_setresuid,
                -1i64 as libc::c_long,
                65534u32 as libc::c_long,
                -1i64 as libc::c_long,
            )
        } == 0,
    );
    r.check(
        "unshare(CLONE_NEWUTS) without CAP_SYS_ADMIN is EPERM",
        unshare(UnshareFlags::NEWUTS) == Err(Errno::PERM),
    );
    r.check(
        "sethostname without the capability is EPERM",
        sethostname(b"nope") == Err(Errno::PERM),
    );
    r.check(
        "setdomainname without the capability is EPERM",
        setdomainname(b"nope") == Err(Errno::PERM),
    );
    r.check(
        "a write to the sysctl is EACCES, the node's own answer",
        write_small(HOSTNAME_PATH, b"nope") == -libc::EACCES as isize,
    );
    r.check(
        "the effective id is restored",
        unsafe {
            libc::syscall(
                libc::SYS_setresuid,
                -1i64 as libc::c_long,
                0u32 as libc::c_long,
                -1i64 as libc::c_long,
            )
        } == 0,
    );
    let after = Doors::read(&mut r);
    r.check(
        "none of the refused calls moved the name",
        eq_bytes(&after.node, b"ns-after-refusal") && after.agree(),
    );

    r.finish()
}
