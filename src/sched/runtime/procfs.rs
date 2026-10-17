use super::child_mm::current_mm;
use super::child_signal::process_members;
use super::task::with_table;
use super::task::{Table, Task, current};
use crate::hal;
use crate::mm::pframe;
use crate::mm::vm::MapFlags;
use crate::mm::vma::{MappedRange, VmaTable};
use crate::sched::policy::TaskId;
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
        let copied = match fresh {
            Some(table) => table.copy(),
            None => crate::vfs::mount::global().copy(),
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

/// The task `id` was spawned by, when it has one in ring 3. This is what
/// `getppid` reads through.
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
    /// The parent, or 0 for a process with none: init's own ppid, which is
    /// what Linux reports for pid 1.
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
    TaskSnapshot {
        tid: id.0,
        pid: task.tgid.0,
        ppid: task.parent.map_or(0, |p| p.0),
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
        threads: task.mm.live.load(core::sync::atomic::Ordering::Relaxed),
        rss_pages,
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
