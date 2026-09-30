//! t-ns-uts - a name that belongs to a *task*, not to the machine.
//!
//! `uname(2)`'s `nodename` and `domainname` are the first namespace this kernel
//! serves (`PLAN.md` §6.3, step 1 of UTS → MNT → PID → NET). What makes a
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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Long enough for the longest name either `/proc/sys` file may hold (64) plus
/// the newline every sysctl ends in.
const NAME_MAX: usize = 96;

const HOSTNAME_PATH: &[u8] = b"/proc/sys/kernel/hostname\0";
const DOMAIN_PATH: &[u8] = b"/proc/sys/kernel/domainname\0";

/// What every report pipe in this test carries: the three doors of the
/// reporting namespace, one newline-terminated line each.
const INSIDE_REPORT: &[u8] = b"inside\ninside.test\ninside\n";

fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Open `path` and read it whole into `buf`; the answer is the byte count, or
/// the negative errno of whichever call failed.
fn read_small(path: &[u8], buf: &mut [u8]) -> isize {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0);
    if fd < 0 {
        return fd;
    }
    let n = lin::read(fd as i32, buf);
    lin::close(fd as i32);
    n
}

/// Write `bytes` to `path`, answering the byte count or the negative errno -
/// from the open *or* from the write. They are the two doors Linux has for this
/// too, and a node whose bits refuse the open and a node whose policy refuses
/// the write both answer `EACCES`, so the caller cannot tell them apart and
/// this test does not have to.
fn write_small(path: &[u8], bytes: &[u8]) -> isize {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_WRONLY, 0);
    if fd < 0 {
        return fd;
    }
    let n = lin::write(fd as i32, bytes);
    lin::close(fd as i32);
    n
}

/// Read a pipe to end-of-file, answering the byte count.
fn drain(fd: i32, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        let n = lin::read(fd, &mut buf[total..]);
        if n <= 0 {
            break;
        }
        total += n as usize;
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

/// The three doors onto one namespace, read at one moment: `uname(2)`'s
/// `nodename` and `domainname`, and the two `/proc/sys/kernel` files.
///
/// They are read together because the property is that they agree - a namespace
/// is not the hostname, it is everything that reports the hostname - and an
/// assertion spread over four separate reads can pass while the four disagree.
struct Doors {
    node: [u8; NAME_MAX],
    node_len: usize,
    domain: [u8; NAME_MAX],
    domain_len: usize,
    host_file: [u8; NAME_MAX],
    host_file_len: usize,
    domain_file: [u8; NAME_MAX],
    domain_file_len: usize,
}

impl Doors {
    /// Read all four, from whatever namespace the caller is in now.
    fn read(failed: &mut bool) -> Doors {
        let mut uts = lin::Utsname::new();
        lin::check_eq(failed, b"uname succeeds", lin::uname(&mut uts), 0);
        let mut doors = Doors {
            node: [0; NAME_MAX],
            node_len: uts.field(1).len(),
            domain: [0; NAME_MAX],
            domain_len: uts.field(5).len(),
            host_file: [0; NAME_MAX],
            host_file_len: 0,
            domain_file: [0; NAME_MAX],
            domain_file_len: 0,
        };
        doors.node[..doors.node_len].copy_from_slice(uts.field(1));
        doors.domain[..doors.domain_len].copy_from_slice(uts.field(5));
        let n = read_small(HOSTNAME_PATH, &mut doors.host_file);
        lin::check_true(failed, b"/proc/sys/kernel/hostname reads", n > 0);
        doors.host_file_len = n.max(0) as usize;
        let n = read_small(DOMAIN_PATH, &mut doors.domain_file);
        lin::check_true(failed, b"/proc/sys/kernel/domainname reads", n > 0);
        doors.domain_file_len = n.max(0) as usize;
        doors
    }

    fn node(&self) -> &[u8] {
        &self.node[..self.node_len]
    }

    fn domain(&self) -> &[u8] {
        &self.domain[..self.domain_len]
    }

    fn host_file_name(&self) -> &[u8] {
        without_newline(&self.host_file[..self.host_file_len])
    }

    fn domain_file_name(&self) -> &[u8] {
        without_newline(&self.domain_file[..self.domain_file_len])
    }

    /// Whether the three doors tell one story, which is what they have to do
    /// whether or not anybody has unshared.
    fn agree(&self) -> bool {
        eq_bytes(self.node(), self.host_file_name())
            && eq_bytes(self.domain(), self.domain_file_name())
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-ns-uts\n");
    let mut failed = false;

    // ---- The three doors agree in the namespace this process inherited ----
    let machine = Doors::read(&mut failed);
    lin::check_true(
        &mut failed,
        b"uname and the two sysctls agree about the boot's own namespace",
        machine.agree(),
    );

    // ---- A child that unshares reports a different name, and this one does
    //      not ----
    //
    // The child writes its three doors down the pipe rather than checking them
    // itself, so the parent compares two *observations* instead of trusting a
    // child's verdict about its own view. The three lines are what a namespace
    // that never diverged cannot produce and a shared global cannot either.
    let mut child_report = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"the report pipe is made",
        lin::pipe2(&mut child_report, 0),
        0,
    );
    let child = lin::fork();
    if child == 0 {
        // The child: its own namespace, or nothing else below is safe.
        lin::close(child_report[0]);
        let r = lin::unshare(lin::nr::CLONE_NEWUTS);
        if r != 0 {
            // Renaming now would rename the *machine's* namespace, which every
            // other process in this image shares. Say so and stop instead.
            lin::write_all(child_report[1], b"!unshare\n");
            lin::close(child_report[1]);
            lin::exit_group(1);
        }
        let _ = lin::sethostname(b"inside");
        let _ = lin::setdomainname(b"inside.test");
        let mut mine = lin::Utsname::new();
        lin::uname(&mut mine);
        lin::write_all(child_report[1], mine.field(1));
        lin::write_all(child_report[1], b"\n");
        lin::write_all(child_report[1], mine.field(5));
        lin::write_all(child_report[1], b"\n");
        let mut buf = [0u8; NAME_MAX];
        let n = read_small(HOSTNAME_PATH, &mut buf);
        if n > 0 {
            lin::write_all(child_report[1], &buf[..n as usize]);
        }
        lin::close(child_report[1]);
        lin::exit_group(0);
    }
    lin::close(child_report[1]);
    lin::check_true(
        &mut failed,
        b"the child that unshares was forked",
        child > 0,
    );
    if child > 0 {
        let mut report = [0u8; NAME_MAX];
        let n = drain(child_report[0], &mut report);
        lin::close(child_report[0]);
        lin::check_true(
            &mut failed,
            b"the unshared child reports its own name through all three doors",
            eq_bytes(&report[..n], INSIDE_REPORT),
        );
        let mut status = 0i32;
        lin::check_true(
            &mut failed,
            b"the child exits cleanly",
            lin::waitpid(child as usize, &mut status, 0) == child && status == 0,
        );
        // ...and the namespace it came from did not follow.
        let after = Doors::read(&mut failed);
        lin::check_true(
            &mut failed,
            b"the parent that did not unshare still reports the old name",
            eq_bytes(after.node(), machine.node()) && eq_bytes(after.domain(), machine.domain()),
        );
        lin::check_true(
            &mut failed,
            b"and its two sysctls still agree with its uname",
            after.agree(),
        );
    }

    // ---- Unsharing in place keeps the names, and then names only this task ----
    //
    // Everything below writes to a namespace this process owns. That is what
    // makes it safe to run in a test at all: the boot's namespace is shared with
    // every other program in the image.
    let r = lin::unshare(lin::nr::CLONE_NEWUTS);
    lin::check_eq(
        &mut failed,
        b"unshare(CLONE_NEWUTS) succeeds for root",
        r,
        0,
    );
    if r != 0 {
        lin::puts(b"t-ns-uts: skipping the in-place checks; the call they need failed\n");
        lin::done(failed);
    }
    let copied = Doors::read(&mut failed);
    lin::check_true(
        &mut failed,
        b"the new namespace starts from the names it was copied from",
        eq_bytes(copied.node(), machine.node()) && eq_bytes(copied.domain(), machine.domain()),
    );

    lin::check_eq(
        &mut failed,
        b"sethostname names the new namespace",
        lin::sethostname(b"ns-outer"),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setdomainname names its domain",
        lin::setdomainname(b"ns-outer.test"),
        0,
    );
    let named = Doors::read(&mut failed);
    lin::check_true(
        &mut failed,
        b"all three doors report the new name",
        eq_bytes(named.node(), b"ns-outer") && eq_bytes(named.domain(), b"ns-outer.test"),
    );
    lin::check_true(
        &mut failed,
        b"and they agree with each other",
        named.agree(),
    );

    // ---- A word that mixes a served namespace with an unserved one unshares
    //      nothing ----
    //
    // The discriminator is a task that was in *this* namespace before the call.
    // Had the served bit been applied before the unserved one was refused - which
    // is what Linux's `ksys_unshare` does, and this kernel deliberately does not
    // - the rename below would land in a copy the child does not hold, and the
    // child would report the old name. It reports the new one, so parent and
    // child are still one namespace.
    let mut go = [-1i32; 2];
    let mut witness_report = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"the witness pipes are made",
        lin::pipe2(&mut go, 0) + lin::pipe2(&mut witness_report, 0),
        0,
    );
    let witness = lin::fork();
    if witness == 0 {
        lin::close(go[1]);
        lin::close(witness_report[0]);
        let mut byte = [0u8; 1];
        // Park until the parent has made its calls: what this child reports is
        // the name *after* them, which is the whole point.
        let _ = lin::read(go[0], &mut byte);
        let mut mine = lin::Utsname::new();
        lin::uname(&mut mine);
        lin::write_all(witness_report[1], mine.field(1));
        lin::write_all(witness_report[1], b"\n");
        lin::close(witness_report[1]);
        lin::exit_group(0);
    }
    lin::close(go[0]);
    lin::close(witness_report[1]);
    lin::check_true(
        &mut failed,
        b"the witness that holds the old namespace was forked",
        witness > 0,
    );
    if witness > 0 {
        lin::check_eq(
            &mut failed,
            b"a word mixing CLONE_NEWUTS with CLONE_NEWNS is EOPNOTSUPP",
            lin::unshare(lin::nr::CLONE_NEWUTS | lin::nr::CLONE_NEWNS),
            lin::EOPNOTSUPP,
        );
        // Only now, in a namespace that must still be the one the witness
        // holds.
        lin::check_eq(
            &mut failed,
            b"and naming this namespace still works",
            lin::sethostname(b"ns-after-refusal"),
            0,
        );
        let _ = lin::write(go[1], b"g");
        lin::close(go[1]);
        let mut report = [0u8; NAME_MAX];
        let n = drain(witness_report[0], &mut report);
        lin::close(witness_report[0]);
        lin::check_true(
            &mut failed,
            b"so the witness, forked before the refusal, sees the rename - nothing was unshared",
            eq_bytes(&report[..n], b"ns-after-refusal\n"),
        );
        let mut status = 0i32;
        let _ = lin::waitpid(witness as usize, &mut status, 0);
    }

    // ---- The capability gate, and its order ----
    //
    // `setresuid(-1, 65534, -1)` lowers only the effective id, so the privilege
    // is reclaimable - the same shape the entropy-ioctl checks use, and the same
    // capability they are gated on. Every one of these then answers EPERM, and
    // the name does not move: the callers that were refused did not half-apply.
    lin::check_eq(
        &mut failed,
        b"setresuid lowers euid and leaves the saved id at 0",
        lin::setresuid(-1, 65534, -1),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"unshare(CLONE_NEWUTS) without CAP_SYS_ADMIN is EPERM",
        lin::unshare(lin::nr::CLONE_NEWUTS),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"sethostname without the capability is EPERM",
        lin::sethostname(b"nope"),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"setdomainname without the capability is EPERM",
        lin::setdomainname(b"nope"),
        lin::EPERM,
    );
    lin::check_eq(
        &mut failed,
        b"a write to the sysctl is EACCES, the node's own answer",
        write_small(HOSTNAME_PATH, b"nope"),
        lin::EACCES,
    );
    lin::check_eq(
        &mut failed,
        b"the effective id is restored",
        lin::setresuid(-1, 0, -1),
        0,
    );
    let after = Doors::read(&mut failed);
    lin::check_true(
        &mut failed,
        b"none of the refused calls moved the name",
        eq_bytes(after.node(), b"ns-after-refusal") && after.agree(),
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-ns-uts: panicked\n")
}
