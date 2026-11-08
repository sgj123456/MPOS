//! `getrusage`'s `ru_maxrss` is the resident-set high-water mark, in KB.
//!
//! Linux keeps TWO books for the field (`build/scratch/maxrss/`, musl static,
//! 7.2.14). A process has its own peak; a reaped child's peak folds into its
//! parent's `RUSAGE_CHILDREN` as a **MAX**, not a sum; and `wait4`'s `rusage`
//! reports the child's own peak OR the largest peak of the children it reaped,
//! whichever is bigger (`RUSAGE_BOTH`). This kernel wrote a literal zero at
//! offset 32 of the 144-byte record beside the CPU and switch pairs it already
//! counted (§6.105's named gap: "no high-water record"), so every process was
//! told it had never held a page. §6.109 added `Mm::hwm_frames` for
//! `/proc/<pid>/status`'s `VmHWM` and made the gap closable.
//!
//! Measured on Linux 7.2.14, with a **freshly forked** child (a child inherits
//! its parent's *current* resident set, so a probe run directly in this
//! container starts at ~1.09 GB and a 8 MiB touch is swallowed - `p.c`):
//!
//! ```text
//! q.c   child baseline SELF=0 THREAD=0
//!       mapped 8 MiB (untouched) SELF=0
//!       touched 8 MiB SELF=8204 THREAD=8204
//!       after munmap SELF=8204            (a true peak, never lowers)
//!       wait4(child) ru_maxrss=8204
//! r.c   C1 own SELF=2060, reaped a grandchild of peak 8204
//!       wait4(C1) ru_maxrss=8204          (own OR children, the max)
//! u.c   fresh worker CHILDREN=0
//!       after BIG child (8 MiB): wait4=8204  CHILDREN=8204
//!       after SMALL child (0):  wait4=0     CHILDREN=8204  (never lowers)
//! s.c   a fork inherits the parent's current peak (VmHWM 32864 -> 32776)
//! t.c   VmHWM dips on munmap (4184 -> 4112) but ru_maxrss stays 7900:
//!       the two fields are NOT the same number.
//! ```
//!
//! Every claim here is a **movement or a size relation**, never an absolute:
//! a process's baseline depends on the machine that spawned it, and `t.c`'s
//! fresh child already read 7900 kB before touching anything. The touch is
//! 32 MiB with a half-of-it margin so it dominates any inherited baseline.

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const BIG: usize = 32 << 20;
const BIG_KB: i64 = (BIG / 1024) as i64; // 32768
const SMALL: usize = 1 << 20;
/// KB of bookkeeping a probe may fault between two readings (stdio, a stack
/// page). Small against a 32 MiB touch, large against an untouched mmap.
const SLOP: i64 = 256;

const SELF: i32 = 0;
const THREAD: i32 = 1;
const CHILDREN: i32 = -1;

/// `ru_maxrss` is the third `long` of `struct rusage`, at byte 32: two
/// `timeval`s precede it. Read by hand because the offset is the claim - a
/// wrapper could not tell a wrong offset from a wrong count.
fn word(record: &[u8; 160]) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&record[32..40]);
    i64::from_le_bytes(b)
}

fn getrusage_raw(who: i32) -> Option<[u8; 160]> {
    let mut record = [0u8; 160];
    let rc = unsafe {
        libc::syscall(
            libc::SYS_getrusage,
            who as libc::c_long,
            record.as_mut_ptr() as *mut libc::c_void,
        )
    };
    (rc == 0).then_some(record)
}

fn maxrss(who: i32) -> Option<i64> {
    getrusage_raw(who).map(|r| word(&r))
}

/// `wait4(pid, &status, 0, &rusage)` through the raw door: the fourth argument
/// is the record whose `ru_maxrss` a `wait` is supposed to fill.
fn wait4_raw(pid: i32) -> Option<[u8; 160]> {
    let mut record = [0u8; 160];
    let mut status: i32 = 0;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_wait4,
            pid as libc::c_long,
            &mut status as *mut i32,
            0 as libc::c_long,
            record.as_mut_ptr() as *mut libc::c_void,
        )
    };
    (rc == pid as i64).then_some(record)
}

fn reserve(bytes: usize) -> *mut libc::c_void {
    unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    }
}

fn fault(p: *mut libc::c_void, bytes: usize) {
    let base = p as *mut u8;
    let mut i = 0;
    while i < bytes {
        // One byte per page is enough to make every page resident; `mmap`
        // alone does not (q.c: "mapped 8 MiB ... SELF=0").
        unsafe { base.add(i).write_volatile(1) };
        i += PAGE;
    }
}

fn unmap(p: *mut libc::c_void, bytes: usize) {
    unsafe { libc::munmap(p, bytes) };
}

fn write_five(fd: i32, vals: &[i64; 5]) {
    let mut buf = [0u8; 40];
    for (i, v) in vals.iter().enumerate() {
        buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, 40);
    }
}

fn read_five(fd: i32) -> Option<[i64; 5]> {
    let mut buf = [0u8; 40];
    let mut got = 0usize;
    while got < 40 {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().add(got) as *mut libc::c_void, 40 - got) };
        if n <= 0 {
            return None;
        }
        got += n as usize;
    }
    let mut out = [0i64; 5];
    for (i, v) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[i * 8..i * 8 + 8]);
        *v = i64::from_le_bytes(b);
    }
    Some(out)
}

/// A fresh process that reports `[before, mapped, touched, thread, after]`:
/// the peak before the mapping, after an **untouched** mmap, after faulting
/// every page, `RUSAGE_THREAD` at that moment, and after the mapping is gone.
fn spawn_peak_probe() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let before = maxrss(SELF).unwrap_or(-1);
        let p = reserve(BIG);
        if p == libc::MAP_FAILED {
            write_five(fds[1], &[-1; 5]);
            unsafe { libc::_exit(1) };
        }
        let mapped = maxrss(SELF).unwrap_or(-1);
        fault(p, BIG);
        let touched = maxrss(SELF).unwrap_or(-1);
        let thread = maxrss(THREAD).unwrap_or(-1);
        unmap(p, BIG);
        let after = maxrss(SELF).unwrap_or(-1);
        write_five(fds[1], &[before, mapped, touched, thread, after]);
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::close(fds[1]) };
    (pid > 0).then_some((pid, fds[0]))
}

/// A fresh worker whose `RUSAGE_CHILDREN` starts at zero. It reaps one big
/// child then one small child and reports `[c0, w_big, c1, w_small, c2]`:
/// CHILDREN before either, the big child's `wait4` peak, CHILDREN after it,
/// the small child's `wait4` peak, CHILDREN after that.
fn spawn_reaper() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { libc::close(fds[0]) };

        fn reap_touching(bytes: usize) -> i64 {
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                let p = reserve(bytes);
                if p != libc::MAP_FAILED {
                    fault(p, bytes);
                    unmap(p, bytes);
                }
                unsafe { libc::_exit(0) };
            }
            wait4_raw(pid).map(|r| word(&r)).unwrap_or(-1)
        }

        let c0 = maxrss(CHILDREN).unwrap_or(-1);
        let w_big = reap_touching(BIG);
        let c1 = maxrss(CHILDREN).unwrap_or(-1);
        let w_small = reap_touching(SMALL);
        let c2 = maxrss(CHILDREN).unwrap_or(-1);
        write_five(fds[1], &[c0, w_big, c1, w_small, c2]);
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::close(fds[1]) };
    (pid > 0).then_some((pid, fds[0]))
}

fn main() {
    let mut r = Report::new();

    // ---- the process's own peak, in a fresh child -------------------------
    let peak = spawn_peak_probe();
    let readings = peak.and_then(|(_, fd)| read_five(fd));
    if let Some((_, fd)) = peak {
        unsafe { libc::close(fd) };
    }
    // Always reap, even when a reading failed, so a dead child never lingers.
    // `wait4` fills the child's own peak in its rusage record - the second door.
    let waited = peak.and_then(|(pid, _)| wait4_raw(pid).map(|rec| word(&rec)));
    r.check(
        "getrusage answers ru_maxrss at offset 32 (a whole reading came back)",
        readings.is_some() && waited.is_some(),
    );

    if let Some(v) = readings {
        let [before, mapped, touched, thread, after] = v;
        r.check(
            &format!("an untouched mapping does not move the peak ({before} -> {mapped} kB)"),
            before >= 0 && mapped >= 0 && mapped <= before + SLOP,
        );
        r.check(
            &format!(
                "touching 32 MiB moves the peak by at least half of it ({before} -> {touched} kB)"
            ),
            touched - before >= BIG_KB / 2,
        );
        r.check(
            &format!("RUSAGE_THREAD is the same peak in a single-threaded process ({thread} against {touched})"),
            (thread - touched).abs() <= SLOP,
        );
        r.check(
            &format!("unmapping never lowers a true peak ({touched} -> {after} kB)"),
            after >= touched - SLOP,
        );
        if let Some(w) = waited {
            r.check(
                &format!("wait4 reports the child's peak ({w} against {touched} kB)"),
                w > 0 && w >= touched - SLOP,
            );
        }
    }

    // ---- the two books of RUSAGE_CHILDREN, in a fresh worker --------------
    let reaper = spawn_reaper();
    let c_readings = reaper.and_then(|(_, fd)| read_five(fd));
    if let Some((pid, fd)) = reaper {
        unsafe {
            libc::close(fd);
            let mut status: i32 = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }
    r.check(
        "a fresh worker reports its CHILDREN readings",
        c_readings.is_some(),
    );
    if let Some(c) = c_readings {
        let [c0, w_big, c1, w_small, c2] = c;
        r.check(
            &format!(
                "a big child's peak is larger than a small one's ({w_big} against {w_small} kB)"
            ),
            w_big >= w_small + BIG_KB / 2,
        );
        r.check(
            &format!(
                "the big child's peak folds into RUSAGE_CHILDREN ({c0} -> {c1}, wait4 {w_big} kB)"
            ),
            c1 > 0 && c1 >= c0 && c1 >= w_big - SLOP,
        );
        r.check(
            &format!("a smaller child never lowers CHILDREN ({c1} -> {c2} kB)"),
            c2 >= c1 - SLOP,
        );
    }

    r.finish();
}
