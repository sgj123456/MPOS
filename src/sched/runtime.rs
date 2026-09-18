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
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{Priority, Scheduler, TaskId};
use crate::sched::signal;
use crate::syscall::linux::nr;
use crate::vfs::RawFd;
use crate::vfs::file::{FdTable, File};

/// How many tasks can exist at once. A hard limit rather than a grown
/// collection, for the reason given in the module docs.
pub const MAX_TASKS: usize = 64;

/// Task stacks are `2^STACK_ORDER` pages. 64 KiB, because the kernel stack has
/// to hold the trap frames that stack up above a task the clock preempted
/// *and* whatever the deepest syscall path needs - and `execve` is by far the
/// deepest: it reads a whole program image, hands it to a parser, and builds a
/// new address space underneath all of that. A debug build's frames are large
/// enough that 16 KiB - Linux's own figure, measured on optimised code - is
/// not enough for it, and the symptom is the worst kind: the overflow eats the
/// fault handler's own frame, so the machine triple-faults with nothing said.
pub const STACK_ORDER: usize = 4;
const STACK_SIZE: usize = PAGE_SIZE << STACK_ORDER;

/// Written at the bottom of every stack, which is where a runaway task arrives
/// last, and checked when the task is reaped. It catches overflow after the fact
/// rather than preventing it; preventing it needs a guard page, and the direct
/// map is not something to punch holes in for one task's benefit.
const STACK_CANARY: u64 = 0x6d70_6f73_5f73_746b;

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
/// How far below the stack pointer a faulting address may be and still be a
/// push. Linux uses the same figure for the same reason: a stack frame is next
/// to `rsp`, and an address a long way below it is a wild pointer that has to
/// stay a fault rather than quietly claim pages.
const STACK_GROWTH_SLACK: usize = 64 * 1024;

// `ARGV_MAX` / `ARGS_BYTES_MAX` live in `policy`: the syscall layer checks
// them on the host too, where this module is not compiled.
use super::policy::{ARGS_BYTES_MAX, ARGV_MAX};

/// Where a dynamically linked program's interpreter is loaded.
///
/// It cannot go at zero with the program: an interpreter is position
/// independent, its segments count from zero, and mapping it there would land
/// it on top of the program's own first pages. Linux gives it a high address
/// out of the way of everything; this is the same idea placed for *this*
/// layout, which has no room at the top - the stack ends at 1 GiB, the mmap
/// region grows down from 0x7f00_0000_0000, and 1.75 GiB is the widest gap
/// left between them.
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

/// Exit codes, kept after the task is gone. The slot a task occupied is free
/// for reuse the moment it is reaped, but the code it exited with can still be
/// wanted - a milestone check reads it one tick later, and `wait` reads it
/// once. Reading it takes it: a code belongs to the one caller that collects
/// it, and anything left over is dropped oldest-first when the cap is reached.
static RETIRED: Mutex<RefCell<Vec<(TaskId, isize)>>> = Mutex::new(RefCell::new(Vec::new()));
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
    /// The thread that started the process: its id is the group's pid, and
    /// `getpid` answers it from any of the threads.
    leader: TaskId,
    /// Where a leaving thread writes zero and wakes whoever waits there -
    /// `CLONE_CHILD_CLEARTID`, which is how a libc's `pthread_join` learns a
    /// thread is gone without polling.
    clear_tid: Mutex<RefCell<Option<usize>>>,
}

impl Mm {
    /// The paging root of the group's address space.
    fn root(&self) -> usize {
        self.root_value
    }

    fn new(leader: TaskId, root: usize, frames: Vec<usize>) -> Arc<Mm> {
        Arc::new(Mm {
            live: core::sync::atomic::AtomicUsize::new(1),
            root_value: root,
            frames: Mutex::new(RefCell::new(frames)),
            leader,
            clear_tid: Mutex::new(RefCell::new(None)),
        })
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
    /// Per-process brk state for heap management. The frames behind the
    /// break's pages are on `user_frames` like every other frame the process
    /// owns; only the bookkeeping of *where* the break is stays here.
    brk_base: usize,
    brk_pages: usize,
    /// Per-process mmap cursor: where the next `mmap(MAP_ANONYMOUS)` that
    /// does not name an address starts looking. Each process starts at
    /// `MMAP_BASE` and only moves forward; no other process shares this.
    mmap_cursor: usize,
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
            mm: Mm::new(TaskId(0), 0, alloc::vec::Vec::new()),
            released: false,
            tgid: TaskId(0),
            body: None,
            space: None,
            user: false,
            user_entry: None,
            user_resume: None,
            signals: 0,
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            cwd_path: None,
            parent: None,
            pgid: TaskId(0),
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
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
        self.current = id;
        (self.context_of(from), self.context_of(id))
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
    fn check_itimers(&mut self) {
        let now = timer::ticks();
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
            if now < state.deadline {
                continue;
            }
            let interval = state.interval_ticks;
            signal::add(&mut task.signals, signal::SIGALRM);
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
    }

    /// The scheduler's whole turn: fire timers, bury the dead, move the
    /// clock on, choose. `None` means there is nothing to run and the caller
    /// should idle.
    fn next_turn(&mut self) -> Option<(*mut Context, *const Context)> {
        self.check_itimers();
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
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            cwd_path: None,
            parent: None,
            pgid: TaskId(0),
            brk_base: 0,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            exe: alloc::string::String::new(),
            argv: alloc::sync::Arc::new(alloc::vec::Vec::new()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
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
                    // Sockets go down with the process that held their last
                    // reference - before the batch is parked for dropping,
                    // while the counts still say who owns them.
                    crate::net::socket::close_sockets_at_exit(&files);
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
        let Some(space) = task.space.as_mut() else {
            return;
        };
        let root = space.root();
        split_cow(space, &mm, address);
        crate::arch::paging::write_user_u32(root, address, 0);
    }

    /// Which task owns the address space a group runs on. A process owns its
    /// own; a thread does not own one at all, so the leader's is the group's.
    fn space_owner(&self, id: TaskId) -> TaskId {
        self.task(id).mm.leader()
    }

    fn reap(&mut self) {
        self.release_exited();
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
) -> Result<(Vec<u8>, Vec<String>), SpawnError> {
    let mut bytes = bytes;
    let mut hops = 0;
    while let Some((interp, argument)) = crate::binfmt::shebang(&bytes) {
        hops += 1;
        if hops > crate::binfmt::SHEBANG_HOPS_MAX {
            // A chain of interpreters that leads back into itself.
            return Err(SpawnError::Loop);
        }
        let mut rewritten: Vec<String> = Vec::with_capacity(argv.len() + 2);
        rewritten.push(interp.clone());
        if let Some(argument) = argument {
            rewritten.push(argument);
        }
        rewritten.extend(argv.iter().cloned());
        argv = rewritten;
        bytes = match start.as_ref() {
            Some(dir) => crate::vfs::read_file_opt(&interp, Some(dir.clone())),
            None => crate::vfs::read_file(&interp),
        }
        .map_err(|_| SpawnError::NoInterpreter)?;
    }
    Ok((bytes, argv))
}

pub fn spawn_init(bytes: &[u8], argv: &[String], env: &[String]) -> Result<TaskId, SpawnError> {
    let (bytes, argv) = resolve_script(bytes.to_vec(), argv.to_vec(), None)?;
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
    let user_rsp = initial_stack_rsp(argv, env, "").ok_or(SpawnError::ArgsTooBig)?;
    // Whether init has an interpreter depends on which init it is. The kernel
    // used to start its own program, statically linked, when the initramfs was
    // the root and no filesystem was up to read a loader from. The machine now
    // hands over to the root filesystem's init - and on this userland that is
    // a link to busybox, which is dynamically linked. Both cases are the same
    // call: an image that names no interpreter reads none.
    let interp_bytes = interpreter_bytes(&image)?;
    let interp = match &interp_bytes {
        Some(bytes) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };

    with_table(|t| {
        let slot = INIT_PID.0;
        assert!(
            t.tasks[slot].is_none(),
            "init's slot {slot} is already taken"
        );
        drop_retired(INIT_PID);
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(&mut space, &image, interp.as_ref());
        write_arg_block(
            loaded.stack_top,
            user_rsp,
            argv,
            env,
            "",
            &image,
            loaded.base,
        );

        let root = space.root();
        let brk_base = image
            .segments
            .iter()
            .map(|s| s.vaddr + s.memsz)
            .max()
            .map_or(0, |end| {
                (end + crate::mm::pframe::PAGE_SIZE - 1) & !(crate::mm::pframe::PAGE_SIZE - 1)
            });

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, true) };

        t.tasks[slot] = Some(Task {
            context,
            priority: Priority::NORMAL,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::new(TaskId(slot), root, loaded.frames),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
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
            brk_base,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            exe: alloc::string::String::new(),
            // The one record of how init was started: `/proc/1/cmdline` and
            // `ps` both read it back, and an empty one reads as a process
            // that has no name at all.
            argv: alloc::sync::Arc::new(argv.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
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
    let user_rsp = initial_stack_rsp(argv, env, exe).ok_or(SpawnError::ArgsTooBig)?;
    // The interpreter, found the same way `execve` finds it. This used to pass
    // none, on the assumption that only `execve` starts dynamic programs -
    // which held only while every program the kernel started itself was static.
    // Alpine's userland is dynamically linked, so the shell the milestones hand
    // the machine to is one of them, and a dynamic program started at its own
    // entry point runs before its relocations are applied: the first call
    // through the GOT jumps to nothing.
    let interp_bytes = interpreter_bytes(&image)?;
    let interp = match &interp_bytes {
        Some(bytes) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };

    with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(&mut space, &image, interp.as_ref());
        // Read before the space moves into the task: a thread switches to
        // this root, and the group is what remembers it.
        let root = space.root();

        let brk_base = image
            .segments
            .iter()
            .map(|s| s.vaddr + s.memsz)
            .max()
            .map_or(0, |end| {
                (end + crate::mm::pframe::PAGE_SIZE - 1) & !(crate::mm::pframe::PAGE_SIZE - 1)
            });

        // The argument block is written last, straight into the top page of
        // the stack this task will land on, through the direct map. The top
        // frame is already in `loaded.frames` - `load_user` reports it
        // separately only so this write knows where the stack's page lives.
        write_arg_block(
            loaded.stack_top,
            user_rsp,
            argv,
            env,
            exe,
            &image,
            loaded.base,
        );

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
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::new(TaskId(slot), root, loaded.frames),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds,
            cwd: t.task(parent).cwd.clone(),
            cwd_path: t.task(parent).cwd_path.clone(),
            parent: Some(parent),
            pgid: t.task(parent).pgid,
            brk_base,
            brk_pages: 0,
            mmap_cursor: crate::syscall::MMAP_BASE,
            exe: alloc::string::String::from(exe),
            argv: alloc::sync::Arc::new(argv.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
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

fn initial_stack_rsp(argv: &[String], env: &[String], exe: &str) -> Option<usize> {
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
    let rsp = (USER_STACK_TOP - total) & !15;
    (USER_STACK_TOP - rsp <= PAGE_SIZE).then_some(rsp)
}

/// Fill the initial stack page with the argument block [`initial_stack_rsp`]
/// laid out: random bytes and strings at the top, auxv, envp and argv
/// pointers and `argc` below them.
///
/// # Safety
///
/// The frame must back the page at `USER_STACK_TOP - PAGE_SIZE`, and
/// `user_rsp` must be the pointer [`initial_stack_rsp`] computed for the same
/// `argv` and `env`, with the whole block inside that one page.
///
/// `exe` is the execution path the `AT_EXECFN` entry points at, the string a
/// libc derives `progname` from by scanning it for NUL. It must be
/// NUL-terminated somewhere in the block: an `argv`-empty spawn with no path
/// of its own falls back to an empty string, never to the random bytes, or
/// the scan walks off the stack.
fn write_arg_block(
    frame: usize,
    user_rsp: usize,
    argv: &[String],
    env: &[String],
    exe: &str,
    image: &elf::Image,
    at_base: usize,
) {
    let page =
        unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE) };
    let from = user_rsp - (USER_STACK_TOP - PAGE_SIZE);
    assert!(
        USER_STACK_TOP - user_rsp <= PAGE_SIZE,
        "argv block is not in the top page"
    );

    let argc = argv.len();
    let mut off = from;

    // argc
    page[off..off + 8].copy_from_slice(&(argc as u64).to_le_bytes());
    off += 8;

    // Random bytes at the very top of the page (16 bytes, 16-byte aligned).
    let random_at = PAGE_SIZE - 16;
    let random_vaddr = (USER_STACK_TOP - PAGE_SIZE) + random_at;
    let random: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];
    page[random_at..random_at + 16].copy_from_slice(&random);

    // Strings, descending from just below the random bytes: argv first, then
    // the environment, then the AT_EXECFN path.
    let mut string_at = random_at;
    let mut argv0_vaddr = 0usize;
    for (i, arg) in argv.iter().enumerate() {
        string_at -= arg.len() + 1;
        page[string_at..string_at + arg.len()].copy_from_slice(arg.as_bytes());
        page[string_at + arg.len()] = 0;
        // argv[i] pointer: virtual address of the string.
        let vaddr = (USER_STACK_TOP - PAGE_SIZE) + string_at;
        if i == 0 {
            argv0_vaddr = vaddr;
        }
        page[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL argv terminator (zero-filled page).
    off += size_of::<usize>();

    for var in env.iter() {
        string_at -= var.len() + 1;
        page[string_at..string_at + var.len()].copy_from_slice(var.as_bytes());
        page[string_at + var.len()] = 0;
        let vaddr = (USER_STACK_TOP - PAGE_SIZE) + string_at;
        page[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL envp terminator (zero-filled page).
    off += size_of::<usize>();

    // The AT_EXECFN string: the path the program was executed through when
    // the caller knew it, argv[0] itself when it did not, and an empty
    // string when neither exists - the block must never point the libc's
    // progname scan at unterminated data.
    let execfn_vaddr = if !exe.is_empty() {
        string_at -= exe.len() + 1;
        page[string_at..string_at + exe.len()].copy_from_slice(exe.as_bytes());
        page[string_at + exe.len()] = 0;
        (USER_STACK_TOP - PAGE_SIZE) + string_at
    } else if argv0_vaddr != 0 {
        argv0_vaddr
    } else {
        string_at -= 1;
        page[string_at] = 0;
        (USER_STACK_TOP - PAGE_SIZE) + string_at
    };

    // Auxiliary vector entries.
    auxv(&mut page[off..], image, at_base, random_vaddr, execfn_vaddr);
}

/// Write the Linux auxiliary vector into `buf` as (type, value) u64 pairs.
///
/// The ids are the Linux `AT_*` numbers, so a Linux-built libc reads the
/// vector without translation: program headers for TLS setup, page size and
/// clock tick for `sysconf`, the identity fields for `getuid`-style calls,
/// and 16 random bytes at `random_vaddr` for the stack guard. `execfn` is
/// the virtual address of `argv[0]`'s string, what `AT_EXECFN` points at.
///
/// `at_base` is where the program interpreter was loaded, and is the one
/// entry a dynamically linked program cannot do without: it is how the
/// interpreter finds itself, since nothing told it where it was put. Zero
/// means the program is static.
fn auxv(buf: &mut [u8], image: &elf::Image, at_base: usize, random_vaddr: usize, execfn: usize) {
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

    entry!(3, image.phdr_addr); // AT_PHDR
    entry!(4, image.phent); // AT_PHENT
    entry!(5, image.phnum); // AT_PHNUM
    entry!(6, PAGE_SIZE); // AT_PAGESZ
    entry!(7, at_base); // AT_BASE (0 = no dynamic linker)
    entry!(9, image.entry); // AT_ENTRY
    entry!(11, 0usize); // AT_UID
    entry!(12, 0usize); // AT_EUID
    entry!(13, 0usize); // AT_GID
    entry!(14, 0usize); // AT_EGID
    entry!(16, 0usize); // AT_HWCAP
    entry!(17, 100usize); // AT_CLKTCK
    entry!(23, 0usize); // AT_SECURE (0 = not setuid)
    entry!(25, random_vaddr); // AT_RANDOM
    entry!(31, execfn); // AT_EXECFN
    entry!(0, 0usize); // AT_NULL
    let _ = off;
}

/// Where the pieces of a loaded program ended up, which is what the initial
/// stack has to describe to the libc that reads it.
struct Loaded {
    /// The address control is transferred to. With an interpreter this is the
    /// interpreter's entry point rather than the program's: the interpreter
    /// is what finishes setting the process up, and it finds the program
    /// through `AT_ENTRY`.
    entry: usize,
    /// The interpreter's load address, reported on the initial stack as
    /// `AT_BASE`. Zero when the program is static, which is what the ABI uses
    /// to mean "there is no interpreter".
    base: usize,
    /// Every frame that was mapped for this process, for the reaper to give
    /// back when it exits.
    frames: Vec<usize>,
    /// The frame holding the top page of the stack, where the argument block
    /// is written.
    stack_top: usize,
}

/// Map every page of `image` into `space` at `base` - file bytes, the zero
/// tail of each segment past them - and record the frames as it goes.
///
/// `base` is the load address. It is zero for the program itself, whose
/// headers name absolute addresses, and [`INTERP_BASE`] for an interpreter,
/// which is position independent and must not land on the program.
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

/// Apply `image`'s R_X86_64_RELATIVE relocations, each writing `addend` at
/// `offset` through the direct map.
///
/// Only the program gets this treatment; an interpreter relocates itself from
/// `AT_BASE` before it does anything else, and a second pass here would be
/// doing a libc's job with a different idea of where it starts.
fn apply_rela(space: &AddressSpace, image: &Image<'_>) {
    for &(offset, addend) in &image.rela {
        if let Some(phys) = space.phys_for_virt(offset) {
            let ptr =
                (mm::phys_to_virt(phys & !(PAGE_SIZE - 1)) + (offset % PAGE_SIZE)) as *mut u64;
            unsafe {
                ptr.write(addend as u64);
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
fn interpreter_bytes(image: &Image<'_>) -> Result<Option<Vec<u8>>, SpawnError> {
    let Some(path) = &image.interp else {
        return Ok(None);
    };
    let path = core::str::from_utf8(path).map_err(|_| SpawnError::NotExecutable)?;
    // A missing or unreadable interpreter reads as "the file was not found":
    // the program itself was fine, and what could not be followed is the path
    // it names.
    crate::vfs::read_file(path)
        .map(Some)
        .map_err(|_| SpawnError::NoInterpreter)
}

/// Lay out a program - and its interpreter, when it has one - in `space`, map
/// the user stack, and report where everything landed.
///
/// The interpreter is loaded first and at [`INTERP_BASE`]; the program follows
/// at the addresses its own headers name. Nothing here tries to link the two
/// together beyond handing over `AT_BASE`, `AT_PHDR` and `AT_ENTRY` on the
/// initial stack: resolving symbols, mapping shared libraries and jumping to
/// the program's entry is the interpreter's whole purpose, and a kernel that
/// did any of it would be guessing at a contract it is not party to.
fn load_user(space: &mut AddressSpace, image: &Image<'_>, interp: Option<&Image<'_>>) -> Loaded {
    let mut frames = Vec::new();

    let base = match interp {
        Some(interp) => {
            map_image(space, interp, INTERP_BASE, &mut frames);
            INTERP_BASE
        }
        None => 0,
    };

    map_image(space, image, 0, &mut frames);
    apply_rela(space, image);

    let mut stack_top = 0;
    for index in 0..USER_STACK_PAGES {
        let frame = mm::alloc_frames(0).expect("out of frames for the user stack");
        frames.push(frame);
        let dest = unsafe {
            core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);
        space.map(Mapping {
            // Index 0 is the highest page, the one `rsp` starts in and the
            // one the argument block is written into below.
            virt: USER_STACK_TOP - (index + 1) * PAGE_SIZE,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        if index == 0 {
            stack_top = frame;
        }
    }

    // Control goes to the interpreter when there is one, and to the program
    // otherwise. The interpreter's own entry is a vaddr in a shared object,
    // so it only means anything once the base is added.
    let entry = match interp {
        Some(interp) => INTERP_BASE + interp.entry,
        None => image.entry,
    };

    Loaded {
        entry,
        base,
        frames,
        stack_top,
    }
}

/// Give up the rest of this slice and go to the back of the band.
pub fn yield_now() {
    schedule(Table::requeue_current);
}

/// Sleep for at least `ticks` clock ticks.
pub fn sleep_ticks(ticks: u64) {
    schedule(move |t| {
        // Read the clock here rather than trusting the scheduler's last turn.
        // This task may have been running for a while since then, and a deadline
        // computed from a stale clock wakes early.
        t.sched.advance(timer::ticks());
        let id = t.current;
        let priority = t.task(id).priority;
        t.sched.sleep(id, priority, ticks);
    });
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
            let done =
                cond() || table_has_deliverable(t) || timer::ticks() >= deadline;
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

/// Whether the running task has a signal that would interrupt a blocking
/// wait: pending, unblocked, and not ignored. The table-borrowing shape of
/// [`has_deliverable_signal`], for a check inside `schedule`, where the table
/// is already borrowed and no second borrow may be taken.
fn table_has_deliverable(t: &Table) -> bool {
    let task = t.task(t.current);
    let mut ready = task.signals & !task.blocked;
    while let Some(sig) = signal::take(&mut ready) {
        if task.dispositions.get(sig).is_ignore() {
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
/// exists to build its frame on. Ignored signals are not pending
/// interruptions and do not count, same as in [`take_deliverable_signal`].
pub fn has_deliverable_signal() -> bool {
    with_table(|t| table_has_deliverable(t))
}

/// Take a child's exit code if one is waiting, and report `None` - not
/// "keep waiting" - when it is not. The non-blocking half of `wait`: this is
/// how a shell reaps jobs it started in the background without stopping for
/// any of them.
pub fn poll_any_child(parent: TaskId) -> Option<(TaskId, isize)> {
    with_table(|t| any_child_code(t, parent))
        .and_then(|(id, _)| take_exit_code(id).map(|code| (id, code)))
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

/// Send `sig` to `id`. Returns `false` when the task does not exist or has
/// already exited, which the syscall layer reports as `ESRCH`.
///
/// `sig` of 0 is the POSIX existence probe: it delivers nothing and only
/// reports liveness. With no handler machinery there is nothing to deliver to
/// anyway: the bit waits in the task's pending set, and the task dies at its
/// next blocking point.
pub fn send_signal(id: TaskId, sig: u8) -> bool {
    with_table(|t| {
        let Some(slot) = t.tasks.get_mut(id.0) else {
            return false;
        };
        let Some(task) = slot.as_mut() else {
            return false;
        };
        if task.exited {
            return false;
        }
        if sig != 0 {
            signal::add(&mut task.signals, sig);
            t.sched.wake(id);
        }
        true
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
            return crate::syscall::err::ESRCH;
        };
        let (parent, leader_of) = (target.parent, target.pgid);
        if pgid.0 >= MAX_TASKS {
            return crate::syscall::err::EPERM;
        }
        // A caller may move itself, or a child that has not yet exec'd - and
        // the second is the case a shell actually uses.
        if id != t.current && parent != Some(t.current) {
            return crate::syscall::err::ESRCH;
        }
        // A group leader cannot leave the group it leads: the group is named
        // after it, and there would be nothing left to name.
        if leader_of == id && id != pgid {
            return crate::syscall::err::EPERM;
        }
        // The group must already exist - either because this task is about to
        // become its leader, or because somebody is in it.
        let exists = pgid == id || t.tasks.iter().flatten().any(|task| task.pgid == pgid);
        if !exists {
            return crate::syscall::err::EPERM;
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

/// Take one pending signal off the running task if it is in `mask`, whatever
/// its disposition says, and drop it from the pending set.
///
/// This is the `sigtimedwait` half of the signal machinery, and it is
/// deliberately different from [`take_deliverable_signal`]: a signal consumed
/// here never runs a handler and never takes its default action - not even for
/// `SIGKILL`, whose whole point is that it cannot be caught anywhere else. A
/// blocked signal in `mask` is exactly the one that can be waited for, which
/// is what makes `sigwait` work.
pub fn take_signal_in_set(mask: signal::Pending) -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        let task = t.task_mut(id);
        let mut ready = task.signals & mask;
        match signal::take(&mut ready) {
            Some(sig) => {
                task.signals &= !signal::bit(sig);
                Some(sig)
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

/// Replace the running task's blocked set. `rt_sigprocmask` and the return
/// from a handler are its two callers.
pub fn set_current_blocked(mask: signal::Pending) {
    with_table(|t| {
        let id = t.current;
        t.task_mut(id).blocked = mask;
    });
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
    let (phys, flags) = crate::arch::paging::translate(virt)?;
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
        record_retired(id, code);

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
            if t.task(parent).dispositions.get(signal::SIGCHLD).is_handler() {
                signal::add(&mut t.task_mut(parent).signals, signal::SIGCHLD);
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
            .find(|(retired, _)| *retired == id)
            .map(|(_, code)| *code)
    })
}

/// Collect this process's `pid`'s exit code, consuming the record. `None` when
/// no death has been recorded for that pid since its slot was last reused.
pub fn take_exit_code(id: TaskId) -> Option<isize> {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        let at = retired.iter().rposition(|(retired, _)| *retired == id)?;
        // The code is collected - so the signal about it can never be read
        // again, and a stale entry must not name a slot a new task owns.
        SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != id);
        Some(retired.remove(at).1)
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

pub fn wait_any_child(parent: TaskId) -> Option<(TaskId, isize)> {
    // The condition is the table itself, so it is asked through the borrow
    // `block_until_on` already holds rather than taking one of its own.
    block_until_on(|t| any_child_code(t, parent).is_some());
    // Taken, not looked up again: a second lookup by pid would race the slot
    // being handed to a new task, and the record would go to it instead.
    let id = with_table(|t| any_child_code(t, parent))?.0;
    take_exit_code(id).map(|code| (id, code))
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
            .find(|(id, _)| {
                // The slot is spoken for by a live task: the record describes
                // an older tenant of the same number, so it is not this
                // parent's business.
                let free = t.tasks[id.0].is_none();
                let ours = free
                    || t.tasks[id.0]
                        .as_ref()
                        .is_some_and(|task| task.parent == Some(parent));
                ours && *id != parent
            })
            .map(|(id, code)| (*id, *code))
    })
}

/// Give up on a record nobody will ever collect, so it cannot be mistaken for
/// a fresh death by the next `wait`. Used by the boot task for init, which it
/// hands to the system and never reaps.
pub fn discard_exit_code(id: TaskId) {
    take_exit_code(id);
}

fn record_retired(id: TaskId, code: isize) {
    critical_section::with(|cs| {
        let mut retired = RETIRED.borrow(cs).borrow_mut();
        if retired.len() == RETIRED_CAP {
            let dropped = retired.remove(0);
            // A code nobody will collect has no signal left to read either.
            SIGCHLD_PENDING.borrow(cs).borrow_mut().retain(|p| *p != dropped.0);
        }
        retired.push((id, code));
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
            .any(|(retired, _)| *retired == id)
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
            .retain(|(retired, _)| *retired != id);
    });
}

/// Whether `id` names a live, un-exited task whose parent is `parent` - the
/// check that keeps `wait` from blocking on someone else's child. A task that
/// has already exited answers false here, but its code is in the retired
/// records for the caller to read.
pub fn is_child(id: TaskId, parent: TaskId) -> bool {
    with_table(|t| {
        t.tasks[id.0]
            .as_ref()
            .is_some_and(|task| !task.exited && task.parent == Some(parent))
    })
}

/// The running task.
pub fn current() -> TaskId {
    with_table(|t| t.current)
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
    let open: alloc::vec::Vec<(i32, alloc::sync::Arc<crate::vfs::File>)> = with_table(|t| {
        match t.tasks.get(pid).and_then(Option::as_ref) {
            Some(task) if task.user => task.fds.open_files(),
            _ => alloc::vec::Vec::new(),
        }
    });
    open.into_iter()
        .map(|(fd, file)| (fd as usize, file.display_name()))
        .collect()
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
            None => return -crate::syscall::err::EINVAL,
        };
        match mapped {
            Ok(()) => {
                // The frame now belongs to the process: the reaper returns it
                // once the last thread is gone, not before.
                task.mm.own(frame);
                virt as isize
            }
            Err(crate::arch::paging::PageTaken) => -crate::syscall::err::EEXIST,
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
    with_table(|t| t.task(current()).space.as_ref()?.phys_for_virt(virt))
}

/// Whether every page of `virt..virt + len` is still unmapped in the current
/// task's address space.
///
/// This is what decides whether an `mmap` address is a hint worth taking, and
/// whether a `MAP_FIXED` request has anything to replace.
pub fn user_range_free(virt: usize, len: usize) -> bool {
    with_table(|t| {
        let owner = t.space_owner(current());
        let task = t.task(owner);
        let Some(space) = task.space.as_ref() else {
            return false;
        };
        (0..len)
            .step_by(crate::mm::pframe::PAGE_SIZE)
            .all(|offset| space.phys_for_virt(virt + offset).is_none())
    })
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
            return crate::syscall::err::ENOMEM;
        }
        let grown_to = task.brk_base + task.brk_pages * PAGE_SIZE;
        if frontier < grown_to {
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
                None => return crate::syscall::err::ENOMEM,
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
                return crate::syscall::err::ENOMEM;
            }
            task.mm.own(frame);
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
        )
    });

    // A task with no address space has no image to copy: `fork` from a kernel
    // task is meaningless, and Linux answers ENOMEM for a process it cannot
    // duplicate.
    if !parent_has_space {
        return crate::syscall::err::ENOMEM;
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
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, false) };

        // Create the child's address space by sharing all parent pages COW.
        let mut child_space = AddressSpace::new();
        let mut child_frames = Vec::new();
        let child_root = child_space.root();

        for &(virt, phys, flags) in &parent_pages {
            // Two address spaces now own this frame; the count keeps it alive
            // until the last of them lets go.
            crate::mm::clone_frame_ref(phys);
            child_frames.push(phys);
            // Map it in the child as COW: read-only for now, with the COW bit
            // marking it for the fault handler.
            child_space.map_cow(virt, phys, flags);
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
        if let Some(parent_space) = &mut t.task_mut(parent_id).space {
            for &(virt, _phys, flags) in &parent_pages {
                if flags.contains(MapFlags::COW) || !flags.contains(MapFlags::WRITE) {
                    continue;
                }
                // Keep everything the page had except write permission: hardening
                // it to `READ | USER` would silently take away execute on a page
                // that is both, and a COW copy made from it later would stay
                // non-executable forever.
                parent_space.protect(virt, crate::mm::PAGE_SIZE, flags & !MapFlags::WRITE);
                parent_space.set_cow_bit(virt);
            }
        }

        t.tasks[slot] = Some(Task {
            context,
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
            mm: Mm::new(TaskId(slot), child_root, child_frames),
            released: false,
            tgid: TaskId(slot),
            signals: 0,
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: Arc::new(parent_fds.duplicate()),
            cwd: parent_cwd,
            cwd_path: parent_cwd_path,
            parent: Some(parent_id),
            pgid: t.task(parent_id).pgid,
            brk_base: parent_brk_base,
            brk_pages: parent_brk_pages,
            mmap_cursor: parent_mmap_cursor,
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
        });

        let child_id = TaskId(slot);
        t.sched.push_ready(child_id, parent_priority);
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
    let user_rsp = initial_stack_rsp(argv, env, exe).ok_or(SpawnError::ArgsTooBig)?;

    // A dynamically linked program names the interpreter that has to run
    // before it does. That is a path into the filesystem, and this is the only
    // place with both the path and the authority to follow it - which is why
    // the interpreter is read here rather than inside the loader.
    let interp_bytes = interpreter_bytes(&image)?;
    let interp = match &interp_bytes {
        Some(bytes) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };

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

        // --- Build new address space ---
        let mut space = AddressSpace::new();
        let loaded = load_user(&mut space, &image, interp.as_ref());

        let brk_base = image
            .segments
            .iter()
            .map(|s| s.vaddr + s.memsz)
            .max()
            .map_or(0, |end| {
                (end + crate::mm::pframe::PAGE_SIZE - 1) & !(crate::mm::pframe::PAGE_SIZE - 1)
            });

        write_arg_block(
            loaded.stack_top,
            user_rsp,
            argv,
            env,
            exe,
            &image,
            loaded.base,
        );

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
        task.mm = Mm::new(id, new_root, loaded.frames);
        task.brk_base = brk_base;
        task.brk_pages = 0;
        task.mmap_cursor = crate::syscall::MMAP_BASE;
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
            for fd in to_close {
                let _ = fds.close(fd);
            }
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
pub fn resolve_stack_growth(addr: usize, rsp: usize) -> bool {
    if addr >= USER_STACK_TOP {
        return false;
    }
    let page = addr & !(PAGE_SIZE - 1);
    if rsp < page || rsp - page > STACK_GROWTH_SLACK {
        return false;
    }
    if page < USER_STACK_TOP - USER_STACK_MAX {
        return false;
    }
    with_table(|t| {
        let id = t.current;
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
/// A *thread* is the one thing this cannot build. `CLONE_THREAD` means the child
/// shares the caller's address space and signal handlers - which is what makes a
/// shared variable a shared variable - and this kernel's address space belongs
/// to exactly one task: two owners would free each other's frames on the way
/// out. Sharing one honestly needs reference-counted page tables, so a thread is
/// refused with `ENOSYS` rather than answered with a child that only looks like
/// one and would break the first time two threads touched the same word.
///
/// `CLONE_VM` *without* `CLONE_THREAD` is a different request, and it is the one
/// a libc actually makes: `posix_spawn` clones with `CLONE_VM|CLONE_VFORK`, and
/// the child's entire job is to `execve` immediately. Such a child only ever
/// *reads* the caller's memory - the path, the argument vector, the file-action
/// list - and reading is exactly what a copy-on-write image and a shared one do
/// identically. So the spawn shape is answered with a copy: the contract the
/// caller asked for, met precisely, with no shared page tables to reference-count
/// and no way for the child to outlive the address space it came from.
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
        return crate::syscall::err::EINVAL;
    }

    let child_id = with_table(|t| {
        let parent_id = t.current;
        let parent = t.task(parent_id);
        let mm = parent.mm.clone();
        let fds = parent.fds.clone();
        let tgid = parent.tgid;
        let pgid = parent.pgid;
        let priority = parent.priority;
        let dispositions = parent.dispositions;
        let cwd = parent.cwd.clone();
        let cwd_path = parent.cwd_path.clone();
        let brk_base = parent.brk_base;
        let brk_pages = parent.brk_pages;
        let mmap_cursor = parent.mmap_cursor;
        let exe = parent.exe.clone();
        let argv = alloc::sync::Arc::clone(&parent.argv);
        let parent_tls = parent.tls;

        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));

        let stack = Stack::new();
        // Interrupts stay off until the thread's `iretq` restores the flags it
        // was created with, exactly as a fork child's does.
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, false) };

        t.tasks[slot] = Some(Task {
            context,
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            // No address space of its own: the group's root is what the
            // dispatcher loads for it.
            space: None,
            user: false,
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
            blocked: 0,
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
            brk_base,
            brk_pages,
            mmap_cursor,
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
        t.sched.push_ready(child_id, priority);
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
