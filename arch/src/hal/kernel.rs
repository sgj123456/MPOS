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
//! at the bottom - are the only names it has for any of it (`PLAN.md` §14.2).
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

    /// The `(base, top)` of the running task's kernel stack, when there is one.
    fn current_stack_range(&self) -> Option<(usize, usize)>;

    /// A write to a page marked copy-on-write: give this space its own frame.
    fn resolve_cow_fault(&self, virt: usize) -> bool;

    /// A fault on a page a mapping covers but nothing has populated yet.
    fn resolve_anon_fault(&self, virt: usize, write: bool) -> bool;

    /// A push past the mapped end of the stack: extend it, within its bounds.
    fn resolve_stack_growth(&self, addr: usize, sp: usize) -> bool;

    /// Whether the kernel may lazily reach `virt` for a write - the question the
    /// page-table walk asks on a fault the kernel itself caused.
    fn kernel_lazy_accessible(&self, virt: usize, write: bool) -> bool;

    /// Queue `sig` for the running task.
    fn send_signal(&self, sig: u8) -> bool;

    /// Whether the running task's disposition for `sig` is `SIG_DFL` - the
    /// signal would kill it rather than run a handler.
    fn disposition_is_default(&self, sig: u8) -> bool;

    /// End the running task with `code`. Never returns.
    fn exit(&self, code: isize) -> !;

    /// Let the scheduler take the CPU at the end of an interrupt.
    fn preempt(&self);

    /// Account one tick to whatever was running when the interrupt landed.
    fn charge_cpu(&self, in_user: bool);

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

/// Resolve a demand-paged fault. See [`Kernel::resolve_anon_fault`].
#[inline]
pub fn resolve_anon_fault(virt: usize, write: bool) -> bool {
    registered().resolve_anon_fault(virt, write)
}

/// Extend the stack. See [`Kernel::resolve_stack_growth`].
#[inline]
pub fn resolve_stack_growth(addr: usize, sp: usize) -> bool {
    registered().resolve_stack_growth(addr, sp)
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

/// Let the scheduler take the CPU. See [`Kernel::preempt`].
#[inline]
pub fn preempt() {
    registered().preempt()
}

/// Account a tick. See [`Kernel::charge_cpu`].
#[inline]
pub fn charge_cpu(in_user: bool) {
    registered().charge_cpu(in_user)
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
