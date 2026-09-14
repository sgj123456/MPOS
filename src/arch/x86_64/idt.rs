//! Interrupt Descriptor Table and dispatch.
//!
//! Handlers use the `x86-interrupt` ABI (nightly-only), which lets the compiler
//! emit the register save/restore and `iretq` itself. That composes with
//! preemption: the clock handler can switch tasks without ever returning,
//! because the trap frame stays on the interrupted task's own stack and unwinds
//! normally whenever that task is resumed. Hand-written stubs would only buy
//! control over the exact frame layout, which nothing here needs.

use core::sync::atomic::{AtomicU64, Ordering};

use spin::Once;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::pic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

static BREAKPOINTS_CAUGHT: AtomicU64 = AtomicU64::new(0);

pub fn breakpoints_caught() -> u64 {
    BREAKPOINTS_CAUGHT.load(Ordering::Relaxed)
}

pub fn init() {
    let idt = IDT.call_once(|| {
        let mut table = InterruptDescriptorTable::new();

        table.divide_error.set_handler_fn(divide_error);
        table.breakpoint.set_handler_fn(breakpoint);
        table.invalid_opcode.set_handler_fn(invalid_opcode);
        table.double_fault.set_handler_fn(double_fault);
        table
            .general_protection_fault
            .set_handler_fn(general_protection);
        table.page_fault.set_handler_fn(page_fault);
        table.stack_segment_fault.set_handler_fn(stack_segment);
        table
            .segment_not_present
            .set_handler_fn(segment_not_present);
        table.alignment_check.set_handler_fn(alignment_check);

        table[pic::IRQ0_VECTOR].set_handler_fn(timer);
        // IRQ4 is COM1 on every PC that has one: the console's input arrives
        // here, a byte at a time, once `serial::enable_rx` has asked for it.
        table[pic::IRQ0_VECTOR + 4].set_handler_fn(serial);

        table
    });

    idt.load();
}

pub fn enable() {
    x86_64::instructions::interrupts::enable();
}

extern "x86-interrupt" fn timer(_frame: InterruptStackFrame) {
    super::timer::tick();
    // EOI before preempting: once the scheduler switches away this handler does
    // not return until the task is resumed, and an unacknowledged PIC would
    // block every interrupt until then.
    pic::end_of_interrupt(0);
    crate::sched::preempt();
}

/// The console's input. Every byte the UART has goes through the line
/// discipline, which echoes it and wakes whoever is reading fd 0.
///
/// Nothing here may switch tasks. It does not have to: `receive_byte` edits
/// the line and calls `sched::wake`, which only moves a task between queues,
/// and the task it wakes runs when this handler returns and the scheduler
/// next gets a turn - not here, on the interrupt's own stack.
extern "x86-interrupt" fn serial(_frame: InterruptStackFrame) {
    crate::drivers::serial::drain_into_tty();
    // After the FIFO is empty, so a byte arriving during the drain is not
    // lost behind an EOI that already went out.
    pic::end_of_interrupt(4);
}

/// Deliberately reachable from tests: `int3` must return here, not panic.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    BREAKPOINTS_CAUGHT.fetch_add(1, Ordering::Relaxed);
    crate::println!("caught #BP at rip {:#x}", frame.instruction_pointer);
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#DE divide error", &frame, 0)
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#UD invalid opcode", &frame, 0)
}

extern "x86-interrupt" fn stack_segment(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#SS stack segment", &frame, code)
}

extern "x86-interrupt" fn segment_not_present(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#NP segment not present", &frame, code)
}

extern "x86-interrupt" fn alignment_check(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#AC alignment check", &frame, code)
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame) {
        return;
    }
    fatal("#GP general protection", &frame, code)
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    fatal("#DF double fault", &frame, code)
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: PageFaultErrorCode) {
    let addr = Cr2::read().map_or(0, |a| a.as_u64()) as usize;
    let rip = frame.instruction_pointer.as_u64() as usize;
    // The top of the canonical lower half: everything a process may map.
    let user_address = addr < (1 << 47);

    // A write to a copy-on-write page is resolved whoever made it. Usually that
    // is ring 3 writing its own variable after a `fork`, but it is just as often
    // ring 0: every syscall that fills a user buffer - `uname`, `getrandom`,
    // `clock_gettime`, the `CLONE_CHILD_SETTID` write - lands on the same shared
    // page, and the page must split for the kernel's write exactly as it would
    // for the process's own. Refusing it here would take the machine down over
    // an operation Linux performs without a second thought.
    if user_address
        && code.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
        && crate::sched::resolve_cow_fault(addr)
    {
        return; // The faulting instruction retries and succeeds.
    }

    // Anything else at a user address is the process's own fault to bear. The
    // `rip` test keeps this from claiming a kernel fault that merely named a
    // user address: a ring-0 dereference of a bad user pointer is a kernel bug,
    // not a signal to deliver.
    if rip < (1 << 47) {
        crate::println!("user #PF at {:#x} code={:?} rip={:#x}", addr, code, rip);
        crate::sched::send_signal(crate::sched::current(), 11);
        crate::sched::preempt();
        return;
    }
    crate::println!(
        "\nKERNEL PANIC: #PF at {:#x} ({:?}) rip {:#x}",
        addr,
        code,
        frame.instruction_pointer
    );
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

fn fatal(name: &str, frame: &InterruptStackFrame, code: u64) -> ! {
    crate::println!(
        "\nKERNEL PANIC: {} code {:#x} rip {:#x} rsp {:#x}",
        name,
        code,
        frame.instruction_pointer,
        frame.stack_pointer
    );
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

/// If the faulting rip is in the lower half (user mode), deliver SIGSEGV and
/// return `true`.  Otherwise return `false` so the caller can panic.
fn user_fault_or_die(frame: &InterruptStackFrame) -> bool {
    let rip = frame.instruction_pointer.as_u64() as usize;
    if rip < (1 << 47) {
        crate::sched::send_signal(crate::sched::current(), 11); // SIGSEGV
        crate::sched::preempt();
        return true;
    }
    false
}
