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

pub fn init() {
    let gdt = GDT.call_once(|| {
        let mut table = GlobalDescriptorTable::new();
        let kernel_code = table.append(Descriptor::kernel_code_segment());
        let kernel_data = table.append(Descriptor::kernel_data_segment());
        let user_code = table.append(Descriptor::user_code_segment());
        let user_data = table.append(Descriptor::user_data_segment());
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
    }
}

/// The ring-3 code and data selectors, for `iretq` into user mode. `append`
/// records each descriptor's privilege level in the selector it returns, so
/// both arrive with RPL 3 already set.
pub fn user_selectors() -> (SegmentSelector, SegmentSelector) {
    let gdt = GDT.get().expect("gdt::init() has not run");
    (gdt.selectors.user_code, gdt.selectors.user_data)
}
