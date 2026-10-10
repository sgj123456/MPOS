//! The wiring layer's own suites: what only a *booted* machine can answer, and
//! the read-back of what the boot's device step registered.
//!
//! Test builds only: the module is declared behind `--features selftest`
//! (`src/board/mod.rs`) and listed in the one place that names suites
//! ([`super::kunit`]), so a kernel that ships carries none of this (`NOTES.md`
//! §15.19). Nothing here is a boot-time decision either - a case *answers*
//! (`Result<(), String>`) rather than asserting, because `panic = "abort"` means
//! an `assert!` would end the machine and hide every case after it, and the
//! harness prints one line per case.
//!
//! Two kinds of case live here, and the difference is why this file exists rather
//! than a suite beside each module:
//!
//! * **The machine's own** - the x86_64 register file and page tables, the
//!   aarch64 PL011's polled receiver and interrupt-driven transmitter. A case
//!   that drives *this* machine's UART or reads *this* machine's register-order
//!   table is this machine's, in the way nothing in the boot flow is: the flow
//!   runs identically on both (`src/board/flow.rs`), and what is left that only
//!   one machine can answer is its evidence. The machine is named here, and this
//!   is the one place in the wiring where that is true.
//! * **The devices suite** ([`SUITE`]) - one set of read-backs over what
//!   `drivers::virtio::probe` registered, on either machine. It is here rather
//!   than beside the probe because its subject is the *boot's* device step: the
//!   probe finds a device and hands it to the subsystem that owns it, and what
//!   this asks is whether the subsystem really has it - the same split
//!   `report::SUITE` makes for the boot stack.

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use mpos::boot::BootInfo;
use mpos::case_check;
use mpos::hal;

#[cfg(target_arch = "x86_64")]
use core::ptr;
#[cfg(target_arch = "x86_64")]
use mpos::mm;
#[cfg(target_arch = "x86_64")]
use mpos::mm::{MapFlags, Mapping, PAGE_SIZE};
#[cfg(target_arch = "x86_64")]
use mpos::println;
#[cfg(target_arch = "x86_64")]
use mpos::{arch, syscall};
#[cfg(target_arch = "aarch64")]
use mpos::{sched, vfs};

use super::kunit::{Case, Phase, Suite};

// ---------------------------------------------------------------------------
// The devices suite: what the boot's own device step registered.
// ---------------------------------------------------------------------------

/// The device read-backs: one case per class the probe opens.
///
/// What it is *for*. The probe is the boot's only device step, and its contract is
/// "find what the machine describes, register it where its subsystem looks for
/// it, and say so" - a contract a boot log line cannot pin, because a line prints
/// whether or not the registration behind it happened. Each case below asks the
/// subsystem, which is the half that would silently be empty: the entropy source
/// the seed and `/dev/hwrng` read, the disk the block layer holds, the adapter the
/// stack was handed, the evdev nodes, the console port, the sound card, the host
/// share's tag, the display whose task is running. A probe that stopped running,
/// or ran and registered nothing, fails here on both machines.
///
/// What it is *not*. These are not the *drivers'* tests: whether an ext4 mounts
/// off the disk, whether a socket moves bytes, whether a key press reaches a
/// program are claims about the subsystems above, and they are pinned by the
/// sweep's programs - which run on both machines, unlike the milestone checks this
/// replaced. What is deliberately absent is the class whose attachment depends on
/// the *host* (vsock needs `/dev/vhost-vsock` to exist before QEMU can offer it),
/// because a case that must pass could only pass by not being asked.
pub const SUITE: Suite = Suite {
    name: "devices",
    phase: Phase::Done,
    cases: &[
        Case {
            name: "entropy_source",
            run: entropy_source,
        },
        Case {
            name: "boot_disk",
            run: boot_disk,
        },
        Case {
            name: "network_adapter",
            run: network_adapter,
        },
        Case {
            name: "input_nodes",
            run: input_nodes,
        },
        Case {
            name: "console_port",
            run: console_port,
        },
        Case {
            name: "sound_card",
            run: sound_card,
        },
        Case {
            name: "host_share",
            run: host_share,
        },
        Case {
            name: "display",
            run: display,
        },
    ],
};

/// The machine's rng device answered the probe, and it is not bytes of nothing.
fn entropy_source(_info: &BootInfo) -> Result<(), String> {
    let bytes = mpos::drivers::virtio::entropy()
        .ok_or("the probe registered no entropy source on a machine that attached one")?;
    case_check!(
        bytes.iter().any(|&byte| byte != 0),
        "the entropy source answered 32 zero bytes"
    );
    Ok(())
}

/// The boot disk is registered with the block layer, with the geometry it reports.
///
/// What it reads is not asked here: `read_at` has no answer to give a caller - a
/// device that fails panics inside the driver - and the payload's own filesystem
/// programs are the ones that read through this disk on both machines.
fn boot_disk(_info: &BootInfo) -> Result<(), String> {
    let disk = mpos::blk::raw_disk().ok_or("the probe registered no block device")?;
    let sectors = disk.capacity_sectors();
    case_check!(sectors > 0, "the boot disk reports {sectors} sectors");
    Ok(())
}

/// The adapter the probe registered is a device with an address of its own.
///
/// The stack over it is the boot's next step (`steps::bring_up_net`) and its
/// liveness is the sweep's: a `socket` and a `connect` need it, and the socket
/// programs run on both machines.
fn network_adapter(_info: &BootInfo) -> Result<(), String> {
    let dev = mpos::drivers::virtio::net::device()
        .ok_or("the probe registered no network adapter on a machine that attached one")?;
    case_check!(
        dev.mac().iter().any(|&byte| byte != 0),
        "the adapter reports a zero MAC, which a configured device never has"
    );
    Ok(())
}

/// The evdev registry holds at least one node - the machine's own keyboard or
/// pointer, published as `/dev/input/eventN`.
#[cfg(mpos_input_registry)]
fn input_nodes(_info: &BootInfo) -> Result<(), String> {
    let nodes = mpos::drivers::virtio::input::nodes();
    case_check!(
        !nodes.is_empty(),
        "the evdev registry is empty; /dev/input would have no nodes"
    );
    Ok(())
}

/// A machine with no input registry has no nodes to ask about, and the class is
/// absent rather than empty: the case does not exist there either.
#[cfg(not(mpos_input_registry))]
fn input_nodes(_info: &BootInfo) -> Result<(), String> {
    Ok(())
}

/// The host-driven console port is installed.
fn console_port(_info: &BootInfo) -> Result<(), String> {
    let port = mpos::drivers::virtio::console::global()
        .ok_or("the probe registered no console port on a machine that attached one")?;
    let _ = port.size();
    Ok(())
}

/// The sound card is installed and reports a stream it can run.
fn sound_card(_info: &BootInfo) -> Result<(), String> {
    let card = mpos::drivers::virtio::sound::global()
        .ok_or("the probe registered no sound card on a machine that attached one")?;
    let (rate, channels) = card.format();
    case_check!(
        rate > 0 && channels > 0,
        "the card reports {rate} Hz and {channels} channels"
    );
    Ok(())
}

/// The host's share is in the registry `mount -t 9p` reads, under its own tag.
fn host_share(_info: &BootInfo) -> Result<(), String> {
    let tags: Vec<String> = mpos::drivers::virtio::fs9p::devices()
        .iter()
        .map(|dev| dev.mount_tag().to_string())
        .collect();
    case_check!(
        !tags.is_empty(),
        "the 9p registry is empty; mount -t 9p would find no share"
    );
    Ok(())
}

/// The display the probe opened is driven by the task that owns it.
fn display(_info: &BootInfo) -> Result<(), String> {
    let view = mpos::drivers::virtio::gpu::global()
        .ok_or("the probe registered no display on a machine that attached one")?;
    let (width, height) = view.dims();
    case_check!(
        width > 0 && height > 0,
        "the display reports {width}x{height}"
    );
    case_check!(
        mpos::drivers::video::render::running(),
        "no render task is running for the registered display"
    );
    Ok(())
}

// ---- arch: interrupts, timer, paging, address spaces ---------------------

/// The arch suite's first case: the clock ticks at the rate it declares, a
/// breakpoint is catchable, and the machine's register-file table agrees with the
/// host-built ABI table.
///
/// Compiled only into a test build: it costs half a second of busy-waiting, and
/// nothing outside the suite registry calls it.
#[cfg(target_arch = "x86_64")]
pub(crate) fn check_arch_clock(_info: &BootInfo) -> Result<(), String> {
    // The clock, and a breakpoint exception.
    {
        let before = hal::ticks();
        hal::busy_wait_ms(500);
        let elapsed = hal::ticks() - before;
        println!("clock: {} ticks in ~500ms ({} Hz)", elapsed, hal::TICK_HZ);
        // Expect half a second's worth of ticks, within ten percent. Written
        // against TICK_HZ, not a literal, so raising the tick rate does not
        // silently turn a working clock into a failed assertion.
        let half = hal::TICK_HZ as u64 / 2;
        let tol = hal::TICK_HZ as u64 / 10;
        case_check!(
            (half - tol..=half + tol).contains(&elapsed),
            "the clock ticked {elapsed} times in ~500 ms, not the {half}±{tol} that {} Hz promises",
            hal::TICK_HZ
        );
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("int3")
        };
        case_check!(
            arch::breakpoints_caught() == 1,
            "#BP was not caught: {} breakpoints recorded",
            arch::breakpoints_caught()
        );
        println!("breakpoint caught and returned from");

        // The machine's copy of the Linux register-file field order must agree
        // with the ABI's. The two tables are duplicated on purpose - one side is
        // bare-metal only, the other is host-built, so neither can name the
        // other's module - and this is what keeps the duplication honest rather
        // than hoped-for (`arch/x86_64/context.rs`, `syscall/linux/abi.rs`).
        for (name, index) in arch::ABI_REG_INDEX {
            case_check!(
                syscall::linux::abi::uregs_by_name(name) == Some(index),
                "the machine puts {name} at {index} and user_regs_struct puts it at {:?}",
                syscall::linux::abi::uregs_by_name(name)
            );
        }
        case_check!(
            syscall::linux::abi::UREGS_COUNT == arch::ABI_REG_COUNT,
            "the register file is {} fields on one side and {} on the other",
            syscall::linux::abi::UREGS_COUNT,
            arch::ABI_REG_COUNT
        );
        println!("register file field order agrees with the ABI");
    }
    Ok(())
}

/// The arch suite's second case: the kernel runs on its own page tables with the
/// permissions it meant, the direct map is usable, and processes get isolated
/// address spaces.
///
/// Compiled only into a test build, like the case above.
#[cfg(target_arch = "x86_64")]
pub(crate) fn check_arch_pages(info: &BootInfo) -> Result<(), String> {
    // The kernel's own page tables, W^X, and user address spaces.
    {
        let sections = arch::kernel_sections();

        // Direct map works after the switch.
        let mut proof: Vec<u8> = Vec::new();
        proof.resize(64 * 1024, 0x3c);
        case_check!(
            proof.iter().all(|&byte| byte == 0x3c),
            "the direct map is broken after the switch: a 64 KiB buffer did not read back"
        );
        println!("64 KiB allocated and written after the switch");

        // W^X across the whole image.
        let image_offset = info.kernel_virt_base - info.kernel_phys_base;
        for (name, range, expect) in [
            ("text", &sections.text, MapFlags::READ | MapFlags::EXEC),
            ("rodata", &sections.rodata, MapFlags::READ),
            ("data", &sections.data, MapFlags::READ | MapFlags::WRITE),
        ] {
            let mut virt = range.start;
            while virt < range.end {
                let (phys, flags) = hal::translate(virt)
                    .ok_or_else(|| alloc::format!("{name} at {virt:#x} is not mapped"))?;
                case_check!(
                    flags == expect,
                    "{name} at {virt:#x} is {flags:?}, not the {expect:?} it was mapped with"
                );
                case_check!(
                    virt - phys == image_offset,
                    "{name} at {virt:#x} points at offset {:#x}, not the image's {image_offset:#x}",
                    virt - phys
                );
                virt += PAGE_SIZE;
            }
        }
        println!("text r-x, rodata r--, data rw- across the whole image");

        // Direct map is writable; null page is unmapped.
        let frame = mm::alloc_frames(0).expect("out of frames");
        let (back, flags) = hal::translate(info.phys_to_virt(frame))
            .expect("the direct map is missing a frame the kernel owns");
        case_check!(
            back == frame,
            "the direct map translates frame {frame:#x} to {back:#x}"
        );
        case_check!(
            flags == MapFlags::READ | MapFlags::WRITE,
            "the direct map is {flags:?}, not read-write"
        );
        mm::free_frames(frame, 0);

        case_check!(hal::translate(0).is_none(), "the null page is mapped");
        println!("direct map writable and non-executable, null page unmapped");

        // Kernel stack is not reclaimable.
        let probe = 0u8;
        let stack = mm::virt_to_phys(&raw const probe as usize);
        let region = info
            .regions()
            .iter()
            .find(|r| r.base <= stack && stack < r.end())
            .ok_or_else(|| alloc::format!("the stack at {stack:#x} is in no memory-map region"))?;
        case_check!(
            !region.kind.is_allocatable(),
            "the kernel stack sits in {region:?}, which the frame allocator hands out"
        );
        println!("kernel stack at {stack:#x} in {region:?}, not reclaimable yet");

        // User address spaces: isolated lower halves, shared kernel half.
        let virt = 0x4000_0000;
        let frame = mm::alloc_frames(0).expect("out of frames");

        let before_mapped = mm::frame_stats().free;
        let mut space = hal::AddressSpace::new();
        space.map(Mapping {
            virt,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        let mapped_cost = before_mapped - mm::frame_stats().free;

        let before_empty = mm::frame_stats().free;
        let empty = hal::AddressSpace::new();
        let empty_cost = before_empty - mm::frame_stats().free;

        case_check!(
            mapped_cost == 4,
            "a single user page needed {mapped_cost} tables"
        );
        case_check!(
            empty_cost == 1,
            "an empty address space needed {empty_cost} tables"
        );

        // The two switches below move CR3 behind the scheduler's back: the
        // boot task has no `space` of its own, so a clock tick landing inside
        // this block would send the dispatcher's `prepare` to switch straight
        // back to the kernel root - and the assert would see a half-done
        // switch, which it did, one boot in ten. Nothing in here sleeps, so
        // holding interrupts off for the block is just the truth about what
        // it needs. exec_replace has the same shape and is already safe: it
        // records the task's new space before pointing CR3 at it.
        //
        // The answer comes *out* of the block rather than returning from inside
        // it, and the kernel's own tables come back on every path: a case that
        // left CR3 on a process's space would take the rest of the boot down with
        // it, which is the mistake §13.14/§13.15 are the record of.
        let in_user_space = critical_section::with(|_| -> Result<(), String> {
            space.switch();
            case_check!(
                hal::active_root() == space.root(),
                "switch() did not switch: the active root is {:#x}, not {:#x}",
                hal::active_root(),
                space.root()
            );

            let (back, flags) = hal::translate(virt).expect("the user page is not mapped");
            case_check!(
                back == frame,
                "the user page points at {back:#x}, not {frame:#x}"
            );
            case_check!(
                flags == MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                "the user page is {flags:?}, not read-write-user"
            );
            unsafe {
                ptr::write_bytes(virt as *mut u8, 0x77, PAGE_SIZE);
                case_check!(
                    *(virt as *const u8) == 0x77 && *((virt + PAGE_SIZE - 1) as *const u8) == 0x77,
                    "a write through the user page did not land at both ends of it"
                );
            }
            println!("user page at {virt:#x} mapped rwu and written through");

            let mut shared: Vec<u8> = Vec::new();
            shared.resize(8 * 1024, 0x11);
            case_check!(
                shared.iter().all(|&byte| byte == 0x11),
                "the heap stopped working in a process address space"
            );
            drop(shared);

            empty.switch();
            case_check!(
                hal::translate(virt).is_none(),
                "a user page leaked between address spaces"
            );
            let text = arch::kernel_sections().text.start;
            case_check!(
                hal::translate(text).is_some(),
                "the kernel half was not shared with the new address space"
            );
            println!("second address space: kernel half shared, user half empty");

            hal::switch_to_kernel();
            Ok(())
        });
        case_check!(
            hal::active_root() == hal::kernel_root(),
            "the machine came out of the user space on {:#x}, not the kernel's own root {:#x}",
            hal::active_root(),
            hal::kernel_root()
        );
        in_user_space?;

        let before_teardown = mm::frame_stats().free;
        drop(empty);
        drop(space);
        case_check!(
            mm::frame_stats().free == before_teardown + mapped_cost + empty_cost,
            "an address space did not give back every table it took: {} frames came back, {} went out",
            mm::frame_stats().free - before_teardown,
            mapped_cost + empty_cost
        );
        mm::free_frames(frame, 0);
        println!(
            "both address spaces torn down, {} frames returned",
            mapped_cost + empty_cost
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The aarch64 machine's own: the PL011 as this machine drives it.
// ---------------------------------------------------------------------------

/// The interrupt-driven console: the machine queues what it writes and the
/// UART's own interrupt hands it to the wire, rather than the writer waiting for
/// the transmitter with interrupts masked.
///
/// The queue is not this machine's - it is `drivers::tx_ring`, which the first
/// machine's 16550 drains through the same three operations this machine's PL011
/// implements - so what is being pinned is narrower and more useful than "output
/// appears": it is that *this* machine's transmitter answer reaches it, that the
/// GIC delivers its line, and that a writer which offered more than one burst
/// was parked and woken rather than spinning. The last of those is why the check
/// asks for more than a line: [`check_console_rx`] covered a byte, and a queue
/// that only ever held one line's worth would pass a test that small.
///
/// How it knows the difference. `stats()` counts what writers queued and what the
/// *handler* handed over, and the second number can only rise inside an
/// interrupt: a polled write moves no bytes through the queue at all, so a
/// machine whose line never fired reports a queue that nothing drains. It is
/// also the check that catches a transmitter asked to interrupt when it should
/// not have been - an idle line asserting for ever would have shown up here as
/// visits that moved nothing, and before there was a counter to ask, as nothing
/// at all.
#[cfg(target_arch = "aarch64")]
pub(crate) fn check_serial_tx(_info: &BootInfo) -> Result<(), String> {
    const LINE: usize = 512;

    let before = hal::tx_ring::stats();
    // The write goes through `crate::console` rather than straight to the wire,
    // because that is the path every line above the machine layer takes - and
    // because its parking half only engages for a caller that may sleep, which
    // this task may.
    let filler: alloc::string::String = core::iter::repeat_n('.', LINE).collect();
    mpos::println!("serial tx: {LINE} bytes through the transmit ring{filler}");

    // Wait for the handler to have handed them over. The wait is bounded, and
    // the bound is ticks rather than a count of its own: a machine whose clock
    // stopped is a machine nothing here can finish anyway.
    let mut waited = 0;
    loop {
        if hal::tx_ring::stats().moved >= before.moved + LINE {
            break;
        }
        waited += 1;
        if waited > 200 {
            // The detail goes through the *machine's* polled writer, not
            // `println!`: a failure here is by definition a queue that is not
            // draining, and a report filed through that same queue is a report
            // nobody reads. The harness's own line carries the short version.
            let now = hal::tx_ring::stats();
            mpos::arch::print_args(format_args!(
                "\nserial tx: {} bytes queued but only {} handed to the PL011 \
                 ({waited} ticks, no progress)\n",
                now.queued - before.queued,
                now.moved - before.moved
            ));
            return Err(alloc::format!(
                "{} bytes queued but only {} handed to the PL011 after {waited} ticks",
                now.queued - before.queued,
                now.moved - before.moved
            ));
        }
        sched::sleep_ticks(1);
    }
    let now = hal::tx_ring::stats();
    let moved = now.moved - before.moved;
    let visits = now.visits - before.visits;
    if visits == 0 {
        mpos::arch::print_args(format_args!(
            "\nserial tx: {moved} bytes went out without a single interrupt\n"
        ));
        return Err(alloc::format!(
            "{moved} bytes went out without a single interrupt"
        ));
    }
    mpos::println!(
        "serial tx: {moved} bytes handed to the PL011 by {visits} interrupt(s), \
         ({waited} ticks elapsed)"
    );
    Ok(())
}

/// Stage 7b's evidence: the polled console input reaches a reader through the
/// `hal::ConsoleInput` capability, not just the raw UART.
///
/// `check_serial_rx` in `arch` already proved `pl011::read_byte` reads the FIFO
/// (loopback). What this proves is the seam *above* the machine layer: a byte put
/// in the receiver by [`hal::console_inject_input`] comes back out of
/// [`hal::console_poll_input`] and, handed to the tty, is readable through the
/// tty's own `read` - the exact moves the pump makes at runtime, driven here by
/// loopback so a headless boot can exercise it. Runs before the pump task is
/// spawned, so the poll here is the only consumer of the injected bytes.
#[cfg(target_arch = "aarch64")]
pub(crate) fn check_console_rx(_info: &BootInfo) -> Result<(), String> {
    // Feed a line: a probe byte and a newline, each injected into the receiver and
    // pulled back out through the capability forward - the exact moves the pump
    // makes at runtime, driven here by loopback so a headless boot can exercise it.
    let mut typed = [0u8; 2];
    typed.copy_from_slice(b"Z\n");
    let tty = vfs::tty::global();
    for &byte in &typed {
        hal::console_inject_input(byte);
        match hal::console_poll_input() {
            Some(got) if got == byte => tty.receive_byte(got),
            other => {
                return Err(alloc::format!(
                    "the poll after injecting {byte:#04x} gave {other:?}"
                ));
            }
        }
    }

    // Read it back through the tty the way a shell would read `fd 0`. A canonical
    // tty returns a whole line once the newline has landed.
    let mut line = [0u8; 8];
    match tty.read(&mut line) {
        Ok(n) if &line[..n] == b"Z\n" => {
            mpos::println!("console rx: the injected line read back through the tty");
            Ok(())
        }
        other => Err(alloc::format!(
            "the tty read gave {other:?}, not the injected \"Z\\n\""
        )),
    }
}
