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
//! edit to the modules that use them (`NOTES.md` §9.1).
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
//! the failure `NOTES.md` §11.3 warned about when it rejected a "generic HAL
//! trait forest" - this is deliberately a trait per concern, not one trait per
//! function. The *baseline* concerns (`Clock`, `WallClock`, `Cpu`, `Interrupts`,
//! `Paging`, `Contexts`, `Console`) are what every machine this kernel boots on
//! has. On top of them sit the *optional capability traits* (`Msi`, `SerialRing`,
//! `FpuTrap`, `SignalFrame`) for the ways one machine does something another has no
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
/// and its home is `mpos-vm`, beside [`Mapping`] - a leaf both this module and
/// the kernel depend on, named directly because this module travels into
/// `mpos-arch` with the machine code and may not reach up into the kernel for
/// it (`NOTES.md` §14.2).
pub use mpos_vm::PageTaken;
use mpos_vm::{KernelSections, MapFlags, Mapping};

/// The seam's other face: what the machine layer asks of the *kernel*.
///
/// Every other trait in this module answers "how does this machine do X". This
/// one is the opposite direction - a fault handler needs to know whether a page
/// is one to populate, and a `svc` needs the syscall table - and it is the last
/// thing that keeps `arch` inside the kernel crate. It lives inside `hal`
/// because it is the same boundary, and it travels into `mpos-arch` with the
/// rest of this module (`NOTES.md` §14.2).
pub mod kernel;

/// The console's transmit queue: the implementation behind [`SerialRing`].
///
/// It is here - and not inside either machine's UART - because it is *machine
/// neutral*: it is parameterised by a [`tx_ring::Transmitter`], knows no
/// register, and both machines drain it through the same three operations. What
/// it holds is the reasoning neither UART owns: when to arm the transmitter,
/// who sleeps for room, what one visit from the interrupt hands over. A machine
/// that implements [`SerialRing`] names it; a machine whose transmitter does
/// not interrupt (`mpos_serial_ring` absent) has no queue to drain and does not
/// compile it.
#[cfg(mpos_serial_ring)]
pub mod tx_ring;

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

    /// Whether this machine can interrupt *between* two clock ticks, so a wait
    /// shorter than one ends when it is due rather than at the next tick.
    ///
    /// The scheduler consults this before it bothers computing a nanosecond
    /// deadline: a machine whose only wake-up source is the periodic tick sleeps
    /// in ticks, and saying so with `false` is the honest shape rather than a
    /// silent rounding. `docs/notes/` §10.7 records which machine answers which.
    const SUB_TICK_WAKE: bool = false;

    /// Arm a one-shot interrupt at `deadline_ns` (nanoseconds since boot), or
    /// cancel whatever is armed when it is `None`.
    ///
    /// The periodic tick stays armed either way: this only sharpens a wake-up
    /// that lands between two ticks, so a lost one-shot costs a tick's latency
    /// and never the machine. The default does nothing, which is what a machine
    /// with no sub-tick source wants.
    fn arm_wake_ns(_deadline_ns: Option<u64>) {}
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

    /// This core's stack pointer, read fresh.
    ///
    /// The one machine question a dying kernel asks about *where* it died: a
    /// panic's stack walk and the stack-use report beside it both need the live
    /// value, and "which register holds it" is the machine's business (`rsp`
    /// here, `sp` there). x86_64's 16550 keep the same split - everything above
    /// the register is shared.
    fn stack_pointer() -> usize;
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
    unsafe fn install(info: &mpos_bootinfo::BootInfo, sections: &mpos_vm::KernelSections) -> usize;

    /// Map a device's register window into the kernel half.
    fn map_mmio(virt: usize, phys: usize, len: usize);

    /// Map a device's MMIO window and *return* the kernel-virtual address the
    /// caller reads its registers through - the allocation-shaped question a
    /// driver asks, as opposed to [`map_mmio`](Self::map_mmio)'s
    /// program-a-given-window primitive. The two machines answer it from
    /// opposite mechanisms: x86_64 bump-allocates one fresh kernel-half mapping
    /// per window (`arch/x86_64/mmio.rs`), because a PCIe BAR has no fixed
    /// address; aarch64 answers from the device window the boot maps whole, so
    /// the range is already reachable and the answer is an offset into it
    /// (`paging::device_virt`). `None` means this machine cannot map that
    /// physical range - the honest refusal a caller reads, not a bogus address.
    ///
    /// This is the seam `NOTES.md` §9.1 names as the first unit of the aarch64
    /// `drivers` stage: it lets `drivers::virtio` reach a device's registers
    /// through `crate::hal` (the one machine-call route every layer above `arch`
    /// is allowed) instead of naming a per-machine window allocator.
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
/// interrupt-driven transmit ring a task can queue into and sleep on is one
/// machine's arrangement rather than every machine's - it needs a transmitter
/// with an interrupt *and* a controller that routes it - so that is the optional
/// [`SerialRing`] beside this, while the polled write every machine can do lives
/// here. Both machines have such a transmitter today (the 16550's line at IRQ4
/// through the PIC, the PL011's SPI through the GIC), so both are `SerialRing`s
/// and neither describes the other's wire.
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
/// `mpos_fpu_trap`, `mpos_signal_frame`, `mpos_console_input` - the sets are in
/// `.cargo/config.toml`'s `[target.…]` blocks, beside each machine's linker
/// script). The gate names the capability, never the
/// architecture, so this file contains no machine name: a port declares the set
/// it has where it already declares its script.
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

/// The `siginfo_t` fields a handler's frame carries, in the shape the ABI layer
/// holds them: the signal, the error, the code, the sender, and the `sigqueue`
/// payload.
///
/// A plain struct rather than `sched`'s `SigInfo`, because `hal` names no layer
/// above it - the caller unpacks its own type into this one.
#[derive(Clone, Copy, Debug, Default)]
pub struct SignalInfo {
    pub signo: i32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: i32,
    /// The `union sigval` a `sigqueue` sent, or zero for a plain `kill`.
    pub value: u64,
    /// The child's exit code, or the signal that ended it: `si_status`, for the
    /// one sender that has one.
    pub status: i32,
    /// The child's CPU time in `USER_HZ` ticks: `si_utime`/`si_stime`, the other
    /// half of the `_sigchld` member. Zero for every other sender, and already
    /// converted by the caller (`crate::abi::clock_ticks_to_user_hz`), so a
    /// machine's frame writer has one unit to write on both.
    pub utime: u64,
    pub stime: u64,
}

impl SignalInfo {
    /// Whether this record's union member is the `_sigchld` one, which decides
    /// what the word at offset 24 holds: the *status*, or the `sigval`.
    ///
    /// Linux's own switch: `copy_siginfo_to_user` picks the union by the
    /// code's class, and the `CLD_*` codes are the positive ones 1..=6 - a
    /// normal `SI_USER` is 0, `SI_KERNEL` 0x80, and every other sender's code
    /// is negative. It lives here because both machines lay a frame out and
    /// the rule is neither machine's: two writers spelling `1..=6` themselves
    /// is how one of them comes to spell it `1..=5`.
    pub fn chld_union(&self) -> bool {
        (1..=6).contains(&self.code)
    }
}

/// What the ABI layer has decided about the handler about to run: everything
/// the machine needs to lay a frame out, beyond the register file itself.
///
/// The register file is passed separately because it is read *and* written -
/// [`SignalFrame::enter`] hands back the handler's entry state through it.
#[derive(Clone, Copy, Debug, Default)]
pub struct SignalDelivery {
    /// The handler's entry address.
    pub handler: usize,
    /// The restorer the disposition provides - where the handler's `ret` goes,
    /// at the front of the frame - or `None` when it has none. Which flag says
    /// so is the disposition's own business; what this carries is the answer,
    /// so a machine whose ABI makes the field mandatory can refuse a frame it
    /// could not lay out.
    pub restorer: Option<usize>,
    /// The call number to run again after the handler returns, or `None` to
    /// resume the interrupted code.
    pub restart: Option<u64>,
    /// The mask to put back, as the wire's `sigset_t`.
    pub blocked_wire: u64,
    /// The signal and its sender.
    pub info: SignalInfo,
    /// The alternate stack's `(base, size)`, described in the frame's `uc_stack`
    /// whether or not the handler runs on it.
    pub alt: Option<(usize, usize)>,
    /// Whether the handler runs *on* that stack - the disposition's
    /// `SA_ONSTACK`.
    pub on_alt: bool,
    /// The interrupted floating-point image, for a machine whose frame carries
    /// one but whose trap frame does not hold it.
    ///
    /// x86_64 is that machine: its `sigcontext` names the image by pointer, and
    /// the registers themselves live in the task's record (the lazy-FPU scheme
    /// saves them there and nowhere else), which the machine may not read -
    /// `arch -> sched` is the one upward edge the seam exists to keep out. So
    /// the caller, which *can* ask the scheduler, hands the image over here and
    /// [`SignalFrame::enter`] writes it into the frame.
    ///
    /// aarch64 is the other shape: its trap frame carries the interrupted
    /// registers and `enter` copies them out of that, so this is `None` there
    /// and unread. `None` on x86_64 is "this caller could not produce one", which
    /// the frame answers with the architectural reset state - not with an all-zero
    /// block, whose `mxcsr` of zero would unmask every SSE exception, and not with
    /// a null `sigcontext.fpstate`, which Linux's `rt_sigreturn` reads as "reset
    /// the registers" (measured, `.workbuddy/probe_signal_fp2.c`'s
    /// `NO_RESTORE` run).
    pub fpu: Option<[u8; FPU_BYTES]>,
}

/// What a handler's frame restores beyond the register file.
///
/// The second half of [`SignalFrame::exit`]'s answer, and the counterpart of
/// [`SignalDelivery`]'s `fpu` field: the mask is policy and the image is the
/// machine's, but both come out of the same bytes and the caller applies both.
#[derive(Clone, Copy, Debug)]
pub struct SignalFrameExit {
    /// The mask the interrupted code had blocked, as the wire's `sigset_t`.
    pub blocked_wire: u64,
    /// The floating-point image the frame carried, when the machine keeps it in
    /// the frame the caller already read back.
    ///
    /// x86_64's `_fpstate` is inside the frame block ([`SignalFrame::enter`] put
    /// it there), so reading it needs no second trip into user memory, and the
    /// answer is always `Some`: the image itself, or the architectural reset
    /// state when the frame's `fpstate` pointer is zero (which is what Linux's
    /// `rt_sigreturn` makes of that frame - measured). aarch64 answers `None`:
    /// its `fpsimd_context` record is inside the frame too, but it belongs in the
    /// trap frame, which `exit` writes directly.
    ///
    /// A frame that names an image *outside* itself is not followed. Linux reads
    /// through `sigcontext.fpstate` wherever it points; every kernel-built frame
    /// points at its own, so the difference is visible only to a caller
    /// hand-building a frame - and `rt_sigreturn` only ever unwraps one this
    /// kernel laid out.
    pub fpu: Option<[u8; FPU_BYTES]>,
}

/// Where a machine put a handler's frame.
#[derive(Clone, Copy, Debug)]
pub struct SignalFrameEntry {
    /// The stack pointer the handler runs on - the frame's own address.
    pub sp: usize,
    /// The `siginfo_t`, the handler's second argument.
    pub siginfo: usize,
    /// The `ucontext`, the handler's third argument.
    pub ucontext: usize,
}

/// The signal frame a handler runs on: the structure the kernel pushes on the
/// process's own stack, which the libc's trampoline hands back to
/// `rt_sigreturn`.
///
/// A capability, not a concern. Its layout - the machine's `sigcontext` field
/// order, the offsets of the frame's three parts, and which saved register each
/// word holds - is the machine's ABI and not a free choice; a port that has not
/// built its signal stage has no frame to lay out and no `rt_sigreturn` to
/// unwrap one, so it declares no `mpos_signal_frame` and the ABI layer delivers
/// no handler there. That is why the signal family in `syscall` is gated on
/// this flag and not on an architecture: an absent capability is absent, not a
/// stub (`NOTES.md` §13.2).
///
/// What is *not* here is policy - which stack, which mask, which restorer - and
/// the two scheduler calls a delivery needs (writing the frame into the
/// process's own memory, installing the handler's mask). `arch -> sched` is the
/// one upward edge this seam exists to keep out, so those stay with the caller.
pub trait SignalFrame {
    /// How many bytes the frame occupies on the handler's stack.
    const FRAME_BYTES: usize;

    /// Where the frame begins, given the stack pointer the handler's
    /// trampoline leaves behind when it calls `rt_sigreturn`.
    ///
    /// This is the one *shape* difference between the two machines' frames
    /// rather than a layout inside one: x86_64's `__restore_rt` reaches the
    /// kernel with the restorer word still at `rsp`, so the frame starts eight
    /// bytes below it; aarch64's restorer is in `x30` and nothing was pushed,
    /// so the frame *is* where `sp` points. The `rt_sigreturn` handler asks
    /// this rather than assuming either.
    fn frame_at(sp: usize) -> usize;

    /// Lay the frame out, and turn the interrupted call's register file into
    /// the handler's entry state.
    ///
    /// On success `out` (at least [`FRAME_BYTES`](Self::FRAME_BYTES) long) holds
    /// the frame bytes and `regs` (`ABI_REG_COUNT` words, the file the
    /// interrupted call was captured in) describes the handler's first
    /// instruction: its `ip`/`sp` are the handler's, and its first three
    /// arguments are the signal number, the frame's `siginfo_t` and its
    /// `ucontext`. The answer names where the frame was put, so the caller can
    /// write it into the process's memory.
    ///
    /// `frame` is the machine's own gate frame for this task - the same opaque
    /// pointer [`Gate`] is handed - and a machine may read from it any
    /// interrupted state it keeps there rather than in the task's record.
    /// aarch64's trap frame carries the interrupted floating-point/SIMD
    /// registers, which is the only place they exist (the record belongs to the
    /// context switch, which writes the kernel's scratch into it), and `enter`
    /// copies them into the frame's `fpsimd_context`. x86_64's trap frame
    /// carries no floating-point image - the lazy-FPU scheme leaves the
    /// registers live and saves them into the task's record - so the image
    /// arrives in [`SignalDelivery::fpu`] instead.
    ///
    /// `None` when there is no frame to write at all - the disposition has no
    /// restorer and this machine's ABI requires one.
    ///
    /// # Safety
    /// As [`Gate::user_ip`]: `frame` must be the gate's own frame for the
    /// calling task, or null.
    unsafe fn enter(
        regs: &mut [u64],
        out: &mut [u8],
        delivery: &SignalDelivery,
        frame: *mut (),
    ) -> Option<SignalFrameEntry>;

    /// The inverse: read a handler's frame back into the ABI register file, and
    /// answer the mask the interrupted code had blocked, as the wire's
    /// `sigset_t` - and the floating-point image, when the machine keeps it in
    /// the frame.
    ///
    /// `bytes` is the frame the caller read back from user memory, whole. The
    /// slots the frame does not carry are left as they are, so `regs` is seeded
    /// from the running task's own file first - the thread pointer lives in a
    /// register the frame does not save.
    ///
    /// `frame` is [`enter`](Self::enter)'s gate frame, and is where a machine
    /// puts state the frame carries but the register file does not - aarch64
    /// restores the floating-point half through it, and the trap's own exit is
    /// what loads the registers.
    ///
    /// # Safety
    /// As [`SignalFrame::enter`].
    unsafe fn exit(bytes: &[u8], regs: &mut [u64], frame: *mut ()) -> SignalFrameExit;
}

/// An interrupt-driven transmit ring behind the [`Console`] sink: queue bytes,
/// sleep while the UART's own interrupt drains the wire, and wake the parked
/// writer when room appears.
///
/// A capability, not part of [`Console`], because it needs a machine that (1) has
/// an interrupting transmitter and (2) has a wired controller and scheduler to
/// park on. Both machines do: x86_64's line reaches a pic, aarch64's its GIC's
/// SPI 33 - and the queue both of them drain is written once above them
/// (`crate::drivers::tx_ring`), so a `SerialRing` impl is a three-line answer to
/// "will you take a byte" rather than a second copy of who sleeps for room.
/// A machine with neither is not one at all, and says so by declaring no
/// `mpos_serial_ring`: the console then always takes the polled path and never
/// parks.
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

/// The syscall gate's trap frame: the register block the entry stub builds, as
/// everything above the machine layer gets hold of it.
///
/// The ABI layer carries this frame as an opaque `*mut ()` and must not know its
/// shape. It is deliberately *not* [`UserFrameOps`]'s frame, and that is why
/// these are free functions over a pointer rather than methods on a type: on
/// x86_64 the gate's frame is the entry stub's saved block on the kernel stack,
/// on aarch64 it is the task's own user frame, and no type both machines share
/// can be named without inventing one - which would be the leak this seam
/// exists to remove.
///
/// What the ABI layer needs is small and machine-neutral in intent: read the
/// instruction the call will return to, and send `execve` somewhere else instead
/// of returning. Anything richer - the whole register file, the signal frame
/// laid over it - is reached through the machine's own code, because *which*
/// register a value lives in is exactly the machine's business.
pub trait Gate {
    /// The user instruction pointer of the call being dispatched: where the gate
    /// will return to.
    ///
    /// # Safety
    /// `frame` must be the gate's own frame for the calling task - the pointer
    /// `syscall::linux_dispatch` was handed.
    unsafe fn user_ip(frame: *mut ()) -> u64;

    /// Send the frame back to userland at `entry` on `sp` - how `execve` makes a
    /// call that never returns.
    ///
    /// Which registers say that is the machine's. The operation is the same one
    /// on both, but a machine whose result slot doubles as something a tracer
    /// reads leaves it for the gate to fill in, while a machine whose frame is
    /// also the task's own record zeroes it here. What a caller may rely on is
    /// the instruction that runs next, the stack it runs on, and that the call
    /// has no return value.
    ///
    /// # Safety
    /// As [`Gate::user_ip`], and `entry`/`sp` must be addresses the task may run
    /// at, which the caller has just loaded an image onto.
    unsafe fn set_exec_return(frame: *mut (), entry: usize, sp: usize);

    /// The ring-3 frame this gate frame describes, with `rax` in the result
    /// slot: the frame a `fork`/`clone` child resumes with, and the one a
    /// tracer reads. `None` when `frame` is null - a call the kernel made on
    /// its own behalf has no registers to name.
    ///
    /// # Safety
    /// As [`Gate::user_ip`].
    unsafe fn user_context(frame: *mut (), rax: usize) -> Option<UserFrame>;

    /// The gate frame read out as the ABI's register file, `result` in the
    /// result slot and `tls` as the thread pointer.
    ///
    /// `result` is what the slot must *show*, which is not one value: at a
    /// syscall exit it is the call's return value, and at an entry it is
    /// whatever [`Gate::syscall_entry_result_slot`] answers for the machine.
    /// The caller decides, because only it knows which moment it is capturing.
    ///
    /// The words are the ones [`UserFrameOps::to_user_regs`] writes, but read
    /// from the frame's own layout rather than through a [`UserFrame`] - which
    /// is why this op exists beside [`Gate::user_context`]: a gate frame also
    /// carries the two slots `syscall` clobbered (`rcx`/`r11`, which the ABI
    /// reports as `rip`/`eflags`) and `orig_rax`, and a `UserFrame` has no
    /// field for either.
    ///
    /// # Safety
    /// As [`Gate::user_ip`], and `out` must be [`ABI_REG_COUNT`] words long.
    ///
    /// [`ABI_REG_COUNT`]: Contexts::ABI_REG_COUNT
    unsafe fn capture_user_regs(frame: *mut (), result: usize, tls: usize, out: &mut [u64]);

    /// The inverse of [`Gate::capture_user_regs`]: write a register file back
    /// into the gate frame, and answer the value the gate should return to
    /// ring 3 - the ABI's `rax`, which is the slot the gate fills in after the
    /// handler returns.
    ///
    /// The thread pointer comes back through `tls` rather than being installed
    /// here. Installing it is the scheduler's, and `arch -> sched` is the one
    /// upward edge this seam exists to keep out; the ABI layer makes the
    /// `sched` call with the value this hands back. The caller seeds `tls` with
    /// the value the task is *running with*, because not every machine's
    /// register file carries one: x86_64's `user_regs_struct` has `fs_base` and
    /// this overwrites the seed, while aarch64's `user_pt_regs` is
    /// `x0..x30`/`sp`/`pc`/`pstate` and `TPIDR_EL0` is not in it - so there the
    /// seed is the answer, and a machine that wrote zero instead would strand
    /// the task's TLS on its next dispatch.
    ///
    /// # Safety
    /// As [`Gate::user_ip`], and `regs` must be [`ABI_REG_COUNT`] words long.
    ///
    /// [`ABI_REG_COUNT`]: Contexts::ABI_REG_COUNT
    unsafe fn restore_user_regs(frame: *mut (), regs: &[u64], tls: &mut usize) -> isize;

    /// The six syscall argument registers as the frame holds them, in the
    /// ABI's argument order (`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9` on x86_64,
    /// `x0`..`x5` on aarch64).
    ///
    /// Read *after* a tracer's stop has had its chance to rewrite the frame, so
    /// the call is made with the arguments the tracer meant rather than the
    /// ones it arrived with. Which register carries which argument is the
    /// machine's - the same reason [`Gate::capture_user_regs`] exists.
    ///
    /// # Safety
    /// As [`Gate::user_ip`].
    unsafe fn syscall_args(frame: *mut ()) -> [usize; 6];

    /// The syscall number the frame names, in the register this machine passes
    /// it in (`orig_rax` on x86_64, `x8` on aarch64).
    ///
    /// Read *after* a stop has had its chance to rewrite the frame, so a tracer
    /// that redirected the call makes the call it meant rather than the one the
    /// task arrived with. Which register carries the number is the machine's,
    /// the same reason [`Gate::syscall_args`] exists.
    ///
    /// # Safety
    /// As [`Gate::user_ip`].
    unsafe fn syscall_number(frame: *mut ()) -> u64;

    /// Write a syscall number into the frame, so the call the gate is about to
    /// make is `nr`.
    ///
    /// The inverse of [`Gate::syscall_number`], and separate from
    /// [`Gate::restore_user_regs`] because the number is not always a word that
    /// op carries: on x86_64 the number lives in `rax`, which
    /// `restore_user_regs` leaves to the gate's own result write, so a tracer's
    /// rewrite of it would otherwise be dropped before the gate read it back.
    ///
    /// # Safety
    /// As [`Gate::user_ip`].
    unsafe fn set_syscall_number(frame: *mut (), nr: u64);

    /// What the ABI's result slot holds at a syscall *entry* stop.
    ///
    /// The one place the two machines' register files disagree about a slot a
    /// tracer reads, and the answer is not a number to write *into* a slot - it
    /// is what the slot already holds. x86_64's `syscall` instruction leaves the
    /// call's *number* in `rax`, which is also the result slot, so a tracer
    /// reading the file at an entry stop finds the number there and this answers
    /// `nr`. aarch64 carries the number in `x8` instead, and its result slot is
    /// `x0` - the call's *first argument*. Answering `nr` there would not fill a
    /// slot with the number, it would replace a live argument, and because the
    /// gate re-reads the arguments out of the frame after the stop, the tracee
    /// would make its call with the number as `arg0`.
    ///
    /// So a machine whose result slot is not the number register answers what
    /// the frame already holds, which is why this is handed the frame rather
    /// than being a constant.
    ///
    /// Measured on the second machine: with the number written into `x0`, the
    /// first traced syscall after `PTRACE_TRACEME` - the test's own `write(2,
    /// ...)` - became a write to descriptor 94, and the `_exit` that followed
    /// became `exit_group(94)`, so the tracee vanished with its own syscall
    /// number as its exit status.
    ///
    /// # Safety
    /// As [`Gate::user_ip`], for a frame that may be null.
    unsafe fn syscall_entry_result_slot(frame: *mut (), nr: usize) -> usize;

    /// Put a call's result in the frame's result slot.
    ///
    /// This is one operation rather than a write by the gate's own assembly
    /// because the slot is not the same thing on both machines. On x86_64 it is
    /// `rax`, a scratch register the ABI gives the call; on aarch64 it is `x0`,
    /// which is *also* the first argument - a handler entered at the end of the
    /// call is handed the signal number there. So the write belongs to the
    /// layer that knows a handler is about to be entered and can do it first,
    /// not to each gate's return sequence: measured, with the gate writing its
    /// own return value afterwards, every aarch64 handler ran with `x0 = 0`
    /// instead of the signal number it was delivered.
    ///
    /// A null frame - a call the kernel made on its own behalf - has no slot,
    /// and this is a no-op.
    ///
    /// # Safety
    /// As [`Gate::user_ip`], for a frame that may be null.
    unsafe fn set_result(frame: *mut (), value: isize);
}

/// A machine this kernel can run on. `Current` names the one being built.
pub trait Platform:
    Clock
    + WallClock
    + Cpu
    + Interrupts
    + Paging
    + Contexts
    + Console
    + Entropy
    + Gate
    + Sized
    + 'static
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

    /// This frame in the Linux `user_regs_struct`/`user_pt_regs` field order,
    /// written into `out` (whose length is [`Contexts::ABI_REG_COUNT`]), with
    /// `tls` as the thread pointer.
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

    /// Words [`UserFrameOps::to_user_regs`] writes: this machine's Linux
    /// register-file field count - 27 on x86_64 (`user_regs_struct`), 34 on
    /// aarch64 (`user_pt_regs`).
    ///
    /// It is the machine's because the structure is. Anything that carries a
    /// register file *by value* - the scheduler's snapshot of a stopped task,
    /// and the ptrace accessors that read and replace it - is sized by this
    /// rather than by either architecture's ABI table, so it needs no
    /// architecture of its own to be right on both. Which slot holds which
    /// register stays the machine's secret: see
    /// [`UserFrameOps::to_user_regs`].
    const ABI_REG_COUNT: usize;

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
// The direct map: the one address-space fact every machine shares.
// ---------------------------------------------------------------------------

/// The loader's direct map: add this to a physical address to reach the byte.
///
/// Deliberately *not* a capability. The two machines do not disagree about it -
/// the loader chose the offset, the loader mapped the whole of physical memory
/// at it, and the boot record carries it - so there is nothing for a port to
/// answer differently. What made this worth a home in `hal` rather than in `mm`
/// is who needs it: the machine layer's own page-table code reaches a table it
/// allocated by adding this to the frame's physical address
/// (`arch/<arch>/paging.rs`, forty-odd call sites), and after `arch` becomes a
/// crate of its own, `crate::mm::phys_to_virt` is a name it no longer has.
/// Reading it here is a downward call, not a hook - there is no indirection.
///
/// Set once, by [`set_direct_map_offset`], from the boot record's own
/// `hhdm_offset` field. Before that call it reads as zero, exactly as
/// `mm::phys_to_virt` read before `mm::init` - the two are the same instant,
/// because `mm::init` is what makes the call.
static DIRECT_MAP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Record the loader's direct-map offset. Called once, from `mm::init`.
#[inline]
pub fn set_direct_map_offset(offset: usize) {
    DIRECT_MAP.store(offset, core::sync::atomic::Ordering::Relaxed);
}

/// The offset [`phys_to_virt`] adds, for a caller that needs it directly.
#[inline]
pub fn direct_map_offset() -> usize {
    DIRECT_MAP.load(core::sync::atomic::Ordering::Relaxed)
}

/// The kernel-virtual address of `phys`, through the loader's direct map.
#[inline]
pub fn phys_to_virt(phys: usize) -> usize {
    phys + direct_map_offset()
}

/// The physical address `virt` is a direct-map view of.
///
/// Named rather than left to the subtraction: the only way to get here with a
/// smaller address is a caller handing back something that never came from the
/// direct map - a physical frame, or a user address - and which one it is says
/// where the mistake is.
#[inline]
pub fn virt_to_phys(virt: usize) -> usize {
    let hhdm = direct_map_offset();
    assert!(
        virt >= hhdm,
        "virt_to_phys({virt:#x}): not an address from the direct map (hhdm {hhdm:#x})"
    );
    virt - hhdm
}

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

/// Whether the machine can wake a wait between two clock ticks.
pub const SUB_TICK_WAKE: bool = <Current as Clock>::SUB_TICK_WAKE;

/// Arm (or cancel) the machine's one-shot wake-up; see [`Clock::arm_wake_ns`].
#[inline]
pub fn arm_wake_ns(deadline_ns: Option<u64>) {
    <Current as Clock>::arm_wake_ns(deadline_ns)
}

/// Nanoseconds in one clock tick - the unit conversion between a wait named in
/// ticks and the deadlines the scheduler now keeps.
#[inline]
pub const fn ns_per_tick() -> u64 {
    1_000_000_000 / <Current as Clock>::TICK_HZ as u64
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

// ---------------------------------------------------------------------------
// Ending the emulator, for a test run whose verdict is its exit status.
// ---------------------------------------------------------------------------

/// The QEMU `isa-debug-exit` protocol: the code the harness reads back.
///
/// The device terminates the emulator with `(value << 1) | 1`, so `0x10` is
/// exit status 33 (the gate's "every test passed") and `0x11` is 35 (a panic) -
/// the two numbers the self-test protocol's `=== SUMMARY` line is read
/// alongside. They are here rather than in the machine's driver because it is
/// the *harness* they are a contract with, not the hardware.
pub const TEST_EXIT_OK: u32 = 0x10;
pub const TEST_EXIT_PANIC: u32 = 0x11;

/// The machine's way of ending the emulator it is running under.
///
/// A capability, gated on `mpos_qemu_debug_exit`: writing to the debug-exit
/// port is a QEMU arrangement, and a machine without it must not pretend - see
/// [`exit_qemu`], which halts instead. Callers name neither the port nor the
/// machine: `reboot(2)`'s stop path and the panic handler both ask "end this
/// run, with this status", and which wire answers is the machine's.
#[cfg(mpos_qemu_debug_exit)]
pub trait TestExit {
    /// Terminate the emulator with `code` as the test status. Never returns on
    /// QEMU; falls through to a halt on a machine whose port nothing answers.
    fn exit_qemu(code: u32) -> !;
}

/// End the emulator with `code` as the test status. See [`TestExit`].
///
/// Without the capability this is a halt: an honest "there is no way out of
/// here" rather than a write to a port nothing is listening on, which is what
/// `reboot(2)`'s stop path gets on the second machine.
#[cfg(mpos_qemu_debug_exit)]
#[inline]
pub fn exit_qemu(code: u32) -> ! {
    <Current as TestExit>::exit_qemu(code)
}

/// A machine whose firmware can end the emulator - the second machine's way
/// out: PSCI `SYSTEM_OFF` turns the machine off, and the emulator that
/// emulates the firmware exits with it. No status travels (a PSCI off has no
/// exit code to carry), which is why this is its own capability beside
/// [`TestExit`] and not a second impl of it.
#[cfg(mpos_psci_off)]
pub trait MachineOff {
    /// End the machine through its firmware. Never returns.
    fn machine_off() -> !;
}

/// End the emulator with `code`. See [`TestExit`]; this machine has none, so
/// it halts - unless its firmware can end it, in which case the status is
/// not the observable it is through a debug-exit port and [`MachineOff`]
/// answers instead.
#[cfg(all(mpos_psci_off, not(mpos_qemu_debug_exit)))]
#[inline]
pub fn exit_qemu(_code: u32) -> ! {
    <Current as MachineOff>::machine_off()
}

/// End the emulator with `code`. See [`TestExit`]; this machine has none, so it
/// halts.
#[cfg(not(any(mpos_qemu_debug_exit, mpos_psci_off)))]
#[inline]
pub fn exit_qemu(_code: u32) -> ! {
    halt()
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

/// This core's stack pointer (see [`Cpu::stack_pointer`]).
#[inline]
pub fn stack_pointer() -> usize {
    <Current as Cpu>::stack_pointer()
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
pub unsafe fn install(info: &mpos_bootinfo::BootInfo, sections: &KernelSections) -> usize {
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

/// Words in the ABI's register file - [`Contexts::ABI_REG_COUNT`], named at the
/// crate level for the same reason as [`FPU_BYTES`]: it is an array length.
pub const ABI_REG_COUNT: usize = <Current as Contexts>::ABI_REG_COUNT;

/// A register file in the ABI's field order: `user_regs_struct` on x86_64,
/// `user_pt_regs` on aarch64.
///
/// The one type the scheduler's snapshot of a stopped task and the syscall
/// layer's ptrace accessors both carry, so neither has to size it from an
/// architecture's own table. *Which* slot holds which register stays the
/// machine's business - see [`UserFrameOps::to_user_regs`].
pub type AbiRegs = [u64; ABI_REG_COUNT];

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

/// Point the CPU's privilege-transition stack at `top`.mpos_fpu_trap
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

/// The instruction the syscall gate will return to. See [`Gate::user_ip`].
///
/// # Safety
/// `frame` must be the gate's own frame for the calling task.
#[inline]
pub unsafe fn user_ip(frame: *mut ()) -> u64 {
    unsafe { <Current as Gate>::user_ip(frame) }
}

/// Point the gate's frame at a new program, as `execve` does.
///
/// # Safety
/// As [`Gate::set_exec_return`].
#[inline]
pub unsafe fn set_exec_return(frame: *mut (), entry: usize, sp: usize) {
    unsafe { <Current as Gate>::set_exec_return(frame, entry, sp) }
}

/// The ring-3 frame the gate frame describes. See [`Gate::user_context`].
///
/// # Safety
/// As [`Gate::user_context`].
#[inline]
pub unsafe fn user_context(frame: *mut (), rax: usize) -> Option<UserFrame> {
    unsafe { <Current as Gate>::user_context(frame, rax) }
}

/// The gate frame as the ABI's register file. See
/// [`Gate::capture_user_regs`].
///
/// # Safety
/// As [`Gate::capture_user_regs`].
#[inline]
pub unsafe fn capture_user_regs(frame: *mut (), rax: usize, tls: usize, out: &mut [u64]) {
    unsafe { <Current as Gate>::capture_user_regs(frame, rax, tls, out) }
}

/// Write the ABI's register file back into the gate frame. See
/// [`Gate::restore_user_regs`].
///
/// # Safety
/// As [`Gate::restore_user_regs`].
#[inline]
pub unsafe fn restore_user_regs(frame: *mut (), regs: &[u64], tls: &mut usize) -> isize {
    unsafe { <Current as Gate>::restore_user_regs(frame, regs, tls) }
}

/// The six syscall argument registers as the frame holds them. See
/// [`Gate::syscall_args`].
///
/// # Safety
/// As [`Gate::syscall_args`].
#[inline]
pub unsafe fn syscall_args(frame: *mut ()) -> [usize; 6] {
    unsafe { <Current as Gate>::syscall_args(frame) }
}

/// The syscall number the frame names. See [`Gate::syscall_number`] - read it
/// after a stop, so a tracer's redirect is honoured.
///
/// # Safety
/// As [`Gate::user_ip`].
#[inline]
pub unsafe fn syscall_number(frame: *mut ()) -> u64 {
    unsafe { <Current as Gate>::syscall_number(frame) }
}

/// Write a syscall number into the frame. See [`Gate::set_syscall_number`].
///
/// # Safety
/// As [`Gate::user_ip`].
#[inline]
pub unsafe fn set_syscall_number(frame: *mut (), nr: u64) {
    unsafe { <Current as Gate>::set_syscall_number(frame, nr) }
}

/// What the ABI's result slot holds at a syscall entry stop. See
/// [`Gate::syscall_entry_result_slot`] - the answer is what the slot already
/// holds on a machine whose number register is somewhere else.
///
/// # Safety
/// As [`Gate::user_ip`], for a frame that may be null.
#[inline]
pub unsafe fn syscall_entry_result_slot(frame: *mut (), nr: usize) -> usize {
    unsafe { <Current as Gate>::syscall_entry_result_slot(frame, nr) }
}

/// Put a call's result in the frame's result slot. See [`Gate::set_result`].
///
/// # Safety
/// As [`Gate::set_result`].
#[inline]
pub unsafe fn set_result(frame: *mut (), value: isize) {
    unsafe { <Current as Gate>::set_result(frame, value) }
}

/// The bytes a signal frame occupies ([`SignalFrame::FRAME_BYTES`]).
#[cfg(mpos_signal_frame)]
pub const SIGNAL_FRAME_BYTES: usize = <Current as SignalFrame>::FRAME_BYTES;

/// Where a handler's frame begins, given the trampoline's `rt_sigreturn` stack
/// pointer. See [`SignalFrame::frame_at`].
#[cfg(mpos_signal_frame)]
#[inline]
pub fn signal_frame_at(sp: usize) -> usize {
    <Current as SignalFrame>::frame_at(sp)
}

/// Lay out the frame a signal handler will run on. See [`SignalFrame::enter`].
///
/// # Safety
/// As [`SignalFrame::enter`].
#[cfg(mpos_signal_frame)]
#[inline]
pub unsafe fn enter_signal_frame(
    regs: &mut [u64],
    out: &mut [u8],
    delivery: &SignalDelivery,
    frame: *mut (),
) -> Option<SignalFrameEntry> {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { <Current as SignalFrame>::enter(regs, out, delivery, frame) }
}

/// Read a handler's frame back. See [`SignalFrame::exit`].
///
/// # Safety
/// As [`SignalFrame::exit`].
#[cfg(mpos_signal_frame)]
#[inline]
pub unsafe fn exit_signal_frame(bytes: &[u8], regs: &mut [u64], frame: *mut ()) -> SignalFrameExit {
    // SAFETY: the caller's contract, passed through unchanged.
    unsafe { <Current as SignalFrame>::exit(bytes, regs, frame) }
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
