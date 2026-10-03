//! The hardware abstraction layer: the traits a machine implements, and the
//! one place the rest of the kernel asks it for anything.
//!
//! Everything above this module is written against *capabilities*, not against
//! a machine. A machine declares which ones it has by implementing the traits
//! for its own type, [`Current`] names the one this build targets, and the
//! forwards below are the only path from generic code to hardware. The point is
//! not indirection - there is one implementation and no `dyn`, so every call
//! monomorphises to the direct one it replaced - but that a second
//! architecture is a *new* `arch/<arch>/` implementing these traits, with no
//! edit to the modules that use them (`PLAN.md` §9.1).
//!
//! # Why capability traits and not one `Arch` blob
//!
//! The seam is only worth having if its shape stops a leak, and the thing that
//! leaks in practice is a *type*: today `arch::gdt::user_selectors` hands
//! `syscall` an x86 `SegmentSelector`, and `arch::context::AddressSpace` is
//! named by the scheduler. Grouping by the six concerns the kernel actually has
//! is what lets each trait name the types it needs and no more, so an
//! architecture-specific type has nowhere to cross. A single trait carrying
//! every method would be the same leak with one import instead of six, which is
//! the failure `PLAN.md` §11.3 warned about when it rejected a "generic HAL
//! trait forest" - this is deliberately *six* traits for six concerns, not one
//! trait per function.
//!
//! # What this does not abstract, and must not
//!
//! The Linux ABI *is* the machine's register layout: `user_regs_struct`, the
//! signal frame, and the `syscall`/`iretq` gate are x86_64 by definition, and
//! on aarch64 the same rows are `x0..x30`/`pc`/`pstate` behind `svc`. That
//! belongs in `arch/<arch>/`, where its shape is legal; wrapping it in a
//! neutral type would be the private dialect `AGENTS.md` §4 rule 3 forbids. So
//! "no leakage" here means: a port writes `arch/<arch>/` and touches no shared
//! file - not that a register frame becomes architecture-neutral.
//!
//! # Naming
//!
//! `crate::hal` is not `crate::drivers::virtio::hal`: the latter is the
//! `virtio_drivers` crate's own `Hal` (a DMA/address-space contract for one
//! device family). This module is the kernel's machine abstraction; the two
//! share a word and nothing else.

/// The paging error the address-space contract answers with. Re-exported
/// because it belongs to the vocabulary a caller matches on (`Err(PageTaken)`)
/// and its home is `mm`, beside [`Mapping`].
pub use crate::mm::vm::PageTaken;
use crate::mm::vm::{KernelSections, MapFlags, Mapping};

/// The scheduler clock: monotonic ticks, and wall-clock-free elapsed time.
///
/// The tick rate is an associated const rather than a call because it is a
/// property of the machine's clock, known at compile time, and callers divide
/// by it (`CLK_TCK`, `USER_HZ`) in arithmetic that should fold.
pub trait Clock {
    /// Ticks per second of [`ticks`](Clock::ticks) - the scheduling and
    /// poll-granularity clock.
    const TICK_HZ: u32;

    /// Monotonic tick count since boot.
    fn ticks() -> u64;

    /// Nanoseconds since boot, finer than a tick where the machine can say so.
    fn since_boot_nanos() -> u64;

    /// Spin for approximately `ms` with interrupts as they are. Boot-only: it
    /// exists for the milestone checks, which must wait without a scheduler.
    fn busy_wait_ms(ms: u64);
}

/// The wall clock: the civil time the boot anchored, stored in the RTC.
///
/// Kept apart from [`Clock`] because the two answer different questions -
/// `clock_gettime(CLOCK_MONOTONIC)` must not be affected by a `settimeofday`,
/// and a machine may have a monotonic source and no battery.
pub trait WallClock {
    /// The Unix epoch, in seconds, the boot anchored wall time at.
    fn boot_epoch() -> u64;

    /// Anchor (or re-anchor) the boot epoch, as `settimeofday` does.
    fn set_boot_epoch(epoch: u64);

    /// Write `epoch` straight to the hardware clock, if the machine has one.
    /// `false` means there is no persistent clock to write.
    fn write_epoch(epoch: u64) -> bool;
}

/// The CPU itself: interrupt state, the idle and halt paths, and per-CPU
/// features userland expects the kernel to have turned on.
pub trait Cpu {
    /// Whether this core would take an interrupt right now. The one honest test
    /// for "may this context sleep".
    fn interrupts_enabled() -> bool;

    /// Let the core take interrupts. Separated from [`init_interrupts`] because
    /// a boot path brings the controllers up before it unmasks.
    ///
    /// [`init_interrupts`]: Cpu::init_interrupts
    fn enable_interrupts();

    /// Stop the machine for good. A panic path with nothing left to do.
    fn halt() -> !;

    /// Wait for the next interrupt and return with interrupts off - the
    /// scheduler's idle path, which expects to be woken.
    fn idle();

    /// Turn on whatever SIMD/FP state userland assumes the kernel has enabled
    /// (on x86_64, `CR4.OSFXSR`/`OSXMMEXCPT`; a neutral name because the
    /// requirement is not x86's).
    fn enable_user_simd();

    /// Bring up interrupts: descriptors, the interrupt controller, the clock.
    fn init_interrupts();
}

/// An interrupt line this kernel masks and unmasks by name.
///
/// Only the lines that cross the seam are named - a driver that must let
/// delivery through says *which line it is on*, never which controller bit,
/// because on another machine the line is not a PIC input at all. A
/// message-signalled device (the network adapter) is not one of these: it has
/// no line to mask, and asks [`Interrupts::net_msi_vector`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Line {
    /// The programmable interval timer, the scheduler's clock.
    Timer,
    /// The first serial port (COM1).
    Serial,
}

/// The interrupt controller: masking one line, acknowledging one, and the two
/// facts a message-signalled device needs before it may arm MSI-X.
pub trait Interrupts {
    /// Allow delivery on `line`.
    fn unmask(line: Line);

    /// Block delivery on `line`.
    fn mask(line: Line);

    /// Acknowledge the interrupt in service on `line`, so the controller may
    /// deliver the next one at or below its priority. Called from interrupt
    /// context, so it must not lock, allocate or fault.
    fn eoi(line: Line);

    /// Bring up message-signalled delivery - on x86_64 the local APIC's page,
    /// whose EOI register the handler writes last, and which must be mapped
    /// before a device arms MSI-X. Idempotent; called by the driver that is
    /// about to arm it, because the mapping needs a window the earliest boot
    /// does not have.
    fn init_message_signalled();

    /// The vector an MSI-X message-table entry for the network adapter must
    /// carry. A plain machine number rather than a neutral type because that is
    /// exactly what the table entry holds - it is written into the device's
    /// memory, not into the kernel's vocabulary.
    fn net_msi_vector() -> u8;
}

/// One process's view of memory: the page tables it runs on.
///
/// The type implementing this is the machine's, reached above the seam only as
/// [`AddressSpace`]; the methods are the whole of what the kernel does with it,
/// so a port implements these and nothing else. The vocabulary they speak -
/// [`Mapping`], [`MapFlags`], [`PageTaken`], [`KernelSections`] - belongs to
/// `mm`, which owns what a mapping *is*; the machine owns how one is made.
pub trait AddressSpaceOps {
    /// An address space with nothing in it but the shared kernel half. Asserts
    /// if the kernel tables have not been installed yet.
    fn new() -> Self;

    /// The root frame, for callers that must say which space is which.
    fn root(&self) -> usize;

    /// Add one mapping. It need not be the active space. A collision is a bug
    /// in the kernel's own plan, so this asserts; a caller taking a request
    /// from a process uses [`try_map`](Self::try_map).
    fn map(&mut self, mapping: Mapping);

    /// Add one mapping, answering [`PageTaken`] where [`map`](Self::map) would
    /// assert - the shape a process's `MAP_FIXED` needs.
    fn try_map(&mut self, mapping: Mapping) -> Result<(), PageTaken>;

    /// Remove every mapping that covers `virt..virt + len`, answering the
    /// frames that were mapped there (the caller frees them).
    fn unmap(&mut self, virt: usize, len: usize) -> alloc::vec::Vec<usize>;

    /// Change the flags of the mappings covering `virt..virt + len`.
    fn protect(&mut self, virt: usize, len: usize, flags: MapFlags);

    /// Make this space the active one on this CPU.
    fn switch(&self);

    /// Map `phys` at `virt` as a copy-on-write page, writable for now.
    fn map_cow(&mut self, virt: usize, phys: usize, flags: MapFlags);

    /// Map `phys` at `virt` shared: the same frame in more than one space.
    fn map_shared(&mut self, virt: usize, phys: usize, flags: MapFlags);

    /// Resolve a write fault on a copy-on-write page: give this space its own
    /// frame and answer `(old_phys, new_phys)`.
    fn resolve_cow(&mut self, virt: usize) -> Option<(usize, usize)>;

    /// Walk every mapped user page, lowest first, handing `(virt, phys, flags)`
    /// to `f`.
    fn walk_user_pages(&self, f: impl FnMut(usize, usize, MapFlags));

    /// Mark the leaf mapping at `virt` copy-on-write in this space's own
    /// tables (not by touching the active ones).
    fn set_cow_bit(&self, virt: usize);

    /// [`set_cow_bit`](Self::set_cow_bit) for a page that is already
    /// copy-on-write, where a re-mark is a no-op rather than an assertion.
    fn set_cow_bit_quietly(&self, virt: usize);

    /// Invalidate this space's TLB entries if it is the active one.
    fn flush_if_active(&self);

    /// The frame mapped at `virt`, if any.
    fn phys_for_virt(&self, virt: usize) -> Option<usize>;
}

/// The page tables: installing the kernel's, and the questions asked about the
/// active one from outside a process's address space.
pub trait Paging {
    /// The address-space type this machine's page tables are.
    type Space: AddressSpaceOps;

    /// Build and install the kernel's page tables from the boot record, and
    /// answer the kernel root's value.
    ///
    /// # Safety
    /// Runs before any mapping exists beyond the loader's; the caller passes
    /// the record the loader wrote and nothing else.
    unsafe fn install(
        info: &crate::boot::BootInfo,
        sections: &crate::mm::vm::KernelSections,
    ) -> usize;

    /// Map a device's register window into the kernel half.
    fn map_mmio(virt: usize, phys: usize, len: usize);

    /// Translate a kernel-visible address in the *active* space.
    fn translate(virt: usize) -> Option<(usize, MapFlags)>;

    /// Whether `start..start + len` is mapped in the active space in a way the
    /// caller's access (`write`) is allowed by.
    fn user_accessible(start: usize, len: usize, write: bool) -> bool;

    /// Whether the leaf mapping at `virt` is marked copy-on-write.
    fn is_cow_page(virt: usize) -> bool;

    /// Write a `u32` to a user address through `root`'s tables, answering
    /// whether it could (used to place a signal trampoline into a space that
    /// is not the active one).
    fn write_user_u32(root: usize, at: usize, value: u32) -> bool;

    /// The root of the address space currently active on this CPU.
    fn active_root() -> usize;

    /// The root of the kernel's own address space.
    fn kernel_root() -> usize;

    /// Make `root` the active space.
    fn switch_to_root(root: usize);

    /// Switch back to the kernel's own space.
    fn switch_to_kernel();

    /// The frame backing `virt` in the space with root `root`, if any.
    fn phys_for_virt_of(root: usize, virt: usize) -> Option<usize>;

    /// Like [`phys_for_virt_of`](Self::phys_for_virt_of), but only for a page
    /// the user may touch (the walk asks for `USER`).
    fn user_phys_for_virt_of(root: usize, virt: usize) -> Option<usize>;
}

/// A machine this kernel can run on. `Current` names the one being built.
pub trait Platform:
    Clock + WallClock + Cpu + Interrupts + Paging + Contexts + Sized + 'static
{
}

/// A task's saved machine state: what a context switch swaps.
pub trait ContextOps {
    /// The floating-point half, which a switch does not save wholesale - it is
    /// saved when a task gives up the FPU and restored when it takes it back.
    type Fpu: FpuOps;

    /// A context with nothing in it: a task that has never run, whose first
    /// switch-in falls out of the bootstrap the machine prepared for it.
    fn empty() -> Self;

    /// This context's FPU state.
    fn fpu(&self) -> &Self::Fpu;

    /// The same, to write.
    fn fpu_mut(&mut self) -> &mut Self::Fpu;
}

/// A task's floating-point state, as an opaque value the machine can copy.
pub trait FpuOps {
    /// The boot-state value: no register of userland has been touched yet.
    fn reset() -> Self;

    /// The state as the bytes a debugger reads. On x86_64 `PTRACE_GETFPREGS` is
    /// the machine's own save area, so the two shapes are one.
    fn to_bytes(&self) -> [u8; FPU_BYTES];

    /// The inverse.
    fn from_bytes(bytes: &[u8; FPU_BYTES]) -> Self;
}

/// The register state a task resumes ring 3 with.
///
/// Opaque on purpose: what a caller above the seam does with it is copy it or
/// redirect it, never name a register. The machine's register names are legal
/// in exactly one other place, the x86_64 Linux ABI layer, because there the
/// ABI *is* the register layout.
pub trait UserFrameOps: Copy {
    /// A `fork`/`clone` child's frame: the caller's registers, resuming on
    /// `child_stack` when it is non-zero and on the caller's own stack when it
    /// is zero. The return-value register is inherited rather than zeroed - the
    /// callers that want a zero hand in a frame whose result slot already is.
    fn forked(&self, child_stack: usize) -> Self;

    /// Write the syscall's return value into the frame.
    fn set_result(&mut self, value: isize);

    /// The instruction pointer, for a signal handler's frame.
    fn ip(&self) -> usize;

    /// The same, to write.
    fn set_ip(&mut self, ip: usize);

    /// The stack pointer.
    fn sp(&self) -> usize;

    /// The same, to write.
    fn set_sp(&mut self, sp: usize);

    /// This frame in the Linux `user_regs_struct` field order, written into
    /// `out` (whose length is the ABI's register count), with `tls` as the
    /// thread pointer.
    ///
    /// This is the one place the machine's registers and the ABI's field order
    /// meet, and it belongs to the machine: *which ABI field a register goes
    /// in* is exactly what differs between one machine's frame and another's.
    fn to_user_regs(&self, tls: usize, out: &mut [u64]);

    /// The inverse: the frame a tracer's register file describes.
    fn from_user_regs(regs: &[u64]) -> Self;

    /// Hand the CPU to this frame. Never returns.
    ///
    /// # Safety
    /// The frame must describe a ring-3 entry or resume point; the caller is
    /// the scheduler, which only builds them from a task's own record.
    unsafe fn resume(&self) -> !;
}

/// The task context: the machine's saved state, its floating-point half, and
/// the frame a task resumes ring 3 with.
pub trait Contexts {
    /// The saved kernel context a switch swaps.
    type Context: ContextOps<Fpu = Self::Fpu>;

    /// The floating-point state.
    type Fpu: FpuOps;

    /// The ring-3 register state.
    type UserFrame: UserFrameOps;

    /// Bytes [`FpuOps::to_bytes`] moves, and the width of the FPU save area.
    const FPU_BYTES: usize;

    /// Whether the FPU is trapped: a task's state is live in the registers and
    /// must be saved before anything else touches them.
    fn fpu_trapped() -> bool;

    /// Save the live FPU registers into a task's state.
    ///
    /// # Safety
    /// Must be called with the FPU belonging to the task being saved.
    unsafe fn save_live_fpu(state: &mut Self::Fpu);

    /// Load a task's FPU state into the registers.
    ///
    /// # Safety
    /// As above, for the task being restored.
    unsafe fn restore_fpu(state: &Self::Fpu);

    /// Take ownership of the live FPU registers for the current task.
    ///
    /// # Safety
    /// Only from the context that is actually running.
    unsafe fn claim_live_fpu();

    /// Give the FPU back to "no task owns it".
    ///
    /// # Safety
    /// Only from the context that actually holds it.
    unsafe fn reset_live_fpu();

    /// Point the CPU's privilege-transition stack at `top` (the TSS's ring-0
    /// stack on x86_64), so the next trap from ring 3 lands on this task's.
    fn set_kernel_stack(top: usize);

    /// Prepare a kernel task's first context: `entry` runs on the top of the
    /// stack, with `slot` in the register the entry point takes it in, and
    /// `interruptible` deciding whether the task may park.
    ///
    /// # Safety
    /// `stack_top` must be the top of a stack nothing else uses.
    unsafe fn bootstrap(
        stack_top: usize,
        entry: extern "C" fn(usize) -> !,
        slot: usize,
        interruptible: bool,
    ) -> Self::Context;

    /// Switch from one saved context to another. Only ever called from the
    /// scheduler's own switch path, holding no lock.
    ///
    /// # Safety
    /// Both pointers must outlive the switch and refer to contexts of live
    /// tasks.
    unsafe fn switch(from: *mut Self::Context, to: *const Self::Context);

    /// Enter ring 3 with `frame`. Never returns.
    ///
    /// # Safety
    /// The frame must be a user task's entry or resume point.
    unsafe fn resume_user(frame: *const Self::UserFrame) -> !;

    /// Enter ring 3 for the first time, at `entry` on `user_rsp`.
    ///
    /// # Safety
    /// `entry` must be a mapped user address and `user_rsp` a usable stack.
    unsafe fn enter_user(entry: usize, user_rsp: usize) -> !;

    /// The `cs`/`ss` a ring-3 frame carries on this machine, as the Linux
    /// `user_regs_struct` reports them. Asked by the ABI layer, which fills
    /// those two fields of a frame it builds from the syscall gate's record.
    fn user_segments() -> (usize, usize);
}

/// The platform this build targets.
///
/// A unit type, and deliberately *owned by this module rather than by an arch
/// module*: the arch module implements the traits for it (so `arch → hal` is
/// the only arrow between them, where a `hal`-names-`arch` alias would make the
/// pair mutual - `cargo xtask depgraph` counts that as a cycle). Only one
/// architecture is ever compiled, so there is exactly one set of impls.
#[cfg(target_arch = "x86_64")]
pub struct Current;

// ---------------------------------------------------------------------------
// The forwards. Everything above the HAL calls these; they are one-line static
// dispatches onto `Current`, so a call site never names an architecture.
// ---------------------------------------------------------------------------

/// Ticks per second of [`ticks`].
pub const TICK_HZ: u32 = <Current as Clock>::TICK_HZ;

/// Monotonic tick count since boot.
#[inline]
pub fn ticks() -> u64 {
    <Current as Clock>::ticks()
}

/// Nanoseconds since boot, finer than a tick where the machine can say so.
#[inline]
pub fn since_boot_nanos() -> u64 {
    <Current as Clock>::since_boot_nanos()
}

/// Spin for approximately `ms`. Boot-only; see [`Clock::busy_wait_ms`].
#[inline]
pub fn busy_wait_ms(ms: u64) {
    <Current as Clock>::busy_wait_ms(ms)
}

/// The Unix epoch the boot anchored wall time at.
#[inline]
pub fn boot_epoch() -> u64 {
    <Current as WallClock>::boot_epoch()
}

/// Anchor the boot epoch, as `settimeofday` does.
#[inline]
pub fn set_boot_epoch(epoch: u64) {
    <Current as WallClock>::set_boot_epoch(epoch)
}

/// Write `epoch` to the persistent clock; `false` if there is none.
#[inline]
pub fn write_epoch(epoch: u64) -> bool {
    <Current as WallClock>::write_epoch(epoch)
}

/// Whether this core would take an interrupt right now.
#[inline]
pub fn interrupts_enabled() -> bool {
    <Current as Cpu>::interrupts_enabled()
}

/// Let the core take interrupts.
#[inline]
pub fn enable_interrupts() {
    <Current as Cpu>::enable_interrupts()
}

/// Stop the machine for good.
#[inline]
pub fn halt() -> ! {
    <Current as Cpu>::halt()
}

/// Wait for the next interrupt, returning with interrupts off.
#[inline]
pub fn idle() {
    <Current as Cpu>::idle()
}

/// Turn on the SIMD/FP state userland expects.
#[inline]
pub fn enable_user_simd() {
    <Current as Cpu>::enable_user_simd()
}

/// Bring up interrupts: descriptors, controller, clock.
#[inline]
pub fn init_interrupts() {
    <Current as Cpu>::init_interrupts()
}

/// Allow delivery on `line`.
#[inline]
pub fn unmask(line: Line) {
    <Current as Interrupts>::unmask(line)
}

/// Block delivery on `line`.
#[inline]
pub fn mask(line: Line) {
    <Current as Interrupts>::mask(line)
}

/// Acknowledge the interrupt in service on `line`.
#[inline]
pub fn eoi(line: Line) {
    <Current as Interrupts>::eoi(line)
}

/// Bring up message-signalled delivery before a device arms MSI-X.
#[inline]
pub fn init_message_signalled() {
    <Current as Interrupts>::init_message_signalled()
}

/// The MSI-X vector the network adapter's message-table entry must carry.
#[inline]
pub fn net_msi_vector() -> u8 {
    <Current as Interrupts>::net_msi_vector()
}

/// One process's address space, as this machine's page tables.
///
/// Reached only through the trait, so a caller never names the machine's type
/// and a port's type takes its place without an edit here.
pub type AddressSpace = <Current as Paging>::Space;

/// Build and install the kernel's page tables from the boot record.
///
/// # Safety
/// Runs before any mapping exists beyond the loader's; see [`Paging::install`].
#[inline]
pub unsafe fn install(info: &crate::boot::BootInfo, sections: &KernelSections) -> usize {
    unsafe { <Current as Paging>::install(info, sections) }
}

/// Map a device's register window into the kernel half.
#[inline]
pub fn map_mmio(virt: usize, phys: usize, len: usize) {
    <Current as Paging>::map_mmio(virt, phys, len)
}

/// Translate a kernel-visible address in the active address space.
#[inline]
pub fn translate(virt: usize) -> Option<(usize, MapFlags)> {
    <Current as Paging>::translate(virt)
}

/// Whether the active space maps `start..start + len` accessibly for `write`.
#[inline]
pub fn user_accessible(start: usize, len: usize, write: bool) -> bool {
    <Current as Paging>::user_accessible(start, len, write)
}

/// Whether the leaf mapping at `virt` is copy-on-write.
#[inline]
pub fn is_cow_page(virt: usize) -> bool {
    <Current as Paging>::is_cow_page(virt)
}

/// Write a `u32` to a user address through `root`'s tables.
#[inline]
pub fn write_user_u32(root: usize, at: usize, value: u32) -> bool {
    <Current as Paging>::write_user_u32(root, at, value)
}

/// The root of the active address space.
#[inline]
pub fn active_root() -> usize {
    <Current as Paging>::active_root()
}

/// The root of the kernel's own address space.
#[inline]
pub fn kernel_root() -> usize {
    <Current as Paging>::kernel_root()
}

/// Make `root` the active address space.
#[inline]
pub fn switch_to_root(root: usize) {
    <Current as Paging>::switch_to_root(root)
}

/// Switch back to the kernel's own address space.
#[inline]
pub fn switch_to_kernel() {
    <Current as Paging>::switch_to_kernel()
}

/// The frame backing `virt` in the space with root `root`.
#[inline]
pub fn phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
    <Current as Paging>::phys_for_virt_of(root, virt)
}

/// The frame backing `virt` in `root`, only if the user may touch it.
#[inline]
pub fn user_phys_for_virt_of(root: usize, virt: usize) -> Option<usize> {
    <Current as Paging>::user_phys_for_virt_of(root, virt)
}

/// A task's saved kernel context.
pub type Context = <Current as Contexts>::Context;

/// A task's floating-point state.
pub type Fpu = <Current as Contexts>::Fpu;

/// A task's ring-3 register state.
pub type UserFrame = <Current as Contexts>::UserFrame;

/// Bytes of FPU state, and the width of its save area.
///
/// A crate-level const rather than only the trait's associated one, because it
/// is used as an array length (`[u8; FPU_BYTES]`) and an associated const in an
/// array length needs `generic_const_exprs`.
pub const FPU_BYTES: usize = <Current as Contexts>::FPU_BYTES;

/// Whether the FPU's state is live in the registers right now.
#[inline]
pub fn fpu_trapped() -> bool {
    <Current as Contexts>::fpu_trapped()
}

/// Save the live FPU registers into a task's state.
#[inline]
pub unsafe fn save_live_fpu(state: &mut Fpu) {
    unsafe { <Current as Contexts>::save_live_fpu(state) }
}

/// Load a task's FPU state into the registers.
#[inline]
pub unsafe fn restore_fpu(state: &Fpu) {
    unsafe { <Current as Contexts>::restore_fpu(state) }
}

/// Take ownership of the live FPU registers for the current task.
#[inline]
pub unsafe fn claim_live_fpu() {
    unsafe { <Current as Contexts>::claim_live_fpu() }
}

/// Give the FPU back to "no task owns it".
#[inline]
pub unsafe fn reset_live_fpu() {
    unsafe { <Current as Contexts>::reset_live_fpu() }
}

/// Point the CPU's privilege-transition stack at `top`.
#[inline]
pub fn set_kernel_stack(top: usize) {
    <Current as Contexts>::set_kernel_stack(top)
}

/// Prepare a kernel task's first context.
///
/// # Safety
/// `stack_top` must be the top of a stack nothing else uses.
#[inline]
pub unsafe fn bootstrap(
    stack_top: usize,
    entry: extern "C" fn(usize) -> !,
    slot: usize,
    interruptible: bool,
) -> Context {
    unsafe { <Current as Contexts>::bootstrap(stack_top, entry, slot, interruptible) }
}

/// Switch from one saved context to another.
///
/// # Safety
/// Both pointers must outlive the switch and refer to live tasks' contexts.
#[inline]
pub unsafe fn switch_context(from: *mut Context, to: *const Context) {
    unsafe { <Current as Contexts>::switch(from, to) }
}

/// Enter ring 3 with `frame`. Never returns.
///
/// # Safety
/// The frame must be a user task's entry or resume point.
#[inline]
pub unsafe fn resume_user(frame: *const UserFrame) -> ! {
    unsafe { <Current as Contexts>::resume_user(frame) }
}

/// Enter ring 3 for the first time, at `entry` on `user_rsp`.
///
/// # Safety
/// `entry` must be a mapped user address and `user_rsp` a usable stack.
#[inline]
pub unsafe fn enter_user(entry: usize, user_rsp: usize) -> ! {
    unsafe { <Current as Contexts>::enter_user(entry, user_rsp) }
}

/// The `cs`/`ss` a ring-3 frame carries on this machine.
#[inline]
pub fn user_segments() -> (usize, usize) {
    <Current as Contexts>::user_segments()
}
