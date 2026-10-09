//! `mpos_boot`: what starts mpos on a UEFI machine, as a library.
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
//! # Two machines, one loader
//!
//! The list above is one list, and it is the same on x86_64 and aarch64: the
//! `uefi` crate's file protocols, memory map and pool allocator are portable,
//! and so is the handoff record - it carries plain data and no page-table root,
//! no GDT and no EFI table, which is exactly the seam `src/boot/mod.rs`
//! describes.
//!
//! Three things are the machine's and not the loader's - the progress lines'
//! wire, the page-table descriptor, and the last instruction - and none of them
//! is in this crate. They are `mpos_arch::early`, one arm per machine and
//! nothing in between; this file names no architecture, and what it knows of the
//! machine it learns from three constants and a boolean. `NOTES.md` §14.6 is the
//! move that put them there, and `arch/src/early/mod.rs` argues why they are a
//! tree of their own rather than part of the kernel's machine layer.
//!
//! What that bought is the shape of the sequence below: it reads as one
//! sequence, and the two questions that really are per-machine ("does this
//! firmware describe the machine with a device tree?", "what lies beyond RAM?")
//! are asked by name, rather than answered by a `#[cfg]` in the middle of a
//! step.

#![no_std]

extern crate alloc;

mod elf;
mod vm;

use alloc::vec::Vec;
use core::panic::PanicInfo;

use mpos_arch::early;
use uefi::CStr16;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::fs::FileSystem;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};
use uefi::prelude::*;
use uefi::system::with_config_table;
use uefi::{Guid, guid};

use crate::elf::PAGE_SIZE;
// The record is the `mpos-bootinfo` leaf, shared with the kernel rather than
// spelled out twice here. The two names below are the ones this file used when
// it carried its own copy (`Region`, `MAGIC`); the leaf calls them `AbiRegion`
// and `HANDOFF_MAGIC`, and the aliases keep every use below unchanged.
use mpos_bootinfo::{AbiRegion as Region, HANDOFF_MAGIC as MAGIC, Handoff, MAX_MEM_REGIONS};

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

/// The GUID a UEFI firmware publishes a device tree under - `EFI_DTB_TABLE_GUID`,
/// which is the one the Linux EFI stub looks for and the one AAVMF uses.
///
/// A device tree is not passed in a register on this machine. Stages 1b-1d
/// received one in `x0` because QEMU's `-kernel` puts it there, which was the
/// bring-up instrument stage 1e deleted; a real firmware describes itself
/// through the configuration table instead, and that is firmware data - it does
/// not survive `exit_boot_services`, so the tree has to be copied out like the
/// two files above.
///
/// The search is guarded by [`early::FIRMWARE_PUBLISHES_DEVICE_TREE`] rather than
/// by a `#[cfg]`, because "does this machine's firmware describe it with a tree
/// or with ACPI?" is a machine fact and not a code-shape fact: the walk below is
/// the same on both machines, and one of them finds nothing.
const DTB_GUID: Guid = guid!("b1b621d5-f19c-41a5-830b-d9152c69aae0");

/// The first two words of a device tree header: its magic, and the size it
/// claims for itself.
///
/// Spelled out here rather than shared with the kernel's reader (`crates/fdt`)
/// for the same reason [`MAGIC`] is: there is no crate between this loader and
/// the kernel, and these two numbers are the whole of what a *copy* needs. The
/// kernel parses the tree and says what it found; this reads them so it knows
/// how many bytes to copy, and refuses a blob that is not a tree at the point
/// where the mistake is still attributable to the firmware.
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_MAX_BYTES: usize = 4 * 1024 * 1024;

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
/// checked by the kernel once it is up. The definition is the handoff crate's,
/// beside the magic, because that is the one crate both sides of the handoff
/// already share - the bytes are the two ends' only agreement about what a
/// healthy stack looks like, so they cannot be two spellings.
use mpos_bootinfo::BOOT_STACK_CANARY;

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
    dtb_phys: 0,
    dtb_len: 0,
};

/// The firmware entry point's body: everything the loader does, from the first
/// progress line to the jump that never returns.
///
/// This is a library function rather than the `#[entry]` itself so that the
/// sequence can be named - and, as `NOTES.md` §14.7 schedules, later *called* -
/// from a binary this crate does not own. `src/main.rs` is the `#[entry]` that
/// currently calls it; the shape is what makes "the loader is a crate" true
/// rather than "the loader is a program that happens to have one function".
pub fn run() -> Status {
    // The wire first, then where the lines go: a line logged before `set_log`
    // has arrived is dropped, and this is the only ordering constraint the
    // progress output has.
    early::serial_init();
    early::set_log(log);
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
    // identity and through the direct map - plus, on the machine that has one,
    // the device window the kernel reaches its console and interrupt controller
    // through. How large that window is is the machine's answer, not this
    // file's, and asking for it before the first frame is taken is the whole
    // reason it is a constant and not a call after the mapping.
    let pool_bytes = vm::pool_bytes_for(ram + early::extra_mapped_bytes());
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
    let cmdline_mem = copy_to_frames(cmdline, "command line");
    // The machine's own description of itself, read and copied while the
    // firmware still has it. On a machine whose firmware describes it another
    // way - x86_64's ACPI - the length says "absent" rather than the kernel
    // being handed an empty tree it would have to special-case.
    let (dtb_phys, dtb_len) = if early::FIRMWARE_PUBLISHES_DEVICE_TREE {
        copy_device_tree()
    } else {
        (0u64, 0u64)
    };

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
    // Whatever this machine's kernel needs beyond RAM. One machine maps its
    // device registers here because it has no other way to reach them yet;
    // the other maps nothing here at all, because its kernel walks a BAR. Both
    // of those are the machine's answers, so the call is the same call and the
    // difference is inside `mpos_arch::early`.
    early::map_extra(&mut tables);
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
        dtb_phys,
        dtb_len,
    );
    drop(after);

    let entry = vm::KERNEL_VIRT_BASE + kernel.entry;
    log("jumping to the kernel");
    // SAFETY: the record was filled in by the write above, which is the last
    // thing to touch it; `entry` is mapped executable by the tables whose root
    // is handed over; `stack_top` is mapped writable by the same tables; and the
    // jump is the loader's last act, so nothing here returns. The record travels
    // as a bare pointer because the two machines put it in different registers,
    // which is the machine's business and not this file's.
    unsafe {
        early::enter(
            core::ptr::addr_of!(HANDOFF).cast(),
            entry,
            stack_top,
            tables.root(),
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
    dtb_phys: u64,
    dtb_len: u64,
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
    record.dtb_phys = dtb_phys;
    record.dtb_len = dtb_len;

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
fn copy_to_frames(bytes: &[u8], what: &str) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let pages = (bytes.len() as u64).div_ceil(PAGE_SIZE) as usize;
    let at = allocate(pages, what);
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
    at
}

/// The firmware's device tree, copied into frames of its own.
///
/// Answers the physical address and the length, or `(0, 0)` when the firmware
/// published none - which the kernel reports by name rather than reading as an
/// empty machine.
///
/// The buffer the configuration table points at has no length of its own: the
/// only thing that says how much of it is the tree is the tree's own header, so
/// this reads the first two words and takes `totalsize` bytes. QEMU's blob
/// claims a megabyte around eight kilobytes of meaning, which is why the number
/// is read rather than assumed.
fn copy_device_tree() -> (u64, u64) {
    let (found, published) = with_config_table(|entries| {
        let found = entries
            .iter()
            .find(|entry| entry.guid == DTB_GUID)
            .map(|entry| entry.address as u64);
        let published: Vec<Guid> = entries.iter().map(|entry| entry.guid).collect();
        (found, published)
    });

    let Some(at) = found else {
        // Say what the firmware *did* publish. A machine that hands its tree
        // over under another name is one line away from this log and an
        // afternoon away from the words "no device tree" on their own.
        for guid in &published {
            log(&format!("config table: {guid}"));
        }
        log("the firmware published no device tree");
        return (0, 0);
    };

    // SAFETY: the firmware named this address as its device tree, and nothing
    // has left boot services yet, so the blob is still there and still its own.
    let head = unsafe { core::slice::from_raw_parts(at as *const u8, 8) };
    let magic = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let total = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    assert_eq!(
        magic, FDT_MAGIC,
        "the firmware's device tree opens with {magic:#x}, not a device tree magic"
    );
    assert!(
        total <= FDT_MAX_BYTES,
        "the firmware's device tree claims {total} bytes, more than the kernel reads"
    );
    // SAFETY: as above. `total` is the size the tree states for itself, which
    // is the only size there is.
    let tree = unsafe { core::slice::from_raw_parts(at as *const u8, total) };
    let mem = copy_to_frames(tree, "device tree");
    log(&format!("device tree: {total} bytes at phys {mem:#x}"));
    (mem, total as u64)
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
    use mpos_bootinfo::*;
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

/// One line of boot progress on the serial port, with the prefix that makes the
/// loader's output distinguishable from the kernel's own.
///
/// The wire is the machine's (`mpos_arch::early`); the *format* is this
/// loader's, which is why the progress lines are formatted here and handed over
/// as whole lines. [`run`] registers this function with `early::set_log`
/// before the first of them, so a line logged from inside `early` - the device
/// window it maps, say - comes out through here and reads like the rest.
pub fn log(message: &str) {
    early::serial_write("mpos-boot: ");
    early::serial_write(message);
    early::serial_write("\n");
}
