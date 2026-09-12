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
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use crate::arch::context::{self, Context};
use crate::arch::gdt;
use crate::arch::paging::{self, AddressSpace};
use crate::arch::{idle, timer};
use crate::mm::elf::{self, Image};
use crate::mm::vm::{MapFlags, Mapping};
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{Priority, Scheduler, TaskId};
use crate::sched::signal;
use crate::vfs::RawFd;
use crate::vfs::file::FdTable;

/// How many tasks can exist at once. A hard limit rather than a grown
/// collection, for the reason given in the module docs.
pub const MAX_TASKS: usize = 64;

/// Task stacks are `2^STACK_ORDER` pages. 16 KiB is what Linux gives a kernel
/// thread, and it has to hold the trap frames that stack up above a task the
/// clock preempted.
pub const STACK_ORDER: usize = 2;
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

/// Set once [`init`] has built the table. Before that the clock still ticks but
/// [`preempt`] has nothing to switch to and must leave the running code alone,
/// which is what lets every milestone before this one keep working unchanged.
static STARTED: AtomicBool = AtomicBool::new(false);

static TABLE: Mutex<Shared> = Mutex::new(Shared::new(Table::new()));

/// Where a user stack ends. Programs are linked around 0x400000, so 1 GiB
/// leaves the whole image below it and everything in between for a heap once
/// user memory grows one (M11+). The stack itself grows down from here.
const USER_STACK_TOP: usize = 0x4000_0000;
const USER_STACK_PAGES: usize = 16;

/// The most strings a user process may be handed, and the most bytes they may
/// take. The syscall layer enforces the same limits on the argv blob it reads
/// from ring 3, so these bounds decide `spawn()`'s error cases too.
pub const ARGV_MAX: usize = 16;
pub const ARGS_BYTES_MAX: usize = 512;

/// Why a user image refused to spawn. The syscall layer maps these onto
/// errnos; nothing here panics, because a bad path from ring 3 is not a
/// kernel bug.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The bytes are not an ELF64 image this kernel can load.
    NotExecutable,
    /// The argument strings do not fit the initial stack.
    ArgsTooBig,
    /// A descriptor the starter asked the child to be born with names nothing
    /// in the starter's own table.
    BadFd,
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
    /// Where a user task's first dispatch goes: the entry point and the stack
    /// `enter_user` hands the CPU. Kernel tasks have none.
    user_entry: Option<(usize, usize)>,
    /// Physical frames behind the user mappings - program pages and stack.
    /// Dropping the address space frees its tables but never these, so the
    /// reaper returns them by hand.
    user_frames: Vec<usize>,
    /// Signals sent to this task and not yet delivered. There are no handlers
    /// yet, so delivery means the task dies at its next blocking point.
    signals: signal::Pending,
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
    /// The task that spawned this one in ring 3, if any. Its `wait` blocks on
    /// this task's exit; kernel-spawned tasks have none.
    parent: Option<TaskId>,
    /// Per-process brk state for heap management. The frames behind the
    /// break's pages are on `user_frames` like every other frame the process
    /// owns; only the bookkeeping of *where* the break is stays here.
    brk_base: usize,
    brk_pages: usize,
    /// The path this process was started from - what `/proc/self/exe` reads
    /// through. Empty for tasks that never had an image (kernel tasks, the
    /// boot task).
    exe: alloc::string::String,
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
            body: None,
            space: None,
            user_entry: None,
            user_frames: Vec::new(),
            signals: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            parent: None,
            brk_base: 0,
            brk_pages: 0,
            exe: alloc::string::String::new(),
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
    /// CR3, its kernel stack under `RSP0` for traps from ring 3.
    fn prepare(&mut self, id: TaskId) {
        let task = self.task(id);
        let root = match &task.space {
            Some(space) => space.root(),
            None => paging::kernel_root(),
        };
        paging::switch_to_root(root);
        if let Some(stack) = &task.stack {
            gdt::set_kernel_stack(stack.top());
        }
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

    /// The scheduler's whole turn: bury the dead, move the clock on, choose.
    /// `None` means there is nothing to run and the caller should idle.
    fn next_turn(&mut self) -> Option<(*mut Context, *const Context)> {
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
            user_entry: None,
            user_frames: Vec::new(),
            signals: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: crate::vfs::file::kernel_fds(),
            cwd: None,
            parent: None,
            brk_base: 0,
            brk_pages: 0,
            exe: alloc::string::String::new(),
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
    fn reap(&mut self) {
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

            // The address space first: if its CR3 is the one loaded, dropping
            // it switches back to the kernel's own before any page table is
            // freed. The drop frees tables only, so the frames the mappings
            // pointed at come back separately below.
            drop(task.space.take());
            for frame in task.user_frames.drain(..) {
                mm::free_frames(frame, 0);
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
    try_spawn_user(bytes, priority, current(), &[], &[], "")
        .expect("spawn_user: not an ELF64 executable mpos can load")
}

/// Start the very first user process: init, in the slot that makes it pid 1.
///
/// Nothing spawns it, so nothing is its parent until it has children of its
/// own; the boot task keeps no link, because it has no `wait` to collect with
/// and a record nobody reads is a record that lies to the next `wait`.
pub fn spawn_init(bytes: &[u8], argv: &[String]) -> Result<TaskId, SpawnError> {
    let id = try_spawn_init(bytes, argv)?;
    assert_eq!(id, INIT_PID, "init did not land on pid 1");
    Ok(id)
}

/// [`spawn_init`] without the pid assertion, so the loader can be exercised
/// before the slot layout is taken as given.
pub fn try_spawn_init(bytes: &[u8], argv: &[String]) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_init() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    let user_rsp = initial_stack_rsp(argv).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = INIT_PID.0;
        assert!(
            t.tasks[slot].is_none(),
            "init's slot {slot} is already taken"
        );
        drop_retired(INIT_PID);
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let (entry, frames, stack_top_frame) = load_user(&mut space, &image);
        write_arg_block(stack_top_frame, user_rsp, argv, &image);

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
            user_entry: Some((entry, user_rsp)),
            user_frames: frames,
            signals: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: Arc::new(FdTable::with_standard(crate::vfs::tty::global())),
            cwd: None,
            parent: None,
            brk_base,
            brk_pages: 0,
            exe: alloc::string::String::new(),
        });
        t.sched.push_ready(INIT_PID, Priority::NORMAL);
        Ok(INIT_PID)
    })
}

/// [`spawn_user`] with arguments and a parent: the process form the `spawn`
/// syscall uses. `argv` is the full argument vector, `argv[0]` being the
/// program name as POSIX frames it - it does not have to match `bytes`' path.
/// `exe` is the path `bytes` was read from, and is what `/proc/self/exe`
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
    redirects: &[Redirect],
    exe: &str,
) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_user() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    let user_rsp = initial_stack_rsp(argv).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let (entry, frames, stack_top_frame) = load_user(&mut space, &image);

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
        // frame is already in `frames` - `load_user` returns it separately
        // only so this write knows where the stack's page lives.
        write_arg_block(stack_top_frame, user_rsp, argv, &image);

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
            user_entry: Some((entry, user_rsp)),
            user_frames: frames,
            signals: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds,
            cwd: t.task(parent).cwd.clone(),
            parent: Some(parent),
            brk_base,
            brk_pages: 0,
            exe: alloc::string::String::from(exe),
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
        if tty.is_foreground(parent) {
            tty.set_foreground(Some(id));
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
/// argc | argv[0..n] | NULL | NULL envp |
/// AT_PHDR | AT_PHNUM | AT_PHENT | AT_ENTRY | AT_RANDOM | AT_PAGESZ | AT_NULL |
/// random bytes (16) | argv strings
/// ```
///
/// The extra auxv entries let musl-based libc (and other Linux-compatible
/// runtimes) find the program headers and initialise TLS on startup.
fn initial_stack_rsp(argv: &[String]) -> Option<usize> {
    if argv.len() > ARGV_MAX || argv.iter().map(|s| s.len() + 1).sum::<usize>() > ARGS_BYTES_MAX {
        return None;
    }
    let strings: usize = argv.iter().map(|s| s.len() + 1).sum();
    let argc = argv.len();
    // argc, argv[0..argc], argv NULL, envp NULL, 7 auxv pairs (14 words).
    let words_bytes = (argc + 17) * size_of::<usize>();
    // 16 bytes of random data for the AT_RANDOM auxv entry.
    let random_bytes = 16usize;
    let total = words_bytes + random_bytes + strings;
    let rsp = (USER_STACK_TOP - total) & !15;
    (USER_STACK_TOP - rsp <= PAGE_SIZE).then_some(rsp)
}

/// Fill the initial stack page with the argument block [`initial_stack_rsp`]
/// laid out: random bytes and strings at the top, auxv, argv pointers and
/// `argc` below them.
///
/// # Safety
///
/// The frame must back the page at `USER_STACK_TOP - PAGE_SIZE`, and
/// `user_rsp` must be the pointer [`initial_stack_rsp`] computed for the same
/// `argv`, with the whole block inside that one page.
fn write_arg_block(frame: usize, user_rsp: usize, argv: &[String], image: &elf::Image) {
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

    // Strings, descending from just below the random bytes.
    let mut string_at = random_at;
    for arg in argv.iter() {
        string_at -= arg.len() + 1;
        page[string_at..string_at + arg.len()].copy_from_slice(arg.as_bytes());
        page[string_at + arg.len()] = 0;
        // argv[i] pointer: virtual address of the string.
        let vaddr = (USER_STACK_TOP - PAGE_SIZE) + string_at;
        page[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }

    // NULL argv terminator and NULL envp terminator (zero-filled page).
    off += 2 * size_of::<usize>();

    // Auxiliary vector entries.
    auxv(&mut page[off..], image, random_vaddr);
}

/// Write the Linux auxiliary vector into `buf` as (type, value) u64 pairs.
fn auxv(buf: &mut [u8], image: &elf::Image, random_vaddr: usize) {
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
    entry!(5, image.phnum); // AT_PHNUM
    entry!(4, image.phent); // AT_PHENT
    entry!(9, image.entry); // AT_ENTRY
    entry!(25, random_vaddr); // AT_RANDOM
    entry!(6, PAGE_SIZE); // AT_PAGESZ
    entry!(0, 0usize); // AT_NULL
    let _ = off;
}

/// Map every page of `image` into `space` - file bytes, the zero tail of each
/// segment past them, and the user stack - and hand back the entry point along
/// with the physical frames the reaper must return, the stack's top frame
/// last, separate.
fn load_user(space: &mut AddressSpace, image: &Image<'_>) -> (usize, Vec<usize>, usize) {
    let mut frames = Vec::new();

    for segment in &image.segments {
        let first = pframe::align_down(segment.vaddr, PAGE_SIZE);
        let limit = pframe::align_up(segment.vaddr + segment.memsz, PAGE_SIZE);
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
            let file_end = segment.vaddr + segment.data.len();
            let from = page.max(segment.vaddr);
            let to = (page + PAGE_SIZE).min(file_end);
            if from < to {
                dest[from - page..to - page]
                    .copy_from_slice(&segment.data[from - segment.vaddr..to - segment.vaddr]);
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

    // Apply R_X86_64_RELATIVE relocations.  Each writes `addend` at `offset`
    // in the user's virtual address space through the direct map.
    if !image.rela.is_empty() {
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

    let mut stack_top_frame = 0;
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
            stack_top_frame = frame;
        }
    }

    (image.entry, frames, stack_top_frame)
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
        if let Some(sig) = take_pending_signal() {
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
        if let Some(sig) = take_pending_signal() {
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

/// Take a child's exit code if one is waiting, and report `None` - not
/// "keep waiting" - when it is not. The non-blocking half of `wait`: this is
/// how a shell reaps jobs it started in the background without stopping for
/// any of them.
pub fn poll_any_child(parent: TaskId) -> Option<(TaskId, isize)> {
    with_table(|t| any_child_code(t, parent))
        .and_then(|(id, _)| take_exit_code(id).map(|code| (id, code)))
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

/// Take one pending signal off the running task. Every signal is fatal for
/// now, so the caller exits; when handlers arrive this grows a disposition
/// check and only fatal signals come back.
fn take_pending_signal() -> Option<u8> {
    with_table(|t| {
        let id = t.current;
        signal::take(&mut t.task_mut(id).signals)
    })
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
            // The console goes back to the parent as the child leaves it: a
            // shell waits, gets its prompt back, and reads again.
            let tty = crate::vfs::tty::global();
            if tty.foreground() == Some(id) {
                tty.set_foreground(Some(parent));
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
            retired.remove(0);
        }
        retired.push((id, code));
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
        let task = t.task_mut(id);
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
                // once the process is gone, not before.
                task.user_frames.push(frame);
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
        let task = t.task_mut(id);
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
        let task = t.task(current());
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
        let task = t.task_mut(id);
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
        let task = t.task_mut(id);
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
        let task = t.task_mut(id);
        for frame in frames {
            task.user_frames.retain(|&held| held != frame);
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
        let grown_to = task.brk_base + task.brk_pages * PAGE_SIZE;
        if frontier < grown_to {
            let dropped = match task.space.as_mut() {
                Some(space) => space.unmap(frontier, grown_to - frontier),
                None => alloc::vec::Vec::new(),
            };
            for frame in dropped {
                task.user_frames.retain(|&held| held != frame);
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
            task.user_frames.push(frame);
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

pub const fn permanent_tasks() -> usize {
    PERMANENT_TASKS
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
    // A task that never blocks would otherwise never see a signal: the clock
    // is the one thing guaranteed to run while it spins. Delivering here is
    // legal for the same reason being preempted is - the trap frame is on the
    // task's own stack, and `exit` leaves it the way `requeue_current` does.
    deliver_pending_signal();
    schedule(Table::requeue_current);
}

/// End the running task if a fatal signal is waiting, at whichever of the two
/// points a task can be interrupted from: a blocking call, or the clock.
///
/// A signal that arrives while the task is spinning in ring 3 is delivered by
/// the next tick, so no program can outlast a `kill` by never making a
/// syscall. Called from interrupt context, where it must not block.
pub fn deliver_pending_signal() {
    if let Some(sig) = take_pending_signal() {
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

        if !switched {
            // Nothing to run. Halt with interrupts on so the next tick wakes us,
            // and let it turn them back off before the table is touched again.
            wait_for_interrupt();
        }
    }
}

/// Where a fresh task's stack first lands. The arch trampoline calls this with
/// the task's slot number in the first argument register; the body it should
/// run, or the user image it should enter, is still in the table.
extern "C" fn task_entry(slot: usize) -> ! {
    let (body, user) = with_table(|t| {
        let task = t.task_mut(TaskId(slot));
        (task.body.take(), task.user_entry.take())
    });
    if let Some(body) = body {
        body();
        exit(0);
    }
    if let Some((entry, user_rsp)) = user {
        // One way door: the kernel stack this task arrived on stays behind as
        // nothing but the trap target, and the reaper eventually frees it.
        unsafe { context::enter_user(entry, user_rsp) };
    }
    panic!("task {slot} has neither a body nor a user image")
}
