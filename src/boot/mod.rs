//! Machine-independent view of the hardware the kernel was handed at boot.
//!
//! This module deliberately does not name any bootloader type. `boot::limine`
//! translates the Limine protocol into these structures; everything else in the
//! kernel only ever sees what is declared here. That is the seam future ports
//! (riscv64 via OpenSBI+DTB, aarch64) plug into.

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub mod limine;

use core::fmt;

pub const MAX_MEM_REGIONS: usize = 64;

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
