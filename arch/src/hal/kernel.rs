//! The seam's other face: what the machine layer asks of the *kernel*.
//!
//! `crate::hal` is mostly one-directional - layers above it ask the machine for
//! something, and the forwards below hand the question to [`Current`]. But a
//! machine is not a leaf: an exception entry has to know whether a fault is a
//! page the process merely has not touched yet, a timer interrupt has to charge
//! the running task and let the scheduler take the CPU, and a `svc` has to reach
//! the syscall table. Those are kernel answers, and today `arch/<arch>/` reaches
//! up for them by name (`crate::sched::resolve_cow_fault` and friends), which is
//! the one thing that keeps the machine layer inside the kernel crate.
//!
//! So the direction is inverted the same way the rest of the seam is: the
//! machine layer *declares* what it needs ([`Kernel`]), the kernel implements it
//! once and registers it at boot, and everything below goes through the
//! forwards. After `arch` becomes its own crate this module - and the forwards
//! at the bottom - are the only names it has for any of it (`NOTES.md` §14.2).
//!
//! # Why a table and not a generic parameter
//!
//! The downward direction stays monomorphised: there is one `Current`, no `dyn`,
//! and every `hal::` call compiles to the direct one it replaced. This face
//! cannot do that, for two reasons. A `Machine<K: Kernel>` would have to thread
//! `K` through every arch type; and the exception entry points are reached from
//! `global_asm!`, so they *cannot* be generic - each would need a `#[no_mangle]`
//! shim the kernel defines and the machine's assembly branches to, a symbol the
//! machine layer references but does not own. What is actually lost is one
//! indirect call on a path that has already taken a fault or a timer interrupt;
//! measured against a page-table walk and a context switch, that is nothing.
//!
//! [`Current`]: super::Current

/// What a resolver did with a demand-paged fault's address.
///
/// Three answers, not two, because a refusal and a machine with no memory to
/// give are different facts about the same address. A refusal is the process's
/// own fault to bear (`SIGSEGV`); no memory is the machine's, and Linux's
/// measured answer to that is the OOM killer - `SIGKILL` to the process that
/// asked, with the machine and every other process running on (`docs/notes/`
/// §8.0). Collapsing the two would make a process that used up the machine's
/// memory indistinguishable from one that dereferenced a wild pointer, which is
/// the shape this kernel's fault path had before the OOM path was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAnswer {
    /// The page is there now; the faulting instruction can be re-executed.
    Resolved,
    /// Not the process's to touch: no mapping covers the address, or the
    /// mapping that does does not permit the access.
    Refused,
    /// The address *is* the process's and the machine has no frame for it.
    OutOfMemory,
}

pub use mpos_vm::FaultKind;

/// The kernel services a machine's trap and interrupt entry needs.
///
/// Deliberately a *fact* interface rather than a policy one: each method answers
/// a question or performs one step, and which of them a machine calls, and in
/// what order, stays the machine's. The one exception is
/// [`disposition_is_default`](Kernel::disposition_is_default), which answers a
/// question about a task's signal disposition without handing out the
/// disposition type - a kernel type a machine has no business naming.
pub trait Kernel: Sync {
    /// Whether the scheduler is running: whether a "current task" exists at all.
    ///
    /// False on the way up, and the reason a machine must ask is that a fault
    /// taken before the first task is a kernel bug rather than a process's.
    fn started(&self) -> bool;

    /// The task running on this CPU, as its pid. Zero-ish before the scheduler
    /// starts; carried as a plain integer because `TaskId` is the kernel's type
    /// and this boundary is the reason a machine never sees it.
    fn current(&self) -> usize;

    /// The running task's program name, as `PR_GET_NAME` would answer it.
    ///
    /// A fault report that names only the task *slot* leaves the reader where
    /// they started: `task=10` says which row of the scheduler's table died and
    /// nothing about which program that row was. Linux names the program in the
    /// same line (`udevd[312]: segfault at …`), and the bytes are already here -
    /// `comm`, 16 of them NUL-padded, which `/proc/<pid>/comm` serves too.
    fn current_comm(&self) -> [u8; 16];

    /// File one line into the kernel log ring, the way a `/dev/kmsg` writer
    /// does.
    ///
    /// The ring is `syslog(2)`'s `READ_ALL` answer and what `klogd`/`dmesg`
    /// drain; the console is the kernel's own voice. Linux keeps a userland
    /// fault in the ring and *off* the console. This machine's report goes to
    /// both for now: the console line is what a boot log shows, and the ring
    /// line is what a program can pin.
    fn log_line(&self, args: core::fmt::Arguments<'_>);

    /// Report a userland fault the way Linux's `show_signal_msg` does.
    ///
    /// Linux files exactly one line, and its shape is load-bearing: the *program*
    /// comes first (`udevd[312]`), so a reader whose log is a screenful of
    /// `[ ok ]` can tell which program died without cross-referencing a
    /// scheduler table they do not have. `comm` is what `PR_GET_NAME` answers,
    /// the same 16 bytes `/proc/<pid>/comm` serves; the number in brackets is
    /// the *thread* id, not the process id, so the line names the one thread
    /// that took the fault.
    ///
    /// `error` is the machine's own fault word. On x86_64 that is the page-fault
    /// error code; on aarch64 it is the `ESR_EL1` fault status field, which is
    /// what Linux's arm64 port prints in the same column. The two machines'
    /// words are therefore not comparable to each other, exactly as on Linux.
    fn report_user_fault(&self, addr: usize, ip: usize, sp: usize, error: usize);

    /// Report a userland trap that has no faulting address - a ring-3 `#UD`
    /// (`hlt` is `#GP`; `0f 0b` is this one) and the five other vectors that
    /// share the machine's trap door.
    ///
    /// Linux has a second line shape for that class and it is not a formatting
    /// accident: `traps: comm[tid] <what happened> ip:… sp:… error:0` carries no
    /// `at <address>` because there is none, and printing a borrowed address
    /// beside it would be an invention. The kernel fills in the words for what
    /// happened from the vector, in its own vocabulary rather than Linux's -
    /// this host's kernel log is not readable, so a borrowed wording would be a
    /// recalled one (`docs/notes/` §6.158).
    ///
    /// `vector` is the machine's own word for *which* exception this was, and it
    /// is the only fact that survives a trap frame: `#DE`, `#UD`, `#SS`, `#NP`,
    /// `#AC` and `#GP` all arrive as an `InterruptStackFrame` with an error code
    /// that is usually zero. The kernel names it
    /// (`crate::kernel_services::trap_name`), and this seam carries the number
    /// rather than the word so the machine layer keeps no second copy of the
    /// table.
    fn report_user_trap(&self, vector: u8, ip: usize, sp: usize, error: usize);

    /// The signal a synchronous CPU exception ends a process with, given which
    /// *kind* of exception the machine took.
    ///
    /// A trap door is not one event with one answer: Linux ends a child with
    /// `SIGFPE` for `#DE`, `SIGTRAP` for `#BP`, `SIGILL` for `#UD`, `SIGBUS`
    /// for `#AC` and `SIGSEGV` for `#GP`, all measured through `WTERMSIG` on
    /// Linux 7.2.14 (`docs/notes/` §6.159). The six vectors share one handler
    /// on x86_64 that used to answer `SIGSEGV` for every one of them, so a
    /// process that executed `ud2` died of a bad memory reference and a parent
    /// that waited for `SIGILL` was told a different signal happened.
    ///
    /// The argument is a [`FaultKind`] rather than a vector because the answer
    /// is the kernel's policy over ABI signal numbers, and the two machines
    /// number their exceptions differently - an `ESR_EL1` class is not a
    /// vector and never needed to be (`docs/notes/` §6.165). Each machine
    /// reads its own word into a kind (`sched::signal::fault_kind_of_vector`
    /// for x86_64, the `EC` field for aarch64) and the table below that is
    /// shared, so a class named by either arrives at the same signal.
    fn signal_for_kind(&self, kind: FaultKind) -> u8;

    /// The kernel's own word for a CPU exception, given its vector.
    ///
    /// A ring-0 exception ends the machine, and the report it leaves behind has
    /// to say *which* one it was - a `#UD` in the kernel and a `#GP` in the
    /// kernel are different bugs and the words differ. The table lives with
    /// the rest of the trap vocabulary (it is the same one `traps:` lines use),
    /// so this seam carries the number rather than the machine layer growing a
    /// second copy that could drift from the first. Measured fact behind
    /// keeping one table: the door that served six vectors named every one of
    /// them "general protection fault" until 2026-11-14 (`docs/notes/`
    /// §6.158).
    fn trap_name(&self, vector: u8) -> &'static str;

    /// The kernel's reading of an x86_64 trap vector: which [`FaultKind`] of
    /// exception the machine took.
    ///
    /// Only one machine has vectors, and the meaning of one is kernel policy -
    /// it is the same table [`signal_for_kind`](Kernel::signal_for_kind) and
    /// [`send_fault_signal`](Kernel::send_fault_signal) are keyed on, so the
    /// machine asks rather than keeping a second copy that could drift
    /// (`docs/notes/` §6.165).
    fn fault_kind_of_vector(&self, vector: u8) -> FaultKind;

    /// Queue the signal a synchronous CPU exception raised, **with its
    /// fault-shaped `siginfo_t` record**, and answer the signal number the
    /// caller must then use for its disposition and death checks.
    ///
    /// `kind` is which exception the machine took, in the vocabulary that both
    /// machines can name ([`FaultKind`]); `addr` is the faulting address the
    /// machine recorded (`CR2` on x86_64, `FAR_EL1` on aarch64) and zero for a
    /// trap with no faulting address; `rip` is the interrupted instruction
    /// pointer, which is `si_addr` for `#DE` and `#UD`.
    ///
    /// The kernel owns the map from a kind to `(signal, si_code, si_addr)` -
    /// measured on Linux 7.2.14, `docs/notes/` §6.160 - so this seam carries
    /// the raw facts rather than a second copy of the table, the same shape as
    /// [`signal_for_kind`](Kernel::signal_for_kind). For a
    /// [`FaultKind::Page`] the kernel also decides `SEGV_ACCERR` over
    /// `SEGV_MAPERR` by asking whether **a mapping describes the address**
    /// (measured on Linux 7.2.14 across four refused-access classes, and on a
    /// never-mapped read and a hole between two mappings: `docs/notes/`
    /// §6.162). The page-table error word is a *different* question and
    /// answers wrong for the most common self-repair case - a `PROT_NONE`
    /// page, which the CPU reports as not present - and a machine cannot
    /// answer it at all, which is why the question moved to the kernel rather
    /// than travelling through this seam as a flag.
    ///
    /// This is deliberately not [`send_signal`](Kernel::send_signal): that
    /// door raises a `SI_USER` record with the sender's pid at wire offset
    /// 16, which is exactly the eight bytes a fault's `si_addr` occupies. A
    /// handler asking "which address faulted" was told a pid.
    fn send_fault_signal(&self, kind: FaultKind, addr: usize, rip: usize) -> u8;

    /// The `(base, top)` of the running task's kernel stack, when there is one.
    fn current_stack_range(&self) -> Option<(usize, usize)>;

    /// A write to a page marked copy-on-write: give this space its own frame.
    fn resolve_cow_fault(&self, virt: usize) -> bool;

    /// The permissions the process's own mapping gives `virt`, or `None` when no
    /// mapping describes that address.
    ///
    /// A *diagnostic's* question rather than a resolver's. The fault handler has
    /// already asked every resolver and they have all said no by the time
    /// anything is reported, so the fact that decides between the two possible
    /// stories - a writable mapping this kernel left read-only, or a process
    /// writing where it may not - is exactly the one missing.
    fn vma_prot_at(&self, virt: usize) -> Option<mpos_vm::MapFlags>;

    /// What kind of mapping describes `virt`, in a word: an anonymous arena, a
    /// private object or a shared one.
    fn vma_kind_at(&self, virt: usize) -> Option<&'static str>;

    /// What the mapping containing `addr` is called, or `None` when no mapping
    /// describes it. A file's path, a `[stack]`, or a name the loader recorded.
    fn mapping_name_at(&self, addr: usize) -> Option<alloc::string::String>;

    /// A fault on a page a mapping covers but nothing has populated yet.
    ///
    /// The answer says which of the three cases it was: the page is in place,
    /// the address is not the process's, or the address is and there was no
    /// frame to bring it in with.
    fn resolve_anon_fault(&self, virt: usize, write: bool) -> FaultAnswer;

    /// A push past the mapped end of the stack: extend it, within its bounds.
    fn resolve_stack_growth(&self, addr: usize) -> bool;

    /// Whether the kernel may lazily reach `virt` for a write - the question the
    /// page-table walk asks on a fault the kernel itself caused.
    fn kernel_lazy_accessible(&self, virt: usize, write: bool) -> bool;

    /// Queue `sig` for the running task.
    fn send_signal(&self, sig: u8) -> bool;

    /// Whether the running task's disposition for `sig` is `SIG_DFL` - the
    /// signal would kill it rather than run a handler.
    fn disposition_is_default(&self, sig: u8) -> bool;

    /// End the running task with `code`. Never returns.
    ///
    /// Only one thread ends. A task that a *fatal signal* kills takes its whole
    /// thread group with it ([`die_of_fatal_signal`](Self::die_of_fatal_signal)):
    /// Linux answers "process or thread?" when the signal is generated, and
    /// every member dies of it. A machine that calls this where the process is
    /// what died leaves a sibling parked in a blocking wait with nobody left to
    /// wake it - which is how the weston shutdown stall of `docs/notes/` §7.4h
    /// got its `retired=false`.
    fn exit(&self, code: isize) -> !;

    /// End the running task **and its whole thread group** of a fatal signal
    /// `sig`: the one door a signal whose default action terminates ends a task
    /// through. Never returns.
    ///
    /// The parent's `wait` reads the cause back as `WIFSIGNALED`/`sig`, exactly
    /// as [`exit`](Self::exit)`(-(sig as isize))` would write it. This exists
    /// beside `exit` rather than replacing it because the two answer different
    /// questions: a seccomp kill and the pid-namespace init's own fault are the
    /// kernel ending *this task*, while a signal Linux's `complete_signal` would
    /// have turned into `SIGNAL_GROUP_EXIT` is the kernel ending the *process*
    /// (`docs/notes/` §6.77b).
    fn die_of_fatal_signal(&self, sig: u8) -> !;

    /// The machine has no frame for the running task's own fault: end the
    /// process the way Linux's OOM killer does, and never return.
    ///
    /// The whole group, not just the faulting thread - Linux kills a process -
    /// and with `SIGKILL`'s number as the exit word, so a parent's `wait` reads
    /// `WIFSIGNALED`/`SIGKILL` exactly as it would on Linux
    /// (`docs/notes/` §8.0). A machine calls this only on
    /// [`FaultAnswer::OutOfMemory`]: the other two answers are not its to end a
    /// task over.
    fn out_of_memory(&self) -> !;

    /// Let the scheduler take the CPU at the end of an interrupt.
    fn preempt(&self);

    /// Let the scheduler take the CPU at an interrupt that landed on a user
    /// address, given the address it landed on.
    ///
    /// The one thing that differs from [`preempt`](Self::preempt) is a task
    /// inside a restartable sequence's span: on a uniprocessor the clock is the
    /// only thing that can break the span, so the scheduler is told where the
    /// interrupt landed and defers the switch instead of breaking it. Every
    /// other trap path keeps the address-less call.
    fn preempt_at(&self, ip: usize);

    /// Account one tick to whatever was running when the interrupt landed.
    fn charge_cpu(&self, in_user: bool);

    /// Hand a signal to the task this interrupt interrupted, if one is waiting
    /// for it and can be delivered **here**.
    ///
    /// This is the second door onto a handled signal, and it exists because the
    /// first one was never the only moment a whole register file is available -
    /// it was the only moment the kernel had *built* one. An interrupt from a
    /// program lands with every register of that program on the kernel stack
    /// already, saved by the stub that called this handler, so a signal that
    /// only needed a frame to run on was waiting for a system call for no reason
    /// (`docs/notes/` §2.13; Linux hands such a signal over at the next tick).
    ///
    /// `frame` is the machine's trap frame for the interrupted program, or null
    /// for an interrupt that landed on the kernel - a trap from ring 0 has no
    /// program context to build a handler's frame on, and the machine says so by
    /// passing null rather than by leaving the question to the kernel.
    ///
    /// The kernel answers `false` and leaves the signal pending when it cannot
    /// deliver from this frame: no handler at all (the fatal path is not this
    /// one), a disposition this machine has no frame for, a tracer that must see
    /// the stop at a syscall, or a stack that cannot be written. Pending is the
    /// safe answer - the syscall exit still has the frame and will try again.
    ///
    /// Present only where the build declares `mpos_signal_frame`: a machine with
    /// no way to lay out a handler's frame has nothing to deliver.
    #[cfg(mpos_signal_frame)]
    fn deliver_at_trap(&self, frame: *mut ()) -> bool;

    /// The machine took a single-step trap for the running task: turn it into
    /// the tracer's stop when one is tracing, and answer whether it did.
    ///
    /// A `ptrace` word rather than a signal because the two are different
    /// events on the same vector: `#DB` with a tracer is `PTRACE_SINGLESTEP`'s
    /// stop (the tracee parks and the tracer is woken), and `#DB` without one is
    /// the program's own `SIGTRAP`. The machine cannot tell which it is.
    fn step_trap(&self, regs: *mut ()) -> bool;

    // -----------------------------------------------------------------------
    // The rest of what a machine asks for. None of these is on a trap path in
    // the sense the methods above are: they are the boot bringing the kernel
    // up, a machine's own diagnostic line, and the two interrupts whose work
    // belongs to a subsystem above.
    // -----------------------------------------------------------------------

    /// Put a formatted line on the kernel's console.
    ///
    /// A fault report and a bring-up message both go through here, so a machine
    /// writes one and the reader sees it wherever the console has been pointed:
    /// the wire, a virtual console, or a framebuffer. The trailing newline is
    /// [`kprintln`]'s, not this method's.
    fn print_line(&self, args: core::fmt::Arguments<'_>);

    /// Pull the console back onto the serial wire.
    ///
    /// For the paths that must be *read*: a boot whose console is a framebuffer
    /// paints a dying machine's report into a window nobody may be looking at,
    /// so the last-resort handlers ask for the wire first.
    fn force_serial(&self);

    /// Hand a byte the machine's receiver took to the console's input mux.
    ///
    /// Which terminal that byte belongs to is the mux's answer, not the
    /// machine's - the same reason this is one call and not the tty's own
    /// `receive_byte`, and the same function both machines' UART handlers end
    /// in.
    fn route_console_input(&self, byte: u8);

    /// Do the work one network interrupt carries: poll the adapter, hand the
    /// frames to the stack, acknowledge the device.
    ///
    /// The machine knows which line fired; what to do about it is the stack's.
    fn service_network(&self);

    /// One block completion's work: mark the mailbox and wake the parked
    /// submitter. Runs in interrupt context, so it must not sleep, allocate
    /// or touch the device - the parked task retires the request itself.
    fn service_blk(&self);

    /// Record the loader's command line, which `/proc/cmdline` reports and the
    /// initramfs's init reads to learn `root=`.
    fn set_cmdline(&self, cmdline: &'static str);

    /// Turn the loader's wire record into the kernel's boot record.
    ///
    /// The machine is handed a [`Handoff`](mpos_bootinfo::Handoff) and cannot
    /// read it: the translation is what decides what a region *means*, and the
    /// magic check that refuses an older loader's record lives with it.
    ///
    /// # Safety
    /// `handoff` must be the record the loader wrote, live for the length of
    /// this call.
    unsafe fn boot_info(&self, handoff: *const mpos_bootinfo::Handoff) -> mpos_bootinfo::BootInfo;

    /// Bring up physical memory. The heap needs no setup of its own: it takes
    /// pages from here on demand, so it is usable the moment this returns.
    ///
    /// # Safety
    /// `info` must describe live memory mapped at `info.hhdm_offset`.
    unsafe fn start_memory(&self, info: &mpos_bootinfo::BootInfo) -> MemoryStartup;

    /// Allocate `2^order` contiguous physical frames, aligned to their own size.
    ///
    /// The machine's page-table code is the caller: a table is a frame, and
    /// which frame is the allocator's answer, not the machine's.
    fn alloc_frames(&self, order: usize) -> Option<usize>;

    /// Return frames allocated by [`alloc_frames`](Self::alloc_frames) with the
    /// same order.
    fn free_frames(&self, phys: usize, order: usize);

    /// How many frames the machine could still hand out.
    ///
    /// A bring-up check's measurement, and the reason it is a count rather than
    /// the kernel's `FrameStats`: the machine has no business naming a
    /// statistics type it only ever reads one field of.
    fn frames_free(&self) -> usize;

    /// Make `task` runnable again.
    ///
    /// The console's transmit ring parks writers on room in the queue instead
    /// of spinning for it, and this is how a visit from the transmitter hands
    /// them back. Carried as a plain integer for the same reason
    /// [`current`](Kernel::current) is.
    fn wake(&self, task: usize);

    /// The x87 device-not-available trap: hand the FPU to the task that touched
    /// it.
    ///
    /// Gated on `mpos_fpu_trap` for the same reason [`FpuTrap`](super::FpuTrap)
    /// is a capability and not part of [`Contexts`](super::Contexts): it is an
    /// answer to one hardware arrangement. A machine that saves floating point
    /// eagerly has no such trap, so it never asks and never has to answer.
    #[cfg(mpos_fpu_trap)]
    fn device_not_available(&self);

    /// Run one Linux call, as the machine's gate asks.
    ///
    /// # Safety
    /// `frame` must be null or the gate's own frame for the calling task - the
    /// same contract [`super::Gate`] states for every op over that pointer.
    unsafe fn dispatch_syscall(
        &self,
        nr: u64,
        args: [usize; 6],
        from_user: bool,
        frame: *mut (),
    ) -> isize;
}

// ---------------------------------------------------------------------------
// The boot's answer about memory, in the machine layer's own words.
// ---------------------------------------------------------------------------

/// What bringing physical memory up produced.
///
/// Deliberately two numbers rather than the kernel's `FrameStats`: a machine
/// prints them at bring-up and reads no other field, so handing it the whole
/// statistics type would be a kernel type crossing a boundary for no reason.
#[derive(Clone, Copy, Debug)]
pub struct MemoryStartup {
    /// How many zones the memory map was split into.
    pub zones: usize,
    /// How many frames in total, of every order.
    pub total: usize,
}

// ---------------------------------------------------------------------------
// The one registered implementation.
// ---------------------------------------------------------------------------

/// The kernel this boot is running.
///
/// Written exactly once, by `main`, before anything that can take a trap exists:
/// before the interrupt controllers are brought up, so no handler can run and
/// find it empty. Read on every fault and every timer interrupt, so the read is
/// a plain load with no lock. There is one writer, it happens first, and the
/// value never changes afterwards.
static mut REGISTERED: Option<&'static dyn Kernel> = None;

/// Give the machine layer its kernel. Called once, from `main/<arch>.rs`.
///
/// Registering twice is a bug in the boot and not something a machine should be
/// silently given the second answer for, so it panics rather than overwriting.
pub fn register(kernel: &'static dyn Kernel) {
    // SAFETY: the unique writer, running before any trap can be taken; taking
    // the address of the static rather than a reference to it keeps this off the
    // `static_mut_refs` lint's radar without changing what it does.
    unsafe {
        let slot = core::ptr::addr_of_mut!(REGISTERED);
        assert!(
            (*slot).is_none(),
            "the kernel services were registered twice"
        );
        core::ptr::write(slot, Some(kernel));
    }
}

/// The registered kernel.
///
/// # Panics
/// If [`register`] has not run. That is a boot-ordering bug, and it is better
/// reported than turned into a null call: a machine that reaches a fault with no
/// kernel has nowhere to go anyway.
fn registered() -> &'static dyn Kernel {
    // SAFETY: the single load of a value written once before any of this can
    // run; see [`REGISTERED`].
    unsafe { (*core::ptr::addr_of!(REGISTERED)).expect("no kernel services registered") }
}

// ---------------------------------------------------------------------------
// The forwards. Everything in the machine layer calls these; they are one-line
// dispatches onto the registered kernel, so a handler never names `sched`.
// ---------------------------------------------------------------------------

/// Whether the scheduler is running. See [`Kernel::started`].
#[inline]
pub fn started() -> bool {
    registered().started()
}

/// The running task's pid. See [`Kernel::current`].
#[inline]
pub fn current() -> usize {
    registered().current()
}

/// The running task's kernel stack. See [`Kernel::current_stack_range`].
#[inline]
pub fn current_stack_range() -> Option<(usize, usize)> {
    registered().current_stack_range()
}

/// Resolve a copy-on-write fault. See [`Kernel::resolve_cow_fault`].
#[inline]
pub fn resolve_cow_fault(virt: usize) -> bool {
    registered().resolve_cow_fault(virt)
}

/// The mapping's permissions at `virt`. See [`Kernel::vma_prot_at`].
#[inline]
pub fn vma_prot_at(virt: usize) -> Option<mpos_vm::MapFlags> {
    registered().vma_prot_at(virt)
}

/// What kind of mapping describes `virt`. See [`Kernel::vma_kind_at`].
#[inline]
pub fn vma_kind_at(virt: usize) -> Option<&'static str> {
    registered().vma_kind_at(virt)
}

/// What the mapping containing `addr` is called. See [`Kernel::mapping_name_at`].
#[inline]
pub fn mapping_name_at(addr: usize) -> Option<alloc::string::String> {
    registered().mapping_name_at(addr)
}

/// Resolve a demand-paged fault. See [`Kernel::resolve_anon_fault`].
#[inline]
pub fn resolve_anon_fault(virt: usize, write: bool) -> FaultAnswer {
    registered().resolve_anon_fault(virt, write)
}

/// Extend the stack. See [`Kernel::resolve_stack_growth`].
#[inline]
pub fn resolve_stack_growth(addr: usize) -> bool {
    registered().resolve_stack_growth(addr)
}

/// Whether the kernel may lazily reach `virt`. See
/// [`Kernel::kernel_lazy_accessible`].
#[inline]
pub fn kernel_lazy_accessible(virt: usize, write: bool) -> bool {
    registered().kernel_lazy_accessible(virt, write)
}

/// Queue `sig` for the running task. See [`Kernel::send_signal`].
#[inline]
pub fn send_signal(sig: u8) -> bool {
    registered().send_signal(sig)
}

/// Whether `sig` would kill the running task rather than run a handler. See
/// [`Kernel::disposition_is_default`].
#[inline]
pub fn disposition_is_default(sig: u8) -> bool {
    registered().disposition_is_default(sig)
}

/// End the running task. See [`Kernel::exit`].
#[inline]
pub fn exit(code: isize) -> ! {
    registered().exit(code)
}

/// End the running task and its thread group of a fatal signal. See
/// [`Kernel::die_of_fatal_signal`].
#[inline]
pub fn die_of_fatal_signal(sig: u8) -> ! {
    registered().die_of_fatal_signal(sig)
}

/// End the process that used up the machine's memory. See
/// [`Kernel::out_of_memory`].
#[inline]
pub fn out_of_memory() -> ! {
    registered().out_of_memory()
}

/// Let the scheduler take the CPU. See [`Kernel::preempt`].
#[inline]
pub fn preempt() {
    registered().preempt()
}

/// Let the scheduler take the CPU, telling it the interrupted user address.
/// See [`Kernel::preempt_at`].
#[inline]
pub fn preempt_at(ip: usize) {
    registered().preempt_at(ip)
}

/// Account a tick. See [`Kernel::charge_cpu`].
#[inline]
pub fn charge_cpu(in_user: bool) {
    registered().charge_cpu(in_user)
}

/// A single-step trap for the running task. See [`Kernel::step_trap`].
#[inline]
pub fn step_trap(regs: *mut ()) -> bool {
    registered().step_trap(regs)
}

/// Hand a waiting signal to the task an interrupt interrupted. See
/// [`Kernel::deliver_at_trap`].
///
/// Present only where the build declares `mpos_signal_frame`.
#[cfg(mpos_signal_frame)]
#[inline]
pub fn deliver_at_trap(frame: *mut ()) -> bool {
    registered().deliver_at_trap(frame)
}

/// The device-not-available trap. See [`Kernel::device_not_available`].
///
/// Present only where the build declares `mpos_fpu_trap`.
#[cfg(mpos_fpu_trap)]
#[inline]
pub fn device_not_available() {
    registered().device_not_available()
}

/// Run one Linux call. See [`Kernel::dispatch_syscall`].
///
/// # Safety
/// As [`Kernel::dispatch_syscall`].
#[inline]
pub unsafe fn dispatch_syscall(
    nr: u64,
    args: [usize; 6],
    from_user: bool,
    frame: *mut (),
) -> isize {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { registered().dispatch_syscall(nr, args, from_user, frame) }
}

/// Format a line and give it to the kernel's console. See
/// [`Kernel::print_line`].
///
/// A macro rather than a plain forward, so a call site keeps `println!`'s shape:
/// one format string, its arguments, checked at compile time. The trailing
/// newline is added in one place.
///
/// The newline goes on through an outer `format_args!` rather than a
/// `concat!($fmt, "\n")`. `concat!` looks like the tidier way and is not: an
/// implicit named argument (`"{rsp:#x}"`, which dozens of these call sites use)
/// is only captured from a *string literal*, and `concat!`'s expansion is a
/// macro call, so every one of those would fail with "there is no argument named
/// `rsp`". The nested form keeps the inner `format_args!` on a literal, which is
/// where capture happens, and `Arguments` is itself `Display`.
macro_rules! kprintln {
    ($($arg:tt)*) => {
        $crate::hal::kernel::print_line(format_args!("{}\n", format_args!($($arg)*)))
    };
}
pub(crate) use kprintln;

/// Put a formatted line on the console. See [`Kernel::print_line`].
#[inline]
pub fn print_line(args: core::fmt::Arguments<'_>) {
    registered().print_line(args)
}

/// File a formatted line into the kernel log ring. See [`Kernel::log_line`].
#[inline]
pub fn log_line(args: core::fmt::Arguments<'_>) {
    registered().log_line(args)
}

/// Report a userland fault in Linux's shape. See
/// [`Kernel::report_user_fault`].
#[inline]
pub fn report_user_fault(addr: usize, ip: usize, sp: usize, error: usize) {
    registered().report_user_fault(addr, ip, sp, error)
}

/// Report a userland trap with no faulting address, naming its vector. See
/// [`Kernel::report_user_trap`].
#[inline]
pub fn report_user_trap(vector: u8, ip: usize, sp: usize, error: usize) {
    registered().report_user_trap(vector, ip, sp, error)
}

/// The signal a synchronous CPU exception ends a process with. See
/// [`Kernel::signal_for_kind`].
#[inline]
pub fn signal_for_kind(kind: FaultKind) -> u8 {
    registered().signal_for_kind(kind)
}

/// The kernel's own word for a CPU exception. See [`Kernel::trap_name`].
#[inline]
pub fn trap_name(vector: u8) -> &'static str {
    registered().trap_name(vector)
}

/// The kernel's reading of a trap vector. See
/// [`Kernel::fault_kind_of_vector`].
#[inline]
pub fn fault_kind_of_vector(vector: u8) -> FaultKind {
    registered().fault_kind_of_vector(vector)
}

/// Queue a CPU exception's fault-shaped record. See
/// [`Kernel::send_fault_signal`].
#[inline]
pub fn send_fault_signal(kind: FaultKind, addr: usize, rip: usize) -> u8 {
    registered().send_fault_signal(kind, addr, rip)
}

/// The running task's `comm`. See [`Kernel::current_comm`].
#[inline]
pub fn current_comm() -> [u8; 16] {
    registered().current_comm()
}

/// Pull the console back onto the serial wire. See [`Kernel::force_serial`].
#[inline]
pub fn force_serial() {
    registered().force_serial()
}

/// Hand a received byte to the console's input mux. See
/// [`Kernel::route_console_input`].
#[inline]
pub fn route_console_input(byte: u8) {
    registered().route_console_input(byte)
}

/// Do one network interrupt's work. See [`Kernel::service_network`].
#[inline]
pub fn service_network() {
    registered().service_network()
}

/// Do one block completion's work. See [`Kernel::service_blk`].
pub fn service_blk() {
    registered().service_blk()
}

/// Record the loader's command line. See [`Kernel::set_cmdline`].
#[inline]
pub fn set_cmdline(cmdline: &'static str) {
    registered().set_cmdline(cmdline)
}

/// Turn the loader's wire record into the kernel's boot record. See
/// [`Kernel::boot_info`].
///
/// # Safety
/// As [`Kernel::boot_info`].
#[inline]
pub unsafe fn boot_info(handoff: *const mpos_bootinfo::Handoff) -> mpos_bootinfo::BootInfo {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { registered().boot_info(handoff) }
}

/// Bring up physical memory. See [`Kernel::start_memory`].
///
/// # Safety
/// As [`Kernel::start_memory`].
#[inline]
pub unsafe fn start_memory(info: &mpos_bootinfo::BootInfo) -> MemoryStartup {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { registered().start_memory(info) }
}

/// Allocate `2^order` contiguous frames. See [`Kernel::alloc_frames`].
#[inline]
pub fn alloc_frames(order: usize) -> Option<usize> {
    registered().alloc_frames(order)
}

/// Return frames allocated by [`alloc_frames`]. See [`Kernel::free_frames`].
#[inline]
pub fn free_frames(phys: usize, order: usize) {
    registered().free_frames(phys, order)
}

/// How many frames the machine could still hand out. See
/// [`Kernel::frames_free`].
#[inline]
pub fn frames_free() -> usize {
    registered().frames_free()
}

/// Make `task` runnable again. See [`Kernel::wake`].
#[inline]
pub fn wake(task: usize) {
    registered().wake(task)
}
