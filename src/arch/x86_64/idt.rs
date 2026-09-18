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
    let cow = user_address
        && code.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
        && crate::sched::resolve_cow_fault(addr);
    if cow {
        return; // The faulting instruction retries and succeeds.
    }

    // A push past the end of what the loader mapped is an ordinary thing for a
    // program to do, and Linux answers it by extending the stack. The bounds
    // that make that safe - how far below `rsp`, and how far down in total -
    // live with the stack itself; here all that matters is that the fault is
    // at a user address and the process was running when it happened.
    if user_address
        && crate::sched::resolve_stack_growth(addr, frame.stack_pointer.as_u64() as usize)
    {
        return; // The page is there now; the instruction retries.
    }

    // Anything else at a user address is the process's own fault to bear. The
    // `rip` test keeps this from claiming a kernel fault that merely named a
    // user address: a ring-0 dereference of a bad user pointer is a kernel bug,
    // not a signal to deliver.
    if rip < (1 << 47) {
        crate::println!("user #PF at {:#x} code={:?} rip={:#x}", addr, code, rip);
        // Still extends the stack when the fault is a push past the mapped
        // end - the same call, moved here so its answer can be printed with
        // the rest of what the page tables say.
        if crate::sched::resolve_stack_growth(addr, frame.stack_pointer.as_u64() as usize) {
            return; // The page is there now; the instruction retries.
        }
        crate::sched::send_signal(crate::sched::current(), 11);
        crate::sched::preempt();
        return;
    }
    crate::println!(
        "\nKERNEL PANIC: #PF at {:#x} ({:?}) rip {:#x} cs {:#x} rsp {:#x}",
        addr,
        code,
        frame.instruction_pointer,
        frame.code_segment.0,
        frame.stack_pointer
    );
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

/// The last-resort report for a fault nobody can resolve.
///
/// The `x86-interrupt` ABI hands the handler a pointer to an
/// `InterruptStackFrame`, and where that pointer lands is worth writing down
/// once, because getting it wrong makes every dump read as garbage:
///
///   +0x00  instruction_pointer   <- the faulting RIP
///   +0x08  code_segment          <- CS as the CPU had it
///   +0x10  cpu_flags             <- RFLAGS
///   +0x18  stack_pointer         <- only on a privilege change
///   +0x20  stack_segment         <- likewise
///
/// The pointer itself is `rsp_hw + 8`, where `rsp_hw` is the first word the
/// CPU pushed. That is the error code for a vector that has one, and the RIP
/// for one that does not - which is exactly why the offset comes out the same
/// for both: the compiler emits one extra push (and a wider `sub`) for a
/// handler carrying an error code, and consumes them in its epilogue with the
/// matching `add $0x10`. Verified against the disassembly of both
/// `page_fault` (10 pushes, `sub $0x1d8`, epilogue `add $0x1d8` + 9 pops +
/// `add $0x10`) and `breakpoint` (9 pushes, `sub $0x30`, epilogue `add $0x30`
/// + 9 pops, no extra skip).
///
/// So the words around the struct read, for *any* vector, as:
///
///   word(-1)  error code
///   word(0)   RIP      (the struct's own field, so this always agrees)
///   word(1)   CS
///   word(2)   RFLAGS
///   word(3)   RSP      - only meaningful on a privilege change
///   word(4)   SS       - likewise
///
/// `word(3)`/`word(4)` are the trap's own idea of the interrupted stack and
/// segment, and for a fault taken in ring 0 the CPU never wrote them: they
/// hold whatever the interrupted code left on its stack. They are printed, but
/// for a ring-0 fault they are not a stack pointer and not a segment.
fn fatal(name: &str, frame: &InterruptStackFrame, code: u64) -> ! {
    crate::println!("\nKERNEL PANIC: {}", name);
    // A non-zero error code on a `#GP` *is* a selector, and a selector is only
    // worth printing if it is named: index in the GDT, and both privilege bits.
    if code != 0 {
        crate::println!(
            "  code {:#x}: gdt index {} ti {} rpl {}",
            code,
            code >> 3,
            (code >> 2) & 1,
            code & 3
        );
    }
    let base = frame as *const InterruptStackFrame as *const u8 as *const u64;
    let word = |index: isize| -> u64 { unsafe { core::ptr::read_unaligned(base.offset(index)) } };
    // The error code lives at the word below the struct. RIP/CS/RFLAGS come
    // from the struct's own fields rather than from `word(0..3)`: they are the
    // same memory, so printing the fields proves the pointer arithmetic - if
    // the two ever disagreed, one of them would be a bug worth seeing.
    crate::println!(
        "  err {:#x} rip {:#x} cs {:#x} rflags {:#x}",
        word(-1),
        frame.instruction_pointer,
        frame.code_segment.0,
        frame.cpu_flags
    );
    // The two privilege-change slots sit at `word(3)`/`word(4)`, and only a
    // trap taken at CPL 3 has them written by the CPU. A fault taken in ring 0
    // pushes four words, not six, so those two slots hold whatever the
    // interrupted kernel code left on its stack - a `ss 0x0` in this dump is
    // that, not a stack segment. Printing them regardless is how a reader ends
    // up chasing a value the hardware never wrote, so they are labelled for
    // what they are and only claimed as real when `cs.rpl` says a privilege
    // change happened.
    if frame.code_segment.0 & 3 == 3 {
        crate::println!("  rsp {:#x} ss {:#x}", word(3), word(4));
    } else {
        crate::println!(
            "  (ring 0: no privilege change, so word3/word4 are stack litter) w3 {:#x} w4 {:#x}",
            word(3),
            word(4)
        );
    }
    // The faulting RIP's role, so the two cases a `#GP` usually is - a bad
    // ring-3 return and a bad `sysret` - can be told apart at a glance.
    let rip = frame.instruction_pointer.as_u64() as usize;
    crate::println!(
        "  rip is {} ({}), cs.rpl = {}",
        if rip < (1 << 47) { "user" } else { "kernel" },
        if rip & 0xfff == 0 {
            "page-aligned"
        } else {
            "not page-aligned"
        },
        frame.code_segment.0 & 3
    );
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

/// If the faulting rip is in the lower half (user mode), deliver SIGSEGV and
/// return `true`.  Otherwise return `false` so the caller can panic.
///
/// The one printed line matters more than it looks: a `#GP` in ring 3 means
/// user code attempted something the CPU refuses - loading a selector whose
/// RPL is below CPL, most commonly - and the only evidence that it happened
/// is this line. Silently converting it to a signal leaves the fault's
/// selector, which is the diagnostic, on the floor.
fn user_fault_or_die(frame: &InterruptStackFrame) -> bool {
    let rip = frame.instruction_pointer.as_u64() as usize;
    if rip < (1 << 47) {
        crate::println!(
            "user #GP/#SS/#NP at rip {:#x} cs {:#x}",
            rip,
            frame.code_segment.0
        );
        crate::sched::send_signal(crate::sched::current(), 11); // SIGSEGV
        crate::sched::preempt();
        return true;
    }
    false
}
