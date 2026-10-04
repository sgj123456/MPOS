//! The aarch64 kernel image: boot entry and panic handler.
//!
//! The x86_64 half of this file's job is done by `mpos-boot`, and since stage 1e
//! so is this one's: the loader reads the kernel off the ESP, applies its
//! relocations against `KERNEL_VIRT_BASE`, builds the page tables, exits boot
//! services and branches here with the machine already running in the higher
//! half. So there is no entry stub and no stack to set up — the one interface is
//! the register the record arrives in, exactly as on x86_64 (`RDI` there, `x0`
//! here), and this function's own prologue is the first thing to use the stack
//! the loader mapped.
//!
//! What this file had until stage 1e was a `global_asm!` `_start` that loaded
//! `sp` from a `__stack_top` symbol the linker script cut out of the image, and
//! a `.bss` walk beneath it. Both belonged to the `-kernel` bring-up: QEMU
//! placed the ELF at a fixed physical address and jumped in with `sp = 0`, so
//! the image had to carry its own stack and could not assume its `NOBITS` tail
//! came back zeroed. The loader does both of those now — `vm::load` zeroes every
//! segment's memory tail and `main.rs` allocates and maps the boot stack — which
//! is why x86_64's entry has neither, and why this one no longer does.

use core::panic::PanicInfo;

use alloc::string::String;
use alloc::vec;

use mpos::boot::handoff::Handoff;
use mpos::{hal, sched, vfs};

/// `mpos-boot` branches here at `KERNEL_VIRT_BASE + entry`, at EL1 with the MMU
/// already on and the machine's own tables installed, on a boot stack the loader
/// mapped through the direct map, with `x0` holding the [`Handoff`] record it
/// wrote.
///
/// The register is the whole interface, and the record has to be checked rather
/// than trusted: nothing in the machine enforces that `x0` holds what this
/// function was compiled to expect, so `boot::handoff::collect` asserts the
/// magic before anything reads a field. That check is the only reason this
/// pointer can be dereferenced at all — the same contract x86_64's `_start`
/// carries, spelled in a different register.
///
/// # Safety
///
/// The caller is the loader's branch to this symbol, with `x0` holding the
/// record it wrote and every address that record names mapped for as long as the
/// kernel runs. Nothing chooses this call from Rust.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(handoff: *const Handoff) -> ! {
    // SAFETY: the caller's contract, restated in `arch::aarch64::boot`'s own
    // `# Safety`: the branch into this symbol carried the record the loader
    // wrote, and the checks there read it under exactly that assumption.
    // `console_milestone` is run by the machine boot once its PL011 is up; it is
    // defined here (in the wiring, which may name `crate::console`) so that
    // `arch` never reaches up into the console service it sits below.
    unsafe { mpos::arch::boot(handoff, console_milestone, init_handoff) }
}

/// The init handoff, run by the machine layer after the EL0 milestone probe - on
/// the boot stack the probe's `eret` resumed onto, which `sched::init` then adopts
/// as task 0. It mirrors the x86_64 `main`'s tail minus the subsystems this machine
/// has not been given yet (no PCI/virtio-blk/net/DRM: the root is a ramfs unpacked
/// from the loader's initramfs, exactly as the x86_64 kernel boot does before the
/// data root is mounted by init's own `mount(2)`).
///
/// Starting the scheduler is what lets the syscall gate work: from here on
/// `sched::started()` is true, a real task is current, and `/init`'s `svc` routes
/// into the shared `syscall::dispatch` table rather than the inline refusal the
/// pre-scheduler probe still gets. `/init` is the four-instruction `exit_group(0)`
/// program `xtask` builds; running it to completion and reaping it proves the
/// whole load -> enter EL0 -> trap -> dispatch -> exit round trip on this machine.
fn init_handoff(initramfs: *const u8, len: usize) -> ! {
    // The archive is the initramfs bytes `boot` read out of the handoff record's
    // direct-mapped `BootInfo` *before* the address-space switch and stashed here.
    // Re-reading the record from its raw pointer after that switch faults (the
    // loader's identity address is not in the kernel's own tables), so the pointer
    // and length are handed over directly. A null pointer means a diskless boot.
    // SAFETY: `boot` passed a pointer and length naming a live direct-mapped range.
    let archive: Option<&[u8]> = if initramfs.is_null() {
        None
    } else {
        Some(unsafe { core::slice::from_raw_parts(initramfs, len) })
    };

    // The scheduler adopts the running context (the boot stack) as task 0 and
    // installs its loop. Must come before any spawn and before the gate relies on
    // `sched::started()`.
    sched::init();

    // A ramfs root, then the initramfs unpacked into it. With no archive this is a
    // diskless boot: there is nothing to hand the machine to, so stop honestly
    // rather than invent an init.
    vfs::file::init_standard(vfs::tty::init());
    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(&*root);

    // Stage 7-dev: the device set a real init mounts as `/dev` builds on this
    // machine and its entropy node reads back. It runs here, after
    // `vfs::tty::init()` has populated the tty table (the console tty is what
    // `standard_devfs()` is built from), not as an early boot milestone - which
    // is the ordering lesson behind the `/dev` note in `PLAN.md` §9.1.
    check_dev();

    // Stage 7b: the polled console *input* path, pinned before anything reads it.
    // The machine can now hand a typed byte through `hal::console_poll_input` (the
    // PL011 RX FIFO); route it into the tty the way x86_64's serial interrupt does
    // (`vcmux::route_input`) and read it back through the tty's own `read`. A
    // headless boot has no terminal, so the UART's own loopback injects the byte.
    // Doing this *before* the pump task is spawned means the self-test is the only
    // reader, so the injected bytes cannot be stolen.
    check_console_rx();

    // The production input pump: poll the wire and hand any byte to the tty, so a
    // shell reading `fd 0` gets typed input. Aarch64 has one console tty (no
    // multi-VC mux yet - `vcmux` is x86_64-only), so this calls the tty's own
    // `receive_byte` directly - the step `vcmux::route_input` performs for the
    // active VC on x86_64. Polled at the tick rate for now; the GIC-routed RX
    // interrupt that would let it park instead of poll is a later cut.
    sched::spawn(
        || loop {
            match hal::console_poll_input() {
                Some(byte) => vfs::tty::global().receive_byte(byte),
                None => sched::sleep_ticks(1),
            }
        },
        sched::Priority::NORMAL,
    );

    // A ramfs root, then the initramfs unpacked into it. With no archive this is a
    // diskless boot: there is nothing to hand the machine to, so stop honestly
    // rather than invent an init.
    let Some(archive) = archive else {
        mpos::println!("initramfs: none - nothing to hand off to");
        hal::halt()
    };
    let members =
        vfs::tar::unpack_into(vfs::global(), archive).expect("the aarch64 initramfs was broken");
    mpos::println!("initramfs: {members} members unpacked");

    // `/init` is read from the ramfs and loaded as a task; it lands on pid 1.
    let init = vfs::read_file("/init").expect("the initramfs carries /init");
    let argv = vec![String::from("/init")];

    // Queue a line on the tty before the init starts reading, so its `read(0,...)`
    // returns real console input instead of blocking. This stands in for a person
    // typing (which `cargo xtask run --arch aarch64` provides live, via the stage-7b
    // input pump); at headless bring-up it lets boot-aarch64 prove the interactive
    // *read* path reaches a userspace program, not just the kernel.
    for &byte in b"echo-test\n" {
        vfs::tty::global().receive_byte(byte);
    }

    let pid = sched::spawn_init(&init, &argv, &[]).expect("the aarch64 init image loaded");
    assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1");
    mpos::println!("mpos: aarch64 machine handed to /init");

    // The image is loaded and queued, but it has not run yet - it runs only once
    // the boot task yields the CPU. `M6_INIT_OK` says the handoff put the program
    // on pid 1; the proof it *executes* comes next, from the init's own `read(0)`
    // returning the queued line and its confirmation write reaching the console.
    mpos::println!("M6_INIT_OK");

    // Yield turns to the scheduler until the init has run and left. Waiting on
    // `is_running(pid)` - the init's own liveness - rather than `has_live_child(pid)`
    // (which asks about init's *children*, and is false from the start because the
    // first process has no parent) is what actually lets the init be scheduled. The
    // committed wait on `has_live_child` returned immediately and never ran the
    // init at all.
    //
    // When the init's `exit_group` traps, the gate routes it to `sched::exit`,
    // which for pid 1 prints `INIT_EXITED` and halts. That line, and the banner the
    // init wrote through `write(1,...)` before it, are the bring-up proof a real
    // userspace program ran at EL0 on this machine. The loop below is only reached
    // if the init somehow returns without exiting.
    while sched::is_running(pid) {
        sched::sleep_ticks(1);
    }
    hal::halt()
}

/// Stage 7-dev's evidence: the machine builds the device set a real init mounts
/// as `/dev` and reads a working node out of it.
///
/// `drivers::devices::standard_devfs()` is exactly what `mount -t devtmpfs /dev`
/// hands back - the dispatch arm this stage opened to aarch64 - and it is built
/// from the console tty, so it can only run once `vfs::tty::init()` has filled
/// the tty table. That is why it is here rather than an early boot milestone: a
/// check that reached for the tty before init registered it panicked in
/// `tty::global()` (the measured `PLAN.md` §9.1 `/dev` finding).
///
/// It reads two nodes through the trait surface (`root -> lookup -> read_at`), the
/// same moves an `open`/`read` make: `/dev/urandom` (the pool `rand` seeded from
/// the MMIO rng) and `/dev/hwrng` (the MMIO rng itself, reached through the same
/// `fill` `check_rng` used), proving the generic set is present and entropy flows
/// on this machine. `/dev/input`, `uinput`, the virtio-PCI `hvc`/`dsp`/`fb0` and
/// the DRM card are gated out here (the machine has no path to them yet), so the
/// set is the honest subset, not a stub of the full x86_64 tree.
fn check_dev() {
    let dev = mpos::drivers::devices::standard_devfs();
    // Mount it. This mirrors what Linux's `CONFIG_DEVTMPFS_MOUNT` does: the
    // kernel creates `/dev` on the root and mounts its devtmpfs there, so a
    // userspace init finds the device set already in place rather than having
    // to build it. The ramfs root carries no `/dev` yet (the aarch64 initramfs
    // is only the hand-encoded ELF programs), so the mountpoint is created here
    // first - exactly the dir an `mknod`/`init` would have had to make anyway.
    let vfs = mpos::vfs::global();
    let _ = vfs.root_inode().create("dev", mpos::vfs::NodeKind::Dir);
    if let Err(e) = vfs::mount("/dev", &*dev) {
        mpos::println!("M7_DEV_FAILED: mounting devfs at /dev gave {e:?}");
        hal::halt()
    }

    // Read two nodes through the *live mount table* (`resolve` is the same walk
    // a userspace `open` makes), proving `/dev` is genuinely on the filesystem -
    // not merely that `standard_devfs()` constructs. `/dev/urandom` fills the
    // whole buffer (it neither blocks nor short-reads), so a 16-byte ask must
    // come back with 16 bytes.
    let Ok(urandom) = vfs::resolve("/dev/urandom") else {
        mpos::println!("M7_DEV_FAILED: /dev/urandom does not resolve through the mount table");
        hal::halt()
    };
    let mut buf = [0u8; 16];
    match urandom.read_at(0, &mut buf) {
        Ok(n) if n == buf.len() => {}
        other => {
            mpos::println!("M7_DEV_FAILED: reading /dev/urandom gave {other:?}, expected 16 bytes");
            hal::halt()
        }
    }
    // `/dev/hwrng` is the entropy device itself - the MMIO virtio-rng `check_rng`
    // installed - reached through the same `fill`. A healthy device answers a read
    // with the whole buffer, so an 8-byte ask must come back with 8 host-generated
    // bytes. Both resolve and read through the mounted tree, not the devfs root.
    let Ok(hwrng) = vfs::resolve("/dev/hwrng") else {
        mpos::println!("M7_DEV_FAILED: /dev/hwrng does not resolve through the mount table");
        hal::halt()
    };
    let mut hbuf = [0u8; 8];
    match hwrng.read_at(0, &mut hbuf) {
        Ok(n) if n == hbuf.len() => mpos::println!("M7_DEV_OK"),
        other => {
            mpos::println!("M7_DEV_FAILED: reading /dev/hwrng gave {other:?}, expected 8 bytes");
            hal::halt()
        }
    }
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
fn check_console_rx() {
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
                mpos::println!(
                    "M6_CONSOLE_RX_FAILED: poll after inject gave {other:?}, not {byte:#04x}"
                );
                hal::halt()
            }
        }
    }

    // Read it back through the tty the way a shell would read `fd 0`. A canonical
    // tty returns a whole line once the newline has landed.
    let mut line = [0u8; 8];
    match tty.read(&mut line) {
        Ok(n) if &line[..n] == b"Z\n" => mpos::println!("M6_CONSOLE_RX_OK"),
        other => {
            mpos::println!("M6_CONSOLE_RX_FAILED: tty read gave {other:?}, expected \"Z\\n\"");
            hal::halt()
        }
    }
}

/// Stage 4b's evidence: `crate::console` - un-gated for this machine one stage ago -
/// actually reaches the wire. Every boot line before this one was `arch`'s own
/// `pl011::Writer`; this is the first through the console service (`println!` ->
/// `print_args` -> the `hal::Console` sink -> the PL011). If the service were
/// mis-wired on this machine the milestone line simply would not appear, and
/// `cargo xtask boot-aarch64` asserts on its presence.
fn console_milestone() {
    // Bring the service's sink up (idempotent over the PL011 the boot already
    // poked) and send one line, then the marker, through the same path a
    // `println!` anywhere above the machine layer will use.
    mpos::console::init();
    mpos::println!("console: kernel log reached the PL011 through crate::console");
    mpos::println!("M4_CONSOLE_OK");
}

/// The panic path: one line on the console, then stop.
///
/// It prints through `arch`'s own machine writer (`arch::print_args`) rather than
/// `mpos::println!` not because the console service is missing on this machine -
/// stage 4a un-gated it - but because a panic must reach the host at *any* point
/// in boot, including before `crate::console` has been brought up or after its
/// state has been corrupted, and the machine writer has no dependencies beyond the
/// mapped device window. A panic that printed nothing because a higher layer was
/// mid-init would be the worst version of the gap: the machine stops with no word
/// about why.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    mpos::arch::print_args(format_args!("\nKERNEL PANIC: {info}\n"));
    mpos::arch::halt()
}
