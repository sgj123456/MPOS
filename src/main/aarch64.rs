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
use mpos::{drivers, hal, sched, vfs};

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

    // Stage 7's disk root: the disk `check_fs` registered, mounted through the
    // VFS and read back by path. After `check_dev` so the `/dev/vda` node it
    // claims is the one the devfs build actually published.
    check_disk_root();

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

    // Stage 8-net: the smoltcp stack over the MMIO adapter `check_net` opened and
    // published. The adapter is the machine's to find (a window the tree names)
    // and the stack is not: it receives the `NetDevice` seam exactly as x86_64's
    // does, so this is the same `net::init` + `spawn_service` pair the first
    // machine boots with, and nothing above `drivers` names a transport.
    init_net();
    register_exit_hooks();

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

    // Stage 7's root switch: `/` becomes the disk, so the tree the machine runs
    // from is the one on the block device rather than the bootstrap ramfs. The
    // bootstrap tree stays mounted at `/oldroot` (the semantics `pivot_root`
    // documents) rather than being dropped, which keeps it reachable until a
    // later cut hands its memory back.
    switch_root();

    // `/init` is read from the *disk* root and loaded as a task; it lands on
    // pid 1. The initramfs's own copy went with the old root to `/oldroot`, which
    // is what makes this a root switch rather than a second mount.
    let init = vfs::read_file("/init").expect("the disk root carries /init");
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

/// Stage 7's disk root on the second machine: the disk `check_fs` registered
/// (`vda`, from the ext4 it mounted over the MMIO virtio-blk) is mounted
/// through the *VFS* and read back by path.
///
/// `check_fs` proved the bytes - an ext4 mounted over a memory-mapped block
/// device, and its file read through the driver's own handle. This proves the
/// layer above it: the block device is a kernel disk the devfs build published
/// as `/dev/vda`, the filesystem is reached through the mount table, and the
/// read goes through path resolution the way a process's `open` does. The
/// machine still boots its initramfs as the root - the disk-root *switch* is
/// the next cut (`PLAN.md` §13.3) - so what is settled here is that the disk is
/// a filesystem this kernel serves, not a driver-local handle.
fn check_disk_root() {
    let Some(dev) = mpos::blk::raw_disk() else {
        mpos::println!("M7_DISK_FAILED: no disk was registered with the block layer");
        hal::halt()
    };
    let Ok(fs) = mpos::vfs::ext4::Ext4Fs::mount(dev) else {
        mpos::println!("M7_DISK_FAILED: the registered disk did not mount as ext4");
        hal::halt()
    };
    let vfs = mpos::vfs::global();
    // The name the marker claims, checked rather than asserted: the devfs build
    // above publishes the disks `blk::disks()` reports, so a disk the block
    // layer holds but `/dev` does not list is a failure here.
    if vfs::resolve("/dev/vda").is_err() {
        mpos::println!("M7_DISK_FAILED: /dev/vda is not in the mounted /dev");
        hal::halt()
    }
    let _ = vfs.root_inode().create("mnt", mpos::vfs::NodeKind::Dir);
    if let Err(e) = vfs::mount("/mnt", &fs) {
        mpos::println!("M7_DISK_FAILED: mounting the disk at /mnt gave {e:?}");
        hal::halt()
    }
    // Through the mount table, not a handle: `read_file` resolves every path
    // component and reads what it lands on. The file is one the *repository*
    // owns - `rootfs/etc/inittab`, part of the overlay `xtask` lays on the disk -
    // so the read proves the disk serves a root tree rather than that a
    // filesystem mounted. Its length is reported so the command can check it
    // against the same file in the tree.
    let Ok(inittab) = vfs::read_file("/mnt/etc/inittab") else {
        mpos::println!("M7_DISK_FAILED: /mnt/etc/inittab did not read through the VFS");
        hal::halt()
    };
    if inittab.is_empty() {
        mpos::println!("M7_DISK_FAILED: /mnt/etc/inittab read empty");
        hal::halt()
    }
    // And the disk carries the init image the initramfs does, so the root switch
    // - still the next cut - has a program to hand pid 1.
    if vfs::resolve("/mnt/init").is_err() {
        mpos::println!("M7_DISK_FAILED: the disk carries no /init for a root switch");
        hal::halt()
    }
    mpos::println!(
        "M7_DISK_OK: /dev/vda mounted at /mnt, /mnt/etc/inittab {} bytes read through the VFS, /mnt/init present",
        inittab.len()
    );
}

/// Stage 8's live network: the shared smoltcp stack over this machine's adapter.
///
/// The mirror of x86_64's `init_net`, and deliberately the same two calls. The
/// difference is only where the device comes from: there the PCI bus is probed
/// (`drivers::virtio::net::VirtioNet::init`), here the adapter is the MMIO window
/// `check_net` found in the device tree and *published* (`mmio::net_device`) -
/// which is why the ring that check posted is the ring the stack's first frame
/// arrives on, rather than a second open of the same window.
///
/// The marker this prints is narrow on purpose. It says the adapter was handed to
/// `net::init`, the two interfaces were built over it (eth0 at the address the
/// stack configures, plus smoltcp's own loopback), and the service task that polls
/// them was spawned. It does **not** claim a round trip: traffic is proven by the
/// programs that drive the stack - `/t-socket`, `/t-udp-loopback` and the rest on
/// this machine's sweep - because a kernel-side self-test ping is exactly the hook
/// this project keeps out of `net` (job 4's pin is a real EL0 program).
///
/// A machine with no adapter is not a failure here: the absence is printed, the
/// stack is never initialized, and the socket syscalls answer `ENETDOWN`, which is
/// the honest answer for a machine without a card. (`check_net` having failed is a
/// different thing entirely, and stops the boot.)
fn init_net() {
    let Some(dev) = drivers::virtio::mmio::net_device() else {
        mpos::println!("net: no virtio-net window opened; sockets answer ENETDOWN");
        return;
    };
    let mac = drivers::virtio::NetDevice::mac(&*dev);
    mpos::net::init(dev);
    mpos::net::spawn_service();
    mpos::println!(
        "net: virtio-net up at 10.0.2.15/24, gateway 10.0.2.2, MAC \
         {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    mpos::println!("M8_NET_STACK_OK");
}

/// Tell the scheduler's exit path what a dying process's descriptors own.
///
/// x86_64's list is five hooks plus two `FD_CLOEXEC` ones; this machine's is one,
/// and the difference is the rest of its port: record locks, inotify, evdev and
/// uinput are subsystems it has not been given. The network stack's it does have,
/// and that hook is not garnish - a socket a dying process leaves in the stack's
/// set keeps its local port reserved for ever, so the next program to ask for that
/// port is refused. Measured on this machine with the stack live and this not
/// registered: `t-tcp`'s bind of 34567 answered EADDRINUSE on a machine that had
/// just booted, and the last check of `t-tcp-listen` (a TIME-WAIT name coming back
/// to a restart that asks for it) failed the same way - because the socket tests
/// that ran before them had exited with their sockets still in the set.
///
/// Registered here rather than inside [`init_net`] because it belongs to the
/// *descriptor*, not the device: a machine whose adapter never probed still has
/// `AF_UNIX` and `AF_VSOCK` sockets, and they are released by this same hook.
fn register_exit_hooks() {
    mpos::sched::exit::register(mpos::net::socket::close_sockets_at_exit);
}

/// Stage 7's root switch: the disk `check_disk_root` mounted at `/mnt` becomes
/// `/`, and the ramfs the boot started on moves to `/oldroot`.
///
/// Everything the machine runs after this resolves against the disk tree, which
/// is what "booting a rootfs" means and the difference between a filesystem the
/// kernel can *read* and the filesystem the system *is*. The x86_64 machine has
/// been on an ext4 root since its initramfs's `/init` script switched to
/// `/dev/vda2`; this is the same arrangement reached from the other side, by the
/// kernel instead of by an init script, because the aarch64 handoff has no
/// script to run.
///
/// `/dev` is re-made on the new root: the devfs the boot mounted hangs off the
/// tree that just moved aside, and the init opens `/dev/urandom`,
/// `/dev/console` and `/dev` itself through libc - a root with no device nodes
/// is not one a real init can start on.
fn switch_root() {
    let vfs = mpos::vfs::global();
    let mnt = match vfs::resolve("/mnt") {
        Ok(inode) => inode,
        Err(e) => {
            mpos::println!("M7_ROOT_FAILED: /mnt does not resolve before the switch: {e:?}");
            hal::halt()
        }
    };
    // `pivot_root` parks the old root at a directory *inside* the new one, so the
    // directory has to exist on the disk first.
    let _ = mnt.create("oldroot", mpos::vfs::NodeKind::Dir);
    if let Err(e) = vfs::pivot_root("/mnt", "/mnt/oldroot") {
        mpos::println!("M7_ROOT_FAILED: pivot_root to the disk gave {e:?}");
        hal::halt()
    }
    // The new root's own /dev, then the claim checked rather than asserted: the
    // disk's own file and a device node both resolve on the switched tree.
    let root = match vfs::resolve("/") {
        Ok(inode) => inode,
        Err(e) => {
            mpos::println!("M7_ROOT_FAILED: the switched root does not resolve: {e:?}");
            hal::halt()
        }
    };
    let _ = root.create("dev", mpos::vfs::NodeKind::Dir);
    let dev = mpos::drivers::devices::standard_devfs();
    if let Err(e) = vfs::mount("/dev", &*dev) {
        mpos::println!("M7_ROOT_FAILED: mounting devfs on the new root gave {e:?}");
        hal::halt()
    }
    if vfs::resolve("/etc/inittab").is_err() || vfs::resolve("/dev/urandom").is_err() {
        mpos::println!(
            "M7_ROOT_FAILED: the switched root serves neither /etc/inittab nor /dev/urandom"
        );
        hal::halt()
    }
    mpos::println!(
        "M7_ROOT_OK: / is the disk rootfs, the ramfs moved to /oldroot, /dev re-mounted, /etc/inittab and /dev/urandom resolve"
    );
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
