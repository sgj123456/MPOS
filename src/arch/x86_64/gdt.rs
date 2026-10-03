//! Global Descriptor Table.
//!
//! The firmware leaves a GDT good enough for ring-0 64-bit code, but we install
//! our own immediately so that descriptor layout is under our control:
//! the user-mode segments and the TSS sit here beside the kernel ones, and
//! nothing had to reshuffle selectors to make room.
//!
//! The TSS is a single static rather than per-CPU state because there is one
//! core, and it exists for two fields. `RSP0` is the stack the CPU loads on every
//! transition from ring 3 - a syscall or an interrupt - and the scheduler rewrites
//! it on every dispatch, so a user task always traps onto its own kernel stack;
//! tasks that never leave ring 0 never cause the CPU to read it.
//! `interrupt_stack_table` is the array of *last-resort* stacks the CPU switches
//! to for the few vectors that name one (`IST`), and it is what lets a fault be
//! reported when the stack it landed on is the thing that is broken - see
//! [`set_ist`] and `super::idt`'s double-fault stack.

use core::cell::UnsafeCell;

use spin::Once;

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

/// Where `RSP0` sits inside that TSS, as a byte count from the static's own
/// address. Not `offset_of!`-style field arithmetic: `TaskStateSegment` is
/// `repr(C, packed(4))`, and a `packed` struct's fields cannot be referenced at
/// all when they land on an odd byte, so the one field the CPU reads on every
/// ring-3 transition has to be reached by address instead.
///
/// Four from the front: `reserved_1` is a `u32` and `RSP0` is the member right
/// after it. Nothing else in the struct comes before them.
const RSP0_OFFSET: usize = 4;

/// Where `interrupt_stack_table` sits in that same packed struct, in bytes from
/// its own address: `reserved_1` (4) + `privilege_stack_table` (3 x 8 = 24) +
/// `reserved_2` (8). Seven slots, `slot 0` being the hardware `IST1`.
///
/// The 8 is measured rather than assumed, and the difference matters: written
/// as 4 this constant put the array at 32, which is four bytes short of where
/// `x86_64`'s `TaskStateSegment` actually has it (`reserved_2` is a `u64`, not
/// a `u32`). A debug build caught that with the assertion in [`set_ist`] - it
/// panicked at boot - while a release build, where the assertion is compiled
/// out, quietly wrote the `#DF` stack over the last half of `reserved_2` and
/// the first half of `IST1`, which is a last-resort stack the CPU would then
/// load from a word that was never filled in.
///
/// Same reason as [`RSP0_OFFSET`] for spelling it as an address instead of a
/// field: the struct is `repr(C, packed(4))`, so a reference into it is not
/// something the compiler will hand out.
const IST_OFFSET: usize = 36;

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
        debug_assert_eq!(
            core::ptr::addr_of!((*TSS.0.get()).privilege_stack_table) as usize
                - TSS.0.get() as usize,
            RSP0_OFFSET,
            "the RSP0 byte offset in the packed TSS moved"
        );
        // `write_unaligned`, not `write_volatile`: the TSS is `repr(C,
        // packed(4))` and this field sits at offset 4 inside it, so the address
        // is only 4-byte aligned and a `*mut u64` there is not a valid
        // `write_volatile` target - the precondition fires under the debug
        // assertion the build enables. The CPU reads this field with an
        // ordinary unaligned load, so an unaligned store is the matching
        // operation. The write is `volatile` in effect rather than by name: it
        // goes through a raw pointer derived from an `UnsafeCell`, and nothing
        // here reads the field back through a reference that the compiler could
        // cache.
        let slot = (TSS.0.get() as *mut u8).add(RSP0_OFFSET) as *mut u64;
        core::ptr::write_unaligned(slot, top as u64);
        // Keep the `syscall` fast path's view of the kernel stack in step with
        // the TSS; `swapgs` in the entry exposes this to kernel code.
        *ENTRY_SCRATCH.kernel_rsp.get() = top;
    }
}

/// Point hardware `IST<index + 1>` at `top`, so a vector installed with that
/// index runs on a stack the CPU loads regardless of what the faulting stack
/// pointer holds.
///
/// The CPU reads this array when it takes a fault whose IDT entry names an IST
/// slot - double fault, NMI, machine check - and it reads it *before* pushing
/// anything, which is the whole reason it exists: a fault taken while the kernel
/// stack is unusable (overflowed off its bottom, or walked over by a wild write)
/// cannot push the frame that would report it. Without an IST stack the failed
/// push becomes a double fault, the double fault cannot push its own frame either,
/// and the third fault is a triple fault: the CPU resets, `cargo xtask run` passes
/// `-no-reboot`, and QEMU exits without writing a single byte of explanation. This
/// is the difference between a machine that dies and a machine that says how.
///
/// `index` is the software one (0 = `IST1`), the same numbering
/// `EntryOptions::set_stack_index` uses.
///
/// ## Safety
///
/// The caller is giving the CPU an address to load `RSP` from on a fault, so it
/// has to be the *top* of a region that is mapped for as long as the machine is
/// up, 16-byte aligned (the frame the CPU pushes assumes a stack aligned the way a
/// `call` would leave it), and not shared with any other vector that can nest.
pub unsafe fn set_ist(index: usize, top: usize) {
    unsafe {
        debug_assert_eq!(
            core::ptr::addr_of!((*TSS.0.get()).interrupt_stack_table) as usize
                - TSS.0.get() as usize,
            IST_OFFSET,
            "the IST array's byte offset in the packed TSS moved"
        );
        debug_assert!(index < 7, "the TSS has seven IST slots");
        debug_assert_eq!(top % 16, 0, "an IST stack top must be 16-byte aligned");
        let slot = (TSS.0.get() as *mut u8).add(IST_OFFSET + index * 8) as *mut u64;
        // `write_unaligned` for the same reason `set_kernel_stack` uses it: this
        // field is only 4-byte aligned inside the packed struct, and the CPU reads
        // it with an ordinary unaligned load.
        core::ptr::write_unaligned(slot, top as u64);
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
/// segments - with the RPL bits pre-applied, because not every CPU applies
/// them itself.
///
/// `sysret` does not take a selector pair: it computes
/// `CS = STAR[63:48] + 16` and `SS = STAR[63:48] + 8`, and the SDM says it
/// forces RPL 3 on both. It does not, on every CPU: this machine boots under
/// KVM, the instruction runs on the host silicon, and the host loads SS as
/// `STAR[63:48] + 8` *without* the OR 3 - the class of behaviour Xen documented
/// as XSA-132. The result was user code running with `SS = 0x18` (RPL 0), which
/// nothing faults on until the next trap out of ring 3 pushes that SS into a
/// frame and `iretq` refuses to pop it: `#GP(0x18)`, kernel mode, at the
/// `iretq`. So the base carries the RPL bits itself: `STAR[63:48] = 0x13`
/// makes `+8` yield `0x1b` and `+16` yield `0x23` outright, and the SDM's
/// `OR 3` - still executed by the CPUs that do honor it - becomes a no-op
/// because both values already end in binary `11`. Adding 8 or 16 cannot
/// carry into the low three bits, so the OR survives the arithmetic on both
/// kinds of CPU.
///
/// The descriptors are queried rather than hardcoded, and the pairing is
/// checked, because getting this wrong does not fail at load time: it fails
/// as a `#GP` on the first `sysretq`, at whatever selector STAR happened to
/// name - the TSS, most likely.
pub fn user_code_star_base() -> u64 {
    let (code, data) = user_selectors();
    let data_base = (data.0 as u64) & !0b11;
    // The code descriptor is one GDT slot (8 bytes) above the data one, and
    // both selectors carry RPL 3. `data_base + 11` is `user_code` outright and
    // `data_base + 3` is `user_data` outright, which is exactly what `+ 16`
    // and `+ 8` from the returned base compute.
    debug_assert_eq!(
        (code.0 as u64) & !0b11,
        data_base + 8,
        "sysret needs the user code descriptor one slot above the user data one"
    );
    debug_assert_eq!(
        code.0 & 0b11,
        0b11,
        "the STAR base pre-applies RPL 3, so the user CS must be a ring-3 descriptor"
    );
    debug_assert_eq!(
        data.0 & 0b11,
        0b11,
        "the STAR base pre-applies RPL 3, so the user SS must be a ring-3 descriptor"
    );
    (data_base - 8) | 0b11
}
