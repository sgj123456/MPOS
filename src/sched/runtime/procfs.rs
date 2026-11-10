use super::child_exit::retired_status;
use super::child_mm::current_mm;
use super::child_signal::{process_members, queued_for_user};
use super::task::with_table;
use super::task::{Table, Task, current};
use crate::hal;
use crate::mm::pframe;
use crate::mm::vm::MapFlags;
use crate::mm::vma::{MappedRange, VmaTable};
use crate::sched::policy::TaskId;
use crate::sched::signal;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub fn current_cwd() -> Arc<dyn crate::vfs::Inode> {
    with_table(|t| match t.task(t.current).fs.cwd() {
        Some(dir) => dir,
        None => crate::vfs::global().root_inode(),
    })
}

/// Move the running task's cwd to `dir`. Checked to be a directory by the
/// caller; this only remembers. The move lands on the process's shared
/// [`crate::sched::FsStruct`], so a thread that `chdir`s moves its whole
/// process - which is Linux's answer (`CLONE_FS` shares `fs_struct`) and what
/// `unshare(CLONE_FS)` is for when it is not wanted.
pub fn set_cwd(dir: Arc<dyn crate::vfs::Inode>) {
    with_table(|t| t.task(t.current).fs.set_cwd(dir));
}

/// The directory this task's absolute paths begin at - the root `chroot(2)`
/// gave it, or the machine's root when it never called one. The `None` fallback
/// is what lets a task exist before the VFS is mounted, exactly as `current_cwd`
/// does.
pub fn current_root() -> Arc<dyn crate::vfs::Inode> {
    with_table(|t| match t.task(t.current).fs.root() {
        Some(dir) => dir,
        None => crate::vfs::global().root_inode(),
    })
}

/// Re-home the running task: make `dir` the root its absolute paths begin at.
/// Checked to be a directory by the caller; this only remembers. The machine's
/// own root is left standing - this is a per-process view, not a namespace -
/// and it lands on the process's shared [`crate::sched::FsStruct`], so a thread
/// that `chroot`s re-homes its process, as Linux's does.
pub fn set_root(dir: Arc<dyn crate::vfs::Inode>) {
    with_table(|t| t.task(t.current).fs.set_root(dir));
}

/// The running task's mount context: the table its paths resolve through and
/// the directory its absolute paths begin at. One call because every resolution
/// needs both — `resolve_walk`'s jail is the root, and the mount crossing that
/// walks over it reads the table — and two answers by context: a task that never
/// unshared holds `None` here and means the machine's one table
/// (`vfs::mount::global`), exactly as the `root` slot's `None` means the
/// machine's root. Boot code runs before any task exists and never calls this;
/// the host build has no task table to ask, so `vfs::mount`'s side of the pair
/// falls back to the table under test there (see `process_fs_context`).
pub fn current_fs_context() -> (
    Option<Arc<crate::vfs::mount::MountTable>>,
    Option<Arc<dyn crate::vfs::Inode>>,
) {
    with_table(|t| {
        let task = t.task(t.current);
        (task.fs_ns.clone(), task.fs.root())
    })
}

/// Give the running task a filesystem context of its own: `unshare(CLONE_FS)`'s
/// scheduler half, and Linux's `copy_fs_struct`. The caller keeps the root, the
/// cwd, the path and the mask it had and stops *sharing* them, so from here a
/// `chdir` moves only this task where before it moved the whole process. An
/// empty context would be a way to lose the machine, which is why the copy is
/// the only shape that can mean "unshare" here.
pub fn unshare_fs() {
    with_table(|t| {
        let me = t.current;
        let copied = t.task(me).fs.copy();
        t.task_mut(me).fs = copied;
    });
}

/// Give the running task a descriptor table of its own:
/// `unshare(CLONE_FILES)`'s scheduler half, Linux's `copy_files`. The copy is
/// `FdTable`'s own (`duplicate`, exactly what a `fork` gives a child): the
/// *numbering* becomes this task's, while the open file descriptions behind the
/// numbers stay shared - so a `close` here no longer closes the number for a
/// sibling, but a read position still moves for both.
pub fn unshare_files() {
    with_table(|t| {
        let me = t.current;
        let copied = Arc::new(t.task(me).fds.duplicate());
        t.task_mut(me).fds = copied;
    });
}

/// Whether the running task shares its thread group with another *live* task -
/// the fact Linux's `check_unshare_flags` asks before it refuses
/// `CLONE_THREAD`, `CLONE_SIGHAND` or `CLONE_VM`.
///
/// Measured on a current host (7.2.9), one child per flag: each of the three is
/// `0` for a single-threaded caller and `EINVAL` the moment a sibling exists,
/// because there is a real thing behind the flag that cannot be split off the
/// caller alone - an address space or a handler table that its siblings are
/// using.
///
/// The walk is [`process_members`], which is the one definition of a thread
/// group's liveness in this file, and using it is not tidiness: a hand-rolled
/// `task.tgid == tgid` filter that forgot `!task.exited` measured **members a
/// group does not have**. A thread whose slot was reused leaves an exited task
/// behind, and a *tgid is a slot number* - so the next process to occupy that
/// slot inherits the dead thread's number as its own group id, finds a "sibling"
/// nobody can see, and is refused `unshare(CLONE_THREAD)` on a machine where it
/// is the only thread. The same slot-reuse shape this file already paid for
/// once, in `docs/notes/` §6.5 (a dead process's orphans handed to the next
/// occupant of its slot).
pub fn thread_group_has_siblings() -> bool {
    let tgid = with_table(|t| t.task(t.current).tgid);
    process_members(tgid).len() > 1
}

/// Give the running task a mount table of its own: `unshare(CLONE_NEWNS)`'s
/// scheduler half. The copy is the VFS's (`MountTable::copy`, Linux's
/// `copy_mnt_ns`) — this only moves the caller's slot, which is what makes the
/// split observable: the task's siblings and children keep pointing at the old
/// table, and everything they resolve goes on answering the mounts the caller is
/// about to stop seeing. Same per-task rule as `unshare_uts` beside it.
pub fn unshare_mnt() {
    with_table(|t| {
        let fresh = t.task(t.current).fs_ns.clone();
        // The copy belongs to the user namespace the caller stands in, which is
        // what a `setns` into it is asked about (Linux's
        // `ns_capable(mnt_ns->user_ns, CAP_SYS_ADMIN)`).
        let owner = crate::sched::current_user_ns();
        let copied = match fresh {
            Some(table) => table.copy(owner),
            None => crate::vfs::mount::global().copy(owner),
        };
        t.task_mut(t.current).fs_ns = Some(copied);
    });
}

/// The path the running task's cwd was reached by, or "/" when it has never
/// moved - the answer `getcwd` owes its caller.
pub fn current_cwd_path() -> alloc::string::String {
    with_table(|t| {
        t.task(t.current)
            .fs
            .cwd_path()
            .unwrap_or_else(|| alloc::string::String::from("/"))
    })
}

/// Remember the path the running task's cwd was reached by. The caller
/// normalizes it; this only stores.
pub fn set_cwd_path(path: alloc::string::String) {
    with_table(|t| t.task(t.current).fs.set_cwd_path(path));
}

/// The group of the task that forked `task`: the parent every record *names*.
///
/// Linux reads it where the link is kept
/// (`task_tgid_nr_ns(rcu_dereference(p->real_parent))`), which is why one link
/// answers two questions: `children` lists by the *task* and the `PPid`-shaped
/// fields name its *group*.
///
/// `None` is one answer with two causes, and both mean there is no parent to
/// name: a task nobody forked (init), and a parent slot the table no longer
/// holds. The second should be unreachable for a live child - a task hands its
/// children to another parent *at its exit* rather than at its reap
/// (`child_exit`'s `forget_original_parent`, which reparents to `INIT_PID`) -
/// and the fallback is there so a stale link answers "nobody" rather than a
/// stranger's group.
fn parent_group(t: &Table, task: &Task) -> Option<TaskId> {
    let parent = task.parent?;
    t.tasks.get(parent.0)?.as_ref().map(|parent| parent.tgid)
}

/// The group of the task that forked `id`, for the doors that ask the question
/// about somebody without building a whole snapshot (`getppid`).
pub fn parent_group_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| {
        t.tasks
            .get(id.0)?
            .as_ref()
            .and_then(|task| parent_group(t, task))
    })
}

/// The task `id` was spawned by, when it has one in ring 3: the **link**
/// itself, not the number a record names (that is [`parent_group_of`]).
///
/// The readers are the ones that ask *which task*: the `ptrace` access check
/// (`a parent may trace its child`), and `children`, which lists by the link.
pub fn parent_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().and_then(|task| task.parent))
}

/// Wait for a signal. With no handlers yet every signal is fatal, so nothing
/// returns from here: the task ends inside [`block_until`].
const COMM_MAX: usize = 15;

/// What `/proc/<pid>` says about one task.
///
/// A copy rather than a view into the table: the table is behind a critical
/// section, and a filesystem read cannot hold one across the formatting of a
/// file - the generator would still be inside it while the next line is
/// written, and anything that wanted the table in between would deadlock. So
/// the facts come out in one look and the file is built from the copy.
#[derive(Clone)]
pub struct TaskSnapshot {
    /// The scheduler's own id for this thread.
    pub tid: usize,
    /// The process id - the thread group leader's tid, which is what `getpid`
    /// answers from any thread and what a directory in `/proc` is named after.
    pub pid: usize,
    /// The task that **forked** this one, by its own id: the *link*, and the
    /// key a `children` file lists by. `None` for the task nobody forked
    /// (init), which is why this is an `Option` while the number a record
    /// prints is `0`.
    ///
    /// Only one file reads it, and that is the point: the link is the
    /// scheduler's (`real_parent`) while the number a record *names* is
    /// [`TaskSnapshot::ppid`], because Linux names the group there.
    pub parent: Option<usize>,
    /// The number a `/proc` record names as this task's parent: the **group**
    /// of the task that forked it - Linux's `task_tgid_nr_ns(real_parent)` -
    /// or `0` for a task nobody forked, which is what Linux reports for pid 1.
    ///
    /// Not the forking task's own id, and the difference is measurable rather
    /// than a nicety: a child forked by a *non-leader thread* is named to that
    /// thread's **group** on Linux, while the link names the thread. Measured
    /// on a current host (`.workbuddy/probe_children_owner.c`, Linux 7.2.11):
    /// the child of a thread read `getppid`, `status`'s `PPid` and `stat`'s
    /// field 4 as `42093`, the group's number, where the thread that forked it
    /// was `42096`.
    pub ppid: usize,
    pub pgid: usize,
    /// The session this task belongs to, named by its leader's task id, or 0 for
    /// a task in none. `status`'s `NSsid` is this leader's chain of numbers, the
    /// same shape as the `NSpid` family beside it.
    pub session: usize,
    /// `R` running, `S` sleeping on something, `Z` exited and not yet
    /// collected - the letter `/proc/<pid>/stat` carries in field 3.
    pub state: char,
    /// The name, as `comm` and `ps` print it: the image's last path
    /// component, cut to [`COMM_MAX`].
    pub comm: alloc::string::String,
    /// The path the process was started from, empty when it has no named
    /// image - what `/proc/<pid>/exe` is a link to.
    pub exe: alloc::string::String,
    /// The argument vector it was entered with. Shared with the task: a
    /// reader only reads it, and a whole process's command line is not worth
    /// copying for a `ps`.
    pub argv: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    /// The environment it was entered with. Shared for the reason `argv` is.
    pub env: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    /// The directory relative paths are resolved from, as the caller spelled
    /// it. `/` for a process that never moved.
    pub cwd: alloc::string::String,
    /// Threads in the group: what `/proc/<pid>/task` lists and
    /// `Status: Threads` reports.
    pub threads: usize,
    /// Frames behind the group's user mappings. `stat` wants resident pages,
    /// `status` wants kilobytes, and both are this number.
    pub rss_pages: usize,
    /// Pages the group's address space has **mapped**, present or not: `stat`'s
    /// field 23 (`vsize`, in bytes) and `/proc/<pid>/statm`'s first field (in
    /// pages). [`crate::sched::child_mm`]'s `Mm::mapped_pages` owns the
    /// counting and its measurement; this is the copy `/proc` formats.
    ///
    /// The high-water marks: Linux's `VmPeak` (largest address space ever) and
    /// `VmHWM` (largest resident set ever), in pages. Measured on Linux 7.2.14
    /// (`build/scratch/vmfields/p.c`): mapping 4 MiB and unmapping it again
    /// leaves `VmPeak: 4324 kB` and `VmRSS: 72 kB` - the peak is a fact about
    /// the past that no current count can answer, so it is recorded where the
    /// tables grow ([`crate::sched::Mm`]'s `record_peak`/`own`), not derived.
    pub peak_pages: usize,
    pub hwm_frames: usize,
    /// Distinct from [`Self::rss_pages`] by construction: a mapping is a claim
    /// on the address space and residency is the substance behind it, which is
    /// why Linux reports both and why the two files that share this number
    /// print the same pages in two units.
    pub virtual_pages: usize,
    /// The nice value `setpriority` stored on this task, `-20..=19` - the
    /// number `/proc/<pid>/stat`'s field 19 prints and its field 18 prints as
    /// `20 + nice`.
    ///
    /// Measured on a current host (`build/scratch/psig/n.c`, Linux 7.2.14):
    /// with the process at nice 19 the two fields read `39` and `19`, so the
    /// pair is one number twice, and a kernel that printed `20`/`0` there was
    /// reporting the defaults rather than the value `setpriority` had stored
    /// (`docs/notes/` §6.101).
    pub nice: i32,
    /// The signal this task's death reports - the byte its `clone` carried,
    /// held as the byte rather than as a boolean because Linux *delivers* it
    /// and prints it. `/proc/<pid>/stat`'s field 38 is the one report of it,
    /// and it needs one fact this field does not carry: whether the task is a
    /// *thread*, whose report is `-1` on Linux. See
    /// `vfs::procfs::exit_signal_field`, which owns the rendering rule and its
    /// measurement.
    pub exit_signal: u8,
    /// CPU the **thread group** has burned, in this kernel's own ticks:
    /// `(utime, stime, cutime, cstime)` - the same four numbers
    /// `getrusage(RUSAGE_SELF)` and `times()` report.
    ///
    /// Measured on a current host (`build/scratch/batch/s8.c`, Linux 7.2.14):
    /// `/proc/<pid>/stat`'s field 14 tracked `RUSAGE_SELF`'s user time while a
    /// second thread spun (`66` USER_HZ ticks = `0.667 s`), and not the
    /// reading thread's own (`0.030 s`) - which is Linux's `whole` flag, set
    /// for `/proc/<pid>/stat` and `/proc/<tid>/stat` and clear only for
    /// `/proc/<pid>/task/<tid>/stat`.
    pub cpu: (u64, u64, u64, u64),
    /// CPU this **task** has burned, `(utime, stime)`, in the kernel's ticks.
    /// The `task/<tid>/stat` spelling's numbers; the group's are [`Self::cpu`].
    pub cpu_own: (u64, u64),
    /// The context switches this **thread** has taken, `(voluntary,
    /// involuntary)` - the two `/proc/<pid>/status` lines, which Linux answers
    /// from the thread and not from the group.
    ///
    /// Measured on a current host (`build/scratch/status/u.c`, Linux 7.2.14): a
    /// worker that slept fifty times read `voluntary_ctxt_switches: 50` in the
    /// worker's own `/proc/<pid>/task/<tid>/status` while the main thread read
    /// `16` - and `getrusage(RUSAGE_SELF)` answered `106` for the same process,
    /// which is the *group's* number and a different door (§6.105).
    pub switches: (u64, u64),
    /// The process's file-creation mask - `umask(2)`'s value, which
    /// `/proc/<pid>/status` prints as four octal digits and which every create
    /// masks its mode with. Per process, because [`crate::sched::FsStruct`] is
    /// shared between a group's threads.
    pub umask: u32,
    /// `PR_SET_NO_NEW_PRIVS`: whether this task's exec keeps privileges from
    /// being gained. `/proc/<pid>/status` prints it as `NoNewPrivs:`.
    pub no_new_privs: bool,
    /// The seccomp mode `/proc/<pid>/status`'s `Seccomp:` line names: `0` none,
    /// `1` strict, `2` filter.
    pub seccomp_mode: u8,
    /// How many filters are stacked - `Seccomp_filters:`.
    pub seccomp_filters: usize,
    /// The supplementary groups, in the order `setgroups(2)` gave them -
    /// `/proc/<pid>/status`'s `Groups:` line, which is also what
    /// `getgroups(2)` reports. Measured on Linux 7.2.14
    /// (`build/scratch/status2/g.c`): `Groups:\t953 966 992 998 1000 ` - one
    /// space after every number, the last one included, and an empty line part
    /// after the tab when the list is empty.
    pub groups: alloc::vec::Vec<u32>,
    /// The four capability sets `/proc/<pid>/status` prints as `CapInh`,
    /// `CapPrm`, `CapEff` and `CapBnd`, straight off the group's
    /// [`crate::cred::Credentials`]: the same words the credential model asks
    /// its own questions with, so a dropped identity prints the sets it really
    /// holds rather than a constant.
    pub cap_inheritable: u64,
    pub cap_permitted: u64,
    pub cap_effective: u64,
    pub cap_bounding: u64,
    /// When this task came into being, in `since_boot_nanos` - the number
    /// `/proc/<pid>/stat`'s field 22 (`starttime`) renders as `USER_HZ` ticks
    /// since boot.
    ///
    /// Measured on a current host (`build/scratch/starttime/p.c`, Linux 7.2.14):
    /// two children created a second apart read `6343185` and `6343194`, so the
    /// field is the creation instant in centiseconds, and `ps` turns it into an
    /// age by subtracting it from the boot time it read separately (`docs/notes/`
    /// §6.104).
    pub start_nanos: u64,
    /// Signals pending on **this task**, in the [`signal::Pending`] layout -
    /// `/proc/<pid>/stat`'s field 31.
    ///
    /// Measured on a current host (`build/scratch/sigfield/p.c`, Linux 7.2.14):
    /// a `SIGWINCH` (28) raised while blocked read the field as `134217728` =
    /// `1 << 27`, i.e. signal *n* in bit *n-1*, and unblocking the default
    /// action (ignore) cleared it again.
    pub pending: signal::Pending,
    /// The signals this task has blocked with `rt_sigprocmask` - field 32.
    /// Measured in the same run: `SIG_BLOCK` of `SIGUSR1` (10) moved the field
    /// to `512` = `1 << 9`, and the pending block above added its own bit.
    pub blocked: signal::Pending,
    /// The signals whose disposition is `SIG_IGN` - field 33, and a *group*
    /// question because a disposition is shared (`CLONE_SIGHAND`). Measured:
    /// `signal(SIGUSR2, SIG_IGN)` added `2048` = `1 << 11`, and installing a
    /// `SIGTERM` handler afterwards left it there.
    pub ignored: signal::Pending,
    /// The signals whose disposition is a handler - field 34.
    ///
    /// A signal is in this set or [`Self::ignored`], never both: `SIG_DFL` is
    /// 0, `SIG_IGN` is 1, and every handler address is above both. Measured:
    /// `sigaction(SIGTERM, handler)` read `16384` = `1 << 14`, and replacing it
    /// with `SIG_IGN` moved the same bit into field 33.
    pub caught: signal::Pending,
    /// Slots this task's descriptor table has ever grown to. `status`'s
    /// `FDSize:` is this rounded up to a power of two and floored at 64
    /// (§6.110). It never shrinks: `close` frees a slot without truncating the
    /// vector, so a table that once named fd 200 still reports a 256-slot
    /// capacity after every descriptor is gone. Measured on Linux 7.2.14
    /// (`build/scratch/fdq/formula.c`, `boundary2.c`): 64 slots -> 64, 65 ->
    /// 128, 129 -> 256, 257 -> 512, 513 -> 1024, and a `dup2` to 1000 -> 1024.
    pub fd_slots: usize,
    /// Real-time records queued for this task's **user**, across every one of
    /// that user's tasks and all signal numbers. `status`'s `SigQ:` numerator;
    /// the same count `RLIMIT_SIGPENDING` bounds in `send_signal_info`
    /// (`child_signal::queued_for_user`). Measured on Linux 7.2.14
    /// (`build/scratch/fdq/sigq3.c`): a child's two extra records raised the
    /// parent's numerator too, so the count is the user's and not the task's.
    pub sig_queued: u64,
    /// This task's `RLIMIT_SIGPENDING` soft limit - `SigQ:`'s denominator, which
    /// belongs to the task being read even though the numerator is shared.
    /// Measured (`build/scratch/fdq/sigq.c`): `setrlimit(3, 3)` moved
    /// `SigQ` from `6/125239` to `6/3`, and the numerator is allowed to exceed
    /// the limit when the limit is lowered underneath already-queued signals.
    pub sig_pending_limit: u64,
    /// This task's `RLIMIT_RSS` soft limit - `/proc/<pid>/stat`'s field 25,
    /// `rsslim`, printed as the bare `u64` row. The row starts at
    /// `RLIM_INFINITY`, so an ordinary process's field reads the full twenty
    /// digits of `u64::MAX`; lowering the soft limit to 4 MiB moves it to
    /// `4194304`, and restoring it moves it back (`build/scratch/rsslim/q.c`,
    /// Linux 7.2.14). A literal `0` claimed no process had ever been given a
    /// ceiling, which is the shape §6.111 records.
    pub rsslim: u64,
    /// `/proc/<pid>/stat`'s field 52, `exit_code`: the encoded `wait(2)` status
    /// of this task's death, or `0` while no death has been recorded. Measured
    /// on a current host (`build/scratch/statmore/q.c`, Linux 7.2.14): a zombie
    /// whose child called `exit(42)` reads `10752`, `SIGTERM` reads `15`,
    /// `SIGKILL` `9` and a core-dumping `SIGSEGV` `139`, each the same word
    /// `waitpid` answers; a live task and a leader whose group has not died yet
    /// both read `0` (`r.c`). The word is computed once by `retired_status`
    /// from the `Retired` record's raw code and core-dump classification, so
    /// `/proc` and `wait` cannot disagree about one death (§6.112).
    pub exit_code: i32,
    /// The program image's three addresses - `stat`'s fields 26, 27 and 28.
    ///
    /// A zombie has no address space at all and reads zeros for all three:
    /// Linux answers `startcode`/`endcode`/`startstack` as `0` for an
    /// uncollected zombie (`build/scratch/zombiev/z.c`).
    pub image: crate::mm::ImageBounds,
    /// The three page books `status` splits `VmExe`/`VmLib`/`VmData`/`VmStk`
    /// from, `(exec, stack, data)` - Linux's `exec_vm`, `stack_vm` and
    /// `data_vm`.
    ///
    /// Gathered here with [`Self::image`] because `VmExe` and `VmLib` are a
    /// *split* of the exec book by where the image's own text lies, so the
    /// addresses and the count have to be one instant's. A zombie reads zeros,
    /// which is also why its `status` prints no `Vm*` line at all.
    pub books: (usize, usize, usize),
    /// The pages this group has pinned - Linux's `mm->locked_vm`, the number
    /// `RLIMIT_MEMLOCK` bounds, `VmLck` prints and `smaps`'s `Locked:` splits
    /// per mapping. A zombie reads zero, like the books above.
    pub locked_pages: usize,
}

/// The two disposition sets `/proc/<pid>/stat` reports as fields 33 and 34.
///
/// One walk of the table rather than two, because the sets partition it: a
/// slot is ignored, caught, or default, and a signal that is `SIG_DFL` belongs
/// to neither word. The loop covers `1..=MAX_SIGNAL`, which is the whole table
/// (`Actions` holds `SIGNALS` entries indexed by number) and the whole word.
///
/// It takes a **snapshot** rather than the shared `SignalHand`, and that is
/// deliberate: a disposition is shared with every thread of the group, so
/// reading it through the `Arc` means holding its lock across sixty-four
/// reads while another thread may be in `sigaction`. `SignalHand::snapshot`
/// copies under one borrow, and the copy is what the report was always going
/// to be - one file's worth of one instant.
fn disposition_sets(actions: signal::Actions) -> (signal::Pending, signal::Pending) {
    let mut ignored = 0;
    let mut caught = 0;
    for sig in 1..=signal::MAX_SIGNAL {
        let action = actions.get(sig);
        if action.is_ignore() {
            ignored |= signal::bit(sig);
        } else if action.is_handler() {
            caught |= signal::bit(sig);
        }
    }
    (ignored, caught)
}

/// The facts about `task`, in the shape `/proc` reports them.
pub(crate) fn snapshot_of(t: &Table, id: TaskId, task: &Task) -> TaskSnapshot {
    // Exited-but-uncollected is a zombie, and it is still in the table
    // because it is still someone's child to collect. Sleeping is a question
    // only the scheduler's own queue can answer: a task blocked in a read is
    // waiting for a driver to wake it, and nothing about the task says so.
    let state = if task.exited {
        'Z'
    } else if task.trace.stop.is_some() {
        // Stopped, not sleeping: the stop is a state a SIGCONT undoes and a
        // `wait(WUNTRACED)` reports, and reporting it as sleep would read as
        // an ordinary blocked syscall - the letter a debugger and a shell's
        // job table both key on.
        'T'
    } else if t.sched.is_sleeping(id) {
        'S'
    } else {
        'R'
    };
    let rss_pages = critical_section::with(|cs| task.mm.frames.borrow(cs).borrow().len());
    let peak_pages = task.mm.peak_pages();
    let hwm_frames = task.mm.hwm_frames();
    let virtual_pages = task.mm.mapped_pages();
    let books = task.mm.vm_books();
    // A zombie has no address space: `release_exited` drops the space, and the
    // `mm` left behind is the table's record of a group whose threads have all
    // ended. Linux reports no `Vm*` line and three zero addresses for one
    // (`build/scratch/zombiev/z.c`), so the image's bounds are only read off a
    // task that still has an address space.
    let image = if task.exited {
        crate::mm::ImageBounds::default()
    } else {
        task.mm.image_bounds()
    };
    let (ignored, caught) = disposition_sets(task.dispositions.snapshot());
    let credentials = task.mm.credentials();
    TaskSnapshot {
        tid: id.0,
        pid: task.tgid.0,
        parent: task.parent.map(|parent| parent.0),
        ppid: parent_group(t, task).map_or(0, |group| group.0),
        pgid: task.pgid.0,
        session: task.session.0,
        state,
        comm: comm_of(task),
        exe: task.exe.clone(),
        argv: alloc::sync::Arc::clone(&task.argv),
        env: alloc::sync::Arc::clone(&task.env),
        cwd: task
            .fs
            .cwd_path()
            .unwrap_or_else(|| alloc::string::String::from("/")),
        // Threads, counted from the table rather than read off the group's
        // counter: the two disagree exactly when a thread has ended but its
        // slot has not been released, and Linux counts what is alive. Measured
        // by `t-exec-threads`: a worker's `exec` ends its siblings, the execing
        // thread takes a fresh `Mm` whose counter says 1, and the *ended*
        // leader's task object still holds the old one counting 2 - so reading
        // the counter answered `Threads: 2` for a process Linux calls single.
        // A zombie is the one case the table cannot answer (every task of the
        // group has exited) and Linux still prints 1 for it.
        threads: t
            .tasks
            .iter()
            .flatten()
            .filter(|sibling| sibling.tgid == task.tgid && !sibling.exited)
            .count()
            .max(1),
        rss_pages,
        peak_pages,
        hwm_frames,
        virtual_pages,
        image,
        books,
        locked_pages: if task.exited {
            0
        } else {
            task.mm.locked_pages()
        },
        nice: task.nice,
        exit_signal: task.exit_signal,
        cpu: task.mm.cpu_all(),
        cpu_own: (
            task.cpu_user.load(core::sync::atomic::Ordering::Relaxed),
            task.cpu_system.load(core::sync::atomic::Ordering::Relaxed),
        ),
        switches: (
            task.nvcsw.load(core::sync::atomic::Ordering::Relaxed),
            task.nivcsw.load(core::sync::atomic::Ordering::Relaxed),
        ),
        umask: task.fs.umask(),
        no_new_privs: task.no_new_privs,
        seccomp_mode: if task.seccomp_strict {
            1
        } else if task.seccomp_filters.is_empty() {
            0
        } else {
            2
        },
        seccomp_filters: task.seccomp_filters.len(),
        groups: task.identity.groups.clone(),
        cap_inheritable: credentials.inheritable,
        cap_permitted: credentials.permitted,
        cap_effective: credentials.effective,
        cap_bounding: credentials.bounding,
        start_nanos: task.start_nanos,
        pending: task.signals,
        blocked: task.blocked,
        ignored,
        caught,
        // Three counters off the same look that already took `rss_pages` and
        // `credentials`: the fd table's high water, and the user's queued total
        // paired with *this* task's own ceiling. `status` is the only reader of
        // all three; keeping them on the snapshot is what lets a file be built
        // outside the table's critical section (§6.110).
        fd_slots: task.fds.slot_count(),
        sig_queued: queued_for_user(t, task.identity.ruid),
        sig_pending_limit: task.rlimits[crate::abi::RLIMIT_SIGPENDING].cur,
        // Field 25 belongs to the task being read, like `rss_pages` beside it:
        // the row is the task's own table, not a global default, so a child
        // that lowers its own ceiling shows a different word from its parent.
        rsslim: task.rlimits[crate::abi::RLIMIT_RSS].cur,
        // Field 52 is the death of the group this slot belongs to, looked up
        // by the group id: a zombie leader's own slot is still here, and the
        // record is what names its encoded status. A live task has no record,
        // so it answers `None` and the field reads `0` (§6.112).
        exit_code: retired_status(task.tgid).unwrap_or(0),
    }
}

/// The name a process is listed under: the image's last path component, cut
/// to the fifteen characters `comm` holds.
///
/// `argv[0]` is the fallback rather than the first choice because a program
/// may exec itself under any name it likes - a shell running as `-sh`, or a
/// wrapper - and what Linux records is the image, not the claim. The
/// truncation takes the cut on a character boundary, since a name in UTF-8
/// with a byte cut through the middle of one is not a name at all.
pub(crate) fn comm_of(task: &Task) -> alloc::string::String {
    // A name set with `PR_SET_NAME` wins over the derived one, as it does on
    // Linux - the whole point of the call is to override what `comm` shows.
    // The stored bytes are an arbitrary byte string (Linux's `comm` is not
    // guaranteed UTF-8), so lossy-decode for the `String` `/proc` reports; the
    // exact bytes are handed to `PR_GET_NAME` by a different path.
    let stored = task.mm.comm_name_bytes();
    if stored[0] != 0 {
        let end = stored.iter().position(|&b| b == 0).unwrap_or(stored.len());
        return alloc::string::String::from_utf8_lossy(&stored[..end]).into_owned();
    }
    let named = if !task.exe.is_empty() {
        task.exe.as_str()
    } else {
        task.argv.first().map_or("", |first| first.as_str())
    };
    let base = named.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        return alloc::string::String::from("unknown");
    }
    match base.char_indices().nth(COMM_MAX) {
        Some((cut, _)) => alloc::string::String::from(&base[..cut]),
        None => alloc::string::String::from(base),
    }
}

/// Every process in the table, as `/proc` reports them.
///
/// Kernel tasks and the boot task are left out, which is the difference
/// between this and the table itself: they share the kernel's address space
/// and have no user image, no descriptors of their own and no pid a userland
/// tool could name - `ps` listing four of them beside the shell would be a
/// report about the kernel's own plumbing, not about the machine's work.
///
/// A zombie stays in: it has exited but not been collected, and until its
/// parent reads its code it is still a process with a pid - the `'Z'` Linux
/// lists. Its address space is already gone, which is why the filter asks
/// `user` rather than `space`.
pub fn snapshots() -> alloc::vec::Vec<TaskSnapshot> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter_map(|(slot, maybe)| {
                let task = maybe.as_ref()?;
                if !task.user {
                    return None;
                }
                Some(snapshot_of(t, TaskId(slot), task))
            })
            .collect()
    })
}

/// Everything `/proc/<pid>/maps` reports about one process, in one place.
///
/// Three sources, because the address space is described by three: the VMA
/// table holds what `mmap`, `mprotect` and `brk` put there, [`MappedRange`]
/// holds what the loader laid out (`MappedRange` explains why those are not in
/// the table), and the heap range is the program break, whose *name* - `[heap]`
/// - is a reporting convention rather than anything the mapping stores.
pub struct MapFacts {
    pub vmas: VmaTable,
    pub eager: Vec<MappedRange>,
    /// `[start, end)` of the program break, or `None` for a process that never
    /// asked for a heap.
    pub heap: Option<(usize, usize)>,
}

/// Record a range whose pages were mapped eagerly, for `/proc/<pid>/maps`.
///
/// This is what a `MAP_PRIVATE` mapping of an ordinary file needs and what its
/// VMA cannot say: the mapping has no VMA at all, because every page of it was
/// read in at `mmap` time and no address in it will ever fault. See
/// [`MappedRange`].
pub fn record_eager_range(start: usize, end: usize, prot: MapFlags, name: Option<Arc<str>>) {
    if start >= end {
        return;
    }
    current_mm().record_range(MappedRange {
        start,
        end,
        prot,
        name,
        sealed: false,
        locked: false,
    });
}

/// Carry the eagerly-mapped ranges inside `start..start + len` to `start + delta`.
///
/// `mremap`'s move half: the pages travel, the VMAs travel, and these must too
/// or the old address stays occupied for ever and the new one looks free.
pub fn move_eager_ranges(start: usize, len: usize, delta: isize) {
    current_mm().move_ranges(start, start.saturating_add(len), delta);
}

/// The address space of the process `pid`, for `/proc/<pid>/maps`. `None` when
/// nothing answers to the pid, which is `ENOENT` from the read.
pub fn map_facts(pid: usize) -> Option<MapFacts> {
    with_table(|t| {
        let task = t.tasks.get(pid)?.as_ref()?;
        if !task.user {
            return None;
        }
        let heap = (task.brk_pages > 0).then(|| {
            let start = task.brk_base;
            (start, start + task.brk_pages * pframe::PAGE_SIZE)
        });
        Some(MapFacts {
            vmas: task.mm.vma_snapshot(),
            eager: task.mm.eager_ranges(),
            heap,
        })
    })
}

/// One task, by pid or tid. `None` when no user task answers to it, which is
/// what turns into `ENOENT` from a read of `/proc/<pid>` - the same answer
/// Linux gives for a pid that has already been reaped.
///
/// A tid names its thread just as a pid names its leader: the files under
/// `/proc/<pid>/task/<tid>` are built by asking this for the thread. A caller
/// that wants leaders only tells the two apart by comparing the snapshot's
/// `pid` with the id it asked for.
pub fn snapshot(pid: usize) -> Option<TaskSnapshot> {
    with_table(|t| {
        let task = t.tasks.get(pid)?.as_ref()?;
        if !task.user {
            return None;
        }
        Some(snapshot_of(t, TaskId(pid), task))
    })
}

/// Every thread of the process `pid`, in the order the task table holds them.
///
/// The group leader is one of them, which is why Linux keeps threads under
/// `/proc/<pid>/task` rather than beside the process: the process *is* its
/// leader, and listing both separately would be two answers to one question.
pub fn thread_snapshots(pid: usize) -> alloc::vec::Vec<TaskSnapshot> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter_map(|(slot, maybe)| {
                let task = maybe.as_ref()?;
                if !task.user || task.tgid.0 != pid {
                    return None;
                }
                Some(snapshot_of(t, TaskId(slot), task))
            })
            .collect()
    })
}

/// The open descriptors of the process `pid`, each with the name
/// `/proc/<pid>/fd` reports it by, in fd-number order.
///
/// Only descriptors still open are listed: a closed slot names nothing, and a
/// listing that carried its number would point tools at a name that answers
/// `ENOENT`. The name is the path the descriptor was opened by when one was
/// recorded, the tree's own name for the inode when the tree knows one, and
/// Linux's spelling of an anonymous kind - `pipe:[ino]`, `socket:[ino]` -
/// when it has neither.
pub fn descriptors_of(pid: usize) -> alloc::vec::Vec<(usize, alloc::string::String)> {
    // The files are collected under the table and named outside it: naming
    // asks the mount table where an inode sits, and the two locks must never
    // nest the other way round.
    let open: alloc::vec::Vec<(i32, alloc::sync::Arc<crate::vfs::File>)> =
        with_table(|t| match t.tasks.get(pid).and_then(Option::as_ref) {
            Some(task) if task.user => task.fds.open_files(),
            _ => alloc::vec::Vec::new(),
        });
    open.into_iter()
        .map(|(fd, file)| (fd as usize, file.display_name()))
        .collect()
}

/// The direction the descriptor `fd` of `pid` was opened in - one of the
/// `ACC_*` words the description records - or `None` when it is not open.
///
/// `/proc/<pid>/fd/<N>`'s *own* mode is derived from this and from nothing else
/// (measured on a current host: Linux reports `0500` for a read-only
/// descriptor, `0300` for a write-only one and `0700` for both, so the two
/// directions are bits of the link's mode rather than a value), which is why
/// the fact is read off the file description rather than remembered on the
/// link.
pub fn descriptor_access(pid: usize, fd: usize) -> Option<u32> {
    with_table(|t| match t.tasks.get(pid).and_then(Option::as_ref) {
        Some(task) if task.user => task
            .fds
            .open_files()
            .into_iter()
            .find(|(number, _file)| *number as usize == fd)
            .map(|(_number, file)| file.access()),
        _ => None,
    })
}

/// The inode another process's descriptor holds.
///
/// The sibling of [`descriptor_access`]: one asks the direction a
/// `/proc/<pid>/fd/<N>` link's own word is derived from, this one asks what the
/// link *names*. `/proc/<pid>/fd/<N>` is a magic link, so following it is not
/// reopening the text `pipe:[123]` - it is naming the very file that process
/// holds, and the walk (`crate::vfs::mount::MountTable`) needs the inode to do
/// that (`docs/notes/` §3.22).
pub fn descriptor_inode_of(
    pid: usize,
    fd: usize,
) -> Option<alloc::sync::Arc<dyn crate::vfs::Inode>> {
    with_table(|t| match t.tasks.get(pid).and_then(Option::as_ref) {
        Some(task) if task.user => task
            .fds
            .open_files()
            .into_iter()
            .find(|(number, _file)| *number as usize == fd)
            .map(|(_number, file)| file.inode()),
        _ => None,
    })
}

/// The descriptor numbers `pid` has open, without asking for any of their
/// names.
///
/// This is what `/proc/<pid>/fd` is *listed* by, and the numbers are also all
/// a listing shows. Naming one is a search of the filesystem tree - see
/// [`crate::vfs::File::display_name`] - and a listing that searched the tree
/// would walk into `/proc/<pid>/fd` while it was in the middle of listing it:
/// the same question one stack deeper, without end, until the kernel stack
/// runs off its bottom and the damage lands on whatever the frame allocator
/// put next door. A name is computed when one is actually wanted - `readlink`
/// on an entry, which is a lookup and not a listing.
pub fn descriptor_numbers(pid: usize) -> alloc::vec::Vec<usize> {
    with_table(|t| match t.tasks.get(pid).and_then(Option::as_ref) {
        Some(task) if task.user => task
            .fds
            .open_files()
            .into_iter()
            .map(|(fd, _file)| fd as usize)
            .collect(),
        _ => alloc::vec::Vec::new(),
    })
}

/// Map a physical frame at `virt` in the current task's address space with
/// the given `flags`.  Returns the virtual address on success, or a negative
pub fn robust_head() -> usize {
    with_table(|t| t.task(t.current).mm.robust_head())
}

/// Record the robust-futex list head from `set_robust_list` for the running
/// task's process.
pub fn set_robust_head(head: usize) {
    with_table(|t| t.task(t.current).mm.set_robust_head(head));
}

/// Set the mmap cursor for the current task.
pub fn set_mmap_cursor(cursor: usize) {
    with_table(|t| {
        let task = t.task_mut(current());
        task.mmap_cursor = cursor;
    })
}

/// The base of the running task's thread-local storage - `FS.base`, which is
/// what `arch_prctl(ARCH_GET_FS)` reports and what every `%fs:` access in ring 3
/// goes through.
pub fn current_tls() -> usize {
    with_table(|t| t.task(current()).tls)
}

/// Move the running task's TLS base, both in its own record and in the register
/// the CPU is using. `arch_prctl(ARCH_SET_FS)` and `clone(CLONE_SETTLS)` are the
/// two ways in.
pub fn set_current_tls(base: usize) {
    with_table(|t| {
        let id = t.current;
        t.task_mut(id).tls = base;
    });
    // Stored per task as well, because `prepare` reinstalls it on every
    // dispatch: a bare register write here would survive only until the next
    // switch.
    hal::set_thread_pointer(base);
}
