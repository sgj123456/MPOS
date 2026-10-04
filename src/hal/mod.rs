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
//! every method would be the same leak with one import instead of a blob, which is
//! the failure `PLAN.md` §11.3 warned about when it rejected a "generic HAL
//! trait forest" - this is deliberately a trait per concern, not one trait per
//! function. The *baseline* concerns (`Clock`, `WallClock`, `Cpu`, `Interrupts`,
//! `Paging`, `Contexts`, `Console`) are what every machine this kernel boots on
//! has. On top of them sit the *optional capability traits* (`Msi`, `SerialRing`,
//! `FpuTrap`, `Segments`) for the ways one machine does something another has no
//! hardware for; a port implements the baseline it must and only the capabilities
//! it has, so an absent capability is *absent* rather than a method every machine
//! has to fake an answer for. Which of them a build has is a `mpos_*` flag
//! declared per target in `.cargo/config.toml`, never an architecture name - the
//! forwards below say `#[cfg(mpos_msi)]`, never a machine name, so this file
//! itself knows no architecture.
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

    /// Point this core's thread pointer at `base`. The register a `TPIDR_EL0` /
    /// `%fs`-relative access resolves against, so it is one per core and every
    /// task that owns thread-local storage must have the scheduler reinstall it
    /// on dispatch - which is why it is a machine capability rather than a field
    /// in the task table: x86_64 writes `IA32_FS_BASE`, aarch64 writes
    /// `TPIDR_EL0`, and the register name is the machine's to know.
    fn set_thread_pointer(base: usize);

    /// The value [`set_thread_pointer`](Cpu::set_thread_pointer) last installed
    /// on this core. A read-back only - the boot check writes a sentinel and
    /// proves the register took it - so a port that cannot read its thread
    /// register may return the value it most recently wrote rather than the live
    /// register, but must never claim a write the hardware dropped.
    fn thread_pointer() -> usize;
}

/// An interrupt line this kernel masks and unmasks by name.
///
/// Only the lines that cross the seam are named - a driver that must let
/// delivery through says *which line it is on*, never which controller bit,
/// because on another machine the line is not a PIC input at all. A
/// message-signalled device is not one of these: it has no line to mask, and
/// asks [`Msi::net_msi_vector`] instead - and that capability belongs to the
/// machines that have an MSI controller, so it is [`Msi`], not [`Interrupts`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Line {
    /// The programmable interval timer, the scheduler's clock.
    Timer,
    /// The first serial port (COM1).
    Serial,
}

/// The interrupt controller: masking a named line, acknowledging one. The
/// lines are [`Line`]. A machine that delivers by a message-signalled write
/// rather than a wired line expresses that as [`Msi`], an optional capability
/// beside this - not more of it.
pub trait Interrupts {
    /// Allow delivery on `line`.
    fn unmask(line: Line);

    /// Block delivery on `line`.
    fn mask(line: Line);

    /// Acknowledge the interrupt in service on `line`, so the controller may
    /// deliver the next one at or below its priority. Called from interrupt
    /// context, so it must not lock, allocate or fault.
    fn eoi(line: Line);
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

    /// Map a device's MMIO window and *return* the kernel-virtual address the
    /// caller reads its registers through - the allocation-shaped question a
    /// driver asks, as opposed to [`map_mmio`](Self::map_mmio)'s
    /// program-a-given-window primitive. The two machines answer it from
    /// opposite mechanisms: x86_64 bump-allocates one fresh kernel-half mapping
    /// per window (`crate::mm::map_mmio`), because a PCIe BAR has no fixed
    /// address; aarch64 answers from the device window the boot maps whole, so
    /// the range is already reachable and the answer is an offset into it
    /// (`paging::device_virt`). `None` means this machine cannot map that
    /// physical range - the honest refusal a caller reads, not a bogus address.
    ///
    /// This is the seam `PLAN.md` §9.1 names as the first unit of the aarch64
    /// `drivers` stage: it lets `drivers::virtio` reach a device's registers
    /// through `crate::hal` (the one machine-call route every layer above `arch`
    /// is allowed) instead of naming `crate::mm::map_mmio`, which is x86_64-only.
    fn map_mmio_window(phys: usize, len: usize) -> Option<usize>;

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

/// The machine's byte sink: the wire the kernel log and the `println!` go to.
///
/// A baseline concern, not an optional capability, because every machine this
/// kernel boots on has exactly one - on x86_64 the 16550 at COM1, on aarch64 the
/// PL011. What is *not* universal is how the wire is made fast: an
/// interrupt-driven transmit ring a task can queue into and sleep on is x86_64's
/// arrangement (and a future aarch64 one), so that is the optional [`SerialRing`]
/// beside this, while the polled write every machine can do lives here.
pub trait Console {
    /// Bring the sink up. Idempotent; the boot may already have poked the wire.
    fn init();

    /// Push bytes on the wire, waiting for the hardware - the polled write. The
    /// floor every machine can do and the only one a machine without a [`SerialRing`]
    /// uses: a `\n` in `bytes` becomes carriage-return-then-linefeed on the wire,
    /// because a terminal that gets only `\n` prints a staircase.
    fn write_bytes(bytes: &[u8]);

    /// Whether a [`SerialRing`] is live and `crate::console` should queue into it.
    /// `false` means "always the polled path, never park" - the honest answer for a
    /// machine that has no interrupting transmitter, not a lie about one it has.
    fn interrupt_driven() -> bool;
}

/// ---------------------------------------------------------------------------
/// Optional capability traits.
///
/// The baseline concerns above are what *every* machine this kernel runs on
/// has: a clock, a wall clock, a CPU, a wired interrupt controller, page tables,
/// task contexts, and a console sink. These four are not universal - they are one
/// machine's way of doing something another machine does differently or not at
/// all. A port implements the baseline and only the optional ones its hardware
/// actually has (`AGENTS.md` §4 rule 4: refuse rather than approximate), so a
/// capability is expressible as *absent* rather than as a method every machine
/// must answer. `crate::Platform` names only the baseline concerns; these are
/// reached through their own forwards, each gated on a `mpos_*` capability flag
/// the build declares for the target (`mpos_msi`, `mpos_serial_ring`,
/// `mpos_fpu_trap`, `mpos_segments`, `mpos_console_input` - the sets are in
/// `.cargo/config.toml`'s `[target.…]` blocks, beside each machine's linker
/// script). The gate names the capability, never the architecture, so this file
/// contains no machine name: a port declares the set it has where it already
/// declares its script.
/// ---------------------------------------------------------------------------

/// Message-signalled interrupt delivery: the local APIC page a PCI device maps
/// its MSI-X table through, and the vector such a table entry must carry.
///
/// This is a capability, not a concern, because it is a property of the
/// *interrupt controller and the bus*, not of the CPU. A machine whose network
/// transport is `virtio`-MMIO on a GIC has wired SPI lines and no MSI-X table at
/// all, so it implements [`Interrupts`] and simply is not an `Msi`. x86_64 is.
pub trait Msi {
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

/// The x87 lazy-FPU ownership protocol: which task currently owns the live FP
/// registers, and saving/restoring them around that ownership.
///
/// A capability, not part of [`Contexts`], because it is an answer to a specific
/// hardware arrangement: x86_64 leaves the FPU trapped (`CR0.TS`), faults an
/// `#NM` on the first FP instruction, and *there* saves the previous task's
/// state - so the scheduler must ask whether the registers are live before it
/// switches, and hand ownership over. A machine that saves FP registers eagerly
/// inside the context switch has no trap to fault on, so there is nothing to
/// query and nothing to hand over: it implements [`Contexts`] and is not a
/// `FpuTrap`. (The `Fpu` *type* and [`FpuOps`] stay in the baseline - every
/// machine that has floating point has state to save and bytes for a debugger to
/// read; only the trap-ownership scheme is x86's.)
pub trait FpuTrap: Contexts {
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
}

/// The segment selectors a ring-3 frame carries - a capability, not a concern,
/// because a machine without memory segmentation has no selectors to report.
///
/// x86_64's `user_regs_struct` has `cs`/`ss` fields, and a tracer's register file
/// reads them; the values come from the GDT. aarch64's `user_pt_regs` has no
/// equivalent - its frame is `x0..x30`/`sp`/`pc`/`pstate` - so that machine is
/// not a `Segments`, and the ABI layer that asks for `cs`/`ss` is x86-only with
/// it.
pub trait Segments {
    /// The `cs`/`ss` a ring-3 frame carries on this machine, as the Linux
    /// `user_regs_struct` reports them. Asked by the ABI layer, which fills
    /// those two fields of a frame it builds from the syscall gate's record.
    fn user_segments() -> (usize, usize);
}

/// An interrupt-driven transmit ring behind the [`Console`] sink: queue bytes,
/// sleep while the UART's own interrupt drains the wire, and wake the parked
/// writer when room appears.
///
/// A capability, not part of [`Console`], because it needs a machine that (1) has
/// an interrupting transmitter and (2) has a wired controller and scheduler to
/// park on. `virt`'s PL011 console at stage 4a is polled only - the GIC line
/// routing that would make it a `SerialRing` is stage 5 - so aarch64 implements
/// [`Console`] and simply is not a `SerialRing`; the console then always takes the
/// polled path and never parks. x86_64 is a `SerialRing`.
pub trait SerialRing {
    /// How many bytes fit in the ring right now.
    fn tx_space() -> usize;

    /// Copy as much of `bytes` as fits into the ring and return the count taken.
    fn tx_submit(bytes: &[u8]) -> usize;

    /// Put this task on the ring's wait list before it parks, so a drain wakes it.
    fn tx_wait_now();

    /// Take this task off the ring's wait list.
    fn tx_stop_waiting();
}

/// A machine's polled console *input* - the typed-byte source the tty reads
/// through. This is a capability, not part of [`Console`], because the two
/// machines get input differently: x86_64's serial receiver is interrupt-driven
/// and its handler drains the FIFO straight into `vfs::vcmux::route_input`, so
/// x86_64 is *not* a `ConsoleInput` at all; AArch64's `virt` PL011 is polled
/// (the GIC RX line is a later cut), so it implements this and the read path
/// asks the machine for a byte when it wants one. A machine with no typed input
/// of its own implements neither and its console has no reader.
pub trait ConsoleInput {
    /// Take one received byte, or `None` when the receiver holds nothing yet.
    fn poll_input() -> Option<u8>;

    /// Put one byte into the receiver through the machine's own loopback, with no
    /// terminal, so the read path can be exercised at bring-up. A *self-test hook*
    /// only - nothing at runtime calls it - which is why it lives on the input
    /// capability rather than being invented above the machine layer.
    fn inject_input(byte: u8);
}

/// The machine's own entropy sources: a CPU instruction (`RDRAND`), a device,
/// or - honestly - nothing.
///
/// A baseline concern rather than an optional capability, because the answer is
/// always defined: a machine folds in what it has, and a machine with neither a
/// generator instruction nor an entropy device folds in nothing, leaving the
/// clock mix in `crate::rand` as the floor. That floor is the same on every
/// machine and is not a claim of entropy - which sources exist, and how to read
/// them, is the machine's to know: x86_64 reads `RDRAND` and its PCI virtio-rng,
/// aarch64 its MMIO virtio-rng, and `crate::rand` knows none of that.
pub trait Entropy {
    /// Fold whatever this machine can ask for into `seed`. Bytes are XORed in,
    /// so a weak source cannot make the seed worse than the clock alone.
    fn mix_into(seed: &mut [u8; 32]);
}

/// A machine this kernel can run on. `Current` names the one being built.
pub trait Platform:
    Clock + WallClock + Cpu + Interrupts + Paging + Contexts + Console + Entropy + Sized + 'static
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
}

/// The platform this build targets.
///
/// A unit type, and deliberately *owned by this module rather than by an arch
/// module*: the arch module implements the traits for it (so `arch → hal` is
/// the only arrow between them, where a `hal`-names-`arch` alias would make the
/// pair mutual - `cargo xtask depgraph` counts that as a cycle). Only one
/// architecture is ever compiled, so there is exactly one set of impls.
#[cfg(target_os = "none")]
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

/// Fold this machine's entropy into `seed` (see [`Entropy`]).
#[inline]
pub fn mix_entropy(seed: &mut [u8; 32]) {
    <Current as Entropy>::mix_into(seed)
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

/// Point this core's thread pointer at `base` (see [`Cpu::set_thread_pointer`]).
/// The scheduler's per-dispatch reinstall and `set_tls`-style syscalls call this.
#[inline]
pub fn set_thread_pointer(base: usize) {
    <Current as Cpu>::set_thread_pointer(base)
}

/// The thread pointer this core is running with (see [`Cpu::thread_pointer`]).
#[inline]
pub fn thread_pointer() -> usize {
    <Current as Cpu>::thread_pointer()
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
///
/// Present only where the build declares the [`Msi`] capability (`mpos_msi`,
/// set per target in `.cargo/config.toml`); a `virt`+GIC machine declares none
/// and this does not exist for it.
#[cfg(mpos_msi)]
#[inline]
pub fn init_message_signalled() {
    <Current as Msi>::init_message_signalled()
}

/// The MSI-X vector the network adapter's message-table entry must carry.
#[cfg(mpos_msi)]
#[inline]
pub fn net_msi_vector() -> u8 {
    <Current as Msi>::net_msi_vector()
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

/// Map a device's MMIO window and return the kernel-virtual address to read it
/// through, or `None` if this machine cannot map that physical range.
#[inline]
pub fn map_mmio_window(phys: usize, len: usize) -> Option<usize> {
    <Current as Paging>::map_mmio_window(phys, len)
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
#[cfg(mpos_fpu_trap)]
#[inline]
pub fn fpu_trapped() -> bool {
    <Current as FpuTrap>::fpu_trapped()
}

/// Save the live FPU registers into a task's state.
#[cfg(mpos_fpu_trap)]
#[inline]
pub unsafe fn save_live_fpu(state: &mut Fpu) {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { <Current as FpuTrap>::save_live_fpu(state) }
}

/// Load a task's FPU state into the registers.
#[cfg(mpos_fpu_trap)]
#[inline]
pub unsafe fn restore_fpu(state: &Fpu) {
    // SAFETY: as above.
    unsafe { <Current as FpuTrap>::restore_fpu(state) }
}

/// Take ownership of the live FPU registers for the current task.
#[cfg(mpos_fpu_trap)]
#[inline]
pub unsafe fn claim_live_fpu() {
    // SAFETY: as above.
    unsafe { <Current as FpuTrap>::claim_live_fpu() }
}

/// Give the FPU back to "no task owns it".
#[cfg(mpos_fpu_trap)]
#[inline]
pub unsafe fn reset_live_fpu() {
    // SAFETY: as above.
    unsafe { <Current as FpuTrap>::reset_live_fpu() }
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
#[cfg(mpos_segments)]
#[inline]
pub fn user_segments() -> (usize, usize) {
    <Current as Segments>::user_segments()
}

// ---------------------------------------------------------------------------
// The console sink. `Console` is baseline (both machines implement it); the
// `SerialRing` queue-and-park path and the polled `ConsoleInput` receiver are
// optional capabilities, each forward gated on the `mpos_*` flag the build
// declares for it, so a machine without one neither has the method nor names it.
// ---------------------------------------------------------------------------

/// Bring the console sink up.
#[inline]
pub fn console_init() {
    <Current as Console>::init()
}

/// Write bytes on the console sink, polled - the `\n`-to-`\r\n` rule is the
/// machine's, so a bare `\n` never reaches a terminal as a staircase.
#[inline]
pub fn console_write_bytes(bytes: &[u8]) {
    <Current as Console>::write_bytes(bytes)
}

/// Whether the console sink has a live [`SerialRing`] to queue into (`false` on a
/// polled-only machine, where the console never parks).
#[inline]
pub fn console_interrupt_driven() -> bool {
    <Current as Console>::interrupt_driven()
}

/// How many bytes the transmit ring has room for right now.
#[cfg(mpos_serial_ring)]
#[inline]
pub fn serial_tx_space() -> usize {
    <Current as SerialRing>::tx_space()
}

/// Copy as much of `bytes` as the ring will take, returning the count.
#[cfg(mpos_serial_ring)]
#[inline]
pub fn serial_tx_submit(bytes: &[u8]) -> usize {
    <Current as SerialRing>::tx_submit(bytes)
}

/// Join this task to the ring's wait list before parking on it.
#[cfg(mpos_serial_ring)]
#[inline]
pub fn serial_tx_wait_now() {
    <Current as SerialRing>::tx_wait_now()
}

/// Leave the ring's wait list.
#[cfg(mpos_serial_ring)]
#[inline]
pub fn serial_tx_stop_waiting() {
    <Current as SerialRing>::tx_stop_waiting()
}

/// Poll the console receiver for one typed byte (`None` when the FIFO is empty).
/// Present only where the build declares the polled [`ConsoleInput`] capability
/// (`mpos_console_input`); a machine that feeds input from an interrupt has no
/// receiver to poll and this does not exist for it.
#[cfg(mpos_console_input)]
#[inline]
pub fn console_poll_input() -> Option<u8> {
    <Current as ConsoleInput>::poll_input()
}

/// Put one byte into the receiver through the UART's loopback, for the bring-up
/// read-path self-test only.
#[cfg(mpos_console_input)]
#[inline]
pub fn console_inject_input(byte: u8) {
    <Current as ConsoleInput>::inject_input(byte)
}
