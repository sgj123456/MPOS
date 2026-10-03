//! `mpos-boot`: what starts mpos on a UEFI machine.
//!
//! The firmware loads this, it loads the kernel, and then it stops being a
//! program. What it leaves behind is a `BootInfo`'s worth of plain data and an
//! address space, handed over in one instruction - so the interesting part of
//! this crate is not the sequence of steps but the order they have to happen in:
//!
//! 1. **Read what the kernel needs off the boot partition.** `mpos.elf`, and
//!    the initramfs and command line beside it. This has to be first because it
//!    is the only step that can fail in a way worth reporting, and because
//!    reading needs the firmware's file protocols, which do not survive step 5.
//! 2. **Work out the memory map.** Also firmware data, also gone at step 5, and
//!    it is what the page tables and the kernel's frame allocator are both built
//!    from.
//! 3. **Allocate everything.** The pool of page tables, the frames the image
//!    lands in, the stack and the copies of the two files. After step 5 there is
//!    no allocator left, not even the loader's own.
//! 4. **Build the address space and load the image into it** - relocated against
//!    `KERNEL_VIRT_BASE`, so that the higher-half jump at step 6 lands somewhere
//!    the kernel's own data already points.
//! 5. **Leave boot services**, which is the point of no return: the firmware's
//!    protocols, allocator and console all stop existing.
//! 6. **Fill in the handoff record and jump.** No return, no `SystemTable`, no
//!    firmware. Just the kernel, at its linked address, with `RDI` pointing at
//!    the record.
//!
//! The one thing the loader deliberately does *not* do is decide anything the
//! kernel could decide itself: it does not pick a root device, parse a command
//! line or touch a file system on the disk. It moves bytes and builds page
//! tables, and everything above that is the kernel's business.
//!
//! # Two machines
//!
//! The list above is one list, and it is the same on x86_64 and aarch64: the
//! `uefi` crate's file protocols, memory map and pool allocator are portable, and
//! so is the handoff record - it carries plain data and no page-table root, no
//! GDT and no EFI table, which is exactly the seam `src/boot/mod.rs` describes.
//! Three things are the machine's, and each has one arm per machine and nothing
//! in between:
//!
//! * **The progress lines' wire.** COM1 is a port and an address and aarch64 has
//!   no port I/O at all, so the aarch64 arm writes a PL011 - the same UART, at
//!   the same fixed address, that the kernel's `arch/aarch64/pl011.rs` owns
//!   (a separate compilation with no crate between them, the same reason the
//!   handoff record is written twice).
//! * **The page-table descriptor.** The three ranges, the pool and the sizing
//!   are shared in `vm.rs`; the format is `vm/x86_64.rs`'s or `vm/aarch64.rs`'s.
//! * **The last instruction.** `mov cr3` and `jmp` against `msr ttbr0_el1` and
//!   `br`, with the record in a different register (`RDI` against `x0`) and a
//!   stack that has to be sixteen-byte aligned rather than one word down.

#![no_std]
#![no_main]

extern crate alloc;

mod elf;
mod handoff;
mod vm;

use alloc::vec::Vec;
use core::panic::PanicInfo;

use uefi::CStr16;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::fs::FileSystem;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};
use uefi::prelude::*;

use elf::PAGE_SIZE;
use handoff::{Handoff, MAGIC, MAX_MEM_REGIONS, Region};

/// `wcslen`, spelled by the compiler rather than by any source here.
///
/// At the workspace's optimization level LLVM recognises the wide-character
/// counting loop inside the `uefi` crate (`CStr16::num_chars`, behind the file
/// protocols this loader reads through) as the `wcslen` idiom and emits a call
/// to the libc symbol for it - a call the UEFI target cannot satisfy, there
/// being no libc. The same loop at the debug profile stays a loop, which is
/// why only the release build of this crate ever linked against the name.
/// Providing it is five lines, and the semantics are the one the idiom
/// promises: the number of `u16`s before the first zero.
///
/// # Safety
///
/// `s` must point at a NUL-terminated run of `u16`s that is valid to read. The
/// caller is not Rust code at all: it is the `wcslen` call LLVM emitted, whose
/// only contract is the C one - and the loop below is the same one the compiler
/// recognised, so anything the compiler was entitled to assume of it holds here.
/// The function is `unsafe` for the same reason the idiom it replaces was: the
/// bound comes from the data, and nothing in the signature can say so.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

/// Where the kernel and its two companions are on the boot partition. Fixed
/// names rather than a boot entry or a config file: the layout is decided by the
/// script that builds the image, the loader is built from the same tree, and a
/// path that arrives from somewhere else is a path an attacker controls.
const KERNEL_PATH: &CStr16 = cstr16!("\\mpos.elf");
const INITRAMFS_PATH: &CStr16 = cstr16!("\\initramfs.tar");
const CMDLINE_PATH: &CStr16 = cstr16!("\\mpos.cmdline");

/// The boot stack, in pages. The kernel runs on this until its scheduler gives
/// every task one of its own; it is the stack `_start` and everything down to
/// `handoff_to_init` run on.
///
/// 64 pages rather than the 16 a task gets, because this stack carries strictly
/// more than a task stack does: the whole boot path - memory discovery, the
/// page-table build, the device probe, the network bring-up, the VFS mount
/// table, and the entire self-test sweep - all of it nested inside `_start`,
/// with nothing ever unwinding in between to give any of it back. `handoff_to_init`
/// then runs `spawn_init` on top of that pile: it parses an ELF, builds an
/// address space, and maps a stack for the new process. A debug build's frames
/// are several times a release build's, and 16 pages was measured to leave the
/// stack within 8 KiB of its bottom by the time init was spawned - close enough
/// that the first trap taken there overwrote the frame its own handler was about
/// to return through. That failure is silent and misleading: the `iretq` pops a
/// word the overflow wrote rather than the selector it pushed, and the panic
/// names whatever that word happens to decode as instead of saying "out of stack".
///
/// This is a boot-time figure and not the kernel's, so it is sized to be
/// obviously sufficient rather than tight: failing here costs 192 KiB of a
/// machine that has hundreds of megabytes, and failing here is a panic during
/// boot with the serial console as the only witness.
const STACK_PAGES: usize = 64;

/// Written into the lowest word of the boot stack before the kernel starts, and
/// checked by the kernel once it is up. The bytes spell `mpos_boo` little-endian,
/// the way [`handoff::MAGIC`] spells `MPOSBOOT`: whoever is looking at a hexdump
/// of the stack can see what left the mark. Spelled out on both sides of the
/// handoff rather than shared, for the same reason the magic is - the loader and
/// the kernel are separate compilations with no crate between them.
const BOOT_STACK_CANARY: u64 = u64::from_le_bytes(*b"mpos_boo");

/// Where the loader leaves the record for the kernel. A `static` and not an
/// allocation, because it is written after boot services are gone and read from
/// a page the loader's own image owns.
static mut HANDOFF: Handoff = EMPTY_HANDOFF;

const EMPTY_HANDOFF: Handoff = Handoff {
    magic: 0,
    hhdm_offset: 0,
    kernel_phys_base: 0,
    kernel_virt_base: 0,
    initramfs_phys: 0,
    initramfs_len: 0,
    cmdline_phys: 0,
    cmdline_len: 0,
    boot_stack_base: 0,
    region_count: 0,
    regions: [Region {
        base: 0,
        length: 0,
        kind: 0,
        reserved: 0,
    }; MAX_MEM_REGIONS],
};

#[entry]
fn main() -> Status {
    serial::init();
    log("mpos-boot starting");

    let image = boot::image_handle();
    let fs = boot::get_image_file_system(image).expect("the boot volume has no file system");
    let mut fs = FileSystem::new(fs);

    let kernel_bytes = read(&mut fs, KERNEL_PATH);
    log(&format!("kernel image: {} KiB", kernel_bytes.len() / 1024));
    let initramfs = read(&mut fs, INITRAMFS_PATH);
    log(&format!("initramfs: {} KiB", initramfs.len() / 1024));
    let cmdline = read(&mut fs, CMDLINE_PATH);
    let cmdline = trim(&cmdline);

    let kernel = elf::Kernel::parse(&kernel_bytes);

    // The map as it stands now, only to size the two allocations that depend on
    // how much RAM there is. The map the kernel is given is the one from after
    // boot services are exited, which is both shorter and more accurate.
    let before = boot::memory_map(MemoryType::LOADER_DATA).expect("no memory map");
    let mut ram = 0u64;
    for entry in before.entries() {
        if vm::is_ram(kind_of(entry.ty)) {
            ram += entry.page_count * PAGE_SIZE;
        }
    }
    log(&format!("RAM: {} MiB", ram / (1024 * 1024)));

    // Everything that needs the firmware's allocator, in one place. The pool is
    // sized from the bytes that will be mapped, which is RAM - mapped twice,
    // identity and through the direct map - plus, on aarch64, the device window
    // the kernel reaches its console and interrupt controller through.
    let pool_bytes = vm::pool_bytes_for(ram + vm::extra_mapped_bytes());
    let pool = allocate((pool_bytes / PAGE_SIZE) as usize, "page-table pool");
    let root_frame = allocate(1, "page tables");
    let image_pages = kernel.span.div_ceil(PAGE_SIZE) as usize;
    let image_mem = allocate(image_pages, "kernel image");
    let stack = allocate(STACK_PAGES, "boot stack");
    // A canary in the lowest word of the stack, checked by the kernel once it is
    // up. The loader is the only party that knows where the stack starts, and the
    // kernel has no way to ask the firmware afterwards - so the mark goes in
    // here, while both the address and boot services are still available.
    //
    // It catches an overflow after the fact, the same way the scheduler's
    // per-task canary does, and for the same reason: without it the stack runs
    // out silently. The `iretq` of whatever trap came next pops a word the
    // overflow wrote instead of the one it pushed, and the machine dies naming a
    // selector that was never loaded - a symptom that points at the segment
    // tables and not at the stack at all.
    unsafe { (stack as *mut u64).write_volatile(BOOT_STACK_CANARY) };
    // Through the direct map, not at the address it was allocated at. The kernel
    // builds its own page tables a moment after `_start` and there is no identity
    // mapping in them - that is the whole point of the higher-half jump - so a
    // stack addressed by its physical address would be unmapped by the time
    // `paging::install` returns to it. Behind `HHDM_OFFSET` it stays mapped for
    // as long as the kernel runs, which is what Limine's bootloader-reclaimable
    // stack used to give it.
    let stack_top = vm::HHDM_OFFSET + stack + (STACK_PAGES as u64) * PAGE_SIZE;
    let initramfs_mem = if initramfs.is_empty() {
        0
    } else {
        let pages = (initramfs.len() as u64).div_ceil(PAGE_SIZE) as usize;
        let at = allocate(pages, "initramfs");
        unsafe {
            core::ptr::copy_nonoverlapping(initramfs.as_ptr(), at as *mut u8, initramfs.len())
        };
        at
    };
    let cmdline_mem = copy_to_frames(cmdline);

    unsafe { core::ptr::write_bytes(root_frame as *mut u8, 0, PAGE_SIZE as usize) };
    unsafe { kernel.load(image_mem as *mut u8, vm::KERNEL_VIRT_BASE) };
    log(&format!(
        "kernel: {} KiB at phys {:#x}, virt {:#x}, entry {:#x}",
        kernel.span / 1024,
        image_mem,
        vm::KERNEL_VIRT_BASE,
        vm::KERNEL_VIRT_BASE + kernel.entry
    ));

    let mut tables = unsafe { vm::PageTables::new(root_frame, vm::Pool::new(pool, pool_bytes)) };
    let mut regions = Vec::new();
    for entry in before.entries() {
        if vm::is_ram(kind_of(entry.ty)) {
            regions.push(Region {
                base: entry.phys_start,
                length: entry.page_count * PAGE_SIZE,
                kind: kind_of(entry.ty),
                reserved: 0,
            });
        }
    }
    let mapped = vm::map_physical_memory(&mut tables, &vm::merge(regions));
    log(&format!(
        "mapped {} MiB of physical memory, identity and through the direct map at {:#x}",
        mapped / (1024 * 1024),
        vm::HHDM_OFFSET
    ));
    // The machine's device registers, on the one machine that has no other way
    // to reach them yet: x86_64's kernel walks a BAR through `mm::map_mmio`, and
    // aarch64 reads a device tree at stage 5. Until then the GIC and the PL011
    // sit at fixed addresses, and this is what keeps them reachable once the
    // firmware's own identity map is gone.
    #[cfg(target_arch = "aarch64")]
    {
        vm::map_device_window(&mut tables);
        log(&format!(
            "device window {:#x}..{:#x} mapped, Device and not executable",
            vm::DEVICE_BASE,
            vm::DEVICE_BASE + vm::DEVICE_WINDOW
        ));
    }
    vm::map_kernel_image(
        &mut tables,
        image_mem,
        kernel.text,
        kernel.rodata,
        kernel.data,
    );
    log(&format!(
        "page tables: {} pages of them",
        tables.pool().pages_taken()
    ));

    // Nothing is freed from here on. The three buffers the files were read into
    // belong to the firmware's pool, which stops existing at the next call
    // anyway, and `kernel` borrows one of them for as long as it is alive.

    // The point of no return.
    log("leaving boot services");
    let after = unsafe { boot::exit_boot_services(None) };
    write_handoff(
        &after,
        image_mem,
        initramfs_mem,
        initramfs.len(),
        cmdline_mem,
        cmdline.len(),
        stack,
    );
    drop(after);

    let entry = vm::KERNEL_VIRT_BASE + kernel.entry;
    log("jumping to the kernel");
    unsafe { enter(&raw const HANDOFF, entry, stack_top, tables.root()) }
}

/// Hand the machine to the kernel.
///
/// # Safety
///
/// `record` must point at a filled-in [`Handoff`], `entry` must be mapped
/// executable in the page tables rooted at `root`, `stack_top` must be mapped
/// writable in those same tables, and both must be reachable from the
/// higher-half addresses they are given as. This is the last thing the loader
/// does and there is no way back from it.
///
/// `root` is one frame on both machines, which is worth saying because it is not
/// obvious on aarch64: that machine has two base registers and this loader
/// points both at the same table, which works because the low half of the tree
/// holds only the identity map and the high half only the direct map and the
/// alias, and no entry is wanted by both. See `vm/aarch64.rs`.
#[cfg(target_arch = "x86_64")]
unsafe fn enter(record: *const Handoff, entry: u64, stack_top: u64, root: u64) -> ! {
    unsafe {
        core::arch::asm!(
            // Interrupts off for the switch: the firmware may have left them on,
            // and a timer between the two instructions below would be delivered
            // on a stack that has already moved.
            "cli",
            "mov cr3, {cr3}",
            // The kernel half was not mapped a moment ago, so this cannot be
            // hoisted above the CR3 write.
            "mov rsp, {stack}",
            "xor rbp, rbp",
            "jmp {entry}",
            cr3 = in(reg) root,
            stack = in(reg) stack_top - 8,
            entry = in(reg) entry,
            in("rdi") record,
            options(noreturn)
        )
    }
}

/// The aarch64 switch, and the order of it is the architecture's:
///
/// * **Interrupts masked first.** The firmware may have left them on, and an
///   interrupt between the base-register writes and the branch would be taken on
///   the old tables and return to a different stack.
/// * **`dsb ish` before the base registers**, so the page tables this loader just
///   wrote are visible to the walker before anything points at them.
/// * **`MAIR`, `TCR`, then both `TTBR`s**, because a translation can only happen
///   once all four agree.
/// * **`tlbi vmalle1` and another `dsb ish`**, because the walker may have cached
///   the *failed* lookups it made while the firmware's tables were in use - the
///   higher half was not mapped a moment ago, so those entries exist and are
///   negative.
/// * **`isb` after the `SCTLR_EL1` write**, so the instruction after it is
///   fetched with the new tables. `M` is already set (the firmware runs with the
///   MMU on, which is the only reason this loader can write a page table), and
///   the register is read-modify-written rather than assigned for the reason
///   `vm/aarch64.rs` gives.
///
/// The stack is handed over page-aligned rather than one word down the way the
/// x86_64 arm hands it: `SP` must be sixteen-byte aligned, and with `SCTLR_EL1.A`
/// set a misaligned one is a fault on the first `stp` of the kernel's own
/// prologue rather than a warning.
#[cfg(target_arch = "aarch64")]
unsafe fn enter(record: *const Handoff, entry: u64, stack_top: u64, root: u64) -> ! {
    // Two blocks rather than one because `options(noreturn)` forbids an asm
    // output, and the `SCTLR_EL1` update needs a register to read the old value
    // into. The split is where it naturally falls: everything above the last
    // `isb` is the switch and arrives somewhere, and the three instructions
    // after it do not.
    unsafe {
        core::arch::asm!(
            "msr daifset, #0xf",
            "dsb ish",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {root}",
            "msr ttbr1_el1, {root}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, {bits}",
            "msr sctlr_el1, {sctlr}",
            "isb",
            mair = in(reg) vm::MAIR,
            tcr = in(reg) vm::TCR,
            root = in(reg) root,
            bits = in(reg) vm::SCTLR_ENABLE,
            sctlr = lateout(reg) _,
        );
        core::arch::asm!(
            "mov sp, {stack}",
            "mov x0, {record}",
            "br {entry}",
            stack = in(reg) stack_top,
            entry = in(reg) entry,
            record = in(reg) record,
            options(noreturn)
        )
    }
}

/// Turn the memory map that survives boot services into the record the kernel
/// reads.
///
/// Runs after boot services are gone, so it allocates nothing: the regions are
/// written straight into the record's fixed array and folded there. UEFI memory
/// maps are ordered by physical address, which is what makes the fold a single
/// pass.
fn write_handoff(
    map: &MemoryMapOwned,
    kernel_phys_base: u64,
    initramfs_phys: u64,
    initramfs_len: usize,
    cmdline_phys: u64,
    cmdline_len: usize,
    stack_phys_base: u64,
) {
    let mut record = EMPTY_HANDOFF;
    let mut count = 0usize;
    let mut dropped = 0usize;

    for entry in map.entries() {
        let kind = kind_of(entry.ty);
        if !vm::is_ram(kind) {
            continue;
        }
        let region = Region {
            base: entry.phys_start,
            length: entry.page_count * PAGE_SIZE,
            kind,
            reserved: 0,
        };
        match record.regions[..count].last_mut() {
            Some(last) if last.kind == region.kind && last.base + last.length == region.base => {
                last.length += region.length;
            }
            _ if count < MAX_MEM_REGIONS => {
                record.regions[count] = region;
                count += 1;
            }
            _ => dropped += 1,
        }
    }

    record.magic = MAGIC;
    record.hhdm_offset = vm::HHDM_OFFSET;
    record.kernel_phys_base = kernel_phys_base;
    record.kernel_virt_base = vm::KERNEL_VIRT_BASE;
    record.initramfs_phys = initramfs_phys;
    record.initramfs_len = initramfs_len as u64;
    record.cmdline_phys = cmdline_phys;
    record.cmdline_len = cmdline_len as u64;
    // Through the direct map, to match where the canary itself was written and
    // every other address the kernel will touch. The physical address means
    // nothing to the kernel once it has built its own tables.
    record.boot_stack_base = vm::HHDM_OFFSET + stack_phys_base;
    record.region_count = count as u64;

    // The one write to the static, after everything in it is final: the kernel
    // reads this record on a machine where boot services and this function no
    // longer exist, so there must be no moment at which a half-filled one is
    // reachable.
    unsafe { core::ptr::write(&raw mut HANDOFF, record) };

    assert_eq!(
        dropped, 0,
        "{dropped} memory regions did not fit in the record"
    );
}

/// Copy a byte string into frames of its own and return where they are.
fn copy_to_frames(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let pages = (bytes.len() as u64).div_ceil(PAGE_SIZE) as usize;
    let at = allocate(pages, "command line");
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
    at
}

/// Allocate `pages` contiguous pages the kernel will never be handed.
///
/// `LOADER_DATA` and not `CONVENTIONAL`: memory the firmware calls conventional
/// is what the kernel's frame allocator will take, and everything this loader
/// keeps alive - its page tables, the loaded image, the stack, the two files -
/// has to be outside that. After boot services are exited, `LOADER_DATA` stays
/// `LOADER_DATA`, which the kernel records as bootloader-reclaimable and never
/// allocates from.
fn allocate(pages: usize, what: &str) -> u64 {
    let at = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
        .unwrap_or_else(|error| panic!("out of memory for the {what}: {error}"))
        .as_ptr() as u64;
    assert!(
        at.is_multiple_of(PAGE_SIZE),
        "the firmware returned an unaligned allocation for the {what}"
    );
    at
}

/// Read a whole file from the boot volume, or nothing at all if it is not there.
fn read(fs: &mut FileSystem, path: &CStr16) -> Vec<u8> {
    fs.read(path).unwrap_or_default()
}

fn trim(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |at| at + 1);
    &bytes[..end]
}

fn kind_of(ty: MemoryType) -> u32 {
    use handoff::*;
    match ty {
        MemoryType::CONVENTIONAL => KIND_USABLE,
        MemoryType::BOOT_SERVICES_CODE | MemoryType::BOOT_SERVICES_DATA => {
            KIND_BOOTLOADER_RECLAIMABLE
        }
        MemoryType::LOADER_CODE | MemoryType::LOADER_DATA => KIND_BOOTLOADER_RECLAIMABLE,
        MemoryType::ACPI_RECLAIM => KIND_ACPI_RECLAIMABLE,
        MemoryType::ACPI_NON_VOLATILE => KIND_ACPI_NVS,
        MemoryType::UNUSABLE => KIND_BAD_MEMORY,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => KIND_RESERVED,
        // Runtime services, persistent memory, PAL, and anything a newer
        // firmware names. The kernel neither allocates from these nor reaches
        // devices through them, so the pessimistic kind costs nothing.
        _ => KIND_RESERVED,
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    log(&format!("mpos-boot panicked: {info}"));
    loop {
        core::hint::spin_loop();
    }
}

/// `format!` for a `no_std` binary with an allocator, kept local so the macros
/// below read the same as everywhere else in the project.
use alloc::format;

/// Diagnostics on the serial port.
///
/// The firmware's console is not where a headless machine's boot is watched, and
/// after boot services are exited there is no console at all. A UART is a
/// register and an address, and it works before, during and after boot services
/// on both machines - which is why there is no firmware-console fallback and why
/// this module has one body per machine.
mod serial {
    pub use imp::{init, write};

    #[cfg(target_arch = "x86_64")]
    mod imp {
        use core::arch::asm;

        const COM1: u16 = 0x3f8;

        fn out(port: u16, value: u8) {
            unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) }
        }

        fn inp(port: u16) -> u8 {
            let value: u8;
            unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
            value
        }

        pub fn init() {
            out(COM1 + 1, 0x00); // no interrupts
            out(COM1 + 3, 0x80); // divisor latch
            out(COM1, 0x01); // 115200 baud
            out(COM1 + 1, 0x00);
            out(COM1 + 3, 0x03); // 8 bits, no parity, one stop
            out(COM1 + 2, 0xc7); // FIFO on, cleared
            out(COM1 + 4, 0x03); // ready to transmit
        }

        pub fn write(text: &str) {
            for byte in text.bytes() {
                if byte == b'\n' {
                    put(b'\r');
                }
                put(byte);
            }
        }

        fn put(byte: u8) {
            while inp(COM1 + 5) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            out(COM1, byte);
        }
    }

    /// The PL011 on QEMU's `virt` machine, at the address the firmware's device
    /// tree gives and `src/arch/aarch64/pl011.rs` also names.
    ///
    /// A second copy of that driver, deliberately: the loader is a separate
    /// compilation with no crate between it and the kernel (the same reason the
    /// handoff record is written twice), and the alternative - a shared crate -
    /// would be the first dependency either has on the other.
    ///
    /// It does *not* reinitialize the UART the way the COM1 arm does. AAVMF has
    /// already brought this one up and is using it as its own console, so
    /// rewriting the baud divisor and line control would take the wire away from
    /// the firmware in the middle of its own output; the enable bits are ORed in
    /// and nothing else is touched, which is all a writer needs.
    #[cfg(target_arch = "aarch64")]
    mod imp {
        const BASE: usize = 0x0900_0000;

        /// Data register: what is written goes out.
        const DR: usize = 0x00;
        /// Flag register; bit 5 is `TXFF`, "the transmit FIFO is full".
        const FR: usize = 0x18;
        /// Control: the enable bits.
        const CR: usize = 0x30;

        const FR_TXFF: u32 = 1 << 5;
        /// `UARTEN | TXE | RXE`.
        const CR_ENABLE: u32 = 1 | (1 << 8) | (1 << 9);

        fn read(offset: usize) -> u32 {
            // SAFETY: every offset above is a register of the PL011 at `BASE`,
            // which the firmware's identity map already covers - this runs
            // before the loader's own tables are installed.
            unsafe { core::ptr::read_volatile((BASE + offset) as *const u32) }
        }

        fn write_reg(offset: usize, value: u32) {
            // SAFETY: as `read`.
            unsafe { core::ptr::write_volatile((BASE + offset) as *mut u32, value) };
        }

        pub fn init() {
            write_reg(CR, read(CR) | CR_ENABLE);
        }

        pub fn write(text: &str) {
            for byte in text.bytes() {
                if byte == b'\n' {
                    put(b'\r');
                }
                put(byte);
            }
        }

        fn put(byte: u8) {
            while read(FR) & FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            write_reg(DR, byte as u32);
        }
    }
}

/// One line of boot progress on the serial port, with the prefix that makes the
/// loader's output distinguishable from the kernel's own.
pub fn log(message: &str) {
    serial::write("mpos-boot: ");
    serial::write(message);
    serial::write("\n");
}
