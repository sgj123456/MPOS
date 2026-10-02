//! The task table, the stacks tasks run on, and the scheduler task that runs
//! between them.
//!
//! # Two invariants, both about aliasing
//!
//! Tasks switch *inside* the interrupts-off window that guards the table, so a
//! `&mut Table` that outlived a switch would be aliased by the next task's own
//! `&mut Table`. Two `&mut` to one value is undefined behaviour even if only one
//! of them is running at a time, so no borrow here is ever alive across
//! [`context::switch`]. Every access is either inside a [`with_table`] closure,
//! which cannot switch, or the single statement at one of the two switch sites,
//! where the borrow is a temporary that dies at the semicolon.
//!
//! A [`Context`] must never move either, because a suspended task's saved stack
//! pointer lives inside it and the switch is handed a pointer to it. That is why
//! the table is a fixed-size array: growing a `Vec` would relocate every
//! suspended task's context.
//!
//! # Why everything goes through the scheduler task
//!
//! A task that gives up the CPU switches to the scheduler, which then picks the
//! next task, rather than switching to it directly. The extra hop is what makes
//! reaping possible: a task cannot free the stack it is standing on, so the one
//! that exits hands over and the scheduler buries it from a stack of its own.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{RefCell, UnsafeCell};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use critical_section::Mutex;

use crate::arch::context::{self, Context, UserContext};
use crate::arch::gdt;
use crate::arch::paging::{self, AddressSpace};
use crate::arch::{idle, timer};
use crate::mm::elf::{self, Image};
use crate::mm::vm::{MapFlags, Mapping};
use crate::mm::vma::{MappedRange, Vma, VmaKind, VmaTable, merge_ranges};
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{Priority, Scheduler, TaskId};
use crate::sched::signal;
use crate::syscall::linux::{abi, nr};
use crate::vfs::RawFd;
use crate::vfs::file::{FdTable, File};

/// How many tasks can exist at once. A hard limit rather than a grown
/// collection, for the reason given in the module docs.
pub const MAX_TASKS: usize = 64;

/// A process's user and group identity: the real, effective and saved id of
/// each, which is the whole of what `setuid(2)`'s family manipulates, plus
/// the supplementary groups `setgroups(2)` names - what a `doas` rule for
/// `:wheel` checks membership in.
///
/// The real id is who started the process, the effective id is who it is
/// acting as now, and the saved id is who it may go back to - the one
/// `setreuid`/`setresuid` dances depend on. A program like `sudo` lowers its
/// effective id to read a file it does not trust itself with and raises it
/// back through the saved one; without all three words, that dance has no
/// floor to stand on. The kernel still does not enforce file modes, so these
/// are what the syscall family reads and writes - the honest half of the
/// ownership model, with the permission half still to come.
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

/// Task stacks are `2^STACK_ORDER` pages. The kernel stack has to hold the trap
/// frames that stack up above a task the clock preempted *and* whatever the
/// deepest syscall path needs - and `execve` is by far the deepest: it reads a
/// whole program image, hands it to a parser, and builds a new address space
/// underneath all of that.
///
/// The figure depends on the build, because that path's frame sizes do. A
/// release image gets Linux's own 16 KiB (`STACK_ORDER` 2), which is what the
/// 64 ring-3 programs in the image are booted and measured against. A debug
/// image keeps 64 KiB: unoptimised frames are several times larger, and 16 KiB
/// was measured to be too few for `execve` there. The symptom of getting it
/// wrong is the worst kind - the overflow eats the fault handler's own frame,
/// so the machine triple-faults with nothing said - which is why the two
/// figures are the ones that have been booted rather than the ones that look
/// smallest.
#[cfg(debug_assertions)]
pub const STACK_ORDER: usize = 4;
#[cfg(not(debug_assertions))]
pub const STACK_ORDER: usize = 2;
const STACK_SIZE: usize = PAGE_SIZE << STACK_ORDER;

/// Written at the bottom of every stack, which is where a runaway task arrives
/// last, and checked when the task is reaped. It catches overflow after the fact
/// rather than preventing it; preventing it needs a guard page, and the direct
/// map is not something to punch holes in for one task's benefit.
const STACK_CANARY: u64 = 0x6d70_6f73_5f73_746b;

/// Whether any task may have an `ITIMER_REAL` armed.
///
/// The hot-path half of [`Table::check_itimers`]: the scan of all 64 slots runs
/// on every scheduler turn and every clock tick, and all but a handful of
/// programs in the image never arm a timer at all. Set here by the one thing
/// that arms one ([`itimer_swap`]) and cleared by the scan itself when it finds
/// none - so it can be wrong only in the direction of "run the scan", never in
/// the direction of "skip a timer that is due".
///
/// Only `ITIMER_REAL` is tracked, because only `ITIMER_REAL` is a wall-clock
/// deadline the scheduler has to honour; the virtual and profiling timers are
/// charged against consumed CPU time and are checked where that time is counted.
static ITIMER_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The same flag for the `timer_create` family, and for the same reason: the
/// scan in [`Table::check_posix_timers`] is skipped entirely while no task has
/// a POSIX timer armed, which is the state of a machine running anything but
/// the program that asked for one. Set by the arm call, cleared by the scan
/// when it finds none - wrong only in the safe direction, "run the scan".
static POSIX_TIMERS_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Slot 0 belongs to whatever was running when [`init`] was called. It adopts
/// that stack instead of getting one, so there is nothing of ours to free.
const BOOT_SLOT: usize = 0;

/// The next slot after the boot task's. Slots below it stay empty rather than
/// first come, first served, so a pid never depends on how many tasks the
/// boot code happened to start: slot 1 is reserved for init, and the tests
/// that ran before it still get pids they can assert on.
///
/// The top slot belongs to the scheduler, which is never queued and never
/// exits; tasks reach it by switching to it. Why the top rather than 1: the
/// first process the kernel starts is init, and a pid is a slot number, so
/// init has to land on 1 - which is free exactly because the scheduler sits
/// out of the way at the far end. See [`Table::alloc_slot`].
const FIRST_ALLOC_SLOT: usize = 2;
const SCHED_ID: TaskId = TaskId(MAX_TASKS - 1);

/// The first user process the kernel starts: init, and the parent every
/// orphan ends up with.
pub const INIT_PID: TaskId = TaskId(1);

/// Permanent tasks: the boot task and the scheduler. Anything above this count
/// is a task the tests started and have not finished reaping.
const PERMANENT_TASKS: usize = 2;

/// Tasks started as permanent since boot - see [`spawn_permanent`]. Kept apart
/// from the constant because it changes at runtime, and [`permanent_tasks`] is
/// the sum of the two.
static PERMANENT_EXTRA: AtomicUsize = AtomicUsize::new(0);

/// Set once [`init`] has built the table. Before that the clock still ticks but
/// [`preempt`] has nothing to switch to and must leave the running code alone,
/// which is what lets every milestone before this one keep working unchanged.
static STARTED: AtomicBool = AtomicBool::new(false);

static TABLE: Mutex<Shared> = Mutex::new(Shared::new(Table::new()));

/// Where a user stack ends. Programs are linked around 0x400000, so 1 GiB
/// leaves the whole image below it and everything in between for a heap once
/// user memory grows one (M11+). The stack itself grows down from here.
const USER_STACK_TOP: usize = 0x4000_0000;
/// How much of the stack exists before the program runs. This is where a stack
/// *starts*, not how big one is: everything below it is mapped on first touch,
/// up to [`USER_STACK_MAX`].
const USER_STACK_PAGES: usize = 16;
/// How far down the main stack may grow. Linux's default `RLIMIT_STACK`, and
/// the whole reason a program may call deeper than any fixed reservation: the
/// region extends as it is used.
const USER_STACK_MAX: usize = 8 * 1024 * 1024;

/// How far below [`USER_STACK_TOP`] a program's stack may start, in pages.
///
/// This is the one randomisation range that is *not* Linux's number: Linux
/// bounds the stack's movement by a fraction of the machine's memory and of
/// `RLIMIT_STACK`, so the range is a property of the box rather than of the
/// ABI. Measured over 64 runs here it came out at about 16 GiB, which this
/// kernel's layout cannot imitate - the stack top is 1 GiB and the program sits
/// below it. Eight MiB is the largest movement that keeps the whole stack clear
/// of the image no matter how far `RLIMIT_STACK` lets it grow.
const ASLR_STACK_PAGES: usize = 1 << 11;

/// How far below its landmark a randomised load address or `mmap` region may
/// sit, in pages: a tebibyte's worth.
///
/// The number comes from measuring Linux rather than from rounding: over 64
/// runs of the same position-independent binary, its code base, its `mmap`
/// region and its heap anchor each moved across a range of this order, and the
/// same program linked `-no-pie` moved its stack and `mmap` region but kept its
/// code address at `0x401156` every time. Matching that last half is the point:
/// randomising a fixed-address image's placement would break it, because the
/// addresses in its own instructions are the ones it was linked to run at.
const ASLR_REGION_PAGES: usize = 1 << 28;

/// A page-aligned random multiple of [`PAGE_SIZE`] below `count` pages.
///
/// Drawn from the same ChaCha20 stream that fills `AT_RANDOM` and `getrandom`,
/// which means it is seeded from the machine at boot and not reproducible by
/// anything running on it. A process that could predict this could rebuild the
/// layout it was supposed to be guessing at, so the draw goes through the pool
/// rather than through the tick counter - the tick is readable by every process
/// on the machine, and a layout derived from it is a layout with no secrets.
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
struct Layout {
    /// The address the image's own `vaddr`s are offsets from. Zero for an
    /// image that named where it wanted to be.
    bias: usize,
    /// The same, for the interpreter. Zero when the program is static.
    interp_bias: usize,
    /// The highest stack address the program may use, one past the top.
    stack_top: usize,
    /// The address the `mmap` region grows down from.
    mmap_top: usize,
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
fn new_layout(image: &Image<'_>, interp: Option<&Image<'_>>) -> Layout {
    let mmap_top = crate::syscall::MMAP_BASE - random_pages(ASLR_REGION_PAGES);
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
use crate::syscall::linux::abi::{RLIM_NLIMITS, Rlimit, default_rlimits, rlimit_admit};

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
                // RLIMIT_NOFILE also caps the descriptor table: keep the two in
                // step so an fd allocation refuses at the new ceiling.
                if resource == 5 {
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
const STACK_GROWTH_SLACK: usize = 64 * 1024;

// `ARGV_MAX` / `ARGS_BYTES_MAX` live in `policy`: the syscall layer checks
// them on the host too, where this module is not compiled.
use super::policy::{ARGS_BYTES_MAX, ARGV_MAX};

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
const INTERP_BASE: usize = 0x0000_0000_7000_0000;

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
#[derive(Clone, Copy)]
struct Retired {
    id: TaskId,
    code: isize,
    /// The tracer this task died under, if any. A traced task's death is the
    /// tracer's to collect: the tracer reaps the corpse first, and only its
    /// taking releases the record to the real parent - the same
    /// release-on-reap order Linux uses. Without it, a `strace -f timeout ...
    /// fish` run reports the fish exit to `timeout` while strace still owns
    /// the body, and the two wait loops race for one record.
    tracer: Option<TaskId>,
}

/// Exit codes, kept after the task is gone. The slot a task occupied is free
/// for reuse the moment it is reaped, but the code it exited with can still be
/// wanted - a milestone check reads it one tick later, and `wait` reads it
/// once. Reading it takes it: a code belongs to the one caller that collects
/// it, and anything left over is dropped oldest-first when the cap is reached.
static RETIRED: Mutex<RefCell<Vec<Retired>>> = Mutex::new(RefCell::new(Vec::new()));
const RETIRED_CAP: usize = 32;

/// The exits whose `SIGCHLD` is still unread.
///
/// Two books, one per consumer: the exit *code* belongs to `wait(2)`, which
/// takes it exactly once, while the *signal* belongs to a `signalfd`, whose
/// reader may be a different caller - and draining one must not starve the
/// other. `nlplug-findfs` leans on exactly that split: it reads the signal
/// off its descriptor and then reaps the child through `waitpid`, and a
/// signal read that also consumed the code would leave the reaper looking at
/// an empty table.
static SIGCHLD_PENDING: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));

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
fn drain_release() {
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
struct Shared(UnsafeCell<Table>);

unsafe impl Send for Shared {}

impl Shared {
    const fn new(table: Table) -> Shared {
        Shared(UnsafeCell::new(table))
    }

    /// Dereferencing the result is sound only inside a critical section, and the
    /// `&mut` must not outlive a context switch. See the module docs.
    fn get(&self) -> *mut Table {
        self.0.get()
    }
}

/// A task's stack, and where it came from.
struct Stack {
    base: usize,
    order: usize,
}

impl Stack {
    fn new() -> Stack {
        let base = mm::alloc_pages(STACK_ORDER).expect("out of memory for a task stack");
        unsafe { (base as *mut u64).write_volatile(STACK_CANARY) };
        Stack {
            base,
            order: STACK_ORDER,
        }
    }

    fn top(&self) -> usize {
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
pub const SIGEV_NONE: u32 = 0;
pub const SIGEV_SIGNAL: u32 = 1;
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

/// A thread group's memory: the frames behind its mappings, how many threads
/// are still running on it, and where a leaving thread says so.
///
/// A process is one thread group. `fork` makes a new one; `clone(CLONE_VM)`
/// adds a thread to an existing one, and the *last* thread out frees what the
/// group owns - which is why the frames live here and not on the task that
/// happened to allocate them. Linux calls this the `mm`; the page tables
/// themselves stay on the task that owns them, because threading is the only
/// case where two tasks reach the same ones.
pub struct Mm {
    /// How many threads are running on this group. The last one out frees the
    /// frames; a group of one is a process that never made a thread.
    live: core::sync::atomic::AtomicUsize,
    /// The paging root of the group's address space, as CR3 is loaded with it.
    /// A thread has no `AddressSpace` of its own and switches to this one.
    root_value: usize,
    /// Physical frames behind the group's user mappings - program pages,
    /// stack and heap. Dropping the address space frees its tables but never
    /// these, so the reaper returns them by hand.
    frames: Mutex<RefCell<Vec<usize>>>,
    /// The group's virtual memory areas: which ranges exist, with what
    /// permissions, and what backs each one.
    ///
    /// This is Linux's `mm->mmap`, and it is what makes a mapping an *address
    /// space* rather than a set of pre-allocated frames: a range with no page
    /// behind it yet is still mapped, and a fault inside it is answered from
    /// the [`crate::mm::vma`] entry instead of by SIGSEGV. The per-group
    /// placement is deliberate - threads share one address space, so they
    /// share one table, and `fork` copies it.
    vmas: Mutex<RefCell<VmaTable>>,
    /// The ranges mapped *eagerly* - every page filled as it was mapped, so no
    /// address in one ever faults. Here rather than in `vmas` for the reason
    /// [`MappedRange`] gives. Only `/proc/<pid>/maps` reads this.
    eager: Mutex<RefCell<Vec<MappedRange>>>,
    /// The thread that started the process: its id is the group's pid, and
    /// `getpid` answers it from any of the threads.
    leader: TaskId,
    /// Where a leaving thread writes zero and wakes whoever waits there -
    /// `CLONE_CHILD_CLEARTID`, which is how a libc's `pthread_join` learns a
    /// thread is gone without polling.
    clear_tid: Mutex<RefCell<Option<usize>>>,
    /// The `MEMBARRIER_CMD_REGISTER_*` commands this process has declared it
    /// will use, as a bitmask.
    ///
    /// Per *process*, not per thread, and that is not a shortcut: the
    /// registration is a statement about threads sharing one address space, and
    /// the sharing is what makes the barrier mean anything - which is why Linux
    /// keeps it on the `mm` too. A thread's `Mm` *is* the group's, so the `Arc`
    /// gives these semantics for free.
    membarrier: core::sync::atomic::AtomicUsize,
    /// The robust-futex list head this process registered with
    /// `set_robust_list`, kept so `get_robust_list` can answer it. Like the
    /// membarrier registrations it lives on the group, not the task: a libc
    /// registers one list per thread, threads are not implemented, so the group
    /// has the one. Walking the list to release held futexes at thread exit is a
    /// separate, named gap - storing the head is what makes `get` honest.
    robust_head: core::sync::atomic::AtomicUsize,
    /// The `comm` a process set with `PR_SET_NAME`, as a 16-byte NUL-terminated
    /// name; all-zero means none was set, and `comm_of` then falls back to the
    /// image's basename. Lives on the group like the other per-process facts
    /// above; `/proc/<pid>/comm` and `PR_GET_NAME` both report it, which is the
    /// whole reason `PR_SET_NAME` is honest rather than accepted-and-dropped.
    comm_name: [core::sync::atomic::AtomicU8; 16],
    /// The group's credentials: the four capability sets, the dumpable flag
    /// and `KEEPCAPS`. On the group rather than the task because every consumer
    /// here - `capget`/`capset`, the `prctl` options, the `execve` derivation
    /// and `ptrace_may_access` - is about the process, and this kernel's
    /// threads are one process; Linux keeps them per-thread and its libc keeps
    /// the threads in step, so a group-wide copy is the same state without the
    /// bookkeeping. The policy itself is [`crate::cred`], which is pure and
    /// host-tested; this is only where it is stored.
    credentials: Mutex<RefCell<crate::cred::Credentials>>,
    /// CPU time the group has spent with the core executing *its* tasks, in
    /// internal clock ticks (the 250 Hz tick), split user / system by the ring
    /// the timer interrupt took the core out of. On the group, not the task,
    /// because `times()` and `getrusage(RUSAGE_SELF)` are process-wide in Linux
    /// and the `Arc`-shared `Mm` is what a process is here: every thread's ticks
    /// land in the same two counters. Aggregating *reaped children* into
    /// `cutime`/`cstime` (and `RUSAGE_CHILDREN`) is a separate named gap -
    /// `PLAN.md` §12.2 B6.
    cpu_utime: core::sync::atomic::AtomicU64,
    cpu_stime: core::sync::atomic::AtomicU64,
    /// The CPU of this process's *reaped* children, accumulated by
    /// [`Mm::absorb_child`] at the reap: `Linux` folds a waited child's
    /// `utime + cutime` into the parent's `cutime`. Zero until a child is
    /// collected. What `tms_cutime`/`tms_cstime` and `getrusage(RUSAGE_CHILDREN)`
    /// report.
    cpu_cutime: core::sync::atomic::AtomicU64,
    cpu_cstime: core::sync::atomic::AtomicU64,
}

impl Mm {
    /// The paging root of the group's address space.
    fn root(&self) -> usize {
        self.root_value
    }

    fn new(leader: TaskId, root: usize, frames: Vec<usize>) -> Arc<Mm> {
        Mm::with_eager(leader, root, frames, Vec::new())
    }

    /// A group whose address space already has eagerly-mapped ranges in it:
    /// what `load_user` laid out, and what a private file `mmap` copied. `fork`
    /// and `clone` are the other callers - a child's address space is its
    /// parent's, so its report is too.
    fn with_eager(
        leader: TaskId,
        root: usize,
        frames: Vec<usize>,
        eager: Vec<MappedRange>,
    ) -> Arc<Mm> {
        Arc::new(Mm {
            live: core::sync::atomic::AtomicUsize::new(1),
            root_value: root,
            frames: Mutex::new(RefCell::new(frames)),
            vmas: Mutex::new(RefCell::new(VmaTable::new())),
            eager: Mutex::new(RefCell::new(eager)),
            leader,
            clear_tid: Mutex::new(RefCell::new(None)),
            membarrier: core::sync::atomic::AtomicUsize::new(0),
            robust_head: core::sync::atomic::AtomicUsize::new(0),
            comm_name: [const { core::sync::atomic::AtomicU8::new(0) }; 16],
            credentials: Mutex::new(RefCell::new(crate::cred::Credentials::initial())),
            cpu_utime: core::sync::atomic::AtomicU64::new(0),
            cpu_stime: core::sync::atomic::AtomicU64::new(0),
            cpu_cutime: core::sync::atomic::AtomicU64::new(0),
            cpu_cstime: core::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Put a whole VMA table in place: what `fork` hands its child, whose
    /// memory is its parent's - address space included.
    fn set_vmas(&self, vmas: VmaTable) {
        critical_section::with(|cs| *self.vmas.borrow(cs).borrow_mut() = vmas);
    }

    /// A copy of the table, for a child about to be given its own.
    fn vma_snapshot(&self) -> VmaTable {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().clone())
    }

    /// The eagerly-mapped ranges of this address space, folded into the runs a
    /// maps line describes. The folding happens here rather than on the way in
    /// because a mapping can be recorded in pieces (a segment list, a page loop)
    /// and only the reader cares that they are one line.
    fn eager_ranges(&self) -> Vec<MappedRange> {
        merge_ranges(critical_section::with(|cs| {
            self.eager.borrow(cs).borrow().clone()
        }))
    }

    /// Add one eagerly-mapped range, for `/proc/<pid>/maps` to report. Called by
    /// the loader and by a private file `mmap`; see [`MappedRange`] for why
    /// neither put a VMA in the table instead.
    fn record_range(&self, range: MappedRange) {
        critical_section::with(|cs| self.eager.borrow(cs).borrow_mut().push(range));
    }

    /// Stretch the reported `[stack]` range down to `page`.
    ///
    /// A stack is a region a program descends into rather than a fixed
    /// allocation, so the line `/proc/<pid>/maps` prints has to follow it -
    /// otherwise a program reading its own maps from deep in a call sees a
    /// stack that stopped 64 KiB above where it is standing.
    fn grow_stack_range(&self, page: usize) {
        critical_section::with(|cs| {
            for range in self.eager.borrow(cs).borrow_mut().iter_mut() {
                // The loader adds exactly one range with this name, and nothing
                // else in this list carries a bracketed one.
                if range.name.as_deref() == Some("[stack]") && page < range.start {
                    range.start = page;
                }
            }
        });
    }

    /// The VMA covering `virt`, if the group has one.
    fn vma_at(&self, virt: usize) -> Option<Vma> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().lookup(virt).cloned())
    }

    /// Whether a VMA covers `virt`.
    fn vma_covers(&self, virt: usize) -> bool {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().covers(virt))
    }

    fn vma_insert(&self, vma: Vma) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().insert(vma));
    }

    fn vma_remove_range(&self, start: usize, end: usize) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().remove_range(start, end));
    }

    fn vma_take_range(&self, start: usize, end: usize) -> Vec<Vma> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().take_range(start, end))
    }

    fn vma_set_prot(&self, start: usize, end: usize, prot: MapFlags) {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow_mut().set_prot(start, end, prot));
    }

    fn vma_gaps(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        critical_section::with(|cs| self.vmas.borrow(cs).borrow().gaps(start, end))
    }

    /// The `membarrier` registrations this group has made. See
    /// [`Mm::membarrier`].
    fn membarrier_registrations(&self) -> usize {
        self.membarrier.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Record that this process may use `barrier`, leaving any other
    /// registration as it was: a process may register for both kinds, and the
    /// second registration is not a claim that the first never happened.
    fn register_membarrier(&self, barrier: usize) {
        self.membarrier
            .fetch_or(barrier, core::sync::atomic::Ordering::Relaxed);
    }

    /// The robust-futex list head this process registered, `0` for none.
    fn robust_head(&self) -> usize {
        self.robust_head.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Record the robust-futex list head from `set_robust_list`. The last
    /// registration wins, as on Linux: a thread re-registering replaces its
    /// list rather than stacking another.
    fn set_robust_head(&self, head: usize) {
        self.robust_head
            .store(head, core::sync::atomic::Ordering::Relaxed);
    }

    /// The 16-byte name set by `PR_SET_NAME`, all-zero when none was. `comm_of`
    /// falls back to the image basename when the first byte is zero.
    fn comm_name_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        for (slot, out) in self.comm_name.iter().zip(b.iter_mut()) {
            *out = slot.load(core::sync::atomic::Ordering::Relaxed);
        }
        b
    }

    /// Store the `PR_SET_NAME` name: copy the caller's bytes up to the first
    /// NUL or the 16th, zero-filling the tail, so the stored array is always a
    /// NUL-terminated name within `TASK_COMM_LEN`.
    fn set_comm_name(&self, name: &[u8]) {
        for i in 0..16 {
            let byte = match name.get(i).copied() {
                Some(0) | None => 0,
                Some(b) => b,
            };
            self.comm_name[i].store(byte, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The group's credentials, as a snapshot the caller can hold.
    fn credentials(&self) -> crate::cred::Credentials {
        critical_section::with(|cs| *self.credentials.borrow(cs).borrow())
    }

    /// Replace the group's credential state. `capget`/`capset`, the `prctl`
    /// options and the identity rule all funnel through here.
    fn set_credentials(&self, creds: crate::cred::Credentials) {
        critical_section::with(|cs| *self.credentials.borrow(cs).borrow_mut() = creds);
    }

    /// Change one field of the group's credentials in place, so a caller that
    /// only moves a bit still gets the table's lock for the read-modify-write.
    fn with_credentials<R>(&self, f: impl FnOnce(&mut crate::cred::Credentials) -> R) -> R {
        critical_section::with(|cs| f(&mut self.credentials.borrow(cs).borrow_mut()))
    }

    /// Add one clock tick of CPU time to the group, split by the ring the tick
    /// interrupted: the core was in this process's user code, or in the kernel
    /// on its behalf. Two atomic increments - safe from the timer interrupt and
    /// allocation-free, as an interrupt handler must be.
    fn charge_tick(&self, in_user: bool) {
        let counter = if in_user {
            &self.cpu_utime
        } else {
            &self.cpu_stime
        };
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// The group's CPU time in internal clock ticks, as `(user, system,
    /// child-user, child-system)` - the four `times()` reports and the pair
    /// behind each `getrusage` direction.
    fn cpu_all(&self) -> (u64, u64, u64, u64) {
        (
            self.cpu_utime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_stime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_cutime.load(core::sync::atomic::Ordering::Relaxed),
            self.cpu_cstime.load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Fold a reaped child's CPU into this process's child totals - Linux's
    /// accounting on `wait`: the parent's `cutime` grows by the child's own
    /// `utime` plus the child's own `cutime` (its children, already folded in),
    /// the same for system. Saturating, so no exit can wrap the counter.
    fn absorb_child(&self, child: (u64, u64, u64, u64)) {
        let (cu, su, ccu, csu) = child;
        self.cpu_cutime.fetch_add(
            cu.saturating_add(ccu),
            core::sync::atomic::Ordering::Relaxed,
        );
        self.cpu_cstime.fetch_add(
            su.saturating_add(csu),
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Swap one frame for another, in place: the copy-on-write path replaces
    /// the page it copied and keeps the group's books straight.
    fn replace(&self, old: usize, new: usize) -> bool {
        critical_section::with(|cs| {
            let mut frames = self.frames.borrow(cs).borrow_mut();
            match frames.iter_mut().find(|held| **held == old) {
                Some(slot) => {
                    *slot = new;
                    true
                }
                None => false,
            }
        })
    }

    /// A frame this group now owns.
    fn own(&self, frame: usize) {
        critical_section::with(|cs| self.frames.borrow(cs).borrow_mut().push(frame));
    }

    /// Forget a frame without giving it back: whoever took it owns it now.
    fn disown(&self, frame: usize) {
        critical_section::with(|cs| {
            self.frames
                .borrow(cs)
                .borrow_mut()
                .retain(|&held| held != frame)
        });
    }

    /// Take every frame out, for the last thread's exit to release.
    fn take_frames(&self) -> Vec<usize> {
        critical_section::with(|cs| core::mem::take(&mut *self.frames.borrow(cs).borrow_mut()))
    }

    /// A thread joins the group: one more alive, and it will free nothing.
    fn join(&self) {
        self.live
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// A thread leaves. The answer says whether it was the last, which is the
    /// one that frees what the group owns.
    fn leave(&self) -> bool {
        self.live.fetch_sub(1, core::sync::atomic::Ordering::AcqRel) == 1
    }

    fn leader(&self) -> TaskId {
        self.leader
    }

    fn set_clear_tid(&self, address: usize) {
        critical_section::with(|cs| *self.clear_tid.borrow(cs).borrow_mut() = Some(address));
    }

    fn clear_tid(&self) -> Option<usize> {
        critical_section::with(|cs| *self.clear_tid.borrow(cs).borrow())
    }
}

struct Task {
    /// Must stay put for as long as the task exists.
    context: Context,
    /// Whether the hardware floating-point registers currently hold *this*
    /// task's state - the whole of the lazy-FPU bookkeeping, and the reason
    /// `context.fpu` is not written on every switch.
    ///
    /// At most one task may have it set, and only the running one: the switch
    /// saves and clears it on the way out, and the `#NM` handler sets it when
    /// it loads an image the switch set `CR0.TS` for. A task that has never run
    /// a floating-point instruction therefore keeps `false` for its whole life
    /// and never costs either a save or a restore.
    fpu_live: bool,
    priority: Priority,
    /// `None` only for the boot task, whose stack belongs to the bootloader.
    stack: Option<Stack>,
    /// Set when the body has returned. The task is not freed here: it is running
    /// on the stack that needs freeing, so the scheduler reaps it later.
    exited: bool,
    body: Option<Box<dyn FnOnce()>>,
    /// The ring-3 address space; `None` for tasks that only run kernel code.
    space: Option<AddressSpace>,
    /// Set for every task that was given a user image to run, and stays set
    /// after it exits: a zombie is still a process, and `/proc` - which has no
    /// address space of its own to look at any more - needs this to tell it
    /// from a kernel task, which never had one at all.
    user: bool,
    /// Where a user task's first dispatch goes: the entry point and the stack
    /// `enter_user` hands the CPU. Kernel tasks have none.
    user_entry: Option<(usize, usize)>,
    /// Where a *forked* child's first dispatch goes: the parent's ring-3
    /// register file at the moment of the call, with `rax` set to zero. A
    /// child resumes mid-program rather than at the entry point, so it needs
    /// a whole register file where a freshly spawned task needs one address.
    user_resume: Option<UserContext>,
    /// The thread group this task runs in: the frames behind its mappings and
    /// how many threads share them. A process with no threads has a group of
    /// one, so every task has one and nothing has to ask whether it is a
    /// thread to find its memory.
    mm: Arc<Mm>,
    /// Set once this task's exit has been accounted for. The release pass runs
    /// on every reap, and a thread that left twice would take the group's count
    /// down twice - which would free memory a sibling is still using.
    released: bool,
    /// The thread group's id - the leader's task id. `getpid` answers this from
    /// every thread; `gettid` answers the task's own.
    tgid: TaskId,
    /// Signals sent to this task and not yet delivered.
    signals: signal::Pending,
    /// The `siginfo_t` behind each pending signal, indexed like `signals`. A
    /// standard signal is not queued, so one record per number is all the ABI
    /// implies; this is what lets a `sigqueue` payload reach the handler.
    siginfo: [signal::SigInfo; signal::SIGINFO_SLOTS],
    /// The signals this task has blocked with `rt_sigprocmask`. A blocked
    /// signal stays pending until it is unblocked, which is what makes a
    /// critical section able to say "not now" and be obeyed.
    blocked: signal::Pending,
    /// What `rt_sigaction` recorded for this process, per signal. Its own
    /// table, not a shared one: a fresh process must answer `SIG_DFL` for
    /// everything, whatever its parent or its sibling installed.
    dispositions: signal::Actions,
    /// The alternate signal stack this process installed with
    /// `sigaltstack`, if it has one.
    alt_stack: Option<signal::AltStack>,
    /// The descriptors this task's fd numbers index. Kernel tasks share the
    /// kernel's table; a process has one of its own.
    fds: Arc<FdTable>,
    /// The directory relative paths name things from. `None` is the root,
    /// which is where everyone starts; the VFS is mounted long after the
    /// first tasks exist, so the root inode cannot be grabbed up front.
    cwd: Option<Arc<dyn crate::vfs::Inode>>,
    /// The path `cwd` was reached by, as the caller spelled it out - what
    /// `getcwd` reports. `None` means "never left /", which is the truth for
    /// every task that has not asked to be anywhere else.
    cwd_path: Option<alloc::string::String>,
    /// The task that spawned this one in ring 3, if any. Its `wait` blocks on
    /// this task's exit; kernel-spawned tasks have none.
    parent: Option<TaskId>,
    /// The process group this task belongs to, named by its leader's id.
    ///
    /// It is what a terminal's interrupt is aimed at: Ctrl-C is not sent to
    /// the process reading the keyboard, it is sent to the *group* the
    /// foreground job lives in, which is why a pipeline of three programs all
    /// stop together and the shell that started them does not. A task inherits
    /// its parent's group, so `setpgid(0, 0)` in a child - which is what a
    /// shell does before `exec` - is the whole of what makes that job separate.
    pgid: TaskId,
    /// The session this task belongs to, named by its leader's id. Zero for
    /// tasks no `setsid` has claimed - the boot task, kernel tasks. What a
    /// session carries here is not a controlling terminal (that is `ctty`,
    /// a per-process property) but the orphan test: a process group whose
    /// members' parents all sit in other sessions - or nowhere - is a group
    /// nobody could un-stop, and stop signals are discarded for it.
    session: TaskId,
    /// The controlling terminal this process acquired with `TIOCSCTTY`, by
    /// device id. `/dev/tty` is a door to exactly this terminal for exactly
    /// this process, and a process that has acquired none cannot open the
    /// name at all - the answer a shell reads as "job control off". Inherited
    /// across `fork`, preserved by `exec`, lost at `setsid`.
    ctty: Option<(u32, u32)>,
    /// Who this process is - the real, effective and saved ids of its user
    /// and group, in that order. Root until something changes it, inherited
    /// across `fork`, preserved by `exec`.
    identity: Identity,
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
    uts: Arc<crate::uts::Uts>,
    /// The file-creation mask, `0o022` until something calls `umask`.
    ///
    /// It belongs to the process and not to the machine: a shell sets its own
    /// and every program it starts inherits that, while a program that sets a
    /// restrictive mask must not narrow what its neighbours may create. It is
    /// applied by masking the mode at every create, which is the only place
    /// it is ever read.
    umask: u32,
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
    oom_score_adj: i32,
    oom_score_adj_min: i32,
    /// The nice value, the `-20..19` number `setpriority` records and
    /// `getpriority` reports (in the raw syscall's `20-nice` encoding). Like
    /// `oom_score_adj` it is state this kernel keeps honestly rather than
    /// schedules by: the scheduler's priority *bands* are its policy and a
    /// caller cannot move a task between them, so the nice has no weight effect
    /// here (a named sub-gap) - but a program that set it and read it back must
    /// see what it set, or the call is the silent-success `PLAN.md` §4 forbids.
    /// Inherited across `fork`, kept across `exec`, as Linux keeps it.
    nice: i32,
    /// The process's resource limits, the `RLIMIT_*` table `getrlimit`/
    /// `setrlimit`/`prlimit64` read and write and `/proc/<pid>/limits` prints.
    /// Inherited whole across fork and exec, the way a limit belongs to the
    /// process rather than to the program it last ran.
    rlimits: [Rlimit; RLIM_NLIMITS],
    /// `SECCOMP_MODE_STRICT` is on for this thread: only `read`, `write`,
    /// `exit` and `sigreturn` are permitted and every other syscall is fatal
    /// (`SIGKILL`). Once set it cannot be cleared, and it survives `exec` and
    /// is inherited across `fork` - so it is a per-task latch, not a limit.
    seccomp_strict: bool,
    /// The `SECCOMP_MODE_FILTER` programs this thread has installed, oldest
    /// first. Cumulative the way Linux's are - a filter can be added and never
    /// taken away - and every one of them runs on every syscall, with the most
    /// restrictive answer deciding. Inherited across `fork`/`clone` and left
    /// alone by `exec`, both of which are the ABI's own rules.
    seccomp_filters: Vec<crate::seccomp::Program>,
    /// Per-process brk state for heap management. The frames behind the
    /// break's pages are on `user_frames` like every other frame the process
    /// owns; only the bookkeeping of *where* the break is stays here.
    brk_base: usize,
    brk_pages: usize,
    /// Per-process mmap cursor: where the next `mmap(MAP_ANONYMOUS)` that
    /// does not name an address starts looking. Each process starts where its
    /// layout drew it and only moves downward; no other process shares this.
    mmap_cursor: usize,
    /// One past the highest address this process's stack may use.
    ///
    /// Part of the same drawn layout as `brk_base` and `mmap_cursor`, and kept
    /// for the same reason: it is not a constant any more. Stack growth answers
    /// "is this address still mine?" with it, and a process that grew past its
    /// own top would be mapping over whatever the kernel had put below it.
    stack_top: usize,
    /// The path this process was started from - what `/proc/self/exe` reads
    /// through. Empty for tasks that never had an image (kernel tasks, the
    /// boot task).
    exe: alloc::string::String,
    /// The argument vector this process was entered with, `argv[0]` included -
    /// what `/proc/<pid>/cmdline` reads through, and the only record of it:
    /// the block was written onto the process's own stack, and once the
    /// program is running it is free to write over it.
    ///
    /// Shared rather than copied so that `fork` - which copies a whole process
    /// for a child that will usually `exec` something else in the next
    /// instruction - hands over one reference instead of cloning every string.
    /// `execve` replaces the whole vector, which drops the old one.
    argv: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    /// Where a child created by `clone(CLONE_CHILD_SETTID)` writes its own tid
    /// before it first runs. Zero when nothing was asked for.
    child_tid: usize,
    /// The thread-local storage base (FS) this task runs with. Per task, not
    /// per CPU: two processes each with their own TLS would otherwise overwrite
    /// each other's on every dispatch. `arch_prctl(ARCH_SET_FS)` and
    /// `clone(CLONE_SETTLS)` are what move it.
    tls: usize,
    /// The three interval timers `setitimer` arms, indexed by the ABI's
    /// `which`. `ITIMER_REAL` really expires - it counts the wall-clock
    /// ticks the board has; the other two are recorded but never fire, for
    /// want of per-task CPU accounting.
    itimers: [Option<ItimerState>; 3],
    /// The POSIX interval timers `timer_create` made, indexed by the id the
    /// call answered with. A timer belongs to the task that created it, not
    /// to its thread group - the divergence `handle_timer_create` records.
    timers: [Option<PosixTimer>; TIMER_SLOTS],
    /// What a tracer has made of this task, if anything.
    trace: Tracing,
}

/// What `ptrace` has made of a task.
///
/// One field rather than half a dozen on `Task`, because a task no tracer has
/// ever touched is the overwhelmingly common case and deserves one word of "no"
/// rather than six. [`Tracing::default`] is that case exactly: untraced, no
/// options, running.
#[derive(Clone, Copy, Default)]
struct Tracing {
    /// The task tracing this one. Set by `PTRACE_TRACEME` from the tracee's own
    /// call and by `PTRACE_ATTACH` from the tracer's.
    ///
    /// This is the whole of the relationship: a task is traced iff something is
    /// named here, and every stop below is gated on it. It is deliberately not
    /// `parent` - a tracee is usually its tracer's child, but not always, and
    /// `PTRACE_DETACH` has to be able to take the link away without touching
    /// the family.
    tracer: Option<TaskId>,
    /// The `PTRACE_O_*` word the tracer set with `PTRACE_SETOPTIONS`.
    options: usize,
    /// Where this task is stopped. `None` is a running, ready or sleeping task;
    /// `Some` is a task sitting still with its history waiting to be read.
    stop: Option<Stop>,
    /// Set by `PTRACE_SYSCALL`: stop at the next syscall boundary.
    ///
    /// Cleared by the stop it causes, not by the resume - which is exactly what
    /// makes the entry and the exit two separate events. A tracer that wants
    /// both re-arms on every resume, and the alternation follows.
    armed: bool,
    /// The user register file as of the stop.
    ///
    /// Saved rather than pointed at, twice over: the frame it came from is on
    /// the tracee's kernel stack, which nobody hands out a pointer to, and a
    /// `PTRACE_SETREGS` needs somewhere to put what it changed.
    regs: Option<[u64; abi::uregs::COUNT]>,
    /// The syscall the tracee stopped in the middle of. Written at the entry
    /// stop and kept for the exit stop, because `PTRACE_GET_SYSCALL_INFO`
    /// describes both ends of one call and the exit half has no arguments of
    /// its own.
    syscall: Option<SyscallStop>,
    /// Set by an exec trap: the syscall-exit stop that would otherwise follow
    /// it has already been reported, and must not be reported twice.
    ///
    /// The trap *is* the `execve`'s completion notice - that is what Linux's
    /// exec trap is for - so an exit stop after it would tell a tracer the same
    /// call had returned a second time. Cleared by the next entry stop, which is
    /// the only thing that can follow a disarmed resume.
    at_exec_exit: bool,
    /// A `SIGCONT` ended a stop of this task and no `wait` has reported it yet.
    ///
    /// It outlives the stop itself, and has to: by the time a parent looks, the
    /// task is running again and the stop it was told about is gone. That is
    /// the `WIFCONTINUED` word, and the reason `wait` takes the flag at all.
    continued: bool,
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
    pending_event: Option<(u32, u64)>,
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
    fn adopted() -> Task {
        Task {
            context: Context::empty(),
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
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            cwd_path: None,
            parent: None,
            pgid: TaskId(0),
            session: TaskId(0),
            ctty: None,
            identity: Identity::default(),
            // A kernel task starts in the machine's own namespace, the one
            // every task is born in until something unshares.
            uts: crate::uts::init(),
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            umask: crate::syscall::DEFAULT_UMASK,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            stack_top: USER_STACK_TOP,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        }
    }
}

struct Table {
    tasks: [Option<Task>; MAX_TASKS],
    sched: Scheduler,
    current: TaskId,
}

impl Table {
    const fn new() -> Table {
        Table {
            tasks: [const { None::<Task> }; MAX_TASKS],
            sched: Scheduler::new(),
            current: SCHED_ID,
        }
    }

    fn task(&self, id: TaskId) -> &Task {
        self.tasks[id.0]
            .as_ref()
            .unwrap_or_else(|| panic!("there is no task {}", id.0))
    }

    fn task_mut(&mut self, id: TaskId) -> &mut Task {
        self.tasks[id.0]
            .as_mut()
            .unwrap_or_else(|| panic!("there is no task {}", id.0))
    }

    fn context_of(&mut self, id: TaskId) -> *mut Context {
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
    fn enter(&mut self, id: TaskId) -> (*mut Context, *const Context) {
        self.prepare(id);
        let from = self.current;
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
    fn hand_over_fpu(&mut self, from: TaskId, to: TaskId) {
        if self.task(from).fpu_live {
            // Safe: `fpu_live` implies `CR0.TS` is clear - the only two things
            // that set the bit are this function and the switch it feeds, and
            // both clear the flag of every task they aim the bit at.
            debug_assert!(!crate::arch::context::fpu_trapped());
            let task = self.task_mut(from);
            unsafe { crate::arch::context::save_live_fpu(&mut task.context.fpu) };
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
    fn inherit_fpu(&mut self, id: TaskId) -> crate::arch::context::FpuState {
        let task = self.task_mut(id);
        if task.fpu_live {
            debug_assert!(!crate::arch::context::fpu_trapped());
            // Safe: `fpu_live` implies `CR0.TS` clear, by the same argument as
            // in `hand_over_fpu`.
            unsafe { crate::arch::context::save_live_fpu(&mut task.context.fpu) };
        }
        let bytes = task.context.fpu.to_bytes();
        crate::arch::context::FpuState::from_bytes(&bytes)
    }

    /// Make `id`'s saved floating-point block current before somebody reads it.
    ///
    /// A block is stale in exactly one situation: its task is running and still
    /// owns the registers, because every switch out of a task writes them back.
    /// The tracer's usual subject is a *stopped* task, so this is normally a
    /// no-op; it is here so that a caller asking about the running task gets its
    /// real state rather than whatever was last saved.
    fn sync_fpu(&mut self, id: TaskId) {
        let live = self
            .tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.fpu_live);
        if live {
            let task = self.task_mut(id);
            // Safe: `fpu_live` implies `CR0.TS` clear.
            unsafe { crate::arch::context::save_live_fpu(&mut task.context.fpu) };
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
    fn reclaim_fpu(&mut self) {
        // Safe: the caller is the `#NM` handler, so the bit is set - clearing
        // it is what lets the faulting instruction be re-executed.
        unsafe { crate::arch::context::claim_live_fpu() };
        let id = self.current;
        let task = self.task_mut(id);
        if !task.fpu_live {
            // Safe: the image is this task's own, 16-byte aligned because
            // `FpuState` is the first member of `Context`, and the registers
            // were just released to it.
            unsafe { crate::arch::context::restore_fpu(&task.context.fpu) };
            task.fpu_live = true;
        }
    }

    /// Aim the CPU at the task that is about to run: its address space under
    /// CR3, its kernel stack under `RSP0` for traps from ring 3, and its TLS
    /// base under FS.
    fn prepare(&mut self, id: TaskId) {
        let task = self.task(id);
        let root = match &task.space {
            Some(space) => space.root(),
            // A thread owns no address space; it runs on the group's, whose
            // paging root the group remembers. A kernel task has neither, and
            // runs on the kernel's own tables.
            None if task.mm.root() != 0 => task.mm.root(),
            None => paging::kernel_root(),
        };
        paging::switch_to_root(root);
        if let Some(stack) = &task.stack {
            gdt::set_kernel_stack(stack.top());
        }
        // FS.base is one register for the whole CPU, so every task that wants
        // its own thread-local storage has to reinstall it here. A process with
        // none gets a cleared base, which is the state a fresh image starts in.
        unsafe { write_msr(MSR_FS_BASE, task.tls as u64) };
    }

    /// Put the running task at the back of its own band. This is a yield, and it
    /// is also what the clock does to a task that has used up its slice.
    fn requeue_current(&mut self) {
        let id = self.current;
        let priority = self.task(id).priority;
        self.sched.push_ready(id, priority);
    }

    /// Let `prepare` decide where the running task goes, then hand the CPU to
    /// the scheduler. One call, so that nothing - not even a tick - can happen
    /// between queueing the task and switching away from it.
    fn hand_over(&mut self, prepare: impl FnOnce(&mut Table)) -> (*mut Context, *const Context) {
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
    fn check_itimers(&mut self) {
        if !ITIMER_ARMED.load(Ordering::Relaxed) {
            return;
        }
        let now = timer::ticks();
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
    fn check_posix_timers(&mut self) {
        if !POSIX_TIMERS_ARMED.load(Ordering::Relaxed) {
            return;
        }
        let now = timer::ticks();
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
    fn next_turn(&mut self) -> Option<(*mut Context, *const Context)> {
        self.check_itimers();
        self.check_posix_timers();
        self.reap();
        self.sched.advance(timer::ticks());
        let picked = self.sched.pick();
        picked.map(|id| self.enter(id))
    }

    /// Slot 1 is left free on purpose - it is init's pid - and the top slot is
    /// the scheduler's. Everything between them is first come, first served.
    fn alloc_slot(&mut self) -> usize {
        (FIRST_ALLOC_SLOT..MAX_TASKS - 1)
            .find(|&slot| self.tasks[slot].is_none())
            .unwrap_or_else(|| panic!("all {MAX_TASKS} task slots are in use"))
    }

    /// Allocate a slot, a stack and a bootstrap context for a kernel task. The
    /// task is not queued: the scheduler is created before there is a policy to
    /// queue it in, and must never be queued at all.
    fn create(
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
    fn create_at(
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
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, interruptible) };

        self.tasks[slot] = Some(Task {
            context,
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
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            cwd_path: None,
            parent: None,
            pgid: TaskId(0),
            session: TaskId(0),
            ctty: None,
            identity: Identity::default(),
            // A kernel task starts in the machine's own namespace, the one
            // every task is born in until something unshares.
            uts: crate::uts::init(),
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            umask: crate::syscall::DEFAULT_UMASK,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            stack_top: USER_STACK_TOP,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
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
    fn release_exited(&mut self) {
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
                    // (`PLAN.md` §11.1.3, V-3, and `sched::exit` for the rule
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
    fn write_clear_tid(&mut self, owner: TaskId, address: usize) {
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
        if paging::phys_for_virt_of(root, address).is_none() {
            populate_in(self, root, address, true);
        }
        crate::arch::paging::write_user_u32(root, address, 0);
    }

    /// Which task owns the address space a group runs on. A process owns its
    /// own; a thread does not own one at all, so the leader's is the group's.
    fn space_owner(&self, id: TaskId) -> TaskId {
        self.task(id).mm.leader()
    }

    fn reap(&mut self) {
        self.release_exited();
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

            // A waited child's CPU folds into its parent's child totals before
            // the slot is freed - Linux accumulates on `wait`. Captured here and
            // applied after the loop, because `self.tasks[parent]` cannot be
            // borrowed while `iter_mut` holds the table. Kernel/boot tasks have
            // no parent, so nothing is folded for them.
            if let Some(parent) = task.parent {
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
            *entry = None;
        }
        // The table borrow is released, so the parent's `Mm` is reachable now.
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

/// Adopt the running stack as the first task and start the scheduler.
///
/// Call this once, before any [`spawn`]. The code that calls it becomes a task:
/// it can yield, sleep and spawn like any other, and when it returns from
/// [`yield_now`] it is because the scheduler put it back.
pub fn init() {
    assert!(
        !STARTED.load(Ordering::Relaxed),
        "the scheduler is already running"
    );

    with_table(|t| {
        t.tasks[BOOT_SLOT] = Some(Task::adopted());
        t.current = TaskId(BOOT_SLOT);

        // The scheduler is a task like any other except that its body never
        // returns and it is entered by switching to it rather than by being
        // picked. It starts with interrupts off and stays that way: the only
        // place it enables them is the idle halt.
        //
        // Its slot is named rather than allocated, because it must never take
        // the slot init is going to be born in.
        let id = t.create_at(SCHED_ID.0, scheduler_loop, Priority::CRITICAL, false);
        assert_eq!(
            id, SCHED_ID,
            "the scheduler did not land in its reserved slot"
        );
    });

    STARTED.store(true, Ordering::Relaxed);
}

/// Start a new task running `body` at `priority`. It runs the next time the
/// scheduler gets a turn, not before this returns.
pub fn spawn(body: impl FnOnce() + 'static, priority: Priority) -> TaskId {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn() was called before sched::init()"
    );
    with_table(|t| {
        let id = t.create(body, priority, true);
        t.sched.push_ready(id, priority);
        id
    })
}

/// Start a task in ring 3, running the ELF64 image `bytes` at `priority`.
///
/// The image is parsed first, then its pages and a stack are mapped into a
/// fresh address space, so the task's first dispatch goes straight to user
/// mode. The program runs the next time the scheduler gets a turn, not before
/// this returns.
///
/// Nothing about the image is trusted past [`elf::parse`]: a segment that
/// reaches into the kernel half is refused there, and every page placed here
/// comes from frames this code allocates, never from the file's claims.
pub fn spawn_user(bytes: &[u8], priority: Priority) -> TaskId {
    try_spawn_user(bytes, priority, current(), &[], &[], &[], "")
        .expect("spawn_user: not an ELF64 executable mpos can load")
}

/// Start the very first user process: init, in the slot that makes it pid 1.
///
/// Nothing spawns it, so nothing is its parent until it has children of its
/// own; the boot task keeps no link, because it has no `wait` to collect with
/// and a record nobody reads is a record that lies to the next `wait`.
/// The bytes and argv of the program a spawn actually runs.
///
/// A `#!` chain is followed here exactly as the `execve` handler follows it,
/// and for the same reason: the kernel's own spawns - pid 1 above all - start
/// from a file's bytes, and the file may be a script whose interpreter is a
/// *path into the filesystem* that only a read can resolve. Without this, a
/// kernel that execs scripts for its processes but not for its own spawns
/// hands pid 1 an image it then refuses to run.
///
/// `start` is the cwd a relative interpreter is resolved against: the syscall
/// passes the caller's, a kernel-side spawn passes the root.
pub fn resolve_script(
    bytes: Vec<u8>,
    mut argv: Vec<String>,
    start: Option<alloc::sync::Arc<dyn crate::vfs::Inode>>,
    exec_path: &str,
) -> Result<(Vec<u8>, Vec<String>), SpawnError> {
    let mut bytes = bytes;
    // The path the file in `bytes` was reached by - `execve`'s own argument for
    // the first file, and each interpreter's name for the next. A script is
    // handed to its interpreter by *this* path, not by the name the caller
    // chose for argv[0]: a `PATH` search finds `/usr/bin/mkfontdir` while the
    // shell still calls it through argv as plain `mkfontdir`, and an
    // interpreter told to open `mkfontdir` looks for it in the working
    // directory and fails on a file that is really elsewhere.
    let mut script_path = String::from(exec_path);
    let mut hops = 0;
    while let Some((interp, argument)) = crate::binfmt::shebang(&bytes) {
        hops += 1;
        if hops > crate::binfmt::SHEBANG_HOPS_MAX {
            // A chain of interpreters that leads back into itself.
            return Err(SpawnError::Loop);
        }
        // The interpreter is the new argv[0]; the resolved path stands in for
        // the name the script was invoked by, and the caller's own arguments
        // follow it.
        let mut rewritten: Vec<String> = Vec::with_capacity(argv.len() + 2);
        rewritten.push(interp.clone());
        if let Some(argument) = argument {
            rewritten.push(argument);
        }
        rewritten.push(core::mem::take(&mut script_path));
        rewritten.extend(argv.iter().skip(1).cloned());
        argv = rewritten;
        script_path = interp.clone();
        bytes = match start.as_ref() {
            Some(dir) => crate::vfs::read_file_opt(&script_path, Some(dir.clone())),
            None => crate::vfs::read_file(&script_path),
        }
        .map_err(|_| SpawnError::NoInterpreter)?;
    }
    Ok((bytes, argv))
}

pub fn spawn_init(bytes: &[u8], argv: &[String], env: &[String]) -> Result<TaskId, SpawnError> {
    let path = argv.first().cloned().unwrap_or_default();
    let (bytes, argv) = resolve_script(bytes.to_vec(), argv.to_vec(), None, &path)?;
    let id = try_spawn_init(&bytes, &argv, env)?;
    assert_eq!(id, INIT_PID, "init did not land on pid 1");
    Ok(id)
}

/// [`spawn_init`] without the pid assertion, so the loader can be exercised
/// before the slot layout is taken as given.
pub fn try_spawn_init(bytes: &[u8], argv: &[String], env: &[String]) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_init() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // Whether init has an interpreter depends on which init it is. The kernel
    // used to start its own program, statically linked, when the initramfs was
    // the root and no filesystem was up to read a loader from. The machine now
    // hands over to the root filesystem's init - and on this userland that is
    // a link to busybox, which is dynamically linked. Both cases are the same
    // call: an image that names no interpreter reads none.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    // The interpreter first, because the layout has to know about it: the two
    // are stacked against each other rather than drawn independently, so that no
    // draw can drop one on top of the other.
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, "", layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = INIT_PID.0;
        assert!(
            t.tasks[slot].is_none(),
            "init's slot {slot} is already taken"
        );
        drop_retired(INIT_PID);
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            "",
            interp_name,
        );
        write_arg_block(space.root(), user_rsp, argv, env, "", &loaded);

        let root = space.root();

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, true) };

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority: Priority::NORMAL,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::with_eager(TaskId(slot), root, loaded.frames, loaded.ranges),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: Arc::new(FdTable::with_standard(crate::vfs::tty::global())),
            cwd: None,
            cwd_path: None,
            parent: None,
            // init leads the first process group, so a command started
            // before any job control exists still has a group to land in.
            pgid: INIT_PID,
            // ...and the first session, of which it is the leader: getty's
            // `setsid` splits away from it, and the orphan test reads the
            // difference between init's session and a getty's.
            session: INIT_PID,
            ctty: None,
            // Init starts as root, like everything the kernel itself spawns.
            identity: Identity::default(),
            // ...and in the machine's own namespace, the one a boot begins in.
            uts: crate::uts::init(),
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            umask: crate::syscall::DEFAULT_UMASK,
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: loaded.brk,
            brk_pages: 0,
            mmap_cursor: layout.mmap_top,
            stack_top: loaded.stack_top,
            exe: alloc::string::String::new(),
            // The one record of how init was started: `/proc/1/cmdline` and
            // `ps` both read it back, and an empty one reads as a process
            // that has no name at all.
            argv: alloc::sync::Arc::new(argv.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });
        t.sched.push_ready(INIT_PID, Priority::NORMAL);
        Ok(INIT_PID)
    })
}

/// [`spawn_user`] with arguments and a parent: the process form the `spawn`
/// syscall uses. `argv` is the full argument vector, `argv[0]` being the
/// program name as POSIX frames it - it does not have to match `bytes`' path.
/// `env` is the environment vector, laid out on the child's stack the way the
/// Linux ABI does: `envp` follows the argv NULL, and is what a libc's
/// `environ` and Rust's `env::var` read through. `exe` is the path `bytes`
/// was read from, and is what `/proc/self/exe`
/// reads through; spawn sites that know it pass it, kernel-internal starts
/// that have no name for their image pass an empty string.
///
/// The parent link is what `wait` needs: when the task exits, its parent is
/// woken and reads the exit code.
pub fn try_spawn_user(
    bytes: &[u8],
    priority: Priority,
    parent: TaskId,
    argv: &[String],
    env: &[String],
    redirects: &[Redirect],
    exe: &str,
) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_user() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // The interpreter, found the same way `execve` finds it. This used to pass
    // none, on the assumption that only `execve` starts dynamic programs -
    // which held only while every program the kernel started itself was static.
    // Alpine's userland is dynamically linked, so the shell the milestones hand
    // the machine to is one of them, and a dynamic program started at its own
    // entry point runs before its relocations are applied: the first call
    // through the GOT jumps to nothing.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, exe, layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            exe,
            interp_name,
        );
        // Read before the space moves into the task: a thread switches to
        // this root, and the group is what remembers it.
        let root = space.root();

        // The argument block is written last, into the stack this task will
        // land on. It is placed from its *top* down - the strings grow
        // towards the program's entry point - and it may span pages, so the
        // write resolves the stack's frames as it crosses them rather than
        // assuming the block lives in one.
        write_arg_block(space.root(), user_rsp, argv, env, exe, &loaded);

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, true) };

        // A process's descriptors are its own from the first instruction: the
        // standard three over the console, and nothing of anyone else's - not
        // even the starter's, which is why a pipe end has to be handed over
        // explicitly. What it closes is then its business, not the shell's.
        let fds = Arc::new(FdTable::with_standard(crate::vfs::tty::global()));
        // The child's table is fresh, so it inherits no descriptor the starter
        // happened to be holding - a pipe's write end in the starter's own
        // table, say. That is what makes a pipeline's end-of-file arrive: the
        // reader sees the writers close because nobody else ever had one.
        for &(from, to) in redirects {
            let file = match t.task(parent).fds.clone_at(from) {
                Ok(file) => file,
                Err(_) => return Err(SpawnError::BadFd),
            };
            fds.install_at(file, to).map_err(|_| SpawnError::BadFd)?;
        }

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::with_eager(TaskId(slot), root, loaded.frames, loaded.ranges),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds,
            cwd: t.task(parent).cwd.clone(),
            cwd_path: t.task(parent).cwd_path.clone(),
            parent: Some(parent),
            pgid: t.task(parent).pgid,
            session: t.task(parent).session,
            ctty: t.task(parent).ctty,
            identity: t.task(parent).identity.clone(),
            // Inherited, not copied: a program the kernel starts shares the
            // namespace of whatever started it until it unshares, which is the
            // rule `fork` follows for the same reason.
            uts: Arc::clone(&t.task(parent).uts),
            umask: t.task(parent).umask,
            oom_score_adj: t.task(parent).oom_score_adj,
            oom_score_adj_min: t.task(parent).oom_score_adj_min,
            nice: t.task(parent).nice,
            rlimits: t.task(parent).rlimits,
            seccomp_strict: t.task(parent).seccomp_strict,
            // A filter is inherited, not shared: the child gets its own copy of
            // the list, so a filter installed *after* the fork does not appear
            // in the child, and the parent cannot add to the child's.
            seccomp_filters: t.task(parent).seccomp_filters.clone(),
            brk_base: loaded.brk,
            brk_pages: 0,
            mmap_cursor: layout.mmap_top,
            stack_top: loaded.stack_top,
            exe: alloc::string::String::from(exe),
            argv: alloc::sync::Arc::new(argv.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });
        let id = TaskId(slot);
        t.sched.push_ready(id, priority);
        // A parent waiting for a child wakes here, in the same critical
        // section that made the condition true.
        if t.tasks.get(parent.0).and_then(Option::as_ref).is_some() {
            t.sched.wake(parent);
        }
        // The console's interrupt belongs to the program now, not to the shell
        // that started it: a shell which has spawned a command is waiting, and
        // a Ctrl-C while it waits is aimed at the command.
        let tty = crate::vfs::tty::global();
        // A new task joins the foreground as part of its group, which is the
        // group it inherited - so for the ordinary case this changes nothing
        // and only records that the group is the thing that matters.
        if tty.is_foreground(parent) {
            let pgid = t.task(id).pgid;
            tty.set_foreground(Some(pgid));
        }
        Ok(id)
    })
}

/// The stack pointer a process is entered with, laid out the way the libc's
/// `_start` reads it, or `None` when the arguments would not fit the top page.
///
/// The Linux x86_64 ABI expects, from low to high addresses:
///
/// ```text
/// argc | argv[0..n] | NULL | envp[0..n] | NULL |
/// AT_PHDR | AT_PHENT | AT_PHNUM | AT_PAGESZ | AT_BASE | AT_ENTRY |
/// AT_UID | AT_EUID | AT_GID | AT_EGID | AT_HWCAP | AT_CLKTCK |
/// AT_SECURE | AT_RANDOM | AT_EXECFN | AT_NULL |
/// random bytes (16) | envp strings | argv strings
/// ```
///
/// The auxv entries let musl-based libc (and other Linux-compatible
/// runtimes) find the program headers, initialise TLS and the stack guard,
/// and answer `getuid`/`sysconf` without a syscall. This is the layout Rust
/// std's `env::args()` and `env::var()` read through: a libc `main` receives
/// `(argc, argv, envp)` off this block, and `environ` points at the `envp`
/// array above it.
const AUXV_WORDS: usize = 32; // 16 (type, value) pairs, AT_NULL included.

fn initial_stack_rsp(
    argv: &[String],
    env: &[String],
    exe: &str,
    stack_top: usize,
) -> Option<usize> {
    let arg_bytes: usize = argv.iter().map(|s| s.len() + 1).sum();
    let env_bytes: usize = env.iter().map(|s| s.len() + 1).sum();
    // The AT_EXECFN string: its own copy when the caller knows the path,
    // nothing extra when argv[0] can serve, a lone NUL when neither exists.
    let execfn_bytes: usize = if !exe.is_empty() {
        exe.len() + 1
    } else if argv.is_empty() {
        1
    } else {
        0
    };
    if argv.len() > ARGV_MAX || arg_bytes + env_bytes + execfn_bytes > ARGS_BYTES_MAX {
        return None;
    }
    let argc = argv.len();
    let envc = env.len();
    // argc, argv[0..argc] + NULL, envp[0..envc] + NULL, auxv.
    let words_bytes = (1 + argc + 1 + envc + 1 + AUXV_WORDS) * size_of::<usize>();
    // 16 bytes of random data for the AT_RANDOM auxv entry.
    let random_bytes = 16usize;
    let total = words_bytes + random_bytes + arg_bytes + env_bytes + execfn_bytes;
    let rsp = (stack_top - total) & !15;
    // The block has to land on the pages the loader maps. Everything below
    // them is demand-paged, and the fault that would claim one has nothing to
    // fault on yet: the program has not started. Linux bounds the block by
    // `ARG_MAX` and lets it span pages; this is the same shape at the size of
    // the stack a program starts with.
    (stack_top - rsp <= USER_STACK_PAGES * PAGE_SIZE).then_some(rsp)
}

/// Fill the initial stack page with the argument block [`initial_stack_rsp`]
/// laid out: random bytes and strings at the top, auxv, envp and argv
/// pointers and `argc` below them.
///
/// # Safety
///
/// The frame must back the pages between `loaded.stack_top` and the `user_rsp`
/// [`initial_stack_rsp`] computed from the same `argv` and `env` - the two are
/// one layout, and a block written against a different stack top lands in
/// whichever pages *that* layout mapped.
///
/// `exe` is the execution path the `AT_EXECFN` entry points at, the string a
/// libc derives `progname` from by scanning it for NUL. It must be
/// NUL-terminated somewhere in the block: an `argv`-empty spawn with no path
/// of its own falls back to an empty string, never to the random bytes, or
/// the scan walks off the stack.
fn write_arg_block(
    root: usize,
    user_rsp: usize,
    argv: &[String],
    env: &[String],
    exe: &str,
    loaded: &Loaded,
) {
    // The block is built here and copied to the user stack in one piece. It
    // may span pages - a *program* is not a pathname, and a completion helper
    // handing a subprocess a few kilobytes of script is an ordinary thing for
    // it to do - while the stack's pages are separate frames, so a writer that
    // reaches them through one page's worth of bytes can only place a block
    // that fits in one. The kernel's own memory has no such shape, and
    // [`write_user_of`] resolves a page at a time on the way across.
    //
    // The address space is named by its root rather than taken as "the
    // current one": a task being spawned is not running yet, so its stack is
    // not the stack the kernel is on.
    let total = loaded.stack_top - user_rsp;
    let mut block = alloc::vec![0u8; total];
    let base = user_rsp;

    let argc = argv.len();
    let mut off = 0usize;

    // argc
    block[off..off + 8].copy_from_slice(&(argc as u64).to_le_bytes());
    off += 8;

    // Random bytes at the very top of the stack (16 bytes, 16-byte aligned).
    let random_at = total - 16;
    let random_vaddr = base + random_at;
    // AT_RANDOM must be unique per exec: libc derives the `__stack_chk_guard`
    // (stack-smashing canary) from these bytes, so a constant here is a
    // predictable canary - the whole point of 8.3's stack protection is lost.
    // Draw them from the kernel ChaCha20 pool, freshly seeded, like `getrandom`.
    let mut random = [0u8; 16];
    crate::rand::fill(&mut random);
    block[random_at..random_at + 16].copy_from_slice(&random);

    // Strings, descending from just below the random bytes: argv first, then
    // the environment, then the AT_EXECFN path.
    let mut string_at = random_at;
    let mut argv0_vaddr = 0usize;
    for (i, arg) in argv.iter().enumerate() {
        string_at -= arg.len() + 1;
        block[string_at..string_at + arg.len()].copy_from_slice(arg.as_bytes());
        block[string_at + arg.len()] = 0;
        // argv[i] pointer: virtual address of the string.
        let vaddr = base + string_at;
        if i == 0 {
            argv0_vaddr = vaddr;
        }
        block[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL argv terminator (zero-filled block).
    off += size_of::<usize>();

    for var in env.iter() {
        string_at -= var.len() + 1;
        block[string_at..string_at + var.len()].copy_from_slice(var.as_bytes());
        block[string_at + var.len()] = 0;
        let vaddr = base + string_at;
        block[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL envp terminator (zero-filled block).
    off += size_of::<usize>();

    // The AT_EXECFN string: the path the program was executed through when
    // the caller knew it, argv[0] itself when it did not, and an empty
    // string when neither exists - the block must never point the libc's
    // progname scan at unterminated data.
    let execfn_vaddr = if !exe.is_empty() {
        string_at -= exe.len() + 1;
        block[string_at..string_at + exe.len()].copy_from_slice(exe.as_bytes());
        block[string_at + exe.len()] = 0;
        base + string_at
    } else if argv0_vaddr != 0 {
        argv0_vaddr
    } else {
        string_at -= 1;
        block[string_at] = 0;
        base + string_at
    };

    // Auxiliary vector entries.
    auxv(&mut block[off..], loaded, random_vaddr, execfn_vaddr);

    assert!(
        write_user_of_at(root, base, &block, false),
        "the argument block did not fit"
    );
}

/// Write the Linux auxiliary vector into `buf` as (type, value) u64 pairs.
///
/// The ids are the Linux `AT_*` numbers, so a Linux-built libc reads the
/// vector without translation: program headers for TLS setup, page size and
/// clock tick for `sysconf`, the identity fields for `getuid`-style calls,
/// and 16 random bytes at `random_vaddr` for the stack guard. `execfn` is
/// the virtual address of `argv[0]`'s string, what `AT_EXECFN` points at.
///
/// Every address in here is a *placed* one, which is the whole reason the vector
/// is written from [`Loaded`] rather than from the parsed image: `AT_PHDR` and
/// `AT_ENTRY` are how a libc and an interpreter find the program they were
/// handed, and a program moved from the addresses it was linked with is exactly
/// the case where the two differ. `AT_BASE` is the one entry a dynamically linked
/// program cannot do without - it is how the interpreter finds *itself*, since
/// nothing told it where it was put - and zero means the program is static.
fn auxv(buf: &mut [u8], loaded: &Loaded, random_vaddr: usize, execfn: usize) {
    let mut off = 0usize;

    // Helper: write one auxv entry (type, value) and advance.
    macro_rules! entry {
        ($typ:expr, $val:expr) => {{
            let t = $typ as u64;
            let v = $val as u64;
            buf[off..off + 8].copy_from_slice(&t.to_le_bytes());
            buf[off + 8..off + 16].copy_from_slice(&v.to_le_bytes());
            off += 16;
        }};
    }

    entry!(3, loaded.at_phdr); // AT_PHDR
    entry!(4, loaded.phent); // AT_PHENT
    entry!(5, loaded.phnum); // AT_PHNUM
    entry!(6, PAGE_SIZE); // AT_PAGESZ
    entry!(7, loaded.base); // AT_BASE (0 = no dynamic linker)
    entry!(9, loaded.at_entry); // AT_ENTRY
    // The ids are the ones the new image will run with, which after a set-uid
    // exec are not the ones the caller had. libc reads them here rather than
    // asking, and a program that found the wrong answer would take the wrong
    // path through its own privilege handling - musl's `issetugid` is this
    // entry.
    let id = current_identity();
    entry!(11, id.ruid); // AT_UID
    entry!(12, id.euid); // AT_EUID
    entry!(13, id.rgid); // AT_GID
    entry!(14, id.egid); // AT_EGID
    entry!(16, 0usize); // AT_HWCAP
    entry!(17, 100usize); // AT_CLKTCK
    // AT_SECURE: the exec raised an effective id, so libc has to take its
    // hardened path - no `LD_PRELOAD`, no `LD_LIBRARY_PATH`, sanitised locale.
    // Linux computes it the same way, as "did the ids come out different".
    let secure = id.euid != id.ruid || id.egid != id.rgid;
    entry!(23, secure as usize); // AT_SECURE
    entry!(25, random_vaddr); // AT_RANDOM
    entry!(31, execfn); // AT_EXECFN
    entry!(0, 0usize); // AT_NULL
    let _ = off;
}

/// Where the pieces of a loaded program ended up, which is what the initial
/// stack has to describe to the libc that reads it.
///
/// Every address here is a *placed* one: the image's own `entry` and `phdr_addr`
/// are offsets from where it was put, and a libc told the offset instead of the
/// address walks off the end of its own memory looking for its program headers.
/// That is the whole reason this structure exists next to [`Layout`] rather than
/// as a few more arguments - the summing happens once, here, and cannot be
/// forgotten in the four places that read them afterwards.
struct Loaded {
    /// The address control is transferred to. With an interpreter this is the
    /// interpreter's entry point rather than the program's: the interpreter
    /// is what finishes setting the process up, and it finds the program
    /// through `AT_ENTRY`.
    entry: usize,
    /// What `AT_ENTRY` says: the *program's* entry, wherever the program
    /// landed - which is not the address the process starts at when there is an
    /// interpreter, and is the same address when there is not.
    at_entry: usize,
    /// What `AT_PHDR` says: the program header table, wherever it landed. The
    /// libc's TLS setup reads this and walks `phnum` entries of `phent` bytes.
    at_phdr: usize,
    phnum: usize,
    phent: usize,
    /// The interpreter's load address, reported on the initial stack as
    /// `AT_BASE`. Zero when the program is static, which is what the ABI uses
    /// to mean "there is no interpreter".
    base: usize,
    /// The stack top this program was laid out against, repeated here so that
    /// the argument block and the pages under it agree without a third value
    /// having to be threaded between them.
    stack_top: usize,
    /// The first address above the image, where the heap begins.
    brk: usize,
    /// What the loader put where, folded into the runs `/proc/<pid>/maps`
    /// prints: the program's segments, the interpreter's, and the stack. See
    /// [`MappedRange`] for why these are not VMAs.
    ranges: Vec<MappedRange>,
    /// Every frame that was mapped for this process, for the reaper to give
    /// back when it exits.
    frames: Vec<usize>,
}

/// Map every page of `image` into `space` at `base` - file bytes, the zero
/// tail of each segment past them - and record the frames as it goes.
///
/// `base` is the load address: the interpreter's is [`INTERP_BASE`], and a
/// position-independent program's is the address its layout drew. A fixed one
/// keeps zero, because the addresses in its own instructions are the ones it
/// expects to run at.
fn map_image(space: &mut AddressSpace, image: &Image<'_>, base: usize, frames: &mut Vec<usize>) {
    for segment in &image.segments {
        let vaddr = base + segment.vaddr;
        let first = pframe::align_down(vaddr, PAGE_SIZE);
        let limit = pframe::align_up(vaddr + segment.memsz, PAGE_SIZE);
        for page in (first..limit).step_by(PAGE_SIZE) {
            let frame = mm::alloc_frames(0).expect("out of frames for a user page");
            frames.push(frame);
            // Fill through the direct map, not through the mapping: the space
            // being loaded is not the active one, and the direct map is part
            // of the shared kernel half, reachable on whatever CR3 is loaded.
            let dest = unsafe {
                core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
            };
            dest.fill(0);
            // The slice of the file this page carries; everything beyond it up
            // to `memsz` stays zero, which is what .bss is.
            let file_end = vaddr + segment.data.len();
            let from = page.max(vaddr);
            let to = (page + PAGE_SIZE).min(file_end);
            if from < to {
                dest[from - page..to - page]
                    .copy_from_slice(&segment.data[from - vaddr..to - vaddr]);
            }
            let mut flags = MapFlags::READ | MapFlags::USER;
            flags.insert(segment.flags & (MapFlags::WRITE | MapFlags::EXEC));
            space.map(Mapping {
                virt: page,
                phys: frame,
                length: PAGE_SIZE,
                flags,
            });
        }
    }
}

/// Apply `image`'s R_X86_64_RELATIVE relocations for a load address of `base`.
///
/// Only the program gets this treatment; an interpreter relocates itself from
/// `AT_BASE` before it does anything else, and a second pass here would be
/// doing a libc's job with a different idea of where it starts.
///
/// Both halves of the record move with the load address, and the second one is
/// easy to miss: `r_offset` says *where* to write, in the addresses the image
/// was linked with, while `r_addend` is the *pointer* to write - an address
/// too. The relocation's whole meaning is "this word is an address, and here is
/// the one it should have been", so placing the image somewhere else has to
/// move the value as well as the slot. A static-pie loaded at a bias with only
/// `r_offset` shifted ends up with a table of pointers into the layout it was
/// linked for, and the first dereference of one is a fault in the middle of the
/// program's own data.
fn apply_rela(space: &AddressSpace, image: &Image<'_>, base: usize) {
    for &(offset, addend) in &image.rela {
        let addr = base + offset;
        if let Some(phys) = space.phys_for_virt(addr) {
            let ptr = (mm::phys_to_virt(phys & !(PAGE_SIZE - 1)) + (addr % PAGE_SIZE)) as *mut u64;
            unsafe {
                ptr.write((addend + base as i64) as u64);
            }
        }
    }
}

/// The interpreter an image names, read from the filesystem - `None` when it
/// names none.
///
/// A dynamically linked program does not run by itself: the kernel maps the
/// loader it names, hands over `AT_BASE`, `AT_PHDR` and `AT_ENTRY` on the
/// initial stack, and starts the loader instead of the program. Both of the
/// ways a program starts need that - the `execve` that replaces one process
/// with another, and the kernel's own spawn, which starts init and the shell
/// the milestones test with - so the lookup lives here rather than in either
/// of them.
///
/// The bytes come back unparsed because the parsed [`Image`] borrows from
/// them; the caller keeps the bytes alive for as long as it holds the image.
/// The path comes back beside them because `/proc/<pid>/maps` names the
/// interpreter's segments with it, and this is the one place that resolved it.
fn interpreter_bytes(image: &Image<'_>) -> Result<Option<(Vec<u8>, String)>, SpawnError> {
    let Some(path) = &image.interp else {
        return Ok(None);
    };
    let path = core::str::from_utf8(path).map_err(|_| SpawnError::NotExecutable)?;
    // A missing or unreadable interpreter reads as "the file was not found":
    // the program itself was fine, and what could not be followed is the path
    // it names.
    crate::vfs::read_file(path)
        .map(|bytes| Some((bytes, String::from(path))))
        .map_err(|_| SpawnError::NoInterpreter)
}

/// Lay out a program - and its interpreter, when it has one - in `space`, map
/// the user stack, and report where everything landed.
///
/// The interpreter is loaded first and at [`INTERP_BASE`]; the program follows
/// at `layout.bias` plus the addresses its own headers name, which is zero for a
/// fixed-address image and a drawn address for a position-independent one.
/// Nothing here tries to link the two together beyond handing over `AT_BASE`,
/// `AT_PHDR` and `AT_ENTRY` on the initial stack: resolving symbols, mapping
/// shared libraries and jumping to the program's entry is the interpreter's
/// whole purpose, and a kernel that did any of it would be guessing at a
/// contract it is not party to.
///
/// That last sentence also decides who relocates. An image with an interpreter
/// is relocated *by* it - `AT_PHDR` is enough for the loader to find the address
/// it was loaded at, which is why the value handed over is the placed one rather
/// than the linked one - so the kernel's own relocation pass runs only for a
/// static image, where nobody else will ever do the job. Doing it for both would
/// add the load bias twice to every pointer the program reads out of its own
/// data.
fn load_user(
    space: &mut AddressSpace,
    image: &Image<'_>,
    interp: Option<&Image<'_>>,
    layout: &Layout,
    program_name: &str,
    interp_name: Option<&str>,
) -> Loaded {
    let mut frames = Vec::new();
    let mut ranges = Vec::new();

    let base = match interp {
        Some(interp) => {
            map_image(space, interp, layout.interp_bias, &mut frames);
            segment_ranges(interp, layout.interp_bias, interp_name, &mut ranges);
            layout.interp_bias
        }
        None => 0,
    };

    map_image(space, image, layout.bias, &mut frames);
    segment_ranges(image, layout.bias, Some(program_name), &mut ranges);
    if interp.is_none() {
        apply_rela(space, image, layout.bias);
    }

    for index in 0..USER_STACK_PAGES {
        let frame = mm::alloc_frames(0).expect("out of frames for the user stack");
        frames.push(frame);
        let dest = unsafe {
            core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);
        space.map(Mapping {
            // Index 0 is the highest page, the one `rsp` starts in and the
            // one the argument block is written into from the top down. The
            // block may reach past it, so the pages below are mapped here
            // too - it is the *stack* the loader lays out, not one page.
            virt: layout.stack_top - (index + 1) * PAGE_SIZE,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
    }
    // The stack is one line in `/proc/<pid>/maps`, and Linux brackets its name
    // because it is not a file: `[stack]`. What is reported is the range the
    // loader mapped, which is the same thing Linux's VMA starts as - the pages
    // a program grows below it are added to the same range as it takes them
    // (`Table::grow_stack`), so the line follows the stack down.
    ranges.push(MappedRange {
        start: layout.stack_top - USER_STACK_PAGES * PAGE_SIZE,
        end: layout.stack_top,
        prot: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        name: Some(Arc::from("[stack]")),
    });

    // Control goes to the interpreter when there is one, and to the program
    // otherwise. The interpreter's own entry is a vaddr in a shared object,
    // so it only means anything once the base is added.
    let entry = match interp {
        Some(interp) => base + interp.entry,
        None => layout.bias + image.entry,
    };
    let brk_end = image
        .segments
        .iter()
        .map(|segment| segment.vaddr + segment.memsz)
        .max();

    Loaded {
        entry,
        at_entry: layout.bias + image.entry,
        at_phdr: layout.bias + image.phdr_addr,
        phnum: image.phnum,
        phent: image.phent,
        base,
        stack_top: layout.stack_top,
        // The heap starts on the page above the highest thing the image owns -
        // placed addresses, since a program that asked `brk` where it was and got
        // the address it was *linked* for would be told about a region that is
        // not its own.
        brk: brk_end.map_or(0, |end| pframe::align_up(layout.bias + end, PAGE_SIZE)),
        ranges: merge_ranges(ranges),
        frames,
    }
}

/// Record what one image's segments occupy, at the load address they were
/// placed at, so `/proc/<pid>/maps` can report them.
///
/// The page granularity and the permission word are `map_image`'s own: it
/// aligns each segment out to whole pages and maps them `READ | USER` plus
/// whatever the segment asked for. Anything else here would report a range
/// that is not the range a program can reach.
fn segment_ranges(
    image: &Image<'_>,
    base: usize,
    name: Option<&str>,
    ranges: &mut Vec<MappedRange>,
) {
    for segment in &image.segments {
        let vaddr = base + segment.vaddr;
        let mut prot = MapFlags::READ | MapFlags::USER;
        prot.insert(segment.flags & (MapFlags::WRITE | MapFlags::EXEC));
        ranges.push(MappedRange {
            start: pframe::align_down(vaddr, PAGE_SIZE),
            end: pframe::align_up(vaddr + segment.memsz, PAGE_SIZE),
            prot,
            // An empty name is no name: a kernel-started task's `exe` is empty
            // (there is no path it was run by), and a maps line with an empty
            // column is not the shape a reader parses.
            name: name.filter(|name| !name.is_empty()).map(Arc::from),
        });
    }
}

/// Give up the rest of this slice and go to the back of the band.
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
    let deadline = timer::ticks().saturating_add(ticks);
    loop {
        if let Some(sig) = take_fatal_signal() {
            exit(-(sig as isize));
        }
        if timer::ticks() >= deadline {
            return;
        }
        schedule(|t| {
            // Read the clock here rather than trusting the scheduler's last
            // turn. This task may have been running for a while since then,
            // and a deadline computed from a stale clock wakes early.
            t.sched.advance(timer::ticks());
            if timer::ticks() >= deadline {
                t.requeue_current();
                return;
            }
            let id = t.current;
            let priority = t.task(id).priority;
            // What is *left* of the wait, so a wake that was not the deadline
            // does not start the whole sleep over.
            let remaining = deadline.saturating_sub(timer::ticks()).max(1);
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
/// the pipe/futex/nanosleep waits already are (`PLAN.md` §2.5; measured on a real
/// Linux: `wait4` interrupted by a caught non-`SA_RESTART` signal returns `EINTR`,
/// and `SA_RESTART` restarts it). The deliverable signal is left pending, not
/// consumed, so the syscall gate can build the handler's frame on the way out.
fn block_until_on_interruptible(cond: impl Fn(&Table) -> bool) -> WaitEnd {
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

pub fn block_until_deadline(cond: impl Fn() -> bool, deadline: u64) -> WaitEnd {
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
        if timer::ticks() >= deadline {
            return WaitEnd::Deadline;
        }
        schedule(|t| {
            let done = cond() || table_has_deliverable(t) || timer::ticks() >= deadline;
            if done {
                t.requeue_current();
            } else {
                let id = t.current;
                let priority = t.task(id).priority;
                // The deadline is absolute, the sleep's is relative: what is
                // left of the wait is what the clock should run down, and a
                // wake may still end it before that.
                t.sched.advance(timer::ticks());
                let remaining = deadline.saturating_sub(timer::ticks()).max(1);
                t.sched.sleep(id, priority, remaining);
            }
        });
    }
}

/// [`block_until_deadline`] with no clock: the wait a syscall can be ended by
/// a signal, which is Linux's `ERESTARTSYS` shape.
///
/// The named spelling exists because that is what a syscall means by "block
/// until this is ready": the condition, or a catchable signal - and never a
/// clock. Writing it as `block_until_deadline(cond, u64::MAX)` at every one of
/// those call sites says the same thing in more words and hides which waits
/// are interruptible from the reader looking for them. The signal is *not*
/// consumed here: the syscall gate delivers it on the way out, which is the
/// only place a handler's frame can be built.
pub fn block_until_interruptible(cond: impl Fn() -> bool) -> WaitEnd {
    block_until_deadline(cond, u64::MAX)
}

/// Whether the running task has a signal that would interrupt a blocking
/// wait: pending, unblocked, and not discarded. The table-borrowing shape of
/// [`has_deliverable_signal`], for a check inside `schedule`, where the table
/// is already borrowed and no second borrow may be taken.
///
/// "Discarded" is wider than `SIG_IGN`, and the difference is measured rather
/// than inferred: a blocked `read` survived a `SIGCHLD` whose child exited, a
/// `SIGWINCH` nobody had trapped, a `SIGURG` and a `SIGCONT` - each still at
/// `SIG_DFL`, none of them ending the wait, every one of them left blocked
/// until a watchdog signal broke it. A signal whose *default action* is to be
/// ignored is as much thrown away as one set to `SIG_IGN`, so it cannot be the
/// reason a syscall comes back `EINTR`.
///
/// This is what makes a full-screen program's resize an interruption only when
/// it has a handler for the resize - which is exactly when redrawing is what the
/// signal was ever for.
fn table_has_deliverable(t: &Table) -> bool {
    let task = t.task(t.current);
    let mut ready = task.signals & !task.blocked;
    while let Some(sig) = signal::take(&mut ready) {
        let action = task.dispositions.get(sig);
        if action.is_ignore()
            || (action.is_default()
                && matches!(signal::default_action(sig), signal::DefaultAction::Ignore))
        {
            continue;
        }
        return true;
    }
    false
}

/// Whether a signal that would interrupt a blocking wait is pending, without
/// taking it off the pending set.
///
/// This is the question a blocking syscall asks before it sleeps: Linux
/// answers an interruptible wait with `EINTR` when a catchable, unblocked
/// signal is pending, and leaves the signal itself for the syscall gate to
/// deliver on the way out - the handler runs there, where a register file
/// exists to build its frame on. Discarded signals - `SIG_IGN`, and `SIG_DFL`
/// for a signal whose default action is to be ignored - are not pending
/// interruptions and do not count, same as in [`take_deliverable_signal`].
pub fn has_deliverable_signal() -> bool {
    with_table(|t| table_has_deliverable(t))
}

/// Take a child's exit code if one is waiting, and report `None` - not
/// "keep waiting" - when it is not. The non-blocking half of `wait`: this is
/// how a shell reaps jobs it started in the background without stopping for
/// any of them.
pub fn poll_any_child(parent: TaskId) -> Option<(TaskId, isize)> {
    with_table(|t| {
        any_child_code(t, parent)
            .and_then(|(id, _)| take_exit_code_for(t, id, parent).map(|code| (id, code)))
    })
}

/// Whether `parent` has a child at all - one still running, or one that has
/// exited and is waiting to be collected.
///
/// `wait` needs the distinction to tell `WNOHANG`'s two "nothing to report"
/// answers apart: a child that has simply not exited yet is a 0, while having no
/// child at all is `ECHILD`.
pub fn has_children(parent: TaskId) -> bool {
    with_table(|t| child_alive(t, parent) || any_child_code(t, parent).is_some())
}

/// The directory relative paths this task names things from, or the root
/// when it has never asked to be anywhere else.
pub fn current_cwd() -> Arc<dyn crate::vfs::Inode> {
    with_table(|t| match &t.task(t.current).cwd {
        Some(dir) => dir.clone(),
        None => crate::vfs::global().root_inode(),
    })
}

/// Move the running task's cwd to `dir`. Checked to be a directory by the
/// caller; this only remembers.
pub fn set_cwd(dir: Arc<dyn crate::vfs::Inode>) {
    with_table(|t| t.task_mut(t.current).cwd = Some(dir));
}

/// The path the running task's cwd was reached by, or "/" when it has never
/// moved - the answer `getcwd` owes its caller.
pub fn current_cwd_path() -> alloc::string::String {
    with_table(|t| {
        t.task(t.current)
            .cwd_path
            .clone()
            .unwrap_or_else(|| alloc::string::String::from("/"))
    })
}

/// Remember the path the running task's cwd was reached by. The caller
/// normalizes it; this only stores.
pub fn set_cwd_path(path: alloc::string::String) {
    with_table(|t| t.task_mut(t.current).cwd_path = Some(path));
}

/// The task `id` was spawned by, when it has one in ring 3. This is what
/// `getppid` reads through.
pub fn parent_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().and_then(|task| task.parent))
}

/// Wait for a signal. With no handlers yet every signal is fatal, so nothing
/// returns from here: the task ends inside [`block_until`].
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
    with_table(|t| t.sched.wake(id))
}

/// Whether a signal is discarded at the moment it is generated, because the
/// task has said it does not want it.
///
/// A disposition of `SIG_IGN` means exactly "never deliver this", and Linux
/// discards such a signal at generation rather than letting it sit pending.
/// The difference is observable and was a real bug here: a self-ignored
/// `SIGALRM` (or any ignored signal) left in the pending set becomes
/// deliverable again the moment the disposition changes back to `SIG_DFL` -
/// the *default action* then running for a signal the process had explicitly
/// refused - and `rt_sigpending` reports a signal no handler will ever take.
///
/// `SIGCONT` is the one exception, and on purpose: ignoring it does not mean
/// "do not resume me". Its effect is on the task's *state* - a stopped task is
/// resumed by it - and Linux carries that out even under `SIG_IGN`. The state
/// change lives in [`send_signal_info`], below the pending-set write this
/// guards, so it stays.
fn discarded_when_generated(task: &Task, sig: u8) -> bool {
    sig != signal::SIGCONT && task.dispositions.get(sig).is_ignore()
}

/// Add `sig` to `task`'s pending set and store the `siginfo_t` a handler will
/// read for it. The one place a bit and its record are set together, so no
/// raise site can leave a handler looking at a zeroed `signo`.
///
/// An ignored signal never gets here in effect: `task`'s disposition decides,
/// and the kernel's own raises (a timer expiry, a broken pipe, a child's exit)
/// follow the same rule a `kill` does.
fn queue_signal(task: &mut Task, sig: u8, info: signal::SigInfo) {
    if discarded_when_generated(task, sig) {
        return;
    }
    signal::add(&mut task.signals, sig);
    if (sig as usize) < signal::SIGINFO_SLOTS {
        task.siginfo[sig as usize] = info;
    }
}

/// Send `sig` to `id` with an `SI_USER` record naming the caller. The plain
/// `kill`/`tgkill` shape; a `sigqueue` goes through [`send_signal_info`] so it
/// can carry a payload.
pub fn send_signal(id: TaskId, sig: u8) -> bool {
    let (pid, uid) = with_table(|t| {
        let cur = t.task(t.current);
        (cur.tgid.0 as i32, cur.identity.ruid as i32)
    });
    send_signal_info(id, sig, signal::SigInfo::user(sig, pid, uid))
}

/// Send `sig` to `id`, recording `info` as the `siginfo_t` a handler will read.
///
/// Returns `false` when the task does not exist or has already exited, which
/// the syscall layer reports as `ESRCH`.
///
/// `sig` of 0 is the POSIX existence probe: it delivers nothing and only
/// reports liveness. With no handler machinery there is nothing to deliver to
/// anyway: the bit waits in the task's pending set, and the task dies at its
/// next blocking point.
pub fn send_signal_info(id: TaskId, sig: u8, info: signal::SigInfo) -> bool {
    with_table(|t| {
        // The first look is immutable: whether the signal is delivered,
        // discarded or refused, the answer is read off the same facts.
        let Some(task) = t.tasks.get(id.0).and_then(Option::as_ref) else {
            return false;
        };
        if task.exited {
            return false;
        }
        if sig == 0 {
            // The existence probe delivers nothing. Every answer it has is in
            // the `true` above.
            return true;
        }
        // POSIX 2.4.3: a stop signal generated for a member of an orphaned
        // process group is discarded. The group is one whose members' parents
        // are all gone or all in other sessions - a group nothing could
        // un-stop, because the only un-stop is a SIGCONT from outside and no
        // same-session parent outside the group is left to send one. A shell
        // that finds itself in the background of its terminal raises SIGTTIN
        // to its own group exactly this way; discarding it is the difference
        // between "job control off" and a login that freezes forever.
        // `SIGSTOP` is not on the list: it is uncatchable, and `kill -STOP`
        // stays exactly what it says.
        if signal::stops_by_default(sig) && sig != signal::SIGSTOP && orphaned_group(t, task.pgid) {
            return true;
        }
        let Some(task) = t.tasks.get_mut(id.0).and_then(Option::as_mut) else {
            return false;
        };
        // An ignored signal is discarded as it is generated: the process has
        // said it does not want it, and a bit left pending would come back to
        // life - with its *default* action - the moment the disposition
        // changed. `SIGCONT` is exempt, and the state change it carries is
        // below this: ignoring a resume does not mean "stay stopped".
        if !discarded_when_generated(task, sig) {
            signal::add(&mut task.signals, sig);
            if (sig as usize) < signal::SIGINFO_SLOTS {
                task.siginfo[sig as usize] = info;
            }
        }
        // `SIGCONT` is the one signal whose effect is on the *state* of a task        // rather than on its pending set, and this is the only place a task's
        // state can be changed from outside it.
        if sig == signal::SIGCONT {
            // It cancels the stop signals still queued, so that a task woken
            // by it does not stop again on the way out.
            for stop_sig in [
                signal::SIGSTOP,
                signal::SIGTSTP,
                signal::SIGTTIN,
                signal::SIGTTOU,
            ] {
                task.signals &= !signal::bit(stop_sig);
            }
            // And it ends a group stop. A *traced* stop is deliberately not
            // ended here: only the tracer may resume its tracee, and a
            // `SIGCONT` that resumed one would not break the stop so much as
            // the protocol the stop exists for.
            if task.trace.stop.is_some_and(|stop| stop.group) {
                task.trace.stop = None;
                task.trace.continued = true;
            }
        }
        // A stop signal has nothing to say to a task that is already stopped,
        // and waking it would say the opposite of the signal: the park a group
        // stop sits in is held by the scheduler's sleep, so a wake releases it
        // and the task runs on with a stop nobody ended. `kill -STOP` on a
        // *running* task still has to wake it, or the stop waits for the next
        // syscall boundary - and one that arrives while it is parked in a
        // `PTRACE_ATTACH`'s own queue is the case a debugger's target is in.
        let held = signal::stops_by_default(sig) && task.trace.stop.is_some_and(|stop| stop.group);
        if !held {
            t.sched.wake(id);
        }
        true
    })
}

/// Whether `pgid` is an orphaned process group: no live member has a live
/// parent in the same session outside the group.
///
/// That is POSIX's definition, in the words this kernel can now honour. The
/// parent that keeps a group un-orphaned is one `wait`ing in the same
/// session but a different group - the shell behind a job. A getty's session
/// has no such parent: the login shell's parent, login, is in the group, and
/// login's own parent, init, sits in another session - so a login shell that
/// raises a stop signal to itself has it discarded rather than stranded
/// forever with the screen frozen behind it.
fn orphaned_group(t: &Table, pgid: TaskId) -> bool {
    t.tasks.iter().flatten().all(|task| {
        !(task.pgid == pgid
            && task
                .parent
                .and_then(|parent| t.tasks.get(parent.0).and_then(Option::as_ref))
                .is_some_and(|parent| {
                    !parent.exited && parent.session == task.session && parent.pgid != pgid
                }))
    })
}

/// The process group the running task belongs to.
pub fn current_pgid() -> TaskId {
    with_table(|t| t.task(t.current).pgid)
}

/// The group `id` is in, or `None` when the task is gone.
pub fn pgid_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().map(|task| task.pgid))
}

/// Bind the running task's controlling terminal, by the terminal's device id.
///
/// `TIOCSCTTY` is the caller. Binding is two facts at once: `/dev/tty` opens
/// onto this terminal for this process from now on, and - the Linux
/// `__proc_set_tty` behaviour - the terminal's foreground goes to the
/// acquiring process's group, so the getty that just became a session leader
/// also owns the line before anyone reads or writes it.
pub fn acquire_controlling_tty(dev_id: (u32, u32)) {
    with_table(|t| {
        t.task_mut(t.current).ctty = Some(dev_id);
    });
    // Outside the table borrow: the foreground is the tty's own state, and
    // `set_foreground` takes its own critical section.
    if let Some(tty) = crate::vfs::tty::by_dev_id(dev_id) {
        tty.set_foreground(Some(current_pgid()));
    }
}

/// The controlling terminal of the running task, by device id - what an
/// open of `/dev/tty` resolves to. `None` is a process that has acquired
/// none, and there is no default to fall back on: a controlling terminal is
/// the process's own history, not a machine-wide answer.
#[cfg(target_os = "none")]
pub fn controlling_tty_of_current() -> Option<(u32, u32)> {
    with_table(|t| t.task(t.current).ctty)
}

/// The identity of the running task: the real, effective and saved ids of
/// its user and group. This is what `getuid`'s family reads and what
/// `setuid`'s family writes, after the syscall layer has applied the man
/// page's rules - the rules live there, because they are answers about
/// calls, and only the storage lives here.
#[cfg(target_os = "none")]
pub fn current_identity() -> Identity {
    with_table(|t| t.task(t.current).identity.clone())
}

/// The six ids of an [`Identity`], as [`crate::cred`] compares them.
fn ids_of(identity: &Identity) -> crate::cred::Ids {
    crate::cred::Ids {
        ruid: identity.ruid,
        euid: identity.euid,
        suid: identity.suid,
        rgid: identity.rgid,
        egid: identity.egid,
        sgid: identity.sgid,
    }
}

/// The running process's credentials, as a snapshot.
pub fn current_credentials() -> crate::cred::Credentials {
    with_table(|t| t.task(t.current).mm.credentials())
}

/// Change one field of the running process's credentials in place. The
/// read-modify-write is the table lock's, so two of these cannot interleave.
pub fn with_current_credentials<R>(f: impl FnOnce(&mut crate::cred::Credentials) -> R) -> R {
    with_table(|t| t.task(t.current).mm.with_credentials(f))
}

/// Another task's credentials, or `None` when no such task is live.
pub fn credentials_of(id: TaskId) -> Option<crate::cred::Credentials> {
    with_table(|t| {
        let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
        (!task.exited).then(|| task.mm.credentials())
    })
}

/// May the running task inspect `target` - the question `PTRACE_ATTACH` and
/// `process_vm_readv`/`writev` both ask? The policy is
/// [`crate::cred::ptrace_may_access`]; this gathers the two sides of it.
///
/// Three answers rather than two, because the callers owe different errnos:
/// `None` when no such task is live (`ESRCH`), `Some(false)` when it is live
/// and refused (`EPERM`), `Some(true)` when it may be inspected. A task always
/// passes for its own thread group, so a thread reading its own memory through
/// `process_vm_*` is unaffected.
pub fn may_inspect(target: TaskId) -> Option<bool> {
    with_table(|t| {
        let me = t.current;
        let them = t.tasks.get(target.0).and_then(Option::as_ref)?;
        if them.exited {
            return None;
        }
        let same_group = t.task(me).tgid == them.tgid;
        Some(crate::cred::ptrace_may_access(
            ids_of(&t.task(me).identity),
            &t.task(me).mm.credentials(),
            ids_of(&them.identity),
            &them.mm.credentials(),
            same_group,
        ))
    })
}

/// Store the running task's identity. The caller has validated the change;
/// this is the table's half of the bargain.
///
/// It is also where the credential rules hang, because every path that moves a
/// uid or gid - `setuid`/`setresuid`/`setreuid` and their group spellings -
/// lands here and nowhere else. A `setgroups` also lands here, with the same
/// six ids, which is why the rule asks whether any of them actually moved
/// before touching a capability: Linux does not clear a capability because a
/// process re-declared its supplementary groups.
#[cfg(target_os = "none")]
pub fn set_current_identity(identity: Identity) {
    with_table(|t| {
        let id = t.current;
        let old = t.task(id).identity.clone();
        let mm = Arc::clone(&t.task(id).mm);
        let (old_ids, new_ids) = (ids_of(&old), ids_of(&identity));
        t.task_mut(id).identity = identity;
        mm.with_credentials(|c| c.on_identity_change(old_ids, new_ids));
    });
}

/// The running task's file-creation mask - what `umask(2)` reports, and what
/// every create masks the mode it was given with.
pub fn current_umask() -> u32 {
    with_table(|t| t.task(t.current).umask)
}

/// Store the running task's file-creation mask. The syscall layer has already
/// narrowed it to the twelve bits a mode can hold.
pub fn set_current_umask(mask: u32) {
    with_table(|t| t.task_mut(t.current).umask = mask);
}

/// The OOM score adjustment of `pid` and the floor of it, as
/// `/proc/<pid>/oom_score_adj` reads the pair. `None` when no such task exists
/// - which is what makes the file go away with the process it described.
#[cfg(target_os = "none")]
pub fn oom_score_adj_of(pid: usize) -> Option<(i32, i32)> {
    with_table(|t| {
        t.tasks
            .get(pid)
            .and_then(Option::as_ref)
            .map(|tk| (tk.oom_score_adj, tk.oom_score_adj_min))
    })
}

/// Write `pid`'s OOM score adjustment, taking the floor down with it when the
/// new value asks for a more protective one - Linux's `sig->oom_score_adj_min`,
/// the record of the most protective value this process has ever been given.
/// `false` says the process is gone, so a write racing an exit reports the
/// vanished file instead of pretending to have changed it.
///
/// Who may lower the floor is not decided here: that is a rule about the
/// writer, and the file answers it where the errnos are mapped.
#[cfg(target_os = "none")]
pub fn set_oom_score_adj(pid: usize, value: i32) -> bool {
    with_table(|t| match t.tasks.get_mut(pid) {
        Some(Some(task)) => {
            task.oom_score_adj = value;
            if value < task.oom_score_adj_min {
                task.oom_score_adj_min = value;
            }
            true
        }
        _ => false,
    })
}

/// `pid`'s nice value, or `None` when no such live task exists. Backs
/// `getpriority`'s `PRIO_PROCESS`.
#[cfg(target_os = "none")]
pub fn nice_of(pid: usize) -> Option<i32> {
    with_table(|t| {
        t.tasks
            .get(pid)
            .and_then(Option::as_ref)
            .filter(|tk| !tk.exited)
            .map(|tk| tk.nice)
    })
}

/// Set `pid`'s nice value. `false` says the process is gone (or was never
/// there), which `setpriority` reports as `ESRCH`.
#[cfg(target_os = "none")]
pub fn set_nice(pid: usize, value: i32) -> bool {
    with_table(|t| match t.tasks.get_mut(pid) {
        Some(Some(task)) if !task.exited => {
            task.nice = value;
            true
        }
        _ => false,
    })
}

/// The live tasks whose real uid is `ruid`. Backs `getpriority`/`setpriority`'s
/// `PRIO_USER`.
#[cfg(target_os = "none")]
pub fn uid_members(ruid: u32) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.identity.ruid == ruid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Record the running process's `comm` from `PR_SET_NAME`: `name` is the
/// caller's bytes, stored NUL-terminated within the 16 that `TASK_COMM_LEN`
/// allows. This is what makes `PR_SET_NAME` do something - and `/proc`'s `comm`
/// and `PR_GET_NAME` report it - instead of accepting and dropping.
#[cfg(target_os = "none")]
pub fn set_current_comm(name: &[u8]) {
    with_table(|t| t.task(t.current).mm.set_comm_name(name));
}

/// The running process's `comm` as the 16 bytes `PR_GET_NAME` hands back: the
/// `PR_SET_NAME` bytes verbatim if one was set, else the derived basename packed
/// NUL-terminated - matching Linux, which reports the image name, not nothing.
#[cfg(target_os = "none")]
pub fn current_comm_bytes() -> [u8; 16] {
    with_table(|t| {
        let task = t.task(t.current);
        let stored = task.mm.comm_name_bytes();
        if stored[0] != 0 {
            return stored;
        }
        let derived = comm_of(task);
        let mut out = [0u8; 16];
        for (slot, &b) in out.iter_mut().zip(derived.as_bytes().iter().take(15)) {
            *slot = b;
        }
        out
    })
}

/// The UTS namespace the running task reads its names from - `uname`'s
/// `nodename` and `domainname`, and what `/proc/sys/kernel/{hostname,domainname}`
/// report.
///
/// The `Arc` is cloned out of the table and the table's lock dropped before the
/// caller asks the namespace anything, so reading a name cannot happen with the
/// scheduler's lock held while it allocates the `String` it hands back. That is
/// the same shape as `current_fds` above, and for the same reason.
#[cfg(target_os = "none")]
pub fn current_uts() -> Arc<crate::uts::Uts> {
    with_table(|t| Arc::clone(&t.task(t.current).uts))
}

/// Give the running task a namespace of its own: `unshare(CLONE_NEWUTS)`.
///
/// The new namespace is a **copy** of the old one - Linux's `copy_utsname`,
/// reached from `create_new_namespaces` - so the caller keeps reporting the names
/// it was already reporting and can now change them without the namespace it came
/// from following. The second half is the whole feature: a caller handed an
/// *empty* namespace would have its own `uname` change under it at the instant of
/// the call, which is not what a program asking for its own namespace is asking
/// for, and would make the call a way to lose state rather than to isolate it.
///
/// Only the calling task moves. A thread of the same process is deliberately left
/// pointing at the old namespace, which is the same thing Linux's per-task
/// `nsproxy` means - `unshare` splits the *caller* from its process, and a thread
/// that wanted to drag its siblings along would have to say so.
#[cfg(target_os = "none")]
pub fn unshare_uts() {
    with_table(|t| {
        let fresh = t.task(t.current).uts.copied();
        t.task_mut(t.current).uts = fresh;
    });
}

/// Charge one clock tick of CPU time to the running task's process group,
/// attributed user or system by the ring the timer interrupt took the core out
/// of. Called from the tick handler before it preempts, so the ticks land on the
/// task that was actually running. Two atomic increments - no allocation, no
/// sleep, bounded, as `AGENTS.md` rule 5 requires of interrupt context.
#[cfg(target_os = "none")]
pub fn charge_cpu(in_user: bool) {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    // Read `current` through the same `get(...).and_then(as_ref)` the panic
    // path uses, not `task()` - `task()` asserts a slot exists and the tick can
    // land while `current` names a slot that is momentarily empty. Nobody to
    // charge means the tick is simply not attributed; the machine is whole.
    with_table(|t| {
        if let Some(task) = t.tasks.get(t.current.0).and_then(|slot| slot.as_ref()) {
            task.mm.charge_tick(in_user);
        }
    });
}

/// The running process's CPU time in internal clock ticks, `(user, system)`.
/// `times()` and `getrusage(RUSAGE_SELF)` read it and convert to their own
/// units; there is no per-thread split because there are no threads here, and a
/// group's `Mm` is where the whole process's ticks gather. `(0, 0)` before the
/// scheduler runs or if `current` names no live task.
#[cfg(target_os = "none")]
pub fn current_cpu_ticks() -> (u64, u64, u64, u64) {
    with_table(
        |t| match t.tasks.get(t.current.0).and_then(|slot| slot.as_ref()) {
            Some(task) => task.mm.cpu_all(),
            None => (0, 0, 0, 0),
        },
    )
}

/// The group ids the running task holds - real, effective and saved.
///
/// Linux asks this as "is the file's group one of the caller's?", and this
/// kernel keeps no supplementary groups, so the three words are the whole of
/// the answer.
pub fn current_groups() -> [u32; 3] {
    let id = current_identity();
    [id.rgid, id.egid, id.sgid]
}

/// `setsid`'s kernel half beyond the group move: the caller becomes the
/// leader of its own session and loses the controlling terminal, which
/// belongs to the session it just left. The getty that calls this re-acquires
/// a terminal with `TIOCSCTTY` immediately after; everything else that
/// `setsid`s meant to leave its terminal behind anyway.
/// The effective user and group of the running process - the ids a file it
/// creates belongs to.
///
/// A *filesystem* asks, because that is where a new node is stamped and there
/// is no way to stamp it later without walking the tree: the creator's
/// identity is not a property of the name, it is a property of the moment.
/// The effective ids are the ones Linux uses, not the real ones.
pub fn current_owner() -> (u32, u32) {
    let id = current_identity();
    (id.euid, id.egid)
}

/// The *real* ids of the running task, which is the pair `access(2)` asks
/// about rather than the effective one above. The distinction is the whole
/// point of the call: a setuid program asking whether the user who invoked
/// it could open a file must not be answered with its own raised
/// privileges.
pub fn current_real_owner() -> (u32, u32) {
    let id = current_identity();
    (id.ruid, id.rgid)
}

/// Whether the running task is superuser, by its real id - the test Linux
/// substitutes for `CAP_SYS_ADMIN` on a kernel without namespaces, which is what
/// the console's ioctls ask when a caller is not the terminal's own foreground
/// session. Real rather than effective because that is the id `tty_ioctl`'s
/// checks read (`uid_eq(tty_audit_uid(...))` aside, the console has never been a
/// place where a setuid bit bought you someone else's screen).
pub fn current_is_root() -> bool {
    current_identity().ruid == 0
}

pub fn become_session_leader() {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        task.session = id;
        task.ctty = None;
    });
}

/// The session id of the session whose controlling terminal is `dev_id`, or
/// `None` when no task holds that terminal - what `TIOCGSID` reports, and what
/// `tcgetsid` is.
///
/// A job-control shell asks this before it decides whether the terminal is
/// *its* terminal to claim: the session leader that owns the line is the one
/// that may take it, and a shell that cannot find out has to assume it may
/// not. The answer is the leader's id, which is the session id everywhere in
/// the ABI.
pub fn session_of_tty(dev_id: (u32, u32)) -> Option<usize> {
    with_table(|t| {
        t.tasks
            .iter()
            .flatten()
            .find(|task| !task.exited && task.ctty == Some(dev_id))
            .map(|task| task.session.0)
    })
}

/// Everyone in a process group, for a signal that has to reach all of them at
/// once - which is what makes Ctrl-C stop a whole pipeline rather than one
/// program of it.
pub fn group_members(pgid: TaskId) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.pgid == pgid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Send `sig` to every member of a process group. Answers whether it reached
/// anyone, which is what `kill` reports for a group.
pub fn send_signal_group(pgid: TaskId, sig: u8) -> bool {
    let mut delivered = false;
    for id in group_members(pgid) {
        delivered |= send_signal(id, sig);
    }
    delivered
}

/// The live tasks of one thread group: the process that a pid names.
pub fn process_members(tgid: TaskId) -> alloc::vec::Vec<TaskId> {
    with_table(|t| {
        t.tasks
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.as_ref()
                    .is_some_and(|task| task.tgid == tgid && !task.exited)
            })
            .map(|(slot, _)| TaskId(slot))
            .collect()
    })
}

/// Linux's `kill_pid(vc->vt_pid, sig, 1)`: hand `sig` to one *process* - every
/// live thread of the group `tgid` leads - and answer the question the console
/// layer actually acts on: whether that process still exists at all.
///
/// The answer matters more than the delivery. `VT_SETMODE`'s `VT_PROCESS` is a
/// promise made by a process, and Linux's `change_console` and `vt_acquire` send
/// its `relsig`/`acqsig` to the recorded pid precisely so that the death of that
/// process is *heard* - as `kill_pid` failing - at the next handover. That is
/// what lets a dead compositor's screen be taken back on the spot (`reset_vc`,
/// and the switch falls through as if the console had been `VT_AUTO`) instead of
/// stranding it behind a timer nobody living can answer. A sender that reported
/// only success would leave the console with no way to learn its promise has no
/// keeper left, and a promise aimed at a *terminal* rather than at the process
/// that made it is worse still: it kills whichever session happens to be in the
/// foreground when the signal goes out.
pub fn send_signal_process(tgid: TaskId, sig: u8) -> bool {
    let mut delivered = false;
    for id in process_members(tgid) {
        delivered |= send_signal(id, sig);
    }
    delivered
}

/// `setpgid(pid, pgid)` — put a task into a process group, creating the group
/// when the caller names the task's own id.
///
/// Linux's rules, less the session half: the target has to exist, the group
/// has to exist or be the target itself, and the caller may only move itself
/// or one of its own children. The one that matters in practice is a shell
/// calling `setpgid(0, 0)` in a freshly forked child, which is what gives a
/// job a group of its own to be signalled through.
pub fn set_pgid(id: TaskId, pgid: TaskId) -> isize {
    with_table(|t| {
        let Some(target) = t.tasks.get(id.0).and_then(Option::as_ref) else {
            return crate::err::ESRCH;
        };
        let (parent, leader_of) = (target.parent, target.pgid);
        if pgid.0 >= MAX_TASKS {
            return crate::err::EPERM;
        }
        // A caller may move itself, or a child that has not yet exec'd - and
        // the second is the case a shell actually uses.
        if id != t.current && parent != Some(t.current) {
            return crate::err::ESRCH;
        }
        // A group leader cannot leave the group it leads: the group is named
        // after it, and there would be nothing left to name.
        if leader_of == id && id != pgid {
            return crate::err::EPERM;
        }
        // The group must already exist - either because this task is about to
        // become its leader, or because somebody is in it.
        let exists = pgid == id || t.tasks.iter().flatten().any(|task| task.pgid == pgid);
        if !exists {
            return crate::err::EPERM;
        }
        t.task_mut(id).pgid = pgid;
        0
    })
}

/// What `id` would do if `sig` arrived now. `None` when the task is gone.
pub fn signal_action(id: TaskId, sig: u8) -> Option<signal::Action> {
    with_table(|t| {
        t.tasks
            .get(id.0)?
            .as_ref()
            .map(|task| task.dispositions.get(sig))
    })
}

/// Record `sig`'s disposition for `id`. `false` when the task is gone or has
/// already exited - a disposition nobody will ever read is not worth storing.
pub fn set_signal_action(id: TaskId, sig: u8, action: signal::Action) -> bool {
    with_table(
        |t| match t.tasks.get_mut(id.0).and_then(|slot| slot.as_mut()) {
            Some(task) if !task.exited => {
                task.dispositions.set(sig, action);
                true
            }
            _ => false,
        },
    )
}

/// The alternate signal stack `id` installed, if any. `None` both for a
/// process that never asked for one and for a task that no longer exists -
/// neither has a stack to report.
pub fn signal_alt_stack(id: TaskId) -> Option<signal::AltStack> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().and_then(|task| task.alt_stack))
}

/// Install (`Some`) or clear (`None`) `id`'s alternate signal stack. `false`
/// when the task is gone, which the syscall layer reports as `ESRCH`.
pub fn set_signal_alt_stack(id: TaskId, stack: Option<signal::AltStack>) -> bool {
    with_table(
        |t| match t.tasks.get_mut(id.0).and_then(|slot| slot.as_mut()) {
            Some(task) if !task.exited => {
                task.alt_stack = stack;
                true
            }
            _ => false,
        },
    )
}

/// Take one pending signal off the running task, skipping the ones it has
/// blocked: a blocked signal stays pending until it is unblocked, which is the
/// whole reason `rt_sigprocmask` exists.
///
/// Signals whose disposition is `SIG_IGN` are dropped here rather than
/// reported, since there is nothing anyone could do with them.
pub fn take_deliverable_signal() -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = signal::take(&mut ready) {
            if task.dispositions.get(sig).is_ignore() {
                continue;
            }
            task.signals &= !signal::bit(sig);
            return Some(sig);
        }
        None
    })
}

/// As [`take_deliverable_signal`], but also handing back the `siginfo_t` the
/// sender attached, so the delivery path can build a handler's frame with the
/// real record - `si_code`, `si_pid`, the `sigqueue` payload - rather than a
/// fixed `SI_USER`.
pub fn take_deliverable_siginfo() -> Option<(u8, signal::SigInfo)> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = signal::take(&mut ready) {
            if task.dispositions.get(sig).is_ignore() {
                continue;
            }
            task.signals &= !signal::bit(sig);
            let slot = (sig as usize).min(signal::SIGNALS - 1);
            let info = task.siginfo[slot];
            task.siginfo[slot] = signal::SigInfo::ZERO;
            return Some((sig, info));
        }
        None
    })
}

/// Take one pending signal off the running task if it is in `mask`, whatever
/// its disposition says, and drop it from the pending set.
///
/// This is the `sigtimedwait` half of the signal machinery, and it is
/// deliberately different from [`take_deliverable_signal`]: a signal consumed
/// here never runs a handler and never takes its default action - not even for
/// `SIGKILL`, whose whole point is that it cannot be caught anywhere else. A
/// blocked signal in `mask` is exactly the one that can be waited for, which
/// is what makes `sigwait` work.
pub fn take_signal_in_set(mask: signal::Pending) -> Option<(u8, signal::SigInfo)> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & mask;
        match signal::take(&mut ready) {
            Some(sig) => {
                task.signals &= !signal::bit(sig);
                // The record comes out with the bit it belongs to: the
                // `siginfo_t` a waiter reads has to be the one the sender
                // left, not a fresh `SI_USER` one - musl's `SIGEV_THREAD`
                // helper checks `si_code == SI_TIMER` before running the
                // callback a POSIX timer exists to make.
                let slot = (sig as usize).min(signal::SIGINFO_SLOTS - 1);
                let record = task.siginfo[slot];
                task.siginfo[slot] = signal::SigInfo::ZERO;
                Some((sig, record))
            }
            None => None,
        }
    })
}

/// Take a pending signal that has to end the task, wherever it happens to be.
///
/// This is what a blocked task can act on by itself: the default action for
/// every signal this kernel names is to die of it. A signal with a handler is
/// left pending - it needs the register frame on the way out of a syscall to
/// run on, and a task sitting in a blocking call has not reached one yet.
pub fn take_fatal_signal() -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & !task.blocked;
        while let Some(sig) = signal::take(&mut ready) {
            // Read from a copy, so a signal that is skipped stays pending.
            if task.dispositions.get(sig).is_handler() {
                continue;
            }
            // Of the signals with no handler, only the ones whose default
            // action is to terminate end a task sitting in a blocking call.
            // `SIGSTOP` is not one of them - it stops, and stopping a task
            // that is already asleep changes nothing - and `SIGCONT` is not
            // one of them either. Before there was a stop state to be in,
            // "no handler" and "dies of it" really were the same thing.
            if signal::default_action(sig) != signal::DefaultAction::Terminate {
                continue;
            }
            task.signals &= !signal::bit(sig);
            return Some(sig);
        }
        None
    })
}

/// What the running task would do about `sig`.
pub fn current_disposition(sig: u8) -> signal::Action {
    with_table(|t| t.task(t.current).dispositions.get(sig))
}

/// Forget a disposition, putting it back to `SIG_DFL`. `SA_RESETHAND` asks
/// for exactly this the moment a handler is entered, which is what keeps a
/// one-shot handler re-entrant-safe.
pub fn reset_disposition(sig: u8) {
    with_table(|t| {
        let id = t.current;
        let mut actions = t.task(id).dispositions;
        actions.set(sig, signal::Action::default());
        t.task_mut(id).dispositions = actions;
    });
}

/// The signals the running task has blocked.
pub fn current_blocked() -> signal::Pending {
    with_table(|t| t.task(t.current).blocked)
}

/// The signals the running task has waiting: the pending set as
/// `rt_sigpending(2)` reads it - a mask of what has arrived, not of what
/// is blocked. The two sets are independent: a signal can be both pending
/// and blocked, and this answer names it either way.
pub fn current_pending() -> signal::Pending {
    with_table(|t| t.task(t.current).signals)
}

/// Replace the running task's blocked set. `rt_sigprocmask` and the return
/// from a handler are its two callers.
pub fn set_current_blocked(mask: signal::Pending) {
    with_table(|t| {
        let id = t.current;
        t.task_mut(id).blocked = mask;
    });
}

// ---------------------------------------------------------------------------
// ptrace: the tracer link, the stops, and what a wait collects
// ---------------------------------------------------------------------------
//
// A tracer and its tracee are two tasks in a protocol, and the protocol has
// exactly two directions:
//
//   * the tracee stops - at a syscall boundary, on a signal, on `execve` - and
//     parks itself until something resumes it;
//   * the tracer sits in `wait(2)` until one of its children has something to
//     report, reads the stop, and resumes it.
//
// Both directions are ordinary task-table operations. The tracee parks in the
// same sleep queue every other blocking call uses, with a deadline of "never"
// and a `stop` record beside it; the tracer's `wait` blocks on the same
// condition mechanism, with the stop record as its condition. There is no new
// synchronisation primitive here, and that is the point: the whole of what makes
// this safe is the rule the rest of the module already follows - publish the
// state and park the waiter in one critical section, so a waiter that has not
// looked yet finds it on its next look.

/// Whether `id` is being traced by anyone.
pub fn has_tracer(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.tracer.is_some())
    })
}

/// Whether `id` is stopped: present, alive, and sitting in a stop.
pub fn is_stopped(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.stop.is_some())
    })
}

/// Bind `id` to `tracer`. Returns false when the task is gone or has exited,
/// which the syscall layer reports as `ESRCH`.
pub fn set_tracer(id: TaskId, tracer: TaskId) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.tracer = Some(tracer);
            true
        }
        _ => false,
    })
}

/// The task tracing `id`, if any.
pub fn tracer_of(id: TaskId) -> Option<TaskId> {
    with_table(|t| t.tasks.get(id.0)?.as_ref()?.trace.tracer)
}

/// The `PTRACE_O_*` word `id`'s tracer set.
pub fn trace_options(id: TaskId) -> usize {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .map_or(0, |task| task.trace.options)
    })
}

/// Record the options a tracer asked for. False when the task is gone.
pub fn set_trace_options(id: TaskId, options: usize) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.options = options;
            true
        }
        _ => false,
    })
}

/// The register file `id` will resume with, if it is stopped.
///
/// A task that has never been out of the kernel has no snapshot to hand back:
/// a snapshot is what [`park`] takes, and nothing has parked it yet. What it has
/// instead is `user_resume` - the state the one-way door into ring 3 will read -
/// and that *is* its register file, not a description of one. The two cases are
/// told apart by `regs` being empty, which for a stopped task happens exactly
/// when nothing has parked it; [`attach_new_child`] is what creates such a task.
pub fn trace_regs(id: TaskId) -> Option<[u64; abi::uregs::COUNT]> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        task.trace.regs.or_else(|| {
            // Only a *stopped* task that never ran: a running task's registers
            // are nobody's to report, which is why the stop is part of the
            // question rather than implied by it.
            let stopped = task.trace.stop.and(task.user_resume.as_ref())?;
            Some(context_regs(stopped, task.tls))
        })
    })
}

/// The register file a task that has never been out of the kernel would resume
/// with, in the order `PTRACE_GETREGS` reports it.
///
/// Three of the twenty-seven slots have no counterpart in [`UserContext`], and
/// each for a reason: no syscall is in flight, so `orig_rax` is nothing; and
/// `rcx` and `r11` are the two registers the `syscall` instruction itself
/// clobbers, which is why the structure does not carry them - a program cannot
/// be relying on them across a call, so there is nothing to carry. The segment
/// registers the structure does not name are left zero, which is the value the
/// machine reads them as. `fs_base` is not in the structure either because it
/// is per *task*: the task table holds it, and it is handed in here.
fn context_regs(state: &UserContext, tls: usize) -> [u64; abi::uregs::COUNT] {
    use abi::uregs::*;

    let mut regs = [0u64; COUNT];
    regs[R15] = state.r15 as u64;
    regs[R14] = state.r14 as u64;
    regs[R13] = state.r13 as u64;
    regs[R12] = state.r12 as u64;
    regs[RBP] = state.rbp as u64;
    regs[RBX] = state.rbx as u64;
    regs[R10] = state.r10 as u64;
    regs[R9] = state.r9 as u64;
    regs[R8] = state.r8 as u64;
    regs[RAX] = state.rax as u64;
    regs[RDX] = state.rdx as u64;
    regs[RSI] = state.rsi as u64;
    regs[RDI] = state.rdi as u64;
    regs[RIP] = state.rip as u64;
    regs[CS] = state.cs as u64;
    regs[EFLAGS] = state.rflags as u64;
    regs[RSP] = state.rsp as u64;
    regs[SS] = state.ss as u64;
    regs[FS_BASE] = tls as u64;
    regs
}

/// The other direction: turn a register file a tracer may have rewritten back
/// into the state the one-way door into ring 3 reads.
fn context_from_regs(regs: &[u64; abi::uregs::COUNT]) -> UserContext {
    use abi::uregs::*;

    UserContext {
        rip: regs[RIP] as usize,
        rsp: regs[RSP] as usize,
        rflags: regs[EFLAGS] as usize,
        rax: regs[RAX] as usize,
        rbx: regs[RBX] as usize,
        rbp: regs[RBP] as usize,
        cs: regs[CS] as usize,
        ss: regs[SS] as usize,
        r12: regs[R12] as usize,
        r13: regs[R13] as usize,
        r14: regs[R14] as usize,
        r15: regs[R15] as usize,
        rdi: regs[RDI] as usize,
        rsi: regs[RSI] as usize,
        rdx: regs[RDX] as usize,
        r8: regs[R8] as usize,
        r9: regs[R9] as usize,
        r10: regs[R10] as usize,
    }
}

/// Replace the register file `id` will resume with. False when the task is
/// gone, or is not stopped - a running task's registers are not the kernel's
/// to rewrite from another task, and `PTRACE_SETREGS` on a running tracee is
/// `ESRCH` on Linux too.
pub fn set_trace_regs(id: TaskId, regs: &[u64; abi::uregs::COUNT]) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited && task.trace.stop.is_some() => {
            match (task.trace.regs, task.user_resume.as_mut()) {
                // A task stopped before its first instruction has no snapshot
                // to replace: what a tracer edits here *is* the state the door
                // will read, so the edit has to land there or nowhere. The rule
                // is the same one `trace_regs` reads by, seen from the writing
                // side.
                (None, Some(state)) => *state = context_from_regs(regs),
                _ => task.trace.regs = Some(*regs),
            }
            true
        }
        _ => false,
    })
}

/// The floating-point registers `id` is stopped with, as the 512-byte image
/// `PTRACE_GETFPREGS` and `PTRACE_SETFPREGS` move.
///
/// A stopped task's saved block is current: every switch out of a task writes
/// its live registers there, and a task that is stopped has been switched out at
/// least once since it last ran.
pub fn trace_fpu(id: TaskId) -> Option<[u8; crate::arch::context::FPU_BYTES]> {
    with_table(|t| {
        t.sync_fpu(id);
        t.tasks
            .get(id.0)?
            .as_ref()
            .map(|task| task.context.fpu.to_bytes())
    })
}

/// Replace the floating-point registers `id` will resume with. The same rule as
/// [`set_trace_regs`]: only a stopped task's state is the kernel's to rewrite.
pub fn set_trace_fpu(id: TaskId, bytes: &[u8; crate::arch::context::FPU_BYTES]) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited && task.trace.stop.is_some() => {
            task.context.fpu = crate::arch::context::FpuState::from_bytes(bytes);
            true
        }
        _ => false,
    })
}

/// What a `PTRACE_GET_SYSCALL_INFO` reports about the syscall `id` is stopped
/// at, as `(entry, nr, args, ip, sp)`.
pub fn trace_syscall(id: TaskId) -> Option<(bool, u64, [u64; 6], u64, u64)> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        let stop = task.trace.stop?;
        // A stop at a syscall boundary describes itself. Any *other* stop - a
        // signal arriving, an exec - is described by the call the tracee was in
        // the middle of, which is the one it last entered: Linux reads that
        // answer out of the registers, and the entry record is the same answer
        // kept somewhere a stopped task can still be asked.
        let entry_stop = stop.syscall.map(|syscall| syscall.entry);
        let syscall = stop.syscall.or(task.trace.syscall)?;
        Some((
            entry_stop.unwrap_or(true),
            syscall.nr,
            syscall.args,
            syscall.ip,
            syscall.sp,
        ))
    })
}

/// The stop `id` is sitting in, without collecting it: what a `wait` looks at
/// while it decides whether there is anything to report.
pub fn stop_of(id: TaskId) -> Option<Stop> {
    with_table(|t| t.tasks.get(id.0)?.as_ref()?.trace.stop)
}

/// Collect `id`'s stop: mark it reported and hand it back. This is the moment
/// the news stops being news, and it is deliberately separate from looking.
pub fn take_stop_report(id: TaskId) -> Option<Stop> {
    with_table(|t| {
        let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
        let stop = task.trace.stop.as_mut()?;
        if stop.reported {
            return None;
        }
        stop.reported = true;
        Some(*stop)
    })
}

/// Take `id`'s `WIFCONTINUED` flag, if a `SIGCONT` set one and no `wait` has
/// reported it.
pub fn take_continued_report(id: TaskId) -> bool {
    with_table(|t| {
        let Some(task) = t.tasks.get_mut(id.0).and_then(Option::as_mut) else {
            return false;
        };
        let flag = task.trace.continued;
        task.trace.continued = false;
        flag
    })
}

/// Whether a `SIGCONT` has ended a stop of `id` and nobody has been told.
pub fn continued_report_pending(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| task.trace.continued)
    })
}

/// The oldest unreported stop among `parent`'s children, if one is reportable.
///
/// `requested` is `WUNTRACED`, and it is not the whole condition: a traced
/// child's stop is reported to its *tracer* whether or not the tracer asked for
/// stops, because the tracer is the only thing that can end it and a tracer
/// that could not hear about one would deadlock on its first `wait`. That is
/// what `stop.traced` says, and it is why `strace`'s very first `waitpid` - with
/// no option bits at all - still returns.
fn unreported_stop(t: &Table, parent: TaskId, requested: bool) -> Option<WaitEvent> {
    for (slot, task) in t.tasks.iter().enumerate() {
        let Some(task) = task.as_ref() else { continue };
        let id = TaskId(slot);
        if !waitable(task, parent) || id == parent {
            continue;
        }
        let Some(stop) = task.trace.stop else {
            continue;
        };
        if stop.reported {
            continue;
        }
        // Who may hear this stop. A ptrace stop belongs to the *tracer* - the
        // real parent waiting without `WUNTRACED` must never see one, and not
        // with `WUNTRACED` either: Linux keeps ptrace-stops out of the real
        // parent's `wait` entirely, because the parent learning "child
        // stopped" while the tracer owns the corpse leaves the parent acting
        // on a state it can do nothing about. `strace -f timeout ...` hung a
        // whole class of runners exactly here, when a born-stopped fork child
        // was reported to a parent that was only collecting exits. A plain
        // group stop (a `SIGSTOP` of job control) is the opposite: it is the
        // parent's news, and only with `WUNTRACED`.
        if stop.traced {
            if task.trace.tracer != Some(parent) {
                continue;
            }
        } else if !requested {
            continue;
        }
        // The scan is ascending, so the first match is the lowest id: two
        // children stopping in the same tick are reported in a fixed order
        // rather than whichever order the table happened to hand out.
        return Some(WaitEvent::Stopped {
            id,
            signal: stop.signal,
            event: stop.event,
        });
    }
    None
}

/// Whether `id` is one `me` may `wait` for.
///
/// Two things make a task waitable, and Linux treats them the same way: being
/// `me`'s child, and being traced by `me`. The second is not a convenience -
/// `PTRACE_ATTACH` makes a tracer responsible for a task it did not spawn, and
/// the only way that task's stops and death can be collected is by waiting for
/// them. Having one rule means the six places that scan for "my children" do
/// not each grow a second predicate, and cannot disagree with each other.
fn waitable(task: &Task, me: TaskId) -> bool {
    task.parent == Some(me) || task.trace.tracer == Some(me)
}

/// Whether `id` is a child of `parent` - the check that keeps a `wait` from
/// blocking on someone else's child.
pub fn is_child(id: TaskId, parent: TaskId) -> bool {
    with_table(|t| {
        t.tasks[id.0]
            .as_ref()
            .is_some_and(|task| !task.exited && waitable(task, parent))
    })
}

/// The lowest unblocked, non-ignored pending signal of `task` - the set
/// [`take_deliverable_signal`] draws from, without taking anything.
///
/// This is what lets the delivery path decide whether there is a signal to
/// report to a tracer before it commits to consuming one.
fn deliverable(task: &Task) -> Option<u8> {
    let mut ready = task.signals & !task.blocked;
    while let Some(sig) = signal::take(&mut ready) {
        if task.dispositions.get(sig).is_ignore() {
            continue;
        }
        return Some(sig);
    }
    None
}

/// Park the running task in `stop` and return only when something resumes it.
///
/// The register file goes in and comes back out: it is stored where
/// `PTRACE_GETREGS` can read it and `PTRACE_SETREGS` can change it, and the
/// caller gets whatever the tracer left there. That is the whole mechanism
/// behind "stop the program, change its registers, let it go".
///
/// Both the tracer and the parent are woken rather than one being chosen
/// between. For a tracer's stop only the tracer can do anything about it, and
/// for a group stop only the parent - but the two are one test apart, and a
/// wake delivered to a task that is not waiting is a no-op.
fn park(regs: &mut [u64; abi::uregs::COUNT], stop: Stop) {
    let id = current();
    let snapshot = *regs;
    schedule(|t| {
        let (tracer, parent) = {
            let task = t.task_mut(id);
            task.trace.stop = Some(stop);
            task.trace.regs = Some(snapshot);
            (task.trace.tracer, task.parent)
        };
        if let Some(tracer) = tracer {
            t.sched.wake(tracer);
        }
        if let Some(parent) = parent {
            t.sched.wake(parent);
        }
        let priority = t.task(id).priority;
        // `u64::MAX` is "until someone wakes me". A stop has no clock on it:
        // a tracee that gave up waiting would break the protocol, and a group
        // stop is ended by `SIGCONT` and nothing else.
        t.sched.sleep(id, priority, u64::MAX);
    });
    *regs = with_table(|t| t.task(id).trace.regs).unwrap_or(snapshot);
}

/// The stop a successful `execve` leaves a traced task in.
///
/// There are two shapes, and which one applies is the tracer's own choice via
/// `PTRACE_O_TRACEEXEC`:
///
/// * With the option set, the call is interrupted mid-flight by an event stop
///   carrying `PTRACE_EVENT_EXEC`, and *the call's exit stop still follows it*.
///   That ordering is Linux's, and it is worth spelling out because it is
///   counter-intuitive - the ptrace(2) man page walks through it:
///   `we get PTRACE_EVENT_EXEC ... we issue PTRACE_SYSCALL ... we get
///   syscall-exit-stop ... PID0 <... execve resumed> ) = 0`. A tracer that
///   treats the event as the call's completion finds its enter/exit bookkeeping
///   one stop ahead from then on, and every later return value read off the
///   following call - the shape `strace`'s output had before this was right.
/// * Without it, the trap is the completion notice: the exit hop that follows
///   has nothing left to say, which `at_exec_exit` records for
///   [`stop_at_syscall`] to skip. The trap is a plain `SIGTRAP` - the classic
///   exec trap every legacy tracer is written against.
///
/// In both shapes the syscall record is left as the *entry* one: the record is
/// only ever written at syscall stops, so at this stop it still describes the
/// call in flight, which is what `PTRACE_GET_SYSCALL_INFO` should say here too.
pub fn exec_trap(regs: &mut [u64; abi::uregs::COUNT]) {
    let id = current();
    let options = with_table(|t| t.task(id).trace.options);
    if options & abi::PTRACE_O_TRACEEXEC != 0 {
        let entry = with_table(|t| t.task(id).trace.syscall);
        park(
            regs,
            Stop {
                signal: signal::SIGTRAP,
                event: abi::PTRACE_EVENT_EXEC,
                traced: true,
                reported: false,
                // The former tid: an exec does not change the task's number
                // here, so it is the same one - but it is the *question* a
                // tracer asks with `PTRACE_GETEVENTMSG` after an exec, and the
                // answer has to come from the stop rather than from whoever
                // happens to be reading it.
                message: id.0 as u64,
                delivering: None,
                syscall: entry,
                group: false,
            },
        );
        return;
    }
    with_table(|t| t.task_mut(id).trace.at_exec_exit = true);
    let entry = with_table(|t| t.task(id).trace.syscall);
    park(
        regs,
        Stop {
            signal: signal::SIGTRAP,
            event: abi::PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: None,
            syscall: entry.map(|call| SyscallStop {
                entry: false,
                ..call
            }),
            group: false,
        },
    );
}

/// Which `PTRACE_EVENT_*` a child's creation is, read off the `clone` flags
/// that asked for it.
///
/// The three-way split is Linux's, and it is not a detail a tracer can recover
/// on its own: `fork`, `vfork` and a thread-creating `clone` all arrive at the
/// kernel as one syscall with different flags, and a tracer that asked about
/// threads but not about processes needs to be told which one it just got.
fn fork_event(flags: usize) -> u32 {
    if flags & nr::CLONE_THREAD != 0 {
        abi::PTRACE_EVENT_CLONE
    } else if flags & nr::CLONE_VFORK != 0 {
        abi::PTRACE_EVENT_VFORK
    } else {
        abi::PTRACE_EVENT_FORK
    }
}

/// Hand a brand-new child to its creator's tracer, if the tracer asked about
/// this kind of child. True when it did.
///
/// Two things have to happen together, and Linux does both inside its own fork
/// path. The child is **attached to the same tracer and born stopped**, so that
/// nothing it does can happen before the tracer has looked at it; and the
/// creator is left **owing its tracer an event stop**, which the syscall layer
/// raises a few lines later, where the frame is.
///
/// Born stopped is what makes `strace -f` work at all. A thread that runs even
/// once before its tracer's first look has already made a syscall the trace
/// will not contain, and a tracer left to guess where the thread was is worse
/// off than one that is simply told.
fn attach_new_child(t: &mut Table, parent: TaskId, child: TaskId, event: u32) -> bool {
    let bit = match event {
        abi::PTRACE_EVENT_FORK => abi::PTRACE_O_TRACEFORK,
        abi::PTRACE_EVENT_VFORK => abi::PTRACE_O_TRACEVFORK,
        abi::PTRACE_EVENT_CLONE => abi::PTRACE_O_TRACECLONE,
        _ => return false,
    };
    let (tracer, options) = {
        let parent = t.task(parent);
        (parent.trace.tracer, parent.trace.options)
    };
    // No tracer, or one that did not ask about this kind of child: the child is
    // nobody's business but its parent's, and it starts running.
    if tracer.is_none() || options & bit == 0 {
        return false;
    }

    let child_task = t.task_mut(child);
    child_task.trace.tracer = tracer;
    // The options come with the child. Linux copies them, and it matters: a
    // tracer sets its options once, on the process it started, and every thread
    // that process creates afterwards is traced with those same options without
    // the tracer saying so again.
    child_task.trace.options = options;
    child_task.trace.stop = Some(Stop {
        // The `SIGSTOP` of being newly attached - the same stop `PTRACE_ATTACH`
        // gives, and the one a tracer is written to swallow. Linux reports
        // `PTRACE_EVENT_STOP` instead when the tracer attached with
        // `PTRACE_SEIZE`; this kernel has no `SEIZE`, so it is the older shape.
        signal: signal::SIGSTOP,
        event: abi::PTRACE_EVENT_NONE,
        traced: true,
        reported: false,
        message: 0,
        // Nothing is pending delivery: the child was not sent a signal, it was
        // born here. A tracer that resumes it *with* `SIGSTOP` gets one
        // delivered, which is what Linux does with that too.
        delivering: None,
        syscall: None,
        // A stop signal's stop, so a `SIGCONT` can end it.
        group: true,
    });
    // It has no saved register file, because it has never been out of the
    // kernel: the registers it will resume with are the ones it was built with,
    // and `trace_regs` reads those out of `user_resume`.
    child_task.trace.regs = None;

    t.task_mut(parent).trace.pending_event = Some((event, child.0 as u64));
    true
}

/// The event stop a task owes its tracer for a child it has just made, taken
/// once. See `Tracing::pending_event`.
pub fn take_pending_event(id: TaskId) -> Option<(u32, u64)> {
    with_table(|t| {
        let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
        task.trace.pending_event.take()
    })
}

/// Hold the running task where it is so its tracer can be told that it made a
/// child, and which child.
///
/// Called from inside the `fork`/`clone`/`vfork` call rather than after it, for
/// the same reason [`exec_trap`] is called inside `execve`: the stop describes
/// a moment in the middle of a syscall, and the call's own exit stop still
/// follows it. `message` is the child's pid, which is what
/// `PTRACE_GETEVENTMSG` answers at this stop.
pub fn fork_trap(regs: &mut [u64; abi::uregs::COUNT], event: u32, message: u64) {
    // The syscall record is deliberately left alone: it still describes the
    // call in flight, and a tracer that asks what this stop is a boundary of
    // should hear "the clone that has not returned yet".
    let entry = with_table(|t| t.task(current()).trace.syscall);
    park(
        regs,
        Stop {
            signal: signal::SIGTRAP,
            event,
            traced: true,
            reported: false,
            message,
            delivering: None,
            syscall: entry,
            group: false,
        },
    );
}

/// Stop the running task at a syscall boundary, if its tracer asked for one.
///
/// Called twice per syscall - once before the handler runs and once after -
/// because that is the pair of moments a tracer wants to see. The `armed` flag
/// is cleared here rather than at the resume: it is the stop that consumes the
/// tracer's request, and the tracer re-arms it, which is what makes an entry
/// stop and an exit stop two events rather than one repeated.
pub fn stop_at_syscall(
    entry: bool,
    regs: &mut [u64; abi::uregs::COUNT],
    nr: u64,
    args: &[u64; 6],
    ip: u64,
    sp: u64,
) {
    let id = current();
    let (armed, options, exec_exit) = with_table(|t| {
        let trace = &t.task(id).trace;
        (
            trace.armed && trace.tracer.is_some(),
            trace.options,
            trace.at_exec_exit,
        )
    });
    if !armed {
        return;
    }
    if !entry && exec_exit {
        // Legacy tracer (no `PTRACE_O_TRACEEXEC`): the exec trap *was* the
        // call's completion notice. See `exec_trap`. With the option set this
        // flag is never raised, and the exit stop follows the event stop the
        // way ptrace(2) describes.
        with_table(|t| t.task_mut(id).trace.at_exec_exit = false);
        return;
    }
    let stop_syscall = with_table(|t| {
        let task = t.task_mut(id);
        // The entry's arguments are remembered across the pair of stops, so the
        // exit half can describe the call as well as the entry half can.
        if entry {
            task.trace.at_exec_exit = false;
            task.trace.syscall = Some(SyscallStop {
                nr,
                args: *args,
                ip,
                sp,
                entry: true,
            });
            task.trace.syscall
        } else {
            let previous = task.trace.syscall.filter(|syscall| syscall.entry);
            task.trace.syscall = Some(SyscallStop {
                nr: previous.map_or(nr, |syscall| syscall.nr),
                args: previous.map_or(*args, |syscall| syscall.args),
                ip,
                sp,
                entry: false,
            });
            task.trace.syscall
        }
    });
    // `TRACESYSGOOD` is the difference between a stop a tracer can recognise
    // and one it has to guess at, so it is applied where the signal is chosen
    // rather than stored anywhere.
    let signal = signal::SIGTRAP
        | if options & abi::PTRACE_O_TRACESYSGOOD != 0 {
            abi::SYSCALL_TRAP
        } else {
            0
        };
    park(
        regs,
        Stop {
            signal,
            event: abi::PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: None,
            syscall: stop_syscall,
            group: false,
        },
    );
}

/// Report the running task's pending signal to its tracer before it is
/// delivered, and hold the task there until the tracer answers.
///
/// The signal stays pending across the stop. The tracer's resume replaces it -
/// with the signal it names, or with nothing at all - and what is left behind is
/// what the delivery path a few lines further on finds. A tracer that names
/// nothing therefore drops the signal, which is the point: that is how a
/// debugger "swallows" a signal, and how `strace` reports one without letting it
/// through twice.
pub fn trace_signal_stop(regs: &mut [u64; abi::uregs::COUNT]) {
    let id = current();
    let traced = with_table(|t| t.task(id).trace.tracer.is_some());
    if !traced {
        return;
    }
    let Some(sig) = with_table(|t| deliverable(t.task(id))) else {
        return;
    };
    // Not a syscall stop, and it must not be reported as one: a tracer that
    // asked about the syscall will find nothing and say so, rather than
    // reading the previous call's arguments back and calling them this stop's.
    with_table(|t| t.task_mut(id).trace.syscall = None);
    park(
        regs,
        Stop {
            signal: sig,
            event: abi::PTRACE_EVENT_NONE,
            traced: true,
            reported: false,
            message: 0,
            delivering: Some(sig),
            syscall: None,
            // A stop reported *for a stop signal* is the tracee's own stop, and
            // a `SIGCONT` ends it. Linux gets there in two steps - the tracer
            // passes the signal on and the task then enters a group stop - and
            // this kernel reports the signal-delivery stop and the group stop as
            // one event. The observable consequence is the one that matters:
            // `kill -CONT` must be able to get a task out of a `SIGSTOP`, and
            // anything that stopped it that way has only that way back.
            group: signal::stops_by_default(sig),
        },
    );
}

/// Stop the running task on a stop signal with nobody tracing it, and hold it
/// there until a `SIGCONT` ends the stop.
///
/// This is the non-ptrace half of "stopped": `kill -STOP`, and Ctrl-Z once
/// there is a keyboard. The parent is told with `WUNTRACED`, which is the only
/// reason `wait` has the flag at all.
pub fn group_stop(regs: &mut [u64; abi::uregs::COUNT], sig: u8) {
    let id = current();
    // A traced task's group stop is its tracer's business as well as its
    // parent's. Linux reports it to the tracer, and it has to: a group stop
    // nobody is told about is a task nothing will ever resume, because the only
    // other way out is a `SIGCONT` and the tracee is inside a tracer that has no
    // shell to send one.
    let traced = with_table(|t| t.task(id).trace.tracer.is_some());
    park(
        regs,
        Stop {
            signal: sig,
            event: abi::PTRACE_EVENT_NONE,
            traced,
            reported: false,
            message: 0,
            delivering: None,
            syscall: None,
            group: true,
        },
    );
}

/// Let a stopped task go on: `PTRACE_CONT`, `PTRACE_SYSCALL`, `PTRACE_DETACH`
/// and the scheduler all end up here.
///
/// `signal` is what the tracer wants delivered, zero for nothing - and that
/// zero is not a no-op. A stop entered to report a signal replaces that signal
/// with whatever the tracer names, so naming nothing is how a tracer drops it.
/// `arm` re-arms the syscall trace.
///
/// Deliberately permissive about a task that is not stopped: a tracer may arm
/// the trace or set options before its tracee has reached its first syscall
/// boundary, and answering `ESRCH` for that would be technically right and
/// useless. A task that is running simply gets the arm, and stops at the next
/// boundary as asked.
pub fn resume_traced(id: TaskId, signal: u8, arm: bool) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            if let Some(reported) = task.trace.stop.and_then(|stop| stop.delivering) {
                task.signals &= !signal::bit(reported);
            }
            task.trace.stop = None;
            task.trace.armed = arm;
            if signal != 0 {
                queue_signal(task, signal, signal::SigInfo::user(signal, 0, 0));
            }
            t.sched.wake(id);
            true
        }
        _ => false,
    })
}

/// Break the tracer link: `PTRACE_DETACH`, and the tracer's own death.
///
/// Whether the tracee comes back with this is not one answer but three, and they
/// are told apart by whose stop it is - see the body. What is always true is
/// that the *trace* is over: the link, the arming and the signal the attach
/// injected all go, because a tracee that kept them would be waiting on a tracer
/// that has gone.
pub fn detach_traced(id: TaskId, signal: u8) -> bool {
    with_table(|t| match t.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if !task.exited => {
            task.trace.tracer = None;
            task.trace.armed = false;
            // Measured against Linux, a detach has three cases:
            //
            // * `attach` then `detach(0)` without ever `wait`ing, and the target
            //   runs on. The `SIGSTOP` the attach injected is still merely
            //   pending here, and dropping it is the whole difference: kept, the
            //   target stops itself on the way out of whatever syscall it is in,
            //   with no tracer left and nothing on the machine but a `SIGCONT`
            //   that could end that stop.
            // * The same, after the stop has been reported. That stop is the one
            //   parked with `delivering` set, so ending it is the first case one
            //   step later.
            // * A process the user stopped *before* the attach stays stopped
            //   after the detach, and stays stopped even when the detach names
            //   `SIGCONT` - the signal a detach injects goes to the tracee, not
            //   to its stop. That stop is the task's own, and only a `SIGCONT`
            //   somebody actually sends ends it.
            if task
                .trace
                .stop
                .is_some_and(|stop| stop.delivering.is_some())
            {
                task.trace.stop = None;
            }
            for stop_sig in [
                signal::SIGSTOP,
                signal::SIGTSTP,
                signal::SIGTTIN,
                signal::SIGTTOU,
            ] {
                task.signals &= !signal::bit(stop_sig);
            }
            if signal != 0 {
                queue_signal(task, signal, signal::SigInfo::user(signal, 0, 0));
            }
            // The register file stays where the tracer left it: Linux writes a
            // `PTRACE_SETREGS` straight into the task's own frame, so edits made
            // before the detach belong to the task from then on. `trace.regs` is
            // only read back by the park this task is leaving, and the next stop
            // overwrites it with a fresh snapshot.
            //
            // Waking follows the same three-way split, and for the third case it
            // is the thing to *not* do: a stopped task is held asleep by `park`,
            // so a wake here would release the very stop the detach was told to
            // leave alone.
            if task.trace.stop.is_none() {
                t.sched.wake(id);
            }
            true
        }
        _ => false,
    })
}

/// What happened to one child, as `wait(2)` reports it.
#[derive(Clone, Copy)]
pub enum WaitEvent {
    /// A death to collect.
    Exited { id: TaskId, code: isize },
    /// A stop to report: the signal `WSTOPSIG` names, and the `PTRACE_EVENT_*`
    /// the status word's high byte carries.
    Stopped { id: TaskId, signal: u8, event: u32 },
    /// A `SIGCONT` ended a stop. Only ever reported to a caller that asked
    /// with `WCONTINUED`.
    Continued { id: TaskId },
}

/// Is there something to report about one of `parent`'s children, given what
/// the caller asked to hear about? `t` is borrowed rather than taken because
/// this is also the blocking condition, which must not change anything.
///
/// The order is stops, then deaths, then resumes. A death comes last of the
/// three because it cannot be reported at all until a stop that precedes it has
/// been; a resume comes last because it is the least interesting thing that can
/// have happened, and a tracer that is told about one when a stop was waiting
/// has lost the stop.
fn any_child_event(t: &Table, parent: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    let children = || {
        t.tasks.iter().enumerate().filter_map(|(slot, task)| {
            let task = task.as_ref()?;
            // A slot this task's own id names is not its child: `any_child_code`
            // makes the same exclusion for the same reason.
            (waitable(task, parent) && TaskId(slot) != parent).then_some((TaskId(slot), task))
        })
    };
    if let Some(stopped) = unreported_stop(t, parent, stops) {
        return Some(stopped);
    }
    if continues {
        for (id, task) in children() {
            if task.trace.continued {
                return Some(WaitEvent::Continued { id });
            }
        }
    }
    // Deaths last, and read out of the retired list exactly as the exit-only
    // `wait` reads them - the same records, so a caller that asks about stops
    // cannot lose an exit code by asking. A record still owned by a tracer is
    // the tracer's alone to see.
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| {
                let id = retired.id;
                let free = t.tasks[id.0].is_none();
                let ours = free
                    || t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| waitable(task, parent));
                ours && id != parent && retired.tracer.is_none_or(|tracer| tracer == parent)
            })
            .map(|retired| WaitEvent::Exited {
                id: retired.id,
                code: retired.code,
            })
    })
}

/// Turn a look into a collection: mark the stop reported, take the `SIGCONT`
/// flag, or consume the exit code. Everything this does is a change, which is
/// why it is a second pass over the table rather than part of the look.
fn take_event(t: &mut Table, event: WaitEvent) -> Option<WaitEvent> {
    match event {
        // The code in the event is a look, not a claim: the value that counts
        // is the one taken here, because between the look and the take another
        // waiter could have collected it.
        WaitEvent::Exited { id, .. } => {
            take_exit_code_for(t, id, t.current).map(|code| WaitEvent::Exited { id, code })
        }
        WaitEvent::Stopped { id, signal, event } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            let stop = task.trace.stop.as_mut()?;
            if stop.reported {
                return None;
            }
            stop.reported = true;
            Some(WaitEvent::Stopped { id, signal, event })
        }
        WaitEvent::Continued { id } => {
            let task = t.tasks.get_mut(id.0).and_then(Option::as_mut)?;
            if !task.trace.continued {
                return None;
            }
            task.trace.continued = false;
            Some(WaitEvent::Continued { id })
        }
    }
}

/// Block until a child of `parent` has something to report, and collect it.
/// The exit-only shape is `wait_any_child`, which this generalises.
pub fn wait_any_child_event(parent: TaskId, stops: bool, continues: bool) -> WaitPark<WaitEvent> {
    if matches!(
        block_until_on_interruptible(|t| any_child_event(t, parent, stops, continues).is_some()),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    match with_table(|t| {
        let event = any_child_event(t, parent, stops, continues)?;
        take_event(t, event)
    }) {
        Some(event) => WaitPark::Event(event),
        None => WaitPark::NoChild,
    }
}

/// The non-blocking half: is there anything to collect right now?
pub fn poll_any_child_event(parent: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    with_table(|t| {
        let event = any_child_event(t, parent, stops, continues)?;
        take_event(t, event)
    })
}

/// The same question for one named child, which is what `waitpid` asks. The
/// asker is the running task; `child_event` says what *it* may hear.
pub fn child_event(id: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    with_table(|t| child_event_in(t, id, t.current, stops, continues))
}

/// [`child_event`] against a table the caller already holds - the shape the
/// blocking condition needs, since it runs inside `schedule`'s borrow.
fn child_event_in(
    t: &Table,
    id: TaskId,
    me: TaskId,
    stops: bool,
    continues: bool,
) -> Option<WaitEvent> {
    let task = t.tasks.get(id.0).and_then(Option::as_ref)?;
    if let Some(stop) = task.trace.stop
        && !stop.reported
        // Who may hear this stop - the same rule `unreported_stop` applies to
        // the wildcard wait: a ptrace stop is the tracer's, a group stop is
        // the parent's when `WUNTRACED` asked for it. Without the tracer check
        // a parent's plain `waitpid(pid)` collects a born-stop its tracer has
        // not even looked at yet.
        && (if stop.traced {
            task.trace.tracer == Some(me)
        } else {
            stops
        })
    {
        return Some(WaitEvent::Stopped {
            id,
            signal: stop.signal,
            event: stop.event,
        });
    }
    if task.trace.continued && continues {
        return Some(WaitEvent::Continued { id });
    }
    None
}

/// Collect `id`'s stop or resume, if there is one waiting.
pub fn take_child_event(id: TaskId, stops: bool, continues: bool) -> Option<WaitEvent> {
    let event = child_event(id, stops, continues)?;
    with_table(|t| take_event(t, event))
}

/// Whether `id` has anything a `wait` with these flags could report.
pub fn child_event_pending(id: TaskId, stops: bool, continues: bool) -> bool {
    child_event(id, stops, continues).is_some()
}

/// Block the running task until `id` has something to report, then collect it.
///
/// The stop-aware relative of [`block_until_code`]. The condition is read
/// through the borrow `block_until_on` already holds for the same reason it is
/// there: a closure taking its own critical section inside `schedule`'s would
/// nest one inside another and hand interrupts back early.
pub fn block_until_child_event(id: TaskId, stops: bool, continues: bool) -> WaitPark<WaitEvent> {
    if matches!(
        block_until_on_interruptible(|t| {
            exit_code(id).is_some() || child_event_in(t, id, t.current, stops, continues).is_some()
        }),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    if let Some(event) = take_child_event(id, stops, continues) {
        return WaitPark::Event(event);
    }
    match take_exit_code_current(id) {
        Some(code) => WaitPark::Event(WaitEvent::Exited { id, code }),
        None => WaitPark::NoChild,
    }
}

/// [`take_exit_code_for`] for the running task - the shape a `waitpid(pid)`
/// call lands in, where the collector is the caller itself.
pub fn take_exit_code_current(id: TaskId) -> Option<isize> {
    with_table(|t| take_exit_code_for(t, id, t.current))
}

/// The paging root `id` runs on: its own address space's, or its thread group's
/// for a thread. `None` for a kernel task, which has no user memory to reach.
pub fn space_root_of(id: TaskId) -> Option<usize> {
    with_table(|t| {
        let task = t.tasks.get(id.0)?.as_ref()?;
        match &task.space {
            Some(space) => Some(space.root()),
            None if task.mm.root() != 0 => Some(task.mm.root()),
            None => None,
        }
    })
}

/// The thread-local storage base `id` runs with - the `fs_base` of a
/// `user_regs_struct` that belongs to somebody other than the caller.
pub fn tls_of(id: TaskId) -> Option<usize> {
    with_table(|t| t.tasks.get(id.0)?.as_ref().map(|task| task.tls))
}

/// The address a process's user virtual address translates to, if it may be
/// reached from ring 3 - and copied first when it is only copy-on-write.
///
/// The copy matters for anything the kernel writes into a process: a signal
/// frame written into a page two processes still share would appear in both
/// of them, and the one that did not take the signal would find its stack
/// rewritten underneath it.
fn user_byte_address(virt: usize, write: bool) -> Option<usize> {
    let page = crate::mm::pframe::align_down(virt, crate::mm::PAGE_SIZE);
    if write && crate::arch::paging::is_cow_page(page) && !resolve_cow_fault(page) {
        return None;
    }
    let (phys, flags) = match crate::arch::paging::translate(virt) {
        Some(found) => found,
        // A page of a mapping the process has not touched yet. The walk that
        // just failed is the same walk the CPU would do, so the answer is the
        // fault handler's: bring the page in and try once more. A miss with no
        // VMA behind it is still a refusal - that is a process handing the
        // kernel an address it never mapped.
        None => {
            if !resolve_anon_fault(virt, write) {
                return None;
            }
            crate::arch::paging::translate(virt)?
        }
    };
    if !flags.contains(MapFlags::USER) {
        return None;
    }
    if write && !flags.contains(MapFlags::WRITE) {
        return None;
    }
    Some(phys)
}

/// Write `bytes` into the running process at `virt`, one page at a time.
///
/// Returns `false` if any part of the range is not a page the process could
/// write itself - which is what makes an unmapped stack a refused signal
/// rather than a kernel panic.
pub fn write_user(virt: usize, bytes: &[u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = user_byte_address(at, true) else {
            return false;
        };
        let dest =
            unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(phys) as *mut u8, chunk) };
        dest.copy_from_slice(&bytes[done..done + chunk]);
        done += chunk;
    }
    true
}

/// Read `bytes` out of the user address space `root` names.
///
/// The cross-process twin of [`read_user`], and the one `PTRACE_PEEKDATA` and
/// `process_vm_readv` are built on. It goes through the direct map rather than
/// through a user pointer, because the address belongs to a *different* process:
/// the kernel may not dereference it as its own, and the page tables that
/// describe it are not the ones CR3 holds. Every frame in the machine is
/// reachable at the HHDM offset, which is what makes "translate here, read
/// there" possible at all.
///
/// A write of a page that is still copy-on-write is deliberately *not*
/// broken here. `strace` only reads; a debugger poking a page a forked tracee
/// still shares would want the copy broken first, and doing that needs the
/// tracee's own `Mm` accounting rather than a physical address. Said plainly
/// rather than half-done.
///
/// The walk asks for `USER` - [`paging::user_phys_for_virt_of`], not the bare
/// translation - because the address belongs to somebody else: every space
/// carries the kernel's own mappings above the user half, and a walk without
/// that bit answers for them too. A same-task caller has
/// [`user_byte_address`] instead, which makes the same refusal, and the
/// user-pointer check the syscall layer runs on its own arguments cannot answer
/// for tables CR3 does not hold.
pub fn read_user_of(root: usize, virt: usize, bytes: &mut [u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = resolve_page_of(root, at, false, true) else {
            return false;
        };
        let src =
            unsafe { core::slice::from_raw_parts(mm::phys_to_virt(phys) as *const u8, chunk) };
        bytes[done..done + chunk].copy_from_slice(src);
        done += chunk;
    }
    true
}

/// Write `bytes` into the user address space `root` names. The writing half of
/// [`read_user_of`], and so the same notes apply: the `USER` bit is asked for,
/// and a copy-on-write page is not broken.
pub fn write_user_of(root: usize, virt: usize, bytes: &[u8]) -> bool {
    write_user_of_at(root, virt, bytes, true)
}

/// [`write_user_of`] with the demand-paging half made optional.
///
/// `populate` is `false` for the one caller that already holds the task table -
/// the `exec`/spawn path writes the argument block into a freshly built stack
/// while `with_table` is on the stack, and taking it again to look for a miss
/// would be a second `&mut Table`. That stack's pages are mapped eagerly by the
/// loader, so the miss this gives up on cannot happen; the flag keeps the
/// impossibility checked rather than assumed.
fn write_user_of_at(root: usize, virt: usize, bytes: &[u8], populate: bool) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = resolve_page_of(root, at, true, populate) else {
            return false;
        };
        let dest =
            unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(phys) as *mut u8, chunk) };
        dest.copy_from_slice(&bytes[done..done + chunk]);
        done += chunk;
    }
    true
}

/// The frame behind `virt` in the address space `root` names, bringing the page
/// in first when it is one of a mapping nobody has touched yet.
///
/// The demand-paging twin of the `user_phys_for_virt_of` walk, and the reason
/// the cross-process copy paths had to change at all: the walk reads page
/// tables, and a lazy page has no entry to read - so a missing page that a VMA
/// covers is populated rather than reported as `EFAULT`.
fn resolve_page_of(root: usize, virt: usize, write: bool, populate: bool) -> Option<usize> {
    match paging::user_phys_for_virt_of(root, virt) {
        Some(phys) => Some(phys),
        None if populate => {
            if !populate_user_page_at(root, virt, write) {
                return None;
            }
            paging::user_phys_for_virt_of(root, virt)
        }
        None => None,
    }
}

/// Read `bytes` from the running process at `virt`, one page at a time.
pub fn read_user(virt: usize, bytes: &mut [u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let at = virt + done;
        let page_end =
            crate::mm::pframe::align_down(at, crate::mm::PAGE_SIZE) + crate::mm::PAGE_SIZE;
        let chunk = core::cmp::min(page_end - at, bytes.len() - done);
        let Some(phys) = user_byte_address(at, false) else {
            return false;
        };
        let src =
            unsafe { core::slice::from_raw_parts(mm::phys_to_virt(phys) as *const u8, chunk) };
        bytes[done..done + chunk].copy_from_slice(src);
        done += chunk;
    }
    true
}

/// End the running task with an exit code. Its slot and stack are freed by the
/// scheduler, so this never returns and nothing needs to be cleaned up by the
/// caller.
pub fn exit(code: isize) -> ! {
    schedule(|t| {
        let id = t.current;
        if id == INIT_PID {
            // The one process the system cannot lose. Init is what waits for
            // everything else; with it gone there is nothing left to run the
            // machine's programs, and nothing to hand orphans to. The kernel
            // has no second init to start, so it says so and stops.
            crate::println!("INIT_EXITED: the system has no init left");
            crate::arch::halt();
        }
        t.task_mut(id).exited = true;
        record_retired(id, code, t.task(id).trace.tracer);

        // Everything the task owned is given back *here*, at its exit, which
        // is Linux's own moment for it (`do_exit` closes the files and tears
        // down the `mm` before the parent is told anything). Waiting for the
        // reap instead - which is where this used to happen, and what the
        // function's own doc said was wrong - left a corpse holding its whole
        // address space and every descriptor it had open for as long as nobody
        // waited for it. An unreaped pid 1-less orphan then held the DRM
        // master and its seat session against the next compositor, and a
        // process with a large `mm` (a compositor's llvmpipe arenas, say) kept
        // every frame it had allocated. Only the slot and the exit code stay
        // now, which is what a zombie is.
        t.release_exited();

        // Whoever was waiting on this task as its parent is woken here, in
        // the same critical section that made the condition true: either it
        // has not slept yet and will find the code on its next look, or it is
        // in the sleep queue this wake empties. There is no third state.
        let parent = t.task(id).parent;
        if let Some(parent) = parent {
            t.sched.wake(parent);
            // The real SIGCHLD, not just the wake. A parent sitting in
            // `wait(2)` needs only the wake - the code it waits for is on
            // file here - but a parent waiting any other way (a futex, a
            // handler of its own) is ended by the signal, and without it
            // such a caller sleeps for ever: musl's waitpid blocks on a
            // futex and lets its SIGCHLD handler do the waking. Queued only
            // when a handler would run: SIGCHLD's default action ignores
            // it, and a signal the parent has ignored must never become
            // pending - Linux discards it as it is generated.
            if t.task(parent)
                .dispositions
                .get(signal::SIGCHLD)
                .is_handler()
            {
                let child_pid = t.task(id).tgid.0 as i32;
                queue_signal(
                    t.task_mut(parent),
                    signal::SIGCHLD,
                    signal::SigInfo::user(signal::SIGCHLD, child_pid, 0),
                );
            }
            // The console goes back to the parent's group as the child leaves
            // it: a shell waits, gets its prompt back, and reads again. A
            // job-controlled shell has already taken the terminal back with
            // `TIOCSPGRP` by then - this is the same answer arrived at without
            // it, for the programs that never ask.
            let tty = crate::vfs::tty::global();
            let left = t.task(id).pgid;
            if tty.foreground() == Some(left) {
                tty.set_foreground(Some(t.task(parent).pgid));
            }
        }

        // Its children outlive it, and a dead task cannot be anyone's parent -
        // nor wait for anyone. They go to init, which waits for everything.
        // Init itself is exempt: it has no parent to lose.
        if id != INIT_PID {
            let orphans: Vec<TaskId> = t
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(slot, task)| {
                    let task = task.as_ref()?;
                    (task.parent == Some(id) && !task.exited).then_some(TaskId(slot))
                })
                .collect();
            let adopted = !orphans.is_empty();
            for orphan in orphans {
                t.task_mut(orphan).parent = Some(INIT_PID);
            }
            if adopted {
                t.sched.wake(INIT_PID);
            }
        }

        // Anything this task was tracing is let go. A tracee whose tracer has
        // died has nobody left to resume it, and a task stopped with nobody to
        // resume it sits there for the machine's life - a worse outcome than an
        // unfinished trace, which is all that running on gives up.
        let tracees: Vec<TaskId> = t
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(slot, task)| {
                let task = task.as_ref()?;
                (task.trace.tracer == Some(id) && !task.exited).then_some(TaskId(slot))
            })
            .collect();
        for tracee in tracees {
            let task = t.task_mut(tracee);
            task.trace.tracer = None;
            task.trace.armed = false;
            task.trace.stop = None;
            task.trace.regs = None;
            t.sched.wake(tracee);
        }
        // Deaths the tracer never got to collect are released with it. A
        // record still owned by a dead tracer is uncollectable for ever - the
        // real parent's `wait` would pass over it every tick while the corpse
        // it names is already gone.
        critical_section::with(|cs| {
            for retired in RETIRED.borrow(cs).borrow_mut().iter_mut() {
                if retired.tracer == Some(id) {
                    retired.tracer = None;
                }
            }
        });
    });
    unreachable!("an exited task is never picked again")
}

/// The exit code of the most recent task with this id to have exited, if any.
///
/// Recorded at exit, not at reap: the reader may come a tick later, after the
/// slot has already been freed and handed to someone else.
///
/// This is the kernel's look; it takes nothing, so a milestone can read the
/// same code twice. A process collecting its child's status uses
/// [`wait_any_child`], which consumes the record, and [`take_exit_code`] for
/// the pid it named.
pub fn exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| retired.id == id)
            .map(|retired| retired.code)
    })
}

/// Collect `id`'s exit code for caller `me`, consuming the record. `None` when
/// no death has been recorded for that pid since its slot was last reused, or
/// when the record is still owned by a tracer other than `me`.
///
/// A record owned by `me` as its tracer is *released*, not removed: the code
/// stays on file, now visible to the real parent, whose own `wait` collects it
/// and removes the record for good. That is the tracer-first order the record
/// exists to keep - unless `me` is the real parent *as well*, the
/// `fork` + `PTRACE_TRACEME` shape every strace run has: there is no other
/// parent to release to, and leaving the record in place would report the same
/// death a second time to the same waiter, whose tcb is already gone. It comes
/// out instead.
fn take_exit_code_for(t: &Table, id: TaskId, me: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|retired| retired.id == id)?;
        if retired[at].tracer.is_some_and(|tracer| tracer != me) {
            return None;
        }
        let also_parent = retired[at].tracer == Some(me)
            && t.tasks[id.0]
                .as_ref()
                .is_none_or(|task| task.parent == Some(me));
        if also_parent {
            SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
            return Some(retired.remove(at).code);
        }
        if retired[at].tracer == Some(me) {
            retired[at].tracer = None;
            return Some(retired[at].code);
        }
        // The code is collected - so the signal about it can never be read
        // again, and a stale entry must not name a slot a new task owns.
        SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
        Some(retired.remove(at).code)
    })
}

/// Collect `id`'s exit code regardless of tracer ownership. The boot task's
/// use for init, whose death nobody reaps, and nothing else.
pub fn take_exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|retired| retired.id == id)?;
        SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
        Some(retired.remove(at).code)
    })
}

/// Block the caller until one of its children exits, then take that child's
/// pid and code. A child that is already gone is reaped from the records
/// without blocking; with none gone, the caller sleeps and is woken by
/// [`exit`]'s parent wake.
///
/// The caller must be a task: this is `wait`'s core, and nothing else calls
/// it.
/// Whether a child of the running task has exited and not yet been
/// collected - the question a `signalfd` for `SIGCHLD` answers. Nothing is
/// consumed: `wait(2)` is what collects.
/// Whether a child of the running task has an unread `SIGCHLD` - the question
/// a `signalfd` answers when polled. Nothing is consumed here.
pub fn child_exited_unreaped() -> Option<usize> {
    let me = current();
    with_table(|t| {
        critical_section::with(|cs| {
            SIGCHLD_PENDING
                .borrow(cs)
                .borrow()
                .iter()
                .rev()
                .find_map(|id| {
                    // The named child must still be ours: a slot a new task
                    // took over describes nobody's child any more.
                    t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| task.parent == Some(me))
                        .then(|| id.0)
                })
        })
    })
}

/// Read one pending `SIGCHLD` of the running task, as a `signalfd` read does:
/// the signal goes to the caller and is no longer pending, while the exit
/// code itself stays on file for `wait(2)` to collect. `None` when no child
/// of this task has an unread signal.
pub fn read_sigchld() -> Option<usize> {
    let me = current();
    with_table(|t| {
        critical_section::with(|cs| {
            let mut pending = SIGCHLD_PENDING.borrow(cs).borrow_mut();
            let at = pending.iter().rposition(|id| {
                t.tasks[id.0]
                    .as_ref()
                    .is_some_and(|task| task.parent == Some(me))
            })?;
            let id = pending.remove(at);
            Some(id.0)
        })
    })
}

pub fn wait_any_child(parent: TaskId) -> WaitPark<(TaskId, isize)> {
    // The condition is the table itself, so it is asked through the borrow
    // `block_until_on_interruptible` already holds rather than taking one of its own.
    if matches!(
        block_until_on_interruptible(|t| any_child_code(t, parent).is_some()),
        WaitEnd::Interrupted
    ) {
        return WaitPark::Interrupted;
    }
    // Taken under the same borrow, not looked up again: a second lookup by pid
    // would race the slot being handed to a new task, and the record would go
    // to it instead.
    match with_table(|t| {
        any_child_code(t, parent)
            .and_then(|(id, _)| take_exit_code_for(t, id, parent).map(|code| (id, code)))
    }) {
        Some(found) => WaitPark::Event(found),
        None => WaitPark::NoChild,
    }
}

/// The pid and code of the oldest of `parent`'s children to have exited and
/// not yet been collected, written as a `NO_CODE` marker when the caller only
/// wants to know that a child died.
///
/// Nothing is consumed here: `wait_any_child` holds the table while it looks,
/// and the record is taken afterwards, by pid, in [`take_exit_code`]. A
/// read-only look keeps this function usable from the `block_until` closure,
/// which must not wake anyone.
fn any_child_code(t: &Table, parent: TaskId) -> Option<(TaskId, isize)> {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .rev()
            .find(|retired| {
                // The slot is spoken for by a live task: the record describes
                // an older tenant of the same number, so it is not this
                // parent's business.
                let id = retired.id;
                let free = t.tasks[id.0].is_none();
                let ours = free
                    || t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| waitable(task, parent));
                ours && id != parent && retired.tracer.is_none_or(|tracer| tracer == parent)
            })
            .map(|retired| (retired.id, retired.code))
    })
}

/// Give up on a record nobody will ever collect, so it cannot be mistaken for
/// a fresh death by the next `wait`. Used by the boot task for init, which it
/// hands to the system and never reaps.
pub fn discard_exit_code(id: TaskId) {
    take_exit_code(id);
}

fn record_retired(id: TaskId, code: isize, tracer: Option<TaskId>) {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        if retired.len() == RETIRED_CAP {
            let dropped = retired.remove(0);
            // A code nobody will collect has no signal left to read either.
            SIGCHLD_PENDING
                .borrow(cs)
                .borrow_mut()
                .retain(|p| *p != dropped.id);
        }
        retired.push(Retired { id, code, tracer });
        // The signal the exit carries: pending until a signalfd reader takes
        // it, independent of who eventually collects the code.
        let mut pending = SIGCHLD_PENDING.borrow(cs).borrow_mut();
        if !pending.contains(&id) {
            pending.push(id);
        }
    });
}

/// Whether a code is still waiting to be collected for `id`.
fn exit_code_pending(id: TaskId) -> bool {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow()
            .iter()
            .any(|retired| retired.id == id)
    })
}

/// Forget every retired code recorded for `id`.
///
/// Called the moment a slot is handed to a new task. The records describe
/// whoever held the slot before it was freed, and a `wait` on the new task
/// must never see them - an old death would otherwise pass for the new
/// task's exit before it has even run.
fn drop_retired(id: TaskId) {
    critical_section::with(|cs| {
        RETIRED
            .borrow(cs)
            .borrow_mut()
            .retain(|retired| retired.id != id);
    });
}

/// The running task.
pub fn current() -> TaskId {
    with_table(|t| t.current)
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

/// How many characters of a process's name Linux's `comm` holds.
///
/// It is a `char comm[16]` in the kernel's task struct, so fifteen plus a
/// terminator - and it is why `ps` truncates a long command name rather than
/// wrapping it.
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
fn snapshot_of(t: &Table, id: TaskId, task: &Task) -> TaskSnapshot {
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
        state,
        comm: comm_of(task),
        exe: task.exe.clone(),
        argv: alloc::sync::Arc::clone(&task.argv),
        cwd: task
            .cwd_path
            .clone()
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
fn comm_of(task: &Task) -> alloc::string::String {
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
/// errno.
///
/// A page that is already mapped answers `-EEXIST` rather than taking the
/// kernel down: the caller is a syscall, and a libc asking for an address it
/// already holds is a request to answer, not a reason to panic.
pub fn current_map_user(virt: usize, frame: usize, flags: crate::mm::vm::MapFlags) -> isize {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        let mapped = match task.space.as_mut() {
            Some(space) => space.try_map(crate::mm::vm::Mapping {
                virt,
                phys: frame,
                length: crate::mm::pframe::PAGE_SIZE,
                flags,
            }),
            None => return -crate::err::EINVAL,
        };
        match mapped {
            Ok(()) => {
                // The frame now belongs to the process: the reaper returns it
                // once the last thread is gone, not before.
                task.mm.own(frame);
                virt as isize
            }
            Err(crate::arch::paging::PageTaken) => -crate::err::EEXIST,
        }
    })
}

/// Put a frame the task *already owns* at `virt`, adding nothing to the list
/// of frames the reaper must return. This is how `mremap` moves a mapping: the
/// pages change address, their ownership does not change hands.
pub fn move_user_page(virt: usize, frame: usize, flags: crate::mm::vm::MapFlags) -> bool {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        match task.space.as_mut() {
            Some(space) => space
                .try_map(crate::mm::vm::Mapping {
                    virt,
                    phys: frame,
                    length: crate::mm::pframe::PAGE_SIZE,
                    flags,
                })
                .is_ok(),
            None => false,
        }
    })
}

/// The frame behind `virt` in the current task's address space, if it has one.
pub fn user_frame_at(virt: usize) -> Option<usize> {
    with_table(|t| {
        // The *group's* space, as every other mapping question asks: a thread
        // owns no address space of its own, so a thread's slot holds `None` and
        // asking it answers "nothing is mapped" for every address the thread
        // names - which is the wrong answer for all four callers. The sharpest
        // of them is `futex`: it reads the word it is about to sleep on, so
        // every thread's `FUTEX_WAIT` would see a word that is always zero and
        // answer `EAGAIN`, and musl's `__timedwait_cp` retries on `EAGAIN` in a
        // loop. A condvar wait inside a thread became a spin.
        let owner = t.space_owner(current());
        t.task(owner).space.as_ref()?.phys_for_virt(virt)
    })
}

/// Whether every page of `virt..virt + len` is still unclaimed in the current
/// task's address space.
///
/// This is what decides whether an `mmap` address is a hint worth taking, and
/// whether a `MAP_FIXED` request has anything to replace. A page counts as
/// taken when a VMA covers it even if no frame is there yet: a lazily-mapped
/// range *is* the process's, and placing a second mapping over it would leave
/// two VMAs describing one address.
pub fn user_range_free(virt: usize, len: usize) -> bool {
    (0..len)
        .step_by(crate::mm::pframe::PAGE_SIZE)
        .all(|offset| !user_mapped(virt + offset))
}

/// Take the mappings covering `virt..virt + len` away from the current task,
/// handing back the frames they pointed at.
pub fn unmap_user(virt: usize, len: usize) -> alloc::vec::Vec<usize> {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        match task.space.as_mut() {
            Some(space) => space.unmap(virt, len),
            None => alloc::vec::Vec::new(),
        }
    })
}

/// Change the permission bits of the pages the current task already has mapped
/// in `virt..virt + len`, leaving their frames alone. Pages that are not mapped
/// are skipped, so this cannot fail the way Linux's `mprotect` fails on a hole.
pub fn protect_user(virt: usize, len: usize, flags: crate::mm::vm::MapFlags) {
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        if let Some(space) = task.space.as_mut() {
            space.protect(virt, len, flags);
        }
    })
}

/// Give back frames an unmapping dropped. Nothing can reach them any more, so
/// the allocator can have them now - but only after they are off the task's own
/// lists, or its exit would free them a second time.
pub fn release_user_frames(frames: alloc::vec::Vec<usize>) {
    if frames.is_empty() {
        return;
    }
    with_table(|t| {
        let id = current();
        let owner = t.space_owner(id);
        let task = t.task_mut(owner);
        for frame in frames {
            task.mm.disown(frame);
            crate::mm::free_frames(frame, 0);
        }
    });
}

// --- Virtual memory areas, and the demand paging they make possible ---------
//
// A mapping is an *address space*: `mmap` records a VMA and stops, and the page
// behind an address appears the first time somebody touches it. Both directions
// that can touch it go through [`populate_in`] - a real `#PF` from ring 3, and
// a kernel copy that walks the page tables and finds nothing - because the two
// must give the same answer, and a page that exists for one of them and not the
// other is a bug with two faces.

/// The process whose address space is rooted at `root`, if any.
///
/// A walk of the task table rather than a lookup, and it is deliberate: the
/// caller has a CR3 value, not a task id, because the two places that need this
/// (the fault handler, and the clear-tid store the scheduler makes on a dying
/// thread's behalf) run in contexts where "the current task" is exactly the
/// thing that is not known. A thread owns no space of its own, so the answer is
/// the group leader.
fn owner_of_root(t: &Table, root: usize) -> Option<TaskId> {
    t.tasks.iter().enumerate().find_map(|(slot, task)| {
        let space = task.as_ref()?.space.as_ref()?;
        (space.root() == root).then_some(TaskId(slot))
    })
}

/// Bring in the page `virt` names, in the address space `root` names.
///
/// This is the one place a lazily-mapped page is created. It looks the VMA up,
/// checks the access against what the range permits, takes a frame - fresh and
/// zeroed for anonymous memory, the backing's own page for a frame-backed
/// mapping - maps it with the range's permissions, and records it as a frame
/// the group owns so the reaper returns it.
///
/// `false` means the address is not the process's to touch: no VMA covers it, or
/// the range does not permit the access. Every caller turns that into the
/// refusal it already had - `SIGSEGV` from the fault handler, `EFAULT` from a
/// kernel copy - so a page that is merely *absent* is distinguishable from one
/// that is not there at all, and only the former is ever created.
fn populate_in(t: &mut Table, root: usize, virt: usize, write: bool) -> bool {
    let page = pframe::align_down(virt, PAGE_SIZE);
    let Some(owner) = owner_of_root(t, root) else {
        return false;
    };
    // Normalised to the group: a thread's slot holds no space, and the frames
    // belong to the group either way.
    let owner = t.space_owner(owner);
    let Some(task) = t.tasks[owner.0].as_mut() else {
        return false;
    };
    let mm = Arc::clone(&task.mm);
    let Some(space) = task.space.as_mut() else {
        return false;
    };
    let Some(vma) = mm.vma_at(page) else {
        return false;
    };
    if !vma.permits(write) {
        return false;
    }
    let frame = match &vma.kind {
        VmaKind::Anon => {
            let Some(frame) = mm::alloc_frames(0) else {
                return false;
            };
            // A page nobody has written reads as zeros - it is a hole in the
            // program's memory, not whatever the frame's last owner left there.
            unsafe { core::ptr::write_bytes(mm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
            frame
        }
        VmaKind::Frames { object, offset, .. } => {
            // The page of the *object* this address corresponds to. The offset
            // travels with the address through every split (see `mm::vma`), so
            // this is the same page the eager loop used to map.
            let index = ((offset + (page - vma.start)) / PAGE_SIZE) as u64;
            match object.frame_at(index) {
                Some(frame) => frame,
                None => return false,
            }
        }
    };
    let mapped = space
        .try_map(Mapping {
            virt: page,
            phys: frame,
            length: PAGE_SIZE,
            flags: vma.prot,
        })
        .is_ok();
    if !mapped {
        // Somebody is already there. On one core that cannot happen between the
        // miss and here, but a `PROT_NONE` reserve that was never populated can
        // still be met, and the frame must go back rather than leak.
        crate::mm::free_frames(frame, 0);
        return false;
    }
    mm.own(frame);
    true
}

/// The `#PF` entry: resolve a fault on a lazily-mapped page, the way
/// [`resolve_cow_fault`] resolves one on a page a `fork` still shares.
///
/// The address space is taken from CR3 rather than from the current task,
/// because the two can differ - the scheduler writes a dying thread's clear-tid
/// word after switching the CPU to that thread's tables - and the tables the
/// fault came through are the ones that decide what the address means.
pub fn resolve_anon_fault(virt: usize, write: bool) -> bool {
    let root = paging::active_root();
    with_table(|t| populate_in(t, root, virt, write))
}

/// [`resolve_anon_fault`] for an address space named by its root rather than by
/// whatever CR3 holds: the cross-process half, what `process_vm_readv` and a
/// tracer's `PTRACE_POKEDATA` reach a lazily-mapped page through.
pub fn populate_user_page_at(root: usize, virt: usize, write: bool) -> bool {
    with_table(|t| populate_in(t, root, virt, write))
}

/// Whether the kernel may treat the *absent* page holding `virt` as one it can
/// touch, because a VMA backs it and the access is permitted.
///
/// `buffer_ok` asks this before a syscall does a raw copy, so a range the
/// process has legally mapped but not touched is not refused as `EFAULT`. The
/// answer is restricted to anonymous ranges on purpose: populating one costs
/// nothing but a frame, while populating a frame-backed page calls into the
/// node that owns it - and the DRM ioctl path validates a caller's array
/// *while holding the card's lock*, then writes through it, so a page that
/// needed the card to answer a fault would be a lock taken twice. A
/// frame-backed page must be present before the kernel will look at it, which
/// is the rule the eager mapping this replaces enforced by never leaving one
/// absent.
pub fn kernel_lazy_accessible(virt: usize, write: bool) -> bool {
    let root = paging::active_root();
    with_table(|t| {
        let Some(owner) = owner_of_root(t, root) else {
            return false;
        };
        let owner = t.space_owner(owner);
        match t.task(owner).mm.vma_at(pframe::align_down(virt, PAGE_SIZE)) {
            Some(vma) => matches!(&vma.kind, VmaKind::Anon) && vma.permits(write),
            None => false,
        }
    })
}

/// The frame list and VMA table of the current task's group.
fn current_mm() -> Arc<Mm> {
    with_table(|t| Arc::clone(&t.task(t.space_owner(t.current)).mm))
}

/// Record a private anonymous mapping of `virt..virt + len`.
///
/// Nothing is allocated: the address space is the whole of the mapping, and the
/// frames arrive as the program touches them. `flags` is what each page will be
/// mapped with, and what an access is checked against.
pub fn map_anon(virt: usize, len: usize, flags: MapFlags) {
    if len == 0 {
        return;
    }
    current_mm().vma_insert(Vma::new(virt, virt + len, flags, VmaKind::Anon));
}

/// Record a mapping of `virt..virt + len` backed by `inode`'s own frames,
/// starting at page `offset` of that object. This is what `memfd`, the
/// framebuffer and a dma-buf get, and what a shared anonymous mapping is made
/// of: the pages are the object's, shared by every mapping of it.
///
/// `shared` is the `MAP_SHARED` bit the caller asked for, which the mapping
/// keeps because `/proc/<pid>/maps` reports it - a `memfd` mapped
/// `MAP_PRIVATE` is the same object and still not a shared mapping. `name` is
/// what that file prints for this line, resolved from the descriptor *now*
/// because resolving it later means walking the filesystem tree (see
/// [`crate::vfs::File::display_name`]).
pub fn map_frames(
    virt: usize,
    len: usize,
    flags: MapFlags,
    object: Arc<dyn crate::mm::vma::MappedObject>,
    offset: usize,
    shared: bool,
    name: Option<Arc<str>>,
) {
    if len == 0 {
        return;
    }
    current_mm().vma_insert(
        Vma::new(
            virt,
            virt + len,
            flags,
            VmaKind::Frames {
                object,
                offset,
                shared,
            },
        )
        .named(name),
    );
}

/// `munmap`'s two halves at once: forget the VMAs covering `virt..virt + len`
/// and hand back the frames the present pages held.
///
/// The order matters. The table goes first, so that a page released here is not
/// also reachable through a stale VMA - which is what `release_user_frames`
/// relies on when it asks the group to forget the frame.
pub fn munmap_user(virt: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = virt.saturating_add(len);
    current_mm().vma_remove_range(virt, end);
    release_user_frames(unmap_user(virt, len));
}

/// `mprotect`'s two halves: the VMAs get the new permissions so that a page
/// faulted in later comes up with them, and the pages already present are
/// re-flagged where they lie.
pub fn protect_user_range(virt: usize, len: usize, flags: MapFlags) {
    if len == 0 {
        return;
    }
    current_mm().vma_set_prot(virt, virt.saturating_add(len), flags);
    protect_user(virt, len, flags);
}

/// `madvise(MADV_DONTNEED)` over `virt..virt + len`: give the range's pages
/// back, leaving the mapping itself in place.
///
/// The mapping stays, so the next access is an ordinary fault and the page
/// comes back from whatever backs the range - and *that* is the whole of the
/// advice's contract, which is why it took demand paging to answer. A private
/// anonymous page comes back freshly zeroed (its frame is gone, so the fault
/// allocates a new one); a page of a frame-backed range - a memfd, a
/// framebuffer, or the unnamed object a shared anonymous mapping is made of -
/// comes back from the object, which still holds it. Linux says the same thing
/// in the same words: "subsequent accesses ... will result in either
/// repopulating the memory contents from ... the underlying mapped file ... or
/// zero-fill-on-demand pages for anonymous private mappings".
///
/// Only pages a VMA covers are touched. That is not a nicety: the program
/// image and the stack are mapped but have no VMA, and dropping one of those
/// would leave an address that no rule says how to refill.
pub fn madvise_dontneed(virt: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = virt.saturating_add(len);
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return;
        };
        let mm = Arc::clone(&task.mm);
        let Some(space) = task.space.as_mut() else {
            return;
        };
        let mut dropped = Vec::new();
        let mut page = pframe::align_down(virt, PAGE_SIZE);
        while page < end {
            if !mm.vma_covers(page) {
                page += PAGE_SIZE;
                continue;
            }
            // The whole run of covered pages in one unmapping, so a range that
            // is all one VMA - the common case - is one page-table walk and one
            // TLB flush rather than one of each per page.
            let run = page;
            while page < end && mm.vma_covers(page) {
                page += PAGE_SIZE;
            }
            dropped.extend(space.unmap(run, page - run));
        }
        // The frames are the mapping's own references. For an `Anon` page this
        // is the last one, so the frame goes back to the allocator and the
        // "zero-filled page" the caller is promised really is a new one; for a
        // frame-backed page it is one of several, and the object's copy is what
        // the next fault finds.
        for frame in dropped {
            mm.disown(frame);
            crate::mm::free_frames(frame, 0);
        }
    });
}

/// Whether an address is the current process's: a page is there, or a VMA says
/// one belongs there.
///
/// The distinction from [`user_frame_at`] is the whole point of demand paging:
/// a mapping is *claimed* at `mmap` time and *filled* on touch, so "is this
/// address mapped?" and "is there a frame behind it?" are different questions.
pub fn user_mapped(virt: usize) -> bool {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        let task = t.task(owner);
        task.space
            .as_ref()
            .and_then(|space| space.phys_for_virt(virt))
            .is_some()
            || task.mm.vma_covers(virt)
    })
}

/// The permissions the current process has on `virt`, if a VMA covers it.
///
/// Used where the *range* decides what a later fault will be allowed to do -
/// `MADV_DONTNEED` dropping pages that must come back writable, `mremap`
/// carrying a mapping's permissions with it.
pub fn vma_prot_at(virt: usize) -> Option<MapFlags> {
    with_table(|t| {
        let owner = t.space_owner(t.current);
        t.task(owner)
            .mm
            .vma_at(pframe::align_down(virt, PAGE_SIZE))
            .map(|vma| vma.prot)
    })
}

/// The VMAs covering `virt..virt + len` in the current process, clipped to the
/// range, with the table left empty there.
///
/// `mremap` takes a mapping's backing out of one place and puts it in another,
/// and the VMAs are what remember which backing that is.
pub fn take_vmas(virt: usize, len: usize) -> Vec<Vma> {
    if len == 0 {
        return Vec::new();
    }
    current_mm().vma_take_range(virt, virt.saturating_add(len))
}

/// The parts of `virt..virt + len` the current process has not already claimed.
pub fn vma_gaps(virt: usize, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    current_mm().vma_gaps(virt, virt.saturating_add(len))
}

/// Put a VMA back, backing and all - how `mremap` completes a move.
pub fn insert_vma(virt: usize, end: usize, flags: MapFlags, kind: VmaKind) {
    if virt >= end {
        return;
    }
    current_mm().vma_insert(Vma::new(virt, end, flags, kind));
}

/// Claim `virt..virt + len` for anonymous memory without disturbing what is
/// already there: the shape a heap growth wants, where a guard page a libc
/// mapped inside the range must survive.
pub fn claim_anon_gaps(virt: usize, len: usize, flags: MapFlags) {
    let mm = current_mm();
    for (start, end) in mm.vma_gaps(virt, virt.saturating_add(len)) {
        mm.vma_insert(Vma::new(start, end, flags, VmaKind::Anon));
    }
}

/// `brk(addr)` — per-task program break (heap end).
///
/// `addr` of 0 asks where the break is. Otherwise the break moves, and the
/// pages between the old and the new position are mapped or given back as the
/// move requires - which is what makes this a *break* rather than a note that
/// the number changed.
///
/// Growing skips a page that is already mapped instead of insisting it is
/// free. That is not a nicety: a libc may put a guard page inside the range the
/// break covers - musl's mallocng maps `PROT_NONE` over the first page of a
/// freshly grown heap - and the break still has to move past it.
pub fn brk(addr: usize) -> isize {
    use crate::mm::pframe::PAGE_SIZE;
    use crate::mm::vm::MapFlags;

    let id = current();
    with_table(|t| {
        let task = t.task_mut(id);
        // Kernel tasks have no brk.
        if task.space.is_none() {
            return 0isize;
        }

        if task.brk_base == 0 {
            return 0isize;
        }

        if addr == 0 {
            return (task.brk_base + task.brk_pages * PAGE_SIZE) as isize;
        }

        let needed_pages = if addr > task.brk_base {
            (addr - task.brk_base).div_ceil(PAGE_SIZE)
        } else {
            0
        };

        // A break that shrinks gives its frames back; one that grows takes
        // more. Either way the pages are this process's from here on.
        let frontier = task.brk_base + needed_pages * PAGE_SIZE;
        // The heap may not walk into the mmap region: between the two lies the
        // hole that gives the address space its Linux shape. A break that far
        // up is a runaway allocation, not a request to honor.
        if frontier >= crate::syscall::MMAP_BASE {
            return crate::err::ENOMEM;
        }
        let grown_to = task.brk_base + task.brk_pages * PAGE_SIZE;
        if frontier < grown_to {
            // The description goes with the pages: a heap that shrank is no
            // longer the process's past the new break, so `munmap`/`madvise`
            // and a later `fork` must not see it there. The range is the tail
            // above the break, which is where a `brk` shrink always cuts.
            task.mm.vma_remove_range(frontier, grown_to);
            let dropped = match task.space.as_mut() {
                Some(space) => space.unmap(frontier, grown_to - frontier),
                None => alloc::vec::Vec::new(),
            };
            for frame in dropped {
                task.mm.disown(frame);
                crate::mm::free_frames(frame, 0);
            }
        }

        for i in task.brk_pages..needed_pages {
            let vaddr = task.brk_base + i * PAGE_SIZE;
            // Already mapped: someone else's guard page, or a page the break
            // has already been past. Either way there is nothing to add.
            if task
                .space
                .as_ref()
                .and_then(|space| space.phys_for_virt(vaddr))
                .is_some()
            {
                continue;
            }
            let frame = match crate::mm::alloc_frames(0) {
                Some(f) => f,
                None => return crate::err::ENOMEM,
            };
            unsafe {
                let ptr = crate::mm::phys_to_virt(frame) as *mut u8;
                core::ptr::write_bytes(ptr, 0, PAGE_SIZE);
            }
            let mapped = match task.space.as_mut() {
                Some(space) => space
                    .try_map(crate::mm::vm::Mapping {
                        virt: vaddr,
                        phys: frame,
                        length: PAGE_SIZE,
                        flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                    })
                    .is_ok(),
                None => false,
            };
            if !mapped {
                crate::mm::free_frames(frame, 0);
                return crate::err::ENOMEM;
            }
            task.mm.own(frame);
        }

        // The pages the break grew over become a VMA too, so that `munmap`,
        // `madvise` and `fork` see one table rather than two - the heap used to
        // be the one region whose mapping only the page tables knew about.
        //
        // Only the *newly* added range is claimed, and only where nothing else
        // already is: a libc is free to have mapped inside the space the break
        // is about to grow into - musl's mallocng puts a `PROT_NONE` guard over
        // the first page of a fresh heap - and that mapping is its own, not the
        // break's.
        if frontier > grown_to {
            for (start, end) in task.mm.vma_gaps(grown_to, frontier) {
                task.mm.vma_insert(Vma::new(
                    start,
                    end,
                    MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                    VmaKind::Anon,
                ));
            }
        }

        task.brk_pages = needed_pages;
        addr as isize
    })
}

/// Set the initial brk base for the current task (called at ELF load time).
pub fn set_brk_base(base: usize) {
    with_table(|t| {
        let task = t.task_mut(current());
        task.brk_base = base;
    })
}

/// Get the mmap cursor for the current task.
pub fn get_mmap_cursor() -> usize {
    with_table(|t| {
        let task = t.task(current());
        task.mmap_cursor
    })
}

/// The `MEMBARRIER_CMD_REGISTER_*` commands the running task's process has
/// declared it will use, as a bitmask of the *barrier* commands they enable.
///
/// The process, not the task: a registration made by one thread is what lets
/// every thread of the group use the barrier, which is the only sense in which
/// a barrier over a process's threads can be asked for at all.
pub fn membarrier_registrations() -> usize {
    with_table(|t| t.task(t.current).mm.membarrier_registrations())
}

/// Record a `membarrier` registration for the running task's process.
pub fn register_membarrier(barrier: usize) {
    with_table(|t| t.task(t.current).mm.register_membarrier(barrier));
}

/// The robust-futex list head the running task's process registered, `0` for
/// none. Backs `get_robust_list`.
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
    unsafe { write_msr(MSR_FS_BASE, base as u64) };
}

/// `IA32_FS_BASE`: where ring 3's `%fs:` segment starts.
const MSR_FS_BASE: u32 = 0xC000_0100;

/// Write one model-specific register. The two halves go in separately because
/// `wrmsr` takes them in `eax` and `edx`.
unsafe fn write_msr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

/// How many tasks are alive, counting the two permanent ones. [`PERMANENT_TASKS`]
/// is what this settles back to once everything a test started has been reaped.
pub fn task_count() -> usize {
    with_table(|t| t.tasks.iter().filter(|slot| slot.is_some()).count())
}

/// Whether any child of `parent` is alive right now - what init's first shell
/// looks like from the boot task's side of the hand-over.
pub fn has_live_child(parent: TaskId) -> bool {
    with_table(|t| child_alive(t, parent))
}

/// [`has_live_child`] against a table the caller already holds.
fn child_alive(t: &Table, parent: TaskId) -> bool {
    t.tasks.iter().enumerate().any(|(slot, task)| {
        task.as_ref().is_some_and(|task| {
            !task.exited && task.parent == Some(parent) && TaskId(slot) != parent
        })
    })
}

/// How many tasks are alive forever, and so are not something a `drain` should
/// wait for: the boot task, the scheduler, and everything started as permanent.
///
/// The count has to move, not be a constant: the network service is a task that
/// never exits and is started at a time of the boot code's choosing, and a
/// `drain` that kept the old figure would wait on it forever.
pub fn permanent_tasks() -> usize {
    PERMANENT_TASKS + PERMANENT_EXTRA.load(Ordering::Relaxed)
}

/// Start a task that is meant to run for the machine's whole life, and count it
/// as one. [`permanent_tasks`] is what tells the boot code's `drain` which
/// tasks are never going to finish.
pub fn spawn_permanent(f: impl FnOnce() + Send + 'static, priority: Priority) -> TaskId {
    let id = spawn(f, priority);
    PERMANENT_EXTRA.fetch_add(1, Ordering::Relaxed);
    id
}

/// Called from the clock interrupt: the running task has used its slice.
///
/// Does nothing before [`init`], and nothing at all when the scheduler itself is
/// running. The scheduler is resumed by tasks handing over to it, never by being
/// picked, so preempting it would leave it suspended halfway through a switch it
/// had already committed to.
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
            remaining_ticks: state.deadline.saturating_sub(timer::ticks()),
        })
    })
}

/// Read the running task's timer `which` without disturbing it.
pub fn itimer_snapshot(which: usize) -> Option<ItimerSnapshot> {
    with_table(|t| {
        let state = t.task(t.current).itimers[which].as_ref()?;
        Some(ItimerSnapshot {
            interval_ticks: state.interval_ticks,
            remaining_ticks: state.deadline.saturating_sub(timer::ticks()),
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
            timer
                .deadline
                .map_or(0, |d| d.saturating_sub(timer::ticks())),
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
            timer
                .deadline
                .map_or(0, |d| d.saturating_sub(timer::ticks())),
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
    idle();
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
pub fn device_not_available() {
    with_table(|t| t.reclaim_fpu());
}

/// The one path off a task's stack.
fn schedule(prepare: impl FnOnce(&mut Table)) {
    critical_section::with(|cs| {
        // Deliberately not `with_table`: the borrow has to be dead before the
        // switch below, and a closure would hold it across.
        let (from, to) = unsafe { (*TABLE.borrow(cs).get()).hand_over(prepare) };
        unsafe { context::switch(from, to) };
    });
}

/// Create a child process: a copy of the calling one, resuming from the very
/// `fork` call that made it.
///
/// Linux's contract, exactly: the child comes back from `fork` with 0 in `rax`,
/// the parent comes back with the child's pid, and both carry on from the
/// instruction after the `syscall`. `state` is the parent's ring-3 register file
/// captured at the gate; the copy the child resumes with differs in `rax`
/// alone.
///
/// The address space is duplicated copy-on-write: both processes start out
/// sharing every frame with the writable pages marked read-only and a COW bit
/// set. The first write on either side faults, allocates a private frame, copies
/// the page, and restores write permission - so each process ends up with its own
/// copy at the moment it actually diverges, and never before.
///
/// The file descriptor table is inherited, not copied: the child gets the same
/// `Arc<FdTable>` the parent holds, which is what a shell's `execve` after
/// `fork` needs. A `CLONE_FILES`-style split is the job of `clone`, not `fork`.
pub fn fork(state: &UserContext) -> isize {
    spawn_copy(state, CloneRequest::default())
}

/// `vfork(2)`: the same copy of the calling process, labelled for the tracer.
///
/// Linux's `vfork` is `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, and that flag is
/// what makes the event a tracer is waiting for `PTRACE_EVENT_VFORK` rather
/// than `PTRACE_EVENT_FORK`. The sharing and the suspended parent are the parts
/// this kernel does not reproduce - see [`clone`] for why a copy meets the
/// contract better here than a share would - but which of the two events a
/// tracer is waiting for is not something it can recover for itself.
pub fn vfork(state: &UserContext) -> isize {
    spawn_copy(
        state,
        CloneRequest {
            flags: nr::CLONE_VFORK,
            ..CloneRequest::default()
        },
    )
}

/// What a `clone` asks for beyond "a copy of me": which flags, where the child's
/// stack is, where the child should record its own tid, and what TLS it runs
/// with.
///
/// A default request is a plain `fork`: no flags, the caller's own stack, no tid
/// write, the caller's TLS.
#[derive(Clone, Copy, Default)]
pub struct CloneRequest {
    pub flags: usize,
    pub child_stack: usize,
    pub child_tid: usize,
    pub tls: usize,
}

/// Create a process that is a copy of the calling one, resuming from the very
/// call that made it.
///
/// This is the shared heart of `fork` and `clone`: `fork` is this with a default
/// request, and `clone` is this with the caller's flags filled in.
///
/// The address space is duplicated copy-on-write: both processes start out
/// sharing every frame with the writable pages marked read-only and a COW bit
/// set. The first write on either side faults, allocates a private frame, copies
/// the page, and restores write permission - so each process ends up with its own
/// copy at the moment it actually diverges, and never before.
///
/// The descriptor table is *copied*, not shared: the child names the same open
/// files under the same numbers, which is what a shell's `dup2`-then-`execve`
/// after a `fork` needs - and what it needs is for the parent's own `close` to
/// stay on the parent's side of the fork.
fn spawn_copy(state: &UserContext, request: CloneRequest) -> isize {
    // Phase 1: Collect all parent state under one immutable borrow, then
    // release it before any mutations.
    let (
        parent_id,
        parent_has_space,
        parent_priority,
        parent_fds,
        parent_cwd,
        parent_cwd_path,
        parent_exe,
        parent_argv,
        parent_brk_base,
        parent_brk_pages,
        parent_mmap_cursor,
        parent_tls,
        parent_pages,
        parent_vmas,
        parent_eager,
    ) = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        let parent_has_space = parent.space.is_some();
        let parent_priority = parent.priority;
        let parent_fds = Arc::clone(&parent.fds);
        let parent_cwd = parent.cwd.clone();
        let parent_cwd_path = parent.cwd_path.clone();
        let parent_exe = parent.exe.clone();
        let parent_argv = alloc::sync::Arc::clone(&parent.argv);
        let parent_brk_base = parent.brk_base;
        let parent_brk_pages = parent.brk_pages;
        let parent_mmap_cursor = parent.mmap_cursor;
        let parent_tls = parent.tls;

        // Collect the parent's user page mappings. We need to do this while
        // holding the borrow, then release it before calling into AddressSpace.
        let mut parent_pages = Vec::new();
        if let Some(space) = &parent.space {
            space.walk_user_pages(|virt, phys, flags| {
                parent_pages.push((virt, phys, flags));
            });
        }
        // The parent's VMAs come across whole. The child's address space is the
        // parent's - the same ranges with the same permissions and the same
        // backing - so a mapping the parent had not touched is one the child has
        // not touched either, and costs nothing to duplicate. That is what makes
        // a `fork` of a process with a large untouched `mmap` cheap: only the
        // *present* pages below are walked at all.
        let parent_vmas = parent.mm.vma_snapshot();
        // And so does what the parent was *loaded* with: a child's address
        // space is its parent's, so `/proc/<pid>/maps` says the same thing on
        // either side of the fork.
        let parent_eager = parent.mm.eager_ranges();
        (
            parent_id,
            parent_has_space,
            parent_priority,
            parent_fds,
            parent_cwd,
            parent_cwd_path,
            parent_exe,
            parent_argv,
            parent_brk_base,
            parent_brk_pages,
            parent_mmap_cursor,
            parent_tls,
            parent_pages,
            parent_vmas,
            parent_eager,
        )
    });

    // A task with no address space has no image to copy: `fork` from a kernel
    // task is meaningless, and Linux answers ENOMEM for a process it cannot
    // duplicate.
    if !parent_has_space {
        return crate::err::ENOMEM;
    }

    // Phase 2: Allocate a slot and build the child state.
    let child_id = with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));

        let stack = Stack::new();
        // Interrupts stay off in the child until its `iretq` restores the
        // parent's RFLAGS: a fork child resumes inside a syscall gate, and the
        // gate runs with IF clear. Enabling them any earlier would let the clock
        // fire on a task that has not reached user mode yet.
        let mut context = unsafe { context::bootstrap(stack.top(), task_entry, slot, false) };
        // The child's floating-point registers are its creator's: they are part
        // of what `fork` and a thread-creating `clone` copy, and `bootstrap`
        // would otherwise hand over the architectural reset state. `inherit_fpu`
        // reads them out of the parent - from the registers while they are still
        // the parent's, from the parent's saved block when they are no longer.
        context.fpu = t.inherit_fpu(parent_id);

        // The child's address space is not a copy of every page: a
        // *frame-backed* range - a memfd, a dma-buf, the framebuffer, or the
        // unnamed object behind `MAP_SHARED|MAP_ANONYMOUS` - is an object both
        // processes are supposed to watch through, so its present pages cross
        // the fork mapped writable and marked nothing. The ranges are collected
        // here, alongside the pages, so the two loops below agree about which
        // pages are which.
        let shared: Vec<(usize, usize)> = parent_vmas
            .iter()
            .filter(|vma| matches!(vma.kind, VmaKind::Frames { .. }))
            .map(|vma| (vma.start, vma.end))
            .collect();

        // Create the child's address space by sharing all parent pages COW.
        let mut child_space = AddressSpace::new();
        let mut child_frames = Vec::new();
        let child_root = child_space.root();

        for &(virt, phys, flags) in &parent_pages {
            // Two address spaces now own this frame; the count keeps it alive
            // until the last of them lets go.
            crate::mm::clone_frame_ref(phys);
            child_frames.push(phys);
            // A page of a shared object is mapped as itself, at the permissions
            // it already has. A page that is *already* copy-on-write is an
            // exception, and deliberately: an older `fork` marked it, and the
            // frame it would otherwise share is one that flag already governs.
            if !flags.contains(MapFlags::COW) && shared.iter().any(|&(s, e)| s <= virt && virt < e)
            {
                child_space.map_shared(virt, phys, flags);
            } else {
                // Map it in the child as COW: read-only for now, with the COW
                // bit marking it for the fault handler.
                child_space.map_cow(virt, phys, flags);
            }
        }

        // Make the parent's writable pages COW too, or the child's copy would be
        // the only read-only one and the parent would write straight into a frame
        // the child is also reading. Re-borrow: the phase-1 borrow is long gone.
        //
        // A page the walk reported as already COW is left alone: it is shared
        // with an older sibling and is protected already. Re-protecting it is
        // harmless, but the walk reported it without `WRITE`, so a test for
        // writability alone would skip it and leave the *parent* writable while
        // the child's copy stays read-only - which is the bug this whole dance
        // exists to avoid.
        //
        // One flush for the whole loop, at the end: the PTE write is what makes
        // the page read-only, but the CPU keeps using the translations it
        // already has until the TLB is dropped, and doing that per page asked
        // the machine to re-walk the parent's entire address space once for
        // every writable page it owns - the single largest cost in a fork of a
        // large process. Nothing runs on this address space between the first
        // `set_cow_bit_quietly` and the flush, because this *is* the task that
        // owns it, inside its own syscall.
        if let Some(parent_space) = &mut t.task_mut(parent_id).space {
            for &(virt, _phys, flags) in &parent_pages {
                if flags.contains(MapFlags::COW) || !flags.contains(MapFlags::WRITE) {
                    continue;
                }
                // A page of a shared object stays writable in the parent too.
                // Hardening it would not protect anything - the child's copy is
                // deliberately writable - and it would make the parent's *next*
                // write take a private copy of a frame the child is watching,
                // which is precisely the sharing this exists to keep.
                if shared.iter().any(|&(s, e)| s <= virt && virt < e) {
                    continue;
                }
                // Everything the page had except write permission, in one PTE
                // write: hardening it to `READ | USER` would silently take away
                // execute on a page that is both, and a COW copy made from it
                // later would stay non-executable forever.
                parent_space.set_cow_bit_quietly(virt);
            }
            parent_space.flush_if_active();
        }

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority: parent_priority,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(child_space),
            user: true,
            // The child does not start at the entry point: it returns from the
            // `fork` that made it, so it carries a whole register file instead.
            user_entry: None,
            user_resume: Some(UserContext {
                // A thread starts on the stack `clone` handed it; a process
                // comes back from the call on the stack its parent was using.
                rsp: if request.child_stack != 0 {
                    request.child_stack
                } else {
                    state.rsp
                },
                ..*state
            }),
            mm: {
                // A child's address space is its parent's, so what the parent
                // was *loaded* with is the child's report too - `/proc` on
                // either says the same thing.
                let mm = Mm::with_eager(TaskId(slot), child_root, child_frames, parent_eager);
                mm.set_vmas(parent_vmas);
                // The credentials come along, as Linux's `copy_creds` shares
                // them: a process that had dropped a bounding capability or
                // become non-dumpable must not hand its child the full set.
                mm.set_credentials(t.task(parent_id).mm.credentials());
                mm
            },
            released: false,
            tgid: TaskId(slot),
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            // The mask is inherited, as POSIX says a child's is: a program
            // that blocks a signal and then forks expects the child to start
            // with it blocked too. The *dispositions* above are deliberately
            // not inherited (this kernel starts every process at `SIG_DFL`),
            // but a mask is not a disposition - it is the caller saying "not
            // now", and a child that un-blocked what its parent held would
            // take a signal the parent meant to handle itself.
            blocked: t.task(parent_id).blocked,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: Arc::new(parent_fds.duplicate()),
            cwd: parent_cwd,
            cwd_path: parent_cwd_path,
            parent: Some(parent_id),
            pgid: t.task(parent_id).pgid,
            // A fork child is in the same session as its parent and holds the
            // same controlling terminal - the identity `/dev/tty` opens for
            // it, whatever its parent was doing when it copied itself.
            session: t.task(parent_id).session,
            ctty: t.task(parent_id).ctty,
            identity: t.task(parent_id).identity.clone(),
            // A fork child is in its parent's namespace, by reference: the two
            // go on reporting one set of names until one of them unshares. A
            // copy here would give a `fork` the effect `unshare` is for.
            uts: Arc::clone(&t.task(parent_id).uts),
            umask: t.task(parent_id).umask,
            oom_score_adj: t.task(parent_id).oom_score_adj,
            oom_score_adj_min: t.task(parent_id).oom_score_adj_min,
            nice: t.task(parent_id).nice,
            rlimits: t.task(parent_id).rlimits,
            seccomp_strict: t.task(parent_id).seccomp_strict,
            seccomp_filters: t.task(parent_id).seccomp_filters.clone(),
            brk_base: parent_brk_base,
            brk_pages: parent_brk_pages,
            mmap_cursor: parent_mmap_cursor,
            // The child's memory is its parent's, addresses included: a stack
            // that started somewhere else would leave the copied page tables
            // describing a region the child does not own.
            stack_top: t.task(parent_id).stack_top,
            exe: parent_exe,
            argv: parent_argv,
            // `CLONE_CHILD_SETTID` parks the child's tid where the child can
            // read it; without the flag there is nothing to record.
            child_tid: if request.flags & nr::CLONE_CHILD_SETTID != 0 {
                request.child_tid
            } else {
                0
            },
            // `CLONE_SETTLS` names a TLS descriptor for the child alone;
            // otherwise the child inherits the caller's.
            tls: if request.flags & nr::CLONE_SETTLS != 0 {
                request.tls
            } else {
                parent_tls
            },
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });

        let child_id = TaskId(slot);
        // A child born into a tracer's hands does not go on the ready queue: it
        // is stopped, and letting it run would be the one thing that stop exists
        // to prevent. Everything else about it is already built - `user_resume`
        // holds the register file it will come back from the call with - so the
        // resume is what puts it on the queue, exactly as it is for a tracee
        // stopped anywhere else.
        if attach_new_child(t, parent_id, child_id, fork_event(request.flags)) {
            t.sched.sleep(child_id, parent_priority, u64::MAX);
        } else {
            t.sched.push_ready(child_id, parent_priority);
        }
        t.sched.wake(parent_id);
        child_id
    });

    child_id.0 as isize
}

/// Replace the current process's image with a new ELF binary. Tears down the
/// old address space, loads the new ELF segments, resets process state, and
/// returns `(entry_point, user_rsp)` so the syscall stub can patch its return
/// frame and enter the new image on `sysretq`.
///
/// This does **not** modify the register frame itself — the caller (syscall
/// dispatch) does that.
pub fn exec_replace(
    bytes: &[u8],
    argv: &[String],
    env: &[String],
    exe: &str,
) -> Result<(usize, usize), SpawnError> {
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // A dynamically linked program names the interpreter that has to run
    // before it does. That is a path into the filesystem, and this is the only
    // place with both the path and the authority to follow it - which is why
    // the interpreter is read here rather than inside the loader.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    // A fresh layout with the fresh image. This is the moment ASLR belongs to:
    // a `fork` copies the parent's addresses because it copies its memory, but
    // an `exec` throws that memory away, so the new program is under no
    // obligation - and gets no benefit - from starting where the old one did.
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, exe, layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);

        // --- Tear down old address space ---
        // Drop the old AddressSpace (frees lower-half page tables).
        if let Some(old_space) = task.space.take() {
            drop(old_space);
        }
        // Give every page the old image owned back. `free_frames` does the
        // reference-count half itself, so a page a `fork` shared is only
        // returned to the allocator by whichever process drops the last claim.
        for frame in task.mm.take_frames() {
            crate::mm::free_frames(frame, 0);
        }
        // The task is about to be a *new* program: it is no longer a fork child
        // waiting to resume its parent's call, and a fresh image starts with no
        // TLS and no tid to record.
        task.user_resume = None;
        task.child_tid = 0;
        task.tls = 0;
        unsafe { write_msr(MSR_FS_BASE, 0) };
        // And a fresh image starts with the floating-point registers reset, for
        // the reason POSIX says so and Linux does the same: the discarded image
        // must not be readable out of them, and its rounding mode must not leak
        // into the next program's arithmetic. Both copies are reset - the live
        // registers, because they are what the new program runs with until the
        // next switch, and the task's saved block, because that is what a switch
        // to it would restore.
        //
        // The claim comes first: `fninit` is a floating-point instruction, and
        // under the lazy scheme a task that never touched the FPU is running
        // with `CR0.TS` set, so resetting without claiming would trap here - in
        // the kernel, with the table lock held. Claiming is also the honest
        // record of what follows: once the reset block is in the live registers
        // this task genuinely owns them, and saying so saves the new image a
        // pointless fault in its first `memcpy`.
        unsafe { crate::arch::context::claim_live_fpu() };
        unsafe { crate::arch::context::reset_live_fpu() };
        task.context.fpu = crate::arch::context::FpuState::reset();
        task.fpu_live = true;

        // --- Build new address space ---
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            exe,
            interp_name,
        );

        write_arg_block(space.root(), user_rsp, argv, env, exe, &loaded);

        // --- Reset process state ---
        //
        // The new root is recorded in the task *before* the CPU is pointed at
        // it, and that order is load-bearing. A clock tick landing between the
        // two would send the dispatcher to `prepare`, which reads the task's
        // space to decide what to load - and a task still holding the address
        // space it has just dropped would be aimed at freed tables. Recording
        // first means the worst a tick can do is load the new root slightly
        // early, which is where the process is going anyway.
        let new_root = space.root();
        task.space = Some(space);
        task.user_entry = Some((loaded.entry, user_rsp));
        // The group's memory becomes the new image's. A process that execs
        // with threads running is a case Linux handles by ending them; this
        // kernel does not yet, and the threads that were there keep the old
        // group until they exit.
        //
        // The credentials cross the boundary with the process, and the exec
        // itself re-derives them: an `euid` of 0 regains the bounding set's
        // capabilities (measured - a root process that had lowered its
        // permitted set gets it back), which is what makes
        // `PR_CAPBSET_DROP` a promise that survives an `exec`. The dumpable
        // flag is deliberately *not* reset here: Linux's `setup_new_exec`
        // only moves it for a setuid image, and this kernel reads no file
        // capabilities or setuid bits at exec.
        let mut carried = task.mm.credentials();
        carried.on_exec(task.identity.euid);
        let next_mm = Mm::with_eager(id, new_root, loaded.frames, loaded.ranges);
        next_mm.set_credentials(carried);
        task.mm = next_mm;
        task.brk_base = loaded.brk;
        task.brk_pages = 0;
        task.mmap_cursor = layout.mmap_top;
        task.stack_top = loaded.stack_top;
        task.exe = alloc::string::String::from(exe);
        // The program that was running is being replaced, and so is the
        // command line that described it: a process that `exec`s is the same
        // pid with a new name, and `/proc/<pid>/cmdline` has to say the new
        // one from here on.
        task.argv = alloc::sync::Arc::new(argv.to_vec());

        // Reset signal dispositions to SIG_DFL (POSIX requirement).
        task.dispositions = signal::Actions::default();
        task.alt_stack = None;
        task.signals = 0;

        // Close O_CLOEXEC file descriptors.
        {
            use crate::vfs::file::FD_CLOEXEC;
            let fds = &task.fds;
            // Collect fds to close (can't modify while iterating).
            let mut to_close = alloc::vec::Vec::new();
            for fd in 0..1024i32 {
                if let Ok(flags) = fds.fd_flags_get(fd)
                    && flags & FD_CLOEXEC != 0
                {
                    to_close.push(fd);
                }
            }
            // The descriptions are held until the batch is whole, because the
            // question each of them is asked - "is this the last reference?" -
            // can only be answered while every one of them is still counted.
            let mut closing = alloc::vec::Vec::new();
            for fd in to_close {
                if let Ok(file) = fds.clone_at(fd) {
                    closing.push(file);
                }
                let _ = fds.close(fd);
            }
            #[cfg(target_os = "none")]
            {
                // Not the exit hooks: this is `execve`'s `FD_CLOEXEC` pass, and
                // the process lives on. Only these two are told, because a
                // reader's *position* is keyed by an address the new image will
                // reuse - the socket, watch and lock paths have their own
                // handling at close time.
                let owner = task.tgid.0 as u32;
                crate::drivers::virtio::input::close_at_exit(&closing, owner);
                crate::drivers::uinput::close_at_exit(&closing, owner);
            }
            drop(closing);
        }

        // Point the CPU at the new image. Dropping the old address space left it
        // on the kernel's tables - a drop must, since it is about to free the
        // hierarchy the CPU is walking - and the kernel's tables do not map the
        // program's entry point. Nothing will call `prepare` for this task
        // either: it is not being dispatched, it is simply continuing, and the
        // next thing it does is `sysretq` into the image that was just loaded.
        paging::switch_to_root(new_root);

        Ok((loaded.entry, user_rsp))
    })
}

/// Resolve a copy-on-write page fault for the current task.
///
/// If the faulting address is mapped with the COW bit, the page is copied: the
/// task gets a private frame, the old one keeps its other owners. The frame
/// lists have to follow, or the new frame leaks and the old one is freed too many
/// times - so the old frame is swapped for the new in this task's list, and the
/// old frame's owner count is dropped by one.
///
/// Returns `false` if the fault cannot be resolved as COW, leaving the caller to
/// deliver SIGSEGV.
pub fn resolve_cow_fault(virt: usize) -> bool {
    with_table(|t| {
        let id = t.current;
        // The space the running task *translates through*, which for a thread
        // is its group leader's, not its own - a thread has no space of its own
        // at all, and asking its slot would answer "no space" and hand every
        // write fault it ever takes to SIGSEGV. A musl thread's first push is a
        // write to the group's memory, so this is the difference between a
        // thread that runs and one that dies on its first instruction.
        let owner = t.space_owner(id);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return false;
        };
        let mm = task.mm.clone();
        let Some(space) = task.space.as_mut() else {
            return false;
        };
        split_cow(space, &mm, virt)
    })
}

/// Give one copy-on-write page of `space` its own frame, and keep the group's
/// books on the swap. `false` if the page was not copy-on-write, in which case
/// nothing changes.
fn split_cow(space: &mut crate::arch::paging::AddressSpace, mm: &Mm, virt: usize) -> bool {
    let Some((old_phys, new_phys)) = space.resolve_cow(virt) else {
        return false;
    };

    // The page the task now owns is `new_phys`; `old_phys` is one claim
    // lighter and may be free.
    if !mm.replace(old_phys, new_phys) {
        // A page outside the tracked list (the ELF loader's mappings are all
        // tracked, so this is unreachable in practice); record the new frame
        // so at least one of the pair is ever given back.
        mm.own(new_phys);
    }
    crate::mm::free_frames(old_phys, 0);
    true
}

/// Grow the main stack to cover `addr` - the fault Linux answers by mapping one
/// more page of stack.
///
/// The loader maps [`USER_STACK_PAGES`] and stops. That is not the size of a
/// stack, it is where one begins: a program's stack is a region that extends as
/// the program descends into it, which is why a function may use more of it
/// than any fixed default. Without this a process is limited to whatever the
/// loader reserved - 64 KiB - and a program with a deep call or a large local
/// frame dies of a segmentation fault it did nothing to deserve. (`apk add`,
/// whose dependency walk is exactly that shape, was the one that found it.)
///
/// Two bounds keep this from becoming a way to claim memory by accident. The
/// address must be at or under `rsp` and within [`STACK_GROWTH_SLACK`] of it,
/// because a push happens next to the stack pointer and a wild pointer does
/// not; and it must stay above the stack's floor. Anything else is still the
/// process's own fault to bear.
///
/// Both ends of that region are the process's own, not the kernel's: the floor
/// is measured from the stack top the process was *laid out* with, which every
/// program draws afresh at `exec`. A growth check against the old constant would
/// hand a process pages above the stack it was given, in a region its own page
/// tables say nothing about.
pub fn resolve_stack_growth(addr: usize, rsp: usize) -> bool {
    let page = addr & !(PAGE_SIZE - 1);
    if rsp < page || rsp - page > STACK_GROWTH_SLACK {
        return false;
    }
    with_table(|t| {
        let id = t.current;
        let stack_top = t.task(id).stack_top;
        if addr >= stack_top {
            return false;
        }
        // RLIMIT_STACK is the floor, read from the faulting task rather than a
        // compile-time constant: a soft limit below the reserved region stops
        // growth early (SIGSEGV beyond it), and a raised or infinite one is
        // capped at the region actually reserved for the stack.
        let stack_max = t
            .task(id)
            .rlimits
            .get(3)
            .map_or(USER_STACK_MAX as u64, |r| r.cur)
            .min(USER_STACK_MAX as u64) as usize;
        if page < stack_top - stack_max {
            return false;
        }
        // The group's space, for the same reason a copy-on-write fault looks
        // there: a thread's stack is a mapping the group shares, and only the
        // leader's slot holds the tables that describe it.
        let owner = t.space_owner(id);
        let Some(task) = t.tasks[owner.0].as_mut() else {
            return false;
        };
        let Some(space) = task.space.as_mut() else {
            return false;
        };
        let Some(frame) = mm::alloc_frames(0) else {
            return false;
        };
        // A stack page nobody has written reads as zeros. It is a hole in the
        // program's memory, not whatever the frame's last owner left there -
        // and a program that reads uninitialised stack expects zeros.
        let dest = unsafe {
            core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);
        let mapped = space.try_map(Mapping {
            virt: page,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        if mapped.is_err() {
            // Something is already mapped there, so this was not a stack
            // growth fault after all: leave the page as it was.
            mm::free_frames(frame, 0);
            return false;
        }
        task.mm.own(frame);
        // The stack's reported range follows it down, so a program deep in a
        // recursion does not read a `[stack]` line that stops 64 KiB below
        // where it is actually standing. Linux's `[stack]` line covers the
        // VMA, which grows the same way.
        task.mm.grow_stack_range(page);
        true
    })
}

/// Run `f` on the task table with interrupts off.
///
/// `f` must not switch tasks. See the module docs for why that is a rule rather
/// than a preference.
fn with_table<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    critical_section::with(|cs| unsafe { f(&mut *TABLE.borrow(cs).get()) })
}

/// The scheduler task's body. Every scheduling decision is made here, on a stack
/// that belongs to no task and is therefore never freed underneath itself.
fn scheduler_loop() {
    loop {
        let switched = critical_section::with(|cs| {
            // As in `schedule`: one statement, borrow dead before the switch.
            let next = unsafe { (*TABLE.borrow(cs).get()).next_turn() };
            match next {
                Some((from, to)) => {
                    unsafe { context::switch(from, to) };
                    true
                }
                None => false,
            }
        });

        // Outside the borrow above: dropping a description can wake a task, and
        // waking needs the table this loop just let go of.
        drain_release();

        if !switched {
            // Nothing to run. Halt with interrupts on so the next tick wakes us,
            // and let it turn them back off before the table is touched again.
            wait_for_interrupt();
        }
    }
}

/// The `clone(2)` half of the process API.
///
/// `fork` is `clone` with a zero flag word; this is the same copy with the
/// caller's requests honoured. The flag word has already been validated by the
/// syscall layer, so what is left here is the part the flags decide.
///
/// A *thread* - `CLONE_THREAD` - is built by [`spawn_thread`]: a task that runs
/// on the caller's memory rather than a copy of it. What makes that possible is
/// the thread group: the frames behind the mappings belong to the group, which
/// counts its members, so a thread that leaves does not free memory its
/// siblings are still using.
///
/// `CLONE_VM` *without* `CLONE_THREAD` is a different request, and it is the one
/// a libc actually makes in `posix_spawn`: it clones with `CLONE_VM|CLONE_VFORK`
/// and the child's entire job is to `execve` immediately. Such a child only ever
/// *reads* the caller's memory - the path, the argument vector, the file-action
/// list - and reading is what a copy and a share do identically. So the spawn
/// shape is answered with a copy: the contract the caller asked for, met
/// precisely, with no shared page tables to reference-count and no way for the
/// child to outlive the address space it came from.
///
/// `CLONE_VFORK` is accepted and not acted on. Linux suspends the parent until
/// the child execs; with a private copy of memory there is nothing the parent
/// could corrupt by continuing, so suspending it would buy nothing - and every
/// `CLONE_VFORK` caller in practice (`posix_spawn`) waits on the child itself.
pub fn clone(state: &UserContext, request: CloneRequest) -> isize {
    if request.flags & nr::CLONE_THREAD != 0 {
        return spawn_thread(state, request);
    }
    spawn_copy(state, request)
}

/// Create a thread: a task that runs on the calling task's memory, with the
/// calling task's descriptors, and its own stack.
///
/// This is what `fork` would be with nothing copied. The address space is not
/// duplicated and no frames are shared copy-on-write - the new task *is* the
/// same process, one more instruction pointer in it - so the group's frame
/// list stays as it is, and the group's count goes up by one: a thread that
/// leaves must not free memory its siblings are still using.
///
/// The child resumes inside the `clone` call its creator made, with `rax` zero
/// and `rsp` on the stack it was given, which is how a libc's thread entry
/// finds its way back to the function it was asked to run: it set that up on
/// the new stack before calling in.
fn spawn_thread(state: &UserContext, request: CloneRequest) -> isize {
    // A thread with no stack of its own would run on its creator's, and the
    // first thing either of them did would corrupt the other. Linux leaves
    // this to the caller; refusing is the kinder answer.
    if request.child_stack == 0 {
        return crate::err::EINVAL;
    }

    let child_id = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        let mm = parent.mm.clone();
        let fds = parent.fds.clone();
        let tgid = parent.tgid;
        let pgid = parent.pgid;
        let session = parent.session;
        let ctty = parent.ctty;
        let identity = parent.identity.clone();
        // A thread is its process: the same namespaces, by reference. Linux
        // makes the same promise from the other side - `CLONE_THREAD` shares
        // the creator's `nsproxy` - so a thread that unshared would split from
        // its own process, which is not what a thread is.
        let uts = alloc::sync::Arc::clone(&parent.uts);
        let umask = parent.umask;
        let oom_score_adj = parent.oom_score_adj;
        let oom_score_adj_min = parent.oom_score_adj_min;
        let nice = parent.nice;
        let rlimits = parent.rlimits;
        let seccomp_strict = parent.seccomp_strict;
        let seccomp_filters = parent.seccomp_filters.clone();
        let priority = parent.priority;
        let dispositions = parent.dispositions;
        // The new thread's signal mask is its creator's, copied: POSIX says a
        // thread inherits the creating thread's mask, and musl's `SIGEV_THREAD`
        // helper depends on it - `timer_create` blocks `SIGRTMIN` in the
        // creating thread and the helper *thread* is what waits on it, so a
        // thread that started unblocked would take the timer's signal as a
        // fatal default action instead of waiting for it.
        let blocked = parent.blocked;
        let cwd = parent.cwd.clone();
        let cwd_path = parent.cwd_path.clone();
        let brk_base = parent.brk_base;
        let brk_pages = parent.brk_pages;
        let mmap_cursor = parent.mmap_cursor;
        // The main stack belongs to the process, and so does where it ends: a
        // thread that grew one of its own pages past that bound would be growth
        // the leader's layout never reserved room for.
        let stack_top = parent.stack_top;
        let exe = parent.exe.clone();
        let argv = alloc::sync::Arc::clone(&parent.argv);
        let parent_tls = parent.tls;

        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));

        let stack = Stack::new();
        // Interrupts stay off until the thread's `iretq` restores the flags it
        // was created with, exactly as a fork child's does.
        let mut context = unsafe { context::bootstrap(stack.top(), task_entry, slot, false) };
        // The child's floating-point registers are its creator's, read the same
        // way `fork`'s child reads them: from the registers if the parent still
        // holds them, from its saved block if it does not.
        context.fpu = t.inherit_fpu(parent_id);

        t.tasks[slot] = Some(Task {
            context,
            fpu_live: false,
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            // No address space of its own: the group's root is what the
            // dispatcher loads for it.
            space: None,
            // ...but it *is* a user task, and saying so is what makes it
            // visible at all: `user` is the flag `/proc` filters on, and it
            // measures "has an image", not "owns an address space" - a thread
            // runs the group's image. Without it `/proc/<pid>/task` lists the
            // leader alone while `/proc/<pid>/status` counts both, which is the
            // one place a `ps -T` and a `Threads:` line can disagree.
            user: true,
            user_entry: None,
            user_resume: Some(UserContext {
                // The child returns from the call with zero, and lands on the
                // stack the caller provided - the one it pushed `func` and
                // `arg` onto before asking for a thread.
                rax: 0,
                rsp: request.child_stack,
                ..*state
            }),
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            // Inherited from the creator (see where `blocked` is read above):
            // the mask a thread starts with is the one the thread that made it
            // held, which is what makes `timer_create`'s `SIGEV_THREAD`
            // helper able to wait for `SIGRTMIN` at all.
            blocked,
            // Linux shares the handler table under `CLONE_SIGHAND`; this
            // kernel copies it, so a thread that installs a handler does not
            // install it for its siblings. Nothing in this system's userland
            // does that yet, and a copy is the honest version of "not shared".
            dispositions,
            alt_stack: None,
            fds,
            cwd,
            cwd_path,
            // Not a child: `wait` collects processes, and a thread is not one.
            // The process becomes collectable when its leader exits.
            parent: None,
            pgid,
            // A thread is its process: the same session, the same controlling
            // terminal its creator's group holds.
            session,
            ctty,
            // ...and the same identity, group and user words alike, and the
            // same creation mask - a thread creates files as its process does.
            identity,
            uts,
            umask,
            oom_score_adj,
            oom_score_adj_min,
            nice,
            rlimits,
            seccomp_strict,
            seccomp_filters,
            brk_base,
            brk_pages,
            mmap_cursor,
            stack_top,
            exe,
            argv,
            child_tid: if request.flags & nr::CLONE_CHILD_SETTID != 0 {
                request.child_tid
            } else {
                0
            },
            tls: if request.flags & nr::CLONE_SETTLS != 0 {
                request.tls
            } else {
                parent_tls
            },
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
            mm: {
                mm.join();
                mm
            },
            released: false,
            tgid,
        });

        if request.flags & nr::CLONE_CHILD_CLEARTID != 0 {
            // Where a joiner sleeps. The thread clears it on the way out, and
            // wakes whoever is waiting there - which is the whole of what
            // `pthread_join` needs from the kernel.
            t.tasks[slot]
                .as_ref()
                .expect("the thread was just built")
                .mm
                .set_clear_tid(request.child_tid);
        }

        let child_id = TaskId(slot);
        // A thread born into a tracer's hands is stopped before its first
        // instruction for the same reason a process child is - and it matters
        // more here, because a thread that has run once is a thread whose first
        // syscalls are missing from the trace.
        if attach_new_child(t, parent_id, child_id, abi::PTRACE_EVENT_CLONE) {
            t.sched.sleep(child_id, priority, u64::MAX);
        } else {
            t.sched.push_ready(child_id, priority);
        }
        child_id
    });

    child_id.0 as isize
}

/// End every thread of the running task's group, then this one.
///
/// `exit_group(2)` is what a libc calls when the *process* decides to end -
/// `exit`, a return from `main`, an unhandled signal - and Linux ends every
/// thread with it. The siblings are marked exited here; the release pass
/// accounts for each of them, so the group's memory goes when the last one has
/// been accounted for, exactly as if they had all returned on their own.
pub fn exit_group(code: isize) -> ! {
    let me = current();
    let tgid = current_tgid();
    with_table(|t| {
        for slot in 0..t.tasks.len() {
            let id = TaskId(slot);
            if id == me {
                continue;
            }
            let Some(task) = t.tasks[slot].as_mut() else {
                continue;
            };
            if task.tgid != tgid || task.exited {
                continue;
            }
            // Never scheduled again - and taken out of every queue, including
            // the sleep queue, so the picker can never reach a task whose slot
            // the release pass is about to free. Waking it here would queue a
            // dead pid: the memory it accounted for is released by the pass,
            // not by a turn it never gets.
            task.exited = true;
            t.sched.retire(id);
        }
    });
    exit(code)
}

/// Name where the running task's exit is announced: the address a leaving
/// thread writes zero to and wakes, which is what a joiner sleeps on.
pub fn set_clear_tid(address: usize) {
    with_table(|t| t.task(t.current).mm.set_clear_tid(address));
}

/// The process id of the running task: the id of its thread group's leader.
///
/// `getpid` answers this from every thread - a process has one pid however
/// many threads it runs - while `gettid` answers the thread's own id, which is
/// what `gettid` is for.
pub fn current_tgid() -> TaskId {
    with_table(|t| t.task(t.current).tgid)
}

/// Where a fresh task's stack first lands. The arch trampoline calls this with
/// the task's slot number in the first argument register; the body it should
/// run, the register file it should resume, or the user image it should enter is
/// still in the table.
extern "C" fn task_entry(slot: usize) -> ! {
    let (body, user, resume, child_tid) = with_table(|t| {
        let task = t.task_mut(TaskId(slot));
        // The resume state is *borrowed*, not taken: `resume_user` never
        // returns, and the task's slot - and so the state inside it - stays put
        // for as long as the task exists. Copying it onto this stack would be
        // pointless work for a door that only opens one way.
        let resume = task
            .user_resume
            .as_ref()
            .map_or(core::ptr::null(), |state| state as *const UserContext);
        (
            task.body.take(),
            task.user_entry.take(),
            resume,
            task.child_tid,
        )
    });
    if let Some(body) = body {
        body();
        exit(0);
    }
    if !resume.is_null() {
        // `clone(CLONE_CHILD_SETTID)` asked for the child's own tid to be
        // waiting where the child can read it. This runs on the child's address
        // space - the dispatcher has already loaded it - so the write lands
        // exactly where the caller asked.
        if child_tid != 0 {
            unsafe { (child_tid as *mut i32).write_volatile(slot as i32) };
        }
        // A forked child: come back from the `fork` call its parent made, with
        // `rax` zero. One way door, exactly as below.
        unsafe { context::resume_user(resume) };
    }
    if let Some((entry, user_rsp)) = user {
        // One way door: the kernel stack this task arrived on stays behind as
        // nothing but the trap target, and the reaper eventually frees it.
        unsafe { context::enter_user(entry, user_rsp) };
    }
    panic!("task {slot} has neither a body nor a user image")
}
