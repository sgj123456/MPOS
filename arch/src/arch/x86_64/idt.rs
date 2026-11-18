//! Interrupt Descriptor Table and dispatch.
//!
//! Nine entries are hand-written asm; every other handler uses the
//! `x86-interrupt` ABI (nightly-only), which lets the compiler emit the register
//! save/restore and `iretq` itself. That composes with preemption: a handler can
//! switch tasks without ever returning, because the trap frame stays on the
//! interrupted task's own stack and unwinds normally whenever that task is
//! resumed.
//!
//! The nine share one reason - the one thing the ABI cannot give:
//! `x86-interrupt` hides the general registers behind a prologue Rust cannot
//! address, and a handled signal has to be handed over *at the trap* - Linux's
//! delivery point - which means rewriting the interrupted program's whole
//! register file. Each stub pushes a [`TrapRegs`] block, calls Rust, pops and
//! `iretq`. The PIT entry ([`timer_stub`]) has done this since signals were
//! first delivered at a trap. The page-fault entry ([`page_fault_stub`]) does it
//! so a fault a task installed a `SIGSEGV` handler for enters that handler here
//! rather than waiting for the next tick - a measured 2/16 in a guest
//! (`docs/notes/` §6.161). The seven CPU exceptions a process can see
//! ([`trap_common`]'s entries, `#DE`/`#BP`/`#UD`/`#NP`/`#SS`/`#GP`/`#AC`) joined
//! them on 2026-11-15 for the same reason: a queued signal waits for a syscall
//! boundary a faulting instruction never reaches, so `div0` died of `SIGFPE`
//! "it could not be handed" instead of running its handler
//! (`docs/notes/` §6.164). These stubs are the same block shape `syscall_entry`
//! builds, which is what lets one `hal::capture_trap_regs` serve every door onto
//! a user register file (`docs/notes/` §2.13). A hand-written stub buys control
//! over the exact frame layout, and only an entry that must build a *signal*
//! frame needs one.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Once;
use x86_64::PrivilegeLevel;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::pic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

/// The vector the virtio-net adapter's MSI-X entry points at: clear of the
/// PIC's 32..47 and of every architecturally reserved slot. The driver arms
/// the device to raise it when a frame lands; until it does, the handler
/// below is registered but never runs.
pub const NET_IRQ_VECTOR: u8 = 0x41;
/// The vector the block device's MSI-X entry points at, beside the network
/// one: a completion on the transfer ring lands here instead of in a masked
/// spin inside the transfer (§10.5's structure change).
pub const BLK_IRQ_VECTOR: u8 = 0x42;

static BREAKPOINTS_CAUGHT: AtomicU64 = AtomicU64::new(0);

pub fn breakpoints_caught() -> u64 {
    BREAKPOINTS_CAUGHT.load(Ordering::Relaxed)
}

/// The stack the CPU loads `RSP` from when it takes a double fault: the vector
/// that reports "the stack I was going to report on was the thing that faulted".
///
/// 32 KiB for a last report that consists of a few formatted lines onto the
/// serial wire and one port write, with room for the format machinery and the
/// `current_stack_range()` check that keeps the return-address walk inside memory
/// it is allowed to read. It is ordinary `.bss`, so it is mapped for the whole
/// life of the machine, and nothing else is given it: a `#DF` that nested inside a
/// `#DF` would overwrite it, and that third fault is a triple fault whether the
/// stack is shared or not.
#[repr(C, align(16))]
struct FaultStack([u8; 32 * 1024]);

/// The `#DF` stack, and the software IST slot it is published in: slot 0 is the
/// hardware `IST1`, which is what `set_stack_index` counts from.
static DOUBLE_FAULT_STACK: FaultStack = FaultStack([0; 32 * 1024]);
const DOUBLE_FAULT_IST: usize = 0;

pub fn init() {
    let idt = IDT.call_once(|| {
        let mut table = InterruptDescriptorTable::new();

        // Every CPU exception a process can see is a hand-written stub, not an
        // `x86-interrupt` handler: a handled `SIGILL`/`SIGFPE`/`SIGTRAP` has to
        // be entered *at the trap*, which means rewriting the program's whole
        // register file - and the ABI hides those registers behind a prologue
        // Rust cannot address (module doc, `docs/notes/` §6.164).
        //
        // `#BP` keeps its DPL of 3: a software `int3` from ring 3 is checked
        // against the gate's privilege, so a ring-0 gate refuses the
        // instruction and raises `#GP(0x1a)` instead - whose error word names
        // vector 3 as the one it could not deliver. Musl's `a_crash()` and
        // chromium's padding are `int3` bytes, so that refusal turned an
        // ordinary trap into a protection fault (`docs/notes/` §6.159).
        unsafe {
            table
                .debug
                .set_handler_addr(x86_64::VirtAddr::new(trap_db as *const () as u64));
            table
                .divide_error
                .set_handler_addr(x86_64::VirtAddr::new(trap_de as *const () as u64));
            table
                .breakpoint
                .set_handler_addr(x86_64::VirtAddr::new(trap_bp as *const () as u64))
                .set_privilege_level(PrivilegeLevel::Ring3);
            table
                .invalid_opcode
                .set_handler_addr(x86_64::VirtAddr::new(trap_ud as *const () as u64));
        }
        // The one vector that gets a stack of its own, and it is not a
        // tidiness. Every other fault pushes its frame on the stack it just found
        // unusable; if that push is what failed, the fault promotes to `#DF`, and a
        // `#DF` with no IST slot pushes *its* frame on the same broken stack and
        // promotes again - to a triple fault. The CPU then resets, `cargo xtask
        // run` passes `-no-reboot` (xtask/src/qemu.rs), and QEMU walks out without
        // the kernel writing one byte of explanation: the report of a kernel that
        // ran off the end of its own stack is exactly the report that cannot be
        // made from that stack. So the CPU is given a stack that does not depend on
        // the fault, and `fatal()` gets to say which vector, at which `rip`, ended
        // the machine.
        unsafe {
            super::gdt::set_ist(
                DOUBLE_FAULT_IST,
                core::ptr::addr_of!(DOUBLE_FAULT_STACK) as usize
                    + core::mem::size_of::<FaultStack>(),
            );
            table
                .double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(DOUBLE_FAULT_IST as u16);
        }
        unsafe {
            table
                .general_protection_fault
                .set_handler_addr(x86_64::VirtAddr::new(trap_gp as *const () as u64));
            table
                .page_fault
                .set_handler_addr(x86_64::VirtAddr::new(page_fault_stub as *const () as u64));
            table
                .stack_segment_fault
                .set_handler_addr(x86_64::VirtAddr::new(trap_ss as *const () as u64));
            table
                .segment_not_present
                .set_handler_addr(x86_64::VirtAddr::new(trap_np as *const () as u64));
            table
                .alignment_check
                .set_handler_addr(x86_64::VirtAddr::new(trap_ac as *const () as u64));
        }
        // Vector 7: raised by the first floating-point instruction a task runs
        // after a switch set `CR0.TS`, which is how the deferred half of the
        // FPU switch is delivered. Not an error path - it is the mechanism.
        table
            .device_not_available
            .set_handler_fn(device_not_available);

        // The tick is the one entry that is hand-written rather than an
        // `x86-interrupt` handler: a handled signal is handed over *at the
        // trap* (`rust_timer`), and that needs the interrupted program's whole
        // register file, which the ABI keeps behind a prologue Rust cannot
        // address. See the module doc and `TrapRegs`.
        unsafe {
            table[pic::IRQ0_VECTOR]
                .set_handler_addr(x86_64::VirtAddr::new(timer_stub as *const () as u64));
        }
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
        // The block device's MSI-X completion: registered once and silent
        // until the driver arms its entry, exactly like the network vector
        // beside it.
        table[BLK_IRQ_VECTOR].set_handler_fn(virtio_blk);

        // The local APIC timer's one-shot: the scheduler arms it at the
        // earliest wait that would otherwise round up to the next PIT tick
        // (`lapic::arm_one_shot`). Like the net vector it is registered once and
        // silent until something arms it.
        table[super::lapic::TIMER_VECTOR].set_handler_fn(apic_timer);
        // The APIC's spurious vector: a delivery the local APIC could not make
        // is reported here so that enabling delivery cannot turn a race into an
        // unhandled vector. It does nothing, which is the whole contract - there
        // is no interrupt, only a notification that there was none.
        table[super::lapic::SPURIOUS_VECTOR].set_handler_fn(spurious);

        table
    });

    idt.load();
}

pub fn enable() {
    x86_64::instructions::interrupts::enable();
}

/// The PIT tick's register block, built by [`timer_stub`].
///
/// Its field order *is* the stub's push order, which is also the architecture's
/// own order for a trap: the fifteen general registers the stub pushes, and then
/// the frame the CPU pushed before entering - `rip`, `cs`, `rflags`, and, only
/// for a trap from ring 3, `rsp` and `ss`. Everything below `ss` is written
/// unconditionally; the two words a ring-0 trap does not push are simply absent
/// from that frame, which is why a ring-0 trap passes a **null** frame to the
/// kernel rather than a block holding whatever was on the stack.
///
/// This is not the shape of [`super::syscall::Regs`], and the difference is the
/// point: `syscall` clobbers `rcx` and `r11` (it stashes the return address and
/// the flags there), so that block reports `rip`/`eflags` in their slots. A trap
/// clobbers neither, so this block carries them where the register file names
/// them - `r11` is the program's `r11`, and the flags live in the CPU's frame.
/// A signal frame built from this block therefore reports the register file the
/// *program* had, not one reconstructed from what an instruction happened to
/// clobber (`docs/notes/` §2.13).
#[repr(C)]
pub(super) struct TrapRegs {
    pub r15: usize,
    pub r14: usize,
    pub r13: usize,
    pub r12: usize,
    pub rbp: usize,
    pub rbx: usize,
    pub r11: usize,
    pub r10: usize,
    pub r9: usize,
    pub r8: usize,
    pub rax: usize,
    pub rcx: usize,
    pub rdx: usize,
    pub rsi: usize,
    pub rdi: usize,
    pub rip: usize,
    pub cs: usize,
    pub rflags: usize,
    pub rsp: usize,
    pub ss: usize,
}

/// The PIT tick, entered through [`timer_stub`].
///
/// `frame` is the register block the stub built, or null when the tick
/// interrupted the kernel: a trap from ring 0 has no program context to hand a
/// signal handler, and its frame carries no user `rsp`/`ss` to build one from.
///
/// The work is what the `x86-interrupt` version did, plus the one thing that
/// version could not do: hand a waiting signal over *here*, which is Linux's
/// delivery point and the only one a program that never calls can reach
/// (`deliver_at_trap`, `docs/notes/` §2.13).
extern "C" fn rust_timer(frame: *mut TrapRegs, in_user: bool) {
    super::timer::tick();
    // Charge the tick to the process that was running before the interrupt,
    // attributed user or system by the ring the core was left in: a code
    // segment equal to the user selector means the tick caught the task in its
    // own ring-3 code (`utime`), anything else means the kernel was running on
    // its behalf (`stime`). Charged before the EOI and the preempt, so the tick
    // lands on the task that actually held the core.
    crate::hal::kernel::charge_cpu(in_user);
    // EOI before preempting: once the scheduler switches away this handler does
    // not return until the task is resumed, and an unacknowledged PIC would
    // block every interrupt until then.
    pic::end_of_interrupt(0);
    // A handled signal is handed over at this trap when there is a program to
    // hand it to - before the preempt below, not after: a delivery rewrites the
    // register block, and a task switched away first would resume into the
    // block without the handler in it. The clock is the interrupt guaranteed to
    // keep arriving while a program computes, which is what makes it the one
    // that can reach a program that never calls.
    #[cfg(mpos_signal_frame)]
    {
        if in_user && !frame.is_null() {
            crate::hal::kernel::deliver_at_trap(frame as *mut ());
        }
    }
    // Where the tick landed is what lets the scheduler keep a restartable
    // sequence unbroken: a task in ring 3 inside an armed span is not switched
    // away from (`crate::sched::rseq`). A tick in kernel mode has no user
    // address to offer, and uses the address-less call.
    if in_user && !frame.is_null() {
        // SAFETY: the stub built this block on the task's own kernel stack and
        // has not returned, so it is live for the length of this call.
        let user_ip = unsafe { (*frame).rip };
        crate::hal::kernel::preempt_at(user_ip);
    } else {
        crate::hal::kernel::preempt();
    }
}

core::arch::global_asm!(
    ".section .text.timer_stub, \"ax\"",
    ".globl timer_stub",
    ".p2align 4",
    "timer_stub:",
    // Save the fifteen general registers, in the order `TrapRegs` lists them
    // from the top down. Nothing here is ring-dependent: the CPU has already
    // pushed its own frame (rip/cs/rflags, plus rsp/ss for a trap from ring 3)
    // above what these pushes build, so the block below it is the same shape
    // either way - which is what lets one struct serve both entries.
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push rcx",
    "    push rax",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    // rbp becomes the frame base, now that the program's own value is safely
    // below. Every load and the epilogue's `mov rsp, rbp` go through it.
    "    mov  rbp, rsp",
    // The ring the trap came from, read from the CS the CPU pushed - at a fixed
    // offset from the base, because 15 pushes sit between it and rbp. `and
    // ecx, 3` answers 0 for a kernel CS and 3 for a user one, so the same value
    // is both the answer and the null/non-null decision for the frame argument.
    "    mov  rcx, [rbp + 128]",
    "    and  ecx, 3",
    // A null frame for a trap from the kernel: there is no program context, and
    // the block's `rsp`/`ss` words did not get pushed at all. `in_user` is a
    // Rust `bool`, so it is set to exactly 0 or 1 - the ring bits are 0 or 3,
    // and handing Rust a 3 as a `bool` is undefined behaviour, not a shorthand
    // for true.
    "    xor  edi, edi",
    "    xor  esi, esi",
    "    cmp  ecx, 3",
    "    jne  2f",
    "    mov  rdi, rbp",
    "    mov  esi, 1",
    "2:",
    // The call's own alignment rule, and rbp is what survives it.
    "    and  rsp, -16",
    "    call {rust_timer}",
    "    mov  rsp, rbp",
    // Pop the block back. `rbp` is restored fifth, which is the last moment the
    // base is needed - after it the stack pointer walks up through the rest on
    // its own.
    "    pop  r15",
    "    pop  r14",
    "    pop  r13",
    "    pop  r12",
    "    pop  rbp",
    "    pop  rbx",
    "    pop  r11",
    "    pop  r10",
    "    pop  r9",
    "    pop  r8",
    "    pop  rax",
    "    pop  rcx",
    "    pop  rdx",
    "    pop  rsi",
    "    pop  rdi",
    // What is left under the stack pointer is the CPU's own frame, and `iretq`
    // pops it exactly as far as the CS it finds says to - so the ring-3 and
    // ring-0 returns are the same instruction here, and neither needs a branch.
    "    iretq",
    rust_timer = sym rust_timer,
);

unsafe extern "C" {
    fn timer_stub();
}

/// The local APIC's one-shot: a wait shorter than a tick is due now.
///
/// It does no bookkeeping of its own. The scheduler's next turn is what reads
/// the clock (`since_boot_nanos`), moves it and wakes whoever's nanosecond
/// deadline has passed - the same turn every other preemption takes, so there is
/// one clock advance and not a second, subtly different one here. The PIT tick
/// stays armed beneath it, which is why a lost one-shot costs a tick's latency
/// and cannot stall the machine.
/// A spurious vector: an interrupt the local APIC withdrew before it was
/// delivered. Acknowledging is all there is to do - the vector was never really
/// in service, and the LAPIC wants no EOI for it.
extern "x86-interrupt" fn spurious(_frame: InterruptStackFrame) {}

extern "x86-interrupt" fn apic_timer(frame: InterruptStackFrame) {
    // The APIC, not the PIC, raised this one: the 8259's EOI port knows nothing
    // about it, and a vector left in service would block this one and every
    // lower-priority delivery. EOI before preempting for the same reason the PIT
    // handler does it - a preempted handler does not return until it is resumed.
    super::lapic::eoi();
    if frame.code_segment == super::gdt::user_selectors().0 {
        crate::hal::kernel::preempt_at(frame.instruction_pointer.as_u64() as usize);
    } else {
        crate::hal::kernel::preempt();
    }
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
    crate::arch::x86_64::serial::drain_into_tty();
    // After the drain, so what the echoed input queued is carried by this same
    // visit rather than waiting for the next one.
    crate::arch::x86_64::serial::pump_tx();
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
/// A block request completed on the transfer ring.
///
/// The mailbox is the whole of the work - ack the device, mark, leave. The
/// submitter is *spinning* on the mailbox with interrupts unmasked (it must
/// not yield: the block layer's callers hold non-reentrant borrows across the
/// call), so the EOI hands the core straight back to it and it retires the
/// request. A preempt here would switch another task in mid-operation —
/// exactly the re-entrancy the unmasked poll exists to avoid.
extern "x86-interrupt" fn virtio_blk(_frame: InterruptStackFrame) {
    crate::hal::kernel::service_blk();
    super::lapic::eoi();
}

extern "x86-interrupt" fn virtio_net(_frame: InterruptStackFrame) {
    crate::hal::kernel::service_network();
    // End the delivery before anything that might switch away, exactly as
    // the clock handler does with the PIC: a switch from here does not
    // return until the woken task yields, and an interrupt left in service
    // would block its own vector - and every lower one - until then.
    super::lapic::eoi();
    // A woken task runs now, not at the next clock tick. In the scheduler's
    // own context - the idle hlt - this is the documented no-op and the
    // scheduler's next turn picks the woken task on its own; in any task's
    // context it hands the core over immediately.
    crate::hal::kernel::preempt();
}

/// A CPU exception taken in **ring 3**, entering through one of the stubs above.
///
/// The one printed line matters more than it looks: a trap in ring 3 means user
/// code executed something the CPU refuses to complete, and the only evidence
/// that it happened is this line. Silently converting it to a signal leaves the
/// bytes, which are the diagnostic, on the floor.
///
/// `vector` is which trap it was: this door serves seven (`#BP` 3, `#DE` 0,
/// `#UD` 6, `#NP` 11, `#SS` 12, `#GP` 13, `#AC` 17), and the stub passes its own
/// number because the frame does not carry it. Until 2026-11-14 the line said
/// `#GP` for all of them, so the `0f 0b` musl's `a_crash` runs after the
/// allocator has found the heap corrupted was reported as a protection fault - a
/// word that sends the reader after a fault that was never raised
/// (`docs/notes/` §6.158).
///
/// The *first* fault is reported in full: the vector, the error code, the bytes
/// of the instruction the CPU refused, and the stack pointer it refused them on.
/// That is because the interesting ring-3 traps are one instruction made illegal
/// by kernel state the process cannot see, and the bytes name it. The rest are
/// the process retrying the same instruction, which Linux logs too, rate
/// limited, and which would otherwise bury the first one.
///
/// What happens after the report is the vector's own signal, not one signal for
/// the whole door. The map was measured one instruction at a time on Linux
/// 7.2.14 (`docs/notes/` §6.159): `#DE` ends the child `SIGFPE`, `#UD` `SIGILL`,
/// `#BP` `SIGTRAP`, `#AC` `SIGBUS`, and `#SS`/`#NP`/`#GP` `SIGSEGV`. A parent's
/// `wait` reads that number back through `WTERMSIG`, so a door that answers
/// `SIGSEGV` for a `ud2` is not a cosmetic difference - it tells the caller a
/// different event happened.
///
/// A handler the process installed is entered **here**, at the trap: the stub
/// hands over a [`TrapRegs`] block, so `deliver_at_trap` rewrites the program's
/// register file and this returns into the handler instead of returning to the
/// faulting instruction. Until 2026-11-15 these entries were `x86-interrupt`
/// handlers, which hide the general registers behind a prologue Rust cannot
/// address, so the signal was queued and waited for the next tick - and a
/// `div0` died of `SIGFPE` "it could not be handed" before one arrived
/// (measured in a guest: `docs/notes/` §6.164). What is left of the old shape is
/// the repeat guard, which still ends a task whose handler returns to the same
/// instruction for ever.
extern "C" fn rust_trap(frame: *mut TrapRegs, vector: u64, code: u64) {
    if frame.is_null() {
        return;
    }
    let (rip, rsp, rflags) = unsafe { ((*frame).rip, (*frame).rsp, (*frame).rflags) };
    let vector = vector as u8;
    // One table answers both halves: the signal number (Item 279) and the
    // fault-shaped `siginfo_t` record (Item 280). A trap with no faulting
    // address passes zero; `#DE`/`#UD` name `rip` as their `si_addr`, and
    // `#BP`'s is zero because the CPU hands over the address *after* the
    // instruction and there is nothing the program wrote that faults.
    // The vector is this machine's own word; the kernel's table is keyed on the
    // *class* of exception (item 284, `docs/notes/` §6.165), so it is read here
    // and only here.
    // **A single-step trap is a `ptrace` stop, not a fault.** `#DB` (vector 1)
    // arrives after every instruction when `RFLAGS.TF` is set, and with a
    // tracer that is exactly `PTRACE_SINGLESTEP`'s stop: the tracee parks, the
    // tracer is woken, and no signal is invented. Without a tracer it falls
    // through to the ordinary path below and is signed `SIGTRAP` like any other
    // trap. The step bit is cleared here so a resumed tracee does not trap
    // again immediately - `PTRACE_SINGLESTEP` sets it afresh each time.
    if vector == 1 {
        let (_slot, bit) = crate::hal::STEP_BIT;
        unsafe { (*frame).rflags &= !(bit as usize) };
        // SAFETY: the stub pushed the general registers at the block's start,
        // which is the `AbiRegs` the scheduler parks with.
        if crate::hal::kernel::step_trap(frame as *mut ()) {
            return;
        }
    }
    let kind = crate::hal::kernel::fault_kind_of_vector(vector);
    let sig = crate::hal::kernel::send_fault_signal(kind, 0, rip);
    if claim_trap_report() {
        // Only up to the end of the page the instruction is on: a read past it
        // would fault inside the handler, and a double fault would take the
        // machine down instead of reporting anything.
        let room = (PAGE_SIZE - (rip & (PAGE_SIZE - 1))).min(16);
        let bytes = unsafe { core::slice::from_raw_parts(rip as *const u8, room) };
        crate::hal::kernel::kprintln!(
            "  trap {vector} code {:#x} rip {:#x} rsp {:#x} rflags {:#x}",
            code,
            rip,
            rsp,
            rflags
        );
        crate::hal::kernel::kprintln!("  bytes at rip: {:02x?}", bytes);
        // The **general registers**, and only their values: printing what a
        // pointer points at would fault inside the fault handler, and a double
        // fault takes the machine down instead of reporting anything (the
        // same reason `bytes` is bounded above).
        //
        // This exists for one fault class. A `#GP` from a byte-wise `memcmp`
        // cannot be an alignment fault - a `movzx` reads one byte - so it names
        // a **non-canonical base register**, and the register file is the only
        // place that answer lives: `docs/notes/16-init-boot-race.md` records two
        // `sh[1]` general-protection deaths whose instruction was a `memcmp`
        // and whose cause could not be read off `rip`/`rsp` alone.
        let r = unsafe { &*frame };
        crate::hal::kernel::kprintln!(
            "  regs: rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x}",
            r.rax,
            r.rbx,
            r.rcx,
            r.rdx
        );
        crate::hal::kernel::kprintln!(
            "        rsi {:#x} rdi {:#x} rbp {:#x} rsp {:#x}",
            r.rsi,
            r.rdi,
            r.rbp,
            r.rsp
        );
        crate::hal::kernel::kprintln!(
            "        r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
            r.r8,
            r.r9,
            r.r10,
            r.r11,
            r.r12,
            r.r13,
            r.r14,
            r.r15
        );
    }
    // **A breakpoint a tracer planted is the tracer's stop, not the program's
    // fault.** `PTRACE_POKETEXT` writes `0xcc` over an instruction, the tracee
    // executes it, and what must happen is what Linux does: the tracee parks,
    // the tracer is woken with a `SIGTRAP` stop, and the traced program never
    // sees a trap at all. Without this the `int3` was signed `SIGTRAP` and the
    // *default disposition killed the tracee* - so a tracer could plant a
    // breakpoint and then watch its subject die on the resume, which is the
    // shape every probe in §4g hit.
    //
    // Two things separate it from a step trap: the saved `rip` points *after*
    // the `int3` (the CPU pushes the following instruction), and Linux's tracer
    // rewinds it by one before reporting, so `PTRACE_GETREGS` answers the
    // address of the byte the tracer wrote. Doing that rewind here is what lets
    // a tracer restore the original instruction and resume.
    if vector == 3 {
        // The rewind is **the tracer's**, not the program's: Linux reports a
        // planted breakpoint with `rip` on the byte the tracer wrote, and
        // reports a program's own `int3` with the saved `rip` (one past it).
        // Rewinding unconditionally put a signal-handling program back on its
        // own `int3` for ever - measured as `t-trap-at-trap` hanging at
        // `traps: … breakpoint ip:43abc4` repeated until its budget ran out.
        if crate::hal::kernel::tracing_self() {
            unsafe { (*frame).rip -= 1 };
            if crate::hal::kernel::step_trap(frame as *mut ()) {
                return;
            }
        }
    }
    crate::hal::kernel::report_user_trap(vector, rip, rsp, code as usize);
    if crate::hal::kernel::disposition_is_default(sig) {
        crate::hal::kernel::die_of_fatal_signal(sig);
    }
    // The delivery point, and the reason these entries are hand-written: the
    // block is a register file this kernel can rewrite, so the handler runs now
    // rather than at whatever syscall the process makes next - which for a
    // faulting instruction is never.
    #[cfg(mpos_signal_frame)]
    {
        if crate::hal::kernel::deliver_at_trap(frame as *mut ()) {
            return;
        }
    }
    if fault_repeats(rip) {
        crate::hal::kernel::kprintln!(
            "task {} died of signal {sig} it could not be handed: trap {vector} code {code:#x} \
             at {rip:#x}, the same fault {FAULT_REPEAT_LIMIT} times over, and a handler is installed",
            crate::hal::kernel::current()
        );
        crate::hal::kernel::die_of_fatal_signal(sig);
    }
    crate::hal::kernel::preempt();
}

/// A CPU exception taken in **ring 0**: a kernel bug, reported and fatal.
///
/// There is no signal to invent and no caller to return to, so the report is the
/// whole point and this never returns. The raw facts arrive as arguments rather
/// than as an [`InterruptStackFrame`] because the stub is what built the block -
/// `base` is the `TrapRegs` it pushed, so the report can walk the kernel stack
/// it interrupted.
///
/// The name comes from the kernel ([`crate::hal::kernel::trap_name`]) and not
/// from the machine: one table serves this report and the `traps:` line, so a
/// ring-0 `#UD` cannot be printed as a protection fault while a ring-3 one is
/// named correctly. `code` is only a selector on `#GP`'s vectors, and printing
/// it as one is the same judgement [`fatal`] makes for `#DF`.
extern "C" fn rust_trap_kernel(
    vector: u64,
    code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    base: usize,
) -> ! {
    let vector = vector as u8;
    crate::hal::kernel::force_serial();
    crate::hal::kernel::kprintln!(
        "\nKERNEL PANIC: {} (vector {vector})",
        crate::hal::kernel::trap_name(vector)
    );
    if code != 0 {
        crate::hal::kernel::kprintln!(
            "  code {:#x}: gdt index {} ti {} rpl {}",
            code,
            code >> 3,
            (code >> 2) & 1,
            code & 3
        );
    }
    crate::hal::kernel::kprintln!("  err {code:#x} rip {rip:#x} cs {cs:#x} rflags {rflags:#x}");
    crate::hal::kernel::kprintln!(
        "  rip is {} ({}), cs.rpl = {}",
        if rip < (1 << 47) { "user" } else { "kernel" },
        if rip & 0xfff == 0 {
            "page-aligned"
        } else {
            "not page-aligned"
        },
        cs & 3
    );
    // `base` is the fifteen-register block the stub pushed, so the words above
    // it are the kernel stack this fault landed on - which is what the walk
    // wants, and the same start `fatal` uses for a `#DF`.
    report_frames(base);
    crate::hal::exit_qemu(crate::hal::TEST_EXIT_PANIC)
}

/// A `#BP` taken in **ring 0**: the boot's own probe, not a process's trap.
///
/// `src/board/suites.rs` executes `int3` at CPL 0 and counts on returning with
/// the counter moved, so this path must count and return. The CPU has already
/// pushed the address *after* the instruction, so the stub's epilogue resumes
/// there with no adjustment - which is also why nothing here can fail.
extern "C" fn rust_breakpoint_kernel(rip: u64) {
    BREAKPOINTS_CAUGHT.fetch_add(1, Ordering::Relaxed);
    crate::hal::kernel::kprintln!("caught #BP at rip {:#x}", rip);
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
    crate::hal::kernel::device_not_available();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    fatal("#DF double fault", &frame, code)
}

/// The `#PF` entry, reached through [`page_fault_stub`] rather than the
/// `x86-interrupt` ABI the other exceptions keep.
///
/// A resolved fault (copy-on-write, a demand-paged mapping, a grown stack) just
/// returns and the instruction retries - that is the overwhelming majority of
/// `#PF`s and needs no signal frame. But a fault a task installed a handler for
/// has to enter that handler *at this trap*: Linux delivers it here, and a
/// faulting instruction never reaches a syscall boundary. The `x86-interrupt`
/// prologue hides the fifteen general registers Rust cannot address, so the
/// hand-written stub pushes a [`TrapRegs`] block - the same one [`rust_timer`]
/// hands `deliver_at_trap` - and this function can rewrite it. Relying on the
/// next PIT tick to do the delivery was a measured 2/16 in a guest
/// (`docs/notes/` §6.160); `fault_repeats` killed the other fourteen before a
/// tick arrived. `docs/notes/` §6.161.
///
/// `frame` is that block: fifteen general registers, then the CPU's five-word
/// frame shifted down over the error code (which `iretq` does not pop and the
/// handler consumes). `code` is the raw error-code word passed separately.
extern "C" fn rust_page_fault(frame: *mut TrapRegs, code: u64) {
    let code = PageFaultErrorCode::from_bits_truncate(code);
    // Copy the facts out before any delivery rewrites the block: holding a
    // `&TrapRegs` across `deliver_at_trap` (which writes through a raw pointer)
    // would alias a shared reference with a mutable one.
    let (rip, rsp, cs) = unsafe { ((*frame).rip, (*frame).rsp, (*frame).cs) };
    let addr = Cr2::read().map_or(0, |a| a.as_u64()) as usize;
    // The top of the canonical lower half: everything a process may map.
    let user_address = addr < (1 << 47);

    // A write to a copy-on-write page is resolved whoever made it - ring 3
    // writing its own variable after a `fork`, or ring 0 filling a user buffer
    // through a syscall (`uname`, `getrandom`, `clock_gettime`). Refusing the
    // kernel's half would take the machine down over an ordinary operation.
    let cow = user_address
        && code.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
        && crate::hal::kernel::resolve_cow_fault(addr);
    if cow {
        return; // The faulting instruction retries and succeeds.
    }

    // A mapping made but never touched, an absent stack page, and the machine's
    // own out-of-memory answer. The order against COW above and stack growth
    // below is the reasoning: a present COW page is handled first, and a stack
    // fault is a growth the stack's bounds decide, over a region with no VMA.
    // A demand fault is not gated on being a write - a read of an address never
    // written is exactly what demand paging is for; the VMA's permissions still
    // refuse a `PROT_NONE` read here. Linux's measured OOM answer is the OOM
    // killer, not a `SIGSEGV` (`docs/notes/` §8.0).
    if user_address {
        match crate::hal::kernel::resolve_anon_fault(
            addr,
            code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        ) {
            crate::hal::kernel::FaultAnswer::Resolved => return,
            crate::hal::kernel::FaultAnswer::Refused => {}
            crate::hal::kernel::FaultAnswer::OutOfMemory => crate::hal::kernel::out_of_memory(),
        }
    }
    if user_address && crate::hal::kernel::resolve_stack_growth(addr) {
        return; // The page is there now; the instruction retries.
    }

    // Anything else at a user address is the process's own fault to bear. The
    // `rip` test keeps a ring-0 dereference of a bad user pointer (a kernel bug)
    // out of the signal path and in the panic below.
    if rip < (1 << 47) {
        crate::hal::kernel::report_user_fault(addr, rip, rsp, code.bits() as usize);
        pf_diag(addr, rip, rsp);
        // A push past the mapped end is still an ordinary stack growth, moved
        // here so its answer prints with the rest of what the tables say.
        if crate::hal::kernel::resolve_stack_growth(addr) {
            return;
        }
        // The fault's own record: `si_addr` is `CR2`, and `si_code` is the
        // **mapping's** answer - `SEGV_ACCERR` when a VMA describes the address
        // and refused the access, `SEGV_MAPERR` when nothing describes it. The
        // error word's `P` bit is a different question and answers wrong for
        // the most common self-repair case, a `PROT_NONE` page
        // (`docs/notes/` §6.162). `send_signal` would put a pid in those eight
        // bytes instead (`docs/notes/` §6.160).
        let sig =
            crate::hal::kernel::send_fault_signal(crate::hal::kernel::FaultKind::Page, addr, rip);
        if crate::hal::kernel::disposition_is_default(sig) {
            crate::hal::kernel::die_of_fatal_signal(sig);
        }
        // Hand the record over *here*. The block has the whole register file,
        // so `deliver_at_trap` rewrites it and this exception's `iretq` enters
        // the handler - not a later timer tick. A traced task (or a build with
        // no signal-frame support) declines the door and keeps the record
        // pending for the syscall/timer paths; `fault_repeats` is the backstop
        // that ends a task stuck re-faulting at one instruction.
        #[cfg(mpos_signal_frame)]
        {
            if crate::hal::kernel::deliver_at_trap(frame as *mut ()) {
                return;
            }
        }
        if fault_repeats(rip) {
            crate::hal::kernel::kprintln!(
                "task {} died of SIGSEGV it could not be handed: {addr:#x} ({code:?}) at {rip:#x}, \
                 the same fault {FAULT_REPEAT_LIMIT} times over, and a handler is installed",
                crate::hal::kernel::current()
            );
            crate::hal::kernel::die_of_fatal_signal(sig);
        }
        crate::hal::kernel::preempt();
        return;
    }

    // A kernel `#PF` is a bug, not a signal. A ring-0 trap's block carries the
    // kernel's own `rsp`, not a user stack pointer, so it is no use as a walk
    // start - walk from the frame's own address, which is the kernel stack the
    // fault came from. The report is the whole point: on a `--mode mirror`
    // boot the console is the framebuffer, so force it back to the wire before
    // the machine stops.
    let kernel_stack = frame as usize + 160;
    crate::hal::kernel::force_serial();
    crate::hal::kernel::kprintln!(
        "\nKERNEL PANIC: #PF at {:#x} ({:?}) rip {:#x} cs {:#x} rsp {:#x}",
        addr,
        code,
        rip,
        cs,
        kernel_stack
    );
    report_frames(kernel_stack);
    crate::hal::exit_qemu(crate::hal::TEST_EXIT_PANIC)
}

core::arch::global_asm!(
    ".section .text.page_fault_stub, \"ax\"",
    ".globl page_fault_stub",
    ".p2align 4",
    "page_fault_stub:",
    // Unlike the timer vector, `#PF` pushes an error code: at entry the stack
    // is [error code, rip, cs, rflags, rsp, ss]. Push the same fifteen general
    // registers as `timer_stub`, so the block below the CPU frame is identical.
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push rcx",
    "    push rax",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov  rbp, rsp",
    // Save the error code in rax (its user value is already safe in the block)
    // and shift the CPU's five-word frame down eight bytes over it. `iretq`
    // does not pop the error code - the handler consumes it - so removing that
    // word is exactly right, and it makes the block at rbp a contiguous
    // `TrapRegs`: fifteen GPRs then rip/cs/rflags/rsp/ss, with no gap. A
    // forward `rep movsq` is safe because the destination is always one word
    // behind the source, so no unread source is overwritten. `cld` in case a
    // user program left DF set. rsi/rdi/rcx are scratch here; their saved user
    // values are in the block and the epilogue pops them back (or the signal
    // frame's values, if `deliver_at_trap` rewrites them).
    "    mov  rax, [rbp + 120]",
    "    cld",
    "    lea  rsi, [rbp + 128]",
    "    lea  rdi, [rbp + 120]",
    "    mov  ecx, 5",
    "    rep  movsq",
    // arg1 = the `TrapRegs` block, arg2 = the error code.
    "    mov  rdi, rbp",
    "    mov  rsi, rax",
    "    and  rsp, -16",
    "    call {rust_page_fault}",
    "    mov  rsp, rbp",
    "    pop  r15",
    "    pop  r14",
    "    pop  r13",
    "    pop  r12",
    "    pop  rbp",
    "    pop  rbx",
    "    pop  r11",
    "    pop  r10",
    "    pop  r9",
    "    pop  r8",
    "    pop  rax",
    "    pop  rcx",
    "    pop  rdx",
    "    pop  rsi",
    "    pop  rdi",
    // What is left under rsp is the shifted CPU frame with no error-code word.
    // It is five words at every privilege level - in 64-bit mode the CPU pushes
    // `SS:RSP` even for a same-ring trap (§6.164) - so `iretq` pops exactly
    // what it pushed. A signal delivery rewrote rip/rsp/rflags in this frame,
    // so this same instruction enters the handler instead of re-faulting.
    "    iretq",
    // Restore the generic `.text` section so later `global_asm!`
    // fragments that name no section do not inherit ours.
    ".section .text, \"ax\"",
    rust_page_fault = sym rust_page_fault,
);

unsafe extern "C" {
    fn page_fault_stub();
}

core::arch::global_asm!(
    ".section .text.trap_stubs, \"ax\"",
    // Every CPU exception a process can see arrives here first.
    //
    // The `x86-interrupt` ABI hides the fifteen general registers behind a
    // prologue Rust cannot address, which is why a handled trap could not be
    // entered from the trap itself: the signal was queued and then waited for
    // the next PIT tick to be handed over, and `fault_repeats` killed the
    // process long before one arrived (measured in a guest: `int3` and `ud2`
    // sometimes resumed, `div0` never did - it died of signal 8 "it could not
    // be handed"). These entries build the same `TrapRegs` block `timer_stub`
    // and `page_fault_stub` build, so `deliver_at_trap` can rewrite the
    // program's registers and `iretq` enters the handler at the trap
    // (`docs/notes/` §6.161, §6.164).
    //
    // Entry contract: each vector label pushes ONE tag word (the vector number)
    // and jumps here. A vector with no error code pushes a zero first, so both
    // shapes are identical from `trap_common` on:
    //
    //     [tag, code-or-zero, rip, cs, rflags, rsp, ss]
    //
    // One body therefore serves all seven entries, and the shift below deletes
    // the tag/code pair so the block at `rbp` is a contiguous `TrapRegs`.
    ".globl trap_common",
    ".p2align 4",
    "trap_common:",
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push rcx",
    "    push rax",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push rbx",
    "    push rbp",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov  rbp, rsp",
    // Fifteen pushes are 120 bytes: the tag is at +120, the code word below it
    // at +128, and the CPU's frame from +136. `cs` is always at +144 whether or
    // not the privilege change pushed `rsp`/`ss` after it.
    "    mov  r15d, [rbp + 120]",
    "    mov  rdx, [rbp + 128]",
    // Shift the five-word CPU frame down over the tag/code pair, which `iretq`
    // does not pop. This is done at *every* privilege level, because in 64-bit
    // mode the CPU pushes `SS:RSP` even when the trap does not change rings -
    // measured in a guest (`docs/notes/` §6.164): a ring-0 `int3` leaves
    // `[rip, cs, rflags, rsp, ss]` on the stack. Shifting only three words there
    // left `rsp`/`ss` reading the frame's neighbours, so `iretq` took the saved
    // `rflags` for a selector and raised `#GP(0x280)` on the boot's own check.
    //
    // A forward `rep movsq` is safe because the destination is always two words
    // below the source, so no unread source is overwritten. `cld` in case the
    // program left DF set; rsi/rdi/rcx are scratch here and their saved values
    // are in the block.
    "    cld",
    "    lea  rsi, [rbp + 136]",
    "    lea  rdi, [rbp + 120]",
    "    mov  ecx, 5",
    "    rep  movsq",
    // The block at rbp is a contiguous `TrapRegs` now, so `cs` is at +128 and
    // the ring test can be made after the shift for both halves.
    "    mov  rax, [rbp + 128]",
    "    and  eax, 3",
    "    cmp  eax, 3",
    "    jne  3f",
    // arg1 = the `TrapRegs` block, arg2 = vector, arg3 = error code.
    "    mov  rdi, rbp",
    "    mov  rsi, r15",
    "    and  rsp, -16",
    "    call {rust_trap}",
    "    mov  rsp, rbp",
    // Both halves rejoin here: a ring-0 `#BP` counts its breakpoint and returns
    // to the instruction after it (the boot's own check runs `int3` in ring 0
    // and counts on coming back), while every other ring-0 exception has
    // already ended the machine inside `rust_trap_kernel`.
    "6:",
    "    pop  r15",
    "    pop  r14",
    "    pop  r13",
    "    pop  r12",
    "    pop  rbp",
    "    pop  rbx",
    "    pop  r11",
    "    pop  r10",
    "    pop  r9",
    "    pop  r8",
    "    pop  rax",
    "    pop  rcx",
    "    pop  rdx",
    "    pop  rsi",
    "    pop  rdi",
    // What is left under rsp is the shifted CPU frame; `iretq` pops it as far
    // as the CS says to. A delivery rewrote rip/rsp/rflags in it, so this
    // enters the handler rather than re-faulting.
    "    iretq",
    // Ring 0: the vector decides, because one of them must come back.
    //
    // `#BP` is that one: the boot's own check runs `int3` in ring 0 and counts
    // on returning to the instruction after it (`src/board/suites.rs`). The CPU
    // has already pushed the *following* address, so the only work is to count
    // it and unwind - which is why this path keeps its registers and its frame
    // instead of ending the machine.
    //
    // Every other exception taken in ring 0 is a kernel bug: there is no
    // signal to invent and no caller to return to. The report is the whole
    // point and it never returns, so nothing is restored; the frame base goes
    // along so the report can walk the kernel stack it interrupted.
    "3:",
    "    cmp  r15d, 3",
    "    jne  4f",
    // Ring 0 `#BP`: the block is already a `TrapRegs`, the CPU saved the
    // address *after* the `int3` in rip, and the shared epilogue pops it back.
    "    mov  rdi, [rbp + 120]",
    "    and  rsp, -16",
    "    call {rust_breakpoint_kernel}",
    "    mov  rsp, rbp",
    // Backwards: the shared epilogue sits above this ring-0 half, and it is
    // reached by falling through `iretq` only from the ring-3 door.
    "    jmp  6b",
    "4:",
    "    mov  rdi, r15",
    "    mov  rsi, rdx",
    "    mov  rdx, [rbp + 120]",
    "    mov  rcx, [rbp + 128]",
    "    mov  r8, [rbp + 136]",
    "    mov  r9, rbp",
    "    and  rsp, -16",
    "    call {rust_trap_kernel}",
    "    ud2",
    // `#BP` reaches the common body like every other vector now.
    ".globl trap_bp",
    ".p2align 4",
    "trap_bp:",
    "    push 0",
    "    push 3",
    "    jmp  trap_common",
    // `#DB` (vector 1): the single-step trap a tracer arms with `RFLAGS.TF`, and
    // the reason `PTRACE_SINGLESTEP` had no meaning here before (it was answered
    // as `SYSCALL`). No error code; tag 1 so the entry knows which it was.
    ".globl trap_db",
    ".p2align 4",
    "trap_db:",
    "    push 0",
    "    push 1",
    "    jmp  trap_common",
    ".globl trap_de",
    ".p2align 4",
    "trap_de:",
    "    push 0",
    "    push 0",
    "    jmp  trap_common",
    ".globl trap_ud",
    ".p2align 4",
    "trap_ud:",
    "    push 0",
    "    push 6",
    "    jmp  trap_common",
    ".globl trap_np",
    ".p2align 4",
    "trap_np:",
    "    push 11",
    "    jmp  trap_common",
    ".globl trap_ss",
    ".p2align 4",
    "trap_ss:",
    "    push 12",
    "    jmp  trap_common",
    ".globl trap_gp",
    ".p2align 4",
    "trap_gp:",
    "    push 13",
    "    jmp  trap_common",
    ".globl trap_ac",
    ".p2align 4",
    "trap_ac:",
    "    push 17",
    "    jmp  trap_common",
    // Restore the generic `.text` section so later fragments that name no
    // section do not inherit ours.
    ".section .text, \"ax\"",
    rust_trap = sym rust_trap,
    rust_trap_kernel = sym rust_trap_kernel,
    rust_breakpoint_kernel = sym rust_breakpoint_kernel,
);

unsafe extern "C" {
    fn trap_bp();
    fn trap_db();
    fn trap_de();
    fn trap_ud();
    fn trap_np();
    fn trap_ss();
    fn trap_gp();
    fn trap_ac();
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
    crate::hal::kernel::kprintln!("  return addresses on the fault's kernel stack:");
    // Bounded by the stack the task was actually given: the walk reads memory,
    // and the only memory it may read without asking anybody is the block the
    // scheduler allocated for this task's own kernel stack.
    let Some((base, top)) = crate::hal::kernel::current_stack_range() else {
        crate::hal::kernel::kprintln!("    (no kernel stack to read)");
        return;
    };
    if rsp < base || rsp >= top {
        crate::hal::kernel::kprintln!(
            "    (rsp {rsp:#x} is outside this task's stack {base:#x}..{top:#x})"
        );
        return;
    }
    let room = ((top - rsp) / 8).min(FRAME_SCAN_WORDS);
    let mut printed = 0usize;
    for i in 0..room {
        let value = unsafe { core::ptr::read_unaligned((rsp + i * 8) as *const u64) };
        if (KERNEL_TEXT..KERNEL_TEXT_END).contains(&value) {
            crate::hal::kernel::kprintln!("    rsp+{:#05x} {:#018x}", i * 8, value);
            printed += 1;
            if printed >= 16 {
                break;
            }
        }
    }
    if printed == 0 {
        crate::hal::kernel::kprintln!("    (none: the fault took the stack with it)");
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

    let task = crate::hal::kernel::current();
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
/// The bytes at `rip`, for a fault report: what the instruction that faulted
/// actually is, up to the end of its page.
///
/// Raw bytes rather than a disassembly, for the reason the report prints
/// addresses: a page fault is the worst moment to go looking for a decoder. A
/// `rep movs`/`rep stos`, a store into a computed address and a `push` are told
/// apart at a glance, and *what was the program doing* is the question a fault
/// report is asked - the address alone answers it only when it is a stack
/// pointer.
///
/// Empty when the page holding `rip` is itself absent: the read is performed by
/// the faulting machine, and a user `rip` is no exception to the rule that this
/// kernel may not dereference one it has not checked.
fn code_at(rip: usize) -> ([u8; 16], usize) {
    let room = (PAGE_SIZE - (rip & (PAGE_SIZE - 1))).min(16);
    let mut code = [0u8; 16];
    if super::paging::leaf_pte(rip & !(PAGE_SIZE - 1)).is_some() {
        let bytes = unsafe { core::slice::from_raw_parts(rip as *const u8, room) };
        code[..room].copy_from_slice(bytes);
        (code, room)
    } else {
        (code, 0)
    }
}

fn pf_diag(addr: usize, rip: usize, rsp: usize) {
    use core::sync::atomic::AtomicUsize;

    // A handful per boot: a task that faults in a loop would otherwise bury
    // every other thing the machine is saying, which is exactly the failure
    // mode `fault_repeats` exists to cut short.
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    if SEEN.fetch_add(1, Ordering::Relaxed) >= 4 {
        return;
    }

    let page_size = mpos_vm::PAGE_SIZE;
    let page = addr & !(page_size - 1);
    // P, W, U, and bit 9 (the COW marker this kernel sets instead of clearing
    // the write bit, if it does).
    let bits = |e: u64| -> u64 {
        (e & 1) | (((e >> 1) & 1) << 1) | (((e >> 2) & 1) << 2) | (((e >> 9) & 1) << 3)
    };

    let Some(entry) = super::paging::leaf_pte(page) else {
        // The bytes of the instruction that faulted, up to the end of its own
        // page: a faulting access is usually one instruction whose operand
        // went bad, and the bytes name it - `48 89` is a store, `f4` is
        // musl's `a_crash`, an `0f` prefix says which SSE form it is.
        //
        // Read **only when that page is mapped**, because on an
        // instruction-fetch fault it is not: `rip` is then the faulting
        // address itself, and a read here faults again from ring 0 - same
        // `CR2`, the error code's U/S bit now clear - so the machine dies of a
        // `KERNEL PANIC: #PF` while it is trying to describe a *user* fault.
        // Measured: `strace -f niri` jumped to a heap address that had been
        // freed, the user fetch fault was taken, and this diagnostic turned it
        // into a kernel panic. When the page is absent there are no bytes to
        // print, and `rip` beside an empty list says exactly that.
        let (code, code_len) = code_at(rip);
        // And how far the *mapped* pages reach around the hole. A dereference
        // of a pointer that went bad lands a page or two from something real,
        // and the run says so; an access into a mapping the kernel never
        // filled in lands at the bottom of a long run of absent pages, and
        // only the length of that run tells the two apart. Bounded, because
        // the answer is a hint and the handler must not walk the address
        // space looking for it.
        let mut lo = page;
        for _ in 0..64 {
            if lo >= page_size && super::paging::leaf_pte(lo - page_size).is_some() {
                lo -= page_size;
            } else {
                break;
            }
        }
        let mut hi = page + page_size;
        for _ in 0..64 {
            if hi < (1 << 47) && super::paging::leaf_pte(hi).is_some() {
                hi += page_size;
            } else {
                break;
            }
        }
        // The run is printed as a range of *mapped* pages, so an empty answer
        // has to say so: with neither neighbour mapped, `lo`/`hi` still bracket
        // the faulting page, and `mapped=[0x73650000..0x73651000)` then reads as
        // "this page is mapped and its neighbours are not" - the opposite of
        // what was measured. A panic beside that range was read as a one-page
        // hole in a live mapping when in fact nothing around it was mapped at
        // all, which is a userland pointer into memory that is gone.
        let empty = lo == page && hi == page + page_size;
        crate::hal::kernel::kprintln!(
            "PFDIAG page={page:#x} NOT MAPPED task={:?} rip={rip:#x} mapped={} bytes={:02x?}{}",
            crate::hal::kernel::current(),
            if empty {
                alloc::string::String::from("none around the page")
            } else {
                alloc::format!("[{lo:#x}..{hi:#x})")
            },
            &code[..code_len],
            if code_len == 0 {
                " (empty: the page holding rip is the absent one)"
            } else {
                ""
            }
        );
        // And the one fact that decides between the two stories an absent page
        // tells: a page whose address the process never mapped is the process's
        // own wild pointer, and a page inside a mapping the process *does* hold
        // is a page this kernel failed to bring in when something asked for it.
        // The present-page branch has always printed the VMA; the absent branch
        // printing only the hole left exactly that question unanswered, and it
        // is the question a real report was read for (udevd's `at 0x73650010`
        // beside `mapped=[0x73650000..0x73651000)` - one page wide, or the whole
        // surrounding region gone?).
        //
        // Both accessors only read the VMA table; the name is the one thing here
        // that allocates, and it is what the present branch prints too, at the
        // same four-per-boot budget.
        match crate::hal::kernel::vma_prot_at(page) {
            Some(prot) => crate::hal::kernel::kprintln!(
                "PFDIAG vma={:#x} ({}{}{}) kind={} - the page is inside a mapping, so nothing brought it in",
                prot.bits(),
                if prot.contains(mpos_vm::MapFlags::READ) {
                    "r"
                } else {
                    "-"
                },
                if prot.contains(mpos_vm::MapFlags::WRITE) {
                    "w"
                } else {
                    "-"
                },
                if prot.contains(mpos_vm::MapFlags::EXEC) {
                    "x"
                } else {
                    "-"
                },
                crate::hal::kernel::vma_kind_at(page).unwrap_or("?"),
            ),
            None => crate::hal::kernel::kprintln!(
                "PFDIAG vma=none - no mapping of this process covers the page"
            ),
        }
        if let Some(name) = crate::hal::kernel::mapping_name_at(page) {
            crate::hal::kernel::kprintln!("PFDIAG fault_in={name}");
        }
        return;
    };

    let want = bits(entry);
    let mut lo = page;
    while lo >= page_size
        && super::paging::leaf_pte(lo - page_size).is_some_and(|e| bits(e) == want)
    {
        lo -= page_size;
    }
    let mut hi = page + page_size;
    while hi < (1 << 47) && super::paging::leaf_pte(hi).is_some_and(|e| bits(e) == want) {
        hi += page_size;
    }

    crate::hal::kernel::kprintln!(
        "PFDIAG page={page:#x} pte={entry:#x} P={} W={} U={} B9={} phys={:#x} run=[{lo:#x}..{hi:#x}) \
         task={:?} rip={rip:#x} rsp={rsp:#x} below_sp={:#x} op={:02x?}",
        entry & 1,
        (entry >> 1) & 1,
        (entry >> 2) & 1,
        (entry >> 9) & 1,
        entry & 0x000f_ffff_ffff_f000,
        crate::hal::kernel::current(),
        rsp.saturating_sub(page),
        &code_at(rip).0[..code_at(rip).1]
    );

    // And what the *process* asked for, which the page tables do not say. A
    // present, user, read-only page is two different stories apart: a writable
    // mapping whose page this kernel left read-only behind the process's back
    // is a defect here, and a read-only mapping is the process writing where it
    // may not, which Linux answers the same way.
    match crate::hal::kernel::vma_prot_at(page) {
        Some(prot) => crate::hal::kernel::kprintln!(
            "PFDIAG vma={:#x} ({}{}{}{}) kind={}",
            prot.bits(),
            if prot.contains(mpos_vm::MapFlags::READ) {
                "r"
            } else {
                "-"
            },
            if prot.contains(mpos_vm::MapFlags::WRITE) {
                "w"
            } else {
                "-"
            },
            if prot.contains(mpos_vm::MapFlags::EXEC) {
                "x"
            } else {
                "-"
            },
            if prot.contains(mpos_vm::MapFlags::USER) {
                "u"
            } else {
                "-"
            },
            crate::hal::kernel::vma_kind_at(page).unwrap_or("?")
        ),
        None => crate::hal::kernel::kprintln!(
            "PFDIAG vma=NONE (no mapping describes {page:#x}: the image, the stack or a hole)"
        ),
    }

    // And *who* is writing, and *where*: the name of the mapping each address
    // belongs to. A `rip` inside a shared object names the library, and the
    // faulting page named `[stack]` or a file says which region the write was
    // aimed at - which is the question this report could not answer before.
    for (what, at) in [("fault_in", page), ("rip_in", rip)] {
        crate::hal::kernel::kprintln!(
            "PFDIAG {what}={:#x} {}",
            at,
            match crate::hal::kernel::mapping_name_at(at) {
                Some(name) => name,
                None => alloc::string::String::from("(no mapping)"),
            }
        );
    }

    // Then the whole walk, not just the leaf. The leaf's own bits are a true
    // statement about the leaf and not about the access: the U/S bit is ANDed
    // along the walk, and a write on a 4 KiB page can be refused by a level
    // above it. Every level is printed, unused slots included, so the shape of
    // the walk is unambiguous.
    for (level, e) in super::paging::entry_chain(page) {
        crate::hal::kernel::kprintln!(
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
/// matching `add $0x10`. Verified against the disassembly of the handlers that
/// still use the ABI: `breakpoint` is 9 pushes, `sub $0x30`, epilogue
/// `add $0x30` + 9 pops with no extra skip, while an error-code sibling is one
/// push and `add $0x10` wider. `#PF` no longer belongs to that set: its entry
/// is [`page_fault_stub`], which consumes the error code itself and leaves a
/// contiguous [`TrapRegs`] block, because a handled fault has to be handed
/// over at the trap (§2.13, §6.161).
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
    // reach the host: on a `--mode mirror` boot the console lives in the
    // framebuffer, and a machine that dies with its report painted into a
    // window nobody is looking at leaves the reader with a vanished QEMU and
    // no explanation. Same move as the panic handler in `main`.
    crate::hal::kernel::force_serial();
    crate::hal::kernel::kprintln!("\nKERNEL PANIC: {}", name);
    // A non-zero error code on a `#GP` *is* a selector, and a selector is only
    // worth printing if it is named: index in the GDT, and both privilege bits.
    if code != 0 {
        crate::hal::kernel::kprintln!(
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
    crate::hal::kernel::kprintln!(
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
        crate::hal::kernel::kprintln!("  rsp {:#x} ss {:#x}", word(3), word(4));
    } else {
        crate::hal::kernel::kprintln!(
            "  (ring 0: no privilege change, so word3/word4 are stack litter) w3 {:#x} w4 {:#x}",
            word(3),
            word(4)
        );
    }
    // The faulting RIP's role, so the two cases a `#GP` usually is - a bad
    // ring-3 return and a bad `sysret` - can be told apart at a glance.
    let rip = frame.instruction_pointer.as_u64() as usize;
    crate::hal::kernel::kprintln!(
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
    crate::hal::exit_qemu(crate::hal::TEST_EXIT_PANIC)
}

/// The bytes of an instruction fit in a 16-byte window, which is also the
/// longest x86-64 instruction: reading up to the end of the faulting page is
/// what keeps the report from faulting itself.
const PAGE_SIZE: usize = 4096;

/// Which task last got a full trap report, so a process that keeps faulting
/// prints once rather than thousands of times.
///
/// The subject is the **task**, not the machine. A process that returns from
/// its own `SIGSEGV` handler faults again on the very next instruction, so one
/// full report per task is what a reader needs - but a single flag for the whole
/// boot is too coarse in the one case this report exists for: a gate runs 338
/// programs, and the process whose death is interesting is usually not the first
/// one to trap. Measured need: two `sh[1]` general-protection deaths
/// (`docs/notes/16-init-boot-race.md`) went unreported because an earlier
/// `int3` in a different program had already consumed the flag.
///
/// Task ids are slots and a slot is reused, so this can suppress a *later*
/// crash that lands on the same slot. That is the right trade: the alternative
/// loses the report of the crash that matters, and a reused slot producing no
/// second dump is bounded by how many tasks a boot makes.
static TRAP_REPORTED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Whether this task should print the full report, latching it if so.
fn claim_trap_report() -> bool {
    let me = crate::hal::kernel::current();
    TRAP_REPORTED.swap(me, Ordering::Relaxed) != me
}
