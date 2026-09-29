//! Machine-independent view of the hardware the kernel was handed at boot.
//!
//! This module deliberately does not name any bootloader type. `boot::handoff`
//! translates what `mpos-boot`, the EFI loader, left behind into these
//! structures; everything else in the kernel only ever sees what is declared
//! here. That is the seam future ports (riscv64 via OpenSBI+DTB, aarch64) plug
//! into.

// The kernel build is the only one that needs this, and the host cannot
// provide the loader's record - but the layout it declares is a promise to
// another crate, so `test` keeps it compiled where the tests can pin it.
#[cfg(all(any(target_os = "none", test), target_arch = "x86_64"))]
pub mod handoff;

/// The consoles this boot writes its kernel log to, read off the command line
/// the way Linux reads `console=`. Not target-gated: walking a command line is
/// pure, and the policy it yields is exactly the kind of thing a host test
/// should be able to argue with.
pub mod console_list;

use core::fmt;

/// Room for every region a firmware's memory map can carry after the loader has
/// merged the neighbours that share a kind. Firmware maps are longer than a
/// bootloader's - OVMF describes the same RAM in far more pieces - and the
/// kernel has no heap yet when this is filled in, so the room is a ceiling and
/// not an allocation.
pub const MAX_MEM_REGIONS: usize = 128;

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
    /// The lowest word of the boot stack, where the loader left its canary, or
    /// zero when the loader planted none. [`check_boot_stack`] reads it; nothing
    /// else needs it, and nothing may write through it.
    pub boot_stack_base: usize,
    pub regions: [MemRegion; MAX_MEM_REGIONS],
    pub region_count: usize,
}

/// The command line the bootloader was given, kept where the filesystems can
/// read it: `/proc/cmdline` is how the initramfs's init learns `root=`.
static CMDLINE: critical_section::Mutex<core::cell::RefCell<&'static str>> =
    critical_section::Mutex::new(core::cell::RefCell::new(""));

/// Record the boot command line. Called once, from the boot task.
pub fn set_cmdline(line: &'static str) {
    critical_section::with(|cs| *CMDLINE.borrow(cs).borrow_mut() = line);
}

/// The boot command line, as the bootloader was given it.
pub fn cmdline() -> &'static str {
    critical_section::with(|cs| *CMDLINE.borrow(cs).borrow())
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
