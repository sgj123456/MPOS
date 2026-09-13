//! Global Descriptor Table.
//!
//! Limine already installs a GDT good enough for ring-0 64-bit code, but we
//! install our own immediately so that descriptor layout is under our control:
//! the user-mode segments and the TSS sit here beside the kernel ones, and
//! nothing had to reshuffle selectors to make room.
//!
//! The TSS is a single static rather than per-CPU state because there is one
//! core, and it exists for exactly one field: `RSP0`, the stack the CPU loads
//! on every transition from ring 3 - a syscall or an interrupt. The scheduler
//! rewrites it on every dispatch, so a user task always traps onto its own
//! kernel stack; tasks that never leave ring 0 never cause the CPU to read it.

use core::cell::UnsafeCell;

use spin::Once;

use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    pub tss: SegmentSelector,
}

struct Gdt {
    table: GlobalDescriptorTable,
    selectors: Selectors,
}

static GDT: Once<Gdt> = Once::new();

/// The TSS, mutable through the cell because [`set_kernel_stack`] rewrites
/// `RSP0` on every dispatch. Not a `static mut`: a cell makes the mutation
/// explicit and keeps the compiler's static-mut rules out of the way.
struct TssCell(UnsafeCell<TaskStateSegment>);

// Safety: one core. Every write happens inside the scheduler with interrupts
// off, and the hardware's only read - on a ring-3 transition - happens while
// user code is running, which is never at the same time as this code.
unsafe impl Sync for TssCell {}

static TSS: TssCell = TssCell(UnsafeCell::new(TaskStateSegment::new()));

/// The two words the `syscall` entry needs before it has anything else: a
/// kernel stack to push on, and somewhere to put a register while it gets
/// there. Both are reached through `IA32_KERNEL_GS_BASE`, which a `swapgs`
/// swaps for the user's GS base - so this is what the entry stub sees as
/// `gs:[0]` and `gs:[8]`, at exactly those offsets (`repr(C)`, and the entry
/// stub spells them as numbers).
#[repr(C)]
struct EntryScratch {
    /// Top of the current task's kernel stack, mirrored from `RSP0` so the
    /// `syscall` fast path - which the CPU does *not* switch onto a kernel
    /// stack the way an interrupt does - can find it without touching the TSS.
    /// `set_kernel_stack` keeps it in lock-step with `RSP0` on every dispatch.
    kernel_rsp: UnsafeCell<usize>,
    /// The entry stub's parking slot. `syscall` switches no stack, so the
    /// caller's RSP has to outlive the load that moves the CPU onto the kernel
    /// one - and it has to do so *without a register*: every general-purpose
    /// register at that point holds a caller value the syscall ABI promises to
    /// return unchanged, `r12` included. Borrowing one of them corrupts user
    /// code that kept a live value there across the call.
    entry_rsp: UnsafeCell<usize>,
}

// Safety: one core - the same assumption the TSS above rests on. Every write
// to `kernel_rsp` happens inside the scheduler with interrupts off; the entry
// stub's own write to `entry_rsp` and its read back are one uninterrupted
// stretch of the same core's ring 0, because `syscall` enters with IF clear.
unsafe impl Sync for EntryScratch {}

static ENTRY_SCRATCH: EntryScratch = EntryScratch {
    kernel_rsp: UnsafeCell::new(0),
    entry_rsp: UnsafeCell::new(0),
};

/// The address `IA32_KERNEL_GS_BASE` points at: what a `swapgs` in the
/// `syscall` entry swaps the user's GS base for.
pub fn entry_scratch_addr() -> u64 {
    &ENTRY_SCRATCH as *const EntryScratch as u64
}

pub fn init() {
    let gdt = GDT.call_once(|| {
        let mut table = GlobalDescriptorTable::new();
        let kernel_code = table.append(Descriptor::kernel_code_segment());
        let kernel_data = table.append(Descriptor::kernel_data_segment());
        // User *data* first, user code second, and never the other way round:
        // `sysret` derives both selectors from a single STAR field (see
        // `user_code_star_base`), which only works when the code descriptor
        // is the slot immediately above the data one. `iretq`, which the
        // user-entry path uses, does not care about the order.
        let user_data = table.append(Descriptor::user_data_segment());
        let user_code = table.append(Descriptor::user_code_segment());
        // A TSS descriptor carries an address, so it needs a `&'static`; the
        // cell hands one out that lives as long as the static does.
        let tss: &'static TaskStateSegment = unsafe { &*TSS.0.get() };
        let tss = table.append(Descriptor::tss_segment(tss));
        Gdt {
            table,
            selectors: Selectors {
                kernel_code,
                kernel_data,
                user_code,
                user_data,
                tss,
            },
        }
    });

    unsafe {
        gdt.table.load();
        load_tss(gdt.selectors.tss);
        CS::set_reg(gdt.selectors.kernel_code);
        DS::set_reg(gdt.selectors.kernel_data);
        ES::set_reg(gdt.selectors.kernel_data);
        SS::set_reg(gdt.selectors.kernel_data);
    }
}

/// Point `RSP0` at `top`, so the next trap from ring 3 lands on the owning
/// task's kernel stack. The scheduler calls this on every dispatch.
pub fn set_kernel_stack(top: usize) {
    unsafe {
        let tss = &mut *TSS.0.get();
        tss.privilege_stack_table[0] = VirtAddr::new(top as u64);
        // Keep the `syscall` fast path's view of the kernel stack in step with
        // the TSS; `swapgs` in the entry exposes this to kernel code.
        *ENTRY_SCRATCH.kernel_rsp.get() = top;
    }
}

/// The ring-0 code and data selectors, in the order `syscall` needs them:
/// `STAR[47:32]` gets the code one and the CPU takes the data one from
/// `+ 8` on its own. Reading them rather than spelling `0x08`/`0x10` keeps
/// the entry stub's idea of the kernel segments tied to this table.
pub fn kernel_selectors() -> (SegmentSelector, SegmentSelector) {
    let gdt = GDT.get().expect("gdt::init() has not run");
    (gdt.selectors.kernel_code, gdt.selectors.kernel_data)
}

/// The ring-3 code and data selectors, for `iretq` into user mode. `append`
/// records each descriptor's privilege level in the selector it returns, so
/// both arrive with RPL 3 already set.
///
/// The pair is ordered *data first* in the GDT, and that is load-bearing:
/// `sysret` derives the user CS and SS from one STAR field as `+ 16` and
/// `+ 8`, so the code descriptor has to sit exactly one slot above the data
/// one. [`user_code_star_base`] is that field.
pub fn user_selectors() -> (SegmentSelector, SegmentSelector) {
    let gdt = GDT.get().expect("gdt::init() has not run");
    (gdt.selectors.user_code, gdt.selectors.user_data)
}

/// What `IA32_STAR[63:48]` must hold for `sysretq` to land on our user
/// segments.
///
/// `sysret` does not take a selector pair: it computes
/// `CS = STAR[63:48] + 16` and `SS = STAR[63:48] + 8`, forcing RPL 3 on both.
/// So STAR holds neither selector - it holds the slot *below* the user data
/// descriptor, from which the CPU reaches both by adding. With the GDT laid
/// out user-data-then-user-code, that is the data selector's index minus 8.
///
/// The descriptors are queried rather than hardcoded, and the pairing is
/// checked, because getting this wrong does not fail at load time: it fails
/// as a `#GP` on the first `sysretq`, at whatever selector STAR happened to
/// name - the TSS, most likely.
pub fn user_code_star_base() -> u64 {
    let (code, data) = user_selectors();
    let data_base = (data.0 as u64) & !0b11;
    // The code descriptor is one GDT slot (8 bytes) above the data one, so
    // `data_base + 16` is what `sysret` will compute for CS from `data_base -
    // 8`, and `data_base + 8` is what it will compute for SS.
    debug_assert_eq!(
        (code.0 as u64) & !0b11,
        data_base + 8,
        "sysret needs the user code descriptor one slot above the user data one"
    );
    debug_assert_eq!(
        code.0 & 0b11,
        0b11,
        "sysret forces RPL 3, so the user CS must be a ring-3 descriptor"
    );
    data_base - 8
}
