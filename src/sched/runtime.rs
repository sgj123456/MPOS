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
}

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
    /// The descriptors this task's fd numbers index. Kernel tasks share the
    /// kernel's table; a process has one of its own.
    fds: Arc<FdTable>,
    /// The task that spawned this one in ring 3, if any. Its `wait` blocks on
    /// this task's exit; kernel-spawned tasks have none.
    parent: Option<TaskId>,
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
            fds: crate::vfs::file::kernel_fds(),
            parent: None,
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
            fds: crate::vfs::file::kernel_fds(),
            parent: None,
        });
        TaskId(slot)
    }

    /// Free every task that has exited. Runs on the scheduler's stack, which is
    /// the only place it is safe to do: the task that exited is not running any
    /// more, but its stack is where it was.
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
    try_spawn_user(bytes, priority, current(), &[])
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
        write_arg_block(stack_top_frame, user_rsp, argv);

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
            fds: Arc::new(FdTable::with_standard(crate::vfs::tty::global())),
            parent: None,
        });
        t.sched.push_ready(INIT_PID, Priority::NORMAL);
        Ok(INIT_PID)
    })
}

/// [`spawn_user`] with arguments and a parent: the process form the `spawn`
/// syscall uses. `argv` is the full argument vector, `argv[0]` being the
/// program name as POSIX frames it - it does not have to match `bytes`' path.
///
/// The parent link is what `wait` needs: when the task exits, its parent is
/// woken and reads the exit code.
pub fn try_spawn_user(
    bytes: &[u8],
    priority: Priority,
    parent: TaskId,
    argv: &[String],
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

        // The argument block is written last, straight into the top page of
        // the stack this task will land on, through the direct map. The top
        // frame is already in `frames` - `load_user` returns it separately
        // only so this write knows where the stack's page lives.
        write_arg_block(stack_top_frame, user_rsp, argv);

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { context::bootstrap(stack.top(), task_entry, slot, true) };

        // A process's descriptors are its own from the first instruction: the
        // standard three over the console, and nothing of anyone else's. What
        // it closes is then its business, not the shell's.
        let fds = Arc::new(FdTable::with_standard(crate::vfs::tty::global()));

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
            fds,
            parent: Some(parent),
        });
        let id = TaskId(slot);
        t.sched.push_ready(id, priority);
        // A parent waiting for a child wakes here, in the same critical
        // section that made the condition true.
        if t.tasks.get(parent.0).and_then(Option::as_ref).is_some() {
            t.sched.wake(parent);
        }
        Ok(id)
    })
}

/// The stack pointer a process is entered with, laid out the way the libc's
/// `_start` reads it, or `None` when the arguments would not fit the top page.
///
/// Descending from the top of the stack: the argument strings, then padding to
/// a 16-byte boundary, then `argc`, the `argv` pointers, the null that ends
/// the array, and a null `envp`. The strings therefore sit above the initial
/// stack pointer, where every later call frame is below them and nothing ever
/// writes over them; the register-less `_start` finds `argc` at `[rsp]`.
fn initial_stack_rsp(argv: &[String]) -> Option<usize> {
    if argv.len() > ARGV_MAX || argv.iter().map(|s| s.len() + 1).sum::<usize>() > ARGS_BYTES_MAX {
        return None;
    }
    let strings: usize = argv.iter().map(|s| s.len() + 1).sum();
    // argc, argv[0..argc], the null terminator, and the null envp.
    let words_bytes = (argv.len() + 3) * size_of::<usize>();
    let strings_base = USER_STACK_TOP - strings;
    let rsp = (strings_base - words_bytes) & !15;
    // Everything must fit the page the strings were written into; the loader
    // writes the block through one frame.
    (USER_STACK_TOP - rsp <= PAGE_SIZE).then_some(rsp)
}

/// Fill the initial stack page with the argument block [`initial_stack_rsp`]
/// laid out: strings at the top, pointers and `argc` below them.
///
/// # Safety
///
/// The frame must back the page at `USER_STACK_TOP - PAGE_SIZE`, and
/// `user_rsp` must be the pointer [`initial_stack_rsp`] computed for the same
/// `argv`, with the whole block inside that one page.
fn write_arg_block(frame: usize, user_rsp: usize, argv: &[String]) {
    let page =
        unsafe { core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE) };
    let from = user_rsp - (USER_STACK_TOP - PAGE_SIZE);
    assert!(
        USER_STACK_TOP - user_rsp <= PAGE_SIZE,
        "argv block is not in the top page"
    );

    // Strings first, packed downward from the very top.
    let mut string_at = PAGE_SIZE;
    for (index, arg) in argv.iter().enumerate() {
        string_at -= arg.len() + 1;
        page[string_at..string_at + arg.len()].copy_from_slice(arg.as_bytes());
        page[string_at + arg.len()] = 0;
        // argv[i], right after argc at the bottom of the block.
        let slot = from + size_of::<usize>() * (1 + index);
        page[slot..slot + 8]
            .copy_from_slice(&(USER_STACK_TOP - (PAGE_SIZE - string_at)).to_le_bytes());
    }
    page[from..from + 8].copy_from_slice(&(argv.len() as u64).to_le_bytes());
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
/// Use [`block_until_on`] when the condition is the task table itself.
pub fn block_until(cond: impl Fn() -> bool) {
    block_until_on(|_| cond())
}

/// Block until `parent` has a live child - the wait init's shell takes to
/// arrive, asked of the table rather than of a count of tasks.
pub fn block_until_child(parent: TaskId) {
    block_until_on(|t| child_alive(t, parent))
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
    with_table(|t| any_child_code(t, parent))
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

/// How many tasks are alive, counting the two permanent ones. [`PERMANENT_TASKS`]
/// is what this settles back to once everything a test started has been reaped.
pub fn task_count() -> usize {
    with_table(|t| t.tasks.iter().filter(|slot| slot.is_some()).count())
}

/// Whether `id` names a live, un-exited task.
///
/// The question a starter asks about what it started: a task that exists and
/// has not exited is one that is running or waiting to run, so whoever spawned
/// it can stop holding its breath.
pub fn is_alive(id: TaskId) -> bool {
    with_table(|t| {
        t.tasks
            .get(id.0)
            .and_then(Option::as_ref)
            .is_some_and(|task| !task.exited)
    })
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
