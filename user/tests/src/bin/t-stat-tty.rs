//! §6.129: `/proc/<pid>/stat`'s fields 6, 7 and 8 — `session`, `tty_nr` and
//! `tpgid` — were a `pgid` copy and two borrowed literals (`"0"` and `"-1"`),
//! on the reasoning that no `setsid` had ever been served and that everything
//! on this machine types at one console. The first half of that reasoning had
//! been false for a long time (`setsid` and `TIOCSCTTY` both work), and the
//! second is a claim about the machine, not about the file.
//!
//! What Linux answers, measured on 7.2.14 (`build/scratch/ttynr/b.c` and
//! `c.c`, which fork, `setsid`, open `/dev/pts/N` and `TIOCSCTTY` in the
//! child because a container's shell is already a process-group leader):
//!
//! ```text
//! no controlling terminal      tty_nr=0      tpgid=-1    pgrp=54880  session=54880
//! slave /dev/pts/1 rdev=0x8801 major=136 minor=1 -> tty_nr 34817
//! leader after setsid+TIOCSCTTY tty_nr=34817  tpgid=58868 pgrp=58868  session=58868
//! the grandchild (same tty)     tty_nr=34817  tpgid=58868 pgrp=58895  session=58868
//! after tcsetpgrp(grandchild)   tty_nr=34817  tpgid=58895 (both files)
//! the dead foreground group     tty_nr=34817  tpgid=58895 (unchanged)
//! ```
//!
//! Three rules come out of those rows, and this program checks each:
//!
//! 1. `tty_nr` is the terminal's device id **recoded**, not its `st_rdev`:
//!    `(minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)`. A pts slave's
//!    `st_rdev` is `0x8801`, and the file says `34817`.
//! 2. `tpgid` is the **terminal's** foreground group, not the reading task's
//!    own: two tasks on one line print the same `tpgid` beside different
//!    `pgrp` values, and moving the foreground moves both files at once.
//! 3. Field 6 is the session, which `setsid` makes distinct from the group: a
//!    session leader reads its own id in both fields, and a task that inherited
//!    the session reads the leader's id beside its own group.
//!
//! Every claim below is a movement rather than a shape, so a file that answers
//! one constant for all processes fails it. The two "no terminal" lines are the
//! control: they read the same before and after the fix, which is what keeps
//! this program from passing on a kernel that answers `0`/`-1` to everything.

use mpos_user_tests::Report;
use std::ffi::CString;

/// `stat`'s field `n` for `pid`, as a signed number.
///
/// The file's first two fields are the pid and `(comm)`, and `comm` may hold
/// spaces and parentheses of its own, so the split starts after the **last**
/// `)`. The first word there is field 3, so field `n` is the `n - 3`-th index
/// into that split (counting from zero).
fn field(pid: i32, n: usize) -> Option<i64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = text.rsplit_once(')')?.1;
    after
        .split_whitespace()
        .nth(n.checked_sub(3)?)?
        .parse()
        .ok()
}

/// The recoding `tty_nr` uses, so the expectation is derived from the device
/// rather than written down as a constant.
fn encode_dev(major: u64, minor: u64) -> i64 {
    ((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as i64
}

/// One ioctl, through `libc::syscall(SYS_ioctl)` - `t-pty`'s helper, for the
/// reason its comment gives: `libc::ioctl`'s request parameter is `c_int` in
/// the musl bindings and `c_ulong` in the gnu ones, and a request is neither.
/// The gate builds this program both ways and for both machines.
fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: usize) -> libc::c_long {
    // SAFETY: the pointer-shaped argument is whatever the caller passed, and
    // every caller here passes a buffer it owns.
    unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd as libc::c_long,
            request as libc::c_long,
            arg,
        )
    }
}

/// A fresh pty pair: its master, and the slave's path.
fn mint() -> Option<(std::os::fd::OwnedFd, CString)> {
    use rustix::fs::{Mode, OFlags, open};
    let master = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).ok()?;
    let mut number: u32 = 0;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    if ioctl(
        fd,
        libc::TIOCGPTN as libc::c_ulong,
        &mut number as *mut u32 as usize,
    ) != 0
    {
        return None;
    }
    let unlocked: i32 = 0;
    if ioctl(
        fd,
        libc::TIOCSPTLCK as libc::c_ulong,
        &unlocked as *const i32 as usize,
    ) != 0
    {
        return None;
    }
    Some((
        master,
        CString::new(format!("/dev/pts/{number}")).expect("no NUL in a path"),
    ))
}

/// What the child reports back: its own numbers, and the group it put in the
/// foreground afterwards.
#[derive(Default)]
struct Report0 {
    leader: i32,
    tty_nr: i64,
    tpgid: i64,
    pgrp: i64,
    session: i64,
    /// The grandchild's numbers, read from the leader's frame.
    grandchild: i32,
    grandchild_tty_nr: i64,
    grandchild_tpgid: i64,
    grandchild_pgrp: i64,
    grandchild_session: i64,
    /// The same two files after the foreground moved to the grandchild.
    leader_tpgid_after: i64,
    grandchild_tpgid_after: i64,
    /// The slave's own device id, as the leader read it with `fstat`.
    slave_major: u64,
    slave_minor: u64,
}

/// Fork a child that becomes a session leader with `path` as its controlling
/// terminal, then measures the four combos this pin needs.
///
/// The work happens in a child for the reason the Linux probe needed it: a
/// process that is already its group's leader cannot `setsid`, and the test
/// runner is exactly that.
fn measure(path: &CString) -> Option<Report0> {
    let (read_end, write_end) = rustix::pipe::pipe().ok()?;
    // SAFETY: fork in a program whose child only measures and exits.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        drop(read_end);
        let mut out = Report0::default();
        // SAFETY: `setsid` fails only for a process that is already a group
        // leader, and this is a fresh child.
        if unsafe { libc::setsid() } < 0 {
            write_report(write_end, &out);
            // SAFETY: `_exit` from a child that must not run the parent's
            // destructors.
            unsafe { libc::_exit(1) };
        }
        out.leader = unsafe { libc::getpid() };
        // SAFETY: the child opens a terminal it means to hold.
        use rustix::fs::{Mode, OFlags, open};
        if let Ok(slave) = open(path.as_c_str(), OFlags::RDWR, Mode::empty()) {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&slave);
            if ioctl(fd, libc::TIOCSCTTY as libc::c_ulong, 0) == 0 {
                // SAFETY: `fstat` fills one `stat` this frame owns.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(std::os::fd::AsRawFd::as_raw_fd(&slave), &mut st) } == 0 {
                    out.slave_major = libc::major(st.st_rdev as libc::dev_t) as u64;
                    out.slave_minor = libc::minor(st.st_rdev as libc::dev_t) as u64;
                }
                out.tty_nr = field(out.leader, 7).unwrap_or(-999);
                out.tpgid = field(out.leader, 8).unwrap_or(-999);
                out.pgrp = field(out.leader, 5).unwrap_or(-999);
                out.session = field(out.leader, 6).unwrap_or(-999);
                // A grandchild: same session and same terminal, its own
                // group once it calls `setpgid`, and parked so the leader can
                // read its file.
                //
                // The foreground move is the *leader's* call, not the
                // grandchild's: a process in the background that touches its
                // terminal's foreground gets `SIGTTOU`, and the default action
                // for that is `stop` - which is a hang, not a measurement.
                // SAFETY: fork in the child; the grandchild measures and exits.
                let grandchild = unsafe { libc::fork() };
                if grandchild == 0 {
                    // SAFETY: `setpgid(0,0)` makes this process its own group,
                    // which is what a shell does for a job.
                    unsafe { libc::setpgid(0, 0) };
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    // SAFETY: `_exit` from a child.
                    unsafe { libc::_exit(0) };
                }
                // Give the grandchild time to take its own group.
                std::thread::sleep(std::time::Duration::from_millis(120));
                out.grandchild = grandchild;
                out.grandchild_tty_nr = field(grandchild, 7).unwrap_or(-999);
                out.grandchild_tpgid = field(grandchild, 8).unwrap_or(-999);
                out.grandchild_pgrp = field(grandchild, 5).unwrap_or(-999);
                out.grandchild_session = field(grandchild, 6).unwrap_or(-999);
                // The leader is the foreground group, so *it* may hand the line
                // over.
                // SAFETY: `tcsetpgrp` names an fd this frame holds and a group
                // that belongs to this session.
                unsafe { libc::tcsetpgrp(std::os::fd::AsRawFd::as_raw_fd(&slave), out.grandchild) };
                out.grandchild_tpgid_after = field(grandchild, 8).unwrap_or(-999);
                out.leader_tpgid_after = field(out.leader, 8).unwrap_or(-999);
                // SAFETY: waiting for the grandchild this frame forked.
                unsafe { libc::waitpid(grandchild, std::ptr::null_mut(), 0) };
            }
        }
        write_report(write_end, &out);
        // SAFETY: `_exit` from a child.
        unsafe { libc::_exit(0) };
    }
    drop(write_end);
    let mut out = Report0::default();
    // Read the child's two records; the second may be absent.
    if read_report(read_end, &mut out) {
        // SAFETY: `waitpid` on a child this frame forked.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        Some(out)
    } else {
        None
    }
}

fn write_report(fd: std::os::fd::OwnedFd, r: &Report0) {
    use std::io::Write;
    let mut f = std::fs::File::from(fd);
    let words = [
        r.leader as i64,
        r.tty_nr,
        r.tpgid,
        r.pgrp,
        r.session,
        r.grandchild as i64,
        r.grandchild_tty_nr,
        r.grandchild_tpgid,
        r.grandchild_pgrp,
        r.grandchild_session,
        r.leader_tpgid_after,
        r.grandchild_tpgid_after,
        r.slave_major as i64,
        r.slave_minor as i64,
    ];
    for w in words {
        let _ = f.write_all(&w.to_ne_bytes());
    }
    let _ = f.flush();
}

fn read_report(fd: std::os::fd::OwnedFd, out: &mut Report0) -> bool {
    use std::io::Read;
    let mut f = std::fs::File::from(fd);
    let mut buf = [0u8; 8 * 14];
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return false,
        }
    }
    if got < 8 * 14 {
        return false;
    }
    let mut w = [0i64; 14];
    for (i, slot) in w.iter_mut().enumerate() {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[i * 8..i * 8 + 8]);
        *slot = i64::from_ne_bytes(bytes);
    }
    out.leader = w[0] as i32;
    out.tty_nr = w[1];
    out.tpgid = w[2];
    out.pgrp = w[3];
    out.session = w[4];
    out.grandchild = w[5] as i32;
    out.grandchild_tty_nr = w[6];
    out.grandchild_tpgid = w[7];
    out.grandchild_pgrp = w[8];
    out.grandchild_session = w[9];
    out.leader_tpgid_after = w[10];
    out.grandchild_tpgid_after = w[11];
    out.slave_major = w[12] as u64;
    out.slave_minor = w[13] as u64;
    true
}

fn main() {
    let mut report = Report::new();

    // Three control readings first: a process with no controlling terminal.
    // These are the same on the fixed and unfixed kernels, which is why the
    // discriminating claims below have to be movements.
    let me = std::process::id() as i32;
    let plain_tty = field(me, 7);
    let plain_tpgid = field(me, 8);
    report.check(
        &format!("a process with no terminal reads tty_nr 0 ({plain_tty:?})"),
        plain_tty == Some(0),
    );
    report.check(
        &format!("and tpgid -1 ({plain_tpgid:?})"),
        plain_tpgid == Some(-1),
    );
    // Field 6 deliberately gets no claim at this point. Whether the two
    // numbers differ here is a fact about who launched this program - the test
    // runner forks each program into a group of its own on the host, while on
    // the machine pid 1 leads both its group and its session, so the same
    // assertion is true in one place and false in the other while saying
    // nothing about the file. The claim that does say something about field 6
    // is the grandchild's, below: it inherits the leader's session while
    // holding a group of its own, and a file that copied `pgid` into field 6
    // prints the grandchild's own group there instead.

    // The master is held for the whole measurement and dropped at the end of
    // `main`: dropping it earlier would hang the line up under the child and
    // take the terminal away in the middle of the claims.
    let Some((_master, path)) = mint() else {
        report.check("a pty pair could be minted for the terminal claims", false);
        report.finish();
    };
    let Some(r) = measure(&path) else {
        report.check("the child could acquire a controlling terminal", false);
        report.finish();
    };
    let want_tty = encode_dev(r.slave_major, r.slave_minor);
    report.check(
        &format!(
            "the child acquired the terminal (slave {}/{}, want tty_nr {want_tty})",
            r.slave_major, r.slave_minor
        ),
        r.tty_nr == want_tty,
    );
    report.check(
        &format!(
            "tty_nr is the recoded device id, not its st_rdev ({}, want {want_tty})",
            r.tty_nr
        ),
        r.tty_nr == want_tty,
    );
    // The recoding only differs from a naive `(major << 8) | minor` past minor
    // 255, and a pty number is small - so what this claims is that the answer
    // is derived from *this* pair's device id rather than written down: a
    // kernel answering a constant `34817` would still pass a `pts/1` check, and
    // this one would not once the pair's number moved. The pair is the
    // allocator's, so the check is that the two agree.
    report.check(
        &format!(
            "and the answer is this pair's device, not a literal ({} vs {}:{})",
            r.tty_nr, r.slave_major, r.slave_minor
        ),
        r.tty_nr == encode_dev(r.slave_major, r.slave_minor) && r.slave_major == 136,
    );
    // `setsid` makes the session distinct from the group only for someone
    // else's group; for the leader both are its own id, which is the point.
    report.check(
        &format!(
            "a session leader's session is its own pid (session {}, pid {})",
            r.session, r.leader
        ),
        r.session == r.leader as i64,
    );
    report.check(
        &format!(
            "and tpgid is the terminal's foreground, which it just took (tpgid {}, pgrp {})",
            r.tpgid, r.pgrp
        ),
        r.tpgid == r.pgrp && r.tpgid == r.leader as i64,
    );
    // The grandchild: same terminal, same session, a group of its own.
    report.check(
        &format!(
            "a grandchild on the same line reads the same tty_nr ({} vs {})",
            r.grandchild_tty_nr, r.tty_nr
        ),
        r.grandchild_tty_nr == r.tty_nr && r.grandchild != 0,
    );
    report.check(
        &format!(
            "its session is the leader's, its group its own (session {}, pgrp {}, leader {})",
            r.grandchild_session, r.grandchild_pgrp, r.leader
        ),
        r.grandchild_session == r.leader as i64 && r.grandchild_pgrp == r.grandchild as i64,
    );
    report.check(
        &format!(
            "and its tpgid is the terminal's, not its own group (tpgid {}, pgrp {})",
            r.grandchild_tpgid, r.grandchild_pgrp
        ),
        r.grandchild_tpgid == r.pgrp && r.grandchild_tpgid != r.grandchild_pgrp,
    );
    // Moving the foreground moves both files at once.
    report.check(
        &format!(
            "moving the foreground moves the grandchild's tpgid ({} -> {})",
            r.grandchild_tpgid, r.grandchild_tpgid_after
        ),
        r.grandchild_tpgid_after == r.grandchild as i64,
    );
    report.check(
        &format!(
            "and the leader's file follows the terminal too ({} -> {})",
            r.tpgid, r.leader_tpgid_after
        ),
        r.leader_tpgid_after == r.grandchild as i64,
    );

    report.finish();
}
