use super::ASLR_REGION_PAGES;
use super::ASLR_STACK_PAGES;
use super::FIRST_ALLOC_SLOT;
use super::ITIMER_ARMED;
use super::POSIX_TIMERS_ARMED;
use super::SCHED_ID;
use super::STACK_CANARY;
use super::STACK_ORDER;
use super::STACK_SIZE;
use super::STARTED;
use super::TABLE;
use super::USER_STACK_TOP;
use super::child_exit::{drop_retired, exit_code_pending, task_entry};

use super::child_exit::{exit, exit_code};
use super::child_mm::{Mm, populate_in, split_cow};
use super::child_signal::{queue_signal, table_has_deliverable, take_fatal_signal};

/// How many tasks can exist at once. A hard limit rather than a grown
/// collection, for the reason given in the module docs.
pub const MAX_TASKS: usize = 64;

/// Where each task last went to sleep, for `/proc/<pid>/stat`'s `wchan` word.
///
/// `0` is "never parked"; the rest name the primitive the task last slept in,
/// so a stuck task can say *which wait* it is in without a debugger - the one
/// question this kernel could not answer while chasing the concurrent-`exec`
/// deadlock (`docs/notes/` §7 R.9), where every candidate had to be ruled out
/// by a boot instead of by one `ps`. The ids are a closed vocabulary because
/// this is instrumentation with a shape, not a log line: see [`PARK_SITES`].
///
/// A task that parked once and is now running keeps its last site. That is
/// enough for the question (`ps` asks about tasks that are *asleep*) and it is
/// one store per park, not one per wake.
pub static PARK_SITE: [AtomicU8; MAX_TASKS] = [const { AtomicU8::new(0) }; MAX_TASKS];

/// The names behind [`PARK_SITE`]'s ids, for the humans reading `wchan`.
///
/// `/proc/<pid>/stat`'s field is a *number* in Linux (a kernel address), so the
/// id goes there verbatim and keeps the field's shape; these names are what the
/// number means, and they live here so the mapping cannot drift from the
/// primitives that write it.
pub const PARK_SITES: [&str; 11] = [
    "0",
    "block_until_on",
    "block_until_nanos",
    "block_until_interruptible_rolling_back",
    "block_until_uninterruptible",
    "block_until_code",
    "block_until_deadline",
    "ext4_slot_wait",
    "ext4_scratch_wait",
    "ext4_set_wait",
    "ext4_reentry_wait",
];

/// The park site a slot last slept in, `0` for never - `wchan`'s answer.
pub fn park_site(slot: usize) -> u8 {
    PARK_SITE
        .get(slot)
        .map(|site| site.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// The last syscall each task took, by number: what a spinning task's loop is
/// calling. Written on the syscall entry path (one store), read by the
/// heartbeat - the boot's stall is a *spin* before `exec` with no image to name
/// it by (`docs/notes/` §7 R.9), and the number names the loop.
pub static LAST_SYSCALL: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Record `nr` as the running task's last syscall.
pub fn note_syscall(nr: u32) {
    with_table(|t| {
        if let Some(slot) = LAST_SYSCALL.get(t.current.0) {
            slot.store(nr, Ordering::Relaxed);
        }
    });
}

/// The bare number of `slot`'s last syscall, `0` for none yet.
pub fn last_syscall(slot: usize) -> u32 {
    LAST_SYSCALL
        .get(slot)
        .map(|slot| slot.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// The task that holds the ext4 cache's fill scratch, and the one that holds
/// its run scratch, as slots (`usize::MAX` for "out with nobody").
///
/// The scratch is one flag cleared only by the holder's own `end_fill`/
/// `end_run`; when a boot stalls with waiters parked on it and no live task in a
/// transfer, the missing question is *whose claim outlived its return*
/// (`docs/notes/` §7 R.9). These two statics are that answer: the cache records
/// the holder where it takes the scratch and clears it where it gives it back,
/// and the heartbeat prints both beside the slot table.
pub static FILL_OWNER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
pub static RUN_OWNER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// The ext4 cache's two scratch flags, mirrored here for the heartbeat: the
/// cache owns them, and a boot that stalls on one has to be readable from the
/// wire without the scheduler reaching into the filesystem (`docs/notes/` §7 R.9
/// - the reading that made this necessary was a waiter asleep at
/// `ext4_scratch_wait` while the owners said none, which only a *flag* answer can
/// settle).
pub static EXT4_FILL_BUSY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
pub static EXT4_RUN_BUSY: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Mirror the ext4 cache's scratch flags.
pub fn note_ext4_scratch(fill: bool, run: bool) {
    EXT4_FILL_BUSY.store(fill, Ordering::Relaxed);
    EXT4_RUN_BUSY.store(run, Ordering::Relaxed);
}

/// How many tasks registered on the ext4 cache's claim-waiter list, and how
/// many the list's drainer handed back. Unequal counts, with a task asleep in
/// the scratch wait and both scratch flags false, name which side of the lost
/// wake is broken: a registration the drainer never saw, or a wake handed to a
/// task that then slept anyway (`docs/notes/` §7 R.9).
pub static CLAIM_REGISTRATIONS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static CLAIM_WOKEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count one registration on the claim-waiter list.
pub fn note_claim_registration() {
    CLAIM_REGISTRATIONS.fetch_add(1, Ordering::Relaxed);
}

/// Count `n` waiters handed back by a claim's end.
pub fn note_claim_wake(n: usize) {
    CLAIM_WOKEN.fetch_add(n as u64, Ordering::Relaxed);
}

/// Record (or clear, with `None`) the fill scratch's holder.
pub fn note_fill_owner(slot: Option<usize>) {
    FILL_OWNER.store(slot.unwrap_or(usize::MAX), Ordering::Relaxed);
}

/// Record (or clear, with `None`) the run scratch's holder.
pub fn note_run_owner(slot: Option<usize>) {
    RUN_OWNER.store(slot.unwrap_or(usize::MAX), Ordering::Relaxed);
}

/// The fill scratch's holder, if one is recorded.
pub fn fill_owner() -> Option<usize> {
    let slot = FILL_OWNER.load(Ordering::Relaxed);
    (slot != usize::MAX).then_some(slot)
}

/// The run scratch's holder, if one is recorded.
pub fn run_owner() -> Option<usize> {
    let slot = RUN_OWNER.load(Ordering::Relaxed);
    (slot != usize::MAX).then_some(slot)
}

/// How many times each slot has parked at all: a slot parked at a claim site
/// with one registration behind it is a slot that slept **twice** - and a second
/// sleep with no re-registration is stranded (`docs/notes/` §7 R.9's last two
/// candidates).
pub static PARKS: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// How many parks `slot` has taken, in total.
pub fn parks_of(slot: usize) -> u32 {
    PARKS
        .get(slot)
        .map(|count| count.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Record that `t`'s running task is about to sleep at `site`.
fn note_park(t: &Table, site: u8) {
    if let Some(slot) = PARK_SITE.get(t.current.0) {
        slot.store(site, Ordering::Relaxed);
    }
}

/// [`note_park`] for a primitive entering its wait, this task's own slot.
fn note_park_current(site: u8) {
    with_table(|t| {
        note_park(t, site);
        if let Some(count) = PARKS.get(t.current.0) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// How often a claim's wake found its waiter asleep (`true`) or not (`false`):
/// the one pair that separates "the wake landed and the waiter slept again" from
/// "the wake found it running and its own re-check failed" (`docs/notes/` §7 R.9).
pub static CLAIM_WAKE_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CLAIM_WAKE_MISSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count one claim wake's result.
pub fn note_claim_wake_result(hit: bool) {
    if hit {
        CLAIM_WAKE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CLAIM_WAKE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

use crate::abi::{RLIM_NLIMITS, Rlimit, default_rlimits, rlimit_admit};
use crate::hal;
use crate::hal::{AbiRegs, AddressSpace, Context, ContextOps, UserFrame};
use crate::mm::elf::Image;
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{Priority, Scheduler, TaskId};
use crate::sched::signal;
use crate::vfs::RawFd;
use crate::vfs::file::{FdTable, File};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{RefCell, UnsafeCell};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use critical_section::Mutex;
/// A process's user and group identity: the real, effective and saved id of
/// each, which is the whole of what `setuid(2)`'s family manipulates, plus
/// the supplementary groups `setgroups(2)` names - what a `doas` rule for
/// `:wheel` checks membership in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The real user id: who the process was started as.
    pub ruid: u32,
    /// The effective user id: who privilege checks act as. Zero is this
    /// kernel's approximation of "may set any id".
    pub euid: u32,
    /// The saved user id: who the process may raise its effective id back to.
    pub suid: u32,
    /// The real group id.
    pub rgid: u32,
    /// The effective group id.
    pub egid: u32,
    /// The saved group id.
    pub sgid: u32,
    /// The supplementary groups, in the order `setgroups(2)` was given them.
    /// Empty until a privileged caller names some; `getgroups(2)` reports
    /// this list and a `:group` rule in `doas.conf` matches against it.
    pub groups: Vec<u32>,
}
fn random_pages(count: usize) -> usize {
    let mut bytes = [0u8; 8];
    crate::rand::fill(&mut bytes);
    let drawn = u64::from_le_bytes(bytes) as usize;
    (drawn % count) * PAGE_SIZE
}

/// Where one program's pieces are going to go, drawn fresh at every `exec`.
///
/// Four landmarks move, and they are the ones Linux moves: where a
/// position-independent image is loaded, where its interpreter is loaded, where
/// its stack starts, and where its anonymous `mmap` region begins. What does not
/// move is a fixed-address (`ET_EXEC`) image's own address - see
/// [`ASLR_REGION_PAGES`] - and a `fork` copies every landmark rather than drawing
/// new ones, because a child that inherited its parent's memory at addresses of
/// its own had would not have inherited its parent's memory.
///
/// The interpreter is the one worth the most care. On this userland `libc` and
/// the program interpreter are the same image, so a fixed interpreter base leaves
/// every process on the machine with the same stock of return addresses and the
/// same table of function pointers - which is exactly what a write-anywhere
/// primitive reaches for. Randomising the program alone would move the thing an
/// attacker least needs to find.
///
/// The reason for all of it: anything that overwrites a pointer with a value it
/// chose needs to know where the thing it points at *is*. A layout that is the
/// same in every process on every boot answers that question once, for
/// everybody, which is why a kernel without this is a kernel where one leaked
/// pointer is a working exploit rather than one guess among thousands.
pub(crate) struct Layout {
    /// The address the image's own `vaddr`s are offsets from. Zero for an
    /// image that named where it wanted to be.
    pub(crate) bias: usize,
    /// The same, for the interpreter. Zero when the program is static.
    pub(crate) interp_bias: usize,
    /// The highest stack address the program may use, one past the top.
    pub(crate) stack_top: usize,
    /// The address the `mmap` region grows down from.
    pub(crate) mmap_top: usize,
}

/// How far a placed image reaches above the addresses it was linked with.
fn span_of(image: &Image<'_>) -> usize {
    let end = image
        .segments
        .iter()
        .map(|segment| segment.vaddr + segment.memsz)
        .max()
        .unwrap_or(0);
    pframe::align_up(end, PAGE_SIZE)
}

/// Draw a program's layout. See [`Layout`].
///
/// The order is not free: the three regions that come out of the `mmap` band are
/// stacked against each other - the `mmap` region's start, then the interpreter
/// below it, then the program below that - with an independent draw between each
/// pair. Two programs of the same size therefore start at addresses that differ
/// by a random amount, and no draw can put one region on top of another. Placing
/// them that way also means the top of a placed image is always below
/// `MMAP_BASE`, so no amount of bias can walk a program's pages up into the
/// kernel half of the address space.
pub(crate) fn new_layout(image: &Image<'_>, interp: Option<&Image<'_>>) -> Layout {
    let mmap_top = crate::mm::elf::MMAP_BASE - random_pages(ASLR_REGION_PAGES);
    let mut ceiling = mmap_top;
    let interp_bias = match interp {
        Some(interp) => {
            ceiling -= random_pages(ASLR_REGION_PAGES);
            let base = ceiling.checked_sub(span_of(interp));
            if let Some(base) = base {
                ceiling = base;
            }
            base.unwrap_or(INTERP_BASE)
        }
        None => 0,
    };
    // An image too large to fit below the interpreter is left at the addresses
    // it names. That is not a weakness in the randomisation but a property of the
    // file: no placement can fit an image whose own span exceeds the band, and
    // `elf::parse` has already refused the ones that reach into the kernel half.
    let bias = if image.pie {
        ceiling -= random_pages(ASLR_REGION_PAGES);
        ceiling.checked_sub(span_of(image)).unwrap_or(0)
    } else {
        0
    };
    Layout {
        bias,
        interp_bias,
        stack_top: USER_STACK_TOP - random_pages(ASLR_STACK_PAGES),
        mmap_top,
    }
}

// The resource-limit table's type and startup values live with the syscall
// layer (which is host-compiled and tests them); the scheduler only carries a
// per-task copy and reads/writes it through the accessors below.

/// The current process's limit for `resource`.
#[cfg(target_os = "none")]
pub fn current_rlimit(resource: usize) -> Option<Rlimit> {
    with_table(|t| t.task(t.current).rlimits.get(resource).copied())
}

/// Set the current process's limit for `resource`, applying `setrlimit`'s rule
/// against the caller's root-ness. `Err(errno)` on a refused change.
#[cfg(target_os = "none")]
pub fn set_current_rlimit(resource: usize, new: Rlimit, is_root: bool) -> Result<(), isize> {
    with_table(|t| {
        let cur = t.current;
        let old = t.task(cur).rlimits[resource];
        match rlimit_admit(old, new, is_root) {
            Ok(v) => {
                t.task_mut(cur).rlimits[resource] = v;
                // RLIMIT_NOFILE (7 in `asm-generic/resource.h`, which is the
                // numbering a caller's `setrlimit` speaks) also caps the
                // descriptor table: keep the two in step so an fd allocation
                // refuses at the new ceiling.
                if resource == 7 {
                    let fds = t.task(cur).fds.clone();
                    fds.set_nofile(v.cur);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    })
}

/// Whether the current task is in `SECCOMP_MODE_STRICT`, read at the syscall
/// dispatch to decide whether a non-whitelisted call is fatal.
#[cfg(target_os = "none")]
pub fn current_is_seccomp_strict() -> bool {
    with_table(|t| t.task(t.current).seccomp_strict)
}

/// Latch the current task into `SECCOMP_MODE_STRICT`: irreversible for its
/// life, inherited across `fork`, kept across `exec`.
#[cfg(target_os = "none")]
pub fn set_seccomp_strict() {
    with_table(|t| t.task_mut(t.current).seccomp_strict = true);
}

/// Whether the current task has any `SECCOMP_MODE_FILTER` program - what
/// `PR_GET_SECCOMP` reports as mode 2, and what a caller must be told if it
/// asks whether it is confined.
#[cfg(target_os = "none")]
pub fn has_seccomp_filter() -> bool {
    with_table(|t| !t.task(t.current).seccomp_filters.is_empty())
}

/// Install one filter on the current task.
///
/// Cumulative and irreversible, both of them Linux's rules: a thread ends up
/// running every program it has ever installed, and the *most restrictive*
/// answer among them is the one that counts. That is why this appends rather
/// than replaces - a sandboxed program installing a second, narrower filter
/// must not thereby lose the first.
#[cfg(target_os = "none")]
pub fn set_seccomp_filter(program: crate::seccomp::Program) {
    // Armed before the filter is stored, so the gate can never see the list
    // without knowing to look at it - the same ordering, and the same reason,
    // as `itimer_swap` setting `ITIMER_ARMED` before it stores the timer.
    crate::seccomp::arm();
    with_table(|t| t.task_mut(t.current).seccomp_filters.push(program));
}

/// Run the current task's filters over one call and answer the action to take,
/// or `None` when it has none - which is the case the gate must be able to skip
/// without touching the table at all.
///
/// The programs run under the task table's own critical section, with
/// interrupts off, because that is where the task's state is. The work is
/// bounded and it has to be: at most `MAX_INSNS` straight-line instructions over
/// a 64-byte block, no allocation, no I/O and no nested lock. A seccomp filter
/// gets no second thread to run on, on this kernel or on Linux.
#[cfg(target_os = "none")]
pub fn run_seccomp_filters(data: &[u8; crate::seccomp::DATA_BYTES]) -> Option<u32> {
    with_table(|t| {
        let filters = &t.task(t.current).seccomp_filters;
        if filters.is_empty() {
            return None;
        }
        // Allowed until a filter says otherwise, and the strictest answer is
        // the one taken - so a program can only ever be narrowed by adding one.
        let mut answer = crate::seccomp::RET_ALLOW;
        for program in filters {
            answer = crate::seccomp::more_restrictive(answer, program.run(data));
        }
        Some(answer)
    })
}

/// The whole limit table of `pid` (`0` = the current process), for
/// `/proc/<pid>/limits`. `None` if no such task exists.
#[cfg(target_os = "none")]
pub fn rlimits_of(pid: usize) -> Option<[Rlimit; RLIM_NLIMITS]> {
    with_table(|t| {
        let id = if pid == 0 { t.current } else { TaskId(pid) };
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .map(|tk| tk.rlimits)
    })
}
/// How far below the stack pointer a faulting address may be and still be a
/// push. Linux uses the same figure for the same reason: a stack frame is next
/// to `rsp`, and an address a long way below it is a wild pointer that has to
/// stay a fault rather than quietly claim pages.
pub(crate) const STACK_GROWTH_SLACK: usize = 64 * 1024;

// `ARGV_MAX` / `ARGS_BYTES_MAX` live in `policy`: the syscall layer checks
/// Where a dynamically linked program's interpreter is loaded when the layout
/// could not find it room anywhere else.
///
/// The normal answer is a drawn address, in [`Layout::interp_bias`]: this image
/// is `libc` as much as it is the loader on this userland, so an address it
/// shares with every other process is a supply of return addresses and function
/// pointers available to anything that can write a pointer anywhere. The fixed
/// one stays as the fallback for the case the draw cannot serve - an interpreter
/// too large to fit below the region the process `mmap`s from - which is a
/// property of the file rather than of the randomiser.
///
/// It cannot go at zero with the program either way: an interpreter is position
/// independent, its segments count from zero, and mapping it there would land it
/// on top of the program's own first pages.
pub(crate) const INTERP_BASE: usize = 0x0000_0000_7000_0000;

/// Why a user image refused to spawn. The syscall layer maps these onto
/// errnos; nothing here panics, because a bad path from ring 3 is not a
/// kernel bug.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The bytes are not an ELF64 image this kernel can load.
    NotExecutable,
    /// The argument strings do not fit the initial stack.
    ArgsTooBig,
    /// A `#!` chain that leads back into itself.
    Loop,
    /// A descriptor the starter asked the child to be born with names nothing
    /// in the starter's own table.
    BadFd,
    /// The image asked for a program interpreter - a dynamically linked
    /// binary - and the path it named is not on the filesystem, or is not an
    /// image that can be loaded.
    NoInterpreter,
}

/// Where a descriptor taken from the starter's table goes in the child's:
/// read `from` in the parent, install at `to` in the child. One pair is one
/// redirection, which is how a shell hands a pipe end to the program it runs.
pub type Redirect = (RawFd, RawFd);

/// One collected-death record.
#[derive(Clone)]
pub(crate) struct Retired {
    pub(crate) id: TaskId,
    pub(crate) code: isize,
    /// The pid numbers the dead process had at every level of its namespace
    /// chain, each with the identity of the namespace that gave it
    /// (`crate::sched::pid_chain_of`), outermost first.
    ///
    /// Carried for `pgid`'s reason and one more: the slot is free by the time a
    /// `wait` reads the record, so `pid_number_of` has nothing left to ask, and
    /// a `wait` must answer the number **the reader's** namespace gives - a
    /// parent one level out and a parent at the root read two different
    /// numbers for the same child. Measured on a current host inside a fresh
    /// pid namespace (`.workbuddy/probe_pid_numbers.c`): `fork` answers `2`,
    /// `wait4(-1)` reports `2`, and a `CLONE_NEWPID` child reads `1` to itself
    /// while its parent's `wait` reports the parent's number.
    pub(crate) numbers: alloc::vec::Vec<(u64, u32)>,
    /// The process group this task was in when it exited.
    ///
    /// Carried here rather than looked up because the slot is free by the time
    /// a `waitid(P_PGID, ...)` asks: the task is gone, and a group selector has
    /// nothing left in the table to read the answer from. Linux's zombie keeps
    /// its whole task; this kernel's record keeps the one field a selector
    /// needs.
    pub(crate) pgid: TaskId,
    /// The signal this task was born to report: the byte its `clone` carried,
    /// carried on the record for `pgid`'s reason - a `waitid` with `__WCLONE`
    /// or `__WALL` asks a *record* which kind of child died, and the task that
    /// held the answer is gone by the time it asks. See [`Task::exit_signal`].
    pub(crate) exit_signal: u8,
    /// The real user id this task ran as when it exited.
    ///
    /// Carried for the same reason `pgid` is: the slot is free by the time a
    /// record is read, and `waitid` owes the child's uid in the record it
    /// writes. Linux reports `task_uid(tsk)` - `cred->uid`, the *real* id, not
    /// the effective one - for every event class (measured on a current host:
    /// `CLD_EXITED`, `CLD_STOPPED`, `CLD_TRACED` and `CLD_CONTINUED` all carry
    /// it, while a call that faces no event at all is a cleared record
    /// instead). This kernel answered a zero here until `docs/uapi/` §5.1
    /// item 23, which is why the field is on the record rather than read from
    /// the task.
    pub(crate) uid: u32,
    /// Whether the death was one the kernel's coredump path classified -
    /// `CLD_DUMPED` in `waitid`'s record and the `WCOREDUMP` bit in `wait4`'s
    /// status word.
    ///
    /// Computed at the death and carried rather than re-derived, because it is
    /// not a fact about the exit code alone: it is the signal's default action
    /// **and** the dying process's own `dumpable` flag, and the flag is gone by
    /// the time a `wait` reads the record (the task is). See
    /// `crate::abi::exit_chld`, and `.workbuddy/probe_wcore_dumpable.c` beside
    /// it for the second half.
    pub(crate) dumped: bool,
    /// The CPU the dying group had used by the time it died, in internal clock
    /// ticks: its own `utime`/`stime` **plus the CPU its own reaped children
    /// folded in**.
    ///
    /// That sum is what Linux's `getrusage(p, RUSAGE_BOTH, ...)` reports for a
    /// child, which is what a `wait4`/`waitid` caller's `struct rusage` carries
    /// (measured, `.workbuddy/probe_wait4_rusage.c` scenario D, Linux 7.2.9: a
    /// grandchild's 200 ms lands in the reaping child's `ru_utime`, not only in
    /// the grandchild's own). Carried on the record for `uid`'s reason: the
    /// slot is free by the time a `wait` reads it, and the group's counters go
    /// with it.
    pub(crate) utime: u64,
    pub(crate) stime: u64,
    /// The tracer this task died under, if any. A traced task's death is the
    /// tracer's to collect: the tracer reaps the corpse first, and only its
    /// taking releases the record to the real parent - the same
    /// release-on-reap order Linux uses. Without it, a `strace -f timeout ...
    /// fish` run reports the fish exit to `timeout` while strace still owns
    /// the body, and the two wait loops race for one record.
    pub(crate) tracer: Option<TaskId>,
}

/// Exit codes, kept after the task is gone. The slot a task occupied is free
/// for reuse the moment it is reaped, but the code it exited with can still be
/// wanted - a milestone check reads it one tick later, and `wait` reads it
/// once. Reading it takes it: a code belongs to the one caller that collects
/// it, and anything left over is dropped oldest-first when the cap is reached.
pub(crate) static RETIRED: Mutex<RefCell<Vec<Retired>>> = Mutex::new(RefCell::new(Vec::new()));
pub(crate) const RETIRED_CAP: usize = 32;

/// Descriptions an exited process held, waiting to be dropped.
///
/// [`Table::release_exited`] runs with the task table borrowed, and dropping a
/// description can wake whoever was blocked on it - which needs the table. So
/// the files are parked here and dropped by the scheduler, which is the one
/// place that holds no borrow while still running often enough that a released
/// pipe reaches its reader promptly.
static RELEASE: Mutex<RefCell<Vec<alloc::vec::Vec<Arc<File>>>>> =
    Mutex::new(RefCell::new(Vec::new()));

fn queue_release(files: alloc::vec::Vec<Arc<File>>) {
    critical_section::with(|cs| RELEASE.borrow(cs).borrow_mut().push(files));
}

/// Drop whatever exited processes have handed over. Called by the scheduler
/// outside its borrow of the task table.
pub(crate) fn drain_release() {
    loop {
        let next = critical_section::with(|cs| RELEASE.borrow(cs).borrow_mut().pop());
        match next {
            Some(files) => drop(files),
            None => break,
        }
    }
}

/// `critical_section::Mutex` requires its contents to be `Send`, and the table
/// holds task bodies - `Box<dyn FnOnce()>`, which is not. On one core there is
/// nowhere to send them to, and the critical section is the only way in, so the
/// bound is met here rather than by infecting every task body with `Send`.
pub(crate) struct Shared(UnsafeCell<Table>);

unsafe impl Send for Shared {}

impl Shared {
    pub(crate) const fn new(table: Table) -> Shared {
        Shared(UnsafeCell::new(table))
    }

    /// Dereferencing the result is sound only inside a critical section, and the
    /// `&mut` must not outlive a context switch. See the module docs.
    pub(crate) fn get(&self) -> *mut Table {
        self.0.get()
    }
}

/// A task's stack, and where it came from.
pub(crate) struct Stack {
    pub(crate) base: usize,
    pub(crate) order: usize,
}

impl Stack {
    pub(crate) fn new() -> Stack {
        let base = mm::alloc_pages(STACK_ORDER).expect("out of memory for a task stack");
        unsafe { (base as *mut u64).write_volatile(STACK_CANARY) };
        Stack {
            base,
            order: STACK_ORDER,
        }
    }

    pub(crate) fn top(&self) -> usize {
        self.base + (PAGE_SIZE << self.order)
    }
}

/// Which of the three interval timers a `setitimer` call names. The numbers
/// are the ABI's own: `ITIMER_REAL`, `ITIMER_VIRTUAL`, `ITIMER_PROF`, in
/// that order.
pub const ITIMER_REAL: usize = 0;
pub const ITIMER_VIRTUAL: usize = 1;
pub const ITIMER_PROF: usize = 2;

/// One armed interval timer: when it fires and what it reloads to.
#[derive(Clone, Copy)]
pub struct ItimerState {
    /// The reload value in ticks. A one-shot timer has none; a periodic one
    /// reloads from where it was due, so a late check skips rather than
    /// queues the periods it missed.
    pub interval_ticks: u64,
    /// The absolute tick the timer fires at.
    pub deadline: u64,
}

/// What `getitimer` reports about one timer: its reload value and the time
/// left on it, in ticks.
#[derive(Clone, Copy)]
pub struct ItimerSnapshot {
    pub interval_ticks: u64,
    pub remaining_ticks: u64,
}

/// How many POSIX interval timers (`timer_create`) one task may hold. Linux
/// bounds them by `RLIMIT_SIGPENDING` — thousands — and grows them on the
/// heap; a fixed slot count bounds the same surface by construction, and the
/// programs that real: vim keeps one. A create past the last slot answers
/// `EAGAIN`, which is the errno a caller that probes reads as "out of
/// resources" — and the message musl hands back through `timer_create`.
pub const TIMER_SLOTS: usize = 8;

/// The `sigev_notify` values `timer_create` accepts, as the ABI numbers them.
///
/// `SIGEV_SIGNAL` is **0** and `SIGEV_NONE` is **1** - the order `linux/signal.h`
/// gives them, and the order `libc`'s linux table carries. The pair used to be
/// written the other way round here, and `abitest/lib/lin.rs` had the same
/// swap, so the two agreed with each other and disagreed with every libc: a
/// caller sending the real `SIGEV_NONE` (1) was read as `SIGEV_SIGNAL` and
/// refused `EINVAL` when its signo was left 0 (which is what a silent timer
/// does), and a caller sending the real `SIGEV_SIGNAL` (0) got a timer that
/// never fired. The migrated `t-timer-create` sends `libc::SIGEV_*` and found
/// it; the claim that 1 is the silent one and 0 the signalled one is pinned
/// beside the create.
pub const SIGEV_SIGNAL: u32 = 0;
pub const SIGEV_NONE: u32 = 1;
pub const SIGEV_THREAD_ID: u32 = 4;

/// `TIMER_ABSTIME`: `timer_settime`'s flag that names a deadline on the clock
/// rather than a delay from now.
pub const TIMER_ABSTIME: u32 = 1;

/// One POSIX interval timer: what it does when it fires, and whether it is
/// still counting.
#[derive(Clone, Copy)]
pub struct PosixTimer {
    /// The reload value in ticks, as `it_interval` spelled it. A one-shot
    /// timer has none; a periodic one reloads from where it was due, the same
    /// coalescing rule the itimers follow.
    interval_ticks: u64,
    /// The absolute tick the timer fires at. `None` is a disarmed timer —
    /// a `timer_settime` with a zero value, or a one-shot that has fired.
    /// The record stays either way: Linux keeps a timer valid until it is
    /// deleted, and `timer_gettime` of an expired one-shot answers zero.
    deadline: Option<u64>,
    /// The signal the expiry raises, in 1..=32 — 1..=31 being the range a
    /// disposition can name and 32 (`SIGRTMIN`) the number musl's
    /// `SIGEV_THREAD` helper waits on.
    signo: u8,
    /// The `union sigval` the creator asked the expiry to carry.
    value: u64,
    /// Where the signal goes: nowhere (`SIGEV_NONE`), the creating task
    /// (`SIGEV_SIGNAL`), or the named thread of the creator's group
    /// (`SIGEV_THREAD_ID`). A process-directed expiry aiming at the creator
    /// rather than "any thread that does not block it" is the one
    /// simplification this table owns, and it is spelled out on
    /// `handle_timer_create`, where its one real consumer cannot see it.
    notify: TimerNotify,
    /// Expiries that found the previous signal still pending — the overrun
    /// count `timer_getoverrun` reports. Standard signals are not queued, so
    /// an expiry that arrives while the last one is unread has nowhere to go
    /// but this counter, which is exactly what the count means.
    overrun: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TimerNotify {
    None,
    Signal,
    Thread(TaskId),
}

impl PosixTimer {
    /// Whether an expiry is due, and what should happen after it.
    fn expired(&self, now: u64) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }
}

pub(crate) struct Task {
    /// Must stay put for as long as the task exists.
    pub(crate) context: Context,
    /// This task's System V semaphore undo list - the adjustments a `semop`
    /// with `SEM_UNDO` recorded, to be applied when the last holder of the list
    /// exits (`sysv_sem::UndoList`).
    ///
    /// `None` until something asks for an undo record, which is why the common
    /// case costs nothing: a program that never uses `SEM_UNDO` never allocates
    /// one. `Arc` rather than a plain `Vec` because `CLONE_SYSVSEM` is a
    /// *sharing* flag - `pthread_create` passes it, so a process's threads
    /// adjust one list, and the list is applied when the last of them exits
    /// (measured: a child forked without the flag neither sees nor applies its
    /// parent's adjustments, `docs/notes/` §6.14).
    pub(crate) sem_undo: Option<Arc<crate::sched::sysv_sem::UndoList>>,
    /// Whether the hardware floating-point registers currently hold *this*
    /// task's state - the whole of the lazy-FPU bookkeeping, and the reason
    /// `context.fpu` is not written on every switch.
    ///
    /// At most one task may have it set, and only the running one: the switch
    /// saves and clears it on the way out, and the `#NM` handler sets it when
    /// it loads an image the switch set `CR0.TS` for. A task that has never run
    /// a floating-point instruction therefore keeps `false` for its whole life
    /// and never costs either a save or a restore.
    ///
    /// Read/written only by the x86_64 lazy-FPU functions; present but inert on
    /// aarch64 (which restores FP in the context switch), so it is unused there
    /// until that machine wires its own FPU path.
    #[allow(dead_code)]
    pub(crate) fpu_live: bool,
    pub(crate) priority: Priority,
    /// `None` only for the boot task, whose stack belongs to the bootloader.
    pub(crate) stack: Option<Stack>,
    /// Set when the body has returned. The task is not freed here: it is running
    /// on the stack that needs freeing, so the scheduler reaps it later.
    pub(crate) exited: bool,
    pub(crate) body: Option<Box<dyn FnOnce()>>,
    /// The ring-3 address space; `None` for tasks that only run kernel code.
    pub(crate) space: Option<AddressSpace>,
    /// Set for every task that was given a user image to run, and stays set
    /// after it exits: a zombie is still a process, and `/proc` - which has no
    /// address space of its own to look at any more - needs this to tell it
    /// from a kernel task, which never had one at all.
    pub(crate) user: bool,
    /// Where a user task's first dispatch goes: the entry point and the stack
    /// `enter_user` hands the CPU. Kernel tasks have none.
    pub(crate) user_entry: Option<(usize, usize)>,
    /// Where a *forked* child's first dispatch goes: the parent's ring-3
    /// register file at the moment of the call, with `rax` set to zero. A
    /// child resumes mid-program rather than at the entry point, so it needs
    /// a whole register file where a freshly spawned task needs one address.
    pub(crate) user_resume: Option<UserFrame>,
    /// The thread group this task runs in: the frames behind its mappings and
    /// how many threads share them. A process with no threads has a group of
    /// one, so every task has one and nothing has to ask whether it is a
    /// thread to find its memory.
    pub(crate) mm: Arc<super::child_mm::Mm>,
    /// Set once this task's exit has been accounted for. The release pass runs
    /// on every reap, and a thread that left twice would take the group's count
    /// down twice - which would free memory a sibling is still using.
    pub(crate) released: bool,
    /// The thread group's id - the leader's task id. `getpid` answers this from
    /// every thread; `gettid` answers the task's own.
    pub(crate) tgid: TaskId,
    /// Signals sent to this task and not yet delivered.
    pub(crate) signals: signal::Pending,
    /// The `siginfo_t` behind each *standard* pending signal, indexed like
    /// `signals`. A standard signal is not queued, so one record per number is
    /// all the ABI implies; this is what lets a `sigqueue` payload reach the
    /// handler. Real-time signals are queued instead — see [`rtq`](Self::rtq).
    pub(crate) siginfo: [signal::SigInfo; signal::SIGINFO_SLOTS],
    /// The pending *real-time* signals, a FIFO ring: Linux queues every copy of
    /// `SIGRTMIN..=SIGRTMAX` (each with its own `si_value`), unlike a standard
    /// signal which coalesces into the slot above. The matching bit in `signals`
    /// stays set while any record for that number is still in this ring.
    pub(crate) rtq: signal::RtQueue,
    /// The signals this task has blocked with `rt_sigprocmask`. A blocked
    /// signal stays pending until it is unblocked, which is what makes a
    /// critical section able to say "not now" and be obeyed.
    pub(crate) blocked: signal::Pending,
    /// What `rt_sigaction` recorded for this process, per signal. Its own
    /// table, not a shared one: a fresh process must answer `SIG_DFL` for
    /// everything, whatever its parent or its sibling installed.
    pub(crate) dispositions: signal::Actions,
    /// The alternate signal stack this process installed with
    /// `sigaltstack`, if it has one.
    pub(crate) alt_stack: Option<signal::AltStack>,
    /// The descriptors this task's fd numbers index. Kernel tasks share the
    /// kernel's table; a process has one of its own.
    pub(crate) fds: Arc<FdTable>,
    /// The process's filesystem context: the root absolute paths begin at, the
    /// cwd, the path that cwd was reached by, and the file-creation mask -
    /// Linux's `fs_struct`, and the one object `CLONE_FS` shares and
    /// `unshare(CLONE_FS)` splits.
    ///
    /// It is one `Arc` rather than four copied fields, and the sharing is the
    /// point rather than a tidy-up: with the four facts copied into a thread, a
    /// thread's `chdir` moved only that thread, where Linux's moves its whole
    /// process. Measured on a current host (7.2.9): a thread that `chdir`s to
    /// `/tmp` takes the creating thread's own `getcwd` with it, and only
    /// `unshare(CLONE_FS)` splits the two (`.workbuddy/probe_unshare_flags.c`).
    /// `crate::sched::fs` holds the object and the rule; this field is whose it
    /// is. The mount namespace is *not* in here - Linux keeps `fs_struct` and
    /// `nsproxy` apart, and so does the `fs_ns` slot below.
    pub(crate) fs: Arc<crate::sched::FsStruct>,
    /// The mount table this task resolves through - its namespace, in Linux's
    /// `nsproxy->mnt_ns`. `None` is the machine's one table (`vfs::mount::global`),
    /// which is where every task starts and what every boot step means;
    /// `unshare(CLONE_NEWNS)` replaces it with [`crate::vfs::mount::MountTable::copy`]
    /// of the current one, and `fork`/`clone` hand it on **by reference** — the
    /// child shares the parent's table until it unshares its own, exactly as the
    /// `uts` slot beside it works. A task's view of "what is mounted where" is
    /// therefore two facts held together: this table and the `root` above, which
    /// is why `current_fs_context` answers both in one call.
    pub(crate) fs_ns: Option<Arc<crate::vfs::mount::MountTable>>,
    /// The task that spawned this one in ring 3, if any. Its `wait` blocks on
    /// this task's exit; kernel-spawned tasks have none.
    pub(crate) parent: Option<TaskId>,
    /// The signal this task's `clone` was born to report - the low byte of the
    /// flag word, which is not a flag at all.
    ///
    /// It is what tells the two kinds of child apart at the *other* end: a task
    /// born to report `SIGCHLD` is a "non-clone" child, one born to report
    /// nothing (`0`) or something else is a "clone" child, and `wait`'s
    /// `__WCLONE`/`__WALL` bits choose which of the two a caller may hear
    /// ([`ChildKind`]). Kept as the byte the caller passed rather than as a
    /// boolean because the byte is also what Linux *delivers* on the death
    /// (measured: `clone(SIGTERM)` puts a `SIGTERM` with `si_code` 2 and the
    /// child's pid in the parent's queue, `.workbuddy/probe_wclone.c` C2) - that
    /// half is still named in `docs/uapi/` §5.2, and this field is its first
    /// half.
    ///
    /// `fork` and `vfork` are `clone(SIGCHLD)` on Linux and set it here too;
    /// a thread is given `SIGCHLD` whatever its word said, because Linux forces
    /// a thread's exit signal away (it never notifies) and this kernel's threads
    /// do retire a record for their creator - rewriting the byte there would
    /// turn every `pthread_create` into a child a default `wait` could not see.
    pub(crate) exit_signal: u8,
    /// The process group this task belongs to, named by its leader's id.
    ///
    /// It is what a terminal's interrupt is aimed at: Ctrl-C is not sent to
    /// the process reading the keyboard, it is sent to the *group* the
    /// foreground job lives in, which is why a pipeline of three programs all
    /// stop together and the shell that started them does not. A task inherits
    /// its parent's group, so `setpgid(0, 0)` in a child - which is what a
    /// shell does before `exec` - is the whole of what makes that job separate.
    pub(crate) pgid: TaskId,
    /// The session this task belongs to, named by its leader's id. Zero for
    /// tasks no `setsid` has claimed - the boot task, kernel tasks. What a
    /// session carries here is not a controlling terminal (that is `ctty`,
    /// a per-process property) but the orphan test: a process group whose
    /// members' parents all sit in other sessions - or nowhere - is a group
    /// nobody could un-stop, and stop signals are discarded for it.
    pub(crate) session: TaskId,
    /// The controlling terminal this process acquired with `TIOCSCTTY`, by
    /// device id. `/dev/tty` is a door to exactly this terminal for exactly
    /// this process, and a process that has acquired none cannot open the
    /// name at all - the answer a shell reads as "job control off". Inherited
    /// across `fork`, preserved by `exec`, lost at `setsid`.
    pub(crate) ctty: Option<(u32, u32)>,
    /// Who this process is - the real, effective and saved ids of its user
    /// and group, in that order. Root until something changes it, inherited
    /// across `fork`, preserved by `exec`.
    pub(crate) identity: Identity,
    /// The UTS namespace this task reads its names from: `uname(2)`'s `nodename`
    /// and `domainname`, and the pair `/proc/sys/kernel/{hostname,domainname}`
    /// report and write.
    ///
    /// An `Arc` rather than the two names themselves, because a namespace is
    /// something tasks *share*: a child inherits its creator's by reference and
    /// goes on reporting the same names until it asks for its own, at which
    /// point `unshare(CLONE_NEWUTS)` hands it a copy - Linux's `copy_utsname` -
    /// and the two diverge. Two owned `String`s per task would pass every test a
    /// single task can write and read like a different mechanism the first time
    /// a parent renamed itself after a `fork`. `src/uts.rs` holds the object and
    /// the rule; this field is only whose it is.
    pub(crate) uts: Arc<crate::uts::Uts>,
    /// The PID namespace this task's **own** numbers come from: what `getpid(2)`
    /// answers, and what `kill`, `wait4` and `/proc/<n>` mean when a reader in
    /// the same namespace asks about this task.
    ///
    /// Inherited by reference and *never* replaced by `unshare` - which is the
    /// point of the field below being a second one. A task that unshares keeps
    /// its number, so the namespace it already lives in must keep naming it.
    /// `crates/pidns` holds the object and the rule, `sched::pid` the two
    /// directions of the translation; this field is only whose it is.
    pub(crate) pid_ns: Arc<crate::pidns::PidNs>,
    /// The network namespace this task resolves and binds through, as the
    /// number `net` knows the stack by - `None` for the machine's own, the shape
    /// `fs_ns` above uses for "the machine's table".
    ///
    /// A *number* rather than an `Arc` of the object, and that is deliberate: the
    /// stacks live in `net`'s registry, and a `sched` holding an `Arc<NetNs>`
    /// would have to name `net` - which already names `sched` (it wakes tasks),
    /// so that would be a mutual dependency the design does not have
    /// (`ARCHITECTURE.md` §2/§3). Nothing else in the task table ever looks at it;
    /// `net` resolves it when a socket call asks.
    ///
    /// Inherited by reference across `fork`/`clone`, and given a fresh value by
    /// `clone(CLONE_NEWNET)`, which is where a namespace is *made* for a child.
    /// `unshare`/`setns` moving a live task are not served yet, and the reason is
    /// named: a socket here lives in its namespace's stack, and the registry's
    /// handles are per-stack indices, so moving a task with sockets open would
    /// let an inherited fd name a socket of the new namespace instead of its own
    /// (`docs/notes/` §6.3).
    pub(crate) net_ns: Option<u64>,
    /// The IPC namespace this task's `ipcs` objects live in, by the number the
    /// four tables key their state by.
    ///
    /// `None` is the machine's own, the value every task carries until something
    /// unshares; a real namespace is the **object**, held by `Arc`, so the last
    /// task to leave it (and the last descriptor naming it) frees the four
    /// tables' state for it (`docs/notes/` §6.16). Inherited by reference across `fork`/`clone` (measured: a
    /// child's `msgctl` finds its parent's queue), and moved by
    /// `unshare(CLONE_NEWIPC)` and by `setns` on `/proc/<pid>/ns/ipc` - unlike
    /// [`Self::net_ns`], which no live task may leave because a socket's stack
    /// is fixed at creation; an IPC object is a *copy* (a queued message, a
    /// set's counts) or a mapping whose address-space bookkeeping is the
    /// segment table's, so a task that moves keeps resolving numbers through
    /// the namespace it is in now, and an id from the old one answers `EINVAL`
    /// (measured, §6.16).
    pub(crate) ipc_ns: Option<alloc::sync::Arc<crate::vfs::ns::IpcNs>>,
    /// `PR_SET_NO_NEW_PRIVS`: whether this task has given up the right to gain
    /// privileges through `execve`, and whether it may therefore install a
    /// seccomp filter without `CAP_SYS_ADMIN`.
    ///
    /// Per thread, **inherited** by `fork`/`clone`, preserved by `exec`, and
    /// **one-way**: measured on a current host (`.workbuddy/probe_nnp.c`), the
    /// setter takes exactly `(1, 0, 0, 0)` - `2`, a non-zero later argument and
    /// a clearing `0` are all `EINVAL` - and the getter refuses a call whose
    /// unused arguments are not zero (a C caller that wrote `prctl(39)` and let
    /// the variadic tail be garbage gets `EINVAL`, which is how the first probe
    /// of this read "unimplemented" for half an hour).
    pub(crate) no_new_privs: bool,
    /// `PR_SET_CHILD_SUBREAPER`: whether an orphan of this task's descendants is
    /// reparented to it rather than to init.
    ///
    /// Per **process**, not per thread: `prctl` writes the group leader's slot
    /// and reads it back from there, which is Linux's `signal_struct` field in
    /// this kernel's vocabulary. Measured on a current host
    /// (`.workbuddy/probe_subreaper.c`): the value is a boolean (7 is stored as
    /// 1), it is **not inherited by `fork`**, it survives `exec`, and the
    /// behaviour it buys is that a grandchild whose own parent dies is reaped by
    /// the marked ancestor instead of by init.
    pub(crate) subreaper: bool,
    /// `PR_SET_PDEATHSIG`: the signal the kernel sends to this task when its
    /// parent dies, or 0 for none.
    ///
    /// Measured on a current host (`.workbuddy/probe_pdeathsig.c`): the value
    /// survives `execve` (a process may inherit one from the chain that started
    /// it - this sandbox's shell carries `SIGKILL`), is **cleared for the child
    /// of a `fork`**, accepts any value `0..=_NSIG` (`SIGKILL` and `SIGSTOP`
    /// included, which is why nothing filters them), and an invalid one is
    /// `EINVAL` with the old value kept. Delivered by the exit path, at the same
    /// walk that hands the dying task's children on.
    pub(crate) pdeathsig: u32,
    /// The namespace this task's **children** are created in - Linux's
    /// `nsproxy->pid_ns_for_children`.
    ///
    /// It starts as the task's own namespace, so an ordinary `fork` in a
    /// machine that never unshares is a no-op here, and `unshare(CLONE_NEWPID)`
    /// or `setns(fd, CLONE_NEWPID)` replaces **this** one only. That single
    /// sentence is the whole of "the call renumbers the caller's children and
    /// not the caller": the new namespace is empty and its first member is 1,
    /// while the task that asked for it goes on answering with the number it
    /// had. A child's `pid_ns` is its creator's value of this field, and the
    /// child is registered in that namespace **and every namespace above it**,
    /// which is what lets the parent, one level out, wait for it and find it in
    /// `/proc`.
    pub(crate) pid_ns_children: Arc<crate::pidns::PidNs>,
    /// The time namespace this task's **own** clocks come from: the offsets
    /// `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE` and
    /// `CLOCK_BOOTTIME` are read through.
    ///
    /// Inherited from the *creating* task's [`Task::time_ns_children`] - not
    /// from its `time_ns` - which is the whole mechanism of the kind: a task
    /// that `unshare(CLONE_NEWTIME)` called keeps reading its own clocks through
    /// the namespace it was already in, and only the children it makes
    /// afterwards wake up in the new one. The measured shape is that a caller's
    /// own `CLOCK_MONOTONIC` never moves, whatever it sets the offsets to
    /// (§6.3, and `.workbuddy/probe-timens*.c`).
    pub(crate) time_ns: Arc<crate::timens::TimeNs>,
    /// The time namespace this task's **children** are created in - Linux's
    /// `nsproxy->time_ns_for_children`.
    ///
    /// Equal to [`Task::time_ns`] until something unshares, so an ordinary task
    /// is in one namespace and creates its children in it; `unshare`/`setns`
    /// replace **this** one alone, which is the same rule
    /// [`Task::pid_ns_children`] follows and the reason `/proc/<pid>/ns` has a
    /// separate `time_for_children` name. `/proc/<pid>/timens_offsets` reads and
    /// writes *this* namespace's offsets, because they are the ones the next
    /// child will be born under - which is what makes the file useful at all.
    pub(crate) time_ns_children: Arc<crate::timens::TimeNs>,
    /// The user namespace this task's ids are read through: the table every
    /// observation of an id - `getuid`'s family, and the owner a `stat` reports
    /// - is translated by.
    ///
    /// **One field, not two.** Unlike PID and TIME, a user namespace has no
    /// children's slot: `unshare(CLONE_NEWUSER)` moves the caller into the new
    /// namespace (measured, §6.3 - the same shape `CLONE_NEWNET` has), so there
    /// is no `user_for_children` name in `/proc/<pid>/ns` and a child simply
    /// inherits the value its creator holds. The stored ids on this task are in
    /// the *parent* namespace's numbering, which is why a fresh namespace shows
    /// its own caller as [`crate::userns::NO_MAP_ID`] until the map is written.
    pub(crate) user_ns: Arc<crate::userns::UserNs>,
    /// The root of the cgroup hierarchy this task's group path is rendered
    /// from, when the task is inside a cgroup namespace: the group its
    /// creator was in at the moment `CLONE_NEWCGROUP` moved it, served by
    /// that namespace as `/`. `None` is the machine's own view - every task
    /// carries it until something unshares - and the answer then is the
    /// machine's path from the real root, exactly what a kernel without a
    /// cgroup namespace serves.
    ///
    /// A cgroup namespace has no objects and no tables: its one state is
    /// this root, so the kind is a path rather than an `Arc` - held by
    /// reference across `fork`/`clone` and pinned at the move, the way the
    /// user namespace is.
    pub(crate) cgroup_ns: Option<alloc::sync::Arc<str>>,
    /// The OOM-killer's score adjustment and the floor of it: the two numbers
    /// Linux keeps as `sig->oom_score_adj` and `sig->oom_score_adj_min`, and
    /// which `/proc/<pid>/oom_score_adj` reads and writes.
    ///
    /// Linux keeps them in the thread group's `signal_struct`; one task per
    /// process here, so they are per-task fields that behave the same way -
    /// inherited across `fork`, kept across `exec`. The floor is the point of
    /// the pair: a process may make itself as unkillable as it likes once, but
    /// going *below* what it has already asked for needs `CAP_SYS_RESOURCE`,
    /// which stops a privileged process from being talked back into the line
    /// by something it later runs. Nothing in this kernel scores a process for
    /// killing yet, so the pair is state with no policy on it - but the file
    /// has to hand back what it was given, or a write to `oom_score_adj` would
    /// read as a lie the next time anything asks.
    pub(crate) oom_score_adj: i32,
    pub(crate) oom_score_adj_min: i32,
    /// The nice value, the `-20..19` number `setpriority` records and
    /// `getpriority` reports (in the raw syscall's `20-nice` encoding). Like
    /// `oom_score_adj` it is state this kernel keeps honestly rather than
    /// schedules by: the scheduler's priority *bands* are its policy and a
    /// caller cannot move a task between them, so the nice has no weight effect
    /// here (a named sub-gap) - but a program that set it and read it back must
    /// see what it set, or the call is the silent-success `NOTES.md` §4 forbids.
    /// Inherited across `fork`, kept across `exec`, as Linux keeps it.
    pub(crate) nice: i32,
    /// The process's resource limits, the `RLIMIT_*` table `getrlimit`/
    /// `setrlimit`/`prlimit64` read and write and `/proc/<pid>/limits` prints.
    /// Inherited whole across fork and exec, the way a limit belongs to the
    /// process rather than to the program it last ran.
    pub(crate) rlimits: [Rlimit; RLIM_NLIMITS],
    /// `SECCOMP_MODE_STRICT` is on for this thread: only `read`, `write`,
    /// `exit` and `sigreturn` are permitted and every other syscall is fatal
    /// (`SIGKILL`). Once set it cannot be cleared, and it survives `exec` and
    /// is inherited across `fork` - so it is a per-task latch, not a limit.
    pub(crate) seccomp_strict: bool,
    /// The `SECCOMP_MODE_FILTER` programs this thread has installed, oldest
    /// first. Cumulative the way Linux's are - a filter can be added and never
    /// taken away - and every one of them runs on every syscall, with the most
    /// restrictive answer deciding. Inherited across `fork`/`clone` and left
    /// alone by `exec`, both of which are the ABI's own rules.
    pub(crate) seccomp_filters: Vec<crate::seccomp::Program>,
    /// Per-process brk state for heap management. The frames behind the
    /// break's pages are on `user_frames` like every other frame the process
    /// owns; only the bookkeeping of *where* the break is stays here.
    pub(crate) brk_base: usize,
    pub(crate) brk_pages: usize,
    /// Per-process mmap cursor: where the next `mmap(MAP_ANONYMOUS)` that
    /// does not name an address starts looking. Each process starts where its
    /// layout drew it and only moves downward; no other process shares this.
    pub(crate) mmap_cursor: usize,
    /// One past the highest address this process's stack may use.
    ///
    /// Part of the same drawn layout as `brk_base` and `mmap_cursor`, and kept
    /// for the same reason: it is not a constant any more. Stack growth answers
    /// "is this address still mine?" with it, and a process that grew past its
    /// own top would be mapping over whatever the kernel had put below it.
    pub(crate) stack_top: usize,
    /// The path this process was started from - what `/proc/self/exe` reads
    /// through. Empty for tasks that never had an image (kernel tasks, the
    /// boot task).
    pub(crate) exe: alloc::string::String,
    /// The argument vector this process was entered with, `argv[0]` included -
    /// what `/proc/<pid>/cmdline` reads through, and the only record of it:
    /// the block was written onto the process's own stack, and once the
    /// program is running it is free to write over it.
    ///
    /// Shared rather than copied so that `fork` - which copies a whole process
    /// for a child that will usually `exec` something else in the next
    /// instruction - hands over one reference instead of cloning every string.
    /// `execve` replaces the whole vector, which drops the old one.
    pub(crate) argv: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    /// The environment the process was entered with, one `KEY=VALUE` per
    /// element - what `/proc/<pid>/environ` reads through.
    ///
    /// Kept for exactly the reasons `argv` is, and kept *only* here for the
    /// same reason: the block `execve` laid on the process's own stack is the
    /// program's to overwrite the moment it runs, so this vector is the one
    /// record of what it started with. Shared with `fork`, replaced (and the
    /// old one dropped) by `execve`.
    ///
    /// The file it feeds is not decoration. Read back as NUL-separated bytes it
    /// is how a program asks "what was I started with", and one that does is
    /// OpenRC: its `init.sh` decides whether `/proc` still needs mounting by
    /// reading `/proc/self/environ` twice under different `VAR=` values and
    /// comparing the digests. With the file absent the `[ -e ]` fails, so it
    /// concluded "not mounted" and mounted over a live procfs - `mount:
    /// mounting proc on /proc failed: Resource busy` on the Alpine boot.
    pub(crate) env: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    /// Where a child created by `clone(CLONE_CHILD_SETTID)` writes its own tid
    /// before it first runs. Zero when nothing was asked for.
    pub(crate) child_tid: usize,
    /// The thread-local storage base (FS) this task runs with. Per task, not
    /// per CPU: two processes each with their own TLS would otherwise overwrite
    /// each other's on every dispatch. `arch_prctl(ARCH_SET_FS)` and
    /// `clone(CLONE_SETTLS)` are what move it.
    pub(crate) tls: usize,
    /// The three interval timers `setitimer` arms, indexed by the ABI's
    /// `which`. `ITIMER_REAL` really expires - it counts the wall-clock
    /// ticks the board has; the other two are recorded but never fire, for
    /// want of per-task CPU accounting.
    pub(crate) itimers: [Option<ItimerState>; 3],
    /// The POSIX interval timers `timer_create` made, indexed by the id the
    /// call answered with. A timer belongs to the task that created it, not
    /// to its thread group - the divergence `handle_timer_create` records.
    pub(crate) timers: [Option<PosixTimer>; TIMER_SLOTS],
    /// What a tracer has made of this task, if anything.
    pub(crate) trace: Tracing,
}

/// What `ptrace` has made of a task.
///
/// One field rather than half a dozen on `Task`, because a task no tracer has
/// ever touched is the overwhelmingly common case and deserves one word of "no"
/// rather than six. [`Tracing::default`] is that case exactly: untraced, no
/// options, running.
#[derive(Clone, Copy, Default)]
pub(crate) struct Tracing {
    /// The task tracing this one. Set by `PTRACE_TRACEME` from the tracee's own
    /// call and by `PTRACE_ATTACH` from the tracer's.
    ///
    /// This is the whole of the relationship: a task is traced iff something is
    /// named here, and every stop below is gated on it. It is deliberately not
    /// `parent` - a tracee is usually its tracer's child, but not always, and
    /// `PTRACE_DETACH` has to be able to take the link away without touching
    /// the family.
    pub(crate) tracer: Option<TaskId>,
    /// The `PTRACE_O_*` word the tracer set with `PTRACE_SETOPTIONS`.
    pub(crate) options: usize,
    /// Where this task is stopped. `None` is a running, ready or sleeping task;
    /// `Some` is a task sitting still with its history waiting to be read.
    pub(crate) stop: Option<Stop>,
    /// Set by `PTRACE_SYSCALL`: stop at the next syscall boundary.
    ///
    /// Cleared by the stop it causes, not by the resume - which is exactly what
    /// makes the entry and the exit two separate events. A tracer that wants
    /// both re-arms on every resume, and the alternation follows.
    pub(crate) armed: bool,
    /// The user register file as of the stop.
    ///
    /// Saved rather than pointed at, twice over: the frame it came from is on
    /// the tracee's kernel stack, which nobody hands out a pointer to, and a
    /// `PTRACE_SETREGS` needs somewhere to put what it changed.
    ///
    /// Sized by the *machine's* register-file width ([`AbiRegs`]) rather than by
    /// any one architecture's ABI table, so the field exists - empty and unused
    /// - on a machine with no ptrace handler yet, and needs no gate.
    pub(crate) regs: Option<AbiRegs>,
    /// The syscall the tracee stopped in the middle of. Written at the entry
    /// stop and kept for the exit stop, because `PTRACE_GET_SYSCALL_INFO`
    /// describes both ends of one call and the exit half has no arguments of
    /// its own.
    pub(crate) syscall: Option<SyscallStop>,
    /// Set by an exec trap: the syscall-exit stop that would otherwise follow
    /// it has already been reported, and must not be reported twice.
    ///
    /// The trap *is* the `execve`'s completion notice - that is what Linux's
    /// exec trap is for - so an exit stop after it would tell a tracer the same
    /// call had returned a second time. Cleared by the next entry stop, which is
    /// the only thing that can follow a disarmed resume.
    ///
    /// Set and read only by the exec-trap and syscall-stop path, and starts
    /// false on a machine whose syscall handler does not raise those stops.
    pub(crate) at_exec_exit: bool,
    /// A `SIGCONT` ended a stop of this task and no `wait` has reported it yet.
    ///
    /// It outlives the stop itself, and has to: by the time a parent looks, the
    /// task is running again and the stop it was told about is gone. That is
    /// the `WIFCONTINUED` word, and the reason `wait` takes the flag at all.
    pub(crate) continued: bool,
    /// The event stop this task owes its tracer for a child it has just made,
    /// as `(PTRACE_EVENT_*, child pid)`.
    ///
    /// It travels this way because of *where* the two halves have to happen.
    /// The child is created deep inside the scheduler, and the stop has to be
    /// raised where the syscall frame is - a stop is a change to the registers
    /// the task will return through, and only the syscall layer has those. The
    /// alternative is raising the stop at the creation, in a place with no
    /// frame, which would report the parent's state from before the call
    /// instead of from inside it - and Linux raises it inside.
    ///
    /// `None` is the overwhelmingly common case: nobody is tracing, or the
    /// tracer did not ask about this kind of child.
    pub(crate) pending_event: Option<(u32, u64)>,
}

/// The syscall a tracee is stopped at, as `PTRACE_GET_SYSCALL_INFO` reports it.
#[derive(Clone, Copy)]
pub struct SyscallStop {
    /// The call's number - `orig_rax` in the register file's terms.
    pub nr: u64,
    /// The six arguments, as they were when the call was made. Kept across the
    /// pair of stops because the exit half has none of its own.
    pub args: [u64; 6],
    /// Where the `syscall` instruction will return to, and the stack it was
    /// issued on: the `instruction_pointer` and `stack_pointer` fields.
    pub ip: u64,
    pub sp: u64,
    /// Whether this is the stop on the way *in*. What the tracer reads to know
    /// whether it is looking at a call's arguments or at its result.
    pub entry: bool,
}

/// A task held where it stands, and what the thing that put it there will be
/// told.
///
/// Two things stop a task, and they differ only in who collects the news. A
/// tracer stops a tracee at every syscall boundary and reports every signal
/// before it is delivered; a stop signal with no tracer stops the task for its
/// parent. Both are this structure, and `traced` is the flag that says which.
#[derive(Clone, Copy)]
pub struct Stop {
    /// The signal `wait(2)` reports as `WSTOPSIG`. For a syscall boundary it is
    /// `SIGTRAP`, with `TRACESYSGOOD`'s `0x80` set when the tracer asked for it;
    /// for a signal-delivery stop it is the signal itself.
    pub signal: u8,
    /// The `PTRACE_EVENT_*` the status word carries in its high byte. Zero for
    /// everything this kernel produces: a syscall boundary, a signal, an exec.
    pub event: u32,
    /// Whether a tracer is the one that will be told. A group stop has no
    /// tracer: it goes to the parent, and only to a parent that asked with
    /// `WUNTRACED`.
    pub traced: bool,
    /// Whether this is a *group* stop: the task was held by a stop signal of its
    /// own rather than by its tracer, and a `SIGCONT` may end it.
    ///
    /// It is deliberately not the same question as `traced`. A group stop of a
    /// traced task is reported to the tracer - it has to be, or nothing would
    /// ever resume it - but it is still the tracee's own stop, and `SIGCONT`
    /// ends it whether or not a tracer is watching. A tracer's stop is the
    /// tracer's alone, and no signal ends that one.
    pub group: bool,
    /// Set once a `wait` has reported it. News is news once, and the tracee
    /// then sits in the stop until something resumes it.
    pub reported: bool,
    /// What `PTRACE_GETEVENTMSG` answers while the tracee sits here.
    ///
    /// Only the events carry one, and each carries a different thing: a fork,
    /// a vfork or a clone reports the **new child's pid**, an exec reports the
    /// **tid the task had before it exec'd**. Zero - "no message" - is a
    /// truthful answer for every other kind of stop, because there is no event
    /// for a message to belong to.
    ///
    /// It lives on the stop rather than on the task because that is the
    /// question the tracer is asking: *this* stop's message. A task that ran
    /// twice carries the second answer, and the first one is gone.
    pub message: u64,
    /// The signal this stop is reporting, when it *is* a signal-delivery stop.
    ///
    /// It is what makes resuming such a stop mean something: the tracer either
    /// passes the signal on or drops it, and Linux's rule is that the signal the
    /// tracer names is the one delivered, in place of this one either way. A
    /// syscall stop names nothing, because there is nothing to replace.
    pub delivering: Option<u8>,
    /// The syscall this stop is a boundary of, when it is one.
    ///
    /// It travels on the stop rather than being read out of the task, because
    /// what `PTRACE_GET_SYSCALL_INFO` must answer depends on *which* stop the
    /// tracer is looking at: an exec event and a signal delivery are stops too,
    /// and for those the honest answer is "this is not a syscall stop".
    pub syscall: Option<SyscallStop>,
}

impl Task {
    /// The boot task: already running, with no body left to start and no stack
    /// of ours beneath it. Its context is empty until it first switches away.
    pub(crate) fn adopted() -> Task {
        Task {
            context: Context::empty(),
            sem_undo: None,
            priority: Priority::NORMAL,
            stack: None,
            exited: false,
            // The boot task has never run a floating-point instruction, and
            // nothing loaded registers for it - whatever the bootloader left in
            // them is not its state.
            fpu_live: false,
            mm: Mm::new(TaskId(0), 0, alloc::vec::Vec::new()),
            released: false,
            tgid: TaskId(0),
            body: None,
            space: None,
            user: false,
            user_entry: None,
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            fs: crate::sched::FsStruct::new(crate::abi::DEFAULT_UMASK),
            fs_ns: None,
            parent: None,
            // Nothing waits for a kernel task, so the kind it was "born to
            // report" is unobservable; `SIGCHLD` is the shape a `fork` child
            // has, which is what these tasks are descended from.
            exit_signal: signal::SIGCHLD,
            pgid: TaskId(0),
            session: TaskId(0),
            ctty: None,
            identity: Identity::default(),
            // A kernel task starts in the machine's own namespace, the one
            // every task is born in until something unshares.
            // The machine's one UTS namespace (`crates/uts`'s `init` keeps the
            // first). The id is minted here because the pool is the kernel's;
            // only the first call's is worn, and a number nothing wears is the
            // price of a leaf that cannot mint its own.
            uts: crate::uts::init(crate::vfs::ns::mint()),
            // The machine's own PID namespace: the one a boot begins in, whose
            // numbers are the task slots themselves. A task created here is
            // registered nowhere - the init namespace's `task_of` is the
            // identity - so this is the field pair every task starts with and
            // nothing observable changes until something unshares.
            pid_ns: super::root_ns(),
            pid_ns_children: super::root_ns(),
            // The machine's own time namespace, in both slots: a kernel task is
            // in it and creates its children in it, which is the `pid_ns` pair
            // above replayed for the clocks. It is frozen from the moment it
            // exists - `init` is in it - which is why the offsets file refuses
            // every write on a machine that never unshared.
            time_ns: crate::vfs::ns::machine_time_ns(),
            time_ns_children: crate::vfs::ns::machine_time_ns(),
            // The machine's own user namespace: the identity map, so every id
            // reads back as itself and no reader changes until something
            // unshares.
            user_ns: crate::vfs::ns::machine_user_ns(),
            cgroup_ns: None,
            // The machine's own network namespace, which is `None`: every socket
            // call resolves to the stack the boot built.
            net_ns: None,
            // And the machine's own IPC namespace, `None` for the same reason:
            // every SysV and POSIX queue resolves to the tables the boot made.
            ipc_ns: None,
            // No parent-death signal: nothing has asked for one.
            pdeathsig: 0,
            // And not a subreaper: init is the machine's only one at boot.
            subreaper: false,
            // No privilege given up yet, and none asked for.
            no_new_privs: false,
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::mm::elf::MMAP_BASE,
            stack_top: USER_STACK_TOP,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            // A kernel task or the boot task: no image, so nothing was handed
            // to it and `/proc/<pid>/environ` reads empty - the honest answer
            // for a thread that was never exec'd.
            env: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        }
    }
}

pub(crate) struct Table {
    pub(crate) tasks: [Option<Task>; MAX_TASKS],
    pub(crate) sched: Scheduler,
    pub(crate) current: TaskId,
    /// The number the slot's **current** tenant was given when it took the
    /// slot, and the counter that hands the next one out.
    ///
    /// A `TaskId` is a slot index, and [`alloc_slot`](Table::alloc_slot) hands
    /// the first free one to the next tenant - so a descriptor that stored a
    /// bare `TaskId` would name whatever task took the slot after the one it
    /// meant was reaped. The generation is what makes a pidfd
    /// (`crate::vfs::pidfd`) a reference to a *task* rather than to a slot: it
    /// is read when the descriptor is made and compared on every later use.
    /// Zero means "never allocated", which is why the first tenant gets one.
    pub(crate) generations: [u64; MAX_TASKS],
    pub(crate) next_generation: u64,
}
impl Table {
    pub(crate) const fn new() -> Table {
        Table {
            tasks: [const { None::<Task> }; MAX_TASKS],
            sched: Scheduler::new(),
            current: SCHED_ID,
            // Slot 1 (init) and the top slot (the scheduler) are the two
            // tenants that never come through `alloc_slot`: they are written
            // into their reserved slots directly. Giving every slot generation
            // 1 says "the first tenant", and starting the counter at 2 keeps
            // the next allocated one from reusing that number. A stale pidfd
            // therefore reads "gone" both for a reused slot (different number)
            // and for the reserved ones (their tenant is the first forever).
            generations: [1; MAX_TASKS],
            next_generation: 2,
        }
    }

    pub(crate) fn task(&self, id: TaskId) -> &Task {
        self.tasks[id.0]
            .as_ref()
            .unwrap_or_else(|| panic!("there is no task {}", id.0))
    }

    pub(crate) fn task_mut(&mut self, id: TaskId) -> &mut Task {
        self.tasks[id.0]
            .as_mut()
            .unwrap_or_else(|| panic!("there is no task {}", id.0))
    }

    pub(crate) fn context_of(&mut self, id: TaskId) -> *mut Context {
        &raw mut self.task_mut(id).context
    }

    /// Make `id` the running task and hand back the pair of contexts to switch
    /// between.
    ///
    /// Aiming the machine at the incoming task happens here, on both paths in:
    /// a hand-over entering the scheduler and a pick entering a chosen task.
    /// It runs inside the critical section, so no interrupt can observe the
    /// half-aimed state, and the CR3 write is safe before the switch itself
    /// because the kernel half is mapped in every address space - the outgoing
    /// task's stack and this code stay reachable until the last moment.
    pub(crate) fn enter(&mut self, id: TaskId) -> (*mut Context, *const Context) {
        self.prepare(id);
        let from = self.current;
        #[cfg(mpos_fpu_trap)]
        self.hand_over_fpu(from, id);
        self.current = id;
        (self.context_of(from), self.context_of(id))
    }

    /// The floating-point half of a switch, on the near side of it.
    ///
    /// The leaving task's registers are written out only if they are actually
    /// its own - `fpu_live` says so - and the entering task is marked as not
    /// holding the registers, with `CR0.TS` left to the assembly to set (see
    /// `mpos_context_switch`: doing it here would put the kernel in the window
    /// where a floating-point instruction faults with the table lock held).
    ///
    /// Writing the leaving task out here, rather than in the switch itself, is
    /// what makes the save correct at all: this runs while the outgoing task is
    /// still the one on the CPU, and the kernel between its last ring-3
    /// instruction and this point never touches an XMM register, so the
    /// registers are uncontroversially its. One consequence is worth naming: a
    /// task that is stopped or preempted always has a current `context.fpu`,
    /// which is exactly what `PTRACE_GETFPREGS` reads.
    ///
    /// x86_64's lazy-FPU scheme only. aarch64 has no `#NM` device-not-available
    /// trap: `mpos_context_switch` saves and restores the floating-point half
    /// *eagerly* into each context's `FpuState` on every switch, so there is
    /// nothing for the scheduler to hand over and no flag to clear. On that
    /// machine this function does not exist and `enter` does not call it.
    #[cfg(mpos_fpu_trap)]
    pub(crate) fn hand_over_fpu(&mut self, from: TaskId, to: TaskId) {
        if self.task(from).fpu_live {
            // Safe: `fpu_live` implies `CR0.TS` is clear - the only two things
            // that set the bit are this function and the switch it feeds, and
            // both clear the flag of every task they aim the bit at.
            debug_assert!(!hal::fpu_trapped());
            let task = self.task_mut(from);
            unsafe { hal::save_live_fpu(task.context.fpu_mut()) };
            task.fpu_live = false;
        }
        self.task_mut(to).fpu_live = false;
    }

    /// The floating-point image a child of `id` starts with.
    ///
    /// `fork` and a thread-creating `clone` copy the creator's registers, and
    /// `bootstrap` would otherwise hand the child the architectural reset state.
    /// Under the lazy scheme the *live* registers are the creator's only while
    /// `fpu_live` says so; when it does not, the registers belong to no task at
    /// all - the last owner had them written out on the way past - and copying
    /// them would hand the child whichever program happened to hold them last.
    /// The authoritative copy is then the one already in `context.fpu`.
    ///
    /// Writing the creator's image back first, while it is still the running
    /// task, is also what keeps `PTRACE_GETFPREGS` honest: a running task's
    /// saved block is stale by design, and this and the switch are the only two
    /// places that make it current.
    pub(crate) fn inherit_fpu(&mut self, id: TaskId) -> hal::Fpu {
        let task = self.task_mut(id);
        #[cfg(mpos_fpu_trap)]
        if task.fpu_live {
            debug_assert!(!hal::fpu_trapped());
            // Safe: `fpu_live` implies `CR0.TS` clear, by the same argument as
            // in `hand_over_fpu`.
            unsafe { hal::save_live_fpu(task.context.fpu_mut()) };
        }
        // On x86_64 the branch above makes the saved block current first; on
        // aarch64 the eager context switch has already written it, so the block
        // is current unconditionally and this is just the copy.
        let bytes = task.context.fpu().to_bytes();
        hal::Fpu::from_bytes(&bytes)
    }

    /// Make `id`'s saved floating-point block current before somebody reads it.
    ///
    /// A block is stale in exactly one situation: its task is running and still
    /// owns the registers, because every switch out of a task writes them back.
    /// The tracer's usual subject is a *stopped* task, so this is normally a
    /// no-op; it is here so that a caller asking about the running task gets its
    /// real state rather than whatever was last saved.
    ///
    /// The lazy-FPU capability only: it exists to make the trap's live registers
    /// current before `PTRACE_GETFPREGS` reads them. A machine whose switch
    /// restores the floating-point half eagerly keeps the saved block current
    /// already, so `trace_fpu` reads it directly and never calls this.
    #[cfg(mpos_fpu_trap)]
    pub(crate) fn sync_fpu(&mut self, id: TaskId) {
        let live = self
            .tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.fpu_live);
        if live {
            let task = self.task_mut(id);
            // Safe: `fpu_live` implies `CR0.TS` clear.
            unsafe { hal::save_live_fpu(task.context.fpu_mut()) };
        }
    }

    /// Give the running task the floating-point registers back, loading its
    /// image if the switch did not.
    ///
    /// Called from the `#NM` handler, which is the only thing that can be
    /// running when the registers are in question: `CR0.TS` is set by the
    /// switch and cleared here, so a task that has already reclaimed its
    /// registers (a kernel-mode use, or a second fault for any other reason)
    /// finds `fpu_live` set and does not load the image over its own live state.
    ///
    /// x86_64 only: the whole `#NM`-trap reclaim scheme is the lazy-FPU design.
    /// aarch64's `mpos_context_switch` restores the incoming task's FP eagerly,
    /// so there is no fault to reclaim and no live-vs-saved split to reconcile.
    #[cfg(mpos_fpu_trap)]
    pub(crate) fn reclaim_fpu(&mut self) {
        // Safe: the caller is the `#NM` handler, so the bit is set - clearing
        // it is what lets the faulting instruction be re-executed.
        unsafe { hal::claim_live_fpu() };
        let id = self.current;
        let task = self.task_mut(id);
        if !task.fpu_live {
            // Safe: the image is this task's own, 16-byte aligned because
            // `FpuState` is the first member of `Context`, and the registers
            // were just released to it.
            unsafe { hal::restore_fpu(task.context.fpu()) };
            task.fpu_live = true;
        }
    }

    /// Give the running task a floating-point image a signal frame carried.
    ///
    /// The mirror of [`reclaim_fpu`](Self::reclaim_fpu), and it runs inside
    /// `rt_sigreturn`: the handler has returned, and what the interrupted code
    /// must resume with is the image the frame kept from before the handler ran.
    ///
    /// Which copy is authoritative depends on the same `fpu_live` bit the rest of
    /// the scheme turns on. If the handler used a floating-point instruction the
    /// registers are the handler's own leftovers, so they have to be replaced
    /// now - writing only the record would leave those leftovers live and the
    /// interrupted code would resume with them, which is the very bug the frame
    /// exists to prevent. If it did not, the registers belong to no task and the
    /// record is what the next `#NM` loads, so writing it is enough.
    ///
    /// x86_64's lazy scheme only: aarch64's `SignalFrame::exit` writes the
    /// image into the trap frame, whose own exit loads it, and this function
    /// does not exist there.
    #[cfg(mpos_fpu_trap)]
    pub(crate) fn set_signal_fpu(&mut self, bytes: &[u8; hal::FPU_BYTES]) {
        let id = self.current;
        let task = self.task_mut(id);
        *task.context.fpu_mut() = hal::Fpu::from_bytes(bytes);
        if task.fpu_live {
            // Safe: `fpu_live` implies `CR0.TS` is clear - the only two things
            // that set the bit are `hand_over_fpu` and the switch it feeds, and
            // both clear the flag of every task they aim the bit at.
            debug_assert!(!hal::fpu_trapped());
            unsafe { hal::restore_fpu(task.context.fpu()) };
        }
    }

    /// Aim the CPU at the task that is about to run: its address space under
    /// CR3, its kernel stack under `RSP0` for traps from ring 3, and its TLS
    /// base under FS.
    pub(crate) fn prepare(&mut self, id: TaskId) {
        // Take the outgoing task's *live* thread pointer before the incoming
        // task's overwrites the one register the whole CPU has.
        //
        // A program can move its own base with no syscall for the kernel to see:
        // aarch64's musl installs its TLS with a direct `msr tpidr_el0` at
        // startup, so the record this kernel wrote (`arch_prctl`-style, or the
        // cleared base a fresh image gets) stops being the truth the moment the
        // program runs. Without this read-back the record's stale value is what
        // the task gets handed on its next dispatch - measured: the aarch64 test
        // sweep's runner had its base reset to 0 by the first context switch
        // after it spawned a child, and its next TLS access faulted at
        // `0xffffffffffffff5c` (NULL minus musl's `errno`/canary slot).
        //
        // x86_64 is unaffected either way: its base is an MSR only the kernel
        // writes, so `thread_pointer` answers the recorded value and this is the
        // same value written back. Kernel tasks are skipped - they have no TLS,
        // and recording a user task's base into one would be a lie the
        // read-back checks could later quote.
        let outgoing = self.current;
        if outgoing != SCHED_ID
            && let Some(task) = self.tasks.get_mut(outgoing.0).and_then(Option::as_mut)
            && (task.space.is_some() || task.mm.root() != 0)
        {
            task.tls = hal::thread_pointer();
        }

        let task = self.task(id);
        let root = match &task.space {
            Some(space) => space.root(),
            // A thread owns no address space; it runs on the group's, whose
            // paging root the group remembers. A kernel task has neither, and
            // runs on the kernel's own tables.
            None if task.mm.root() != 0 => task.mm.root(),
            None => hal::kernel_root(),
        };
        hal::switch_to_root(root);
        if let Some(stack) = &task.stack {
            hal::set_kernel_stack(stack.top());
        }
        // FS.base is one register for the whole CPU, so every task that wants
        // its own thread-local storage has to reinstall it here. A process with
        // none gets a cleared base, which is the state a fresh image starts in.
        hal::set_thread_pointer(task.tls);
    }

    /// Put the running task at the back of its own band. This is a yield, and it
    /// is also what the clock does to a task that has used up its slice.
    pub(crate) fn requeue_current(&mut self) {
        let id = self.current;
        let priority = self.task(id).priority;
        self.sched.push_ready(id, priority);
    }

    /// Let `prepare` decide where the running task goes, then hand the CPU to
    /// the scheduler. One call, so that nothing - not even a tick - can happen
    /// between queueing the task and switching away from it.
    pub(crate) fn hand_over(
        &mut self,
        prepare: impl FnOnce(&mut Table),
    ) -> (*mut Context, *const Context) {
        prepare(self);
        assert_ne!(
            self.current, SCHED_ID,
            "the scheduler runs its own loop and never yields"
        );
        self.enter(SCHED_ID)
    }

    /// Fire every interval timer whose deadline has passed.
    ///
    /// Firing is signal-add plus wake - the exact shape `send_signal` uses:
    /// the bit lands in the task's pending set, and a task parked in a
    /// `block_until` is pulled out of the sleep queue so it can act on it.
    /// A periodic timer reloads from where it was due, coalescing any
    /// periods a late check skipped; a one-shot disarms.
    ///
    /// This runs on every scheduler turn and on every clock tick, so the scan
    /// of all 64 slots is skipped entirely while `ITIMER_ARMED` says no task has
    /// an `ITIMER_REAL` - which is the state of every machine that never calls
    /// `setitimer`, and so of almost every program in the image.
    pub(crate) fn check_itimers(&mut self) {
        if !ITIMER_ARMED.load(Ordering::Relaxed) {
            return;
        }
        let now = hal::ticks();
        // Recomputed rather than trusted as "set": the one thing that can
        // disarm a timer without going through `itimer_swap` is a task exiting
        // while one is armed, and a flag that stayed set forever would cost the
        // full scan on every turn for the rest of the boot. One extra scan
        // after the last timer goes away is the price of never being stale in
        // the dangerous direction - saying "none armed" while one is.
        let mut any_armed = false;
        for (i, slot) in self.tasks.iter_mut().enumerate() {
            let Some(task) = slot.as_mut() else {
                continue;
            };
            if task.exited {
                continue;
            }
            let Some(state) = task.itimers[ITIMER_REAL].as_ref() else {
                continue;
            };
            any_armed = true;
            if now < state.deadline {
                continue;
            }
            let interval = state.interval_ticks;
            queue_signal(
                task,
                signal::SIGALRM,
                signal::SigInfo::timer(signal::SIGALRM),
            );
            self.sched.wake(TaskId(i));
            match task.itimers[ITIMER_REAL].as_mut() {
                Some(st) if interval > 0 => {
                    let mut next = st.deadline + interval;
                    while next <= now {
                        next += interval;
                    }
                    st.deadline = next;
                }
                _ => task.itimers[ITIMER_REAL] = None,
            }
        }
        ITIMER_ARMED.store(any_armed, Ordering::Relaxed);
    }

    /// Fire every POSIX interval timer whose deadline has passed.
    ///
    /// The itimer scan above and this one are two scans over the same slots
    /// for the same reason: a `timer_create` timer can aim at a *thread*, so
    /// its expiry may queue a signal on a task other than the one that owns
    /// it, and writing that cross-task queueing inline in one pass would hold
    /// two `&mut` tasks at once. So the pass runs in three steps: read the
    /// table to see what each target is (its group, its pending set), walk
    /// the timers and decide, then deliver - one `&mut` borrow at a time.
    /// The armed flags are separate from the itimer's, so a machine with one
    /// kind of timer and not the other never scans for the other.
    pub(crate) fn check_posix_timers(&mut self) {
        if !POSIX_TIMERS_ARMED.load(Ordering::Relaxed) {
            return;
        }
        let now = hal::ticks();
        // Step 1: what the scan needs about every task, without holding any
        // of them mutably. A signal queued in this pass is also tracked in
        // `fresh` below, so a second timer aiming at the same thread in the
        // same pass still counts an overrun rather than double-queueing.
        let mut targets: alloc::vec::Vec<(TaskId, TaskId, bool, signal::Pending)> =
            alloc::vec::Vec::with_capacity(MAX_TASKS);
        for (i, slot) in self.tasks.iter().enumerate() {
            if let Some(task) = slot {
                targets.push((TaskId(i), task.tgid, task.exited, task.signals));
            }
        }
        // Step 2: walk the timers and decide what each expiry does.
        let mut deliver: alloc::vec::Vec<(TaskId, u8, signal::SigInfo)> = alloc::vec::Vec::new();
        let mut fresh: alloc::vec::Vec<(TaskId, u8)> = alloc::vec::Vec::new();
        let mut any_armed = false;
        for (i, slot) in self.tasks.iter_mut().enumerate() {
            let Some(task) = slot.as_mut() else {
                continue;
            };
            if task.exited {
                continue;
            }
            let owner = TaskId(i);
            let owner_tgid = task.tgid;
            for timer in task.timers.iter_mut().flatten() {
                if !timer.expired(now) {
                    if timer.deadline.is_some() {
                        any_armed = true;
                    }
                    continue;
                }
                // Reload before anything else, so an expiry is never lost to
                // a refusal below: a periodic timer coalesces the periods a
                // late check skipped exactly the way an itimer does, and a
                // one-shot disarms.
                match timer.deadline {
                    Some(d) if timer.interval_ticks > 0 => {
                        let mut next = d + timer.interval_ticks;
                        while next <= now {
                            next += timer.interval_ticks;
                        }
                        timer.deadline = Some(next);
                        any_armed = true;
                    }
                    _ => timer.deadline = None,
                }
                // Who the expiry targets: nowhere, the creating task, or the
                // named thread - resolved here, because a `SIGEV_THREAD_ID`
                // thread can have left the group since the timer was made. A
                // timer whose thread is gone stays armed and silent, which is
                // what Linux's does; only the delivery is skipped.
                let target = match timer.notify {
                    TimerNotify::None => continue,
                    TimerNotify::Signal => Some(owner),
                    TimerNotify::Thread(t) => targets
                        .iter()
                        .find(|(id, _, _, _)| *id == t)
                        .filter(|(_, tgid, exited, _)| !*exited && *tgid == owner_tgid)
                        .map(|(id, _, _, _)| *id),
                };
                let Some(target_id) = target else {
                    continue;
                };
                // A standard signal is not queued, so an expiry that finds
                // its own last signal unread has nowhere to go but the
                // overrun count - Linux's own semantics for this class.
                let pending_there = targets
                    .iter()
                    .find(|(id, _, _, _)| *id == target_id)
                    .is_some_and(|(_, _, _, signals)| *signals & signal::bit(timer.signo) != 0)
                    || fresh.contains(&(target_id, timer.signo));
                if pending_there {
                    timer.overrun += 1;
                } else {
                    timer.overrun = 0;
                    let mut info = signal::SigInfo::timer(timer.signo);
                    info.value = timer.value;
                    fresh.push((target_id, timer.signo));
                    deliver.push((target_id, timer.signo, info));
                }
            }
        }
        // Step 3: deliver, one target at a time, and wake it. The record goes
        // through `queue_signal`, so the pending bit and its `siginfo_t` are
        // set together the way every other raise site sets them.
        for (target, signo, info) in deliver {
            if let Some(task) = self.tasks.get_mut(target.0).and_then(Option::as_mut) {
                if !task.exited {
                    queue_signal(task, signo, info);
                    self.sched.wake(target);
                }
            }
        }
        POSIX_TIMERS_ARMED.store(any_armed, Ordering::Relaxed);
    }

    /// The scheduler's whole turn: fire timers, bury the dead, move the
    /// clock on, choose. `None` means there is nothing to run and the caller
    /// should idle.
    pub(crate) fn next_turn(&mut self) -> Option<(*mut Context, *const Context)> {
        self.check_itimers();
        self.check_posix_timers();
        self.reap();
        // The clock's finest reading, because a deadline is now a nanosecond
        // one (§10.7). A tick-only reading would make every sub-tick wait wake
        // at the tick above its deadline and waste the one-shot that fired here.
        self.sched.advance(hal::since_boot_nanos());
        self.arm_next_wake();
        // The picker hands back a queued id, and a queue can hold one that is
        // no longer a task: a task puts itself back when it yields or sleeps
        // without asking anything about itself first, and a slot the reaper
        // freed - or a task a group exit marked - cannot be run. Dropping the
        // id here is the answer, and this is the one place that can give it:
        // the queues are the scheduler's, and the table is the only thing that
        // knows whether the task behind an id is still one. `Scheduler::retire`
        // says the same thing from the other side: "a task left queued after
        // its slot went away is a pid the picker would dereference into
        // nothing", and `retire` is called for the siblings of a group exit
        // only - every other route to that state lands here.
        while let Some(id) = self.sched.pick() {
            match self.tasks[id.0].as_ref() {
                Some(task) if !task.exited => {
                    LAST_RAN_SLOT.store(id.0, core::sync::atomic::Ordering::Relaxed);
                    return Some(self.enter(id));
                }
                _ => continue,
            }
        }
        None
    }

    /// Arm the machine's one-shot for the earliest wait the next tick would be
    /// too late for, or cancel it when there is none.
    ///
    /// This runs on every turn, and recomputing the *earliest* deadline each time
    /// is what keeps the notes' coalescing trap shut: an arm that merely pushed a
    /// new instant could postpone a deadline somebody else had already armed, and
    /// the postponement would only show up under load. Asking the heap for its
    /// minimum and programming exactly that is idempotent by construction.
    ///
    /// Nothing here is done on a machine that cannot wake between ticks: it has
    /// no one-shot to arm, and `SUB_TICK_WAKE` is its own statement of that.
    fn arm_next_wake(&mut self) {
        if !hal::SUB_TICK_WAKE {
            return;
        }
        let now = hal::since_boot_nanos();
        let per_tick = hal::ns_per_tick();
        // The instant the periodic clock next advances; a deadline at or after it
        // is the PIT's to serve.
        let next_tick = now
            .checked_div(per_tick)
            .and_then(|t| (t + 1).checked_mul(per_tick));
        let arm = match (self.sched.next_deadline(), next_tick) {
            (Some(deadline), Some(boundary)) if deadline < boundary => Some(deadline),
            // A `None` here cancels an arm whose wait was woken by an event and
            // is no longer the earliest - without it a stale one-shot would fire
            // into a turn that has nothing to wake.
            _ => None,
        };
        hal::arm_wake_ns(arm);
    }

    /// Slot 1 is left free on purpose - it is init's pid - and the top slot is
    /// the scheduler's. Everything between them is first come, first served.
    pub(crate) fn alloc_slot(&mut self) -> usize {
        let slot = (FIRST_ALLOC_SLOT..MAX_TASKS - 1)
            .find(|&slot| self.tasks[slot].is_none())
            .unwrap_or_else(|| panic!("all {MAX_TASKS} task slots are in use"));
        // The new tenant's identity. Bumping here rather than on release means
        // a freed slot keeps the dead tenant's number until somebody takes it,
        // so a pidfd that outlives the task reads "gone" either way - the slot
        // is empty, or its number is not the one the descriptor recorded.
        self.generations[slot] = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        slot
    }

    /// Allocate a slot, a stack and a bootstrap context for a kernel task. The
    /// task is not queued: the scheduler is created before there is a policy to
    /// queue it in, and must never be queued at all.
    pub(crate) fn create(
        &mut self,
        body: impl FnOnce() + 'static,
        priority: Priority,
        interruptible: bool,
    ) -> TaskId {
        let slot = self.alloc_slot();
        self.create_at(slot, body, priority, interruptible)
    }

    /// [`create`](Self::create) in a slot the caller names, for the one task
    /// that cannot take whatever is free: the scheduler, whose slot is
    /// reserved so that it never collides with a pid.
    pub(crate) fn create_at(
        &mut self,
        slot: usize,
        body: impl FnOnce() + 'static,
        priority: Priority,
        interruptible: bool,
    ) -> TaskId {
        assert!(self.tasks[slot].is_none(), "slot {slot} is already taken");
        drop_retired(TaskId(slot));

        let stack = Stack::new();
        // Safety: the stack was just allocated and nothing else can reach it, so
        // the space below its top is ours to write the bootstrap state into.
        let context = unsafe { hal::bootstrap(stack.top(), task_entry, slot, interruptible) };

        self.tasks[slot] = Some(Task {
            context,
            // A kernel task has no System V adjustments to record or apply.
            sem_undo: None,
            fpu_live: false,
            priority,
            stack: Some(stack),
            exited: false,
            body: Some(Box::new(body)),
            space: None,
            user: false,
            mm: Mm::new(TaskId(slot), 0, alloc::vec::Vec::new()),
            released: false,
            tgid: TaskId(slot),
            user_entry: None,
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            fs: crate::sched::FsStruct::new(crate::abi::DEFAULT_UMASK),
            fs_ns: None,
            parent: None,
            // Nothing waits for a kernel task, so the kind it was "born to
            // report" is unobservable; `SIGCHLD` is the shape a `fork` child
            // has, which is what these tasks are descended from.
            exit_signal: signal::SIGCHLD,
            pgid: TaskId(0),
            session: TaskId(0),
            ctty: None,
            identity: Identity::default(),
            // A kernel task starts in the machine's own namespace, the one
            // every task is born in until something unshares.
            // The machine's one UTS namespace (`crates/uts`'s `init` keeps the
            // first). The id is minted here because the pool is the kernel's;
            // only the first call's is worn, and a number nothing wears is the
            // price of a leaf that cannot mint its own.
            uts: crate::uts::init(crate::vfs::ns::mint()),
            // The machine's own PID namespace: the one a boot begins in, whose
            // numbers are the task slots themselves. A task created here is
            // registered nowhere - the init namespace's `task_of` is the
            // identity - so this is the field pair every task starts with and
            // nothing observable changes until something unshares.
            pid_ns: super::root_ns(),
            pid_ns_children: super::root_ns(),
            // The machine's own time namespace, in both slots: a kernel task is
            // in it and creates its children in it, which is the `pid_ns` pair
            // above replayed for the clocks. It is frozen from the moment it
            // exists - `init` is in it - which is why the offsets file refuses
            // every write on a machine that never unshared.
            time_ns: crate::vfs::ns::machine_time_ns(),
            time_ns_children: crate::vfs::ns::machine_time_ns(),
            // The machine's own user namespace: the identity map, so every id
            // reads back as itself and no reader changes until something
            // unshares.
            user_ns: crate::vfs::ns::machine_user_ns(),
            cgroup_ns: None,
            // The machine's own network namespace, which is `None`: every socket
            // call resolves to the stack the boot built.
            net_ns: None,
            // And the machine's own IPC namespace, `None` for the same reason:
            // every SysV and POSIX queue resolves to the tables the boot made.
            ipc_ns: None,
            // No parent-death signal: nothing has asked for one.
            pdeathsig: 0,
            // And not a subreaper: init is the machine's only one at boot.
            subreaper: false,
            // No privilege given up yet, and none asked for.
            no_new_privs: false,
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::mm::elf::MMAP_BASE,
            stack_top: USER_STACK_TOP,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            // A kernel task or the boot task: no image, so nothing was handed
            // to it and `/proc/<pid>/environ` reads empty - the honest answer
            // for a thread that was never exec'd.
            env: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });
        TaskId(slot)
    }

    /// Free every task that has exited and been collected.
    ///
    /// Runs on the scheduler's stack, which is the only place it is safe to
    /// do: the task that exited is not running any more, but its stack is
    /// where it was.
    ///
    /// A task whose exit code is still waiting to be taken keeps its slot - it
    /// is the zombie a `wait` has not reached yet. Freeing it early would hand
    /// the pid to a new task, and the parent asking for that pid would be told
    /// about the wrong death. `discard_exit_code` is how a code nobody will
    /// ever want is cleared, which lets the slot go.
    /// Give back everything an exited process owned, at its exit rather than at
    /// its reap.
    ///
    /// A process stops needing its memory and its descriptors the moment it
    /// stops running. Releasing them only once its parent has collected its exit
    /// code would leave a zombie holding every pipe it ever wrote to open, and
    /// whoever is reading the other end would wait for an end-of-file that
    /// cannot arrive until the parent gets round to waiting - which, for the
    /// second stage of a pipeline, is exactly the order that deadlocks.
    ///
    /// What stays is the slot: a zombie still has a pid, a parent, and a code
    /// for that parent to read. Only the stack goes, and only in [`Self::reap`].
    pub(crate) fn release_exited(&mut self) {
        // Which tasks are waiting to be released, and which address space each
        // one's memory lives in. Both are collected before any of them is
        // touched: releasing a task borrows the slot it sits in, and asking
        // *then* which space its group runs on would borrow the table twice.
        let due: alloc::vec::Vec<(usize, Option<TaskId>)> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.as_ref().is_some_and(|t| t.exited && !t.released))
            .map(|(slot, _)| {
                let id = crate::sched::policy::TaskId(slot);
                (slot, Some(self.space_owner(id)))
            })
            .collect();

        // The words the leaving threads named, written after the loop rather
        // than inside it: the write reaches into the leader's task, and the
        // loop already holds a mutable borrow of the table.
        let mut clear_tids: alloc::vec::Vec<(Option<TaskId>, usize)> = alloc::vec::Vec::new();

        for (slot, group) in due {
            let Some(task) = self.tasks[slot].as_mut() else {
                continue;
            };
            task.released = true;

            // A thread that leaves does not take the group's memory with it:
            // its siblings are still running on the same frames. Only the last
            // one out does what a process exit has always done here.
            let last = task.mm.leave();
            if last {
                // The address space first: if its CR3 is the one loaded,
                // dropping it switches back to the kernel's own before any page
                // table is freed. The drop frees tables only, so the frames the
                // mappings pointed at come back separately below.
                drop(task.space.take());
                for frame in task.mm.take_frames() {
                    // A frame shared by a copy-on-write fork is released, not
                    // freed: the process that still maps it owns it now.
                    if mm::free_frame_ref(frame) {
                        mm::free_frames(frame, 0);
                    }
                }
            } else if let Some(address) = task.mm.clear_tid() {
                // A thread says where it left, and wakes whoever waits there:
                // `pthread_join` sleeps on a futex at exactly this address.
                // The word is written first and the sleepers woken after,
                // because a joiner that wakes early reads the word, finds the
                // thread still there and goes back to sleep.
                clear_tids.push((group, address));
            }

            // The descriptors are handed over rather than dropped: dropping a
            // pipe end wakes whoever was blocked on it, and this runs with the
            // task table borrowed. The scheduler drops them once it is out.
            //
            // Only a table this task owns outright is its to close. Every
            // kernel task starts on the one shared table, and emptying that
            // would close the console out from under all the others - the
            // table is shared, so the descriptors do not belong to the task
            // that happened to exit first.
            if Arc::strong_count(&task.fds) == 1 {
                let files = task.fds.take_all();
                if !files.is_empty() {
                    // Every per-open-file resource the process was holding is
                    // released here, by the subsystems that own those resources
                    // rather than by this one: each registers a hook at boot and
                    // the list is walked in registration order, *before* the
                    // batch is parked for dropping, while the counts still say
                    // who owns what. Record locks come first, because a lock is
                    // keyed by `(device, inode)` and the inode is reached
                    // *through* a descriptor; then the socket, inotify, evdev and
                    // uinput state keyed by the open file description. What this
                    // function used to do instead was name `vfs::locks`,
                    // `net::socket::close_sockets_at_exit`, `vfs::inotify`,
                    // `drivers::virtio::input` and `drivers::uinput` one after
                    // another, which is what made the scheduler unbuildable
                    // without the VFS, the network and the drivers
                    // (`NOTES.md` §11.1.3, V-3, and `sched::exit` for the rule
                    // that the order is the registration order).
                    crate::sched::exit::run(&files, task.tgid.0 as u32);
                    queue_release(files);
                }
            }
        }

        for (owner, address) in clear_tids {
            if let Some(owner) = owner {
                self.write_clear_tid(owner, address);
            }
            crate::sched::futex::wake_all(address);
        }
    }

    /// Zero the word an exiting thread named in `CLONE_CHILD_CLEARTID`.
    ///
    /// The word lives in the leader's address space and this runs on the
    /// scheduler, translating through whoever happens to be current - so the
    /// space is loaded for the write, and a page the group still shares with a
    /// fork parent is copied first. That copy is not a nicety:
    /// `user_accessible` deliberately counts a copy-on-write page as writable,
    /// because for ring 3 the fault handler answers the write, but this store
    /// is made in ring 0, where a fault is a panic rather than a copy.
    pub(crate) fn write_clear_tid(&mut self, owner: TaskId, address: usize) {
        let Some(task) = self.tasks[owner.0].as_mut() else {
            return;
        };
        let mm = task.mm.clone();
        let Some(root) = task.space.as_ref().map(|space| space.root()) else {
            return;
        };
        if let Some(space) = self.tasks[owner.0].as_mut().and_then(|t| t.space.as_mut()) {
            split_cow(space, &mm, address);
        }
        // The word can sit in a page the group has never touched - a thread's
        // TLS is mapped on demand by its libc. The store below is made in ring
        // 0, where a fault is a panic rather than a copy, so the page is
        // brought in here, where the table is already borrowed and asking for
        // it again would be a second `&mut Table`.
        if hal::phys_for_virt_of(root, address).is_none() {
            populate_in(self, root, address, true);
        }
        crate::hal::write_user_u32(root, address, 0);
    }

    /// Which task owns the address space a group runs on. A process owns its
    /// own; a thread does not own one at all, so the leader's is the group's.
    pub(crate) fn space_owner(&self, id: TaskId) -> TaskId {
        self.task(id).mm.leader()
    }

    pub(crate) fn reap(&mut self) {
        self.release_exited();
        // How many tasks name each slot as the owner of the address space they
        // run on. A thread has no address space of its own: every question it
        // asks about its own memory is answered by looking its group **leader**
        // up (`space_owner`, above) and using the space the leader's slot owns.
        // A leader that is *named* is therefore not freeable, however dead it
        // is - its memory is already gone, and what remains is the answer to
        // the lookups its threads are still making. Linux keeps a thread group
        // leader until the group's last thread is gone for the same reason, and
        // the count is taken before the loop because the loop holds the table
        // mutably.
        //
        // Measured on both sides with `t-leader-outlived`. Without this count:
        // a compositor's main thread ended, the shell beside it collected the
        // pid (its prompt came back), the reaper freed the slot - and with it
        // the address space the compositor's worker threads were executing on -
        // and the next worker to ask for that space was answered with a panic
        // naming a pid the shell had already buried: `there is no task 10`.
        // Its siblings took `user #PF` on the pages the freed space had owned.
        let mut named = [0u32; MAX_TASKS];
        for task in self.tasks.iter().flatten() {
            named[task.mm.leader().0] += 1;
        }
        let mut folds: alloc::vec::Vec<(TaskId, (u64, u64, u64, u64))> = alloc::vec::Vec::new();
        for (slot, entry) in self.tasks.iter_mut().enumerate() {
            let Some(task) = entry else { continue };
            if !task.exited {
                continue;
            }
            assert_ne!(
                TaskId(slot),
                self.current,
                "reaping the task that is running"
            );
            if exit_code_pending(TaskId(slot)) {
                continue;
            }
            // More than its own reference: a thread of this task's group still
            // runs on the space this slot owns, so the slot stays a zombie
            // until that thread is gone and the count above falls to one.
            if named[slot] > 1 {
                continue;
            }

            // A waited child's CPU folds into its parent's child totals before
            // the slot is freed - Linux accumulates on `wait`. Captured here and
            // applied after the loop, because `self.tasks[parent]` cannot be
            // borrowed while `iter_mut` holds the table. Kernel/boot tasks have
            // no parent, so nothing is folded for them.
            //
            // A **thread** is not a waited child, and its `mm` counters are the
            // group's (`cpu_all`), so folding one would charge the whole
            // group's work to the parent once per thread that happened to be
            // collected. Only the leader's slot is a process to hand up.
            if task.tgid.0 == slot
                && let Some(parent) = task.parent
            {
                folds.push((parent, task.mm.cpu_all()));
            }

            let stack = task
                .stack
                .take()
                .expect("only the boot task has no stack of ours");
            let canary = unsafe { (stack.base as *const u64).read_volatile() };
            assert_eq!(
                canary,
                STACK_CANARY,
                "task {slot} ran off the bottom of its {} KiB stack",
                STACK_SIZE / 1024
            );
            mm::free_pages(stack.base, stack.order);
            // Its numbers are forgotten where they were handed out, at every
            // level of its namespace chain: a number that outlived its task
            // would name whatever task next took the slot.
            super::release_pid(task, slot);
            *entry = None;
        }
        // The table borrow is released, so the parent's `super::mm::Mm` is reachable now.
        // Saturating, so a child cannot wrap the parent's counter; a parent whose
        // slot is already gone has nobody left to report the child to (the
        // reparent-to-init edge, beyond what the waited case needs).
        for (parent, child) in folds {
            if let Some(parent_task) = self.tasks.get(parent.0).and_then(|slot| slot.as_ref()) {
                parent_task.mm.absorb_child(child);
            }
        }
    }
}
pub fn current() -> TaskId {
    with_table(|t| t.current)
}

/// Whether the task has already run its exit path - a task this kernel will
/// never schedule again ("a task with `exited` set is never scheduled again").
///
/// A device-job requester in that state cannot wait for its answer: parking
/// would park a task the scheduler has stopped running, and the job's
/// completion wake would land on a slot that has stopped being this task. The
/// exit path's device teardown (the cursor hide, the buffer unrefs) posts its
/// commands fire-and-forget for exactly this reason - the commands none of
/// them read their results back.
pub fn is_exited(id: TaskId) -> bool {
    with_table(|t| t.task(id).exited)
}

/// Whether the scheduler has taken the machine over.
///
/// The boot code is not a task yet, and there is no queue to put it back on:
/// a caller that would sleep has to spin instead, which is what the console's
/// output path asks before it parks on the transmit ring.
pub fn started() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// The kernel stack the running task is on, as `(base, top)` - the low and
/// high addresses of the block the frame allocator handed out for it.
///
/// The panic handler asks for this and nothing else does. A stack that ran
/// into its own bottom is the one failure whose damage lands on whatever the
/// frame allocator put in the next block over, which reads as corruption
/// anywhere but the stack; knowing how much of it was in use at the moment of
/// the fault is what tells that apart from a wild write. `None` for the boot
/// task, whose stack the bootloader owns and the kernel never measured.
pub fn current_stack_range() -> Option<(usize, usize)> {
    with_table(|t| {
        let task = t.tasks.get(t.current.0)?.as_ref()?;
        let stack = task.stack.as_ref()?;
        Some((stack.base, stack.top()))
    })
}

/// The descriptor table of the running task: what its fd numbers index.
///
/// Before [`init`] there is no running task to ask - the boot code is not one
/// yet - so the kernel's shared table answers, the same way [`spawn`] refuses
/// to guess before it could know.
pub fn current_fds() -> Arc<FdTable> {
    if !STARTED.load(Ordering::Relaxed) {
        return crate::vfs::file::kernel_fds();
    }
    let id = current();
    with_table(|t| t.task(id).fds.clone())
}

/// The path the running process was started from - what a `readlink` of
/// `/proc/self/exe` hands back. Empty when the process has no named image.
pub fn current_exe() -> alloc::string::String {
    let id = current();
    with_table(|t| t.task(id).exe.clone())
}

/// Which of a task's children a wait is about.
///
/// Linux's two wait doors name their subject in three ways - `wait4`'s pid
/// argument and `waitid`'s `idtype` are two spellings of the same three
/// selectors - and this is the one vocabulary both are translated into, so the
/// two doors cannot disagree about *which* child an answer describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitFor {
    /// Every child: `wait4`'s `-1` and `waitid`'s `P_ALL`.
    Any,
    /// One child by id: `wait4`'s positive pid and `waitid`'s `P_PID`.
    Pid(TaskId),
    /// Every child in one process group: `waitid`'s `P_PGID`, whose `id` is a
    /// group rather than a task. `wait4`'s `0` and negative pids name the same
    /// selector - the caller's own group - which that door does not ask yet.
    Pgid(TaskId),
}

impl WaitFor {
    /// Whether the task in slot `id` is one this selector names.
    pub(crate) fn names_task(self, id: TaskId, task: &Task) -> bool {
        match self {
            WaitFor::Any => true,
            WaitFor::Pid(want) => id == want,
            WaitFor::Pgid(group) => task.pgid == group,
        }
    }

    /// The same question about a *record*: its own task is gone (the slot was
    /// freed, or handed to a new tenant), so a group answer is read out of the
    /// record itself - which is why [`Retired`] carries a `pgid` and the
    /// signal the task was born to report.
    ///
    /// The kind is asked here rather than at the callers because it is the same
    /// question: Linux's `wait_consider_task` narrows by `exit_signal` in the
    /// one place that decides whether a child counts at all, so a selector and a
    /// kind that could disagree would be two spellings of one filter.
    pub(crate) fn names_retired(self, retired: &Retired, kind: ChildKind) -> bool {
        kind.sees(retired.exit_signal)
            && match self {
                WaitFor::Any => true,
                WaitFor::Pid(want) => retired.id == want,
                WaitFor::Pgid(group) => retired.pgid == group,
            }
    }
}

/// Which of a task's children a wait is allowed to be *answered* about, which is
/// a second question from which one it names.
///
/// `clone`'s low byte is not a flag but a signal - what the child's death should
/// send its parent - and Linux reads that byte back at the other end: a child
/// born to report `SIGCHLD` is a "non-clone" child, one born to report nothing
/// or something else is a "clone" child, and `wait`'s option word picks which of
/// the two it can hear (`wait(2)`: `__WCLONE` is clone children only, `__WALL`
/// is both, neither is non-clone children only). Measured on a current host
/// (`.workbuddy/probe_wclone.c`, Linux 7.2.9) - and the byte is what decides,
/// not the flag word alone: `clone(SIGTERM)` is a clone child too
/// (`.workbuddy/probe_wclone.c` C1).
///
/// The narrowing is not a hint. A wait that cannot see a child is answered
/// `ECHILD` — the same word as having no child at all, blocking or not
/// (measured: B1/B2/B3/G1/G2) — so a kernel that stores the byte and never asks
/// it hands a caller a child Linux would say it does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    /// No `__WCLONE`, no `__WALL`: only the children born to report `SIGCHLD`.
    NonClone,
    /// `__WCLONE` alone: only the children born to report nothing or something
    /// other than `SIGCHLD`.
    Clone,
    /// `__WALL`: both kinds. `__WALL` wins when both bits are set, measured.
    Either,
}

impl ChildKind {
    /// Whether a child born to report `exit_signal` is one a wait with these
    /// options can be answered about.
    pub(crate) fn sees(self, exit_signal: u8) -> bool {
        match self {
            ChildKind::NonClone => exit_signal == signal::SIGCHLD,
            ChildKind::Clone => exit_signal != signal::SIGCHLD,
            ChildKind::Either => true,
        }
    }
}

/// What happened to one child, as `wait(2)` reports it.
///
/// Every variant carries the child's reported CPU as `utime`/`stime` in internal
/// clock ticks (own plus its own reaped children - `Retired::utime`'s pair),
/// because `wait4` and `waitid` both fill their `struct rusage` argument on
/// *every* event they report and not only on a death: Linux copies it from the
/// same `getrusage(p, RUSAGE_BOTH, ...)` call on the stop and the resume paths
/// (measured, `.workbuddy/probe_wait4_rusage.c` scenarios C, H and I, Linux
/// 7.2.9 - a stop, a `waitid(WSTOPPED)` and a resume each fill it). A death
/// carries it off the record, where the group is gone; a stop or a resume reads
/// the still-live group.
#[derive(Clone)]
pub enum WaitEvent {
    /// A death to collect.
    ///
    /// The uid rides the event because the record does not reach the caller: a
    /// collection takes the record out of the table, and `waitid` owes the
    /// child's real uid beside the code it writes.
    ///
    /// `dumped` is the same carry for the same reason: the core-dump
    /// classification (`CLD_DUMPED`/`WCOREDUMP`) is a fact about the *dead*
    /// child's `dumpable` flag, which is gone once the task is, so the record
    /// holds it and this hands it on.
    Exited {
        id: TaskId,
        code: isize,
        /// The dead process's numbers per namespace level, off the record -
        /// see [`Retired::numbers`]. The slot is gone by the time a caller
        /// reads this event, so the record is the only carrier.
        numbers: alloc::vec::Vec<(u64, u32)>,
        uid: u32,
        dumped: bool,
        /// The death's CPU, off the record - see the enum's own note.
        utime: u64,
        stime: u64,
    },
    /// A stop to report: the signal `WSTOPSIG` names, and the `PTRACE_EVENT_*`
    /// the status word's high byte carries.
    ///
    /// `traced` says whether a *tracer* took this stop rather than the parent's
    /// `WUNTRACED`. The `wait4` status word does not need it - a traced stop is
    /// already a `SIGTRAP` there - but `waitid` reports the two as different
    /// `CLD_*` codes (`CLD_TRAPPED` and `CLD_STOPPED`), and the event word
    /// cannot tell them apart: a ptrace signal-delivery stop carries
    /// `PTRACE_EVENT_NONE`, exactly as a group stop does.
    Stopped {
        id: TaskId,
        signal: u8,
        event: u32,
        traced: bool,
        /// The stopped child's CPU so far - see the enum's own note.
        utime: u64,
        stime: u64,
    },
    /// A `SIGCONT` ended a stop. Only ever reported to a caller that asked
    /// with `WCONTINUED`.
    Continued {
        id: TaskId,
        /// The resumed child's CPU so far - see the enum's own note.
        utime: u64,
        stime: u64,
    },
}

pub fn yield_now() {
    schedule(Table::requeue_current);
}

/// Sleep for at least `ticks` clock ticks, or until a fatal signal ends the
/// task.
///
/// The signal check is the whole of why this is not simply "park until the
/// deadline". Linux ends a sleeping task when a fatal signal arrives, and a
/// sleep that ignores them is a `sleep 100` that survives `SIGTERM` - which
/// is what `timeout` reports as a kill that never happened, with the sleeper
/// still on the machine afterwards.
pub fn sleep_ticks(ticks: u64) {
    // A tick-denominated wait is still a wait to an *instant*, so the unit is
    // converted here - at the one boundary that knows what a tick is - rather
    // than rounding every deadline below up to the next tick.
    let nanos = ticks.saturating_mul(hal::ns_per_tick());
    sleep_until(hal::since_boot_nanos().saturating_add(nanos));
}

/// Sleep for at least `nanos` nanoseconds, or until a fatal signal ends the
/// task.
///
/// The finer sibling of [`sleep_ticks`], and what `nanosleep(2)` asks: Linux's
/// sleep does not round a duration up to a timer period, and a 1 ms sleep on a
/// 250 Hz clock that rounded would take four. On a machine with a sub-tick
/// wake-up (`hal::SUB_TICK_WAKE`) the scheduler arms a one-shot at the deadline;
/// on one without, the same call still ends on the next tick - honestly late,
/// rather than silently redefined.
pub fn sleep_ns(nanos: u64) {
    sleep_until(hal::since_boot_nanos().saturating_add(nanos));
}

/// The sleep both entry points share: wait until the absolute deadline,
/// re-reading the clock on every pass so a wake that was not the deadline does
/// not restart the wait.
fn sleep_until(deadline: u64) {
    let clock = || hal::since_boot_nanos();
    loop {
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if clock() >= deadline {
            return;
        }
        schedule(|t| {
            // Read the clock here rather than trusting the scheduler's last
            // turn. This task may have been running for a while since then,
            // and a deadline computed from a stale clock wakes early.
            t.sched.advance(clock());
            if clock() >= deadline {
                t.requeue_current();
                return;
            }
            let id = t.current;
            let priority = t.task(id).priority;
            // What is *left* of the wait, so a wake that was not the deadline
            // does not start the whole sleep over.
            let remaining = deadline.saturating_sub(clock()).max(1);
            t.sched.sleep(id, priority, remaining);
        });
    }
}

/// Block the running task until `cond` returns true.
///
/// The last check of `cond` and the sleep that follows it happen in one
/// critical section, so a waker - which makes its side of the condition true
/// and calls [`wake`] from its own critical section - can never slip between
/// the two. That is what closes the lost-wakeup window: either the waker sees
/// the condition already true and does nothing, or it finds this task in the
/// sleep queue and wakes it. There is no third state to fall into.
///
/// Block until `id` has an exit code to collect.
///
/// A named wait rather than a `block_until` closure because the condition is
/// the retired-code list, and the second look below happens while `schedule`
/// holds the task table: a closure calling `exit_code` would take its own
/// critical section inside that one, and releasing the inner one would turn
/// interrupts back on before the outer section is done.
pub fn block_until_code(id: TaskId) {
    note_park_current(5);
    loop {
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if exit_code(id).is_some() {
            return;
        }
        schedule(|t| {
            if exit_code(id).is_some() {
                t.requeue_current();
            } else {
                let me = t.current;
                let priority = t.task(me).priority;
                t.sched.sleep(me, priority, u64::MAX);
            }
        });
    }
}

/// The condition runs with interrupts off and must be short. It reads shared
/// state through its own borrow, which is the ordinary case.
pub fn block_until(cond: impl Fn() -> bool) {
    block_until_on(|_| cond())
}

/// [`block_until`] for a condition that reads the task table.
///
/// The condition is handed the table instead of reaching for it, because the
/// second look below happens while `schedule` already holds it: a condition
/// that took its own critical section there would nest one inside another, and
/// the inner release would hand interrupts back before the outer section is
/// done. Taking the borrow as a parameter makes the shared case the easy one.
fn block_until_on(cond: impl Fn(&Table) -> bool) {
    note_park_current(1);
    loop {
        // Signals are checked before the condition on every pass, so a fatal
        // signal kills a blocked task even if what it waited for also arrived.
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if with_table(|t| cond(t)) {
            return;
        }
        schedule(|t| {
            if cond(t) {
                t.requeue_current();
            } else {
                // u64::MAX means "until someone wakes me": the deadline
                // saturates, and only an explicit wake can end the sleep.
                let id = t.current;
                let priority = t.task(id).priority;
                t.sched.sleep(id, priority, u64::MAX);
            }
        });
    }
}

/// [`block_until_on`] for a condition a *catchable* signal may end. On the same
/// table borrow it also checks `table_has_deliverable`, so a trapped signal
/// wakes the caller with [`WaitEnd::Interrupted`] instead of being swallowed and
/// re-slept - which is exactly what makes `wait4`/`waitid` interruptible the way
/// the pipe/futex/nanosleep waits already are (`NOTES.md` §2.5; measured on a real
/// Linux: `wait4` interrupted by a caught non-`SA_RESTART` signal returns `EINTR`,
/// and `SA_RESTART` restarts it). The deliverable signal is left pending, not
/// consumed, so the syscall gate can build the handler's frame on the way out.
pub(crate) fn block_until_on_interruptible(cond: impl Fn(&Table) -> bool) -> WaitEnd {
    loop {
        // As in `block_until_on`: a fatal signal is taken before the condition,
        // so a task that is killing itself does not first collect a child event.
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if with_table(|t| cond(t)) {
            return WaitEnd::Condition;
        }
        if with_table(|t| table_has_deliverable(t)) {
            return WaitEnd::Interrupted;
        }
        schedule(|t| {
            if cond(t) || table_has_deliverable(t) {
                t.requeue_current();
            } else {
                let id = t.current;
                let priority = t.task(id).priority;
                t.sched.sleep(id, priority, u64::MAX);
            }
        });
    }
}

/// How a blocking wait's park ended. A child event was collected (`Event`),
/// there is no child to wait for (`NoChild`, which the caller turns into
/// `ECHILD`), or a catchable signal ended the wait (`Interrupted`, which the
/// caller turns into `EINTR` and the syscall gate restarts under `SA_RESTART`).
/// Keeping `Interrupted` distinct from `NoChild` is the whole reason this is not
/// an `Option`.
pub enum WaitPark<T> {
    Event(T),
    NoChild,
    Interrupted,
}

/// [`block_until`] with a clock on the wait: block until `cond` is true, the
/// `deadline` - a tick deadline, as the sleep queue keeps them - passes, or a
/// catchable signal demands the wait's attention. A wake still ends the sleep
/// early, exactly as it does for [`block_until`]; the clock and the signal
/// only add ways out. `u64::MAX` is the [`block_until`] spelling of "never",
/// which is how a caller with no timeout spells this call.
///
/// The three answers are the caller's to interpret: the condition holding
/// means woken, the clock means timed out, and a pending signal means the
/// wait has to go answer EINTR - the syscall gate delivers the signal on the
/// way out, and a libc's own handler is often the thing meant to end the
/// wait, which is why sleeping through it would hang the caller. The signal
/// is left pending, not consumed: taking it here would strand it where no
/// register file exists to build the handler's frame on.
pub enum WaitEnd {
    /// The condition held.
    Condition,
    /// The clock ran out with the condition still false.
    Deadline,
    /// A catchable, unblocked, non-ignored signal is pending.
    Interrupted,
}

/// Block until `cond` is true, the `deadline` passes, or a signal arrives.
///
/// The deadline is **nanoseconds since boot** — the clock `clock_gettime`
/// answers and the only one a wait can share with its own expiry check. There
/// used to be a tick-denominated spelling of this door whose input was
/// multiplied into nanoseconds on the way in, and that multiply is the bug this
/// shape exists to keep out: the tick *counter* and the nanosecond clock are two
/// clocks that any masked window longer than the wait can put a tick apart, and
/// a park that answers "deadline" against one while the caller checks expiry
/// against the other spins forever with interrupts masked — which is exactly
/// the aarch64 sweep freeze, measured 2026-10-24 (`docs/notes/` §6.23). One
/// door, one unit, and a caller cannot mix them.
///
/// `u64::MAX` stays "no clock at all" through every conversion.
pub fn block_until_nanos(cond: impl Fn() -> bool, deadline: u64) -> WaitEnd {
    note_park_current(2);
    loop {
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if cond() {
            return WaitEnd::Condition;
        }
        if with_table(|t| table_has_deliverable(t)) {
            return WaitEnd::Interrupted;
        }
        if hal::since_boot_nanos() >= deadline {
            return WaitEnd::Deadline;
        }
        schedule(|t| {
            let done = cond() || table_has_deliverable(t) || hal::since_boot_nanos() >= deadline;
            if done {
                t.requeue_current();
            } else {
                let id = t.current;
                let priority = t.task(id).priority;
                // The deadline is absolute, the sleep's is relative: what is
                // left of the wait is what the clock should run down, and a
                // wake may still end it before that.
                t.sched.advance(hal::since_boot_nanos());
                let remaining = deadline.saturating_sub(hal::since_boot_nanos()).max(1);
                t.sched.sleep(id, priority, remaining);
            }
        });
    }
}

/// [`block_until_nanos`] with no clock: the wait a syscall can be ended by
/// a signal, which is Linux's `ERESTARTSYS` shape.
///
/// The named spelling exists because that is what a syscall means by "block
/// until this is ready": the condition, or a catchable signal - and never a
/// clock. Writing it as `block_until_nanos(cond, u64::MAX)` at every one of
/// those call sites says the same thing in more words and hides which waits
/// are interruptible from the reader looking for them. The signal is *not*
/// consumed here: the syscall gate delivers it on the way out, which is the
/// only place a handler's frame can be built.
pub fn block_until_interruptible(cond: impl Fn() -> bool) -> WaitEnd {
    block_until_nanos(cond, u64::MAX)
}

/// [`block_until_interruptible`] for a caller that has something to give back
/// before the task dies.
///
/// The one difference between the two is what a **fatal** signal does. Every
/// wait above takes a fatal signal as `exit`, in its loop's first line, so a
/// blocked task dies *inside* its own park - which is right for a park that took
/// nothing, because a woken task with nothing to undo has no use for the frame
/// it would have woken into. A park that *took* something cannot die there: the
/// give-back is the code after the park, and a death inside the park skips it.
/// The FIFO's open-time rendezvous is that caller - it takes a read or write
/// count so the peer it is waiting for can see it, and gives the count back on
/// every way out except the one where the task is ended mid-wait — the shape
/// that was named in `docs/uapi/` §5.2 and is served now (`docs/uapi/` §5.1 item
/// 28; `.workbuddy/probe_fifo_park.c` is the measurement).
///
/// So a fatal signal **ends this wait** and is left pending, exactly as a
/// catchable one is, and the death happens where Linux's does: the syscall gate
/// finds the signal still pending on its way out and takes its default action
/// (`syscall::linux::proc::deliver_signal`), which for every fatal signal is
/// `exit(-sig)` - the same death, of the same number, one syscall frame later
/// than before. Linux's own order, one `goto` away: `fifo_open`'s
/// `wait_event_interruptible` returns `-ERESTARTSYS` for `SIGKILL` too, the
/// `err_rd:` label gives the count back, and the task dies after the syscall has
/// unwound rather than in the middle of it.
///
/// Nothing about the call being interrupted changes for the caller: it still
/// reads [`WaitEnd::Interrupted`], and it is still the gate that acts on the
/// signal - including for a *traced* task, which now gets the signal-delivery
/// stop Linux gives it rather than dying without one.
pub fn block_until_interruptible_rolling_back(cond: impl Fn() -> bool) -> WaitEnd {
    note_park_current(3);
    loop {
        // The condition first, and not the fatal check: Linux's own precedence.
        // `wait_for_partner` leaves its loop when the count it watches changed
        // even with a fatal signal pending, so the open *succeeds* and the task
        // dies afterwards with its descriptor installed - and that path needs no
        // rollback at all, because the last close gives the count back.
        if cond() {
            return WaitEnd::Condition;
        }
        if with_table(|t| table_has_deliverable(t)) {
            return WaitEnd::Interrupted;
        }
        schedule(|t| {
            let done = cond() || table_has_deliverable(t);
            if done {
                t.requeue_current();
            } else {
                let id = t.current;
                let priority = t.task(id).priority;
                t.sched.sleep(id, priority, u64::MAX);
            }
        });
    }
}

/// Block until `cond` is true — with **no way out but the condition**.
///
/// This is Linux's `TASK_UNINTERRUPTIBLE`, the state `ps` prints as `D`, and
/// it exists here for the same callers Linux gives it to: a task with an
/// operation **in flight on a device**. Every other wait in this file answers
/// a signal somehow — a fatal one by dying inside the park — and for a caller
/// that took nothing that is right. A transfer is not that caller: its request
/// and response records live on its own stack, and the device is reading or
/// writing through descriptors that name that stack. A fatal exit mid-wait
/// would free the frame under the DMA, leave the mailbox's waiter id naming a
/// corpse, and strand the in-flight request where no one will ever retire it —
/// so the wait ignores signals entirely and they stay pending until the
/// transfer is done, at which point the syscall gate finds them and acts, the
/// same one-frame-later death every other wait takes. Linux's own order:
/// `wait_on_buffer` sleeps uninterruptibly, and `SIGKILL` waits for the I/O.
///
/// The condition is re-checked inside `schedule`'s closure — the second look
/// every wait here takes — so a waker that runs between the first check and
/// the sleep cannot lose its wake: the waker's `wake` needs the task table the
/// sleeper holds for that second look, so it finds the task either already
/// asleep or already requeued on a true condition.
pub fn block_until_uninterruptible(cond: impl Fn() -> bool) {
    block_until_uninterruptible_at(4, cond)
}

/// [`block_until_uninterruptible`] with its [`PARK_SITE`] id named by the
/// caller.
///
/// Two waits of this shape that belong to different subsystems answer one
/// question differently - *which* hand-off is the stuck task in - and
/// `/proc/<pid>/wchan` is only as useful as the vocabulary it prints. The
/// block mailbox and the ext4 cache claims are exactly that pair: both park
/// here, and only the caller knows which (`docs/notes/` §7 R.9).
pub fn block_until_uninterruptible_at(site: u8, cond: impl Fn() -> bool) {
    block_until_uninterruptible_armed(site, cond, || {})
}

/// [`block_until_uninterruptible_at`] whose wake-up registration is **per
/// sleep**: `arm` runs before every park, so a waiter that is woken and sent back
/// to sleep (its condition false again - a new claim took what it waited for) is
/// registered for the next wake as well.
///
/// The one-shot shape is right for a caller that does its own arming around each
/// park; it is wrong inside a loop like this one, and the desktop boot is the
/// measurement: a waiter on the ext4 cache's scratch was woken by every claim that
/// ended - `claim-hits` in the heartbeat counted each one - and slept again after
/// each, registered only for the first. When the next claim ended there was
/// nobody on the list, and the boot stopped at `Starting udev ...` with the
/// waiter parked and both scratch flags false (`docs/notes/` §7 R.9).
pub fn block_until_uninterruptible_armed(site: u8, cond: impl Fn() -> bool, arm: impl Fn()) {
    note_park_current(site);
    loop {
        if cond() {
            return;
        }
        // The registration and the check after it: a wake that lands between them
        // finds a running task (a no-op) and the second look finds the condition
        // true, so the pair cannot lose it.
        arm();
        if cond() {
            return;
        }
        schedule(|t| {
            if cond() {
                t.requeue_current();
            } else {
                let id = t.current;
                let priority = t.task(id).priority;
                t.sched.sleep(id, priority, u64::MAX);
            }
        });
    }
}

pub fn pause() -> ! {
    block_until(|| false);
    unreachable!("a task blocked on `false` is only ever woken to die")
}

/// Make a sleeping task runnable again, ahead of its deadline.
///
/// Returns whether it was asleep. Every other answer is a no-op: a running or
/// ready task will look at its condition again on its own, and an exited task
/// is gone. The caller must have made the task's wait condition true *before*
/// calling this, inside the same critical section if the condition is shared
/// state - otherwise the task may wake, see a false condition, and sleep again.
pub fn wake(id: TaskId) -> bool {
    let hit = with_table(|t| t.sched.wake(id));
    WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
    if hit {
        WAKE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        WAKE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
    hit
}

/// How often [`wake`] was called, and how often the task it named was actually
/// asleep. A wake that misses is a no-op by contract (a running or ready task
/// re-checks its condition on its own) - so a stall with `misses` growing and a
/// task parked anyway is the one reading that separates "the wake went to the
/// wrong place" from "the woken task slept again" (`docs/notes/` §7 R.9).
pub static WAKE_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static WAKE_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static WAKE_MISSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Fire any interval timer whose deadline has passed, waking its sleeper.
///
/// Three call sites because the CPU can be in three places when a deadline
/// comes due: the clock interrupting a task (preempt), the scheduler idling
/// (next_turn), and the syscall gate. Missing one would leave a caller that
/// only ever does one of the three waiting forever.
pub fn check_itimer() {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    heartbeat();
    with_table(|t| t.check_itimers());
}

/// Install `new` as the running task's timer `which`, reporting what was
/// there - the value-plus-interval pair `setitimer`'s old_value wants.
pub fn itimer_swap(which: usize, new: Option<ItimerState>) -> Option<ItimerSnapshot> {
    // Before the table lock: this is the one place a timer appears, so it is
    // where the scan's fast path has to be told to look again. Disarming does
    // not clear it - another task may still have one armed, and the scan is
    // what finds out.
    if which == ITIMER_REAL && new.is_some() {
        ITIMER_ARMED.store(true, Ordering::Relaxed);
    }
    with_table(|t| {
        let task = t.task_mut(t.current);
        let old = task.itimers[which];
        task.itimers[which] = new;
        old.map(|state| ItimerSnapshot {
            interval_ticks: state.interval_ticks,
            remaining_ticks: state.deadline.saturating_sub(hal::ticks()),
        })
    })
}

/// Read the running task's timer `which` without disturbing it.
pub fn itimer_snapshot(which: usize) -> Option<ItimerSnapshot> {
    with_table(|t| {
        let state = t.task(t.current).itimers[which].as_ref()?;
        Some(ItimerSnapshot {
            interval_ticks: state.interval_ticks,
            remaining_ticks: state.deadline.saturating_sub(hal::ticks()),
        })
    })
}

/// Make the running task a POSIX timer, answering the id `timer_t` names it
/// by, or the errno the refusal carries.
///
/// `target` is the thread a `SIGEV_THREAD_ID` expiry aims at; the table is
/// what can check it belongs to the caller's thread group, which is Linux's
/// own validation - a timer aimed at another process's thread is a way to
/// signal a process the caller may have no right to signal.
///
/// A timer is born disarmed, exactly as Linux's is: `timer_create` only makes
/// the record, and `timer_settime` is what arms it. The new timer's slot is
/// the lowest free one - ids are the caller's to hold, not a sequence to
/// read meaning into.
pub fn timer_alloc(signo: u8, value: u64, notify: TimerNotify) -> Result<usize, isize> {
    with_table(|t| {
        if let TimerNotify::Thread(target) = notify {
            let ok = task_ok_for_timer(t, target).is_some_and(|grp| grp == t.task(t.current).tgid);
            if !ok {
                return Err(crate::err::EINVAL);
            }
        }
        let task = t.task_mut(t.current);
        let slot = (0..TIMER_SLOTS).find(|&s| task.timers[s].is_none());
        let Some(slot) = slot else {
            return Err(crate::err::EAGAIN);
        };
        task.timers[slot] = Some(PosixTimer {
            interval_ticks: 0,
            deadline: None,
            signo,
            value,
            notify,
            overrun: 0,
        });
        Ok(slot)
    })
}

/// The thread group a task belongs to, for the `SIGEV_THREAD_ID` check, or
/// `None` when no such task is alive.
fn task_ok_for_timer(t: &Table, target: TaskId) -> Option<TaskId> {
    t.tasks
        .get(target.0)
        .and_then(Option::as_ref)
        .filter(|task| !task.exited)
        .map(|task| task.tgid)
}

/// Arm or disarm the running task's timer `slot`, reporting the state it
/// replaced as `(interval, remaining)` ticks - the pair `timer_settime`'s
/// `old_value` wants. `None` is an id that names no timer of this task's.
///
/// `deadline` is `None` for a disarm and `Some(ticks)` for an arm; the caller
/// has already converted the timespec and refused the absolute spelling this
/// kernel cannot honour.
pub fn timer_arm(slot: usize, interval_ticks: u64, deadline: Option<u64>) -> Option<(u64, u64)> {
    if deadline.is_some() {
        POSIX_TIMERS_ARMED.store(true, Ordering::Relaxed);
    }
    with_table(|t| {
        let task = t.task_mut(t.current);
        let timer = task.timers.get_mut(slot)?.as_mut()?;
        let old = (
            timer.interval_ticks,
            timer.deadline.map_or(0, |d| d.saturating_sub(hal::ticks())),
        );
        timer.interval_ticks = interval_ticks;
        timer.deadline = deadline;
        Some(old)
    })
}

/// Read a timer's `(interval, remaining)` in ticks, for `timer_gettime`.
/// `None` is an id that names no timer of this task's; a disarmed timer
/// answers zero remaining, exactly as Linux's does.
pub fn timer_state(slot: usize) -> Option<(u64, u64)> {
    with_table(|t| {
        let timer = t.task(t.current).timers.get(slot)?.as_ref()?;
        Some((
            timer.interval_ticks,
            timer.deadline.map_or(0, |d| d.saturating_sub(hal::ticks())),
        ))
    })
}

/// The overrun count `timer_getoverrun` reports, or `None` for an id that
/// names no timer of this task's.
pub fn timer_overrun(slot: usize) -> Option<u32> {
    with_table(|t| Some(t.task(t.current).timers.get(slot)?.as_ref()?.overrun))
}

/// Free a timer's slot. The id dies with it: a later use of the same number
/// is `EINVAL`, whether or not the slot has been handed out again, because a
/// timer that was deleted is not a timer whatever occupies its slot now.
pub fn timer_remove(slot: usize) -> bool {
    with_table(|t| {
        let task = t.task_mut(t.current);
        match task.timers.get_mut(slot) {
            Some(slot @ Some(_)) => slot.take().is_some(),
            _ => false,
        }
    })
}

/// Called from the clock interrupt: the running task has used its slice.
///
/// Does nothing before [`init`], and nothing at all when the scheduler itself is
/// running. The scheduler is resumed by tasks handing over to it, never by being
/// picked, so preempting it would leave it suspended halfway through a switch it
/// had already committed to.
pub fn preempt() {
    if !STARTED.load(Ordering::Relaxed) || current() == SCHED_ID {
        return;
    }
    // An interval timer that came due while the task spun in ring 3 fires
    // here, on the clock that is guaranteed to run - which also wakes a task
    // parked in a blocking wait past its deadline.
    check_itimer();
    // A task that never blocks would otherwise never see a signal: the clock
    // is the one thing guaranteed to run while it spins. Delivering here is
    // legal for the same reason being preempted is - the trap frame is on the
    // task's own stack, and `exit` leaves it the way `requeue_current` does.
    deliver_pending_signal();
    schedule(Table::requeue_current);
}

/// End the running task if a fatal signal is waiting, at the one point a task
/// can be interrupted from without a register file to build on: the clock.
///
/// A signal that arrives while the task is spinning in ring 3 is acted on by
/// the next tick, so no program can outlast a `kill` by never making a
/// syscall. Called from interrupt context, where it must not block.
///
/// Signals that cannot be finished here are left alone rather than dropped:
/// one with a handler needs the task's register file to build a frame on, and
/// the syscall return path is the only place that has one - so it goes back
/// into the pending set, where the next syscall the task makes will find it.
/// A signal the task has blocked is not even looked at: that is what blocking
/// means.
pub fn deliver_pending_signal() {
    if let Some(sig) = take_fatal_signal() {
        exit(-(sig as isize));
    }
}

/// Turn interrupts on and wait for the next one.
///
/// The only sanctioned way to stand still. A task resumed by [`schedule`] has
/// interrupts off - the switch saves the flags after its own `cli` - so a
/// busy loop parks this core deaf to its own clock, its own keyboard and every
/// other line. This is `sti; hlt`: the `sti` lets the interrupt through, the
/// `hlt` stops the core until it arrives, and `idle` takes interrupts back off
/// before returning so the caller is in the state `with_table` expects.
///
/// For a task with nothing left to do, and for the scheduler when the ready
/// queue is empty.
pub fn wait_for_interrupt() {
    hal::idle();
}

/// The `#NM` (device-not-available) trap: a task used a floating-point
/// instruction before its registers had been loaded back, which is the deferred
/// half of the switch.
///
/// Interrupts arrive here already off, and this takes the table lock the same
/// way any other handler does - which is safe because a fault of this kind can
/// only be raised by ring-3 code: the kernel is built `soft-float`, so nothing
/// between the switch setting `CR0.TS` and this handler was ever holding the
/// lock with a vector instruction in flight.
///
/// The `#NM` vector is x86_64's; the aarch64 IDT has no such entry, so this
/// handler (and the lazy `reclaim_fpu` it drives) exists only on x86_64.
#[cfg(mpos_fpu_trap)]
pub fn device_not_available() {
    with_table(|t| t.reclaim_fpu());
}

/// Put a signal frame's floating-point image back on the running task.
///
/// The free-function door to [`Table::set_signal_fpu`], because `rt_sigreturn`
/// is the caller and it lives above the table: the frame's machine half hands the
/// image back (`hal::SignalFrameExit::fpu`) and this is what makes it the task's
/// again.
///
/// x86_64's lazy scheme only - see the method.
#[cfg(mpos_fpu_trap)]
pub fn set_signal_fpu(bytes: &[u8; hal::FPU_BYTES]) {
    with_table(|t| t.set_signal_fpu(bytes));
}

/// The one path off a task's stack.
pub(crate) fn schedule(prepare: impl FnOnce(&mut Table)) {
    critical_section::with(|cs| {
        // Deliberately not `with_table`: the borrow has to be dead before the
        // switch below, and a closure would hold it across.
        let (from, to) = unsafe { (*TABLE.borrow(cs).get()).hand_over(prepare) };
        unsafe { hal::switch_context(from, to) };
    });
}

/// Run `f` on the task table with interrupts off.
///
/// `f` must not switch tasks. See the module docs for why that is a rule rather
/// than a preference.
pub(crate) fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    critical_section::with(|cs| unsafe { f(&mut *TABLE.borrow(cs).get()) })
}

/// When a task last ran and which one: the answer to "the machine is alive but
/// nothing is progressing", readable from the wire.
static LAST_PROGRESS_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LAST_RAN_SLOT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
static LAST_BEAT_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How often the wire gets a heartbeat, and how stale "progress" may get before
/// the line says so.
///
/// The heartbeat exists because a hung boot cannot run `ps`: it says, every five
/// seconds, how long ago the last task switch was and which slot made it. A
/// large age is the stall itself; an age near zero while nothing visible
/// happens names the task that is spinning (`docs/notes/` §7 R.9 - the udev
/// stall's last reading had no idle gap at all, which is exactly the case a
/// single "is anything parked" dump cannot distinguish from progress).
const HEARTBEAT_NS: u64 = 5_000_000_000;

/// One table every [`HEARTBEAT_NS`] from the tick, in interrupt context: the
/// console's own write never parks, which is why this is safe from here where a
/// dump built in the idle loop was not (it printed nothing at all - the reading
/// is in the note).
///
/// The whole slot table and not one line, because the two halves of a hang have
/// to be read together: which slots are *parked* (and where) says what waits,
/// and which slots are switched to says what spins - a single last-ran line
/// named the spinner and could not say that udev's own processes were asleep
/// (`docs/notes/` §7 R.9).
fn heartbeat() {
    use core::sync::atomic::Ordering;
    let now = hal::since_boot_nanos();
    let last = LAST_BEAT_NS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < HEARTBEAT_NS {
        return;
    }
    LAST_BEAT_NS.store(now, Ordering::Relaxed);
    let progress = LAST_PROGRESS_NS.load(Ordering::Relaxed);
    let age = now.saturating_sub(progress) / 1_000_000;
    let running = LAST_RAN_SLOT.load(Ordering::Relaxed);
    let fill = fill_owner()
        .map(|slot| {
            let pid = super::pid::pid_number_of(TaskId(slot)).unwrap_or(0);
            alloc::format!("slot {slot}/pid {pid}")
        })
        .unwrap_or_else(|| "none".into());
    let run = run_owner()
        .map(|slot| {
            let pid = super::pid::pid_number_of(TaskId(slot)).unwrap_or(0);
            alloc::format!("slot {slot}/pid {pid}")
        })
        .unwrap_or_else(|| "none".into());
    crate::hal::kernel::print_line(format_args!(
        "mpos: heartbeat age={age}ms fill-owner={fill} run-owner={run} fill-busy={} run-busy={} -- live slots:\n",
        EXT4_FILL_BUSY.load(Ordering::Relaxed),
        EXT4_RUN_BUSY.load(Ordering::Relaxed)
    ));
    crate::hal::kernel::print_line(format_args!(
        "mpos:   claim registrations={} handed-back={}\n",
        CLAIM_REGISTRATIONS.load(Ordering::Relaxed),
        CLAIM_WOKEN.load(Ordering::Relaxed)
    ));
    crate::hal::kernel::print_line(format_args!(
        "mpos:   wakes calls={} hits={} misses={} claim-hits={} claim-misses={}\n",
        WAKE_CALLS.load(Ordering::Relaxed),
        WAKE_HITS.load(Ordering::Relaxed),
        WAKE_MISSES.load(Ordering::Relaxed),
        CLAIM_WAKE_HITS.load(Ordering::Relaxed),
        CLAIM_WAKE_MISSES.load(Ordering::Relaxed)
    ));
    for slot in 0..MAX_TASKS {
        let live = with_table(|t| {
            t.tasks
                .get(slot)
                .and_then(|task| task.as_ref())
                .map(|task| !task.exited)
                .unwrap_or(false)
        });
        if !live {
            continue;
        }
        let pid = super::pid::pid_number_of(TaskId(slot)).unwrap_or(0);
        let park = park_site(slot);
        let nr = last_syscall(slot);
        crate::hal::kernel::print_line(format_args!(
            "mpos:   slot {slot} pid {pid} park {park} ({}) syscall {nr} parks={}{}\n",
            PARK_SITES.get(park as usize).copied().unwrap_or("?"),
            parks_of(slot),
            if slot == running { "  <-- running" } else { "" }
        ));
    }
}

/// The scheduler task's body. Every scheduling decision is made here, on a stack
/// that belongs to no task and is therefore never freed underneath itself.
pub(crate) fn scheduler_loop() {
    loop {
        let switched = critical_section::with(|cs| {
            // As in `schedule`: one statement, borrow dead before the switch.
            let next = unsafe { (*TABLE.borrow(cs).get()).next_turn() };
            match next {
                Some((from, to)) => {
                    unsafe { hal::switch_context(from, to) };
                    true
                }
                None => false,
            }
        });

        // Outside the borrow above: dropping a description can wake a task, and
        // waking needs the table this loop just let go of.
        drain_release();

        if switched {
            LAST_PROGRESS_NS.store(
                hal::since_boot_nanos(),
                core::sync::atomic::Ordering::Relaxed,
            );
        } else {
            // Nothing to run. Halt with interrupts on so the next tick wakes us,
            // and let it turn them back off before the table is touched again.
            wait_for_interrupt();
        }
    }
}
