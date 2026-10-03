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

/// A machine this kernel can run on. `Current` names the one being built.
pub trait Platform: Clock + WallClock + Cpu + Interrupts + Sized + 'static {}

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
