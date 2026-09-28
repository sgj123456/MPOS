//! Interrupt Descriptor Table and dispatch.
//!
//! Handlers use the `x86-interrupt` ABI (nightly-only), which lets the compiler
//! emit the register save/restore and `iretq` itself. That composes with
//! preemption: the clock handler can switch tasks without ever returning,
//! because the trap frame stays on the interrupted task's own stack and unwinds
//! normally whenever that task is resumed. Hand-written stubs would only buy
//! control over the exact frame layout, which nothing here needs.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Once;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::pic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

/// The vector the virtio-net adapter's MSI-X entry points at: clear of the
/// PIC's 32..47 and of every architecturally reserved slot. The driver arms
/// the device to raise it when a frame lands; until it does, the handler
/// below is registered but never runs.
pub const NET_IRQ_VECTOR: u8 = 0x41;

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
        // Vector 7: raised by the first floating-point instruction a task runs
        // after a switch set `CR0.TS`, which is how the deferred half of the
        // FPU switch is delivered. Not an error path - it is the mechanism.
        table
            .device_not_available
            .set_handler_fn(device_not_available);

        table[pic::IRQ0_VECTOR].set_handler_fn(timer);
        // IRQ4 is COM1 on every PC that has one: the console's input arrives
        // here, a byte at a time, once `serial::enable_rx` has asked for it.
        // It is the machine's only input line - the console multiplexer is
        // what turns it into the six virtual consoles.
        table[pic::IRQ0_VECTOR + 4].set_handler_fn(serial);

        // The virtio-net adapter's message-signalled interrupt, armed by the
        // driver when it manages to enable MSI-X. The vector sits above the
        // PIC's range and is never a CPU exception; if the driver never arms
        // it, nothing ever raises it, and the handler simply stands ready.
        table[NET_IRQ_VECTOR].set_handler_fn(virtio_net);

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

/// The console's input and the console's queued output.
///
/// Every byte the UART has goes through the line discipline, which echoes it
/// and wakes whoever is reading fd 0; and because an echo is output, the same
/// interrupt is what drains whatever the line discipline queued behind it.
///
/// Nothing here may switch tasks. It does not have to: `receive_byte` edits the
/// line and calls `sched::wake`, which only moves a task between queues, and
/// the task it wakes runs when this handler returns and the scheduler next gets
/// a turn - not here, on the interrupt's own stack.
extern "x86-interrupt" fn serial(_frame: InterruptStackFrame) {
    crate::drivers::serial::drain_into_tty();
    // After the drain, so what the echoed input queued is carried by this same
    // visit rather than waiting for the next one.
    crate::drivers::serial::pump_tx();
    // After the FIFO is empty, so a byte arriving during the drain is not
    // lost behind an EOI that already went out.
    pic::end_of_interrupt(4);
}

/// A frame (or a burst of them) is on the virtio-net receive ring.
///
/// The stack does the work - poll the interface, which hands the frames to
/// smoltcp and wakes every task registered on socket readiness, and
/// acknowledge the device - and the APIC gets the EOI, which is where the
/// message-signalled interrupt arrived and the only place it can be ended.
/// Nothing here switches tasks: like the serial handler, it wakes and lets
/// the scheduler pick the woken task up on the next turn, so the interrupted
/// context is never left half-modified.
extern "x86-interrupt" fn virtio_net(_frame: InterruptStackFrame) {
    crate::net::irq_service();
    // End the delivery before anything that might switch away, exactly as
    // the clock handler does with the PIC: a switch from here does not
    // return until the woken task yields, and an interrupt left in service
    // would block its own vector - and every lower one - until then.
    super::lapic::eoi();
    // A woken task runs now, not at the next clock tick. In the scheduler's
    // own context - the idle hlt - this is the documented no-op and the
    // scheduler's next turn picks the woken task on its own; in any task's
    // context it hands the core over immediately.
    crate::sched::preempt();
}

/// Deliberately reachable from tests: `int3` must return here, not panic.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    BREAKPOINTS_CAUGHT.fetch_add(1, Ordering::Relaxed);
    crate::println!("caught #BP at rip {:#x}", frame.instruction_pointer);
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    if user_fault_or_die(&frame, 0) {
        return;
    }
    fatal("#DE divide error", &frame, 0)
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    if user_fault_or_die(&frame, 0) {
        return;
    }
    fatal("#UD invalid opcode", &frame, 0)
}

extern "x86-interrupt" fn stack_segment(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame, code) {
        return;
    }
    fatal("#SS stack segment", &frame, code)
}

extern "x86-interrupt" fn segment_not_present(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame, code) {
        return;
    }
    fatal("#NP segment not present", &frame, code)
}

extern "x86-interrupt" fn alignment_check(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame, code) {
        return;
    }
    fatal("#AC alignment check", &frame, code)
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, code: u64) {
    if user_fault_or_die(&frame, code) {
        return;
    }
    fatal("#GP general protection", &frame, code)
}

/// `#NM`, vector 7 - no error code, and no frame to inspect.
///
/// The lazy half of the floating-point switch, and therefore *not* a fault to
/// report: the task that took it has just run its first vector instruction since
/// it was switched in, and what it needs is its own registers loaded and
/// `CR0.TS` cleared. Nothing here can fail - the image is the task's own and is
/// 16-byte aligned by construction - so there is no path out of it that does not
/// end in the instruction being re-executed.
///
/// A trap taken in ring 0 would mean the kernel itself ran a floating-point
/// instruction, which `soft-float` says never happens; the handler is the same
/// either way, and it is written to be, because the alternative is a dead
/// machine rather than a diagnostic.
extern "x86-interrupt" fn device_not_available(_frame: InterruptStackFrame) {
    crate::sched::device_not_available();
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

    // A page of a mapping the process has made but never touched. The order
    // against the two neighbours is the whole of the reasoning: a *present* page
    // that is copy-on-write is handled above, so a page reaching here is absent
    // and a COW split could not have been what it wanted; and the stack is
    // handled below because a stack fault is a growth the stack's own bounds
    // decide, over a region with no VMA to find.
    //
    // Unlike the COW case this is not gated on the fault being a write. A read
    // of an address that has never been written is exactly what a demand-paged
    // mapping is for, and the populate tells the two apart the only way that
    // matters: the VMA's permissions decide, so a read of a `PROT_NONE` page is
    // still refused and still a signal.
    if user_address
        && crate::sched::resolve_anon_fault(
            addr,
            code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        )
    {
        return; // The page is there now; the instruction retries.
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
        pf_diag(addr, rip);
        // Still extends the stack when the fault is a push past the mapped
        // end - the same call, moved here so its answer can be printed with
        // the rest of what the page tables say.
        if crate::sched::resolve_stack_growth(addr, frame.stack_pointer.as_u64() as usize) {
            return; // The page is there now; the instruction retries.
        }
        crate::sched::send_signal(crate::sched::current(), 11);
        // A queued signal is delivered at a syscall boundary, and a faulting
        // instruction never reaches one: returning to it just faults again,
        // forever, with the signal never leaving the pending set. A SIGSEGV
        // with no handler is death - end the task here, the way the delivery
        // path at a syscall exit would, with the signal number in the exit
        // code for the parent's `wait` to read.
        if crate::sched::current_disposition(11).is_default() {
            crate::sched::exit(-11);
        }
        // A task that *installed* a handler needs the signal-frame machinery,
        // and that machinery is written against the syscall gate's frame - a
        // faulting instruction has none, and re-entering it produces the same
        // fault at the same instruction for as long as the task is allowed to
        // run. That is a livelock, and it is not a harmless one: the console
        // gets a line per fault (thousands a second, which is how `fish`'s own
        // crash handler buried everything else the machine was saying) and the
        // task holds its slice forever. Counting repeats at one instruction and
        // ending the task is what a kernel without an interrupt-frame signal
        // path can honestly do: the handler does not run either way, and a
        // death with a reason beats a spin with a flood.
        if fault_repeats(rip) {
            crate::println!(
                "task {} died of SIGSEGV it could not be handed: {addr:#x} ({code:?}) at {rip:#x}, \
                 the same fault {FAULT_REPEAT_LIMIT} times over, and a handler is installed",
                crate::sched::current().0
            );
            crate::sched::exit(-11);
        }
        crate::sched::preempt();
        return;
    }
    // The report is the whole point of this path, and on a GUI or compositor
    // boot the console is the framebuffer - so without pulling it back to the
    // wire first, a kernel that dies here leaves the reader with a vanished
    // machine and no explanation. The same move as `fatal` and the panic
    // handler in `main`.
    crate::console::force_serial();
    crate::println!(
        "\nKERNEL PANIC: #PF at {:#x} ({:?}) rip {:#x} cs {:#x} rsp {:#x}",
        addr,
        code,
        frame.instruction_pointer,
        frame.code_segment.0,
        frame.stack_pointer
    );
    // One address is a coincidence; the chain is the diagnosis. A kernel fault
    // is noticed at the instruction that touched the bad address, and what the
    // reader needs is *how the kernel got there* - which, without a frame
    // pointer to walk, means the words on the faulting stack that can only be
    // return addresses into kernel text. Printed here rather than left to a
    // host-side script for the reason the report is printed at all: on a GUI or
    // compositor boot this is the only copy that leaves the machine.
    report_frames(frame.stack_pointer.as_u64() as usize);
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

/// How many words of the faulting kernel stack are walked for return addresses.
///
/// Deep enough to cross a syscall's frame and the scheduler step underneath it,
/// and bounded because the walk reads memory the fault may itself have damaged.
const FRAME_SCAN_WORDS: usize = 96;

/// Print the return addresses on the stack of a fault that is about to be fatal.
///
/// x86-64 code this kernel builds carries no frame pointer, so there is nothing
/// to *walk* - but a kernel return address is recognisable by being in kernel
/// text, and a stack full of them is the call chain in order of freshness. That
/// is the difference between "a #PF writing 0x0 at one instruction" and "the
/// exec path, from `execve`, got there through these four functions", which is
/// the question a reader of a dead machine is asking.
///
/// Printed as plain addresses rather than resolved here: the symbol table is
/// what the host has, not what the kernel kept, and a page fault is the worst
/// possible moment to go looking for it.
fn report_frames(rsp: usize) {
    const KERNEL_TEXT: u64 = 0xffff_ffff_8000_0000;
    const KERNEL_TEXT_END: u64 = 0xffff_ffff_c000_0000;
    crate::println!("  return addresses on the fault's kernel stack:");
    // Bounded by the stack the task was actually given: the walk reads memory,
    // and the only memory it may read without asking anybody is the block the
    // scheduler allocated for this task's own kernel stack.
    let Some((base, top)) = crate::sched::current_stack_range() else {
        crate::println!("    (no kernel stack to read)");
        return;
    };
    if rsp < base || rsp >= top {
        crate::println!("    (rsp {rsp:#x} is outside this task's stack {base:#x}..{top:#x})");
        return;
    }
    let room = ((top - rsp) / 8).min(FRAME_SCAN_WORDS);
    let mut printed = 0usize;
    for i in 0..room {
        let value = unsafe { core::ptr::read_unaligned((rsp + i * 8) as *const u64) };
        if (KERNEL_TEXT..KERNEL_TEXT_END).contains(&value) {
            crate::println!("    rsp+{:#05x} {:#018x}", i * 8, value);
            printed += 1;
            if printed >= 16 {
                break;
            }
        }
    }
    if printed == 0 {
        crate::println!("    (none: the fault took the stack with it)");
    }
}

/// How many times one instruction may fault unresolvably before the task stuck
/// on it is ended rather than sent round again.
const FAULT_REPEAT_LIMIT: usize = 3;

/// Has the task on the CPU now faulted at the *same* instruction
/// [`FAULT_REPEAT_LIMIT`] times running?
///
/// One triple of globals is enough: only one task runs at a time, so the pair
/// (task, rip) is exactly the condition worth counting, and a different task or
/// a different instruction is a different problem - which is why the counters
/// are replaced rather than decremented. No lock: the values are advisory, two
/// faults cannot happen at once on one CPU, and being wrong by one only changes
/// how soon the task is ended.
fn fault_repeats(rip: usize) -> bool {
    static LAST_TASK: AtomicUsize = AtomicUsize::new(usize::MAX);
    static LAST_RIP: AtomicUsize = AtomicUsize::new(usize::MAX);
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    let task = crate::sched::current().0;
    if LAST_TASK.load(Ordering::Relaxed) == task && LAST_RIP.load(Ordering::Relaxed) == rip {
        return COUNT.fetch_add(1, Ordering::Relaxed) + 1 >= FAULT_REPEAT_LIMIT;
    }
    LAST_TASK.store(task, Ordering::Relaxed);
    LAST_RIP.store(rip, Ordering::Relaxed);
    COUNT.store(1, Ordering::Relaxed);
    false
}

/// What the page tables say about the page a user fault named, printed beside
/// the fault itself.
///
/// This is the same instinct as the register dump in [`fatal`]: when a fault
/// has a cause the error code cannot express, read the scene rather than
/// deduce it from the code. The `#PF` error code says "write, user, present" -
/// four bits - and the interesting half of the answer is in the entry that
/// refused the access: whether the leaf permits a write, whether the levels
/// above it do, and how far the mapping reaches in each direction.
///
/// It has earned its place twice over. A page whose own PTE read `P=1 W=1 U=1`
/// was still refusing writes, and the run - identical bits down to page 0, then
/// a page whose bits differed - was the only way to see that the *tables above*
/// were the ones withholding it. And a write to `0x8c` in a program's own
/// read-only image is a null `FILE *` passed to stdio, which the run makes
/// obvious and the address alone does not.
fn pf_diag(addr: usize, rip: usize) {
    use core::sync::atomic::AtomicUsize;

    // A handful per boot: a task that faults in a loop would otherwise bury
    // every other thing the machine is saying, which is exactly the failure
    // mode `fault_repeats` exists to cut short.
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) >= 4 {
        return;
    }

    let page_size = crate::mm::PAGE_SIZE;
    let page = addr & !(page_size - 1);
    // P, W, U, and bit 9 (the COW marker this kernel sets instead of clearing
    // the write bit, if it does).
    let bits = |e: u64| -> u64 {
        (e & 1) | (((e >> 1) & 1) << 1) | (((e >> 2) & 1) << 2) | (((e >> 9) & 1) << 3)
    };

    let Some(entry) = crate::arch::paging::leaf_pte(page) else {
        // The bytes of the instruction that faulted, up to the end of its own
        // page: a faulting access is usually one instruction whose operand
        // went bad, and the bytes name it - `48 89` is a store, `f4` is
        // musl's `a_crash`, an `0f` prefix says which SSE form it is.
        let room = (PAGE_SIZE - (rip & (PAGE_SIZE - 1))).min(16);
        let bytes = unsafe { core::slice::from_raw_parts(rip as *const u8, room) };
        // And how far the *mapped* pages reach around the hole. A dereference
        // of a pointer that went bad lands a page or two from something real,
        // and the run says so; an access into a mapping the kernel never
        // filled in lands at the bottom of a long run of absent pages, and
        // only the length of that run tells the two apart. Bounded, because
        // the answer is a hint and the handler must not walk the address
        // space looking for it.
        let mut lo = page;
        for _ in 0..64 {
            if lo >= page_size && crate::arch::paging::leaf_pte(lo - page_size).is_some() {
                lo -= page_size;
            } else {
                break;
            }
        }
        let mut hi = page + page_size;
        for _ in 0..64 {
            if hi < (1 << 47) && crate::arch::paging::leaf_pte(hi).is_some() {
                hi += page_size;
            } else {
                break;
            }
        }
        crate::println!(
            "PFDIAG page={page:#x} NOT MAPPED task={:?} rip={rip:#x} mapped=[{lo:#x}..{hi:#x}) bytes={:02x?}",
            crate::sched::current().0,
            bytes
        );
        return;
    };

    let want = bits(entry);
    let mut lo = page;
    while lo >= page_size
        && crate::arch::paging::leaf_pte(lo - page_size).is_some_and(|e| bits(e) == want)
    {
        lo -= page_size;
    }
    let mut hi = page + page_size;
    while hi < (1 << 47) && crate::arch::paging::leaf_pte(hi).is_some_and(|e| bits(e) == want) {
        hi += page_size;
    }

    crate::println!(
        "PFDIAG page={page:#x} pte={entry:#x} P={} W={} U={} B9={} phys={:#x} run=[{lo:#x}..{hi:#x}) \
         task={:?} rip={rip:#x}",
        entry & 1,
        (entry >> 1) & 1,
        (entry >> 2) & 1,
        (entry >> 9) & 1,
        entry & 0x000f_ffff_ffff_f000,
        crate::sched::current().0
    );

    // Then the whole walk, not just the leaf. The leaf's own bits are a true
    // statement about the leaf and not about the access: the U/S bit is ANDed
    // along the walk, and a write on a 4 KiB page can be refused by a level
    // above it. Every level is printed, unused slots included, so the shape of
    // the walk is unambiguous.
    for (level, e) in crate::arch::paging::entry_chain(page) {
        crate::println!(
            "PFDIAG L{level} entry={e:#x} P={} W={} U={} PS={}",
            e & 1,
            (e >> 1) & 1,
            (e >> 2) & 1,
            (e >> 7) & 1
        );
    }
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
    // The report is the whole reason for the last-resort path, so it has to
    // reach the host: on a GUI or compositor boot the console lives in the
    // framebuffer, and a machine that dies with its report painted into a
    // window nobody is looking at leaves the reader with a vanished QEMU and
    // no explanation. Same move as the panic handler in `main`.
    crate::console::force_serial();
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
    report_frames(frame.stack_pointer.as_u64() as usize);
    crate::drivers::qemu::exit(crate::drivers::qemu::EXIT_FAILURE)
}

/// If the faulting rip is in the lower half (user mode), end the task or send
/// it a signal, and return `true`. Otherwise return `false` so the caller can
/// panic.
///
/// The one printed line matters more than it looks: a `#GP` in ring 3 means
/// user code attempted something the CPU refuses - `hlt` outside ring 0, a
/// non-canonical operand, an SSE access the FPU state does not allow - and the
/// only evidence that it happened is this line. Silently converting it to a
/// signal leaves the bytes, which are the diagnostic, on the floor.
///
/// The *first* fault is reported in full - the error code, the bytes of the
/// instruction the CPU refused, and the stack pointer it refused them on -
/// because the interesting ring-3 `#GP`s are one instruction made illegal by
/// kernel state the process cannot see, and the bytes name it. The rest are
/// the process retrying the same instruction, which Linux logs too, rate
/// limited, and which would otherwise bury the first one.
///
/// What happens after the report is the same answer the `#PF` path gives, and
/// for the same reason: a signal queued here is delivered at a *syscall*
/// boundary, and a faulting instruction never reaches one, so returning to it
/// only faults again. A default disposition is therefore death here (with the
/// signal number as the exit code), and a task that installed a handler is
/// ended once it has faulted at the same instruction
/// [`FAULT_REPEAT_LIMIT`] times - the handler cannot be entered from a trap
/// frame either way, and a death with a reason beats a flood. `hlt` is the
/// case that made this visible: it is musl's `a_crash()`, the instruction its
/// allocator runs when it has found the heap corrupted, and a compositor that
/// hits it used to spin instead of dying.
fn user_fault_or_die(frame: &InterruptStackFrame, code: u64) -> bool {
    let rip = frame.instruction_pointer.as_u64() as usize;
    if rip < (1 << 47) {
        if !GP_REPORTED.swap(true, Ordering::Relaxed) {
            // Only up to the end of the page the instruction is on: a read
            // past it would fault inside the handler, and a double fault
            // would take the machine down instead of reporting anything.
            let room = (PAGE_SIZE - (rip & (PAGE_SIZE - 1))).min(16);
            let bytes = unsafe { core::slice::from_raw_parts(rip as *const u8, room) };
            crate::println!(
                "  #GP code {:#x} rip {:#x} rsp {:#x} rflags {:#x}",
                code,
                rip,
                frame.stack_pointer.as_u64(),
                frame.cpu_flags.bits()
            );
            crate::println!("  bytes at rip: {:02x?}", bytes);
        }
        crate::println!(
            "user #GP/#SS/#NP at rip {:#x} cs {:#x}",
            rip,
            frame.code_segment.0
        );
        crate::sched::send_signal(crate::sched::current(), 11); // SIGSEGV
        if crate::sched::current_disposition(11).is_default() {
            crate::sched::exit(-11);
        }
        if fault_repeats(rip) {
            crate::println!(
                "task {} died of SIGSEGV it could not be handed: #GP {code:#x} at {rip:#x}, \
                 the same fault {FAULT_REPEAT_LIMIT} times over, and a handler is installed",
                crate::sched::current().0
            );
            crate::sched::exit(-11);
        }
        crate::sched::preempt();
        return true;
    }
    false
}

/// The bytes of an instruction fit in a 16-byte window, which is also the
/// longest x86-64 instruction: reading up to the end of the faulting page is
/// what keeps the report from faulting itself.
const PAGE_SIZE: usize = 4096;

/// Whether the full report of the first ring-3 `#GP` has been printed. A
/// process that returns from its own SIGSEGV handler faults again on the very
/// next instruction, so the console sees the same fault thousands of times;
/// one full report is what a reader needs.
static GP_REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
