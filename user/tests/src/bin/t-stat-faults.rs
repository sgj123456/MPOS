//! `/proc/<pid>/stat`'s minor-fault columns are counters, not literals.
//!
//! Fields 10 and 11 were two borrowed `"0"`s. What Linux answers, measured on
//! a current host (Linux 7.2.14, `build/scratch/statflags/`):
//!
//! * `j.c` - mapping 3 MiB and touching nothing moves field 10 not at all;
//!   writing pages moves it. The kernel allocates in batches, so the *count* is
//!   the claim and not the per-page one (three pages cost two, six more cost
//!   one).
//! * `l.c` - a child reporting `minflt=99` moved the parent's `cminflt` from
//!   `4312` to `4411`, and `RUSAGE_CHILDREN.ru_minflt` then read the same
//!   `4411`: two doors, one book.
//! * `o.c` - `RUSAGE_SELF`'s `ru_minflt` sits at offset 64 of `struct rusage`,
//!   `ru_majflt` at 72, and `RUSAGE_SELF`, `RUSAGE_THREAD` and
//!   `RUSAGE_CHILDREN` read three different numbers a moment apart
//!   (`5227`/`4557`/`4281`).
//! * `r.c` - the two `/proc` spellings are two books: one worker faulting 400
//!   pages left `/proc/<pid>/stat` reading the group's `5401`, the leader's own
//!   `task/<leader>/stat` reading `4525`, and that worker's `task/<worker>/stat`
//!   reading `403`.
//! * `k.c`, `m.c` - a copy-on-write split and a grown stack page are both minor
//!   faults, counted where they happen.
//!
//! `majflt`/`cmajflt` (12 and 13) stay `0` and this test asserts that: a *major*
//! fault is one the kernel answered by reading the device, and nothing in this
//! kernel's frame path carries that fact, so counting one either way would be a
//! guess (the named gap §6.130 records). Linux's own `majflt` reads `0` on a
//! machine with no swap and a warm page cache, so no fixture could tell a
//! correct zero from an unmeasured one.

use mpos_user_tests::Report;

/// Field `n` of a `/proc` `stat` line: the zero-based index `n - 3` of the
/// whitespace split that starts after the parenthesised `comm`. Field 1 is the
/// pid and field 2 the comm, which is why the first field after `)` is `3` -
/// and `comm` may contain spaces, so the split has to start at the last `)`.
fn field(text: &str, n: usize) -> Option<i64> {
    let after = text.rsplit_once(')')?.1;
    after.split_whitespace().nth(n - 3)?.parse().ok()
}

fn stat_of(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Every `stat` spelling for one task, as field 10 wants it: the process
/// spelling is the group's own count, the `task/<tid>/` one is that thread's.
fn minor_faults(path: &str) -> Option<i64> {
    field(&stat_of(path)?, 10)
}

/// `struct rusage`'s two fault counters, read at their ABI offsets rather than
/// through a libc struct: offsets 64 and 72 are the claim, so they are what
/// this reads (the same shape `t-rusage-switch` uses for the switch pair).
fn rusage_faults(who: i32) -> Option<(i64, i64)> {
    let mut raw = [0u8; 160];
    let ret = unsafe { libc::syscall(libc::SYS_getrusage, who, raw.as_mut_ptr()) };
    if ret != 0 {
        return None;
    }
    let word = |at: usize| -> i64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&raw[at..at + 8]);
        i64::from_ne_bytes(bytes)
    };
    Some((word(64), word(72)))
}

/// A fresh anonymous mapping, `pages` pages long, made with `mmap` rather than
/// the allocator: a `Vec` of a few hundred KiB may be carved out of heap this
/// process has already touched, in which case writing it faults nothing and the
/// measurement reads as a kernel that does not count. `mmap` has no such
/// history - every page of it is new to this address space.
struct Fresh {
    at: *mut u8,
    pages: usize,
}

impl Fresh {
    fn new(pages: usize) -> Option<Self> {
        let len = pages * 4096;
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return None;
        }
        Some(Self {
            at: at as *mut u8,
            pages,
        })
    }

    /// Write one byte into every page, which is one minor fault per page.
    fn touch(&mut self) {
        for i in 0..self.pages {
            unsafe { self.at.add(i * 4096).write_volatile(1) };
        }
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.at as *mut libc::c_void, self.pages * 4096) };
    }
}

fn main() {
    let mut r = Report::new();
    let me = unsafe { libc::getpid() };
    let group = format!("/proc/{me}/stat");
    let own = format!("/proc/{me}/task/{me}/stat");

    // A single-threaded process reads the same number from both spellings, so
    // this is the control the split has to move away from. It cannot be an
    // equality: opening and reading this very file allocates buffers in this
    // process, and those allocations are faults of the process whose count the
    // second read then reports. A couple of pages of slack is the reading's own
    // cost, not a property of the answer.
    // Both spellings name one book for a single-threaded process, and this is
    // the control the thread split has to move away from. It cannot be an
    // equality: reading either file allocates in this process, so each read
    // pays faults into the very counter it reports, and the `task/<tid>/` path
    // resolves two directory levels rather than one and pays more. The shape is
    // therefore a range with the read's own cost allowed for - measured at six
    // pages on this host, and an equality here was the first version of this
    // claim to go red on a loaded gate lane.
    fn settled(path: &str) -> Option<i64> {
        let mut best: Option<i64> = None;
        for _ in 0..6 {
            if let Some(reading) = minor_faults(path) {
                best = Some(best.map_or(reading, |b: i64| b.min(reading)));
            }
        }
        best
    }
    let before_group = settled(&group);
    let before_own = settled(&own);
    r.check(
        &format!(
            "a one-thread process's two spellings are the same book ({before_group:?} / {before_own:?}, within the read's own cost)"
        ),
        matches!((before_group, before_own), (Some(g), Some(o)) if (g - o).abs() <= 16),
    );

    // Mapping without touching: the count must not move. This is the half that
    // separates a counter from a mapped-bytes total, and it is why the mapping
    // is made with `mmap`: a `Vec` this size may be cut out of heap already in
    // use, and then the "untouched" buffer is not untouched at all.
    let unmapped_before = minor_faults(&group);
    let untouched = Fresh::new(768);
    let unmapped_after = minor_faults(&group);
    r.check(
        &format!(
            "a 3 MiB mapping nobody touched moves nothing ({unmapped_before:?} -> {unmapped_after:?}, mapping {})",
            untouched.is_some()
        ),
        matches!((unmapped_before, unmapped_after), (Some(b), Some(a)) if a - b <= 4),
    );
    drop(untouched);

    // Touching pages moves it. The delta is not asserted page-for-page: the
    // kernel batches frame allocation, so `j.c` moved 3 pages' worth by 2 and
    // six more by 1. `> 0` is the claim a literal zero cannot make.
    let faulted_before = minor_faults(&group);
    let mut held = Fresh::new(64);
    if let Some(mapping) = held.as_mut() {
        mapping.touch();
    }
    let faulted_after = minor_faults(&group);
    r.check(
        &format!(
            "touching 64 fresh pages moves the group's minor-fault column ({faulted_before:?} -> {faulted_after:?})"
        ),
        matches!((faulted_before, faulted_after), (Some(b), Some(a)) if a > b),
    );

    // The same count, read through `getrusage`: one book behind both doors
    // (`o.c` read `stat` `5230` against `RUSAGE_SELF` `5227`, the same counter a
    // few faults apart, so the claim is "close", not "equal").
    let stat_now = minor_faults(&group).unwrap_or(-1);
    let self_faults = rusage_faults(libc::RUSAGE_SELF).map(|f| f.0);
    r.check(
        &format!("RUSAGE_SELF's ru_minflt is the same book ({stat_now} at /proc, {self_faults:?} at getrusage)"),
        matches!(self_faults, Some(ru) if (ru - stat_now).abs() <= 16),
    );
    r.check(
        &format!(
            "and its ru_majflt is the honest zero ({:?})",
            rusage_faults(libc::RUSAGE_SELF).map(|f| f.1)
        ),
        rusage_faults(libc::RUSAGE_SELF).map(|f| f.1) == Some(0),
    );

    // A thread's own faults are its own column: this is the half that makes the
    // group's figure and a thread's figure two different answers, and it is why
    // `/proc/<pid>/stat` and `/proc/<pid>/task/<tid>/stat` are not synonyms when
    // a process has two threads.
    let before_thread = minor_faults(&own);
    let before_leader_group = minor_faults(&group);
    let spawned = std::thread::spawn(|| {
        let mut held = Fresh::new(96);
        if let Some(mapping) = held.as_mut() {
            mapping.touch();
        }
        // The worker's *own* tid, so its `task/<tid>/` file can be read for a
        // thread that is not the leader. It has to be read here, while this
        // thread is alive: the last thing this function does is return, and a
        // thread that has returned has no `task/<tid>/` left to read
        // (`statflags/o.c` walked into exactly that and read `-1`).
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let me = unsafe { libc::getpid() };
        let own_reading = minor_faults(&format!("/proc/{me}/task/{tid}/stat"));
        let group_reading = minor_faults(&format!("/proc/{me}/stat"));
        (tid, own_reading, group_reading, held.is_some())
    });
    let (worker_tid, worker_own, worker_group, worker_ok) =
        spawned.join().unwrap_or((-1, None, None, false));
    r.check(
        &format!("the worker faulted its pages ({worker_ok}, tid {worker_tid})"),
        worker_ok,
    );
    let after_thread = minor_faults(&own);
    let after_leader_group = minor_faults(&group);
    // The worker's faults are in the *group's* column (they share one `mm`) and
    // not in the leader's own. Both halves together are the claim; either alone
    // would pass for a kernel that copies one number into both. The leader's own
    // figure gets a few pages of slack because reading this file makes it fault
    // too, and the worker's 96 pages are what has to land on the group.
    r.check(
        &format!(
            "a sibling's faults land in the group, not this thread ({before_leader_group:?} -> {after_leader_group:?} group, {before_thread:?} -> {after_thread:?} own)"
        ),
        matches!((before_leader_group, after_leader_group), (Some(b), Some(a)) if a - b >= 64)
            && matches!((before_thread, after_thread), (Some(b), Some(a)) if a - b <= 4),
    );
    // And the worker's own spelling is its own count, which is what makes the
    // three answers three: the group's, the leader's own, and this thread's. The
    // worker read all three files itself, so the comparison is between numbers
    // taken at one moment rather than across the parent's own faulting.
    r.check(
        &format!(
            "the worker's own task/<tid> spelling is its own count (own {worker_own:?} < group {worker_group:?})"
        ),
        matches!((worker_own, worker_group), (Some(o), Some(g)) if o >= 64 && g > o),
    );

    // The children's column is the reaped child's work: `cminflt` starts empty
    // for this fresh process and grows by what the child faulted.
    let child_faults_before = field(&stat_of(&group).unwrap_or_default(), 11);
    let mut pipe = [0i32; 2];
    let forked = unsafe { libc::pipe(pipe.as_mut_ptr()) };
    let child = if forked == 0 {
        match unsafe { libc::fork() } {
            0 => {
                // The child faults its own pages and reports how many it saw,
                // then says so on the pipe so the parent is not guessing.
                let mut held = Fresh::new(64);
                if let Some(mapping) = held.as_mut() {
                    mapping.touch();
                }
                let seen = minor_faults("/proc/self/stat").unwrap_or(-1);
                drop(held);
                unsafe {
                    libc::write(pipe[1], &seen as *const i64 as *const libc::c_void, 8);
                    libc::_exit(0);
                }
            }
            pid => pid,
        }
    } else {
        -1
    };
    if child > 0 {
        let mut seen: i64 = -1;
        unsafe {
            libc::read(pipe[0], &mut seen as *mut i64 as *mut libc::c_void, 8);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
            libc::waitpid(child, core::ptr::null_mut(), 0);
        }
        let child_faults_after = field(&stat_of(&group).unwrap_or_default(), 11);
        r.check(
            &format!("the child faulted pages of its own ({seen})"),
            seen > 0,
        );
        // The bound is the 64 pages the child touched, not the number the child
        // reported: making the claim depend on that number would let a kernel
        // that counted nothing pass by reporting nothing (the child's own
        // reading is asserted above, so a zero there is already a failure).
        r.check(
            &format!(
                "and collecting it files those faults in cminflt ({child_faults_before:?} -> {child_faults_after:?}, child saw {seen})"
            ),
            matches!((child_faults_before, child_faults_after), (Some(b), Some(a)) if a >= b + 64),
        );
        // The same total through the other door: `RUSAGE_CHILDREN`'s
        // `ru_minflt` is the book `cminflt` is (`l.c` read one number from
        // both), so a kernel that counted them separately would disagree here.
        let children_ru = rusage_faults(libc::RUSAGE_CHILDREN).map(|f| f.0);
        r.check(
            &format!(
                "RUSAGE_CHILDREN's ru_minflt is that same book ({child_faults_after:?} at /proc, {children_ru:?} at getrusage)"
            ),
            matches!((child_faults_after, children_ru), (Some(c), Some(ru)) if ru == c),
        );
    } else {
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        r.check("a child could be forked for the cminflt claim", false);
    }

    // Both fault columns of the process spelling are numbers now, and the two
    // that cannot be measured yet are still honest zeros.
    let text = stat_of(&group).unwrap_or_default();
    let minflt_read = field(&text, 10);
    let cminflt_read = field(&text, 11);
    r.check(
        &format!("fields 10 and 11 are numbers ({minflt_read:?}, {cminflt_read:?})"),
        minflt_read.is_some_and(|n| n >= 0) && cminflt_read.is_some_and(|n| n >= 0),
    );
    let majflt = field(&text, 12);
    let cmajflt = field(&text, 13);
    r.check(
        &format!("field 12 majflt and 13 cmajflt stay zero ({majflt:?}, {cmajflt:?})"),
        majflt == Some(0) && cmajflt == Some(0),
    );
    let majflt_ru = rusage_faults(libc::RUSAGE_CHILDREN).map(|f| f.1);
    r.check(
        &format!("and RUSAGE_CHILDREN's ru_majflt agrees ({majflt_ru:?})"),
        majflt_ru == Some(0),
    );

    r.finish();
}
