//! The boot record, and the kernel's view of it: the one vocabulary both sides
//! of the loader seam speak.
//!
//! A leaf crate, and for the same reason as the other leaves under `crates/`:
//! it is *data* a machine layer and the kernel both have to name, so neither
//! can own it. `mpos-arch`'s `Paging::install` takes a [`BootInfo`] and its
//! aarch64 entry takes a [`Handoff`], which is exactly why these two cannot
//! live inside the kernel once the machine layer is its own crate
//! (`NOTES.md` §14).
//!
//! # The two halves, and why only one of them is an ABI
//!
//! [`Handoff`] is a wire format. It is `#[repr(C)]`, it crosses from one
//! compilation (the UEFI loader) into another (the kernel), and every field in
//! it is a promise to a binary this crate never sees. Both compilations read
//! this one definition: the loader (`mpos-boot`) depends on this crate, so the
//! layout is written once and cannot drift out of step with itself. The
//! [`HANDOFF_MAGIC`] the loader writes is still checked by the kernel before it
//! reads anything else: the kernel travels inside the loader now, so the two
//! ends are always built together and cannot disagree by accident, but a boot
//! volume assembled or half-updated by hand is a real thing to guard against and
//! the check is one comparison.
//!
//! [`BootInfo`] is not a wire format. It is what the kernel turned the record
//! into: plain data, no pointers, no page-table root, no EFI table, and a
//! `&'static str` where the record had an address and a length. Translating one
//! into the other is `mpos`'s `boot::handoff::collect`, which stays in the
//! kernel because it is the code that decides what a region *means*.
//!
//! Nothing here allocates, locks or touches hardware, so the whole crate is
//! host-testable.

#![no_std]

use core::fmt;

/// Room for every region a firmware's memory map can carry after the loader has
/// merged the neighbours that share a kind. Firmware maps are longer than a
/// bootloader's - OVMF describes the same RAM in far more pieces - and the
/// kernel has no heap yet when this is filled in, so the room is a ceiling and
/// not an allocation.
pub const MAX_MEM_REGIONS: usize = 128;

// ---------------------------------------------------------------------------
// The record: what the loader writes and the kernel reads.
// ---------------------------------------------------------------------------

/// `"MPOSBOOT"` as little-endian bytes. Spelled out rather than written as a
/// number so the two copies can be compared by eye.
pub const HANDOFF_MAGIC: u64 = u64::from_le_bytes(*b"MPOSBOOT");

/// What the loader writes into the lowest word of the boot stack, and the kernel
/// checks before it hands the machine to init. The bytes spell `mpos_boo`
/// little-endian, the way [`HANDOFF_MAGIC`] spells `MPOSBOOT`.
///
/// It lives here, with the magic, because this crate *is* the one both sides of
/// the handoff share: it was written twice (once in `mpos-boot`, once in
/// `board/x86_64.rs`) with a comment claiming there was no crate between them, and
/// that claim had stopped being true when this leaf was created. A canary whose
/// two ends can drift is a canary that reports an overflow which is really a
/// misspelling - and the second machine reads it too, from the same field.
pub const BOOT_STACK_CANARY: u64 = u64::from_le_bytes(*b"mpos_boo");

/// A memory region as it crosses the boundary. `kind` holds one of the
/// `KIND_*` constants below rather than a Rust enum, because the enum's
/// discriminant is a compiler decision the loader must not have to share.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiRegion {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub reserved: u32,
}

pub const KIND_USABLE: u32 = 0;
pub const KIND_RESERVED: u32 = 1;
pub const KIND_ACPI_RECLAIMABLE: u32 = 2;
pub const KIND_ACPI_NVS: u32 = 3;
pub const KIND_BAD_MEMORY: u32 = 4;
pub const KIND_BOOTLOADER_RECLAIMABLE: u32 = 5;
pub const KIND_KERNEL_AND_MODULES: u32 = 6;
pub const KIND_FRAMEBUFFER: u32 = 7;

/// Everything the loader knows that the kernel needs, in one page.
///
/// Defaulted to all zeroes by the loader before it fills in what it has, so a
/// field it does not set reads as "absent" rather than as a stale address.
#[repr(C)]
pub struct Handoff {
    pub magic: u64,
    /// Add to a physical address to reach it through the kernel's direct map.
    /// The loader chose this and mapped it; the kernel never invents one.
    pub hhdm_offset: u64,
    /// Physical address of the first byte of the loaded kernel image.
    pub kernel_phys_base: u64,
    /// Virtual address the image is mapped at, in the kernel half.
    pub kernel_virt_base: u64,
    /// The initramfs archive, in the direct map. Zero length when none was
    /// loaded, which is the diskless boot.
    pub initramfs_phys: u64,
    pub initramfs_len: u64,
    /// The command line, in the direct map, without a terminator.
    pub cmdline_phys: u64,
    pub cmdline_len: u64,
    /// Direct-map address of the lowest word of the boot stack - the one the
    /// loader wrote its canary into. Zero when the loader planted none, which is
    /// how a handoff from an older loader reads.
    ///
    /// The kernel needs this to check that canary, and it cannot work the
    /// address out for itself: the stack is an allocation of the loader's, and
    /// nothing else records where it began. Its top is in the kernel's initial
    /// `rsp`, but the bottom - the end an overflow reaches last, and the only
    /// place a mark survives being walked over - is only ever known here.
    pub boot_stack_base: u64,
    pub region_count: u64,
    pub regions: [AbiRegion; MAX_MEM_REGIONS],
    /// The machine's own description of itself, copied out of the firmware's
    /// configuration table, and how many bytes of it there are. Both zero on a
    /// machine whose firmware handed none over, which is every x86_64 boot and
    /// an aarch64 one whose firmware does not publish a tree.
    ///
    /// Copied rather than pointed at: under UEFI the tree lives in firmware
    /// data, which does not survive `exit_boot_services`. The loader reads the
    /// header's `totalsize` and takes that many bytes, because the buffer the
    /// firmware points at has no length of its own.
    ///
    /// # Why these two are last
    ///
    /// A field added to a fixed-layout record that two separate compilations
    /// write and read is a field an older counterpart will not have written -
    /// and the danger is not a zero, it is a *value*: appended here, a stale
    /// loader's bytes read as `dtb_len == 0` and the kernel says "no tree",
    /// where inserted beside `cmdline` they would have shifted every field
    /// after them and been read as one of those instead. This is the only
    /// position in the record where the change is safe in both directions, and
    /// it is why [`HANDOFF_MAGIC`] does not have to move for it: every offset a
    /// counterpart written from an older tree uses is untouched, and the one
    /// field it did not write reads as absent.
    pub dtb_phys: u64,
    pub dtb_len: u64,
    /// Physical address of the ACPI root pointer (`RSDP`), or zero when the
    /// firmware published none - which is every aarch64 boot, whose machine
    /// describes itself with a device tree instead.
    ///
    /// Kept as the address the firmware named rather than copied, unlike the
    /// device tree above: the root pointer and everything it names live in the
    /// firmware's ACPI tables, which the memory map reports as
    /// `AcpiReclaimable` - memory the frame allocator never hands out
    /// ([`MemKind::is_allocatable`]) - so the kernel reads them through the
    /// direct map whenever it likes. A copy would be a second version of what
    /// the firmware already published, to be kept in step with nothing.
    ///
    /// Appended for the same reason the two fields above it are: a loader from
    /// before this field writes nothing here, and that reads as "no tables"
    /// rather than as a stale address, so [`HANDOFF_MAGIC`] does not move.
    pub rsdp_phys: u64,
}

// ---------------------------------------------------------------------------
// The kernel's view: what `collect` turns the record into.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemKind {
    Usable,
    Reserved,
    AcpiReclaimable,
    AcpiNvs,
    BadMemory,
    BootloaderReclaimable,
    KernelAndModules,
    Framebuffer,
}

impl MemKind {
    /// Whether the kernel may hand this region's pages out to allocators.
    ///
    /// Deliberately narrower than "memory we could eventually use".
    /// `BootloaderReclaimable` holds the stack this kernel is still running on.
    /// Its page tables were in there too, but M3 replaced those with the kernel's
    /// own; the stack lasts until M4 gives every task one of its own.
    /// `AcpiReclaimable` holds tables we have not parsed yet.
    ///
    /// That exclusion is load-bearing rather than merely conservative, and it is
    /// not left to be inferred: `handoff::collect` refuses a record that offers
    /// the boot stack as allocatable memory (`NOTES.md` §13.17), so a loader that
    /// disagreed about it stops by name instead of handing the kernel the stack it
    /// is standing on.
    pub fn is_allocatable(self) -> bool {
        matches!(self, MemKind::Usable)
    }
}

#[derive(Clone, Copy)]
pub struct MemRegion {
    /// Physical address.
    pub base: usize,
    pub length: usize,
    pub kind: MemKind,
}

impl MemRegion {
    pub fn end(&self) -> usize {
        self.base + self.length
    }
}

impl fmt::Debug for MemRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{:016x}..{:016x}) {:?} {} KiB",
            self.base,
            self.end(),
            self.kind,
            self.length / 1024
        )
    }
}

/// Everything the kernel needs to know about the machine, gathered once at boot.
pub struct BootInfo {
    /// Add to a physical address to get its kernel-virtual direct-map address.
    pub hhdm_offset: usize,
    pub kernel_phys_base: usize,
    pub kernel_virt_base: usize,
    pub bootloader_name: &'static str,
    pub bootloader_version: &'static str,
    /// The initramfs archive the bootloader loaded, bytes as they are on disk.
    /// The mapping belongs to the bootloader's `KernelAndModules` region, which
    /// the frame allocator never hands out, so the bytes stay valid for good.
    pub initramfs: Option<&'static [u8]>,
    /// The command line the bootloader was given for this kernel. `/proc/cmdline`
    /// reports it, and the initramfs's init reads it to learn `root=`.
    pub cmdline: &'static str,
    /// The machine's own description of itself, on a machine that has one.
    ///
    /// A device tree is how a firmware describes a machine with no PCI and no
    /// ACPI: which devices exist, where their registers are, which interrupt
    /// line each is on. `None` means the firmware published none, which is
    /// every x86_64 boot - so a caller that *needs* one asks for it by name
    /// rather than treating the absence as an empty machine.
    pub dtb: Option<&'static [u8]>,
    /// Physical address of the ACPI root pointer, or zero.
    ///
    /// Not a slice, because what it names is a tree rather than a blob: the root
    /// pointer names a table, that table names others, and each carries its own
    /// length (`crates/acpi`). A caller reads the bytes through
    /// [`BootInfo::phys_to_virt`] and walks them; the kernel's first caller is
    /// `drivers::pci`, looking for MCFG's memory-mapped configuration window.
    pub rsdp_phys: usize,
    /// The lowest word of the boot stack, where the loader left its canary, or
    /// zero when the loader planted none. `check_boot_stack` reads it; nothing
    /// else needs it, and nothing may write through it.
    pub boot_stack_base: usize,
    pub regions: [MemRegion; MAX_MEM_REGIONS],
    pub region_count: usize,
}

impl BootInfo {
    #[inline]
    pub fn phys_to_virt(&self, phys: usize) -> usize {
        phys + self.hhdm_offset
    }

    pub fn regions(&self) -> &[MemRegion] {
        &self.regions[..self.region_count]
    }

    /// Total bytes of memory the kernel could eventually hand out.
    pub fn allocatable_bytes(&self) -> usize {
        self.regions()
            .iter()
            .filter(|r| r.kind.is_allocatable())
            .map(|r| r.length)
            .sum()
    }
}
